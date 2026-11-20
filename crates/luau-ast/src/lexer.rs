//! Port of `Ast/src/Lexer.cpp` — the Luau lexer.
//!
//! A faithful transliteration of the original C++ implementation, using a
//! `&[u8]` source buffer. Lexemes reference source content via byte offsets
//! (see [`crate::lexeme::LexemeData`]).

use crate::lexeme::{Lexeme, LexemeData, LexemeType};
use crate::location::{Location, Position};
use crate::name_table::AstNameTable;

/// Quote-preserving brace context for interpolated strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BraceType {
    InterpolatedString,
    Normal,
}

#[inline]
pub fn is_space(ch: u8) -> bool {
    ch == b' ' || ch == b'\t' || ch == b'\r' || ch == b'\n' || ch == b'\x0b' || ch == b'\x0c'
}

#[inline]
fn is_alpha(ch: u8) -> bool {
    // use or trick to convert to lower case and unsigned comparison to do range check
    (ch | b' ').wrapping_sub(b'a') < 26
}

#[inline]
fn is_digit(ch: u8) -> bool {
    ch.wrapping_sub(b'0') < 10
}

#[inline]
fn is_hex_digit(ch: u8) -> bool {
    // use or trick to convert to lower case and unsigned comparison to do range check
    ch.wrapping_sub(b'0') < 10 || (ch | b' ').wrapping_sub(b'a') < 6
}

#[inline]
fn is_newline(ch: u8) -> bool {
    ch == b'\n'
}

pub struct Lexer<'src, 'names> {
    buffer: &'src [u8],

    offset: u32,
    line: u32,
    /// stores `0 - startPosition.column` (wrapping), mirroring the C++
    /// initializer `lineOffset(0u - startPosition.column)`.
    line_offset: u32,

    lexeme: Lexeme,
    prev_location: Location,

    names: &'names mut AstNameTable,

    skip_comments: bool,
    read_names: bool,

    brace_stack: Vec<BraceType>,
}

impl<'src, 'names> Lexer<'src, 'names> {
    pub fn new(
        buffer: &'src [u8],
        names: &'names mut AstNameTable,
        start_position: Position,
    ) -> Self {
        Lexer {
            buffer,
            offset: 0,
            line: start_position.line,
            line_offset: 0u32.wrapping_sub(start_position.column),
            lexeme: Lexeme::new(
                Location::on_line(Position::new(start_position.line, start_position.column), 0),
                LexemeType::Eof,
            ),
            prev_location: Location::default(),
            names,
            skip_comments: false,
            read_names: true,
            brace_stack: Vec::new(),
        }
    }

    /// Convenience constructor mirroring the C++ default argument
    /// (`startPosition = {0, 0}`).
    pub fn from_source(buffer: &'src [u8], names: &'names mut AstNameTable) -> Self {
        Self::new(buffer, names, Position::new(0, 0))
    }

    pub fn set_skip_comments(&mut self, skip: bool) {
        self.skip_comments = skip;
    }

    pub fn set_read_names(&mut self, read: bool) {
        self.read_names = read;
    }

    #[inline]
    pub fn previous_location(&self) -> Location {
        self.prev_location
    }

    pub fn next(&mut self) -> Lexeme {
        self.next_with(self.skip_comments, true)
    }

    /// Interns a name in the shared name table (for names the parser
    /// synthesizes, e.g. string-keyed table type properties).
    pub fn intern_name(&mut self, name: &[u8]) -> &'static str {
        self.names.get_or_add(name)
    }

    pub fn next_with(&mut self, skip_comments: bool, mut update_prev_location: bool) -> Lexeme {
        // in skipComments mode we reject valid comments
        loop {
            // consume whitespace before the token
            while is_space(self.peekch()) {
                self.consume_any();
            }

            if update_prev_location {
                self.prev_location = self.lexeme.location;
            }

            self.lexeme = self.read_next();
            update_prev_location = false;

            if !(skip_comments
                && (self.lexeme.ty == LexemeType::Comment
                    || self.lexeme.ty == LexemeType::BlockComment))
            {
                break;
            }
        }

        self.lexeme
    }

    pub fn nextline(&mut self) {
        while self.peekch() != 0 && self.peekch() != b'\r' && !is_newline(self.peekch()) {
            self.consume();
        }

        self.next();
    }

    /// Reads the next lexeme but restores all lexer state afterwards,
    /// allowing the caller to "peek" at the upcoming token.
    pub fn lookahead(&mut self) -> Lexeme {
        let current_offset = self.offset;
        let current_line = self.line;
        let current_line_offset = self.line_offset;
        let current_lexeme = self.lexeme;
        let current_prev_location = self.prev_location;
        let current_brace_stack_size = self.brace_stack.len();
        let current_brace_type =
            if self.brace_stack.is_empty() { BraceType::Normal } else { self.brace_stack[self.brace_stack.len() - 1] };

        let result = self.next();

        self.offset = current_offset;
        self.line = current_line;
        self.line_offset = current_line_offset;
        self.lexeme = current_lexeme;
        self.prev_location = current_prev_location;

        if self.brace_stack.len() < current_brace_stack_size {
            self.brace_stack.push(current_brace_type);
        } else if self.brace_stack.len() > current_brace_stack_size {
            self.brace_stack.pop();
        }

        result
    }

    #[inline]
    pub fn current(&self) -> &Lexeme {
        &self.lexeme
    }

    /// Returns true if the word is one of the Luau reserved words.
    pub fn is_reserved(word: &str) -> bool {
        crate::name_table::RESERVED_WORDS.iter().any(|&w| w == word)
    }

    #[inline]
    pub fn get_offset(&self) -> u32 {
        self.offset
    }

    /// Type of the innermost open brace, if any.
    pub fn peek_brace_stack_top(&self) -> Option<BraceType> {
        self.brace_stack.last().copied()
    }

    // ---- low-level helpers ----

    #[inline]
    fn peekch(&self) -> u8 {
        if (self.offset as usize) < self.buffer.len() {
            self.buffer[self.offset as usize]
        } else {
            0
        }
    }

    #[inline]
    fn peekch_at(&self, lookahead: u32) -> u8 {
        let pos = self.offset as usize + lookahead as usize;
        if pos < self.buffer.len() {
            self.buffer[pos]
        } else {
            0
        }
    }

    #[inline]
    fn position(&self) -> Position {
        Position::new(self.line, self.offset.wrapping_sub(self.line_offset))
    }

    /// consume() assumes the current character is not a newline; use
    /// consume_any() otherwise.
    #[inline]
    fn consume(&mut self) {
        debug_assert!(!is_newline(self.buffer[self.offset as usize]));
        self.offset += 1;
    }

    #[inline]
    fn consume_any(&mut self) {
        if (self.offset as usize) < self.buffer.len() && is_newline(self.buffer[self.offset as usize]) {
            self.line += 1;
            self.line_offset = self.offset + 1;
        }

        self.offset += 1;
    }

    // ---- token readers (ports of the C++ methods) ----

    fn read_comment_body(&mut self) -> Lexeme {
        let start = self.position();

        debug_assert!(self.peekch() == b'-' && self.peekch_at(1) == b'-');
        self.consume();
        self.consume();

        let start_offset = self.offset;

        if self.peekch() == b'[' {
            let sep = self.skip_long_separator();

            if sep >= 0 {
                return self.read_long_string(start, sep, LexemeType::BlockComment, LexemeType::BrokenComment);
            }
        }

        // fall back to single-line comment
        while self.peekch() != 0 && self.peekch() != b'\r' && !is_newline(self.peekch()) {
            self.consume();
        }

        Lexeme::new_slice(
            Location::spanning(Location::point(start), Location::point(self.position())),
            LexemeType::Comment,
            start_offset,
            self.offset - start_offset,
        )
    }

    /// Given a sequence [===[ or ]===], returns:
    /// 1. number of equal signs (or 0 if none present) between the brackets
    /// 2. -1 if this is not a long comment/string separator
    /// 3. -N if this is a malformed separator
    /// Does *not* consume the closing brace.
    fn skip_long_separator(&mut self) -> i32 {
        let start = self.peekch();

        debug_assert!(start == b'[' || start == b']');
        self.consume();

        let mut count = 0;

        while self.peekch() == b'=' {
            self.consume();
            count += 1;
        }

        if start == self.peekch() {
            count
        } else {
            (-count) - 1
        }
    }

    fn read_long_string(
        &mut self,
        start: Position,
        sep: i32,
        ok: LexemeType,
        broken: LexemeType,
    ) -> Lexeme {
        // skip (second) [
        debug_assert!(self.peekch() == b'[');
        self.consume();

        let start_offset = self.offset;

        while self.peekch() != 0 {
            if self.peekch() == b']' {
                if self.skip_long_separator() == sep {
                    debug_assert!(self.peekch() == b']');
                    self.consume(); // skip (second) ]

                    let end_offset = self.offset - sep as u32 - 2;
                    debug_assert!(end_offset >= start_offset);

                    return Lexeme::new_slice(
                        Location::spanning(Location::point(start), Location::point(self.position())),
                        ok,
                        start_offset,
                        end_offset - start_offset,
                    );
                }
            } else {
                self.consume_any();
            }
        }

        Lexeme::new(
            Location::spanning(Location::point(start), Location::point(self.position())),
            broken,
        )
    }

    fn read_backslash_in_string(&mut self) {
        debug_assert!(self.peekch() == b'\\');
        self.consume();
        match self.peekch() {
            b'\r' => {
                self.consume();
                if self.peekch() == b'\n' {
                    self.consume_any();
                }
            }

            0 => {}

            b'z' => {
                self.consume();
                while is_space(self.peekch()) {
                    self.consume_any();
                }
            }

            _ => {
                self.consume_any();
            }
        }
    }

    fn read_quoted_string(&mut self) -> Lexeme {
        let start = self.position();

        let delimiter = self.peekch();
        debug_assert!(delimiter == b'\'' || delimiter == b'"');
        self.consume();

        let start_offset = self.offset;

        while self.peekch() != delimiter {
            match self.peekch() {
                0 | b'\r' | b'\n' => {
                    return Lexeme::new(
                        Location::spanning(Location::point(start), Location::point(self.position())),
                        LexemeType::BrokenString,
                    );
                }

                b'\\' => {
                    self.read_backslash_in_string();
                }

                _ => {
                    self.consume();
                }
            }
        }

        self.consume();

        Lexeme::new_slice(
            Location::spanning(Location::point(start), Location::point(self.position())),
            LexemeType::QuotedString,
            start_offset,
            self.offset - start_offset - 1,
        )
    }

    fn read_interpolated_string_begin(&mut self) -> Lexeme {
        debug_assert!(self.peekch() == b'`');

        let start = self.position();
        self.consume();

        self.read_interpolated_string_section(start, LexemeType::InterpStringBegin, LexemeType::InterpStringSimple)
    }

    fn read_interpolated_string_section(
        &mut self,
        start: Position,
        format_type: LexemeType,
        end_type: LexemeType,
    ) -> Lexeme {
        let start_offset = self.offset;

        while self.peekch() != b'`' {
            match self.peekch() {
                0 | b'\r' | b'\n' => {
                    return Lexeme::new(
                        Location::spanning(Location::point(start), Location::point(self.position())),
                        LexemeType::BrokenString,
                    );
                }

                b'\\' => {
                    // Allow for \u{}, which would otherwise be consumed by looking for {
                    if self.peekch_at(1) == b'u' && self.peekch_at(2) == b'{' {
                        self.consume(); // backslash
                        self.consume(); // u
                        self.consume(); // {
                        continue; // C++ switch-break equivalent: continue scanning the loop
                    }

                    self.read_backslash_in_string();
                }

                b'{' => {
                    self.brace_stack.push(BraceType::InterpolatedString);

                    if self.peekch_at(1) == b'{' {
                        let broken_double_brace = Lexeme::new_slice(
                            Location::spanning(Location::point(start), Location::point(self.position())),
                            LexemeType::BrokenInterpDoubleBrace,
                            start_offset,
                            self.offset - start_offset,
                        );
                        self.consume();
                        self.consume();
                        return broken_double_brace;
                    }

                    self.consume();
                    return Lexeme::new_slice(
                        Location::spanning(Location::point(start), Location::point(self.position())),
                        format_type,
                        start_offset,
                        self.offset - start_offset - 1,
                    );
                }

                _ => {
                    self.consume();
                }
            }
        }

        self.consume();

        Lexeme::new_slice(
            Location::spanning(Location::point(start), Location::point(self.position())),
            end_type,
            start_offset,
            self.offset - start_offset - 1,
        )
    }

    fn read_number(&mut self, start: Position, start_offset: u32) -> Lexeme {
        debug_assert!(is_digit(self.peekch()));

        // This function does not do the number parsing - it only skips a number-like pattern.
        // It uses the same logic as Lua stock lexer; the resulting string is later converted
        // to a number with proper verification.
        loop {
            self.consume();

            let ch = self.peekch();
            if !(is_digit(ch) || ch == b'.' || ch == b'_') {
                break;
            }
        }

        if self.peekch() == b'e' || self.peekch() == b'E' {
            self.consume();

            if self.peekch() == b'+' || self.peekch() == b'-' {
                self.consume();
            }
        }

        while is_alpha(self.peekch()) || is_digit(self.peekch()) || self.peekch() == b'_' {
            self.consume();
        }

        Lexeme::new_slice(
            Location::spanning(Location::point(start), Location::point(self.position())),
            LexemeType::Number,
            start_offset,
            self.offset - start_offset,
        )
    }

    fn read_name(&mut self) -> (&'static str, LexemeType) {
        debug_assert!(is_alpha(self.peekch()) || self.peekch() == b'_' || self.peekch() == b'@');

        let start_offset = self.offset as usize;

        loop {
            self.consume();

            let ch = self.peekch();
            if !(is_alpha(ch) || is_digit(ch) || ch == b'_') {
                break;
            }
        }

        let slice = &self.buffer[start_offset..self.offset as usize];
        if self.read_names {
            self.names.get_or_add_with_type(slice)
        } else {
            let (name, ty) = self.names.get_with_type(slice);
            (name.unwrap_or(""), ty)
        }
    }

    fn read_utf8_error(&mut self) -> Lexeme {
        let start = self.position();

        let ch = self.peekch();

        // (leader byte pattern, payload mask) — mirrors the C++ if/else chain
        let decoded = if (ch & 0b10000000) == 0b00000000 {
            Some((1u32, (ch & 0x7f) as u32))
        } else if (ch & 0b11100000) == 0b11000000 {
            Some((2u32, (ch & 0b11111) as u32))
        } else if (ch & 0b11110000) == 0b11100000 {
            Some((3u32, (ch & 0b1111) as u32))
        } else if (ch & 0b11111000) == 0b11110000 {
            Some((4u32, (ch & 0b111) as u32))
        } else {
            self.consume();
            return Lexeme::new(
                Location::spanning(Location::point(start), Location::point(self.position())),
                LexemeType::BrokenUnicode,
            );
        };

        let (size, mut codepoint) = decoded.unwrap();

        self.consume();

        for _ in 1..size {
            if (self.peekch() & 0b11000000) != 0b10000000 {
                return Lexeme::new(
                    Location::spanning(Location::point(start), Location::point(self.position())),
                    LexemeType::BrokenUnicode,
                );
            }

            codepoint = codepoint << 6;
            codepoint |= (self.peekch() & 0b00111111) as u32;
            self.consume();
        }

        Lexeme::new(
            Location::spanning(Location::point(start), Location::point(self.position())),
            LexemeType::BrokenUnicode,
        )
        .with_codepoint(codepoint)
    }

    fn read_next(&mut self) -> Lexeme {
        let start = self.position();

        match self.peekch() {
            0 => Lexeme::new(Location::on_line(start, 0), LexemeType::Eof),

            b'-' => {
                if self.peekch_at(1) == b'>' {
                    self.consume();
                    self.consume();
                    Lexeme::new(Location::on_line(start, 2), LexemeType::SkinnyArrow)
                } else if self.peekch_at(1) == b'=' {
                    self.consume();
                    self.consume();
                    Lexeme::new(Location::on_line(start, 2), LexemeType::SubAssign)
                } else if self.peekch_at(1) == b'-' {
                    self.read_comment_body()
                } else {
                    self.consume();
                    Lexeme::new_char(Location::on_line(start, 1), b'-')
                }
            }

            b'[' => {
                let sep = self.skip_long_separator();

                if sep >= 0 {
                    self.read_long_string(start, sep, LexemeType::RawString, LexemeType::BrokenString)
                } else if sep == -1 {
                    Lexeme::new_char(Location::on_line(start, 1), b'[')
                } else {
                    Lexeme::new(
                        Location::spanning(Location::point(start), Location::point(self.position())),
                        LexemeType::BrokenString,
                    )
                }
            }

            b'{' => {
                self.consume();

                if !self.brace_stack.is_empty() {
                    self.brace_stack.push(BraceType::Normal);
                }

                Lexeme::new_char(Location::on_line(start, 1), b'{')
            }

            b'}' => {
                self.consume();

                if self.brace_stack.is_empty() {
                    return Lexeme::new_char(Location::on_line(start, 1), b'}');
                }

                let brace_stack_top = self.brace_stack.pop().unwrap();

                if brace_stack_top != BraceType::InterpolatedString {
                    return Lexeme::new_char(Location::on_line(start, 1), b'}');
                }

                self.read_interpolated_string_section(
                    start,
                    LexemeType::InterpStringMid,
                    LexemeType::InterpStringEnd,
                )
            }

            b'=' => {
                self.consume();

                if self.peekch() == b'=' {
                    self.consume();
                    Lexeme::new(Location::on_line(start, 2), LexemeType::Equal)
                } else {
                    Lexeme::new_char(Location::on_line(start, 1), b'=')
                }
            }

            b'<' => {
                self.consume();

                if self.peekch() == b'=' {
                    self.consume();
                    Lexeme::new(Location::on_line(start, 2), LexemeType::LessEqual)
                } else {
                    Lexeme::new_char(Location::on_line(start, 1), b'<')
                }
            }

            b'>' => {
                self.consume();

                if self.peekch() == b'=' {
                    self.consume();
                    Lexeme::new(Location::on_line(start, 2), LexemeType::GreaterEqual)
                } else {
                    Lexeme::new_char(Location::on_line(start, 1), b'>')
                }
            }

            b'~' => {
                self.consume();

                if self.peekch() == b'=' {
                    self.consume();
                    Lexeme::new(Location::on_line(start, 2), LexemeType::NotEqual)
                } else {
                    Lexeme::new_char(Location::on_line(start, 1), b'~')
                }
            }

            b'"' | b'\'' => self.read_quoted_string(),

            b'`' => self.read_interpolated_string_begin(),

            b'.' => {
                self.consume();

                if self.peekch() == b'.' {
                    self.consume();

                    if self.peekch() == b'.' {
                        self.consume();

                        Lexeme::new(Location::on_line(start, 3), LexemeType::Dot3)
                    } else if self.peekch() == b'=' {
                        self.consume();

                        Lexeme::new(Location::on_line(start, 3), LexemeType::ConcatAssign)
                    } else {
                        Lexeme::new(Location::on_line(start, 2), LexemeType::Dot2)
                    }
                } else {
                    if is_digit(self.peekch()) {
                        return self.read_number(start, self.offset - 1);
                    } else {
                        Lexeme::new_char(Location::on_line(start, 1), b'.')
                    }
                }
            }

            b'+' => {
                self.consume();

                if self.peekch() == b'=' {
                    self.consume();
                    Lexeme::new(Location::on_line(start, 2), LexemeType::AddAssign)
                } else {
                    Lexeme::new_char(Location::on_line(start, 1), b'+')
                }
            }

            b'/' => {
                self.consume();

                let ch = self.peekch();

                if ch == b'=' {
                    self.consume();
                    Lexeme::new(Location::on_line(start, 2), LexemeType::DivAssign)
                } else if ch == b'/' {
                    self.consume();

                    if self.peekch() == b'=' {
                        self.consume();
                        Lexeme::new(Location::on_line(start, 3), LexemeType::FloorDivAssign)
                    } else {
                        Lexeme::new(Location::on_line(start, 2), LexemeType::FloorDiv)
                    }
                } else {
                    Lexeme::new_char(Location::on_line(start, 1), b'/')
                }
            }

            b'*' => {
                self.consume();

                if self.peekch() == b'=' {
                    self.consume();
                    Lexeme::new(Location::on_line(start, 2), LexemeType::MulAssign)
                } else {
                    Lexeme::new_char(Location::on_line(start, 1), b'*')
                }
            }

            b'%' => {
                self.consume();

                if self.peekch() == b'=' {
                    self.consume();
                    Lexeme::new(Location::on_line(start, 2), LexemeType::ModAssign)
                } else {
                    Lexeme::new_char(Location::on_line(start, 1), b'%')
                }
            }

            b'^' => {
                self.consume();

                if self.peekch() == b'=' {
                    self.consume();
                    Lexeme::new(Location::on_line(start, 2), LexemeType::PowAssign)
                } else {
                    Lexeme::new_char(Location::on_line(start, 1), b'^')
                }
            }

            b':' => {
                self.consume();
                if self.peekch() == b':' {
                    self.consume();
                    Lexeme::new(Location::on_line(start, 2), LexemeType::DoubleColon)
                } else {
                    Lexeme::new_char(Location::on_line(start, 1), b':')
                }
            }

            b'(' | b')' | b']' | b';' | b',' | b'#' | b'?' | b'&' | b'|' => {
                let ch = self.peekch();
                self.consume();

                Lexeme::new_char(Location::on_line(start, 1), ch)
            }

            b'@' => {
                if self.peekch_at(1) == b'[' {
                    self.consume();
                    self.consume();

                    Lexeme::new(Location::on_line(start, 2), LexemeType::AttributeOpen)
                } else {
                    // consume @ first
                    self.consume();

                    if is_alpha(self.peekch()) || self.peekch() == b'_' {
                        let (name, _) = self.read_name();
                        Lexeme::new_name(
                            Location::spanning(Location::point(start), Location::point(self.position())),
                            LexemeType::Attribute,
                            name,
                        )
                    } else {
                        Lexeme::new_name(
                            Location::spanning(Location::point(start), Location::point(self.position())),
                            LexemeType::Attribute,
                            "",
                        )
                    }
                }
            }

            _ => {
                if is_digit(self.peekch()) {
                    self.read_number(start, self.offset)
                } else if is_alpha(self.peekch()) || self.peekch() == b'_' {
                    let (name, ty) = self.read_name();

                    Lexeme::new_name(
                        Location::spanning(Location::point(start), Location::point(self.position())),
                        ty,
                        name,
                    )
                } else if self.peekch() & 0x80 != 0 {
                    self.read_utf8_error()
                } else {
                    let ch = self.peekch();
                    self.consume();

                    Lexeme::new_char(Location::on_line(start, 1), ch)
                }
            }
        }
    }

    // ---- string fixup helpers (static in C++) ----
}

/// Port of `toUtf8` from `Ast/src/Lexer.cpp`: encodes a codepoint into a
/// byte vector, or `None` for out-of-range codepoints.
pub fn to_utf8(code: u32) -> Option<Vec<u8>> {
    // U+0000..U+007F
    if code < 0x80 {
        Some(vec![code as u8])
    }
    // U+0080..U+07FF
    else if code < 0x800 {
        Some(vec![(0xC0 | (code >> 6)) as u8, (0x80 | (code & 0x3F)) as u8])
    }
    // U+0800..U+FFFF
    else if code < 0x10000 {
        Some(vec![
            (0xE0 | (code >> 12)) as u8,
            (0x80 | ((code >> 6) & 0x3F)) as u8,
            (0x80 | (code & 0x3F)) as u8,
        ])
    }
    // U+10000..U+10FFFF
    else if code < 0x110000 {
        Some(vec![
            (0xF0 | (code >> 18)) as u8,
            (0x80 | ((code >> 12) & 0x3F)) as u8,
            (0x80 | ((code >> 6) & 0x3F)) as u8,
            (0x80 | (code & 0x3F)) as u8,
        ])
    } else {
        None
    }
}

fn unescape(ch: u8) -> u8 {
    match ch {
        b'a' => 0x07, // \a
        b'b' => 0x08, // \b
        b'f' => 0x0c, // \f
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'v' => 0x0b, // \v
        _ => ch,
    }
}

/// Port of `Lexer::fixupQuotedString`. Operates on raw bytes (the string
/// contents between the quote characters, as captured by the lexer).
/// Returns false if the escape sequence is malformed.
pub fn fixup_quoted_string(data: &mut Vec<u8>) -> bool {
    if data.is_empty() || !data.contains(&b'\\') {
        return true;
    }

    let size = data.len();
    let mut write = 0usize;

    let mut i = 0usize;
    while i < size {
        if data[i] != b'\\' {
            data[write] = data[i];
            write += 1;
            i += 1;
            continue;
        }

        if i + 1 == size {
            return false;
        }

        let escape = data[i + 1];
        i += 2; // skip \e

        match escape {
            b'\n' => {
                data[write] = b'\n';
                write += 1;
            }

            b'\r' => {
                data[write] = b'\n';
                write += 1;
                if i < size && data[i] == b'\n' {
                    i += 1;
                }
            }

            0 => return false,

            b'x' => {
                // hex escape codes are exactly 2 hex digits long
                if i + 2 > size {
                    return false;
                }

                let mut code: u32 = 0;

                for j in 0..2 {
                    let ch = data[i + j];
                    if !is_hex_digit(ch) {
                        return false;
                    }

                    // use or trick to convert to lower case
                    code = 16 * code
                        + (if is_digit(ch) {
                            (ch - b'0') as u32
                        } else {
                            (((ch | b' ') - b'a') + 10) as u32
                        });
                }

                data[write] = code as u8;
                write += 1;
                i += 2;
            }

            b'z' => {
                while i < size && is_space(data[i]) {
                    i += 1;
                }
                continue; // note: no write
            }

            b'u' => {
                // unicode escape codes are at least 3 characters including braces
                if i + 3 > size {
                    return false;
                }

                if data[i] != b'{' {
                    return false;
                }
                i += 1;

                if data[i] == b'}' {
                    return false;
                }

                let mut code: u32 = 0;

                for _ in 0..16 {
                    if i == size {
                        return false;
                    }

                    let ch = data[i];

                    if ch == b'}' {
                        break;
                    }

                    if !is_hex_digit(ch) {
                        return false;
                    }

                    // use or trick to convert to lower case
                    code = 16 * code
                        + (if is_digit(ch) {
                            (ch - b'0') as u32
                        } else {
                            (((ch | b' ') - b'a') + 10) as u32
                        });
                    i += 1;
                }

                if i == size || data[i] != b'}' {
                    return false;
                }
                i += 1;

                let utf8 = match to_utf8(code) {
                    Some(bytes) => bytes,
                    None => return false,
                };

                data[write..write + utf8.len()].copy_from_slice(&utf8);
                write += utf8.len();
            }

            _ => {
                if is_digit(escape) {
                    let mut code = (escape - b'0') as u32;

                    for _ in 0..2 {
                        if i == size || !is_digit(data[i]) {
                            break;
                        }

                        code = 10 * code + (data[i] - b'0') as u32;
                        i += 1;
                    }

                    if code > 255 {
                        return false;
                    }

                    data[write] = code as u8;
                    write += 1;
                } else {
                    data[write] = unescape(escape);
                    write += 1;
                }
            }
        }
    }

    debug_assert!(write <= size);
    data.resize(write, 0);

    true
}

/// Port of `Lexer::fixupMultilineString`. Normalizes newlines inside long
/// strings: `\r\n` and `\n` are newlines; the first newline is skipped and
/// all others are normalized to `\n`.
pub fn fixup_multiline_string(data: &mut Vec<u8>) {
    if data.is_empty() {
        return;
    }

    // skip leading newline
    let mut src = 0usize;
    if data[0] == b'\r' && data.len() > 1 && data[1] == b'\n' {
        src += 2;
    } else if data[0] == b'\n' {
        src += 1;
    }

    // parse the rest of the string, converting newlines as we go
    let mut dst = 0usize;
    while src < data.len() {
        if data[src] == b'\r' && src + 1 < data.len() && data[src + 1] == b'\n' {
            data[dst] = b'\n';
            dst += 1;
            src += 2;
        } else {
            // note, this handles \n by just writing it without changes
            data[dst] = data[src];
            dst += 1;
            src += 1;
        }
    }

    data.resize(dst, 0);
}

impl Lexeme {
    #[inline]
    fn with_codepoint(mut self, codepoint: u32) -> Self {
        self.data = LexemeData::Codepoint(codepoint);
        self
    }
}
