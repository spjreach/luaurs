//! Compile-time model of the VM registers for the x64 translator.
//!
//! Within straight-line code the translator tracks, per VM register, the
//! value's type tag when it is known and where the value currently lives:
//! the VM stack slot, an XMM register (numbers as doubles, vectors as packed
//! floats) and/or a general-purpose register (the u32 form of a number, an
//! `integer`, or a table/buffer pointer). Results are produced in machine
//! registers and only written back to the stack ("flushed") when something
//! needs them there; a value overwritten before that is never stored.
//!
//! Scratch registers the templates may use freely: rax, rcx, rdx, xmm0, xmm1.
//! r8, r9 and xmm2 are scratch too, but are clobbered by the store/reload
//! helpers here, so templates must not keep values in them across calls into
//! `St`.

use super::asm::{Asm, R, X};
use super::x64::{load_vec, reg, INT_TAG, NUM_TAG, TAGWORD};
use crate::lobject::*;

pub const T_ANY: u8 = 0xff;
pub const TNIL: u8 = LUA_TNIL as u8;
pub const TBOOL: u8 = LUA_TBOOLEAN as u8;
pub const TNUM: u8 = LUA_TNUMBER as u8;
pub const TINT: u8 = LUA_TINTEGER as u8;
pub const TVEC: u8 = LUA_TVECTOR as u8;
pub const TSTR: u8 = LUA_TSTRING as u8;
pub const TTAB: u8 = LUA_TTABLE as u8;
pub const TBUF: u8 = LUA_TBUFFER as u8;

/// First XMM register used for caching (xmm0..xmm2 are scratch).
pub const XS: u8 = 3;
/// General-purpose registers used for caching.
pub const GP: [R; 6] = [R::Rbx, R::Rbp, R::Rsi, R::Rdi, R::R10, R::R11];
const VEC_TAG: u64 = (LUA_TVECTOR as u64) << 32;

/// A value that has to be written to its stack slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sv {
    pub r: u8,
    pub tag: u8,
    /// xmm holding it (0: use `g`)
    pub x: u8,
    /// index + 1 into `GP`
    pub g: u8,
    /// the slot's tag word is already right
    pub memtag: bool,
}

/// A machine register to load back from a stack slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rl {
    pub r: u8,
    pub tag: u8,
    pub x: u8,
    pub g: u8,
}

/// Writes cached values to their stack slots (clobbers r8, r9, xmm2).
pub fn emit_stores(a: &mut Asm, vs: &[Sv]) {
    // tag word currently in r9
    let mut r9 = 0u64;
    for &v in vs {
        let off = reg(v.r as i32);
        match v.tag {
            TNUM | TINT => {
                if v.tag == TINT {
                    a.store(R::R13, off, GP[v.g as usize - 1]);
                } else if v.x != 0 {
                    a.movsd_store(R::R13, off, X(v.x));
                } else {
                    a.cvtsi2sd64(X(2), GP[v.g as usize - 1]);
                    a.movsd_store(R::R13, off, X(2));
                }
                if !v.memtag {
                    let tag = if v.tag == TINT { INT_TAG } else { NUM_TAG };
                    if r9 != tag {
                        a.mov_ri(R::R9, tag);
                        r9 = tag;
                    }
                    a.store(R::R13, off + TAGWORD, R::R9);
                }
            }
            TVEC => {
                store_vec_to(a, R::R13, off, X(v.x));
                r9 = 0;
            }
            _ => unreachable!("only numbers, integers and vectors are stored lazily"),
        }
    }
}

/// Writes one cached value to its stack slot (clobbers r8, r9, xmm2).
pub fn emit_store(a: &mut Asm, v: Sv) {
    emit_stores(a, &[v]);
}

/// Stores vector `x` at [base + off]: x,y as 8 bytes, then (z, tag) as the
/// 8-byte tag word (clobbers r8, r9, xmm2).
pub fn store_vec_to(a: &mut Asm, base: R, off: i32, x: X) {
    static ONE16: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ONE16.get_or_init(|| {
        std::arch::is_x86_feature_detected!("sse4.1") && std::env::var("LUAU_CODEGEN_OFF").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0) & 262144 == 0
    }) {
        // one 16-byte store with the tag in lane 3: the narrower loads that
        // read the slot back (x,y / z / tag word) all forward from it, and z
        // never goes through a general-purpose register
        a.movaps(X(2), x);
        a.mov_ri(R::R9, LUA_TVECTOR as u64);
        a.pinsrd(X(2), R::R9, 3);
        a.movups_store(base, off, X(2));
        return;
    }
    a.movq_store(base, off, x);
    a.movhlps(X(2), x);
    a.movd_rx(R::R9, X(2));
    a.mov_ri(R::R8, VEC_TAG);
    a.or_rr(R::R9, R::R8);
    a.store(base, off + TAGWORD, R::R9);
}

/// Loads a machine register back from the stack slot (clobbers xmm2).
pub fn emit_reload(a: &mut Asm, v: Rl) {
    let off = reg(v.r as i32);
    match v.tag {
        TNUM => {
            if v.x != 0 {
                a.movsd_load(X(v.x), R::R13, off);
            }
            if v.g != 0 {
                let g = GP[v.g as usize - 1];
                a.cvttsd2si64_m(g, R::R13, off);
                a.mov_rr32(g, g);
            }
        }
        TVEC => load_vec(a, X(v.x), X(2), R::R13, off),
        _ => a.load(GP[v.g as usize - 1], R::R13, off),
    }
}

#[derive(Clone)]
pub struct St {
    /// known type tag of each VM register (`T_ANY`: unknown)
    pub tag: [u8; 256],
    /// the stack slot's tag word matches `tag`
    pub memtag: [bool; 256],
    /// the stack slot's value is stale (the value lives in `x` and/or `g`)
    pub dirty: [bool; 256],
    /// xmm caching the register (0: none)
    pub x: [u8; 256],
    /// index + 1 into `GP` (0: none)
    pub g: [u8; 256],
    /// for numbers: `g` is exactly the value (not just its u32 conversion)
    pub gexact: [bool; 256],
    xown: [i16; 16],
    gown: [i16; 6],
    xlru: [u32; 16],
    glru: [u32; 6],
    tick: u32,
    /// machine registers handed out during the current instruction
    pinx: u16,
    ping: u8,
    /// the function environment is known to be "safe" (builtins not replaced)
    pub safeenv: bool,
    /// bumped whenever the set of values to store changes
    pub ver: u32,
    /// value number of each register: changes whenever the register is
    /// written (equal ids = equal values)
    pub vid: [u32; 256],
    nextvid: u32,
    /// xmm registers no register owns that still hold value `xorph` (a known
    /// common subexpression; 0: none)
    xorph: [u32; 16],
    /// general-purpose registers no register owns holding the i32
    /// truncation of value `gorph` (0: none)
    gorph: [u32; 6],
    /// pure operations already computed in this block: (op, operand a,
    /// operand b, result value)
    cse: Vec<(u8, u64, u64, u32)>,
}

impl St {
    pub fn new() -> St {
        St {
            tag: [T_ANY; 256],
            memtag: [true; 256],
            dirty: [false; 256],
            x: [0; 256],
            g: [0; 256],
            gexact: [false; 256],
            xown: [-1; 16],
            gown: [-1; 6],
            xlru: [0; 16],
            glru: [0; 6],
            tick: 0,
            pinx: 0,
            ping: 0,
            safeenv: false,
            ver: 0,
            vid: std::array::from_fn(|r| r as u32 + 1),
            nextvid: 257,
            xorph: [0; 16],
            gorph: [0; 6],
            cse: Vec::new(),
        }
    }

    /// `r` holds a new value.
    pub fn fresh(&mut self, r: usize) {
        self.vid[r] = self.nextvid;
        self.nextvid += 1;
    }

    /// Nothing is known about any value any more.
    fn invalidate_values(&mut self) {
        for r in 0..256 {
            self.fresh(r);
        }
        self.cse.clear();
        self.drop_orphans();
    }

    /// Machine registers not owned by a VM register are clobbered (calls,
    /// slow paths that rejoin).
    pub fn drop_orphans(&mut self) {
        self.xorph = [0; 16];
        self.gorph = [0; 6];
    }

    fn is_cse_result(&self, v: u32) -> bool {
        self.cse.iter().any(|e| e.3 == v)
    }

    /// Forgets the entries of operation `op` (their inputs changed).
    pub fn cse_remove_op(&mut self, op: u8) {
        self.cse.retain(|e| e.0 != op);
    }

    /// Machine registers of VM registers that are dead from here on are
    /// released without storing them (a stale stack slot then holds
    /// nothing known; common subexpressions stay available as orphans).
    pub fn release_dead(&mut self, live: &super::live::Mask) {
        for r in self.owners() {
            if !super::live::has(live, r) {
                if self.dirty[r] {
                    self.dirty[r] = false;
                    self.kill(r);
                } else {
                    self.unmap(r);
                    self.ver += 1;
                }
            }
        }
    }

    /// Loop-carried register `r` (a number or vector, by `t`) lives in xmm
    /// `x` from here: loaded from its stack slot when `load`, and treated as
    /// stale there (so every exit stores it). `x` must not hold a value
    /// anything still needs.
    pub fn attach_fixed(&mut self, a: &mut Asm, r: usize, t: u8, x: u8, load: bool) {
        self.unmap(r);
        let prev = self.xown[x as usize];
        if prev >= 0 {
            self.x[prev as usize] = 0;
            self.dirty[prev as usize] = false;
        }
        self.xorph[x as usize] = 0;
        if load {
            if t == TVEC {
                load_vec(a, X(x), X(2), R::R13, reg(r as i32));
            } else {
                a.movsd_load(X(x), R::R13, reg(r as i32));
            }
        }
        self.x[r] = x;
        self.xown[x as usize] = r as i16;
        self.pinx |= 1 << x;
        self.tick += 1;
        self.xlru[x as usize] = self.tick;
        self.tag[r] = t;
        self.dirty[r] = true;
        self.memtag[r] = false;
        self.fresh(r);
        self.ver += 1;
    }

    /// The value an earlier identical pure operation produced, if any.
    pub fn cse_find(&self, op: u8, a: u64, b: u64) -> Option<u32> {
        self.cse.iter().rev().find(|e| e.0 == op && e.1 == a && e.2 == b).map(|e| e.3)
    }

    pub fn cse_add(&mut self, op: u8, a: u64, b: u64, v: u32) {
        if self.cse.len() >= 64 {
            self.cse.remove(0);
        }
        self.cse.push((op, a, b, v));
    }

    /// Defines `r` as the number with value id `v` when that value is still
    /// held by another register or an orphaned xmm; false otherwise.
    pub fn def_num_from_vid(&mut self, a: &mut Asm, r: usize, v: u32) -> bool {
        if self.vid[r] == v && self.tag[r] == TNUM {
            return true;
        }
        if let Some(s) = (0..256).find(|&s| s != r && self.vid[s] == v && self.tag[s] == TNUM) {
            let xs = self.num_x(a, s);
            let xd = self.def_num(a, r);
            if xd != xs {
                a.movapd(xd, xs);
            }
            self.vid[r] = v;
            return true;
        }
        if let Some(x) = (XS as usize..16).find(|&x| self.xorph[x] == v && self.xown[x] < 0) {
            let x = x as u8;
            self.unmap(r);
            self.xorph[x as usize] = 0;
            self.x[r] = x;
            self.xown[x as usize] = r as i16;
            self.use_x(x);
            self.def_common(r, TNUM);
            self.vid[r] = v;
            return true;
        }
        false
    }

    /// A double constant in an xmm register, kept (unowned) for reuse within
    /// the block.
    pub fn const_x(&mut self, a: &mut Asm, bits: u64) -> X {
        const CSE_CONST: u8 = 101;
        if let Some(v) = self.cse_find(CSE_CONST, bits, 0) {
            if let Some(x) = (XS as usize..16).find(|&x| self.xorph[x] == v && self.xown[x] < 0) {
                return self.use_x(x as u8);
            }
        }
        let v = self.nextvid;
        self.nextvid += 1;
        let x = self.alloc_x(a);
        if bits == 0 {
            a.xorps(X(x), X(x));
        } else {
            a.mov_ri(R::Rax, bits);
            a.movq_xr(X(x), R::Rax);
        }
        self.xorph[x as usize] = v;
        self.cse.retain(|e| !(e.0 == CSE_CONST && e.1 == bits));
        self.cse_add(CSE_CONST, bits, 0, v);
        X(x)
    }

    /// The number in `r` truncated to i32 like `cvttsd2si` (as the VM's
    /// integer arguments), zero-extended; cached per value.
    pub fn trunc32(&mut self, a: &mut Asm, r: usize) -> R {
        let v = self.vid[r];
        if let Some(g) = (0..6).find(|&g| self.gorph[g] == v && self.gown[g] < 0) {
            return self.use_g(g as u8 + 1);
        }
        let x = self.num_x(a, r);
        let g = self.alloc_g(a);
        let gr = GP[g as usize - 1];
        a.cvttsd2si_x(gr, x);
        self.gorph[g as usize - 1] = v;
        gr
    }

    pub fn begin_insn(&mut self) {
        self.pinx = 0;
        self.ping = 0;
    }

    /// Releases the general-purpose registers pinned so far in this
    /// instruction (for templates that fold over many operands).
    pub fn unpin_g(&mut self) {
        self.ping = 0;
    }

    fn sv(&self, r: usize) -> Sv {
        Sv { r: r as u8, tag: self.tag[r], x: self.x[r], g: if self.x[r] != 0 { 0 } else { self.g[r] }, memtag: self.memtag[r] }
    }

    /// Writes register `r` to its stack slot if it is stale there.
    pub fn flush(&mut self, a: &mut Asm, r: usize) {
        if self.dirty[r] {
            emit_store(a, self.sv(r));
            self.dirty[r] = false;
            self.memtag[r] = true;
            self.ver += 1;
        }
    }

    fn owners(&self) -> Vec<usize> {
        let mut v = Vec::new();
        for x in XS as usize..16 {
            if self.xown[x] >= 0 {
                v.push(self.xown[x] as usize);
            }
        }
        for g in 0..6 {
            if self.gown[g] >= 0 && self.x[self.gown[g] as usize] == 0 {
                v.push(self.gown[g] as usize);
            }
        }
        v
    }

    /// Writes every stale value to the stack (the cache stays valid).
    pub fn flush_all(&mut self, a: &mut Asm) {
        self.flush_live(a, &super::live::ALL);
    }

    /// Writes the stale values of registers in `live` to the stack.
    pub fn flush_live(&mut self, a: &mut Asm, live: &super::live::Mask) {
        let vs = self.snapshot_live(live);
        if vs.is_empty() {
            return;
        }
        emit_stores(a, &vs);
        for v in vs {
            self.dirty[v.r as usize] = false;
            self.memtag[v.r as usize] = true;
        }
        self.ver += 1;
    }

    /// Stale values of registers in `live`.
    pub fn snapshot_live(&self, live: &super::live::Mask) -> Vec<Sv> {
        self.owners().into_iter().filter(|&r| self.dirty[r] && super::live::has(live, r)).map(|r| self.sv(r)).collect()
    }

    /// Writes stale values of registers below `limit` (others are dead).
    pub fn flush_below(&mut self, a: &mut Asm, limit: usize) {
        for r in self.owners() {
            if r < limit {
                self.flush(a, r);
            }
        }
    }

    /// Writes stale values of registers in `lo..hi`.
    pub fn flush_range(&mut self, a: &mut Asm, lo: usize, hi: usize) {
        for r in self.owners() {
            if r >= lo && r < hi {
                self.flush(a, r);
            }
        }
    }

    /// Forgets all machine registers. Values that were still stale are dead
    /// (callers flush what is needed first); their slots hold something
    /// older, so nothing is known about those registers afterwards.
    pub fn drop_all(&mut self) {
        for r in self.owners() {
            if self.dirty[r] {
                self.tag[r] = T_ANY;
                self.memtag[r] = true;
                self.fresh(r);
            }
        }
        self.drop_orphans();
        for x in XS as usize..16 {
            let r = self.xown[x];
            if r >= 0 {
                self.x[r as usize] = 0;
                self.dirty[r as usize] = false;
                self.xown[x] = -1;
            }
        }
        for g in 0..6 {
            let r = self.gown[g];
            if r >= 0 {
                self.g[r as usize] = 0;
                self.dirty[r as usize] = false;
                self.gown[g] = -1;
            }
        }
        self.ver += 1;
    }

    /// Everything to the stack, nothing known (join points).
    pub fn reset(&mut self, a: &mut Asm, live: &super::live::Mask) {
        self.flush_live(a, live);
        self.forget();
    }

    /// Forgets everything without storing (after code that never falls
    /// through, or once everything is in memory).
    pub fn forget(&mut self) {
        self.drop_all();
        self.invalidate_values();
        self.tag = [T_ANY; 256];
        self.memtag = [true; 256];
        self.safeenv = false;
    }

    /// Type knowledge lost for all registers (they are all in memory).
    pub fn forget_tags(&mut self) {
        self.drop_all();
        self.invalidate_values();
        self.tag = [T_ANY; 256];
        self.memtag = [true; 256];
    }

    fn unmap(&mut self, r: usize) {
        let x = self.x[r];
        if x != 0 {
            self.xown[x as usize] = -1;
            self.x[r] = 0;
            // a known common subexpression stays available
            if self.tag[r] == TNUM && self.is_cse_result(self.vid[r]) {
                self.xorph[x as usize] = self.vid[r];
            }
        }
        let g = self.g[r];
        if g != 0 {
            self.gown[g as usize - 1] = -1;
            self.g[r] = 0;
        }
        self.gexact[r] = false;
    }

    /// `r` is about to be overwritten in memory with a value of unknown type.
    pub fn kill(&mut self, r: usize) {
        self.unmap(r);
        self.fresh(r);
        self.dirty[r] = false;
        self.tag[r] = T_ANY;
        self.memtag[r] = true;
        self.ver += 1;
    }

    /// `r` was written in memory with a value of type `t`.
    pub fn set_mem(&mut self, r: usize, t: u8) {
        self.kill(r);
        self.tag[r] = t;
    }

    fn evict_x(&mut self, a: &mut Asm, x: u8) {
        let r = self.xown[x as usize];
        if r >= 0 {
            let r = r as usize;
            self.flush(a, r);
            self.x[r] = 0;
            self.xown[x as usize] = -1;
            self.ver += 1;
        }
    }

    fn evict_g(&mut self, a: &mut Asm, gi: usize) {
        let r = self.gown[gi];
        if r >= 0 {
            let r = r as usize;
            if self.x[r] == 0 {
                self.flush(a, r);
            }
            self.g[r] = 0;
            self.gexact[r] = false;
            self.gown[gi] = -1;
            self.ver += 1;
        }
    }

    fn alloc_x(&mut self, a: &mut Asm) -> u8 {
        let mut pick = 0u8;
        for x in XS..16 {
            if self.xown[x as usize] < 0 && self.xorph[x as usize] == 0 && self.pinx & (1 << x) == 0 {
                pick = x;
                break;
            }
        }
        if pick == 0 {
            let mut best = u32::MAX;
            for x in XS..16 {
                if self.xown[x as usize] < 0 && self.pinx & (1 << x) == 0 && self.xlru[x as usize] < best {
                    best = self.xlru[x as usize];
                    pick = x;
                }
            }
        }
        if pick == 0 {
            let mut best = u32::MAX;
            for x in XS..16 {
                if self.pinx & (1 << x) == 0 && self.xlru[x as usize] < best {
                    best = self.xlru[x as usize];
                    pick = x;
                }
            }
            assert!(pick != 0, "all xmm registers pinned");
            self.evict_x(a, pick);
        }
        self.xorph[pick as usize] = 0;
        self.pinx |= 1 << pick;
        self.tick += 1;
        self.xlru[pick as usize] = self.tick;
        pick
    }

    fn alloc_g(&mut self, a: &mut Asm) -> u8 {
        let mut pick = usize::MAX;
        for g in 0..6 {
            if self.gown[g] < 0 && self.gorph[g] == 0 && self.ping & (1 << g) == 0 {
                pick = g;
                break;
            }
        }
        if pick == usize::MAX {
            let mut best = u32::MAX;
            for g in 0..6 {
                if self.gown[g] < 0 && self.ping & (1 << g) == 0 && self.glru[g] < best {
                    best = self.glru[g];
                    pick = g;
                }
            }
        }
        if pick == usize::MAX {
            let mut best = u32::MAX;
            for g in 0..6 {
                if self.ping & (1 << g) == 0 && self.glru[g] < best {
                    best = self.glru[g];
                    pick = g;
                }
            }
            assert!(pick != usize::MAX, "all general-purpose cache registers pinned");
            self.evict_g(a, pick);
        }
        self.gorph[pick] = 0;
        self.ping |= 1 << pick;
        self.tick += 1;
        self.glru[pick] = self.tick;
        pick as u8 + 1
    }

    fn use_x(&mut self, x: u8) -> X {
        self.pinx |= 1 << x;
        self.tick += 1;
        self.xlru[x as usize] = self.tick;
        X(x)
    }

    fn use_g(&mut self, g: u8) -> R {
        self.ping |= 1 << (g - 1);
        self.tick += 1;
        self.glru[g as usize - 1] = self.tick;
        GP[g as usize - 1]
    }

    /// The number in `r` as a double (requires `tag[r] == TNUM`).
    pub fn num_x(&mut self, a: &mut Asm, r: usize) -> X {
        debug_assert_eq!(self.tag[r], TNUM);
        if self.x[r] != 0 {
            return self.use_x(self.x[r]);
        }
        // keep the general-purpose copy alive while allocating
        if self.g[r] != 0 {
            self.ping |= 1 << (self.g[r] - 1);
        }
        let x = self.alloc_x(a);
        if self.g[r] != 0 && self.gexact[r] {
            a.cvtsi2sd64(X(x), GP[self.g[r] as usize - 1]);
        } else {
            debug_assert!(!self.dirty[r]);
            a.movsd_load(X(x), R::R13, reg(r as i32));
        }
        self.x[r] = x;
        self.xown[x as usize] = r as i16;
        self.ver += 1;
        X(x)
    }

    /// The number in `r` converted like `luai_num2unsigned`, zero-extended
    /// (requires `tag[r] == TNUM`).
    pub fn num_u32(&mut self, a: &mut Asm, r: usize) -> R {
        debug_assert_eq!(self.tag[r], TNUM);
        if self.g[r] != 0 {
            return self.use_g(self.g[r]);
        }
        if self.x[r] != 0 {
            self.pinx |= 1 << self.x[r];
        }
        let g = self.alloc_g(a);
        let gr = GP[g as usize - 1];
        if self.x[r] != 0 {
            a.cvttsd2si64_x(gr, X(self.x[r]));
        } else {
            a.cvttsd2si64_m(gr, R::R13, reg(r as i32));
        }
        a.mov_rr32(gr, gr);
        self.g[r] = g;
        self.gexact[r] = false;
        self.gown[g as usize - 1] = r as i16;
        gr
    }

    /// Keeps a copy of the number in `r`, just checked to be a non-negative
    /// integer and held in `src`, as an exact u32 (later array indexing and
    /// bit operations skip the conversion).
    pub fn attach_exact_u32(&mut self, a: &mut Asm, r: usize, src: R) {
        if self.tag[r] != TNUM || self.g[r] != 0 {
            return;
        }
        if self.x[r] != 0 {
            self.pinx |= 1 << self.x[r];
        }
        let g = self.alloc_g(a);
        let gr = GP[g as usize - 1];
        a.mov_rr32(gr, src);
        self.g[r] = g;
        self.gexact[r] = true;
        self.gown[g as usize - 1] = r as i16;
        self.ver += 1;
    }

    /// True when `r` is a number known to be an exact u32 held in a register.
    pub fn has_exact_u32(&self, r: usize) -> bool {
        self.tag[r] == TNUM && self.g[r] != 0 && self.gexact[r]
    }

    /// The `integer` in `r` (requires `tag[r] == TINT`).
    pub fn int_g(&mut self, a: &mut Asm, r: usize) -> R {
        debug_assert_eq!(self.tag[r], TINT);
        self.ptr_g(a, r)
    }

    /// The 8-byte payload of `r` (table/buffer pointer, integer) in a
    /// register; the value must not be a lazily stored number or vector.
    pub fn ptr_g(&mut self, a: &mut Asm, r: usize) -> R {
        if self.g[r] != 0 {
            return self.use_g(self.g[r]);
        }
        let g = self.alloc_g(a);
        a.load(GP[g as usize - 1], R::R13, reg(r as i32));
        self.g[r] = g;
        self.gown[g as usize - 1] = r as i16;
        GP[g as usize - 1]
    }

    /// The vector in `r` as packed floats (requires `tag[r] == TVEC`).
    pub fn vec_x(&mut self, a: &mut Asm, r: usize) -> X {
        debug_assert_eq!(self.tag[r], TVEC);
        if self.x[r] != 0 {
            return self.use_x(self.x[r]);
        }
        let x = self.alloc_x(a);
        load_vec(a, X(x), X(2), R::R13, reg(r as i32));
        self.x[r] = x;
        self.xown[x as usize] = r as i16;
        self.ver += 1;
        X(x)
    }

    fn def_common(&mut self, r: usize, t: u8) {
        self.fresh(r);
        self.memtag[r] = self.tag[r] == t && self.memtag[r];
        self.tag[r] = t;
        self.dirty[r] = true;
        self.ver += 1;
    }

    /// Defines `r` as a number; returns the xmm to write it to. Call after
    /// the instruction's last guard.
    pub fn def_num(&mut self, a: &mut Asm, r: usize) -> X {
        let mut keep = self.x[r];
        if keep != 0 && self.tag[r] == TNUM && self.is_cse_result(self.vid[r]) {
            keep = 0; // unmap keeps the old value as an orphan
        }
        if keep != 0 {
            self.x[r] = 0;
            self.xown[keep as usize] = -1;
        }
        self.unmap(r);
        let x = if keep != 0 {
            self.use_x(keep).0
        } else {
            self.alloc_x(a)
        };
        self.x[r] = x;
        self.xown[x as usize] = r as i16;
        self.def_common(r, TNUM);
        X(x)
    }

    /// Defines `r` as a number that is an exact u32; returns the register to
    /// write it to (zero-extended).
    pub fn def_u32(&mut self, a: &mut Asm, r: usize) -> R {
        let g = self.def_g(a, r);
        self.gexact[r] = true;
        self.def_common(r, TNUM);
        g
    }

    /// Defines `r` as an `integer`.
    pub fn def_int(&mut self, a: &mut Asm, r: usize) -> R {
        let g = self.def_g(a, r);
        self.def_common(r, TINT);
        g
    }

    fn def_g(&mut self, a: &mut Asm, r: usize) -> R {
        let keep = self.g[r];
        if keep != 0 {
            self.g[r] = 0;
            self.gown[keep as usize - 1] = -1;
        }
        self.unmap(r);
        let g = if keep != 0 {
            self.use_g(keep);
            keep
        } else {
            self.alloc_g(a)
        };
        self.g[r] = g;
        self.gown[g as usize - 1] = r as i16;
        GP[g as usize - 1]
    }

    /// Defines `r` as a vector; returns the xmm to write it to.
    pub fn def_vec(&mut self, a: &mut Asm, r: usize) -> X {
        let x = self.def_num(a, r);
        self.tag[r] = TVEC;
        self.memtag[r] = false;
        x
    }

    /// Values that are stale on the stack.
    pub fn snapshot(&self) -> Vec<Sv> {
        self.owners().into_iter().filter(|&r| self.dirty[r]).map(|r| self.sv(r)).collect()
    }

    /// Machine registers currently caching VM registers.
    pub fn cached(&self) -> Vec<Rl> {
        let mut v = Vec::new();
        for x in XS as usize..16 {
            if self.xown[x] >= 0 {
                let r = self.xown[x] as usize;
                v.push(Rl { r: r as u8, tag: self.tag[r], x: x as u8, g: 0 });
            }
        }
        for g in 0..6 {
            if self.gown[g] >= 0 {
                let r = self.gown[g] as usize;
                v.push(Rl { r: r as u8, tag: self.tag[r], x: 0, g: g as u8 + 1 });
            }
        }
        v
    }

    pub fn any_dirty(&self) -> bool {
        self.owners().into_iter().any(|r| self.dirty[r])
    }
}
