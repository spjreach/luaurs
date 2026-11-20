//! Port of `VM/src/ltm.cpp` (tag methods / metamethods).

use crate::lobject::*;
use crate::lstring::{luaS_fix, luaS_new};
use crate::ltable::luaH_getstr;
use crate::ludata::UTAG_PROXY;

pub type TMS = i32;
pub const TM_INDEX: TMS = 0;
pub const TM_NEWINDEX: TMS = 1;
pub const TM_MODE: TMS = 2;
pub const TM_NAMECALL: TMS = 3;
pub const TM_CALL: TMS = 4;
pub const TM_ITER: TMS = 5;
pub const TM_LEN: TMS = 6;
pub const TM_EQ: TMS = 7;
pub const TM_ADD: TMS = 8;
pub const TM_SUB: TMS = 9;
pub const TM_MUL: TMS = 10;
pub const TM_DIV: TMS = 11;
pub const TM_IDIV: TMS = 12;
pub const TM_MOD: TMS = 13;
pub const TM_POW: TMS = 14;
pub const TM_UNM: TMS = 15;
pub const TM_LT: TMS = 16;
pub const TM_LE: TMS = 17;
pub const TM_CONCAT: TMS = 18;
pub const TM_TYPE: TMS = 19;
pub const TM_METATABLE: TMS = 20;

pub const luaT_typenames: [&str; LUA_T_COUNT] =
    ["nil", "boolean", "userdata", "number", "integer", "vector", "string", "table", "function", "userdata", "thread", "buffer", "class", "object"];

pub const luaT_eventname: [&str; TM_N] = [
    "__index",
    "__newindex",
    "__mode",
    "__namecall",
    "__call",
    "__iter",
    "__len",
    "__eq",
    "__add",
    "__sub",
    "__mul",
    "__div",
    "__idiv",
    "__mod",
    "__pow",
    "__unm",
    "__lt",
    "__le",
    "__concat",
    "__type",
    "__metatable",
];

pub unsafe fn luaT_init(L: *mut lua_State) {
    let g = (*L).global;
    for i in 0..LUA_T_COUNT {
        (*g).ttname[i] = luaS_new(L, luaT_typenames[i]);
        luaS_fix((*g).ttname[i]);
    }
    for i in 0..TM_N {
        (*g).tmname[i] = luaS_new(L, luaT_eventname[i]);
        luaS_fix((*g).tmname[i]);
    }
}

/// `luaT_gettm`: optimized for the absence of tag methods.
pub unsafe fn luaT_gettm(events: *mut LuaTable, event: TMS, ename: *mut TString) -> *const TValue {
    let tm = luaH_getstr(events, ename);
    if ttisnil(tm) {
        (*events).tmcache |= 1u8 << event;
        std::ptr::null()
    } else {
        tm
    }
}

/// `fasttm` / `gfasttm`: null when absent.
#[inline(always)]
pub unsafe fn fasttm(L: *mut lua_State, et: *mut LuaTable, e: TMS) -> *const TValue {
    if et.is_null() || ((*et).tmcache & (1u8 << e)) != 0 {
        std::ptr::null()
    } else if hasmetacache(et) {
        getmetacache(et, e)
    } else {
        luaT_gettm(et, e, (*(*L).global).tmname[e as usize])
    }
}

#[inline(always)]
pub unsafe fn fastnotm(et: *mut LuaTable, e: TMS) -> bool {
    et.is_null() || ((*et).tmcache & (1u8 << e)) != 0
}

pub unsafe fn luaT_gettmbyobj(L: *mut lua_State, o: *const TValue, event: TMS) -> *const TValue {
    let mt: *mut LuaTable = match (*o).tt {
        LUA_TTABLE => (*hvalue(o)).metatable,
        LUA_TUSERDATA => (*uvalue(o)).metatable,
        LUA_TCLASS => (*classvalue(o)).metatable,
        LUA_TOBJECT => (*(*objectvalue(o)).lclass).instancemetatable,
        tt => (*(*L).global).mt[tt as usize],
    };
    if !mt.is_null() && hasmetacache(mt) {
        return getmetacache(mt, event);
    }
    if mt.is_null() {
        luaO_nilobject()
    } else {
        luaH_getstr(mt, (*(*L).global).tmname[event as usize])
    }
}

pub unsafe fn luaT_objtypenamestr(L: *mut lua_State, o: *const TValue) -> *mut TString {
    let g = (*L).global;
    if ttisuserdata(o) && (*uvalue(o)).tag as usize != UTAG_PROXY && !(*uvalue(o)).metatable.is_null() {
        let ty = luaH_getstr((*uvalue(o)).metatable, (*g).tmname[TM_TYPE as usize]);
        if ttisstring(ty) {
            return tsvalue(ty);
        }
        return (*g).ttname[(*o).tt as usize];
    }
    if ttislightuserdata(o) {
        let tag = lightuserdatatag(o);
        if (tag as u32) < LUA_LUTAG_LIMIT as u32 {
            let name = (*g).lightuserdataname[tag as usize];
            if !name.is_null() {
                return name;
            }
        }
    }
    let mt = (*g).mt[(*o).tt as usize];
    if !mt.is_null() {
        let ty = luaH_getstr(mt, (*g).tmname[TM_TYPE as usize]);
        if ttisstring(ty) {
            return tsvalue(ty);
        }
    }
    (*g).ttname[(*o).tt as usize]
}

/// `luaT_objtypename` as Rust text.
/// Borrows the (GC-owned, interned) type name: no heap allocation, so error
/// paths that longjmp past it leak nothing. Valid until the next GC step.
pub unsafe fn luaT_objtypename<'a>(L: *mut lua_State, o: *const TValue) -> std::borrow::Cow<'a, str> {
    let ts = luaT_objtypenamestr(L, o);
    let bytes: &'a [u8] = std::slice::from_raw_parts(getstr(ts), (*ts).len as usize);
    String::from_utf8_lossy(bytes)
}
