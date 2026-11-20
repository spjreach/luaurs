//! Port of `Ast/include/Luau/Ast.h` — the Luau AST node definitions.
//!
//! The C++ hierarchy is a single-rooted class tree rooted at `AstNode`, with
//! subclasses `AstExpr*`, `AstStat*`, `AstType*`, `AstTypePack*`,
//! `AstGenericType*` and `AstAttr`. This port represents each C++ subclass
//! hierarchy as a Rust enum whose variants hold a boxed payload struct carrying
//! the fields of the corresponding C++ class (the C++ base-class `location`
//! field is replicated in every payload struct).
//!
//! Ownership map from C++ pointers:
//! - `AstExpr*` / `AstStat*` / `AstType*` / `AstTypePack*` children become
//!   `Box<...>` (required) or `Option<Box<...>>` (nullable), matching
//!   single-pointer fields.
//! - `AstArray<T>` fields become `Vec<...>`; arrays of base-class pointers
//!   become `Vec<Box<...>>` to keep recursive node sizes finite.
//! - `AstLocal*` becomes `Arc<AstLocal>`: C++ relies on identity of locals (the
//!   same `AstLocal*` appears in `AstStatLocal::vars`, `AstExprLocal::local` and
//!   `AstExprFunction::args`), which `Arc` preserves. Self-referencing cycles (a
//!   local's annotation naming the local itself via
//!   `AstTypeReference::prefixLocal`) may leak, mirroring the arena semantics
//!   of the C++ front end (which frees the whole AST in one arena release).
//!
//! Visitor: see [`AstVisitor`]. The C++ `AstVisitor::visit` boolean return
//! value ("should we recurse into children?") is replaced by separate `visit_*`
//! (pre-order callbacks) and `walk_*` (visit + recurse) default members.
//! Two payloads that C++ reaches via a base-class pointer away from an enum
//! (`AstStatBlock` bodies, `AstExprFunction` bodies of statements) get their
//! own payload-level hooks (`visit_block`, `visit_function`) because no
//! borrowed enum wrapper exists at those sites; a visitor that wants to see
//! every statement/expression implements those hooks alongside
//! `visit_stat`/`visit_expr`.

use std::sync::Arc;

use crate::location::{Location, Position};

// ---------------------------------------------------------------------------
// AstName
// ---------------------------------------------------------------------------

/// A name interned by the parser's `AstNameTable` (C++ `AstName`).
///
/// C++ compares `AstName`s by interned-pointer identity; here names are
/// interned (deduplicated) `&'static str`s, so content equality is equivalent
/// to pointer equality. The default (null) C++ `AstName` is represented by
/// [`AstName::empty`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct AstName(pub &'static str);

impl AstName {
    /// The null/empty name (`AstName()` in C++, where `value == nullptr`).
    #[inline]
    pub fn empty() -> Self {
        AstName("")
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[inline]
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for AstName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

// ---------------------------------------------------------------------------
// Shared helper nodes (C++ free structs / AstNode subclasses outside the
// Expr/Stat/Type hierarchies)
// ---------------------------------------------------------------------------

/// A declared local (C++ `AstLocal`).
///
/// `shadow` is the (nullable) local this declaration shadows;
/// `function_depth`/`loop_depth` are the scope depths; `is_exported` is only a
/// property set after construction (see C++ comment).
#[derive(Debug, Clone)]
pub struct AstLocal {
    pub name: AstName,
    pub location: Location,
    /// Pointer to the previous local with the same name (C++ `AstLocal* shadow`).
    pub shadow: Option<Arc<AstLocal>>,
    pub function_depth: usize,
    pub loop_depth: usize,
    pub is_const: bool,
    /// C++ field `isExported`.
    pub is_exported: bool,
    pub annotation: Option<Box<AstType>>,
}

/// A tag/name + its location (C++ `AstArgumentName`, a `std::pair<AstName, Location>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AstArgumentName {
    pub name: AstName,
    pub location: Location,
}

/// A generic type parameter (C++ `AstGenericType`).
#[derive(Debug, Clone)]
pub struct AstGenericType {
    pub location: Location,
    pub name: AstName,
    pub default_value: Option<Box<AstType>>,
}

/// A generic type-pack parameter (C++ `AstGenericTypePack`).
#[derive(Debug, Clone)]
pub struct AstGenericTypePack {
    pub location: Location,
    pub name: AstName,
    pub default_value: Option<Box<AstTypePack>>,
}

/// A list of types plus an optional trailing variadic tail (C++ `AstTypeList`).
///
/// A null tail indicates "no tail", not an untyped tail (see C++ comment).
#[derive(Debug, Clone, Default)]
pub struct AstTypeList {
    pub types: Vec<Box<AstType>>,
    pub tail_type: Option<Box<AstTypePack>>,
}

/// Holds either a [`AstType`] or a [`AstTypePack`] (C++ `AstTypeOrPack`).
///
/// Exactly one of the two should be present.
#[derive(Debug, Clone)]
pub struct AstTypeOrPack {
    /// C++ field `type`.
    pub ty: Option<Box<AstType>>,
    /// C++ field `typePack`.
    pub type_pack: Option<Box<AstTypePack>>,
}

// ---------------------------------------------------------------------------
// AstAttr (attributes: @checked / @native / @deprecated ...)
// ---------------------------------------------------------------------------

/// Attribute tag (C++ `AstAttr::Type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AstAttrKind {
    Checked,
    Native,
    Deprecated,
    DebugNoinline,
    Unknown,
}

/// C++ `AstAttr::DeprecatedInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AstAttrDeprecatedInfo {
    pub deprecated: bool,
    /// C++ field `use` (renamed: `use` is a Rust keyword).
    pub use_: Option<String>,
    pub reason: Option<String>,
}

/// An `@attribute` node attached to functions (C++ `AstAttr`).
#[derive(Debug, Clone)]
pub struct AstAttr {
    pub location: Location,
    /// C++ field `type`.
    pub kind: AstAttrKind,
    pub args: Vec<Box<AstExpr>>,
    /// Deprecated attributes can carry a name (second C++ constructor).
    pub name: AstName,
}

impl AstAttr {
    /// Port of `AstAttr::deprecatedInfo`: extracts the
    /// `@deprecated(use=..., reason=...)` payload from the first argument when
    /// it is a table literal.
    pub fn deprecated_info(&self) -> AstAttrDeprecatedInfo {
        let mut info = AstAttrDeprecatedInfo {
            deprecated: self.kind == AstAttrKind::Deprecated,
            use_: None,
            reason: None,
        };

        if info.deprecated && !self.args.is_empty() {
            if let AstExpr::Table(table) = self.args[0].as_ref() {
                if let Some(expr) = table.get_record(b"use") {
                    if let AstExpr::ConstantString(value) = expr {
                        info.use_ = Some(String::from_utf8_lossy(&value.value).into_owned());
                    }
                }
                if let Some(expr) = table.get_record(b"reason") {
                    if let AstExpr::ConstantString(value) = expr {
                        info.reason = Some(String::from_utf8_lossy(&value.value).into_owned());
                    }
                }
            }
        }

        info
    }

    /// The node's `location` (`AstNode::location`).
    pub fn location(&self) -> Location {
        self.location
    }
}

impl AstGenericType {
    /// The node's `location` (`AstNode::location`).
    pub fn location(&self) -> Location {
        self.location
    }
}

impl AstGenericTypePack {
    /// The node's `location` (`AstNode::location`).
    pub fn location(&self) -> Location {
        self.location
    }
}

impl AstLocal {
    /// The node's `location`.
    pub fn location(&self) -> Location {
        self.location
    }
}

// ---------------------------------------------------------------------------
// Constant value representation (C++ `Compile::Constant` from
// Compiler/src/ConstantFolding.h — the folded value carried alongside AST
// nodes by the compiler front end)
// ---------------------------------------------------------------------------

/// Tag of a folded constant (C++ `Constant::Type`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConstantKind {
    /// C++ `Type_Unknown` — not a compile-time value.
    Unknown,
    Nil,
    Boolean(bool),
    /// C++ `Type_Number` (64-bit double).
    Number(f64),
    /// C++ `Type_Integer` (64-bit integer).
    Integer(i64),
    /// C++ `Type_Vectorf` (single-precision vector components).
    Vectorf([f32; 3]),
    /// C++ `Type_Vectord` (double-precision vector components).
    Vectord([f64; 3]),
    /// C++ `Type_String`; C++ stores `(const char*, stringLength)`, here the
    /// interned string carries its own length.
    String(&'static str),
    /// C++ `Type_Table`: index into the constant table entry holding the
    /// table's constant properties (C++ field `valueTable`).
    Table(usize),
}

/// A folded constant value (C++ `Compile::Constant`): a `Type` tag plus a
/// value union in C++, expressed as a tagged enum here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Constant {
    pub kind: ConstantKind,
}

impl Constant {
    /// Port of `Constant::isTruthful` (debug-asserts non-Unknown in C++).
    pub fn is_truthful(&self) -> bool {
        debug_assert!(
            self.kind != ConstantKind::Unknown,
            "is_truthful called on Unknown constant"
        );
        match self.kind {
            ConstantKind::Nil => false,
            ConstantKind::Boolean(false) => false,
            _ => true,
        }
    }
}

/// Result of parsing a numeric literal (C++ `ConstantNumberParseResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConstantNumberParseResult {
    Ok,
    Imprecise,
    Malformed,
    BinOverflow,
    HexOverflow,
    IntOverflow,
}

// ---------------------------------------------------------------------------
// AstExpr hierarchy
// ---------------------------------------------------------------------------

/// Unary operator (C++ `AstExprUnary::Op`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AstExprUnaryOp {
    Not,
    Minus,
    Len,
}

impl AstExprUnaryOp {
    /// Port of `toString(AstExprUnary::Op)`.
    pub fn to_str(self) -> &'static str {
        match self {
            AstExprUnaryOp::Minus => "-",
            AstExprUnaryOp::Not => "not",
            AstExprUnaryOp::Len => "#",
        }
    }
}

impl std::fmt::Display for AstExprUnaryOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.to_str())
    }
}

/// Binary operator (C++ `AstExprBinary::Op`).
///
/// C++ ends the enum with an `Op__Count` sentinel; use
/// [`AstExprBinaryOp::COUNT`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AstExprBinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    Concat,
    CompareNe,
    CompareEq,
    CompareLt,
    CompareLe,
    CompareGt,
    CompareGe,
    And,
    Or,
}

impl AstExprBinaryOp {
    /// The C++ `Op__Count` sentinel.
    pub const COUNT: usize = 16;

    /// Port of `toString(AstExprBinary::Op)`.
    pub fn to_str(self) -> &'static str {
        match self {
            AstExprBinaryOp::Add => "+",
            AstExprBinaryOp::Sub => "-",
            AstExprBinaryOp::Mul => "*",
            AstExprBinaryOp::Div => "/",
            AstExprBinaryOp::FloorDiv => "//",
            AstExprBinaryOp::Mod => "%",
            AstExprBinaryOp::Pow => "^",
            AstExprBinaryOp::Concat => "..",
            AstExprBinaryOp::CompareNe => "~=",
            AstExprBinaryOp::CompareEq => "==",
            AstExprBinaryOp::CompareLt => "<",
            AstExprBinaryOp::CompareLe => "<=",
            AstExprBinaryOp::CompareGt => ">",
            AstExprBinaryOp::CompareGe => ">=",
            AstExprBinaryOp::And => "and",
            AstExprBinaryOp::Or => "or",
        }
    }
}

impl std::fmt::Display for AstExprBinaryOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.to_str())
    }
}

/// C++ `AstExprGroup`: `(` expr `)`.
#[derive(Debug, Clone)]
pub struct AstExprGroup {
    pub location: Location,
    pub expr: Box<AstExpr>,
}

/// C++ `AstExprConstantNil`.
#[derive(Debug, Clone)]
pub struct AstExprConstantNil {
    pub location: Location,
}

/// C++ `AstExprConstantBool`.
#[derive(Debug, Clone)]
pub struct AstExprConstantBool {
    pub location: Location,
    pub value: bool,
}

/// C++ `AstExprConstantNumber`.
#[derive(Debug, Clone)]
pub struct AstExprConstantNumber {
    pub location: Location,
    pub value: f64,
    pub parse_result: ConstantNumberParseResult,
}

/// C++ `AstExprConstantInteger`.
#[derive(Debug, Clone)]
pub struct AstExprConstantInteger {
    pub location: Location,
    pub value: i64,
    pub parse_result: ConstantNumberParseResult,
}

/// Quote style of a string literal (C++ `AstExprConstantString::QuoteStyle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AstExprConstantStringQuoteStyle {
    /// `"foo"` / interpolated simple string.
    QuotedSimple,
    /// `'bar'`
    QuotedSingle,
    /// `[[ Gee, ... ]]` raw string.
    QuotedRaw,
    /// Unquoted "string" context, e.g. `{ foo = 42 }` — `foo` here.
    Unquoted,
}

/// C++ `AstExprConstantString`.
///
/// `value` holds the decoded literal bytes (C++ `AstArray<char>`).
#[derive(Debug, Clone)]
pub struct AstExprConstantString {
    pub location: Location,
    pub value: Box<[u8]>,
    pub quote_style: AstExprConstantStringQuoteStyle,
}

impl AstExprConstantString {
    /// Port of `AstExprConstantString::isQuoted`.
    pub fn is_quoted(&self) -> bool {
        matches!(
            self.quote_style,
            AstExprConstantStringQuoteStyle::QuotedSimple
                | AstExprConstantStringQuoteStyle::QuotedSingle
                | AstExprConstantStringQuoteStyle::QuotedRaw
        )
    }
}

/// C++ `AstExprLocal`.
#[derive(Debug, Clone)]
pub struct AstExprLocal {
    pub location: Location,
    pub local: Arc<AstLocal>,
    /// True if `local` refers to an enclosing function's local (upvalue).
    pub upvalue: bool,
}

/// C++ `AstExprGlobal`.
#[derive(Debug, Clone)]
pub struct AstExprGlobal {
    pub location: Location,
    pub name: AstName,
}

/// C++ `AstExprVarargs` (`...`).
#[derive(Debug, Clone)]
pub struct AstExprVarargs {
    pub location: Location,
}

/// C++ `AstExprCall`.
#[derive(Debug, Clone)]
pub struct AstExprCall {
    pub location: Location,
    pub func: Box<AstExpr>,
    /// Only filled specifically for `t:f<<A, B>>()`; in `f<<A, B>>()` the
    /// annotation is parsed as the `f<<A, B>>` expression, which is then
    /// called (see C++ comment).
    pub type_arguments: Vec<AstTypeOrPack>,
    pub args: Vec<Box<AstExpr>>,
    /// Method call with implicit self (`m:foo()`) (C++ field `self`).
    pub self_: bool,
    /// Direct table call (`{...}()`); set post-construction in C++.
    pub table_call: bool,
    pub arg_location: Location,
}

/// C++ `AstExprIndexName`: `expr.name` / `expr:name`.
#[derive(Debug, Clone)]
pub struct AstExprIndexName {
    pub location: Location,
    pub expr: Box<AstExpr>,
    pub index: AstName,
    pub index_location: Location,
    pub op_position: Position,
    /// `.` or `:` (C++ field `op`, default `'.'`).
    pub op: char,
    /// Private field/method access made within a class method body.
    pub private_access: bool,
}

/// C++ `AstExprIndexExpr`: `expr[index]`.
#[derive(Debug, Clone)]
pub struct AstExprIndexExpr {
    pub location: Location,
    pub expr: Box<AstExpr>,
    pub index: Box<AstExpr>,
}

/// C++ `AstExprFunction`.
#[derive(Debug, Clone)]
pub struct AstExprFunction {
    pub location: Location,
    pub attributes: Vec<AstAttr>,
    pub generics: Vec<AstGenericType>,
    pub generic_packs: Vec<AstGenericTypePack>,
    /// Implicit `self` for method definitions (C++ field `self`, nullable).
    pub self_local: Option<Arc<AstLocal>>,
    pub args: Vec<Arc<AstLocal>>,
    pub return_annotation: Option<Box<AstTypePack>>,
    pub vararg: bool,
    pub vararg_location: Location,
    pub vararg_annotation: Option<Box<AstTypePack>>,
    pub body: Box<AstStatBlock>,
    pub function_depth: usize,
    pub debugname: AstName,
    /// Location of the `(`/`args` list when present
    /// (C++ `std::optional<Location> argLocation`).
    pub arg_location: Option<Location>,
}

/// Kind of a table-literal item (C++ `AstExprTable::Item::Kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AstExprTableItemKind {
    /// `foo` — `key` is absent (C++ comment: key is a nullptr).
    List,
    /// `foo=bar` — `key` is an `AstExprConstantString`.
    Record,
    /// `[foo]=bar`.
    General,
}

/// C++ `AstExprTable::Item`.
#[derive(Debug, Clone)]
pub struct AstExprTableItem {
    pub kind: AstExprTableItemKind,
    /// C++ comment: "can be nullptr!"
    pub key: Option<Box<AstExpr>>,
    pub value: Box<AstExpr>,
}

/// C++ `AstExprTable`.
#[derive(Debug, Clone)]
pub struct AstExprTable {
    pub location: Location,
    pub items: Vec<AstExprTableItem>,
}

impl AstExprTable {
    /// Port of `AstExprTable::getRecord`: the value of the `Record` item whose
    /// key string equals `key`, if any. (C++ compares with `strcmp` on the
    /// first constant string arg.)
    pub fn get_record(&self, key: &[u8]) -> Option<&AstExpr> {
        self.items.iter().find_map(|item| {
            if item.kind != AstExprTableItemKind::Record {
                return None;
            }
            match item.key.as_deref() {
                Some(AstExpr::ConstantString(value)) if value.value.as_ref() == key => {
                    Some(item.value.as_ref())
                }
                _ => None,
            }
        })
    }
}

/// C++ `AstExprUnary`.
#[derive(Debug, Clone)]
pub struct AstExprUnary {
    pub location: Location,
    pub op: AstExprUnaryOp,
    pub expr: Box<AstExpr>,
}

/// C++ `AstExprBinary`.
#[derive(Debug, Clone)]
pub struct AstExprBinary {
    pub location: Location,
    pub op: AstExprBinaryOp,
    pub left: Box<AstExpr>,
    pub right: Box<AstExpr>,
}

/// C++ `AstExprTypeAssertion`: `expr :: annotation`.
#[derive(Debug, Clone)]
pub struct AstExprTypeAssertion {
    pub location: Location,
    pub expr: Box<AstExpr>,
    pub annotation: Box<AstType>,
}

/// C++ `AstExprIfElse`: `if c then a else b`.
#[derive(Debug, Clone)]
pub struct AstExprIfElse {
    pub location: Location,
    pub condition: Box<AstExpr>,
    pub has_then: bool,
    pub true_expr: Box<AstExpr>,
    pub has_else: bool,
    pub false_expr: Box<AstExpr>,
    /// Active for `if local` / `if const` expressions; `condition_local` is
    /// bound to `condition` and in scope for `true_expr` only.
    pub condition_local: Option<Arc<AstLocal>>,
    pub condition_is_const: bool,
    pub condition_keyword_location: Option<Location>,
    /// Location of the `=` in an `if local`/`if const` binding.
    pub condition_equals_location: Option<Location>,
}

/// C++ `AstExprInterpString`: `` `foo{bar}baz` ``.
///
/// `strings` always has one more element than `expressions`.
#[derive(Debug, Clone)]
pub struct AstExprInterpString {
    pub location: Location,
    pub strings: Vec<Box<[u8]>>,
    pub expressions: Vec<Box<AstExpr>>,
}

/// C++ `AstExprInstantiate`: `f<<T>>`.
#[derive(Debug, Clone)]
pub struct AstExprInstantiate {
    pub location: Location,
    pub expr: Box<AstExpr>,
    pub type_arguments: Vec<AstTypeOrPack>,
}

/// C++ `AstExprError`: parse-error placeholder.
#[derive(Debug, Clone)]
pub struct AstExprError {
    pub location: Location,
    pub expressions: Vec<Box<AstExpr>>,
    pub message_index: u32,
}

/// An expression node (`AstExpr` in C++ plus one enum per subclass).
#[derive(Debug, Clone)]
pub enum AstExpr {
    /// `AstExprGroup`
    Group(Box<AstExprGroup>),
    /// `AstExprConstantNil`
    ConstantNil(Box<AstExprConstantNil>),
    /// `AstExprConstantBool`
    ConstantBool(Box<AstExprConstantBool>),
    /// `AstExprConstantNumber`
    ConstantNumber(Box<AstExprConstantNumber>),
    /// `AstExprConstantInteger`
    ConstantInteger(Box<AstExprConstantInteger>),
    /// `AstExprConstantString`
    ConstantString(Box<AstExprConstantString>),
    /// `AstExprLocal`
    Local(Box<AstExprLocal>),
    /// `AstExprGlobal`
    Global(Box<AstExprGlobal>),
    /// `AstExprVarargs`
    Varargs(Box<AstExprVarargs>),
    /// `AstExprCall`
    Call(Box<AstExprCall>),
    /// `AstExprIndexName`
    IndexName(Box<AstExprIndexName>),
    /// `AstExprIndexExpr`
    IndexExpr(Box<AstExprIndexExpr>),
    /// `AstExprFunction`
    Function(Arc<AstExprFunction>),
    /// `AstExprTable`
    Table(Box<AstExprTable>),
    /// `AstExprUnary`
    Unary(Box<AstExprUnary>),
    /// `AstExprBinary`
    Binary(Box<AstExprBinary>),
    /// `AstExprTypeAssertion`
    TypeAssertion(Box<AstExprTypeAssertion>),
    /// `AstExprIfElse`
    IfElse(Box<AstExprIfElse>),
    /// `AstExprInterpString`
    InterpString(Box<AstExprInterpString>),
    /// `AstExprInstantiate`
    Instantiate(Box<AstExprInstantiate>),
    /// `AstExprError`
    Error(Box<AstExprError>),
}

impl AstExpr {
    /// The node's `location` (`AstNode::location`).
    pub fn location(&self) -> Location {
        match self {
            AstExpr::Group(n) => n.location,
            AstExpr::ConstantNil(n) => n.location,
            AstExpr::ConstantBool(n) => n.location,
            AstExpr::ConstantNumber(n) => n.location,
            AstExpr::ConstantInteger(n) => n.location,
            AstExpr::ConstantString(n) => n.location,
            AstExpr::Local(n) => n.location,
            AstExpr::Global(n) => n.location,
            AstExpr::Varargs(n) => n.location,
            AstExpr::Call(n) => n.location,
            AstExpr::IndexName(n) => n.location,
            AstExpr::IndexExpr(n) => n.location,
            AstExpr::Function(n) => n.location,
            AstExpr::Table(n) => n.location,
            AstExpr::Unary(n) => n.location,
            AstExpr::Binary(n) => n.location,
            AstExpr::TypeAssertion(n) => n.location,
            AstExpr::IfElse(n) => n.location,
            AstExpr::InterpString(n) => n.location,
            AstExpr::Instantiate(n) => n.location,
            AstExpr::Error(n) => n.location,
        }
    }

    /// Port of `getIdentifier`: the identifier this expression names when it is
    /// a global or local reference; `None` for the null `AstName`.
    pub fn as_name(&self) -> Option<&'static str> {
        match self {
            AstExpr::Global(expr) => {
                if expr.name.is_empty() {
                    None
                } else {
                    Some(expr.name.0)
                }
            }
            AstExpr::Local(expr) => {
                if expr.local.name.is_empty() {
                    None
                } else {
                    Some(expr.local.name.0)
                }
            }
            _ => None,
        }
    }

    /// Downcast to `AstExprLocal`.
    pub fn as_local(&self) -> Option<&AstExprLocal> {
        match self {
            AstExpr::Local(n) => Some(n),
            _ => None,
        }
    }

    /// Downcast to `AstExprGlobal`.
    pub fn as_global(&self) -> Option<&AstExprGlobal> {
        match self {
            AstExpr::Global(n) => Some(n),
            _ => None,
        }
    }

    /// Downcast to `AstExprCall`.
    pub fn as_call(&self) -> Option<&AstExprCall> {
        match self {
            AstExpr::Call(n) => Some(n),
            _ => None,
        }
    }

    /// Downcast to `AstExprTable`.
    pub fn as_table(&self) -> Option<&AstExprTable> {
        match self {
            AstExpr::Table(n) => Some(n),
            _ => None,
        }
    }

    /// Port of `isLValue`.
    pub fn is_lvalue(&self) -> bool {
        matches!(
            self,
            AstExpr::Local(_) | AstExpr::Global(_) | AstExpr::IndexName(_) | AstExpr::IndexExpr(_)
        )
    }

    /// Port of `isConstantLiteral`.
    pub fn is_constant_literal(&self) -> bool {
        matches!(
            self,
            AstExpr::ConstantNil(_)
                | AstExpr::ConstantBool(_)
                | AstExpr::ConstantNumber(_)
                | AstExpr::ConstantString(_)
        )
    }

    /// Port of `isLiteralTable`.
    pub fn is_literal_table(&self) -> bool {
        let AstExpr::Table(table) = self else {
            return false;
        };

        for item in &table.items {
            match item.kind {
                AstExprTableItemKind::General => return false,
                AstExprTableItemKind::Record | AstExprTableItemKind::List => {
                    if !item.value.is_constant_literal() && !item.value.is_literal_table() {
                        return false;
                    }
                }
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// AstStat hierarchy
// ---------------------------------------------------------------------------

/// C++ `AstStatBlock`.
///
/// `has_end` indicates whether the block was terminated in a syntactically
/// valid way — usually with `end`, but an `if`'s `then` body may be closed by
/// `else`/`elseif`, and a `repeat` body by `until` (see C++ comment).
#[derive(Debug, Clone)]
pub struct AstStatBlock {
    pub location: Location,
    /// C++ `AstStat` base-class `hasSemicolon`.
    pub has_semicolon: bool,
    pub body: Vec<Box<AstStat>>,
    pub has_end: bool,
}

/// C++ `AstStatIf`.
#[derive(Debug, Clone)]
pub struct AstStatIf {
    pub location: Location,
    pub has_semicolon: bool,
    pub condition: Box<AstExpr>,
    pub thenbody: Box<AstStatBlock>,
    /// The `else`/`elseif` body; absent for `if ... end`.
    pub elsebody: Option<Box<AstStat>>,
    pub then_location: Option<Location>,
    /// Active for `elseif` as well (see C++ comment).
    pub else_location: Option<Location>,
    /// Active for `if local` / `if const` statements.
    pub condition_local: Option<Arc<AstLocal>>,
    pub condition_is_const: bool,
    pub condition_keyword_location: Option<Location>,
    /// Location of the `=` in an `if local`/`if const` binding.
    pub condition_equals_location: Option<Location>,
}

/// C++ `AstStatWhile`.
#[derive(Debug, Clone)]
pub struct AstStatWhile {
    pub location: Location,
    pub has_semicolon: bool,
    pub condition: Box<AstExpr>,
    pub body: Box<AstStatBlock>,
    pub has_do: bool,
    pub do_location: Location,
    /// Active for `while local` / `while const`; `condition_local` is bound to
    /// `condition` and re-evaluated each iteration, in scope for `body` only.
    pub condition_local: Option<Arc<AstLocal>>,
    pub condition_is_const: bool,
    pub condition_keyword_location: Option<Location>,
    /// Location of the `=` in a `while local`/`while const` binding.
    pub condition_equals_location: Option<Location>,
}

/// C++ `AstStatRepeat`.
#[derive(Debug, Clone)]
pub struct AstStatRepeat {
    pub location: Location,
    pub has_semicolon: bool,
    pub condition: Box<AstExpr>,
    pub body: Box<AstStatBlock>,
    /// C++ field `DEPRECATED_hasUntil`.
    pub deprecated_has_until: bool,
}

/// C++ `AstStatBreak`.
#[derive(Debug, Clone)]
pub struct AstStatBreak {
    pub location: Location,
    pub has_semicolon: bool,
}

/// C++ `AstStatContinue`.
#[derive(Debug, Clone)]
pub struct AstStatContinue {
    pub location: Location,
    pub has_semicolon: bool,
}

/// C++ `AstStatReturn`.
#[derive(Debug, Clone)]
pub struct AstStatReturn {
    pub location: Location,
    pub has_semicolon: bool,
    pub list: Vec<Box<AstExpr>>,
}

/// C++ `AstStatExpr`: expression statement.
#[derive(Debug, Clone)]
pub struct AstStatExpr {
    pub location: Location,
    pub has_semicolon: bool,
    pub expr: Box<AstExpr>,
}

/// C++ `AstStatLocal`.
#[derive(Debug, Clone)]
pub struct AstStatLocal {
    pub location: Location,
    pub has_semicolon: bool,
    pub vars: Vec<Arc<AstLocal>>,
    pub values: Vec<Box<AstExpr>>,
    pub is_const: bool,
    pub is_exported: bool,
    /// If this statement exports a binding, the location of `const`/`local`.
    pub keyword_location: Option<Location>,
    pub equals_sign_location: Option<Location>,
}

/// C++ `AstStatFor` (`for i = a, b [, step] do ... end`).
#[derive(Debug, Clone)]
pub struct AstStatFor {
    pub location: Location,
    pub has_semicolon: bool,
    pub var: Arc<AstLocal>,
    pub from: Box<AstExpr>,
    pub to: Box<AstExpr>,
    /// Optional step expression (C++ `AstExpr* step`).
    pub step: Option<Box<AstExpr>>,
    pub body: Box<AstStatBlock>,
    pub has_do: bool,
    pub do_location: Location,
}

/// C++ `AstStatForIn`.
#[derive(Debug, Clone)]
pub struct AstStatForIn {
    pub location: Location,
    pub has_semicolon: bool,
    pub vars: Vec<Arc<AstLocal>>,
    pub values: Vec<Box<AstExpr>>,
    pub body: Box<AstStatBlock>,
    pub has_in: bool,
    pub in_location: Location,
    pub has_do: bool,
    pub do_location: Location,
}

/// C++ `AstStatAssign`.
#[derive(Debug, Clone)]
pub struct AstStatAssign {
    pub location: Location,
    pub has_semicolon: bool,
    pub vars: Vec<Box<AstExpr>>,
    pub values: Vec<Box<AstExpr>>,
}

/// C++ `AstStatCompoundAssign`.
#[derive(Debug, Clone)]
pub struct AstStatCompoundAssign {
    pub location: Location,
    pub has_semicolon: bool,
    /// C++ uses `AstExprBinary::Op` (`+=`, `-=` ...).
    pub op: AstExprBinaryOp,
    pub var: Box<AstExpr>,
    pub value: Box<AstExpr>,
}

/// C++ `AstStatFunction`.
#[derive(Debug, Clone)]
pub struct AstStatFunction {
    pub location: Location,
    pub has_semicolon: bool,
    pub name: Box<AstExpr>,
    pub func: Arc<AstExprFunction>,
}

/// C++ `AstStatLocalFunction`.
#[derive(Debug, Clone)]
pub struct AstStatLocalFunction {
    pub location: Location,
    pub has_semicolon: bool,
    pub name: Arc<AstLocal>,
    pub func: Arc<AstExprFunction>,
    pub is_const: bool,
    /// Position of the `const` keyword; `Position::missing()` when `is_const`
    /// is false.
    pub const_keyword_begin: Position,
}

/// C++ `AstStatTypeAlias`.
#[derive(Debug, Clone)]
pub struct AstStatTypeAlias {
    pub location: Location,
    pub has_semicolon: bool,
    pub name: AstName,
    pub name_location: Location,
    pub generics: Vec<AstGenericType>,
    pub generic_packs: Vec<AstGenericTypePack>,
    /// C++ field `type` (renamed: `type` is a Rust keyword).
    pub ty: Box<AstType>,
    pub exported: bool,
}

/// C++ `AstStatTypeFunction`.
#[derive(Debug, Clone)]
pub struct AstStatTypeFunction {
    pub location: Location,
    pub has_semicolon: bool,
    pub name: AstName,
    pub name_location: Location,
    pub body: Arc<AstExprFunction>,
    pub exported: bool,
    pub has_errors: bool,
}

/// C++ `AstStatDeclareGlobal`.
#[derive(Debug, Clone)]
pub struct AstStatDeclareGlobal {
    pub location: Location,
    pub has_semicolon: bool,
    pub name: AstName,
    pub name_location: Location,
    /// C++ field `type`.
    pub ty: Box<AstType>,
}

/// C++ `AstStatDeclareFunction`.
#[derive(Debug, Clone)]
pub struct AstStatDeclareFunction {
    pub location: Location,
    pub has_semicolon: bool,
    pub attributes: Vec<AstAttr>,
    pub name: AstName,
    pub name_location: Location,
    pub generics: Vec<AstGenericType>,
    pub generic_packs: Vec<AstGenericTypePack>,
    pub params: AstTypeList,
    pub param_names: Vec<AstArgumentName>,
    pub vararg: bool,
    pub vararg_location: Location,
    /// C++ field `retTypes`.
    pub ret_types: Box<AstTypePack>,
}

/// Table access modifier (C++ `AstTableAccess`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AstTableAccess {
    Read,
    Write,
    ReadWrite,
}

/// Class access modifier (C++ `AstClassAccess`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AstClassAccess {
    Public,
    Private,
}

/// C++ `AstClassProperty`.
#[derive(Debug, Clone)]
pub struct AstClassProperty {
    pub qualifier_location: Location,
    pub name: AstName,
    pub name_location: Location,
    pub type_colon_location: Option<Location>,
    /// C++ field `ty` (nullable).
    pub ty: Option<Box<AstType>>,
    pub access: AstClassAccess,
}

/// C++ `AstClassMethod`.
///
/// Methods with no access modifiers are public by default (C++ comment).
#[derive(Debug, Clone)]
pub struct AstClassMethod {
    pub qualifier_location: Option<Location>,
    pub keyword_location: Location,
    pub function_name: AstName,
    pub name_location: Location,
    pub function: Arc<AstExprFunction>,
    pub access: AstClassAccess,
}

/// C++ `AstClassMember` (`Variant<AstClassProperty, AstClassMethod>`).
#[derive(Debug, Clone)]
pub enum AstClassMember {
    Property(AstClassProperty),
    Method(AstClassMethod),
}

/// C++ `AstStatClass` (user-defined classes,
/// `FFlag::DebugLuauUserDefinedClasses`).
#[derive(Debug, Clone)]
pub struct AstStatClass {
    pub location: Location,
    pub has_semicolon: bool,
    pub name: Arc<AstLocal>,
    /// Superclass expression; nullable in C++ (`AstExpr* super`).
    pub super_: Option<Box<AstExpr>>,
    pub members: Vec<AstClassMember>,
    pub exported: bool,
    pub open: bool,
}

/// C++ `AstDeclaredExternTypeProperty`.
#[derive(Debug, Clone)]
pub struct AstDeclaredExternTypeProperty {
    pub name: AstName,
    pub name_location: Location,
    /// C++ field `ty` (defaults to nullptr in C++; the parser always fills it,
    /// so the Rust port keeps it as a required child).
    pub ty: Box<AstType>,
    pub is_method: bool,
    pub location: Location,
    pub access: AstTableAccess,
}

/// C++ `AstTableIndexer`.
#[derive(Debug, Clone)]
pub struct AstTableIndexer {
    pub index_type: Box<AstType>,
    pub result_type: Box<AstType>,
    pub location: Location,
    pub access: AstTableAccess,
    pub access_location: Option<Location>,
}

/// C++ `AstStatDeclareExternType`.
#[derive(Debug, Clone)]
pub struct AstStatDeclareExternType {
    pub location: Location,
    pub has_semicolon: bool,
    pub name: AstName,
    pub super_name: Option<AstName>,
    pub props: Vec<AstDeclaredExternTypeProperty>,
    pub indexer: Option<Box<AstTableIndexer>>,
}

/// C++ `AstStatError`: parse-error placeholder.
#[derive(Debug, Clone)]
pub struct AstStatError {
    pub location: Location,
    pub has_semicolon: bool,
    pub expressions: Vec<Box<AstExpr>>,
    pub statements: Vec<Box<AstStat>>,
    pub message_index: u32,
}

/// A statement node (`AstStat` in C++ plus one enum per subclass).
///
/// The C++ base-class `hasSemicolon` flag lives on every payload struct
/// (mirrored by [`AstStat::has_semicolon`] / [`AstStat::set_has_semicolon`]).
#[derive(Debug, Clone)]
pub enum AstStat {
    /// `AstStatBlock`
    Block(Box<AstStatBlock>),
    /// `AstStatIf`
    If(Box<AstStatIf>),
    /// `AstStatWhile`
    While(Box<AstStatWhile>),
    /// `AstStatRepeat`
    Repeat(Box<AstStatRepeat>),
    /// `AstStatBreak`
    Break(Box<AstStatBreak>),
    /// `AstStatContinue`
    Continue(Box<AstStatContinue>),
    /// `AstStatReturn`
    Return(Box<AstStatReturn>),
    /// `AstStatExpr`
    Expr(Box<AstStatExpr>),
    /// `AstStatLocal`
    Local(Box<AstStatLocal>),
    /// `AstStatFor`
    For(Box<AstStatFor>),
    /// `AstStatForIn`
    ForIn(Box<AstStatForIn>),
    /// `AstStatAssign`
    Assign(Box<AstStatAssign>),
    /// `AstStatCompoundAssign`
    CompoundAssign(Box<AstStatCompoundAssign>),
    /// `AstStatFunction`
    Function(Box<AstStatFunction>),
    /// `AstStatLocalFunction`
    LocalFunction(Box<AstStatLocalFunction>),
    /// `AstStatTypeAlias`
    TypeAlias(Box<AstStatTypeAlias>),
    /// `AstStatTypeFunction`
    TypeFunction(Box<AstStatTypeFunction>),
    /// `AstStatDeclareGlobal`
    DeclareGlobal(Box<AstStatDeclareGlobal>),
    /// `AstStatDeclareFunction`
    DeclareFunction(Box<AstStatDeclareFunction>),
    /// `AstStatClass`
    Class(Box<AstStatClass>),
    /// `AstStatDeclareExternType`
    DeclareExternType(Box<AstStatDeclareExternType>),
    /// `AstStatError`
    Error(Box<AstStatError>),
}

impl AstStat {
    /// The node's `location` (`AstNode::location`).
    pub fn location(&self) -> Location {
        match self {
            AstStat::Block(n) => n.location,
            AstStat::If(n) => n.location,
            AstStat::While(n) => n.location,
            AstStat::Repeat(n) => n.location,
            AstStat::Break(n) => n.location,
            AstStat::Continue(n) => n.location,
            AstStat::Return(n) => n.location,
            AstStat::Expr(n) => n.location,
            AstStat::Local(n) => n.location,
            AstStat::For(n) => n.location,
            AstStat::ForIn(n) => n.location,
            AstStat::Assign(n) => n.location,
            AstStat::CompoundAssign(n) => n.location,
            AstStat::Function(n) => n.location,
            AstStat::LocalFunction(n) => n.location,
            AstStat::TypeAlias(n) => n.location,
            AstStat::TypeFunction(n) => n.location,
            AstStat::DeclareGlobal(n) => n.location,
            AstStat::DeclareFunction(n) => n.location,
            AstStat::Class(n) => n.location,
            AstStat::DeclareExternType(n) => n.location,
            AstStat::Error(n) => n.location,
        }
    }

    /// The C++ base-class `hasSemicolon` flag.
    pub fn has_semicolon(&self) -> bool {
        match self {
            AstStat::Block(n) => n.has_semicolon,
            AstStat::If(n) => n.has_semicolon,
            AstStat::While(n) => n.has_semicolon,
            AstStat::Repeat(n) => n.has_semicolon,
            AstStat::Break(n) => n.has_semicolon,
            AstStat::Continue(n) => n.has_semicolon,
            AstStat::Return(n) => n.has_semicolon,
            AstStat::Expr(n) => n.has_semicolon,
            AstStat::Local(n) => n.has_semicolon,
            AstStat::For(n) => n.has_semicolon,
            AstStat::ForIn(n) => n.has_semicolon,
            AstStat::Assign(n) => n.has_semicolon,
            AstStat::CompoundAssign(n) => n.has_semicolon,
            AstStat::Function(n) => n.has_semicolon,
            AstStat::LocalFunction(n) => n.has_semicolon,
            AstStat::TypeAlias(n) => n.has_semicolon,
            AstStat::TypeFunction(n) => n.has_semicolon,
            AstStat::DeclareGlobal(n) => n.has_semicolon,
            AstStat::DeclareFunction(n) => n.has_semicolon,
            AstStat::Class(n) => n.has_semicolon,
            AstStat::DeclareExternType(n) => n.has_semicolon,
            AstStat::Error(n) => n.has_semicolon,
        }
    }

    /// Mutable counterpart of [`AstStat::has_semicolon`] (the C++ parser sets
    /// this flag after construction).
    pub fn set_has_semicolon(&mut self, value: bool) {
        match self {
            AstStat::Block(n) => n.has_semicolon = value,
            AstStat::If(n) => n.has_semicolon = value,
            AstStat::While(n) => n.has_semicolon = value,
            AstStat::Repeat(n) => n.has_semicolon = value,
            AstStat::Break(n) => n.has_semicolon = value,
            AstStat::Continue(n) => n.has_semicolon = value,
            AstStat::Return(n) => n.has_semicolon = value,
            AstStat::Expr(n) => n.has_semicolon = value,
            AstStat::Local(n) => n.has_semicolon = value,
            AstStat::For(n) => n.has_semicolon = value,
            AstStat::ForIn(n) => n.has_semicolon = value,
            AstStat::Assign(n) => n.has_semicolon = value,
            AstStat::CompoundAssign(n) => n.has_semicolon = value,
            AstStat::Function(n) => n.has_semicolon = value,
            AstStat::LocalFunction(n) => n.has_semicolon = value,
            AstStat::TypeAlias(n) => n.has_semicolon = value,
            AstStat::TypeFunction(n) => n.has_semicolon = value,
            AstStat::DeclareGlobal(n) => n.has_semicolon = value,
            AstStat::DeclareFunction(n) => n.has_semicolon = value,
            AstStat::Class(n) => n.has_semicolon = value,
            AstStat::DeclareExternType(n) => n.has_semicolon = value,
            AstStat::Error(n) => n.has_semicolon = value,
        }
    }

    /// Downcast to `AstStatBlock`.
    pub fn as_block(&self) -> Option<&AstStatBlock> {
        match self {
            AstStat::Block(n) => Some(n),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// AstType hierarchy
// ---------------------------------------------------------------------------

/// C++ `AstTypeReference`.
#[derive(Debug, Clone)]
pub struct AstTypeReference {
    pub location: Location,
    pub has_parameter_list: bool,
    /// Optional prefix, e.g. the `Module` in `Module.Type`.
    pub prefix: Option<AstName>,
    pub prefix_location: Option<Location>,
    /// The local the prefix resolved to (`if local`/`if const` type bindings;
    /// C++ field `prefixLocal`).
    pub prefix_local: Option<Arc<AstLocal>>,
    pub name: AstName,
    pub name_location: Location,
    pub parameters: Vec<AstTypeOrPack>,
}

/// Table property (C++ `AstTableProp`).
#[derive(Debug, Clone)]
pub struct AstTableProp {
    pub name: AstName,
    pub location: Location,
    /// C++ field `type` (renamed: `type` is a Rust keyword).
    pub ty: Box<AstType>,
    pub access: AstTableAccess,
    pub access_location: Option<Location>,
}

/// C++ `AstTypeTable`.
#[derive(Debug, Clone)]
pub struct AstTypeTable {
    pub location: Location,
    pub props: Vec<AstTableProp>,
    pub indexer: Option<Box<AstTableIndexer>>,
    pub is_exact: bool,
}

/// C++ `AstTypeFunction`.
#[derive(Debug, Clone)]
pub struct AstTypeFunction {
    pub location: Location,
    pub attributes: Vec<AstAttr>,
    pub generics: Vec<AstGenericType>,
    pub generic_packs: Vec<AstGenericTypePack>,
    pub arg_types: AstTypeList,
    /// Named arguments (C++ `AstArray<std::optional<AstArgumentName>>`):
    /// either empty, or one entry (possibly unnamed) per entry of
    /// `arg_types.types`.
    pub arg_names: Vec<Option<AstArgumentName>>,
    /// C++ field `returnTypes`.
    pub return_types: Box<AstTypePack>,
}

/// C++ `AstTypeTypeof`.
#[derive(Debug, Clone)]
pub struct AstTypeTypeof {
    pub location: Location,
    pub expr: Box<AstExpr>,
}

/// C++ `AstTypeOptional`: the `?` of `T?`, a `nil` member of the
/// enclosing `AstTypeUnion`.
#[derive(Debug, Clone)]
pub struct AstTypeOptional {
    pub location: Location,
}

/// C++ `AstTypeUnion`.
#[derive(Debug, Clone)]
pub struct AstTypeUnion {
    pub location: Location,
    pub types: Vec<Box<AstType>>,
}

/// C++ `AstTypeIntersection`.
#[derive(Debug, Clone)]
pub struct AstTypeIntersection {
    pub location: Location,
    pub types: Vec<Box<AstType>>,
}

/// C++ `AstTypeSingletonBool`.
#[derive(Debug, Clone)]
pub struct AstTypeSingletonBool {
    pub location: Location,
    pub value: bool,
}

/// C++ `AstTypeSingletonString`.
#[derive(Debug, Clone)]
pub struct AstTypeSingletonString {
    pub location: Location,
    /// Decoded literal bytes (C++ `AstArray<char> value`).
    pub value: Box<[u8]>,
}

/// C++ `AstTypeGroup`: `( type )`.
#[derive(Debug, Clone)]
pub struct AstTypeGroup {
    pub location: Location,
    /// C++ field `type`.
    pub ty: Box<AstType>,
}

/// C++ `AstTypeError`.
#[derive(Debug, Clone)]
pub struct AstTypeError {
    pub location: Location,
    pub types: Vec<Box<AstType>>,
    pub is_missing: bool,
    pub message_index: u32,
}

/// A type-annotation node (`AstType` in C++ plus one enum per subclass).
#[derive(Debug, Clone)]
pub enum AstType {
    /// `AstTypeReference`
    Reference(Box<AstTypeReference>),
    /// `AstTypeTable`
    Table(Box<AstTypeTable>),
    /// `AstTypeFunction`
    Function(Box<AstTypeFunction>),
    /// `AstTypeTypeof`
    Typeof(Box<AstTypeTypeof>),
    /// `AstTypeOptional`
    Optional(Box<AstTypeOptional>),
    /// `AstTypeUnion`
    Union(Box<AstTypeUnion>),
    /// `AstTypeIntersection`
    Intersection(Box<AstTypeIntersection>),
    /// `AstTypeSingletonBool`
    SingletonBool(Box<AstTypeSingletonBool>),
    /// `AstTypeSingletonString`
    SingletonString(Box<AstTypeSingletonString>),
    /// `AstTypeGroup`
    Group(Box<AstTypeGroup>),
    /// `AstTypeError`
    Error(Box<AstTypeError>),
}

impl AstType {
    /// The node's `location` (`AstNode::location`).
    pub fn location(&self) -> Location {
        match self {
            AstType::Reference(n) => n.location,
            AstType::Table(n) => n.location,
            AstType::Function(n) => n.location,
            AstType::Typeof(n) => n.location,
            AstType::Optional(n) => n.location,
            AstType::Union(n) => n.location,
            AstType::Intersection(n) => n.location,
            AstType::SingletonBool(n) => n.location,
            AstType::SingletonString(n) => n.location,
            AstType::Group(n) => n.location,
            AstType::Error(n) => n.location,
        }
    }
}

// ---------------------------------------------------------------------------
// AstTypePack hierarchy
// ---------------------------------------------------------------------------

/// C++ `AstTypePackExplicit`.
#[derive(Debug, Clone)]
pub struct AstTypePackExplicit {
    pub location: Location,
    pub type_list: AstTypeList,
}

/// C++ `AstTypePackVariadic`.
#[derive(Debug, Clone)]
pub struct AstTypePackVariadic {
    pub location: Location,
    pub variadic_type: Box<AstType>,
}

/// C++ `AstTypePackGeneric` (`...T` where `T` is a generic type-pack name).
#[derive(Debug, Clone)]
pub struct AstTypePackGeneric {
    pub location: Location,
    /// C++ field `genericName`.
    pub generic_name: AstName,
}

/// A type-pack annotation node (`AstTypePack` in C++ plus one enum per subclass).
#[derive(Debug, Clone)]
pub enum AstTypePack {
    /// `AstTypePackExplicit`
    Explicit(Box<AstTypePackExplicit>),
    /// `AstTypePackVariadic`
    Variadic(Box<AstTypePackVariadic>),
    /// `AstTypePackGeneric`
    Generic(Box<AstTypePackGeneric>),
}

impl AstTypePack {
    /// The node's `location` (`AstNode::location`).
    pub fn location(&self) -> Location {
        match self {
            AstTypePack::Explicit(n) => n.location,
            AstTypePack::Variadic(n) => n.location,
            AstTypePack::Generic(n) => n.location,
        }
    }
}

// ---------------------------------------------------------------------------
// Free helper functions (end of Ast.h)
// ---------------------------------------------------------------------------

/// Types exposing a `location() -> Location` — used by [`node_list_location`],
/// mirroring C++ `getLocation(AstArray<T*>)` for
/// `T ∈ {AstExpr, AstStat, AstLocal, ...}`.
pub trait AsLocation {
    fn location(&self) -> Location;
}

impl AsLocation for AstExpr {
    fn location(&self) -> Location {
        self.location()
    }
}

impl AsLocation for Box<AstExpr> {
    fn location(&self) -> Location {
        (**self).location()
    }
}

impl AsLocation for AstStat {
    fn location(&self) -> Location {
        self.location()
    }
}

impl AsLocation for Box<AstStat> {
    fn location(&self) -> Location {
        (**self).location()
    }
}

impl AsLocation for Arc<AstLocal> {
    fn location(&self) -> Location {
        (**self).location()
    }
}

/// Port of the C++ template `getLocation(AstArray<T*>)`: the span from the
/// first element's begin to the last element's end; an empty (default)
/// [`Location`] when the list is empty.
pub fn node_list_location<T: AsLocation>(list: &[T]) -> Location {
    match (list.first(), list.last()) {
        (Some(first), Some(last)) => {
            Location::new(first.location().begin, last.location().end)
        }
        _ => Location::default(),
    }
}

/// Port of `getLocation(const AstTypeList&)`.
///
/// The C++ version computes `Location{types.front()->location,
/// types.back()->location}` — begin of the first location and end of the last
/// one — and then extends the end to the tail pack.
pub fn type_list_location(list: &AstTypeList) -> Location {
    let mut result = Location::default();
    if let (Some(first), Some(last)) = (list.types.first(), list.types.last()) {
        result = Location::new(first.location().begin, last.location().end);
    }
    if let Some(tail) = &list.tail_type {
        result.end = tail.location().end;
    }
    result
}

// ---------------------------------------------------------------------------
// AstVisitor
// ---------------------------------------------------------------------------

/// Port of `Luau::AstVisitor` (`Ast.h`).
///
/// The C++ visitor has ~60 overloads of `bool visit(T*)`, with the boolean
/// controlling descent into children. The Rust port splits that into:
///
/// - `visit_*` — called for each node in pre-order, before its children;
/// - `walk_*` — `visit` + recurse into children (C++ recursion happens when
///   the `visit` overload returned `true`; override a `walk_*` method instead
///   if you need to veto recursion, mirroring a `false` return in C++).
///
/// Unlike the C++ defaults (where `visit(AstType*)` returns `false`,
/// disabling annotation traversal), the walker here transports type
/// annotations; a visitor not interested in them simply leaves
/// `visit_type`/`visit_type_pack` as no-ops (the default) and ignores the
/// callbacks.
///
/// Child traversal order exactly matches the `T::visit` implementations in
/// `Ast/src/Ast.cpp` (e.g. `AstStatRepeat` walks `body` before `condition`;
/// `AstExprFunction` walks arg annotations, then vararg annotation, then
/// return annotation, then `body`).
///
/// Two payloads never appear wrapped as enum values at a borrowable call site
/// (`AstStatBlock` bodies nested inside other statements/expression functions,
/// and `AstExprFunction` bodies of `AstStatFunction`/`AstStatLocalFunction`/
/// `AstStatTypeFunction`/`AstClassMethod`), so the C++
/// `visit(AstStatBlock*)`/`visit(AstExprFunction*)` overloads are also exposed
/// as payload-level hooks `visit_block`/`visit_function`. For everything
/// reachable as `AstExpr`/`AstStat`, `visit_expr`/`visit_stat` are called.
pub trait AstVisitor {
    /// C++ `visit(AstAttr*)` hook.
    fn visit_attr(&mut self, _attr: &AstAttr) {}

    /// C++ `visit(AstExpr*)` (called for every expression node, pre-order).
    fn visit_expr(&mut self, _expr: &AstExpr) {}

    /// C++ `visit(AstStat*)` (called for every statement node, pre-order).
    fn visit_stat(&mut self, _stat: &AstStat) {}

    /// C++ `visit(AstType*)`.
    fn visit_type(&mut self, _ty: &AstType) {}

    /// C++ `visit(AstTypePack*)`.
    fn visit_type_pack(&mut self, _pack: &AstTypePack) {}

    /// C++ `visit(AstGenericType*)`.
    fn visit_generic(&mut self, _generic: &AstGenericType) {}

    /// C++ `visit(AstGenericTypePack*)`.
    fn visit_generic_pack(&mut self, _generic_pack: &AstGenericTypePack) {}

    /// C++ `visit(AstStatBlock*)` (see trait docs).
    fn visit_block(&mut self, _block: &AstStatBlock) {}

    /// C++ `visit(AstExprFunction*)` (see trait docs).
    fn visit_function(&mut self, _function: &AstExprFunction) {}

    /// C++ `AstGenericType::visit` equivalent.
    fn walk_generic(&mut self, generic: &AstGenericType) {
        self.visit_generic(generic);
        if let Some(default_value) = &generic.default_value {
            self.walk_type(default_value);
        }
    }

    /// C++ `AstGenericTypePack::visit` equivalent.
    fn walk_generic_pack(&mut self, generic_pack: &AstGenericTypePack) {
        self.visit_generic_pack(generic_pack);
        if let Some(default_value) = &generic_pack.default_value {
            self.walk_type_pack(default_value);
        }
    }

    /// C++ `AstAttr::visit` + args traversal.
    fn walk_attr(&mut self, attr: &AstAttr) {
        self.visit_attr(attr);
        for arg in &attr.args {
            self.walk_expr(arg);
        }
    }

    /// C++ `AstStatBlock::visit` equivalent for borrowed blocks.
    fn walk_block(&mut self, block: &AstStatBlock) {
        self.visit_block(block);
        for child in &block.body {
            self.walk_stat(child);
        }
    }

    /// C++ `AstExprFunction::visit` equivalent for borrowed function bodies:
    /// arg annotations, then vararg annotation, then return annotation, then
    /// the body block (order taken from `Ast/src/Ast.cpp`).
    fn walk_function(&mut self, function: &AstExprFunction) {
        self.visit_function(function);
        for arg in &function.args {
            if let Some(annotation) = &arg.annotation {
                self.walk_type(annotation);
            }
        }
        if let Some(annotation) = &function.vararg_annotation {
            self.walk_type_pack(annotation);
        }
        if let Some(annotation) = &function.return_annotation {
            self.walk_type_pack(annotation);
        }
        self.walk_block(&function.body);
    }

    /// Visit an expression and all of its children, depth-first
    /// (equivalent to `node->visit(visitor)` for `AstExpr` subclasses).
    fn walk_expr(&mut self, expr: &AstExpr) {
        self.visit_expr(expr);
        match expr {
            AstExpr::Group(node) => self.walk_expr(&node.expr),
            AstExpr::ConstantNil(_)
            | AstExpr::ConstantBool(_)
            | AstExpr::ConstantNumber(_)
            | AstExpr::ConstantInteger(_)
            | AstExpr::ConstantString(_)
            | AstExpr::Local(_)
            | AstExpr::Global(_)
            | AstExpr::Varargs(_) => {}
            AstExpr::Call(node) => {
                self.walk_expr(&node.func);
                for arg in &node.args {
                    self.walk_expr(arg);
                }
            }
            AstExpr::IndexName(node) => self.walk_expr(&node.expr),
            AstExpr::IndexExpr(node) => {
                self.walk_expr(&node.expr);
                self.walk_expr(&node.index);
            }
            AstExpr::Function(node) => self.walk_function(node),
            AstExpr::Table(node) => {
                for item in &node.items {
                    if let Some(key) = &item.key {
                        self.walk_expr(key);
                    }
                    self.walk_expr(&item.value);
                }
            }
            AstExpr::Unary(node) => self.walk_expr(&node.expr),
            AstExpr::Binary(node) => {
                self.walk_expr(&node.left);
                self.walk_expr(&node.right);
            }
            AstExpr::TypeAssertion(node) => {
                self.walk_expr(&node.expr);
                self.walk_type(&node.annotation);
            }
            AstExpr::IfElse(node) => {
                // C++ gates this on FFlag::LuauExperimentalIfLocalSyntax; the
                // Rust walker reproduces it unguarded (it is a no-op when
                // condition_local is None).
                if let Some(local) = &node.condition_local {
                    if let Some(annotation) = &local.annotation {
                        self.walk_type(annotation);
                    }
                }
                self.walk_expr(&node.condition);
                self.walk_expr(&node.true_expr);
                self.walk_expr(&node.false_expr);
            }
            AstExpr::InterpString(node) => {
                for expression in &node.expressions {
                    self.walk_expr(expression);
                }
            }
            AstExpr::Instantiate(node) => {
                self.walk_expr(&node.expr);
                self.walk_type_or_pack_slice(&node.type_arguments);
            }
            AstExpr::Error(node) => {
                for expression in &node.expressions {
                    self.walk_expr(expression);
                }
            }
        }
    }

    /// Visit a statement and all of its children (C++ statement visit
    /// dispatch).
    fn walk_stat(&mut self, stat: &AstStat) {
        self.visit_stat(stat);
        match stat {
            AstStat::Block(node) => {
                for child in &node.body {
                    self.walk_stat(child);
                }
            }
            AstStat::If(node) => {
                // C++ gates on FFlag::LuauExperimentalIfLocalSyntax; see
                // walk_expr(AstExpr::IfElse).
                if let Some(local) = &node.condition_local {
                    if let Some(annotation) = &local.annotation {
                        self.walk_type(annotation);
                    }
                }
                self.walk_expr(&node.condition);
                self.walk_block(&node.thenbody);
                if let Some(elsebody) = &node.elsebody {
                    self.walk_stat(elsebody);
                }
            }
            AstStat::While(node) => {
                if let Some(local) = &node.condition_local {
                    if let Some(annotation) = &local.annotation {
                        self.walk_type(annotation);
                    }
                }
                self.walk_expr(&node.condition);
                self.walk_block(&node.body);
            }
            AstStat::Repeat(node) => {
                // C++ walks the body before the condition here.
                self.walk_block(&node.body);
                self.walk_expr(&node.condition);
            }
            AstStat::Break(_) | AstStat::Continue(_) => {}
            AstStat::Return(node) => {
                for expr in &node.list {
                    self.walk_expr(expr);
                }
            }
            AstStat::Expr(node) => self.walk_expr(&node.expr),
            AstStat::Local(node) => {
                for var in &node.vars {
                    if let Some(annotation) = &var.annotation {
                        self.walk_type(annotation);
                    }
                }
                for value in &node.values {
                    self.walk_expr(value);
                }
            }
            AstStat::For(node) => {
                if let Some(annotation) = &node.var.annotation {
                    self.walk_type(annotation);
                }
                self.walk_expr(&node.from);
                self.walk_expr(&node.to);
                if let Some(step) = &node.step {
                    self.walk_expr(step);
                }
                self.walk_block(&node.body);
            }
            AstStat::ForIn(node) => {
                for var in &node.vars {
                    if let Some(annotation) = &var.annotation {
                        self.walk_type(annotation);
                    }
                }
                for value in &node.values {
                    self.walk_expr(value);
                }
                self.walk_block(&node.body);
            }
            AstStat::Assign(node) => {
                for var in &node.vars {
                    self.walk_expr(var);
                }
                for value in &node.values {
                    self.walk_expr(value);
                }
            }
            AstStat::CompoundAssign(node) => {
                self.walk_expr(&node.var);
                self.walk_expr(&node.value);
            }
            AstStat::Function(node) => {
                self.walk_expr(&node.name);
                self.walk_function(&node.func);
            }
            AstStat::LocalFunction(node) => {
                self.walk_function(&node.func);
            }
            AstStat::TypeAlias(node) => {
                for generic in &node.generics {
                    self.walk_generic(generic);
                }
                for generic_pack in &node.generic_packs {
                    self.walk_generic_pack(generic_pack);
                }
                self.walk_type(&node.ty);
            }
            AstStat::TypeFunction(node) => {
                self.walk_function(&node.body);
            }
            AstStat::DeclareGlobal(node) => {
                self.walk_type(&node.ty);
            }
            AstStat::DeclareFunction(node) => {
                self.walk_type_list(&node.params);
                self.walk_type_pack(&node.ret_types);
            }
            AstStat::Class(node) => {
                if let Some(super_) = &node.super_ {
                    self.walk_expr(super_);
                }
                for member in &node.members {
                    match member {
                        AstClassMember::Property(prop) => {
                            if let Some(ty) = &prop.ty {
                                self.walk_type(ty);
                            }
                        }
                        AstClassMember::Method(method) => {
                            self.walk_function(&method.function);
                        }
                    }
                }
            }
            AstStat::DeclareExternType(node) => {
                for prop in &node.props {
                    self.walk_type(&prop.ty);
                }
            }
            AstStat::Error(node) => {
                for expression in &node.expressions {
                    self.walk_expr(expression);
                }
                for statement in &node.statements {
                    self.walk_stat(statement);
                }
            }
        }
    }

    /// Visit a type annotation and its children.
    fn walk_type(&mut self, ty: &AstType) {
        self.visit_type(ty);
        match ty {
            AstType::Reference(node) => {
                self.walk_type_or_pack_slice(&node.parameters);
            }
            AstType::Table(node) => {
                for prop in &node.props {
                    self.walk_type(&prop.ty);
                }
                if let Some(indexer) = &node.indexer {
                    self.walk_type(&indexer.index_type);
                    self.walk_type(&indexer.result_type);
                }
            }
            AstType::Function(node) => {
                self.walk_type_list(&node.arg_types);
                self.walk_type_pack(&node.return_types);
            }
            AstType::Typeof(node) => self.walk_expr(&node.expr),
            AstType::Optional(_) => {}
            AstType::Union(node) => {
                for child in &node.types {
                    self.walk_type(child);
                }
            }
            AstType::Intersection(node) => {
                for child in &node.types {
                    self.walk_type(child);
                }
            }
            AstType::SingletonBool(_) | AstType::SingletonString(_) => {}
            AstType::Group(node) => self.walk_type(&node.ty),
            AstType::Error(node) => {
                for child in &node.types {
                    self.walk_type(child);
                }
            }
        }
    }

    /// Visit a type pack and its children.
    fn walk_type_pack(&mut self, pack: &AstTypePack) {
        self.visit_type_pack(pack);
        match pack {
            AstTypePack::Explicit(node) => {
                self.walk_type_list(&node.type_list);
            }
            AstTypePack::Variadic(node) => {
                self.walk_type(&node.variadic_type);
            }
            AstTypePack::Generic(_) => {}
        }
    }

    /// C++ static helper `visitTypeList(AstVisitor*, const AstTypeList&)`.
    fn walk_type_list(&mut self, list: &AstTypeList) {
        for ty in &list.types {
            self.walk_type(ty);
        }
        if let Some(tail) = &list.tail_type {
            self.walk_type_pack(tail);
        }
    }

    /// C++ static helper `visitTypeOrPackArray(AstVisitor*, const AstArray<AstTypeOrPack>&)`.
    fn walk_type_or_pack_slice(&mut self, params: &[AstTypeOrPack]) {
        for param in params {
            if let Some(ty) = &param.ty {
                self.walk_type(ty);
            } else if let Some(type_pack) = &param.type_pack {
                self.walk_type_pack(type_pack);
            }
        }
    }
}
