//! Rust port of Luau's CLI (`CLI/src/Repl.cpp`) on top of `luau-rt`:
//! `luau [options] [file list] [-a] [arg list]`.
//!
//! Scripts under a `conformance` directory (or with `LUAU_HARNESS=1`) run
//! through the conformance harness instead (see [`harness`]).

#![allow(non_snake_case)]

mod crashinfo;
mod harness;
mod luaprof;
mod profiler;
mod require;
use luau_rt::ltasklib as task;
mod vfs;

use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;

use luau_compiler::CompileOptions;
use luau_rt::lapi::*;
use luau_rt::laux::*;
use luau_rt::lbaselib::flush_stdout;
use luau_rt::ldebug::lua_debugtrace;
use luau_rt::ldo::lua_resume;
use luau_rt::linit::*;
use luau_rt::lobject::*;
use luau_rt::lstate::lua_close;
use luau_rt::lvmload::luau_load;

#[global_allocator]
static GLOBAL: luau_rt::fastalloc::FastAlloc = luau_rt::fastalloc::FastAlloc;

/// Native stack for the interpreter thread: C-call recursion (metamethods,
/// pcall, require) nests `luau_execute` frames.
const STACK_SIZE: usize = 256 * 1024 * 1024;

pub struct GlobalOptions {
    pub optimization_level: u8,
    pub debug_level: u8,
    pub coverage: bool,
    /// Compile loaded chunks to native code: on by default where the JIT is
    /// supported (x64 Windows), off with `--no-codegen`
    pub codegen: bool,
}

pub static mut GLOBAL_OPTIONS: GlobalOptions = GlobalOptions { optimization_level: 1, debug_level: 1, coverage: false, codegen: false };

/// Compiles the function on top of the stack to native code when
/// `--codegen` is on (like the reference CLI's `Luau::CodeGen::compile`).
pub unsafe fn maybe_codegen(L: *mut lua_State) {
    if GLOBAL_OPTIONS.codegen || std::env::var_os("LUAU_CODEGEN").is_some_and(|v| v == "1") {
        #[cfg(all(target_arch = "x86_64", windows))]
        {
            let (ok, skipped) = luau_rt::codegen::compile(L, -1);
            if std::env::var_os("LUAU_CODEGEN_STATS").is_some() {
                eprintln!("codegen: {} functions compiled, {} skipped", ok, skipped);
            }
        }
    }
}

/// `copts()`.
pub fn copts() -> CompileOptions {
    #[allow(static_mut_refs)]
    let g = unsafe { &GLOBAL_OPTIONS };
    CompileOptions {
        optimization_level: g.optimization_level,
        debug_level: g.debug_level,
        coverage_level: if g.coverage { 2 } else { 0 },
        // LUAU_TYPEINFO=0 matches `luau-compile -t0` (byte comparisons)
        type_info_level: if std::env::var_os("LUAU_TYPEINFO").is_some_and(|v| v == "0") { 0 } else { 1 },
        vector_double: false,
        classes: unsafe { luau_rt::lvmutils::CLASSES_RUNTIME },
        ..Default::default()
    }
}

unsafe fn lua_loadstring(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1).to_vec();
    let chunkname = if lua_isnoneornil(L, 2) { s.clone() } else { luaL_checklstring(L, 2).to_vec() };
    lua_setsafeenv(L, LUA_ENVIRONINDEX, 0);
    let bytecode = luau_compiler::compile_source(&s, copts());
    if luau_load(L, &chunkname, &bytecode, 0) == 0 {
        return 1;
    }
    lua_pushnil(L);
    lua_insert(L, -2);
    2
}

unsafe fn lua_collectgarbage(L: *mut lua_State) -> i32 {
    let option = luaL_optlstring(L, 1, b"collect");
    if option == b"collect" {
        lua_gc(L, LUA_GCCOLLECT, 0);
        return 0;
    }
    if option == b"count" {
        let c = lua_gc(L, LUA_GCCOUNT, 0);
        lua_pushnumber(L, c as f64);
        return 1;
    }
    luaL_error(L, "collectgarbage must be called with 'count' or 'collect'")
}

unsafe fn setup_state(L: *mut lua_State) {
    luaL_openlibs(L);
    let funcs: [luaL_Reg; 2] = [("loadstring", lua_loadstring), ("collectgarbage", lua_collectgarbage)];
    lua_pushvalue(L, LUA_GLOBALSINDEX);
    luaL_register(L, None, &funcs);
    lua_pop(L, 1);
    require::luaopen_require(L);
    luau_rt::linit::luaL_openextlibs(L);
    luaL_sandbox(L);
}

/// `normalizePath` (`CLI/src/FileUtils.cpp`).
pub fn normalize_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let absolute = path.starts_with('/');
    let (prefix, rest) = if path.len() >= 2 && path.as_bytes()[1] == b':' {
        (path[..2].to_string(), &path[2..])
    } else {
        (String::new(), path.as_str())
    };
    let rooted = absolute || !prefix.is_empty() && rest.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if matches!(parts.last(), Some(p) if *p != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let mut out = prefix;
    if rooted {
        out.push('/');
    } else if parts.is_empty() || parts[0] != ".." {
        out.push_str("./");
    }
    out.push_str(&parts.join("/"));
    out
}

fn get_file_path(name: &str) -> Option<String> {
    let is_file = |p: &str| std::path::Path::new(p).is_file();
    if is_file(name) {
        return Some(name.to_string());
    }
    for ext in [".luau", ".lua"] {
        let p = format!("{}{}", name, ext);
        if is_file(&p) {
            return Some(p);
        }
    }
    None
}

unsafe fn report_error(L: *mut lua_State, status: i32) -> String {
    let mut error = if status == LUA_YIELD {
        "thread yielded unexpectedly".to_string()
    } else {
        lua_tostring(L, -1).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default()
    };
    error.push_str("\nstacktrace:\n");
    error.push_str(&String::from_utf8_lossy(&lua_debugtrace(L)));
    error
}

unsafe fn run_file(name: &str, GL: *mut lua_State, program_args: &[String], repl: bool) -> bool {
    let Some(source) = get_file_path(name).and_then(|p| std::fs::read(p).ok()) else {
        eprintln!("Error opening {}", name);
        return false;
    };

    let L = lua_newthread(GL);
    luaL_sandboxthread(L);

    // bench_support.lua reads these from _G; the reference passes them as
    // program arguments, the port also accepts environment variables
    for var in ["LUAU_BENCH_RUNS", "LUAU_BENCH_ONLY"] {
        if let Some(v) = std::env::var_os(var) {
            lua_pushstring(L, &v.to_string_lossy());
            lua_setglobal(L, var);
        }
    }

    let chunkname = format!("@{}", normalize_path(name));
    let precompiled = name.ends_with(".bc");
    let bytecode = if precompiled { source } else { luau_compiler::compile_source(&source, copts()) };
    if let Some(out) = std::env::var_os("LUAU_BCOUT") {
        let _ = std::fs::write(out, &bytecode);
        lua_pop(GL, 1);
        return true;
    }

    let status = if luau_load(L, chunkname.as_bytes(), &bytecode, 0) == 0 {
        maybe_codegen(L);
        for a in program_args {
            lua_pushstring(L, a);
        }
        lua_resume(L, std::ptr::null_mut(), program_args.len() as i32)
    } else {
        LUA_ERRSYNTAX
    };

    flush_stdout();
    let mut ok = status == 0;
    if status != 0 && status != LUA_YIELD {
        let error = report_error(L, status);
        eprint!("{}", error);
    }
    // task.defer/delay/wait and Signal:Wait continue here; a script that
    // never schedules anything is unaffected
    if status == 0 || status == LUA_YIELD {
        let outcome = task::run_scheduler(GL, L);
        flush_stdout();
        match outcome {
            task::MainOutcome::Done => ok = true,
            task::MainOutcome::Error => {
                let error = report_error(L, LUA_ERRRUN);
                lua_pop(L, 1);
                eprint!("{}", error);
                ok = false;
            }
            task::MainOutcome::Unexpected => {
                eprint!("{}", report_error(L, LUA_YIELD));
                ok = false;
            }
        }
    } else {
        task::run_scheduler(GL, L);
    }
    flush_stdout();

    if repl {
        run_repl_impl(L);
    }
    lua_pop(GL, 1);
    ok
}

/// `runCode`: compiles and runs one REPL chunk, printing its results.
unsafe fn run_code(L: *mut lua_State, source: &[u8]) -> Option<String> {
    let bytecode = luau_compiler::compile_source(source, copts());
    if luau_load(L, b"=stdin", &bytecode, 0) != 0 {
        let msg = lua_tostring(L, -1).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
        lua_pop(L, 1);
        return Some(msg);
    }
    let T = lua_newthread(L);
    lua_pushvalue(L, -2);
    lua_remove(L, -3);
    lua_xmove(L, T, 1);
    let status = lua_resume(T, std::ptr::null_mut(), 0);
    let result = if status == 0 {
        let n = lua_gettop(T);
        if n != 0 {
            luaL_checkstack(T, LUA_MINSTACK, "too many results to print");
            lua_getglobal(T, "_PRETTYPRINT");
            if lua_isnil(T, -1) {
                lua_pop(T, 1);
                lua_getglobal(T, "print");
            }
            lua_insert(T, 1);
            lua_pcall(T, n, 0, 0);
        }
        None
    } else {
        Some(report_error(T, status))
    };
    flush_stdout();
    lua_pop(L, 1);
    result
}

unsafe fn run_repl_impl(L: *mut lua_State) {
    let stdin = io::stdin();
    let mut buffer = String::new();
    let mut lines = stdin.lock().lines();
    loop {
        print!("{}", if buffer.is_empty() { "> " } else { ">> " });
        let _ = io::stdout().flush();
        let Some(Ok(line)) = lines.next() else { break };
        if buffer.is_empty() && line.trim().is_empty() {
            continue;
        }
        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(&line);

        // expressions are printed: try `return <buffer>` first
        let ret = format!("return {}", buffer);
        let ret_bc = luau_compiler::compile_source(ret.as_bytes(), copts());
        let source = if ret_bc.first() != Some(&0) { ret } else { buffer.clone() };
        let bc = luau_compiler::compile_source(source.as_bytes(), copts());
        if bc.first() == Some(&0) && String::from_utf8_lossy(&bc[1..]).ends_with("<eof>'") {
            continue; // incomplete input: keep reading
        }
        if let Some(err) = run_code(L, source.as_bytes()) {
            eprintln!("{}", err);
        }
        buffer.clear();
    }
}

fn display_help(argv0: &str) {
    println!("Usage: {} [options] [file list] [-a] [arg list]", argv0);
    println!();
    println!("When file list is omitted, an interactive REPL is started instead.");
    println!();
    println!("Available options:");
    println!("  --coverage: collect code coverage while running the code");
    println!("  -h, --help: Display this usage message.");
    println!("  -i, --interactive: Run an interactive REPL after executing the last script specified.");
    println!("  -O<n>: compile with optimization level n (default 1, n should be between 0 and 2).");
    println!("  -g<n>: compile with debug level n (default 1, n should be between 0 and 2).");
    println!("  --codegen: execute code using native code generation (the default where supported)");
    println!("  --no-codegen: execute code using the interpreter only");
    println!("  --profile[=N]: profile the code using N Hz sampling (default 10000) and output results to profile.out");
    println!("  --program-args,-a: declare start of arguments to be passed to the Luau program");
}

fn repl_main(argv: Vec<String>) -> i32 {
    crashinfo::install();
    let _profiler = std::env::var("LUAU_PROFILE").ok().map(profiler::Profiler::start);
    let mut interactive = false;
    let mut profile = 0;
    let mut no_codegen = false;
    let mut force_codegen = false;
    let mut timetrace = false;
    let mut program_args_at = argv.len();
    let mut files = Vec::new();
    for i in 1..argv.len() {
        let a = argv[i].as_str();
        if a == "-h" || a == "--help" {
            display_help(&argv[0]);
            return 0;
        } else if a == "-i" || a == "--interactive" {
            interactive = true;
        } else if let Some(n) = a.strip_prefix("-O") {
            let level: i32 = n.parse().unwrap_or(0);
            if !(0..=2).contains(&level) {
                eprintln!("Error: Optimization level must be between 0 and 2 inclusive.");
                return 1;
            }
            unsafe { GLOBAL_OPTIONS.optimization_level = level as u8 };
        } else if let Some(n) = a.strip_prefix("-g") {
            let level: i32 = n.parse().unwrap_or(0);
            if !(0..=2).contains(&level) {
                eprintln!("Error: Debug level must be between 0 and 2 inclusive.");
                return 1;
            }
            unsafe { GLOBAL_OPTIONS.debug_level = level as u8 };
        } else if a == "--coverage" {
            unsafe { GLOBAL_OPTIONS.coverage = true };
        } else if let Some(flags) = a.strip_prefix("--fflags=") {
            // only the flags this port implements are recognized
            for f in flags.split(',') {
                if f == "DebugLuauUserDefinedClasses=true" || f == "DebugLuauUserDefinedClassesRuntime=true" {
                    unsafe { luau_rt::lvmutils::CLASSES_RUNTIME = true };
                    luau_ast::parser::USER_DEFINED_CLASSES.store(true, std::sync::atomic::Ordering::Relaxed);
                } else if f == "FixMathNoisePrecision=true" {
                    luau_rt::lmathlib::FIX_MATH_NOISE_PRECISION.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        } else if a == "--codegen" {
            unsafe { GLOBAL_OPTIONS.codegen = true };
            force_codegen = true;
        } else if a == "--no-codegen" {
            no_codegen = true;
        } else if a == "--profile" {
            profile = 10000; // default to 10 KHz
        } else if let Some(n) = a.strip_prefix("--profile=") {
            profile = n.parse().unwrap_or(0);
        } else if a == "--timetrace" {
            timetrace = true;
        } else if a.starts_with("--codegen") || a == "--counters" || a == "--jit-inliner" {
            // accepted for compatibility; no effect in this port
        } else if a == "--program-args" || a == "-a" {
            program_args_at = i + 1;
            break;
        } else if a.starts_with('-') {
            eprintln!("Error: Unrecognized option '{}'.\n", a);
            display_help(&argv[0]);
            return 1;
        } else {
            files.push(a.to_string());
        }
    }
    let program_args: Vec<String> = argv[program_args_at.min(argv.len())..].to_vec();

    // native code is the default wherever the JIT works
    #[cfg(all(target_arch = "x86_64", windows))]
    let jit_ok = luau_rt::codegen::supported();
    #[cfg(not(all(target_arch = "x86_64", windows)))]
    let jit_ok = false;
    if force_codegen && !jit_ok {
        eprintln!("Cannot enable --codegen, native code generation is not supported in current configuration");
        return 1;
    }
    // LUAU_CODEGEN=0 forces the interpreter (test scripts)
    let env_off = std::env::var_os("LUAU_CODEGEN").is_some_and(|v| v == "0");
    unsafe { GLOBAL_OPTIONS.codegen = !no_codegen && !env_off && jit_ok };

    if timetrace {
        eprintln!("To run with --timetrace, Luau has to be built with LUAU_ENABLE_TIME_TRACE enabled");
        return 1;
    }

    // conformance scripts run under the test harness
    let harness = std::env::var_os("LUAU_HARNESS").map_or(false, |v| v == "1");
    if files.len() == 1 && (harness || files[0].replace('\\', "/").contains("/conformance/")) {
        return harness::run_conformance(&files[0]);
    }

    unsafe {
        let L = luaL_newstate();
        setup_state(L);
        if files.is_empty() {
            if io::stdin().is_terminal() {
                println!("Luau (Rust port)");
            }
            luaL_sandboxthread(L);
            run_repl_impl(L);
            lua_close(L);
            return 0;
        }
        if profile != 0 {
            luaprof::start(L, profile);
        }
        let mut failed = 0;
        for (i, f) in files.iter().enumerate() {
            let last = i == files.len() - 1;
            failed += (!run_file(f, L, &program_args, interactive && last)) as i32;
        }
        flush_stdout();
        if profile != 0 {
            luaprof::stop();
            luaprof::dump("profile.out");
        }
        lua_close(L);
        if failed != 0 {
            1
        } else {
            0
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let result = std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || repl_main(argv))
        .expect("failed to spawn interpreter thread")
        .join();
    match result {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code as u8),
        Err(_) => {
            flush_stdout();
            eprintln!("luau: interpreter panicked");
            ExitCode::FAILURE
        }
    }
}
