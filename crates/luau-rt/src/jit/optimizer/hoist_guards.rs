//! Loop-invariant checks and loads move before the loop.
//!
//! The recorder hoists invariant work when it closes the loop; later passes
//! (removing repeats, sinking allocations - which drops the stores that
//! blocked hoisting loads) leave more of it. Here, in trace order, a body
//! instruction whose operands are all defined before the loop moves there
//! (its exit becomes the entry snapshot: nothing done yet) when nothing in
//! the body can change what it reads:
//! - checks of values (`Guard*` on invariant operands), prototype checks,
//!   upvalues stored in closures, environments: always;
//! - hash slots (`HRef`), loads (`XLoad`), array sizes: when the body writes
//!   no table memory and moves no table storage;
//! - metatables: when the body calls no `setmetatable` / Lua code;
//! - the environment's safe flag: when the body calls no Lua code.

use crate::jit::ir::types::IrType;
use super::super::ir::instruction::{Op, Ref, SnapId};
use super::super::recorder::set_snapshot;
use super::super::trace::Trace;
use super::hoist_consts::move_before_loop;
use super::util::phi_back_refs;
use luau_common::bytecode::BuiltinFunction as B;

pub fn run(t: &mut Trace) -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| std::env::var_os("LUAU_TRACE_NOHOISTG").is_some()) {
        return false;
    }
    let Some(loop_ref) = t.loop_ref else { return false };
    let l = loop_ref.index();
    let n = t.insts.len();
    let body = l + 1..n;
    let writes = body.clone().any(|i| {
        let x = &t.insts[i];
        x.moves_tables()
            || matches!(x.op, Op::XStore { .. } | Op::TSet { .. })
            || matches!(x.op, Op::Builtin { bfid, .. } if bfid == B::Rawset as u8 || bfid == B::TableInsert as u8)
    });
    // what can move or resize table storage (add keys, grow arrays): value
    // stores can't
    let moves = body.clone().any(|i| {
        let x = &t.insts[i];
        x.moves_tables() || matches!(x.op, Op::TSet { .. }) || matches!(x.op, Op::Builtin { bfid, .. } if bfid == B::Rawset as u8 || bfid == B::TableInsert as u8)
    });
    let calls = body.clone().any(|i| matches!(t.insts[i].op, Op::CallC { .. }));
    let closes = body.clone().any(|i| matches!(t.insts[i].op, Op::CloseUpvals { .. }));
    let metas = calls || body.clone().any(|i| matches!(t.insts[i].op, Op::Builtin { bfid, .. } if bfid == B::Setmetatable as u8));
    let backs = phi_back_refs(t);
    let mut pre = vec![false; n];
    for p in pre.iter_mut().take(l) {
        *p = true;
    }
    let mut moved = Vec::new();
    for i in body {
        if backs.contains(&Ref(i as u32)) || matches!(t.insts[i].op, Op::Phi { .. } | Op::Nop) {
            continue;
        }
        let inst = &t.insts[i];
        let ok = match &inst.op {
            Op::GuardProto { .. } | Op::ULoad { by_ref: false, .. } | Op::ClEnv { .. } | Op::GuardCFunc { .. } => true,
            Op::Guard { .. } | Op::GuardTruthy { .. } | Op::GuardSame { .. } | Op::GuardDiff { .. } => true,
            // (a load can see a store in the loop; sizes, storage and node
            // addresses only change when storage moves)
            Op::XLoad { .. } => !writes,
            Op::HRef { .. } | Op::ASize(_) | Op::HSlot { .. } | Op::ABase(_) | Op::ARef { .. } => !moves,
            // (a key's absence: only a store can add it)
            Op::HMiss { .. } => !moves && !writes && !crate::trace_flag!("LUAU_TRACE_NOHMISSHOIST"),
            // (stores into tables don't change metatables or the read-only
            // flag: only setmetatable / calls - table.freeze is one)
            Op::TMeta { .. } | Op::GuardPlainTable { .. } => !metas,
            Op::GuardFlag { .. } => !calls,
            Op::UPtr { .. } => !calls && !closes,
            _ => false,
        };
        if !ok || !inst.operands().iter().all(|o| pre[o.index()]) {
            continue;
        }
        pre[i] = true;
        moved.push(i);
    }
    let mut changed = false;
    if !moved.is_empty() {
        for &i in &moved {
            set_snapshot(&mut t.insts[i].op, SnapId(0));
        }
        move_before_loop(t, &moved);
        changed = true;
    }
    // (passes after sinking must keep the sunk tables' states: these
    // don't, so not with any)
    let keys = !crate::trace_flag!("LUAU_TRACE_NOMEMKEYS") && t.sinks.is_empty();
    if changed && keys {
        // (hoisted copies of values computed before the loop already -
        // the same upvalue's table, its field addresses: one each)
        super::redundant::run(t);
        super::cse::run(t);
        super::dce::run(t);
    }
    if keys {
        return changed | mem_phis_fields(t);
    }
    changed | mem_phis(t)
}

/// What a pointer addresses, as far as the trace can tell: a node of a
/// table's hash part (the table's value, the node index) - two of these
/// alias exactly when both are equal (for the same table value) - or just
/// the pointer's value.
fn addr_key(t: &Trace, p: Ref) -> (Ref, u32) {
    match t.inst(p).op {
        Op::HSlot { t: tb, idx } | Op::HRef { t: tb, idx, .. } => (tb, idx as u32),
        _ => (p, u32::MAX),
    }
}

/// `mem_phis` for loops storing through several pointers from before the
/// loop: fields of one table (distinct nodes never alias), or one pointer.
/// Each loaded before its first store becomes a PHI.
fn mem_phis_fields(t: &mut Trace) -> bool {
    use super::super::ir::instruction::Inst;
    let Some(loop_ref) = t.loop_ref else { return false };
    let l = loop_ref.index();
    let n = t.insts.len();
    // per key: (a pre-loop pointer to it, the store positions)
    let mut targets: Vec<((Ref, u32), Ref, Vec<usize>)> = Vec::new();
    for i in l + 1..n {
        let x = &t.insts[i];
        match x.op {
            Op::XStore { p, .. } if p.index() < l => {
                let k = addr_key(t, p);
                match targets.iter_mut().find(|e| e.0 == k) {
                    Some(e) => e.2.push(i),
                    None => targets.push((k, p, vec![i])),
                }
            }
            Op::XStore { .. } | Op::TSet { .. } | Op::CallC { .. } | Op::UStore { .. } | Op::CloseUpvals { .. } | Op::CCall { .. } | Op::Concat { .. } => return false,
            _ if x.moves_tables() || matches!(x.op, Op::Builtin { bfid, .. } if bfid == B::Rawset as u8 || bfid == B::TableInsert as u8) => return false,
            _ => {}
        }
    }
    if targets.is_empty() {
        return false;
    }
    // (several: all nodes of one table, else they may alias)
    if targets.len() > 1 && (targets.iter().any(|e| e.0 .1 == u32::MAX) || targets.iter().any(|e| e.0 .0 != targets[0].0 .0)) {
        return false;
    }
    let backs = phi_back_refs(t);
    let mut plan: Vec<(Ref, Vec<usize>, Ref, IrType)> = Vec::new();
    for (k, p, stores) in &targets {
        let first = stores[0];
        let Op::XStore { v: last, .. } = t.insts[*stores.last().unwrap()].op else { continue };
        if last.index() <= l {
            continue;
        }
        let loads: Vec<usize> = (l + 1..first).filter(|&i| matches!(t.insts[i].op, Op::XLoad { p: q, .. } if addr_key(t, q) == *k) && !backs.contains(&Ref(i as u32))).collect();
        let Some(&ld) = loads.first() else { continue };
        let ty = t.insts[ld].ty;
        if t.ty(last) != ty || loads.iter().any(|&i| t.insts[i].ty != ty) {
            continue;
        }
        plan.push((*p, loads, last, ty));
    }
    if plan.is_empty() {
        return false;
    }
    let mut pres = Vec::new();
    let mut phis = Vec::new();
    let mut repl: Vec<(usize, usize)> = Vec::new();
    for (p, loads, last, ty) in &plan {
        t.insts.push(Inst { op: Op::XLoad { p: *p, snap: SnapId(0) }, ty: *ty });
        let pre_ld = t.insts.len() - 1;
        t.insts.push(Inst { op: Op::Phi { pre: Ref(pre_ld as u32), back: *last }, ty: *ty });
        let phi = t.insts.len() - 1;
        pres.push(pre_ld);
        phis.push(phi);
        for &i in loads {
            repl.push((i, phi));
        }
    }
    let mut full: Vec<Ref> = (0..t.insts.len() as u32).map(Ref).collect();
    for &(i, phi) in &repl {
        full[i] = Ref(phi as u32);
    }
    for (_, loads, _, _) in &plan {
        for &i in loads {
            t.insts[i].op = Op::Nop;
        }
    }
    super::util::rewrite(t, &full);
    let mut order: Vec<usize> = (0..l).collect();
    order.extend(pres.iter().copied());
    order.push(l);
    let body: Vec<usize> = (l + 1..n).collect();
    let lead = body.iter().take_while(|&&i| matches!(t.insts[i].op, Op::Phi { .. } | Op::Nop)).count();
    order.extend(body[..lead].iter().copied());
    order.extend(phis.iter().copied());
    order.extend(body[lead..].iter().copied());
    super::hoist_consts::reorder(t, &order);
    true
}

/// A loop whose only memory writes are stores through one pointer from
/// before the loop (an upvalue's, a field's): a load through it before
/// the first store becomes a PHI (loaded once before the loop, then the
/// value the iteration stored last) - the store stays, so memory is
/// current wherever the trace exits.
fn mem_phis(t: &mut Trace) -> bool {
    use super::super::ir::instruction::Inst;
    let Some(loop_ref) = t.loop_ref else { return false };
    let l = loop_ref.index();
    let n = t.insts.len();
    let mut target: Option<Ref> = None;
    let mut stores: Vec<usize> = Vec::new();
    for i in l + 1..n {
        let x = &t.insts[i];
        match x.op {
            Op::XStore { p, .. } if p.index() < l && target.is_none_or(|q| q == p) => {
                target = Some(p);
                stores.push(i);
            }
            Op::XStore { .. } | Op::TSet { .. } | Op::CallC { .. } | Op::UStore { .. } | Op::CloseUpvals { .. } | Op::CCall { .. } | Op::Concat { .. } => return false,
            _ if x.moves_tables() || matches!(x.op, Op::Builtin { bfid, .. } if bfid == B::Rawset as u8 || bfid == B::TableInsert as u8) => return false,
            _ => {}
        }
    }
    let Some(p) = target else { return false };
    let backs = phi_back_refs(t);
    let first_store = stores[0];
    let Op::XStore { v: last, .. } = t.insts[*stores.last().unwrap()].op else { return false };
    if last.index() <= l {
        return false;
    }
    let loads: Vec<usize> = (l + 1..first_store).filter(|&i| matches!(t.insts[i].op, Op::XLoad { p: q, .. } if q == p) && !backs.contains(&Ref(i as u32))).collect();
    let Some(&ld) = loads.first() else { return false };
    let ty = t.insts[ld].ty;
    if t.ty(last) != ty {
        return false;
    }
    // before the loop: the load; after the PHIs: the PHI
    t.insts.push(Inst { op: Op::XLoad { p, snap: SnapId(0) }, ty });
    let pre_ld = t.insts.len() - 1;
    t.insts.push(Inst { op: Op::Phi { pre: Ref(pre_ld as u32), back: last }, ty });
    let phi = t.insts.len() - 1;
    let mut map: Vec<Ref> = (0..t.insts.len() as u32).map(Ref).collect();
    for &i in &loads {
        map[i] = Ref(phi as u32);
        t.insts[i].op = Op::Nop;
    }
    super::util::rewrite(t, &map);
    let mut order: Vec<usize> = (0..l).collect();
    order.push(pre_ld);
    order.push(l);
    let body: Vec<usize> = (l + 1..n).collect();
    let lead = body.iter().take_while(|&&i| matches!(t.insts[i].op, Op::Phi { .. } | Op::Nop)).count();
    order.extend(body[..lead].iter().copied());
    order.push(phi);
    order.extend(body[lead..].iter().copied());
    super::hoist_consts::reorder(t, &order);
    true
}
