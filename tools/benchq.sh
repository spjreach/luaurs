#!/usr/bin/env bash
# usage: [FLAGS=--codegen] [N=3] benchq.sh <pattern>...   -- min wall time over N runs, Rust port vs reference
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
R="${RBIN:-$ROOT/target/release/luau.exe}"; C="${LUAU_REF:-$ROOT/../luau-reference-build/luau.exe}"
N="${N:-3}"
cd "$ROOT/bench"
export LUAU_BENCH_RUNS=1
best() { local b=999999999; for _ in $(seq $N); do local s=$(date +%s%N); "$@" >/dev/null 2>&1 || { echo -1; return; }; local t=$(( ($(date +%s%N)-s)/100000 )); [ $t -lt $b ] && b=$t; done; echo $b; }
for pat in "$@"; do
  for f in $(find tests micro_tests gc other \( -name "*.lua" -o -name "*.luau" \) | grep -E "$pat" | sort); do
    rt=$(best "$R" $FLAGS "$f"); ct=$(best "$C" $FLAGS "$f")
    awk -v f="$f" -v r=$rt -v c=$ct 'BEGIN{ if (r<0||c<0) printf "%-50s FAIL rust=%s ref=%s\n", f, r, c; else printf "%-50s rust=%7.1fms ref=%7.1fms ratio=%.2f\n", f, r/10, c/10, r/c }'
  done
done
