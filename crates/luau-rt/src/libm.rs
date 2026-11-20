//! Math library routines used by `math.*`, the fast-call builtins and `^`.
//!
//! On Windows the Rust toolchain links its bundled MinGW libm, whose `sin`,
//! `cos`, `pow`, `exp`, `log` etc. go through slow x87 long-double code paths
//! (several times slower than the reference build's newer MinGW). The system's
//! Universal CRT (`ucrtbase.dll`) has fast, correctly rounded-in-practice SSE2
//! implementations; they are resolved once at first use, with the Rust (libm)
//! functions as the fallback (and on other platforms).

use std::sync::OnceLock;

type F1 = unsafe extern "C" fn(f64) -> f64;
type F2 = unsafe extern "C" fn(f64, f64) -> f64;

struct Libm {
    sin: F1,
    cos: F1,
    tan: F1,
    asin: F1,
    acos: F1,
    atan: F1,
    sinh: F1,
    cosh: F1,
    tanh: F1,
    exp: F1,
    log: F1,
    log2: F1,
    log10: F1,
    atan2: F2,
    pow: F2,
}

macro_rules! fallback1 {
    ($name:ident, $f:expr) => {
        unsafe extern "C" fn $name(x: f64) -> f64 {
            let f: fn(f64) -> f64 = $f;
            f(x)
        }
    };
}
fallback1!(r_sin, f64::sin);
fallback1!(r_cos, f64::cos);
fallback1!(r_tan, f64::tan);
fallback1!(r_asin, f64::asin);
fallback1!(r_acos, f64::acos);
fallback1!(r_atan, f64::atan);
fallback1!(r_sinh, f64::sinh);
fallback1!(r_cosh, f64::cosh);
fallback1!(r_tanh, f64::tanh);
fallback1!(r_exp, f64::exp);
fallback1!(r_log, f64::ln);
fallback1!(r_log2, f64::log2);
fallback1!(r_log10, f64::log10);
unsafe extern "C" fn r_atan2(y: f64, x: f64) -> f64 {
    y.atan2(x)
}
unsafe extern "C" fn r_pow(x: f64, y: f64) -> f64 {
    x.powf(y)
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryA(name: *const u8) -> *mut u8;
    fn GetProcAddress(module: *mut u8, name: *const u8) -> *mut u8;
}

fn load() -> Libm {
    let mut m = Libm {
        sin: r_sin,
        cos: r_cos,
        tan: r_tan,
        asin: r_asin,
        acos: r_acos,
        atan: r_atan,
        sinh: r_sinh,
        cosh: r_cosh,
        tanh: r_tanh,
        exp: r_exp,
        log: r_log,
        log2: r_log2,
        log10: r_log10,
        atan2: r_atan2,
        pow: r_pow,
    };
    #[cfg(windows)]
    unsafe {
        let h = LoadLibraryA(c"ucrtbase.dll".as_ptr() as *const u8);
        if !h.is_null() {
            macro_rules! get {
                ($field:ident, $name:literal, $t:ty) => {
                    let p = GetProcAddress(h, concat!($name, "\0").as_ptr());
                    if !p.is_null() {
                        m.$field = std::mem::transmute::<*mut u8, $t>(p);
                    }
                };
            }
            get!(sin, "sin", F1);
            get!(cos, "cos", F1);
            get!(tan, "tan", F1);
            get!(asin, "asin", F1);
            get!(acos, "acos", F1);
            get!(atan, "atan", F1);
            get!(sinh, "sinh", F1);
            get!(cosh, "cosh", F1);
            get!(tanh, "tanh", F1);
            get!(exp, "exp", F1);
            get!(log, "log", F1);
            get!(log2, "log2", F1);
            get!(log10, "log10", F1);
            get!(atan2, "atan2", F2);
            get!(pow, "pow", F2);
        }
    }
    m
}

static LIBM: OnceLock<Libm> = OnceLock::new();

#[inline(always)]
fn lib() -> &'static Libm {
    LIBM.get_or_init(load)
}

macro_rules! export1 {
    ($($name:ident),*) => {
        $(
            #[inline]
            pub fn $name(x: f64) -> f64 {
                unsafe { (lib().$name)(x) }
            }
        )*
    };
}
export1!(sin, cos, tan, asin, acos, atan, sinh, cosh, tanh, exp, log, log2, log10);

#[inline]
pub fn atan2(y: f64, x: f64) -> f64 {
    unsafe { (lib().atan2)(y, x) }
}

#[inline]
pub fn pow(x: f64, y: f64) -> f64 {
    unsafe { (lib().pow)(x, y) }
}
