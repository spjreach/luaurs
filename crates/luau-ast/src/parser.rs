//! Port of `Ast/src/Parser.cpp` — the Luau parser.
//!
//! Produces the AST defined in [`crate::ast`] from source bytes. The C++
//! parser recovers from syntax errors and materializes `AstExprError` /
//! `AstStatError` nodes; this port instead stops at the first syntax error and
//! returns it alongside the statements parsed so far (a documented, simpler
//! recovery strategy).
//!
//! Grammar coverage mirrors Luau rather than stock Lua: type annotations and
//! type aliases, `if ... then ... else` expressions, string interpolation,
//! `continue`, compound assignment, function attributes, and `local`/`const`
//! condition bindings in `if`/`while`.

use std::sync::Arc;

use crate::ast::*;
use crate::lexeme::{Lexeme, LexemeType, QuoteStyle};
use crate::lexer::{fixup_multiline_string, fixup_quoted_string, Lexer};
use crate::location::{Location, Position};
use crate::name_table::AstNameTable;

/// A syntax error with the location it was detected at.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub location: Location,
    pub message: String,
}

/// Result of parsing a chunk.
#[derive(Debug, Clone)]
pub struct ParseResult {
    /// The parsed chunk. When `errors` is non-empty this holds the statements
    /// parsed before the first error (possibly none).
    pub root: AstStatBlock,
    pub errors: Vec<ParseError>,
    /// `--!directive` comments (`ParseResult::hotcomments`).
    pub hot_comments: Vec<HotComment>,
    /// Rough upper bound on the number of expressions (from the source
    /// size), used to pre-size the compiler's per-expression maps.
    pub size_hint: usize,
    /// `ParseResult::lines`: line count of the chunk (0 after an error).
    pub lines: usize,
}

/// C++ `HotComment`: a `--!` comment; `header` is set for the ones before
/// the first token (only those configure the compiler and type checker).
#[derive(Debug, Clone)]
pub struct HotComment {
    pub header: bool,
    pub location: Location,
    /// Text after `--!`, trailing whitespace trimmed.
    pub content: String,
}

/// Collects hot comments the way `Parser::nextLexeme` does.
fn scan_hot_comments(source: &[u8], names: &mut AstNameTable) -> Vec<HotComment> {
    let mut out = Vec::new();
    if !source.windows(3).any(|w| w == b"--!") {
        return out;
    }
    let mut lexer = Lexer::from_source(source, names);
    let mut header = true;
    loop {
        let lex = lexer.next_with(false, true);
        match lex.ty {
            LexemeType::Eof => break,
            LexemeType::Comment => {
                let text = lex.get_slice(source);
                if text.first() == Some(&b'!') {
                    let mut end = text.len();
                    while end > 0 && text[end - 1].is_ascii_whitespace() {
                        end -= 1;
                    }
                    out.push(HotComment {
                        header,
                        location: lex.location,
                        content: String::from_utf8_lossy(&text[1..end.max(1)]).into_owned(),
                    });
                }
            }
            LexemeType::BlockComment => {}
            _ => header = false,
        }
    }
    out
}

impl ParseResult {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Parse a chunk of Luau source.
pub fn parse(source: &[u8], names: &mut AstNameTable) -> ParseResult {
    let hot_comments = scan_hot_comments(source, names);
    let size_hint = (source.len() / 2).min(1 << 18);
    let mut parser = Parser::new(source, names);
    match parser.parse_chunk() {
        Ok(root) => {
            let lines = parser.lexer.current().location.end.line as usize
                + (!source.is_empty() && source[source.len() - 1] != b'\n') as usize;
            ParseResult { root, errors: Vec::new(), hot_comments, size_hint, lines }
        }
        Err(err) => ParseResult {
            root: AstStatBlock {
                location: Location::default(),
                has_semicolon: false,
                body: Vec::new(),
                has_end: false,
            },
            errors: vec![err],
            hot_comments,
            size_hint: 0,
            lines: 0,
        },
    }
}

type PResult<T> = Result<T, ParseError>;

pub struct Parser<'src, 'names> {
    source: &'src [u8],
    lexer: Lexer<'src, 'names>,
    function_depth: usize,
    loop_depth: usize,
    /// Lexical scopes, innermost last, used for `shadow` links.
    scopes: Vec<Vec<Arc<AstLocal>>>,
    /// Inside a class method body: `_name` indexes are private accesses.
    in_class_method_body: bool,
    /// Locals declared next are exported (`export local` / `export function`).
    export_pending: bool,
    /// Names exported so far (duplicates are errors).
    declared_exports: Vec<AstName>,
    /// The module has a top-level `return` (incompatible with exports).
    has_module_return: bool,
    /// Nesting depth of statement blocks inside the current function.
    block_depth: usize,
    /// The enclosing function is variadic (C++ `functionStack.back().vararg`).
    vararg: bool,
    /// `function` keyword of the function being parsed (C++ `matchFunction`).
    match_fn: Lexeme,
    /// `endMismatchSuspect`: (opening token, its position).
    end_mismatch_suspect: Option<(LexemeType, Position)>,
    /// Statement/expression nesting (C++ `recursionCounter`).
    recursion: u32,
    /// Locals declared below this function depth are outside the type
    /// function being parsed (C++ `typeFunctionDepth`).
    type_fn_depth: usize,
}

/// `FFlag::DebugLuauUserDefinedClasses`: parse `class` declarations.
pub static USER_DEFINED_CLASSES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn classes_enabled() -> bool {
    USER_DEFINED_CLASSES.load(std::sync::atomic::Ordering::Relaxed)
}

/// `FInt::LuauRecursionLimit`.
const RECURSION_LIMIT: u32 = 1000;

/// Token types after which a contextual keyword (`type`, `export`,
/// `continue`, `const`) is still an ordinary name: the statement is an
/// assignment or a call (C++ parses the primary expression first).
fn continues_expression(t: LexemeType) -> bool {
    matches!(
        t,
        LexemeType::Char(b'=' | b',' | b'(' | b'.' | b'[' | b':' | b'{')
            | LexemeType::QuotedString
            | LexemeType::RawString
            | LexemeType::AddAssign
            | LexemeType::SubAssign
            | LexemeType::MulAssign
            | LexemeType::DivAssign
            | LexemeType::FloorDivAssign
            | LexemeType::ModAssign
            | LexemeType::PowAssign
            | LexemeType::ConcatAssign
    )
}

/// A single leading underscore marks a private member (`__` names are
/// metamethods and stay public).
fn is_private_field_name(name: AstName) -> bool {
    let b = name.0.as_bytes();
    !b.is_empty() && b[0] == b'_' && b.get(1) != Some(&b'_')
}

impl<'src, 'names> Parser<'src, 'names> {
    pub fn new(source: &'src [u8], names: &'names mut AstNameTable) -> Self {
        let mut lexer = Lexer::from_source(source, names);
        // Parser::Parser in C++ does `lexer.setSkipComments(true)`.
        lexer.set_skip_comments(true);
        // The C++ Parser constructor primes the lexer with the first token.
        lexer.next();
        Parser {
            source,
            lexer,
            function_depth: 0,
            loop_depth: 0,
            scopes: vec![Vec::new()],
            in_class_method_body: false,
            export_pending: false,
            declared_exports: Vec::new(),
            has_module_return: false,
            block_depth: 0,
            vararg: true,
            match_fn: Lexeme::new(Location::default(), LexemeType::Eof),
            end_mismatch_suspect: None,
            recursion: 0,
            type_fn_depth: 0,
        }
    }

    // -----------------------------------------------------------------
    // Cursor helpers
    // -----------------------------------------------------------------

    #[inline]
    fn cur(&self) -> LexemeType {
        self.lexer.current().ty
    }

    #[inline]
    fn cur_lexeme(&self) -> Lexeme {
        *self.lexer.current()
    }

    #[inline]
    fn advance(&mut self) -> Lexeme {
        self.lexer.next()
    }

    fn accept_char(&mut self, ch: u8) -> bool {
        if self.cur() == LexemeType::Char(ch) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Content of the current token when it is an identifier.
    fn cur_name(&self) -> Option<&'static str> {
        match self.cur_lexeme().data {
            crate::lexeme::LexemeData::Name(n) => Some(n),
            _ => None,
        }
    }

    /// Accepts a contextual keyword (`type`, `export`, `continue`, ...),
    /// which the lexer reports as an ordinary `Name` token.
    fn accept_contextual(&mut self, kw: &str) -> bool {
        if self.cur_name() == Some(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn accept_kw(&mut self, kw: &'static str) -> bool {
        if self.cur() == LexemeType::Reserved(kw) {
            if kw == "function" {
                self.match_fn = self.cur_lexeme();
            }
            self.advance();
            true
        } else {
            false
        }
    }

    fn fail<T>(&self, location: Location, message: impl Into<String>) -> PResult<T> {
        Err(ParseError { location, message: message.into() })
    }

    fn cur_str(&self) -> String {
        self.cur_lexeme().to_string(self.source)
    }

    fn type_str(t: LexemeType) -> String {
        Lexeme::new(Location::default(), t).to_string(&[])
    }

    /// `expectAndConsumeFail`.
    fn expect_fail<T>(&self, t: LexemeType, ctx: &str) -> PResult<T> {
        let loc = self.cur_lexeme().location;
        if ctx.is_empty() {
            self.fail(loc, format!("Expected {}, got {}", Self::type_str(t), self.cur_str()))
        } else {
            self.fail(loc, format!("Expected {} when parsing {}, got {}", Self::type_str(t), ctx, self.cur_str()))
        }
    }

    /// `expectAndConsume(char, context)`.
    fn expect_char(&mut self, ch: u8, ctx: &str) -> PResult<()> {
        if self.accept_char(ch) {
            Ok(())
        } else {
            self.expect_fail(LexemeType::Char(ch), ctx)
        }
    }

    /// `expectAndConsume(Reserved, context)`.
    fn expect_kw(&mut self, kw: &'static str, ctx: &str) -> PResult<()> {
        if self.accept_kw(kw) {
            Ok(())
        } else {
            self.expect_fail(LexemeType::Reserved(kw), ctx)
        }
    }

    /// `expectMatchAndConsumeFail`.
    fn match_fail<T>(&self, t: LexemeType, open: &Lexeme, extra: &str) -> PResult<T> {
        let cur = self.cur_lexeme();
        let what = Self::type_str(t);
        let opener = Self::type_str(open.ty);
        let p = open.location.begin;
        let msg = if cur.location.begin.line == p.line {
            format!("Expected {} (to close {} at column {}), got {}{}", what, opener, p.column + 1, self.cur_str(), extra)
        } else {
            format!("Expected {} (to close {} at line {}), got {}{}", what, opener, p.line + 1, self.cur_str(), extra)
        };
        self.fail(cur.location, msg)
    }

    /// `expectMatchAndConsume`.
    fn expect_match(&mut self, ch: u8, open: &Lexeme) -> PResult<()> {
        if self.accept_char(ch) {
            Ok(())
        } else {
            self.match_fail(LexemeType::Char(ch), open, "")
        }
    }

    /// `expectMatchEndAndConsume`.
    fn expect_match_end(&mut self, kw: &'static str, open: &Lexeme) -> PResult<()> {
        let cur = self.cur_lexeme();
        let p = open.location.begin;
        if cur.ty != LexemeType::Reserved(kw) {
            if let Some((sty, spos)) = self.end_mismatch_suspect {
                if spos.line > p.line {
                    let extra = format!("; did you forget to close {} at line {}?", Self::type_str(sty), spos.line + 1);
                    return self.match_fail(LexemeType::Reserved(kw), open, &extra);
                }
            }
            return self.match_fail(LexemeType::Reserved(kw), open, "");
        }
        if cur.location.begin.line != p.line
            && cur.location.begin.column != p.column
            && self.end_mismatch_suspect.map_or(true, |(_, sp)| sp.line < p.line)
        {
            self.end_mismatch_suspect = Some((open.ty, p));
        }
        self.advance();
        Ok(())
    }

    /// `parseNameOpt(context)` failing.
    fn name_fail<T>(&self, ctx: &str) -> PResult<T> {
        let loc = self.cur_lexeme().location;
        if ctx.is_empty() {
            self.fail(loc, format!("Expected identifier, got {}", self.cur_str()))
        } else {
            self.fail(loc, format!("Expected identifier when parsing {}, got {}", ctx, self.cur_str()))
        }
    }

    fn expect_name(&mut self, ctx: &str) -> PResult<(AstName, Location)> {
        let lex = self.cur_lexeme();
        if let LexemeType::Name = lex.ty {
            if let crate::lexeme::LexemeData::Name(name) = lex.data {
                self.advance();
                return Ok((AstName(name), lex.location));
            }
        }
        self.name_fail(ctx)
    }

    /// `parseIndexName`: a reserved word on the same line as the `.`/`:`
    /// is taken as the (incomplete) name.
    fn parse_index_name(&mut self, ctx: &str, previous: Position) -> PResult<(AstName, Location)> {
        let _ = previous;
        self.expect_name(ctx)
    }

    fn enter(&mut self, ctx: &str) -> PResult<()> {
        self.recursion += 1;
        if self.recursion > RECURSION_LIMIT {
            return self.fail(
                self.cur_lexeme().location,
                format!("Exceeded allowed recursion depth; simplify your {ctx} to make the code compile"),
            );
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Scope handling
    // -----------------------------------------------------------------

    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn find_shadow(&self, name: AstName) -> Option<Arc<AstLocal>> {
        for scope in self.scopes.iter().rev() {
            for local in scope.iter().rev() {
                if local.name == name {
                    return Some(local.clone());
                }
            }
        }
        None
    }

    fn declare_local(
        &mut self,
        name: AstName,
        location: Location,
        is_const: bool,
        annotation: Option<Box<AstType>>,
    ) -> Arc<AstLocal> {
        let shadow = self.find_shadow(name);
        let local = Arc::new(AstLocal {
            name,
            location,
            shadow,
            function_depth: self.function_depth,
            loop_depth: self.loop_depth,
            is_const,
            is_exported: self.export_pending,
            annotation,
        });
        if let Some(scope) = self.scopes.last_mut() {
            scope.push(local.clone());
        }
        local
    }

    // -----------------------------------------------------------------
    // Chunk / block
    // -----------------------------------------------------------------

    fn parse_chunk(&mut self) -> PResult<AstStatBlock> {
        let block = self.parse_block()?;
        if self.cur() != LexemeType::Eof {
            return self.expect_fail(LexemeType::Eof, "");
        }
        Ok(block)
    }

    fn at_block_end(&self) -> bool {
        matches!(
            self.cur(),
            LexemeType::Eof
                | LexemeType::Reserved("end")
                | LexemeType::Reserved("else")
                | LexemeType::Reserved("elseif")
                | LexemeType::Reserved("until")
        )
    }

    fn parse_block(&mut self) -> PResult<AstStatBlock> {
        // `parseBlockNoScope`: a block spans from the end of the token before
        // it to the start of the token that follows it.
        let begin = self.lexer.previous_location().end;
        let mut body = Vec::new();
        let mut has_end = false;
        self.block_depth += 1;

        while !self.at_block_end() {
            if let LexemeType::Reserved("return") = self.cur() {
                let stat = self.parse_return()?;
                body.push(Box::new(stat));
                break;
            }

            let saved = self.recursion;
            self.enter("block")?;
            let stat = self.parse_statement()?;
            self.recursion = saved;
            let is_last = matches!(stat, AstStat::Break(_) | AstStat::Continue(_));
            let is_semicolon_stat = matches!(stat, AstStat::Block(_)) && false;
            let _ = is_semicolon_stat;
            body.push(Box::new(stat));
            if is_last {
                break;
            }

            // A `return` may be followed by `end`, and any statement may be
            // terminated by a semicolon.
            if let LexemeType::Reserved("return") = self.cur() {
                let stat = self.parse_return()?;
                body.push(Box::new(stat));
                break;
            }
        }

        self.block_depth -= 1;
        let end = self.cur_lexeme().location.begin;

        if matches!(self.cur(), LexemeType::Reserved("end")) {
            has_end = true;
        }

        Ok(AstStatBlock {
            location: Location::new(begin, end),
            has_semicolon: false,
            body,
            has_end,
        })
    }

    fn parse_return(&mut self) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        self.expect_kw("return", "")?;

        let mut list = Vec::new();
        if !self.at_block_end() && self.cur() != LexemeType::Char(b';') {
            list = self.parse_expr_list()?;
        }

        let mut end = self.cur_lexeme().location.begin;
        if !list.is_empty() {
            end = list.last().unwrap().location().end;
        }

        let has_semicolon = self.accept_char(b';');
        if has_semicolon {
            end = self.cur_lexeme().location.begin;
        }

        if self.function_depth == 0 {
            if !self.declared_exports.is_empty() {
                return Err(ParseError {
                    location: Location::new(begin, end),
                    message: "Exporting values is not compatible with top-level return (export/return conflict)".to_string(),
                });
            }
            self.has_module_return = true;
        }

        Ok(AstStat::Return(Box::new(AstStatReturn {
            location: Location::new(begin, end),
            has_semicolon,
            list,
        })))
    }

    fn parse_statement(&mut self) -> PResult<AstStat> {
        match self.cur() {
            LexemeType::Reserved("if") => self.parse_if_statement(),
            LexemeType::Reserved("while") => self.parse_while_statement(),
            LexemeType::Reserved("repeat") => self.parse_repeat_statement(),
            LexemeType::Reserved("for") => self.parse_for_statement(),
            LexemeType::Reserved("do") => self.parse_do_statement(),
            LexemeType::Reserved("break") => self.parse_break_statement(),
            LexemeType::Reserved("local") => self.parse_local_statement(),
            LexemeType::Reserved("function") => self.parse_function_statement(),
            LexemeType::Attribute | LexemeType::AttributeOpen => self.parse_attributed_function_statement(),

            // contextual keywords are names unless the statement turns out
            // to be neither a call nor an assignment (C++ `parseStat`)
            LexemeType::Name => match self.cur_name() {
                Some("continue") if !continues_expression(self.lexer.lookahead().ty) => self.parse_continue_statement(),
                Some("type") if !continues_expression(self.lexer.lookahead().ty) => self.parse_type_alias_statement(false),
                Some("export") if !continues_expression(self.lexer.lookahead().ty) => self.parse_export_statement(),
                Some("const") if !continues_expression(self.lexer.lookahead().ty) => self.parse_const_statement(),
                Some("class") if classes_enabled() && !continues_expression(self.lexer.lookahead().ty) => {
                    self.parse_class_statement(false, false)
                }
                Some("open") if classes_enabled() && !continues_expression(self.lexer.lookahead().ty) => {
                    if !matches!(self.lexer.lookahead().data, crate::lexeme::LexemeData::Name("class")) {
                        return self.fail(self.cur_lexeme().location, "Incomplete statement: expected a class definition after 'open'");
                    }
                    self.parse_class_statement(false, true)
                }
                _ => self.parse_expression_statement(),
            },

            _ => self.parse_expression_statement(),
        }
    }

    fn skip_semicolon(&mut self) -> bool {
        self.accept_char(b';')
    }

    fn parse_do_statement(&mut self) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        let match_do = self.cur_lexeme();
        self.advance();
        self.push_scope();
        let body = self.parse_block()?;
        self.pop_scope();
        self.expect_match_end("end", &match_do)?;
        let has_semicolon = self.skip_semicolon();
        Ok(AstStat::Block(Box::new(AstStatBlock {
            location: Location::new(begin, self.lexer.previous_location().end),
            has_semicolon,
            body: body.body,
            has_end: true,
        })))
    }

    fn parse_break_statement(&mut self) -> PResult<AstStat> {
        let start = self.cur_lexeme().location;
        let begin = start.begin;
        self.advance();
        if self.loop_depth == 0 {
            return Err(ParseError { location: start, message: "break statement must be inside a loop".to_string() });
        }
        let has_semicolon = self.skip_semicolon();
        Ok(AstStat::Break(Box::new(AstStatBreak {
            location: Location::new(begin, self.lexer.previous_location().end),
            has_semicolon,
        })))
    }

    fn parse_continue_statement(&mut self) -> PResult<AstStat> {
        let start = self.cur_lexeme().location;
        let begin = start.begin;
        self.advance();
        if self.loop_depth == 0 {
            return Err(ParseError { location: start, message: "continue statement must be inside a loop".to_string() });
        }
        let has_semicolon = self.skip_semicolon();
        Ok(AstStat::Continue(Box::new(AstStatContinue {
            location: Location::new(begin, self.lexer.previous_location().end),
            has_semicolon,
        })))
    }

    /// Parses the optional `local`/`const` binding form of an `if`/`while`
    /// condition: `if local x = expr then ...`.
    fn parse_condition(&mut self, kind: &str) -> PResult<(AstExpr, Option<Arc<AstLocal>>, bool, Option<Location>, Option<Location>)> {
        let mut condition_is_const = false;
        let mut condition_local = None;
        let mut keyword_location = None;
        let mut equals_location = None;

        if matches!(self.cur(), LexemeType::Reserved("local")) {
            keyword_location = Some(self.cur_lexeme().location);
            self.advance();

            let (name, loc) = self.expect_name("variable name")?;
            let annotation = self.parse_optional_type_annotation()?;
            if self.cur() == LexemeType::Char(b',') {
                return self.fail(
                    self.cur_lexeme().location,
                    format!("Expected '=' after variable name in '{kind} local', got ','; only a single binding is allowed"),
                );
            }
            let local = self.declare_local(name, loc, false, annotation);
            equals_location = Some(self.cur_lexeme().location);
            self.expect_char(b'=', &format!("{kind} local declaration"))?;
            let value = self.parse_expr()?;
            condition_local = Some(local);
            return Ok((value, condition_local, condition_is_const, keyword_location, equals_location));
        }

        // `const` is not a reserved word: it lexes as a name. It only starts a
        // binding when a name follows (`while const > 0` reads the variable
        // `const`).
        if self.cur_name() == Some("const")
            && matches!(self.lexer.lookahead().ty, LexemeType::Name)
        {
            keyword_location = Some(self.cur_lexeme().location);
            self.advance();
            let (name, loc) = self.expect_name("variable name")?;
            let annotation = self.parse_optional_type_annotation()?;
            if self.cur() == LexemeType::Char(b',') {
                return self.fail(
                    self.cur_lexeme().location,
                    format!("Expected '=' after variable name in '{kind} local', got ','; only a single binding is allowed"),
                );
            }
            let local = self.declare_local(name, loc, true, annotation);
            equals_location = Some(self.cur_lexeme().location);
            self.expect_char(b'=', &format!("{kind} local declaration"))?;
            let value = self.parse_expr()?;
            condition_local = Some(local);
            condition_is_const = true;
            return Ok((value, condition_local, condition_is_const, keyword_location, equals_location));
        }

        let expr = self.parse_expr()?;
        Ok((expr, condition_local, condition_is_const, keyword_location, equals_location))
    }

    fn parse_if_statement(&mut self) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        self.advance();
        self.scopes.push(Vec::new());

        let (condition, condition_local, condition_is_const, condition_keyword_location, condition_equals_location) =
            self.parse_condition("if")?;

        let then_location = Some(self.cur_lexeme().location);
        let match_then = self.cur_lexeme();
        self.expect_kw("then", "if statement")?;

        let thenbody = {
            self.push_scope();
            let b = self.parse_block()?;
            self.pop_scope();
            b
        };

        // The condition binding (`if local x = ...`) is visible in the then
        // branch only, so drop its scope before parsing `elseif`/`else`.
        self.scopes.pop();

        let mut elsebody: Option<Box<AstStat>> = None;
        let mut else_location = None;

        if matches!(self.cur(), LexemeType::Reserved("elseif")) {
            else_location = Some(self.cur_lexeme().location);
            elsebody = Some(Box::new(self.parse_if_statement_from_elseif()?));
        } else if matches!(self.cur(), LexemeType::Reserved("else")) {
            else_location = Some(self.cur_lexeme().location);
            let match_else = self.cur_lexeme();
            self.advance();
            self.push_scope();
            let block = self.parse_block()?;
            self.pop_scope();
            self.expect_match_end("end", &match_else)?;
            elsebody = Some(Box::new(AstStat::Block(Box::new(AstStatBlock {
                location: block.location,
                has_semicolon: false,
                body: block.body,
                has_end: true,
            }))));
        } else {
            self.expect_match_end("end", &match_then)?;
        }

        let has_semicolon = self.skip_semicolon();
        let end = self.lexer.previous_location().end;

        Ok(AstStat::If(Box::new(AstStatIf {
            location: Location::new(begin, end),
            has_semicolon,
            condition: Box::new(condition),
            thenbody: Box::new(thenbody),
            elsebody,
            then_location,
            else_location,
            condition_local,
            condition_is_const,
            condition_keyword_location,
            condition_equals_location,
        })))
    }

    /// Parses an `elseif` continuation: the `elseif` token is current.
    fn parse_if_statement_from_elseif(&mut self) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        self.advance();
        self.scopes.push(Vec::new());

        let (condition, condition_local, condition_is_const, condition_keyword_location, condition_equals_location) =
            self.parse_condition("if")?;

        let then_location = Some(self.cur_lexeme().location);
        let match_then = self.cur_lexeme();
        self.expect_kw("then", "if statement")?;

        let thenbody = {
            self.push_scope();
            let b = self.parse_block()?;
            self.pop_scope();
            b
        };

        // As in `parse_if_statement`: the elseif binding is scoped to its own
        // then-branch.
        self.scopes.pop();

        let mut elsebody: Option<Box<AstStat>> = None;
        let mut else_location = None;

        if matches!(self.cur(), LexemeType::Reserved("elseif")) {
            else_location = Some(self.cur_lexeme().location);
            elsebody = Some(Box::new(self.parse_if_statement_from_elseif()?));
        } else if matches!(self.cur(), LexemeType::Reserved("else")) {
            else_location = Some(self.cur_lexeme().location);
            let match_else = self.cur_lexeme();
            self.advance();
            self.push_scope();
            let block = self.parse_block()?;
            self.pop_scope();
            self.expect_match_end("end", &match_else)?;
            elsebody = Some(Box::new(AstStat::Block(Box::new(AstStatBlock {
                location: block.location,
                has_semicolon: false,
                body: block.body,
                has_end: true,
            }))));
        } else {
            self.expect_match_end("end", &match_then)?;
        }

        let has_semicolon = self.skip_semicolon();
        let end = self.lexer.previous_location().end;

        Ok(AstStat::If(Box::new(AstStatIf {
            location: Location::new(begin, end),
            has_semicolon,
            condition: Box::new(condition),
            thenbody: Box::new(thenbody),
            elsebody,
            then_location,
            else_location,
            condition_local,
            condition_is_const,
            condition_keyword_location,
            condition_equals_location,
        })))
    }

    fn parse_while_statement(&mut self) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        self.advance();
        self.scopes.push(Vec::new());

        let (condition, condition_local, condition_is_const, condition_keyword_location, condition_equals_location) =
            self.parse_condition("while")?;

        let do_location = self.cur_lexeme().location;
        let match_do = self.cur_lexeme();
        self.expect_kw("do", "while loop")?;
        let has_do = true;

        self.loop_depth += 1;
        self.push_scope();
        let body = self.parse_block()?;
        self.pop_scope();
        self.loop_depth -= 1;

        self.expect_match_end("end", &match_do)?;
        self.scopes.pop();

        let has_semicolon = self.skip_semicolon();
        let end = self.lexer.previous_location().end;

        Ok(AstStat::While(Box::new(AstStatWhile {
            location: Location::new(begin, end),
            has_semicolon,
            condition: Box::new(condition),
            body: Box::new(body),
            has_do,
            do_location,
            condition_local,
            condition_is_const,
            condition_keyword_location,
            condition_equals_location,
        })))
    }

    fn parse_repeat_statement(&mut self) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        let match_repeat = self.cur_lexeme();
        self.advance();

        self.loop_depth += 1;
        self.push_scope();
        let body = self.parse_block()?;

        self.expect_match_end("until", &match_repeat)?;
        // The `until` condition is part of the body scope, so locals declared
        // inside the loop remain visible to it.
        let condition = self.parse_expr()?;
        self.pop_scope();
        self.loop_depth -= 1;

        let has_semicolon = self.skip_semicolon();

        Ok(AstStat::Repeat(Box::new(AstStatRepeat {
            location: Location::new(begin, self.lexer.previous_location().end),
            has_semicolon,
            condition: Box::new(condition),
            body: Box::new(body),
            deprecated_has_until: true,
        })))
    }

    fn parse_for_statement(&mut self) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        self.advance();

        let (first_name, first_loc) = self.expect_name("variable name")?;
        let first_annotation = self.parse_optional_type_annotation()?;

        if self.accept_char(b'=') {
            // numeric for
            // Bounds are parsed before the loop variable is declared, so
            // `for i=i,1,-1` refers to an outer/global `i`.
            self.push_scope();
            let from = self.parse_expr()?;
            self.expect_char(b',', "index range")?;
            let to = self.parse_expr()?;
            let step = if self.accept_char(b',') { Some(self.parse_expr()?) } else { None };
            let do_location = self.cur_lexeme().location;
            let match_do = self.cur_lexeme();
            self.expect_kw("do", "for loop")?;
            let has_do = true;

            // the loop variable belongs to the loop (C++ declares it after
            // `loopDepth++`)
            self.loop_depth += 1;
            let var = self.declare_local(first_name, first_loc, false, first_annotation);
            self.push_scope();
            let body = self.parse_block()?;
            self.pop_scope();
            self.loop_depth -= 1;

            self.expect_match_end("end", &match_do)?;
            self.pop_scope();
            let has_semicolon = self.skip_semicolon();

            return Ok(AstStat::For(Box::new(AstStatFor {
                location: Location::new(begin, self.lexer.previous_location().end),
                has_semicolon,
                var,
                from: Box::new(from),
                to: Box::new(to),
                step: step.map(Box::new),
                body: Box::new(body),
                has_do,
                do_location,
            })));
        }

        // generic for: parse the remaining names first (they are not in scope
        // for the iterator expressions), then declare them.
        let mut names = vec![(first_name, first_loc, first_annotation)];
        while self.accept_char(b',') {
            let (n, l) = self.expect_name("variable name")?;
            let a = self.parse_optional_type_annotation()?;
            names.push((n, l, a));
        }

        let in_location = self.cur_lexeme().location;
        self.expect_kw("in", "for loop")?;
        let has_in = true;
        let values = self.parse_expr_list()?;

        let do_location = self.cur_lexeme().location;
        let match_do = self.cur_lexeme();
        self.expect_kw("do", "for loop")?;
        let has_do = true;

        self.push_scope();
        self.loop_depth += 1;
        let vars: Vec<Arc<AstLocal>> = names
            .into_iter()
            .map(|(name, loc, annotation)| self.declare_local(name, loc, false, annotation))
            .collect();

        self.push_scope();
        let body = self.parse_block()?;
        self.pop_scope();
        self.loop_depth -= 1;

        self.expect_match_end("end", &match_do)?;
        self.pop_scope();
        let has_semicolon = self.skip_semicolon();

        Ok(AstStat::ForIn(Box::new(AstStatForIn {
            location: Location::new(begin, self.lexer.previous_location().end),
            has_semicolon,
            vars,
            values,
            body: Box::new(body),
            has_in,
            in_location,
            has_do,
            do_location,
        })))
    }

    fn parse_local_statement(&mut self) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        self.advance();

        if self.accept_kw("function") {
            // diagnostics point at `local` when it is on the same line
            if self.match_fn.location.begin.line == begin.line {
                self.match_fn.location.begin.column = begin.column;
            }
            let (name, name_loc) = self.expect_name("variable name")?;
            let local = self.declare_local(name, name_loc, false, None);
            let func = self.parse_function_body(Some(name))?;
            let has_semicolon = self.skip_semicolon();
            return Ok(AstStat::LocalFunction(Box::new(AstStatLocalFunction {
                location: Location::new(begin, self.lexer.previous_location().end),
                has_semicolon,
                name: local,
                func: Arc::new(func),
                is_const: false,
                const_keyword_begin: Position::missing(),
            })));
        }

        // `local const x = ...` (experimental integer/const feature) falls
        // through: `const` lexes as a name, so it is handled as an ordinary
        // name here.
        let mut vars = Vec::new();
        loop {
            let (name, loc) = self.expect_name("variable name")?;
            let annotation = self.parse_optional_type_annotation()?;
            vars.push((name, loc, annotation));
            if !self.accept_char(b',') {
                break;
            }
        }

        let mut values = Vec::new();
        let mut equals_sign_location = None;
        if self.cur() == LexemeType::Char(b'=') {
            equals_sign_location = Some(self.cur_lexeme().location);
            self.advance();
            values = self.parse_expr_list()?;
        }

        let locals: Vec<Arc<AstLocal>> = vars
            .into_iter()
            .map(|(name, loc, annotation)| self.declare_local(name, loc, false, annotation))
            .collect();

        let has_semicolon = self.skip_semicolon();
        Ok(AstStat::Local(Box::new(AstStatLocal {
            location: Location::new(begin, self.lexer.previous_location().end),
            has_semicolon,
            vars: locals,
            values,
            is_const: false,
            is_exported: false,
            keyword_location: None,
            equals_sign_location,
        })))
    }

    /// `const Name: T = value` (contextual keyword, Luau's `const` locals).
    fn parse_const_statement(&mut self) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        let keyword_location = self.cur_lexeme().location;
        self.accept_contextual("const");

        if self.accept_kw("function") {
            if self.match_fn.location.begin.line == begin.line {
                self.match_fn.location.begin.column = begin.column;
            }
            let (name, name_loc) = self.expect_name("variable name")?;
            let local = self.declare_local(name, name_loc, true, None);
            let func = self.parse_function_body(Some(name))?;
            let has_semicolon = self.skip_semicolon();
            return Ok(AstStat::LocalFunction(Box::new(AstStatLocalFunction {
                location: Location::new(begin, self.lexer.previous_location().end),
                has_semicolon,
                name: local,
                func: Arc::new(func),
                is_const: true,
                const_keyword_begin: begin,
            })));
        }

        let mut vars = Vec::new();
        loop {
            let (name, loc) = self.expect_name("variable name")?;
            let annotation = self.parse_optional_type_annotation()?;
            vars.push((name, loc, annotation));
            if !self.accept_char(b',') {
                break;
            }
        }

        let mut values = Vec::new();
        let mut equals_sign_location = None;
        if self.cur() == LexemeType::Char(b'=') {
            equals_sign_location = Some(self.cur_lexeme().location);
            self.advance();
            values = self.parse_expr_list()?;
        }

        // `isEnoughValues`: a multret last value may provide the rest
        let enough = values.last().is_some_and(|v| matches!(**v, AstExpr::Call(_) | AstExpr::Varargs(_))) || values.len() == vars.len();
        if !enough {
            let end = match values.last() {
                Some(v) => v.location().end,
                None => self.lexer.previous_location().end,
            };
            return self.fail(Location::new(begin, end), "Missing initializer in const declaration");
        }

        let locals: Vec<Arc<AstLocal>> = vars
            .into_iter()
            .map(|(name, loc, annotation)| self.declare_local(name, loc, true, annotation))
            .collect();

        let has_semicolon = self.skip_semicolon();
        Ok(AstStat::Local(Box::new(AstStatLocal {
            location: Location::new(begin, self.lexer.previous_location().end),
            has_semicolon,
            vars: locals,
            values,
            is_const: true,
            is_exported: false,
            keyword_location: Some(keyword_location),
            equals_sign_location,
        })))
    }

    /// `class Name [extends Super] ... end` (experimental user-defined
    /// classes; `AstStatClass`).
    fn parse_class_statement(&mut self, exported: bool, open: bool) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        if open && self.cur_name() == Some("open") {
            self.advance();
        }
        self.accept_contextual("class");

        let (name, name_loc) = self.expect_name("type name")?;
        let local = self.declare_local(name, name_loc, true, None);

        let mut super_ = None;
        if self.accept_contextual("extends") {
            super_ = Some(Box::new(self.parse_expr()?));
        }

        let mut members = Vec::new();

        while !matches!(self.cur(), LexemeType::Reserved("end") | LexemeType::Eof) {
            if self.accept_char(b';') {
                continue;
            }

            let qualifier_location = self.cur_lexeme().location;
            let mut has_qualifier = false;
            let mut access = AstClassAccess::Public;
            if self.cur_name() == Some("public") {
                has_qualifier = true;
                self.advance();
            } else if self.cur_name() == Some("private") {
                has_qualifier = true;
                access = AstClassAccess::Private;
                self.advance();
            }

            if self.accept_kw("function") {
                let keyword_location = qualifier_location;
                let (function_name, function_location) = self.expect_name("method name")?;
                let old = std::mem::replace(&mut self.in_class_method_body, true);
                let function = self.parse_function_body(Some(function_name));
                self.in_class_method_body = old;
                let function = function?;
                members.push(AstClassMember::Method(AstClassMethod {
                    qualifier_location: if has_qualifier { Some(qualifier_location) } else { None },
                    keyword_location,
                    function_name,
                    name_location: function_location,
                    function: Arc::new(function),
                    access,
                }));
            } else {
                let (property_name, property_location) = self.expect_name("class property name")?;

                let mut ty = None;
                let mut type_colon_location = None;
                if self.cur() == LexemeType::Char(b':') {
                    type_colon_location = Some(self.cur_lexeme().location);
                    self.advance();
                    ty = Some(Box::new(self.parse_type()?));
                }

                // Field initializers are parsed and ignored for now
                // (`AstClassProperty` has no initializer slot).
                if self.accept_char(b'=') {
                    let _ = self.parse_expr()?;
                }

                members.push(AstClassMember::Property(AstClassProperty {
                    qualifier_location: if has_qualifier {
                        qualifier_location
                    } else {
                        Location::default()
                    },
                    name: property_name,
                    name_location: property_location,
                    type_colon_location,
                    ty,
                    access,
                }));
            }

            self.accept_char(b',');
        }

        self.expect_kw("end", "class")?;
        let has_semicolon = self.skip_semicolon();

        Ok(AstStat::Class(Box::new(AstStatClass {
            location: Location::new(begin, self.lexer.previous_location().end),
            has_semicolon,
            name: local,
            super_,
            members,
            exported,
            open,
        })))
    }

    /// `validateAttribute`.
    fn attribute_kind(&self, loc: Location, name: AstName, attrs: &[AstAttr]) -> PResult<AstAttrKind> {
        let kind = match name.0 {
            "checked" => AstAttrKind::Checked,
            "native" => AstAttrKind::Native,
            "deprecated" => AstAttrKind::Deprecated,
            "" => return self.fail(loc, "Attribute name is missing"),
            n => return self.fail(loc, format!("Invalid attribute '@{n}'")),
        };
        if attrs.iter().any(|a| a.kind == kind) {
            return self.fail(loc, format!("Cannot duplicate attribute '@{}'", name.0));
        }
        Ok(kind)
    }

    /// `deprecatedArgsValidator`.
    fn check_deprecated_args(&self, loc: Location, args: &[Box<AstExpr>]) -> PResult<()> {
        if args.is_empty() {
            return Ok(());
        }
        if args.len() > 1 {
            return self.fail(loc, "@deprecated can be parametrized only by 1 argument");
        }
        let AstExpr::Table(t) = &*args[0] else {
            return self.fail(args[0].location(), "Unknown argument type for @deprecated");
        };
        for item in &t.items {
            if item.kind == AstExprTableItemKind::Record {
                let key = item.key.as_deref().unwrap();
                let AstExpr::ConstantString(k) = key else { continue };
                let ks = String::from_utf8_lossy(&k.value).into_owned();
                if ks != "use" && ks != "reason" {
                    return self.fail(
                        key.location(),
                        format!("Unknown argument '{ks}' for @deprecated. Only string constants for 'use' and 'reason' are allowed"),
                    );
                } else if !matches!(*item.value, AstExpr::ConstantString(_)) {
                    return self.fail(item.value.location(), format!("Only constant string allowed as value for '{ks}'"));
                }
            } else {
                return self.fail(item.value.location(), "Only constants keys 'use' and 'reason' are allowed for @deprecated attribute");
            }
        }
        Ok(())
    }

    /// `parseAttributes`: `@name` and `@[name(args), ...]`.
    fn parse_attributes(&mut self) -> PResult<Vec<AstAttr>> {
        let mut attrs: Vec<AstAttr> = Vec::new();
        loop {
            match self.cur() {
                LexemeType::Attribute => {
                    let lex = self.cur_lexeme();
                    let name = match lex.data {
                        crate::lexeme::LexemeData::Name(n) => AstName(n),
                        _ => AstName::empty(),
                    };
                    let kind = self.attribute_kind(lex.location, name, &attrs)?;
                    self.advance();
                    attrs.push(AstAttr { location: lex.location, kind, args: Vec::new(), name });
                }
                LexemeType::AttributeOpen => {
                    let open = self.cur_lexeme();
                    self.advance();
                    if self.cur() == LexemeType::Char(b']') {
                        return self.fail(Location::spanning(open.location, self.cur_lexeme().location), "Attribute list cannot be empty");
                    }
                    loop {
                        let (name, name_loc) = self.expect_name("attribute name")?;
                        let mut args = Vec::new();
                        let mut location = name_loc;
                        if matches!(self.cur(), LexemeType::RawString | LexemeType::QuotedString | LexemeType::Char(b'{' | b'(')) {
                            let args_begin = self.cur_lexeme().location;
                            args = self.parse_call_args(name_loc, false)?;
                            let args_loc = Location::new(args_begin.end, self.lexer.previous_location().end);
                            for a in &args {
                                if !a.is_constant_literal() && !a.is_literal_table() {
                                    return self.fail(args_loc, "Only literals can be passed as arguments for attributes");
                                }
                            }
                            location = Location::new(name_loc.begin, args_loc.end);
                        }
                        let kind = self.attribute_kind(name_loc, name, &attrs)?;
                        if kind == AstAttrKind::Deprecated {
                            self.check_deprecated_args(name_loc, &args)?;
                        }
                        attrs.push(AstAttr { location, kind, args, name });
                        if !self.accept_char(b',') {
                            break;
                        }
                    }
                    self.expect_match(b']', &open)?;
                }
                _ => break,
            }
        }
        Ok(attrs)
    }

    fn parse_attributed_function_statement(&mut self) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        let attributes = self.parse_attributes()?;

        if self.cur() == LexemeType::Reserved("local") && self.lexer.lookahead().ty != LexemeType::Reserved("function") {
            self.advance();
            return self.fail(
                self.cur_lexeme().location,
                format!("Expected 'function' after local declaration with attribute, but got {} instead", self.cur_str()),
            );
        }
        if self.accept_kw("local") {
            self.expect_kw("function", "")?;
            let (name, name_loc) = self.expect_name("variable name")?;
            let local = self.declare_local(name, name_loc, false, None);
            let mut func = self.parse_function_body(Some(name))?;
            // the function starts at its first attribute
            if let Some(first) = attributes.first() {
                func.location.begin = first.location.begin;
            }
            func.attributes = attributes;
            let has_semicolon = self.skip_semicolon();
            return Ok(AstStat::LocalFunction(Box::new(AstStatLocalFunction {
                location: Location::new(begin, self.lexer.previous_location().end),
                has_semicolon,
                name: local,
                func: Arc::new(func),
                is_const: false,
                const_keyword_begin: Position::missing(),
            })));
        }

        let contextual = self.cur_name();
        if contextual == Some("export") || contextual == Some("const") {
            let keyword = contextual.unwrap();
            if self.lexer.lookahead().ty != LexemeType::Reserved("function") {
                self.advance();
                let what = if keyword == "export" { "export" } else { "local" };
                return self.fail(
                    self.cur_lexeme().location,
                    format!("Expected 'function' after {what} declaration with attribute, but got {} instead", self.cur_str()),
                );
            }
            let mut stat = if keyword == "export" { self.parse_export_statement()? } else { self.parse_const_statement()? };
            if let AstStat::LocalFunction(lf) = &mut stat {
                let func = Arc::get_mut(&mut lf.func).expect("freshly parsed function");
                if let Some(first) = attributes.first() {
                    func.location.begin = first.location.begin;
                    lf.location.begin = first.location.begin;
                }
                func.attributes = attributes;
            }
            return Ok(stat);
        }
        if self.cur() != LexemeType::Reserved("function") {
            return self.fail(
                self.cur_lexeme().location,
                format!(
                    "Expected 'function', 'local function', 'const function', 'declare function' or a function type declaration after attribute, but got {} instead",
                    self.cur_str()
                ),
            );
        }
        self.expect_kw("function", "")?;
        let (name, func, _) = self.parse_function_name_and_body(None)?;
        let mut func = func;
        if let Some(first) = attributes.first() {
            func.location.begin = first.location.begin;
        }
        func.attributes = attributes;
        let has_semicolon = self.skip_semicolon();
        Ok(AstStat::Function(Box::new(AstStatFunction {
            location: Location::new(begin, self.lexer.previous_location().end),
            has_semicolon,
            name: Box::new(name),
            func: Arc::new(func),
        })))
    }

    fn parse_function_statement(&mut self) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        self.expect_kw("function", "")?;
        let (name, func, _method_name) = self.parse_function_name_and_body(None)?;
        let has_semicolon = self.skip_semicolon();
        Ok(AstStat::Function(Box::new(AstStatFunction {
            location: Location::new(begin, self.lexer.previous_location().end),
            has_semicolon,
            name: Box::new(name),
            func: Arc::new(func),
        })))
    }

    /// Parses the `a.b.c[:method]` name of a function statement and the body.
    fn parse_function_name_and_body(
        &mut self,
        debugname: Option<AstName>,
    ) -> PResult<(AstExpr, AstExprFunction, Option<AstName>)> {
        let (first, first_loc) = self.expect_name("function name")?;
        let mut expr = if let Some(local) = self.resolve_local(first) {
            AstExpr::Local(Box::new(AstExprLocal { location: first_loc, local, upvalue: false }))
        } else {
            AstExpr::Global(Box::new(AstExprGlobal { location: first_loc, name: first }))
        };

        let mut debug = debugname.unwrap_or(first);
        // `isExprLValue`: constants cannot be redefined
        if let AstExpr::Local(l) = &expr {
            if l.local.is_const && self.cur() != LexemeType::Char(b'.') && self.cur() != LexemeType::Char(b':') {
                return self.fail(l.location, format!("Variable '{}' is constant and may not be reassigned", l.local.name.0));
            }
        }
        let mut method_name = None;

        loop {
            if self.accept_char(b'.') {
                let (name, loc) = self.expect_name("field name")?;
                expr = AstExpr::IndexName(Box::new(AstExprIndexName {
                    location: Location::new(expr.location().begin, loc.end),
                    expr: Box::new(expr),
                    index: name,
                    index_location: loc,
                    op_position: loc.begin,
                    op: '.',
                    private_access: false,
                }));
                debug = name;
            } else if self.cur() == LexemeType::Char(b':') {
                let op_position = self.cur_lexeme().location.begin;
                self.advance();
                let (name, loc) = self.expect_name("method name")?;
                expr = AstExpr::IndexName(Box::new(AstExprIndexName {
                    location: Location::new(expr.location().begin, loc.end),
                    expr: Box::new(expr),
                    index: name,
                    index_location: loc,
                    op_position,
                    op: ':',
                    private_access: false,
                }));
                debug = name;
                method_name = Some(name);
                break;
            } else {
                break;
            }
        }

        let is_method = method_name.is_some();
        let func = self.parse_function_body_for_method(Some(debug), is_method)?;
        Ok((expr, func, method_name))
    }

    fn resolve_local(&self, name: AstName) -> Option<Arc<AstLocal>> {
        self.find_shadow(name)
    }

    fn parse_function_body(&mut self, debugname: Option<AstName>) -> PResult<AstExprFunction> {
        self.parse_function_body_for_method(debugname, false)
    }

    fn parse_function_body_for_method(
        &mut self,
        debugname: Option<AstName>,
        is_method: bool,
    ) -> PResult<AstExprFunction> {
        let match_fn = self.match_fn;
        let begin = self.cur_lexeme().location.begin;
        let generic_begin = begin;

        let mut generics = Vec::new();
        let mut generic_packs = Vec::new();
        if self.cur() == LexemeType::Char(b'<') {
            self.parse_generic_declaration(&mut generics, &mut generic_packs, false)?;
        }

        let match_paren = self.cur_lexeme();
        self.expect_char(b'(', "function")?;

        self.push_scope();
        self.function_depth += 1;
        // loop depth is per function (C++ `functionStack.back().loopDepth`)
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);

        let mut self_local = None;
        if is_method {
            let name = AstName("self");
            let loc = Location::new(generic_begin, generic_begin);
            self_local = Some(self.declare_local(name, loc, false, None));
        }

        let mut args: Vec<Arc<AstLocal>> = Vec::new();
        let mut vararg = false;
        let mut vararg_location = Location::default();
        let mut vararg_annotation = None;
        let arg_location;

        if self.cur() != LexemeType::Char(b')') {
            loop {
                if self.cur() == LexemeType::Dot3 {
                    vararg_location = self.cur_lexeme().location;
                    self.advance();
                    vararg = true;
                    if self.accept_char(b':') {
                        vararg_annotation = Some(Box::new(self.parse_variadic_argument_type_pack()?));
                    }
                    break;
                }

                let (name, loc) = self.expect_name("variable name")?;
                let annotation = self.parse_optional_type_annotation()?;
                // Parameter locals become visible in later parameter
                // annotations and in the body.
                let local = self.declare_local(name, loc, false, annotation);
                args.push(local);

                if !self.accept_char(b',') {
                    break;
                }
            }
        }

        let close_paren = self.cur_lexeme().location;
        self.expect_match(b')', &match_paren)?;
        arg_location = Some(Location::new(begin, close_paren.end));

        let mut return_annotation = None;
        if self.cur() == LexemeType::SkinnyArrow {
            return self.fail(self.cur_lexeme().location, "Function return type annotations are written after ':' instead of '->'");
        }
        if self.accept_char(b':') {
            return_annotation = Some(Box::new(self.parse_return_type()?));
            if self.cur() == LexemeType::Char(b',') {
                return self.fail(
                    self.cur_lexeme().location,
                    "Expected a statement, got ','; did you forget to wrap the list of return types in parentheses?",
                );
            }
        }

        let saved_vararg = std::mem::replace(&mut self.vararg, vararg);
        let body = self.parse_block();
        self.vararg = saved_vararg;
        let body = body?;
        self.expect_match_end("end", &match_fn)?;

        self.function_depth -= 1;
        self.loop_depth = saved_loop_depth;
        self.pop_scope();

        Ok(AstExprFunction {
            location: Location::new(match_fn.location.begin, self.lexer.previous_location().end),
            attributes: Vec::new(),
            generics,
            generic_packs,
            self_local,
            args,
            return_annotation,
            vararg,
            vararg_location,
            vararg_annotation,
            body: Box::new(body),
            function_depth: self.function_depth,
            debugname: debugname.unwrap_or(AstName::empty()),
            arg_location,
        })
    }

    fn starts_call_args(&self) -> bool {
        matches!(
            self.cur(),
            LexemeType::Char(b'(') | LexemeType::Char(b'{') | LexemeType::QuotedString | LexemeType::RawString
        )
    }

    /// `parseFunctionArgs` (argument list only); `func` is the callee's
    /// location for diagnostics.
    fn parse_call_args(&mut self, func: Location, self_: bool) -> PResult<Vec<Box<AstExpr>>> {
        match self.cur() {
            LexemeType::Char(b'(') => {
                let match_paren = self.cur_lexeme();
                self.advance();
                let mut args = Vec::new();
                if self.cur() != LexemeType::Char(b')') {
                    args = self.parse_expr_list()?;
                }
                self.expect_match(b')', &match_paren)?;
                Ok(args)
            }
            LexemeType::Char(b'{') => Ok(vec![Box::new(self.parse_table_constructor()?)]),
            LexemeType::QuotedString | LexemeType::RawString => Ok(vec![Box::new(self.parse_string_literal()?)]),
            _ => {
                let cur = self.cur_lexeme().location;
                if self_ && cur.begin.line != func.end.line {
                    self.fail(func, "Expected function call arguments after '('")
                } else {
                    self.fail(
                        Location::new(func.begin, cur.begin),
                        format!("Expected '(', '{{' or <string> when parsing function call, got {}", self.cur_str()),
                    )
                }
            }
        }
    }

    /// `isExprLValue` / `reportLValueError`: assignment targets are
    /// variables or fields, and constants may not be reassigned.
    fn check_lvalue(expr: &AstExpr) -> PResult<()> {
        match expr {
            AstExpr::Local(l) if l.local.is_const => Err(ParseError {
                location: l.location,
                message: format!("Variable '{}' is constant and may not be reassigned", l.local.name.0),
            }),
            AstExpr::Local(_) | AstExpr::Global(_) | AstExpr::IndexName(_) | AstExpr::IndexExpr(_) => Ok(()),
            _ => Err(ParseError { location: expr.location(), message: "Assigned expression must be a variable or a field".to_string() }),
        }
    }

    fn parse_expression_statement(&mut self) -> PResult<AstStat> {
        if !matches!(self.cur(), LexemeType::Name | LexemeType::Char(b'(')) {
            return self.name_fail("expression");
        }
        let begin = self.cur_lexeme().location.begin;
        let first = self.parse_suffixed_expr(true)?;

        if matches!(first, AstExpr::Call(_)) {
            let has_semicolon = self.skip_semicolon();
            return Ok(AstStat::Expr(Box::new(AstStatExpr {
                location: Location::new(begin, self.lexer.previous_location().end),
                has_semicolon,
                expr: Box::new(first),
            })));
        }

        // assignment?
        if self.cur() == LexemeType::Char(b'=') || self.cur() == LexemeType::Char(b',') {
            Self::check_lvalue(&first)?;
            let mut vars = vec![Box::new(first)];
            while self.accept_char(b',') {
                let v = self.parse_suffixed_expr(true)?;
                Self::check_lvalue(&v)?;
                vars.push(Box::new(v));
            }
            self.expect_char(b'=', "assignment")?;
            let values = self.parse_expr_list()?;
            let has_semicolon = self.skip_semicolon();
            return Ok(AstStat::Assign(Box::new(AstStatAssign {
                location: Location::new(begin, self.lexer.previous_location().end),
                has_semicolon,
                vars,
                values,
            })));
        }

        // compound assignment?
        if let Some(op) = self.compound_assign_op() {
            Self::check_lvalue(&first)?;
            self.advance();
            let value = self.parse_expr()?;
            let has_semicolon = self.skip_semicolon();
            return Ok(AstStat::CompoundAssign(Box::new(AstStatCompoundAssign {
                location: Location::new(begin, self.lexer.previous_location().end),
                has_semicolon,
                op,
                var: Box::new(first),
                value: Box::new(value),
            })));
        }

        self.fail(first.location(), "Incomplete statement: expected assignment or a function call")
    }

    fn compound_assign_op(&self) -> Option<AstExprBinaryOp> {
        match self.cur() {
            LexemeType::AddAssign => Some(AstExprBinaryOp::Add),
            LexemeType::SubAssign => Some(AstExprBinaryOp::Sub),
            LexemeType::MulAssign => Some(AstExprBinaryOp::Mul),
            LexemeType::DivAssign => Some(AstExprBinaryOp::Div),
            LexemeType::FloorDivAssign => Some(AstExprBinaryOp::FloorDiv),
            LexemeType::ModAssign => Some(AstExprBinaryOp::Mod),
            LexemeType::PowAssign => Some(AstExprBinaryOp::Pow),
            LexemeType::ConcatAssign => Some(AstExprBinaryOp::Concat),
            _ => None,
        }
    }

    fn parse_export_statement(&mut self) -> PResult<AstStat> {
        let export_loc = self.cur_lexeme().location;
        let export_begin = export_loc.begin;
        self.advance();
        let value_export = matches!(self.cur(), LexemeType::Reserved("local") | LexemeType::Reserved("function"))
            || self.cur_name() == Some("const");
        if value_export {
            let start = export_loc;
            if self.function_depth != 0 || self.block_depth != 1 {
                return Err(ParseError { location: start, message: "'export' may only be applied to top-level statements".to_string() });
            }
            if self.has_module_return {
                return Err(ParseError {
                    location: start,
                    message: "Exporting values is not compatible with top-level return (export/return conflict)".to_string(),
                });
            }
            let keyword_location = self.cur_lexeme().location;
            if self.cur() == LexemeType::Reserved("function") {
                // `export function f` declares a constant exported local function
                self.match_fn = self.cur_lexeme();
                self.advance();
                let (name, name_loc) = self.expect_name("variable name")?;
                if self.declared_exports.contains(&name) {
                    return Err(ParseError { location: name_loc, message: format!("Duplicate exported identifier '{}'", name.0) });
                }
                self.declared_exports.push(name);
                self.export_pending = true;
                let local = self.declare_local(name, name_loc, true, None);
                self.export_pending = false;
                let func = self.parse_function_body(Some(name))?;
                let has_semicolon = self.skip_semicolon();
                return Ok(AstStat::LocalFunction(Box::new(AstStatLocalFunction {
                    location: Location::new(export_begin, self.lexer.previous_location().end),
                    has_semicolon,
                    name: local,
                    func: Arc::new(func),
                    is_const: true,
                    const_keyword_begin: export_begin,
                })));
            }
            let is_local = self.cur() == LexemeType::Reserved("local");
            if matches!(self.lexer.lookahead().ty, LexemeType::Reserved("function")) {
                let message = if is_local {
                    "'export' must be followed by an identifier or 'function'; try removing 'local'"
                } else {
                    "'export' must be followed by an identifier or 'function'"
                };
                return Err(ParseError { location: start, message: message.to_string() });
            }
            self.export_pending = true;
            let stat = if is_local { self.parse_local_statement() } else { self.parse_const_statement() };
            self.export_pending = false;
            let mut stat = stat?;
            if let AstStat::Local(local) = &mut stat {
                local.is_exported = true;
                local.keyword_location = Some(keyword_location);
                local.location.begin = export_begin;
                for var in &local.vars {
                    if self.declared_exports.contains(&var.name) {
                        return Err(ParseError { location: var.location, message: format!("Duplicate exported identifier '{}'", var.name.0) });
                    }
                    self.declared_exports.push(var.name);
                }
            }
            return Ok(stat);
        }
        match self.cur() {
            _ if self.cur_name() == Some("type") => self.parse_type_alias_statement(true),
            // `export class` / `export open class` (user-defined classes).
            _ if classes_enabled() && self.cur_name() == Some("class") =>
            {
                self.parse_class_statement(true, false)
            }
            _ if classes_enabled() && self.cur_name() == Some("open")
                && matches!(
                    self.lexer.lookahead().data,
                    crate::lexeme::LexemeData::Name("class")
                ) =>
            {
                self.parse_class_statement(true, true)
            }
            _ => self.fail(export_loc, "Incomplete statement: expected assignment or a function call"),
        }
    }

    fn parse_type_alias_statement(&mut self, exported: bool) -> PResult<AstStat> {
        let begin = self.cur_lexeme().location.begin;
        self.advance();

        if self.accept_kw("function") {
            let (name, name_loc) = self.expect_name("type function name")?;
            let old = std::mem::replace(&mut self.in_class_method_body, false);
            let old_depth = std::mem::replace(&mut self.type_fn_depth, self.function_depth + 1);
            let func = self.parse_function_body(Some(name));
            self.type_fn_depth = old_depth;
            self.in_class_method_body = old;
            let func = func?;
            let has_semicolon = self.skip_semicolon();
            return Ok(AstStat::TypeFunction(Box::new(AstStatTypeFunction {
                location: Location::new(begin, self.lexer.previous_location().end),
                has_semicolon,
                name,
                name_location: name_loc,
                body: Arc::new(func),
                exported,
                has_errors: false,
            })));
        }

        let (name, name_loc) = self.expect_name("type name")?;

        let mut generics = Vec::new();
        let mut generic_packs = Vec::new();
        if self.cur() == LexemeType::Char(b'<') {
            self.parse_generic_declaration(&mut generics, &mut generic_packs, true)?;
        }

        self.expect_char(b'=', "type alias")?;
        let ty = self.parse_type()?;
        let has_semicolon = self.skip_semicolon();

        Ok(AstStat::TypeAlias(Box::new(AstStatTypeAlias {
            location: Location::new(begin, self.lexer.previous_location().end),
            has_semicolon,
            name,
            name_location: name_loc,
            generics,
            generic_packs,
            ty: Box::new(ty),
            exported,
        })))
    }

    // -----------------------------------------------------------------
    // Expressions
    // -----------------------------------------------------------------

    fn parse_expr_list(&mut self) -> PResult<Vec<Box<AstExpr>>> {
        let mut list = vec![Box::new(self.parse_expr()?)];
        while self.accept_char(b',') {
            if self.cur() == LexemeType::Char(b')') {
                return self.fail(self.cur_lexeme().location, "Expected expression after ',' but got ')' instead");
            }
            list.push(Box::new(self.parse_expr()?));
        }
        Ok(list)
    }

    fn parse_expr(&mut self) -> PResult<AstExpr> {
        self.parse_binary_expr(1)
    }

    fn binary_op_info(&self) -> Option<(AstExprBinaryOp, u8, bool)> {
        // (operator, precedence, right-associative)
        match self.cur() {
            LexemeType::Reserved("or") => Some((AstExprBinaryOp::Or, 1, false)),
            LexemeType::Reserved("and") => Some((AstExprBinaryOp::And, 2, false)),
            LexemeType::LessEqual => Some((AstExprBinaryOp::CompareLe, 3, false)),
            LexemeType::GreaterEqual => Some((AstExprBinaryOp::CompareGe, 3, false)),
            LexemeType::NotEqual => Some((AstExprBinaryOp::CompareNe, 3, false)),
            LexemeType::Equal => Some((AstExprBinaryOp::CompareEq, 3, false)),
            LexemeType::Char(b'<') => Some((AstExprBinaryOp::CompareLt, 3, false)),
            LexemeType::Char(b'>') => Some((AstExprBinaryOp::CompareGt, 3, false)),
            LexemeType::Dot2 => Some((AstExprBinaryOp::Concat, 4, true)),
            LexemeType::Char(b'+') => Some((AstExprBinaryOp::Add, 5, false)),
            LexemeType::Char(b'-') => Some((AstExprBinaryOp::Sub, 5, false)),
            LexemeType::Char(b'*') => Some((AstExprBinaryOp::Mul, 6, false)),
            LexemeType::Char(b'/') => Some((AstExprBinaryOp::Div, 6, false)),
            LexemeType::FloorDiv => Some((AstExprBinaryOp::FloorDiv, 6, false)),
            LexemeType::Char(b'%') => Some((AstExprBinaryOp::Mod, 6, false)),
            LexemeType::Char(b'^') => Some((AstExprBinaryOp::Pow, 8, true)),
            _ => None,
        }
    }

    /// `checkBinaryConfusables`: `&&`, `||` and `!=`.
    fn binary_confusable(&mut self, min_precedence: u8) -> PResult<()> {
        let cur = self.cur_lexeme();
        let (second, prec, msg) = match cur.ty {
            LexemeType::Char(b'&') => (b'&', 2, "Unexpected '&&'; did you mean 'and'?"),
            LexemeType::Char(b'|') => (b'|', 1, "Unexpected '||'; did you mean 'or'?"),
            LexemeType::Char(b'!') => (b'=', 3, "Unexpected '!='; did you mean '~='?"),
            _ => return Ok(()),
        };
        let next = self.lexer.lookahead();
        if next.ty == LexemeType::Char(second) && cur.location.end == next.location.begin && prec >= min_precedence {
            return self.fail(Location::spanning(cur.location, next.location), msg);
        }
        Ok(())
    }

    fn parse_binary_expr(&mut self, min_precedence: u8) -> PResult<AstExpr> {
        let saved = self.recursion;
        self.enter("expression")?;
        let mut left = self.parse_unary_expr()?;

        loop {
            let Some((op, precedence, right_assoc)) = self.binary_op_info() else {
                if matches!(self.cur(), LexemeType::Char(b'&' | b'|' | b'!')) {
                    self.binary_confusable(min_precedence)?;
                }
                break;
            };
            if precedence < min_precedence {
                break;
            }

            self.advance();
            let next_min = if right_assoc { precedence } else { precedence + 1 };
            let right = self.parse_binary_expr(next_min)?;

            left = AstExpr::Binary(Box::new(AstExprBinary {
                location: Location::new(left.location().begin, right.location().end),
                op,
                left: Box::new(left),
                right: Box::new(right),
            }));
            self.enter("expression")?;
        }

        self.recursion = saved;
        Ok(left)
    }

    fn parse_unary_expr(&mut self) -> PResult<AstExpr> {
        let begin = self.cur_lexeme().location.begin;

        let op = match self.cur() {
            LexemeType::Reserved("not") => Some(AstExprUnaryOp::Not),
            LexemeType::Char(b'-') => Some(AstExprUnaryOp::Minus),
            LexemeType::Char(b'#') => Some(AstExprUnaryOp::Len),
            LexemeType::Char(b'!') => return self.fail(self.cur_lexeme().location, "Unexpected '!'; did you mean 'not'?"),
            _ => None,
        };

        if let Some(op) = op {
            self.advance();
            // Unary binds less tightly than `^` but tighter than everything
            // else: parse at the power precedence.
            let expr = self.parse_binary_expr(8)?;
            // `-123i` stays a unary minus over the literal, like the C++
            // parser; the compiler and interpreter special-case that shape
            // (negating any other integer is a runtime error).
            return Ok(AstExpr::Unary(Box::new(AstExprUnary {
                location: Location::new(begin, expr.location().end),
                op,
                expr: Box::new(expr),
            })));
        }

        self.parse_suffixed_expr(false)
    }

    fn parse_suffixed_expr(&mut self, as_statement: bool) -> PResult<AstExpr> {
        let mut expr = self.parse_primary_expr()?;
        let _ = as_statement;

        loop {
            match self.cur() {
                LexemeType::Char(b'.') => {
                    let op_pos = self.cur_lexeme().location.begin;
                    self.advance();
                    let (name, loc) = self.parse_index_name("", op_pos)?;
                    expr = AstExpr::IndexName(Box::new(AstExprIndexName {
                        location: Location::new(expr.location().begin, loc.end),
                        expr: Box::new(expr),
                        index: name,
                        index_location: loc,
                        op_position: loc.begin,
                        op: '.',
                        private_access: self.in_class_method_body && is_private_field_name(name),
                    }));
                }

                LexemeType::Char(b'[') => {
                    let match_bracket = self.cur_lexeme();
                    self.advance();
                    let index = self.parse_expr()?;
                    let end = self.cur_lexeme().location;
                    self.expect_match(b']', &match_bracket)?;
                    expr = AstExpr::IndexExpr(Box::new(AstExprIndexExpr {
                        location: Location::new(expr.location().begin, end.end),
                        expr: Box::new(expr),
                        index: Box::new(index),
                    }));
                }

                LexemeType::Char(b':') => {
                    let op_position = self.cur_lexeme().location.begin;
                    self.advance();
                    let (name, name_loc) = self.parse_index_name("method name", op_position)?;
                    let func = AstExpr::IndexName(Box::new(AstExprIndexName {
                        location: Location::new(expr.location().begin, name_loc.end),
                        expr: Box::new(expr),
                        index: name,
                        index_location: name_loc,
                        op_position,
                        op: ':',
                        private_access: self.in_class_method_body && is_private_field_name(name),
                    }));

                    // `t:f<<A, B>>(args)`: explicit type arguments live on
                    // the call node for method calls (see `AstExprCall`).
                    let mut type_arguments = Vec::new();
                    if self.cur() == LexemeType::Char(b'<')
                        && self.lexer.lookahead().ty == LexemeType::Char(b'<')
                    {
                        type_arguments = self.parse_type_instantiation()?;
                    }

                    let arg_loc = self.cur_lexeme().location;
                    if arg_loc.begin.line != func.location().end.line && self.cur() == LexemeType::Char(b'(') {
                        return self.ambiguous_call();
                    }
                    let args = self.parse_call_args(func.location(), true)?;
                    expr = AstExpr::Call(Box::new(AstExprCall {
                        location: Location::new(func.location().begin, arg_loc.end),
                        func: Box::new(func),
                        type_arguments,
                        args,
                        self_: true,
                        table_call: false,
                        arg_location: arg_loc,
                    }));
                }

                // Explicit type instantiation: `f<<T>>`, `f<<(A, B)>>`.
                LexemeType::Char(b'<') if self.lexer.lookahead().ty == LexemeType::Char(b'<') => {
                    let begin = expr.location().begin;
                    let type_arguments = self.parse_type_instantiation()?;
                    let close = self.lexer.previous_location();

                    expr = AstExpr::Instantiate(Box::new(AstExprInstantiate {
                        location: Location::new(begin, close.end),
                        expr: Box::new(expr),
                        type_arguments,
                    }));
                }

                LexemeType::DoubleColon => {
                    self.advance();
                    let annotation = self.parse_type()?;
                    expr = AstExpr::TypeAssertion(Box::new(AstExprTypeAssertion {
                        location: Location::new(expr.location().begin, annotation.location().end),
                        expr: Box::new(expr),
                        annotation: Box::new(annotation),
                    }));
                }

                // A call suffix binds to prefix expressions only (locals,
                // globals, indexing, calls, groups, functions and tables); a
                // literal such as `1 (f)()` ends the expression, so the `(`
                // starts a new statement. Parenthesized argument lists must
                // also start on the callee's line (Luau reports these as
                // ambiguous calls; this port splits the statements instead).
                _ if self.starts_call_args() && Self::is_callable_expr(&expr) => {
                    let arg_loc = self.cur_lexeme().location;
                    if self.cur() == LexemeType::Char(b'(') && arg_loc.begin.line != expr.location().end.line {
                        return self.ambiguous_call();
                    }
                    let is_table = self.cur() == LexemeType::Char(b'{');
                    let args = self.parse_call_args(expr.location(), false)?;
                    let end = self.lexer.previous_location().end;
                    expr = AstExpr::Call(Box::new(AstExprCall {
                        location: Location::new(expr.location().begin, end),
                        func: Box::new(expr),
                        type_arguments: Vec::new(),
                        args,
                        self_: false,
                        table_call: is_table,
                        arg_location: arg_loc,
                    }));
                }

                _ => break,
            }
        }

        Ok(expr)
    }

    /// Expressions that can take a call suffix (`prefixexp` in the Lua
    /// grammar, plus Luau's table-constructor and function-expression calls).
    fn is_callable_expr(expr: &AstExpr) -> bool {
        matches!(
            expr,
            AstExpr::Local(_)
                | AstExpr::Global(_)
                | AstExpr::IndexName(_)
                | AstExpr::IndexExpr(_)
                | AstExpr::Call(_)
                | AstExpr::Group(_)
                | AstExpr::Instantiate(_)
        )
    }

    fn ambiguous_call<T>(&self) -> PResult<T> {
        self.fail(
            self.cur_lexeme().location,
            "Ambiguous syntax: this looks like an argument list for a function call, but could also be a start of new statement; use ';' to separate statements",
        )
    }

    /// `parseTypeInstantiationExpr`: `<<T, ...>>` (current token is the
    /// first `<`).
    fn parse_type_instantiation(&mut self) -> PResult<Vec<AstTypeOrPack>> {
        let outer = self.cur_lexeme();
        self.advance();
        let inner = self.cur_lexeme();
        self.advance();
        let mut type_arguments = Vec::new();
        if self.cur() != LexemeType::Char(b'>') {
            loop {
                type_arguments.push(self.parse_type_argument()?);
                if !self.accept_char(b',') {
                    break;
                }
            }
        }
        self.expect_match(b'>', &inner)?;
        self.expect_match(b'>', &outer)?;
        Ok(type_arguments)
    }

    fn parse_primary_expr(&mut self) -> PResult<AstExpr> {
        let lex = self.cur_lexeme();

        match lex.ty {
            LexemeType::Reserved("nil") => {
                self.advance();
                Ok(AstExpr::ConstantNil(Box::new(AstExprConstantNil { location: lex.location })))
            }
            LexemeType::Reserved("true") => {
                self.advance();
                Ok(AstExpr::ConstantBool(Box::new(AstExprConstantBool {
                    location: lex.location,
                    value: true,
                })))
            }
            LexemeType::Reserved("false") => {
                self.advance();
                Ok(AstExpr::ConstantBool(Box::new(AstExprConstantBool {
                    location: lex.location,
                    value: false,
                })))
            }
            LexemeType::Number => {
                self.advance();
                let text = lex.get_slice(self.source);
                if text.last() == Some(&b'i') {
                    let (value, parse_result) = parse_integer_literal(text);
                    if parse_result == ConstantNumberParseResult::Malformed {
                        return Err(ParseError { location: lex.location, message: "Malformed integer".into() });
                    }
                    if parse_result != ConstantNumberParseResult::Ok {
                        return Err(ParseError { location: lex.location, message: "Integer overflow".into() });
                    }
                    Ok(AstExpr::ConstantInteger(Box::new(AstExprConstantInteger {
                        location: lex.location,
                        value,
                        parse_result,
                    })))
                } else {
                    let (value, parse_result) = parse_number_literal(text);
                    if parse_result == ConstantNumberParseResult::Malformed {
                        return Err(ParseError { location: lex.location, message: "Malformed number".into() });
                    }
                    Ok(AstExpr::ConstantNumber(Box::new(AstExprConstantNumber {
                        location: lex.location,
                        value,
                        parse_result,
                    })))
                }
            }
            LexemeType::QuotedString | LexemeType::RawString => self.parse_string_literal(),
            LexemeType::InterpStringBegin | LexemeType::InterpStringSimple => {
                self.parse_interp_string()
            }
            LexemeType::Reserved("function") => {
                self.match_fn = lex;
                self.advance();
                let func = self.parse_function_body(None)?;
                Ok(AstExpr::Function(Arc::new(func)))
            }
            LexemeType::Attribute | LexemeType::AttributeOpen => {
                let attributes = self.parse_attributes()?;
                if self.cur() != LexemeType::Reserved("function") {
                    return self.fail(
                        lex.location,
                        format!("Expected 'function' declaration after attribute, but got {} instead", self.cur_str()),
                    );
                }
                self.match_fn = self.cur_lexeme();
                self.advance();
                let mut func = self.parse_function_body(None)?;
                if let Some(first) = attributes.first() {
                    func.location.begin = first.location.begin;
                }
                func.attributes = attributes;
                Ok(AstExpr::Function(Arc::new(func)))
            }
            LexemeType::Char(b'{') => self.parse_table_constructor(),
            LexemeType::Char(b'(') => {
                let begin = lex.location.begin;
                let match_paren = lex;
                self.advance();
                let expr = self.parse_expr()?;
                let end = self.cur_lexeme().location;
                if self.cur() != LexemeType::Char(b')') {
                    let extra = if self.cur() == LexemeType::Char(b'=') { "; did you mean to use '{' when defining a table?" } else { "" };
                    return self.match_fail(LexemeType::Char(b')'), &match_paren, extra);
                }
                self.advance();
                Ok(AstExpr::Group(Box::new(AstExprGroup {
                    location: Location::new(begin, end.end),
                    expr: Box::new(expr),
                })))
            }
            LexemeType::Dot3 => {
                if !self.vararg {
                    return self.fail(lex.location, "Cannot use '...' outside of a vararg function");
                }
                self.advance();
                Ok(AstExpr::Varargs(Box::new(AstExprVarargs { location: lex.location })))
            }
            LexemeType::Reserved("if") => self.parse_if_expr(),
            LexemeType::Name => {
                self.advance();
                let name = match lex.data {
                    crate::lexeme::LexemeData::Name(n) => AstName(n),
                    _ => AstName::empty(),
                };
                if let Some(local) = self.find_shadow(name) {
                    if local.function_depth < self.type_fn_depth {
                        return self.fail(
                            self.cur_lexeme().location,
                            format!("Type function cannot reference outer local '{}'", local.name.0),
                        );
                    }
                    let upvalue = local.function_depth != self.function_depth;
                    Ok(AstExpr::Local(Box::new(AstExprLocal {
                        location: lex.location,
                        local,
                        upvalue,
                    })))
                } else {
                    Ok(AstExpr::Global(Box::new(AstExprGlobal { location: lex.location, name })))
                }
            }
            LexemeType::BrokenString => self.fail(lex.location, "Malformed string; did you forget to finish it?"),
            LexemeType::BrokenInterpDoubleBrace => {
                self.fail(lex.location, "Double braces are not permitted within interpolated strings; did you mean '\\{'?")
            }
            _ => self.name_fail("expression"),
        }
    }

    fn parse_if_expr(&mut self) -> PResult<AstExpr> {
        let begin = self.cur_lexeme().location.begin;
        self.advance();
        self.push_scope();

        let (condition, condition_local, condition_is_const, condition_keyword_location, condition_equals_location) =
            self.parse_condition("if")?;
        self.expect_kw("then", "if then else expression")?;
        let true_expr = self.parse_expr()?;

        if self.cur() == LexemeType::Reserved("elseif") {
            self.pop_scope();
            let nested = self.parse_if_expr_from_elseif()?;
            return Ok(AstExpr::IfElse(Box::new(AstExprIfElse {
                location: Location::new(begin, nested.location().end),
                condition: Box::new(condition),
                has_then: true,
                true_expr: Box::new(true_expr),
                has_else: true,
                false_expr: Box::new(nested),
                condition_local,
                condition_is_const,
                condition_keyword_location,
                condition_equals_location,
            })));
        }

        // The binding scope ends before the `else` expression is parsed.
        self.pop_scope();
        self.expect_kw("else", "if then else expression")?;
        let false_expr = self.parse_expr()?;

        Ok(AstExpr::IfElse(Box::new(AstExprIfElse {
            location: Location::new(begin, false_expr.location().end),
            condition: Box::new(condition),
            has_then: true,
            true_expr: Box::new(true_expr),
            has_else: true,
            false_expr: Box::new(false_expr),
            condition_local,
            condition_is_const,
            condition_keyword_location,
            condition_equals_location,
        })))
    }

    fn parse_if_expr_from_elseif(&mut self) -> PResult<AstExpr> {
        let begin = self.cur_lexeme().location.begin;
        self.advance();
        self.push_scope();

        let (condition, condition_local, condition_is_const, condition_keyword_location, condition_equals_location) =
            self.parse_condition("if")?;
        self.expect_kw("then", "if then else expression")?;
        let true_expr = self.parse_expr()?;

        if self.cur() == LexemeType::Reserved("elseif") {
            self.pop_scope();
            let nested = self.parse_if_expr_from_elseif()?;
            return Ok(AstExpr::IfElse(Box::new(AstExprIfElse {
                location: Location::new(begin, nested.location().end),
                condition: Box::new(condition),
                has_then: true,
                true_expr: Box::new(true_expr),
                has_else: true,
                false_expr: Box::new(nested),
                condition_local,
                condition_is_const,
                condition_keyword_location,
                condition_equals_location,
            })));
        }

        // The binding scope ends before the `else` expression is parsed.
        self.pop_scope();
        self.expect_kw("else", "if then else expression")?;
        let false_expr = self.parse_expr()?;
        Ok(AstExpr::IfElse(Box::new(AstExprIfElse {
            location: Location::new(begin, false_expr.location().end),
            condition: Box::new(condition),
            has_then: true,
            true_expr: Box::new(true_expr),
            has_else: true,
            false_expr: Box::new(false_expr),
            condition_local,
            condition_is_const,
            condition_keyword_location,
            condition_equals_location,
        })))
    }

    fn parse_string_literal(&mut self) -> PResult<AstExpr> {
        let lex = self.cur_lexeme();
        let raw = lex.get_slice(self.source).to_vec();

        let (value, quote_style) = match lex.ty {
            LexemeType::QuotedString => {
                let mut data = raw;
                if !fixup_quoted_string(&mut data) {
                    return self.fail(lex.location, "String literal contains malformed escape sequence");
                }
                let style = match lex.get_quote_style(self.source) {
                    QuoteStyle::Single => AstExprConstantStringQuoteStyle::QuotedSingle,
                    QuoteStyle::Double => AstExprConstantStringQuoteStyle::QuotedSimple,
                };
                (data, style)
            }
            LexemeType::RawString => {
                let mut data = raw;
                fixup_multiline_string(&mut data);
                (data, AstExprConstantStringQuoteStyle::QuotedRaw)
            }
            _ => unreachable!("parse_string_literal on a non-string token"),
        };

        self.advance();

        Ok(AstExpr::ConstantString(Box::new(AstExprConstantString {
            location: lex.location,
            value: value.into_boxed_slice(),
            quote_style,
        })))
    }

    /// `parseInterpString`.
    fn parse_interp_string(&mut self) -> PResult<AstExpr> {
        let start = self.cur_lexeme().location;
        let mut end;
        let mut strings = Vec::new();
        let mut expressions = Vec::new();

        loop {
            let lex = self.cur_lexeme();
            end = lex.location;
            let mut data = lex.get_slice(self.source).to_vec();
            if !fixup_quoted_string(&mut data) {
                return self.fail(Location::spanning(start, end), "Interpolated string literal contains malformed escape sequence");
            }
            self.advance();
            strings.push(data.into_boxed_slice());

            if matches!(lex.ty, LexemeType::InterpStringEnd | LexemeType::InterpStringSimple) {
                break;
            }

            match self.cur() {
                LexemeType::InterpStringMid | LexemeType::InterpStringEnd => {
                    return self.fail(end, "Malformed interpolated string, expected expression inside '{}'");
                }
                LexemeType::BrokenString => {
                    return self.fail(end, "Malformed interpolated string; did you forget to add a '`'?");
                }
                _ => expressions.push(Box::new(self.parse_expr()?)),
            }

            match self.cur() {
                LexemeType::InterpStringBegin | LexemeType::InterpStringMid | LexemeType::InterpStringEnd => {}
                LexemeType::BrokenInterpDoubleBrace => {
                    return self.fail(end, "Double braces are not permitted within interpolated strings; did you mean '\\{'?");
                }
                LexemeType::BrokenString | LexemeType::Eof => {
                    if self.cur() == LexemeType::BrokenString {
                        self.advance();
                    }
                    let prev = self.lexer.previous_location();
                    match self.lexer.peek_brace_stack_top() {
                        Some(crate::lexer::BraceType::InterpolatedString) => {
                            return self.fail(prev, "Malformed interpolated string; did you forget to add a '}'?");
                        }
                        None => return self.fail(prev, "Malformed interpolated string; did you forget to add a '`'?"),
                        Some(_) => {
                            return Ok(AstExpr::InterpString(Box::new(AstExprInterpString {
                                location: Location::spanning(start, prev),
                                strings,
                                expressions,
                            })));
                        }
                    }
                }
                _ => return self.fail(end, format!("Malformed interpolated string, got {}", self.cur_str())),
            }
        }

        Ok(AstExpr::InterpString(Box::new(AstExprInterpString {
            location: Location::spanning(start, end),
            strings,
            expressions,
        })))
    }

    fn parse_table_constructor(&mut self) -> PResult<AstExpr> {
        let begin = self.cur_lexeme().location.begin;
        let match_brace = self.cur_lexeme();
        self.advance();

        let mut items = Vec::new();

        while self.cur() != LexemeType::Char(b'}') {
            let item = if self.cur() == LexemeType::Char(b'[') {
                let match_bracket = self.cur_lexeme();
                self.advance();
                let key = self.parse_expr()?;
                self.expect_match(b']', &match_bracket)?;
                self.expect_char(b'=', "table field")?;
                let value = self.parse_expr()?;
                AstExprTableItem {
                    kind: AstExprTableItemKind::General,
                    key: Some(Box::new(key)),
                    value: Box::new(value),
                }
            } else if self.cur() == LexemeType::Name && self.peek_is_equals() {
                let lex = self.cur_lexeme();
                let name = match lex.data {
                    crate::lexeme::LexemeData::Name(n) => AstName(n),
                    _ => AstName::empty(),
                };
                self.advance();
                self.expect_char(b'=', "table field")?;
                let mut value = self.parse_expr()?;
                // `{ foo = function() end }` names the closure after its key
                // (Parser.cpp sets `func->debugname = name.name`).
                if let AstExpr::Function(func) = &mut value {
                    Arc::make_mut(func).debugname = name; // freshly parsed: unique, no copy
                }
                let key = AstExpr::ConstantString(Box::new(AstExprConstantString {
                    location: Location::new(lex.location.begin, lex.location.end),
                    value: name.0.as_bytes().to_vec().into_boxed_slice(),
                    quote_style: AstExprConstantStringQuoteStyle::Unquoted,
                }));
                AstExprTableItem {
                    kind: AstExprTableItemKind::Record,
                    key: Some(Box::new(key)),
                    value: Box::new(value),
                }
            } else {
                let value = self.parse_expr()?;
                AstExprTableItem {
                    kind: AstExprTableItemKind::List,
                    key: None,
                    value: Box::new(value),
                }
            };

            items.push(item);

            if self.accept_char(b',') || self.accept_char(b';') {
                continue;
            }
            if matches!(self.cur(), LexemeType::Char(b'[') | LexemeType::Name) {
                return self.fail(self.cur_lexeme().location, "Expected ',' after table constructor element");
            }
            if self.cur() != LexemeType::Char(b'}') {
                break;
            }
        }

        let end = self.cur_lexeme().location;
        self.expect_match(b'}', &match_brace)?;

        Ok(AstExpr::Table(Box::new(AstExprTable {
            location: Location::new(begin, end.end),
            items,
        })))
    }

    /// True if the token after the current (name) token is `=`. Used to tell
    /// `{ foo = 1 }` record items from `{ foo }` list items.
    fn peek_is_equals(&mut self) -> bool {
        // The lexer exposes a state-restoring lookahead.
        self.lexer.lookahead().ty == LexemeType::Char(b'=')
    }

    // -----------------------------------------------------------------
    // Types
    // -----------------------------------------------------------------

    /// A single type argument of an explicit instantiation: a type, a type
    /// pack in parentheses, or `...T`.
    fn parse_type_argument(&mut self) -> PResult<AstTypeOrPack> {
        if self.should_parse_type_pack() {
            let pack = self.parse_variadic_or_generic_pack()?;
            return Ok(AstTypeOrPack { ty: None, type_pack: Some(Box::new(pack)) });
        }

        if self.cur() == LexemeType::Char(b'(') {
            let begin = self.cur_lexeme().location;
            let pack = match self.parse_paren_type_or_pack()? {
                Ok(pack) => pack,
                Err(ty) => {
                    let ty = self.parse_type_suffix(Some(ty), begin)?;
                    return Ok(AstTypeOrPack { ty: Some(Box::new(ty)), type_pack: None });
                }
            };
            // `(T)` followed by `|`, `&` or `?` is a parenthesized type
            if let AstTypePack::Explicit(e) = &pack {
                if e.type_list.tail_type.is_none()
                    && e.type_list.types.len() == 1
                    && matches!(self.cur(), LexemeType::Char(b'|' | b'?' | b'&'))
                {
                    let AstTypePack::Explicit(e) = pack else { unreachable!() };
                    let inner = e.type_list.types.into_iter().next().unwrap();
                    let group = AstType::Group(Box::new(AstTypeGroup { location: inner.location(), ty: inner }));
                    return Ok(AstTypeOrPack { ty: Some(Box::new(self.parse_type_suffix(Some(group), begin)?)), type_pack: None });
                }
            }
            return Ok(AstTypeOrPack { ty: None, type_pack: Some(Box::new(pack)) });
        }

        Ok(AstTypeOrPack { ty: Some(Box::new(self.parse_type()?)), type_pack: None })
    }

    fn parse_optional_type_annotation(&mut self) -> PResult<Option<Box<AstType>>> {
        if self.accept_char(b':') {
            Ok(Some(Box::new(self.parse_type()?)))
        } else {
            Ok(None)
        }
    }

    fn parse_type(&mut self) -> PResult<AstType> {
        let saved = self.recursion;
        let begin = self.cur_lexeme().location;
        let first = if matches!(self.cur(), LexemeType::Char(b'|' | b'&')) { None } else { Some(self.parse_simple_type_any()?) };
        self.recursion = saved;
        let r = self.parse_type_suffix(first, begin);
        self.recursion = saved;
        r
    }

    /// `parseSimpleType(allowPack = false)`.
    fn parse_simple_type_any(&mut self) -> PResult<AstType> {
        self.enter("type annotation")?;
        if matches!(self.cur(), LexemeType::Char(b'(' | b'<')) {
            self.parse_function_or_group_type()
        } else {
            self.parse_simple_type()
        }
    }

    /// `parseTypeSuffix`: `|`, `&` and `?` (`T?` is a union with an
    /// `AstTypeOptional` member, as in C++).
    fn parse_type_suffix(&mut self, first: Option<AstType>, begin: Location) -> PResult<AstType> {
        let mut parts: Vec<Box<AstType>> = first.into_iter().map(Box::new).collect();
        self.enter("type annotation")?;
        let mut is_union = false;
        let mut is_intersection = false;
        let mut optional_count = 0usize;
        loop {
            match self.cur() {
                LexemeType::Char(b'|') => {
                    self.advance();
                    let saved = self.recursion;
                    parts.push(Box::new(self.parse_simple_type_any()?));
                    self.recursion = saved;
                    is_union = true;
                }
                LexemeType::Char(b'?') => {
                    let location = self.cur_lexeme().location;
                    self.advance();
                    parts.push(Box::new(AstType::Optional(Box::new(AstTypeOptional { location }))));
                    optional_count += 1;
                    is_union = true;
                }
                LexemeType::Char(b'&') => {
                    self.advance();
                    let saved = self.recursion;
                    parts.push(Box::new(self.parse_simple_type_any()?));
                    self.recursion = saved;
                    is_intersection = true;
                }
                LexemeType::Dot3 => return self.fail(self.cur_lexeme().location, "Unexpected '...' after type annotation"),
                _ => break,
            }
            if parts.len() > RECURSION_LIMIT as usize + optional_count {
                return self.fail(
                    parts.last().unwrap().location(),
                    "Exceeded allowed type length; simplify your type annotation to make the code compile",
                );
            }
        }

        if parts.len() == 1 && !is_union && !is_intersection {
            return Ok(*parts.pop().unwrap());
        }
        let location = Location::new(begin.begin, parts.last().map_or(begin.end, |t| t.location().end));
        if is_union && is_intersection {
            return self.fail(location, "Mixing union and intersection types is not allowed; consider wrapping in parentheses.");
        }
        if is_union {
            Ok(AstType::Union(Box::new(AstTypeUnion { location, types: parts })))
        } else {
            Ok(AstType::Intersection(Box::new(AstTypeIntersection { location, types: parts })))
        }
    }

    fn should_parse_type_pack(&mut self) -> bool {
        self.cur() == LexemeType::Dot3 || (self.cur() == LexemeType::Name && self.lexer.lookahead().ty == LexemeType::Dot3)
    }

    /// `parseTypePack`: `...T` or `T...`.
    fn parse_variadic_or_generic_pack(&mut self) -> PResult<AstTypePack> {
        let start = self.cur_lexeme().location;
        if self.cur() == LexemeType::Dot3 {
            self.advance();
            let ty = self.parse_type()?;
            return Ok(AstTypePack::Variadic(Box::new(AstTypePackVariadic {
                location: Location::new(start.begin, ty.location().end),
                variadic_type: Box::new(ty),
            })));
        }
        let (name, loc) = self.expect_name("generic name")?;
        let end = self.cur_lexeme().location;
        self.advance(); // `...` (guaranteed by the lookahead)
        Ok(AstTypePack::Generic(Box::new(AstTypePackGeneric { location: Location::spanning(loc, end), generic_name: name })))
    }

    /// `parseVariadicArgumentTypePack`: the annotation of `...: T`
    /// (`T...` is a generic pack, anything else a variadic type).
    fn parse_variadic_argument_type_pack(&mut self) -> PResult<AstTypePack> {
        if self.cur() == LexemeType::Name && self.lexer.lookahead().ty == LexemeType::Dot3 {
            return self.parse_variadic_or_generic_pack();
        }
        let ty = self.parse_type()?;
        Ok(AstTypePack::Variadic(Box::new(AstTypePackVariadic { location: ty.location(), variadic_type: Box::new(ty) })))
    }

    /// `parseTypeList`: types with optional `name:` labels and a trailing
    /// type pack.
    #[allow(clippy::type_complexity)]
    fn parse_type_list(&mut self) -> PResult<(Vec<Box<AstType>>, Vec<Option<AstArgumentName>>, Option<Box<AstTypePack>>)> {
        let mut types = Vec::new();
        let mut names: Vec<Option<AstArgumentName>> = Vec::new();
        loop {
            if self.should_parse_type_pack() {
                let pack = self.parse_variadic_or_generic_pack()?;
                return Ok((types, names, Some(Box::new(pack))));
            }
            if self.cur() == LexemeType::Name && self.lexer.lookahead().ty == LexemeType::Char(b':') {
                while names.len() < types.len() {
                    names.push(None);
                }
                let lex = self.cur_lexeme();
                if let crate::lexeme::LexemeData::Name(n) = lex.data {
                    names.push(Some(AstArgumentName { name: AstName(n), location: lex.location }));
                }
                self.advance();
                self.expect_char(b':', "")?;
            } else if !names.is_empty() {
                names.push(None);
            }
            types.push(Box::new(self.parse_type()?));
            if !self.accept_char(b',') {
                break;
            }
            if self.cur() == LexemeType::Char(b')') {
                return self.fail(self.cur_lexeme().location, "Expected type after ',' but got ')' instead");
            }
        }
        Ok((types, names, None))
    }

    /// `parseFunctionType(allowPack = false)`: a parenthesized type or a
    /// function type.
    fn parse_function_or_group_type(&mut self) -> PResult<AstType> {
        let begin = self.cur_lexeme();
        let mut force = self.cur() == LexemeType::Char(b'<');
        let mut generics = Vec::new();
        let mut generic_packs = Vec::new();
        if force {
            self.parse_generic_declaration(&mut generics, &mut generic_packs, false)?;
        }

        let param_start = self.cur_lexeme();
        self.expect_char(b'(', "function parameters")?;
        let (types, names, tail) =
            if self.cur() != LexemeType::Char(b')') { self.parse_type_list()? } else { (Vec::new(), Vec::new(), None) };
        let close = self.cur_lexeme().location;
        self.expect_match(b')', &param_start)?;

        if !names.is_empty() {
            force = true;
        }
        let introducer = matches!(self.cur(), LexemeType::SkinnyArrow | LexemeType::Char(b':'));
        if types.len() == 1 && tail.is_none() && !force && !introducer {
            let ty = types.into_iter().next().unwrap();
            return Ok(AstType::Group(Box::new(AstTypeGroup { location: Location::spanning(param_start.location, close), ty })));
        }
        self.parse_function_type_tail(&begin, generics, generic_packs, types, names, tail)
    }

    /// `parseFunctionType(allowPack = true)`: a parenthesized type pack, or
    /// (as `Err`) a function type.
    fn parse_paren_type_or_pack(&mut self) -> PResult<Result<AstTypePack, AstType>> {
        let begin = self.cur_lexeme();
        self.advance();
        let (types, names, tail) =
            if self.cur() != LexemeType::Char(b')') { self.parse_type_list()? } else { (Vec::new(), Vec::new(), None) };
        let end = self.cur_lexeme().location;
        self.expect_match(b')', &begin)?;
        let introducer = matches!(self.cur(), LexemeType::SkinnyArrow | LexemeType::Char(b':'));
        if names.is_empty() && !introducer {
            return Ok(Ok(AstTypePack::Explicit(Box::new(AstTypePackExplicit {
                location: Location::spanning(begin.location, end),
                type_list: AstTypeList { types, tail_type: tail },
            }))));
        }
        Ok(Err(self.parse_function_type_tail(&begin, Vec::new(), Vec::new(), types, names, tail)?))
    }

    /// `parseFunctionTypeTail`.
    fn parse_function_type_tail(
        &mut self,
        begin: &Lexeme,
        generics: Vec<AstGenericType>,
        generic_packs: Vec<AstGenericTypePack>,
        types: Vec<Box<AstType>>,
        arg_names: Vec<Option<AstArgumentName>>,
        tail: Option<Box<AstTypePack>>,
    ) -> PResult<AstType> {
        self.enter("type annotation")?;
        if self.cur() == LexemeType::Char(b':') {
            return self.fail(self.cur_lexeme().location, "Return types in function type annotations are written after '->' instead of ':'");
        } else if self.cur() != LexemeType::SkinnyArrow && generics.is_empty() && generic_packs.is_empty() && types.is_empty() {
            return self.fail(
                Location::spanning(begin.location, self.lexer.previous_location()),
                "Expected '->' after '()' when parsing function type; did you mean 'nil'?",
            );
        } else if self.cur() != LexemeType::SkinnyArrow {
            return self.expect_fail(LexemeType::SkinnyArrow, "function type");
        }
        self.advance();
        let return_types = self.parse_return_type()?;
        Ok(AstType::Function(Box::new(AstTypeFunction {
            location: Location::new(begin.location.begin, return_types.location().end),
            attributes: Vec::new(),
            generics,
            generic_packs,
            arg_types: AstTypeList { types, tail_type: tail },
            arg_names,
            return_types: Box::new(return_types),
        })))
    }

    /// `parseReturnType`.
    fn parse_return_type(&mut self) -> PResult<AstTypePack> {
        let saved = self.recursion;
        self.enter("type annotation")?;
        let r = self.parse_return_type_inner();
        self.recursion = saved;
        r
    }

    fn parse_return_type_inner(&mut self) -> PResult<AstTypePack> {
        let begin = self.cur_lexeme();
        if self.cur() != LexemeType::Char(b'(') {
            if self.should_parse_type_pack() {
                return self.parse_variadic_or_generic_pack();
            }
            let ty = self.parse_type()?;
            return Ok(AstTypePack::Explicit(Box::new(AstTypePackExplicit {
                location: ty.location(),
                type_list: AstTypeList { types: vec![Box::new(ty)], tail_type: None },
            })));
        }

        self.advance();
        let (mut types, names, tail) =
            if self.cur() != LexemeType::Char(b')') { self.parse_type_list()? } else { (Vec::new(), Vec::new(), None) };
        let location = Location::spanning(begin.location, self.cur_lexeme().location);
        self.expect_match(b')', &begin)?;

        if self.cur() != LexemeType::SkinnyArrow && names.is_empty() {
            if types.len() == 1 {
                let ty = types.pop().unwrap();
                let inner = if tail.is_none() && matches!(self.cur(), LexemeType::Char(b'|' | b'?' | b'&')) {
                    AstType::Group(Box::new(AstTypeGroup { location, ty }))
                } else {
                    *ty
                };
                let ret = self.parse_type_suffix(Some(inner), begin.location)?;
                return Ok(AstTypePack::Explicit(Box::new(AstTypePackExplicit {
                    location,
                    type_list: AstTypeList { types: vec![Box::new(ret)], tail_type: tail },
                })));
            }
            return Ok(AstTypePack::Explicit(Box::new(AstTypePackExplicit {
                location,
                type_list: AstTypeList { types, tail_type: tail },
            })));
        }

        let func = self.parse_function_type_tail(&begin, Vec::new(), Vec::new(), types, names, tail)?;
        Ok(AstTypePack::Explicit(Box::new(AstTypePackExplicit {
            location: Location::spanning(location, func.location()),
            type_list: AstTypeList { types: vec![Box::new(func)], tail_type: None },
        })))
    }

    /// `parseGenericTypeList`.
    fn parse_generic_declaration(
        &mut self,
        generics: &mut Vec<AstGenericType>,
        generic_packs: &mut Vec<AstGenericTypePack>,
        with_defaults: bool,
    ) -> PResult<()> {
        let open = self.cur_lexeme();
        self.advance();
        let mut seen_pack = false;
        let mut seen_default = false;

        loop {
            let (name, loc) = self.expect_name("")?;
            if self.cur() == LexemeType::Dot3 || seen_pack {
                seen_pack = true;
                if self.cur() != LexemeType::Dot3 {
                    return self.fail(self.cur_lexeme().location, "Generic types come before generic type packs");
                }
                self.advance();
                let mut default_value = None;
                if with_defaults && self.cur() == LexemeType::Char(b'=') {
                    seen_default = true;
                    self.advance();
                    if self.should_parse_type_pack() {
                        default_value = Some(Box::new(self.parse_variadic_or_generic_pack()?));
                    } else if self.cur() == LexemeType::Char(b'(') {
                        match self.parse_paren_type_or_pack()? {
                            Ok(pack) => default_value = Some(Box::new(pack)),
                            Err(ty) => return self.fail(ty.location(), "Expected type pack after '=', got type"),
                        }
                    } else {
                        let ty = self.parse_type()?;
                        return self.fail(ty.location(), "Expected type pack after '=', got type");
                    }
                } else if seen_default {
                    return self.fail(self.cur_lexeme().location, "Expected default type pack after type pack name");
                }
                generic_packs.push(AstGenericTypePack { location: loc, name, default_value });
            } else {
                let mut default_value = None;
                if with_defaults && self.cur() == LexemeType::Char(b'=') {
                    seen_default = true;
                    self.advance();
                    default_value = Some(Box::new(self.parse_type()?));
                } else if seen_default {
                    return self.fail(self.cur_lexeme().location, "Expected default type after type name");
                }
                generics.push(AstGenericType { location: loc, name, default_value });
            }

            if self.accept_char(b',') {
                if self.cur() == LexemeType::Char(b'>') {
                    return self.fail(self.cur_lexeme().location, "Expected type after ',' but got '>' instead");
                }
            } else {
                break;
            }
        }

        self.expect_match(b'>', &open)
    }

    fn parse_simple_type(&mut self) -> PResult<AstType> {
        let lex = self.cur_lexeme();
        let begin = lex.location.begin;

        match lex.ty {
            LexemeType::Reserved("nil") => {
                self.advance();
                Ok(AstType::Reference(Box::new(AstTypeReference {
                    location: lex.location,
                    has_parameter_list: false,
                    prefix: None,
                    prefix_location: None,
                    prefix_local: None,
                    name: AstName("nil"),
                    name_location: lex.location,
                    parameters: Vec::new(),
                })))
            }
            LexemeType::Reserved("true") | LexemeType::Reserved("false") => {
                let value = lex.ty == LexemeType::Reserved("true");
                self.advance();
                Ok(AstType::SingletonBool(Box::new(AstTypeSingletonBool {
                    location: lex.location,
                    value,
                })))
            }
            LexemeType::QuotedString | LexemeType::RawString => {
                let expr = self.parse_string_literal()?;
                if let AstExpr::ConstantString(s) = expr {
                    Ok(AstType::SingletonString(Box::new(AstTypeSingletonString {
                        location: s.location,
                        value: s.value.clone(),
                    })))
                } else {
                    unreachable!()
                }
            }
            LexemeType::Name => {
                let (name, name_loc) = self.expect_name("type name")?;

                if self.cur() == LexemeType::Dot3 {
                    return self.fail(self.cur_lexeme().location, "Unexpected '...' after type name; type pack is not allowed in this context");
                }
                // `typeof(expr)`
                if name.0 == "typeof" && self.cur() != LexemeType::Char(b'.') {
                    let open = self.cur_lexeme();
                    self.expect_char(b'(', "typeof type")?;
                    let expr = self.parse_expr()?;
                    let end = self.cur_lexeme().location;
                    self.expect_match(b')', &open)?;
                    return Ok(AstType::Typeof(Box::new(AstTypeTypeof {
                        location: Location::new(begin, end.end),
                        expr: Box::new(expr),
                    })));
                }

                let mut prefix = None;
                let mut prefix_location = None;
                let mut prefix_local = None;
                let mut final_name = name;
                let mut final_loc = name_loc;

                if self.cur() == LexemeType::Char(b'.') {
                    let dot = self.cur_lexeme().location.begin;
                    self.advance();
                    prefix = Some(name);
                    prefix_location = Some(name_loc);
                    if let Some(local) = self.find_shadow(name) {
                        prefix_local = Some(local);
                    }
                    let (n, l) = self.parse_index_name("field name", dot)?;
                    final_name = n;
                    final_loc = l;
                }

                let mut parameters = Vec::new();
                let mut has_parameter_list = false;
                if self.cur() == LexemeType::Char(b'<') {
                    has_parameter_list = true;
                    let open = self.cur_lexeme();
                    self.advance();
                    if self.cur() != LexemeType::Char(b'>') {
                        loop {
                            parameters.push(self.parse_type_argument()?);
                            if !self.accept_char(b',') {
                                break;
                            }
                        }
                    }
                    self.expect_match(b'>', &open)?;
                }

                Ok(AstType::Reference(Box::new(AstTypeReference {
                    location: Location::new(begin, self.lexer.previous_location().end),
                    has_parameter_list,
                    prefix,
                    prefix_location,
                    prefix_local,
                    name: final_name,
                    name_location: final_loc,
                    parameters,
                })))
            }
            LexemeType::Char(b'{') => self.parse_table_type(),
            LexemeType::Reserved("function") => self.fail(
                lex.location,
                "Using 'function' as a type annotation is not supported, consider replacing with a function type annotation e.g. '(...any) -> ...any'",
            ),
            LexemeType::BrokenString => self.fail(lex.location, "Malformed string; did you forget to finish it?"),
            LexemeType::InterpStringBegin | LexemeType::InterpStringSimple => {
                self.parse_interp_string()?;
                self.fail(lex.location, "Interpolated string literals cannot be used as types")
            }
            LexemeType::Attribute | LexemeType::AttributeOpen => self.fail(lex.location, "attributes are not allowed in declaration context"),
            _ => self.fail(
                Location::new(self.lexer.previous_location().end, lex.location.end),
                format!("Expected type, got {}", self.cur_str()),
            ),
        }
    }

    fn parse_table_type(&mut self) -> PResult<AstType> {
        let begin = self.cur_lexeme().location.begin;
        let saved = self.recursion;
        self.enter("type annotation")?;
        let open = self.cur_lexeme();
        self.advance();

        let mut props = Vec::new();
        let mut indexer: Option<Box<AstTableIndexer>> = None;

        while self.cur() != LexemeType::Char(b'}') {
            // Access modifier (`read`/`write`), unless the name is itself a
            // field (`{ read: T }`).
            let mut access = AstTableAccess::ReadWrite;
            let mut access_location = None;
            if self.cur() == LexemeType::Name && self.lexer.lookahead().ty != LexemeType::Char(b':') {
                if let crate::lexeme::LexemeData::Name(n) = self.cur_lexeme().data {
                    if n == "read" || n == "write" {
                        access_location = Some(self.cur_lexeme().location);
                        access = if n == "read" { AstTableAccess::Read } else { AstTableAccess::Write };
                        self.advance();
                    }
                }
            }

            if self.cur() == LexemeType::Char(b'[') {
                let bracket = self.cur_lexeme();
                self.advance();

                // `["name"]: T` declares a property with a string key.
                if matches!(self.cur(), LexemeType::QuotedString | LexemeType::RawString)
                    && self.lexer.lookahead().ty == LexemeType::Char(b']')
                {
                    let lex = self.cur_lexeme();
                    let mut data = lex.get_slice(self.source).to_vec();
                    let ok = if lex.ty == LexemeType::QuotedString {
                        fixup_quoted_string(&mut data)
                    } else {
                        fixup_multiline_string(&mut data);
                        true
                    };
                    self.advance();
                    self.expect_match(b']', &bracket)?;
                    self.expect_char(b':', "table field")?;
                    let ty = self.parse_type()?;
                    if !ok || data.contains(&0) {
                        return self.fail(bracket.location, "String literal contains malformed escape sequence or \\0");
                    }
                    let name = AstName(self.lexer.intern_name(&data));
                    props.push(AstTableProp { name, location: bracket.location, ty: Box::new(ty), access, access_location });
                } else {
                    // indexer `[K]: V`
                    let index_type = self.parse_type()?;
                    self.expect_match(b']', &bracket)?;
                    self.expect_char(b':', "table field")?;
                    let result_type = self.parse_type()?;
                    let location = Location::new(bracket.location.begin, result_type.location().end);
                    if indexer.is_some() {
                        return self.fail(location, "Cannot have more than one table indexer");
                    }
                    indexer = Some(Box::new(AstTableIndexer {
                        index_type: Box::new(index_type),
                        result_type: Box::new(result_type),
                        location,
                        access,
                        access_location,
                    }));
                }
            } else if props.is_empty()
                && indexer.is_none()
                && !(self.cur() == LexemeType::Name && self.lexer.lookahead().ty == LexemeType::Char(b':'))
            {
                // Array-like table type: `{T}` desugars into `{[number]: T}`.
                let ty = self.parse_type()?;
                let location = Location::new(begin, ty.location().end);
                indexer = Some(Box::new(AstTableIndexer {
                    index_type: Box::new(AstType::Reference(Box::new(AstTypeReference {
                        location,
                        has_parameter_list: false,
                        prefix: None,
                        prefix_location: None,
                        prefix_local: None,
                        name: AstName("number"),
                        name_location: location,
                        parameters: Vec::new(),
                    }))),
                    result_type: Box::new(ty),
                    location,
                    access,
                    access_location,
                }));
                break;
            } else {
                let (name, name_loc) = self.expect_name("table field")?;
                self.expect_char(b':', "table field")?;
                let ty = self.parse_type()?;
                props.push(AstTableProp { name, location: name_loc, ty: Box::new(ty), access, access_location });
            }

            if self.accept_char(b',') || self.accept_char(b';') {
                continue;
            }
            if self.cur() != LexemeType::Char(b'}') {
                break;
            }
        }

        let end = self.cur_lexeme().location;
        self.expect_match(b'}', &open)?;
        self.recursion = saved;

        Ok(AstType::Table(Box::new(AstTypeTable { location: Location::new(begin, end.end), props, indexer, is_exact: false })))
    }
}

// ---------------------------------------------------------------------------
// Literal conversion helpers
// ---------------------------------------------------------------------------

/// `strtoull(data, &end, base)` requiring the whole string to be consumed:
/// `None` when malformed, `Some(None)` on overflow.
fn parse_u64_exact(digits: &str, radix: u32) -> Option<Option<u64>> {
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    Some(u64::from_str_radix(digits, radix).ok())
}

/// Port of `parseDouble` (`Parser::parseNumber`): hex and binary literals
/// are integers (`strtoull`), everything else goes through `strtod`.
pub fn parse_number_literal(text: &[u8]) -> (f64, ConstantNumberParseResult) {
    use ConstantNumberParseResult::*;
    let s = std::str::from_utf8(text).unwrap_or("");
    let cleaned: String = s.chars().filter(|&c| c != '_').collect();
    let b = cleaned.as_bytes();

    let integer = |digits: &str, radix: u32, overflow: ConstantNumberParseResult| match parse_u64_exact(digits, radix) {
        None => (0.0, Malformed),
        Some(None) => (u64::MAX as f64, overflow),
        Some(Some(v)) => {
            let r = v as f64;
            if v >= (1u64 << 53) && r as u64 != v {
                (r, Imprecise)
            } else {
                (r, Ok)
            }
        }
    };

    if b.len() > 2 && b[0] == b'0' && (b[1] == b'b' || b[1] == b'B') {
        let digits = &cleaned[2..];
        // some libcs accept a second 0b prefix; C++ rejects it explicitly
        if digits.starts_with("0b") || digits.starts_with("0B") {
            return (0.0, Malformed);
        }
        return integer(digits, 2, BinOverflow);
    }
    if b.len() > 2 && b[0] == b'0' && (b[1] == b'x' || b[1] == b'X') {
        return integer(&cleaned[2..], 16, HexOverflow);
    }

    // strtod over a decimal literal: digits, optional fraction and exponent
    if !b.iter().all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-')) {
        return (0.0, Malformed);
    }
    match cleaned.parse::<f64>() {
        Result::Ok(v) => {
            if v >= (1u64 << 53) as f64 && b.iter().all(|c| c.is_ascii_digit()) && format!("{v:.0}") != cleaned {
                (v, Imprecise)
            } else {
                (v, Ok)
            }
        }
        Err(_) => (0.0, Malformed),
    }
}

/// Port of `parseInteger64` for `123i` literals.
pub fn parse_integer_literal(text: &[u8]) -> (i64, ConstantNumberParseResult) {
    use ConstantNumberParseResult::*;
    let s = std::str::from_utf8(text).unwrap_or("");
    let cleaned: String = s.chars().filter(|&c| c != '_').collect();
    let Some(body) = cleaned.strip_suffix('i') else { return (0, Malformed) };
    let bb = body.as_bytes();

    if bb.len() >= 2 && bb[0] == b'0' && (bb[1] == b'x' || bb[1] == b'X') {
        return match parse_u64_exact(&body[2..], 16) {
            None => (0, Malformed),
            Some(None) => (0, HexOverflow),
            Some(Some(v)) => (v as i64, Ok),
        };
    }
    if bb.len() >= 2 && bb[0] == b'0' && (bb[1] == b'b' || bb[1] == b'B') {
        let digits = &body[2..];
        if digits.starts_with("0b") || digits.starts_with("0B") {
            return (0, Malformed);
        }
        return match parse_u64_exact(digits, 2) {
            None => (0, Malformed),
            Some(None) => (0, BinOverflow),
            Some(Some(v)) => (v as i64, Ok),
        };
    }
    if body.is_empty() || !bb.iter().all(|c| c.is_ascii_digit()) {
        return (0, Malformed);
    }
    match body.parse::<i64>() {
        Result::Ok(v) => (v, Ok),
        Err(_) => (i64::MAX, IntOverflow),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> AstStatBlock {
        // The AST's names point into the table's storage: it must outlive
        // the returned tree (leaked: tests only).
        let names: &'static mut AstNameTable = Box::leak(Box::new(AstNameTable::new()));
        let result = parse(src.as_bytes(), names);
        assert!(
            result.errors.is_empty(),
            "unexpected parse errors for {:?}: {:?}",
            src,
            result.errors
        );
        result.root
    }

    fn parse_err(src: &str) -> ParseError {
        let mut names = AstNameTable::new();
        let result = parse(src.as_bytes(), &mut names);
        assert!(!result.errors.is_empty(), "expected a parse error for {src:?}");
        result.errors.into_iter().next().unwrap()
    }

    #[test]
    fn parses_local_and_arithmetic() {
        let root = parse_ok("local x = 1 + 2 * 3\nreturn x");
        assert_eq!(root.body.len(), 2);

        let AstStat::Local(local) = root.body[0].as_ref() else {
            panic!("expected local");
        };
        assert_eq!(local.vars.len(), 1);
        assert_eq!(local.vars[0].name.0, "x");
        assert_eq!(local.values.len(), 1);
    }

    #[test]
    fn precedence_matches_lua() {
        // 1 + 2 * 3 => 1 + (2 * 3)
        let root = parse_ok("return 1 + 2 * 3");
        let AstStat::Return(ret) = root.body[0].as_ref() else { panic!() };
        let AstExpr::Binary(add) = ret.list[0].as_ref() else { panic!() };
        assert_eq!(add.op, AstExprBinaryOp::Add);
        assert!(matches!(add.right.as_ref(), AstExpr::Binary(b) if b.op == AstExprBinaryOp::Mul));

        // -2 ^ 2 => -(2 ^ 2)
        let root = parse_ok("return -2 ^ 2");
        let AstStat::Return(ret) = root.body[0].as_ref() else { panic!() };
        let AstExpr::Unary(un) = ret.list[0].as_ref() else { panic!() };
        assert_eq!(un.op, AstExprUnaryOp::Minus);
        assert!(matches!(un.expr.as_ref(), AstExpr::Binary(b) if b.op == AstExprBinaryOp::Pow));

        // a .. b .. c is right associative
        let root = parse_ok("return a .. b .. c");
        let AstStat::Return(ret) = root.body[0].as_ref() else { panic!() };
        let AstExpr::Binary(concat) = ret.list[0].as_ref() else { panic!() };
        assert_eq!(concat.op, AstExprBinaryOp::Concat);
        assert!(matches!(concat.right.as_ref(), AstExpr::Binary(b) if b.op == AstExprBinaryOp::Concat));
    }

    #[test]
    fn parses_functions_and_calls() {
        let root = parse_ok("function t:m(a, b, ...)\n  return a + b\nend");
        let AstStat::Function(func) = root.body[0].as_ref() else { panic!() };
        assert!(matches!(func.name.as_ref(), AstExpr::IndexName(i) if i.op == ':'));
        assert!(func.func.self_local.is_some());
        assert_eq!(func.func.args.len(), 2);
        assert!(func.func.vararg);
    }

    #[test]
    fn parses_loops() {
        parse_ok("for i = 1, 10, 2 do print(i) end");
        parse_ok("for k, v in pairs(t) do print(k, v) end");
        parse_ok("while true do break end");
        parse_ok("repeat continue until false");
    }

    #[test]
    fn parses_tables_and_interp() {
        parse_ok("local t = { 1, 2, x = 3, [4] = 5 }");
        let root = parse_ok("return `hello {name}!`");
        let AstStat::Return(ret) = root.body[0].as_ref() else { panic!() };
        let AstExpr::InterpString(interp) = ret.list[0].as_ref() else { panic!() };
        assert_eq!(interp.strings.len(), 2);
        assert_eq!(interp.expressions.len(), 1);
        assert_eq!(interp.strings[0].as_ref(), b"hello ");
        assert_eq!(interp.strings[1].as_ref(), b"!");
    }

    #[test]
    fn parses_type_annotations() {
        parse_ok("local x: number = 1");
        parse_ok("local f: (number, string) -> (boolean, ...number) = nil");
        parse_ok("type Point = { x: number, y: number }");
        parse_ok("type M<T, U...> = { value: T }");
        parse_ok("local t: { [string]: number } = {}");
        parse_ok("local u: number | string? = nil");
        parse_ok("local function g(x: number): number return x end");
        parse_ok("local function h<T>(x: T): T return x end");
    }

    #[test]
    fn parses_if_expr_and_compound_assign() {
        parse_ok("local x = if a then b elseif c then d else e");
        parse_ok("x += 1");
        parse_ok("t.a //= 2");
    }

    #[test]
    fn reports_syntax_errors() {
        let err = parse_err("local = 1");
        assert!(err.message.contains("identifier"));

        let err = parse_err("if x then");
        assert!(!err.message.is_empty());
    }

    #[test]
    fn number_literals() {
        assert_eq!(parse_number_literal(b"123").0, 123.0);
        assert_eq!(parse_number_literal(b"1_000").0, 1000.0);
        assert_eq!(parse_number_literal(b"0xff").0, 255.0);
        assert_eq!(parse_number_literal(b"0b101").0, 5.0);
        assert_eq!(parse_number_literal(b"1.5e2").0, 150.0);
        assert_eq!(parse_number_literal(b"0x1p4").1, ConstantNumberParseResult::Malformed);
    }
}
