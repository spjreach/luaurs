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
            Op::TMeta { .. } | Op::GuardPlainTable { .. } => !metas && !writes,
            Op::GuardFlag { .. } => !calls,
            _ => false,
        };
        if !ok || !inst.operands().iter().all(|o| pre[o.index()]) {
            continue;
        }
        pre[i] = true;
        moved.push(i);
    }
    if moved.is_empty() {
        return false;
    }
    for &i in &moved {
        set_snapshot(&mut t.insts[i].op, SnapId(0));
    }
    move_before_loop(t, &moved);
    true
}
