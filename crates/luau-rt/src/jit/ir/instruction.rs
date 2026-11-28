//! IR instructions.
//!
//! The IR is a linear SSA list in the style of LuaJIT's: every instruction
//! defines at most one value, named by its index (`Ref`). Constants are
//! instructions too (interned by the builder). Control flow inside a trace
//! is limited to guards (which leave the trace through a snapshot) and the
//! single `Loop` marker that separates the first iteration (the "pre-roll",
//! where loads and invariant work happen) from the loop body, whose `Phi`s
//! carry values from one iteration to the next.

use super::types::IrType;

/// An SSA value: the index of the instruction that defines it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ref(pub u32);

impl Default for Ref {
    fn default() -> Ref {
        Ref(0)
    }
}

impl Ref {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Index into `Trace::snapshots`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SnapId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmp {
    Lt,
    Le,
    Eq,
    Ne,
    /// not less than (`!(a < b)`, which differs from `>=` for NaN)
    Nlt,
    /// not less or equal
    Nle,
    /// unsigned less than (I32s: `0 <= a < b` in one compare when b >= 0)
    ULt,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    // ---- constants (interned; `ty` gives the type) ----------------------
    KNil,
    KBool(bool),
    KNum(f64),
    KInt(i64),
    /// a GC object or other pointer constant (strings, tables, functions,
    /// closures; the trace keeps them alive through its constant list)
    KPtr(usize),
    /// a 32-bit machine integer constant
    KI32(i32),

    // ---- the VM stack -------------------------------------------------
    /// load stack slot `slot` (relative to the trace's base) as `ty`;
    /// guarded: exits through `snap` if the slot's tag isn't `ty`'s tag
    SLoad { slot: u16, guarded: bool, snap: SnapId },
    /// (stores happen only through snapshots and at trace exit: the stack
    /// is written back lazily, LuaJIT style)

    // ---- guards ---------------------------------------------------------
    /// exits through `snap` unless `cmp(a, b)` holds (numbers or I32s)
    Guard { cmp: Cmp, a: Ref, b: Ref, snap: SnapId },
    // ---- 64-bit machine integers (I64) ----------------------------------
    KI64(i64),
    /// the number `a * 2^shift` as an I64; exits through `snap` unless it
    /// is exactly one (not -0 or NaN) below `lim` in magnitude. (A loop
    /// PHI's entry: body re-entry checks the same.)
    NumToI64 { a: Ref, lim: i64, shift: u8, snap: SnapId },
    I32ToI64(Ref),
    AddI64(Ref, Ref),
    SubI64(Ref, Ref),
    MulI64(Ref, Ref),
    NegI64(Ref),
    /// `a` mod `k` rounded down (k >= 2): Lua's % on integers
    ModI64K(Ref, i64),
    /// the I64 as a number, times 2^-shift (exact: a power of two)
    I64ToNum(Ref, u8),
    /// numbers: `t` when `a cmp b` holds (NaN: it doesn't), else `f` (a
    /// branch of the loop body if-converted, see `Recorder::if_convert`)
    Select { cmp: Cmp, a: Ref, b: Ref, t: Ref, f: Ref },
    /// exits unless `a` is truthy (`want == true`) / falsy
    GuardTruthy { a: Ref, want: bool, snap: SnapId },
    /// exits unless `a` (a GC object) is the object `k`
    GuardSame { a: Ref, k: Ref, snap: SnapId },
    /// exits if `a` is the object `k`
    GuardDiff { a: Ref, k: Ref, snap: SnapId },
    /// `luaV_strcmp(a, b)` of two strings (an I32: <0, 0, >0)
    StrCmp { a: Ref, b: Ref },

    // ---- numbers (f64) --------------------------------------------------
    Add(Ref, Ref),
    Sub(Ref, Ref),
    Mul(Ref, Ref),
    Div(Ref, Ref),
    /// Luau's `a - floor(a / b) * b`
    Mod(Ref, Ref),
    Pow(Ref, Ref),
    IDiv(Ref, Ref),
    Neg(Ref),
    /// a math builtin of one argument (floor, sqrt, abs, ...): `luau_common`
    /// builtin id
    Math1(u8, Ref),
    /// `math.random()`: the next number of the PCG32 generator whose state
    /// is at `state` (two steps, as the library function)
    Random { state: usize },
    /// a function trace's RETURN into another call of the same function
    /// (down-recursion): exits unless the frame returns to the caller
    /// whose savedpc is `ret` (one of this function's call sites) and isn't
    /// a call from C; then pops the frame - `L->ci`, `L->base`, `L->top` and
    /// the trace's base become the caller's. The caller's closure.
    PopFrame { ret: usize, snap: SnapId },

    // ---- vectors (3 x f32, in an xmm register; lane 3 is junk) -----------
    /// lane-wise `a op b` in single precision (`op`: 0x58 add, 0x5c sub,
    /// 0x59 mul, 0x5e div); an operand that is a Number is rounded to f32
    /// and used for every lane; `floor`: each lane floored after (IDIV)
    VArith { op: u8, a: Ref, b: Ref, floor: bool },
    /// `-v`
    VNeg(Ref),
    /// lane `lane` of vector `v`, as a Number
    VGet { v: Ref, lane: u8 },
    /// the vector of three Numbers (rounded to f32)
    VMake { x: Ref, y: Ref, z: Ref },
    /// exits unless `a == b` (every lane, IEEE: NaN never equal) is `want`
    GuardVecEq { a: Ref, b: Ref, want: bool, snap: SnapId },

    // ---- conversions ----------------------------------------------------
    /// f64 -> I32, exits unless exact (array indexing by a number)
    NumToI32 { a: Ref, snap: SnapId },
    I32ToNum(Ref),
    /// an integer-valued number as an I32, truncating (no check: see
    /// `optimizer::narrow`)
    TruncI32(Ref),
    AddI32(Ref, Ref),
    /// bitwise and of I32s
    AndI32(Ref, Ref),

    // ---- tables ---------------------------------------------------------
    /// the table's array size (`sizearray`, I32)
    ASize(Ref),
    /// pointer to array element `idx` (I32, 0-based) of table `t` (a Table,
    /// or its `ABase`); the caller guards `idx < ASize(t)` first
    ARef { t: Ref, idx: Ref },
    /// table `t`'s array part (a Ptr; it moves only when keys are added)
    ABase(Ref),
    /// load the TValue at a Ptr as `ty`; exits through `snap` on another tag
    XLoad { p: Ref, snap: SnapId },
    /// store `v` into the TValue at a Ptr (with its tag); a GC barrier is
    /// emitted for collectable values
    XStore { p: Ref, v: Ref },
    /// exits unless the table has no metatable (`__index`/`__newindex`
    /// can't run) and isn't read-only (for stores)
    GuardPlainTable { t: Ref, store: bool, snap: SnapId },

    /// pointer to the value of hash node `idx` of table `t` (a Table, or a
    /// Ptr to one), whose key must be the string `k`: exits through `snap`
    /// unless the hash part still has `2^lsize` nodes and that node's key is
    /// `k` (LuaJIT's HREFK). `store`: also exits unless the node's value is
    /// non-nil and the table isn't read-only (an existing field is assigned
    /// without `__newindex` or a new key)
    HRef { t: Ref, k: Ref, idx: u32, lsize: u8, store: bool, snap: SnapId },
    /// pointer to the value of hash node `idx` of table `t`, unchecked: an
    /// `HRef` of the same node was checked before the loop (a trace never
    /// adds keys, so the table's hash part can't change while it runs)
    HSlot { t: Ref, idx: u32 },

    /// GC write barrier after storing `v` (a GC object) into table `t`
    /// (`luaC_barriert`)
    Barrier { t: Ref, v: Ref },
    /// `#t` of a table without a metatable (a Number)
    TLen { t: Ref },
    /// length of string `s` (a Number)
    SLen { s: Ref },
    /// the metatable of table `t`; exits through `snap` when it has none
    TMeta { t: Ref, snap: SnapId },
    /// exits through `snap` unless `t[k]` (a string key) is nil
    HMiss { t: Ref, k: Ref, snap: SnapId },
    /// exits through `snap` unless `f` is a C function with code at `fp`
    GuardCFunc { f: Ref, fp: usize, snap: SnapId },
    /// pointer to `t[k]` for any key (`luaH_get`; a nil object when absent),
    /// the key passed through the scratch TValue at `buf`
    TGet { t: Ref, k: Ref, buf: usize },
    /// `t[k] = v` raw, inserting the key if needed (the table may be
    /// rehashed: see `Trace::inserts`); exits through `snap` for a nil or NaN
    /// key (the interpreter raises the error)
    TSet { t: Ref, k: Ref, v: Ref, buf: usize, snap: SnapId },
    /// a call of a leaf C function (one that only reads its arguments: no
    /// callbacks into Lua, no yields) in stack slot `slot` of the trace's
    /// frame with `nargs` arguments above it, `nres` results left there;
    /// the slots `snap` lists are stored first (the GC and error handling
    /// see the frame), `L->ci->savedpc` is bytecode index `pc`
    /// (`savedpc`: the instruction pointer to report; `frames`: a leaked
    /// `Vec<SnapFrame>` of the inlined frames to materialize around the
    /// call, 0 at the trace's own frame; `off`: the calling frame's base)
    CCall { slot: u16, nargs: u8, nres: u8, savedpc: usize, frames: usize, off: u16, snap: SnapId },
    /// a call of any C function (it may call Lua: `pcall`, `table.sort` with
    /// a comparator, ...) in trace slot `slot` with `nargs` arguments,
    /// `nres` results (-1: all of them), like the interpreter's CALL; the
    /// slots `snap` lists are stored first (everything after it reads the
    /// stack again). Defines the number of results (an I32). When the
    /// function yields, the trace leaves the VM there (`EXIT_LEFT_VM`)
    CallC { slot: u16, nargs: u8, nres: i16, savedpc: usize, frames: usize, off: u16, snap: SnapId },
    /// `R(a) = R(b) .. ... .. R(c)` (strings and numbers) by `luaV_concat`
    /// on the stack, after storing the slots `snap` lists
    Concat { a: u16, b: u16, c: u16, savedpc: usize, frames: usize, off: u16, snap: SnapId },
    /// upvalue `idx` of closure `cl` = `v` (through the scratch TValue at
    /// `buf`); exits through `snap` if the upvalue is open on the trace's
    /// frames (the recorder handles those itself)
    UStore { cl: Ref, idx: u8, v: Ref, buf: usize, snap: SnapId },
    /// `not a` of a boolean
    Not(Ref),
    /// a `pairs`/`ipairs` iterator index (light userdata) as an I32
    LudToI32(Ref),
    /// an I32 as an iterator index (stored with the iterator tag)
    I32ToLud(Ref),
    /// stack slot `slot` (after a C call left a result there): guarded like
    /// SLoad, but where it is (never hoisted)
    SReload { slot: u16, snap: SnapId },
    /// a closure (`NEWCLOSURE` / `DUPCLOSURE`) as described by the leaked
    /// `ClosureSite` at `site`, inside closure `cl`, its captured register
    /// values `vals[..nvals]` passed through the scratch TValues at `buf`
    /// (a local captured by reference: stored to its stack slot, which the
    /// new upvalue points at)
    MakeClosure { cl: Ref, site: usize, vals: [Ref; 8], nvals: u8, buf: usize },
    /// exits through `snap` unless L->top is trace slot `top` (a side trace
    /// starting where a call left a variable number of results)
    GuardTop { top: u16, snap: SnapId },
    /// `CLOSEUPVALS`: closes the open upvalues at or above trace slot
    /// `slot`, after storing the slots `snap` lists (only when there are
    /// any: the stack must hold the values they capture)
    CloseUpvals { slot: u16, snap: SnapId },
    /// a table the trace doesn't create (`Trace::sinks` says how exits
    /// build it); only snapshots name it
    Sunk,
    /// a new table (`NEWTABLE`)
    NewTable { narray: u32, nhash: u32 },
    /// a copy of template table `tpl` (`DUPTABLE`; a Table constant)
    DupTable { tpl: Ref },
    /// Luau's fast builtin `bfid` (`luauF_table`) on `args[..nargs]`, through
    /// the scratch TValues at `buf` (result, then arguments). Exits through
    /// `snap` (resuming at the FASTCALL, which the interpreter then does the
    /// slow way) when the builtin declines, or when its result (`nres` = 1)
    /// isn't of this instruction's type
    Builtin { bfid: u8, args: [Ref; 3], nargs: u8, nres: u8, buf: usize, snap: SnapId },

    /// vararg `idx` (an I32, 0-based; the caller checked it is below the
    /// count) of the trace's own frame, which has `Trace::varargs` of them
    /// below its base; exits through `snap` unless it has this type
    VargLoad { idx: Ref, snap: SnapId },

    // ---- functions --------------------------------------------------------
    /// the running closure of the trace's own frame (`L->ci->func`)
    FrameFunc,
    /// upvalue `idx` of closure `cl`: through its upvalue object when
    /// `by_ref` (exits unless it is one, or if it points into the stack
    /// below `base + stack`: an open upvalue of a slot the trace may hold in
    /// a register), else stored in the closure; exits through `snap` unless
    /// the value has this instruction's type
    ULoad { cl: Ref, idx: u8, by_ref: bool, stack: u16, snap: SnapId },
    /// pointer to the value of upvalue `idx` of closure `cl` (its UpVal's
    /// `v`); exits through `snap` unless it is an upvalue object not open
    /// on the stack below `base + stack` (see `ULoad`)
    UPtr { cl: Ref, idx: u8, stack: u16, snap: SnapId },
    /// exits unless upvalue `idx` of closure `cl` is open on stack slot
    /// `slot` of the trace's frame (the trace then uses its own value of
    /// that slot)
    GuardUvSlot { cl: Ref, idx: u8, slot: u16, snap: SnapId },
    /// the environment (globals table) of closure `cl` (a Ptr)
    ClEnv { cl: Ref },
    /// exits unless function `f` is a Lua closure of prototype `p` (a Ptr
    /// constant)
    GuardProto { f: Ref, p: Ref, snap: SnapId },
    /// exits through `snap` when the byte at `[p + off]` is zero (`p` a Ptr
    /// constant; e.g. a global table's `safeenv`)
    GuardFlag { p: Ref, off: i32, snap: SnapId },

    // ---- loops ----------------------------------------------------------
    /// the loop header: instructions after it form the loop body
    Loop,
    /// at the header, `pre` (defined before `Loop`) on entry and `back`
    /// (defined in the body) on every later iteration
    Phi { pre: Ref, back: Ref },

    /// a no-op left behind by optimizer passes (keeps `Ref`s stable)
    Nop,
}

#[derive(Clone, Debug)]
pub struct Inst {
    pub op: Op,
    pub ty: IrType,
}

impl Inst {
    /// The values this instruction reads.
    pub fn operands(&self) -> Vec<Ref> {
        use Op::*;
        match &self.op {
            KNil | KBool(_) | KNum(_) | KInt(_) | KPtr(_) | KI32(_) | KI64(_) | SLoad { .. } | Loop | Nop => vec![],
            Guard { a, b, .. } => vec![*a, *b],
            NumToI64 { a, .. } => vec![*a],
            I32ToI64(a) | NegI64(a) | ModI64K(a, _) | I64ToNum(a, _) => vec![*a],
            AddI64(a, b) | SubI64(a, b) | MulI64(a, b) => vec![*a, *b],
            Select { a, b, t, f, .. } => vec![*a, *b, *t, *f],
            GuardTruthy { a, .. } => vec![*a],
            GuardSame { a, k, .. } | GuardDiff { a, k, .. } => vec![*a, *k],
            StrCmp { a, b } => vec![*a, *b],
            Add(a, b) | Sub(a, b) | Mul(a, b) | Div(a, b) | Mod(a, b) | Pow(a, b) | IDiv(a, b) | AddI32(a, b) | AndI32(a, b) => {
                vec![*a, *b]
            }
            Neg(a) | Math1(_, a) | I32ToNum(a) | ASize(a) | VNeg(a) | ABase(a) | TruncI32(a) => vec![*a],
            VArith { a, b, .. } | GuardVecEq { a, b, .. } => vec![*a, *b],
            VGet { v, .. } => vec![*v],
            VargLoad { idx, .. } => vec![*idx],
            VMake { x, y, z } => vec![*x, *y, *z],
            NumToI32 { a, .. } => vec![*a],
            ARef { t, idx } => vec![*t, *idx],
            XLoad { p, .. } => vec![*p],
            XStore { p, v } => vec![*p, *v],
            GuardPlainTable { t, .. } => vec![*t],
            FrameFunc | Random { .. } | PopFrame { .. } | NewTable { .. } | CCall { .. } | SReload { .. } | Concat { .. } | CloseUpvals { .. } | CallC { .. } | GuardTop { .. } | Sunk => vec![],
            UStore { cl, v, .. } => vec![*cl, *v],
            Not(a) | LudToI32(a) | I32ToLud(a) => vec![*a],
            TMeta { t, .. } => vec![*t],
            MakeClosure { cl, vals, nvals, .. } => {
                let mut v = vec![*cl];
                v.extend_from_slice(&vals[..*nvals as usize]);
                v
            }
            TGet { t, k, .. } => vec![*t, *k],
            TSet { t, k, v, .. } => vec![*t, *k, *v],
            HMiss { t, k, .. } => vec![*t, *k],
            GuardCFunc { f, .. } => vec![*f],
            Barrier { t, v } => vec![*t, *v],
            TLen { t } => vec![*t],
            SLen { s } => vec![*s],
            DupTable { tpl } => vec![*tpl],
            Builtin { args, nargs, .. } => args[..*nargs as usize].to_vec(),
            HRef { t, k, .. } => vec![*t, *k],
            HSlot { t, .. } => vec![*t],
            ULoad { cl, .. } | ClEnv { cl } | GuardUvSlot { cl, .. } | UPtr { cl, .. } => vec![*cl],
            GuardProto { f, p, .. } => vec![*f, *p],
            GuardFlag { p, .. } => vec![*p],
            Phi { pre, back } => vec![*pre, *back],
        }
    }

    /// Guards and other instructions that can leave the trace.
    pub fn snapshot(&self) -> Option<SnapId> {
        use Op::*;
        match &self.op {
            SLoad { guarded: true, snap, .. } => Some(*snap),
            Guard { snap, .. } | NumToI64 { snap, .. } | GuardTruthy { snap, .. } | GuardSame { snap, .. } | GuardDiff { snap, .. } | NumToI32 { snap, .. } | XLoad { snap, .. } | GuardPlainTable { snap, .. } | GuardFlag { snap, .. } | ULoad { snap, .. } | GuardProto { snap, .. } | HRef { snap, .. } | GuardUvSlot { snap, .. } | Builtin { snap, .. } | TMeta { snap, .. } | HMiss { snap, .. } | GuardCFunc { snap, .. } | TSet { snap, .. } | CCall { snap, .. } | SReload { snap, .. } | Concat { snap, .. } | UStore { snap, .. } | GuardVecEq { snap, .. } | VargLoad { snap, .. } | CloseUpvals { snap, .. } | CallC { snap, .. } | UPtr { snap, .. } | GuardTop { snap, .. } | PopFrame { snap, .. } => {
                Some(*snap)
            }
            _ => None,
        }
    }

    /// No side effects and no exits: removable when unused, and two equal
    /// ones compute the same value (CSE).
    pub fn is_pure(&self) -> bool {
        use Op::*;
        matches!(
            self.op,
            KNil | KBool(_) | KNum(_) | KInt(_) | KPtr(_) | KI32(_) | Add(..) | Sub(..) | Mul(..) | Div(..) | Mod(..)
                | Pow(..) | IDiv(..) | Neg(_) | Math1(..) | I32ToNum(_) | AddI32(..) | AndI32(..) | ARef { .. } | ABase(_) | TruncI32(_) | FrameFunc | ClEnv { .. } | HSlot { .. } | Not(_) | LudToI32(_) | I32ToLud(_)
                | VArith { .. } | VNeg(_) | VGet { .. } | VMake { .. } | Select { .. }
                | KI64(_) | I32ToI64(_) | AddI64(..) | SubI64(..) | MulI64(..) | NegI64(_) | ModI64K(..) | I64ToNum(..)
        )
    }

    /// Can move a table's array or node storage (adds keys).
    pub fn moves_tables(&self) -> bool {
        use Op::*;
        match self.op {
            // (a call of Lua code can do anything to any table)
            TSet { .. } | CallC { .. } => true,
            Builtin { bfid, .. } => bfid == luau_common::bytecode::BuiltinFunction::TableInsert as u8,
            _ => false,
        }
    }

    pub fn is_const(&self) -> bool {
        use Op::*;
        matches!(self.op, KNil | KBool(_) | KNum(_) | KInt(_) | KPtr(_) | KI32(_) | KI64(_))
    }
}
