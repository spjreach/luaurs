//! `luau-ast` — Rust port of Luau's Ast module (lexer, parser, AST nodes).
//!
//! Ported so far (from `Ast/src` and `Ast/include/Luau`):
//! - `ast` — AST nodes (Ast.h)
//! - `location` — Position/Location (Location.cpp)
//! - `lexeme` — Lexeme token types (Lexer.h)
//! - `name_table` — AstNameTable interner (Lexer.cpp)
//! - `lexer` — full byte-level lexer (Lexer.cpp)
//! - `confusables` — Unicode confusables table + lookup (Confusables.cpp)
//! - `parser` — recursive-descent parser (Parser.cpp)

pub mod ast;
pub mod confusables;
pub mod lexer;
pub mod lexeme;
pub mod location;
pub mod name_table;
pub mod parser;
