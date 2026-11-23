//! A recorded trace.

use super::ir::instruction::{Inst, Op, Ref};
use super::ir::types::IrType;
use super::snapshots::Snapshot;
use crate::lobject::Proto;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraceId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceState {
    Idle,
    Recording,
    Optimizing,
    Compiling,
    Finished,
    Aborted,
}

/// Why recording or compiling gave up (for diagnostics and blacklisting).
#[derive(Clone, Debug, PartialEq)]
pub enum AbortReason {
    UnsupportedOpcode(&'static str),
    /// a value of a type the recorder can't handle at this opcode
    UnsupportedType(&'static str),
    TooLong,
    /// left the function, or control flow the recorder doesn't follow
    UnsupportedControlFlow,
    /// the recorded iteration left the loop (its last one): not the loop's
    /// fault, so it is recorded again later without a penalty
    LeftLoop,
    /// a call, yield, error or other VM re-entry during recording
    LeftFrame,
    Internal(String),
}

pub struct Trace {
    pub id: TraceId,
    /// the function the trace's loop is in
    pub proto: *mut Proto,
    /// bytecode index of the loop header the trace starts (and loops) at
    pub start_pc: u32,
    pub insts: Vec<Inst>,
    pub snapshots: Vec<Snapshot>,
    /// index of the `Loop` instruction, once the loop is closed
    pub loop_ref: Option<Ref>,
    pub state: TraceState,
    /// machine code, once compiled
    pub native: Option<super::compiler::CompiledTrace>,
    /// side exits taken by the compiled code since it was installed; a
    /// compiled trace that keeps failing its guards is dropped after
    /// [`EXIT_STORM_LIMIT`](super::EXIT_STORM_LIMIT) of them so the loop is
    /// re-recorded for the current types
    pub side_exits: u32,
    /// last bytecode index of the loop body (exits past it left the loop)
    pub end_pc: u32,
    /// the closure the trace was recorded for, when it depends on it
    /// (upvalues, environment): checked at entry. 0: any closure of `proto`
    pub closure: usize,
    /// stack slots (from the frame base) the trace may touch, inlined
    /// frames included: checked against the stack's size at entry
    pub stack_need: u32,
    /// GC objects the machine code refers to (kept alive while it exists)
    pub gc_refs: Vec<(usize, u8)>,
    /// prototypes inlined into the trace (besides `proto`)
    pub protos: Vec<usize>,
    /// the same trace without loop-invariant hoisting (whose values stay in
    /// registers across the loop): compiled instead when this one runs out
    /// of registers
    pub fallback: Option<Box<Trace>>,
    /// side trace: (tag of the parent trace, its snapshot this trace
    /// continues from); it ends by jumping back into the root trace
    pub side_of: Option<(u16, u32)>,
    /// short id in exit codes (`(tag << 16) | snapshot`), once compiled
    pub tag: u16,
    /// per snapshot: machine code its exit stub continues at instead of
    /// returning (a side trace), 0 for none. Leaked: compiled code refers
    /// to it for as long as the code exists
    pub link_table: usize,
    /// per snapshot: exits taken, and side trace recordings tried
    pub exit_counts: Vec<u32>,
    pub exit_tries: Vec<u8>,
    /// root trace: tags of its side traces
    pub sides: Vec<u16>,
    /// the trace may add keys to tables (TSet, table.insert): a table's
    /// array/node storage can move, so table addresses aren't reused across
    /// that, and nothing table-related is hoisted out of the loop
    pub inserts: bool,
    /// cycles trace runs took before exits counted in `side_exits`
    pub storm_cycles: u64,
}

impl Trace {
    pub fn new(id: TraceId, proto: *mut Proto, start_pc: u32) -> Trace {
        Trace { id, proto, start_pc, insts: Vec::new(), snapshots: Vec::new(), loop_ref: None, state: TraceState::Recording, native: None, side_exits: 0, end_pc: u32::MAX, closure: 0, stack_need: 0, gc_refs: Vec::new(), protos: Vec::new(), fallback: None, side_of: None, tag: 0, link_table: 0, exit_counts: Vec::new(), exit_tries: Vec::new(), sides: Vec::new(), inserts: false, storm_cycles: 0 }
    }

    pub fn inst(&self, r: Ref) -> &Inst {
        &self.insts[r.index()]
    }

    pub fn ty(&self, r: Ref) -> IrType {
        self.insts[r.index()].ty
    }

    pub fn len(&self) -> usize {
        self.insts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.insts.is_empty()
    }

    /// Text dump, one instruction per line (`LUAU_TRACE_DUMP`).
    pub fn dump(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(s, "trace {} at pc {}", self.id.0, self.start_pc);
        for (i, inst) in self.insts.iter().enumerate() {
            if matches!(inst.op, Op::Nop) {
                continue;
            }
            let _ = writeln!(s, "{:04} {:<14} {:?}", i, format!("{:?}", inst.ty), inst.op);
        }
        for (i, sn) in self.snapshots.iter().enumerate() {
            let e: Vec<String> = sn.entries.iter().map(|e| format!("r{}={}", e.slot, e.value.0)).collect();
            let _ = writeln!(s, "snap {} pc {} [{}]", i, sn.pc, e.join(" "));
        }
        s
    }
}
