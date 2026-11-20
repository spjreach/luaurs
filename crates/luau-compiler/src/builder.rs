//! Port of `Bytecode/src/BytecodeBuilder.cpp` (emission, constant pools,
//! jump patching/folding/expansion, serialization). Text dumping lives in
//! the `dump` submodule.
//!
//! The configuration matches the reference CLI flags (`Luau*` flags on):
//! bytecode version 14, type encoding version 3, call feedback slots.

#[path = "dump.rs"]
pub mod dump;

use luau_common::fasthash::HashMap;

use luau_common::bytecode::{get_op_length, insn, Opcode};

pub const MAX_REGISTER_COUNT: u32 = 255;
pub const MAX_UPVALUE_COUNT: u32 = 200;
pub const MAX_LOCAL_COUNT: u32 = 200;
pub const MAX_INSTRUCTION_COUNT: u32 = 1_000_000_000;
pub const INVALID_REG: u8 = 255;
pub const MAX_CONSTANT_COUNT: u32 = 1 << 23;
pub const MAX_CLOSURE_COUNT: u32 = 1 << 15;
pub const MAX_JUMP_DISTANCE: i32 = 1 << 23;

const VERSION: u8 = 14;
const TYPES_VERSION: u8 = 3;

// LBC_CONSTANT_*
const K_NIL: u8 = 0;
const K_BOOLEAN: u8 = 1;
const K_NUMBER: u8 = 2;
const K_STRING: u8 = 3;
const K_IMPORT: u8 = 4;
const K_TABLE: u8 = 5;
const K_CLOSURE: u8 = 6;
const K_VECTOR: u8 = 7;
const K_TABLE_WITH_CONSTANTS: u8 = 8;
const K_INTEGER: u8 = 9;
const K_VECTORD: u8 = 11;
const K_CLASS_SHAPE: u8 = 10;
/// `LBC_VERSION_CLASSES`.
const VERSION_CLASSES: u8 = 100;

/// `LPF_INLINABLE`.
pub const LPF_INLINABLE: u8 = 1 << 3;

fn write_var_int(ss: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 127) as u8;
        value >>= 7;
        if value == 0 {
            ss.push(byte);
            break;
        }
        ss.push(byte | 128);
    }
}

fn write_int(ss: &mut Vec<u8>, v: i32) {
    ss.extend_from_slice(&v.to_le_bytes());
}

fn log2(v: i32) -> i32 {
    let mut r = 0;
    while v >= (2 << r) {
        r += 1;
    }
    r
}

/// `BytecodeBuilder::TableShape`: keys (string constants) of a template
/// table, optionally with constant values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct TableShape {
    pub keys: Vec<i32>,
    pub constants: Vec<i32>,
    pub has_constants: bool,
}

impl TableShape {
    pub const MAX_LENGTH: usize = 32;
}

#[derive(Debug, Clone, Copy)]
enum Constant {
    Nil,
    Boolean(bool),
    Number(f64),
    Integer(i64),
    Vectorf([f32; 4]),
    Vectord([f64; 4]),
    String(u32),
    Import(u32),
    Table(u32),
    Closure(u32),
    ClassShape(u32),
}

/// `BytecodeBuilder::ClassShape`.
#[derive(Debug, Clone, Default)]
pub struct ClassShape {
    pub class_name: i32,
    pub num_private_instance_props: u8,
    pub num_private_static_props: u8,
    pub property_names: Vec<i32>,
    pub method_names: Vec<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ConstantKey {
    Nil,
    Boolean(bool),
    Number(u64),
    Integer(i64),
    Vectorf([u32; 4]),
    Vectord([u64; 4]),
    String(u32),
    Import(u32),
    Closure(u32),
}

#[derive(Default)]
struct Function {
    data: Vec<u8>,
    dump: String,
    dumpname: Vec<u8>,
    maxstacksize: u8,
    numparams: u8,
    numupvalues: u8,
    isvararg: bool,
    debugname: u32,
    debuglinedefined: i32,
}

#[derive(Clone, Copy)]
struct DebugLocal {
    name: u32,
    reg: u8,
    startpc: u32,
    endpc: u32,
}

#[derive(Clone, Copy)]
struct Jump {
    source: u32,
    target: u32,
}

#[derive(Clone, Copy)]
enum FeedbackSlot {
    CallTarget(u32),
    Construct(u32),
}

#[derive(Default)]
pub struct BytecodeBuilder {
    functions: Vec<Function>,
    current_function: Option<u32>,
    main_function: u32,
    total_instruction_count: usize,

    insns: Vec<u32>,
    lines: Vec<i32>,
    constants: Vec<Constant>,
    protos: Vec<u32>,
    jumps: Vec<Jump>,
    table_shapes: Vec<TableShape>,
    class_shapes: Vec<ClassShape>,
    fb_slots: Vec<FeedbackSlot>,
    /// `DebugLuauUserDefinedClasses`: emits `LBC_VERSION_CLASSES` bytecode.
    pub classes: bool,
    has_long_jumps: bool,

    constant_map: HashMap<ConstantKey, i32>,
    table_shape_map: HashMap<TableShape, i32>,
    proto_map: HashMap<u32, i16>,

    debug_line: i32,
    debug_locals: Vec<DebugLocal>,
    debug_upvals: Vec<u32>,
    /// `Function::typeinfo` of the function being built.
    typeinfo: Vec<u8>,
    /// `typedLocals`: (type, reg, startpc, endpc)
    typed_locals: Vec<(u8, u8, u32, u32)>,
    /// `typedUpvals`
    typed_upvals: Vec<u8>,

    string_table: HashMap<Vec<u8>, u32>,
    strings: Vec<Vec<u8>>,

    bytecode: Vec<u8>,

    dump_flags: u32,
    dump_source: Vec<Vec<u8>>,
    /// (instruction index, text) for the function being built
    debug_remarks: Vec<(u32, String)>,
    /// (source line, text) over the whole module
    dump_remarks: Vec<(i32, String)>,
}

impl BytecodeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// `setFunctionTypeInfo`.
    pub fn set_function_type_info(&mut self, value: Vec<u8>) {
        self.typeinfo = value;
    }

    pub fn begin_function(&mut self, numparams: u8, isvararg: bool) -> u32 {
        debug_assert!(self.current_function.is_none());
        let id = self.functions.len() as u32;
        self.functions.push(Function { numparams, isvararg, ..Default::default() });
        self.current_function = Some(id);
        self.has_long_jumps = false;
        self.debug_line = 0;
        id
    }

    fn clear_state(&mut self) {
        self.insns.clear();
        self.lines.clear();
        self.constants.clear();
        self.protos.clear();
        self.jumps.clear();
        self.fb_slots.clear();
        self.table_shapes.clear();
        self.class_shapes.clear();
        self.debug_locals.clear();
        self.debug_upvals.clear();
        self.typeinfo.clear();
        self.typed_locals.clear();
        self.typed_upvals.clear();
        self.constant_map.clear();
        self.table_shape_map.clear();
        self.proto_map.clear();
        self.debug_remarks.clear();
    }

    pub fn end_function(&mut self, maxstacksize: u8, numupvalues: u8, flags: u8, cost: u64) {
        let id = self.current_function.expect("no current function") as usize;
        self.functions[id].maxstacksize = maxstacksize;
        self.functions[id].numupvalues = numupvalues;
        if self.dump_flags != 0 {
            self.functions[id].dump = self.dump_current_function();
        }
        let mut data = Vec::with_capacity(32 + self.insns.len() * 7);
        self.write_function(&mut data, id, flags, cost);
        self.functions[id].data = data;
        self.current_function = None;
        self.total_instruction_count += self.insns.len();
        self.clear_state();
    }

    pub fn set_main_function(&mut self, fid: u32) {
        self.main_function = fid;
    }

    fn add_constant(&mut self, key: ConstantKey, value: Constant) -> i32 {
        if let Some(&id) = self.constant_map.get(&key) {
            return id;
        }
        let id = self.constants.len() as u32;
        if id >= MAX_CONSTANT_COUNT {
            return -1;
        }
        self.constant_map.insert(key, id as i32);
        self.constants.push(value);
        id as i32
    }

    /// 1-based string table index (0 means nil in the serialization format).
    fn add_string_table_entry(&mut self, value: &[u8]) -> u32 {
        if let Some(&i) = self.string_table.get(value) {
            return i;
        }
        self.strings.push(value.to_vec());
        let index = self.strings.len() as u32;
        self.string_table.insert(value.to_vec(), index);
        index
    }

    pub fn add_constant_nil(&mut self) -> i32 {
        self.add_constant(ConstantKey::Nil, Constant::Nil)
    }

    pub fn add_constant_boolean(&mut self, v: bool) -> i32 {
        self.add_constant(ConstantKey::Boolean(v), Constant::Boolean(v))
    }

    pub fn add_constant_number(&mut self, v: f64) -> i32 {
        self.add_constant(ConstantKey::Number(v.to_bits()), Constant::Number(v))
    }

    pub fn add_constant_integer(&mut self, v: i64) -> i32 {
        self.add_constant(ConstantKey::Integer(v), Constant::Integer(v))
    }

    pub fn add_constant_vectorf(&mut self, x: f32, y: f32, z: f32, w: f32) -> i32 {
        let key = ConstantKey::Vectorf([x.to_bits(), y.to_bits(), z.to_bits(), w.to_bits()]);
        self.add_constant(key, Constant::Vectorf([x, y, z, w]))
    }

    pub fn add_constant_vectord(&mut self, x: f64, y: f64, z: f64, w: f64) -> i32 {
        let key = ConstantKey::Vectord([x.to_bits(), y.to_bits(), z.to_bits(), w.to_bits()]);
        self.add_constant(key, Constant::Vectord([x, y, z, w]))
    }

    pub fn add_constant_string(&mut self, s: &[u8]) -> i32 {
        let index = self.add_string_table_entry(s);
        self.add_constant(ConstantKey::String(index), Constant::String(index))
    }

    pub fn add_import(&mut self, iid: u32) -> i32 {
        self.add_constant(ConstantKey::Import(iid), Constant::Import(iid))
    }

    pub fn add_constant_table(&mut self, shape: TableShape) -> i32 {
        if let Some(&id) = self.table_shape_map.get(&shape) {
            return id;
        }
        let id = self.constants.len() as u32;
        if id >= MAX_CONSTANT_COUNT {
            return -1;
        }
        let index = self.table_shapes.len() as u32;
        self.table_shape_map.insert(shape.clone(), id as i32);
        self.table_shapes.push(shape);
        self.constants.push(Constant::Table(index));
        id as i32
    }

    pub fn add_constant_closure(&mut self, fid: u32) -> i32 {
        self.add_constant(ConstantKey::Closure(fid), Constant::Closure(fid))
    }

    pub fn add_class_shape(&mut self, shape: ClassShape) -> i32 {
        let id = self.constants.len() as u32;
        if id >= MAX_CONSTANT_COUNT {
            return -1;
        }
        let index = self.class_shapes.len() as u32;
        self.class_shapes.push(shape);
        self.constants.push(Constant::ClassShape(index));
        id as i32
    }

    pub fn add_construct_slot(&mut self, shapecid: u32) -> u32 {
        self.fb_slots.push(FeedbackSlot::Construct(shapecid));
        (self.fb_slots.len() - 1) as u32
    }

    /// `patchAux`: overwrites the AUX word emitted at `offset`.
    pub fn patch_aux(&mut self, offset: usize, aux: u32) {
        self.insns[offset] = aux;
    }

    pub fn add_call_target_slot(&mut self, pc: u32) -> u32 {
        self.fb_slots.push(FeedbackSlot::CallTarget(pc));
        (self.fb_slots.len() - 1) as u32
    }

    pub fn add_child_function(&mut self, fid: u32) -> i16 {
        if let Some(&id) = self.proto_map.get(&fid) {
            return id;
        }
        let id = self.protos.len() as u32;
        if id >= MAX_CLOSURE_COUNT {
            return -1;
        }
        self.proto_map.insert(fid, id as i16);
        self.protos.push(fid);
        id as i16
    }

    pub fn emit_abc(&mut self, op: Opcode, a: u8, b: u8, c: u8) {
        self.insns.push(op as u32 | (a as u32) << 8 | (b as u32) << 16 | (c as u32) << 24);
        self.lines.push(self.debug_line);
    }

    pub fn emit_ad(&mut self, op: Opcode, a: u8, d: i16) {
        self.insns.push(op as u32 | (a as u32) << 8 | ((d as u16) as u32) << 16);
        self.lines.push(self.debug_line);
    }

    pub fn emit_e(&mut self, op: Opcode, e: i32) {
        self.insns.push(op as u32 | (e as u32) << 8);
        self.lines.push(self.debug_line);
    }

    pub fn emit_aux(&mut self, aux: u32) {
        self.insns.push(aux);
        self.lines.push(self.debug_line);
    }

    /// Removes the last instruction (which must be `op`), adjusting debug
    /// local ranges that referenced it.
    pub fn undo_emit(&mut self, op: Opcode) {
        debug_assert_eq!(*self.insns.last().unwrap() & 0xff, op as u32);
        let n = self.insns.len() as u32;
        let mut i = 0;
        while i < self.debug_locals.len() {
            let l = &mut self.debug_locals[i];
            if l.startpc == n {
                self.debug_locals.remove(i);
                continue;
            }
            if l.endpc == n {
                l.endpc -= 1;
            }
            i += 1;
        }
        let mut i = 0;
        while i < self.typed_locals.len() {
            let l = &mut self.typed_locals[i];
            if l.2 == n {
                self.typed_locals.remove(i);
                continue;
            }
            if l.3 == n {
                l.3 -= 1;
            }
            i += 1;
        }
        self.insns.pop();
        self.lines.pop();
    }

    /// `pushLocalTypeInfo`.
    pub fn push_local_type_info(&mut self, ty: u8, reg: u8, startpc: u32, endpc: u32) {
        self.typed_locals.push((ty, reg, startpc, endpc));
    }

    /// `pushUpvalTypeInfo`.
    pub fn push_upval_type_info(&mut self, ty: u8) {
        self.typed_upvals.push(ty);
    }

    pub fn last_instruction(&self) -> u32 {
        *self.insns.last().unwrap()
    }

    pub fn emit_label(&self) -> usize {
        self.insns.len()
    }

    #[must_use]
    pub fn patch_jump_d(&mut self, jump_label: usize, target_label: usize) -> bool {
        let offset = target_label as i32 - jump_label as i32 - 1;
        if offset as i16 as i32 == offset {
            self.insns[jump_label] |= ((offset as i16 as u16) as u32) << 16;
        } else if offset.abs() < MAX_JUMP_DISTANCE {
            self.has_long_jumps = true;
        } else {
            return false;
        }
        self.jumps.push(Jump { source: jump_label as u32, target: target_label as u32 });
        true
    }

    #[must_use]
    pub fn patch_skip_c(&mut self, jump_label: usize, target_label: usize) -> bool {
        let offset = target_label as i32 - jump_label as i32 - 1;
        if offset as u8 as i32 != offset {
            return false;
        }
        self.insns[jump_label] |= (offset as u32) << 24;
        true
    }

    pub fn set_debug_function_name(&mut self, name: &[u8]) {
        let index = self.add_string_table_entry(name);
        let id = self.current_function.unwrap() as usize;
        self.functions[id].debugname = index;
        if self.dump_flags != 0 {
            self.functions[id].dumpname = name.to_vec();
        }
    }

    pub fn set_debug_function_line_defined(&mut self, line: i32) {
        let id = self.current_function.unwrap() as usize;
        self.functions[id].debuglinedefined = line;
    }

    pub fn set_debug_line(&mut self, line: i32) {
        self.debug_line = line;
    }

    pub fn push_debug_local(&mut self, name: &[u8], reg: u8, startpc: u32, endpc: u32) {
        let index = self.add_string_table_entry(name);
        self.debug_locals.push(DebugLocal { name: index, reg, startpc, endpc });
    }

    pub fn push_debug_upval(&mut self, name: &[u8]) {
        let index = self.add_string_table_entry(name);
        self.debug_upvals.push(index);
    }

    pub fn get_instruction_count(&self) -> usize {
        self.insns.len()
    }

    pub fn get_total_instruction_count(&self) -> usize {
        self.total_instruction_count
    }

    pub fn get_debug_pc(&self) -> u32 {
        self.insns.len() as u32
    }

    pub fn finalize(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(if self.classes { VERSION_CLASSES } else { VERSION });
        out.push(TYPES_VERSION);
        // string table
        write_var_int(&mut out, self.strings.len() as u64);
        for s in &self.strings {
            write_var_int(&mut out, s.len() as u64);
            out.extend_from_slice(s);
        }
        // userdata type mapping (none) terminator
        out.push(0);
        write_var_int(&mut out, self.functions.len() as u64);
        for f in &self.functions {
            write_var_int(&mut out, f.data.len() as u64);
            out.extend_from_slice(&f.data);
        }
        write_var_int(&mut out, self.main_function as u64);
        self.bytecode = out.clone();
        out
    }

    fn write_function(&self, ss: &mut Vec<u8>, id: usize, flags: u8, cost: u64) {
        let func = &self.functions[id];
        ss.push(func.maxstacksize);
        ss.push(func.numparams);
        ss.push(func.numupvalues);
        ss.push(func.isvararg as u8);
        ss.push(flags);
        if self.typeinfo.is_empty() && self.typed_upvals.is_empty() && self.typed_locals.is_empty() {
            write_var_int(ss, 0);
        } else {
            // sizes of the function / upvalue / local type sections, then data
            let mut temp = Vec::with_capacity(self.typeinfo.len() + 3);
            write_var_int(&mut temp, self.typeinfo.len() as u64);
            write_var_int(&mut temp, self.typed_upvals.len() as u64);
            write_var_int(&mut temp, self.typed_locals.len() as u64);
            temp.extend_from_slice(&self.typeinfo);
            temp.extend_from_slice(&self.typed_upvals);
            for &(ty, reg, startpc, endpc) in &self.typed_locals {
                temp.push(ty);
                temp.push(reg);
                write_var_int(&mut temp, startpc as u64);
                write_var_int(&mut temp, (endpc - startpc) as u64);
            }
            write_var_int(ss, temp.len() as u64);
            ss.extend_from_slice(&temp);
        }

        write_var_int(ss, self.insns.len() as u64);
        for &i in &self.insns {
            write_int(ss, i as i32);
        }

        write_var_int(ss, self.constants.len() as u64);
        for c in &self.constants {
            match *c {
                Constant::Nil => ss.push(K_NIL),
                Constant::Boolean(b) => {
                    ss.push(K_BOOLEAN);
                    ss.push(b as u8);
                }
                Constant::Number(n) => {
                    ss.push(K_NUMBER);
                    ss.extend_from_slice(&n.to_le_bytes());
                }
                Constant::Integer(v) => {
                    ss.push(K_INTEGER);
                    if v < 0 {
                        ss.push(1);
                        write_var_int(ss, (!(v as u64)).wrapping_add(1));
                    } else {
                        ss.push(0);
                        write_var_int(ss, v as u64);
                    }
                }
                Constant::Vectorf(v) => {
                    ss.push(K_VECTOR);
                    for x in v {
                        ss.extend_from_slice(&x.to_le_bytes());
                    }
                }
                Constant::Vectord(v) => {
                    ss.push(K_VECTORD);
                    for x in v {
                        ss.extend_from_slice(&x.to_le_bytes());
                    }
                }
                Constant::String(i) => {
                    ss.push(K_STRING);
                    write_var_int(ss, i as u64);
                }
                Constant::Import(i) => {
                    ss.push(K_IMPORT);
                    write_int(ss, i as i32);
                }
                Constant::Table(t) => {
                    let shape = &self.table_shapes[t as usize];
                    if shape.has_constants {
                        ss.push(K_TABLE_WITH_CONSTANTS);
                        write_var_int(ss, shape.keys.len() as u64);
                        for (k, c) in shape.keys.iter().zip(&shape.constants) {
                            write_var_int(ss, *k as u64);
                            write_int(ss, *c);
                        }
                    } else {
                        ss.push(K_TABLE);
                        write_var_int(ss, shape.keys.len() as u64);
                        for k in &shape.keys {
                            write_var_int(ss, *k as u64);
                        }
                    }
                }
                Constant::Closure(f) => {
                    ss.push(K_CLOSURE);
                    write_var_int(ss, f as u64);
                }
                Constant::ClassShape(c) => {
                    let cs = &self.class_shapes[c as usize];
                    ss.push(K_CLASS_SHAPE);
                    write_var_int(ss, cs.class_name as u64);
                    ss.push(cs.num_private_instance_props);
                    ss.push(cs.num_private_static_props);
                    write_var_int(ss, (cs.property_names.len() + cs.num_private_instance_props as usize) as u64);
                    write_var_int(ss, (cs.method_names.len() + cs.num_private_static_props as usize) as u64);
                    for &n in &cs.property_names {
                        write_var_int(ss, n as u64);
                    }
                    for &n in &cs.method_names {
                        write_var_int(ss, n as u64);
                    }
                }
            }
        }

        write_var_int(ss, self.protos.len() as u64);
        for &p in &self.protos {
            write_var_int(ss, p as u64);
        }

        write_var_int(ss, func.debuglinedefined as u32 as u64);
        write_var_int(ss, func.debugname as u64);

        let has_lines = !self.lines.iter().any(|&l| l == 0);
        if has_lines {
            ss.push(1);
            self.write_line_info(ss);
        } else {
            ss.push(0);
        }

        if !self.debug_locals.is_empty() || !self.debug_upvals.is_empty() {
            ss.push(1);
            write_var_int(ss, self.debug_locals.len() as u64);
            for l in &self.debug_locals {
                write_var_int(ss, l.name as u64);
                write_var_int(ss, l.startpc as u64);
                write_var_int(ss, l.endpc as u64);
                ss.push(l.reg);
            }
            write_var_int(ss, self.debug_upvals.len() as u64);
            for &u in &self.debug_upvals {
                write_var_int(ss, u as u64);
            }
        } else {
            ss.push(0);
        }

        // feedback slots (`LuauEmitCallFeedback`, `LuauCompileRefactorFeedback`)
        write_var_int(ss, self.fb_slots.len() as u64);
        for slot in &self.fb_slots {
            match *slot {
                FeedbackSlot::CallTarget(pc) => {
                    ss.push(0);
                    write_var_int(ss, pc as u64);
                }
                FeedbackSlot::Construct(shape) => {
                    ss.push(1);
                    write_var_int(ss, shape as u64);
                }
            }
        }

        if flags & LPF_INLINABLE != 0 {
            write_var_int(ss, cost);
        }
    }

    fn calc_lines_span(&self) -> i32 {
        let mut span: i32 = 1 << 24;
        let mut offset = 0usize;
        while offset < self.lines.len() {
            let mut next = offset;
            let mut min = self.lines[offset];
            let mut max = self.lines[offset];
            while next < self.lines.len() && next < offset + span as usize {
                min = min.min(self.lines[next]);
                max = max.max(self.lines[next]);
                if max - min > 255 {
                    break;
                }
                next += 1;
            }
            if next < self.lines.len() && next - offset < span as usize {
                span = 1 << log2((next - offset) as i32);
            }
            offset += span as usize;
        }
        span
    }

    fn write_line_info(&self, ss: &mut Vec<u8>) {
        let span = self.calc_lines_span() as usize;
        let baseline_size = (self.lines.len() - 1) / span + 1;
        let mut baseline = vec![0i32; baseline_size];
        let mut offset = 0;
        while offset < self.lines.len() {
            let mut min = self.lines[offset];
            let mut next = offset;
            while next < self.lines.len() && next < offset + span {
                min = min.min(self.lines[next]);
                next += 1;
            }
            baseline[offset / span] = min;
            offset += span;
        }
        let logspan = log2(span as i32);
        ss.push(logspan as u8);
        let mut last_offset: u8 = 0;
        for (i, &line) in self.lines.iter().enumerate() {
            let delta = line - baseline[i >> logspan];
            debug_assert!((0..=255).contains(&delta));
            ss.push((delta as u8).wrapping_sub(last_offset));
            last_offset = delta as u8;
        }
        let mut last_line = 0;
        for &b in &baseline {
            write_int(ss, b - last_line);
            last_line = b;
        }
    }

    pub fn get_import_id1(id0: i32) -> u32 {
        (1u32 << 30) | ((id0 as u32) << 20)
    }

    pub fn get_import_id2(id0: i32, id1: i32) -> u32 {
        (2u32 << 30) | ((id0 as u32) << 20) | ((id1 as u32) << 10)
    }

    pub fn get_import_id3(id0: i32, id1: i32, id2: i32) -> u32 {
        (3u32 << 30) | ((id0 as u32) << 20) | ((id1 as u32) << 10) | id2 as u32
    }

    /// `getStringHash`: Lua 5.1 string hash (matches `luaS_hash` for short
    /// strings), used as the predicted table slot.
    pub fn get_string_hash(key: &[u8]) -> u32 {
        let mut h = key.len() as u32;
        let mut i = key.len();
        while i > 0 {
            i -= 1;
            h ^= (h << 5).wrapping_add(h >> 2).wrapping_add(key[i] as u32);
        }
        h
    }

    pub fn fold_jumps(&mut self) {
        if self.has_long_jumps {
            return;
        }
        for j in 0..self.jumps.len() {
            let jump_label = self.jumps[j].source as usize;
            let jump_insn = self.insns[jump_label];
            let mut target_label = (jump_label as i32 + 1 + insn::d(jump_insn)) as usize;
            let mut target_insn = self.insns[target_label];
            while insn::op(target_insn) == Opcode::Jump as u32 && insn::d(target_insn) >= 0 {
                target_label = (target_label as i32 + 1 + insn::d(target_insn)) as usize;
                target_insn = self.insns[target_label];
            }
            let offset = target_label as i32 - jump_label as i32 - 1;
            if insn::op(jump_insn) == Opcode::Jump as u32 && insn::op(target_insn) == Opcode::Return as u32 {
                self.insns[jump_label] = target_insn;
            } else if offset as i16 as i32 == offset {
                self.insns[jump_label] &= 0xffff;
                self.insns[jump_label] |= ((offset as i16 as u16) as u32) << 16;
            }
            self.jumps[j].target = target_label as u32;
        }
    }

    /// Rewrites jumps whose offsets don't fit 16 bits through `JUMPX`
    /// trampolines. Returns false when even that is out of range.
    pub fn expand_jumps(&mut self) -> bool {
        if !self.has_long_jumps {
            return true;
        }
        const CONSERVATIVE: i32 = 32767 / 3;
        self.jumps.sort_by_key(|j| j.source);
        let mut remap = vec![0u32; self.insns.len()];
        let mut newinsns: Vec<u32> = Vec::with_capacity(self.insns.len());
        let mut newlines: Vec<i32> = Vec::with_capacity(self.insns.len());
        let mut current = 0usize;
        let mut i = 0usize;
        while i < self.insns.len() {
            let op = insn::op(self.insns[i]) as u8;
            if current < self.jumps.len() && self.jumps[current].source as usize == i {
                let offset = self.jumps[current].target as i32 - self.jumps[current].source as i32 - 1;
                if offset.abs() >= CONSERVATIVE {
                    newinsns.push(Opcode::Jump as u32 | (1 << 16));
                    newinsns.push(Opcode::JumpX as u32);
                    newlines.push(self.lines[i]);
                    newlines.push(self.lines[i]);
                }
                current += 1;
            }
            let oplen = get_op_length(unsafe { std::mem::transmute::<u8, Opcode>(op) });
            for _ in 0..oplen {
                remap[i] = newinsns.len() as u32;
                newinsns.push(self.insns[i]);
                newlines.push(self.lines[i]);
                i += 1;
            }
        }
        for j in &self.jumps {
            let offset = j.target as i32 - j.source as i32 - 1;
            let newoffset = remap[j.target as usize] as i32 - remap[j.source as usize] as i32 - 1;
            if newoffset.abs() + 1 >= MAX_JUMP_DISTANCE {
                return false;
            } else if offset.abs() >= CONSERVATIVE {
                let t = remap[j.source as usize] as usize - 1;
                newinsns[t] &= 0xff;
                newinsns[t] |= ((newoffset + 1) as u32) << 8;
                let s = remap[j.source as usize] as usize;
                newinsns[s] &= 0xffff;
                newinsns[s] |= ((-2i16 as u16) as u32) << 16;
            } else {
                let s = remap[j.source as usize] as usize;
                newinsns[s] &= 0xffff;
                newinsns[s] |= ((newoffset as i16 as u16) as u32) << 16;
            }
        }
        self.insns = newinsns;
        self.lines = newlines;
        for l in &mut self.debug_locals {
            if l.startpc != l.endpc {
                l.endpc = remap[l.endpc as usize - 1] + 1;
            } else {
                l.endpc = remap[l.endpc as usize];
            }
            l.startpc = remap[l.startpc as usize];
        }
        for l in &mut self.typed_locals {
            if l.2 != l.3 {
                l.3 = remap[l.3 as usize - 1] + 1;
            } else {
                l.3 = remap[l.3 as usize];
            }
            l.2 = remap[l.2 as usize];
        }
        for slot in &mut self.fb_slots {
            if let FeedbackSlot::CallTarget(pc) = slot {
                *pc = remap[*pc as usize];
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_hash_matches_lua51() {
        // values computed with the C++ getStringHash
        assert_eq!(BytecodeBuilder::get_string_hash(b""), 0);
        assert_eq!(BytecodeBuilder::get_string_hash(b"a"), 1 ^ ((1 << 5) + (1 >> 2) + 97));
    }

    #[test]
    fn var_int_encoding() {
        let mut v = Vec::new();
        write_var_int(&mut v, 300);
        assert_eq!(v, vec![0xac, 0x02]);
    }
}
