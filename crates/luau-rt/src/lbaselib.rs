//! Port of `VM/src/lbaselib.cpp`: base library.

use std::io::Write;

use crate::lapi::*;
use crate::laux::*;
use crate::ldebug::{lua_Debug, lua_getinfo};
use crate::ldo::luaD_checkstack;
use crate::lnumutils::{isspace, strtoull};
use crate::lobject::*;
use crate::ludata::UTAG_PROXY;

#[inline]
pub unsafe fn luaL_argcheck(L: *mut lua_State, cond: bool, arg: i32, extramsg: &str) {
    if !cond {
        luaL_argerror(L, arg, extramsg);
    }
}

#[inline]
pub unsafe fn luaL_argexpected(L: *mut lua_State, cond: bool, arg: i32, tname: &str) {
    if !cond {
        luaL_typeerror(L, arg, tname);
    }
}

thread_local! {
    static STDOUT: std::cell::RefCell<std::io::BufWriter<std::io::Stdout>> =
        std::cell::RefCell::new(std::io::BufWriter::with_capacity(1 << 16, std::io::stdout()));
}

/// Flushes buffered `print` output (called before writing to stderr and at
/// exit).
pub fn flush_stdout() {
    STDOUT.with(|s| {
        let _ = s.borrow_mut().flush();
    });
}

pub fn write_stdout(bytes: &[u8]) {
    STDOUT.with(|s| {
        let _ = s.borrow_mut().write_all(bytes);
    });
}

unsafe fn luaB_print(L: *mut lua_State) -> i32 {
    let n = lua_gettop(L);
    let mut out: Vec<u8> = Vec::new();
    for i in 1..=n {
        let s = luaL_tolstring(L, i);
        if i > 1 {
            out.push(b'\t');
        }
        out.extend_from_slice(s);
        lua_pop(L, 1);
    }
    out.push(b'\n');
    write_stdout(&out);
    0
}

unsafe fn luaB_tonumber(L: *mut lua_State) -> i32 {
    let base = luaL_optinteger(L, 2, 10);
    if base == 10 {
        let mut isnum = 0;
        let n = lua_tonumberx(L, 1, Some(&mut isnum));
        if isnum != 0 {
            lua_pushnumber(L, n);
            return 1;
        }
        luaL_checkany(L, 1);
    } else {
        let s1 = luaL_checkstring(L, 1);
        luaL_argcheck(L, (2..=36).contains(&base), 2, "base out of range");
        let (n, end) = strtoull(s1, base as u32);
        if end != 0 {
            let mut e = end;
            while e < s1.len() && isspace(s1[e]) {
                e += 1;
            }
            if e == s1.len() {
                lua_pushnumber(L, n as f64);
                return 1;
            }
        }
    }
    lua_pushnil(L);
    1
}

unsafe fn luaB_error(L: *mut lua_State) -> i32 {
    let level = luaL_optinteger(L, 2, 1);
    lua_settop(L, 1);
    if lua_isstring(L, 1) != 0 && level > 0 {
        luaL_where(L, level);
        lua_pushvalue(L, 1);
        lua_concat(L, 2);
    }
    lua_error(L)
}

unsafe fn luaB_getmetatable(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    if lua_getmetatable(L, 1) == 0 {
        lua_pushnil(L);
        return 1;
    }
    luaL_getmetafield(L, 1, "__metatable");
    1
}

unsafe fn luaB_setmetatable(L: *mut lua_State) -> i32 {
    let t = lua_type(L, 2);
    luaL_checktype(L, 1, LUA_TTABLE);
    luaL_argexpected(L, t == LUA_TNIL || t == LUA_TTABLE, 2, "nil or table");
    if luaL_getmetafield(L, 1, "__metatable") != 0 {
        luaL_error(L, "cannot change a protected metatable");
    }
    lua_settop(L, 2);
    lua_setmetatable(L, 1);
    1
}

unsafe fn getfunc(L: *mut lua_State, opt: bool) {
    if lua_isfunction(L, 1) {
        lua_pushvalue(L, 1);
    } else {
        let level = if opt { luaL_optinteger(L, 1, 1) } else { luaL_checkinteger(L, 1) };
        luaL_argcheck(L, level >= 0, 1, "level must be non-negative");
        let mut ar = lua_Debug::default();
        if lua_getinfo(L, level, b"f", &mut ar) == 0 {
            luaL_argerror(L, 1, "invalid level");
        }
        if lua_isnil(L, -1) {
            luaL_error_owned(L, format!("no function environment for tail call at level {}", level));
        }
    }
}

unsafe fn luaB_getfenv(L: *mut lua_State) -> i32 {
    getfunc(L, true);
    if lua_iscfunction(L, -1) != 0 {
        lua_pushvalue(L, LUA_GLOBALSINDEX);
    } else {
        lua_getfenv(L, -1);
    }
    lua_setsafeenv(L, -1, 0);
    1
}

unsafe fn luaB_setfenv(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 2, LUA_TTABLE);
    getfunc(L, false);
    lua_pushvalue(L, 2);
    lua_setsafeenv(L, -1, 0);
    if lua_isnumber(L, 1) != 0 && lua_tonumber(L, 1) == 0.0 {
        lua_pushthread(L);
        lua_insert(L, -2);
        lua_setfenv(L, -2);
        return 0;
    } else if lua_iscfunction(L, -2) != 0 || lua_setfenv(L, -2) == 0 {
        luaL_error(L, "'setfenv' cannot change environment of given object");
    }
    1
}

unsafe fn luaB_rawequal(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    luaL_checkany(L, 2);
    lua_pushboolean(L, lua_rawequal(L, 1, 2));
    1
}

unsafe fn luaB_rawget(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    luaL_checkany(L, 2);
    lua_settop(L, 2);
    lua_rawget(L, 1);
    1
}

unsafe fn luaB_rawset(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    luaL_checkany(L, 2);
    luaL_checkany(L, 3);
    lua_settop(L, 3);
    lua_rawset(L, 1);
    1
}

unsafe fn luaB_rawlen(L: *mut lua_State) -> i32 {
    let tt = lua_type(L, 1);
    luaL_argcheck(L, tt == LUA_TTABLE || tt == LUA_TSTRING, 1, "table or string expected");
    lua_pushinteger(L, lua_objlen(L, 1));
    1
}

unsafe fn luaB_gcinfo(L: *mut lua_State) -> i32 {
    lua_pushinteger(L, lua_gc(L, LUA_GCCOUNT, 0));
    1
}

unsafe fn luaB_type(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    lua_pushstring(L, lua_typename(lua_type(L, 1)));
    1
}

unsafe fn luaB_typeof(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    let n = luaL_typename(L, 1);
    lua_pushstring(L, &n);
    1
}

pub unsafe fn luaB_next(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    lua_settop(L, 2);
    if lua_next(L, 1) != 0 {
        2
    } else {
        lua_pushnil(L);
        1
    }
}

unsafe fn luaB_pairs(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    lua_pushvalue(L, lua_upvalueindex(1));
    lua_pushvalue(L, 1);
    lua_pushnil(L);
    3
}

pub unsafe fn luaB_inext(L: *mut lua_State) -> i32 {
    let mut i = luaL_checkinteger(L, 2);
    luaL_checktype(L, 1, LUA_TTABLE);
    i += 1;
    lua_pushinteger(L, i);
    lua_rawgeti(L, 1, i);
    if lua_isnil(L, -1) {
        0
    } else {
        2
    }
}

unsafe fn luaB_ipairs(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    lua_pushvalue(L, lua_upvalueindex(1));
    lua_pushvalue(L, 1);
    lua_pushinteger(L, 0);
    3
}

unsafe fn luaB_assert(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    if lua_toboolean(L, 1) == 0 {
        let msg = luaL_optlstring(L, 2, b"assertion failed!");
        luaL_error_bytes(L, &msg);
    }
    lua_gettop(L)
}

unsafe fn luaB_select(L: *mut lua_State) -> i32 {
    let n = lua_gettop(L);
    if lua_type(L, 1) == LUA_TSTRING && lua_tostring(L, 1).unwrap().first() == Some(&b'#') {
        lua_pushinteger(L, n - 1);
        1
    } else {
        let mut i = luaL_checkinteger(L, 1);
        if i < 0 {
            i += n;
        } else if i > n {
            i = n;
        }
        luaL_argcheck(L, 1 <= i, 1, "index out of range");
        n - i
    }
}

unsafe fn luaB_pcally(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    lua_pcallyieldable(L, lua_gettop(L) - 1, LUA_MULTRET, 0)
}

unsafe fn luaB_pcallcont(L: *mut lua_State, status: i32) -> i32 {
    luaD_checkstack(L, 1);
    if status == 0 {
        let base = (*L).base;
        let top = (*L).top;
        let mut q = top;
        while q > base {
            setobj(q, q.sub(1));
            q = q.sub(1);
        }
        setbvalue(base, true);
        (*L).top = top.add(1);
        (*L).top.offset_from(base) as i32
    } else {
        let top = (*L).top;
        setobj(top, top.sub(1));
        setbvalue(top.sub(1), false);
        (*L).top = top.add(1);
        2
    }
}

unsafe fn luaB_xpcally(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 2, LUA_TFUNCTION);
    lua_pushvalue(L, 1);
    lua_pushvalue(L, 2);
    lua_replace(L, 1);
    lua_replace(L, 2);
    lua_pcallyieldable(L, lua_gettop(L) - 2, LUA_MULTRET, 1)
}

unsafe fn luaB_xpcallcont(L: *mut lua_State, status: i32) -> i32 {
    if status == 0 {
        let base = (*L).base;
        setbvalue(base, true);
        (*L).top.offset_from(base) as i32
    } else {
        luaD_checkstack(L, 1);
        let top = (*L).top;
        setobj(top, top.sub(1));
        setbvalue(top.sub(1), false);
        (*L).top = top.add(1);
        2
    }
}

pub(crate) unsafe fn luaB_tostring(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    luaL_tolstring(L, 1);
    1
}

unsafe fn luaB_newproxy(L: *mut lua_State) -> i32 {
    let t = lua_type(L, 1);
    luaL_argexpected(L, t == LUA_TNONE || t == LUA_TNIL || t == LUA_TBOOLEAN, 1, "nil or boolean");
    let needsmt = lua_toboolean(L, 1) != 0;
    lua_newuserdatatagged(L, 0, UTAG_PROXY as i32);
    if needsmt {
        lua_newtable(L);
        lua_setmetatable(L, -2);
    }
    1
}

const BASE_FUNCS: &[luaL_Reg] = &[
    ("assert", luaB_assert),
    ("error", luaB_error),
    ("gcinfo", luaB_gcinfo),
    ("getfenv", luaB_getfenv),
    ("getmetatable", luaB_getmetatable),
    ("next", luaB_next),
    ("newproxy", luaB_newproxy),
    ("print", luaB_print),
    ("rawequal", luaB_rawequal),
    ("rawget", luaB_rawget),
    ("rawset", luaB_rawset),
    ("rawlen", luaB_rawlen),
    ("select", luaB_select),
    ("setfenv", luaB_setfenv),
    ("setmetatable", luaB_setmetatable),
    ("tonumber", luaB_tonumber),
    ("tostring", luaB_tostring),
    ("type", luaB_type),
    ("typeof", luaB_typeof),
];

unsafe fn auxopen(L: *mut lua_State, name: &str, f: lua_CFunction, u: lua_CFunction) {
    lua_pushcfunction(L, u, None);
    lua_pushcclosure(L, f, Some(name), 1);
    lua_setfield(L, -2, name);
}

pub unsafe fn luaopen_base(L: *mut lua_State) -> i32 {
    lua_pushvalue(L, LUA_GLOBALSINDEX);
    lua_setglobal(L, "_G");
    luaL_register(L, Some("_G"), BASE_FUNCS);
    lua_pushstring(L, "Luau");
    lua_setglobal(L, "_VERSION");
    auxopen(L, "ipairs", luaB_ipairs, luaB_inext);
    auxopen(L, "pairs", luaB_pairs, luaB_next);
    lua_pushcclosurek(L, luaB_pcally, Some("pcall"), 0, Some(luaB_pcallcont));
    (*(*L).global).builtinPcall = clvalue(luaA_toobject(L, -1));
    lua_setfield(L, -2, "pcall");
    lua_pushcclosurek(L, luaB_xpcally, Some("xpcall"), 0, Some(luaB_xpcallcont));
    (*(*L).global).builtinXpcall = clvalue(luaA_toobject(L, -1));
    lua_setfield(L, -2, "xpcall");
    1
}
