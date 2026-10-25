use std::fmt;

use crate::NetError;
use crate::error::protocol;

/// Longest accepted UID-set expression, in bytes.
const MAX_SET_BYTES: usize = 512;
/// Largest number of ranges in one set. A longer set cannot fit the command bound anyway.
const MAX_RANGES: usize = 256;

/// One end of a UID range: a concrete UID or the highest UID on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bound {
    Uid(u32),
    Star,
}

impl fmt::Display for Bound {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uid(uid) => write!(formatter, "{uid}"),
            Self::Star => formatter.write_str("*"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    start: Bound,
    end: Bound,
}

impl fmt::Display for Range {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.start, self.end) {
            (Bound::Uid(start), Bound::Uid(end)) if start == end => write!(formatter, "{start}"),
            (Bound::Star, Bound::Star) => formatter.write_str("*"),
            (start, end) => write!(formatter, "{start}:{end}"),
        }
    }
}

/// A validated IMAP sequence set for UIDs: `42`, `1:*,100:200`.
///
/// Ranges are kept as written, never expanded: `1:*` on a 500 000-message mailbox is one range, not
/// half a million numbers. Empty sets, zero, reversed ranges and `*` as a start are rejected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UidSet {
    ranges: Vec<Range>,
}

impl UidSet {
    /// Parse a wire sequence set.
    ///
    /// Accepts `n`, `*`, `n:*`, `n:m` and comma-separated combinations. Rejects empty input, `0`,
    /// reversed ranges (`5:3`), `*:n`, negative numbers, non-digits and more than
    /// [`MAX_RANGES`] ranges.
    pub fn parse(spec: &str) -> Result<Self, NetError> {
        if spec.is_empty() {
            return Err(protocol("UID set is empty"));
        }
        if spec.len() > MAX_SET_BYTES {
            return Err(NetError::TooLarge { what: "UID set", limit: MAX_SET_BYTES });
        }
        let mut ranges = Vec::new();
        for part in spec.split(',') {
            if ranges.len() >= MAX_RANGES {
                return Err(NetError::TooLarge { what: "UID ranges", limit: MAX_RANGES });
            }
            ranges.push(parse_range(part)?);
        }
        Ok(Self { ranges })
    }

    /// A set with one UID.
    pub fn single(uid: u32) -> Result<Self, NetError> {
        if uid == 0 {
            return Err(protocol("UID 0 does not exist"));
        }
        Ok(Self { ranges: vec![Range { start: Bound::Uid(uid), end: Bound::Uid(uid) }] })
    }

    /// A set with one inclusive range.
    pub fn range(from: u32, to: u32) -> Result<Self, NetError> {
        if from == 0 || to == 0 {
            return Err(protocol("UID 0 does not exist"));
        }
        if from > to {
            return Err(protocol("UID range is reversed"));
        }
        Ok(Self { ranges: vec![Range { start: Bound::Uid(from), end: Bound::Uid(to) }] })
    }

    /// Whether the set has no ranges and therefore cannot be sent.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Number of ranges, not of UIDs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// The wire form, or a typed error when the set is empty.
    pub(crate) fn to_wire(&self) -> Result<String, NetError> {
        if self.ranges.is_empty() {
            return Err(protocol("UID set is empty"));
        }
        Ok(self.to_string())
    }
}

impl fmt::Display for UidSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, range) in self.ranges.iter().enumerate() {
            if index > 0 {
                formatter.write_str(",")?;
            }
            write!(formatter, "{range}")?;
        }
        Ok(())
    }
}

fn parse_range(part: &str) -> Result<Range, NetError> {
    if part.is_empty() {
        return Err(protocol("UID set has an empty element"));
    }
    let mut pieces = part.split(':');
    let first = pieces.next().ok_or_else(|| protocol("UID set has an empty element"))?;
    let start = parse_bound(first)?;
    let Some(second) = pieces.next() else {
        return Ok(Range { start, end: start });
    };
    if pieces.next().is_some() {
        return Err(protocol("UID range has more than two ends"));
    }
    let end = parse_bound(second)?;
    if start == Bound::Star {
        return Err(protocol("a UID range cannot start at *"));
    }
    if let (Bound::Uid(start), Bound::Uid(end)) = (start, end)
        && start > end
    {
        return Err(protocol("UID range is reversed"));
    }
    Ok(Range { start, end })
}

fn parse_bound(text: &str) -> Result<Bound, NetError> {
    if text == "*" {
        return Ok(Bound::Star);
    }
    if text.is_empty() || text.len() > 10 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(protocol("UID must be a positive number or *"));
    }
    let uid = text.parse::<u32>().map_err(|_| protocol("UID is out of range"))?;
    if uid == 0 {
        return Err(protocol("UID 0 does not exist"));
    }
    Ok(Bound::Uid(uid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_grammar() {
        for spec in ["1", "1:2", "1:*", "*", "42,100:200", "1,3:5,9"] {
            let set = UidSet::parse(spec).unwrap_or_else(|error| panic!("{spec}: {error}"));
            assert_eq!(set.to_string(), spec);
        }
        assert_eq!(UidSet::parse("1:1").map(|set| set.to_string()).as_deref(), Ok("1"));
    }

    #[test]
    fn rejects_invalid_sets() {
        let cases = ["", "0", "0:5", "5:3", "5:0", "*:5", "*:*", "-1", "1:-2", "a", "1,,2", "1,", ",1", "1:", ":2", "1:2:3", "1 2", "4294967296"];
        for case in cases {
            assert!(UidSet::parse(case).is_err(), "{case:?} should be refused");
        }
    }

    #[test]
    fn builds_single_and_range() {
        assert_eq!(UidSet::single(7).map(|set| set.to_string()).as_deref(), Ok("7"));
        assert_eq!(UidSet::range(3, 9).map(|set| set.to_string()).as_deref(), Ok("3:9"));
        assert!(UidSet::single(0).is_err());
        assert!(UidSet::range(9, 3).is_err());
    }

    #[test]
    fn empty_set_cannot_be_sent() {
        let set = UidSet::default();
        assert!(set.is_empty());
        assert!(set.to_wire().is_err());
    }

    #[test]
    fn bounds_the_expression() {
        let long = "1,".repeat(MAX_SET_BYTES);
        assert!(matches!(UidSet::parse(&long), Err(NetError::TooLarge { .. })));
        let many = (1..=MAX_RANGES + 1).map(|n| n.to_string()).collect::<Vec<_>>().join(",");
        assert!(matches!(UidSet::parse(&many), Err(NetError::TooLarge { .. })));
    }
}
