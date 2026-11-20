//! Port of the conformance runner in `tests/Conformance.test.cpp`
//! (`runConformance` plus the per-test setup/yield callbacks and the
//! post-run API checks that exercise the VM).
//!
//! Selected when the script lives in a `conformance` directory (or
//! `LUAU_HARNESS=1`). The test passes when the chunk returns `"OK"` and every
//! harness `CHECK` holds.

#![allow(non_snake_case, static_mut_refs)]

use std::cell::RefCell;

use luau_rt::lapi::*;
use luau_rt::laux::*;
use luau_rt::lbaselib::{flush_stdout, luaL_argexpected};
use luau_rt::ldebug::*;
use luau_rt::ldo::*;
use luau_rt::linit::*;
use luau_rt::lobject::*;
use luau_rt::lstate::*;
use luau_rt::lvmexecute::luau_callhook;
use luau_rt::lvmload::luau_load;

use luau_compiler::CompileOptions;

thread_local! {
    static FAILURES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

macro_rules! check {
    ($cond:expr) => {
        if !($cond) {
            FAILURES.with(|f| f.borrow_mut().push(format!("CHECK failed (line {}): {}", line!(), stringify!($cond))));
        }
    };
}

fn take_failures() -> Vec<String> {
    FAILURES.with(|f| std::mem::take(&mut *f.borrow_mut()))
}

const MIB8: usize = 8 * 1024 * 1024;

unsafe fn setglobalfn(L: *mut lua_State, name: &'static str, f: lua_CFunction) {
    lua_pushcfunction(L, f, Some(name));
    lua_setglobal(L, name);
}

unsafe fn setglobalk(L: *mut lua_State, name: &'static str, f: lua_CFunction, k: lua_Continuation) {
    lua_pushcclosurek(L, f, Some(name), 0, Some(k));
    lua_setglobal(L, name);
}

fn bytes_eq(a: Option<&[u8]>, b: &str) -> bool {
    a == Some(b.as_bytes())
}

// ---------------------------------------------------------------------------
// globals registered for every test
// ---------------------------------------------------------------------------

unsafe fn lua_collectgarbage(L: *mut lua_State) -> i32 {
    const OPTS: [&str; 9] = ["stop", "restart", "collect", "count", "isrunning", "step", "setgoal", "setstepmul", "setstepsize"];
    const OPTSNUM: [i32; 9] = [
        LUA_GCSTOP,
        LUA_GCRESTART,
        LUA_GCCOLLECT,
        LUA_GCCOUNT,
        LUA_GCISRUNNING,
        LUA_GCSTEP,
        LUA_GCSETGOAL,
        LUA_GCSETSTEPMUL,
        LUA_GCSETSTEPSIZE,
    ];
    let o = luaL_checkoption(L, 1, Some("collect"), &OPTS) as usize;
    let ex = luaL_optinteger(L, 2, 0);
    let res = lua_gc(L, OPTSNUM[o], ex);
    match OPTSNUM[o] {
        LUA_GCSTEP | LUA_GCISRUNNING => lua_pushboolean(L, res),
        _ => lua_pushnumber(L, res as f64),
    }
    1
}

unsafe fn lua_loadstring(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1).to_vec();
    let chunkname = if lua_isnoneornil(L, 2) { s.clone() } else { luaL_checklstring(L, 2).to_vec() };
    lua_setsafeenv(L, LUA_ENVIRONINDEX, 0);
    // luau_compile with null options: O1, debug level 1
    let opts = CompileOptions { classes: unsafe { luau_rt::lvmutils::CLASSES_RUNTIME }, ..Default::default() };
    let bytecode = luau_compiler::compile_source(&s, opts);
    if luau_load(L, &chunkname, &bytecode, 0) == 0 {
        return 1;
    }
    lua_pushnil(L);
    lua_insert(L, -2);
    2
}

unsafe fn lua_silence(_L: *mut lua_State) -> i32 {
    0
}

// ---------------------------------------------------------------------------
// native helpers (no native code generation in this port)
// ---------------------------------------------------------------------------

unsafe fn setup_native_helpers(L: *mut lua_State) {
    unsafe fn is_native(L: *mut lua_State) -> i32 {
        lua_pushboolean(L, luaG_isnative(L, 1));
        1
    }
    unsafe fn is_native_if_supported(L: *mut lua_State) -> i32 {
        lua_pushboolean(L, 1);
        1
    }
    setglobalfn(L, "is_native", is_native);
    setglobalfn(L, "is_native_if_supported", is_native_if_supported);
}

// ---------------------------------------------------------------------------
// vector helpers
// ---------------------------------------------------------------------------

unsafe fn lua_vector_dot(L: *mut lua_State) -> i32 {
    let a = luaL_checkvector(L, 1);
    let b = luaL_checkvector(L, 2);
    lua_pushnumber(L, (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]) as f64);
    1
}

unsafe fn lua_vector_cross(L: *mut lua_State) -> i32 {
    let a = luaL_checkvector(L, 1);
    let b = luaL_checkvector(L, 2);
    lua_pushvector(L, a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]);
    1
}

fn cstr(s: &[u8]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf8_lossy(&s[..end]).into_owned()
}

unsafe fn lua_vector_index(L: *mut lua_State) -> i32 {
    let v = luaL_checkvector(L, 1);
    let name = cstr(luaL_checkstring(L, 2));
    match name.as_str() {
        "Magnitude" => {
            lua_pushnumber(L, (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt() as f64);
            1
        }
        "Unit" => {
            let inv = 1.0f32 / (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            lua_pushvector(L, v[0] * inv, v[1] * inv, v[2] * inv);
            1
        }
        "Dot" => {
            lua_pushcfunction(L, lua_vector_dot, Some("Dot"));
            1
        }
        _ => luaL_error(L, &format!("{} is not a valid member of vector", name)),
    }
}

unsafe fn lua_vector_namecall(L: *mut lua_State) -> i32 {
    if let Some(s) = lua_namecallatom(L, None) {
        if s == b"Dot" {
            return lua_vector_dot(L);
        }
        if s == b"Cross" {
            return lua_vector_cross(L);
        }
    }
    let n = cstr(luaL_checkstring(L, 1));
    luaL_error(L, &format!("{} is not a valid method of vector", n))
}

unsafe fn setup_vector_helpers(L: *mut lua_State) {
    lua_pushvector(L, 0.0, 0.0, 0.0);
    luaL_newmetatable(L, "vector");
    lua_pushstring(L, "__index");
    lua_pushcfunction(L, lua_vector_index, None);
    lua_settable(L, -3);
    lua_pushstring(L, "__namecall");
    lua_pushcfunction(L, lua_vector_namecall, None);
    lua_settable(L, -3);
    lua_setreadonly(L, -1, 1);
    lua_setmetatable(L, -2);
    lua_pop(L, 1);
}

// ---------------------------------------------------------------------------
// userdata helpers (vec2 / vertex)
// ---------------------------------------------------------------------------

const K_TAG_VEC2: i32 = 12;
const K_TAG_VERTEX: i32 = 13;

#[repr(C)]
#[derive(Clone, Copy)]
struct Vec2 {
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
}

unsafe fn vec2_push(L: *mut lua_State) -> *mut Vec2 {
    let d = lua_newuserdatatagged(L, std::mem::size_of::<Vec2>(), K_TAG_VEC2) as *mut Vec2;
    lua_getuserdatametatable(L, K_TAG_VEC2);
    lua_setmetatable(L, -2);
    d
}

unsafe fn vec2_get(L: *mut lua_State, idx: i32) -> *mut Vec2 {
    luaL_checkudatatagged(L, idx, K_TAG_VEC2) as *mut Vec2
}

unsafe fn lua_vec2(L: *mut lua_State) -> i32 {
    let x = luaL_checknumber(L, 1);
    let y = luaL_checknumber(L, 2);
    let d = vec2_push(L);
    (*d).x = x as f32;
    (*d).y = y as f32;
    1
}

unsafe fn vec2_dot(L: *mut lua_State, s: *mut Vec2) -> i32 {
    let b = vec2_get(L, 2);
    lua_pushnumber(L, ((*s).x * (*b).x + (*s).y * (*b).y) as f64);
    1
}

unsafe fn vec2_min(L: *mut lua_State, s: *mut Vec2) -> i32 {
    let b = vec2_get(L, 2);
    let d = vec2_push(L);
    (*d).x = if (*s).x < (*b).x { (*s).x } else { (*b).x };
    (*d).y = if (*s).y < (*b).y { (*s).y } else { (*b).y };
    1
}

unsafe fn vec2_clone(L: *mut lua_State, s: *mut Vec2) -> i32 {
    let r = vec2_push(L);
    *r = *s;
    1
}

unsafe fn vec2_reenter(L: *mut lua_State, s: *mut Vec2) -> i32 {
    lua_getglobal(L, "reenterCallback");
    check!(lua_isfunction(L, -1));
    lua_pcall(L, 0, 0, 0);
    lua_pushnumber(L, ((*s).x + (*s).y) as f64);
    1
}

unsafe fn lua_vec2_index(L: *mut lua_State) -> i32 {
    let v = vec2_get(L, 1);
    let name = cstr(luaL_checkstring(L, 2));
    match name.as_str() {
        "X" => lua_pushnumber(L, (*v).x as f64),
        "Y" => lua_pushnumber(L, (*v).y as f64),
        "Magnitude" => lua_pushnumber(L, ((*v).x * (*v).x + (*v).y * (*v).y).sqrt() as f64),
        "Unit" => {
            let inv = 1.0f32 / ((*v).x * (*v).x + (*v).y * (*v).y).sqrt();
            let d = vec2_push(L);
            (*d).x = (*v).x * inv;
            (*d).y = (*v).y * inv;
        }
        "sizeof" => lua_pushnumber(L, std::mem::size_of::<Vec2>() as f64),
        _ => luaL_error(L, &format!("{} is not a valid member of vector", name)),
    }
    1
}

unsafe fn lua_vec2_newindex(L: *mut lua_State) -> i32 {
    let v = vec2_get(L, 1);
    let name = cstr(luaL_checkstring(L, 2));
    let value = luaL_checknumber(L, 3);
    match name.as_str() {
        "X" => (*v).x = value as f32,
        "Y" => (*v).y = value as f32,
        _ => luaL_error(L, &format!("{} is not a writable member of vec2", name)),
    }
    0
}

unsafe fn lua_vec2_namecall(L: *mut lua_State) -> i32 {
    if let Some(s) = lua_namecallatom(L, None) {
        let s = s.to_vec();
        let this = vec2_get(L, 1);
        match s.as_slice() {
            b"Dot" => return vec2_dot(L, this),
            b"Min" => return vec2_min(L, this),
            b"Clone" => return vec2_clone(L, this),
            b"Reenter" => return vec2_reenter(L, this),
            _ => {}
        }
    }
    let n = cstr(luaL_checkstring(L, 1));
    luaL_error(L, &format!("{} is not a valid method of vector", n))
}

unsafe fn vec2_binop(L: *mut lua_State, f: fn(f32, f32) -> f32) -> i32 {
    let a = vec2_get(L, 1);
    let b = vec2_get(L, 2);
    let d = vec2_push(L);
    (*d).x = f((*a).x, (*b).x);
    (*d).y = f((*a).y, (*b).y);
    1
}

unsafe fn vec2_add(L: *mut lua_State) -> i32 {
    vec2_binop(L, |a, b| a + b)
}
unsafe fn vec2_sub(L: *mut lua_State) -> i32 {
    vec2_binop(L, |a, b| a - b)
}
unsafe fn vec2_mul(L: *mut lua_State) -> i32 {
    vec2_binop(L, |a, b| a * b)
}
unsafe fn vec2_div(L: *mut lua_State) -> i32 {
    vec2_binop(L, |a, b| a / b)
}
unsafe fn vec2_unm(L: *mut lua_State) -> i32 {
    let a = vec2_get(L, 1);
    let d = vec2_push(L);
    (*d).x = -(*a).x;
    (*d).y = -(*a).y;
    1
}

unsafe fn vertex_push(L: *mut lua_State) -> *mut Vertex {
    let d = lua_newuserdatatagged(L, std::mem::size_of::<Vertex>(), K_TAG_VERTEX) as *mut Vertex;
    lua_getuserdatametatable(L, K_TAG_VERTEX);
    lua_setmetatable(L, -2);
    d
}

unsafe fn vertex_get(L: *mut lua_State, idx: i32) -> *mut Vertex {
    let a = lua_touserdatatagged(L, idx, K_TAG_VERTEX) as *mut Vertex;
    if !a.is_null() {
        return a;
    }
    luaL_typeerror(L, idx, "vertex")
}

unsafe fn lua_vertex(L: *mut lua_State) -> i32 {
    let pos = luaL_checkvector(L, 1);
    let normal = luaL_checkvector(L, 2);
    let uv = vec2_get(L, 3);
    let d = vertex_push(L);
    (*d).pos = pos;
    (*d).normal = normal;
    (*d).uv = [(*uv).x, (*uv).y];
    1
}

unsafe fn lua_vertex_index(L: *mut lua_State) -> i32 {
    let v = vertex_get(L, 1);
    let name = cstr(luaL_checkstring(L, 2));
    match name.as_str() {
        "pos" => lua_pushvector(L, (*v).pos[0], (*v).pos[1], (*v).pos[2]),
        "normal" => lua_pushvector(L, (*v).normal[0], (*v).normal[1], (*v).normal[2]),
        "uv" => {
            let uv = vec2_push(L);
            (*uv).x = (*v).uv[0];
            (*uv).y = (*v).uv[1];
        }
        "sizeof" => lua_pushnumber(L, std::mem::size_of::<Vertex>() as f64),
        _ => luaL_error(L, &format!("{} is not a valid member of vertex", name)),
    }
    1
}

unsafe fn lua_vertex_newindex(L: *mut lua_State) -> i32 {
    let v = vertex_get(L, 1);
    let name = cstr(luaL_checkstring(L, 2));
    match name.as_str() {
        "pos" => (*v).pos = luaL_checkvector(L, 3),
        "normal" => (*v).normal = luaL_checkvector(L, 3),
        "uv" => {
            let uv = vec2_get(L, 3);
            (*v).uv = [(*uv).x, (*uv).y];
        }
        _ => luaL_error(L, &format!("{} is not a writable member of vertex", name)),
    }
    0
}

unsafe fn lua_vertex_namecall(L: *mut lua_State) -> i32 {
    if let Some(s) = lua_namecallatom(L, None) {
        if s == b"Clone" {
            let this = vertex_get(L, 1);
            let r = vertex_push(L);
            *r = *this;
            return 1;
        }
    }
    let n = cstr(luaL_checkstring(L, 1));
    luaL_error(L, &format!("{} is not a valid method of vertex", n))
}

unsafe fn setup_userdata_helpers(L: *mut lua_State) {
    lua_newtable(L);
    lua_pushvalue(L, -1);
    lua_setuserdatametatable(L, K_TAG_VEC2);
    lua_pushstring(L, "vec2");
    lua_setfield(L, -2, "__type");
    let mms: [(&str, lua_CFunction); 8] = [
        ("__index", lua_vec2_index),
        ("__newindex", lua_vec2_newindex),
        ("__namecall", lua_vec2_namecall),
        ("__add", vec2_add),
        ("__sub", vec2_sub),
        ("__mul", vec2_mul),
        ("__div", vec2_div),
        ("__unm", vec2_unm),
    ];
    for (k, f) in mms {
        lua_pushcfunction(L, f, None);
        lua_setfield(L, -2, k);
    }
    lua_setreadonly(L, -1, 1);
    lua_pushcfunction(L, lua_vec2, Some("vec2"));
    lua_setglobal(L, "vec2");
    lua_pop(L, 1);

    lua_newtable(L);
    lua_pushvalue(L, -1);
    lua_setuserdatametatable(L, K_TAG_VERTEX);
    lua_pushstring(L, "vertex");
    lua_setfield(L, -2, "__type");
    lua_pushcfunction(L, lua_vertex_index, None);
    lua_setfield(L, -2, "__index");
    lua_pushcfunction(L, lua_vertex_newindex, None);
    lua_setfield(L, -2, "__newindex");
    lua_pushcfunction(L, lua_vertex_namecall, None);
    lua_setfield(L, -2, "__namecall");
    lua_setreadonly(L, -1, 1);
    lua_pushcfunction(L, lua_vertex, Some("vertex"));
    lua_setglobal(L, "vertex");
    lua_pop(L, 1);

    lua_gc(L, LUA_GCCOLLECT, 0);
}

// ---------------------------------------------------------------------------
// int64 userdata (userdata.luau)
// ---------------------------------------------------------------------------

const K_INT64_TAG: i32 = 1;

unsafe fn get_int64(L: *mut lua_State, idx: i32) -> i64 {
    let p = lua_touserdatatagged(L, idx, K_INT64_TAG);
    if !p.is_null() {
        return *(p as *mut i64);
    }
    if lua_isnumber(L, idx) != 0 {
        return lua_tointeger(L, idx) as i64;
    }
    luaL_typeerror(L, 1, "int64")
}

unsafe fn push_int64(L: *mut lua_State, v: i64) {
    let p = lua_newuserdatatagged(L, 8, K_INT64_TAG) as *mut i64;
    luaL_getmetatable(L, "int64");
    lua_setmetatable(L, -2);
    *p = v;
}

unsafe fn i64_index(L: *mut lua_State) -> i32 {
    let p = luaL_checkudatatagged(L, 1, K_INT64_TAG) as *mut i64;
    let name = cstr(luaL_checkstring(L, 2));
    if name == "value" {
        lua_pushnumber(L, *p as f64);
        return 1;
    }
    luaL_error(L, &format!("unknown field {}", name))
}

unsafe fn i64_newindex(L: *mut lua_State) -> i32 {
    let p = luaL_checkudatatagged(L, 1, K_INT64_TAG) as *mut i64;
    let name = cstr(luaL_checkstring(L, 2));
    if name == "value" {
        *p = luaL_checknumber(L, 3) as i64;
        return 0;
    }
    luaL_error(L, &format!("unknown field {}", name))
}

unsafe fn i64_eq(L: *mut lua_State) -> i32 {
    let r = get_int64(L, 1) == get_int64(L, 2);
    lua_pushboolean(L, r as i32);
    1
}
unsafe fn i64_lt(L: *mut lua_State) -> i32 {
    let r = get_int64(L, 1) < get_int64(L, 2);
    lua_pushboolean(L, r as i32);
    1
}
unsafe fn i64_le(L: *mut lua_State) -> i32 {
    let r = get_int64(L, 1) <= get_int64(L, 2);
    lua_pushboolean(L, r as i32);
    1
}
unsafe fn i64_bin(L: *mut lua_State, f: fn(i64, i64) -> i64) -> i32 {
    let a = get_int64(L, 1);
    let b = get_int64(L, 2);
    push_int64(L, f(a, b));
    1
}
unsafe fn i64_add(L: *mut lua_State) -> i32 {
    i64_bin(L, i64::wrapping_add)
}
unsafe fn i64_sub(L: *mut lua_State) -> i32 {
    i64_bin(L, i64::wrapping_sub)
}
unsafe fn i64_mul(L: *mut lua_State) -> i32 {
    i64_bin(L, i64::wrapping_mul)
}
unsafe fn i64_div(L: *mut lua_State) -> i32 {
    i64_bin(L, |a, b| a.checked_div(b).unwrap_or(0))
}
unsafe fn i64_idiv(L: *mut lua_State) -> i32 {
    i64_bin(L, |a, b| (a as f64 / b as f64).floor() as i64)
}
unsafe fn i64_mod(L: *mut lua_State) -> i32 {
    i64_bin(L, |a, b| a.checked_rem(b).unwrap_or(0))
}
unsafe fn i64_pow(L: *mut lua_State) -> i32 {
    i64_bin(L, |a, b| (a as f64).powf(b as f64) as i64)
}
unsafe fn i64_unm(L: *mut lua_State) -> i32 {
    let a = get_int64(L, 1);
    push_int64(L, a.wrapping_neg());
    1
}
unsafe fn i64_tostring(L: *mut lua_State) -> i32 {
    let v = get_int64(L, 1);
    lua_pushstring(L, &v.to_string());
    1
}
unsafe fn i64_ctor(L: *mut lua_State) -> i32 {
    let v = luaL_checknumber(L, 1);
    push_int64(L, v as i64);
    1
}

unsafe fn setup_int64(L: *mut lua_State) {
    luaL_newmetatable(L, "int64");
    lua_pushstring(L, "int64");
    lua_setfield(L, -2, "__type");
    let mms: [(&str, lua_CFunction); 14] = [
        ("__index", i64_index),
        ("__newindex", i64_newindex),
        ("__eq", i64_eq),
        ("__lt", i64_lt),
        ("__le", i64_le),
        ("__add", i64_add),
        ("__sub", i64_sub),
        ("__mul", i64_mul),
        ("__div", i64_div),
        ("__idiv", i64_idiv),
        ("__mod", i64_mod),
        ("__pow", i64_pow),
        ("__unm", i64_unm),
        ("__tostring", i64_tostring),
    ];
    for (k, f) in mms {
        lua_pushcfunction(L, f, None);
        lua_setfield(L, -2, k);
    }
    setglobalfn(L, "int64", i64_ctor);
}

// ---------------------------------------------------------------------------
// tables / gc / pcall / coroutinehost / iter
// ---------------------------------------------------------------------------

unsafe fn makelud(L: *mut lua_State) -> i32 {
    if lua_type(L, 1) == LUA_TNUMBER {
        let v = luaL_checkunsigned(L, 1);
        lua_pushlightuserdata(L, v as usize as *mut u8);
    } else {
        let p = lua_topointer(L, 1);
        lua_pushlightuserdata(L, p as *mut u8);
    }
    1
}

unsafe fn setallocationlimit(L: *mut lua_State) -> i32 {
    let v = luaL_checkunsigned(L, 1) as usize;
    (*(*L).global).memlimit = if v == 0 { MIB8 } else { v };
    0
}

unsafe fn setblockallocations(L: *mut lua_State) -> i32 {
    (*(*L).global).allocblocked = luaL_checkboolean(L, 1) != 0;
    0
}

unsafe fn cxxthrow(L: *mut lua_State) -> i32 {
    luaL_error(L, "oops")
}

unsafe fn resumeerror(L: *mut lua_State) -> i32 {
    let co = lua_tothread(L, 1);
    lua_xmove(L, co, 1);
    lua_resumeerror(co, L);
    0
}

unsafe fn run_finalizers(L: *mut lua_State, toclose: bool) -> i32 {
    let co = lua_tothread(L, 1);
    let NL = lua_newthread(L);
    lua_pushfinalizerfunction(NL, co, toclose as i32);
    let status = lua_resume(NL, std::ptr::null_mut(), 0);
    let results = if status == LUA_OK { lua_gettop(NL) } else { 1 };
    lua_pushboolean(L, (status == LUA_OK) as i32);
    lua_xmove(NL, L, results);
    results + 1
}

unsafe fn hostresume(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTHREAD);
    let co = lua_tothread(L, 1);
    let status = lua_resume(co, std::ptr::null_mut(), 0);
    if status == LUA_YIELD || status == LUA_BREAK {
        return 0;
    }
    if lua_hasfinalizers(co) != 0 {
        return run_finalizers(L, false);
    }
    0
}

unsafe fn hostcancel(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTHREAD);
    let co = lua_tothread(L, 1);
    if lua_hasfinalizers(co) != 0 {
        return run_finalizers(L, true);
    }
    lua_resetthread(co);
    lua_pushboolean(L, 1);
    1
}

unsafe fn c_yielding_iterator_cont(L: *mut lua_State, _status: i32) -> i32 {
    let index = luaL_checkinteger(L, 2);
    lua_pushinteger(L, index + 1);
    lua_pushinteger(L, index + 1);
    2
}

unsafe fn c_yielding_iterator(L: *mut lua_State) -> i32 {
    let max = luaL_checkinteger(L, 1);
    let index = luaL_checkinteger(L, 2);
    if index >= max {
        return 0;
    }
    lua_pushinteger(L, index + 1);
    lua_yield(L, 1)
}

// ---------------------------------------------------------------------------
// cyield
// ---------------------------------------------------------------------------

unsafe fn single_yield(L: *mut lua_State) -> i32 {
    lua_pushnumber(L, 2.0);
    lua_yield(L, 1)
}
unsafe fn single_yield_cont(L: *mut lua_State, _s: i32) -> i32 {
    lua_pushnumber(L, 4.0);
    1
}

unsafe fn multiple_yields(L: *mut lua_State) -> i32 {
    lua_settop(L, 1);
    let base = luaL_checkinteger(L, 1);
    let pos = 1;
    lua_pushinteger(L, pos);
    lua_pushinteger(L, base + pos);
    lua_yield(L, 1)
}
unsafe fn multiple_yields_cont(L: *mut lua_State, _s: i32) -> i32 {
    let base = luaL_checkinteger(L, 1);
    let pos = luaL_checkinteger(L, 2) + 1;
    lua_pushinteger(L, pos);
    lua_replace(L, 2);
    lua_pushinteger(L, base + pos);
    if pos < 4 {
        lua_yield(L, 1)
    } else {
        1
    }
}

unsafe fn nested_helper(L: *mut lua_State) -> i32 {
    let context = luaL_checkinteger(L, lua_upvalueindex(1));
    lua_pushinteger(L, 100 + context);
    lua_yield(L, 1)
}
unsafe fn nested_helper_cont(L: *mut lua_State, _s: i32) -> i32 {
    let context = luaL_checkinteger(L, lua_upvalueindex(1));
    lua_pushinteger(L, 110 + context);
    1
}
unsafe fn nested_helper_nonyielding(L: *mut lua_State) -> i32 {
    let context = luaL_checkinteger(L, lua_upvalueindex(1));
    lua_pushinteger(L, 105 + context);
    1
}

unsafe fn multiple_yields_nested(L: *mut lua_State) -> i32 {
    lua_settop(L, 2);
    let nested_should_yield = luaL_checkboolean(L, 2) != 0;
    lua_pushinteger(L, 0);
    lua_pushnumber(L, 5.0);
    if nested_should_yield {
        lua_pushcclosurek(L, nested_helper, None, 1, Some(nested_helper_cont));
    } else {
        lua_pushcclosurek(L, nested_helper_nonyielding, None, 1, None);
    }
    lua_callyieldable(L, 0, 1)
}
unsafe fn multiple_yields_nested_cont(L: *mut lua_State, _s: i32) -> i32 {
    let state = luaL_checkinteger(L, 3);
    lua_pushinteger(L, state + 1);
    lua_replace(L, 3);
    if state == 0 {
        lua_yield(L, lua_gettop(L) - 3)
    } else if state == 1 {
        lua_pushnumber(L, (luaL_checkinteger(L, 1) + 200) as f64);
        lua_yield(L, 1)
    } else {
        lua_pushnumber(L, (luaL_checkinteger(L, 1) + 210) as f64);
        1
    }
}

unsafe fn passthrough_call(L: *mut lua_State) -> i32 {
    lua_pushvalue(L, 1);
    lua_pushvalue(L, 2);
    lua_pushvalue(L, 3);
    lua_callyieldable(L, 2, 1)
}
unsafe fn passthrough_call_cont(L: *mut lua_State, _s: i32) -> i32 {
    check!(lua_gettop(L) == 4);
    check!(lua_tonumber(L, -1) == 0.5);
    1
}
unsafe fn passthrough_more(L: *mut lua_State) -> i32 {
    lua_pushvalue(L, 1);
    lua_pushvalue(L, 2);
    lua_pushvalue(L, 3);
    lua_callyieldable(L, 2, 10)
}
unsafe fn passthrough_more_cont(L: *mut lua_State, _s: i32) -> i32 {
    check!(lua_gettop(L) == 13);
    for _ in 0..9 {
        check!(lua_isnil(L, -1));
        lua_pop(L, 1);
    }
    check!(lua_tonumber(L, -1) == 0.5);
    1
}
unsafe fn passthrough_reuse(L: *mut lua_State) -> i32 {
    lua_callyieldable(L, 2, 1)
}
unsafe fn passthrough_reuse_cont(L: *mut lua_State, _s: i32) -> i32 {
    check!(lua_gettop(L) == 1);
    check!(lua_tonumber(L, -1) == 0.5);
    1
}
unsafe fn passthrough_vararg(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    lua_callyieldable(L, lua_gettop(L) - 1, LUA_MULTRET)
}
unsafe fn passthrough_vararg_cont(L: *mut lua_State, _s: i32) -> i32 {
    lua_gettop(L)
}
unsafe fn passthrough_state(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    let args = lua_gettop(L) - 1;
    lua_pushnumber(L, 42.0);
    lua_insert(L, 1);
    lua_callyieldable(L, args, LUA_MULTRET)
}
unsafe fn passthrough_state_cont(L: *mut lua_State, _s: i32) -> i32 {
    check!(luaL_checkinteger(L, 1) == 42);
    lua_gettop(L) - 1
}
unsafe fn pcall_then_xcall(L: *mut lua_State) -> i32 {
    luaL_checkany(L, 1);
    luaL_checkany(L, 2);
    luaL_checkstack(L, 3, "pcallThenCall");
    lua_pushinteger(L, 0);
    lua_pushinteger(L, 0);
    lua_pushvalue(L, 1);
    lua_pcallyieldable(L, 0, 1, 0)
}
unsafe fn pcall_then_xcall_cont(L: *mut lua_State, status: i32) -> i32 {
    luaL_checkstack(L, 1, "pcallThenCallContinuation");
    let variant = lua_tointeger(L, lua_upvalueindex(1));
    let state = luaL_checkinteger(L, 3);
    if state == 0 {
        if status != LUA_OK {
            lua_pushinteger(L, -1);
            lua_replace(L, 4);
        } else {
            lua_replace(L, 4);
        }
        lua_pushinteger(L, 1);
        lua_replace(L, 3);
        lua_pushvalue(L, 2);
        return if variant != 0 { lua_pcallyieldable(L, 0, LUA_MULTRET, 0) } else { lua_callyieldable(L, 0, LUA_MULTRET) };
    }
    let multiplier = luaL_checkinteger(L, 4);
    let mut value = -1;
    if status != LUA_OK {
        check!(variant != 0);
    } else {
        value = lua_tointeger(L, -1);
    }
    lua_pushinteger(L, multiplier * value);
    1
}

unsafe fn setup_cyield(L: *mut lua_State) {
    setglobalk(L, "singleYield", single_yield, single_yield_cont);
    setglobalk(L, "multipleYields", multiple_yields, multiple_yields_cont);
    setglobalk(L, "multipleYieldsWithNestedCall", multiple_yields_nested, multiple_yields_nested_cont);
    setglobalk(L, "passthroughCall", passthrough_call, passthrough_call_cont);
    setglobalk(L, "passthroughCallMoreResults", passthrough_more, passthrough_more_cont);
    setglobalk(L, "passthroughCallArgReuse", passthrough_reuse, passthrough_reuse_cont);
    setglobalk(L, "passthroughCallVaradic", passthrough_vararg, passthrough_vararg_cont);
    setglobalk(L, "passthroughCallWithState", passthrough_state, passthrough_state_cont);
    lua_pushinteger(L, 0);
    lua_pushcclosurek(L, pcall_then_xcall, Some("pcallThenCall"), 1, Some(pcall_then_xcall_cont));
    lua_setglobal(L, "pcallThenCall");
    lua_pushinteger(L, 1);
    lua_pushcclosurek(L, pcall_then_xcall, Some("pcallThenPcall"), 1, Some(pcall_then_xcall_cont));
    lua_setglobal(L, "pcallThenPcall");
}

// ---------------------------------------------------------------------------
// types.luau: RTTI
// ---------------------------------------------------------------------------

/// Approximates the type-derived `RTTI` table (`populateRTTI` over the
/// builtin definitions) by describing the runtime globals.
unsafe fn populate_rtti(L: *mut lua_State, idx: i32) {
    let idx = lua_absindex(L, idx);
    lua_newtable(L);
    lua_pushnil(L);
    while lua_next(L, idx) != 0 {
        // key at -2, value at -1
        if lua_type(L, -2) == LUA_TSTRING && lua_tostring(L, -2) != Some(b"_G") {
            if lua_istable(L, -1) {
                populate_rtti(L, -1);
            } else {
                let t = lua_type(L, -1);
                lua_pushstring(L, lua_typename(t));
            }
            lua_pushvalue(L, -3);
            lua_insert(L, -2);
            lua_rawset(L, -5);
        }
        lua_pop(L, 1);
    }
}

unsafe fn setup_types(L: *mut lua_State) {
    populate_rtti(L, LUA_GLOBALSINDEX);
    lua_setglobal(L, "RTTI");
}

// ---------------------------------------------------------------------------
// coverage
// ---------------------------------------------------------------------------

unsafe fn getcoverage(L: *mut lua_State) -> i32 {
    luaL_argexpected(L, lua_isLfunction(L, 1) != 0, 1, "function");
    lua_newtable(L);
    lua_getcoverage(L, 1, &mut |function, linedefined, depth, hits| {
        lua_newtable(L);
        match function {
            Some(f) => lua_pushlstring(L, f),
            None => lua_pushnil(L),
        }
        lua_setfield(L, -2, "name");
        lua_pushinteger(L, linedefined);
        lua_setfield(L, -2, "linedefined");
        lua_pushinteger(L, depth);
        lua_setfield(L, -2, "depth");
        for (i, &h) in hits.iter().enumerate() {
            if h != -1 {
                lua_pushinteger(L, h);
                lua_rawseti(L, -2, i as i32);
            }
        }
        lua_rawseti(L, -2, lua_objlen(L, -2) + 1);
    });
    1
}

// ---------------------------------------------------------------------------
// debugger
// ---------------------------------------------------------------------------

static mut BREAKHITS: i32 = 0;
static mut INTERRUPTEDTHREAD: *mut lua_State = std::ptr::null_mut();
static mut SINGLESTEP: bool = false;
static mut STEPHITS: i32 = 0;

unsafe fn find_local(L: *mut lua_State, level: i32, name: &str) -> bool {
    let mut n = 1;
    while let Some(local) = lua_getlocal(L, level, n) {
        n += 1;
        if local == name.as_bytes() {
            return true;
        }
        lua_pop(L, 1);
    }
    false
}

unsafe fn dbg_step(_L: *mut lua_State, _ar: *mut lua_Debug) {
    STEPHITS += 1;
}

unsafe fn dbg_break(L: *mut lua_State, _ar: *mut lua_Debug) {
    BREAKHITS += 1;
    lua_debugtrace(L);
    if BREAKHITS % 2 == 1 {
        lua_break(L);
    }
}

unsafe fn dbg_interrupt(_L: *mut lua_State, ar: *mut lua_Debug) {
    check!(INTERRUPTEDTHREAD.is_null());
    check!((*ar).userdata != 0);
    INTERRUPTEDTHREAD = (*ar).userdata as *mut lua_State;
}

unsafe fn dbg_breakpoint(L: *mut lua_State) -> i32 {
    let line = luaL_checkinteger(L, 1);
    let enabled = luaL_optboolean(L, 2, 1);
    let mut ar = lua_Debug::default();
    lua_getinfo(L, lua_stackdepth(L) - 1, b"f", &mut ar);
    lua_breakpoint(L, -1, line, enabled);
    0
}

unsafe fn setup_debugger(L: *mut lua_State) {
    let cb = lua_callbacks(L);
    lua_singlestep(L, SINGLESTEP as i32);
    (*cb).debugstep = Some(dbg_step);
    (*cb).debugbreak = Some(dbg_break);
    (*cb).debuginterrupt = Some(dbg_interrupt);
    lua_pushcclosurek(L, dbg_breakpoint, Some("breakpoint"), 0, None);
    lua_setglobal(L, "breakpoint");
}

unsafe fn debugger_yield(L: *mut lua_State) -> bool {
    check!(BREAKHITS % 2 == 1);
    match BREAKHITS {
        1 => {
            let a = lua_getargument(L, 0, 1);
            check!(a != 0);
            check!(lua_tointeger(L, -1) == 50);
            lua_pop(L, 1);
            let v = lua_getargument(L, 0, 2);
            check!(v != 0);
            check!(lua_tointeger(L, -1) == 42);
            lua_pop(L, 1);
            let found = find_local(L, 0, "b");
            check!(found);
            check!(lua_tointeger(L, -1) == 50);
            lua_pop(L, 1);
            let mut ar = lua_Debug::default();
            lua_getinfo(L, 0, b"f", &mut ar);
            let u = lua_getupvalue(L, -1, 1).map(|s| s.to_vec());
            check!(u.as_deref() == Some(b"a".as_slice()));
            check!(lua_tointeger(L, -1) == 5);
            lua_pop(L, 2);
        }
        3 => {
            let found = find_local(L, 0, "a");
            check!(found);
            check!(lua_tointeger(L, -1) == 6);
            lua_pop(L, 1);
        }
        5 | 7 | 9 => {
            let expected = match BREAKHITS {
                5 => 7,
                7 => 8,
                _ => 9,
            };
            let found = find_local(L, 1, "a");
            check!(found);
            check!(lua_tointeger(L, -1) == expected);
            lua_pop(L, 1);
        }
        13 => {
            let found = find_local(L, 0, "a");
            check!(found);
            check!(lua_isnil(L, -1));
            lua_pop(L, 1);
        }
        15 => {
            let x = lua_getlocal(L, 2, 1);
            check!(x.as_deref() == Some(b"x".as_slice()));
            if x.is_some() {
                lua_pop(L, 1);
            }
            let a1 = lua_getlocal(L, 2, 2);
            check!(a1.is_none());
        }
        17 | 19 | 23 | 25 => {
            check!(!INTERRUPTEDTHREAD.is_null());
            if !INTERRUPTEDTHREAD.is_null() {
                let x = lua_getlocal(INTERRUPTEDTHREAD, 0, 1);
                check!(x.as_deref() == Some(b"arg".as_slice()));
                if x.is_some() {
                    check!(lua_tointeger(INTERRUPTEDTHREAD, -1) == 21);
                    lua_pop(INTERRUPTEDTHREAD, 1);
                }
            }
        }
        21 => {
            let x = lua_getlocal(L, 0, 2);
            check!(x.as_deref() == Some(b"value".as_slice()));
            if x.is_some() {
                check!(lua_tointeger(L, -1) == 42);
                lua_pop(L, 1);
            }
        }
        _ => {}
    }
    if !INTERRUPTEDTHREAD.is_null() {
        lua_resume(INTERRUPTEDTHREAD, std::ptr::null_mut(), 0);
        INTERRUPTEDTHREAD = std::ptr::null_mut();
    }
    false
}

// ---------------------------------------------------------------------------
// ndebug_upvalues / tmerror
// ---------------------------------------------------------------------------

unsafe fn ndebug_yield(L: *mut lua_State) -> bool {
    let mut ar = lua_Debug::default();
    check!(lua_getinfo(L, 1, b"f", &mut ar) != 0);
    let u = lua_getupvalue(L, -1, 1).map(|s| s.to_vec());
    check!(u.as_deref() == Some(b"".as_slice()));
    check!(lua_tointeger(L, -1) == 5);
    lua_pop(L, 2);
    false
}

static mut TM_INDEX: usize = 0;
static mut TM_BREAK: bool = false;
const TM_EXPECTED: [i32; 3] = [37, 54, 73];

unsafe fn tm_protectederror(L: *mut lua_State) {
    let mut found = None;
    let mut ar = lua_Debug::default();
    let mut i = 0;
    while lua_getinfo(L, i, b"sl", &mut ar) != 0 {
        if ar.what == "Lua" {
            found = Some(ar.currentline);
            break;
        }
        i += 1;
    }
    check!(lua_isyieldable(L) != 0);
    check!(found.is_some());
    check!(TM_INDEX < TM_EXPECTED.len());
    if let Some(line) = found {
        if TM_INDEX < TM_EXPECTED.len() {
            check!(line == TM_EXPECTED[TM_INDEX]);
        }
    }
    TM_INDEX += 1;
    if TM_BREAK {
        lua_break(L);
    }
}

unsafe fn setup_tmerror(L: *mut lua_State) {
    (*lua_callbacks(L)).debugprotectederror = Some(tm_protectederror);
}

unsafe fn yield_resumeerror(_L: *mut lua_State) -> bool {
    true
}

// ---------------------------------------------------------------------------
// interrupt.luau / jit_inliner.luau post-run checks
// ---------------------------------------------------------------------------

static mut INT_INDEX: usize = 0;
const EXPECTED_HITS: [i32; 22] = [11, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 8, 20, 15, 15, 15, 15, 18, 25, 23, 26];

unsafe fn interrupt_lines(L: *mut lua_State, gc: i32) {
    if gc >= 0 {
        return;
    }
    check!(INT_INDEX < EXPECTED_HITS.len());
    let mut ar = lua_Debug::default();
    lua_getinfo(L, 0, b"l", &mut ar);
    if INT_INDEX < EXPECTED_HITS.len() {
        if ar.currentline != EXPECTED_HITS[INT_INDEX] {
            FAILURES.with(|f| {
                f.borrow_mut().push(format!(
                    "interrupt hit #{}: line {} (expected {})",
                    INT_INDEX, ar.currentline, EXPECTED_HITS[INT_INDEX]
                ))
            });
        }
    }
    INT_INDEX += 1;
    if INT_INDEX == 4 {
        lua_yield(L, 0);
    }
}

unsafe fn interrupt_yield11(L: *mut lua_State, gc: i32) {
    if gc >= 0 {
        return;
    }
    check!(INT_INDEX < 11);
    INT_INDEX += 1;
    if INT_INDEX == 11 {
        lua_yield(L, 0);
    }
}

unsafe fn interrupt_timeout(L: *mut lua_State, gc: i32) {
    if gc >= 0 {
        return;
    }
    INT_INDEX += 1;
    if INT_INDEX >= 1000 {
        luaL_error(L, "timeout");
    }
}

unsafe fn interrupt_timeout_reset(L: *mut lua_State, gc: i32) {
    if gc >= 0 {
        return;
    }
    INT_INDEX += 1;
    if INT_INDEX == 1000 {
        INT_INDEX = 0;
        luaL_error(L, "timeout");
    }
}

unsafe fn newthread_with_global(L: *mut lua_State, name: &str) -> *mut lua_State {
    let T = lua_newthread(L);
    lua_getglobal(T, name);
    T
}

unsafe fn post_interrupt(L: *mut lua_State) {
    let cb = lua_callbacks(L);
    (*cb).interrupt = Some(interrupt_lines);
    {
        let T = newthread_with_global(L, "test");
        INT_INDEX = 0;
        let status = lua_resume(T, std::ptr::null_mut(), 0);
        check!(status == LUA_YIELD);
        check!(INT_INDEX == 4);
        let status = lua_resume(T, std::ptr::null_mut(), 0);
        check!(status == LUA_OK);
        check!(INT_INDEX == EXPECTED_HITS.len());
        lua_pop(L, 1);
    }
    (*cb).interrupt = Some(interrupt_yield11);
    for test in 1..=10 {
        let T = newthread_with_global(L, &format!("infloop{}", test));
        INT_INDEX = 0;
        let status = lua_resume(T, std::ptr::null_mut(), 0);
        check!(status == LUA_YIELD);
        check!(INT_INDEX == 11);
        lua_pop(L, 1);
    }
    (*cb).interrupt = Some(interrupt_timeout);
    for test in 1..=7 {
        let T = newthread_with_global(L, &format!("hang{}", test));
        INT_INDEX = 0;
        let status = lua_resume(T, std::ptr::null_mut(), 0);
        check!(status == LUA_ERRRUN);
        check!(lua_tostring(T, -1).map_or(false, |s| s.windows(7).any(|w| w == b"timeout")));
        lua_pop(L, 1);
    }
    (*cb).interrupt = Some(interrupt_timeout_reset);
    {
        let T = newthread_with_global(L, "hangpcall");
        INT_INDEX = 0;
        let status = lua_resume(T, std::ptr::null_mut(), 0);
        check!(status == LUA_OK);
        lua_pop(L, 1);
    }
}

unsafe fn post_jit_inliner(L: *mut lua_State) {
    (*lua_callbacks(L)).interrupt = Some(interrupt_timeout);
    for test in 1..=3 {
        let T = newthread_with_global(L, &format!("fuzzfail_infinite{}", test));
        INT_INDEX = 0;
        let status = lua_resume(T, std::ptr::null_mut(), 0);
        check!(status == LUA_ERRRUN);
        check!(lua_tostring(T, -1).map_or(false, |s| s.windows(7).any(|w| w == b"timeout")));
        lua_pop(L, 1);
    }
    {
        let T = newthread_with_global(L, "fuzzfail_stale_feedback_pc");
        (*lua_callbacks(T)).interrupt = None;
        let status = lua_resume(T, std::ptr::null_mut(), 0);
        check!(status == 0);
        lua_pop(L, 1);
    }
    {
        lua_getglobal(L, "counters_proto_promotion");
        let status = lua_pcall(L, 0, 0, 0);
        check!(status == LUA_OK);
    }
}

// ---------------------------------------------------------------------------
// runner
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Alloc {
    Default,
    Limited,
    Blockable,
}

struct Test {
    setup: Option<unsafe fn(*mut lua_State)>,
    yield_cb: Option<unsafe fn(*mut lua_State) -> bool>,
    alloc: Alloc,
    opts: CompileOptions,
    post: Option<unsafe fn(*mut lua_State)>,
}

fn default_options() -> CompileOptions {
    CompileOptions { optimization_level: 1, debug_level: 1, coverage_level: 0, type_info_level: 1, vector_double: true, classes: false, ..Default::default() }
}

unsafe fn setup_native_only(L: *mut lua_State) {
    setup_native_helpers(L);
}
unsafe fn setup_tables(L: *mut lua_State) {
    setglobalfn(L, "makelud", makelud);
    setglobalfn(L, "setallocationlimit", setallocationlimit);
}
unsafe fn setup_gc(L: *mut lua_State) {
    setglobalfn(L, "setblockallocations", setblockallocations);
}
unsafe fn setup_pcall(L: *mut lua_State) {
    setglobalfn(L, "cxxthrow", cxxthrow);
    setglobalfn(L, "resumeerror", resumeerror);
}
unsafe fn setup_coroutinehost(L: *mut lua_State) {
    setglobalfn(L, "hostresume", hostresume);
    setglobalfn(L, "hostcancel", hostcancel);
}
unsafe fn setup_vector(L: *mut lua_State) {
    setup_vector_helpers(L);
    setup_native_helpers(L);
}
unsafe fn setup_iter(L: *mut lua_State) {
    setup_native_helpers(L);
    setglobalk(L, "cYieldingIterator", c_yielding_iterator, c_yielding_iterator_cont);
}
unsafe fn setup_coverage(L: *mut lua_State) {
    setglobalfn(L, "getcoverage", getcoverage);
}
unsafe fn setup_vec_userdata(L: *mut lua_State) {
    setup_vector_helpers(L);
    setup_userdata_helpers(L);
}

/// Tests that only run when native code generation is available.
const CODEGEN_ONLY: [&str; 3] = ["native.luau", "native_types.luau", "integers_regspill.luau"];

/// The runs `TEST_CASE` performs for a file (one per SUBCASE).
fn plan(name: &str) -> Vec<(Test, Option<unsafe fn()>)> {
    let t = |setup: Option<unsafe fn(*mut lua_State)>| Test {
        setup,
        yield_cb: None,
        alloc: Alloc::Default,
        opts: default_options(),
        post: None,
    };
    let at_levels = |setup: Option<unsafe fn(*mut lua_State)>| -> Vec<(Test, Option<unsafe fn()>)> {
        (0..=2u8)
            .map(|lvl| {
                let mut x = t(setup);
                x.opts.optimization_level = lvl;
                (x, None)
            })
            .collect()
    };
    match name {
        "buffers.luau" | "integers.luau" | "vector_library.luau" => vec![(t(Some(setup_native_only)), None)],
        "tables.luau" => {
            let mut x = t(Some(setup_tables));
            x.alloc = Alloc::Limited;
            vec![(x, None)]
        }
        "gc.luau" => {
            let mut x = t(Some(setup_gc));
            x.alloc = Alloc::Blockable;
            vec![(x, None)]
        }
        "pcall.luau" => {
            let mut x = t(Some(setup_pcall));
            x.alloc = Alloc::Limited;
            vec![(x, None)]
        }
        "apicalls.luau" | "exceptions.luau" => {
            let mut x = t(None);
            x.alloc = Alloc::Limited;
            vec![(x, None)]
        }
        "coroutinehost.luau" => vec![(t(Some(setup_coroutinehost)), None)],
        "cyield.luau" => vec![(t(Some(setup_cyield)), None)],
        "vector.luau" => at_levels(Some(setup_vector)),
        "types.luau" => vec![(t(Some(setup_types)), None)],
        "debugger.luau" => {
            unsafe fn plain() {
                SINGLESTEP = false;
                BREAKHITS = 0;
                STEPHITS = 0;
                INTERRUPTEDTHREAD = std::ptr::null_mut();
            }
            unsafe fn stepping() {
                SINGLESTEP = true;
                BREAKHITS = 0;
                STEPHITS = 0;
                INTERRUPTEDTHREAD = std::ptr::null_mut();
            }
            let mk = || {
                let mut x = t(Some(setup_debugger));
                x.yield_cb = Some(debugger_yield);
                x.opts.debug_level = 2;
                x.post = Some(|_L| unsafe {
                    check!(BREAKHITS == 26);
                    if SINGLESTEP {
                        check!(STEPHITS > 100);
                    }
                });
                x
            };
            vec![(mk(), Some(plain)), (mk(), Some(stepping))]
        }
        "ndebug_upvalues.luau" => {
            let mut x = t(None);
            x.yield_cb = Some(ndebug_yield);
            x.opts.debug_level = 0;
            x.opts.optimization_level = 0;
            vec![(x, None)]
        }
        "tmerror.luau" => {
            unsafe fn nobreak() {
                TM_INDEX = 0;
                TM_BREAK = false;
            }
            unsafe fn withbreak() {
                TM_INDEX = 0;
                TM_BREAK = true;
            }
            let mk = || {
                let mut x = t(Some(setup_tmerror));
                x.yield_cb = Some(yield_resumeerror);
                x.post = Some(|_L| unsafe { check!(TM_INDEX == TM_EXPECTED.len()) });
                x
            };
            vec![(mk(), Some(nobreak)), (mk(), Some(withbreak))]
        }
        "coverage.luau" => {
            let mut x = t(Some(setup_coverage));
            x.opts.coverage_level = 2;
            vec![(x, None)]
        }
        "interrupt.luau" => {
            let mut x = t(None);
            x.post = Some(post_interrupt);
            vec![(x, None)]
        }
        "iter.luau" => vec![(t(Some(setup_iter)), None)],
        "userdata.luau" => vec![(t(Some(setup_int64)), None)],
        "native_integer_spills.luau" => at_levels(None),
        "native_userdata.luau" => at_levels(Some(setup_vec_userdata)),
        "promotion.luau" | "udata_direct.luau" => vec![(t(Some(setup_vec_userdata)), None)],
        "jit_inliner.luau" => {
            let mut x = t(None);
            x.post = Some(post_jit_inliner);
            vec![(x, None)]
        }
        _ => vec![(t(None), None)],
    }
}

/// Runs one conformance file; returns the process exit code.
pub fn run_conformance(path: &str) -> i32 {
    let name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string();
    let source = match std::fs::read(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("File {} is not found: {}", path, e);
            return 1;
        }
    };
    if CODEGEN_ONLY.contains(&name.as_str()) {
        println!("{}: skipped (requires native code generation)", name);
        return 0;
    }
    let classes = name == "classes.luau" || name == "jit_inliner.luau";
    unsafe {
        luau_rt::lvmutils::CLASSES_RUNTIME = classes;
    }
    luau_ast::parser::USER_DEFINED_CLASSES.store(classes, std::sync::atomic::Ordering::Relaxed);
    let verbose = std::env::var_os("LUAU_VERBOSE").is_some();
    let mut failed = false;
    for (test, before) in plan(&name) {
        if let Some(b) = before {
            unsafe { b() };
        }
        if let Err(msg) = unsafe { run_one(&name, &source, &test, verbose) } {
            eprintln!("{}", msg);
            failed = true;
            break;
        }
        let fails = take_failures();
        if !fails.is_empty() {
            for f in fails {
                eprintln!("{}: {}", name, f);
            }
            failed = true;
            break;
        }
    }
    flush_stdout();
    if failed {
        1
    } else {
        0
    }
}

unsafe fn run_one(name: &str, source: &[u8], test: &Test, verbose: bool) -> Result<(), String> {
    let L = luaL_newstate();
    let g = (*L).global;
    (*g).memlimit = if test.alloc == Alloc::Limited { MIB8 } else { 0 };
    (*g).allocblocked = false;
    let _ = Alloc::Blockable;

    luaL_openlibs(L);
    lua_pushvalue(L, LUA_GLOBALSINDEX);
    let mut funcs: Vec<luaL_Reg> = vec![("collectgarbage", lua_collectgarbage), ("loadstring", lua_loadstring)];
    if !verbose {
        funcs.push(("print", lua_silence));
    }
    luaL_register(L, None, &funcs);
    lua_pop(L, 1);

    if let Some(setup) = test.setup {
        setup(L);
    }

    luaL_sandbox(L);
    luaL_sandboxthread(L);
    lua_pushvalue(L, LUA_GLOBALSINDEX);
    lua_setfield(L, -1, "_G");

    let chunkname = format!("={}", name);
    let mut opts = test.opts.clone();
    opts.classes = luau_rt::lvmutils::CLASSES_RUNTIME;
    let bytecode = luau_compiler::compile_source(source, opts);
    let result = luau_load(L, chunkname.as_bytes(), &bytecode, 0);
    if result == 0 {
        crate::maybe_codegen(L);
    }

    let mut status = if result == 0 { lua_resume(L, std::ptr::null_mut(), 0) } else { LUA_ERRSYNTAX };
    if let Some(y) = test.yield_cb {
        while status == LUA_YIELD || status == LUA_BREAK {
            let resume_error = y(L);
            status = if resume_error {
                lua_resumeerror(L, std::ptr::null_mut())
            } else {
                lua_resume(L, std::ptr::null_mut(), 0)
            };
        }
    }

    let outcome = if status == 0 {
        let ok = lua_isstring(L, -1) != 0 && lua_tostring(L, -1) == Some(b"OK".as_slice());
        if ok {
            lua_pop(L, 1);
            if let Some(post) = test.post {
                post(L);
            }
            Ok(())
        } else {
            let got = lua_tostring(L, -1).map(|s| String::from_utf8_lossy(s).into_owned());
            Err(format!("{}: expected \"OK\", got {:?}", name, got))
        }
    } else {
        let mut error = if status == LUA_YIELD {
            "thread yielded unexpectedly".to_string()
        } else {
            lua_tostring(L, -1).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default()
        };
        error.push_str("\nstacktrace:\n");
        error.push_str(&String::from_utf8_lossy(&lua_debugtrace(L)));
        Err(error)
    };
    flush_stdout();
    lua_close(L);
    outcome
}

/// Keeps the hook helper linked for callers that simulate breaks.
#[allow(dead_code)]
unsafe fn callhook_noop(L: *mut lua_State) {
    unsafe fn hook(_L: *mut lua_State, _ar: *mut lua_Debug) {}
    luau_callhook(L, hook, 0);
}

#[allow(dead_code)]
fn _unused(a: Option<&[u8]>) -> bool {
    bytes_eq(a, "")
}
