//! The trace recorder.
//!
//! When a loop gets hot, the interpreter keeps running it normally but calls
//! `step` before executing each instruction. The recorder turns that
//! instruction into IR using the *current* types of its operands (guarded,
//! so the compiled trace exits when they change), and follows whatever path
//! the interpreter actually takes. Arriving back at the loop header closes
//! the trace: `close` restructures it into
//!
//! ```text
//!   constants, SLoads              (run once, at trace entry)
//!   LOOP
//!   PHI slot values carried around the loop
//!   body                            (guards exit through snapshots)
//!   -> back to after LOOP
//! ```
//!
//! Anything the recorder doesn't model aborts the recording; the
//! interpreter is unaffected either way (it executes every instruction
//! itself; the recorder only watches).

use super::ir::builder::IrBuilder;
use super::ir::instruction::{Cmp, Inst, Op, Ref, SnapId};
use super::ir::types::IrType;
use super::snapshots::{SnapEntry, SnapFrame, Snapshot};
use super::trace::{AbortReason, Trace, TraceId};
use crate::lobject::*;
use luau_common::bytecode::{get_op_length, Opcode as O};
use std::collections::{BTreeMap, HashMap};

pub enum Step {
    Continue,
    /// the loop closed: a finished (not yet optimized) trace
    Closed(Trace),
    Abort(AbortReason),
}

pub struct Recorder {
    b: IrBuilder,
    proto: *mut Proto,
    start_pc: u32,
    /// last back edge in the proto whose target is `start_pc`: the loop body
    /// is [start_pc, loop_end]; recording stops if the interpreter leaves it
    loop_end: u32,
    id: TraceId,
    steps: usize,
    max_len: usize,
    /// the trace frame's base when recording started (inlined frames are
    /// at fixed offsets from it)
    root_base: StkId,
    /// (proto, closure now, the closure as an IR value once needed) of the
    /// root frame and every inlined frame: a trace runs for any closure of
    /// its prototypes, so upvalues and environments are read from the
    /// closure at run time
    funcs: Vec<(*mut Proto, *mut Closure, Option<Ref>)>,
    /// most stack slots any recorded frame needs (from the root base)
    stack_need: usize,
    gc_refs: Vec<(usize, u8)>,
    /// inlined prototypes (the trace is dropped when one is freed)
    protos: Vec<usize>,
    /// side trace: the loop header it ends at (its root's), and the parent
    /// exit it continues
    side: Option<(u32, (u16, u32))>,
    /// a builtin call just recorded: (its instruction if it has a result,
    /// the result's trace slot, the pc the interpreter must be at next). Its
    /// type is the one the interpreter produced.
    pending: Option<(Option<Ref>, usize, u32, bool)>,
    /// the thread being recorded
    l: *mut lua_State,
    /// the stack top (trace slot) after a call returning a variable number
    /// of values, until the instruction taking them
    top: Option<usize>,
    /// iterations of inner loops recorded so far
    unrolled: usize,
    /// a key may be added to a table (see Trace::inserts)
    inserts: bool,
    /// a leaf C call just recorded: (first result's frame slot, results,
    /// the pc the interpreter must be at next)
    pending_c: Option<(usize, usize, u32)>,
    /// the trace stores to upvalues (their loads aren't loop-invariant)
    ustores: bool,
    /// metamethods being recorded (innermost last): (frame depth, the
    /// result's trace slot - CMP_RESULT for a comparison, whose result's
    /// truthiness is guarded instead - , the snapshot resuming at the
    /// instruction)
    mm: Vec<(usize, usize, SnapId)>,
    /// tables created inside metamethods (the only ones they may change:
    /// an exit there re-runs the metamethod)
    fresh: Vec<Ref>,
    /// the trace's own frame's vararg count, when the trace reads them
    /// (checked at entry)
    varargs: Option<u16>,
    /// a call of a C function that may run Lua code was just recorded:
    /// (its frame slot, results wanted, the pc after it, the CallInfo depth
    /// of its caller - deeper instructions are the callee's, not recorded
    /// - , the result count it defines)
    callback: Option<(usize, i32, u32, isize, Ref)>,
    /// a FASTCALL just recorded: (its Builtin instruction, the result slot
    /// (frame-relative) and the value it had) - undone when the builtin
    /// declines while recording (the fallback CALL is recorded instead)
    builtin_undo: Option<(Ref, Option<(usize, Option<Ref>)>)>,
    /// that FASTCALL's call: (builtin id, the CALL's frame slot, its
    /// arguments) - for a builtin returning several values, recorded as
    /// the call of its function instead
    builtin_call: Option<(u8, usize, [Ref; 3], usize)>,
    /// a FASTCALL recorded as the call of its function once it's known
    /// the builtin ran (else its fallback code is recorded): (builtin, the
    /// CALL's slot, arguments, results wanted, the pc after the CALL)
    deferred_call: Option<((u8, usize, [Ref; 3], usize), i32, u32)>,
}

/// `Snapshot::top` of an exit that keeps the L->top the trace left (after
/// a call returning a variable number of results).
pub const KEEP_TOP: u16 = u16::MAX;

/// `Recorder::mm` result slot of a comparison metamethod.
const CMP_RESULT: usize = usize::MAX;

/// `Op::Builtin::nres` for a call taking every result (the trace checks
/// there is exactly one).
pub const MULTRET1: u8 = 0xff;

/// Iterations of inner loops one trace may unroll (longer inner loops get
/// traces of their own; the outer loop then stays native).
const UNROLL_LIMIT: usize = 64;

/// Deepest call inlining.
const MAX_INLINE_DEPTH: usize = 6;

/// Stack slots (from the trace's base) an open upvalue read by the trace
/// must not point into (covers every inlined frame: see `stack_need`).
const STACK_WINDOW: usize = 1024;

#[inline]
fn op_of(i: u32) -> u8 {
    (i & 0xff) as u8
}
#[inline]
fn a_of(i: u32) -> usize {
    ((i >> 8) & 0xff) as usize
}
#[inline]
fn b_of(i: u32) -> usize {
    ((i >> 16) & 0xff) as usize
}
#[inline]
fn c_of(i: u32) -> usize {
    (i >> 24) as usize
}
#[inline]
fn d_of(i: u32) -> i32 {
    (i as i32) >> 16
}

type R<T> = Result<T, AbortReason>;

fn unsupported_type(what: &'static str) -> AbortReason {
    AbortReason::UnsupportedType(what)
}

impl Recorder {
    /// Starts recording the loop whose header is instruction `start_pc` of
    /// `proto`; `base` is the frame's stack base right now.
    /// `cl` is the running closure (not always at `base - 1`: a vararg
    /// function's base is above its varargs).
    pub unsafe fn new(id: TraceId, proto: *mut Proto, cl: *mut Closure, start_pc: u32, base: StkId, max_len: usize) -> Recorder {
        let nslots = (*proto).maxstacksize as usize;
        let loop_end = find_loop_end(proto, start_pc);
        Recorder {
            b: IrBuilder::new(Trace::new(id, proto, start_pc), nslots, start_pc),
            proto,
            start_pc,
            loop_end,
            id,
            steps: 0,
            max_len,
            root_base: base,
            funcs: vec![(proto, cl, None)],
            stack_need: nslots,
            gc_refs: Vec::new(),
            protos: Vec::new(),
            side: None,
            pending: None,
            l: std::ptr::null_mut(),
            top: None,
            unrolled: 0,
            inserts: false,
            pending_c: None,
            ustores: false,
            mm: Vec::new(),
            fresh: Vec::new(),
            varargs: None,
            callback: None,
            builtin_undo: None,
            builtin_call: None,
            deferred_call: None,
        }
    }

    /// Starts recording a side trace at `start_pc` (where exit `parent` of
    /// a trace of the loop at `header` resumes), up to the loop header.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn new_side(
        id: TraceId,
        proto: *mut Proto,
        cl: *mut Closure,
        start_pc: u32,
        base: StkId,
        max_len: usize,
        header: u32,
        loop_end: u32,
        parent: (u16, u32),
        frames: Vec<SnapFrame>,
        top: Option<(usize, bool)>,
    ) -> Recorder {
        let mut r = Recorder::new(id, proto, cl, start_pc, base, max_len);
        r.loop_end = loop_end;
        r.side = Some((header, parent));
        // starting inside inlined calls: their frames are the parent's
        // (virtual: no CallInfos while the trace runs)
        if let Some(last) = frames.last() {
            r.b.off = last.base as usize;
        }
        r.b.frames = frames.clone();
        // (after a call with a variable number of results, L->top is where
        // that call left it: checked, and kept by exits at the start)
        let keep = top.is_some_and(|t| t.1);
        r.set_top(if keep { Some(KEEP_TOP as usize) } else { top.map(|t| t.0) });
        // snapshot 0: the start, nothing done (the entry checks exit here)
        let s0 = r.b.snapshot();
        if let (Some((t, true)), _) = (top, s0) {
            r.b.emit(Op::GuardTop { top: t as u16, snap: s0 }, IrType::Nil);
            r.set_top(Some(t));
        }
        let off = r.b.off;
        r.b.off = 0;
        for f in &frames {
            let fv = base.add(f.func_slot());
            let fcl = clvalue(fv);
            let fp = Closure::proto(fcl);
            let fr = r.b.slot(f.func_slot(), IrType::Function);
            r.funcs.push((fp, fcl, Some(fr)));
            if !r.protos.contains(&(fp as usize)) {
                r.protos.push(fp as usize);
            }
            r.stack_need = r.stack_need.max(f.base as usize + f.nargs as usize + (*fcl).stacksize as usize + LUA_MINSTACK as usize);
        }
        r.b.off = off;
        r
    }

    /// While a recorded C call runs Lua code: the CallInfo depth below
    /// which its instructions are (they aren't recorded).
    pub fn paused_depth(&self) -> Option<isize> {
        self.callback.map(|c| c.3)
    }

    /// The parent exit a side trace continues.
    pub fn side_parent(&self) -> Option<(u16, u32)> {
        self.side.map(|(_, p)| p)
    }

    pub fn set_state(&mut self, l: *mut lua_State) {
        self.l = l;
    }

    pub fn is_side(&self) -> bool {
        self.side.is_some()
    }

    /// The loop header the trace belongs to.
    pub fn header(&self) -> u32 {
        self.side.map_or(self.start_pc, |(h, _)| h)
    }

    /// Whether the interpreter is in the frame the recorder expects next
    /// (`p` running at `base`).
    pub fn expects(&self, p: *mut Proto, base: StkId) -> bool {
        let (fp, _, _) = *self.funcs.last().unwrap();
        fp == p && unsafe { self.root_base.add(self.b.off) } == base
    }

    fn cur_proto(&self) -> *mut Proto {
        self.funcs.last().unwrap().0
    }

    /// The current frame's closure: (now, as an IR value).
    fn cur_closure(&mut self) -> (*mut Closure, Ref) {
        let last = self.funcs.len() - 1;
        let (_, cl, r) = self.funcs[last];
        let r = match r {
            Some(r) => r,
            None => {
                // the root frame's (inlined frames have theirs from the CALL)
                let r = self.b.emit(Op::FrameFunc, IrType::Function);
                self.funcs[last].2 = Some(r);
                r
            }
        };
        (cl, r)
    }

    /// A GC object constant the trace's code refers to.
    fn gc_const(&mut self, p: usize, ty: IrType) -> Ref {
        // (Ptr constants - upvalue objects, environments - belong to a
        // closure that is kept alive itself)
        if let Some(tag) = ty.tag().filter(|_| ty.is_gc()) {
            if !self.gc_refs.iter().any(|&(q, _)| q == p) {
                self.gc_refs.push((p, tag as u8));
            }
        }
        self.b.kptr(p, ty)
    }

    pub fn start_pc(&self) -> u32 {
        self.start_pc
    }

    pub fn proto(&self) -> *mut Proto {
        self.proto
    }

    pub fn id(&self) -> TraceId {
        self.id
    }

    /// Records the instruction at `pc` (about to execute, with the frame's
    /// stack at `base`).
    pub unsafe fn step(&mut self, pc: u32, base: StkId) -> Step {
        if let Some((c, nres, next)) = self.deferred_call.take() {
            if pc == next {
                if let Err(e) = self.builtin_as_call(None, c, nres, pc, base) {
                    return Step::Abort(e);
                }
            }
        }
        let undo = self.builtin_undo.take();
        let bcall = self.builtin_call.take();
        if let Some((r, slot, next, multret)) = self.pending.take() {
            if pc != next {
                // the builtin declined: the interpreter runs the fallback
                // (a call of the same function), which is recorded instead
                let Some((inst, set)) = undo else {
                    return Step::Abort(AbortReason::UnsupportedType("builtin fell back"));
                };
                self.b.trace.insts[inst.index()].op = Op::Nop;
                if let Some((ra, prev)) = set {
                    match prev {
                        Some(p) => self.b.set_slot(ra, p),
                        None => self.b.forget_slot(ra),
                    }
                }
            } else {
                if multret {
                // a variable number of results: one, as the trace checks
                let n = (*self.l).top.offset_from(self.root_base.add(slot));
                if n != 1 {
                    // several: the call of the builtin's function instead
                    // (the same results)
                    return match (undo, bcall) {
                        (Some((inst, _)), Some(c)) => match self.builtin_as_call(Some(inst), c, -1, pc, base) {
                            Ok(()) => self.step_rest(pc, base),
                            Err(e) => Step::Abort(e),
                        },
                        _ => Step::Abort(AbortReason::UnsupportedType("builtin returned several results")),
                    };
                }
                self.set_top(Some(slot + 1));
                }
                if let Some(r) = r {
                    let tv = self.root_base.add(slot);
                    match IrType::of_tag((*tv).tt) {
                        Some(ty) => self.b.trace.insts[r.index()].ty = ty,
                        _ => return Step::Abort(AbortReason::UnsupportedType("builtin result type")),
                    }
                }
            }
        }
        self.step_rest(pc, base)
    }

    /// `step` after a just-recorded builtin was dealt with.
    unsafe fn step_rest(&mut self, pc: u32, base: StkId) -> Step {
        if let Some((a, nres, next, _, cnt)) = self.callback.take() {
            if pc != next {
                return Step::Abort(AbortReason::Internal("C call didn't return normally".into()));
            }
            // (exits from here on resume after the call)
            self.b.pc = pc;
            if let Err(e) = self.after_callback(a, nres, cnt, base) {
                return Step::Abort(e);
            }
        }
        if let Some((slot, nres, next)) = self.pending_c.take() {
            if pc != next {
                return Step::Abort(AbortReason::Internal("C call didn't return normally".into()));
            }
            // the results, as the function left them (a result of another
            // type exits after the call: never run it twice)
            self.b.pc = pc;
            for i in 0..nres {
                let tv = base.add(slot + i);
                let Some(ty) = IrType::of_tag((*tv).tt) else {
                    return Step::Abort(AbortReason::UnsupportedType("C function result type"));
                };
                let snap = self.b.snapshot();
                let v = self.b.emit(Op::SReload { slot: (self.b.off + slot + i) as u16, snap }, ty);
                self.b.set_slot(slot + i, v);
            }
        }
        let inlined = self.funcs.len() > 1;
        if let Some((header, _)) = self.side {
            if pc == header && !inlined {
                return match self.close_side() {
                    Ok(t) => Step::Closed(t),
                    Err(e) => Step::Abort(e),
                };
            }
            if !inlined && (pc < header || pc > self.loop_end) {
                return Step::Abort(AbortReason::LeftLoop);
            }
            self.steps += 1;
            if self.b.trace.insts.len() > self.max_len {
                return Step::Abort(AbortReason::TooLong);
            }
            return match self.record(pc, base) {
                Ok(()) => Step::Continue,
                Err(e) => Step::Abort(e),
            };
        }
        if pc == self.start_pc && self.steps > 0 && !inlined {
            return match self.close() {
                Ok(t) => Step::Closed(t),
                Err(e) => Step::Abort(e),
            };
        }
        // outside the loop: a break, a goto out of the body or the loop's
        // own exit path; the trace can't close there
        if self.steps > 0 && !inlined && (pc < self.start_pc || pc > self.loop_end) {
            return Step::Abort(AbortReason::LeftLoop);
        }
        self.steps += 1;
        if self.b.trace.insts.len() > self.max_len {
            return Step::Abort(AbortReason::TooLong);
        }
        match self.record(pc, base) {
            Ok(()) => Step::Continue,
            Err(e) => Step::Abort(e),
        }
    }

    // ---- operand helpers ------------------------------------------------

    /// The value of slot `r` as the trace sees it, typed by what's in the
    /// stack now.
    unsafe fn read(&mut self, r: usize, base: StkId) -> R<Ref> {
        let tv = base.add(r);
        let Some(ty) = IrType::of_tag((*tv).tt) else { return Err(unsupported_type("slot type")) };
        if ty == IrType::LightUserdata && (*tv).extra[0] != LU_TAG_ITERATOR {
            return Err(unsupported_type("light userdata"));
        }
        if let Some(v) = self.b.peek(r) {
            // the trace's own value for the slot must agree with the stack
            let vt = self.b.trace.ty(v);
            if vt != ty {
                return Err(AbortReason::Internal(format!("slot {} is {:?} in the trace but {:?} in the stack", r, vt, ty)));
            }
            return Ok(v);
        }
        Ok(self.b.slot(r, ty))
    }

    unsafe fn read_num(&mut self, r: usize, base: StkId) -> R<Ref> {
        if (*base.add(r)).tt != LUA_TNUMBER {
            return Err(unsupported_type("arithmetic on a non-number"));
        }
        self.read(r, base)
    }

    unsafe fn konst(&mut self, k: usize) -> R<Ref> {
        let kv = (*self.cur_proto()).k.add(k);
        self.value_const(kv)
    }

    /// A constant for the value `*tv` (which can't change while the trace
    /// runs: a constant table entry, an immutable upvalue, an import).
    unsafe fn value_const(&mut self, tv: *const TValue) -> R<Ref> {
        Ok(match (*tv).tt {
            LUA_TNIL => self.b.knil(),
            LUA_TBOOLEAN => self.b.kbool((*tv).value.b != 0),
            LUA_TNUMBER => self.b.knum((*tv).value.n),
            LUA_TINTEGER => self.b.kint((*tv).value.l),
            LUA_TSTRING | LUA_TTABLE | LUA_TFUNCTION | LUA_TUSERDATA | LUA_TBUFFER => {
                let ty = IrType::of_tag((*tv).tt).ok_or(unsupported_type("constant"))?;
                self.gc_const((*tv).value.gc as usize, ty)
            }
            LUA_TVECTOR => {
                // (loaded from the constant itself: it outlives the trace)
                let p = self.b.kptr(tv as usize, IrType::Ptr);
                let snap = self.b.snapshot();
                self.b.emit(Op::XLoad { p, snap }, IrType::Vector)
            }
            _ => return Err(unsupported_type("constant")),
        })
    }

    unsafe fn knum(&mut self, k: usize) -> R<Ref> {
        let kv = (*self.cur_proto()).k.add(k);
        if (*kv).tt != LUA_TNUMBER {
            return Err(unsupported_type("arithmetic with a non-number constant"));
        }
        Ok(self.b.knum((*kv).value.n))
    }

    /// The operands of an arithmetic instruction the interpreter does on
    /// vectors (vector +-*/ vector, vector */ number, number */ vector,
    /// vector // number, constants included), if it is one.
    unsafe fn vector_operands(&mut self, op: u8, bb: usize, c: usize, base: StkId) -> R<Option<(Ref, Ref)>> {
        let o = |x: O| op == x as u8;
        let kv = |c: usize| (*self.cur_proto()).k.add(c) as *const TValue;
        let (tb, tc): (*const TValue, *const TValue) = if o(O::Add) || o(O::Sub) || o(O::Mul) || o(O::Div) || o(O::IDiv) {
            (base.add(bb), base.add(c))
        } else if o(O::AddK) || o(O::SubK) || o(O::MulK) || o(O::DivK) || o(O::IDivK) {
            (base.add(bb), kv(c))
        } else if o(O::DivRk) {
            (kv(bb), base.add(c))
        } else {
            return Ok(None);
        };
        let (vb, vc) = ((*tb).tt == LUA_TVECTOR, (*tc).tt == LUA_TVECTOR);
        if !vb && !vc {
            return Ok(None);
        }
        let (nb, nc) = ((*tb).tt == LUA_TNUMBER, (*tc).tt == LUA_TNUMBER);
        let ok = if o(O::Add) || o(O::Sub) || o(O::AddK) || o(O::SubK) {
            vb && vc
        } else if o(O::IDiv) || o(O::IDivK) {
            vb && nc
        } else {
            (vb && (vc || nc)) || (nb && vc)
        };
        if !ok {
            return Err(unsupported_type("vector arithmetic the interpreter doesn't do"));
        }
        let x = if o(O::DivRk) { self.konst(bb)? } else { self.read(bb, base)? };
        let y = if o(O::AddK) || o(O::SubK) || o(O::MulK) || o(O::DivK) || o(O::IDivK) { self.konst(c)? } else { self.read(c, base)? };
        Ok(Some((x, y)))
    }

    fn arith(&mut self, op: u8, a: Ref, b: Ref) -> Ref {
        let o = |x: O| op == x as u8;
        let ir = if o(O::Add) || o(O::AddK) {
            Op::Add(a, b)
        } else if o(O::Sub) || o(O::SubK) || o(O::SubRk) {
            Op::Sub(a, b)
        } else if o(O::Mul) || o(O::MulK) {
            Op::Mul(a, b)
        } else if o(O::Div) || o(O::DivK) || o(O::DivRk) {
            Op::Div(a, b)
        } else if o(O::Mod) || o(O::ModK) {
            Op::Mod(a, b)
        } else if o(O::Pow) || o(O::PowK) {
            Op::Pow(a, b)
        } else {
            Op::IDiv(a, b)
        };
        self.b.emit(ir, IrType::Number)
    }

    /// The table in slot `r`, with an array element at 0-based `idx` that
    /// the trace may access directly: emits the bounds guard and returns a
    /// pointer to the element.
    unsafe fn array_slot(&mut self, t: Ref, idx: Ref) -> R<Ref> {
        let snap = self.b.snapshot();
        let size = self.b.emit(Op::ASize(t), IrType::I32);
        // (0 <= idx < size: one unsigned compare)
        self.b.emit(Op::Guard { cmp: Cmp::ULt, a: idx, b: size, snap }, IrType::Nil);
        Ok(self.aref(t, idx))
    }

    /// Pointer to array element `idx` of table `t` (bounds checked by the
    /// caller).
    fn aref(&mut self, t: Ref, idx: Ref) -> Ref {
        let base = self.b.emit(Op::ABase(t), IrType::Ptr);
        self.b.emit(Op::ARef { t: base, idx }, IrType::Ptr)
    }

    /// 0-based array index for a number key in slot `r` (guarded integral).
    unsafe fn index_of(&mut self, r: usize, base: StkId) -> R<(Ref, usize)> {
        let key = base.add(r);
        if (*key).tt != LUA_TNUMBER {
            return Err(unsupported_type("non-number table key"));
        }
        let n = (*key).value.n;
        if n != n.floor() || !(1.0..=(i32::MAX as f64)).contains(&n) {
            return Err(unsupported_type("non-array table key"));
        }
        let k = self.read(r, base)?;
        let snap = self.b.snapshot();
        let i = self.b.emit(Op::NumToI32 { a: k, snap }, IrType::I32);
        let m1 = self.b.ki32(-1);
        let i0 = self.b.emit(Op::AddI32(i, m1), IrType::I32);
        Ok((i0, n as usize - 1))
    }

    /// Whether `R(t)[R(k)]` (k = usize::MAX: never) can take the array
    /// path: a number key that is an index of the array part (holding a
    /// value, for reads; in bounds of a plain table, for stores).
    unsafe fn array_key(base: StkId, t: usize, k: usize, store: bool) -> bool {
        if k == usize::MAX {
            return false;
        }
        let kv = base.add(k);
        if (*kv).tt != LUA_TNUMBER {
            return false;
        }
        let n = (*kv).value.n;
        if n != n.floor() || !(1.0..=(i32::MAX as f64)).contains(&n) {
            return false;
        }
        Self::observe_array(base, t, n as usize - 1, store).is_ok()
    }

    /// Checks (at record time) that the table in slot `r` has a live array
    /// element at `idx` (reads: non-nil; stores: plain table in bounds).
    unsafe fn observe_array(base: StkId, r: usize, idx: usize, store: bool) -> R<*mut LuaTable> {
        let tv = base.add(r);
        if (*tv).tt != LUA_TTABLE {
            return Err(unsupported_type("indexing a non-table"));
        }
        let h = (*tv).value.gc as *mut LuaTable;
        if idx >= (*h).sizearray as usize {
            return Err(unsupported_type("index outside the array part"));
        }
        if store {
            if !(*h).metatable.is_null() || (*h).readonly != 0 {
                return Err(unsupported_type("store into a table with a metatable"));
            }
        } else if (*(*h).array.add(idx)).tt == LUA_TNIL {
            return Err(unsupported_type("read of a nil array element"));
        }
        Ok(h)
    }

    // ---- one instruction ---------------------------------------------

    unsafe fn record(&mut self, pc: u32, base: StkId) -> R<()> {
        let proto = self.cur_proto();
        let code = (*proto).code;
        let insn = *code.add(pc as usize);
        let aux = *code.add(pc as usize + 1);
        let op = op_of(insn);
        let o = |x: O| op == x as u8;
        let (a, bb, c) = (a_of(insn), b_of(insn), c_of(insn));
        // a guard taken now resumes at this instruction (it hasn't run yet)
        self.b.pc = pc;

        if o(O::Nop) || o(O::Coverage) || o(O::Jump) || o(O::JumpX) {
            // (the interpreter follows the jump; the trace just continues)
        } else if o(O::LoadNil) {
            let v = self.b.knil();
            self.b.set_slot(a, v);
        } else if o(O::LoadB) {
            let v = self.b.kbool(bb != 0);
            self.b.set_slot(a, v);
        } else if o(O::LoadN) {
            let v = self.b.knum(d_of(insn) as f64);
            self.b.set_slot(a, v);
        } else if o(O::LoadK) {
            let v = self.konst(d_of(insn) as usize)?;
            self.b.set_slot(a, v);
        } else if o(O::LoadKx) {
            let v = self.konst(aux as usize)?;
            self.b.set_slot(a, v);
        } else if o(O::Move) {
            let v = self.read(bb, base)?;
            self.b.set_slot(a, v);
        } else if (o(O::Add) || o(O::Sub) || o(O::Mul) || o(O::Div)) && ((*base.add(bb)).tt == LUA_TTABLE || (*base.add(c)).tt == LUA_TTABLE) {
            self.record_arith_mm(op, a, bb, Some(c), None, base)?;
        } else if (o(O::AddK) || o(O::SubK) || o(O::MulK) || o(O::DivK)) && (*base.add(bb)).tt == LUA_TTABLE {
            self.record_arith_mm(op, a, bb, None, Some(c), base)?;
        } else if o(O::Minus) && (*base.add(bb)).tt == LUA_TTABLE {
            self.record_arith_mm(op, a, bb, Some(bb), None, base)?;
        } else if let Some((x, y)) = self.vector_operands(op, bb, c, base)? {
            let opc = if o(O::Add) || o(O::AddK) {
                0x58
            } else if o(O::Sub) || o(O::SubK) {
                0x5c
            } else if o(O::Mul) || o(O::MulK) {
                0x59
            } else {
                0x5e
            };
            let floor = o(O::IDiv) || o(O::IDivK);
            let v = self.b.emit(Op::VArith { op: opc, a: x, b: y, floor }, IrType::Vector);
            self.b.set_slot(a, v);
        } else if o(O::Minus) && (*base.add(bb)).tt == LUA_TVECTOR {
            let x = self.read(bb, base)?;
            let v = self.b.emit(Op::VNeg(x), IrType::Vector);
            self.b.set_slot(a, v);
        } else if o(O::Add) || o(O::Sub) || o(O::Mul) || o(O::Div) || o(O::Mod) || o(O::Pow) || o(O::IDiv) {
            let x = self.read_num(bb, base)?;
            let y = self.read_num(c, base)?;
            let v = self.arith(op, x, y);
            self.b.set_slot(a, v);
        } else if o(O::AddK) || o(O::SubK) || o(O::MulK) || o(O::DivK) || o(O::ModK) || o(O::PowK) || o(O::IDivK) {
            let x = self.read_num(bb, base)?;
            let y = self.knum(c)?;
            let v = if o(O::PowK) && matches!(self.b.trace.inst(y).op, Op::KNum(e) if e == 2.0) {
                self.b.emit(Op::Mul(x, x), IrType::Number)
            } else {
                self.arith(op, x, y)
            };
            self.b.set_slot(a, v);
        } else if o(O::SubRk) || o(O::DivRk) {
            let x = self.knum(bb)?;
            let y = self.read_num(c, base)?;
            let v = self.arith(op, x, y);
            self.b.set_slot(a, v);
        } else if o(O::Minus) {
            let x = self.read_num(bb, base)?;
            let v = self.b.emit(Op::Neg(x), IrType::Number);
            self.b.set_slot(a, v);
        } else if o(O::JumpIf) || o(O::JumpIfNot) {
            let tv = base.add(a);
            let truthy = !((*tv).tt == LUA_TNIL || ((*tv).tt == LUA_TBOOLEAN && (*tv).value.b == 0));
            let v = self.read(a, base)?;
            // only nil/boolean values can change truthiness without a type change
            if matches!(self.b.trace.ty(v), IrType::Nil | IrType::Boolean) {
                let snap = self.b.snapshot();
                self.b.emit(Op::GuardTruthy { a: v, want: truthy, snap }, IrType::Nil);
            }
        } else if o(O::JumpIfEq) || o(O::JumpIfNotEq) || o(O::JumpIfLe) || o(O::JumpIfNotLe) || o(O::JumpIfLt) || o(O::JumpIfNotLt) {
            let rb = aux as usize;
            let ta = (*base.add(a)).tt;
            let tb = (*base.add(rb)).tt;
            let is_eq = o(O::JumpIfEq) || o(O::JumpIfNotEq);
            if is_eq && ta != tb {
                // different types are never equal: the tag guards pin it
                self.read(a, base)?;
                self.read(rb, base)?;
            } else if is_eq && ta == LUA_TTABLE {
                if !self.record_cmp_mm(2, a, rb, base)? {
                    // no __eq applies: identity
                    let same = (*base.add(a)).value.gc == (*base.add(rb)).value.gc;
                    let x = self.read(a, base)?;
                    let y = self.read(rb, base)?;
                    let snap = self.b.snapshot();
                    let op = if same { Op::GuardSame { a: x, k: y, snap } } else { Op::GuardDiff { a: x, k: y, snap } };
                    self.b.emit(op, IrType::Nil);
                }
            } else if !is_eq && ta == LUA_TTABLE && tb == LUA_TTABLE {
                let lt = o(O::JumpIfLt) || o(O::JumpIfNotLt);
                if !self.record_cmp_mm(if lt { 0 } else { 1 }, a, rb, base)? {
                    return Err(unsupported_type("comparison of tables without metamethods"));
                }
            } else if is_eq && ta == LUA_TVECTOR {
                let eq = crate::lobject::luaO_rawequalObj(base.add(a), base.add(rb));
                let x = self.read(a, base)?;
                let y = self.read(rb, base)?;
                let snap = self.b.snapshot();
                self.b.emit(Op::GuardVecEq { a: x, b: y, want: eq, snap }, IrType::Nil);
            } else if is_eq && ta != LUA_TNUMBER && ta != LUA_TVECTOR && ta != LUA_TUSERDATA {
                // same non-number type: identity (strings are interned);
                // tables compare with __eq only when both have metatables
                // and differ
                let (va, vb) = ((*base.add(a)).value.gc, (*base.add(rb)).value.gc);
                let x = self.read(a, base)?;
                let y = self.read(rb, base)?;
                let snap = self.b.snapshot();
                match ta {
                    LUA_TNIL => {}
                    LUA_TBOOLEAN => {
                        let eq = (*base.add(a)).value.b == (*base.add(rb)).value.b;
                        self.b.emit(Op::Guard { cmp: if eq { Cmp::Eq } else { Cmp::Ne }, a: x, b: y, snap }, IrType::Nil);
                    }
                    LUA_TSTRING | LUA_TFUNCTION | LUA_TLIGHTUSERDATA | LUA_TBUFFER => {
                        if ta == LUA_TLIGHTUSERDATA {
                            return Err(unsupported_type("light userdata comparison"));
                        }
                        let op = if va == vb { Op::GuardSame { a: x, k: y, snap } } else { Op::GuardDiff { a: x, k: y, snap } };
                        self.b.emit(op, IrType::Nil);
                    }
                    _ => return Err(unsupported_type("comparison of this type")),
                }
            } else if !is_eq && ta == LUA_TSTRING && tb == LUA_TSTRING {
                let c = crate::lvmutils::luaV_strcmp(tsvalue(base.add(a)), tsvalue(base.add(rb)));
                let x = self.read(a, base)?;
                let y = self.read(rb, base)?;
                let r = self.b.emit(Op::StrCmp { a: x, b: y }, IrType::I32);
                let zero = self.b.ki32(0);
                let lt = o(O::JumpIfLt) || o(O::JumpIfNotLt);
                let holds = if lt { c < 0 } else { c <= 0 };
                let cmp = match (lt, holds) {
                    (true, true) => Cmp::Lt,
                    (true, false) => Cmp::Nlt,
                    (false, true) => Cmp::Le,
                    (false, false) => Cmp::Nle,
                };
                let snap = self.b.snapshot();
                self.b.emit(Op::Guard { cmp, a: r, b: zero, snap }, IrType::Nil);
            } else if ta != LUA_TNUMBER || tb != LUA_TNUMBER {
                return Err(unsupported_type("comparison of non-numbers"));
            } else {
                let (va, vb) = ((*base.add(a)).value.n, (*base.add(rb)).value.n);
                let x = self.read(a, base)?;
                let y = self.read(rb, base)?;
                // the condition as it held now (the guard keeps it holding)
                let (cmp_true, cmp_false, holds) = if o(O::JumpIfEq) || o(O::JumpIfNotEq) {
                    (Cmp::Eq, Cmp::Ne, va == vb)
                } else if o(O::JumpIfLe) || o(O::JumpIfNotLe) {
                    (Cmp::Le, Cmp::Nle, va <= vb)
                } else {
                    (Cmp::Lt, Cmp::Nlt, va < vb)
                };
                let snap = self.b.snapshot();
                let cmp = if holds { cmp_true } else { cmp_false };
                self.b.emit(Op::Guard { cmp, a: x, b: y, snap }, IrType::Nil);
            }
        } else if o(O::JumpXEqKN) {
            if (*base.add(a)).tt != LUA_TNUMBER {
                return Err(unsupported_type("comparison of a non-number"));
            }
            let k = self.knum((aux & 0xffffff) as usize)?;
            let kval = match self.b.trace.insts[k.index()].op {
                Op::KNum(n) => n,
                _ => unreachable!(),
            };
            let x = self.read(a, base)?;
            let eq = (*base.add(a)).value.n == kval;
            let snap = self.b.snapshot();
            self.b.emit(Op::Guard { cmp: if eq { Cmp::Eq } else { Cmp::Ne }, a: x, b: k, snap }, IrType::Nil);
        } else if o(O::JumpXEqKNil) || o(O::JumpXEqKB) || o(O::JumpXEqKS) {
            // A comparison jump against a constant. Reading the slot pins
            // its tag (the guard exits when the tag changes, so the branch
            // direction stays pinned); within the tag only a boolean can
            // differ in value without a tag change, and a string constant
            // equality is pointer equality (interned strings).
            let tv = base.add(a);
            let ktt = if o(O::JumpXEqKNil) {
                LUA_TNIL
            } else if o(O::JumpXEqKB) {
                LUA_TBOOLEAN
            } else {
                LUA_TSTRING
            };
            if (*tv).tt == ktt {
                if o(O::JumpXEqKB) {
                    let x = self.read(a, base)?;
                    let want = (*tv).value.b != 0;
                    let snap = self.b.snapshot();
                    self.b.emit(Op::GuardTruthy { a: x, want, snap }, IrType::Nil);
                } else if o(O::JumpXEqKS) {
                    let kv = (*(*proto).k.add((aux & 0xffffff) as usize)).value.gc as usize;
                    if (*tv).value.gc as usize == kv {
                        // equal (same object): pin it
                        let x = self.read(a, base)?;
                        let k = self.konst((aux & 0xffffff) as usize)?;
                        let snap = self.b.snapshot();
                        self.b.emit(Op::GuardSame { a: x, k, snap }, IrType::Nil);
                    } else {
                        let x = self.read(a, base)?;
                        let k = self.konst((aux & 0xffffff) as usize)?;
                        let snap = self.b.snapshot();
                        self.b.emit(Op::GuardDiff { a: x, k, snap }, IrType::Nil);
                    }
                } else {
                    // nil: the SLoad tag guard pins it
                    self.read(a, base)?;
                }
            } else {
                // the constant has another type: the tag guard alone pins
                // the comparison result
                self.read(a, base)?;
            }
        } else if o(O::ForNPrep) {
            // an inner loop: unrolled into the trace (its iterations are
            // recorded one after the other, see UNROLL_LIMIT)
            let (lim_v, step_v, idx_v) = ((*base.add(a)).value.n, (*base.add(a + 1)).value.n, (*base.add(a + 2)).value.n);
            let limit = self.read_num(a, base)?;
            let step = self.read_num(a + 1, base)?;
            let idx = self.read_num(a + 2, base)?;
            if step_v == 0.0 || step_v.is_nan() {
                return Err(unsupported_type("for loop step"));
            }
            let zero = self.b.knum(0.0);
            let snap = self.b.snapshot();
            if step_v > 0.0 {
                self.b.emit(Op::Guard { cmp: Cmp::Lt, a: zero, b: step, snap }, IrType::Nil);
            } else {
                self.b.emit(Op::Guard { cmp: Cmp::Lt, a: step, b: zero, snap }, IrType::Nil);
            }
            // whether the loop runs at all, as it did now
            let enters = if step_v > 0.0 { idx_v <= lim_v } else { lim_v <= idx_v };
            let (x, y) = if step_v > 0.0 { (idx, limit) } else { (limit, idx) };
            self.b.emit(Op::Guard { cmp: if enters { Cmp::Le } else { Cmp::Nle }, a: x, b: y, snap }, IrType::Nil);
        } else if o(O::ForGPrepInext) || o(O::ForGPrepNext) || o(O::ForGPrep) {
            // an inner generic for over a table: the built-in iteration
            // (unrolled with its FORGLOOP below)
            let (cl, clr) = self.cur_closure();
            if o(O::ForGPrep) {
                if (*base.add(a)).tt != LUA_TTABLE || !(*hvalue(base.add(a))).metatable.is_null() {
                    return Err(unsupported_type("generic for over an iterator"));
                }
                let t = self.read(a, base)?;
                let snap = self.b.snapshot();
                self.b.emit(Op::GuardPlainTable { t, store: false, snap }, IrType::Nil);
                self.b.set_slot(a + 1, t);
            } else {
                if (*(*cl).env).safeenv == 0 || (*base.add(a + 1)).tt != LUA_TTABLE {
                    return Err(unsupported_type("generic for over an iterator"));
                }
                if o(O::ForGPrepInext) {
                    if (*base.add(a + 2)).tt != LUA_TNUMBER || (*base.add(a + 2)).value.n != 0.0 {
                        return Err(unsupported_type("ipairs with a start value"));
                    }
                    let z = self.read(a + 2, base)?;
                    let zero = self.b.knum(0.0);
                    let snap = self.b.snapshot();
                    self.b.emit(Op::Guard { cmp: Cmp::Eq, a: z, b: zero, snap }, IrType::Nil);
                } else if (*base.add(a + 2)).tt != LUA_TNIL {
                    return Err(unsupported_type("next with a start key"));
                } else {
                    self.read(a + 2, base)?;
                }
                self.guard_safeenv(clr);
                self.read(a + 1, base)?;
            }
            let nil = self.b.knil();
            self.b.set_slot(a, nil);
            let zero = self.b.ki32(0);
            let st = self.b.emit(Op::I32ToLud(zero), IrType::LightUserdata);
            self.b.set_slot(a + 2, st);
        } else if o(O::ForGLoop) && (pc as i64 + 1 + d_of(insn) as i64) as u32 != self.header() {
            // an unrolled inner generic for: next array element, or the end
            self.unrolled += 1;
            if self.unrolled > UNROLL_LIMIT {
                return Err(unsupported_type("inner loop too long to unroll"));
            }
            let aux = aux as i32;
            if (*base.add(a)).tt != LUA_TNIL || (*base.add(a + 1)).tt != LUA_TTABLE || (*base.add(a + 2)).tt != LUA_TLIGHTUSERDATA {
                return Err(unsupported_type("generic for over an iterator function"));
            }
            let h = hvalue(base.add(a + 1));
            let i_now = (*base.add(a + 2)).value.p as usize;
            let t = self.read(a + 1, base)?;
            let st = self.read(a + 2, base)?;
            let i = self.b.emit(Op::LudToI32(st), IrType::I32);
            let size = self.b.emit(Op::ASize(t), IrType::I32);
            let snap = self.b.snapshot();
            let continues = i_now < (*h).sizearray as usize && (*(*h).array.add(i_now)).tt != LUA_TNIL;
            if continues {
                self.b.emit(Op::Guard { cmp: Cmp::Lt, a: i, b: size, snap }, IrType::Nil);
                let p = self.aref(t, i);
                let ety = IrType::of_tag((*(*h).array.add(i_now)).tt).ok_or(unsupported_type("array element type"))?;
                let v = self.b.emit(Op::XLoad { p, snap }, ety);
                let one = self.b.ki32(1);
                let i1 = self.b.emit(Op::AddI32(i, one), IrType::I32);
                let key = self.b.emit(Op::I32ToNum(i1), IrType::Number);
                let st1 = self.b.emit(Op::I32ToLud(i1), IrType::LightUserdata);
                self.b.set_slot(a + 2, st1);
                self.b.set_slot(a + 3, key);
                self.b.set_slot(a + 4, v);
                for k in 2..aux.max(2) as usize {
                    let nil = self.b.knil();
                    self.b.set_slot(a + 3 + k, nil);
                }
            } else if i_now >= (*h).sizearray as usize && (aux < 0 || (*h).node == crate::ltable::dummynode()) {
                // the end: past the array part, and (pairs) no hash part
                self.b.emit(Op::Guard { cmp: Cmp::Nlt, a: i, b: size, snap }, IrType::Nil);
                if aux >= 0 {
                    let node = self.b.emit(Op::HSlot { t, idx: 0 }, IrType::Ptr);
                    let dummy = self.b.kptr(crate::ltable::dummynode() as usize, IrType::Ptr);
                    self.b.emit(Op::GuardSame { a: node, k: dummy, snap }, IrType::Nil);
                }
            } else {
                return Err(unsupported_type("generic for over a hash part or a hole"));
            }
        } else if o(O::ForGLoop) && (pc as i64 + 1 + d_of(insn) as i64) as u32 == self.header() {
            // pairs/ipairs over the array part: the next element, or exit
            // (the hash part, a hole, the end: the interpreter goes on)
            let aux = aux as i32;
            if (*base.add(a)).tt != LUA_TNIL || (*base.add(a + 1)).tt != LUA_TTABLE || (*base.add(a + 2)).tt != LUA_TLIGHTUSERDATA {
                return Err(unsupported_type("generic for over an iterator function"));
            }
            let h = hvalue(base.add(a + 1));
            let i_now = (*base.add(a + 2)).value.p as usize;
            if i_now >= (*h).sizearray as usize || (*(*h).array.add(i_now)).tt == LUA_TNIL {
                return Err(AbortReason::LeftLoop);
            }
            self.read(a, base)?;
            let t = self.read(a + 1, base)?;
            let st = self.read(a + 2, base)?;
            let i = self.b.emit(Op::LudToI32(st), IrType::I32);
            let size = self.b.emit(Op::ASize(t), IrType::I32);
            let snap = self.b.snapshot();
            self.b.emit(Op::Guard { cmp: Cmp::Lt, a: i, b: size, snap }, IrType::Nil);
            let p = self.aref(t, i);
            let ety = IrType::of_tag((*(*h).array.add(i_now)).tt).ok_or(unsupported_type("array element type"))?;
            let v = self.b.emit(Op::XLoad { p, snap }, ety);
            let one = self.b.ki32(1);
            let i1 = self.b.emit(Op::AddI32(i, one), IrType::I32);
            let key = self.b.emit(Op::I32ToNum(i1), IrType::Number);
            let st1 = self.b.emit(Op::I32ToLud(i1), IrType::LightUserdata);
            self.b.set_slot(a + 2, st1);
            self.b.set_slot(a + 3, key);
            self.b.set_slot(a + 4, v);
            // (as the interpreter: extra variables only for a positive aux;
            // a negative one is ipairs' flag)
            for k in 2..aux.max(2) as usize {
                let nil = self.b.knil();
                self.b.set_slot(a + 3 + k, nil);
            }
        } else if o(O::ForNLoop) && (pc as i64 + 1 + d_of(insn) as i64) as u32 != self.header() {
            // the back edge of an unrolled inner loop: its direction is a
            // guard (a different trip count exits, for a side trace)
            self.unrolled += 1;
            if self.unrolled > UNROLL_LIMIT {
                return Err(unsupported_type("inner loop too long to unroll"));
            }
            let target = (pc as i64 + 1 + d_of(insn) as i64) as u32;
            let (lim_v, step_v, idx_v) = ((*base.add(a)).value.n, (*base.add(a + 1)).value.n, (*base.add(a + 2)).value.n);
            let limit = self.read_num(a, base)?;
            let step = self.read_num(a + 1, base)?;
            let idx = self.read_num(a + 2, base)?;
            let next = idx_v + step_v;
            let continues = if step_v > 0.0 { next <= lim_v } else { lim_v <= next };
            let i1 = self.b.emit(Op::Add(idx, step), IrType::Number);
            self.b.set_slot(a + 2, i1);
            // a failed guard resumes where the other direction goes
            self.b.pc = if continues { pc + 1 } else { target };
            let snap = self.b.snapshot();
            let (x, y) = if step_v > 0.0 { (i1, limit) } else { (limit, i1) };
            self.b.emit(Op::Guard { cmp: if continues { Cmp::Le } else { Cmp::Nle }, a: x, b: y, snap }, IrType::Nil);
        } else if o(O::JumpBack) && (pc as i64 + 1 + d_of(insn) as i64) as u32 != self.header() {
            // an unrolled inner while/repeat loop's back edge
            self.unrolled += 1;
            if self.unrolled > UNROLL_LIMIT {
                return Err(unsupported_type("inner loop too long to unroll"));
            }
        } else if o(O::ForNLoop) {
            let target = (pc as i64 + 1 + d_of(insn) as i64) as u32;
            if target != self.header() {
                return Err(AbortReason::UnsupportedControlFlow);
            }
            let (lim_v, step_v, idx_v) = ((*base.add(a)).value.n, (*base.add(a + 1)).value.n, (*base.add(a + 2)).value.n);
            let limit = self.read_num(a, base)?;
            let step = self.read_num(a + 1, base)?;
            let idx = self.read_num(a + 2, base)?;
            let next = idx_v + step_v;
            let continues = if step_v > 0.0 { next <= lim_v } else { lim_v <= next };
            if !continues {
                return Err(AbortReason::LeftLoop);
            }
            // the step's sign is fixed for the whole loop (FORNPREP's
            // registers aren't written by the body): guard it once
            let zero = self.b.knum(0.0);
            let snap = self.b.snapshot();
            if step_v > 0.0 {
                self.b.emit(Op::Guard { cmp: Cmp::Lt, a: zero, b: step, snap }, IrType::Nil);
            } else {
                self.b.emit(Op::Guard { cmp: Cmp::Lt, a: step, b: zero, snap }, IrType::Nil);
            }
            let i1 = self.b.emit(Op::Add(idx, step), IrType::Number);
            self.b.set_slot(a + 2, i1);
            // past the limit: the loop is over (FORNLOOP fell through)
            self.b.pc = pc + 1;
            let snap = self.b.snapshot();
            if step_v > 0.0 {
                self.b.emit(Op::Guard { cmp: Cmp::Le, a: i1, b: limit, snap }, IrType::Nil);
            } else {
                self.b.emit(Op::Guard { cmp: Cmp::Le, a: limit, b: i1, snap }, IrType::Nil);
            }
        } else if o(O::JumpBack) {
            let target = (pc as i64 + 1 + d_of(insn) as i64) as u32;
            if target != self.header() {
                return Err(AbortReason::UnsupportedControlFlow);
            }
        } else if o(O::GetTableN) && Self::observe_array(base, bb, c, false).is_err() {
            let h = if (*base.add(bb)).tt == LUA_TTABLE { hvalue(base.add(bb)) } else { return Err(unsupported_type("indexing a non-table")) };
            let val = crate::ltable::luaH_getnum(h, c as i32 + 1);
            let k = self.b.knum((c + 1) as f64);
            let v = self.record_tget(bb, k, val, base)?;
            self.b.set_slot(a, v);
        } else if o(O::GetTableN) {
            Self::observe_array(base, bb, c, false)?;
            let t = self.read(bb, base)?;
            let idx = self.b.ki32(c as i32);
            let p = self.array_slot(t, idx)?;
            let ety = IrType::of_tag((*(*((*base.add(bb)).value.gc as *mut LuaTable)).array.add(c)).tt)
                .ok_or(unsupported_type("array element type"))?;
            let snap = self.b.snapshot();
            let v = self.b.emit(Op::XLoad { p, snap }, ety);
            self.b.set_slot(a, v);
        } else if o(O::GetTable) && !Self::array_key(base, bb, c, false) {
            let tv = base.add(bb);
            if (*tv).tt != LUA_TTABLE {
                return Err(unsupported_type("indexing a non-table"));
            }
            let val = crate::ltable::luaH_get(hvalue(tv), base.add(c));
            let k = self.read(c, base)?;
            if matches!(self.b.trace.ty(k), IrType::Nil) {
                return Err(unsupported_type("nil key"));
            }
            let v = self.record_tget(bb, k, val, base)?;
            self.b.set_slot(a, v);
        } else if o(O::GetTable) {
            let tv = base.add(c);
            if (*tv).tt != LUA_TNUMBER {
                return Err(unsupported_type("non-number table key"));
            }
            let n = (*tv).value.n;
            if n != n.floor() || !(1.0..=(i32::MAX as f64)).contains(&n) {
                return Err(unsupported_type("non-array table key"));
            }
            let h = Self::observe_array(base, bb, n as usize - 1, false)?;
            let t = self.read(bb, base)?;
            let (idx, i) = self.index_of(c, base)?;
            let p = self.array_slot(t, idx)?;
            let ety = IrType::of_tag((*(*h).array.add(i)).tt).ok_or(unsupported_type("array element type"))?;
            let snap = self.b.snapshot();
            let v = self.b.emit(Op::XLoad { p, snap }, ety);
            self.b.set_slot(a, v);
        } else if (o(O::SetTableN) || o(O::SetTable)) && !Self::array_key(base, bb, if o(O::SetTableN) { usize::MAX } else { c }, true) && !(o(O::SetTableN) && Self::observe_array(base, bb, c, true).is_ok()) {
            let k = if o(O::SetTableN) { self.b.knum((c + 1) as f64) } else { self.read(c, base)? };
            let v = self.read(a, base)?;
            self.record_tset(bb, k, v, base)?;
        } else if o(O::SetTableN) || o(O::SetTable) {
            let i = if o(O::SetTableN) {
                c
            } else {
                let tv = base.add(c);
                if (*tv).tt != LUA_TNUMBER {
                    return Err(unsupported_type("non-number table key"));
                }
                let n = (*tv).value.n;
                if n != n.floor() || !(1.0..=(i32::MAX as f64)).contains(&n) {
                    return Err(unsupported_type("non-array table key"));
                }
                n as usize - 1
            };
            Self::observe_array(base, bb, i, true)?;
            let t = self.read(bb, base)?;
            self.mm_store_ok(Some(t))?;
            let snap = self.b.snapshot();
            self.b.emit(Op::GuardPlainTable { t, store: true, snap }, IrType::Nil);
            let idx = if o(O::SetTableN) { self.b.ki32(c as i32) } else { self.index_of(c, base)?.0 };
            let p = self.array_slot(t, idx)?;
            let v = self.read(a, base)?;
            self.b.emit(Op::XStore { p, v }, IrType::Nil);
            if self.b.trace.ty(v).is_gc() {
                self.b.emit(Op::Barrier { t, v }, IrType::Nil);
            }
        } else if o(O::And) || o(O::Or) || o(O::AndK) || o(O::OrK) {
            // R(A) = R(B) and/or R(C) (or constant C): which operand is
            // taken depends only on R(B)'s truthiness, which its type pins
            // except for nil/boolean (guarded)
            let tv = base.add(bb);
            let truthy = !((*tv).tt == LUA_TNIL || ((*tv).tt == LUA_TBOOLEAN && (*tv).value.b == 0));
            let x = self.read(bb, base)?;
            if matches!(self.b.trace.ty(x), IrType::Nil | IrType::Boolean) {
                let snap = self.b.snapshot();
                self.b.emit(Op::GuardTruthy { a: x, want: truthy, snap }, IrType::Nil);
            }
            let take_b = if o(O::And) || o(O::AndK) { !truthy } else { truthy };
            let v = if take_b {
                x
            } else if o(O::AndK) || o(O::OrK) {
                self.konst(c)?
            } else {
                self.read(c, base)?
            };
            self.b.set_slot(a, v);
        } else if o(O::GetTableKs) && (*base.add(bb)).tt == LUA_TVECTOR {
            // v.x / v.y / v.z (any case)
            let kv = (*proto).k.add(aux as usize);
            let lane = if (*kv).tt == LUA_TSTRING {
                let name = crate::lobject::sbytes(tsvalue(kv));
                if name.len() == 1 { ((name[0] | b' ').wrapping_sub(b'x') as u32) as usize } else { 3 }
            } else {
                3
            };
            if lane >= 3 {
                return Err(unsupported_type("vector field other than x/y/z"));
            }
            let x = self.read(bb, base)?;
            let v = self.b.emit(Op::VGet { v: x, lane: lane as u8 }, IrType::Number);
            self.b.set_slot(a, v);
        } else if o(O::GetTableKs) || o(O::GetGlobal) {
            // a string-keyed field (of R(B), or of the environment)
            let kidx = aux as usize;
            let (h, t) = if o(O::GetTableKs) {
                if (*base.add(bb)).tt != LUA_TTABLE {
                    return Err(unsupported_type("field of a non-table"));
                }
                (hvalue(base.add(bb)), self.read(bb, base)?)
            } else {
                let (cl, clr) = self.cur_closure();
                (((*cl).env), self.b.emit(Op::ClEnv { cl: clr }, IrType::Ptr))
            };
            let field = self.field_ref(h, t, kidx, false);
            let v = match field {
                Ok(p) => {
                    let ety = IrType::of_tag((*p.1).tt).ok_or(unsupported_type("field type"))?;
                    let snap = self.b.snapshot();
                    self.b.emit(Op::XLoad { p: p.0, snap }, ety)
                }
                // a missing field of a table: generic lookup
                Err(_) if o(O::GetTableKs) => {
                    let kv = (*self.cur_proto()).k.add(kidx);
                    let val = crate::ltable::luaH_getstr(h, tsvalue(kv));
                    let k = self.konst(kidx)?;
                    self.record_tget(bb, k, val, base)?
                }
                Err(e) => return Err(e),
            };
            self.b.set_slot(a, v);
        } else if o(O::SetTableKs) || o(O::SetGlobal) {
            let kidx = aux as usize;
            let (h, t) = if o(O::SetTableKs) {
                if (*base.add(bb)).tt != LUA_TTABLE {
                    return Err(unsupported_type("field of a non-table"));
                }
                (hvalue(base.add(bb)), self.read(bb, base)?)
            } else {
                let (cl, clr) = self.cur_closure();
                (((*cl).env), self.b.emit(Op::ClEnv { cl: clr }, IrType::Ptr))
            };
            let v = self.read(a, base)?;
            self.mm_store_ok(Some(t))?;
            let (p, _) = match self.field_ref(h, t, kidx, true) {
                Ok(x) => x,
                // a new key: generic store
                Err(_) if o(O::SetTableKs) => {
                    let k = self.konst(kidx)?;
                    return self.record_tset(bb, k, v, base);
                }
                Err(e) => return Err(e),
            };
            self.b.emit(Op::XStore { p, v }, IrType::Nil);
            if self.b.trace.ty(v).is_gc() {
                self.b.emit(Op::Barrier { t, v }, IrType::Nil);
            }
        } else if o(O::Length) {
            let tv = base.add(bb);
            let x = self.read(bb, base)?;
            let v = if (*tv).tt == LUA_TTABLE {
                if !(*hvalue(tv)).metatable.is_null() {
                    return Err(unsupported_type("length of a table with a metatable"));
                }
                let snap = self.b.snapshot();
                self.b.emit(Op::GuardPlainTable { t: x, store: false, snap }, IrType::Nil);
                self.b.emit(Op::TLen { t: x }, IrType::Number)
            } else if (*tv).tt == LUA_TSTRING {
                self.b.emit(Op::SLen { s: x }, IrType::Number)
            } else {
                return Err(unsupported_type("length of a non-table"));
            };
            self.b.set_slot(a, v);
        } else if o(O::NewTable) {
            let nhash = if bb == 0 { 0 } else { 1u32 << (bb - 1) };
            let v = self.b.emit(Op::NewTable { narray: aux, nhash }, IrType::Table);
            if !self.mm.is_empty() {
                self.fresh.push(v);
            }
            self.b.set_slot(a, v);
        } else if o(O::DupTable) {
            let kv = (*proto).k.add(d_of(insn) as usize);
            if (*kv).tt != LUA_TTABLE {
                return Err(unsupported_type("DUPTABLE of a non-table"));
            }
            let tpl = self.b.kptr((*kv).value.gc as usize, IrType::Table);
            let v = self.b.emit(Op::DupTable { tpl }, IrType::Table);
            if !self.mm.is_empty() {
                self.fresh.push(v);
            }
            self.b.set_slot(a, v);
        } else if o(O::SetList) {
            // R(A)[aux + i] = R(B + i): one store each
            let n = if c == 0 {
                let top = self.top.ok_or(AbortReason::UnsupportedType("SETLIST without a known top"))?;
                self.set_top(None);
                top - (self.b.off + bb)
            } else {
                c - 1
            };
            for i in 0..n {
                let k = self.b.knum((aux as usize + i) as f64);
                let v = self.read(bb + i, base)?;
                self.record_tset(a, k, v, base)?;
            }
        } else if o(O::Not) {
            let x = self.read(bb, base)?;
            let v = match self.b.trace.ty(x) {
                IrType::Nil => self.b.kbool(true),
                IrType::Boolean => self.b.emit(Op::Not(x), IrType::Boolean),
                _ => self.b.kbool(false),
            };
            self.b.set_slot(a, v);
        } else if (o(O::Concat) || o(O::SetUpval)) && !self.mm.is_empty() {
            return Err(unsupported_type("side effect inside a metamethod"));
        } else if o(O::Concat) {
            for s in bb..=c {
                let tt = (*base.add(s)).tt;
                if tt != LUA_TSTRING && tt != LUA_TNUMBER {
                    return Err(unsupported_type("concatenation of a non-string"));
                }
                // (the tag guards keep them strings/numbers: no __concat)
                self.read(s, base)?;
            }
            let snap = self.b.snapshot();
            let (savedpc, frames) = self.call_context(pc + 1);
            self.b.emit(Op::Concat { a: (self.b.off + a) as u16, b: (self.b.off + bb) as u16, c: (self.b.off + c) as u16, savedpc, frames, off: self.b.off as u16, snap }, IrType::Nil);
            for s in bb..=c {
                self.b.forget_slot(s);
            }
            self.b.forget_slot(a);
            self.pending_c = Some((a, 1, pc + 1));
        } else if o(O::SetUpval) {
            let (cl, clr) = self.cur_closure();
            let ur = Closure::uprefs(cl).add(bb);
            if (*ur).tt != LUA_TUPVAL {
                return Err(unsupported_type("store to a closed-by-value upvalue"));
            }
            let val = (*upvalue(ur)).v as *const TValue;
            let rel = (val as isize - self.root_base as isize) / std::mem::size_of::<TValue>() as isize;
            let v = self.read(a, base)?;
            if rel >= 0 && rel < STACK_WINDOW as isize {
                // open on a slot of the trace's frames: that slot is written
                let slot = rel as usize;
                if slot >= self.b.off {
                    return Err(AbortReason::Internal("upvalue of the current frame".into()));
                }
                let snap = self.b.snapshot();
                self.b.emit(Op::GuardUvSlot { cl: clr, idx: bb as u8, slot: slot as u16, snap }, IrType::Nil);
                let saved = self.b.off;
                self.b.off = 0;
                self.b.set_slot(slot, v);
                self.b.off = saved;
            } else {
                self.ustores = true;
                let buf = Box::leak(vec![0u64; 2].into_boxed_slice()).as_mut_ptr() as usize;
                let snap = self.b.snapshot();
                self.b.emit(Op::UStore { cl: clr, idx: bb as u8, v, buf, snap }, IrType::Nil);
            }
        } else if o(O::PrepVarargs) {
            // (an inlined vararg callee's first instruction: its frame
            // moves up past the arguments, the fixed ones copied up)
            if self.funcs.len() == 1 {
                return Err(AbortReason::Internal("PREPVARARGS in the trace's frame".into()));
            }
            let f = self.b.frames.last().unwrap().clone();
            let np = a;
            let shift = (f.nargs as usize).max(np);
            let mut fixed = Vec::with_capacity(np);
            for i in 0..np {
                fixed.push(self.read(i, base)?);
            }
            for i in 0..np {
                let nil = self.b.knil();
                self.b.set_slot(i, nil);
            }
            self.b.shift_frame(shift as u16);
            for (i, v) in fixed.into_iter().enumerate() {
                self.b.set_slot(i, v);
            }
        } else if o(O::GetVarargs) {
            let (n, at) = self.varargs_of(base)?;
            let vbase = if at.is_some() { self.root_base.add(at.unwrap()).add(n) } else { base };
            let want = bb as i32 - 1;
            let count = if want < 0 { n } else { want as usize };
            for j in 0..count {
                let v = if j < n { self.vararg(j, n, at, vbase)? } else { self.b.knil() };
                self.b.set_slot(a + j, v);
            }
            if want < 0 {
                self.set_top(Some(self.b.off + a + n));
            }
        } else if o(O::FastCall1) && a as u8 == luau_common::bytecode::BuiltinFunction::SelectVararg as u8 {
            // select(i, ...) / select("#", ...): from the frame's varargs
            let call = *code.add(pc as usize + 1 + c);
            let (ra, nres) = (a_of(call), c_of(call) as i32 - 1);
            let (cl, clr) = self.cur_closure();
            let arg = base.add(bb);
            let (n0, _) = self.varargs_of(base)?;
            let takes = nres == 1
                && (((*arg).tt == LUA_TSTRING && *getstr(tsvalue(arg)) == b'#')
                    || ((*arg).tt == LUA_TNUMBER && (1..=n0 as i32).contains(&crate::lvmexecute::d2i_trunc((*arg).value.n))));
            if (*(*cl).env).safeenv == 0 || !takes {
                // the builtin declines: the CALL after it runs (recorded
                // next); the guards keep the operand types that decided it
                if (*(*cl).env).safeenv != 0 {
                    self.read(bb, base)?;
                }
                return Ok(());
            }
            let (n, at) = self.varargs_of(base)?;
            let vbase = if at.is_some() { self.root_base.add(at.unwrap()).add(n) } else { base };
            let arg = base.add(bb);
            let x = self.read(bb, base)?;
            let snap = self.b.snapshot();
            let v = if (*arg).tt == LUA_TSTRING && *getstr(tsvalue(arg)) == b'#' {
                if !matches!(self.b.trace.inst(x).op, Op::KPtr(_)) {
                    let k = self.gc_const((*arg).value.gc as usize, IrType::String);
                    self.b.emit(Op::GuardSame { a: x, k, snap }, IrType::Nil);
                }
                self.b.knum(n as f64)
            } else if (*arg).tt == LUA_TNUMBER {
                let i = crate::lvmexecute::d2i_trunc((*arg).value.n);
                if !(1..=n as i32).contains(&i) {
                    return Err(unsupported_type("select of a vararg out of range"));
                }
                match at {
                    Some(_) => {
                        // (an inlined frame's varargs are trace slots)
                        let k = self.b.knum(i as f64);
                        self.b.emit(Op::Guard { cmp: Cmp::Eq, a: x, b: k, snap }, IrType::Nil);
                        self.vararg(i as usize - 1, n, at, vbase)?
                    }
                    None => {
                        let i1 = self.b.emit(Op::NumToI32 { a: x, snap }, IrType::I32);
                        let m1 = self.b.ki32(-1);
                        let idx = self.b.emit(Op::AddI32(i1, m1), IrType::I32);
                        let zero = self.b.ki32(0);
                        let kn = self.b.ki32(n as i32);
                        self.b.emit(Op::Guard { cmp: Cmp::Le, a: zero, b: idx, snap }, IrType::Nil);
                        self.b.emit(Op::Guard { cmp: Cmp::Lt, a: idx, b: kn, snap }, IrType::Nil);
                        let tv = vbase.sub(n).add(i as usize - 1);
                        let ty = IrType::of_tag((*tv).tt).ok_or(unsupported_type("vararg type"))?;
                        if ty == IrType::LightUserdata {
                            return Err(unsupported_type("light userdata"));
                        }
                        self.b.emit(Op::VargLoad { idx, snap }, ty)
                    }
                }
            } else {
                return Err(unsupported_type("select with this argument"));
            };
            self.guard_safeenv(clr);
            self.b.set_slot(ra, v);
        } else if o(O::NewClosure) || o(O::DupClosure) {
            self.record_closure(pc, insn, base)?;
        } else if o(O::FastPCall) {
            // pcall / xpcall of a function: the interpreter's fast path is
            // the CALL of the builtin after it, recorded as that
            let call_at = pc as usize + 1 + c;
            let call = *code.add(call_at);
            let (ra, nres) = (a_of(call), c_of(call) as i32 - 1);
            let g = (*self.l).global;
            let target = if a == 0 { (*g).builtinPcall } else { (*g).builtinXpcall };
            let (cl, clr) = self.cur_closure();
            let nparams = if b_of(call) == 0 {
                match self.top {
                    Some(t) => t - (self.b.off + ra) - 1,
                    None => return Err(unsupported_type("pcall without a known top")),
                }
            } else {
                b_of(call) - 1
            };
            let funcs_ok = if a == 0 {
                nparams >= 1 && (*base.add(ra + 1)).tt == LUA_TFUNCTION
            } else {
                nparams >= 2 && (*base.add(ra + 1)).tt == LUA_TFUNCTION && (*base.add(ra + 2)).tt == LUA_TFUNCTION
            };
            if (*(*cl).env).safeenv == 0 || (*self.l).nCcalls > (*self.l).baseCcalls || target.is_null() || !funcs_ok {
                // the fast path declines: the CALL after it runs (recorded next)
                return Ok(());
            }
            self.guard_safeenv(clr);
            let nargs = self.call_args(ra, b_of(call))?;
            let f = self.gc_const(target as usize, IrType::Function);
            self.b.set_slot(ra, f);
            let ret_pc = call_at as u32 + if op_of(call) == O::CallFb as u8 { 2 } else { 1 };
            self.record_callback(ra, nargs, nres, ret_pc, base)?;
        } else if o(O::CloseUpvals) {
            let snap = self.b.snapshot();
            self.b.emit(Op::CloseUpvals { slot: (self.b.off + a) as u16, snap }, IrType::Nil);
        } else if o(O::Namecall) {
            self.record_namecall(insn, aux, base)?;
        } else if o(O::GetUpval) {
            let (cl, clr) = self.cur_closure();
            let ur = Closure::uprefs(cl).add(bb);
            let by_ref = (*ur).tt == LUA_TUPVAL;
            let val = if by_ref { (*upvalue(ur)).v as *const TValue } else { ur as *const TValue };
            let rel = (val as isize - self.root_base as isize) / std::mem::size_of::<TValue>() as isize;
            if by_ref && rel >= 0 && rel < STACK_WINDOW as isize {
                // open on a slot of the trace's own frames, which the trace
                // may hold in a register: use the trace's value of the slot
                let slot = rel as usize;
                if slot >= self.b.off {
                    // (the current frame's own slots: not an upvalue of it)
                    return Err(AbortReason::Internal("upvalue of the current frame".into()));
                }
                let snap = self.b.snapshot();
                self.b.emit(Op::GuardUvSlot { cl: clr, idx: bb as u8, slot: slot as u16, snap }, IrType::Nil);
                let ty = IrType::of_tag((*val).tt).ok_or(unsupported_type("upvalue type"))?;
                let saved = self.b.off;
                self.b.off = 0;
                let v = match self.b.peek(slot) {
                    Some(v) => v,
                    None => self.b.slot(slot, ty),
                };
                self.b.off = saved;
                if self.b.trace.ty(v) != ty {
                    return Err(AbortReason::Internal("upvalue slot type mismatch".into()));
                }
                self.b.set_slot(a, v);
            } else {
                let ty = IrType::of_tag((*val).tt).ok_or(unsupported_type("upvalue type"))?;
                let snap = self.b.snapshot();
                let v = self.b.emit(Op::ULoad { cl: clr, idx: bb as u8, by_ref, stack: STACK_WINDOW as u16, snap }, ty);
                self.b.set_slot(a, v);
            }
        } else if o(O::GetImport) {
            let kv = (*proto).k.add(d_of(insn) as usize);
            let (cl, clr) = self.cur_closure();
            if (*kv).tt == LUA_TNIL || (*(*cl).env).safeenv == 0 {
                return Err(unsupported_type("import from an unsafe environment"));
            }
            self.guard_safeenv(clr);
            let v = self.value_const(kv)?;
            self.b.set_slot(a, v);
        } else if o(O::FastCall1) {
            // FASTCALL1 bfid, arg, skip: the CALL it replaces is `skip`
            // instructions after the next; on success the interpreter jumps
            // past that CALL with the result in its R(A)
            let call = *code.add(pc as usize + 1 + c);
            let (ra, nres) = (a_of(call), c_of(call) as i32 - 1);
            let bfid = a as u8;
            let (cl, clr) = self.cur_closure();
            if (*(*cl).env).safeenv == 0 || nres != 1 || !math1_supported(bfid) || (*base.add(bb)).tt != LUA_TNUMBER {
                return self.record_builtin(pc, insn, aux, base);
            }
            self.guard_safeenv(clr);
            let x = self.read_num(bb, base)?;
            let v = self.b.emit(Op::Math1(bfid, x), IrType::Number);
            self.b.set_slot(ra, v);
        } else if o(O::Call) || o(O::CallFb) {
            self.record_call(pc, insn, base)?;
        } else if o(O::Return) {
            self.record_return(insn, base)?;
        } else if o(O::FastCall1) || o(O::FastCall2) || o(O::FastCall2K) || o(O::FastCall3) {
            self.record_builtin(pc, insn, aux, base)?;
        } else if o(O::FastCall) {
            self.record_builtin(pc, insn, aux, base)?;
        } else {
            let name = O::try_from(op).map(|x| opname(x)).unwrap_or("?");
            return Err(AbortReason::UnsupportedOpcode(name));
        }
        Ok(())
    }

    /// The node of table `h` (IR value `t`) holding string constant `kidx`
    /// of the current function: (pointer to its value, the value now).
    /// Only existing, non-nil fields: anything else (a new key, `__index`,
    /// `__newindex`) is left to the interpreter.
    unsafe fn field_ref(&mut self, h: *mut LuaTable, t: Ref, kidx: usize, store: bool) -> R<(Ref, *const TValue)> {
        let kv = (*self.cur_proto()).k.add(kidx);
        if (*kv).tt != LUA_TSTRING {
            return Err(unsupported_type("non-string field key"));
        }
        let k = self.konst(kidx)?;
        self.field_ref_str(h, t, tsvalue(kv), k, store)
    }

    /// `field_ref` for a key string `ks` (IR value `k`).
    unsafe fn field_ref_str(&mut self, h: *mut LuaTable, t: Ref, ks: *mut TString, k: Ref, store: bool) -> R<(Ref, *const TValue)> {
        if store && (*h).readonly != 0 {
            return Err(unsupported_type("store into a read-only table"));
        }
        let res = crate::ltable::luaH_getstr(h, ks);
        if res == luaO_nilobject() || (*res).tt == LUA_TNIL {
            return Err(unsupported_type("missing field"));
        }
        let node = (*h).node;
        let idx = (res as *const LuaNode).offset_from(node);
        if idx < 0 || idx >= (1isize << (*h).lsizenode) {
            return Err(AbortReason::Internal("field outside the hash part".into()));
        }
        let snap = self.b.snapshot();
        let p = self.b.emit(Op::HRef { t, k, idx: idx as u32, lsize: (*h).lsizenode, store, snap }, IrType::Ptr);
        Ok((p, res))
    }

    /// NAMECALL A B K: R(A+1) = R(B); R(A) = the method K, found in the
    /// table itself, or in its metatable's __index table (classes), or in
    /// the string library for strings.
    unsafe fn record_namecall(&mut self, insn: u32, aux: u32, base: StkId) -> R<()> {
        let (a, bb) = (a_of(insn), b_of(insn));
        let kv = (*self.cur_proto()).k.add(aux as usize);
        if (*kv).tt != LUA_TSTRING {
            return Err(unsupported_type("non-string method name"));
        }
        let ks = tsvalue(kv);
        let recv = base.add(bb);
        let obj = self.read(bb, base)?;
        let k = self.konst(aux as usize)?;
        let g = (*self.l).global;
        let method = if (*recv).tt == LUA_TTABLE {
            let h = hvalue(recv);
            let own = crate::ltable::luaH_getstr(h, ks);
            if own != luaO_nilobject() && (*own).tt != LUA_TNIL {
                let (p, val) = self.field_ref_str(h, obj, ks, k, false)?;
                let ty = IrType::of_tag((*val).tt).ok_or(unsupported_type("method type"))?;
                let snap = self.b.snapshot();
                self.b.emit(Op::XLoad { p, snap }, ty)
            } else {
                // not in the table: its metatable's __index table
                let mt = (*h).metatable;
                if mt.is_null() {
                    return Err(unsupported_type("missing method"));
                }
                let snap = self.b.snapshot();
                self.b.emit(Op::HMiss { t: obj, k, snap }, IrType::Nil);
                let mtr = self.b.emit(Op::TMeta { t: obj, snap }, IrType::Table);
                let idx_name = (*g).tmname[crate::ltm::TM_INDEX as usize];
                let kidx = self.b.kptr(idx_name as usize, IrType::String);
                let (ip, iv) = self.field_ref_str(mt, mtr, idx_name, kidx, false)?;
                if (*iv).tt != LUA_TTABLE {
                    return Err(unsupported_type("__index is not a table"));
                }
                let snap = self.b.snapshot();
                let idx = self.b.emit(Op::XLoad { p: ip, snap }, IrType::Table);
                let (p, val) = self.field_ref_str(hvalue(iv), idx, ks, k, false)?;
                let ty = IrType::of_tag((*val).tt).ok_or(unsupported_type("method type"))?;
                let snap = self.b.snapshot();
                self.b.emit(Op::XLoad { p, snap }, ty)
            }
        } else if (*recv).tt == LUA_TSTRING {
            // the string library (the string metatable's __index: permanent)
            let smt = (*g).mt[LUA_TSTRING as usize];
            if smt.is_null() {
                return Err(unsupported_type("strings without a metatable"));
            }
            let idx_name = (*g).tmname[crate::ltm::TM_INDEX as usize];
            let lib = crate::ltable::luaH_getstr(smt, idx_name);
            if (*lib).tt != LUA_TTABLE {
                return Err(unsupported_type("string __index is not a table"));
            }
            let libr = self.b.kptr((*lib).value.gc as usize, IrType::Table);
            let (p, val) = self.field_ref_str(hvalue(lib), libr, ks, k, false)?;
            let ty = IrType::of_tag((*val).tt).ok_or(unsupported_type("method type"))?;
            let snap = self.b.snapshot();
            self.b.emit(Op::XLoad { p, snap }, ty)
        } else {
            return Err(unsupported_type("method call on this type"));
        };
        self.b.set_slot(a + 1, obj);
        self.b.set_slot(a, method);
        Ok(())
    }

    /// `t[k]` for table slot `tslot` and key IR value `k` (the interpreter is
    /// about to read it): generic lookup, typed by the value there now. A
    /// nil result only for a table without a metatable (no __index).
    unsafe fn record_tget(&mut self, tslot: usize, k: Ref, val: *const TValue, base: StkId) -> R<Ref> {
        let tv = base.add(tslot);
        if (*tv).tt != LUA_TTABLE {
            return Err(unsupported_type("indexing a non-table"));
        }
        let t = self.read(tslot, base)?;
        let ty = IrType::of_tag((*val).tt).ok_or(unsupported_type("table value type"))?;
        if ty == IrType::Nil || !(*hvalue(tv)).metatable.is_null() && (*val).tt == LUA_TNIL {
            let snap = self.b.snapshot();
            self.b.emit(Op::GuardPlainTable { t, store: false, snap }, IrType::Nil);
        }
        let buf = Box::leak(vec![0u64; 4].into_boxed_slice()).as_mut_ptr() as usize;
        let p = self.b.emit(Op::TGet { t, k, buf }, IrType::Ptr);
        let snap = self.b.snapshot();
        Ok(self.b.emit(Op::XLoad { p, snap }, ty))
    }

    /// `t[k] = v` raw for table slot `tslot` (no metatable, writable).
    unsafe fn record_tset(&mut self, tslot: usize, k: Ref, v: Ref, base: StkId) -> R<()> {
        let tv = base.add(tslot);
        if (*tv).tt != LUA_TTABLE {
            return Err(unsupported_type("indexing a non-table"));
        }
        let h = hvalue(tv);
        if (*h).readonly != 0 {
            return Err(unsupported_type("store into a read-only table"));
        }
        let t = self.read(tslot, base)?;
        self.mm_store_ok(Some(t))?;
        let snap = self.b.snapshot();
        let mt = (*h).metatable;
        if mt.is_null() {
            self.b.emit(Op::GuardPlainTable { t, store: true, snap }, IrType::Nil);
        } else {
            // a metatable without __newindex: a raw store (checked at run
            // time: the metatable is read, its __newindex must be absent)
            let name = (*(*self.l).global).tmname[crate::ltm::TM_NEWINDEX as usize];
            let ni = crate::ltable::luaH_getstr(mt, name);
            if ni != luaO_nilobject() && (*ni).tt != LUA_TNIL {
                return Err(unsupported_type("store into a table with __newindex"));
            }
            let mtr = self.b.emit(Op::TMeta { t, snap }, IrType::Table);
            let k = self.b.kptr(name as usize, IrType::String);
            self.b.emit(Op::HMiss { t: mtr, k, snap }, IrType::Nil);
        }
        let buf = Box::leak(vec![0u64; 4].into_boxed_slice()).as_mut_ptr() as usize;
        self.b.emit(Op::TSet { t, k, v, buf, snap }, IrType::Nil);
        self.inserts = true;
        Ok(())
    }

    /// FASTCALL1/2/2K/3 of any builtin that doesn't change tables or
    /// metatables: the trace calls Luau's own fast implementation.
    unsafe fn record_builtin(&mut self, pc: u32, insn: u32, aux: u32, base: StkId) -> R<()> {
        use luau_common::bytecode::BuiltinFunction as B;
        let op = op_of(insn);
        let bfid = a_of(insn) as u8;
        // (these have no Builtin instruction: recorded as calls of their
        // functions)
        let as_call = [B::Rawset, B::TableUnpack, B::TableClear].iter().any(|&b| b as u8 == bfid);
        if bfid == B::SelectVararg as u8 {
            return Err(AbortReason::UnsupportedOpcode(builtin_name(bfid)));
        }
        let writes = [B::TableInsert, B::BufferWriteU8, B::BufferWriteU16, B::BufferWriteU32, B::BufferWriteF32, B::BufferWriteF64, B::BufferWriteInteger];
        if !self.mm.is_empty() && writes.iter().any(|&b| b as u8 == bfid) {
            return Err(unsupported_type("side effect inside a metamethod"));
        }
        if bfid == B::TableInsert as u8 {
            self.inserts = true;
        }
        let code = (*self.cur_proto()).code;
        // where the replaced CALL is (skip counts from after the FASTCALL)
        let (len, skip) = if op == O::FastCall1 as u8 || op == O::FastCall as u8 { (1, c_of(insn)) } else { (2, c_of(insn) - 1) };
        let call_at = pc as usize + len + skip;
        let call = *code.add(call_at);
        let (ra, nres) = (a_of(call), c_of(call) as i32 - 1);
        let (cl, clr) = self.cur_closure();
        if (*(*cl).env).safeenv == 0 {
            return Err(unsupported_type("builtin in an unsafe environment"));
        }
        let mut args = [Ref(0); 3];
        let mut nargs = 0;
        if op == O::FastCall as u8 {
            // the CALL's own arguments, already in place
            nargs = self.call_args(ra, b_of(call))?;
            if nargs > 3 {
                return Err(unsupported_type("builtin with many arguments"));
            }
            for k in 0..nargs {
                args[k] = self.read(ra + 1 + k, base)?;
            }
        } else {
            args[0] = self.read(b_of(insn), base)?;
        }
        if op == O::FastCall as u8 {
        } else if op == O::FastCall1 as u8 {
            nargs = 1;
        } else if op == O::FastCall2 as u8 {
            args[1] = self.read(aux as usize, base)?;
            nargs = 2;
        } else if op == O::FastCall2K as u8 {
            args[1] = self.konst(aux as usize)?;
            nargs = 2;
        } else {
            args[1] = self.read((aux & 0xff) as usize, base)?;
            args[2] = self.read(((aux >> 8) & 0xff) as usize, base)?;
            nargs = 3;
        }
        if bfid == B::Vector as u8 && nres == 1 && (2..=3).contains(&nargs) && args[..nargs].iter().all(|a| self.b.trace.ty(*a) == IrType::Number) {
            // vector.create(x, y[, z]) of numbers: built in place (the
            // interpreter's builtin succeeds and skips the CALL)
            let z = if nargs == 3 { args[2] } else { self.b.knum(0.0) };
            let v = self.b.emit(Op::VMake { x: args[0], y: args[1], z }, IrType::Vector);
            self.b.set_slot(ra, v);
            self.pending = Some((None, 0, call_at as u32 + 1, false));
            return Ok(());
        }
        if nres > 1 || as_call {
            // several results: the call of the builtin's function (the
            // fallback code calls it the same way)
            builtin_closure(self.l, bfid).ok_or(unsupported_type("builtin without a function"))?;
            self.mm_store_ok(None)?;
            self.guard_safeenv(clr);
            self.deferred_call = Some(((bfid, ra, args, nargs), nres, call_at as u32 + 1));
            return Ok(());
        }
        if bfid == B::Setmetatable as u8 {
            self.mm_store_ok(Some(args[0]))?;
        }
        self.guard_safeenv(clr);
        let buf = Box::leak(vec![0u64; 8].into_boxed_slice()).as_mut_ptr() as usize;
        let snap = self.b.snapshot();
        // (the result's type is filled in at the next step, from the stack)
        let v = self.b.emit(Op::Builtin { bfid, args, nargs: nargs as u8, nres: if nres < 0 { MULTRET1 } else { nres as u8 }, buf, snap }, IrType::Nil);
        let next = call_at as u32 + 1;
        if nres != 0 {
            let prev = self.b.peek(ra);
            self.b.set_slot(ra, v);
            self.pending = Some((Some(v), self.b.off + ra, next, nres < 0));
            self.builtin_undo = Some((v, Some((ra, prev))));
            self.builtin_call = Some((bfid, ra, args, nargs));
        } else {
            self.pending = Some((None, 0, next, false));
            self.builtin_undo = Some((v, None));
        }
        Ok(())
    }

    /// Exits unless the environment of closure `cl` is still safe (its
    /// globals unchanged since the imports were resolved).
    unsafe fn guard_safeenv(&mut self, cl: Ref) {
        let p = self.b.emit(Op::ClEnv { cl }, IrType::Ptr);
        let snap = self.b.snapshot();
        self.b.emit(Op::GuardFlag { p, off: std::mem::offset_of!(LuaTable, safeenv) as i32, snap }, IrType::Nil);
    }

    /// CALL of a Lua function: inlined (the callee's instructions are
    /// recorded next, in a frame at a fixed slot offset).
    unsafe fn record_call(&mut self, pc: u32, insn: u32, base: StkId) -> R<()> {
        let (a, bb, c) = (a_of(insn), b_of(insn), c_of(insn));
        let fv = base.add(a);
        if (*fv).tt != LUA_TFUNCTION {
            return Err(unsupported_type("call of a non-function"));
        }
        let cl = clvalue(fv);
        if (*cl).isC != 0 {
            return self.record_c_call(pc, insn, base, cl);
        }
        let p = Closure::proto(cl);
        if self.funcs.len() >= MAX_INLINE_DEPTH {
            return Err(AbortReason::UnsupportedControlFlow);
        }
        let (nargs, nres) = (self.call_args(a, bb)?, c as i32 - 1);
        // the callee must stay a closure of this prototype (any closure:
        // its upvalues are read from it at run time)
        let f = self.read(a, base)?;
        let k = self.b.kptr(p as usize, IrType::Ptr);
        let snap = self.b.snapshot();
        self.b.emit(Op::GuardProto { f, p: k, snap }, IrType::Nil);
        if !self.protos.contains(&(p as usize)) {
            self.protos.push(p as usize);
        }
        let fb = self.b.off + a + 1;
        if fb + (*cl).stacksize as usize + LUA_MINSTACK as usize > STACK_WINDOW {
            return Err(AbortReason::UnsupportedControlFlow);
        }
        let ret_pc = pc + if op_of(insn) == O::CallFb as u8 { 2 } else { 1 };
        self.b.push_frame(SnapFrame { base: fb as u16, func: cl as usize, ret_pc, nresults: nres, nargs: nargs as u16, shift: 0, vararg: false });
        self.funcs.push((p, cl, Some(f)));
        // missing parameters are nil
        for i in nargs..(*p).numparams as usize {
            let nil = self.b.knil();
            self.b.set_slot(i, nil);
        }
        // (a vararg callee's frame moves up past its arguments: room)
        self.stack_need = self.stack_need.max(fb + nargs.max((*p).numparams as usize) + nargs + (*cl).stacksize as usize + LUA_MINSTACK as usize);
        Ok(())
    }

    /// The varargs of the current frame: (count, where: Some(first trace
    /// slot) for an inlined frame, None for the trace's own frame - below
    /// its base, the count checked at trace entry).
    unsafe fn varargs_of(&mut self, base: StkId) -> R<(usize, Option<usize>)> {
        let p = self.cur_proto();
        if (*p).is_vararg == 0 {
            return Err(AbortReason::Internal("varargs of a fixed-argument function".into()));
        }
        let np = (*p).numparams as usize;
        if self.funcs.len() == 1 {
            let func = (*(*self.l).ci).func;
            let n = base.offset_from(func) as usize - 1 - np;
            if self.side.is_none() {
                self.varargs = Some(n as u16);
            } else if self.varargs != Some(n as u16) && self.varargs.is_some() {
                return Err(AbortReason::Internal("vararg count changed".into()));
            }
            return Ok((n, None));
        }
        let f = self.b.frames.last().unwrap();
        if !f.vararg {
            return Err(AbortReason::Internal("varargs before PREPVARARGS".into()));
        }
        let n = (f.shift as usize).saturating_sub(np);
        Ok((n, Some(f.func_slot() + 1 + np)))
    }

    /// Vararg `j` (0-based, < count) of the current frame.
    unsafe fn vararg(&mut self, j: usize, n: usize, at: Option<usize>, base: StkId) -> R<Ref> {
        let tv = base.sub(n).add(j);
        let ty = IrType::of_tag((*tv).tt).ok_or(unsupported_type("vararg type"))?;
        if ty == IrType::LightUserdata {
            return Err(unsupported_type("light userdata"));
        }
        Ok(match at {
            Some(s) => {
                let saved = self.b.off;
                self.b.off = 0;
                let v = self.read(s + j, self.root_base);
                self.b.off = saved;
                v?
            }
            None => {
                let idx = self.b.ki32(j as i32);
                let snap = self.b.snapshot();
                self.b.emit(Op::VargLoad { idx, snap }, ty)
            }
        })
    }

    fn set_top(&mut self, top: Option<usize>) {
        self.top = top;
        self.b.set_top(top);
    }

    /// Arguments of a CALL with B at frame slot a: fixed, or up to the top
    /// a call just left.
    fn call_args(&mut self, a: usize, bb: usize) -> R<usize> {
        if bb != 0 {
            return Ok(bb - 1);
        }
        let top = self.top.ok_or(AbortReason::UnsupportedType("variable arguments without a known top"))?;
        self.set_top(None);
        top.checked_sub(self.b.off + a + 1).ok_or(AbortReason::Internal("stack top below the call".into()))
    }

    /// Arithmetic on a table with a metamethod (a Lua function), recorded
    /// as a call to it: its frame sits where `callTMluau` puts it (at the
    /// stack top), its one result goes to R(a). Exits inside resume at the
    /// arithmetic instruction (the interpreter re-runs the metamethod).
    unsafe fn record_arith_mm(&mut self, op: u8, a: usize, bb: usize, c: Option<usize>, kc: Option<usize>, base: StkId) -> R<()> {
        use crate::ltm::*;
        let o = |x: O| op == x as u8;
        let event = if o(O::Add) || o(O::AddK) {
            TM_ADD
        } else if o(O::Sub) || o(O::SubK) {
            TM_SUB
        } else if o(O::Mul) || o(O::MulK) {
            TM_MUL
        } else if o(O::Div) || o(O::DivK) {
            TM_DIV
        } else {
            TM_UNM
        };
        let rb = base.add(bb);
        let rc: *const TValue = match (c, kc) {
            (Some(c), _) => base.add(c),
            (None, Some(k)) => (*self.cur_proto()).k.add(k),
            _ => unreachable!(),
        };
        // the first operand's metamethod, else the second's
        let (owner, owner_is_b) = {
            let tb = luaT_gettmbyobj(self.l, rb, event);
            if !tb.is_null() && (*tb).tt != LUA_TNIL {
                (rb as *const TValue, true)
            } else if (*rb).tt == LUA_TNUMBER {
                (rc, false)
            } else {
                return Err(unsupported_type("arithmetic on a table without the metamethod"));
            }
        };
        if (*owner).tt != LUA_TTABLE {
            return Err(unsupported_type("metamethod of a non-table"));
        }
        let fv = luaT_gettmbyobj(self.l, owner, event);
        if fv.is_null() || (*fv).tt != LUA_TFUNCTION || (*clvalue(fv)).isC != 0 {
            return Err(unsupported_type("metamethod is not a Lua function"));
        }
        let cl = clvalue(fv);
        let p = Closure::proto(cl);
        if (*p).is_vararg != 0 || self.funcs.len() >= MAX_INLINE_DEPTH {
            return Err(unsupported_type("metamethod call"));
        }
        // operands (tag guards pin the types: the metamethod choice)
        let x = self.read(bb, base)?;
        let y = match (c, kc) {
            (Some(c), _) => self.read(c, base)?,
            (None, Some(k)) => self.konst(k)?,
            _ => unreachable!(),
        };
        let own = if owner_is_b { x } else { y };
        // the metamethod: from the owner's metatable at run time, guarded
        // by prototype
        let resume = self.b.snapshot();
        let mt = self.b.emit(Op::TMeta { t: own, snap: resume }, IrType::Table);
        let mth = (*hvalue(owner)).metatable;
        let name = (*(*self.l).global).tmname[event as usize];
        let kname = self.b.kptr(name as usize, IrType::String);
        let (fp, _) = self.field_ref_str(mth, mt, name, kname, false)?;
        let f = self.b.emit(Op::XLoad { p: fp, snap: resume }, IrType::Function);
        let kp = self.b.kptr(p as usize, IrType::Ptr);
        self.b.emit(Op::GuardProto { f, p: kp, snap: resume }, IrType::Nil);
        if !self.protos.contains(&(p as usize)) {
            self.protos.push(p as usize);
        }
        let result = self.b.off + a;
        self.enter_mm(cl, f, x, y, result, resume)
    }

    /// Enters metamethod `cl` (the IR value `f`, guarded) called with `x`,
    /// `y` as `callTMres` does: its frame at the stack top; `result`: see
    /// `mm`; exits inside resume through `resume`.
    unsafe fn enter_mm(&mut self, cl: *mut Closure, f: Ref, x: Ref, y: Ref, result: usize, resume: SnapId) -> R<()> {
        let p = Closure::proto(cl);
        // the frame: function at the stack top, then the two operands
        let top = ((*self.l).top as usize - self.root_base as usize) / std::mem::size_of::<TValue>();
        let fb = top + 1;
        if fb + 2 + (*cl).stacksize as usize + LUA_MINSTACK as usize > STACK_WINDOW {
            return Err(AbortReason::UnsupportedControlFlow);
        }
        let saved = self.b.off;
        self.b.off = 0;
        self.b.set_slot(top, f);
        self.b.set_slot(fb, x);
        self.b.set_slot(fb + 1, y);
        self.b.off = saved;
        self.b.push_frame(SnapFrame { base: fb as u16, func: cl as usize, ret_pc: 0, nresults: 1, nargs: 2, shift: 0, vararg: false });
        self.funcs.push((p, cl, Some(f)));
        for i in 2..(*p).numparams as usize {
            let nil = self.b.knil();
            self.b.set_slot(i, nil);
        }
        self.stack_need = self.stack_need.max(fb + 2 + (*cl).stacksize as usize + LUA_MINSTACK as usize);
        self.mm.push((self.funcs.len() - 1, result, resume));
        self.b.forced = Some(resume);
        Ok(())
    }

    /// A comparison of the tables in slots `ra`, `rb` (`kind` 0: `<`, 1:
    /// `<=`, 2: `==`) through a Lua metamethod, as `luaV_lessthan` /
    /// `luaV_lessequal` / `luaV_equalval` pick it: recorded as a call (the
    /// jump then follows the result's truthiness, guarded). False when no
    /// metamethod applies (`==` only: identity, with the guards keeping it
    /// that way emitted here).
    unsafe fn record_cmp_mm(&mut self, kind: u8, ra: usize, rb: usize, base: StkId) -> R<bool> {
        use crate::ltm::*;
        let (lv, rv) = (base.add(ra) as *const TValue, base.add(rb) as *const TValue);
        let (ml, mr) = ((*hvalue(lv)).metatable, (*hvalue(rv)).metatable);
        let g = (*self.l).global;
        let tmname = move |e: TMS| (*g).tmname[e as usize];
        let has = |mt: *mut LuaTable, e: TMS| -> *const TValue {
            if mt.is_null() {
                return std::ptr::null();
            }
            let v = crate::ltable::luaH_getstr(mt, tmname(e));
            if v == luaO_nilobject() || (*v).tt == LUA_TNIL { std::ptr::null() } else { v }
        };
        let x = self.read(ra, base)?;
        let y = self.read(rb, base)?;
        let resume = self.b.snapshot();
        // (event, operands swapped: `a <= b` as `not (b < a)`)
        let (event, swap) = match kind {
            2 => {
                let tm1 = has(ml, TM_EQ);
                if tm1.is_null() {
                    // the left operand has no __eq: nothing applies
                    self.guard_no_tm(x, ml, TM_EQ, resume)?;
                    return Ok(false);
                }
                let tm2 = if ml == mr { tm1 } else { has(mr, TM_EQ) };
                if tm2.is_null() || !luaO_rawequalObj(tm1, tm2) {
                    return Err(unsupported_type("tables with different __eq"));
                }
                (TM_EQ, false)
            }
            0 => (TM_LT, false),
            _ => {
                let le = has(ml, TM_LE);
                if !le.is_null() {
                    (TM_LE, false)
                } else {
                    // no __le on the left: `not (b < a)`
                    self.guard_no_tm(x, ml, TM_LE, resume)?;
                    (TM_LT, true)
                }
            }
        };
        let (h1, h2, o1, o2) = if swap { (rv, lv, y, x) } else { (lv, rv, x, y) };
        let (m1, m2) = ((*hvalue(h1)).metatable, (*hvalue(h2)).metatable);
        let tm1 = has(m1, event);
        let tm2 = has(m2, event);
        if tm1.is_null() || tm2.is_null() || !luaO_rawequalObj(tm1, tm2) {
            return Err(unsupported_type("comparison of tables the interpreter rejects"));
        }
        if (*tm1).tt != LUA_TFUNCTION || (*clvalue(tm1)).isC != 0 {
            return Err(unsupported_type("metamethod is not a Lua function"));
        }
        let cl = clvalue(tm1);
        if (*Closure::proto(cl)).is_vararg != 0 || self.funcs.len() >= MAX_INLINE_DEPTH {
            return Err(unsupported_type("metamethod call"));
        }
        // the metamethod, from the first operand's metatable (guarded by
        // prototype); the second's must be the same
        let name = tmname(event);
        let kname = self.b.kptr(name as usize, IrType::String);
        let mt1 = self.b.emit(Op::TMeta { t: o1, snap: resume }, IrType::Table);
        let (fp, _) = self.field_ref_str(m1, mt1, name, kname, false)?;
        let f = self.b.emit(Op::XLoad { p: fp, snap: resume }, IrType::Function);
        let p = Closure::proto(cl);
        let kp = self.b.kptr(p as usize, IrType::Ptr);
        self.b.emit(Op::GuardProto { f, p: kp, snap: resume }, IrType::Nil);
        let mt2 = self.b.emit(Op::TMeta { t: o2, snap: resume }, IrType::Table);
        if m1 == m2 {
            self.b.emit(Op::GuardSame { a: mt2, k: mt1, snap: resume }, IrType::Nil);
        } else {
            let (fp2, _) = self.field_ref_str(m2, mt2, name, kname, false)?;
            let f2 = self.b.emit(Op::XLoad { p: fp2, snap: resume }, IrType::Function);
            self.b.emit(Op::GuardSame { a: f2, k: f, snap: resume }, IrType::Nil);
        }
        if !self.protos.contains(&(p as usize)) {
            self.protos.push(p as usize);
        }
        self.enter_mm(cl, f, o1, o2, CMP_RESULT, resume)?;
        Ok(true)
    }

    /// Exits through `snap` unless table `t` (whose metatable is `mt` now)
    /// has no metamethod `event`.
    unsafe fn guard_no_tm(&mut self, t: Ref, mt: *mut LuaTable, event: crate::ltm::TMS, snap: SnapId) -> R<()> {
        if mt.is_null() {
            self.b.emit(Op::GuardPlainTable { t, store: false, snap }, IrType::Nil);
        } else {
            let name = (*(*self.l).global).tmname[event as usize];
            let k = self.b.kptr(name as usize, IrType::String);
            let m = self.b.emit(Op::TMeta { t, snap }, IrType::Table);
            self.b.emit(Op::HMiss { t: m, k, snap }, IrType::Nil);
        }
        Ok(())
    }

    /// NEWCLOSURE / DUPCLOSURE. A local captured by reference gets an open
    /// upvalue on its stack slot (the trace stores the slot's value there
    /// first; calls of the closure in the trace use the trace's value of
    /// the slot - `GuardUvSlot` - and CLOSEUPVALS stores it again).
    unsafe fn record_closure(&mut self, pc: u32, insn: u32, base: StkId) -> R<()> {
        let a = a_of(insn);
        let proto = self.cur_proto();
        let dup = op_of(insn) == O::DupClosure as u8;
        let (target, nups) = if dup {
            let kv = (*proto).k.add(d_of(insn) as usize);
            if (*kv).tt != LUA_TFUNCTION {
                return Err(unsupported_type("DUPCLOSURE of a non-function"));
            }
            let kcl = clvalue(kv);
            (kcl as usize, (*kcl).nupvalues as usize)
        } else {
            let pv = *(*proto).p.add(d_of(insn) as usize);
            (pv as usize, (*pv).nups as usize)
        };
        let code = (*proto).code;
        let mut caps = Vec::with_capacity(nups);
        let mut vals = [Ref(0); 8];
        let mut nvals = 0usize;
        for ui in 0..nups {
            let uinsn = *code.add(pc as usize + 1 + ui);
            let (kind, idx) = (a_of(uinsn) as u8, b_of(uinsn));
            let by_ref = kind as u32 == crate::lvmexecute::LCT_REF;
            if by_ref && dup {
                return Err(unsupported_type("DUPCLOSURE capturing by reference"));
            }
            if kind as u32 == crate::lvmexecute::LCT_VAL || by_ref {
                if nvals == vals.len() {
                    return Err(unsupported_type("closure with many captured values"));
                }
                vals[nvals] = self.read(idx, base)?;
                caps.push(Capture { kind, idx: nvals as u8, slot: (self.b.off + idx) as u16 });
                nvals += 1;
            } else {
                caps.push(Capture { kind, idx: idx as u8, slot: 0 });
            }
        }
        let (_, clr) = self.cur_closure();
        let site = Box::leak(Box::new(ClosureSite { dup, target, caps })) as *const ClosureSite as usize;
        let buf = Box::leak(vec![0u64; 16].into_boxed_slice()).as_mut_ptr() as usize;
        let v = self.b.emit(Op::MakeClosure { cl: clr, site, vals, nvals: nvals as u8, buf }, IrType::Function);
        if !self.mm.is_empty() {
            self.fresh.push(v);
        }
        self.b.set_slot(a, v);
        Ok(())
    }

    /// Inside a metamethod only tables created there may change (an exit
    /// re-runs it from the start).
    fn mm_store_ok(&self, t: Option<Ref>) -> R<()> {
        if self.mm.is_empty() {
            return Ok(());
        }
        match t {
            Some(t) if self.fresh.contains(&t) => Ok(()),
            _ => Err(unsupported_type("side effect inside a metamethod")),
        }
    }

    /// CALL of a C function that is a builtin with a fast implementation
    /// (`string.sub` through a method call, `math.sin` through a local, ...).
    unsafe fn record_c_call(&mut self, pc: u32, insn: u32, base: StkId, cl: *mut Closure) -> R<()> {
        let (a, bb, c) = (a_of(insn), b_of(insn), c_of(insn));
        let Some(fp) = Closure::c(cl).f.map(|f| f as usize) else {
            return Err(AbortReason::UnsupportedOpcode("Call (C function)"));
        };
        if get_by_name((*self.l).gt, b"select").is_some_and(|v| (*v).tt == LUA_TFUNCTION && (*clvalue(v)).isC != 0 && Closure::c(clvalue(v)).f.map(|f| f as usize) == Some(fp)) {
            return self.record_select(insn, base, fp);
        }
        let Some(bfid) = builtin_of(self.l, fp) else {
            return self.record_leaf_c_call(pc, insn, base, fp);
        };
        let nargs = self.call_args(a, bb)?;
        if nargs > 3 || c > 2 {
            // (the builtin returns one value: anything else, the call)
            let f = self.read(a, base)?;
            let snap = self.b.snapshot();
            self.b.emit(Op::GuardCFunc { f, fp, snap }, IrType::Nil);
            let ret_pc = pc + if op_of(insn) == O::CallFb as u8 { 2 } else { 1 };
            return self.record_callback(a, nargs, c as i32 - 1, ret_pc, base);
        }
        let nres = c as i32 - 1;
        let f = self.read(a, base)?;
        let snap = self.b.snapshot();
        self.b.emit(Op::GuardCFunc { f, fp, snap }, IrType::Nil);
        let mut args = [Ref(0); 3];
        for k in 0..nargs {
            args[k] = self.read(a + 1 + k, base)?;
        }
        let buf = Box::leak(vec![0u64; 8].into_boxed_slice()).as_mut_ptr() as usize;
        let snap = self.b.snapshot();
        if bfid == luau_common::bytecode::BuiltinFunction::TableInsert as u8 {
            self.inserts = true;
        }
        let v = self.b.emit(Op::Builtin { bfid, args, nargs: nargs as u8, nres: if nres < 0 { MULTRET1 } else { nres as u8 }, buf, snap }, IrType::Nil);
        let next = pc + if op_of(insn) == O::CallFb as u8 { 2 } else { 1 };
        if nres != 0 {
            self.b.set_slot(a, v);
            self.pending = Some((Some(v), self.b.off + a, next, nres < 0));
            // (several results: recorded as the plain call instead)
            self.builtin_undo = Some((v, None));
            self.builtin_call = Some((bfid, a, args, nargs));
        } else {
            self.pending = Some((None, 0, next, false));
        }
        Ok(())
    }

    /// CALL of `select` (`luaB_select`): its results are some of its
    /// arguments (or their count), known to the trace without a call.
    unsafe fn record_select(&mut self, insn: u32, base: StkId, fp: usize) -> R<()> {
        let (a, bb, c) = (a_of(insn), b_of(insn), c_of(insn));
        let nargs = self.call_args(a, bb)?;
        if nargs == 0 {
            return Err(unsupported_type("select without arguments"));
        }
        let f = self.read(a, base)?;
        let snap = self.b.snapshot();
        self.b.emit(Op::GuardCFunc { f, fp, snap }, IrType::Nil);
        let sel = base.add(a + 1);
        let x = self.read(a + 1, base)?;
        let n = nargs as i64;
        let results: Vec<Ref> = if (*sel).tt == LUA_TSTRING && *getstr(tsvalue(sel)) == b'#' {
            if !matches!(self.b.trace.inst(x).op, Op::KPtr(_)) {
                let k = self.gc_const((*sel).value.gc as usize, IrType::String);
                self.b.emit(Op::GuardSame { a: x, k, snap }, IrType::Nil);
            }
            vec![self.b.knum((n - 1) as f64)]
        } else if (*sel).tt == LUA_TNUMBER && (*sel).value.n == (*sel).value.n.trunc() {
            let mut i = (*sel).value.n as i64;
            let k = self.b.knum((*sel).value.n);
            self.b.emit(Op::Guard { cmp: Cmp::Eq, a: x, b: k, snap }, IrType::Nil);
            if i < 0 {
                i += n;
            } else if i > n {
                i = n;
            }
            if i < 1 {
                return Err(unsupported_type("select index out of range"));
            }
            let mut v = Vec::new();
            for j in i as usize..nargs {
                v.push(self.read(a + 1 + j, base)?);
            }
            v
        } else {
            return Err(unsupported_type("select with this argument"));
        };
        let want = c as i32 - 1;
        let count = if want < 0 { results.len() } else { want as usize };
        for j in 0..count {
            let v = results.get(j).copied().unwrap_or_else(|| self.b.knil());
            self.b.set_slot(a + j, v);
        }
        // (slots above the results were the call's: dead now)
        for s in a + count..a + 1 + nargs {
            self.b.forget_slot(s);
        }
        if want < 0 {
            self.set_top(Some(self.b.off + a + count));
        }
        Ok(())
    }

    /// CALL of a leaf C function (see `leaf_c`): the trace calls it directly.
    unsafe fn record_leaf_c_call(&mut self, pc: u32, insn: u32, base: StkId, fp: usize) -> R<()> {
        let (a, bb, c) = (a_of(insn), b_of(insn), c_of(insn));
        self.mm_store_ok(None)?;
        let Some(table_arg) = leaf_c(self.l, fp) else {
            // any other C function: called as the interpreter does (it may
            // call Lua)
            let nargs = self.call_args(a, bb)?;
            let f = self.read(a, base)?;
            let snap = self.b.snapshot();
            self.b.emit(Op::GuardCFunc { f, fp, snap }, IrType::Nil);
            let ret_pc = pc + if op_of(insn) == O::CallFb as u8 { 2 } else { 1 };
            return self.record_callback(a, nargs, c as i32 - 1, ret_pc, base);
        };
        if c == 0 || c > 4 {
            // a variable number of results (or many): the general call
            let nargs = self.call_args(a, bb)?;
            let f = self.read(a, base)?;
            let snap = self.b.snapshot();
            self.b.emit(Op::GuardCFunc { f, fp, snap }, IrType::Nil);
            let ret_pc = pc + if op_of(insn) == O::CallFb as u8 { 2 } else { 1 };
            return self.record_callback(a, nargs, c as i32 - 1, ret_pc, base);
        }
        let nargs = self.call_args(a, bb)?;
        let nres = c - 1;
        let f = self.read(a, base)?;
        let snap = self.b.snapshot();
        self.b.emit(Op::GuardCFunc { f, fp, snap }, IrType::Nil);
        for k in 0..nargs {
            let tv = base.add(a + 1 + k);
            let ok = matches!((*tv).tt, LUA_TNUMBER | LUA_TSTRING | LUA_TBOOLEAN | LUA_TNIL | LUA_TVECTOR) || (k == 0 && table_arg && (*tv).tt == LUA_TTABLE);
            if !ok {
                // (other types could run metamethods: Lua code)
                return Err(unsupported_type("C call argument type"));
            }
            // the tag guards keep the argument types
            self.read(a + 1 + k, base)?;
        }
        let ret_pc = pc + if op_of(insn) == O::CallFb as u8 { 2 } else { 1 };
        let snap = self.b.snapshot();
        let (savedpc, frames) = self.call_context(ret_pc);
        self.b.emit(Op::CCall { slot: (self.b.off + a) as u16, nargs: nargs as u8, nres: nres as u8, savedpc, frames, off: self.b.off as u16, snap }, IrType::Nil);
        // the call left everything from its function slot up undefined
        for s in a..a + 1 + nargs.max(nres) {
            self.b.forget_slot(s);
        }
        self.pending_c = Some((a, nres, ret_pc));
        Ok(())
    }

    /// A FASTCALL that returned several values (the interpreter is past its
    /// CALL, `pc`): its Builtin instruction `inst` becomes the CALL of the
    /// builtin's function (found by name, as the fallback code loads it).
    unsafe fn builtin_as_call(&mut self, inst: Option<Ref>, (bfid, ra, args, nargs): (u8, usize, [Ref; 3], usize), nres: i32, pc: u32, base: StkId) -> R<()> {
        let cl = builtin_closure(self.l, bfid).ok_or(unsupported_type("builtin without a function"))?;
        if let Some(inst) = inst {
            self.b.trace.insts[inst.index()].op = Op::Nop;
        }
        let f = self.gc_const(cl as usize, IrType::Function);
        self.b.set_slot(ra, f);
        for k in 0..nargs {
            self.b.set_slot(ra + 1 + k, args[k]);
        }
        let snap = self.b.snapshot();
        let (savedpc, frames) = self.call_context(pc);
        let cnt = self.b.emit(Op::CallC { slot: (self.b.off + ra) as u16, nargs: nargs as u8, nres: nres as i16, savedpc, frames, off: self.b.off as u16, snap }, IrType::I32);
        self.inserts = true;
        self.ustores = true;
        for s in ra..ra + 1 + nargs {
            self.b.forget_slot(s);
        }
        self.b.pc = pc;
        self.after_callback(ra, nres, cnt, base)
    }

    /// A call of a C function that may run Lua code (its function is in
    /// frame slot `a`, guarded, `nargs` arguments above it, `nres` results
    /// wanted, -1: all). Everything the trace knows about memory and slots
    /// is read again after it (`after_callback`).
    unsafe fn record_callback(&mut self, a: usize, nargs: usize, nres: i32, ret_pc: u32, base: StkId) -> R<()> {
        self.mm_store_ok(None)?;
        if nargs > 250 {
            return Err(unsupported_type("C call with many arguments"));
        }
        for k in 0..nargs {
            // (their values go to the stack with the snapshot)
            self.read(a + 1 + k, base)?;
        }
        self.emit_callc(a, nargs, nres, ret_pc)
    }

    /// `record_callback` once the function and arguments are in their
    /// slots.
    unsafe fn emit_callc(&mut self, a: usize, nargs: usize, nres: i32, ret_pc: u32) -> R<()> {
        self.mm_store_ok(None)?;
        let snap = self.b.snapshot();
        let (savedpc, frames) = self.call_context(ret_pc);
        let cnt = self.b.emit(Op::CallC { slot: (self.b.off + a) as u16, nargs: nargs as u8, nres: nres as i16, savedpc, frames, off: self.b.off as u16, snap }, IrType::I32);
        self.inserts = true;
        self.ustores = true;
        for s in a..a + 1 + nargs {
            self.b.forget_slot(s);
        }
        let depth = (*self.l).ci.offset_from((*self.l).base_ci);
        self.callback = Some((a, nres, ret_pc, depth, cnt));
        Ok(())
    }

    /// After a recorded C call that may have run Lua code: every slot the
    /// trace knows is read again (Lua code can change the frame's locals
    /// through upvalues); the results, as many as there were (guarded).
    unsafe fn after_callback(&mut self, a: usize, nres: i32, cnt: Ref, base: StkId) -> R<()> {
        let off = self.b.off;
        self.b.reload = true;
        // (the stack has every value: they were stored before the call)
        let mut slots = self.b.known_slots();
        let saved = self.b.off;
        self.b.off = 0;
        for &s in &slots {
            self.b.forget_slot(s);
        }
        self.b.off = saved;
        let count = if nres < 0 { (*self.l).top.offset_from(base.add(a)) as usize } else { nres as usize };
        if nres < 0 {
            // a different number of results exits right after the call,
            // L->top where the call left it
            self.b.set_top(Some(KEEP_TOP as usize));
            let snap = self.b.snapshot();
            let k = self.b.ki32(count as i32);
            self.b.emit(Op::Guard { cmp: Cmp::Eq, a: cnt, b: k, snap }, IrType::Nil);
            self.set_top(Some(off + a + count));
        }
        for j in 0..count {
            slots.push(off + a + j);
        }
        slots.sort_unstable();
        slots.dedup();
        self.b.reload = true;
        let saved = self.b.off;
        self.b.off = 0;
        for s in slots {
            let tv = self.root_base.add(s);
            let Some(ty) = IrType::of_tag((*tv).tt) else {
                self.b.off = saved;
                return Err(unsupported_type("slot type after a C call"));
            };
            if ty == IrType::LightUserdata && (*tv).extra[0] != LU_TAG_ITERATOR {
                self.b.off = saved;
                return Err(unsupported_type("light userdata"));
            }
            let snap = self.b.snapshot();
            let v = self.b.emit(Op::SReload { slot: s as u16, snap }, ty);
            self.b.set_slot(s, v);
        }
        self.b.off = saved;
        Ok(())
    }

    /// For a call out of the trace at bytecode index `pc` of the current
    /// function: the instruction pointer to report, and the inlined frames
    /// to materialize around it (a leaked Vec, 0 at the trace's own frame:
    /// errors and tracebacks then see the right functions and lines).
    fn call_context(&self, pc: u32) -> (usize, usize) {
        let savedpc = unsafe { (*self.cur_proto()).code.add(pc as usize) } as usize;
        let frames = if self.b.frames.is_empty() { 0 } else { Box::leak(Box::new(self.b.frames.clone())) as *const Vec<SnapFrame> as usize };
        (savedpc, frames)
    }

    /// RETURN from an inlined frame: its results go to the caller's slots.
    unsafe fn record_return(&mut self, insn: u32, base: StkId) -> R<()> {
        if self.funcs.len() == 1 {
            // returning from the loop's function: the loop is left
            return Err(AbortReason::LeftLoop);
        }
        let (a, bb) = (a_of(insn), b_of(insn));
        let nret = if bb != 0 {
            bb - 1
        } else {
            let top = self.top.ok_or(AbortReason::UnsupportedType("variable results without a known top"))?;
            self.set_top(None);
            top.checked_sub(self.b.off + a).ok_or(AbortReason::Internal("stack top below the results".into()))?
        };
        let want = self.b.frames.last().unwrap().nresults;
        // (a caller taking every result gets exactly these, up to the top)
        let nres = if want < 0 { nret } else { want as usize };
        let mut vals = Vec::with_capacity(nres);
        for i in 0..nres {
            vals.push(if i < nret { self.read(a + i, base)? } else { self.b.knil() });
        }
        if let Some(&(depth, result, resume)) = self.mm.last() {
            if depth == self.funcs.len() - 1 {
                // a metamethod returning: its result goes to the
                // instruction's target, as callTMres does
                let v = if nret > 0 { self.read(a, base)? } else { self.b.knil() };
                let truthy = nret > 0 && !((*base.add(a)).tt == LUA_TNIL || ((*base.add(a)).tt == LUA_TBOOLEAN && (*base.add(a)).value.b == 0));
                self.b.pop_frame().unwrap();
                self.funcs.pop();
                self.mm.pop();
                self.b.forced = self.mm.last().map(|m| m.2);
                if result == CMP_RESULT {
                    // a comparison: the jump follows the result's truthiness
                    if matches!(self.b.trace.ty(v), IrType::Nil | IrType::Boolean) {
                        self.b.emit(Op::GuardTruthy { a: v, want: truthy, snap: resume }, IrType::Nil);
                    }
                    return Ok(());
                }
                let saved = self.b.off;
                self.b.off = 0;
                self.b.set_slot(result, v);
                self.b.off = saved;
                return Ok(());
            }
        }
        let f = self.b.pop_frame().unwrap();
        self.funcs.pop();
        // results start at the caller's function slot
        let dst = f.func_slot() - self.b.off;
        for (i, v) in vals.into_iter().enumerate() {
            self.b.set_slot(dst + i, v);
        }
        if want < 0 {
            self.set_top(Some(self.b.off + dst + nres));
        }
        Ok(())
    }

    /// Which slots of the trace's frame are live at the loop header (None:
    /// unknown).
    fn live_at_header(&self) -> Option<Vec<bool>> {
        #[cfg(all(target_arch = "x86_64", windows))]
        unsafe {
            let p = self.proto;
            let n = (*p).sizecode as usize;
            let code = (*p).code;
            let mut starts = vec![false; n];
            let mut i = 0;
            while i < n {
                starts[i] = true;
                i += op_len(op_of(*code.add(i)));
            }
            let live = crate::codegen::live::analyze(code, n, &starts, &[])?;
            let m = live[self.start_pc as usize];
            Some((0..256).map(|r| crate::codegen::live::has(&m, r)).collect())
        }
        #[cfg(not(all(target_arch = "x86_64", windows)))]
        None
    }

    // ---- closing the loop ------------------------------------------------

    /// Turns the recorded iteration into `pre-roll; LOOP; PHIs; body`
    /// (with loop-invariant work hoisted into the pre-roll, and a version
    /// without as the fallback).
    fn close(&mut self) -> R<Trace> {
        let rec = std::mem::replace(&mut self.b.trace, Trace::new(TraceId(0), std::ptr::null_mut(), 0));
        // (a trace that may move table storage keeps every table access in
        // the loop body)
        let (mut t, hoisted) = self.close_with(&rec, !self.inserts)?;
        t.inserts = self.inserts;
        if hoisted {
            let mut fb = self.close_with(&rec, false)?.0;
            fb.inserts = self.inserts;
            t.fallback = Some(Box::new(fb));
        }
        Ok(t)
    }

    /// Finishes a side trace at the loop header: linear code whose last
    /// snapshot (every slot it wrote, resuming at the header) is what it
    /// stores before jumping back into the root trace.
    fn close_side(&mut self) -> R<Trace> {
        let (header, parent) = self.side.unwrap();
        let mut out = std::mem::replace(&mut self.b.trace, Trace::new(TraceId(0), std::ptr::null_mut(), 0));
        let entries: Vec<SnapEntry> = self.b.written().into_iter().map(|(s, v)| SnapEntry { slot: s as u16, value: v }).collect();
        out.snapshots.push(Snapshot { pc: header, entries, ref_limit: Ref(out.insts.len() as u32), frames: vec![], top: None, stack_current: false });
        out.start_pc = header;
        out.end_pc = self.loop_end;
        out.loop_ref = None;
        out.side_of = Some(parent);
        out.varargs = self.varargs;
        out.inserts = self.inserts;
        out.protos = self.protos.clone();
        out.stack_need = self.stack_need as u32;
        super::ir::validate(&out).map_err(AbortReason::Internal)?;
        Ok(out)
    }

    /// Instructions computing the same value on every iteration when their
    /// operands do (`HRef` is hoisted as a check: the body uses `HSlot`).
    fn hoistable(inst: &Inst) -> bool {
        (inst.is_pure() && !inst.is_const())
            || matches!(
            inst.op,
            Op::FrameFunc
                | Op::ClEnv { .. }
                | Op::HRef { .. }
                | Op::GuardProto { .. }
                | Op::GuardFlag { .. }
                | Op::ULoad { .. }
                | Op::GuardUvSlot { .. }
                | Op::Guard { .. }
                | Op::GuardTruthy { .. }
                | Op::GuardSame { .. }
                | Op::GuardDiff { .. }
                | Op::NumToI32 { .. }
                | Op::ASize(_)
        )
    }

    fn close_with(&self, rec: &Trace, hoist: bool) -> R<(Trace, bool)> {
        let written: BTreeMap<usize, Ref> = self.b.written().into_iter().collect();
        let n = rec.insts.len();

        // which slots each recorded SLoad reads
        let mut loads: BTreeMap<usize, Ref> = BTreeMap::new();
        for (i, inst) in rec.insts.iter().enumerate() {
            if let Op::SLoad { slot, .. } = inst.op {
                loads.insert(slot as usize, Ref(i as u32));
            }
        }
        // loop-carried values: every slot the iteration writes. A slot that
        // is also read gets its recorded SLoad as the PHI's pre; a slot the
        // body writes without reading first still needs one: an exit before
        // the write (e.g. a break out of the loop) must restore the
        // previous iteration's value, not the trace-entry value - for those
        // an entry SLoad is synthesized in the pre-roll below
        // (a slot dead at the loop header - a temporary - needs no
        // carrying: no path from the header reads it before writing it, so
        // no exit before its write can need its old value either)
        let live = self.live_at_header();
        let written_missing: Vec<(usize, Ref)> = written
            .iter()
            .filter(|(s, _)| !loads.contains_key(s) && live.as_ref().is_none_or(|m| m[**s]))
            .map(|(&s, &w)| (s, w))
            .collect();

        let mut out = Trace::new(rec.id, rec.proto, rec.start_pc);
        out.varargs = self.varargs;
        out.end_pc = self.loop_end;
        out.protos = self.protos.clone();
        out.stack_need = self.stack_need as u32;
        out.gc_refs = self.gc_refs.clone();
        let mut map: Vec<Option<Ref>> = vec![None; n];
        let push = |out: &mut Trace, inst: Inst| -> Ref {
            out.insts.push(inst);
            Ref(out.insts.len() as u32 - 1)
        };

        // entry snapshot: resume at the loop header, nothing modified yet
        out.snapshots.push(Snapshot { pc: rec.start_pc, entries: vec![], ref_limit: Ref(0), frames: vec![], top: None, stack_current: false });
        let entry = SnapId(0);

        // pre-roll: slot loads. Constants stay in the body so their
        // registers are only live within an iteration (they are redefined
        // every time, which is cheap and avoids keeping them live to END).
        for (i, inst) in rec.insts.iter().enumerate() {
            let op = match &inst.op {
                Op::SLoad { slot, guarded, .. } => Op::SLoad { slot: *slot, guarded: *guarded, snap: entry },
                _ => continue,
            };
            map[i] = Some(push(&mut out, Inst { op, ty: inst.ty }));
        }
        // entry values for the write-only carried slots (typed by the value
        // the body writes: the guard only has to hold for the same tag)
        let mut synth: HashMap<usize, Ref> = HashMap::new();
        for &(s, w) in &written_missing {
            let r = push(&mut out, Inst { op: Op::SLoad { slot: s as u16, guarded: true, snap: entry }, ty: rec.ty(w) });
            synth.insert(s, r);
        }
        // loop-invariant instructions: operands are entry loads of slots the
        // loop doesn't write, constants or hoisted values. They run once,
        // before the loop, exiting through the entry snapshot.
        let mut hoisted_href = vec![false; n];
        let mut any_hoisted = false;
        // (recorded load, its pre-loop value, the stored value it gets back)
        let mut mem_phis: Vec<(usize, Ref, Ref)> = Vec::new();
        if hoist {
            let carried: std::collections::HashSet<Ref> = written.values().copied().collect();
            let mut pre: Vec<Option<Ref>> = vec![None; n];
            for (&s, &ld) in &loads {
                if !written.contains_key(&s) {
                    pre[ld.index()] = map[ld.index()];
                }
            }
            for (i, inst) in rec.insts.iter().enumerate() {
                if !Self::hoistable(inst) || carried.contains(&Ref(i as u32)) || (self.ustores && matches!(inst.op, Op::ULoad { .. })) {
                    continue;
                }
                let ops = inst.operands();
                if !ops.iter().all(|o| pre[o.index()].is_some() || rec.inst(*o).is_const()) {
                    continue;
                }
                for o in ops {
                    if pre[o.index()].is_none() {
                        // a constant: copied before the loop
                        pre[o.index()] = Some(push(&mut out, rec.inst(o).clone()));
                    }
                }
                let mut op = inst.op.clone();
                remap_operands(&mut op, &|r: Ref| pre[r.index()].unwrap());
                set_snapshot(&mut op, entry);
                let r = push(&mut out, Inst { op, ty: inst.ty });
                pre[i] = Some(r);
                any_hoisted = true;
                if matches!(inst.op, Op::HRef { .. }) {
                    // checked here; the body takes the slot's address
                    hoisted_href[i] = true;
                } else {
                    map[i] = Some(r);
                }
            }
            // memory at loop-invariant addresses (LuaJIT's loop forwarding):
            // a load no store in the loop may alias is done once, before the
            // loop; a load of a location the loop stores to (and nothing
            // else that may alias it), before that store in the iteration,
            // becomes a PHI of the value loaded before the loop and the
            // value the iteration stores
            let stores: Vec<(usize, Ref, Ref)> = rec
                .insts
                .iter()
                .enumerate()
                .filter_map(|(j, x)| match x.op {
                    Op::XStore { p, v } => Some((j, p, v)),
                    _ => None,
                })
                .collect();
            for (i, inst) in rec.insts.iter().enumerate() {
                let Op::XLoad { p, .. } = inst.op else { continue };
                let Some(pp) = pre[p.index()] else { continue };
                if map[i].is_some() {
                    continue;
                }
                let m = rec_mem(rec, p);
                let aliasing: Vec<&(usize, Ref, Ref)> = stores.iter().filter(|s| rec_may_alias(m, rec_mem(rec, s.1))).collect();
                let same = |q: Ref| rec_key(rec, q).is_some() && rec_key(rec, q) == rec_key(rec, p);
                if aliasing.is_empty() {
                    let r = push(&mut out, Inst { op: Op::XLoad { p: pp, snap: entry }, ty: inst.ty });
                    map[i] = Some(r);
                    pre[i] = Some(r);
                    any_hoisted = true;
                } else if aliasing.iter().all(|s| same(s.1) && s.0 > i) {
                    let v = aliasing.last().unwrap().2;
                    let v_in_body = pre[v.index()].is_none() || rec.inst(v).is_const();
                    if rec.ty(v) == inst.ty && v_in_body && !carried.contains(&Ref(i as u32)) {
                        let r = push(&mut out, Inst { op: Op::XLoad { p: pp, snap: entry }, ty: inst.ty });
                        mem_phis.push((i, r, v));
                        any_hoisted = true;
                    }
                }
            }
            // the body's HSlots use the hoisted operands
            for (i, h) in hoisted_href.iter().enumerate() {
                if *h {
                    if let Op::HRef { t, .. } = rec.inst(Ref(i as u32)).op {
                        if map[t.index()].is_none() {
                            map[t.index()] = pre[t.index()];
                        }
                    }
                }
            }
        }
        let loop_ref = push(&mut out, Inst { op: Op::Loop, ty: IrType::Nil });
        // PHIs (their back operands are filled in after the body)
        let mut phis: Vec<(usize, Ref, Ref)> = Vec::new();
        for (&s, &w) in written.iter() {
            let (pre, recorded_load) = match loads.get(&s) {
                Some(&ld) => {
                    if rec.ty(ld) != rec.ty(w) {
                        return Err(unsupported_type("loop-carried slot changes type"));
                    }
                    (map[ld.index()].unwrap(), true)
                }
                None => match synth.get(&s) {
                    Some(&r) => (r, false),
                    None => continue,
                },
            };
            let phi = push(&mut out, Inst { op: Op::Phi { pre, back: pre }, ty: rec.ty(w) });
            if recorded_load {
                // inside the body, the slot's entry value is the PHI
                map[loads[&s].index()] = Some(phi);
            }
            phis.push((s, phi, w));
        }
        let phi_of: HashMap<usize, Ref> = phis.iter().map(|&(s, p, _)| (s, p)).collect();
        let mut mphis: Vec<(Ref, Ref)> = Vec::new();
        for &(i, pre_v, v) in &mem_phis {
            let phi = push(&mut out, Inst { op: Op::Phi { pre: pre_v, back: pre_v }, ty: rec.inst(Ref(i as u32)).ty });
            map[i] = Some(phi);
            mphis.push((phi, v));
        }

        // body, with snapshots copied and completed
        let mut snap_map: HashMap<u32, SnapId> = HashMap::new();
        for (i, inst) in rec.insts.iter().enumerate() {
            if map[i].is_some() {
                continue;
            }
            let rm = |r: Ref| map[r.index()].expect("operand defined earlier");
            let mut op = inst.op.clone();
            if hoisted_href[i] {
                if let Op::HRef { t, idx, .. } = op {
                    op = Op::HSlot { t, idx };
                }
            }
            remap_operands(&mut op, &rm);
            if let Some(s) = inst.snapshot() {
                let ns = *snap_map.entry(s.0).or_insert_with(|| {
                    let old = &rec.snapshots[s.0 as usize];
                    let mut entries: BTreeMap<u16, Ref> = old.entries.iter().map(|e| (e.slot, rm(e.value))).collect();
                    // carried slots not yet rewritten this iteration hold
                    // the PHI (the stack still has the trace-entry value) -
                    // unless a call of Lua code stored them all
                    if !old.stack_current {
                        for (&slot, &phi) in &phi_of {
                            entries.entry(slot as u16).or_insert(phi);
                        }
                    }
                    out.snapshots.push(Snapshot {
                        pc: old.pc,
                        entries: entries.into_iter().map(|(slot, value)| SnapEntry { slot, value }).collect(),
                        ref_limit: Ref(u32::MAX),
                        frames: old.frames.clone(),
                        top: old.top,
                        stack_current: old.stack_current,
                    });
                    SnapId(out.snapshots.len() as u32 - 1)
                });
                set_snapshot(&mut op, ns);
            }
            map[i] = Some(push(&mut out, Inst { op, ty: inst.ty }));
        }
        for &(phi, v) in &mphis {
            let back = map[v.index()].ok_or_else(|| AbortReason::Internal("memory PHI back value outside the body".into()))?;
            if back <= loop_ref {
                return Err(AbortReason::Internal("memory PHI back value is invariant".into()));
            }
            if let Op::Phi { back: b, .. } = &mut out.insts[phi.index()].op {
                *b = back;
            }
        }
        for &(_, phi, w) in &phis {
            let back = map[w.index()].ok_or_else(|| AbortReason::Internal("PHI back value outside the body".into()))?;
            // (a slot reset to a constant / invariant every iteration: the
            // PHI form needs its back value computed in the body)
            if back <= loop_ref {
                return Err(unsupported_type("loop-carried slot set to an invariant"));
            }
            if let Op::Phi { back: b, .. } = &mut out.insts[phi.index()].op {
                *b = back;
            }
        }
        // explicit snapshot at the loop close: every slot the body wrote
        // holds its end-of-iteration value. The compiler's interrupt exit
        // (back edge taken, running out of trace budget) restores from the
        // LAST snapshot; with this one that restore is unconditional - the
        // interrupt can only happen after the whole body ran.
        let close_entries: Vec<SnapEntry> = written
            .iter()
            .map(|(&slot, &w)| {
                let v = map[w.index()].ok_or_else(|| AbortReason::Internal("loop-close slot value outside the body".into()))?;
                Ok(SnapEntry { slot: slot as u16, value: v })
            })
            .collect::<R<_>>()?;
        out.snapshots.push(Snapshot {
            pc: rec.start_pc,
            entries: close_entries,
            ref_limit: Ref(out.insts.len() as u32),
            frames: vec![],
            top: None,
            stack_current: false,
        });
        // each snapshot covers the values defined before its first user
        for (i, inst) in out.insts.iter().enumerate() {
            if let Some(s) = inst.snapshot() {
                let sn = &mut out.snapshots[s.0 as usize];
                if Ref(i as u32) < sn.ref_limit {
                    sn.ref_limit = Ref(i as u32);
                }
            }
        }
        out.loop_ref = Some(loop_ref);
        super::ir::validate(&out).map_err(AbortReason::Internal)?;
        Ok((out, any_hoisted))
    }
}

/// The last back edge in `p` whose target is `header`: the end of the loop
/// whose body starts there. All three back-edge opcodes jump to
/// `pc + 1 + D`. Returns `header` itself when no such instruction exists
/// (recording then ends at the first step outside the header, which can't
/// happen: the caller starts recording on a taken back edge).
unsafe fn find_loop_end(p: *mut Proto, header: u32) -> u32 {
    let code = (*p).code;
    let n = (*p).sizecode as isize;
    let mut last = header;
    let mut pc: isize = header as isize;
    while pc < n {
        let insn = *code.offset(pc) as u32;
        let op = op_of(insn);
        if op == O::JumpBack as u8 || op == O::ForNLoop as u8 || op == O::ForGLoop as u8 {
            if (pc + 1 + d_of(insn) as isize) as u32 == header {
                last = pc as u32;
            }
        }
        pc += O::try_from(op).map(|o| get_op_length(o) as isize).unwrap_or(1);
    }
    last
}

/// Alias class of a recorded pointer (see `optimizer::forwarding`): hash
/// node index, or array element (table, index, constant index).
#[derive(Clone, Copy, PartialEq)]
enum RecMem {
    Node(u32),
    Elem(Option<i32>),
    Other,
}

fn rec_mem(t: &Trace, p: Ref) -> RecMem {
    match t.inst(p).op {
        Op::HRef { idx, .. } | Op::HSlot { idx, .. } => RecMem::Node(idx),
        Op::ARef { idx, .. } => RecMem::Elem(match t.inst(idx).op {
            Op::KI32(k) => Some(k),
            _ => None,
        }),
        _ => RecMem::Other,
    }
}

fn rec_may_alias(a: RecMem, b: RecMem) -> bool {
    match (a, b) {
        (RecMem::Node(x), RecMem::Node(y)) => x == y,
        (RecMem::Node(_), RecMem::Elem(_)) | (RecMem::Elem(_), RecMem::Node(_)) => false,
        (RecMem::Elem(Some(x)), RecMem::Elem(Some(y))) => x == y,
        _ => true,
    }
}

/// The exact location of a recorded pointer: (kind, table, index).
fn rec_key(t: &Trace, p: Ref) -> Option<(u8, Ref, u32)> {
    match t.inst(p).op {
        Op::HRef { t: tb, idx, .. } | Op::HSlot { t: tb, idx } => Some((0, tb, idx)),
        Op::ARef { t: tb, idx } => Some((1, tb, idx.0)),
        _ => None,
    }
}

/// How a closure is built (`Op::MakeClosure`): DUPCLOSURE of the constant
/// closure `target`, or NEWCLOSURE of prototype `target`; per upvalue, its
/// capture.
pub struct ClosureSite {
    pub dup: bool,
    pub target: usize,
    pub caps: Vec<Capture>,
}

/// One upvalue of a new closure: `kind` LCT_VAL (`idx`: index into the
/// passed values), LCT_REF (`idx` likewise: the value of trace slot
/// `slot`, which the upvalue is opened on) or LCT_UPVAL (`idx`: upvalue of
/// the creating closure).
#[derive(Clone, Copy)]
pub struct Capture {
    pub kind: u8,
    pub idx: u8,
    pub slot: u16,
}

/// Builtins the trace compiler has an instruction for.
fn math1_supported(bfid: u8) -> bool {
    use luau_common::bytecode::BuiltinFunction as B;
    [B::MathSqrt, B::MathFloor, B::MathCeil, B::MathAbs].iter().any(|&b| b as u8 == bfid)
}

pub(crate) fn remap_operands(op: &mut Op, rm: &dyn Fn(Ref) -> Ref) {
    use Op::*;
    match op {
        Guard { a, b, .. } => {
            *a = rm(*a);
            *b = rm(*b);
        }
        GuardTruthy { a, .. } => *a = rm(*a),
        GuardSame { a, k, .. } | GuardDiff { a, k, .. } => {
            *a = rm(*a);
            *k = rm(*k);
        }
        StrCmp { a, b } => {
            *a = rm(*a);
            *b = rm(*b);
        }
        Add(a, b) | Sub(a, b) | Mul(a, b) | Div(a, b) | Mod(a, b) | Pow(a, b) | IDiv(a, b) | AddI32(a, b) => {
            *a = rm(*a);
            *b = rm(*b);
        }
        Neg(a) | Math1(_, a) | I32ToNum(a) | ASize(a) | ABase(a) | TruncI32(a) => *a = rm(*a),
        NumToI32 { a, .. } => *a = rm(*a),
        ARef { t, idx } => {
            *t = rm(*t);
            *idx = rm(*idx);
        }
        XLoad { p, .. } => *p = rm(*p),
        XStore { p, v } => {
            *p = rm(*p);
            *v = rm(*v);
        }
        GuardPlainTable { t, .. } => *t = rm(*t),
        ULoad { cl, .. } | ClEnv { cl } | GuardUvSlot { cl, .. } => *cl = rm(*cl),
        HRef { t, k, .. } => {
            *t = rm(*t);
            *k = rm(*k);
        }
        HSlot { t, .. } => *t = rm(*t),
        Barrier { t, v } => {
            *t = rm(*t);
            *v = rm(*v);
        }
        TLen { t } => *t = rm(*t),
        TMeta { t, .. } => *t = rm(*t),
        MakeClosure { cl, vals, nvals, .. } => {
            *cl = rm(*cl);
            for x in vals.iter_mut().take(*nvals as usize) {
                *x = rm(*x);
            }
        }
        TGet { t, k, .. } => {
            *t = rm(*t);
            *k = rm(*k);
        }
        TSet { t, k, v, .. } => {
            *t = rm(*t);
            *k = rm(*k);
            *v = rm(*v);
        }
        HMiss { t, k, .. } => {
            *t = rm(*t);
            *k = rm(*k);
        }
        GuardCFunc { f, .. } => *f = rm(*f),
        SLen { s } => *s = rm(*s),
        DupTable { tpl } => *tpl = rm(*tpl),
        Builtin { args, nargs, .. } => {
            for x in args.iter_mut().take(*nargs as usize) {
                *x = rm(*x);
            }
        }
        GuardProto { f, p, .. } => {
            *f = rm(*f);
            *p = rm(*p);
        }
        GuardFlag { p, .. } => *p = rm(*p),
        Phi { pre, back } => {
            *pre = rm(*pre);
            *back = rm(*back);
        }
        KNil | KBool(_) | KNum(_) | KInt(_) | KPtr(_) | KI32(_) | SLoad { .. } | Loop | Nop | FrameFunc | NewTable { .. } | CCall { .. } | SReload { .. } | Concat { .. } | CloseUpvals { .. } | CallC { .. } | GuardTop { .. } | Sunk => {}
        UStore { cl, v, .. } => {
            *cl = rm(*cl);
            *v = rm(*v);
        }
        Not(a) | LudToI32(a) | I32ToLud(a) | VNeg(a) => *a = rm(*a),
        VArith { a, b, .. } | GuardVecEq { a, b, .. } => {
            *a = rm(*a);
            *b = rm(*b);
        }
        VGet { v, .. } => *v = rm(*v),
        VargLoad { idx, .. } => *idx = rm(*idx),
        VMake { x, y, z } => {
            *x = rm(*x);
            *y = rm(*y);
            *z = rm(*z);
        }
    }
}

fn set_snapshot(op: &mut Op, s: SnapId) {
    use Op::*;
    match op {
        SLoad { snap, .. }
        | Guard { snap, .. }
        | GuardTruthy { snap, .. }
        | GuardSame { snap, .. }
        | GuardDiff { snap, .. }
        | NumToI32 { snap, .. }
        | XLoad { snap, .. }
        | GuardPlainTable { snap, .. }
        | GuardFlag { snap, .. }
        | ULoad { snap, .. }
        | GuardProto { snap, .. }
        | HRef { snap, .. }
        | GuardUvSlot { snap, .. }
        | Builtin { snap, .. }
        | TMeta { snap, .. }
        | HMiss { snap, .. }
        | GuardCFunc { snap, .. }
        | TSet { snap, .. }
        | CCall { snap, .. }
        | SReload { snap, .. }
        | Concat { snap, .. }
        | GuardVecEq { snap, .. }
        | VargLoad { snap, .. }
        | CloseUpvals { snap, .. }
        | CallC { snap, .. }
        | GuardTop { snap, .. }
        | UStore { snap, .. } => *snap = s,
        _ => {}
    }
}

/// The fast builtin a standard library C function corresponds to (found by
/// name in the globals: `math.sin` is MathSin, `type` is Type, ...).
unsafe fn builtin_of(l: *mut lua_State, fp: usize) -> Option<u8> {
    use luau_common::bytecode::BuiltinFunction as B;
    thread_local! {
        static MAP: std::cell::RefCell<Option<HashMap<usize, u8>>> = const { std::cell::RefCell::new(None) };
    }
    if l.is_null() {
        return None;
    }
    MAP.with(|m| {
        let mut m = m.borrow_mut();
        if m.is_none() {
            let mut map = HashMap::new();
            let globals = (*l).gt;
            let banned = [B::Rawset, B::TableUnpack, B::SelectVararg, B::Setmetatable, B::TableClear];
            for id in 1..=255u8 {
                let Ok(b) = B::try_from(id) else { continue };
                if banned.contains(&b) {
                    continue;
                }
                let name = format!("{:?}", b);
                let (lib, fname) = if b == B::Vector {
                    (Some("vector".to_string()), "create".to_string())
                } else {
                    ["Math", "Bit32", "String", "Buffer", "Vector"]
                        .iter()
                        .find(|p| name.starts_with(*p) && name.len() > p.len())
                        .map(|p| (Some(p.to_lowercase()), name[p.len()..].to_lowercase()))
                        .unwrap_or((None, name.to_lowercase()))
                };
                let table = match lib {
                    Some(lib) => match get_by_name(globals, lib.as_bytes()) {
                        Some(v) if (*v).tt == LUA_TTABLE => hvalue(v),
                        _ => continue,
                    },
                    None => globals,
                };
                if let Some(v) = get_by_name(table, fname.as_bytes()) {
                    if (*v).tt == LUA_TFUNCTION && (*clvalue(v)).isC != 0 {
                        if let Some(f) = Closure::c(clvalue(v)).f {
                            map.insert(f as usize, id);
                        }
                    }
                }
            }
            *m = Some(map);
        }
        m.as_ref().unwrap().get(&fp).copied()
    })
}

/// The library C function builtin `bfid` stands for (`string.byte` for
/// StringByte, ...), looked up in the globals.
unsafe fn builtin_closure(l: *mut lua_State, bfid: u8) -> Option<*mut Closure> {
    use luau_common::bytecode::BuiltinFunction as B;
    let b = B::try_from(bfid).ok()?;
    let name = format!("{:?}", b);
    let (lib, fname) = if b == B::Vector {
        (Some("vector".to_string()), "create".to_string())
    } else {
        ["Math", "Bit32", "String", "Buffer", "Vector", "Table"]
            .iter()
            .find(|p| name.starts_with(*p) && name.len() > p.len())
            .map(|p| (Some(p.to_lowercase()), name[p.len()..].to_lowercase()))
            .unwrap_or((None, name.to_lowercase()))
    };
    let globals = (*l).gt;
    let table = match lib {
        Some(lib) => match get_by_name(globals, lib.as_bytes()) {
            Some(v) if (*v).tt == LUA_TTABLE => hvalue(v),
            _ => return None,
        },
        None => globals,
    };
    let v = get_by_name(table, fname.as_bytes())?;
    ((*v).tt == LUA_TFUNCTION && (*clvalue(v)).isC != 0).then(|| clvalue(v))
}

/// Whether C function `fp` is a standard library leaf function (it only
/// reads its arguments: no callbacks into Lua, no yields, no table writes),
/// and whether its first argument may be a table (`table.concat`).
unsafe fn leaf_c(l: *mut lua_State, fp: usize) -> Option<bool> {
    thread_local! {
        static MAP: std::cell::RefCell<Option<HashMap<usize, bool>>> = const { std::cell::RefCell::new(None) };
    }
    if l.is_null() {
        return None;
    }
    MAP.with(|m| {
        let mut m = m.borrow_mut();
        if m.is_none() {
            let mut map = HashMap::new();
            let globals = (*l).gt;
            let leaves: [(&str, &[&str]); 4] = [
                ("math", &["random", "noise"]),
                ("os", &["clock", "time"]),
                ("string", &["format", "rep", "upper", "lower", "reverse", "find", "match", "len", "byte", "sub", "char"]),
                ("table", &["concat"]),
            ];
            // (globals: ipairs/pairs only build the iterator triple; their
            // table argument is only type-checked)
            for name in ["ipairs", "pairs"] {
                if let Some(v) = get_by_name(globals, name.as_bytes()) {
                    if (*v).tt == LUA_TFUNCTION && (*clvalue(v)).isC != 0 {
                        if let Some(f) = Closure::c(clvalue(v)).f {
                            map.insert(f as usize, true);
                        }
                    }
                }
            }
            for (lib, names) in leaves {
                let Some(t) = get_by_name(globals, lib.as_bytes()) else { continue };
                if (*t).tt != LUA_TTABLE {
                    continue;
                }
                for name in names {
                    if let Some(v) = get_by_name(hvalue(t), name.as_bytes()) {
                        if (*v).tt == LUA_TFUNCTION && (*clvalue(v)).isC != 0 {
                            if let Some(f) = Closure::c(clvalue(v)).f {
                                map.insert(f as usize, lib == "table");
                            }
                        }
                    }
                }
            }
            *m = Some(map);
        }
        m.as_ref().unwrap().get(&fp).copied()
    })
}

/// A string-keyed entry of `h` by the key's bytes, or of its metatable's
/// __index table (sandboxed globals are a proxy like that).
unsafe fn get_by_name(h: *mut LuaTable, name: &[u8]) -> Option<*const TValue> {
    if let Some(v) = get_own_by_name(h, name) {
        return Some(v);
    }
    let mt = (*h).metatable;
    if mt.is_null() {
        return None;
    }
    match get_own_by_name(mt, b"__index") {
        Some(i) if (*i).tt == LUA_TTABLE && hvalue(i) != h => get_own_by_name(hvalue(i), name),
        _ => None,
    }
}

unsafe fn get_own_by_name(h: *mut LuaTable, name: &[u8]) -> Option<*const TValue> {
    let n = 1usize << (*h).lsizenode;
    for i in 0..n {
        let node = (*h).node.add(i);
        if (*node).key.tt() == LUA_TSTRING && (*crate::ltable::gval(node)).tt != LUA_TNIL {
            let ts = (*node).key.value.gc as *mut TString;
            if std::slice::from_raw_parts(getstr(ts), (*ts).len as usize) == name {
                return Some(crate::ltable::gval(node));
            }
        }
    }
    None
}

/// Builtin names for abort diagnostics ("builtin:MathMax"; built once).
fn builtin_name(bfid: u8) -> &'static str {
    use luau_common::bytecode::BuiltinFunction as B;
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    let names = NAMES.get_or_init(|| {
        (0..=255u8)
            .map(|b| match B::try_from(b) {
                Ok(f) => &*Box::leak(format!("builtin:{:?}", f).into_boxed_str()),
                Err(_) => "builtin:?",
            })
            .collect()
    });
    names[bfid as usize]
}

/// Opcode names for abort diagnostics (built once).
fn opname(o: O) -> &'static str {
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    let names = NAMES.get_or_init(|| {
        (0..=255u8)
            .map(|b| match O::try_from(b) {
                Ok(o) => &*Box::leak(format!("{:?}", o).into_boxed_str()),
                Err(_) => "?",
            })
            .collect()
    });
    names[o as u8 as usize]
}

#[allow(dead_code)]
fn op_len(op: u8) -> usize {
    O::try_from(op).map(|o| get_op_length(o) as usize).unwrap_or(1)
}
