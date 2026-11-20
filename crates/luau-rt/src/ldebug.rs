//! Port of `VM/src/ldebug.cpp`: debug info and runtime error helpers.

use crate::lapi::{luaA_pushvalue, luaA_toobject, lua_pushlstring, lua_rawcheckstack};
use crate::ldo::{incr_top, luaD_throw};
use crate::lfunc::luaF_getlocal;
use crate::lgc::luaC_threadbarrier;
use crate::lobject::*;
use crate::ltm::*;
use luau_common::bytecode::Opcode;

/// `lua_Debug` with owned strings (`None` = C `NULL`).
#[derive(Default, Clone)]
pub struct lua_Debug {
    pub name: Option<Vec<u8>>,
    pub what: &'static str,
    pub source: Option<Vec<u8>>,
    pub short_src: Vec<u8>,
    pub linedefined: i32,
    pub currentline: i32,
    pub protoid: i32,
    pub bytecodeid: i32,
    pub nupvals: u8,
    pub nparams: u8,
    pub isvararg: u8,
    pub userdata: usize,
}

#[inline(always)]
pub unsafe fn pcRel(pc: *const Instruction, p: *const Proto) -> i32 {
    if !pc.is_null() && pc != (*p).code as *const Instruction {
        pc.offset_from((*p).code) as i32 - 1
    } else {
        0
    }
}

unsafe fn currentpc(ci: *mut CallInfo) -> i32 {
    pcRel((*ci).savedpc, (*ci).p)
}

unsafe fn currentline(ci: *mut CallInfo) -> i32 {
    luaG_getline((*ci).p, currentpc(ci))
}

pub unsafe fn luaG_getline(p: *const Proto, pc: i32) -> i32 {
    if (*p).lineinfo.is_null() {
        return 0;
    }
    *(*p).abslineinfo.add((pc >> (*p).linegaplog2) as usize) + *(*p).lineinfo.add(pc as usize) as i32
}

pub unsafe fn lua_getargument(L: *mut lua_State, level: i32, n: i32) -> i32 {
    if (level as u32) >= (*L).ci.offset_from((*L).base_ci) as u32 {
        return 0;
    }
    let ci = (*L).ci.sub(level as usize);
    if (*ci).flags & LUA_CALLINFO_NATIVE != 0 {
        return 0;
    }
    let fp = (*ci).p;
    let mut res = 0;
    if !fp.is_null() && n > 0 {
        if n <= (*fp).numparams as i32 {
            luaC_threadbarrier(L);
            luaA_pushvalue(L, (*ci).base.add(n as usize - 1));
            res = 1;
        } else if (*fp).is_vararg != 0 && (n as isize) < (*ci).base.offset_from((*ci).func) {
            luaC_threadbarrier(L);
            luaA_pushvalue(L, (*ci).func.add(n as usize));
            res = 1;
        }
    }
    res
}

pub unsafe fn lua_getlocal(L: *mut lua_State, level: i32, n: i32) -> Option<Vec<u8>> {
    if (level as u32) >= (*L).ci.offset_from((*L).base_ci) as u32 {
        return None;
    }
    let ci = (*L).ci.sub(level as usize);
    if (*ci).flags & LUA_CALLINFO_NATIVE != 0 {
        return None;
    }
    let fp = (*ci).p;
    let var = if fp.is_null() { std::ptr::null() } else { luaF_getlocal(fp, n, currentpc(ci)) };
    if var.is_null() {
        return None;
    }
    luaC_threadbarrier(L);
    luaA_pushvalue(L, (*ci).base.add((*var).reg as usize));
    Some(sbytes((*var).varname).to_vec())
}

pub unsafe fn lua_setlocal(L: *mut lua_State, level: i32, n: i32) -> Option<Vec<u8>> {
    if (level as u32) >= (*L).ci.offset_from((*L).base_ci) as u32 {
        return None;
    }
    let ci = (*L).ci.sub(level as usize);
    if (*ci).flags & LUA_CALLINFO_NATIVE != 0 {
        return None;
    }
    let fp = (*ci).p;
    let var = if fp.is_null() { std::ptr::null() } else { luaF_getlocal(fp, n, currentpc(ci)) };
    if !var.is_null() {
        setobj((*ci).base.add((*var).reg as usize), (*L).top.sub(1));
    }
    (*L).top = (*L).top.sub(1);
    if var.is_null() {
        None
    } else {
        Some(sbytes((*var).varname).to_vec())
    }
}

unsafe fn getfuncname(cl: *mut Closure) -> Option<Vec<u8>> {
    if (*cl).isC != 0 {
        let s = Closure::c(cl).debugname;
        if !s.is_null() {
            return Some(sbytes(s).to_vec());
        }
    } else {
        let p = Closure::proto(cl);
        if !(*p).debugname.is_null() {
            return Some(sbytes((*p).debugname).to_vec());
        }
    }
    None
}

unsafe fn auxgetinfo(what: &[u8], ar: &mut lua_Debug, f: *mut Closure, ci: *mut CallInfo) -> *mut Closure {
    let mut cl = std::ptr::null_mut();
    let proto = || if !ci.is_null() { (*ci).p } else { Closure::proto(f) };
    for &w in what {
        match w {
            b's' => {
                if (*f).isC != 0 {
                    ar.source = Some(b"=[C]".to_vec());
                    ar.what = "C";
                    ar.linedefined = -1;
                    ar.short_src = b"[C]".to_vec();
                } else {
                    let source = (*proto()).source;
                    ar.source = Some(sbytes(source).to_vec());
                    ar.what = "Lua";
                    ar.linedefined = (*proto()).linedefined;
                    ar.short_src = luaO_chunkid(sbytes(source), LUA_IDSIZE);
                }
            }
            b'l' => {
                if !ci.is_null() {
                    ar.currentline = if isLua(ci) { currentline(ci) } else { -1 };
                } else {
                    ar.currentline = if (*f).isC != 0 { -1 } else { (*Closure::proto(f)).linedefined };
                }
            }
            b'u' => ar.nupvals = (*f).nupvalues,
            b'a' => {
                if (*f).isC != 0 {
                    ar.isvararg = 1;
                    ar.nparams = 0;
                } else {
                    ar.isvararg = (*proto()).is_vararg;
                    ar.nparams = (*proto()).numparams;
                }
            }
            b'p' => {
                if (*f).isC != 0 {
                    ar.protoid = 0;
                    ar.bytecodeid = -1;
                } else {
                    let p = proto();
                    ar.protoid = (*p).funid as i32;
                    ar.bytecodeid = (*p).bytecodeid;
                }
            }
            b'n' => {
                ar.name = if !ci.is_null() { getfuncname(ci_func(ci)) } else { getfuncname(f) };
            }
            b'f' => cl = f,
            _ => {}
        }
    }
    cl
}

pub unsafe fn lua_stackdepth(L: *mut lua_State) -> i32 {
    (*L).ci.offset_from((*L).base_ci) as i32
}

pub unsafe fn lua_getinfo(L: *mut lua_State, level: i32, what: &[u8], ar: &mut lua_Debug) -> i32 {
    let mut f: *mut Closure = std::ptr::null_mut();
    let mut ci: *mut CallInfo = std::ptr::null_mut();
    if level < 0 {
        if (-level) as isize > (*L).top.offset_from((*L).base) {
            return 0;
        }
        let func = (*L).top.offset(level as isize);
        if !ttisfunction(func) {
            return 0;
        }
        f = clvalue(func);
    } else if (level as u32) < (*L).ci.offset_from((*L).base_ci) as u32 {
        ci = (*L).ci.sub(level as usize);
        f = clvalue((*ci).func);
    }
    if !f.is_null() {
        let fcl = auxgetinfo(what, ar, f, ci);
        if !fcl.is_null() {
            luaC_threadbarrier(L);
            setclvalue((*L).top, fcl);
            incr_top(L);
        }
    }
    (!f.is_null()) as i32
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

#[cold]
#[inline(never)]
pub unsafe fn luaG_typeerror(L: *mut lua_State, o: *const TValue, op: &str) -> ! {
    let t = luaT_objtypename(L, o);
    luaG_runerror_owned(L, format!("attempt to {} a {} value", op, t))
}

#[cold]
#[inline(never)]
pub unsafe fn luaG_forerror(L: *mut lua_State, o: *const TValue, what: &str) -> ! {
    let t = luaT_objtypename(L, o);
    luaG_runerror_owned(L, format!("invalid 'for' {} (number expected, got {})", what, t))
}

#[cold]
#[inline(never)]
pub unsafe fn luaG_concaterror(L: *mut lua_State, p1: StkId, p2: StkId) -> ! {
    let t1 = luaT_objtypename(L, p1);
    let t2 = luaT_objtypename(L, p2);
    luaG_runerror_owned(L, format!("attempt to concatenate {} with {}", t1, t2))
}

#[cold]
#[inline(never)]
pub unsafe fn luaG_aritherror(L: *mut lua_State, p1: *const TValue, p2: *const TValue, op: TMS) -> ! {
    let t1 = luaT_objtypenamestr(L, p1);
    let t2 = luaT_objtypenamestr(L, p2);
    let opname = &luaT_eventname[op as usize][2..];
    let s1 = String::from_utf8_lossy(sbytes(t1));
    if t1 == t2 {
        luaG_runerror_owned(L, format!("attempt to perform arithmetic ({}) on {}", opname, s1))
    } else {
        let s2 = String::from_utf8_lossy(sbytes(t2));
        luaG_runerror_owned(L, format!("attempt to perform arithmetic ({}) on {} and {}", opname, s1, s2))
    }
}

#[cold]
#[inline(never)]
pub unsafe fn luaG_ordererror(L: *mut lua_State, p1: *const TValue, p2: *const TValue, op: TMS) -> ! {
    let t1 = luaT_objtypename(L, p1);
    let t2 = luaT_objtypename(L, p2);
    let opname = if op == TM_LT {
        "<"
    } else if op == TM_LE {
        "<="
    } else {
        "=="
    };
    luaG_runerror_owned(L, format!("attempt to compare {} {} {}", t1, opname, t2))
}

#[cold]
#[inline(never)]
pub unsafe fn luaG_indexerror(L: *mut lua_State, p1: *const TValue, p2: *const TValue) -> ! {
    let t1 = luaT_objtypename(L, p1);
    let t2 = luaT_objtypename(L, p2);
    let msg = if ttisstring(p2) && (*tsvalue(p2)).len <= 64 {
        format!("attempt to index {} with '{}'", t1, String::from_utf8_lossy(sbytes(tsvalue(p2))))
    } else {
        format!("attempt to index {} with {}", t1, t2)
    };
    luaG_runerror_owned(L, msg)
}

#[cold]
#[inline(never)]
pub unsafe fn luaG_missingmembererror(L: *mut lua_State, p1: *const TValue, p2: *const TValue) -> ! {
    if !ttisstring(p2) {
        let t1 = luaT_objtypename(L, p1);
        let t2 = luaT_objtypename(L, p2);
        luaG_runerror_owned(L, format!("cannot index {} with a {}", t1, t2))
    } else {
        let t1 = luaT_objtypename(L, p1);
        let key = String::from_utf8_lossy(sbytes(tsvalue(p2)));
        luaG_runerror_owned(L, format!("this {} does not have a key named '{}'", t1, key))
    }
}

#[cold]
#[inline(never)]
pub unsafe fn luaG_methoderror(L: *mut lua_State, p1: *const TValue, p2: *const TString) -> ! {
    let t1 = luaT_objtypename(L, p1);
    let name = String::from_utf8_lossy(sbytes(p2));
    luaG_runerror_owned(L, format!("attempt to call missing method '{}' of {}", name, t1))
}

#[cold]
#[inline(never)]
pub unsafe fn luaG_readonlyerror(L: *mut lua_State) -> ! {
    luaG_runerror(L, "attempt to modify a readonly table")
}

unsafe fn pusherror(L: *mut lua_State, msg: &[u8]) {
    let ci = (*L).ci;
    if isLua(ci) {
        let source = (*(*ci).p).source;
        let mut out = luaO_chunkid(sbytes(source), LUA_IDSIZE);
        let line = currentline(ci);
        out.extend_from_slice(format!(":{}: ", line).as_bytes());
        out.extend_from_slice(msg);
        out.truncate(LUA_BUFFERSIZE - 1);
        lua_pushlstring(L, &out);
    } else {
        lua_pushlstring(L, msg);
    }
}

/// Like `luaG_runerror`, but takes the message by value and frees it before
/// throwing, so that neither this frame nor the caller's needs an unwinding
/// cleanup pad and nothing leaks when `luaD_throw` longjmps over the frames.
#[cold]
#[inline(never)]
pub unsafe fn luaG_runerror_owned(L: *mut lua_State, msg: String) -> ! {
    {
        let m = &msg.as_bytes()[..msg.len().min(LUA_BUFFERSIZE - 1)];
        lua_rawcheckstack(L, 1);
        pusherror(L, m);
    }
    drop(msg);
    luaD_throw(L, LUA_ERRRUN)
}

/// `luaG_runerror`: raises `msg` prefixed with the current Lua position.
#[cold]
#[inline(never)]
pub unsafe fn luaG_runerror(L: *mut lua_State, msg: &str) -> ! {
    luaG_runerror_bytes(L, msg.as_bytes())
}

#[cold]
#[inline(never)]
pub unsafe fn luaG_runerror_bytes(L: *mut lua_State, msg: &[u8]) -> ! {
    let msg = &msg[..msg.len().min(LUA_BUFFERSIZE - 1)];
    lua_rawcheckstack(L, 1);
    pusherror(L, msg);
    luaD_throw(L, LUA_ERRRUN)
}

pub unsafe fn luaG_pusherror(L: *mut lua_State, error: &str) {
    lua_rawcheckstack(L, 1);
    pusherror(L, error.as_bytes());
}

pub unsafe fn luaG_onbreak(L: *mut lua_State) -> bool {
    if (*L).ci == (*L).base_ci || !isLua((*L).ci) {
        return false;
    }
    (*(*(*L).ci).savedpc & 0xff) == Opcode::Break as u32
}

pub unsafe fn luaG_isnative(L: *mut lua_State, level: i32) -> i32 {
    if (level as u32) >= (*L).ci.offset_from((*L).base_ci) as u32 {
        return 0;
    }
    let ci = (*L).ci.sub(level as usize);
    ((*ci).flags & LUA_CALLINFO_NATIVE != 0) as i32
}

pub unsafe fn luaG_hasnative(_L: *mut lua_State, _level: i32) -> i32 {
    0
}

pub unsafe fn lua_singlestep(L: *mut lua_State, enabled: i32) {
    (*L).singlestep = enabled != 0;
}

unsafe fn getmaxline(p: *mut Proto) -> i32 {
    let mut result = -1;
    for i in 0..(*p).sizecode {
        result = result.max(luaG_getline(p, i));
    }
    for i in 0..(*p).sizep as usize {
        result = result.max(getmaxline(*(*p).p.add(i)));
    }
    result
}

unsafe fn getnextline(p: *mut Proto, line: i32) -> i32 {
    let mut closest = -1;
    if !(*p).lineinfo.is_null() {
        for i in 0..(*p).sizecode {
            if (*(*p).code.add(i as usize) & 0xff) == Opcode::PrepVarargs as u32 {
                continue;
            }
            let candidate = luaG_getline(p, i);
            if candidate == line {
                return line;
            }
            if candidate > line && (closest == -1 || candidate < closest) {
                closest = candidate;
            }
        }
    }
    for i in 0..(*p).sizep as usize {
        let candidate = getnextline(*(*p).p.add(i), line);
        if candidate == line {
            return line;
        }
        if candidate > line && (closest == -1 || candidate < closest) {
            closest = candidate;
        }
    }
    closest
}

pub unsafe fn luaG_breakpoint(L: *mut lua_State, p: *mut Proto, line: i32, enable: bool) {
    // native code doesn't support breakpoints: the function goes back to the
    // interpreter (like the C++ CodeGen `onDisable` callback)
    #[cfg(all(target_arch = "x86_64", windows))]
    if enable {
        crate::codegen::disable(p);
    }
    if !(*p).lineinfo.is_null() {
        for i in 0..(*p).sizecode {
            let code = (*p).code.add(i as usize);
            if (*code & 0xff) == Opcode::PrepVarargs as u32 {
                continue;
            }
            if luaG_getline(p, i) != line {
                continue;
            }
            if (*p).debuginsn.is_null() {
                (*p).debuginsn = crate::lmem::luaM_newarray(L, (*p).sizecode as usize, (*p).hdr.memcat);
                for j in 0..(*p).sizecode as usize {
                    *(*p).debuginsn.add(j) = (*(*p).code.add(j) & 0xff) as u8;
                }
            }
            let op = if enable { Opcode::Break as u32 } else { *(*p).debuginsn.add(i as usize) as u32 };
            *code = (*code & !0xff) | op;
            if enable {
                (*p).execflags |= PEF_DEBUGLOCKED;
            }
            break;
        }
    }
    for i in 0..(*p).sizep as usize {
        luaG_breakpoint(L, *(*p).p.add(i), line, enable);
    }
}

pub unsafe fn lua_breakpoint(L: *mut lua_State, funcindex: i32, line: i32, enabled: i32) -> i32 {
    let func = luaA_toobject(L, funcindex);
    let p = Closure::proto(clvalue(func));
    let target = getnextline(p, line);
    if target != -1 {
        luaG_breakpoint(L, p, target, enabled != 0);
    }
    target
}

pub unsafe fn lua_atbreakpoint(L: *mut lua_State) -> i32 {
    luaG_onbreak(L) as i32
}

/// `lua_getcoverage`: calls `callback(name, linedefined, depth, hits)` for
/// every proto in the function's tree.
pub unsafe fn lua_getcoverage(L: *mut lua_State, funcindex: i32, callback: &mut dyn FnMut(Option<&[u8]>, i32, i32, &[i32])) {
    let func = luaA_toobject(L, funcindex);
    let p = Closure::proto(clvalue(func));
    let size = getmaxline(p) + 1;
    if size <= 0 {
        return;
    }
    let mut buffer = vec![-1i32; size as usize];
    unsafe fn walk(p: *mut Proto, depth: i32, buffer: &mut [i32], cb: &mut dyn FnMut(Option<&[u8]>, i32, i32, &[i32])) {
        for b in buffer.iter_mut() {
            *b = -1;
        }
        for i in 0..(*p).sizecode {
            let insn = *(*p).code.add(i as usize);
            if (insn & 0xff) != Opcode::Coverage as u32 {
                continue;
            }
            let line = luaG_getline(p, i) as usize;
            let hits = (insn >> 8) as i32;
            if buffer[line] < hits {
                buffer[line] = hits;
            }
        }
        let name = if (*p).debugname.is_null() { None } else { Some(sbytes((*p).debugname)) };
        cb(name, (*p).linedefined, depth, buffer);
        for i in 0..(*p).sizep as usize {
            walk(*(*p).p.add(i), depth + 1, buffer, cb);
        }
    }
    walk(p, 0, &mut buffer, callback);
}

pub unsafe fn lua_debugtrace(L: *mut lua_State) -> Vec<u8> {
    let limit1 = 10;
    let limit2 = 10;
    let depth = (*L).ci.offset_from((*L).base_ci) as i32;
    let mut buf = Vec::new();
    let mut ar = lua_Debug::default();
    let mut level = 0;
    while lua_getinfo(L, level, b"sln", &mut ar) != 0 {
        if ar.source.is_some() {
            buf.extend_from_slice(&ar.short_src);
        }
        if ar.currentline > 0 {
            buf.extend_from_slice(format!(":{}", ar.currentline).as_bytes());
        }
        if let Some(n) = &ar.name {
            buf.extend_from_slice(b" function ");
            buf.extend_from_slice(n);
        }
        buf.push(b'\n');
        if depth > limit1 + limit2 && level == limit1 - 1 {
            buf.extend_from_slice(format!("... (+{} frames)\n", depth - limit1 - limit2).as_bytes());
            level = depth - limit2 - 1;
        }
        level += 1;
        ar = lua_Debug::default();
    }
    buf.truncate(4095);
    buf
}
