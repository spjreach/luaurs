//! Port of `VM/src/ldblib.cpp` (`debug.info`, `debug.traceback`), plus the
//! Lua 5.1-style introspection helpers (`getupvalue`, `setupvalue`,
//! `getlocal`, `setlocal`, `getmetatable`, `setmetatable`, `getregistry`)
//! that embedders commonly expose, and `newcclosure` / `newlclosure` /
//! `setreadonly`.

use crate::lapi::*;
use crate::laux::*;
use crate::lbaselib::luaL_argcheck;
use crate::ldebug::*;
use crate::lobject::*;
use crate::lgc::luaC_objbarrier;
use crate::lstring::luaS_newlstr;
use crate::lvmload::luau_load;

unsafe fn getthread(L: *mut lua_State) -> (*mut lua_State, i32) {
    if lua_isthread(L, 1) {
        (lua_tothread(L, 1), 1)
    } else {
        (L, 0)
    }
}

unsafe fn db_info(L: *mut lua_State) -> i32 {
    let (L1, arg) = getthread(L);
    let mut l1top = 0;
    if L != L1 {
        lua_rawcheckstack(L1, 1);
        l1top = lua_gettop(L1);
    }

    let level;
    if lua_isnumber(L, arg + 1) != 0 {
        level = lua_tointeger(L, arg + 1);
        luaL_argcheck(L, level >= 0, arg + 1, "level can't be negative");
    } else if arg == 0 && lua_isfunction(L, 1) {
        level = -lua_gettop(L);
    } else {
        luaL_argerror(L, arg + 1, "function or level expected");
    }

    let options = luaL_checkstring(L, arg + 2);

    let mut ar = lua_Debug::default();
    if lua_getinfo(L1, level, &options, &mut ar) == 0 {
        return 0;
    }

    let mut results = 0;
    let mut occurs = [false; 26];

    for &c in options.iter() {
        if c.wrapping_sub(b'a') < 26 {
            let k = (c - b'a') as usize;
            if occurs[k] {
                if L != L1 {
                    lua_settop(L1, l1top);
                }
                luaL_argerror(L, arg + 2, "duplicate option");
            }
            occurs[k] = true;
        }
        match c {
            b's' => {
                lua_pushlstring(L, &ar.short_src);
                results += 1;
            }
            b'l' => {
                lua_pushinteger(L, ar.currentline);
                results += 1;
            }
            b'n' => {
                lua_pushlstring(L, ar.name.as_deref().unwrap_or(b""));
                results += 1;
            }
            b'f' => {
                if L1 == L {
                    lua_pushvalue(L, -1 - results);
                } else {
                    lua_xmove(L1, L, 1);
                }
                results += 1;
            }
            b'a' => {
                lua_pushinteger(L, ar.nparams as i32);
                lua_pushboolean(L, ar.isvararg as i32);
                results += 2;
            }
            _ => {
                if L != L1 {
                    lua_settop(L1, l1top);
                }
                luaL_argerror(L, arg + 2, "invalid option");
            }
        }
    }
    results
}

unsafe fn db_traceback(L: *mut lua_State) -> i32 {
    let (L1, arg) = getthread(L);
    let msg = if lua_isnoneornil(L, arg + 1) { None } else { Some(luaL_checklstring(L, arg + 1)) };
    let level = luaL_optinteger(L, arg + 2, if L == L1 { 1 } else { 0 });
    luaL_argcheck(L, level >= 0, arg + 2, "level can't be negative");
    luaL_traceback(L, L1, msg.as_deref(), level);
    1
}

// ---- extensions ----

unsafe fn db_getupvalue(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    let n = luaL_checkinteger(L, 2);
    match lua_getupvalue(L, 1, n) {
        None => 0,
        Some(name) => {
            let name = name.to_vec();
            lua_pushlstring(L, &name);
            lua_insert(L, -2);
            2
        }
    }
}

unsafe fn db_setupvalue(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    let n = luaL_checkinteger(L, 2);
    luaL_checkany(L, 3);
    lua_settop(L, 3);
    match lua_setupvalue(L, 1, n) {
        None => 0,
        Some(name) => {
            let name = name.to_vec();
            lua_pushlstring(L, &name);
            1
        }
    }
}

unsafe fn db_getlocal(L: *mut lua_State) -> i32 {
    let (L1, arg) = getthread(L);
    let level = luaL_checkinteger(L, arg + 1);
    let n = luaL_checkinteger(L, arg + 2);
    luaL_argcheck(L, level >= 0, arg + 1, "level can't be negative");
    // level 0 is getlocal itself, level 1 its caller (Lua 5.1 semantics)
    if L1 != L {
        lua_rawcheckstack(L1, 1);
    }
    match lua_getlocal(L1, level, n) {
        None => {
            lua_pushnil(L);
            1
        }
        Some(name) => {
            if L1 != L {
                lua_xmove(L1, L, 1);
            }
            lua_pushlstring(L, &name);
            lua_insert(L, -2);
            2
        }
    }
}

unsafe fn db_setlocal(L: *mut lua_State) -> i32 {
    let (L1, arg) = getthread(L);
    let level = luaL_checkinteger(L, arg + 1);
    let n = luaL_checkinteger(L, arg + 2);
    luaL_checkany(L, arg + 3);
    lua_settop(L, arg + 3);
    if L1 != L {
        lua_xmove(L, L1, 1);
    }
    match lua_setlocal(L1, level, n) {
        None => lua_pushnil(L),
        Some(name) => lua_pushlstring(L, &name),
    }
    1
}

unsafe fn db_getmetatable(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    if lua_getmetatable(L, 1) == 0 {
        lua_pushnil(L);
    }
    1
}

unsafe fn db_setmetatable(L: *mut lua_State) -> i32 {
    let t = lua_type(L, 2);
    luaL_argexpected_nil_or_table(L, t);
    lua_settop(L, 2);
    lua_setmetatable(L, 1);
    1
}

unsafe fn luaL_argexpected_nil_or_table(L: *mut lua_State, t: i32) {
    if t != LUA_TNIL && t != LUA_TTABLE {
        luaL_typeerror(L, 2, "nil or table");
    }
}

unsafe fn db_getregistry(L: *mut lua_State) -> i32 {
    lua_pushvalue(L, LUA_REGISTRYINDEX);
    1
}

/// The C closure made by `debug.newcclosure`: calls the wrapped function
/// (upvalue 1) with all arguments and returns all its results.
unsafe fn cclosure_call(L: *mut lua_State) -> i32 {
    let n = lua_gettop(L);
    lua_pushvalue(L, lua_upvalueindex(1));
    lua_insert(L, 1);
    lua_call(L, n, LUA_MULTRET);
    lua_gettop(L)
}

/// Resumes `cclosure_call` after the wrapped function yielded.
unsafe fn cclosure_cont(L: *mut lua_State, _status: i32) -> i32 {
    lua_gettop(L)
}

/// `debug.newcclosure(f [, name])`: a C function wrapping `f` (yields pass
/// through); `debug.info(_, "n")` reports `name`.
unsafe fn db_newcclosure(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    let name = if lua_isnoneornil(L, 2) { None } else { Some(String::from_utf8_lossy(luaL_checklstring(L, 2)).into_owned()) };
    lua_settop(L, 1);
    lua_pushcclosurek(L, cclosure_call, name.as_deref(), 1, Some(cclosure_cont));
    1
}

/// `debug.setreadonly(t, readonly)`: sets or clears a table's read-only flag.
unsafe fn db_setreadonly(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    luaL_checktype(L, 2, LUA_TBOOLEAN);
    lua_setreadonly(L, 1, lua_toboolean(L, 2));
    0
}

/// `debug.newlclosure(f [, name])`: a Lua function wrapping `f`, named
/// `name` (or `f`'s name) for `debug.info`.
unsafe fn db_newlclosure(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    let name: Option<Vec<u8>> = if lua_isnoneornil(L, 2) {
        let cl = clvalue(index2addr(L, 1));
        let n = if (*cl).isC != 0 { Closure::c(cl).debugname } else { (*Closure::proto(cl)).debugname };
        (!n.is_null()).then(|| sbytes(n).to_vec())
    } else {
        Some(luaL_checklstring(L, 2).to_vec())
    };
    thread_local! {
        static WRAPPER: Vec<u8> = luau_compiler::compile_source(b"local f = ... return function(...) return f(...) end", luau_compiler::CompileOptions::default());
    }
    let loaded = WRAPPER.with(|bc| luau_load(L, b"=newlclosure", bc, 0));
    if loaded != 0 {
        lua_error(L);
    }
    lua_pushvalue(L, 1);
    lua_call(L, 1, 1);
    if let Some(name) = name {
        let p = Closure::proto(clvalue(index2addr(L, -1)));
        let s = luaS_newlstr(L, &name);
        (*p).debugname = s;
        luaC_objbarrier(L, p as *mut GCObject, s as *mut GCObject);
    }
    1
}


const DBLIB: &[luaL_Reg] = &[
    ("info", db_info),
    ("traceback", db_traceback),
    ("getupvalue", db_getupvalue),
    ("setupvalue", db_setupvalue),
    ("getlocal", db_getlocal),
    ("setlocal", db_setlocal),
    ("getmetatable", db_getmetatable),
    ("setmetatable", db_setmetatable),
    ("getregistry", db_getregistry),
    ("newcclosure", db_newcclosure),
    ("newlclosure", db_newlclosure),
    ("setreadonly", db_setreadonly),
];

pub unsafe fn luaopen_debug(L: *mut lua_State) -> i32 {
    luaL_register(L, Some("debug"), DBLIB);
    1
}
