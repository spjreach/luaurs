//! Building a trace's IR while recording.
//!
//! The builder keeps the recorder's view of the VM stack: for each slot,
//! the IR value it currently holds (if the trace has read or written it).
//! Reading a slot that hasn't been touched emits a guarded `SLoad`;
//! writing one just updates the map (the store happens in snapshots and
//! at trace exit, LuaJIT style).

use super::instruction::{Inst, Op, Ref, SnapId};
use super::types::IrType;
use crate::jit::snapshots::{SnapEntry, SnapFrame, Snapshot};
use crate::jit::trace::Trace;
use std::collections::HashMap;

pub struct IrBuilder {
    pub trace: Trace,
    /// bytecode index that a snapshot taken now resumes at
    pub pc: u32,
    /// slot -> current value (None: untouched, still in the VM stack)
    slots: Vec<Option<Ref>>,
    /// slots written by the trace (snapshots must restore them)
    dirty: Vec<bool>,
    consts: HashMap<ConstKey, Ref>,
    /// the last snapshot, reused while nothing it covers changed
    last_snap: Option<(SnapId, u32, usize)>,
    /// bumped on every slot write (invalidates `last_snap`)
    writes: usize,
    /// slot offset of the current (innermost inlined) frame: `slot`,
    /// `peek`, `set_slot` and `forget_slot` take frame-relative slots
    pub off: usize,
    /// inlined frames, outermost first (copied into snapshots)
    pub frames: Vec<SnapFrame>,
    /// see `Snapshot::top`
    top: Option<u16>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ConstKey {
    Nil,
    Bool(bool),
    Num(u64),
    Int(i64),
    Ptr(usize, u8),
    I32(i32),
}

impl IrBuilder {
    pub fn new(trace: Trace, nslots: usize, pc: u32) -> IrBuilder {
        IrBuilder {
            trace,
            pc,
            slots: vec![None; nslots],
            dirty: vec![false; nslots],
            consts: HashMap::new(),
            last_snap: None,
            writes: 0,
            off: 0,
            frames: Vec::new(),
            top: None,
        }
    }

    /// The finished trace.
    pub fn into_trace(self) -> Trace {
        self.trace
    }

    /// Slots read (loaded) by the trace so far, with their SLoad.
    pub fn loaded(&self) -> Vec<(usize, Ref)> {
        let mut v = Vec::new();
        for (i, inst) in self.trace.insts.iter().enumerate() {
            if let Op::SLoad { slot, .. } = inst.op {
                v.push((slot as usize, Ref(i as u32)));
            }
        }
        v
    }

    pub fn emit(&mut self, op: Op, ty: IrType) -> Ref {
        let r = Ref(self.trace.insts.len() as u32);
        self.trace.insts.push(Inst { op, ty });
        r
    }

    fn konst(&mut self, key: ConstKey, op: Op, ty: IrType) -> Ref {
        if let Some(&r) = self.consts.get(&key) {
            return r;
        }
        let r = self.emit(op, ty);
        self.consts.insert(key, r);
        r
    }

    pub fn knil(&mut self) -> Ref {
        self.konst(ConstKey::Nil, Op::KNil, IrType::Nil)
    }
    pub fn kbool(&mut self, b: bool) -> Ref {
        self.konst(ConstKey::Bool(b), Op::KBool(b), IrType::Boolean)
    }
    pub fn knum(&mut self, n: f64) -> Ref {
        self.konst(ConstKey::Num(n.to_bits()), Op::KNum(n), IrType::Number)
    }
    pub fn kint(&mut self, n: i64) -> Ref {
        self.konst(ConstKey::Int(n), Op::KInt(n), IrType::Integer)
    }
    pub fn ki32(&mut self, n: i32) -> Ref {
        self.konst(ConstKey::I32(n), Op::KI32(n), IrType::I32)
    }
    pub fn kptr(&mut self, p: usize, ty: IrType) -> Ref {
        self.konst(ConstKey::Ptr(p, ty as u8), Op::KPtr(p), ty)
    }

    /// The value of stack slot `slot`; `ty` is the type observed in the
    /// interpreter right now (its guard checks that on later runs).
    pub fn slot(&mut self, slot: usize, ty: IrType) -> Ref {
        let slot = self.abs(slot);
        if let Some(r) = self.slots[slot] {
            return r;
        }
        let snap = self.snapshot();
        let r = self.emit(Op::SLoad { slot: slot as u16, guarded: true, snap }, ty);
        self.slots[slot] = Some(r);
        r
    }

    /// The value currently mapped to `slot`, if any (no load).
    pub fn peek(&mut self, slot: usize) -> Option<Ref> {
        let slot = self.abs(slot);
        self.slots[slot]
    }

    /// Frame-relative slot -> trace slot (growing the slot map).
    fn abs(&mut self, slot: usize) -> usize {
        let s = self.off + slot;
        if s >= self.slots.len() {
            self.slots.resize(s + 1, None);
            self.dirty.resize(s + 1, false);
        }
        s
    }

    /// Enters an inlined call frame whose base is frame-relative slot
    /// `base` of the current frame.
    pub fn push_frame(&mut self, f: SnapFrame) {
        self.off = f.base as usize;
        self.frames.push(f);
        self.writes += 1;
    }

    /// Sets (or clears) the stack top that snapshots carry.
    pub fn set_top(&mut self, top: Option<usize>) {
        self.top = top.map(|t| t as u16);
        self.writes += 1;
    }

    /// Leaves the innermost inlined frame: its slots (from its function
    /// slot up) are dead.
    pub fn pop_frame(&mut self) -> Option<SnapFrame> {
        let f = self.frames.pop()?;
        let from = f.base as usize - 1;
        for s in from..self.slots.len() {
            self.slots[s] = None;
            self.dirty[s] = false;
        }
        self.off = self.frames.last().map_or(0, |g| g.base as usize);
        self.writes += 1;
        Some(f)
    }

    pub fn set_slot(&mut self, slot: usize, v: Ref) {
        let slot = self.abs(slot);
        self.slots[slot] = Some(v);
        self.dirty[slot] = true;
        self.writes += 1;
    }

    /// Forget a slot's value (written in a way the trace doesn't model):
    /// the next read reloads it.
    pub fn forget_slot(&mut self, slot: usize) {
        let slot = self.abs(slot);
        self.slots[slot] = None;
        self.dirty[slot] = false;
        self.writes += 1;
    }

    /// A snapshot of the current state (resuming at `self.pc`); consecutive
    /// guards with no slot writes in between share one.
    pub fn snapshot(&mut self) -> SnapId {
        if let Some((id, pc, w)) = self.last_snap {
            if pc == self.pc && w == self.writes {
                return id;
            }
        }
        let entries = self
            .slots
            .iter()
            .enumerate()
            .filter(|&(s, v)| v.is_some() && self.dirty[s])
            .map(|(s, v)| SnapEntry { slot: s as u16, value: v.unwrap() })
            .collect();
        let id = SnapId(self.trace.snapshots.len() as u32);
        self.trace.snapshots.push(Snapshot { pc: self.pc, entries, ref_limit: Ref(self.trace.insts.len() as u32), frames: self.frames.clone(), top: self.top });
        self.last_snap = Some((id, self.pc, self.writes));
        id
    }

    /// Slots written by the trace and their values (for loop closing: the
    /// loop-carried values).
    pub fn written(&self) -> Vec<(usize, Ref)> {
        self.slots.iter().enumerate().filter(|&(s, v)| v.is_some() && self.dirty[s]).map(|(s, v)| (s, v.unwrap())).collect()
    }
}
