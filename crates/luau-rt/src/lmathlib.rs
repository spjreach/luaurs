//! Port of `VM/src/lmathlib.cpp`.

/// `luai_numpow` (`pow`).
#[inline(always)]
pub fn luai_numpow(a: f64, b: f64) -> f64 {
    crate::libm::pow(a, b)
}

/// C `frexp`.
pub fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || x.is_nan() || x.is_infinite() {
        return (x, 0);
    }
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    if exp == 0 {
        // subnormal: normalize first
        let (f, e) = frexp(x * f64::from_bits(0x4350000000000000)); // 2^54
        return (f, e - 54);
    }
    let e = exp - 1022;
    let f = f64::from_bits((bits & !(0x7ffu64 << 52)) | (1022u64 << 52));
    (f, e)
}

/// C `ldexp`.
pub fn ldexp(x: f64, e: i32) -> f64 {
    let mut x = x;
    let mut e = e;
    while e > 1000 {
        x *= 2f64.powi(1000);
        e -= 1000;
    }
    while e < -1000 {
        x *= 2f64.powi(-1000);
        e += 1000;
    }
    x * 2f64.powi(e)
}

/// C `modf`: (integral, fractional).
pub fn modf(x: f64) -> (f64, f64) {
    if x.is_infinite() {
        return (x, 0.0f64.copysign(x));
    }
    let ip = crate::lnumutils::luai_trunc(x);
    (ip, (x - ip).copysign(x))
}

// ---------------------------------------------------------------------------
// math library (`lmathlib.cpp`)
// ---------------------------------------------------------------------------

use crate::lapi::*;
use crate::laux::*;
use crate::lbaselib::{luaL_argcheck, luaL_argexpected};
use crate::lobject::*;

const PI: f64 = 3.14159265358979323846;
const RADIANS_PER_DEGREE: f64 = PI / 180.0;
const PCG32_INC: u64 = 105;

fn pcg32_random(state: &mut u64) -> u32 {
    let oldstate = *state;
    *state = oldstate.wrapping_mul(6364136223846793005).wrapping_add(PCG32_INC | 1);
    let xorshifted = (((oldstate >> 18) ^ oldstate) >> 27) as u32;
    let rot = (oldstate >> 59) as u32;
    (xorshifted >> rot) | (xorshifted << ((rot as i32).wrapping_neg() & 31))
}

fn pcg32_seed(state: &mut u64, seed: u64) {
    *state = 0;
    pcg32_random(state);
    *state = state.wrapping_add(seed);
    pcg32_random(state);
}

macro_rules! m1 {
    ($name:ident, $f:expr) => {
        unsafe fn $name(L: *mut lua_State) -> i32 {
            let f: fn(f64) -> f64 = $f;
            lua_pushnumber(L, f(luaL_checknumber(L, 1)));
            1
        }
    };
}

m1!(math_abs, f64::abs);
m1!(math_sin, crate::libm::sin);
m1!(math_sinh, crate::libm::sinh);
m1!(math_cos, crate::libm::cos);
m1!(math_cosh, crate::libm::cosh);
m1!(math_tan, crate::libm::tan);
m1!(math_tanh, crate::libm::tanh);
m1!(math_asin, crate::libm::asin);
m1!(math_acos, crate::libm::acos);
m1!(math_atan, crate::libm::atan);
m1!(math_ceil, crate::lnumutils::luai_ceil);
m1!(math_floor, crate::lnumutils::luai_floor);
m1!(math_sqrt, f64::sqrt);
m1!(math_log10, crate::libm::log10);
m1!(math_exp, crate::libm::exp);
m1!(math_deg, |x| x / RADIANS_PER_DEGREE);
m1!(math_rad, |x| x * RADIANS_PER_DEGREE);
m1!(math_round, crate::lnumutils::luai_round);
m1!(math_sign, crate::lnumutils::luai_signd);

unsafe fn math_atan2(L: *mut lua_State) -> i32 {
    lua_pushnumber(L, crate::libm::atan2(luaL_checknumber(L, 1), luaL_checknumber(L, 2)));
    1
}

unsafe fn math_fmod(L: *mut lua_State) -> i32 {
    lua_pushnumber(L, luaL_checknumber(L, 1) % luaL_checknumber(L, 2));
    1
}

unsafe fn math_modf(L: *mut lua_State) -> i32 {
    let (ip, fp) = modf(luaL_checknumber(L, 1));
    lua_pushnumber(L, ip);
    lua_pushnumber(L, fp);
    2
}

unsafe fn math_pow(L: *mut lua_State) -> i32 {
    lua_pushnumber(L, luai_numpow(luaL_checknumber(L, 1), luaL_checknumber(L, 2)));
    1
}

unsafe fn math_log(L: *mut lua_State) -> i32 {
    let x = luaL_checknumber(L, 1);
    let res = if lua_isnoneornil(L, 2) {
        crate::libm::log(x)
    } else {
        let base = luaL_checknumber(L, 2);
        if base == 2.0 {
            crate::libm::log2(x)
        } else if base == 10.0 {
            crate::libm::log10(x)
        } else {
            crate::libm::log(x) / crate::libm::log(base)
        }
    };
    lua_pushnumber(L, res);
    1
}

unsafe fn math_frexp(L: *mut lua_State) -> i32 {
    let (f, e) = frexp(luaL_checknumber(L, 1));
    lua_pushnumber(L, f);
    lua_pushinteger(L, e);
    2
}

unsafe fn math_ldexp(L: *mut lua_State) -> i32 {
    lua_pushnumber(L, ldexp(luaL_checknumber(L, 1), luaL_checkinteger(L, 2)));
    1
}

unsafe fn math_min(L: *mut lua_State) -> i32 {
    let n = lua_gettop(L);
    let mut dmin = luaL_checknumber(L, 1);
    for i in 2..=n {
        let d = luaL_checknumber(L, i);
        if d < dmin {
            dmin = d;
        }
    }
    lua_pushnumber(L, dmin);
    1
}

unsafe fn math_max(L: *mut lua_State) -> i32 {
    let n = lua_gettop(L);
    let mut dmax = luaL_checknumber(L, 1);
    for i in 2..=n {
        let d = luaL_checknumber(L, i);
        if d > dmax {
            dmax = d;
        }
    }
    lua_pushnumber(L, dmax);
    1
}

pub(crate) unsafe fn math_random(L: *mut lua_State) -> i32 {
    let g = (*L).global;
    match lua_gettop(L) {
        0 => {
            let rl = pcg32_random(&mut (*g).rngstate);
            let rh = pcg32_random(&mut (*g).rngstate);
            let rd = ldexp((rl as u64 | ((rh as u64) << 32)) as f64, -64);
            lua_pushnumber(L, rd);
        }
        1 => {
            let u = luaL_checkinteger(L, 1);
            luaL_argcheck(L, 1 <= u, 1, "interval is empty");
            let x = (u as u64).wrapping_mul(pcg32_random(&mut (*g).rngstate) as u64);
            lua_pushinteger(L, (1 + (x >> 32)) as i32);
        }
        2 => {
            let l = luaL_checkinteger(L, 1);
            let u = luaL_checkinteger(L, 2);
            luaL_argcheck(L, l <= u, 2, "interval is empty");
            let ul = (u as u32).wrapping_sub(l as u32);
            luaL_argcheck(L, ul < u32::MAX, 2, "interval is too large");
            let x = ((ul + 1) as u64).wrapping_mul(pcg32_random(&mut (*g).rngstate) as u64);
            lua_pushinteger(L, (l as i64 + (x >> 32) as i64) as i32);
        }
        _ => luaL_error(L, "wrong number of arguments"),
    }
    1
}

unsafe fn math_randomseed(L: *mut lua_State) -> i32 {
    let seed = luaL_checkinteger(L, 1);
    pcg32_seed(&mut (*(*L).global).rngstate, seed as i64 as u64);
    0
}

static K_PERLIN_HASH: [u8; 257] = [
    151, 160, 137, 91, 90, 15, 131, 13, 201, 95, 96, 53, 194, 233, 7, 225, 140, 36, 103, 30, 69, 142, 8, 99, 37, 240, 21, 10, 23, 190, 6, 148,
    247, 120, 234, 75, 0, 26, 197, 62, 94, 252, 219, 203, 117, 35, 11, 32, 57, 177, 33, 88, 237, 149, 56, 87, 174, 20, 125, 136, 171, 168, 68,
    175, 74, 165, 71, 134, 139, 48, 27, 166, 77, 146, 158, 231, 83, 111, 229, 122, 60, 211, 133, 230, 220, 105, 92, 41, 55, 46, 245, 40, 244,
    102, 143, 54, 65, 25, 63, 161, 1, 216, 80, 73, 209, 76, 132, 187, 208, 89, 18, 169, 200, 196, 135, 130, 116, 188, 159, 86, 164, 100, 109,
    198, 173, 186, 3, 64, 52, 217, 226, 250, 124, 123, 5, 202, 38, 147, 118, 126, 255, 82, 85, 212, 207, 206, 59, 227, 47, 16, 58, 17, 182, 189,
    28, 42, 223, 183, 170, 213, 119, 248, 152, 2, 44, 154, 163, 70, 221, 153, 101, 155, 167, 43, 172, 9, 129, 22, 39, 253, 19, 98, 108, 110, 79,
    113, 224, 232, 178, 185, 112, 104, 218, 246, 97, 228, 251, 34, 242, 193, 238, 210, 144, 12, 191, 179, 162, 241, 81, 51, 145, 235, 249, 14,
    239, 107, 49, 192, 214, 31, 181, 199, 106, 157, 184, 84, 204, 176, 115, 121, 50, 45, 127, 4, 150, 254, 138, 236, 205, 93, 222, 114, 67, 29,
    24, 72, 243, 141, 128, 195, 78, 66, 215, 61, 156, 180, 151,
];

static K_PERLIN_GRAD: [[f32; 3]; 16] = [
    [1.0, 1.0, 0.0],
    [-1.0, 1.0, 0.0],
    [1.0, -1.0, 0.0],
    [-1.0, -1.0, 0.0],
    [1.0, 0.0, 1.0],
    [-1.0, 0.0, 1.0],
    [1.0, 0.0, -1.0],
    [-1.0, 0.0, -1.0],
    [0.0, 1.0, 1.0],
    [0.0, -1.0, 1.0],
    [0.0, 1.0, -1.0],
    [0.0, -1.0, -1.0],
    [1.0, 1.0, 0.0],
    [0.0, -1.0, 1.0],
    [-1.0, 1.0, 0.0],
    [0.0, -1.0, -1.0],
];

#[inline]
fn perlin_fade(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}
#[inline]
fn perlin_lerp(t: f32, a: f32, b: f32) -> f32 {
    a + t * (b - a)
}
#[inline]
fn perlin_grad(hash: u8, x: f32, y: f32, z: f32) -> f32 {
    let g = &K_PERLIN_GRAD[(hash & 15) as usize];
    g[0] * x + g[1] * y + g[2] * z
}

fn perlin(x: f32, y: f32, z: f32) -> f32 {
    let fl = crate::lnumutils::luai_floorf;
    let (xflr, yflr, zflr) = (fl(x), fl(y), fl(z));
    // int(float) like C++ on x86 (cvttss2si: out of range -> INT_MIN), not a
    // saturating Rust cast
    let fi = |f: f32| -> i32 {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::x86_64::_mm_cvttss_si32(core::arch::x86_64::_mm_set_ss(f))
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            if f.is_nan() || !(-2147483648.0..2147483648.0).contains(&f) { i32::MIN } else { f as i32 }
        }
    };
    let xi = (fi(xflr) & 255) as usize;
    let yi = (fi(yflr) & 255) as usize;
    let zi = (fi(zflr) & 255) as usize;
    let (xf, yf, zf) = (x - xflr, y - yflr, z - zflr);
    let (u, v, w) = (perlin_fade(xf), perlin_fade(yf), perlin_fade(zf));
    let p = &K_PERLIN_HASH;
    let a = (p[xi] as usize + yi) & 255;
    let aa = (p[a] as usize + zi) & 255;
    let ab = (p[a + 1] as usize + zi) & 255;
    let b = (p[xi + 1] as usize + yi) & 255;
    let ba = (p[b] as usize + zi) & 255;
    let bb = (p[b + 1] as usize + zi) & 255;
    let la = perlin_lerp(u, perlin_grad(p[aa], xf, yf, zf), perlin_grad(p[ba], xf - 1.0, yf, zf));
    let lb = perlin_lerp(u, perlin_grad(p[ab], xf, yf - 1.0, zf), perlin_grad(p[bb], xf - 1.0, yf - 1.0, zf));
    let la1 = perlin_lerp(u, perlin_grad(p[aa + 1], xf, yf, zf - 1.0), perlin_grad(p[ba + 1], xf - 1.0, yf, zf - 1.0));
    let lb1 = perlin_lerp(u, perlin_grad(p[ab + 1], xf, yf - 1.0, zf - 1.0), perlin_grad(p[bb + 1], xf - 1.0, yf - 1.0, zf - 1.0));
    perlin_lerp(w, perlin_lerp(v, la, lb), perlin_lerp(v, la1, lb1))
}

/// `FFlag::FixMathNoisePrecision` (off by default, like the reference CLI).
pub static FIX_MATH_NOISE_PRECISION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

unsafe fn math_noise(L: *mut lua_State) -> i32 {
    let (mut nx, mut ny, mut nz) = (0, 0, 0);
    let x = lua_tonumberx(L, 1, Some(&mut nx));
    let y = lua_tonumberx(L, 2, Some(&mut ny));
    let z = lua_tonumberx(L, 3, Some(&mut nz));
    luaL_argexpected(L, nx != 0, 1, "number");
    luaL_argexpected(L, ny != 0 || lua_isnoneornil(L, 2), 2, "number");
    luaL_argexpected(L, nz != 0 || lua_isnoneornil(L, 3), 3, "number");
    let (mut x, mut y, mut z) = (x, y, z);
    if FIX_MATH_NOISE_PRECISION.load(std::sync::atomic::Ordering::Relaxed) {
        // fmod(v, 256), computed inline: v/256 and the product are exact, so
        // this equals fmod up to the sign of a zero result (perlin ignores it)
        let m = |v: f64| v - crate::lnumutils::luai_trunc(v * (1.0 / 256.0)) * 256.0;
        (x, y, z) = (m(x), m(y), m(z));
    }
    lua_pushnumber(L, perlin(x as f32, y as f32, z as f32) as f64);
    1
}

unsafe fn math_clamp(L: *mut lua_State) -> i32 {
    let v = luaL_checknumber(L, 1);
    let min = luaL_checknumber(L, 2);
    let max = luaL_checknumber(L, 3);
    luaL_argcheck(L, min <= max, 3, "max must be greater than or equal to min");
    let r = if v < min { min } else { v };
    lua_pushnumber(L, if r > max { max } else { r });
    1
}

unsafe fn math_map(L: *mut lua_State) -> i32 {
    let x = luaL_checknumber(L, 1);
    let inmin = luaL_checknumber(L, 2);
    let inmax = luaL_checknumber(L, 3);
    let outmin = luaL_checknumber(L, 4);
    let outmax = luaL_checknumber(L, 5);
    lua_pushnumber(L, outmin + (x - inmin) * (outmax - outmin) / (inmax - inmin));
    1
}

unsafe fn math_lerp(L: *mut lua_State) -> i32 {
    let a = luaL_checknumber(L, 1);
    let b = luaL_checknumber(L, 2);
    let t = luaL_checknumber(L, 3);
    lua_pushnumber(L, if t == 1.0 { b } else { a + (b - a) * t });
    1
}

unsafe fn math_isnan(L: *mut lua_State) -> i32 {
    lua_pushboolean(L, luaL_checknumber(L, 1).is_nan() as i32);
    1
}

unsafe fn math_isinf(L: *mut lua_State) -> i32 {
    lua_pushboolean(L, luaL_checknumber(L, 1).is_infinite() as i32);
    1
}

unsafe fn math_isfinite(L: *mut lua_State) -> i32 {
    lua_pushboolean(L, luaL_checknumber(L, 1).is_finite() as i32);
    1
}

const MATHLIB: &[luaL_Reg] = &[
    ("abs", math_abs),
    ("acos", math_acos),
    ("asin", math_asin),
    ("atan2", math_atan2),
    ("atan", math_atan),
    ("ceil", math_ceil),
    ("cosh", math_cosh),
    ("cos", math_cos),
    ("deg", math_deg),
    ("exp", math_exp),
    ("floor", math_floor),
    ("fmod", math_fmod),
    ("frexp", math_frexp),
    ("ldexp", math_ldexp),
    ("log10", math_log10),
    ("log", math_log),
    ("max", math_max),
    ("min", math_min),
    ("modf", math_modf),
    ("pow", math_pow),
    ("rad", math_rad),
    ("random", math_random),
    ("randomseed", math_randomseed),
    ("sinh", math_sinh),
    ("sin", math_sin),
    ("sqrt", math_sqrt),
    ("tanh", math_tanh),
    ("tan", math_tan),
    ("noise", math_noise),
    ("clamp", math_clamp),
    ("sign", math_sign),
    ("round", math_round),
    ("map", math_map),
    ("lerp", math_lerp),
    ("isnan", math_isnan),
    ("isinf", math_isinf),
    ("isfinite", math_isfinite),
];

pub unsafe fn luaopen_math(L: *mut lua_State) -> i32 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let seed = (L as usize as u64) ^ t ^ (crate::los::lua_clock() * 1e6) as u64;
    pcg32_seed(&mut (*(*L).global).rngstate, seed);
    luaL_register(L, Some("math"), MATHLIB);
    for (name, v) in [
        ("pi", PI),
        ("huge", f64::INFINITY),
        ("nan", f64::NAN),
        ("e", 2.71828182845904523536),
        ("phi", 1.61803398874989484820),
        ("sqrt2", 1.41421356237309504880),
        ("tau", 6.28318530717958647692),
    ] {
        lua_pushnumber(L, v);
        lua_setfield(L, -2, name);
    }
    1
}
