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
            Op::HRef { .. } | Op::XLoad { .. } | Op::ASize(_) | Op::HSlot { .. } | Op::ABase(_) => !writes,
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
    changed | mem_phis(t)
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
