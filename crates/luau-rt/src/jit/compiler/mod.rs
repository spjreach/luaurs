//! Trace -> native code.

#[cfg(all(target_arch = "x86_64", windows))]
pub mod x64;

use super::trace::Trace;

/// Native code for a trace. `entry` is an
/// `extern "sysv64" fn(base: StkId, L: *mut lua_State) -> u32` returning the
/// snapshot it exited through (high bit set: interrupted at the loop header,
/// low bits = the loop-closing snapshot whose slots were written back).
#[derive(Clone, Copy)]
pub struct CompiledTrace {
    pub entry: usize,
    pub size: usize,
    /// root trace: where a side trace jumps to run it again (after the
    /// prologue: base and L already in r13 / r12)
    pub reentry: usize,
}

pub type TraceFn = unsafe extern "sysv64" fn(*mut crate::lobject::TValue, *mut crate::lobject::lua_State) -> u32;

/// How a trace ends: looping, jumping to its root's re-entry (a side
/// trace), or jumping through a link cell to another loop's trace.
#[derive(Clone, Copy, PartialEq)]
pub enum End {
    Loop,
    Root(usize),
    Cell(usize),
}

/// `t.tag` and `t.link_table` must be set. Side traces (`t.side_of`) have
/// no prologue: they run in their root's frame.
pub fn compile(t: &Trace, end: End) -> Result<CompiledTrace, String> {
    super::ir::validate(t)?;
    #[cfg(all(target_arch = "x86_64", windows))]
    {
        let code = unsafe { x64::compile(t, end)? };
        // (LUAU_TRACE_ASM=dir: the machine code, for a disassembler)
        if let Some(dir) = std::env::var_os("LUAU_TRACE_ASM") {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(std::path::Path::new(&dir).join(format!("trace-{}{}.bin", t.id.0, if t.side_of.is_some() { "s" } else { "" })), &code.bytes);
        }
        let entry = crate::codegen::mem::with_code(|m| unsafe { m.place(&code.bytes) }).ok_or("out of executable memory")?;
        Ok(CompiledTrace { entry, size: code.bytes.len(), reentry: entry + code.reentry })
    }
    #[cfg(not(all(target_arch = "x86_64", windows)))]
    {
        let _ = end;
        Err("no trace compiler for this platform".into())
    }
}
