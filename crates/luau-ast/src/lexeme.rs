//! Port of `Luau/Lexer.h` lexeme definitions.
//!
//! Lexemes created by the lexer reference slices of the source buffer via
//! byte offsets (`LexemeData::Slice`) — the equivalent of the C++ union's
//! `data` pointer plus `length` — and interned names via `LexemeData::Name`.

use crate::location::Location;

/// Single-character token kinds (punctuation).
pub type CharToken = u8;

/// Lexeme type. Mirrors the C++ `Lexeme::Type` enum.
///
/// In the C++ version, values 1..255 are actual character values; here they
/// are represented as [`LexemeType::Char`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LexemeType {
    Eof,

    Equal,
    LessEqual,
    GreaterEqual,
    NotEqual,
    Dot2,
    Dot3,
    SkinnyArrow,
    DoubleColon,
    FloorDiv,

    InterpStringBegin,
    InterpStringMid,
    InterpStringEnd,
    /// An interpolated string with no expressions (like `x`)
    InterpStringSimple,

    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    FloorDivAssign,
    ModAssign,
    PowAssign,
    ConcatAssign,

    RawString,
    QuotedString,
    Number,
    Name,

    Comment,
    BlockComment,

    Attribute,
    AttributeOpen,

    BrokenString,
    BrokenComment,
    BrokenUnicode,
    BrokenInterpDoubleBrace,
    Error,

    Reserved(&'static str),

    /// Single-character punctuation token; carries the raw character value
    /// (C++ lexemes use the character value directly as the token type).
    Char(u8),
}

impl LexemeType {
    pub fn is_reserved(&self) -> bool {
        matches!(self, LexemeType::Reserved(_))
    }
}

/// Quote style of a `QuotedString` lexeme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuoteStyle {
    Single,
    Double,
}

/// Payload data of a lexeme.
///
/// The C++ version stores a pointer into the source buffer (for strings,
/// numbers, comments), a pointer to an interned name (identifiers), or a
/// codepoint (for `BrokenUnicode`). Here, source-buffer slices keep byte
/// offsets instead — this keeps `Lexeme` self-contained and copyable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LexemeData {
    None,
    /// Byte range in the source buffer (strings, numbers, comments).
    Slice { start: u32, length: u32 },
    /// Interned identifier/attribute name (`Name`, `Attribute`, reserved words).
    Name(&'static str),
    /// Unicode codepoint (`BrokenUnicode`).
    Codepoint(u32),
}

/// A lexer token. Mirrors the C++ `Lexeme` struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Lexeme {
    pub ty: LexemeType,
    pub location: Location,
    pub data: LexemeData,
}

impl Lexeme {
    #[inline]
    pub fn new(location: Location, ty: LexemeType) -> Self {
        Lexeme { ty, location, data: LexemeData::None }
    }

    /// Create a single-character token (`Lexeme(location, character)`).
    #[inline]
    pub fn new_char(location: Location, ch: u8) -> Self {
        Lexeme { ty: LexemeType::Char(ch), location, data: LexemeData::None }
    }

    /// Create a lexeme referencing a source slice
    /// (`Lexeme(location, type, data, size)`).
    #[inline]
    pub fn new_slice(location: Location, ty: LexemeType, start: u32, length: u32) -> Self {
        debug_assert!(matches!(
            ty,
            LexemeType::RawString
                | LexemeType::QuotedString
                | LexemeType::InterpStringBegin
                | LexemeType::InterpStringMid
                | LexemeType::InterpStringEnd
                | LexemeType::InterpStringSimple
                | LexemeType::BrokenInterpDoubleBrace
                | LexemeType::Number
                | LexemeType::Comment
                | LexemeType::BlockComment
        ));
        Lexeme { ty, location, data: LexemeData::Slice { start, length } }
    }

    /// Create a name-bearing lexeme (`Lexeme(location, type, name)`).
    #[inline]
    pub fn new_name(location: Location, ty: LexemeType, name: &'static str) -> Self {
        debug_assert!(matches!(
            ty,
            LexemeType::Name | LexemeType::Attribute | LexemeType::Reserved(_)
        ));
        Lexeme { ty, location, data: LexemeData::Name(name) }
    }

    /// Extract the source value for slice lexemes; empty for other types.
    pub fn get_slice<'a>(&self, source: &'a [u8]) -> &'a [u8] {
        match self.data {
            LexemeData::Slice { start, length } => {
                &source[start as usize..(start + length) as usize]
            }
            _ => &[],
        }
    }

    /// Length of the source slice. Panics for non-slice lexeme types,
    /// mirroring the C++ assert in `Lexeme::getLength`.
    pub fn get_length(&self) -> u32 {
        debug_assert!(matches!(
            self.ty,
            LexemeType::RawString
                | LexemeType::QuotedString
                | LexemeType::InterpStringBegin
                | LexemeType::InterpStringMid
                | LexemeType::InterpStringEnd
                | LexemeType::InterpStringSimple
                | LexemeType::BrokenInterpDoubleBrace
                | LexemeType::Number
                | LexemeType::Comment
                | LexemeType::BlockComment
        ));
        match self.data {
            LexemeData::Slice { length, .. } => length,
            _ => 0,
        }
    }

    /// Block comment/long string depth (`==` count in `[===[ ... ]===]`).
    ///
    /// Port of `Lexeme::getBlockDepth`; requires a reference to the source
    /// buffer since the depth is encoded by the closing delimiter that
    /// immediately follows the content slice.
    pub fn get_block_depth(&self, source: &[u8]) -> u32 {
        debug_assert!(self.ty == LexemeType::RawString || self.ty == LexemeType::BlockComment);

        let (start, length) = match self.data {
            LexemeData::Slice { start, length } => (start as usize, length as usize),
            _ => panic!("getBlockDepth requires slice data"),
        };

        // If we have a well-formed string, we are guaranteed to see 2 `]` characters
        // after the end of the string contents
        debug_assert_eq!(source[start + length], b']');

        let mut depth = 0;
        loop {
            depth += 1;
            if source[start + length + depth] == b']' {
                break;
            }
        }
        (depth - 1) as u32
    }

    /// Quote style of a quoted string lexeme.
    pub fn get_quote_style(&self, source: &[u8]) -> QuoteStyle {
        debug_assert!(self.ty == LexemeType::QuotedString);
        let (start, length) = match self.data {
            LexemeData::Slice { start, length } => (start as usize, length as usize),
            _ => panic!("getQuoteStyle requires slice data"),
        };
        match source[start + length] {
            b'\'' => QuoteStyle::Single,
            b'"' => QuoteStyle::Double,
            _ => unreachable!("unknown quote style"),
        }
    }

    /// Port of `Lexeme::toString`. Requires access to the source buffer to
    /// render slice lexemes.
    pub fn to_string(&self, source: &[u8]) -> String {
        match self.ty {
            LexemeType::Eof => "<eof>".to_string(),
            LexemeType::Equal => "'=='".to_string(),
            LexemeType::LessEqual => "'<='".to_string(),
            LexemeType::GreaterEqual => "'>='".to_string(),
            LexemeType::NotEqual => "'~='".to_string(),
            LexemeType::Dot2 => "'..'".to_string(),
            LexemeType::Dot3 => "'...'".to_string(),
            LexemeType::SkinnyArrow => "'->'".to_string(),
            LexemeType::DoubleColon => "'::'".to_string(),
            LexemeType::FloorDiv => "'//'".to_string(),
            LexemeType::AddAssign => "'+='".to_string(),
            LexemeType::SubAssign => "'-='".to_string(),
            LexemeType::MulAssign => "'*='".to_string(),
            LexemeType::DivAssign => "'/='".to_string(),
            LexemeType::FloorDivAssign => "'//='".to_string(),
            LexemeType::ModAssign => "'%='".to_string(),
            LexemeType::PowAssign => "'^='".to_string(),
            LexemeType::ConcatAssign => "'..='".to_string(),

            LexemeType::RawString | LexemeType::QuotedString => match self.data {
                LexemeData::Slice { .. } => {
                    let s = std::str::from_utf8(self.get_slice(source)).unwrap_or("");
                    format!("\"{s}\"")
                }
                _ => "string".to_string(),
            },

            LexemeType::InterpStringBegin => match self.data {
                LexemeData::Slice { .. } => {
                    let s = std::str::from_utf8(self.get_slice(source)).unwrap_or("");
                    format!("`{s}{{")
                }
                _ => "the beginning of an interpolated string".to_string(),
            },
            LexemeType::InterpStringMid => match self.data {
                LexemeData::Slice { .. } => {
                    let s = std::str::from_utf8(self.get_slice(source)).unwrap_or("");
                    format!("}}{s}{{")
                }
                _ => "the middle of an interpolated string".to_string(),
            },
            LexemeType::InterpStringEnd => match self.data {
                LexemeData::Slice { .. } => {
                    let s = std::str::from_utf8(self.get_slice(source)).unwrap_or("");
                    format!("}}{s}`")
                }
                _ => "the end of an interpolated string".to_string(),
            },
            LexemeType::InterpStringSimple => match self.data {
                LexemeData::Slice { .. } => {
                    let s = std::str::from_utf8(self.get_slice(source)).unwrap_or("");
                    format!("`{s}`")
                }
                _ => "interpolated string".to_string(),
            },

            LexemeType::Number => match self.data {
                LexemeData::Slice { .. } => {
                    let s = std::str::from_utf8(self.get_slice(source)).unwrap_or("");
                    format!("'{s}'")
                }
                _ => "number".to_string(),
            },

            LexemeType::Name => match self.data {
                LexemeData::Name(name) => {
                    if name.is_empty() {
                        "identifier".to_string()
                    } else {
                        format!("'{name}'")
                    }
                }
                _ => "identifier".to_string(),
            },

            LexemeType::Comment => "comment".to_string(),

            LexemeType::Attribute => match self.data {
                LexemeData::Name(name) => {
                    if name.is_empty() {
                        // C++ stores a non-null pointer to "" here and would print "''";
                        // we mirror that behavior for close parity.
                        "''".to_string()
                    } else {
                        format!("'{name}'")
                    }
                }
                _ => "attribute".to_string(),
            },

            LexemeType::AttributeOpen => "'@['".to_string(),

            LexemeType::BrokenString => "malformed string".to_string(),
            LexemeType::BrokenComment => "unfinished comment".to_string(),
            LexemeType::BrokenInterpDoubleBrace => {
                "'{{', which is invalid (did you mean '\\{'?)".to_string()
            }
            LexemeType::BrokenUnicode => match self.data {
                LexemeData::Codepoint(0) => "invalid UTF-8 sequence".to_string(),
                LexemeData::Codepoint(codepoint) => {
                    if let Some(confusable) = crate::confusables::find_confusable(codepoint) {
                        format!("Unicode character U+{codepoint:x} (did you mean '{confusable}'?)")
                    } else {
                        format!("Unicode character U+{codepoint:x}")
                    }
                }
                _ => "invalid UTF-8 sequence".to_string(),
            },

            LexemeType::Char(ch) => format!("'{}'", ch as char),

            LexemeType::Reserved(word) => format!("'{word}'"),

            _ => "<unknown>".to_string(),
        }
    }
}
