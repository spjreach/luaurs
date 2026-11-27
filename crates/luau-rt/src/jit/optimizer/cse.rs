//! Common-subexpression elimination for pure instructions.
//!
//! Two pure instructions with the same op, type and operands are the same
//! value.  The later one becomes a `Nop`, and uses are remapped to the first.
//! CSE is kept within one loop region (pre-roll vs body) so a `Phi back`
//! operand never becomes a pre-loop ref.

use super::super::ir::instruction::{Inst, Op, Ref};
use super::super::trace::Trace;
use super::util::{identity, loop_index, map_op, phi_back_refs, rewrite};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// A pure instruction as a map key: equal when region, type and op are
/// (number constants bit for bit); hashed by region, type, the op's kind
/// and operands (equal ops have those equal: consistent).
struct Key(u8, Inst);

impl PartialEq for Key {
    fn eq(&self, o: &Key) -> bool {
        // (constants bit for bit: -0 and 0 differ, a NaN is itself)
        self.0 == o.0
            && self.1.ty == o.1.ty
            && match (&self.1.op, &o.1.op) {
                (Op::KNum(x), Op::KNum(y)) => x.to_bits() == y.to_bits(),
                (x, y) => x == y,
            }
    }
}

impl Eq for Key {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.0.hash(h);
        self.1.ty.hash(h);
        std::mem::discriminant(&self.1.op).hash(h);
        for o in self.1.operands() {
            o.hash(h);
        }
    }
}

pub fn run(t: &mut Trace) -> bool {
    let loop_at = loop_index(t);
    let backs = phi_back_refs(t);
    let mut map = identity(t);
    let mut seen: HashMap<Key, Ref> = HashMap::new();
    let mut changed = false;

    for i in 0..t.insts.len() {
        let inst = &t.insts[i];
        if inst.moves_tables() {
            // table storage may have moved: no address before is reused
            seen.retain(|k, _| !matches!(k.1.op, Op::ARef { .. } | Op::HSlot { .. } | Op::ABase(_)));
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
        let key = Key(region, Inst { op, ty: inst.ty });
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
