#!/usr/bin/env bash
# usage: jitprof.sh <bench script> [top-n]  -- samples a --codegen run and attributes time to bytecode instructions
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; BIN="$ROOT/target/release/luau.exe"
D=$(mktemp -d); s="$1"
( B="$ROOT/bench"; case "$(cd "$(dirname "$s")" && pwd)/" in "$(cd "$B" && pwd)"/*) cd "$B"; s="${s#*bench/}";; esac; LUAU_BENCH_RUNS=1 LUAU_CODEGEN_DUMP="$D/dump.txt" LUAU_PROFILE="$D/prof.txt" "$BIN" --codegen "$s" >/dev/null 2>&1 )
python "$ROOT/tools/jitprof.py" "$D/dump.txt" "$D/prof.txt" "${2:-25}"
rm -rf "$D"
