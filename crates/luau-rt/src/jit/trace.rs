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
    /// reached the back edge of an inner while / repeat loop without a
    /// trace: that loop is traced first (made hot), the outer one later
    /// links to it
    InnerLoop { proto: usize, header: u32 },
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
    /// the trace reads its own frame's varargs: there are this many
    /// (checked at entry)
    pub varargs: Option<u16>,
    /// tables this trace doesn't create (built on exits that need them)
    pub sinks: Vec<Sink>,
    /// a trace ending at another loop's header: that loop's (proto,
    /// header), run next through `link_cell`
    pub link: Option<(usize, u32)>,
    /// the cell holding the linked trace's re-entry (0: none - the trace
    /// exits to the interpreter there)
    pub link_cell: usize,
    /// (proto, header) of the loop whose root trace this is or belongs to
    pub root_key: (usize, u32),
    /// a side trace's first spill slot (below: its root's, kept for the
    /// root's body re-entry)
    pub spill_base: u32,
    /// a root trace: what its body re-entry assumes still holds (see
    /// `finish_trace`): slots read before the loop, slots of its PHIs,
    /// whether it hoisted table layout / memory reads
    pub reads: Vec<u16>,
    pub phi_slots: Vec<u16>,
    pub layout_hoisted: bool,
    pub mem_hoisted: bool,
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
    /// per snapshot: (slot, kind 0 gpr / 1 xmm, register) of the values its
    /// exit holds in registers (a side trace attached there finds them
    /// there: `inherit`); empty where the exit calls helpers
    pub exit_regs: Vec<Vec<(u16, u8, u8)>>,
    /// a side trace: (instruction, kind, register) of its entry loads whose
    /// value its parent's exit leaves in that register (no load)
    pub inherit: Vec<(u32, u8, u8)>,
    /// snapshots of the integer conversions' entry checks, with the level
    /// an exit through one sets for the loop (see `JitState::no_int`)
    pub int_snaps: Vec<(u32, u8)>,
    /// integer conversions left out for this loop (they failed): 1 the
    /// 64-bit ones, 2 all
    pub no_int: u8,
    /// per snapshot: resume pc | FAST_EXIT when an exit through it needs no
    /// bookkeeping (leaves the loop, or ends it at the header: no frames,
    /// no stack top) - native code's cached entry handles those itself
    pub exit_pcs: Box<[u32]>,
    /// per snapshot: exits seen on the full path (leaked: LoopCells point
    /// at it)
    pub exit_seen: *mut u8,
}

/// A trace's return value when a C function it called yielded (the VM is
/// left).
pub const EXIT_LEFT_VM: u32 = u32::MAX;

/// A table the trace doesn't create (see `optimizer::sink`): exits through
/// the snapshots in `states` build it.
#[derive(Clone, Debug)]
pub struct Sink {
    /// its (now `Op::Sunk`) instruction: the value snapshots name
    pub at: Ref,
    /// template table to copy (DupTable), 0: a new table of this size
    pub tpl: usize,
    pub narray: u32,
    pub nhash: u32,
    pub states: Vec<SinkState>,
    /// a table carried around the loop (see `optimizer::sink`): (the
    /// table it was on entry, a Boolean that is true until the first
    /// iteration ends) - exits before then store that table itself
    pub entry: Option<(Ref, Ref)>,
}

/// A sunk table's contents at the exits through snapshot `snap`.
#[derive(Clone, Debug)]
pub struct SinkState {
    pub snap: u32,
    /// (hash node, value) stored by then
    pub fields: Vec<(u32, Ref)>,
    pub meta: Option<Ref>,
}

/// `Trace::exit_pcs` flag.
pub const FAST_EXIT: u32 = 0x8000_0000;

impl Trace {
    pub fn new(id: TraceId, proto: *mut Proto, start_pc: u32) -> Trace {
        Trace { id, proto, start_pc, insts: Vec::new(), snapshots: Vec::new(), loop_ref: None, state: TraceState::Recording, native: None, side_exits: 0, end_pc: u32::MAX, closure: 0, varargs: None, sinks: Vec::new(), link: None, link_cell: 0, root_key: (0, 0), spill_base: 0, reads: Vec::new(), phi_slots: Vec::new(), layout_hoisted: false, mem_hoisted: false, stack_need: 0, gc_refs: Vec::new(), protos: Vec::new(), fallback: None, side_of: None, tag: 0, link_table: 0, exit_counts: Vec::new(), exit_tries: Vec::new(), sides: Vec::new(), inserts: false, storm_cycles: 0, int_snaps: Vec::new(), no_int: 0, exit_regs: Vec::new(), inherit: Vec::new(), exit_pcs: Box::new([]), exit_seen: std::ptr::null_mut() }
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
        let _ = writeln!(s, "trace {} at pc {} tag {} side_of {:?}", self.id.0, self.start_pc, self.tag, self.side_of);
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
