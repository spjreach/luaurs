#![feature(thread_local)]
//! `luau-common` — Rust port of Luau's `Common` module.
//!
//! Contains the bytecode definitions (`bytecode`) that are shared between the
//! compiler and the virtual machine, plus small shared utilities.

pub mod bytecode;
pub mod fasthash;
pub mod fastalloc;

#[cfg(test)]
mod tests {
    use super::bytecode::*;

    /// Order-sensitivity checks against the C++ `LuauOpcode` enum.
    #[test]
    fn opcode_discriminants() {
        use Opcode as O;
        assert_eq!(O::Nop as u8, 0);
        assert_eq!(O::Break as u8, 1);
        assert_eq!(O::LoadNil as u8, 2);
        assert_eq!(O::LoadK as u8, 5);
        assert_eq!(O::Move as u8, 6);
        assert_eq!(O::GetGlobal as u8, 7);
        assert_eq!(O::SetGlobal as u8, 8);
        assert_eq!(O::GetImport as u8, 12);
        assert_eq!(O::Namecall as u8, 20);
        assert_eq!(O::Call as u8, 21);
        assert_eq!(O::Return as u8, 22);
        assert_eq!(O::Jump as u8, 23);
        assert_eq!(O::JumpBack as u8, 24);
        assert_eq!(O::JumpIf as u8, 25);
        assert_eq!(O::JumpIfEq as u8, 27);
        assert_eq!(O::Add as u8, 33);
        assert_eq!(O::AddK as u8, 39);
        assert_eq!(O::And as u8, 45);
        assert_eq!(O::Concat as u8, 49);
        assert_eq!(O::Not as u8, 50);
        assert_eq!(O::NewTable as u8, 53);
        assert_eq!(O::DupTable as u8, 54);
        assert_eq!(O::SetList as u8, 55);
        assert_eq!(O::ForNPrep as u8, 56);
        assert_eq!(O::ForNLoop as u8, 57);
        assert_eq!(O::ForGLoop as u8, 58);
        assert_eq!(O::ForGPrepInext as u8, 59);
        assert_eq!(O::FastCall3 as u8, 60);
        assert_eq!(O::ForGPrepNext as u8, 61);
        assert_eq!(O::NativeCall as u8, 62);
        assert_eq!(O::GetVarargs as u8, 63);
        assert_eq!(O::DupClosure as u8, 64);
        assert_eq!(O::PrepVarargs as u8, 65);
        assert_eq!(O::LoadKx as u8, 66);
        assert_eq!(O::JumpX as u8, 67);
        assert_eq!(O::FastCall as u8, 68);
        assert_eq!(O::Coverage as u8, 69);
        assert_eq!(O::Capture as u8, 70);
        assert_eq!(O::SubRk as u8, 71);
        assert_eq!(O::DivRk as u8, 72);
        assert_eq!(O::FastCall1 as u8, 73);
        assert_eq!(O::FastCall2 as u8, 74);
        assert_eq!(O::FastCall2K as u8, 75);
        assert_eq!(O::ForGPrep as u8, 76);
        assert_eq!(O::JumpXEqKNil as u8, 77);
        assert_eq!(O::JumpXEqKB as u8, 78);
        assert_eq!(O::JumpXEqKN as u8, 79);
        assert_eq!(O::JumpXEqKS as u8, 80);
        assert_eq!(O::IDiv as u8, 81);
        assert_eq!(O::IDivK as u8, 82);
        assert_eq!(O::GetUdataKs as u8, 83);
        assert_eq!(O::SetUdataKs as u8, 84);
        assert_eq!(O::NamecallUdata as u8, 85);
        assert_eq!(O::NewClassMember as u8, 86);
        assert_eq!(O::CallFb as u8, 87);
        assert_eq!(O::CmpProto as u8, 88);
        assert_eq!(O::FastPCall as u8, 89);
        assert_eq!(O::NewClass as u8, 90);
        assert_eq!(O::Construct as u8, 91);
        assert_eq!(O::FinConstruct as u8, 92);
        assert_eq!(O::GetPrivateField as u8, 93);
        assert_eq!(O::SetPrivateField as u8, 94);
        assert_eq!(O::Count as u8, 95);
    }

    #[test]
    fn builtin_discriminants() {
        use BuiltinFunction as B;
        assert_eq!(B::None as u8, 0);
        assert_eq!(B::Assert as u8, 1);
        assert_eq!(B::MathAbs as u8, 2);
        assert_eq!(B::MathTan as u8, 27);
        assert_eq!(B::Bit32Arshift as u8, 28);
        assert_eq!(B::Bit32Rshift as u8, 39);
        assert_eq!(B::Type as u8, 40);
        assert_eq!(B::StringByte as u8, 41);
        assert_eq!(B::Typeof as u8, 44);
        assert_eq!(B::StringSub as u8, 45);
        assert_eq!(B::MathClamp as u8, 46);
        assert_eq!(B::Rawset as u8, 49);
        assert_eq!(B::TableInsert as u8, 52);
        assert_eq!(B::Vector as u8, 54);
        assert_eq!(B::Bit32Countlz as u8, 55);
        assert_eq!(B::SelectVararg as u8, 57);
        assert_eq!(B::Rawlen as u8, 58);
        assert_eq!(B::Bit32ExtractK as u8, 59);
        assert_eq!(B::Getmetatable as u8, 60);
        assert_eq!(B::Tonumber as u8, 62);
        assert_eq!(B::Bit32Byteswap as u8, 64);
        assert_eq!(B::BufferReadI8 as u8, 65);
        assert_eq!(B::BufferWriteF64 as u8, 77);
        assert_eq!(B::VectorMagnitude as u8, 78);
        assert_eq!(B::MathLerp as u8, 89);
        assert_eq!(B::VectorLerp as u8, 90);
        assert_eq!(B::MathIsnan as u8, 91);
        assert_eq!(B::IntegerCreate as u8, 94);
        assert_eq!(B::BufferReadInteger as u8, 131);
        assert_eq!(B::TableCreate as u8, 133);
        assert_eq!(B::TableClear as u8, 134);
    }

    #[test]
    fn insn_field_extraction() {
        // LOADN with A=3, D=-12: op 4, A 3, D -12 => (4) | (3 << 8) | ((-12u32 as u16) << 16)
        let insn: u32 = (b'\x04' as u32) | (3 << 8) | ((((-12i32) as u16 as u32)) << 16);
        assert_eq!(insn::op(insn), 4);
        assert_eq!(insn::a(insn), 3);
        assert_eq!(insn::d(insn), -12);

        // E encoding: signed 24-bit occupying bits 8..31, sign-extended via the top byte.
        let insn: u32 = (-5i32 as u32) << 8;
        assert_eq!(insn::e(insn), -5);
        let insn: u32 = 8388607u32 << 8; // 2^23 - 1
        assert_eq!(insn::e(insn), 8388607);
    }
}
