//! Bytecode -> x64 translator.
//!
//! One pass over the bytecode. Between join points (jump targets, call
//! returns) the translator tracks what it knows about each VM register in
//! `St` (see `state.rs`): type tags proven by earlier guards, and values held
//! in machine registers instead of the VM stack. Each instruction is
//! translated against that knowledge, so repeated type checks, tag stores and
//! stack round trips disappear inside a block.
//!
//! Every instruction with a guard has an out-of-line slow path that first
//! writes the stale values back to the stack and then runs the interpreter's
//! own single-instruction step. Two kinds exist:
//! * rejoin: the step's result has the same shape as the fast path's, so
//!   the stub reloads the machine registers and continues after the
//!   instruction (re-checking facts the fast path had proven by guards);
//! * bail: the fast path produces a typed result (arithmetic, builtins), so
//!   the stub leaves native code for the rest of the call and records type
//!   feedback; the function is then recompiled for the types seen.

use super::asm::{Asm, Cond, Label, R, X};
use super::state::*;
use super::x64::*;
use crate::lobject::*;
use crate::lvmexecute::{handlers_table, native_call, native_return, STEPS};
use luau_common::bytecode::{get_op_length, BuiltinFunction as B, Opcode as O};

/// Feedback entry kinds (stored in the tag fields).
pub const FB_GENERIC: u8 = 0xfe;
pub const FB_NOINLINE: u8 = 0xfd;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Opnd {
    Reg(usize),
    K(usize),
}

struct Site {
    label: Label,
    i: usize,
    bail: bool,
    snap: Vec<Sv>,
    reload: Vec<Rl>,
    verify: Vec<(u8, u8)>,
    safeenv: bool,
    cont: Option<Label>,
    next_i: usize,
    /// a bail of the carried-register check of the loop at this head
    /// (recompile without carrying there)
    carry_fail: Option<usize>,
    /// tracing JIT hot counter of the loop instruction at `i` (0: none):
    /// the stub calls `jit::native_loop_hook` instead of the step
    hook: usize,
}

struct Edge {
    label: Label,
    snap: Vec<Sv>,
    target: Label,
}

struct Tr<'f> {
    a: Asm,
    st: St,
    /// pass 2: loop head -> label the back edge jumps to (registers carried)
    carry_inner: std::collections::HashMap<usize, Label>,
    p: *mut Proto,
    code: *const u32,
    n: usize,
    labels: Vec<Label>,
    starts: Vec<bool>,
    /// join points: reached with everything on the stack and nothing known
    entry: Vec<bool>,
    /// join points other than call returns
    jentry: Vec<bool>,
    /// resume points after the fallback CALL of an inlined builtin: entered
    /// from the interpreter only, which is sent straight back
    resume_exit: Vec<Option<Label>>,
    sites: Vec<Site>,
    site_start: usize,
    slow_cache: Option<(u32, bool, Label)>,
    /// The builtin just expanded set `L->top` itself (variable result count).
    dyn_top: bool,
    /// The builtin being expanded has no live result (no values wanted and
    /// the result slot is dead): pure builtins only need their guards.
    result_dead: bool,
    edges: Vec<Edge>,
    learned: Vec<(u8, u8)>,
    volatile: Vec<u8>,
    is_volatile: [bool; 256],
    pins: Vec<(usize, usize, usize)>,
    stepk: Vec<Option<f64>>,
    cx: Ctx,
    i: usize,
    /// live-in registers per instruction
    live: Vec<super::live::Mask>,
    sse41: bool,
    /// 3-operand VEX encodings
    avx: bool,
    generic: bool,
    fb: &'f [(u32, u8, u8)],
}

fn linear(op: u8) -> bool {
    const L: &[O] = &[
        O::Nop, O::LoadNil, O::LoadN, O::LoadK, O::Move, O::GetGlobal, O::SetGlobal, O::GetUpval, O::SetUpval, O::CloseUpvals,
        O::GetImport, O::GetTable, O::SetTable, O::GetTableKs, O::SetTableKs, O::GetTableN, O::SetTableN, O::Namecall,
        O::Add, O::Sub, O::Mul, O::Div, O::Mod, O::Pow, O::AddK, O::SubK, O::MulK, O::DivK, O::ModK, O::PowK, O::And, O::Or, O::AndK,
        O::OrK, O::Concat, O::Not, O::Minus, O::Length, O::NewTable, O::DupTable, O::SetList, O::GetVarargs,
        O::PrepVarargs, O::LoadKx, O::Capture, O::SubRk, O::DivRk, O::IDiv, O::IDivK, O::Coverage,
    ];
    L.iter().any(|&o| o as u8 == op)
}

impl<'f> Tr<'f> {
    fn pc(&self, i: usize) -> usize {
        self.code as usize + i * 4
    }
    unsafe fn k_tag(&self, idx: usize) -> u8 {
        (*(*self.p).k.add(idx)).tt as u8
    }
    unsafe fn k_num(&self, idx: usize) -> Option<f64> {
        let k = (*self.p).k.add(idx);
        if (*k).tt == LUA_TNUMBER {
            Some((*k).value.n)
        } else {
            None
        }
    }
    fn can_be(&self, r: usize, t: u8) -> bool {
        self.st.tag[r] == T_ANY || self.st.tag[r] == t
    }
    fn feedback(&self) -> Option<(u8, u8)> {
        self.fb.iter().find(|e| e.0 == self.i as u32).map(|e| (e.1, e.2))
    }

    /// The slow path for the current instruction and cache state.
    fn slow(&mut self, bail: bool) -> Label {
        if let Some((v, b, l)) = self.slow_cache {
            if v == self.st.ver && b == bail {
                return l;
            }
        }
        let l = self.a.label();
        self.sites.push(Site {
            label: l,
            i: self.i,
            bail,
            snap: self.st.snapshot_live(&self.live[self.i]),
            reload: Vec::new(),
            verify: Vec::new(),
            safeenv: false,
            cont: None,
            next_i: 0,
            carry_fail: None,
            hook: 0,
        });
        self.slow_cache = Some((self.st.ver, bail, l));
        l
    }

    /// With the tracing JIT on: a counter decremented at this loop
    /// instruction; at zero the loop's state goes to the stack and
    /// `jit::native_loop_hook` runs the instruction (and records or runs a
    /// trace). Emitted right after the loop checks (rax/rcx are free).
    fn trace_counter(&mut self) {
        if !crate::jit::state::trace_enabled() {
            return;
        }
        let ctr = crate::jit::new_loop_counter();
        let l = self.a.label();
        self.sites.push(Site {
            label: l,
            i: self.i,
            bail: false,
            snap: self.st.snapshot_live(&self.live[self.i]),
            reload: Vec::new(),
            verify: Vec::new(),
            safeenv: false,
            cont: None,
            next_i: 0,
            carry_fail: None,
            hook: ctr as usize,
        });
        self.a.mov_ri(R::Rax, ctr as u64);
        self.a.add32_mi(R::Rax, 0, -1);
        self.a.jcc(Cond::E, l);
    }

    /// Makes sure `r` has tag `t` (the caller checked `can_be`).
    fn guard(&mut self, r: usize, t: u8, bail: bool) {
        if self.st.tag[r] == t {
            return;
        }
        debug_assert_eq!(self.st.tag[r], T_ANY);
        let s = self.slow(bail);
        self.a.cmp32_mi(R::R13, reg(r as i32) + TT, t as i32);
        self.a.jcc(Cond::NE, s);
        self.st.tag[r] = t;
        self.st.memtag[r] = true;
        self.learned.push((r as u8, t));
    }

    /// Entry check of a carried register of the loop at `head`: a different
    /// type bails and recompiles without carrying for that loop.
    fn carry_guard(&mut self, r: usize, t: u8, head: usize) {
        let l = self.a.label();
        self.sites.push(Site {
            label: l,
            i: self.i,
            bail: true,
            snap: self.st.snapshot_live(&self.live[self.i]),
            reload: Vec::new(),
            verify: Vec::new(),
            safeenv: false,
            cont: None,
            next_i: 0,
            carry_fail: Some(head),
            hook: 0,
        });
        self.a.cmp32_mi(R::R13, reg(r as i32) + TT, t as i32);
        self.a.jcc(Cond::NE, l);
        self.st.tag[r] = t;
        self.st.memtag[r] = true;
    }

    /// Everything still needed goes to the stack; no machine register is
    /// kept (before calls into the runtime).
    fn sync(&mut self) {
        self.st.flush_live(&mut self.a, &self.live[self.i]);
        self.st.drop_all();
    }

    /// Stores what is live at the current instruction (templates that work
    /// on the stack slots and jump directly).
    fn flush_here(&mut self) {
        self.st.flush_live(&mut self.a, &self.live[self.i]);
    }

    /// Conditional jump to instruction `target` (a join point).
    fn jcc_to(&mut self, c: Cond, target: usize) {
        let tl = self.labels[target];
        let snap = self.st.snapshot_live(&self.live[target]);
        if !snap.is_empty() {
            let l = self.a.label();
            self.edges.push(Edge { label: l, snap, target: tl });
            self.a.jcc(c, l);
        } else {
            self.a.jcc(c, tl);
        }
    }

    fn jmp_to(&mut self, target: usize) {
        self.st.flush_live(&mut self.a, &self.live[target]);
        let tl = self.labels[target];
        self.a.jmp(tl);
    }

    /// R(ra) = TValue at [base + off] (unknown type).
    fn set_from_mem(&mut self, ra: usize, base: R, off: i32) {
        self.st.kill(ra);
        copy_tv(&mut self.a, R::R13, reg(ra as i32), base, off);
    }

    /// The type instruction `j` needs register `r` to have for its fast
    /// path (number, integer or vector), if it has one.
    unsafe fn use_type(&self, j: usize, r: usize) -> Option<u8> {
        let insn = *self.code.add(j);
        let op = op_of(insn);
        let (a, b, c) = (a_of(insn) as usize, b_of(insn) as usize, c_of(insn) as usize);
        let is = |o: O| op == o as u8;
        let fb = self.fb.iter().find(|e| e.0 == j as u32).map(|e| (e.1, e.2));
        let typed = |t: u8| if t == TNUM || t == TVEC || t == TINT { Some(t) } else { None };
        if is(O::Add) || is(O::Sub) || is(O::Mul) || is(O::Div) || is(O::Mod) || is(O::IDiv) || is(O::Pow) {
            return match fb {
                None => Some(TNUM),
                Some((tb, tc)) => typed(if b == r { tb } else { tc }),
            };
        }
        if is(O::AddK) || is(O::SubK) || is(O::MulK) || is(O::DivK) || is(O::ModK) || is(O::IDivK) || is(O::PowK) || is(O::Minus) {
            return match fb {
                None => Some(TNUM),
                Some((tb, _)) => typed(tb),
            };
        }
        if is(O::SubRk) || is(O::DivRk) {
            return match fb {
                None => Some(TNUM),
                Some((_, tc)) => typed(tc),
            };
        }
        if is(O::JumpIfLt) || is(O::JumpIfLe) || is(O::JumpIfNotLt) || is(O::JumpIfNotLe) || is(O::ForNPrep) {
            return Some(TNUM);
        }
        if (is(O::GetTable) && c == r && b != r) || (is(O::SetTable) && c == r && a != r && b != r) {
            // a GETTABLE that saw another key type uses the hash path
            return if is(O::GetTable) && fb.is_some() { None } else { Some(TNUM) };
        }
        if is(O::FastCall) || is(O::FastCall1) || is(O::FastCall2) || is(O::FastCall2K) || is(O::FastCall3) {
            if fb.is_some() {
                return None;
            }
            let bfid = a as u32;
            let isb = |x: B| bfid == x as u32;
            if isb(B::BufferReadInteger) {
                return if b == r { None } else { Some(TNUM) };
            }
            if isb(B::BufferWriteInteger) {
                // (buffer, offset, value)
                let aux = *self.code.add(j + 1);
                return if b == r {
                    None
                } else if ((aux >> 8) & 0xff) as usize == r {
                    Some(TINT)
                } else {
                    Some(TNUM)
                };
            }
            if isb(B::IntegerCreate) {
                return Some(TNUM);
            }
            const INTS: &[B] = &[
                B::IntegerToNumber, B::IntegerLt, B::IntegerLe, B::IntegerGt, B::IntegerGe, B::IntegerUlt, B::IntegerUle, B::IntegerUgt,
                B::IntegerUge, B::IntegerMin, B::IntegerMax, B::IntegerDiv, B::IntegerRem, B::IntegerUDiv, B::IntegerURem,
            ];
            if int_builtin(bfid).is_some() || INTS.iter().any(|&x| isb(x)) {
                return Some(TINT);
            }
            if let Some(_) = buffer_op(bfid) {
                return if b == r { None } else { Some(TNUM) };
            }
            if isb(B::VectorMagnitude) || isb(B::VectorNormalize) || isb(B::VectorAbs) || isb(B::VectorFloor) || isb(B::VectorCeil)
                || isb(B::VectorDot) || isb(B::VectorCross) || isb(B::VectorMin) || isb(B::VectorMax)
            {
                return Some(TVEC);
            }
            const NUMS: &[B] = &[
                B::Vector, B::Bit32Band, B::Bit32Bor, B::Bit32Bxor, B::Bit32Bnot, B::Bit32Byteswap, B::Bit32Countlz, B::Bit32Countrz,
                B::Bit32Lshift, B::Bit32Rshift, B::Bit32Arshift, B::Bit32Lrotate, B::Bit32Rrotate, B::Bit32ExtractK, B::MathSqrt,
                B::MathAbs, B::MathFloor, B::MathCeil, B::MathRound, B::MathSin, B::MathCos, B::MathTan, B::MathAsin, B::MathAcos,
                B::MathAtan, B::MathExp, B::MathLog, B::MathLog10, B::MathSinh, B::MathCosh, B::MathTanh, B::MathMin, B::MathMax,
                B::MathPow, B::MathAtan2, B::MathFmod,
            ];
            if NUMS.iter().any(|&x| isb(x)) {
                return Some(TNUM);
            }
        }
        None
    }

    /// When the value about to be loaded into `r` is next read (in this
    /// block) by an instruction whose fast path needs a particular type,
    /// returns that type: the load can then check the type at the source and
    /// keep the value in a machine register.
    unsafe fn spec(&self, r: usize, from: usize) -> Option<u8> {
        if self.generic || dbg_off(1) || self.is_volatile[r] || self.feedback().is_some_and(|f| f.1 == FB_NOINLINE || f.0 == FB_GENERIC) {
            return None;
        }
        let mut j = from;
        for _ in 0..32 {
            if j >= self.n || self.jentry[j] {
                return None;
            }
            let inf = super::live::info(self.code, self.n, j)?;
            if super::live::has(&inf.uses, r) {
                return self.use_type(j, r);
            }
            if super::live::has(&inf.defs, r) {
                return None;
            }
            let op = op_of(*self.code.add(j));
            let fast = [O::FastCall, O::FastCall1, O::FastCall2, O::FastCall2K, O::FastCall3].iter().any(|&o| o as u8 == op);
            if fast {
                if super::live::has(&inf.defs1, r) {
                    return None;
                }
                j = inf.succ[1];
            } else if inf.succ[1] == usize::MAX && inf.succ[0] != usize::MAX && op != O::Call as u8 && op != O::CallFb as u8 {
                j = inf.succ[0];
            } else {
                return None;
            }
        }
        None
    }

    /// R(ra) = TValue at [base + off]; `next` is the instruction that runs
    /// afterwards. Loads the value typed, straight into a machine register,
    /// when its next use wants that (bailing out if the type differs).
    unsafe fn load_tv(&mut self, ra: usize, base: R, off: i32, next: usize) {
        let t = self.spec(ra, next);
        let Some(t) = t else {
            self.set_from_mem(ra, base, off);
            return;
        };
        let s = self.slow(true);
        self.a.cmp32_mi(base, off + TT, t as i32);
        self.a.jcc(Cond::NE, s);
        match t {
            TNUM => {
                let xd = self.st.def_num(&mut self.a, ra);
                self.a.movsd_load(xd, base, off);
            }
            TINT => {
                let gd = self.st.def_int(&mut self.a, ra);
                self.a.load(gd, base, off);
            }
            _ => {
                let xd = self.st.def_vec(&mut self.a, ra);
                load_vec(&mut self.a, xd, X(2), base, off);
            }
        }
        // slow paths of this instruction that rejoin must find this type
        self.learned.push((ra as u8, t));
    }

    /// TValue at [base + off] = R(r).
    fn copy_to_mem(&mut self, base: R, off: i32, r: usize) {
        let st = &self.st;
        match st.tag[r] {
            TNUM if st.x[r] != 0 || (st.g[r] != 0 && st.gexact[r]) => {
                if st.x[r] != 0 {
                    self.a.movsd_store(base, off, X(st.x[r]));
                } else {
                    self.a.cvtsi2sd64(X(2), GP[st.g[r] as usize - 1]);
                    self.a.movsd_store(base, off, X(2));
                }
                self.a.mov_ri(R::R9, NUM_TAG);
                self.a.store(base, off + TAGWORD, R::R9);
            }
            TINT if st.g[r] != 0 => {
                self.a.store(base, off, GP[st.g[r] as usize - 1]);
                self.a.mov_ri(R::R9, INT_TAG);
                self.a.store(base, off + TAGWORD, R::R9);
            }
            TVEC if st.x[r] != 0 => store_vec_to(&mut self.a, base, off, X(st.x[r])),
            _ => {
                debug_assert!(!st.dirty[r]);
                copy_tv(&mut self.a, base, off, R::R13, reg(r as i32));
            }
        }
    }

    /// Proves the environment is safe (bails otherwise).
    fn ensure_safeenv(&mut self, bail: bool) {
        if !self.st.safeenv {
            let s = self.slow(bail);
            self.a.load(R::Rax, R::R15, OFF_CL_ENV);
            self.a.cmp8_mi(R::Rax, OFF_T_SAFEENV, 0);
            self.a.jcc(Cond::E, s);
            self.st.safeenv = true;
        }
    }

    /// rcx = node of table `t` at the slot cached in the instruction (byte at
    /// `cslot`) holding string key `kgc` with a non-nil value.
    fn node_at(&mut self, t: R, cslot: usize, kgc: usize, s: Label) {
        let a = &mut self.a;
        a.mov_ri(R::Rdx, cslot as u64);
        a.movzx8(R::Rcx, R::Rdx, 0);
        a.movzx8(R::Rdx, t, OFF_T_NODEMASK8);
        a.alu32_rr(0x21, R::Rcx, R::Rdx);
        a.shl_ri(R::Rcx, NODE_SHIFT);
        a.load(R::Rdx, t, OFF_T_NODE);
        a.add_rr(R::Rcx, R::Rdx);
        a.load32(R::Rdx, R::Rcx, NODE_KEYTT);
        a.and32_ri(R::Rdx, 0xf);
        a.cmp32_ri(R::Rdx, LUA_TSTRING);
        a.jcc(Cond::NE, s);
        a.mov_ri(R::Rdx, kgc as u64);
        a.cmp_rm(R::Rdx, R::Rcx, NODE_KEY);
        a.jcc(Cond::NE, s);
        a.cmp32_mi(R::Rcx, TT, LUA_TNIL);
        a.jcc(Cond::E, s);
    }

    /// Branches to the slow path if storing R(ra) into the table/upvalue
    /// object `obj` needs a GC barrier.
    fn barrier(&mut self, obj: R, marked_off: i32, ra: usize) {
        let t = self.st.tag[ra];
        if t != T_ANY && t < TSTR {
            return;
        }
        let s = self.slow(false);
        let ok = self.a.label();
        if t == T_ANY {
            self.a.cmp32_mi(R::R13, reg(ra as i32) + TT, LUA_TSTRING);
            self.a.jcc(Cond::L, ok);
        }
        self.a.test8_mi(obj, marked_off, BLACK);
        self.a.jcc(Cond::NE, s);
        self.a.bind(ok);
    }

    // -- generic step ----------------------------------------------------------

    /// Runs instruction `i` through the interpreter's step function.
    unsafe fn step(&mut self, len: usize) {
        let i = self.i;
        let insn = *self.code.add(i);
        let op = op_of(insn);
        let is_frame_op = op == O::Call as u8 || op == O::CallFb as u8 || op == O::FastPCall as u8 || op == O::Return as u8 || op == O::Break as u8;
        self.sync();
        let step = match STEPS[op as usize] {
            Some(s) if !is_frame_op => s as usize,
            _ => {
                let pc = self.pc(i);
                emit_exit(&mut self.a, pc, op);
                self.st.forget();
                return;
            }
        };
        let pc = self.pc(i);
        emit_args(&mut self.a, pc);
        self.a.call_abs(step);
        self.a.test_rr(R::Rax, R::Rax);
        self.a.jcc(Cond::E, self.cx.ret);
        self.a.load(R::R13, R::R12, OFF_BASE);
        if !linear(op) || i + len >= self.n {
            self.a.mov_ri(R::Rcx, self.pc(i + len) as u64);
            self.a.cmp_rr(R::Rax, R::Rcx);
            self.a.jcc(Cond::NE, self.cx.dispatch);
        }
        self.st.safeenv = false;
        let ra = a_of(insn) as usize;
        let is = |o: O| op == o as u8;
        if is(O::NewTable) || is(O::DupTable) {
            self.st.set_mem(ra, TTAB);
        } else if is(O::NewClosure) || is(O::DupClosure) {
            self.st.set_mem(ra, LUA_TFUNCTION as u8);
        } else if is(O::Not) {
            self.st.set_mem(ra, TBOOL);
        } else if is(O::Concat) {
            // number operands are converted to strings in place
            for r in b_of(insn) as usize..=c_of(insn) as usize {
                self.st.kill(r);
            }
            self.st.kill(ra);
        } else if is(O::GetGlobal) || is(O::GetTableKs) || is(O::GetTable) || is(O::GetTableN) || is(O::GetImport) || is(O::GetUpval)
            || is(O::And) || is(O::Or) || is(O::AndK) || is(O::OrK) || is(O::Minus) || is(O::Length) || is(O::Mod)
            || is(O::Pow) || is(O::IDiv) || is(O::ModK) || is(O::PowK) || is(O::IDivK) || is(O::LoadKx) || is(O::Add) || is(O::Sub)
            || is(O::Mul) || is(O::Div) || is(O::AddK) || is(O::SubK) || is(O::MulK) || is(O::DivK) || is(O::SubRk) || is(O::DivRk)
            || is(O::LoadK) || is(O::Move) || is(O::LoadN) || is(O::LoadNil) || is(O::LoadB)
        {
            self.st.kill(ra);
        } else if is(O::SetGlobal) || is(O::SetUpval) || is(O::SetTable) || is(O::SetTableKs) || is(O::SetTableN) || is(O::SetList)
            || is(O::CloseUpvals) || is(O::Capture) || is(O::Nop) || is(O::Coverage) || is(O::Jump) || is(O::JumpBack) || is(O::JumpX)
            || is(O::JumpIf) || is(O::JumpIfNot) || is(O::JumpIfEq) || is(O::JumpIfLe) || is(O::JumpIfLt) || is(O::JumpIfNotEq)
            || is(O::JumpIfNotLe) || is(O::JumpIfNotLt) || is(O::JumpXEqKNil) || is(O::JumpXEqKB) || is(O::JumpXEqKN) || is(O::JumpXEqKS)
        {
            // no register written
        } else if is(O::ForNPrep) {
            for r in ra..ra + 3 {
                self.st.set_mem(r, TNUM);
            }
        } else if is(O::GetVarargs) || is(O::Namecall) || is(O::ForGPrep) || is(O::ForGPrepInext) || is(O::ForGPrepNext) || is(O::ForGLoop) {
            for r in ra..256 {
                self.st.fresh(r);
                self.st.tag[r] = T_ANY;
            }
        } else {
            self.st.forget_tags();
        }
    }

    // -- arithmetic -------------------------------------------------------------

    unsafe fn opnd_tag(&self, o: Opnd) -> u8 {
        match o {
            Opnd::Reg(r) => self.st.tag[r],
            Opnd::K(k) => self.k_tag(k),
        }
    }

    /// kind: 0 add, 1 sub, 2 mul, 3 div, 4 mod, 5 idiv, 6 pow
    unsafe fn t_arith(&mut self, kind: u8, ra: usize, lhs: Opnd, rhs: Opnd) -> bool {
        let fb = self.feedback();
        if self.generic || fb == Some((FB_GENERIC, FB_GENERIC)) {
            return kind <= 3 && self.t_arith_generic(kind, ra, lhs, rhs);
        }
        // expected operand types: proven, else observed, else number
        let pl = self.opnd_tag(lhs);
        let pr = self.opnd_tag(rhs);
        // without feedback, guess number; but vector +/- needs a vector on
        // both sides, so guess that when the other operand is one (a wrong
        // guess bails and records feedback like any other)
        let guess = |other: u8| if kind <= 1 && other == TVEC { TVEC } else { TNUM };
        let mut el = pl;
        let mut er = pr;
        if el == T_ANY {
            el = fb.map_or(guess(pr), |f| f.0);
        }
        if er == T_ANY {
            er = fb.map_or(guess(pl), |f| f.1);
        }
        if el == TNUM && er == TNUM {
            return self.t_arith_num(kind, ra, lhs, rhs);
        }
        let vec_ok = match (el, er) {
            (TVEC, TVEC) => kind <= 3,
            (TVEC, TNUM) | (TNUM, TVEC) => kind == 2 || kind == 3,
            _ => false,
        };
        if vec_ok {
            return self.t_arith_vec(kind, ra, lhs, rhs, el, er);
        }
        // an operand seen as something else (userdata / table: metamethods)
        // goes through the generic helper rather than an interpreter step
        let other = |t: u8| t != T_ANY && t != TNUM && t != TVEC;
        if (other(el) || other(er)) && !dbg_off(67108864) {
            return kind <= 3 && self.t_arith_generic(kind, ra, lhs, rhs);
        }
        false
    }

    /// GETUPVAL of an upvalue already read in this block (and not written
    /// since): a copy of that register. Records the read otherwise (the
    /// caller then loads it).
    fn t_getupval_cse(&mut self, ra: usize, idx: usize) -> bool {
        let Some(v) = self.st.cse_find(CSE_UPVAL, idx as u64, 0) else {
            return false;
        };
        let Some(s) = (0..256).find(|&s| s != ra && self.st.vid[s] == v) else {
            return false;
        };
        let t = self.st.tag[s];
        if t == TNUM {
            return self.st.def_num_from_vid(&mut self.a, ra, v);
        }
        if t == TINT || t == TVEC || t == T_ANY && self.st.dirty[s] {
            return false;
        }
        // in memory: copy the slot, keep what is known about it
        self.st.flush(&mut self.a, s);
        self.st.kill(ra);
        copy_tv(&mut self.a, R::R13, reg(ra as i32), R::R13, reg(s as i32));
        self.st.tag[ra] = t;
        self.st.vid[ra] = v;
        true
    }

    /// Operand identity for common subexpressions: a register's value
    /// number, or a constant's index.
    fn cse_opnd(&self, o: Opnd) -> u64 {
        match o {
            Opnd::Reg(r) => self.st.vid[r] as u64,
            Opnd::K(k) => (1u64 << 40) | k as u64,
        }
    }

    /// Number arithmetic, reusing the result of an identical earlier
    /// operation of the block when it is still held somewhere.
    unsafe fn t_arith_num(&mut self, kind: u8, ra: usize, lhs: Opnd, rhs: Opnd) -> bool {
        if kind > 3 || dbg_off(8192) {
            return self.t_arith_num_compute(kind, ra, lhs, rhs);
        }
        for o in [lhs, rhs] {
            if let Opnd::Reg(r) = o {
                self.guard(r, TNUM, true);
            }
        }
        let (ka, kb) = (self.cse_opnd(lhs), self.cse_opnd(rhs));
        if let Some(v) = self.st.cse_find(kind, ka, kb) {
            if self.st.def_num_from_vid(&mut self.a, ra, v) {
                return true;
            }
        }
        if !self.t_arith_num_compute(kind, ra, lhs, rhs) {
            return false;
        }
        if self.st.tag[ra] == TNUM {
            let v = self.st.vid[ra];
            self.st.cse_add(kind, ka, kb, v);
        }
        true
    }

    unsafe fn t_arith_num_compute(&mut self, kind: u8, ra: usize, lhs: Opnd, rhs: Opnd) -> bool {
        if (kind == 4 || kind == 5) && !self.sse41 {
            return false;
        }
        // x ^ 2 and x ^ 0.5 need no call
        let powk = if kind == 6 {
            match rhs {
                Opnd::K(k) => self.k_num(k),
                _ => None,
            }
        } else {
            None
        };
        for o in [lhs, rhs] {
            if let Opnd::Reg(r) = o {
                self.guard(r, TNUM, true);
            }
        }
        let xl = match lhs {
            Opnd::Reg(r) => Some(self.st.num_x(&mut self.a, r)),
            Opnd::K(_) => None,
        };
        let xr = match rhs {
            Opnd::Reg(r) => Some(self.st.num_x(&mut self.a, r)),
            Opnd::K(_) => None,
        };
        let koff = |o: Opnd| match o {
            Opnd::K(k) => reg(k as i32),
            _ => 0,
        };
        let sdop = [0x58u8, 0x5c, 0x59, 0x5e];
        // x / 2^n is exactly x * 2^-n (both are one correctly rounded
        // operation on the same real value), and mulsd has a fraction of
        // divsd's latency
        let (kind, xr) = match (kind, rhs, xr) {
            (3, Opnd::K(k), None) => match self.k_num(k).and_then(exact_reciprocal) {
                Some(r) if !dbg_off(8192) => (2, Some(self.st.const_x(&mut self.a, r.to_bits()))),
                Some(r) => {
                    self.a.mov_ri(R::Rax, r.to_bits());
                    self.a.movq_xr(X(1), R::Rax);
                    (2, Some(X(1)))
                }
                None => (kind, xr),
            },
            _ => (kind, xr),
        };
        if kind <= 3 {
            let in_place_ok = xl.is_some() && rhs != Opnd::Reg(ra);
            if in_place_ok {
                let xl = xl.unwrap();
                let xd = self.st.def_num(&mut self.a, ra);
                if xd != xl && self.avx {
                    match xr {
                        Some(xr) => self.a.vsd_rr(sdop[kind as usize], xd, xl, xr),
                        None => self.a.vsd_rm(sdop[kind as usize], xd, xl, R::R14, koff(rhs)),
                    }
                    return true;
                }
                if xd != xl {
                    self.a.movapd(xd, xl);
                }
                match xr {
                    Some(xr) => self.a.sd_rr(sdop[kind as usize], xd, xr),
                    None => self.a.sd_rm(sdop[kind as usize], xd, R::R14, koff(rhs)),
                }
                return true;
            }
            match xl {
                Some(xl) => self.a.movapd(X(0), xl),
                None => self.a.movsd_load(X(0), R::R14, koff(lhs)),
            }
            match xr {
                Some(xr) => self.a.sd_rr(sdop[kind as usize], X(0), xr),
                None => self.a.sd_rm(sdop[kind as usize], X(0), R::R14, koff(rhs)),
            }
        } else {
            match xl {
                Some(xl) => self.a.movapd(X(0), xl),
                None => self.a.movsd_load(X(0), R::R14, koff(lhs)),
            }
            let xr = match xr {
                Some(xr) => xr,
                None => {
                    self.a.movsd_load(X(1), R::R14, koff(rhs));
                    X(1)
                }
            };
            match kind {
                4 => {
                    // a - floor(a / b) * b
                    self.a.movapd(X(2), X(0));
                    self.a.divsd(X(2), xr);
                    self.a.roundsd(X(2), X(2), 9);
                    self.a.mulsd(X(2), xr);
                    self.a.subsd(X(0), X(2));
                }
                5 => {
                    self.a.divsd(X(0), xr);
                    self.a.roundsd(X(0), X(0), 9);
                }
                _ => {
                    if powk == Some(2.0) {
                        self.a.mulsd(X(0), X(0));
                    } else if powk == Some(0.5) {
                        self.a.sqrtsd(X(0), X(0));
                    } else if powk == Some(3.0) {
                        self.a.movapd(X(2), X(0));
                        self.a.mulsd(X(0), X(0));
                        self.a.mulsd(X(0), X(2));
                    } else {
                        if xr != X(1) {
                            self.a.movapd(X(1), xr);
                        }
                        self.sync();
                        self.a.call_abs(jit_pow as usize);
                    }
                }
            }
        }
        let xd = self.st.def_num(&mut self.a, ra);
        self.a.movapd(xd, X(0));
        true
    }

    unsafe fn t_arith_vec(&mut self, kind: u8, ra: usize, lhs: Opnd, rhs: Opnd, el: u8, er: u8) -> bool {
        for (o, t) in [(lhs, el), (rhs, er)] {
            if let Opnd::Reg(r) = o {
                if !self.can_be(r, t) {
                    return false;
                }
            }
        }
        for (o, t) in [(lhs, el), (rhs, er)] {
            if let Opnd::Reg(r) = o {
                self.guard(r, t, true);
            }
        }
        // operands as packed floats: lhs in xmm0, rhs in a register
        let fetch = |tr: &mut Self, o: Opnd, t: u8, scratch: X| -> X {
            match (o, t) {
                (Opnd::Reg(r), TVEC) => tr.st.vec_x(&mut tr.a, r),
                (Opnd::Reg(r), _) => {
                    let x = tr.st.num_x(&mut tr.a, r);
                    tr.a.cvtsd2ss(scratch, x);
                    tr.a.shufps(scratch, scratch, 0);
                    scratch
                }
                (Opnd::K(k), TVEC) => {
                    load_vec(&mut tr.a, scratch, X(2), R::R14, reg(k as i32));
                    scratch
                }
                (Opnd::K(k), _) => {
                    tr.a.cvtsd2ss_m(scratch, R::R14, reg(k as i32));
                    tr.a.shufps(scratch, scratch, 0);
                    scratch
                }
            }
        };
        let xl = fetch(self, lhs, el, X(0));
        let xr = fetch(self, rhs, er, X(1));
        if self.avx && !dbg_off(32) {
            let xd = self.st.def_vec(&mut self.a, ra);
            self.a.vps_rr([0x58u8, 0x5c, 0x59, 0x5e][kind as usize], xd, xl, xr);
            return true;
        }
        if xl != X(0) {
            self.a.movaps(X(0), xl);
        }
        self.a.ps_rr([0x58u8, 0x5c, 0x59, 0x5e][kind as usize], X(0), xr);
        let xd = self.st.def_vec(&mut self.a, ra);
        self.a.movaps(xd, X(0));
        true
    }

    /// Operand types decided at run time: numbers, vector +- vector,
    /// vector */ vector-or-number, number */ vector; anything else runs the
    /// step.
    unsafe fn t_arith_generic(&mut self, kind: u8, ra: usize, lhs: Opnd, rhs: Opnd) -> bool {
        for o in [lhs, rhs] {
            if let Opnd::Reg(r) = o {
                self.st.flush(&mut self.a, r);
            }
        }
        let s = self.slow(false);
        let (lb, lo) = match lhs {
            Opnd::Reg(r) => (R::R13, reg(r as i32)),
            Opnd::K(k) => (R::R14, reg(k as i32)),
        };
        let (rbase, ro) = match rhs {
            Opnd::Reg(r) => (R::R13, reg(r as i32)),
            Opnd::K(k) => (R::R14, reg(k as i32)),
        };
        let a = &mut self.a;
        let done = a.label();
        let notnum = a.label();
        a.cmp32_mi(lb, lo + TT, LUA_TNUMBER);
        a.jcc(Cond::NE, notnum);
        a.cmp32_mi(rbase, ro + TT, LUA_TNUMBER);
        a.jcc(Cond::NE, notnum);
        a.movsd_load(X(0), lb, lo);
        a.sd_rm([0x58u8, 0x5c, 0x59, 0x5e][kind as usize], X(0), rbase, ro);
        a.movsd_store(R::R13, reg(ra as i32), X(0));
        a.mov_ri(R::Rax, NUM_TAG);
        a.store(R::R13, reg(ra as i32) + TAGWORD, R::Rax);
        a.jmp(done);
        a.bind(notnum);
        let psop = [0x58u8, 0x5c, 0x59, 0x5e][kind as usize];
        let vv = a.label();
        let vn = a.label();
        let nv = a.label();
        let store_vec_l = a.label();
        a.cmp32_mi(lb, lo + TT, LUA_TVECTOR);
        let lhs_not_vec = a.label();
        a.jcc(Cond::NE, lhs_not_vec);
        a.cmp32_mi(rbase, ro + TT, LUA_TVECTOR);
        a.jcc(Cond::E, vv);
        if kind >= 2 {
            a.cmp32_mi(rbase, ro + TT, LUA_TNUMBER);
            a.jcc(Cond::E, vn);
        }
        a.jmp(s);
        a.bind(lhs_not_vec);
        if kind >= 2 {
            a.cmp32_mi(lb, lo + TT, LUA_TNUMBER);
            a.jcc(Cond::NE, s);
            a.cmp32_mi(rbase, ro + TT, LUA_TVECTOR);
            a.jcc(Cond::E, nv);
        }
        a.jmp(s);
        a.bind(vv);
        load_vec(a, X(0), X(2), lb, lo);
        load_vec(a, X(1), X(2), rbase, ro);
        a.ps_rr(psop, X(0), X(1));
        a.jmp(store_vec_l);
        a.bind(vn);
        load_vec(a, X(0), X(2), lb, lo);
        a.cvtsd2ss_m(X(1), rbase, ro);
        a.shufps(X(1), X(1), 0);
        a.ps_rr(psop, X(0), X(1));
        a.jmp(store_vec_l);
        a.bind(nv);
        a.cvtsd2ss_m(X(0), lb, lo);
        a.shufps(X(0), X(0), 0);
        load_vec(a, X(1), X(2), rbase, ro);
        a.ps_rr(psop, X(0), X(1));
        a.bind(store_vec_l);
        store_vec_to(a, R::R13, reg(ra as i32), X(0));
        a.bind(done);
        self.st.kill(ra);
        true
    }

    unsafe fn t_minus(&mut self, ra: usize, rb: usize) -> bool {
        let fb = self.feedback();
        if self.generic || fb == Some((FB_GENERIC, FB_GENERIC)) {
            return false;
        }
        let mut t = self.st.tag[rb];
        if t == T_ANY {
            t = fb.map_or(TNUM, |f| f.0);
        }
        if t == TNUM {
            self.guard(rb, TNUM, true);
            let xs = self.st.num_x(&mut self.a, rb);
            self.a.mov_ri(R::Rax, 1u64 << 63);
            self.a.movq_xr(X(1), R::Rax);
            self.a.movapd(X(0), xs);
            self.a.xorpd(X(0), X(1));
            let xd = self.st.def_num(&mut self.a, ra);
            self.a.movapd(xd, X(0));
            true
        } else if t == TVEC {
            self.guard(rb, TVEC, true);
            let xs = self.st.vec_x(&mut self.a, rb);
            self.a.mov_ri(R::Rax, 0x8000_0000);
            self.a.movd_xr(X(1), R::Rax);
            self.a.shufps(X(1), X(1), 0);
            self.a.movaps(X(0), xs);
            self.a.xorps(X(0), X(1));
            let xd = self.st.def_vec(&mut self.a, ra);
            self.a.movaps(xd, X(0));
            true
        } else {
            false
        }
    }

    // -- tables -----------------------------------------------------------------

    /// ecx = zero-based array index from the number in `rc`; rejoin slow path
    /// when it is not an integer.
    fn array_index(&mut self, rc: usize) {
        if self.st.has_exact_u32(rc) {
            let g = self.st.num_u32(&mut self.a, rc);
            self.a.mov_rr32(R::Rcx, g);
        } else {
            let xi = self.st.num_x(&mut self.a, rc);
            // a bail, not a rejoin: the index is cached as an exact integer
            // below, which a rejoin would reload by truncation
            let s = self.slow(true);
            self.a.cvttsd2si_x(R::Rcx, xi);
            self.a.cvtsi2sd(X(1), R::Rcx);
            self.a.ucomisd(X(1), xi);
            self.a.jcc(Cond::NE, s);
            self.a.jcc(Cond::P, s);
            // negative indices never hit the array part: no need to cache them
            self.a.test32_rr(R::Rcx, R::Rcx);
            self.a.jcc(Cond::L, s);
            self.st.attach_exact_u32(&mut self.a, rc, R::Rcx);
        }
        self.a.sub32_ri(R::Rcx, 1);
    }

    /// rdx = address of array slot ecx of table `t` (no metatable).
    fn array_slot(&mut self, t: R) {
        let s = self.slow(false);
        self.a.cmp64_mi(t, OFF_T_METATABLE, 0);
        self.a.jcc(Cond::NE, s);
        self.a.cmp32_rm(R::Rcx, t, OFF_T_SIZEARRAY);
        self.a.jcc(Cond::AE, s);
        self.a.load(R::Rdx, t, OFF_T_ARRAY);
        self.a.shl_ri(R::Rcx, 4);
        self.a.add_rr(R::Rdx, R::Rcx);
    }

    unsafe fn t_gettable(&mut self, ra: usize, rb: usize, rc: usize) -> bool {
        if !self.can_be(rb, TTAB) {
            return false;
        }
        // a key that was not a number before: hash lookup for any key type
        if self.feedback().is_some() || !self.can_be(rc, TNUM) {
            return self.t_gettable_any(ra, rb, rc);
        }
        self.guard(rb, TTAB, false);
        // a key of another type bails once and recompiles with the hash path
        self.guard(rc, TNUM, true);
        let t = self.st.ptr_g(&mut self.a, rb);
        self.array_index(rc);
        self.array_slot(t);
        self.load_tv(ra, R::Rdx, 0, self.i + 1);
        true
    }

    /// `t[k]` for a key of any type: `luaH_get`; a nil result from a table
    /// with a metatable takes the interpreter path (`__index`).
    unsafe fn t_gettable_any(&mut self, ra: usize, rb: usize, rc: usize) -> bool {
        if ra == rb || ra == rc {
            return false;
        }
        self.guard(rb, TTAB, false);
        self.st.flush(&mut self.a, rc);
        self.sync();
        let s = self.slow(false);
        let found = self.a.label();
        self.a.load(R::Rdi, R::R13, reg(rb as i32));
        self.a.lea(R::Rsi, R::R13, reg(rc as i32));
        self.a.call_abs(jit_luah_get as usize);
        self.a.cmp32_mi(R::Rax, TT, LUA_TNIL);
        self.a.jcc(Cond::NE, found);
        self.a.load(R::Rcx, R::R13, reg(rb as i32));
        self.a.cmp64_mi(R::Rcx, OFF_T_METATABLE, 0);
        self.a.jcc(Cond::NE, s);
        self.a.bind(found);
        self.a.load(R::Rcx, R::Rax, 0);
        self.a.store(R::R13, reg(ra as i32), R::Rcx);
        self.a.load(R::Rcx, R::Rax, 8);
        self.a.store(R::R13, reg(ra as i32) + 8, R::Rcx);
        self.st.kill(ra);
        true
    }

    fn t_settable(&mut self, ra: usize, rb: usize, rc: usize) -> bool {
        // keys that were not array indices before (or objects that were not
        // tables): luaV_settable straight from native code
        if !self.can_be(rb, TTAB) || !self.can_be(rc, TNUM) || self.feedback().is_some() || self.generic {
            if dbg_off(1024) {
                return false;
            }
            self.sync();
            self.a.mov_rr(R::Rdi, R::R12);
            self.a.lea(R::Rsi, R::R13, reg(rb as i32));
            self.a.lea(R::Rdx, R::R13, reg(rc as i32));
            self.a.lea(R::Rcx, R::R13, reg(ra as i32));
            self.a.mov_ri(R::R8, self.pc(self.i) as u64 + 4);
            self.a.call_abs(jit_settable as usize);
            self.a.load(R::R13, R::R12, OFF_BASE);
            return true;
        }
        self.guard(rb, TTAB, false);
        // another key type bails once and recompiles with the path above
        self.guard(rc, TNUM, !dbg_off(1024));
        let t = self.st.ptr_g(&mut self.a, rb);
        self.array_index(rc);
        self.array_slot(t);
        let s = self.slow(false);
        self.a.cmp8_mi(t, OFF_T_READONLY, 0);
        self.a.jcc(Cond::NE, s);
        self.barrier(t, OFF_T_MARKED, ra);
        self.copy_to_mem(R::Rdx, 0, ra);
        true
    }

    unsafe fn t_tablen(&mut self, set: bool, ra: usize, rb: usize, idx: i32) -> bool {
        // not a table (proven, or seen at run time): luaV_gettable/settable
        // straight from native code
        // (also tables with a metatable, which bail once from the template)
        if !self.can_be(rb, TTAB) || ((self.feedback().is_some() || self.generic) && !dbg_off(1024)) {
            self.sync();
            self.a.mov_rr(R::Rdi, R::R12);
            self.a.lea(R::Rsi, R::R13, reg(rb as i32));
            self.a.mov_ri(R::Rdx, (idx as u32 as u64) | ((set as u64) << 32));
            self.a.lea(R::Rcx, R::R13, reg(ra as i32));
            self.a.mov_ri(R::R8, self.pc(self.i) as u64 + 4);
            self.a.call_abs(jit_tablen as usize);
            self.a.load(R::R13, R::R12, OFF_BASE);
            if !set {
                self.st.kill(ra);
            }
            return true;
        }
        // a non-table bails once and recompiles with the path above
        self.guard(rb, TTAB, !dbg_off(1024));
        let t = self.st.ptr_g(&mut self.a, rb);
        // a metatable bails once too (recompiled with the luaV_* path)
        let sm = self.slow(!dbg_off(1024));
        self.a.cmp64_mi(t, OFF_T_METATABLE, 0);
        self.a.jcc(Cond::NE, sm);
        let s = self.slow(false);
        self.a.cmp32_mi(t, OFF_T_SIZEARRAY, idx);
        self.a.jcc(Cond::BE, s);
        if set {
            self.a.cmp8_mi(t, OFF_T_READONLY, 0);
            self.a.jcc(Cond::NE, s);
            self.barrier(t, OFF_T_MARKED, ra);
            self.a.load(R::Rdx, t, OFF_T_ARRAY);
            self.copy_to_mem(R::Rdx, reg(idx), ra);
        } else {
            self.a.load(R::Rdx, t, OFF_T_ARRAY);
            self.load_tv(ra, R::Rdx, reg(idx), self.i + 1);
        }
        true
    }

    /// SETLIST of values known not to be collectable into a table whose array
    /// part is already big enough: stored straight from where the values are
    /// (no stack round trip, no barrier needed). Anything else runs in the
    /// interpreter.
    unsafe fn t_setlist(&mut self, ra: usize, rb: usize, c: usize, index: u32) -> bool {
        if c == 0 || index == 0 || self.st.tag[ra] != TTAB {
            return false;
        }
        let n = c - 1;
        if rb + n > 256 || !(rb..rb + n).all(|r| matches!(self.st.tag[r], TNUM | TINT | TVEC | TBOOL | TNIL)) {
            return false;
        }
        let last = index as i64 + n as i64 - 1;
        if last > i32::MAX as i64 {
            return false;
        }
        let t = self.st.ptr_g(&mut self.a, ra);
        let s = self.slow(false);
        self.a.cmp32_mi(t, OFF_T_SIZEARRAY, last as i32);
        self.a.jcc(Cond::L, s);
        // (copy_to_mem only uses r8, r9 and xmm2 as scratch)
        self.a.load(R::Rdx, t, OFF_T_ARRAY);
        for k in 0..n {
            self.copy_to_mem(R::Rdx, (index as i32 - 1 + k as i32) * 16, rb + k);
        }
        true
    }

    /// `#t` of a table without a metatable: luaH_getn. Other objects bail
    /// once and then stay in the interpreter.
    unsafe fn t_length(&mut self, ra: usize, rb: usize) -> bool {
        if !self.can_be(rb, TTAB) || self.feedback().is_some() || self.generic {
            return false;
        }
        self.guard(rb, TTAB, true);
        self.sync();
        let s = self.slow(false);
        self.a.load(R::Rdi, R::R13, reg(rb as i32));
        self.a.cmp64_mi(R::Rdi, OFF_T_METATABLE, 0);
        self.a.jcc(Cond::NE, s);
        self.a.call_abs(jit_getn as usize);
        let gd = self.st.def_u32(&mut self.a, ra);
        self.a.mov_rr32(gd, R::Rax);
        true
    }

    /// Two-operand `integer` builtin computed straight into the destination
    /// register, with immediate forms for constant operands. False (nothing
    /// emitted) for the shapes left to the generic sequence.
    unsafe fn t_int2(&mut self, kind: u8, x: Opnd, y: Opnd, dst: usize) -> bool {
        let kval = |tr: &Self, o: Opnd| match o {
            Opnd::K(k) => Some((*(*tr.p).k.add(k)).value.l),
            _ => None,
        };
        let ky = kval(self, y);
        // variable shift/rotate counts need cl: generic path
        if kind >= 6 && ky.is_none() {
            return false;
        }
        let Opnd::Reg(rx) = x else { return false };
        let gx = self.st.int_g(&mut self.a, rx);
        let gy = match y {
            Opnd::Reg(ry) => Some(self.st.int_g(&mut self.a, ry)),
            _ => None,
        };
        let gd = self.st.def_int(&mut self.a, dst);
        // the second operand must survive writing the first into gd
        let gy = match gy {
            Some(g) if g == gd && gd != gx => {
                self.a.mov_rr(R::Rcx, g);
                Some(R::Rcx)
            }
            g => g,
        };
        let fits = |v: i64| (i32::MIN as i64..=i32::MAX as i64).contains(&v);
        if kind == 2 {
            if let Some(v) = ky.filter(|&v| fits(v)) {
                self.a.imul64_rri(gd, gx, v as i32);
                return true;
            }
        }
        if gd != gx {
            self.a.mov_rr(gd, gx);
        }
        match (kind, gy, ky) {
            (0..=5, Some(g), _) => {
                if kind == 2 {
                    self.a.imul64_rr(gd, g);
                } else {
                    self.a.alu64_rr([0x01, 0x29, 0, 0x21, 0x09, 0x31][kind as usize], gd, g);
                }
            }
            (0..=5, None, Some(v)) if kind != 2 && fits(v) => {
                self.a.alu64_ri([0, 5, 0, 4, 1, 6][kind as usize], gd, v as i32);
            }
            (0..=5, None, Some(v)) => {
                self.a.mov_ri(R::Rcx, v as u64);
                if kind == 2 {
                    self.a.imul64_rr(gd, R::Rcx);
                } else {
                    self.a.alu64_rr([0x01, 0x29, 0, 0x21, 0x09, 0x31][kind as usize], gd, R::Rcx);
                }
            }
            // rotates: the count is taken mod 64
            (6 | 7, None, Some(v)) => {
                let c = (v & 63) as u8;
                if c != 0 {
                    self.a.shift64_ri(if kind == 6 { 0 } else { 1 }, gd, c);
                }
            }
            // shifts: a count in -63..=63 (negative reverses), otherwise 0
            (8 | 9, None, Some(v)) => {
                let (fwd, rev) = if kind == 8 { (4u8, 5u8) } else { (5, 4) };
                if (1..=63).contains(&v) {
                    self.a.shift64_ri(fwd, gd, v as u8);
                } else if (-63..=-1).contains(&v) {
                    self.a.shift64_ri(rev, gd, (-v) as u8);
                } else if v != 0 {
                    self.a.alu64_rr(0x31, gd, gd);
                }
            }
            _ => unreachable!(),
        }
        true
    }

    /// Lane of a vector component name (x/y/z, either case).
    unsafe fn vec_lane(kv: *const TValue) -> Option<u8> {
        let ts = (*kv).value.gc as *mut TString;
        let s = sbytes(ts);
        if s.len() != 1 {
            return None;
        }
        match s[0] | 0x20 {
            b'x' => Some(0),
            b'y' => Some(1),
            b'z' => Some(2),
            _ => None,
        }
    }

    unsafe fn t_gettableks(&mut self, ra: usize, rb: usize, aux: u32) -> bool {
        let kv = (*self.p).k.add(aux as usize);
        if (*kv).tt != LUA_TSTRING {
            return false;
        }
        let lane = Self::vec_lane(kv);
        let fb = self.feedback();
        let generic = self.generic || fb.is_some_and(|f| f.0 == FB_GENERIC);
        // the object's type: proven, else seen at run time, else a table
        let mut want = self.st.tag[rb];
        if want == T_ANY {
            want = match fb {
                Some((t, _)) if !generic => t,
                _ => TTAB,
            };
        }
        let cslot = self.pc(self.i) + 3;
        let kgc = (*kv).value.gc as usize;
        if generic && self.st.tag[rb] == T_ANY && lane.is_some() {
            // table or vector, decided at run time (the result is untyped)
            let lane = lane.unwrap() as i32;
            let s = self.slow(false);
            let tbl = self.a.label();
            let done = self.a.label();
            let off = reg(rb as i32);
            self.a.cmp32_mi(R::R13, off + TT, LUA_TTABLE);
            self.a.jcc(Cond::E, tbl);
            self.a.cmp32_mi(R::R13, off + TT, LUA_TVECTOR);
            self.a.jcc(Cond::NE, s);
            self.a.cvtss2sd_m(X(0), R::R13, off + 4 * lane);
            self.a.movsd_store(R::R13, reg(ra as i32), X(0));
            self.a.mov_ri(R::Rax, NUM_TAG);
            self.a.store(R::R13, reg(ra as i32) + TAGWORD, R::Rax);
            self.a.jmp(done);
            self.a.bind(tbl);
            self.a.load(R::Rax, R::R13, off);
            self.node_at(R::Rax, cslot, kgc, s);
            copy_tv(&mut self.a, R::R13, reg(ra as i32), R::Rcx, 0);
            self.a.bind(done);
            self.st.kill(ra);
            return true;
        }
        if want == TVEC {
            let Some(lane) = lane else { return false };
            self.guard(rb, TVEC, true);
            let xv = self.st.vec_x(&mut self.a, rb);
            self.a.movaps(X(0), xv);
            if lane != 0 {
                self.a.shufps(X(0), X(0), lane);
            }
            let xd = self.st.def_num(&mut self.a, ra);
            self.a.cvtss2sd(xd, X(0));
            return true;
        }
        if want != TTAB {
            return false;
        }
        // a vector component name on an untyped object: find out what it is
        // (a vector bails once and the function is recompiled for it)
        let probe = lane.is_some() && fb.is_none() && !self.generic;
        self.guard(rb, TTAB, probe);
        let t = self.st.ptr_g(&mut self.a, rb);
        let s = self.slow(false);
        self.node_at(t, cslot, kgc, s);
        self.load_tv(ra, R::Rcx, 0, self.i + 2);
        true
    }

    unsafe fn t_settableks(&mut self, ra: usize, rb: usize, aux: u32) -> bool {
        let kv = (*self.p).k.add(aux as usize);
        if (*kv).tt != LUA_TSTRING {
            return false;
        }
        // not a table (proven, or seen at run time): luaV_settable from
        // native code
        if !self.can_be(rb, TTAB) || ((self.feedback().is_some() || self.generic) && self.st.tag[rb] != TTAB && !dbg_off(1024)) {
            self.sync();
            self.a.mov_rr(R::Rdi, R::R12);
            self.a.lea(R::Rsi, R::R13, reg(rb as i32));
            self.a.lea(R::Rdx, R::R14, reg(aux as i32));
            self.a.lea(R::Rcx, R::R13, reg(ra as i32));
            self.a.mov_ri(R::R8, self.pc(self.i) as u64 + 8);
            self.a.call_abs(jit_settablek as usize);
            self.a.load(R::R13, R::R12, OFF_BASE);
            return true;
        }
        // a non-table bails once and recompiles with the path above
        self.guard(rb, TTAB, !dbg_off(1024));
        let t = self.st.ptr_g(&mut self.a, rb);
        let s = self.slow(false);
        self.node_at(t, self.pc(self.i) + 3, (*kv).value.gc as usize, s);
        self.a.cmp8_mi(t, OFF_T_READONLY, 0);
        self.a.jcc(Cond::NE, s);
        self.barrier(t, OFF_T_MARKED, ra);
        self.copy_to_mem(R::Rcx, 0, ra);
        true
    }

    // -- branches ---------------------------------------------------------------

    fn t_jumpif(&mut self, ra: usize, target: usize, when_true: bool) {
        match self.st.tag[ra] {
            TNIL => {
                if !when_true {
                    self.jmp_to(target);
                }
            }
            TBOOL => {
                self.a.cmp32_mi(R::R13, reg(ra as i32), 0);
                self.jcc_to(if when_true { Cond::NE } else { Cond::E }, target);
            }
            T_ANY => {
                self.flush_here();
                let tl = self.labels[target];
                let a = &mut self.a;
                let falsy = a.label();
                let truthy = a.label();
                a.cmp32_mi(R::R13, reg(ra as i32) + TT, LUA_TNIL);
                a.jcc(Cond::E, falsy);
                a.cmp32_mi(R::R13, reg(ra as i32) + TT, LUA_TBOOLEAN);
                a.jcc(Cond::NE, truthy);
                a.cmp32_mi(R::R13, reg(ra as i32), 0);
                a.jcc(Cond::E, falsy);
                a.bind(truthy);
                if when_true {
                    a.jmp(tl);
                    a.bind(falsy);
                } else {
                    let over = a.label();
                    a.jmp(over);
                    a.bind(falsy);
                    a.jmp(tl);
                    a.bind(over);
                }
            }
            _ => {
                if when_true {
                    self.jmp_to(target);
                }
            }
        }
    }

    /// cond: 0 lt, 1 le, 2 not lt, 3 not le (R(ra) ? R(rb))
    fn t_jumpcmp(&mut self, cond: u8, ra: usize, rb: usize, target: usize) -> bool {
        if !self.can_be(ra, TNUM) || !self.can_be(rb, TNUM) {
            return false;
        }
        self.guard(ra, TNUM, false);
        self.guard(rb, TNUM, false);
        let xa = self.st.num_x(&mut self.a, ra);
        let xb = self.st.num_x(&mut self.a, rb);
        // compare b with a: a < b <=> b > a ("above", false if unordered)
        self.a.ucomisd(xb, xa);
        let c = [Cond::A, Cond::AE, Cond::BE, Cond::B][cond as usize];
        self.jcc_to(c, target);
        true
    }

    fn t_jumpeq(&mut self, ra: usize, rb: usize, target: usize, when_eq: bool) {
        if self.st.tag[ra] == TINT && self.st.tag[rb] == TINT {
            let ga = self.st.int_g(&mut self.a, ra);
            let gb = self.st.int_g(&mut self.a, rb);
            self.a.cmp_rr(ga, gb);
            self.jcc_to(if when_eq { Cond::E } else { Cond::NE }, target);
            return;
        }
        if self.st.tag[ra] == TNUM && self.st.tag[rb] == TNUM {
            let xa = self.st.num_x(&mut self.a, ra);
            let xb = self.st.num_x(&mut self.a, rb);
            self.a.ucomisd(xa, xb);
            if when_eq {
                let over = self.a.label();
                self.a.jcc(Cond::P, over);
                self.jcc_to(Cond::E, target);
                self.a.bind(over);
            } else {
                self.jcc_to(Cond::P, target);
                self.jcc_to(Cond::NE, target);
            }
            return;
        }
        self.flush_here();
        let s = self.slow(false);
        let tl = self.labels[target];
        let (oa, ob) = (reg(ra as i32), reg(rb as i32));
        let a = &mut self.a;
        let eq = a.label();
        let ne = a.label();
        let num = a.label();
        let boolean = a.label();
        let ptr = a.label();
        let table = a.label();
        a.load32(R::Rax, R::R13, oa + TT);
        a.cmp32_rm(R::Rax, R::R13, ob + TT);
        a.jcc(Cond::NE, ne); // different types are never equal
        a.cmp32_ri(R::Rax, LUA_TNIL);
        a.jcc(Cond::E, eq);
        a.cmp32_ri(R::Rax, LUA_TNUMBER);
        a.jcc(Cond::E, num);
        a.cmp32_ri(R::Rax, LUA_TBOOLEAN);
        a.jcc(Cond::E, boolean);
        a.cmp32_ri(R::Rax, LUA_TSTRING);
        a.jcc(Cond::E, ptr);
        a.cmp32_ri(R::Rax, LUA_TINTEGER);
        a.jcc(Cond::E, ptr); // same 8-byte payload
        a.cmp32_ri(R::Rax, LUA_TTABLE);
        a.jcc(Cond::E, table);
        a.jmp(s);
        // tables without a metatable: identical means equal (with a
        // metatable, __eq runs even for the same table)
        a.bind(table);
        a.load(R::Rcx, R::R13, oa);
        a.cmp_rm(R::Rcx, R::R13, ob);
        a.jcc(Cond::NE, s);
        a.cmp64_mi(R::Rcx, OFF_T_METATABLE, 0);
        a.jcc(Cond::E, eq);
        a.jmp(s);
        a.bind(num);
        a.movsd_load(X(0), R::R13, oa);
        a.ucomisd_m(X(0), R::R13, ob);
        a.jcc(Cond::P, ne);
        a.jcc(Cond::NE, ne);
        a.jmp(eq);
        a.bind(boolean);
        a.load32(R::Rcx, R::R13, oa);
        a.cmp32_rm(R::Rcx, R::R13, ob);
        a.jcc(Cond::E, eq);
        a.jmp(ne);
        // interned strings: equal iff the same object
        a.bind(ptr);
        a.load(R::Rcx, R::R13, oa);
        a.cmp_rm(R::Rcx, R::R13, ob);
        a.jcc(Cond::NE, ne);
        a.bind(eq);
        if when_eq {
            a.jmp(tl);
            a.bind(ne);
        } else {
            let over = a.label();
            a.jmp(over);
            a.bind(ne);
            a.jmp(tl);
            a.bind(over);
        }
    }

    /// JUMPXEQK{NIL,B,N,S}: kind 0 nil, 1 boolean, 2 number, 3 string
    unsafe fn t_jumpxeqk(&mut self, kind: u8, ra: usize, aux: u32, target: usize) {
        let not = aux >> 31 != 0;
        let kidx = (aux & 0xffffff) as usize;
        let ktag = [TNIL, TBOOL, TNUM, TSTR][kind as usize];
        let t = self.st.tag[ra];
        if t != T_ANY && t != ktag {
            // never equal
            if not {
                self.jmp_to(target);
            }
            return;
        }
        if t == TNIL {
            if !not {
                self.jmp_to(target);
            }
            return;
        }
        if t == TNUM {
            let xa = self.st.num_x(&mut self.a, ra);
            self.a.ucomisd_m(xa, R::R14, reg(kidx as i32));
            if not {
                self.jcc_to(Cond::P, target);
                self.jcc_to(Cond::NE, target);
            } else {
                let over = self.a.label();
                self.a.jcc(Cond::P, over);
                self.jcc_to(Cond::E, target);
                self.a.bind(over);
            }
            return;
        }
        self.flush_here();
        let tl = self.labels[target];
        let off = reg(ra as i32);
        let kgc = if kind == 3 { (*(*self.p).k.add(kidx)).value.gc as u64 } else { 0 };
        let a = &mut self.a;
        let skip = a.label();
        let ne = if not { tl } else { skip };
        match kind {
            0 => {
                a.cmp32_mi(R::R13, off + TT, LUA_TNIL);
                a.jcc(Cond::NE, ne);
            }
            1 => {
                if t == T_ANY {
                    a.cmp32_mi(R::R13, off + TT, LUA_TBOOLEAN);
                    a.jcc(Cond::NE, ne);
                }
                a.cmp32_mi(R::R13, off, (aux & 1) as i32);
                a.jcc(Cond::NE, ne);
            }
            2 => {
                a.cmp32_mi(R::R13, off + TT, LUA_TNUMBER);
                a.jcc(Cond::NE, ne);
                a.movsd_load(X(0), R::R13, off);
                a.ucomisd_m(X(0), R::R14, reg(kidx as i32));
                a.jcc(Cond::P, ne);
                a.jcc(Cond::NE, ne);
            }
            _ => {
                if t == T_ANY {
                    a.cmp32_mi(R::R13, off + TT, LUA_TSTRING);
                    a.jcc(Cond::NE, ne);
                }
                a.mov_ri(R::Rax, kgc);
                a.cmp_rm(R::Rax, R::R13, off);
                a.jcc(Cond::NE, ne);
            }
        }
        if !not {
            a.jmp(tl);
        }
        a.bind(skip);
    }

    // -- loops ------------------------------------------------------------------

    fn t_fornprep(&mut self, ra: usize, target: usize) -> bool {
        for r in ra..ra + 3 {
            if !self.can_be(r, TNUM) {
                return false;
            }
        }
        for r in ra..ra + 3 {
            self.guard(r, TNUM, false);
        }
        let xl = self.st.num_x(&mut self.a, ra);
        let xi = self.st.num_x(&mut self.a, ra + 2);
        match self.stepk[self.i] {
            Some(k) if k > 0.0 => {
                self.a.ucomisd(xl, xi);
                self.jcc_to(Cond::B, target);
            }
            Some(k) if k < 0.0 => {
                self.a.ucomisd(xi, xl);
                self.jcc_to(Cond::B, target);
            }
            _ => {
                let xs = self.st.num_x(&mut self.a, ra + 1);
                let neg = self.a.label();
                let body = self.a.label();
                self.a.xorpd(X(0), X(0));
                self.a.ucomisd(xs, X(0));
                self.a.jcc(Cond::BE, neg);
                self.a.ucomisd(xl, xi);
                self.jcc_to(Cond::B, target);
                self.a.jmp(body);
                self.a.bind(neg);
                self.a.ucomisd(xi, xl);
                self.jcc_to(Cond::B, target);
                self.a.bind(body);
            }
        }
        true
    }

    fn t_fornloop(&mut self, ra: usize, target: usize) {
        // an entry point: everything is on the stack (but carried registers);
        // limit, step and index are numbers since FORNPREP
        let s = self.slow(false);
        let tl = self.carry_inner.get(&target).copied().unwrap_or(self.labels[target]);
        let prep = target.wrapping_sub(1);
        let stepk = if prep < self.n { self.stepk[prep] } else { None };
        emit_loop_checks(&mut self.a, s);
        self.trace_counter();
        let a = &mut self.a;
        let (ol, os, oi) = (reg(ra as i32), reg(ra as i32 + 1), reg(ra as i32 + 2));
        a.movsd_load(X(0), R::R13, oi);
        a.addsd_m(X(0), R::R13, os);
        a.movsd_store(R::R13, oi, X(0));
        a.movsd_load(X(1), R::R13, ol);
        match stepk {
            Some(k) if k > 0.0 => {
                a.ucomisd(X(1), X(0));
                a.jcc(Cond::AE, tl);
            }
            Some(k) if k < 0.0 => {
                a.ucomisd(X(0), X(1));
                a.jcc(Cond::AE, tl);
            }
            _ => {
                a.xorpd(X(2), X(2));
                a.ucomisd_m(X(2), R::R13, os);
                let neg = a.label();
                let done = a.label();
                a.jcc(Cond::AE, neg); // 0 >= step
                a.ucomisd(X(1), X(0));
                a.jcc(Cond::AE, tl);
                a.jmp(done);
                a.bind(neg);
                a.ucomisd(X(0), X(1));
                a.jcc(Cond::AE, tl);
                a.bind(done);
            }
        }
    }

    fn t_forgloop(&mut self, ra: usize, auxi: i32, target: usize, exit: usize) -> bool {
        if auxi > 2 {
            return false;
        }
        self.flush_here();
        let s = self.slow(false);
        let tl = self.labels[target];
        let el = self.labels[exit];
        // the key (e.g. `_` in `for _, v in t`) need not be written when
        // the body never reads it
        let key_dead = !super::live::has(&self.live[target], ra + 3) && !self.is_volatile[ra + 3] && !dbg_off(8388608);
        let ra = ra as i32;
        let a = &mut self.a;
        emit_loop_checks(a, s);
        a.cmp32_mi(R::R13, reg(ra) + TT, LUA_TNIL);
        a.jcc(Cond::NE, s);
        a.cmp32_mi(R::R13, reg(ra + 1) + TT, LUA_TTABLE);
        a.jcc(Cond::NE, s);
        a.load(R::Rax, R::R13, reg(ra + 1));
        a.load32(R::Rcx, R::R13, reg(ra + 2));
        let top = a.label();
        let found = a.label();
        let hash = a.label();
        a.bind(top);
        a.cmp32_rm(R::Rcx, R::Rax, OFF_T_SIZEARRAY);
        a.jcc(Cond::AE, hash);
        a.load(R::Rdx, R::Rax, OFF_T_ARRAY);
        a.mov_rr32(R::R8, R::Rcx);
        a.shl_ri(R::R8, 4);
        a.add_rr(R::Rdx, R::R8);
        a.cmp32_mi(R::Rdx, TT, LUA_TNIL);
        a.jcc(Cond::NE, found);
        if auxi < 0 {
            a.jmp(el); // ipairs stops at the first nil
        } else {
            a.sub32_ri(R::Rcx, -1);
            a.jmp(top);
        }
        a.bind(found);
        // iterator state = index + 1, key = index + 1, value
        a.mov_rr32(R::R8, R::Rcx);
        a.add_ri(R::R8, 1);
        a.store(R::R13, reg(ra + 2), R::R8);
        a.mov_ri(R::R9, (LU_TAG_ITERATOR as u32 as u64) | ((LUA_TLIGHTUSERDATA as u64) << 32));
        a.store(R::R13, reg(ra + 2) + TAGWORD, R::R9);
        if !key_dead {
            a.cvtsi2sd64(X(0), R::R8);
            a.movsd_store(R::R13, reg(ra + 3), X(0));
            a.mov_ri(R::R9, NUM_TAG);
            a.store(R::R13, reg(ra + 3) + TAGWORD, R::R9);
        }
        copy_tv(a, R::R13, reg(ra + 4), R::Rdx, 0);
        a.jmp(tl);
        a.bind(hash);
        if auxi < 0 {
            a.jmp(el);
        } else {
            // hash part: j = index - sizearray over the 1 << lsizenode nodes
            let hloop = a.label();
            let hfound = a.label();
            a.mov_rr32(R::Rdx, R::Rcx);
            a.load32(R::R8, R::Rax, OFF_T_SIZEARRAY);
            a.alu32_rr(0x29, R::Rdx, R::R8); // sub edx, r8d
            a.movzx8(R::Rcx, R::Rax, OFF_T_LSIZENODE);
            a.mov_ri(R::R9, 1);
            a.shift32_cl(4, R::R9); // shl r9d, cl
            a.bind(hloop);
            a.cmp32_rr(R::Rdx, R::R9);
            a.jcc(Cond::AE, el);
            a.mov_rr32(R::R8, R::Rdx);
            a.shl_ri(R::R8, 5); // sizeof(LuaNode)
            a.load(R::Rcx, R::Rax, OFF_T_NODE);
            a.add_rr(R::R8, R::Rcx);
            a.cmp32_mi(R::R8, TT, LUA_TNIL);
            a.jcc(Cond::NE, hfound);
            a.sub32_ri(R::Rdx, -1);
            a.jmp(hloop);
            a.bind(hfound);
            // iterator state = index + 1 = j + sizearray + 1
            a.load32(R::Rcx, R::Rax, OFF_T_SIZEARRAY);
            a.alu32_rr(0x01, R::Rcx, R::Rdx); // add ecx, edx
            a.add_ri(R::Rcx, 1);
            a.store(R::R13, reg(ra + 2), R::Rcx);
            a.mov_ri(R::R9, (LU_TAG_ITERATOR as u32 as u64) | ((LUA_TLIGHTUSERDATA as u64) << 32));
            a.store(R::R13, reg(ra + 2) + TAGWORD, R::R9);
            // key (`getnodekey`: the tag is the low 4 bits of ttnext)
            a.load(R::Rcx, R::R8, NODE_KEY);
            a.store(R::R13, reg(ra + 3), R::Rcx);
            a.load32(R::Rcx, R::R8, NODE_KEY + TAGWORD);
            a.store32(R::R13, reg(ra + 3) + TAGWORD, R::Rcx);
            a.load32(R::Rcx, R::R8, NODE_KEY + TT);
            a.and32_ri(R::Rcx, 0xf);
            a.store32(R::R13, reg(ra + 3) + TT, R::Rcx);
            // value
            a.load(R::Rcx, R::R8, 0);
            a.store(R::R13, reg(ra + 4), R::Rcx);
            a.load(R::Rcx, R::R8, 8);
            a.store(R::R13, reg(ra + 4) + 8, R::Rcx);
            a.jmp(tl);
        }
        // the exit label is a join point only if marked; reaching the next
        // instruction by falling through never happens here
        true
    }

    // -- builtins ---------------------------------------------------------------

    /// Number operand as a double (constants into `scratch`).
    fn opx(&mut self, o: Opnd, scratch: X) -> X {
        match o {
            Opnd::Reg(r) => self.st.num_x(&mut self.a, r),
            Opnd::K(k) => {
                self.a.movsd_load(scratch, R::R14, reg(k as i32));
                scratch
            }
        }
    }

    /// eax = u32 form of the first operand folded with `ext` (alu /ext
    /// encoding: 4 and, 1 or, 6 xor) over the others.
    unsafe fn fold_u32(&mut self, ext: u8, args: &[Opnd]) {
        let rr = [0u8, 0x09, 0, 0, 0x21, 0, 0x31][ext as usize];
        for (n, &o) in args.iter().enumerate() {
            match o {
                Opnd::Reg(r) => {
                    let g = self.st.num_u32(&mut self.a, r);
                    if n == 0 {
                        self.a.mov_rr32(R::Rax, g);
                    } else {
                        self.a.alu32_rr(rr, R::Rax, g);
                    }
                    self.st.unpin_g();
                }
                Opnd::K(k) => {
                    let v = crate::lnumutils::luai_num2unsigned(self.k_num(k).unwrap());
                    if n == 0 {
                        self.a.mov_ri(R::Rax, v as u64);
                    } else {
                        self.a.alu32_ri(ext, R::Rax, v as i32);
                    }
                }
            }
        }
    }

    /// Inline expansion of builtin `bfid`; returns false (nothing emitted
    /// that changes state) when it is not handled.
    unsafe fn t_builtin(&mut self, bfid: u32, args: &[Opnd], dst: usize, nres: i32) -> bool {
        let is = |b: B| bfid == b as u32;
        // with no results wanted the interpreter still writes the result slot
        let value_result = nres <= 1;
        // tostring of a value whose type is known: a string is itself, nil is
        // the interned "nil", and with the result unused (and no object
        // whose __tostring could run) nothing needs to happen at all
        if is(B::Tostring) && args.len() == 1 && value_result && !dbg_off(131072) {
            if let Opnd::Reg(r) = args[0] {
                let t = self.st.tag[r];
                if self.result_dead && matches!(t, TNIL | TBOOL | TNUM | TINT | TSTR) {
                    self.ensure_safeenv(true);
                    return true;
                }
                if t == TSTR || t == TNIL {
                    self.ensure_safeenv(true);
                    if t == TSTR {
                        if dst != r {
                            self.st.kill(dst);
                            copy_tv(&mut self.a, R::R13, reg(dst as i32), R::R13, reg(r as i32));
                        }
                    } else {
                        self.st.kill(dst);
                        self.a.load(R::Rax, R::R12, OFF_GLOBAL);
                        self.a.load(R::Rax, R::Rax, OFF_TTNAME_NIL);
                        self.a.store(R::R13, reg(dst as i32), R::Rax);
                        self.a.mov_ri(R::Rax, (LUA_TSTRING as u64) << 32);
                        self.a.store(R::R13, reg(dst as i32) + TAGWORD, R::Rax);
                    }
                    self.st.tag[dst] = TSTR;
                    return true;
                }
            }
        }
        let all_num = |tr: &Self| {
            args.iter().all(|&o| match o {
                Opnd::Reg(r) => tr.can_be(r, TNUM),
                Opnd::K(k) => tr.k_tag(k) == TNUM,
            })
        };
        let guard_nums = |tr: &mut Self| {
            tr.ensure_safeenv(true);
            for &o in args {
                if let Opnd::Reg(r) = o {
                    tr.guard(r, TNUM, true);
                }
            }
        };

        // assert(v) with no results
        if is(B::Assert) {
            if nres != 0 || args.len() != 1 {
                return false;
            }
            let Opnd::Reg(r) = args[0] else { return false };
            match self.st.tag[r] {
                TNIL => return false,
                TBOOL => {
                    self.ensure_safeenv(true);
                    let s = self.slow(true);
                    self.a.cmp32_mi(R::R13, reg(r as i32), 0);
                    self.a.jcc(Cond::E, s);
                }
                T_ANY => {
                    self.ensure_safeenv(true);
                    let s = self.slow(true);
                    let truthy = self.a.label();
                    self.a.cmp32_mi(R::R13, reg(r as i32) + TT, LUA_TNIL);
                    self.a.jcc(Cond::E, s);
                    self.a.cmp32_mi(R::R13, reg(r as i32) + TT, LUA_TBOOLEAN);
                    self.a.jcc(Cond::NE, truthy);
                    self.a.cmp32_mi(R::R13, reg(r as i32), 0);
                    self.a.jcc(Cond::E, s);
                    self.a.bind(truthy);
                }
                _ => self.ensure_safeenv(true),
            }
            return true;
        }

        // table.insert(t, v) with no results: one direct call instead of the
        // FASTCALL step plus the fallback call sequence
        if is(B::TableInsert) {
            if nres != 0 || args.len() != 2 {
                return false;
            }
            let Opnd::Reg(rt) = args[0] else { return false };
            if !self.can_be(rt, TTAB) {
                return false;
            }
            self.ensure_safeenv(true);
            self.guard(rt, TTAB, true);
            if let Opnd::Reg(rv) = args[1] {
                self.st.flush(&mut self.a, rv);
            }
            self.sync();
            let s = self.slow(true);
            self.a.load(R::Rsi, R::R13, reg(rt as i32));
            self.a.cmp8_mi(R::Rsi, OFF_T_READONLY, 0);
            self.a.jcc(Cond::NE, s);
            self.a.mov_rr(R::Rdi, R::R12);
            match args[1] {
                Opnd::Reg(rv) => self.a.lea(R::Rdx, R::R13, reg(rv as i32)),
                Opnd::K(k) => self.a.lea(R::Rdx, R::R14, reg(k as i32)),
            }
            self.a.call_abs(jit_tinsert as usize);
            return true;
        }

        // select(i, ...) / select("#", ...) on the frame's varargs
        if is(B::SelectVararg) {
            if nres != 1 || args.len() != 1 {
                return false;
            }
            let Opnd::Reg(r) = args[0] else { return false };
            self.ensure_safeenv(true);
            self.st.flush(&mut self.a, r);
            let s = self.slow(true);
            let numparams = (*self.p).numparams as i32;
            let strcase = self.a.label();
            let done = self.a.label();
            // rax = number of varargs = (base - ci->func) / 16 - numparams - 1
            self.a.load(R::Rcx, R::R12, OFF_CI);
            self.a.load(R::Rcx, R::Rcx, OFF_CI_FUNC);
            self.a.mov_rr(R::Rax, R::R13);
            self.a.sub_rr(R::Rax, R::Rcx);
            self.a.shr_ri(R::Rax, 4);
            self.a.sub_ri(R::Rax, numparams + 1);
            self.a.cmp32_mi(R::R13, reg(r as i32) + TT, LUA_TNUMBER);
            self.a.jcc(Cond::NE, strcase);
            // number: i - 1 < n (unsigned) copies base[i - 1 - n]
            self.a.cvttsd2si_m(R::Rcx, R::R13, reg(r as i32));
            self.a.sub32_ri(R::Rcx, 1);
            self.a.cmp32_rr(R::Rcx, R::Rax);
            self.a.jcc(Cond::AE, s);
            self.a.mov_rr32(R::Rdx, R::Rcx);
            self.a.sub_rr(R::Rdx, R::Rax);
            self.a.shl_ri(R::Rdx, 4);
            self.a.add_rr(R::Rdx, R::R13);
            self.a.load(R::Rcx, R::Rdx, 0);
            self.a.store(R::R13, reg(dst as i32), R::Rcx);
            self.a.load(R::Rcx, R::Rdx, 8);
            self.a.store(R::R13, reg(dst as i32) + 8, R::Rcx);
            self.a.jmp(done);
            // "#": the count
            self.a.bind(strcase);
            self.a.cmp32_mi(R::R13, reg(r as i32) + TT, LUA_TSTRING);
            self.a.jcc(Cond::NE, s);
            self.a.load(R::Rdx, R::R13, reg(r as i32));
            self.a.cmp8_mi(R::Rdx, OFF_TS_DATA, b'#');
            self.a.jcc(Cond::NE, s);
            self.a.cvtsi2sd64(X(0), R::Rax);
            self.a.movsd_store(R::R13, reg(dst as i32), X(0));
            self.a.mov_ri(R::Rcx, NUM_TAG);
            self.a.store(R::R13, reg(dst as i32) + TAGWORD, R::Rcx);
            self.a.bind(done);
            self.st.kill(dst);
            return true;
        }

        // table.unpack(t) into a variable number of results
        if is(B::TableUnpack) {
            // table.unpack(t) or table.unpack(t, 1, n)
            if nres != -1 || (args.len() != 1 && args.len() != 3) {
                return false;
            }
            let Opnd::Reg(rt) = args[0] else { return false };
            if !self.can_be(rt, TTAB) {
                return false;
            }
            if args.len() == 3 {
                let numeric = |tr: &Self, o: Opnd| match o {
                    Opnd::Reg(r) => tr.can_be(r, TNUM),
                    Opnd::K(k) => tr.k_tag(k) == TNUM,
                };
                if !numeric(self, args[1]) || !numeric(self, args[2]) {
                    return false;
                }
            }
            self.ensure_safeenv(true);
            self.guard(rt, TTAB, true);
            if args.len() == 3 {
                for &o in &args[1..] {
                    if let Opnd::Reg(r) = o {
                        self.guard(r, TNUM, true);
                    }
                }
                // i must be exactly 1; n goes to the helper as an int
                let xi = self.opx(args[1], X(0));
                let s = self.slow(true);
                self.a.mov_ri(R::Rax, 1.0f64.to_bits());
                self.a.movq_xr(X(1), R::Rax);
                self.a.ucomisd(xi, X(1));
                self.a.jcc(Cond::NE, s);
                self.a.jcc(Cond::P, s);
                let xn = self.opx(args[2], X(0));
                self.a.cvttsd2si_x(R::Rcx, xn);
            }
            self.sync();
            // r8 = 1: unpack `#t` elements (rcx unused)
            self.a.mov_ri(R::R8, (args.len() == 1) as u64);
            let s = self.slow(true);
            self.a.load(R::Rsi, R::R13, reg(rt as i32));
            self.a.mov_rr(R::Rdi, R::R12);
            self.a.lea(R::Rdx, R::R13, reg(dst as i32));
            self.a.call_abs(jit_tunpack as usize);
            self.a.cmp32_ri(R::Rax, 0);
            self.a.jcc(Cond::L, s);
            // registers from dst up hold the unpacked values now
            self.st.forget();
            self.dyn_top = true;
            return true;
        }

        // buffer.writeinteger(b, offset, integer)
        if is(B::BufferWriteInteger) {
            if nres != 0 || args.len() != 3 {
                return false;
            }
            let (Opnd::Reg(rb), Opnd::Reg(roff), Opnd::Reg(rval)) = (args[0], args[1], args[2]) else { return false };
            if !self.can_be(rb, TBUF) || !self.can_be(roff, TNUM) || !self.can_be(rval, TINT) {
                return false;
            }
            self.ensure_safeenv(true);
            self.guard(rb, TBUF, true);
            self.guard(roff, TNUM, true);
            self.guard(rval, TINT, true);
            let b = self.st.ptr_g(&mut self.a, rb);
            let v = self.st.int_g(&mut self.a, rval);
            let base = self.buffer_addr(b, Opnd::Reg(roff), 8);
            self.a.store(base, BUF_DATA, v);
            return true;
        }

        // buffer writes
        if let Some((size, true, kind)) = buffer_op(bfid) {
            if nres != 0 || args.len() != 3 {
                return false;
            }
            let (Opnd::Reg(rb), Opnd::Reg(roff), Opnd::Reg(rval)) = (args[0], args[1], args[2]) else { return false };
            if !self.can_be(rb, TBUF) || !self.can_be(roff, TNUM) || !self.can_be(rval, TNUM) {
                return false;
            }
            self.ensure_safeenv(true);
            self.guard(rb, TBUF, true);
            self.guard(roff, TNUM, true);
            self.guard(rval, TNUM, true);
            let b = self.st.ptr_g(&mut self.a, rb);
            enum V {
                G(R),
                Xm(X),
            }
            let v = if kind >= 6 { V::Xm(self.st.num_x(&mut self.a, rval)) } else { V::G(self.st.num_u32(&mut self.a, rval)) };
            let base = self.buffer_addr(b, Opnd::Reg(roff), size);
            match v {
                V::Xm(x) if kind == 6 => {
                    self.a.cvtsd2ss(X(0), x);
                    self.a.movss_store(base, BUF_DATA, X(0));
                }
                V::Xm(x) => self.a.movsd_store(base, BUF_DATA, x),
                V::G(g) => self.a.store_sized(size, base, BUF_DATA, g),
            }
            return true;
        }

        if !value_result {
            return false;
        }

        // buffer reads
        if let Some((size, false, kind)) = buffer_op(bfid) {
            if args.len() != 2 {
                return false;
            }
            let Opnd::Reg(rb) = args[0] else { return false };
            let off_ok = match args[1] {
                Opnd::Reg(r) => self.can_be(r, TNUM),
                Opnd::K(k) => self.k_tag(k) == TNUM,
            };
            if !self.can_be(rb, TBUF) || !off_ok {
                return false;
            }
            self.ensure_safeenv(true);
            self.guard(rb, TBUF, true);
            if let Opnd::Reg(r) = args[1] {
                self.guard(r, TNUM, true);
            }
            let b = self.st.ptr_g(&mut self.a, rb);
            let base = self.buffer_addr(b, args[1], size);
            match kind {
                6 => {
                    self.a.cvtss2sd_m(X(0), base, BUF_DATA);
                    let xd = self.st.def_num(&mut self.a, dst);
                    self.a.movapd(xd, X(0));
                }
                7 => {
                    self.a.movsd_load(X(0), base, BUF_DATA);
                    let xd = self.st.def_num(&mut self.a, dst);
                    self.a.movapd(xd, X(0));
                }
                0 | 2 | 4 => {
                    self.a.load_ext(kind, R::Rax, base, BUF_DATA);
                    let gd = self.st.def_u32(&mut self.a, dst);
                    self.a.mov_rr32(gd, R::Rax);
                }
                k => {
                    self.a.load_ext(k, R::Rax, base, BUF_DATA);
                    let xd = self.st.def_num(&mut self.a, dst);
                    self.a.cvtsi2sd64(xd, R::Rax);
                }
            }
            return true;
        }

        // buffer.readinteger(b, offset)
        if is(B::BufferReadInteger) {
            if args.len() != 2 {
                return false;
            }
            let Opnd::Reg(rb) = args[0] else { return false };
            let off_ok = match args[1] {
                Opnd::Reg(r) => self.can_be(r, TNUM),
                Opnd::K(k) => self.k_tag(k) == TNUM,
            };
            if !self.can_be(rb, TBUF) || !off_ok {
                return false;
            }
            self.ensure_safeenv(true);
            self.guard(rb, TBUF, true);
            if let Opnd::Reg(r) = args[1] {
                self.guard(r, TNUM, true);
            }
            let b = self.st.ptr_g(&mut self.a, rb);
            let base = self.buffer_addr(b, args[1], 8);
            self.a.load(R::Rax, base, BUF_DATA);
            let gd = self.st.def_int(&mut self.a, dst);
            self.a.mov_rr(gd, R::Rax);
            return true;
        }

        // integer.create(number): exact integers only (anything else is nil)
        if is(B::IntegerCreate) {
            if args.len() != 1 || !all_num(self) {
                return false;
            }
            guard_nums(self);
            let xs = self.opx(args[0], X(0));
            let s = self.slow(true);
            self.a.cvttsd2si64_x(R::Rax, xs);
            self.a.cvtsi2sd64(X(1), R::Rax);
            self.a.ucomisd(X(1), xs);
            self.a.jcc(Cond::NE, s);
            self.a.jcc(Cond::P, s);
            let gd = self.st.def_int(&mut self.a, dst);
            self.a.mov_rr(gd, R::Rax);
            return true;
        }

        // integer.tonumber, comparisons, min/max
        let int_all = |tr: &Self| {
            args.iter().all(|&o| match o {
                Opnd::Reg(r) => tr.can_be(r, TINT),
                Opnd::K(k) => tr.k_tag(k) == TINT,
            })
        };
        let cmp_cond = if is(B::IntegerLt) {
            Some(Cond::L)
        } else if is(B::IntegerLe) {
            Some(Cond::LE)
        } else if is(B::IntegerGt) {
            Some(Cond::G)
        } else if is(B::IntegerGe) {
            Some(Cond::GE)
        } else if is(B::IntegerUlt) {
            Some(Cond::B)
        } else if is(B::IntegerUle) {
            Some(Cond::BE)
        } else if is(B::IntegerUgt) {
            Some(Cond::A)
        } else if is(B::IntegerUge) {
            Some(Cond::AE)
        } else {
            None
        };
        let minmax = is(B::IntegerMin) || is(B::IntegerMax);
        // (signed, remainder)
        let divkind = if is(B::IntegerDiv) {
            Some((true, false))
        } else if is(B::IntegerRem) {
            Some((true, true))
        } else if is(B::IntegerUDiv) {
            Some((false, false))
        } else if is(B::IntegerURem) {
            Some((false, true))
        } else {
            None
        };
        if is(B::IntegerToNumber) || ((cmp_cond.is_some() || minmax || divkind.is_some()) && args.len() == 2) {
            if (is(B::IntegerToNumber) && args.len() != 1) || !int_all(self) {
                return false;
            }
            self.ensure_safeenv(true);
            for &o in args {
                if let Opnd::Reg(r) = o {
                    self.guard(r, TINT, true);
                }
            }
            for (n, &o) in args.iter().enumerate() {
                let dstr = if n == 0 { R::Rax } else { R::Rcx };
                match o {
                    Opnd::Reg(r) => {
                        let g = self.st.int_g(&mut self.a, r);
                        self.a.mov_rr(dstr, g);
                        self.st.unpin_g();
                    }
                    Opnd::K(k) => {
                        let v = (*(*self.p).k.add(k)).value.l as u64;
                        self.a.mov_ri(dstr, v);
                    }
                }
            }
            if is(B::IntegerToNumber) {
                let xd = self.st.def_num(&mut self.a, dst);
                self.a.cvtsi2sd64(xd, R::Rax);
            } else if let Some((signed, rem)) = divkind {
                // division by zero (and MIN / -1) is left to the builtin
                let s = self.slow(true);
                let a = &mut self.a;
                let done = a.label();
                a.test_rr(R::Rcx, R::Rcx);
                a.jcc(Cond::E, s);
                if signed {
                    let normal = a.label();
                    a.cmp_ri(R::Rcx, -1);
                    a.jcc(Cond::NE, normal);
                    if rem {
                        a.mov_ri(R::Rax, 0);
                    } else {
                        a.mov_ri(R::Rdx, 1u64 << 63);
                        a.cmp_rr(R::Rax, R::Rdx);
                        a.jcc(Cond::E, s);
                        a.unary64(3, R::Rax);
                    }
                    a.jmp(done);
                    a.bind(normal);
                    a.cqo();
                    a.div64(7, R::Rcx);
                } else {
                    a.mov_ri(R::Rdx, 0);
                    a.div64(6, R::Rcx);
                }
                if rem {
                    a.mov_rr(R::Rax, R::Rdx);
                }
                a.bind(done);
                let gd = self.st.def_int(&mut self.a, dst);
                self.a.mov_rr(gd, R::Rax);
            } else if let Some(c) = cmp_cond {
                self.a.cmp_rr(R::Rax, R::Rcx);
                self.a.mov_ri(R::Rax, 0);
                self.a.mov_ri(R::Rdx, 1);
                self.a.cmov64(c, R::Rax, R::Rdx);
                self.st.kill(dst);
                self.a.store(R::R13, reg(dst as i32), R::Rax);
                self.a.mov_ri(R::Rax, BOOL_TAG);
                self.a.store(R::R13, reg(dst as i32) + TAGWORD, R::Rax);
                self.st.tag[dst] = TBOOL;
            } else {
                // min: b < a ? b : a; max: b > a ? b : a
                self.a.cmp_rr(R::Rcx, R::Rax);
                self.a.cmov64(if is(B::IntegerMin) { Cond::L } else { Cond::G }, R::Rax, R::Rcx);
                let gd = self.st.def_int(&mut self.a, dst);
                self.a.mov_rr(gd, R::Rax);
            }
            return true;
        }

        // integer library on LUA_TINTEGER operands
        if let Some(kind) = int_builtin(bfid) {
            let fold = (3..=5).contains(&kind);
            let arity_ok = if kind >= 10 { args.len() == 1 } else if fold { !args.is_empty() } else { args.len() == 2 };
            let types_ok = args.iter().all(|&o| match o {
                Opnd::Reg(r) => self.can_be(r, TINT),
                Opnd::K(k) => self.k_tag(k) == TINT,
            });
            if !arity_ok || !types_ok {
                return false;
            }
            self.ensure_safeenv(true);
            for &o in args {
                if let Opnd::Reg(r) = o {
                    self.guard(r, TINT, true);
                }
            }
            if args.len() == 2 && kind <= 9 && !dbg_off(65536) && self.t_int2(kind, args[0], args[1], dst) {
                return true;
            }
            for (n, &o) in args.iter().enumerate() {
                let dstr = if n == 0 { R::Rax } else { R::Rcx };
                match o {
                    Opnd::Reg(r) => {
                        let g = self.st.int_g(&mut self.a, r);
                        self.a.mov_rr(dstr, g);
                        self.st.unpin_g();
                    }
                    Opnd::K(k) => {
                        let v = (*(*self.p).k.add(k)).value.l as u64;
                        self.a.mov_ri(dstr, v);
                    }
                }
                if n > 0 {
                    emit_int_op(&mut self.a, kind);
                }
            }
            match kind {
                10 => self.a.unary64(2, R::Rax),
                11 => self.a.unary64(3, R::Rax),
                12 => self.a.bswap64(R::Rax),
                _ => {}
            }
            let gd = self.st.def_int(&mut self.a, dst);
            self.a.mov_rr(gd, R::Rax);
            return true;
        }

        // vector.create(x, y[, z])
        if is(B::Vector) {
            if !(args.len() == 2 || args.len() == 3) || !all_num(self) {
                return false;
            }
            guard_nums(self);
            if self.result_dead {
                return true;
            }
            let mut lane = |tr: &mut Self, o: Opnd, into: X| {
                tr.a.xorps(into, into);
                match o {
                    Opnd::Reg(r) => {
                        let x = tr.st.num_x(&mut tr.a, r);
                        tr.a.cvtsd2ss(into, x);
                    }
                    Opnd::K(k) => tr.a.cvtsd2ss_m(into, R::R14, reg(k as i32)),
                }
            };
            lane(self, args[0], X(0));
            lane(self, args[1], X(1));
            self.a.unpcklps(X(0), X(1));
            if args.len() == 3 {
                lane(self, args[2], X(1));
                self.a.movlhps(X(0), X(1));
            }
            let xd = self.st.def_vec(&mut self.a, dst);
            self.a.movaps(xd, X(0));
            return true;
        }

        // vector library on vector operands
        let vec1 = is(B::VectorMagnitude) || is(B::VectorNormalize) || is(B::VectorAbs) || is(B::VectorFloor) || is(B::VectorCeil);
        let vec2 = is(B::VectorDot) || is(B::VectorCross) || is(B::VectorMin) || is(B::VectorMax);
        if vec1 || vec2 {
            let want = if vec1 { 1 } else { 2 };
            if args.len() != want || ((is(B::VectorFloor) || is(B::VectorCeil)) && !self.sse41) {
                return false;
            }
            let mut regs = [0usize; 2];
            for (n, &o) in args.iter().enumerate() {
                let Opnd::Reg(r) = o else { return false };
                if !self.can_be(r, TVEC) {
                    return false;
                }
                regs[n] = r;
            }
            self.ensure_safeenv(true);
            for &r in &regs[..want] {
                self.guard(r, TVEC, true);
            }
            let xa = self.st.vec_x(&mut self.a, regs[0]);
            let xb = if vec2 { self.st.vec_x(&mut self.a, regs[1]) } else { xa };
            let a = &mut self.a;
            // xmm0 = x*x' + y*y' + z*z' of xmm0 (float, lane 0)
            let hsum = |a: &mut Asm| {
                a.movaps(X(1), X(0));
                a.shufps(X(1), X(1), 1);
                a.movaps(X(2), X(0));
                a.shufps(X(2), X(2), 2);
                a.ss_rr(0x58, X(0), X(1));
                a.ss_rr(0x58, X(0), X(2));
            };
            if is(B::VectorDot) || is(B::VectorMagnitude) {
                a.movaps(X(0), xa);
                a.ps_rr(0x59, X(0), xb);
                hsum(a);
                if is(B::VectorMagnitude) {
                    a.ss_rr(0x51, X(0), X(0));
                }
                let xd = self.st.def_num(&mut self.a, dst);
                self.a.cvtss2sd(xd, X(0));
            } else {
                if is(B::VectorNormalize) {
                    a.movaps(X(0), xa);
                    a.ps_rr(0x59, X(0), xa);
                    hsum(a);
                    a.ss_rr(0x51, X(0), X(0));
                    a.mov_ri(R::Rax, 1.0f32.to_bits() as u64);
                    a.movd_xr(X(1), R::Rax);
                    a.ss_rr(0x5e, X(1), X(0));
                    a.shufps(X(1), X(1), 0);
                    a.movaps(X(0), xa);
                    a.ps_rr(0x59, X(0), X(1));
                } else if is(B::VectorCross) {
                    // a.yzx * b.zxy - a.zxy * b.yzx
                    a.movaps(X(0), xa);
                    a.shufps(X(0), X(0), 0xc9);
                    a.movaps(X(1), xb);
                    a.shufps(X(1), X(1), 0xd2);
                    a.ps_rr(0x59, X(0), X(1));
                    a.movaps(X(1), xa);
                    a.shufps(X(1), X(1), 0xd2);
                    a.movaps(X(2), xb);
                    a.shufps(X(2), X(2), 0xc9);
                    a.ps_rr(0x59, X(1), X(2));
                    a.ps_rr(0x5c, X(0), X(1));
                } else if is(B::VectorAbs) {
                    a.mov_ri(R::Rax, 0x7fff_ffff);
                    a.movd_xr(X(0), R::Rax);
                    a.shufps(X(0), X(0), 0);
                    a.ps_op(0x54, X(0), xa);
                } else if is(B::VectorFloor) || is(B::VectorCeil) {
                    a.roundps(X(0), xa, if is(B::VectorFloor) { 9 } else { 10 });
                } else {
                    // min: b < a ? b : a per lane (minps keeps the second
                    // operand on ties and NaN)
                    a.movaps(X(0), xb);
                    a.ps_op(if is(B::VectorMin) { 0x5d } else { 0x5f }, X(0), xa);
                }
                let xd = self.st.def_vec(&mut self.a, dst);
                self.a.movaps(xd, X(0));
            }
            return true;
        }

        if !all_num(self) || args.is_empty() {
            return false;
        }

        // bit32
        let fold_ext = if is(B::Bit32Band) {
            Some(4u8)
        } else if is(B::Bit32Bor) {
            Some(1)
        } else if is(B::Bit32Bxor) {
            Some(6)
        } else {
            None
        };
        if let Some(ext) = fold_ext {
            guard_nums(self);
            self.fold_u32(ext, args);
            let gd = self.st.def_u32(&mut self.a, dst);
            self.a.mov_rr32(gd, R::Rax);
            return true;
        }
        if args.len() == 1 && (is(B::Bit32Bnot) || is(B::Bit32Byteswap) || is(B::Bit32Countlz) || is(B::Bit32Countrz)) {
            guard_nums(self);
            self.fold_u32(4, args);
            let a = &mut self.a;
            if is(B::Bit32Bnot) {
                a.not32(R::Rax);
            } else if is(B::Bit32Byteswap) {
                a.bswap32(R::Rax);
            } else {
                let zero = a.label();
                let done = a.label();
                a.test32_rr(R::Rax, R::Rax);
                a.jcc(Cond::E, zero);
                if is(B::Bit32Countlz) {
                    a.bitscan32(0xbd, R::Rax, R::Rax);
                    a.alu32_ri(6, R::Rax, 31);
                } else {
                    a.bitscan32(0xbc, R::Rax, R::Rax);
                }
                a.jmp(done);
                a.bind(zero);
                a.mov_ri(R::Rax, 32);
                a.bind(done);
            }
            let gd = self.st.def_u32(&mut self.a, dst);
            self.a.mov_rr32(gd, R::Rax);
            return true;
        }
        let shift_ext = if is(B::Bit32Lshift) {
            Some((4u8, true))
        } else if is(B::Bit32Rshift) {
            Some((5, true))
        } else if is(B::Bit32Arshift) {
            Some((7, true))
        } else if is(B::Bit32Lrotate) {
            Some((0, false))
        } else if is(B::Bit32Rrotate) {
            Some((1, false))
        } else {
            None
        };
        if let Some((ext, bounded)) = shift_ext {
            if args.len() != 2 {
                return false;
            }
            let kcount = match args[1] {
                Opnd::K(k) => {
                    let c = crate::lvmexecute::d2i_trunc(self.k_num(k).unwrap());
                    if bounded && !(0..32).contains(&c) {
                        return false;
                    }
                    Some((c & 31) as u8)
                }
                _ => None,
            };
            guard_nums(self);
            self.fold_u32(4, &args[..1]);
            match (kcount, args[1]) {
                (Some(c), _) => {
                    if c != 0 {
                        self.a.shift32_ri(ext, R::Rax, c);
                    }
                }
                (None, Opnd::Reg(rc)) => {
                    if self.st.has_exact_u32(rc) {
                        let g = self.st.num_u32(&mut self.a, rc);
                        self.a.mov_rr32(R::Rcx, g);
                    } else {
                        let xc = self.st.num_x(&mut self.a, rc);
                        self.a.cvttsd2si_x(R::Rcx, xc);
                    }
                    if bounded {
                        let s = self.slow(true);
                        self.a.cmp32_ri(R::Rcx, 32);
                        self.a.jcc(Cond::AE, s);
                    }
                    self.a.shift32_cl(ext, R::Rax);
                }
                _ => unreachable!(),
            }
            let gd = self.st.def_u32(&mut self.a, dst);
            self.a.mov_rr32(gd, R::Rax);
            return true;
        }
        if is(B::Bit32ExtractK) && args.len() == 2 {
            let Opnd::K(k) = args[1] else { return false };
            let fw = self.k_num(k).unwrap() as i32;
            let f = (fw & 31) as u8;
            let w1 = fw >> 5;
            let m = !(0xfffffffeu32.wrapping_shl(w1 as u32));
            guard_nums(self);
            self.fold_u32(4, &args[..1]);
            if f != 0 {
                self.a.shift32_ri(5, R::Rax, f);
            }
            self.a.alu32_ri(4, R::Rax, m as i32);
            let gd = self.st.def_u32(&mut self.a, dst);
            self.a.mov_rr32(gd, R::Rax);
            return true;
        }

        // math
        if args.len() == 1 {
            let round_mode = if is(B::MathFloor) {
                Some(9u8)
            } else if is(B::MathCeil) {
                Some(10)
            } else {
                None
            };
            if is(B::MathSqrt) || is(B::MathAbs) || (self.sse41 && (round_mode.is_some() || is(B::MathRound))) {
                guard_nums(self);
                let xs = self.opx(args[0], X(0));
                if is(B::MathSqrt) {
                    self.a.sqrtsd(X(0), xs);
                } else if is(B::MathAbs) {
                    self.a.mov_ri(R::Rax, u64::MAX >> 1);
                    self.a.movq_xr(X(1), R::Rax);
                    if xs != X(0) {
                        self.a.movapd(X(0), xs);
                    }
                    self.a.andpd(X(0), X(1));
                } else if let Some(mode) = round_mode {
                    self.a.roundsd(X(0), xs, mode);
                } else {
                    // trunc(x + copysign(0.49999999999999994, x))
                    self.a.mov_ri(R::Rax, 1u64 << 63);
                    self.a.movq_xr(X(1), R::Rax);
                    self.a.andpd(X(1), xs);
                    self.a.mov_ri(R::Rax, 0.49999999999999994f64.to_bits());
                    self.a.movq_xr(X(2), R::Rax);
                    self.a.orpd(X(1), X(2));
                    self.a.addsd(X(1), xs);
                    self.a.roundsd(X(0), X(1), 11);
                }
                let xd = self.st.def_num(&mut self.a, dst);
                self.a.movapd(xd, X(0));
                return true;
            }
            let f1: Option<usize> = if is(B::MathSin) {
                Some(jit_sin as usize)
            } else if is(B::MathCos) {
                Some(jit_cos as usize)
            } else if is(B::MathTan) {
                Some(jit_tan as usize)
            } else if is(B::MathAsin) {
                Some(jit_asin as usize)
            } else if is(B::MathAcos) {
                Some(jit_acos as usize)
            } else if is(B::MathAtan) {
                Some(jit_atan as usize)
            } else if is(B::MathExp) {
                Some(jit_exp as usize)
            } else if is(B::MathLog) {
                Some(jit_log as usize)
            } else if is(B::MathLog10) {
                Some(jit_log10 as usize)
            } else if is(B::MathSinh) {
                Some(jit_sinh as usize)
            } else if is(B::MathCosh) {
                Some(jit_cosh as usize)
            } else if is(B::MathTanh) {
                Some(jit_tanh as usize)
            } else {
                None
            };
            if let Some(f) = f1 {
                guard_nums(self);
                let xs = self.opx(args[0], X(0));
                if xs != X(0) {
                    self.a.movapd(X(0), xs);
                }
                self.sync();
                self.a.call_abs(f);
                let xd = self.st.def_num(&mut self.a, dst);
                self.a.movapd(xd, X(0));
                return true;
            }
            return false;
        }
        if args.len() == 2 {
            if is(B::MathMin) || is(B::MathMax) {
                guard_nums(self);
                let xa = self.opx(args[0], X(1));
                let xb = self.opx(args[1], X(0));
                // max: b > a ? b : a; min: b < a ? b : a (as the interpreter)
                if xb != X(0) {
                    self.a.movapd(X(0), xb);
                }
                self.a.sd_rr(if is(B::MathMax) { 0x5f } else { 0x5d }, X(0), xa);
                let xd = self.st.def_num(&mut self.a, dst);
                self.a.movapd(xd, X(0));
                return true;
            }
            let f2: Option<usize> = if is(B::MathPow) {
                Some(jit_pow as usize)
            } else if is(B::MathAtan2) {
                Some(jit_atan2 as usize)
            } else if is(B::MathFmod) {
                Some(jit_fmod as usize)
            } else {
                None
            };
            if let Some(f) = f2 {
                guard_nums(self);
                let xa = self.opx(args[0], X(0));
                if xa != X(0) {
                    self.a.movapd(X(0), xa);
                }
                let xb = self.opx(args[1], X(1));
                if xb != X(1) {
                    self.a.movapd(X(1), xb);
                }
                self.sync();
                self.a.call_abs(f);
                let xd = self.st.def_num(&mut self.a, dst);
                self.a.movapd(xd, X(0));
                return true;
            }
        }
        false
    }

    /// rdx = buffer `b` + offset for a `size`-byte access (bails on
    /// out-of-bounds offsets; the builtin then reports them).
    /// Bounds-checks a `size`-byte access at `off` of buffer `b`; the next
    /// memory operand (on the returned base) addresses it: `[base + BUF_DATA]`.
    unsafe fn buffer_addr(&mut self, b: R, off: Opnd, size: u8) -> R {
        let idx = match off {
            Opnd::Reg(r) => {
                if self.st.has_exact_u32(r) {
                    self.st.num_u32(&mut self.a, r)
                } else if !dbg_off(8192) {
                    self.st.trunc32(&mut self.a, r)
                } else {
                    let x = self.st.num_x(&mut self.a, r);
                    self.a.cvttsd2si_x(R::Rcx, x);
                    R::Rcx
                }
            }
            Opnd::K(k) => {
                let v = crate::lvmexecute::d2i_trunc(self.k_num(k).unwrap());
                self.a.mov_ri(R::Rcx, v as u32 as u64);
                R::Rcx
            }
        };
        let s = self.slow(true);
        self.a.lea(R::Rdx, idx, size as i32 - 1);
        self.a.load32(R::Rax, b, OFF_BUF_LEN);
        self.a.cmp_rr(R::Rdx, R::Rax);
        self.a.jcc(Cond::AE, s);
        if dbg_off(32768) {
            self.a.mov_rr(R::Rdx, b);
            self.a.add_rr(R::Rdx, idx);
            return R::Rdx;
        }
        self.a.set_index(idx);
        b
    }

    /// Calls builtin `bfid`'s fast function on the stacked arguments. When it
    /// declines (-1) this bails: the function is recompiled with the
    /// FASTCALL's native fallback call (the fallback code is not translated
    /// here, so continuing natively after a decline isn't possible).
    unsafe fn t_fastcall_generic(&mut self, op: u8, bfid: u32, aux: u32, args: &[Opnd], dst: usize, nres: usize) {
        for &o in args {
            if let Opnd::Reg(r) = o {
                self.st.flush(&mut self.a, r);
            }
        }
        self.ensure_safeenv(true);
        self.sync();
        let s = self.slow(true);
        let Opnd::Reg(rb) = args[0] else { unreachable!() };
        self.a.mov_rr(R::Rdi, R::R12);
        self.a.mov_ri(R::Rsi, bfid as u64);
        self.a.lea(R::Rdx, R::R13, reg(dst as i32));
        self.a.lea(R::Rcx, R::R13, reg(rb as i32));
        if op == O::FastCall3 as u8 {
            // the interpreter passes copies of arguments 2 and 3 at the top
            self.a.mov_ri(R::Rsi, (bfid as usize | (nres << 8)) as u64);
            self.a.lea(R::R8, R::R13, reg((aux & 0xff) as i32));
            self.a.lea(R::R9, R::R13, reg(((aux >> 8) & 0xff) as i32));
            self.a.call_abs(jit_fastcall3 as usize);
        } else {
            if op == O::FastCall1 as u8 {
                self.a.mov_ri(R::R8, 0);
            } else if op == O::FastCall2K as u8 {
                self.a.lea(R::R8, R::R14, reg(aux as i32));
            } else {
                self.a.lea(R::R8, R::R13, reg((aux & 0xff) as i32));
            }
            self.a.mov_ri(R::R9, (nres | (args.len() << 8)) as u64);
            self.a.call_abs(jit_fastcall as usize);
        }
        self.a.test_rr(R::Rax, R::Rax);
        self.a.jcc(Cond::L, s);
        // builtins write their result slots even when fewer results are
        // wanted; nothing is cached after `sync`, so this only drops what
        // was known about those slots' tags
        for r in dst..(dst + nres.max(1) + 2).min(256) {
            self.st.kill(r);
        }
    }

    /// FASTCALL*: expands the builtin inline when possible; returns the index
    /// of the instruction after the fallback CALL.
    unsafe fn t_fastcall(&mut self, op: u8, insn: u32, aux: u32, len: usize, top_in: Option<usize>) -> Option<(usize, Option<usize>)> {
        if self.feedback().is_some() || self.generic || dbg_off(2) {
            return None;
        }
        let i = self.i;
        let (rb, rc) = (b_of(insn) as usize, c_of(insn) as usize);
        let one = op == O::FastCall1 as u8;
        let variadic = op == O::FastCall as u8;
        let skip = if one || variadic { rc } else { rc.checked_sub(1)? };
        let call_i = i + len + skip;
        if call_i + 1 >= self.n || !self.starts[call_i] {
            return None;
        }
        let call = *self.code.add(call_i);
        if op_of(call) != O::Call as u8 {
            return None;
        }
        // the fallback sequence is dropped: nothing may jump into it
        if (i + len..=call_i).any(|q| self.jentry[q]) {
            return None;
        }
        let dst = a_of(call) as usize;
        let nres = c_of(call) - 1;
        let args: Vec<Opnd> = if variadic {
            // "all values up to the top": known when the last argument was
            // produced by a builtin expanded just before
            let nparams = if b_of(call) == 0 { top_in?.checked_sub(dst + 1)? as i32 } else { b_of(call) - 1 };
            if nparams < 1 || nparams > 8 {
                return None;
            }
            (1..=nparams as usize).map(|q| Opnd::Reg(dst + q)).collect()
        } else if one {
            vec![Opnd::Reg(rb)]
        } else if op == O::FastCall2 as u8 {
            vec![Opnd::Reg(rb), Opnd::Reg((aux & 0xff) as usize)]
        } else if op == O::FastCall2K as u8 {
            vec![Opnd::Reg(rb), Opnd::K(aux as usize)]
        } else {
            vec![Opnd::Reg(rb), Opnd::Reg((aux & 0xff) as usize), Opnd::Reg(((aux >> 8) & 0xff) as usize)]
        };
        self.dyn_top = false;
        self.result_dead = nres == 0 && call_i + 1 < self.n && !super::live::has(&self.live[call_i + 1], dst) && !self.is_volatile[dst];
        if !self.t_builtin(a_of(insn) as u32, &args, dst, nres) {
            // not expanded inline: call the builtin's fast function directly
            if variadic || nres < 0 || dbg_off(512) {
                return None;
            }
            self.t_fastcall_generic(op, a_of(insn) as u32, aux, &args, dst, nres as usize);
        }
        let mut top = None;
        if self.dyn_top {
            self.dyn_top = false;
        } else if nres == -1 {
            set_top(&mut self.a, dst as i32 + 1);
            top = Some(dst + 1);
        } else if variadic && b_of(call) == 0 {
            // the arguments ran "up to the top": put the top back at the
            // frame's limit (as the interpreter does), or the collector
            // would not see the registers above it
            self.a.load(R::Rax, R::R12, OFF_CI);
            self.a.load(R::Rax, R::Rax, OFF_CI_TOP);
            self.a.store(R::R12, OFF_TOP, R::Rax);
        }
        Some((call_i + 1, top))
    }

    // -- per-instruction bookkeeping ------------------------------------------------

    /// Completes the current instruction's slow paths: they rejoin at
    /// instruction `next` with the state reached here.
    fn finish_insn(&mut self, next: usize) {
        for k in 0..self.volatile.len() {
            let r = self.volatile[k] as usize;
            self.st.flush(&mut self.a, r);
            self.st.kill(r);
        }
        if self.site_start < self.sites.len() {
            let joins = next < self.n && !self.entry[next];
            let reload = if joins { self.st.cached() } else { Vec::new() };
            let verify: Vec<(u8, u8)> =
                if joins { self.learned.iter().copied().filter(|&(r, t)| self.st.tag[r as usize] == t).collect() } else { Vec::new() };
            let cont = if next < self.n { Some(self.labels[next]) } else { None };
            let safeenv = joins && self.st.safeenv;
            if self.sites[self.site_start..].iter().any(|s| !s.bail) {
                // the interpreter step on the slow path clobbers registers
                // no VM register owns
                self.st.drop_orphans();
            }
            for s in &mut self.sites[self.site_start..] {
                if !s.bail {
                    s.reload = reload.clone();
                    s.verify = verify.clone();
                    s.cont = cont;
                    s.next_i = next;
                    s.safeenv = safeenv;
                }
            }
        }
        self.site_start = self.sites.len();
        self.learned.clear();
        self.slow_cache = None;
    }
}

/// Pass 1: at the FORNLOOP `f` of a loop without other join points, notes
/// the live number/vector registers held in xmm registers.
unsafe fn record_carry(tr: &Tr, f: usize, insn: u32, live: &super::live::Mask, rec: &mut CarryPlan) {
    let h = (f as isize + 1 + d_of(insn) as isize) as usize;
    if h >= f || (h + 1..f).any(|j| tr.entry[j]) || carry_disabled(tr.p, h) {
        return;
    }
    let a = a_of(insn) as usize;
    let mut list = Vec::new();
    for r in 0..256 {
        if list.len() >= CARRY_MAX {
            break;
        }
        let t = tr.st.tag[r];
        if (t == TNUM || t == TVEC)
            && tr.st.x[r] != 0
            && super::live::has(live, r)
            && super::live::has(&tr.live[h], r)
            && !tr.is_volatile[r]
            && !(a..a + 3).contains(&r)
            && carry_seen(tr.p, h, r).is_none_or(|seen| seen == t)
        {
            list.push((r, t));
        }
    }
    if !list.is_empty() {
        rec.insert(h, (f, list));
    }
}

/// Common-subexpression key of GETUPVAL (arithmetic uses 0..=3).
const CSE_UPVAL: u8 = 100;

/// Debugging aid: `LUAU_CODEGEN_OFF` is a bit mask of translator features to
/// disable (1 typed loads, 2 inline builtins, 4 table templates, 8 arithmetic).
pub(super) fn dbg_off(bit: u32) -> bool {
    static MASK: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MASK.get_or_init(|| std::env::var("LUAU_CODEGEN_OFF").ok().and_then(|v| v.parse().ok()).unwrap_or(0)) & bit != 0
}

thread_local! {
    /// Offsets of all instructions of the function translated last (only
    /// kept when `LUAU_CODEGEN_DUMP` is set).
    pub static LAST_OFFSETS: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A builtin's fast function for native code: results at `res`; negative
/// when it declines. `counts` = nresults | nparams << 8.
unsafe extern "sysv64-unwind" fn jit_fastcall(L: *mut lua_State, bfid: u64, res: StkId, arg0: *mut TValue, args: StkId, counts: u64) -> i64 {
    let f = crate::lbuiltins::luauF_table[bfid as usize];
    f(L, res, arg0, (counts & 0xff) as i32, args, (counts >> 8) as i32) as i64
}

/// [`jit_fastcall`] with three arguments; `bn` = bfid | nresults << 8.
unsafe extern "sysv64-unwind" fn jit_fastcall3(L: *mut lua_State, bn: u64, res: StkId, arg0: *mut TValue, arg2: *const TValue, arg3: *const TValue) -> i64 {
    let f = crate::lbuiltins::luauF_table[(bn & 0xff) as usize];
    let top = (*L).top;
    setobj(top, arg2);
    setobj(top.add(1), arg3);
    f(L, res, arg0, (bn >> 8) as i32, top, 3) as i64
}

/// GETTABLEN / SETTABLEN through the generic VM paths (metamethods, any
/// object type); `cs` = index | set << 32; `pc` is the next instruction.
unsafe extern "sysv64-unwind" fn jit_tablen(L: *mut lua_State, obj: *const TValue, cs: u64, val: StkId, pc: *const u32) {
    (*(*L).ci).savedpc = pc;
    let mut n = TValue::NIL;
    setnvalue(&mut n, ((cs & 0xffff_ffff) + 1) as f64);
    if cs >> 32 != 0 {
        crate::lvmutils::luaV_settable(L, obj, &mut n, val);
        return;
    }
    if !dbg_off(16777216) {
        // fast cases: the table's own array slot, or the array slot of the
        // table that a userdata's / table's __index is (non-nil only: a nil
        // goes on to the full lookup, which also handles further metatables)
        let i = (cs & 0xffff_ffff) as usize;
        let mt = match (*obj).tt {
            LUA_TUSERDATA => (*uvalue(obj)).metatable,
            LUA_TTABLE => {
                let h = hvalue(obj);
                if i < (*h).sizearray as usize && !ttisnil((*h).array.add(i)) {
                    setobj(val, (*h).array.add(i));
                    return;
                }
                (*h).metatable
            }
            _ => std::ptr::null_mut(),
        };
        let tm = crate::ltm::fasttm(L, mt, crate::ltm::TM_INDEX);
        if !tm.is_null() && ttistable(tm) {
            let h = hvalue(tm);
            if i < (*h).sizearray as usize && !ttisnil((*h).array.add(i)) {
                setobj(val, (*h).array.add(i));
                return;
            }
        }
    }
    crate::lvmutils::luaV_gettable(L, obj, &mut n, val);
}

/// SETTABLE through the generic VM path; `pc` is the next instruction.
unsafe extern "sysv64-unwind" fn jit_settable(L: *mut lua_State, obj: *const TValue, key: *mut TValue, val: StkId, pc: *const u32) {
    (*(*L).ci).savedpc = pc;
    crate::lvmutils::luaV_settable(L, obj, key, val);
}

/// SETTABLEKS through the generic VM path; `pc` is the next instruction.
unsafe extern "sysv64-unwind" fn jit_settablek(L: *mut lua_State, obj: *const TValue, key: *mut TValue, val: StkId, pc: *const u32) {
    (*(*L).ci).savedpc = pc;
    crate::lvmutils::luaV_settable(L, obj, key, val);
}

/// CONCAT for native code; `abc` = a | b << 8 | c << 16, `pc` the next
/// instruction.
unsafe extern "sysv64-unwind" fn jit_concat(L: *mut lua_State, abc: u64, pc: *const u32) {
    let (a, b, c) = ((abc & 0xff) as usize, ((abc >> 8) & 0xff) as i32, ((abc >> 16) & 0xff) as i32);
    (*(*L).ci).savedpc = pc;
    crate::lvmutils::luaV_concat(L, c - b + 1, c);
    let base = (*L).base;
    setobj(base.add(a), base.add(b as usize));
    crate::lgc::luaC_checkGC(L);
}

/// PREPVARARGS for native code (as the interpreter: may grow the stack).
unsafe extern "sysv64-unwind" fn jit_prepvarargs(L: *mut lua_State, numparams: u64) {
    let numparams = numparams as usize;
    let ci = (*L).ci;
    let cl = clvalue((*ci).func);
    crate::ldo::luaD_checkstack(L, (*cl).stacksize as i32 + numparams as i32);
    let fixed = (*L).base;
    let base = (*L).top;
    for i in 0..numparams {
        setobj(base.add(i), fixed.add(i));
        setnilvalue(fixed.add(i));
    }
    let ci = (*L).ci;
    (*ci).base = base;
    (*ci).top = base.add((*cl).stacksize as usize);
    (*L).base = base;
    (*L).top = (*ci).top;
}

/// `#t` for native code (no allocation, never raises).
unsafe extern "sysv64" fn jit_getn(t: *mut LuaTable) -> u64 {
    crate::ltable::luaH_getn(t) as u32 as u64
}

/// `luaH_get` for native code (no allocation, never raises).
unsafe extern "sysv64" fn jit_luah_get(t: *mut LuaTable, key: *const TValue) -> *const TValue {
    crate::ltable::luaH_get(t, key)
}

/// `luauF_tinsert` for a non-readonly table: `t[#t + 1] = v`.
unsafe extern "sysv64" fn jit_tinsert(L: *mut lua_State, t: *mut LuaTable, v: *const TValue) {
    let pos = crate::ltable::luaH_getn(t) + 1;
    setobj(crate::ltable::luaH_setnum(L, t, pos), v);
    crate::lgc::luaC_barriert(L, t, v);
}

/// `luauF_tunpack`: copies `t[1..n]` (`#t` elements with `use_len`) from the array part
/// to `res` and sets `L->top`; -1 (nothing changed) when the fast path does
/// not apply.
unsafe extern "sysv64" fn jit_tunpack(L: *mut lua_State, t: *mut LuaTable, res: StkId, n: i32, use_len: i32) -> i32 {
    let n = if use_len != 0 { crate::ltable::luaH_getn(t) } else { n };
    if n < 0 || n > (*t).sizearray || ((*L).stack_last.offset_from(res) as i64) < n as i64 || n + 1 > crate::lobject::LUAI_MAXCSTACK {
        return -1;
    }
    let array = (*t).array;
    for i in 0..n as usize {
        setobj(res.add(i), array.add(i));
    }
    crate::ldo::expandstacklimit(L, res.add(n as usize));
    (*L).top = res.add(n as usize);
    n
}

macro_rules! thunk1 {
    ($($name:ident = $f:expr;)*) => {
        $(unsafe extern "sysv64" fn $name(x: f64) -> f64 { let f: fn(f64) -> f64 = $f; f(x) })*
    };
}
thunk1! {
    jit_sin = crate::libm::sin;
    jit_cos = crate::libm::cos;
    jit_tan = crate::libm::tan;
    jit_asin = crate::libm::asin;
    jit_acos = crate::libm::acos;
    jit_atan = crate::libm::atan;
    jit_exp = crate::libm::exp;
    jit_log = crate::libm::log;
    jit_log10 = crate::libm::log10;
    jit_sinh = crate::libm::sinh;
    jit_cosh = crate::libm::cosh;
    jit_tanh = crate::libm::tanh;
}
unsafe extern "sysv64" fn jit_pow(x: f64, y: f64) -> f64 {
    crate::lmathlib::luai_numpow(x, y)
}
unsafe extern "sysv64" fn jit_atan2(y: f64, x: f64) -> f64 {
    crate::libm::atan2(y, x)
}
unsafe extern "sysv64" fn jit_fmod(a: f64, b: f64) -> f64 {
    a % b
}

/// Translates `p`'s bytecode; returns the native code and per-instruction
/// table offsets (plus the dispatch-table fixup position as the last
/// element), or None if the function can't be compiled.
/// Loop-carried registers: loop head -> (its FORNLOOP, [(register, tag)]).
type CarryPlan = std::collections::HashMap<usize, (usize, Vec<(usize, u8)>)>;

/// Most loop-carried registers kept in xmm registers per loop.
const CARRY_MAX: usize = 6;

/// Translates `p`. A first pass records, for simple numeric for loops, the
/// number/vector registers held in xmm registers at the back edge; a second
/// pass keeps those in fixed xmm registers across iterations instead of
/// storing and reloading them every time (the first pass's code is used
/// when the second can't keep to the plan).
pub unsafe fn translate(p: *mut Proto, generic: bool, fb: &[(u32, u8, u8)]) -> Option<(Vec<u8>, Vec<Option<u32>>)> {
    let mut rec = CarryPlan::new();
    let mut abort = false;
    let first = translate_pass(p, generic, fb, &CarryPlan::new(), &mut rec, &mut abort)?;
    if rec.is_empty() || dbg_off(524288) {
        return Some(first);
    }
    if std::env::var_os("LUAU_DBG_CARRY").is_some() {
        eprintln!("carry plan: {:?}", rec);
    }
    let mut unused = CarryPlan::new();
    let mut abort2 = false;
    match translate_pass(p, generic, fb, &rec, &mut unused, &mut abort2) {
        Some(second) if !abort2 => Some(second),
        _ => Some(first),
    }
}

unsafe fn translate_pass(
    p: *mut Proto,
    generic: bool,
    fb: &[(u32, u8, u8)],
    plan: &CarryPlan,
    rec: &mut CarryPlan,
    abort: &mut bool,
) -> Option<(Vec<u8>, Vec<Option<u32>>)> {
    let code = (*p).code as *const u32;
    let n = (*p).sizecode as usize;
    let mut a = Asm::new();
    let cx = Ctx { ret: a.label(), dispatch: a.label(), interp: a.label(), exit_dyn: a.label() };
    let mut starts = vec![false; n];
    let mut i = 0;
    while i < n {
        starts[i] = true;
        let op = O::try_from(op_of(*code.add(i))).ok()?;
        i += get_op_length(op) as usize;
    }
    if i != n {
        return None;
    }
    let labels: Vec<Label> = (0..n).map(|_| a.label()).collect();

    // join points
    let mut entry = vec![false; n];
    let mut after_call = vec![false; n];
    entry[0] = true;
    let ce = ((*p).codeentry as usize - code as usize) / 4;
    if ce < n {
        entry[ce] = true;
    }
    let mut volatile = Vec::new();
    let mut is_volatile = [false; 256];
    let mut bad = false;
    let mut j = 0;
    while j < n {
        let insn = *code.add(j);
        let op = op_of(insn);
        let len = get_op_length(O::try_from(op).ok()?) as usize;
        let mut mark = |e: &mut Vec<bool>, t: isize| {
            if t >= 0 && (t as usize) < n && starts[t as usize] {
                e[t as usize] = true;
            } else {
                bad = true;
            }
        };
        let is = |o: O| op == o as u8;
        if is(O::LoadB) && c_of(insn) != 0 {
            mark(&mut entry, j as isize + 1 + c_of(insn) as isize);
        } else if is(O::JumpX) {
            mark(&mut entry, j as isize + 1 + ((insn as i32) >> 8) as isize);
        } else if is(O::Jump) || is(O::JumpBack) || is(O::JumpIf) || is(O::JumpIfNot) || is(O::JumpIfEq) || is(O::JumpIfLe)
            || is(O::JumpIfLt) || is(O::JumpIfNotEq) || is(O::JumpIfNotLe) || is(O::JumpIfNotLt) || is(O::JumpXEqKNil)
            || is(O::JumpXEqKB) || is(O::JumpXEqKN) || is(O::JumpXEqKS) || is(O::ForNPrep) || is(O::ForNLoop)
            || is(O::ForGPrep) || is(O::ForGPrepInext) || is(O::ForGPrepNext) || is(O::ForGLoop)
        {
            mark(&mut entry, j as isize + 1 + d_of(insn) as isize);
        }
        // resumption points: after calls, and loop instructions (interrupts)
        if is(O::Call) || is(O::CallFb) || is(O::FastPCall) || is(O::Namecall) {
            if j + len < n {
                after_call[j + len] = true;
            }
        }
        if is(O::ForNLoop) || is(O::JumpBack) || is(O::ForGLoop) {
            entry[j] = true;
        }
        // loop exits are reached from the loop instruction's slow path too
        // (and from trace exits when the tracing JIT is on)
        if (is(O::ForGLoop) || ((is(O::ForNLoop) || is(O::JumpBack)) && crate::jit::state::trace_enabled())) && j + len < n {
            entry[j + len] = true;
        }
        // registers captured by reference can be read and written by other
        // functions at any call: never cached, never typed
        if is(O::Capture) && a_of(insn) == 1 {
            let r = b_of(insn) as usize;
            if !is_volatile[r] {
                is_volatile[r] = true;
                volatile.push(r as u8);
            }
        }
        j += len;
    }
    if bad {
        return None;
    }

    // numeric for loops: constant steps, and index registers the body never
    // writes (the index is then a number throughout the body)
    let mut stepk: Vec<Option<f64>> = vec![None; n];
    let mut pins: Vec<(usize, usize, usize)> = Vec::new();
    let mut prev: Vec<usize> = vec![usize::MAX; n];
    let mut last = usize::MAX;
    for q in 0..n {
        if starts[q] {
            prev[q] = last;
            last = q;
        }
    }
    let mut j = 0;
    while j < n {
        let insn = *code.add(j);
        let op = op_of(insn);
        let len = get_op_length(O::try_from(op).ok()?) as usize;
        if op == O::ForNPrep as u8 {
            // the loads right before FORNPREP set limit, step and index
            let step_reg = a_of(insn) + 1;
            let mut q = prev[j];
            let mut seen = 0;
            let mut at = j;
            while q != usize::MAX && seen < 3 && !entry[at] && !after_call[at] {
                let bi = *code.add(q);
                let bop = op_of(bi);
                if bop == O::LoadN as u8 && a_of(bi) == step_reg {
                    stepk[j] = Some(d_of(bi) as f64);
                    break;
                }
                if !(bop == O::LoadN as u8 || bop == O::LoadK as u8 || bop == O::Move as u8) || a_of(bi) == step_reg {
                    break;
                }
                at = q;
                q = prev[q];
                seen += 1;
            }
        }
        if op == O::ForNLoop as u8 {
            let start = (j as isize + 1 + d_of(insn) as isize) as usize;
            let idx = a_of(insn) as usize + 2;
            let mut ok = start < j && !is_volatile[idx];
            let mut q = start;
            while ok && q < j {
                let bi = *code.add(q);
                let bop = op_of(bi);
                let blen = get_op_length(O::try_from(bop).ok()?) as usize;
                let ba = a_of(bi) as usize;
                let is = |o: O| bop == o as u8;
                let reads_a_only = is(O::SetTable) || is(O::SetTableKs) || is(O::SetTableN) || is(O::SetGlobal) || is(O::SetUpval)
                    || is(O::JumpIf) || is(O::JumpIfNot) || is(O::JumpIfEq) || is(O::JumpIfLe) || is(O::JumpIfLt)
                    || is(O::JumpIfNotEq) || is(O::JumpIfNotLe) || is(O::JumpIfNotLt) || is(O::JumpXEqKNil) || is(O::JumpXEqKB)
                    || is(O::JumpXEqKN) || is(O::JumpXEqKS) || is(O::Return) || is(O::Jump) || is(O::JumpBack) || is(O::JumpX)
                    || is(O::Nop) || is(O::Coverage);
                let multi = is(O::Call) || is(O::CallFb) || is(O::Namecall) || is(O::GetVarargs) || is(O::ForGLoop)
                    || is(O::ForNPrep) || is(O::ForNLoop) || is(O::ForGPrep) || is(O::ForGPrepInext) || is(O::ForGPrepNext)
                    || is(O::FastPCall) || is(O::NewClosure) || is(O::DupClosure);
                if multi {
                    if ba <= idx {
                        ok = false;
                    }
                } else if !reads_a_only && ba == idx {
                    ok = false;
                }
                q += blen;
            }
            if ok {
                pins.push((start, j, idx));
            }
        }
        j += len;
    }

    let live = match super::live::analyze(code, n, &starts, &volatile) {
        Some(l) if std::env::var_os("LUAU_CODEGEN_NOLIVE").is_none() => l,
        _ => vec![super::live::ALL; n],
    };
    let jentry = entry.clone();
    for q in 0..n {
        entry[q] |= after_call[q];
    }
    let mut tr = Tr {
        a,
        st: St::new(),
        carry_inner: std::collections::HashMap::new(),
        p,
        code,
        n,
        labels,
        starts,
        entry,
        jentry,
        resume_exit: vec![None; n],
        sites: Vec::new(),
        site_start: 0,
        slow_cache: None,
        dyn_top: false,
        result_dead: false,
        edges: Vec::new(),
        learned: Vec::new(),
        volatile,
        is_volatile,
        pins,
        stepk,
        cx,
        i: 0,
        live,
        sse41: std::arch::is_x86_feature_detected!("sse4.1"),
        avx: std::arch::is_x86_feature_detected!("avx") && !dbg_off(32),
        generic,
        fb,
    };
    // targets of backward jumps start loops: aligned for steady fetch
    let mut loop_heads = vec![false; n];
    if !dbg_off(128) {
        let mut j = 0;
        while j < n {
            let w = *code.add(j);
            let o = op_of(w);
            let d = (w as i32) >> 16;
            if [O::ForNLoop, O::ForGLoop, O::JumpBack, O::Jump].iter().any(|&x| x as u8 == o) && d < 0 {
                let t = (j as isize + 1 + d as isize) as usize;
                if t < n {
                    loop_heads[t] = true;
                }
            }
            j += get_op_length(O::try_from(o).ok()?) as usize;
        }
    }
    // instructions skipped because a builtin was expanded inline
    let mut skipped = vec![false; n];

    // L->top as left by a builtin expanded in the previous instruction
    let mut top_known: Option<usize> = None;
    let mut i = 0;
    while i < n {
        let insn = *code.add(i);
        let op = op_of(insn);
        let len = get_op_length(O::try_from(op).ok()?) as usize;
        tr.i = i;
        let top_in = if tr.entry[i] { None } else { top_known };
        top_known = None;
        // FORNLOOP of a loop whose registers are carried (pass 2)
        let carried_back = if tr.entry[i] && op == O::ForNLoop as u8 {
            let h = (i as isize + 1 + d_of(insn) as isize) as usize;
            plan.get(&h).filter(|e| e.0 == i).map(|e| (h, e.1.clone()))
        } else {
            None
        };
        let mut fl_inner = None;
        if let Some((_, list)) = &carried_back {
            // the back edge: carried registers into their fixed xmm
            // registers, everything else live to the stack; then past this
            // instruction's entry stub
            if list.iter().any(|&(r, t)| tr.st.tag[r] != t || tr.st.x[r] == 0) {
                *abort = true;
            }
            let mut moved = Vec::new();
            for (k, &(r, _)) in list.iter().enumerate() {
                if tr.st.x[r] != 0 && tr.st.x[r] != XS + k as u8 {
                    tr.st.flush(&mut tr.a, r);
                    moved.push(k);
                }
            }
            let mut m = tr.live[i];
            for &(r, _) in list {
                m[r >> 6] &= !(1u64 << (r & 63));
            }
            tr.st.flush_live(&mut tr.a, &m);
            for &k in &moved {
                let (r, t) = list[k];
                let x = X(XS + k as u8);
                if t == TVEC {
                    load_vec(&mut tr.a, x, X(2), R::R13, reg(r as i32));
                } else {
                    tr.a.movsd_load(x, R::R13, reg(r as i32));
                }
            }
            let l = tr.a.label();
            tr.a.jmp(l);
            fl_inner = Some(l);
            tr.st.forget();
        } else if tr.entry[i] {
            let live = tr.live[i];
            if op == O::ForNLoop as u8 && plan.is_empty() {
                record_carry(&tr, i, insn, &live, rec);
            }
            tr.st.reset(&mut tr.a, &live);
        }
        if loop_heads[i] && fl_inner.is_none() && !plan.contains_key(&i) {
            tr.a.align(32);
        }
        tr.a.bind(tr.labels[i]);
        tr.st.begin_insn();
        // entry stubs of carried loops (loop head, and FORNLOOP entered from
        // the interpreter): check and load the carried registers
        let stub = if let Some((_, list)) = &carried_back {
            Some(list.clone())
        } else {
            plan.get(&i).map(|e| e.1.clone())
        };
        if let Some(list) = stub {
            let head = carried_back.as_ref().map_or(i, |c| c.0);
            for (k, &(r, t)) in list.iter().enumerate() {
                tr.carry_guard(r, t, head);
                tr.st.attach_fixed(&mut tr.a, r, t, XS + k as u8, true);
            }
            tr.a.align(32);
            let inner = fl_inner.unwrap_or_else(|| tr.a.label());
            tr.a.bind(inner);
            if fl_inner.is_none() {
                tr.carry_inner.insert(i, inner);
            }
        }
        if !dbg_off(16384) {
            let live = tr.live[i];
            tr.st.release_dead(&live);
        }
        for k in 0..tr.pins.len() {
            let (ps, pe, r) = tr.pins[k];
            if i >= ps && i < pe && tr.st.tag[r] == T_ANY {
                tr.st.tag[r] = TNUM;
                tr.st.memtag[r] = true;
            }
        }
        let (ra, rb, rc) = (a_of(insn) as usize, b_of(insn) as usize, c_of(insn) as usize);
        let aux = if len > 1 { *code.add(i + 1) } else { 0 };
        let jt = (i as isize + 1 + d_of(insn) as isize) as usize;
        let is = |o: O| op == o as u8;
        let mut next = i + len;
        // false: run the instruction through its step function
        let mut done = true;

        if is(O::Nop) {
        } else if is(O::LoadNil) {
            tr.st.kill(ra);
            tr.a.store64_i(R::R13, reg(ra as i32) + TAGWORD, 0);
            tr.st.tag[ra] = TNIL;
        } else if is(O::LoadB) {
            tr.st.kill(ra);
            tr.a.store64_i(R::R13, reg(ra as i32), rb as i32);
            tr.a.mov_ri(R::Rax, BOOL_TAG);
            tr.a.store(R::R13, reg(ra as i32) + TAGWORD, R::Rax);
            tr.st.tag[ra] = TBOOL;
            if rc != 0 {
                tr.jmp_to(i + 1 + rc);
            }
        } else if is(O::LoadN) {
            let v = d_of(insn) as f64;
            let xd = tr.st.def_num(&mut tr.a, ra);
            if v == 0.0 {
                tr.a.xorpd(xd, xd);
            } else {
                tr.a.mov_ri(R::Rax, v.to_bits());
                tr.a.movq_xr(xd, R::Rax);
            }
        } else if is(O::LoadK) {
            let kd = (d_of(insn) as u32 & 0xffff) as usize;
            let kt = tr.k_tag(kd);
            if kt == TNUM {
                let xd = tr.st.def_num(&mut tr.a, ra);
                tr.a.movsd_load(xd, R::R14, reg(kd as i32));
            } else {
                tr.st.kill(ra);
                copy_tv(&mut tr.a, R::R13, reg(ra as i32), R::R14, reg(kd as i32));
                tr.st.tag[ra] = kt;
            }
        } else if is(O::Move) {
            if ra != rb {
                match tr.st.tag[rb] {
                    TNUM => {
                        let xs = tr.st.num_x(&mut tr.a, rb);
                        let xd = tr.st.def_num(&mut tr.a, ra);
                        tr.a.movapd(xd, xs);
                        tr.st.vid[ra] = tr.st.vid[rb];
                    }
                    TINT => {
                        let gs = tr.st.int_g(&mut tr.a, rb);
                        let gd = tr.st.def_int(&mut tr.a, ra);
                        tr.a.mov_rr(gd, gs);
                    }
                    TVEC => {
                        let xs = tr.st.vec_x(&mut tr.a, rb);
                        let xd = tr.st.def_vec(&mut tr.a, ra);
                        tr.a.movaps(xd, xs);
                    }
                    T_ANY if !tr.is_volatile[rb] && tr.spec(ra, i + 1).is_some() => {
                        // the copy is next used as a number/integer/vector:
                        // prove the source has that type and copy it typed
                        match tr.spec(ra, i + 1).unwrap() {
                            TNUM => {
                                tr.guard(rb, TNUM, true);
                                let xs = tr.st.num_x(&mut tr.a, rb);
                                let xd = tr.st.def_num(&mut tr.a, ra);
                                tr.a.movapd(xd, xs);
                            }
                            TINT => {
                                tr.guard(rb, TINT, true);
                                let gs = tr.st.int_g(&mut tr.a, rb);
                                let gd = tr.st.def_int(&mut tr.a, ra);
                                tr.a.mov_rr(gd, gs);
                            }
                            _ => {
                                tr.guard(rb, TVEC, true);
                                let xs = tr.st.vec_x(&mut tr.a, rb);
                                let xd = tr.st.def_vec(&mut tr.a, ra);
                                tr.a.movaps(xd, xs);
                            }
                        }
                    }
                    t => {
                        tr.st.kill(ra);
                        copy_tv(&mut tr.a, R::R13, reg(ra as i32), R::R13, reg(rb as i32));
                        tr.st.tag[ra] = t;
                    }
                }
            }
        } else if dbg_off(8) && [O::Add, O::Sub, O::Mul, O::Div, O::Mod, O::IDiv, O::Pow, O::AddK, O::SubK, O::MulK, O::DivK, O::ModK, O::IDivK, O::PowK, O::SubRk, O::DivRk, O::Minus].iter().any(|&o| is(o)) {
            done = false;
        } else if is(O::Add) || is(O::Sub) || is(O::Mul) || is(O::Div) || is(O::Mod) || is(O::IDiv) || is(O::Pow) {
            let kind = if is(O::Add) { 0 } else if is(O::Sub) { 1 } else if is(O::Mul) { 2 } else if is(O::Div) { 3 } else if is(O::Mod) { 4 } else if is(O::IDiv) { 5 } else { 6 };
            done = tr.t_arith(kind, ra, Opnd::Reg(rb), Opnd::Reg(rc));
        } else if is(O::AddK) || is(O::SubK) || is(O::MulK) || is(O::DivK) || is(O::ModK) || is(O::IDivK) || is(O::PowK) {
            let kind = if is(O::AddK) { 0 } else if is(O::SubK) { 1 } else if is(O::MulK) { 2 } else if is(O::DivK) { 3 } else if is(O::ModK) { 4 } else if is(O::IDivK) { 5 } else { 6 };
            done = tr.t_arith(kind, ra, Opnd::Reg(rb), Opnd::K(rc));
        } else if is(O::SubRk) || is(O::DivRk) {
            done = tr.t_arith(if is(O::SubRk) { 1 } else { 3 }, ra, Opnd::K(rb), Opnd::Reg(rc));
        } else if is(O::Minus) {
            done = tr.t_minus(ra, rb);
        } else if dbg_off(4) && (is(O::GetTable) || is(O::SetTable) || is(O::GetTableN) || is(O::SetTableN) || is(O::GetTableKs) || is(O::SetTableKs)) {
            done = false;
        } else if is(O::GetTable) {
            done = tr.t_gettable(ra, rb, rc);
        } else if is(O::SetTable) {
            done = tr.t_settable(ra, rb, rc);
        } else if is(O::GetTableN) {
            done = tr.t_tablen(false, ra, rb, rc as i32);
        } else if is(O::SetTableN) {
            done = tr.t_tablen(true, ra, rb, rc as i32);
        } else if is(O::GetTableKs) {
            done = tr.t_gettableks(ra, rb, aux);
        } else if is(O::SetTableKs) {
            done = tr.t_settableks(ra, rb, aux);
        } else if is(O::Concat) && !dbg_off(2097152) {
            // R(A) = R(B) .. ... .. R(C) through luaV_concat (metamethods,
            // GC step), without the interpreter step around it
            tr.flush_here();
            tr.sync();
            tr.a.mov_rr(R::Rdi, R::R12);
            tr.a.mov_ri(R::Rsi, (ra as u64) | ((rb as u64) << 8) | ((rc as u64) << 16));
            tr.a.mov_ri(R::Rdx, tr.pc(i) as u64 + 4);
            tr.a.call_abs(jit_concat as usize);
            tr.a.load(R::R13, R::R12, OFF_BASE);
            for r in rb..=rc {
                tr.st.kill(r);
            }
            // (any type: __concat may return anything)
            tr.st.kill(ra);
        } else if is(O::PrepVarargs) && !dbg_off(1048576) {
            // fixed parameters move above the varargs; the frame is rebased
            tr.sync();
            tr.a.mov_rr(R::Rdi, R::R12);
            tr.a.mov_ri(R::Rsi, ra as u64);
            tr.a.call_abs(jit_prepvarargs as usize);
            tr.a.load(R::R13, R::R12, OFF_BASE);
            tr.st.forget();
        } else if is(O::SetList) && !dbg_off(4096) {
            done = tr.t_setlist(ra, rb, rc, aux);
        } else if is(O::Length) && !dbg_off(2048) {
            done = tr.t_length(ra, rb);
        } else if is(O::GetUpval) && !dbg_off(8192) && tr.t_getupval_cse(ra, rb) {
        } else if is(O::GetUpval) {
            let up = OFF_UPREFS + reg(rb as i32);
            let fin = tr.a.label();
            // rdx = the value's address: the closure slot, or the UpVal's
            tr.a.lea(R::Rdx, R::R15, up);
            tr.a.cmp32_mi(R::R15, up + TT, LUA_TUPVAL);
            tr.a.jcc(Cond::NE, fin);
            tr.a.load(R::Rdx, R::R15, up);
            tr.a.load(R::Rdx, R::Rdx, OFF_UPVAL_V);
            tr.a.bind(fin);
            tr.load_tv(ra, R::Rdx, 0, i + 1);
            let v = tr.st.vid[ra];
            tr.st.cse_add(CSE_UPVAL, rb as u64, 0, v);
        } else if is(O::SetUpval) {
            tr.st.cse_remove_op(CSE_UPVAL);
            // upvalues written by SETUPVAL are always UpVal objects
            let up = OFF_UPREFS + reg(rb as i32);
            tr.a.load(R::Rdx, R::R15, up);
            tr.barrier(R::Rdx, OFF_UPVAL_MARKED, ra);
            tr.a.load(R::Rdx, R::Rdx, OFF_UPVAL_V);
            tr.copy_to_mem(R::Rdx, 0, ra);
        } else if is(O::GetGlobal) {
            let kv = (*p).k.add(aux as usize);
            if (*kv).tt != LUA_TSTRING {
                done = false;
            } else {
                let s = tr.slow(false);
                tr.a.load(R::Rax, R::R15, OFF_CL_ENV);
                let cslot = tr.pc(i) + 3;
                tr.node_at(R::Rax, cslot, (*kv).value.gc as usize, s);
                tr.load_tv(ra, R::Rcx, 0, i + 2);
            }
        } else if is(O::GetImport) {
            let kd = (insn >> 16) as usize;
            let kt = tr.k_tag(kd);
            if kt == TNIL {
                done = false;
            } else {
                // resolved at load time; valid while the environment is safe
                tr.ensure_safeenv(false);
                tr.st.kill(ra);
                copy_tv(&mut tr.a, R::R13, reg(ra as i32), R::R14, reg(kd as i32));
                tr.st.tag[ra] = kt;
            }
        } else if is(O::Not) {
            let t = tr.st.tag[rb];
            if t == T_ANY || t == TBOOL {
                let tl = tr.a.label();
                let fl = tr.a.label();
                let tagl = tr.a.label();
                let off = reg(rb as i32);
                if t == T_ANY {
                    tr.a.cmp32_mi(R::R13, off + TT, LUA_TNIL);
                    tr.a.jcc(Cond::E, tl);
                    tr.a.cmp32_mi(R::R13, off + TT, LUA_TBOOLEAN);
                    tr.a.jcc(Cond::NE, fl);
                }
                tr.a.cmp32_mi(R::R13, off, 0);
                tr.a.jcc(Cond::E, tl);
                tr.a.bind(fl);
                tr.a.store64_i(R::R13, reg(ra as i32), 0);
                tr.a.jmp(tagl);
                tr.a.bind(tl);
                tr.a.store64_i(R::R13, reg(ra as i32), 1);
                tr.a.bind(tagl);
            } else {
                tr.a.store64_i(R::R13, reg(ra as i32), (t == TNIL) as i32);
            }
            tr.st.kill(ra);
            tr.a.mov_ri(R::Rax, BOOL_TAG);
            tr.a.store(R::R13, reg(ra as i32) + TAGWORD, R::Rax);
            tr.st.tag[ra] = TBOOL;
        } else if is(O::JumpXEqKNil) || is(O::JumpXEqKB) || is(O::JumpXEqKN) || is(O::JumpXEqKS) {
            let kind = if is(O::JumpXEqKNil) { 0 } else if is(O::JumpXEqKB) { 1 } else if is(O::JumpXEqKN) { 2 } else { 3 };
            tr.t_jumpxeqk(kind, ra, aux, jt);
        } else if is(O::FastCall) || is(O::FastCall1) || is(O::FastCall2) || is(O::FastCall2K) || is(O::FastCall3) {
            match tr.t_fastcall(op, insn, aux, len, top_in) {
                Some((after, top)) => {
                    top_known = top;
                    for q in i + len..after {
                        skipped[q] = true;
                    }
                    if !tr.jentry[after] {
                        // only the interpreter can arrive here from elsewhere
                        // (the fallback call returned): send it back
                        tr.entry[after] = false;
                        tr.resume_exit[after] = Some(tr.a.label());
                    }
                    next = after;
                }
                None => done = false,
            }
        } else if (is(O::NewTable) || is(O::DupTable)) && i + len < n && !super::live::has(&tr.live[i + len], ra) && !tr.is_volatile[ra] {
            // the table is never read: allocating it has no visible effect
            // (C++ IR removes dead NEW_TABLE/DUP_TABLE the same way)
            tr.st.kill(ra);
        } else if is(O::ForNPrep) {
            done = tr.t_fornprep(ra, jt);
        } else if is(O::ForNLoop) {
            for r in ra..ra + 3 {
                tr.st.fresh(r);
                if !tr.is_volatile[r] {
                    tr.st.tag[r] = TNUM;
                }
            }
            tr.t_fornloop(ra, jt);
        } else if is(O::ForGLoop) {
            done = i + len < n && tr.t_forgloop(ra, aux as i32, jt, i + len);
        } else if is(O::JumpIfEq) || is(O::JumpIfNotEq) {
            tr.t_jumpeq(ra, aux as usize, jt, is(O::JumpIfEq));
        } else if is(O::Namecall) {
            let kv = (*p).k.add(aux as usize);
            if (*kv).tt != LUA_TSTRING {
                done = false;
            } else {
                // R(A+1) = R(B); R(A) = R(B)[K]: the table's own field, or the
                // field of its metatable's __index table at the slot cached in
                // the instruction (interpreter fast path); otherwise the step
                tr.sync();
                let s = tr.slow(false);
                let ts = (*kv).value.gc as *mut TString;
                let (ra_, rb_) = (ra as i32, rb as i32);
                let a = &mut tr.a;
                let found = a.label();
                let not_own = a.label();
                a.cmp32_mi(R::R13, reg(rb_) + TT, LUA_TTABLE);
                a.jcc(Cond::NE, s);
                a.load(R::Rax, R::R13, reg(rb_));
                // own field at the key's main position
                a.mov_ri(R::Rdx, (*ts).hash as u64);
                main_node(a, R::Rax);
                a.mov_ri(R::R9, ts as u64);
                node_has_key(a, R::R9, not_own);
                a.cmp32_mi(R::Rdx, TT, LUA_TNIL);
                a.jcc(Cond::E, s);
                a.jmp(found);
                a.bind(not_own);
                // absent from the table only if that node has no chain
                a.load32(R::R8, R::Rdx, NODE_KEYTT);
                a.cmp32_ri(R::R8, 16); // next == 0 <=> ttnext < 16
                a.jcc(Cond::AE, s);
                a.cmp32_ri(R::R8, 0);
                a.jcc(Cond::L, s);
                // metatable with an __index table
                a.load(R::R10, R::Rax, OFF_T_METATABLE);
                a.test_rr(R::R10, R::R10);
                a.jcc(Cond::E, s);
                a.test8_mi(R::R10, OFF_T_TMCACHE, 1 << crate::ltm::TM_INDEX);
                a.jcc(Cond::NE, s);
                a.load(R::R11, R::R12, OFF_GLOBAL);
                a.load(R::R11, R::R11, OFF_TMNAME_INDEX); // "__index"
                a.load32(R::Rdx, R::R11, OFF_TS_HASH);
                main_node(a, R::R10);
                node_has_key(a, R::R11, s);
                a.cmp32_mi(R::Rdx, TT, LUA_TTABLE);
                a.jcc(Cond::NE, s);
                a.load(R::R10, R::Rdx, 0); // the __index table
                // method at the cached slot (C byte, kept up to date)
                a.mov_ri(R::Rdx, code as u64 + i as u64 * 4 + 3);
                a.movzx8(R::Rcx, R::Rdx, 0);
                a.movzx8(R::Rdx, R::R10, OFF_T_NODEMASK8);
                a.alu32_rr(0x21, R::Rcx, R::Rdx);
                a.shl_ri(R::Rcx, NODE_SHIFT);
                a.load(R::Rdx, R::R10, OFF_T_NODE);
                a.add_rr(R::Rdx, R::Rcx);
                node_has_key(a, R::R9, s);
                a.cmp32_mi(R::Rdx, TT, LUA_TNIL);
                a.jcc(Cond::E, s);
                a.bind(found);
                copy_tv(a, R::R13, reg(ra_ + 1), R::R13, reg(rb_));
                copy_tv(a, R::R13, reg(ra_), R::Rdx, 0);
                tr.st.kill(ra);
                tr.st.kill(ra + 1);
            }
        } else if is(O::Jump) {
            tr.jmp_to(jt);
        } else if is(O::JumpX) {
            tr.jmp_to((i as isize + 1 + ((insn as i32) >> 8) as isize) as usize);
        } else if is(O::JumpBack) {
            tr.flush_here();
            let s = tr.slow(false);
            emit_loop_checks(&mut tr.a, s);
            tr.trace_counter();
            let tl = tr.labels[jt];
            tr.a.jmp(tl);
        } else if is(O::JumpIf) || is(O::JumpIfNot) {
            tr.t_jumpif(ra, jt, is(O::JumpIf));
        } else if is(O::JumpIfLt) || is(O::JumpIfLe) || is(O::JumpIfNotLt) || is(O::JumpIfNotLe) {
            let c = if is(O::JumpIfLt) { 0 } else if is(O::JumpIfLe) { 1 } else if is(O::JumpIfNotLt) { 2 } else { 3 };
            done = tr.t_jumpcmp(c, ra, aux as usize, jt);
        } else if is(O::Call) || is(O::CallFb) {
            // arguments (and everything below them) must be on the stack;
            // registers above the arguments are dead
            tr.flush_here();
            tr.st.forget();
            // argument count: fixed, or known from the builtin just expanded
            let nargs1 = if rb != 0 { Some(rb) } else { top_in.and_then(|t| t.checked_sub(ra)) };
            let cpath = (i + len < tr.n && !dbg_off(16)).then(|| tr.a.label());
            let mut cchecked = None;
            if let Some(b) = nargs1.filter(|&b| b >= 1 && b <= 255) {
                let fbl = tr.a.label();
                let npc = tr.pc(i + len);
                emit_inline_call(&mut tr.a, (insn & !0x00ff_0000) | ((b as u32) << 16), npc, fbl, cpath);
                cchecked = cpath;
                tr.a.bind(fbl);
            }
            let pc = tr.pc(i);
            if cpath.is_some() {
                let slow = tr.a.label();
                emit_c_call(&mut tr.a, &tr.cx, insn, pc, tr.labels[i + len], slow, cchecked);
                tr.a.bind(slow);
            }
            emit_frame_op(&mut tr.a, &tr.cx, pc, op, native_call as usize);
        } else if is(O::Return) {
            // only the returned values are still needed
            tr.flush_here();
            tr.st.forget();
            let nvals1 = if rb != 0 { Some(rb) } else { top_in.and_then(|t| t.checked_sub(ra)).map(|v| v + 1) };
            if let Some(b) = nvals1.filter(|&b| b >= 1 && b <= 255) {
                let fbl = tr.a.label();
                emit_inline_return(&mut tr.a, (insn & !0x00ff_0000) | ((b as u32) << 16), fbl);
                tr.a.bind(fbl);
            }
            let pc = tr.pc(i);
            emit_frame_op(&mut tr.a, &tr.cx, pc, op, native_return as usize);
        } else {
            done = false;
        }

        if !done {
            tr.step(len);
        }
        tr.finish_insn(next);
        i = next;
    }
    // falling off the end never happens in valid bytecode
    tr.a.jmp(tr.cx.ret);

    // conditional-jump edges that have values to store first
    let edges = std::mem::take(&mut tr.edges);
    for e in edges {
        tr.a.bind(e.label);
        emit_stores(&mut tr.a, &e.snap);
        tr.a.jmp(e.target);
    }

    // out-of-line slow paths
    let sites = std::mem::take(&mut tr.sites);
    for s in sites {
        let a = &mut tr.a;
        a.bind(s.label);
        emit_stores(a, &s.snap);
        let insn = *code.add(s.i);
        let op = op_of(insn);
        let step = STEPS[op as usize]? as usize;
        let pc = code as usize + s.i * 4;
        if s.hook != 0 {
            // rax = 0: left the VM; 1: interpret the frame on top; else the
            // pc to continue at in this function
            emit_args(a, pc);
            a.mov_ri(R::R8, s.hook as u64);
            a.call_abs(crate::jit::native_loop_hook as usize);
            a.test_rr(R::Rax, R::Rax);
            a.jcc(Cond::E, tr.cx.ret);
            a.load(R::R13, R::R12, OFF_BASE);
            a.cmp_ri(R::Rax, 1);
            a.jcc(Cond::E, tr.cx.interp);
            a.jmp(tr.cx.dispatch);
            continue;
        }
        if s.bail {
            // record what was seen (the function is recompiled for it), run
            // the instruction, and stay in the interpreter
            a.mov_ri(R::Rdi, p as u64);
            a.mov_ri(R::Rsi, s.carry_fail.unwrap_or(s.i) as u64);
            a.mov_rr(R::Rdx, R::R12);
            a.call_abs(if s.carry_fail.is_some() { on_carry_fail as usize } else { on_bail as usize });
            emit_args(a, pc);
            a.call_abs(step);
            a.test_rr(R::Rax, R::Rax);
            a.jcc(Cond::E, tr.cx.ret);
            a.load(R::R13, R::R12, OFF_BASE);
            a.jmp(tr.cx.exit_dyn);
            continue;
        }
        emit_args(a, pc);
        a.call_abs(step);
        a.test_rr(R::Rax, R::Rax);
        a.jcc(Cond::E, tr.cx.ret);
        a.load(R::R13, R::R12, OFF_BASE);
        match s.cont {
            Some(cont) => {
                a.mov_ri(R::Rcx, (code as usize + s.next_i * 4) as u64);
                a.cmp_rr(R::Rax, R::Rcx);
                a.jcc(Cond::NE, tr.cx.dispatch);
                // facts the fast path proved with guards may not hold here
                for &(r, t) in &s.verify {
                    a.cmp32_mi(R::R13, reg(r as i32) + TT, t as i32);
                    a.jcc(Cond::NE, tr.cx.exit_dyn);
                }
                if s.safeenv {
                    a.load(R::Rcx, R::R15, OFF_CL_ENV);
                    a.cmp8_mi(R::Rcx, OFF_T_SAFEENV, 0);
                    a.jcc(Cond::E, tr.cx.exit_dyn);
                }
                for &v in &s.reload {
                    emit_reload(a, v);
                }
                a.jmp(cont);
            }
            None => a.jmp(tr.cx.dispatch),
        }
    }

    // interpreter-only resume points
    for q in 0..n {
        if let Some(l) = tr.resume_exit[q] {
            tr.a.bind(l);
            tr.a.mov_ri(R::Rax, (code as usize + q * 4) as u64);
            tr.a.jmp(tr.cx.exit_dyn);
        }
    }

    let a = &mut tr.a;
    // rax = pc: continue in the interpreter there
    a.bind(tr.cx.exit_dyn);
    a.mov_rr(R::Rsi, R::Rax);
    a.mov_rr(R::Rdi, R::R12);
    a.mov_rr(R::Rdx, R::R13);
    a.mov_rr(R::Rcx, R::R14);
    a.movzx8(R::Rax, R::Rsi, 0);
    a.shl_ri(R::Rax, 3);
    a.mov_ri(R::R8, handlers_table() as u64);
    a.add_rr(R::Rax, R::R8);
    a.load(R::Rax, R::Rax, 0);
    emit_epilogue(a);
    a.jmp_r(R::Rax);

    // a step left the VM (yield, break, error return): return like the
    // interpreter does
    a.bind(tr.cx.ret);
    emit_epilogue(a);
    a.ret();

    // continue in the interpreter: the frame on top at its savedpc
    a.bind(tr.cx.interp);
    a.mov_rr(R::Rdi, R::R12);
    a.load(R::Rcx, R::R12, OFF_CI);
    a.load(R::Rsi, R::Rcx, OFF_CI_SAVEDPC);
    a.load(R::Rdx, R::R12, OFF_BASE);
    a.load(R::Rcx, R::Rcx, OFF_CI_P);
    a.load(R::Rcx, R::Rcx, OFF_P_K);
    a.movzx8(R::Rax, R::Rsi, 0);
    a.shl_ri(R::Rax, 3);
    a.mov_ri(R::R8, handlers_table() as u64);
    a.add_rr(R::Rax, R::R8);
    a.load(R::Rax, R::Rax, 0);
    emit_epilogue(a);
    a.jmp_r(R::Rax);

    // rax = pc: jump to that instruction's native code, or back to the
    // interpreter when it has no entry there
    a.bind(tr.cx.dispatch);
    a.mov_ri(R::Rcx, code as u64);
    a.mov_rr(R::Rdx, R::Rax);
    a.sub_rr(R::Rdx, R::Rcx);
    a.shr_ri(R::Rdx, 2);
    let table_fixup = a.pos() as usize + 2; // imm64 of the next mov
    a.mov_ri64(R::Rcx, u64::MAX); // patched with the table address
    a.load_index8(R::Rcx, R::Rcx, R::Rdx);
    a.test_rr(R::Rcx, R::Rcx);
    a.jcc(Cond::E, tr.cx.exit_dyn);
    a.jmp_r(R::Rcx);

    if !tr.a.finish() {
        return None;
    }
    let mut offs = vec![None; n];
    for q in 0..n {
        if let Some(l) = tr.resume_exit[q] {
            offs[q] = Some(tr.a.label_offset(l));
        } else if tr.entry[q] && !skipped[q] {
            offs[q] = Some(tr.a.label_offset(tr.labels[q]));
        }
    }
    offs.push(Some(table_fixup as u32));
    if std::env::var_os("LUAU_CODEGEN_DUMP").is_some() {
        // every translated instruction's offset, for profiling
        let all: Vec<u32> = (0..n).map(|q| if tr.starts[q] && !skipped[q] && tr.a.is_bound(tr.labels[q]) { tr.a.label_offset(tr.labels[q]) } else { u32::MAX }).collect();
        LAST_OFFSETS.with(|c| *c.borrow_mut() = all);
    }
    Some((tr.a.code, offs))
}

/// 1/v when v is a power of two whose reciprocal is an exact normal double.
fn exact_reciprocal(v: f64) -> Option<f64> {
    let b = v.to_bits();
    let exp = (b >> 52) & 0x7ff;
    if b & ((1u64 << 52) - 1) == 0 && (1..=2045).contains(&exp) {
        Some(f64::from_bits((b & (1u64 << 63)) | ((2046 - exp) << 52)))
    } else {
        None
    }
}
