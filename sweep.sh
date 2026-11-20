#!/usr/bin/env bash
# Full conformance sweep. Each test runs in its own shell with a hard address
# space cap and a wall-clock timeout so a runaway test can never exhaust RAM.
cd "$(dirname "$0")"
BIN=${BIN:-./target/release/luau.exe}
pass=0; fail=0; failed=()
for f in tests/conformance/*.luau; do
  name=$(basename "$f" .luau)
  ( ulimit -v 1500000 2>/dev/null; timeout ${TO:-60} "$BIN" "$f" >"/tmp/out_$name.txt" 2>&1 )
  code=$?
  if [ $code -eq 0 ]; then
    echo "PASS $name"; pass=$((pass+1))
  else
    echo "FAIL $name (exit $code)"; fail=$((fail+1)); failed+=("$name")
  fi
done
echo "=== $pass passed, $fail failed ==="
echo "failing: ${failed[*]}"
