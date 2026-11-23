//! IR unit tests: building a loop by hand and validating it.

use super::ir::builder::IrBuilder;
use super::ir::instruction::{Cmp, Op};
use super::ir::types::IrType;
use super::ir::validate;
use super::trace::{Trace, TraceId};

/// `for i = 1, n do s = s + i end`, as the recorder would build it:
/// slot 0 = s, slot 1 = limit, slot 3 = i.
fn sum_loop() -> Trace {
    let mut b = IrBuilder::new(Trace::new(TraceId(1), std::ptr::null_mut(), 5), 8, 5);
    let s0 = b.slot(0, IrType::Number);
    let lim = b.slot(1, IrType::Number);
    let i0 = b.slot(3, IrType::Number);
    let one = b.knum(1.0);
    let lp = b.emit(Op::Loop, IrType::Nil);
    // Phis get their `back` operand once the body is recorded
    let s = b.emit(Op::Phi { pre: s0, back: s0 }, IrType::Number);
    let i = b.emit(Op::Phi { pre: i0, back: i0 }, IrType::Number);
    let s1 = b.emit(Op::Add(s, i), IrType::Number);
    b.set_slot(0, s1);
    let i1 = b.emit(Op::Add(i, one), IrType::Number);
    b.set_slot(3, i1);
    b.pc = 9;
    let snap = b.snapshot();
    b.emit(Op::Guard { cmp: Cmp::Le, a: i1, b: lim, snap }, IrType::Nil);
    let mut t = b.into_trace();
    t.insts[s.index()].op = Op::Phi { pre: s0, back: s1 };
    t.insts[i.index()].op = Op::Phi { pre: i0, back: i1 };
    t.loop_ref = Some(lp);
    t
}

#[cfg(all(target_arch = "x86_64", windows))]
#[test]
fn hand_built_loop_compiles() {
    let t = sum_loop();
    let code = unsafe { super::compiler::x64::compile(&t, super::compiler::End::Loop) }.expect("sum_loop should compile");
    assert!(!code.bytes.is_empty());
}

#[test]
fn hand_built_loop_validates() {
    let t = sum_loop();
    validate(&t).unwrap();
    let d = t.dump();
    assert!(d.contains("Loop") && d.contains("Phi"), "{d}");
}

#[test]
fn constants_are_interned() {
    let mut b = IrBuilder::new(Trace::new(TraceId(1), std::ptr::null_mut(), 0), 4, 0);
    assert_eq!(b.knum(2.0), b.knum(2.0));
    assert_ne!(b.knum(2.0), b.knum(-2.0));
    assert_eq!(b.ki32(7), b.ki32(7));
}

#[test]
fn snapshot_reused_until_a_write() {
    let mut b = IrBuilder::new(Trace::new(TraceId(1), std::ptr::null_mut(), 0), 4, 0);
    let a = b.slot(0, IrType::Number);
    let s1 = b.snapshot();
    assert_eq!(s1, b.snapshot());
    b.set_slot(1, a);
    assert_ne!(s1, b.snapshot());
}

#[test]
fn validation_catches_use_before_def() {
    let mut t = sum_loop();
    let n = t.insts.len() as u32;
    t.insts.push(super::ir::instruction::Inst { op: Op::Neg(super::ir::instruction::Ref(n + 5)), ty: IrType::Number });
    assert!(validate(&t).is_err());
}

#[test]
fn validation_catches_type_mismatch() {
    let mut b = IrBuilder::new(Trace::new(TraceId(1), std::ptr::null_mut(), 0), 4, 0);
    let s = b.slot(0, IrType::String);
    let one = b.knum(1.0);
    b.emit(Op::Add(s, one), IrType::Number);
    assert!(validate(&b.into_trace()).is_err());
}
