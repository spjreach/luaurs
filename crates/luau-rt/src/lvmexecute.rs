//! Port of `VM/src/lvmexecute.cpp`: the bytecode interpreter.
//!
//! The structure mirrors C++: `pc`, `base`, `k` and `cl` live in locals;
//! Lua->Lua calls and returns stay in the loop; slow paths save `pc`
//! (`VM_PROTECT`) and reload `base` afterwards. Flags as in the reference
//! CLI (`LuauFastpcall`, `LuauRuntimeFeedback` on).

#![allow(clippy::cognitive_complexity)]

use luau_common::bytecode::Opcode;

use crate::lbuiltins::{luauF_table, luauPF_table};
use crate::ldebug::*;
use crate::ldo::*;
use crate::lfunc::*;
use crate::lgc::*;
use crate::lnumutils::*;
use crate::lobject::*;
use crate::ltable::*;
use crate::ltm::*;
use crate::lvmutils::*;

const OP_ADD: u8 = Opcode::Add as u8;
const OP_ADD_K: u8 = Opcode::AddK as u8;
const OP_AND: u8 = Opcode::And as u8;
const OP_AND_K: u8 = Opcode::AndK as u8;
const OP_BREAK: u8 = Opcode::Break as u8;
const OP_CALL: u8 = Opcode::Call as u8;
const OP_CALL_FB: u8 = Opcode::CallFb as u8;
const OP_CLOSE_UPVALS: u8 = Opcode::CloseUpvals as u8;
const OP_CMP_PROTO: u8 = Opcode::CmpProto as u8;
const OP_CONCAT: u8 = Opcode::Concat as u8;
const OP_CONSTRUCT: u8 = Opcode::Construct as u8;
const OP_COVERAGE: u8 = Opcode::Coverage as u8;
const OP_DIV: u8 = Opcode::Div as u8;
const OP_DIV_K: u8 = Opcode::DivK as u8;
const OP_DIV_RK: u8 = Opcode::DivRk as u8;
const OP_DUP_CLOSURE: u8 = Opcode::DupClosure as u8;
const OP_DUP_TABLE: u8 = Opcode::DupTable as u8;
const OP_FAST_CALL: u8 = Opcode::FastCall as u8;
const OP_FAST_CALL1: u8 = Opcode::FastCall1 as u8;
const OP_FAST_CALL2: u8 = Opcode::FastCall2 as u8;
const OP_FAST_CALL2_K: u8 = Opcode::FastCall2K as u8;
const OP_FAST_CALL3: u8 = Opcode::FastCall3 as u8;
const OP_FAST_P_CALL: u8 = Opcode::FastPCall as u8;
const OP_FIN_CONSTRUCT: u8 = Opcode::FinConstruct as u8;
const OP_FOR_G_LOOP: u8 = Opcode::ForGLoop as u8;
const OP_FOR_G_PREP: u8 = Opcode::ForGPrep as u8;
const OP_FOR_G_PREP_INEXT: u8 = Opcode::ForGPrepInext as u8;
const OP_FOR_G_PREP_NEXT: u8 = Opcode::ForGPrepNext as u8;
const OP_FOR_N_LOOP: u8 = Opcode::ForNLoop as u8;
const OP_FOR_N_PREP: u8 = Opcode::ForNPrep as u8;
const OP_GET_GLOBAL: u8 = Opcode::GetGlobal as u8;
const OP_GET_IMPORT: u8 = Opcode::GetImport as u8;
const OP_GET_PRIVATE_FIELD: u8 = Opcode::GetPrivateField as u8;
const OP_GET_TABLE: u8 = Opcode::GetTable as u8;
const OP_GET_TABLE_KS: u8 = Opcode::GetTableKs as u8;
const OP_GET_TABLE_N: u8 = Opcode::GetTableN as u8;
const OP_GET_UDATA_KS: u8 = Opcode::GetUdataKs as u8;
const OP_GET_UPVAL: u8 = Opcode::GetUpval as u8;
const OP_GET_VARARGS: u8 = Opcode::GetVarargs as u8;
const OP_I_DIV: u8 = Opcode::IDiv as u8;
const OP_I_DIV_K: u8 = Opcode::IDivK as u8;
const OP_JUMP: u8 = Opcode::Jump as u8;
const OP_JUMP_BACK: u8 = Opcode::JumpBack as u8;
const OP_JUMP_IF: u8 = Opcode::JumpIf as u8;
const OP_JUMP_IF_EQ: u8 = Opcode::JumpIfEq as u8;
const OP_JUMP_IF_LE: u8 = Opcode::JumpIfLe as u8;
const OP_JUMP_IF_LT: u8 = Opcode::JumpIfLt as u8;
const OP_JUMP_IF_NOT: u8 = Opcode::JumpIfNot as u8;
const OP_JUMP_IF_NOT_EQ: u8 = Opcode::JumpIfNotEq as u8;
const OP_JUMP_IF_NOT_LE: u8 = Opcode::JumpIfNotLe as u8;
const OP_JUMP_IF_NOT_LT: u8 = Opcode::JumpIfNotLt as u8;
const OP_JUMP_X: u8 = Opcode::JumpX as u8;
const OP_JUMP_X_EQ_K_B: u8 = Opcode::JumpXEqKB as u8;
const OP_JUMP_X_EQ_K_N: u8 = Opcode::JumpXEqKN as u8;
const OP_JUMP_X_EQ_K_NIL: u8 = Opcode::JumpXEqKNil as u8;
const OP_JUMP_X_EQ_K_S: u8 = Opcode::JumpXEqKS as u8;
const OP_LENGTH: u8 = Opcode::Length as u8;
const OP_LOAD_B: u8 = Opcode::LoadB as u8;
const OP_LOAD_K: u8 = Opcode::LoadK as u8;
const OP_LOAD_KX: u8 = Opcode::LoadKx as u8;
const OP_LOAD_N: u8 = Opcode::LoadN as u8;
const OP_LOAD_NIL: u8 = Opcode::LoadNil as u8;
const OP_MINUS: u8 = Opcode::Minus as u8;
const OP_MOD: u8 = Opcode::Mod as u8;
const OP_MOD_K: u8 = Opcode::ModK as u8;
const OP_MOVE: u8 = Opcode::Move as u8;
const OP_MUL: u8 = Opcode::Mul as u8;
const OP_MUL_K: u8 = Opcode::MulK as u8;
const OP_NAMECALL: u8 = Opcode::Namecall as u8;
const OP_NAMECALL_UDATA: u8 = Opcode::NamecallUdata as u8;
const OP_NATIVE_CALL: u8 = Opcode::NativeCall as u8;
const OP_NEW_CLASS: u8 = Opcode::NewClass as u8;
const OP_NEW_CLASS_MEMBER: u8 = Opcode::NewClassMember as u8;
const OP_NEW_CLOSURE: u8 = Opcode::NewClosure as u8;
const OP_NEW_TABLE: u8 = Opcode::NewTable as u8;
const OP_NOP: u8 = Opcode::Nop as u8;
const OP_NOT: u8 = Opcode::Not as u8;
const OP_OR: u8 = Opcode::Or as u8;
const OP_OR_K: u8 = Opcode::OrK as u8;
const OP_POW: u8 = Opcode::Pow as u8;
const OP_POW_K: u8 = Opcode::PowK as u8;
const OP_PREP_VARARGS: u8 = Opcode::PrepVarargs as u8;
const OP_RETURN: u8 = Opcode::Return as u8;
const OP_SET_GLOBAL: u8 = Opcode::SetGlobal as u8;
const OP_SET_LIST: u8 = Opcode::SetList as u8;
const OP_SET_PRIVATE_FIELD: u8 = Opcode::SetPrivateField as u8;
const OP_SET_TABLE: u8 = Opcode::SetTable as u8;
const OP_SET_TABLE_KS: u8 = Opcode::SetTableKs as u8;
const OP_SET_TABLE_N: u8 = Opcode::SetTableN as u8;
const OP_SET_UDATA_KS: u8 = Opcode::SetUdataKs as u8;
const OP_SET_UPVAL: u8 = Opcode::SetUpval as u8;
const OP_SUB: u8 = Opcode::Sub as u8;
const OP_SUB_K: u8 = Opcode::SubK as u8;
const OP_SUB_RK: u8 = Opcode::SubRk as u8;

const NO_OVR: u32 = 256;
pub(crate) const LCT_VAL: u32 = 0;
pub(crate) const LCT_REF: u32 = 1;
const LCT_UPVAL: u32 = 2;
const FBSLOT_SEALED: u32 = 0xffff_ffff;

#[inline(always)]
fn op_of(i: u32) -> u8 {
    (i & 0xff) as u8
}
#[inline(always)]
fn ia(i: u32) -> usize {
    ((i >> 8) & 0xff) as usize
}
#[inline(always)]
fn ib(i: u32) -> usize {
    ((i >> 16) & 0xff) as usize
}
#[inline(always)]
fn ic(i: u32) -> usize {
    (i >> 24) as usize
}
#[inline(always)]
fn id(i: u32) -> isize {
    ((i as i32) >> 16) as isize
}
#[inline(always)]
fn ie(i: u32) -> isize {
    ((i as i32) >> 8) as isize
}
#[inline(always)]
fn aux_kv(aux: u32) -> usize {
    (aux & 0xffffff) as usize
}
#[inline(always)]
fn aux_kb(aux: u32) -> i32 {
    (aux & 1) as i32
}
#[inline(always)]
fn aux_not(aux: u32) -> i32 {
    (aux >> 31) as i32
}

/// `luau_execute`: runs Lua frames until a frame with
/// `LUA_CALLINFO_RETURN` returns or the thread yields/breaks.
pub unsafe fn luau_execute(L: *mut lua_State) {
    // The tracing JIT records from the single-step interpreter. With native
    // code, loops run natively and call `jit::native_loop_hook` when hot
    // (which runs the single-step interpreter just while recording);
    // without it, everything runs single-step so back edges are seen.
    let tracing = crate::jit::state::trace_enabled();
    if tracing && !crate::jit::TRACE_HOOKS.load(std::sync::atomic::Ordering::Relaxed) && std::env::var_os("LUAU_TRACE_NOTHREADED").is_none() {
        crate::jit::TRACE_HOOKS.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    // (LUAU_TRACE_THREADED: the threaded interpreter with tracing hooks even
    // without native code, to test that path)
    static FORCE_THREADED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let force_threaded = *FORCE_THREADED.get_or_init(|| std::env::var_os("LUAU_TRACE_THREADED").is_some());
    if (*L).singlestep || (tracing && !force_threaded && ENTER_STUB.load(std::sync::atomic::Ordering::Relaxed) == 0) {
        execute::<true>(L)
    } else {
        execute_threaded(L)
    }
}

#[inline(never)]
pub unsafe fn luau_callhook(L: *mut lua_State, hook: unsafe fn(*mut lua_State, *mut lua_Debug), userdata: usize) {
    let base = savestack(L, (*L).base);
    let top = savestack(L, (*L).top);
    let ci_top = savestack(L, (*(*L).ci).top);
    let status = (*L).status as i32;
    if status == LUA_YIELD || status == LUA_BREAK {
        (*L).status = 0;
        (*L).base = (*(*L).ci).base;
    }
    let cl = clvalue((*(*L).ci).func);
    let ci = (*L).ci;
    let oldsavedpc = (*ci).savedpc;
    if !(*ci).savedpc.is_null() && (*ci).savedpc != (*(*ci).p).code.add((*(*ci).p).sizecode as usize) {
        (*ci).savedpc = (*ci).savedpc.add(1);
    }
    luaD_checkstack(L, LUA_MINSTACK);
    (*(*L).ci).top = (*L).top.add(LUA_MINSTACK as usize);
    let mut ar = lua_Debug {
        currentline: if (*cl).isC != 0 { -1 } else { luaG_getline((*(*L).ci).p, pcRel((*(*L).ci).savedpc, (*(*L).ci).p)) },
        userdata,
        ..Default::default()
    };
    hook(L, &mut ar);
    (*(*L).ci).savedpc = oldsavedpc;
    (*(*L).ci).top = restorestack(L, ci_top);
    (*L).top = restorestack(L, top);
    if status == LUA_YIELD && (*L).status as i32 != LUA_YIELD {
        (*L).status = LUA_YIELD as u8;
        (*L).base = restorestack(L, base);
    } else if status == LUA_BREAK {
        (*L).status = LUA_BREAK as u8;
        (*L).base = restorestack(L, base);
    }
}

#[inline(never)]
unsafe fn luau_addexeccount(p: *mut Proto, count: f64) {
    let space = u32::MAX / 4 - (*p).execcount.min(u32::MAX / 4);
    (*p).execcount += if count < space as f64 { count as u32 } else { space };
}

/// `double -> int` truncation as the C++ VM does it (`cvttsd2si`): values
/// out of range (and NaN) give `i32::MIN` instead of Rust's saturation,
/// which costs extra instructions on every table index.
#[inline(always)]
pub(crate) fn d2i_trunc(x: f64) -> i32 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::{_mm_cvttsd_si32, _mm_set_sd};
        _mm_cvttsd_si32(_mm_set_sd(x))
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        x as i32
    }
}

#[inline(always)]
unsafe fn tm_cfunc(fn_: *const TValue) -> bool {
    ttisfunction(fn_) && (*clvalue(fn_)).isC != 0
}

#[inline(always)]
unsafe fn vec3(v: *const f32) -> (f32, f32, f32) {
    (*v, *v.add(1), f32::from_bits(*(v.add(2) as *const u32)))
}

/// Runs `L->ci` from its savedpc in the single-step interpreter (tracing
/// JIT: recording, and stretches between a trace exit and native code).
pub(crate) unsafe fn execute_single_step(L: *mut lua_State) {
    execute::<true>(L)
}

unsafe fn execute<const SS: bool>(L: *mut lua_State) {
    let mut pc: *const Instruction;
    let mut cl: *mut Closure;
    let mut base: StkId;
    let mut k: *mut TValue;
    // (true in the single-instruction steps native code runs: they only do
    // the instruction - native code calls the tracing JIT itself)
    const IN_STEP: bool = false;

    macro_rules! reload_frame {
        () => {{
            let ci = (*L).ci;
            pc = (*ci).savedpc;
            cl = clvalue((*ci).func);
            base = (*L).base;
            k = (*(*ci).p).k;
        }};
    }
    macro_rules! protect {
        ($e:expr) => {{
            (*(*L).ci).savedpc = pc;
            let r = $e;
            base = (*L).base;
            r
        }};
    }
    macro_rules! protect_pc {
        () => {
            (*(*L).ci).savedpc = pc
        };
    }
    macro_rules! reg {
        ($i:expr) => {
            base.add($i as usize)
        };
    }
    macro_rules! kv {
        ($i:expr) => {
            k.add($i as usize)
        };
    }
    macro_rules! uv {
        ($i:expr) => {
            Closure::uprefs(cl).add($i as usize)
        };
    }
    macro_rules! patch_c {
        ($p:expr, $slot:expr) => {{
            let p = $p as *mut Instruction;
            *p = ((($slot) as u8 as u32) << 24) | (0x00ffffff & *p);
        }};
    }
    macro_rules! interrupt {
        () => {{
            if let Some(f) = (*(*L).global).cb.interrupt {
                (*(*L).ci).savedpc = pc.add(1);
                f(L, -1);
                base = (*L).base;
                if (*L).status != 0 {
                    (*(*L).ci).savedpc = pc;
                    return;
                }
            }
        }};
    }
    macro_rules! check_gc {
        () => {{
            if luaC_needsGC(L) {
                (*(*L).ci).savedpc = pc;
                luaC_step(L, true);
                base = (*L).base;
            }
        }};
    }
    // arithmetic opcode body (`ADD`/`SUB`/...): number fast path, then
    // vectors, then userdata C metamethods, then the generic path.
    macro_rules! arith {
        ($insn:expr, $rb:expr, $rc:expr, $tm:expr, $num:expr, $vec:expr) => {{
            let insn = $insn;
            let ra = reg!(ia(insn));
            let rb: *const TValue = $rb;
            let rc: *const TValue = $rc;
            if ttisnumber(rb) && ttisnumber(rc) {
                setnvalue(ra, $num(nvalue(rb), nvalue(rc)));
                continue;
            }
            if let Some(v) = $vec(rb, rc) {
                let (x, y, z): (f32, f32, f32) = v;
                setvvalue(ra, x, y, z);
                continue;
            }
            let rbc = if ttisnumber(rb) { rc } else { rb };
            if ttisuserdata(rbc) {
                let f = luaT_gettmbyobj(L, rbc, $tm);
                if tm_cfunc(f) {
                    let top = (*L).top;
                    setobj(top, f);
                    setobj(top.add(1), rb);
                    setobj(top.add(2), rc);
                    (*L).top = top.add(3);
                    protect!(luaV_callTM(L, 2, ia(insn) as i32));
                    continue;
                }
            }
            protect!(luaV_doarith(L, ra, rb, rc, $tm));
            continue;
        }};
    }

    // a taken back edge (pc at the loop header): the tracing JIT may run
    // the loop's trace or record it. The single-step interpreter does so
    // every time; the threaded one, when tracing is on, every 16th time
    // (crate::jit::threaded_backedge)
    macro_rules! trace_backedge {
        () => {{
            if SS {
                if crate::jit::state::trace_enabled() {
                    let p = (*(*L).ci).p;
                    if let Some(resume) = crate::jit::on_backedge(L, p, pc.offset_from((*p).code) as u32, base) {
                        if resume == crate::jit::RESUME_FRAME {
                            { reload_frame!() }
                        } else {
                            pc = (*p).code.add(resume as usize);
                        }
                    }
                }
            } else if !IN_STEP && crate::jit::TRACE_HOOKS.load(std::sync::atomic::Ordering::Relaxed) {
                let p = (*(*L).ci).p;
                match crate::jit::threaded_backedge(L, p, pc.offset_from((*p).code) as u32, base) {
                    0 => {}
                    1 => {
                        { reload_frame!() }
                    }
                    2 => {
                        return;
                    }
                    r => {
                        pc = (*p).code.add((r - 3) as usize);
                    }
                }
            }
        }};
    }
    reload_frame!();
    let mut ovr: u32 = NO_OVR;

    loop {
        if SS && crate::jit::ss_hooks() {
            let p = (*(*L).ci).p;
            if crate::jit::is_recording() {
                crate::jit::record_step(L, p, pc.offset_from((*p).code) as u32, base);
            }
            if crate::jit::should_stop(L, p, pc) {
                (*(*L).ci).savedpc = pc;
                return;
            }
        }
        if SS {
            if let Some(step) = (*(*L).global).cb.debugstep {
                let op = op_of(*pc);
                if op != Opcode::PrepVarargs as u8 && op != Opcode::Break as u8 {
                    protect!(luau_callhook(L, step, 0));
                    if (*L).status != 0 {
                        return;
                    }
                }
            }
        }
        let insn = *pc;
        // `VM_CONTINUE` after BREAK: execute the original opcode
        let op = if ovr == NO_OVR {
            op_of(insn)
        } else {
            std::hint::cold_path();
            let o = ovr as u8;
            ovr = NO_OVR;
            o
        };

        match op {
            OP_BREAK => {
                let p = (*(*L).ci).p;
                let orig = *(*p).debuginsn.add(pc.offset_from((*p).code) as usize);
                if let Some(hook) = (*(*L).global).cb.debugbreak {
                    protect!(luau_callhook(L, hook, 0));
                    if (*L).status != 0 {
                        return;
                    }
                }
                ovr = orig as u32;
            }
            // no direct userdata access registered: rewrite to the generic op
            OP_GET_UDATA_KS => {
                let kidx = *pc.add(1) & 0xffff;
                *(pc as *mut Instruction) = (insn & 0xffffff00) | OP_GET_TABLE_KS as u32;
                *(pc.add(1) as *mut Instruction) = kidx;
            }
            OP_SET_UDATA_KS => {
                let kidx = *pc.add(1) & 0xffff;
                *(pc as *mut Instruction) = (insn & 0xffffff00) | OP_SET_TABLE_KS as u32;
                *(pc.add(1) as *mut Instruction) = kidx;
            }
            OP_NAMECALL_UDATA => {
                let kidx = *pc.add(1) & 0xffff;
                *(pc as *mut Instruction) = (insn & 0xffffff00) | OP_NAMECALL as u32;
                *(pc.add(1) as *mut Instruction) = kidx;
            }
            x @ OP_NOP => {
                pc = pc.add(1);
            }
            x @ OP_LOAD_NIL => {
                pc = pc.add(1);
                setnilvalue(reg!(ia(insn)));
            }
            x @ OP_LOAD_B => {
                pc = pc.add(1);
                setbvalue(reg!(ia(insn)), ib(insn) != 0);
                pc = pc.add(ic(insn));
            }
            x @ OP_LOAD_N => {
                pc = pc.add(1);
                setnvalue(reg!(ia(insn)), id(insn) as f64);
            }
            x @ OP_LOAD_K => {
                pc = pc.add(1);
                setobj(reg!(ia(insn)), kv!(id(insn)));
            }
            x @ OP_MOVE => {
                pc = pc.add(1);
                setobj(reg!(ia(insn)), reg!(ib(insn)));
            }
            x @ OP_GET_GLOBAL => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                let ra = reg!(ia(insn));
                let kv = kv!(aux);
                let h = (*cl).env;
                let slot = ic(insn) & (*h).nodemask8 as usize;
                let n = (*h).node.add(slot);
                if (*n).key.tt() == LUA_TSTRING && (*n).key.value.gc == (*kv).value.gc && !ttisnil(gval(n)) {
                    setobj(ra, gval(n));
                } else {
                    let mut g = TValue::NIL;
                    sethvalue(&mut g, h);
                    (*L).cachedslot = slot as i32;
                    protect!(luaV_gettable(L, &g, kv, ra));
                    patch_c!(pc.sub(2), (*L).cachedslot);
                }
            }
            x @ OP_SET_GLOBAL => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                let ra = reg!(ia(insn));
                let kv = kv!(aux);
                let h = (*cl).env;
                let slot = ic(insn) & (*h).nodemask8 as usize;
                let n = (*h).node.add(slot);
                if (*n).key.tt() == LUA_TSTRING && (*n).key.value.gc == (*kv).value.gc && !ttisnil(gval(n)) && (*h).readonly == 0 {
                    setobj(gval(n), ra);
                    luaC_barriert(L, h, ra);
                } else {
                    let mut g = TValue::NIL;
                    sethvalue(&mut g, h);
                    (*L).cachedslot = slot as i32;
                    protect!(luaV_settable(L, &g, kv, ra));
                    patch_c!(pc.sub(2), (*L).cachedslot);
                }
            }
            x @ OP_GET_UPVAL => {
                pc = pc.add(1);
                let ur = uv!(ib(insn));
                let v = if (*ur).tt == LUA_TUPVAL { (*upvalue(ur)).v as *const TValue } else { ur };
                setobj(reg!(ia(insn)), v);
            }
            x @ OP_SET_UPVAL => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let uv = upvalue(uv!(ib(insn)));
                setobj((*uv).v, ra);
                luaC_barrier(L, uv as *mut GCObject, ra);
            }
            x @ OP_CLOSE_UPVALS => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                if !(*L).openupval.is_null() && (*(*L).openupval).v >= ra {
                    luaF_close(L, ra);
                }
            }
            x @ OP_GET_IMPORT => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let kv = kv!(id(insn));
                if !ttisnil(kv) && (*(*cl).env).safeenv != 0 {
                    setobj(ra, kv);
                    pc = pc.add(1);
                } else {
                    let aux = *pc;
                    pc = pc.add(1);
                    protect!(luaV_getimport(L, (*cl).env, k, ra, aux, false));
                }
            }
            x @ OP_GET_TABLE_KS => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let kv = kv!(aux);
                if ttistable(rb) {
                    let h = hvalue(rb);
                    let slot = ic(insn) & (*h).nodemask8 as usize;
                    let n = (*h).node.add(slot);
                    if (*n).key.tt() == LUA_TSTRING && (*n).key.value.gc == (*kv).value.gc && !ttisnil(gval(n)) {
                        setobj(ra, gval(n));
                    } else if (*h).metatable.is_null() {
                        let res = luaH_getstr(h, tsvalue(kv));
                        if res != luaO_nilobject() {
                            patch_c!(pc.sub(2), gval2slot(h, res));
                        }
                        setobj(ra, res);
                    } else {
                        (*L).cachedslot = slot as i32;
                        protect!(luaV_gettable(L, rb, kv, ra));
                        patch_c!(pc.sub(2), (*L).cachedslot);
                    }
                    continue;
                }
                if ttisuserdata(rb) {
                    let f = fasttm(L, (*uvalue(rb)).metatable, TM_INDEX);
                    if !f.is_null() && tm_cfunc(f) {
                        let top = (*L).top;
                        setobj(top, f);
                        setobj(top.add(1), rb);
                        setobj(top.add(2), kv);
                        (*L).top = top.add(3);
                        (*L).cachedslot = ic(insn) as i32;
                        protect!(luaV_callTM(L, 2, ia(insn) as i32));
                        patch_c!(pc.sub(2), (*L).cachedslot);
                        continue;
                    }
                } else if ttisvector(rb) {
                    let name = sbytes(tsvalue(kv));
                    if name.len() == 1 {
                        let c = (name[0] | b' ').wrapping_sub(b'x') as u32;
                        if c < 3 {
                            let (x, y, z) = vec3(vvalue(rb));
                            setnvalue(ra, [x, y, z][c as usize] as f64);
                            continue;
                        }
                    }
                    let f = fasttm(L, (*(*L).global).mt[LUA_TVECTOR as usize], TM_INDEX);
                    if !f.is_null() && tm_cfunc(f) {
                        let top = (*L).top;
                        setobj(top, f);
                        setobj(top.add(1), rb);
                        setobj(top.add(2), kv);
                        (*L).top = top.add(3);
                        (*L).cachedslot = ic(insn) as i32;
                        protect!(luaV_callTM(L, 2, ia(insn) as i32));
                        patch_c!(pc.sub(2), (*L).cachedslot);
                        continue;
                    }
                } else if classes_enabled() && ttisobject(rb) {
                    let slot = ic(insn) as u32;
                    let inst = objectvalue(rb);
                    let lc = (*inst).lclass;
                    if slot < crate::lclass::luaR_offsettomemberlength(lc) && tsvalue(kv) == *(*lc).offsettomember.add(slot as usize) {
                        setobj(ra, crate::lclass::luaR_lookupmemberatoffset(inst, slot));
                    } else {
                        let off = luaH_getstr((*lc).memberstooffset, tsvalue(kv));
                        if ttisnil(off) {
                            protect_pc!();
                            luaG_missingmembererror(L, rb, kv);
                        }
                        let offsetnum = nvalue(off) as u32;
                        setobj(ra, crate::lclass::luaR_lookupmemberatoffset(inst, offsetnum));
                        patch_c!(pc.sub(2), offsetnum);
                    }
                    continue;
                }
                protect!(luaV_gettable(L, rb, kv, ra));
            }
            x @ OP_SET_TABLE_KS => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let kv = kv!(aux);
                if ttistable(rb) {
                    let h = hvalue(rb);
                    let slot = ic(insn) & (*h).nodemask8 as usize;
                    let n = (*h).node.add(slot);
                    if (*n).key.tt() == LUA_TSTRING && (*n).key.value.gc == (*kv).value.gc && !ttisnil(gval(n)) && (*h).readonly == 0 {
                        setobj(gval(n), ra);
                        luaC_barriert(L, h, ra);
                    } else if fastnotm((*h).metatable, TM_NEWINDEX) && (*h).readonly == 0 {
                        protect_pc!();
                        let res = luaH_setstr(L, h, tsvalue(kv));
                        patch_c!(pc.sub(2), gval2slot(h, res));
                        setobj(res, ra);
                        luaC_barriert(L, h, ra);
                    } else {
                        (*L).cachedslot = slot as i32;
                        protect!(luaV_settable(L, rb, kv, ra));
                        patch_c!(pc.sub(2), (*L).cachedslot);
                    }
                } else {
                    let f = if ttisuserdata(rb) { fasttm(L, (*uvalue(rb)).metatable, TM_NEWINDEX) } else { std::ptr::null() };
                    if !f.is_null() && tm_cfunc(f) {
                        let top = (*L).top;
                        setobj(top, f);
                        setobj(top.add(1), rb);
                        setobj(top.add(2), kv);
                        setobj(top.add(3), ra);
                        (*L).top = top.add(4);
                        (*L).cachedslot = ic(insn) as i32;
                        protect!(luaV_callTM(L, 3, -1));
                        patch_c!(pc.sub(2), (*L).cachedslot);
                    } else {
                        protect!(luaV_settable(L, rb, kv, ra));
                    }
                }
            }
            x @ OP_GET_TABLE => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let rc = reg!(ic(insn));
                if ttistable(rb) && ttisnumber(rc) {
                    let h = hvalue(rb);
                    let indexd = nvalue(rc);
                    let index = d2i_trunc(indexd);
                    if (index as u32).wrapping_sub(1) < (*h).sizearray as u32 && (*h).metatable.is_null() && index as f64 == indexd {
                        setobj(ra, (*h).array.add(index as usize - 1));
                        continue;
                    }
                }
                protect!(luaV_gettable(L, rb, rc, ra));
            }
            x @ OP_SET_TABLE => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let rc = reg!(ic(insn));
                if ttistable(rb) && ttisnumber(rc) {
                    let h = hvalue(rb);
                    let indexd = nvalue(rc);
                    let index = d2i_trunc(indexd);
                    if (index as u32).wrapping_sub(1) < (*h).sizearray as u32
                        && (*h).metatable.is_null()
                        && (*h).readonly == 0
                        && index as f64 == indexd
                    {
                        setobj((*h).array.add(index as usize - 1), ra);
                        luaC_barriert(L, h, ra);
                        continue;
                    }
                }
                protect!(luaV_settable(L, rb, rc, ra));
            }
            x @ OP_GET_TABLE_N => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let c = ic(insn);
                if ttistable(rb) {
                    let h = hvalue(rb);
                    if (c as u32) < (*h).sizearray as u32 && (*h).metatable.is_null() {
                        setobj(ra, (*h).array.add(c));
                        continue;
                    }
                }
                let mut n = TValue::NIL;
                setnvalue(&mut n, (c + 1) as f64);
                protect!(luaV_gettable(L, rb, &mut n, ra));
            }
            x @ OP_SET_TABLE_N => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let c = ic(insn);
                if ttistable(rb) {
                    let h = hvalue(rb);
                    if (c as u32) < (*h).sizearray as u32 && (*h).metatable.is_null() && (*h).readonly == 0 {
                        setobj((*h).array.add(c), ra);
                        luaC_barriert(L, h, ra);
                        continue;
                    }
                }
                let mut n = TValue::NIL;
                setnvalue(&mut n, (c + 1) as f64);
                protect!(luaV_settable(L, rb, &mut n, ra));
            }
            x @ OP_NEW_CLOSURE => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let pv = *(*(*(*L).ci).p).p.add(id(insn) as usize);
                protect_pc!();
                let ncl = luaF_newLclosure(L, (*pv).nups as i32, (*cl).env, pv);
                setclvalue(ra, ncl);
                for ui in 0..(*pv).nups as usize {
                    let uinsn = *pc;
                    pc = pc.add(1);
                    let dst = Closure::uprefs(ncl).add(ui);
                    match ia(uinsn) as u32 {
                        LCT_VAL => setobj(dst, reg!(ib(uinsn))),
                        LCT_REF => setupvalue(dst, luaF_findupval(L, reg!(ib(uinsn)))),
                        _ => setobj(dst, uv!(ib(uinsn))),
                    }
                }
                protect!(luaC_checkGC(L));
            }
            x @ OP_NAMECALL => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                let mut ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let kv = kv!(aux);
                if ttistable(rb) {
                    let h = hvalue(rb);
                    let n = (*h).node.add(((*tsvalue(kv)).hash & (sizenode(h) as u32 - 1)) as usize);
                    let mut mtn: *mut LuaNode = std::ptr::null_mut();
                    if (*n).key.tt() == LUA_TSTRING && (*n).key.value.gc == (*kv).value.gc && !ttisnil(gval(n)) {
                        setobj(ra.add(1), rb);
                        setobj(ra, gval(n));
                    } else if gnext(n) == 0 && {
                        let mt = fasttm(L, (*h).metatable, TM_INDEX);
                        !mt.is_null() && ttistable(mt) && {
                            let mh = hvalue(mt);
                            mtn = (*mh).node.add(ic(insn) & (*mh).nodemask8 as usize);
                            (*mtn).key.tt() == LUA_TSTRING && (*mtn).key.value.gc == (*kv).value.gc && !ttisnil(gval(mtn))
                        }
                    } {
                        setobj(ra.add(1), rb);
                        setobj(ra, gval(mtn));
                    } else {
                        setobj(ra.add(1), rb);
                        (*L).cachedslot = ic(insn) as i32;
                        protect!(luaV_gettable(L, rb, kv, ra));
                        patch_c!(pc.sub(2), (*L).cachedslot);
                        ra = reg!(ia(insn));
                        if ttisnil(ra) {
                            luaG_methoderror(L, ra.add(1), tsvalue(kv));
                        }
                    }
                } else {
                    let mt = if ttisuserdata(rb) { (*uvalue(rb)).metatable } else { (*(*L).global).mt[(*rb).tt as usize] };
                    let f = fasttm(L, mt, TM_NAMECALL);
                    let tmi;
                    if !f.is_null() {
                        setobj(ra.add(1), rb);
                        setobj(ra, f);
                        (*L).namecall = tsvalue(kv);
                    } else if {
                        tmi = fasttm(L, mt, TM_INDEX);
                        !tmi.is_null() && ttistable(tmi)
                    } {
                        let h = hvalue(tmi);
                        let slot = ic(insn) & (*h).nodemask8 as usize;
                        let n = (*h).node.add(slot);
                        if (*n).key.tt() == LUA_TSTRING && (*n).key.value.gc == (*kv).value.gc && !ttisnil(gval(n)) {
                            setobj(ra.add(1), rb);
                            setobj(ra, gval(n));
                        } else {
                            setobj(ra.add(1), rb);
                            (*L).cachedslot = slot as i32;
                            protect!(luaV_gettable(L, rb, kv, ra));
                            patch_c!(pc.sub(2), (*L).cachedslot);
                            ra = reg!(ia(insn));
                            if ttisnil(ra) {
                                luaG_methoderror(L, ra.add(1), tsvalue(kv));
                            }
                        }
                    } else if classes_enabled() && ttisobject(rb) {
                        let slot = ic(insn) as u32;
                        let inst = objectvalue(rb);
                        let lc = (*inst).lclass;
                        if slot < crate::lclass::luaR_offsettomemberlength(lc) && tsvalue(kv) == *(*lc).offsettomember.add(slot as usize) {
                            setobj(ra.add(1), rb);
                            setobj(ra, crate::lclass::luaR_lookupmemberatoffset(inst, slot));
                        } else {
                            let off = luaH_getstr((*lc).memberstooffset, tsvalue(kv));
                            if ttisnil(off) {
                                protect_pc!();
                                luaG_missingmembererror(L, rb, kv);
                            }
                            let offsetnum = nvalue(off) as u32;
                            setobj(ra.add(1), rb);
                            setobj(ra, crate::lclass::luaR_lookupmemberatoffset(inst, offsetnum));
                            patch_c!(pc.sub(2), offsetnum);
                        }
                    } else {
                        setobj(ra.add(1), rb);
                        protect!(luaV_gettable(L, rb, kv, ra));
                        ra = reg!(ia(insn));
                        if ttisnil(ra) {
                            luaG_methoderror(L, ra.add(1), tsvalue(kv));
                        }
                    }
                }
            }
            x @ (OP_CALL | OP_CALL_FB) => {
                interrupt!();
                let isfb = x == Opcode::CallFb as u8;
                pc = pc.add(1);
                let mut feedback_slot = FBSLOT_SEALED;
                if isfb {
                    feedback_slot = *pc;
                    pc = pc.add(1);
                }
                let ra = reg!(ia(insn));
                let nparams = ib(insn) as i32 - 1;
                let nresults = ic(insn) as i32 - 1;
                let mut argtop = if nparams == LUA_MULTRET { (*L).top } else { ra.add(1 + nparams as usize) };
                if !ttisfunction(ra) {
                    if isfb && feedback_slot != FBSLOT_SEALED {
                        *(pc.sub(1) as *mut Instruction) = FBSLOT_SEALED;
                    }
                    protect_pc!();
                    luaV_tryfuncTM(L, ra);
                    argtop = argtop.add(1);
                }
                let ccl = clvalue(ra);
                (*(*L).ci).savedpc = pc;
                let ci = incr_ci(L);
                (*ci).func = ra;
                (*ci).p = if (*ccl).isC != 0 { std::ptr::null_mut() } else { Closure::proto(ccl) };
                (*ci).base = ra.add(1);
                (*ci).top = argtop.add((*ccl).stacksize as usize);
                (*ci).savedpc = std::ptr::null();
                (*ci).flags = 0;
                (*ci).nresults = nresults;
                (*L).base = (*ci).base;
                (*L).top = argtop;
                luaD_checkstackfornewci(L, (*ccl).stacksize as i32);
                if (*ccl).isC == 0 {
                    let p = Closure::proto(ccl);
                    (*p).execcount += 1;
                    let ci = (*L).ci;
                    let mut argi = (*L).top;
                    let argend = (*L).base.add((*p).numparams as usize);
                    while argi < argend {
                        setnilvalue(argi);
                        argi = argi.add(1);
                    }
                    (*L).top = if (*p).is_vararg != 0 { argi } else { (*ci).top };
                    pc = if SS { (*p).code } else { (*p).codeentry };
                    cl = ccl;
                    base = (*L).base;
                    k = (*p).k;
                } else {
                    if isfb && feedback_slot != FBSLOT_SEALED {
                        *(pc.sub(1) as *mut Instruction) = FBSLOT_SEALED;
                    }
                    let func = Closure::c(ccl).f.unwrap_unchecked();
                    let n = func(L);
                    if n < 0 {
                        return;
                    }
                    let ci = (*L).ci;
                    let cip = ci.sub(1);
                    let mut res = (*ci).func;
                    let mut vali = (*L).top.sub(n as usize);
                    let valend = (*L).top;
                    let mut i = nresults;
                    while i != 0 && vali < valend {
                        setobj(res, vali);
                        res = res.add(1);
                        vali = vali.add(1);
                        i -= 1;
                    }
                    while i > 0 {
                        setnilvalue(res);
                        res = res.add(1);
                        i -= 1;
                    }
                    (*L).ci = cip;
                    (*L).base = (*cip).base;
                    (*L).top = if nresults == LUA_MULTRET { res } else { (*cip).top };
                    base = (*L).base;
                }
            }
            x @ OP_RETURN => {
                interrupt!();
                pc = pc.add(1);
                let ra = base.add(ia(insn));
                let b = ib(insn) as i32 - 1;
                let ci = (*L).ci;
                let cip = ci.sub(1);
                let mut res = (*ci).func;
                let mut vali = ra;
                let valend = if b == LUA_MULTRET { (*L).top } else { ra.add(b as usize) };
                let nresults = (*ci).nresults;
                let mut i = nresults;
                while i != 0 && vali < valend {
                    setobj(res, vali);
                    res = res.add(1);
                    vali = vali.add(1);
                    i -= 1;
                }
                while i > 0 {
                    setnilvalue(res);
                    res = res.add(1);
                    i -= 1;
                }
                (*L).ci = cip;
                (*L).base = (*cip).base;
                (*L).top = if nresults == LUA_MULTRET { res } else { (*cip).top };
                if (*ci).flags & LUA_CALLINFO_RETURN != 0 {
                    return;
                }
                pc = (*cip).savedpc;
                cl = clvalue((*cip).func);
                base = (*L).base;
                k = (*(*cip).p).k;
            }
            x @ OP_JUMP => {
                pc = pc.add(1).offset(id(insn));
            }
            x @ OP_JUMP_IF => {
                pc = pc.add(1);
                if !l_isfalse(reg!(ia(insn))) {
                    pc = pc.offset(id(insn));
                }
            }
            x @ OP_JUMP_IF_NOT => {
                pc = pc.add(1);
                if l_isfalse(reg!(ia(insn))) {
                    pc = pc.offset(id(insn));
                }
            }
            x @ (OP_JUMP_IF_EQ | OP_JUMP_IF_NOT_EQ) => {
                let iseq = x == Opcode::JumpIfEq as u8;
                pc = pc.add(1);
                let aux = *pc;
                let ra = reg!(ia(insn));
                let rb = reg!(aux);
                let jump = id(insn);
                // `take(eq)`: branch when the comparison outcome matches the opcode
                macro_rules! take {
                    ($eq:expr) => {{
                        pc = pc.offset(if ($eq) == iseq { jump } else { 1 });
                        continue;
                    }};
                }
                if (*ra).tt != (*rb).tt {
                    take!(false);
                }
                match (*ra).tt {
                    LUA_TNIL => take!(true),
                    LUA_TBOOLEAN => take!(bvalue(ra) == bvalue(rb)),
                    LUA_TLIGHTUSERDATA => take!(pvalue(ra) == pvalue(rb) && lightuserdatatag(ra) == lightuserdatatag(rb)),
                    LUA_TNUMBER => take!(nvalue(ra) == nvalue(rb)),
                    LUA_TINTEGER => take!(lvalue(ra) == lvalue(rb)),
                    LUA_TVECTOR => take!(luai_veceq(vvalue(ra), vvalue(rb))),
                    LUA_TSTRING | LUA_TFUNCTION | LUA_TTHREAD | LUA_TBUFFER => take!(gcvalue(ra) == gcvalue(rb)),
                    LUA_TCLASS => take!(gcvalue(ra) == gcvalue(rb)),
                    LUA_TTABLE => {
                        if (*hvalue(ra)).metatable == (*hvalue(rb)).metatable && fasttm(L, (*hvalue(ra)).metatable, TM_EQ).is_null() {
                            take!(hvalue(ra) == hvalue(rb));
                        }
                    }
                    LUA_TUSERDATA => {
                        if (*uvalue(ra)).metatable == (*uvalue(rb)).metatable {
                            let f = fasttm(L, (*uvalue(ra)).metatable, TM_EQ);
                            if f.is_null() {
                                take!(uvalue(ra) == uvalue(rb));
                            } else if tm_cfunc(f) {
                                let top = (*L).top;
                                setobj(top, f);
                                setobj(top.add(1), ra);
                                setobj(top.add(2), rb);
                                let res = top.offset_from(base) as i32;
                                (*L).top = top.add(3);
                                protect!(luaV_callTM(L, 2, res));
                                take!(!l_isfalse(base.add(res as usize)));
                            }
                        }
                    }
                    _ => {}
                }
                let res = protect!(luaV_equalval(L, ra, rb));
                take!(res == 1);
            }
            x @ (OP_JUMP_IF_LE | OP_JUMP_IF_NOT_LE | OP_JUMP_IF_LT | OP_JUMP_IF_NOT_LT) =>
            {
                let islt = x == Opcode::JumpIfLt as u8 || x == Opcode::JumpIfNotLt as u8;
                let want = x == Opcode::JumpIfLe as u8 || x == Opcode::JumpIfLt as u8;
                pc = pc.add(1);
                let aux = *pc;
                let ra = reg!(ia(insn));
                let rb = reg!(aux);
                let cond = if ttisnumber(ra) && ttisnumber(rb) {
                    if islt {
                        nvalue(ra) < nvalue(rb)
                    } else {
                        nvalue(ra) <= nvalue(rb)
                    }
                } else if ttisstring(ra) && ttisstring(rb) {
                    let c = luaV_strcmp(tsvalue(ra), tsvalue(rb));
                    if islt {
                        c < 0
                    } else {
                        c <= 0
                    }
                } else if islt {
                    protect!(luaV_lessthan(L, ra, rb)) == 1
                } else {
                    protect!(luaV_lessequal(L, ra, rb)) == 1
                };
                pc = pc.offset(if cond == want { id(insn) } else { 1 });
            }
            x @ OP_ADD => {
                pc = pc.add(1);
                arith!(
                    insn,
                    reg!(ib(insn)),
                    reg!(ic(insn)),
                    TM_ADD,
                    |a: f64, b: f64| a + b,
                    |rb: *const TValue, rc: *const TValue| if ttisvector(rb) && ttisvector(rc) {
                        let (a, b, c) = vec3(vvalue(rb));
                        let (d, e, f) = vec3(vvalue(rc));
                        Some((a + d, b + e, c + f))
                    } else {
                        None
                    }
                );
            }
            x @ OP_SUB => {
                pc = pc.add(1);
                arith!(
                    insn,
                    reg!(ib(insn)),
                    reg!(ic(insn)),
                    TM_SUB,
                    |a: f64, b: f64| a - b,
                    |rb: *const TValue, rc: *const TValue| if ttisvector(rb) && ttisvector(rc) {
                        let (a, b, c) = vec3(vvalue(rb));
                        let (d, e, f) = vec3(vvalue(rc));
                        Some((a - d, b - e, c - f))
                    } else {
                        None
                    }
                );
            }
            x @ OP_MUL => {
                pc = pc.add(1);
                arith!(insn, reg!(ib(insn)), reg!(ic(insn)), TM_MUL, |a: f64, b: f64| a * b, |rb, rc| vec_muldiv(rb, rc, false));
            }
            x @ OP_DIV => {
                pc = pc.add(1);
                arith!(insn, reg!(ib(insn)), reg!(ic(insn)), TM_DIV, |a: f64, b: f64| a / b, |rb, rc| vec_muldiv(rb, rc, true));
            }
            x @ OP_I_DIV => {
                pc = pc.add(1);
                arith!(insn, reg!(ib(insn)), reg!(ic(insn)), TM_IDIV, luai_numidiv, |rb, rc| vec_idiv(rb, rc));
            }
            x @ OP_MOD => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let rc = reg!(ic(insn));
                if ttisnumber(rb) && ttisnumber(rc) {
                    setnvalue(ra, luai_nummod(nvalue(rb), nvalue(rc)));
                } else {
                    protect!(luaV_doarith(L, ra, rb, rc, TM_MOD));
                }
            }
            x @ OP_POW => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let rc = reg!(ic(insn));
                if ttisnumber(rb) && ttisnumber(rc) {
                    setnvalue(ra, crate::lmathlib::luai_numpow(nvalue(rb), nvalue(rc)));
                } else {
                    protect!(luaV_doarith(L, ra, rb, rc, TM_POW));
                }
            }
            x @ OP_ADD_K => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let kv = kv!(ic(insn));
                if ttisnumber(rb) {
                    setnvalue(ra, nvalue(rb) + nvalue(kv));
                } else {
                    protect!(luaV_doarith(L, ra, rb, kv, TM_ADD));
                }
            }
            x @ OP_SUB_K => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let kv = kv!(ic(insn));
                if ttisnumber(rb) {
                    setnvalue(ra, nvalue(rb) - nvalue(kv));
                } else {
                    protect!(luaV_doarith(L, ra, rb, kv, TM_SUB));
                }
            }
            x @ OP_MUL_K => {
                pc = pc.add(1);
                arith!(insn, reg!(ib(insn)), kv!(ic(insn)), TM_MUL, |a: f64, b: f64| a * b, |rb, rc| vec_muldiv(rb, rc, false));
            }
            x @ OP_DIV_K => {
                pc = pc.add(1);
                arith!(insn, reg!(ib(insn)), kv!(ic(insn)), TM_DIV, |a: f64, b: f64| a / b, |rb, rc| vec_muldiv(rb, rc, true));
            }
            x @ OP_I_DIV_K => {
                pc = pc.add(1);
                arith!(insn, reg!(ib(insn)), kv!(ic(insn)), TM_IDIV, luai_numidiv, |rb, rc| vec_idiv(rb, rc));
            }
            x @ OP_MOD_K => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let kv = kv!(ic(insn));
                if ttisnumber(rb) {
                    setnvalue(ra, luai_nummod(nvalue(rb), nvalue(kv)));
                } else {
                    protect!(luaV_doarith(L, ra, rb, kv, TM_MOD));
                }
            }
            x @ OP_POW_K => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                let kv = kv!(ic(insn));
                if ttisnumber(rb) {
                    let nb = nvalue(rb);
                    let nk = nvalue(kv);
                    let r = if nk == 2.0 {
                        nb * nb
                    } else if nk == 0.5 {
                        nb.sqrt()
                    } else if nk == 3.0 {
                        nb * nb * nb
                    } else {
                        crate::lmathlib::luai_numpow(nb, nk)
                    };
                    setnvalue(ra, r);
                } else {
                    protect!(luaV_doarith(L, ra, rb, kv, TM_POW));
                }
            }
            x @ OP_AND => {
                pc = pc.add(1);
                let rb = reg!(ib(insn));
                setobj(reg!(ia(insn)), if l_isfalse(rb) { rb } else { reg!(ic(insn)) });
            }
            x @ OP_OR => {
                pc = pc.add(1);
                let rb = reg!(ib(insn));
                setobj(reg!(ia(insn)), if l_isfalse(rb) { reg!(ic(insn)) } else { rb });
            }
            x @ OP_AND_K => {
                pc = pc.add(1);
                let rb = reg!(ib(insn));
                setobj(reg!(ia(insn)), if l_isfalse(rb) { rb as *const TValue } else { kv!(ic(insn)) });
            }
            x @ OP_OR_K => {
                pc = pc.add(1);
                let rb = reg!(ib(insn));
                setobj(reg!(ia(insn)), if l_isfalse(rb) { kv!(ic(insn)) as *const TValue } else { rb });
            }
            x @ OP_CONCAT => {
                pc = pc.add(1);
                let b = ib(insn) as i32;
                let c = ic(insn) as i32;
                protect!(luaV_concat(L, c - b + 1, c));
                setobj(reg!(ia(insn)), base.add(b as usize));
                protect!(luaC_checkGC(L));
            }
            x @ OP_NOT => {
                pc = pc.add(1);
                let res = l_isfalse(reg!(ib(insn)));
                setbvalue(reg!(ia(insn)), res);
            }
            x @ OP_MINUS => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                if ttisnumber(rb) {
                    setnvalue(ra, -nvalue(rb));
                } else if ttisvector(rb) {
                    let (a, b, c) = vec3(vvalue(rb));
                    setvvalue(ra, -a, -b, -c);
                } else {
                    let f = if ttisuserdata(rb) { luaT_gettmbyobj(L, rb, TM_UNM) } else { std::ptr::null() };
                    if !f.is_null() && tm_cfunc(f) {
                        let top = (*L).top;
                        setobj(top, f);
                        setobj(top.add(1), rb);
                        (*L).top = top.add(2);
                        protect!(luaV_callTM(L, 1, ia(insn) as i32));
                    } else {
                        protect!(luaV_doarith(L, ra, rb, rb, TM_UNM));
                    }
                }
            }
            x @ OP_LENGTH => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                if ttistable(rb) {
                    let h = hvalue(rb);
                    if fastnotm((*h).metatable, TM_LEN) {
                        setnvalue(ra, luaH_getn(h) as f64);
                    } else {
                        protect!(luaV_dolen(L, ra, rb));
                    }
                } else if ttisstring(rb) {
                    setnvalue(ra, (*tsvalue(rb)).len as f64);
                } else {
                    protect!(luaV_dolen(L, ra, rb));
                }
            }
            x @ OP_NEW_TABLE => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                let b = ib(insn) as i32;
                protect_pc!();
                sethvalue(reg!(ia(insn)), luaH_new(L, aux as i32, if b == 0 { 0 } else { 1 << (b - 1) }));
                protect!(luaC_checkGC(L));
            }
            x @ OP_DUP_TABLE => {
                pc = pc.add(1);
                protect_pc!();
                sethvalue(reg!(ia(insn)), luaH_clone(L, hvalue(kv!(id(insn)))));
                protect!(luaC_checkGC(L));
            }
            x @ OP_SET_LIST => {
                let index = *pc.add(1);
                pc = pc.add(2);
                let ra = reg!(ia(insn));
                let rb = base.add(ib(insn));
                let mut c = ic(insn) as i32 - 1;
                if c == LUA_MULTRET {
                    c = (*L).top.offset_from(rb) as i32;
                    (*L).top = (*(*L).ci).top;
                }
                if !ttistable(ra) {
                    return;
                }
                let h = hvalue(ra);
                let last = index as i32 + c - 1;
                if last > (*h).sizearray {
                    protect_pc!();
                    luaH_resizearray(L, h, last);
                }
                let array = (*h).array;
                for i in 0..c as usize {
                    setobj(array.add(index as usize + i - 1), rb.add(i));
                }
                luaC_barrierfast(L, h);
            }
            x @ OP_FOR_N_PREP => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                if !ttisnumber(ra) || !ttisnumber(ra.add(1)) || !ttisnumber(ra.add(2)) {
                    protect_pc!();
                    luaV_prepareFORN(L, ra, ra.add(1), ra.add(2));
                }
                let limit = nvalue(ra);
                let step = nvalue(ra.add(1));
                let idx = nvalue(ra.add(2));
                if if step > 0.0 { idx <= limit } else { limit <= idx } {
                    luau_addexeccount((*(*L).ci).p, (limit - idx) / step + 1.0);
                } else {
                    pc = pc.offset(id(insn));
                }
            }
            x @ OP_FOR_N_LOOP => {
                interrupt!();
                check_gc!();
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let limit = nvalue(ra);
                let step = nvalue(ra.add(1));
                let idx = nvalue(ra.add(2)) + step;
                setnvalue(ra.add(2), idx);
                // a real (predicted) branch: a cmov here would chain the next
                // instruction fetch to the floating point compare
                if if step > 0.0 { idx <= limit } else { limit <= idx } {
                    pc = pc.offset(id(insn));
                    trace_backedge!();
                } else {
                    std::hint::cold_path();
                }
            }
            x @ OP_FOR_G_PREP => {
                pc = pc.add(1);
                let mut ra = reg!(ia(insn));
                if !ttisfunction(ra) {
                    let mt = if ttistable(ra) {
                        (*hvalue(ra)).metatable
                    } else if ttisuserdata(ra) {
                        (*uvalue(ra)).metatable
                    } else {
                        std::ptr::null_mut()
                    };
                    let mut f = fasttm(L, mt, TM_ITER);
                    if f.is_null() && classes_enabled() && ttisobject(ra) {
                        f = luaT_gettmbyobj(L, ra, TM_ITER);
                        if ttisnil(f) {
                            protect_pc!();
                            luaG_typeerror(L, ra, "iterate over");
                        }
                    }
                    if !f.is_null() {
                        setobj(ra.add(1), ra);
                        setobj(ra, f);
                        (*L).top = ra.add(2);
                        protect!(luaD_call(L, ra, 3));
                        (*L).top = (*(*L).ci).top;
                        ra = reg!(ia(insn));
                        if ttisnil(ra) {
                            protect_pc!();
                            luaG_typeerror(L, ra, "call");
                        }
                    } else if !fasttm(L, mt, TM_CALL).is_null() {
                        // table/userdata with __call: generic loop
                    } else if ttistable(ra) {
                        luau_addexeccount((*(*L).ci).p, ((*hvalue(ra)).sizearray + sizenode(hvalue(ra))) as f64);
                        setobj(ra.add(1), ra);
                        setpvalue(ra.add(2), std::ptr::null_mut(), LU_TAG_ITERATOR);
                        setnilvalue(ra);
                    } else {
                        protect_pc!();
                        luaG_typeerror(L, ra, "iterate over");
                    }
                }
                pc = pc.offset(id(insn));
            }
            x @ OP_FOR_G_LOOP => {
                interrupt!();
                check_gc!();
                pc = pc.add(1);
                let mut ra = reg!(ia(insn));
                let aux = *pc;
                if ttisnil(ra) && ttistable(ra.add(1)) {
                    let h = hvalue(ra.add(1));
                    let mut index = pvalue(ra.add(2)) as usize as i32;
                    let sizearray = (*h).sizearray;
                    if aux as i32 > 2 {
                        for i in 2..aux as i32 {
                            setnilvalue(ra.add(3 + i as usize));
                        }
                    }
                    if (aux as i32) < 0 && ((index as u32) >= sizearray as u32 || ttisnil((*h).array.add(index as usize))) {
                        pc = pc.add(1);
                        continue;
                    }
                    let mut found = false;
                    while (index as u32) < sizearray as u32 {
                        let e = (*h).array.add(index as usize);
                        if !ttisnil(e) {
                            setpvalue(ra.add(2), (index + 1) as usize as *mut u8, LU_TAG_ITERATOR);
                            setnvalue(ra.add(3), (index + 1) as f64);
                            setobj(ra.add(4), e);
                            pc = pc.offset(id(insn));
                            found = true;
                            break;
                        }
                        index += 1;
                    }
                    if found {
                        if SS && crate::jit::state::trace_enabled() {
                            let p = (*(*L).ci).p;
                            if let Some(resume) = crate::jit::on_backedge(L, p, pc.offset_from((*p).code) as u32, base) {
                                pc = (*p).code.add(resume as usize);
                            }
                        }
                        continue;
                    }
                    let sizenode = 1i32 << (*h).lsizenode;
                    while ((index - sizearray) as u32) < sizenode as u32 {
                        let n = (*h).node.add((index - sizearray) as usize);
                        if !ttisnil(gval(n)) {
                            setpvalue(ra.add(2), (index + 1) as usize as *mut u8, LU_TAG_ITERATOR);
                            getnodekey(ra.add(3), n);
                            setobj(ra.add(4), gval(n));
                            pc = pc.offset(id(insn));
                            found = true;
                            break;
                        }
                        index += 1;
                    }
                    if !found {
                        pc = pc.add(1);
                    } else {
                        trace_backedge!();
                    }
                } else {
                    (*(*(*L).ci).p).execcount += 1;
                    setobj(ra.add(5), ra.add(2));
                    setobj(ra.add(4), ra.add(1));
                    setobj(ra.add(3), ra);
                    (*L).top = ra.add(6);
                    let yielded = protect!(luaD_performcally(L, ra.add(3), aux as u8 as i32));
                    if yielded {
                        return;
                    }
                    (*L).top = (*(*L).ci).top;
                    ra = reg!(ia(insn));
                    setobj(ra.add(2), ra.add(3));
                    pc = pc.offset(if ttisnil(ra.add(3)) { 1 } else { id(insn) });
                }
            }
            x @ OP_FOR_G_PREP_INEXT => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                if (*(*cl).env).safeenv != 0 && ttistable(ra.add(1)) && ttisnumber(ra.add(2)) && nvalue(ra.add(2)) == 0.0 {
                    luau_addexeccount((*(*L).ci).p, (*hvalue(ra.add(1))).sizearray as f64);
                    setnilvalue(ra);
                    setpvalue(ra.add(2), std::ptr::null_mut(), LU_TAG_ITERATOR);
                } else if !ttisfunction(ra) {
                    protect_pc!();
                    luaG_typeerror(L, ra, "iterate over");
                }
                pc = pc.offset(id(insn));
            }
            x @ OP_FOR_G_PREP_NEXT => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                if (*(*cl).env).safeenv != 0 && ttistable(ra.add(1)) && ttisnil(ra.add(2)) {
                    let h = hvalue(ra.add(1));
                    luau_addexeccount((*(*L).ci).p, ((*h).sizearray + sizenode(h)) as f64);
                    setnilvalue(ra);
                    setpvalue(ra.add(2), std::ptr::null_mut(), LU_TAG_ITERATOR);
                } else if !ttisfunction(ra) {
                    protect_pc!();
                    luaG_typeerror(L, ra, "iterate over");
                }
                pc = pc.offset(id(insn));
            }
            x @ OP_NATIVE_CALL => {
                // no native code generation: continue in the interpreter
                pc = pc.add(1);
            }
            x @ OP_GET_VARARGS => {
                pc = pc.add(1);
                let b = ib(insn) as i32 - 1;
                let n = base.offset_from((*(*L).ci).func) as i32 - (*(*(*L).ci).p).numparams as i32 - 1;
                if b == LUA_MULTRET {
                    protect!(luaD_checkstack(L, n));
                    let ra = reg!(ia(insn));
                    for j in 0..n as usize {
                        setobj(ra.add(j), base.sub(n as usize).add(j));
                    }
                    (*L).top = ra.add(n as usize);
                } else {
                    let ra = reg!(ia(insn));
                    let mut j = 0;
                    while j < b && j < n {
                        setobj(ra.add(j as usize), base.sub(n as usize).add(j as usize));
                        j += 1;
                    }
                    let mut j = n.max(0);
                    while j < b {
                        setnilvalue(ra.add(j as usize));
                        j += 1;
                    }
                }
            }
            x @ OP_DUP_CLOSURE => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let kcl = clvalue(kv!(id(insn)));
                protect_pc!();
                let mut ncl = if (*kcl).env == (*cl).env {
                    kcl
                } else {
                    luaF_newLclosure(L, (*kcl).nupvalues as i32, (*cl).env, Closure::proto(kcl))
                };
                setclvalue(ra, ncl);
                let mut ui: i32 = 0;
                while ui < (*kcl).nupvalues as i32 {
                    let uinsn = *pc.add(ui as usize);
                    let uvp: *mut TValue = if ia(uinsn) as u32 == LCT_VAL { reg!(ib(uinsn)) } else { uv!(ib(uinsn)) };
                    let dst = Closure::uprefs(ncl).add(ui as usize);
                    if ncl == kcl && luaO_rawequalObj(dst, uvp) {
                        ui += 1;
                        continue;
                    }
                    if ncl == kcl && (*kcl).preload == 0 {
                        ncl = luaF_newLclosure(L, (*kcl).nupvalues as i32, (*cl).env, Closure::proto(kcl));
                        setclvalue(ra, ncl);
                        ui = 0;
                        continue;
                    }
                    setobj(dst, uvp);
                    luaC_barrier(L, ncl as *mut GCObject, uvp);
                    ui += 1;
                }
                (*ncl).preload = 0;
                if kcl != ncl {
                    protect!(luaC_checkGC(L));
                }
                pc = pc.add((*kcl).nupvalues as usize);
            }
            x @ OP_PREP_VARARGS => {
                pc = pc.add(1);
                let numparams = ia(insn);
                protect!(luaD_checkstack(L, (*cl).stacksize as i32 + numparams as i32));
                let fixed = base;
                base = (*L).top;
                for i in 0..numparams {
                    setobj(base.add(i), fixed.add(i));
                    setnilvalue(fixed.add(i));
                }
                let ci = (*L).ci;
                (*ci).base = base;
                (*ci).top = base.add((*cl).stacksize as usize);
                (*L).base = base;
                (*L).top = (*ci).top;
            }
            x @ OP_JUMP_BACK => {
                interrupt!();
                check_gc!();
                (*(*(*L).ci).p).execcount += 1;
                pc = pc.add(1).offset(id(insn));
                trace_backedge!();
            }
            x @ OP_LOAD_KX => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                setobj(reg!(ia(insn)), kv!(aux));
            }
            x @ OP_JUMP_X => {
                interrupt!();
                check_gc!();
                pc = pc.add(1).offset(ie(insn));
            }
            x @ OP_FAST_CALL => {
                pc = pc.add(1);
                let bfid = ia(insn);
                let skip = ic(insn);
                let call = *pc.add(skip);
                let ra = reg!(ia(call));
                let mut nparams = ib(call) as i32 - 1;
                let nresults = ic(call) as i32 - 1;
                if nparams == LUA_MULTRET {
                    nparams = (*L).top.offset_from(ra) as i32 - 1;
                }
                if (*(*cl).env).safeenv != 0 {
                    protect_pc!();
                    let n = luauF_table[bfid](L, ra, ra.add(1), nresults, ra.add(2), nparams);
                    if n >= 0 {
                        (*L).top = if nresults == LUA_MULTRET { ra.add(n as usize) } else { (*(*L).ci).top };
                        pc = pc.add(skip + 1);
                    }
                }
            }
            x @ OP_COVERAGE => {
                let mut hits = (insn >> 8) as i32;
                if hits < (1 << 23) - 1 {
                    hits += 1;
                }
                *(pc as *mut Instruction) = ((hits as u32) << 8) | (insn & 0xff);
                pc = pc.add(1);
            }
            x @ OP_SUB_RK => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let kv = kv!(ib(insn));
                let rc = reg!(ic(insn));
                if ttisnumber(rc) {
                    setnvalue(ra, nvalue(kv) - nvalue(rc));
                } else {
                    protect!(luaV_doarith(L, ra, kv, rc, TM_SUB));
                }
            }
            x @ OP_DIV_RK => {
                pc = pc.add(1);
                let ra = reg!(ia(insn));
                let kv = kv!(ib(insn));
                let rc = reg!(ic(insn));
                if ttisnumber(rc) {
                    setnvalue(ra, nvalue(kv) / nvalue(rc));
                } else if ttisvector(rc) {
                    let nb = nvalue(kv) as f32;
                    let (a, b, c) = vec3(vvalue(rc));
                    setvvalue(ra, nb / a, nb / b, nb / c);
                } else {
                    protect!(luaV_doarith(L, ra, kv, rc, TM_DIV));
                }
            }
            x @ OP_FAST_CALL1 => {
                pc = pc.add(1);
                let bfid = ia(insn);
                let arg = reg!(ib(insn));
                let skip = ic(insn);
                let call = *pc.add(skip);
                let ra = reg!(ia(call));
                let nresults = ic(call) as i32 - 1;
                if (*(*cl).env).safeenv != 0 {
                    protect_pc!();
                    let n = luauF_table[bfid](L, ra, arg, nresults, std::ptr::null_mut(), 1);
                    if n >= 0 {
                        if nresults == LUA_MULTRET {
                            (*L).top = ra.add(n as usize);
                        }
                        pc = pc.add(skip + 1);
                    }
                }
            }
            x @ (OP_FAST_CALL2 | OP_FAST_CALL2_K) => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                let bfid = ia(insn);
                let skip = ic(insn) - 1;
                let arg1 = reg!(ib(insn));
                let arg2: *mut TValue = if x == Opcode::FastCall2 as u8 { reg!(aux) } else { kv!(aux) };
                let call = *pc.add(skip);
                let ra = reg!(ia(call));
                let nresults = ic(call) as i32 - 1;
                if (*(*cl).env).safeenv != 0 {
                    protect_pc!();
                    let n = luauF_table[bfid](L, ra, arg1, nresults, arg2, 2);
                    if n >= 0 {
                        if nresults == LUA_MULTRET {
                            (*L).top = ra.add(n as usize);
                        }
                        pc = pc.add(skip + 1);
                    }
                }
            }
            x @ OP_FAST_CALL3 => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                let bfid = ia(insn);
                let skip = ic(insn) - 1;
                let arg1 = reg!(ib(insn));
                let arg2 = reg!(aux & 0xff);
                let arg3 = reg!((aux >> 8) & 0xff);
                let call = *pc.add(skip);
                let ra = reg!(ia(call));
                let nresults = ic(call) as i32 - 1;
                if (*(*cl).env).safeenv != 0 {
                    protect_pc!();
                    let top = (*L).top;
                    setobj(top, arg2);
                    setobj(top.add(1), arg3);
                    let n = luauF_table[bfid](L, ra, arg1, nresults, top, 3);
                    if n >= 0 {
                        if nresults == LUA_MULTRET {
                            (*L).top = ra.add(n as usize);
                        }
                        pc = pc.add(skip + 1);
                    }
                }
            }
            x @ OP_JUMP_X_EQ_K_NIL => {
                pc = pc.add(1);
                let aux = *pc;
                let ra = reg!(ia(insn));
                pc = pc.offset(if (((*ra).tt - 1) ^ aux as i32) < 0 { id(insn) } else { 1 });
            }
            x @ OP_JUMP_X_EQ_K_B => {
                pc = pc.add(1);
                let aux = *pc;
                let ra = reg!(ia(insn));
                let eq = (ttisboolean(ra) && bvalue(ra) == aux_kb(aux)) as i32;
                pc = pc.offset(if eq != aux_not(aux) { id(insn) } else { 1 });
            }
            x @ OP_JUMP_X_EQ_K_N => {
                pc = pc.add(1);
                let aux = *pc;
                let ra = reg!(ia(insn));
                let kv = kv!(aux_kv(aux));
                let eq = (ttisnumber(ra) && nvalue(ra) == nvalue(kv)) as i32;
                pc = pc.offset(if eq != aux_not(aux) { id(insn) } else { 1 });
            }
            x @ OP_JUMP_X_EQ_K_S => {
                pc = pc.add(1);
                let aux = *pc;
                let ra = reg!(ia(insn));
                let kv = kv!(aux_kv(aux));
                let eq = (ttisstring(ra) && gcvalue(ra) == gcvalue(kv)) as i32;
                pc = pc.offset(if eq != aux_not(aux) { id(insn) } else { 1 });
            }
            x @ OP_CMP_PROTO => {
                let funid = *pc.add(1);
                pc = pc.add(2);
                let ra = reg!(ia(insn));
                if !ttisfunction(ra) || (*clvalue(ra)).isC != 0 || (*Closure::proto(clvalue(ra))).funid != funid {
                    pc = pc.offset(id(insn) - 1);
                }
            }
            x @ OP_FAST_P_CALL => {
                pc = pc.add(1);
                interrupt!();
                let pfid = ia(insn);
                let skip = ic(insn);
                let call = *pc.add(skip);
                let ra = reg!(ia(call));
                let mut nparams = ib(call) as i32 - 1;
                let nresults = ic(call) as i32 - 1;
                if nparams == LUA_MULTRET {
                    nparams = (*L).top.offset_from(ra) as i32 - 1;
                }
                if (*(*cl).env).safeenv == 0 || (*L).nCcalls > (*L).baseCcalls {
                    continue;
                }
                let errfunc = luauPF_table[pfid](L, ra, nparams);
                if errfunc < 0 {
                    continue;
                }
                (*(*L).ci).savedpc = pc.add(skip + 1);
                (*L).top = ra.add(1 + nparams as usize);
                luau_pushhandlerci(L, ra, errfunc, nresults);
                let callerfunc = (*(*L).ci).base.add(errfunc as usize);
                let calleeresults = if nresults > 0 { nresults - 1 } else { nresults };
                let pr = luau_precall(L, callerfunc, calleeresults);
                if pr == PCRLUA {
                    (*(*L).ci).flags |= LUA_CALLINFO_RETURN;
                    let ci = (*L).ci;
                    cl = clvalue((*ci).func);
                    let p = (*ci).p;
                    pc = if SS { (*p).code } else { (*p).codeentry };
                    base = (*L).base;
                    k = (*p).k;
                } else if pr == PCRC {
                    luau_pospcallsuccess(L);
                    base = (*L).base;
                    pc = pc.add(skip + 1);
                } else {
                    return;
                }
            }
            x @ OP_NEW_CLASS_MEMBER => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                let ra = reg!(ia(insn));
                let rc = reg!(ic(insn));
                protect_pc!();
                if ib(insn) == 0 {
                    crate::lclass::luaR_addpublicstaticmember(L, classvalue(ra), tsvalue(kv!(aux)), rc);
                } else {
                    crate::lclass::luaR_addprivatestaticmember(L, classvalue(ra), (aux & 0xff) as u8, rc);
                }
            }
            x @ OP_NEW_CLASS => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                let ra = reg!(ia(insn));
                let sup = ib(insn);
                protect_pc!();
                setobj(ra, kv!(aux));
                let newcls = classvalue(ra);
                (*newcls).isopen = (ic(insn) & 1) != 0;
                if sup != 0xff {
                    let rb = reg!(sup);
                    if !ttisclass(rb) {
                        luaG_typeerror(L, rb, "extend");
                    }
                    let inherited = crate::lclass::luaR_inheritclass(L, newcls, classvalue(rb));
                    setclassvalue(ra, inherited);
                }
            }
            x @ OP_CONSTRUCT => {
                let fbslot = *pc.add(1);
                pc = pc.add(2);
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                protect_pc!();
                crate::lclass::luaR_tryconstructobject(L, ra, rb, fbslot);
            }
            x @ OP_FIN_CONSTRUCT => {
                pc = pc.add(1);
                let arg = reg!(ia(insn));
                if ttisobject(arg) {
                    let skip = ic(insn);
                    let call = *pc.add(skip);
                    let ra = reg!(ia(call));
                    let nresults = ic(call) as i32 - 1;
                    let mut res = ra;
                    setobj(res, arg);
                    res = res.add(1);
                    for _ in 1..nresults.max(1) {
                        setnilvalue(res);
                        res = res.add(1);
                    }
                    (*L).top = if nresults == LUA_MULTRET { ra.add(1) } else { (*(*L).ci).top };
                    pc = pc.add(skip + 1);
                }
            }
            x @ OP_GET_PRIVATE_FIELD => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                protect_pc!();
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                crate::lclass::getprivatefield(L, ra, rb, uv!(ic(insn)), aux);
            }
            x @ OP_SET_PRIVATE_FIELD => {
                let aux = *pc.add(1);
                pc = pc.add(2);
                protect_pc!();
                let ra = reg!(ia(insn));
                let rb = reg!(ib(insn));
                crate::lclass::setprivatefield(L, ra, rb, uv!(ic(insn)), aux);
            }
            _ => {
                // CAPTURE and unknown opcodes never execute
                debug_assert!(false, "unexpected opcode {}", op);
                return;
            }
        }
    }
}

/// vector +/- vector for the threaded fast paths
#[inline(always)]
unsafe fn vec_addsub_fast(rb: *const TValue, rc: *const TValue, sub: bool) -> Option<(f32, f32, f32)> {
    if ttisvector(rb) && ttisvector(rc) {
        let (a, b, c) = vec3(vvalue(rb));
        let (d, e, g) = vec3(vvalue(rc));
        Some(if sub { (a - d, b - e, c - g) } else { (a + d, b + e, c + g) })
    } else {
        None
    }
}

#[inline(always)]
unsafe fn vec_muldiv(rb: *const TValue, rc: *const TValue, div: bool) -> Option<(f32, f32, f32)> {
    let f = |a: f32, b: f32| if div { a / b } else { a * b };
    if ttisvector(rb) && ttisnumber(rc) {
        let (a, b, c) = vec3(vvalue(rb));
        let n = nvalue(rc) as f32;
        Some((f(a, n), f(b, n), f(c, n)))
    } else if ttisvector(rb) && ttisvector(rc) {
        let (a, b, c) = vec3(vvalue(rb));
        let (d, e, g) = vec3(vvalue(rc));
        Some((f(a, d), f(b, e), f(c, g)))
    } else if ttisnumber(rb) && ttisvector(rc) {
        let n = nvalue(rb) as f32;
        let (a, b, c) = vec3(vvalue(rc));
        Some((f(n, a), f(n, b), f(n, c)))
    } else {
        None
    }
}

#[inline(always)]
unsafe fn vec_idiv(rb: *const TValue, rc: *const TValue) -> Option<(f32, f32, f32)> {
    if ttisvector(rb) && ttisnumber(rc) {
        let (a, b, c) = vec3(vvalue(rb));
        let n = nvalue(rc) as f32;
        Some((luai_floorf(a / n), luai_floorf(b / n), luai_floorf(c / n)))
    } else {
        None
    }
}

pub unsafe fn luau_finishop(L: *mut lua_State) {
    let ci = (*L).ci;
    (*ci).flags &= !LUA_CALLINFO_OPYIELD;
    let base = (*L).base;
    let mut pc = (*ci).savedpc;
    let insn = *pc.sub(1);
    if op_of(insn) == Opcode::ForGLoop as u8 {
        let ra = base.add(ia(insn));
        setobj(ra.add(2), ra.add(3));
        pc = pc.offset(if ttisnil(ra.add(3)) { 1 } else { id(insn) });
    }
    (*(*L).ci).savedpc = pc;
}

pub unsafe fn luau_precall(L: *mut lua_State, func: StkId, nresults: i32) -> i32 {
    if !ttisfunction(func) {
        luaV_tryfuncTM(L, func);
    }
    let ccl = clvalue(func);
    let ci = incr_ci(L);
    (*ci).func = func;
    (*ci).p = if (*ccl).isC != 0 { std::ptr::null_mut() } else { Closure::proto(ccl) };
    (*ci).base = func.add(1);
    (*ci).top = (*L).top.add((*ccl).stacksize as usize);
    (*ci).savedpc = std::ptr::null();
    (*ci).flags = 0;
    (*ci).nresults = nresults;
    (*L).base = (*ci).base;
    luaD_checkstackfornewci(L, (*ccl).stacksize as i32);
    let ci = (*L).ci;
    if (*ccl).isC == 0 {
        let p = Closure::proto(ccl);
        (*p).execcount += 1;
        let mut argi = (*L).top;
        let argend = (*L).base.add((*p).numparams as usize);
        while argi < argend {
            setnilvalue(argi);
            argi = argi.add(1);
        }
        (*L).top = if (*p).is_vararg != 0 { argi } else { (*ci).top };
        (*ci).savedpc = (*p).code;
        PCRLUA
    } else {
        let f = Closure::c(ccl).f.unwrap_unchecked();
        let n = f(L);
        if n < 0 {
            return PCRYIELD;
        }
        let ci = (*L).ci;
        let cip = ci.sub(1);
        let mut res = (*ci).func;
        let mut vali = (*L).top.sub(n as usize);
        let valend = (*L).top;
        let mut i = nresults;
        while i != 0 && vali < valend {
            setobj(res, vali);
            res = res.add(1);
            vali = vali.add(1);
            i -= 1;
        }
        while i > 0 {
            setnilvalue(res);
            res = res.add(1);
            i -= 1;
        }
        (*L).ci = cip;
        (*L).base = (*cip).base;
        (*L).top = res;
        PCRC
    }
}

pub unsafe fn luau_pushhandlerci(L: *mut lua_State, funcslot: StkId, errfunc: i32, nresults: i32) {
    let ccl = clvalue(funcslot);
    let ci = incr_ci(L);
    (*ci).func = funcslot;
    (*ci).p = std::ptr::null_mut();
    (*ci).base = funcslot.add(1);
    (*ci).top = (*L).top.add((*ccl).stacksize as usize);
    (*ci).flags = LUA_CALLINFO_HANDLE | LUA_CALLINFO_PCALL;
    (*ci).nresults = nresults;
    (*ci).set_errfunc(errfunc);
    luaD_checkstackfornewci(L, (*ccl).stacksize as i32);
}

pub unsafe fn luau_poscall(L: *mut lua_State, first: StkId) {
    let ci = (*L).ci;
    let cip = ci.sub(1);
    let mut res = (*ci).func;
    let mut vali = first;
    let valend = (*L).top;
    let mut i = (*ci).nresults;
    while i != 0 && vali < valend {
        setobj_fresh(res, vali);
        res = res.add(1);
        vali = vali.add(1);
        i -= 1;
    }
    while i > 0 {
        setnilvalue(res);
        res = res.add(1);
        i -= 1;
    }
    (*L).ci = cip;
    (*L).base = (*cip).base;
    (*L).top = if (*ci).nresults == LUA_MULTRET { res } else { (*cip).top };
}

pub unsafe fn luau_pospcallsuccess(L: *mut lua_State) {
    let ci = (*L).ci;
    let cip = ci.sub(1);
    let calleeresults = if (*ci).nresults > 0 { (*ci).nresults - 1 } else { (*ci).nresults };
    let mut res = (*ci).func;
    setbvalue(res, true);
    res = res.add(1);
    if (*ci).errfunc() != 0 {
        let mut vali = (*L).base.add((*ci).errfunc() as usize);
        let valend = if calleeresults == LUA_MULTRET { (*L).top } else { vali.add(calleeresults as usize) };
        while vali < valend {
            setobj(res, vali);
            res = res.add(1);
            vali = vali.add(1);
        }
    } else {
        res = res.add(if calleeresults == LUA_MULTRET {
            (*L).top.offset_from((*L).base) as usize
        } else {
            calleeresults as usize
        });
    }
    (*L).ci = cip;
    (*L).base = (*cip).base;
    (*L).top = if calleeresults == LUA_MULTRET { res } else { (*cip).top };
}

/// `luaV_getimport` (`lvmload.cpp`).
pub unsafe fn luaV_getimport(L: *mut lua_State, env: *mut LuaTable, k: *mut TValue, res: StkId, id: u32, propagatenil: bool) {
    let count = id >> 30;
    let id0 = ((id >> 20) & 1023) as usize;
    let id1 = ((id >> 10) & 1023) as usize;
    let id2 = (id & 1023) as usize;
    let resp = savestack(L, res);
    let mut g = TValue::NIL;
    sethvalue(&mut g, env);
    luaV_gettable(L, &g, k.add(id0), res);
    if count < 2 {
        return;
    }
    let res = restorestack(L, resp);
    if !propagatenil || !ttisnil(res) {
        luaV_gettable(L, res, k.add(id1), res);
    }
    if count < 3 {
        return;
    }
    let res = restorestack(L, resp);
    if !propagatenil || !ttisnil(res) {
        luaV_gettable(L, res, k.add(id2), res);
    }
}

/// Address of the native code entry stub (`codegen::mem`), 0 until native
/// code exists.
pub static ENTER_STUB: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Continues the current Lua frame at `pc` in its native code (the frame's
/// function must have native code).
#[inline(never)]
unsafe extern "sysv64-unwind" fn native_enter(L: *mut lua_State, pc: *const Instruction, base: StkId, k: *mut TValue) {
    #[cfg(all(target_arch = "x86_64", windows))]
    {
        let p = (*(*L).ci).p;
        let np = (*p).execdata as *const crate::codegen::x64::NativeProto;
        let target = (*np).table[pc.offset_from((*p).code) as usize];
        if target != 0 {
            let stub: Handler = std::mem::transmute(ENTER_STUB.load(std::sync::atomic::Ordering::Relaxed));
            become stub(L, target as *const Instruction, base, k);
        }
    }
    become dispatch(L, pc, base, k)
}

/// Address of the interpreter's handler table (native code exits).
pub(crate) fn handlers_table() -> usize {
    HANDLERS.as_ptr() as usize
}

/// Native code address of `pc` in `p` (0 when `p` has no native code).
#[cfg(all(target_arch = "x86_64", windows))]
#[inline(always)]
pub(crate) unsafe fn native_addr(p: *mut Proto, pc: *const Instruction) -> usize {
    let np = (*p).execdata as *const crate::codegen::x64::NativeProto;
    if np.is_null() {
        return 0;
    }
    (&(*np).table)[pc.offset_from((*p).code) as usize]
}

/// Results of [`native_call`] / [`native_return`] (anything else is the
/// native address to continue at, after reloading base and k from `L->ci`).
pub const NATIVE_EXIT: usize = 0; // nothing done: run this instruction in the interpreter
pub const NATIVE_LEAVE: usize = 1; // leave the VM (yield / return to C)
pub const NATIVE_INTERP: usize = 2; // continue interpreting `L->ci` at its savedpc
pub const NATIVE_CONT: usize = 3; // [`native_call_c`]: continue with the next instruction

/// CALL / CALL_FB from native code: performs the call like the interpreter's
/// fast path, without leaving the native frame. Lua callees with native code
/// continue at their entry; C functions run to completion here.
#[cfg(all(target_arch = "x86_64", windows))]
pub(crate) unsafe extern "sysv64-unwind" fn native_call(L: *mut lua_State, pc: *const Instruction, base: StkId, _k: *mut TValue) -> usize {
    let insn = *pc;
    if (*(*L).global).cb.interrupt.is_some() {
        return NATIVE_EXIT;
    }
    let ra = base.add(ia(insn) as usize);
    if !ttisfunction(ra) || (*L).ci == (*L).end_ci {
        return NATIVE_EXIT;
    }
    let ccl = clvalue(ra);
    let isfb = (insn & 0xff) as u8 == Opcode::CallFb as u8;
    let npc = pc.add(if isfb { 2 } else { 1 });
    let nparams = ib(insn) as i32 - 1;
    let nresults = ic(insn) as i32 - 1;
    let argtop = if nparams == LUA_MULTRET { (*L).top } else { ra.add(1 + nparams as usize) };
    if (*ccl).isC != 0 {
        let caller = (*(*L).ci).p;
        if !call_c(L, pc, ccl, ra, npc, isfb, argtop, nresults) {
            return NATIVE_LEAVE;
        }
        let t = native_addr(caller, npc);
        return if t != 0 { t } else { NATIVE_INTERP };
    }
    let p = Closure::proto(ccl);
    if (*p).execdata.is_null() {
        return NATIVE_EXIT;
    }
    if ((*L).stack_last as usize).wrapping_sub(argtop as usize) as isize <= (*ccl).stacksize as isize * std::mem::size_of::<TValue>() as isize {
        return NATIVE_EXIT;
    }
    (*(*L).ci).savedpc = npc;
    let ci = (*L).ci.add(1);
    (*L).ci = ci;
    (*ci).func = ra;
    (*ci).p = p;
    (*ci).base = ra.add(1);
    (*ci).top = argtop.add((*ccl).stacksize as usize);
    (*ci).savedpc = std::ptr::null();
    (*ci).flags = 0;
    (*ci).nresults = nresults;
    let nbase = ra.add(1);
    (*L).base = nbase;
    (*p).execcount += 1;
    let mut argi = argtop;
    let argend = nbase.add((*p).numparams as usize);
    while argi < argend {
        setnilvalue(argi);
        argi = argi.add(1);
    }
    (*L).top = if (*p).is_vararg != 0 { argi } else { (*ci).top };
    native_addr(p, (*p).codeentry)
}

/// Runs C closure `ccl` in R(A) for a CALL / CALL_FB at `pc` from native
/// code; false when it yielded.
#[cfg(all(target_arch = "x86_64", windows))]
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn call_c(L: *mut lua_State, pc: *const Instruction, ccl: *mut Closure, ra: StkId, npc: *const Instruction, isfb: bool, argtop: StkId, nresults: i32) -> bool {
    (*(*L).ci).savedpc = npc;
    let ci = incr_ci(L);
    (*ci).func = ra;
    (*ci).p = std::ptr::null_mut();
    (*ci).base = ra.add(1);
    (*ci).top = argtop.add((*ccl).stacksize as usize);
    (*ci).savedpc = std::ptr::null();
    (*ci).flags = 0;
    (*ci).nresults = nresults;
    (*L).base = (*ci).base;
    (*L).top = argtop;
    luaD_checkstackfornewci(L, (*ccl).stacksize as i32);
    if isfb && *pc.add(1) != FBSLOT_SEALED {
        *(pc.add(1) as *mut Instruction) = FBSLOT_SEALED;
    }
    let func = Closure::c(ccl).f.unwrap_unchecked();
    let n = func(L);
    if n < 0 {
        return false;
    }
    let ci = (*L).ci;
    let cip = ci.sub(1);
    let mut res = (*ci).func;
    let mut vali = (*L).top.sub(n as usize);
    let valend = (*L).top;
    let mut i = nresults;
    while i != 0 && vali < valend {
        setobj_fresh(res, vali);
        res = res.add(1);
        vali = vali.add(1);
        i -= 1;
    }
    while i > 0 {
        setnilvalue(res);
        res = res.add(1);
        i -= 1;
    }
    (*L).ci = cip;
    (*L).base = (*cip).base;
    (*L).top = if nresults == LUA_MULTRET { res } else { (*cip).top };
    true
}

/// CALL / CALL_FB of a C function from native code, which has checked that
/// no interrupt is set, R(A) is a C closure and a CallInfo is free. Returns
/// NATIVE_CONT to go on with the next instruction in the same frame.
#[cfg(all(target_arch = "x86_64", windows))]
pub(crate) unsafe extern "sysv64-unwind" fn native_call_c(L: *mut lua_State, pc: *const Instruction, base: StkId, _k: *mut TValue) -> usize {
    let insn = *pc;
    let ra = base.add(ia(insn) as usize);
    let isfb = (insn & 0xff) as u8 == Opcode::CallFb as u8;
    let npc = pc.add(if isfb { 2 } else { 1 });
    let nparams = ib(insn) as i32 - 1;
    let argtop = if nparams == LUA_MULTRET { (*L).top } else { ra.add(1 + nparams as usize) };
    if call_c(L, pc, clvalue(ra), ra, npc, isfb, argtop, ic(insn) as i32 - 1) { NATIVE_CONT } else { NATIVE_LEAVE }
}

/// CALL (not CALL_FB) of a C function with a fixed argument count and `NR`
/// results (0..=2), as `native_call_c` with the shape known at compile time:
/// `ra` = the callee's slot, `argtop` = one past the last argument, `npc` =
/// the next instruction.
#[cfg(all(target_arch = "x86_64", windows))]
pub(crate) unsafe extern "sysv64-unwind" fn native_call_c_fixed<const NR: i32>(L: *mut lua_State, ra: StkId, argtop: StkId, npc: *const Instruction) -> usize {
    let ccl = clvalue(ra);
    (*(*L).ci).savedpc = npc;
    let ci = incr_ci(L);
    (*ci).func = ra;
    (*ci).p = std::ptr::null_mut();
    (*ci).base = ra.add(1);
    (*ci).top = argtop.add((*ccl).stacksize as usize);
    (*ci).savedpc = std::ptr::null();
    (*ci).flags = 0;
    (*ci).nresults = NR;
    (*L).base = (*ci).base;
    (*L).top = argtop;
    luaD_checkstackfornewci(L, (*ccl).stacksize as i32);
    let func = Closure::c(ccl).f.unwrap_unchecked();
    let n = func(L);
    if n < 0 {
        return NATIVE_LEAVE;
    }
    let ci = (*L).ci;
    let cip = ci.sub(1);
    // (the stack may have moved: everything is re-read from the CallInfo)
    let res = (*ci).func;
    let top = (*L).top;
    let mut i = 0;
    while i < NR {
        if i < n {
            setobj_fresh(res.add(i as usize), top.sub((n - i) as usize));
        } else {
            setnilvalue(res.add(i as usize));
        }
        i += 1;
    }
    (*L).ci = cip;
    (*L).base = (*cip).base;
    (*L).top = (*cip).top;
    NATIVE_CONT
}

/// RETURN from native code: pops the frame like the interpreter's fast path
/// and continues the caller natively when it has native code.
#[cfg(all(target_arch = "x86_64", windows))]
pub(crate) unsafe extern "sysv64-unwind" fn native_return(L: *mut lua_State, pc: *const Instruction, base: StkId, _k: *mut TValue) -> usize {
    if (*(*L).global).cb.interrupt.is_some() {
        return NATIVE_EXIT;
    }
    let insn = *pc;
    let ra = base.add(ia(insn) as usize);
    let b = ib(insn) as i32 - 1;
    let ci = (*L).ci;
    let cip = ci.sub(1);
    let mut res = (*ci).func;
    let mut vali = ra;
    let valend = if b == LUA_MULTRET { (*L).top } else { ra.add(b as usize) };
    let nresults = (*ci).nresults;
    let mut i = nresults;
    while i != 0 && vali < valend {
        setobj_fresh(res, vali);
        res = res.add(1);
        vali = vali.add(1);
        i -= 1;
    }
    while i > 0 {
        setnilvalue(res);
        res = res.add(1);
        i -= 1;
    }
    (*L).ci = cip;
    (*L).base = (*cip).base;
    (*L).top = if nresults == LUA_MULTRET { res } else { (*cip).top };
    if (*ci).flags & LUA_CALLINFO_RETURN != 0 {
        return NATIVE_LEAVE;
    }
    let t = native_addr((*cip).p, (*cip).savedpc);
    if t != 0 { t } else { NATIVE_INTERP }
}

/// Dispatch after a call-frame switch: enters native code when the frame's
/// function has it.
#[inline(always)]
unsafe extern "sysv64-unwind" fn dispatch_frame(L: *mut lua_State, pc: *const Instruction, base: StkId, k: *mut TValue) {
    if !(*(*(*L).ci).p).execdata.is_null() {
        become native_enter(L, pc, base, k);
    }
    become dispatch(L, pc, base, k)
}

include!("lvmthreaded.rs");
