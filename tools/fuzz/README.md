# Differential fuzzer

`fuzz.py` generates deterministic Luau programs and compares three runs:

| run | command |
| --- | --- |
| our JIT | `target/release/luau --codegen` |
| our interpreter | `target/release/luau --no-codegen` |
| C++ reference | `luau.exe --codegen` |

It reports any difference in output or exit code.  Matching output is normalized
for object addresses (`0x...` becomes `0xADDR`), so table identities do not
count as mismatches.  Programs avoid clocks, randomness, hash iteration order
and error-message text.

Build the release binaries first (the fuzzer never builds), then run:

```sh
python tools/fuzz/fuzz.py --count 100
python tools/fuzz/fuzz.py --seed 1234 --count 50 --verbose
```

The C++ reference path defaults to the local `luau-reference-build` checkout;
override it with `--ref` or `LUAU_REF`.  Reduced failures are written to
`tools/fuzz/failures/` as a `.lua` program and a `.txt` report.  Use
`--no-shrink` to keep the original program, and `--ref-interp` to also compare
the reference interpreter.

The generator covers arithmetic and integer operations, tables and metamethods,
loops with mid-loop type changes, closures/upvalues, varargs, strings,
`pcall`/`error`, vectors, logic/comparisons and multiple-return calls.  Each
generated block is self-contained, which lets the shrinker remove whole blocks
before trying individual lines.
