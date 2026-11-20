//! Port of `VM/src/ltablib.cpp`: table library (`LuauTableFindImprovement`
//! on).

use crate::lapi::*;
use crate::laux::*;
use crate::lbaselib::luaL_argcheck;
use crate::ldebug::luaG_readonlyerror;
use crate::ldo::luaD_call;
use crate::lgc::*;
use crate::lobject::*;
use crate::lstring::luaS_new;
use crate::ltable::*;
use crate::ltm::{luaT_gettmbyobj, TM_EQ};
use crate::lvmutils::{equalobj, luaV_lessthan};

unsafe fn foreachi(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    luaL_checktype(L, 2, LUA_TFUNCTION);
    let n = lua_objlen(L, 1);
    for i in 1..=n {
        lua_pushvalue(L, 2);
        lua_pushinteger(L, i);
        lua_rawgeti(L, 1, i);
        lua_call(L, 2, 1);
        if !lua_isnil(L, -1) {
            return 1;
        }
        lua_pop(L, 1);
    }
    0
}

unsafe fn foreach(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    luaL_checktype(L, 2, LUA_TFUNCTION);
    lua_pushnil(L);
    while lua_next(L, 1) != 0 {
        lua_pushvalue(L, 2);
        lua_pushvalue(L, -3);
        lua_pushvalue(L, -3);
        lua_call(L, 2, 1);
        if !lua_isnil(L, -1) {
            return 1;
        }
        lua_pop(L, 2);
    }
    0
}

unsafe fn maxn(L: *mut lua_State) -> i32 {
    let mut max = 0.0f64;
    luaL_checktype(L, 1, LUA_TTABLE);
    let t = hvalue((*L).base);
    for i in 0..(*t).sizearray {
        if !ttisnil((*t).array.add(i as usize)) {
            max = (i + 1) as f64;
        }
    }
    for i in 0..sizenode(t) {
        let n = gnode(t, i);
        if !ttisnil(gval(n)) && (*n).key.tt() == LUA_TNUMBER {
            let v = (*n).key.value.n;
            if v > max {
                max = v;
            }
        }
    }
    lua_pushnumber(L, max);
    1
}

unsafe fn getn(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    lua_pushinteger(L, lua_objlen(L, 1));
    1
}

unsafe fn shouldsparsemove(src: *mut LuaTable, dst: *mut LuaTable, n: i32) -> bool {
    let srcelems = (*src).sizearray + sizenode(src);
    let dstelems = (*dst).sizearray + sizenode(dst);
    let maxelems = srcelems.max(dstelems);
    n > 32 && n / 2 > maxelems
}

unsafe fn tovalidintkey(L: *mut lua_State, idx: i32, f: i32, e: i32) -> Option<i32> {
    if lua_type(L, idx) == LUA_TNUMBER {
        let nkey = lua_tonumber(L, idx);
        if nkey >= f as f64 && nkey <= e as f64 {
            let r = nkey as i32;
            if r as f64 == nkey {
                return Some(r);
            }
        }
    }
    None
}

/// Overlap-safe copy of `n` TValues. The C runtime's `memmove` (msvcrt on
/// windows-gnu) is several times slower than a 16-byte loop for the large
/// overlapping shifts done by `table.remove`/`table.insert`.
#[inline(never)]
unsafe fn move_tvalues(s: *const TValue, d: *mut TValue, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_storeu_si128};
        const _: () = assert!(std::mem::size_of::<TValue>() == 16);
        let s = s as *const __m128i;
        let d = d as *mut __m128i;
        if (d as usize) <= (s as usize) || (d as usize) >= (s as usize) + n * 16 {
            let mut i = 0;
            while i + 4 <= n {
                let (a, b, c, e) = (_mm_loadu_si128(s.add(i)), _mm_loadu_si128(s.add(i + 1)), _mm_loadu_si128(s.add(i + 2)), _mm_loadu_si128(s.add(i + 3)));
                _mm_storeu_si128(d.add(i), a);
                _mm_storeu_si128(d.add(i + 1), b);
                _mm_storeu_si128(d.add(i + 2), c);
                _mm_storeu_si128(d.add(i + 3), e);
                i += 4;
            }
            while i < n {
                _mm_storeu_si128(d.add(i), _mm_loadu_si128(s.add(i)));
                i += 1;
            }
        } else {
            let mut i = n;
            while i >= 4 {
                let (a, b, c, e) = (_mm_loadu_si128(s.add(i - 1)), _mm_loadu_si128(s.add(i - 2)), _mm_loadu_si128(s.add(i - 3)), _mm_loadu_si128(s.add(i - 4)));
                _mm_storeu_si128(d.add(i - 1), a);
                _mm_storeu_si128(d.add(i - 2), b);
                _mm_storeu_si128(d.add(i - 3), c);
                _mm_storeu_si128(d.add(i - 4), e);
                i -= 4;
            }
            while i > 0 {
                i -= 1;
                _mm_storeu_si128(d.add(i), _mm_loadu_si128(s.add(i)));
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    std::ptr::copy(s, d, n);
}

unsafe fn moveelements(L: *mut lua_State, srct: i32, dstt: i32, f: i32, e: i32, t: i32, sparsemove: bool) {
    let src = hvalue((*L).base.add(srct as usize - 1));
    let dst = hvalue((*L).base.add(dstt as usize - 1));
    if (*dst).readonly != 0 {
        luaG_readonlyerror(L);
    }
    let n = e - f + 1;
    let (fu, tu, nu) = (f as u32, t as u32, n as u32);
    if fu.wrapping_sub(1) < (*src).sizearray as u32
        && tu.wrapping_sub(1) < (*dst).sizearray as u32
        && fu.wrapping_sub(1).wrapping_add(nu) <= (*src).sizearray as u32
        && tu.wrapping_sub(1).wrapping_add(nu) <= (*dst).sizearray as u32
    {
        // `setobj2t` is a plain value copy, so either loop direction of the
        // C++ code is a memmove (which handles the overlap)
        if n > 0 {
            move_tvalues((*src).array.add((f - 1) as usize), (*dst).array.add((t - 1) as usize), n as usize);
        }
        luaC_barrierfast(L, dst);
    } else if sparsemove {
        let srcta = lua_absindex(L, srct);
        let dstta = lua_absindex(L, dstt);
        let te = t + (n - 1);
        lua_newtable(L);
        let mut iter = 0;
        loop {
            iter = lua_rawiter(L, srcta, iter);
            if iter == -1 {
                break;
            }
            if let Some(ikey) = tovalidintkey(L, -2, f, e) {
                lua_rawseti(L, -3, ikey);
            } else {
                lua_pop(L, 1);
            }
            lua_pop(L, 1);
        }
        let mut iter = 0;
        loop {
            iter = lua_rawiter(L, dstta, iter);
            if iter == -1 {
                break;
            }
            if let Some(ikey) = tovalidintkey(L, -2, t, te) {
                lua_pushnil(L);
                lua_rawseti(L, dstta, ikey);
            }
            lua_pop(L, 2);
        }
        let mut iter = 0;
        loop {
            iter = lua_rawiter(L, -1, iter);
            if iter == -1 {
                break;
            }
            let ikey = lua_tointeger(L, -2);
            lua_rawseti(L, dstta, ikey - f + t);
            lua_pop(L, 1);
        }
        lua_pop(L, 1);
    } else if t > e || t <= f || dst != src {
        for i in 0..n {
            lua_rawgeti(L, srct, f + i);
            lua_rawseti(L, dstt, t + i);
        }
    } else {
        let mut i = n - 1;
        while i >= 0 {
            lua_rawgeti(L, srct, f + i);
            lua_rawseti(L, dstt, t + i);
            i -= 1;
        }
    }
}

unsafe fn tinsert(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    let n = lua_objlen(L, 1);
    let pos = match lua_gettop(L) {
        2 => n + 1,
        3 => {
            let pos = luaL_checkinteger(L, 2);
            if 1 <= pos && pos <= n {
                moveelements(L, 1, 1, pos, n, pos + 1, false);
            }
            pos
        }
        _ => luaL_error(L, "wrong number of arguments to 'insert'"),
    };
    lua_rawseti(L, 1, pos);
    0
}

unsafe fn tremove(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    let n = lua_objlen(L, 1);
    let pos = luaL_optinteger(L, 2, n);
    if !(1 <= pos && pos <= n) {
        return 0;
    }
    lua_rawgeti(L, 1, pos);
    moveelements(L, 1, 1, pos + 1, n, pos, false);
    lua_pushnil(L);
    lua_rawseti(L, 1, n);
    1
}

unsafe fn tmove(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    let f = luaL_checkinteger(L, 2);
    let e = luaL_checkinteger(L, 3);
    let t = luaL_checkinteger(L, 4);
    let tt = if !lua_isnoneornil(L, 5) { 5 } else { 1 };
    luaL_checktype(L, tt, LUA_TTABLE);
    if e >= f {
        luaL_argcheck(L, f > 0 || e < i32::MAX + f, 3, "too many elements to move");
        let n = e - f + 1;
        luaL_argcheck(L, t <= i32::MAX - n + 1, 4, "destination wrap around");
        let dst = hvalue((*L).base.add(tt as usize - 1));
        if (*dst).readonly != 0 {
            luaG_readonlyerror(L);
        }
        let sparsemove = shouldsparsemove(hvalue((*L).base), dst, n);
        if t > 0 && (t - 1) <= (*dst).sizearray && (t - 1 + n) > (*dst).sizearray {
            luaH_resizearray(L, dst, t - 1 + n);
        }
        moveelements(L, 1, tt, f, e, t, sparsemove);
    }
    lua_pushvalue(L, tt);
    1
}

unsafe fn addfield(L: *mut lua_State, b: &mut luaL_Strbuf, i: i32, t: *mut LuaTable) {
    if !t.is_null() && ((i - 1) as u32) < (*t).sizearray as u32 && ttisstring((*t).array.add(i as usize - 1)) {
        b.addlstring(sbytes(tsvalue((*t).array.add(i as usize - 1))));
    } else {
        let tt = lua_rawgeti(L, 1, i);
        if tt != LUA_TSTRING && tt != LUA_TNUMBER {
            let tn = luaL_typename(L, -1);
            luaL_error_owned(L, format!("invalid value ({}) at index {} in table for 'concat'", tn, i));
        }
        b.addvalue();
    }
}

unsafe fn tconcat(L: *mut lua_State) -> i32 {
    let sep = luaL_optlstring(L, 2, b"");
    luaL_checktype(L, 1, LUA_TTABLE);
    let mut i = luaL_optinteger(L, 3, 1);
    let last = if lua_isnoneornil(L, 4) { lua_objlen(L, 1) } else { luaL_checkinteger(L, 4) };
    let t = hvalue((*L).base);
    let mut b = luaL_Strbuf::new(L);
    crate::errguard!(L, b);
    while i < last {
        addfield(L, &mut b, i, t);
        if !sep.is_empty() {
            b.addlstring(&sep);
        }
        i += 1;
    }
    if i == last {
        addfield(L, &mut b, i, t);
    }
    b.pushresult();
    1
}

unsafe fn tpack(L: *mut lua_State) -> i32 {
    let n = lua_gettop(L);
    lua_createtable(L, n, 1);
    let t = hvalue((*L).top.sub(1));
    for i in 0..n as usize {
        setobj((*t).array.add(i), (*L).base.add(i));
    }
    let nv = luaH_setstr(L, t, luaS_new(L, "n"));
    setnvalue(nv, n as f64);
    1
}

unsafe fn tunpack(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    let t = hvalue((*L).base);
    let mut i = luaL_optinteger(L, 2, 1);
    let e = if lua_isnoneornil(L, 3) { lua_objlen(L, 1) } else { luaL_checkinteger(L, 3) };
    if i > e {
        return 0;
    }
    let mut n = (e as u32).wrapping_sub(i as u32);
    if n >= i32::MAX as u32 || {
        n += 1;
        lua_checkstack(L, n as i32) == 0
    } {
        luaL_error(L, "too many results to unpack");
    }
    if i == 1 && n as i32 <= (*t).sizearray {
        for k in 0..n as usize {
            setobj((*L).top.add(k), (*t).array.add(k));
        }
        (*L).top = (*L).top.add(n as usize);
    } else {
        while i < e {
            lua_rawgeti(L, 1, i);
            i += 1;
        }
        lua_rawgeti(L, 1, e);
    }
    n as i32
}


unsafe fn sort_func(L: *mut lua_State, l: *const TValue, r: *const TValue) -> i32 {
    setobj((*L).top, (*L).base.add(1));
    setobj((*L).top.add(1), l);
    setobj((*L).top.add(2), r);
    (*L).top = (*L).top.add(3);
    luaD_call(L, (*L).top.sub(3), 1);
    (*L).top = (*L).top.sub(1);
    (!l_isfalse((*L).top)) as i32
}

#[inline]
unsafe fn sort_swap(t: *mut LuaTable, i: i32, j: i32) {
    let arr = (*t).array;
    let (a, b) = (arr.add(i as usize), arr.add(j as usize));
    let tmp = *a;
    setobj(a, b);
    setobj(b, &tmp);
}

#[inline]
unsafe fn sort_less<P: Fn(*mut lua_State, *const TValue, *const TValue) -> i32 + Copy>(L: *mut lua_State, t: *mut LuaTable, i: i32, j: i32, pred: P) -> bool {
    let arr = (*t).array;
    let n = (*t).sizearray;
    let res = pred(L, arr.add(i as usize), arr.add(j as usize));
    if (*t).sizearray != n {
        luaL_error(L, "table modified during sorting");
    }
    res != 0
}

unsafe fn sort_siftheap<P: Fn(*mut lua_State, *const TValue, *const TValue) -> i32 + Copy>(L: *mut lua_State, t: *mut LuaTable, l: i32, u: i32, pred: P, mut root: i32) {
    let count = u - l + 1;
    while root * 2 + 2 < count {
        let left = root * 2 + 1;
        let right = root * 2 + 2;
        let mut next = root;
        if sort_less(L, t, l + next, l + left, pred) {
            next = left;
        }
        if sort_less(L, t, l + next, l + right, pred) {
            next = right;
        }
        if next == root {
            break;
        }
        sort_swap(t, l + root, l + next);
        root = next;
    }
    let lastleft = root * 2 + 1;
    if lastleft == count - 1 && sort_less(L, t, l + root, l + lastleft, pred) {
        sort_swap(t, l + root, l + lastleft);
    }
}

unsafe fn sort_heap<P: Fn(*mut lua_State, *const TValue, *const TValue) -> i32 + Copy>(L: *mut lua_State, t: *mut LuaTable, l: i32, u: i32, pred: P) {
    let count = u - l + 1;
    let mut i = count / 2 - 1;
    while i >= 0 {
        sort_siftheap(L, t, l, u, pred, i);
        i -= 1;
    }
    let mut i = count - 1;
    while i > 0 {
        sort_swap(t, l, l + i);
        sort_siftheap(L, t, l, l + i - 1, pred, 0);
        i -= 1;
    }
}

unsafe fn sort_rec<P: Fn(*mut lua_State, *const TValue, *const TValue) -> i32 + Copy>(L: *mut lua_State, t: *mut LuaTable, mut l: i32, mut u: i32, mut limit: i32, pred: P) {
    while l < u {
        if limit == 0 {
            return sort_heap(L, t, l, u, pred);
        }
        if sort_less(L, t, u, l, pred) {
            sort_swap(t, u, l);
        }
        if u - l == 1 {
            break;
        }
        let m = l + ((u - l) >> 1);
        if sort_less(L, t, m, l, pred) {
            sort_swap(t, m, l);
        } else if sort_less(L, t, u, m, pred) {
            sort_swap(t, m, u);
        }
        if u - l == 2 {
            break;
        }
        let p = u - 1;
        sort_swap(t, m, u - 1);
        let mut i = l;
        let mut j = u - 1;
        loop {
            i += 1;
            while sort_less(L, t, i, p, pred) {
                if i >= u {
                    luaL_error(L, "invalid order function for sorting");
                }
                i += 1;
            }
            j -= 1;
            while sort_less(L, t, p, j, pred) {
                if j <= l {
                    luaL_error(L, "invalid order function for sorting");
                }
                j -= 1;
            }
            if j < i {
                break;
            }
            sort_swap(t, i, j);
        }
        sort_swap(t, p, i);
        limit = (limit >> 1) + (limit >> 2);
        if i - l < u - i {
            sort_rec(L, t, l, i - 1, limit, pred);
            l = i + 1;
        } else {
            sort_rec(L, t, i + 1, u, limit, pred);
            u = i - 1;
        }
    }
}

unsafe fn tsort(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    let t = hvalue((*L).base);
    let n = luaH_getn(t);
    if (*t).readonly != 0 {
        luaG_readonlyerror(L);
    }
    let custom = !lua_isnoneornil(L, 2);
    if custom {
        luaL_checktype(L, 2, LUA_TFUNCTION);
    }
    lua_settop(L, 2);
    if n > 0 {
        // monomorphized per comparator, with numbers compared inline by default
        if custom {
            sort_rec(L, t, 0, n - 1, n, |L, a, b| unsafe { sort_func(L, a, b) });
        } else {
            sort_rec(L, t, 0, n - 1, n, |L, a, b| unsafe {
                if ttisnumber(a) && ttisnumber(b) {
                    (nvalue(a) < nvalue(b)) as i32
                } else {
                    luaV_lessthan(L, a, b)
                }
            });
        }
    }
    0
}

unsafe fn tcreate(L: *mut lua_State) -> i32 {
    let size = luaL_checkinteger(L, 1);
    if size < 0 {
        luaL_argerror(L, 1, "size out of range");
    }
    lua_createtable(L, size, 0);
    if !lua_isnoneornil(L, 2) {
        let t = hvalue((*L).top.sub(1));
        let v = (*L).base.add(1);
        for i in 0..size as usize {
            setobj((*t).array.add(i), v);
        }
    }
    1
}

unsafe fn tfind(L: *mut lua_State) -> i32 {
    let mut init = 1;
    let nargs = (*L).top.offset_from((*L).base) as i32;
    if nargs < 1 || !ttistable((*L).base) {
        luaL_checktype(L, 1, LUA_TTABLE);
    }
    if nargs < 2 {
        luaL_checkany(L, 2);
    }
    if nargs >= 3 && !ttisnil((*L).base.add(2)) {
        init = luaL_checkinteger(L, 3);
    }
    if init < 1 {
        luaL_argerror(L, 3, "index out of range");
    }
    let t = hvalue((*L).base);
    if init > (*t).sizearray && (*t).node == dummynode() {
        lua_pushnil(L);
        return 1;
    }
    let v = (*L).base.add(1);
    if !iscollectable(v) || ttisnil(luaT_gettmbyobj(L, v, TM_EQ)) {
        let index = luaH_rawfind(t, init, v);
        if index != 0 {
            lua_pushinteger(L, index);
        } else {
            lua_pushnil(L);
        }
        return 1;
    }
    tfind_meta(L, t, init)
}

/// table.find for a value with an `__eq` metamethod (kept out of line so
/// the common path stays small).
#[cold]
#[inline(never)]
unsafe fn tfind_meta(L: *mut lua_State, t: *mut LuaTable, init: i32) -> i32 {
    let mut i = init;
    loop {
        let e = luaH_getnum(t, i);
        if ttisnil(e) {
            break;
        }
        if equalobj(L, (*L).base.add(1), e) {
            lua_pushinteger(L, i);
            return 1;
        }
        i += 1;
    }
    lua_pushnil(L);
    1
}

unsafe fn tclear(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    let tt = hvalue((*L).base);
    if (*tt).readonly != 0 {
        luaG_readonlyerror(L);
    }
    luaH_clear(tt);
    0
}

unsafe fn tfreeze(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    luaL_argcheck(L, lua_getreadonly(L, 1) == 0, 1, "table is already frozen");
    luaL_argcheck(L, luaL_getmetafield(L, 1, "__metatable") == 0, 1, "table has a protected metatable");
    lua_setreadonly(L, 1, 1);
    lua_pushvalue(L, 1);
    1
}

unsafe fn tisfrozen(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    lua_pushboolean(L, lua_getreadonly(L, 1));
    1
}

unsafe fn tclone(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTABLE);
    luaL_argcheck(L, luaL_getmetafield(L, 1, "__metatable") == 0, 1, "table has a protected metatable");
    let tt = luaH_clone(L, hvalue((*L).base));
    let mut v = TValue::NIL;
    sethvalue(&mut v, tt);
    luaA_pushvalue(L, &v);
    1
}

const TAB_FUNCS: &[luaL_Reg] = &[
    ("concat", tconcat),
    ("foreach", foreach),
    ("foreachi", foreachi),
    ("getn", getn),
    ("maxn", maxn),
    ("insert", tinsert),
    ("remove", tremove),
    ("sort", tsort),
    ("pack", tpack),
    ("unpack", tunpack),
    ("move", tmove),
    ("create", tcreate),
    ("find", tfind),
    ("clear", tclear),
    ("freeze", tfreeze),
    ("isfrozen", tisfrozen),
    ("clone", tclone),
];

pub unsafe fn luaopen_table(L: *mut lua_State) -> i32 {
    luaL_register(L, Some("table"), TAB_FUNCS);
    lua_pushcfunction(L, tunpack, Some("unpack"));
    lua_setglobal(L, "unpack");
    1
}
