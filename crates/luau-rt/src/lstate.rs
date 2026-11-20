//! Port of `VM/src/lstate.cpp`: global state and thread creation.

use std::ptr;

use crate::ldo::*;
use crate::lfunc::luaF_close;
use crate::lgc::*;
use crate::lmem::*;
use crate::lobject::*;
use crate::lstring::{luaS_fix, luaS_new, luaS_resize};
use crate::ltable::luaH_new;
use crate::ltm::luaT_init;

/// Main thread + global state (`LG`).
#[repr(C)]
struct LG {
    l: lua_State,
    g: global_State,
}

unsafe fn stack_init(L1: *mut lua_State, L: *mut lua_State) {
    (*L1).base_ci = luaM_newarray(L, BASIC_CI_SIZE as usize, (*L1).hdr.memcat);
    (*L1).ci = (*L1).base_ci;
    (*L1).size_ci = BASIC_CI_SIZE;
    (*L1).end_ci = (*L1).base_ci.add((*L1).size_ci as usize - 1);
    let n = (BASIC_STACK_SIZE + EXTRA_STACK) as usize;
    (*L1).stack = luaM_newarray(L, n, (*L1).hdr.memcat);
    (*L1).stacksize = n as i32;
    let stack = (*L1).stack;
    for i in 0..n {
        *stack.add(i) = TValue::NIL;
    }
    (*L1).top = stack;
    (*L1).stack_last = stack.add(((*L1).stacksize - EXTRA_STACK) as usize);
    let ci = (*L1).ci;
    *ci = CallInfo {
        base: ptr::null_mut(),
        func: (*L1).top,
        top: ptr::null_mut(),
        p: ptr::null_mut(),
        savedpc: ptr::null(),
        nresults: 0,
        flags: 0,
    };
    setnilvalue((*L1).top);
    (*L1).top = (*L1).top.add(1);
    (*L1).base = (*L1).top;
    (*ci).base = (*L1).top;
    (*ci).top = (*L1).top.add(LUA_MINSTACK as usize);
}

unsafe fn freestack(L: *mut lua_State, L1: *mut lua_State) {
    luaM_freearray(L, (*L1).base_ci, (*L1).size_ci as usize, (*L1).hdr.memcat);
    luaM_freearray(L, (*L1).stack, (*L1).stacksize as usize, (*L1).hdr.memcat);
}

unsafe fn f_luaopen(L: *mut lua_State) {
    let g = (*L).global;
    stack_init(L, L);
    (*L).gt = luaH_new(L, 0, 2);
    sethvalue(registry(L), luaH_new(L, 0, 2));
    luaS_resize(L, LUA_MINSTRTABSIZE);
    luaT_init(L);
    luaS_fix(luaS_new(L, LUA_MEMERRMSG));
    luaS_fix(luaS_new(L, LUA_ERRERRMSG));
    (*g).GCthreshold = 4 * (*g).totalbytes;
}

unsafe fn close_state(L: *mut lua_State) {
    let g = (*L).global;
    luaF_close(L, (*L).stack);
    luaC_freeall(L);
    if !(*g).strt.hash.is_null() {
        luaM_freearray(L, (*g).strt.hash, (*g).strt.size as usize, 0);
    }
    if !(*L).stack.is_null() {
        freestack(L, L);
    }
    if !(*g).strbuf_ptr.is_null() {
        drop(Vec::from_raw_parts((*g).strbuf_ptr, 0, (*g).strbuf_cap));
        (*g).strbuf_ptr = std::ptr::null_mut();
    }
    drop(Box::from_raw(L as *mut LG));
}

pub unsafe fn luaE_newthread(L: *mut lua_State) -> *mut lua_State {
    let L1 = luaM_newgco_(L, std::mem::size_of::<lua_State>(), (*L).activememcat) as *mut lua_State;
    ptr::write_bytes(L1 as *mut u8, 0, std::mem::size_of::<lua_State>());
    luaC_init(L, L1 as *mut GCObject, LUA_TTHREAD);
    (*L1).global = (*L).global;
    (*L1).activememcat = (*L).activememcat;
    stack_init(L1, L);
    (*L1).gt = (*L).gt;
    (*L1).singlestep = (*L).singlestep;
    L1
}

pub unsafe fn luaE_freethread(L: *mut lua_State, L1: *mut lua_State) {
    let g = (*L).global;
    if let Some(cb) = (*g).cb.userthread {
        cb(ptr::null_mut(), L1);
    }
    freestack(L, L1);
    luaM_freegco_(L, L1 as *mut GCObject, std::mem::size_of::<lua_State>(), (*L1).hdr.memcat);
}

pub unsafe fn lua_resetthread(L: *mut lua_State) {
    luaF_close(L, (*L).stack);
    let ci = (*L).base_ci;
    (*ci).p = ptr::null_mut();
    (*ci).func = (*L).stack;
    (*ci).base = (*ci).func.add(1);
    (*ci).top = (*ci).base.add(LUA_MINSTACK as usize);
    setnilvalue((*ci).func);
    (*L).ci = ci;
    if (*L).size_ci != BASIC_CI_SIZE {
        luaD_reallocCI(L, BASIC_CI_SIZE);
    }
    (*L).status = LUA_OK as u8;
    (*L).base = (*(*L).ci).base;
    (*L).top = (*(*L).ci).base;
    (*L).nCcalls = 0;
    (*L).baseCcalls = 0;
    if (*L).stacksize != BASIC_STACK_SIZE + EXTRA_STACK {
        luaD_reallocstack(L, BASIC_STACK_SIZE, false);
    }
    for i in 0..(*L).stacksize as usize {
        setnilvalue((*L).stack.add(i));
    }
    (*L).finalizers = ptr::null_mut();
}

pub unsafe fn lua_isthreadreset(L: *mut lua_State) -> i32 {
    ((*L).ci == (*L).base_ci && (*L).base == (*L).top && (*L).status == LUA_OK as u8) as i32
}

/// `lua_newstate` with the default allocator.
pub unsafe fn lua_newstate() -> *mut lua_State {
    let lg: Box<LG> = Box::new(std::mem::zeroed());
    let lg = Box::into_raw(lg);
    let L = ptr::addr_of_mut!((*lg).l);
    let g = ptr::addr_of_mut!((*lg).g);
    (*L).hdr.tt = LUA_TTHREAD as u8;
    (*g).currentwhite = bitmask(WHITE0BIT) | bitmask(FIXEDBIT);
    (*L).hdr.marked = (*g).currentwhite;
    (*L).global = g;
    (*g).mainthread = L;
    let head = ptr::addr_of_mut!((*g).uvhead);
    (*g).uvhead.u.open.prev = head;
    (*g).uvhead.u.open.next = head;
    (*g).ptrenckey[0] = 1;
    (*g).strt.saltmult = 0x9e3779b9;
    (*g).strt.hashshift = 32;
    (*g).totalbytes = std::mem::size_of::<LG>();
    (*g).memcatbytes[0] = std::mem::size_of::<LG>();
    (*g).gcstate = GCSpause;
    (*g).gcgoal = LUAI_GCGOAL;
    (*g).gcstepmul = LUAI_GCSTEPMUL;
    (*g).gcstepsize = LUAI_GCSTEPSIZE << 10;
    (*g).registry = TValue::NIL;
    (*g).pseudotemp = TValue::NIL;
    (*g).lastprotoid = 1;
    (*g).lastclassid = 1;
    if luaD_rawrunprotected(L, &mut || f_luaopen(L)) != 0 {
        close_state(L);
        return ptr::null_mut();
    }
    L
}

pub unsafe fn lua_close(L: *mut lua_State) {
    let L = (*(*L).global).mainthread;
    luaF_close(L, (*L).stack);
    close_state(L);
}
