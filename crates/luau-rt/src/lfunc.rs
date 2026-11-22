//! Port of `VM/src/lfunc.cpp` (prototypes, closures, upvalues), plus
//! `ludata.cpp` and `lbuffer.cpp`.

use std::ptr;

use crate::lgc::*;
use crate::lmem::*;
use crate::lobject::*;

pub unsafe fn luaF_newproto(L: *mut lua_State) -> *mut Proto {
    let f = luaM_newgco_(L, std::mem::size_of::<Proto>(), (*L).activememcat) as *mut Proto;
    // zero everything after the header
    ptr::write_bytes((f as *mut u8).add(std::mem::size_of::<GCheader>()), 0, std::mem::size_of::<Proto>() - std::mem::size_of::<GCheader>());
    luaC_init(L, f as *mut GCObject, LUA_TPROTO);
    f
}

pub unsafe fn luaF_newLclosure(L: *mut lua_State, nelems: i32, e: *mut LuaTable, p: *mut Proto) -> *mut Closure {
    let c = luaM_newgco_(L, sizeLclosure(nelems as usize), (*L).activememcat) as *mut Closure;
    luaC_init(L, c as *mut GCObject, LUA_TFUNCTION);
    (*c).isC = 0;
    (*c).env = e;
    (*c).nupvalues = nelems as u8;
    (*c).stacksize = (*p).maxstacksize;
    (*c).preload = 0;
    (*c).gclist = ptr::null_mut();
    Closure::l(c).p = p;
    let up = Closure::uprefs(c);
    for i in 0..nelems as usize {
        setnilvalue(up.add(i));
    }
    c
}

pub unsafe fn luaF_newCclosure(L: *mut lua_State, nelems: i32, e: *mut LuaTable) -> *mut Closure {
    let c = luaM_newgco_(L, sizeCclosure(nelems as usize), (*L).activememcat) as *mut Closure;
    luaC_init(L, c as *mut GCObject, LUA_TFUNCTION);
    (*c).isC = 1;
    (*c).env = e;
    (*c).nupvalues = nelems as u8;
    (*c).stacksize = LUA_MINSTACK as u8;
    (*c).preload = 0;
    (*c).gclist = ptr::null_mut();
    let cc = Closure::c(c);
    cc.f = None;
    cc.cont = None;
    cc.debugname = ptr::null_mut();
    c
}

pub unsafe fn luaF_findupval(L: *mut lua_State, level: StkId) -> *mut UpVal {
    let g = (*L).global;
    let mut pp: *mut *mut UpVal = ptr::addr_of_mut!((*L).openupval);
    while !(*pp).is_null() && (**pp).v >= level {
        let p = *pp;
        if (*p).v == level {
            return p;
        }
        pp = ptr::addr_of_mut!((*p).u.open.threadnext);
    }
    let uv = luaM_newgco_(L, std::mem::size_of::<UpVal>(), (*L).activememcat) as *mut UpVal;
    luaC_init(L, uv as *mut GCObject, LUA_TUPVAL);
    (*uv).markedopen = 0;
    (*uv).v = level;
    (*uv).u.open.threadnext = *pp;
    *pp = uv;
    let head = ptr::addr_of_mut!((*g).uvhead);
    (*uv).u.open.prev = head;
    (*uv).u.open.next = (*head).u.open.next;
    (*(*uv).u.open.next).u.open.prev = uv;
    (*head).u.open.next = uv;
    uv
}

pub unsafe fn luaF_freeupval(L: *mut lua_State, uv: *mut UpVal) {
    luaM_freegco_(L, uv as *mut GCObject, std::mem::size_of::<UpVal>(), (*uv).hdr.memcat);
}

pub unsafe fn luaF_close(L: *mut lua_State, level: StkId) {
    while !(*L).openupval.is_null() && (*(*L).openupval).v >= level {
        let uv = (*L).openupval;
        (*L).openupval = (*uv).u.open.threadnext;
        luaF_closeupval(L, uv, false);
    }
}

pub unsafe fn luaF_closeupval(L: *mut lua_State, uv: *mut UpVal, dead: bool) {
    (*(*uv).u.open.next).u.open.prev = (*uv).u.open.prev;
    (*(*uv).u.open.prev).u.open.next = (*uv).u.open.next;
    if dead {
        return;
    }
    let v = *(*uv).v;
    (*uv).u.value = v;
    (*uv).v = ptr::addr_of_mut!((*uv).u.value);
    luaC_upvalclosed(L, uv);
}

pub unsafe fn luaF_freeproto(L: *mut lua_State, f: *mut Proto) {
    #[cfg(all(target_arch = "x86_64", windows))]
    crate::codegen::free_execdata(f);
    crate::jit::forget_proto(f);
    let m = (*f).hdr.memcat;
    luaM_freearray(L, (*f).code, (*f).sizecode as usize, m);
    luaM_freearray(L, (*f).p, (*f).sizep as usize, m);
    luaM_freearray(L, (*f).k, (*f).sizek as usize, m);
    if !(*f).lineinfo.is_null() {
        luaM_freearray(L, (*f).lineinfo, (*f).sizelineinfo as usize, m);
    }
    luaM_freearray(L, (*f).locvars, (*f).sizelocvars as usize, m);
    luaM_freearray(L, (*f).upvalues, (*f).sizeupvalues as usize, m);
    if !(*f).debuginsn.is_null() {
        luaM_freearray(L, (*f).debuginsn, (*f).sizecode as usize, m);
    }
    if !(*f).typeinfo.is_null() {
        luaM_freearray(L, (*f).typeinfo, (*f).sizetypeinfo as usize, m);
    }
    if !(*f).feedbackvec.is_null() {
        luaM_freearray(L, (*f).feedbackvec, (*f).feedbackvecsize as usize, m);
    }
    luaM_freegco_(L, f as *mut GCObject, std::mem::size_of::<Proto>(), m);
}

pub unsafe fn luaF_freeclosure(L: *mut lua_State, c: *mut Closure) {
    let size = if (*c).isC != 0 { sizeCclosure((*c).nupvalues as usize) } else { sizeLclosure((*c).nupvalues as usize) };
    luaM_freegco_(L, c as *mut GCObject, size, (*c).hdr.memcat);
}

pub unsafe fn luaF_getlocal(f: *const Proto, mut local_number: i32, pc: i32) -> *const LocVar {
    for i in 0..(*f).sizelocvars as usize {
        let lv = (*f).locvars.add(i);
        if pc >= (*lv).startpc && pc < (*lv).endpc {
            local_number -= 1;
            if local_number == 0 {
                return lv;
            }
        }
    }
    ptr::null()
}

pub unsafe fn luaF_findlocal(f: *const Proto, local_reg: i32, pc: i32) -> *const LocVar {
    for i in 0..(*f).sizelocvars as usize {
        let lv = (*f).locvars.add(i);
        if local_reg == (*lv).reg as i32 && pc >= (*lv).startpc && pc < (*lv).endpc {
            return lv;
        }
    }
    ptr::null()
}

// ---------------------------------------------------------------------------
// ludata.cpp
// ---------------------------------------------------------------------------

#[inline]
pub const fn sizeudata(len: usize) -> usize {
    Udata::DATA_OFFSET + if len > 16 { (len + 15) & !15 } else { len }
}

pub unsafe fn luaU_newudata(L: *mut lua_State, s: usize, tag: i32) -> *mut Udata {
    if s > i32::MAX as usize - Udata::DATA_OFFSET {
        luaM_toobig(L);
    }
    let u = luaM_newgco_(L, sizeudata(s), (*L).activememcat) as *mut Udata;
    luaC_init(L, u as *mut GCObject, LUA_TUSERDATA);
    (*u).len = s as i32;
    (*u).metatable = ptr::null_mut();
    (*u).tag = tag as u8;
    u
}

pub unsafe fn luaU_freeudata(L: *mut lua_State, u: *mut Udata) {
    let mut dtor: Option<unsafe fn(*mut lua_State, *mut u8)> = None;
    if ((*u).tag as usize) < LUA_UTAG_LIMIT {
        dtor = (*(*L).global).udatagc[(*u).tag as usize];
    } else if (*u).tag as usize == crate::ludata::UTAG_IDTOR {
        let at = Udata::data(u).add((*u).len as usize - std::mem::size_of::<lua_Destructor>()) as *const lua_Destructor;
        dtor = Some(ptr::read_unaligned(at));
    }
    if let Some(d) = dtor {
        d(L, Udata::data(u));
    }
    luaM_freegco_(L, u as *mut GCObject, sizeudata((*u).len as usize), (*u).hdr.memcat);
}

// ---------------------------------------------------------------------------
// lbuffer.cpp
// ---------------------------------------------------------------------------

pub const MAX_BUFFER_SIZE: usize = 1 << 30;

#[inline]
pub const fn sizebuffer(len: usize) -> usize {
    Buffer::DATA_OFFSET + if len < 8 { 8 } else { len }
}

pub unsafe fn luaB_newbuffer(L: *mut lua_State, s: usize) -> *mut Buffer {
    if s > MAX_BUFFER_SIZE {
        luaM_toobig(L);
    }
    let b = luaM_newgco_(L, sizebuffer(s), (*L).activememcat) as *mut Buffer;
    luaC_init(L, b as *mut GCObject, LUA_TBUFFER);
    (*b).len = s as u32;
    ptr::write_bytes(Buffer::data(b), 0, s);
    b
}

pub unsafe fn luaB_freebuffer(L: *mut lua_State, b: *mut Buffer) {
    luaM_freegco_(L, b as *mut GCObject, sizebuffer((*b).len as usize), (*b).hdr.memcat);
}
