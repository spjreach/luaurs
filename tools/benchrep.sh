#!/usr/bin/env bash
# usage: [K=3] [FLAGS=--codegen] benchrep.sh <pattern>...
# Compares the best in-process run time each test reports via bench_support
# (no process startup), min over K processes, Rust port vs reference.
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
R="${RBIN:-$ROOT/target/release/luau.exe}"; C="${LUAU_REF:-$ROOT/../luau-reference-build/luau.exe}"
K="${K:-3}"; FLAGS="${FLAGS---codegen}"
cd "$ROOT/bench"
best() { for _ in $(seq $K); do "$@" 2>/dev/null | tr -d '\r' | grep -o '|><|[^|]*|><|[0-9.e-]*' | sed 's/|><|/\t/g' | cut -f2,3; done | awk -F'\t' '{ if (!($1 in m) || $2 < m[$1]) m[$1] = $2 } END { for (k in m) printf "%s\t%s\n", k, m[k] }' | sort; }
for pat in "$@"; do
  for f in $(find tests micro_tests gc other \( -name "*.lua" -o -name "*.luau" \) | grep -E "$pat" | sort); do
    join -t$'\t' <(best "$R" $FLAGS "$f") <(best "$C" $FLAGS "$f") | awk -F'\t' -v f="$f" '{ printf "%-46s %-34s rust=%8.3f ref=%8.3f ratio=%.2f\n", f, substr($1,1,34), $2, $3, ($3 > 0 ? $2/$3 : 0) }'
  done
done
