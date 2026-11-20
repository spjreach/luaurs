//! Port of `VM/src/lapi.cpp`: the stack-based C API.
//!
//! String results are returned as byte slices that borrow GC memory; like
//! the C API, they stay valid while the value is reachable.

use std::ptr;

use crate::ldebug::*;
use crate::ldo::*;
use crate::lfunc::*;
use crate::lgc::*;
use crate::lobject::*;
use crate::lstring::*;
use crate::ltable::*;
use crate::ltm::*;
use crate::ludata::UTAG_IDTOR;
use crate::lvmutils::*;

#[inline(always)]
#[inline(always)]
unsafe fn ensure_stack_impl(L: *mut lua_State, errorL: *mut lua_State, size: i32) {
    if (*L).top.add(size.max(0) as usize) > (*(*L).ci).top {
        ensure_stack_grow(L, errorL, size);
    }
}

#[cold]
#[inline(never)]
unsafe fn ensure_stack_grow(L: *mut lua_State, errorL: *mut lua_State, size: i32) {
    if lua_checkstack(L, size) == 0 {
        luaO_pushfstring(errorL, "stack overflow");
        lua_error(errorL);
    }
}

#[inline(always)]
unsafe fn ensure_stack(L: *mut lua_State, size: i32) {
    ensure_stack_impl(L, L, size)
}

#[inline(always)]
unsafe fn api_incr_top(L: *mut lua_State) {
    (*L).top = (*L).top.add(1);
}

pub unsafe fn getcurrenv(L: *mut lua_State) -> *mut LuaTable {
    if (*L).ci == (*L).base_ci {
        (*L).gt
    } else {
        (*curr_func(L)).env
    }
}

#[inline(never)]
unsafe fn pseudo2addr(L: *mut lua_State, idx: i32) -> *mut TValue {
    let g = (*L).global;
    match idx {
        LUA_REGISTRYINDEX => registry(L),
        LUA_ENVIRONINDEX => {
            sethvalue(ptr::addr_of_mut!((*g).pseudotemp), getcurrenv(L));
            ptr::addr_of_mut!((*g).pseudotemp)
        }
        LUA_GLOBALSINDEX => {
            sethvalue(ptr::addr_of_mut!((*g).pseudotemp), (*L).gt);
            ptr::addr_of_mut!((*g).pseudotemp)
        }
        _ => {
            let func = curr_func(L);
            let idx = LUA_GLOBALSINDEX - idx;
            if idx <= (*func).nupvalues as i32 {
                Closure::cupvals(func).add(idx as usize - 1)
            } else {
                luaO_nilobject() as *mut TValue
            }
        }
    }
}

#[inline(always)]
pub unsafe fn index2addr(L: *mut lua_State, idx: i32) -> *mut TValue {
    if idx > 0 {
        let o = (*L).base.add(idx as usize - 1);
        if o >= (*L).top {
            luaO_nilobject() as *mut TValue
        } else {
            o
        }
    } else if idx > LUA_REGISTRYINDEX {
        (*L).top.offset(idx as isize)
    } else {
        pseudo2addr(L, idx)
    }
}

pub unsafe fn luaA_toobject(L: *mut lua_State, idx: i32) -> *const TValue {
    let p = index2addr(L, idx);
    if p as *const TValue == luaO_nilobject() {
        ptr::null()
    } else {
        p
    }
}

pub unsafe fn luaA_pushvalue(L: *mut lua_State, o: *const TValue) {
    ensure_stack(L, 1);
    setobj((*L).top, o);
    api_incr_top(L);
}

pub unsafe fn luaA_pushclass(L: *mut lua_State, lco: *mut LuauClass) {
    ensure_stack(L, 1);
    setclassvalue((*L).top, lco);
    api_incr_top(L);
}

pub unsafe fn lua_checkstack(L: *mut lua_State, size: i32) -> i32 {
    let mut res = 1;
    if size > LUAI_MAXCSTACK || ((*L).top.offset_from((*L).base) as i32 + size) > LUAI_MAXCSTACK {
        res = 0;
    } else if size > 0 {
        if stacklimitreached(L, size) && luaD_rawrunprotected(L, &mut || luaD_growstack(L, size)) != LUA_OK {
            return 0;
        }
        expandstacklimit(L, (*L).top.add(size as usize));
    }
    res
}

pub unsafe fn lua_rawcheckstack(L: *mut lua_State, size: i32) {
    luaD_checkstack(L, size);
    expandstacklimit(L, (*L).top.add(size as usize));
}

pub unsafe fn lua_xmove(from: *mut lua_State, to: *mut lua_State, n: i32) {
    if from == to {
        return;
    }
    luaC_threadbarrier(to);
    ensure_stack_impl(to, from, n);
    let ttop = (*to).top;
    let ftop = (*from).top.sub(n as usize);
    for i in 0..n as usize {
        setobj(ttop.add(i), ftop.add(i));
    }
    (*from).top = ftop;
    (*to).top = ttop.add(n as usize);
}

pub unsafe fn lua_xpush(from: *mut lua_State, to: *mut lua_State, idx: i32) {
    luaC_threadbarrier(to);
    ensure_stack_impl(to, from, 1);
    setobj((*to).top, index2addr(from, idx));
    api_incr_top(to);
}

pub unsafe fn lua_newthread(L: *mut lua_State) -> *mut lua_State {
    luaC_checkGC(L);
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let L1 = crate::lstate::luaE_newthread(L);
    setthvalue((*L).top, L1);
    api_incr_top(L);
    let g = (*L).global;
    if let Some(cb) = (*g).cb.userthread {
        cb(L, L1);
    }
    L1
}

pub unsafe fn lua_mainthread(L: *mut lua_State) -> *mut lua_State {
    (*(*L).global).mainthread
}

pub unsafe fn lua_absindex(L: *mut lua_State, idx: i32) -> i32 {
    if idx > 0 || lua_ispseudo(idx) {
        idx
    } else {
        (*L).top.offset_from((*L).base) as i32 + idx + 1
    }
}

#[inline(always)]
pub unsafe fn lua_gettop(L: *mut lua_State) -> i32 {
    (*L).top.offset_from((*L).base) as i32
}

pub unsafe fn lua_settop(L: *mut lua_State, idx: i32) {
    if idx >= 0 {
        ensure_stack(L, idx - (*L).top.offset_from((*L).base) as i32);
        let target = (*L).base.add(idx as usize);
        while (*L).top < target {
            setnilvalue((*L).top);
            (*L).top = (*L).top.add(1);
        }
        (*L).top = target;
    } else {
        (*L).top = (*L).top.offset(idx as isize + 1);
    }
}

#[inline(always)]
pub unsafe fn lua_pop(L: *mut lua_State, n: i32) {
    lua_settop(L, -n - 1)
}

pub unsafe fn lua_remove(L: *mut lua_State, idx: i32) {
    let mut p = index2addr(L, idx);
    loop {
        p = p.add(1);
        if p >= (*L).top {
            break;
        }
        setobj(p.sub(1), p);
    }
    (*L).top = (*L).top.sub(1);
}

pub unsafe fn lua_insert(L: *mut lua_State, idx: i32) {
    luaC_threadbarrier(L);
    let p = index2addr(L, idx);
    let mut q = (*L).top;
    while q > p {
        setobj(q, q.sub(1));
        q = q.sub(1);
    }
    setobj(p, (*L).top);
}

pub unsafe fn lua_replace(L: *mut lua_State, idx: i32) {
    luaC_threadbarrier(L);
    let o = index2addr(L, idx);
    if idx == LUA_ENVIRONINDEX {
        let func = curr_func(L);
        (*func).env = hvalue((*L).top.sub(1));
        luaC_barrier(L, func as *mut GCObject, (*L).top.sub(1));
    } else if idx == LUA_GLOBALSINDEX {
        (*L).gt = hvalue((*L).top.sub(1));
    } else {
        setobj(o, (*L).top.sub(1));
        if idx < LUA_GLOBALSINDEX {
            luaC_barrier(L, curr_func(L) as *mut GCObject, (*L).top.sub(1));
        }
    }
    (*L).top = (*L).top.sub(1);
}

pub unsafe fn lua_pushvalue(L: *mut lua_State, idx: i32) {
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let o = index2addr(L, idx);
    setobj((*L).top, o);
    api_incr_top(L);
}

// ---------------------------------------------------------------------------
// access functions
// ---------------------------------------------------------------------------

#[inline(always)]
pub unsafe fn lua_type(L: *mut lua_State, idx: i32) -> i32 {
    let o = index2addr(L, idx);
    if o as *const TValue == luaO_nilobject() {
        LUA_TNONE
    } else {
        (*o).tt
    }
}

pub fn lua_typename(t: i32) -> &'static str {
    if t == LUA_TNONE {
        "no value"
    } else {
        luaT_typenames[t as usize]
    }
}

#[inline]
pub unsafe fn lua_isnil(L: *mut lua_State, n: i32) -> bool {
    lua_type(L, n) == LUA_TNIL
}
#[inline]
pub unsafe fn lua_isnone(L: *mut lua_State, n: i32) -> bool {
    lua_type(L, n) == LUA_TNONE
}
#[inline]
pub unsafe fn lua_isnoneornil(L: *mut lua_State, n: i32) -> bool {
    lua_type(L, n) <= LUA_TNIL
}
#[inline]
pub unsafe fn lua_istable(L: *mut lua_State, n: i32) -> bool {
    lua_type(L, n) == LUA_TTABLE
}
#[inline]
pub unsafe fn lua_isfunction(L: *mut lua_State, n: i32) -> bool {
    lua_type(L, n) == LUA_TFUNCTION
}
#[inline]
pub unsafe fn lua_isboolean(L: *mut lua_State, n: i32) -> bool {
    lua_type(L, n) == LUA_TBOOLEAN
}
#[inline]
pub unsafe fn lua_isthread(L: *mut lua_State, n: i32) -> bool {
    lua_type(L, n) == LUA_TTHREAD
}
#[inline]
pub unsafe fn lua_isvector(L: *mut lua_State, n: i32) -> bool {
    lua_type(L, n) == LUA_TVECTOR
}
#[inline]
pub unsafe fn lua_isbuffer(L: *mut lua_State, n: i32) -> bool {
    lua_type(L, n) == LUA_TBUFFER
}
#[inline]
pub unsafe fn lua_islightuserdata(L: *mut lua_State, n: i32) -> bool {
    lua_type(L, n) == LUA_TLIGHTUSERDATA
}

pub unsafe fn lua_iscfunction(L: *mut lua_State, idx: i32) -> i32 {
    iscfunction(index2addr(L, idx)) as i32
}

pub unsafe fn lua_isLfunction(L: *mut lua_State, idx: i32) -> i32 {
    isLfunction(index2addr(L, idx)) as i32
}

pub unsafe fn lua_isnumber(L: *mut lua_State, idx: i32) -> i32 {
    let mut n = TValue::NIL;
    let o = index2addr(L, idx);
    (!tonumber(o, &mut n).is_null()) as i32
}

pub unsafe fn lua_isstring(L: *mut lua_State, idx: i32) -> i32 {
    let t = lua_type(L, idx);
    (t == LUA_TSTRING || t == LUA_TNUMBER) as i32
}

pub unsafe fn lua_isuserdata(L: *mut lua_State, idx: i32) -> i32 {
    let o = index2addr(L, idx);
    (ttisuserdata(o) || ttislightuserdata(o)) as i32
}

pub unsafe fn lua_rawequal(L: *mut lua_State, index1: i32, index2: i32) -> i32 {
    let o1 = index2addr(L, index1);
    let o2 = index2addr(L, index2);
    if o1 as *const TValue == luaO_nilobject() || o2 as *const TValue == luaO_nilobject() {
        0
    } else {
        luaO_rawequalObj(o1, o2) as i32
    }
}

pub unsafe fn lua_equal(L: *mut lua_State, index1: i32, index2: i32) -> i32 {
    let o1 = index2addr(L, index1);
    let o2 = index2addr(L, index2);
    if o1 as *const TValue == luaO_nilobject() || o2 as *const TValue == luaO_nilobject() {
        0
    } else {
        equalobj(L, o1, o2) as i32
    }
}

pub unsafe fn lua_lessthan(L: *mut lua_State, index1: i32, index2: i32) -> i32 {
    let o1 = index2addr(L, index1);
    let o2 = index2addr(L, index2);
    if o1 as *const TValue == luaO_nilobject() || o2 as *const TValue == luaO_nilobject() {
        0
    } else {
        luaV_lessthan(L, o1, o2)
    }
}

pub unsafe fn lua_tonumberx(L: *mut lua_State, idx: i32, isnum: Option<&mut i32>) -> f64 {
    let mut n = TValue::NIL;
    let o = tonumber(index2addr(L, idx), &mut n);
    if !o.is_null() {
        if let Some(i) = isnum {
            *i = 1;
        }
        nvalue(o)
    } else {
        if let Some(i) = isnum {
            *i = 0;
        }
        0.0
    }
}

#[inline]
pub unsafe fn lua_tonumber(L: *mut lua_State, idx: i32) -> f64 {
    lua_tonumberx(L, idx, None)
}

pub unsafe fn lua_tointegerx(L: *mut lua_State, idx: i32, isnum: Option<&mut i32>) -> i32 {
    let mut n = TValue::NIL;
    let o = tonumber(index2addr(L, idx), &mut n);
    if !o.is_null() {
        if let Some(i) = isnum {
            *i = 1;
        }
        // (int)d like C++: out of range gives INT_MIN, not a saturated value
        crate::lvmexecute::d2i_trunc(nvalue(o))
    } else {
        if let Some(i) = isnum {
            *i = 0;
        }
        0
    }
}

#[inline]
pub unsafe fn lua_tointeger(L: *mut lua_State, idx: i32) -> i32 {
    lua_tointegerx(L, idx, None)
}

pub unsafe fn lua_tounsignedx(L: *mut lua_State, idx: i32, isnum: Option<&mut i32>) -> u32 {
    let mut n = TValue::NIL;
    let o = tonumber(index2addr(L, idx), &mut n);
    if !o.is_null() {
        if let Some(i) = isnum {
            *i = 1;
        }
        crate::lnumutils::luai_num2unsigned(nvalue(o))
    } else {
        if let Some(i) = isnum {
            *i = 0;
        }
        0
    }
}

#[inline]
pub unsafe fn lua_tounsigned(L: *mut lua_State, idx: i32) -> u32 {
    lua_tounsignedx(L, idx, None)
}

pub unsafe fn lua_toboolean(L: *mut lua_State, idx: i32) -> i32 {
    (!l_isfalse(index2addr(L, idx))) as i32
}

pub unsafe fn lua_tointeger64(L: *mut lua_State, idx: i32, isinteger: Option<&mut i32>) -> i64 {
    let o = index2addr(L, idx);
    if ttisinteger(o) {
        if let Some(i) = isinteger {
            *i = 1;
        }
        lvalue(o)
    } else {
        if let Some(i) = isinteger {
            *i = 0;
        }
        0
    }
}

/// `lua_tolstring`: converts numbers in place; `None` for other types.
pub unsafe fn lua_tolstring<'a>(L: *mut lua_State, idx: i32) -> Option<&'a [u8]> {
    let mut o = index2addr(L, idx);
    if !ttisstring(o) {
        luaC_threadbarrier(L);
        if luaV_tostring(L, o) == 0 {
            return None;
        }
        luaC_checkGC(L);
        o = index2addr(L, idx);
    }
    Some(sbytes(tsvalue(o)))
}

#[inline]
pub unsafe fn lua_tostring<'a>(L: *mut lua_State, idx: i32) -> Option<&'a [u8]> {
    lua_tolstring(L, idx)
}

pub unsafe fn lua_tostringatom<'a>(L: *mut lua_State, idx: i32, atom: Option<&mut i32>) -> Option<&'a [u8]> {
    let o = index2addr(L, idx);
    if !ttisstring(o) {
        return None;
    }
    let s = tsvalue(o);
    if let Some(a) = atom {
        luaS_updateatom(L, s);
        *a = (*s).atom as i32;
    }
    Some(sbytes(s))
}

pub unsafe fn lua_namecallatom<'a>(L: *mut lua_State, atom: Option<&mut i32>) -> Option<&'a [u8]> {
    let s = (*L).namecall;
    if s.is_null() {
        return None;
    }
    if let Some(a) = atom {
        luaS_updateatom(L, s);
        *a = (*s).atom as i32;
    }
    Some(sbytes(s))
}

#[inline]
pub unsafe fn luaS_updateatom(L: *mut lua_State, ts: *mut TString) {
    if (*ts).atom == ATOM_UNDEF {
        (*ts).atom = match (*(*L).global).cb.useratom {
            Some(f) => f(L, getstr(ts), (*ts).len as usize),
            None => -1,
        };
    }
}

pub unsafe fn lua_tovector(L: *mut lua_State, idx: i32) -> Option<[f32; 3]> {
    let o = index2addr(L, idx);
    if !ttisvector(o) {
        return None;
    }
    let v = vvalue(o);
    Some([*v, *v.add(1), *v.add(2)])
}

pub unsafe fn lua_objlen(L: *mut lua_State, idx: i32) -> i32 {
    let o = index2addr(L, idx);
    match (*o).tt {
        LUA_TSTRING => (*tsvalue(o)).len as i32,
        LUA_TUSERDATA => (*uvalue(o)).len,
        LUA_TBUFFER => (*bufvalue(o)).len as i32,
        LUA_TTABLE => luaH_getn(hvalue(o)),
        _ => 0,
    }
}

pub unsafe fn lua_tocfunction(L: *mut lua_State, idx: i32) -> Option<lua_CFunction> {
    let o = index2addr(L, idx);
    if !iscfunction(o) {
        None
    } else {
        Closure::c(clvalue(o)).f
    }
}

pub unsafe fn lua_tolightuserdata(L: *mut lua_State, idx: i32) -> *mut u8 {
    let o = index2addr(L, idx);
    if !ttislightuserdata(o) {
        ptr::null_mut()
    } else {
        pvalue(o)
    }
}

pub unsafe fn lua_tolightuserdatatagged(L: *mut lua_State, idx: i32, tag: i32) -> *mut u8 {
    let o = index2addr(L, idx);
    if !ttislightuserdata(o) || lightuserdatatag(o) != tag {
        ptr::null_mut()
    } else {
        pvalue(o)
    }
}

pub unsafe fn lua_touserdata(L: *mut lua_State, idx: i32) -> *mut u8 {
    let o = index2addr(L, idx);
    if ttisuserdata(o) {
        Udata::data(uvalue(o))
    } else if ttislightuserdata(o) {
        pvalue(o)
    } else {
        ptr::null_mut()
    }
}

pub unsafe fn lua_touserdatatagged(L: *mut lua_State, idx: i32, tag: i32) -> *mut u8 {
    let o = index2addr(L, idx);
    if ttisuserdata(o) && (*uvalue(o)).tag as i32 == tag {
        Udata::data(uvalue(o))
    } else {
        ptr::null_mut()
    }
}

pub unsafe fn lua_userdatatag(L: *mut lua_State, idx: i32) -> i32 {
    let o = index2addr(L, idx);
    if ttisuserdata(o) {
        (*uvalue(o)).tag as i32
    } else {
        -1
    }
}

pub unsafe fn lua_lightuserdatatag(L: *mut lua_State, idx: i32) -> i32 {
    let o = index2addr(L, idx);
    if ttislightuserdata(o) {
        lightuserdatatag(o)
    } else {
        -1
    }
}

pub unsafe fn lua_tothread(L: *mut lua_State, idx: i32) -> *mut lua_State {
    let o = index2addr(L, idx);
    if !ttisthread(o) {
        ptr::null_mut()
    } else {
        thvalue(o)
    }
}

pub unsafe fn lua_tobuffer(L: *mut lua_State, idx: i32, len: Option<&mut usize>) -> *mut u8 {
    let o = index2addr(L, idx);
    if !ttisbuffer(o) {
        return ptr::null_mut();
    }
    let b = bufvalue(o);
    if let Some(l) = len {
        *l = (*b).len as usize;
    }
    Buffer::data(b)
}

pub unsafe fn lua_topointer(L: *mut lua_State, idx: i32) -> *const u8 {
    let o = index2addr(L, idx);
    match (*o).tt {
        LUA_TUSERDATA => Udata::data(uvalue(o)),
        LUA_TLIGHTUSERDATA => pvalue(o),
        _ => {
            if iscollectable(o) {
                gcvalue(o) as *const u8
            } else {
                ptr::null()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// push functions
// ---------------------------------------------------------------------------

pub unsafe fn lua_pushnil(L: *mut lua_State) {
    ensure_stack(L, 1);
    setnilvalue((*L).top);
    api_incr_top(L);
}

pub unsafe fn lua_pushnumber(L: *mut lua_State, n: f64) {
    ensure_stack(L, 1);
    setnvalue((*L).top, n);
    api_incr_top(L);
}

pub unsafe fn lua_pushinteger(L: *mut lua_State, n: i32) {
    ensure_stack(L, 1);
    setnvalue((*L).top, n as f64);
    api_incr_top(L);
}

pub unsafe fn lua_pushinteger64(L: *mut lua_State, n: i64) {
    ensure_stack(L, 1);
    setlvalue((*L).top, n);
    api_incr_top(L);
}

pub unsafe fn lua_pushunsigned(L: *mut lua_State, u: u32) {
    ensure_stack(L, 1);
    setnvalue((*L).top, u as f64);
    api_incr_top(L);
}

pub unsafe fn lua_pushvector(L: *mut lua_State, x: f32, y: f32, z: f32) {
    ensure_stack(L, 1);
    setvvalue((*L).top, x, y, z);
    api_incr_top(L);
}

pub unsafe fn lua_pushlstring(L: *mut lua_State, s: &[u8]) {
    luaC_checkGC(L);
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    setsvalue((*L).top, luaS_newlstr(L, s));
    api_incr_top(L);
}

#[inline]
pub unsafe fn lua_pushstring(L: *mut lua_State, s: &str) {
    lua_pushlstring(L, s.as_bytes())
}

/// `lua_pushfstring` with an already formatted message.
pub unsafe fn lua_pushfstring(L: *mut lua_State, s: &str) {
    luaC_checkGC(L);
    luaC_threadbarrier(L);
    luaO_pushfstring(L, s);
}

pub unsafe fn lua_pushcclosurek(L: *mut lua_State, f: lua_CFunction, debugname: Option<&str>, nup: i32, cont: Option<lua_Continuation>) {
    luaC_checkGC(L);
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let cl = luaF_newCclosure(L, nup, getcurrenv(L));
    let cc = Closure::c(cl);
    cc.f = Some(f);
    cc.cont = cont;
    cc.debugname = match debugname {
        Some(n) => luaS_new(L, n),
        None => ptr::null_mut(),
    };
    (*L).top = (*L).top.sub(nup as usize);
    let up = Closure::cupvals(cl);
    for i in (0..nup as usize).rev() {
        setobj(up.add(i), (*L).top.add(i));
    }
    setclvalue((*L).top, cl);
    api_incr_top(L);
}

#[inline]
pub unsafe fn lua_pushcfunction(L: *mut lua_State, f: lua_CFunction, debugname: Option<&str>) {
    lua_pushcclosurek(L, f, debugname, 0, None)
}

#[inline]
pub unsafe fn lua_pushcclosure(L: *mut lua_State, f: lua_CFunction, debugname: Option<&str>, nup: i32) {
    lua_pushcclosurek(L, f, debugname, nup, None)
}

pub unsafe fn lua_pushboolean(L: *mut lua_State, b: i32) {
    ensure_stack(L, 1);
    setbvalue((*L).top, b != 0);
    api_incr_top(L);
}

pub unsafe fn lua_pushlightuserdatatagged(L: *mut lua_State, p: *mut u8, tag: i32) {
    ensure_stack(L, 1);
    setpvalue((*L).top, p, tag);
    api_incr_top(L);
}

#[inline]
pub unsafe fn lua_pushlightuserdata(L: *mut lua_State, p: *mut u8) {
    lua_pushlightuserdatatagged(L, p, 0)
}

pub unsafe fn lua_pushthread(L: *mut lua_State) -> i32 {
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    setthvalue((*L).top, L);
    api_incr_top(L);
    ((*(*L).global).mainthread == L) as i32
}

// ---------------------------------------------------------------------------
// get functions
// ---------------------------------------------------------------------------

pub unsafe fn lua_gettable(L: *mut lua_State, idx: i32) -> i32 {
    luaC_threadbarrier(L);
    let t = index2addr(L, idx);
    luaV_gettable(L, t, (*L).top.sub(1), (*L).top.sub(1));
    (*(*L).top.sub(1)).tt
}

pub unsafe fn lua_getfield(L: *mut lua_State, idx: i32, k: &str) -> i32 {
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let t = index2addr(L, idx);
    let mut key = TValue::NIL;
    setsvalue(&mut key, luaS_new(L, k));
    luaV_gettable(L, t, &mut key, (*L).top);
    api_incr_top(L);
    (*(*L).top.sub(1)).tt
}

pub unsafe fn lua_rawgetfield(L: *mut lua_State, idx: i32, k: &str) -> i32 {
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let t = index2addr(L, idx);
    let key = luaS_new(L, k);
    setobj((*L).top, luaH_getstr(hvalue(t), key));
    api_incr_top(L);
    (*(*L).top.sub(1)).tt
}

pub unsafe fn lua_rawget(L: *mut lua_State, idx: i32) -> i32 {
    luaC_threadbarrier(L);
    let t = index2addr(L, idx);
    setobj((*L).top.sub(1), luaH_get(hvalue(t), (*L).top.sub(1)));
    (*(*L).top.sub(1)).tt
}

pub unsafe fn lua_rawgeti(L: *mut lua_State, idx: i32, n: i32) -> i32 {
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let t = index2addr(L, idx);
    setobj((*L).top, luaH_getnum(hvalue(t), n));
    api_incr_top(L);
    (*(*L).top.sub(1)).tt
}

pub unsafe fn lua_rawgetptagged(L: *mut lua_State, idx: i32, p: *mut u8, tag: i32) -> i32 {
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let t = index2addr(L, idx);
    setobj((*L).top, luaH_getp(hvalue(t), p, tag));
    api_incr_top(L);
    (*(*L).top.sub(1)).tt
}

pub unsafe fn lua_createtable(L: *mut lua_State, narray: i32, nrec: i32) {
    luaC_checkGC(L);
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    sethvalue((*L).top, luaH_new(L, narray, nrec));
    api_incr_top(L);
}

#[inline]
pub unsafe fn lua_newtable(L: *mut lua_State) {
    lua_createtable(L, 0, 0)
}

pub unsafe fn lua_setreadonly(L: *mut lua_State, objindex: i32, enabled: i32) {
    let o = index2addr(L, objindex);
    luaH_setreadonly(L, hvalue(o), enabled != 0);
}

pub unsafe fn lua_getreadonly(L: *mut lua_State, objindex: i32) -> i32 {
    luaH_getreadonly(hvalue(index2addr(L, objindex)))
}

pub unsafe fn lua_setsafeenv(L: *mut lua_State, objindex: i32, enabled: i32) {
    (*hvalue(index2addr(L, objindex))).safeenv = (enabled != 0) as u8;
}

pub unsafe fn lua_getmetatable(L: *mut lua_State, objindex: i32) -> i32 {
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let obj = index2addr(L, objindex);
    let mt = match (*obj).tt {
        LUA_TTABLE => (*hvalue(obj)).metatable,
        LUA_TUSERDATA => (*uvalue(obj)).metatable,
        LUA_TOBJECT => (*(*objectvalue(obj)).lclass).instancemetatable,
        tt => (*(*L).global).mt[tt as usize],
    };
    if !mt.is_null() {
        sethvalue((*L).top, mt);
        api_incr_top(L);
    }
    (!mt.is_null()) as i32
}

pub unsafe fn lua_getfenv(L: *mut lua_State, idx: i32) {
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let o = index2addr(L, idx);
    match (*o).tt {
        LUA_TFUNCTION => sethvalue((*L).top, (*clvalue(o)).env),
        LUA_TTHREAD => sethvalue((*L).top, (*thvalue(o)).gt),
        _ => setnilvalue((*L).top),
    }
    api_incr_top(L);
}

// ---------------------------------------------------------------------------
// set functions
// ---------------------------------------------------------------------------

pub unsafe fn lua_settable(L: *mut lua_State, idx: i32) {
    let t = index2addr(L, idx);
    luaV_settable(L, t, (*L).top.sub(2), (*L).top.sub(1));
    (*L).top = (*L).top.sub(2);
}

pub unsafe fn lua_setfield(L: *mut lua_State, idx: i32, k: &str) {
    let t = index2addr(L, idx);
    let mut key = TValue::NIL;
    setsvalue(&mut key, luaS_new(L, k));
    luaV_settable(L, t, &mut key, (*L).top.sub(1));
    (*L).top = (*L).top.sub(1);
}

#[inline]
pub unsafe fn lua_setglobal(L: *mut lua_State, k: &str) {
    lua_setfield(L, LUA_GLOBALSINDEX, k)
}

#[inline]
pub unsafe fn lua_getglobal(L: *mut lua_State, k: &str) -> i32 {
    lua_getfield(L, LUA_GLOBALSINDEX, k)
}

pub unsafe fn lua_rawsetfield(L: *mut lua_State, idx: i32, k: &str) {
    let t = index2addr(L, idx);
    if (*hvalue(t)).readonly != 0 {
        luaG_readonlyerror(L);
    }
    setobj(luaH_setstr(L, hvalue(t), luaS_new(L, k)), (*L).top.sub(1));
    luaC_barriert(L, hvalue(t), (*L).top.sub(1));
    (*L).top = (*L).top.sub(1);
}

pub unsafe fn lua_rawset(L: *mut lua_State, idx: i32) {
    let t = index2addr(L, idx);
    if (*hvalue(t)).readonly != 0 {
        luaG_readonlyerror(L);
    }
    setobj(luaH_set(L, hvalue(t), (*L).top.sub(2)), (*L).top.sub(1));
    luaC_barriert(L, hvalue(t), (*L).top.sub(1));
    (*L).top = (*L).top.sub(2);
}

pub unsafe fn lua_rawseti(L: *mut lua_State, idx: i32, n: i32) {
    let o = index2addr(L, idx);
    if (*hvalue(o)).readonly != 0 {
        luaG_readonlyerror(L);
    }
    setobj(luaH_setnum(L, hvalue(o), n), (*L).top.sub(1));
    luaC_barriert(L, hvalue(o), (*L).top.sub(1));
    (*L).top = (*L).top.sub(1);
}

pub unsafe fn lua_rawsetptagged(L: *mut lua_State, idx: i32, p: *mut u8, tag: i32) {
    let o = index2addr(L, idx);
    if (*hvalue(o)).readonly != 0 {
        luaG_readonlyerror(L);
    }
    setobj(luaH_setp(L, hvalue(o), p, tag), (*L).top.sub(1));
    luaC_barriert(L, hvalue(o), (*L).top.sub(1));
    (*L).top = (*L).top.sub(1);
}

pub unsafe fn lua_setmetatable(L: *mut lua_State, objindex: i32) -> i32 {
    let obj = index2addr(L, objindex);
    let mut mt: *mut LuaTable = ptr::null_mut();
    if !ttisnil((*L).top.sub(1)) {
        mt = hvalue((*L).top.sub(1));
    }
    match (*obj).tt {
        LUA_TTABLE => {
            if (*hvalue(obj)).readonly != 0 {
                luaG_readonlyerror(L);
            }
            (*hvalue(obj)).metatable = mt;
            if !mt.is_null() {
                luaC_objbarrier(L, hvalue(obj) as *mut GCObject, mt as *mut GCObject);
            }
        }
        LUA_TUSERDATA => {
            (*uvalue(obj)).metatable = mt;
            if !mt.is_null() {
                luaC_objbarrier(L, uvalue(obj) as *mut GCObject, mt as *mut GCObject);
            }
        }
        tt => {
            (*(*L).global).mt[tt as usize] = mt;
        }
    }
    (*L).top = (*L).top.sub(1);
    1
}

pub unsafe fn lua_setfenv(L: *mut lua_State, idx: i32) -> i32 {
    let o = index2addr(L, idx);
    let mut res = 1;
    match (*o).tt {
        LUA_TFUNCTION => (*clvalue(o)).env = hvalue((*L).top.sub(1)),
        LUA_TTHREAD => (*thvalue(o)).gt = hvalue((*L).top.sub(1)),
        _ => res = 0,
    }
    if res != 0 {
        luaC_objbarrier(L, gcvalue(o), hvalue((*L).top.sub(1)) as *mut GCObject);
    }
    (*L).top = (*L).top.sub(1);
    res
}

// ---------------------------------------------------------------------------
// calls
// ---------------------------------------------------------------------------

#[inline(always)]
unsafe fn adjustresults(L: *mut lua_State, nres: i32) {
    if nres == LUA_MULTRET && (*L).top >= (*(*L).ci).top {
        (*(*L).ci).top = (*L).top;
    }
}

pub unsafe fn lua_call(L: *mut lua_State, nargs: i32, nresults: i32) {
    if nresults > nargs + 1 {
        ensure_stack(L, nresults - (nargs + 1));
    }
    let func = (*L).top.sub(nargs as usize + 1);
    luaD_call(L, func, nresults);
    adjustresults(L, nresults);
}

pub unsafe fn lua_pcall(L: *mut lua_State, nargs: i32, nresults: i32, errfunc: i32) -> i32 {
    if nresults > nargs + 1 {
        ensure_stack(L, nresults - (nargs + 1));
    }
    let mut func = 0;
    if errfunc != 0 {
        let o = index2addr(L, errfunc);
        func = savestack(L, o);
    }
    let cfunc = (*L).top.sub(nargs as usize + 1);
    let status = luaD_pcall(L, &mut || luaD_call(L, cfunc, nresults), savestack(L, cfunc), func);
    adjustresults(L, nresults);
    status
}

pub unsafe fn lua_cpcall(L: *mut lua_State, func: lua_CFunction, ud: *mut u8) -> i32 {
    let top = savestack(L, (*L).top);
    luaD_pcall(
        L,
        &mut || {
            if lua_checkstack(L, 2) == 0 {
                luaG_runerror(L, "stack limit");
            }
            lua_pushcclosurek(L, func, None, 0, None);
            lua_pushlightuserdata(L, ud);
            luaD_call(L, (*L).top.sub(2), 0);
        },
        top,
        0,
    )
}

pub unsafe fn lua_callyieldable(L: *mut lua_State, nargs: i32, nresults: i32) -> i32 {
    let cl = clvalue((*(*L).ci).func);
    lua_call(L, nargs, nresults);
    if isyielded(L) {
        return C_CALL_YIELD;
    }
    (Closure::c(cl).cont.unwrap())(L, LUA_OK)
}

pub unsafe fn lua_pcallyieldable(L: *mut lua_State, nargs: i32, nresults: i32, errfunc: i32) -> i32 {
    let cl = clvalue((*(*L).ci).func);
    (*(*L).ci).set_errfunc(errfunc);
    (*(*L).ci).flags |= LUA_CALLINFO_HANDLE;
    let func = (*L).top.sub(nargs as usize + 1);
    let savedfunc = savestack(L, func);
    let savederrfunc = if errfunc != 0 { savestack(L, (*L).base.add(errfunc as usize - 1)) } else { 0 };
    let status = luaD_pcall(
        L,
        &mut || {
            let f = restorestack(L, savedfunc);
            luaD_callint(L, f, nresults, lua_isyieldable(L) != 0)
        },
        savedfunc,
        savederrfunc,
    );
    expandstacklimit(L, (*L).top);
    if status == 0 && isyielded(L) {
        return C_CALL_YIELD;
    }
    (*(*L).ci).flags &= !LUA_CALLINFO_HANDLE;
    (Closure::c(cl).cont.unwrap())(L, status)
}

pub unsafe fn lua_status(L: *mut lua_State) -> i32 {
    (*L).status as i32
}

pub unsafe fn lua_costatus(_L: *mut lua_State, co: *mut lua_State) -> i32 {
    if co == _L {
        return LUA_CORUN;
    }
    let st = (*co).status as i32;
    if st == LUA_YIELD {
        return LUA_COSUS;
    }
    if st == LUA_BREAK {
        return LUA_CONOR;
    }
    if st != 0 {
        return LUA_COERR;
    }
    if (*co).ci != (*co).base_ci {
        return LUA_CONOR;
    }
    if (*co).top == (*co).base {
        return LUA_COFIN;
    }
    LUA_COSUS
}

pub unsafe fn lua_hasfinalizers(L: *mut lua_State) -> i32 {
    (!(*L).finalizers.is_null()) as i32
}

unsafe fn runfinalizery(L: *mut lua_State) -> i32 {
    let co = lua_tothread(L, lua_upvalueindex(1));
    let result = lua_tointeger(L, lua_upvalueindex(2));
    if co.is_null() {
        luaG_runerror(L, "finalizer function has already been called");
    }
    if co == L {
        luaG_runerror(L, "cannot finalize running thread");
    }
    if (*co).finalizers.is_null() {
        luaG_runerror(L, "coroutine has no pending finalizers");
    }
    lua_settop(L, 0);
    lua_pushvalue(L, lua_upvalueindex(1));
    setnilvalue(index2addr(L, lua_upvalueindex(1)));
    if result == FINALLY_CANCELLED {
        luaD_preparefinalizestate(L, co, result);
        crate::lstate::lua_resetthread(co);
    } else {
        luaD_preparefinalize(L, co, result);
    }
    luaD_runfinalizers(L, false)
}

unsafe fn runfinalizercont(L: *mut lua_State, status: i32) -> i32 {
    if status != LUA_OK {
        luaD_throw(L, status);
    }
    luaD_runfinalizers(L, false)
}

pub unsafe fn lua_pushfinalizerfunction(L: *mut lua_State, co: *mut lua_State, toclose: i32) {
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    setthvalue((*L).top, co);
    api_incr_top(L);
    lua_pushinteger(
        L,
        if toclose != 0 {
            FINALLY_CANCELLED
        } else if (*co).status == LUA_OK as u8 {
            FINALLY_FINISHED
        } else {
            FINALLY_ERROR
        },
    );
    lua_pushcclosurek(L, runfinalizery, Some("finalize"), 2, Some(runfinalizercont));
}

pub unsafe fn lua_addfinalizer(L: *mut lua_State, co: *mut lua_State, idx: i32) {
    if co == lua_mainthread(L) {
        luaG_runerror(L, "cannot register a finalizer on the main thread");
    }
    let status = lua_costatus(L, co);
    if status == LUA_COFIN || status == LUA_COERR {
        luaG_runerror(L, "cannot register a finalizer on dead coroutine");
    }
    let cb = index2addr(L, idx);
    if (*co).finalizers.is_null() {
        (*co).finalizers = luaH_new(L, 1, 0);
        luaC_objbarrier(L, co as *mut GCObject, (*co).finalizers as *mut GCObject);
    }
    let slot = luaH_setnum(L, (*co).finalizers, luaH_getn((*co).finalizers) + 1);
    setobj(slot, cb);
    luaC_barriert(L, (*co).finalizers, cb);
}

pub unsafe fn lua_getthreaddata(L: *mut lua_State) -> *mut u8 {
    (*L).userdata
}

pub unsafe fn lua_setthreaddata(L: *mut lua_State, data: *mut u8) {
    (*L).userdata = data;
}

// ---------------------------------------------------------------------------
// GC
// ---------------------------------------------------------------------------

pub const LUA_GCISPAUSED: i32 = 10;

pub unsafe fn lua_gc(L: *mut lua_State, what: i32, data: i32) -> i32 {
    let mut res = 0;
    let g = (*L).global;
    match what {
        LUA_GCSTOP => (*g).GCthreshold = usize::MAX,
        LUA_GCRESTART => (*g).GCthreshold = (*g).totalbytes,
        LUA_GCCOLLECT => {
            if (*g).gcdisabled == 0 {
                luaC_fullgc(L);
            }
        }
        LUA_GCCOUNT => res = ((*g).totalbytes >> 10) as i32,
        LUA_GCCOUNTB => res = ((*g).totalbytes & 1023) as i32,
        LUA_GCISRUNNING => res = ((*g).GCthreshold != usize::MAX) as i32,
        LUA_GCSTEP => {
            if (*g).gcdisabled == 0 {
                let amount = (data as usize) << 10;
                let oldcredit: isize = if (*g).gcstate == GCSpause { 0 } else { (*g).GCthreshold as isize - (*g).totalbytes as isize };
                if amount <= (*g).totalbytes {
                    (*g).GCthreshold = (*g).totalbytes - amount;
                } else {
                    (*g).GCthreshold = 0;
                }
                let mut actualwork = 0usize;
                while (*g).GCthreshold <= (*g).totalbytes {
                    let stepsize = luaC_step(L, false);
                    actualwork += stepsize;
                    if (*g).gcstate == GCSpause {
                        res = 1;
                        break;
                    }
                }
                if (*g).gcstate != GCSpause {
                    let newthreshold = (*g).totalbytes as isize + actualwork as isize + oldcredit;
                    (*g).GCthreshold = if newthreshold < 0 { 0 } else { newthreshold as usize };
                }
            }
        }
        LUA_GCSETGOAL => {
            res = (*g).gcgoal;
            (*g).gcgoal = data;
        }
        LUA_GCSETSTEPMUL => {
            res = (*g).gcstepmul;
            (*g).gcstepmul = data;
        }
        LUA_GCSETSTEPSIZE => {
            res = (*g).gcstepsize >> 10;
            (*g).gcstepsize = data << 10;
        }
        LUA_GCISPAUSED => res = ((*g).gcstate == GCSpause) as i32,
        _ => res = -1,
    }
    res
}

// ---------------------------------------------------------------------------
// misc
// ---------------------------------------------------------------------------

pub unsafe fn lua_error(L: *mut lua_State) -> ! {
    luaD_throw(L, LUA_ERRRUN)
}

pub unsafe fn lua_next(L: *mut lua_State, idx: i32) -> i32 {
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let t = index2addr(L, idx);
    let more = luaH_next(L, hvalue(t), (*L).top.sub(1));
    if more != 0 {
        api_incr_top(L);
    } else {
        (*L).top = (*L).top.sub(1);
    }
    more
}

pub unsafe fn lua_rawiter(L: *mut lua_State, idx: i32, mut iter: i32) -> i32 {
    luaC_threadbarrier(L);
    ensure_stack(L, 2);
    let t = index2addr(L, idx);
    let h = hvalue(t);
    let sizearray = (*h).sizearray;
    while (iter as u32) < sizearray as u32 {
        let e = (*h).array.add(iter as usize);
        if !ttisnil(e) {
            let top = (*L).top;
            setnvalue(top, (iter + 1) as f64);
            setobj(top.add(1), e);
            (*L).top = top.add(2);
            return iter + 1;
        }
        iter += 1;
    }
    let sizenode = 1i32 << (*h).lsizenode;
    while ((iter - sizearray) as u32) < sizenode as u32 {
        let n = (*h).node.add((iter - sizearray) as usize);
        if !ttisnil(gval(n)) {
            let top = (*L).top;
            getnodekey(top, n);
            setobj(top.add(1), gval(n));
            (*L).top = top.add(2);
            return iter + 1;
        }
        iter += 1;
    }
    -1
}

pub unsafe fn lua_concat(L: *mut lua_State, n: i32) {
    if n >= 2 {
        luaC_checkGC(L);
        luaC_threadbarrier(L);
        luaV_concat(L, n, (*L).top.offset_from((*L).base) as i32 - 1);
        (*L).top = (*L).top.sub(n as usize - 1);
    } else if n == 0 {
        luaC_threadbarrier(L);
        ensure_stack(L, 1);
        setsvalue((*L).top, luaS_newlstr(L, b""));
        api_incr_top(L);
    }
}

pub unsafe fn lua_newuserdatatagged(L: *mut lua_State, sz: usize, tag: i32) -> *mut u8 {
    luaC_checkGC(L);
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let u = luaU_newudata(L, sz, tag);
    setuvalue((*L).top, u);
    api_incr_top(L);
    Udata::data(u)
}

#[inline]
pub unsafe fn lua_newuserdata(L: *mut lua_State, sz: usize) -> *mut u8 {
    lua_newuserdatatagged(L, sz, 0)
}

pub unsafe fn lua_newuserdatataggedwithmetatable(L: *mut lua_State, sz: usize, tag: i32) -> *mut u8 {
    luaC_checkGC(L);
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let u = luaU_newudata(L, sz, tag);
    (*u).metatable = (*(*L).global).udatamt[tag as usize];
    setuvalue((*L).top, u);
    api_incr_top(L);
    Udata::data(u)
}

pub unsafe fn lua_newuserdatadtor(L: *mut lua_State, sz: usize, dtor: lua_Destructor) -> *mut u8 {
    luaC_checkGC(L);
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let as_ = sz + std::mem::size_of::<lua_Destructor>();
    let u = luaU_newudata(L, as_, UTAG_IDTOR as i32);
    ptr::write_unaligned(Udata::data(u).add(sz) as *mut lua_Destructor, dtor);
    setuvalue((*L).top, u);
    api_incr_top(L);
    Udata::data(u)
}

pub unsafe fn lua_newbuffer(L: *mut lua_State, sz: usize) -> *mut u8 {
    luaC_checkGC(L);
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let b = luaB_newbuffer(L, sz);
    setbufvalue((*L).top, b);
    api_incr_top(L);
    Buffer::data(b)
}

/// `aux_upvalue`: the upvalue slot and its name (`b""` when unnamed).
unsafe fn aux_upvalue<'a>(fi: StkId, n: i32) -> Option<(*mut TValue, &'a [u8])> {
    if !ttisfunction(fi) {
        return None;
    }
    let f = clvalue(fi);
    if (*f).isC != 0 {
        if !(1 <= n && n <= (*f).nupvalues as i32) {
            return None;
        }
        Some((Closure::cupvals(f).add(n as usize - 1), b""))
    } else {
        let p = Closure::proto(f);
        if !(1 <= n && n <= (*p).nups as i32) {
            return None;
        }
        let r = Closure::uprefs(f).add(n as usize - 1);
        let val = if (*r).tt == LUA_TUPVAL { (*upvalue(r)).v } else { r };
        if !(1 <= n && n <= (*p).sizeupvalues) {
            return Some((val, b""));
        }
        Some((val, sbytes(*(*p).upvalues.add(n as usize - 1))))
    }
}

pub unsafe fn lua_getupvalue<'a>(L: *mut lua_State, funcindex: i32, n: i32) -> Option<&'a [u8]> {
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let (val, name) = aux_upvalue(index2addr(L, funcindex), n)?;
    setobj((*L).top, val);
    api_incr_top(L);
    Some(name)
}

pub unsafe fn lua_setupvalue<'a>(L: *mut lua_State, funcindex: i32, n: i32) -> Option<&'a [u8]> {
    let fi = index2addr(L, funcindex);
    let (val, name) = aux_upvalue(fi, n)?;
    (*L).top = (*L).top.sub(1);
    setobj(val, (*L).top);
    luaC_barrier(L, clvalue(fi) as *mut GCObject, (*L).top);
    Some(name)
}

pub unsafe fn lua_ref(L: *mut lua_State, idx: i32) -> i32 {
    let mut r = LUA_REFNIL;
    let g = (*L).global;
    let p = index2addr(L, idx);
    if !ttisnil(p) {
        let reg = hvalue(registry(L));
        if (*g).registryfree != 0 {
            r = (*g).registryfree;
        } else {
            r = luaH_getn(reg) + 1;
        }
        let slot = luaH_setnum(L, reg, r);
        if (*g).registryfree != 0 {
            (*g).registryfree = nvalue(slot) as i32;
        }
        setobj(slot, p);
        luaC_barriert(L, reg, p);
    }
    r
}

pub unsafe fn lua_unref(L: *mut lua_State, r: i32) -> i32 {
    if r <= LUA_REFNIL {
        return LUA_NOREF;
    }
    let g = (*L).global;
    let reg = hvalue(registry(L));
    let slot = luaH_getnum(reg, r) as *mut TValue;
    setnvalue(slot, (*g).registryfree as f64);
    (*g).registryfree = r;
    LUA_NOREF
}

pub unsafe fn lua_setuserdatatag(L: *mut lua_State, idx: i32, tag: i32) {
    (*uvalue(index2addr(L, idx))).tag = tag as u8;
}

pub unsafe fn lua_setuserdatadtor(L: *mut lua_State, tag: i32, dtor: Option<lua_Destructor>) {
    (*(*L).global).udatagc[tag as usize] = dtor;
}

pub unsafe fn lua_getuserdatadtor(L: *mut lua_State, tag: i32) -> Option<lua_Destructor> {
    (*(*L).global).udatagc[tag as usize]
}

pub unsafe fn lua_setuserdatametatable(L: *mut lua_State, tag: i32) {
    (*(*L).global).udatamt[tag as usize] = hvalue((*L).top.sub(1));
    (*L).top = (*L).top.sub(1);
}

pub unsafe fn lua_getuserdatametatable(L: *mut lua_State, tag: i32) {
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let h = (*(*L).global).udatamt[tag as usize];
    if !h.is_null() {
        sethvalue((*L).top, h);
    } else {
        setnilvalue((*L).top);
    }
    api_incr_top(L);
}

pub unsafe fn lua_getuserdataname(L: *mut lua_State, tag: i32) -> Vec<u8> {
    let mt = (*(*L).global).udatamt[tag as usize];
    if !mt.is_null() {
        let ty = crate::ltable::luaH_getstr(mt, (*(*L).global).tmname[crate::ltm::TM_TYPE as usize]);
        if ttisstring(ty) {
            return sbytes(tsvalue(ty)).to_vec();
        }
    }
    b"userdata".to_vec()
}

pub unsafe fn lua_setlightuserdataname(L: *mut lua_State, tag: i32, name: &str) {
    let g = (*L).global;
    if (*g).lightuserdataname[tag as usize].is_null() {
        (*g).lightuserdataname[tag as usize] = luaS_new(L, name);
        luaS_fix((*g).lightuserdataname[tag as usize]);
    }
}

pub unsafe fn lua_getlightuserdataname<'a>(L: *mut lua_State, tag: i32) -> Option<&'a [u8]> {
    let name = (*(*L).global).lightuserdataname[tag as usize];
    if name.is_null() {
        None
    } else {
        Some(sbytes(name))
    }
}

pub unsafe fn lua_clonefunction(L: *mut lua_State, idx: i32) {
    luaC_checkGC(L);
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let p = index2addr(L, idx);
    let cl = clvalue(p);
    let newcl = luaF_newLclosure(L, (*cl).nupvalues as i32, (*L).gt, Closure::proto(cl));
    for i in 0..(*cl).nupvalues as usize {
        setobj(Closure::uprefs(newcl).add(i), Closure::uprefs(cl).add(i));
    }
    setclvalue((*L).top, newcl);
    api_incr_top(L);
}

/// `lua_usesexport`: whether the Luau function at `idx` is a module body that
/// exports values (`export local` / `export function` / exported types).
pub unsafe fn lua_usesexport(L: *mut lua_State, idx: i32) -> i32 {
    let o = luaA_toobject(L, idx);
    if o.is_null() || !ttisfunction(o) || (*clvalue(o)).isC != 0 {
        return 0;
    }
    ((*Closure::proto(clvalue(o))).flags as u32 & luau_common::bytecode::proto_flag::USES_EXPORT != 0) as i32
}

pub unsafe fn lua_cleartable(L: *mut lua_State, idx: i32) {
    let t = index2addr(L, idx);
    let tt = hvalue(t);
    if (*tt).readonly != 0 {
        luaG_readonlyerror(L);
    }
    luaH_clear(tt);
}

pub unsafe fn lua_clonetable(L: *mut lua_State, idx: i32) {
    luaC_checkGC(L);
    luaC_threadbarrier(L);
    ensure_stack(L, 1);
    let t = index2addr(L, idx);
    let tt = luaH_clone(L, hvalue(t));
    sethvalue((*L).top, tt);
    api_incr_top(L);
}

pub unsafe fn lua_callbacks(L: *mut lua_State) -> *mut lua_Callbacks {
    ptr::addr_of_mut!((*(*L).global).cb)
}

pub unsafe fn lua_setmemcat(L: *mut lua_State, category: i32) {
    (*L).activememcat = category as u8;
}

pub unsafe fn lua_totalbytes(L: *mut lua_State, category: i32) -> usize {
    if category < 0 {
        (*(*L).global).totalbytes
    } else {
        (*(*L).global).memcatbytes[category as usize]
    }
}

pub unsafe fn lua_allocationrate(L: *mut lua_State) -> i64 {
    luaC_allocationrate(L)
}

pub unsafe fn lua_encodepointer(_L: *mut lua_State, p: usize) -> usize {
    p
}
