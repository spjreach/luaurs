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

/// A call frame of a function inlined into the trace, live at a snapshot:
/// leaving the trace there recreates its CallInfo.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapFrame {
    /// the frame's base, as a slot of the trace's frame (the function value
    /// is in slot `base - 1`)
    pub base: u16,
    /// the called closure
    pub func: usize,
    /// bytecode index in the caller to return to (after its CALL)
    pub ret_pc: u32,
    /// results the caller wants (the CALL's C - 1, never LUA_MULTRET)
    pub nresults: i32,
    /// arguments passed (the frame's top is base + nargs + stacksize)
    pub nargs: u16,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Snapshot {
    /// index of the bytecode instruction to resume at (in the trace's proto)
    pub pc: u32,
    /// slots to restore, sorted by slot
    pub entries: Vec<SnapEntry>,
    /// the first IR instruction *not* covered: values defined at or after
    /// this ref aren't available yet when this snapshot is taken
    pub ref_limit: Ref,
    /// inlined call frames, outermost first (`pc` is then in the innermost
    /// frame's function)
    pub frames: Vec<SnapFrame>,
    /// the stack top (a trace slot) between a call returning a variable
    /// number of values and the instruction taking them (L->top on exit)
    pub top: Option<u16>,
}
