//! Unit tests for the optimizer passes, built directly through `IrBuilder`.

use super::super::ir::builder::IrBuilder;
use super::super::ir::instruction::{Cmp, Op, Ref};
use super::super::ir::types::IrType;
use super::super::ir::validate;
use super::super::trace::{Trace, TraceId};
use super::*;

fn trace() -> IrBuilder {
    IrBuilder::new(Trace::new(TraceId(1), std::ptr::null_mut(), 0), 8, 0)
}

#[test]
fn folds_number_constants() {
    let mut b = trace();
    let a = b.knum(1.5);
    let c = b.knum(2.5);
    let sum = b.emit(Op::Add(a, c), IrType::Number);
    let mut t = b.into_trace();
    assert!(constant_folding::run(&mut t));
    assert!(matches!(t.inst(sum).op, Op::KNum(n) if n == 4.0));
    validate(&t).unwrap();
}

#[test]
fn folds_constants_in_place_and_keeps_refs() {
    let mut b = trace();
    let a = b.knum(2.0);
    let c = b.knum(3.0);
    let mul = b.emit(Op::Mul(a, c), IrType::Number);
    let mut t = b.into_trace();
    let before = t.insts.len();
    assert!(constant_folding::run(&mut t));
    assert_eq!(t.insts.len(), before);
    assert!(matches!(t.inst(mul).op, Op::KNum(n) if n == 6.0));
    validate(&t).unwrap();
}

#[test]
fn dead_constants_become_nops() {
    let mut b = trace();
    let unused = b.knum(7.0);
    let mut t = b.into_trace();
    assert!(dce::run(&mut t));
    assert!(matches!(t.inst(unused).op, Op::Nop));
    validate(&t).unwrap();
}

#[test]
fn copy_propagation_removes_mul_by_one() {
    let mut b = trace();
    let x = b.slot(0, IrType::Number);
    let one = b.knum(1.0);
    let y = b.emit(Op::Mul(x, one), IrType::Number);
    let mut t = b.into_trace();
    assert!(copy_propagation::run(&mut t));
    assert!(matches!(t.inst(y).op, Op::Nop));
    validate(&t).unwrap();
}

#[test]
fn strength_reduction_turns_mul2_into_add() {
    let mut b = trace();
    let x = b.slot(0, IrType::Number);
    let two = b.knum(2.0);
    let y = b.emit(Op::Mul(x, two), IrType::Number);
    let mut t = b.into_trace();
    assert!(strength::run(&mut t));
    assert!(matches!(t.inst(y).op, Op::Add(a, b) if a == x && b == x));
    validate(&t).unwrap();
}

#[test]
fn cse_reuses_the_first_equal_expression() {
    let mut b = trace();
    let x = b.slot(0, IrType::Number);
    let one = b.knum(1.0);
    let a = b.emit(Op::Add(x, one), IrType::Number);
    let c = b.emit(Op::Add(x, one), IrType::Number);
    let mut t = b.into_trace();
    assert!(cse::run(&mut t));
    assert!(matches!(t.inst(a).op, Op::Add(..)));
    assert!(matches!(t.inst(c).op, Op::Nop));
    validate(&t).unwrap();
}

#[test]
fn forwards_a_store_to_a_load_from_the_same_pointer() {
    let mut b = trace();
    let table = b.kptr(0x1000, IrType::Table);
    let idx = b.ki32(0);
    let p = b.emit(Op::ARef { t: table, idx }, IrType::Ptr);
    let v = b.knum(7.0);
    b.emit(Op::XStore { p, v }, IrType::Nil);
    let snap = b.snapshot();
    let load = b.emit(Op::XLoad { p, snap }, IrType::Number);
    let mut t = b.into_trace();
    assert!(forwarding::run(&mut t));
    assert!(matches!(t.inst(load).op, Op::Nop));
    validate(&t).unwrap();
}

#[test]
fn licm_hoists_into_a_dead_pre_loop_slot() {
    let mut b = trace();
    let x = b.slot(0, IrType::Number);
    let two = b.knum(2.0);
    let dead = b.knum(99.0);
    let lp = b.emit(Op::Loop, IrType::Nil);
    let mul = b.emit(Op::Mul(x, two), IrType::Number);
    let out = b.emit(Op::Add(mul, two), IrType::Number);
    let snap = b.snapshot();
    b.emit(Op::GuardTruthy { a: out, want: true, snap }, IrType::Nil);
    let mut t = b.into_trace();
    t.loop_ref = Some(lp);
    dce::run(&mut t);
    assert!(matches!(t.inst(dead).op, Op::Nop));
    assert!(licm::run(&mut t));
    assert!(matches!(t.inst(mul).op, Op::Nop));
    assert!(matches!(t.inst(dead).op, Op::Mul(..)));
    validate(&t).unwrap();
}

#[test]
fn full_pipeline_keeps_ir_valid() {
    let mut b = trace();
    let x = b.slot(0, IrType::Number);
    let one = b.knum(1.0);
    let mul = b.emit(Op::Mul(x, one), IrType::Number);
    let dead = b.emit(Op::Add(mul, one), IrType::Number);
    let _ = dead;
    let mut t = b.into_trace();
    optimize_checked(&mut t).unwrap();
    assert!(matches!(t.inst(mul).op, Op::Nop));
    validate(&t).unwrap();
}

#[test]
fn guard_folding_keeps_snapshot_ids() {
    let mut b = trace();
    let x = b.slot(0, IrType::Number);
    let one = b.knum(1.0);
    let two = b.knum(2.0);
    let snap = b.snapshot();
    let guard = b.emit(Op::Guard { cmp: Cmp::Lt, a: one, b: two, snap }, IrType::Nil);
    let mut t = b.into_trace();
    assert!(constant_folding::run(&mut t));
    assert!(matches!(t.inst(guard).op, Op::Nop));
    assert!(matches!(t.inst(x).op, Op::SLoad { .. }));
    validate(&t).unwrap();
}


#[test]
fn copy_propagation_remaps_snapshot_values() {
    let mut b = trace();
    let x = b.slot(0, IrType::Number);
    let one = b.knum(1.0);
    let y = b.emit(Op::Mul(x, one), IrType::Number);
    b.set_slot(1, y);
    let snap = b.snapshot();
    let mut t = b.into_trace();
    assert!(copy_propagation::run(&mut t));
    let saved = t.snapshots[snap.0 as usize]
        .entries
        .iter()
        .find(|e| e.slot == 1)
        .expect("slot 1 snapshot entry");
    assert_eq!(saved.value, x);
    validate(&t).unwrap();
}

#[test]
fn dce_keeps_a_value_named_by_a_snapshot() {
    let mut b = trace();
    let v = b.knum(5.0);
    b.set_slot(0, v);
    let _snap = b.snapshot();
    let mut t = b.into_trace();
    assert!(!dce::run(&mut t));
    assert!(matches!(t.inst(v).op, Op::KNum(n) if n == 5.0));
}

#[test]
fn forwarding_clears_on_a_store_to_another_pointer() {
    let mut b = trace();
    let p = b.kptr(0x1000, IrType::Ptr);
    let q = b.kptr(0x2000, IrType::Ptr);
    let v = b.knum(1.0);
    let w = b.knum(2.0);
    b.emit(Op::XStore { p, v }, IrType::Nil);
    b.emit(Op::XStore { p: q, v: w }, IrType::Nil);
    let snap = b.snapshot();
    let load = b.emit(Op::XLoad { p, snap }, IrType::Number);
    let mut t = b.into_trace();
    assert!(!forwarding::run(&mut t));
    assert!(matches!(t.inst(load).op, Op::XLoad { .. }));
    validate(&t).unwrap();
}

#[test]
fn folds_a_phi_whose_two_inputs_are_equal_constants() {
    let mut b = trace();
    let pre = b.knum(3.0);
    let lp = b.emit(Op::Loop, IrType::Nil);
    let phi = b.emit(Op::Nop, IrType::Number);
    let back = b.emit(Op::KNum(3.0), IrType::Number);
    set_phi(&mut b, phi, pre, back);
    let mut t = b.into_trace();
    t.loop_ref = Some(lp);
    assert!(constant_folding::run(&mut t));
    assert!(matches!(t.inst(phi).op, Op::KNum(n) if n == 3.0));
    validate(&t).unwrap();
}

fn set_phi(b: &mut IrBuilder, phi: Ref, pre: Ref, back: Ref) {
    b.trace.insts[phi.index()].op = Op::Phi { pre, back };
}


#[test]
fn licm_leaves_constants_in_the_body() {
    let mut b = trace();
    let dead = b.knum(99.0);
    let lp = b.emit(Op::Loop, IrType::Nil);
    let k = b.emit(Op::KNum(3.0), IrType::Number);
    let snap = b.snapshot();
    b.emit(Op::GuardTruthy { a: k, want: true, snap }, IrType::Nil);
    let mut t = b.into_trace();
    t.loop_ref = Some(lp);
    dce::run(&mut t);
    assert!(matches!(t.inst(dead).op, Op::Nop));
    assert!(!licm::run(&mut t));
    assert!(matches!(t.inst(k).op, Op::KNum(n) if n == 3.0));
    validate(&t).unwrap();
}
