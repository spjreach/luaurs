//! Minimal x86-64 assembler for the native code generator.
//!
//! Only the instruction forms the code generator emits are supported. Memory
//! operands are always `[base + disp32]` (or `[base + index*8]`), which keeps
//! the encoder simple: mod=10 with a SIB byte for rsp/r12 bases.

#![allow(dead_code)]

/// General-purpose registers (encoding numbers).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum R {
    Rax = 0,
    Rcx = 1,
    Rdx = 2,
    Rbx = 3,
    Rsp = 4,
    Rbp = 5,
    Rsi = 6,
    Rdi = 7,
    R8 = 8,
    R9 = 9,
    R10 = 10,
    R11 = 11,
    R12 = 12,
    R13 = 13,
    R14 = 14,
    R15 = 15,
}

/// SSE registers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct X(pub u8);

/// Condition codes (low nibble of `Jcc`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Cond {
    B = 0x2,
    AE = 0x3,
    E = 0x4,
    NE = 0x5,
    BE = 0x6,
    A = 0x7,
    P = 0xa,
    NP = 0xb,
    L = 0xc,
    GE = 0xd,
    LE = 0xe,
    G = 0xf,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Label(pub u32);

pub struct Asm {
    pub code: Vec<u8>,
    labels: Vec<Option<u32>>,
    fixups: Vec<(u32, Label)>,
    /// index register for the next memory operand: `[base + index + disp]`
    pidx: Option<R>,
}

impl Asm {
    pub fn new() -> Self {
        Asm { code: Vec::with_capacity(4096), labels: Vec::new(), fixups: Vec::new(), pidx: None }
    }

    pub fn pos(&self) -> u32 {
        self.code.len() as u32
    }

    pub fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() as u32 - 1)
    }

    pub fn bind(&mut self, l: Label) {
        debug_assert!(self.labels[l.0 as usize].is_none());
        self.labels[l.0 as usize] = Some(self.pos());
    }

    /// Offset of a bound label.
    pub fn label_offset(&self, l: Label) -> u32 {
        self.labels[l.0 as usize].expect("unbound label")
    }

    pub fn is_bound(&self, l: Label) -> bool {
        self.labels[l.0 as usize].is_some()
    }

    /// Resolves all label references; returns false if a label is unbound.
    pub fn finish(&mut self) -> bool {
        for &(at, l) in &self.fixups {
            let Some(target) = self.labels[l.0 as usize] else { return false };
            let rel = target as i64 - (at as i64 + 4);
            self.code[at as usize..at as usize + 4].copy_from_slice(&(rel as i32).to_le_bytes());
        }
        true
    }

    #[inline]
    fn byte(&mut self, b: u8) {
        self.code.push(b);
    }
    #[inline]
    fn dword(&mut self, v: u32) {
        self.code.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn qword(&mut self, v: u64) {
        self.code.extend_from_slice(&v.to_le_bytes());
    }

    #[inline]
    /// The next memory operand gets `index` added (scale 1).
    pub fn set_index(&mut self, index: R) {
        debug_assert!(self.pidx.is_none() && index != R::Rsp);
        self.pidx = Some(index);
    }

    fn rex(&mut self, w: bool, reg: u8, index: u8, base: u8, force: bool) {
        let index = index | self.pidx.map_or(0, |r| r as u8);
        let v = 0x40 | ((w as u8) << 3) | (((reg >> 3) & 1) << 2) | (((index >> 3) & 1) << 1) | ((base >> 3) & 1);
        if v != 0x40 || force {
            self.byte(v);
        }
    }

    /// ModRM (+SIB) + disp32 for `[base + disp]` with the given reg field.
    fn mem(&mut self, reg: u8, base: R, disp: i32) {
        let b = base as u8;
        if let Some(x) = self.pidx.take() {
            let md = if disp == 0 && b & 7 != 5 { 0 } else if (-128..=127).contains(&disp) { 1 } else { 2 };
            self.byte((md << 6) | ((reg & 7) << 3) | 4);
            self.byte(((x as u8 & 7) << 3) | (b & 7));
            match md {
                1 => self.byte(disp as i8 as u8),
                2 => self.dword(disp as u32),
                _ => {}
            }
            return;
        }
        if disp == 0 && b & 7 != 5 {
            self.byte(((reg & 7) << 3) | (b & 7));
            if b & 7 == 4 {
                self.byte(0x24);
            }
        } else if (-128..=127).contains(&disp) {
            self.byte(0x40 | ((reg & 7) << 3) | (b & 7));
            if b & 7 == 4 {
                self.byte(0x24);
            }
            self.byte(disp as i8 as u8);
        } else {
            self.byte(0x80 | ((reg & 7) << 3) | (b & 7));
            if b & 7 == 4 {
                self.byte(0x24);
            }
            self.dword(disp as u32);
        }
    }

    fn modrm_rr(&mut self, reg: u8, rm: u8) {
        self.byte(0xc0 | ((reg & 7) << 3) | (rm & 7));
    }

    // -- integer ----------------------------------------------------------

    pub fn push(&mut self, r: R) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0x50 + (r as u8 & 7));
    }
    pub fn pop(&mut self, r: R) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0x58 + (r as u8 & 7));
    }
    pub fn ret(&mut self) {
        self.byte(0xc3);
    }

    /// mov dst, src (64-bit)
    pub fn mov_rr(&mut self, dst: R, src: R) {
        self.rex(true, src as u8, 0, dst as u8, false);
        self.byte(0x89);
        self.modrm_rr(src as u8, dst as u8);
    }
    /// mov dst32, src32 (zero-extends)
    pub fn mov_rr32(&mut self, dst: R, src: R) {
        self.rex(false, src as u8, 0, dst as u8, false);
        self.byte(0x89);
        self.modrm_rr(src as u8, dst as u8);
    }
    /// mov dst, imm64
    pub fn mov_ri(&mut self, dst: R, imm: u64) {
        if imm <= u32::MAX as u64 {
            // mov r32, imm32 zero-extends
            self.rex(false, 0, 0, dst as u8, false);
            self.byte(0xb8 + (dst as u8 & 7));
            self.dword(imm as u32);
        } else {
            self.rex(true, 0, 0, dst as u8, false);
            self.byte(0xb8 + (dst as u8 & 7));
            self.qword(imm);
        }
    }
    /// mov dst, qword [base + disp]
    pub fn load(&mut self, dst: R, base: R, disp: i32) {
        self.rex(true, dst as u8, 0, base as u8, false);
        self.byte(0x8b);
        self.mem(dst as u8, base, disp);
    }
    /// mov dst32, dword [base + disp]
    pub fn load32(&mut self, dst: R, base: R, disp: i32) {
        self.rex(false, dst as u8, 0, base as u8, false);
        self.byte(0x8b);
        self.mem(dst as u8, base, disp);
    }
    /// mov qword [base + disp], src
    pub fn store(&mut self, base: R, disp: i32, src: R) {
        self.rex(true, src as u8, 0, base as u8, false);
        self.byte(0x89);
        self.mem(src as u8, base, disp);
    }
    /// mov dword [base + disp], imm32
    pub fn store32_i(&mut self, base: R, disp: i32, imm: u32) {
        self.rex(false, 0, 0, base as u8, false);
        self.byte(0xc7);
        self.mem(0, base, disp);
        self.dword(imm);
    }
    /// mov qword [base + disp], imm32 (sign-extended)
    pub fn store64_i(&mut self, base: R, disp: i32, imm: i32) {
        self.rex(true, 0, 0, base as u8, false);
        self.byte(0xc7);
        self.mem(0, base, disp);
        self.dword(imm as u32);
    }
    /// cmp dword [base + disp], imm32
    pub fn cmp32_mi(&mut self, base: R, disp: i32, imm: i32) {
        self.rex(false, 0, 0, base as u8, false);
        self.byte(0x81);
        self.mem(7, base, disp);
        self.dword(imm as u32);
    }
    /// cmp qword [base + disp], imm32
    pub fn cmp64_mi(&mut self, base: R, disp: i32, imm: i32) {
        self.rex(true, 0, 0, base as u8, false);
        self.byte(0x81);
        self.mem(7, base, disp);
        self.dword(imm as u32);
    }
    /// cmp a, b (64-bit)
    pub fn cmp_rr(&mut self, a: R, b: R) {
        self.rex(true, b as u8, 0, a as u8, false);
        self.byte(0x39);
        self.modrm_rr(b as u8, a as u8);
    }
    /// cmp r, qword [base + disp]
    pub fn cmp_rm(&mut self, r: R, base: R, disp: i32) {
        self.rex(true, r as u8, 0, base as u8, false);
        self.byte(0x3b);
        self.mem(r as u8, base, disp);
    }
    /// test r, r (64-bit)
    pub fn test_rr(&mut self, a: R, b: R) {
        self.rex(true, b as u8, 0, a as u8, false);
        self.byte(0x85);
        self.modrm_rr(b as u8, a as u8);
    }
    /// sub dst, src
    pub fn sub_rr(&mut self, dst: R, src: R) {
        self.rex(true, src as u8, 0, dst as u8, false);
        self.byte(0x29);
        self.modrm_rr(src as u8, dst as u8);
    }
    /// add/sub r, imm32 (64-bit)
    pub fn add_ri(&mut self, r: R, imm: i32) {
        self.rex(true, 0, 0, r as u8, false);
        self.byte(0x81);
        self.modrm_rr(0, r as u8);
        self.dword(imm as u32);
    }
    pub fn sub_ri(&mut self, r: R, imm: i32) {
        self.rex(true, 0, 0, r as u8, false);
        self.byte(0x81);
        self.modrm_rr(5, r as u8);
        self.dword(imm as u32);
    }
    /// sub rsp, imm8 / add rsp, imm8 (fixed encodings, used in the prologue
    /// described by the unwind info)
    pub fn sub_rsp8(&mut self, imm: u8) {
        self.byte(0x48);
        self.byte(0x83);
        self.byte(0xec);
        self.byte(imm);
    }
    pub fn add_rsp8(&mut self, imm: u8) {
        self.byte(0x48);
        self.byte(0x83);
        self.byte(0xc4);
        self.byte(imm);
    }
    /// shl/shr r, imm8
    pub fn shr_ri(&mut self, r: R, imm: u8) {
        self.rex(true, 0, 0, r as u8, false);
        self.byte(0xc1);
        self.modrm_rr(5, r as u8);
        self.byte(imm);
    }
    pub fn shl_ri(&mut self, r: R, imm: u8) {
        self.rex(true, 0, 0, r as u8, false);
        self.byte(0xc1);
        self.modrm_rr(4, r as u8);
        self.byte(imm);
    }

    /// add dst, src (64-bit)
    pub fn add_rr(&mut self, dst: R, src: R) {
        self.rex(true, src as u8, 0, dst as u8, false);
        self.byte(0x01);
        self.modrm_rr(src as u8, dst as u8);
    }
    /// sub r32, imm32
    pub fn sub32_ri(&mut self, r: R, imm: i32) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0x81);
        self.modrm_rr(5, r as u8);
        self.dword(imm as u32);
    }
    /// and r32, imm32
    pub fn and32_ri(&mut self, r: R, imm: i32) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0x81);
        self.modrm_rr(4, r as u8);
        self.dword(imm as u32);
    }
    /// cmp r32, imm32
    pub fn cmp32_ri(&mut self, r: R, imm: i32) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0x81);
        self.modrm_rr(7, r as u8);
        self.dword(imm as u32);
    }
    /// cmp r, imm32 (64-bit)
    pub fn cmp_ri(&mut self, r: R, imm: i32) {
        self.rex(true, 0, 0, r as u8, false);
        self.byte(0x81);
        self.modrm_rr(7, r as u8);
        self.dword(imm as u32);
    }
    /// movzx r32, byte [base + disp]
    pub fn movzx8(&mut self, dst: R, base: R, disp: i32) {
        self.rex(false, dst as u8, 0, base as u8, false);
        self.byte(0x0f);
        self.byte(0xb6);
        self.mem(dst as u8, base, disp);
    }
    /// cmp r32, dword [base + disp]
    pub fn cmp32_rm(&mut self, r: R, base: R, disp: i32) {
        self.rex(false, r as u8, 0, base as u8, false);
        self.byte(0x3b);
        self.mem(r as u8, base, disp);
    }
    /// cmp byte [base + disp], imm8
    pub fn cmp8_mi(&mut self, base: R, disp: i32, imm: u8) {
        self.rex(false, 0, 0, base as u8, false);
        self.byte(0x80);
        self.mem(7, base, disp);
        self.byte(imm);
    }
    /// test byte [base + disp], imm8
    pub fn test8_mi(&mut self, base: R, disp: i32, imm: u8) {
        self.rex(false, 0, 0, base as u8, false);
        self.byte(0xf6);
        self.mem(0, base, disp);
        self.byte(imm);
    }

    // -- control flow -----------------------------------------------------

    pub fn jmp(&mut self, l: Label) {
        self.byte(0xe9);
        self.fixups.push((self.pos(), l));
        self.dword(0);
    }
    pub fn jcc(&mut self, c: Cond, l: Label) {
        self.byte(0x0f);
        self.byte(0x80 | c as u8);
        self.fixups.push((self.pos(), l));
        self.dword(0);
    }
    /// jmp r
    pub fn jmp_r(&mut self, r: R) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0xff);
        self.modrm_rr(4, r as u8);
    }
    /// call r
    pub fn call_r(&mut self, r: R) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0xff);
        self.modrm_rr(2, r as u8);
    }
    /// jmp qword [base + index*8]
    pub fn jmp_table(&mut self, base: R, index: R) {
        assert!(base as u8 & 7 != 5, "rbp/r13 base needs a displacement");
        self.rex(false, 0, index as u8, base as u8, false);
        self.byte(0xff);
        self.byte(0x24); // mod=00 reg=4 rm=100 (SIB)
        self.byte(0xc0 | ((index as u8 & 7) << 3) | (base as u8 & 7)); // scale 8
    }

    // -- SSE --------------------------------------------------------------

    fn sse_rm(&mut self, prefix: u8, op: u8, reg: X, base: R, disp: i32) {
        self.byte(prefix);
        self.rex(false, reg.0, 0, base as u8, false);
        self.byte(0x0f);
        self.byte(op);
        self.mem(reg.0, base, disp);
    }
    fn sse_rr(&mut self, prefix: u8, op: u8, dst: X, src: X) {
        if prefix != 0 {
            self.byte(prefix);
        }
        self.rex(false, dst.0, 0, src.0, false);
        self.byte(0x0f);
        self.byte(op);
        self.modrm_rr(dst.0, src.0);
    }
    pub fn movsd_load(&mut self, dst: X, base: R, disp: i32) {
        self.sse_rm(0xf2, 0x10, dst, base, disp);
    }
    pub fn movsd_store(&mut self, base: R, disp: i32, src: X) {
        self.sse_rm(0xf2, 0x11, src, base, disp);
    }
    pub fn addsd_m(&mut self, dst: X, base: R, disp: i32) {
        self.sse_rm(0xf2, 0x58, dst, base, disp);
    }
    pub fn mulsd_m(&mut self, dst: X, base: R, disp: i32) {
        self.sse_rm(0xf2, 0x59, dst, base, disp);
    }
    pub fn subsd_m(&mut self, dst: X, base: R, disp: i32) {
        self.sse_rm(0xf2, 0x5c, dst, base, disp);
    }
    pub fn divsd_m(&mut self, dst: X, base: R, disp: i32) {
        self.sse_rm(0xf2, 0x5e, dst, base, disp);
    }
    pub fn addsd(&mut self, dst: X, src: X) {
        self.sse_rr(0xf2, 0x58, dst, src);
    }
    pub fn subsd(&mut self, dst: X, src: X) {
        self.sse_rr(0xf2, 0x5c, dst, src);
    }
    pub fn mulsd(&mut self, dst: X, src: X) {
        self.sse_rr(0xf2, 0x59, dst, src);
    }
    pub fn divsd(&mut self, dst: X, src: X) {
        self.sse_rr(0xf2, 0x5e, dst, src);
    }
    /// cvttsd2si r64, qword [base + disp]
    pub fn cvttsd2si64_m(&mut self, dst: R, base: R, disp: i32) {
        self.byte(0xf2);
        self.rex(true, dst as u8, 0, base as u8, false);
        self.byte(0x0f);
        self.byte(0x2c);
        self.mem(dst as u8, base, disp);
    }
    /// cvtsi2sd xmm, r64
    pub fn cvtsi2sd64(&mut self, dst: X, src: R) {
        // writes only the low half: zero the register first so the result
        // doesn't depend on its previous value
        self.xorps(dst, dst);
        self.byte(0xf2);
        self.rex(true, dst.0, 0, src as u8, false);
        self.byte(0x0f);
        self.byte(0x2a);
        self.modrm_rr(dst.0, src as u8);
    }
    /// 32-bit ALU op dst, src (0x21 and, 0x09 or, 0x31 xor)
    pub fn alu32_rr(&mut self, opcode: u8, dst: R, src: R) {
        self.rex(false, src as u8, 0, dst as u8, false);
        self.byte(opcode);
        self.modrm_rr(src as u8, dst as u8);
    }
    /// not r32
    pub fn not32(&mut self, r: R) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0xf7);
        self.modrm_rr(2, r as u8);
    }
    /// shift/rotate r32 by cl (ext: 0 rol, 1 ror, 4 shl, 5 shr, 7 sar)
    pub fn shift32_cl(&mut self, ext: u8, r: R) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0xd3);
        self.modrm_rr(ext, r as u8);
    }
    /// shr r32, imm8
    pub fn shr32_ri(&mut self, r: R, imm: u8) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0xc1);
        self.modrm_rr(5, r as u8);
        self.byte(imm);
    }
    /// Loads with extension from [base + disp] into a 64-bit register:
    /// kind 0 movzx8, 1 movsx8, 2 movzx16, 3 movsx16, 4 mov32, 5 movsxd
    pub fn load_ext(&mut self, kind: u8, dst: R, base: R, disp: i32) {
        match kind {
            0 | 1 | 2 | 3 => {
                self.rex(true, dst as u8, 0, base as u8, false);
                self.byte(0x0f);
                self.byte([0xb6, 0xbe, 0xb7, 0xbf][kind as usize]);
            }
            4 => {
                self.rex(false, dst as u8, 0, base as u8, false);
                self.byte(0x8b);
            }
            _ => {
                self.rex(true, dst as u8, 0, base as u8, false);
                self.byte(0x63);
            }
        }
        self.mem(dst as u8, base, disp);
    }
    /// Stores the low `size` bytes (1, 2, 4) of src to [base + disp]
    pub fn store_sized(&mut self, size: u8, base: R, disp: i32, src: R) {
        if size == 2 {
            self.byte(0x66);
        }
        // force REX for byte stores so sil/dil etc. are not misencoded
        self.rex(false, src as u8, 0, base as u8, size == 1);
        self.byte(if size == 1 { 0x88 } else { 0x89 });
        self.mem(src as u8, base, disp);
    }
    /// cvtss2sd xmm, dword [base + disp]
    pub fn cvtss2sd_m(&mut self, dst: X, base: R, disp: i32) {
        self.xorps(dst, dst);
        self.sse_rm(0xf3, 0x5a, dst, base, disp);
    }
    /// cvtsd2ss dst, src
    pub fn cvtsd2ss(&mut self, dst: X, src: X) {
        self.sse_rr(0xf2, 0x5a, dst, src);
    }
    /// movss dword [base + disp], xmm
    pub fn movss_store(&mut self, base: R, disp: i32, src: X) {
        self.sse_rm(0xf3, 0x11, src, base, disp);
    }
    /// add dword [base + disp], imm32
    pub fn add32_mi(&mut self, base: R, disp: i32, imm: i32) {
        self.rex(false, 0, 0, base as u8, false);
        self.byte(0x81);
        self.mem(0, base, disp);
        self.dword(imm as u32);
    }
    /// sub r, qword [base + disp]
    pub fn sub_rm(&mut self, r: R, base: R, disp: i32) {
        self.rex(true, r as u8, 0, base as u8, false);
        self.byte(0x2b);
        self.mem(r as u8, base, disp);
    }
    /// test dword [base + disp], imm32
    pub fn test32_mi(&mut self, base: R, disp: i32, imm: u32) {
        self.rex(false, 0, 0, base as u8, false);
        self.byte(0xf7);
        self.mem(0, base, disp);
        self.dword(imm);
    }
    /// movq xmm, r64
    pub fn movq_xr(&mut self, dst: X, src: R) {
        self.byte(0x66);
        self.rex(true, dst.0, 0, src as u8, false);
        self.byte(0x0f);
        self.byte(0x6e);
        self.modrm_rr(dst.0, src as u8);
    }
    /// movups xmm, [base + disp]
    pub fn movups_load(&mut self, dst: X, base: R, disp: i32) {
        self.rex(false, dst.0, 0, base as u8, false);
        self.byte(0x0f);
        self.byte(0x10);
        self.mem(dst.0, base, disp);
    }
    /// movups [base + disp], xmm
    pub fn movups_store(&mut self, base: R, disp: i32, src: X) {
        self.rex(false, src.0, 0, base as u8, false);
        self.byte(0x0f);
        self.byte(0x11);
        self.mem(src.0, base, disp);
    }
    /// packed single op dst, src (0x58 add, 0x5c sub, 0x59 mul, 0x5e div)
    /// AVX packed single op `dst = a op b` by opcode (vaddps/vsubps/vmulps/vdivps)
    pub fn vps_rr(&mut self, op: u8, dst: X, a: X, b: X) {
        self.vex(0, dst.0, a.0, b.0);
        self.byte(op);
        self.modrm_rr(dst.0, b.0);
    }
    /// pinsrd xmm, r32, lane (SSE4.1)
    pub fn pinsrd(&mut self, dst: X, src: R, lane: u8) {
        self.byte(0x66);
        self.rex(false, dst.0, 0, src as u8, false);
        self.byte(0x0f);
        self.byte(0x3a);
        self.byte(0x22);
        self.modrm_rr(dst.0, src as u8);
        self.byte(lane);
    }
    pub fn ps_rr(&mut self, op: u8, dst: X, src: X) {
        self.sse_rr(0, op, dst, src);
    }
    /// cvtsd2ss xmm, qword [base + disp]
    pub fn cvtsd2ss_m(&mut self, dst: X, base: R, disp: i32) {
        self.sse_rm(0xf2, 0x5a, dst, base, disp);
    }
    /// shufps dst, src, imm
    pub fn shufps(&mut self, dst: X, src: X, imm: u8) {
        self.sse_rr(0, 0xc6, dst, src);
        self.byte(imm);
    }
    /// movq xmm, qword [base + disp] (zeroes the upper half)
    pub fn movq_load(&mut self, dst: X, base: R, disp: i32) {
        self.sse_rm(0xf3, 0x7e, dst, base, disp);
    }
    /// movq qword [base + disp], xmm
    pub fn movq_store(&mut self, base: R, disp: i32, src: X) {
        self.sse_rm(0x66, 0xd6, src, base, disp);
    }
    /// movss xmm, dword [base + disp] (zeroes the other lanes)
    pub fn movss_load(&mut self, dst: X, base: R, disp: i32) {
        self.sse_rm(0xf3, 0x10, dst, base, disp);
    }
    /// movlhps dst, src (dst.high64 = src.low64)
    pub fn movlhps(&mut self, dst: X, src: X) {
        self.sse_rr(0, 0x16, dst, src);
    }
    /// movhlps dst, src (dst.low64 = src.high64)
    pub fn movhlps(&mut self, dst: X, src: X) {
        self.sse_rr(0, 0x12, dst, src);
    }
    /// movd r32, xmm
    pub fn movd_rx(&mut self, dst: R, src: X) {
        self.byte(0x66);
        self.rex(false, src.0, 0, dst as u8, false);
        self.byte(0x0f);
        self.byte(0x7e);
        self.modrm_rr(src.0, dst as u8);
    }
    /// or dst, src (64-bit)
    pub fn or_rr(&mut self, dst: R, src: R) {
        self.rex(true, src as u8, 0, dst as u8, false);
        self.byte(0x09);
        self.modrm_rr(src as u8, dst as u8);
    }
    /// 64-bit ALU op (0x01 add, 0x29 sub, 0x21 and, 0x09 or, 0x31 xor): dst op= src
    pub fn alu64_rr(&mut self, opcode: u8, dst: R, src: R) {
        self.rex(true, src as u8, 0, dst as u8, false);
        self.byte(opcode);
        self.modrm_rr(src as u8, dst as u8);
    }
    /// 64-bit ALU op r, imm32 sign-extended (ext: 0 add, 1 or, 4 and, 5 sub,
    /// 6 xor, 7 cmp)
    pub fn alu64_ri(&mut self, ext: u8, r: R, imm: i32) {
        self.rex(true, 0, 0, r as u8, false);
        self.byte(0x81);
        self.modrm_rr(ext, r as u8);
        self.dword(imm as u32);
    }
    /// imul dst, src, imm32 (64-bit)
    pub fn imul64_rri(&mut self, dst: R, src: R, imm: i32) {
        self.rex(true, dst as u8, 0, src as u8, false);
        self.byte(0x69);
        self.modrm_rr(dst as u8, src as u8);
        self.dword(imm as u32);
    }
    /// imul dst, src (64-bit)
    pub fn imul64_rr(&mut self, dst: R, src: R) {
        self.rex(true, dst as u8, 0, src as u8, false);
        self.byte(0x0f);
        self.byte(0xaf);
        self.modrm_rr(dst as u8, src as u8);
    }
    /// not r64 (ext 2) / neg r64 (ext 3)
    pub fn unary64(&mut self, ext: u8, r: R) {
        self.rex(true, 0, 0, r as u8, false);
        self.byte(0xf7);
        self.modrm_rr(ext, r as u8);
    }
    /// bswap r64
    pub fn bswap64(&mut self, r: R) {
        self.rex(true, 0, 0, r as u8, false);
        self.byte(0x0f);
        self.byte(0xc8 + (r as u8 & 7));
    }
    /// shift/rotate r64 by cl (ext: 0 rol, 1 ror, 4 shl, 5 shr, 7 sar)
    pub fn shift64_cl(&mut self, ext: u8, r: R) {
        self.rex(true, 0, 0, r as u8, false);
        self.byte(0xd3);
        self.modrm_rr(ext, r as u8);
    }
    /// unpcklps dst, src (dst = dst0, src0, dst1, src1)
    pub fn unpcklps(&mut self, dst: X, src: X) {
        self.sse_rr(0, 0x14, dst, src);
    }
    /// sqrtsd dst, src
    pub fn sqrtsd(&mut self, dst: X, src: X) {
        if dst != src {
            self.xorps(dst, dst);
        }
        self.sse_rr(0xf2, 0x51, dst, src);
    }
    /// roundsd dst, src, mode (SSE4.1; 9 = floor, 10 = ceil)
    pub fn roundsd(&mut self, dst: X, src: X, mode: u8) {
        self.byte(0x66);
        self.rex(false, dst.0, 0, src.0, false);
        self.byte(0x0f);
        self.byte(0x3a);
        self.byte(0x0b);
        self.modrm_rr(dst.0, src.0);
        self.byte(mode);
    }
    /// cvttsd2si r32, qword [base + disp]
    pub fn cvttsd2si_m(&mut self, dst: R, base: R, disp: i32) {
        self.byte(0xf2);
        self.rex(false, dst as u8, 0, base as u8, false);
        self.byte(0x0f);
        self.byte(0x2c);
        self.mem(dst as u8, base, disp);
    }
    /// cvtsi2sd xmm, r32
    pub fn cvtsi2sd(&mut self, dst: X, src: R) {
        // writes only the low half: zero the register first so the result
        // doesn't depend on its previous value
        self.xorps(dst, dst);
        self.byte(0xf2);
        self.rex(false, dst.0, 0, src as u8, false);
        self.byte(0x0f);
        self.byte(0x2a);
        self.modrm_rr(dst.0, src as u8);
    }
    /// ucomisd xmm, qword [base + disp]
    pub fn ucomisd_m(&mut self, a: X, base: R, disp: i32) {
        self.sse_rm(0x66, 0x2e, a, base, disp);
    }
    pub fn ucomisd(&mut self, a: X, b: X) {
        self.sse_rr(0x66, 0x2e, a, b);
    }
    pub fn xorpd(&mut self, dst: X, src: X) {
        self.sse_rr(0x66, 0x57, dst, src);
    }
    pub fn movapd(&mut self, dst: X, src: X) {
        self.sse_rr(0x66, 0x28, dst, src);
    }

    // -- additions for the register-tracking translator ---------------------

    /// mov dword [base + disp], src32
    pub fn store32(&mut self, base: R, disp: i32, src: R) {
        self.rex(false, src as u8, 0, base as u8, false);
        self.byte(0x89);
        self.mem(src as u8, base, disp);
    }
    /// 32-bit ALU op r32, imm32 (ext: 0 add, 1 or, 4 and, 5 sub, 6 xor, 7 cmp)
    pub fn alu32_ri(&mut self, ext: u8, r: R, imm: i32) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0x81);
        self.modrm_rr(ext, r as u8);
        self.dword(imm as u32);
    }
    /// shift/rotate r32 by imm8 (ext: 0 rol, 1 ror, 4 shl, 5 shr, 7 sar)
    pub fn shift32_ri(&mut self, ext: u8, r: R, imm: u8) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0xc1);
        self.modrm_rr(ext, r as u8);
        self.byte(imm);
    }
    /// shift/rotate r64 by imm8
    pub fn shift64_ri(&mut self, ext: u8, r: R, imm: u8) {
        self.rex(true, 0, 0, r as u8, false);
        self.byte(0xc1);
        self.modrm_rr(ext, r as u8);
        self.byte(imm);
    }
    /// bswap r32
    pub fn bswap32(&mut self, r: R) {
        self.rex(false, 0, 0, r as u8, false);
        self.byte(0x0f);
        self.byte(0xc8 + (r as u8 & 7));
    }
    /// test r32, r32
    pub fn test32_rr(&mut self, a: R, b: R) {
        self.rex(false, b as u8, 0, a as u8, false);
        self.byte(0x85);
        self.modrm_rr(b as u8, a as u8);
    }
    /// cmp a32, b32
    pub fn cmp32_rr(&mut self, a: R, b: R) {
        self.rex(false, b as u8, 0, a as u8, false);
        self.byte(0x39);
        self.modrm_rr(b as u8, a as u8);
    }
    /// lea dst, [base + disp]
    pub fn lea(&mut self, dst: R, base: R, disp: i32) {
        self.rex(true, dst as u8, 0, base as u8, false);
        self.byte(0x8d);
        self.mem(dst as u8, base, disp);
    }
    /// Pads with NOPs to a multiple of `n` bytes (code is placed 32-byte
    /// aligned).
    pub fn align(&mut self, n: u32) {
        const NOPS: [&[u8]; 9] = [
            &[],
            &[0x90],
            &[0x66, 0x90],
            &[0x0f, 0x1f, 0x00],
            &[0x0f, 0x1f, 0x40, 0x00],
            &[0x0f, 0x1f, 0x44, 0x00, 0x00],
            &[0x66, 0x0f, 0x1f, 0x44, 0x00, 0x00],
            &[0x0f, 0x1f, 0x80, 0x00, 0x00, 0x00, 0x00],
            &[0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
        ];
        let mut pad = (n - self.pos() % n) % n;
        while pad > 0 {
            let k = pad.min(8);
            for &b in NOPS[k as usize] {
                self.byte(b);
            }
            pad -= k;
        }
    }
    /// cvttsd2si r64, xmm
    pub fn cvttsd2si64_x(&mut self, dst: R, src: X) {
        self.byte(0xf2);
        self.rex(true, dst as u8, 0, src.0, false);
        self.byte(0x0f);
        self.byte(0x2c);
        self.modrm_rr(dst as u8, src.0);
    }
    /// cvttsd2si r32, xmm
    pub fn cvttsd2si_x(&mut self, dst: R, src: X) {
        self.byte(0xf2);
        self.rex(false, dst as u8, 0, src.0, false);
        self.byte(0x0f);
        self.byte(0x2c);
        self.modrm_rr(dst as u8, src.0);
    }
    /// cvtss2sd dst, src
    pub fn cvtss2sd(&mut self, dst: X, src: X) {
        // writes only the low half: break the dependency on dst's old value
        if dst != src {
            self.xorps(dst, dst);
        }
        self.sse_rr(0xf3, 0x5a, dst, src);
    }
    /// movaps dst, src
    pub fn movaps(&mut self, dst: X, src: X) {
        self.sse_rr(0, 0x28, dst, src);
    }
    /// xorps dst, src
    pub fn xorps(&mut self, dst: X, src: X) {
        self.sse_rr(0, 0x57, dst, src);
    }
    /// packed single 0F-map op dst, src (0x54 and, 0x55 andn, 0x56 or, 0x5d min, 0x5f max, 0x51 sqrt)
    pub fn ps_op(&mut self, op: u8, dst: X, src: X) {
        self.sse_rr(0, op, dst, src);
    }
    /// andpd dst, src
    pub fn andpd(&mut self, dst: X, src: X) {
        self.sse_rr(0x66, 0x54, dst, src);
    }
    /// orpd dst, src
    pub fn orpd(&mut self, dst: X, src: X) {
        self.sse_rr(0x66, 0x56, dst, src);
    }
    /// movq r64, xmm
    pub fn movq_rx(&mut self, dst: R, src: X) {
        self.byte(0x66);
        self.rex(true, src.0, 0, dst as u8, false);
        self.byte(0x0f);
        self.byte(0x7e);
        self.modrm_rr(src.0, dst as u8);
    }
    /// movd xmm, r32
    pub fn movd_xr(&mut self, dst: X, src: R) {
        self.byte(0x66);
        self.rex(false, dst.0, 0, src as u8, false);
        self.byte(0x0f);
        self.byte(0x6e);
        self.modrm_rr(dst.0, src as u8);
    }
    /// scalar single op dst, src (0x58 add, 0x5c sub, 0x59 mul, 0x5e div, 0x51 sqrt)
    pub fn ss_rr(&mut self, op: u8, dst: X, src: X) {
        self.sse_rr(0xf3, op, dst, src);
    }
    /// roundps dst, src, mode (SSE4.1)
    pub fn roundps(&mut self, dst: X, src: X, mode: u8) {
        self.byte(0x66);
        self.rex(false, dst.0, 0, src.0, false);
        self.byte(0x0f);
        self.byte(0x3a);
        self.byte(0x08);
        self.modrm_rr(dst.0, src.0);
        self.byte(mode);
    }
    /// dpps dst, src, imm (SSE4.1)
    pub fn dpps(&mut self, dst: X, src: X, imm: u8) {
        self.byte(0x66);
        self.rex(false, dst.0, 0, src.0, false);
        self.byte(0x0f);
        self.byte(0x3a);
        self.byte(0x40);
        self.modrm_rr(dst.0, src.0);
        self.byte(imm);
    }
    /// scalar double op xmm, xmm by opcode (0x58 add, 0x5c sub, 0x59 mul, 0x5e div)
    /// VEX prefix (map 0F, W0, L0) for `reg`, second source `v`, rm/base `b`.
    fn vex(&mut self, pp: u8, reg: u8, v: u8, b: u8) {
        let r = ((reg >> 3) & 1) ^ 1;
        let bb = ((b >> 3) & 1) ^ 1;
        let vv = (!v & 0xf) << 3;
        if bb == 1 {
            self.byte(0xc5);
            self.byte((r << 7) | vv | pp);
        } else {
            self.byte(0xc4);
            self.byte((r << 7) | (1 << 6) | (bb << 5) | 1);
            self.byte(vv | pp);
        }
    }
    /// AVX scalar double op `dst = a op b` by opcode (vaddsd/vsubsd/vmulsd/vdivsd)
    pub fn vsd_rr(&mut self, op: u8, dst: X, a: X, b: X) {
        self.vex(3, dst.0, a.0, b.0);
        self.byte(op);
        self.modrm_rr(dst.0, b.0);
    }
    /// AVX scalar double op `dst = a op qword [base + disp]`
    pub fn vsd_rm(&mut self, op: u8, dst: X, a: X, base: R, disp: i32) {
        self.vex(3, dst.0, a.0, base as u8);
        self.byte(op);
        self.mem(dst.0, base, disp);
    }
    pub fn sd_rr(&mut self, op: u8, dst: X, src: X) {
        self.sse_rr(0xf2, op, dst, src);
    }
    /// scalar double op xmm, qword [base + disp] by opcode
    pub fn sd_rm(&mut self, op: u8, dst: X, base: R, disp: i32) {
        self.sse_rm(0xf2, op, dst, base, disp);
    }
    /// bsr / bsf r32, r32 (op 0xbd / 0xbc)
    pub fn bitscan32(&mut self, op: u8, dst: R, src: R) {
        self.rex(false, dst as u8, 0, src as u8, false);
        self.byte(0x0f);
        self.byte(op);
        self.modrm_rr(dst as u8, src as u8);
    }
    /// mov dst, imm64 with the full 10-byte encoding (patchable)
    pub fn mov_ri64(&mut self, dst: R, imm: u64) {
        self.rex(true, 0, 0, dst as u8, false);
        self.byte(0xb8 + (dst as u8 & 7));
        self.qword(imm);
    }
    /// call a host function at an absolute address (clobbers rax)
    pub fn call_abs(&mut self, addr: usize) {
        self.mov_ri(R::Rax, addr as u64);
        self.call_r(R::Rax);
    }
    /// movsd xmm, xmm is not used (movapd copies); cvtsd2ss xmm, xmm is above
    /// mov r64, qword [base + index*8]
    pub fn load_index8(&mut self, dst: R, base: R, index: R) {
        assert!(base as u8 & 7 != 5, "rbp/r13 base needs a displacement");
        self.rex(true, dst as u8, index as u8, base as u8, false);
        self.byte(0x8b);
        self.byte(((dst as u8 & 7) << 3) | 4);
        self.byte(0xc0 | ((index as u8 & 7) << 3) | (base as u8 & 7));
    }
    /// cmovcc r64, r64
    pub fn cmov64(&mut self, c: Cond, dst: R, src: R) {
        self.rex(true, dst as u8, 0, src as u8, false);
        self.byte(0x0f);
        self.byte(0x40 | c as u8);
        self.modrm_rr(dst as u8, src as u8);
    }
    /// cqo (sign-extends rax into rdx:rax)
    pub fn cqo(&mut self) {
        self.byte(0x48);
        self.byte(0x99);
    }
    /// idiv r64 (ext 7) / div r64 (ext 6): rdx:rax by r
    pub fn div64(&mut self, ext: u8, r: R) {
        self.rex(true, 0, 0, r as u8, false);
        self.byte(0xf7);
        self.modrm_rr(ext, r as u8);
    }
}
