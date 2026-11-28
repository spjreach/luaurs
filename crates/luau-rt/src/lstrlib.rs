//! Port of `VM/src/lstrlib.cpp`: string library (Lua patterns, format,
//! pack/unpack). `LuauOptimizeStringSplit`/`LuauOptimizeStringGsub` on.
//!
//! The matcher works on raw pointers like C: Lua strings carry a NUL
//! terminator after their data, which the C code relies on.

use std::ptr;

use crate::lapi::*;
use crate::laux::*;
use crate::lbaselib::{luaL_argcheck, luaL_argexpected};
use crate::lcfmt::*;
use crate::lobject::*;
use crate::lstring::MAXSSIZE;

#[inline]
fn posrelat(pos: i32, len: usize) -> i32 {
    let pos = if pos < 0 { pos + len as i32 + 1 } else { pos };
    if pos >= 0 {
        pos
    } else {
        0
    }
}

/// `luaL_checklstring` returning a pointer to NUL-terminated data.
unsafe fn checkptr(L: *mut lua_State, narg: i32) -> (*const u8, usize) {
    let s = luaL_checklstring(L, narg);
    (s.as_ptr(), s.len())
}

unsafe fn str_len(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1);
    lua_pushinteger(L, s.len() as i32);
    1
}

unsafe fn str_sub(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1);
    let l = s.len();
    let mut start = posrelat(luaL_checkinteger(L, 2), l);
    let mut end = posrelat(luaL_optinteger(L, 3, -1), l);
    if start < 1 {
        start = 1;
    }
    if end > l as i32 {
        end = l as i32;
    }
    if start <= end {
        let sub = std::slice::from_raw_parts(s.as_ptr().add(start as usize - 1), (end - start + 1) as usize);
        lua_pushlstring(L, sub);
    } else {
        lua_pushlstring(L, b"");
    }
    1
}

pub(crate) unsafe fn str_reverse(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1);
    if s.len() <= 256 {
        let mut buf = [0u8; 256];
        for (d, &c) in buf.iter_mut().zip(s.iter().rev()) {
            *d = c;
        }
        lua_pushlstring(L, &buf[..s.len()]);
    } else {
        let r: Vec<u8> = s.iter().rev().copied().collect();
        lua_pushlstring(L, &r);
    }
    1
}

pub(crate) unsafe fn str_lower(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1);
    push_mapped(L, &s, |c| c.to_ascii_lowercase());
    1
}

/// Pushes `s` with each byte mapped by `f` (short strings: no allocation).
unsafe fn push_mapped(L: *mut lua_State, s: &[u8], f: impl Fn(u8) -> u8) {
    if s.len() <= 256 {
        let mut buf = [0u8; 256];
        for (d, &c) in buf.iter_mut().zip(s) {
            *d = f(c);
        }
        lua_pushlstring(L, &buf[..s.len()]);
    } else {
        let r: Vec<u8> = s.iter().map(|&c| f(c)).collect();
        lua_pushlstring(L, &r);
    }
}

pub(crate) unsafe fn str_upper(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1);
    push_mapped(L, &s, |c| c.to_ascii_uppercase());
    1
}

unsafe fn str_rep(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1);
    let n = luaL_checkinteger(L, 2);
    if n <= 0 {
        lua_pushlstring(L, b"");
        return 1;
    }
    let l = s.len();
    if l > MAXSSIZE / n as usize {
        luaL_error(L, "resulting string too large");
    }
    let total = l * n as usize;
    if total == 0 {
        lua_pushlstring(L, b"");
        return 1;
    }
    // small results: filled on the stack, no allocation besides the string
    if total <= 256 {
        if (*(*L).global).allocblocked {
            crate::ldo::luaD_throw(L, LUA_ERRMEM);
        }
        // (uninitialized: only the first `total` bytes are written and read)
        let mut buf = std::mem::MaybeUninit::<[u8; 256]>::uninit();
        let bp = buf.as_mut_ptr() as *mut u8;
        std::ptr::copy_nonoverlapping(s.as_ptr(), bp, l);
        let mut have = l;
        while have < total {
            let k = have.min(total - have);
            std::ptr::copy_nonoverlapping(bp, bp.add(have), k);
            have += k;
        }
        lua_pushlstring(L, std::slice::from_raw_parts(bp, total));
        return 1;
    }
    let mut r: Vec<u8> = Vec::new();
    if r.try_reserve_exact(total).is_err() {
        crate::ldo::luaD_throw(L, LUA_ERRMEM);
    }
    // the C++ buffer is a GC-accounted string: enforce the memory limit
    let g = (*L).global;
    if ((*g).memlimit != 0 && total > (*g).memlimit) || (*g).allocblocked {
        crate::ldo::luaD_throw(L, LUA_ERRMEM);
    }
    r.extend_from_slice(s);
    while r.len() * 2 <= total {
        r.extend_from_within(..);
    }
    let rem = total - r.len();
    r.extend_from_within(..rem);
    lua_pushlstring(L, &r);
    1
}

unsafe fn str_byte(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1);
    let l = s.len();
    let mut posi = posrelat(luaL_optinteger(L, 2, 1), l);
    let mut pose = posrelat(luaL_optinteger(L, 3, posi), l);
    if posi <= 0 {
        posi = 1;
    }
    if pose as usize > l {
        pose = l as i32;
    }
    if posi > pose {
        return 0;
    }
    let n = pose - posi + 1;
    if posi.checked_add(n).is_none_or(|x| x <= pose) {
        luaL_error(L, "string slice too long");
    }
    luaL_checkstack(L, n, "string slice too long");
    let p = s.as_ptr();
    for i in 0..n {
        lua_pushinteger(L, *p.add((posi + i - 1) as usize) as i32);
    }
    n
}

unsafe fn str_char(L: *mut lua_State) -> i32 {
    let n = lua_gettop(L);
    let mut b = Vec::with_capacity(n as usize);
    crate::errguard!(L, b);
    for i in 1..=n {
        let c = luaL_checkinteger(L, i);
        luaL_argcheck(L, (c as u8) as i32 == c, i, "invalid value");
        b.push(c as u8);
    }
    lua_pushlstring(L, &b);
    1
}

// ---------------------------------------------------------------------------
// pattern matching
// ---------------------------------------------------------------------------

const CAP_UNFINISHED: isize = -1;
const CAP_POSITION: isize = -2;
const L_ESC: u8 = b'%';
const SPECIALS: &[u8] = b"^$*+?.([%-";

struct MatchState {
    matchdepth: i32,
    src_init: *const u8,
    src_end: *const u8,
    p_end: *const u8,
    L: *mut lua_State,
    level: i32,
    capture: [(*const u8, isize); LUA_MAXCAPTURES],
}

impl MatchState {
    fn new(L: *mut lua_State, s: *const u8, ls: usize, p: *const u8, lp: usize) -> Self {
        MatchState {
            matchdepth: LUAI_MAXCCALLS,
            src_init: s,
            src_end: unsafe { s.add(ls) },
            p_end: unsafe { p.add(lp) },
            L,
            level: 0,
            capture: [(ptr::null(), 0); LUA_MAXCAPTURES],
        }
    }
    fn reprep(&mut self) {
        self.level = 0;
    }
}

unsafe fn check_capture(ms: &MatchState, l: i32) -> usize {
    let l = l - b'1' as i32;
    if l < 0 || l >= ms.level || ms.capture[l as usize].1 == CAP_UNFINISHED {
        luaL_error_owned(ms.L, format!("invalid capture index %{}", l + 1));
    }
    l as usize
}

unsafe fn capture_to_close(ms: &MatchState) -> usize {
    let mut level = ms.level - 1;
    while level >= 0 {
        if ms.capture[level as usize].1 == CAP_UNFINISHED {
            return level as usize;
        }
        level -= 1;
    }
    luaL_error(ms.L, "invalid pattern capture")
}

unsafe fn classend(ms: &MatchState, mut p: *const u8) -> *const u8 {
    let c = *p;
    p = p.add(1);
    match c {
        L_ESC => {
            if p == ms.p_end {
                luaL_error(ms.L, "malformed pattern (ends with '%')");
            }
            p.add(1)
        }
        b'[' => {
            if *p == b'^' {
                p = p.add(1);
            }
            loop {
                if p == ms.p_end {
                    luaL_error(ms.L, "malformed pattern (missing ']')");
                }
                let c = *p;
                p = p.add(1);
                if c == L_ESC && p < ms.p_end {
                    p = p.add(1);
                }
                if *p == b']' {
                    break;
                }
            }
            p.add(1)
        }
        _ => p,
    }
}

fn match_class(c: i32, cl: i32) -> bool {
    let cu = c as u8;
    let res = match (cl as u8).to_ascii_lowercase() {
        b'a' => cu.is_ascii_alphabetic(),
        b'c' => cu.is_ascii_control(),
        b'd' => cu.is_ascii_digit(),
        b'g' => cu.is_ascii_graphic(),
        b'l' => cu.is_ascii_lowercase(),
        b'p' => cu.is_ascii_punctuation(),
        b's' => matches!(cu, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c),
        b'u' => cu.is_ascii_uppercase(),
        b'w' => cu.is_ascii_alphanumeric(),
        b'x' => cu.is_ascii_hexdigit(),
        b'z' => c == 0,
        _ => return cl == c,
    };
    if (cl as u8).is_ascii_lowercase() {
        res
    } else {
        !res
    }
}

unsafe fn matchbracketclass(c: i32, mut p: *const u8, ec: *const u8) -> bool {
    let mut sig = true;
    if *p.add(1) == b'^' {
        sig = false;
        p = p.add(1);
    }
    loop {
        p = p.add(1);
        if p >= ec {
            break;
        }
        if *p == L_ESC {
            p = p.add(1);
            if match_class(c, *p as i32) {
                return sig;
            }
        } else if *p.add(1) == b'-' && p.add(2) < ec {
            p = p.add(2);
            if (*p.sub(2) as i32) <= c && c <= *p as i32 {
                return sig;
            }
        } else if *p as i32 == c {
            return sig;
        }
    }
    !sig
}

unsafe fn singlematch(ms: &MatchState, s: *const u8, p: *const u8, ep: *const u8) -> bool {
    if s >= ms.src_end {
        return false;
    }
    let c = *s as i32;
    match *p {
        b'.' => true,
        L_ESC => match_class(c, *p.add(1) as i32),
        b'[' => matchbracketclass(c, p, ep.sub(1)),
        pc => pc as i32 == c,
    }
}

unsafe fn matchbalance(ms: &MatchState, mut s: *const u8, p: *const u8) -> *const u8 {
    if p >= ms.p_end.sub(1) {
        luaL_error(ms.L, "malformed pattern (missing arguments to '%b')");
    }
    if *s != *p {
        return ptr::null();
    }
    let b = *p;
    let e = *p.add(1);
    let mut cont = 1;
    loop {
        s = s.add(1);
        if s >= ms.src_end {
            break;
        }
        if *s == e {
            cont -= 1;
            if cont == 0 {
                return s.add(1);
            }
        } else if *s == b {
            cont += 1;
        }
    }
    ptr::null()
}

unsafe fn max_expand(ms: &mut MatchState, s: *const u8, p: *const u8, ep: *const u8) -> *const u8 {
    let mut i: isize = 0;
    while singlematch(ms, s.offset(i), p, ep) {
        i += 1;
    }
    while i >= 0 {
        let res = do_match(ms, s.offset(i), ep.add(1));
        if !res.is_null() {
            return res;
        }
        i -= 1;
    }
    ptr::null()
}

unsafe fn min_expand(ms: &mut MatchState, mut s: *const u8, p: *const u8, ep: *const u8) -> *const u8 {
    loop {
        let res = do_match(ms, s, ep.add(1));
        if !res.is_null() {
            return res;
        } else if singlematch(ms, s, p, ep) {
            s = s.add(1);
        } else {
            return ptr::null();
        }
    }
}

unsafe fn start_capture(ms: &mut MatchState, s: *const u8, p: *const u8, what: isize) -> *const u8 {
    let level = ms.level;
    if level as usize >= LUA_MAXCAPTURES {
        luaL_error(ms.L, "too many captures");
    }
    ms.capture[level as usize] = (s, what);
    ms.level = level + 1;
    let res = do_match(ms, s, p);
    if res.is_null() {
        ms.level -= 1;
    }
    res
}

unsafe fn end_capture(ms: &mut MatchState, s: *const u8, p: *const u8) -> *const u8 {
    let l = capture_to_close(ms);
    ms.capture[l].1 = s.offset_from(ms.capture[l].0);
    let res = do_match(ms, s, p);
    if res.is_null() {
        ms.capture[l].1 = CAP_UNFINISHED;
    }
    res
}

unsafe fn match_capture(ms: &MatchState, s: *const u8, l: i32) -> *const u8 {
    let l = check_capture(ms, l);
    let len = ms.capture[l].1 as usize;
    if (ms.src_end.offset_from(s) as usize) >= len
        && std::slice::from_raw_parts(ms.capture[l].0, len) == std::slice::from_raw_parts(s, len)
    {
        s.add(len)
    } else {
        ptr::null()
    }
}

unsafe fn do_match(ms: &mut MatchState, mut s: *const u8, mut p: *const u8) -> *const u8 {
    if ms.matchdepth == 0 {
        luaL_error(ms.L, "pattern too complex");
    }
    ms.matchdepth -= 1;
    let L = ms.L;
    if let Some(interrupt) = (*(*L).global).cb.interrupt {
        (*L).nCcalls += 1;
        interrupt(L, -1);
        (*L).nCcalls -= 1;
    }
    'init: loop {
        if p == ms.p_end {
            break;
        }
        let mut goto_dflt = false;
        match *p {
            b'(' => {
                s = if *p.add(1) == b')' {
                    start_capture(ms, s, p.add(2), CAP_POSITION)
                } else {
                    start_capture(ms, s, p.add(1), CAP_UNFINISHED)
                };
                break;
            }
            b')' => {
                s = end_capture(ms, s, p.add(1));
                break;
            }
            b'$' if p.add(1) == ms.p_end => {
                s = if s == ms.src_end { s } else { ptr::null() };
                break;
            }
            L_ESC => match *p.add(1) {
                b'b' => {
                    s = matchbalance(ms, s, p.add(2));
                    if !s.is_null() {
                        p = p.add(4);
                        continue 'init;
                    }
                    break;
                }
                b'f' => {
                    p = p.add(2);
                    if *p != b'[' {
                        luaL_error(ms.L, "missing '[' after '%f' in pattern");
                    }
                    let ep = classend(ms, p);
                    let previous = if s == ms.src_init { 0 } else { *s.sub(1) };
                    if !matchbracketclass(previous as i32, p, ep.sub(1)) && matchbracketclass(*s as i32, p, ep.sub(1)) {
                        p = ep;
                        continue 'init;
                    }
                    s = ptr::null();
                    break;
                }
                b'0'..=b'9' => {
                    s = match_capture(ms, s, *p.add(1) as i32);
                    if !s.is_null() {
                        p = p.add(2);
                        continue 'init;
                    }
                    break;
                }
                _ => goto_dflt = true,
            },
            _ => goto_dflt = true,
        }
        if goto_dflt {
            let ep = classend(ms, p);
            if !singlematch(ms, s, p, ep) {
                if *ep == b'*' || *ep == b'?' || *ep == b'-' {
                    p = ep.add(1);
                    continue 'init;
                }
                s = ptr::null();
            } else {
                match *ep {
                    b'?' => {
                        let res = do_match(ms, s.add(1), ep.add(1));
                        if !res.is_null() {
                            s = res;
                        } else {
                            p = ep.add(1);
                            continue 'init;
                        }
                    }
                    b'+' => s = max_expand(ms, s.add(1), p, ep),
                    b'*' => s = max_expand(ms, s, p, ep),
                    b'-' => s = min_expand(ms, s, p, ep),
                    _ => {
                        s = s.add(1);
                        p = ep;
                        continue 'init;
                    }
                }
            }
            break;
        }
    }
    ms.matchdepth += 1;
    s
}

extern "C" {
    #[link_name = "memchr"]
    fn c_memchr(s: *const u8, c: i32, n: usize) -> *const u8;
    #[link_name = "memcmp"]
    fn c_memcmp(a: *const u8, b: *const u8, n: usize) -> i32;
}

/// Index of the first `x` in `s` (the C runtime's vectorized `memchr`).
#[inline]
pub fn memchr(x: u8, s: &[u8]) -> Option<usize> {
    // SAFETY: the range is a valid slice
    let p = unsafe { c_memchr(s.as_ptr(), x as i32, s.len()) };
    if p.is_null() {
        None
    } else {
        Some(p as usize - s.as_ptr() as usize)
    }
}

/// `findliteral`: first occurrence of the non-empty literal `p` in
/// `[s, end)`.
#[inline]
unsafe fn findliteral(s: *const u8, end: *const u8, p: &[u8]) -> Option<*const u8> {
    let lp = p.len();
    if (end.offset_from(s) as usize) < lp {
        return None;
    }
    let last = end.sub(lp);
    if lp == 1 {
        // matches that immediately follow each other need no memchr call
        if *s == p[0] {
            return Some(s);
        }
        let r = c_memchr(s, p[0] as i32, last.offset_from(s) as usize + 1);
        return if r.is_null() { None } else { Some(r) };
    }
    let (p0, pl) = (p[0], p[lp - 1]);
    let mut s = s;
    while s <= last {
        if *s == p0 && *s.add(lp - 1) == pl && c_memcmp(s, p.as_ptr(), lp) == 0 {
            return Some(s);
        }
        s = s.add(1);
    }
    None
}

/// `lmemfind`: first occurrence of `s2` in `s1` (memchr + memcmp).
#[inline]
fn lmemfind(s1: &[u8], s2: &[u8]) -> Option<usize> {
    let l2 = s2.len();
    if l2 == 0 {
        return Some(0);
    }
    if l2 > s1.len() {
        return None;
    }
    let l2 = l2 - 1;
    let mut l1 = s1.len() - l2;
    let mut init = 0usize;
    while l1 > 0 {
        let off = memchr(s2[0], &s1[init..init + l1])?;
        let at = init + off;
        // SAFETY: at + 1 + l2 <= s1.len()
        if l2 == 0 || unsafe { c_memcmp(s1.as_ptr().add(at + 1), s2.as_ptr().add(1), l2) } == 0 {
            return Some(at);
        }
        let skip = off + 1;
        l1 -= skip;
        init = at + 1;
    }
    None
}

unsafe fn push_onecapture(ms: &MatchState, i: usize, s: *const u8, e: *const u8) {
    if i as i32 >= ms.level {
        if i == 0 {
            lua_pushlstring(ms.L, std::slice::from_raw_parts(s, e.offset_from(s) as usize));
        } else {
            luaL_error(ms.L, "invalid capture index");
        }
    } else {
        let (init, l) = ms.capture[i];
        if l == CAP_UNFINISHED {
            luaL_error(ms.L, "unfinished capture");
        }
        if l == CAP_POSITION {
            lua_pushinteger(ms.L, init.offset_from(ms.src_init) as i32 + 1);
        } else {
            lua_pushlstring(ms.L, std::slice::from_raw_parts(init, l as usize));
        }
    }
}

unsafe fn push_captures(ms: &MatchState, s: *const u8, e: *const u8) -> i32 {
    let nlevels = if ms.level == 0 && !s.is_null() { 1 } else { ms.level };
    luaL_checkstack(ms.L, nlevels, "too many captures");
    for i in 0..nlevels {
        push_onecapture(ms, i as usize, s, e);
    }
    nlevels
}

fn nospecials(p: &[u8]) -> bool {
    !p.iter().any(|c| SPECIALS.contains(c))
}

unsafe fn str_find_aux(L: *mut lua_State, find: bool) -> i32 {
    let (s, ls) = checkptr(L, 1);
    let (mut p, mut lp) = checkptr(L, 2);
    let mut init = posrelat(luaL_optinteger(L, 3, 1), ls);
    if init < 1 {
        init = 1;
    } else if init > ls as i32 + 1 {
        lua_pushnil(L);
        return 1;
    }
    let pat = std::slice::from_raw_parts(p, lp);
    if find && (lua_toboolean(L, 4) != 0 || nospecials(pat)) {
        let hay = std::slice::from_raw_parts(s.add(init as usize - 1), ls - init as usize + 1);
        if let Some(off) = lmemfind(hay, pat) {
            let start = init as usize - 1 + off;
            lua_pushinteger(L, (start + 1) as i32);
            lua_pushinteger(L, (start + lp) as i32);
            return 2;
        }
    } else {
        let mut s1 = s.add(init as usize - 1);
        let anchor = *p == b'^';
        if anchor {
            p = p.add(1);
            lp -= 1;
        }
        let mut ms = MatchState::new(L, s, ls, p, lp);
        loop {
            ms.reprep();
            let res = do_match(&mut ms, s1, p);
            if !res.is_null() {
                if find {
                    lua_pushinteger(L, s1.offset_from(s) as i32 + 1);
                    lua_pushinteger(L, res.offset_from(s) as i32);
                    return push_captures(&ms, ptr::null(), ptr::null()) + 2;
                } else {
                    return push_captures(&ms, s1, res);
                }
            }
            let cont = s1 < ms.src_end && !anchor;
            s1 = s1.add(1);
            if !cont {
                break;
            }
        }
    }
    lua_pushnil(L);
    1
}

unsafe fn str_find(L: *mut lua_State) -> i32 {
    str_find_aux(L, true)
}

unsafe fn str_match(L: *mut lua_State) -> i32 {
    str_find_aux(L, false)
}

unsafe fn gmatch_aux(L: *mut lua_State) -> i32 {
    let s = lua_tolstring(L, lua_upvalueindex(1)).unwrap();
    let p = lua_tolstring(L, lua_upvalueindex(2)).unwrap();
    let (sp, ls) = (s.as_ptr(), s.len());
    let (pp, lp) = (p.as_ptr(), p.len());
    let mut ms = MatchState::new(L, sp, ls, pp, lp);
    let mut src = sp.add(lua_tointeger(L, lua_upvalueindex(3)) as usize);
    while src <= ms.src_end {
        ms.reprep();
        let e = do_match(&mut ms, src, pp);
        if !e.is_null() {
            let mut newstart = e.offset_from(sp) as i32;
            if e == src {
                newstart += 1;
            }
            lua_pushinteger(L, newstart);
            lua_replace(L, lua_upvalueindex(3));
            return push_captures(&ms, src, e);
        }
        src = src.add(1);
    }
    0
}

unsafe fn gmatch(L: *mut lua_State) -> i32 {
    luaL_checkstring(L, 1);
    luaL_checkstring(L, 2);
    lua_settop(L, 2);
    lua_pushinteger(L, 0);
    lua_pushcclosure(L, gmatch_aux, None, 3);
    1
}

unsafe fn add_s(ms: &MatchState, b: &mut luaL_Strbuf, s: *const u8, e: *const u8) {
    let news = lua_tolstring(ms.L, 3).unwrap();
    let l = news.len();
    let mut i = 0;
    while i < l {
        if news[i] != L_ESC {
            b.addchar(news[i]);
        } else {
            i += 1;
            let c = news.get(i).copied().unwrap_or(0);
            if !c.is_ascii_digit() {
                if c != L_ESC {
                    luaL_error(ms.L, "invalid use of '%' in replacement string");
                }
                b.addchar(c);
            } else if c == b'0' {
                b.addlstring(std::slice::from_raw_parts(s, e.offset_from(s) as usize));
            } else {
                push_onecapture(ms, (c - b'1') as usize, s, e);
                b.addvalue();
            }
        }
        i += 1;
    }
}

unsafe fn add_value(ms: &MatchState, b: &mut luaL_Strbuf, s: *const u8, e: *const u8, tr: i32) {
    let L = ms.L;
    match tr {
        LUA_TFUNCTION => {
            lua_pushvalue(L, 3);
            let n = push_captures(ms, s, e);
            lua_call(L, n, 1);
        }
        LUA_TTABLE => {
            push_onecapture(ms, 0, s, e);
            lua_gettable(L, 3);
        }
        _ => {
            add_s(ms, b, s, e);
            return;
        }
    }
    if lua_toboolean(L, -1) == 0 {
        lua_pop(L, 1);
        lua_pushlstring(L, std::slice::from_raw_parts(s, e.offset_from(s) as usize));
    } else if lua_isstring(L, -1) == 0 {
        let t = luaL_typename(L, -1);
        luaL_error_owned(L, format!("invalid replacement value (a {})", t));
    }
    b.addvalue();
}

fn isliteralpattern(p: &[u8]) -> bool {
    !p.iter().any(|c| SPECIALS.contains(c) || *c == b')')
}

unsafe fn str_gsub(L: *mut lua_State) -> i32 {
    let (srcp, srcl) = checkptr(L, 1);
    let (mut p, mut lp) = checkptr(L, 2);
    let tr = lua_type(L, 3);
    let max_s = luaL_optinteger(L, 4, srcl as i32 + 1);
    let anchor = *p == b'^';
    let mut n = 0;
    luaL_argexpected(L, tr == LUA_TNUMBER || tr == LUA_TSTRING || tr == LUA_TFUNCTION || tr == LUA_TTABLE, 3, "string/function/table");
    let mut b = luaL_Strbuf::new(L);
    crate::errguard!(L, b);
    if anchor {
        p = p.add(1);
        lp -= 1;
    }
    let mut ms = MatchState::new(L, srcp, srcl, p, lp);
    let mut src = srcp;
    let pat = std::slice::from_raw_parts(p, lp);
    if !anchor && lp > 0 && isliteralpattern(pat) {
        let mut plainrepl: i32 = -1;
        let mut repl: &[u8] = &[];
        while n < max_s {
            let Some(e) = findliteral(src, ms.src_end, pat) else { break };
            n += 1;
            if e != src {
                b.addlstring(std::slice::from_raw_parts(src, e.offset_from(src) as usize));
            }
            if plainrepl < 0 {
                if tr == LUA_TSTRING || tr == LUA_TNUMBER {
                    repl = lua_tolstring(L, 3).unwrap();
                    plainrepl = (memchr(L_ESC, repl).is_none()) as i32;
                } else {
                    plainrepl = 0;
                }
            }
            if plainrepl != 0 {
                b.addlstring(repl);
            } else {
                ms.reprep();
                add_value(&ms, &mut b, e, e.add(lp), tr);
            }
            src = e.add(lp);
        }
    } else {
        while n < max_s {
            ms.reprep();
            let e = do_match(&mut ms, src, p);
            if !e.is_null() {
                n += 1;
                add_value(&ms, &mut b, src, e, tr);
            }
            if !e.is_null() && e > src {
                src = e;
            } else if src < ms.src_end {
                b.addchar(*src);
                src = src.add(1);
            } else {
                break;
            }
            if anchor {
                break;
            }
        }
    }
    b.addlstring(std::slice::from_raw_parts(src, ms.src_end.offset_from(src) as usize));
    b.pushresult();
    lua_pushinteger(L, n);
    2
}

// ---------------------------------------------------------------------------
// format
// ---------------------------------------------------------------------------

const FLAGS: &[u8] = b"-+ #0";

unsafe fn addquoted(L: *mut lua_State, b: &mut luaL_Strbuf, arg: i32) {
    let s = luaL_checklstring(L, arg);
    b.addchar(b'"');
    for &c in s {
        match c {
            b'"' | b'\\' | b'\n' => {
                b.addchar(b'\\');
                b.addchar(c);
            }
            b'\r' => b.addlstring(b"\\r"),
            0 => b.addlstring(b"\\000"),
            _ => b.addchar(c),
        }
    }
    b.addchar(b'"');
}

/// `scanformat`: returns the spec bytes and the index of the conversion.
unsafe fn scanformat(L: *mut lua_State, f: &[u8], mut i: usize) -> (Vec<u8>, usize) {
    let start = i;
    while i < f.len() && FLAGS.contains(&f[i]) {
        i += 1;
    }
    if i - start >= FLAGS.len() + 1 {
        luaL_error(L, "invalid format (repeated flags)");
    }
    let digit = |i: usize| i < f.len() && f[i].is_ascii_digit();
    if digit(i) {
        i += 1;
    }
    if digit(i) {
        i += 1;
    }
    if i < f.len() && f[i] == b'.' {
        i += 1;
        if digit(i) {
            i += 1;
        }
        if digit(i) {
            i += 1;
        }
    }
    if digit(i) {
        luaL_error(L, "invalid format (width or precision too long)");
    }
    (f[start..i].to_vec(), i)
}

unsafe fn str_format(L: *mut lua_State) -> i32 {
    let top = lua_gettop(L);
    let mut arg = 1;
    let fmt = luaL_checklstring(L, arg);
    let mut b = luaL_Strbuf::new(L);
    crate::errguard!(L, b);
    let mut i = 0;
    while i < fmt.len() {
        let c = fmt[i];
        if c != L_ESC {
            b.addchar(c);
            i += 1;
            continue;
        }
        i += 1;
        let c2 = fmt.get(i).copied().unwrap_or(0);
        if c2 == L_ESC {
            b.addchar(c2);
            i += 1;
        } else if c2 == b'*' {
            i += 1;
            arg += 1;
            if arg > top {
                luaL_error_owned(L, format!("missing argument #{}", arg));
            }
            b.addvalueany(arg);
        } else {
            arg += 1;
            if arg > top {
                luaL_error_owned(L, format!("missing argument #{}", arg));
            }
            let (specb, ci) = scanformat(L, &fmt, i);
            let conv = fmt.get(ci).copied().unwrap_or(0);
            i = ci + 1;
            let sp = parse_spec(&specb);
            match conv {
                b'c' => {
                    let v = luaL_checknumber(L, arg) as i32;
                    fmt_char(&mut b.buf, &sp, v as u8);
                }
                b'd' | b'i' => {
                    let v = if lua_type(L, arg) == LUA_TINTEGER {
                        luaL_checkinteger64(L, arg)
                    } else {
                        // C's (long long) cast on x64 (cvttsd2si): out of
                        // range and NaN give INT64_MIN, not a saturated value
                        let n = luaL_checknumber(L, arg);
                        if n >= -9223372036854775808.0 && n < 9223372036854775808.0 { n as i64 } else { i64::MIN }
                    };
                    fmt_int(&mut b.buf, &sp, v);
                }
                b'o' | b'u' | b'x' | b'X' => {
                    let v = if lua_type(L, arg) == LUA_TINTEGER {
                        luaL_checkinteger64(L, arg) as u64
                    } else {
                        let a = luaL_checknumber(L, arg);
                        if a < 0.0 {
                            (a as i64) as u64
                        } else {
                            a as u64
                        }
                    };
                    fmt_uint(&mut b.buf, &sp, v, conv);
                }
                b'e' | b'E' | b'f' | b'g' | b'G' => {
                    let v = luaL_checknumber(L, arg);
                    fmt_float(&mut b.buf, &sp, v, conv);
                }
                b'q' => addquoted(L, &mut b, arg),
                b's' => {
                    let s = luaL_checklstring(L, arg);
                    if specb.is_empty() || (!specb.contains(&b'.') && s.len() >= 100) {
                        b.addlstring(&s);
                    } else {
                        // C `%s` stops at an embedded NUL
                        let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
                        let mut tmp = Vec::new();
                        fmt_str(&mut tmp, &sp, &s[..end]);
                        tmp.truncate(511);
                        b.addlstring(&tmp);
                    }
                }
                b'*' => luaL_error(L, "'%*' does not take a form"),
                _ => {
                    let ch = if conv == 0 { String::new() } else { (conv as char).to_string() };
                    luaL_error_owned(L, format!("invalid option '%{}' to 'format'", ch));
                }
            }
        }
    }
    b.pushresult();
    1
}

unsafe fn str_split(L: *mut lua_State) -> i32 {
    // the arguments stay on the stack (alive) for the whole call
    let hay = luaL_checklstring(L, 1);
    let needle = luaL_optlstring(L, 2, b",");
    let mut num = 0;
    if needle.is_empty() {
        lua_createtable(L, hay.len() as i32, 0);
        for c in hay {
            lua_pushlstring(L, std::slice::from_ref(c));
            num += 1;
            lua_rawseti(L, -2, num);
        }
        return 1;
    }
    let mut span = 0usize;
    if needle.len() == 1 {
        let sep = needle[0];
        let mut count = 1;
        let mut from = 0;
        while let Some(off) = memchr(sep, &hay[from..]) {
            count += 1;
            from += off + 1;
        }
        lua_createtable(L, count as i32, 0);
        while let Some(off) = memchr(sep, &hay[span..]) {
            lua_pushlstring(L, &hay[span..span + off]);
            num += 1;
            lua_rawseti(L, -2, num);
            span += off + 1;
        }
    } else {
        lua_createtable(L, 0, 0);
        if needle.len() <= hay.len() {
            // first and last bytes are checked inline to avoid a memcmp
            // call at most positions
            let nl = needle.len();
            let last = hay.len() - nl;
            let (n0, nlast) = (needle[0], needle[nl - 1]);
            let mut it = 0usize;
            let hp = hay.as_ptr();
            while it <= last {
                // it + nl - 1 < hay.len() since it <= last
                if *hp.add(it) == n0 && *hp.add(it + nl - 1) == nlast && c_memcmp(hp.add(it), needle.as_ptr(), nl) == 0 {
                    lua_pushlstring(L, &hay[span..it]);
                    num += 1;
                    lua_rawseti(L, -2, num);
                    span = it + nl;
                    it = span;
                } else {
                    it += 1;
                }
            }
        }
    }
    lua_pushlstring(L, &hay[span..]);
    num += 1;
    lua_rawseti(L, -2, num);
    1
}

// ---------------------------------------------------------------------------
// pack / unpack
// ---------------------------------------------------------------------------

const MAXINTSIZE: i32 = 16;
const SZINT: i32 = 8;
const MAXALIGN: i32 = 8;

#[derive(PartialEq, Eq, Clone, Copy)]
enum KOption {
    Int,
    Uint,
    Float,
    Char,
    String,
    Zstr,
    Padding,
    Paddalign,
    Nop,
}

struct Header {
    L: *mut lua_State,
    islittle: bool,
    maxalign: i32,
}

unsafe fn getnum(h: &Header, fmt: &[u8], i: &mut usize, df: i32) -> i32 {
    let digit = |i: usize| i < fmt.len() && fmt[i].is_ascii_digit();
    if !digit(*i) {
        return df;
    }
    let mut a: i32 = 0;
    loop {
        a = a * 10 + (fmt[*i] - b'0') as i32;
        *i += 1;
        if !(digit(*i) && a <= (i32::MAX - 9) / 10) {
            break;
        }
    }
    if a as usize > MAXSSIZE || digit(*i) {
        luaL_error(h.L, "size specifier is too large");
    }
    a
}

unsafe fn getnumlimit(h: &Header, fmt: &[u8], i: &mut usize, df: i32) -> i32 {
    let sz = getnum(h, fmt, i, df);
    if sz > MAXINTSIZE || sz <= 0 {
        luaL_error_owned(h.L, format!("integral size ({}) out of limits [1,{}]", sz, MAXINTSIZE));
    }
    sz
}

unsafe fn getoption(h: &mut Header, fmt: &[u8], i: &mut usize, size: &mut i32) -> KOption {
    let opt = fmt[*i];
    *i += 1;
    *size = 0;
    match opt {
        b'b' => {
            *size = 1;
            KOption::Int
        }
        b'B' => {
            *size = 1;
            KOption::Uint
        }
        b'h' => {
            *size = 2;
            KOption::Int
        }
        b'H' => {
            *size = 2;
            KOption::Uint
        }
        b'l' => {
            *size = 8;
            KOption::Int
        }
        b'L' => {
            *size = 8;
            KOption::Uint
        }
        b'j' => {
            *size = 4;
            KOption::Int
        }
        b'J' | b'T' => {
            *size = 4;
            KOption::Uint
        }
        b'f' => {
            *size = 4;
            KOption::Float
        }
        b'd' | b'n' => {
            *size = 8;
            KOption::Float
        }
        b'i' => {
            *size = getnumlimit(h, fmt, i, 4);
            KOption::Int
        }
        b'I' => {
            *size = getnumlimit(h, fmt, i, 4);
            KOption::Uint
        }
        b's' => {
            *size = getnumlimit(h, fmt, i, 4);
            KOption::String
        }
        b'c' => {
            *size = getnum(h, fmt, i, -1);
            if *size == -1 {
                luaL_error(h.L, "missing size for format option 'c'");
            }
            KOption::Char
        }
        b'z' => KOption::Zstr,
        b'x' => {
            *size = 1;
            KOption::Padding
        }
        b'X' => KOption::Paddalign,
        b' ' => KOption::Nop,
        b'<' => {
            h.islittle = true;
            KOption::Nop
        }
        b'>' => {
            h.islittle = false;
            KOption::Nop
        }
        b'=' => {
            h.islittle = true;
            KOption::Nop
        }
        b'!' => {
            h.maxalign = getnumlimit(h, fmt, i, MAXALIGN);
            KOption::Nop
        }
        _ => luaL_error_owned(h.L, format!("invalid format option '{}'", opt as char)),
    }
}

unsafe fn getdetails(h: &mut Header, totalsize: usize, fmt: &[u8], i: &mut usize, psize: &mut i32, ntoalign: &mut i32) -> KOption {
    let opt = getoption(h, fmt, i, psize);
    let mut align = *psize;
    if opt == KOption::Paddalign && (*i >= fmt.len() || getoption(h, fmt, i, &mut align) == KOption::Char || align == 0) {
        luaL_argerror(h.L, 1, "invalid next option for option 'X'");
    }
    if align <= 1 || opt == KOption::Char {
        *ntoalign = 0;
    } else {
        if align > h.maxalign {
            align = h.maxalign;
        }
        if (align & (align - 1)) != 0 {
            luaL_argerror(h.L, 1, "format asks for alignment not power of 2");
        }
        *ntoalign = (align - (totalsize as i32 & (align - 1))) & (align - 1);
    }
    opt
}

fn packint(b: &mut luaL_Strbuf, mut n: u64, islittle: bool, size: i32, neg: bool) {
    let size = size as usize;
    let mut buff = [0u8; MAXINTSIZE as usize];
    buff[if islittle { 0 } else { size - 1 }] = (n & 0xff) as u8;
    for i in 1..size {
        n >>= 8;
        buff[if islittle { i } else { size - 1 - i }] = (n & 0xff) as u8;
    }
    if neg && size > SZINT as usize {
        for i in SZINT as usize..size {
            buff[if islittle { i } else { size - 1 - i }] = 0xff;
        }
    }
    b.addlstring(&buff[..size]);
}

fn copywithendian(dest: &mut [u8], src: &[u8], size: usize, islittle: bool) {
    if islittle {
        dest[..size].copy_from_slice(&src[..size]);
    } else {
        for k in 0..size {
            dest[size - 1 - k] = src[k];
        }
    }
}

unsafe fn str_pack(L: *mut lua_State) -> i32 {
    let fmt = luaL_checkstring(L, 1);
    let mut arg = 1;
    let mut totalsize: usize = 0;
    let mut h = Header { L, islittle: true, maxalign: 1 };
    lua_pushnil(L);
    let mut b = luaL_Strbuf::new(L);
    crate::errguard!(L, b);
    let mut i = 0;
    while i < fmt.len() && fmt[i] != 0 {
        let mut size = 0;
        let mut ntoalign = 0;
        let opt = getdetails(&mut h, totalsize, &fmt, &mut i, &mut size, &mut ntoalign);
        totalsize += (ntoalign + size) as usize;
        for _ in 0..ntoalign {
            b.addchar(0);
        }
        arg += 1;
        match opt {
            KOption::Int => {
                let n = luaL_checknumber(L, arg) as i64;
                if size < SZINT {
                    let lim: i64 = 1i64 << (size * 8 - 1);
                    luaL_argcheck(L, -lim <= n && n < lim, arg, "integer overflow");
                }
                packint(&mut b, n as u64, h.islittle, size, n < 0);
            }
            KOption::Uint => {
                let n = luaL_checknumber(L, arg) as i64;
                if size < SZINT {
                    luaL_argcheck(L, (n as u64) < (1u64 << (size * 8)), arg, "unsigned overflow");
                }
                packint(&mut b, n as u64, h.islittle, size, false);
            }
            KOption::Float => {
                let n = luaL_checknumber(L, arg);
                let mut buff = [0u8; MAXINTSIZE as usize];
                if size == 4 {
                    copywithendian(&mut buff, &(n as f32).to_le_bytes(), 4, h.islittle);
                } else {
                    copywithendian(&mut buff, &n.to_le_bytes(), 8, h.islittle);
                }
                b.addlstring(&buff[..size as usize]);
            }
            KOption::Char => {
                let s = luaL_checklstring(L, arg);
                luaL_argcheck(L, s.len() <= size as usize, arg, "string longer than given size");
                b.addlstring(&s);
                for _ in s.len()..size as usize {
                    b.addchar(0);
                }
            }
            KOption::String => {
                let s = luaL_checklstring(L, arg);
                luaL_argcheck(L, size >= 8 || (s.len() as u64) < (1u64 << (size * 8)), arg, "string length does not fit in given size");
                packint(&mut b, s.len() as u64, h.islittle, size, false);
                b.addlstring(&s);
                totalsize += s.len();
            }
            KOption::Zstr => {
                let s = luaL_checklstring(L, arg);
                luaL_argcheck(L, !s.contains(&0), arg, "string contains zeros");
                b.addlstring(&s);
                b.addchar(0);
                totalsize += s.len() + 1;
            }
            KOption::Padding => {
                b.addchar(0);
                arg -= 1;
            }
            KOption::Paddalign | KOption::Nop => arg -= 1,
        }
    }
    b.pushresult();
    1
}

unsafe fn str_packsize(L: *mut lua_State) -> i32 {
    let fmt = luaL_checkstring(L, 1);
    let mut totalsize: i32 = 0;
    let mut h = Header { L, islittle: true, maxalign: 1 };
    let mut i = 0;
    while i < fmt.len() && fmt[i] != 0 {
        let mut size = 0;
        let mut ntoalign = 0;
        let opt = getdetails(&mut h, totalsize as usize, &fmt, &mut i, &mut size, &mut ntoalign);
        luaL_argcheck(L, opt != KOption::String && opt != KOption::Zstr, 1, "variable-length format");
        size += ntoalign;
        luaL_argcheck(L, totalsize as i64 <= MAXSSIZE as i64 - size as i64, 1, "format result too large");
        totalsize += size;
    }
    lua_pushinteger(L, totalsize);
    1
}

unsafe fn unpackint(L: *mut lua_State, s: &[u8], islittle: bool, size: i32, issigned: bool) -> i64 {
    let mut res: u64 = 0;
    let limit = if size <= SZINT { size } else { SZINT };
    let size_u = size as usize;
    let mut i = limit - 1;
    while i >= 0 {
        res <<= 8;
        res |= s[if islittle { i as usize } else { size_u - 1 - i as usize }] as u64;
        i -= 1;
    }
    if size < SZINT {
        if issigned {
            let mask = 1u64 << (size * 8 - 1);
            res = (res ^ mask).wrapping_sub(mask);
        }
    } else if size > SZINT {
        let mask: u8 = if !issigned || (res as i64) >= 0 { 0 } else { 0xff };
        for i in limit as usize..size_u {
            if s[if islittle { i } else { size_u - 1 - i }] != mask {
                luaL_error_owned(L, format!("{}-byte integer does not fit into Lua Integer", size));
            }
        }
    }
    res as i64
}

unsafe fn str_unpack(L: *mut lua_State) -> i32 {
    let fmt = luaL_checkstring(L, 1);
    let data = luaL_checklstring(L, 2);
    let ld = data.len();
    let mut pos = posrelat(luaL_optinteger(L, 3, 1), ld) - 1;
    if pos < 0 {
        pos = 0;
    }
    let mut n = 0;
    luaL_argcheck(L, pos as usize <= ld, 3, "initial position out of string");
    let mut h = Header { L, islittle: true, maxalign: 1 };
    let mut i = 0;
    while i < fmt.len() && fmt[i] != 0 {
        let mut size = 0;
        let mut ntoalign = 0;
        let opt = getdetails(&mut h, pos as usize, &fmt, &mut i, &mut size, &mut ntoalign);
        luaL_argcheck(L, (ntoalign + size) as usize <= ld - pos as usize, 2, "data string too short");
        pos += ntoalign;
        luaL_checkstack(L, 2, "too many results");
        n += 1;
        let at = &data[pos as usize..];
        match opt {
            KOption::Int => {
                let res = unpackint(L, at, h.islittle, size, true);
                lua_pushnumber(L, res as f64);
            }
            KOption::Uint => {
                let res = unpackint(L, at, h.islittle, size, false) as u64;
                lua_pushnumber(L, res as f64);
            }
            KOption::Float => {
                let mut buff = [0u8; 8];
                copywithendian(&mut buff, at, size as usize, h.islittle);
                let num = if size == 4 {
                    f32::from_le_bytes([buff[0], buff[1], buff[2], buff[3]]) as f64
                } else {
                    f64::from_le_bytes(buff)
                };
                lua_pushnumber(L, num);
            }
            KOption::Char => lua_pushlstring(L, &at[..size as usize]),
            KOption::String => {
                let len = unpackint(L, at, h.islittle, size, false) as u64 as usize;
                luaL_argcheck(L, len <= ld - pos as usize - size as usize, 2, "data string too short");
                lua_pushlstring(L, &at[size as usize..size as usize + len]);
                pos += len as i32;
            }
            KOption::Zstr => {
                let len = at.iter().position(|&c| c == 0).unwrap_or(at.len());
                luaL_argcheck(L, pos as usize + len < ld, 2, "unfinished string for format 'z'");
                lua_pushlstring(L, &at[..len]);
                pos += len as i32 + 1;
            }
            KOption::Paddalign | KOption::Padding | KOption::Nop => n -= 1,
        }
        pos += size;
    }
    lua_pushinteger(L, pos + 1);
    n + 1
}

const STRLIB: &[luaL_Reg] = &[
    ("byte", str_byte),
    ("char", str_char),
    ("find", str_find),
    ("format", str_format),
    ("gmatch", gmatch),
    ("gsub", str_gsub),
    ("len", str_len),
    ("lower", str_lower),
    ("match", str_match),
    ("rep", str_rep),
    ("reverse", str_reverse),
    ("sub", str_sub),
    ("upper", str_upper),
    ("split", str_split),
    ("pack", str_pack),
    ("packsize", str_packsize),
    ("unpack", str_unpack),
];

unsafe fn createmetatable(L: *mut lua_State) {
    lua_createtable(L, 0, 1);
    lua_pushlstring(L, b"");
    lua_pushvalue(L, -2);
    lua_setmetatable(L, -2);
    lua_pop(L, 1);
    lua_pushvalue(L, -2);
    lua_setfield(L, -2, "__index");
    lua_pop(L, 1);
}

pub unsafe fn luaopen_string(L: *mut lua_State) -> i32 {
    luaL_register(L, Some("string"), STRLIB);
    createmetatable(L);
    1
}
