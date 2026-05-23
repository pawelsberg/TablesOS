//! Unlimited-magnitude integers.
//!
//! The spec promises integers, decimal mantissas and calendar years with **no
//! width limit**. Crucially, TablesOS performs *no arithmetic* on stored
//! values — there are no aggregates or computed columns. The engine only ever
//! needs to **parse**, **compare**, **format** and (for leap-year checks)
//! take a value **modulo a small constant**. So the representation is just the
//! decimal digits themselves: trivially correct, trivially unbounded.

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

/// A non-negative integer of unlimited size.
///
/// Invariant (canonical form): `digits` is most-significant-first, contains no
/// leading zeros, and is empty iff the value is zero.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct BigUint {
    digits: Vec<u8>, // each element 0..=9
}

impl BigUint {
    pub fn zero() -> Self {
        BigUint { digits: Vec::new() }
    }

    pub fn is_zero(&self) -> bool {
        self.digits.is_empty()
    }

    /// Parse a run of ASCII digits (no sign, no separators). Empty input and
    /// any non-digit byte are rejected.
    pub fn parse(s: &str) -> Option<BigUint> {
        if s.is_empty() {
            return None;
        }
        let mut digits = Vec::with_capacity(s.len());
        for b in s.bytes() {
            if !b.is_ascii_digit() {
                return None;
            }
            digits.push(b - b'0');
        }
        let mut v = BigUint { digits };
        v.normalize();
        Some(v)
    }

    /// Build from raw most-significant-first digits (each 0..=9). Used by the
    /// codec; non-digit bytes yield `None`.
    pub fn from_digits(digits: &[u8]) -> Option<BigUint> {
        if digits.iter().any(|d| *d > 9) {
            return None;
        }
        let mut v = BigUint {
            digits: digits.to_vec(),
        };
        v.normalize();
        Some(v)
    }

    /// Canonical digits, most-significant-first (empty for zero).
    pub fn digits(&self) -> &[u8] {
        &self.digits
    }

    fn normalize(&mut self) {
        let lead = self.digits.iter().take_while(|d| **d == 0).count();
        if lead > 0 {
            self.digits.drain(0..lead);
        }
    }

    pub fn to_dec_string(&self) -> String {
        if self.is_zero() {
            return String::from("0");
        }
        let mut s = String::with_capacity(self.digits.len());
        for d in &self.digits {
            s.push((b'0' + d) as char);
        }
        s
    }

    /// `self mod m` for a small modulus, via Horner's method over the decimal
    /// digits. Used only for Gregorian leap-year tests on the year.
    pub fn rem_u32(&self, m: u32) -> u32 {
        debug_assert!(m != 0);
        let mut r: u64 = 0;
        for d in &self.digits {
            r = (r * 10 + *d as u64) % m as u64;
        }
        r as u32
    }

    /// Number of decimal digits (0 has zero digits, matching the canonical
    /// form; callers that want "1 digit for zero" handle that themselves).
    pub fn digit_count(&self) -> usize {
        self.digits.len()
    }
}

impl PartialOrd for BigUint {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BigUint {
    fn cmp(&self, other: &Self) -> Ordering {
        // Canonical form ⇒ longer digit string is the larger number.
        match self.digits.len().cmp(&other.digits.len()) {
            Ordering::Equal => self.digits.cmp(&other.digits),
            ord => ord,
        }
    }
}

/// A signed integer of unlimited magnitude. Zero is always non-negative.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BigInt {
    negative: bool,
    mag: BigUint,
}

impl BigInt {
    pub fn zero() -> Self {
        BigInt {
            negative: false,
            mag: BigUint::zero(),
        }
    }

    pub fn from_parts(negative: bool, mag: BigUint) -> Self {
        // Never represent "negative zero".
        let negative = negative && !mag.is_zero();
        BigInt { negative, mag }
    }

    pub fn is_negative(&self) -> bool {
        self.negative
    }

    pub fn is_zero(&self) -> bool {
        self.mag.is_zero()
    }

    pub fn magnitude(&self) -> &BigUint {
        &self.mag
    }

    /// Parse `[+-]?digits`. A lone sign, empty string, or stray characters are
    /// rejected. `-0`, `+0`, `007` all canonicalise.
    pub fn parse(s: &str) -> Option<BigInt> {
        let (negative, rest) = match s.as_bytes().first() {
            Some(b'-') => (true, &s[1..]),
            Some(b'+') => (false, &s[1..]),
            _ => (false, s),
        };
        let mag = BigUint::parse(rest)?;
        Some(BigInt::from_parts(negative, mag))
    }

    pub fn to_dec_string(&self) -> String {
        let mut s = String::new();
        if self.negative {
            s.push('-');
        }
        s.push_str(&self.mag.to_dec_string());
        s
    }
}

impl PartialOrd for BigInt {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BigInt {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => self.mag.cmp(&other.mag),
            // Both negative: larger magnitude is the smaller number.
            (true, true) => other.mag.cmp(&self.mag),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format_roundtrip() {
        assert_eq!(BigUint::parse("0").unwrap().to_dec_string(), "0");
        assert_eq!(BigUint::parse("007").unwrap().to_dec_string(), "7");
        let huge = "9".repeat(500);
        assert_eq!(BigUint::parse(&huge).unwrap().to_dec_string(), huge);
        assert!(BigUint::parse("12a").is_none());
        assert!(BigUint::parse("").is_none());
    }

    #[test]
    fn unsigned_ordering() {
        let a = BigUint::parse("999999999999999999999999").unwrap();
        let b = BigUint::parse("1000000000000000000000000").unwrap();
        assert!(a < b);
        assert!(BigUint::parse("0").unwrap() < a);
        assert_eq!(
            BigUint::parse("42").unwrap(),
            BigUint::parse("0042").unwrap()
        );
    }

    #[test]
    fn signed_parse_and_order() {
        assert_eq!(BigInt::parse("-0").unwrap(), BigInt::zero());
        assert_eq!(BigInt::parse("+5").unwrap().to_dec_string(), "5");
        let neg_big = BigInt::parse(&format!("-{}", "9".repeat(100))).unwrap();
        let neg_small = BigInt::parse("-1").unwrap();
        assert!(neg_big < neg_small);
        assert!(neg_small < BigInt::zero());
        assert!(BigInt::zero() < BigInt::parse("1").unwrap());
        assert!(BigInt::parse("-1").unwrap() < BigInt::parse("1").unwrap());
        assert!(BigInt::parse("lol").is_none());
        assert!(BigInt::parse("-").is_none());
    }

    #[test]
    fn rem_small_modulus() {
        let y = BigUint::parse("2000").unwrap();
        assert_eq!(y.rem_u32(400), 0);
        assert_eq!(y.rem_u32(100), 0);
        assert_eq!(y.rem_u32(4), 0);
        let y = BigUint::parse("1900").unwrap();
        assert_eq!(y.rem_u32(400), 300);
        let y = BigUint::parse(&"123456789".repeat(10)).unwrap();
        // sanity: result is within range
        assert!(y.rem_u32(97) < 97);
    }
}
