import sys, bisect
from collections import Counter
dump, prof = sys.argv[1], sys.argv[2]
OPN = {}
# opcode names from bytecode.rs enum order
import re
import os
src = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "crates", "luau-common", "src", "bytecode.rs"), encoding="utf-8").read()
body = src[src.index("pub enum Opcode"):]
body = body[body.index("{")+1: body.index("\n}")]
names = [l.strip().rstrip(',') for l in body.split("\n") if re.match(r"^\s*[A-Z][A-Za-z0-9]*,?\s*$", l)]
entries = []  # (addr, proto, idx, op)
proto = None
for ln in open(dump):
    if ln.startswith("proto"):
        parts = ln.split()
        proto = parts[1] + ":" + parts[2] + (":G" if "generic=true" in ln else "")
        continue
    a, i, op = ln.split()
    entries.append((int(a, 16), proto, int(i), int(op)))
entries.sort()
addrs = [e[0] for e in entries]
c = Counter(); tot = 0; jit = 0
for ln in open(prof):
    n, a = ln.split(); n = int(n); a = int(a, 16); tot += n
    k = bisect.bisect_right(addrs, a) - 1
    if k >= 0 and a - addrs[k] < 4096:
        e = entries[k]; jit += n
        c[(e[1], e[2], names[e[3]] if e[3] < len(names) else e[3])] += n
print("total", tot, "attributed to native instructions", jit)
for k, v in c.most_common(int(sys.argv[3]) if len(sys.argv) > 3 else 25):
    print(f"{100*v/tot:5.1f}% {k}")
