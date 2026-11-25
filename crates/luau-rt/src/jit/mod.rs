//! Tracing JIT (in progress).
//!
//! Pipeline, LuaJIT style, on top of the method JIT's x64 assembler:
//!
//! 1. `hotness`: backward branches count loop executions per header.
//! 2. `recorder`: a hot loop is recorded while the interpreter runs one
//!    iteration, instruction by instruction, into SSA IR (`ir`), with type
//!    guards and `snapshots` of the VM state for side exits.
//! 3. `optimizer`: passes over the IR (folding, CSE, DCE, LICM, forwarding).
//! 4. `compiler`: the trace becomes x64 code (`codegen::asm`); guards jump to
//!    exit stubs that restore the stack from a snapshot (`side_exit`).
//! 5. The interpreter enters the compiled trace at the loop header.
//!
//! Enabled with `LUAU_TRACE=1` (off by default while incomplete).
//!
//! Ownership while three agents work on this: recorder / IR / snapshots /
//! side exits / compiler / state: Claude; `optimizer/`: DeepSeek;
//! `config.rs` / `debug.rs` (env vars, logging, dumps): GPT.

pub mod compiler;
pub mod config;
pub mod debug;
pub mod hotness;
pub mod ir;
pub mod lualib;
pub mod optimizer;
pub mod snapshots;
pub mod state;
pub mod trace;

#[cfg(test)]
mod tests;

pub mod recorder;

use crate::lobject::{lua_State, Proto, StkId};
use std::cell::{Cell, RefCell};

thread_local! {
    /// L->singlestep before recording forced it on (so calls from C -
    /// metamethods - run in the single-step interpreter the recorder sees)
    static SAVED_SS: Cell<Option<(usize, bool)>> = const { Cell::new(None) };
    static RECORDER: RefCell<Option<(recorder::Recorder, StkId)>> = const { RefCell::new(None) };
    static RECORDING: Cell<bool> = const { Cell::new(false) };
}

unsafe fn begin_recording(L: *mut lua_State) {
    SAVED_SS.with(|s| s.set(Some((L as usize, (*L).singlestep))));
    (*L).singlestep = true;
    RECORDING.with(|r| r.set(true));
}

unsafe fn end_recording() {
    RECORDING.with(|r| r.set(false));
    if let Some((l, ss)) = SAVED_SS.with(|s| s.take()) {
        (*(l as *mut lua_State)).singlestep = ss;
    }
}

/// True while a trace is being recorded on this thread (checked by the
/// single-step interpreter before every instruction).
#[inline(always)]
pub fn is_recording() -> bool {
    RECORDING.with(|r| r.get())
}

/// Side exits a compiled trace may take before it is dropped and the loop
/// is re-recorded for the current types (a guard that keeps failing would
/// otherwise exit on every iteration - correct but slower than the
/// interpreter).
pub const EXIT_STORM_LIMIT: u32 = 16;

/// Exits through one snapshot before a side trace is recorded from it.
pub const HOTEXIT: u32 = 10;

/// `exit_tries` of an exit whose side trace left the loop (a break /
/// return path: the loop's normal end, not worth tracing or a storm).
pub const EXIT_LEAVES: u8 = 250;

/// Average cycles a trace runs before an exit that can't be continued,
/// below which (with EXIT_STORM_LIMIT such exits) it is re-recorded.
pub const STORM_CYCLES: u64 = 2000;

/// `on_backedge` result: the trace left through a snapshot inside inlined
/// calls; their frames were pushed and `L->ci` resumes at its savedpc.
pub const RESUME_FRAME: u32 = u32::MAX;

/// `on_backedge` result: a C function the trace called yielded; the VM is
/// left as the interpreter's CALL would leave it.
pub const LEFT_VM: u32 = u32::MAX - 1;

/// Recreates the CallInfos of the inlined calls live at snapshot `sn` (the
/// trace already wrote their slots) and points the innermost at `sn.pc`.
unsafe fn push_frames(L: *mut lua_State, base: StkId, sn: &snapshots::Snapshot) {
    use crate::lobject::Closure;
    for f in &sn.frames {
        let caller = (*L).ci;
        (*caller).savedpc = (*(*caller).p).code.add(f.ret_pc as usize);
        let func = base.add(f.func_slot());
        // (the trace wrote the function slot if it set it)
        let cl = crate::lobject::clvalue(func);
        let ci = crate::ldo::incr_ci(L);
        (*ci).func = func;
        (*ci).p = Closure::proto(cl);
        (*ci).base = base.add(f.base as usize);
        (*ci).top = (*ci).base.add(f.top_off((*cl).stacksize as usize));
        (*ci).savedpc = std::ptr::null();
        (*ci).flags = 0;
        (*ci).nresults = f.nresults;
    }
    let ci = (*L).ci;
    (*ci).savedpc = (*(*ci).p).code.add(sn.pc as usize);
    (*L).base = (*ci).base;
    (*L).top = (*ci).top;
}

/// Drops everything known about a freed function.
pub fn forget_proto(p: *mut Proto) {
    if !state::trace_enabled() {
        return;
    }
    state::try_with(|s| {
        if !s.traces.is_empty() {
            let dead: Vec<(usize, u32)> = s
                .traces
                .iter()
                .filter(|(&(q, _), t)| q == p as usize || t.protos.contains(&(p as usize)))
                .map(|(&k, _)| k)
                .collect();
            for k in dead {
                s.remove_root(k);
            }
        }
        s.hot.forget_proto(p);
    });
}

/// A taken backward branch to `header` in `p` (single-step interpreter,
/// tracing enabled). Runs the loop's compiled trace if there is one and
/// returns the pc to resume interpreting at; otherwise counts the loop and
/// starts recording when it's hot.
pub unsafe fn on_backedge(L: *mut lua_State, p: *mut Proto, header: u32, base: StkId) -> Option<u32> {
    if is_recording() {
        return None;
    }
    let native = state::with(|s| s.traces.get(&(p as usize, header)).and_then(|t| t.native.map(|n| (n.entry, t.id, t.stack_need))));
    if let Some((entry, id, need)) = native {
        // room for the trace's inlined frames (the trace checks too, but
        // can only exit): growing the stack moves it, so the caller then
        // reloads its frame (RESUME_FRAME)
        let mut base = base;
        let moved = (*L).stack_last.offset_from(base) < need as isize;
        if moved {
            let n = need as isize - (*L).top.offset_from(base);
            crate::ldo::luaD_checkstack(L, n.max(1) as i32);
            base = (*L).base;
        }
        let resume_at = move |pc: u32| -> Option<u32> {
            if moved {
                let ci = (*L).ci;
                (*ci).savedpc = (*(*ci).p).code.add(pc as usize);
                Some(RESUME_FRAME)
            } else {
                Some(pc)
            }
        };
        let f: compiler::TraceFn = std::mem::transmute(entry);
        let t0 = cycles();
        let ex = f(base, L);
        LAST_RUN.with(|c| c.set(cycles().wrapping_sub(t0)));
        return handle_exit(L, p, header, base, id, ex, moved);
    }
    let start = state::with(|s| {
        if !s.enabled || s.traces.contains_key(&(p as usize, header)) || s.traces.len() >= s.max_traces {
            return None;
        }
        if s.hot.tick(p, header, s.hotloop) {
            Some((s.new_id(), s.max_trace_len))
        } else {
            None
        }
    });
    if let Some((id, max_len)) = start {
        debug::log(debug::Event::TraceStart { id: id.0, pc: header });
        let mut rec = recorder::Recorder::new(id, p, crate::lobject::clvalue((*(*L).ci).func), header, base, max_len);
        rec.set_state(L);
        RECORDER.with(|r| *r.borrow_mut() = Some((rec, base)));
        begin_recording(L);
    }
    None
}

/// What happens after the trace of the loop at `header` in `p` returned
/// `ex` (run with `base`; `moved`: the stack was grown before, so the
/// caller must reload its frame): side exit counting, side trace
/// recording, storms, frames of inlined calls. Returns what `on_backedge`
/// does.
unsafe fn handle_exit(L: *mut lua_State, p: *mut Proto, header: u32, base: StkId, id: trace::TraceId, ex: u32, moved: bool) -> Option<u32> {
        if ex == trace::EXIT_LEFT_VM {
            return Some(LEFT_VM);
        }
        let resume_at = move |pc: u32| -> Option<u32> {
            if moved {
                let ci = (*L).ci;
                (*ci).savedpc = (*(*ci).p).code.add(pc as usize);
                Some(RESUME_FRAME)
            } else {
                Some(pc)
            }
        };
        let (tag, k) = (((ex >> 16) & 0x7fff) as u16, ex & 0xffff);
        if ex & 0x8000_0000 != 0 {
            // interrupted at a back edge or link, or GC work is due (traces
            // that allocate exit for it): resume where it was going (a
            // loop's header, or the header a link goes to)
            let at = state::with(|s| s.by_tag_mut(tag).map(|t| if t.loop_ref.is_some() { t.start_pc } else { t.snapshots[k as usize].pc })).unwrap_or(header);
            if crate::lgc::luaC_needsGC(L) {
                // the collector may shrink (move) the stack: the caller
                // reloads its frame (RESUME_FRAME)
                let ci = (*L).ci;
                (*ci).savedpc = (*(*ci).p).code.add(at as usize);
                crate::lgc::luaC_step(L, true);
                return Some(RESUME_FRAME);
            }
            return resume_at(at);
        }
        debug::log(debug::Event::SideExit { id: id.0, snap: ex });
        // (the trace that exited may be another loop's: traces link)
        let Some((key, dead)) = state::with(|s| s.root_key_of(tag)) else {
            return resume_at(header);
        };
        if dead {
            // a dropped trace still running: resume as its snapshot says
            let (pc, sn) = state::with(|s| {
                let t = s.by_tag_mut(tag).unwrap();
                let sn = t.snapshots[k as usize].clone();
                (sn.pc, sn)
            });
            let keep = (*L).top;
            if !sn.frames.is_empty() {
                push_frames(L, base, &sn);
                if let Some(tp) = sn.top {
                    (*L).top = if tp == recorder::KEEP_TOP { keep } else { base.add(tp as usize) };
                }
                return Some(RESUME_FRAME);
            }
            if let Some(tp) = sn.top {
                (*L).top = if tp == recorder::KEEP_TOP { keep } else { base.add(tp as usize) };
            }
            return resume_at(pc);
        }
        let header = key.1;
        // Leaving the loop is normal. An exit back into the loop body that
        // gets hot is continued by a side trace (recorded from there to the
        // loop header, then linked to the exit); exits that can't be
        // side-traced count toward the storm limit, which drops the trace
        // for a re-record.
        let run = LAST_RUN.with(|c| c.get());
        let (storm, resume, frames, side, top) = state::with(|s| {
            let max_sides = 64;
            let (nsides, root_end) = match s.traces.get(&key) {
                Some(r) => (r.sides.len(), r.end_pc),
                None => return (false, header, None, None, None),
            };
            let recording = is_recording();
            let t = s.by_tag_mut(tag).expect("exit from an unknown trace");
            let sn = &t.snapshots[k as usize];
            let in_loop = !sn.frames.is_empty() || (sn.pc >= header && sn.pc <= root_end);
            let sn_frames_empty = sn.frames.is_empty();
            let tries = t.exit_tries[k as usize];
            // (an exit whose side trace left the loop is the loop's end)
            let leaves = tries == EXIT_LEAVES;
            // (an exit leaving the loop too: its side trace may run on to an
            // enclosing loop's trace)
            let traceable = !leaves && sn.pc != header && t.exit_tries[k as usize] < 3 && nsides < max_sides;
            let (pc, frames, top) = (sn.pc, if sn.frames.is_empty() { None } else { Some(sn.clone()) }, sn.top);
            let mut side = None;
            // (an in-body guard resuming at the header is a while loop's
            // condition failing: the loop's normal end, like leaving it)
            let loop_end_exit = (sn.pc == header && k != 0) || leaves;
            let mut count_storm = in_loop && !traceable && !loop_end_exit;
            if traceable {
                t.exit_counts[k as usize] += 1;
                if t.exit_counts[k as usize] >= HOTEXIT && !recording && std::env::var_os("LUAU_TRACE_NOSIDE").is_none() {
                    t.exit_counts[k as usize] = 0;
                    t.exit_tries[k as usize] += 1;
                    side = Some((tag, k, pc));
                }
            } else if !in_loop {
                count_storm = false;
            }
            if count_storm && std::env::var_os("LUAU_TRACE_STORMDBG").is_some() {
                eprintln!("storm-exit frames={} header={} k0={} tries={} sides={}", !sn_frames_empty, pc == header, k == 0, tries, nsides);
            }
            let root = s.traces.get_mut(&key).unwrap();
            if count_storm {
                root.side_exits += 1;
                root.storm_cycles += run;
            }
            // a trace is dropped for a re-record when its exits that can't
            // be continued come often and quickly (a trace running a long
            // time between them is still worth it)
            let storm = root.side_exits >= EXIT_STORM_LIMIT && root.storm_cycles / (root.side_exits as u64) < STORM_CYCLES;
            (storm, pc, frames.clone(), side.map(|x| (x, root.end_pc, frames.map(|f| f.frames).unwrap_or_default())), top)
        });
        if storm {
            let reason = trace::AbortReason::Internal("side exit storm".into());
            debug::log(debug::Event::TraceAbort { id: id.0, pc: header, reason: &reason });
            state::with(|s| {
                s.remove_root(key);
                s.hot.penalize(p, header);
                s.note_abort((id, header, reason));
            });
        } else if let Some(((ptag, k, pc), end, sframes)) = side {
            // record a side trace from the exit's pc (the interpreter runs
            // on from there)
            let (sid, max_len) = state::with(|s| (s.new_id(), s.max_trace_len));
            debug::log(debug::Event::TraceStart { id: sid.0, pc });
            // (the stack top a call left for the next instruction)
            let side_top = top.map(|t| if t == recorder::KEEP_TOP { ((*L).top.offset_from(base) as usize, true) } else { (t as usize, false) });
            let mut rec = recorder::Recorder::new_side(sid, p, crate::lobject::clvalue((*(*L).ci).func), pc, base, max_len, header, end, (ptag, k), sframes, side_top);
            rec.set_state(L);
            RECORDER.with(|r| *r.borrow_mut() = Some((rec, base)));
            begin_recording(L);
        }
        // Resume where the guard's snapshot says, even when the trace is
        // dropped (resuming at `header` would replay the iteration).
        let keep = (*L).top;
        if let Some(sn) = frames {
            push_frames(L, base, &sn);
            if let Some(t) = top {
                (*L).top = if t == recorder::KEEP_TOP { keep } else { base.add(t as usize) };
            }
            return Some(RESUME_FRAME);
        }
        if let Some(t) = top {
            (*L).top = if t == recorder::KEEP_TOP { keep } else { base.add(t as usize) };
        }
        return resume_at(resume);
}

/// Called before the single-step interpreter executes instruction `pc` of
/// `p` (frame base `base`) while recording.
pub unsafe fn record_step(L: *mut lua_State, p: *mut Proto, pc: u32, base: StkId) {
    let result = RECORDER.with(|r| {
        let mut r = r.borrow_mut();
        let (rec, _) = r.as_mut().expect("recording");
        if rec.paused_depth().is_some_and(|d| ci_depth(L) > d) {
            // Lua code a recorded C call runs: not part of the trace
            return None;
        }
        if !rec.expects(p, base) {
            // a call, a return, or a stack reallocation: leave it to the
            // interpreter
            return Some(recorder::Step::Abort(trace::AbortReason::LeftFrame));
        }
        match rec.step(pc, base) {
            recorder::Step::Continue => None,
            other => Some(other),
        }
    });
    let Some(result) = result else { return };
    let (id, header, proto, side, parent) = RECORDER.with(|r| {
        let r = r.borrow();
        let (rec, _) = r.as_ref().unwrap();
        (rec.id(), rec.header(), rec_proto(rec), rec.is_side(), rec.side_parent())
    });
    RECORDER.with(|r| *r.borrow_mut() = None);
    end_recording();
    match result {
        recorder::Step::Closed(t) => {
            let _ = L;
            finish_trace(t, id, header, proto);
        }
        recorder::Step::Abort(reason) => {
            debug::log(debug::Event::TraceAbort { id: id.0, pc, reason: &reason });
            state::with(|s| {
                if let (Some((ptag, k)), trace::AbortReason::LeftLoop) = (parent, &reason) {
                    if let Some(t) = s.by_tag_mut(ptag) {
                        if let Some(x) = t.exit_tries.get_mut(k as usize) {
                            *x = EXIT_LEAVES;
                        }
                    }
                }
                if !side {
                    if reason == trace::AbortReason::LeftLoop {
                        s.hot.left_loop(proto, header);
                    } else {
                        s.hot.penalize(proto, header);
                    }
                }
                s.note_abort((id, pc, reason));
            });
        }
        recorder::Step::Continue => unreachable!(),
    }
}

/// Optimizes, compiles and installs a recorded trace (a root trace for its
/// loop, or a side trace linked to its parent's exit).
fn finish_trace(mut t: trace::Trace, id: trace::TraceId, header: u32, proto: *mut Proto) {
    let side_of = t.side_of;
    let root_key = (proto as usize, header);
    t.root_key = root_key;
    // how it ends: looping; a side trace by running its root again; a
    // trace ending at another loop's header by running that loop's trace
    // (through a cell: that trace may come and go)
    let link_to = match (side_of, t.link) {
        (_, Some(target)) => {
            let cell = Box::leak(Box::new(0usize)) as *mut usize as usize;
            let re = state::with(|s| {
                s.link_cells.entry(target).or_default().push(cell);
                s.traces.get(&target).and_then(|r| r.native.map(|n| n.reentry))
            });
            unsafe { *(cell as *mut usize) = re.unwrap_or(0) };
            t.link_cell = cell;
            compiler::End::Cell(cell)
        }
        (Some(_), None) => {
            // (decided after optimizing, below: the body re-entry when
            // nothing the side trace does breaks the root's pre-loop work)
            match state::with(|s| s.traces.get(&root_key).and_then(|r| r.native.map(|n| n.reentry))) {
                Some(r) => compiler::End::Root(r),
                None => return,
            }
        }
        (None, None) => compiler::End::Loop,
    };
    let Some(tag) = state::with(|s| s.new_tag()) else { return };
    let prepare = |t: &mut trace::Trace| {
        optimizer::optimize(t);
        t.tag = tag;
        let n = t.snapshots.len().max(1);
        t.link_table = Box::leak(vec![0usize; n].into_boxed_slice()).as_mut_ptr() as usize;
        t.exit_counts = vec![0; n];
        t.exit_tries = vec![0; n];
        t.exit_seen = Box::leak(vec![0u8; n].into_boxed_slice()).as_mut_ptr();
        // (exiting where a link has no trace: nothing to continue)
        if t.link.is_some() {
            t.exit_tries[n - 1] = EXIT_LEAVES;
        }
    };
    prepare(&mut t);
    let _ = debug::dump_ir(&t);
    let mut link_to = link_to;
    if side_of.is_some() {
        // a side trace spills above its root's slots, and may run the root's
        // body again directly
        let root = state::with(|s| s.traces.get(&root_key).map(|r| (r.native, r.reads.clone(), r.phi_slots.clone(), r.layout_hoisted, r.mem_hoisted)));
        if let Some((Some(n), reads, phis, layout, mem)) = root {
            t.spill_base = n.reserved;
            if t.link.is_none() && n.body_entry != 0 && std::env::var_os("LUAU_TRACE_NOBODY").is_none() && body_reentry_ok(&t, &reads, &phis, layout, mem) {
                link_to = compiler::End::Root(n.body_entry);
            }
        }
    } else {
        root_facts(&mut t);
    }
    let mut compiled = compiler::compile(&t, link_to);
    if compiled.is_err() {
        if let Some(mut fb) = t.fallback.take() {
            prepare(&mut fb);
            root_facts(&mut fb);
            if let Ok(c) = compiler::compile(&fb, link_to) {
                fb.side_of = side_of;
                t = *fb;
                compiled = Ok(c);
            }
        }
    }
    t.fallback = None;
    let native = match compiled {
        Ok(n) => n,
        Err(e) => {
            let reason = trace::AbortReason::Internal(format!("compile: {}", e));
            debug::log(debug::Event::TraceAbort { id: id.0, pc: header, reason: &reason });
            state::with(|s| {
                // (a root that doesn't compile waits longer to be recorded
                // again; a side trace's exit has its own retry limit)
                if side_of.is_none() {
                    s.hot.penalize(proto, header);
                }
                s.note_abort((id, header, reason));
            });
            return;
        }
    };
    t.native = Some(native);
    debug::log(debug::Event::TraceCompiled { id: id.0, pc: header, insts: t.len() });
    state::with(|s| match side_of {
        None => {
            t.exit_pcs = t
                .snapshots
                .iter()
                .enumerate()
                .map(|(k, sn)| {
                    let leaves = sn.pc < header || sn.pc > t.end_pc;
                    let loop_end = sn.pc == header && k != 0;
                    let simple = sn.frames.is_empty() && sn.top.is_none() && (leaves || loop_end);
                    sn.pc | if simple { trace::FAST_EXIT } else { 0 }
                })
                .collect();
            s.remove_root(root_key);
            s.by_tag.insert(tag, state::TraceKey::Root(root_key.0, root_key.1));
            s.traces.insert(root_key, t);
            // traces linking to this loop run this one from now on
            s.set_links(root_key, native.reentry);
        }
        Some((ptag, k)) => {
            // link: the parent's exit stub jumps here from now on
            let Some(parent) = s.by_tag_mut(ptag) else { return };
            let slot = (parent.link_table as *mut usize).wrapping_add(k as usize);
            unsafe { *slot = native.entry };
            s.by_tag.insert(tag, state::TraceKey::Side(tag));
            s.sides.insert(tag, t);
            if let Some(r) = s.traces.get_mut(&root_key) {
                r.sides.push(tag);
            }
        }
    });
}

/// What a root trace's body re-entry relies on (see `x64::compile`):
/// the slots it read before the loop, its PHIs' slots, whether it hoisted
/// table layout checks / memory reads.
fn root_facts(t: &mut trace::Trace) {
    use ir::instruction::Op;
    let Some(l) = t.loop_ref else { return };
    let l = l.index();
    let mut reads = Vec::new();
    let (mut layout, mut mem) = (false, false);
    for x in &t.insts[..l] {
        match x.op {
            Op::SLoad { slot, .. } => reads.push(slot),
            Op::HRef { .. } | Op::HSlot { .. } | Op::ASize(_) | Op::ABase(_) | Op::ARef { .. } => layout = true,
            Op::XLoad { .. } | Op::TMeta { .. } | Op::ULoad { by_ref: true, .. } | Op::GuardFlag { .. } | Op::GuardPlainTable { .. } | Op::HMiss { .. } | Op::TGet { .. } | Op::TLen { .. } => mem = true,
            _ => {}
        }
    }
    let mut phis = Vec::new();
    for x in &t.insts[l + 1..] {
        match x.op {
            Op::Phi { pre, .. } => {
                match t.inst(pre).op {
                    Op::SLoad { slot, .. } => phis.push(slot),
                    // (a loop counter's integer twin: its slot)
                    Op::NumToI32 { a, .. } | Op::NumToI64 { a, .. } => {
                        if let Op::SLoad { slot, .. } = t.inst(a).op {
                            phis.push(slot);
                        }
                    }
                    _ => {}
                }
            }
            Op::Nop => {}
            _ => break,
        }
    }
    t.reads = reads;
    t.phi_slots = phis;
    t.layout_hoisted = layout;
    t.mem_hoisted = mem;
}

/// Whether side trace `t` may end in its root's body re-entry: it writes
/// no slot the root read before the loop (other than its PHIs'), calls no
/// Lua code, moves no table storage when the root hoisted layout checks,
/// and writes no memory when the root hoisted reads.
fn body_reentry_ok(t: &trace::Trace, reads: &[u16], phis: &[u16], layout: bool, mem: bool) -> bool {
    use ir::instruction::Op;
    use luau_common::bytecode::BuiltinFunction as B;
    let Some(last) = t.snapshots.last() else { return false };
    if last.entries.iter().any(|e| reads.contains(&e.slot) && !phis.contains(&e.slot)) {
        return false;
    }
    for x in &t.insts {
        if matches!(x.op, Op::CallC { .. }) {
            return false;
        }
        if layout && x.moves_tables() {
            return false;
        }
        let writes = matches!(x.op, Op::XStore { .. } | Op::TSet { .. } | Op::UStore { .. } | Op::CloseUpvals { .. })
            || matches!(x.op, Op::Builtin { bfid, .. } if bfid == B::Rawset as u8 || bfid == B::TableInsert as u8 || bfid == B::Setmetatable as u8);
        if mem && writes {
            return false;
        }
    }
    true
}

fn rec_proto(r: &recorder::Recorder) -> *mut Proto {
    r.proto()
}

// ---------------------------------------------------------------------------
// Native code (method JIT) integration.
//
// With native code, the method JIT keeps running everything; each compiled
// loop instruction (FORNLOOP, JUMPBACK) decrements a counter and, at zero,
// calls `native_loop_hook`. The hook runs the loop instruction, then the
// loop's trace when there is one, or starts recording and runs the
// single-step interpreter for the iteration being recorded. Whenever the
// interpreter has to take over (recording, a trace exit in the middle of the
// body), it runs only until the frame reaches a pc native code can enter.

/// The threaded interpreter calls `threaded_backedge` at back edges (set
/// once tracing is on).
pub static TRACE_HOOKS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    /// back edges until the threaded interpreter looks at the tracing JIT
    static TICK: Cell<u32> = const { Cell::new(16) };
    /// the innermost single-step run stops as soon as the recording ends
    /// (not only where native code can take over)
    static STOP_ANY: Cell<bool> = const { Cell::new(false) };
    /// CallInfo depth the innermost single-step run stops at (-1: none)
    static STOP_DEPTH: Cell<isize> = const { Cell::new(-1) };
    /// the thread (lua_State) that run belongs to: other threads it
    /// resumes (coroutines) run on without stopping
    static STOP_L: Cell<usize> = const { Cell::new(0) };
    /// the single-step run stopped (instead of leaving the VM)
    static STOPPED: Cell<bool> = const { Cell::new(false) };
}

/// Exits through one snapshot that take the full path before native code's
/// cached entry handles them itself.
const FAST_AFTER: u8 = 40;

/// Iterations a numeric loop must have left for native code to enter its
/// trace (or record one).
const SHORT_TRIP: u32 = 32;

/// A new hot counter for one compiled loop instruction (lives as long as
/// the code).
pub fn new_loop_counter() -> *mut u32 {
    Box::leak(Box::new(LoopCell { ctr: 1, entries: 0, cycles: 0, entry: 0, exits: std::ptr::null(), seen: std::ptr::null_mut(), nexits: 0, tag: 0, need: 0, gen: 0 })) as *mut LoopCell as *mut u32
}

/// Per compiled loop instruction: the counter native code decrements
/// (first field), and how long its trace runs per entry from native code.
#[repr(C)]
struct LoopCell {
    ctr: u32,
    entries: u32,
    cycles: u64,
    /// cached root trace of the loop (valid while `gen` is current)
    entry: usize,
    exits: *const u32,
    /// per exit: times seen on the full path (the fast path only after
    /// FAST_AFTER: side traces may continue those exits)
    seen: *mut u8,
    nexits: u32,
    tag: u16,
    need: u32,
    gen: u64,
}

/// Cycles a trace must run per entry, on average, to be entered from
/// native code: entering (through the hook) costs a few hundred cycles, so
/// a loop that only runs a few iterations per entry is faster left native.
const MIN_TRACE_CYCLES: u64 = 800;

thread_local! {
    /// cycles the last trace run took (rdtsc)
    static LAST_RUN: Cell<u64> = const { Cell::new(u64::MAX) };
}

#[inline(always)]
fn cycles() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}

/// True when the single-step interpreter must call into the JIT before each
/// instruction (recording, or a run that stops at native code).
#[inline(always)]
pub fn ss_hooks() -> bool {
    RECORDING.with(|r| r.get()) || STOP_DEPTH.with(|d| d.get()) >= 0
}

#[inline(always)]
unsafe fn ci_depth(L: *mut lua_State) -> isize {
    (*L).ci.offset_from((*L).base_ci)
}

/// Single-step interpreter, before instruction `pc` of `p`: true when the
/// run should stop and return to native code.
pub unsafe fn should_stop(L: *mut lua_State, p: *mut Proto, pc: *const crate::lobject::Instruction) -> bool {
    let d = STOP_DEPTH.with(|d| d.get());
    if d < 0 || STOP_L.with(|l| l.get()) != L as usize {
        return false;
    }
    let cur = ci_depth(L);
    let stop = if cur < d {
        // the frame returned: its caller continues in the interpreter
        true
    } else {
        cur == d && !is_recording() && (STOP_ANY.with(|s| s.get()) || native_entry(p, pc))
    };
    if stop {
        STOPPED.with(|s| s.set(true));
    }
    stop
}

#[cfg(all(target_arch = "x86_64", windows))]
unsafe fn native_entry(p: *mut Proto, pc: *const crate::lobject::Instruction) -> bool {
    crate::lvmexecute::native_addr(p, pc) != 0
}
#[cfg(not(all(target_arch = "x86_64", windows)))]
unsafe fn native_entry(_p: *mut Proto, _pc: *const crate::lobject::Instruction) -> bool {
    false
}

struct StopGuard(isize, usize);
impl StopGuard {
    /// Stops single-step runs of `L` at CallInfo depth `depth` until dropped.
    fn set(L: *mut lua_State, depth: isize) -> StopGuard {
        StopGuard(STOP_DEPTH.with(|d| d.replace(depth)), STOP_L.with(|l| l.replace(L as usize)))
    }
}
impl Drop for StopGuard {
    fn drop(&mut self) {
        STOP_L.with(|l| l.set(self.1));
        STOP_DEPTH.with(|d| d.set(self.0));
    }
}

/// Runs the current frame from `pc` in the single-step interpreter until
/// it can continue natively. Returns what `native_loop_hook` returns.
#[cfg(all(target_arch = "x86_64", windows))]
unsafe fn run_until_native(L: *mut lua_State, pc: *const crate::lobject::Instruction, depth: isize) -> usize {
    (*(*L).ci).savedpc = pc;
    let _g = StopGuard::set(L, depth);
    STOPPED.with(|s| s.set(false));
    crate::lvmexecute::execute_single_step(L);
    if STOPPED.with(|s| s.replace(false)) {
        if ci_depth(L) == depth {
            (*(*L).ci).savedpc as usize
        } else {
            1
        }
    } else {
        // returned to C, yielded, or broke
        0
    }
}

/// A taken back edge to `header` in the threaded interpreter (tracing on;
/// sampled). Returns 0: go on; 1: continue at L->ci's savedpc (frames
/// changed, or a recording just ran); 2: the VM was left; 3 + pc: continue
/// at pc in this function.
pub unsafe fn threaded_backedge(L: *mut lua_State, p: *mut Proto, header: u32, base: StkId) -> u32 {
    let t = TICK.with(|t| t.get());
    if t > 1 {
        TICK.with(|c| c.set(t - 1));
        return 0;
    }
    static EVERY: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    TICK.with(|c| c.set(*EVERY.get_or_init(|| std::env::var("LUAU_TRACE_TICK").ok().and_then(|v| v.parse().ok()).unwrap_or(16))));
    if is_recording() {
        return 0;
    }
    // (a function with native code gets here when its native code left
    // for the interpreter mid-loop: the loop then runs here until the
    // frame changes, so it is traced from here too. Native code's own
    // loop instructions run as steps, which never get here: IN_STEP)
    let depth = ci_depth(L);
    let r = on_backedge(L, p, header, base);
    if is_recording() {
        // a recording started (of the loop, or of a side trace from where
        // its trace exited): the single-step interpreter runs until it ends
        match r {
            // (frames pushed: L->ci's savedpc is set)
            Some(RESUME_FRAME) => {}
            Some(pc) => (*(*L).ci).savedpc = (*p).code.add(pc as usize),
            None => (*(*L).ci).savedpc = (*p).code.add(header as usize),
        }
    }
    match r {
        _ if is_recording() => {
            let _g = StopAnyGuard(STOP_ANY.with(|s| s.replace(true)));
            let _d = StopGuard::set(L, depth);
            STOPPED.with(|s| s.set(false));
            crate::lvmexecute::execute_single_step(L);
            if STOPPED.with(|s| s.replace(false)) {
                1
            } else {
                2
            }
        }
        Some(RESUME_FRAME) => 1,
        Some(LEFT_VM) => 2,
        Some(pc) => 3 + pc,
        None => 0,
    }
}

struct StopAnyGuard(bool);
impl Drop for StopAnyGuard {
    fn drop(&mut self) {
        STOP_ANY.with(|s| s.set(self.0));
    }
}

/// Called by native code when a loop instruction's counter (`ctr`) reaches
/// zero; the frame is on the stack. Returns 0 when the VM was left, 1 to
/// interpret the frame on top at its savedpc, or the pc to continue at in
/// this function.
#[cfg(all(target_arch = "x86_64", windows))]
pub unsafe extern "sysv64-unwind" fn native_loop_hook(
    L: *mut lua_State,
    pc: *const crate::lobject::Instruction,
    base: StkId,
    k: *mut crate::lobject::TValue,
    ctr: *mut u32,
) -> usize {
    let p = (*(*L).ci).p;
    let insn = *pc;
    let step = crate::lvmexecute::STEPS[(insn & 0xff) as usize].expect("loop instruction step");
    let npc = step(L, pc, base, k);
    if npc.is_null() {
        return 0;
    }
    let code = (*p).code;
    let header = (pc.offset_from(code) + 1 + ((insn as i32) >> 16) as isize) as u32;
    if npc != code.add(header as usize) {
        // the loop ended
        *ctr = 1;
        return npc as usize;
    }
    let base = (*L).base;
    // A numeric loop with few iterations left gains nothing from entering a
    // trace (and the entry costs more than the native iterations): stay
    // native and look again in a while.
    if insn & 0xff == luau_common::bytecode::Opcode::ForNLoop as u32 && !is_recording() {
        let ra = base.add(((insn >> 8) & 0xff) as usize);
        let (limit, stepv, idx) = (crate::lobject::nvalue(ra), crate::lobject::nvalue(ra.add(1)), crate::lobject::nvalue(ra.add(2)));
        let left = ((limit - idx) / stepv).floor();
        if left < SHORT_TRIP as f64 {
            *ctr = 1024;
            return npc as usize;
        }
    }
    let depth = ci_depth(L);
    let cell = &mut *(ctr as *mut LoopCell);
    // the cached trace: run it, and handle the common exits right here
    if cell.entry != 0 && cell.gen == state::generation() && !is_recording() && (*L).stack_last.offset_from(base) >= cell.need as isize {
        let f: compiler::TraceFn = std::mem::transmute(cell.entry);
        let t0 = cycles();
        let ex = f(base, L);
        let ran = cycles().wrapping_sub(t0);
        if ex == trace::EXIT_LEFT_VM {
            return 0;
        }
        let k = ex & 0xffff;
        if ex & 0x8000_0000 == 0 && ((ex >> 16) & 0x7fff) as u16 == cell.tag && k < cell.nexits {
            let e = *cell.exits.add(k as usize);
            let seen = cell.seen.add(k as usize);
            if e & trace::FAST_EXIT != 0 && *seen >= FAST_AFTER {
                debug::log(debug::Event::SideExit { id: 0, snap: ex });
                *ctr = 1;
                note_run(cell, ran);
                let npc = code.add((e & !trace::FAST_EXIT) as usize);
                return if native_entry(p, npc) { npc as usize } else { run_until_native(L, npc, depth) };
            }
        }
        // anything else: the full treatment
        if ex & 0x8000_0000 == 0 && ((ex >> 16) & 0x7fff) as u16 == cell.tag && k < cell.nexits {
            let seen = cell.seen.add(k as usize);
            *seen = (*seen).saturating_add(1);
        }
        LAST_RUN.with(|c| c.set(ran));
        let id = state::with(|s| s.traces.get(&(p as usize, header)).map(|t| t.id)).unwrap_or(trace::TraceId(0));
        let resume = handle_exit(L, p, header, base, id, ex, false).unwrap_or(header);
        *ctr = 1;
        note_run(cell, ran);
        return if resume == RESUME_FRAME {
            run_until_native(L, (*(*L).ci).savedpc, depth)
        } else {
            let npc = code.add(resume as usize);
            if is_recording() || !native_entry(p, npc) { run_until_native(L, npc, depth) } else { npc as usize }
        };
    }
    LAST_RUN.with(|c| c.set(u64::MAX));
    let resume = on_backedge(L, p, header, base).unwrap_or(header);
    if resume == LEFT_VM {
        return 0;
    }
    let ran = LAST_RUN.with(|c| c.get());
    // cache the loop's trace for the next entries
    state::with(|s| {
        if let Some(t) = s.traces.get(&(p as usize, header)) {
            if let Some(n) = t.native {
                cell.entry = n.entry;
                cell.exits = t.exit_pcs.as_ptr();
                cell.seen = t.exit_seen;
                cell.nexits = t.exit_pcs.len() as u32;
                cell.tag = t.tag;
                cell.need = t.stack_need;
                cell.gen = state::generation();
            }
        }
    });
    let r = if resume == RESUME_FRAME {
        // inside inlined calls: interpret until back in this frame
        run_until_native(L, (*(*L).ci).savedpc, depth)
    } else {
        let npc = code.add(resume as usize);
        if is_recording() || !native_entry(p, npc) { run_until_native(L, npc, depth) } else { npc as usize }
    };
    // when to call again: every time while a trace exists or the loop may
    // still get one; practically never once it's given up on
    let again = state::with(|s| {
        s.traces.contains_key(&(p as usize, header)) || (s.enabled && !s.hot.is_blacklisted(p, header) && s.traces.len() < s.max_traces)
    });
    *ctr = if again { 1 } else { u32::MAX };
    if ran != u64::MAX {
        note_run(&mut *(ctr as *mut LoopCell), ran);
    }
    r
}

/// A trace run from native code took `ran` cycles: is it worth entering
/// from here? (a short loop is left native for a while)
fn note_run(cell: &mut LoopCell, ran: u64) {
    cell.entries += 1;
    cell.cycles += ran;
    if cell.entries >= 64 {
        let short = cell.cycles / (cell.entries as u64) < MIN_TRACE_CYCLES;
        cell.entries = 0;
        cell.cycles = 0;
        static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if short && !*OFF.get_or_init(|| std::env::var_os("LUAU_TRACE_NOCYCLES").is_some()) {
            // stay native (look again in a while)
            cell.ctr = 1 << 16;
        }
    }
}
