//! Port of `VM/src/lclass.cpp`: user-defined classes (`LuauClass`) and
//! their instances (`LuauObject`).

use std::ptr;

use crate::lapi::getcurrenv;
use crate::laux::luaL_error_owned;
use crate::ldebug::*;
use crate::ldo::{luaD_call, luaD_checkstack};
use crate::lfunc::luaF_newCclosure;
use crate::lgc::*;
use crate::lmem::*;
use crate::lobject::*;
use crate::lstring::luaS_new;
use crate::ltable::*;
use crate::ltm::*;
use crate::lvmutils::luaV_gettable;

const LCM_MISMATCH: u32 = 0;
const LCM_MATCH: u32 = 1;
const LCM_MATCH_ZEROED: u32 = 2;

#[inline(always)]
pub unsafe fn luaR_offsettomemberlength(cls: *const LuauClass) -> u32 {
    (*cls).numberofmembers - (*cls).numberofprivatestaticmembers as u32
}

#[inline(always)]
pub unsafe fn luaR_numstaticmembers(cls: *const LuauClass) -> u32 {
    (*cls).numberofmembers - (*cls).numberofinstancemembers
}

#[inline(always)]
pub unsafe fn luaR_privatestaticmember(cls: *const LuauClass, pindex: u32) -> *mut TValue {
    (*cls).privatestaticmembers.add(pindex as usize)
}

#[inline(always)]
pub unsafe fn luaR_lookupmemberatoffset(inst: *const LuauObject, offset: u32) -> *mut TValue {
    let cls = (*inst).lclass;
    if offset < (*cls).numberofinstancemembers {
        (*inst).members.add(offset as usize)
    } else {
        (*cls).staticmembers.add((offset - (*cls).numberofinstancemembers) as usize)
    }
}

unsafe fn sname(s: *const TString) -> String {
    String::from_utf8_lossy(sbytes(s)).into_owned()
}

pub unsafe fn luaR_newblankclass(L: *mut lua_State, name: *mut TString, isopen: bool) -> *mut LuauClass {
    let c = luaM_newgco_(L, std::mem::size_of::<LuauClass>(), (*L).activememcat) as *mut LuauClass;
    luaC_init(L, c as *mut GCObject, LUA_TCLASS);
    let g = (*L).global;
    (*c).gclist = ptr::null_mut();
    (*c).name = name;
    (*c).id = if (*g).lastclassid == 0 {
        0
    } else {
        let id = (*g).lastclassid;
        (*g).lastclassid += 1;
        id
    };
    (*c).super_ = ptr::null_mut();
    (*c).staticmembers = ptr::null_mut();
    (*c).privatestaticmembers = ptr::null_mut();
    (*c).memberstooffset = ptr::null_mut();
    (*c).offsettomember = ptr::null_mut();
    (*c).metatable = ptr::null_mut();
    (*c).instancemetatable = ptr::null_mut();
    (*c).numberofinstancemembers = 0;
    (*c).numberofmembers = 0;
    (*c).numberofprivatefields = 0;
    (*c).ownprivatefieldsstart = 0;
    (*c).numberofprivatestaticmembers = 0;
    (*c).isopen = isopen;
    (*c).hasuserinitinchain = false;
    c
}

unsafe fn luaR_setupconstructor(L: *mut lua_State, c: *mut LuauClass, env: *mut LuaTable) {
    (*c).metatable = luaH_new(L, 0, 1);
    let ctor = luaF_newCclosure(L, 0, env);
    Closure::c(ctor).f = Some(luaR_constructobject);
    Closure::c(ctor).debugname = luaS_new(L, "luaR_constructobject");
    let slot = luaH_setstr(L, (*c).metatable, (*(*L).global).tmname[TM_CALL as usize]);
    setclvalue(slot, ctor);
    (*(*c).metatable).readonly = 1;
    let dctor = luaF_newCclosure(L, 1, env);
    Closure::c(dctor).f = Some(luaR_defaultcreateobject);
    Closure::c(dctor).debugname = luaS_new(L, "luaR_defaultcreateobject");
    setclassvalue(Closure::cupvals(dctor), c);
    let init_key = luaS_new(L, "__init");
    let init_index = luaH_getstr((*c).memberstooffset, init_key);
    let init_offset = nvalue(init_index) as u32 - (*c).numberofinstancemembers;
    setclvalue((*c).staticmembers.add(init_offset as usize), dctor);
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn luaR_newclass(
    L: *mut lua_State,
    name: *mut TString,
    memberstooffset: *mut LuaTable,
    offsettomember: *mut *mut TString,
    numberofinstancemembers: u32,
    numberofstaticmembers: u32,
    numberofprivatefields: u8,
    numberofprivatemethods: u8,
    envt: *mut LuaTable,
) -> *mut LuauClass {
    let c = luaR_newblankclass(L, name, false);
    (*c).staticmembers = luaM_newarray(L, numberofstaticmembers as usize, (*c).hdr.memcat);
    for i in 0..numberofstaticmembers as usize {
        setnilvalue((*c).staticmembers.add(i));
    }
    (*c).memberstooffset = memberstooffset;
    (*c).offsettomember = offsettomember;
    (*c).numberofinstancemembers = numberofinstancemembers;
    (*c).numberofmembers = numberofinstancemembers + numberofstaticmembers;
    (*c).numberofprivatefields = numberofprivatefields as u32;
    (*c).ownprivatefieldsstart = numberofinstancemembers - numberofprivatefields as u32;
    (*c).numberofprivatestaticmembers = numberofprivatemethods;
    (*c).privatestaticmembers = (*c).staticmembers.add((numberofstaticmembers - numberofprivatemethods as u32) as usize);
    luaR_setupconstructor(L, c, envt);
    c
}

unsafe fn luaR_registerstaticmember(L: *mut lua_State, c: *mut LuauClass, member: *mut TString, val: *const TValue, offset: u32, soff: u32) {
    setobj((*c).staticmembers.add(soff as usize), val);
    *(*c).offsettomember.add(offset as usize) = member;
    setnvalue(luaH_setstr(L, (*c).memberstooffset, member), offset as f64);
}

pub unsafe fn luaR_inheritclass(L: *mut lua_State, child: *const LuauClass, parent: *mut LuauClass) -> *mut LuauClass {
    if !(*parent).isopen {
        luaG_runerror_owned(L, format!("Non-open class '{}' cannot be extended", sname((*parent).name)));
    }
    for idx in 0..(*parent).numberofinstancemembers {
        let mn = *(*parent).offsettomember.add(idx as usize);
        if mn.is_null() {
            continue;
        }
        if ttisnil(luaH_getstr((*child).memberstooffset, mn)) {
            continue;
        }
        luaG_runerror(
            L,
            &format!(
                "Cannot override instance member '{}' of parent class '{}' in child class '{}'",
                sname(mn),
                sname((*parent).name),
                sname((*child).name)
            ),
        );
    }
    let nc = luaR_newblankclass(L, (*child).name, (*child).isopen);
    (*nc).super_ = parent;
    let mut to_copy = 0u32;
    let parent_end = luaR_offsettomemberlength(parent);
    for idx in (*parent).numberofinstancemembers..parent_end {
        let mn = *(*parent).offsettomember.add(idx as usize);
        if ttisnil(luaH_getstr((*child).memberstooffset, mn)) {
            to_copy += 1;
        }
    }
    let num_members = (*child).numberofmembers + (*parent).numberofinstancemembers + to_copy;
    (*nc).offsettomember = luaM_newarray(L, (num_members - (*child).numberofprivatestaticmembers as u32) as usize, (*nc).hdr.memcat);
    (*nc).numberofmembers = num_members;
    (*nc).memberstooffset = luaH_new(
        L,
        0,
        (num_members - (*child).numberofprivatefields - (*parent).numberofprivatefields - (*child).numberofprivatestaticmembers as u32) as i32,
    );
    (*nc).hasuserinitinchain = (*parent).hasuserinitinchain;
    let mut offset = 0u32;
    while offset < (*parent).numberofinstancemembers {
        let mn = *(*parent).offsettomember.add(offset as usize);
        *(*nc).offsettomember.add(offset as usize) = mn;
        if !mn.is_null() {
            setnvalue(luaH_setstr(L, (*nc).memberstooffset, mn), offset as f64);
        }
        offset += 1;
    }
    for idx in 0..(*child).numberofinstancemembers {
        let mn = *(*child).offsettomember.add(idx as usize);
        *(*nc).offsettomember.add(offset as usize) = mn;
        if !mn.is_null() {
            setnvalue(luaH_setstr(L, (*nc).memberstooffset, mn), offset as f64);
        }
        offset += 1;
    }
    (*nc).numberofprivatefields = (*child).numberofprivatefields + (*parent).numberofprivatefields;
    (*nc).ownprivatefieldsstart = (*child).ownprivatefieldsstart + (*parent).numberofinstancemembers;
    (*nc).staticmembers = luaM_newarray(L, (num_members - offset) as usize, (*nc).hdr.memcat);
    for i in 0..(num_members - offset) as usize {
        setnilvalue((*nc).staticmembers.add(i));
    }
    let private_off = (*child).numberofmembers - (*child).numberofinstancemembers - (*child).numberofprivatestaticmembers as u32 + to_copy;
    (*nc).privatestaticmembers = (*nc).staticmembers.add(private_off as usize);
    (*nc).numberofprivatestaticmembers = (*child).numberofprivatestaticmembers;
    (*nc).numberofinstancemembers = offset;
    let mut copied = 0u32;
    for idx in (*parent).numberofinstancemembers..parent_end {
        let mn = *(*parent).offsettomember.add(idx as usize);
        if ttisnil(luaH_getstr((*child).memberstooffset, mn)) {
            let pv = (*parent).staticmembers.add((idx - (*parent).numberofinstancemembers) as usize);
            luaR_registerstaticmember(L, nc, mn, pv, offset, copied);
            offset += 1;
            copied += 1;
        }
    }
    let child_end = luaR_offsettomemberlength(child);
    for idx in (*child).numberofinstancemembers..child_end {
        let mn = *(*child).offsettomember.add(idx as usize);
        let cv = (*child).staticmembers.add((idx - (*child).numberofinstancemembers) as usize);
        luaR_registerstaticmember(L, nc, mn, cv, offset, copied);
        offset += 1;
        copied += 1;
    }
    for i in 0..(*child).numberofprivatestaticmembers as usize {
        setobj((*nc).privatestaticmembers.add(i), (*child).privatestaticmembers.add(i));
    }
    if !(*parent).instancemetatable.is_null() {
        (*nc).instancemetatable = luaH_clone(L, (*parent).instancemetatable);
    }
    luaR_setupconstructor(L, nc, getcurrenv(L));
    nc
}

pub unsafe fn luaR_addpublicstaticmember(L: *mut lua_State, c: *mut LuauClass, name: *mut TString, value: *mut TValue) {
    let off = luaH_getstr((*c).memberstooffset, name);
    let offi = nvalue(off) as u32;
    setobj((*c).staticmembers.add((offi - (*c).numberofinstancemembers) as usize), value);
    luaC_barrier(L, c as *mut GCObject, value);
    let g = (*L).global;
    if name == crate::lstring::luaS_newlstr(L, b"__init") {
        (*c).hasuserinitinchain = true;
    }
    let mut is_mm = name == crate::lstring::luaS_newlstr(L, b"__tostring");
    for i in 0..TM_N {
        if is_mm {
            break;
        }
        is_mm = name == (*g).tmname[i];
    }
    if is_mm {
        if (*c).instancemetatable.is_null() {
            (*c).instancemetatable = luaH_new(L, 0, 1);
            luaC_objbarrier(L, c as *mut GCObject, (*c).instancemetatable as *mut GCObject);
        } else {
            let is_cmp = name == (*g).tmname[TM_EQ as usize] || name == (*g).tmname[TM_LT as usize] || name == (*g).tmname[TM_LE as usize];
            if is_cmp && !ttisnil(luaH_getstr((*c).instancemetatable, name)) {
                luaG_runerror(
                    L,
                    &format!("Overriding comparison metamethods is not allowed ('{}' in class '{}')", sname(name), sname((*c).name)),
                );
            }
        }
        let dest = luaH_setstr(L, (*c).instancemetatable, name);
        setobj(dest, value);
        luaC_barrier(L, (*c).instancemetatable as *mut GCObject, value);
    }
}

pub unsafe fn luaR_newblankobject(L: *mut lua_State, c: *mut LuauClass) -> *mut LuauObject {
    let o = luaM_newgco_(L, std::mem::size_of::<LuauObject>(), (*L).activememcat) as *mut LuauObject;
    luaC_init(L, o as *mut GCObject, LUA_TOBJECT);
    (*o).gclist = ptr::null_mut();
    (*o).lclass = c;
    (*o).members = luaM_newarray(L, (*c).numberofinstancemembers as usize, (*L).activememcat);
    (*o).numberofmembers = (*c).numberofinstancemembers;
    for i in 0..(*o).numberofmembers as usize {
        setnilvalue((*o).members.add(i));
    }
    o
}

pub unsafe fn luaR_addprivatestaticmember(L: *mut lua_State, c: *mut LuauClass, pindex: u8, value: *mut TValue) {
    setobj(luaR_privatestaticmember(c, pindex as u32), value);
    luaC_barrier(L, c as *mut GCObject, value);
}

pub unsafe fn luaR_constructobject(L: *mut lua_State) -> i32 {
    let c = classvalue((*L).base);
    let this = luaR_newblankobject(L, c);
    let init_key = luaS_new(L, "__init");
    let init_index = luaH_getstr((*c).memberstooffset, init_key);
    let init_offset = nvalue(init_index) as u32 - (*c).numberofinstancemembers;
    let init_fn = *(*c).staticmembers.add(init_offset as usize);
    let numargs = (*L).top.offset_from((*L).base) as i32 - 1;
    setobjectvalue((*L).top, this);
    (*L).top = (*L).top.add(1);
    luaD_checkstack(L, 2 + numargs);
    let args_base = (*L).top;
    setobj((*L).top, &init_fn);
    (*L).top = (*L).top.add(1);
    setobjectvalue((*L).top, this);
    (*L).top = (*L).top.add(1);
    for i in 1..=numargs {
        setobj((*L).top, (*L).base.add(i as usize));
        (*L).top = (*L).top.add(1);
    }
    luaD_call(L, args_base, 0);
    1
}

pub unsafe fn luaR_defaultcreateobject(L: *mut lua_State) -> i32 {
    let cl = clvalue((*(*L).ci).func);
    let c = classvalue(Closure::cupvals(cl));
    let cname = sname((*c).name);
    if (*c).hasuserinitinchain {
        luaL_error_owned(L, format!("Class {} must define a constructor because it is derived from a class that defines one", cname));
    } else if (*c).numberofprivatefields != 0 {
        if !(*c).super_.is_null() {
            luaL_error_owned(L, format!("Class {} must define a constructor because it or a class it inherits from has private fields", cname));
        } else {
            luaL_error_owned(L, format!("Class {} must define a constructor because it has private fields", cname));
        }
    }
    let numargs = crate::lapi::lua_gettop(L);
    if numargs != 2 {
        luaL_error_owned(L, format!("The constructor of {} must be called with 2 arguments.  Got {}", cname, numargs));
    }
    if !ttisobject((*L).base) {
        luaL_error_owned(L, format!("{}.__init must be called with an instance of the class as its first argument", cname));
    }
    let inst = objectvalue((*L).base);
    if (*inst).lclass != c {
        luaL_error_owned(L, format!("Cannot call {}.__init on an instance of class {}", cname, sname((*(*inst).lclass).name)));
    }
    setnilvalue((*L).top);
    (*L).top = (*L).top.add(1);
    for idx in 0..(*c).numberofinstancemembers as usize {
        let mn = *(*c).offsettomember.add(idx);
        if mn.is_null() {
            continue;
        }
        let mut key = TValue::NIL;
        setsvalue(&mut key, mn);
        luaV_gettable(L, (*L).base.add(1), &mut key, (*L).top.sub(1));
        setobj((*inst).members.add(idx), (*L).top.sub(1));
        luaC_barrier(L, inst as *mut GCObject, (*inst).members.add(idx));
    }
    (*L).top = (*L).top.sub(1);
    0
}

unsafe fn classmatchesshape(k: *mut LuauClass, shape: *mut LuaTable) -> u32 {
    if (*k).hasuserinitinchain || (*k).numberofprivatefields > 0 {
        return LCM_MISMATCH;
    }
    let mut membercount = 0u32;
    let mut defaultinit = true;
    for i in 0..sizenode(shape) {
        let node = gnode(shape, i);
        let tt = (*node).key.tt();
        if tt == LUA_TNIL {
            continue;
        }
        if tt != LUA_TSTRING {
            return LCM_MISMATCH;
        }
        let off = luaH_getstr((*k).memberstooffset, (*node).key.value.gc as *mut TString);
        if !ttisnumber(off) || nvalue(off) as u32 >= (*k).numberofinstancemembers {
            return LCM_MISMATCH;
        }
        membercount += 1;
        let v = gval(node);
        if !(ttisnumber(v) && nvalue(v) == 0.0 && !nvalue(v).is_sign_negative()) {
            defaultinit = false;
        }
    }
    if defaultinit && membercount == (*k).numberofinstancemembers {
        LCM_MATCH_ZEROED
    } else {
        LCM_MATCH
    }
}

pub unsafe fn luaR_tryconstructobject(L: *mut lua_State, target: StkId, maybeclass: StkId, slotid: u32) {
    let p = (*(*L).ci).p;
    let slot = (*p).feedbackvec.add(slotid as usize);
    let shape = (*p).k.add((*slot).a as usize);
    let shape_table = hvalue(shape);
    if !ttisclass(maybeclass) {
        sethvalue(target, luaH_clone(L, shape_table));
        return;
    }
    let k = classvalue(maybeclass);
    if (*slot).b != (*k).id {
        if (*k).id == 0 {
            (*slot).c = LCM_MISMATCH;
            (*slot).b = 0;
        } else {
            (*slot).c = classmatchesshape(k, shape_table);
            (*slot).b = (*k).id;
        }
    }
    if (*slot).c != LCM_MISMATCH {
        let this = luaR_newblankobject(L, k);
        if (*slot).c == LCM_MATCH_ZEROED {
            for i in 0..(*k).numberofinstancemembers as usize {
                setnvalue((*this).members.add(i), 0.0);
            }
        } else {
            for i in 0..(*k).numberofinstancemembers as usize {
                setobj((*this).members.add(i), luaH_getstr(shape_table, *(*k).offsettomember.add(i)));
            }
        }
        setobjectvalue(target, this);
    } else {
        sethvalue(target, luaH_clone(L, shape_table));
    }
}

pub unsafe fn luaR_isinstance(inst: *const LuauObject, cls: *const LuauClass) -> bool {
    let mut c = (*inst).lclass;
    while !c.is_null() {
        if c as *const LuauClass == cls {
            return true;
        }
        c = (*c).super_;
    }
    false
}

pub unsafe fn luaR_issubclass(child: *const LuauClass, ancestor: *const LuauClass) -> bool {
    let mut c = child;
    while !c.is_null() {
        if c == ancestor {
            return true;
        }
        c = (*c).super_;
    }
    false
}

pub unsafe fn luaR_freeclass(L: *mut lua_State, c: *mut LuauClass) {
    let m = (*c).hdr.memcat;
    if !(*c).staticmembers.is_null() {
        luaM_freearray(L, (*c).staticmembers, ((*c).numberofmembers - (*c).numberofinstancemembers) as usize, m);
    }
    if !(*c).offsettomember.is_null() {
        luaM_freearray(L, (*c).offsettomember, luaR_offsettomemberlength(c) as usize, m);
    }
    luaM_freegco_(L, c as *mut GCObject, std::mem::size_of::<LuauClass>(), m);
}

pub unsafe fn luaR_freeobject(L: *mut lua_State, o: *mut LuauObject) {
    let m = (*o).hdr.memcat;
    if !(*o).members.is_null() {
        luaM_freearray(L, (*o).members, (*o).numberofmembers as usize, m);
    }
    luaM_freegco_(L, o as *mut GCObject, std::mem::size_of::<LuauObject>(), m);
}

// GETPRIVATEFIELD / SETPRIVATEFIELD bodies (`lvmexecute.cpp`)

#[inline(always)]
fn aux_a(aux: u32) -> u32 {
    aux & 0xff
}
#[inline(always)]
fn aux_static(aux: u32) -> bool {
    (aux >> 8) & 1 != 0
}
#[inline(always)]
fn aux_namecall(aux: u32) -> bool {
    (aux >> 9) & 1 != 0
}

pub unsafe fn getprivatefield(L: *mut lua_State, ra: StkId, rb: StkId, clsuv: *mut TValue, aux: u32) {
    if !(ttisobject(rb) || ttisclass(rb)) {
        luaG_typeerror(L, rb, "access private field of");
    }
    let clsv = if (*clsuv).tt == LUA_TUPVAL { (*upvalue(clsuv)).v } else { clsuv };
    if !ttisclass(clsv) {
        luaG_typeerror(L, clsv, "verify object is an instance of");
    }
    let cls = classvalue(clsv);
    if ttisobject(rb) {
        let inst = objectvalue(rb);
        if !luaR_isinstance(inst, cls) {
            luaG_runerror(L, "attempt to illegally access private field");
        }
        if aux_static(aux) {
            if aux_namecall(aux) {
                setobj(ra.add(1), rb);
            }
            setobj(ra, luaR_privatestaticmember(cls, aux_a(aux)));
        } else {
            let pindex = aux_a(aux) + (*cls).ownprivatefieldsstart;
            if aux_namecall(aux) {
                setobj(ra.add(1), rb);
            }
            setobj(ra, (*inst).members.add(pindex as usize));
        }
    } else {
        let srccls = classvalue(rb);
        if !luaR_issubclass(srccls, cls) {
            luaG_runerror(L, "attempt to illegally access private field of a class");
        }
        if !aux_static(aux) {
            luaG_runerror(L, "attempt to access private instance field of a class");
        }
        if aux_namecall(aux) {
            setobj(ra.add(1), rb);
        }
        setobj(ra, luaR_privatestaticmember(cls, aux_a(aux)));
    }
}

pub unsafe fn setprivatefield(L: *mut lua_State, ra: StkId, rb: StkId, clsuv: *mut TValue, aux: u32) {
    if !ttisobject(rb) {
        luaG_typeerror(L, rb, "assign private field of");
    }
    let clsv = if (*clsuv).tt == LUA_TUPVAL { (*upvalue(clsuv)).v } else { clsuv };
    if !ttisclass(clsv) {
        luaG_typeerror(L, clsv, "assign private field of");
    }
    let inst = objectvalue(rb);
    let cls = classvalue(clsv);
    if !luaR_isinstance(inst, cls) {
        luaG_runerror(L, "attempt to assign private field of an object with a different class");
    }
    let pindex = (aux & 0xff) + (*cls).ownprivatefieldsstart;
    setobj((*inst).members.add(pindex as usize), ra);
    luaC_barrier(L, inst as *mut GCObject, ra);
}
