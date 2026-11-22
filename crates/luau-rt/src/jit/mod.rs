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
    static RECORDER: RefCell<Option<(recorder::Recorder, StkId)>> = const { RefCell::new(None) };
    static RECORDING: Cell<bool> = const { Cell::new(false) };
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

/// A taken backward branch to `header` in `p` (single-step interpreter,
/// tracing enabled). Runs the loop's compiled trace if there is one and
/// returns the pc to resume interpreting at; otherwise counts the loop and
/// starts recording when it's hot.
pub unsafe fn on_backedge(L: *mut lua_State, p: *mut Proto, header: u32, base: StkId) -> Option<u32> {
    if is_recording() {
        return None;
    }
    let native = state::with(|s| s.traces.get(&(p as usize, header)).and_then(|t| t.native.map(|n| (n.entry, t.id))));
    if let Some((entry, id)) = native {
        let f: compiler::TraceFn = std::mem::transmute(entry);
        let ex = f(base, L);
        if ex & 0x8000_0000 == 0 {
            debug::log(debug::Event::SideExit { id: id.0, snap: ex });
            // a side exit: counts toward the storm limit (see
            // EXIT_STORM_LIMIT), which drops the trace for a re-record
            let (storm, resume) = state::with(|s| {
                let t = s.traces.get_mut(&(p as usize, header)).expect("trace vanished");
                // Resume where the guard's snapshot says, even when the
                // trace is dropped. Returning `header` here replays the
                // iteration that was about to leave the loop.
                let resume = if ex & 0x8000_0000 != 0 { header } else { t.snapshots[ex as usize].pc };
                // leaving the loop is normal; exits back into the body are
                // what a storm is made of
                if resume >= t.start_pc && resume <= t.end_pc {
                    t.side_exits += 1;
                }
                (t.side_exits >= EXIT_STORM_LIMIT, resume)
            });
            if storm {
                let reason = trace::AbortReason::Internal("side exit storm".into());
                debug::log(debug::Event::TraceAbort { id: id.0, pc: header, reason: &reason });
                state::with(|s| {
                    s.traces.remove(&(p as usize, header));
                    s.hot.penalize(p, header);
                    s.aborts.push((id, header, reason));
                });
                return Some(resume);
            }
            return Some(resume);
        }
        return Some(state::with(|s| {
            let t = &s.traces[&(p as usize, header)];
            if ex & 0x8000_0000 != 0 {
                // interrupted at the back edge: resume at the header
                header
            } else {
                t.snapshots[ex as usize].pc
            }
        }));
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
        let rec = recorder::Recorder::new(id, p, header, base, max_len);
        RECORDER.with(|r| *r.borrow_mut() = Some((rec, base)));
        RECORDING.with(|r| r.set(true));
    }
    None
}

/// Called before the single-step interpreter executes instruction `pc` of
/// `p` (frame base `base`) while recording.
pub unsafe fn record_step(_L: *mut lua_State, p: *mut Proto, pc: u32, base: StkId) {
    let result = RECORDER.with(|r| {
        let mut r = r.borrow_mut();
        let (rec, rbase) = r.as_mut().expect("recording");
        if *rbase != base || rec_proto(rec) != p {
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
    let (id, header, proto) = RECORDER.with(|r| {
        let r = r.borrow();
        let (rec, _) = r.as_ref().unwrap();
        (rec.id(), rec.start_pc(), rec_proto(rec))
    });
    RECORDER.with(|r| *r.borrow_mut() = None);
    RECORDING.with(|r| r.set(false));
    match result {
        recorder::Step::Closed(mut t) => {
            optimizer::optimize(&mut t);
            let _ = debug::dump_ir(&t);
            match compiler::compile(&t) {
                Ok(native) => {
                    t.native = Some(native);
                    debug::log(debug::Event::TraceCompiled { id: id.0, pc: header, insts: t.len() });
                    state::with(|s| s.traces.insert((proto as usize, header), t));
                }
                Err(e) => {
                    let reason = trace::AbortReason::Internal(format!("compile: {}", e));
                    debug::log(debug::Event::TraceAbort { id: id.0, pc: header, reason: &reason });
                    state::with(|s| {
                        s.hot.penalize(proto, header);
                        s.aborts.push((id, header, reason));
                    });
                }
            }
        }
        recorder::Step::Abort(reason) => {
            debug::log(debug::Event::TraceAbort { id: id.0, pc, reason: &reason });
            state::with(|s| {
                s.hot.penalize(proto, header);
                s.aborts.push((id, pc, reason));
            });
        }
        recorder::Step::Continue => unreachable!(),
    }
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

thread_local! {
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
    Box::leak(Box::new(1u32))
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
        cur == d && !is_recording() && native_entry(p, pc)
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
unsafe fn run_until_native(L: *mut lua_State, pc: *const crate::lobject::Instruction) -> usize {
    (*(*L).ci).savedpc = pc;
    let depth = ci_depth(L);
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
    let resume = on_backedge(L, p, header, base).unwrap_or(header);
    let npc = code.add(resume as usize);
    let r = if is_recording() || !native_entry(p, npc) { run_until_native(L, npc) } else { npc as usize };
    // when to call again: every time while a trace exists or the loop may
    // still get one; practically never once it's given up on
    let again = state::with(|s| {
        s.traces.contains_key(&(p as usize, header)) || (s.enabled && !s.hot.is_blacklisted(p, header) && s.traces.len() < s.max_traces)
    });
    *ctr = if again { 1 } else { u32::MAX };
    r
}
