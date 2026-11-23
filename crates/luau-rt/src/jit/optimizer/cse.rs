//! Common-subexpression elimination for pure instructions.
//!
//! Two pure instructions with the same op, type and operands are the same
//! value.  The later one becomes a `Nop`, and uses are remapped to the first.
//! CSE is kept within one loop region (pre-roll vs body) so a `Phi back`
//! operand never becomes a pre-loop ref.

use super::super::ir::instruction::{Op, Ref};
use super::super::trace::Trace;
use super::util::{identity, loop_index, map_op, phi_back_refs, rewrite};
use std::collections::HashMap;

pub fn run(t: &mut Trace) -> bool {
    let loop_at = loop_index(t);
    let backs = phi_back_refs(t);
    let mut map = identity(t);
    let mut seen: HashMap<String, Ref> = HashMap::new();
    let mut changed = false;

    for i in 0..t.insts.len() {
        let inst = &t.insts[i];
        if inst.moves_tables() {
            // table storage may have moved: no address before is reused
            seen.retain(|k, _| !k.contains("ARef") && !k.contains("HSlot"));
            continue;
        }
        if !inst.is_pure() || matches!(inst.op, Op::Nop) {
            continue;
        }
        if backs.contains(&Ref(i as u32)) {
            continue;
        }
        let region = match loop_at {
            Some(l) if i > l => 1,
            _ => 0,
        };
        let op = map_op(&inst.op, &map);
        let key = format!("{region}|{:?}|{:?}", inst.ty, op);
        if let Some(&first) = seen.get(&key) {
            map[i] = first;
            t.insts[i].op = Op::Nop;
            changed = true;
        } else {
            seen.insert(key, Ref(i as u32));
        }
    }

    if changed {
        rewrite(t, &map);
    }
    changed
}
