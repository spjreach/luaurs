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
}

pub type TraceFn = unsafe extern "sysv64" fn(*mut crate::lobject::TValue, *mut crate::lobject::lua_State) -> u32;

pub fn compile(t: &Trace) -> Result<CompiledTrace, String> {
    super::ir::validate(t)?;
    #[cfg(all(target_arch = "x86_64", windows))]
    {
        let code = unsafe { x64::compile(t)? };
        let entry = crate::codegen::mem::with_code(|m| unsafe { m.place(&code.bytes) }).ok_or("out of executable memory")?;
        Ok(CompiledTrace { entry, size: code.bytes.len() })
    }
    #[cfg(not(all(target_arch = "x86_64", windows)))]
    {
        Err("no trace compiler for this platform".into())
    }
}
