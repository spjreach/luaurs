//! Port of `VM/src/ldo.cpp`: stack management, calls, protected execution,
//! coroutine resume/yield.
//!
//! Errors unwind like the C++ build without `LUA_USE_LONGJMP`: `luaD_throw`
//! raises a [`LuaException`] payload (via `resume_unwind`, which skips the
//! panic hook) and `luaD_rawrunprotected` catches it. Flags as in the
//! reference CLI: `LuauFastpcall`, `LuauPcallErrorContinuationReentry`,
//! `LuauIsYieldableSimplify` on.

use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

use crate::lapi::*;
use crate::ldebug::luaG_runerror;
use crate::lfunc::luaF_close;
use crate::lgc::*;
use crate::lmem::*;
use crate::lobject::*;
use crate::lstring::luaS_new;
use crate::ltable::luaH_getn;
use crate::lvmexecute::*;

pub const MAX_STACK_SIZE: i32 = (1024 / std::mem::size_of::<TValue>() as i32) * 1024 * 1024;

pub const PCRLUA: i32 = 0;
pub const PCRC: i32 = 1;
pub const PCRYIELD: i32 = 2;
pub const C_CALL_YIELD: i32 = -1;
pub const SCHEDULED_REENTRY: u8 = 0x7f;

pub const FINALLY_FINISHED: i32 = 0;
pub const FINALLY_ERROR: i32 = 1;
pub const FINALLY_CANCELLED: i32 = 2;

/// Unwinding payload of `luaD_throw` (`lua_exception`).
pub struct LuaException {
    pub L: *mut lua_State,
    pub status: i32,
}

unsafe impl Send for LuaException {}

// ---------------------------------------------------------------------------
// stack helpers (ldo.h macros)
// ---------------------------------------------------------------------------

#[inline(always)]
pub unsafe fn getgrownstacksize(L: *mut lua_State, n: i32) -> i32 {
    if n <= (*L).stacksize {
        2 * (*L).stacksize
    } else {
        (*L).stacksize + n
    }
}

#[inline(always)]
pub unsafe fn stacklimitreached(L: *mut lua_State, n: i32) -> bool {
    ((*L).stack_last as usize).wrapping_sub((*L).top as usize) as isize <= (n as isize) * std::mem::size_of::<TValue>() as isize
}

#[inline(always)]
pub unsafe fn luaD_checkstackfornewci(L: *mut lua_State, n: i32) {
    if stacklimitreached(L, n) {
        luaD_reallocstack(L, getgrownstacksize(L, n), true);
    }
}

#[inline(always)]
pub unsafe fn luaD_checkstack(L: *mut lua_State, n: i32) {
    if stacklimitreached(L, n) {
        luaD_growstack(L, n);
    }
}

#[inline(always)]
pub unsafe fn incr_top(L: *mut lua_State) {
    luaD_checkstack(L, 1);
    (*L).top = (*L).top.add(1);
}

#[inline(always)]
pub unsafe fn savestack(L: *mut lua_State, p: StkId) -> isize {
    (p as *mut u8).offset_from((*L).stack as *mut u8)
}

#[inline(always)]
pub unsafe fn restorestack(L: *mut lua_State, n: isize) -> StkId {
    ((*L).stack as *mut u8).offset(n) as StkId
}

#[inline(always)]
pub unsafe fn saveci(L: *mut lua_State, p: *mut CallInfo) -> isize {
    (p as *mut u8).offset_from((*L).base_ci as *mut u8)
}

#[inline(always)]
pub unsafe fn restoreci(L: *mut lua_State, n: isize) -> *mut CallInfo {
    ((*L).base_ci as *mut u8).offset(n) as *mut CallInfo
}

#[inline(always)]
pub unsafe fn expandstacklimit(L: *mut lua_State, p: StkId) {
    if (*(*L).ci).top < p {
        (*(*L).ci).top = p;
    }
}

#[inline(always)]
pub unsafe fn incr_ci(L: *mut lua_State) -> *mut CallInfo {
    if (*L).ci == (*L).end_ci {
        luaD_growCI(L)
    } else {
        (*L).ci = (*L).ci.add(1);
        (*L).ci
    }
}

#[inline(always)]
pub unsafe fn isyielded(L: *mut lua_State) -> bool {
    let s = (*L).status;
    s == LUA_YIELD as u8 || s == LUA_BREAK as u8 || s == SCHEDULED_REENTRY
}

// ---------------------------------------------------------------------------
// error recovery
// ---------------------------------------------------------------------------

// Lua errors use the reference VM's `LUA_USE_LONGJMP` scheme on x86-64
// Windows: `luaD_throw` jumps straight back to the innermost
// `luaD_rawrunprotected` instead of unwinding -- SEH unwinding costs
// microseconds per error. `luau_rt_try` saves the Win64 callee-saved state
// and runs the body with the SysV ABI (like the interpreter's handlers), so
// the body does not save xmm6-15 a second time; on return it restores what a
// SysV callee may clobber. Frames skipped by the jump must not own resources
// (error paths free their message strings before throwing). Genuine Rust
// panics are caught by the trampoline and re-raised once back in Rust.
// Elsewhere errors unwind as a `LuaException` panic payload.

#[cfg(all(target_arch = "x86_64", windows))]
std::arch::global_asm!(
    ".text",
    ".globl luau_rt_try",
    ".def luau_rt_try; .scl 2; .type 32; .endef",
    ".p2align 4",
    "luau_rt_try:",
    ".seh_proc luau_rt_try",
    "mov [rcx], rbx",
    "mov [rcx+8], rbp",
    "mov [rcx+16], rdi",
    "mov [rcx+24], rsi",
    "mov [rcx+32], r12",
    "mov [rcx+40], r13",
    "mov [rcx+48], r14",
    "mov [rcx+56], r15",
    "lea rax, [rsp+8]",
    "mov [rcx+64], rax",
    "mov rax, [rsp]",
    "mov [rcx+72], rax",
    "movdqa [rcx+80], xmm6",
    "movdqa [rcx+96], xmm7",
    "movdqa [rcx+112], xmm8",
    "movdqa [rcx+128], xmm9",
    "movdqa [rcx+144], xmm10",
    "movdqa [rcx+160], xmm11",
    "movdqa [rcx+176], xmm12",
    "movdqa [rcx+192], xmm13",
    "movdqa [rcx+208], xmm14",
    "movdqa [rcx+224], xmm15",
    "sub rsp, 40",
    ".seh_stackalloc 40",
    ".seh_endprologue",
    "mov rbx, rcx",
    "mov rdi, r8",
    "call rdx",
    "mov rdi, [rbx+16]",
    "mov rsi, [rbx+24]",
    "movdqa xmm6, [rbx+80]",
    "movdqa xmm7, [rbx+96]",
    "movdqa xmm8, [rbx+112]",
    "movdqa xmm9, [rbx+128]",
    "movdqa xmm10, [rbx+144]",
    "movdqa xmm11, [rbx+160]",
    "movdqa xmm12, [rbx+176]",
    "movdqa xmm13, [rbx+192]",
    "movdqa xmm14, [rbx+208]",
    "movdqa xmm15, [rbx+224]",
    "mov rbx, [rbx]",
    "xor eax, eax",
    "add rsp, 40",
    "ret",
    ".seh_endproc",
    "",
    ".globl luau_rt_longjmp",
    ".def luau_rt_longjmp; .scl 2; .type 32; .endef",
    ".p2align 4",
    "luau_rt_longjmp:",
    "mov rbx, [rcx]",
    "mov rbp, [rcx+8]",
    "mov rdi, [rcx+16]",
    "mov rsi, [rcx+24]",
    "mov r12, [rcx+32]",
    "mov r13, [rcx+40]",
    "mov r14, [rcx+48]",
    "mov r15, [rcx+56]",
    "movdqa xmm6, [rcx+80]",
    "movdqa xmm7, [rcx+96]",
    "movdqa xmm8, [rcx+112]",
    "movdqa xmm9, [rcx+128]",
    "movdqa xmm10, [rcx+144]",
    "movdqa xmm11, [rcx+160]",
    "movdqa xmm12, [rcx+176]",
    "movdqa xmm13, [rcx+192]",
    "movdqa xmm14, [rcx+208]",
    "movdqa xmm15, [rcx+224]",
    "mov rsp, [rcx+64]",
    "mov eax, 1",
    "jmp qword ptr [rcx+72]",
);

#[cfg(all(target_arch = "x86_64", windows))]
extern "C" {
    /// Saves the callee-saved state into `buf` and calls the SysV function
    /// `f(ud)`; returns 0 when it returns, 1 after `luau_rt_longjmp(buf)`.
    fn luau_rt_try(buf: *mut LuaJmpBuf, f: *const u8, ud: *mut u8) -> i32;
    fn luau_rt_longjmp(buf: *mut LuaJmpBuf) -> !;
}

/// Node of the `global_State::cleanups` stack (see `errguard!`).
#[repr(C)]
pub struct ErrCleanup {
    prev: *mut ErrCleanup,
    run: unsafe fn(*mut ErrCleanup),
}

/// Frees `*val` if a Lua error longjmps past the frame owning it; on normal
/// exit (or a real unwind) it just unregisters. Create with `errguard!`, and
/// never move the guarded value while the guard is alive.
#[repr(C)]
pub struct DropGuard<T> {
    node: ErrCleanup,
    g: *mut global_State,
    val: *mut T,
}

impl<T> DropGuard<T> {
    #[inline(always)]
    pub unsafe fn new(L: *mut lua_State, val: *mut T) -> Self {
        unsafe fn run<T>(c: *mut ErrCleanup) {
            std::ptr::drop_in_place((*(c as *mut DropGuard<T>)).val);
        }
        DropGuard { node: ErrCleanup { prev: std::ptr::null_mut(), run: run::<T> }, g: (*L).global, val }
    }
    /// Registers the guard; it must not move afterwards.
    #[inline(always)]
    pub unsafe fn arm(&mut self) {
        self.node.prev = (*self.g).cleanups;
        (*self.g).cleanups = &mut self.node;
    }
}

impl<T> Drop for DropGuard<T> {
    #[inline(always)]
    fn drop(&mut self) {
        unsafe { (*self.g).cleanups = self.node.prev }
    }
}

/// `errguard!(L, v)`: frees local `v` if a Lua error jumps out of this frame.
#[macro_export]
macro_rules! errguard {
    ($L:expr, $v:ident) => {
        let mut __errguard = $crate::ldo::DropGuard::new($L, ::core::ptr::addr_of_mut!($v));
        __errguard.arm();
    };
}

/// `lua_jmpbuf`: saved registers of a protected call (layout used by the
/// assembly above) plus the error chain.
#[repr(C, align(16))]
pub struct LuaJmpBuf {
    regs: [u64; 10],
    xmm: [u128; 10],
    prev: *mut LuaJmpBuf,
    status: i32,
    /// false: a `catch_unwind` protector (errors must unwind to it)
    jumpable: bool,
    /// `global_State::cleanups` when the protector was entered
    cleanmark: *mut ErrCleanup,
    panic: Option<Box<dyn std::any::Any + Send + 'static>>,
}

/// `luaD_rawrunprotected` with a longjmp target: entering costs a few ns
/// more than the zero-cost `catch_unwind` protector, but an error is caught
/// ~100x faster. Used where errors are expected (pcall, error handling).
#[cfg(all(target_arch = "x86_64", windows))]
#[inline]
pub unsafe fn luaD_rawrunprotected_jmp<F: FnMut()>(L: *mut lua_State, mut f: F) -> i32 {
    struct Ctx<F> {
        f: F,
        jb: *mut LuaJmpBuf,
    }
    unsafe extern "sysv64" fn tramp<F: FnMut()>(ud: *mut Ctx<F>) {
        let ctx = &mut *ud;
        if let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (ctx.f)())) {
            (*ctx.jb).panic = Some(p);
        }
    }
    let g = (*L).global;
    let mut jb: std::mem::MaybeUninit<LuaJmpBuf> = std::mem::MaybeUninit::uninit();
    let jbp = jb.as_mut_ptr();
    let prev = (*g).errorjmp;
    std::ptr::addr_of_mut!((*jbp).prev).write(prev);
    std::ptr::addr_of_mut!((*jbp).status).write(0);
    std::ptr::addr_of_mut!((*jbp).jumpable).write(true);
    std::ptr::addr_of_mut!((*jbp).cleanmark).write((*g).cleanups);
    std::ptr::addr_of_mut!((*jbp).panic).write(None);
    let mut ctx = Ctx { f: &mut f, jb: jbp };
    (*g).errorjmp = jbp;
    let r = luau_rt_try(jbp, tramp::<&mut F> as *const u8, &mut ctx as *mut Ctx<&mut F> as *mut u8);
    (*g).errorjmp = prev;
    if let Some(p) = std::ptr::read_volatile(std::ptr::addr_of!((*jbp).panic)) {
        resume_unwind(p);
    }
    if r == 0 { 0 } else { std::ptr::read_volatile(std::ptr::addr_of!((*jbp).status)) }
}

/// `luaD_rawrunprotected`: runs `f`, returning the status of a Lua error it
/// raised (0 when none). Zero-cost when no error occurs; an error raised
/// directly inside unwinds to it. Non-Lua panics (bugs) keep unwinding.
#[inline]
pub unsafe fn luaD_rawrunprotected<F: FnMut()>(L: *mut lua_State, f: F) -> i32 {
    #[cfg(all(target_arch = "x86_64", windows))]
    let (g, prev, mut jb) = {
        // chain a marker so that luaD_throw knows to unwind to us
        let g = (*L).global;
        let prev = (*g).errorjmp;
        let jb: std::mem::MaybeUninit<LuaJmpBuf> = std::mem::MaybeUninit::uninit();
        (g, prev, jb)
    };
    #[cfg(all(target_arch = "x86_64", windows))]
    {
        let jbp = jb.as_mut_ptr();
        std::ptr::addr_of_mut!((*jbp).jumpable).write(false);
        (*g).errorjmp = jbp;
    }
    let r = catch_unwind(AssertUnwindSafe(f));
    #[cfg(all(target_arch = "x86_64", windows))]
    {
        (*g).errorjmp = prev;
    }
    let _ = L;
    match r {
        Ok(()) => 0,
        Err(payload) => match payload.downcast::<LuaException>() {
            Ok(e) => e.status,
            Err(other) => resume_unwind(other),
        },
    }
}

#[cfg(not(all(target_arch = "x86_64", windows)))]
pub use luaD_rawrunprotected as luaD_rawrunprotected_jmp;

#[cold]
pub unsafe fn luaD_throw(L: *mut lua_State, errcode: i32) -> ! {
    #[cfg(all(target_arch = "x86_64", windows))]
    {
        let jb = (*(*L).global).errorjmp;
        if !jb.is_null() && (*jb).jumpable {
            // free what the frames being skipped own (they are still intact)
            let g = (*L).global;
            while (*g).cleanups != (*jb).cleanmark {
                let c = (*g).cleanups;
                (*g).cleanups = (*c).prev;
                ((*c).run)(c);
            }
            (*jb).status = errcode;
            luau_rt_longjmp(jb);
        }
    }
    resume_unwind(Box::new(LuaException { L, status: errcode }))
}

unsafe fn correctstack(L: *mut lua_State, oldstack: *mut TValue) {
    let newstack = (*L).stack;
    let fix = |p: StkId| -> StkId { newstack.offset(p.offset_from(oldstack)) };
    (*L).top = fix((*L).top);
    let mut up = (*L).openupval;
    while !up.is_null() {
        (*up).v = fix((*up).v);
        up = (*up).u.open.threadnext;
    }
    let mut ci = (*L).base_ci;
    while ci <= (*L).ci {
        (*ci).top = fix((*ci).top);
        (*ci).base = fix((*ci).base);
        (*ci).func = fix((*ci).func);
        ci = ci.add(1);
    }
    (*L).base = fix((*L).base);
}

pub unsafe fn luaD_reallocstack(L: *mut lua_State, newsize: i32, fornewci: bool) {
    if newsize > MAX_STACK_SIZE {
        if fornewci {
            let cip = (*L).ci.sub(1);
            (*L).ci = cip;
            (*L).base = (*cip).base;
            (*L).top = (*cip).top;
        }
        luaD_throw(L, LUA_ERRMEM);
    }
    let oldstack = (*L).stack;
    let realsize = newsize + EXTRA_STACK;
    (*L).stack = luaM_reallocarray(L, (*L).stack, (*L).stacksize as usize, realsize as usize, (*L).hdr.memcat);
    let newstack = (*L).stack;
    for i in (*L).stacksize..realsize {
        setnilvalue(newstack.add(i as usize));
    }
    (*L).stacksize = realsize;
    (*L).stack_last = newstack.add(newsize as usize);
    correctstack(L, oldstack);
}

pub unsafe fn luaD_reallocCI(L: *mut lua_State, newsize: i32) {
    let oldci = (*L).base_ci;
    (*L).base_ci = luaM_reallocarray(L, (*L).base_ci, (*L).size_ci as usize, newsize as usize, (*L).hdr.memcat);
    (*L).size_ci = newsize;
    (*L).ci = (*L).base_ci.offset((*L).ci.offset_from(oldci));
    (*L).end_ci = (*L).base_ci.add((*L).size_ci as usize - 1);
}

pub unsafe fn luaD_growstack(L: *mut lua_State, n: i32) {
    luaD_reallocstack(L, getgrownstacksize(L, n), false);
}

pub unsafe fn luaD_growCI(L: *mut lua_State) -> *mut CallInfo {
    let hardlimit = LUAI_MAXCALLS + (LUAI_MAXCALLS >> 3);
    if (*L).size_ci >= hardlimit {
        luaD_throw(L, LUA_ERRERR);
    }
    let request = (*L).size_ci * 2;
    luaD_reallocCI(
        L,
        if (*L).size_ci >= LUAI_MAXCALLS {
            hardlimit
        } else if request < LUAI_MAXCALLS {
            request
        } else {
            LUAI_MAXCALLS
        },
    );
    if (*L).size_ci > LUAI_MAXCALLS {
        luaG_runerror(L, "stack overflow");
    }
    (*L).ci = (*L).ci.add(1);
    (*L).ci
}

pub unsafe fn luaD_checkCstack(L: *mut lua_State) {
    let hardlimit = LUAI_MAXCCALLS + (LUAI_MAXCCALLS >> 3);
    if (*L).nCcalls as i32 == LUAI_MAXCCALLS {
        luaG_runerror(L, "C stack overflow");
    } else if (*L).nCcalls as i32 >= hardlimit {
        luaD_throw(L, LUA_ERRERR);
    }
}

unsafe fn performcall(L: *mut lua_State, func: StkId, nresults: i32, preparereentry: bool) {
    if luau_precall(L, func, nresults) == PCRLUA {
        (*(*L).ci).flags |= LUA_CALLINFO_RETURN;
        let oldactive = (*L).isactive;
        (*L).isactive = true;
        luaC_threadbarrier(L);
        if preparereentry {
            (*L).status = SCHEDULED_REENTRY;
        } else {
            let basecioffset = saveci(L, (*L).ci.sub(1));
            luau_execute(L);
            resume_continue(L, basecioffset);
        }
        if !oldactive {
            (*L).isactive = false;
        }
    }
}

/// `luaD_performcally`: a yieldable call from an opcode (metamethod); true
/// when the call yielded.
pub unsafe fn luaD_performcally(L: *mut lua_State, func: StkId, nresults: i32) -> bool {
    (*L).nCcalls += 1;
    if (*L).nCcalls as i32 >= LUAI_MAXCCALLS {
        luaD_checkCstack(L);
    }
    (*L).baseCcalls += 1;
    (*(*L).ci).flags |= LUA_CALLINFO_OPYIELD;
    performcall(L, func, nresults, false);
    if (*L).status != LUA_OK as u8 {
        return true;
    }
    (*(*L).ci).flags &= !LUA_CALLINFO_OPYIELD;
    (*L).baseCcalls -= 1;
    (*L).nCcalls -= 1;
    luaC_checkGC(L);
    false
}

pub unsafe fn luaD_callint(L: *mut lua_State, func: StkId, nresults: i32, preparereentry: bool) {
    (*L).nCcalls += 1;
    if (*L).nCcalls as i32 >= LUAI_MAXCCALLS {
        luaD_checkCstack(L);
    }
    let mut fromyieldableccall = false;
    if (*L).ci != (*L).base_ci {
        let ccl = clvalue((*(*L).ci).func);
        if (*ccl).isC != 0 && Closure::c(ccl).cont.is_some() {
            fromyieldableccall = true;
            (*L).baseCcalls += 1;
        }
    }
    let funcoffset = savestack(L, func);
    let cioffset = saveci(L, (*L).ci);
    performcall(L, func, nresults, preparereentry);
    let yielded = isyielded(L);
    if fromyieldableccall {
        (*L).baseCcalls -= 1;
        if yielded {
            let callerci = restoreci(L, cioffset);
            (*callerci).top = restorestack(L, funcoffset).add(if nresults != LUA_MULTRET { nresults as usize } else { 0 });
        }
    }
    if nresults != LUA_MULTRET && !yielded {
        (*L).top = restorestack(L, funcoffset).add(nresults as usize);
    }
    (*L).nCcalls -= 1;
    luaC_checkGC(L);
}

#[inline]
pub unsafe fn luaD_call(L: *mut lua_State, func: StkId, nresults: i32) {
    luaD_callint(L, func, nresults, false)
}

/// Non-yieldable call (metamethods from C helpers).
pub unsafe fn luaD_callny(L: *mut lua_State, func: StkId, nresults: i32) {
    (*L).nCcalls += 1;
    if (*L).nCcalls as i32 >= LUAI_MAXCCALLS {
        luaD_checkCstack(L);
    }
    let funcoffset = savestack(L, func);
    performcall(L, func, nresults, false);
    if nresults != LUA_MULTRET {
        (*L).top = restorestack(L, funcoffset).add(nresults as usize);
    }
    (*L).nCcalls -= 1;
    luaC_checkGC(L);
}

pub unsafe fn luaD_seterrorobj(L: *mut lua_State, errcode: i32, oldtop: StkId) {
    match errcode {
        LUA_ERRMEM => setsvalue(oldtop, luaS_new(L, LUA_MEMERRMSG)),
        LUA_ERRERR => setsvalue(oldtop, luaS_new(L, LUA_ERRERRMSG)),
        LUA_ERRSYNTAX | LUA_ERRRUN => setobj(oldtop, (*L).top.sub(1)),
        _ => {}
    }
    (*L).top = oldtop.add(1);
}

pub unsafe fn luaD_preparefinalizestate(L: *mut lua_State, co: *mut lua_State, finresult: i32) {
    sethvalue((*L).top, (*co).finalizers);
    (*L).top = (*L).top.add(1);
    setnvalue((*L).top, luaH_getn((*co).finalizers) as f64);
    (*L).top = (*L).top.add(1);
    setnvalue((*L).top, finresult as f64);
    (*L).top = (*L).top.add(1);
    (*co).finalizers = std::ptr::null_mut();
}

pub unsafe fn luaD_preparefinalize(L: *mut lua_State, co: *mut lua_State, finresult: i32) {
    let nres = if finresult == FINALLY_FINISHED { (*co).top.offset_from((*co).base) as i32 } else { 1 };
    if lua_checkstack(L, nres + 3) == 0 {
        luaG_runerror(L, "too many results to invoke finalizer");
    }
    luaD_preparefinalizestate(L, co, finresult);
    lua_xmove(co, L, nres);
}

pub unsafe fn luaD_runfinalizers(L: *mut lua_State, returnstatus: bool) -> i32 {
    let results = lua_gettop(L) - 4;
    let position = lua_tointeger(L, 3);
    let status = lua_tointeger(L, 4);
    if position != 0 {
        if lua_checkstack(L, results + 2) == 0 {
            lua_pushstring(L, "too many results to invoke finalizer");
            lua_error(L);
        }
        lua_pushinteger(L, position - 1);
        lua_replace(L, 3);
        lua_rawgeti(L, 2, position);
        lua_pushstring(
            L,
            if status == FINALLY_CANCELLED {
                "cancelled"
            } else if status == FINALLY_FINISHED {
                "finished"
            } else {
                "error"
            },
        );
        for i in 0..results {
            lua_pushvalue(L, 5 + i);
        }
        return lua_pcallyieldable(L, results + 1, 0, 0);
    }
    if returnstatus {
        setbvalue((*L).base.add(3), status != FINALLY_ERROR);
        return results + 1;
    }
    if status == FINALLY_ERROR {
        lua_error(L);
    }
    results
}

#[inline(always)]
unsafe fn resume_continue(L: *mut lua_State, basecioffset: isize) {
    while ((*L).status == LUA_OK as u8 || (*L).status == SCHEDULED_REENTRY) && (*L).ci > restoreci(L, basecioffset) {
        (*L).status = LUA_OK as u8;
        let cl = curr_func(L);
        if (*cl).isC != 0 {
            if (*(*L).ci).flags & LUA_CALLINFO_PCALL != 0 {
                luau_pospcallsuccess(L);
                continue;
            }
            (*(*L).ci).flags &= !LUA_CALLINFO_HANDLE;
            let cont = Closure::c(cl).cont.unwrap();
            let n = cont(L, 0);
            if (*L).status == LUA_BREAK as u8 || (*L).status == LUA_YIELD as u8 {
                break;
            }
            if (*L).status == SCHEDULED_REENTRY {
                continue;
            }
            luau_poscall(L, (*L).top.sub(n as usize));
        } else {
            if (*(*L).ci).flags & LUA_CALLINFO_OPYIELD != 0 {
                luau_finishop(L);
            }
            luau_execute(L);
        }
    }
}

#[inline(always)]
unsafe fn resume(L: *mut lua_State, mut firstArg: StkId) {
    if (*L).status == LUA_OK as u8 {
        if firstArg == (*L).base {
            luaG_runerror(L, "cannot resume dead coroutine");
        }
        let precallresult = luau_precall(L, firstArg.sub(1), LUA_MULTRET);
        if (*L).status == SCHEDULED_REENTRY {
            firstArg = (*L).base;
        } else {
            if precallresult != PCRLUA {
                return;
            }
            (*(*L).ci).flags |= LUA_CALLINFO_RETURN;
        }
    }
    if (*L).status != LUA_OK as u8 {
        (*L).status = LUA_OK as u8;
        let cl = curr_func(L);
        if (*cl).isC != 0 {
            if Closure::c(cl).cont.is_none() {
                luau_poscall(L, firstArg);
            } else {
                (*L).base = (*(*L).ci).base;
            }
        } else {
            (*L).base = (*(*L).ci).base;
        }
    }
    resume_continue(L, 0);
}

unsafe fn resume_findhandler(L: *mut lua_State) -> *mut CallInfo {
    let mut ci = (*L).ci;
    while ci > (*L).base_ci {
        if (*ci).flags & LUA_CALLINFO_HANDLE != 0 {
            return ci;
        }
        ci = ci.sub(1);
    }
    std::ptr::null_mut()
}

unsafe fn restore_stack_limit(L: *mut lua_State) {
    if (*L).size_ci > LUAI_MAXCALLS {
        let inuse = (*L).ci.offset_from((*L).base_ci) as i32;
        if inuse + 1 < LUAI_MAXCALLS {
            luaD_reallocCI(L, LUAI_MAXCALLS);
        }
    }
}

unsafe fn callerrfunc(L: *mut lua_State, errfunc: StkId) {
    setobj((*L).top, (*L).top.sub(1));
    setobj((*L).top.sub(1), errfunc);
    incr_top(L);
    luaD_callny(L, (*L).top.sub(2), 1);
}

unsafe fn resume_handle(L: *mut lua_State, mut ci: *mut CallInfo) {
    let cl = ci_func(ci);
    (*ci).flags &= !LUA_CALLINFO_HANDLE;
    let mut status = (*L).status as i32;
    (*L).status = LUA_OK as u8;
    if status != LUA_ERRRUN {
        luaD_seterrorobj(L, status, (*L).top);
    }
    if (*ci).errfunc() != 0 {
        let old_ci = saveci(L, ci);
        let ef = (*ci).base.add((*ci).errfunc() as usize - 1);
        let err = luaD_rawrunprotected_jmp(L, &mut || callerrfunc(L, ef));
        status = if err == 0 {
            LUA_ERRRUN
        } else if status == LUA_ERRMEM && err == LUA_ERRMEM {
            LUA_ERRMEM
        } else {
            LUA_ERRERR
        };
        luaD_seterrorobj(L, status, (*L).top.sub(1));
        ci = restoreci(L, old_ci);
        (*ci).set_errfunc(0);
    }
    (*L).nCcalls = (*L).baseCcalls;
    (*L).ci = ci;
    luaF_close(L, (*(*L).ci).base);
    (*L).base = (*ci).base;
    (*ci).top = (*L).top;
    restore_stack_limit(L);
    let cont = Closure::c(cl).cont.unwrap();
    let n = cont(L, status);
    if (*L).status == SCHEDULED_REENTRY {
        resume_continue(L, 0);
        return;
    }
    if (*L).status != LUA_OK as u8 {
        return;
    }
    luau_poscall(L, (*L).top.sub(n as usize));
    resume_continue(L, 0);
}

unsafe fn resume_error(L: *mut lua_State, msg: &str, narg: i32) -> i32 {
    (*L).top = (*L).top.sub(narg as usize);
    setsvalue((*L).top, luaS_new(L, msg));
    incr_top(L);
    LUA_ERRRUN
}

#[inline]
unsafe fn resume_start(L: *mut lua_State, from: *mut lua_State, nargs: i32) -> i32 {
    let st = (*L).status as i32;
    if st != LUA_YIELD && st != LUA_BREAK && (st != 0 || (*L).ci != (*L).base_ci) {
        return resume_error(L, "cannot resume non-suspended coroutine", nargs);
    }
    (*L).nCcalls = if from.is_null() { 0 } else { (*from).nCcalls };
    if (*L).nCcalls as i32 >= LUAI_MAXCCALLS {
        return resume_error(L, "C stack overflow", nargs);
    }
    (*L).nCcalls += 1;
    (*L).baseCcalls = (*L).nCcalls;
    (*L).isactive = true;
    luaC_threadbarrier(L);
    LUA_OK
}

#[inline]
unsafe fn resume_finish(L: *mut lua_State, mut status: i32, oldnCcalls: i32) -> i32 {
    loop {
        if status == LUA_OK {
            break;
        }
        let ch = resume_findhandler(L);
        if ch.is_null() {
            break;
        }
        if lua_isyieldable(L) != 0 {
            if let Some(f) = (*(*L).global).cb.debugprotectederror {
                f(L);
                if (*L).status == LUA_BREAK as u8 {
                    status = LUA_OK;
                    break;
                }
            }
        }
        (*L).baseCcalls = oldnCcalls as u16;
        (*L).status = status as u8;
        status = luaD_rawrunprotected_jmp(L, &mut || resume_handle(L, ch));
    }
    (*L).nCcalls = (oldnCcalls - 1) as u16;
    (*L).baseCcalls = (*L).nCcalls;
    (*L).isactive = false;
    if status != LUA_OK {
        (*L).status = status as u8;
        luaD_seterrorobj(L, status, (*L).top);
        (*(*L).ci).top = (*L).top;
    } else if (*L).status == LUA_OK as u8 {
        expandstacklimit(L, (*L).top);
    }
    (*L).status as i32
}

#[inline]
pub unsafe fn lua_resume(L: *mut lua_State, from: *mut lua_State, nargs: i32) -> i32 {
    let starterror = resume_start(L, from, nargs);
    if starterror != 0 {
        return starterror;
    }
    let oldnCcalls = (*L).nCcalls as i32;
    let first = (*L).top.sub(nargs as usize);
    let status = luaD_rawrunprotected(L, &mut || resume(L, first));
    resume_finish(L, status, oldnCcalls)
}

pub unsafe fn lua_resumeerror(L: *mut lua_State, from: *mut lua_State) -> i32 {
    let starterror = resume_start(L, from, 1);
    if starterror != 0 {
        return starterror;
    }
    let oldnCcalls = (*L).nCcalls as i32;
    let mut status = LUA_ERRRUN;
    let ci = resume_findhandler(L);
    if !ci.is_null() {
        (*L).status = status as u8;
        status = luaD_rawrunprotected_jmp(L, &mut || resume_handle(L, ci));
    }
    resume_finish(L, status, oldnCcalls)
}

pub unsafe fn lua_yield(L: *mut lua_State, nresults: i32) -> i32 {
    if (*L).nCcalls > (*L).baseCcalls {
        luaG_runerror(L, "attempt to yield across metamethod/C-call boundary");
    }
    (*L).base = (*L).top.sub(nresults as usize);
    (*L).status = LUA_YIELD as u8;
    -1
}

pub unsafe fn lua_break(L: *mut lua_State) -> i32 {
    if (*L).nCcalls > (*L).baseCcalls {
        luaG_runerror(L, "attempt to break across metamethod/C-call boundary");
    }
    (*L).status = LUA_BREAK as u8;
    -1
}

pub unsafe fn lua_isyieldable(L: *mut lua_State) -> i32 {
    ((*L).nCcalls == (*L).baseCcalls) as i32
}

/// `luaD_pcall`: runs `func` protected; on error restores the call state,
/// runs the error handler at stack offset `ef` (if nonzero) and leaves the
/// error object at `old_top`.
pub unsafe fn luaD_pcall(L: *mut lua_State, func: &mut dyn FnMut(), old_top: isize, ef: isize) -> i32 {
    let oldnCcalls = (*L).nCcalls;
    let oldbaseCcalls = (*L).baseCcalls;
    let old_ci = saveci(L, (*L).ci);
    let oldactive = (*L).isactive;
    let mut status = luaD_rawrunprotected_jmp(L, func);
    if status != 0 {
        let mut errstatus = status;
        if ef != 0 {
            if status != LUA_ERRRUN {
                luaD_seterrorobj(L, status, (*L).top);
            }
            let errf = restorestack(L, ef);
            let err = luaD_rawrunprotected_jmp(L, &mut || callerrfunc(L, errf));
            if err == 0 {
                errstatus = LUA_ERRRUN;
            } else if status == LUA_ERRMEM && err == LUA_ERRMEM {
                errstatus = LUA_ERRMEM;
            } else {
                errstatus = LUA_ERRERR;
                status = LUA_ERRERR;
            }
        }
        if !oldactive {
            (*L).isactive = false;
        }
        let yieldable = lua_isyieldable(L) != 0;
        (*L).nCcalls = oldnCcalls;
        (*L).baseCcalls = oldbaseCcalls;
        if yieldable {
            if let Some(f) = (*(*L).global).cb.debugprotectederror {
                f(L);
                if (*L).status == LUA_BREAK as u8 {
                    return 0;
                }
            }
        }
        let oldtop = restorestack(L, old_top);
        luaF_close(L, oldtop);
        luaD_seterrorobj(L, errstatus, oldtop);
        (*L).ci = restoreci(L, old_ci);
        (*L).base = (*(*L).ci).base;
        restore_stack_limit(L);
    }
    status
}
