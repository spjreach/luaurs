//! Fast non-cryptographic hashing for the compiler's internal maps.
//!
//! `std::collections::HashMap` defaults to SipHash-1-3, which is DoS-resistant
//! but several times slower than needed for the compiler's keys (AST node
//! pointers, small integers, short names), and showed up as ~30% of
//! `loadstring` time. The C++ compiler uses its own `DenseHashMap` with a
//! cheap hash; this is the equivalent: an FxHash-style multiply-rotate hasher
//! (the one rustc uses internally). Iteration order is deterministic.

use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// FxHash-style hasher: `h = (h.rotl(5) ^ word) * K` per input word.
#[derive(Default, Clone, Copy)]
pub struct FastHasher {
    hash: u64,
}

impl FastHasher {
    #[inline(always)]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            self.add(u64::from_le_bytes(bytes[..8].try_into().unwrap()));
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            self.add(u32::from_le_bytes(bytes[..4].try_into().unwrap()) as u64);
            bytes = &bytes[4..];
        }
        for &b in bytes {
            self.add(b as u64);
        }
    }
    #[inline(always)]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline(always)]
    fn write_u16(&mut self, i: u16) {
        self.add(i as u64);
    }
    #[inline(always)]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline(always)]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline(always)]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline(always)]
    fn finish(&self) -> u64 {
        // hashbrown takes the bucket index from the low bits and its 7-bit
        // control tag from the top bits. A multiply only carries differences
        // upward, so keys differing only in their high bits (f64 constants,
        // aligned pointers) would collide in the low bits: finish with a full
        // avalanche (murmur3 fmix64).
        let mut h = self.hash;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        h ^ (h >> 33)
    }
}

pub type FastBuildHasher = BuildHasherDefault<FastHasher>;

/// `HashMap` with [`FastHasher`]; construct with `HashMap::default()`.
pub type HashMap<K, V> = std::collections::HashMap<K, V, FastBuildHasher>;
/// `HashSet` with [`FastHasher`]; construct with `HashSet::default()`.
pub type HashSet<K> = std::collections::HashSet<K, FastBuildHasher>;

/// `HashMap::with_capacity` for [`HashMap`].
#[inline]
pub fn map_with_capacity<K, V>(n: usize) -> HashMap<K, V> {
    HashMap::with_capacity_and_hasher(n, Default::default())
}

// (native thread-local: see `fastalloc`)
#[thread_local]
static SIZE_HINT: std::cell::Cell<[usize; 3]> = std::cell::Cell::new([0; 3]);

/// [`map_hinted`] kinds: maps keyed per expression, per local, per call.
pub const HINT_EXPRS: usize = 0;
pub const HINT_LOCALS: usize = 1;
pub const HINT_CALLS: usize = 2;

/// Sets the expected number of AST expressions for the chunk being compiled
/// (maps keyed per expression are created with this capacity, avoiding
/// repeated rehashing while they grow).
pub fn set_size_hint(n: usize) {
    set_size_hints([n, 0, 0]);
}

/// Sets the expected numbers of expressions, locals and calls (see
/// [`HINT_EXPRS`] ...) for the chunk being compiled.
pub fn set_size_hints(h: [usize; 3]) {
    SIZE_HINT.set(h);
}

/// A map pre-sized with the current [`set_size_hint`].
#[inline]
pub fn map_hinted<K, V>() -> HashMap<K, V> {
    map_with_capacity(SIZE_HINT.get()[HINT_EXPRS])
}

/// A map pre-sized with the current hint of the given kind.
#[inline]
pub fn map_hinted_for<K, V>(kind: usize) -> HashMap<K, V> {
    map_with_capacity(SIZE_HINT.get()[kind])
}
