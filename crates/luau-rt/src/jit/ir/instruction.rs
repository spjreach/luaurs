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
    /// exits unless `a` is truthy (`want == true`) / falsy
    GuardTruthy { a: Ref, want: bool, snap: SnapId },
    /// exits unless `a` (a GC object) is the object `k`
    GuardSame { a: Ref, k: Ref, snap: SnapId },

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

    // ---- conversions ----------------------------------------------------
    /// f64 -> I32, exits unless exact (array indexing by a number)
    NumToI32 { a: Ref, snap: SnapId },
    I32ToNum(Ref),
    AddI32(Ref, Ref),

    // ---- tables ---------------------------------------------------------
    /// the table's array size (`sizearray`, I32)
    ASize(Ref),
    /// pointer to array element `idx` (I32, 0-based) of table `t`; the
    /// caller guards `idx < ASize(t)` first
    ARef { t: Ref, idx: Ref },
    /// load the TValue at a Ptr as `ty`; exits through `snap` on another tag
    XLoad { p: Ref, snap: SnapId },
    /// store `v` into the TValue at a Ptr (with its tag); a GC barrier is
    /// emitted for collectable values
    XStore { p: Ref, v: Ref },
    /// exits unless the table has no metatable (`__index`/`__newindex`
    /// can't run) and isn't read-only (for stores)
    GuardPlainTable { t: Ref, store: bool, snap: SnapId },

    // ---- functions --------------------------------------------------------
    /// the running closure of the trace's own frame (`L->ci->func`)
    FrameFunc,
    /// upvalue `idx` of closure `cl`: through its upvalue object when
    /// `by_ref` (exits unless it is one), else stored in the closure; exits
    /// through `snap` unless the value has this instruction's type
    ULoad { cl: Ref, idx: u8, by_ref: bool, snap: SnapId },
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
            KNil | KBool(_) | KNum(_) | KInt(_) | KPtr(_) | KI32(_) | SLoad { .. } | Loop | Nop => vec![],
            Guard { a, b, .. } => vec![*a, *b],
            GuardTruthy { a, .. } => vec![*a],
            GuardSame { a, k, .. } => vec![*a, *k],
            Add(a, b) | Sub(a, b) | Mul(a, b) | Div(a, b) | Mod(a, b) | Pow(a, b) | IDiv(a, b) | AddI32(a, b) => {
                vec![*a, *b]
            }
            Neg(a) | Math1(_, a) | I32ToNum(a) | ASize(a) => vec![*a],
            NumToI32 { a, .. } => vec![*a],
            ARef { t, idx } => vec![*t, *idx],
            XLoad { p, .. } => vec![*p],
            XStore { p, v } => vec![*p, *v],
            GuardPlainTable { t, .. } => vec![*t],
            FrameFunc => vec![],
            ULoad { cl, .. } | ClEnv { cl } => vec![*cl],
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
            Guard { snap, .. } | GuardTruthy { snap, .. } | GuardSame { snap, .. } | NumToI32 { snap, .. } | XLoad { snap, .. } | GuardPlainTable { snap, .. } | GuardFlag { snap, .. } | ULoad { snap, .. } | GuardProto { snap, .. } => {
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
                | Pow(..) | IDiv(..) | Neg(_) | Math1(..) | I32ToNum(_) | AddI32(..) | ARef { .. } | FrameFunc | ClEnv { .. }
        )
    }

    pub fn is_const(&self) -> bool {
        use Op::*;
        matches!(self.op, KNil | KBool(_) | KNum(_) | KInt(_) | KPtr(_) | KI32(_))
    }
}
