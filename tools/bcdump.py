import sys, struct

OPS = """NOP BREAK LOADNIL LOADB LOADN LOADK MOVE GETGLOBAL SETGLOBAL GETUPVAL SETUPVAL CLOSEUPVALS GETIMPORT
GETTABLE SETTABLE GETTABLEKS SETTABLEKS GETTABLEN SETTABLEN NEWCLOSURE NAMECALL CALL RETURN JUMP JUMPBACK
JUMPIF JUMPIFNOT JUMPIFEQ JUMPIFLE JUMPIFLT JUMPIFNOTEQ JUMPIFNOTLE JUMPIFNOTLT ADD SUB MUL DIV MOD POW
ADDK SUBK MULK DIVK MODK POWK AND OR ANDK ORK CONCAT NOT MINUS LENGTH NEWTABLE DUPTABLE SETLIST FORNPREP
FORNLOOP FORGLOOP FORGPREP_INEXT FASTCALL3 FORGPREP_NEXT NATIVECALL GETVARARGS DUPCLOSURE PREPVARARGS
LOADKX JUMPX FASTCALL COVERAGE CAPTURE SUBRK DIVRK FASTCALL1 FASTCALL2 FASTCALL2K FORGPREP JUMPXEQKNIL
JUMPXEQKB JUMPXEQKN JUMPXEQKS IDIV IDIVK""".split()


class R:
    def __init__(s, b):
        s.b = b
        s.p = 0

    def u8(s):
        v = s.b[s.p]
        s.p += 1
        return v

    def var(s):
        r = 0
        sh = 0
        while True:
            c = s.u8()
            r |= (c & 127) << sh
            sh += 7
            if not c & 128:
                return r

    def raw(s, n):
        v = s.b[s.p:s.p + n]
        s.p += n
        return v


def dump(path):
    out = []
    r = R(open(path, 'rb').read())
    ver = r.u8()
    tv = r.u8()
    out.append(f"version {ver} types {tv}")
    strs = [r.raw(r.var()) for _ in range(r.var())]
    for i, s in enumerate(strs):
        out.append(f"S{i+1} {s!r}")
    if tv >= 3:
        while True:
            idx = r.u8()
            if idx == 0:
                break
            out.append(f"udt {idx} {r.var()}")
    np = r.var()
    for pi in range(np):
        flen = r.var()
        fstart = r.p
        ms, npar, nup, va, fl = r.u8(), r.u8(), r.u8(), r.u8(), r.u8()
        ts = r.var()
        tb = r.raw(ts)
        out.append(f"== proto {pi} maxstack={ms} params={npar} ups={nup} vararg={va} flags={fl} types={tb.hex()}")
        nc = r.var()
        code = [struct.unpack('<I', r.raw(4))[0] for _ in range(nc)]
        nk = r.var()
        ks = []
        for _ in range(nk):
            t = r.u8()
            if t == 0: ks.append('nil')
            elif t == 1: ks.append(f"b{r.u8()}")
            elif t == 2: ks.append(f"n{struct.unpack('<d', r.raw(8))[0]!r}")
            elif t == 3: ks.append(f"s{r.var()}")
            elif t == 4: ks.append(f"imp{struct.unpack('<I', r.raw(4))[0]:x}")
            elif t == 5:
                n = r.var(); ks.append(f"tab{[r.var() for _ in range(n)]}")
            elif t == 6: ks.append(f"clo{r.var()}")
            elif t == 7: ks.append(f"vec{r.raw(16).hex()}")
            elif t == 8:
                n = r.var(); ks.append(f"tabk{[(r.var(), struct.unpack('<i', r.raw(4))[0]) for _ in range(n)]}")
            elif t == 9:
                sg = r.u8(); v = r.var(); ks.append(f"int{-v if sg else v}")
            elif t == 11: ks.append(f"vecd{r.raw(32).hex()}")
            elif t == 10:
                cn=r.var(); a=r.u8(); b=r.u8(); np_=r.var(); nm=r.var()
                ks.append(f"class(s{cn},{a},{b},props={[r.var() for _ in range(np_-a)]},meths={[r.var() for _ in range(nm-b)]})")
            else: ks.append(f"?{t}")
        nps = r.var()
        ps = [r.var() for _ in range(nps)]
        ld = r.var()
        dn = r.var()
        out.append(f"   K={ks}")
        out.append(f"   P={ps} linedefined={ld} name={dn}")
        li = r.u8()
        lines = None
        if li:
            lg = r.u8()
            intervals = ((nc - 1) >> lg) + 1
            lo = r.raw(nc)
            ab = []
            acc = 0
            for _ in range(intervals):
                acc += struct.unpack('<i', r.raw(4))[0]
                ab.append(acc)
            lines = []
            last = 0
            for i in range(nc):
                last = (last + lo[i]) & 255
                lines.append(ab[i >> lg] + last)
        di = r.u8()
        if di:
            nl = r.var()
            for _ in range(nl):
                out.append(f"   local {r.var()} {r.var()} {r.var()} {r.u8()}")
            nu = r.var()
            for _ in range(nu):
                out.append(f"   upval {r.var()}")
        nfb = r.var()
        fbs = []
        for _ in range(nfb):
            k = r.u8(); fbs.append((k, r.var()))
        out.append(f"   FB={fbs}")
        if r.p != fstart + flen:
            out.append(f"   !! {fstart + flen - r.p} trailing bytes")
        r.p = fstart + flen
        for i, w in enumerate(code):
            op = w & 255
            name = OPS[op] if op < len(OPS) else f"op{op}"
            a, b, c = (w >> 8) & 255, (w >> 16) & 255, w >> 24
            d = struct.unpack('<h', struct.pack('<H', w >> 16))[0]
            ln = lines[i] if lines else '-'
            out.append(f"   {i:4} L{ln} {w:08x} {name} A={a} B={b} C={c} D={d}")
    out.append(f"main {r.var()}")
    return out


if __name__ == '__main__':
    a = dump(sys.argv[1])
    if len(sys.argv) > 2:
        b = dump(sys.argv[2])
        import difflib
        lines = list(difflib.unified_diff(a, b, 'ref', 'rs', n=6, lineterm=''))
        print('\n'.join(lines[: int(sys.argv[3]) if len(sys.argv) > 3 else 80]))
    else:
        print('\n'.join(a))
