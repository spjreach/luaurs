<p align="center">
  <img src="assets/icon.png" alt="luaurs" width="200">
</p>

<h1 align="center">luaurs</h1>

<p align="center">
  <a href="https://luau.org">Luau</a>, rewritten from scratch in Rust, JIT included.
</p>

---

luaurs started as a fun experiment: could the whole Luau toolchain (parser, compiler, VM and
JIT) be written in Rust and come out *faster* than the original C++? It turns out it can.

What you get:

- **The full language.** It passes all 59 of Luau's own conformance tests, with the JIT on and
  with it off, and the compiler produces byte-for-byte the same bytecode as the C++ one.
- **Speed.** On Luau's benchmark suite, the luaurs JIT beats the official C++ JIT on
  **158 of 224 benchmarks** and is about **21% faster overall** (numbers below).
- **A Rust library.** Drop it into your Rust project, run Luau code and pass values back and
  forth in a few lines.
- **Some extras for game-style scripts.** A `task` scheduler, `Signal` objects, a `Regex`
  library and a few more `debug` helpers, in the style of Roblox's APIs.

## Benchmarks

All numbers are from Luau's own benchmark suite (`bench/`), comparing the luaurs JIT against
the C++ Luau JIT (`luau --codegen`). Each benchmark ran 3 times on the same Windows x64 machine
and the best time of each counts. "Speed vs C++" above 1 means luaurs is faster (bold when
it's at least 3% faster).

Over all 224 benchmarks (the ones below plus the micro, GC and "vibemark" sets):

| | |
|---|---|
| luaurs faster | 158 of 224 |
| Overall (geometric mean of time) | luaurs takes **0.79×** the time of C++ |
| Best case | `sha3_int64`: about 5× faster |
| Worst case | `air`, `murmur128_int64`: about 10% slower |

<details>
<summary><b>The main benchmark set, one by one</b></summary>

| Benchmark | C++ Luau JIT (ms) | luaurs JIT (ms) | Speed vs C++ |
|---|--:|--:|--:|
| base64 | 10.80 | 9.02 | **1.20×** |
| chess | 49.80 | 50.82 | 0.98× |
| integers/blake2b_bit32 | 14.21 | 9.30 | **1.53×** |
| integers/blake2b_int64 | 0.84 | 0.68 | **1.22×** |
| integers/murmur128_bit32 | 45.33 | 33.62 | **1.35×** |
| integers/murmur128_int64 | 0.85 | 0.94 | 0.90× |
| integers/pollard_rho_bit32 | 90.26 | 35.13 | **2.57×** |
| integers/pollard_rho_int64 | 1.62 | 1.50 | **1.08×** |
| integers/sha3_bit32 | 20.38 | 10.67 | **1.91×** |
| integers/sha3_int64 | 4.50 | 0.90 | **5.00×** |
| integers/sha512_bit32 | 4.10 | 3.83 | **1.07×** |
| integers/sha512_int64 | 3.02 | 1.48 | **2.05×** |
| integers/xxh64_bit32 | 49.79 | 37.83 | **1.32×** |
| integers/xxh64_int64 | 1.94 | 0.94 | **2.07×** |
| life | 21.30 | 23.17 | 0.92× |
| matrixmult | 7.43 | 5.69 | **1.31×** |
| mesh-normal-scalar | 15.96 | 11.87 | **1.34×** |
| mesh-normal-vector | 6.33 | 5.55 | **1.14×** |
| pcmmix | 0.54 | 0.59 | 0.91× |
| qsort | 41.69 | 39.78 | **1.05×** |
| sha256 | 6.01 | 3.17 | **1.90×** |
| shootout/ack | 27.89 | 26.96 | **1.03×** |
| shootout/binary-trees | 16.72 | 15.63 | **1.07×** |
| shootout/fannkuch-redux | 5.15 | 5.00 | **1.03×** |
| shootout/fixpoint-fact | 38.06 | 38.42 | 0.99× |
| shootout/heapsort | 11.09 | 8.48 | **1.31×** |
| shootout/mandel | 37.22 | 38.06 | 0.98× |
| shootout/n-body | 8.21 | 8.07 | 1.02× |
| shootout/n-body-vector | 4.84 | 5.10 | 0.95× |
| shootout/qt | 29.75 | 25.06 | **1.19×** |
| shootout/queen | 0.75 | 0.56 | **1.35×** |
| shootout/scimark | 21.23 | 19.83 | **1.07×** |
| shootout/spectral-norm | 11.25 | 4.35 | **2.58×** |
| sieve | 109.20 | 98.66 | **1.11×** |
| sunspider/3d-cube | 3.25 | 3.08 | **1.05×** |
| sunspider/3d-morph | 11.99 | 2.53 | **4.74×** |
| sunspider/3d-raytrace | 3.63 | 2.80 | **1.30×** |
| sunspider/controlflow-recursive | 2.95 | 2.35 | **1.25×** |
| sunspider/crypto-aes | 5.67 | 5.69 | 1.00× |
| sunspider/fannkuch | 6.88 | 6.30 | **1.09×** |
| sunspider/math-cordic | 4.46 | 3.11 | **1.43×** |
| sunspider/math-partial-sums | 6.43 | 3.02 | **2.13×** |
| sunspider/n-body-oop | 10.63 | 11.10 | 0.96× |
| tictactoe | 64.49 | 62.28 | **1.04×** |
| trig | 23.70 | 5.77 | **4.11×** |
| vector-math | 2.65 | 1.82 | **1.45×** |
| voxelgen | 39.38 | 35.66 | **1.10×** |
| zefbench/air | 726.20 | 806.95 | 0.90× |
| zefbench/basic | 505.81 | 465.20 | **1.09×** |
| zefbench/cdx | 561.31 | 530.48 | **1.06×** |
| zefbench/richards | 713.79 | 642.14 | **1.11×** |

</details>

Your numbers will differ with your CPU, so run them yourself (see
[Building and testing](#building-and-testing)). Differences of a few percent are within the
noise between runs.

## Using it from the command line

```sh
cargo build --release
./target/release/luau script.luau            # run a script (JIT on; --no-codegen for the interpreter)
./target/release/luau                        # interactive REPL
./target/release/luau-compile --text a.luau  # see the bytecode
```

It takes the same flags as the C++ `luau` tool: `-O0..2`, `-g0..2`, `--codegen`,
`--no-codegen`, `--profile`, `--coverage`, `-a` for script arguments, and so on.

## Using it from Rust

Add it to your `Cargo.toml`:

```toml
[dependencies]
luaurs = { git = "https://github.com/spjreach/luaurs" }
```

Then:

```rust
use luaurs::{Function, Luau, Value};

fn main() -> Result<(), luaurs::Error> {
    let lua = Luau::new();

    // run some code
    lua.load(r#"print("hello from " .. _VERSION)"#).exec()?;

    // get a value back
    let answer: f64 = lua.load("return 6 * 7").eval()?;

    // let Luau call a Rust function
    let shout = lua.create_function(|_, args| {
        let s = args.first().and_then(Value::as_str).unwrap_or("").to_uppercase();
        Ok(vec![Value::from(s)])
    });
    lua.globals().set("shout", shout)?;

    // hand Luau a table
    let config = lua.create_table();
    config.set("width", 1280)?;
    lua.globals().set("config", config)?;

    // call a Luau function from Rust
    let fib: Function = lua
        .load("local function fib(n) return n < 2 and n or fib(n-1) + fib(n-2) end return fib")
        .eval()?;
    let fib30: f64 = fib.call_get((30,))?;

    println!("{answer} {fib30}");
    Ok(())
}
```

Runnable versions are in `crates/luaurs/examples/`:

```sh
cargo run -p luaurs --example hello     # the tour above
cargo run -p luaurs --example compiled  # bytecode, errors, Rust functions
cargo run -p luaurs --example regex     # the Regex library from Rust and Luau
```

### Compiling once and handling errors

`lua.compile` returns bytecode that can be loaded and run as often as you like, with the
usual `Chunk` arguments:

```rust
let bytecode = lua.compile("local a, b = ... return a * b + 1")?;
let results = lua.load(&bytecode).bytecode().call((6, 7))?;
let answer = results.first().and_then(Value::as_number).unwrap_or(0.0);
assert_eq!(answer, 43.0);
```

Runtime errors carry the chunk name and line, and compile errors are returned before any
script runs:

```rust
match lua.load("return missing.value").name("demo").exec() {
    Ok(()) => {}
    Err(error) => eprintln!("runtime: {error}"),
}

if let Err(error) = lua.compile("local = 1") {
    eprintln!("compile: {error}");
}
```

The `task`, `Signal` and `Regex` extras are installed by default, so scripts loaded through
this crate can use them exactly like the CLI does. See the `regex` example for using the
regex engine directly from Rust as well.

### The API in short

| | |
|---|---|
| `Luau::new()` / `Luau::with_options(Options { .. })` | create a state; options turn the JIT, sandboxing and the extra libraries on or off and set `-O`/`-g` levels |
| `lua.load(src)` | gives a `Chunk`: `.name("x")`, `.exec()`, `.eval::<T>()`, `.call(args)`, `.into_function()`, `.bytecode()` |
| `lua.compile(src)` | compile to bytecode (`Vec<u8>`); load it later with `lua.load(bc).bytecode()` |
| `lua.globals()`, `set_global`, `get_global` | the global table |
| `Table` | `get`, `set` (metamethods apply), `raw_get`, `raw_set`, `len`, `pairs`, `sequence` |
| `Function` | `call(args) -> Vec<Value>`, `call_get::<T>(args)` |
| `lua.create_function(\|lua, args\| ...)` | wrap a Rust closure; returning `Err` raises a Luau error |
| `Value` | `Nil`, `Boolean`, `Number`, `Integer`, `Vector`, `String`, `Table`, `Function`, `Other` |
| `lua.run_scheduler()` | run whatever `task.defer` / `task.delay` / `task.wait` left pending |

Arguments can be `()`, tuples of up to three values, or a `Vec<Value>`. Results convert to
`f64`, `i64`, `bool`, `String`, `Table`, `Function`, `Value` or `Option<T>`.

Need to go lower? `luaurs::luau_rt` is the VM with the classic C-style API (`lua_pushnumber`,
`lua_pcall`, ...) and `luaurs::luau_compiler` is the compiler.

## The extras

These aren't part of standard Luau. The CLI always loads them; in the library they're on by
default and `Options::task_library` turns them off.

**`task`**: `spawn(f | thread, ...)`, `defer(...)`, `delay(seconds, ...)`,
`wait(seconds?)` (returns the time waited), `cancel(thread)`, and `desynchronize()` /
`synchronize()`, which do nothing here. If a scheduled thread errors, you get its stack trace
and the rest of the script keeps going.

**`Signal`**: `Signal.new()` gives you an object with `:Connect(fn)`, `:Once(fn)`, `:Wait()`,
`:Fire(...)` and `:DisconnectAll()`. A connection has `:Disconnect()` and `.Connected`, and
`typeof(signal)` is `"Signal"`.

```lua
local done = Signal.new()
task.delay(1, function() done:Fire("ready") end)
print(done:Wait())  --> ready
```

**`Regex`**: a regular-expression engine. Patterns are compiled to a Thompson NFA and run by
a Pike VM, so matching is linear in the size of the input (there is no catastrophic
backtracking). A literal prefix and a first-character class skip regions that cannot start a
match.

`Regex.new(pattern [, flags])` returns a `Regex` object. `flags` is a string made of `i`
(case-insensitive), `m` (multi-line `^`/`$`), `s` (`.` matches a newline) and `x` (ignore
whitespace and `#` comments). Inline flags (`(?i)`, `(?im-sx:...)`) work too.
`Regex.Escape(text)` escapes a string so it can be used literally in a pattern.

| | |
|---|---|
| `re:Match(contents) -> match?` | first match, or `nil` |
| `re:MatchMany(contents [, limit]) -> {match}` | every non-overlapping match |
| `re:Find(contents [, init]) -> match?` | first match at or after the 1-based `init` |
| `re:Test(contents)`, `re:IsMatch(contents)` | `true` when the pattern matches |
| `re:Replace(contents, replacement) -> string` | replace the first match |
| `re:ReplaceAll(contents, replacement) -> string` | replace every match |
| `re:Split(contents [, limit]) -> {string}` | split on matches |
| `re:Count(contents) -> number` | number of matches |
| `re.Pattern`, `re.Flags`, `re.GroupCount`, `re.GroupNames` | pattern information |
| `re:GetPattern()` | the pattern |

A `match` has `.Text` (also `.Value`), `.Start`/`.Finish` (1-based, inclusive, like
`string.find`), `.ByteStart`/`.ByteEnd` (0-based, end exclusive), `.Captures[0..n]`,
`.Starts[0..n]` and `.Ends[0..n]`, plus `:GetCapture(i)`, `:GetStart(i)`, `:GetEnd(i)` and
`:GetText()`. Capture 0 is the whole match. `tostring(match)` returns its text.

`replacement` uses `$0` (whole match), `$1`..`$99`, `${name}` and `$name` for named groups,
and `$$` for a literal `$`. It can also be a function that receives the match and returns
the string to insert. `Replace` also accepts a match object as `contents`.

```lua
local input = "[DEBUG] (ID): 99821"
local label = "[DEBUG] (ID): "

local escaped = Regex.Escape(label)
local regex = Regex.new(escaped .. "(\\d+)")
local match = regex:Match(input)

if match then
    print("ID:", regex:Replace(match, "$1"))  --> ID: 99821
end
```

Supported syntax: literals, `.`, `^`, `$`, `\A`, `\z`, `\Z`, `\b`, `\B`, character classes
(`[...]`, `[^...]`, POSIX `[[:alpha:]]`), escapes (`\n`, `\t`, `\xHH`, `\u{...}`,
`\Q...\E`), `\d \D \w \W \s \S`, Unicode properties (`\p{L}`, `\p{Lu}`, ...), groups
(capturing, `(?:...)`, named `(?<name>...)` / `(?P<name>...)`), alternation and greedy/lazy
quantifiers (`* + ? {n} {n,} {n,m}`). Backreferences and lookaround raise an error instead of
silently doing the wrong thing.

```lua
local regex = Regex.new("(?<word>[A-Za-z]+):(\\d+)")
local m = regex:Match("My Cool Id: 31235335")
if m then
    print(m.Captures[1])                             --> Id
    print(regex:Replace(m, "$2"))                    --> 31235335
    print(regex:ReplaceAll("a:1 b:2", "${word}=$2")) --> a=1 b=2
end
```

**`debug`**: on top of the usual `info` and `traceback`, there are:
- `newcclosure(f, name?)` and `newlclosure(f, name?)`;
- `setreadonly(t, bool)`;
- `getupvalue`, `setupvalue`, `getlocal`, `setlocal`, `getmetatable`, `setmetatable` and
  `getregistry`.

## How it's organized

| Crate | What's in it |
|---|---|
| `crates/luau-common` | bytecode format, opcodes, builtin ids, hashing, allocator |
| `crates/luau-ast` | lexer and parser |
| `crates/luau-compiler` | the bytecode compiler (constant folding, inlining, loop unrolling, type info) |
| `crates/luau-rt` | VM, garbage collector, coroutines, standard libraries, `task`/`Signal`/`Regex`, and the x64 JIT (`codegen/`) |
| `crates/luau-config`, `crates/luau-require` | `.luaurc` / `.config.luau` and require-by-string |
| `crates/luaurs` | the Rust library |
| `crates/luau` | the `luau` and `luau-compile` programs |

The JIT turns bytecode straight into x64 machine code, keeping track of each register's type and
where it lives as it goes. Most of its speed comes from three tricks:
- it spots repeated calculations and reuses the result;
- it keeps loop variables in CPU registers between iterations instead of writing them back to
  memory;
- when a guess about a type turns out wrong, it recompiles the function using what it saw.

## Building and testing

You need Rust (stable is fine; `.cargo/config.toml` switches on the two unstable features the
code uses).

```sh
cargo build --release
cargo test --release      # unit and library tests
bash sweep.sh             # Luau's conformance tests with the JIT (LUAU_CODEGEN=0 bash sweep.sh for the interpreter)
bash tools/jitdiff.sh     # tests/jit: the JIT must print exactly what the interpreter prints
```

To compare against C++ Luau yourself, build it with its JIT enabled, point `LUAU_REF` at its
`luau.exe`, and run `bash tools/benchrep.sh .`.

## Roadmap

The goal hasn't changed since day one: **be faster than C++ Luau everywhere**, not just on
average. Right now luaurs wins 158 of 224 benchmarks; the rest are the to-do list.

| Priority | Improvement |
|---|---|
| High | Close the remaining benchmark gaps (`air`, `murmur128_int64`, proxy `__index`, `pcmmix`, `life`, `table.find` loops, string interpolation: 4–11% behind) |
| High | Expand JIT support beyond Windows x64 |
| High | More JIT/interpreter differential tests |
| High | Improve JIT deoptimization and fallback coverage |
| Medium | Type checker integration |
| Medium | More detailed JIT profiling tools |
| Medium | Public API documentation and examples |

## Current limits

- The JIT only runs on **x64 Windows** for now. Elsewhere your code runs in the interpreter.
- Luau's type checker, linter and autocomplete aren't ported. Type annotations still parse and
  compile fine, they just aren't checked.
- Of the syntax errors in a file, you get the first one, the same one the C++ parser reports.

## License

MIT; see [LICENSE](LICENSE). luaurs is a port of [Luau](https://github.com/luau-lang/luau)
by Roblox (MIT), which itself comes from [Lua 5.1](https://www.lua.org) by Lua.org, PUC-Rio
(MIT). The conformance tests in `tests/conformance` and the benchmarks in `bench` come from the
Luau repository (see `tests/LUAU_LICENSE.txt`).
