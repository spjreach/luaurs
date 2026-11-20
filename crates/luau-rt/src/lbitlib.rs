//! Port of `VM/src/lbitlib.cpp`.

use crate::lapi::*;
use crate::laux::*;
use crate::lbaselib::luaL_argcheck;
use crate::lobject::*;

const NBITS: i32 = 32;

#[inline]
fn mask(n: i32) -> u32 {
    !((!0u32 << 1).wrapping_shl((n - 1) as u32))
}

unsafe fn andaux(L: *mut lua_State) -> u32 {
    let n = lua_gettop(L);
    let mut r = !0u32;
    for i in 1..=n {
        r &= luaL_checkunsigned(L, i);
    }
    r
}

unsafe fn b_and(L: *mut lua_State) -> i32 {
    let r = andaux(L);
    lua_pushunsigned(L, r);
    1
}

unsafe fn b_test(L: *mut lua_State) -> i32 {
    let r = andaux(L);
    lua_pushboolean(L, (r != 0) as i32);
    1
}

unsafe fn b_or(L: *mut lua_State) -> i32 {
    let n = lua_gettop(L);
    let mut r = 0u32;
    for i in 1..=n {
        r |= luaL_checkunsigned(L, i);
    }
    lua_pushunsigned(L, r);
    1
}

unsafe fn b_xor(L: *mut lua_State) -> i32 {
    let n = lua_gettop(L);
    let mut r = 0u32;
    for i in 1..=n {
        r ^= luaL_checkunsigned(L, i);
    }
    lua_pushunsigned(L, r);
    1
}

unsafe fn b_not(L: *mut lua_State) -> i32 {
    let r = !luaL_checkunsigned(L, 1);
    lua_pushunsigned(L, r);
    1
}

unsafe fn b_shift(L: *mut lua_State, mut r: u32, i: i32) -> i32 {
    if i < 0 {
        let i = i.wrapping_neg();
        r = if i >= NBITS || i < 0 { 0 } else { r >> i };
    } else {
        r = if i >= NBITS { 0 } else { r << i };
    }
    lua_pushunsigned(L, r);
    1
}

unsafe fn b_lshift(L: *mut lua_State) -> i32 {
    let r = luaL_checkunsigned(L, 1);
    b_shift(L, r, luaL_checkinteger(L, 2))
}

unsafe fn b_rshift(L: *mut lua_State) -> i32 {
    let r = luaL_checkunsigned(L, 1);
    b_shift(L, r, luaL_checkinteger(L, 2).wrapping_neg())
}

unsafe fn b_arshift(L: *mut lua_State) -> i32 {
    let mut r = luaL_checkunsigned(L, 1);
    let i = luaL_checkinteger(L, 2);
    if i < 0 || (r & (1u32 << (NBITS - 1))) == 0 {
        b_shift(L, r, i.wrapping_neg())
    } else {
        r = if i >= NBITS { !0 } else { (r >> i) | !(!0u32 >> i) };
        lua_pushunsigned(L, r);
        1
    }
}

unsafe fn b_rot(L: *mut lua_State, i: i32) -> i32 {
    let r = luaL_checkunsigned(L, 1);
    lua_pushunsigned(L, r.rotate_left((i & (NBITS - 1)) as u32));
    1
}

unsafe fn b_lrot(L: *mut lua_State) -> i32 {
    let i = luaL_checkinteger(L, 2);
    b_rot(L, i)
}

unsafe fn b_rrot(L: *mut lua_State) -> i32 {
    let i = luaL_checkinteger(L, 2);
    b_rot(L, i.wrapping_neg())
}

unsafe fn fieldargs(L: *mut lua_State, farg: i32) -> (i32, i32) {
    let f = luaL_checkinteger(L, farg);
    let w = luaL_optinteger(L, farg + 1, 1);
    luaL_argcheck(L, 0 <= f, farg, "field cannot be negative");
    luaL_argcheck(L, 0 < w, farg + 1, "width must be positive");
    if f.wrapping_add(w) > NBITS || f.checked_add(w).is_none() {
        luaL_error(L, "trying to access non-existent bits");
    }
    (f, w)
}

unsafe fn b_extract(L: *mut lua_State) -> i32 {
    let r = luaL_checkunsigned(L, 1);
    let (f, w) = fieldargs(L, 2);
    lua_pushunsigned(L, (r >> f) & mask(w));
    1
}

unsafe fn b_replace(L: *mut lua_State) -> i32 {
    let r = luaL_checkunsigned(L, 1);
    let mut v = luaL_checkunsigned(L, 2);
    let (f, w) = fieldargs(L, 3);
    let m = mask(w);
    v &= m;
    lua_pushunsigned(L, (r & !(m << f)) | (v << f));
    1
}

unsafe fn b_countlz(L: *mut lua_State) -> i32 {
    let v = luaL_checkunsigned(L, 1);
    lua_pushunsigned(L, v.leading_zeros());
    1
}

unsafe fn b_countrz(L: *mut lua_State) -> i32 {
    let v = luaL_checkunsigned(L, 1);
    lua_pushunsigned(L, v.trailing_zeros());
    1
}

unsafe fn b_swap(L: *mut lua_State) -> i32 {
    let n = luaL_checkunsigned(L, 1);
    lua_pushunsigned(L, n.swap_bytes());
    1
}

const BITLIB: &[luaL_Reg] = &[
    ("arshift", b_arshift),
    ("band", b_and),
    ("bnot", b_not),
    ("bor", b_or),
    ("bxor", b_xor),
    ("btest", b_test),
    ("extract", b_extract),
    ("lrotate", b_lrot),
    ("lshift", b_lshift),
    ("replace", b_replace),
    ("rrotate", b_rrot),
    ("rshift", b_rshift),
    ("countlz", b_countlz),
    ("countrz", b_countrz),
    ("byteswap", b_swap),
];

pub unsafe fn luaopen_bit32(L: *mut lua_State) -> i32 {
    luaL_register(L, Some("bit32"), BITLIB);
    1
}
