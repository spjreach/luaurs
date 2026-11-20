#!/usr/bin/env bash
# usage: jitdis.sh <script> <function-name> [objdump args]  -- disassembles the JIT code of one function
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; BIN="$ROOT/target/release/luau.exe"
D=$(mktemp -d); s="$1"; fn="$2"; shift 2
( B="$ROOT/bench"; case "$(cd "$(dirname "$s")" && pwd)/" in "$(cd "$B" && pwd)"/*) cd "$B"; s="${s#*bench/}";; esac; LUAU_BENCH_RUNS=1 LUAU_CODEGEN_BIN="$D" timeout 120 "$BIN" --codegen "$s" >/dev/null 2>&1 )
for f in "$D"/${fn}_*.bin; do echo "== $(basename "$f")"; /c/msys64/mingw64/bin/objdump -D -b binary -mi386:x86-64 -M intel "$@" "$f" | tail -n +8; done
rm -rf "$D"
