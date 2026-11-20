//! Port of Luau's `Config` module: `.luaurc` (JSON-like) parsing
//! (`Config.cpp`), lint names (`LinterConfig.cpp`) and `.config.luau`
//! extraction (`LuauConfig.cpp`).

pub mod linter;
pub mod luau_config;

use std::collections::HashMap;

use luau_ast::lexeme::{LexemeData, LexemeType};
use luau_ast::lexer::Lexer;
use luau_ast::name_table::AstNameTable;

pub use linter::{LintCode, LintOptions};

pub const CONFIG_NAME: &str = ".luaurc";
pub const LUAU_CONFIG_NAME: &str = ".config.luau";

pub type Error = Option<String>;

/// Type checking mode (`Luau::Mode`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    NoCheck,
    Nonstrict,
    Strict,
    Definition,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AliasInfo {
    pub value: String,
    /// where the alias was configured (empty when unknown)
    pub config_location: String,
    /// the alias in its original case
    pub original_case: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub mode: Mode,
    pub enabled_lint: LintOptions,
    pub fatal_lint: LintOptions,
    pub lint_errors: bool,
    pub type_errors: bool,
    pub globals: Vec<String>,
    /// keyed by the lower-cased alias
    pub aliases: HashMap<String, AliasInfo>,
}

impl Default for Config {
    fn default() -> Self {
        let mut enabled_lint = LintOptions::default();
        enabled_lint.set_defaults();
        Config {
            mode: Mode::Nonstrict,
            enabled_lint,
            fatal_lint: LintOptions::default(),
            lint_errors: false,
            type_errors: true,
            globals: Vec::new(),
            aliases: HashMap::new(),
        }
    }
}

fn to_lower(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c }).collect()
}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_alias(&mut self, alias: &str, value: &str) {
        self.aliases
            .insert(to_lower(alias), AliasInfo { value: value.to_string(), config_location: String::new(), original_case: alias.to_string() });
    }

    pub fn set_alias_at(&mut self, alias: &str, value: &str, config_location: &str) {
        self.aliases.insert(
            to_lower(alias),
            AliasInfo { value: value.to_string(), config_location: config_location.to_string(), original_case: alias.to_string() },
        );
    }
}

#[derive(Clone, Debug, Default)]
pub struct AliasOptions {
    pub config_location: Option<String>,
    pub overwrite_aliases: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ConfigOptions {
    pub compat: bool,
    pub alias_options: Option<AliasOptions>,
}

fn parse_boolean(result: &mut bool, value: &str) -> Error {
    match value {
        "true" => *result = true,
        "false" => *result = false,
        _ => return Some(format!("Bad setting '{}'.  Valid options are true and false", value)),
    }
    None
}

pub fn parse_mode_string(mode: &mut Mode, mode_string: &str, compat: bool) -> Error {
    *mode = match mode_string {
        "nocheck" => Mode::NoCheck,
        "strict" => Mode::Strict,
        "nonstrict" => Mode::Nonstrict,
        "noinfer" if compat => Mode::NoCheck,
        _ => return Some(format!("Bad mode \"{}\".  Valid options are nocheck, nonstrict, and strict", mode_string)),
    };
    None
}

fn parse_lint_rule_string_for_code(enabled: &mut LintOptions, fatal: &mut LintOptions, code: u32, value: &str, compat: bool) -> Error {
    match value {
        "true" => enabled.enable_warning(code),
        "false" => enabled.disable_warning(code),
        "enabled" if compat => {
            enabled.enable_warning(code);
            fatal.disable_warning(code);
        }
        "disabled" if compat => {
            enabled.disable_warning(code);
            fatal.disable_warning(code);
        }
        "fatal" if compat => {
            enabled.enable_warning(code);
            fatal.enable_warning(code);
        }
        _ if compat => return Some(format!("Bad setting '{}'.  Valid options are enabled, disabled, and fatal", value)),
        _ => return Some(format!("Bad setting '{}'.  Valid options are true and false", value)),
    }
    None
}

pub fn parse_lint_rule_string(enabled: &mut LintOptions, fatal: &mut LintOptions, warning_name: &str, value: &str, compat: bool) -> Error {
    if warning_name == "*" {
        for code in 0..linter::WARNING_NAMES.len() as u32 {
            if let Some(err) = parse_lint_rule_string_for_code(enabled, fatal, code, value, compat) {
                return Some(format!("In key {}: {}", warning_name, err));
            }
        }
    } else {
        let code = linter::parse_name(warning_name);
        if code == 0 {
            return Some(format!("Unknown lint {}", warning_name));
        }
        if let Some(err) = parse_lint_rule_string_for_code(enabled, fatal, code, value, compat) {
            return Some(format!("In key {}: {}", warning_name, err));
        }
    }
    None
}

pub fn is_valid_alias(alias: &str) -> bool {
    if alias.is_empty() {
        return false;
    }
    if alias == "." || alias == ".." || alias.contains(['\\', '/']) {
        return false;
    }
    for (i, ch) in alias.bytes().enumerate() {
        if i == 0 && ch == b'@' {
            continue;
        }
        if !(ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'_' || ch == b'.') {
            return false;
        }
    }
    true
}

pub fn parse_alias(config: &mut Config, alias_key: &str, alias_value: &str, alias_options: &Option<AliasOptions>) -> Error {
    if !is_valid_alias(alias_key) {
        return Some(format!("Invalid alias {}", alias_key));
    }
    let Some(opts) = alias_options else {
        return Some("Cannot parse aliases without alias options".to_string());
    };
    // (like the C++ code, the lookup uses the key as written)
    if opts.overwrite_aliases || !config.aliases.contains_key(alias_key) {
        match &opts.config_location {
            Some(loc) => config.set_alias_at(alias_key, alias_value, loc),
            None => config.set_alias(alias_key, alias_value),
        }
    }
    None
}

fn next(lexer: &mut Lexer) {
    lexer.next();
    // skip C-style comments as the lexer only understands Lua-style comments
    while lexer.current().ty == LexemeType::FloorDiv {
        lexer.nextline();
    }
}

fn fail(lexer: &Lexer, source: &[u8], message: &str) -> Error {
    let cur = lexer.current();
    Some(format!("Expected {} at line {}, got {} instead", message, cur.location.begin.line + 1, cur.to_string(source)))
}

fn slice(source: &[u8], data: LexemeData) -> String {
    match data {
        LexemeData::Slice { start, length } => String::from_utf8_lossy(&source[start as usize..(start + length) as usize]).into_owned(),
        _ => String::new(),
    }
}

fn parse_json(contents: &str, mut action: impl FnMut(&[String], &str) -> Error) -> Error {
    let source = contents.as_bytes();
    let mut names = AstNameTable::new();
    let mut lexer = Lexer::from_source(source, &mut names);
    next(&mut lexer);

    let mut keys: Vec<String> = Vec::new();
    let mut array_top = false; // nested arrays are not supported
    let is = |lexer: &Lexer, c: u8| lexer.current().ty == LexemeType::Char(c);

    if !is(&lexer, b'{') {
        return fail(&lexer, source, "'{'");
    }
    next(&mut lexer);

    loop {
        if array_top {
            if is(&lexer, b']') {
                next(&mut lexer);
                array_top = false;
                keys.pop();
                if is(&lexer, b',') {
                    next(&mut lexer);
                } else if !is(&lexer, b'}') {
                    return fail(&lexer, source, "',' or '}'");
                }
            } else if lexer.current().ty == LexemeType::QuotedString {
                let value = slice(source, lexer.current().data);
                next(&mut lexer);
                if let Some(err) = action(&keys, &value) {
                    return Some(err);
                }
                if is(&lexer, b',') {
                    next(&mut lexer);
                } else if !is(&lexer, b']') {
                    return fail(&lexer, source, "',' or ']'");
                }
            } else {
                return fail(&lexer, source, "array element or ']'");
            }
        } else if is(&lexer, b'}') {
            next(&mut lexer);
            if keys.is_empty() {
                if lexer.current().ty != LexemeType::Eof {
                    return fail(&lexer, source, "end of file");
                }
                return None;
            }
            keys.pop();
            if is(&lexer, b',') {
                next(&mut lexer);
            } else if !is(&lexer, b'}') {
                return fail(&lexer, source, "',' or '}'");
            }
        } else if lexer.current().ty == LexemeType::QuotedString {
            let key = slice(source, lexer.current().data);
            next(&mut lexer);
            keys.push(key);
            if !is(&lexer, b':') {
                return fail(&lexer, source, "':'");
            }
            next(&mut lexer);
            let ty = lexer.current().ty;
            if is(&lexer, b'{') || is(&lexer, b'[') {
                array_top = is(&lexer, b'[');
                next(&mut lexer);
            } else if ty == LexemeType::QuotedString || ty == LexemeType::Reserved("true") || ty == LexemeType::Reserved("false") {
                let value = match ty {
                    LexemeType::QuotedString => slice(source, lexer.current().data),
                    LexemeType::Reserved("true") => "true".to_string(),
                    _ => "false".to_string(),
                };
                next(&mut lexer);
                if let Some(err) = action(&keys, &value) {
                    return Some(err);
                }
                keys.pop();
                if is(&lexer, b',') {
                    next(&mut lexer);
                } else if !is(&lexer, b'}') {
                    return fail(&lexer, source, "',' or '}'");
                }
            } else {
                return fail(&lexer, source, "field value");
            }
        } else {
            return fail(&lexer, source, "field key");
        }
    }
}

/// Parses the contents of a `.luaurc` file into `config`.
pub fn parse_config(contents: &str, config: &mut Config, options: &ConfigOptions) -> Error {
    parse_json(contents, |keys, value| {
        let k: Vec<&str> = keys.iter().map(String::as_str).collect();
        match k.as_slice() {
            ["languageMode"] => parse_mode_string(&mut config.mode, value, options.compat),
            ["lint", name] => parse_lint_rule_string(&mut config.enabled_lint, &mut config.fatal_lint, name, value, options.compat),
            ["lintErrors"] => parse_boolean(&mut config.lint_errors, value),
            ["typeErrors"] => parse_boolean(&mut config.type_errors, value),
            ["globals"] => {
                config.globals.push(value.to_string());
                None
            }
            ["aliases", name] => parse_alias(config, name, value, &options.alias_options),
            ["language", "mode"] if options.compat => parse_mode_string(&mut config.mode, value, options.compat),
            _ => Some(format!("Unknown key {}", k.join("/"))),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ConfigOptions {
        ConfigOptions { compat: false, alias_options: Some(AliasOptions { config_location: None, overwrite_aliases: false }) }
    }

    #[test]
    fn parses_luaurc() {
        let mut c = Config::new();
        let src = r#"{
            // comment
            "languageMode": "strict",
            "lint": { "*": false, "LocalShadow": true },
            "lintErrors": true,
            "typeErrors": false,
            "globals": ["a", "b"],
            "aliases": { "Dep": "./dep", "other": "@Dep/x" }
        }"#;
        assert_eq!(parse_config(src, &mut c, &opts()), None);
        assert_eq!(c.mode, Mode::Strict);
        assert!(c.lint_errors && !c.type_errors);
        assert_eq!(c.globals, vec!["a", "b"]);
        assert_eq!(c.aliases["dep"].value, "./dep");
        assert_eq!(c.aliases["dep"].original_case, "Dep");
        assert!(c.enabled_lint.is_enabled(linter::parse_name("LocalShadow")));
        assert!(!c.enabled_lint.is_enabled(linter::parse_name("LocalUnused")));
    }

    #[test]
    fn reports_errors() {
        let mut c = Config::new();
        assert_eq!(parse_config("[", &mut c, &opts()), Some("Expected '{' at line 1, got '[' instead".to_string()));
        assert_eq!(parse_config(r#"{"x": true}"#, &mut c, &opts()), Some("Unknown key x".to_string()));
        assert_eq!(parse_config(r#"{"aliases": {"a/b": "./x"}}"#, &mut c, &opts()), Some("Invalid alias a/b".to_string()));
        assert_eq!(
            parse_config(r#"{"languageMode": "bad"}"#, &mut c, &opts()),
            Some("Bad mode \"bad\".  Valid options are nocheck, nonstrict, and strict".to_string())
        );
        assert_eq!(parse_config(r#"{"lint": {"Nope": true}}"#, &mut c, &opts()), Some("Unknown lint Nope".to_string()));
    }

    #[test]
    fn alias_names() {
        assert!(is_valid_alias("my-alias_1.x"));
        assert!(is_valid_alias("@x"));
        assert!(!is_valid_alias(""));
        assert!(!is_valid_alias(".."));
        assert!(!is_valid_alias("a b"));
    }
}
