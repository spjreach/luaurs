//! Load/store forwarding for exact pointer values.
//!
//! `XStore { p, v }` followed by `XLoad { p }` with no intervening store can
//! read `v` directly.  A store to any other pointer clears the table because
//! pointers may alias conservatively; the `Loop` marker also clears it.

use super::super::ir::instruction::{Op, Ref};
use super::super::trace::Trace;
use super::util::{identity, phi_back_refs, resolve, rewrite};

pub fn run(t: &mut Trace) -> bool {
    let backs = phi_back_refs(t);
    let mut map = identity(t);
    let mut last: Option<(Ref, Ref)> = None;
    let mut changed = false;

    for i in 0..t.insts.len() {
        let op = t.insts[i].op.clone();
        match op {
            Op::Loop => last = None,
            Op::XStore { p, v } => {
                // Any store may alias a previous pointer; only the latest
                // store can be forwarded from.
                last = Some((resolve(&map, p), resolve(&map, v)));
            }
            Op::XLoad { p, .. } => {
                if backs.contains(&Ref(i as u32)) {
                    continue;
                }
                let p = resolve(&map, p);
                if let Some((stored_p, v)) = last {
                    if stored_p == p && t.ty(v) == t.insts[i].ty {
                        map[i] = v;
                        t.insts[i].op = Op::Nop;
                        changed = true;
                    }
                }
            }
            _ => {}
        }
    }

    if changed {
        rewrite(t, &map);
    }
    changed
}
