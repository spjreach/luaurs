//! Port of `VM/src/lintlib.cpp` (the `integer` library) and
//! `VM/src/lclasslib.cpp` (the `class` library).

use crate::lapi::*;
use crate::laux::*;
use crate::lbaselib::luaL_argcheck;
use crate::lclass::luaR_isinstance;
use crate::lobject::*;

#[inline(always)]
fn mask64(w: i64) -> u64 {
    u64::MAX >> (64 - w)
}

unsafe fn int64_create(L: *mut lua_State) -> i32 {
    let x = luaL_checknumber(L, 1);
    if x >= -9223372036854775808.0 && x < 9223372036854775808.0 {
        let l = x as i64;
        if l as f64 == x {
            lua_pushinteger64(L, l);
            return 1;
        }
    }
    lua_pushnil(L);
    1
}

unsafe fn int64_fromstring(L: *mut lua_State) -> i32 {
    let s = luaL_checkstring(L, 1);
    let base = luaL_optinteger(L, 2, 10);
    luaL_argcheck(L, (2..=36).contains(&base), 2, "base out of range");
    match luaO_str2l(s, base as u32) {
        Some(r) => lua_pushinteger64(L, r),
        None => lua_pushnil(L),
    }
    1
}

unsafe fn int64_tonumber(L: *mut lua_State) -> i32 {
    let x = luaL_checkinteger64(L, 1);
    lua_pushnumber(L, x as f64);
    1
}

unsafe fn un(L: *mut lua_State, f: fn(i64) -> i64) -> i32 {
    let x = luaL_checkinteger64(L, 1);
    lua_pushinteger64(L, f(x));
    1
}

unsafe fn bin(L: *mut lua_State, f: fn(i64, i64) -> i64) -> i32 {
    let x = luaL_checkinteger64(L, 1);
    let y = luaL_checkinteger64(L, 2);
    lua_pushinteger64(L, f(x, y));
    1
}

unsafe fn cmp(L: *mut lua_State, f: fn(i64, i64) -> bool) -> i32 {
    let x = luaL_checkinteger64(L, 1);
    let y = luaL_checkinteger64(L, 2);
    lua_pushboolean(L, f(x, y) as i32);
    1
}

unsafe fn fold(L: *mut lua_State, init: i64, f: fn(i64, i64) -> i64) -> i64 {
    let n = lua_gettop(L);
    let mut r = init;
    for i in 1..=n {
        r = f(r, luaL_checkinteger64(L, i));
    }
    r
}

unsafe fn int64_neg(L: *mut lua_State) -> i32 {
    un(L, |x| x.wrapping_neg())
}
unsafe fn int64_add(L: *mut lua_State) -> i32 {
    bin(L, i64::wrapping_add)
}
unsafe fn int64_sub(L: *mut lua_State) -> i32 {
    bin(L, i64::wrapping_sub)
}
unsafe fn int64_mul(L: *mut lua_State) -> i32 {
    bin(L, i64::wrapping_mul)
}

unsafe fn divargs(L: *mut lua_State) -> (i64, i64) {
    let a = luaL_checkinteger64(L, 1);
    let b = luaL_checkinteger64(L, 2);
    if b == 0 {
        luaL_error(L, "division by zero");
    }
    (a, b)
}

unsafe fn int64_div(L: *mut lua_State) -> i32 {
    let (a, b) = divargs(L);
    if a == i64::MIN && b == -1 {
        luaL_error(L, "integer overflow");
    }
    lua_pushinteger64(L, a / b);
    1
}

unsafe fn int64_idiv(L: *mut lua_State) -> i32 {
    let (a, b) = divargs(L);
    if a == i64::MIN && b == -1 {
        luaL_error(L, "integer overflow");
    }
    let mut r = a / b;
    if (a ^ b) < 0 && a % b != 0 {
        r -= 1;
    }
    lua_pushinteger64(L, r);
    1
}

unsafe fn int64_rem(L: *mut lua_State) -> i32 {
    let (a, b) = divargs(L);
    lua_pushinteger64(L, a.wrapping_rem(b));
    1
}

unsafe fn int64_mod(L: *mut lua_State) -> i32 {
    let (a, b) = divargs(L);
    let mut r = 0;
    if a != i64::MIN || b != -1 {
        r = a % b;
        if r != 0 && ((a < 0) != (b < 0)) {
            r += b;
        }
    }
    lua_pushinteger64(L, r);
    1
}

unsafe fn int64_udiv(L: *mut lua_State) -> i32 {
    let (a, b) = divargs(L);
    lua_pushinteger64(L, ((a as u64) / (b as u64)) as i64);
    1
}

unsafe fn int64_urem(L: *mut lua_State) -> i32 {
    let (a, b) = divargs(L);
    lua_pushinteger64(L, ((a as u64) % (b as u64)) as i64);
    1
}

unsafe fn int64_min(L: *mut lua_State) -> i32 {
    let first = luaL_checkinteger64(L, 1);
    let n = lua_gettop(L);
    let mut r = first;
    for i in 2..=n {
        r = r.min(luaL_checkinteger64(L, i));
    }
    lua_pushinteger64(L, r);
    1
}

unsafe fn int64_max(L: *mut lua_State) -> i32 {
    let first = luaL_checkinteger64(L, 1);
    let n = lua_gettop(L);
    let mut r = first;
    for i in 2..=n {
        r = r.max(luaL_checkinteger64(L, i));
    }
    lua_pushinteger64(L, r);
    1
}

unsafe fn int64_band(L: *mut lua_State) -> i32 {
    let r = fold(L, -1, |a, b| a & b);
    lua_pushinteger64(L, r);
    1
}
unsafe fn int64_bor(L: *mut lua_State) -> i32 {
    let r = fold(L, 0, |a, b| a | b);
    lua_pushinteger64(L, r);
    1
}
unsafe fn int64_bxor(L: *mut lua_State) -> i32 {
    let r = fold(L, 0, |a, b| a ^ b);
    lua_pushinteger64(L, r);
    1
}
unsafe fn int64_btest(L: *mut lua_State) -> i32 {
    let r = fold(L, -1, |a, b| a & b);
    lua_pushboolean(L, (r != 0) as i32);
    1
}
unsafe fn int64_bnot(L: *mut lua_State) -> i32 {
    un(L, |x| !x)
}

unsafe fn int64_lt(L: *mut lua_State) -> i32 {
    cmp(L, |a, b| a < b)
}
unsafe fn int64_le(L: *mut lua_State) -> i32 {
    cmp(L, |a, b| a <= b)
}
unsafe fn int64_gt(L: *mut lua_State) -> i32 {
    cmp(L, |a, b| a > b)
}
unsafe fn int64_ge(L: *mut lua_State) -> i32 {
    cmp(L, |a, b| a >= b)
}
unsafe fn int64_ult(L: *mut lua_State) -> i32 {
    cmp(L, |a, b| (a as u64) < (b as u64))
}
unsafe fn int64_ule(L: *mut lua_State) -> i32 {
    cmp(L, |a, b| (a as u64) <= (b as u64))
}
unsafe fn int64_ugt(L: *mut lua_State) -> i32 {
    cmp(L, |a, b| (a as u64) > (b as u64))
}
unsafe fn int64_uge(L: *mut lua_State) -> i32 {
    cmp(L, |a, b| (a as u64) >= (b as u64))
}

unsafe fn int64_lshift(L: *mut lua_State) -> i32 {
    bin(L, |n, i| {
        let n = n as u64;
        if (-63..=63).contains(&i) {
            (if i < 0 { n >> (-i) } else { n << i }) as i64
        } else {
            0
        }
    })
}

unsafe fn int64_rshift(L: *mut lua_State) -> i32 {
    bin(L, |n, i| {
        let n = n as u64;
        if (-63..=63).contains(&i) {
            (if i < 0 { n << (-i) } else { n >> i }) as i64
        } else {
            0
        }
    })
}

unsafe fn int64_arshift(L: *mut lua_State) -> i32 {
    bin(L, |n, i| {
        if (-63..=63).contains(&i) {
            if i < 0 {
                ((n as u64) << (-i)) as i64
            } else {
                n >> i
            }
        } else if i < -63 {
            0
        } else if n < 0 {
            -1
        } else {
            0
        }
    })
}

unsafe fn int64_lrotate(L: *mut lua_State) -> i32 {
    bin(L, |n, s| (n as u64).rotate_left(((s as u64) % 64) as u32) as i64)
}

unsafe fn int64_rrotate(L: *mut lua_State) -> i32 {
    bin(L, |n, s| (n as u64).rotate_right(((s as u64) % 64) as u32) as i64)
}

unsafe fn int64_extract(L: *mut lua_State) -> i32 {
    let n = luaL_checkinteger64(L, 1);
    let f = luaL_checkinteger64(L, 2);
    let w = luaL_optinteger64(L, 3, 1);
    luaL_argcheck(L, 0 <= f, 2, "field cannot be negative");
    luaL_argcheck(L, 0 < w, 3, "width must be positive");
    if w > 64 - f {
        luaL_error(L, "trying to access non-existent bits");
    }
    lua_pushinteger64(L, (((n as u64) >> f) & mask64(w)) as i64);
    1
}

unsafe fn int64_replace(L: *mut lua_State) -> i32 {
    let n = luaL_checkinteger64(L, 1);
    let r = luaL_checkinteger64(L, 2);
    let f = luaL_checkinteger64(L, 3);
    let w = luaL_optinteger64(L, 4, 1);
    luaL_argcheck(L, 0 <= f, 3, "field cannot be negative");
    luaL_argcheck(L, 0 < w, 4, "width must be positive");
    if w > 64 - f {
        luaL_error(L, "trying to access non-existent bits");
    }
    let base = mask64(w);
    let repl = ((r as u64) & base) << f;
    let mask = u64::MAX ^ (base << f);
    lua_pushinteger64(L, (((n as u64) & mask) | repl) as i64);
    1
}

unsafe fn int64_clamp(L: *mut lua_State) -> i32 {
    let a = luaL_checkinteger64(L, 1);
    let mi = luaL_checkinteger64(L, 2);
    let mx = luaL_checkinteger64(L, 3);
    luaL_argcheck(L, mi <= mx, 3, "max must be greater than or equal to min");
    lua_pushinteger64(L, if a < mi { mi } else if a > mx { mx } else { a });
    1
}

unsafe fn int64_countrz(L: *mut lua_State) -> i32 {
    un(L, |n| n.trailing_zeros() as i64)
}
unsafe fn int64_countlz(L: *mut lua_State) -> i32 {
    un(L, |n| n.leading_zeros() as i64)
}
unsafe fn int64_bswap(L: *mut lua_State) -> i32 {
    un(L, i64::swap_bytes)
}

const INT64LIB: &[luaL_Reg] = &[
    ("create", int64_create),
    ("tonumber", int64_tonumber),
    ("neg", int64_neg),
    ("add", int64_add),
    ("sub", int64_sub),
    ("mul", int64_mul),
    ("div", int64_div),
    ("min", int64_min),
    ("max", int64_max),
    ("rem", int64_rem),
    ("idiv", int64_idiv),
    ("udiv", int64_udiv),
    ("urem", int64_urem),
    ("mod", int64_mod),
    ("clamp", int64_clamp),
    ("band", int64_band),
    ("bor", int64_bor),
    ("bnot", int64_bnot),
    ("bxor", int64_bxor),
    ("lt", int64_lt),
    ("le", int64_le),
    ("ult", int64_ult),
    ("ule", int64_ule),
    ("gt", int64_gt),
    ("ge", int64_ge),
    ("ugt", int64_ugt),
    ("uge", int64_uge),
    ("lshift", int64_lshift),
    ("rshift", int64_rshift),
    ("arshift", int64_arshift),
    ("lrotate", int64_lrotate),
    ("rrotate", int64_rrotate),
    ("extract", int64_extract),
    ("replace", int64_replace),
    ("btest", int64_btest),
    ("countrz", int64_countrz),
    ("countlz", int64_countlz),
    ("bswap", int64_bswap),
    ("fromstring", int64_fromstring),
];

pub unsafe fn luaopen_integer(L: *mut lua_State) -> i32 {
    luaL_register(L, Some("integer"), INT64LIB);
    lua_pushinteger64(L, i64::MAX);
    lua_setfield(L, -2, "maxsigned");
    lua_pushinteger64(L, i64::MIN);
    lua_setfield(L, -2, "minsigned");
    1
}

// ---- class library ----

unsafe fn class_isinstance(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    luaL_checktype(L, 2, LUA_TCLASS);
    let obj = luaA_toobject(L, 1);
    if !ttisobject(obj) {
        lua_pushboolean(L, 0);
        return 1;
    }
    let cls = luaA_toobject(L, 2);
    lua_pushboolean(L, luaR_isinstance(objectvalue(obj), classvalue(cls)) as i32);
    1
}

unsafe fn class_classof(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    let inst = luaA_toobject(L, 1);
    if !ttisobject(inst) {
        lua_pushnil(L);
        return 1;
    }
    luaA_pushclass(L, (*objectvalue(inst)).lclass);
    1
}

const CLASSLIB: &[luaL_Reg] = &[("isinstance", class_isinstance), ("classof", class_classof)];

pub unsafe fn luaopen_class(L: *mut lua_State) -> i32 {
    luaL_register(L, Some("class"), CLASSLIB);
    1
}
