//! Native code generation (port of Luau's `CodeGen` module, x64 Windows).
//!
//! Stage 1 is a baseline template compiler (see `x64.rs`): it removes
//! instruction dispatch and keeps the frame in registers, with inline code
//! for hot instructions and the interpreter's own single-instruction steps
//! for the rest. Enabled per chunk with [`compile`] (`luau --codegen`).

#![cfg(all(target_arch = "x86_64", windows))]

pub mod asm;
pub mod mem;
pub mod live;
pub mod state;
pub mod trans;
pub mod x64;

use crate::lobject::*;

/// True when native code generation is available on this platform.
/// Total bytes of native code generated so far in this process.
pub fn code_size() -> usize {
    mem::PLACED.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn supported() -> bool {
    mem::enter_stub() != 0
}

/// Debugging aid: `LUAU_CODEGEN_RANGE=lo:hi` compiles only the functions
/// whose sequence number (in compilation order) is in `lo..hi`.
fn in_debug_range() -> bool {
    static RANGE: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let r = RANGE.get_or_init(|| {
        let v = std::env::var("LUAU_CODEGEN_RANGE").ok()?;
        let (a, b) = v.split_once(':')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    });
    match r {
        None => true,
        Some((lo, hi)) => {
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            n >= *lo && n < *hi
        }
    }
}

/// `Proto::exectarget` bit: compile to native code on the first call (see
/// [`compile_lazy`]).
pub const LAZY_BIT: usize = 1 << 30;

/// Compiles `p` if it is still waiting for its first call; returns whether
/// it has native code now.
#[cold]
#[inline(never)]
pub unsafe fn lazy_compile(p: *mut Proto) -> bool {
    (*p).exectarget &= !LAZY_BIT;
    in_debug_range() && x64::compile_proto(p)
}

unsafe fn mark_tree(p: *mut Proto) {
    if (*p).execdata.is_null() && (*p).sizecode != 0 {
        (*p).exectarget |= LAZY_BIT;
    }
    for i in 0..(*p).sizep as usize {
        mark_tree(*(*p).p.add(i));
    }
}

/// Like [`compile`], but only the function itself is compiled now; nested
/// functions are compiled on their first call (most functions of a large
/// chunk never run, and compiling them all up front costs more than
/// parsing and compiling the source). `LUAU_CODEGEN_EAGER=1` compiles
/// everything at once instead.
pub unsafe fn compile_lazy(L: *mut lua_State, idx: i32) -> (u32, u32) {
    static EAGER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *EAGER.get_or_init(|| std::env::var_os("LUAU_CODEGEN_EAGER").is_some_and(|v| v == "1")) {
        return compile(L, idx);
    }
    let o = crate::lapi::luaA_toobject(L, idx);
    if o.is_null() || !ttisfunction(o) {
        return (0, 0);
    }
    let cl = clvalue(o);
    if (*cl).isC != 0 {
        return (0, 0);
    }
    let stub = mem::enter_stub();
    if stub == 0 {
        return (0, 0);
    }
    crate::lvmexecute::ENTER_STUB.store(stub, std::sync::atomic::Ordering::Relaxed);
    let p = Closure::proto(cl);
    for i in 0..(*p).sizep as usize {
        mark_tree(*(*p).p.add(i));
    }
    let ok = in_debug_range() && x64::compile_proto(p);
    (ok as u32, !ok as u32)
}

unsafe fn compile_tree(p: *mut Proto, stats: &mut (u32, u32)) {
    // (with the tracing JIT on, compiled loops count iterations and hand
    // hot ones to `jit::native_loop_hook`)
    if in_debug_range() && x64::compile_proto(p) {
        stats.0 += 1;
    } else {
        stats.1 += 1;
    }
    for i in 0..(*p).sizep as usize {
        compile_tree(*(*p).p.add(i), stats);
    }
}

/// `Luau::CodeGen::compile(L, idx)`: compiles the Lua function at stack
/// index `idx` and all functions nested in it. Returns (compiled, skipped).
pub unsafe fn compile(L: *mut lua_State, idx: i32) -> (u32, u32) {
    let o = crate::lapi::luaA_toobject(L, idx);
    if o.is_null() || !ttisfunction(o) {
        return (0, 0);
    }
    let cl = clvalue(o);
    if (*cl).isC != 0 {
        return (0, 0);
    }
    let stub = mem::enter_stub();
    if stub == 0 {
        return (0, 0);
    }
    crate::lvmexecute::ENTER_STUB.store(stub, std::sync::atomic::Ordering::Relaxed);
    let mut stats = (0, 0);
    compile_tree(Closure::proto(cl), &mut stats);
    stats
}

/// Stops running `p` natively (breakpoints). Native frames of `p` that are
/// suspended inside a helper call may still return into its code, so the
/// record is kept alive (leaked) rather than freed.
pub unsafe fn disable(p: *mut Proto) {
    (*p).exectarget &= !LAZY_BIT;
    if !(*p).execdata.is_null() {
        // keep the code alive: chain it to a parked list freed at exit
        PARKED.lock().unwrap().push((*p).execdata as usize);
    }
    (*p).execdata = std::ptr::null_mut();
}

/// Native code of functions taken back by the interpreter (breakpoints):
/// frames may still return into it, so it is never freed.
static PARKED: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// Releases a function's native code record (the machine code itself stays
/// in the code region).
pub unsafe fn free_execdata(p: *mut Proto) {
    if (*p).exectarget != 0 || !(*p).execdata.is_null() {
        x64::forget_feedback(p);
    }
    if !(*p).execdata.is_null() {
        drop(Box::from_raw((*p).execdata as *mut x64::NativeProto));
        (*p).execdata = std::ptr::null_mut();
    }
}
