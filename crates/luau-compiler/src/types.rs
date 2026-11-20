//! Port of the function-signature part of `Compiler/src/Types.cpp`
//! (`buildTypeMap` -> `functionTypes`): per-function argument type tags that
//! native code uses for its entry guards.
//!
//! Local/upvalue/expression type recording (`typeInfoLevel >= 1` register
//! types) is not ported yet; the VM only consumes the function signature.

use luau_common::fasthash::{HashMap, HashSet};

use luau_ast::ast::*;

use crate::visit::{key, walk_block, walk_expr, walk_stat, Visitor};

pub mod lbc_type {
    pub const NIL: u8 = 0;
    pub const BOOLEAN: u8 = 1;
    pub const NUMBER: u8 = 2;
    pub const STRING: u8 = 3;
    pub const TABLE: u8 = 4;
    pub const FUNCTION: u8 = 5;
    pub const THREAD: u8 = 6;
    pub const USERDATA: u8 = 7;
    pub const VECTOR: u8 = 8;
    pub const BUFFER: u8 = 9;
    pub const INTEGER: u8 = 10;
    pub const ANY: u8 = 15;
    pub const OPTIONAL_BIT: u8 = 1 << 7;
}

const INVALID: u16 = 256;

fn primitive_type(name: &str) -> u16 {
    use lbc_type::*;
    (match name {
        "nil" => NIL,
        "boolean" => BOOLEAN,
        "number" => NUMBER,
        "integer" => INTEGER,
        "string" => STRING,
        "thread" => THREAD,
        "buffer" => BUFFER,
        "vector" => VECTOR,
        "any" | "unknown" => ANY,
        _ => return INVALID,
    }) as u16
}

struct Ctx<'a, 'm> {
    aliases: &'m HashMap<&'static str, &'a AstStatTypeAlias>,
    host_vector_type: Option<&'m str>,
}

fn get_type(ctx: &Ctx, ty: &AstType, generics: &[AstGenericType], seen: &mut HashSet<&'static str>) -> u8 {
    use lbc_type::*;
    match ty {
        AstType::Reference(r) => {
            if r.prefix.is_some() {
                return ANY;
            }
            if let Some(alias) = ctx.aliases.get(r.name.0) {
                if seen.contains(alias.name.0) {
                    return ANY;
                }
                seen.insert(r.name.0);
                let t = get_type(ctx, &alias.ty, &alias.generics, seen);
                seen.remove(r.name.0);
                return t;
            }
            if generics.iter().any(|g| g.name.0 == r.name.0) {
                return ANY;
            }
            if ctx.host_vector_type == Some(r.name.0) {
                return VECTOR;
            }
            let prim = primitive_type(r.name.0);
            if prim != INVALID {
                return prim as u8;
            }
            // host-provided type: assumed userdata
            USERDATA
        }
        AstType::Table(_) => TABLE,
        AstType::Function(_) => FUNCTION,
        AstType::Union(u) => union_type(ctx, u.types.iter().map(|t| &**t), generics, seen),
        // `T?` is a union with an `AstTypeOptional` (nil) member
        AstType::Optional(_) => NIL,
        AstType::Intersection(_) => ANY,
        AstType::Group(g) => get_type(ctx, &g.ty, generics, seen),
        AstType::SingletonBool(_) => BOOLEAN,
        AstType::SingletonString(_) => STRING,
        _ => ANY,
    }
}

fn union_type<'t>(
    ctx: &Ctx,
    types: impl Iterator<Item = &'t AstType>,
    generics: &[AstGenericType],
    seen: &mut HashSet<&'static str>,
) -> u8 {
    use lbc_type::*;
    let mut optional = false;
    let mut ty = INVALID;
    for t in types {
        let et = get_type(ctx, t, generics, seen);
        if et == NIL {
            optional = true;
            continue;
        }
        if ty == INVALID {
            ty = et as u16;
            continue;
        }
        if ty != et as u16 {
            return ANY;
        }
    }
    if ty == INVALID {
        return ANY;
    }
    let ty = ty as u8;
    if optional && ty != ANY {
        ty | OPTIONAL_BIT
    } else {
        ty
    }
}

fn function_type(ctx: &Ctx, f: &AstExprFunction) -> Vec<u8> {
    let has_self = f.self_local.is_some();
    let mut info = Vec::with_capacity(f.args.len() + 3);
    info.push(lbc_type::FUNCTION);
    info.push((has_self as usize + f.args.len()) as u8);
    if has_self {
        info.push(lbc_type::TABLE);
    }
    let mut non_any = false;
    for arg in &f.args {
        let mut seen = HashSet::default();
        let ty = match &arg.annotation {
            Some(a) => get_type(ctx, a, &f.generics, &mut seen),
            None => lbc_type::ANY,
        };
        if ty != lbc_type::ANY {
            non_any = true;
        }
        info.push(ty);
    }
    if !non_any {
        return Vec::new();
    }
    info
}

/// `buildTypeMap` (function signatures only), keyed by function address.
pub fn build_function_types(root: &AstStatBlock, host_vector_type: Option<&str>) -> HashMap<usize, Vec<u8>> {
    struct V<'a, 'h> {
        aliases: HashMap<&'static str, &'a AstStatTypeAlias>,
        stack: Vec<(&'static str, Option<&'a AstStatTypeAlias>)>,
        host_vector_type: Option<&'h str>,
        out: HashMap<usize, Vec<u8>>,
    }
    impl<'a> V<'a, '_> {
        fn push(&mut self, block: &'a AstStatBlock) -> usize {
            let top = self.stack.len();
            for s in &block.body {
                if let AstStat::TypeAlias(a) = &**s {
                    let prev = self.aliases.insert(a.name.0, a);
                    self.stack.push((a.name.0, prev));
                }
            }
            top
        }
        fn pop(&mut self, top: usize) {
            while self.stack.len() > top {
                let (name, prev) = self.stack.pop().unwrap();
                match prev {
                    Some(p) => {
                        self.aliases.insert(name, p);
                    }
                    None => {
                        self.aliases.remove(name);
                    }
                }
            }
        }
    }
    impl<'a> Visitor<'a> for V<'a, '_> {
        fn visit_block(&mut self, b: &'a AstStatBlock) -> bool {
            let top = self.push(b);
            for s in &b.body {
                walk_stat(self, s);
            }
            self.pop(top);
            false
        }
        fn visit_stat(&mut self, s: &'a AstStat) -> bool {
            if let AstStat::Repeat(r) = s {
                // the condition sees the body's aliases
                let top = self.push(&r.body);
                for st in &r.body.body {
                    walk_stat(self, st);
                }
                walk_expr(self, &r.condition);
                self.pop(top);
                return false;
            }
            true
        }
        fn visit_function(&mut self, f: &'a AstExprFunction) -> bool {
            let ctx = Ctx { aliases: &self.aliases, host_vector_type: self.host_vector_type };
            let t = function_type(&ctx, f);
            if !t.is_empty() {
                self.out.insert(key(f), t);
            }
            true
        }
    }
    let mut v = V { aliases: HashMap::default(), stack: Vec::new(), host_vector_type, out: HashMap::default() };
    walk_block(&mut v, root);
    v.out
}

// ---------------------------------------------------------------------------
// Full type map (`TypeMapVisitor`): local and expression types
// ---------------------------------------------------------------------------

/// A resolved type: an annotation from the source, or one of the builtin
/// references the C++ visitor synthesizes (`BuiltinAstTypes`).
#[derive(Clone, Copy)]
enum R<'a> {
    Ast(&'a AstType),
    Builtin(&'static str),
}

/// Type of one expression: the resolved type (`resolvedExprs`) and its
/// bytecode type (`exprTypes`), kept in one map entry (both are recorded for
/// the same expressions, and one lookup is cheaper than two).
#[derive(Clone, Copy)]
pub struct ExprType<'a> {
    r: R<'a>,
    pub bty: u8,
}

/// Result of [`build_type_map`].
#[derive(Default)]
pub struct TypeMap<'a> {
    /// function key -> signature bytes
    pub function_types: HashMap<usize, Vec<u8>>,
    /// local key -> bytecode type
    pub local_types: HashMap<usize, u8>,
    /// expression key -> resolved and bytecode type
    pub expr_types: HashMap<usize, ExprType<'a>>,
}

struct TypeMapVisitor<'a, 'm> {
    aliases: HashMap<&'static str, &'a AstStatTypeAlias>,
    stack: Vec<(&'static str, Option<&'a AstStatTypeAlias>)>,
    builtins: &'m HashMap<usize, i32>,
    globals: &'m HashMap<&'static str, crate::analysis::Global>,
    resolved_locals: HashMap<usize, R<'a>>,
    function_return_types: HashMap<usize, &'a AstType>,
    out: TypeMap<'a>,
}

fn lkey(l: &std::sync::Arc<AstLocal>) -> usize {
    crate::analysis::local_key(l)
}

impl<'a> TypeMapVisitor<'a, '_> {
    fn push(&mut self, block: &'a AstStatBlock) -> usize {
        let top = self.stack.len();
        for s in &block.body {
            if let AstStat::TypeAlias(a) = &**s {
                let prev = self.aliases.insert(a.name.0, a);
                self.stack.push((a.name.0, prev));
            }
        }
        top
    }

    fn pop(&mut self, top: usize) {
        while self.stack.len() > top {
            let (name, prev) = self.stack.pop().unwrap();
            match prev {
                Some(p) => {
                    self.aliases.insert(name, p);
                }
                None => {
                    self.aliases.remove(name);
                }
            }
        }
    }

    fn resolve_aliases(&self, ty: R<'a>) -> R<'a> {
        if self.aliases.is_empty() {
            return ty;
        }
        let name = match ty {
            R::Ast(AstType::Reference(r)) if r.prefix.is_none() => r.name.0,
            R::Builtin(n) => n,
            _ => return ty,
        };
        match self.aliases.get(name) {
            Some(alias) => R::Ast(&alias.ty),
            None => ty,
        }
    }

    fn bc_type(&self, ty: R<'a>) -> u8 {
        let ctx = Ctx { aliases: &self.aliases, host_vector_type: crate::analysis::host_vector().ty };
        let mut seen = HashSet::default();
        match ty {
            R::Ast(t) => get_type(&ctx, t, &[], &mut seen),
            R::Builtin(name) => {
                // a reference to `name` (aliases may shadow builtin names)
                if let Some(alias) = if self.aliases.is_empty() { None } else { self.aliases.get(name) } {
                    seen.insert(name);
                    return get_type(&ctx, &alias.ty, &alias.generics, &mut seen);
                }
                let prim = primitive_type(name);
                if prim != INVALID {
                    prim as u8
                } else {
                    lbc_type::USERDATA
                }
            }
        }
    }

    fn record_expr(&mut self, e: &AstExpr, ty: R<'a>) -> u8 {
        let ty = self.resolve_aliases(ty);
        let bty = self.bc_type(ty);
        self.out.expr_types.insert(key(e), ExprType { r: ty, bty });
        bty
    }

    fn record_local(&mut self, l: &std::sync::Arc<AstLocal>, ty: R<'a>) -> u8 {
        let ty = self.resolve_aliases(ty);
        self.resolved_locals.insert(lkey(l), ty);
        let bty = self.bc_type(ty);
        if bty != lbc_type::ANY {
            self.out.local_types.insert(lkey(l), bty);
        }
        bty
    }

    fn table_indexer(&self, e: &AstExpr) -> Option<(&'a AstType, &'a AstType)> {
        match self.out.expr_types.get(&key(e)).map(|x| x.r)? {
            R::Ast(AstType::Table(t)) => t.indexer.as_ref().map(|i| (&*i.index_type, &*i.result_type)),
            _ => None,
        }
    }

    fn is_global(&self, e: &AstExpr, name: &str) -> bool {
        matches!(e, AstExpr::Global(g) if g.name.0 == name
            && crate::analysis::global_state(self.globals, name) == crate::analysis::Global::Default)
    }

    fn call_result(bfid: i32) -> Option<&'static str> {
        use crate::analysis::lbf::*;
        Some(match bfid {
            MATH_ABS | MATH_ACOS | MATH_ASIN | MATH_ATAN2 | MATH_ATAN | MATH_CEIL | MATH_COSH | MATH_COS | MATH_DEG
            | MATH_EXP | MATH_FLOOR | MATH_FMOD | MATH_FREXP | MATH_LDEXP | MATH_LOG10 | MATH_LOG | MATH_MAX
            | MATH_MIN | MATH_MODF | MATH_POW | MATH_RAD | MATH_SINH | MATH_SIN | MATH_SQRT | MATH_TANH | MATH_TAN
            | BIT32_ARSHIFT | BIT32_BAND | BIT32_BNOT | BIT32_BOR | BIT32_BXOR | BIT32_BTEST | BIT32_EXTRACT
            | BIT32_LROTATE | BIT32_LSHIFT | BIT32_REPLACE | BIT32_RROTATE | BIT32_RSHIFT | STRING_BYTE
            | STRING_LEN | MATH_CLAMP | MATH_SIGN | MATH_ROUND | BIT32_COUNTLZ | BIT32_COUNTRZ | RAWLEN
            | BIT32_EXTRACTK | TONUMBER | BIT32_BYTESWAP | BUFFER_READI8 | BUFFER_READU8 | BUFFER_READI16
            | BUFFER_READU16 | BUFFER_READI32 | BUFFER_READU32 | BUFFER_READF32 | BUFFER_READF64
            | VECTOR_MAGNITUDE | VECTOR_DOT | MATH_LERP | INTEGER_TONUMBER => "number",
            TYPE | STRING_CHAR | TYPEOF | STRING_SUB | TOSTRING => "string",
            MATH_ISNAN | MATH_ISINF | MATH_ISFINITE | RAWEQUAL | INTEGER_LT | INTEGER_LE | INTEGER_GT
            | INTEGER_GE | INTEGER_ULT | INTEGER_ULE | INTEGER_UGT | INTEGER_UGE | INTEGER_BTEST => "boolean",
            VECTOR | VECTOR_NORMALIZE | VECTOR_CROSS | VECTOR_FLOOR | VECTOR_CEIL | VECTOR_ABS | VECTOR_SIGN
            | VECTOR_CLAMP | VECTOR_MIN | VECTOR_MAX | VECTOR_LERP => "vector",
            INTEGER_ADD | INTEGER_SUB | INTEGER_MOD | INTEGER_MUL | INTEGER_DIV | INTEGER_IDIV | INTEGER_UDIV
            | INTEGER_REM | INTEGER_UREM | INTEGER_MAX | INTEGER_MIN | INTEGER_BAND | INTEGER_BOR | INTEGER_BNOT
            | INTEGER_BXOR | INTEGER_LSHIFT | INTEGER_RSHIFT | INTEGER_ARSHIFT | INTEGER_LROTATE | INTEGER_RROTATE
            | INTEGER_EXTRACT | INTEGER_COUNTLZ | INTEGER_COUNTRZ | INTEGER_BSWAP | INTEGER_CLAMP | INTEGER_NEG
            | INTEGER_CREATE | BUFFER_READINTEGER => "integer",
            _ => return None,
        })
    }
}

impl<'a> Visitor<'a> for TypeMapVisitor<'a, '_> {
    fn visit_block(&mut self, b: &'a AstStatBlock) -> bool {
        let top = self.push(b);
        for s in &b.body {
            walk_stat(self, s);
        }
        self.pop(top);
        false
    }

    fn visit_function(&mut self, f: &'a AstExprFunction) -> bool {
        let ctx = Ctx { aliases: &self.aliases, host_vector_type: crate::analysis::host_vector().ty };
        let t = function_type(&ctx, f);
        if !t.is_empty() {
            self.out.function_types.insert(key(f), t);
        }
        true
    }

    fn visit_stat(&mut self, s: &'a AstStat) -> bool {
        match s {
            AstStat::Repeat(r) => {
                let top = self.push(&r.body);
                for st in &r.body.body {
                    walk_stat(self, st);
                }
                walk_expr(self, &r.condition);
                self.pop(top);
                false
            }
            AstStat::For(f) => {
                self.record_local(&f.var, R::Builtin("number"));
                true
            }
            AstStat::ForIn(n) => {
                for e in &n.values {
                    walk_expr(self, e);
                }
                if n.vars.len() == 2 && n.values.len() == 1 {
                    match &*n.values[0] {
                        AstExpr::Call(call) if call.args.len() == 1 => {
                            let arg = &call.args[0];
                            if self.is_global(&call.func, "ipairs") {
                                if let Some((_, rt)) = self.table_indexer(arg) {
                                    self.record_local(&n.vars[0], R::Builtin("number"));
                                    self.record_local(&n.vars[1], R::Ast(rt));
                                }
                            } else if self.is_global(&call.func, "pairs") {
                                if let Some((it, rt)) = self.table_indexer(arg) {
                                    self.record_local(&n.vars[0], R::Ast(it));
                                    self.record_local(&n.vars[1], R::Ast(rt));
                                }
                            }
                        }
                        AstExpr::Call(_) => {}
                        v => {
                            if let Some((it, rt)) = self.table_indexer(v) {
                                self.record_local(&n.vars[0], R::Ast(it));
                                self.record_local(&n.vars[1], R::Ast(rt));
                            }
                        }
                    }
                }
                for v in &n.vars {
                    if let Some(a) = &v.annotation {
                        self.record_local(v, R::Ast(a));
                    }
                }
                walk_block(self, &n.body);
                false
            }
            AstStat::LocalFunction(lf) => {
                if let Some(rt) = &lf.func.return_annotation {
                    if let AstTypePack::Explicit(e) = &**rt {
                        if let Some(first) = e.type_list.types.first() {
                            self.function_return_types.insert(lkey(&lf.name), first);
                        }
                    }
                }
                true
            }
            AstStat::Local(n) => {
                for e in &n.values {
                    walk_expr(self, e);
                }
                for (i, v) in n.vars.iter().enumerate() {
                    if v.annotation.is_none() {
                        if let Some(val) = n.values.get(i) {
                            if let Some(t) = self.out.expr_types.get(&key(&**val)).map(|x| x.r) {
                                self.resolved_locals.insert(lkey(v), t);
                            }
                        }
                    }
                }
                false
            }
            AstStat::If(n) => {
                if let Some(cl) = n.condition_local.as_ref().filter(|cl| cl.annotation.is_none()) {
                    walk_expr(self, &n.condition);
                    if let Some(t) = self.out.expr_types.get(&key(&*n.condition)).map(|x| x.r) {
                        self.resolved_locals.insert(lkey(cl), t);
                    }
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

    fn visit_expr(&mut self, e: &'a AstExpr) -> bool {
        use lbc_type::*;
        match e {
            AstExpr::Local(l) => {
                let local = &l.local;
                if let Some(a) = &local.annotation {
                    let ty = self.record_expr(e, R::Ast(a));
                    if ty != ANY {
                        self.out.local_types.insert(lkey(local), ty);
                    }
                } else if let Some(&t) = self.resolved_locals.get(&lkey(local)) {
                    let ty = self.record_expr(e, t);
                    self.out.local_types.insert(lkey(local), ty);
                }
                false
            }
            AstExpr::IndexExpr(n) => {
                walk_expr(self, &n.expr);
                walk_expr(self, &n.index);
                if let Some((_, rt)) = self.table_indexer(&n.expr) {
                    self.record_expr(e, R::Ast(rt));
                }
                false
            }
            AstExpr::IndexName(n) => {
                walk_expr(self, &n.expr);
                if let Some(R::Ast(AstType::Table(t))) = self.out.expr_types.get(&key(&*n.expr)).map(|x| x.r) {
                    for prop in &t.props {
                        if prop.name.0 == n.index.0 {
                            self.record_expr(e, R::Ast(&prop.ty));
                            return false;
                        }
                    }
                }
                if self.out.expr_types.get(&key(&*n.expr)).map(|x| x.bty) == Some(VECTOR) && matches!(n.index.0, "X" | "Y" | "Z" | "x" | "y" | "z") {
                    self.record_expr(e, R::Builtin("number"));
                    return false;
                }
                if let AstExpr::Global(g) = &*n.expr {
                    if g.name.0 == "vector"
                        && matches!(n.index.0, "zero" | "one")
                        && crate::analysis::global_state(self.globals, "vector") == crate::analysis::Global::Default
                    {
                        self.record_expr(e, R::Builtin("vector"));
                        return false;
                    }
                }
                false
            }
            AstExpr::Unary(n) => {
                walk_expr(self, &n.expr);
                match n.op {
                    AstExprUnaryOp::Not => {
                        self.record_expr(e, R::Builtin("boolean"));
                    }
                    AstExprUnaryOp::Minus => {
                        let t = self.out.expr_types.get(&key(&*n.expr)).map(|x| x.r);
                        let bt = self.out.expr_types.get(&key(&*n.expr)).map(|x| x.bty);
                        if let (Some(t), Some(bt)) = (t, bt) {
                            if bt == VECTOR || bt == NUMBER {
                                self.record_expr(e, t);
                            }
                        }
                    }
                    AstExprUnaryOp::Len => {
                        self.record_expr(e, R::Builtin("number"));
                    }
                }
                false
            }
            AstExpr::Binary(n) => {
                walk_expr(self, &n.left);
                walk_expr(self, &n.right);
                use AstExprBinaryOp as B;
                match n.op {
                    B::CompareNe | B::CompareEq | B::CompareLt | B::CompareLe | B::CompareGt | B::CompareGe => {
                        self.record_expr(e, R::Builtin("boolean"));
                        return false;
                    }
                    B::Concat | B::And | B::Or => return false,
                    _ => {}
                }
                let (Some(lt), Some(lb)) =
                    (self.out.expr_types.get(&key(&*n.left)).map(|x| x.r), self.out.expr_types.get(&key(&*n.left)).map(|x| x.bty))
                else {
                    return false;
                };
                let (Some(rt), Some(rb)) =
                    (self.out.expr_types.get(&key(&*n.right)).map(|x| x.r), self.out.expr_types.get(&key(&*n.right)).map(|x| x.bty))
                else {
                    return false;
                };
                if lb == VECTOR {
                    self.record_expr(e, lt);
                } else if rb == VECTOR {
                    self.record_expr(e, rt);
                } else if lb == NUMBER && rb == NUMBER {
                    self.record_expr(e, lt);
                }
                false
            }
            AstExpr::Group(g) => {
                walk_expr(self, &g.expr);
                if let Some(t) = self.out.expr_types.get(&key(&*g.expr)).map(|x| x.r) {
                    self.record_expr(e, t);
                }
                false
            }
            AstExpr::TypeAssertion(t) => {
                walk_expr(self, &t.expr);
                self.record_expr(e, R::Ast(&t.annotation));
                false
            }
            AstExpr::ConstantBool(_) => {
                self.record_expr(e, R::Builtin("boolean"));
                false
            }
            AstExpr::ConstantNumber(_) => {
                self.record_expr(e, R::Builtin("number"));
                false
            }
            AstExpr::ConstantInteger(_) => {
                self.record_expr(e, R::Builtin("integer"));
                false
            }
            AstExpr::ConstantString(_) | AstExpr::InterpString(_) => {
                self.record_expr(e, R::Builtin("string"));
                false
            }
            AstExpr::IfElse(n) => {
                walk_expr(self, &n.condition);
                if let Some(cl) = n.condition_local.as_ref().filter(|cl| cl.annotation.is_none()) {
                    if let Some(t) = self.out.expr_types.get(&key(&*n.condition)).map(|x| x.r) {
                        self.resolved_locals.insert(lkey(cl), t);
                    }
                }
                walk_expr(self, &n.true_expr);
                walk_expr(self, &n.false_expr);
                let tt = self.out.expr_types.get(&key(&*n.true_expr)).map(|x| x.r);
                let tb = self.out.expr_types.get(&key(&*n.true_expr)).map(|x| x.bty);
                let fb = self.out.expr_types.get(&key(&*n.false_expr)).map(|x| x.bty);
                if let (Some(tt), Some(tb), Some(fb)) = (tt, tb, fb) {
                    if tb == fb {
                        self.record_expr(e, tt);
                    }
                }
                false
            }
            AstExpr::Call(c) => {
                if let Some(&bfid) = self.builtins.get(&key(e)) {
                    if let Some(name) = Self::call_result(bfid) {
                        self.record_expr(e, R::Builtin(name));
                    }
                } else if let AstExpr::Local(l) = &*c.func {
                    if let Some(&t) = self.function_return_types.get(&lkey(&l.local)) {
                        self.record_expr(e, R::Ast(t));
                    }
                }
                true
            }
            _ => true,
        }
    }
}

/// `buildTypeMap`: function signatures, local types and expression types.
pub fn build_type_map<'a>(
    root: &'a AstStatBlock,
    builtins: &HashMap<usize, i32>,
    globals: &HashMap<&'static str, crate::analysis::Global>,
) -> TypeMap<'a> {
    let mut v = TypeMapVisitor {
        aliases: HashMap::default(),
        stack: Vec::new(),
        builtins,
        globals,
        resolved_locals: HashMap::default(),
        function_return_types: HashMap::default(),
        out: TypeMap { expr_types: luau_common::fasthash::map_hinted(), ..TypeMap::default() },
    };
    walk_block(&mut v, root);
    v.out
}
