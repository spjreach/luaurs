//! Port of `Luau/Location.h` / `Ast/src/Location.cpp`.

/// A position in the source: line and column (both 0-based, matching C++).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

impl Position {
    #[inline]
    pub fn new(line: u32, column: u32) -> Self {
        Position { line, column }
    }

    #[inline]
    pub fn missing() -> Self {
        Position { line: u32::MAX, column: u32::MAX }
    }

    #[inline]
    pub fn has_value(&self) -> bool {
        self.line != u32::MAX || self.column != u32::MAX
    }

    /// Port of `Position::shift`.
    pub fn shift(&mut self, start: Position, old_end: Position, new_end: Position) {
        if *self >= start {
            if self.line > old_end.line {
                self.line += new_end.line - old_end.line;
            } else {
                self.line = new_end.line;
                self.column += new_end.column - old_end.column;
            }
        }
    }
}

impl Default for Position {
    #[inline]
    fn default() -> Self {
        Position { line: 0, column: 0 }
    }
}

/// A half-open [begin, end) location range in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Location {
    pub begin: Position,
    pub end: Position,
}

impl Location {
    #[inline]
    pub fn new(begin: Position, end: Position) -> Self {
        Location { begin, end }
    }

    /// Location spanning `begin` for `length` columns on the same line.
    #[inline]
    pub fn on_line(begin: Position, length: u32) -> Self {
        Location { begin, end: Position::new(begin.line, begin.column + length) }
    }

    /// Location spanning from `begin.begin` to `end.end`.
    #[inline]
    pub fn spanning(begin: Location, end: Location) -> Self {
        Location { begin: begin.begin, end: end.end }
    }

    #[inline]
    pub fn point(p: Position) -> Self {
        Location { begin: p, end: p }
    }

    pub fn encloses(&self, l: Location) -> bool {
        self.begin <= l.begin && self.end >= l.end
    }

    pub fn overlaps(&self, l: Location) -> bool {
        (self.begin <= l.begin && self.end >= l.begin)
            || (self.begin <= l.end && self.end >= l.end)
            || (self.begin >= l.begin && self.end <= l.end)
    }

    pub fn contains(&self, p: Position) -> bool {
        self.begin <= p && p < self.end
    }

    pub fn contains_closed(&self, p: Position) -> bool {
        self.begin <= p && p <= self.end
    }

    pub fn extend(&mut self, other: Location) {
        if other.begin < self.begin {
            self.begin = other.begin;
        }
        if other.end > self.end {
            self.end = other.end;
        }
    }

    pub fn shift(&mut self, start: Position, old_end: Position, new_end: Position) {
        self.begin.shift(start, old_end, new_end);
        self.end.shift(start, old_end, new_end);
    }
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.begin.line, self.begin.column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: u32, col: u32) -> Position {
        Position::new(line, col)
    }

    #[test]
    fn ordering() {
        assert!(p(0, 5) < p(0, 10));
        assert!(p(0, 5) < p(1, 0));
        assert!(p(1, 0) == p(1, 0));
    }

    #[test]
    fn encloses_overlaps() {
        let a = Location::new(p(0, 0), p(0, 10));
        let b = Location::new(p(0, 3), p(0, 5));
        assert!(a.encloses(b));
        assert!(!b.encloses(a));
        assert!(a.overlaps(b));

        let c = Location::new(p(0, 20), p(0, 25));
        assert!(!a.overlaps(c));
        assert!(!a.encloses(c));
    }

    #[test]
    fn contains() {
        let a = Location::new(p(0, 0), p(0, 10));
        assert!(a.contains(p(0, 3)));
        assert!(!a.contains(p(0, 10)));
        assert!(a.contains_closed(p(0, 10)));
    }

    #[test]
    fn extend() {
        let mut a = Location::new(p(0, 0), p(0, 10));
        a.extend(Location::new(p(1, 2), p(1, 5)));
        assert_eq!(a.begin, p(0, 0));
        assert_eq!(a.end, p(1, 5));
    }

    #[test]
    fn shift_position() {
        // replacing [2..5) with [2..9): positions after 2 shift right by 4
        let mut pos = Position::new(0, 7);
        pos.shift(Position::new(0, 2), Position::new(0, 5), Position::new(0, 9));
        assert_eq!(pos, Position::new(0, 11));

        let mut pos2 = Position::new(0, 4);
        pos2.shift(Position::new(0, 2), Position::new(0, 5), Position::new(0, 9));
        assert_eq!(pos2, Position::new(0, 8));

        let mut pos3 = Position::new(0, 1);
        pos3.shift(Position::new(0, 2), Position::new(0, 5), Position::new(0, 9));
        assert_eq!(pos3, Position::new(0, 1));
    }
}
