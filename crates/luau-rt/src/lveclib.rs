//! Port of `VM/src/lveclib.cpp` (`LUA_VECTOR_SIZE == 3`, float components).

use crate::lapi::*;
use crate::laux::*;
use crate::lbaselib::luaL_argcheck;
use crate::lnumutils::{luai_clampf, luai_lerpf, luai_signf};
use crate::lobject::*;

unsafe fn vector_create(L: *mut lua_State) -> i32 {
    let count = lua_gettop(L);
    let x = luaL_checknumber(L, 1);
    let y = luaL_checknumber(L, 2);
    let z = if count >= 3 { luaL_checknumber(L, 3) } else { 0.0 };
    lua_pushvector(L, x as f32, y as f32, z as f32);
    1
}

unsafe fn vector_magnitude(L: *mut lua_State) -> i32 {
    let v = luaL_checkvector(L, 1);
    lua_pushnumber(L, (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt() as f64);
    1
}

unsafe fn vector_normalize(L: *mut lua_State) -> i32 {
    let v = luaL_checkvector(L, 1);
    let inv = 1.0f32 / (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    lua_pushvector(L, v[0] * inv, v[1] * inv, v[2] * inv);
    1
}

#[inline(always)]
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

unsafe fn vector_cross(L: *mut lua_State) -> i32 {
    let a = luaL_checkvector(L, 1);
    let b = luaL_checkvector(L, 2);
    let c = cross(a, b);
    lua_pushvector(L, c[0], c[1], c[2]);
    1
}

unsafe fn vector_dot(L: *mut lua_State) -> i32 {
    let a = luaL_checkvector(L, 1);
    let b = luaL_checkvector(L, 2);
    lua_pushnumber(L, (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]) as f64);
    1
}

unsafe fn vector_angle(L: *mut lua_State) -> i32 {
    let a = luaL_checkvector(L, 1);
    let b = luaL_checkvector(L, 2);
    let axis = if lua_isnoneornil(L, 3) { None } else { Some(luaL_checkvector(L, 3)) };
    let c = cross(a, b);
    let sin_a = (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt() as f64;
    let cos_a = (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]) as f64;
    let mut angle = crate::libm::atan2(sin_a, cos_a);
    if let Some(axis) = axis {
        if c[0] * axis[0] + c[1] * axis[1] + c[2] * axis[2] < 0.0 {
            angle = -angle;
        }
    }
    lua_pushnumber(L, angle);
    1
}

unsafe fn vmap(L: *mut lua_State, f: fn(f32) -> f32) -> i32 {
    let v = luaL_checkvector(L, 1);
    lua_pushvector(L, f(v[0]), f(v[1]), f(v[2]));
    1
}

unsafe fn vector_floor(L: *mut lua_State) -> i32 {
    vmap(L, f32::floor)
}

unsafe fn vector_ceil(L: *mut lua_State) -> i32 {
    vmap(L, f32::ceil)
}

unsafe fn vector_abs(L: *mut lua_State) -> i32 {
    vmap(L, f32::abs)
}

unsafe fn vector_sign(L: *mut lua_State) -> i32 {
    vmap(L, luai_signf)
}

unsafe fn vector_clamp(L: *mut lua_State) -> i32 {
    let v = luaL_checkvector(L, 1);
    let min = luaL_checkvector(L, 2);
    let max = luaL_checkvector(L, 3);
    luaL_argcheck(L, min[0] <= max[0], 3, "max.x must be greater than or equal to min.x");
    luaL_argcheck(L, min[1] <= max[1], 3, "max.y must be greater than or equal to min.y");
    luaL_argcheck(L, min[2] <= max[2], 3, "max.z must be greater than or equal to min.z");
    lua_pushvector(
        L,
        luai_clampf(v[0], min[0], max[0]),
        luai_clampf(v[1], min[1], max[1]),
        luai_clampf(v[2], min[2], max[2]),
    );
    1
}

unsafe fn vector_min(L: *mut lua_State) -> i32 {
    let n = lua_gettop(L);
    let mut r = luaL_checkvector(L, 1);
    for i in 2..=n {
        let b = luaL_checkvector(L, i);
        for k in 0..3 {
            if b[k] < r[k] {
                r[k] = b[k];
            }
        }
    }
    lua_pushvector(L, r[0], r[1], r[2]);
    1
}

unsafe fn vector_max(L: *mut lua_State) -> i32 {
    let n = lua_gettop(L);
    let mut r = luaL_checkvector(L, 1);
    for i in 2..=n {
        let b = luaL_checkvector(L, i);
        for k in 0..3 {
            if b[k] > r[k] {
                r[k] = b[k];
            }
        }
    }
    lua_pushvector(L, r[0], r[1], r[2]);
    1
}

unsafe fn vector_index(L: *mut lua_State) -> i32 {
    let v = luaL_checkvector(L, 1);
    let name = luaL_checklstring(L, 2);
    if name.len() == 1 {
        let ic = ((name[0] | b' ') as i32) - (b'x' as i32);
        if (ic as u32) < 3 {
            lua_pushnumber(L, v[ic as usize] as f64);
            return 1;
        }
    }
    let nm = match name.iter().position(|&c| c == 0) {
        Some(z) => &name[..z],
        None => name,
    };
    let mut msg = b"attempt to index vector with '".to_vec();
    msg.extend_from_slice(nm);
    msg.push(b'\'');
    luaL_error_bytes(L, &msg)
}

unsafe fn vector_lerp(L: *mut lua_State) -> i32 {
    let a = luaL_checkvector(L, 1);
    let b = luaL_checkvector(L, 2);
    let t = luaL_checknumber(L, 3) as f32;
    lua_pushvector(L, luai_lerpf(a[0], b[0], t), luai_lerpf(a[1], b[1], t), luai_lerpf(a[2], b[2], t));
    1
}

const VECTORLIB: &[luaL_Reg] = &[
    ("create", vector_create),
    ("magnitude", vector_magnitude),
    ("normalize", vector_normalize),
    ("cross", vector_cross),
    ("dot", vector_dot),
    ("angle", vector_angle),
    ("floor", vector_floor),
    ("ceil", vector_ceil),
    ("abs", vector_abs),
    ("sign", vector_sign),
    ("clamp", vector_clamp),
    ("max", vector_max),
    ("min", vector_min),
    ("lerp", vector_lerp),
];

unsafe fn createmetatable(L: *mut lua_State) {
    lua_createtable(L, 0, 1);
    lua_pushvector(L, 0.0, 0.0, 0.0);
    lua_pushvalue(L, -2);
    lua_setmetatable(L, -2);
    lua_pop(L, 1);
    lua_pushcfunction(L, vector_index, None);
    lua_setfield(L, -2, "__index");
    lua_setreadonly(L, -1, 1);
    lua_pop(L, 1);
}

pub unsafe fn luaopen_vector(L: *mut lua_State) -> i32 {
    luaL_register(L, Some("vector"), VECTORLIB);
    lua_pushvector(L, 0.0, 0.0, 0.0);
    lua_setfield(L, -2, "zero");
    lua_pushvector(L, 1.0, 1.0, 1.0);
    lua_setfield(L, -2, "one");
    createmetatable(L);
    1
}
