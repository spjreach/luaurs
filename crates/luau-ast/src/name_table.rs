//! Port of the `AstNameTable` from `Luau/Lexer.h` / `Ast/src/Lexer.cpp`.
//!
//! The C++ table interns names into a bump allocator and returns
//! pointer-identity-compared `AstName` values. The Rust port interns names
//! by leaking owned strings and handing out `&'static str`, which keeps
//! cheap content equality while preserving the interned (deduplicated)
//! property.
//!
//! Reserved words are pre-registered with their reserved lexeme type, so
//! reading `and` yields `LexemeType::Reserved("and")` exactly like the C++
//! table.

use luau_common::fasthash::HashMap;

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
    index: HashMap<&'static [u8], (&'static str, LexemeType)>,
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
            index: luau_common::fasthash::map_with_capacity(128),
            arena: Vec::new(),
            reserved: luau_common::fasthash::map_with_capacity(RESERVED_WORDS.len()),
        };

        // register reserved words with their corresponding lexeme types
        for &word in RESERVED_WORDS.iter() {
            assert!(word.is_ascii(), "reserved words are ASCII");
            table.add_static(word, LexemeType::Reserved(word));
            table.reserved.insert(word, ());
        }

        table
    }

    /// Register a static name with the given type; returns the name.
    pub fn add_static(&mut self, name: &'static str, ty: LexemeType) -> &'static str {
        self.index.insert(name.as_bytes(), (name, ty));
        name
    }

    /// Look up (or intern) a name; returns the interned name and its type.
    pub fn get_or_add_with_type(&mut self, name: &[u8]) -> (&'static str, LexemeType) {
        if let Some(&entry) = self.index.get(name) {
            return entry;
        }

        let interned = self.intern(name);
        let ty = if interned.as_bytes().first() == Some(&b'@') {
            LexemeType::Attribute
        } else {
            LexemeType::Name
        };
        self.index.insert(interned.as_bytes(), (interned, ty));
        (interned, ty)
    }

    /// Look up a name without interning; returns `Name` as type if missing,
    /// mirroring `AstNameTable::getWithType`.
    pub fn get_with_type(&self, name: &[u8]) -> (Option<&'static str>, LexemeType) {
        match self.index.get(name) {
            Some(&(canonical, ty)) => (Some(canonical), ty),
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
        assert_eq!(ty, LexemeType::Reserved("and"));

        let (name, ty) = t.get_with_type(b"while");
        assert_eq!(name, Some("while"));
        assert!(matches!(ty, LexemeType::Reserved("while")));

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
