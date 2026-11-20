//! Port of `VM/src/lvmutils.cpp`: slow paths of the interpreter
//! (metamethod dispatch, comparisons, concatenation, arithmetic).

use crate::ldebug::*;
use crate::ldo::*;
use crate::lgc::*;
use crate::lobject::*;
use crate::lnumutils::*;
use crate::lstring::*;
use crate::ltable::*;
use crate::ltm::*;
use crate::lvmexecute::luau_execute;

const MAXTAGLOOP: i32 = 100;

/// Runtime switch for user-defined classes
/// (`DebugLuauUserDefinedClassesRuntime`; the conformance runner enables it
/// for the class tests).
pub static mut CLASSES_RUNTIME: bool = false;

#[inline(always)]
pub unsafe fn classes_enabled() -> bool {
    CLASSES_RUNTIME
}

pub unsafe fn luaV_tonumber(obj: *const TValue, n: *mut TValue) -> *const TValue {
    if ttisnumber(obj) {
        return obj;
    }
    if ttisstring(obj) {
        if let Some(num) = luaO_str2d(sbytes(tsvalue(obj))) {
            setnvalue(n, num);
            return n;
        }
    }
    std::ptr::null()
}

/// `tonumber` macro: returns the numeric view of `o` or null.
#[inline(always)]
pub unsafe fn tonumber(o: *const TValue, n: *mut TValue) -> *const TValue {
    if (*o).tt == LUA_TNUMBER {
        o
    } else {
        luaV_tonumber(o, n)
    }
}

pub unsafe fn luaV_tostring(L: *mut lua_State, obj: StkId) -> i32 {
    if !ttisnumber(obj) {
        return 0;
    }
    let mut nb = crate::lnumutils::NumBuf::new();
    let s = crate::lnumutils::luai_num2buf(&mut nb, nvalue(obj));
    setsvalue(obj, luaS_newlstr(L, s));
    1
}

/// `tostring` macro.
#[inline(always)]
pub unsafe fn tostring(L: *mut lua_State, o: StkId) -> bool {
    (*o).tt == LUA_TSTRING || luaV_tostring(L, o) != 0
}

/// `equalobj` macro.
#[inline(always)]
pub unsafe fn equalobj(L: *mut lua_State, o1: *const TValue, o2: *const TValue) -> bool {
    (*o1).tt == (*o2).tt && luaV_equalval(L, o1, o2) != 0
}

#[inline(always)]
unsafe fn callTMluau(L: *mut lua_State, top: StkId, ccl: *mut Closure, nresults: i32) {
    (*L).nCcalls += 1;
    if (*L).nCcalls as i32 >= LUAI_MAXCCALLS {
        luaD_checkCstack(L);
    }
    let funcoffset = savestack(L, top);
    let p = Closure::proto(ccl);
    (*p).execcount += 1;
    let ci = incr_ci(L);
    (*ci).func = top;
    (*ci).p = p;
    (*ci).base = top.add(1);
    (*ci).top = (*L).top.add((*ccl).stacksize as usize);
    (*ci).savedpc = (*p).code;
    (*ci).flags = LUA_CALLINFO_RETURN;
    (*ci).nresults = nresults;
    (*L).base = (*ci).base;
    luaD_checkstackfornewci(L, (*ccl).stacksize as i32);
    let ci = (*L).ci;
    let mut argi = (*L).top;
    let argend = (*L).base.add((*p).numparams as usize);
    while argi < argend {
        setnilvalue(argi);
        argi = argi.add(1);
    }
    (*L).top = if (*p).is_vararg != 0 { argi } else { (*ci).top };
    luau_execute(L);
    (*L).top = restorestack(L, funcoffset).add(nresults as usize);
    (*L).nCcalls -= 1;
}

unsafe fn callTMres(L: *mut lua_State, res: StkId, f: *const TValue, p1: *const TValue, p2: *const TValue) -> StkId {
    let result = savestack(L, res);
    let top = (*L).top;
    setobj(top, f);
    setobj(top.add(1), p1);
    setobj(top.add(2), p2);
    if (*L).isactive && isLua((*L).ci) && ttisfunction(top) && (*clvalue(top)).isC == 0 {
        (*L).top = (*L).top.add(3);
        callTMluau(L, top, clvalue(top), 1);
    } else {
        luaD_checkstack(L, 3);
        let func = (*L).top;
        (*L).top = (*L).top.add(3);
        luaD_call(L, func, 1);
    }
    let res = restorestack(L, result);
    (*L).top = (*L).top.sub(1);
    setobj(res, (*L).top);
    res
}

#[inline(never)]
unsafe fn callTM(L: *mut lua_State, f: *const TValue, p1: *const TValue, p2: *const TValue, p3: *const TValue) {
    let top = (*L).top;
    setobj(top, f);
    setobj(top.add(1), p1);
    setobj(top.add(2), p2);
    setobj(top.add(3), p3);
    if (*L).isactive && isLua((*L).ci) && ttisfunction(top) && (*clvalue(top)).isC == 0 {
        (*L).top = (*L).top.add(4);
        callTMluau(L, top, clvalue(top), 0);
    } else {
        luaD_checkstack(L, 4);
        let func = (*L).top;
        (*L).top = (*L).top.add(4);
        luaD_call(L, func, 0);
    }
}

pub unsafe fn luaV_gettable(L: *mut lua_State, t: *const TValue, key: *mut TValue, val: StkId) {
    let mut t = t;
    for _ in 0..MAXTAGLOOP {
        let tm: *const TValue;
        if ttistable(t) {
            let h = hvalue(t);
            let res = luaH_get(h, key);
            if res != luaO_nilobject() {
                (*L).cachedslot = gval2slot(h, res);
            }
            if !ttisnil(res) {
                setobj(val, res);
                return;
            }
            tm = fasttm(L, (*h).metatable, TM_INDEX);
            if tm.is_null() {
                setnilvalue(val);
                return;
            }
        } else if classes_enabled() && ttisobject(t) {
            let inst = objectvalue(t);
            let off = luaH_get((*(*inst).lclass).memberstooffset, key);
            if ttisnil(off) {
                luaG_missingmembererror(L, t, key);
            }
            let offset = nvalue(off) as u32;
            setobj(val, crate::lclass::luaR_lookupmemberatoffset(inst, offset));
            return;
        } else if classes_enabled() && ttisclass(t) {
            let lco = classvalue(t);
            let res = luaH_get((*lco).memberstooffset, key);
            if ttisnil(res) {
                luaG_missingmembererror(L, t, key);
            }
            let offset = nvalue(res) as u32;
            if offset < (*lco).numberofinstancemembers {
                luaG_missingmembererror(L, t, key);
            }
            setobj(val, (*lco).staticmembers.add((offset - (*lco).numberofinstancemembers) as usize));
            return;
        } else {
            tm = luaT_gettmbyobj(L, t, TM_INDEX);
            if ttisnil(tm) {
                luaG_indexerror(L, t, key);
            }
        }
        if ttisfunction(tm) {
            callTMres(L, val, tm, t, key);
            return;
        }
        t = tm;
    }
    luaG_runerror(L, "'__index' chain too long; possible loop")
}

pub unsafe fn luaV_settable(L: *mut lua_State, t: *const TValue, key: *mut TValue, val: StkId) {
    let mut t = t;
    let mut temp = TValue::NIL;
    for _ in 0..MAXTAGLOOP {
        let tm: *const TValue;
        if ttistable(t) {
            let h = hvalue(t);
            let oldval = luaH_get(h, key);
            let mut tmv = std::ptr::null();
            if !ttisnil(oldval) || {
                tmv = fasttm(L, (*h).metatable, TM_NEWINDEX);
                tmv.is_null()
            } {
                if (*h).readonly != 0 {
                    luaG_readonlyerror(L);
                }
                let newval = luaH_setslot(L, h, oldval, key);
                (*L).cachedslot = gval2slot(h, newval);
                setobj(newval, val);
                luaC_barriert(L, h, val);
                return;
            }
            tm = tmv;
        } else if classes_enabled() && ttisobject(t) {
            let inst = objectvalue(t);
            let off = luaH_get((*(*inst).lclass).memberstooffset, key);
            if ttisnil(off) {
                luaG_missingmembererror(L, t, key);
            }
            let offsetnum = nvalue(off) as u32;
            if offsetnum >= (*(*inst).lclass).numberofinstancemembers {
                luaG_indexerror(L, t, key);
            }
            setobj((*inst).members.add(offsetnum as usize), val);
            luaC_barrier(L, inst as *mut GCObject, val);
            return;
        } else {
            tm = luaT_gettmbyobj(L, t, TM_NEWINDEX);
            if ttisnil(tm) {
                luaG_indexerror(L, t, key);
            }
        }
        if ttisfunction(tm) {
            callTM(L, tm, t, key, val);
            return;
        }
        temp = *tm;
        t = &temp;
    }
    luaG_runerror(L, "'__newindex' chain too long; possible loop")
}

unsafe fn call_binTM(L: *mut lua_State, p1: *const TValue, p2: *const TValue, res: StkId, event: TMS) -> bool {
    let mut tm = luaT_gettmbyobj(L, p1, event);
    if ttisnil(tm) {
        tm = luaT_gettmbyobj(L, p2, event);
    }
    if ttisnil(tm) {
        return false;
    }
    callTMres(L, res, tm, p1, p2);
    true
}

unsafe fn get_compTM(L: *mut lua_State, mt1: *mut LuaTable, mt2: *mut LuaTable, event: TMS) -> *const TValue {
    let tm1 = fasttm(L, mt1, event);
    if tm1.is_null() {
        return std::ptr::null();
    }
    if mt1 == mt2 {
        return tm1;
    }
    let tm2 = fasttm(L, mt2, event);
    if tm2.is_null() {
        return std::ptr::null();
    }
    if luaO_rawequalObj(tm1, tm2) {
        return tm1;
    }
    std::ptr::null()
}

unsafe fn call_orderTM(L: *mut lua_State, p1: *const TValue, p2: *const TValue, event: TMS, error: bool) -> i32 {
    let tm1 = luaT_gettmbyobj(L, p1, event);
    if ttisnil(tm1) {
        if error {
            luaG_ordererror(L, p1, p2, event);
        }
        return -1;
    }
    let tm2 = luaT_gettmbyobj(L, p2, event);
    if !luaO_rawequalObj(tm1, tm2) {
        if error {
            luaG_ordererror(L, p1, p2, event);
        }
        return -1;
    }
    callTMres(L, (*L).top, tm1, p1, p2);
    (!l_isfalse((*L).top)) as i32
}

pub unsafe fn luaV_strcmp(ls: *const TString, rs: *const TString) -> i32 {
    if ls == rs {
        return 0;
    }
    let l = sbytes(ls);
    let r = sbytes(rs);
    // C++ compares the first bytes through the NUL terminator first
    let l0 = *getstr(ls);
    let r0 = *getstr(rs);
    if l0 != r0 {
        return l0 as i32 - r0 as i32;
    }
    let lmin = l.len().min(r.len());
    match l[..lmin].cmp(&r[..lmin]) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Equal => {
            if l.len() == r.len() {
                0
            } else if l.len() < r.len() {
                -1
            } else {
                1
            }
        }
    }
}

pub unsafe fn luaV_lessthan(L: *mut lua_State, l: *const TValue, r: *const TValue) -> i32 {
    if (*l).tt != (*r).tt {
        luaG_ordererror(L, l, r, TM_LT);
    } else if ttisnumber(l) {
        (nvalue(l) < nvalue(r)) as i32
    } else if ttisstring(l) {
        (luaV_strcmp(tsvalue(l), tsvalue(r)) < 0) as i32
    } else {
        call_orderTM(L, l, r, TM_LT, true)
    }
}

pub unsafe fn luaV_lessequal(L: *mut lua_State, l: *const TValue, r: *const TValue) -> i32 {
    if (*l).tt != (*r).tt {
        luaG_ordererror(L, l, r, TM_LE);
    } else if ttisnumber(l) {
        return (nvalue(l) <= nvalue(r)) as i32;
    } else if ttisstring(l) {
        return (luaV_strcmp(tsvalue(l), tsvalue(r)) <= 0) as i32;
    }
    let res = call_orderTM(L, l, r, TM_LE, false);
    if res != -1 {
        return res;
    }
    let res = call_orderTM(L, r, l, TM_LT, false);
    if res == -1 {
        luaG_ordererror(L, l, r, TM_LE);
    }
    (res == 0) as i32
}

pub unsafe fn luaV_equalval(L: *mut lua_State, t1: *const TValue, t2: *const TValue) -> i32 {
    let tm: *const TValue;
    match (*t1).tt {
        LUA_TNIL => return 1,
        LUA_TNUMBER => return (nvalue(t1) == nvalue(t2)) as i32,
        LUA_TINTEGER => return (lvalue(t1) == lvalue(t2)) as i32,
        LUA_TVECTOR => return luai_veceq(vvalue(t1), vvalue(t2)) as i32,
        LUA_TBOOLEAN => return (bvalue(t1) == bvalue(t2)) as i32,
        LUA_TLIGHTUSERDATA => return (pvalue(t1) == pvalue(t2) && lightuserdatatag(t1) == lightuserdatatag(t2)) as i32,
        LUA_TUSERDATA => {
            tm = get_compTM(L, (*uvalue(t1)).metatable, (*uvalue(t2)).metatable, TM_EQ);
            if tm.is_null() {
                return (uvalue(t1) == uvalue(t2)) as i32;
            }
        }
        LUA_TCLASS => return (classvalue(t1) == classvalue(t2)) as i32,
        LUA_TOBJECT => {
            tm = get_compTM(
                L,
                (*(*objectvalue(t1)).lclass).instancemetatable,
                (*(*objectvalue(t2)).lclass).instancemetatable,
                TM_EQ,
            );
            if tm.is_null() {
                return (objectvalue(t1) == objectvalue(t2)) as i32;
            }
        }
        LUA_TTABLE => {
            tm = get_compTM(L, (*hvalue(t1)).metatable, (*hvalue(t2)).metatable, TM_EQ);
            if tm.is_null() {
                return (hvalue(t1) == hvalue(t2)) as i32;
            }
        }
        _ => return (gcvalue(t1) == gcvalue(t2)) as i32,
    }
    callTMres(L, (*L).top, tm, t1, t2);
    (!l_isfalse((*L).top)) as i32
}

pub unsafe fn luaV_concat(L: *mut lua_State, mut total: i32, mut last: i32) {
    loop {
        let top = (*L).base.add(last as usize + 1);
        let mut n = 2;
        if !(ttisstring(top.sub(2)) || ttisnumber(top.sub(2))) || !tostring(L, top.sub(1)) {
            if !call_binTM(L, top.sub(2), top.sub(1), top.sub(2), TM_CONCAT) {
                luaG_concaterror(L, top.sub(2), top.sub(1));
            }
        } else if (*tsvalue(top.sub(1))).len == 0 {
            tostring(L, top.sub(2));
        } else {
            let mut tl = (*tsvalue(top.sub(1))).len as usize;
            n = 1;
            while n < total && tostring(L, top.sub(n as usize + 1)) {
                let l = (*tsvalue(top.sub(n as usize + 1))).len as usize;
                if l > MAXSSIZE - tl {
                    luaG_runerror(L, "string length overflow");
                }
                tl += l;
                n += 1;
            }
            if tl < LUA_BUFFERSIZE {
                let mut buf = [0u8; LUA_BUFFERSIZE];
                let mut off = 0;
                let mut i = n;
                while i > 0 {
                    let s = sbytes(tsvalue(top.sub(i as usize)));
                    buf[off..off + s.len()].copy_from_slice(s);
                    off += s.len();
                    i -= 1;
                }
                setsvalue(top.sub(n as usize), luaS_newlstr(L, &buf[..off]));
            } else {
                let ts = luaS_bufstart(L, tl);
                let data = std::ptr::addr_of_mut!((*ts).data) as *mut u8;
                let mut off = 0;
                let mut i = n;
                while i > 0 {
                    let s = sbytes(tsvalue(top.sub(i as usize)));
                    std::ptr::copy_nonoverlapping(s.as_ptr(), data.add(off), s.len());
                    off += s.len();
                    i -= 1;
                }
                setsvalue(top.sub(n as usize), luaS_buffinish(L, ts));
            }
        }
        total -= n - 1;
        last -= n - 1;
        if total <= 1 {
            break;
        }
    }
}

#[inline(always)]
unsafe fn vec3(v: *const f32) -> [f32; 3] {
    [*v, *v.add(1), f32::from_bits(*(v.add(2) as *const u32))]
}

/// `luaV_doarithimpl<op>`.
pub unsafe fn luaV_doarith(L: *mut lua_State, ra: StkId, rb: *const TValue, rc: *const TValue, op: TMS) {
    let mut tempb = TValue::NIL;
    let mut tempc = TValue::NIL;
    let vb = if ttisvector(rb) { Some(vec3(vvalue(rb))) } else { None };
    let vc = if ttisvector(rc) { Some(vec3(vvalue(rc))) } else { None };
    if let (Some(b), Some(c)) = (vb, vc) {
        let r = match op {
            TM_ADD => Some([b[0] + c[0], b[1] + c[1], b[2] + c[2]]),
            TM_SUB => Some([b[0] - c[0], b[1] - c[1], b[2] - c[2]]),
            TM_MUL => Some([b[0] * c[0], b[1] * c[1], b[2] * c[2]]),
            TM_DIV => Some([b[0] / c[0], b[1] / c[1], b[2] / c[2]]),
            TM_IDIV => Some([(b[0] / c[0]).floor(), (b[1] / c[1]).floor(), (b[2] / c[2]).floor()]),
            TM_UNM => Some([-b[0], -b[1], -b[2]]),
            _ => None,
        };
        if let Some(r) = r {
            setvvalue(ra, r[0], r[1], r[2]);
            return;
        }
    } else if let Some(b) = vb {
        let c = tonumber(rc, &mut tempc);
        if !c.is_null() {
            let nc = nvalue(c) as f32;
            let r = match op {
                TM_MUL => Some([b[0] * nc, b[1] * nc, b[2] * nc]),
                TM_DIV => Some([b[0] / nc, b[1] / nc, b[2] / nc]),
                TM_IDIV => Some([(b[0] / nc).floor(), (b[1] / nc).floor(), (b[2] / nc).floor()]),
                _ => None,
            };
            if let Some(r) = r {
                setvvalue(ra, r[0], r[1], r[2]);
                return;
            }
        }
    } else if let Some(c) = vc {
        let b = tonumber(rb, &mut tempb);
        if !b.is_null() {
            let nb = nvalue(b) as f32;
            let r = match op {
                TM_MUL => Some([nb * c[0], nb * c[1], nb * c[2]]),
                TM_DIV => Some([nb / c[0], nb / c[1], nb / c[2]]),
                TM_IDIV => Some([(nb / c[0]).floor(), (nb / c[1]).floor(), (nb / c[2]).floor()]),
                _ => None,
            };
            if let Some(r) = r {
                setvvalue(ra, r[0], r[1], r[2]);
                return;
            }
        }
    }
    let b = luaV_tonumber(rb, &mut tempb);
    let c = if b.is_null() { std::ptr::null() } else { luaV_tonumber(rc, &mut tempc) };
    if !b.is_null() && !c.is_null() {
        let nb = nvalue(b);
        let nc = nvalue(c);
        let r = match op {
            TM_ADD => nb + nc,
            TM_SUB => nb - nc,
            TM_MUL => nb * nc,
            TM_DIV => nb / nc,
            TM_IDIV => luai_numidiv(nb, nc),
            TM_MOD => luai_nummod(nb, nc),
            TM_POW => crate::lmathlib::luai_numpow(nb, nc),
            TM_UNM => -nb,
            _ => 0.0,
        };
        setnvalue(ra, r);
    } else if !call_binTM(L, rb, rc, ra, op) {
        luaG_aritherror(L, rb, rc, op);
    }
}

pub unsafe fn luaV_dolen(L: *mut lua_State, ra: StkId, rb: *const TValue) {
    let tm: *const TValue;
    match (*rb).tt {
        LUA_TTABLE => {
            let h = hvalue(rb);
            let t = fasttm(L, (*h).metatable, TM_LEN);
            if t.is_null() {
                setnvalue(ra, luaH_getn(h) as f64);
                return;
            }
            tm = t;
        }
        LUA_TSTRING => {
            setnvalue(ra, (*tsvalue(rb)).len as f64);
            return;
        }
        _ => tm = luaT_gettmbyobj(L, rb, TM_LEN),
    }
    if ttisnil(tm) {
        luaG_typeerror(L, rb, "get length of");
    }
    let res = callTMres(L, ra, tm, rb, luaO_nilobject());
    if !ttisnumber(res) {
        luaG_runerror(L, "'__len' must return a number");
    }
}

#[inline(never)]
pub unsafe fn luaV_prepareFORN(L: *mut lua_State, plimit: StkId, pstep: StkId, pinit: StkId) {
    if !ttisnumber(pinit) && luaV_tonumber(pinit, pinit).is_null() {
        luaG_forerror(L, pinit, "initial value");
    }
    if !ttisnumber(plimit) && luaV_tonumber(plimit, plimit).is_null() {
        luaG_forerror(L, plimit, "limit");
    }
    if !ttisnumber(pstep) && luaV_tonumber(pstep, pstep).is_null() {
        luaG_forerror(L, pstep, "step");
    }
}

#[inline(never)]
pub unsafe fn luaV_callTM(L: *mut lua_State, nparams: i32, res: i32) {
    (*L).nCcalls += 1;
    if (*L).nCcalls as i32 >= LUAI_MAXCCALLS {
        luaD_checkCstack(L);
    }
    luaD_checkstack(L, LUA_MINSTACK);
    let top = (*L).top;
    let fun = top.sub(nparams as usize + 1);
    let ci = incr_ci(L);
    (*ci).func = fun;
    (*ci).p = std::ptr::null_mut();
    (*ci).base = fun.add(1);
    (*ci).top = top.add(LUA_MINSTACK as usize);
    (*ci).savedpc = std::ptr::null();
    (*ci).flags = 0;
    (*ci).nresults = (res >= 0) as i32;
    (*L).base = fun.add(1);
    let func = Closure::c(clvalue(fun)).f.unwrap();
    let n = func(L);
    let cip = (*L).ci.sub(1);
    if res >= 0 {
        if n > 0 {
            setobj((*cip).base.add(res as usize), (*L).top.sub(n as usize));
        } else {
            setnilvalue((*cip).base.add(res as usize));
        }
    }
    (*L).ci = cip;
    (*L).base = (*cip).base;
    (*L).top = (*cip).top;
    (*L).nCcalls -= 1;
}

#[inline(never)]
pub unsafe fn luaV_tryfuncTM(L: *mut lua_State, func: StkId) {
    let tm = luaT_gettmbyobj(L, func, TM_CALL);
    if !ttisfunction(tm) {
        luaG_typeerror(L, func, "call");
    }
    let mut p = (*L).top;
    while p > func {
        setobj(p, p.sub(1));
        p = p.sub(1);
    }
    (*L).top = (*L).top.add(1);
    setobj(func, tm);
}
