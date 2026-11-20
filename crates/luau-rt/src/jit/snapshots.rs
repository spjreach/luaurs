//! Snapshots: the interpreter state at a point of the trace.
//!
//! A snapshot records, for every stack slot the trace has *modified* since
//! entry (or loaded and may keep in a register), which IR value it holds,
//! plus the bytecode pc to resume at. Slots not listed still hold what the
//! interpreter left there. Leaving the trace through a snapshot ("side
//! exit") writes those values back to the stack (boxing numbers etc.) and
//! continues interpreting at `pc`.

use super::ir::instruction::Ref;

#[derive(Clone, Debug, PartialEq)]
pub struct SnapEntry {
    /// stack slot, relative to the trace's frame base
    pub slot: u16,
    pub value: Ref,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// index of the bytecode instruction to resume at (in the trace's proto)
    pub pc: u32,
    /// slots to restore, sorted by slot
    pub entries: Vec<SnapEntry>,
    /// the first IR instruction *not* covered: values defined at or after
    /// this ref aren't available yet when this snapshot is taken
    pub ref_limit: Ref,
}
