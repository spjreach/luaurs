#!/usr/bin/env bash
# usage: jitbisect.sh <script (relative to cwd)> [max]  -- finds the single function whose native code breaks the script
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/luau.exe"
ok() { LUAU_BENCH_RUNS=1 LUAU_CODEGEN_RANGE="$1:$2" timeout 120 "$BIN" --codegen "$3" >/dev/null 2>&1; }
lo=0; hi=${2:-512}
if ok 0 0 "$1"; then :; else echo "fails even with no native code"; exit 1; fi
if ok $lo $hi "$1"; then echo "passes with all functions"; exit 0; fi
while [ $((hi - lo)) -gt 1 ]; do mid=$(( (lo + hi) / 2 )); if ok $lo $mid "$1"; then lo=$mid; else hi=$mid; fi; done
echo "function #$lo (LUAU_CODEGEN_RANGE=$lo:$hi)"
