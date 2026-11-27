//! Repeated checks and lookups go.
//!
//! Inlined code reads the same things again and again (`x.re` twice in
//! `Complex.mul`): the same hash slot (`HRef`), metatable (`TMeta`),
//! prototype (`GuardProto`), upvalue stored in a closure, environment flag.
//! A second identical one, in the same region (before the loop / one
//! iteration), with nothing in between that can change what it checks, is
//! the first one: guards drop, values are reused (memory forwarding can
//! then see that two loads read one location).
//!
//! What can change things:
//! - a table's keys / hash layout: instructions that move table storage
//!   (`moves_tables`: TSet, calls of Lua code, `table.insert`) and `rawset`;
//! - a field becoming nil (an `HRef` checking for a value): stores of nil;
//! - metatables: `setmetatable`, and calls of Lua code;
//! - the environment's safe flag: calls of Lua code.
//!
//! Also: `setmetatable(t, mt)` returns `t` - its result is `t`.

use super::super::ir::instruction::{Op, Ref};
use super::super::ir::types::IrType;
use super::super::trace::Trace;
use super::util::{identity, phi_back_refs, resolve, rewrite};
use luau_common::bytecode::BuiltinFunction as B;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// depends on table keys / layout
    Layout,
    /// a slot checked to hold a value
    Value,
    /// a metatable
    Meta,
    /// the environment flag
    Env,
    /// never changes
    Fixed,
    /// an upvalue's value pointer (changes when upvalues close)
    Upval,
}

pub fn run(t: &mut Trace) -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| std::env::var_os("LUAU_TRACE_NOREDUNDANT").is_some()) {
        return false;
    }
    let backs = phi_back_refs(t);
    let loop_at = t.loop_ref.map(|r| r.index());
    let mut map = identity(t);
    let mut seen: HashMap<(u8, u8, u32, u32, u64), (Ref, Kind)> = HashMap::new();
    let mut changed = false;
    for i in 0..t.insts.len() {
        let op = t.insts[i].op.clone();
        let is_call = matches!(op, Op::CallC { .. });
        // what this instruction may change
        if matches!(op, Op::Loop) {
            seen.clear();
            continue;
        }
        let layout = t.insts[i].moves_tables() || matches!(op, Op::Builtin { bfid, .. } if bfid == B::Rawset as u8);
        let meta = is_call || matches!(op, Op::Builtin { bfid, .. } if bfid == B::Setmetatable as u8);
        let nil_store = matches!(op, Op::XStore { v, .. } if t.ty(resolve(&map, v)) == IrType::Nil) || layout;
        let closes = is_call || matches!(op, Op::CloseUpvals { .. });
        if layout || meta || nil_store || closes {
            seen.retain(|_, &mut (_, k)| match k {
                Kind::Upval => !closes,
                Kind::Layout => !layout,
                Kind::Value => !(layout || nil_store),
                Kind::Meta => !(meta || layout),
                Kind::Env => !is_call,
                Kind::Fixed => true,
            });
        }
        let r = |x: Ref| resolve(&map, x).0;
        let ty = t.insts[i].ty;
        // (a fixed-size key: what the instruction reads, by kind - no
        // formatting or allocation per instruction)
        let key: Option<((u8, u32, u32, u64), Kind)> = match op {
            Op::HRef { t: tb, k, idx, lsize, store, .. } => Some(((1, r(tb), r(k), idx as u64 | (lsize as u64) << 32 | (store as u64) << 40), if store { Kind::Value } else { Kind::Layout })),
            Op::TMeta { t: tb, .. } => Some(((2, r(tb), 0, 0), Kind::Meta)),
            Op::GuardPlainTable { t: tb, store, .. } => Some(((3, r(tb), 0, store as u64), Kind::Meta)),
            Op::GuardProto { f, p, .. } => Some(((4, r(f), r(p), 0), Kind::Fixed)),
            Op::GuardCFunc { f, fp, .. } => Some(((5, r(f), 0, fp as u64), Kind::Fixed)),
            Op::ULoad { cl, idx, by_ref: false, .. } => Some(((6, r(cl), idx as u32, ty as u64), Kind::Fixed)),
            Op::GuardFlag { p, off, .. } => Some(((7, r(p), 0, off as u32 as u64), Kind::Env)),
            Op::UPtr { cl, idx, stack, .. } => Some(((8, r(cl), idx as u32, stack as u64), Kind::Upval)),
            // (sizes change only when storage moves; a comparison of the
            // same values holds again)
            Op::ASize(tb) => Some(((9, r(tb), 0, 0), Kind::Layout)),
            Op::ABase(tb) => Some(((10, r(tb), 0, 0), Kind::Layout)),
            Op::Guard { cmp, a, b, .. } => Some(((11, r(a), r(b), cmp as u64), Kind::Fixed)),
            Op::GuardTruthy { a, want, .. } => Some(((12, r(a), 0, want as u64), Kind::Fixed)),
            Op::GuardSame { a, k, .. } => Some(((13, r(a), r(k), 0), Kind::Fixed)),
            Op::GuardDiff { a, k, .. } => Some(((14, r(a), r(k), 0), Kind::Fixed)),
            _ => None,
        };
        let Some(((tag, x, y, z), kind)) = key else { continue };
        let region: u8 = match loop_at {
            Some(l) if i > l => 1,
            _ => 0,
        };
        let key = (region, tag, x, y, z);
        match seen.get(&key) {
            Some(&(first, _)) if !backs.contains(&Ref(i as u32)) => {
                map[i] = first;
                t.insts[i].op = Op::Nop;
                changed = true;
            }
            _ => {
                seen.insert(key, (Ref(i as u32), kind));
            }
        }
    }
    // (y + a) + b of I32 constants: y + (a + b) - wrapping either way -,
    // or y itself when they cancel (index + 1 - 1)
    let kconst = |t: &Trace, r: Ref| match t.inst(r).op {
        Op::KI32(c) => Some(c),
        _ => None,
    };
    for i in 0..t.insts.len() {
        let Op::AddI32(x, k2) = t.insts[i].op else { continue };
        let (x, k2) = (resolve(&map, x), resolve(&map, k2));
        let Some(b) = kconst(t, k2) else { continue };
        let Op::AddI32(y, k1) = t.insts[x.index()].op else { continue };
        let (y, k1) = (resolve(&map, y), resolve(&map, k1));
        let Some(a) = kconst(t, k1) else { continue };
        let sum = a.wrapping_add(b);
        if backs.contains(&Ref(i as u32)) {
            continue;
        }
        if sum == 0 {
            if loop_at.is_some_and(|l| i > l && y.index() < l) {
                // (keep values computed in the body there)
                continue;
            }
            map[i] = y;
            t.insts[i].op = Op::Nop;
            changed = true;
        } else if let Some(k) = (0..t.insts.len()).find(|&j| matches!(t.insts[j].op, Op::KI32(c) if c == sum) && loop_at.is_none_or(|l| j < l || i > l)) {
            if k < i {
                t.insts[i].op = Op::AddI32(y, Ref(k as u32));
                changed = true;
            }
        }
    }
    // setmetatable's result is its table
    for i in 0..t.insts.len() {
        if let Op::Builtin { bfid, args, nargs, .. } = t.insts[i].op {
            if bfid == B::Setmetatable as u8 && nargs >= 1 && t.insts[i].ty == IrType::Table && map[i] == Ref(i as u32) {
                let a0 = resolve(&map, args[0]);
                if t.ty(a0) != IrType::Table {
                    continue;
                }
                // (a PHI's back value must stay in the loop body)
                if backs.contains(&Ref(i as u32)) && loop_at.is_some_and(|l| a0.index() <= l) {
                    continue;
                }
                map[i] = a0;
                changed = true;
            }
        }
    }
    if changed {
        rewrite(t, &map);
    }
    changed
}
