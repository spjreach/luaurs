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
const OFF_CL_ENV: i32 = offset_of!(Closure, env) as i32;
const OFF_CL_P: i32 = (CLOSURE_HEADER + offset_of!(LClosure, p)) as i32;
const OFF_CL_UPREFS: i32 = (CLOSURE_HEADER + offset_of!(LClosure, uprefs)) as i32;

/// r13 = base, r12 = L (as in the method JIT); rax, rcx, rdx and xmm0/1 are
/// scratch.
const GPRS: [R; 10] = [R::Rbx, R::Rbp, R::R14, R::R15, R::Rsi, R::Rdi, R::R8, R::R9, R::R10, R::R11];
const XMMS: [u8; 14] = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Home {
    None,
    G(R),
    F(X),
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
}

/// Compiles `t`. Fails (cleanly) on anything the backend doesn't handle yet.
pub unsafe fn compile(t: &Trace) -> Result<Code, String> {
    let n = t.insts.len();
    let loop_at = t.loop_ref.ok_or("trace has no loop")?.index();
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
    let mut free_g: Vec<R> = GPRS.iter().rev().copied().collect();
    let mut free_x: Vec<X> = XMMS.iter().rev().map(|&x| X(x)).collect();
    let mut dying: Vec<Vec<usize>> = vec![Vec::new(); n + 1];
    for (i, inst) in t.insts.iter().enumerate() {
        // values whose last use was the previous instruction are free now
        for &v in &dying[i] {
            match home[v] {
                Home::G(r) => free_g.push(r),
                Home::F(x) => free_x.push(x),
                Home::None => {}
            }
        }
        if matches!(inst.op, Op::Nop | Op::Loop) {
            continue;
        }
        let defines = !matches!(
            inst.op,
            Op::Guard { .. } | Op::GuardTruthy { .. } | Op::GuardSame { .. } | Op::GuardPlainTable { .. } | Op::XStore { .. } | Op::GuardFlag { .. } | Op::GuardProto { .. } | Op::GuardUvSlot { .. }
        );
        if !defines {
            continue;
        }
        if inst.is_const() && !used[i] {
            continue;
        }
        let Some(fp) = needs_reg(inst.ty) else { continue };
        home[i] = if fp {
            Home::F(free_x.pop().ok_or("out of xmm registers")?)
        } else {
            Home::G(free_g.pop().ok_or("out of general registers")?)
        };
        let end = if used[i] { last[i] } else { i };
        if end != END {
            dying[(end + 1).min(n)].push(i);
        }
    }

    // ---- emission -------------------------------------------------------
    let mut a = Asm::new();
    let exit = a.label();
    let loop_top = a.label();
    let stubs: Vec<Label> = (0..t.snapshots.len() + 1).map(|_| a.label()).collect();
    let interrupt_stub = stubs[t.snapshots.len()];
    let used_g: Vec<R> = [R::Rbx, R::Rbp, R::R12, R::R13, R::R14, R::R15].to_vec();
    for &r in &used_g {
        a.push(r);
    }
    a.mov_rr(R::R13, R::Rdi);
    a.mov_rr(R::R12, R::Rsi);
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

    let g = |r: Ref| match home[r.index()] {
        Home::G(r) => Ok(r),
        h => Err(format!("value {} is not in a general register ({:?})", r.0, h)),
    };
    let f = |r: Ref| match home[r.index()] {
        Home::F(x) => Ok(x),
        h => Err(format!("value {} is not in an xmm register ({:?})", r.0, h)),
    };

    for (i, inst) in t.insts.iter().enumerate() {
        let dst = home[i];
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
                let vt = t.ty(*v);
                if vt.is_gc() {
                    return Err("store of a GC object (needs a write barrier)".into());
                }
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
    }
    // back edge: interrupt check, PHIs take their back values, loop
    a.load(R::Rax, R::R12, OFF_GLOBAL);
    a.cmp64_mi(R::Rax, OFF_INTERRUPT, 0);
    a.jcc(Cond::NE, interrupt_stub);
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

    // exit stubs: write the snapshot's slots back, return its index
    let mut snaps: Vec<&Snapshot> = t.snapshots.iter().collect();
    let interrupt_snap = Snapshot { pc: t.start_pc, ..t.snapshots[back_snap.0 as usize].clone() };
    snaps.push(&interrupt_snap);
    for (k, sn) in snaps.iter().enumerate() {
        a.bind(stubs[k]);
        for e in &sn.entries {
            store_value(&mut a, &home, t, e.value, R::R13, e.slot as i32 * TV)?;
        }
        // the interrupt exit reports the loop-closing snapshot's index with
        // the high bit set (the interpreter resumes at the header)
        let id = if k == t.snapshots.len() { 0x8000_0000 | back_snap.0 } else { k as u32 };
        a.mov_ri(R::Rax, id as u64);
        a.jmp(exit);
    }
    a.bind(exit);
    for &r in used_g.iter().rev() {
        a.pop(r);
    }
    a.ret();
    if !a.finish() {
        return Err("assembler: unresolved label".into());
    }
    Ok(Code { bytes: a.code })
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
        (d, s) => return Err(format!("move between {:?} and {:?}", d, s)),
    }
    Ok(())
}
