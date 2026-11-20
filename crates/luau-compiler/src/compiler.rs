//! Port of `Compiler/src/Compiler.cpp` — AST to bytecode.
//!
//! Follows the C++ compiler function by function with the reference CLI's
//! flag configuration (all `Luau*` flags on, `Debug*` flags off), so that the
//! output can be compared byte-for-byte with `luau-compile --binary`.
//!
//! Not ported yet (all `-O2`-only or type-info features): call inlining,
//! loop unrolling, builtin constant folding (`builtinsFold`), type info
//! emission. User-defined classes are behind `DebugLuauUserDefinedClasses`
//! (off in the CLI) and are rejected like the reference.

use luau_common::fasthash::{HashMap, HashSet};
use std::sync::Arc;

use luau_ast::ast::*;
use luau_ast::location::Location;
use luau_ast::parser::{HotComment, ParseResult};

use crate::analysis::{
    always_terminates, analyze_builtins, build_table_constant_map, get_builtin, global_state,
    compute_cost, get_builtin_info, get_trip_count, lbf, local_key, model_cost, predict_table_shapes, undo_changes,
    ChangeLog, Constant, ConstantFolder, Global, Init, LocalKey, Shape, ValueVisitor, Variable,
};
use crate::builder::{BytecodeBuilder, TableShape, INVALID_REG, LPF_INLINABLE, MAX_INSTRUCTION_COUNT, MAX_LOCAL_COUNT, MAX_REGISTER_COUNT, MAX_UPVALUE_COUNT};
use crate::visit::{key, unwrap_expr, walk_block, walk_expr, walk_function, Visitor};
use luau_common::bytecode::{insn, proto_flag, Opcode};

const DEFAULT_ALLOC_PC: u32 = u32::MAX;
const TABLE_T: u8 = crate::types::lbc_type::TABLE;
const NUMBER_T: u8 = crate::types::lbc_type::NUMBER;

/// `Luau::CompileOptions` (the subset the port honours).
#[derive(Debug, Clone)]
pub struct CompileOptions {
    pub optimization_level: u8,
    pub debug_level: u8,
    pub coverage_level: u8,
    /// `typeInfoLevel`: 1 records function argument types (native entry
    /// guards).
    pub type_info_level: u8,
    /// `vectorPrecision == 1` (double precision vector constants).
    pub vector_double: bool,
    /// `DebugLuauUserDefinedClasses`: compile `class` declarations
    /// (bytecode version `LBC_VERSION_CLASSES`).
    pub classes: bool,
    /// `vectorLib` / `vectorCtor` / `vectorType`: host-provided vector
    /// constructor (`lib.ctor(...)` or global `ctor(...)`) and type name.
    pub vector_lib: Option<String>,
    pub vector_ctor: Option<String>,
    pub vector_type: Option<String>,
}

impl Default for CompileOptions {
    fn default() -> Self {
        CompileOptions {
            optimization_level: 1,
            debug_level: 1,
            coverage_level: 0,
            type_info_level: 0,
            vector_double: false,
            classes: false,
            vector_lib: None,
            vector_ctor: None,
            vector_type: None,
        }
    }
}

/// `Luau::CompileError`.
#[derive(Debug, Clone)]
pub struct CompileError {
    pub location: Location,
    pub message: String,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, ":{}: {}", self.location.begin.line + 1, self.message)
    }
}

impl std::error::Error for CompileError {}

impl CompileError {
    /// `CompileError::raise`: unwinds to [`compile`], which turns it into
    /// an `Err`.
    pub fn raise(location: Location, message: impl Into<String>) -> ! {
        install_quiet_hook();
        std::panic::panic_any(CompileError { location, message: message.into() })
    }
}

/// Keeps the default panic hook from printing compile errors (they are
/// reported as values by [`compile`]).
fn install_quiet_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if info.payload().downcast_ref::<CompileError>().is_none() {
                prev(info);
            }
        }));
    });
}

// ---------------------------------------------------------------------------
// Compiler state
// ---------------------------------------------------------------------------

/// The function being compiled: an `AstExprFunction`, or the synthetic main
/// function wrapping the chunk body.
#[derive(Clone, Copy)]
struct FuncRef<'a> {
    key: usize,
    self_local: Option<&'a Arc<AstLocal>>,
    args: &'a [Arc<AstLocal>],
    vararg: bool,
    body: &'a AstStatBlock,
    depth: usize,
    debugname: &'static str,
    location: Location,
    native: bool,
}

impl<'a> FuncRef<'a> {
    fn of(f: &'a AstExprFunction) -> Self {
        FuncRef {
            key: key(f),
            self_local: f.self_local.as_ref(),
            args: &f.args,
            vararg: f.vararg,
            body: &f.body,
            depth: f.function_depth + 1, // C++ counts the function itself
            debugname: f.debugname.0,
            location: f.location,
            native: f.attributes.iter().any(|a| a.kind == AstAttrKind::Native),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum ReturnKind {
    #[default]
    None,
    Expression,
    Parameter,
    Local,
    Unknown,
}

#[derive(Clone, Default)]
struct FunctionInfo {
    id: u32,
    upvals: Vec<Arc<AstLocal>>,
    // inlining information (-O2)
    can_inline: bool,
    stack_size: u32,
    cost_model: u64,
    returns_one: bool,
    return_kind: ReturnKind,
    return_local: Option<Arc<AstLocal>>,
    return_local_captured: bool,
}

struct InlineArg<'a> {
    local: &'a Arc<AstLocal>,
    reg: u8,
    value: Constant,
    allocpc: u32,
    init: Option<Init<'a>>,
}

struct InlineFrame {
    func: usize,
    local_offset: usize,
    target: u8,
    target_count: u8,
    result_local: Option<Arc<AstLocal>>,
    return_jumps: Vec<usize>,
}

const INLINE_THRESHOLD: i32 = 25;
const INLINE_THRESHOLD_MAX_BOOST: i32 = 300;
const INLINE_DEPTH: usize = 5;
const LOOP_UNROLL_THRESHOLD: i32 = 25;
const LOOP_UNROLL_THRESHOLD_MAX_BOOST: i32 = 300;

#[derive(Clone, Copy, Default)]
struct Local {
    reg: u8,
    allocated: bool,
    captured: bool,
    reused: bool,
    debugpc: u32,
    allocpc: u32,
}

#[derive(Clone)]
struct FreeLocal {
    reg: u8,
    local: Local,
    ast_local: Arc<AstLocal>,
    is_argument: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoopJumpType {
    Break,
    Continue,
}

#[derive(Clone, Copy)]
struct LoopJump {
    ty: LoopJumpType,
    label: usize,
}

#[derive(Clone, Copy)]
struct Loop {
    local_offset: usize,
    local_offset_continue: usize,
    continue_used: Option<Location>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LValueKind {
    Local,
    Upvalue,
    Global,
    IndexName,
    IndexNumber,
    IndexExpr,
    PrivateField,
}

#[derive(Clone)]
struct LValue {
    kind: LValueKind,
    reg: u8,
    upval: u8,
    index: u8,
    number: u8,
    name: Vec<u8>,
    /// Private field name (`Kind_PrivateField`).
    ast_name: &'static str,
    location: Location,
}

impl LValue {
    fn new(kind: LValueKind, location: Location) -> Self {
        LValue { kind, reg: 0, upval: 0, index: 0, number: 0, name: Vec::new(), ast_name: "", location }
    }
}

struct Exports {
    table_local: Arc<AstLocal>,
    exported_functions: HashSet<LocalKey>,
    exported_variables: Vec<Arc<AstLocal>>,
    exported_classes: Vec<(Arc<AstLocal>, u8)>,
    exported_table_cid: i32,
    has_exports: bool,
    has_type_exports: bool,
    has_module_return: bool,
}

impl Exports {
    fn is_empty(&self) -> bool {
        !self.has_exports && self.exported_classes.is_empty() && self.exported_functions.is_empty() && self.exported_variables.is_empty()
    }
}

struct Compiler<'a> {
    bytecode: BytecodeBuilder,
    options: CompileOptions,

    functions: HashMap<usize, FunctionInfo>,
    locals: HashMap<LocalKey, Local>,
    globals: HashMap<&'static str, Global>,
    variables: HashMap<LocalKey, Variable<'a>>,
    constants: HashMap<usize, Constant>,
    locstants: HashMap<LocalKey, Constant>,
    /// `exprChanges` / `localChanges`: shared by inlining and loop unrolling
    /// (a nested user clears the outer one's log, as in C++).
    expr_changes: ChangeLog,
    local_changes: ChangeLog,
    table_constants: HashSet<LocalKey>,
    table_shapes: HashMap<usize, Shape>,
    builtins: HashMap<usize, i32>,
    function_types: HashMap<usize, Vec<u8>>,

    reg_top: u32,
    stack_size: u32,
    arg_count: usize,
    has_loops: bool,
    has_multi_ret: bool,
    has_require: bool,
    current_function: Option<FuncRef<'a>>,
    block_depth: usize,
    getfenv_used: bool,
    setfenv_used: bool,

    local_stack: Vec<Arc<AstLocal>>,
    reg_captured: [bool; 256],
    active_locals: usize,
    upvals: Vec<Arc<AstLocal>>,
    loop_jumps: Vec<LoopJump>,
    loops: Vec<Loop>,
    local_uses: [i32; 256],
    free_local_regs: Vec<FreeLocal>,

    exports: Exports,

    /// Class enclosing each function (`enclosingClasses`, keyed by function).
    enclosing_classes: HashMap<usize, Option<&'a AstStatClass>>,
    /// Private member slots per class: name -> (offset, is static).
    class_private_field_offsets: HashMap<usize, HashMap<&'static str, (u8, bool)>>,
    curr_class: Option<&'a AstStatClass>,

    /// `builtinsFold` (-O2): fold builtin calls with constant arguments
    builtins_fold: bool,
    builtins_fold_library_k: bool,
    inline_frames: Vec<InlineFrame>,

    /// `localTypes` / `exprTypes` (`buildTypeMap`)
    local_types: HashMap<usize, u8>,
    expr_types: HashMap<usize, crate::types::ExprType<'a>>,
}

macro_rules! scoped {
    ($self:ident, $body:block) => {{
        let __old = $self.reg_top;
        // a closure, so that `return` inside the body still restores `reg_top`
        #[allow(clippy::redundant_closure_call)]
        let __r = (|| $body)();
        $self.reg_top = __old;
        __r
    }};
}

fn const_limit(loc: Location) -> ! {
    CompileError::raise(loc, "Exceeded constant limit; simplify the code to compile")
}

fn jump_limit(loc: Location) -> ! {
    CompileError::raise(loc, "Exceeded jump distance limit; simplify the code to compile")
}

fn is_stat_break(node: &AstStatBlock) -> bool {
    node.body.len() == 1 && matches!(&*node.body[0], AstStat::Break(_))
}

fn extract_stat_continue(block: &AstStatBlock) -> Option<&AstStatContinue> {
    if block.body.len() == 1 {
        if let AstStat::Continue(c) = &*block.body[0] {
            return Some(c);
        }
    }
    None
}

fn encode_hash_size(hash_size: u32) -> u8 {
    let mut log2 = 0;
    while (1u32 << log2) < hash_size {
        log2 += 1;
    }
    if hash_size == 0 { 0 } else { (log2 + 1) as u8 }
}

fn escape_and_append(buffer: &mut Vec<u8>, s: &[u8]) {
    for &c in s {
        buffer.push(c);
        if c == b'%' {
            buffer.push(b'%');
        }
    }
}

fn unroll_concats<'a>(args: &mut Vec<&'a AstExpr>) {
    loop {
        let last = *args.last().unwrap();
        match last {
            AstExpr::Binary(be) if be.op == AstExprBinaryOp::Concat => {
                let n = args.len();
                args[n - 1] = &be.left;
                args.push(&be.right);
            }
            _ => break,
        }
    }
}

fn arith_op(op: AstExprBinaryOp, k: bool) -> Opcode {
    use AstExprBinaryOp as B;
    match op {
        B::Add => if k { Opcode::AddK } else { Opcode::Add },
        B::Sub => if k { Opcode::SubK } else { Opcode::Sub },
        B::Mul => if k { Opcode::MulK } else { Opcode::Mul },
        B::Div => if k { Opcode::DivK } else { Opcode::Div },
        B::FloorDiv => if k { Opcode::IDivK } else { Opcode::IDiv },
        B::Mod => if k { Opcode::ModK } else { Opcode::Mod },
        B::Pow => if k { Opcode::PowK } else { Opcode::Pow },
        _ => unreachable!("unexpected binary operation"),
    }
}

fn jump_op_compare(op: AstExprBinaryOp, not: bool) -> Opcode {
    use AstExprBinaryOp as B;
    match op {
        B::CompareNe => if not { Opcode::JumpIfEq } else { Opcode::JumpIfNotEq },
        B::CompareEq => if not { Opcode::JumpIfNotEq } else { Opcode::JumpIfEq },
        B::CompareLt | B::CompareGt => if not { Opcode::JumpIfNotLt } else { Opcode::JumpIfLt },
        B::CompareLe | B::CompareGe => if not { Opcode::JumpIfNotLe } else { Opcode::JumpIfLe },
        _ => unreachable!("unexpected compare operation"),
    }
}

impl<'a> Compiler<'a> {
    fn new(options: CompileOptions) -> Self {
        let table_local = Arc::new(AstLocal {
            name: AstName("__EXP"),
            location: Location::default(),
            shadow: None,
            function_depth: 0,
            loop_depth: 0,
            is_const: false,
            is_exported: true,
            annotation: None,
        });
        Compiler {
            bytecode: BytecodeBuilder::new(),
            options,
            functions: HashMap::default(),
            locals: HashMap::default(),
            globals: HashMap::default(),
            variables: HashMap::default(),
            constants: luau_common::fasthash::map_hinted(),
            locstants: HashMap::default(),
            expr_changes: ChangeLog::new(),
            local_changes: ChangeLog::new(),
            table_constants: HashSet::default(),
            table_shapes: HashMap::default(),
            builtins: HashMap::default(),
            function_types: HashMap::default(),
            reg_top: 0,
            stack_size: 0,
            arg_count: 0,
            has_loops: false,
            has_multi_ret: false,
            has_require: false,
            current_function: None,
            block_depth: 0,
            getfenv_used: false,
            setfenv_used: false,
            local_stack: Vec::new(),
            reg_captured: [false; 256],
            active_locals: 0,
            upvals: Vec::new(),
            loop_jumps: Vec::new(),
            loops: Vec::new(),
            local_uses: [0; 256],
            free_local_regs: Vec::new(),
            exports: Exports {
                table_local,
                exported_functions: HashSet::default(),
                exported_variables: Vec::new(),
                exported_classes: Vec::new(),
                exported_table_cid: -1,
                has_exports: false,
                has_type_exports: false,
                has_module_return: false,
            },
            enclosing_classes: HashMap::default(),
            class_private_field_offsets: HashMap::default(),
            curr_class: None,
            builtins_fold: false,
            builtins_fold_library_k: false,
            inline_frames: Vec::new(),
            local_types: HashMap::default(),
            expr_types: HashMap::default(),
        }
    }

    /// `hintTemporaryRegType`: records the known type of a temporary when it
    /// differs from what the instruction expects.
    fn hint_temporary_reg_type(&mut self, expr: &AstExpr, reg: u8, expected: u8, inst_len: u32) {
        if let Some(ty) = self.expr_types.get(&key(expr)).map(|x| x.bty) {
            if ty != expected {
                let pc = self.bytecode.get_debug_pc();
                self.bytecode.push_local_type_info(ty, reg, pc - inst_len, pc);
            }
        }
    }

    /// `hintTemporaryExprRegType`: only for temporaries (not locals).
    fn hint_temporary_expr_reg_type(&mut self, expr: &AstExpr, reg: u8, expected: u8, inst_len: u32) {
        if !matches!(unwrap_expr(expr), AstExpr::Local(_)) {
            self.hint_temporary_reg_type(expr, reg, expected, inst_len);
        }
    }

    // -----------------------------------------------------------------
    // Locals and upvalues
    // -----------------------------------------------------------------

    fn get_local_reg(&self, local: &Arc<AstLocal>) -> i32 {
        match self.locals.get(&local_key(local)) {
            Some(l) if l.allocated => l.reg as i32,
            _ => -1,
        }
    }

    fn get_upval(&mut self, local: &Arc<AstLocal>) -> u8 {
        for (uid, uv) in self.upvals.iter().enumerate() {
            if Arc::ptr_eq(uv, local) {
                return uid as u8;
            }
        }
        if self.upvals.len() as u32 >= MAX_UPVALUE_COUNT {
            CompileError::raise(
                local.location,
                format!(
                    "Out of upvalue registers when trying to allocate {}: exceeded limit {}",
                    local.name.0, MAX_UPVALUE_COUNT
                ),
            );
        }
        // mark local as captured so that closeLocals emits LOP_CLOSEUPVALS accordingly
        let written = self.variables.get(&local_key(local)).is_some_and(|v| v.written);
        if written {
            let l = self.locals.entry(local_key(local)).or_default();
            l.captured = true;
            if l.allocated {
                let reg = l.reg;
                self.reg_captured[reg as usize] = true;
            }
        }
        self.upvals.push(local.clone());
        (self.upvals.len() - 1) as u8
    }

    fn at_top_level(&self) -> bool {
        matches!(self.current_function, Some(f) if f.depth == 0) && self.block_depth == 0 && self.loops.is_empty()
    }

    fn check_exported_local(&self, local: &AstLocal, location: Location) {
        if local.is_exported && !self.at_top_level() {
            CompileError::raise(location, "'export' may only be applied to top-level statements");
        }
    }

    fn ensure_export_table(&mut self, loc: Location) {
        self.exports.has_exports = true;
        if self.locals.contains_key(&local_key(&self.exports.table_local)) {
            return;
        }
        let table_reg = self.alloc_reg(loc, 1);
        let cid = self.exports.exported_table_cid;
        if cid != -1 && cid < 32768 {
            self.bytecode.emit_ad(Opcode::DupTable, table_reg, cid as i16);
        } else {
            self.bytecode.emit_abc(Opcode::NewTable, table_reg, encode_hash_size(0), 0);
            self.bytecode.emit_aux(0);
        }
        let tl = self.exports.table_local.clone();
        self.push_local(&tl, table_reg, DEFAULT_ALLOC_PC);
    }

    fn get_export_table_reg(&mut self, loc: Location) -> u8 {
        let tl = self.exports.table_local.clone();
        let reg = self.get_local_reg(&tl);
        if reg >= 0 {
            return reg as u8;
        }
        let upval = self.get_upval(&tl);
        let reg = self.alloc_reg(loc, 1);
        self.bytecode.emit_abc(Opcode::GetUpval, reg, upval, 0);
        reg
    }

    fn build_export_table_shape(&mut self) {
        if self.exports.exported_variables.len() >= TableShape::MAX_LENGTH {
            return;
        }
        let mut shape = TableShape::default();
        let exported = self.exports.exported_variables.clone();
        for local in &exported {
            let Some(variable) = self.variables.get(&local_key(local)).cloned() else {
                CompileError::raise(local.location, "Local does not have corresponding variable");
            };
            let key_cid = self.bytecode.add_constant_string(local.name.0.as_bytes());
            if key_cid < 0 {
                return;
            }
            shape.keys.push(key_cid);
            shape.constants.push(-1);
            if variable.constant && !variable.written {
                let value_cid = match variable.init {
                    Some(Init::Expr(e)) => self.get_constant_index(e),
                    _ => -1,
                };
                if value_cid < 0 {
                    return;
                }
                *shape.constants.last_mut().unwrap() = value_cid;
                shape.has_constants = true;
            }
        }
        if !shape.keys.is_empty() {
            self.exports.exported_table_cid = self.bytecode.add_constant_table(shape);
        }
    }

    fn always_terminates(&self, node: &AstStat) -> bool {
        always_terminates(&self.constants, node)
    }

    fn always_terminates_block(&self, node: &AstStatBlock) -> bool {
        crate::analysis::always_terminates_block(&self.constants, node)
    }

    fn emit_load_k(&mut self, target: u8, cid: i32) {
        if cid < 32768 {
            self.bytecode.emit_ad(Opcode::LoadK, target, cid as i16);
        } else {
            self.bytecode.emit_ad(Opcode::LoadKx, target, 0);
            self.bytecode.emit_aux(cid as u32);
        }
    }

    // -----------------------------------------------------------------
    // Functions
    // -----------------------------------------------------------------

    fn compile_export_table(&mut self) {
        let loc = self.current_function.unwrap().location;
        let table_reg: u8;
        if !self.exports.is_empty() {
            self.ensure_export_table(loc);
            let tl = self.exports.table_local.clone();
            table_reg = self.get_local_reg(&tl) as u8;
            let classes = self.exports.exported_classes.clone();
            for (class_local, class_reg) in classes {
                let name = class_local.name.0.as_bytes();
                let cid = self.bytecode.add_constant_string(name);
                if cid < 0 {
                    const_limit(loc);
                }
                self.bytecode.emit_abc(Opcode::SetTableKs, class_reg, table_reg, BytecodeBuilder::get_string_hash(name) as u8);
                self.bytecode.emit_aux(cid as u32);
            }
            let funcs: Vec<Arc<AstLocal>> = self
                .exports
                .exported_variables
                .iter()
                .filter(|l| self.exports.exported_functions.contains(&local_key(l)))
                .cloned()
                .collect();
            for f in funcs {
                let cid = self.bytecode.add_constant_string(f.name.0.as_bytes());
                if cid < 0 {
                    const_limit(f.location);
                }
                let func_reg = self.get_local_reg(&f) as u8;
                self.bytecode.emit_abc(
                    Opcode::SetTableKs,
                    func_reg,
                    table_reg,
                    BytecodeBuilder::get_string_hash(f.name.0.as_bytes()) as u8,
                );
                self.bytecode.emit_aux(cid as u32);
            }
        } else {
            table_reg = self.alloc_reg(loc, 1);
            self.bytecode.emit_abc(Opcode::NewTable, table_reg, encode_hash_size(0), 0);
            self.bytecode.emit_aux(0);
        }

        let freeze_reg = self.alloc_reg(loc, 2);
        let freeze_cid = self.bytecode.add_constant_string(b"freeze");
        if freeze_cid < 0 {
            const_limit(loc);
        }
        let table_cid = self.bytecode.add_constant_string(b"table");
        if table_cid < 0 {
            const_limit(loc);
        }
        let mut fallback = true;
        if table_cid < 1024 && freeze_cid < 1024 {
            let iid = BytecodeBuilder::get_import_id2(table_cid, freeze_cid);
            let cid = self.bytecode.add_import(iid);
            if (0..32768).contains(&cid) {
                self.bytecode.emit_ad(Opcode::GetImport, freeze_reg, cid as i16);
                self.bytecode.emit_aux(iid);
                fallback = false;
            }
        }
        if fallback {
            self.bytecode.emit_abc(Opcode::GetGlobal, freeze_reg, 0, BytecodeBuilder::get_string_hash(b"table") as u8);
            self.bytecode.emit_aux(table_cid as u32);
            self.bytecode.emit_abc(Opcode::GetTableKs, freeze_reg, freeze_reg, BytecodeBuilder::get_string_hash(b"freeze") as u8);
            self.bytecode.emit_aux(freeze_cid as u32);
        }
        self.bytecode.emit_abc(Opcode::Move, freeze_reg + 1, table_reg, 0);
        self.bytecode.emit_abc(Opcode::Call, freeze_reg, 2, 2);
        self.close_locals(0);
        self.bytecode.emit_abc(Opcode::Return, freeze_reg, 2, 0);
    }

    fn compile_function(&mut self, func: FuncRef<'a>, protoflags: &mut u8) -> u32 {
        self.current_function = Some(func);
        if self.at_top_level() {
            self.build_export_table_shape();
        }

        let old_top = self.reg_top;
        let has_self = func.self_local.is_some() as u8;
        let nargs = has_self as usize + func.args.len();
        let fid = self.bytecode.begin_function(nargs as u8, func.vararg);

        self.set_debug_line(func.location);

        if func.vararg {
            self.bytecode.emit_abc(Opcode::PrepVarargs, nargs as u8, 0, 0);
        }

        let args = self.alloc_reg(func.location, nargs as u32);
        if let Some(s) = func.self_local {
            self.push_local(s, args, DEFAULT_ALLOC_PC);
        }
        for (i, a) in func.args.iter().enumerate() {
            self.push_local(a, args + has_self + i as u8, DEFAULT_ALLOC_PC);
        }
        self.arg_count = self.local_stack.len();

        let body = func.body;
        let mut terminates_early = false;
        self.current_function = Some(func);
        for stat in &body.body {
            self.pop_stat_locals(body, stat, 0);
            self.compile_stat(stat);
            if self.always_terminates(stat) {
                terminates_early = true;
                break;
            }
        }

        self.set_debug_line_end(body.location);
        if !self.exports.is_empty() && self.at_top_level() {
            self.compile_export_table();
        } else if self.exports.has_type_exports && !self.exports.has_module_return && self.at_top_level() {
            self.compile_export_table();
        } else if !terminates_early {
            self.close_locals(0);
            self.bytecode.emit_abc(Opcode::Return, 0, 1, 0);
        }

        self.bytecode.set_debug_function_line_defined(func.location.begin.line as i32 + 1);
        if self.options.debug_level >= 1 && !func.debugname.is_empty() {
            self.bytecode.set_debug_function_name(func.debugname.as_bytes());
        }
        if self.options.debug_level >= 2 && !self.upvals.is_empty() {
            for l in self.upvals.clone() {
                self.bytecode.push_debug_upval(l.name.0.as_bytes());
            }
        }

        if let Some(t) = self.function_types.remove(&func.key) {
            self.bytecode.set_function_type_info(t);
        }
        if self.options.type_info_level >= 1 {
            for l in self.upvals.clone() {
                let ty = self.local_types.get(&local_key(&l)).copied().unwrap_or(crate::types::lbc_type::ANY);
                self.bytecode.push_upval_type_info(ty);
            }
        }

        if self.options.optimization_level >= 1 {
            self.bytecode.fold_jumps();
        }
        if !self.bytecode.expand_jumps() {
            jump_limit(func.location);
        }

        self.pop_locals(0);

        if self.bytecode.get_instruction_count() as u32 > MAX_INSTRUCTION_COUNT {
            CompileError::raise(func.location, "Exceeded function instruction limit; split the function into parts to compile");
        }

        if func.depth == 0 && !self.has_loops {
            *protoflags |= proto_flag::NATIVE_COLD as u8;
        }
        if func.native {
            *protoflags |= proto_flag::NATIVE_FUNCTION as u8;
        }
        if !self.exports.is_empty() && func.depth == 0 {
            *protoflags |= proto_flag::USES_EXPORT as u8;
        }
        if self.exports.has_type_exports && !self.exports.has_module_return && func.depth == 0 {
            *protoflags |= proto_flag::USES_EXPORT as u8;
        }

        let inlinable = !self.has_multi_ret && !self.getfenv_used && !self.setfenv_used && !self.has_require;
        let mut cost = 0u64;
        if inlinable && self.upvals.is_empty() {
            *protoflags |= LPF_INLINABLE;
            cost = model_cost(func.body, func.args, &self.builtins, &self.constants);
        }

        self.bytecode.end_function(self.stack_size as u8, self.upvals.len() as u8, *protoflags, cost);

        let mut info = FunctionInfo { id: fid, upvals: self.upvals.clone(), ..Default::default() };
        // record information for inlining
        if self.options.optimization_level >= 2 && !func.vararg && func.self_local.is_none() && !self.getfenv_used && !self.setfenv_used {
            info.can_inline = true;
            info.stack_size = self.stack_size;
            info.cost_model = if cost == 0 { model_cost(func.body, func.args, &self.builtins, &self.constants) } else { cost };
            if self.always_terminates_block(func.body) {
                let (returns_one, kind, local) = self.return_visitor(func);
                info.returns_one = returns_one;
                info.return_kind = kind;
                if let Some(l) = &local {
                    info.return_local_captured = self.locals.get(&local_key(l)).is_some_and(|lc| lc.captured);
                }
                info.return_local = local;
            }
        }
        self.functions.insert(func.key, info);

        self.upvals.clear();
        self.stack_size = 0;
        self.free_local_regs.clear();
        self.arg_count = 0;
        self.has_loops = false;
        self.has_multi_ret = false;
        self.has_require = false;
        self.current_function = None;
        self.reg_top = old_top;
        fid
    }

    // -----------------------------------------------------------------
    // Calls
    // -----------------------------------------------------------------

    /// Conservative at `-O1`: any call or vararg may return multiple values.
    fn is_expr_mult_ret(&self, node: &AstExpr) -> bool {
        let AstExpr::Call(call) = node else {
            return matches!(node, AstExpr::Varargs(_));
        };
        // conservative version, optimized for compilation throughput
        if self.options.optimization_level <= 1 {
            return true;
        }
        // builtin calls that can be constant-folded
        if self.is_constant(node) {
            return false;
        }
        // builtin calls known to return one value
        if let Some(&bfid) = self.builtins.get(&key(node)) {
            if bfid != lbf::NONE {
                return get_builtin_info(bfid).1 != 1;
            }
        }
        // local function calls known to return one value
        if let Some(fk) = self.get_function_key(&call.func) {
            if self.functions.get(&fk).is_some_and(|fi| fi.returns_one) {
                return false;
            }
        }
        true
    }

    /// [`Self::get_function_expr`] for nodes of any lifetime (by key).
    fn get_function_key(&self, node: &AstExpr) -> Option<usize> {
        match node {
            AstExpr::Local(e) => {
                let lv = self.variables.get(&local_key(&e.local))?;
                if lv.written {
                    return None;
                }
                match lv.init? {
                    Init::Func(f) => Some(key(&**f)),
                    Init::Expr(x) => self.get_function_expr(x).map(key),
                }
            }
            AstExpr::IndexName(e) if !(self.options.classes && e.private_access) => {
                let v = self.try_index_constant_table(e)?;
                self.get_function_expr(v).map(key)
            }
            AstExpr::Group(g) => self.get_function_key(&g.expr),
            AstExpr::TypeAssertion(t) => self.get_function_key(&t.expr),
            AstExpr::Instantiate(i) => self.get_function_key(&i.expr),
            AstExpr::Function(f) => Some(key(&**f)),
            _ => None,
        }
    }

    /// `getFunctionExpr`: the function literal a callee expression always
    /// evaluates to, if known.
    fn get_function_expr(&self, node: &'a AstExpr) -> Option<&'a AstExprFunction> {
        match node {
            AstExpr::Local(e) => {
                let lv = self.variables.get(&local_key(&e.local))?;
                if lv.written {
                    return None;
                }
                match lv.init? {
                    Init::Func(f) => Some(&**f),
                    Init::Expr(x) => self.get_function_expr(x),
                }
            }
            AstExpr::IndexName(e) if !(self.options.classes && e.private_access) => {
                let v = self.try_index_constant_table(e)?;
                self.get_function_expr(v)
            }
            AstExpr::Group(g) => self.get_function_expr(&g.expr),
            AstExpr::TypeAssertion(t) => self.get_function_expr(&t.expr),
            AstExpr::Instantiate(i) => self.get_function_expr(&i.expr),
            AstExpr::Function(f) => Some(&**f),
            _ => None,
        }
    }

    /// `tryIndexConstantTable`: the initializer of `t.name` when `t` is a
    /// constant table local.
    fn try_index_constant_table(&self, expr: &AstExprIndexName) -> Option<&'a AstExpr> {
        let AstExpr::Local(tl) = unwrap_expr(&expr.expr) else { return None };
        let lv = self.variables.get(&local_key(&tl.local))?;
        if lv.written || !self.table_constants.contains(&local_key(&tl.local)) {
            return None;
        }
        let Some(Init::Expr(init)) = lv.init else { return None };
        let AstExpr::Table(table) = unwrap_expr(init) else { return None };
        let mut found: Option<&'a AstExpr> = None;
        for item in &table.items {
            if matches!(item.kind, AstExprTableItemKind::Record | AstExprTableItemKind::General) {
                let k = item.key.as_deref()?;
                match self.constants.get(&key(k)) {
                    None => found = None,
                    Some(Constant::String(sv)) if !sv.is_empty() => {
                        if &**sv == expr.index.0.as_bytes() {
                            found = Some(&item.value);
                        }
                    }
                    _ => {}
                }
            }
        }
        found
    }

    /// `ReturnVisitor`: (returns one value, return kind, returned local).
    fn return_visitor(&self, func: FuncRef<'a>) -> (bool, ReturnKind, Option<Arc<AstLocal>>) {
        struct V<'c, 'a> {
            c: &'c Compiler<'a>,
            args: &'a [Arc<AstLocal>],
            returns_one: bool,
            kind: ReturnKind,
            local: Option<Arc<AstLocal>>,
        }
        impl<'z, 'c, 'a> Visitor<'z> for V<'c, 'a> {
            fn visit_expr(&mut self, _e: &'z AstExpr) -> bool {
                false
            }
            fn visit_function(&mut self, _f: &'z AstExprFunction) -> bool {
                false
            }
            fn visit_stat(&mut self, s: &'z AstStat) -> bool {
                let AstStat::Return(stat) = s else { return true };
                if stat.list.len() == 1 && !self.c.is_expr_mult_ret(&stat.list[0]) {
                    if let AstExpr::Local(el) = unwrap_expr(&stat.list[0]) {
                        let mut kind = ReturnKind::Local;
                        let mut local = Some(el.local.clone());
                        if self.args.iter().any(|a| Arc::ptr_eq(a, &el.local)) {
                            kind = ReturnKind::Parameter;
                        }
                        if el.upvalue {
                            kind = ReturnKind::Expression;
                            local = None;
                        }
                        let same = match (&self.local, &local) {
                            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                            (None, None) => true,
                            _ => false,
                        };
                        if self.kind == ReturnKind::None {
                            self.kind = kind;
                            self.local = local;
                        } else if self.kind != kind || !same {
                            self.kind = ReturnKind::Unknown;
                            self.local = None;
                        }
                    } else if self.kind == ReturnKind::None {
                        self.kind = ReturnKind::Expression;
                    } else if self.kind != ReturnKind::Expression {
                        self.kind = ReturnKind::Unknown;
                        self.local = None;
                    }
                } else {
                    self.returns_one = false;
                    self.kind = ReturnKind::Unknown;
                    self.local = None;
                }
                false
            }
        }
        let mut v = V { c: self, args: func.args, returns_one: true, kind: ReturnKind::None, local: None };
        walk_block(&mut v, func.body);
        (v.returns_one, v.kind, v.local)
    }

    /// `foldConstants` over `walk`'s nodes with the compiler's maps,
    /// optionally logging changes for undo.
    /// Folds while recording into the shared change logs.
    fn fold_logged_shared(&mut self, walk: impl for<'m> FnOnce(&mut ConstantFolder<'m, 'a>)) {
        let mut ec = std::mem::take(&mut self.expr_changes);
        let mut lc = std::mem::take(&mut self.local_changes);
        self.fold_constants_logged(Some(&mut ec), Some(&mut lc), walk);
        self.expr_changes = ec;
        self.local_changes = lc;
    }

    /// `undoChanges` over the shared logs (which are left as they are).
    fn undo_shared_changes(&mut self) {
        let ec = std::mem::take(&mut self.expr_changes);
        let lc = std::mem::take(&mut self.local_changes);
        undo_changes(&mut self.constants, &ec);
        undo_changes(&mut self.locstants, &lc);
        self.expr_changes = ec;
        self.local_changes = lc;
    }

    fn fold_constants_logged(
        &mut self,
        expr_log: Option<&mut ChangeLog>,
        local_log: Option<&mut ChangeLog>,
        walk: impl for<'m> FnOnce(&mut ConstantFolder<'m, 'a>),
    ) {
        let builtins = if self.builtins_fold { Some(&self.builtins) } else { None };
        ConstantFolder::run_on(
            &mut self.constants,
            &mut self.variables,
            &mut self.locstants,
            builtins,
            self.builtins_fold_library_k,
            self.options.vector_double,
            &self.table_constants,
            expr_log,
            local_log,
            walk,
        );
    }

    fn cost_model_inlined_call(&mut self, call: &'a AstExprCall, func: &'a AstExprFunction) -> u64 {
        for (i, var) in func.args.iter().enumerate() {
            let arg = call.args.get(i).map(|b| &**b);
            // last expression is a multret: no constant for it, it fills all values
            if i + 1 == call.args.len() && func.args.len() > call.args.len() && arg.is_some_and(|a| self.is_expr_mult_ret(a)) {
                break;
            }
            if self.variables.get(&local_key(var)).is_some_and(|v| v.written) {
                continue;
            }
            match arg {
                None => {
                    self.locstants.insert(local_key(var), Constant::Nil);
                }
                Some(a) => {
                    if let Some(cv) = self.constants.get(&key(a)) {
                        if cv.is_known() {
                            let cv = cv.clone();
                            self.locstants.insert(local_key(var), cv);
                        }
                    }
                }
            }
        }
        self.expr_changes.clear();
        self.local_changes.clear();
        self.fold_logged_shared(|f: &mut ConstantFolder<'_, 'a>| walk_block(f, &func.body));
        let cost = model_cost(&func.body, &func.args, &self.builtins, &self.constants);
        for var in &func.args {
            if let Some(v) = self.locstants.get_mut(&local_key(var)) {
                *v = Constant::Unknown;
            }
        }
        self.undo_shared_changes();
        cost
    }

    #[allow(clippy::too_many_arguments)]
    fn try_compile_inlined_call(
        &mut self,
        call: &'a AstExprCall,
        func: &'a AstExprFunction,
        target: u8,
        target_count: u8,
        mult_ret: bool,
        threshold_base: i32,
        threshold_max_boost: i32,
        depth_limit: usize,
    ) -> bool {
        let fi = self.functions.get(&key(func)).cloned().unwrap_or_default();
        // make sure we have enough register space
        if self.reg_top > 128 || fi.stack_size > 32 {
            self.bytecode.add_debug_remark("inlining failed: high register pressure".into());
            return false;
        }
        if self.inline_frames.len() >= depth_limit {
            self.bytecode.add_debug_remark("inlining failed: too many inlined frames".into());
            return false;
        }
        if self.inline_frames.iter().any(|f| f.func == key(func)) {
            self.bytecode.add_debug_remark("inlining failed: can't inline recursive calls".into());
            return false;
        }
        // multret calls expect L->top to be adjusted
        if mult_ret {
            self.bytecode.add_debug_remark("inlining failed: can't convert fixed returns to multret".into());
            return false;
        }
        // constant bitvector of the arguments for the cost model
        let mut varc = [false; 8];
        let mut has_constant = false;
        for i in 0..func.args.len().min(call.args.len()).min(8) {
            if self.is_constant(&call.args[i]) {
                varc[i] = true;
                has_constant = true;
            }
        }
        // if the last argument only returns a single value, all following arguments are nil
        if !call.args.is_empty() && !self.is_expr_mult_ret(call.args.last().unwrap()) {
            for v in varc.iter_mut().take(func.args.len().min(8)).skip(call.args.len()) {
                *v = true;
                has_constant = true;
            }
        }
        let call_cost_model = if has_constant { self.cost_model_inlined_call(call, func) } else { fi.cost_model };
        let inlined_cost = compute_cost(call_cost_model, &varc[..func.args.len().min(8)]);
        let baseline_cost = compute_cost(fi.cost_model, &[]) + 3;
        let inline_profit = if inlined_cost == 0 { threshold_max_boost } else { threshold_max_boost.min(100 * baseline_cost / inlined_cost) };
        let threshold = threshold_base * inline_profit / 100;
        let is_iife = matches!(unwrap_expr(&call.func), AstExpr::Function(_));
        if inlined_cost > threshold && !is_iife {
            if self.bytecode.needs_debug_remarks() {
                self.bytecode.add_debug_remark(format!(
                    "inlining failed: too expensive (cost {}, profit {:.2}x)",
                    inlined_cost,
                    inline_profit as f64 / 100.0
                ));
            }
            return false;
        }
        if self.bytecode.needs_debug_remarks() {
            self.bytecode.add_debug_remark(format!(
                "inlining succeeded (cost {}, profit {:.2}x, depth {})",
                inlined_cost,
                inline_profit as f64 / 100.0,
                self.inline_frames.len()
            ));
        }
        self.compile_inlined_call(call, func, target, target_count);
        true
    }

    /// `isRegisterUsedInCallArguments`.
    fn is_register_used_in_call_arguments(&self, call: &AstExprCall, arg: Option<&AstExpr>, start_arg: usize, reg: u8) -> bool {
        struct V<'c, 'a> {
            c: &'c Compiler<'a>,
            reg: u8,
            used: bool,
        }
        impl<'z> Visitor<'z> for V<'_, '_> {
            fn visit_expr(&mut self, e: &'z AstExpr) -> bool {
                if let AstExpr::Local(l) = e {
                    if self.c.get_local_reg(&l.local) == self.reg as i32 {
                        self.used = true;
                    }
                }
                !self.used
            }
        }
        let mut v = V { c: self, reg, used: false };
        if let Some(a) = arg {
            if self.get_expr_local_reg(a) != reg as i32 {
                walk_expr(&mut v, &call.args[start_arg]);
            }
        }
        let mut i = start_arg + 1;
        while i < call.args.len() && !v.used {
            walk_expr(&mut v, &call.args[i]);
            i += 1;
        }
        v.used
    }

    fn compile_inlined_call(&mut self, call: &'a AstExprCall, func: &'a AstExprFunction, target: u8, target_count: u8) {
        let fi = self.functions.get(&key(func)).cloned().unwrap_or_default();
        // results go straight to the target register unless the returned local
        // is captured, there is no target, or the target is captured
        let no_target_move_elision = fi.return_local_captured || target_count == 0 || self.reg_captured[target as usize];

        let old_top = self.reg_top;
        let old_locals = self.local_stack.len();
        let mut args: Vec<InlineArg<'a>> = Vec::with_capacity(func.args.len());

        for (i, var) in func.args.iter().enumerate() {
            let arg: Option<&'a AstExpr> = call.args.get(i).map(|b| &**b);
            let can_use_parameter_target = fi.return_kind == ReturnKind::Parameter
                && fi.return_local.as_ref().is_some_and(|l| Arc::ptr_eq(l, var))
                && !no_target_move_elision;

            if i + 1 == call.args.len() && func.args.len() > call.args.len() && arg.is_some_and(|a| self.is_expr_mult_ret(a)) {
                // the last argument fills all remaining parameters
                let a = arg.unwrap();
                let tail = (func.args.len() - call.args.len()) as u32 + 1;
                let reg = self.alloc_reg(a.location(), tail);
                let allocpc = self.bytecode.get_debug_pc();
                match a {
                    AstExpr::Call(c) => self.compile_expr_call(a, c, reg, tail as u8, true, false),
                    AstExpr::Varargs(v) => self.compile_expr_varargs(v.location, reg, tail as u8, false),
                    _ => unreachable!("unexpected expression type"),
                }
                for (j, l) in func.args.iter().enumerate().skip(i) {
                    args.push(InlineArg { local: l, reg: reg + (j - i) as u8, value: Constant::Unknown, allocpc, init: None });
                }
                break;
            } else if self.variables.get(&local_key(var)).is_some_and(|v| v.written) {
                if can_use_parameter_target && !self.is_register_used_in_call_arguments(call, arg, i, target) {
                    match arg {
                        Some(a) => self.compile_expr_temp(a, target),
                        None => self.bytecode.emit_abc(Opcode::LoadNil, target, 0, 0),
                    }
                    args.push(InlineArg { local: var, reg: target, value: Constant::Unknown, allocpc: DEFAULT_ALLOC_PC, init: None });
                } else {
                    // a mutated argument needs a fresh register even if it's a constant
                    let reg = self.alloc_reg(call.location, 1);
                    let allocpc = self.bytecode.get_debug_pc();
                    match arg {
                        Some(a) => self.compile_expr_temp(a, reg),
                        None => self.bytecode.emit_abc(Opcode::LoadNil, reg, 0, 0),
                    }
                    args.push(InlineArg { local: var, reg, value: Constant::Unknown, allocpc, init: None });
                }
            } else if arg.is_none() {
                args.push(InlineArg { local: var, reg: INVALID_REG, value: Constant::Nil, allocpc: 0, init: None });
            } else if let Some(cv) = self.constants.get(&key(arg.unwrap())).filter(|c| c.is_known()).cloned() {
                args.push(InlineArg { local: var, reg: INVALID_REG, value: cv, allocpc: 0, init: None });
            } else {
                let a = arg.unwrap();
                let le = Self::get_expr_local(a);
                let lv = le.and_then(|le| self.variables.get(&local_key(&le.local)));
                let lreg = le.map_or(-1, |le| self.get_local_reg(&le.local));
                if lreg >= 0 && lv.is_none_or(|lv| !lv.written) {
                    let init = lv.and_then(|lv| lv.init);
                    args.push(InlineArg { local: var, reg: lreg as u8, value: Constant::Unknown, allocpc: DEFAULT_ALLOC_PC, init });
                } else if can_use_parameter_target && !self.is_register_used_in_call_arguments(call, arg, i, target) {
                    self.compile_expr_temp(a, target);
                    args.push(InlineArg { local: var, reg: target, value: Constant::Unknown, allocpc: DEFAULT_ALLOC_PC, init: None });
                } else {
                    let temp = self.alloc_reg(a.location(), 1);
                    let allocpc = self.bytecode.get_debug_pc();
                    let temp = self.compile_expr_auto_temp(a, temp, target);
                    args.push(InlineArg { local: var, reg: temp, value: Constant::Unknown, allocpc, init: Some(Init::Expr(a)) });
                }
            }
        }

        // evaluate extra expressions for side effects
        for a in call.args.iter().skip(func.args.len()) {
            self.compile_expr_side(a);
        }

        // apply all evaluated arguments to the compiler state
        for a in &args {
            if !a.value.is_known() {
                self.push_local(a.local, a.reg, a.allocpc);
                if let Some(init) = a.init {
                    if let Some(lv) = self.variables.get_mut(&local_key(a.local)) {
                        lv.init = Some(init);
                    }
                }
            } else {
                self.locstants.insert(local_key(a.local), a.value.clone());
            }
        }

        let result_local = if fi.return_kind == ReturnKind::Local && !no_target_move_elision { fi.return_local.clone() } else { None };
        self.inline_frames.push(InlineFrame {
            func: key(func),
            local_offset: old_locals,
            target,
            target_count,
            result_local,
            return_jumps: Vec::new(),
        });

        // builtins of the inlined body, recorded for undo
        let mut builtins_backup: Vec<(usize, i32)> = Vec::new();
        for (k, bfid) in analyze_builtins(&func.body, &self.globals, &self.variables) {
            let cur = self.builtins.get(&k).copied().unwrap_or(lbf::NONE);
            if bfid != cur {
                builtins_backup.push((k, cur));
                self.builtins.insert(k, bfid);
            }
        }

        self.expr_changes.clear();
        self.local_changes.clear();
        self.fold_logged_shared(|f: &mut ConstantFolder<'_, 'a>| walk_block(f, &func.body));

        let mut terminates_early = false;
        for stat in &func.body.body {
            self.pop_stat_locals(&func.body, stat, old_locals);
            self.compile_stat(stat);
            if self.always_terminates(stat) {
                terminates_early = true;
                // remove the last jump, which jumps directly to the next instruction
                let label = self.bytecode.emit_label();
                let frame = self.inline_frames.last_mut().unwrap();
                if frame.return_jumps.last().is_some_and(|&j| j + 1 == label) {
                    frame.return_jumps.pop();
                    self.bytecode.undo_emit(Opcode::Jump);
                }
                break;
            }
        }

        // the fallthrough path clears the target registers
        if !terminates_early {
            for i in 0..target_count {
                self.bytecode.emit_abc(Opcode::LoadNil, target + i, 0, 0);
            }
            self.close_locals(old_locals);
        }

        self.pop_locals(old_locals);

        let return_label = self.bytecode.emit_label();
        let jumps = std::mem::take(&mut self.inline_frames.last_mut().unwrap().return_jumps);
        self.patch_jumps(call.location, &jumps, return_label);
        self.inline_frames.pop();

        // clean up constant state for future inlining attempts
        for var in &func.args {
            if let Some(v) = self.locstants.get_mut(&local_key(var)) {
                *v = Constant::Unknown;
            }
            if let Some(lv) = self.variables.get_mut(&local_key(var)) {
                lv.init = None;
            }
        }
        for (k, bfid) in builtins_backup {
            self.builtins.insert(k, bfid);
        }
        self.undo_shared_changes();
        self.reg_top = old_top;
    }

    /// `compileInlineReturn`.
    fn compile_inline_return(&mut self, stat: &'a AstStatReturn) {
        self.set_debug_line(stat.location);
        let (target, target_count, local_offset) = {
            let f = self.inline_frames.last().unwrap();
            (f.target, f.target_count, f.local_offset)
        };
        self.compile_expr_list_temp(&stat.list, target, target_count, false);
        self.close_locals(local_offset);
        let jump_label = self.bytecode.emit_label();
        self.bytecode.emit_ad(Opcode::Jump, 0, 0);
        self.inline_frames.last_mut().unwrap().return_jumps.push(jump_label);
    }

    /// `tryCompileUnrolledFor`.
    fn try_compile_unrolled_for(&mut self, stat: &'a AstStatFor, threshold_base: i32, threshold_max_boost: i32) -> bool {
        let fromc = self.get_constant(&stat.from);
        let toc = self.get_constant(&stat.to);
        let stepc = match &stat.step {
            Some(s) => self.get_constant(s),
            None => Constant::Number(1.0),
        };
        let trip_count = match (&fromc, &toc, &stepc) {
            (Constant::Number(f), Constant::Number(t), Constant::Number(st)) => get_trip_count(*f, *t, *st),
            _ => -1,
        };
        if trip_count < 0 {
            self.bytecode.add_debug_remark("loop unroll failed: invalid iteration count".into());
            return false;
        }
        if trip_count == 0 {
            self.bytecode.add_debug_remark("loop unroll succeeded: empty loop".into());
            return true;
        }
        if trip_count > threshold_base {
            self.bytecode.add_debug_remark(format!("loop unroll failed: too many iterations ({trip_count})"));
            return false;
        }
        if self.variables.get(&local_key(&stat.var)).is_some_and(|v| v.written) {
            self.bytecode.add_debug_remark("loop unroll failed: mutable loop variable".into());
            return false;
        }
        let cost_model = model_cost(&stat.body, std::slice::from_ref(&stat.var), &self.builtins, &self.constants);
        let unrolled_cost = compute_cost(cost_model, &[true]) * trip_count;
        let baseline_cost = (compute_cost(cost_model, &[]) + 1) * trip_count;
        let unroll_profit = if unrolled_cost == 0 { threshold_max_boost } else { threshold_max_boost.min(100 * baseline_cost / unrolled_cost) };
        let threshold = threshold_base * unroll_profit / 100;
        if unrolled_cost > threshold {
            if self.bytecode.needs_debug_remarks() {
                self.bytecode.add_debug_remark(format!(
                    "loop unroll failed: too expensive (iterations {}, cost {}, profit {:.2}x)",
                    trip_count,
                    unrolled_cost,
                    unroll_profit as f64 / 100.0
                ));
            }
            return false;
        }
        if self.bytecode.needs_debug_remarks() {
            self.bytecode.add_debug_remark(format!(
                "loop unroll succeeded (iterations {}, cost {}, profit {:.2}x)",
                trip_count,
                unrolled_cost,
                unroll_profit as f64 / 100.0
            ));
        }
        let (Constant::Number(from), Constant::Number(step)) = (fromc, stepc) else { unreachable!() };
        self.compile_unrolled_for(stat, trip_count, from, step);
        true
    }

    fn compile_unrolled_for(&mut self, stat: &'a AstStatFor, trip_count: i32, from: f64, step: f64) {
        let var = local_key(&stat.var);
        let old_locals = self.local_stack.len();
        let old_jumps = self.loop_jumps.len();
        self.loops.push(Loop { local_offset: old_locals, local_offset_continue: old_locals, continue_used: None });

        self.expr_changes.clear();
        self.local_changes.clear();
        for iv in 0..trip_count {
            // re-fold the loop body with the new value of the induction variable
            self.locstants.insert(var, Constant::Number(from + iv as f64 * step));
            let walk = |f: &mut ConstantFolder<'_, 'a>| {
                walk_expr(f, &stat.from);
                walk_expr(f, &stat.to);
                if let Some(s) = &stat.step {
                    walk_expr(f, s);
                }
                walk_block(f, &stat.body);
            };
            if iv == 0 {
                self.fold_logged_shared(walk);
            } else {
                self.fold_constants_logged(None, None, walk);
            }

            let iter_jumps = self.loop_jumps.len();
            self.compile_block_stat(&stat.body);

            // continue jumps go to the next iteration
            let cont_label = self.bytecode.emit_label();
            for i in iter_jumps..self.loop_jumps.len() {
                if matches!(self.loop_jumps[i].ty, LoopJumpType::Continue) {
                    let l = self.loop_jumps[i].label;
                    self.patch_jump(stat.location, l, cont_label);
                }
            }
        }

        // break jumps go past the loop
        let end_label = self.bytecode.emit_label();
        for i in old_jumps..self.loop_jumps.len() {
            if matches!(self.loop_jumps[i].ty, LoopJumpType::Break) {
                let l = self.loop_jumps[i].label;
                self.patch_jump(stat.location, l, end_label);
            }
        }
        self.loop_jumps.truncate(old_jumps);
        self.loops.pop();

        if let Some(v) = self.locstants.get_mut(&var) {
            *v = Constant::Unknown;
        }
        self.undo_shared_changes();
    }

    fn compile_expr_temp_mult_ret(&mut self, node: &'a AstExpr, target: u8) -> bool {
        match node {
            // calls that always return one value become fixedret calls
            AstExpr::Call(_) if self.options.optimization_level >= 2 && !self.is_expr_mult_ret(node) => {
                self.compile_expr_temp(node, target);
                false
            }
            AstExpr::Call(call) => {
                let old = self.reg_top;
                self.reg_top = target as u32;
                self.compile_expr_call(node, call, target, 0, true, true);
                self.reg_top = old;
                true
            }
            AstExpr::Varargs(v) => {
                let old = self.reg_top;
                self.reg_top = target as u32;
                self.compile_expr_varargs(v.location, target, 0, true);
                self.reg_top = old;
                true
            }
            _ => {
                self.compile_expr_temp(node, target);
                false
            }
        }
    }

    fn compile_expr_temp_top(&mut self, node: &'a AstExpr, target: u8) {
        let old = self.reg_top;
        self.reg_top = target as u32 + 1;
        self.compile_expr_temp(node, target);
        self.reg_top = old;
    }

    fn compile_expr_varargs(&mut self, loc: Location, target: u8, target_count: u8, mult_ret: bool) {
        self.set_debug_line(loc);
        self.bytecode.emit_abc(Opcode::GetVarargs, target, if mult_ret { 0 } else { target_count + 1 }, 0);
    }

    fn compile_expr_select_vararg(
        &mut self,
        call: &'a AstExprCall,
        target: u8,
        target_count: u8,
        target_top: bool,
        mult_ret: bool,
        regs: u8,
    ) {
        let arg = &call.args[0];
        let argreg = match self.get_expr_local_reg(arg) {
            r if r >= 0 => r as u8,
            _ => {
                let r = regs + 1;
                self.compile_expr_temp_top(arg, r);
                r
            }
        };
        let fastcall_label = self.bytecode.emit_label();
        self.bytecode.emit_abc(Opcode::FastCall1, lbf::SELECT_VARARG as u8, argreg, 0);
        self.compile_expr_temp(&call.func, regs);
        if argreg != regs + 1 {
            self.bytecode.emit_abc(Opcode::Move, regs + 1, argreg, 0);
        }
        self.bytecode.emit_abc(Opcode::GetVarargs, regs + 2, 0, 0);
        let call_label = self.bytecode.emit_label();
        if !self.bytecode.patch_skip_c(fastcall_label, call_label) {
            jump_limit(call.func.location());
        }
        self.bytecode.emit_abc(Opcode::Call, regs, 0, if mult_ret { 0 } else { target_count + 1 });
        if !target_top {
            for i in 0..target_count {
                self.bytecode.emit_abc(Opcode::Move, target + i, regs + i, 0);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_expr_fastcall_n(
        &mut self,
        call: &'a AstExprCall,
        target: u8,
        target_count: u8,
        target_top: bool,
        mult_ret: bool,
        regs: u8,
        bfid: i32,
        bf_k: i32,
    ) {
        let nargs = call.args.len();
        let opc = if nargs == 1 {
            Opcode::FastCall1
        } else if bf_k >= 0 || (nargs == 2 && self.is_constant(&call.args[1])) {
            Opcode::FastCall2K
        } else if nargs == 2 {
            Opcode::FastCall2
        } else {
            Opcode::FastCall3
        };

        let mut args = [0u32; 3];
        for i in 0..nargs {
            if i > 0 && opc == Opcode::FastCall2K {
                let cid = self.get_constant_index(&call.args[i]);
                if cid < 0 {
                    const_limit(call.location);
                }
                args[i] = cid as u32;
            } else {
                let reg = self.get_expr_local_reg(&call.args[i]);
                if reg >= 0 {
                    args[i] = reg as u32;
                } else {
                    let r = regs as u32 + 1 + i as u32;
                    let old = self.reg_top;
                    self.reg_top = r + 1;
                    args[i] = self.compile_expr_auto_temp(&call.args[i], r as u8, INVALID_REG) as u32;
                    self.reg_top = old;
                }
            }
        }

        let fastcall_label = self.bytecode.emit_label();
        self.bytecode.emit_abc(opc, bfid as u8, args[0] as u8, 0);
        if opc == Opcode::FastCall3 {
            self.bytecode.emit_aux(args[1] | (args[2] << 8));
        } else if opc != Opcode::FastCall1 {
            self.bytecode.emit_aux(if bf_k >= 0 { bf_k as u32 } else { args[1] });
        }

        for i in 0..nargs {
            let dst = regs + 1 + i as u8;
            if i > 0 && opc == Opcode::FastCall2K {
                self.emit_load_k(dst, args[i] as i32);
            } else if args[i] != dst as u32 {
                self.bytecode.emit_abc(Opcode::Move, dst, args[i] as u8, 0);
            }
        }

        self.compile_expr_temp(&call.func, regs);
        let call_label = self.bytecode.emit_label();
        if !self.bytecode.patch_skip_c(fastcall_label, call_label) {
            jump_limit(call.func.location());
        }
        self.bytecode.emit_abc(Opcode::Call, regs, nargs as u8 + 1, if mult_ret { 0 } else { target_count + 1 });
        if !target_top {
            for i in 0..target_count {
                self.bytecode.emit_abc(Opcode::Move, target + i, regs + i, 0);
            }
        }
    }

    fn compile_expr_call(
        &mut self,
        node: &'a AstExpr,
        call: &'a AstExprCall,
        target: u8,
        target_count: u8,
        target_top: bool,
        mult_ret: bool,
    ) {
        self.set_debug_line(call.location);

        if !call.self_ {
            if let AstExpr::Global(g) = &*call.func {
                if g.name.0 == "require" {
                    self.has_require = true;
                }
            }
        }

        // try inlining the function
        if self.options.optimization_level >= 2 && !call.self_ {
            if let Some(func) = self.get_function_expr(&call.func) {
                if self.functions.get(&key(func)).is_some_and(|fi| fi.can_inline)
                    && self.try_compile_inlined_call(
                        call,
                        func,
                        target,
                        target_count,
                        mult_ret,
                        INLINE_THRESHOLD,
                        INLINE_THRESHOLD_MAX_BOOST,
                        INLINE_DEPTH,
                    )
                {
                    return;
                }
                // add a debug remark for cases when we didn't even call tryCompileInlinedCall
                if self.bytecode.needs_debug_remarks() {
                    let fi = self.functions.get(&key(func)).map(|fi| fi.can_inline);
                    if fi != Some(true) {
                        if func.vararg {
                            self.bytecode.add_debug_remark("inlining failed: function is variadic".into());
                        } else if fi.is_none() {
                            self.bytecode.add_debug_remark("inlining failed: can't inline recursive calls".into());
                        } else if self.getfenv_used || self.setfenv_used {
                            self.bytecode.add_debug_remark("inlining failed: module uses getfenv/setfenv".into());
                        }
                    }
                }
            }
        }

        let old_top = self.reg_top;

        let self_n = call.self_ as u32;
        let reg_count = (1 + self_n + call.args.len() as u32).max(target_count as u32);
        let regs = if target_top {
            self.alloc_reg(call.location, reg_count - target_count as u32) - target_count
        } else {
            self.alloc_reg(call.location, reg_count)
        };

        let mut selfreg = 0u8;
        let mut bfid = -1;
        if self.options.optimization_level >= 1 && !call.self_ {
            if let Some(&id) = self.builtins.get(&key(node)) {
                if id != lbf::NONE {
                    bfid = id;
                }
            }
        }

        if bfid >= 0 && self.bytecode.needs_debug_remarks() {
            let builtin = crate::analysis::get_builtin(&call.func, &self.globals, &self.variables);
            let last_mult = !call.args.is_empty() && self.is_expr_mult_ret(call.args.last().unwrap());
            let plus = if last_mult { "+" } else { "" };
            if let Some(obj) = builtin.object {
                self.bytecode.add_debug_remark(format!("builtin {}.{}/{}{}", obj, builtin.method.unwrap_or(""), call.args.len(), plus));
            } else if let Some(m) = builtin.method {
                self.bytecode.add_debug_remark(format!("builtin {}/{}{}", m, call.args.len(), plus));
            }
        }

        if bfid == lbf::SELECT_VARARG {
            if !mult_ret && target_count == 1 {
                self.compile_expr_select_vararg(call, target, target_count, target_top, mult_ret, regs);
                self.reg_top = old_top;
                return;
            }
            bfid = -1;
        }

        if bfid == lbf::BIT32_EXTRACT && call.args.len() == 3 && self.is_constant(&call.args[1]) && self.is_constant(&call.args[2]) {
            let fc = self.get_constant(&call.args[1]);
            let wc = self.get_constant(&call.args[2]);
            let fi = if let Constant::Number(n) = fc { n as i32 } else { -1 };
            let wi = if let Constant::Number(n) = wc { n as i32 } else { -1 };
            if fi >= 0 && wi > 0 && fi + wi <= 32 {
                let fwp = fi | ((wi - 1) << 5);
                let cid = self.bytecode.add_constant_number(fwp as f64);
                if cid < 0 {
                    const_limit(call.location);
                }
                self.compile_expr_fastcall_n(call, target, target_count, target_top, mult_ret, regs, lbf::BIT32_EXTRACTK, cid);
                self.reg_top = old_top;
                return;
            }
        }

        let mut max_fastcall_args = 2;
        if bfid >= 0 && call.args.len() == 3 {
            for a in &call.args {
                if self.get_expr_local_reg(a) >= 0 {
                    max_fastcall_args = 3;
                    break;
                }
            }
        }

        if bfid >= 0 && !call.args.is_empty() && call.args.len() <= max_fastcall_args {
            if !self.is_expr_mult_ret(call.args.last().unwrap()) {
                self.compile_expr_fastcall_n(call, target, target_count, target_top, mult_ret, regs, bfid, -1);
                self.reg_top = old_top;
                return;
            } else if self.options.optimization_level >= 2 {
                // none-safe builtins with matching arity behave the same
                // whether the last expression returns 0 or >1 values
                let (params, _, none_safe) = get_builtin_info(bfid);
                if call.args.len() as i32 == params && none_safe {
                    self.compile_expr_fastcall_n(call, target, target_count, target_top, mult_ret, regs, bfid, -1);
                    self.reg_top = old_top;
                    return;
                }
            }
        }

        let mut fast_pcall_id = -1;
        if self.options.optimization_level >= 1 && !call.self_ {
            if let AstExpr::Global(g) = &*call.func {
                if self.can_import(g) {
                    if g.name.0 == "pcall" && !call.args.is_empty() {
                        fast_pcall_id = 0;
                    } else if g.name.0 == "xpcall" && call.args.len() >= 2 {
                        fast_pcall_id = 1;
                    }
                }
            }
        }

        if call.self_ {
            let AstExpr::IndexName(fi) = &*call.func else { unreachable!() };
            let reg = self.get_expr_local_reg(&fi.expr);
            if reg >= 0 {
                selfreg = reg as u8;
            } else {
                selfreg = regs;
                self.compile_expr_temp_top(&fi.expr, selfreg);
            }
        } else if bfid < 0 && fast_pcall_id < 0 {
            self.compile_expr_temp_top(&call.func, regs);
        }

        let mut mult_call = false;
        let mut construct_call = false;
        if self.options.classes && !call.self_ && call.table_call && bfid < 0 && fast_pcall_id < 0 {
            let AstExpr::Table(t) = &*call.args[0] else { unreachable!("construct call takes a table") };
            let arg = regs + 1;
            let old = self.reg_top;
            self.reg_top = arg as u32 + 1;
            self.compile_expr_table(t, arg, true, regs);
            self.reg_top = old;
            construct_call = true;
        } else {
            for (i, a) in call.args.iter().enumerate() {
                let t = regs + 1 + self_n as u8 + i as u8;
                if i + 1 == call.args.len() {
                    mult_call = self.compile_expr_temp_mult_ret(a, t);
                } else {
                    self.compile_expr_temp_top(a, t);
                }
            }
        }

        self.set_debug_line_end(call.func.location());

        if call.self_ {
            let AstExpr::IndexName(fi) = &*call.func else { unreachable!() };
            self.set_debug_line(fi.index_location);
            if self.options.classes && fi.private_access {
                self.compile_private_field_access(fi, regs, selfreg, true);
            } else {
                let iname = fi.index.0.as_bytes();
                let cid = self.bytecode.add_constant_string(iname);
                if cid < 0 {
                    const_limit(fi.location);
                }
                self.bytecode.emit_abc(Opcode::Namecall, regs, selfreg, BytecodeBuilder::get_string_hash(iname) as u8);
                self.bytecode.emit_aux(cid as u32);
                self.hint_temporary_expr_reg_type(&fi.expr, selfreg, TABLE_T, 2);
            }
        } else if bfid >= 0 || fast_pcall_id >= 0 {
            let fastcall_label = self.bytecode.emit_label();
            if fast_pcall_id >= 0 {
                let explicit = call.args.len() as u8 - mult_call as u8;
                self.bytecode.emit_abc(Opcode::FastPCall, fast_pcall_id as u8, explicit, 0);
            } else {
                self.bytecode.emit_abc(Opcode::FastCall, bfid as u8, 0, 0);
            }
            self.compile_expr_temp(&call.func, regs);
            let call_label = self.bytecode.emit_label();
            if !self.bytecode.patch_skip_c(fastcall_label, call_label) {
                jump_limit(call.func.location());
            }
        }

        let jump_label = self.bytecode.emit_label();
        if construct_call {
            self.bytecode.emit_abc(Opcode::FinConstruct, regs + 1, 0, 0);
        }
        let call_label = self.bytecode.emit_label();

        let b = if mult_call { 0 } else { (self_n + call.args.len() as u32 + 1) as u8 };
        let c = if mult_ret { 0 } else { target_count + 1 };
        let can_inline = self.current_function.unwrap().depth != 0 && !mult_call && !mult_ret;
        if bfid < 0 && fast_pcall_id < 0 && can_inline && !construct_call {
            let slot = self.bytecode.add_call_target_slot(self.bytecode.get_instruction_count() as u32);
            self.bytecode.emit_abc(Opcode::CallFb, regs, b, c);
            self.bytecode.emit_aux(slot);
        } else {
            self.bytecode.emit_abc(Opcode::Call, regs, b, c);
        }
        if construct_call && !self.bytecode.patch_skip_c(jump_label, call_label) {
            jump_limit(call.location);
        }

        if !target_top {
            for i in 0..target_count {
                self.bytecode.emit_abc(Opcode::Move, target + i, regs + i, 0);
            }
        }
        self.reg_top = old_top;
    }

    fn should_share_closure(&self, func_key: usize, seen: &mut Vec<usize>) -> bool {
        let Some(f) = self.functions.get(&func_key) else { return false };
        for uv in &f.upvals {
            let Some(ul) = self.variables.get(&local_key(uv)) else { return false };
            if ul.written {
                return false;
            }
            if uv.function_depth != 0 || uv.loop_depth != 0 {
                let uf = match ul.init {
                    Some(Init::Func(f)) => Some(key(&**f)),
                    Some(Init::Expr(AstExpr::Function(f))) => Some(key(&**f)),
                    _ => None,
                };
                let Some(uf) = uf else { return false };
                if uf != func_key {
                    if seen.contains(&uf) {
                        return false;
                    }
                    seen.push(uf);
                    let ok = self.should_share_closure(uf, seen);
                    seen.pop();
                    if !ok {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn compile_expr_function(&mut self, f: &'a AstExprFunction, target: u8) {
        scoped!(self, {
            let fk = key(f);
            let info = self.functions.get(&fk).cloned().expect("function not compiled");
            let pid = self.bytecode.add_child_function(info.id);
            if pid < 0 {
                CompileError::raise(f.location, "Exceeded closure limit; simplify the code to compile");
            }

            let mut captures: Vec<(u8, u8)> = Vec::with_capacity(info.upvals.len());
            for uv in &info.upvals {
                let reg = self.get_local_reg(uv);
                if reg >= 0 {
                    let immutable = self.variables.get(&local_key(uv)).map_or(true, |v| !v.written);
                    captures.push((if immutable { 0 } else { 1 }, reg as u8));
                } else if let Some(uc) = self.locstants.get(&local_key(uv)).cloned().filter(|c| c.is_known()) {
                    let reg = self.alloc_reg(f.location, 1);
                    self.compile_expr_constant(f.location, &uc, reg);
                    captures.push((0, reg));
                } else {
                    let uid = self.get_upval(uv);
                    captures.push((2, uid));
                }
            }

            let mut shared: i16 = -1;
            if self.options.optimization_level >= 1 && !self.setfenv_used {
                let mut seen = vec![fk];
                if self.should_share_closure(fk, &mut seen) {
                    let cid = self.bytecode.add_constant_closure(info.id);
                    if (0..32768).contains(&cid) {
                        shared = cid as i16;
                    }
                }
            }

            if shared < 0 && self.bytecode.needs_debug_remarks() {
                self.bytecode.add_debug_remark(format!("allocation: closure with {} upvalues", captures.len()));
            }

            if shared >= 0 {
                self.bytecode.emit_ad(Opcode::DupClosure, target, shared);
            } else {
                self.bytecode.emit_ad(Opcode::NewClosure, target, pid);
            }
            for (ty, data) in captures {
                self.bytecode.emit_abc(Opcode::Capture, ty, data, 0);
            }
        })
    }

    // -----------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------

    fn is_constant(&self, node: &AstExpr) -> bool {
        self.constants.get(&key(node)).is_some_and(|c| c.is_known())
    }

    fn is_constant_true(&self, node: &AstExpr) -> bool {
        self.constants.get(&key(node)).is_some_and(|c| c.is_known() && c.is_truthful())
    }

    fn is_constant_false(&self, node: &AstExpr) -> bool {
        self.constants.get(&key(node)).is_some_and(|c| c.is_known() && !c.is_truthful())
    }

    fn get_constant(&self, node: &AstExpr) -> Constant {
        self.constants.get(&key(node)).cloned().unwrap_or(Constant::Unknown)
    }

    fn get_constant_number(&mut self, node: &AstExpr) -> i32 {
        if let Some(Constant::Number(n)) = self.constants.get(&key(node)).cloned() {
            let cid = self.bytecode.add_constant_number(n);
            if cid < 0 {
                const_limit(node.location());
            }
            return cid;
        }
        -1
    }

    fn get_constant_index(&mut self, node: &AstExpr) -> i32 {
        let c = self.get_constant(node);
        let cid = match c {
            Constant::Unknown | Constant::Table(_) => return -1,
            Constant::Nil => self.bytecode.add_constant_nil(),
            Constant::Boolean(b) => self.bytecode.add_constant_boolean(b),
            Constant::Number(n) => self.bytecode.add_constant_number(n),
            Constant::Integer(i) => self.bytecode.add_constant_integer(i),
            Constant::Vectorf(v) => self.bytecode.add_constant_vectorf(v[0], v[1], v[2], v[3]),
            Constant::Vectord(v) => self.bytecode.add_constant_vectord(v[0], v[1], v[2], v[3]),
            Constant::String(s) => self.bytecode.add_constant_string(&s),
        };
        if cid < 0 {
            const_limit(node.location());
        }
        cid
    }

    // -----------------------------------------------------------------
    // Conditions
    // -----------------------------------------------------------------

    fn compile_compare_jump(&mut self, expr: &'a AstExprBinary, not: bool) -> usize {
        scoped!(self, {
            use AstExprBinaryOp as B;
            let is_eq = matches!(expr.op, B::CompareEq | B::CompareNe);
            let mut left: &'a AstExpr = &expr.left;
            let mut right: &'a AstExpr = &expr.right;

            let mut operand_is_constant = self.is_constant(right);
            if is_eq && !operand_is_constant {
                operand_is_constant = self.is_constant(left);
                if operand_is_constant {
                    std::mem::swap(&mut left, &mut right);
                }
            }
            if operand_is_constant
                && matches!(self.get_constant(right), Constant::Vectorf(_) | Constant::Vectord(_) | Constant::Integer(_))
            {
                operand_is_constant = false;
            }

            let rl = self.compile_expr_auto(left);

            if is_eq && operand_is_constant {
                let cv = self.get_constant(right);
                let flip: u32 = if (expr.op == B::CompareEq) == not { 0x8000_0000 } else { 0 };
                let (opc, cid) = match cv {
                    Constant::Nil => (Opcode::JumpXEqKNil, 0),
                    Constant::Boolean(b) => (Opcode::JumpXEqKB, b as i32),
                    Constant::Number(_) => (Opcode::JumpXEqKN, self.get_constant_index(right)),
                    Constant::String(_) => (Opcode::JumpXEqKS, self.get_constant_index(right)),
                    _ => unreachable!("unexpected constant type"),
                };
                if cid < 0 {
                    const_limit(expr.location);
                }
                let label = self.bytecode.emit_label();
                self.bytecode.emit_ad(opc, rl, 0);
                self.bytecode.emit_aux(cid as u32 | flip);
                label
            } else {
                let opc = jump_op_compare(expr.op, not);
                let rr = self.compile_expr_auto(right);
                let label = self.bytecode.emit_label();
                if matches!(expr.op, B::CompareGt | B::CompareGe) {
                    self.bytecode.emit_ad(opc, rr, 0);
                    self.bytecode.emit_aux(rl as u32);
                } else {
                    self.bytecode.emit_ad(opc, rl, 0);
                    self.bytecode.emit_aux(rr as u32);
                }
                label
            }
        })
    }

    fn compile_condition_value(&mut self, node: &'a AstExpr, target: Option<u8>, skip_jump: &mut Vec<usize>, only_truth: bool) {
        use AstExprBinaryOp as B;
        if let Some(cv) = self.constants.get(&key(node)).cloned().filter(|c| c.is_known()) {
            if cv.is_truthful() == only_truth {
                if let Some(t) = target {
                    self.compile_expr_temp(node, t);
                }
                skip_jump.push(self.bytecode.emit_label());
                self.bytecode.emit_ad(Opcode::Jump, 0, 0);
            }
            return;
        }

        if let AstExpr::Binary(expr) = node {
            match expr.op {
                B::And | B::Or => {
                    if only_truth == (expr.op == B::And) {
                        let mut else_jump = Vec::new();
                        self.compile_condition_value(&expr.left, None, &mut else_jump, !only_truth);
                        self.compile_condition_value(&expr.right, target, skip_jump, only_truth);
                        let else_label = self.bytecode.emit_label();
                        self.patch_jumps(expr.location, &else_jump, else_label);
                    } else {
                        self.compile_condition_value(&expr.left, target, skip_jump, only_truth);
                        self.compile_condition_value(&expr.right, target, skip_jump, only_truth);
                    }
                    return;
                }
                B::CompareNe | B::CompareEq | B::CompareLt | B::CompareLe | B::CompareGt | B::CompareGe => {
                    if let Some(t) = target {
                        self.bytecode.emit_abc(Opcode::LoadB, t, only_truth as u8, 0);
                    }
                    let label = self.compile_compare_jump(expr, !only_truth);
                    skip_jump.push(label);
                    return;
                }
                _ => {}
            }
        }

        if let AstExpr::Unary(u) = node {
            if target.is_none() && u.op == AstExprUnaryOp::Not {
                self.compile_condition_value(&u.expr, target, skip_jump, !only_truth);
                return;
            }
        }

        if let AstExpr::Group(g) = node {
            self.compile_condition_value(&g.expr, target, skip_jump, only_truth);
            return;
        }

        scoped!(self, {
            let reg = match target {
                Some(t) => {
                    self.compile_expr_temp(node, t);
                    t
                }
                None => self.compile_expr_auto(node),
            };
            skip_jump.push(self.bytecode.emit_label());
            self.bytecode.emit_ad(if only_truth { Opcode::JumpIf } else { Opcode::JumpIfNot }, reg, 0);
        })
    }

    fn is_condition_fast(&self, node: &AstExpr) -> bool {
        use AstExprBinaryOp as B;
        if self.is_constant(node) {
            return true;
        }
        match node {
            AstExpr::Binary(b) => matches!(
                b.op,
                B::And | B::Or | B::CompareNe | B::CompareEq | B::CompareLt | B::CompareLe | B::CompareGt | B::CompareGe
            ),
            AstExpr::Group(g) => self.is_condition_fast(&g.expr),
            _ => false,
        }
    }

    fn compile_expr_and_or(&mut self, expr: &'a AstExprBinary, target: u8, target_temp: bool) {
        let and = expr.op == AstExprBinaryOp::And;
        scoped!(self, {
            if let Some(cl) = self.constants.get(&key(&*expr.left)).cloned().filter(|c| c.is_known()) {
                let e: &'a AstExpr = if and == cl.is_truthful() { &expr.right } else { &expr.left };
                self.compile_expr(e, target, target_temp);
                return;
            }

            if !self.is_condition_fast(&expr.left) {
                let reg = self.get_expr_local_reg(&expr.right);
                if reg >= 0 {
                    let lr = self.compile_expr_auto(&expr.left);
                    self.bytecode.emit_abc(if and { Opcode::And } else { Opcode::Or }, target, lr, reg as u8);
                    return;
                }
                let cid = self.get_constant_index(&expr.right);
                if (0..=255).contains(&cid) {
                    let lr = self.compile_expr_auto(&expr.left);
                    self.bytecode.emit_abc(if and { Opcode::AndK } else { Opcode::OrK }, target, lr, cid as u8);
                    return;
                }
            }

            let reg = if target_temp { target } else { self.alloc_reg(expr.location, 1) };
            let mut skip_jump = Vec::new();
            self.compile_condition_value(&expr.left, Some(reg), &mut skip_jump, !and);
            self.compile_expr_temp(&expr.right, reg);
            let move_label = self.bytecode.emit_label();
            self.patch_jumps(expr.location, &skip_jump, move_label);
            if target != reg {
                self.bytecode.emit_abc(Opcode::Move, target, reg, 0);
            }
        })
    }

    fn compile_expr_unary(&mut self, expr: &'a AstExprUnary, target: u8) {
        scoped!(self, {
            if expr.op == AstExprUnaryOp::Minus {
                if let AstExpr::ConstantInteger(ci) = &*expr.expr {
                    let cid = self.bytecode.add_constant_integer((!(ci.value as u64)).wrapping_add(1) as i64);
                    if cid < 0 {
                        const_limit(expr.location);
                    }
                    self.emit_load_k(target, cid);
                    return;
                }
            }
            let re = self.compile_expr_auto(&expr.expr);
            let op = match expr.op {
                AstExprUnaryOp::Not => Opcode::Not,
                AstExprUnaryOp::Minus => Opcode::Minus,
                AstExprUnaryOp::Len => Opcode::Length,
            };
            self.bytecode.emit_abc(op, target, re, 0);
        })
    }

    fn compile_expr_binary(&mut self, node: &AstExpr, expr: &'a AstExprBinary, target: u8, target_temp: bool) {
        use AstExprBinaryOp as B;
        scoped!(self, {
            match expr.op {
                B::Add | B::Sub | B::Mul | B::Div | B::FloorDiv | B::Mod | B::Pow => {
                    let rc = self.get_constant_number(&expr.right);
                    if (0..=255).contains(&rc) {
                        let rl = self.compile_expr_auto(&expr.left);
                        self.bytecode.emit_abc(arith_op(expr.op, true), target, rl, rc as u8);
                        self.hint_temporary_expr_reg_type(&expr.left, rl, NUMBER_T, 1);
                    } else {
                        if matches!(expr.op, B::Sub | B::Div) {
                            let lc = self.get_constant_number(&expr.left);
                            if (0..=255).contains(&lc) {
                                let rr = self.compile_expr_auto(&expr.right);
                                let op = if expr.op == B::Sub { Opcode::SubRk } else { Opcode::DivRk };
                                self.bytecode.emit_abc(op, target, lc as u8, rr);
                                self.hint_temporary_expr_reg_type(&expr.right, rr, NUMBER_T, 1);
                                return;
                            }
                        } else if self.options.optimization_level >= 2 && matches!(expr.op, B::Add | B::Mul) {
                            // replace k*r with r*k when r is known to be a number
                            if let Some(ty) = self.expr_types.get(&key(node)).map(|x| x.bty) {
                                if ty == NUMBER_T || (ty == crate::types::lbc_type::VECTOR && expr.op == B::Mul) {
                                    let lc = self.get_constant_number(&expr.left);
                                    if (0..=255).contains(&lc) {
                                        let rr = self.compile_expr_auto(&expr.right);
                                        self.bytecode.emit_abc(arith_op(expr.op, true), target, rr, lc as u8);
                                        self.hint_temporary_expr_reg_type(&expr.right, rr, NUMBER_T, 1);
                                        return;
                                    }
                                }
                            }
                        }
                        let rl = self.compile_expr_auto(&expr.left);
                        let rr = self.compile_expr_auto(&expr.right);
                        self.bytecode.emit_abc(arith_op(expr.op, false), target, rl, rr);
                        self.hint_temporary_expr_reg_type(&expr.left, rl, NUMBER_T, 1);
                        self.hint_temporary_expr_reg_type(&expr.right, rr, NUMBER_T, 1);
                    }
                }
                B::Concat => {
                    let mut args: Vec<&'a AstExpr> = vec![&expr.left, &expr.right];
                    unroll_concats(&mut args);
                    let regs = self.alloc_reg(expr.location, args.len() as u32);
                    for (i, a) in args.iter().enumerate() {
                        self.compile_expr_temp_top(a, regs + i as u8);
                    }
                    self.bytecode.emit_abc(Opcode::Concat, target, regs, regs + args.len() as u8 - 1);
                }
                B::CompareNe | B::CompareEq | B::CompareLt | B::CompareLe | B::CompareGt | B::CompareGe => {
                    let jump_label = self.compile_compare_jump(expr, false);
                    self.bytecode.emit_abc(Opcode::LoadB, target, 0, 1);
                    let then_label = self.bytecode.emit_label();
                    self.bytecode.emit_abc(Opcode::LoadB, target, 1, 0);
                    self.patch_jump(expr.location, jump_label, then_label);
                }
                B::And | B::Or => self.compile_expr_and_or(expr, target, target_temp),
            }
        })
    }

    fn compile_expr_if_else_and_or(&mut self, and: bool, creg: u8, other: &'a AstExpr, target: u8) {
        let cid = self.get_constant_index(other);
        if (0..=255).contains(&cid) {
            self.bytecode.emit_abc(if and { Opcode::AndK } else { Opcode::OrK }, target, creg, cid as u8);
        } else {
            scoped!(self, {
                let oreg = self.compile_expr_auto(other);
                self.bytecode.emit_abc(if and { Opcode::And } else { Opcode::Or }, target, creg, oreg);
            })
        }
    }

    fn compile_expr_if_else_local(&mut self, expr: &'a AstExprIfElse, target: u8, target_temp: bool) {
        let cl = expr.condition_local.as_ref().unwrap();
        let skip_else = self.is_constant_true(&expr.condition);
        let old_locals = self.local_stack.len();
        let mut else_jump = 0;
        scoped!(self, {
            let reg = self.alloc_reg(expr.location, 1);
            let allocpc = self.bytecode.get_debug_pc();
            self.compile_expr_temp(&expr.condition, reg);
            self.push_local(cl, reg, allocpc);
            if !skip_else {
                else_jump = self.bytecode.emit_label();
                self.bytecode.emit_ad(Opcode::JumpIfNot, reg, 0);
            }
            self.compile_expr(&expr.true_expr, target, target_temp);
            self.close_locals(old_locals);
            self.pop_locals(old_locals);
        });
        if skip_else {
            return;
        }
        let then_label = self.bytecode.emit_label();
        self.bytecode.emit_ad(Opcode::Jump, 0, 0);
        let else_label = self.bytecode.emit_label();
        self.compile_expr(&expr.false_expr, target, target_temp);
        let end_label = self.bytecode.emit_label();
        self.patch_jump(expr.location, else_jump, else_label);
        self.patch_jump(expr.location, then_label, end_label);
    }

    fn compile_expr_if_else(&mut self, expr: &'a AstExprIfElse, target: u8, target_temp: bool) {
        if expr.condition_local.is_some() {
            self.compile_expr_if_else_local(expr, target, target_temp);
            return;
        }
        if self.is_constant(&expr.condition) {
            if self.is_constant_true(&expr.condition) {
                self.compile_expr(&expr.true_expr, target, target_temp);
            } else {
                self.compile_expr(&expr.false_expr, target, target_temp);
            }
            return;
        }
        let creg = self.get_expr_local_reg(&expr.condition);
        if creg >= 0 {
            if creg == self.get_expr_local_reg(&expr.true_expr)
                && (self.get_expr_local_reg(&expr.false_expr) >= 0 || self.is_constant(&expr.false_expr))
            {
                return self.compile_expr_if_else_and_or(false, creg as u8, &expr.false_expr, target);
            } else if creg == self.get_expr_local_reg(&expr.false_expr)
                && (self.get_expr_local_reg(&expr.true_expr) >= 0 || self.is_constant(&expr.true_expr))
            {
                return self.compile_expr_if_else_and_or(true, creg as u8, &expr.true_expr, target);
            }
        }
        let mut else_jump = Vec::new();
        self.compile_condition_value(&expr.condition, None, &mut else_jump, false);
        self.compile_expr(&expr.true_expr, target, target_temp);
        let then_label = self.bytecode.emit_label();
        self.bytecode.emit_ad(Opcode::Jump, 0, 0);
        let else_label = self.bytecode.emit_label();
        self.compile_expr(&expr.false_expr, target, target_temp);
        let end_label = self.bytecode.emit_label();
        self.patch_jumps(expr.location, &else_jump, else_label);
        self.patch_jump(expr.location, then_label, end_label);
    }

    fn compile_expr_interp_string(&mut self, expr: &'a AstExprInterpString, target: u8, target_temp: bool) {
        let mut format = Vec::new();
        let mut skipped_sub = 0usize;
        for (idx, s) in expr.strings.iter().enumerate() {
            escape_and_append(&mut format, s);
            if let Some(e) = expr.expressions.get(idx) {
                if let Some(Constant::String(c)) = self.constants.get(&key(&**e)).cloned() {
                    escape_and_append(&mut format, &c);
                    skipped_sub += 1;
                } else {
                    format.extend_from_slice(b"%*");
                }
            }
        }
        let format_index = self.bytecode.add_constant_string(&format);
        if format_index < 0 {
            const_limit(expr.location);
        }

        scoped!(self, {
            let reg_count = (2 + expr.expressions.len() - skipped_sub) as u32;
            let target_top = target_temp && target as u32 == self.reg_top - 1;
            let base_reg = if target_top {
                self.alloc_reg(expr.location, reg_count - 1) - 1
            } else {
                self.alloc_reg(expr.location, reg_count)
            };
            self.emit_load_k(base_reg, format_index);
            let mut skipped = 0;
            for (index, sub) in expr.expressions.iter().enumerate() {
                if matches!(self.constants.get(&key(&**sub)), Some(Constant::String(_))) {
                    skipped += 1;
                } else {
                    self.compile_expr_temp_top(sub, base_reg + 2 + (index - skipped) as u8);
                }
            }
            let method_index = self.bytecode.add_constant_string(b"format");
            if method_index < 0 {
                const_limit(expr.location);
            }
            self.bytecode.emit_abc(Opcode::Namecall, base_reg, base_reg, BytecodeBuilder::get_string_hash(b"format") as u8);
            self.bytecode.emit_aux(method_index as u32);
            self.bytecode.emit_abc(Opcode::Call, base_reg, (expr.expressions.len() + 2 - skipped_sub) as u8, 2);
            if target != base_reg {
                self.bytecode.emit_abc(Opcode::Move, target, base_reg, 0);
            }
        })
    }

    fn compile_expr_table(&mut self, expr: &'a AstExprTable, target: u8, target_temp: bool, construct_reg: u8) {
        if expr.items.is_empty() {
            let shape = self.table_shapes.get(&key(expr)).copied().unwrap_or_default();
            if self.bytecode.needs_debug_remarks() {
                self.bytecode.add_debug_remark(format!("allocation: table hash {}", shape.hash_size));
            }
            self.bytecode.emit_abc(Opcode::NewTable, target, encode_hash_size(shape.hash_size), 0);
            self.bytecode.emit_aux(shape.array_size);
            return;
        }

        let mut array_size = 0u32;
        let mut hash_size = 0u32;
        let mut record_size = 0u32;
        let mut index_size = 0u32;
        for item in &expr.items {
            array_size += (item.kind == AstExprTableItemKind::List) as u32;
            hash_size += (item.kind != AstExprTableItemKind::List) as u32;
            record_size += (item.kind == AstExprTableItemKind::Record) as u32;
        }

        if array_size == 0 && hash_size > 0 {
            for item in &expr.items {
                let k = item.key.as_ref().unwrap();
                if let Some(Constant::Number(n)) = self.constants.get(&key(&**k)) {
                    if *n == (index_size + 1) as f64 {
                        index_size += 1;
                    }
                }
            }
            if hash_size == record_size + index_size {
                hash_size = record_size;
            } else {
                index_size = 0;
            }
        }

        let encoded_hash_size = encode_hash_size(hash_size);

        scoped!(self, {
            let reg = if target_temp { target } else { self.alloc_reg(expr.location, 1) };

            // insertion-ordered key -> value constant
            let mut last_key_val: Vec<(i32, i32)> = Vec::new();
            if array_size == 0
                && index_size == 0
                && hash_size == record_size
                && record_size >= 1
                && record_size as usize <= TableShape::MAX_LENGTH
            {
                for item in &expr.items {
                    let Some(k) = &item.key else { unreachable!() };
                    let AstExpr::ConstantString(ck) = &**k else { unreachable!() };
                    let key_cid = self.bytecode.add_constant_string(&ck.value);
                    if key_cid < 0 {
                        const_limit(ck.location);
                    }
                    let value_cid = self.get_constant_index(&item.value);
                    match last_key_val.iter_mut().find(|(k, _)| *k == key_cid) {
                        Some((_, v)) => {
                            if *v == -1 {
                                continue;
                            }
                            *v = value_cid;
                        }
                        None => last_key_val.push((key_cid, value_cid)),
                    }
                }
                let mut shape = TableShape::default();
                for &(k, v) in &last_key_val {
                    shape.keys.push(k);
                    shape.constants.push(v);
                    if v >= 0 {
                        shape.has_constants = true;
                    }
                }
                let tid = self.bytecode.add_constant_table(shape);
                if tid < 0 {
                    const_limit(expr.location);
                }
                if self.bytecode.needs_debug_remarks() {
                    self.bytecode.add_debug_remark(format!("allocation: table template {hash_size}"));
                }
                if self.options.classes && construct_reg != INVALID_REG {
                    let slot = self.bytecode.add_construct_slot(tid as u32);
                    self.bytecode.emit_abc(Opcode::Construct, reg, construct_reg, 0);
                    self.bytecode.emit_aux(slot);
                } else if tid < 32768 {
                    self.bytecode.emit_ad(Opcode::DupTable, reg, tid as i16);
                } else {
                    last_key_val.clear();
                    self.bytecode.emit_abc(Opcode::NewTable, reg, encoded_hash_size, 0);
                    self.bytecode.emit_aux(0);
                }
            } else {
                let trailing_varargs = expr
                    .items
                    .last()
                    .is_some_and(|l| l.kind == AstExprTableItemKind::List && matches!(&*l.value, AstExpr::Varargs(_)));
                let array_allocation = array_size - trailing_varargs as u32 + index_size;
                if self.bytecode.needs_debug_remarks() {
                    self.bytecode.add_debug_remark(if hash_size == 0 {
                        format!("allocation: table array {array_allocation}")
                    } else if array_allocation == 0 {
                        format!("allocation: table hash {hash_size}")
                    } else {
                        format!("allocation: table hash {hash_size} array {array_allocation}")
                    });
                }
                self.bytecode.emit_abc(Opcode::NewTable, reg, encoded_hash_size, 0);
                self.bytecode.emit_aux(array_allocation);
            }

            let chunk_size = 16u32.min(array_size);
            let chunk_reg = self.alloc_reg(expr.location, chunk_size);
            let mut chunk_current = 0u32;
            let mut array_index = 1u32;
            let mut mult_ret = false;

            for (i, item) in expr.items.iter().enumerate() {
                let k = item.key.as_deref();
                let value: &'a AstExpr = &item.value;

                if !last_key_val.is_empty() {
                    if let Some(AstExpr::ConstantString(ck)) = k {
                        let key_cid = self.bytecode.add_constant_string(&ck.value);
                        if let Some(&(_, v)) = last_key_val.iter().find(|(kk, _)| *kk == key_cid) {
                            if v >= 0 {
                                continue;
                            }
                        }
                    }
                }

                self.set_debug_line(value.location());
                if self.options.coverage_level >= 2 {
                    self.bytecode.emit_abc(Opcode::Coverage, 0, 0, 0);
                }

                if chunk_current > 0 && (k.is_some() || chunk_current == chunk_size) {
                    self.bytecode.emit_abc(Opcode::SetList, reg, chunk_reg, (chunk_current + 1) as u8);
                    self.bytecode.emit_aux(array_index);
                    array_index += chunk_current;
                    chunk_current = 0;
                }

                if let Some(k) = item.key.as_deref() {
                    scoped!(self, {
                        let lv = self.compile_lvalue_index(reg, k);
                        let rv = self.compile_expr_auto(value);
                        self.compile_assign(&lv, rv, None);
                    });
                } else {
                    let temp = chunk_reg + chunk_current as u8;
                    if i + 1 == expr.items.len() {
                        mult_ret = self.compile_expr_temp_mult_ret(value, temp);
                    } else {
                        self.compile_expr_temp_top(value, temp);
                    }
                    chunk_current += 1;
                }
            }

            if chunk_current > 0 {
                self.bytecode.emit_abc(Opcode::SetList, reg, chunk_reg, if mult_ret { 0 } else { (chunk_current + 1) as u8 });
                self.bytecode.emit_aux(array_index);
            }

            if target != reg {
                self.bytecode.emit_abc(Opcode::Move, target, reg, 0);
            }
        })
    }

    fn can_import(&self, e: &AstExprGlobal) -> bool {
        self.options.optimization_level >= 1 && global_state(&self.globals, e.name.0) != Global::Written
    }

    fn can_import_chain(&self, e: &AstExprGlobal) -> bool {
        self.options.optimization_level >= 1 && global_state(&self.globals, e.name.0) == Global::Default
    }

    fn compile_expr_index_name(&mut self, expr: &'a AstExprIndexName, target: u8, target_temp: bool) {
        self.set_debug_line(expr.location);

        let (import_root, import1, import2): (Option<&AstExprGlobal>, &AstExprIndexName, Option<&AstExprIndexName>) =
            match &*expr.expr {
                AstExpr::IndexName(index) => (
                    match &*index.expr {
                        AstExpr::Global(g) => Some(g),
                        _ => None,
                    },
                    index,
                    Some(expr),
                ),
                AstExpr::Global(g) => (Some(g), expr, None),
                _ => (None, expr, None),
            };

        if let Some(root) = import_root {
            if self.can_import_chain(root) {
                if self.options.classes && (import1.private_access || import2.is_some_and(|i| i.private_access)) {
                    CompileError::raise(expr.location, "Attempt to access private field of a global");
                }
                let id0 = self.bytecode.add_constant_string(root.name.0.as_bytes());
                let id1 = self.bytecode.add_constant_string(import1.index.0.as_bytes());
                let id2 = match import2 {
                    Some(i2) => self.bytecode.add_constant_string(i2.index.0.as_bytes()),
                    None => -1,
                };
                if id0 < 0 || id1 < 0 || (import2.is_some() && id2 < 0) {
                    const_limit(expr.location);
                }
                if id0 < 1024 && id1 < 1024 && id2 < 1024 {
                    let iid = if import2.is_some() {
                        BytecodeBuilder::get_import_id3(id0, id1, id2)
                    } else {
                        BytecodeBuilder::get_import_id2(id0, id1)
                    };
                    let cid = self.bytecode.add_import(iid);
                    if (0..32768).contains(&cid) {
                        self.bytecode.emit_ad(Opcode::GetImport, target, cid as i16);
                        self.bytecode.emit_aux(iid);
                        return;
                    }
                }
            }
        }

        scoped!(self, {
            let local_reg = self.get_expr_local_reg(&expr.expr);
            let reg = if local_reg >= 0 {
                local_reg as u8
            } else if target_temp {
                self.compile_expr_auto_temp(&expr.expr, target, INVALID_REG)
            } else {
                self.compile_expr_auto(&expr.expr)
            };
            self.set_debug_line(expr.index_location);
            if self.options.classes && expr.private_access {
                self.compile_private_field_access(expr, target, reg, false);
                return;
            }
            let iname = expr.index.0.as_bytes();
            let cid = self.bytecode.add_constant_string(iname);
            if cid < 0 {
                const_limit(expr.location);
            }
            self.bytecode.emit_abc(Opcode::GetTableKs, target, reg, BytecodeBuilder::get_string_hash(iname) as u8);
            self.bytecode.emit_aux(cid as u32);
            self.hint_temporary_expr_reg_type(&expr.expr, reg, TABLE_T, 2);
        })
    }

    fn compile_expr_index_expr(&mut self, expr: &'a AstExprIndexExpr, target: u8) {
        scoped!(self, {
            let cv = self.get_constant(&expr.index);
            match cv {
                Constant::Number(n) if (1.0..=256.0).contains(&n) && (n as i32) as f64 == n => {
                    let i = (n as i32 - 1) as u8;
                    let rt = self.compile_expr_auto(&expr.expr);
                    self.set_debug_line(expr.index.location());
                    self.bytecode.emit_abc(Opcode::GetTableN, target, rt, i);
                    self.hint_temporary_expr_reg_type(&expr.expr, rt, TABLE_T, 1);
                }
                Constant::String(s) => {
                    let cid = self.bytecode.add_constant_string(&s);
                    if cid < 0 {
                        const_limit(expr.location);
                    }
                    let rt = self.compile_expr_auto(&expr.expr);
                    self.set_debug_line(expr.index.location());
                    self.bytecode.emit_abc(Opcode::GetTableKs, target, rt, BytecodeBuilder::get_string_hash(&s) as u8);
                    self.bytecode.emit_aux(cid as u32);
                    self.hint_temporary_expr_reg_type(&expr.expr, rt, TABLE_T, 2);
                }
                _ => {
                    let rt = self.compile_expr_auto(&expr.expr);
                    let ri = self.compile_expr_auto(&expr.index);
                    self.bytecode.emit_abc(Opcode::GetTable, target, rt, ri);
                    self.hint_temporary_expr_reg_type(&expr.expr, rt, TABLE_T, 1);
                    self.hint_temporary_expr_reg_type(&expr.index, ri, NUMBER_T, 1);
                }
            }
        })
    }

    fn compile_expr_global(&mut self, expr: &AstExprGlobal, target: u8) {
        if self.can_import(expr) {
            let id0 = self.bytecode.add_constant_string(expr.name.0.as_bytes());
            if id0 < 0 {
                const_limit(expr.location);
            }
            if id0 < 1024 {
                let iid = BytecodeBuilder::get_import_id1(id0);
                let cid = self.bytecode.add_import(iid);
                if (0..32768).contains(&cid) {
                    self.bytecode.emit_ad(Opcode::GetImport, target, cid as i16);
                    self.bytecode.emit_aux(iid);
                    return;
                }
            }
        }
        let gname = expr.name.0.as_bytes();
        let cid = self.bytecode.add_constant_string(gname);
        if cid < 0 {
            const_limit(expr.location);
        }
        self.bytecode.emit_abc(Opcode::GetGlobal, target, 0, BytecodeBuilder::get_string_hash(gname) as u8);
        self.bytecode.emit_aux(cid as u32);
    }

    fn compile_expr_constant(&mut self, loc: Location, cv: &Constant, target: u8) {
        match cv {
            Constant::Nil => self.bytecode.emit_abc(Opcode::LoadNil, target, 0, 0),
            Constant::Boolean(b) => self.bytecode.emit_abc(Opcode::LoadB, target, *b as u8, 0),
            Constant::Number(d) => {
                let d = *d;
                if d >= i16::MIN as f64 && d <= i16::MAX as f64 && (d as i16) as f64 == d && !(d == 0.0 && d.is_sign_negative()) {
                    self.bytecode.emit_ad(Opcode::LoadN, target, d as i16);
                } else {
                    let cid = self.bytecode.add_constant_number(d);
                    if cid < 0 {
                        const_limit(loc);
                    }
                    self.emit_load_k(target, cid);
                }
            }
            Constant::Integer(i) => {
                let cid = self.bytecode.add_constant_integer(*i);
                if cid < 0 {
                    const_limit(loc);
                }
                self.emit_load_k(target, cid);
            }
            Constant::Vectorf(v) => {
                let cid = self.bytecode.add_constant_vectorf(v[0], v[1], v[2], v[3]);
                if cid < 0 {
                    const_limit(loc);
                }
                self.emit_load_k(target, cid);
            }
            Constant::Vectord(v) => {
                let cid = self.bytecode.add_constant_vectord(v[0], v[1], v[2], v[3]);
                if cid < 0 {
                    const_limit(loc);
                }
                self.emit_load_k(target, cid);
            }
            Constant::String(s) => {
                let cid = self.bytecode.add_constant_string(s);
                if cid < 0 {
                    const_limit(loc);
                }
                self.emit_load_k(target, cid);
            }
            Constant::Unknown | Constant::Table(_) => unreachable!("unexpected constant type"),
        }
    }

    fn compile_expr(&mut self, node: &'a AstExpr, target: u8, target_temp: bool) {
        self.set_debug_line(node.location());

        if self.options.coverage_level >= 2 {
            self.bytecode.emit_abc(Opcode::Coverage, 0, 0, 0);
        }

        if let Some(cv) = self.constants.get(&key(node)).cloned().filter(|c| c.is_known()) {
            self.compile_expr_constant(node.location(), &cv, target);
            return;
        }

        match node {
            AstExpr::Group(g) => self.compile_expr(&g.expr, target, target_temp),
            AstExpr::ConstantNil(_) => self.bytecode.emit_abc(Opcode::LoadNil, target, 0, 0),
            AstExpr::ConstantBool(b) => self.bytecode.emit_abc(Opcode::LoadB, target, b.value as u8, 0),
            AstExpr::ConstantNumber(n) => {
                let cid = self.bytecode.add_constant_number(n.value);
                if cid < 0 {
                    const_limit(n.location);
                }
                self.emit_load_k(target, cid);
            }
            AstExpr::ConstantInteger(n) => {
                let cid = self.bytecode.add_constant_integer(n.value);
                if cid < 0 {
                    const_limit(n.location);
                }
                self.emit_load_k(target, cid);
            }
            AstExpr::ConstantString(s) => {
                let cid = self.bytecode.add_constant_string(&s.value);
                if cid < 0 {
                    const_limit(s.location);
                }
                self.emit_load_k(target, cid);
            }
            AstExpr::Local(e) => {
                if e.local.is_exported
                    && !self.exports.exported_classes.iter().any(|(l, _)| Arc::ptr_eq(l, &e.local))
                    && !self.exports.exported_functions.contains(&local_key(&e.local))
                {
                    let name = e.local.name.0.as_bytes();
                    let cid = self.bytecode.add_constant_string(name);
                    if cid < 0 {
                        const_limit(e.location);
                    }
                    let tl = self.exports.table_local.clone();
                    let table_reg = self.get_local_reg(&tl);
                    if table_reg >= 0 {
                        self.bytecode.emit_abc(Opcode::GetTableKs, target, table_reg as u8, BytecodeBuilder::get_string_hash(name) as u8);
                        self.bytecode.emit_aux(cid as u32);
                    } else {
                        let upval = self.get_upval(&tl);
                        self.bytecode.emit_abc(Opcode::GetUpval, target, upval, 0);
                        self.bytecode.emit_abc(Opcode::GetTableKs, target, target, BytecodeBuilder::get_string_hash(name) as u8);
                        self.bytecode.emit_aux(cid as u32);
                    }
                } else {
                    let reg = self.get_expr_local_reg(node);
                    if reg >= 0 {
                        if self.options.optimization_level == 0 || target as i32 != reg {
                            self.bytecode.emit_abc(Opcode::Move, target, reg as u8, 0);
                        }
                    } else {
                        let uid = self.get_upval(&e.local);
                        self.bytecode.emit_abc(Opcode::GetUpval, target, uid, 0);
                    }
                }
            }
            AstExpr::Global(g) => self.compile_expr_global(g, target),
            AstExpr::Varargs(v) => self.compile_expr_varargs(v.location, target, 1, false),
            AstExpr::Call(call) => {
                if target_temp && target as u32 == self.reg_top - 1 {
                    self.compile_expr_call(node, call, target, 1, true, false);
                } else {
                    self.compile_expr_call(node, call, target, 1, false, false);
                }
            }
            AstExpr::IndexName(e) => self.compile_expr_index_name(e, target, target_temp),
            AstExpr::IndexExpr(e) => self.compile_expr_index_expr(e, target),
            AstExpr::Function(f) => self.compile_expr_function(f, target),
            AstExpr::Table(t) => self.compile_expr_table(t, target, target_temp, INVALID_REG),
            AstExpr::Unary(u) => self.compile_expr_unary(u, target),
            AstExpr::Binary(b) => self.compile_expr_binary(node, b, target, target_temp),
            AstExpr::TypeAssertion(t) => self.compile_expr(&t.expr, target, target_temp),
            AstExpr::IfElse(ie) => self.compile_expr_if_else(ie, target, target_temp),
            AstExpr::InterpString(s) => self.compile_expr_interp_string(s, target, target_temp),
            AstExpr::Instantiate(i) => self.compile_expr(&i.expr, target, target_temp),
            AstExpr::Error(e) => CompileError::raise(e.location, "Unexpected error expression"),
        }
    }

    fn compile_expr_temp(&mut self, node: &'a AstExpr, target: u8) {
        self.compile_expr(node, target, true)
    }

    /// Compiles into `target`, but if that produced a single `MOVE` from an
    /// uncaptured register, undoes it and returns the source register.
    fn compile_expr_auto_temp(&mut self, node: &'a AstExpr, target: u8, forbidden: u8) -> u8 {
        if self.options.optimization_level == 0 {
            self.compile_expr_temp(node, target);
            return target;
        }
        let marker = self.bytecode.emit_label();
        self.compile_expr_temp(node, target);
        if self.bytecode.emit_label() == marker + 1 {
            let i = self.bytecode.last_instruction();
            if insn::op(i) == Opcode::Move as u32 && insn::a(i) == target as u32 {
                let src = insn::b(i) as u8;
                if !self.reg_captured[src as usize] && src != forbidden {
                    self.bytecode.undo_emit(Opcode::Move);
                    return src;
                }
            }
        }
        target
    }

    /// `compileExprAuto`: the register is owned by the caller's scope.
    fn compile_expr_auto(&mut self, node: &'a AstExpr) -> u8 {
        let reg = self.get_expr_local_reg(node);
        if reg >= 0 {
            return reg as u8;
        }
        let reg = self.alloc_reg(node.location(), 1);
        self.compile_expr_auto_temp(node, reg, INVALID_REG)
    }

    fn compile_expr_side(&mut self, node: &'a AstExpr) {
        if matches!(node, AstExpr::Local(_) | AstExpr::Global(_) | AstExpr::Varargs(_) | AstExpr::Function(_)) || self.is_constant(node) {
            return;
        }
        // note: the remark is omitted for calls as it's fairly noisy due to inlining
        if !matches!(node, AstExpr::Call(_)) {
            self.bytecode.add_debug_remark("expression only compiled for side effects".into());
        }
        scoped!(self, {
            self.compile_expr_auto(node);
        })
    }

    fn compile_expr_temp_n(&mut self, node: &'a AstExpr, target: u8, target_count: u8, target_top: bool) {
        if target_count == 255 {
            CompileError::raise(node.location(), "Exceeded result count limit; simplify the code to compile");
        }
        match node {
            AstExpr::Call(call) => self.compile_expr_call(node, call, target, target_count, target_top, false),
            AstExpr::Varargs(v) => self.compile_expr_varargs(v.location, target, target_count, false),
            _ => {
                self.compile_expr_temp(node, target);
                for i in 1..target_count {
                    self.bytecode.emit_abc(Opcode::LoadNil, target + i, 0, 0);
                }
            }
        }
    }

    fn compile_expr_list_temp(&mut self, list: &'a [Box<AstExpr>], target: u8, target_count: u8, target_top: bool) {
        let n = list.len();
        let tc = target_count as usize;
        if n == tc {
            for (i, e) in list.iter().enumerate() {
                self.compile_expr_temp(e, target + i as u8);
            }
        } else if n > tc {
            for i in 0..tc {
                self.compile_expr_temp(&list[i], target + i as u8);
            }
            for e in &list[tc..] {
                self.compile_expr_side(e);
            }
        } else if n > 0 {
            for i in 0..n - 1 {
                self.compile_expr_temp(&list[i], target + i as u8);
            }
            self.compile_expr_temp_n(&list[n - 1], target + (n - 1) as u8, target_count - (n - 1) as u8, target_top);
        } else {
            for i in 0..target_count {
                self.bytecode.emit_abc(Opcode::LoadNil, target + i, 0, 0);
            }
        }
    }

    // -----------------------------------------------------------------
    // L-values
    // -----------------------------------------------------------------

    fn compile_lvalue_index(&mut self, reg: u8, index: &'a AstExpr) -> LValue {
        let cv = self.get_constant(index);
        match cv {
            Constant::Number(n) if (1.0..=256.0).contains(&n) && (n as i32) as f64 == n => {
                let mut r = LValue::new(LValueKind::IndexNumber, index.location());
                r.reg = reg;
                r.number = (n as i32 - 1) as u8;
                r
            }
            Constant::String(s) => {
                let mut r = LValue::new(LValueKind::IndexName, index.location());
                r.reg = reg;
                r.name = s.to_vec();
                r
            }
            _ => {
                let mut r = LValue::new(LValueKind::IndexExpr, index.location());
                r.reg = reg;
                r.index = self.compile_expr_auto(index);
                r
            }
        }
    }

    fn compile_lvalue(&mut self, node: &'a AstExpr) -> LValue {
        self.set_debug_line(node.location());
        match node {
            AstExpr::Local(e) => {
                if e.local.is_exported && !self.exports.exported_functions.contains(&local_key(&e.local)) {
                    let table_reg = self.get_export_table_reg(node.location());
                    let mut r = LValue::new(LValueKind::IndexName, node.location());
                    r.reg = table_reg;
                    r.name = e.local.name.0.as_bytes().to_vec();
                    return r;
                }
                let reg = self.get_expr_local_reg(node);
                if reg >= 0 {
                    let mut r = LValue::new(LValueKind::Local, node.location());
                    r.reg = reg as u8;
                    r
                } else {
                    let mut r = LValue::new(LValueKind::Upvalue, node.location());
                    r.upval = self.get_upval(&e.local);
                    r
                }
            }
            AstExpr::Global(g) => {
                let mut r = LValue::new(LValueKind::Global, node.location());
                r.name = g.name.0.as_bytes().to_vec();
                r
            }
            AstExpr::IndexName(e) => {
                let private = self.options.classes && e.private_access;
                let mut r = LValue::new(if private { LValueKind::PrivateField } else { LValueKind::IndexName }, node.location());
                r.reg = self.compile_expr_auto(&e.expr);
                if private {
                    r.ast_name = e.index.0;
                } else {
                    r.name = e.index.0.as_bytes().to_vec();
                }
                r
            }
            AstExpr::IndexExpr(e) => {
                let reg = self.compile_expr_auto(&e.expr);
                self.compile_lvalue_index(reg, &e.index)
            }
            _ => CompileError::raise(node.location(), "Unknown assignment expression"),
        }
    }

    fn compile_lvalue_use(&mut self, lv: &LValue, reg: u8, set: bool, target_expr: Option<&AstExpr>) {
        self.set_debug_line(lv.location);
        match lv.kind {
            LValueKind::Local => {
                if set {
                    self.bytecode.emit_abc(Opcode::Move, lv.reg, reg, 0);
                } else {
                    self.bytecode.emit_abc(Opcode::Move, reg, lv.reg, 0);
                }
            }
            LValueKind::Upvalue => {
                self.bytecode.emit_abc(if set { Opcode::SetUpval } else { Opcode::GetUpval }, reg, lv.upval, 0);
            }
            LValueKind::Global => {
                let cid = self.bytecode.add_constant_string(&lv.name);
                if cid < 0 {
                    const_limit(lv.location);
                }
                self.bytecode.emit_abc(
                    if set { Opcode::SetGlobal } else { Opcode::GetGlobal },
                    reg,
                    0,
                    BytecodeBuilder::get_string_hash(&lv.name) as u8,
                );
                self.bytecode.emit_aux(cid as u32);
            }
            LValueKind::IndexName => {
                let cid = self.bytecode.add_constant_string(&lv.name);
                if cid < 0 {
                    const_limit(lv.location);
                }
                self.bytecode.emit_abc(
                    if set { Opcode::SetTableKs } else { Opcode::GetTableKs },
                    reg,
                    lv.reg,
                    BytecodeBuilder::get_string_hash(&lv.name) as u8,
                );
                self.bytecode.emit_aux(cid as u32);
                if let Some(AstExpr::IndexName(t)) = target_expr {
                    self.hint_temporary_expr_reg_type(&t.expr, lv.reg, TABLE_T, 2);
                }
            }
            LValueKind::IndexNumber => {
                self.bytecode.emit_abc(if set { Opcode::SetTableN } else { Opcode::GetTableN }, reg, lv.reg, lv.number);
                if let Some(AstExpr::IndexExpr(t)) = target_expr {
                    self.hint_temporary_expr_reg_type(&t.expr, lv.reg, TABLE_T, 1);
                }
            }
            LValueKind::IndexExpr => {
                self.bytecode.emit_abc(if set { Opcode::SetTable } else { Opcode::GetTable }, reg, lv.reg, lv.index);
                if let Some(AstExpr::IndexExpr(t)) = target_expr {
                    self.hint_temporary_expr_reg_type(&t.expr, lv.reg, TABLE_T, 1);
                    self.hint_temporary_expr_reg_type(&t.index, lv.index, NUMBER_T, 1);
                }
            }
            LValueKind::PrivateField => {
                let what = if set { "assign" } else { "access" };
                let cls = self.curr_class.expect("private field outside of a class");
                let Some(offsets) = self.class_private_field_offsets.get(&key(cls)) else {
                    CompileError::raise(lv.location, format!("Attempt to {} private field of a class with no private fields", what))
                };
                let Some(&(offset, is_static)) = offsets.get(lv.ast_name) else {
                    CompileError::raise(lv.location, format!("Attempt to {} undefined private field", what))
                };
                if is_static && set {
                    CompileError::raise(lv.location, "Attempt to assign static field of a class");
                }
                let upval = self.get_upval(&cls.name);
                self.bytecode.emit_abc(if set { Opcode::SetPrivateField } else { Opcode::GetPrivateField }, reg, lv.reg, upval);
                self.bytecode.emit_aux(offset as u32);
            }
        }
    }

    /// `compilePrivateFieldAccess`.
    fn compile_private_field_access(&mut self, expr: &AstExprIndexName, target: u8, source: u8, namecall: bool) {
        let cls = self.curr_class.expect("private field outside of a class");
        let Some(offsets) = self.class_private_field_offsets.get(&key(cls)) else {
            CompileError::raise(expr.location, "Attempt to access private field of a class with no private fields")
        };
        let Some(&(offset, is_static)) = offsets.get(expr.index.0) else {
            CompileError::raise(expr.location, "Attempt to access undefined private field")
        };
        let upval = self.get_upval(&cls.name);
        self.bytecode.emit_abc(Opcode::GetPrivateField, target, source, upval);
        self.bytecode.emit_aux(((namecall as u32) << 9) | ((is_static as u32) << 8) | offset as u32);
    }

    /// `compileClassDeclaration`.
    fn compile_class_declaration(&mut self, decl: &'a AstStatClass) {
        let dest = self.alloc_reg(decl.location, 1);
        self.push_local(&decl.name, dest, DEFAULT_ALLOC_PC);
        if decl.exported {
            self.ensure_export_table(decl.location);
            self.exports.exported_classes.push((decl.name.clone(), dest));
        }
        scoped!(self, {
            let cval = decl.open as u8;
            if let Some(sup) = &decl.super_ {
                let super_reg = self.get_expr_local_reg(sup);
                let super_dest = self.alloc_reg(decl.location, if super_reg < 0 { 1 } else { 0 });
                if super_reg >= 0 {
                    self.bytecode.emit_abc(Opcode::NewClass, dest, super_reg as u8, cval);
                } else {
                    self.compile_expr(sup, super_dest, false);
                    self.bytecode.emit_abc(Opcode::NewClass, dest, super_dest, cval);
                }
            } else {
                self.bytecode.emit_abc(Opcode::NewClass, dest, INVALID_REG, cval);
            }
            let aux_offset = self.bytecode.emit_label();
            self.bytecode.emit_aux(0xDEADBEEF);

            let mut shape = crate::builder::ClassShape::default();
            shape.class_name = self.bytecode.add_constant_string(decl.name.name.0.as_bytes());
            if shape.class_name < 0 {
                const_limit(decl.name.location);
            }
            let temp = self.alloc_reg(decl.location, 1);
            let mut has_ctor = false;
            for member in &decl.members {
                match member {
                    AstClassMember::Property(prop) => match prop.access {
                        AstClassAccess::Public => {
                            let cid = self.bytecode.add_constant_string(prop.name.0.as_bytes());
                            if cid < 0 {
                                const_limit(prop.name_location);
                            }
                            shape.property_names.push(cid);
                        }
                        AstClassAccess::Private => shape.num_private_instance_props += 1,
                    },
                    AstClassMember::Method(method) => {
                        self.compile_expr_function(&method.function, temp);
                        match method.access {
                            AstClassAccess::Public => {
                                let cid = self.bytecode.add_constant_string(method.function_name.0.as_bytes());
                                if cid < 0 {
                                    const_limit(method.function.location);
                                }
                                shape.method_names.push(cid);
                                self.bytecode.emit_abc(Opcode::NewClassMember, dest, 0, temp);
                                self.bytecode.emit_aux(cid as u32);
                            }
                            AstClassAccess::Private => {
                                self.bytecode.emit_abc(Opcode::NewClassMember, dest, 1, temp);
                                self.bytecode.emit_aux(shape.num_private_static_props as u32);
                                shape.num_private_static_props += 1;
                            }
                        }
                        if method.function_name.0 == "__init" {
                            has_ctor = true;
                        }
                    }
                }
            }
            if !has_ctor {
                let cid = self.bytecode.add_constant_string(b"__init");
                if cid < 0 {
                    const_limit(decl.location);
                }
                shape.method_names.push(cid);
            }
            let class_const = self.bytecode.add_class_shape(shape);
            if class_const < 0 {
                const_limit(decl.location);
            }
            self.bytecode.patch_aux(aux_offset, class_const as u32);
        })
    }

    fn compile_assign(&mut self, lv: &LValue, source: u8, target_expr: Option<&AstExpr>) {
        self.compile_lvalue_use(lv, source, true, target_expr);
    }

    fn get_expr_local(node: &'a AstExpr) -> Option<&'a AstExprLocal> {
        match unwrap_expr(node) {
            AstExpr::Local(l) => Some(l),
            _ => None,
        }
    }

    fn get_expr_local_reg(&self, node: &AstExpr) -> i32 {
        match unwrap_expr(node) {
            AstExpr::Local(l) => self.get_local_reg(&l.local),
            _ => -1,
        }
    }

    // -----------------------------------------------------------------
    // Statements
    // -----------------------------------------------------------------

    fn compile_stat_if_local(&mut self, stat: &'a AstStatIf) {
        let cl = stat.condition_local.as_ref().unwrap();
        let then_terminates = self.always_terminates_block(&stat.thenbody);
        let skip_else = self.is_constant_true(&stat.condition);
        let old_locals = self.local_stack.len();
        let mut else_jump = 0;
        scoped!(self, {
            let reg = self.alloc_reg(stat.location, 1);
            let allocpc = self.bytecode.get_debug_pc();
            self.compile_expr_temp(&stat.condition, reg);
            self.push_local(cl, reg, allocpc);
            if !skip_else {
                else_jump = self.bytecode.emit_label();
                self.bytecode.emit_ad(Opcode::JumpIfNot, reg, 0);
            }
            self.compile_block_stat(&stat.thenbody);
            if !then_terminates {
                self.close_locals(old_locals);
            }
            self.pop_locals(old_locals);
        });
        if skip_else {
            return;
        }
        if let Some(else_body) = &stat.elsebody {
            if then_terminates {
                let else_label = self.bytecode.emit_label();
                self.compile_stat(else_body);
                self.patch_jump(stat.location, else_jump, else_label);
            } else {
                let then_label = self.bytecode.emit_label();
                self.bytecode.emit_ad(Opcode::Jump, 0, 0);
                let else_label = self.bytecode.emit_label();
                self.compile_stat(else_body);
                let end_label = self.bytecode.emit_label();
                self.patch_jump(stat.location, else_jump, else_label);
                self.patch_jump(stat.location, then_label, end_label);
            }
        } else {
            let end_label = self.bytecode.emit_label();
            self.patch_jump(stat.location, else_jump, end_label);
        }
    }

    fn compile_stat_if(&mut self, stat: &'a AstStatIf) {
        if self.is_constant_false(&stat.condition) {
            if let Some(e) = &stat.elsebody {
                self.compile_stat(e);
            }
            return;
        }
        if let AstExpr::Binary(cand) = &*stat.condition {
            if cand.op == AstExprBinaryOp::And && self.is_constant_false(&cand.right) {
                self.compile_expr_side(&cand.left);
                if let Some(e) = &stat.elsebody {
                    self.compile_stat(e);
                }
                return;
            }
        }
        if stat.condition_local.is_some() {
            self.compile_stat_if_local(stat);
            return;
        }

        if stat.elsebody.is_none() && is_stat_break(&stat.thenbody) && !self.are_locals_captured(self.loops.last().unwrap().local_offset) {
            let mut else_jump = Vec::new();
            self.compile_condition_value(&stat.condition, None, &mut else_jump, true);
            for j in else_jump {
                self.loop_jumps.push(LoopJump { ty: LoopJumpType::Break, label: j });
            }
            return;
        }

        if stat.elsebody.is_none() {
            if let Some(cont) = extract_stat_continue(&stat.thenbody) {
                if !self.are_locals_captured(self.loops.last().unwrap().local_offset_continue) {
                    let lp = self.loops.last_mut().unwrap();
                    if lp.continue_used.is_none() {
                        lp.continue_used = Some(cont.location);
                    }
                    let mut else_jump = Vec::new();
                    self.compile_condition_value(&stat.condition, None, &mut else_jump, true);
                    for j in else_jump {
                        self.loop_jumps.push(LoopJump { ty: LoopJumpType::Continue, label: j });
                    }
                    return;
                }
            }
        }

        let mut else_jump = Vec::new();
        self.compile_condition_value(&stat.condition, None, &mut else_jump, false);
        self.compile_block_stat(&stat.thenbody);

        match &stat.elsebody {
            Some(else_body) if !else_jump.is_empty() => {
                if self.always_terminates_block(&stat.thenbody) {
                    let else_label = self.bytecode.emit_label();
                    self.compile_stat(else_body);
                    self.patch_jumps(stat.location, &else_jump, else_label);
                } else {
                    let then_label = self.bytecode.emit_label();
                    self.bytecode.emit_ad(Opcode::Jump, 0, 0);
                    let else_label = self.bytecode.emit_label();
                    self.compile_stat(else_body);
                    let end_label = self.bytecode.emit_label();
                    self.patch_jumps(stat.location, &else_jump, else_label);
                    self.patch_jump(stat.location, then_label, end_label);
                }
            }
            _ => {
                let end_label = self.bytecode.emit_label();
                self.patch_jumps(stat.location, &else_jump, end_label);
            }
        }
    }

    fn compile_stat_while_local(&mut self, stat: &'a AstStatWhile) {
        let cl = stat.condition_local.as_ref().unwrap();
        if self.is_constant_false(&stat.condition) {
            return;
        }
        scoped!(self, {
            let old_jumps = self.loop_jumps.len();
            let old_locals = self.local_stack.len();
            self.loops.push(Loop { local_offset: old_locals, local_offset_continue: old_locals, continue_used: None });
            self.has_loops = true;
            let reg = self.alloc_reg(stat.location, 1);
            let allocpc = self.bytecode.get_debug_pc();
            let loop_label = self.bytecode.emit_label();
            self.compile_expr_temp(&stat.condition, reg);
            self.push_local(cl, reg, allocpc);
            let exit_jump = self.bytecode.emit_label();
            self.bytecode.emit_ad(Opcode::JumpIfNot, reg, 0);
            self.compile_block_stat(&stat.body);
            self.close_locals(old_locals);
            self.pop_locals(old_locals);
            let cont_label = self.bytecode.emit_label();
            let back_label = self.bytecode.emit_label();
            self.set_debug_line(stat.condition.location());
            self.bytecode.emit_ad(Opcode::JumpBack, 0, 0);
            let end_label = self.bytecode.emit_label();
            self.patch_jump(stat.location, back_label, loop_label);
            self.patch_jump(stat.location, exit_jump, end_label);
            self.patch_loop_jumps(stat.location, old_jumps, end_label, cont_label);
            self.loop_jumps.truncate(old_jumps);
            self.loops.pop();
        })
    }

    fn compile_stat_while(&mut self, stat: &'a AstStatWhile) {
        if stat.condition_local.is_some() {
            self.compile_stat_while_local(stat);
            return;
        }
        if self.is_constant_false(&stat.condition) {
            return;
        }
        let old_jumps = self.loop_jumps.len();
        let old_locals = self.local_stack.len();
        self.loops.push(Loop { local_offset: old_locals, local_offset_continue: old_locals, continue_used: None });
        self.has_loops = true;

        let loop_label = self.bytecode.emit_label();
        let mut else_jump = Vec::new();
        self.compile_condition_value(&stat.condition, None, &mut else_jump, false);
        self.compile_block_stat(&stat.body);
        let cont_label = self.bytecode.emit_label();
        let back_label = self.bytecode.emit_label();
        self.set_debug_line(stat.condition.location());
        self.bytecode.emit_ad(Opcode::JumpBack, 0, 0);
        let end_label = self.bytecode.emit_label();
        self.patch_jump(stat.location, back_label, loop_label);
        self.patch_jumps(stat.location, &else_jump, end_label);
        self.patch_loop_jumps(stat.location, old_jumps, end_label, cont_label);
        self.loop_jumps.truncate(old_jumps);
        self.loops.pop();
    }

    fn compile_stat_repeat(&mut self, stat: &'a AstStatRepeat) {
        let old_jumps = self.loop_jumps.len();
        let old_locals = self.local_stack.len();
        self.loops.push(Loop { local_offset: old_locals, local_offset_continue: old_locals, continue_used: None });
        self.has_loops = true;
        let loop_label = self.bytecode.emit_label();
        let body = &stat.body;

        scoped!(self, {
            let mut continue_validated = false;
            let mut condition_locals = 0;
            for (i, s) in body.body.iter().enumerate() {
                self.pop_stat_locals(body, s, old_locals);
                self.compile_stat(s);
                if self.loops.last().unwrap().continue_used.is_none() {
                    self.loops.last_mut().unwrap().local_offset_continue = self.local_stack.len();
                }
                if let Some(cont) = self.loops.last().unwrap().continue_used {
                    if !continue_validated {
                        self.validate_continue_until(cont, &stat.condition, body, i + 1);
                        continue_validated = true;
                        condition_locals = self.local_stack.len();
                    }
                }
            }
            if continue_validated {
                self.set_debug_line_end(body.body.last().unwrap().location());
                self.close_locals(condition_locals);
                self.pop_locals(condition_locals);
            }

            let cont_label = self.bytecode.emit_label();
            let end_label;
            self.set_debug_line(stat.condition.location());
            if self.is_constant_true(&stat.condition) {
                self.close_locals(old_locals);
                end_label = self.bytecode.emit_label();
            } else {
                let mut skip_jump = Vec::new();
                self.compile_condition_value(&stat.condition, None, &mut skip_jump, true);
                self.close_locals(old_locals);
                let back_label = self.bytecode.emit_label();
                self.bytecode.emit_ad(Opcode::JumpBack, 0, 0);
                let skip_label = self.bytecode.emit_label();
                self.close_locals(old_locals);
                end_label = self.bytecode.emit_label();
                self.patch_jump(stat.location, back_label, loop_label);
                self.patch_jumps(stat.location, &skip_jump, skip_label);
            }
            self.pop_locals(old_locals);
            self.patch_loop_jumps(stat.location, old_jumps, end_label, cont_label);
            self.loop_jumps.truncate(old_jumps);
            self.loops.pop();
        })
    }

    fn compile_stat_return(&mut self, stat: &'a AstStatReturn) {
        if stat.list.len() >= 255 {
            CompileError::raise(stat.location, "Exceeded return count limit; simplify the code to compile");
        }
        if self.current_function.unwrap().depth == 0 && self.inline_frames.is_empty() {
            self.exports.has_module_return = true;
        }
        scoped!(self, {
            let mut temp = 0u8;
            let mut consecutive = false;
            let mut mult_ret = false;

            let reg = if !stat.list.is_empty() { self.get_expr_local_reg(&stat.list[0]) } else { -1 };
            if reg >= 0 {
                temp = reg as u8;
                consecutive = true;
                for i in 1..stat.list.len() {
                    if self.get_expr_local_reg(&stat.list[i]) != temp as i32 + i as i32 {
                        consecutive = false;
                        break;
                    }
                }
            }

            if !consecutive && stat.list.len() > 1 {
                let all_locals = stat.list.iter().all(|e| self.get_expr_local_reg(e) != -1);
                if all_locals {
                    let base = self.get_expr_local_reg(&stat.list[0]) as u8;
                    let mut prefix = 1usize;
                    while prefix < stat.list.len() && self.get_expr_local_reg(&stat.list[prefix]) == base as i32 + prefix as i32 {
                        prefix += 1;
                    }
                    if base as u32 + prefix as u32 == self.reg_top {
                        self.alloc_reg(stat.location, (stat.list.len() - prefix) as u32);
                        for i in prefix..stat.list.len() {
                            let src = self.get_expr_local_reg(&stat.list[i]) as u8;
                            self.bytecode.emit_abc(Opcode::Move, base + i as u8, src, 0);
                        }
                        temp = base;
                        consecutive = true;
                    }
                }
            }

            if !consecutive && stat.list.len() == 1 && !self.is_expr_mult_ret(&stat.list[0]) {
                temp = self.alloc_reg(stat.location, 1);
                temp = self.compile_expr_auto_temp(&stat.list[0], temp, INVALID_REG);
            } else if !consecutive && !stat.list.is_empty() {
                temp = self.alloc_reg(stat.location, stat.list.len() as u32);
                for (i, e) in stat.list.iter().enumerate() {
                    if i + 1 == stat.list.len() {
                        mult_ret = self.compile_expr_temp_mult_ret(e, temp + i as u8);
                    } else {
                        self.compile_expr_temp_top(e, temp + i as u8);
                    }
                }
            }

            self.close_locals(0);
            if mult_ret {
                self.has_multi_ret = true;
            }
            self.bytecode.emit_abc(Opcode::Return, temp, if mult_ret { 0 } else { stat.list.len() as u8 + 1 }, 0);
        })
    }

    fn are_locals_redundant(&self, stat: &AstStatLocal) -> bool {
        if stat.values.len() > stat.vars.len() {
            return false;
        }
        for local in &stat.vars {
            match self.variables.get(&local_key(local)) {
                Some(v) if v.constant => {}
                _ => return false,
            }
            if local.is_exported && self.exports.exported_table_cid == -1 {
                return false;
            }
        }
        true
    }

    fn compile_stat_local(&mut self, stat: &'a AstStatLocal) {
        if self.options.optimization_level >= 1 && self.are_locals_redundant(stat) {
            return;
        }

        if self.options.optimization_level >= 1 && stat.vars.len() == 1 && stat.values.len() == 1 {
            if let Some(re) = Self::get_expr_local(&stat.values[0]) {
                let lv_written = self.variables.get(&local_key(&stat.vars[0])).is_some_and(|v| v.written);
                let rv_written = self.variables.get(&local_key(&re.local)).is_some_and(|v| v.written);
                let reg = self.get_local_reg(&re.local);
                if reg >= 0 && !lv_written && !rv_written && !stat.vars[0].is_exported && !re.local.is_exported {
                    self.push_local(&stat.vars[0], reg as u8, DEFAULT_ALLOC_PC);
                    return;
                }
            }
            // a local that becomes the inlined function result lives in the target register
            if let Some(frame) = self.inline_frames.last() {
                if frame.result_local.as_ref().is_some_and(|l| Arc::ptr_eq(l, &stat.vars[0])) {
                    let target = frame.target;
                    self.compile_expr_temp(&stat.values[0], target);
                    self.push_local(&stat.vars[0], target, DEFAULT_ALLOC_PC);
                    return;
                }
            }
            if let Some(target) = self.try_reuse_local_reg(&stat.vars[0]) {
                let allocpc = self.bytecode.get_debug_pc();
                self.compile_expr_temp(&stat.values[0], target);
                self.push_local(&stat.vars[0], target, allocpc);
                return;
            }
        }

        let vars = self.alloc_reg(stat.location, stat.vars.len() as u32);
        let allocpc = self.bytecode.get_debug_pc();
        self.compile_expr_list_temp(&stat.values, vars, stat.vars.len() as u8, true);

        for (i, local) in stat.vars.iter().enumerate() {
            let constant = self.variables.get(&local_key(local)).is_some_and(|v| v.constant);
            if local.is_exported && (!constant || self.exports.exported_table_cid == -1) {
                self.ensure_export_table(stat.location);
                let name = local.name.0.as_bytes();
                let cid = self.bytecode.add_constant_string(name);
                if cid < 0 {
                    const_limit(local.location);
                }
                let table_reg = self.get_export_table_reg(stat.location);
                self.bytecode.emit_abc(Opcode::SetTableKs, vars + i as u8, table_reg, BytecodeBuilder::get_string_hash(name) as u8);
                self.bytecode.emit_aux(cid as u32);
            } else {
                self.push_local(local, vars + i as u8, allocpc);
            }
        }
    }

    fn compile_stat_for(&mut self, stat: &'a AstStatFor) {
        // small loops can be unrolled when it is profitable
        if self.options.optimization_level >= 2
            && self.is_constant(&stat.to)
            && self.is_constant(&stat.from)
            && stat.step.as_ref().is_none_or(|s| self.is_constant(s))
        {
            let old = self.reg_top;
            let done = self.try_compile_unrolled_for(stat, LOOP_UNROLL_THRESHOLD, LOOP_UNROLL_THRESHOLD_MAX_BOOST);
            self.reg_top = old;
            if done {
                return;
            }
        }
        scoped!(self, {
            let old_locals = self.local_stack.len();
            let old_jumps = self.loop_jumps.len();
            self.loops.push(Loop { local_offset: old_locals, local_offset_continue: old_locals, continue_used: None });
            self.has_loops = true;

            let regs = self.alloc_reg(stat.location, 3);
            let mut varreg = regs + 2;
            let varregallocpc = self.bytecode.get_debug_pc();
            if self.variables.get(&local_key(&stat.var)).is_some_and(|v| v.written) {
                varreg = self.alloc_reg(stat.location, 1);
            }

            self.compile_expr_temp(&stat.from, regs + 2);
            self.compile_expr_temp(&stat.to, regs);
            match &stat.step {
                Some(st) => self.compile_expr_temp(st, regs + 1),
                None => self.bytecode.emit_abc(Opcode::LoadN, regs + 1, 1, 0),
            }

            let for_label = self.bytecode.emit_label();
            self.bytecode.emit_ad(Opcode::ForNPrep, regs, 0);
            let loop_label = self.bytecode.emit_label();
            if varreg != regs + 2 {
                self.bytecode.emit_abc(Opcode::Move, varreg, regs + 2, 0);
            }
            self.push_local(&stat.var, varreg, varregallocpc);
            self.compile_block_stat(&stat.body);
            self.close_locals(old_locals);
            self.pop_locals(old_locals);

            self.set_debug_line(stat.location);
            let cont_label = self.bytecode.emit_label();
            let back_label = self.bytecode.emit_label();
            self.bytecode.emit_ad(Opcode::ForNLoop, regs, 0);
            let end_label = self.bytecode.emit_label();
            self.patch_jump(stat.location, for_label, end_label);
            self.patch_jump(stat.location, back_label, loop_label);
            self.patch_loop_jumps(stat.location, old_jumps, end_label, cont_label);
            self.loop_jumps.truncate(old_jumps);
            self.loops.pop();
        })
    }

    fn compile_stat_for_in(&mut self, stat: &'a AstStatForIn) {
        scoped!(self, {
            let old_locals = self.local_stack.len();
            let old_jumps = self.loop_jumps.len();
            self.loops.push(Loop { local_offset: old_locals, local_offset_continue: old_locals, continue_used: None });
            self.has_loops = true;

            let regs = self.alloc_reg(stat.location, 3);
            self.compile_expr_list_temp(&stat.values, regs, 3, true);
            let vars = self.alloc_reg(stat.location, (stat.vars.len() as u32).max(2));
            let varsallocpc = self.bytecode.get_debug_pc();

            let mut skip_op = Opcode::ForGPrep;
            if self.options.optimization_level >= 1 && stat.vars.len() <= 2 {
                if stat.values.len() == 1 {
                    if let AstExpr::Call(c) = &*stat.values[0] {
                        let b = get_builtin(&c.func, &self.globals, &self.variables);
                        if b.is_global("ipairs") {
                            skip_op = Opcode::ForGPrepInext;
                        } else if b.is_global("pairs") {
                            skip_op = Opcode::ForGPrepNext;
                        }
                    }
                } else if stat.values.len() == 2 && !self.getfenv_used && !self.setfenv_used {
                    let b = get_builtin(&stat.values[0], &self.globals, &self.variables);
                    if b.is_global("next") {
                        skip_op = Opcode::ForGPrepNext;
                    }
                }
            }

            let skip_label = self.bytecode.emit_label();
            self.bytecode.emit_ad(skip_op, regs, 0);
            let loop_label = self.bytecode.emit_label();
            for (i, v) in stat.vars.iter().enumerate() {
                self.push_local(v, vars + i as u8, varsallocpc);
            }
            self.compile_block_stat(&stat.body);
            self.close_locals(old_locals);
            self.pop_locals(old_locals);

            self.set_debug_line(stat.location);
            let cont_label = self.bytecode.emit_label();
            let back_label = self.bytecode.emit_label();
            self.bytecode.emit_ad(Opcode::ForGLoop, regs, 0);
            self.bytecode.emit_aux(
                (if skip_op == Opcode::ForGPrepInext { 0x8000_0000 } else { 0 }) | stat.vars.len() as u32,
            );
            let end_label = self.bytecode.emit_label();
            self.patch_jump(stat.location, skip_label, back_label);
            self.patch_jump(stat.location, back_label, loop_label);
            self.patch_loop_jumps(stat.location, old_jumps, end_label, cont_label);
            self.loop_jumps.truncate(old_jumps);
            self.loops.pop();
        })
    }

    fn resolve_assign_conflicts(&mut self, loc: Location, vars: &mut [(LValue, u8, u8)], values: &'a [Box<AstExpr>]) {
        struct V<'c> {
            locals: &'c HashMap<LocalKey, Local>,
            conflict: [bool; 256],
            assigned: [bool; 256],
        }
        impl<'z> Visitor<'z> for V<'_> {
            fn visit_expr(&mut self, e: &AstExpr) -> bool {
                if let AstExpr::Local(l) = e {
                    if let Some(loc) = self.locals.get(&local_key(&l.local)) {
                        if loc.allocated && self.assigned[loc.reg as usize] {
                            self.conflict[loc.reg as usize] = true;
                        }
                    }
                }
                true
            }
        }
        let mut v = V { locals: &self.locals, conflict: [false; 256], assigned: [false; 256] };
        for (i, (li, _, _)) in vars.iter().enumerate() {
            if li.kind == LValueKind::Local {
                if let Some(e) = values.get(i) {
                    walk_expr(&mut v, e);
                }
                v.assigned[li.reg as usize] = true;
            }
        }
        for (i, (li, _, _)) in vars.iter().enumerate() {
            if li.kind != LValueKind::Local {
                if let Some(e) = values.get(i) {
                    walk_expr(&mut v, e);
                }
            }
        }
        for e in values.iter().skip(vars.len()) {
            walk_expr(&mut v, e);
        }
        for (li, _, _) in vars.iter() {
            if matches!(li.kind, LValueKind::IndexName | LValueKind::IndexNumber | LValueKind::IndexExpr | LValueKind::PrivateField)
                && v.assigned[li.reg as usize]
            {
                v.conflict[li.reg as usize] = true;
            }
            if li.kind == LValueKind::IndexExpr && v.assigned[li.index as usize] {
                v.conflict[li.index as usize] = true;
            }
        }
        let conflict = v.conflict;
        for var in vars.iter_mut() {
            if var.0.kind == LValueKind::Local && conflict[var.0.reg as usize] {
                var.1 = self.alloc_reg(loc, 1);
            }
        }
    }

    fn compile_stat_assign(&mut self, stat: &'a AstStatAssign) {
        scoped!(self, {
            if stat.vars.len() == 1 && stat.values.len() == 1 {
                let var = self.compile_lvalue(&stat.vars[0]);
                if var.kind == LValueKind::Local {
                    self.compile_expr(&stat.values[0], var.reg, false);
                } else {
                    let reg = self.compile_expr_auto(&stat.values[0]);
                    self.set_debug_line(stat.vars[0].location());
                    self.compile_assign(&var, reg, Some(&stat.vars[0]));
                }
                return;
            }

            // (lvalue, conflictReg, valueReg)
            let mut vars: Vec<(LValue, u8, u8)> = Vec::with_capacity(stat.vars.len());
            for v in &stat.vars {
                let lv = self.compile_lvalue(v);
                vars.push((lv, INVALID_REG, INVALID_REG));
            }
            self.resolve_assign_conflicts(stat.location, &mut vars, &stat.values);

            let nvars = stat.vars.len();
            let nvalues = stat.values.len();
            let mut i = 0;
            while i < nvars && i < nvalues {
                let value: &'a AstExpr = &stat.values[i];
                if i + 1 == nvalues && nvars > nvalues {
                    let rest = (nvars - nvalues + 1) as u32;
                    let temp = self.alloc_reg(stat.location, rest);
                    self.compile_expr_temp_n(value, temp, rest as u8, true);
                    for j in i..nvars {
                        vars[j].2 = temp + (j - i) as u8;
                    }
                } else if vars[i].0.kind == LValueKind::Local {
                    let vr = if vars[i].1 == INVALID_REG { vars[i].0.reg } else { vars[i].1 };
                    vars[i].2 = vr;
                    self.compile_expr(value, vr, false);
                } else {
                    vars[i].2 = self.compile_expr_auto(value);
                }
                i += 1;
            }

            for e in stat.values.iter().skip(nvars) {
                self.compile_expr_side(e);
            }

            for (pos, (lv, _, value_reg)) in vars.iter().enumerate() {
                if lv.kind != LValueKind::Local {
                    self.set_debug_line(lv.location);
                    self.compile_assign(lv, *value_reg, stat.vars.get(pos).map(|v| &**v));
                }
            }
            for (lv, _, value_reg) in &vars {
                if lv.kind == LValueKind::Local && *value_reg != lv.reg {
                    self.bytecode.emit_abc(Opcode::Move, lv.reg, *value_reg, 0);
                }
            }
        })
    }

    fn compile_stat_compound_assign(&mut self, stat: &'a AstStatCompoundAssign) {
        use AstExprBinaryOp as B;
        scoped!(self, {
            let var = self.compile_lvalue(&stat.var);
            let target = if var.kind == LValueKind::Local { var.reg } else { self.alloc_reg(stat.location, 1) };
            match stat.op {
                B::Add | B::Sub | B::Mul | B::Div | B::FloorDiv | B::Mod | B::Pow => {
                    if var.kind != LValueKind::Local {
                        self.compile_lvalue_use(&var, target, false, Some(&stat.var));
                    }
                    let rc = self.get_constant_number(&stat.value);
                    if (0..=255).contains(&rc) {
                        self.bytecode.emit_abc(arith_op(stat.op, true), target, target, rc as u8);
                    } else {
                        let rr = self.compile_expr_auto(&stat.value);
                        self.bytecode.emit_abc(arith_op(stat.op, false), target, target, rr);
                        if var.kind != LValueKind::Local {
                            self.hint_temporary_reg_type(&stat.var, target, NUMBER_T, 1);
                        }
                        self.hint_temporary_expr_reg_type(&stat.value, rr, NUMBER_T, 1);
                    }
                }
                B::Concat => {
                    let mut args: Vec<&'a AstExpr> = vec![&stat.value];
                    unroll_concats(&mut args);
                    let regs = self.alloc_reg(stat.location, 1 + args.len() as u32);
                    self.compile_lvalue_use(&var, regs, false, Some(&stat.var));
                    for (i, a) in args.iter().enumerate() {
                        self.compile_expr_temp_top(a, regs + 1 + i as u8);
                    }
                    self.bytecode.emit_abc(Opcode::Concat, target, regs, regs + args.len() as u8);
                }
                _ => CompileError::raise(stat.location, "Unexpected compound assignment operation"),
            }
            if var.kind != LValueKind::Local {
                self.compile_assign(&var, target, Some(&stat.var));
            }
        })
    }

    fn compile_stat_function(&mut self, stat: &'a AstStatFunction) {
        let reg = self.get_expr_local_reg(&stat.name);
        if reg >= 0 {
            self.set_debug_line(stat.func.location);
            self.compile_expr_function(&stat.func, reg as u8);
            return;
        }
        scoped!(self, {
            let reg = self.alloc_reg(stat.location, 1);
            self.set_debug_line(stat.func.location);
            self.compile_expr_function(&stat.func, reg);
            let var = self.compile_lvalue(&stat.name);
            self.compile_assign(&var, reg, Some(&stat.name));
        })
    }

    /// A nested block (`if`/loop/function body) compiled as a statement.
    fn compile_block_stat(&mut self, block: &'a AstStatBlock) {
        self.set_debug_line(block.location);
        self.compile_block_inner(block);
    }

    fn compile_block_inner(&mut self, block: &'a AstStatBlock) {
        scoped!(self, {
            let old_locals = self.local_stack.len();
            self.block_depth += 1;
            let mut terminates_early = false;
            for s in &block.body {
                self.pop_stat_locals(block, s, 0);
                self.compile_stat(s);
                if self.always_terminates(s) {
                    terminates_early = true;
                    break;
                }
            }
            self.block_depth -= 1;
            if !terminates_early {
                self.close_locals(old_locals);
            }
            self.pop_locals(old_locals);
        })
    }

    fn compile_stat(&mut self, node: &'a AstStat) {
        self.set_debug_line(node.location());
        if self.options.coverage_level >= 1 && !matches!(node, AstStat::Block(_) | AstStat::TypeAlias(_)) {
            self.bytecode.emit_abc(Opcode::Coverage, 0, 0, 0);
        }

        match node {
            AstStat::Block(b) => self.compile_block_inner(b),
            AstStat::If(s) => self.compile_stat_if(s),
            AstStat::While(s) => self.compile_stat_while(s),
            AstStat::Repeat(s) => self.compile_stat_repeat(s),
            AstStat::Break(_) => {
                let off = self.loops.last().unwrap().local_offset;
                self.close_locals(off);
                let label = self.bytecode.emit_label();
                self.bytecode.emit_ad(Opcode::Jump, 0, 0);
                self.loop_jumps.push(LoopJump { ty: LoopJumpType::Break, label });
            }
            AstStat::Continue(c) => {
                let lp = self.loops.last_mut().unwrap();
                if lp.continue_used.is_none() {
                    lp.continue_used = Some(c.location);
                }
                let off = lp.local_offset_continue;
                self.close_locals(off);
                let label = self.bytecode.emit_label();
                self.bytecode.emit_ad(Opcode::Jump, 0, 0);
                self.loop_jumps.push(LoopJump { ty: LoopJumpType::Continue, label });
            }
            AstStat::Return(s) => {
                if self.options.optimization_level >= 2 && !self.inline_frames.is_empty() {
                    self.compile_inline_return(s);
                } else {
                    self.compile_stat_return(s);
                }
            }
            AstStat::Expr(s) => {
                if let AstExpr::Call(call) = &*s.expr {
                    let target = self.reg_top as u8;
                    self.compile_expr_call(&s.expr, call, target, 0, false, false);
                } else {
                    self.compile_expr_side(&s.expr);
                }
            }
            AstStat::Local(s) => {
                for l in &s.vars {
                    self.check_exported_local(l, s.location);
                }
                self.compile_stat_local(s);
            }
            AstStat::For(s) => self.compile_stat_for(s),
            AstStat::ForIn(s) => self.compile_stat_for_in(s),
            AstStat::Assign(s) => self.compile_stat_assign(s),
            AstStat::CompoundAssign(s) => self.compile_stat_compound_assign(s),
            AstStat::Function(s) => self.compile_stat_function(s),
            AstStat::LocalFunction(s) => {
                let var = match self.try_reuse_local_reg(&s.name) {
                    Some(r) => r,
                    None => self.alloc_reg(s.location, 1),
                };
                self.push_local(&s.name, var, DEFAULT_ALLOC_PC);
                self.set_debug_line(s.func.location);
                self.compile_expr_function(&s.func, var);
                let pc = self.bytecode.get_debug_pc();
                self.locals.get_mut(&local_key(&s.name)).unwrap().debugpc = pc;
            }
            AstStat::TypeAlias(alias) => {
                if alias.exported && self.at_top_level() {
                    self.exports.has_type_exports = true;
                }
            }
            AstStat::TypeFunction(_) | AstStat::DeclareGlobal(_) | AstStat::DeclareFunction(_) | AstStat::DeclareExternType(_) => {}
            AstStat::Class(c) => {
                if !self.options.classes {
                    CompileError::raise(c.location, "Classes are not supported");
                }
                self.compile_class_declaration(c);
            }
            AstStat::Error(e) => CompileError::raise(e.location, "Unexpected error statement"),
        }
    }

    fn validate_continue_until(&mut self, cont: Location, condition: &'a AstExpr, body: &'a AstStatBlock, start: usize) {
        struct V<'c> {
            functions: &'c HashMap<usize, FunctionInfo>,
            locals: HashSet<LocalKey>,
            undef: Option<Arc<AstLocal>>,
        }
        impl V<'_> {
            fn check(&mut self, l: &Arc<AstLocal>) {
                if self.undef.is_none() && self.locals.contains(&local_key(l)) {
                    self.undef = Some(l.clone());
                }
            }
        }
        impl<'z> Visitor<'z> for V<'_> {
            fn visit_expr(&mut self, e: &AstExpr) -> bool {
                match e {
                    AstExpr::Local(l) => {
                        if !l.upvalue {
                            self.check(&l.local);
                        }
                        false
                    }
                    AstExpr::Function(f) => {
                        if let Some(info) = self.functions.get(&key(&**f)) {
                            for uv in info.upvals.clone() {
                                if uv.function_depth == f.function_depth {
                                    self.check(&uv);
                                }
                            }
                        }
                        false
                    }
                    _ => true,
                }
            }
        }
        let mut v = V { functions: &self.functions, locals: HashSet::default(), undef: None };
        for s in &body.body[start..] {
            match &**s {
                AstStat::Local(l) => {
                    for x in &l.vars {
                        v.locals.insert(local_key(x));
                    }
                }
                AstStat::LocalFunction(f) => {
                    v.locals.insert(local_key(&f.name));
                }
                _ => {}
            }
        }
        walk_expr(&mut v, condition);
        if let Some(u) = v.undef {
            CompileError::raise(
                condition.location(),
                format!(
                    "Local {} used in the repeat..until condition is undefined because continue statement on line {} jumps over it",
                    u.name.0,
                    cont.begin.line + 1
                ),
            );
        }
    }

    fn push_local(&mut self, local: &Arc<AstLocal>, reg: u8, allocpc: u32) {
        if self.active_locals as u32 >= MAX_LOCAL_COUNT {
            CompileError::raise(
                local.location,
                format!("Out of local registers when trying to allocate {}: exceeded limit {}", local.name.0, MAX_LOCAL_COUNT),
            );
        }
        self.local_stack.push(local.clone());
        self.active_locals += 1;
        let debugpc = self.bytecode.get_debug_pc();
        let l = self.locals.entry(local_key(local)).or_default();
        l.reg = reg;
        l.allocated = true;
        l.debugpc = debugpc;
        l.allocpc = if allocpc == DEFAULT_ALLOC_PC { debugpc } else { allocpc };
        let captured = l.captured;
        self.reg_captured[reg as usize] = captured;
        self.local_uses[reg as usize] += 1;
    }

    fn are_locals_captured(&self, start: usize) -> bool {
        self.local_stack[start..].iter().any(|l| self.locals.get(&local_key(l)).is_some_and(|x| x.captured))
    }

    fn close_locals(&mut self, start: usize) {
        let mut captured = false;
        let mut capture_reg = 255u8;
        for l in &self.local_stack[start..] {
            let loc = self.locals.get(&local_key(l)).unwrap();
            if loc.captured {
                captured = true;
                capture_reg = capture_reg.min(loc.reg);
            }
        }
        if captured {
            self.bytecode.emit_abc(Opcode::CloseUpvals, capture_reg, 0, 0);
        }
    }

    fn record_local_debug_info(&mut self, ast_local: &Arc<AstLocal>, l: Local, is_argument: bool, endpc: u32) {
        if self.options.debug_level >= 2 {
            self.bytecode.push_debug_local(ast_local.name.0.as_bytes(), l.reg, l.debugpc, endpc);
        }
        if self.options.type_info_level >= 1 && !is_argument {
            let ty = self.local_types.get(&local_key(ast_local)).copied().unwrap_or(crate::types::lbc_type::ANY);
            self.bytecode.push_local_type_info(ty, l.reg, l.allocpc, endpc);
        }
    }

    fn release_local(&mut self, k: LocalKey) -> bool {
        let l = self.locals.get_mut(&k).unwrap();
        l.allocated = false;
        let reg = l.reg as usize;
        self.active_locals -= 1;
        self.reg_captured[reg] = false;
        self.local_uses[reg] -= 1;
        self.local_uses[reg] == 0
    }

    fn pop_locals(&mut self, start: usize) {
        for i in start..self.local_stack.len() {
            let ast_local = self.local_stack[i].clone();
            let k = local_key(&ast_local);
            let l = *self.locals.get(&k).unwrap();
            if l.reused {
                let pos = self.free_local_regs.partition_point(|f| f.reg < l.reg);
                if pos < self.free_local_regs.len() && self.free_local_regs[pos].reg == l.reg {
                    let f = self.free_local_regs.remove(pos);
                    let pc = self.bytecode.get_debug_pc();
                    self.record_local_debug_info(&f.ast_local, f.local, f.is_argument, pc);
                }
                let lm = self.locals.get_mut(&k).unwrap();
                lm.allocated = false;
                lm.reused = false;
            } else if l.allocated {
                self.release_local(k);
                let pc = self.bytecode.get_debug_pc();
                let l = *self.locals.get(&k).unwrap();
                self.record_local_debug_info(&ast_local, l, i < self.arg_count, pc);
            }
        }
        self.local_stack.truncate(start);
    }

    fn pop_stat_locals(&mut self, owner: &AstStatBlock, before: &AstStat, start: usize) {
        if self.options.optimization_level == 0 {
            return;
        }
        let owner_key = key(owner);
        for i in start..self.local_stack.len() {
            let ast_local = self.local_stack[i].clone();
            let k = local_key(&ast_local);
            let l = *self.locals.get(&k).unwrap();
            if !l.allocated || l.reused || l.captured {
                continue;
            }
            let Some(var) = self.variables.get(&k) else { continue };
            if var.owner != owner_key || var.non_lexical_use {
                continue;
            }
            let stat_start = before.location().begin;
            let released = match var.last_used {
                None => true,
                Some(end) => stat_start.line > end.line || (stat_start.line == end.line && stat_start.column > end.column),
            };
            if released {
                if self.release_local(k) {
                    let l = *self.locals.get(&k).unwrap();
                    let free = FreeLocal { reg: l.reg, local: l, ast_local: ast_local.clone(), is_argument: i < self.arg_count };
                    let pos = self.free_local_regs.partition_point(|f| f.reg < l.reg);
                    self.free_local_regs.insert(pos, free);
                    self.locals.get_mut(&k).unwrap().reused = true;
                } else {
                    let pc = self.bytecode.get_debug_pc();
                    let l = *self.locals.get(&k).unwrap();
                    self.record_local_debug_info(&ast_local, l, i < self.arg_count, pc);
                }
            }
        }
    }

    fn patch_jump(&mut self, loc: Location, label: usize, target: usize) {
        if !self.bytecode.patch_jump_d(label, target) {
            jump_limit(loc);
        }
    }

    fn patch_jumps(&mut self, loc: Location, labels: &[usize], target: usize) {
        for &l in labels {
            self.patch_jump(loc, l, target);
        }
    }

    fn patch_loop_jumps(&mut self, loc: Location, old_jumps: usize, end_label: usize, cont_label: usize) {
        for i in old_jumps..self.loop_jumps.len() {
            let lj = self.loop_jumps[i];
            match lj.ty {
                LoopJumpType::Break => self.patch_jump(loc, lj.label, end_label),
                LoopJumpType::Continue => self.patch_jump(loc, lj.label, cont_label),
            }
        }
    }

    fn alloc_reg(&mut self, loc: Location, count: u32) -> u8 {
        let top = self.reg_top;
        if top + count > MAX_REGISTER_COUNT {
            CompileError::raise(
                loc,
                format!("Out of registers when trying to allocate {} registers: exceeded limit {}", count, MAX_REGISTER_COUNT),
            );
        }
        self.reg_top += count;
        self.stack_size = self.stack_size.max(self.reg_top);
        top as u8
    }

    fn try_reuse_local_reg(&mut self, local: &Arc<AstLocal>) -> Option<u8> {
        if local.is_exported || self.free_local_regs.is_empty() {
            return None;
        }
        self.variables.get(&local_key(local))?;
        if self.locals.get(&local_key(local)).is_some_and(|l| l.captured) {
            return None;
        }
        let free = self.free_local_regs.pop().unwrap();
        let pc = self.bytecode.get_debug_pc();
        self.record_local_debug_info(&free.ast_local, free.local, free.is_argument, pc);
        Some(free.reg)
    }

    fn set_debug_line(&mut self, loc: Location) {
        if self.options.debug_level >= 1 {
            self.bytecode.set_debug_line(loc.begin.line as i32 + 1);
        }
    }

    fn set_debug_line_end(&mut self, loc: Location) {
        if self.options.debug_level >= 1 {
            self.bytecode.set_debug_line(loc.end.line as i32 + 1);
        }
    }
}

// ---------------------------------------------------------------------------
// Driver (`compileOrThrow`)
// ---------------------------------------------------------------------------

/// Collects every function in post-order: nested functions come before the
/// functions that reference them.
fn collect_functions<'a>(root: &'a AstStatBlock) -> Vec<&'a AstExprFunction> {
    struct V<'a> {
        out: Vec<&'a AstExprFunction>,
    }
    impl<'a> Visitor<'a> for V<'a> {
        fn visit_function(&mut self, f: &'a AstExprFunction) -> bool {
            walk_block(self, &f.body);
            self.out.push(f);
            false
        }
        fn visit_stat(&mut self, s: &'a AstStat) -> bool {
            !matches!(s, AstStat::TypeFunction(_))
        }
    }
    let mut v = V { out: Vec::new() };
    walk_block(&mut v, root);
    v.out
}

type ClassMaps<'a> = (HashMap<usize, Option<&'a AstStatClass>>, HashMap<usize, HashMap<&'static str, (u8, bool)>>);

/// `ClassVisitor`: the class enclosing every function, and the private
/// member slots of every class.
fn class_visitor(root: &AstStatBlock) -> ClassMaps<'_> {
    struct V<'a> {
        enclosing: HashMap<usize, Option<&'a AstStatClass>>,
        offsets: HashMap<usize, HashMap<&'static str, (u8, bool)>>,
        curr: Option<&'a AstStatClass>,
    }
    impl<'a> Visitor<'a> for V<'a> {
        fn visit_stat(&mut self, s: &'a AstStat) -> bool {
            let AstStat::Class(cls) = s else { return true };
            let cls: &'a AstStatClass = cls;
            self.curr = Some(cls);
            let mut private = HashMap::default();
            let (mut ninstance, mut nstatic) = (0u8, 0u8);
            for m in &cls.members {
                match m {
                    AstClassMember::Property(p) => {
                        if p.access == AstClassAccess::Private {
                            private.insert(p.name.0, (ninstance, false));
                            ninstance += 1;
                        }
                    }
                    AstClassMember::Method(m) => {
                        if m.access == AstClassAccess::Private {
                            private.insert(m.function_name.0, (nstatic, true));
                            nstatic += 1;
                        }
                        walk_function(self, &m.function);
                    }
                }
            }
            if !private.is_empty() {
                self.offsets.insert(key(cls), private);
            }
            self.curr = None;
            false
        }
        fn visit_function(&mut self, f: &'a AstExprFunction) -> bool {
            self.enclosing.insert(key(f), self.curr);
            true
        }
    }
    let mut v = V { enclosing: HashMap::default(), offsets: HashMap::default(), curr: None };
    walk_block(&mut v, root);
    (v.enclosing, v.offsets)
}

fn uses_fenv(root: &AstStatBlock) -> (bool, bool) {
    struct V {
        get: bool,
        set: bool,
    }
    impl<'z> Visitor<'z> for V {
        fn visit_expr(&mut self, e: &AstExpr) -> bool {
            if let AstExpr::Global(g) = e {
                if g.name.0 == "getfenv" {
                    self.get = true;
                }
                if g.name.0 == "setfenv" {
                    self.set = true;
                }
                return false;
            }
            true
        }
    }
    let mut v = V { get: false, set: false };
    walk_block(&mut v, root);
    (v.get, v.set)
}

fn compile_or_throw(
    root: &AstStatBlock,
    hot_comments: &[HotComment],
    mut options: CompileOptions,
    bcb: BytecodeBuilder,
) -> (Vec<u8>, BytecodeBuilder) {
    let mut main_flags: u8 = 0;
    for hc in hot_comments.iter().filter(|h| h.header) {
        if let Some(level) = hc.content.strip_prefix("optimize ") {
            let level: i64 = level.trim().parse().unwrap_or(0);
            options.optimization_level = level.clamp(0, 2) as u8;
        }
        if hc.content == "native" {
            main_flags |= proto_flag::NATIVE_MODULE as u8;
            // `setCompileOptionsForNativeCompilation`
            options.optimization_level = 2;
            options.type_info_level = 1;
        }
    }
    let functions = collect_functions(root);
    if functions.iter().any(|f| f.attributes.iter().any(|a| a.kind == AstAttrKind::Native)) {
        options.optimization_level = 2;
        options.type_info_level = 1;
    }

    let mut c = Compiler::new(options.clone());
    c.bytecode = bcb;
    // `assignMutable`: `_G` contents are unknown at compile time
    c.globals.insert("_G", Global::Mutable);

    {
        let mut exported_functions = std::mem::take(&mut c.exports.exported_functions);
        let mut exported_variables = std::mem::take(&mut c.exports.exported_variables);
        ValueVisitor::run(&mut c.globals, &mut c.variables, &mut exported_functions, &mut exported_variables, root);
        c.exports.exported_functions = exported_functions;
        c.exports.exported_variables = exported_variables;
    }

    if options.optimization_level >= 1 {
        let (g, s) = uses_fenv(root);
        c.getfenv_used = g;
        c.setfenv_used = s;
    }

    // builtin folding is enabled on optimization level 2 since we can't de-optimize folding at runtime
    if options.optimization_level >= 2 && !c.getfenv_used && !c.setfenv_used {
        c.builtins_fold = true;
        if global_state(&c.globals, "math") == Global::Default {
            c.builtins_fold_library_k = true;
        }
    }

    if options.optimization_level >= 1 {
        c.builtins = analyze_builtins(root, &c.globals, &c.variables);
        c.table_constants = build_table_constant_map(&c.variables, root);
        ConstantFolder::run(
            &mut c.constants,
            &mut c.variables,
            &mut c.locstants,
            if c.builtins_fold { Some(&c.builtins) } else { None },
            c.builtins_fold_library_k,
            options.vector_double,
            &c.table_constants,
            root,
        );
        c.table_shapes = predict_table_shapes(root);
    }

    // `setCompileOptionsForNativeCompilation` (type info part): native
    // functions get argument type guards.
    let has_native = functions.iter().any(|f| f.attributes.iter().any(|a| a.kind == AstAttrKind::Native));
    if options.type_info_level >= 1 || options.optimization_level >= 2 || has_native {
        let tm = crate::types::build_type_map(root, &c.builtins, &c.globals);
        c.function_types = tm.function_types;
        c.local_types = tm.local_types;
        c.expr_types = tm.expr_types;
    }

    c.bytecode.classes = options.classes;
    if options.classes {
        let (enclosing, offsets) = class_visitor(root);
        c.enclosing_classes = enclosing;
        c.class_private_field_offsets = offsets;
    }

    for f in functions {
        let mut protoflags = 0u8;
        if options.classes {
            c.curr_class = c.enclosing_classes.get(&key(f)).copied().flatten();
        }
        c.compile_function(FuncRef::of(f), &mut protoflags);
        c.curr_class = None;
        if protoflags & proto_flag::NATIVE_FUNCTION as u8 != 0 && main_flags & proto_flag::NATIVE_MODULE as u8 == 0 {
            main_flags |= proto_flag::NATIVE_FUNCTION as u8;
        }
    }

    let main = FuncRef {
        key: 0,
        self_local: None,
        args: &[],
        vararg: true,
        body: root,
        depth: 0,
        debugname: "",
        location: root.location,
        native: false,
    };
    let mainid = c.compile_function(main, &mut main_flags);
    c.bytecode.set_main_function(mainid);
    let bytes = c.bytecode.finalize();
    (bytes, std::mem::take(&mut c.bytecode))
}

/// Compiles a parsed chunk to serialized bytecode (`Luau::compile`),
/// honouring its `--!optimize` / `--!native` hot comments.
pub fn compile_parsed(parsed: &ParseResult, options: CompileOptions) -> Result<Vec<u8>, CompileError> {
    luau_common::fasthash::set_size_hint(parsed.size_hint);
    let r = compile_impl(&parsed.root, &parsed.hot_comments, options);
    luau_common::fasthash::set_size_hint(0);
    r
}

/// `Luau::compile(source, options)`: parses and compiles a chunk; syntax
/// and compile errors come back as error bytecode (`BytecodeBuilder::getError`,
/// a zero version byte followed by `:line: message`) that `luau_load`
/// reports as a load failure.
pub fn compile_source(source: &[u8], options: CompileOptions) -> Vec<u8> {
    use luau_common::fastalloc;
    // The AST (and name table) are bump-allocated in a scoped arena while
    // parsing; afterwards they are released in one step instead of being
    // freed node by node (like the C++ parser's arena allocator).
    struct ArenaScope(bool);
    impl Drop for ArenaScope {
        fn drop(&mut self) {
            fastalloc::arena_end();
        }
    }
    let arena = ArenaScope(fastalloc::arena_begin());
    let names = luau_ast::name_table::AstNameTable::new();
    let mut names = names;
    let parsed = luau_ast::parser::parse(source, &mut names);
    fastalloc::arena_end();
    let result = if let Some(e) = parsed.errors.first() {
        Err(format!(":{}: {}", e.location.begin.line + 1, e.message))
    } else {
        compile_parsed(&parsed, options).map_err(|e| e.to_string())
    };
    if arena.0 {
        // everything the parse allocated lives in the arena: skip the
        // per-node drops and release it wholesale
        std::mem::forget(parsed);
        std::mem::forget(names);
        // SAFETY: the AST and name table were forgotten above, and nothing
        // that outlives this call (the bytecode, error text) was allocated
        // while the arena was on
        unsafe { fastalloc::arena_reset() };
    } else {
        drop(parsed);
        drop(names);
    }
    drop(arena);
    match result {
        Ok(bytes) => bytes,
        Err(error) => {
            let mut out = vec![0u8];
            out.extend_from_slice(error.as_bytes());
            out
        }
    }
}

/// Compiles a chunk body to serialized bytecode.
pub fn compile(root: &AstStatBlock, options: CompileOptions) -> Result<Vec<u8>, CompileError> {
    compile_impl(root, &[], options)
}

fn compile_impl(root: &AstStatBlock, hot_comments: &[HotComment], options: CompileOptions) -> Result<Vec<u8>, CompileError> {
    compile_impl_with(root, hot_comments, options, BytecodeBuilder::new()).map(|r| r.0)
}

/// Like [`compile_parsed`], but compiles into a caller-configured builder
/// (dump flags, dump source) and hands it back for text dumping
/// (`Luau::compileOrThrow(BytecodeBuilder&, ...)`).
pub fn compile_parsed_with(
    parsed: &ParseResult,
    options: CompileOptions,
    bcb: BytecodeBuilder,
) -> Result<(Vec<u8>, BytecodeBuilder), CompileError> {
    luau_common::fasthash::set_size_hint(parsed.size_hint);
    let r = compile_impl_with(&parsed.root, &parsed.hot_comments, options, bcb);
    luau_common::fasthash::set_size_hint(0);
    r
}

fn compile_impl_with(
    root: &AstStatBlock,
    hot_comments: &[HotComment],
    options: CompileOptions,
    bcb: BytecodeBuilder,
) -> Result<(Vec<u8>, BytecodeBuilder), CompileError> {
    install_quiet_hook();
    let leak = |s: &Option<String>| s.as_ref().map(|s| &*Box::leak(s.clone().into_boxed_str()));
    let hv = crate::analysis::HostVector { lib: leak(&options.vector_lib), ctor: leak(&options.vector_ctor), ty: leak(&options.vector_type) };
    let has_hv = hv.lib.is_some() || hv.ctor.is_some() || hv.ty.is_some();
    if has_hv {
        crate::analysis::HOST_VECTOR.with(|h| *h.borrow_mut() = hv);
    }
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| compile_or_throw(root, hot_comments, options, bcb)));
    if has_hv {
        crate::analysis::HOST_VECTOR.with(|h| *h.borrow_mut() = Default::default());
    }
    match r {
        Ok(r) => Ok(r),
        Err(payload) => match payload.downcast::<CompileError>() {
            Ok(e) => Err(*e),
            Err(other) => std::panic::resume_unwind(other),
        },
    }
}
