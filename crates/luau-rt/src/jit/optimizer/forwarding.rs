//! Load/store forwarding.
//!
//! Within one stretch of the trace (the pre-roll, or one loop iteration),
//! the values known to be in memory are tracked per pointer: an
//! `XStore { p, v }` makes `v` the content of `p`, an `XLoad { p }` makes its
//! own result the content. A later `XLoad` of a pointer with known content
//! of the same type is replaced by that value. A store forgets every pointer
//! it may alias:
//!
//! - different hash nodes (`HRef`/`HSlot` with different node indexes) never
//!   overlap, of the same table or not;
//! - hash nodes never overlap array elements (`ARef`);
//! - array elements at different constant indexes never overlap;
//! - anything else may.
//!
//! The `Loop` marker forgets everything (the body runs many times).

use super::super::ir::instruction::{Op, Ref};
use super::super::trace::Trace;
use super::util::{const_i32, identity, phi_back_refs, resolve, rewrite};

/// What a pointer points at, as far as aliasing goes.
#[derive(Clone, Copy, PartialEq)]
enum Mem {
    /// hash node `idx` (of some table)
    Node(u32),
    /// array element: (table, index, constant index if known)
    Elem(Ref, Ref, Option<i32>),
    Other,
}

fn mem_of(t: &Trace, map: &[Ref], p: Ref) -> Mem {
    match t.insts[p.index()].op {
        Op::HRef { idx, .. } | Op::HSlot { idx, .. } => Mem::Node(idx),
        Op::ARef { t: tb, idx } => {
            let (tb, idx) = (resolve(map, tb), resolve(map, idx));
            Mem::Elem(tb, idx, const_i32(t, idx))
        }
        _ => Mem::Other,
    }
}

fn may_alias(a: Mem, b: Mem) -> bool {
    match (a, b) {
        (Mem::Node(x), Mem::Node(y)) => x == y,
        (Mem::Node(_), Mem::Elem(..)) | (Mem::Elem(..), Mem::Node(_)) => false,
        (Mem::Elem(_, _, Some(x)), Mem::Elem(_, _, Some(y))) => x == y,
        _ => true,
    }
}

pub fn run(t: &mut Trace) -> bool {
    let backs = phi_back_refs(t);
    let mut map = identity(t);
    // (pointer, its alias class, value known to be there)
    let mut known: Vec<(Ref, Mem, Ref)> = Vec::new();
    let mut changed = false;

    for i in 0..t.insts.len() {
        let op = t.insts[i].op.clone();
        match op {
            Op::Loop => known.clear(),
            Op::XStore { p, v } => {
                let p = resolve(&map, p);
                let v = resolve(&map, v);
                let m = mem_of(t, &map, p);
                known.retain(|&(q, qm, _)| q != p && !may_alias(m, qm));
                known.push((p, m, v));
            }
            Op::XLoad { p, .. } => {
                let p = resolve(&map, p);
                if !backs.contains(&Ref(i as u32)) {
                    if let Some(&(_, _, v)) = known.iter().find(|&&(q, _, _)| q == p) {
                        if t.ty(v) == t.insts[i].ty {
                            map[i] = v;
                            t.insts[i].op = Op::Nop;
                            changed = true;
                            continue;
                        }
                    }
                }
                let m = mem_of(t, &map, p);
                known.retain(|&(q, _, _)| q != p);
                known.push((p, m, Ref(i as u32)));
            }
            _ => {}
        }
    }

    if changed {
        rewrite(t, &map);
    }
    changed
}
