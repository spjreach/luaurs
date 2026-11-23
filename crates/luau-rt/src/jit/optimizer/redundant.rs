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
}

pub fn run(t: &mut Trace) -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| std::env::var_os("LUAU_TRACE_NOREDUNDANT").is_some()) {
        return false;
    }
    let backs = phi_back_refs(t);
    let loop_at = t.loop_ref.map(|r| r.index());
    let mut map = identity(t);
    let mut seen: HashMap<String, (Ref, Kind)> = HashMap::new();
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
        if layout || meta || nil_store || is_call {
            seen.retain(|_, &mut (_, k)| match k {
                Kind::Layout => !layout,
                Kind::Value => !(layout || nil_store),
                Kind::Meta => !(meta || layout),
                Kind::Env => !is_call,
                Kind::Fixed => true,
            });
        }
        let r = |x: Ref| resolve(&map, x).0;
        let ty = t.insts[i].ty;
        let key = match op {
            Op::HRef { t: tb, k, idx, lsize, store, .. } => Some((format!("HRef {} {} {} {} {}", r(tb), r(k), idx, lsize, store), if store { Kind::Value } else { Kind::Layout })),
            Op::TMeta { t: tb, .. } => Some((format!("TMeta {}", r(tb)), Kind::Meta)),
            Op::GuardPlainTable { t: tb, store, .. } => Some((format!("Plain {} {}", r(tb), store), Kind::Meta)),
            Op::GuardProto { f, p, .. } => Some((format!("Proto {} {}", r(f), r(p)), Kind::Fixed)),
            Op::GuardCFunc { f, fp, .. } => Some((format!("CFunc {} {}", r(f), fp), Kind::Fixed)),
            Op::ULoad { cl, idx, by_ref: false, .. } => Some((format!("UVal {} {} {:?}", r(cl), idx, ty), Kind::Fixed)),
            Op::GuardFlag { p, off, .. } => Some((format!("Flag {} {}", r(p), off), Kind::Env)),
            _ => None,
        };
        let Some((key, kind)) = key else { continue };
        let region = match loop_at {
            Some(l) if i > l => 1,
            _ => 0,
        };
        let key = format!("{region}|{key}");
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
