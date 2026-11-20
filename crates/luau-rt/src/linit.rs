//! Port of `VM/src/linit.cpp`.

use crate::lapi::*;
use crate::lbaselib::luaopen_base;
use crate::lbitlib::luaopen_bit32;
use crate::lbuflib::luaopen_buffer;
use crate::lcorolib::luaopen_coroutine;
use crate::ldblib::luaopen_debug;
use crate::lintlib::{luaopen_class, luaopen_integer};
use crate::lmathlib::luaopen_math;
use crate::lobject::*;
use crate::los::luaopen_os;
use crate::lstate::lua_newstate;
use crate::lstrlib::luaopen_string;
use crate::ltablib::luaopen_table;
use crate::lutf8lib::luaopen_utf8;
use crate::lveclib::luaopen_vector;
use crate::lvmutils::classes_enabled;

const LUALIBS: &[(&str, lua_CFunction)] = &[
    ("", luaopen_base),
    ("coroutine", luaopen_coroutine),
    ("table", luaopen_table),
    ("os", luaopen_os),
    ("string", luaopen_string),
    ("math", luaopen_math),
    ("debug", luaopen_debug),
    ("utf8", luaopen_utf8),
    ("bit32", luaopen_bit32),
    ("buffer", luaopen_buffer),
    ("vector", luaopen_vector),
    ("integer", luaopen_integer),
];

pub unsafe fn luaL_openlibs(L: *mut lua_State) {
    for &(name, f) in LUALIBS {
        lua_pushcfunction(L, f, None);
        lua_pushstring(L, name);
        lua_call(L, 1, 0);
    }
    if classes_enabled() {
        lua_pushcfunction(L, luaopen_class, None);
        lua_pushstring(L, "class");
        lua_call(L, 1, 0);
    }
}

/// Opens the non-standard libraries: `task`, `Signal` and `Regex`
/// (Roblox-style APIs plus the regex engine; `luaL_openlibs` stays standard).
pub unsafe fn luaL_openextlibs(L: *mut lua_State) {
    crate::ltasklib::luaopen_task(L);
    crate::lsignal::luaopen_signal(L);
    crate::lregex::luaopen_regex(L);
    crate::jit::lualib::luaopen_jit(L);
}

pub unsafe fn luaL_sandbox(L: *mut lua_State) {
    lua_pushnil(L);
    while lua_next(L, LUA_GLOBALSINDEX) != 0 {
        if lua_istable(L, -1) {
            lua_setreadonly(L, -1, 1);
        }
        lua_pop(L, 1);
    }

    lua_pushstring(L, "");
    if lua_getmetatable(L, -1) != 0 {
        lua_setreadonly(L, -1, 1);
        lua_pop(L, 2);
    } else {
        lua_pop(L, 1);
    }

    lua_pushvector(L, 0.0, 0.0, 0.0);
    if lua_getmetatable(L, -1) != 0 {
        lua_setreadonly(L, -1, 1);
        lua_pop(L, 2);
    } else {
        lua_pop(L, 1);
    }

    lua_setreadonly(L, LUA_GLOBALSINDEX, 1);
    lua_setsafeenv(L, LUA_GLOBALSINDEX, 1);
}

pub unsafe fn luaL_sandboxthread(L: *mut lua_State) {
    lua_newtable(L);
    lua_newtable(L);
    lua_pushvalue(L, LUA_GLOBALSINDEX);
    lua_setfield(L, -2, "__index");
    lua_setreadonly(L, -1, 1);
    lua_setmetatable(L, -2);
    lua_replace(L, LUA_GLOBALSINDEX);
    lua_setsafeenv(L, LUA_GLOBALSINDEX, 1);
}

pub unsafe fn luaL_newstate() -> *mut lua_State {
    lua_newstate()
}
