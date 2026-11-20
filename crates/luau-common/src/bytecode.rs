//! Port of `Luau/Bytecode.h` — the bytecode definition shared between the
//! compiler (bytecode builder) and the interpreter.
//!
//! All enums here are **order-sensitive**: values are baked into serialized
//! bytecode, so the declaration order exactly matches the C++ `LuauOpcode`
//! enum and must never be reshuffled. This is verified by unit tests that
//! spot-check discriminant values.
#![allow(non_camel_case_types)]

/// Implements `TryFrom<u8>` (and convenience `from_u8`) for dense `repr(u8)`
/// fieldless enums, using `transmute` guarded by a range check.
///
/// `$max` must be the number of variants (first invalid value).
macro_rules! dense_u8_enum {
    ($t:ty, $max:expr) => {
        impl TryFrom<u8> for $t {
            type Error = u8;
            #[inline]
            fn try_from(v: u8) -> Result<Self, u8> {
                if v < $max {
                    // SAFETY: `repr(u8)` fieldless enum with dense discriminants
                    // `0..$max`; `v < $max` guarantees the value is valid.
                    Ok(unsafe { core::mem::transmute(v) })
                } else {
                    Err(v)
                }
            }
        }
        impl $t {
            #[inline]
            pub fn from_u8(v: u8) -> $t {
                <$t>::try_from(v).expect("invalid enum value")
            }
        }
    };
}

/// Bytecode opcode, part of the instruction header.
///
/// Order-sensitive: keep in exact sync with C++ `LuauOpcode` enum.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Opcode {
    /// NOP: noop
    Nop,
    /// BREAK: debugger break
    Break,
    /// LOADNIL: sets register to nil
    /// A: target register
    LoadNil,
    /// LOADB: sets register to boolean and jumps to a given short offset
    /// A: target register, B: value (0/1), C: jump offset
    LoadB,
    /// LOADN: sets register to a number literal
    /// A: target register, D: value (-32768..32767)
    LoadN,
    /// LOADK: sets register to an entry from the constant table (number/vector/string)
    /// A: target register, D: constant table index (0..32767)
    LoadK,
    /// MOVE: move (copy) value from one register to another
    /// A: target register, B: source register
    Move,
    /// GETGLOBAL: load value from global table using constant string as a key
    /// A: target register, C: predicted slot index, AUX: constant table index
    GetGlobal,
    /// SETGLOBAL: set value in global table using constant string as a key
    /// A: source register, C: predicted slot index, AUX: constant table index
    SetGlobal,
    /// GETUPVAL: load upvalue from the upvalue table for the current function
    /// A: target register, B: upvalue index
    GetUpval,
    /// SETUPVAL: store value into the upvalue table for the current function
    /// A: target register, B: upvalue index
    SetUpval,
    /// CLOSEUPVALS: close (migrate to heap) all upvalues captured for registers >= target
    /// A: target register
    CloseUpvals,
    /// GETIMPORT: load imported global table global from the constant table
    /// A: target register, D: constant table index,
    /// AUX: 3 10-bit indices of constant strings that, combined, constitute an import path
    GetImport,
    /// GETTABLE: load value from table into target register using key from register
    /// A: target register, B: table register, C: index register
    GetTable,
    /// SETTABLE: store source register into table using key from register
    /// A: source register, B: table register, C: index register
    SetTable,
    /// GETTABLEKS: load value from table using constant string as a key
    /// A: target register, B: table register, C: predicted slot index, AUX: constant table index
    GetTableKs,
    /// SETTABLEKS: store source register into table using constant string as a key
    /// A: source register, B: table register, C: predicted slot index, AUX: constant table index
    SetTableKs,
    /// GETTABLEN: load value from table using small integer index as a key
    /// A: target register, B: table register, C: index-1 (index is 1..256)
    GetTableN,
    /// SETTABLEN: store source register into table using small integer index as a key
    /// A: source register, B: table register, C: index-1 (index is 1..256)
    SetTableN,
    /// NEWCLOSURE: create closure from a child proto; followed by a CAPTURE per upvalue
    /// A: target register, D: child proto index (0..32767)
    NewClosure,
    /// NAMECALL: prepare to call specified method by name
    /// A: target register, B: source register, C: predicted slot index, AUX: constant table index
    /// Must be followed directly by CALL.
    Namecall,
    /// CALL: call specified function
    /// A: register of function + args; results placed starting from same register
    /// B: argument count + 1, or 0 (MULTRET); C: result count + 1, or 0 (MULTRET)
    Call,
    /// RETURN: return specified values from the function
    /// A: register where returned values start, B: value count + 1 or 0 (MULTRET)
    Return,
    /// JUMP: jump to target offset
    /// D: jump offset (-32768..32767; 0 = "next instruction")
    Jump,
    /// JUMPBACK: jump to target offset; safepoint for loop interruption
    /// D: jump offset
    JumpBack,
    /// JUMPIF: jump to target offset if register is not nil/false
    /// A: source register, D: jump offset
    JumpIf,
    /// JUMPIFNOT: jump to target offset if register is nil/false
    /// A: source register, D: jump offset
    JumpIfNot,
    /// Comparison jumps: AUX holds source register 2, D holds jump offset
    /// A: source register 1, D: jump offset, AUX: source register 2
    JumpIfEq,
    JumpIfLe,
    JumpIfLt,
    JumpIfNotEq,
    JumpIfNotLe,
    JumpIfNotLt,
    /// Arithmetic on two registers: A: target, B: source 1, C: source 2
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    /// Arithmetic on register and constant: A: target, B: source, C: constant index (0..255)
    AddK,
    SubK,
    MulK,
    DivK,
    ModK,
    PowK,
    /// AND/OR: select between two registers based on truthiness
    /// A: target, B: source 1, C: source 2
    And,
    Or,
    /// ANDK/ORK: select between register and constant based on truthiness
    /// A: target, B: source, C: constant index (0..255)
    AndK,
    OrK,
    /// CONCAT: concatenate all strings between B and C (inclusive) into A
    Concat,
    /// Unary ops: A: target, B: source
    Not,
    Minus,
    Length,
    /// NEWTABLE: create table in target register
    /// A: target register, B: table size encoded hash size, AUX: array size
    NewTable,
    /// DUPTABLE: duplicate table using constant table template
    /// A: target register, D: constant table index
    DupTable,
    /// SETLIST: set a list of values into table in target register
    /// A: target register, B: source register start, C: value count + 1 or 0 (MULTRET),
    /// AUX: table index to start from
    SetList,
    /// FORNPREP: prepare numeric for loop; jump over the loop if first iteration doesn't run
    /// A: target register (layout: [limit, step, index, variable]), D: jump offset
    ForNPrep,
    /// FORNLOOP: adjust loop variables for one iteration, jump back if loop continues
    /// A: target register, D: jump offset
    ForNLoop,
    /// FORGLOOP: generic for loop iteration
    /// A: target register (layout: [generator, state, index, variables...]), D: jump offset
    /// AUX: variable count (1..255) in low 8 bits, high bit = ipairs-style fast path
    ForGLoop,
    /// FORGPREP_INEXT: prepare FORGLOOP with 2 output variables, assuming generator is luaB_inext
    /// A: target register, D: jump offset
    ForGPrepInext,
    /// FASTCALL3: fast call of a built-in function using 3 register arguments
    /// A: builtin id, B: source argument register, C: jump offset to following CALL,
    /// AUX: source register 2 in low byte, source register 3 in next byte
    FastCall3,
    /// FORGPREP_NEXT: prepare FORGLOOP with 2 output variables, assuming generator is luaB_next
    /// A: target register, D: jump offset
    ForGPrepNext,
    /// NATIVECALL: start executing new function in native code (pseudo-instruction,
    /// never emitted by the compiler; constructed at runtime for native dispatch)
    NativeCall,
    /// GETVARARGS: copy variables into the target register from vararg storage
    /// A: target register, B: variable count + 1, or 0 (MULTRET)
    GetVarargs,
    /// DUPCLOSURE: create closure from a pre-created function object
    /// A: target register, D: constant table index
    DupClosure,
    /// PREPVARARGS: prepare stack for variadic functions
    /// A: number of fixed arguments
    PrepVarargs,
    /// LOADKX: sets register to an entry from the constant table
    /// A: target register, AUX: constant table index
    LoadKx,
    /// JUMPX: extended jump
    /// E: jump offset (-2^23..2^23; 0 = "next instruction")
    JumpX,
    /// FASTCALL: perform a fast call of a built-in function
    /// A: builtin function id, C: jump offset to following CALL
    /// Followed by one of (GETIMPORT, MOVE, GETUPVAL) and a CALL instruction.
    FastCall,
    /// COVERAGE: update coverage information stored in the instruction
    /// E: hit count (0..2^23-1)
    Coverage,
    /// CAPTURE: capture a local or upvalue into a newly created closure (after NEWCLOSURE)
    /// A: capture type (see CaptureType), B: source register or upvalue index
    Capture,
    /// SUBRK/DIVRK: arithmetic with constant as first operand
    /// A: target, B: constant index (0..255), C: source register
    SubRk,
    DivRk,
    /// FASTCALL1: fast call with 1 register argument
    /// A: builtin id, B: source argument register, C: jump offset to following CALL
    FastCall1,
    /// FASTCALL2: fast call with 2 register arguments
    /// A: builtin id, B: source argument register, C: jump offset, AUX: source register 2 (low byte)
    FastCall2,
    /// FASTCALL2K: fast call with 1 register argument and 1 constant argument
    /// A: builtin id, B: source argument register, C: jump offset, AUX: constant index
    FastCall2K,
    /// FORGPREP: prepare loop variables for a generic for loop
    /// A: target register, D: jump offset
    ForGPrep,
    /// JUMPXEQKNIL/JUMPXEQKB: comparison jumps against nil/boolean constants
    /// A: source register, D: jump offset,
    /// AUX: constant value (boolean, low bit) + NOT flag (high bit)
    JumpXEqKNil,
    JumpXEqKB,
    /// JUMPXEQKN/JUMPXEQKS: comparison jumps against number/string constants
    /// A: source register, D: jump offset,
    /// AUX: constant table index in low 24 bits + NOT flag in high bit
    JumpXEqKN,
    JumpXEqKS,
    /// IDIV: floor division between two registers
    /// A: target, B: source 1, C: source 2
    IDiv,
    /// IDIVK: floor division between register and constant
    /// A: target, B: source, C: constant index (0..255)
    IDivK,
    /// Atom-based userdata field access acceleration (AUX low 16 bits = constant index,
    /// upper 16 bits = runtime cache)
    GetUdataKs,
    SetUdataKs,
    NamecallUdata,
    /// NEWCLASSMEMBER: register a method on a class object (experimental, WIP bytecode)
    /// A: source register of class, B: member visibility, C: initial value,
    /// AUX: member name constant (public) or private method index (private)
    NewClassMember,
    /// CALLFB: call with collecting runtime stats in a feedback slot (experimental)
    /// A: register of function + args, B: arg count + 1 or 0, C: result count + 1 or 0,
    /// AUX: feedback slot id (0xFFFFFFFF = sealed)
    CallFb,
    /// CMPPROTO: check if register contains a closure with a specified proto id (experimental)
    /// A: closure register, D: jump offset if proto doesn't match, AUX: proto id
    CmpProto,
    /// FASTPCALL: fastcall of a built-in protected call function (experimental)
    /// A: protected function id (0 = pcall, 1 = xpcall), B: explicit arg count, C: jump offset
    FastPCall,

    // ---- WIP bytecode versions (>= 100) ----

    /// NEWCLASS: reify a class object (experimental)
    NewClass,
    /// CONSTRUCT: duplicate table into ra using a constant template (experimental)
    Construct,
    /// FINCONSTRUCT: finish object construction started by CONSTRUCT (experimental)
    FinConstruct,
    /// GETPRIVATEFIELD: retrieve value of a private field (experimental)
    GetPrivateField,
    /// SETPRIVATEFIELD: set value of a private field (experimental)
    SetPrivateField,

    /// Sentinel: number of opcodes, not a valid opcode itself.
    Count,
}

impl Opcode {
    /// Number of opcodes (mirrors `LOP__COUNT`).
    pub const COUNT: u8 = Opcode::Count as u8;
}

/// Bytecode instruction header helpers.
///
/// Instruction words are 32-bit integers; the opcode occupies the low byte.
pub mod insn {
    /// Opcode of an instruction word (`LUAU_INSN_OP`).
    #[inline]
    pub fn op(insn: u32) -> u32 {
        insn & 0xff
    }

    /// ABC encoding: three 8-bit values (`LUAU_INSN_A/B/C`).
    #[inline]
    pub fn a(insn: u32) -> u32 {
        (insn >> 8) & 0xff
    }

    #[inline]
    pub fn b(insn: u32) -> u32 {
        (insn >> 16) & 0xff
    }

    #[inline]
    pub fn c(insn: u32) -> u32 {
        (insn >> 24) & 0xff
    }

    /// AD encoding: one 8-bit value + signed 16-bit value (`LUAU_INSN_D`).
    #[inline]
    pub fn d(insn: u32) -> i32 {
        (insn as i32) >> 16
    }

    /// E encoding: one signed 24-bit value (`LUAU_INSN_E`).
    #[inline]
    pub fn e(insn: u32) -> i32 {
        (insn as i32) >> 8
    }

    /// Auxiliary AB: two 8-bit values (used in FASTCALL3).
    #[inline]
    pub fn aux_a(aux: u32) -> u32 {
        aux & 0xff
    }

    #[inline]
    pub fn aux_b(aux: u32) -> u32 {
        (aux >> 8) & 0xff
    }

    /// Auxiliary KV: unsigned 24-bit constant index (JUMPXEQK* instructions).
    #[inline]
    pub fn aux_kv(aux: u32) -> u32 {
        aux & 0xffffff
    }

    /// Auxiliary KB: 1-bit constant value (JUMPXEQKB).
    #[inline]
    pub fn aux_kb(aux: u32) -> u32 {
        aux & 1
    }

    /// Auxiliary NOT: 1-bit negation flag (JUMPXEQK*).
    #[inline]
    pub fn aux_not(aux: u32) -> u32 {
        aux >> 31
    }

    /// Auxiliary 16-bit constant index and slot (UDATA instructions).
    #[inline]
    pub fn aux_kv16(aux: u32) -> u32 {
        aux & 0xffff
    }

    #[inline]
    pub fn aux_slot(aux: u32) -> u32 {
        aux >> 16
    }

    /// Auxiliary static flag (GETPRIVATEFIELD).
    #[inline]
    pub fn aux_static(aux: u32) -> u32 {
        (aux >> 8) & 1
    }

    /// Auxiliary namecall flag (GETPRIVATEFIELD).
    #[inline]
    pub fn aux_namecall(aux: u32) -> u32 {
        (aux >> 9) & 1
    }

    /// Feedback slot id meaning "sealed" (`LUAU_INSN_FBSLOT_SEALED`).
    pub const FB_SLOT_SEALED: u32 = 0xffff_ffff;
}

/// Bytecode tags used for serialized bytecode / constant table entries.
///
/// Order-sensitive.
pub mod tag {
    /// Bytecode version; runtime supports [MIN, MAX], compiler emits TARGET by default.
    pub const VERSION_MIN: u16 = 3;
    pub const VERSION_MAX: u16 = 14;
    pub const VERSION_TARGET: u16 = 13;
    /// WIP version for experimental class support.
    pub const VERSION_CLASSES: u16 = 100;

    /// Type encoding version.
    pub const TYPE_VERSION_MIN: u16 = 1;
    pub const TYPE_VERSION_MAX: u16 = 3;
    pub const TYPE_VERSION_TARGET: u16 = 3;

    /// Types of constant table entries. Order-sensitive.
    pub const CONSTANT_NIL: u8 = 0;
    pub const CONSTANT_BOOLEAN: u8 = 1;
    pub const CONSTANT_NUMBER: u8 = 2;
    pub const CONSTANT_STRING: u8 = 3;
    pub const CONSTANT_IMPORT: u8 = 4;
    pub const CONSTANT_TABLE: u8 = 5;
    pub const CONSTANT_CLOSURE: u8 = 6;
    pub const CONSTANT_VECTOR: u8 = 7;
    pub const CONSTANT_TABLE_WITH_CONSTANTS: u8 = 8;
    pub const CONSTANT_INTEGER: u8 = 9;
    pub const CONSTANT_CLASS_SHAPE: u8 = 10;
    pub const CONSTANT_VECTORD: u8 = 11;

    /// Number of constant types (`LBC_CONSTANT__COUNT`).
    pub const CONSTANT_COUNT: u8 = 12;
}

/// Bytecode type tags for serialized type information.
pub mod type_tag {
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
    pub const CLASS: u8 = 11;
    pub const OBJECT: u8 = 12;

    pub const ANY: u8 = 15;

    pub const TAGGED_USERDATA_BASE: u8 = 64;
    pub const TAGGED_USERDATA_END: u8 = 64 + 32;

    pub const OPTIONAL_BIT: u8 = 1 << 7;

    pub const INVALID: u16 = 256;
}

/// Builtin function ids, used in FASTCALL opcodes. Order-sensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BuiltinFunction {
    None = 0,

    // assert()
    Assert,

    // math.
    MathAbs,
    MathAcos,
    MathAsin,
    MathAtan2,
    MathAtan,
    MathCeil,
    MathCosh,
    MathCos,
    MathDeg,
    MathExp,
    MathFloor,
    MathFmod,
    MathFrexp,
    MathLdexp,
    MathLog10,
    MathLog,
    MathMax,
    MathMin,
    MathModf,
    MathPow,
    MathRad,
    MathSinh,
    MathSin,
    MathSqrt,
    MathTanh,
    MathTan,

    // bit32.
    Bit32Arshift,
    Bit32Band,
    Bit32Bnot,
    Bit32Bor,
    Bit32Bxor,
    Bit32Btest,
    Bit32Extract,
    Bit32Lrotate,
    Bit32Lshift,
    Bit32Replace,
    Bit32Rrotate,
    Bit32Rshift,

    // type()
    Type,

    // string.
    StringByte,
    StringChar,
    StringLen,

    // typeof()
    Typeof,

    // string.
    StringSub,

    // math.
    MathClamp,
    MathSign,
    MathRound,

    // raw*
    Rawset,
    Rawget,
    Rawequal,

    // table.
    TableInsert,
    TableUnpack,

    // vector ctor
    Vector,

    // bit32.count
    Bit32Countlz,
    Bit32Countrz,

    // select(_, ...)
    SelectVararg,

    // rawlen
    Rawlen,

    // bit32.extract(_, k, k)
    Bit32ExtractK,

    // get/setmetatable
    Getmetatable,
    Setmetatable,

    // tonumber/tostring
    Tonumber,
    Tostring,

    // bit32.byteswap(n)
    Bit32Byteswap,

    // buffer.
    BufferReadI8,
    BufferReadU8,
    BufferWriteU8,
    BufferReadI16,
    BufferReadU16,
    BufferWriteU16,
    BufferReadI32,
    BufferReadU32,
    BufferWriteU32,
    BufferReadF32,
    BufferWriteF32,
    BufferReadF64,
    BufferWriteF64,

    // vector.
    VectorMagnitude,
    VectorNormalize,
    VectorCross,
    VectorDot,
    VectorFloor,
    VectorCeil,
    VectorAbs,
    VectorSign,
    VectorClamp,
    VectorMin,
    VectorMax,

    // math.lerp
    MathLerp,

    // vector.lerp
    VectorLerp,

    // math.
    MathIsnan,
    MathIsinf,
    MathIsfinite,

    // integer
    IntegerCreate,
    IntegerToNumber,
    IntegerNeg,
    IntegerAdd,
    IntegerSub,
    IntegerMul,
    IntegerDiv,
    IntegerMin,
    IntegerMax,
    IntegerRem,
    IntegerIDiv,
    IntegerUDiv,
    IntegerURem,
    IntegerMod,
    IntegerClamp,
    IntegerBand,
    IntegerBor,
    IntegerBnot,
    IntegerBxor,
    IntegerLt,
    IntegerLe,
    IntegerUlt,
    IntegerUle,
    IntegerGt,
    IntegerGe,
    IntegerUgt,
    IntegerUge,
    IntegerLshift,
    IntegerRshift,
    IntegerArshift,
    IntegerLrotate,
    IntegerRrotate,
    IntegerExtract,
    IntegerBtest,
    IntegerCountrz,
    IntegerCountlz,
    IntegerBswap,

    // buffer.readinteger / buffer.writeinteger (int64)
    BufferReadInteger,
    BufferWriteInteger,

    // table.create / table.clear
    TableCreate,
    TableClear,
}

impl BuiltinFunction {}
/// Capture type, used in LOP_CAPTURE. Order-sensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CaptureType {
    Val = 0,
    Ref,
    Upval,
}

impl CaptureType {}

/// Class member visibility, used in LOP_NEWCLASSMEMBER.
/// Encoded in the bottom bit of B; the upper 7 bits are reserved and must be 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ClassMemberVisibility {
    Public = 0,
    Private = 1,
}

/// Proto flag bitmask, stored in `Proto::flags`.
pub mod proto_flag {
    /// Main proto of modules with `--!native`.
    pub const NATIVE_MODULE: u32 = 1 << 0;
    /// Individual protos not profitable to compile natively.
    pub const NATIVE_COLD: u32 = 1 << 1;
    /// Main proto of modules with at least one function with native attribute.
    pub const NATIVE_FUNCTION: u32 = 1 << 2;
    /// Function can be inlined.
    pub const INLINABLE: u32 = 1 << 3;
    /// Top-level function uses export statements and returns the export table.
    pub const USES_EXPORT: u32 = 1 << 4;
}

/// Feedback vector type. Order-sensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum FeedbackType {
    CallTarget = 0,
    /// Experimental
    Construct = 1,
}

/// Port of `Luau::getOpLength` from `BytecodeUtils.h`: number of 32-bit
/// words an instruction occupies (instructions with an AUX word take 2).
#[inline]
pub fn get_op_length(op: Opcode) -> u32 {
    match op {
        Opcode::GetGlobal
        | Opcode::SetGlobal
        | Opcode::GetImport
        | Opcode::GetTableKs
        | Opcode::SetTableKs
        | Opcode::Namecall
        | Opcode::JumpIfEq
        | Opcode::JumpIfLe
        | Opcode::JumpIfLt
        | Opcode::JumpIfNotEq
        | Opcode::JumpIfNotLe
        | Opcode::JumpIfNotLt
        | Opcode::NewTable
        | Opcode::SetList
        | Opcode::ForGLoop
        | Opcode::LoadKx
        | Opcode::FastCall2
        | Opcode::FastCall2K
        | Opcode::FastCall3
        | Opcode::JumpXEqKNil
        | Opcode::JumpXEqKB
        | Opcode::JumpXEqKN
        | Opcode::JumpXEqKS
        | Opcode::GetUdataKs
        | Opcode::SetUdataKs
        | Opcode::NamecallUdata
        | Opcode::NewClassMember
        | Opcode::CallFb
        | Opcode::CmpProto
        | Opcode::NewClass
        | Opcode::Construct
        | Opcode::GetPrivateField
        | Opcode::SetPrivateField => 2,

        _ => 1,
    }
}

dense_u8_enum!(Opcode, 95);
dense_u8_enum!(BuiltinFunction, 135);
dense_u8_enum!(CaptureType, 3);
