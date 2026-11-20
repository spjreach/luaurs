//! Port of `VM/src/loslib.cpp` and `lperf.cpp` (`lua_clock`).
//!
//! `os.date` goes through the C runtime's `gmtime`/`localtime`/`strftime` so
//! the output matches the reference build byte for byte.

#![allow(non_camel_case_types)]

use std::sync::OnceLock;
#[cfg(not(windows))]
use std::time::Instant;

use crate::lapi::*;
use crate::laux::*;
use crate::lobject::*;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceCounter(count: *mut i64) -> i32;
    fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
}

/// `lua_clock`: monotonic high-resolution timestamp in seconds. On Windows
/// this is the performance counter scaled to seconds (time since boot), as
/// in `lperf.cpp`.
#[cfg(windows)]
pub fn lua_clock() -> f64 {
    static PERIOD: OnceLock<f64> = OnceLock::new();
    let period = *PERIOD.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: plain out-parameter
        unsafe { QueryPerformanceFrequency(&mut f) };
        1.0 / f as f64
    });
    let mut c = 0i64;
    // SAFETY: plain out-parameter
    unsafe { QueryPerformanceCounter(&mut c) };
    c as f64 * period
}

#[cfg(not(windows))]
pub fn lua_clock() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    start.elapsed().as_secs_f64()
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct tm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    // glibc/BSD carry tm_gmtoff/tm_zone; keep room for them
    _pad: [i64; 4],
}

extern "C" {
    fn strftime(buf: *mut u8, max: usize, fmt: *const u8, tm: *const tm) -> usize;
}

#[cfg(windows)]
extern "C" {
    fn _gmtime64_s(out: *mut tm, t: *const i64) -> i32;
    fn _localtime64_s(out: *mut tm, t: *const i64) -> i32;
}

#[cfg(not(windows))]
extern "C" {
    fn gmtime_r(t: *const i64, out: *mut tm) -> *mut tm;
    fn localtime_r(t: *const i64, out: *mut tm) -> *mut tm;
}

fn c_gmtime(t: i64) -> Option<tm> {
    let mut r = tm::default();
    #[cfg(windows)]
    let ok = unsafe { _gmtime64_s(&mut r, &t) == 0 };
    #[cfg(not(windows))]
    let ok = unsafe { !gmtime_r(&t, &mut r).is_null() };
    ok.then_some(r)
}

fn c_localtime(t: i64) -> Option<tm> {
    let mut r = tm::default();
    #[cfg(windows)]
    let ok = unsafe { _localtime64_s(&mut r, &t) == 0 };
    #[cfg(not(windows))]
    let ok = unsafe { !localtime_r(&t, &mut r).is_null() };
    ok.then_some(r)
}

fn os_timegm(t: &tm) -> i64 {
    let day = t.tm_mday;
    let month = t.tm_mon + 1;
    let year = t.tm_year + 1900;
    let mut a = if t.tm_mon % 12 < 2 { 1 } else { 0 };
    a -= t.tm_mon / 12;
    let y = year + 4800 - a;
    let m = month + (12 * a) - 3;
    let julianday = day + ((153 * m + 2) / 5) + (365 * y) + (y / 4) - (y / 100) + (y / 400) - 32045;
    const UTCSTARTASJULIANDAY: i32 = 2440588;
    const UTCSTARTASJULIANSECOND: i64 = UTCSTARTASJULIANDAY as i64 * 86400;
    if julianday < UTCSTARTASJULIANDAY {
        return -1;
    }
    let daysecond = t.tm_hour as i64 * 3600 + t.tm_min as i64 * 60 + t.tm_sec as i64;
    let julianseconds = julianday as i64 * 86400 + daysecond;
    if julianseconds < UTCSTARTASJULIANSECOND {
        return -1;
    }
    julianseconds - UTCSTARTASJULIANSECOND
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

unsafe fn os_clock(L: *mut lua_State) -> i32 {
    lua_pushnumber(L, lua_clock());
    1
}

unsafe fn setfield(L: *mut lua_State, key: &str, value: i32) {
    lua_pushinteger(L, value);
    lua_setfield(L, -2, key);
}

unsafe fn setboolfield(L: *mut lua_State, key: &str, value: i32) {
    if value < 0 {
        return;
    }
    lua_pushboolean(L, value);
    lua_setfield(L, -2, key);
}

unsafe fn getboolfield(L: *mut lua_State, key: &str) -> i32 {
    lua_rawgetfield(L, -1, key);
    let res = if lua_isnil(L, -1) { -1 } else { lua_toboolean(L, -1) };
    lua_pop(L, 1);
    res
}

unsafe fn getfield(L: *mut lua_State, key: &str, d: i32) -> i32 {
    lua_rawgetfield(L, -1, key);
    let res = if lua_isnumber(L, -1) != 0 {
        lua_tointeger(L, -1)
    } else {
        if d < 0 {
            luaL_error_owned(L, format!("field '{}' missing in date table", key));
        }
        d
    };
    lua_pop(L, 1);
    res
}

const STRFTIMEOPTIONS: &[u8] = b"aAbBcdHIjmMpSUwWxXyYzZ%";

unsafe fn os_date(L: *mut lua_State) -> i32 {
    let mut s = luaL_optlstring(L, 1, b"%c");
    let t: i64 = if lua_isnoneornil(L, 2) { now() } else { luaL_checknumber(L, 2) as i64 };

    let stm = if s.first() == Some(&b'!') {
        s = &s[1..];
        c_gmtime(t)
    } else if t < 0 {
        None
    } else {
        c_localtime(t)
    };

    // C semantics: the format stops at the first NUL
    if let Some(z) = s.iter().position(|&c| c == 0) {
        s = &s[..z];
    }

    match stm {
        None => lua_pushnil(L),
        Some(stm) if s == b"*t" => {
            lua_createtable(L, 0, 9);
            setfield(L, "sec", stm.tm_sec);
            setfield(L, "min", stm.tm_min);
            setfield(L, "hour", stm.tm_hour);
            setfield(L, "day", stm.tm_mday);
            setfield(L, "month", stm.tm_mon + 1);
            setfield(L, "year", stm.tm_year + 1900);
            setfield(L, "wday", stm.tm_wday + 1);
            setfield(L, "yday", stm.tm_yday + 1);
            setboolfield(L, "isdst", stm.tm_isdst);
        }
        Some(stm) => {
            let mut b: Vec<u8> = Vec::new();
            let mut i = 0;
            while i < s.len() {
                if s[i] != b'%' || i + 1 >= s.len() {
                    b.push(s[i]);
                } else if !STRFTIMEOPTIONS.contains(&s[i + 1]) {
                    luaL_argerror(L, 1, "invalid conversion specifier");
                } else {
                    i += 1;
                    let cc = [b'%', s[i], 0];
                    let mut buff = [0u8; 200];
                    let n = strftime(buff.as_mut_ptr(), buff.len(), cc.as_ptr(), &stm);
                    b.extend_from_slice(&buff[..n]);
                }
                i += 1;
            }
            lua_pushlstring(L, &b);
        }
    }
    1
}

unsafe fn os_time(L: *mut lua_State) -> i32 {
    let t = if lua_isnoneornil(L, 1) {
        now()
    } else {
        luaL_checktype(L, 1, LUA_TTABLE);
        lua_settop(L, 1);
        let mut ts = tm::default();
        ts.tm_sec = getfield(L, "sec", 0);
        ts.tm_min = getfield(L, "min", 0);
        ts.tm_hour = getfield(L, "hour", 12);
        ts.tm_mday = getfield(L, "day", -1);
        ts.tm_mon = getfield(L, "month", -1) - 1;
        ts.tm_year = getfield(L, "year", -1) - 1900;
        ts.tm_isdst = getboolfield(L, "isdst");
        os_timegm(&ts)
    };
    if t == -1 {
        lua_pushnil(L);
    } else {
        lua_pushnumber(L, t as f64);
    }
    1
}

unsafe fn os_difftime(L: *mut lua_State) -> i32 {
    let a = luaL_checknumber(L, 1) as i64;
    let b = luaL_optnumber(L, 2, 0.0) as i64;
    lua_pushnumber(L, (a - b) as f64);
    1
}

const SYSLIB: &[luaL_Reg] = &[("clock", os_clock), ("date", os_date), ("difftime", os_difftime), ("time", os_time)];

pub unsafe fn luaopen_os(L: *mut lua_State) -> i32 {
    luaL_register(L, Some("os"), SYSLIB);
    1
}
