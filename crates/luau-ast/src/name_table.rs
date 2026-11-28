//! Port of the `AstNameTable` from `Luau/Lexer.h` / `Ast/src/Lexer.cpp`.
//!
//! The C++ table interns names into a bump allocator and returns
//! pointer-identity-compared `AstName` values. The Rust port keeps the names
//! in the table's own arena and hands out `&'static str`, which keeps cheap
//! content equality while preserving the interned (deduplicated) property.
//!
//! The `'static` is a promise the caller keeps, as in C++: those strings
//! are freed with the table, so it must outlive every AST parsed with it.
//!
//! Reserved words are pre-registered with their reserved lexeme type, so
//! reading `and` yields `LexemeType::Reserved(Kw::AND)` exactly like the C++
//! table.

use luau_common::fasthash::HashMap;

/// Incremental name hash, fed one byte at a time (the lexer computes it
/// while scanning an identifier): bytes are packed into 8-byte words and
/// each full word is mixed in.
#[derive(Clone, Copy)]
pub struct NameHasher {
    h: u64,
    w: u64,
    shift: u32,
}

impl NameHasher {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;

    #[inline(always)]
    pub fn new() -> Self {
        NameHasher { h: 0, w: 0, shift: 0 }
    }

    #[inline(always)]
    pub fn byte(&mut self, c: u8) {
        self.w |= (c as u64) << self.shift;
        self.shift += 8;
        if self.shift == 64 {
            self.h = (self.h.rotate_left(5) ^ self.w).wrapping_mul(Self::K);
            self.w = 0;
            self.shift = 0;
        }
    }

    #[inline(always)]
    pub fn finish(self) -> u64 {
        let h = (self.h.rotate_left(5) ^ self.w ^ ((self.shift as u64) << 56)).wrapping_mul(Self::K);
        h ^ (h >> 29)
    }
}

impl Default for NameHasher {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn name_hash(b: &[u8]) -> u64 {
    let mut h = NameHasher::new();
    for &c in b {
        h.byte(c);
    }
    h.finish()
}

/// Open-addressing index of interned names (replaces a general hash map on
/// the lexer's hottest lookup).
#[derive(Debug)]
struct NameIndex {
    slots: Vec<Option<(u64, &'static str, LexemeType)>>,
    len: usize,
}

impl NameIndex {
    fn with_capacity(n: usize) -> Self {
        let cap = (n * 2).next_power_of_two().max(64);
        NameIndex { slots: vec![None; cap], len: 0 }
    }

    #[inline(always)]
    fn get(&self, name: &[u8]) -> Option<(&'static str, LexemeType)> {
        self.get_hashed(name, name_hash(name))
    }

    #[inline(always)]
    fn get_hashed(&self, name: &[u8], h: u64) -> Option<(&'static str, LexemeType)> {
        let mask = self.slots.len() - 1;
        let mut i = h as usize & mask;
        loop {
            match &self.slots[i] {
                None => return None,
                Some((sh, s, ty)) => {
                    if *sh == h && s.len() == name.len() && s.as_bytes() == name {
                        return Some((s, *ty));
                    }
                }
            }
            i = (i + 1) & mask;
        }
    }

    fn insert(&mut self, name: &'static str, ty: LexemeType) {
        if (self.len + 1) * 2 > self.slots.len() {
            let cap = self.slots.len() * 2;
            let old = std::mem::replace(&mut self.slots, vec![None; cap]);
            self.len = 0;
            for (_, s, t) in old.into_iter().flatten() {
                self.insert(s, t);
            }
        }
        let h = name_hash(name.as_bytes());
        let mask = self.slots.len() - 1;
        let mut i = h as usize & mask;
        loop {
            match &mut self.slots[i] {
                slot @ None => {
                    *slot = Some((h, name, ty));
                    self.len += 1;
                    return;
                }
                Some((sh, s, t)) => {
                    if *sh == h && *s == name {
                        *t = ty;
                        return;
                    }
                }
            }
            i = (i + 1) & mask;
        }
    }
}

#[cfg(test)]
use crate::lexeme::Kw;
use crate::lexeme::LexemeType;

/// The 21 Luau reserved words, in the same order as `kReserved` in the C++
/// lexer. Do not reorder: `Lexeme::toString` output and serialized bytecode
/// type tables rely on the mapping.
pub const RESERVED_WORDS: [&str; 21] = [
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in", "local",
    "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// Interned name table (`AstNameTable`).
#[derive(Debug)]
pub struct AstNameTable {
    /// Content-index of interned names: raw bytes -> canonical interned
    /// string and its lexeme type (Name / Attribute / Reserved), so the
    /// lexer resolves an identifier with a single lookup.
    index: NameIndex,
    /// Storage of the names interned by this table. The AST refers to them as
    /// `&'static str`, but they live only as long as the table: an AST must
    /// be dropped before the table that produced it (all callers declare the
    /// table first). Freed with the table, so repeated compiles (`loadstring`)
    /// do not accumulate names.
    arena: Vec<Box<str>>,
    /// True if the word is reserved (parallel check used by `Lexer::isReserved`).
    reserved: HashMap<&'static str, ()>,
}

impl Default for AstNameTable {
    fn default() -> Self {
        Self::new()
    }
}

impl AstNameTable {
    pub fn new() -> Self {
        let mut table = AstNameTable {
            index: NameIndex::with_capacity(512),
            arena: Vec::new(),
            reserved: luau_common::fasthash::map_with_capacity(RESERVED_WORDS.len()),
        };

        // register reserved words with their corresponding lexeme types
        for (i, &word) in RESERVED_WORDS.iter().enumerate() {
            assert!(word.is_ascii(), "reserved words are ASCII");
            table.add_static(word, LexemeType::Reserved(crate::lexeme::Kw(i as u8)));
            table.reserved.insert(word, ());
        }

        table
    }

    /// Register a static name with the given type; returns the name.
    pub fn add_static(&mut self, name: &'static str, ty: LexemeType) -> &'static str {
        self.index.insert(name, ty);
        name
    }

    /// Look up (or intern) a name; returns the interned name and its type.
    pub fn get_or_add_with_type(&mut self, name: &[u8]) -> (&'static str, LexemeType) {
        self.get_or_add_hashed(name, name_hash(name))
    }

    /// [`Self::get_or_add_with_type`] with the name's [`NameHasher`] hash.
    #[inline]
    pub fn get_or_add_hashed(&mut self, name: &[u8], hash: u64) -> (&'static str, LexemeType) {
        if let Some(entry) = self.index.get_hashed(name, hash) {
            return entry;
        }
        self.add_new(name)
    }

    /// [`Self::get_with_type`] with the name's [`NameHasher`] hash.
    #[inline]
    pub fn get_hashed(&self, name: &[u8], hash: u64) -> (Option<&'static str>, LexemeType) {
        match self.index.get_hashed(name, hash) {
            Some((canonical, ty)) => (Some(canonical), ty),
            None => (None, LexemeType::Name),
        }
    }

    #[cold]
    #[inline(never)]
    fn add_new(&mut self, name: &[u8]) -> (&'static str, LexemeType) {
        let interned = self.intern(name);
        let ty = if interned.as_bytes().first() == Some(&b'@') {
            LexemeType::Attribute
        } else {
            LexemeType::Name
        };
        self.index.insert(interned, ty);
        (interned, ty)
    }

    /// Look up a name without interning; returns `Name` as type if missing,
    /// mirroring `AstNameTable::getWithType`.
    pub fn get_with_type(&self, name: &[u8]) -> (Option<&'static str>, LexemeType) {
        match self.index.get(name) {
            Some((canonical, ty)) => (Some(canonical), ty),
            None => (None, LexemeType::Name),
        }
    }

    pub fn get_or_add(&mut self, name: &[u8]) -> &'static str {
        self.get_or_add_with_type(name).0
    }

    pub fn get_or_add_str(&mut self, name: &str) -> &'static str {
        self.get_or_add_with_type(name.as_bytes()).0
    }

    pub fn get(&self, name: &str) -> Option<&'static str> {
        self.get_with_type(name.as_bytes()).0
    }

    pub fn get_type(&self, name: &str) -> Option<LexemeType> {
        self.index.get(name.as_bytes()).map(|e| e.1)
    }

    /// True if the word is one of the reserved keywords (`Lexer::isReserved`).
    pub fn is_reserved(&self, word: &str) -> bool {
        self.reserved.contains_key(word)
    }

    /// Intern a byte string in this table's arena. Non-UTF-8 sequences are
    /// replaced lossily (Luau identifiers may contain arbitrary bytes, but
    /// non-UTF-8 names cannot be represented in Rust strings; such sources
    /// are exceedingly rare and treated here like malformed names).
    fn intern(&mut self, name: &[u8]) -> &'static str {
        let owned: Box<str> = String::from_utf8_lossy(name).into_owned().into_boxed_str();
        // SAFETY: the box's heap allocation never moves and is freed only when
        // the table is dropped; see `arena`.
        let s: &'static str = unsafe { &*(&*owned as *const str) };
        self.arena.push(owned);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_words_pre_registered() {
        let t = AstNameTable::new();
        let (name, ty) = t.get_with_type(b"and");
        assert_eq!(name, Some("and"));
        assert_eq!(ty, LexemeType::Reserved(Kw::AND));

        let (name, ty) = t.get_with_type(b"while");
        assert_eq!(name, Some("while"));
        assert!(matches!(ty, LexemeType::Reserved(Kw::WHILE)));

        assert!(t.is_reserved("elseif"));
        assert!(!t.is_reserved("elseyif"));
    }

    #[test]
    fn intern_and_lookup() {
        let mut t = AstNameTable::new();
        let (name, ty) = t.get_or_add_with_type(b"foo");
        assert_eq!(name, "foo");
        assert_eq!(ty, LexemeType::Name);

        // second lookup returns the same canonical interned string
        let (name2, ty2) = t.get_or_add_with_type(b"foo");
        assert!(std::ptr::eq(name, name2));
        assert_eq!(ty2, LexemeType::Name);

        // non-interning lookup finds it
        let (name3, ty3) = t.get_with_type(b"foo");
        assert_eq!(name3, Some("foo"));
        assert_eq!(ty3, LexemeType::Name);
    }

    #[test]
    fn attribute_names() {
        let mut t = AstNameTable::new();
        let (name, ty) = t.get_or_add_with_type(b"@native");
        assert_eq!(name, "@native");
        assert_eq!(ty, LexemeType::Attribute);
    }

    #[test]
    fn missing_lookup_returns_name() {
        let t = AstNameTable::new();
        let (name, ty) = t.get_with_type(b"missing");
        assert_eq!(name, None);
        assert_eq!(ty, LexemeType::Name);
    }
}
