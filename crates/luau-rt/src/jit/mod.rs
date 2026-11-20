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
            let storm = state::with(|s| {
                let t = s.traces.get_mut(&(p as usize, header)).expect("trace vanished");
                t.side_exits += 1;
                t.side_exits >= EXIT_STORM_LIMIT
            });
            if storm {
                let reason = trace::AbortReason::Internal("side exit storm".into());
                debug::log(debug::Event::TraceAbort { id: id.0, pc: header, reason: &reason });
                state::with(|s| {
                    s.traces.remove(&(p as usize, header));
                    s.hot.penalize(p, header);
                    s.aborts.push((id, header, reason));
                });
                return Some(header);
            }
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
