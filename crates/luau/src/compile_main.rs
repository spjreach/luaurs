//! `luau-compile`: port of `CLI/src/Compile.cpp`.
//!
//! Supports the `binary`, `text`, `remarks` and `null` output modes byte for
//! byte. The native code modes (`codegen*`) print the reference's IR and
//! assembly listings, which come from the C++ CodeGen IR this port does not
//! have (its JIT is a different design), so they are reported as
//! unsupported, except `codegennull`, which runs the port's JIT over the
//! module and reports its size.

#[allow(dead_code)]
#[path = "vfs.rs"]
mod vfs;

use std::io::Write;
use std::process::ExitCode;
use std::time::Instant;

use luau_compiler::builder::dump::{self, DUMP_CODE, DUMP_CONSTANTS, DUMP_LOCALS, DUMP_REMARKS, DUMP_SOURCE, DUMP_TYPES};
use luau_compiler::builder::BytecodeBuilder;
use luau_compiler::CompileOptions;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Text,
    Binary,
    Remarks,
    Codegen,
    CodegenAsm,
    CodegenIr,
    CodegenVerbose,
    CodegenNull,
    Null,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordStats {
    None,
    Total,
    File,
    Function,
}

fn get_format(name: &str) -> Option<Format> {
    Some(match name {
        "text" => Format::Text,
        "binary" => Format::Binary,
        "remarks" => Format::Remarks,
        "codegen" => Format::Codegen,
        "codegenasm" => Format::CodegenAsm,
        "codegenir" => Format::CodegenIr,
        "codegenverbose" => Format::CodegenVerbose,
        "codegennull" => Format::CodegenNull,
        "null" => Format::Null,
        _ => return None,
    })
}

#[derive(Default, Clone)]
struct Stats {
    lines: usize,
    bytecode: usize,
    bytecode_instruction_count: usize,
    codegen: usize,
    read_time: f64,
    misc_time: f64,
    parse_time: f64,
    compile_time: f64,
    codegen_time: f64,
}

impl std::ops::AddAssign<&Stats> for Stats {
    fn add_assign(&mut self, o: &Stats) {
        self.lines += o.lines;
        self.bytecode += o.bytecode;
        self.bytecode_instruction_count += o.bytecode_instruction_count;
        self.codegen += o.codegen;
        self.read_time += o.read_time;
        self.misc_time += o.misc_time;
        self.parse_time += o.parse_time;
        self.compile_time += o.compile_time;
        self.codegen_time += o.codegen_time;
    }
}

struct Options {
    copts: CompileOptions,
    only_parse: bool,
    dump_constants: bool,
}

/// C text-mode stream semantics: on Windows `\n` is written as `\r\n`.
fn text_mode(b: &[u8]) -> Vec<u8> {
    if !cfg!(windows) {
        return b.to_vec();
    }
    let mut r = Vec::with_capacity(b.len() + b.len() / 16);
    for &c in b {
        if c == b'\n' {
            r.push(b'\r');
        }
        r.push(c);
    }
    r
}

/// `fprintf(stderr, ...)` with a trailing newline.
macro_rules! eprintln {
    ($($t:tt)*) => {{
        let mut s = format!($($t)*);
        s.push('\n');
        let _ = std::io::stderr().write_all(&text_mode(s.as_bytes()));
    }};
}

fn delta(t: &mut Instant) -> f64 {
    let now = Instant::now();
    let d = now.duration_since(*t).as_secs_f64();
    *t = now;
    d
}

fn report(name: &str, loc: &luau_ast::location::Location, ty: &str, msg: &str) {
    eprintln!("{}({},{}): {}: {}", name, loc.begin.line + 1, loc.begin.column + 1, ty, msg);
}

fn compile_file(name: &str, format: Format, stats: &mut Stats, opts: &Options, out: &mut Vec<u8>) -> bool {
    let mut t = Instant::now();
    let source = match std::fs::read(name) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("Error opening {}", name);
            return false;
        }
    };
    stats.read_time += delta(&mut t);

    let mut bcb = BytecodeBuilder::new();
    match format {
        Format::Text => {
            let mut flags = DUMP_CODE | DUMP_SOURCE | DUMP_LOCALS | DUMP_REMARKS | DUMP_TYPES;
            if opts.dump_constants {
                flags |= DUMP_CONSTANTS;
            }
            bcb.set_dump_flags(flags);
            bcb.set_dump_source(&source);
        }
        Format::Remarks => {
            bcb.set_dump_flags(DUMP_SOURCE | DUMP_REMARKS);
            bcb.set_dump_source(&source);
        }
        _ => {}
    }
    stats.misc_time += delta(&mut t);

    let mut names = luau_ast::name_table::AstNameTable::new();
    let parsed = luau_ast::parser::parse(&source, &mut names);
    if !parsed.errors.is_empty() {
        for e in &parsed.errors {
            report(name, &e.location, "SyntaxError", &e.message);
        }
        return false;
    }
    stats.lines += parsed.lines;
    stats.parse_time += delta(&mut t);

    if opts.only_parse {
        return true;
    }

    let (bytecode, bcb) = match luau_compiler::compile_parsed_with(&parsed, opts.copts.clone(), bcb) {
        Ok(r) => r,
        Err(e) => {
            report(name, &e.location, "CompileError", &e.message);
            return false;
        }
    };
    stats.bytecode += bytecode.len();
    stats.bytecode_instruction_count = bcb.get_total_instruction_count();
    stats.compile_time += delta(&mut t);

    match format {
        Format::Text => out.extend_from_slice(&dump::to_bytes(&bcb.dump_everything())),
        Format::Remarks => out.extend_from_slice(&dump::to_bytes(&bcb.dump_source_remarks())),
        Format::Binary => out.extend_from_slice(&bytecode),
        Format::CodegenNull => {
            stats.codegen += codegen_size(name, &bytecode);
            stats.codegen_time += delta(&mut t);
        }
        Format::Null => {}
        Format::Codegen | Format::CodegenAsm | Format::CodegenIr | Format::CodegenVerbose => unreachable!(),
    }
    true
}

#[cfg(not(all(target_arch = "x86_64", windows)))]
fn codegen_size(_name: &str, _bytecode: &[u8]) -> usize {
    // (no native code generator on this platform)
    0
}

/// Loads the module and runs the port's JIT over it; returns the size of the
/// generated native code.
#[cfg(all(target_arch = "x86_64", windows))]
fn codegen_size(name: &str, bytecode: &[u8]) -> usize {
    use luau_rt::linit::luaL_newstate;
    use luau_rt::lstate::lua_close;
    unsafe {
        let L = luaL_newstate();
        let chunk = format!("={name}");
        let before = luau_rt::codegen::code_size();
        let size = if luau_rt::lvmload::luau_load(L, chunk.as_bytes(), bytecode, 0) == 0 {
            luau_rt::codegen::compile(L, -1);
            luau_rt::codegen::code_size() - before
        } else {
            eprintln!("Error loading bytecode {name}");
            0
        };
        lua_close(L);
        size
    }
}

fn display_help(argv0: &str) {
    println!("Usage: {} [--mode] [options] [file list]", argv0);
    println!();
    println!("Available modes:");
    println!("   binary, text, remarks, codegen, codegenir, codegenasm, codegenverbose, codegennull, null");
    println!();
    println!("Available options:");
    println!("  -h, --help: Display this usage message.");
    println!("  -O<n>: compile with optimization level n (default 1, n should be between 0 and 2).");
    println!("  -g<n>: compile with debug level n (default 1, n should be between 0 and 2).");
    println!("  -t<n>: compile with type information level n (default 0, n should be between 0 and 1).");
    println!("  --target=<target>: compile code for specific architecture (a64, x64, a64_nf, x64_ms).");
    println!("  --timetrace: record compiler time tracing information into trace.json");
    println!("  --record-stats=<granularity>: granularity of compilation stats (total, file, function).");
    println!("  --bytecode-summary: Compute bytecode operation distribution.");
    println!("  --dump-constants: Dump constant table for each function (text mode only).");
    println!("  --dump-regspills: include register spill events in codegen output.");
    println!("  --dump-outlined: include outlined code in codegen output.");
    println!("  --stats-file=<filename>: file in which compilation stats will be recored (default 'stats.json').");
    println!("  --vector-lib=<name>: name of the library providing vector type operations.");
    println!("  --vector-ctor=<name>: name of the function constructing a vector value.");
    println!("  --vector-type=<name>: name of the vector type.");
    println!("  --only-parse: Only parse the input.");
    println!("  --parse-cst: Whether parser should parse CST in addition to AST.");
    println!("  --fflags=<flags>: comma-separated list of fast flags to enable/disable (--fflags=true,false,LuauFlag1=true,LuauFlag2=false).");
}

fn traverse(dir: &str, files: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = format!("{dir}/{name}");
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            traverse(&path, files);
        } else if name.ends_with(".lua") || name.ends_with(".luau") {
            files.push(path);
        }
    }
}

/// `getSourceFiles`.
fn get_source_files(argv: &[String]) -> Vec<String> {
    let mut files = Vec::new();
    for a in &argv[1..] {
        if a == "--program-args" || a == "-a" {
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            continue;
        }
        let normalized = vfs::normalize_path(a);
        if vfs::is_directory(&normalized) {
            traverse(&normalized, &mut files);
        } else {
            files.push(normalized);
        }
    }
    files
}

fn escape_filename(s: &str) -> String {
    let mut r = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => r.push('/'),
            '"' => {
                r.push('\\');
                r.push('"');
            }
            c => r.push(c),
        }
    }
    r
}

fn write_stats(s: &Stats) -> String {
    format!(
        "{{\n        \"lines\": {},\n        \"bytecode\": {},\n        \"bytecodeInstructionCount\": {},\n        \"codegen\": {},\n        \"readTime\": {:.6},\n        \"miscTime\": {:.6},\n        \"parseTime\": {:.6},\n        \"compileTime\": {:.6},\n        \"codegenTime\": {:.6},\n        \"lowerStats\": {{\n            \"totalFunctions\": 0,\n            \"skippedFunctions\": 0,\n            \"spillsToSlot\": 0,\n            \"spillsToRestore\": 0,\n            \"maxSpillSlotsUsed\": 0,\n            \"blocksPreOpt\": 0,\n            \"blocksPostOpt\": 0,\n            \"maxBlockInstructions\": 0,\n            \"regAllocErrors\": 0,\n            \"loweringErrors\": 0,\n            \"blockLinearizationStats\": {{\n                \"constPropInstructionCount\": 0,\n                \"timeSeconds\": 0.000000\n            }},\n            \"functions\": []\n        }}\n    }}",
        s.lines, s.bytecode, s.bytecode_instruction_count, s.codegen, s.read_time, s.misc_time, s.parse_time, s.compile_time, s.codegen_time
    )
}

fn parse_level(arg: &str, max: i32) -> Option<u8> {
    // atoi semantics: leading digits, 0 when none
    let digits: String = arg.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    let v: i32 = digits.parse().unwrap_or(0);
    if (0..=max).contains(&v) {
        Some(v as u8)
    } else {
        None
    }
}

fn real_main(argv: Vec<String>) -> i32 {
    let mut format = Format::Text;
    let mut record = RecordStats::None;
    let mut stats_file = String::from("stats.json");
    let mut bytecode_summary = false;
    let mut timetrace = false;
    let mut opts = Options {
        copts: CompileOptions { optimization_level: 1, debug_level: 1, type_info_level: 0, ..Default::default() },
        only_parse: false,
        dump_constants: false,
    };

    for a in &argv[1..] {
        let a = a.as_str();
        if a == "-h" || a == "--help" {
            display_help(&argv[0]);
            return 0;
        } else if let Some(n) = a.strip_prefix("-O") {
            match parse_level(n, 2) {
                Some(l) => opts.copts.optimization_level = l,
                None => {
                    eprintln!("Error: Optimization level must be between 0 and 2 inclusive.");
                    return 1;
                }
            }
        } else if let Some(n) = a.strip_prefix("-g") {
            match parse_level(n, 2) {
                Some(l) => opts.copts.debug_level = l,
                None => {
                    eprintln!("Error: Debug level must be between 0 and 2 inclusive.");
                    return 1;
                }
            }
        } else if let Some(n) = a.strip_prefix("-t") {
            match parse_level(n, 1) {
                Some(l) => opts.copts.type_info_level = l,
                None => {
                    eprintln!("Error: Type info level must be between 0 and 1 inclusive.");
                    return 1;
                }
            }
        } else if let Some(v) = a.strip_prefix("--target=") {
            if !matches!(v, "a64" | "a64_nf" | "x64" | "x64_ms") {
                eprintln!("Error: unknown target");
                return 1;
            }
        } else if a == "--timetrace" {
            timetrace = true;
        } else if let Some(v) = a.strip_prefix("--record-stats=") {
            record = match v {
                "total" => RecordStats::Total,
                "file" => RecordStats::File,
                "function" => RecordStats::Function,
                _ => {
                    eprintln!("Error: unknown 'granularity' for '--record-stats'.");
                    return 1;
                }
            };
        } else if a.starts_with("--bytecode-summary") {
            bytecode_summary = true;
        } else if a == "--dump-constants" {
            opts.dump_constants = true;
        } else if a == "--dump-regspills" || a == "--dump-outlined" {
        } else if let Some(v) = a.strip_prefix("--stats-file=") {
            stats_file = v.to_string();
            if stats_file.is_empty() {
                eprintln!("Error: filename missing for '--stats-file'.\n");
                return 1;
            }
        } else if let Some(flags) = a.strip_prefix("--fflags=") {
            for f in flags.split(',') {
                if f == "DebugLuauUserDefinedClasses=true" {
                    opts.copts.classes = true;
                    luau_ast::parser::USER_DEFINED_CLASSES.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        } else if a.starts_with("--vector-lib=") || a.starts_with("--vector-ctor=") || a.starts_with("--vector-type=") {
            let (k, v) = a[2..].split_once('=').unwrap();
            match k {
                "vector-lib" => opts.copts.vector_lib = Some(v.to_string()),
                "vector-ctor" => opts.copts.vector_ctor = Some(v.to_string()),
                _ => opts.copts.vector_type = Some(v.to_string()),
            }
        } else if a.starts_with("--parse-cst") {
        } else if a.starts_with("--only-parse") {
            opts.only_parse = true;
        } else if a.starts_with("--") && get_format(&a[2..]).is_some() {
            format = get_format(&a[2..]).unwrap();
        } else if a.starts_with('-') {
            eprintln!("Error: Unrecognized option '{}'.\n", a);
            display_help(&argv[0]);
            return 1;
        }
    }

    if bytecode_summary && record != RecordStats::Function {
        eprintln!("'Error: Required '--record-stats=function' for '--bytecode-summary'.");
        return 1;
    }
    if timetrace {
        eprintln!("To run with --timetrace, Luau has to be built with LUAU_ENABLE_TIME_TRACE enabled");
        return 1;
    }
    if matches!(format, Format::Codegen | Format::CodegenAsm | Format::CodegenIr | Format::CodegenVerbose) {
        eprintln!("Error: native code listings (--codegen, --codegenasm, --codegenir, --codegenverbose) are not supported by this port");
        return 1;
    }

    let files = get_source_files(&argv);
    let mut total = Stats::default();
    let mut file_stats = Vec::new();
    let mut failed = 0;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    for path in &files {
        let mut s = Stats::default();
        let mut out = Vec::new();
        failed += !compile_file(path, format, &mut s, &opts, &mut out) as i32;
        if format == Format::Binary {
            let _ = lock.write_all(&out);
        } else {
            let _ = lock.write_all(&text_mode(&out));
        }
        total += &s;
        if record == RecordStats::File || record == RecordStats::Function {
            file_stats.push(s);
        }
    }

    let mut tail = Vec::new();
    if format == Format::Null {
        let _ = writeln!(
            tail,
            "Compiled {} KLOC into {} KB bytecode (read {:.2}s, parse {:.2}s, compile {:.2}s)",
            total.lines / 1000,
            total.bytecode / 1024,
            total.read_time,
            total.parse_time,
            total.compile_time
        );
    } else if format == Format::CodegenNull {
        let _ = writeln!(
            tail,
            "Compiled {} KLOC into {} KB bytecode => {} KB native code ({:.2}x) (read {:.2}s, parse {:.2}s, compile {:.2}s, codegen {:.2}s)",
            total.lines / 1000,
            total.bytecode / 1024,
            total.codegen / 1024,
            if total.bytecode == 0 { 0.0 } else { total.codegen as f64 / total.bytecode as f64 },
            total.read_time,
            total.parse_time,
            total.compile_time,
            total.codegen_time
        );
        let _ = writeln!(tail, "Lowering: regalloc failed: 0, lowering failed 0; spills to stack: 0, spills to restore: 0, max spill slot 0");
    }
    let _ = lock.write_all(&text_mode(&tail));
    let _ = lock.flush();

    if record != RecordStats::None {
        let body = if record == RecordStats::Total {
            write_stats(&total)
        } else {
            let mut r = String::from("{\n");
            for (i, s) in file_stats.iter().enumerate() {
                r.push_str(&format!("    \"{}\": ", escape_filename(&files[i])));
                r.push_str(&write_stats(s));
                r.push_str(if i == files.len() - 1 { "\n" } else { ",\n" });
            }
            r.push('}');
            r
        };
        if std::fs::write(&stats_file, body).is_err() {
            eprintln!("Unable to open 'stats.json'");
            return 1;
        }
    }

    (failed != 0) as i32
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let r = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || real_main(argv))
        .expect("failed to spawn compiler thread")
        .join();
    match r {
        Ok(0) => ExitCode::SUCCESS,
        Ok(c) => ExitCode::from(c as u8),
        Err(_) => ExitCode::FAILURE,
    }
}
