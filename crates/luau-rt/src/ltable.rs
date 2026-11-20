//! Port of `VM/src/ltable.cpp` (array part + chained scatter hash with
//! Brent's variation; boundary kept in the array part).
//!
//! Flags as in the reference CLI: `LuauTableFindImprovement` and
//! `LuauTableRobustOom` on.

use std::ptr;

use crate::ldebug::luaG_runerror;
use crate::ldo::luaD_throw;
use crate::lgc::*;
use crate::lmem::*;
use crate::lobject::*;
use crate::ltm::TM_EQ;

pub const MAXBITS: i32 = 26;
pub const MAXSIZE: i32 = 1 << MAXBITS;

/// `luaH_dummynode`: shared empty hash part. Never written (all writers
/// check for it first).
pub static mut DUMMYNODE: LuaNode = LuaNode {
    val: TValue::NIL,
    key: TKey { value: Value { l: 0 }, extra: [0], ttnext: 0 },
};

#[inline(always)]
pub fn dummynode() -> *mut LuaNode {
    unsafe { ptr::addr_of_mut!(DUMMYNODE) }
}

#[inline(always)]
pub unsafe fn gnode(t: *const LuaTable, i: i32) -> *mut LuaNode {
    (*t).node.offset(i as isize)
}
#[inline(always)]
pub unsafe fn gkey(n: *mut LuaNode) -> *mut TKey {
    ptr::addr_of_mut!((*n).key)
}
#[inline(always)]
pub unsafe fn gval(n: *mut LuaNode) -> *mut TValue {
    ptr::addr_of_mut!((*n).val)
}
#[inline(always)]
pub unsafe fn gnext(n: *const LuaNode) -> i32 {
    (*n).key.next()
}
#[inline(always)]
unsafe fn set_gnext(n: *mut LuaNode, v: i32) {
    (*n).key.set_next(v)
}
#[inline(always)]
pub unsafe fn gval2slot(t: *const LuaTable, v: *const TValue) -> i32 {
    (v as *const LuaNode).offset_from((*t).node) as i32
}
#[inline(always)]
pub unsafe fn invalidateTMcache(t: *mut LuaTable) {
    (*t).tmcache = 0;
}

#[inline(always)]
unsafe fn hashpow2(t: *const LuaTable, n: u32) -> *mut LuaNode {
    gnode(t, (n & ((1u32 << (*t).lsizenode) - 1)) as i32)
}
#[inline(always)]
pub unsafe fn hashstr(t: *const LuaTable, s: *const TString) -> *mut LuaNode {
    hashpow2(t, (*s).hash)
}
#[inline(always)]
unsafe fn hashboolean(t: *const LuaTable, p: i32) -> *mut LuaNode {
    hashpow2(t, p as u32)
}
#[inline(always)]
unsafe fn hashpointer(t: *const LuaTable, p: *const u8) -> *mut LuaNode {
    let mut h = p as usize as u32;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85ebca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2ae35);
    h ^= h >> 16;
    hashpow2(t, h)
}
#[inline(always)]
unsafe fn hash64(t: *const LuaTable, bits: u64, mask_sign: bool) -> *mut LuaNode {
    let mut h1 = bits as u32;
    let mut h2 = (bits >> 32) as u32;
    if mask_sign {
        h2 &= 0x7fffffff;
    }
    const M: u32 = 0x5bd1e995;
    h1 ^= h2 >> 18;
    h1 = h1.wrapping_mul(M);
    h2 ^= h1 >> 22;
    h2 = h2.wrapping_mul(M);
    h1 ^= h2 >> 17;
    h1 = h1.wrapping_mul(M);
    h2 ^= h1 >> 19;
    h2 = h2.wrapping_mul(M);
    hashpow2(t, h2)
}
#[inline(always)]
pub unsafe fn hashnum(t: *const LuaTable, n: f64) -> *mut LuaNode {
    hash64(t, n.to_bits(), true)
}
#[inline(always)]
unsafe fn hashint(t: *const LuaTable, n: i64) -> *mut LuaNode {
    hash64(t, n as u64, false)
}
#[inline(always)]
unsafe fn hashvec(t: *const LuaTable, v: *const f32) -> *mut LuaNode {
    let mut i = [*(v as *const u32), *(v.add(1) as *const u32), *(v.add(2) as *const u32)];
    for x in i.iter_mut() {
        if *x == 0x80000000 {
            *x = 0;
        }
        *x ^= *x >> 17;
    }
    let h = i[0].wrapping_mul(73856093) ^ i[1].wrapping_mul(19349663) ^ i[2].wrapping_mul(83492791);
    hashpow2(t, h)
}

/// `mainposition`.
#[inline(always)]
pub unsafe fn mainposition(t: *const LuaTable, key: *const TValue) -> *mut LuaNode {
    match (*key).tt {
        LUA_TNUMBER => hashnum(t, (*key).value.n),
        LUA_TINTEGER => hashint(t, (*key).value.l),
        LUA_TVECTOR => hashvec(t, vvalue(key)),
        LUA_TSTRING => hashstr(t, tsvalue(key)),
        LUA_TBOOLEAN => hashboolean(t, (*key).value.b),
        LUA_TLIGHTUSERDATA => hashpointer(t, (*key).value.p),
        _ => hashpointer(t, (*key).value.gc as *const u8),
    }
}

/// Same as `mainposition` for a node key.
unsafe fn mainposition_key(t: *const LuaTable, n: *const LuaNode) -> *mut LuaNode {
    let mut k = TValue::NIL;
    getnodekey(&mut k, n);
    mainposition(t, &k)
}

#[inline(always)]
fn arrayindex(key: f64) -> i32 {
    let i = key as i32;
    if i as f64 == key {
        i
    } else {
        -1
    }
}

unsafe fn findindex(L: *mut lua_State, t: *mut LuaTable, key: StkId) -> i32 {
    if ttisnil(key) {
        return -1;
    }
    let i = if ttisnumber(key) { arrayindex(nvalue(key)) } else { -1 };
    if 0 < i && i <= (*t).sizearray {
        return i - 1;
    }
    let mut n = mainposition(t, key);
    loop {
        let nk = gkey(n);
        if luaO_rawequalKey(nk, key) || ((*nk).tt() == LUA_TDEADKEY && iscollectable(key) && (*nk).value.gc == (*key).value.gc) {
            let i = n.offset_from(gnode(t, 0)) as i32;
            return i + (*t).sizearray;
        }
        if gnext(n) == 0 {
            break;
        }
        n = n.offset(gnext(n) as isize);
    }
    luaG_runerror(L, "invalid key to 'next'")
}

pub unsafe fn luaH_next(L: *mut lua_State, t: *mut LuaTable, key: StkId) -> i32 {
    let mut i = findindex(L, t, key);
    i += 1;
    while i < (*t).sizearray {
        let v = (*t).array.add(i as usize);
        if !ttisnil(v) {
            setnvalue(key, (i + 1) as f64);
            setobj(key.add(1), v);
            return 1;
        }
        i += 1;
    }
    i -= (*t).sizearray;
    while i < sizenode(t) {
        let n = gnode(t, i);
        if !ttisnil(gval(n)) {
            getnodekey(key, n);
            setobj(key.add(1), gval(n));
            return 1;
        }
        i += 1;
    }
    0
}

unsafe fn rawfindnode(t: *const LuaTable, startpos: i32, value: *const TValue) -> i32 {
    let mut i = startpos;
    loop {
        let e = luaH_getnum_hash(t, i);
        if ttisnil(e) {
            return 0;
        }
        if luaO_rawequalObj(e, value) {
            return i;
        }
        i += 1;
    }
}

/// Hash-part lookup of integer `key` (`chainsearchtagged<LUA_TNUMBER>`).
unsafe fn luaH_getnum_hash(t: *const LuaTable, key: i32) -> *const TValue {
    let nk = key as f64;
    let mut n = hashnum(t, nk);
    loop {
        if (*n).key.tt() == LUA_TNUMBER && (*n).key.value.n == nk {
            return gval(n);
        }
        if gnext(n) == 0 {
            break;
        }
        n = n.offset(gnext(n) as isize);
    }
    luaO_nilobject()
}

pub unsafe fn luaH_rawfind(t: *mut LuaTable, startpos: i32, value: StkId) -> i32 {
    if (startpos as u32).wrapping_sub(1) >= (*t).sizearray as u32 {
        if (*t).node == dummynode() || startpos < 1 {
            return 0;
        }
        return rawfindnode(t, startpos, value);
    }
    // `rawfindarray`, specialized per value type
    macro_rules! find {
        ($tag:expr, |$e:ident| $eq:expr) => {{
            // a pointer walk with the index computed on a hit (as tight as
            // the C++ loop)
            let tag = $tag;
            let arr = (*t).array;
            let end = arr.add((*t).sizearray as usize);
            let mut $e = arr.add(startpos as usize - 1);
            while $e < end {
                let tt = (*$e).tt;
                // keep the scan itself to one taken branch per element
                if std::intrinsics::unlikely(tt == LUA_TNIL) {
                    return 0;
                }
                if std::intrinsics::unlikely(tt == tag && $eq) {
                    return $e.offset_from(arr) as i32 + 1;
                }
                $e = $e.add(1);
            }
            0
        }};
    }
    match (*value).tt {
        LUA_TNIL => 0,
        LUA_TBOOLEAN => {
            let b = (*value).value.b;
            find!(LUA_TBOOLEAN, |e| (*e).value.b == b)
        }
        LUA_TLIGHTUSERDATA => {
            let (p, tag) = ((*value).value.p, (*value).extra[0]);
            find!(LUA_TLIGHTUSERDATA, |e| (*e).value.p == p && (*e).extra[0] == tag)
        }
        LUA_TNUMBER => {
            let v = (*value).value.n;
            find!(LUA_TNUMBER, |e| (*e).value.n == v)
        }
        LUA_TINTEGER => {
            let v = (*value).value.l;
            find!(LUA_TINTEGER, |e| (*e).value.l == v)
        }
        LUA_TVECTOR => find!(LUA_TVECTOR, |e| luai_veceq(vvalue(e), vvalue(value))),
        tt => {
            let g = (*value).value.gc;
            find!(tt, |e| (*e).value.gc == g)
        }
    }
}

// ---------------------------------------------------------------------------
// Rehash
// ---------------------------------------------------------------------------

#[inline(always)]
unsafe fn maybesetaboundary(t: *mut LuaTable, boundary: i32) {
    if (*t).lastfree <= 0 {
        (*t).lastfree = -boundary;
    }
}
#[inline(always)]
unsafe fn getaboundary(t: *const LuaTable) -> i32 {
    if (*t).lastfree < 0 {
        -(*t).lastfree
    } else {
        (*t).sizearray
    }
}

fn computesizes(nums: &[i32], narray: &mut i32) -> i32 {
    let mut a = 0;
    let mut na = 0;
    let mut n = 0;
    let mut i = 0;
    let mut twotoi: i32 = 1;
    while twotoi / 2 < *narray {
        if nums[i] > 0 {
            a += nums[i];
            if a > twotoi / 2 {
                n = twotoi;
                na = a;
            }
        }
        if a == *narray {
            break;
        }
        i += 1;
        twotoi = twotoi.wrapping_mul(2);
    }
    *narray = n;
    na
}

fn countint(key: f64, nums: &mut [i32]) -> i32 {
    let k = arrayindex(key);
    if 0 < k && k <= MAXSIZE {
        nums[ceillog2(k as u32) as usize] += 1;
        1
    } else {
        0
    }
}

unsafe fn numusearray(t: *const LuaTable, nums: &mut [i32]) -> i32 {
    let mut ause = 0;
    let mut i = 1;
    let mut ttlg = 1;
    for lg in 0..=MAXBITS as usize {
        let mut lc = 0;
        let mut lim = ttlg;
        if lim > (*t).sizearray {
            lim = (*t).sizearray;
            if i > lim {
                break;
            }
        }
        while i <= lim {
            if !ttisnil((*t).array.add(i as usize - 1)) {
                lc += 1;
            }
            i += 1;
        }
        nums[lg] += lc;
        ause += lc;
        ttlg *= 2;
    }
    ause
}

unsafe fn numusehash(t: *const LuaTable, nums: &mut [i32], pnasize: &mut i32) -> i32 {
    let mut totaluse = 0;
    let mut ause = 0;
    let mut i = sizenode(t);
    while i > 0 {
        i -= 1;
        let n = (*t).node.add(i as usize);
        if !ttisnil(gval(n)) {
            if (*n).key.tt() == LUA_TNUMBER {
                ause += countint((*n).key.value.n, nums);
            }
            totaluse += 1;
        }
    }
    *pnasize += ause;
    totaluse
}

unsafe fn hashslotsused(t: *const LuaTable) -> i32 {
    let mut totaluse = 0;
    let mut i = sizenode(t);
    while i > 0 {
        i -= 1;
        totaluse += !ttisnil(gval((*t).node.add(i as usize))) as i32;
    }
    totaluse
}

unsafe fn setarrayvector(L: *mut lua_State, t: *mut LuaTable, size: i32) {
    if size > MAXSIZE {
        luaG_runerror(L, "table overflow");
    }
    (*t).array = luaM_reallocarray(L, (*t).array, (*t).sizearray as usize, size as usize, (*t).hdr.memcat);
    for i in (*t).sizearray..size {
        setnilvalue((*t).array.add(i as usize));
    }
    (*t).sizearray = size;
}

unsafe fn setnodevector(L: *mut lua_State, t: *mut LuaTable, size: i32) {
    let lsize;
    let mut size = size;
    if size == 0 {
        (*t).node = dummynode();
        lsize = 0;
    } else {
        lsize = ceillog2(size as u32);
        if lsize > MAXBITS {
            luaG_runerror(L, "table overflow");
        }
        size = twoto(lsize);
        (*t).node = luaM_newarray(L, size as usize, (*t).hdr.memcat);
        for i in 0..size {
            let n = gnode(t, i);
            (*n).key.ttnext = 0;
            setnilvalue(gval(n));
        }
    }
    (*t).lsizenode = lsize as u8;
    (*t).nodemask8 = ((1 << lsize) - 1) as u8;
    (*t).lastfree = size;
}

unsafe fn resize(L: *mut lua_State, t: *mut LuaTable, nasize: i32, mut nhsize: i32) {
    if nasize > MAXSIZE || nhsize > MAXSIZE {
        luaG_runerror(L, "table overflow");
    }
    let mut lsizenode = 0;
    if nhsize != 0 {
        lsizenode = ceillog2(nhsize as u32);
        if lsizenode > MAXBITS {
            luaG_runerror(L, "table overflow");
        }
        nhsize = twoto(lsizenode);
    }
    let oldsizearray = (*t).sizearray;
    let oldsizenode = twoto((*t).lsizenode as i32);
    let oldarray = (*t).array;
    let oldnode = (*t).node;
    let memcat = (*t).hdr.memcat;

    let mut newarray = (*t).array;
    if nasize != oldsizearray {
        newarray = luaM_newarray::<TValue>(L, nasize as usize, memcat);
        if nasize < oldsizearray {
            if nasize != 0 {
                ptr::copy_nonoverlapping(oldarray, newarray, nasize as usize);
            }
        } else {
            if oldsizearray != 0 {
                ptr::copy_nonoverlapping(oldarray, newarray, oldsizearray as usize);
            }
            for i in oldsizearray..nasize {
                setnilvalue(newarray.add(i as usize));
            }
        }
    }

    let mut newnode = dummynode();
    if nhsize != 0 {
        if oldarray != newarray {
            newnode = luaM_trynew_(L, nhsize as usize * std::mem::size_of::<LuaNode>(), memcat) as *mut LuaNode;
            if newnode.is_null() {
                luaM_freearray(L, newarray, nasize as usize, memcat);
                luaD_throw(L, LUA_ERRMEM);
            }
        } else {
            newnode = luaM_newarray(L, nhsize as usize, memcat);
        }
        for i in 0..nhsize {
            let n = newnode.add(i as usize);
            (*n).key.ttnext = 0;
            setnilvalue(gval(n));
        }
    }

    (*t).array = newarray;
    (*t).sizearray = nasize;
    (*t).node = newnode;
    (*t).lsizenode = lsizenode as u8;
    (*t).nodemask8 = ((1 << lsizenode) - 1) as u8;
    (*t).lastfree = nhsize;

    for i in nasize..oldsizearray {
        let ov = oldarray.add(i as usize);
        if !ttisnil(ov) {
            let mut ok = TValue::NIL;
            setnvalue(&mut ok, (i + 1) as f64);
            setobj(reinsertkey(L, t, &ok, false), ov);
        }
    }
    let mut i = oldsizenode - 1;
    while i >= 0 {
        let old = oldnode.add(i as usize);
        if !ttisnil(gval(old)) {
            let mut ok = TValue::NIL;
            getnodekey(&mut ok, old);
            setobj(reinsertkey(L, t, &ok, true), gval(old));
        }
        i -= 1;
    }

    if nasize != oldsizearray {
        luaM_freearray(L, oldarray, oldsizearray as usize, memcat);
    }
    if oldnode != dummynode() {
        luaM_freearray(L, oldnode, oldsizenode as usize, memcat);
    }
}

unsafe fn adjustasize(t: *mut LuaTable, size: i32, extraintkey: i32) -> i32 {
    let tbound = (*t).node != dummynode() || size < (*t).sizearray;
    let mut size = size;
    while size + 1 == extraintkey || (tbound && !ttisnil(luaH_getnum(t, size + 1))) {
        size += 1;
    }
    size
}

pub unsafe fn luaH_resizearray(L: *mut lua_State, t: *mut LuaTable, nasize: i32) {
    if nasize > MAXSIZE {
        luaG_runerror(L, "table overflow");
    }
    let nsize = if (*t).node == dummynode() { 0 } else { sizenode(t) };
    let asize = adjustasize(t, nasize, -1);
    resize(L, t, asize, nsize);
}

pub unsafe fn luaH_resizehash(L: *mut lua_State, t: *mut LuaTable, nhsize: i32) {
    resize(L, t, (*t).sizearray, nhsize);
}

unsafe fn rehash(L: *mut lua_State, t: *mut LuaTable, isintkey: bool, newintkey: i32) {
    if !isintkey {
        let nh = hashslotsused(t) + 1;
        resize(L, t, (*t).sizearray, nh);
        return;
    }
    let mut nums = [0i32; MAXBITS as usize + 1];
    let mut nasize = 0;
    let mut totaluse = 0;
    if (*t).sizearray != 0 {
        nasize = numusearray(t, &mut nums);
        totaluse = nasize;
    }
    if (*t).node != dummynode() {
        totaluse += numusehash(t, &mut nums, &mut nasize);
    }
    if 0 < newintkey && newintkey <= MAXSIZE {
        nums[ceillog2(newintkey as u32) as usize] += 1;
        nasize += 1;
    }
    totaluse += 1;
    let na = computesizes(&nums, &mut nasize);
    let mut nh = totaluse - na;
    let nadjusted = adjustasize(t, nasize, newintkey);
    let aextra = nadjusted - nasize;
    if aextra != 0 {
        nh -= aextra;
        nasize = nadjusted + aextra;
        nasize = adjustasize(t, nasize, newintkey);
    }
    resize(L, t, nasize, nh);
}

// ---------------------------------------------------------------------------

pub unsafe fn luaH_new(L: *mut lua_State, narray: i32, nhash: i32) -> *mut LuaTable {
    let t = luaM_newgco_(L, std::mem::size_of::<LuaTable>(), (*L).activememcat) as *mut LuaTable;
    luaC_init(L, t as *mut GCObject, LUA_TTABLE);
    (*t).metatable = ptr::null_mut();
    (*t).tmcache = !0;
    (*t).array = ptr::null_mut();
    (*t).sizearray = 0;
    (*t).lastfree = 0;
    (*t).lsizenode = 0;
    (*t).readonly = 0;
    (*t).safeenv = 0;
    (*t).nodemask8 = 0;
    (*t).node = dummynode();
    (*t).gclist = ptr::null_mut();
    if narray > 0 {
        setarrayvector(L, t, narray);
    }
    if nhash > 0 {
        setnodevector(L, t, nhash);
    }
    t
}

pub unsafe fn luaH_free(L: *mut lua_State, t: *mut LuaTable) {
    let memcat = (*t).hdr.memcat;
    if (*t).node != dummynode() {
        luaM_freearray(L, (*t).node, sizenode(t) as usize, memcat);
    }
    if !(*t).array.is_null() {
        if hasmetacache(t) {
            luaM_freearray(L, (*t).array.sub(TM_N), (*t).sizearray as usize + TM_N, memcat);
        } else {
            luaM_freearray(L, (*t).array, (*t).sizearray as usize, memcat);
        }
    }
    luaM_freegco_(L, t as *mut GCObject, std::mem::size_of::<LuaTable>(), memcat);
}

unsafe fn getfreepos(t: *mut LuaTable) -> *mut LuaNode {
    while (*t).lastfree > 0 {
        (*t).lastfree -= 1;
        let n = gnode(t, (*t).lastfree);
        if (*n).key.tt() == LUA_TNIL {
            return n;
        }
    }
    ptr::null_mut()
}

unsafe fn newcollidingkey(t: *mut LuaTable, mp: *mut LuaNode, n: *mut LuaNode) -> *mut LuaNode {
    let mut othern = mainposition_key(t, mp);
    if othern != mp {
        while othern.offset(gnext(othern) as isize) != mp {
            othern = othern.offset(gnext(othern) as isize);
        }
        set_gnext(othern, n.offset_from(othern) as i32);
        *n = *mp;
        if gnext(mp) != 0 {
            set_gnext(n, gnext(n) + mp.offset_from(n) as i32);
            set_gnext(mp, 0);
        }
        setnilvalue(gval(mp));
        mp
    } else {
        if gnext(mp) != 0 {
            set_gnext(n, mp.offset(gnext(mp) as isize).offset_from(n) as i32);
        }
        set_gnext(mp, n.offset_from(mp) as i32);
        n
    }
}

#[inline(never)]
unsafe fn newkey_impl(L: *mut lua_State, t: *mut LuaTable, key: *const TValue, isintkey: bool, newintkey: i32) -> *mut TValue {
    if isintkey && newintkey == (*t).sizearray + 1 {
        rehash(L, t, true, newintkey);
        if (newintkey as u32).wrapping_sub(1) < (*t).sizearray as u32 {
            return (*t).array.add(newintkey as usize - 1);
        }
        return newkey_impl(L, t, key, isintkey, newintkey);
    }
    let mut mp = mainposition(t, key);
    if !ttisnil(gval(mp)) || mp == dummynode() {
        let n = getfreepos(t);
        if n.is_null() {
            rehash(L, t, isintkey, newintkey);
            if isintkey && (newintkey as u32).wrapping_sub(1) < (*t).sizearray as u32 {
                return (*t).array.add(newintkey as usize - 1);
            }
            return newkey_impl(L, t, key, isintkey, newintkey);
        }
        mp = newcollidingkey(t, mp, n);
    }
    setnodekey(mp, key);
    luaC_barriert(L, t, key);
    gval(mp)
}

unsafe fn reinsertkey(L: *mut lua_State, t: *mut LuaTable, key: *const TValue, checkarraypart: bool) -> *mut TValue {
    if checkarraypart && ttisnumber(key) {
        let n = nvalue(key);
        let k = crate::lvmexecute::d2i_trunc(n);
        if k as f64 == n && (k as u32).wrapping_sub(1) < (*t).sizearray as u32 {
            return (*t).array.add(k as usize - 1);
        }
    }
    let mut mp = mainposition(t, key);
    if !ttisnil(gval(mp)) || mp == dummynode() {
        mp = newcollidingkey(t, mp, getfreepos(t));
    }
    setnodekey(mp, key);
    luaC_barriert(L, t, key);
    gval(mp)
}

#[inline]
pub unsafe fn luaH_getnum(t: *mut LuaTable, key: i32) -> *const TValue {
    if (key as u32).wrapping_sub(1) < (*t).sizearray as u32 {
        (*t).array.add(key as usize - 1)
    } else if (*t).node != dummynode() {
        luaH_getnum_hash(t, key)
    } else {
        luaO_nilobject()
    }
}

#[inline]
pub unsafe fn luaH_getstr(t: *mut LuaTable, key: *mut TString) -> *const TValue {
    let mut n = hashstr(t, key);
    loop {
        if (*n).key.tt() == LUA_TSTRING && (*n).key.value.gc == key as *mut GCObject {
            return gval(n);
        }
        let nx = gnext(n);
        if nx == 0 {
            break;
        }
        n = n.offset(nx as isize);
    }
    luaO_nilobject()
}

#[inline]
pub unsafe fn luaH_getp(t: *mut LuaTable, key: *mut u8, tag: i32) -> *const TValue {
    let mut n = hashpointer(t, key);
    loop {
        let nk = &(*n).key;
        if nk.tt() == LUA_TLIGHTUSERDATA && nk.value.p == key && nk.extra[0] == tag {
            return gval(n);
        }
        if gnext(n) == 0 {
            break;
        }
        n = n.offset(gnext(n) as isize);
    }
    luaO_nilobject()
}

#[inline(always)]
unsafe fn chainsearch(t: *mut LuaTable, key: *const TValue) -> *const TValue {
    let mut n = mainposition(t, key);
    let tt = (*key).tt;
    loop {
        if (*n).key.tt() == tt && luaO_rawequalKey(gkey(n), key) {
            return gval(n);
        }
        if gnext(n) == 0 {
            break;
        }
        n = n.offset(gnext(n) as isize);
    }
    luaO_nilobject()
}

#[inline(always)]
pub unsafe fn luaH_get(t: *mut LuaTable, key: *const TValue) -> *const TValue {
    match (*key).tt {
        LUA_TNIL => luaO_nilobject(),
        LUA_TLIGHTUSERDATA => luaH_getp(t, (*key).value.p, (*key).extra[0]),
        LUA_TNUMBER => {
            let n = (*key).value.n;
            let k = crate::lvmexecute::d2i_trunc(n);
            if k as f64 == n {
                return luaH_getnum(t, k);
            }
            chainsearch(t, key)
        }
        LUA_TSTRING => luaH_getstr(t, tsvalue(key)),
        _ => chainsearch(t, key),
    }
}

pub unsafe fn luaH_set(L: *mut lua_State, t: *mut LuaTable, key: *const TValue) -> *mut TValue {
    let p = luaH_get(t, key);
    invalidateTMcache(t);
    if p != luaO_nilobject() {
        p as *mut TValue
    } else {
        luaH_newkey(L, t, key)
    }
}

pub unsafe fn luaH_newkey(L: *mut lua_State, t: *mut LuaTable, key: *const TValue) -> *mut TValue {
    match (*key).tt {
        LUA_TNIL => luaG_runerror(L, "table index is nil"),
        LUA_TNUMBER => {
            let n = (*key).value.n;
            if n.is_nan() {
                luaG_runerror(L, "table index is NaN");
            }
            let k = crate::lvmexecute::d2i_trunc(n);
            if k as f64 == n {
                return newkey_impl(L, t, key, true, k);
            }
            newkey_impl(L, t, key, false, -1)
        }
        LUA_TVECTOR => {
            let v = vvalue(key);
            if (*v).is_nan() || (*v.add(1)).is_nan() || f32::from_bits(*(v.add(2) as *const u32)).is_nan() {
                luaG_runerror(L, "table index contains NaN");
            }
            newkey_impl(L, t, key, false, -1)
        }
        _ => newkey_impl(L, t, key, false, -1),
    }
}

pub unsafe fn luaH_setnum(L: *mut lua_State, t: *mut LuaTable, key: i32) -> *mut TValue {
    if (key as u32).wrapping_sub(1) < (*t).sizearray as u32 {
        return (*t).array.add(key as usize - 1);
    }
    let p = luaH_getnum(t, key);
    if p != luaO_nilobject() {
        p as *mut TValue
    } else {
        let mut k = TValue::NIL;
        setnvalue(&mut k, key as f64);
        newkey_impl(L, t, &k, true, key)
    }
}

pub unsafe fn luaH_setstr(L: *mut lua_State, t: *mut LuaTable, key: *mut TString) -> *mut TValue {
    let p = luaH_getstr(t, key);
    invalidateTMcache(t);
    if p != luaO_nilobject() {
        p as *mut TValue
    } else {
        let mut k = TValue::NIL;
        setsvalue(&mut k, key);
        newkey_impl(L, t, &k, false, -1)
    }
}

pub unsafe fn luaH_setp(L: *mut lua_State, t: *mut LuaTable, key: *mut u8, tag: i32) -> *mut TValue {
    let p = luaH_getp(t, key, tag);
    if p != luaO_nilobject() {
        p as *mut TValue
    } else {
        let mut k = TValue::NIL;
        setpvalue(&mut k, key, tag);
        newkey_impl(L, t, &k, false, -1)
    }
}

/// `luaH_setslot`: store target for `key` given a previous `luaH_get`.
#[inline(always)]
pub unsafe fn luaH_setslot(L: *mut lua_State, t: *mut LuaTable, slot: *const TValue, key: *const TValue) -> *mut TValue {
    invalidateTMcache(t);
    if slot == luaO_nilobject() {
        luaH_newkey(L, t, key)
    } else {
        slot as *mut TValue
    }
}

unsafe fn updateaboundary(t: *mut LuaTable, boundary: i32) -> i32 {
    let arr = (*t).array;
    if boundary < (*t).sizearray && ttisnil(arr.add(boundary as usize - 1)) {
        if boundary >= 2 && !ttisnil(arr.add(boundary as usize - 2)) {
            maybesetaboundary(t, boundary - 1);
            return boundary - 1;
        }
    } else if boundary + 1 < (*t).sizearray && !ttisnil(arr.add(boundary as usize)) && ttisnil(arr.add(boundary as usize + 1)) {
        maybesetaboundary(t, boundary + 1);
        return boundary + 1;
    }
    0
}

/// `luaH_getn`; the cached-boundary fast paths are inlined into callers.
#[inline(always)]
pub unsafe fn luaH_getn(t: *mut LuaTable) -> i32 {
    let boundary = getaboundary(t);
    let arr = (*t).array;
    if boundary > 0 {
        if !ttisnil(arr.add((*t).sizearray as usize - 1)) && (*t).node == dummynode() {
            return (*t).sizearray;
        }
        if boundary < (*t).sizearray && !ttisnil(arr.add(boundary as usize - 1)) && ttisnil(arr.add(boundary as usize)) {
            return boundary;
        }
    }
    luaH_getn_slow(t, boundary)
}

#[inline(never)]
unsafe fn luaH_getn_slow(t: *mut LuaTable, boundary: i32) -> i32 {
    let arr = (*t).array;
    if boundary > 0 {
        let found = updateaboundary(t, boundary);
        if found > 0 {
            return found;
        }
    }
    let j = (*t).sizearray;
    if j > 0 && ttisnil(arr.add(j as usize - 1)) {
        let mut base = arr;
        let mut rest = j;
        loop {
            let half = rest >> 1;
            if half == 0 {
                break;
            }
            base = if ttisnil(base.add(half as usize)) { base } else { base.add(half as usize) };
            rest -= half;
        }
        let boundary = (!ttisnil(base)) as i32 + base.offset_from(arr) as i32;
        maybesetaboundary(t, boundary);
        boundary
    } else {
        j
    }
}

pub unsafe fn luaH_clone(L: *mut lua_State, tt: *mut LuaTable) -> *mut LuaTable {
    let t = luaM_newgco_(L, std::mem::size_of::<LuaTable>(), (*L).activememcat) as *mut LuaTable;
    luaC_init(L, t as *mut GCObject, LUA_TTABLE);
    (*t).metatable = (*tt).metatable;
    (*t).tmcache = (*tt).tmcache;
    (*t).array = ptr::null_mut();
    (*t).sizearray = 0;
    (*t).lsizenode = 0;
    (*t).nodemask8 = 0;
    (*t).readonly = 0;
    (*t).safeenv = 0;
    (*t).node = dummynode();
    (*t).lastfree = 0;
    (*t).gclist = ptr::null_mut();
    if (*tt).sizearray != 0 {
        (*t).array = luaM_newarray(L, (*tt).sizearray as usize, (*t).hdr.memcat);
        maybesetaboundary(t, getaboundary(tt));
        (*t).sizearray = (*tt).sizearray;
        ptr::copy_nonoverlapping((*tt).array, (*t).array, (*t).sizearray as usize);
    }
    if (*tt).node != dummynode() {
        let size = 1usize << (*tt).lsizenode;
        (*t).node = luaM_newarray(L, size, (*t).hdr.memcat);
        (*t).lsizenode = (*tt).lsizenode;
        (*t).nodemask8 = (*tt).nodemask8;
        ptr::copy_nonoverlapping((*tt).node, (*t).node, size);
        (*t).lastfree = (*tt).lastfree;
    }
    t
}

pub unsafe fn luaH_clear(tt: *mut LuaTable) {
    for i in 0..(*tt).sizearray {
        setnilvalue((*tt).array.add(i as usize));
    }
    maybesetaboundary(tt, 0);
    if (*tt).node != dummynode() {
        let size = sizenode(tt);
        (*tt).lastfree = size;
        for i in 0..size {
            let n = gnode(tt, i);
            (*n).key.ttnext = 0;
            setnilvalue(gval(n));
        }
    }
    (*tt).tmcache = !0;
}

pub unsafe fn luaH_setreadonly(L: *mut lua_State, t: *mut LuaTable, readonly: bool) {
    if luaH_getreadonly(t) == readonly as i32 {
        return;
    }
    if readonly {
        (*t).readonly = 1;
        if (*t).node == dummynode() {
            return;
        }
        let mut metamethods = [TValue::NIL; TM_N];
        let mut found = false;
        let mut tmcache: u8 = 0;
        for i in 0..TM_N {
            let val = luaH_getstr(t, (*(*L).global).tmname[i]);
            metamethods[TM_N - (i + 1)] = *val;
            let has = !ttisnil(val);
            found |= has;
            if i <= TM_EQ as usize && !has {
                tmcache |= 1u8 << i;
            }
        }
        (*t).tmcache = tmcache;
        if found {
            let memcat = (*t).hdr.memcat;
            let newarray = luaM_trynew_(L, ((*t).sizearray as usize + TM_N) * std::mem::size_of::<TValue>(), memcat) as *mut TValue;
            if !newarray.is_null() {
                let oldarray = (*t).array;
                if (*t).sizearray != 0 {
                    ptr::copy_nonoverlapping(oldarray, newarray.add(TM_N), (*t).sizearray as usize);
                }
                ptr::copy_nonoverlapping(metamethods.as_ptr(), newarray, TM_N);
                (*t).array = newarray.add(TM_N);
                (*t).readonly |= 2;
                if !oldarray.is_null() {
                    luaM_freearray(L, oldarray, (*t).sizearray as usize, memcat);
                }
            }
        }
    } else {
        if hasmetacache(t) {
            let memcat = (*t).hdr.memcat;
            let oldarray = (*t).array;
            let mut newarray: *mut TValue = ptr::null_mut();
            if (*t).sizearray != 0 {
                newarray = luaM_newarray(L, (*t).sizearray as usize, memcat);
                ptr::copy_nonoverlapping(oldarray, newarray, (*t).sizearray as usize);
            }
            (*t).array = newarray;
            luaM_freearray(L, oldarray.sub(TM_N), (*t).sizearray as usize + TM_N, memcat);
        }
        (*t).readonly = 0;
    }
}

#[inline(always)]
pub unsafe fn luaH_getreadonly(t: *mut LuaTable) -> i32 {
    ((*t).readonly != 0) as i32
}
