//! Baseline x64 code generator: translates a function's bytecode into native
//! code, one template per instruction.
//!
//! Register state inside native code (System V register usage, matching the
//! interpreter's threaded handlers):
//!   r12 = lua_State*, r13 = base (stack frame), r14 = constants (k)
//! Native code is entered through the shared entry stub (rsi = target),
//! which saves rbx/rbp/r12-r15 and establishes the frame described by the
//! region's unwind info.
//!
//! Instruction handling:
//! * hot instructions get inline machine code with a fallback for the
//!   uncommon operand types;
//! * other instructions call the interpreter's single-instruction step
//!   function (generated from the same source as the interpreter) and
//!   continue at the pc it returns;
//! * instructions that switch call frames (calls, returns) leave native code
//!   by jumping to the interpreter handler; the interpreter re-enters native
//!   code when it later resumes a function that has it.

use super::asm::{Asm, Cond, Label, R, X};
use crate::lobject::*;
use crate::lvmexecute::{handler_address, native_call_c, NATIVE_EXIT, NATIVE_INTERP, NATIVE_LEAVE};
use luau_common::bytecode::Opcode as O;
use std::mem::offset_of;

/// Native code attached to a `Proto` (`Proto::execdata`).
#[repr(C)]
pub struct NativeProto {
    /// `table.as_ptr()` (read by native code at offset 0)
    pub table_ptr: *const usize,
    /// native address of each instruction (0 for AUX words)
    pub table: Box<[usize]>,
    /// the machine code block (address, length)
    pub code: (usize, usize),
    /// the version this one replaced (frames may still have been running
    /// it); freed together with this one
    pub prev: *mut NativeProto,
}

impl Drop for NativeProto {
    fn drop(&mut self) {
        super::mem::with_code(|m| {
            m.release(self.code.0, self.code.1);
            Some(())
        });
        if !self.prev.is_null() {
            drop(unsafe { Box::from_raw(self.prev) });
        }
    }
}

pub(super) const NUM_TAG: u64 = (LUA_TNUMBER as u64) << 32;
pub(super) const BOOL_TAG: u64 = (LUA_TBOOLEAN as u64) << 32;
pub(super) const TT: i32 = 12; // offset of TValue::tt
pub(super) const TAGWORD: i32 = 8; // offset of the 8-byte (extra, tt) word

pub(super) const OFF_BASE: i32 = offset_of!(lua_State, base) as i32;
pub(super) const OFF_TOP: i32 = offset_of!(lua_State, top) as i32;
pub(super) const OFF_END_CI: i32 = offset_of!(lua_State, end_ci) as i32;
pub(super) const OFF_STACK_LAST: i32 = offset_of!(lua_State, stack_last) as i32;
pub(super) const OFF_CI_BASE: i32 = offset_of!(CallInfo, base) as i32;
pub(super) const OFF_CI_TOP: i32 = offset_of!(CallInfo, top) as i32;
pub(super) const OFF_CI_NRESULTS: i32 = offset_of!(CallInfo, nresults) as i32;
pub(super) const OFF_CI_FLAGS: i32 = offset_of!(CallInfo, flags) as i32;
pub(super) const SIZE_CI: i32 = std::mem::size_of::<CallInfo>() as i32;
pub(super) const OFF_CL_ISC: i32 = offset_of!(Closure, isC) as i32;
pub(super) const OFF_CL_STACKSIZE: i32 = offset_of!(Closure, stacksize) as i32;
pub(super) const OFF_CL_P: i32 = (offset_of!(Closure, u) + offset_of!(LClosure, p)) as i32;
pub(super) const OFF_P_EXECDATA: i32 = offset_of!(Proto, execdata) as i32;
pub(super) const OFF_P_CODE: i32 = offset_of!(Proto, code) as i32;
pub(super) const OFF_P_CODEENTRY: i32 = offset_of!(Proto, codeentry) as i32;
pub(super) const OFF_P_NUMPARAMS: i32 = offset_of!(Proto, numparams) as i32;
pub(super) const OFF_P_ISVARARG: i32 = offset_of!(Proto, is_vararg) as i32;
pub(super) const OFF_P_EXECCOUNT: i32 = offset_of!(Proto, execcount) as i32;
pub(super) const OFF_BUF_LEN: i32 = offset_of!(Buffer, len) as i32;
pub(super) const BUF_DATA: i32 = Buffer::DATA_OFFSET as i32;

/// Buffer builtins handled inline: (size, read kind / write) per builtin id.
/// Read kinds as `Asm::load_ext`; 6 = f32, 7 = f64.
pub(super) fn buffer_op(bfid: u32) -> Option<(u8, bool, u8)> {
    use luau_common::bytecode::BuiltinFunction as B;
    let t = [
        (B::BufferReadI8, 1, false, 1),
        (B::BufferReadU8, 1, false, 0),
        (B::BufferWriteU8, 1, true, 0),
        (B::BufferReadI16, 2, false, 3),
        (B::BufferReadU16, 2, false, 2),
        (B::BufferWriteU16, 2, true, 0),
        (B::BufferReadI32, 4, false, 5),
        (B::BufferReadU32, 4, false, 4),
        (B::BufferWriteU32, 4, true, 0),
        (B::BufferReadF32, 4, false, 6),
        (B::BufferWriteF32, 4, true, 6),
        (B::BufferReadF64, 8, false, 7),
        (B::BufferWriteF64, 8, true, 7),
    ];
    t.iter().find(|e| e.0 as u32 == bfid).map(|e| (e.1, e.2, e.3))
}

pub(super) const OFF_GLOBAL: i32 = offset_of!(lua_State, global) as i32;
/// `global_State::ttname[LUA_TNIL]` (the interned "nil")
pub(super) const OFF_TTNAME_NIL: i32 = offset_of!(global_State, ttname) as i32;
pub(super) const OFF_CI: i32 = offset_of!(lua_State, ci) as i32;
pub(super) const OFF_CI_SAVEDPC: i32 = offset_of!(CallInfo, savedpc) as i32;
pub(super) const OFF_CI_P: i32 = offset_of!(CallInfo, p) as i32;
pub(super) const OFF_CI_FUNC: i32 = offset_of!(CallInfo, func) as i32;
pub(super) const OFF_UPREFS: i32 = (offset_of!(Closure, u) + offset_of!(LClosure, uprefs)) as i32;
pub(super) const OFF_UPVAL_V: i32 = offset_of!(UpVal, v) as i32;
pub(super) const OFF_UPVAL_MARKED: i32 = (offset_of!(UpVal, hdr) + offset_of!(GCheader, marked)) as i32;
pub(super) const OFF_P_K: i32 = offset_of!(Proto, k) as i32;
pub(super) const OFF_INTERRUPT: i32 = (offset_of!(global_State, cb) + offset_of!(lua_Callbacks, interrupt)) as i32;
pub(super) const OFF_TOTALBYTES: i32 = offset_of!(global_State, totalbytes) as i32;
pub(super) const OFF_GCTHRESHOLD: i32 = offset_of!(global_State, GCthreshold) as i32;
pub(super) const OFF_T_METATABLE: i32 = offset_of!(LuaTable, metatable) as i32;
pub(super) const OFF_T_ARRAY: i32 = offset_of!(LuaTable, array) as i32;
pub(super) const OFF_T_SIZEARRAY: i32 = offset_of!(LuaTable, sizearray) as i32;
pub(super) const OFF_T_READONLY: i32 = offset_of!(LuaTable, readonly) as i32;
pub(super) const OFF_T_MARKED: i32 = (offset_of!(LuaTable, hdr) + offset_of!(GCheader, marked)) as i32;
pub(super) const BLACK: u8 = 1 << crate::lgc::BLACKBIT;
pub(super) const OFF_T_NODE: i32 = offset_of!(LuaTable, node) as i32;
pub(super) const OFF_T_NODEMASK8: i32 = offset_of!(LuaTable, nodemask8) as i32;
pub(super) const OFF_T_SAFEENV: i32 = offset_of!(LuaTable, safeenv) as i32;
pub(super) const OFF_CL_ENV: i32 = offset_of!(Closure, env) as i32;
pub(super) const NODE_SHIFT: u8 = 5; // size_of::<LuaNode>() == 32
pub(super) const OFF_T_LSIZENODE: i32 = offset_of!(LuaTable, lsizenode) as i32;
pub(super) const OFF_T_TMCACHE: i32 = offset_of!(LuaTable, tmcache) as i32;
pub(super) const OFF_TS_HASH: i32 = offset_of!(TString, hash) as i32;
pub(super) const OFF_TS_DATA: i32 = offset_of!(TString, data) as i32;
pub(super) const OFF_TMNAME_INDEX: i32 = (offset_of!(global_State, tmname) + crate::ltm::TM_INDEX as usize * 8) as i32;
pub(super) const NODE_KEY: i32 = offset_of!(LuaNode, key) as i32;
pub(super) const NODE_KEYTT: i32 = (offset_of!(LuaNode, key) + offset_of!(TKey, ttnext)) as i32;

#[inline]
pub(super) fn op_of(i: u32) -> u8 {
    (i & 0xff) as u8
}
#[inline]
pub(super) fn a_of(i: u32) -> i32 {
    ((i >> 8) & 0xff) as i32
}
#[inline]
pub(super) fn b_of(i: u32) -> i32 {
    ((i >> 16) & 0xff) as i32
}
#[inline]
pub(super) fn c_of(i: u32) -> i32 {
    ((i >> 24) & 0xff) as i32
}
#[inline]
pub(super) fn d_of(i: u32) -> i32 {
    (i as i32) >> 16
}

/// Stack slot offsets.
#[inline]
pub(super) fn reg(r: i32) -> i32 {
    r * 16
}

/// The shared entry stub: `enter(L, target, base, k)` (System V argument
/// registers). Its prologue must match the unwind info in `mem.rs`.
pub fn emit_enter_stub() -> Vec<u8> {
    let mut a = Asm::new();
    a.push(R::Rbx);
    a.push(R::Rbp);
    a.push(R::R12);
    a.push(R::R13);
    a.push(R::R14);
    a.push(R::R15);
    a.sub_rsp8(8);
    debug_assert_eq!(a.pos(), super::mem::STUB_PROLOGUE as u32);
    a.mov_rr(R::R12, R::Rdi);
    a.mov_rr(R::R13, R::Rdx);
    a.mov_rr(R::R14, R::Rcx);
    load_closure(&mut a);
    a.jmp_r(R::Rsi);
    a.code
}

/// r15 = the running closure (`clvalue(L->ci->func)`).
pub(super) fn load_closure(a: &mut Asm) {
    a.load(R::R15, R::R12, OFF_CI);
    a.load(R::R15, R::R15, OFF_CI_FUNC);
    a.load(R::R15, R::R15, 0);
}

pub(super) fn emit_epilogue(a: &mut Asm) {
    a.add_rsp8(8);
    a.pop(R::R15);
    a.pop(R::R14);
    a.pop(R::R13);
    a.pop(R::R12);
    a.pop(R::Rbp);
    a.pop(R::Rbx);
}

/// Loads the handler-call arguments (L, pc, base, k) for `pc`.
pub(super) fn emit_args(a: &mut Asm, pc: usize) {
    a.mov_rr(R::Rdi, R::R12);
    a.mov_ri(R::Rsi, pc as u64);
    a.mov_rr(R::Rdx, R::R13);
    a.mov_rr(R::Rcx, R::R14);
}

/// Leaves native code: continues in the interpreter handler for `op` at `pc`.
pub(super) fn emit_exit(a: &mut Asm, pc: usize, op: u8) {
    emit_args(a, pc);
    emit_epilogue(a);
    a.mov_ri(R::Rax, handler_address(op) as u64);
    a.jmp_r(R::Rax);
}

/// Calls a frame helper (`native_call`/`native_return`) for instruction
/// `pc` and continues where it says.
/// CALL to a Lua function with native code, fixed arguments: builds the
/// frame inline (as `native_call`) and jumps to the callee; anything else
/// branches to `fallback` (the helper path) before changing any state.
pub(super) fn emit_inline_call(a: &mut Asm, insn: u32, npc: usize, fallback: Label, cfun: Option<Label>) {
    let ra = a_of(insn);
    let nparams = b_of(insn) - 1;
    let nresults = c_of(insn) - 1;
    a.load(R::Rax, R::R12, OFF_GLOBAL);
    a.cmp64_mi(R::Rax, OFF_INTERRUPT, 0);
    a.jcc(Cond::NE, fallback);
    a.cmp32_mi(R::R13, reg(ra) + TT, LUA_TFUNCTION);
    a.jcc(Cond::NE, fallback);
    a.load(R::Rax, R::R13, reg(ra)); // closure
    a.cmp8_mi(R::Rax, OFF_CL_ISC, 0);
    // a C function: rax = closure, interrupt and tag already checked
    a.jcc(Cond::NE, cfun.unwrap_or(fallback));
    a.load(R::Rdx, R::Rax, OFF_CL_P); // proto
    a.cmp64_mi(R::Rdx, OFF_P_EXECDATA, 0);
    a.jcc(Cond::E, fallback);
    a.load(R::Rcx, R::R12, OFF_CI);
    a.cmp_rm(R::Rcx, R::R12, OFF_END_CI);
    a.jcc(Cond::E, fallback);
    // r8 = argtop; stack room: stack_last - argtop > stacksize * 16
    a.mov_rr(R::R8, R::R13);
    a.add_ri(R::R8, reg(ra + 1 + nparams));
    a.load(R::R9, R::R12, OFF_STACK_LAST);
    a.sub_rr(R::R9, R::R8);
    a.movzx8(R::R10, R::Rax, OFF_CL_STACKSIZE);
    a.shl_ri(R::R10, 4);
    a.cmp_rr(R::R9, R::R10);
    a.jcc(Cond::LE, fallback);
    // commit: caller savedpc, new CallInfo
    a.mov_ri(R::R11, npc as u64);
    a.store(R::Rcx, OFF_CI_SAVEDPC, R::R11);
    a.add_ri(R::Rcx, SIZE_CI);
    a.store(R::R12, OFF_CI, R::Rcx);
    a.mov_rr(R::R9, R::R13);
    a.add_ri(R::R9, reg(ra));
    a.store(R::Rcx, OFF_CI_FUNC, R::R9);
    a.store(R::Rcx, OFF_CI_P, R::Rdx);
    a.add_ri(R::R9, 16); // new base
    a.store(R::Rcx, OFF_CI_BASE, R::R9);
    a.add_rr(R::R10, R::R8); // ci->top = argtop + stacksize
    a.store(R::Rcx, OFF_CI_TOP, R::R10);
    a.store64_i(R::Rcx, OFF_CI_SAVEDPC, 0);
    a.store32_i(R::Rcx, OFF_CI_FLAGS, 0);
    a.store32_i(R::Rcx, OFF_CI_NRESULTS, nresults as u32);
    a.store(R::R12, OFF_BASE, R::R9);
    a.add32_mi(R::Rdx, OFF_P_EXECCOUNT, 1);
    // nil the missing parameters
    a.movzx8(R::R11, R::Rdx, OFF_P_NUMPARAMS);
    a.shl_ri(R::R11, 4);
    a.add_rr(R::R11, R::R9);
    let fill = a.label();
    let filled = a.label();
    a.bind(fill);
    a.cmp_rr(R::R8, R::R11);
    a.jcc(Cond::AE, filled);
    a.store64_i(R::R8, TAGWORD, 0);
    a.add_ri(R::R8, 16);
    a.jmp(fill);
    a.bind(filled);
    // L->top = vararg ? first unfilled slot : ci->top
    let vararg = a.label();
    let topset = a.label();
    a.cmp8_mi(R::Rdx, OFF_P_ISVARARG, 0);
    a.jcc(Cond::NE, vararg);
    a.store(R::R12, OFF_TOP, R::R10);
    a.jmp(topset);
    a.bind(vararg);
    a.store(R::R12, OFF_TOP, R::R8);
    a.bind(topset);
    // switch to the callee and jump to its entry
    a.mov_rr(R::R13, R::R9);
    a.load(R::R14, R::Rdx, OFF_P_K);
    a.mov_rr(R::R15, R::Rax);
    a.load(R::Rcx, R::Rdx, OFF_P_CODEENTRY);
    a.sub_rm(R::Rcx, R::Rdx, OFF_P_CODE);
    a.shr_ri(R::Rcx, 2);
    a.load(R::Rdx, R::Rdx, OFF_P_EXECDATA);
    a.load(R::Rdx, R::Rdx, 0); // table_ptr
    a.jmp_table(R::Rdx, R::Rcx);
}

/// RETURN with a fixed number of values to a caller with native code: pops
/// the frame inline (as `native_return`); branches to `fallback` before
/// changing state when the inline path doesn't apply.
pub(super) fn emit_inline_return(a: &mut Asm, insn: u32, fallback: Label) {
    let ra = a_of(insn);
    let nvals = b_of(insn) - 1;
    a.load(R::Rax, R::R12, OFF_GLOBAL);
    a.cmp64_mi(R::Rax, OFF_INTERRUPT, 0);
    a.jcc(Cond::NE, fallback);
    a.load(R::Rcx, R::R12, OFF_CI);
    // leaving the VM, or a caller without native code: helper path
    a.test32_mi(R::Rcx, OFF_CI_FLAGS, LUA_CALLINFO_RETURN);
    a.jcc(Cond::NE, fallback);
    a.mov_rr(R::Rdx, R::Rcx);
    a.sub_ri(R::Rdx, SIZE_CI); // caller ci
    a.load(R::Rax, R::Rdx, OFF_CI_P);
    a.test_rr(R::Rax, R::Rax);
    a.jcc(Cond::E, fallback);
    a.load(R::Rbx, R::Rax, OFF_P_EXECDATA);
    a.test_rr(R::Rbx, R::Rbx);
    a.jcc(Cond::E, fallback);
    // copy results: res = ci->func, i = nresults
    a.load(R::R11, R::Rcx, OFF_CI_FUNC);
    a.load32(R::R10, R::Rcx, OFF_CI_NRESULTS);
    let mut q = 0;
    let done = a.label();
    while q < nvals {
        a.cmp32_ri(R::R10, 0);
        a.jcc(Cond::E, done);
        copy_tv(a, R::R11, 0, R::R13, reg(ra + q));
        a.add_ri(R::R11, 16);
        a.sub32_ri(R::R10, 1);
        q += 1;
    }
    // missing results become nil (no-op for MULTRET: i < 0)
    let fill = a.label();
    a.bind(fill);
    a.cmp32_ri(R::R10, 0);
    a.jcc(Cond::LE, done);
    a.store64_i(R::R11, TAGWORD, 0);
    a.add_ri(R::R11, 16);
    a.sub32_ri(R::R10, 1);
    a.jmp(fill);
    a.bind(done);
    // pop the frame
    a.store(R::R12, OFF_CI, R::Rdx);
    a.load(R::R13, R::Rdx, OFF_CI_BASE);
    a.store(R::R12, OFF_BASE, R::R13);
    let multret = a.label();
    let topset = a.label();
    a.cmp32_mi(R::Rcx, OFF_CI_NRESULTS, -1);
    a.jcc(Cond::E, multret);
    a.load(R::R10, R::Rdx, OFF_CI_TOP);
    a.store(R::R12, OFF_TOP, R::R10);
    a.jmp(topset);
    a.bind(multret);
    a.store(R::R12, OFF_TOP, R::R11);
    a.bind(topset);
    // continue the caller at its savedpc
    a.load(R::R14, R::Rax, OFF_P_K);
    a.load(R::R15, R::Rdx, OFF_CI_FUNC);
    a.load(R::R15, R::R15, 0);
    a.load(R::Rcx, R::Rdx, OFF_CI_SAVEDPC);
    a.sub_rm(R::Rcx, R::Rax, OFF_P_CODE);
    a.shr_ri(R::Rcx, 2);
    a.load(R::Rbx, R::Rbx, 0); // table_ptr
    a.jmp_table(R::Rbx, R::Rcx);
}

pub(super) fn emit_frame_op(a: &mut Asm, cx: &Ctx, pc: usize, op: u8, helper: usize) {
    emit_args(a, pc);
    a.mov_ri(R::Rax, helper as u64);
    a.call_r(R::Rax);
    let done = a.label();
    a.cmp_ri(R::Rax, NATIVE_EXIT as i32);
    a.jcc(Cond::NE, done);
    emit_exit(a, pc, op);
    a.bind(done);
    a.cmp_ri(R::Rax, NATIVE_LEAVE as i32);
    a.jcc(Cond::E, cx.ret);
    a.cmp_ri(R::Rax, NATIVE_INTERP as i32);
    a.jcc(Cond::E, cx.interp);
    // continue natively in the frame now on top
    a.load(R::R13, R::R12, OFF_BASE);
    a.load(R::Rcx, R::R12, OFF_CI);
    a.load(R::Rcx, R::Rcx, OFF_CI_P);
    a.load(R::R14, R::Rcx, OFF_P_K);
    load_closure(a);
    a.jmp_r(R::Rax);
}

/// CALL of a C function: calls `native_call_c` and continues at `next` in
/// this frame; branches to `slow` (before changing any state) unless R(A) is
/// a C closure, no interrupt is set and a CallInfo is free.
pub(super) fn emit_c_call(a: &mut Asm, cx: &Ctx, insn: u32, pc: usize, next: Label, slow: Label, checked: Option<Label>) {
    let ra = a_of(insn);
    a.load(R::Rax, R::R12, OFF_GLOBAL);
    a.cmp64_mi(R::Rax, OFF_INTERRUPT, 0);
    a.jcc(Cond::NE, slow);
    a.cmp32_mi(R::R13, reg(ra) + TT, LUA_TFUNCTION);
    a.jcc(Cond::NE, slow);
    a.load(R::Rax, R::R13, reg(ra));
    a.cmp8_mi(R::Rax, OFF_CL_ISC, 0);
    a.jcc(Cond::E, slow);
    // entered here from the inline Lua-call checks (rax = the C closure):
    // only the CallInfo check is left
    if let Some(l) = checked {
        a.bind(l);
    }
    a.load(R::Rcx, R::R12, OFF_CI);
    a.cmp_rm(R::Rcx, R::R12, OFF_END_CI);
    a.jcc(Cond::E, slow);
    let nparams = b_of(insn) - 1;
    let nres = c_of(insn) - 1;
    let isfb = (insn & 0xff) as u8 == O::CallFb as u8;
    if !isfb && nparams >= 0 && (0..=2).contains(&nres) && !super::trans::dbg_off(4194304) {
        // fixed shape: a helper specialized for it
        a.mov_rr(R::Rdi, R::R12);
        a.lea(R::Rsi, R::R13, reg(ra));
        a.lea(R::Rdx, R::R13, reg(ra + 1 + nparams));
        a.mov_ri(R::Rcx, (pc + 4) as u64);
        let f = match nres {
            0 => crate::lvmexecute::native_call_c_fixed::<0> as usize,
            1 => crate::lvmexecute::native_call_c_fixed::<1> as usize,
            _ => crate::lvmexecute::native_call_c_fixed::<2> as usize,
        };
        a.mov_ri(R::Rax, f as u64);
    } else {
        emit_args(a, pc);
        a.mov_ri(R::Rax, native_call_c as usize as u64);
    }
    a.call_r(R::Rax);
    a.cmp_ri(R::Rax, NATIVE_LEAVE as i32);
    a.jcc(Cond::E, cx.ret);
    // same frame: k and the closure are unchanged, the stack may have moved
    a.load(R::R13, R::R12, OFF_BASE);
    a.jmp(next);
}

/// Shared exit labels of a function's native code.
pub(super) struct Ctx {
    /// returns from native code (a step left the VM)
    pub ret: Label,
    /// rax = pc: continue at that instruction's native code
    pub dispatch: Label,
    /// continue interpreting the frame on top at its savedpc
    pub interp: Label,
    /// rax = pc: continue in the interpreter at that instruction
    pub exit_dyn: Label,
}

/// Branches to `slow` unless the interrupt hook is unset and no GC step is due.
pub(super) fn emit_loop_checks(a: &mut Asm, slow: Label) {
    a.load(R::Rax, R::R12, OFF_GLOBAL);
    a.cmp64_mi(R::Rax, OFF_INTERRUPT, 0);
    a.jcc(Cond::NE, slow);
    a.load(R::Rcx, R::Rax, OFF_TOTALBYTES);
    a.cmp_rm(R::Rcx, R::Rax, OFF_GCTHRESHOLD);
    a.jcc(Cond::AE, slow);
}

/// Copies a TValue between [src_base + src] and [dst_base + dst] (two
/// 8-byte moves: no wide load right after split stores).
pub(super) fn copy_tv(a: &mut Asm, dst_base: R, dst: i32, src_base: R, src: i32) {
    a.load(R::R8, src_base, src);
    a.load(R::R9, src_base, src + 8);
    a.store(dst_base, dst, R::R8);
    a.store(dst_base, dst + 8, R::R9);
}

/// L->top = &R(r)
pub(super) const INT_TAG: u64 = (LUA_TINTEGER as u64) << 32;

/// Inline kind of an `integer` library builtin: 0 add, 1 sub, 2 mul, 3 band,
/// 4 bor, 5 bxor, 6 lrotate, 7 rrotate, 8 lshift, 9 rshift (two operands);
/// 10 bnot, 11 neg, 12 bswap (one operand).
pub(super) fn int_builtin(bfid: u32) -> Option<u8> {
    use luau_common::bytecode::BuiltinFunction as B;
    let t = [
        (B::IntegerAdd, 0u8),
        (B::IntegerSub, 1),
        (B::IntegerMul, 2),
        (B::IntegerBand, 3),
        (B::IntegerBor, 4),
        (B::IntegerBxor, 5),
        (B::IntegerLrotate, 6),
        (B::IntegerRrotate, 7),
        (B::IntegerLshift, 8),
        (B::IntegerRshift, 9),
        (B::IntegerBnot, 10),
        (B::IntegerNeg, 11),
        (B::IntegerBswap, 12),
    ];
    t.iter().find(|e| e.0 as u32 == bfid).map(|e| e.1)
}

/// rax = rax <kind> rcx for the two-operand kinds of `int_builtin`
/// (clobbers rcx, rdx).
pub(super) fn emit_int_op(a: &mut Asm, kind: u8) {
    match kind {
        0 => a.alu64_rr(0x01, R::Rax, R::Rcx),
        1 => a.alu64_rr(0x29, R::Rax, R::Rcx),
        2 => a.imul64_rr(R::Rax, R::Rcx),
        3 => a.alu64_rr(0x21, R::Rax, R::Rcx),
        4 => a.alu64_rr(0x09, R::Rax, R::Rcx),
        5 => a.alu64_rr(0x31, R::Rax, R::Rcx),
        6 => a.shift64_cl(0, R::Rax), // the CPU takes the count mod 64
        7 => a.shift64_cl(1, R::Rax),
        _ => {
            // shifts: a count in -63..=63 (negative reverses), otherwise 0
            let (fwd, rev) = if kind == 8 { (4, 5) } else { (5, 4) };
            let zero = a.label();
            let neg = a.label();
            let done = a.label();
            a.mov_rr(R::Rdx, R::Rcx);
            a.add_ri(R::Rdx, 63);
            a.cmp_ri(R::Rdx, 126);
            a.jcc(Cond::A, zero);
            a.test_rr(R::Rcx, R::Rcx);
            a.jcc(Cond::L, neg);
            a.shift64_cl(fwd, R::Rax);
            a.jmp(done);
            a.bind(neg);
            a.unary64(3, R::Rcx);
            a.shift64_cl(rev, R::Rax);
            a.jmp(done);
            a.bind(zero);
            a.alu32_rr(0x31, R::Rax, R::Rax);
            a.bind(done);
        }
    }
}

pub(super) fn set_top(a: &mut Asm, r: i32) {
    a.mov_rr(R::Rax, R::R13);
    a.add_ri(R::Rax, reg(r));
    a.store(R::R12, OFF_TOP, R::Rax);
}

/// x = (x, y, z, 0) of the vector at [base + off]: an 8-byte and a 4-byte
/// load, matching how values are stored (a 16-byte load right after split
/// stores would stall on store forwarding).
pub(super) fn load_vec(a: &mut Asm, x: X, tmp: X, base: R, off: i32) {
    a.movq_load(x, base, off);
    a.movss_load(tmp, base, off + 8);
    a.movlhps(x, tmp);
}

/// rdx = main-position node of `hash` (rdx in: hash as u32) in table `t`
/// (clobbers rcx).
pub(super) fn main_node(a: &mut Asm, t: R) {
    a.movzx8(R::Rcx, t, OFF_T_LSIZENODE);
    a.mov_ri(R::R8, 1);
    a.shift32_cl(4, R::R8); // 1 << lsizenode
    a.sub32_ri(R::R8, 1);
    a.alu32_rr(0x21, R::Rdx, R::R8);
    a.shl_ri(R::Rdx, NODE_SHIFT);
    a.load(R::Rcx, t, OFF_T_NODE);
    a.add_rr(R::Rdx, R::Rcx);
}

/// Branches to `no` unless node rdx has string key `key` (register) with a
/// non-nil value.
pub(super) fn node_has_key(a: &mut Asm, key: R, no: Label) {
    a.load32(R::R8, R::Rdx, NODE_KEYTT);
    a.and32_ri(R::R8, 0xf);
    a.cmp32_ri(R::R8, LUA_TSTRING);
    a.jcc(Cond::NE, no);
    a.cmp_rm(key, R::Rdx, NODE_KEY);
    a.jcc(Cond::NE, no);
}

/// Type feedback per function (keyed by Proto address): for each instruction
/// that bailed out, the operand types seen (or `FB_GENERIC` / `FB_NOINLINE`),
/// and how often the function was recompiled.
static FEEDBACK: std::sync::Mutex<Option<std::collections::HashMap<usize, (Vec<(u32, u8, u8)>, u32)>>> = std::sync::Mutex::new(None);
/// `Proto::exectarget` bit: compiled with run-time type dispatch everywhere.
const GENERIC_BIT: usize = 1 << 31;
const MAX_RECOMPILES: u32 = 24;

/// Loops (function, head instruction) whose registers must not be carried:
/// their entry check failed (a speculated type did not hold).
/// (Counted: the first failures usually come from code compiled before type
/// feedback arrived; a loop is given up after `CARRY_TRIES`.)
static NOCARRY: std::sync::Mutex<Option<std::collections::HashMap<(usize, usize), u32>>> = std::sync::Mutex::new(None);
const CARRY_TRIES: u32 = 4;

/// Register types seen at a loop head when its carried-register check failed.
static CARRY_SEEN: std::sync::Mutex<Option<std::collections::HashMap<(usize, usize), Vec<u8>>>> = std::sync::Mutex::new(None);

/// The type register `r` had at loop `head` when a carry check failed there.
pub(super) fn carry_seen(p: *mut Proto, head: usize, r: usize) -> Option<u8> {
    CARRY_SEEN.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&(p as usize, head)).map(|v| v.get(r).copied().unwrap_or(0xff))))
}

pub(super) fn carry_disabled(p: *mut Proto, head: usize) -> bool {
    NOCARRY.lock().ok().and_then(|g| g.as_ref().and_then(|s| s.get(&(p as usize, head)).copied())).unwrap_or(0) >= CARRY_TRIES
}

/// An entry check of a loop with carried registers failed: recompile the
/// function without carrying for that loop (no type feedback recorded).
pub(super) unsafe extern "sysv64" fn on_carry_fail(p: *mut Proto, head: u64, L: *mut lua_State) {
    // what the registers really hold: the next plan skips any that differ
    let base = (*L).base;
    let n = ((*(*L).ci).top.offset_from(base).max(0) as usize).min(256);
    let seen: Vec<u8> = (0..n).map(|r| (*base.add(r)).tt as u8).collect();
    if let Ok(mut g) = CARRY_SEEN.lock() {
        g.get_or_insert_with(Default::default).insert((p as usize, head as usize), seen);
    }
    if let Ok(mut g) = NOCARRY.lock() {
        let n = g.get_or_insert_with(Default::default).entry((p as usize, head as usize)).or_insert(0);
        if *n >= CARRY_TRIES {
            return;
        }
        *n += 1;
    }
    let generic = (*p).exectarget & GENERIC_BIT != 0;
    if let Some(np) = build(p, generic, &bailed_of(p)) {
        (*np).prev = (*p).execdata as *mut NativeProto;
        (*p).execdata = np as *mut u8;
    }
}

fn is_table_access(op: u8) -> bool {
    [O::GetTable, O::GetTableN, O::GetTableKs, O::SetTable, O::SetTableN, O::SetTableKs].iter().any(|&o| o as u8 == op)
}

/// The other table accesses of `p` whose object register holds the same
/// value source (GETUPVAL / GETIMPORT / GETGLOBAL of the same thing, as last
/// written in straight-line order) as at instruction `at`. A heuristic: it
/// only picks the generic (always correct) path for those sites.
unsafe fn same_source_sites(p: *mut Proto, at: usize) -> Vec<usize> {
    // (kind, operand) of what each register was last loaded from
    let n = (*p).sizecode as usize;
    let mut src: [Option<(u8, u32)>; 256] = [None; 256];
    let mut sites: Vec<(usize, Option<(u8, u32)>)> = Vec::new();
    let mut want = None;
    let mut i = 0;
    while i < n {
        let insn = *(*p).code.add(i);
        let op = op_of(insn);
        let aux = if i + 1 < n { *(*p).code.add(i + 1) } else { 0 };
        if is_table_access(op) {
            let s = src[b_of(insn) as usize];
            if i == at {
                want = s;
            } else {
                sites.push((i, s));
            }
        }
        let a = a_of(insn) as usize;
        let o = |x: O| op == x as u8;
        if o(O::GetUpval) {
            src[a] = Some((0, b_of(insn) as u32));
        } else if o(O::GetImport) {
            src[a] = Some((1, aux));
        } else if o(O::GetGlobal) {
            src[a] = Some((2, aux));
        } else if o(O::Move) {
            src[a] = src[b_of(insn) as usize];
        } else if !(o(O::SetTable) || o(O::SetTableN) || o(O::SetTableKs) || o(O::SetUpval) || o(O::SetGlobal)) {
            // anything else may write R(A) (and calls more): forget it
            src[a] = None;
            if o(O::Call) || o(O::CallFb) || o(O::Namecall) || o(O::ForGLoop) || o(O::GetVarargs) || o(O::FastPCall) {
                src = [None; 256];
            }
        }
        i += luau_common::bytecode::get_op_length(O::from_u8(op)) as usize;
    }
    match want {
        Some(w) => sites.into_iter().filter(|&(_, s)| s == Some(w)).map(|(j, _)| j).collect(),
        None => Vec::new(),
    }
}

fn bailed_of(p: *mut Proto) -> Vec<(u32, u8, u8)> {
    let g = FEEDBACK.lock().unwrap();
    g.as_ref().and_then(|m| m.get(&(p as usize))).map(|e| e.0.clone()).unwrap_or_default()
}

/// Forgets a freed function's feedback.
pub fn forget_feedback(p: *mut Proto) {
    if let Ok(mut g) = FEEDBACK.lock() {
        if let Some(m) = g.as_mut() {
            m.remove(&(p as usize));
        }
    }
}

/// Called by a bail-out stub (before it leaves native code, with the stack
/// up to date): records what the instruction saw and recompiles the function
/// for it. The old code stays valid for frames still running it.
pub(super) unsafe extern "sysv64" fn on_bail(p: *mut Proto, idx: u64, L: *mut lua_State) {
    use super::trans::{FB_GENERIC, FB_NOINLINE};
    if (*p).exectarget & GENERIC_BIT != 0 {
        return;
    }
    let insn = *(*p).code.add(idx as usize);
    let op = op_of(insn);
    let base = (*L).base;
    let rtag = |r: i32| (*base.add(r as usize)).tt as u8;
    let ktag = |k: i32| (*(*p).k.add(k as usize)).tt as u8;
    let is = |o: O| op == o as u8;
    let seen = if is(O::Add) || is(O::Sub) || is(O::Mul) || is(O::Div) || is(O::Mod) || is(O::IDiv) || is(O::Pow) {
        (rtag(b_of(insn)), rtag(c_of(insn)))
    } else if is(O::AddK) || is(O::SubK) || is(O::MulK) || is(O::DivK) || is(O::ModK) || is(O::IDivK) || is(O::PowK) {
        (rtag(b_of(insn)), ktag(c_of(insn)))
    } else if is(O::SubRk) || is(O::DivRk) {
        (ktag(b_of(insn)), rtag(c_of(insn)))
    } else if is(O::Minus) {
        (rtag(b_of(insn)), rtag(b_of(insn)))
    } else if is(O::GetTableKs) {
        // not a table: the object's type; a table: the field's type was
        // not the one its next use wanted (stop loading it typed)
        let t = rtag(b_of(insn));
        if t == LUA_TTABLE as u8 {
            (t, FB_NOINLINE)
        } else {
            (t, 0)
        }
    } else {
        (FB_NOINLINE, FB_NOINLINE)
    };
    let (bailed, generic) = {
        let mut g = FEEDBACK.lock().unwrap();
        let e = g.get_or_insert_with(Default::default).entry(p as usize).or_default();
        match e.0.iter_mut().find(|x| x.0 == idx as u32) {
            Some(x) => {
                if x.1 == FB_GENERIC || x.1 == FB_NOINLINE {
                    return;
                }
                // a second set of types at the same instruction
                x.1 = FB_GENERIC;
                x.2 = FB_GENERIC;
            }
            None => e.0.push((idx as u32, seen.0, seen.1)),
        }
        // an object that isn't a table at one table access is usually not
        // one at the others reading the same upvalue / global / import:
        // give them the generic path now rather than one recompile each
        if is_table_access(op) && rtag(b_of(insn)) != LUA_TTABLE as u8 && !super::trans::dbg_off(33554432) {
            let objtag = rtag(b_of(insn));
            for j in same_source_sites(p, idx as usize) {
                if !e.0.iter().any(|x| x.0 == j as u32) {
                    let jop = op_of(*(*p).code.add(j));
                    let fb = if jop == O::GetTableKs as u8 { (objtag, 0) } else { (FB_NOINLINE, FB_NOINLINE) };
                    e.0.push((j as u32, fb.0, fb.1));
                }
            }
        }
        e.1 += 1;
        (e.0.clone(), e.1 > MAX_RECOMPILES)
    };
    if generic {
        (*p).exectarget |= GENERIC_BIT;
    }
    if let Some(np) = build(p, generic, &bailed) {
        (*np).prev = (*p).execdata as *mut NativeProto;
        (*p).execdata = np as *mut u8;
    }
}

pub unsafe fn compile_proto(p: *mut Proto) -> bool {
    if !(*p).execdata.is_null() || (*p).sizecode == 0 {
        return !(*p).execdata.is_null();
    }
    match build(p, (*p).exectarget & GENERIC_BIT != 0, &bailed_of(p)) {
        Some(np) => {
            (*p).execdata = np as *mut u8;
            true
        }
        None => false,
    }
}

/// Translates and places `p`'s code; returns its NativeProto.
unsafe fn build(p: *mut Proto, generic: bool, bailed: &[(u32, u8, u8)]) -> Option<*mut NativeProto> {
    let Some((mut code, mut offs)) = super::trans::translate(p, generic, bailed) else { return None };
    let table_fixup = offs.pop().flatten().unwrap() as usize;
    let n = (*p).sizecode as usize;
    let mut table = vec![0usize; n].into_boxed_slice();
    let table_addr = table.as_ptr() as u64;
    code[table_fixup..table_fixup + 8].copy_from_slice(&table_addr.to_le_bytes());
    let base = super::mem::with_code(|m| unsafe { m.place(&code) })?;
    for i in 0..n {
        if let Some(o) = offs[i] {
            table[i] = base + o as usize;
        }
    }
    let table_ptr = table.as_ptr();
    if let Some(path) = std::env::var_os("LUAU_CODEGEN_DUMP") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let name = if (*p).debugname.is_null() { "?".to_string() } else { String::from_utf8_lossy(crate::lobject::sbytes((*p).debugname)).into_owned() };
            let _ = writeln!(f, "proto {} {} generic={} bailed={} end={:#x}", name, (*p).linedefined, generic, bailed.len(), base + code.len());
            super::trans::LAST_OFFSETS.with(|c| {
                for (i, &o) in c.borrow().iter().enumerate() {
                    if o != u32::MAX {
                        let _ = writeln!(f, "{:#x} {} {}", base + o as usize, i, *(*p).code.add(i) & 0xff);
                    }
                }
            });
        }
    }
    // LUAU_CODEGEN_BIN=dir: raw machine code per function, for
    // `objdump -D -b binary -mi386:x86-64 --adjust-vma=<base>`
    if let Some(dir) = std::env::var_os("LUAU_CODEGEN_BIN") {
        let name = if (*p).debugname.is_null() { "anon".to_string() } else { String::from_utf8_lossy(crate::lobject::sbytes((*p).debugname)).into_owned() };
        let path = std::path::Path::new(&dir).join(format!("{}_{}_{:x}.bin", name, (*p).linedefined, base));
        let _ = std::fs::write(path, &code);
    }
    Some(Box::into_raw(Box::new(NativeProto { table_ptr, table, code: (base, code.len()), prev: std::ptr::null_mut() })))
}
