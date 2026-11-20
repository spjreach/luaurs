//! AST traversal with the C++ `AstVisitor` contract: each `visit_*` hook
//! returns whether to descend into the node's children. Child order matches
//! `Ast/src/Ast.cpp` (and `luau_ast::ast::AstVisitor`). Type annotations are
//! not traversed: no compiler pass looks at them.
//!
//! Blocks are statements in C++ (`AstStatBlock : AstStat`); here a nested
//! block (`if` body, function body, ...) is reported through `visit_block`
//! and a block statement through `visit_stat` followed by `visit_block`.

use luau_ast::ast::*;

pub trait Visitor<'a> {
    fn visit_expr(&mut self, _e: &'a AstExpr) -> bool {
        true
    }
    fn visit_stat(&mut self, _s: &'a AstStat) -> bool {
        true
    }
    fn visit_block(&mut self, _b: &'a AstStatBlock) -> bool {
        true
    }
    /// A function body (`AstExprFunction`), reached as an expression or via
    /// `function`/`local function` statements.
    fn visit_function(&mut self, _f: &'a AstExprFunction) -> bool {
        true
    }
}

pub fn walk_block<'a, V: Visitor<'a> + ?Sized>(v: &mut V, b: &'a AstStatBlock) {
    if v.visit_block(b) {
        for s in &b.body {
            walk_stat(v, s);
        }
    }
}

pub fn walk_function<'a, V: Visitor<'a> + ?Sized>(v: &mut V, f: &'a AstExprFunction) {
    if v.visit_function(f) {
        walk_block(v, &f.body);
    }
}

pub fn walk_expr<'a, V: Visitor<'a> + ?Sized>(v: &mut V, e: &'a AstExpr) {
    if !v.visit_expr(e) {
        return;
    }
    match e {
        AstExpr::Group(n) => walk_expr(v, &n.expr),
        AstExpr::Call(n) => {
            walk_expr(v, &n.func);
            for a in &n.args {
                walk_expr(v, a);
            }
        }
        AstExpr::IndexName(n) => walk_expr(v, &n.expr),
        AstExpr::IndexExpr(n) => {
            walk_expr(v, &n.expr);
            walk_expr(v, &n.index);
        }
        AstExpr::Function(f) => walk_function(v, f),
        AstExpr::Table(n) => {
            for item in &n.items {
                if let Some(k) = &item.key {
                    walk_expr(v, k);
                }
                walk_expr(v, &item.value);
            }
        }
        AstExpr::Unary(n) => walk_expr(v, &n.expr),
        AstExpr::Binary(n) => {
            walk_expr(v, &n.left);
            walk_expr(v, &n.right);
        }
        AstExpr::TypeAssertion(n) => walk_expr(v, &n.expr),
        AstExpr::IfElse(n) => {
            walk_expr(v, &n.condition);
            walk_expr(v, &n.true_expr);
            walk_expr(v, &n.false_expr);
        }
        AstExpr::InterpString(n) => {
            for x in &n.expressions {
                walk_expr(v, x);
            }
        }
        AstExpr::Instantiate(n) => walk_expr(v, &n.expr),
        AstExpr::Error(n) => {
            for x in &n.expressions {
                walk_expr(v, x);
            }
        }
        _ => {}
    }
}

pub fn walk_stat<'a, V: Visitor<'a> + ?Sized>(v: &mut V, s: &'a AstStat) {
    if !v.visit_stat(s) {
        return;
    }
    match s {
        AstStat::Block(b) => walk_block(v, b),
        AstStat::If(n) => {
            walk_expr(v, &n.condition);
            walk_block(v, &n.thenbody);
            if let Some(e) = &n.elsebody {
                walk_stat(v, e);
            }
        }
        AstStat::While(n) => {
            walk_expr(v, &n.condition);
            walk_block(v, &n.body);
        }
        AstStat::Repeat(n) => {
            walk_block(v, &n.body);
            walk_expr(v, &n.condition);
        }
        AstStat::Return(n) => {
            for x in &n.list {
                walk_expr(v, x);
            }
        }
        AstStat::Expr(n) => walk_expr(v, &n.expr),
        AstStat::Local(n) => {
            for x in &n.values {
                walk_expr(v, x);
            }
        }
        AstStat::For(n) => {
            walk_expr(v, &n.from);
            walk_expr(v, &n.to);
            if let Some(st) = &n.step {
                walk_expr(v, st);
            }
            walk_block(v, &n.body);
        }
        AstStat::ForIn(n) => {
            for x in &n.values {
                walk_expr(v, x);
            }
            walk_block(v, &n.body);
        }
        AstStat::Assign(n) => {
            for x in &n.vars {
                walk_expr(v, x);
            }
            for x in &n.values {
                walk_expr(v, x);
            }
        }
        AstStat::CompoundAssign(n) => {
            walk_expr(v, &n.var);
            walk_expr(v, &n.value);
        }
        AstStat::Function(n) => {
            walk_expr(v, &n.name);
            walk_function(v, &n.func);
        }
        AstStat::LocalFunction(n) => walk_function(v, &n.func),
        AstStat::TypeFunction(n) => walk_function(v, &n.body),
        AstStat::Class(n) => {
            if let Some(sup) = &n.super_ {
                walk_expr(v, sup);
            }
            for m in &n.members {
                if let AstClassMember::Method(method) = m {
                    walk_function(v, &method.function);
                }
            }
        }
        _ => {}
    }
}

/// Address of a node, used as its identity (C++ keys analysis maps by node
/// pointer). Nodes are boxed, so addresses are stable while the AST lives.
#[inline]
pub fn key<T>(node: &T) -> usize {
    node as *const T as usize
}

/// `unwrapExprOfType`: strips groups and type assertions.
pub fn unwrap_expr(e: &AstExpr) -> &AstExpr {
    match e {
        AstExpr::Group(g) => unwrap_expr(&g.expr),
        AstExpr::TypeAssertion(t) => unwrap_expr(&t.expr),
        other => other,
    }
}
