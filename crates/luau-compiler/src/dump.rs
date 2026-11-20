//! Port of the text dumping half of `Bytecode/src/BytecodeBuilder.cpp`
//! (`dumpCurrentFunction`, `dumpInstruction`, `dumpConstant`,
//! `dumpEverything`, `dumpSourceRemarks`, `dumpTypeInfo`), used by
//! `luau-compile --text` / `--remarks`.

use std::fmt::Write;

use luau_common::bytecode::{get_op_length, insn, type_tag, Opcode};

use super::{BytecodeBuilder, Constant, FeedbackSlot};

pub const DUMP_CODE: u32 = 1 << 0;
pub const DUMP_LINES: u32 = 1 << 1;
pub const DUMP_SOURCE: u32 = 1 << 2;
pub const DUMP_LOCALS: u32 = 1 << 3;
pub const DUMP_REMARKS: u32 = 1 << 4;
pub const DUMP_TYPES: u32 = 1 << 5;
pub const DUMP_CONSTANTS: u32 = 1 << 6;

/// C `printf("%.{prec}g", v)`.
pub fn fmt_g(v: f64, prec: usize) -> String {
    if v.is_nan() {
        // the reference (MinGW printf) prints every NaN unsigned
        return "nan".into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf".into() } else { "inf".into() };
    }
    let p = prec.max(1);
    let e = format!("{:.*e}", p - 1, v);
    let (mant, exp) = e.split_once('e').unwrap();
    let x: i32 = exp.parse().unwrap();
    if x < -4 || x >= p as i32 {
        let mant = strip_zeros(mant);
        format!("{}e{}{:02}", mant, if x < 0 { '-' } else { '+' }, x.abs())
    } else {
        let f = format!("{:.*}", (p as i32 - 1 - x) as usize, v);
        strip_zeros(&f).to_string()
    }
}

/// Bytes as a `String` via Latin-1, so arbitrary source bytes survive the
/// dump; [`to_bytes`] converts the finished text back.
fn lat(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

/// Inverse of the Latin-1 mapping used by the dump functions.
pub fn to_bytes(s: &str) -> Vec<u8> {
    s.chars().map(|c| c as u32 as u8).collect()
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

fn is_jump_d(op: Opcode) -> bool {
    use Opcode::*;
    matches!(
        op,
        Jump | JumpIf
            | JumpIfNot
            | JumpIfEq
            | JumpIfLe
            | JumpIfLt
            | JumpIfNotEq
            | JumpIfNotLe
            | JumpIfNotLt
            | ForNPrep
            | ForNLoop
            | ForGPrep
            | ForGLoop
            | ForGPrepInext
            | ForGPrepNext
            | JumpBack
            | JumpXEqKNil
            | JumpXEqKB
            | JumpXEqKN
            | JumpXEqKS
            | CmpProto
    )
}

fn is_fast_call(op: Opcode) -> bool {
    use Opcode::*;
    matches!(op, FastCall | FastCall1 | FastCall2 | FastCall2K | FastCall3 | FastPCall | FinConstruct)
}

/// `getJumpTarget`.
pub fn get_jump_target(i: u32, pc: u32) -> i32 {
    let op = Opcode::from_u8(insn::op(i) as u8);
    if is_jump_d(op) {
        pc as i32 + insn::d(i) + 1
    } else if is_fast_call(op) {
        pc as i32 + insn::c(i) as i32 + 2
    } else if op == Opcode::LoadB && insn::c(i) != 0 {
        pc as i32 + insn::c(i) as i32 + 1
    } else if op == Opcode::JumpX {
        pc as i32 + insn::e(i) + 1
    } else {
        -1
    }
}

fn base_type_string(ty: u8) -> &'static str {
    match ty & !type_tag::OPTIONAL_BIT {
        type_tag::NIL => "nil",
        type_tag::BOOLEAN => "boolean",
        type_tag::NUMBER => "number",
        type_tag::INTEGER => "integer",
        type_tag::STRING => "string",
        type_tag::TABLE => "table",
        type_tag::FUNCTION => "function",
        type_tag::THREAD => "thread",
        type_tag::USERDATA => "userdata",
        type_tag::VECTOR => "vector",
        type_tag::BUFFER => "buffer",
        type_tag::ANY => "any",
        _ => "??",
    }
}

fn printable(s: &[u8]) -> bool {
    s.iter().all(|&c| c >= b' ')
}

fn ceil_log2(v: usize) -> u32 {
    let mut r = 0;
    while (1usize << r) < v {
        r += 1;
    }
    r
}

/// Splits source into lines like `setDumpSource` (strips trailing `\r`).
pub fn split_source(source: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = source.split(|&c| c == b'\n').map(|l| l.to_vec()).collect();
    for l in &mut out {
        if l.last() == Some(&b'\r') {
            l.pop();
        }
    }
    out
}

impl BytecodeBuilder {
    fn string_at(&self, index: u32) -> &[u8] {
        &self.strings[index as usize - 1]
    }

    fn dump_constant(&self, r: &mut String, k: usize, detailed: bool) {
        match self.constants[k] {
            Constant::Nil => r.push_str("nil"),
            Constant::Boolean(b) => r.push_str(if b { "true" } else { "false" }),
            Constant::Number(n) => r.push_str(&fmt_g(n, 17)),
            Constant::Integer(v) => {
                let _ = write!(r, "{v}");
            }
            Constant::Vectorf(v) => {
                let n = if v[3] == 0.0 { 3 } else { 4 };
                let parts: Vec<String> = v[..n].iter().map(|&x| fmt_g(x as f64, 9)).collect();
                r.push_str(&parts.join(", "));
            }
            Constant::Vectord(v) => {
                let n = if v[3] == 0.0 { 3 } else { 4 };
                let parts: Vec<String> = v[..n].iter().map(|&x| fmt_g(x, 17)).collect();
                r.push_str(&parts.join(", "));
            }
            Constant::String(i) => {
                let s = self.string_at(i);
                if printable(s) {
                    if s.len() < 32 {
                        let _ = write!(r, "'{}'", lat(s));
                    } else {
                        let _ = write!(r, "'{}'...", lat(&s[..32]));
                    }
                } else {
                    r.push('\'');
                    let mut tmp = Vec::new();
                    for &c in s.iter().take(32) {
                        if c < b' ' {
                            tmp.extend_from_slice(format!("\\x{c:02X}").as_bytes());
                        } else {
                            tmp.push(c);
                        }
                    }
                    r.push_str(&lat(&tmp));
                    r.push_str(if s.len() >= 32 { "'..." } else { "'" });
                }
            }
            Constant::Import(iid) => {
                let count = iid >> 30;
                let ids = [(iid >> 20) & 1023, (iid >> 10) & 1023, iid & 1023];
                for (n, &id) in ids.iter().enumerate().take(count as usize) {
                    if let Constant::String(si) = self.constants[id as usize] {
                        if n > 0 {
                            r.push('.');
                        }
                        r.push_str(&lat(self.string_at(si)));
                    }
                }
            }
            Constant::Table(t) => {
                if detailed {
                    let shape = &self.table_shapes[t as usize];
                    let len = shape.keys.len();
                    let sizenode: u32 = if len > 0 { 1 << ceil_log2(len) } else { 0 };
                    let mask = if sizenode > 0 { sizenode - 1 } else { 0 };
                    let mut slots = vec![0u32; len];
                    let mut owner = vec![u32::MAX; sizenode as usize];
                    for i in 0..len {
                        let s = match self.constants[shape.keys[i] as usize] {
                            Constant::String(si) => self.string_at(si),
                            _ => &[],
                        };
                        slots[i] = BytecodeBuilder::get_string_hash(s) & mask;
                        if owner[slots[i] as usize] == u32::MAX {
                            owner[slots[i] as usize] = i as u32;
                        }
                    }
                    r.push('{');
                    for i in 0..len {
                        if i > 0 {
                            r.push_str(", ");
                        }
                        r.push('[');
                        self.dump_constant(r, shape.keys[i] as usize, false);
                        r.push(']');
                        if shape.has_constants && shape.constants[i] != -1 {
                            r.push_str(" = ");
                            self.dump_constant(r, shape.constants[i] as usize, false);
                        }
                        let _ = write!(r, " #{}", slots[i]);
                        if owner[slots[i] as usize] != i as u32 {
                            r.push_str(" (conflict)");
                        }
                    }
                    let _ = write!(r, "}} sizenode={sizenode}");
                } else {
                    r.push_str("{...}");
                }
            }
            Constant::Closure(f) => {
                let name = &self.functions[f as usize].dumpname;
                if detailed {
                    if !name.is_empty() {
                        let _ = write!(r, "function {}", lat(name));
                    } else {
                        r.push_str("function");
                    }
                } else if !name.is_empty() {
                    let _ = write!(r, "'{}'", lat(name));
                }
            }
            Constant::ClassShape(c) => {
                let cs = &self.class_shapes[c as usize];
                let name = match self.constants[cs.class_name as usize] {
                    Constant::String(si) => lat(self.string_at(si)),
                    _ => String::new(),
                };
                let _ = write!(
                    r,
                    "class {} (public fields: {}, public methods: {}, private fields: {}, private methods: {})",
                    name,
                    cs.property_names.len(),
                    cs.method_names.len(),
                    cs.num_private_instance_props,
                    cs.num_private_static_props
                );
                if detailed {
                    if !cs.property_names.is_empty() {
                        r.push_str("\n  props:");
                        for &p in &cs.property_names {
                            let _ = write!(r, "\n    K{p} [");
                            self.dump_constant(r, p as usize, false);
                            r.push(']');
                        }
                    }
                    if !cs.method_names.is_empty() {
                        r.push_str("\n  methods:");
                        for &p in &cs.method_names {
                            let _ = write!(r, "\n    K{p} [");
                            self.dump_constant(r, p as usize, false);
                            r.push(']');
                        }
                    }
                }
            }
        }
    }

    fn dump_k(&self, r: &mut String, k: u32) {
        r.push('[');
        self.dump_constant(r, k as usize, false);
        r.push_str("]\n");
    }

    fn dump_instruction(&self, code: &[u32], r: &mut String, target: i32) {
        use Opcode::*;
        let i = code[0];
        let aux = code.get(1).copied().unwrap_or(0);
        let (a, b, c, d) = (insn::a(i), insn::b(i), insn::c(i), insn::d(i));
        let op = Opcode::from_u8(insn::op(i) as u8);
        let not = if aux >> 31 != 0 { " NOT" } else { "" };
        macro_rules! w { ($($t:tt)*) => {{ let _ = write!(r, $($t)*); }} }
        match op {
            LoadNil => w!("LOADNIL R{a}\n"),
            LoadB => {
                if c != 0 {
                    w!("LOADB R{a} {b} +{c}\n")
                } else {
                    w!("LOADB R{a} {b}\n")
                }
            }
            LoadN => w!("LOADN R{a} {d}\n"),
            LoadK => {
                w!("LOADK R{a} K{d} ");
                self.dump_k(r, d as u32);
            }
            Move => w!("MOVE R{a} R{b}\n"),
            GetGlobal | SetGlobal => {
                w!("{} R{a} K{aux} ", if op == GetGlobal { "GETGLOBAL" } else { "SETGLOBAL" });
                self.dump_k(r, aux);
            }
            GetUpval => w!("GETUPVAL R{a} U{b}\n"),
            SetUpval => w!("SETUPVAL R{a} U{b}\n"),
            CloseUpvals => w!("CLOSEUPVALS R{a}\n"),
            GetImport => {
                w!("GETIMPORT R{a} {d} ");
                self.dump_k(r, d as u32);
            }
            GetTable => w!("GETTABLE R{a} R{b} R{c}\n"),
            SetTable => w!("SETTABLE R{a} R{b} R{c}\n"),
            GetTableKs | SetTableKs | Namecall => {
                let n = match op {
                    GetTableKs => "GETTABLEKS",
                    SetTableKs => "SETTABLEKS",
                    _ => "NAMECALL",
                };
                w!("{n} R{a} R{b} K{aux} ");
                self.dump_k(r, aux);
            }
            GetTableN => w!("GETTABLEN R{a} R{b} {}\n", c + 1),
            SetTableN => w!("SETTABLEN R{a} R{b} {}\n", c + 1),
            NewClosure => w!("NEWCLOSURE R{a} P{d}\n"),
            Call => w!("CALL R{a} {} {}\n", b as i32 - 1, c as i32 - 1),
            CallFb => w!("CALLFB R{a} {} {} [{}]\n", b as i32 - 1, c as i32 - 1, aux as i32),
            Return => w!("RETURN R{a} {}\n", b as i32 - 1),
            Jump => w!("JUMP L{target}\n"),
            JumpIf => w!("JUMPIF R{a} L{target}\n"),
            JumpIfNot => w!("JUMPIFNOT R{a} L{target}\n"),
            JumpIfEq | JumpIfLe | JumpIfLt | JumpIfNotEq | JumpIfNotLe | JumpIfNotLt => {
                let n = match op {
                    JumpIfEq => "JUMPIFEQ",
                    JumpIfLe => "JUMPIFLE",
                    JumpIfLt => "JUMPIFLT",
                    JumpIfNotEq => "JUMPIFNOTEQ",
                    JumpIfNotLe => "JUMPIFNOTLE",
                    _ => "JUMPIFNOTLT",
                };
                w!("{n} R{a} R{aux} L{target}\n")
            }
            Add | Sub | Mul | Div | IDiv | Mod | Pow | And | Or | Concat => {
                let n = match op {
                    Add => "ADD",
                    Sub => "SUB",
                    Mul => "MUL",
                    Div => "DIV",
                    IDiv => "IDIV",
                    Mod => "MOD",
                    Pow => "POW",
                    And => "AND",
                    Or => "OR",
                    _ => "CONCAT",
                };
                w!("{n} R{a} R{b} R{c}\n")
            }
            AddK | SubK | MulK | DivK | IDivK | ModK | PowK | AndK | OrK => {
                let n = match op {
                    AddK => "ADDK",
                    SubK => "SUBK",
                    MulK => "MULK",
                    DivK => "DIVK",
                    IDivK => "IDIVK",
                    ModK => "MODK",
                    PowK => "POWK",
                    AndK => "ANDK",
                    _ => "ORK",
                };
                w!("{n} R{a} R{b} K{c} ");
                self.dump_k(r, c);
            }
            SubRk | DivRk => {
                w!("{} R{a} K{b} [", if op == SubRk { "SUBRK" } else { "DIVRK" });
                self.dump_constant(r, b as usize, false);
                w!("] R{c}\n");
            }
            Not => w!("NOT R{a} R{b}\n"),
            Minus => w!("MINUS R{a} R{b}\n"),
            Length => w!("LENGTH R{a} R{b}\n"),
            NewTable => w!("NEWTABLE R{a} {} {aux}\n", if b == 0 { 0 } else { 1u32 << (b - 1) }),
            DupTable => w!("DUPTABLE R{a} {d}\n"),
            SetList => w!("SETLIST R{a} R{b} {} [{aux}]\n", c as i32 - 1),
            ForNPrep => w!("FORNPREP R{a} L{target}\n"),
            ForNLoop => w!("FORNLOOP R{a} L{target}\n"),
            ForGPrep => w!("FORGPREP R{a} L{target}\n"),
            ForGLoop => w!("FORGLOOP R{a} L{target} {}{}\n", aux as u8, if (aux as i32) < 0 { " [inext]" } else { "" }),
            ForGPrepInext => w!("FORGPREP_INEXT R{a} L{target}\n"),
            ForGPrepNext => w!("FORGPREP_NEXT R{a} L{target}\n"),
            GetVarargs => w!("GETVARARGS R{a} {}\n", b as i32 - 1),
            DupClosure => {
                w!("DUPCLOSURE R{a} K{d} ");
                self.dump_k(r, d as u32);
            }
            Break => w!("BREAK\n"),
            JumpBack => w!("JUMPBACK L{target}\n"),
            LoadKx => {
                w!("LOADKX R{a} K{aux} ");
                self.dump_k(r, aux);
            }
            JumpX => w!("JUMPX L{target}\n"),
            FastCall => w!("FASTCALL {a} L{target}\n"),
            FastCall1 => w!("FASTCALL1 {a} R{b} L{target}\n"),
            FastCall2 => w!("FASTCALL2 {a} R{b} R{aux} L{target}\n"),
            FastCall2K => {
                w!("FASTCALL2K {a} R{b} K{aux} L{target} ");
                self.dump_k(r, aux);
            }
            FastCall3 => w!("FASTCALL3 {a} R{b} R{} R{} L{target}\n", aux & 0xff, (aux >> 8) & 0xff),
            Coverage => w!("COVERAGE\n"),
            Capture => {
                let (n, p) = match a {
                    0 => ("VAL", 'R'),
                    1 => ("REF", 'R'),
                    2 => ("UPVAL", 'U'),
                    _ => ("", 'R'),
                };
                w!("CAPTURE {n} {p}{b}\n")
            }
            JumpXEqKNil => w!("JUMPXEQKNIL R{a} L{target}{not}\n"),
            JumpXEqKB => w!("JUMPXEQKB R{a} {} L{target}{not}\n", aux & 1),
            JumpXEqKN | JumpXEqKS => {
                let k = aux & 0xffffff;
                w!("{} R{a} K{k} L{target}{not} ", if op == JumpXEqKN { "JUMPXEQKN" } else { "JUMPXEQKS" });
                self.dump_k(r, k);
            }
            GetUdataKs | SetUdataKs | NamecallUdata => {
                let n = match op {
                    GetUdataKs => "GETUDATAKS",
                    SetUdataKs => "SETUDATAKS",
                    _ => "NAMECALLUDATA",
                };
                let k = insn::aux_kv16(aux);
                w!("{n} R{a} R{b} K{k} ");
                self.dump_k(r, k);
            }
            NewClassMember => {
                // LCMV_PRIVATE == 1
                if b == 1 {
                    w!("NEWCLASSMEMBER R{a} R{c} {} [private]\n", insn::aux_a(aux))
                } else {
                    w!("NEWCLASSMEMBER R{a} R{c} [public] ");
                    self.dump_k(r, aux);
                }
            }
            CmpProto => w!("CMPPROTO R{a} #{aux} L{target}\n"),
            FastPCall => w!("FASTPCALL {} L{target}\n", if a == 0 { "pcall" } else { "xpcall" }),
            NewClass => {
                if b == 0xff {
                    w!("NEWCLASS R{a} no_base K{aux} {c} ")
                } else {
                    w!("NEWCLASS R{a} R{b} K{aux} {c} ")
                }
                self.dump_k(r, aux);
            }
            Construct => w!("CONSTRUCT R{a} R{b} slot {aux}\n"),
            FinConstruct => w!("FINCONSTRUCT R{a} L{target}\n"),
            GetPrivateField => {
                w!("GETPRIVATEFIELD R{a} R{b} U{c} {} [", insn::aux_a(aux));
                w!(
                    "{}]{}\n",
                    if insn::aux_static(aux) != 0 { "static" } else { "instance" },
                    if insn::aux_namecall(aux) != 0 { " [namecall]" } else { "" }
                )
            }
            SetPrivateField => w!("SETPRIVATEFIELD R{a} R{b} U{c} {}\n", insn::aux_a(aux)),
            _ => {}
        }
    }

    /// `dumpCurrentFunction`.
    pub(crate) fn dump_current_function(&self) -> String {
        let flags = self.dump_flags;
        if flags & (DUMP_CODE | DUMP_CONSTANTS) == 0 {
            return String::new();
        }
        let mut r = String::new();
        let lines = &self.lines;

        if flags & DUMP_LOCALS != 0 {
            for (i, l) in self.debug_locals.iter().enumerate() {
                if flags & DUMP_CODE != 0 {
                    let _ = write!(r, "local {} ({}): ", i, lat(self.string_at(l.name)));
                } else {
                    let _ = write!(r, "local {i}: ");
                }
                let _ = write!(r, "reg {}, start pc {} line {}, ", l.reg, l.startpc, lines[l.startpc as usize]);
                if l.startpc == l.endpc {
                    r.push_str("no live range\n");
                } else {
                    let _ = writeln!(r, "end pc {} line {}", l.endpc - 1, lines[l.endpc as usize - 1]);
                }
            }
        }

        if flags & DUMP_TYPES != 0 {
            let opt = |t: u8| if t & type_tag::OPTIONAL_BIT != 0 { "?" } else { "" };
            for (i, &et) in self.typeinfo.iter().enumerate().skip(2) {
                let _ = writeln!(r, "R{}: {}{} [argument]", i - 2, base_type_string(et), opt(et));
            }
            for (i, &t) in self.typed_upvals.iter().enumerate() {
                let _ = writeln!(r, "U{}: {}{}", i, base_type_string(t), opt(t));
            }
            for &(t, reg, startpc, endpc) in &self.typed_locals {
                let _ = writeln!(r, "R{}: {}{} from {} to {}", reg, base_type_string(t), opt(t), startpc, endpc);
            }
        }

        if flags & DUMP_CONSTANTS != 0 {
            for i in 0..self.constants.len() {
                let _ = write!(r, "K{i}: ");
                self.dump_constant(&mut r, i, true);
                r.push('\n');
            }
            for (i, s) in self.fb_slots.iter().enumerate() {
                match *s {
                    FeedbackSlot::CallTarget(pc) => {
                        let _ = writeln!(r, "slot {i}: call target at pc {pc}");
                    }
                    FeedbackSlot::Construct(k) => {
                        let _ = writeln!(r, "slot {i}: shape K{k}");
                    }
                }
            }
        }

        if flags & DUMP_CODE != 0 {
            let insns = &self.insns;
            let mut labels = vec![-1i32; insns.len()];
            let mut i = 0;
            while i < insns.len() {
                let t = get_jump_target(insns[i], i as u32);
                if t >= 0 {
                    labels[t as usize] = 0;
                }
                i += get_op_length(Opcode::from_u8(insn::op(insns[i]) as u8)) as usize;
            }
            let mut next = 0;
            for l in labels.iter_mut() {
                if *l == 0 {
                    *l = next;
                    next += 1;
                }
            }

            let mut last_line = -1;
            let mut next_remark = 0;
            let mut i = 0;
            while i < insns.len() {
                let op = Opcode::from_u8(insn::op(insns[i]) as u8);
                if op == Opcode::PrepVarargs {
                    i += 1;
                    continue;
                }
                if flags & DUMP_REMARKS != 0 {
                    while next_remark < self.debug_remarks.len() && self.debug_remarks[next_remark].0 as usize == i {
                        let _ = writeln!(r, "REMARK {}", self.debug_remarks[next_remark].1);
                        next_remark += 1;
                    }
                }
                if flags & DUMP_SOURCE != 0 {
                    let line = lines[i];
                    if line > 0 && line != last_line {
                        let src = self.dump_source.get(line as usize - 1).map(|l| lat(l)).unwrap_or_default();
                        let _ = writeln!(r, "{line:5}: {src}");
                        last_line = line;
                    }
                }
                if flags & DUMP_LINES != 0 {
                    let _ = write!(r, "{}: ", lines[i]);
                }
                if labels[i] != -1 {
                    let _ = write!(r, "L{}: ", labels[i]);
                }
                let t = get_jump_target(insns[i], i as u32);
                let len = get_op_length(op) as usize;
                self.dump_instruction(&insns[i..i + len], &mut r, if t >= 0 { labels[t as usize] } else { -1 });
                i += len;
            }
        }
        r
    }

    /// `setDumpFlags`.
    pub fn set_dump_flags(&mut self, flags: u32) {
        self.dump_flags = flags;
    }

    /// `setDumpSource`.
    pub fn set_dump_source(&mut self, source: &[u8]) {
        self.dump_source = split_source(source);
    }

    /// `needsDebugRemarks`.
    pub fn needs_debug_remarks(&self) -> bool {
        self.dump_flags & DUMP_REMARKS != 0
    }

    /// `addDebugRemark`.
    pub fn add_debug_remark(&mut self, text: String) {
        if self.dump_flags & DUMP_REMARKS == 0 {
            return;
        }
        self.dump_remarks.push((self.debug_line, text.clone()));
        self.debug_remarks.push((self.insns.len() as u32, text));
    }

    /// `dumpFunction`.
    pub fn dump_function(&self, id: u32) -> &str {
        &self.functions[id as usize].dump
    }

    /// `dumpEverything`.
    pub fn dump_everything(&self) -> String {
        let mut r = String::new();
        for (i, f) in self.functions.iter().enumerate() {
            let name = if f.dumpname.is_empty() { "??".into() } else { lat(&f.dumpname) };
            let _ = writeln!(r, "Function {i} ({name}):");
            r.push_str(&f.dump);
            r.push('\n');
        }
        r
    }

    /// `dumpSourceRemarks`.
    pub fn dump_source_remarks(&self) -> String {
        let mut r = String::new();
        let mut remarks = self.dump_remarks.clone();
        remarks.sort();
        let mut next = 0;
        for (i, line) in self.dump_source.iter().enumerate() {
            let indent = line.iter().take_while(|&&c| c == b' ' || c == b'\t').count();
            while next < remarks.len() && remarks[next].0 == i as i32 + 1 {
                let _ = writeln!(r, "{}-- remark: {}", lat(&line[..indent]), remarks[next].1);
                next += 1;
                while next < remarks.len() && remarks[next] == remarks[next - 1] {
                    next += 1;
                }
            }
            r.push_str(&lat(line));
            if i + 1 < self.dump_source.len() {
                r.push('\n');
            }
        }
        r
    }
}
