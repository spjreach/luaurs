//! `Regex` library: regular expressions for Luau scripts.
//!
//! This is not part of standard Luau. Like `task` and `Signal` it is opened by
//! `luaL_openextlibs`, and it is written in Rust (no Luau prelude).
//!
//! The engine compiles the pattern to a Thompson NFA and runs it with a Pike
//! VM. That makes matching linear in the size of the input -- a pattern such
//! as `(a*)*b` cannot blow up exponentially -- while still tracking capture
//! groups and leftmost-first (greedy/lazy) priority. A literal prefix and a
//! first-character class are used to skip regions that cannot start a match.
//!
//! Supported syntax: literals, `.`, `^`, `$`, `\A`, `\z`, `\Z`, `\b`, `\B`,
//! character classes (`[...]`, `[^...]`, POSIX `[[:alpha:]]`), escapes
//! (`\n`, `\t`, `\xHH`, `\u{...}`, `\Q...\E`), `\d \D \w \W \s \S`,
//! Unicode properties (`\p{L}`, `\p{Lu}`, ...), groups (capturing, `(?:...)`,
//! named `(?<name>...)` / `(?P<name>...)`), alternation, greedy/lazy
//! quantifiers (`* + ? {n} {n,} {n,m}`) and inline flags (`(?im-sx)`).
//! Backreferences and lookaround are rejected with a clear error instead of
//! silently doing the wrong thing.

use std::collections::HashMap;
use std::fmt;

use crate::lapi::*;
use crate::laux::*;
use crate::lobject::*;

// -- limits -------------------------------------------------------------------------

const MAX_PATTERN: usize = 1 << 20;
const MAX_PROGRAM: usize = 200_000;
const MAX_REPEAT: u32 = 1000;
const MAX_GROUPS: u32 = 64;
const MAX_PARSE_DEPTH: u32 = 500;
const NO_POS: u32 = u32::MAX;
const MAX_UNICODE: u32 = 0x10FFFF;

// -- character classes ---------------------------------------------------------------

const P_DIGIT: u16 = 1 << 0;
const P_WORD: u16 = 1 << 1;
const P_SPACE: u16 = 1 << 2;
const P_ALPHA: u16 = 1 << 3;
const P_ALNUM: u16 = 1 << 4;
const P_UPPER: u16 = 1 << 5;
const P_LOWER: u16 = 1 << 6;
const P_PUNCT: u16 = 1 << 7;
const P_NUMERIC: u16 = 1 << 8;
const P_CONTROL: u16 = 1 << 9;
const P_GRAPH: u16 = 1 << 10;
const P_PRINT: u16 = 1 << 11;
const P_ASCII: u16 = 1 << 12;
const P_ANY: u16 = 1 << 13;

#[inline]
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn pred_matches(preds: u16, c: char) -> bool {
    if (preds & P_DIGIT) != 0 && c.is_ascii_digit() {
        return true;
    }
    if (preds & P_WORD) != 0 && is_word_char(c) {
        return true;
    }
    if (preds & P_SPACE) != 0 && c.is_whitespace() {
        return true;
    }
    if (preds & P_ALPHA) != 0 && c.is_alphabetic() {
        return true;
    }
    if (preds & P_ALNUM) != 0 && c.is_alphanumeric() {
        return true;
    }
    if (preds & P_UPPER) != 0 && c.is_uppercase() {
        return true;
    }
    if (preds & P_LOWER) != 0 && c.is_lowercase() {
        return true;
    }
    if (preds & P_PUNCT) != 0 && !c.is_alphanumeric() && !c.is_whitespace() && !c.is_control() {
        return true;
    }
    if (preds & P_NUMERIC) != 0 && c.is_numeric() {
        return true;
    }
    if (preds & P_CONTROL) != 0 && c.is_control() {
        return true;
    }
    if (preds & P_GRAPH) != 0 && !c.is_whitespace() && !c.is_control() {
        return true;
    }
    if (preds & P_PRINT) != 0 && !c.is_control() {
        return true;
    }
    if (preds & P_ASCII) != 0 && (c as u32) < 0x80 {
        return true;
    }
    if (preds & P_ANY) != 0 {
        return true;
    }
    false
}

/// True when any *negated* predicate in `preds` matches `c`. Multiple bits are
/// ORed individually: `\D\W` means "not digit **or** not word", not
/// "neither digit nor word".
fn neg_pred_matches(preds: u16, c: char) -> bool {
    if (preds & P_DIGIT) != 0 && !c.is_ascii_digit() {
        return true;
    }
    if (preds & P_WORD) != 0 && !is_word_char(c) {
        return true;
    }
    if (preds & P_SPACE) != 0 && !c.is_whitespace() {
        return true;
    }
    if (preds & P_ALPHA) != 0 && !c.is_alphabetic() {
        return true;
    }
    if (preds & P_ALNUM) != 0 && !c.is_alphanumeric() {
        return true;
    }
    if (preds & P_UPPER) != 0 && !c.is_uppercase() {
        return true;
    }
    if (preds & P_LOWER) != 0 && !c.is_lowercase() {
        return true;
    }
    if (preds & P_PUNCT) != 0 && (c.is_alphanumeric() || c.is_whitespace() || c.is_control()) {
        return true;
    }
    if (preds & P_NUMERIC) != 0 && !c.is_numeric() {
        return true;
    }
    if (preds & P_CONTROL) != 0 && !c.is_control() {
        return true;
    }
    if (preds & P_GRAPH) != 0 && (c.is_whitespace() || c.is_control()) {
        return true;
    }
    if (preds & P_PRINT) != 0 && c.is_control() {
        return true;
    }
    if (preds & P_ASCII) != 0 && (c as u32) >= 0x80 {
        return true;
    }
    false
}

#[derive(Clone, Debug, Default)]
struct CharClass {
    /// Bits 0..=127, split into two u64 words.
    ascii: [u64; 2],
    /// Ranges above ASCII (sorted/merged by `normalize`).
    ranges: Vec<(u32, u32)>,
    /// Positive Unicode predicates (see `P_*`).
    preds: u16,
    /// Negated predicates, each ORed into the class.
    not_preds: u16,
    /// Whole-class negation (`[^...]`).
    negated: bool,
    /// Set when the class was parsed under the `i` flag.
    case_insensitive: bool,
}

impl CharClass {
    fn add_char(&mut self, c: char) {
        self.add_range(c as u32, c as u32);
    }

    fn add_range(&mut self, lo: u32, hi: u32) {
        if lo > hi {
            return;
        }
        if lo < 128 {
            let end = hi.min(127);
            for cp in lo..=end {
                self.ascii[(cp >> 6) as usize] |= 1u64 << (cp & 63);
            }
        }
        if hi >= 128 {
            self.ranges.push((lo.max(128), hi));
        }
    }

    fn absorb(&mut self, other: &CharClass) {
        self.ascii[0] |= other.ascii[0];
        self.ascii[1] |= other.ascii[1];
        self.ranges.extend_from_slice(&other.ranges);
        self.preds |= other.preds;
        self.not_preds |= other.not_preds;
        self.case_insensitive |= other.case_insensitive;
    }

    fn normalize(&mut self) {
        if self.ranges.len() > 1 {
            self.ranges.sort_unstable();
            let mut out: Vec<(u32, u32)> = Vec::with_capacity(self.ranges.len());
            for &(lo, hi) in &self.ranges {
                if let Some(last) = out.last_mut() {
                    if lo <= last.1.saturating_add(1) {
                        if hi > last.1 {
                            last.1 = hi;
                        }
                        continue;
                    }
                }
                out.push((lo, hi));
            }
            self.ranges = out;
        }
    }

    fn raw_matches(&self, c: char) -> bool {
        let cp = c as u32;
        if cp < 128 {
            if self.ascii[(cp >> 6) as usize] & (1u64 << (cp & 63)) != 0 {
                return true;
            }
        } else if !self.ranges.is_empty() {
            let mut lo = 0usize;
            let mut hi = self.ranges.len();
            while lo < hi {
                let mid = (lo + hi) / 2;
                let (a, b) = self.ranges[mid];
                if cp < a {
                    hi = mid;
                } else if cp > b {
                    lo = mid + 1;
                } else {
                    return true;
                }
            }
        }
        if self.preds != 0 && pred_matches(self.preds, c) {
            return true;
        }
        if self.not_preds != 0 && neg_pred_matches(self.not_preds, c) {
            return true;
        }
        false
    }

    fn matches(&self, c: char) -> bool {
        let mut hit = self.raw_matches(c);
        if !hit && self.case_insensitive {
            for v in c.to_lowercase().chain(c.to_uppercase()) {
                if v != c && self.raw_matches(v) {
                    hit = true;
                    break;
                }
            }
        }
        hit != self.negated
    }
}

fn pred_class(pred: u16) -> CharClass {
    CharClass {
        preds: pred,
        ..CharClass::default()
    }
}

fn neg_pred_class(pred: u16) -> CharClass {
    CharClass {
        not_preds: pred,
        ..CharClass::default()
    }
}

fn complement_class(class: &CharClass) -> CharClass {
    let mut out = CharClass::default();
    let mut start: Option<u32> = None;
    let mut cp = 0u32;
    while cp <= MAX_UNICODE {
        let inside = match char::from_u32(cp) {
            Some(c) => class.raw_matches(c),
            None => false,
        };
        if !inside {
            if start.is_none() {
                start = Some(cp);
            }
        } else if let Some(s) = start.take() {
            out.add_range(s, cp - 1);
        }
        cp += 1;
    }
    if let Some(s) = start {
        out.add_range(s, MAX_UNICODE);
    }
    out
}

/// Converts `\P{...}` / `[[:^alpha:]]` into something that can be unioned into
/// an enclosing class.
fn negated_class_item(mut class: CharClass) -> Result<CharClass, String> {
    if class.preds != 0 && class.ascii == [0, 0] && class.ranges.is_empty() && !class.negated {
        class.not_preds |= class.preds;
        class.preds = 0;
        Ok(class)
    } else if class.preds == 0 && !class.negated {
        Ok(complement_class(&class))
    } else if class.negated {
        class.negated = false;
        Ok(class)
    } else {
        Err("unsupported negated Unicode property".to_string())
    }
}

// -- AST & parser --------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AssertKind {
    StartText,
    EndText,
    EndTextOrFinalNewline,
    StartLine,
    EndLine,
    WordBoundary,
    NotWordBoundary,
}

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Char(char),
    Class(CharClass),
    Assert(AssertKind),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        min: u32,
        max: Option<u32>,
        greedy: bool,
        node: Box<Node>,
    },
    Group {
        index: u32,
        node: Box<Node>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegexError {
    pub message: String,
    pub position: usize,
}

impl fmt::Display for RegexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at byte {})", self.message, self.position)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegexFlags {
    pub case_insensitive: bool,
    pub multiline: bool,
    pub dot_matches_new_line: bool,
    pub ignore_whitespace: bool,
}

impl RegexFlags {
    fn set(&mut self, c: u8, on: bool) -> bool {
        match c {
            b'i' => self.case_insensitive = on,
            b'm' => self.multiline = on,
            b's' => self.dot_matches_new_line = on,
            b'x' => self.ignore_whitespace = on,
            _ => return false,
        }
        true
    }

    fn as_string(self) -> String {
        let mut s = String::new();
        if self.case_insensitive {
            s.push('i');
        }
        if self.multiline {
            s.push('m');
        }
        if self.dot_matches_new_line {
            s.push('s');
        }
        if self.ignore_whitespace {
            s.push('x');
        }
        s
    }
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    flags: RegexFlags,
    group_count: u32,
    group_names: Vec<Option<String>>,
    names: HashMap<String, u32>,
}

#[derive(Clone, Debug)]
enum ClassItem {
    Char(char),
    Class(CharClass),
}

fn decode_char(bytes: &[u8]) -> Option<(char, usize)> {
    let first = *bytes.first()?;
    if first < 0x80 {
        return Some((first as char, 1));
    }
    let len = if first >= 0xF0 {
        4
    } else if first >= 0xE0 {
        3
    } else if first >= 0xC0 {
        2
    } else {
        return None;
    };
    if bytes.len() < len {
        return None;
    }
    std::str::from_utf8(&bytes[..len])
        .ok()
        .and_then(|s| s.chars().next())
        .map(|c| (c, len))
}

#[inline]
fn next_char_at(hay: &[u8], pos: usize) -> Option<(char, usize)> {
    let b = *hay.get(pos)?;
    if b < 0x80 {
        Some((b as char, 1))
    } else {
        Some(decode_char(&hay[pos..]).unwrap_or((b as char, 1)))
    }
}

impl<'a> Parser<'a> {
    fn new(src: &'a [u8], flags: RegexFlags) -> Parser<'a> {
        Parser {
            src,
            pos: 0,
            flags,
            group_count: 0,
            group_names: vec![None],
            names: HashMap::new(),
        }
    }

    fn err(&self, message: impl Into<String>) -> RegexError {
        RegexError {
            message: message.into(),
            position: self.pos,
        }
    }

    fn err_at(&self, message: impl Into<String>, position: usize) -> RegexError {
        RegexError {
            message: message.into(),
            position,
        }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_byte_at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek_byte()?;
        self.pos += 1;
        Some(b)
    }

    fn peek_char(&self) -> Option<char> {
        next_char_at(self.src, self.pos).map(|(c, _)| c)
    }

    fn next_char(&mut self) -> Option<char> {
        let (c, len) = next_char_at(self.src, self.pos)?;
        self.pos += len;
        Some(c)
    }

    fn skip_ignored(&mut self) {
        if !self.flags.ignore_whitespace {
            return;
        }
        loop {
            match self.peek_byte() {
                Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) => {
                    self.pos += 1;
                }
                Some(b'#') => {
                    while let Some(b) = self.peek_byte() {
                        self.pos += 1;
                        if b == b'\n' {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    }

    fn parse(
        mut self,
    ) -> Result<
        (
            Node,
            u32,
            Vec<Option<String>>,
            HashMap<String, u32>,
            RegexFlags,
        ),
        RegexError,
    > {
        if self.src.len() > MAX_PATTERN {
            return Err(self.err("pattern is too long"));
        }
        self.skip_ignored();
        let node = self.parse_alt(0)?;
        self.skip_ignored();
        if self.peek_byte().is_some() {
            return Err(self.err("unexpected ')'"));
        }
        Ok((
            node,
            self.group_count,
            self.group_names,
            self.names,
            self.flags,
        ))
    }

    fn literal(&self, c: char) -> Node {
        if self.flags.case_insensitive {
            let mut class = CharClass::default();
            class.add_char(c);
            class.case_insensitive = true;
            Node::Class(class)
        } else {
            Node::Char(c)
        }
    }

    fn parse_alt(&mut self, depth: u32) -> Result<Node, RegexError> {
        if depth > MAX_PARSE_DEPTH {
            return Err(self.err("pattern nesting is too deep"));
        }
        let mut alts = vec![self.parse_concat(depth)?];
        loop {
            self.skip_ignored();
            if self.peek_byte() == Some(b'|') {
                self.bump();
                alts.push(self.parse_concat(depth)?);
            } else {
                break;
            }
        }
        if alts.len() == 1 {
            Ok(alts.pop().unwrap())
        } else {
            Ok(Node::Alt(alts))
        }
    }

    fn parse_concat(&mut self, depth: u32) -> Result<Node, RegexError> {
        let mut items = Vec::new();
        loop {
            self.skip_ignored();
            match self.peek_byte() {
                None | Some(b'|') | Some(b')') => break,
                _ => items.push(self.parse_repeat(depth)?),
            }
        }
        if items.len() == 1 {
            Ok(items.pop().unwrap())
        } else {
            Ok(Node::Concat(items))
        }
    }

    fn parse_repeat(&mut self, depth: u32) -> Result<Node, RegexError> {
        let atom = self.parse_atom(depth)?;
        self.skip_ignored();
        let (min, max) = match self.peek_byte() {
            Some(b'*') => {
                self.bump();
                (0, None)
            }
            Some(b'+') => {
                self.bump();
                (1, None)
            }
            Some(b'?') => {
                self.bump();
                (0, Some(1))
            }
            Some(b'{') => match self.try_parse_bounds()? {
                Some(bounds) => bounds,
                None => return Ok(atom),
            },
            _ => return Ok(atom),
        };
        if matches!(atom, Node::Assert(_)) {
            return Err(self.err("a zero-width assertion cannot be repeated"));
        }
        self.skip_ignored();
        let greedy = if self.peek_byte() == Some(b'?') {
            self.bump();
            false
        } else {
            true
        };
        Ok(Node::Repeat {
            min,
            max,
            greedy,
            node: Box::new(atom),
        })
    }

    fn try_parse_bounds(&mut self) -> Result<Option<(u32, Option<u32>)>, RegexError> {
        let save = self.pos;
        self.bump(); // '{'
        self.skip_ignored();
        let min = match self.parse_decimal() {
            Some(n) => n,
            None => {
                self.pos = save;
                return Ok(None);
            }
        };
        self.skip_ignored();
        let max = if self.peek_byte() == Some(b',') {
            self.bump();
            self.skip_ignored();
            let n = self.parse_decimal();
            self.skip_ignored();
            n
        } else {
            Some(min)
        };
        if self.peek_byte() != Some(b'}') {
            self.pos = save;
            return Ok(None);
        }
        self.bump();
        if min > MAX_REPEAT || max.map_or(false, |m| m > MAX_REPEAT) {
            return Err(self.err("repetition count is too large"));
        }
        if let Some(m) = max {
            if m < min {
                return Err(self.err("repetition maximum is smaller than the minimum"));
            }
        }
        Ok(Some((min, max)))
    }

    fn parse_decimal(&mut self) -> Option<u32> {
        let start = self.pos;
        let mut value: u32 = 0;
        while let Some(b @ b'0'..=b'9') = self.peek_byte() {
            self.bump();
            value = value.saturating_mul(10).saturating_add((b - b'0') as u32);
        }
        if self.pos == start {
            None
        } else {
            Some(value)
        }
    }

    fn parse_atom(&mut self, depth: u32) -> Result<Node, RegexError> {
        let c = match self.next_char() {
            Some(c) => c,
            None => return Err(self.err("unexpected end of pattern")),
        };
        match c {
            '\\' => self.parse_escape_node(),
            '(' => self.parse_group(depth),
            '[' => self.parse_class(),
            '.' => {
                let mut class = CharClass::default();
                if self.flags.dot_matches_new_line {
                    class.preds = P_ANY;
                } else {
                    // Any character except '\n'.
                    class.add_char('\n');
                    class.negated = true;
                }
                class.case_insensitive = self.flags.case_insensitive;
                Ok(Node::Class(class))
            }
            '^' => Ok(Node::Assert(if self.flags.multiline {
                AssertKind::StartLine
            } else {
                AssertKind::StartText
            })),
            '$' => Ok(Node::Assert(if self.flags.multiline {
                AssertKind::EndLine
            } else {
                AssertKind::EndText
            })),
            ')' => Err(self.err("unmatched ')'")),
            '|' => Err(self.err("empty alternative")),
            '*' | '+' | '?' => Err(self.err("nothing to repeat")),
            _ => Ok(self.literal(c)),
        }
    }

    fn parse_group(&mut self, depth: u32) -> Result<Node, RegexError> {
        if depth > MAX_PARSE_DEPTH {
            return Err(self.err("pattern nesting is too deep"));
        }
        let saved_flags = self.flags;
        if self.peek_byte() != Some(b'?') {
            self.group_count += 1;
            if self.group_count > MAX_GROUPS {
                return Err(self.err("too many capture groups"));
            }
            let index = self.group_count;
            self.group_names.push(None);
            let body = self.parse_alt(depth + 1)?;
            if self.peek_byte() != Some(b')') {
                return Err(self.err("missing ')'"));
            }
            self.bump();
            self.flags = saved_flags;
            return Ok(Node::Group {
                index,
                node: Box::new(body),
            });
        }

        self.bump(); // '?'
        match self.peek_byte() {
            Some(b':') => {
                self.bump();
                let body = self.parse_alt(depth + 1)?;
                if self.peek_byte() != Some(b')') {
                    return Err(self.err("missing ')'"));
                }
                self.bump();
                self.flags = saved_flags;
                Ok(body)
            }
            Some(b'P') => {
                self.bump();
                if self.peek_byte() != Some(b'<') {
                    return Err(self.err("expected '<' after '(?P'"));
                }
                self.parse_named_group(depth, saved_flags)
            }
            Some(b'<') => {
                if matches!(self.peek_byte_at(1), Some(b'=') | Some(b'!')) {
                    return Err(self.err("lookbehind assertions are not supported"));
                }
                self.parse_named_group(depth, saved_flags)
            }
            Some(b'=') | Some(b'!') => Err(self.err("lookahead assertions are not supported")),
            Some(b'#') => {
                while let Some(b) = self.bump() {
                    if b == b')' {
                        break;
                    }
                }
                self.flags = saved_flags;
                Ok(Node::Empty)
            }
            Some(b'-' | b'i' | b'm' | b's' | b'x') => {
                let mut new_flags = self.flags;
                let mut on = true;
                loop {
                    match self.peek_byte() {
                        Some(b'-') => {
                            if !on {
                                return Err(self.err("duplicate '-' in inline flags"));
                            }
                            self.bump();
                            on = false;
                        }
                        Some(c @ (b'i' | b'm' | b's' | b'x')) => {
                            self.bump();
                            if !new_flags.set(c, on) {
                                return Err(self.err("unsupported inline flag"));
                            }
                        }
                        Some(b':') => {
                            self.bump();
                            self.flags = new_flags;
                            let body = self.parse_alt(depth + 1)?;
                            if self.peek_byte() != Some(b')') {
                                return Err(self.err("missing ')'"));
                            }
                            self.bump();
                            self.flags = saved_flags;
                            return Ok(body);
                        }
                        Some(b')') => {
                            self.bump();
                            self.flags = new_flags;
                            return Ok(Node::Empty);
                        }
                        _ => return Err(self.err("invalid inline flags")),
                    }
                }
            }
            Some(b'\'') => Err(self.err("subroutine calls are not supported")),
            _ => Err(self.err("unrecognized group")),
        }
    }

    fn parse_named_group(
        &mut self,
        depth: u32,
        saved_flags: RegexFlags,
    ) -> Result<Node, RegexError> {
        self.bump(); // '<'
        let start = self.pos;
        while let Some(c) = self.peek_char() {
            if c == '>' {
                break;
            }
            if !(c.is_ascii_alphanumeric() || c == '_') {
                return Err(self.err("invalid capture group name"));
            }
            self.next_char();
        }
        if self.peek_byte() != Some(b'>') {
            return Err(self.err("unterminated capture group name"));
        }
        let name = match std::str::from_utf8(&self.src[start..self.pos]) {
            Ok(s) if !s.is_empty() => s.to_string(),
            _ => return Err(self.err_at("empty capture group name", start)),
        };
        if self.names.contains_key(&name) {
            return Err(self.err_at(format!("duplicate capture group name '{}'", name), start));
        }
        self.bump(); // '>'
        self.group_count += 1;
        if self.group_count > MAX_GROUPS {
            return Err(self.err("too many capture groups"));
        }
        let index = self.group_count;
        self.group_names.push(Some(name.clone()));
        self.names.insert(name, index);
        let body = self.parse_alt(depth + 1)?;
        if self.peek_byte() != Some(b')') {
            return Err(self.err("missing ')'"));
        }
        self.bump();
        self.flags = saved_flags;
        Ok(Node::Group {
            index,
            node: Box::new(body),
        })
    }

    fn parse_escape_node(&mut self) -> Result<Node, RegexError> {
        let c = match self.next_char() {
            Some(c) => c,
            None => return Err(self.err("trailing backslash")),
        };
        match c {
            'd' => Ok(Node::Class(pred_class(P_DIGIT))),
            'D' => Ok(Node::Class(neg_pred_class(P_DIGIT))),
            'w' => Ok(Node::Class(pred_class(P_WORD))),
            'W' => Ok(Node::Class(neg_pred_class(P_WORD))),
            's' => Ok(Node::Class(pred_class(P_SPACE))),
            'S' => Ok(Node::Class(neg_pred_class(P_SPACE))),
            'b' => Ok(Node::Assert(AssertKind::WordBoundary)),
            'B' => Ok(Node::Assert(AssertKind::NotWordBoundary)),
            'A' => Ok(Node::Assert(AssertKind::StartText)),
            'z' => Ok(Node::Assert(AssertKind::EndText)),
            'Z' => Ok(Node::Assert(AssertKind::EndTextOrFinalNewline)),
            'p' | 'P' => {
                let class = self.parse_property(c == 'P')?;
                Ok(Node::Class(class))
            }
            'Q' => Ok(self.parse_quoted()),
            'n' => Ok(self.literal('\n')),
            'r' => Ok(self.literal('\r')),
            't' => Ok(self.literal('\t')),
            'f' => Ok(self.literal('\x0c')),
            'v' => Ok(self.literal('\x0b')),
            'a' => Ok(self.literal('\x07')),
            'e' => Ok(self.literal('\x1b')),
            '0' => Ok(self.literal('\0')),
            'x' => {
                let c = self.parse_hex_escape(2)?;
                Ok(self.literal(c))
            }
            'u' => {
                let c = if self.peek_byte() == Some(b'{') {
                    self.parse_braced_hex()?
                } else {
                    self.parse_hex_escape(4)?
                };
                Ok(self.literal(c))
            }
            '1'..='9' => Err(self.err("backreferences are not supported")),
            'k' => Err(self.err("named backreferences are not supported")),
            _ => Ok(self.literal(c)),
        }
    }

    fn parse_quoted(&mut self) -> Node {
        let mut items = Vec::new();
        while self.pos < self.src.len() {
            if self.peek_byte() == Some(b'\\') && self.peek_byte_at(1) == Some(b'E') {
                self.pos += 2;
                break;
            }
            match self.next_char() {
                Some(c) => items.push(self.literal(c)),
                None => break,
            }
        }
        if items.is_empty() {
            Node::Empty
        } else {
            Node::Concat(items)
        }
    }

    fn parse_hex_escape(&mut self, count: usize) -> Result<char, RegexError> {
        let mut value: u32 = 0;
        for _ in 0..count {
            let c = match self.next_char() {
                Some(c) => c,
                None => return Err(self.err("incomplete hex escape")),
            };
            let d = match c.to_digit(16) {
                Some(d) => d,
                None => return Err(self.err("invalid hex escape")),
            };
            value = value * 16 + d;
        }
        char::from_u32(value).ok_or_else(|| self.err("invalid Unicode code point"))
    }

    fn parse_braced_hex(&mut self) -> Result<char, RegexError> {
        self.bump(); // '{'
        let start = self.pos;
        let mut value: u32 = 0;
        loop {
            match self.peek_char() {
                Some('}') => break,
                Some(c) => match c.to_digit(16) {
                    Some(d) => {
                        if self.pos - start > 6 {
                            return Err(self.err("Unicode escape is too long"));
                        }
                        value = value.saturating_mul(16).saturating_add(d);
                        self.next_char();
                    }
                    None => return Err(self.err("invalid Unicode escape")),
                },
                None => return Err(self.err("unterminated Unicode escape")),
            }
        }
        self.bump(); // '}'
        char::from_u32(value).ok_or_else(|| self.err("invalid Unicode code point"))
    }

    fn parse_property(&mut self, negated: bool) -> Result<CharClass, RegexError> {
        let name = if self.peek_byte() == Some(b'{') {
            self.bump();
            let start = self.pos;
            while let Some(b) = self.peek_byte() {
                if b == b'}' {
                    break;
                }
                self.bump();
            }
            if self.peek_byte() != Some(b'}') {
                return Err(self.err("unterminated \\p{...}"));
            }
            let end = self.pos;
            self.bump();
            std::str::from_utf8(&self.src[start..end])
                .map_err(|_| self.err("invalid Unicode property name"))?
                .to_string()
        } else {
            let start = self.pos;
            if self.next_char().is_none() {
                return Err(self.err("missing Unicode property name"));
            }
            std::str::from_utf8(&self.src[start..self.pos])
                .map_err(|_| self.err("invalid Unicode property name"))?
                .to_string()
        };
        let mut class = property_class(&name).ok_or_else(|| {
            self.err_at(format!("unsupported Unicode property '{}'", name), self.pos)
        })?;
        if negated {
            class = negated_class_item(class).map_err(|e| self.err(e))?;
        }
        class.case_insensitive = self.flags.case_insensitive;
        Ok(class)
    }

    fn parse_class(&mut self) -> Result<Node, RegexError> {
        let mut class = CharClass::default();
        if self.peek_byte() == Some(b'^') {
            self.bump();
            class.negated = true;
        }
        let mut first = true;
        loop {
            let b = match self.peek_byte() {
                Some(b) => b,
                None => return Err(self.err("unterminated character class")),
            };
            if b == b']' && !first {
                self.bump();
                break;
            }
            first = false;
            if b == b'[' && self.peek_byte_at(1) == Some(b':') {
                if self.parse_posix_class(&mut class)? {
                    continue;
                }
            }
            let item = self.parse_class_item()?;
            match item {
                ClassItem::Char(lo) => {
                    if lo != '-' && self.peek_byte() == Some(b'-') {
                        let after = self.peek_byte_at(1);
                        if after.is_some() && after != Some(b']') {
                            self.bump(); // '-'
                            let hi_item = self.parse_class_item()?;
                            match hi_item {
                                ClassItem::Char(hi) => {
                                    if lo > hi {
                                        return Err(self.err("invalid character class range"));
                                    }
                                    class.add_range(lo as u32, hi as u32);
                                }
                                ClassItem::Class(other) => {
                                    class.add_char(lo);
                                    class.add_char('-');
                                    class.absorb(&other);
                                }
                            }
                            continue;
                        }
                    }
                    class.add_char(lo);
                }
                ClassItem::Class(other) => class.absorb(&other),
            }
        }
        class.normalize();
        class.case_insensitive = self.flags.case_insensitive;
        Ok(Node::Class(class))
    }

    fn parse_class_item(&mut self) -> Result<ClassItem, RegexError> {
        let c = match self.next_char() {
            Some(c) => c,
            None => return Err(self.err("unterminated character class")),
        };
        if c != '\\' {
            return Ok(ClassItem::Char(c));
        }
        let e = match self.next_char() {
            Some(c) => c,
            None => return Err(self.err("trailing backslash in character class")),
        };
        match e {
            'd' => Ok(ClassItem::Class(pred_class(P_DIGIT))),
            'D' => Ok(ClassItem::Class(neg_pred_class(P_DIGIT))),
            'w' => Ok(ClassItem::Class(pred_class(P_WORD))),
            'W' => Ok(ClassItem::Class(neg_pred_class(P_WORD))),
            's' => Ok(ClassItem::Class(pred_class(P_SPACE))),
            'S' => Ok(ClassItem::Class(neg_pred_class(P_SPACE))),
            'p' | 'P' => {
                let class = self.parse_property(e == 'P')?;
                Ok(ClassItem::Class(class))
            }
            'n' => Ok(ClassItem::Char('\n')),
            'r' => Ok(ClassItem::Char('\r')),
            't' => Ok(ClassItem::Char('\t')),
            'f' => Ok(ClassItem::Char('\x0c')),
            'v' => Ok(ClassItem::Char('\x0b')),
            'a' => Ok(ClassItem::Char('\x07')),
            'e' => Ok(ClassItem::Char('\x1b')),
            '0' => Ok(ClassItem::Char('\0')),
            'b' => Ok(ClassItem::Char('\x08')),
            'x' => Ok(ClassItem::Char(self.parse_hex_escape(2)?)),
            'u' => {
                if self.peek_byte() == Some(b'{') {
                    Ok(ClassItem::Char(self.parse_braced_hex()?))
                } else {
                    Ok(ClassItem::Char(self.parse_hex_escape(4)?))
                }
            }
            _ => Ok(ClassItem::Char(e)),
        }
    }

    /// `[[:alpha:]]` / `[[:^alpha:]]`; returns false when this is not a POSIX
    /// class (the caller then parses `[` as a literal).
    fn parse_posix_class(&mut self, class: &mut CharClass) -> Result<bool, RegexError> {
        let save = self.pos;
        self.bump(); // '['
        self.bump(); // ':'
        let negated = if self.peek_byte() == Some(b'^') {
            self.bump();
            true
        } else {
            false
        };
        let start = self.pos;
        while let Some(b) = self.peek_byte() {
            if b == b':' || b == b']' {
                break;
            }
            self.bump();
        }
        if self.peek_byte() != Some(b':') || self.peek_byte_at(1) != Some(b']') {
            self.pos = save;
            return Ok(false);
        }
        let end = self.pos;
        self.bump(); // ':'
        self.bump(); // ']'
        let name = match std::str::from_utf8(&self.src[start..end]) {
            Ok(s) => s,
            Err(_) => {
                self.pos = save;
                return Ok(false);
            }
        };
        let item = match posix_class(name) {
            Some(c) => c,
            None => {
                self.pos = save;
                return Ok(false);
            }
        };
        let item = if negated {
            match negated_class_item(item) {
                Ok(c) => c,
                Err(e) => return Err(self.err(e)),
            }
        } else {
            item
        };
        let mut item = item;
        item.case_insensitive = self.flags.case_insensitive;
        class.absorb(&item);
        Ok(true)
    }
}

fn property_class(name: &str) -> Option<CharClass> {
    let mut c = CharClass::default();
    let key = name.replace(['_', '-', ' '], "").to_ascii_lowercase();
    match key.as_str() {
        "l" | "letter" | "alpha" | "alphabetic" => c.preds |= P_ALPHA,
        "lu" | "uppercase" | "upper" | "uppercaseletter" => c.preds |= P_UPPER,
        "ll" | "lowercase" | "lower" | "lowercaseletter" => c.preds |= P_LOWER,
        "n" | "number" | "numeric" => c.preds |= P_NUMERIC,
        "nd" | "digit" | "decimalnumber" | "decimaldigit" => c.preds |= P_NUMERIC,
        "p" | "punctuation" | "punct" => c.preds |= P_PUNCT,
        "z" | "zs" | "space" | "whitespace" | "separator" => c.preds |= P_SPACE,
        "w" | "word" => c.preds |= P_WORD,
        "alnum" => c.preds |= P_ALNUM,
        "c" | "cc" | "control" | "cntrl" => c.preds |= P_CONTROL,
        "ascii" => c.preds |= P_ASCII,
        "any" => c.preds |= P_ANY,
        _ => return None,
    }
    Some(c)
}

fn posix_class(name: &str) -> Option<CharClass> {
    let mut c = CharClass::default();
    match name {
        "alnum" => c.preds |= P_ALNUM,
        "alpha" => c.preds |= P_ALPHA,
        "ascii" => {
            c.add_range(0, 0x7F);
        }
        "blank" => {
            c.add_char(' ');
            c.add_char('\t');
        }
        "cntrl" => c.preds |= P_CONTROL,
        "digit" => {
            c.add_range(b'0' as u32, b'9' as u32);
        }
        "graph" => c.preds |= P_GRAPH,
        "lower" => c.preds |= P_LOWER,
        "print" => c.preds |= P_PRINT,
        "punct" => c.preds |= P_PUNCT,
        "space" => c.preds |= P_SPACE,
        "upper" => c.preds |= P_UPPER,
        "word" => c.preds |= P_WORD,
        "xdigit" => {
            c.add_range(b'0' as u32, b'9' as u32);
            c.add_range(b'a' as u32, b'f' as u32);
            c.add_range(b'A' as u32, b'F' as u32);
        }
        _ => return None,
    }
    Some(c)
}

// -- compiler -----------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Inst {
    Char(u32),
    Split(usize, usize),
    Jmp(usize),
    Save(u32),
    Assert(AssertKind),
    Match,
}

struct Compiler {
    insts: Vec<Inst>,
    classes: Vec<CharClass>,
}

impl Compiler {
    fn new() -> Compiler {
        Compiler {
            insts: Vec::new(),
            classes: Vec::new(),
        }
    }

    fn push(&mut self, inst: Inst) -> Result<usize, RegexError> {
        if self.insts.len() >= MAX_PROGRAM {
            return Err(RegexError {
                message: "pattern is too complex".to_string(),
                position: 0,
            });
        }
        self.insts.push(inst);
        Ok(self.insts.len() - 1)
    }

    fn emit_class(&mut self, class: CharClass) -> u32 {
        let index = self.classes.len() as u32;
        self.classes.push(class);
        index
    }

    fn compile(&mut self, node: &Node) -> Result<(), RegexError> {
        match node {
            Node::Empty => {}
            Node::Char(c) => {
                let mut class = CharClass::default();
                class.add_char(*c);
                let index = self.emit_class(class);
                self.push(Inst::Char(index))?;
            }
            Node::Class(class) => {
                let index = self.emit_class(class.clone());
                self.push(Inst::Char(index))?;
            }
            Node::Assert(kind) => {
                self.push(Inst::Assert(*kind))?;
            }
            Node::Concat(nodes) => {
                for node in nodes {
                    self.compile(node)?;
                }
            }
            Node::Alt(alts) => {
                let mut jumps = Vec::new();
                for (i, alt) in alts.iter().enumerate() {
                    if i + 1 < alts.len() {
                        let split = self.push(Inst::Split(0, 0))?;
                        let body = self.insts.len();
                        self.compile(alt)?;
                        let jmp = self.push(Inst::Jmp(0))?;
                        let after = self.insts.len();
                        self.insts[split] = Inst::Split(body, after);
                        jumps.push(jmp);
                    } else {
                        self.compile(alt)?;
                    }
                }
                let end = self.insts.len();
                for jmp in jumps {
                    self.insts[jmp] = Inst::Jmp(end);
                }
            }
            Node::Group { index, node } => {
                self.push(Inst::Save(2 * index))?;
                self.compile(node)?;
                self.push(Inst::Save(2 * index + 1))?;
            }
            Node::Repeat {
                min,
                max,
                greedy,
                node,
            } => {
                self.compile_repeat(*min, *max, *greedy, node)?;
            }
        }
        Ok(())
    }

    fn compile_repeat(
        &mut self,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        node: &Node,
    ) -> Result<(), RegexError> {
        for _ in 0..min {
            self.compile(node)?;
        }
        match max {
            None => {
                let split = self.push(Inst::Split(0, 0))?;
                let body = self.insts.len();
                self.compile(node)?;
                self.push(Inst::Jmp(split))?;
                let after = self.insts.len();
                self.insts[split] = if greedy {
                    Inst::Split(body, after)
                } else {
                    Inst::Split(after, body)
                };
            }
            Some(max) => {
                let extra = max.saturating_sub(min);
                for _ in 0..extra {
                    let split = self.push(Inst::Split(0, 0))?;
                    let body = self.insts.len();
                    self.compile(node)?;
                    let after = self.insts.len();
                    self.insts[split] = if greedy {
                        Inst::Split(body, after)
                    } else {
                        Inst::Split(after, body)
                    };
                }
            }
        }
        Ok(())
    }
}

// -- matching -----------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct RegexMatch {
    /// Byte offset of the start of the whole match.
    pub start: usize,
    /// Byte offset just past the end of the whole match.
    pub end: usize,
    /// Capture group 0 (the whole match) followed by groups 1..n.
    pub groups: Vec<Option<(usize, usize)>>,
}

impl RegexMatch {
    pub fn group_start(&self, group: usize) -> Option<usize> {
        self.groups.get(group).copied().flatten().map(|(s, _)| s)
    }

    pub fn group_end(&self, group: usize) -> Option<usize> {
        self.groups.get(group).copied().flatten().map(|(_, e)| e)
    }

    pub fn group_text<'a>(&self, hay: &'a [u8], group: usize) -> Option<&'a [u8]> {
        let (start, end) = self.groups.get(group).copied().flatten()?;
        hay.get(start..end)
    }
}

struct Thread {
    pc: u32,
    caps: Box<[u32]>,
}

struct ThreadList {
    threads: Vec<Thread>,
    seen: Vec<u32>,
    generation: u32,
}

impl ThreadList {
    fn new(size: usize) -> ThreadList {
        ThreadList {
            threads: Vec::new(),
            seen: vec![0; size],
            generation: 1,
        }
    }

    fn clear(&mut self) {
        self.threads.clear();
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            for slot in &mut self.seen {
                *slot = 0;
            }
            self.generation = 1;
        }
    }

    /// Marks `pc` as visited in this step. Returns false when it already was.
    fn mark(&mut self, pc: usize) -> bool {
        if self.seen[pc] == self.generation {
            false
        } else {
            self.seen[pc] = self.generation;
            true
        }
    }
}

#[inline]
fn prev_char(hay: &[u8], pos: usize) -> Option<char> {
    if pos == 0 {
        return None;
    }
    let b = hay[pos - 1];
    if b < 0x80 {
        return Some(b as char);
    }
    let start = pos.saturating_sub(4);
    for i in (start..pos).rev() {
        if let Some((c, len)) = decode_char(&hay[i..]) {
            if i + len == pos {
                return Some(c);
            }
        }
    }
    Some(b as char)
}

fn assert_holds(kind: AssertKind, hay: &[u8], pos: usize) -> bool {
    match kind {
        AssertKind::StartText => pos == 0,
        AssertKind::EndText => pos == hay.len(),
        AssertKind::EndTextOrFinalNewline => {
            pos == hay.len() || (pos + 1 == hay.len() && hay[pos] == b'\n')
        }
        AssertKind::StartLine => pos == 0 || hay.get(pos.wrapping_sub(1)) == Some(&b'\n'),
        AssertKind::EndLine => pos == hay.len() || hay.get(pos) == Some(&b'\n'),
        AssertKind::WordBoundary => {
            let before = prev_char(hay, pos).map_or(false, is_word_char);
            let after = next_char_at(hay, pos).map_or(false, |(c, _)| is_word_char(c));
            before != after
        }
        AssertKind::NotWordBoundary => {
            let before = prev_char(hay, pos).map_or(false, is_word_char);
            let after = next_char_at(hay, pos).map_or(false, |(c, _)| is_word_char(c));
            before == after
        }
    }
}

fn add_thread(
    re: &Regex,
    list: &mut ThreadList,
    pc: usize,
    caps: Box<[u32]>,
    hay: &[u8],
    pos: usize,
    stack: &mut Vec<(usize, Box<[u32]>)>,
) {
    stack.clear();
    stack.push((pc, caps));
    while let Some((pc, caps)) = stack.pop() {
        if !list.mark(pc) {
            continue;
        }
        match re.program[pc] {
            Inst::Jmp(target) => stack.push((target, caps)),
            Inst::Split(first, second) => {
                stack.push((second, caps.clone()));
                stack.push((first, caps));
            }
            Inst::Save(slot) => {
                let mut caps = caps;
                caps[slot as usize] = pos as u32;
                stack.push((pc + 1, caps));
            }
            Inst::Assert(kind) => {
                if assert_holds(kind, hay, pos) {
                    stack.push((pc + 1, caps));
                }
            }
            Inst::Char(_) | Inst::Match => {
                list.threads.push(Thread {
                    pc: pc as u32,
                    caps,
                });
            }
        }
    }
}

fn find_prefix(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(from);
    }
    if from >= hay.len() || hay.len() - from < needle.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    let first = needle[0];
    let mut i = from;
    while i <= last {
        let window = &hay[i..=last];
        let offset = match window.iter().position(|&b| b == first) {
            Some(offset) => offset,
            None => return None,
        };
        i += offset;
        if i > last {
            return None;
        }
        if hay[i..i + needle.len()] == *needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// A compiled regular expression.
#[derive(Clone, Debug)]
pub struct Regex {
    pattern: String,
    flags: RegexFlags,
    program: Vec<Inst>,
    classes: Vec<CharClass>,
    group_count: u32,
    group_names: Vec<Option<String>>,
    name_to_group: HashMap<String, u32>,
    first: Vec<CharClass>,
    prefix: Vec<u8>,
    anchored: bool,
}

impl Regex {
    pub fn new(pattern: &str) -> Result<Regex, RegexError> {
        Regex::with_flags(pattern, RegexFlags::default())
    }

    pub fn with_flags(pattern: &str, flags: RegexFlags) -> Result<Regex, RegexError> {
        let parser = Parser::new(pattern.as_bytes(), flags);
        let (node, group_count, group_names, name_to_group, effective_flags) = parser.parse()?;

        let mut prefix = Vec::new();
        collect_prefix(&node, &mut prefix);
        let first = if prefix.is_empty() {
            let (classes, nullable) = first_info(&node);
            if nullable {
                Vec::new()
            } else {
                classes
            }
        } else {
            Vec::new()
        };
        let anchored = is_anchored(&node);

        let mut compiler = Compiler::new();
        compiler.push(Inst::Save(0))?;
        compiler.compile(&node)?;
        compiler.push(Inst::Save(1))?;
        compiler.push(Inst::Match)?;

        Ok(Regex {
            pattern: pattern.to_string(),
            flags: effective_flags,
            program: compiler.insts,
            classes: compiler.classes,
            group_count,
            group_names,
            name_to_group,
            first,
            prefix,
            anchored,
        })
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn flags(&self) -> RegexFlags {
        self.flags
    }

    pub fn group_count(&self) -> u32 {
        self.group_count
    }

    pub fn group_names(&self) -> &[Option<String>] {
        &self.group_names
    }

    pub fn name_to_group(&self) -> &HashMap<String, u32> {
        &self.name_to_group
    }

    pub fn is_match(&self, hay: &[u8]) -> bool {
        self.find_at(hay, 0).is_some()
    }

    pub fn find(&self, hay: &[u8]) -> Option<RegexMatch> {
        self.find_at(hay, 0)
    }

    pub fn find_at(&self, hay: &[u8], start: usize) -> Option<RegexMatch> {
        if start > hay.len() || (self.anchored && start > 0) {
            return None;
        }
        let slot_count = 2 * (self.group_count as usize + 1);
        let mut clist = ThreadList::new(self.program.len());
        let mut nlist = ThreadList::new(self.program.len());
        let mut stack: Vec<(usize, Box<[u32]>)> = Vec::new();
        let mut best: Option<Box<[u32]>> = None;
        let mut matched = false;
        let mut pos = start;

        loop {
            if self.anchored && pos > start && clist.threads.is_empty() {
                break;
            }
            if !matched && clist.threads.is_empty() && !self.prefix.is_empty() && !self.anchored {
                match find_prefix(hay, pos, &self.prefix) {
                    Some(p) => pos = p,
                    None => break,
                }
            }

            if !matched && (!self.anchored || pos == start) && self.can_start_at(hay, pos) {
                let caps = vec![NO_POS; slot_count].into_boxed_slice();
                add_thread(self, &mut clist, 0, caps, hay, pos, &mut stack);
            }

            let threads = std::mem::take(&mut clist.threads);
            if threads.is_empty() && (matched || pos >= hay.len()) {
                break;
            }
            let next = next_char_at(hay, pos);
            let mut cut = false;
            for thread in threads {
                if cut {
                    break;
                }
                match self.program[thread.pc as usize] {
                    Inst::Match => {
                        best = Some(thread.caps);
                        matched = true;
                        cut = true;
                    }
                    Inst::Char(class_index) => {
                        if let Some((c, char_len)) = next {
                            if self.classes[class_index as usize].matches(c) {
                                add_thread(
                                    self,
                                    &mut nlist,
                                    thread.pc as usize + 1,
                                    thread.caps,
                                    hay,
                                    pos + char_len,
                                    &mut stack,
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }

            match next {
                Some((_, char_len)) => {
                    std::mem::swap(&mut clist, &mut nlist);
                    nlist.clear();
                    pos += char_len;
                }
                None => break,
            }
            if matched && clist.threads.is_empty() {
                break;
            }
        }

        let caps = best?;
        let mut groups = Vec::with_capacity(self.group_count as usize + 1);
        for group in 0..=self.group_count as usize {
            let s = caps[2 * group];
            let e = caps[2 * group + 1];
            if s != NO_POS && e != NO_POS && s <= e {
                groups.push(Some((s as usize, e as usize)));
            } else {
                groups.push(None);
            }
        }
        let (start, end) = groups[0]?;
        Some(RegexMatch { start, end, groups })
    }

    fn can_start_at(&self, hay: &[u8], pos: usize) -> bool {
        if pos > hay.len() {
            return false;
        }
        if !self.prefix.is_empty() {
            return hay
                .get(pos..)
                .map_or(false, |rest| rest.starts_with(&self.prefix));
        }
        if !self.first.is_empty() {
            let (c, _) = match next_char_at(hay, pos) {
                Some(v) => v,
                None => return false,
            };
            return self.first.iter().any(|class| class.matches(c));
        }
        true
    }

    pub fn matches<'a>(&'a self, hay: &'a [u8]) -> Matches<'a> {
        Matches {
            regex: self,
            hay,
            at: 0,
            done: false,
        }
    }

    pub fn replace(&self, hay: &[u8], replacement: &[u8]) -> Vec<u8> {
        match self.find_at(hay, 0) {
            Some(m) => {
                let mut out = Vec::with_capacity(hay.len());
                out.extend_from_slice(&hay[..m.start]);
                self.append_replacement(&mut out, hay, &m, replacement);
                out.extend_from_slice(&hay[m.end..]);
                out
            }
            None => hay.to_vec(),
        }
    }

    pub fn replace_all(&self, hay: &[u8], replacement: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(hay.len());
        let mut last = 0;
        for m in self.matches(hay) {
            out.extend_from_slice(&hay[last..m.start]);
            self.append_replacement(&mut out, hay, &m, replacement);
            last = m.end;
        }
        out.extend_from_slice(&hay[last..]);
        out
    }

    pub fn split(&self, hay: &[u8], limit: usize) -> Vec<Vec<u8>> {
        if limit == 1 {
            return vec![hay.to_vec()];
        }
        let mut out = Vec::new();
        let mut last = 0;
        for m in self.matches(hay) {
            if limit > 1 && out.len() + 1 >= limit {
                break;
            }
            out.push(hay[last..m.start].to_vec());
            last = m.end;
        }
        out.push(hay[last..].to_vec());
        out
    }

    fn append_replacement(
        &self,
        out: &mut Vec<u8>,
        hay: &[u8],
        m: &RegexMatch,
        replacement: &[u8],
    ) {
        let mut i = 0;
        while i < replacement.len() {
            let b = replacement[i];
            if b != b'$' {
                out.push(b);
                i += 1;
                continue;
            }
            if i + 1 >= replacement.len() {
                out.push(b'$');
                break;
            }
            let next = replacement[i + 1];
            if next == b'$' {
                out.push(b'$');
                i += 2;
                continue;
            }
            if next == b'{' {
                let mut j = i + 2;
                while j < replacement.len() && replacement[j] != b'}' {
                    j += 1;
                }
                if j < replacement.len() {
                    let name = std::str::from_utf8(&replacement[i + 2..j]).ok();
                    if let Some(name) = name {
                        if let Some(&group) = self.name_to_group.get(name) {
                            self.append_group(out, hay, m, group as usize);
                        }
                    }
                    i = j + 1;
                } else {
                    out.push(b'$');
                    i += 1;
                }
                continue;
            }
            if next.is_ascii_digit() {
                let start = i + 1;
                let mut j = start;
                while j < replacement.len() && replacement[j].is_ascii_digit() && j - start < 2 {
                    j += 1;
                }
                let mut group = 0usize;
                for &d in &replacement[start..j] {
                    group = group * 10 + (d - b'0') as usize;
                }
                self.append_group(out, hay, m, group);
                i = j;
                continue;
            }
            if next.is_ascii_alphabetic() || next == b'_' {
                let start = i + 1;
                let mut j = start;
                while j < replacement.len()
                    && (replacement[j].is_ascii_alphanumeric() || replacement[j] == b'_')
                {
                    j += 1;
                }
                let name = std::str::from_utf8(&replacement[start..j]).ok();
                if let Some(name) = name {
                    if let Some(&group) = self.name_to_group.get(name) {
                        self.append_group(out, hay, m, group as usize);
                        i = j;
                        continue;
                    }
                }
                out.push(b'$');
                i += 1;
                continue;
            }
            out.push(b'$');
            i += 1;
        }
    }

    fn append_group(&self, out: &mut Vec<u8>, hay: &[u8], m: &RegexMatch, group: usize) {
        if let Some(Some((start, end))) = m.groups.get(group) {
            if let Some(slice) = hay.get(*start..*end) {
                out.extend_from_slice(slice);
            }
        }
    }

    pub fn escape(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() + 8);
        for &b in input {
            if b.is_ascii() && !b.is_ascii_alphanumeric() && b != b'_' && !b.is_ascii_whitespace() {
                out.push(b'\\');
            }
            out.push(b);
        }
        out
    }
}

pub struct Matches<'a> {
    regex: &'a Regex,
    hay: &'a [u8],
    at: usize,
    done: bool,
}

impl<'a> Iterator for Matches<'a> {
    type Item = RegexMatch;

    fn next(&mut self) -> Option<RegexMatch> {
        if self.done {
            return None;
        }
        let m = match self.regex.find_at(self.hay, self.at) {
            Some(m) => m,
            None => {
                self.done = true;
                return None;
            }
        };
        if m.start == m.end {
            match next_char_at(self.hay, m.end) {
                Some((_, len)) => self.at = m.end + len,
                None => self.done = true,
            }
        } else {
            self.at = m.end;
        }
        Some(m)
    }
}

fn is_nullable(node: &Node) -> bool {
    match node {
        Node::Empty | Node::Assert(_) => true,
        Node::Char(_) | Node::Class(_) => false,
        Node::Concat(nodes) => nodes.iter().all(is_nullable),
        Node::Alt(nodes) => nodes.iter().any(is_nullable),
        Node::Repeat { min, node, .. } => *min == 0 || is_nullable(node),
        Node::Group { node, .. } => is_nullable(node),
    }
}

/// Collects the longest mandatory literal prefix. Returns nothing after the
/// first part that cannot be followed literally.
fn collect_prefix(node: &Node, out: &mut Vec<u8>) -> bool {
    match node {
        Node::Char(c) => {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            true
        }
        Node::Empty | Node::Assert(_) => true,
        Node::Group { node, .. } => collect_prefix(node, out),
        Node::Concat(nodes) => {
            for node in nodes {
                if !collect_prefix(node, out) {
                    return false;
                }
            }
            true
        }
        Node::Repeat { min, node, .. } => {
            if *min >= 1 && !is_nullable(node) {
                collect_prefix(node, out);
            }
            false
        }
        Node::Alt(_) | Node::Class(_) => false,
    }
}

fn first_info(node: &Node) -> (Vec<CharClass>, bool) {
    match node {
        Node::Empty | Node::Assert(_) => (Vec::new(), true),
        Node::Char(c) => {
            let mut class = CharClass::default();
            class.add_char(*c);
            (vec![class], false)
        }
        Node::Class(class) => (vec![class.clone()], false),
        Node::Group { node, .. } => first_info(node),
        Node::Concat(nodes) => {
            let mut classes = Vec::new();
            let mut nullable = true;
            for node in nodes {
                let (first, node_nullable) = first_info(node);
                classes.extend(first);
                if !node_nullable {
                    nullable = false;
                    break;
                }
            }
            (classes, nullable)
        }
        Node::Alt(nodes) => {
            let mut classes = Vec::new();
            let mut nullable = false;
            for node in nodes {
                let (first, node_nullable) = first_info(node);
                classes.extend(first);
                nullable |= node_nullable;
            }
            (classes, nullable)
        }
        Node::Repeat { min, node, .. } => {
            let (first, node_nullable) = first_info(node);
            let nullable = *min == 0 || node_nullable;
            (first, nullable)
        }
    }
}

fn is_anchored(node: &Node) -> bool {
    match node {
        Node::Assert(AssertKind::StartText) => true,
        Node::Group { node, .. } => is_anchored(node),
        Node::Concat(nodes) => nodes
            .iter()
            .find(|node| !matches!(node, Node::Empty))
            .map_or(false, is_anchored),
        _ => false,
    }
}

// -- Lua binding ---------------------------------------------------------------------

const REGEX_MT: &str = "luau.regex";
const REGEX_METHODS: &str = "luau.regex.methods";
const MATCH_MT: &str = "luau.regex.match";
const MATCH_METHODS: &str = "luau.regex.matchmethods";

unsafe fn set_fn(L: *mut lua_State, name: &str, f: lua_CFunction) {
    lua_pushcclosurek(L, f, Some(name), 0, None);
    lua_setfield(L, -2, name);
}

unsafe fn regex_dtor(_L: *mut lua_State, p: *mut u8) {
    let ptr = std::ptr::read_unaligned(p as *const *mut Regex);
    if !ptr.is_null() {
        drop(Box::from_raw(ptr));
    }
}

unsafe fn push_regex(L: *mut lua_State, re: Regex) -> i32 {
    let data = lua_newuserdatadtor(L, std::mem::size_of::<*mut Regex>(), regex_dtor);
    std::ptr::write_unaligned(data as *mut *mut Regex, Box::into_raw(Box::new(re)));
    lua_getfield(L, LUA_REGISTRYINDEX, REGEX_MT);
    lua_setmetatable(L, -2);
    1
}

unsafe fn check_regex<'a>(L: *mut lua_State, idx: i32) -> &'a Regex {
    let data = luaL_checkudata(L, idx, REGEX_MT) as *const *mut Regex;
    &**data
}

fn format_regex_error(pattern: &[u8], error: &RegexError) -> String {
    let shown = String::from_utf8_lossy(pattern);
    let mut shown: String = shown.chars().take(120).collect();
    if shown.len() < pattern.len() {
        shown.push_str("...");
    }
    format!("invalid regex '{}': {}", shown, error)
}

unsafe fn compile_pattern(L: *mut lua_State, pattern: &[u8], flags: RegexFlags) -> Regex {
    let pattern_str = match std::str::from_utf8(pattern) {
        Ok(s) => s,
        Err(_) => luaL_error(L, "regex pattern must be valid UTF-8"),
    };
    match Regex::with_flags(pattern_str, flags) {
        Ok(re) => re,
        Err(error) => luaL_error_owned(L, format_regex_error(pattern, &error)),
    }
}

unsafe fn check_flags(L: *mut lua_State, idx: i32) -> RegexFlags {
    let mut flags = RegexFlags::default();
    if lua_isnoneornil(L, idx) {
        return flags;
    }
    let text = luaL_checklstring(L, idx);
    for &b in text {
        match b {
            b'i' => flags.case_insensitive = true,
            b'm' => flags.multiline = true,
            b's' => flags.dot_matches_new_line = true,
            b'x' => flags.ignore_whitespace = true,
            // Accepted for compatibility but meaningless here: the engine is
            // always Unicode-aware and Regex objects are already global.
            b'g' | b'u' | b'U' => {}
            _ => luaL_error_owned(L, format!("invalid regex flag '{}'", b as char)),
        }
    }
    flags
}

unsafe fn push_pos(L: *mut lua_State, pos: usize) {
    if pos <= i32::MAX as usize {
        lua_pushinteger(L, pos as i32);
    } else {
        lua_pushinteger64(L, pos as i64);
    }
}

unsafe fn push_match(L: *mut lua_State, hay: &[u8], m: &RegexMatch, group_count: u32) -> i32 {
    lua_createtable(L, 0, 12);

    lua_createtable(L, m.groups.len() as i32, 0);
    for (index, group) in m.groups.iter().enumerate() {
        match group {
            Some((start, end)) => lua_pushlstring(L, &hay[*start..*end]),
            None => lua_pushnil(L),
        }
        lua_rawseti(L, -2, index as i32);
    }
    lua_setfield(L, -2, "Captures");

    lua_createtable(L, m.groups.len() as i32, 0);
    for (index, group) in m.groups.iter().enumerate() {
        match group {
            Some((start, _)) => push_pos(L, start + 1),
            None => lua_pushnil(L),
        }
        lua_rawseti(L, -2, index as i32);
    }
    lua_setfield(L, -2, "Starts");

    lua_createtable(L, m.groups.len() as i32, 0);
    for (index, group) in m.groups.iter().enumerate() {
        match group {
            Some((_, end)) => push_pos(L, *end),
            None => lua_pushnil(L),
        }
        lua_rawseti(L, -2, index as i32);
    }
    lua_setfield(L, -2, "Ends");

    lua_pushlstring(L, &hay[m.start..m.end]);
    lua_setfield(L, -2, "Text");
    lua_pushlstring(L, &hay[m.start..m.end]);
    lua_setfield(L, -2, "Value");
    push_pos(L, m.start + 1);
    lua_setfield(L, -2, "Start");
    push_pos(L, m.end);
    lua_setfield(L, -2, "Finish");
    push_pos(L, m.start);
    lua_setfield(L, -2, "ByteStart");
    push_pos(L, m.end);
    lua_setfield(L, -2, "ByteEnd");
    lua_pushinteger(L, group_count as i32);
    lua_setfield(L, -2, "GroupCount");

    lua_getfield(L, LUA_REGISTRYINDEX, MATCH_MT);
    lua_setmetatable(L, -2);
    1
}

unsafe fn contents_bytes<'a>(L: *mut lua_State, idx: i32) -> &'a [u8] {
    let idx = lua_absindex(L, idx);
    if lua_istable(L, idx) && lua_getmetatable(L, idx) != 0 {
        lua_getfield(L, LUA_REGISTRYINDEX, MATCH_MT);
        let same = lua_rawequal(L, -1, -2) != 0;
        lua_pop(L, 2);
        if same {
            lua_rawgetfield(L, idx, "Text");
            let text = luaL_checklstring(L, -1);
            lua_pop(L, 1);
            return text;
        }
    }
    luaL_checklstring(L, idx)
}

unsafe fn regex_new(L: *mut lua_State) -> i32 {
    let pattern = luaL_checklstring(L, 1);
    let flags = check_flags(L, 2);
    let re = compile_pattern(L, pattern, flags);
    push_regex(L, re)
}

unsafe fn regex_escape(L: *mut lua_State) -> i32 {
    let text = luaL_checklstring(L, 1);
    let escaped = Regex::escape(text);
    lua_pushlstring(L, &escaped);
    1
}

unsafe fn regex_tostring(L: *mut lua_State) -> i32 {
    let re = check_regex(L, 1);
    lua_pushstring(L, &format!("Regex({})", re.pattern()));
    1
}

unsafe fn regex_index(L: *mut lua_State) -> i32 {
    let key = luaL_checklstring(L, 2);
    lua_getfield(L, LUA_REGISTRYINDEX, REGEX_METHODS);
    lua_pushvalue(L, 2);
    lua_rawget(L, -2);
    if !lua_isnil(L, -1) {
        return 1;
    }
    lua_pop(L, 2);
    let re = check_regex(L, 1);
    match key {
        b"Pattern" | b"Source" => lua_pushstring(L, re.pattern()),
        b"Flags" => lua_pushstring(L, &re.flags.as_string()),
        b"GroupCount" => lua_pushinteger(L, re.group_count as i32),
        b"GroupNames" => {
            lua_createtable(L, 0, re.name_to_group.len() as i32);
            for (name, &index) in &re.name_to_group {
                lua_pushstring(L, name);
                lua_pushinteger(L, index as i32);
                lua_settable(L, -3);
            }
        }
        _ => lua_pushnil(L),
    }
    1
}

unsafe fn match_tostring(L: *mut lua_State) -> i32 {
    lua_getfield(L, 1, "Text");
    if lua_isnil(L, -1) {
        lua_pop(L, 1);
        lua_pushstring(L, "");
    }
    1
}

unsafe fn match_get_capture(L: *mut lua_State) -> i32 {
    let index = luaL_optinteger(L, 2, 0);
    if index < 0 {
        luaL_argerror(L, 2, "capture index must be non-negative");
    }
    lua_getfield(L, 1, "Captures");
    lua_rawgeti(L, -1, index);
    1
}

unsafe fn match_get_start(L: *mut lua_State) -> i32 {
    let index = luaL_optinteger(L, 2, 0);
    if index < 0 {
        luaL_argerror(L, 2, "capture index must be non-negative");
    }
    lua_getfield(L, 1, "Starts");
    lua_rawgeti(L, -1, index);
    1
}

unsafe fn match_get_end(L: *mut lua_State) -> i32 {
    let index = luaL_optinteger(L, 2, 0);
    if index < 0 {
        luaL_argerror(L, 2, "capture index must be non-negative");
    }
    lua_getfield(L, 1, "Ends");
    lua_rawgeti(L, -1, index);
    1
}

unsafe fn match_get_text(L: *mut lua_State) -> i32 {
    lua_getfield(L, 1, "Text");
    1
}

unsafe fn regex_match(L: *mut lua_State) -> i32 {
    let re = check_regex(L, 1);
    let hay = luaL_checklstring(L, 2);
    match re.find_at(hay, 0) {
        Some(m) => push_match(L, hay, &m, re.group_count),
        None => {
            lua_pushnil(L);
            1
        }
    }
}

unsafe fn regex_find(L: *mut lua_State) -> i32 {
    let re = check_regex(L, 1);
    let hay = luaL_checklstring(L, 2);
    let init = luaL_optinteger(L, 3, 1);
    let start = if init > 0 {
        (init - 1) as usize
    } else if init < 0 {
        hay.len().saturating_sub((-init) as usize)
    } else {
        0
    };
    match re.find_at(hay, start.min(hay.len())) {
        Some(m) => push_match(L, hay, &m, re.group_count),
        None => {
            lua_pushnil(L);
            1
        }
    }
}

unsafe fn regex_match_many(L: *mut lua_State) -> i32 {
    let re = check_regex(L, 1);
    let hay = luaL_checklstring(L, 2);
    let limit = luaL_optinteger(L, 3, -1);
    let limit = if limit <= 0 {
        usize::MAX
    } else {
        limit as usize
    };
    lua_createtable(L, 0, 0);
    let mut count = 0usize;
    for m in re.matches(hay) {
        if count >= limit {
            break;
        }
        push_match(L, hay, &m, re.group_count);
        lua_rawseti(L, -2, count as i32 + 1);
        count += 1;
    }
    1
}

unsafe fn regex_test(L: *mut lua_State) -> i32 {
    let re = check_regex(L, 1);
    let hay = luaL_checklstring(L, 2);
    lua_pushboolean(L, re.is_match(hay) as i32);
    1
}

unsafe fn regex_replace_impl(L: *mut lua_State, all: bool) -> i32 {
    let re = check_regex(L, 1);
    let hay = contents_bytes(L, 2);
    if !lua_isfunction(L, 3) {
        let replacement = luaL_checklstring(L, 3);
        let out = if all {
            re.replace_all(hay, replacement)
        } else {
            re.replace(hay, replacement)
        };
        lua_pushlstring(L, &out);
        return 1;
    }

    // A Lua replacement callback may run arbitrary code, so keep our own copy
    // of the input instead of relying on Lua's string lifetime.
    let hay = hay.to_vec();
    let mut out = Vec::with_capacity(hay.len());
    let mut last = 0usize;
    let mut any = false;
    for m in re.matches(&hay) {
        out.extend_from_slice(&hay[last..m.start]);
        lua_pushvalue(L, 3);
        push_match(L, &hay, &m, re.group_count);
        let status = lua_pcall(L, 1, 1, 0);
        if status != LUA_OK {
            drop(out);
            lua_error(L);
        }
        let result = lua_tolstring(L, -1);
        match result {
            Some(s) => out.extend_from_slice(s),
            None => {
                let ty = luaL_typename(L, -1);
                drop(out);
                luaL_error_owned(
                    L,
                    format!(
                        "regex replacement function must return a string, got {}",
                        ty
                    ),
                );
            }
        }
        lua_pop(L, 1);
        last = m.end;
        any = true;
        if !all {
            break;
        }
    }
    if !any {
        lua_pushlstring(L, &hay);
        return 1;
    }
    out.extend_from_slice(&hay[last..]);
    lua_pushlstring(L, &out);
    1
}

unsafe fn regex_replace(L: *mut lua_State) -> i32 {
    regex_replace_impl(L, false)
}

unsafe fn regex_replace_all(L: *mut lua_State) -> i32 {
    regex_replace_impl(L, true)
}

unsafe fn regex_split(L: *mut lua_State) -> i32 {
    let re = check_regex(L, 1);
    let hay = luaL_checklstring(L, 2);
    let limit = luaL_optinteger(L, 3, -1);
    let limit = if limit <= 0 {
        usize::MAX
    } else {
        limit as usize
    };
    let pieces = re.split(hay, limit);
    lua_createtable(L, pieces.len().min(i32::MAX as usize) as i32, 0);
    for (index, piece) in pieces.iter().enumerate() {
        lua_pushlstring(L, piece);
        lua_rawseti(L, -2, index as i32 + 1);
    }
    1
}

unsafe fn regex_count(L: *mut lua_State) -> i32 {
    let re = check_regex(L, 1);
    let hay = luaL_checklstring(L, 2);
    let count = re.matches(hay).count();
    push_pos(L, count);
    1
}

unsafe fn regex_get_pattern(L: *mut lua_State) -> i32 {
    let re = check_regex(L, 1);
    lua_pushstring(L, re.pattern());
    1
}

/// Opens the `Regex` library (global `Regex` plus the `Regex`/match
/// metatables and their method tables).
pub unsafe fn luaopen_regex(L: *mut lua_State) -> i32 {
    // Regex methods
    lua_newtable(L);
    set_fn(L, "Match", regex_match);
    set_fn(L, "match", regex_match);
    set_fn(L, "MatchMany", regex_match_many);
    set_fn(L, "matchMany", regex_match_many);
    set_fn(L, "MatchAll", regex_match_many);
    set_fn(L, "matchAll", regex_match_many);
    set_fn(L, "Find", regex_find);
    set_fn(L, "find", regex_find);
    set_fn(L, "Test", regex_test);
    set_fn(L, "test", regex_test);
    set_fn(L, "IsMatch", regex_test);
    set_fn(L, "isMatch", regex_test);
    set_fn(L, "Replace", regex_replace);
    set_fn(L, "replace", regex_replace);
    set_fn(L, "ReplaceAll", regex_replace_all);
    set_fn(L, "replaceAll", regex_replace_all);
    set_fn(L, "Split", regex_split);
    set_fn(L, "split", regex_split);
    set_fn(L, "Count", regex_count);
    set_fn(L, "count", regex_count);
    set_fn(L, "GetPattern", regex_get_pattern);
    set_fn(L, "getPattern", regex_get_pattern);
    lua_setreadonly(L, -1, 1);
    lua_pushvalue(L, -1);
    lua_setfield(L, LUA_REGISTRYINDEX, REGEX_METHODS);
    lua_pop(L, 1);

    // Regex metatable
    lua_newtable(L);
    set_fn(L, "__index", regex_index);
    set_fn(L, "__tostring", regex_tostring);
    lua_pushstring(L, "Regex");
    lua_setfield(L, -2, "__type");
    lua_pushstring(L, "The metatable is locked");
    lua_setfield(L, -2, "__metatable");
    lua_setreadonly(L, -1, 1);
    lua_pushvalue(L, -1);
    lua_setfield(L, LUA_REGISTRYINDEX, REGEX_MT);
    lua_pop(L, 1);

    // Match methods
    lua_newtable(L);
    set_fn(L, "GetCapture", match_get_capture);
    set_fn(L, "getCapture", match_get_capture);
    set_fn(L, "GetStart", match_get_start);
    set_fn(L, "getStart", match_get_start);
    set_fn(L, "GetEnd", match_get_end);
    set_fn(L, "getEnd", match_get_end);
    set_fn(L, "GetText", match_get_text);
    set_fn(L, "getText", match_get_text);
    lua_setreadonly(L, -1, 1);
    lua_pushvalue(L, -1);
    lua_setfield(L, LUA_REGISTRYINDEX, MATCH_METHODS);
    lua_pop(L, 1);

    // Match metatable
    lua_newtable(L);
    lua_getfield(L, LUA_REGISTRYINDEX, MATCH_METHODS);
    lua_setfield(L, -2, "__index");
    set_fn(L, "__tostring", match_tostring);
    lua_pushstring(L, "RegexMatch");
    lua_setfield(L, -2, "__type");
    lua_pushstring(L, "The metatable is locked");
    lua_setfield(L, -2, "__metatable");
    lua_setreadonly(L, -1, 1);
    lua_pushvalue(L, -1);
    lua_setfield(L, LUA_REGISTRYINDEX, MATCH_MT);
    lua_pop(L, 1);

    // public library table
    lua_newtable(L);
    set_fn(L, "new", regex_new);
    set_fn(L, "compile", regex_new);
    set_fn(L, "Escape", regex_escape);
    set_fn(L, "escape", regex_escape);
    lua_setreadonly(L, -1, 1);
    lua_setglobal(L, "Regex");
    0
}

// -- tests ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn find(pattern: &str, hay: &str) -> Option<RegexMatch> {
        Regex::new(pattern).unwrap().find(hay.as_bytes())
    }

    #[test]
    fn literals_and_captures() {
        let re = Regex::new(r"My Cool Id:\s*(\w+)").unwrap();
        let hay = b"My Cool Id: 31235335";
        let m = re.find(hay).unwrap();
        assert_eq!(m.group_text(hay, 1), Some(&b"31235335"[..]));
        assert_eq!(m.group_text(hay, 2), None);
        assert_eq!(re.group_count(), 1);
    }

    #[test]
    fn escape_roundtrip() {
        let input = "[DEBUG] (ID): 99821";
        let label = "[DEBUG] (ID): ";
        let escaped = Regex::escape(label.as_bytes());
        let mut pattern = String::from_utf8(escaped).unwrap();
        pattern.push_str("(\\d+)");
        let re = Regex::new(&pattern).unwrap();
        let m = re.find(input.as_bytes()).unwrap();
        assert_eq!(m.group_text(input.as_bytes(), 1), Some(&b"99821"[..]));
        assert_eq!(re.replace(input.as_bytes(), b"$1"), b"99821".to_vec());
    }

    #[test]
    fn greedy_lazy_and_alternation() {
        assert_eq!(find("a+", "aaa").unwrap().end, 3);
        assert_eq!(find("a+?", "aaa").unwrap().end, 1);
        assert_eq!(find("ab|a", "ab").unwrap().end, 2);
        assert_eq!(find("a{2,3}", "aaaa").unwrap().end, 3);
        assert_eq!(find("a{2,3}?", "aaaa").unwrap().end, 2);
    }

    #[test]
    fn classes_and_predicates() {
        assert_eq!(find("[a-c]+", "xxabcxx").unwrap().start, 2);
        assert_eq!(find("[^0-9]+", "123abc").unwrap().start, 3);
        assert!(find(r"\d+", "abc123").is_some());
        assert!(find(r"\D+", "123abc").is_some());
        assert!(find(r"\p{L}+", "123h\u{e9}llo").is_some());
        assert!(find(r"[[:alpha:]]+", "123abc").is_some());
        assert_eq!(find(r"\bfoo\b", "a foo b").unwrap().start, 2);
        assert!(find(r"\bfoo\b", "afoob").is_none());
    }

    #[test]
    fn anchors_and_flags() {
        assert!(find("^abc", "xabc").is_none());
        assert!(find(r"\Aabc", "abc").is_some());
        assert!(find("abc$", "abcx").is_none());
        assert!(find("abc$", "abc").is_some());
        assert!(find(r"(?m)^b", "a\nb").is_some());
        assert!(find(r"(?s)a.b", "a\nb").is_some());
        assert!(find("a.b", "a\nb").is_none());
        assert!(find(r"(?i)abc", "ABC").is_some());
        assert!(find(r"(?i)[a-z]+", "ABC").is_some());
        assert!(find(r"(?i:abc)d", "ABCd").is_some());
        assert!(find(r"(?i:abc)d", "ABCe").is_none());
        assert!(find(r"(?i)a(?-i:b)c", "AbC").is_some());
        assert!(find(r"(?i)a(?-i:b)c", "ABC").is_none());
    }

    #[test]
    fn unicode_and_empty() {
        assert!(find("\u{e9}+", "caf\u{e9}").is_some());
        assert!(find(".", "\u{e9}").is_some());
        let m = find("a*", "ba").unwrap();
        assert_eq!((m.start, m.end), (0, 0));
        let all: Vec<_> = Regex::new("a*").unwrap().matches(b"ba").collect();
        assert_eq!(
            all.iter().map(|m| (m.start, m.end)).collect::<Vec<_>>(),
            vec![(0, 0), (1, 2), (2, 2)]
        );
    }

    #[test]
    fn named_groups_and_replace() {
        let re = Regex::new(r"(?P<year>\d{4})-(?<month>\d{2})").unwrap();
        assert_eq!(
            re.replace_all(b"2024-05 and 1999-12", b"${month}/${year}"),
            b"05/2024 and 12/1999".to_vec()
        );
        assert_eq!(re.replace(b"2024-05", b"$2"), b"05".to_vec());
    }

    #[test]
    fn split_and_iteration() {
        let re = Regex::new(r"\s*,\s*").unwrap();
        let pieces = re.split(b"a, b,c", 0);
        assert_eq!(pieces, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        let re = Regex::new(r"[a-z](\d+)").unwrap();
        let found: Vec<(usize, usize)> = re
            .matches(b"a1 b22 c333")
            .map(|m| (m.start, m.end))
            .collect();
        assert_eq!(found, vec![(0, 2), (3, 6), (7, 11)]);
    }

    #[test]
    fn unmatched_groups() {
        let re = Regex::new(r"(a)?b").unwrap();
        let m = re.find(b"b").unwrap();
        assert_eq!(m.group_text(b"b", 1), None);
        assert_eq!(m.group_text(b"b", 0), Some(&b"b"[..]));
    }

    #[test]
    fn no_catastrophic_backtracking() {
        let re = Regex::new(r"(a*)*b").unwrap();
        let hay = vec![b'a'; 20000];
        assert!(re.find(&hay).is_none());
    }

    #[test]
    fn parse_errors() {
        assert!(Regex::new("(").is_err());
        assert!(Regex::new("[a").is_err());
        assert!(Regex::new(r"(a)\1").is_err());
        assert!(Regex::new(r"(?=a)").is_err());
        assert!(Regex::new("a{3,2}").is_err());
    }
}
