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
    pending: Option<(Option<Ref>, usize, u32)>,
    /// the thread being recorded
    l: *mut lua_State,
}

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
    ) -> Recorder {
        let mut r = Recorder::new(id, proto, cl, start_pc, base, max_len);
        r.loop_end = loop_end;
        r.side = Some((header, parent));
        // snapshot 0: the start, nothing done (the entry checks exit here)
        r.b.snapshot();
        r
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
        if let Some((r, slot, next)) = self.pending.take() {
            if pc != next {
                // the builtin declined: the interpreter called the function
                return Step::Abort(AbortReason::UnsupportedType("builtin fell back"));
            }
            if let Some(r) = r {
                let tv = self.root_base.add(slot);
                match IrType::of_tag((*tv).tt) {
                    Some(ty) if ty != IrType::Vector => self.b.trace.insts[r.index()].ty = ty,
                    _ => return Step::Abort(AbortReason::UnsupportedType("builtin result type")),
                }
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
        let zero = self.b.ki32(0);
        self.b.emit(Op::Guard { cmp: Cmp::Le, a: zero, b: idx, snap }, IrType::Nil);
        self.b.emit(Op::Guard { cmp: Cmp::Lt, a: idx, b: size, snap }, IrType::Nil);
        Ok(self.b.emit(Op::ARef { t, idx }, IrType::Ptr))
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
                    LUA_TSTRING | LUA_TFUNCTION | LUA_TTABLE | LUA_TLIGHTUSERDATA | LUA_TBUFFER => {
                        if ta == LUA_TTABLE && va != vb {
                            for t in [x, y] {
                                self.b.emit(Op::GuardPlainTable { t, store: false, snap }, IrType::Nil);
                            }
                        }
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
                return Err(AbortReason::UnsupportedControlFlow);
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
            let p = self.field_ref(h, t, kidx, false)?;
            let ety = IrType::of_tag((*p.1).tt).ok_or(unsupported_type("field type"))?;
            let snap = self.b.snapshot();
            let v = self.b.emit(Op::XLoad { p: p.0, snap }, ety);
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
            let (p, _) = self.field_ref(h, t, kidx, true)?;
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
            self.b.set_slot(a, v);
        } else if o(O::DupTable) {
            let kv = (*proto).k.add(d_of(insn) as usize);
            if (*kv).tt != LUA_TTABLE {
                return Err(unsupported_type("DUPTABLE of a non-table"));
            }
            let tpl = self.b.kptr((*kv).value.gc as usize, IrType::Table);
            let v = self.b.emit(Op::DupTable { tpl }, IrType::Table);
            self.b.set_slot(a, v);
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

    /// FASTCALL1/2/2K/3 of any builtin that doesn't change tables or
    /// metatables: the trace calls Luau's own fast implementation.
    unsafe fn record_builtin(&mut self, pc: u32, insn: u32, aux: u32, base: StkId) -> R<()> {
        use luau_common::bytecode::BuiltinFunction as B;
        let op = op_of(insn);
        let bfid = a_of(insn) as u8;
        let banned = [B::Rawset, B::TableInsert, B::TableUnpack, B::SelectVararg, B::Setmetatable, B::TableClear, B::Vector];
        if banned.iter().any(|&b| b as u8 == bfid) {
            return Err(AbortReason::UnsupportedOpcode(builtin_name(bfid)));
        }
        let code = (*self.cur_proto()).code;
        // where the replaced CALL is (skip counts from after the FASTCALL)
        let (len, skip) = if op == O::FastCall1 as u8 || op == O::FastCall as u8 { (1, c_of(insn)) } else { (2, c_of(insn) - 1) };
        let call_at = pc as usize + len + skip;
        let call = *code.add(call_at);
        let (ra, nres) = (a_of(call), c_of(call) as i32 - 1);
        if !(0..=1).contains(&nres) {
            return Err(unsupported_type("builtin with variable results"));
        }
        let (cl, clr) = self.cur_closure();
        if (*(*cl).env).safeenv == 0 {
            return Err(unsupported_type("builtin in an unsafe environment"));
        }
        let mut args = [Ref(0); 3];
        let mut nargs = 0;
        if op == O::FastCall as u8 {
            // the CALL's own arguments, already in place
            let n = b_of(call);
            if n == 0 || n - 1 > 3 {
                return Err(unsupported_type("builtin with variable or many arguments"));
            }
            nargs = n - 1;
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
        for a in &args[..nargs] {
            if self.b.trace.ty(*a) == IrType::Vector {
                return Err(unsupported_type("vector builtin argument"));
            }
        }
        self.guard_safeenv(clr);
        let buf = Box::leak(vec![0u64; 8].into_boxed_slice()).as_mut_ptr() as usize;
        let snap = self.b.snapshot();
        // (the result's type is filled in at the next step, from the stack)
        let v = self.b.emit(Op::Builtin { bfid, args, nargs: nargs as u8, nres: nres as u8, buf, snap }, IrType::Nil);
        let next = call_at as u32 + 1;
        if nres == 1 {
            self.b.set_slot(ra, v);
            self.pending = Some((Some(v), self.b.off + ra, next));
        } else {
            self.pending = Some((None, 0, next));
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
        if bb == 0 || c == 0 {
            return Err(unsupported_type("call with variable arguments or results"));
        }
        let p = Closure::proto(cl);
        if (*p).is_vararg != 0 {
            return Err(unsupported_type("call of a vararg function"));
        }
        if self.funcs.len() >= MAX_INLINE_DEPTH {
            return Err(AbortReason::UnsupportedControlFlow);
        }
        let (nargs, nres) = (bb - 1, c as i32 - 1);
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
        self.b.push_frame(SnapFrame { base: fb as u16, func: cl as usize, ret_pc, nresults: nres, nargs: nargs as u16 });
        self.funcs.push((p, cl, Some(f)));
        // missing parameters are nil
        for i in nargs..(*p).numparams as usize {
            let nil = self.b.knil();
            self.b.set_slot(i, nil);
        }
        self.stack_need = self.stack_need.max(fb + nargs + (*cl).stacksize as usize + LUA_MINSTACK as usize);
        Ok(())
    }

    /// CALL of a C function that is a builtin with a fast implementation
    /// (`string.sub` through a method call, `math.sin` through a local, ...).
    unsafe fn record_c_call(&mut self, pc: u32, insn: u32, base: StkId, cl: *mut Closure) -> R<()> {
        let (a, bb, c) = (a_of(insn), b_of(insn), c_of(insn));
        let Some(fp) = Closure::c(cl).f.map(|f| f as usize) else {
            return Err(AbortReason::UnsupportedOpcode("Call (C function)"));
        };
        let Some(bfid) = builtin_of(self.l, fp) else {
            return Err(AbortReason::UnsupportedOpcode("Call (C function)"));
        };
        if bb == 0 || bb - 1 > 3 || !(1..=2).contains(&c) {
            return Err(unsupported_type("builtin call with variable or many arguments/results"));
        }
        let (nargs, nres) = (bb - 1, c as i32 - 1);
        let f = self.read(a, base)?;
        let snap = self.b.snapshot();
        self.b.emit(Op::GuardCFunc { f, fp, snap }, IrType::Nil);
        let mut args = [Ref(0); 3];
        for k in 0..nargs {
            args[k] = self.read(a + 1 + k, base)?;
            if self.b.trace.ty(args[k]) == IrType::Vector {
                return Err(unsupported_type("vector builtin argument"));
            }
        }
        let buf = Box::leak(vec![0u64; 8].into_boxed_slice()).as_mut_ptr() as usize;
        let snap = self.b.snapshot();
        let v = self.b.emit(Op::Builtin { bfid, args, nargs: nargs as u8, nres: nres as u8, buf, snap }, IrType::Nil);
        let next = pc + if op_of(insn) == O::CallFb as u8 { 2 } else { 1 };
        if nres == 1 {
            self.b.set_slot(a, v);
            self.pending = Some((Some(v), self.b.off + a, next));
        } else {
            self.pending = Some((None, 0, next));
        }
        Ok(())
    }

    /// RETURN from an inlined frame: its results go to the caller's slots.
    unsafe fn record_return(&mut self, insn: u32, base: StkId) -> R<()> {
        if self.funcs.len() == 1 {
            return Err(AbortReason::UnsupportedControlFlow);
        }
        let (a, bb) = (a_of(insn), b_of(insn));
        if bb == 0 {
            return Err(unsupported_type("return of variable results"));
        }
        let nret = bb - 1;
        let nres = self.b.frames.last().unwrap().nresults as usize;
        let mut vals = Vec::with_capacity(nres);
        for i in 0..nres {
            vals.push(if i < nret { self.read(a + i, base)? } else { self.b.knil() });
        }
        let f = self.b.pop_frame().unwrap();
        self.funcs.pop();
        // results start at the caller's function slot
        let dst = f.base as usize - 1 - self.b.off;
        for (i, v) in vals.into_iter().enumerate() {
            self.b.set_slot(dst + i, v);
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
        let (mut t, hoisted) = self.close_with(&rec, true)?;
        if hoisted {
            t.fallback = Some(Box::new(self.close_with(&rec, false)?.0));
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
        out.snapshots.push(Snapshot { pc: header, entries, ref_limit: Ref(out.insts.len() as u32), frames: vec![] });
        out.start_pc = header;
        out.end_pc = self.loop_end;
        out.loop_ref = None;
        out.side_of = Some(parent);
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
        out.snapshots.push(Snapshot { pc: rec.start_pc, entries: vec![], ref_limit: Ref(0), frames: vec![] });
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
                if !Self::hoistable(inst) || carried.contains(&Ref(i as u32)) {
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
                    // the PHI (the stack still has the trace-entry value)
                    for (&slot, &phi) in &phi_of {
                        entries.entry(slot as u16).or_insert(phi);
                    }
                    out.snapshots.push(Snapshot {
                        pc: old.pc,
                        entries: entries.into_iter().map(|(slot, value)| SnapEntry { slot, value }).collect(),
                        ref_limit: Ref(u32::MAX),
                        frames: old.frames.clone(),
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

/// Builtins the trace compiler has an instruction for.
fn math1_supported(bfid: u8) -> bool {
    use luau_common::bytecode::BuiltinFunction as B;
    [B::MathSqrt, B::MathFloor, B::MathCeil, B::MathAbs].iter().any(|&b| b as u8 == bfid)
}

fn remap_operands(op: &mut Op, rm: &dyn Fn(Ref) -> Ref) {
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
        Neg(a) | Math1(_, a) | I32ToNum(a) | ASize(a) => *a = rm(*a),
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
        KNil | KBool(_) | KNum(_) | KInt(_) | KPtr(_) | KI32(_) | SLoad { .. } | Loop | Nop | FrameFunc | NewTable { .. } => {}
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
        | GuardCFunc { snap, .. } => *snap = s,
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
            let banned = [B::Rawset, B::TableInsert, B::TableUnpack, B::SelectVararg, B::Setmetatable, B::TableClear, B::Vector];
            for id in 1..=255u8 {
                let Ok(b) = B::try_from(id) else { continue };
                if banned.contains(&b) {
                    continue;
                }
                let name = format!("{:?}", b);
                let (lib, fname) = ["Math", "Bit32", "String", "Buffer"]
                    .iter()
                    .find(|p| name.starts_with(*p) && name.len() > p.len())
                    .map(|p| (Some(p.to_lowercase()), name[p.len()..].to_lowercase()))
                    .unwrap_or((None, name.to_lowercase()));
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
