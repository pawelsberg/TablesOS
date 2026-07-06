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

    /// `self + 1`, computed directly on the decimal digits (we keep no binary
    /// representation). Used only by the UI's "next unique value" shortcut —
    /// the engine itself still performs no arithmetic on stored values.
    pub fn succ(&self) -> BigUint {
        let mut digits = self.digits.clone(); // most-significant-first
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, 1); // carry rippled off the top (…9 -> 1 0…, or 0 -> 1)
                break;
            }
            i -= 1;
            if digits[i] == 9 {
                digits[i] = 0; // carry
            } else {
                digits[i] += 1;
                break;
            }
        }
        let mut v = BigUint { digits };
        v.normalize();
        v
    }

    /// `self - 1`, saturating at zero (`0.pred() == 0`). Companion to
    /// [`succ`](BigUint::succ); used to take the successor of a negative
    /// [`BigInt`] (whose magnitude *decreases* as the value increases).
    pub fn pred(&self) -> BigUint {
        let mut digits = self.digits.clone();
        let mut i = digits.len();
        loop {
            if i == 0 {
                break; // self was zero — saturate
            }
            i -= 1;
            if digits[i] == 0 {
                digits[i] = 9; // borrow
            } else {
                digits[i] -= 1;
                break;
            }
        }
        let mut v = BigUint { digits };
        v.normalize();
        v
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

    /// `self + 1`. For a non-negative value the magnitude grows by one; for a
    /// negative value it *shrinks* (`-3 -> -2`, `-1 -> 0`). `from_parts` drops
    /// any resulting "negative zero". Backs the UI's "next unique value"
    /// shortcut on signed-integer columns.
    pub fn succ(&self) -> BigInt {
        if self.negative {
            BigInt::from_parts(true, self.mag.pred())
        } else {
            BigInt::from_parts(false, self.mag.succ())
        }
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
    fn unsigned_succ_and_pred() {
        let s = |x: &str| BigUint::parse(x).unwrap().succ().to_dec_string();
        let p = |x: &str| BigUint::parse(x).unwrap().pred().to_dec_string();
        assert_eq!(s("0"), "1");
        assert_eq!(s("9"), "10");
        assert_eq!(s("199"), "200");
        assert_eq!(s(&"9".repeat(50)), format!("1{}", "0".repeat(50)));
        assert_eq!(p("1"), "0");
        assert_eq!(p("10"), "9");
        assert_eq!(p("200"), "199");
        assert_eq!(p("0"), "0"); // saturates
    }

    #[test]
    fn signed_succ() {
        let s = |x: &str| BigInt::parse(x).unwrap().succ().to_dec_string();
        assert_eq!(s("0"), "1");
        assert_eq!(s("41"), "42");
        assert_eq!(s("-1"), "0");
        assert_eq!(s("-3"), "-2");
        assert_eq!(s(&format!("-{}", "1".to_string() + &"0".repeat(20))), {
            // -100…0 + 1 = -99…9
            format!("-{}", "9".repeat(20))
        });
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
