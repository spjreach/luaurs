//! Port of `VM/src/lstring.cpp` (salted string interning table).

use std::ptr;

use crate::lgc::*;
use crate::lmem::*;
use crate::lobject::*;

pub const MAXSSIZE: usize = 1 << 30;
pub const ATOM_UNDEF: i16 = -32768;

#[inline(always)]
pub const fn sizestring(len: usize) -> usize {
    std::mem::offset_of!(TString, data) + len + 1
}

#[inline(always)]
fn salthash(hash: u32, saltmult: u32, saltadd: u32) -> u32 {
    saltmult.wrapping_mul(hash).wrapping_add(saltadd)
}

#[inline(always)]
fn tobucket(hash: u32, hashshift: u8) -> usize {
    // hashshift is 32 - log2(size); a shift of 32 only happens for size 1
    if hashshift >= 32 {
        0
    } else {
        (hash >> hashshift) as usize
    }
}

pub fn luaS_hash(s: &[u8]) -> u32 {
    let mut a: u32 = 0;
    let mut b: u32 = 0;
    let mut h: u32 = s.len() as u32;
    let mut str = s;
    #[inline(always)]
    fn rol(x: u32, s: u32) -> u32 {
        (x >> s) | (x << (32 - s))
    }
    while str.len() >= 32 {
        let b0 = u32::from_le_bytes([str[0], str[1], str[2], str[3]]);
        let b1 = u32::from_le_bytes([str[4], str[5], str[6], str[7]]);
        let b2 = u32::from_le_bytes([str[8], str[9], str[10], str[11]]);
        a = a.wrapping_add(b0);
        b = b.wrapping_add(b1);
        h = h.wrapping_add(b2);
        a ^= h;
        a = a.wrapping_sub(rol(h, 14));
        b ^= a;
        b = b.wrapping_sub(rol(a, 11));
        h ^= b;
        h = h.wrapping_sub(rol(b, 25));
        str = &str[12..];
    }
    // (a plain countdown: `(1..=n).rev()` adds a check per byte)
    let mut i = str.len();
    while i > 0 {
        i -= 1;
        h ^= (h << 5).wrapping_add(h >> 2).wrapping_add(str[i] as u32);
    }
    h
}

unsafe fn luaS_rehash(L: *mut lua_State, newsize: i32, saltmult: u32, saltadd: u32) {
    let g = (*L).global;
    let newhash: *mut *mut TString = luaM_newarray(L, newsize as usize, 0);
    let tb = ptr::addr_of_mut!((*g).strt);
    let hashshift = (32 - luaO_log2(newsize as u32)) as u8;
    for i in 0..newsize as usize {
        *newhash.add(i) = ptr::null_mut();
    }
    for i in 0..(*tb).size as usize {
        let mut p = *(*tb).hash.add(i);
        while !p.is_null() {
            let next = (*p).snext;
            let saltedh = salthash((*p).hash, saltmult, saltadd);
            let h1 = tobucket(saltedh, hashshift);
            (*p).snext = *newhash.add(h1);
            *newhash.add(h1) = p;
            p = next;
        }
    }
    luaM_freearray(L, (*tb).hash, (*tb).size as usize, 0);
    (*tb).size = newsize;
    (*tb).hash = newhash;
    (*tb).saltmult = saltmult;
    (*tb).saltadd = saltadd;
    (*tb).hashshift = hashshift;
}

pub unsafe fn luaS_resize(L: *mut lua_State, newsize: i32) {
    let tb = ptr::addr_of_mut!((*(*L).global).strt);
    luaS_rehash(L, newsize, (*tb).saltmult, (*tb).saltadd);
}

pub unsafe fn luaS_sethashsalt(L: *mut lua_State, saltmult: u32, saltadd: u32) {
    let saltmult = saltmult | 1;
    let tb = ptr::addr_of_mut!((*(*L).global).strt);
    if (*tb).saltmult == saltmult && (*tb).saltadd == saltadd {
        return;
    }
    luaS_rehash(L, (*tb).size, saltmult, saltadd);
}

unsafe fn newlstr(L: *mut lua_State, s: &[u8], h: u32, saltedh: u32) -> *mut TString {
    let l = s.len();
    if l > MAXSSIZE {
        luaM_toobig(L);
    }
    let ts = luaM_newgco_(L, sizestring(l), (*L).activememcat) as *mut TString;
    luaC_init(L, ts as *mut GCObject, LUA_TSTRING);
    (*ts).atom = ATOM_UNDEF;
    (*ts).hash = h;
    (*ts).len = l as u32;
    let data = ptr::addr_of_mut!((*ts).data) as *mut u8;
    ptr::copy_nonoverlapping(s.as_ptr(), data, l);
    *data.add(l) = 0;
    let tb = ptr::addr_of_mut!((*(*L).global).strt);
    let bucket = tobucket(saltedh, (*tb).hashshift);
    (*ts).snext = *(*tb).hash.add(bucket);
    *(*tb).hash.add(bucket) = ts;
    (*tb).nuse += 1;
    if (*tb).nuse > (*tb).size as u32 && (*tb).size <= i32::MAX / 2 {
        luaS_resize(L, (*tb).size * 2);
    }
    ts
}

/// `luaS_bufstart`: an uninterned string of `size` bytes to fill in place.
pub unsafe fn luaS_bufstart(L: *mut lua_State, size: usize) -> *mut TString {
    if size > MAXSSIZE {
        luaM_toobig(L);
    }
    let ts = luaM_newgco_(L, sizestring(size), (*L).activememcat) as *mut TString;
    luaC_init(L, ts as *mut GCObject, LUA_TSTRING);
    (*ts).atom = ATOM_UNDEF;
    (*ts).hash = 0;
    (*ts).len = size as u32;
    (*ts).snext = ptr::null_mut();
    ts
}

/// `luaS_buffinish`: interns a string built with `luaS_bufstart`, returning
/// an existing equal string when there is one.
pub unsafe fn luaS_buffinish(L: *mut lua_State, ts: *mut TString) -> *mut TString {
    let bytes = TString::bytes(ts);
    let h = luaS_hash(bytes);
    let tb = ptr::addr_of_mut!((*(*L).global).strt);
    let saltedh = salthash(h, (*tb).saltmult, (*tb).saltadd);
    let bucket = tobucket(saltedh, (*tb).hashshift);
    let mut el = *(*tb).hash.add(bucket);
    while !el.is_null() {
        if (*el).len == (*ts).len && TString::bytes(el) == bytes {
            if isdead((*L).global, el as *mut GCObject) {
                changewhite(el as *mut GCObject);
            }
            return el;
        }
        el = (*el).snext;
    }
    (*ts).hash = h;
    *(ptr::addr_of_mut!((*ts).data) as *mut u8).add((*ts).len as usize) = 0;
    (*ts).atom = ATOM_UNDEF;
    (*ts).snext = *(*tb).hash.add(bucket);
    *(*tb).hash.add(bucket) = ts;
    (*tb).nuse += 1;
    if (*tb).nuse > (*tb).size as u32 && (*tb).size <= i32::MAX / 2 {
        luaS_resize(L, (*tb).size * 2);
    }
    ts
}

pub unsafe fn luaS_newlstr(L: *mut lua_State, s: &[u8]) -> *mut TString {
    let g = (*L).global;
    let h = luaS_hash(s);
    let saltedh = salthash(h, (*g).strt.saltmult, (*g).strt.saltadd);
    let mut el = *(*g).strt.hash.add(tobucket(saltedh, (*g).strt.hashshift));
    while !el.is_null() {
        if (*el).len as usize == s.len() && TString::bytes(el) == s {
            if isdead(g, el as *mut GCObject) {
                changewhite(el as *mut GCObject);
            }
            return el;
        }
        el = (*el).snext;
    }
    newlstr(L, s, h, saltedh)
}

#[inline]
pub unsafe fn luaS_new(L: *mut lua_State, s: &str) -> *mut TString {
    luaS_newlstr(L, s.as_bytes())
}

#[inline]
pub unsafe fn luaS_fix(s: *mut TString) {
    (*s).hdr.marked |= bitmask(FIXEDBIT);
}

unsafe fn unlinkstr(L: *mut lua_State, ts: *mut TString) -> bool {
    let g = (*L).global;
    let saltedh = salthash((*ts).hash, (*g).strt.saltmult, (*g).strt.saltadd);
    let mut p: *mut *mut TString = (*g).strt.hash.add(tobucket(saltedh, (*g).strt.hashshift));
    while !(*p).is_null() {
        let curr = *p;
        if curr == ts {
            *p = (*curr).snext;
            return true;
        }
        p = ptr::addr_of_mut!((*curr).snext);
    }
    false
}

pub unsafe fn luaS_free(L: *mut lua_State, ts: *mut TString) {
    if unlinkstr(L, ts) {
        (*(*L).global).strt.nuse -= 1;
    }
    luaM_freegco_(L, ts as *mut GCObject, sizestring((*ts).len as usize), (*ts).hdr.memcat);
}
