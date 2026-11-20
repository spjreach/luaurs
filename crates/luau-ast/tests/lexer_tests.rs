// This file is part of the Luau programming language and is licensed under MIT License; see LICENSE.txt for details
//! Conformance tests for the Rust lexer port, mirroring behavior from
//! `Ast/src/Lexer.cpp` and the upstream `tests/Ast.test.cpp` lexer cases.

use luau_ast::confusables::find_confusable;
use luau_ast::lexer::{fixup_multiline_string, fixup_quoted_string, is_space, Lexer};
use luau_ast::lexeme::{LexemeData, LexemeType, QuoteStyle};
use luau_ast::location::Position;
use luau_ast::name_table::AstNameTable;

fn lex_all(source: &str) -> Vec<LexemeType> {
    let mut names = AstNameTable::new();
    let mut lexer = Lexer::from_source(source.as_bytes(), &mut names);
    std::iter::from_fn(|| {
        let lexeme = lexer.next();
        if matches!(lexeme.ty, LexemeType::Eof) {
            None
        } else {
            Some(lexeme.ty)
        }
    })
    .collect()
}

#[test]
fn basic_statement_tokens() {
    let types = lex_all("local x = 124 + 1.234 -- test comment");
    use LexemeType::*;
    assert_eq!(
        types,
        vec![Reserved("local"), Name, Char(b'='), Number, Char(b'+'), Number, Comment]
    );
}

#[test]
fn all_operators() {
    let types = lex_all("== <= >= ~= .. ... -> :: // += -= *= /= //= %= ^= ..=");
    use LexemeType::*;
    assert_eq!(
        types,
        vec![
            Equal,
            LessEqual,
            GreaterEqual,
            NotEqual,
            Dot2,
            Dot3,
            SkinnyArrow,
            DoubleColon,
            FloorDiv,
            AddAssign,
            SubAssign,
            MulAssign,
            DivAssign,
            FloorDivAssign,
            ModAssign,
            PowAssign,
            ConcatAssign,
        ]
    );
}

#[test]
fn single_char_tokens() {
    let types = lex_all("+-*/%^~<>():;,#?&|{}[].");
    use LexemeType::*;
    assert_eq!(
        types,
        vec![
            Char(b'+'),
            Char(b'-'),
            Char(b'*'),
            Char(b'/'),
            Char(b'%'),
            Char(b'^'),
            Char(b'~'),
            Char(b'<'),
            Char(b'>'),
            Char(b'('),
            Char(b')'),
            Char(b':'),
            Char(b';'),
            Char(b','),
            Char(b'#'),
            Char(b'?'),
            Char(b'&'),
            Char(b'|'),
            Char(b'{'),
            Char(b'}'),
            Char(b'['),
            Char(b']'),
            Char(b'.'),
        ]
    );
}

#[test]
fn reserved_words() {
    let types = lex_all("and break do else elseif end false for function if in local nil not or repeat return then true until while");
    assert_eq!(types.len(), 21);
    for t in &types {
        assert!(t.is_reserved(), "'{t:?}' should be a reserved word");
    }
    // spot check first/last
    assert_eq!(types[0], LexemeType::Reserved("and"));
    assert_eq!(types[20], LexemeType::Reserved("while"));
}

#[test]
fn string_lexemes() {
    let mut names = AstNameTable::new();
    let mut lexer = Lexer::from_source(b"'single' \"double\"", &mut names);

    let l1 = lexer.next();
    assert_eq!(l1.ty, LexemeType::QuotedString);
    assert_eq!(l1.get_quote_style(b"'single' \"double\""), QuoteStyle::Single);
    assert_eq!(l1.get_length(), 6);
    assert_eq!(l1.get_slice(b"'single' \"double\""), b"single");

    let l2 = lexer.next();
    assert_eq!(l2.ty, LexemeType::QuotedString);
    assert_eq!(l2.get_quote_style(b"'single' \"double\""), QuoteStyle::Double);
    assert_eq!(l2.get_length(), 6);
    assert_eq!(l2.get_slice(b"'single' \"double\""), b"double");
}

#[test]
fn broken_string_on_missing_quote() {
    let mut names = AstNameTable::new();
    let mut lexer = Lexer::from_source(b"'unterminated", &mut names);
    let l = lexer.next();
    assert_eq!(l.ty, LexemeType::BrokenString);
}

#[test]
fn long_string_and_block_comment() {
    let mut names = AstNameTable::new();
    let source = b"[[long string]] --[=[block comment]=] x";
    let mut lexer = Lexer::from_source(source, &mut names);

    let l1 = lexer.next();
    assert_eq!(l1.ty, LexemeType::RawString);
    assert_eq!(l1.get_slice(source), b"long string");
    assert_eq!(l1.get_block_depth(source), 0);

    let l2 = lexer.next();
    assert_eq!(l2.ty, LexemeType::BlockComment);
    assert_eq!(l2.get_slice(source), b"block comment");
    assert_eq!(l2.get_block_depth(source), 1);

    let l3 = lexer.next();
    assert_eq!(l3.ty, LexemeType::Name); // x
    assert!(matches!(lexer.next().ty, LexemeType::Eof));
}

#[test]
fn comments() {
    let mut names = AstNameTable::new();
    let source = b"--line comment\n--[==[ multiline ]==] x";
    let mut lexer = Lexer::from_source(source, &mut names);

    let c1 = lexer.next();
    assert_eq!(c1.ty, LexemeType::Comment);
    assert_eq!(c1.get_slice(source), b"line comment");

    let c2 = lexer.next();
    assert_eq!(c2.ty, LexemeType::BlockComment);
    assert_eq!(c2.get_slice(source), b" multiline ");

    let x = lexer.next();
    assert_eq!(x.ty, LexemeType::Name);

    assert!(matches!(lexer.next().ty, LexemeType::Eof));
}

#[test]
fn skip_comments_mode() {
    let mut names = AstNameTable::new();
    let mut lexer = Lexer::from_source(b"-- bye\n1", &mut names);
    lexer.set_skip_comments(true);
    let l = lexer.next();
    assert_eq!(l.ty, LexemeType::Number);
}

#[test]
fn interpolated_strings() {
    let mut names = AstNameTable::new();
    let source = b"`hello {x} world {y} end`";
    let mut lexer = Lexer::from_source(source, &mut names);

    let b = lexer.next();
    assert_eq!(b.ty, LexemeType::InterpStringBegin);
    assert_eq!(b.get_slice(source), b"hello ");
    assert_eq!(lexer.peek_brace_stack_top(), Some(luau_ast::lexer::BraceType::InterpolatedString));

    let x = lexer.next();
    assert_eq!(x.ty, LexemeType::Name);

    let m = lexer.next();
    assert_eq!(m.ty, LexemeType::InterpStringMid);
    assert_eq!(m.get_slice(source), b" world ");

    let y = lexer.next();
    assert_eq!(y.ty, LexemeType::Name);

    let e = lexer.next();
    assert_eq!(e.ty, LexemeType::InterpStringEnd);
    assert_eq!(e.get_slice(source), b" end");
    assert!(matches!(lexer.next().ty, LexemeType::Eof));
}

#[test]
fn interpolated_string_simple_and_broken_double_brace() {
    let mut names = AstNameTable::new();
    let mut lexer = Lexer::from_source(b"`a {{`", &mut names);

    let s = lexer.next();
    assert_eq!(s.ty, LexemeType::BrokenInterpDoubleBrace);
    assert_eq!(s.get_slice(b"`a {{`"), b"a ");

    // The two `{` chars are consumed by the broken lexeme; the terminating
    // backtick begins a new interpolated string section that immediately
    // hits EOF -> BrokenString (matches C++ behavior exactly).
    let t = lexer.next();
    assert_eq!(t.ty, LexemeType::BrokenString);
    assert!(matches!(lexer.next().ty, LexemeType::Eof));
}

#[test]
fn number_lexeme_captures_directive_suffixes() {
    let mut names = AstNameTable::new();
    let source = b"3.14 1e5 0x10 .5 1.2e-3";
    let mut lexer = Lexer::from_source(source, &mut names);

    let n1 = lexer.next();
    assert_eq!(n1.ty, LexemeType::Number);
    assert_eq!(n1.get_slice(source), b"3.14");

    let n2 = lexer.next();
    assert_eq!(n2.get_slice(source), b"1e5");

    let n3 = lexer.next();
    assert_eq!(n3.get_slice(source), b"0x10");

    let n4 = lexer.next();
    assert_eq!(n4.get_slice(source), b".5");

    let n5 = lexer.next();
    assert_eq!(n5.get_slice(source), b"1.2e-3");
}

#[test]
fn identifier_and_position_tracking() {
    let mut names = AstNameTable::new();
    let source = b"local foo_bar = 1\nx += @safe";
    let mut lexer = Lexer::from_source(source, &mut names);

    let l0 = lexer.next();
    assert_eq!(l0.location.begin, Position::new(0, 0));
    assert_eq!(l0.location.end, Position::new(0, 5));

    let id = lexer.next();
    assert_eq!(id.ty, LexemeType::Name);
    assert_eq!(id.location.begin, Position::new(0, 6));
    assert_eq!(id.location.end, Position::new(0, 13));

    // previous_location reports the location of the last token before the
    // current one
    assert_eq!(lexer.previous_location(), l0.location);
}

#[test]
fn attribute_lexeme() {
    let mut names = AstNameTable::new();
    let source = b"@safe";
    let mut lexer = Lexer::from_source(source, &mut names);

    let a = lexer.next();
    assert_eq!(a.ty, LexemeType::Attribute);
    match a.data {
        LexemeData::Name(n) => assert_eq!(n, "safe"),
        _ => panic!("expected name data"),
    }

    let mut names = AstNameTable::new();
    let source = b"@[";
    let mut lexer = Lexer::from_source(source, &mut names);
    assert_eq!(lexer.next().ty, LexemeType::AttributeOpen);
}

#[test]
fn utf8_error_and_confusables() {
    let mut names = AstNameTable::new();
    // U+48 (0) — Cyrillic О looks like O; our table has 48 -> "O"
    let source: &[u8] = b"\xCF\x9F x";
    let mut lexer = Lexer::from_source(source, &mut names);
    let l = lexer.next();
    assert_eq!(l.ty, LexemeType::BrokenUnicode);
    match l.data {
        LexemeData::Codepoint(cp) => assert_eq!(cp, 991), // U+0E9F from \xCF\x9F
        _ => panic!("expected codepoint"),
    }

    assert_eq!(find_confusable(48), Some("O"));
    assert_eq!(find_confusable(49), Some("l"));
    assert_eq!(find_confusable(128876), Some("VB"));
    assert_eq!(find_confusable(128877), None);
    assert_eq!(find_confusable(34), Some("\""));
}

#[test]
fn named_escape_semicolon_and_fixups() {
    // escaped sequences produce raw slices; the fixup kernel unescapes them
    let mut buf = b"a\\nb\\x41c\\u{1F600}d\\z   e".to_vec();
    assert!(fixup_quoted_string(&mut buf));
    assert_eq!(buf, b"a\nb\x41c\xF0\x9F\x98\x80de".to_vec());

    let mut bad = b"\\xZZ".to_vec();
    assert!(!fixup_quoted_string(&mut bad));

    let mut multiline = b"\r\nfirst\r\nsecond\nthird".to_vec();
    fixup_multiline_string(&mut multiline);
    assert_eq!(multiline, b"first\nsecond\nthird".to_vec());
}

#[test]
fn whitespace_kinds() {
    assert!(is_space(b' '));
    assert!(is_space(b'\t'));
    assert!(is_space(b'\r'));
    assert!(is_space(b'\n'));
    assert!(is_space(0x0b));
    assert!(is_space(0x0c));
    assert!(!is_space(b'x'));
}

#[test]
fn lookahead_restores_state() {
    let mut names = AstNameTable::new();
    let mut lexer = Lexer::from_source(b"foo bar", &mut names);

    let l1 = lexer.next();
    assert_eq!(l1.ty, LexemeType::Name);

    let peek = lexer.lookahead();
    assert_eq!(peek.ty, LexemeType::Name);
    match peek.data {
        LexemeData::Name(n) => assert_eq!(n, "bar"),
        _ => panic!("expected name data"),
    }

    let l2 = lexer.next();
    assert_eq!(l2.ty, LexemeType::Name);
    match l2.data {
        LexemeData::Name(n) => assert_eq!(n, "bar"),
        _ => panic!("expected name data"),
    }
    assert!(matches!(lexer.next().ty, LexemeType::Eof));
}

#[test]
fn leading_position_offset() {
    let mut names = AstNameTable::new();
    // starting at line 1 col 4 mirrors how the Repl lexes residual input
    let mut lexer = Lexer::new(b"abc", &mut names, Position::new(1, 4));
    let l = lexer.next();
    assert_eq!(l.location.begin, Position::new(1, 4));
    assert_eq!(l.location.end, Position::new(1, 7));
    match l.data {
        LexemeData::Name(n) => assert_eq!(n, "abc"),
        _ => panic!("expected name data"),
    }
}
