"""Summarize bench.sh output: python benchsum.py results.txt [min_ms]"""
import math, re, sys

path = sys.argv[1]
min_ms = int(sys.argv[2]) if len(sys.argv) > 2 else 200
rows = []
for ln in open(path):
    m = re.match(r"(\S+) rust=(\d+)/(\d+)ms ref=(\d+)/(\d+)ms", ln)
    if not m:
        continue
    name, rc, rt, cc, ct = m.group(1), int(m.group(2)), int(m.group(3)), int(m.group(4)), int(m.group(5))
    rows.append((name, rc, rt, cc, ct))
ok = [(n, rt, ct) for n, rc, rt, cc, ct in rows if rc == 0 and cc == 0 and ct >= min_ms]
mismatch = [(n, rc, cc) for n, rc, rt, cc, ct in rows if rc != cc]
logs = [math.log(rt / ct) for _, rt, ct in ok]
gm = math.exp(sum(logs) / len(logs)) if logs else float("nan")
faster = sum(1 for _, rt, ct in ok if rt < ct)
print(f"{len(ok)} benchmarks >= {min_ms}ms; geomean rust/ref = {gm:.3f}; rust faster on {faster}")
print(f"exit-code mismatches: {mismatch}")
print("slowest ratios:")
for n, rt, ct in sorted(ok, key=lambda r: r[1] / r[2], reverse=True)[:15]:
    print(f"  {rt / ct:5.2f}  {n}  ({rt} vs {ct} ms)")
print("fastest ratios:")
for n, rt, ct in sorted(ok, key=lambda r: r[1] / r[2])[:8]:
    print(f"  {rt / ct:5.2f}  {n}  ({rt} vs {ct} ms)")
