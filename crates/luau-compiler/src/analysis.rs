//! Compile-time analyses: ports of `ValueTracking.cpp`, `ConstantFolding.cpp`,
//! `BuiltinFolding.cpp`, `TableShape.cpp`, `Builtins.cpp` and `CostModel.cpp`.
//!
//! The C++ passes key their results by AST node pointer; here nodes are keyed
//! by address (`visit::key`), locals by `Arc::as_ptr`.

use luau_common::fasthash::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use luau_ast::ast::*;
use luau_ast::location::Position;

use crate::visit::{key, unwrap_expr, walk_block, walk_expr, walk_function, walk_stat, Visitor};

pub type LocalKey = usize;
pub type ExprKey = usize;

#[inline]
pub fn local_key(l: &Arc<AstLocal>) -> LocalKey {
    Arc::as_ptr(l) as usize
}

#[inline]
pub fn func_key(f: &Arc<AstExprFunction>) -> usize {
    Arc::as_ptr(f) as usize
}

// ---------------------------------------------------------------------------
// Constant
// ---------------------------------------------------------------------------

/// `Compile::Constant`.
#[derive(Debug, Clone, PartialEq)]
pub enum Constant {
    Unknown,
    Nil,
    Boolean(bool),
    Number(f64),
    Integer(i64),
    Vectorf([f32; 4]),
    Vectord([f64; 4]),
    String(Rc<[u8]>),
    /// Index into the constant-table property list.
    Table(usize),
}

impl Constant {
    pub fn is_known(&self) -> bool {
        !matches!(self, Constant::Unknown)
    }

    pub fn is_truthful(&self) -> bool {
        !matches!(self, Constant::Nil | Constant::Boolean(false))
    }

    fn str(s: &[u8]) -> Constant {
        Constant::String(Rc::from(s))
    }
}

fn constants_equal(la: &Constant, ra: &Constant) -> bool {
    match (la, ra) {
        (Constant::Nil, Constant::Nil) => true,
        (Constant::Boolean(a), Constant::Boolean(b)) => a == b,
        (Constant::Number(a), Constant::Number(b)) => a == b,
        (Constant::Integer(a), Constant::Integer(b)) => a == b,
        (Constant::Vectorf(a), Constant::Vectorf(b)) => a == b,
        (Constant::Vectord(a), Constant::Vectord(b)) => a == b,
        (Constant::String(a), Constant::String(b)) => a == b,
        (Constant::Table(a), Constant::Table(b)) => a == b,
        _ => false,
    }
}

fn vectors_differ_only_in_w(la: &Constant, ra: &Constant) -> bool {
    match (la, ra) {
        (Constant::Vectorf(a), Constant::Vectorf(b)) => {
            a[0] == b[0] && a[1] == b[1] && a[2] == b[2] && a[3] != b[3]
        }
        (Constant::Vectord(a), Constant::Vectord(b)) => {
            a[0] == b[0] && a[1] == b[1] && a[2] == b[2] && a[3] != b[3]
        }
        _ => false,
    }
}

const CONSTANT_FOLD_STRING_LIMIT: usize = 4096;

fn fold_unary(op: AstExprUnaryOp, arg: &Constant) -> Constant {
    match op {
        AstExprUnaryOp::Not => {
            if arg.is_known() {
                Constant::Boolean(!arg.is_truthful())
            } else {
                Constant::Unknown
            }
        }
        AstExprUnaryOp::Minus => match arg {
            Constant::Number(n) => Constant::Number(-n),
            Constant::Vectorf(v) => Constant::Vectorf([-v[0], -v[1], -v[2], -v[3]]),
            Constant::Vectord(v) => Constant::Vectord([-v[0], -v[1], -v[2], -v[3]]),
            _ => Constant::Unknown,
        },
        AstExprUnaryOp::Len => match arg {
            Constant::String(s) => Constant::Number(s.len() as f64),
            _ => Constant::Unknown,
        },
    }
}

/// Component-wise vector folding with the `w` rule from `foldBinary`: the
/// result is only folded when `w` stays zero or some input already had it.
fn fold_vec_f(la: &Constant, ra: &Constant, f: fn(f32, f32) -> f32, w_rule: bool, floor_w_num: bool) -> Constant {
    let (a, b, a_had, b_had): ([f32; 4], [f32; 4], bool, bool) = match (la, ra) {
        (Constant::Vectorf(a), Constant::Vectorf(b)) => (*a, *b, a[3] != 0.0, b[3] != 0.0),
        (Constant::Number(n), Constant::Vectorf(b)) => {
            let x = *n as f32;
            ([x, x, x, x], *b, false, b[3] != 0.0)
        }
        (Constant::Vectorf(a), Constant::Number(n)) => {
            let x = *n as f32;
            (*a, [x, x, x, x], a[3] != 0.0, false)
        }
        _ => return Constant::Unknown,
    };
    let _ = floor_w_num;
    let had_w = a_had || b_had;
    let w = f(a[3], b[3]);
    if !w_rule || w == 0.0 || had_w {
        Constant::Vectorf([f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2]), w])
    } else {
        Constant::Unknown
    }
}

fn fold_vec_d(la: &Constant, ra: &Constant, f: fn(f64, f64) -> f64, w_rule: bool) -> Constant {
    let (a, b, a_had, b_had): ([f64; 4], [f64; 4], bool, bool) = match (la, ra) {
        (Constant::Vectord(a), Constant::Vectord(b)) => (*a, *b, a[3] != 0.0, b[3] != 0.0),
        (Constant::Number(n), Constant::Vectord(b)) => ([*n; 4], *b, false, b[3] != 0.0),
        (Constant::Vectord(a), Constant::Number(n)) => (*a, [*n; 4], a[3] != 0.0, false),
        _ => return Constant::Unknown,
    };
    let had_w = a_had || b_had;
    let w = f(a[3], b[3]);
    if !w_rule || w == 0.0 || had_w {
        Constant::Vectord([f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2]), w])
    } else {
        Constant::Unknown
    }
}

fn fold_binary(op: AstExprBinaryOp, la: &Constant, ra: &Constant) -> Constant {
    use AstExprBinaryOp as B;
    use self::Constant as C;
    let nums = match (la, ra) {
        (C::Number(a), C::Number(b)) => Some((*a, *b)),
        _ => None,
    };
    match op {
        B::Add | B::Sub => {
            if let Some((a, b)) = nums {
                return C::Number(if op == B::Add { a + b } else { a - b });
            }
            let ff: fn(f32, f32) -> f32 = if op == B::Add { |x, y| x + y } else { |x, y| x - y };
            let fd: fn(f64, f64) -> f64 = if op == B::Add { |x, y| x + y } else { |x, y| x - y };
            // add/sub only fold vector+vector (no number broadcast)
            match (la, ra) {
                (C::Vectorf(_), C::Vectorf(_)) => fold_vec_f(la, ra, ff, false, false),
                (C::Vectord(_), C::Vectord(_)) => fold_vec_d(la, ra, fd, false),
                _ => C::Unknown,
            }
        }
        B::Mul | B::Div | B::FloorDiv => {
            if let Some((a, b)) = nums {
                return C::Number(match op {
                    B::Mul => a * b,
                    B::Div => a / b,
                    _ => (a / b).floor(),
                });
            }
            let ff: fn(f32, f32) -> f32 = match op {
                B::Mul => |x, y| x * y,
                B::Div => |x, y| x / y,
                _ => |x, y| (x / y).floor(),
            };
            let fd: fn(f64, f64) -> f64 = match op {
                B::Mul => |x, y| x * y,
                B::Div => |x, y| x / y,
                _ => |x, y| (x / y).floor(),
            };
            match (la, ra) {
                (C::Vectorf(_), _) | (_, C::Vectorf(_)) => fold_vec_f(la, ra, ff, true, op == B::FloorDiv),
                (C::Vectord(_), _) | (_, C::Vectord(_)) => fold_vec_d(la, ra, fd, true),
                _ => C::Unknown,
            }
        }
        B::Mod => match nums {
            Some((a, b)) => C::Number(a - (a / b).floor() * b),
            None => C::Unknown,
        },
        B::Pow => match nums {
            Some((a, b)) => C::Number(a.powf(b)),
            None => C::Unknown,
        },
        B::Concat => match (la, ra) {
            (C::String(a), C::String(b)) if a.len() + b.len() <= CONSTANT_FOLD_STRING_LIMIT => {
                let mut v = a.to_vec();
                v.extend_from_slice(b);
                C::String(Rc::from(v))
            }
            _ => C::Unknown,
        },
        B::CompareNe | B::CompareEq => {
            if la.is_known() && ra.is_known() && !vectors_differ_only_in_w(la, ra) {
                let eq = constants_equal(la, ra);
                C::Boolean(if op == B::CompareEq { eq } else { !eq })
            } else {
                C::Unknown
            }
        }
        B::CompareLt => nums.map_or(C::Unknown, |(a, b)| C::Boolean(a < b)),
        B::CompareLe => nums.map_or(C::Unknown, |(a, b)| C::Boolean(a <= b)),
        B::CompareGt => nums.map_or(C::Unknown, |(a, b)| C::Boolean(a > b)),
        B::CompareGe => nums.map_or(C::Unknown, |(a, b)| C::Boolean(a >= b)),
        B::And => {
            if la.is_known() {
                if la.is_truthful() { ra.clone() } else { la.clone() }
            } else {
                C::Unknown
            }
        }
        B::Or => {
            if la.is_known() {
                if la.is_truthful() { la.clone() } else { ra.clone() }
            } else {
                C::Unknown
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Builtin folding (BuiltinFolding.cpp)
// ---------------------------------------------------------------------------

pub mod lbf {
    pub const NONE: i32 = 0;
    pub const ASSERT: i32 = 1;
    pub const MATH_ABS: i32 = 2;
    pub const MATH_ACOS: i32 = 3;
    pub const MATH_ASIN: i32 = 4;
    pub const MATH_ATAN2: i32 = 5;
    pub const MATH_ATAN: i32 = 6;
    pub const MATH_CEIL: i32 = 7;
    pub const MATH_COSH: i32 = 8;
    pub const MATH_COS: i32 = 9;
    pub const MATH_DEG: i32 = 10;
    pub const MATH_EXP: i32 = 11;
    pub const MATH_FLOOR: i32 = 12;
    pub const MATH_FMOD: i32 = 13;
    pub const MATH_FREXP: i32 = 14;
    pub const MATH_LDEXP: i32 = 15;
    pub const MATH_LOG10: i32 = 16;
    pub const MATH_LOG: i32 = 17;
    pub const MATH_MAX: i32 = 18;
    pub const MATH_MIN: i32 = 19;
    pub const MATH_MODF: i32 = 20;
    pub const MATH_POW: i32 = 21;
    pub const MATH_RAD: i32 = 22;
    pub const MATH_SINH: i32 = 23;
    pub const MATH_SIN: i32 = 24;
    pub const MATH_SQRT: i32 = 25;
    pub const MATH_TANH: i32 = 26;
    pub const MATH_TAN: i32 = 27;
    pub const BIT32_ARSHIFT: i32 = 28;
    pub const BIT32_BAND: i32 = 29;
    pub const BIT32_BNOT: i32 = 30;
    pub const BIT32_BOR: i32 = 31;
    pub const BIT32_BXOR: i32 = 32;
    pub const BIT32_BTEST: i32 = 33;
    pub const BIT32_EXTRACT: i32 = 34;
    pub const BIT32_LROTATE: i32 = 35;
    pub const BIT32_LSHIFT: i32 = 36;
    pub const BIT32_REPLACE: i32 = 37;
    pub const BIT32_RROTATE: i32 = 38;
    pub const BIT32_RSHIFT: i32 = 39;
    pub const TYPE: i32 = 40;
    pub const STRING_BYTE: i32 = 41;
    pub const STRING_CHAR: i32 = 42;
    pub const STRING_LEN: i32 = 43;
    pub const TYPEOF: i32 = 44;
    pub const STRING_SUB: i32 = 45;
    pub const MATH_CLAMP: i32 = 46;
    pub const MATH_SIGN: i32 = 47;
    pub const MATH_ROUND: i32 = 48;
    pub const RAWSET: i32 = 49;
    pub const RAWGET: i32 = 50;
    pub const RAWEQUAL: i32 = 51;
    pub const TABLE_INSERT: i32 = 52;
    pub const TABLE_UNPACK: i32 = 53;
    pub const VECTOR: i32 = 54;
    pub const BIT32_COUNTLZ: i32 = 55;
    pub const BIT32_COUNTRZ: i32 = 56;
    pub const SELECT_VARARG: i32 = 57;
    pub const RAWLEN: i32 = 58;
    pub const BIT32_EXTRACTK: i32 = 59;
    pub const GETMETATABLE: i32 = 60;
    pub const SETMETATABLE: i32 = 61;
    pub const TONUMBER: i32 = 62;
    pub const TOSTRING: i32 = 63;
    pub const BIT32_BYTESWAP: i32 = 64;
    pub const BUFFER_READI8: i32 = 65;
    pub const BUFFER_READU8: i32 = 66;
    pub const BUFFER_WRITEU8: i32 = 67;
    pub const BUFFER_READI16: i32 = 68;
    pub const BUFFER_READU16: i32 = 69;
    pub const BUFFER_WRITEU16: i32 = 70;
    pub const BUFFER_READI32: i32 = 71;
    pub const BUFFER_READU32: i32 = 72;
    pub const BUFFER_WRITEU32: i32 = 73;
    pub const BUFFER_READF32: i32 = 74;
    pub const BUFFER_WRITEF32: i32 = 75;
    pub const BUFFER_READF64: i32 = 76;
    pub const BUFFER_WRITEF64: i32 = 77;
    pub const VECTOR_MAGNITUDE: i32 = 78;
    pub const VECTOR_NORMALIZE: i32 = 79;
    pub const VECTOR_CROSS: i32 = 80;
    pub const VECTOR_DOT: i32 = 81;
    pub const VECTOR_FLOOR: i32 = 82;
    pub const VECTOR_CEIL: i32 = 83;
    pub const VECTOR_ABS: i32 = 84;
    pub const VECTOR_SIGN: i32 = 85;
    pub const VECTOR_CLAMP: i32 = 86;
    pub const VECTOR_MIN: i32 = 87;
    pub const VECTOR_MAX: i32 = 88;
    pub const MATH_LERP: i32 = 89;
    pub const VECTOR_LERP: i32 = 90;
    pub const MATH_ISNAN: i32 = 91;
    pub const MATH_ISINF: i32 = 92;
    pub const MATH_ISFINITE: i32 = 93;
    pub const INTEGER_CREATE: i32 = 94;
    pub const INTEGER_TONUMBER: i32 = 95;
    pub const INTEGER_NEG: i32 = 96;
    pub const INTEGER_ADD: i32 = 97;
    pub const INTEGER_SUB: i32 = 98;
    pub const INTEGER_MUL: i32 = 99;
    pub const INTEGER_DIV: i32 = 100;
    pub const INTEGER_MIN: i32 = 101;
    pub const INTEGER_MAX: i32 = 102;
    pub const INTEGER_REM: i32 = 103;
    pub const INTEGER_IDIV: i32 = 104;
    pub const INTEGER_UDIV: i32 = 105;
    pub const INTEGER_UREM: i32 = 106;
    pub const INTEGER_MOD: i32 = 107;
    pub const INTEGER_CLAMP: i32 = 108;
    pub const INTEGER_BAND: i32 = 109;
    pub const INTEGER_BOR: i32 = 110;
    pub const INTEGER_BNOT: i32 = 111;
    pub const INTEGER_BXOR: i32 = 112;
    pub const INTEGER_LT: i32 = 113;
    pub const INTEGER_LE: i32 = 114;
    pub const INTEGER_ULT: i32 = 115;
    pub const INTEGER_ULE: i32 = 116;
    pub const INTEGER_GT: i32 = 117;
    pub const INTEGER_GE: i32 = 118;
    pub const INTEGER_UGT: i32 = 119;
    pub const INTEGER_UGE: i32 = 120;
    pub const INTEGER_LSHIFT: i32 = 121;
    pub const INTEGER_RSHIFT: i32 = 122;
    pub const INTEGER_ARSHIFT: i32 = 123;
    pub const INTEGER_LROTATE: i32 = 124;
    pub const INTEGER_RROTATE: i32 = 125;
    pub const INTEGER_EXTRACT: i32 = 126;
    pub const INTEGER_BTEST: i32 = 127;
    pub const INTEGER_COUNTRZ: i32 = 128;
    pub const INTEGER_COUNTLZ: i32 = 129;
    pub const INTEGER_BSWAP: i32 = 130;
    pub const BUFFER_READINTEGER: i32 = 131;
    pub const BUFFER_WRITEINTEGER: i32 = 132;
    pub const TABLE_CREATE: i32 = 133;
    pub const TABLE_CLEAR: i32 = 134;
}

fn bit32(v: f64) -> u32 {
    v as i64 as u32
}

pub fn fold_builtin(bfid: i32, args: &[Constant], vector_double: bool) -> Constant {
    use self::Constant as C;
    let n = args.len();
    let num = |i: usize| -> Option<f64> {
        match args.get(i) {
            Some(C::Number(x)) => Some(*x),
            _ => None,
        }
    };
    let one = |f: fn(f64) -> f64| -> Constant {
        match (n, num(0)) {
            (1, Some(x)) => C::Number(f(x)),
            _ => C::Unknown,
        }
    };
    let two = |f: fn(f64, f64) -> f64| -> Constant {
        match (n, num(0), num(1)) {
            (2, Some(a), Some(b)) => C::Number(f(a, b)),
            _ => C::Unknown,
        }
    };
    const K_RAD_DEG: f64 = std::f64::consts::PI / 180.0;
    match bfid {
        lbf::MATH_ABS => one(f64::abs),
        lbf::MATH_ACOS => one(f64::acos),
        lbf::MATH_ASIN => one(f64::asin),
        lbf::MATH_ATAN2 => two(f64::atan2),
        lbf::MATH_ATAN => one(f64::atan),
        lbf::MATH_CEIL => one(f64::ceil),
        lbf::MATH_COSH => one(f64::cosh),
        lbf::MATH_COS => one(f64::cos),
        lbf::MATH_DEG => one(|x| x / K_RAD_DEG),
        lbf::MATH_EXP => one(f64::exp),
        lbf::MATH_FLOOR => one(f64::floor),
        lbf::MATH_FMOD => two(|a, b| a % b),
        lbf::MATH_LDEXP => two(|a, b| a * 2f64.powi(b as i32)),
        lbf::MATH_LOG10 => one(f64::log10),
        lbf::MATH_LOG => {
            if n == 1 {
                one(f64::ln)
            } else if let (2, Some(a), Some(b)) = (n, num(0), num(1)) {
                if b == 2.0 {
                    C::Number(a.log2())
                } else if b == 10.0 {
                    C::Number(a.log10())
                } else {
                    C::Number(a.ln() / b.ln())
                }
            } else {
                C::Unknown
            }
        }
        lbf::MATH_MAX | lbf::MATH_MIN => {
            let Some(mut r) = (if n >= 1 { num(0) } else { None }) else { return C::Unknown };
            for i in 1..n {
                let Some(a) = num(i) else { return C::Unknown };
                r = if bfid == lbf::MATH_MAX {
                    if a > r { a } else { r }
                } else if a < r {
                    a
                } else {
                    r
                };
            }
            C::Number(r)
        }
        lbf::MATH_POW => two(f64::powf),
        lbf::MATH_RAD => one(|x| x * K_RAD_DEG),
        lbf::MATH_SINH => one(f64::sinh),
        lbf::MATH_SIN => one(f64::sin),
        lbf::MATH_SQRT => one(f64::sqrt),
        lbf::MATH_TANH => one(f64::tanh),
        lbf::MATH_TAN => one(f64::tan),
        lbf::BIT32_ARSHIFT => match (n, num(0), num(1)) {
            (2, Some(a), Some(b)) => {
                let s = b as i32;
                if (s as u32) < 32 {
                    C::Number(((bit32(a) as i32) >> s) as u32 as f64)
                } else {
                    C::Unknown
                }
            }
            _ => C::Unknown,
        },
        lbf::BIT32_BAND | lbf::BIT32_BOR | lbf::BIT32_BXOR | lbf::BIT32_BTEST => {
            let Some(a0) = (if n >= 1 { num(0) } else { None }) else { return C::Unknown };
            let mut r = bit32(a0);
            for i in 1..n {
                let Some(a) = num(i) else { return C::Unknown };
                let u = bit32(a);
                r = match bfid {
                    lbf::BIT32_BOR => r | u,
                    lbf::BIT32_BXOR => r ^ u,
                    _ => r & u,
                };
            }
            if bfid == lbf::BIT32_BTEST {
                C::Boolean(r != 0)
            } else {
                C::Number(r as f64)
            }
        }
        lbf::BIT32_BNOT => match (n, num(0)) {
            (1, Some(a)) => C::Number((!bit32(a)) as f64),
            _ => C::Unknown,
        },
        lbf::BIT32_EXTRACT => {
            if n >= 2 && num(0).is_some() && num(1).is_some() && (n == 2 || num(2).is_some()) {
                let u = bit32(num(0).unwrap());
                let f = num(1).unwrap() as i32;
                let w = if n == 2 { 1 } else { num(2).unwrap() as i32 };
                if f >= 0 && w > 0 && f + w <= 32 {
                    let m = !(0xfffffffeu32 << (w - 1));
                    return C::Number(((u >> f) & m) as f64);
                }
            }
            C::Unknown
        }
        lbf::BIT32_LROTATE | lbf::BIT32_RROTATE => match (n, num(0), num(1)) {
            (2, Some(a), Some(b)) => {
                let u = bit32(a);
                let s = b as i32;
                let r = if bfid == lbf::BIT32_LROTATE {
                    (u << (s & 31)) | (u >> ((32 - s) & 31))
                } else {
                    (u >> (s & 31)) | (u << ((32 - s) & 31))
                };
                C::Number(r as f64)
            }
            _ => C::Unknown,
        },
        lbf::BIT32_LSHIFT | lbf::BIT32_RSHIFT => match (n, num(0), num(1)) {
            (2, Some(a), Some(b)) => {
                let s = b as i32;
                if (s as u32) < 32 {
                    let u = bit32(a);
                    C::Number(if bfid == lbf::BIT32_LSHIFT { u << s } else { u >> s } as f64)
                } else {
                    C::Unknown
                }
            }
            _ => C::Unknown,
        },
        lbf::BIT32_REPLACE => {
            if n >= 3 && num(0).is_some() && num(1).is_some() && num(2).is_some() && (n == 3 || num(3).is_some()) {
                let nn = bit32(num(0).unwrap());
                let v = bit32(num(1).unwrap());
                let f = num(2).unwrap() as i32;
                let w = if n == 3 { 1 } else { num(3).unwrap() as i32 };
                if f >= 0 && w > 0 && f + w <= 32 {
                    let m = !(0xfffffffeu32 << (w - 1));
                    return C::Number(((nn & !(m << f)) | ((v & m) << f)) as f64);
                }
            }
            C::Unknown
        }
        lbf::TYPE | lbf::TYPEOF => {
            if n == 1 && args[0].is_known() {
                let name: &[u8] = match &args[0] {
                    C::Nil => b"nil",
                    C::Boolean(_) => b"boolean",
                    C::Number(_) => b"number",
                    C::Integer(_) => b"integer",
                    C::Vectorf(_) | C::Vectord(_) => {
                        if bfid == lbf::TYPEOF {
                            return C::Unknown;
                        }
                        b"vector"
                    }
                    C::String(_) => b"string",
                    _ => return C::Unknown,
                };
                C::str(name)
            } else {
                C::Unknown
            }
        }
        lbf::STRING_BYTE => match (n, args.first()) {
            (1, Some(C::String(s))) => {
                if !s.is_empty() {
                    C::Number(s[0] as f64)
                } else {
                    C::Unknown
                }
            }
            (2, Some(C::String(s))) => match num(1) {
                Some(i) => {
                    let i = i as i32;
                    if i > 0 && (i as usize) <= s.len() {
                        C::Number(s[i as usize - 1] as f64)
                    } else {
                        C::Unknown
                    }
                }
                None => C::Unknown,
            },
            _ => C::Unknown,
        },
        lbf::STRING_CHAR => {
            if n < 128 {
                let mut buf = Vec::with_capacity(n);
                for i in 0..n {
                    let Some(x) = num(i) else { return C::Unknown };
                    let ch = x as i32;
                    if (ch as u8) as i32 != ch {
                        return C::Unknown;
                    }
                    buf.push(ch as u8);
                }
                C::String(Rc::from(buf))
            } else {
                C::Unknown
            }
        }
        lbf::STRING_LEN => match (n, args.first()) {
            (1, Some(C::String(s))) => C::Number(s.len() as f64),
            _ => C::Unknown,
        },
        lbf::STRING_SUB => {
            if n >= 2 {
                if let (C::String(s), Some(start)) = (&args[0], num(1)) {
                    if n >= 3 && num(2).is_none() {
                        return C::Unknown;
                    }
                    let len = s.len() as i32;
                    let mut start = start as i32;
                    let mut end = if n >= 3 { num(2).unwrap() as i32 } else { len };
                    if start < 0 {
                        start += len + 1;
                    }
                    if end < 0 {
                        end += len + 1;
                    }
                    if end < 1 {
                        return C::str(b"");
                    }
                    start = start.max(1);
                    end = end.min(len);
                    if start <= end {
                        return C::str(&s[(start - 1) as usize..end as usize]);
                    }
                    return C::str(b"");
                }
            }
            C::Unknown
        }
        lbf::MATH_CLAMP => match (n, num(0), num(1), num(2)) {
            (3, Some(v), Some(lo), Some(hi)) if lo <= hi => {
                let v = if v < lo { lo } else { v };
                C::Number(if v > hi { hi } else { v })
            }
            _ => C::Unknown,
        },
        lbf::MATH_SIGN => one(|v| if v > 0.0 { 1.0 } else if v < 0.0 { -1.0 } else { 0.0 }),
        lbf::MATH_ROUND => one(|x| {
            // C `round`; f64::round lowers to a broken libm routine on MinGW
            let t = x.trunc();
            if (x - t).abs() >= 0.5 {
                t + x.signum()
            } else {
                t
            }
        }),
        lbf::VECTOR => {
            if n >= 2 {
                if let (Some(x), Some(y)) = (num(0), num(1)) {
                    let z = if n >= 3 { num(2) } else { Some(0.0) };
                    let w = if n == 4 { num(3) } else { Some(0.0) };
                    if n <= 4 {
                        if let (Some(z), Some(w)) = (z, w) {
                            return if vector_double {
                                C::Vectord([x, y, z, w])
                            } else {
                                C::Vectorf([x as f32, y as f32, z as f32, w as f32])
                            };
                        }
                    }
                }
            }
            C::Unknown
        }
        lbf::MATH_LERP => match (n, num(0), num(1), num(2)) {
            (3, Some(a), Some(b), Some(t)) => C::Number(if t == 1.0 { b } else { a + (b - a) * t }),
            _ => C::Unknown,
        },
        lbf::MATH_ISNAN | lbf::MATH_ISINF | lbf::MATH_ISFINITE => match (n, num(0)) {
            (1, Some(x)) => C::Boolean(match bfid {
                lbf::MATH_ISNAN => x.is_nan(),
                lbf::MATH_ISINF => x.is_infinite(),
                _ => x.is_finite(),
            }),
            _ => C::Unknown,
        },
        _ => C::Unknown,
    }
}

pub fn fold_builtin_math(index: &str) -> Constant {
    match index {
        "pi" => Constant::Number(std::f64::consts::PI),
        "huge" => Constant::Number(f64::INFINITY),
        "nan" => Constant::Number(f64::NAN),
        "e" => Constant::Number(std::f64::consts::E),
        "phi" => Constant::Number(1.618_033_988_749_895),
        "sqrt2" => Constant::Number(std::f64::consts::SQRT_2),
        "tau" => Constant::Number(std::f64::consts::TAU),
        _ => Constant::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Value tracking (ValueTracking.cpp)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Global {
    Default,
    Mutable,
    Written,
}

/// Initial value of a variable: an expression or (for `local function`) a
/// function node.
#[derive(Clone, Copy)]
pub enum Init<'a> {
    Expr(&'a AstExpr),
    Func(&'a Arc<AstExprFunction>),
}

#[derive(Clone, Default)]
pub struct Variable<'a> {
    pub init: Option<Init<'a>>,
    pub owner: usize,
    pub last_used: Option<Position>,
    pub written: bool,
    pub constant: bool,
    pub non_lexical_use: bool,
}

pub fn global_state(globals: &HashMap<&'static str, Global>, name: &str) -> Global {
    globals.get(name).copied().unwrap_or(Global::Default)
}

pub struct ValueVisitor<'a, 'm> {
    pub globals: &'m mut HashMap<&'static str, Global>,
    pub variables: &'m mut HashMap<LocalKey, Variable<'a>>,
    pub exported_functions: &'m mut HashSet<LocalKey>,
    pub exported_variables: &'m mut Vec<Arc<AstLocal>>,
    block_owners: Vec<usize>,
}

impl<'a, 'm> ValueVisitor<'a, 'm> {
    pub fn run(
        globals: &'m mut HashMap<&'static str, Global>,
        variables: &'m mut HashMap<LocalKey, Variable<'a>>,
        exported_functions: &'m mut HashSet<LocalKey>,
        exported_variables: &'m mut Vec<Arc<AstLocal>>,
        root: &'a AstStatBlock,
    ) {
        let mut v = ValueVisitor { globals, variables, exported_functions, exported_variables, block_owners: Vec::new() };
        v.block(root);
    }

    fn var(&mut self, l: &Arc<AstLocal>) -> &mut Variable<'a> {
        self.variables.entry(local_key(l)).or_default()
    }

    fn use_local(&mut self, e: &AstExprLocal) {
        let v = self.var(&e.local);
        v.last_used = Some(e.location.end);
        if e.upvalue || e.local.is_exported {
            v.non_lexical_use = true;
        }
    }

    fn assign(&mut self, var: &'a AstExpr) {
        match var {
            AstExpr::Local(lv) => {
                self.var(&lv.local).written = true;
                self.use_local(lv);
            }
            AstExpr::Global(g) => {
                self.globals.insert(g.name.0, Global::Written);
            }
            other => self.expr(other),
        }
    }

    fn block(&mut self, b: &'a AstStatBlock) {
        self.block_owners.push(key(b));
        for s in &b.body {
            self.stat(s);
        }
        self.block_owners.pop();
    }

    fn function(&mut self, f: &'a AstExprFunction) {
        for a in &f.args {
            self.var(a).init = None;
        }
        self.block(&f.body);
    }

    fn expr(&mut self, e: &'a AstExpr) {
        match e {
            AstExpr::Local(l) => self.use_local(l),
            AstExpr::Function(f) => self.function(f),
            AstExpr::IfElse(n) => {
                if let Some(cl) = &n.condition_local {
                    self.var(cl).init = Some(Init::Expr(&n.condition));
                }
                self.expr(&n.condition);
                self.expr(&n.true_expr);
                self.expr(&n.false_expr);
            }
            AstExpr::Group(n) => self.expr(&n.expr),
            AstExpr::Call(n) => {
                self.expr(&n.func);
                for a in &n.args {
                    self.expr(a);
                }
            }
            AstExpr::IndexName(n) => self.expr(&n.expr),
            AstExpr::IndexExpr(n) => {
                self.expr(&n.expr);
                self.expr(&n.index);
            }
            AstExpr::Table(n) => {
                for item in &n.items {
                    if let Some(k) = &item.key {
                        self.expr(k);
                    }
                    self.expr(&item.value);
                }
            }
            AstExpr::Unary(n) => self.expr(&n.expr),
            AstExpr::Binary(n) => {
                self.expr(&n.left);
                self.expr(&n.right);
            }
            AstExpr::TypeAssertion(n) => self.expr(&n.expr),
            AstExpr::InterpString(n) => {
                for x in &n.expressions {
                    self.expr(x);
                }
            }
            AstExpr::Instantiate(n) => self.expr(&n.expr),
            AstExpr::Error(n) => {
                for x in &n.expressions {
                    self.expr(x);
                }
            }
            _ => {}
        }
    }

    fn stat(&mut self, s: &'a AstStat) {
        match s {
            AstStat::Block(b) => self.block(b),
            AstStat::Local(n) => {
                for (i, v) in n.vars.iter().enumerate() {
                    let init = n.values.get(i).map(|e| Init::Expr(&**e));
                    self.var(v).init = init;
                }
                for v in &n.vars {
                    if v.is_exported {
                        self.exported_variables.push(v.clone());
                    }
                }
                let owner = *self.block_owners.last().unwrap();
                for v in &n.vars {
                    self.var(v).owner = owner;
                }
                for x in &n.values {
                    self.expr(x);
                }
            }
            AstStat::If(n) => {
                if let Some(cl) = &n.condition_local {
                    self.var(cl).init = Some(Init::Expr(&n.condition));
                }
                self.expr(&n.condition);
                self.block(&n.thenbody);
                if let Some(e) = &n.elsebody {
                    self.stat(e);
                }
            }
            AstStat::While(n) => {
                self.expr(&n.condition);
                self.block(&n.body);
            }
            AstStat::Repeat(n) => {
                self.block(&n.body);
                self.expr(&n.condition);
            }
            AstStat::Return(n) => {
                for x in &n.list {
                    self.expr(x);
                }
            }
            AstStat::Expr(n) => self.expr(&n.expr),
            AstStat::For(n) => {
                self.expr(&n.from);
                self.expr(&n.to);
                if let Some(st) = &n.step {
                    self.expr(st);
                }
                self.block(&n.body);
            }
            AstStat::ForIn(n) => {
                for x in &n.values {
                    self.expr(x);
                }
                self.block(&n.body);
            }
            AstStat::Assign(n) => {
                for v in &n.vars {
                    self.assign(v);
                }
                for x in &n.values {
                    self.expr(x);
                }
            }
            AstStat::CompoundAssign(n) => {
                self.assign(&n.var);
                self.expr(&n.value);
            }
            AstStat::LocalFunction(n) => {
                let owner = *self.block_owners.last().unwrap();
                {
                    let v = self.var(&n.name);
                    v.init = Some(Init::Func(&n.func));
                    v.owner = owner;
                    if n.name.is_exported {
                        v.non_lexical_use = true;
                    }
                }
                if n.name.is_exported {
                    self.exported_functions.insert(local_key(&n.name));
                    self.exported_variables.push(n.name.clone());
                }
                self.function(&n.func);
            }
            AstStat::Function(n) => {
                self.assign(&n.name);
                self.function(&n.func);
            }
            AstStat::TypeFunction(n) => self.function(&n.body),
            AstStat::Class(n) => {
                if let Some(sup) = &n.super_ {
                    self.expr(sup);
                }
                for m in &n.members {
                    if let AstClassMember::Method(m) = m {
                        self.function(&m.function);
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Builtins (Builtins.cpp)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct Builtin {
    pub object: Option<&'static str>,
    pub method: Option<&'static str>,
}

impl Builtin {
    pub fn is_empty(&self) -> bool {
        self.object.is_none() && self.method.is_none()
    }
    pub fn is_global(&self, name: &str) -> bool {
        self.object.is_none() && self.method == Some(name)
    }
}

pub fn get_builtin(
    node: &AstExpr,
    globals: &HashMap<&'static str, Global>,
    variables: &HashMap<LocalKey, Variable>,
) -> Builtin {
    match node {
        AstExpr::Local(e) => match variables.get(&local_key(&e.local)) {
            Some(v) if !v.written => match v.init {
                Some(Init::Expr(init)) => get_builtin(init, globals, variables),
                _ => Builtin::default(),
            },
            _ => Builtin::default(),
        },
        AstExpr::IndexName(e) => {
            if let AstExpr::Local(object) = &*e.expr {
                if let Some(v) = variables.get(&local_key(&object.local)) {
                    if !v.written {
                        if let Some(Init::Expr(init)) = v.init {
                            let g = match init {
                                AstExpr::Global(g) => Some(g.name.0),
                                AstExpr::Binary(b) if b.op == AstExprBinaryOp::Or => match &*b.left {
                                    AstExpr::Global(g) => Some(g.name.0),
                                    _ => None,
                                },
                                _ => None,
                            };
                            if let Some(name) = g {
                                return if global_state(globals, name) == Global::Default {
                                    Builtin { object: Some(name), method: Some(e.index.0) }
                                } else {
                                    Builtin::default()
                                };
                            }
                        }
                    }
                }
            }
            if let AstExpr::Global(object) = &*e.expr {
                if global_state(globals, object.name.0) == Global::Default {
                    return Builtin { object: Some(object.name.0), method: Some(e.index.0) };
                }
            }
            Builtin::default()
        }
        AstExpr::Global(e) => {
            if global_state(globals, e.name.0) == Global::Default {
                Builtin { object: None, method: Some(e.name.0) }
            } else {
                Builtin::default()
            }
        }
        _ => Builtin::default(),
    }
}

/// Host vector options (`CompileOptions::vectorLib`, `vectorCtor`,
/// `vectorType`) for the compilation running on this thread.
#[derive(Clone, Default)]
pub struct HostVector {
    pub lib: Option<&'static str>,
    pub ctor: Option<&'static str>,
    pub ty: Option<&'static str>,
}

thread_local! {
    pub static HOST_VECTOR: std::cell::RefCell<HostVector> = std::cell::RefCell::new(HostVector::default());
}

pub fn host_vector() -> HostVector {
    HOST_VECTOR.with(|h| h.borrow().clone())
}

pub fn get_builtin_function_id(b: &Builtin) -> i32 {
    let id = get_builtin_function_id_base(b);
    if id >= 0 {
        return id;
    }
    let hv = host_vector();
    if let Some(ctor) = hv.ctor {
        match hv.lib {
            Some(lib) => {
                if b.object == Some(lib) && b.method == Some(ctor) {
                    return lbf::VECTOR;
                }
            }
            None => {
                if b.is_global(ctor) {
                    return lbf::VECTOR;
                }
            }
        }
    }
    -1
}

fn get_builtin_function_id_base(b: &Builtin) -> i32 {
    let Some(method) = b.method else { return -1 };
    match b.object {
        None => match method {
            "assert" => lbf::ASSERT,
            "type" => lbf::TYPE,
            "typeof" => lbf::TYPEOF,
            "rawset" => lbf::RAWSET,
            "rawget" => lbf::RAWGET,
            "rawequal" => lbf::RAWEQUAL,
            "rawlen" => lbf::RAWLEN,
            "unpack" => lbf::TABLE_UNPACK,
            "select" => lbf::SELECT_VARARG,
            "getmetatable" => lbf::GETMETATABLE,
            "setmetatable" => lbf::SETMETATABLE,
            "tonumber" => lbf::TONUMBER,
            "tostring" => lbf::TOSTRING,
            _ => -1,
        },
        Some("math") => match method {
            "abs" => lbf::MATH_ABS,
            "acos" => lbf::MATH_ACOS,
            "asin" => lbf::MATH_ASIN,
            "atan2" => lbf::MATH_ATAN2,
            "atan" => lbf::MATH_ATAN,
            "ceil" => lbf::MATH_CEIL,
            "cosh" => lbf::MATH_COSH,
            "cos" => lbf::MATH_COS,
            "deg" => lbf::MATH_DEG,
            "exp" => lbf::MATH_EXP,
            "floor" => lbf::MATH_FLOOR,
            "fmod" => lbf::MATH_FMOD,
            "frexp" => lbf::MATH_FREXP,
            "ldexp" => lbf::MATH_LDEXP,
            "log10" => lbf::MATH_LOG10,
            "log" => lbf::MATH_LOG,
            "max" => lbf::MATH_MAX,
            "min" => lbf::MATH_MIN,
            "modf" => lbf::MATH_MODF,
            "pow" => lbf::MATH_POW,
            "rad" => lbf::MATH_RAD,
            "sinh" => lbf::MATH_SINH,
            "sin" => lbf::MATH_SIN,
            "sqrt" => lbf::MATH_SQRT,
            "tanh" => lbf::MATH_TANH,
            "tan" => lbf::MATH_TAN,
            "clamp" => lbf::MATH_CLAMP,
            "sign" => lbf::MATH_SIGN,
            "round" => lbf::MATH_ROUND,
            "lerp" => lbf::MATH_LERP,
            "isnan" => lbf::MATH_ISNAN,
            "isinf" => lbf::MATH_ISINF,
            "isfinite" => lbf::MATH_ISFINITE,
            _ => -1,
        },
        Some("bit32") => match method {
            "arshift" => lbf::BIT32_ARSHIFT,
            "band" => lbf::BIT32_BAND,
            "bnot" => lbf::BIT32_BNOT,
            "bor" => lbf::BIT32_BOR,
            "bxor" => lbf::BIT32_BXOR,
            "btest" => lbf::BIT32_BTEST,
            "extract" => lbf::BIT32_EXTRACT,
            "lrotate" => lbf::BIT32_LROTATE,
            "lshift" => lbf::BIT32_LSHIFT,
            "replace" => lbf::BIT32_REPLACE,
            "rrotate" => lbf::BIT32_RROTATE,
            "rshift" => lbf::BIT32_RSHIFT,
            "countlz" => lbf::BIT32_COUNTLZ,
            "countrz" => lbf::BIT32_COUNTRZ,
            "byteswap" => lbf::BIT32_BYTESWAP,
            _ => -1,
        },
        Some("string") => match method {
            "byte" => lbf::STRING_BYTE,
            "char" => lbf::STRING_CHAR,
            "len" => lbf::STRING_LEN,
            "sub" => lbf::STRING_SUB,
            _ => -1,
        },
        Some("table") => match method {
            "insert" => lbf::TABLE_INSERT,
            "unpack" => lbf::TABLE_UNPACK,
            "create" => lbf::TABLE_CREATE,
            "clear" => lbf::TABLE_CLEAR,
            _ => -1,
        },
        Some("buffer") => match method {
            "readi8" => lbf::BUFFER_READI8,
            "readu8" => lbf::BUFFER_READU8,
            "writei8" | "writeu8" => lbf::BUFFER_WRITEU8,
            "readi16" => lbf::BUFFER_READI16,
            "readu16" => lbf::BUFFER_READU16,
            "writei16" | "writeu16" => lbf::BUFFER_WRITEU16,
            "readi32" => lbf::BUFFER_READI32,
            "readu32" => lbf::BUFFER_READU32,
            "writei32" | "writeu32" => lbf::BUFFER_WRITEU32,
            "readf32" => lbf::BUFFER_READF32,
            "writef32" => lbf::BUFFER_WRITEF32,
            "readf64" => lbf::BUFFER_READF64,
            "writef64" => lbf::BUFFER_WRITEF64,
            "readinteger" => lbf::BUFFER_READINTEGER,
            "writeinteger" => lbf::BUFFER_WRITEINTEGER,
            _ => -1,
        },
        Some("vector") => match method {
            "create" => lbf::VECTOR,
            "magnitude" => lbf::VECTOR_MAGNITUDE,
            "normalize" => lbf::VECTOR_NORMALIZE,
            "cross" => lbf::VECTOR_CROSS,
            "dot" => lbf::VECTOR_DOT,
            "floor" => lbf::VECTOR_FLOOR,
            "ceil" => lbf::VECTOR_CEIL,
            "abs" => lbf::VECTOR_ABS,
            "sign" => lbf::VECTOR_SIGN,
            "clamp" => lbf::VECTOR_CLAMP,
            "min" => lbf::VECTOR_MIN,
            "max" => lbf::VECTOR_MAX,
            "lerp" => lbf::VECTOR_LERP,
            _ => -1,
        },
        Some("integer") => match method {
            "add" => lbf::INTEGER_ADD,
            "sub" => lbf::INTEGER_SUB,
            "mod" => lbf::INTEGER_MOD,
            "mul" => lbf::INTEGER_MUL,
            "div" => lbf::INTEGER_DIV,
            "idiv" => lbf::INTEGER_IDIV,
            "udiv" => lbf::INTEGER_UDIV,
            "rem" => lbf::INTEGER_REM,
            "urem" => lbf::INTEGER_UREM,
            "min" => lbf::INTEGER_MIN,
            "max" => lbf::INTEGER_MAX,
            "neg" => lbf::INTEGER_NEG,
            "create" => lbf::INTEGER_CREATE,
            "clamp" => lbf::INTEGER_CLAMP,
            "band" => lbf::INTEGER_BAND,
            "bor" => lbf::INTEGER_BOR,
            "bxor" => lbf::INTEGER_BXOR,
            "bnot" => lbf::INTEGER_BNOT,
            "btest" => lbf::INTEGER_BTEST,
            "bswap" => lbf::INTEGER_BSWAP,
            "lt" => lbf::INTEGER_LT,
            "le" => lbf::INTEGER_LE,
            "ult" => lbf::INTEGER_ULT,
            "ule" => lbf::INTEGER_ULE,
            "gt" => lbf::INTEGER_GT,
            "ge" => lbf::INTEGER_GE,
            "ugt" => lbf::INTEGER_UGT,
            "uge" => lbf::INTEGER_UGE,
            "lshift" => lbf::INTEGER_LSHIFT,
            "rshift" => lbf::INTEGER_RSHIFT,
            "arshift" => lbf::INTEGER_ARSHIFT,
            "lrotate" => lbf::INTEGER_LROTATE,
            "rrotate" => lbf::INTEGER_RROTATE,
            "countrz" => lbf::INTEGER_COUNTRZ,
            "countlz" => lbf::INTEGER_COUNTLZ,
            "extract" => lbf::INTEGER_EXTRACT,
            "tonumber" => lbf::INTEGER_TONUMBER,
            _ => -1,
        },
        _ => -1,
    }
}

/// `getBuiltinInfo`: (params, results, none-safe).
pub fn get_builtin_info(bfid: i32) -> (i32, i32, bool) {
    use lbf::*;
    match bfid {
        NONE | ASSERT => (-1, -1, false),
        MATH_ABS | MATH_ACOS | MATH_ASIN | MATH_ATAN | MATH_CEIL | MATH_COSH | MATH_COS | MATH_DEG | MATH_EXP
        | MATH_FLOOR | MATH_LOG10 | MATH_RAD | MATH_SINH | MATH_SIN | MATH_SQRT | MATH_TANH | MATH_TAN
        | MATH_SIGN | MATH_ROUND | BIT32_BNOT | BIT32_COUNTLZ | BIT32_COUNTRZ | STRING_LEN | RAWLEN
        | BIT32_BYTESWAP | VECTOR_MAGNITUDE | VECTOR_NORMALIZE | VECTOR_FLOOR | VECTOR_CEIL | VECTOR_ABS
        | VECTOR_SIGN | MATH_ISNAN | MATH_ISINF | MATH_ISFINITE | INTEGER_BNOT | INTEGER_BSWAP
        | INTEGER_NEG | INTEGER_COUNTLZ | INTEGER_COUNTRZ | INTEGER_TONUMBER | INTEGER_CREATE => (1, 1, true),
        MATH_ATAN2 | MATH_FMOD | MATH_LDEXP | MATH_POW | BIT32_ARSHIFT | BIT32_LROTATE | BIT32_LSHIFT
        | BIT32_RROTATE | BIT32_RSHIFT | BUFFER_READI8 | BUFFER_READU8 | BUFFER_READI16 | BUFFER_READU16
        | BUFFER_READI32 | BUFFER_READU32 | BUFFER_READF32 | BUFFER_READF64 | BUFFER_READINTEGER
        | VECTOR_CROSS | VECTOR_DOT | INTEGER_ADD | INTEGER_SUB | INTEGER_DIV | INTEGER_REM | INTEGER_UDIV
        | INTEGER_UREM | INTEGER_MOD | INTEGER_MUL | INTEGER_IDIV | INTEGER_LT | INTEGER_LE | INTEGER_ULT
        | INTEGER_ULE | INTEGER_GT | INTEGER_GE | INTEGER_UGT | INTEGER_UGE | INTEGER_LSHIFT
        | INTEGER_RSHIFT | INTEGER_ARSHIFT | INTEGER_LROTATE | INTEGER_RROTATE => (2, 1, true),
        MATH_FREXP | MATH_MODF => (1, 2, true),
        MATH_CLAMP | BIT32_EXTRACTK | VECTOR_CLAMP | VECTOR_LERP | MATH_LERP | INTEGER_CLAMP => (3, 1, true),
        BUFFER_WRITEU8 | BUFFER_WRITEU16 | BUFFER_WRITEU32 | BUFFER_WRITEF32 | BUFFER_WRITEF64
        | BUFFER_WRITEINTEGER => (3, 0, true),
        MATH_LOG | MATH_MAX | MATH_MIN | BIT32_BAND | BIT32_BOR | BIT32_BXOR | BIT32_BTEST | BIT32_EXTRACT
        | BIT32_REPLACE | STRING_CHAR | STRING_SUB | TABLE_CREATE | VECTOR | TONUMBER | VECTOR_MIN
        | VECTOR_MAX | INTEGER_BAND | INTEGER_BOR | INTEGER_BXOR | INTEGER_BTEST | INTEGER_MIN
        | INTEGER_MAX | INTEGER_EXTRACT => (-1, 1, false),
        TYPE | TYPEOF | GETMETATABLE | TOSTRING => (1, 1, false),
        STRING_BYTE | TABLE_UNPACK | SELECT_VARARG => (-1, -1, false),
        RAWSET => (3, 1, false),
        RAWGET | RAWEQUAL | SETMETATABLE => (2, 1, false),
        TABLE_INSERT => (-1, 0, false),
        TABLE_CLEAR => (1, 0, false),
        _ => (-1, -1, false),
    }
}

/// `analyzeBuiltins`: maps call nodes to builtin ids.
pub fn analyze_builtins(
    root: &AstStatBlock,
    globals: &HashMap<&'static str, Global>,
    variables: &HashMap<LocalKey, Variable>,
) -> HashMap<ExprKey, i32> {
    struct V<'g, 'v, 'a> {
        globals: &'g HashMap<&'static str, Global>,
        variables: &'v HashMap<LocalKey, Variable<'a>>,
        result: HashMap<ExprKey, i32>,
    }
    impl<'z> Visitor<'z> for V<'_, '_, '_> {
        fn visit_expr(&mut self, e: &AstExpr) -> bool {
            if let AstExpr::Call(call) = e {
                let b = if call.self_ { Builtin::default() } else { get_builtin(&call.func, self.globals, self.variables) };
                if b.is_empty() {
                    return true;
                }
                let mut bfid = get_builtin_function_id(&b);
                if bfid == lbf::SELECT_VARARG && !(call.args.len() == 2 && matches!(&*call.args[1], AstExpr::Varargs(_))) {
                    bfid = -1;
                }
                if bfid >= 0 {
                    self.result.insert(key(e), bfid);
                }
            }
            true
        }
    }
    let mut v = V { globals, variables, result: HashMap::default() };
    walk_block(&mut v, root);
    v.result
}

// ---------------------------------------------------------------------------
// Constant folding (ConstantFolding.cpp)
// ---------------------------------------------------------------------------

/// `buildTableConstantMap`: locals initialized to a table literal that never
/// escapes or gets mutated.
pub fn build_table_constant_map(variables: &HashMap<LocalKey, Variable>, root: &AstStatBlock) -> HashSet<LocalKey> {
    struct Tracker {
        escaped: HashSet<LocalKey>,
    }
    impl Tracker {
        fn mark_escaped(&mut self, mut e: &AstExpr) {
            loop {
                match e {
                    AstExpr::Local(l) => {
                        self.escaped.insert(local_key(&l.local));
                        return;
                    }
                    AstExpr::Group(g) => e = &g.expr,
                    AstExpr::TypeAssertion(t) => e = &t.expr,
                    AstExpr::Instantiate(i) => e = &i.expr,
                    AstExpr::IfElse(ie) => {
                        self.mark_escaped(&ie.true_expr);
                        e = &ie.false_expr;
                    }
                    AstExpr::Binary(b) if matches!(b.op, AstExprBinaryOp::And | AstExprBinaryOp::Or) => {
                        self.mark_escaped(&b.left);
                        e = &b.right;
                    }
                    _ => return,
                }
            }
        }
        fn mark_index(&mut self, e: &AstExpr) {
            match e {
                AstExpr::IndexName(i) => self.mark_escaped(&i.expr),
                AstExpr::IndexExpr(i) => self.mark_escaped(&i.expr),
                _ => {}
            }
        }
        fn mark_binop(&mut self, l: &AstExpr, r: &AstExpr, op: AstExprBinaryOp) {
            use AstExprBinaryOp as B;
            if matches!(op, B::CompareLt | B::CompareLe | B::CompareGt | B::CompareGe | B::CompareEq | B::CompareNe | B::And | B::Or) {
                return;
            }
            self.mark_escaped(l);
            self.mark_escaped(r);
        }
    }
    impl<'z> Visitor<'z> for Tracker {
        fn visit_expr(&mut self, e: &AstExpr) -> bool {
            match e {
                AstExpr::Call(c) => {
                    for a in &c.args {
                        self.mark_escaped(a);
                    }
                    if c.self_ {
                        self.mark_index(&c.func);
                    }
                }
                AstExpr::Table(t) => {
                    for item in &t.items {
                        if let Some(k) = &item.key {
                            self.mark_escaped(k);
                        }
                        self.mark_escaped(&item.value);
                    }
                }
                AstExpr::Binary(b) => self.mark_binop(&b.left, &b.right, b.op),
                AstExpr::IndexExpr(i) => self.mark_escaped(&i.index),
                _ => {}
            }
            true
        }
        fn visit_stat(&mut self, s: &AstStat) -> bool {
            match s {
                AstStat::Local(n) => {
                    for v in n.values.iter().take(n.vars.len()) {
                        self.mark_escaped(v);
                    }
                }
                AstStat::Assign(n) => {
                    for v in &n.values {
                        self.mark_escaped(v);
                    }
                    for v in &n.vars {
                        self.mark_index(v);
                    }
                }
                AstStat::CompoundAssign(n) => {
                    self.mark_index(&n.var);
                    self.mark_binop(&n.var, &n.value, n.op);
                }
                AstStat::Function(n) => self.mark_index(&n.name),
                AstStat::ForIn(n) => {
                    for v in &n.values {
                        self.mark_escaped(v);
                    }
                }
                AstStat::Return(n) => {
                    for v in &n.list {
                        self.mark_escaped(v);
                    }
                }
                _ => {}
            }
            true
        }
    }
    let mut t = Tracker { escaped: HashSet::default() };
    walk_block(&mut t, root);
    let mut out = HashSet::default();
    for (local, var) in variables {
        if var.written {
            continue;
        }
        match var.init {
            Some(Init::Expr(e)) if matches!(unwrap_expr(e), AstExpr::Table(_)) => {}
            _ => continue,
        }
        if !t.escaped.contains(local) {
            out.insert(*local);
        }
    }
    out
}

pub struct ConstantFolder<'m, 'a> {
    pub constants: &'m mut HashMap<ExprKey, Constant>,
    pub variables: &'m mut HashMap<LocalKey, Variable<'a>>,
    pub locals: &'m mut HashMap<LocalKey, Constant>,
    pub builtins: Option<&'m HashMap<ExprKey, i32>>,
    pub fold_library_k: bool,
    pub vector_double: bool,
    pub table_constants: &'m HashSet<LocalKey>,
    constant_tables: Vec<HashMap<Vec<u8>, Constant>>,
    table_locals: HashMap<LocalKey, Constant>,
    was_empty: bool,
    expr_log: Option<&'m mut ChangeLog>,
    local_log: Option<&'m mut ChangeLog>,
}

/// `ExprConstantChangeLog` / `LocalConstantChangeLog`: (key, previous value;
/// `None` when the key was absent), replayed in reverse by [`undo_changes`].
pub type ChangeLog = Vec<(usize, Option<Constant>)>;

/// `undoChanges`.
pub fn undo_changes(map: &mut HashMap<usize, Constant>, log: &ChangeLog) {
    for (k, old) in log.iter().rev() {
        match old {
            None => {
                if let Some(v) = map.get_mut(k) {
                    *v = Constant::Unknown;
                }
            }
            Some(v) => {
                map.insert(*k, v.clone());
            }
        }
    }
}

impl<'m, 'a> ConstantFolder<'m, 'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        constants: &'m mut HashMap<ExprKey, Constant>,
        variables: &'m mut HashMap<LocalKey, Variable<'a>>,
        locals: &'m mut HashMap<LocalKey, Constant>,
        builtins: Option<&'m HashMap<ExprKey, i32>>,
        fold_library_k: bool,
        vector_double: bool,
        table_constants: &'m HashSet<LocalKey>,
        root: &AstStatBlock,
    ) {
        Self::run_on(constants, variables, locals, builtins, fold_library_k, vector_double, table_constants, None, None, |f| {
            walk_block(f, root)
        });
    }

    /// `foldConstants` over an arbitrary node, optionally logging changes.
    #[allow(clippy::too_many_arguments)]
    pub fn run_on(
        constants: &'m mut HashMap<ExprKey, Constant>,
        variables: &'m mut HashMap<LocalKey, Variable<'a>>,
        locals: &'m mut HashMap<LocalKey, Constant>,
        builtins: Option<&'m HashMap<ExprKey, i32>>,
        fold_library_k: bool,
        vector_double: bool,
        table_constants: &'m HashSet<LocalKey>,
        expr_log: Option<&'m mut ChangeLog>,
        local_log: Option<&'m mut ChangeLog>,
        walk: impl FnOnce(&mut ConstantFolder<'m, 'a>),
    ) {
        let was_empty = constants.is_empty() && locals.is_empty();
        let mut f = ConstantFolder {
            constants,
            variables,
            locals,
            builtins,
            fold_library_k,
            vector_double,
            table_constants,
            constant_tables: Vec::new(),
            table_locals: HashMap::default(),
            was_empty,
            expr_log,
            local_log,
        };
        walk(&mut f);
    }

    fn record(map: &mut HashMap<usize, Constant>, was_empty: bool, log: Option<&mut ChangeLog>, k: usize, value: &Constant) {
        match value {
            Constant::Table(_) => {}
            Constant::Unknown => {
                if !was_empty {
                    if let Some(old) = map.get_mut(&k) {
                        if let Some(log) = log {
                            log.push((k, Some(old.clone())));
                        }
                        *old = Constant::Unknown;
                    }
                }
            }
            v => {
                if let Some(log) = log {
                    log.push((k, map.get(&k).cloned()));
                }
                map.insert(k, v.clone());
            }
        }
    }

    fn analyze(&mut self, node: &AstExpr) -> Constant {
        use self::Constant as C;
        let result = match node {
            AstExpr::Group(g) => self.analyze(&g.expr),
            AstExpr::ConstantNil(_) => C::Nil,
            AstExpr::ConstantBool(b) => C::Boolean(b.value),
            AstExpr::ConstantNumber(n) => C::Number(n.value),
            AstExpr::ConstantInteger(n) => C::Integer(n.value),
            AstExpr::ConstantString(s) => C::String(Rc::from(&*s.value)),
            AstExpr::Local(l) => {
                let k = local_key(&l.local);
                if let Some(c) = self.locals.get(&k) {
                    c.clone()
                } else if let Some(c) = self.table_locals.get(&k) {
                    c.clone()
                } else {
                    C::Unknown
                }
            }
            AstExpr::Global(_) | AstExpr::Varargs(_) => C::Unknown,
            AstExpr::Call(call) => {
                self.analyze(&call.func);
                let bfid = self.builtins.and_then(|b| b.get(&key(node))).copied();
                match bfid {
                    Some(bfid) if bfid != lbf::NONE => {
                        let mut args = Vec::with_capacity(call.args.len());
                        let mut can_fold = true;
                        for a in &call.args {
                            let ac = self.analyze(a);
                            if matches!(ac, C::Unknown | C::Table(_)) {
                                can_fold = false;
                            } else {
                                args.push(ac);
                            }
                        }
                        if can_fold {
                            fold_builtin(bfid, &args, self.vector_double)
                        } else {
                            C::Unknown
                        }
                    }
                    _ => {
                        for a in &call.args {
                            self.analyze(a);
                        }
                        C::Unknown
                    }
                }
            }
            AstExpr::IndexName(e) => {
                let value = self.analyze(&e.expr);
                match value {
                    C::Table(t) => self.constant_tables[t].get(e.index.0.as_bytes()).cloned().unwrap_or(C::Unknown),
                    C::Vectorf(v) => match e.index.0 {
                        "x" | "X" => C::Number(v[0] as f64),
                        "y" | "Y" => C::Number(v[1] as f64),
                        "z" | "Z" => C::Number(v[2] as f64),
                        _ => C::Unknown,
                    },
                    C::Vectord(v) => match e.index.0 {
                        "x" | "X" => C::Number(v[0]),
                        "y" | "Y" => C::Number(v[1]),
                        "z" | "Z" => C::Number(v[2]),
                        _ => C::Unknown,
                    },
                    _ => {
                        if self.fold_library_k {
                            if let AstExpr::Global(g) = &*e.expr {
                                if g.name.0 == "math" {
                                    fold_builtin_math(e.index.0)
                                } else {
                                    C::Unknown
                                }
                            } else {
                                C::Unknown
                            }
                        } else {
                            C::Unknown
                        }
                    }
                }
            }
            AstExpr::IndexExpr(e) => {
                let index = self.analyze(&e.index);
                let table = self.analyze(&e.expr);
                match (table, index) {
                    (C::Table(t), C::String(s)) if !s.is_empty() => {
                        self.constant_tables[t].get(&*s).cloned().unwrap_or(C::Unknown)
                    }
                    _ => C::Unknown,
                }
            }
            AstExpr::Function(f) => {
                walk_block(self, &f.body);
                C::Unknown
            }
            AstExpr::Table(t) => {
                let mut props: HashMap<Vec<u8>, Constant> = HashMap::default();
                let mut counted = 0usize;
                for item in &t.items {
                    let value = self.analyze(&item.value);
                    if let Some(k) = &item.key {
                        let kv = self.analyze(k);
                        if let C::String(s) = &kv {
                            if !matches!(value, C::Unknown | C::Table(_)) && !s.is_empty() {
                                props.insert(s.to_vec(), value);
                            }
                        }
                    }
                }
                counted += props.len();
                if counted == t.items.len() {
                    self.constant_tables.push(props);
                    C::Table(self.constant_tables.len() - 1)
                } else {
                    C::Unknown
                }
            }
            AstExpr::Unary(u) => {
                let arg = self.analyze(&u.expr);
                if arg.is_known() { fold_unary(u.op, &arg) } else { C::Unknown }
            }
            AstExpr::Binary(b) => {
                let la = self.analyze(&b.left);
                let ra = self.analyze(&b.right);
                if la.is_known() { fold_binary(b.op, &la, &ra) } else { C::Unknown }
            }
            AstExpr::TypeAssertion(t) => self.analyze(&t.expr),
            AstExpr::IfElse(ie) => {
                let cond = self.analyze(&ie.condition);
                let t = self.analyze(&ie.true_expr);
                let f = self.analyze(&ie.false_expr);
                if cond.is_known() {
                    if cond.is_truthful() { t } else { f }
                } else {
                    C::Unknown
                }
            }
            AstExpr::InterpString(s) => {
                let mut only_const = true;
                let mut parts: Vec<Rc<[u8]>> = Vec::new();
                for x in &s.expressions {
                    match self.analyze(x) {
                        C::String(v) => parts.push(v),
                        _ => only_const = false,
                    }
                }
                if only_const {
                    let total: usize =
                        s.strings.iter().map(|x| x.len()).sum::<usize>() + parts.iter().map(|x| x.len()).sum::<usize>();
                    if total > CONSTANT_FOLD_STRING_LIMIT {
                        C::Unknown
                    } else {
                        let mut out = Vec::with_capacity(total);
                        for (i, st) in s.strings.iter().enumerate() {
                            out.extend_from_slice(st);
                            if let Some(p) = parts.get(i) {
                                out.extend_from_slice(p);
                            }
                        }
                        C::String(Rc::from(out))
                    }
                } else {
                    C::Unknown
                }
            }
            AstExpr::Instantiate(i) => self.analyze(&i.expr),
            AstExpr::Error(_) => C::Unknown,
        };
        Self::record(self.constants, self.was_empty, self.expr_log.as_deref_mut(), key(node), &result);
        result
    }

    fn record_value(&mut self, local: &Arc<AstLocal>, value: &Constant) {
        let k = local_key(local);
        let Some(v) = self.variables.get_mut(&k) else { return };
        if !v.written {
            if let Constant::Table(_) = value {
                v.constant = false;
                self.table_locals.insert(k, value.clone());
            } else {
                v.constant = value.is_known();
                Self::record(self.locals, self.was_empty, self.local_log.as_deref_mut(), k, value);
            }
        }
    }

    fn record_local(&mut self, local: &Arc<AstLocal>, value: &AstExpr) {
        let arg = self.analyze(value);
        if let Constant::Table(_) = arg {
            if self.table_constants.contains(&local_key(local)) {
                self.record_value(local, &arg);
            } else {
                self.record_value(local, &Constant::Unknown);
            }
        } else {
            self.record_value(local, &arg);
        }
    }
}

impl<'z> Visitor<'z> for ConstantFolder<'_, '_> {
    fn visit_expr(&mut self, e: &AstExpr) -> bool {
        if let AstExpr::IfElse(ie) = e {
            if let Some(cl) = &ie.condition_local {
                self.record_local(cl, &ie.condition);
            }
        }
        self.analyze(e);
        false
    }

    fn visit_stat(&mut self, s: &AstStat) -> bool {
        match s {
            AstStat::Local(n) => {
                for (v, e) in n.vars.iter().zip(&n.values) {
                    self.record_local(v, e);
                }
                if n.vars.len() > n.values.len() {
                    let mult = n
                        .values
                        .last()
                        .is_some_and(|l| matches!(&**l, AstExpr::Call(_) | AstExpr::Varargs(_)));
                    if !mult {
                        for v in &n.vars[n.values.len()..] {
                            self.record_value(v, &Constant::Nil);
                        }
                    }
                } else {
                    for e in &n.values[n.vars.len()..] {
                        self.analyze(e);
                    }
                }
                false
            }
            AstStat::If(n) => {
                if let Some(cl) = &n.condition_local {
                    self.record_local(cl, &n.condition);
                    walk_block(self, &n.thenbody);
                    if let Some(e) = &n.elsebody {
                        walk_stat(self, e);
                    }
                    false
                } else {
                    true
                }
            }
            _ => true,
        }
    }
}

// ---------------------------------------------------------------------------
// Table shapes (TableShape.cpp)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct Shape {
    pub array_size: u32,
    pub hash_size: u32,
}

pub fn predict_table_shapes(root: &AstStatBlock) -> HashMap<usize, Shape> {
    const MAX_LOOP_BOUND: f64 = 16.0;

    fn table_hint(e: &AstExpr) -> Option<&AstExprTable> {
        match e {
            AstExpr::Table(t) => Some(t),
            AstExpr::Call(c) if !c.self_ && c.args.len() == 2 => match (&*c.func, &*c.args[0]) {
                (AstExpr::Global(g), AstExpr::Table(t)) if g.name.0 == "setmetatable" => Some(t),
                _ => None,
            },
            _ => None,
        }
    }

    struct V {
        shapes: HashMap<usize, Shape>,
        tables: HashMap<LocalKey, usize>,
        fields: HashSet<(usize, &'static str)>,
        loops: HashMap<LocalKey, u32>,
    }
    impl V {
        fn assign_field_name(&mut self, e: &AstExpr, index: &'static str) {
            if let AstExpr::Local(lv) = e {
                if let Some(&t) = self.tables.get(&local_key(&lv.local)) {
                    if self.fields.insert((t, index)) {
                        self.shapes.entry(t).or_default().hash_size += 1;
                    }
                }
            }
        }
        fn assign_field_expr(&mut self, e: &AstExpr, index: &AstExpr) {
            let AstExpr::Local(lv) = e else { return };
            let Some(&t) = self.tables.get(&local_key(&lv.local)) else { return };
            match index {
                AstExpr::ConstantNumber(n) => {
                    let shape = self.shapes.entry(t).or_default();
                    if n.value == (shape.array_size + 1) as f64 {
                        shape.array_size += 1;
                    }
                }
                AstExpr::Local(iter) => {
                    if let Some(&bound) = self.loops.get(&local_key(&iter.local)) {
                        let shape = self.shapes.entry(t).or_default();
                        if shape.array_size == 0 {
                            shape.array_size = bound;
                        }
                    }
                }
                _ => {}
            }
        }
        fn assign(&mut self, var: &AstExpr) {
            match var {
                AstExpr::IndexName(i) => self.assign_field_name(&i.expr, i.index.0),
                AstExpr::IndexExpr(i) => self.assign_field_expr(&i.expr, &i.index),
                _ => {}
            }
        }
    }
    impl<'z> Visitor<'z> for V {
        fn visit_stat(&mut self, s: &AstStat) -> bool {
            match s {
                AstStat::Local(n) => {
                    if n.vars.len() == 1 && n.values.len() == 1 {
                        if let Some(t) = table_hint(&n.values[0]) {
                            if t.items.is_empty() {
                                self.tables.insert(local_key(&n.vars[0]), key(t));
                            }
                        }
                    }
                    true
                }
                AstStat::Assign(n) => {
                    for v in &n.vars {
                        self.assign(v);
                    }
                    for v in &n.values {
                        walk_expr(self, v);
                    }
                    false
                }
                AstStat::Function(n) => {
                    self.assign(&n.name);
                    walk_function(self, &n.func);
                    false
                }
                AstStat::For(n) => {
                    if let (AstExpr::ConstantNumber(from), AstExpr::ConstantNumber(to)) = (&*n.from, &*n.to) {
                        if from.value == 1.0 && to.value >= 1.0 && to.value <= MAX_LOOP_BOUND && n.step.is_none() {
                            self.loops.insert(local_key(&n.var), to.value as u32);
                        }
                    }
                    true
                }
                _ => true,
            }
        }
    }
    let mut v = V { shapes: HashMap::default(), tables: HashMap::default(), fields: HashSet::default(), loops: HashMap::default() };
    walk_block(&mut v, root);
    v.shapes
}

// ---------------------------------------------------------------------------
// Cost model (CostModel.cpp)
// ---------------------------------------------------------------------------

fn parallel_add_sat(x: u64, y: u64) -> u64 {
    let r = x.wrapping_add(y);
    let s = r & 0x8080_8080_8080_8080;
    (r ^ s) | (s.wrapping_sub(s >> 7))
}

fn parallel_mul_sat(a: u64, b: i32) -> u64 {
    let bs = b.min(127) as u64;
    let l = bs.wrapping_mul(a & 0x007f_007f_007f_007f);
    let h = bs.wrapping_mul((a >> 8) & 0x007f_007f_007f_007f);
    let ls = l.wrapping_add(0x7f80_7f80_7f80_7f80);
    let hs = h.wrapping_add(0x7f80_7f80_7f80_7f80);
    let s = (hs & 0x8000_8000_8000_8000) | ((ls & 0x8000_8000_8000_8000) >> 8);
    let r = ((h & 0x007f_007f_007f_007f) << 8) | (l & 0x007f_007f_007f_007f);
    r | (s.wrapping_sub(s >> 7))
}

#[derive(Clone, Copy, Default)]
struct Cost {
    model: u64,
    constant: u64,
}

const K_LITERAL: u64 = !0;

impl Cost {
    fn new(cost: i32, constant: u64) -> Cost {
        Cost { model: if cost < 0x7f { cost as u64 } else { 0x7f }, constant }
    }
    fn c(cost: i32) -> Cost {
        Cost::new(cost, 0)
    }
    fn add(self, o: Cost) -> Cost {
        Cost { model: parallel_add_sat(self.model, o.model), constant: 0 }
    }
    fn add_assign(&mut self, o: Cost) {
        self.model = parallel_add_sat(self.model, o.model);
        self.constant = 0;
    }
    fn mul(self, o: i32) -> Cost {
        Cost { model: parallel_mul_sat(self.model, o), constant: 0 }
    }
    fn fold(x: Cost, y: Cost) -> Cost {
        let newmodel = parallel_add_sat(x.model, y.model);
        let newconstant = x.constant & y.constant;
        let extra = if newconstant == K_LITERAL { 0 } else { 1 | (0x0101_0101_0101_0101 & newconstant) };
        Cost { model: parallel_add_sat(newmodel, extra), constant: newconstant }
    }
}

struct CostVisitor<'m> {
    builtins: &'m HashMap<ExprKey, i32>,
    constants: &'m HashMap<ExprKey, Constant>,
    vars: HashMap<LocalKey, u64>,
    result: Cost,
}

impl CostVisitor<'_> {
    fn model(&mut self, node: &AstExpr) -> Cost {
        if self.constants.contains_key(&key(node)) {
            return Cost::new(0, K_LITERAL);
        }
        match node {
            AstExpr::Group(g) => self.model(&g.expr),
            AstExpr::ConstantNil(_)
            | AstExpr::ConstantBool(_)
            | AstExpr::ConstantNumber(_)
            | AstExpr::ConstantString(_)
            | AstExpr::ConstantInteger(_) => Cost::new(0, K_LITERAL),
            AstExpr::Local(l) => Cost::new(0, self.vars.get(&local_key(&l.local)).copied().unwrap_or(0)),
            AstExpr::Global(_) => Cost::c(1),
            AstExpr::Varargs(_) => Cost::c(3),
            AstExpr::Call(c) => {
                let bfid = self.builtins.get(&key(node)).copied();
                let builtin = matches!(bfid, Some(b) if b != lbf::NONE);
                let short = builtin && c.args.len() <= 3;
                let mut cost = Cost::c(if builtin { 2 } else { 3 });
                if !builtin {
                    let f = self.model(&c.func);
                    cost.add_assign(f);
                }
                for a in &c.args {
                    let ac = self.model(a);
                    cost.add_assign(if ac.model == 0 && !short { Cost::c(1) } else { ac });
                }
                cost
            }
            AstExpr::IndexName(i) => self.model(&i.expr).add(Cost::c(1)),
            AstExpr::IndexExpr(i) => {
                let a = self.model(&i.expr);
                let b = self.model(&i.index);
                a.add(b).add(Cost::c(1))
            }
            AstExpr::Function(_) => Cost::c(10),
            AstExpr::Table(t) => {
                let mut cost = Cost::c(10);
                for item in &t.items {
                    if let Some(k) = &item.key {
                        let kc = self.model(k);
                        cost.add_assign(kc);
                    }
                    let vc = self.model(&item.value);
                    cost.add_assign(vc);
                    cost.add_assign(Cost::c(1));
                }
                cost
            }
            AstExpr::Unary(u) => {
                let a = self.model(&u.expr);
                Cost::fold(a, Cost::new(0, K_LITERAL))
            }
            AstExpr::Binary(b) => {
                let l = self.model(&b.left);
                let r = self.model(&b.right);
                Cost::fold(l, r)
            }
            AstExpr::TypeAssertion(t) => self.model(&t.expr),
            AstExpr::IfElse(ie) => {
                let cond = self.model(&ie.condition);
                if let Some(cl) = &ie.condition_local {
                    if cond.constant != 0 {
                        self.vars.insert(local_key(cl), cond.constant);
                    }
                }
                let t = self.model(&ie.true_expr);
                let f = self.model(&ie.false_expr);
                cond.add(t).add(f).add(Cost::c(2))
            }
            AstExpr::InterpString(s) => {
                let mut cost = Cost::c(3);
                for x in &s.expressions {
                    let c = self.model(x);
                    cost.add_assign(c);
                }
                cost
            }
            AstExpr::Instantiate(i) => self.model(&i.expr),
            AstExpr::Error(_) => Cost::default(),
        }
    }

    fn assign(&mut self, e: &AstExpr) {
        if let AstExpr::Local(l) = e {
            if let Some(v) = self.vars.get_mut(&local_key(&l.local)) {
                *v = 0;
            }
        }
    }

    fn loop_body(&mut self, body: &AstStatBlock, iter: Cost, factor: i32) {
        let before = self.result;
        self.result = Cost::default();
        walk_block(self, body);
        self.result = before.add(self.result.add(iter).mul(factor));
    }

    fn get_number(&self, e: &AstExpr) -> Option<f64> {
        match self.constants.get(&key(e)) {
            Some(Constant::Number(n)) => Some(*n),
            _ => None,
        }
    }

    fn is_const_true(&self, e: &AstExpr) -> bool {
        matches!(self.constants.get(&key(e)), Some(c) if c.is_known() && c.is_truthful())
    }

    fn is_const_false(&self, e: &AstExpr) -> bool {
        matches!(self.constants.get(&key(e)), Some(c) if c.is_known() && !c.is_truthful())
    }
}

impl<'z> Visitor<'z> for CostVisitor<'_> {
    fn visit_expr(&mut self, e: &AstExpr) -> bool {
        let c = self.model(e);
        self.result.add_assign(c);
        false
    }

    /// Function definitions in statement position (`function f()`, `local
    /// function f()`) are visited as expressions in C++: a flat allocation
    /// cost, without descending into the body.
    fn visit_function(&mut self, _f: &AstExprFunction) -> bool {
        self.result.add_assign(Cost::c(10));
        false
    }

    fn visit_block(&mut self, b: &AstStatBlock) -> bool {
        for s in &b.body {
            walk_stat(self, s);
            if always_terminates(self.constants, s) {
                break;
            }
        }
        false
    }

    fn visit_stat(&mut self, s: &AstStat) -> bool {
        match s {
            AstStat::For(n) => {
                let a = self.model(&n.from);
                self.result.add_assign(a);
                let b = self.model(&n.to);
                self.result.add_assign(b);
                if let Some(st) = &n.step {
                    let c = self.model(st);
                    self.result.add_assign(c);
                }
                let mut trip = -1;
                if let (Some(from), Some(to)) = (self.get_number(&n.from), self.get_number(&n.to)) {
                    let step = match &n.step {
                        Some(st) => self.get_number(st),
                        None => Some(1.0),
                    };
                    if let Some(step) = step {
                        trip = get_trip_count(from, to, step);
                    }
                }
                self.loop_body(&n.body, Cost::c(1), if trip < 0 { 3 } else { trip });
                false
            }
            AstStat::ForIn(n) => {
                for v in &n.values {
                    let c = self.model(v);
                    self.result.add_assign(c);
                }
                self.loop_body(&n.body, Cost::c(1), 3);
                false
            }
            AstStat::While(n) => {
                let cond = self.model(&n.condition);
                if let Some(cl) = &n.condition_local {
                    if cond.constant != 0 {
                        self.vars.insert(local_key(cl), cond.constant);
                    }
                }
                self.loop_body(&n.body, cond, 3);
                false
            }
            AstStat::Repeat(n) => {
                let cond = self.model(&n.condition);
                self.loop_body(&n.body, cond, 3);
                false
            }
            AstStat::If(n) => {
                if let Some(cl) = &n.condition_local {
                    let arg = self.model(&n.condition);
                    if arg.constant != 0 {
                        self.vars.insert(local_key(cl), arg.constant);
                    }
                }
                if self.is_const_false(&n.condition) {
                    if let Some(e) = &n.elsebody {
                        walk_stat(self, e);
                    }
                    return false;
                }
                if self.is_const_true(&n.condition) {
                    walk_block(self, &n.thenbody);
                    return false;
                }
                let extra = matches!(&n.elsebody, Some(e) if !matches!(&**e, AstStat::If(_)));
                self.result.add_assign(Cost::c(1 + extra as i32));
                true
            }
            AstStat::Local(n) => {
                for (i, v) in n.values.iter().enumerate() {
                    let arg = self.model(v);
                    if arg.constant != 0 && i < n.vars.len() {
                        self.vars.insert(local_key(&n.vars[i]), arg.constant);
                    }
                    self.result.add_assign(arg);
                }
                false
            }
            AstStat::Assign(n) => {
                for v in &n.vars {
                    self.assign(v);
                }
                let count = n.vars.len().max(n.values.len());
                for i in 0..count {
                    let mut ac = Cost::default();
                    if let Some(v) = n.vars.get(i) {
                        let c = self.model(v);
                        ac.add_assign(c);
                    }
                    if let Some(v) = n.values.get(i) {
                        let c = self.model(v);
                        ac.add_assign(c);
                    }
                    self.result.add_assign(if ac.model == 0 { Cost::c(1) } else { ac });
                }
                false
            }
            AstStat::CompoundAssign(n) => {
                self.assign(&n.var);
                let c = if matches!(&*n.var, AstExpr::Local(_)) { 1 } else { 2 };
                self.result.add_assign(Cost::c(c));
                true
            }
            AstStat::Break(_) | AstStat::Continue(_) => {
                self.result.add_assign(Cost::c(1));
                false
            }
            _ => true,
        }
    }
}

/// `modelCost`.
pub fn model_cost(
    body: &AstStatBlock,
    vars: &[Arc<AstLocal>],
    builtins: &HashMap<ExprKey, i32>,
    constants: &HashMap<ExprKey, Constant>,
) -> u64 {
    let mut v = CostVisitor { builtins, constants, vars: HashMap::default(), result: Cost::default() };
    for (i, var) in vars.iter().enumerate().take(7) {
        v.vars.insert(local_key(var), 0xffu64 << (i * 8 + 8));
    }
    walk_block(&mut v, body);
    v.result.model
}

/// `computeCost`: the modeled cost with discounts for constant variables.
pub fn compute_cost(model: u64, vars_const: &[bool]) -> i32 {
    let mut cost = (model & 0x7f) as i32;
    if cost == 0x7f {
        return cost;
    }
    for (i, &c) in vars_const.iter().enumerate().take(7) {
        cost -= ((model >> (i * 8 + 8)) & 0x7f) as i32 * c as i32;
    }
    cost
}

/// `getTripCount`.
pub fn get_trip_count(from: f64, to: f64, step: f64) -> i32 {
    let conv = |x: f64| -> Option<i32> {
        if (-32767.0..=32767.0).contains(&x) && (x as i32) as f64 == x {
            Some(x as i32)
        } else {
            None
        }
    };
    let (Some(f), Some(t), Some(s)) = (conv(from), conv(to), conv(step)) else { return -1 };
    if s == 0 {
        return -1;
    }
    if (s < 0 && t > f) || (s > 0 && t < f) {
        return 0;
    }
    (t - f) / s + 1
}

// ---------------------------------------------------------------------------
// Utils.h
// ---------------------------------------------------------------------------

pub fn always_terminates(constants: &HashMap<ExprKey, Constant>, node: &AstStat) -> bool {
    match node {
        AstStat::Block(b) => always_terminates_block(constants, b),
        AstStat::Return(_) | AstStat::Break(_) | AstStat::Continue(_) => true,
        AstStat::If(n) => {
            let c = constants.get(&key(&*n.condition));
            let known = c.is_some_and(|c| c.is_known());
            if known && c.unwrap().is_truthful() {
                return always_terminates_block(constants, &n.thenbody);
            }
            if known && !c.unwrap().is_truthful() {
                if let Some(e) = &n.elsebody {
                    return always_terminates(constants, e);
                }
            }
            match &n.elsebody {
                Some(e) => always_terminates_block(constants, &n.thenbody) && always_terminates(constants, e),
                None => false,
            }
        }
        _ => false,
    }
}

pub fn always_terminates_block(constants: &HashMap<ExprKey, Constant>, b: &AstStatBlock) -> bool {
    b.body.iter().any(|s| always_terminates(constants, s))
}
