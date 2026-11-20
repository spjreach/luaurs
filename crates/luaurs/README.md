<p align="center">
  <img src="https://raw.githubusercontent.com/<you>/luaurs/main/assets/logo.svg" alt="luaurs: Luau in Rust" width="720">
</p>

<p align="center">
  A complete <a href="https://luau.org">Luau</a> implementation written in Rust:
  parser, bytecode compiler, register VM with all standard libraries, and an x64 JIT,
  usable as a command-line tool or embedded as a Rust library.
</p>

---

## Highlights

- **Compatible.** Passes all 59 Luau conformance scripts, both interpreted and JIT-compiled.
  The compiler's bytecode matches the reference C++ compiler at every optimization and debug
  level.
- **Fast.** Measured on the Luau benchmark suite on x64 Windows:
  - the JIT runs in about **0.79×** the time of the C++ JIT overall (geometric mean over 224
    benchmarks) and is faster on about 160 of them;
  - the interpreter runs in about **0.9×** the time of the C++ interpreter.
- **Embeddable.** The `luaurs` crate wraps everything in a small safe API:
  load and run code, exchange values and tables, expose Rust functions.
- **Batteries for game-style scripting.** Optional `task` scheduler, `Signal` objects and
  extra `debug` functions (Roblox-style APIs).

## Command line

```sh
cargo build --release
./target/release/luau script.luau            # run (JIT on by default; --no-codegen for the interpreter)
./target/release/luau                        # REPL
./target/release/luau-compile --text a.luau  # bytecode listing, like the C++ luau-compile
```

Flags follow the C++ `luau` CLI: `-O0..2`, `-g0..2`, `--codegen`, `--no-codegen`,
`--profile`, `--coverage`, `-a` (program arguments) and the rest.

## Embedding in Rust

The crate isn't on crates.io yet. Add it from git, with your repository URL:

```toml
[dependencies]
luaurs = { git = "https://github.com/<you>/luaurs" }
```

```rust
use luaurs::{Function, Luau, Value};

fn main() -> Result<(), luaurs::Error> {
    let lua = Luau::new();

    // run code
    lua.load(r#"print("hello from " .. _VERSION)"#).exec()?;

    // results
    let answer: f64 = lua.load("return 6 * 7").eval()?;

    // Rust functions callable from Luau
    let shout = lua.create_function(|_, args| {
        let s = args.first().and_then(Value::as_str).unwrap_or("").to_uppercase();
        Ok(vec![Value::from(s)])
    });
    lua.globals().set("shout", shout)?;

    // tables both ways
    let config = lua.create_table();
    config.set("width", 1280)?;
    lua.globals().set("config", config)?;

    // Luau functions callable from Rust
    let fib: Function = lua
        .load("local function fib(n) return n < 2 and n or fib(n-1) + fib(n-2) end return fib")
        .eval()?;
    let fib30: f64 = fib.call_get((30,))?;
    println!("{answer} {fib30}");
    Ok(())
}
```

### API at a glance

| | |
|---|---|
| `Luau::new()` / `Luau::with_options(Options { .. })` | a state: JIT on/off, `-O`/`-g` levels, sandboxing, extra libraries |
| `lua.load(src)` → `Chunk` | `.name("x")`, `.exec()`, `.eval::<T>()`, `.call(args)`, `.into_function()`, `.bytecode()` |
| `lua.compile(src)` | Luau bytecode (`Vec<u8>`), loadable with `lua.load(bc).bytecode()` |
| `lua.globals()`, `set_global`, `get_global` | the global table |
| `Table` | `get`, `set` (metamethods apply), `raw_get`, `raw_set`, `len`, `pairs`, `sequence` |
| `Function` | `call(args) -> Vec<Value>`, `call_get::<T>(args)` |
| `lua.create_function(\|lua, args\| ...)` | a Rust closure as a Luau function; returned `Err`s become Luau errors |
| `Value` | `Nil`, `Boolean`, `Number`, `Integer`, `Vector`, `String`, `Table`, `Function`, `Other` |
| `lua.run_scheduler()` | runs pending `task.defer` / `task.delay` / `task.wait` work |

Arguments can be `()`, tuples of up to three values, or a `Vec<Value>`. Results convert into
`f64`, `i64`, `bool`, `String`, `Table`, `Function`, `Value` and `Option<T>`.

For anything lower level, `luaurs::luau_rt` exposes the VM with the familiar C API
(`lua_pushnumber`, `lua_pcall`, ...) and `luaurs::luau_compiler` exposes the compiler.

Run the bundled example with `cargo run --release -p luaurs --example hello`.

## Extra libraries

These are opened by the CLI and by `luaurs` (unless `Options::task_library` is off).
They are not part of standard Luau.

**`task`**: `spawn(f | thread, ...)`, `defer(...)`, `delay(seconds, ...)`,
`wait(seconds?) -> elapsed`, `cancel(thread)`, `desynchronize()` / `synchronize()` (no-ops).
Errors in scheduled threads are reported with a stack trace and don't stop the script.

**`Signal`**: `Signal.new()` returns an object with `:Connect(fn)`, `:Once(fn)`, `:Wait()`,
`:Fire(...)` and `:DisconnectAll()`. Connections have `:Disconnect()` and `.Connected`, and
`typeof(signal) == "Signal"`.

```lua
local done = Signal.new()
task.delay(1, function() done:Fire("ready") end)
print(done:Wait())  --> ready
```

**`debug`**: on top of `info` and `traceback`:
- `newcclosure(f, name?)` and `newlclosure(f, name?)`;
- `setreadonly(t, bool)`;
- `getupvalue`, `setupvalue`, `getlocal`, `setlocal`, `getmetatable`, `setmetatable` and
  `getregistry`.

## Project layout

| Crate | What it is |
|---|---|
| `crates/luau-common` | bytecode format, opcodes, builtin ids, hashing, allocator |
| `crates/luau-ast` | lexer and parser |
| `crates/luau-compiler` | bytecode compiler (constant folding, inlining, loop unrolling, type info) |
| `crates/luau-rt` | VM, GC, coroutines, standard libraries, `task`/`Signal`, x64 JIT (`codegen/`) |
| `crates/luau-config`, `crates/luau-require` | `.luaurc` / `.config.luau` and require-by-string |
| `crates/luaurs` | the embedding API |
| `crates/luau` | the `luau` and `luau-compile` executables |

The JIT translates bytecode straight to x64 machine code, keeping each register's type and
location as it goes. On top of that it does:
- value numbering and common-subexpression elimination;
- keeping loop-carried values in registers across iterations;
- recompiling with type feedback when a speculation fails.

## Building and testing

The workspace builds with stable Rust; `.cargo/config.toml` enables the two unstable
features it uses.

```sh
cargo build --release
cargo test --release          # unit and API tests
bash sweep.sh                 # Luau conformance tests, JIT (LUAU_CODEGEN=0 for the interpreter)
bash tools/jitdiff.sh         # tests/jit: interpreter and JIT must print the same
```

The benchmark tools in `tools/` (`benchrep.sh`, `benchq.sh`) compare against a C++ Luau build;
point `LUAU_REF` at its `luau.exe`.

## Status and limits

- The JIT targets **x64 Windows**. Elsewhere code runs in the interpreter.
- Luau's type checker, linter and autocomplete (`Analysis/`) are not ported. Type
  annotations are parsed and compiled as usual.
- Error messages for syntax errors match the C++ parser's first error.

## License

MIT; see [LICENSE](LICENSE). This project is a port of
[Luau](https://github.com/luau-lang/luau) by Roblox (MIT), which derives from
[Lua 5.1](https://www.lua.org) by Lua.org, PUC-Rio (MIT). The Luau conformance tests and
benchmarks under `tests/conformance` and `bench` come from the Luau repository
(see `tests/LUAU_LICENSE.txt`).
