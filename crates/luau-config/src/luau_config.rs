//! `.config.luau` support (`LuauConfig.cpp`): the configuration is a Luau
//! script, run in a fresh sandboxed VM, that returns a table.

#![allow(non_snake_case)]

use std::time::{Duration, Instant};

use luau_rt::lapi::*;
use luau_rt::ldo::lua_resume;
use luau_rt::linit::{luaL_newstate, luaL_openlibs, luaL_sandbox};
use luau_rt::lobject::*;
use luau_rt::lstate::lua_close;
use luau_rt::lvmload::luau_load;

use crate::{parse_alias, parse_lint_rule_string, parse_mode_string, AliasOptions, Config};

#[derive(Clone, Debug, PartialEq)]
pub enum ConfigTableKey {
    String(String),
    Number(f64),
}

impl ConfigTableKey {
    fn display(&self) -> String {
        match self {
            ConfigTableKey::String(s) => s.clone(),
            ConfigTableKey::Number(n) => format!("{}", n),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConfigValue {
    String(String),
    Number(f64),
    Boolean(bool),
    Table(ConfigTable),
}

/// A table returned by a configuration script (insertion order = traversal
/// order of the Luau table).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConfigTable(pub Vec<(ConfigTableKey, ConfigValue)>);

impl ConfigTable {
    pub fn find(&self, key: &str) -> Option<&ConfigValue> {
        self.0.iter().find(|(k, _)| matches!(k, ConfigTableKey::String(s) if s == key)).map(|(_, v)| v)
    }
}

/// How long a configuration script may run (None: no limit).
#[derive(Clone, Copy, Debug)]
pub struct InterruptCallbacks {
    pub timeout: Option<Duration>,
}

impl Default for InterruptCallbacks {
    fn default() -> Self {
        InterruptCallbacks { timeout: None }
    }
}

/// Serializes the table on top of the stack (popped afterwards).
unsafe fn serialize_table(L: *mut lua_State) -> Result<ConfigTable, String> {
    let mut table = ConfigTable::default();
    let mut result = Ok(());
    lua_pushnil(L);
    while lua_next(L, -2) != 0 {
        let key = match lua_type(L, -2) {
            LUA_TNUMBER => Some(ConfigTableKey::Number(lua_tonumber(L, -2))),
            LUA_TSTRING => Some(ConfigTableKey::String(String::from_utf8_lossy(lua_tostring(L, -2).unwrap_or(b"")).into_owned())),
            _ => None,
        };
        let Some(key) = key else {
            result = Err("configuration table keys must be strings or numbers".to_string());
            lua_pop(L, 2);
            break;
        };
        let value = match lua_type(L, -1) {
            LUA_TNUMBER => Ok(ConfigValue::Number(lua_tonumber(L, -1))),
            LUA_TSTRING => Ok(ConfigValue::String(String::from_utf8_lossy(lua_tostring(L, -1).unwrap_or(b"")).into_owned())),
            LUA_TBOOLEAN => Ok(ConfigValue::Boolean(lua_toboolean(L, -1) != 0)),
            LUA_TTABLE => {
                lua_pushvalue(L, -1); // copy for the recursive call (it pops it)
                serialize_table(L).map(ConfigValue::Table)
            }
            _ => Err(format!("configuration value for key \"{}\" must be a string, number, boolean, or nested table", key.display())),
        };
        match value {
            Ok(v) => table.0.push((key, v)),
            Err(e) => {
                result = Err(e);
                lua_pop(L, 2);
                break;
            }
        }
        lua_pop(L, 1); // value
    }
    lua_pop(L, 1); // table
    result.map(|_| table)
}

unsafe fn interrupt(L: *mut lua_State, _gc: i32) {
    let deadline = lua_getthreaddata(L) as *const Instant;
    if !deadline.is_null() && Instant::now() >= *deadline {
        luau_rt::laux::luaL_error(L, "configuration execution timed out");
    }
}

unsafe fn execute_and_extract(L: *mut lua_State, callbacks: &InterruptCallbacks) -> Result<ConfigTable, String> {
    // the deadline outlives the run: it is only read from the interrupt
    let deadline: Option<Box<Instant>> = callbacks.timeout.map(|t| Box::new(Instant::now() + t));
    if let Some(d) = &deadline {
        lua_setthreaddata(L, &**d as *const Instant as *mut u8);
        (*lua_callbacks(L)).interrupt = Some(interrupt);
    }
    let status = lua_resume(L, std::ptr::null_mut(), 0);
    (*lua_callbacks(L)).interrupt = None;
    lua_setthreaddata(L, std::ptr::null_mut());
    drop(deadline);
    match status {
        LUA_OK => {}
        LUA_BREAK | LUA_YIELD => return Err("configuration execution cannot yield".to_string()),
        _ => return Err(String::from_utf8_lossy(lua_tostring(L, -1).unwrap_or(b"")).into_owned()),
    }
    if lua_gettop(L) != 1 {
        return Err("configuration must return exactly one value".to_string());
    }
    if lua_type(L, -1) != LUA_TTABLE {
        return Err("configuration did not return a table".to_string());
    }
    serialize_table(L)
}

/// Runs a configuration script and returns the table it produces.
pub fn extract_config(source: &str, callbacks: &InterruptCallbacks) -> Result<ConfigTable, String> {
    unsafe {
        let L = luaL_newstate();
        luaL_openlibs(L);
        luaL_sandbox(L);
        let bytecode = luau_compiler::compile_source(source.as_bytes(), luau_compiler::CompileOptions::default());
        let result = if luau_load(L, b"=config", &bytecode, 0) != 0 {
            Err(String::from_utf8_lossy(lua_tostring(L, -1).unwrap_or(b"")).into_owned())
        } else {
            execute_and_extract(L, callbacks)
        };
        lua_close(L);
        result
    }
}

fn create_luau_config_from_luau_table(config: &mut Config, luau_table: &ConfigTable, alias_options: &Option<AliasOptions>) -> Option<String> {
    for (k, v) in &luau_table.0 {
        let ConfigTableKey::String(key) = k else {
            return Some("configuration keys in \"luau\" table must be strings".to_string());
        };
        match key.as_str() {
            "languagemode" => {
                let ConfigValue::String(value) = v else {
                    return Some("configuration value for key \"languagemode\" must be a string".to_string());
                };
                if let Some(e) = parse_mode_string(&mut config.mode, value, false) {
                    return Some(e);
                }
            }
            "lint" => {
                let ConfigValue::Table(lint) = v else {
                    return Some("configuration value for key \"lint\" must be a table".to_string());
                };
                let flag = |b: bool| if b { "true" } else { "false" };
                if let Some(value) = lint.find("*") {
                    let ConfigValue::Boolean(enabled) = value else {
                        return Some("configuration values in \"lint\" table must be booleans".to_string());
                    };
                    if let Some(e) = parse_lint_rule_string(&mut config.enabled_lint, &mut config.fatal_lint, "*", flag(*enabled), false) {
                        return Some(e);
                    }
                }
                for (k, v) in &lint.0 {
                    let ConfigTableKey::String(name) = k else {
                        return Some("configuration keys in \"lint\" table must be strings".to_string());
                    };
                    if name == "*" {
                        continue;
                    }
                    let ConfigValue::Boolean(enabled) = v else {
                        return Some("configuration values in \"lint\" table must be booleans".to_string());
                    };
                    if let Some(e) = parse_lint_rule_string(&mut config.enabled_lint, &mut config.fatal_lint, name, flag(*enabled), false) {
                        return Some(e);
                    }
                }
            }
            "linterrors" => {
                let ConfigValue::Boolean(value) = v else {
                    return Some("configuration value for key \"linterrors\" must be a boolean".to_string());
                };
                config.lint_errors = *value;
            }
            "typeerrors" => {
                let ConfigValue::Boolean(value) = v else {
                    return Some("configuration value for key \"typeerrors\" must be a boolean".to_string());
                };
                config.type_errors = *value;
            }
            "globals" => {
                let ConfigValue::Table(globals_table) = v else {
                    return Some("configuration value for key \"globals\" must be an array of strings".to_string());
                };
                let n = globals_table.0.len();
                let mut globals = vec![String::new(); n];
                for (k, v) in &globals_table.0 {
                    let ConfigTableKey::Number(key) = k else {
                        return Some("configuration array \"globals\" must only have numeric keys".to_string());
                    };
                    let index = *key as usize;
                    if index < 1 || n < index {
                        return Some("configuration array \"globals\" contains invalid numeric key".to_string());
                    }
                    let ConfigValue::String(global) = v else {
                        return Some("configuration value in \"globals\" table must be a string".to_string());
                    };
                    globals[index - 1] = global.clone();
                }
                config.globals = globals;
            }
            "aliases" => {
                let ConfigValue::Table(aliases) = v else {
                    return Some("configuration value for key \"aliases\" must be a table".to_string());
                };
                for (k, v) in &aliases.0 {
                    let ConfigTableKey::String(alias_key) = k else {
                        return Some("configuration keys in \"aliases\" table must be strings".to_string());
                    };
                    let ConfigValue::String(alias_value) = v else {
                        return Some("configuration values in \"aliases\" table must be strings".to_string());
                    };
                    if let Some(e) = parse_alias(config, alias_key, alias_value, alias_options) {
                        return Some(e);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Runs a `.config.luau` script and applies its `luau` table to `config`.
pub fn extract_luau_config(source: &str, config: &mut Config, alias_options: Option<AliasOptions>, callbacks: InterruptCallbacks) -> Option<String> {
    let table = match extract_config(source, &callbacks) {
        Ok(t) => t,
        Err(e) => return Some(e),
    };
    match table.find("luau") {
        None => None,
        Some(ConfigValue::Table(luau_table)) => create_luau_config_from_luau_table(config, luau_table, &alias_options),
        Some(_) => Some("configuration value for key \"luau\" must be a table".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_luau_table() {
        let mut c = Config::new();
        let src = r#"return { luau = { languagemode = "strict", typeerrors = false, globals = {"a", "b"},
            lint = { ["*"] = false, LocalShadow = true }, aliases = { Dep = "./dep" } } }"#;
        let opts = Some(AliasOptions { config_location: None, overwrite_aliases: false });
        assert_eq!(extract_luau_config(src, &mut c, opts, InterruptCallbacks::default()), None);
        assert_eq!(c.mode, crate::Mode::Strict);
        assert!(!c.type_errors);
        assert_eq!(c.globals, vec!["a", "b"]);
        assert_eq!(c.aliases["dep"].value, "./dep");
    }

    #[test]
    fn reports_script_errors() {
        let mut c = Config::new();
        assert_eq!(extract_luau_config("return 1", &mut c, None, InterruptCallbacks::default()), Some("configuration did not return a table".to_string()));
        assert_eq!(
            extract_luau_config("return { luau = 1 }", &mut c, None, InterruptCallbacks::default()),
            Some("configuration value for key \"luau\" must be a table".to_string())
        );
        assert!(extract_luau_config("error('x')", &mut c, None, InterruptCallbacks::default()).is_some());
        let slow = InterruptCallbacks { timeout: Some(Duration::from_millis(50)) };
        assert_eq!(extract_luau_config("while true do end", &mut c, None, slow), Some("configuration execution timed out".to_string()));
    }
}
