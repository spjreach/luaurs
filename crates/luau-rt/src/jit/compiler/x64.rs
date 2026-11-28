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
const OFF_CI_BASE: i32 = offset_of!(CallInfo, base) as i32;
const OFF_CI_TOP: i32 = offset_of!(CallInfo, top) as i32;
const OFF_CI_P: i32 = offset_of!(CallInfo, p) as i32;
const OFF_CI_SAVEDPC: i32 = offset_of!(CallInfo, savedpc) as i32;
const OFF_CI_FLAGS: i32 = offset_of!(CallInfo, flags) as i32;
const OFF_CI_NRESULTS: i32 = offset_of!(CallInfo, nresults) as i32;
const OFF_END_CI: i32 = offset_of!(lua_State, end_ci) as i32;
const SIZE_CI: i32 = std::mem::size_of::<CallInfo>() as i32;
const OFF_BASE: i32 = offset_of!(lua_State, base) as i32;
const OFF_STACK_LAST: i32 = offset_of!(lua_State, stack_last) as i32;
const OFF_UV_V: i32 = offset_of!(UpVal, v) as i32;
const OFF_OPENUPVAL: i32 = offset_of!(lua_State, openupval) as i32;
const OFF_TOP: i32 = offset_of!(lua_State, top) as i32;
const OFF_T_LSIZENODE: i32 = offset_of!(LuaTable, lsizenode) as i32;
const OFF_T_NODE: i32 = offset_of!(LuaTable, node) as i32;
const NODE_SIZE: i32 = std::mem::size_of::<LuaNode>() as i32;
const OFF_NODE_KEY: i32 = (offset_of!(LuaNode, key) + offset_of!(TKey, value)) as i32;
const OFF_NODE_KEYTT: i32 = (offset_of!(LuaNode, key) + offset_of!(TKey, ttnext)) as i32;
const OFF_MARKED: i32 = offset_of!(GCheader, marked) as i32;
const OFF_TS_LEN: i32 = offset_of!(TString, len) as i32;
const OFF_TS_HASH: i32 = offset_of!(TString, hash) as i32;
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
const SPILL_SLOTS: usize = 256;
const SPILL_BYTES: i32 = SPILL_SLOTS as i32 * 16;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Home {
    None,
    G(R),
    F(X),
    /// spill slot [rsp + 8 * n] (an xmm or general value, by its type)
    S(u8),
}

/// (16 bytes each: a vector fits; xmm spills always move all 16)
fn spill_off(n: u8) -> i32 {
    n as i32 * 16
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
    /// offset of the root trace's body re-entry (if it has one)
    pub body_entry: Option<usize>,
    /// offset of the body re-entry with the PHIs in their homes
    pub body_regs: Option<usize>,
    /// see `CompiledTrace::phi_homes`
    pub phi_homes: &'static [(u16, u8, u8, IrType)],
    /// spill slots side traces must not use
    pub reserved: u32,
    /// see `Trace::exit_regs`
    pub exit_regs: Vec<Vec<(u16, u8, u8)>>,
}

/// Compiles `t`. Fails (cleanly) on anything the backend doesn't handle yet.
pub unsafe fn compile(t: &Trace, end: super::End) -> Result<Code, String> {
    use super::End;
    let n = t.insts.len();
    // side traces run in their root's frame: no prologue
    let side = t.side_of.is_some();
    // (a trace that doesn't loop ends by jumping somewhere)
    let loop_at = match t.loop_ref {
        Some(r) if end == End::Loop => r.index(),
        None if end != End::Loop => usize::MAX,
        _ => return Err("trace ending doesn't fit its loop".into()),
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
    // conversions only exits read (no instruction uses them) are done in
    // the exit code, not on every iteration: exits need their operand
    let mut operand_use = vec![false; n];
    for x in &t.insts {
        for o in x.operands() {
            operand_use[o.index()] = true;
        }
    }
    let deferred: Vec<bool> = (0..n).map(|i| matches!(t.insts[i].op, Op::I32ToNum(_) | Op::I64ToNum(..)) && !operand_use[i]).collect();
    // I32 constants used only by integer adds / ands: immediates, no
    // register
    let mut imm_only = vec![true; n];
    for x in &t.insts {
        let imm_user = matches!(x.op, Op::AddI32(..) | Op::AndI32(..) | Op::AddI64(..) | Op::SubI64(..) | Op::MulI64(..));
        for o in x.operands() {
            if !imm_user {
                imm_only[o.index()] = false;
            }
        }
    }
    for (i, f) in imm_only.iter_mut().enumerate() {
        *f = *f && (matches!(t.insts[i].op, Op::KI32(_)) || matches!(t.insts[i].op, Op::KI64(k) if i32::try_from(k).is_ok()));
    }
    // and integer + constant values only those conversions read: added in
    // the exit code too (an index + 1 kept for exits)
    let mut other_use = vec![false; n];
    for (i, x) in t.insts.iter().enumerate() {
        if deferred[i] {
            continue;
        }
        for o in x.operands() {
            other_use[o.index()] = true;
        }
    }
    for s in &t.snapshots {
        for e in &s.entries {
            if !deferred[e.value.index()] {
                other_use[e.value.index()] = true;
            }
        }
    }
    for sk in &t.sinks {
        let vals = sk.states.iter().flat_map(|st| st.fields.iter().map(|f| f.1).chain(st.meta)).chain(sk.entry.into_iter().flat_map(|(x, y)| [x, y]));
        for o in vals {
            if !deferred[o.index()] {
                other_use[o.index()] = true;
            }
        }
    }
    let deferred_add: Vec<bool> = (0..n)
        .map(|i| !other_use[i] && operand_use[i] && matches!(t.insts[i].op, Op::AddI32(y, k) if imm_only[k.index()] && !imm_only[y.index()]))
        .collect();
    let exit_need = |v: Ref| -> Ref {
        match t.insts[v.index()].op {
            Op::I32ToNum(x) if deferred[v.index()] => match t.insts[x.index()].op {
                Op::AddI32(y, _) if deferred_add[x.index()] => y,
                _ => x,
            },
            Op::I64ToNum(x, _) if deferred[v.index()] => x,
            _ => v,
        }
    };
    // the loop's interrupt exit resumes at the header with every slot the
    // iteration wrote. The recorder appends that snapshot last; using the
    // last snapshot (not the last *guard*) keeps the restore complete.
    let back_snap = SnapId(t.snapshots.len().checked_sub(1).ok_or("trace has no snapshot")? as u32);
    // per snapshot: values exits through it need to build sunk tables
    let mut sink_vals: Vec<Vec<Ref>> = vec![Vec::new(); t.snapshots.len()];
    for sk in &t.sinks {
        for st in &sk.states {
            let v = &mut sink_vals[st.snap as usize];
            v.extend(st.fields.iter().map(|f| f.1));
            v.extend(st.meta);
            if let Some((x, e)) = sk.entry {
                v.extend([x, e]);
            }
        }
    }
    // a PHI lives from the loop head (all of them are written at once
    // there: none may share a register) to its last use - not to the
    // loop's end: the back edge only writes it (its back value may then
    // take its register, see the allocation)
    let phi_block_end = t.insts.iter().enumerate().filter(|(_, x)| matches!(x.op, Op::Phi { .. })).map(|(j, _)| j).max().unwrap_or(0);
    for (i, inst) in t.insts.iter().enumerate() {
        match inst.op {
            Op::Phi { pre, back } => {
                touch(pre, loop_at, &mut last, &mut used);
                touch(back, END, &mut last, &mut used);
                if last[i] < phi_block_end {
                    last[i] = phi_block_end;
                }
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
                touch(exit_need(e.value), i, &mut last, &mut used);
            }
            for &v in &sink_vals[s.0 as usize] {
                touch(exit_need(v), i, &mut last, &mut used);
            }
        }
    }
    for e in &t.snapshots[back_snap.0 as usize].entries {
        touch(exit_need(e.value), END, &mut last, &mut used);
    }
    for &v in &sink_vals[back_snap.0 as usize] {
        touch(exit_need(v), END, &mut last, &mut used);
    }

    // ---- allocation -----------------------------------------------------
    let mut home = vec![Home::None; n];
    // (a side trace runs in its root's frame: the root's slots below)
    let spill_base = t.spill_base as usize;
    // (LUAU_TRACE_REGS=n: only n registers of each kind, to test spilling)
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let limit = *LIMIT.get_or_init(|| std::env::var("LUAU_TRACE_REGS").ok().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX));
    let mut free_g: Vec<R> = GPRS.iter().take(limit).rev().copied().collect();
    let mut free_x: Vec<X> = XMMS.iter().take(limit).rev().map(|&x| X(x)).collect();
    // a side trace's entry values its parent's exit left in registers (see
    // `Trace::inherit`): those registers are theirs from the start
    let mut forced: Vec<Option<Home>> = vec![None; n];
    for &(i, kind, reg) in &t.inherit {
        let i = i as usize;
        if i >= n || !used[i] || !matches!(t.insts[i].op, Op::SLoad { guarded: false, .. }) || (0..i).any(|j| !matches!(t.insts[j].op, Op::SLoad { .. } | Op::Nop)) {
            continue;
        }
        if kind == 1 {
            if let Some(k) = free_x.iter().position(|x| x.0 == reg) {
                forced[i] = Some(Home::F(free_x.remove(k)));
            }
        } else if let Some(k) = free_g.iter().position(|r| *r as u8 == reg) {
            forced[i] = Some(Home::G(free_g.remove(k)));
        }
    }
    // (inherited values that didn't get their parent's register: filled
    // in from it at the start - computed once homes are known, below)
    let inherit_rest: Vec<(usize, u8, u8)> = t.inherit.iter().map(|&(i, k, r)| (i as usize, k, r)).filter(|&(i, _, _)| i < n && forced[i].is_none() && matches!(t.insts[i].op, Op::SLoad { guarded: false, .. }) && (0..i).all(|j| matches!(t.insts[j].op, Op::SLoad { .. } | Op::Nop))).collect();
    // spill slots: occupied now, and the last instruction a past occupant
    // used it at (a value spilled after the fact holds its slot from its
    // definition on, so only a slot unused since then will do)
    let mut slot_busy = [false; SPILL_SLOTS];
    let mut slot_until: [Option<usize>; SPILL_SLOTS] = [None; SPILL_SLOTS];
    // values holding a register now (to pick one to spill)
    let mut active: Vec<usize> = Vec::new();
    let mut dying: Vec<Vec<usize>> = vec![Vec::new(); n + 1];
    let end_of = |v: usize, last: &[usize], used: &[bool]| if used[v] { last[v] } else { v };
    // back value -> its PHI
    let mut phi_of: Vec<Option<usize>> = vec![None; n];
    for (j, x) in t.insts.iter().enumerate() {
        if let Op::Phi { back, .. } = x.op {
            phi_of[back.index()] = Some(j);
        }
    }
    // registers handed from a dying PHI to its back value (not freed)
    let mut handed = vec![false; n];
    // values from before the loop that the body reads as operands (the
    // others it only names in exits: they wait in spill slots, leaving
    // the registers to the loop)
    let mut body_operand = vec![false; n];
    if loop_at != usize::MAX {
        for x in t.insts.iter().skip(loop_at + 1) {
            for o in x.operands() {
                body_operand[o.index()] = true;
            }
            if let Op::Phi { pre, .. } = x.op {
                body_operand[pre.index()] = true;
            }
        }
    }
    for (i, inst) in t.insts.iter().enumerate() {
        // values whose last use was the previous instruction are free now
        for &v in &dying[i] {
            if handed[v] {
                active.retain(|&w| w != v);
                continue;
            }
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
        if matches!(inst.op, Op::Nop | Op::Loop) || deferred[i] || imm_only[i] || deferred_add[i] {
            continue;
        }
        let defines = !matches!(
            inst.op,
            Op::Guard { .. } | Op::GuardTruthy { .. } | Op::GuardSame { .. } | Op::GuardDiff { .. } | Op::GuardPlainTable { .. } | Op::XStore { .. } | Op::GuardFlag { .. } | Op::GuardProto { .. } | Op::GuardUvSlot { .. } | Op::Barrier { .. } | Op::HMiss { .. } | Op::GuardCFunc { .. } | Op::TSet { .. } | Op::CCall { .. } | Op::Concat { .. } | Op::UStore { .. } | Op::GuardVecEq { .. } | Op::CloseUpvals { .. } | Op::GuardTop { .. } | Op::Sunk
        ) && !matches!(inst.op, Op::Builtin { nres: 0, .. });
        if !defines {
            continue;
        }
        if inst.is_const() && !used[i] {
            continue;
        }
        let Some(fp) = needs_reg(inst.ty) else { continue };
        let end = end_of(i, &last, &used);
        if let Some(h) = forced[i] {
            home[i] = h;
            active.push(i);
            if end != END {
                dying[(end + 1).min(n)].push(i);
            }
            continue;
        }
        // a back value computed from its PHI's last use takes the PHI's
        // register (no move at the back edge); only for instructions
        // without exits that allow their result in an operand's register
        if let Some(p) = phi_of[i] {
            let shares = matches!(inst.op, Op::Add(..) | Op::Sub(..) | Op::Mul(..) | Op::Div(..) | Op::AddI32(..) | Op::Neg(_));
            if shares && last[p] == i && !handed[p] && matches!((home[p], fp), (Home::G(_), false) | (Home::F(_), true)) {
                home[i] = home[p];
                handed[p] = true;
                active.retain(|&w| w != p);
                active.push(i);
                if end != END {
                    dying[(end + 1).min(n)].push(i);
                }
                continue;
            }
        }
        if loop_at != usize::MAX && i < loop_at && end == END && !body_operand[i] && !inst.is_const() {
            if let Some(k) = (spill_base..SPILL_SLOTS).find(|&k| !slot_busy[k] && slot_until[k].is_none_or(|u| u < i)) {
                slot_busy[k] = true;
                home[i] = Home::S(k as u8);
                continue;
            }
        }
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
                // (not an inherited entry value: it is in its register, never
                // loaded - a spill slot of it would hold nothing)
                let same = |w: &usize| matches!((home[*w], fp), (Home::F(_), true) | (Home::G(_), false)) && forced[*w].is_none();
                let victim = active.iter().copied().filter(same).max_by_key(|&w| end_of(w, &last, &used));
                let mut take_slot = |from: usize| -> Result<Home, String> {
                    let k = (spill_base..SPILL_SLOTS).find(|&k| !slot_busy[k] && slot_until[k].is_none_or(|u| u < from)).ok_or("out of spill slots")?;
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

    // body re-entry (root traces): side traces ending here run the loop
    // body again without the pre-loop part. The pre-loop values the body
    // uses are saved at the loop head (spilled ones already are: side
    // traces spill above the root's slots), the PHIs reload from their
    // stack slots.
    let mut saves: Vec<(usize, u8)> = Vec::new();
    // (PHI, its slot, whether it is a counter's I32 twin: the slot's
    // number converted)
    let mut phi_loads: Vec<(usize, u16, bool)> = Vec::new();
    let mut body_entry_ok = false;
    let mut reserved = spill_base;
    for h in &home {
        if let Home::S(s) = h {
            reserved = reserved.max(*s as usize + 1);
        }
    }
    if loop_at != usize::MAX && !side {
        body_entry_ok = true;
        for v in 0..loop_at {
            if used[v] && last[v] == END && matches!(home[v], Home::G(_) | Home::F(_)) {
                if reserved >= SPILL_SLOTS {
                    body_entry_ok = false;
                    break;
                }
                saves.push((v, reserved as u8));
                reserved += 1;
            }
        }
        for (j, x) in t.insts.iter().enumerate().skip(loop_at + 1) {
            match x.op {
                Op::Phi { pre, .. } => match t.inst(pre).op {
                    Op::SLoad { slot, .. } if x.ty.tag().is_some() => phi_loads.push((j, slot, false)),
                    Op::NumToI32 { a: src, .. } if x.ty == IrType::I32 => match t.inst(src).op {
                        Op::SLoad { slot, .. } => phi_loads.push((j, slot, true)),
                        _ => body_entry_ok = false,
                    },
                    Op::NumToI64 { a: src, .. } if x.ty == IrType::I64 => match t.inst(src).op {
                        Op::SLoad { slot, .. } => phi_loads.push((j, slot, true)),
                        _ => body_entry_ok = false,
                    },
                    _ => body_entry_ok = false,
                },
                Op::Nop => {}
                _ => break,
            }
        }
    }
    if crate::trace_flag!("LUAU_TRACE_HOMES") {
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
    let allocates = t.insts.iter().any(|x| matches!(x.op, Op::NewTable { .. } | Op::DupTable { .. } | Op::NumToStr(_) | Op::Builtin { .. } | Op::TSet { .. } | Op::CCall { .. } | Op::Concat { .. } | Op::UStore { .. } | Op::MakeClosure { .. } | Op::CallC { .. }));

    // stores whose slot already holds the value's type: an earlier load of
    // that slot checked it, and nothing since could have changed any
    // slot's type to another (only stores of that same type ran) - the
    // value alone is written
    let mut tag_known = vec![false; n];
    if !crate::trace_flag!("LUAU_TRACE_NOTAGELIDE") {
        for (i, x) in t.insts.iter().enumerate() {
            let Op::XStore { p, v } = x.op else { continue };
            let ty = t.ty(v);
            if ty != IrType::Number {
                continue;
            }
            for j in (0..i).rev() {
                match &t.insts[j].op {
                    Op::XLoad { p: q, .. } if *q == p && t.insts[j].ty == ty => {
                        tag_known[i] = true;
                        break;
                    }
                    Op::XStore { v: w, .. } if t.ty(*w) == ty => {}
                    Op::XLoad { .. } | Op::SLoad { .. } | Op::Guard { .. } | Op::GuardTruthy { .. } | Op::GuardSame { .. } | Op::GuardDiff { .. } | Op::NumToI32 { .. } | Op::NumToI64 { .. } | Op::GuardPlainTable { .. } | Op::Nop => {}
                    _ if t.insts[j].is_pure() => {}
                    // (a PHI, the loop head, calls, other stores: no)
                    _ => break,
                }
            }
        }
    }

    let mut moved_in: Vec<(Home, Home)> = Vec::new();
    let mut inherited_moved = vec![false; n];
    for &(i, kind, reg) in &inherit_rest {
        if home[i] == Home::None {
            inherited_moved[i] = true;
            continue;
        }
        let src = if kind == 1 { Home::F(X(reg)) } else { Home::G(gpr_of(reg).ok_or("inherited register")?) };
        moved_in.push((home[i], src));
        inherited_moved[i] = true;
    }

    // ---- emission -------------------------------------------------------
    let mut a = Asm::new();
    let exit = a.label();
    let loop_top = a.label();
    let stubs: Vec<Label> = (0..t.snapshots.len() + 1).map(|_| a.label()).collect();
    let interrupt_stub = stubs[t.snapshots.len()];
    // a called C function yielded: leave as it left the VM
    let left_vm = a.label();
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
    // (before the entry checks: their exits store these values)
    if !moved_in.is_empty() {
        parallel_move(&mut a, &moved_in)?;
    }
    let counting = crate::jit::debug::counting();
    if counting {
        // (LUAU_TRACE_COUNT: entries of this trace)
        a.mov_ri64(R::Rax, crate::jit::debug::counter(t.tag, 0) as u64);
        a.add32_mi(R::Rax, 0, 1);
    }
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
    if let (Some(n), false) = (t.varargs, side) {
        // the frame has as many varargs as when recorded: base - func =
        // 1 + numparams + n slots
        a.load(R::Rax, R::R12, OFF_CI);
        a.load(R::Rax, R::Rax, OFF_CI_FUNC);
        a.mov_rr(R::Rcx, R::R13);
        a.sub_rr(R::Rcx, R::Rax);
        a.cmp_ri(R::Rcx, (1 + (*t.proto).numparams as i32 + n as i32) * TV);
        a.jcc(Cond::NE, stubs[0]);
    }
    // (a frame has room for its function's own slots: the call checked;
    // a vararg function's base moved up after that)
    let room = if !t.proto.is_null() && (*t.proto).is_vararg == 0 && !crate::trace_flag!("LUAU_TRACE_STACKCHK") { (*t.proto).maxstacksize as u32 } else { 0 };
    if t.stack_need > room {
        a.load(R::Rax, R::R12, OFF_STACK_LAST);
        a.sub_rr(R::Rax, R::R13);
        a.mov_ri(R::Rcx, (t.stack_need as u64) * TV as u64);
        a.cmp_rr(R::Rax, R::Rcx);
        a.jcc(Cond::L, stubs[0]);
    }

    for (i, inst) in t.insts.iter().enumerate() {
        // (an inherited entry value is in its register already, or moved
        // there from the parent's)
        if deferred[i] || deferred_add[i] || forced[i].is_some() || inherited_moved[i] {
            continue;
        }
        // spilled operands go to the temp registers for this instruction
        // (stores and builtin arguments take values from anywhere)
        let mut over: Vec<(usize, Home)> = Vec::new();
        let (mut tg, mut tx) = (0usize, 0usize);
        if !matches!(inst.op, Op::Phi { .. } | Op::Loop | Op::Builtin { .. }) {
            let mut ops = inst.operands();
            if let Op::XStore { v, .. } = inst.op {
                ops.retain(|&o| o != v);
            }
            if let Op::MakeClosure { vals, nvals, .. } = inst.op {
                ops.retain(|o| !vals[..nvals as usize].contains(o));
            }
            if let Op::TGet { k, .. } | Op::TSet { k, .. } = inst.op {
                ops.retain(|&o| o != k);
            }
            if let Op::TSet { v, .. } | Op::UStore { v, .. } = inst.op {
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
                        a.movups_load(x, R::Rsp, spill_off(s));
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
            Op::KI64(v) => {
                if let Home::G(r) = dst {
                    a.mov_ri64(r, *v as u64);
                }
            }
            Op::NumToI64 { a: x, lim, shift, snap } => {
                let x = f(*x)?;
                let d = if let Home::G(d) = dst { d } else { R::Rax };
                num_to_i64(&mut a, d, x, *lim, *shift, stub(*snap));
            }
            Op::I32ToI64(x) => {
                if let Home::G(d) = dst {
                    a.movsxd(d, g(*x)?);
                }
            }
            Op::I64ToNum(x, s) => {
                if let Home::F(d) = dst {
                    // (cleared first: cvtsi2sd keeps the register's upper
                    // half, a dependency on its last value)
                    a.xorps(d, d);
                    a.cvtsi2sd64(d, g(*x)?);
                    scale_down(&mut a, d, *s);
                }
            }
            Op::NegI64(x) => {
                if let Home::G(d) = dst {
                    let x = g(*x)?;
                    if d != x {
                        a.mov_rr(d, x);
                    }
                    a.unary64(3, d);
                }
            }
            Op::AddI64(x, y) | Op::SubI64(x, y) | Op::MulI64(x, y) => {
                if let Home::G(d) = dst {
                    let kv = |r: Ref| if imm_only[r.index()] { if let Op::KI64(k) = t.inst(r).op { Some(k as i32) } else { None } } else { None };
                    let sub = matches!(inst.op, Op::SubI64(..));
                    let mul = matches!(inst.op, Op::MulI64(..));
                    // (commutative ones with the constant first: swapped)
                    let (x, y) = if !sub && kv(*x).is_some() { (*y, *x) } else { (*x, *y) };
                    if kv(x).is_some() {
                        return Err("I64 op with a constant first operand".into());
                    }
                    let xr = g(x)?;
                    if let Some(c) = kv(y) {
                        if mul {
                            a.imul64_rri(d, xr, c);
                        } else {
                            if d != xr {
                                a.mov_rr(d, xr);
                            }
                            a.alu64_ri(if sub { 5 } else { 0 }, d, c);
                        }
                    } else {
                        let yr = g(y)?;
                        // in rcx: d may be either operand
                        a.mov_rr(R::Rcx, xr);
                        if mul {
                            a.imul64_rr(R::Rcx, yr);
                        } else {
                            a.alu64_rr(if sub { 0x29 } else { 0x01 }, R::Rcx, yr);
                        }
                        a.mov_rr(d, R::Rcx);
                    }
                }
            }
            Op::ModI64K(x, k) => {
                if let Home::G(d) = dst {
                    let (m, s) = magic_s64(*k).ok_or("no division magic")?;
                    let xr = g(*x)?;
                    // q = x / k rounded toward zero (Hacker's Delight 10-1)
                    a.mov_ri64(R::Rax, m as u64);
                    a.unary64(5, xr); // rdx:rax = rax * x
                    if m < 0 {
                        a.add_rr(R::Rdx, xr);
                    }
                    if s > 0 {
                        a.sar_ri(R::Rdx, s as u8);
                    }
                    a.mov_rr(R::Rax, xr);
                    a.shr_ri(R::Rax, 63);
                    a.add_rr(R::Rdx, R::Rax);
                    // r = x - q * k, then + k when negative (rounded down)
                    a.imul64_rri(R::Rdx, R::Rdx, *k as i32);
                    a.mov_rr(R::Rcx, xr);
                    a.sub_rr(R::Rcx, R::Rdx);
                    a.mov_rr(R::Rax, R::Rcx);
                    a.sar_ri(R::Rax, 63);
                    a.alu64_ri(4, R::Rax, *k as i32);
                    a.add_rr(R::Rcx, R::Rax);
                    a.mov_rr(d, R::Rcx);
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
                store_value_tag(&mut a, &home, t, *v, p, 0, !tag_known[i])?;
            }
            Op::Select { cmp, a: x, b: y, t: tv, f: fv } => {
                if let (Home::F(d), false, Cmp::Lt | Cmp::Le | Cmp::Nlt | Cmp::Nle | Cmp::Eq | Cmp::Ne) = (dst, crate::trace_flag!("LUAU_TRACE_BRSELECT"), cmp) {
                    // branchless: the condition as a mask (cmpsd's
                    // predicates treat NaN as the comparisons do), then a
                    // blend
                    let (x, y, tv, fv) = (f(*x)?, f(*y)?, f(*tv)?, f(*fv)?);
                    let pred = match cmp {
                        Cmp::Lt => 1,
                        Cmp::Le => 2,
                        Cmp::Nlt => 5,
                        Cmp::Nle => 6,
                        Cmp::Eq => 0,
                        _ => 4,
                    };
                    a.movapd(X(0), x);
                    a.cmpsd(X(0), y, pred);
                    a.movapd(X(1), fv);
                    a.blendvpd(X(1), tv);
                    a.movapd(d, X(1));
                } else if let Home::F(d) = dst {
                    // (a native branch: predicted like the interpreter's)
                    let (x, y, tv, fv) = (f(*x)?, f(*y)?, f(*tv)?, f(*fv)?);
                    let no = a.label();
                    let done = a.label();
                    match cmp {
                        Cmp::Lt => {
                            a.ucomisd(y, x);
                            a.jcc(Cond::BE, no);
                        }
                        Cmp::Le => {
                            a.ucomisd(y, x);
                            a.jcc(Cond::B, no);
                        }
                        Cmp::Nlt => {
                            a.ucomisd(y, x);
                            a.jcc(Cond::A, no);
                        }
                        Cmp::Nle => {
                            a.ucomisd(y, x);
                            a.jcc(Cond::AE, no);
                        }
                        Cmp::Eq => {
                            a.ucomisd(x, y);
                            a.jcc(Cond::NE, no);
                            a.jcc(Cond::P, no);
                        }
                        Cmp::Ne => {
                            let yes = a.label();
                            a.ucomisd(x, y);
                            a.jcc(Cond::P, yes);
                            a.jcc(Cond::E, no);
                            a.bind(yes);
                        }
                        Cmp::ULt => return Err("unsigned select of numbers".into()),
                    }
                    if d != tv {
                        a.movapd(d, tv);
                    }
                    a.jmp(done);
                    a.bind(no);
                    if d != fv {
                        a.movapd(d, fv);
                    }
                    a.bind(done);
                }
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
                        Cmp::ULt => return Err("unsigned compare of numbers".into()),
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
                        Cmp::ULt => Cond::AE,
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
                    // (a power-of-two divisor: multiply by its reciprocal -
                    // the same exact scaling, one rounding)
                    let recip = match t.inst(*y).op {
                        Op::KNum(v) => exact_reciprocal(v),
                        _ => None,
                    };
                    let (x, y) = (f(*x)?, f(*y)?);
                    // q = floor(x / y)
                    if let Some(r) = recip {
                        a.mov_ri64(R::Rax, r.to_bits());
                        a.movq_xr(X(0), R::Rax);
                        a.sd_rr(0x59, X(0), x);
                    } else {
                        a.movapd(X(0), x);
                        a.sd_rr(0x5e, X(0), y);
                    }
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
            Op::Pow(x, y) => {
                let live = live_across(i);
                call_out(&mut a, &live, helpers::pow as usize, &[Arg::F(f(*x)?), Arg::F(f(*y)?)]);
                if let Home::F(d) = dst {
                    a.movapd(d, X(0));
                }
            }
            Op::VargLoad { idx, snap } => {
                let n = t.varargs.ok_or("vararg load without a vararg count")? as i32;
                let s = stub(*snap);
                let tag = inst.ty.tag().ok_or("vararg of a non-Lua type")?;
                let (p, disp) = match t.inst(*idx).op {
                    Op::KI32(k) => (R::R13, (k - n) * TV),
                    _ => {
                        a.mov_rr32(R::Rcx, g(*idx)?);
                        a.shl_ri(R::Rcx, 4);
                        a.add_rr(R::Rcx, R::R13);
                        (R::Rcx, -n * TV)
                    }
                };
                a.cmp32_mi(p, disp + OFF_TT, tag);
                a.jcc(Cond::NE, s);
                load_value(&mut a, dst, inst.ty, p, disp)?;
            }
            Op::VArith { op, a: x, b: y, floor } => {
                // operands to xmm0 / xmm1 (a number: rounded, broadcast)
                for (k, v) in [(0u8, *x), (1u8, *y)] {
                    let src = f(v)?;
                    if t.ty(v) == IrType::Number {
                        a.cvtsd2ss(X(k), src);
                        a.shufps(X(k), X(k), 0);
                    } else {
                        a.movaps(X(k), src);
                    }
                }
                a.ps_rr(*op, X(0), X(1));
                if *floor {
                    a.roundps(X(0), X(0), 9);
                }
                if let Home::F(d) = dst {
                    a.movaps(d, X(0));
                }
            }
            Op::VNeg(x) => {
                a.mov_ri(R::Rax, 0x8000_0000);
                a.movd_xr(X(1), R::Rax);
                a.shufps(X(1), X(1), 0);
                a.movaps(X(0), f(*x)?);
                a.xorps(X(0), X(1));
                if let Home::F(d) = dst {
                    a.movaps(d, X(0));
                }
            }
            Op::VGet { v, lane } => {
                a.movaps(X(0), f(*v)?);
                if *lane != 0 {
                    a.shufps(X(0), X(0), *lane);
                }
                if let Home::F(d) = dst {
                    a.cvtss2sd(d, X(0));
                }
            }
            Op::VMake { x, y, z } => {
                a.cvtsd2ss(X(0), f(*x)?);
                a.cvtsd2ss(X(1), f(*y)?);
                a.unpcklps(X(0), X(1));
                a.cvtsd2ss(X(1), f(*z)?);
                a.movlhps(X(0), X(1));
                if let Home::F(d) = dst {
                    a.movaps(d, X(0));
                }
            }
            Op::GuardVecEq { a: x, b: y, want, snap } => {
                a.movaps(X(0), f(*x)?);
                a.cmpps(X(0), f(*y)?, 0);
                a.movmskps(R::Rax, X(0));
                a.and32_ri(R::Rax, 7);
                a.cmp32_ri(R::Rax, 7);
                a.jcc(if *want { Cond::NE } else { Cond::E }, stub(*snap));
            }
            Op::LudToI32(x) | Op::I32ToLud(x) => {
                if let Home::G(d) = dst {
                    a.mov_rr32(d, g(*x)?);
                }
            }
            Op::Not(x) => {
                if let Home::G(d) = dst {
                    a.mov_rr(d, g(*x)?);
                    a.alu32_ri(6, d, 1);
                }
            }
            Op::Concat { a: ra, b, c, savedpc, frames, off, snap } => {
                for e in &t.snapshots[snap.0 as usize].entries {
                    store_value(&mut a, &home, t, e.value, R::R13, e.slot as i32 * TV)?;
                }
                let live = live_across(i);
                let (ra, b, c) = (*ra - *off, *b - *off, *c - *off);
                // (two operands: a per-site cache of results, see
                // `helpers::concat`)
                let cache = if c == b + 1 { Box::leak(Box::new(helpers::ConcatCache::new())) as *mut _ as u64 } else { 0 };
                call_out(&mut a, &live, helpers::concat as usize, &[Arg::L, Arg::Imm(ra as u64 | (b as u64) << 16 | (c as u64) << 32), Arg::Imm(*savedpc as u64), Arg::Imm(*frames as u64), Arg::Imm(cache)]);
                a.load(R::R13, R::R12, OFF_BASE);
            }
            Op::UStore { cl, idx, v, buf, snap } => {
                a.mov_ri64(R::Rdx, *buf as u64);
                store_value(&mut a, &home, t, *v, R::Rdx, 0)?;
                let live = live_across(i);
                call_out(&mut a, &live, helpers::ustore as usize, &[Arg::L, Arg::G(g(*cl)?), Arg::Imm(*idx as u64), Arg::Imm(*buf as u64)]);
                a.test_rr(R::Rax, R::Rax);
                a.jcc(Cond::E, stub(*snap));
            }
            Op::PopFrame { ret, snap } => {
                // (a call from C, or another caller: exit at the RETURN)
                let s = stub(*snap);
                a.load(R::Rcx, R::R12, OFF_CI);
                a.test32_mi(R::Rcx, OFF_CI_FLAGS, LUA_CALLINFO_RETURN);
                a.jcc(Cond::NE, s);
                a.mov_rr(R::Rdx, R::Rcx);
                a.sub_ri(R::Rdx, SIZE_CI);
                a.mov_ri64(R::Rax, *ret as u64);
                a.cmp_rm(R::Rax, R::Rdx, OFF_CI_SAVEDPC);
                a.jcc(Cond::NE, s);
                // the caller's frame from here on
                a.store(R::R12, OFF_CI, R::Rdx);
                a.load(R::R13, R::Rdx, OFF_CI_BASE);
                a.store(R::R12, OFF_BASE, R::R13);
                a.load(R::Rax, R::Rdx, OFF_CI_TOP);
                a.store(R::R12, OFF_TOP, R::Rax);
                if let Home::G(d) = dst {
                    a.load(d, R::Rdx, OFF_CI_FUNC);
                    a.load(d, d, 0);
                }
            }
            Op::NumToStr(x) => {
                let live = live_across(i);
                // (a per-site cache of results: see `helpers::num_tostring`)
                let cache = Box::leak(Box::new([[0u64; 3]; 256])) as *mut _ as u64;
                call_out(&mut a, &live, helpers::num_tostring as usize, &[Arg::L, Arg::F(f(*x)?), Arg::Imm(cache)]);
                if let Home::G(d) = dst {
                    a.mov_rr(d, R::Rax);
                }
            }
            Op::Random { state } => {
                // PCG32 twice (see `lmathlib::pcg32_random`): lo, hi
                const MUL: u64 = 6364136223846793005;
                a.mov_ri64(R::R10, *state as u64);
                a.load(R::Rax, R::R10, 0);
                a.mov_ri64(R::R11, MUL);
                a.mov_rr(R::Rdx, R::Rax);
                a.imul64_rr(R::Rdx, R::R11);
                a.alu64_ri(0, R::Rdx, 105);
                a.mov_rr(R::Rcx, R::Rdx);
                a.imul64_rr(R::Rcx, R::R11);
                a.alu64_ri(0, R::Rcx, 105);
                a.store(R::R10, 0, R::Rcx);
                // the output of state s: rotr32(((s >> 18) ^ s) >> 27, s >> 59)
                for (s, out) in [(R::Rax, R::R10), (R::Rdx, R::R11)] {
                    a.mov_rr(R::Rcx, s);
                    a.shr_ri(R::Rcx, 59);
                    a.mov_rr(out, s);
                    a.shr_ri(out, 18);
                    a.alu64_rr(0x31, out, s);
                    a.shr_ri(out, 27);
                    a.shift32_cl(1, out);
                }
                // v = lo | hi << 32, as a double (unsigned: halved with the
                // lost bit kept when the top bit is set, then doubled)
                a.mov_rr(R::Rax, R::R11);
                a.shl_ri(R::Rax, 32);
                a.alu64_rr(0x09, R::Rax, R::R10);
                a.mov_rr(R::R11, R::Rax);
                a.shr_ri(R::R11, 63);
                a.mov_rr(R::Rdx, R::Rax);
                a.shr_ri(R::Rdx, 1);
                a.mov_rr(R::Rcx, R::Rax);
                a.alu32_ri(4, R::Rcx, 1);
                a.alu64_rr(0x09, R::Rdx, R::Rcx);
                a.test_rr(R::Rax, R::Rax);
                a.cmov64(Cond::L, R::Rax, R::Rdx);
                a.cvtsi2sd64(X(0), R::Rax);
                a.movq_rx(R::Rax, X(0));
                a.shl_ri(R::R11, 52);
                a.alu64_rr(0x01, R::Rax, R::R11);
                a.movq_xr(X(0), R::Rax);
                // * 2^-64
                a.mov_ri64(R::Rax, 0x3bf0_0000_0000_0000);
                a.movq_xr(X(1), R::Rax);
                a.sd_rr(0x59, X(0), X(1));
                if let Home::F(d) = dst {
                    a.movapd(d, X(0));
                }
            }
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
            Op::SReload { slot, snap } => {
                let disp = *slot as i32 * TV;
                let tag = inst.ty.tag().ok_or("SReload of a non-Lua type")?;
                a.cmp32_mi(R::R13, disp + OFF_TT, tag);
                a.jcc(Cond::NE, stub(*snap));
                load_value(&mut a, dst, inst.ty, R::R13, disp)?;
            }
            Op::CCall { slot, nargs, nres, savedpc, frames, off, snap } => {
                // the frame goes to the stack first
                for e in &t.snapshots[snap.0 as usize].entries {
                    store_value(&mut a, &home, t, e.value, R::R13, e.slot as i32 * TV)?;
                }
                let live = live_across(i);
                call_out(
                    &mut a,
                    &live,
                    helpers::ccall as usize,
                    &[Arg::L, Arg::Imm((*slot - *off) as u64), Arg::Imm(*nargs as u64), Arg::Imm(*nres as u64), Arg::Imm(*savedpc as u64), Arg::Imm(*frames as u64)],
                );
                // (the call may have moved the stack)
                a.load(R::R13, R::R12, OFF_BASE);
            }
            Op::CallC { slot, nargs, nres, savedpc, frames, off, snap } => {
                for e in &t.snapshots[snap.0 as usize].entries {
                    store_value(&mut a, &home, t, e.value, R::R13, e.slot as i32 * TV)?;
                }
                let live = live_across(i);
                call_out(
                    &mut a,
                    &live,
                    helpers::callc as usize,
                    &[Arg::L, Arg::Imm((*slot - *off) as u64), Arg::Imm(*nargs as u64), Arg::Imm(*nres as i64 as u64), Arg::Imm(*savedpc as u64), Arg::Imm(*frames as u64)],
                );
                a.cmp_ri(R::Rax, -1);
                a.jcc(Cond::E, left_vm);
                // (Lua code may have moved the stack)
                a.load(R::R13, R::R12, OFF_BASE);
                if let Home::G(d) = dst {
                    a.mov_rr32(d, R::Rax);
                }
            }
            Op::TGet { t: tb, k, .. } if t.ty(*k) == IrType::String && !crate::trace_flag!("LUAU_TRACE_NOSTRGET") => {
                // a string key: the hash part's chain, inline (as
                // luaH_getstr: interned strings compare by address)
                let tb = g(*tb)?;
                let kr = g(*k)?;
                let lp = a.label();
                let next = a.label();
                let found = a.label();
                let missing = a.label();
                a.movzx8(R::Rcx, tb, OFF_T_LSIZENODE);
                a.mov_ri(R::Rax, 1);
                a.shift32_cl(4, R::Rax);
                a.alu32_ri(5, R::Rax, 1);
                a.load32(R::Rdx, kr, OFF_TS_HASH);
                a.alu32_rr(0x21, R::Rdx, R::Rax);
                a.imul64_rri(R::Rdx, R::Rdx, NODE_SIZE);
                a.load(R::Rax, tb, OFF_T_NODE);
                a.add_rr(R::Rax, R::Rdx);
                a.bind(lp);
                a.cmp_rm(kr, R::Rax, OFF_NODE_KEY);
                a.jcc(Cond::NE, next);
                a.load32(R::Rcx, R::Rax, OFF_NODE_KEYTT);
                a.and32_ri(R::Rcx, 0xf);
                a.cmp32_ri(R::Rcx, LUA_TSTRING);
                a.jcc(Cond::E, found);
                a.bind(next);
                a.load32(R::Rcx, R::Rax, OFF_NODE_KEYTT);
                a.shift32_ri(7, R::Rcx, 4);
                a.jcc(Cond::E, missing);
                a.movsxd(R::Rcx, R::Rcx);
                a.imul64_rri(R::Rcx, R::Rcx, NODE_SIZE);
                a.add_rr(R::Rax, R::Rcx);
                a.jmp(lp);
                a.bind(missing);
                a.mov_ri64(R::Rax, luaO_nilobject() as u64);
                a.bind(found);
                if let Home::G(d) = dst {
                    a.mov_rr(d, R::Rax);
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
            Op::MakeClosure { cl, site, vals, nvals, buf } => {
                let caps = &(*(*site as *const crate::jit::recorder::ClosureSite)).caps;
                for c in caps {
                    let v = vals[c.idx as usize];
                    if c.kind as u32 == crate::lvmexecute::LCT_REF {
                        // (the stack slot the upvalue opens on)
                        store_value(&mut a, &home, t, v, R::R13, c.slot as i32 * TV)?;
                    } else if c.kind as u32 == crate::lvmexecute::LCT_VAL {
                        a.mov_ri64(R::Rdx, *buf as u64);
                        store_value(&mut a, &home, t, v, R::Rdx, c.idx as i32 * TV)?;
                    }
                }
                let _ = nvals;
                let live = live_across(i);
                call_out(&mut a, &live, helpers::make_closure as usize, &[Arg::L, Arg::G(g(*cl)?), Arg::Imm(*site as u64), Arg::Imm(*buf as u64)]);
                if let Home::G(d) = dst {
                    a.mov_rr(d, R::Rax);
                }
            }
            Op::GuardTop { top, snap } => {
                a.load(R::Rax, R::R12, OFF_TOP);
                a.sub_rr(R::Rax, R::R13);
                a.cmp_ri(R::Rax, *top as i32 * TV);
                a.jcc(Cond::NE, stub(*snap));
            }
            Op::CloseUpvals { slot, snap } => {
                // only when an open upvalue is at or above the slot (the
                // list is sorted by level, highest first)
                let skip = a.label();
                a.load(R::Rax, R::R12, OFF_OPENUPVAL);
                a.test_rr(R::Rax, R::Rax);
                a.jcc(Cond::E, skip);
                a.load(R::Rcx, R::Rax, OFF_UV_V);
                a.lea(R::Rdx, R::R13, *slot as i32 * TV);
                a.cmp_rr(R::Rcx, R::Rdx);
                a.jcc(Cond::B, skip);
                for e in &t.snapshots[snap.0 as usize].entries {
                    store_value(&mut a, &home, t, e.value, R::R13, e.slot as i32 * TV)?;
                }
                let live = live_across(i);
                call_out(&mut a, &live, helpers::close_upvals as usize, &[Arg::L, Arg::Imm(*slot as u64)]);
                a.bind(skip);
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
            Op::UPtr { cl, idx, stack, snap } => {
                let s = stub(*snap);
                let disp = OFF_CL_UPREFS + *idx as i32 * TV;
                a.mov_rr(R::Rcx, g(*cl)?);
                a.cmp32_mi(R::Rcx, disp + OFF_TT, LUA_TUPVAL as i32);
                a.jcc(Cond::NE, s);
                a.load(R::Rcx, R::Rcx, disp);
                a.load(R::Rcx, R::Rcx, OFF_UV_V);
                // not open on a slot the trace may hold in a register
                let ok = a.label();
                a.cmp_rr(R::Rcx, R::R13);
                a.jcc(Cond::B, ok);
                a.lea(R::Rax, R::R13, *stack as i32 * TV);
                a.cmp_rr(R::Rcx, R::Rax);
                a.jcc(Cond::B, s);
                a.bind(ok);
                if let Home::G(d) = dst {
                    a.mov_rr(d, R::Rcx);
                }
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
            Op::TruncI32(x) => {
                if let Home::G(d) = dst {
                    a.cvttsd2si_x(d, f(*x)?);
                }
            }
            Op::I32ToNum(x) => {
                if let Home::F(d) = dst {
                    // (cleared first: see I64ToNum)
                    a.xorps(d, d);
                    a.cvtsi2sd(d, g(*x)?);
                }
            }
            Op::AndI32(x, y) | Op::AddI32(x, y) if imm_only[x.index()] || imm_only[y.index()] => {
                // (a constant operand: an immediate)
                if let Home::G(d) = dst {
                    let (v, k) = if imm_only[y.index()] { (*x, *y) } else { (*y, *x) };
                    let Op::KI32(c) = t.inst(k).op else { return Err("immediate of a non-constant".into()) };
                    if d != g(v)? {
                        a.mov_rr32(d, g(v)?);
                    }
                    a.alu32_ri(if matches!(inst.op, Op::AndI32(..)) { 4 } else { 0 }, d, c);
                }
            }
            Op::AndI32(x, y) => {
                if let Home::G(d) = dst {
                    let (x, y) = (g(*x)?, g(*y)?);
                    if d == y {
                        a.alu32_rr(0x21, d, x);
                    } else {
                        a.mov_rr32(d, x);
                        a.alu32_rr(0x21, d, y);
                    }
                }
            }
            Op::AddI32(x, y) => {
                if let Home::G(d) = dst {
                    let (x, y) = (g(*x)?, g(*y)?);
                    if d == y {
                        a.alu32_rr(0x01, d, x);
                    } else {
                        if d != x {
                            a.mov_rr32(d, x);
                        }
                        a.alu32_rr(0x01, d, y);
                    }
                }
            }
            Op::ASize(tb) => {
                if let Home::G(d) = dst {
                    a.load32(d, g(*tb)?, OFF_T_SIZEARRAY);
                }
            }
            Op::ARef { t: tb, idx } => {
                if let Home::G(d) = dst {
                    let is_base = t.ty(*tb) == IrType::Ptr;
                    let (tb, idx) = (g(*tb)?, g(*idx)?);
                    if is_base && d != tb {
                        a.mov_rr32(d, idx);
                        a.shl_ri(d, 4);
                        a.add_rr(d, tb);
                    } else {
                        a.mov_rr32(R::Rax, idx);
                        a.shl_ri(R::Rax, 4);
                        if is_base {
                            a.add_rr(R::Rax, tb);
                        } else {
                            a.load(R::Rcx, tb, OFF_T_ARRAY);
                            a.add_rr(R::Rax, R::Rcx);
                        }
                        a.mov_rr(d, R::Rax);
                    }
                }
            }
            Op::ABase(tb) => {
                if let Home::G(d) = dst {
                    a.load(d, g(*tb)?, OFF_T_ARRAY);
                }
            }
            Op::Loop => {
                // (for the body re-entry)
                if body_entry_ok {
                    for &(v, s) in &saves {
                        match home[v] {
                            Home::G(r) => a.store(R::Rsp, spill_off(s), r),
                            Home::F(x) => a.movups_store(R::Rsp, spill_off(s), x),
                            _ => {}
                        }
                    }
                }
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
                if counting {
                    a.mov_ri64(R::Rax, crate::jit::debug::counter(t.tag, 1) as u64);
                    a.add32_mi(R::Rax, 0, 1);
                }
            }
            Op::Phi { .. } => {}
            Op::Sunk => {}
        }
        if let Some(s) = spill_to {
            match dst {
                Home::F(x) => a.movups_store(R::Rsp, spill_off(s), x),
                Home::G(r) => a.store(R::Rsp, spill_off(s), r),
                _ => {}
            }
        }
    }
    if end != End::Loop {
        // the end: the state goes to the stack (the last snapshot) and the
        // next trace runs from its re-entry: the root's (a side trace), or
        // another loop's through the link cell (which checks interrupts
        // and GC first: links may form a cycle without a back edge)
        if let End::Cell(_) = end {
            a.load(R::Rax, R::R12, OFF_GLOBAL);
            a.cmp64_mi(R::Rax, OFF_INTERRUPT, 0);
            a.jcc(Cond::NE, interrupt_stub);
            if allocates {
                a.load(R::Rcx, R::Rax, OFF_TOTALBYTES);
                a.cmp_rm(R::Rcx, R::Rax, OFF_GCTHRESHOLD);
                a.jcc(Cond::AE, interrupt_stub);
            }
        }
        let sn = &t.snapshots[back_snap.0 as usize];
        materialize(&mut a, &home, t, back_snap.0, sn)?;
        // (into the root's body with the PHIs' values in registers: when
        // every PHI's slot has a value here of its type, in a register, a
        // spill slot, or a number constant)
        let mut end = end;
        if let End::RootRegs(regs, body) = end {
            let mut moves: Vec<(Home, Home)> = Vec::new();
            let mut consts: Vec<(Home, u64)> = Vec::new();
            let mut ok = !t.regs_entry.is_empty();
            for &(slot, kind, idx, ty) in t.regs_entry.iter() {
                let dst = match kind {
                    0 => Home::G(gpr_of(idx).ok_or("phi register")?),
                    1 => Home::F(X(idx)),
                    _ => Home::S(idx),
                };
                let Some(e) = sn.entries.iter().find(|e| e.slot == slot) else {
                    ok = false;
                    break;
                };
                if t.ty(e.value) != ty {
                    ok = false;
                    break;
                }
                match (home[e.value.index()], &t.inst(e.value).op) {
                    (Home::G(_) | Home::F(_) | Home::S(_), _) => moves.push((dst, home[e.value.index()])),
                    (Home::None, Op::KNum(n)) if ty == IrType::Number => consts.push((dst, n.to_bits())),
                    _ => {
                        ok = false;
                        break;
                    }
                }
                // (a gpr home and an xmm value, or the other way: no)
                if let Some(&(d, s)) = moves.last() {
                    if matches!((d, s), (Home::G(_), Home::F(_)) | (Home::F(_), Home::G(_))) {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                let rest: Vec<crate::jit::snapshots::SnapEntry> = sn.entries.iter().filter(|e| !t.regs_entry.iter().any(|p| p.0 == e.slot)).cloned().collect();
                store_entries(&mut a, &home, t, &rest)?;
                parallel_move(&mut a, &moves)?;
                for (d, bits) in consts {
                    a.mov_ri64(R::Rax, bits);
                    match d {
                        Home::F(x) => a.movq_xr(x, R::Rax),
                        Home::S(s) => a.store(R::Rsp, spill_off(s), R::Rax),
                        Home::G(r) => a.mov_rr(r, R::Rax),
                        Home::None => {}
                    }
                }
                a.mov_ri64(R::Rax, regs as u64);
                a.jmp_r(R::Rax);
            }
            end = if ok { End::RootRegs(regs, body) } else { End::Root(body) };
        }
        if !matches!(end, End::RootRegs(..)) {
            store_entries(&mut a, &home, t, &sn.entries)?;
        }
        if let Some(pf) = t.push {
            // up-recursion: the call's frame, as the interpreter builds it
            // (no CallInfo or stack room: exit before the call)
            let fail = stubs[back_snap.0 as usize];
            // (the function's trace gone: exit before the call)
            if let End::Cell(cell) = end {
                a.mov_ri64(R::Rax, cell as u64);
                a.load(R::Rax, R::Rax, 0);
                a.test_rr(R::Rax, R::Rax);
                a.jcc(Cond::E, fail);
            }
            a.load(R::Rcx, R::R12, OFF_CI);
            a.cmp_rm(R::Rcx, R::R12, OFF_END_CI);
            a.jcc(Cond::E, fail);
            a.lea(R::R8, R::R13, (pf.ra as i32 + 1 + pf.nargs as i32) * TV);
            a.load(R::R9, R::R12, OFF_STACK_LAST);
            a.sub_rr(R::R9, R::R8);
            a.cmp_ri(R::R9, pf.stacksize as i32 * TV);
            a.jcc(Cond::LE, fail);
            a.mov_ri64(R::R11, pf.ret as u64);
            a.store(R::Rcx, OFF_CI_SAVEDPC, R::R11);
            // (the callee's proto: this frame's)
            a.load(R::Rdx, R::Rcx, OFF_CI_P);
            a.add_ri(R::Rcx, SIZE_CI);
            a.store(R::R12, OFF_CI, R::Rcx);
            a.lea(R::R9, R::R13, pf.ra as i32 * TV);
            a.store(R::Rcx, OFF_CI_FUNC, R::R9);
            a.store(R::Rcx, OFF_CI_P, R::Rdx);
            a.add_ri(R::R9, TV);
            a.store(R::Rcx, OFF_CI_BASE, R::R9);
            a.lea(R::R10, R::R8, pf.stacksize as i32 * TV);
            a.store(R::Rcx, OFF_CI_TOP, R::R10);
            a.store64_i(R::Rcx, OFF_CI_SAVEDPC, 0);
            a.store32_i(R::Rcx, OFF_CI_FLAGS, 0);
            a.store32_i(R::Rcx, OFF_CI_NRESULTS, pf.nres as u32);
            a.store(R::R12, OFF_BASE, R::R9);
            for k in pf.nargs..pf.numparams {
                a.store64_i(R::R9, k as i32 * TV + OFF_TAGWORD, 0);
            }
            a.store(R::R12, OFF_TOP, R::R10);
            a.mov_rr(R::R13, R::R9);
        }
        match end {
            End::RootRegs(..) => {}
            End::Root(root) => {
                a.mov_ri64(R::Rax, root as u64);
                a.jmp_r(R::Rax);
            }
            End::Cell(cell) => {
                // (no trace there now: exit there)
                a.mov_ri64(R::Rax, cell as u64);
                a.load(R::Rax, R::Rax, 0);
                a.test_rr(R::Rax, R::Rax);
                a.jcc(Cond::E, stubs[back_snap.0 as usize]);
                a.jmp_r(R::Rax);
            }
            End::Loop => unreachable!(),
        }
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

    let body_entry = if body_entry_ok {
        let at = a.pos() as usize;
        for &(v, s) in &saves {
            match home[v] {
                Home::G(r) => a.load(r, R::Rsp, spill_off(s)),
                Home::F(x) => a.movups_load(x, R::Rsp, spill_off(s)),
                _ => {}
            }
        }
        for &(j, slot, twin) in &phi_loads {
            let ty = t.insts[j].ty;
            let disp = slot as i32 * TV;
            if twin && ty == IrType::I64 {
                // the number, converted as on entry (exactly, in bounds, or
                // the interpreter runs the iteration)
                let Op::Phi { pre, .. } = t.insts[j].op else { return Err("I64 PHI".into()) };
                let Op::NumToI64 { lim, shift, .. } = t.inst(pre).op else { return Err("I64 PHI entry".into()) };
                a.cmp32_mi(R::R13, disp + OFF_TT, LUA_TNUMBER);
                a.jcc(Cond::NE, stubs[0]);
                a.movsd_load(X(1), R::R13, disp);
                num_to_i64(&mut a, R::Rax, X(1), lim, shift, stubs[0]);
                match home[j] {
                    Home::G(r) => a.mov_rr(r, R::Rax),
                    Home::S(s2) => a.store(R::Rsp, spill_off(s2), R::Rax),
                    _ => {}
                }
                continue;
            }
            if twin {
                // the counter's number, converted (exactly, or exit)
                a.cmp32_mi(R::R13, disp + OFF_TT, LUA_TNUMBER);
                a.jcc(Cond::NE, stubs[0]);
                a.movsd_load(X(0), R::R13, disp);
                a.cvttsd2si_x(R::Rax, X(0));
                a.cvtsi2sd(X(1), R::Rax);
                a.ucomisd(X(1), X(0));
                a.jcc(Cond::NE, stubs[0]);
                a.jcc(Cond::P, stubs[0]);
                match home[j] {
                    Home::G(r) => a.mov_rr32(r, R::Rax),
                    Home::S(s2) => a.store(R::Rsp, spill_off(s2), R::Rax),
                    _ => {}
                }
                continue;
            }
            a.cmp32_mi(R::R13, disp + OFF_TT, ty.tag().unwrap());
            // (another type: the interpreter runs the iteration)
            a.jcc(Cond::NE, stubs[0]);
            match home[j] {
                Home::G(_) | Home::F(_) => load_value(&mut a, home[j], ty, R::R13, disp)?,
                Home::S(s) => {
                    let tmp = if needs_reg(ty) == Some(true) { Home::F(TEMP_X[0]) } else { Home::G(TEMP_G[0]) };
                    load_value(&mut a, tmp, ty, R::R13, disp)?;
                    match tmp {
                        Home::F(x) => a.movups_store(R::Rsp, spill_off(s), x),
                        Home::G(r) => a.store(R::Rsp, spill_off(s), r),
                        _ => {}
                    }
                }
                Home::None => {}
            }
        }
        a.jmp(loop_top);
        Some(at)
    } else {
        None
    };
    // the same, with the PHIs' values already in their homes (a side
    // trace moves them there): only the pre-loop values come back
    let mut phi_homes: Vec<(u16, u8, u8, IrType)> = Vec::new();
    let body_regs = if body_entry.is_some() && !crate::trace_flag!("LUAU_TRACE_NOREGENTRY") {
        let mut ok = true;
        for &(j, slot, twin) in &phi_loads {
            let ty = t.insts[j].ty;
            match home[j] {
                _ if twin => ok = false,
                Home::G(r) => phi_homes.push((slot, 0, r as u8, ty)),
                Home::F(x) => phi_homes.push((slot, 1, x.0, ty)),
                Home::S(s) => phi_homes.push((slot, 2, s, ty)),
                Home::None => {}
            }
        }
        if ok && !phi_homes.is_empty() {
            let at = a.pos() as usize;
            for &(v, s) in &saves {
                match home[v] {
                    Home::G(r) => a.load(r, R::Rsp, spill_off(s)),
                    Home::F(x) => a.movups_load(x, R::Rsp, spill_off(s)),
                    _ => {}
                }
            }
            a.jmp(loop_top);
            Some(at)
        } else {
            phi_homes.clear();
            None
        }
    } else {
        None
    };

    // exit stubs: write the snapshot's slots back, return its index
    // exit codes: (tag << 16) | snapshot; the high bit: interrupted at the
    // loop header
    let mut snaps: Vec<&Snapshot> = t.snapshots.iter().collect();
    // (a loop resumes at its header; a link where it was going)
    let interrupt_snap = if end == End::Loop { Snapshot { pc: t.start_pc, ..t.snapshots[back_snap.0 as usize].clone() } } else { t.snapshots[back_snap.0 as usize].clone() };
    if !matches!(end, End::Root(_) | End::RootRegs(..)) {
        snaps.push(&interrupt_snap);
    }
    if t.snapshots.len() > 0xffff {
        return Err("too many snapshots".into());
    }
    let tag = (t.tag as u32 & 0x7fff) << 16;
    a.bind(left_vm);
    a.mov_ri(R::Rax, crate::jit::trace::EXIT_LEFT_VM as u64);
    a.jmp(exit);
    for (k, sn) in snaps.iter().enumerate() {
        a.bind(stubs[k]);
        // (the interrupt exit stores the loop-close snapshot)
        let sid = if k == t.snapshots.len() { back_snap.0 } else { k as u32 };
        materialize(&mut a, &home, t, sid, sn)?;
        // (a side trace attached here stores the values held in registers
        // itself: see `jit::takeover`; the predicate is `exit_regs`')
        let sinks_here = t.sinks.iter().any(|sk| sk.states.iter().any(|st| st.snap == k as u32)) || sn.entries.iter().any(|e| matches!(t.inst(e.value).op, Op::Sunk));
        let in_reg = |e: &crate::jit::snapshots::SnapEntry| -> bool {
            let ty = t.ty(e.value);
            ty.tag().is_some()
                && match home.get(e.value.index()) {
                    Some(Home::F(_)) => ty == IrType::Number,
                    Some(Home::G(_)) => needs_reg(ty) == Some(false),
                    _ => false,
                }
        };
        let split = k < t.snapshots.len() && t.link_table != 0 && !sinks_here && crate::jit::takeover();
        if split {
            let rest: Vec<crate::jit::snapshots::SnapEntry> = sn.entries.iter().filter(|e| !in_reg(e)).cloned().collect();
            store_entries(&mut a, &home, t, &rest)?;
            let none = a.label();
            a.mov_ri64(R::Rax, (t.link_table + k * 8) as u64);
            a.load(R::Rax, R::Rax, 0);
            a.test_rr(R::Rax, R::Rax);
            a.jcc(Cond::E, none);
            a.jmp_r(R::Rax);
            a.bind(none);
            let regs: Vec<crate::jit::snapshots::SnapEntry> = sn.entries.iter().filter(|e| in_reg(e)).cloned().collect();
            store_entries(&mut a, &home, t, &regs)?;
            a.mov_ri(R::Rax, (tag | k as u32) as u64);
            a.jmp(exit);
            continue;
        }
        store_entries(&mut a, &home, t, &sn.entries)?;
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
    // per exit: the values it holds in registers (not where it rebuilds
    // sunk tables: those calls clobber them)
    let exit_regs: Vec<Vec<(u16, u8, u8)>> = t
        .snapshots
        .iter()
        .enumerate()
        .map(|(k, sn)| {
            if t.sinks.iter().any(|sk| sk.states.iter().any(|st| st.snap == k as u32)) || sn.entries.iter().any(|e| matches!(t.inst(e.value).op, Op::Sunk)) {
                return Vec::new();
            }
            sn.entries
                .iter()
                .filter_map(|e| {
                    let ty = t.ty(e.value);
                    ty.tag()?;
                    match home.get(e.value.index())? {
                        Home::F(x) if ty == IrType::Number => Some((e.slot, 1, x.0)),
                        Home::G(r) if needs_reg(ty) == Some(false) => Some((e.slot, 0, *r as u8)),
                        _ => None,
                    }
                })
                .collect()
        })
        .collect();
    Ok(Code { bytes: a.code, reentry, body_entry, body_regs, phi_homes: Box::leak(phi_homes.into_boxed_slice()), reserved: reserved as u32, exit_regs })
}

/// 1/v when v is a power of two whose reciprocal is a normal double.
fn exact_reciprocal(v: f64) -> Option<f64> {
    let b = v.to_bits();
    let exp = (b >> 52) & 0x7ff;
    if b & ((1u64 << 52) - 1) == 0 && (1..=2045).contains(&exp) {
        Some(f64::from_bits((b & (1u64 << 63)) | ((2046 - exp) << 52)))
    } else {
        None
    }
}

/// The CPU has AVX (three-operand scalar arithmetic: no copies).
fn avx() -> bool {
    static AVX: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVX.get_or_init(|| std::arch::is_x86_feature_detected!("avx") && !crate::trace_flag!("LUAU_TRACE_NOAVX"))
}

/// d = x op y (scalar double), without clobbering x or y.
fn binop(a: &mut Asm, opc: u8, d: X, x: X, y: X) {
    if avx() {
        a.vsd_rr(opc, d, x, y);
    } else if d == x {
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
        (Home::F(x), IrType::Vector) => a.movups_load(x, base, disp),
        (Home::G(r), IrType::Boolean) => a.load32(r, base, disp),
        (Home::G(r), _) => a.load(r, base, disp),
        (h, ty) => return Err(format!("can't load {:?} into {:?}", ty, h)),
    }
    Ok(())
}

/// Stores IR value `v` as a full TValue (payload + tag) at [base + disp].
fn store_value(a: &mut Asm, home: &[Home], t: &Trace, v: Ref, base: R, disp: i32) -> Result<(), String> {
    store_value_tag(a, home, t, v, base, disp, true)
}

/// A snapshot's stores into the stack (sunk tables apart: built before),
/// with each tag word loaded once for a run of same-typed values.
fn store_entries(a: &mut Asm, home: &[Home], t: &Trace, entries: &[crate::jit::snapshots::SnapEntry]) -> Result<(), String> {
    let mut rax: Option<u64> = None;
    for e in entries {
        if matches!(t.inst(e.value).op, Op::Sunk) {
            continue;
        }
        let ty = t.ty(e.value);
        let disp = e.slot as i32 * TV;
        let plain = matches!(home[e.value.index()], Home::F(_) | Home::G(_)) && ty != IrType::Vector && !crate::trace_flag!("LUAU_TRACE_NOTAGRUN");
        if !plain {
            store_value(a, home, t, e.value, R::R13, disp)?;
            rax = None;
            continue;
        }
        store_value_tag(a, home, t, e.value, R::R13, disp, false)?;
        let tag = ty.tag().ok_or_else(|| format!("store of a non-Lua value {:?}", ty))?;
        let extra = if ty == IrType::LightUserdata { LU_TAG_ITERATOR as u32 as u64 } else { 0 };
        let w = ((tag as u32 as u64) << 32) | extra;
        if rax != Some(w) {
            a.mov_ri64(R::Rax, w);
            rax = Some(w);
        }
        a.store(R::R13, disp + OFF_TAGWORD, R::Rax);
    }
    Ok(())
}

/// `store_value`; without the tag word when `tag` is false (the slot holds
/// the value's type already).
fn store_value_tag(a: &mut Asm, home: &[Home], t: &Trace, v: Ref, base: R, disp: i32, tag_word: bool) -> Result<(), String> {
    let ty = t.ty(v);
    let tag = ty.tag().ok_or_else(|| format!("store of a non-Lua value {:?}", ty))?;
    if ty == IrType::Vector {
        // x, y, z (z in the extra word), then the tag over lane 3
        match home[v.index()] {
            Home::F(x) => a.movups_store(base, disp, x),
            Home::S(s) => {
                a.movups_load(X(0), R::Rsp, spill_off(s));
                a.movups_store(base, disp, X(0));
            }
            h => return Err(format!("vector {} has no home {:?}", v.0, h)),
        }
        a.store32_i(base, disp + OFF_TT, tag as u32);
        return Ok(());
    }
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
        (Home::None, Op::Sunk) => return Err(format!("sunk table {} stored outside an exit", v.0)),
        (Home::None, Op::I32ToNum(x)) if matches!((home[x.index()], &t.inst(*x).op), (Home::None, Op::AddI32(..))) => {
            // (an integer + constant, then the conversion: only here)
            let Op::AddI32(y, k) = t.inst(*x).op else { unreachable!() };
            let Op::KI32(c) = t.inst(k).op else { return Err("deferred add of a non-constant".into()) };
            match home[y.index()] {
                Home::G(r) => a.mov_rr32(R::Rax, r),
                Home::S(s) => a.load(R::Rax, R::Rsp, spill_off(s)),
                h => return Err(format!("added value {} has no home {:?}", y.0, h)),
            }
            a.alu32_ri(0, R::Rax, c);
            a.cvtsi2sd(X(0), R::Rax);
            a.movsd_store(base, disp, X(0));
        }
        (Home::None, Op::I64ToNum(x, sh)) => {
            // (a conversion done only where it is stored)
            match home[x.index()] {
                Home::G(r) => a.cvtsi2sd64(X(0), r),
                Home::S(s) => {
                    a.load(R::Rax, R::Rsp, spill_off(s));
                    a.cvtsi2sd64(X(0), R::Rax);
                }
                h => return Err(format!("converted value {} has no home {:?}", x.0, h)),
            }
            scale_down(a, X(0), *sh);
            a.movsd_store(base, disp, X(0));
        }
        (Home::None, Op::I32ToNum(x)) => {
            // (a conversion done only where it is stored)
            match home[x.index()] {
                Home::G(r) => a.cvtsi2sd(X(0), r),
                Home::S(s) => {
                    a.load(R::Rax, R::Rsp, spill_off(s));
                    a.cvtsi2sd(X(0), R::Rax);
                }
                h => return Err(format!("converted value {} has no home {:?}", x.0, h)),
            }
            a.movsd_store(base, disp, X(0));
        }
        (Home::None, _) if ty == IrType::Nil => {}
        (h, op) => return Err(format!("value {} ({:?}) has no home {:?}", v.0, op, h)),
    }
    if !tag_word {
        return Ok(());
    }
    // (extra, tt) as one 8-byte word: extra = 0 (an iterator index: its
    // tag, the only light userdata traces handle)
    let extra = if ty == IrType::LightUserdata { LU_TAG_ITERATOR as u32 as u64 } else { 0 };
    a.mov_ri64(R::Rax, ((tag as u32 as u64) << 32) | extra);
    a.store(base, disp + OFF_TAGWORD, R::Rax);
    Ok(())
}

/// Exit code: builds the sunk tables snapshot `sn` (number `sid`) holds,
/// stores each into its slots (the caller stores the other entries).
fn materialize(a: &mut Asm, home: &[Home], t: &Trace, sid: u32, sn: &Snapshot) -> Result<(), String> {
    let mut done: Vec<Ref> = Vec::new();
    for e in &sn.entries {
        if !matches!(t.inst(e.value).op, Op::Sunk) || done.contains(&e.value) {
            continue;
        }
        done.push(e.value);
        let sk = t.sinks.iter().find(|s| s.at == e.value).ok_or("sunk table without a description")?;
        let st = sk.states.iter().find(|s| s.snap == sid).ok_or("sunk table without a state for this exit")?;
        // registers the rest of the exit still needs survive the call
        let mut live: Vec<Home> = Vec::new();
        for v in sn.entries.iter().map(|e| e.value).chain(st.fields.iter().map(|f| f.1)).chain(st.meta).chain(sk.entry.map(|e| e.0)) {
            let h = home[v.index()];
            if matches!(h, Home::G(_) | Home::F(_)) && !live.contains(&h) {
                live.push(h);
            }
        }
        for s2 in &t.sinks {
            if let Some(o) = s2.states.iter().find(|s| s.snap == sid) {
                for v in o.fields.iter().map(|f| f.1).chain(o.meta) {
                    let h = home[v.index()];
                    if matches!(h, Home::G(_) | Home::F(_)) && !live.contains(&h) {
                        live.push(h);
                    }
                }
            }
        }
        // (a table carried around the loop: in its first iteration it is
        // still the entry table)
        let done = a.label();
        if let Some((x, flag)) = sk.entry {
            let build = a.label();
            match (home[flag.index()], &t.inst(flag).op) {
                (Home::G(r), _) => a.test_rr(r, r),
                (Home::S(s), _) => {
                    a.load(R::Rax, R::Rsp, spill_off(s));
                    a.test_rr(R::Rax, R::Rax);
                }
                (h, _) => return Err(format!("first-iteration flag {} has no home {:?}", flag.0, h)),
            }
            a.jcc(Cond::E, build);
            for e2 in sn.entries.iter().filter(|x| x.value == e.value) {
                store_value(a, home, t, x, R::R13, e2.slot as i32 * TV)?;
            }
            a.jmp(done);
            a.bind(build);
        }
        if sk.tpl != 0 {
            call_out(a, &live, helpers::dup_table as usize, &[Arg::L, Arg::Imm(sk.tpl as u64)]);
        } else {
            call_out(a, &live, helpers::new_table as usize, &[Arg::L, Arg::Imm(sk.narray as u64), Arg::Imm(sk.nhash as u64)]);
        }
        a.mov_rr(R::R11, R::Rax);
        for &(node, v) in &st.fields {
            a.load(R::R10, R::R11, OFF_T_NODE);
            store_value(a, home, t, v, R::R10, node as i32 * NODE_SIZE)?;
        }
        if let Some(m) = st.meta {
            let r = match (home[m.index()], &t.inst(m).op) {
                (Home::G(r), _) => r,
                (Home::S(s), _) => {
                    a.load(R::Rax, R::Rsp, spill_off(s));
                    R::Rax
                }
                (Home::None, Op::KPtr(p)) => {
                    a.mov_ri64(R::Rax, *p as u64);
                    R::Rax
                }
                (h, _) => return Err(format!("metatable {} has no home {:?}", m.0, h)),
            };
            a.store(R::R11, OFF_T_METATABLE, r);
        }
        let tag = ((LUA_TTABLE as u32 as u64) << 32) as u64;
        for e2 in sn.entries.iter().filter(|x| x.value == e.value) {
            let disp = e2.slot as i32 * TV;
            a.store(R::R13, disp, R::R11);
            a.mov_ri64(R::Rax, tag);
            a.store(R::R13, disp + OFF_TAGWORD, R::Rax);
        }
        a.bind(done);
    }
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

/// The general register numbered `n`.
fn gpr_of(n: u8) -> Option<R> {
    [R::Rax, R::Rcx, R::Rdx, R::Rbx, R::Rsp, R::Rbp, R::Rsi, R::Rdi, R::R8, R::R9, R::R10, R::R11, R::R12, R::R13, R::R14, R::R15].get(n as usize).copied()
}

fn mov_home(a: &mut Asm, d: Home, s: Home) -> Result<(), String> {
    match (d, s) {
        (Home::F(d), Home::F(s)) => a.movapd(d, s),
        (Home::G(d), Home::G(s)) => a.mov_rr(d, s),
        (Home::F(d), Home::G(s)) => a.movq_xr(d, s),
        (Home::G(d), Home::F(s)) => a.movq_rx(d, s),
        (Home::G(d), Home::S(s)) => a.load(d, R::Rsp, spill_off(s)),
        (Home::F(d), Home::S(s)) => a.movups_load(d, R::Rsp, spill_off(s)),
        (Home::S(d), Home::G(s)) => a.store(R::Rsp, spill_off(d), s),
        (Home::S(d), Home::F(s)) => a.movups_store(R::Rsp, spill_off(d), s),
        (Home::S(d), Home::S(s)) => {
            a.movups_load(X(0), R::Rsp, spill_off(s));
            a.movups_store(R::Rsp, spill_off(d), X(0));
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
    /// a number in an xmm register (the k-th float argument: xmm0, xmm1)
    F(X),
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
    let size = 8 * gs.len() as i32 + 16 * xs.len() as i32;
    let frame = (size + 15) / 16 * 16 + 8;
    a.sub_ri(R::Rsp, frame);
    for (k, &r) in gs.iter().enumerate() {
        a.store(R::Rsp, 8 * k as i32, r);
    }
    for (k, &x) in xs.iter().enumerate() {
        a.movups_store(R::Rsp, 8 * gs.len() as i32 + 16 * k as i32, x);
    }
    const ARGS: [R; 6] = [R::Rdi, R::Rsi, R::Rdx, R::Rcx, R::R8, R::R9];
    // register arguments move all at once (a source can be another
    // argument's register: cycles go through rax); then arguments from the
    // save area, L and constants, which nothing else reads
    let mut moves: Vec<(R, R)> = Vec::new();
    for (k, arg) in args.iter().filter(|x| !matches!(x, Arg::F(_))).enumerate() {
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
    // float arguments (from allocated or temp xmm registers, intact)
    let mut nf = 0u8;
    for arg in args {
        if let Arg::F(x) = *arg {
            a.movapd(X(nf), x);
            nf += 1;
        }
    }
    let mut gi = 0usize;
    for arg in args.iter() {
        if matches!(arg, Arg::F(_)) {
            continue;
        }
        let d = ARGS[gi];
        gi += 1;
        match *arg {
            Arg::F(_) => {}
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
        a.movups_load(x, R::Rsp, 8 * gs.len() as i32 + 16 * k as i32);
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

    /// A leaf C function in slot `slot` of the trace's frame (L->ci): a
    /// normal (non-yieldable) call leaving `nres` results at the slot.
    /// Makes the inlined frames of a call site real CallInfos (so errors
    /// and tracebacks see them); returns the trace frame's CallInfo index.
    unsafe fn push_virtual(L: *mut lua_State, frames: *const Vec<crate::jit::snapshots::SnapFrame>) -> isize {
        let ci0 = (*L).ci.offset_from((*L).base_ci);
        if frames.is_null() {
            return ci0;
        }
        let base = (*L).base;
        for f in (*frames).iter() {
            let caller = (*L).ci;
            (*caller).savedpc = (*(*caller).p).code.add(f.ret_pc as usize);
            let func = base.add(f.func_slot());
            let cl = clvalue(func);
            let ci = crate::ldo::incr_ci(L);
            (*ci).func = func;
            (*ci).p = Closure::proto(cl);
            (*ci).base = base.add(f.base as usize);
            (*ci).top = (*ci).base.add(f.top_off((*cl).stacksize as usize));
            (*ci).savedpc = std::ptr::null();
            (*ci).flags = 0;
            (*ci).nresults = f.nresults;
        }
        (*L).base = (*(*L).ci).base;
        (*L).top = (*(*L).ci).top;
        ci0
    }

    unsafe fn pop_virtual(L: *mut lua_State, ci0: isize) {
        (*L).ci = (*L).base_ci.offset(ci0);
        (*L).base = (*(*L).ci).base;
        (*L).top = (*(*L).ci).top;
    }

    pub unsafe extern "sysv64" fn ccall(L: *mut lua_State, slot: i64, nargs: i64, nres: i64, savedpc: *const u32, frames: *const Vec<crate::jit::snapshots::SnapFrame>) {
        let ci0 = push_virtual(L, frames);
        ccall_here(L, slot, nargs, nres, savedpc);
        pop_virtual(L, ci0);
    }

    unsafe fn ccall_here(L: *mut lua_State, slot: i64, nargs: i64, nres: i64, savedpc: *const u32) {
        // the interpreter's direct C call (a leaf function never yields)
        (*(*L).ci).savedpc = savedpc;
        let ra = (*L).base.add(slot as usize);
        let argtop = ra.add(1 + nargs as usize);
        let ccl = clvalue(ra);
        let ci = crate::ldo::incr_ci(L);
        (*ci).func = ra;
        (*ci).p = std::ptr::null_mut();
        (*ci).base = ra.add(1);
        (*ci).top = argtop.add((*ccl).stacksize as usize);
        (*ci).savedpc = std::ptr::null();
        (*ci).flags = 0;
        (*ci).nresults = nres as i32;
        (*L).base = (*ci).base;
        (*L).top = argtop;
        crate::ldo::luaD_checkstackfornewci(L, (*ccl).stacksize as i32);
        let f = Closure::c(ccl).f.unwrap_unchecked();
        let n = f(L);
        debug_assert!(n >= 0, "a leaf C function yielded");
        let ci = (*L).ci;
        let cip = ci.sub(1);
        let mut res = (*ci).func;
        let mut vali = (*L).top.sub(n.max(0) as usize);
        let valend = (*L).top;
        let mut i = nres;
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
        (*L).top = (*cip).top;
    }

    /// Any C function in slot `slot` of the trace's frame, called as the
    /// interpreter's CALL does (inlined frames made real around it): the
    /// number of results; -1 when it yielded (the VM is left as the CALL
    /// leaves it: frames and all).
    pub unsafe extern "sysv64" fn callc(L: *mut lua_State, slot: i64, nargs: i64, nres: i64, savedpc: *const u32, frames: *const Vec<crate::jit::snapshots::SnapFrame>) -> i64 {
        let ci0 = push_virtual(L, frames);
        (*(*L).ci).savedpc = savedpc;
        let ra = (*L).base.add(slot as usize);
        let argtop = ra.add(1 + nargs as usize);
        let ccl = clvalue(ra);
        let ci = crate::ldo::incr_ci(L);
        (*ci).func = ra;
        (*ci).p = std::ptr::null_mut();
        (*ci).base = ra.add(1);
        (*ci).top = argtop.add((*ccl).stacksize as usize);
        (*ci).savedpc = std::ptr::null();
        (*ci).flags = 0;
        (*ci).nresults = nres as i32;
        (*L).base = (*ci).base;
        (*L).top = argtop;
        crate::ldo::luaD_checkstackfornewci(L, (*ccl).stacksize as i32);
        let f = Closure::c(ccl).f.unwrap_unchecked();
        let n = f(L);
        if n < 0 {
            return -1;
        }
        let ci = (*L).ci;
        let cip = ci.sub(1);
        let func = (*ci).func;
        let mut res = func;
        let mut vali = (*L).top.sub(n as usize);
        let valend = (*L).top;
        let mut i = nres;
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
        let count = res.offset_from(func) as i64;
        pop_virtual(L, ci0);
        if nres < 0 {
            // (the instruction taking every result reads L->top)
            (*L).top = res;
        }
        count
    }

    /// tostring of a number. `cache`: (number bits, GC_EPOCH, string),
    /// direct-mapped; an entry counts only in the epoch it was filled in
    /// (the string, alive then, stays alive until the next one: see
    /// `ConcatCache`).
    pub unsafe extern "sysv64" fn num_tostring(L: *mut lua_State, n: f64, cache: *mut [[u64; 3]; 256]) -> *mut TString {
        let bits = n.to_bits();
        let epoch = crate::lgc::GC_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
        let h = (bits.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize;
        let e = &mut (*cache)[h];
        if e[0] == bits && e[1] == epoch && e[2] != 0 {
            return e[2] as *mut TString;
        }
        let mut nb = crate::lnumutils::NumBuf::new();
        let s = crate::lnumutils::luai_num2buf(&mut nb, n);
        let r = crate::lstring::luaS_newlstr(L, s);
        *e = [bits, epoch, r as u64];
        r
    }

    pub extern "sysv64" fn pow(a: f64, b: f64) -> f64 {
        crate::lmathlib::luai_numpow(a, b)
    }

    /// Results of a two-operand CONCAT site: (operand values, their tags,
    /// the result string), direct-mapped. Owns nothing: an entry counts
    /// only while `GC_EPOCH` is the one it was filled in (the operand and
    /// result strings, alive then, are still alive and at the same
    /// addresses; numbers are keyed by their bits).
    #[repr(C)]
    pub struct ConcatCache {
        e: [[u64; 4]; 256],
    }

    impl ConcatCache {
        pub fn new() -> Self {
            ConcatCache { e: [[0; 4]; 256] }
        }
    }

    /// CONCAT on the trace frame's slots b..c into a.
    /// (`abc`: a | b << 16 | c << 32)
    pub unsafe extern "sysv64" fn concat(L: *mut lua_State, abc: u64, savedpc: *const u32, frames: *const Vec<crate::jit::snapshots::SnapFrame>, cache: *mut ConcatCache) {
        let (a, b, c) = ((abc & 0xffff) as i64, ((abc >> 16) & 0xffff) as i64, (abc >> 32) as i64);
        let mut slot = None;
        if !cache.is_null() {
            // (strings and numbers only: never metamethods or errors)
            let base = (*L).base;
            let (x, y) = (base.add(b as usize), base.add(c as usize));
            let sn = |tt: i32| tt == LUA_TSTRING || tt == LUA_TNUMBER;
            if sn((*x).tt) && sn((*y).tt) {
                let kx = std::ptr::read(x as *const u64);
                let ky = std::ptr::read(y as *const u64);
                let epoch = crate::lgc::GC_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
                let meta = (epoch << 16) | (((*x).tt as u64) << 8) | (*y).tt as u64;
                let h = ((kx ^ ky.rotate_left(29)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize;
                let e = &mut (*cache).e[h];
                if e[0] == kx && e[1] == ky && e[2] == meta {
                    setsvalue(base.add(a as usize), e[3] as *mut TString);
                    return;
                }
                slot = Some((h, kx, ky, meta));
            }
        }
        let ci0 = push_virtual(L, frames);
        (*(*L).ci).savedpc = savedpc;
        crate::lvmutils::luaV_concat(L, (c - b + 1) as i32, c as i32);
        let base = (*L).base;
        setobj(base.add(a as usize), base.add(b as usize));
        if let Some((h, kx, ky, meta)) = slot {
            // (the epoch read before: a collection during the concat
            // leaves an entry that never matches)
            (*cache).e[h] = [kx, ky, meta, tsvalue(base.add(b as usize)) as u64];
        }
        pop_virtual(L, ci0);
    }

    /// Stores buf[0] into upvalue `idx` of `cl`: 0 (nothing done) when it is
    /// open on the trace's frames.
    pub unsafe extern "sysv64" fn ustore(L: *mut lua_State, cl: *mut Closure, idx: i64, buf: *const TValue) -> i64 {
        let ur = Closure::uprefs(cl).add(idx as usize);
        if (*ur).tt != LUA_TUPVAL {
            return 0;
        }
        let uv = upvalue(ur);
        let v = (*uv).v;
        let base = (*L).base;
        if v >= base && v < base.add(1024) {
            return 0;
        }
        setobj(v, buf);
        crate::lgc::luaC_barrier(L, uv as *mut GCObject, buf);
        1
    }

    /// CLOSEUPVALS at trace slot `slot`.
    pub unsafe extern "sysv64" fn close_upvals(L: *mut lua_State, slot: i64) {
        crate::lfunc::luaF_close(L, (*L).base.add(slot as usize));
    }

    /// NEWCLOSURE / DUPCLOSURE as the interpreter does them.
    pub unsafe extern "sysv64" fn make_closure(L: *mut lua_State, cl: *mut Closure, site: *const crate::jit::recorder::ClosureSite, buf: *const TValue) -> *mut Closure {
        use crate::lfunc::luaF_newLclosure;
        let site = &*site;
        let src = |kind: u8, idx: u8| -> *const TValue {
            if kind as u32 == crate::lvmexecute::LCT_VAL {
                buf.add(idx as usize)
            } else {
                Closure::uprefs(cl).add(idx as usize)
            }
        };
        if !site.dup {
            let pv = site.target as *mut Proto;
            let ncl = luaF_newLclosure(L, (*pv).nups as i32, (*cl).env, pv);
            for (ui, c) in site.caps.iter().enumerate() {
                let dst = Closure::uprefs(ncl).add(ui);
                if c.kind as u32 == crate::lvmexecute::LCT_REF {
                    setupvalue(dst, crate::lfunc::luaF_findupval(L, (*L).base.add(c.slot as usize)));
                } else {
                    setobj(dst, src(c.kind, c.idx));
                }
            }
            return ncl;
        }
        let kcl = site.target as *mut Closure;
        let mut ncl = if (*kcl).env == (*cl).env { kcl } else { luaF_newLclosure(L, (*kcl).nupvalues as i32, (*cl).env, Closure::proto(kcl)) };
        let mut ui = 0usize;
        while ui < site.caps.len() {
            let (kind, idx) = (site.caps[ui].kind, site.caps[ui].idx);
            let uvp = src(kind, idx);
            let dst = Closure::uprefs(ncl).add(ui);
            if ncl == kcl && luaO_rawequalObj(dst, uvp) {
                ui += 1;
                continue;
            }
            if ncl == kcl && (*kcl).preload == 0 {
                ncl = luaF_newLclosure(L, (*kcl).nupvalues as i32, (*cl).env, Closure::proto(kcl));
                ui = 0;
                continue;
            }
            setobj(dst, uvp);
            crate::lgc::luaC_barrier(L, ncl as *mut GCObject, uvp);
            ui += 1;
        }
        (*ncl).preload = 0;
        ncl
    }

    pub unsafe extern "sysv64" fn tget(t: *mut LuaTable, key: *const TValue) -> *const TValue {
        crate::ltable::luaH_get(t, key)
    }

    /// Raw store; 0 (nothing done) for a key the interpreter must reject.
    pub unsafe extern "sysv64" fn tset(L: *mut lua_State, t: *mut LuaTable, buf: *const TValue) -> i64 {
        let key = buf;
        // (a read-only table or a bad key: the interpreter raises the error)
        if (*t).readonly != 0 || (*key).tt == LUA_TNIL || ((*key).tt == LUA_TNUMBER && (*key).value.n.is_nan()) {
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

/// Signed division magic for `k` (2 <= k < 2^31): (m, s) with
/// trunc(n / k) = ((mulhi(m, n) [+ n when m < 0]) >> s) + (n < 0), checked
/// on sample values (None: don't use it).
fn magic_s64(k: i64) -> Option<(i64, u32)> {
    if !(2..(1 << 31)).contains(&k) {
        return None;
    }
    // Hacker's Delight, figure 10-1 (64-bit)
    let two63: u64 = 1 << 63;
    let ad = k as u64;
    let tt = two63;
    let anc = tt - 1 - tt % ad;
    let mut p: u32 = 63;
    let mut q1 = two63 / anc;
    let mut r1 = two63 - q1 * anc;
    let mut q2 = two63 / ad;
    let mut r2 = two63 - q2 * ad;
    loop {
        p += 1;
        q1 = q1.wrapping_mul(2);
        r1 = r1.wrapping_mul(2);
        if r1 >= anc {
            q1 = q1.wrapping_add(1);
            r1 = r1.wrapping_sub(anc);
        }
        q2 = q2.wrapping_mul(2);
        r2 = r2.wrapping_mul(2);
        if r2 >= ad {
            q2 = q2.wrapping_add(1);
            r2 = r2.wrapping_sub(ad);
        }
        let delta = ad - r2;
        if !(q1 < delta || (q1 == delta && r1 == 0)) || p >= 127 {
            break;
        }
    }
    let m = q2.wrapping_add(1) as i64;
    let s = p - 64;
    let div = |n: i64| -> i64 {
        let mut q = ((m as i128 * n as i128) >> 64) as i64;
        if m < 0 {
            q = q.wrapping_add(n);
        }
        q >>= s;
        q + ((n as u64) >> 63) as i64
    };
    let lim: i64 = 1 << 52;
    let mut samples = vec![0, 1, -1, k, -k, k - 1, -(k - 1), k + 1, -(k + 1), lim - 1, -(lim - 1)];
    for j in 1..64i64 {
        let v = (lim - 1) / 64 * j + j * 7919;
        samples.extend([v, -v, v / k * k, v / k * k - 1, -(v / k * k), -(v / k * k) + 1]);
    }
    for n in samples {
        if n.abs() < lim && div(n) != n / k {
            return None;
        }
    }
    Some((m, s))
}

/// `d` = the number in `x` times 2^shift (exact: a power of two) as an
/// i64; jumps to `fail` unless it is exactly one (bit for bit: not -0, NaN,
/// out of range) with |d| < lim. Uses rcx, rdx, xmm0.
fn num_to_i64(a: &mut Asm, d: R, x: X, lim: i64, shift: u8, fail: Label) {
    let src = if shift > 0 {
        a.mov_ri64(R::Rcx, (2f64).powi(shift as i32).to_bits());
        a.movq_xr(X(0), R::Rcx);
        a.mulsd(X(0), x);
        X(0)
    } else {
        x
    };
    a.movq_rx(R::Rdx, src);
    a.cvttsd2si64_x(d, src);
    a.cvtsi2sd64(X(0), d);
    a.movq_rx(R::Rcx, X(0));
    a.cmp_rr(R::Rcx, R::Rdx);
    a.jcc(Cond::NE, fail);
    a.mov_ri64(R::Rcx, lim as u64);
    a.cmp_rr(d, R::Rcx);
    a.jcc(Cond::GE, fail);
    a.mov_ri64(R::Rcx, lim.wrapping_neg() as u64);
    a.cmp_rr(d, R::Rcx);
    a.jcc(Cond::LE, fail);
}

/// `x` *= 2^-shift (exact). Uses rcx and xmm1.
fn scale_down(a: &mut Asm, x: X, shift: u8) {
    if shift > 0 {
        a.mov_ri64(R::Rcx, (2f64).powi(-(shift as i32)).to_bits());
        a.movq_xr(X(1), R::Rcx);
        a.mulsd(x, X(1));
    }
}
