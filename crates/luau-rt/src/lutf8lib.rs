//! Port of `VM/src/lutf8lib.cpp`.

use crate::lapi::*;
use crate::laux::*;
use crate::lbaselib::luaL_argcheck;
use crate::lobject::*;

const MAXUNICODE: u32 = 0x10FFFF;

#[inline]
fn byte_at(s: &[u8], i: usize) -> u8 {
    // C strings are NUL terminated; reading one past the end yields 0
    if i < s.len() { s[i] } else { 0 }
}

#[inline]
fn iscont(s: &[u8], i: usize) -> bool {
    (byte_at(s, i) & 0xC0) == 0x80
}

fn u_posrelat(pos: i32, len: usize) -> i32 {
    if pos >= 0 {
        pos
    } else if (0usize.wrapping_sub(pos as isize as usize)) > len {
        0
    } else {
        len as i32 + pos + 1
    }
}

/// Decodes one UTF-8 sequence at `i`; returns the index past it.
fn utf8_decode(s: &[u8], i: usize) -> Option<(usize, i32)> {
    const LIMITS: [u32; 4] = [0xFF, 0x7F, 0x7FF, 0xFFFF];
    let mut c = byte_at(s, i) as u32;
    let mut res: u32 = 0;
    let mut count = 0usize;
    if c < 0x80 {
        res = c;
    } else {
        while c & 0x40 != 0 {
            count += 1;
            let cc = byte_at(s, i + count) as u32;
            if (cc & 0xC0) != 0x80 {
                return None;
            }
            res = (res << 6) | (cc & 0x3F);
            c <<= 1;
        }
        res |= (c & 0x7F) << (count * 5);
        if count > 3 || res > MAXUNICODE || res <= LIMITS[count] {
            return None;
        }
        if res.wrapping_sub(0xD800) < 0x800 {
            return None;
        }
    }
    Some((i + count + 1, res as i32))
}

unsafe fn utflen(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1);
    let len = s.len();
    let mut n = 0;
    let mut posi = u_posrelat(luaL_optinteger(L, 2, 1), len);
    let mut posj = u_posrelat(luaL_optinteger(L, 3, -1), len);
    luaL_argcheck(L, 1 <= posi && { posi -= 1; posi } <= len as i32, 2, "initial position out of string");
    posj -= 1;
    luaL_argcheck(L, posj < len as i32, 3, "final position out of string");
    while posi <= posj {
        match utf8_decode(s, posi as usize) {
            None => {
                lua_pushnil(L);
                lua_pushinteger(L, posi + 1);
                return 2;
            }
            Some((next, _)) => posi = next as i32,
        }
        n += 1;
    }
    lua_pushinteger(L, n);
    1
}

unsafe fn codepoint(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1);
    let len = s.len();
    let posi = u_posrelat(luaL_optinteger(L, 2, 1), len);
    let pose = u_posrelat(luaL_optinteger(L, 3, posi), len);
    luaL_argcheck(L, posi >= 1, 2, "out of range");
    luaL_argcheck(L, pose <= len as i32, 3, "out of range");
    if posi > pose {
        return 0;
    }
    if (pose as i64 - posi as i64) >= i32::MAX as i64 {
        luaL_error(L, "string slice too long");
    }
    let n = pose - posi + 1;
    luaL_checkstack(L, n, "string slice too long");
    let mut n = 0;
    let se = pose as usize;
    let mut p = (posi - 1) as usize;
    while p < se {
        match utf8_decode(s, p) {
            None => luaL_error(L, "invalid UTF-8 code"),
            Some((next, code)) => {
                p = next;
                lua_pushinteger(L, code);
                n += 1;
            }
        }
    }
    n
}

pub fn utf8esc(buff: &mut [u8; 8], mut x: u32) -> usize {
    let mut n = 1;
    if x < 0x80 {
        buff[7] = x as u8;
    } else {
        let mut mfb: u32 = 0x3f;
        loop {
            buff[8 - n] = (0x80 | (x & 0x3f)) as u8;
            n += 1;
            x >>= 6;
            mfb >>= 1;
            if x <= mfb {
                break;
            }
        }
        buff[8 - n] = ((!mfb << 1) | x) as u8;
    }
    n
}

unsafe fn buffutfchar(L: *mut lua_State, arg: i32, buff: &mut [u8; 8]) -> usize {
    let code = luaL_checkinteger(L, arg);
    luaL_argcheck(L, 0 <= code && code as u32 <= MAXUNICODE, arg, "value out of range");
    utf8esc(buff, code as u32)
}

unsafe fn utfchar(L: *mut lua_State) -> i32 {
    let mut buff = [0u8; 8];
    let n = lua_gettop(L);
    if n == 1 {
        let l = buffutfchar(L, 1, &mut buff);
        lua_pushlstring(L, &buff[8 - l..]);
    } else {
        let mut b: Vec<u8> = Vec::new();
        for i in 1..=n {
            let l = buffutfchar(L, i, &mut buff);
            b.extend_from_slice(&buff[8 - l..]);
        }
        lua_pushlstring(L, &b);
    }
    1
}

unsafe fn byteoffset(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1);
    let len = s.len() as i32;
    let mut n = luaL_checkinteger(L, 2);
    let posi0 = if n >= 0 { 1 } else { len + 1 };
    let mut posi = u_posrelat(luaL_optinteger(L, 3, posi0), s.len());
    luaL_argcheck(L, 1 <= posi && { posi -= 1; posi } <= len, 3, "position out of range");
    if n == 0 {
        while posi > 0 && iscont(s, posi as usize) {
            posi -= 1;
        }
    } else {
        if iscont(s, posi as usize) {
            luaL_error(L, "initial position is a continuation byte");
        }
        if n < 0 {
            while n < 0 && posi > 0 {
                loop {
                    posi -= 1;
                    if !(posi > 0 && iscont(s, posi as usize)) {
                        break;
                    }
                }
                n += 1;
            }
        } else {
            n -= 1;
            while n > 0 && posi < len {
                loop {
                    posi += 1;
                    if !iscont(s, posi as usize) {
                        break;
                    }
                }
                n -= 1;
            }
        }
    }
    if n == 0 {
        lua_pushinteger(L, posi + 1);
    } else {
        lua_pushnil(L);
    }
    1
}

unsafe fn iter_aux(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1);
    let len = s.len() as i32;
    let mut n = lua_tointeger(L, 2).wrapping_sub(1);
    if n < 0 {
        n = 0;
    } else if n < len {
        n += 1;
        while iscont(s, n as usize) {
            n += 1;
        }
    }
    if n >= len {
        0
    } else {
        match utf8_decode(s, n as usize) {
            Some((next, code)) if !iscont(s, next) => {
                lua_pushinteger(L, n + 1);
                lua_pushinteger(L, code);
                2
            }
            _ => luaL_error(L, "invalid UTF-8 code"),
        }
    }
}

unsafe fn iter_codes(L: *mut lua_State) -> i32 {
    luaL_checkstring(L, 1);
    lua_pushcfunction(L, iter_aux, None);
    lua_pushvalue(L, 1);
    lua_pushinteger(L, 0);
    3
}

const FUNCS: &[luaL_Reg] = &[
    ("offset", byteoffset),
    ("codepoint", codepoint),
    ("char", utfchar),
    ("len", utflen),
    ("codes", iter_codes),
];

pub unsafe fn luaopen_utf8(L: *mut lua_State) -> i32 {
    luaL_register(L, Some("utf8"), FUNCS);
    lua_pushlstring(L, b"[\0-\x7F\xC2-\xF4][\x80-\xBF]*");
    lua_setfield(L, -2, "charpattern");
    1
}
