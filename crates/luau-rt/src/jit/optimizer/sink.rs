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
    sink_phis(t);
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
    t.sinks.push(Sink { at: ar, tpl: tpl_ptr.unwrap_or(0), narray, nhash, states: snaps, entry: None });
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

/// Tables carried around the loop: a PHI whose back value is a table the
/// loop creates (and only reads through the PHI: fields, metatable)
/// becomes one PHI per field of it (the entry table's fields before the
/// loop, the stored values after each iteration) and the metatable the
/// new table gets (the entry table's checked to be the same). Exits
/// build it from those - or, in the first iteration, when it is still
/// the entry table, store that table itself (a Boolean PHI says which).
/// The new table is then an ordinary candidate for sinking.
fn sink_phis(t: &mut Trace) {
    if t.loop_ref.is_none() {
        return;
    }
    for _ in 0..8 {
        let l = t.loop_ref.unwrap().index();
        let n = t.insts.len();
        let mut users: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, inst) in t.insts.iter().enumerate() {
            for o in inst.operands() {
                users[o.index()].push(i);
            }
        }
        let mut snap_users: Vec<Vec<usize>> = vec![Vec::new(); t.snapshots.len()];
        for (i, inst) in t.insts.iter().enumerate() {
            if let Some(s) = inst.snapshot() {
                snap_users[s.0 as usize].push(i);
            }
        }
        let mut found = None;
        for p in l + 1..n {
            let Op::Phi { pre: x, back: a } = t.insts[p].op else { continue };
            if t.insts[p].ty != IrType::Table || a.index() <= l || !matches!(t.insts[a.index()].op, Op::DupTable { .. }) {
                continue;
            }
            if let Some(plan) = plan_phi(t, p, x, a.index(), &users, &snap_users) {
                found = Some(plan);
                break;
            }
        }
        let Some(plan) = found else { return };
        apply_phi(t, plan);
    }
}

struct PhiPlan {
    p: usize,
    x: Ref,
    a: usize,
    /// per field node of the new table: (node, key, lsize, value stored by
    /// the iteration's end, its type)
    fields: Vec<(u32, Ref, u8, Ref, IrType)>,
    /// the new table's metatable
    meta: Option<Ref>,
    /// loads through the PHI: (load, node)
    loads: Vec<(usize, u32)>,
    /// the PHI's slot lookups (go) and metatable reads
    drop: Vec<usize>,
    metas: Vec<usize>,
}

fn plan_phi(t: &Trace, p: usize, x: Ref, a: usize, users: &[Vec<usize>], snap_users: &[Vec<usize>]) -> Option<PhiPlan> {
    let pr = Ref(p as u32);
    let ar = Ref(a as u32);
    // the PHI: read only (in the first iteration it is the entry table)
    let mut loads = Vec::new();
    let mut drop = Vec::new();
    let mut metas = Vec::new();
    for &u in &users[p] {
        match t.insts[u].op {
            Op::HRef { t: tb, store: false, idx, .. } if tb == pr => {
                for &w in &users[u] {
                    match t.insts[w].op {
                        Op::XLoad { p: pp, .. } if pp.index() == u => loads.push((w, idx)),
                        _ => return None,
                    }
                }
                drop.push(u);
            }
            Op::TMeta { t: tb, .. } if tb == pr => metas.push(u),
            _ => return None,
        }
    }
    // the new table: its own slots, setmetatable, the PHI, snapshots
    let mut fields: std::collections::BTreeMap<u32, (Ref, u8, Ref)> = std::collections::BTreeMap::new();
    let mut meta = None;
    let mut events: Vec<usize> = Vec::new();
    for &u in &users[a] {
        match &t.insts[u].op {
            Op::Phi { .. } if u == p => {}
            Op::HRef { t: tb, .. } if *tb == ar => {
                for &w in &users[u] {
                    match t.insts[w].op {
                        Op::XLoad { p: pp, .. } if pp.index() == u => {}
                        Op::XStore { p: pp, v } if pp.index() == u && v != ar && v != pr => events.push(w),
                        _ => return None,
                    }
                }
            }
            Op::Builtin { bfid, args, nargs, .. } if *bfid == B::Setmetatable as u8 && *nargs == 2 && args[0] == ar && users[u].is_empty() && t.ty(args[1]) == IrType::Table => {
                meta = Some(args[1]);
            }
            Op::TMeta { t: tb, .. } | Op::GuardPlainTable { t: tb, .. } if *tb == ar => {}
            Op::Barrier { t: tb, v } if *tb == ar && *v != ar => {}
            _ => return None,
        }
    }
    events.sort_unstable();
    for e in events {
        if let Op::XStore { p: pp, v } = t.insts[e].op {
            if let Op::HRef { k, idx, lsize, .. } = t.inst(pp).op {
                fields.insert(idx, (k, lsize, v));
            }
        }
    }
    let l = t.loop_ref?.index();
    // carried field values come from the body; the PHI's reads read
    // stored fields of the type stored
    let mut out = Vec::new();
    for (&idx, &(k, lsize, v)) in &fields {
        if v.index() <= l || t.ty(v) == IrType::Nil || k.index() >= l {
            return None;
        }
        out.push((idx, k, lsize, v, t.ty(v)));
    }
    for &(w, idx) in &loads {
        match fields.get(&idx) {
            Some(&(_, _, v)) if t.ty(v) == t.insts[w].ty => {}
            _ => return None,
        }
    }
    if !metas.is_empty() && meta.is_none() {
        return None;
    }
    if let Some(m) = meta {
        if m == pr || m == ar {
            return None;
        }
    }
    // snapshots holding the PHI: exits only
    for (s, sn) in t.snapshots.iter().enumerate() {
        if sn.entries.iter().any(|e| e.value == pr) && snap_users[s].iter().any(|&u| flushes(&t.insts[u].op)) {
            return None;
        }
    }
    Some(PhiPlan { p, x, a, fields: out, meta, loads, drop, metas })
}

fn apply_phi(t: &mut Trace, plan: PhiPlan) {
    use super::super::ir::instruction::{Inst, SnapId};
    let l = t.loop_ref.unwrap().index();
    let orig = t.insts.len();
    let entry_snap = SnapId(0);
    fn push(t: &mut Trace, op: Op, ty: IrType) -> usize {
        t.insts.push(Inst { op, ty });
        t.insts.len() - 1
    }
    let mut pre_new: Vec<usize> = Vec::new();
    let mut phi_new: Vec<usize> = Vec::new();
    let mut body_new: Vec<usize> = Vec::new();
    // before the loop: the entry table's fields (and metatable)
    let mut pre_loads = Vec::new();
    for &(idx, k, lsize, _, ty) in &plan.fields {
        let h = push(t, Op::HRef { t: plan.x, k, idx, lsize, store: false, snap: entry_snap }, IrType::Ptr);
        let ld = push(t, Op::XLoad { p: Ref(h as u32), snap: entry_snap }, ty);
        pre_new.extend([h, ld]);
        pre_loads.push(ld);
    }
    let meta_val: Option<usize> = match plan.meta {
        None => None,
        Some(m) => {
            let tm = push(t, Op::TMeta { t: plan.x, snap: entry_snap }, IrType::Table);
            pre_new.push(tm);
            if m.index() < l {
                // the same metatable all along
                let g = push(t, Op::GuardSame { a: Ref(tm as u32), k: m, snap: entry_snap }, IrType::Nil);
                pre_new.push(g);
                Some(m.index())
            } else {
                let mp = push(t, Op::Phi { pre: Ref(tm as u32), back: m }, IrType::Table);
                phi_new.push(mp);
                Some(mp)
            }
        }
    };
    // the first-iteration flag
    let kt = push(t, Op::KBool(true), IrType::Boolean);
    pre_new.push(kt);
    let nb = push(t, Op::Not(Ref(kt as u32)), IrType::Boolean);
    body_new.push(nb);
    let ep = push(t, Op::Phi { pre: Ref(kt as u32), back: Ref(nb as u32) }, IrType::Boolean);
    phi_new.push(ep);
    let mut field_phis: Vec<(u32, usize)> = Vec::new();
    for (j, &(idx, _, _, v, ty)) in plan.fields.iter().enumerate() {
        let fp = push(t, Op::Phi { pre: Ref(pre_loads[j] as u32), back: v }, ty);
        phi_new.push(fp);
        field_phis.push((idx, fp));
    }
    // reads through the PHI take the field PHIs / the metatable
    let n = t.insts.len();
    let mut map: Vec<Ref> = (0..n as u32).map(Ref).collect();
    for &(w, idx) in &plan.loads {
        let fp = field_phis.iter().find(|f| f.0 == idx).unwrap().1;
        map[w] = Ref(fp as u32);
        t.insts[w].op = Op::Nop;
    }
    for &u in &plan.metas {
        map[u] = Ref(meta_val.unwrap() as u32);
        t.insts[u].op = Op::Nop;
    }
    for &u in &plan.drop {
        t.insts[u].op = Op::Nop;
    }
    super::util::rewrite(t, &map);
    // the PHI itself: only exits name it now
    t.insts[plan.p].op = Op::Sunk;
    // order: pre-loop, new pre-loop, the old PHI, Loop, PHIs, new PHIs,
    // body, new body
    let is_phi = |t: &Trace, i: usize| matches!(t.insts[i].op, Op::Phi { .. });
    let mut order: Vec<usize> = (0..l).collect();
    order.extend(pre_new.iter().copied());
    order.push(plan.p);
    order.push(l);
    let body: Vec<usize> = (l + 1..orig).filter(|&i| i != plan.p).collect();
    // (the PHI block: leading PHIs and Nops)
    let lead = body.iter().take_while(|&&i| is_phi(t, i) || matches!(t.insts[i].op, Op::Nop)).count();
    order.extend(body[..lead].iter().copied());
    order.extend(phi_new.iter().copied());
    order.extend(body[lead..].iter().copied());
    order.extend(body_new.iter().copied());
    let new_of = super::hoist_consts::reorder(t, &order);
    let rn = |i: usize| Ref(new_of[i]);
    // how exits build it
    let tpl = match t.insts[new_of[plan.a] as usize].op {
        Op::DupTable { tpl } => match t.inst(tpl).op {
            Op::KPtr(p) => p,
            _ => 0,
        },
        _ => 0,
    };
    let pr = rn(plan.p);
    let fields: Vec<(u32, Ref)> = field_phis.iter().map(|&(idx, fp)| (idx, rn(fp))).collect();
    let meta = meta_val.map(rn);
    let states = t
        .snapshots
        .iter()
        .enumerate()
        .filter(|(_, sn)| sn.entries.iter().any(|e| e.value == pr))
        .map(|(s, _)| SinkState { snap: s as u32, fields: fields.clone(), meta })
        .collect();
    t.sinks.push(Sink { at: pr, tpl, narray: 0, nhash: 0, states, entry: Some((rn(plan.x.index()), rn(ep))) });
}
