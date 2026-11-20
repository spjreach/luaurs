//! Optional tracing-JIT diagnostics (`LUAU_TRACE_LOG`, `LUAU_TRACE_DUMP`).

use std::fs;
use std::io;
use std::path::PathBuf;

use super::config::TraceConfig;
use super::trace::{AbortReason, Trace};

/// A stable diagnostic event emitted by the recorder/compiler.
#[derive(Clone, Copy, Debug)]
pub enum Event<'a> {
    TraceStart {
        id: u32,
        pc: u32,
    },
    TraceAbort {
        id: u32,
        pc: u32,
        reason: &'a AbortReason,
    },
    TraceCompiled {
        id: u32,
        pc: u32,
        insts: usize,
    },
    SideExit {
        id: u32,
        snap: u32,
    },
}

impl Event<'_> {
    /// Format the machine-readable line without the `[TRACE]` prefix.
    pub fn format(self) -> String {
        match self {
            Event::TraceStart { id, pc } => format!("start id={id} pc={pc}"),
            Event::TraceAbort { pc, reason, .. } => {
                format!("abort pc={pc} reason={}", abort_name(reason))
            }
            Event::TraceCompiled { id, pc, insts } => {
                format!("compiled id={id} pc={pc} insts={insts}")
            }
            Event::SideExit { id, snap } => format!("side_exit id={id} snap={snap}"),
        }
    }
}

/// Emit an event to stderr when `LUAU_TRACE_LOG` is enabled.
pub fn log(event: Event<'_>) {
    if TraceConfig::global().log {
        eprintln!("[TRACE] {}", event.format());
    }
}

/// Write the IR to `trace-N.ir` when `LUAU_TRACE_DUMP` names a directory.
///
/// Returns the created path, `Ok(None)` when dumping is disabled, or the I/O
/// error if the directory/file could not be created. The caller decides how
/// to handle a diagnostic failure; it must not change trace execution.
pub fn dump_ir(trace: &Trace) -> io::Result<Option<PathBuf>> {
    let Some(dir) = TraceConfig::global().dump_dir.as_ref() else {
        return Ok(None);
    };
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("trace-{}.ir", trace.id.0));
    fs::write(&path, trace.dump())?;
    Ok(Some(path))
}

fn abort_name(reason: &AbortReason) -> &'static str {
    match reason {
        AbortReason::UnsupportedOpcode(_) => "unsupported_opcode",
        AbortReason::UnsupportedType(_) => "unsupported_type",
        AbortReason::TooLong => "too_long",
        AbortReason::UnsupportedControlFlow => "unsupported_control_flow",
        AbortReason::LeftFrame => "left_frame",
        AbortReason::Internal(_) => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::{AbortReason, Event};

    #[test]
    fn event_lines_have_stable_names_and_fields() {
        assert_eq!(
            Event::TraceStart { id: 2, pc: 17 }.format(),
            "start id=2 pc=17"
        );
        assert_eq!(
            Event::TraceCompiled {
                id: 2,
                pc: 17,
                insts: 8
            }
            .format(),
            "compiled id=2 pc=17 insts=8"
        );
        assert_eq!(
            Event::SideExit { id: 2, snap: 4 }.format(),
            "side_exit id=2 snap=4"
        );
        assert_eq!(
            Event::TraceAbort {
                id: 2,
                pc: 17,
                reason: &AbortReason::TooLong
            }
            .format(),
            "abort pc=17 reason=too_long"
        );
    }

    #[test]
    fn abort_detail_does_not_break_the_stable_log_format() {
        let reason = AbortReason::UnsupportedOpcode("SOME_OPCODE");
        assert_eq!(
            Event::TraceAbort {
                id: 1,
                pc: 3,
                reason: &reason
            }
            .format(),
            "abort pc=3 reason=unsupported_opcode"
        );
    }
}
