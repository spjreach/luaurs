//! Parses every script in Luau's own conformance corpus
//! (`tests/conformance/*.luau`) and asserts the Rust parser
//! accepts them. This is the parser-side conformance check: the C++ reference
//! implementation parses all of these files during `luau-tests`.
//!
//! Scripts using user-defined classes are parsed with
//! `DebugLuauUserDefinedClasses` on, like the conformance harness does.

use std::path::PathBuf;

use luau_ast::name_table::AstNameTable;
use luau_ast::parser::parse;

fn conformance_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance")
}

/// Scripts that need `DebugLuauUserDefinedClasses`.
const CLASS_SCRIPTS: &[&str] = &["classes.luau", "jit_inliner.luau"];

#[test]
fn parses_luau_conformance_corpus() {
    let dir = conformance_dir();
    assert!(dir.is_dir(), "conformance corpus not found at {}", dir.display());

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read conformance dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "luau").unwrap_or(false))
        .collect();
    files.sort();

    assert!(!files.is_empty(), "no conformance scripts found");

    let mut failures = Vec::new();
    let mut parsed = 0;

    for file in &files {
        let name = file.file_name().unwrap().to_string_lossy().to_string();
        let source = std::fs::read(file).expect("read script");
        let mut names = AstNameTable::new();
        luau_ast::parser::USER_DEFINED_CLASSES
            .store(CLASS_SCRIPTS.contains(&name.as_str()), std::sync::atomic::Ordering::Relaxed);
        let result = parse(&source, &mut names);

        if result.errors.is_empty() {
            parsed += 1;
        } else {
            let first = &result.errors[0];
            failures.push(format!(
                "{name}:{}:{}: {}",
                first.location.begin.line + 1,
                first.location.begin.column + 1,
                first.message
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "parser rejected {} of {} conformance scripts:\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );

    // Guard against the corpus silently disappearing.
    assert!(parsed >= 50, "only parsed {parsed} scripts");
}
