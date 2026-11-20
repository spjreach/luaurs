//! Port of `VM/src/lcorolib.cpp` (`LuauCoroutineFinally` on): stackless
//! coroutines on top of `lua_resume`/`lua_yield`.

use crate::lapi::*;
use crate::laux::*;
use crate::lbaselib::luaL_argexpected;
use crate::ldo::*;
use crate::lgc::luaC_threadbarrier;
use crate::lobject::*;
use crate::lstate::lua_resetthread;
use crate::lvmexecute::luau_callhook;

const STATNAMES: [&str; 5] = ["running", "suspended", "normal", "dead", "dead"];

unsafe fn costatus(L: *mut lua_State) -> i32 {
    let co = lua_tothread(L, 1);
    luaL_argexpected(L, !co.is_null(), 1, "thread");
    lua_pushstring(L, STATNAMES[lua_costatus(L, co) as usize]);
    1
}

unsafe fn interrupt_thread(L: *mut lua_State, co: *mut lua_State) -> i32 {
    if let Some(h) = (*(*L).global).cb.debuginterrupt {
        luau_callhook(L, h, co as usize);
    }
    lua_break(L)
}

unsafe fn finisherror(L: *mut lua_State, wrap: bool) -> i32 {
    if wrap {
        if lua_isstring(L, -1) != 0 {
            luaL_where(L, 1);
            lua_insert(L, -2);
            lua_concat(L, 2);
        }
        lua_error(L)
    } else {
        lua_pushboolean(L, 0);
        lua_insert(L, -2);
        2
    }
}

#[inline]
unsafe fn finishsuccess(L: *mut lua_State, co: *mut lua_State, wrap: bool) -> i32 {
    let nres = (*co).top.offset_from((*co).base) as i32;
    if nres + 1 > LUA_MINSTACK && lua_checkstack(L, nres + 1) == 0 {
        luaL_error(L, "too many results to resume");
    }
    if !wrap {
        lua_pushboolean(L, 1);
    }
    if nres != 0 {
        luaC_threadbarrier(L);
        let totop = (*L).top;
        let fromtop = (*co).top.sub(nres as usize);
        for i in 0..nres as usize {
            setobj_fresh(totop.add(i), fromtop.add(i));
        }
        (*co).top = fromtop;
        (*L).top = totop.add(nres as usize);
    }
    nres + if wrap { 0 } else { 1 }
}

#[inline]
unsafe fn auxresumefinish(L: *mut lua_State, co: *mut lua_State, status: i32, wrap: bool) -> i32 {
    if status == LUA_YIELD {
        return finishsuccess(L, co, wrap);
    }
    if status == LUA_BREAK {
        return interrupt_thread(L, co);
    }
    if status == (*co).status as i32 && !(*co).finalizers.is_null() {
        if wrap {
            luaL_error(L, "cannot run finalizers from a wrapped coroutine");
        }
        luaD_preparefinalize(L, co, if (*co).status == LUA_OK as u8 { FINALLY_FINISHED } else { FINALLY_ERROR });
        return luaD_runfinalizers(L, true);
    }
    if status == LUA_OK {
        return finishsuccess(L, co, wrap);
    }
    lua_xmove(co, L, 1);
    finisherror(L, wrap)
}

#[inline]
unsafe fn auxresume(L: *mut lua_State, co: *mut lua_State, narg: i32, wrap: bool) -> i32 {
    if (*co).status != LUA_YIELD as u8 {
        let status = lua_costatus(L, co);
        if status != LUA_COSUS {
            lua_pushstring(L, &format!("cannot resume {} coroutine", STATNAMES[status as usize]));
            return finisherror(L, wrap);
        }
    }
    if narg != 0 {
        if lua_checkstack(co, narg) == 0 {
            luaL_error(L, "too many arguments to resume");
        }
        lua_xmove(L, co, narg);
    } else if (*co).top.offset_from((*co).base) > LUAI_MAXCSTACK as isize {
        luaL_error(L, "too many arguments to resume");
    }
    (*co).singlestep = (*L).singlestep;
    let status = lua_resume(co, L, narg);
    auxresumefinish(L, co, status, wrap)
}

unsafe fn coresumey(L: *mut lua_State) -> i32 {
    let co = lua_tothread(L, 1);
    luaL_argexpected(L, !co.is_null(), 1, "thread");
    let narg = (*L).top.offset_from((*L).base) as i32 - 1;
    auxresume(L, co, narg, false)
}

unsafe fn coresumecont(L: *mut lua_State, status: i32) -> i32 {
    if status != LUA_OK {
        lua_pushboolean(L, 0);
        lua_insert(L, -2);
        return 2;
    }
    let co = lua_tothread(L, 1);
    luaL_argexpected(L, !co.is_null(), 1, "thread");
    if lua_gettop(L) == 1 {
        return auxresumefinish(L, co, (*co).status as i32, false);
    }
    luaD_runfinalizers(L, true)
}

unsafe fn auxwrapy(L: *mut lua_State) -> i32 {
    let co = lua_tothread(L, lua_upvalueindex(1));
    let narg = (*L).top.offset_from((*L).base) as i32;
    auxresume(L, co, narg, true)
}

unsafe fn auxwrapcont(L: *mut lua_State, _status: i32) -> i32 {
    let co = lua_tothread(L, lua_upvalueindex(1));
    auxresumefinish(L, co, (*co).status as i32, true)
}

unsafe fn cocreate(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    let nl = lua_newthread(L);
    lua_xpush(L, nl, 1);
    1
}

unsafe fn cowrap(L: *mut lua_State) -> i32 {
    cocreate(L);
    lua_pushcclosurek(L, auxwrapy, None, 1, Some(auxwrapcont));
    1
}

unsafe fn coyield(L: *mut lua_State) -> i32 {
    let nres = (*L).top.offset_from((*L).base) as i32;
    lua_yield(L, nres)
}

unsafe fn corunning(L: *mut lua_State) -> i32 {
    if lua_pushthread(L) != 0 {
        lua_pushnil(L);
    }
    1
}

unsafe fn coyieldable(L: *mut lua_State) -> i32 {
    lua_pushboolean(L, lua_isyieldable(L));
    1
}

unsafe fn coclosey(L: *mut lua_State) -> i32 {
    let co = lua_tothread(L, 1);
    luaL_argexpected(L, !co.is_null(), 1, "thread");
    let status = lua_costatus(L, co);
    if status != LUA_COFIN && status != LUA_COERR && status != LUA_COSUS {
        luaL_error_owned(L, format!("cannot close {} coroutine", STATNAMES[status as usize]));
    }
    let st = (*co).status as i32;
    if st == LUA_OK || st == LUA_YIELD {
        if !(*co).finalizers.is_null() {
            lua_settop(L, 1);
            luaD_preparefinalizestate(L, co, FINALLY_CANCELLED);
            lua_resetthread(co);
            return luaD_runfinalizers(L, true);
        }
        lua_pushboolean(L, 1);
        lua_resetthread(co);
        1
    } else {
        lua_pushboolean(L, 0);
        if st == LUA_ERRMEM {
            lua_pushstring(L, LUA_MEMERRMSG);
        } else if st == LUA_ERRERR {
            lua_pushstring(L, LUA_ERRERRMSG);
        } else if lua_gettop(co) != 0 {
            lua_xmove(co, L, 1);
        }
        lua_resetthread(co);
        2
    }
}

unsafe fn coclosecont(L: *mut lua_State, status: i32) -> i32 {
    if status != LUA_OK {
        lua_pushboolean(L, 0);
        lua_insert(L, -2);
        return 2;
    }
    luaD_runfinalizers(L, true)
}

unsafe fn cofinally(L: *mut lua_State) -> i32 {
    let co = lua_tothread(L, 1);
    luaL_argexpected(L, !co.is_null(), 1, "thread");
    if lua_isnoneornil(L, 2) {
        luaL_error(L, "missing argument #2");
    }
    lua_addfinalizer(L, co, 2);
    0
}

const CO_FUNCS: &[luaL_Reg] = &[
    ("create", cocreate),
    ("running", corunning),
    ("status", costatus),
    ("wrap", cowrap),
    ("yield", coyield),
    ("isyieldable", coyieldable),
    ("finally", cofinally),
];

pub unsafe fn luaopen_coroutine(L: *mut lua_State) -> i32 {
    luaL_register(L, Some("coroutine"), CO_FUNCS);
    lua_pushcclosurek(L, coresumey, Some("resume"), 0, Some(coresumecont));
    lua_setfield(L, -2, "resume");
    lua_pushcclosurek(L, coclosey, Some("close"), 0, Some(coclosecont));
    lua_setfield(L, -2, "close");
    1
}
