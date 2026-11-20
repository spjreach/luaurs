//! Environment-driven configuration for the tracing JIT.
//!
//! Configuration is captured once when the JIT state is first initialized;
//! changing environment variables afterwards does not affect a running state.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::OnceLock;

use super::hotness::DEFAULT_HOTLOOP;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceConfig {
    /// Whether trace recording and execution are enabled (`LUAU_TRACE`).
    pub enabled: bool,
    /// Emit stable `[TRACE]` event lines (`LUAU_TRACE_LOG`).
    pub log: bool,
    /// Loop back-edge count required before recording (`LUAU_TRACE_HOTLOOP`).
    pub hotloop: u32,
    /// Maximum recorded IR instruction count (`LUAU_TRACE_MAXLEN`).
    pub max_trace_len: usize,
    /// Optional directory for `trace-N.ir` files (`LUAU_TRACE_DUMP`).
    pub dump_dir: Option<PathBuf>,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            log: false,
            hotloop: DEFAULT_HOTLOOP,
            max_trace_len: 4000,
            dump_dir: None,
        }
    }
}

impl TraceConfig {
    /// Read the current environment. Boolean options are enabled by any value
    /// except `0`; unset or non-Unicode values are treated as disabled.
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    fn from_lookup(get: impl Fn(&str) -> Option<OsString>) -> Self {
        let defaults = Self::default();
        Self {
            enabled: get("LUAU_TRACE").is_some_and(|value| value != "0"),
            log: get("LUAU_TRACE_LOG").is_some_and(|value| value != "0"),
            hotloop: get("LUAU_TRACE_HOTLOOP")
                .and_then(|value| value.to_str().and_then(|value| value.parse().ok()))
                .unwrap_or(defaults.hotloop),
            max_trace_len: get("LUAU_TRACE_MAXLEN")
                .and_then(|value| value.to_str().and_then(|value| value.parse().ok()))
                .unwrap_or(defaults.max_trace_len),
            dump_dir: get("LUAU_TRACE_DUMP")
                .filter(|value| value != OsStr::new(""))
                .map(PathBuf::from),
        }
    }

    /// Process-wide environment configuration, initialized on first use.
    pub fn global() -> &'static Self {
        static CONFIG: OnceLock<TraceConfig> = OnceLock::new();
        CONFIG.get_or_init(Self::from_env)
    }
}

#[cfg(test)]
mod tests {
    use super::TraceConfig;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn defaults_disable_tracing_and_diagnostics() {
        let config = TraceConfig::default();
        assert!(!config.enabled);
        assert!(!config.log);
        assert!(config.dump_dir.is_none());
        assert_eq!(config.max_trace_len, 4000);
    }

    #[test]
    fn parses_options_and_uses_defaults_for_invalid_numbers() {
        let values = HashMap::from([
            ("LUAU_TRACE", OsString::from("1")),
            ("LUAU_TRACE_LOG", OsString::from("0")),
            ("LUAU_TRACE_HOTLOOP", OsString::from("12")),
            ("LUAU_TRACE_MAXLEN", OsString::from("invalid")),
            ("LUAU_TRACE_DUMP", OsString::from("trace-dumps")),
        ]);
        let config = TraceConfig::from_lookup(|name| values.get(name).cloned());
        assert!(config.enabled);
        assert!(!config.log);
        assert_eq!(config.hotloop, 12);
        assert_eq!(config.max_trace_len, 4000);
        assert_eq!(config.dump_dir, Some(PathBuf::from("trace-dumps")));
    }

    #[test]
    fn parses_disabled_flags_and_empty_dump_path() {
        let values = HashMap::from([
            ("LUAU_TRACE", OsString::from("0")),
            ("LUAU_TRACE_LOG", OsString::from("0")),
            ("LUAU_TRACE_DUMP", OsString::from("")),
        ]);
        let config = TraceConfig::from_lookup(|name| values.get(name).cloned());
        assert!(!config.enabled);
        assert!(!config.log);
        assert!(config.dump_dir.is_none());
    }

    #[test]
    fn defaults_match_the_hotness_and_recorder_limits() {
        let config = TraceConfig::default();
        assert_eq!(config.hotloop, super::DEFAULT_HOTLOOP);
        assert_eq!(config.max_trace_len, 4000);
    }
}
