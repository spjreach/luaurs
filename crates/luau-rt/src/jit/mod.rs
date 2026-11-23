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

/// Recreates the CallInfos of the inlined calls live at snapshot `sn` (the
/// trace already wrote their slots) and points the innermost at `sn.pc`.
unsafe fn push_frames(L: *mut lua_State, base: StkId, sn: &snapshots::Snapshot) {
    use crate::lobject::Closure;
    for f in &sn.frames {
        let caller = (*L).ci;
        (*caller).savedpc = (*(*caller).p).code.add(f.ret_pc as usize);
        let func = base.add(f.base as usize - 1);
        // (the trace wrote the function slot if it set it)
        let cl = crate::lobject::clvalue(func);
        let ci = crate::ldo::incr_ci(L);
        (*ci).func = func;
        (*ci).p = Closure::proto(cl);
        (*ci).base = func.add(1);
        (*ci).top = (*ci).base.add(f.nargs as usize + (*cl).stacksize as usize);
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
        let resume_at = move |pc: u32| -> Option<u32> {
            if moved {
                let ci = (*L).ci;
                (*ci).savedpc = (*(*ci).p).code.add(pc as usize);
                Some(RESUME_FRAME)
            } else {
                Some(pc)
            }
        };
        if ex & 0x8000_0000 != 0 {
            // interrupted at the back edge, or GC work is due (traces that
            // allocate exit for it): resume at the header
            if crate::lgc::luaC_needsGC(L) {
                // the collector may shrink (move) the stack: the caller
                // reloads its frame (RESUME_FRAME)
                let ci = (*L).ci;
                (*ci).savedpc = (*(*ci).p).code.add(header as usize);
                crate::lgc::luaC_step(L, true);
                return Some(RESUME_FRAME);
            }
            return resume_at(header);
        }
        let (tag, k) = (((ex >> 16) & 0x7fff) as u16, ex & 0xffff);
        debug::log(debug::Event::SideExit { id: id.0, snap: ex });
        let key = (p as usize, header);
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
            let traceable = in_loop && !leaves && sn.pc != header && t.exit_tries[k as usize] < 3 && nsides < max_sides;
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
            let mut rec = recorder::Recorder::new_side(sid, p, crate::lobject::clvalue((*(*L).ci).func), pc, base, max_len, header, end, (ptag, k), sframes);
            rec.set_state(L);
            RECORDER.with(|r| *r.borrow_mut() = Some((rec, base)));
            begin_recording(L);
        }
        // Resume where the guard's snapshot says, even when the trace is
        // dropped (resuming at `header` would replay the iteration).
        if let Some(sn) = frames {
            push_frames(L, base, &sn);
            if let Some(t) = top {
                (*L).top = base.add(t as usize);
            }
            return Some(RESUME_FRAME);
        }
        if let Some(t) = top {
            (*L).top = base.add(t as usize);
        }
        return resume_at(resume);
}

/// Called before the single-step interpreter executes instruction `pc` of
/// `p` (frame base `base`) while recording.
pub unsafe fn record_step(L: *mut lua_State, p: *mut Proto, pc: u32, base: StkId) {
    let result = RECORDER.with(|r| {
        let mut r = r.borrow_mut();
        let (rec, _) = r.as_mut().expect("recording");
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
    // a side trace ends by running its root again
    let link_to = match side_of {
        Some(_) => match state::with(|s| s.traces.get(&root_key).and_then(|r| r.native.map(|n| n.reentry))) {
            Some(r) => Some(r),
            None => return,
        },
        None => None,
    };
    let Some(tag) = state::with(|s| s.new_tag()) else { return };
    let prepare = |t: &mut trace::Trace| {
        optimizer::optimize(t);
        t.tag = tag;
        let n = t.snapshots.len().max(1);
        t.link_table = Box::leak(vec![0usize; n].into_boxed_slice()).as_mut_ptr() as usize;
        t.exit_counts = vec![0; n];
        t.exit_tries = vec![0; n];
    };
    prepare(&mut t);
    let _ = debug::dump_ir(&t);
    let mut compiled = compiler::compile(&t, link_to);
    if compiled.is_err() {
        if let Some(mut fb) = t.fallback.take() {
            prepare(&mut fb);
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
    /// the single-step run stopped (instead of leaving the VM)
    static STOPPED: Cell<bool> = const { Cell::new(false) };
}

/// Iterations a numeric loop must have left for native code to enter its
/// trace (or record one).
const SHORT_TRIP: u32 = 32;

/// A new hot counter for one compiled loop instruction (lives as long as
/// the code).
pub fn new_loop_counter() -> *mut u32 {
    Box::leak(Box::new(LoopCell { ctr: 1, entries: 0, cycles: 0, entry: 0, exits: std::ptr::null(), nexits: 0, tag: 0, need: 0, gen: 0 })) as *mut LoopCell as *mut u32
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
    if d < 0 {
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

struct StopGuard(isize);
impl Drop for StopGuard {
    fn drop(&mut self) {
        STOP_DEPTH.with(|d| d.set(self.0));
    }
}

/// Runs the current frame from `pc` in the single-step interpreter until
/// it can continue natively. Returns what `native_loop_hook` returns.
#[cfg(all(target_arch = "x86_64", windows))]
unsafe fn run_until_native(L: *mut lua_State, pc: *const crate::lobject::Instruction, depth: isize) -> usize {
    (*(*L).ci).savedpc = pc;
    let _g = StopGuard(STOP_DEPTH.with(|d| d.replace(depth)));
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
    // Only functions without native code: one with native code is in the
    // threaded interpreter just for a while (after a bail) and is traced
    // from its native loops; mixing trace exits with native re-entry
    // mid-function there was found to corrupt state.
    #[cfg(all(target_arch = "x86_64", windows))]
    if !(*p).execdata.is_null() {
        return 0;
    }
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
            let _d = StopGuard(STOP_DEPTH.with(|d| d.replace(depth)));
            STOPPED.with(|s| s.set(false));
            crate::lvmexecute::execute_single_step(L);
            if STOPPED.with(|s| s.replace(false)) {
                1
            } else {
                2
            }
        }
        Some(RESUME_FRAME) => 1,
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
        let k = ex & 0xffff;
        if ex & 0x8000_0000 == 0 && ((ex >> 16) & 0x7fff) as u16 == cell.tag && k < cell.nexits {
            let e = *cell.exits.add(k as usize);
            if e & trace::FAST_EXIT != 0 {
                debug::log(debug::Event::SideExit { id: 0, snap: ex });
                *ctr = 1;
                note_run(cell, ran);
                let npc = code.add((e & !trace::FAST_EXIT) as usize);
                return if native_entry(p, npc) { npc as usize } else { run_until_native(L, npc, depth) };
            }
        }
        // anything else: the full treatment
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
    let ran = LAST_RUN.with(|c| c.get());
    // cache the loop's trace for the next entries
    state::with(|s| {
        if let Some(t) = s.traces.get(&(p as usize, header)) {
            if let Some(n) = t.native {
                cell.entry = n.entry;
                cell.exits = t.exit_pcs.as_ptr();
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
