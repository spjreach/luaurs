//! Port of `VM/src/lgc.cpp`: incremental non-generational mark & sweep.
//!
//! Identical marking, atomic phase, barriers and pacing. Sweeping walks the
//! `global_State::allgc` object list (the C++ VM walks its allocator pages);
//! the cost model charges `GC_SWEEPPAGESTEPCOST` per visited object.

use std::ptr;

use crate::ldo::luaD_rawrunprotected;
use crate::lfunc::*;
use crate::lobject::*;
use crate::lstring::{luaS_free, luaS_resize};
use crate::ltable::*;
use crate::ltm::*;

pub const LUAI_GCGOAL: i32 = 200;
pub const LUAI_GCSTEPMUL: i32 = 200;
pub const LUAI_GCSTEPSIZE: i32 = 1;

pub const GCSpause: u8 = 0;
pub const GCSpropagate: u8 = 1;
pub const GCSpropagateagain: u8 = 2;
pub const GCSatomic: u8 = 3;
pub const GCSsweep: u8 = 4;

pub const WHITE0BIT: u8 = 0;
pub const WHITE1BIT: u8 = 1;
pub const BLACKBIT: u8 = 2;
pub const FIXEDBIT: u8 = 3;

#[inline(always)]
pub const fn bitmask(b: u8) -> u8 {
    1 << b
}
pub const WHITEBITS: u8 = bitmask(WHITE0BIT) | bitmask(WHITE1BIT);
const MASKMARKS: u8 = !(bitmask(BLACKBIT) | WHITEBITS);

const GC_SWEEPPAGESTEPCOST: usize = 16;

#[inline(always)]
pub unsafe fn keepinvariant(g: *const global_State) -> bool {
    let s = (*g).gcstate;
    s == GCSpropagate || s == GCSpropagateagain || s == GCSatomic
}
#[inline(always)]
pub unsafe fn iswhite(o: *const GCObject) -> bool {
    (*o).marked & WHITEBITS != 0
}
#[inline(always)]
pub unsafe fn isblack(o: *const GCObject) -> bool {
    (*o).marked & bitmask(BLACKBIT) != 0
}
#[inline(always)]
pub unsafe fn isgray(o: *const GCObject) -> bool {
    (*o).marked & (WHITEBITS | bitmask(BLACKBIT)) == 0
}
#[inline(always)]
pub unsafe fn isfixed(o: *const GCObject) -> bool {
    (*o).marked & bitmask(FIXEDBIT) != 0
}
#[inline(always)]
pub unsafe fn otherwhite(g: *const global_State) -> u8 {
    (*g).currentwhite ^ WHITEBITS
}
#[inline(always)]
pub unsafe fn isdead(g: *const global_State, o: *const GCObject) -> bool {
    ((*o).marked & (WHITEBITS | bitmask(FIXEDBIT))) == (otherwhite(g) & WHITEBITS)
}
#[inline(always)]
pub unsafe fn changewhite(o: *mut GCObject) {
    (*o).marked ^= WHITEBITS;
}
#[inline(always)]
unsafe fn gray2black(o: *mut GCObject) {
    (*o).marked |= bitmask(BLACKBIT);
}
#[inline(always)]
unsafe fn white2gray(o: *mut GCObject) {
    (*o).marked &= !WHITEBITS;
}
#[inline(always)]
unsafe fn black2gray(o: *mut GCObject) {
    (*o).marked &= !bitmask(BLACKBIT);
}
#[inline(always)]
unsafe fn stringmark(s: *mut TString) {
    (*s).hdr.marked &= !WHITEBITS;
    (*s).hdr.marked |= bitmask(BLACKBIT);
}
#[inline(always)]
pub unsafe fn luaC_white(g: *const global_State) -> u8 {
    (*g).currentwhite & WHITEBITS
}
#[inline(always)]
unsafe fn makewhite(g: *const global_State, o: *mut GCObject) {
    (*o).marked = ((*o).marked & MASKMARKS) | luaC_white(g);
}

#[inline(always)]
pub unsafe fn luaC_init(L: *mut lua_State, o: *mut GCObject, tt: i32) {
    (*o).marked = luaC_white((*L).global);
    (*o).tt = tt as u8;
    (*o).memcat = (*L).activememcat;
}

#[inline(always)]
pub unsafe fn luaC_needsGC(L: *mut lua_State) -> bool {
    let g = (*L).global;
    (*g).totalbytes >= (*g).GCthreshold
}

#[inline(always)]
pub unsafe fn luaC_checkGC(L: *mut lua_State) {
    if luaC_needsGC(L) {
        luaC_step(L, true);
    }
}

#[inline(always)]
pub unsafe fn luaC_barrier(L: *mut lua_State, p: *mut GCObject, v: *const TValue) {
    if iscollectable(v) && isblack(p) && iswhite(gcvalue(v)) {
        luaC_barrierf(L, p, gcvalue(v));
    }
}

#[inline(always)]
pub unsafe fn luaC_barriert(L: *mut lua_State, t: *mut LuaTable, v: *const TValue) {
    if iscollectable(v) && isblack(t as *mut GCObject) && iswhite(gcvalue(v)) {
        luaC_barriertable(L, t, gcvalue(v));
    }
}

#[inline(always)]
pub unsafe fn luaC_barrierfast(L: *mut lua_State, t: *mut LuaTable) {
    if isblack(t as *mut GCObject) {
        luaC_barrierback(L, t as *mut GCObject, ptr::addr_of_mut!((*t).gclist));
    }
}

#[inline(always)]
pub unsafe fn luaC_objbarrier(L: *mut lua_State, p: *mut GCObject, o: *mut GCObject) {
    if isblack(p) && iswhite(o) {
        luaC_barrierf(L, p, o);
    }
}

#[inline(always)]
pub unsafe fn luaC_threadbarrier(L: *mut lua_State) {
    if isblack(L as *mut GCObject) {
        luaC_barrierback(L, L as *mut GCObject, ptr::addr_of_mut!((*L).gclist));
    }
}

#[inline(always)]
unsafe fn markvalue(g: *mut global_State, o: *const TValue) {
    if iscollectable(o) && iswhite(gcvalue(o)) {
        reallymarkobject(g, gcvalue(o));
    }
}

#[inline(always)]
unsafe fn markobject<T>(g: *mut global_State, t: *mut T) {
    let o = t as *mut GCObject;
    if iswhite(o) {
        reallymarkobject(g, o);
    }
}

unsafe fn removeentry(n: *mut LuaNode) {
    if (*n).key.tt() >= LUA_TSTRING {
        (*n).key.set_tt(LUA_TDEADKEY);
    }
}

unsafe fn reallymarkobject(g: *mut global_State, o: *mut GCObject) {
    white2gray(o);
    match (*o).tt as i32 {
        LUA_TSTRING => {
            gray2black(o);
        }
        LUA_TUSERDATA => {
            gray2black(o);
            let mt = (*(o as *mut Udata)).metatable;
            if !mt.is_null() {
                markobject(g, mt);
            }
        }
        LUA_TUPVAL => {
            let uv = o as *mut UpVal;
            markvalue(g, (*uv).v);
            if !upisopen(uv) {
                gray2black(o);
            }
        }
        LUA_TFUNCTION => {
            (*(o as *mut Closure)).gclist = (*g).gray;
            (*g).gray = o;
        }
        LUA_TTABLE => {
            (*(o as *mut LuaTable)).gclist = (*g).gray;
            (*g).gray = o;
        }
        LUA_TTHREAD => {
            (*(o as *mut lua_State)).gclist = (*g).gray;
            (*g).gray = o;
        }
        LUA_TBUFFER => {
            gray2black(o);
        }
        LUA_TPROTO => {
            (*(o as *mut Proto)).gclist = (*g).gray;
            (*g).gray = o;
        }
        LUA_TCLASS => {
            (*(o as *mut LuauClass)).gclist = (*g).gray;
            (*g).gray = o;
        }
        LUA_TOBJECT => {
            (*(o as *mut LuauObject)).gclist = (*g).gray;
            (*g).gray = o;
        }
        _ => debug_assert!(false, "unexpected GC object type"),
    }
}

/// `gettablemode`: the `__mode` string of `h`'s metatable, if any.
unsafe fn gettablemode<'a>(g: *mut global_State, h: *mut LuaTable) -> Option<&'a [u8]> {
    let mt = (*h).metatable;
    if mt.is_null() || ((*mt).tmcache & (1u8 << TM_MODE)) != 0 {
        return None;
    }
    let mode = if hasmetacache(mt) {
        getmetacache(mt, TM_MODE) as *const TValue
    } else {
        luaT_gettm(mt, TM_MODE, (*g).tmname[TM_MODE as usize])
    };
    if !mode.is_null() && ttisstring(mode) {
        Some(sbytes(tsvalue(mode)))
    } else {
        None
    }
}

unsafe fn traversetable(g: *mut global_State, h: *mut LuaTable) -> bool {
    let mut weakkey = false;
    let mut weakvalue = false;
    if !(*h).metatable.is_null() {
        markobject(g, (*h).metatable);
    }
    if let Some(modev) = gettablemode(g, h) {
        weakkey = modev.contains(&b'k');
        weakvalue = modev.contains(&b'v');
        if weakkey || weakvalue {
            (*h).gclist = (*g).weak;
            (*g).weak = h as *mut GCObject;
        }
    }
    if weakkey && weakvalue {
        return true;
    }
    if !weakvalue {
        let mut i = (*h).sizearray;
        while i > 0 {
            i -= 1;
            markvalue(g, (*h).array.add(i as usize));
        }
    }
    let mut i = sizenode(h);
    while i > 0 {
        i -= 1;
        let n = gnode(h, i);
        if ttisnil(gval(n)) {
            removeentry(n);
        } else {
            if !weakkey {
                let mut k = TValue::NIL;
                getnodekey(&mut k, n);
                markvalue(g, &k);
            }
            if !weakvalue {
                markvalue(g, gval(n));
            }
        }
    }
    weakkey || weakvalue
}

unsafe fn traverseproto(g: *mut global_State, f: *mut Proto) {
    if !(*f).source.is_null() {
        stringmark((*f).source);
    }
    if !(*f).debugname.is_null() {
        stringmark((*f).debugname);
    }
    for i in 0..(*f).sizek as usize {
        markvalue(g, (*f).k.add(i));
    }
    for i in 0..(*f).sizeupvalues as usize {
        let s = *(*f).upvalues.add(i);
        if !s.is_null() {
            stringmark(s);
        }
    }
    for i in 0..(*f).sizep as usize {
        let p = *(*f).p.add(i);
        if !p.is_null() {
            markobject(g, p);
        }
    }
    for i in 0..(*f).sizelocvars as usize {
        let s = (*(*f).locvars.add(i)).varname;
        if !s.is_null() {
            stringmark(s);
        }
    }
}

unsafe fn traverseclosure(g: *mut global_State, cl: *mut Closure) {
    markobject(g, (*cl).env);
    if (*cl).isC != 0 {
        let up = Closure::cupvals(cl);
        for i in 0..(*cl).nupvalues as usize {
            markvalue(g, up.add(i));
        }
    } else {
        markobject(g, Closure::proto(cl));
        let up = Closure::uprefs(cl);
        for i in 0..(*cl).nupvalues as usize {
            markvalue(g, up.add(i));
        }
    }
}

unsafe fn traversestack(g: *mut global_State, l: *mut lua_State) {
    markobject(g, (*l).gt);
    if !(*l).namecall.is_null() {
        stringmark((*l).namecall);
    }
    if !(*l).finalizers.is_null() {
        markobject(g, (*l).finalizers);
    }
    let mut o = (*l).stack;
    while o < (*l).top {
        markvalue(g, o);
        o = o.add(1);
    }
    let mut uv = (*l).openupval;
    while !uv.is_null() {
        (*uv).markedopen = 1;
        markobject(g, uv);
        uv = (*uv).u.open.threadnext;
    }
}

unsafe fn traverseclass(g: *mut global_State, c: *mut LuauClass) {
    markobject(g, (*c).name);
    if !(*c).super_.is_null() {
        markobject(g, (*c).super_);
    }
    markobject(g, (*c).memberstooffset);
    for i in 0..crate::lclass::luaR_offsettomemberlength(c) as usize {
        let s = *(*c).offsettomember.add(i);
        if !s.is_null() {
            markobject(g, s);
        }
    }
    for i in 0..crate::lclass::luaR_numstaticmembers(c) as usize {
        markvalue(g, (*c).staticmembers.add(i));
    }
    if !(*c).metatable.is_null() {
        markobject(g, (*c).metatable);
    }
    if !(*c).instancemetatable.is_null() {
        markobject(g, (*c).instancemetatable);
    }
}

unsafe fn traverseobject(g: *mut global_State, o: *mut LuauObject) {
    markobject(g, (*o).lclass);
    for i in 0..(*o).numberofmembers as usize {
        markvalue(g, (*o).members.add(i));
    }
}

unsafe fn clearstack(l: *mut lua_State) {
    let stack_end = (*l).stack.add((*l).stacksize as usize);
    let mut o = (*l).top;
    while o < stack_end {
        setnilvalue(o);
        o = o.add(1);
    }
}

unsafe fn shrinkstack(L: *mut lua_State) {
    let mut lim = (*L).top;
    let mut ci = (*L).base_ci;
    while ci <= (*L).ci {
        if lim < (*ci).top {
            lim = (*ci).top;
        }
        ci = ci.add(1);
    }
    let ci_used = (*L).ci.offset_from((*L).base_ci) as i32;
    let s_used = lim.offset_from((*L).stack) as i32;
    if (*L).size_ci > LUAI_MAXCALLS {
        return;
    }
    if 3 * (ci_used as usize) < (*L).size_ci as usize && 2 * BASIC_CI_SIZE < (*L).size_ci {
        crate::ldo::luaD_reallocCI(L, (*L).size_ci / 2);
    }
    if 3 * (s_used as usize) < (*L).stacksize as usize && 2 * (BASIC_STACK_SIZE + EXTRA_STACK) < (*L).stacksize {
        crate::ldo::luaD_reallocstack(L, (*L).stacksize / 2, false);
    }
}

unsafe fn shrinkstackprotected(L: *mut lua_State) {
    let _ = luaD_rawrunprotected(L, &mut || shrinkstack(L));
}

unsafe fn table_cost(h: *const LuaTable) -> usize {
    std::mem::size_of::<LuaTable>()
        + std::mem::size_of::<TValue>() * (*h).sizearray as usize
        + std::mem::size_of::<LuaNode>() * if (*h).node == dummynode() { 0 } else { sizenode(h) as usize }
}

unsafe fn propagatemark(g: *mut global_State) -> usize {
    let o = (*g).gray;
    gray2black(o);
    match (*o).tt as i32 {
        LUA_TTABLE => {
            let h = o as *mut LuaTable;
            (*g).gray = (*h).gclist;
            if traversetable(g, h) {
                black2gray(o);
            }
            table_cost(h)
        }
        LUA_TFUNCTION => {
            let cl = o as *mut Closure;
            (*g).gray = (*cl).gclist;
            traverseclosure(g, cl);
            if (*cl).isC != 0 {
                sizeCclosure((*cl).nupvalues as usize)
            } else {
                sizeLclosure((*cl).nupvalues as usize)
            }
        }
        LUA_TTHREAD => {
            let th = o as *mut lua_State;
            (*g).gray = (*th).gclist;
            let active = (*th).isactive || th == (*(*th).global).mainthread;
            traversestack(g, th);
            if active {
                (*th).gclist = (*g).grayagain;
                (*g).grayagain = o;
                black2gray(o);
            }
            if !active || (*g).gcstate == GCSatomic {
                clearstack(th);
            }
            if (*g).gcstate == GCSpropagate {
                shrinkstackprotected(th);
            }
            std::mem::size_of::<lua_State>()
                + std::mem::size_of::<TValue>() * (*th).stacksize as usize
                + std::mem::size_of::<CallInfo>() * (*th).size_ci as usize
        }
        LUA_TPROTO => {
            let p = o as *mut Proto;
            (*g).gray = (*p).gclist;
            traverseproto(g, p);
            std::mem::size_of::<Proto>()
                + 4 * (*p).sizecode as usize
                + 8 * (*p).sizep as usize
                + std::mem::size_of::<TValue>() * (*p).sizek as usize
                + (*p).sizelineinfo as usize
                + std::mem::size_of::<LocVar>() * (*p).sizelocvars as usize
                + 8 * (*p).sizeupvalues as usize
                + (*p).sizetypeinfo as usize
        }
        LUA_TCLASS => {
            let c = o as *mut LuauClass;
            (*g).gray = (*c).gclist;
            traverseclass(g, c);
            std::mem::size_of::<LuauClass>()
                + crate::lclass::luaR_numstaticmembers(c) as usize * std::mem::size_of::<TValue>()
                + crate::lclass::luaR_offsettomemberlength(c) as usize * 8
        }
        LUA_TOBJECT => {
            let ob = o as *mut LuauObject;
            (*g).gray = (*ob).gclist;
            traverseobject(g, ob);
            std::mem::size_of::<LuauObject>() + (*ob).numberofmembers as usize * std::mem::size_of::<TValue>()
        }
        _ => 0,
    }
}

unsafe fn propagateall(g: *mut global_State) -> usize {
    let mut work = 0;
    while !(*g).gray.is_null() {
        work += propagatemark(g);
    }
    work
}

unsafe fn isobjcleared(o: *mut GCObject) -> bool {
    if (*o).tt as i32 == LUA_TSTRING {
        stringmark(o as *mut TString);
        return false;
    }
    iswhite(o)
}

#[inline(always)]
unsafe fn iscleared_tt(tt: i32, gc: *mut GCObject) -> bool {
    tt >= LUA_TSTRING && isobjcleared(gc)
}

unsafe fn rebuildmetacache(L: *mut lua_State, h: *mut LuaTable) {
    let mut tmcache: u8 = 0;
    for event in 0..TM_N as i32 {
        let value = luaH_getstr(h, (*(*L).global).tmname[event as usize]);
        *getmetacache(h, event) = *value;
        if event <= TM_EQ && ttisnil(value) {
            tmcache |= 1u8 << event;
        }
    }
    (*h).tmcache = tmcache;
}

unsafe fn cleartable(L: *mut lua_State, mut l: *mut GCObject) -> usize {
    let mut work = 0;
    let g = (*L).global;
    while !l.is_null() {
        let h = l as *mut LuaTable;
        work += table_cost(h);
        let mut i = (*h).sizearray;
        while i > 0 {
            i -= 1;
            let o = (*h).array.add(i as usize);
            if iscleared_tt((*o).tt, (*o).value.gc) {
                setnilvalue(o);
            }
        }
        let mut i = sizenode(h);
        let mut activevalues = 0;
        while i > 0 {
            i -= 1;
            let n = gnode(h, i);
            if !ttisnil(gval(n)) {
                let kt = (*n).key.tt();
                if iscleared_tt(kt, (*n).key.value.gc) || iscleared_tt((*gval(n)).tt, (*gval(n)).value.gc) {
                    setnilvalue(gval(n));
                    removeentry(n);
                } else {
                    activevalues += 1;
                }
            }
        }
        if hasmetacache(h) {
            rebuildmetacache(L, h);
        }
        if (*h).readonly == 0 {
            if let Some(modev) = gettablemode(g, h) {
                if modev.contains(&b's') && activevalues < sizenode(h) * 3 / 8 {
                    let _ = luaD_rawrunprotected(L, &mut || luaH_resizehash(L, h, activevalues));
                }
            }
        }
        l = (*h).gclist;
    }
    work
}

unsafe fn freeobj(L: *mut lua_State, o: *mut GCObject) {
    match (*o).tt as i32 {
        LUA_TPROTO => luaF_freeproto(L, o as *mut Proto),
        LUA_TFUNCTION => luaF_freeclosure(L, o as *mut Closure),
        LUA_TUPVAL => luaF_freeupval(L, o as *mut UpVal),
        LUA_TTABLE => luaH_free(L, o as *mut LuaTable),
        LUA_TTHREAD => crate::lstate::luaE_freethread(L, o as *mut lua_State),
        LUA_TSTRING => luaS_free(L, o as *mut TString),
        LUA_TUSERDATA => luaU_freeudata(L, o as *mut Udata),
        LUA_TBUFFER => luaB_freebuffer(L, o as *mut Buffer),
        LUA_TCLASS => crate::lclass::luaR_freeclass(L, o as *mut LuauClass),
        LUA_TOBJECT => crate::lclass::luaR_freeobject(L, o as *mut LuauObject),
        _ => debug_assert!(false),
    }
}

unsafe fn shrinkbuffers(L: *mut lua_State) {
    let g = (*L).global;
    if (*g).strt.nuse < ((*g).strt.size / 4) as u32 && (*g).strt.size > LUA_MINSTRTABSIZE * 2 {
        let ns = (*g).strt.size / 2;
        let _ = luaD_rawrunprotected(L, &mut || luaS_resize(L, ns));
    }
}

unsafe fn shrinkbuffersfull(L: *mut lua_State) {
    let g = (*L).global;
    let mut hashsize = (*g).strt.size;
    while (*g).strt.nuse < (hashsize / 4) as u32 && hashsize > LUA_MINSTRTABSIZE * 2 {
        hashsize /= 2;
    }
    if hashsize != (*g).strt.size {
        let _ = luaD_rawrunprotected(L, &mut || luaS_resize(L, hashsize));
    }
}

/// `luaC_freeall`: frees every collectable object (state shutdown).
pub unsafe fn luaC_freeall(L: *mut lua_State) {
    GC_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let g = (*L).global;
    crate::lmem::luaM_visitgco(L, &mut |page, o| {
        (*g).freepage = page;
        freeobj(L, o);
    });
}

unsafe fn markmt(g: *mut global_State) {
    for i in 0..LUA_T_COUNT {
        if !(*g).mt[i].is_null() {
            markobject(g, (*g).mt[i]);
        }
    }
}

unsafe fn marktaggetmt(g: *mut global_State) {
    for i in 0..LUA_UTAG_LIMIT {
        if !(*g).udatamt[i].is_null() {
            markobject(g, (*g).udatamt[i]);
        }
    }
}

unsafe fn markfastpcalls(g: *mut global_State) {
    if !(*g).builtinPcall.is_null() {
        markobject(g, (*g).builtinPcall);
    }
    if !(*g).builtinXpcall.is_null() {
        markobject(g, (*g).builtinXpcall);
    }
}

unsafe fn markroot(L: *mut lua_State) {
    let g = (*L).global;
    (*g).gray = ptr::null_mut();
    (*g).grayagain = ptr::null_mut();
    (*g).weak = ptr::null_mut();
    markobject(g, (*g).mainthread);
    markobject(g, (*(*g).mainthread).gt);
    markvalue(g, registry(L));
    markfastpcalls(g);
    markmt(g);
    marktaggetmt(g);
    for i in 0..LUA_LUTAG_LIMIT {
        let s = (*g).lightuserdataname[i];
        if !s.is_null() {
            markobject(g, s);
        }
    }
    (*g).gcstate = GCSpropagate;
}

unsafe fn remarkupvals(g: *mut global_State) -> usize {
    let mut work = 0;
    let head = ptr::addr_of_mut!((*g).uvhead);
    let mut uv = (*head).u.open.next;
    while uv != head {
        work += std::mem::size_of::<UpVal>();
        if isgray(uv as *mut GCObject) {
            markvalue(g, (*uv).v);
        }
        uv = (*uv).u.open.next;
    }
    work
}

unsafe fn clearupvals(L: *mut lua_State) -> usize {
    let g = (*L).global;
    let mut work = 0;
    let head = ptr::addr_of_mut!((*g).uvhead);
    let mut uv = (*head).u.open.next;
    while uv != head {
        work += std::mem::size_of::<UpVal>();
        if (*uv).markedopen != 0 {
            (*uv).markedopen = 0;
            uv = (*uv).u.open.next;
        } else {
            let next = (*uv).u.open.next;
            luaF_closeupval(L, uv, iswhite(uv as *mut GCObject));
            uv = next;
        }
    }
    work
}

/// Counts atomic phases (and state shutdowns) of every state: an object
/// can only be freed by a sweep after one, so a pointer seen alive with
/// the count unchanged is still alive (caches of interned strings use it).
pub static GC_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

unsafe fn atomic(L: *mut lua_State) -> usize {
    GC_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let g = (*L).global;
    let mut work = 0;
    work += remarkupvals(g);
    work += propagateall(g);
    (*g).gray = (*g).weak;
    (*g).weak = ptr::null_mut();
    markobject(g, L);
    markmt(g);
    marktaggetmt(g);
    markfastpcalls(g);
    work += propagateall(g);
    (*g).gray = (*g).grayagain;
    (*g).grayagain = ptr::null_mut();
    work += propagateall(g);
    work += cleartable(L, (*g).weak);
    (*g).weak = ptr::null_mut();
    work += clearupvals(L);
    (*g).currentwhite = otherwhite(g);
    (*g).sweepgcopage = (*g).allgcopages;
    (*g).gcstate = GCSsweep;
    work
}

/// `sweepgcopage`: sweeps one GC page; returns the number of blocks visited.
unsafe fn sweepgcopage(L: *mut lua_State, page: *mut crate::lmem::lua_Page) -> usize {
    let g = (*L).global;
    let (start, end, mut busy, block_size) = crate::lmem::luaM_getpagewalkinfo(page);
    let deadmask = otherwhite(g);
    let newwhite = luaC_white(g);
    let mut pos = start;
    while pos != end {
        let o = pos as *mut GCObject;
        pos = pos.add(block_size);
        if (*o).tt as i32 == LUA_TNIL {
            continue;
        }
        if ((*o).marked ^ WHITEBITS) & deadmask != 0 {
            (*o).marked = ((*o).marked & MASKMARKS) | newwhite;
        } else {
            (*g).freepage = page;
            freeobj(L, o);
            busy -= 1;
            // the last block of the page frees the page itself
            if busy == 0 {
                return (pos as usize - start as usize) / block_size;
            }
        }
    }
    (end as usize - start as usize) / block_size
}

/// Sweeps pages from the cursor until `limit` is reached; true when done.
unsafe fn sweepstep(L: *mut lua_State, cost: &mut usize, limit: usize) -> bool {
    let g = (*L).global;
    while !(*g).sweepgcopage.is_null() && *cost < limit {
        let page = (*g).sweepgcopage;
        let next = crate::lmem::luaM_getnextpage(page);
        let steps = sweepgcopage(L, page);
        (*g).sweepgcopage = next;
        *cost += steps * GC_SWEEPPAGESTEPCOST;
    }
    (*g).sweepgcopage.is_null()
}

unsafe fn gcstep(L: *mut lua_State, limit: usize) -> usize {
    let mut cost = 0;
    let g = (*L).global;
    match (*g).gcstate {
        GCSpause => {
            markroot(L);
        }
        GCSpropagate => {
            while !(*g).gray.is_null() && cost < limit {
                cost += propagatemark(g);
            }
            if (*g).gray.is_null() {
                (*g).gray = (*g).grayagain;
                (*g).grayagain = ptr::null_mut();
                (*g).gcstate = GCSpropagateagain;
            }
        }
        GCSpropagateagain => {
            while !(*g).gray.is_null() && cost < limit {
                cost += propagatemark(g);
            }
            if (*g).gray.is_null() {
                (*g).gcstate = GCSatomic;
            }
        }
        GCSatomic => {
            (*g).gcstats.atomicstarttimestamp = crate::los::lua_clock();
            (*g).gcstats.atomicstarttotalsizebytes = (*g).totalbytes;
            cost = atomic(L);
        }
        GCSsweep => {
            if sweepstep(L, &mut cost, limit) {
                makewhite(g, (*g).mainthread as *mut GCObject);
                shrinkbuffers(L);
                (*g).gcstate = GCSpause;
            }
        }
        _ => {}
    }
    cost
}

unsafe fn getheaptriggererroroffset(g: *mut global_State) -> i64 {
    let st = &mut (*g).gcstats;
    let error_kb = ((st.atomicstarttotalsizebytes.wrapping_sub(st.heapgoalsizebytes)) as isize / 1024) as i32;
    let n = st.triggerterms.len() as u32;
    let slot = &mut st.triggerterms[(st.triggertermpos % n) as usize];
    let prev = *slot;
    *slot = error_kb;
    st.triggerintegral = st.triggerintegral.wrapping_add(error_kb.wrapping_sub(prev));
    st.triggertermpos = st.triggertermpos.wrapping_add(1);
    const KU: f64 = 0.9;
    const TU: f64 = 2.5;
    const KP: f64 = 0.45 * KU;
    const TI: f64 = 0.8 * TU;
    const KI: f64 = 0.54 * KU / TI;
    let total = KP * error_kb as f64 + KI * st.triggerintegral as f64;
    (total * 1024.0) as i64
}

fn getheapgrowth(current: usize, previous: usize) -> usize {
    current.saturating_sub(previous)
}

unsafe fn getheaptrigger(g: *mut global_State, heapgoal: usize) -> usize {
    const DURATIONTHRESHOLD: f64 = 1e-3;
    let st = (*g).gcstats;
    let allocationduration = st.atomicstarttimestamp - st.endtimestamp;
    if allocationduration < DURATIONTHRESHOLD {
        return heapgoal;
    }
    let allocationrate = getheapgrowth(st.atomicstarttotalsizebytes, st.endtotalsizebytes) as f64 / allocationduration;
    let markduration = st.atomicstarttimestamp - st.starttimestamp;
    let expectedgrowth = (markduration * allocationrate) as i64;
    let offset = getheaptriggererroroffset(g);
    let heaptrigger = heapgoal as i64 - (expectedgrowth + offset);
    if heaptrigger < (*g).totalbytes as i64 {
        (*g).totalbytes
    } else if heaptrigger > heapgoal as i64 {
        heapgoal
    } else {
        heaptrigger as usize
    }
}

pub unsafe fn luaC_step(L: *mut lua_State, assist: bool) -> usize {
    let g = (*L).global;
    if (*g).gcdisabled != 0 {
        return 0;
    }
    let mut lim = (*g).gcstepsize as usize * (*g).gcstepmul as usize / 100;
    let debt = (*g).totalbytes.saturating_sub((*g).GCthreshold);
    if let Some(interrupt) = (*g).cb.interrupt {
        interrupt(L, 0);
    }
    if (*g).gcstate == GCSpause {
        (*g).gcstats.starttimestamp = crate::los::lua_clock();
    }
    if assist {
        let need = debt * (*g).gcstepmul as usize / 100;
        if need > lim {
            lim = need;
        }
    }
    let lastgcstate = (*g).gcstate;
    let work = gcstep(L, lim);
    let actualstepsize = work * 100 / (*g).gcstepmul as usize;
    if (*g).gcstate == GCSpause {
        let heapgoal = ((*g).totalbytes / 100) * (*g).gcgoal as usize;
        let heaptrigger = getheaptrigger(g, heapgoal);
        (*g).GCthreshold = heaptrigger;
        (*g).gcstats.heapgoalsizebytes = heapgoal;
        (*g).gcstats.endtimestamp = crate::los::lua_clock();
        (*g).gcstats.endtotalsizebytes = (*g).totalbytes;
    } else {
        (*g).GCthreshold = (*g).totalbytes + actualstepsize;
        if (*g).GCthreshold >= debt {
            (*g).GCthreshold -= debt;
        }
    }
    if let Some(interrupt) = (*g).cb.interrupt {
        interrupt(L, lastgcstate as i32);
    }
    actualstepsize
}

pub unsafe fn luaC_fullgc(L: *mut lua_State) {
    let g = (*L).global;
    if keepinvariant(g) {
        (*g).sweepgcopage = (*g).allgcopages;
        (*g).gray = ptr::null_mut();
        (*g).grayagain = ptr::null_mut();
        (*g).weak = ptr::null_mut();
        (*g).gcstate = GCSsweep;
    }
    while (*g).gcstate != GCSpause {
        gcstep(L, usize::MAX);
    }
    let head = ptr::addr_of_mut!((*g).uvhead);
    let mut uv = (*head).u.open.next;
    while uv != head {
        (*uv).markedopen = 0;
        uv = (*uv).u.open.next;
    }
    markroot(L);
    while (*g).gcstate != GCSpause {
        gcstep(L, usize::MAX);
    }
    shrinkbuffersfull(L);
    let heapgoalsizebytes = ((*g).totalbytes / 100) * (*g).gcgoal as usize;
    let mul = (*g).gcstepmul as usize;
    let factor = ((*g).gcgoal as usize * mul / 100).saturating_sub(100);
    (*g).GCthreshold = (*g).totalbytes * factor / mul;
    if (*g).GCthreshold < (*g).totalbytes {
        (*g).GCthreshold = (*g).totalbytes;
    }
    (*g).gcstats.heapgoalsizebytes = heapgoalsizebytes;
    (*g).gcstats.endtimestamp = crate::los::lua_clock();
    (*g).gcstats.endtotalsizebytes = (*g).totalbytes;
}

pub unsafe fn luaC_barrierf(L: *mut lua_State, o: *mut GCObject, v: *mut GCObject) {
    let g = (*L).global;
    if keepinvariant(g) {
        reallymarkobject(g, v);
    } else {
        makewhite(g, o);
    }
}

pub unsafe fn luaC_barriertable(L: *mut lua_State, t: *mut LuaTable, v: *mut GCObject) {
    let g = (*L).global;
    let o = t as *mut GCObject;
    if (*g).gcstate == GCSpropagateagain {
        reallymarkobject(g, v);
        return;
    }
    black2gray(o);
    (*t).gclist = (*g).grayagain;
    (*g).grayagain = o;
}

pub unsafe fn luaC_barrierback(L: *mut lua_State, o: *mut GCObject, gclist: *mut *mut GCObject) {
    let g = (*L).global;
    black2gray(o);
    *gclist = (*g).grayagain;
    (*g).grayagain = o;
}

pub unsafe fn luaC_upvalclosed(L: *mut lua_State, uv: *mut UpVal) {
    let g = (*L).global;
    let o = uv as *mut GCObject;
    if isgray(o) {
        if keepinvariant(g) {
            gray2black(o);
            luaC_barrier(L, o, (*uv).v);
        } else {
            makewhite(g, o);
        }
    }
}

pub unsafe fn luaC_allocationrate(L: *mut lua_State) -> i64 {
    let g = (*L).global;
    const DURATIONTHRESHOLD: f64 = 1e-3;
    let st = (*g).gcstats;
    if (*g).gcstate <= GCSatomic {
        let duration = crate::los::lua_clock() - st.endtimestamp;
        if duration < DURATIONTHRESHOLD {
            return -1;
        }
        return (getheapgrowth((*g).totalbytes, st.endtotalsizebytes) as f64 / duration) as i64;
    }
    let duration = st.atomicstarttimestamp - st.endtimestamp;
    if duration < DURATIONTHRESHOLD {
        return -1;
    }
    (getheapgrowth(st.atomicstarttotalsizebytes, st.endtotalsizebytes) as f64 / duration) as i64
}

pub fn luaC_statename(state: i32) -> Option<&'static str> {
    match state as u8 {
        GCSpause => Some("pause"),
        GCSpropagate => Some("mark"),
        GCSpropagateagain => Some("remark"),
        GCSatomic => Some("atomic"),
        GCSsweep => Some("sweep"),
        _ => None,
    }
}
