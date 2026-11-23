//! Allocation sinking: tables that never escape the trace aren't created.
//!
//! A `DupTable` / `NewTable` whose only uses are
//! - its own hash slots (`HRef`), read (`XLoad`) or written (`XStore` of a
//!   value that isn't a table being sunk itself),
//! - `setmetatable(t, mt)` with a table `mt`, its metatable (`TMeta`) and
//!   plain-table checks, write barriers,
//! - and the snapshots of guards (exits)
//!
//! is replaced by what the trace knows about it: loads of its fields
//! become the values stored there, its metatable the one set. An exit
//! through a snapshot holding it builds it then (`Trace::sinks`): the
//! same template (or empty table), the fields as stored before that guard,
//! the metatable. Everything else (stored somewhere, passed to a call,
//! carried around the loop, flushed to the stack before a call, compared)
//! keeps the allocation. So does a snapshot shared by guards that see
//! different field values (each exit builds one state).
//!
//! The layout checks of its slots always pass (a fresh table from one
//! template has one layout), so they go too.

use super::super::ir::instruction::{Op, Ref};
use super::super::ir::types::IrType;
use super::super::trace::{Sink, SinkState, Trace};
use luau_common::bytecode::BuiltinFunction as B;
use std::collections::{HashMap, HashSet};

/// Instructions whose snapshot lists slots to store before a call (not an
/// exit): a table in it would have to exist.
fn flushes(op: &Op) -> bool {
    matches!(op, Op::CCall { .. } | Op::Concat { .. } | Op::CallC { .. } | Op::CloseUpvals { .. })
}

pub fn run(t: &mut Trace) {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| std::env::var_os("LUAU_TRACE_NOSINK").is_some()) {
        return;
    }
    let n = t.insts.len();
    let cands: Vec<usize> = (0..n).filter(|&i| matches!(t.insts[i].op, Op::DupTable { .. } | Op::NewTable { .. })).collect();
    if cands.is_empty() {
        return;
    }
    // users of every value
    let mut users: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, inst) in t.insts.iter().enumerate() {
        for o in inst.operands() {
            users[o.index()].push(i);
        }
    }
    // snapshot -> instructions using it
    let mut snap_users: Vec<Vec<usize>> = vec![Vec::new(); t.snapshots.len()];
    for (i, inst) in t.insts.iter().enumerate() {
        if let Some(s) = inst.snapshot() {
            snap_users[s.0 as usize].push(i);
        }
    }
    let last_snap = t.snapshots.len() - 1;
    let mut sunk: HashSet<usize> = HashSet::new();
    for &a in &cands {
        if analyse(t, a, &users, &cands) {
            sunk.insert(a);
        }
    }
    // a table stored into another (even a sunk one) escapes
    loop {
        let mut drop = None;
        for &a in &sunk {
            for &u in &users[a] {
                if let Op::XStore { v, .. } = t.insts[u].op {
                    if v.index() == a {
                        drop = Some(a);
                    }
                }
            }
        }
        match drop {
            Some(a) => {
                sunk.remove(&a);
            }
            None => break,
        }
    }
    let mut sunk: Vec<usize> = sunk.into_iter().collect();
    sunk.sort_unstable();
    for a in sunk {
        sink_one(t, a, &users, &snap_users, last_snap);
    }
}

/// Whether table `a`'s uses all fit (see the module comment).
fn analyse(t: &Trace, a: usize, users: &[Vec<usize>], cands: &[usize]) -> bool {
    let ar = Ref(a as u32);
    for &u in &users[a] {
        let ok = match &t.insts[u].op {
            Op::HRef { t: tb, .. } if *tb == ar => users[u].iter().all(|&w| match t.insts[w].op {
                Op::XLoad { p, .. } => p.index() == u,
                Op::XStore { p, v } => p.index() == u && v != ar && !cands.contains(&v.index()),
                _ => false,
            }),
            Op::Builtin { bfid, args, nargs, .. } => {
                users[u].is_empty()
                    && *bfid == B::Setmetatable as u8 && *nargs == 2 && args[0] == ar && args[1] != ar && t.ty(args[1]) == IrType::Table && !cands.contains(&args[1].index())
            }
            Op::TMeta { t: tb, .. } | Op::GuardPlainTable { t: tb, .. } => *tb == ar,
            Op::Barrier { t: tb, v } => *tb == ar && *v != ar,
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

fn sink_one(t: &mut Trace, a: usize, users: &[Vec<usize>], snap_users: &[Vec<usize>], last_snap: usize) {
    let ar = Ref(a as u32);
    // the table's history in trace order: field stores and metatable
    let mut events: Vec<usize> = users[a].clone();
    for &u in &users[a] {
        if matches!(t.insts[u].op, Op::HRef { .. }) {
            events.extend(users[u].iter().copied());
        }
    }
    events.sort_unstable();
    events.dedup();
    // simulate: replace loads, check guards
    let mut fields: HashMap<u32, Ref> = HashMap::new();
    let mut meta: Option<Ref> = None;
    let mut subst: Vec<(usize, Ref)> = Vec::new();
    let mut states: Vec<(usize, HashMap<u32, Ref>, Option<Ref>)> = Vec::new();
    let node_of = |t: &Trace, p: Ref| match t.inst(p).op {
        Op::HRef { idx, .. } => Some(idx),
        _ => None,
    };
    for &e in &events {
        match t.insts[e].op.clone() {
            Op::XStore { p, v } => {
                if t.ty(v) == IrType::Nil {
                    return;
                }
                fields.insert(node_of(t, p).unwrap(), v);
            }
            Op::XLoad { p, .. } => match fields.get(&node_of(t, p).unwrap()) {
                Some(&v) if t.ty(v) == t.insts[e].ty => subst.push((e, v)),
                // (a template value, or another type: kept as it is)
                _ => return,
            },
            Op::HRef { store: true, idx, .. } => {
                // checks the slot has a value: one stored, or the template's
                if !fields.contains_key(&idx) && !template_has(t, a, idx) {
                    return;
                }
            }
            Op::Builtin { args, .. } => meta = Some(args[1]),
            Op::TMeta { .. } => match meta {
                Some(m) => subst.push((e, m)),
                None => return,
            },
            Op::GuardPlainTable { .. } => {
                if meta.is_some() {
                    return;
                }
            }
            _ => {}
        }
        states.push((e, fields.clone(), meta));
    }
    // the state at position `at` (after every event before it)
    let state_at = |at: usize| -> (HashMap<u32, Ref>, Option<Ref>) {
        let mut f = HashMap::new();
        let mut m = None;
        for (e, ff, mm) in &states {
            if *e < at {
                f = ff.clone();
                m = *mm;
            }
        }
        (f, m)
    };
    // the snapshots holding it: exits only, one state each
    let mut snaps: Vec<SinkState> = Vec::new();
    for (s, sn) in t.snapshots.iter().enumerate() {
        if !sn.entries.iter().any(|e| e.value == ar) {
            continue;
        }
        let at: Vec<usize> = if s == last_snap && t.loop_ref.is_some() { vec![usize::MAX] } else { snap_users[s].clone() };
        if at.iter().any(|&u| u != usize::MAX && flushes(&t.insts[u].op)) {
            return;
        }
        let mut st: Option<(HashMap<u32, Ref>, Option<Ref>)> = None;
        for &u in &at {
            let here = state_at(u);
            match &st {
                None => st = Some(here),
                Some(prev) if *prev != here => return,
                _ => {}
            }
        }
        // (no guard uses it: the state at the end, never needed)
        let (fields, meta) = st.unwrap_or_else(|| state_at(usize::MAX));
        let mut fields: Vec<(u32, Ref)> = fields.into_iter().collect();
        fields.sort_unstable();
        snaps.push(SinkState { snap: s as u32, fields, meta });
    }
    // commit: loads and metatable reads take the known values, the rest goes
    let mut map: Vec<Ref> = (0..t.insts.len() as u32).map(Ref).collect();
    for (e, v) in subst {
        map[e] = v;
    }
    for &u in &users[a] {
        if matches!(t.insts[u].op, Op::HRef { .. }) {
            for &w in &users[u] {
                t.insts[w].op = Op::Nop;
            }
        }
        t.insts[u].op = Op::Nop;
    }
    let (tpl, narray, nhash) = match t.insts[a].op {
        Op::DupTable { tpl } => (Some(tpl), 0, 0),
        Op::NewTable { narray, nhash } => (None, narray, nhash),
        _ => unreachable!(),
    };
    t.insts[a].op = Op::Sunk;
    let tpl_ptr = tpl.map(|r| match t.inst(r).op {
        Op::KPtr(p) => p,
        _ => 0,
    });
    t.sinks.push(Sink { at: ar, tpl: tpl_ptr.unwrap_or(0), narray, nhash, states: snaps });
    super::util::rewrite(t, &map);
}

/// Whether the template of DupTable `a` has a value in node `idx`.
fn template_has(t: &Trace, a: usize, idx: u32) -> bool {
    let Op::DupTable { tpl } = t.insts[a].op else { return false };
    let Op::KPtr(p) = t.inst(tpl).op else { return false };
    unsafe {
        let h = p as *mut crate::lobject::LuaTable;
        if idx as usize >= (1usize << (*h).lsizenode) {
            return false;
        }
        (*(*h).node.add(idx as usize)).val.tt != crate::lobject::LUA_TNIL
    }
}
