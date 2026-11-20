//! Trace -> x64 (in progress).

use super::trace::Trace;

/// Native code for a trace: entered at `entry` with the interpreter's
/// (L, base) and returning at a side exit.
pub struct CompiledTrace {
    pub entry: usize,
    pub size: usize,
}

pub fn compile(t: &Trace) -> Result<CompiledTrace, String> {
    super::ir::validate(t)?;
    Err("trace compiler not implemented yet".into())
}
