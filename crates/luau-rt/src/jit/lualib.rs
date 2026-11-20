//! Luau-visible controls for the tracing JIT.
//!
//! This library is installed by `luaL_openextlibs`; it remains available while
//! trace recording is under development so scripts and tests can inspect the
//! current tracing configuration and counters.

use crate::lapi::*;
use crate::lobject::lua_State;
use crate::ltasklib::set_fn;

use super::state;

unsafe fn jit_on(L: *mut lua_State) -> i32 {
    state::set_enabled(true);
    lua_pushboolean(L, 1);
    1
}

unsafe fn jit_off(L: *mut lua_State) -> i32 {
    state::set_enabled(false);
    lua_pushboolean(L, 0);
    1
}

/// Return `(enabled, compiled_trace_count, aborted_trace_count)`.
unsafe fn jit_status(L: *mut lua_State) -> i32 {
    let (enabled, compiled, aborted) = state::with(|jit| {
        (
            jit.enabled,
            jit.traces.len() as i64,
            jit.aborts.len() as i64,
        )
    });
    lua_pushboolean(L, enabled as i32);
    lua_pushinteger64(L, compiled);
    lua_pushinteger64(L, aborted);
    3
}

unsafe fn jit_flush(_L: *mut lua_State) -> i32 {
    state::flush();
    0
}

pub unsafe fn luaopen_jit(L: *mut lua_State) -> i32 {
    lua_newtable(L);
    set_fn(L, "on", jit_on);
    set_fn(L, "off", jit_off);
    set_fn(L, "status", jit_status);
    set_fn(L, "flush", jit_flush);
    lua_setglobal(L, "jit");
    0
}
