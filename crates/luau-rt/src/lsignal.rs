//! `Signal` objects: connect callbacks, fire them, wait for the next fire
//! (Roblox-style API, not part of standard Luau; opened by
//! `luaL_openextlibs`). Callbacks and waiting threads run through the `task`
//! scheduler (see `ltasklib`).

use crate::lapi::*;
use crate::laux::*;
use crate::lobject::*;
use crate::ltasklib::{is_suspended, park_and_yield, resume, set_fn};

const REG_SIGNALS: &str = "luau.task.signals"; // weak-keyed: signal -> state table
const REG_SIGNAL_MT: &str = "luau.task.signalmt";
const REG_CONN_MT: &str = "luau.task.connmt";

// -- Signal --------------------------------------------------------------------------

/// Pushes the state table of the signal at `idx` (errors for anything else).
unsafe fn signal_state(L: *mut lua_State, idx: i32) {
    let idx = lua_absindex(L, idx);
    lua_getfield(L, LUA_REGISTRYINDEX, REG_SIGNALS);
    lua_pushvalue(L, idx);
    lua_rawget(L, -2);
    lua_remove(L, -2);
    if !lua_istable(L, -1) {
        luaL_error(L, "expected a Signal (use ':' to call its methods)");
    }
}

/// `Signal.new()`.
unsafe fn signal_new(L: *mut lua_State) -> i32 {
    lua_newuserdata(L, 0);
    lua_getfield(L, LUA_REGISTRYINDEX, REG_SIGNAL_MT);
    lua_setmetatable(L, -2);
    lua_getfield(L, LUA_REGISTRYINDEX, REG_SIGNALS);
    lua_pushvalue(L, -2);
    lua_createtable(L, 0, 2);
    lua_newtable(L);
    lua_setfield(L, -2, "connections");
    lua_newtable(L);
    lua_setfield(L, -2, "waiting");
    lua_rawset(L, -3);
    lua_pop(L, 1);
    1
}

unsafe fn connect(L: *mut lua_State, once: bool) -> i32 {
    signal_state(L, 1);
    luaL_checktype(L, 2, LUA_TFUNCTION);
    // connection: { Connected = true, [1] = callback, [2] = once, [3] = state }
    lua_createtable(L, 3, 1);
    lua_pushboolean(L, 1);
    lua_setfield(L, -2, "Connected");
    lua_pushvalue(L, 2);
    lua_rawseti(L, -2, 1);
    lua_pushboolean(L, once as i32);
    lua_rawseti(L, -2, 2);
    lua_pushvalue(L, -2);
    lua_rawseti(L, -2, 3);
    lua_getfield(L, LUA_REGISTRYINDEX, REG_CONN_MT);
    lua_setmetatable(L, -2);
    // append to state.connections
    lua_getfield(L, -2, "connections");
    let n = lua_objlen(L, -1) as i32;
    lua_pushvalue(L, -2);
    lua_rawseti(L, -2, n + 1);
    lua_pop(L, 1);
    1
}

unsafe fn signal_connect(L: *mut lua_State) -> i32 {
    connect(L, false)
}

unsafe fn signal_once(L: *mut lua_State) -> i32 {
    connect(L, true)
}

/// `connection:Disconnect()`.
unsafe fn conn_disconnect(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    lua_getfield(L, 1, "Connected");
    let connected = lua_toboolean(L, -1) != 0;
    lua_pop(L, 1);
    if !connected {
        return 0;
    }
    lua_pushboolean(L, 0);
    lua_setfield(L, 1, "Connected");
    lua_rawgeti(L, 1, 3);
    if lua_istable(L, -1) {
        lua_getfield(L, -1, "connections");
        let n = lua_objlen(L, -1) as i32;
        let mut found = 0;
        for i in 1..=n {
            lua_rawgeti(L, -1, i);
            let same = lua_rawequal(L, -1, 1) != 0;
            lua_pop(L, 1);
            if same {
                found = i;
                break;
            }
        }
        if found != 0 {
            for i in found..n {
                lua_rawgeti(L, -1, i + 1);
                lua_rawseti(L, -2, i);
            }
            lua_pushnil(L);
            lua_rawseti(L, -2, n);
        }
    }
    0
}

/// Runs `f` (at `fidx`, a function or suspended thread) with copies of the
/// `nargs` values starting at `first`, like `task.spawn`.
unsafe fn spawn_copy(L: *mut lua_State, fidx: i32, first: i32, nargs: i32) {
    let co = if lua_isfunction(L, fidx) {
        let co = lua_newthread(L);
        lua_pushvalue(L, fidx);
        lua_xmove(L, co, 1);
        co
    } else {
        let co = lua_tothread(L, fidx);
        if co.is_null() || !is_suspended(L, co) {
            return;
        }
        lua_pushvalue(L, fidx); // keep it referenced while it runs
        co
    };
    lua_checkstack(co, nargs + 1);
    for i in 0..nargs {
        lua_pushvalue(L, first + i);
    }
    lua_xmove(L, co, nargs);
    resume(L, co, nargs);
    lua_pop(L, 1);
}

/// `signal:Fire(...)`: runs every connection, then resumes `Wait`ers.
unsafe fn signal_fire(L: *mut lua_State) -> i32 {
    let nargs = lua_gettop(L) - 1;
    signal_state(L, 1);
    let st = lua_gettop(L);
    // snapshot of the connections (callbacks may connect/disconnect)
    lua_getfield(L, st, "connections");
    let n = lua_objlen(L, -1) as i32;
    lua_createtable(L, n, 0);
    for i in 1..=n {
        lua_rawgeti(L, -2, i);
        lua_rawseti(L, -2, i);
    }
    let snap = lua_gettop(L);
    for i in 1..=n {
        lua_rawgeti(L, snap, i);
        let conn = lua_gettop(L);
        lua_getfield(L, conn, "Connected");
        let connected = lua_toboolean(L, -1) != 0;
        lua_pop(L, 1);
        if connected {
            lua_rawgeti(L, conn, 2);
            let once = lua_toboolean(L, -1) != 0;
            lua_pop(L, 1);
            if once {
                lua_pushcclosurek(L, conn_disconnect, None, 0, None);
                lua_pushvalue(L, conn);
                lua_call(L, 1, 0);
            }
            lua_rawgeti(L, conn, 1);
            spawn_copy(L, lua_gettop(L), 2, nargs);
            lua_pop(L, 1);
        }
        lua_pop(L, 1);
    }
    // waiting threads: take the list, then resume each
    lua_getfield(L, st, "waiting");
    let w = lua_gettop(L);
    lua_newtable(L);
    lua_setfield(L, st, "waiting");
    let m = lua_objlen(L, w) as i32;
    for i in 1..=m {
        lua_rawgeti(L, w, i);
        spawn_copy(L, lua_gettop(L), 2, nargs);
        lua_pop(L, 1);
    }
    lua_settop(L, 0);
    0
}

/// `signal:Wait()`: yields until the next `Fire`; returns its arguments.
unsafe fn signal_wait(L: *mut lua_State) -> i32 {
    signal_state(L, 1);
    lua_getfield(L, -1, "waiting");
    let n = lua_objlen(L, -1) as i32;
    lua_pushthread(L);
    lua_rawseti(L, -2, n + 1);
    lua_settop(L, 0);
    park_and_yield(L)
}

/// `signal:DisconnectAll()`.
unsafe fn signal_disconnect_all(L: *mut lua_State) -> i32 {
    signal_state(L, 1);
    lua_getfield(L, -1, "connections");
    let t = lua_gettop(L);
    while lua_objlen(L, t) > 0 {
        lua_pushcclosurek(L, conn_disconnect, None, 0, None);
        lua_rawgeti(L, t, 1);
        lua_call(L, 1, 0);
    }
    0
}

unsafe fn signal_tostring(L: *mut lua_State) -> i32 {
    lua_pushstring(L, "Signal");
    1
}

/// Opens the `Signal` library (signal and connection metatables, and the
/// `Signal` global).
pub unsafe fn luaopen_signal(L: *mut lua_State) -> i32 {
    // signal states, weak-keyed by the signal object
    lua_newtable(L);
    lua_newtable(L);
    lua_pushstring(L, "k");
    lua_setfield(L, -2, "__mode");
    lua_setmetatable(L, -2);
    lua_setfield(L, LUA_REGISTRYINDEX, REG_SIGNALS);

    // signal metatable
    lua_newtable(L);
    lua_newtable(L);
    set_fn(L, "Connect", signal_connect);
    set_fn(L, "Once", signal_once);
    set_fn(L, "Wait", signal_wait);
    set_fn(L, "Fire", signal_fire);
    set_fn(L, "DisconnectAll", signal_disconnect_all);
    set_fn(L, "connect", signal_connect);
    set_fn(L, "once", signal_once);
    set_fn(L, "wait", signal_wait);
    set_fn(L, "fire", signal_fire);
    set_fn(L, "Destroy", signal_disconnect_all);
    lua_setfield(L, -2, "__index");
    lua_pushstring(L, "Signal");
    lua_setfield(L, -2, "__type");
    set_fn(L, "__tostring", signal_tostring);
    lua_pushstring(L, "The metatable is locked");
    lua_setfield(L, -2, "__metatable");
    lua_setreadonly(L, -1, 1);
    lua_setfield(L, LUA_REGISTRYINDEX, REG_SIGNAL_MT);

    // connection metatable
    lua_newtable(L);
    lua_newtable(L);
    set_fn(L, "Disconnect", conn_disconnect);
    set_fn(L, "disconnect", conn_disconnect);
    lua_setfield(L, -2, "__index");
    lua_setreadonly(L, -1, 1);
    lua_setfield(L, LUA_REGISTRYINDEX, REG_CONN_MT);

    // Signal library
    lua_newtable(L);
    set_fn(L, "new", signal_new);
    lua_setglobal(L, "Signal");

    0
}
