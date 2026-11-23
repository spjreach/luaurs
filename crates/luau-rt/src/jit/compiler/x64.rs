//! Trace -> x64 machine code (reusing the method JIT's assembler).
//!
//! A compiled trace is `extern "sysv64" fn(base: StkId, L: *mut lua_State)
//! -> u32`: it runs the loop until a guard fails and returns the index of
//! the snapshot it left through, after writing that snapshot's slots back
//! to the VM stack. The interpreter then resumes at the snapshot's pc.
//!
//! Register allocation is a single linear pass: every IR value gets one
//! machine register for its whole lifetime (xmm for numbers, a general
//! register for pointers / I32s / booleans / integers), freed after its last
//! use. Values used inside the loop body, PHIs and PHI back values live to
//! the end of the loop. Because homes never move, the exit code for a
//! snapshot is the same at every guard that uses it: one stub per snapshot.

use crate::codegen::asm::{Asm, Cond, Label, R, X};
use crate::jit::ir::instruction::{Cmp, Op, Ref, SnapId};
use crate::jit::ir::types::IrType;
use crate::jit::snapshots::Snapshot;
use crate::jit::trace::Trace;
use crate::lobject::*;
use std::mem::offset_of;

const TV: i32 = std::mem::size_of::<TValue>() as i32;
const OFF_TT: i32 = offset_of!(TValue, tt) as i32;
const OFF_TAGWORD: i32 = offset_of!(TValue, extra) as i32;
const OFF_T_SIZEARRAY: i32 = offset_of!(LuaTable, sizearray) as i32;
const OFF_T_ARRAY: i32 = offset_of!(LuaTable, array) as i32;
const OFF_T_METATABLE: i32 = offset_of!(LuaTable, metatable) as i32;
const OFF_T_READONLY: i32 = offset_of!(LuaTable, readonly) as i32;
const OFF_GLOBAL: i32 = offset_of!(lua_State, global) as i32;
const OFF_INTERRUPT: i32 = (offset_of!(global_State, cb) + offset_of!(lua_Callbacks, interrupt)) as i32;
const OFF_CI: i32 = offset_of!(lua_State, ci) as i32;
const OFF_CI_FUNC: i32 = offset_of!(CallInfo, func) as i32;
const OFF_STACK_LAST: i32 = offset_of!(lua_State, stack_last) as i32;
const OFF_UV_V: i32 = offset_of!(UpVal, v) as i32;
const OFF_T_LSIZENODE: i32 = offset_of!(LuaTable, lsizenode) as i32;
const OFF_T_NODE: i32 = offset_of!(LuaTable, node) as i32;
const NODE_SIZE: i32 = std::mem::size_of::<LuaNode>() as i32;
const OFF_NODE_KEY: i32 = (offset_of!(LuaNode, key) + offset_of!(TKey, value)) as i32;
const OFF_NODE_KEYTT: i32 = (offset_of!(LuaNode, key) + offset_of!(TKey, ttnext)) as i32;
const OFF_MARKED: i32 = offset_of!(GCheader, marked) as i32;
const OFF_TS_LEN: i32 = offset_of!(TString, len) as i32;
const OFF_TOTALBYTES: i32 = offset_of!(global_State, totalbytes) as i32;
const OFF_GCTHRESHOLD: i32 = offset_of!(global_State, GCthreshold) as i32;
const OFF_T_META: i32 = offset_of!(LuaTable, metatable) as i32;
const OFF_CL_CFUNC: i32 = (CLOSURE_HEADER + offset_of!(CClosure, f)) as i32;
const OFF_CL_ENV: i32 = offset_of!(Closure, env) as i32;
const OFF_CL_P: i32 = (CLOSURE_HEADER + offset_of!(LClosure, p)) as i32;
const OFF_CL_UPREFS: i32 = (CLOSURE_HEADER + offset_of!(LClosure, uprefs)) as i32;

/// r13 = base, r12 = L (as in the method JIT); rax, rcx, rdx and xmm0/1 are
/// scratch.
const GPRS: [R; 8] = [R::Rbx, R::Rbp, R::R14, R::R15, R::Rsi, R::Rdi, R::R8, R::R9];
const XMMS: [u8; 12] = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
/// registers spilled operands are reloaded into (and spilled results are
/// computed in) for one instruction
const TEMP_G: [R; 2] = [R::R10, R::R11];
const TEMP_X: [X; 2] = [X(14), X(15)];
/// spill slots (8 bytes each) every trace's native frame has, below the
/// saved registers; side traces run in their root's frame, so it's fixed
const SPILL_SLOTS: usize = 64;
const SPILL_BYTES: i32 = SPILL_SLOTS as i32 * 8;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Home {
    None,
    G(R),
    F(X),
    /// spill slot [rsp + 8 * n] (an xmm or general value, by its type)
    S(u8),
}

fn spill_off(n: u8) -> i32 {
    n as i32 * 8
}

fn needs_reg(ty: IrType) -> Option<bool> {
    // Some(true): xmm; Some(false): gpr; None: no value to hold
    match ty {
        IrType::Number | IrType::Vector => Some(true),
        IrType::Nil | IrType::Any => None,
        _ => Some(false),
    }
}

pub struct Code {
    pub bytes: Vec<u8>,
    /// offset of the root trace's re-entry point
    pub reentry: usize,
}

/// Compiles `t`. Fails (cleanly) on anything the backend doesn't handle yet.
pub unsafe fn compile(t: &Trace, link_to: Option<usize>) -> Result<Code, String> {
    let n = t.insts.len();
    let side = link_to.is_some();
    // (a side trace is linear: no loop)
    let loop_at = match t.loop_ref {
        Some(r) => r.index(),
        None if side => usize::MAX,
        None => return Err("trace has no loop".into()),
    };
    const END: usize = usize::MAX;

    // ---- liveness -----------------------------------------------------
    let mut last = vec![0usize; n];
    let mut used = vec![false; n];
    let mut touch = |v: Ref, at: usize, last: &mut Vec<usize>, used: &mut Vec<bool>| {
        let v = v.index();
        used[v] = true;
        let at = if v < loop_at && at > loop_at { END } else { at };
        if last[v] != END && at > last[v] {
            last[v] = at;
        }
    };
    // the loop's interrupt exit resumes at the header with every slot the
    // iteration wrote. The recorder appends that snapshot last; using the
    // last snapshot (not the last *guard*) keeps the restore complete.
    let back_snap = SnapId(t.snapshots.len().checked_sub(1).ok_or("trace has no snapshot")? as u32);
    for (i, inst) in t.insts.iter().enumerate() {
        match inst.op {
            Op::Phi { pre, back } => {
                touch(pre, loop_at, &mut last, &mut used);
                touch(back, END, &mut last, &mut used);
                last[i] = END;
                used[i] = true;
            }
            _ => {
                for o in inst.operands() {
                    touch(o, i, &mut last, &mut used);
                }
            }
        }
        if let Some(s) = inst.snapshot() {
            for e in &t.snapshots[s.0 as usize].entries {
                touch(e.value, i, &mut last, &mut used);
            }
        }
    }
    for e in &t.snapshots[back_snap.0 as usize].entries {
        touch(e.value, END, &mut last, &mut used);
    }

    // ---- allocation -----------------------------------------------------
    let mut home = vec![Home::None; n];
    // (LUAU_TRACE_REGS=n: only n registers of each kind, to test spilling)
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let limit = *LIMIT.get_or_init(|| std::env::var("LUAU_TRACE_REGS").ok().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX));
    let mut free_g: Vec<R> = GPRS.iter().take(limit).rev().copied().collect();
    let mut free_x: Vec<X> = XMMS.iter().take(limit).rev().map(|&x| X(x)).collect();
    // spill slots: occupied now, and the last instruction a past occupant
    // used it at (a value spilled after the fact holds its slot from its
    // definition on, so only a slot unused since then will do)
    let mut slot_busy = [false; SPILL_SLOTS];
    let mut slot_until: [Option<usize>; SPILL_SLOTS] = [None; SPILL_SLOTS];
    // values holding a register now (to pick one to spill)
    let mut active: Vec<usize> = Vec::new();
    let mut dying: Vec<Vec<usize>> = vec![Vec::new(); n + 1];
    let end_of = |v: usize, last: &[usize], used: &[bool]| if used[v] { last[v] } else { v };
    for (i, inst) in t.insts.iter().enumerate() {
        // values whose last use was the previous instruction are free now
        for &v in &dying[i] {
            match home[v] {
                Home::G(r) => free_g.push(r),
                Home::F(x) => free_x.push(x),
                Home::S(s) => {
                    slot_busy[s as usize] = false;
                    slot_until[s as usize] = Some(i);
                }
                Home::None => {}
            }
            active.retain(|&w| w != v);
        }
        if matches!(inst.op, Op::Nop | Op::Loop) {
            continue;
        }
        let defines = !matches!(
            inst.op,
            Op::Guard { .. } | Op::GuardTruthy { .. } | Op::GuardSame { .. } | Op::GuardDiff { .. } | Op::GuardPlainTable { .. } | Op::XStore { .. } | Op::GuardFlag { .. } | Op::GuardProto { .. } | Op::GuardUvSlot { .. } | Op::Barrier { .. } | Op::HMiss { .. } | Op::GuardCFunc { .. } | Op::TSet { .. }
        ) && !matches!(inst.op, Op::Builtin { nres: 0, .. });
        if !defines {
            continue;
        }
        if inst.is_const() && !used[i] {
            continue;
        }
        let Some(fp) = needs_reg(inst.ty) else { continue };
        let end = end_of(i, &last, &used);
        let reg = if fp { free_x.pop().map(Home::F) } else { free_g.pop().map(Home::G) };
        home[i] = match reg {
            Some(h) => {
                active.push(i);
                h
            }
            None => {
                // out of registers: the value (this one or one holding a
                // register of the same kind) needed furthest ahead lives in
                // a spill slot for its whole life
                let same = |w: &usize| matches!((home[*w], fp), (Home::F(_), true) | (Home::G(_), false));
                let victim = active.iter().copied().filter(same).max_by_key(|&w| end_of(w, &last, &used));
                let mut take_slot = |from: usize| -> Result<Home, String> {
                    let k = (0..SPILL_SLOTS).find(|&k| !slot_busy[k] && slot_until[k].is_none_or(|u| u < from)).ok_or("out of spill slots")?;
                    slot_busy[k] = true;
                    Ok(Home::S(k as u8))
                };
                match victim {
                    Some(w) if end_of(w, &last, &used) > end => {
                        let h = home[w];
                        home[w] = take_slot(w)?;
                        active.retain(|&x| x != w);
                        active.push(i);
                        h
                    }
                    _ => take_slot(i)?,
                }
            }
        };
        if end != END {
            dying[(end + 1).min(n)].push(i);
        }
    }

    if std::env::var_os("LUAU_TRACE_HOMES").is_some() {
        eprintln!("-- homes ({} insts)", n);
        for (i, h) in home.iter().enumerate() {
            if used[i] || *h != Home::None {
                eprintln!("{:04} {:?} used={} last={}", i, h, used[i], if last[i] == END { "END".to_string() } else { last[i].to_string() });
            }
        }
    }
    // values in registers still needed after instruction i (saved around a
    // call out of the trace)
    let live_across = |i: usize| -> Vec<Home> {
        (0..i).filter(|&j| matches!(home[j], Home::G(_) | Home::F(_)) && used[j] && (last[j] == END || last[j] > i)).map(|j| home[j]).collect()
    };
    // a trace that allocates checks for GC work at its back edge (and exits
    // for the collector to run: nothing collects while a trace runs)
    let allocates = t.insts.iter().any(|x| matches!(x.op, Op::NewTable { .. } | Op::DupTable { .. } | Op::Builtin { .. } | Op::TSet { .. }));

    // ---- emission -------------------------------------------------------
    let mut a = Asm::new();
    let exit = a.label();
    let loop_top = a.label();
    let stubs: Vec<Label> = (0..t.snapshots.len() + 1).map(|_| a.label()).collect();
    let interrupt_stub = stubs[t.snapshots.len()];
    let used_g: Vec<R> = [R::Rbx, R::Rbp, R::R12, R::R13, R::R14, R::R15].to_vec();
    // a side trace runs inside its root's native frame: entered from an
    // exit stub with everything set up, it has no prologue
    if !side {
        for &r in &used_g {
            a.push(r);
        }
        a.mov_rr(R::R13, R::Rdi);
        a.mov_rr(R::R12, R::Rsi);
        a.sub_ri(R::Rsp, SPILL_BYTES);
    }
    let reentry = a.pos() as usize;
    // entry checks (exit through the entry snapshot: nothing written yet):
    // the recorded closure, and stack room for inlined frames
    if t.closure != 0 {
        // (L->ci->func: a vararg frame's base isn't right above it)
        a.load(R::Rax, R::R12, OFF_CI);
        a.load(R::Rax, R::Rax, OFF_CI_FUNC);
        a.load(R::Rax, R::Rax, 0);
        a.mov_ri64(R::Rcx, t.closure as u64);
        a.cmp_rr(R::Rax, R::Rcx);
        a.jcc(Cond::NE, stubs[0]);
    }
    if t.stack_need > 0 {
        a.load(R::Rax, R::R12, OFF_STACK_LAST);
        a.sub_rr(R::Rax, R::R13);
        a.mov_ri(R::Rcx, (t.stack_need as u64) * TV as u64);
        a.cmp_rr(R::Rax, R::Rcx);
        a.jcc(Cond::L, stubs[0]);
    }

    for (i, inst) in t.insts.iter().enumerate() {
        // spilled operands go to the temp registers for this instruction
        // (stores and builtin arguments take values from anywhere)
        let mut over: Vec<(usize, Home)> = Vec::new();
        let (mut tg, mut tx) = (0usize, 0usize);
        if !matches!(inst.op, Op::Phi { .. } | Op::Loop | Op::Builtin { .. }) {
            let mut ops = inst.operands();
            if let Op::XStore { v, .. } = inst.op {
                ops.retain(|&o| o != v);
            }
            if let Op::TGet { k, .. } | Op::TSet { k, .. } = inst.op {
                ops.retain(|&o| o != k);
            }
            if let Op::TSet { v, .. } = inst.op {
                ops.retain(|&o| o != v);
            }
            for o in ops {
                if over.iter().any(|&(v, _)| v == o.index()) {
                    continue;
                }
                if let Home::S(s) = home[o.index()] {
                    if needs_reg(t.ty(o)) == Some(true) {
                        let x = *TEMP_X.get(tx).ok_or("too many spilled operands")?;
                        tx += 1;
                        a.movsd_load(x, R::Rsp, spill_off(s));
                        over.push((o.index(), Home::F(x)));
                    } else {
                        let r = *TEMP_G.get(tg).ok_or("too many spilled operands")?;
                        tg += 1;
                        a.load(r, R::Rsp, spill_off(s));
                        over.push((o.index(), Home::G(r)));
                    }
                }
            }
        }
        // a spilled result is computed in a temp, then stored
        let (dst, spill_to) = match home[i] {
            Home::S(s) if !matches!(inst.op, Op::Phi { .. }) => {
                if needs_reg(inst.ty) == Some(true) {
                    (Home::F(TEMP_X[tx.min(1)]), Some(s))
                } else {
                    (Home::G(TEMP_G[tg.min(1)]), Some(s))
                }
            }
            h => (h, None),
        };
        let hm = |r: Ref| over.iter().find(|&&(v, _)| v == r.index()).map(|&(_, h)| h).unwrap_or(home[r.index()]);
        let g = |r: Ref| match hm(r) {
            Home::G(r) => Ok(r),
            h => Err(format!("value {} is not in a general register ({:?})", r.0, h)),
        };
        let f = |r: Ref| match hm(r) {
            Home::F(x) => Ok(x),
            h => Err(format!("value {} is not in an xmm register ({:?})", r.0, h)),
        };
        let stub = |s: crate::jit::ir::instruction::SnapId| stubs[s.0 as usize];
        match &inst.op {
            Op::Nop => {}
            Op::KNil => {}
            Op::KBool(b) => {
                if let Home::G(r) = dst {
                    a.mov_ri(r, *b as u64);
                }
            }
            Op::KNum(v) => {
                if let Home::F(x) = dst {
                    a.mov_ri64(R::Rax, v.to_bits());
                    a.movq_xr(x, R::Rax);
                }
            }
            Op::KInt(v) => {
                if let Home::G(r) = dst {
                    a.mov_ri64(r, *v as u64);
                }
            }
            Op::KPtr(p) => {
                if let Home::G(r) = dst {
                    a.mov_ri64(r, *p as u64);
                }
            }
            Op::KI32(v) => {
                if let Home::G(r) = dst {
                    a.mov_ri(r, *v as u32 as u64);
                }
            }
            Op::SLoad { slot, guarded, snap } => {
                let disp = *slot as i32 * TV;
                if *guarded {
                    let tag = inst.ty.tag().ok_or("SLoad of a non-Lua type")?;
                    a.cmp32_mi(R::R13, disp + OFF_TT, tag);
                    a.jcc(Cond::NE, stub(*snap));
                }
                load_value(&mut a, dst, inst.ty, R::R13, disp)?;
            }
            Op::XLoad { p, snap } => {
                let p = g(*p)?;
                let tag = inst.ty.tag().ok_or("XLoad of a non-Lua type")?;
                a.cmp32_mi(p, OFF_TT, tag);
                a.jcc(Cond::NE, stub(*snap));
                load_value(&mut a, dst, inst.ty, p, 0)?;
            }
            Op::XStore { p, v } => {
                let p = g(*p)?;
                store_value(&mut a, &home, t, *v, p, 0)?;
            }
            Op::Guard { cmp, a: x, b: y, snap } => {
                let s = stub(*snap);
                let (tx, ty) = (t.ty(*x), t.ty(*y));
                if tx == IrType::Number && ty == IrType::Number {
                    let (x, y) = (f(*x)?, f(*y)?);
                    match cmp {
                        // exit when the condition doesn't hold (NaN: false)
                        Cmp::Lt => {
                            a.ucomisd(y, x);
                            a.jcc(Cond::BE, s);
                        }
                        Cmp::Le => {
                            a.ucomisd(y, x);
                            a.jcc(Cond::B, s);
                        }
                        Cmp::Nlt => {
                            a.ucomisd(y, x);
                            a.jcc(Cond::A, s);
                        }
                        Cmp::Nle => {
                            a.ucomisd(y, x);
                            a.jcc(Cond::AE, s);
                        }
                        Cmp::Eq => {
                            a.ucomisd(x, y);
                            a.jcc(Cond::NE, s);
                            a.jcc(Cond::P, s);
                        }
                        Cmp::Ne => {
                            let ok = a.label();
                            a.ucomisd(x, y);
                            a.jcc(Cond::P, ok);
                            a.jcc(Cond::E, s);
                            a.bind(ok);
                        }
                    }
                } else if (tx == IrType::Boolean && ty == IrType::Boolean) && matches!(cmp, Cmp::Eq | Cmp::Ne) {
                    a.cmp32_rr(g(*x)?, g(*y)?);
                    a.jcc(if matches!(cmp, Cmp::Eq) { Cond::NE } else { Cond::E }, s);
                } else if tx == IrType::I32 && ty == IrType::I32 {
                    a.cmp32_rr(g(*x)?, g(*y)?);
                    // exit when the condition fails (signed compare)
                    let exit_if = match cmp {
                        Cmp::Lt => Cond::GE,
                        Cmp::Le => Cond::G,
                        Cmp::Nlt => Cond::L,
                        Cmp::Nle => Cond::LE,
                        Cmp::Eq => Cond::NE,
                        Cmp::Ne => Cond::E,
                    };
                    a.jcc(exit_if, s);
                } else {
                    return Err(format!("guard on {:?} / {:?}", tx, ty));
                }
            }
            Op::GuardTruthy { a: v, want, snap } => {
                let s = stub(*snap);
                match t.ty(*v) {
                    IrType::Nil => {
                        if *want {
                            a.jmp(s);
                        }
                    }
                    IrType::Boolean => {
                        let r = g(*v)?;
                        a.test_rr(r, r);
                        a.jcc(if *want { Cond::E } else { Cond::NE }, s);
                    }
                    other => return Err(format!("truthiness guard on {:?}", other)),
                }
            }
            Op::GuardSame { a: v, k, snap } => {
                // Nil has no register; both nil is always equal and a
                // mixed pair can never pass the guard.
                let (vt, kt) = (t.ty(*v), t.ty(*k));
                if vt == IrType::Nil && kt == IrType::Nil {
                    // always equal
                } else if vt == IrType::Nil || kt == IrType::Nil {
                    a.jmp(stub(*snap));
                } else {
                    a.cmp_rr(g(*v)?, g(*k)?);
                    a.jcc(Cond::NE, stub(*snap));
                }
            }
            Op::GuardDiff { a: v, k, snap } => {
                let (vt, kt) = (t.ty(*v), t.ty(*k));
                if vt == IrType::Nil && kt == IrType::Nil {
                    a.jmp(stub(*snap));
                } else if vt == IrType::Nil || kt == IrType::Nil {
                    // never the same
                } else {
                    a.cmp_rr(g(*v)?, g(*k)?);
                    a.jcc(Cond::E, stub(*snap));
                }
            }
            Op::StrCmp { a: x, b: y } => {
                let live = live_across(i);
                call_out(&mut a, &live, helpers::strcmp as usize, &[Arg::G(g(*x)?), Arg::G(g(*y)?)]);
                if let Home::G(d) = dst {
                    a.mov_rr32(d, R::Rax);
                }
            }
            Op::GuardPlainTable { t: tb, store, snap } => {
                let r = g(*tb)?;
                a.cmp64_mi(r, OFF_T_METATABLE, 0);
                a.jcc(Cond::NE, stub(*snap));
                if *store {
                    a.cmp8_mi(r, OFF_T_READONLY, 0);
                    a.jcc(Cond::NE, stub(*snap));
                }
            }
            Op::Add(x, y) | Op::Sub(x, y) | Op::Mul(x, y) | Op::Div(x, y) => {
                let opc = match inst.op {
                    Op::Add(..) => 0x58,
                    Op::Sub(..) => 0x5c,
                    Op::Mul(..) => 0x59,
                    _ => 0x5e,
                };
                if let Home::F(d) = dst {
                    binop(&mut a, opc, d, f(*x)?, f(*y)?);
                }
            }
            Op::Mod(x, y) | Op::IDiv(x, y) => {
                if let Home::F(d) = dst {
                    let (x, y) = (f(*x)?, f(*y)?);
                    // q = floor(x / y)
                    a.movapd(X(0), x);
                    a.sd_rr(0x5e, X(0), y);
                    a.roundsd(X(0), X(0), 1);
                    if matches!(inst.op, Op::Mod(..)) {
                        // x - q * y
                        a.sd_rr(0x59, X(0), y);
                        a.movapd(X(1), x);
                        a.sd_rr(0x5c, X(1), X(0));
                        a.movapd(d, X(1));
                    } else {
                        a.movapd(d, X(0));
                    }
                }
            }
            Op::Neg(x) => {
                if let Home::F(d) = dst {
                    a.mov_ri64(R::Rax, 0x8000_0000_0000_0000);
                    a.movq_xr(X(0), R::Rax);
                    a.movapd(X(1), f(*x)?);
                    a.xorpd(X(1), X(0));
                    a.movapd(d, X(1));
                }
            }
            Op::Pow(..) => return Err("pow not supported yet".into()),
            Op::Math1(bfid, x) => {
                use luau_common::bytecode::BuiltinFunction as B;
                if let Home::F(d) = dst {
                    let x = f(*x)?;
                    let b = *bfid;
                    if b == B::MathSqrt as u8 {
                        a.sd_rr(0x51, d, x);
                    } else if b == B::MathFloor as u8 {
                        a.roundsd(d, x, 1);
                    } else if b == B::MathCeil as u8 {
                        a.roundsd(d, x, 2);
                    } else if b == B::MathAbs as u8 {
                        a.mov_ri64(R::Rax, 0x7fff_ffff_ffff_ffff);
                        a.movq_xr(X(0), R::Rax);
                        a.movapd(X(1), x);
                        a.andpd(X(1), X(0));
                        a.movapd(d, X(1));
                    } else {
                        return Err(format!("math builtin {}", b));
                    }
                }
            }
            Op::HRef { t: tb, k, idx, lsize, store, snap } => {
                let s = stub(*snap);
                let tb = g(*tb)?;
                a.cmp8_mi(tb, OFF_T_LSIZENODE, *lsize);
                a.jcc(Cond::NE, s);
                if *store {
                    a.cmp8_mi(tb, OFF_T_READONLY, 0);
                    a.jcc(Cond::NE, s);
                }
                a.load(R::Rcx, tb, OFF_T_NODE);
                let off = *idx as i32 * NODE_SIZE;
                // key: a string, and this one
                a.load32(R::Rax, R::Rcx, off + OFF_NODE_KEYTT);
                a.and32_ri(R::Rax, 0xf);
                a.cmp32_ri(R::Rax, LUA_TSTRING as i32);
                a.jcc(Cond::NE, s);
                a.load(R::Rax, R::Rcx, off + OFF_NODE_KEY);
                a.cmp_rr(R::Rax, g(*k)?);
                a.jcc(Cond::NE, s);
                if *store {
                    a.cmp32_mi(R::Rcx, off + OFF_TT, LUA_TNIL as i32);
                    a.jcc(Cond::E, s);
                }
                if let Home::G(d) = dst {
                    a.lea(d, R::Rcx, off);
                }
            }
            Op::Barrier { t: tb, v } => {
                // only a black table can need it (the helper checks the rest)
                let skip = a.label();
                let tb = g(*tb)?;
                a.test8_mi(tb, OFF_MARKED, 1 << crate::lgc::BLACKBIT);
                a.jcc(Cond::E, skip);
                let live = live_across(i);
                call_out(&mut a, &live, helpers::barrier as usize, &[Arg::L, Arg::G(tb), Arg::G(g(*v)?)]);
                a.bind(skip);
            }
            Op::TLen { t: tb } => {
                let live = live_across(i);
                call_out(&mut a, &live, helpers::len as usize, &[Arg::G(g(*tb)?)]);
                if let Home::F(d) = dst {
                    a.cvtsi2sd64(d, R::Rax);
                }
            }
            Op::SLen { s } => {
                if let Home::F(d) = dst {
                    a.load32(R::Rax, g(*s)?, OFF_TS_LEN);
                    a.cvtsi2sd64(d, R::Rax);
                }
            }
            Op::NewTable { narray, nhash } => {
                let live = live_across(i);
                call_out(&mut a, &live, helpers::new_table as usize, &[Arg::L, Arg::Imm(*narray as u64), Arg::Imm(*nhash as u64)]);
                if let Home::G(d) = dst {
                    a.mov_rr(d, R::Rax);
                }
            }
            Op::DupTable { tpl } => {
                let live = live_across(i);
                call_out(&mut a, &live, helpers::dup_table as usize, &[Arg::L, Arg::G(g(*tpl)?)]);
                if let Home::G(d) = dst {
                    a.mov_rr(d, R::Rax);
                }
            }
            Op::Builtin { bfid, args, nargs, nres, buf, snap } => {
                let s = stub(*snap);
                a.mov_ri64(R::Rdx, *buf as u64);
                for k in 0..*nargs as usize {
                    store_value(&mut a, &home, t, args[k], R::Rdx, (1 + k as i32) * TV)?;
                }
                let live = live_across(i);
                call_out(
                    &mut a,
                    &live,
                    helpers::fastcall as usize,
                    &[Arg::L, Arg::Imm(*bfid as u64), Arg::Imm(*nargs as u64), Arg::Imm(if *nres == crate::jit::recorder::MULTRET1 { u64::MAX } else { *nres as u64 }), Arg::Imm(*buf as u64)],
                );
                // declined: the interpreter does it the slow way (taking
                // every result: exactly one, as recorded)
                if *nres == crate::jit::recorder::MULTRET1 {
                    a.cmp_ri(R::Rax, 1);
                    a.jcc(Cond::NE, s);
                } else {
                    a.test_rr(R::Rax, R::Rax);
                    a.jcc(Cond::L, s);
                }
                if *nres != 0 {
                    let tag = inst.ty.tag().ok_or("builtin result of a non-Lua type")?;
                    a.mov_ri64(R::Rcx, *buf as u64);
                    a.cmp32_mi(R::Rcx, OFF_TT, tag);
                    a.jcc(Cond::NE, s);
                    load_value(&mut a, dst, inst.ty, R::Rcx, 0)?;
                }
            }
            Op::TGet { t: tb, k, buf } => {
                a.mov_ri64(R::Rdx, *buf as u64);
                store_value(&mut a, &home, t, *k, R::Rdx, 0)?;
                let live = live_across(i);
                call_out(&mut a, &live, helpers::tget as usize, &[Arg::G(g(*tb)?), Arg::Imm(*buf as u64)]);
                if let Home::G(d) = dst {
                    a.mov_rr(d, R::Rax);
                }
            }
            Op::TSet { t: tb, k, v, buf, snap } => {
                a.mov_ri64(R::Rdx, *buf as u64);
                store_value(&mut a, &home, t, *k, R::Rdx, 0)?;
                a.mov_ri64(R::Rdx, *buf as u64);
                store_value(&mut a, &home, t, *v, R::Rdx, TV)?;
                let live = live_across(i);
                call_out(&mut a, &live, helpers::tset as usize, &[Arg::L, Arg::G(g(*tb)?), Arg::Imm(*buf as u64)]);
                a.test_rr(R::Rax, R::Rax);
                a.jcc(Cond::E, stub(*snap));
            }
            Op::TMeta { t: tb, snap } => {
                let tb = g(*tb)?;
                a.load(R::Rax, tb, OFF_T_META);
                a.test_rr(R::Rax, R::Rax);
                a.jcc(Cond::E, stub(*snap));
                if let Home::G(d) = dst {
                    a.mov_rr(d, R::Rax);
                }
            }
            Op::HMiss { t: tb, k, snap } => {
                let live = live_across(i);
                call_out(&mut a, &live, helpers::getstr as usize, &[Arg::G(g(*tb)?), Arg::G(g(*k)?)]);
                a.cmp32_mi(R::Rax, OFF_TT, LUA_TNIL as i32);
                a.jcc(Cond::NE, stub(*snap));
            }
            Op::GuardCFunc { f: fv, fp, snap } => {
                // (a Lua closure has its proto there: never C code)
                a.load(R::Rax, g(*fv)?, OFF_CL_CFUNC);
                a.mov_ri64(R::Rcx, *fp as u64);
                a.cmp_rr(R::Rax, R::Rcx);
                a.jcc(Cond::NE, stub(*snap));
            }
            Op::HSlot { t: tb, idx } => {
                if let Home::G(d) = dst {
                    let tb = g(*tb)?;
                    a.load(R::Rax, tb, OFF_T_NODE);
                    a.lea(d, R::Rax, *idx as i32 * NODE_SIZE);
                }
            }
            Op::FrameFunc => {
                if let Home::G(d) = dst {
                    a.load(d, R::R12, OFF_CI);
                    a.load(d, d, OFF_CI_FUNC);
                    a.load(d, d, 0);
                }
            }
            Op::ClEnv { cl } => {
                if let Home::G(d) = dst {
                    a.load(d, g(*cl)?, OFF_CL_ENV);
                }
            }
            Op::GuardProto { f: fv, p, snap } => {
                // (a C closure has its function pointer there: never a proto)
                a.load(R::Rax, g(*fv)?, OFF_CL_P);
                a.cmp_rr(R::Rax, g(*p)?);
                a.jcc(Cond::NE, stub(*snap));
            }
            Op::GuardUvSlot { cl, idx, slot, snap } => {
                let s = stub(*snap);
                let disp = OFF_CL_UPREFS + *idx as i32 * TV;
                a.mov_rr(R::Rcx, g(*cl)?);
                a.cmp32_mi(R::Rcx, disp + OFF_TT, LUA_TUPVAL as i32);
                a.jcc(Cond::NE, s);
                a.load(R::Rcx, R::Rcx, disp);
                a.load(R::Rcx, R::Rcx, OFF_UV_V);
                a.lea(R::Rax, R::R13, *slot as i32 * TV);
                a.cmp_rr(R::Rcx, R::Rax);
                a.jcc(Cond::NE, s);
            }
            Op::ULoad { cl, idx, by_ref, stack, snap } => {
                let s = stub(*snap);
                let disp = OFF_CL_UPREFS + *idx as i32 * TV;
                a.mov_rr(R::Rcx, g(*cl)?);
                if *by_ref {
                    a.cmp32_mi(R::Rcx, disp + OFF_TT, LUA_TUPVAL as i32);
                    a.jcc(Cond::NE, s);
                    a.load(R::Rcx, R::Rcx, disp);
                    a.load(R::Rcx, R::Rcx, OFF_UV_V);
                    // not open on a slot the trace may hold in a register:
                    // exit when base <= v < base + stack
                    let ok = a.label();
                    a.cmp_rr(R::Rcx, R::R13);
                    a.jcc(Cond::B, ok);
                    a.lea(R::Rax, R::R13, *stack as i32 * TV);
                    a.cmp_rr(R::Rcx, R::Rax);
                    a.jcc(Cond::B, s);
                    a.bind(ok);
                } else {
                    a.add_ri(R::Rcx, disp);
                }
                let tag = inst.ty.tag().ok_or("ULoad of a non-Lua type")?;
                a.cmp32_mi(R::Rcx, OFF_TT, tag);
                a.jcc(Cond::NE, s);
                load_value(&mut a, dst, inst.ty, R::Rcx, 0)?;
            }
            Op::GuardFlag { p, off, snap } => {
                a.cmp8_mi(g(*p)?, *off, 0);
                a.jcc(Cond::E, stub(*snap));
            }
            Op::NumToI32 { a: x, snap } => {
                if let Home::G(d) = dst {
                    let x = f(*x)?;
                    a.cvttsd2si_x(d, x);
                    a.cvtsi2sd(X(0), d);
                    a.ucomisd(X(0), x);
                    a.jcc(Cond::NE, stub(*snap));
                    a.jcc(Cond::P, stub(*snap));
                } else {
                    // unused result: still guard
                    let x = f(*x)?;
                    a.cvttsd2si_x(R::Rax, x);
                    a.cvtsi2sd(X(0), R::Rax);
                    a.ucomisd(X(0), x);
                    a.jcc(Cond::NE, stub(*snap));
                    a.jcc(Cond::P, stub(*snap));
                }
            }
            Op::I32ToNum(x) => {
                if let Home::F(d) = dst {
                    a.cvtsi2sd(d, g(*x)?);
                }
            }
            Op::AddI32(x, y) => {
                if let Home::G(d) = dst {
                    let (x, y) = (g(*x)?, g(*y)?);
                    a.mov_rr32(R::Rax, x);
                    a.alu32_rr(0x01, R::Rax, y);
                    a.mov_rr32(d, R::Rax);
                }
            }
            Op::ASize(tb) => {
                if let Home::G(d) = dst {
                    a.load32(d, g(*tb)?, OFF_T_SIZEARRAY);
                }
            }
            Op::ARef { t: tb, idx } => {
                if let Home::G(d) = dst {
                    let (tb, idx) = (g(*tb)?, g(*idx)?);
                    a.mov_rr32(R::Rax, idx);
                    a.shl_ri(R::Rax, 4);
                    a.load(R::Rcx, tb, OFF_T_ARRAY);
                    a.add_rr(R::Rax, R::Rcx);
                    a.mov_rr(d, R::Rax);
                }
            }
            Op::Loop => {
                // PHIs start with their pre values
                let moves: Vec<(Home, Home)> = t.insts[i + 1..]
                    .iter()
                    .enumerate()
                    .take_while(|(_, x)| matches!(x.op, Op::Phi { .. } | Op::Nop))
                    .filter_map(|(j, x)| match x.op {
                        Op::Phi { pre, .. } => Some((home[i + 1 + j], home[pre.index()])),
                        _ => None,
                    })
                    .collect();
                parallel_move(&mut a, &moves)?;
                a.align(16);
                a.bind(loop_top);
            }
            Op::Phi { .. } => {}
        }
        if let Some(s) = spill_to {
            match dst {
                Home::F(x) => a.movsd_store(R::Rsp, spill_off(s), x),
                Home::G(r) => a.store(R::Rsp, spill_off(s), r),
                _ => {}
            }
        }
    }
    if let Some(root) = link_to {
        // end of a side trace: the state goes to the stack (its last
        // snapshot) and the root trace runs again from its pre-roll
        let sn = &t.snapshots[back_snap.0 as usize];
        for e in &sn.entries {
            store_value(&mut a, &home, t, e.value, R::R13, e.slot as i32 * TV)?;
        }
        a.mov_ri64(R::Rax, root as u64);
        a.jmp_r(R::Rax);
    } else {
        // back edge: interrupt check, PHIs take their back values, loop
        a.load(R::Rax, R::R12, OFF_GLOBAL);
        a.cmp64_mi(R::Rax, OFF_INTERRUPT, 0);
        a.jcc(Cond::NE, interrupt_stub);
        if allocates {
            a.load(R::Rcx, R::Rax, OFF_TOTALBYTES);
            a.cmp_rm(R::Rcx, R::Rax, OFF_GCTHRESHOLD);
            a.jcc(Cond::AE, interrupt_stub);
        }
        let moves: Vec<(Home, Home)> = t
            .insts
            .iter()
            .enumerate()
            .filter_map(|(j, x)| match x.op {
                Op::Phi { back, .. } => Some((home[j], home[back.index()])),
                _ => None,
            })
            .collect();
        parallel_move(&mut a, &moves)?;
        a.jmp(loop_top);
    }

    // exit stubs: write the snapshot's slots back, return its index
    // exit codes: (tag << 16) | snapshot; the high bit: interrupted at the
    // loop header
    let mut snaps: Vec<&Snapshot> = t.snapshots.iter().collect();
    let interrupt_snap = Snapshot { pc: t.start_pc, ..t.snapshots[back_snap.0 as usize].clone() };
    if !side {
        snaps.push(&interrupt_snap);
    }
    if t.snapshots.len() > 0xffff {
        return Err("too many snapshots".into());
    }
    let tag = (t.tag as u32 & 0x7fff) << 16;
    for (k, sn) in snaps.iter().enumerate() {
        a.bind(stubs[k]);
        for e in &sn.entries {
            store_value(&mut a, &home, t, e.value, R::R13, e.slot as i32 * TV)?;
        }
        if k == t.snapshots.len() {
            a.mov_ri(R::Rax, (0x8000_0000 | tag | back_snap.0) as u64);
        } else {
            // a side trace attached to this exit continues the work
            if t.link_table != 0 {
                let none = a.label();
                a.mov_ri64(R::Rax, (t.link_table + k * 8) as u64);
                a.load(R::Rax, R::Rax, 0);
                a.test_rr(R::Rax, R::Rax);
                a.jcc(Cond::E, none);
                a.jmp_r(R::Rax);
                a.bind(none);
            }
            a.mov_ri(R::Rax, (tag | k as u32) as u64);
        }
        a.jmp(exit);
    }
    a.bind(exit);
    a.add_ri(R::Rsp, SPILL_BYTES);
    for &r in used_g.iter().rev() {
        a.pop(r);
    }
    a.ret();
    if !a.finish() {
        return Err("assembler: unresolved label".into());
    }
    Ok(Code { bytes: a.code, reentry })
}

/// d = x op y (scalar double), without clobbering x or y.
fn binop(a: &mut Asm, opc: u8, d: X, x: X, y: X) {
    if d == x {
        a.sd_rr(opc, d, y);
    } else if d != y {
        a.movapd(d, x);
        a.sd_rr(opc, d, y);
    } else {
        a.movapd(X(0), x);
        a.sd_rr(opc, X(0), y);
        a.movapd(d, X(0));
    }
}

/// Loads the payload of the TValue at [base + disp] into `h` as `ty`.
fn load_value(a: &mut Asm, h: Home, ty: IrType, base: R, disp: i32) -> Result<(), String> {
    match (h, ty) {
        (Home::None, _) => {}
        (Home::F(x), IrType::Number) => a.movsd_load(x, base, disp),
        (Home::G(r), IrType::Boolean) => a.load32(r, base, disp),
        (Home::G(r), _) => a.load(r, base, disp),
        (h, ty) => return Err(format!("can't load {:?} into {:?}", ty, h)),
    }
    Ok(())
}

/// Stores IR value `v` as a full TValue (payload + tag) at [base + disp].
fn store_value(a: &mut Asm, home: &[Home], t: &Trace, v: Ref, base: R, disp: i32) -> Result<(), String> {
    let ty = t.ty(v);
    let tag = ty.tag().ok_or_else(|| format!("store of a non-Lua value {:?}", ty))?;
    match (home[v.index()], &t.inst(v).op) {
        (Home::F(x), _) => a.movsd_store(base, disp, x),
        (Home::G(r), _) => a.store(base, disp, r),
        (Home::S(s), _) => {
            // (exit stubs and stores: rsp is the trace frame's here)
            a.load(R::Rcx, R::Rsp, spill_off(s));
            a.store(base, disp, R::Rcx);
        }
        (Home::None, Op::KNil) => {}
        (Home::None, Op::KBool(b)) => {
            a.mov_ri(R::Rax, *b as u64);
            a.store(base, disp, R::Rax);
        }
        (Home::None, Op::KNum(n)) => {
            a.mov_ri64(R::Rax, n.to_bits());
            a.store(base, disp, R::Rax);
        }
        (Home::None, Op::KPtr(p)) => {
            a.mov_ri64(R::Rax, *p as u64);
            a.store(base, disp, R::Rax);
        }
        (Home::None, Op::KInt(n)) => {
            a.mov_ri64(R::Rax, *n as u64);
            a.store(base, disp, R::Rax);
        }
        (Home::None, _) if ty == IrType::Nil => {}
        (h, op) => return Err(format!("value {} ({:?}) has no home {:?}", v.0, op, h)),
    }
    // (extra, tt) as one 8-byte word: extra = 0
    a.mov_ri64(R::Rax, (tag as u32 as u64) << 32);
    a.store(base, disp + OFF_TAGWORD, R::Rax);
    Ok(())
}

/// Moves `src -> dst` for every pair as if all at once (cycles go through
/// the scratch registers).
fn parallel_move(a: &mut Asm, moves: &[(Home, Home)]) -> Result<(), String> {
    let mut pending: Vec<(Home, Home)> = moves.iter().copied().filter(|(d, s)| d != s && *d != Home::None).collect();
    while !pending.is_empty() {
        if let Some(k) = (0..pending.len()).find(|&k| !pending.iter().any(|&(_, s)| s == pending[k].0)) {
            let (d, s) = pending.remove(k);
            mov_home(a, d, s)?;
        } else {
            // a cycle: park one source in scratch
            let (_, s) = pending[0];
            let tmp = match s {
                Home::F(_) => Home::F(X(1)),
                _ => Home::G(R::Rdx),
            };
            mov_home(a, tmp, s)?;
            for m in pending.iter_mut() {
                if m.1 == s {
                    m.1 = tmp;
                }
            }
        }
    }
    Ok(())
}

fn mov_home(a: &mut Asm, d: Home, s: Home) -> Result<(), String> {
    match (d, s) {
        (Home::F(d), Home::F(s)) => a.movapd(d, s),
        (Home::G(d), Home::G(s)) => a.mov_rr(d, s),
        (Home::F(d), Home::G(s)) => a.movq_xr(d, s),
        (Home::G(d), Home::F(s)) => a.movq_rx(d, s),
        (Home::G(d), Home::S(s)) => a.load(d, R::Rsp, spill_off(s)),
        (Home::F(d), Home::S(s)) => a.movsd_load(d, R::Rsp, spill_off(s)),
        (Home::S(d), Home::G(s)) => a.store(R::Rsp, spill_off(d), s),
        (Home::S(d), Home::F(s)) => a.movsd_store(R::Rsp, spill_off(d), s),
        (Home::S(d), Home::S(s)) => {
            a.load(R::Rax, R::Rsp, spill_off(s));
            a.store(R::Rsp, spill_off(d), R::Rax);
        }
        (d, s) => return Err(format!("move between {:?} and {:?}", d, s)),
    }
    Ok(())
}

// ---- calls out of the trace ---------------------------------------------

/// An argument of a call out of the trace.
enum Arg {
    /// the lua_State
    L,
    /// a value in a general register home
    G(R),
    Imm(u64),
}

/// Caller-saved registers the allocator hands out (sysv64: every xmm too).
const CALLER_SAVED: [R; 6] = [R::Rsi, R::Rdi, R::R8, R::R9, R::R10, R::R11];

/// Calls `f` (sysv64) with `args`, keeping the caller-saved registers in
/// `live` intact; the result is left in rax. rsp is 8 mod 16 in trace code
/// (six pushes after the call into it).
fn call_out(a: &mut Asm, live: &[Home], f: usize, args: &[Arg]) {
    let gs: Vec<R> = live.iter().filter_map(|h| match h {
        Home::G(r) if CALLER_SAVED.contains(r) => Some(*r),
        _ => None,
    }).collect();
    let xs: Vec<X> = live.iter().filter_map(|h| match h {
        Home::F(x) => Some(*x),
        _ => None,
    }).collect();
    let size = 8 * (gs.len() + xs.len()) as i32;
    let frame = (size + 15) / 16 * 16 + 8;
    a.sub_ri(R::Rsp, frame);
    for (k, &r) in gs.iter().enumerate() {
        a.store(R::Rsp, 8 * k as i32, r);
    }
    for (k, &x) in xs.iter().enumerate() {
        a.movsd_store(R::Rsp, 8 * (gs.len() + k) as i32, x);
    }
    const ARGS: [R; 6] = [R::Rdi, R::Rsi, R::Rdx, R::Rcx, R::R8, R::R9];
    // register arguments move all at once (a source can be another
    // argument's register: cycles go through rax); then arguments from the
    // save area, L and constants, which nothing else reads
    let mut moves: Vec<(R, R)> = Vec::new();
    for (k, arg) in args.iter().enumerate() {
        if let Arg::G(r) = *arg {
            if !gs.contains(&r) && r != ARGS[k] {
                moves.push((ARGS[k], r));
            }
        }
    }
    while !moves.is_empty() {
        if let Some(j) = (0..moves.len()).find(|&j| !moves.iter().any(|&(_, s)| s == moves[j].0)) {
            let (d, s) = moves.remove(j);
            a.mov_rr(d, s);
        } else {
            let (_, s) = moves[0];
            a.mov_rr(R::Rax, s);
            for m in moves.iter_mut() {
                if m.1 == s {
                    m.1 = R::Rax;
                }
            }
        }
    }
    for (k, arg) in args.iter().enumerate() {
        let d = ARGS[k];
        match *arg {
            Arg::L => a.mov_rr(d, R::R12),
            Arg::Imm(v) => a.mov_ri64(d, v),
            Arg::G(r) => {
                if let Some(slot) = gs.iter().position(|&s| s == r) {
                    a.load(d, R::Rsp, 8 * slot as i32);
                }
            }
        }
    }
    a.call_abs(f);
    for (k, &r) in gs.iter().enumerate() {
        a.load(r, R::Rsp, 8 * k as i32);
    }
    for (k, &x) in xs.iter().enumerate() {
        a.movsd_load(x, R::Rsp, 8 * (gs.len() + k) as i32);
    }
    a.add_ri(R::Rsp, frame);
}

/// Runtime helpers trace code calls (sysv64; they never raise errors or
/// run the collector).
mod helpers {
    use crate::lobject::*;

    pub unsafe extern "sysv64" fn barrier(L: *mut lua_State, t: *mut LuaTable, v: *mut GCObject) {
        if crate::lgc::isblack(t as *mut GCObject) && crate::lgc::iswhite(v) {
            crate::lgc::luaC_barriertable(L, t, v);
        }
    }

    pub unsafe extern "sysv64" fn tget(t: *mut LuaTable, key: *const TValue) -> *const TValue {
        crate::ltable::luaH_get(t, key)
    }

    /// Raw store; 0 (nothing done) for a key the interpreter must reject.
    pub unsafe extern "sysv64" fn tset(L: *mut lua_State, t: *mut LuaTable, buf: *const TValue) -> i64 {
        let key = buf;
        if (*key).tt == LUA_TNIL || ((*key).tt == LUA_TNUMBER && (*key).value.n.is_nan()) {
            return 0;
        }
        let slot = crate::ltable::luaH_set(L, t, key);
        setobj(slot, buf.add(1));
        crate::lgc::luaC_barriert(L, t, buf.add(1));
        1
    }

    pub unsafe extern "sysv64" fn getstr(t: *mut LuaTable, k: *mut TString) -> *const TValue {
        crate::ltable::luaH_getstr(t, k)
    }

    pub unsafe extern "sysv64" fn strcmp(a: *const TString, b: *const TString) -> i64 {
        crate::lvmutils::luaV_strcmp(a, b) as i64
    }

    pub unsafe extern "sysv64" fn len(t: *mut LuaTable) -> i64 {
        crate::ltable::luaH_getn(t) as i64
    }

    pub unsafe extern "sysv64" fn new_table(L: *mut lua_State, narray: i64, nhash: i64) -> *mut LuaTable {
        crate::ltable::luaH_new(L, narray as i32, nhash as i32)
    }

    /// A fast builtin through `buf` (TValues: result, arguments): >= 0 when
    /// it handled the call.
    pub unsafe extern "sysv64" fn fastcall(L: *mut lua_State, bfid: i64, nargs: i64, nres: i64, buf: *mut TValue) -> i64 {
        let f = crate::lbuiltins::luauF_table[bfid as usize];
        f(L, buf, buf.add(1), nres as i32, buf.add(2), nargs as i32) as i64
    }

    pub unsafe extern "sysv64" fn dup_table(L: *mut lua_State, tpl: *mut LuaTable) -> *mut LuaTable {
        crate::ltable::luaH_clone(L, tpl)
    }
}
