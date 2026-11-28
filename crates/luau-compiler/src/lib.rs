#![feature(thread_local)]
//! `luau-compiler` — Rust port of Luau's Compiler module (AST to bytecode).

pub mod analysis;
pub mod builder;
pub mod compiler;
pub mod types;
pub mod visit;

pub use compiler::{compile, compile_parsed, compile_parsed_with, compile_source, CompileError, CompileOptions};
