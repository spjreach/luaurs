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
use super::snapshots::{SnapEntry, Snapshot};
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
    id: TraceId,
    steps: usize,
    max_len: usize,
}

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
    pub unsafe fn new(id: TraceId, proto: *mut Proto, start_pc: u32, base: StkId, max_len: usize) -> Recorder {
        let nslots = (*proto).maxstacksize as usize;
        let _ = base;
        Recorder {
            b: IrBuilder::new(Trace::new(id, proto, start_pc), nslots, start_pc),
            proto,
            start_pc,
            id,
            steps: 0,
            max_len,
        }
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
        if pc == self.start_pc && self.steps > 0 {
            return match self.close() {
                Ok(t) => Step::Closed(t),
                Err(e) => Step::Abort(e),
            };
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
        let kv = (*self.proto).k.add(k);
        Ok(match (*kv).tt {
            LUA_TNIL => self.b.knil(),
            LUA_TBOOLEAN => self.b.kbool((*kv).value.b != 0),
            LUA_TNUMBER => self.b.knum((*kv).value.n),
            LUA_TINTEGER => self.b.kint((*kv).value.l),
            LUA_TSTRING => self.b.kptr((*kv).value.gc as usize, IrType::String),
            _ => return Err(unsupported_type("constant")),
        })
    }

    unsafe fn knum(&mut self, k: usize) -> R<Ref> {
        let kv = (*self.proto).k.add(k);
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
        let code = (*self.proto).code;
        let insn = *code.add(pc as usize);
        let aux = *code.add(pc as usize + 1);
        let op = op_of(insn);
        let o = |x: O| op == x as u8;
        let (a, bb, c) = (a_of(insn), b_of(insn), c_of(insn));
        // a guard taken now resumes at this instruction (it hasn't run yet)
        self.b.pc = pc;

        if o(O::Nop) || o(O::Coverage) || o(O::Jump) {
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
            let v = self.arith(op, x, y);
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
            if (*base.add(a)).tt != LUA_TNUMBER || (*base.add(rb)).tt != LUA_TNUMBER {
                return Err(unsupported_type("comparison of non-numbers"));
            }
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
        } else if o(O::ForNLoop) {
            let target = (pc as i64 + 1 + d_of(insn) as i64) as u32;
            if target != self.start_pc {
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
            if target != self.start_pc {
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
        } else {
            let name = O::try_from(op).map(|x| opname(x)).unwrap_or("?");
            return Err(AbortReason::UnsupportedOpcode(name));
        }
        Ok(())
    }

    // ---- closing the loop ------------------------------------------------

    /// Turns the recorded iteration into `pre-roll; LOOP; PHIs; body`.
    fn close(&mut self) -> R<Trace> {
        let rec = std::mem::replace(&mut self.b.trace, Trace::new(TraceId(0), std::ptr::null_mut(), 0));
        let written: BTreeMap<usize, Ref> = self.b.written().into_iter().collect();
        let n = rec.insts.len();

        // which slots each recorded SLoad reads
        let mut loads: BTreeMap<usize, Ref> = BTreeMap::new();
        for (i, inst) in rec.insts.iter().enumerate() {
            if let Op::SLoad { slot, .. } = inst.op {
                loads.insert(slot as usize, Ref(i as u32));
            }
        }
        // loop-carried: read before written, and written by the iteration.
        // TODO(before traces run): a slot written by the iteration but read
        // only after its write is restored by exits after that write; an
        // exit *before* it leaves the trace-entry value in the stack, which
        // is wrong if the interpreter can read the slot at that exit's pc
        // (bytecode liveness decides; such slots must be carried too)
        let carried: Vec<(usize, Ref, Ref)> = loads
            .iter()
            .filter_map(|(&s, &ld)| written.get(&s).map(|&w| (s, ld, w)))
            .collect();
        for &(_, ld, w) in &carried {
            if rec.ty(ld) != rec.ty(w) {
                return Err(unsupported_type("loop-carried slot changes type"));
            }
        }

        let mut out = Trace::new(rec.id, rec.proto, rec.start_pc);
        let mut map: Vec<Option<Ref>> = vec![None; n];
        let push = |out: &mut Trace, inst: Inst| -> Ref {
            out.insts.push(inst);
            Ref(out.insts.len() as u32 - 1)
        };

        // entry snapshot: resume at the loop header, nothing modified yet
        out.snapshots.push(Snapshot { pc: rec.start_pc, entries: vec![], ref_limit: Ref(0) });
        let entry = SnapId(0);

        // pre-roll: constants and slot loads
        for (i, inst) in rec.insts.iter().enumerate() {
            let op = match &inst.op {
                Op::SLoad { slot, guarded, .. } => Op::SLoad { slot: *slot, guarded: *guarded, snap: entry },
                _ if inst.is_const() => inst.op.clone(),
                _ => continue,
            };
            map[i] = Some(push(&mut out, Inst { op, ty: inst.ty }));
        }
        let loop_ref = push(&mut out, Inst { op: Op::Loop, ty: IrType::Nil });
        // PHIs (their back operands are filled in after the body)
        let mut phis: Vec<(usize, Ref, Ref)> = Vec::new();
        for &(s, ld, w) in &carried {
            let pre = map[ld.index()].unwrap();
            let phi = push(&mut out, Inst { op: Op::Phi { pre, back: pre }, ty: rec.ty(ld) });
            // inside the body, the slot's entry value is the PHI
            map[ld.index()] = Some(phi);
            phis.push((s, phi, w));
        }
        let phi_of: HashMap<usize, Ref> = phis.iter().map(|&(s, p, _)| (s, p)).collect();

        // body, with snapshots copied and completed
        let mut snap_map: HashMap<u32, SnapId> = HashMap::new();
        for (i, inst) in rec.insts.iter().enumerate() {
            if map[i].is_some() {
                continue;
            }
            let rm = |r: Ref| map[r.index()].expect("operand defined earlier");
            let mut op = inst.op.clone();
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
                    });
                    SnapId(out.snapshots.len() as u32 - 1)
                });
                set_snapshot(&mut op, ns);
            }
            map[i] = Some(push(&mut out, Inst { op, ty: inst.ty }));
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
        Ok(out)
    }
}

fn remap_operands(op: &mut Op, rm: &dyn Fn(Ref) -> Ref) {
    use Op::*;
    match op {
        Guard { a, b, .. } => {
            *a = rm(*a);
            *b = rm(*b);
        }
        GuardTruthy { a, .. } => *a = rm(*a),
        GuardSame { a, k, .. } => {
            *a = rm(*a);
            *k = rm(*k);
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
        Phi { pre, back } => {
            *pre = rm(*pre);
            *back = rm(*back);
        }
        KNil | KBool(_) | KNum(_) | KInt(_) | KPtr(_) | KI32(_) | SLoad { .. } | Loop | Nop => {}
    }
}

fn set_snapshot(op: &mut Op, s: SnapId) {
    use Op::*;
    match op {
        SLoad { snap, .. }
        | Guard { snap, .. }
        | GuardTruthy { snap, .. }
        | GuardSame { snap, .. }
        | NumToI32 { snap, .. }
        | XLoad { snap, .. }
        | GuardPlainTable { snap, .. } => *snap = s,
        _ => {}
    }
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
