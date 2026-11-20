#!/usr/bin/env bash
# Differential JIT tests: every script in tests/jit must print the same with --no-codegen and --codegen.
cd "$(dirname "$0")/.."
BIN=${BIN:-./target/release/luau.exe}
fail=0
for f in tests/jit/*.lua; do
  a=$(timeout 60 "$BIN" --no-codegen "$f" 2>&1); ra=$?
  b=$(timeout 60 "$BIN" --codegen "$f" 2>&1); rb=$?
  # table/function addresses differ between runs
  a=$(echo "$a" | sed -E 's/0x[0-9a-fA-F]+/ADDR/g'); b=$(echo "$b" | sed -E 's/0x[0-9a-fA-F]+/ADDR/g')
  if [ "$a" == "$b" ] && [ $ra -eq $rb ]; then echo "ok   $f"; else echo "DIFF $f (exit $ra vs $rb)"; fail=1; diff <(echo "$a") <(echo "$b") | head -8; fi
done
exit $fail
