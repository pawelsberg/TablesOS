//! Typed values. Every variant has unlimited size (the spec's central
//! promise) because the numeric parts are decimal-digit vectors with no
//! width cap.
//!
//! Design choice — *no arithmetic*: the engine never adds or converts values,
//! so we never need a calendar/day-number algorithm or decimal alignment for
//! sums. Ordering and equality (all that `UNIQUE` and foreign keys require)
//! are computed component-wise on canonical forms. For the zoned types this
//! means equality is *literal* (`12:00+00:00` ≠ `13:00+01:00`), not
//! instant-based; this is recorded in IMPLEMENTATION.md.

use crate::bignum::{BigInt, BigUint};
use crate::types::{perr, Type};
use crate::Result;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;

/// Signed decimal: sign + unlimited integer part + unlimited fractional part.
/// Canonical: no leading integer zeros, no trailing fractional zeros, and the
/// sign is never set when the value is zero.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Decimal {
    negative: bool,
    int: BigUint,
    frac: Vec<u8>, // fractional digits 0..=9, canonical (no trailing zeros)
}

impl Decimal {
    fn canonical(negative: bool, int: BigUint, mut frac: Vec<u8>) -> Decimal {
        while frac.last() == Some(&0) {
            frac.pop();
        }
        let negative = negative && !(int.is_zero() && frac.is_empty());
        Decimal {
            negative,
            int,
            frac,
        }
    }

    pub fn parse(s: &str) -> Option<Decimal> {
        let (negative, rest) = match s.as_bytes().first() {
            Some(b'-') => (true, &s[1..]),
            Some(b'+') => (false, &s[1..]),
            _ => (false, s),
        };
        if rest.is_empty() {
            return None;
        }
        let (int_part, frac_part) = match rest.split_once('.') {
            Some((i, f)) => (i, f),
            None => (rest, ""),
        };
        // Allow ".5" and "5." but not "" / "." / "-.".
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }
        let int = if int_part.is_empty() {
            BigUint::zero()
        } else {
            BigUint::parse(int_part)?
        };
        let mut frac = Vec::with_capacity(frac_part.len());
        for b in frac_part.bytes() {
            if !b.is_ascii_digit() {
                return None;
            }
            frac.push(b - b'0');
        }
        Some(Decimal::canonical(negative, int, frac))
    }

    pub fn display(&self) -> String {
        let mut s = String::new();
        if self.negative {
            s.push('-');
        }
        s.push_str(&self.int.to_dec_string());
        if !self.frac.is_empty() {
            s.push('.');
            for d in &self.frac {
                s.push((b'0' + d) as char);
            }
        }
        s
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => return Ordering::Greater,
            (true, false) => return Ordering::Less,
            _ => {}
        }
        let mag = self
            .int
            .cmp(&other.int)
            .then_with(|| cmp_frac(&self.frac, &other.frac));
        if self.negative {
            mag.reverse()
        } else {
            mag
        }
    }
}
impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn cmp_frac(a: &[u8], b: &[u8]) -> Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => {}
            ord => return ord,
        }
    }
    Ordering::Equal
}

/// A proleptic-Gregorian calendar date with an unlimited (signed) year.
/// Astronomical year numbering (a year 0 exists); leap rule is the standard
/// Gregorian one applied to the absolute year.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Date {
    pub year: BigInt,
    pub month: u8, // 1..=12
    pub day: u8,   // 1..=days_in_month
}

fn is_leap(year: &BigInt) -> bool {
    let y = year.magnitude();
    y.rem_u32(4) == 0 && (y.rem_u32(100) != 0 || y.rem_u32(400) == 0)
}

fn days_in_month(year: &BigInt, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

impl Date {
    pub fn parse(s: &str) -> Option<Date> {
        // Optional leading '-' for a negative (BCE-ish) year, then
        // <year-digits>-MM-DD. Year is everything before the *last two* dashes.
        let neg = s.starts_with('-');
        let body = if neg { &s[1..] } else { s };
        let mut parts = body.rsplitn(3, '-');
        let dd = parts.next()?;
        let mm = parts.next()?;
        let yy = parts.next()?;
        if yy.is_empty() {
            return None;
        }
        let year = BigInt::from_parts(neg, BigUint::parse(yy)?);
        let month = parse_fixed(mm, 2)? as u8;
        let day = parse_fixed(dd, 2)? as u8;
        if month < 1 || month > 12 {
            return None;
        }
        if day < 1 || day > days_in_month(&year, month) {
            return None;
        }
        Some(Date { year, month, day })
    }

    pub fn display(&self) -> String {
        let mut s = String::new();
        if self.year.is_negative() {
            s.push('-');
        }
        // ISO-style: pad the year to at least four digits.
        let yd = self.year.magnitude().to_dec_string();
        for _ in yd.len()..4 {
            s.push('0');
        }
        s.push_str(&yd);
        s.push('-');
        push2(&mut s, self.month);
        s.push('-');
        push2(&mut s, self.day);
        s
    }
}

impl Ord for Date {
    fn cmp(&self, other: &Self) -> Ordering {
        self.year
            .cmp(&other.year)
            .then(self.month.cmp(&other.month))
            .then(self.day.cmp(&other.day))
    }
}
impl PartialOrd for Date {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Time of day in `[00:00:00, 24:00:00)` with unlimited sub-second precision.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TimeOfDay {
    pub secs: u32,    // 0..=86399
    pub frac: Vec<u8>, // sub-second digits, canonical (no trailing zeros)
}

impl TimeOfDay {
    pub fn parse(s: &str) -> Option<TimeOfDay> {
        let (hms, frac_str) = match s.split_once('.') {
            Some((h, f)) => (h, Some(f)),
            None => (s, None),
        };
        let mut it = hms.split(':');
        let h = parse_fixed(it.next()?, 2)?;
        let m = parse_fixed(it.next()?, 2)?;
        let sec = parse_fixed(it.next()?, 2)?;
        if it.next().is_some() || h > 23 || m > 59 || sec > 59 {
            return None;
        }
        let mut frac = Vec::new();
        if let Some(f) = frac_str {
            if f.is_empty() {
                return None;
            }
            for b in f.bytes() {
                if !b.is_ascii_digit() {
                    return None;
                }
                frac.push(b - b'0');
            }
            while frac.last() == Some(&0) {
                frac.pop();
            }
        }
        Some(TimeOfDay {
            secs: h * 3600 + m * 60 + sec,
            frac,
        })
    }

    pub fn display(&self) -> String {
        let mut s = String::new();
        push2(&mut s, (self.secs / 3600) as u8);
        s.push(':');
        push2(&mut s, ((self.secs % 3600) / 60) as u8);
        s.push(':');
        push2(&mut s, (self.secs % 60) as u8);
        if !self.frac.is_empty() {
            s.push('.');
            for d in &self.frac {
                s.push((b'0' + d) as char);
            }
        }
        s
    }
}

impl Ord for TimeOfDay {
    fn cmp(&self, other: &Self) -> Ordering {
        self.secs
            .cmp(&other.secs)
            .then_with(|| cmp_frac(&self.frac, &other.frac))
    }
}
impl PartialOrd for TimeOfDay {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A zone offset in minutes, `-1439..=1439`. Stored and compared literally.
fn parse_offset(s: &str) -> Option<i16> {
    if s == "Z" || s == "z" {
        return Some(0);
    }
    let neg = match s.as_bytes().first()? {
        b'+' => false,
        b'-' => true,
        _ => return None,
    };
    let body = &s[1..];
    let (hh, mm) = body.split_once(':')?;
    let h = parse_fixed(hh, 2)? as i32;
    let m = parse_fixed(mm, 2)? as i32;
    if h > 23 || m > 59 {
        return None;
    }
    let total = h * 60 + m;
    Some(if neg { -total as i16 } else { total as i16 })
}

fn display_offset(off: i16, out: &mut String) {
    let (sign, a) = if off < 0 {
        ('-', (-(off as i32)) as u32)
    } else {
        ('+', off as u32)
    };
    out.push(sign);
    push2(out, (a / 60) as u8);
    out.push(':');
    push2(out, (a % 60) as u8);
}

/// Parse exactly `width` ASCII digits into a `u32`.
fn parse_fixed(s: &str, width: usize) -> Option<u32> {
    if s.len() != width || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut v = 0u32;
    for b in s.bytes() {
        v = v * 10 + (b - b'0') as u32;
    }
    Some(v)
}

fn push2(s: &mut String, v: u8) {
    s.push((b'0' + v / 10) as char);
    s.push((b'0' + v % 10) as char);
}

/// A single typed cell. `NULL` is *not* a `Value`; nullability is carried as
/// `Option<Value>` by rows and the codec.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    Integer(BigInt),
    Unsigned(BigUint),
    Decimal(Decimal),
    Str(String),
    Date(Date),
    DateTz(Date, i16),
    Time(TimeOfDay),
    DateTime(Date, TimeOfDay),
    DateTimeTz(Date, TimeOfDay, i16),
}

impl Value {
    pub fn type_of(&self) -> Type {
        match self {
            Value::Integer(_) => Type::Integer,
            Value::Unsigned(_) => Type::UnsignedInteger,
            Value::Decimal(_) => Type::Decimal,
            Value::Str(_) => Type::String,
            Value::Date(_) => Type::Date,
            Value::DateTz(..) => Type::DateTz,
            Value::Time(_) => Type::Time,
            Value::DateTime(..) => Type::DateTime,
            Value::DateTimeTz(..) => Type::DateTimeTz,
        }
    }

    /// Parse user input for a column of type `ty`. The `Err` message is shown
    /// inline next to the field per the UI spec.
    pub fn parse(ty: Type, input: &str) -> Result<Value> {
        let bad = |t: &str| perr(alloc::format!("not a valid {t}"));
        match ty {
            Type::Integer => BigInt::parse(input)
                .map(Value::Integer)
                .ok_or_else(|| bad("integer")),
            Type::UnsignedInteger => BigUint::parse(input)
                .map(Value::Unsigned)
                .ok_or_else(|| bad("unsigned integer")),
            Type::Decimal => Decimal::parse(input)
                .map(Value::Decimal)
                .ok_or_else(|| bad("decimal")),
            // Strings accept any UTF-8, including empty.
            Type::String => Ok(Value::Str(input.to_string())),
            Type::Date => Date::parse(input)
                .map(Value::Date)
                .ok_or_else(|| bad("date")),
            Type::DateTz => split_tz(input)
                .and_then(|(d, off)| Some(Value::DateTz(Date::parse(d)?, off)))
                .ok_or_else(|| bad("date tz")),
            Type::Time => TimeOfDay::parse(input)
                .map(Value::Time)
                .ok_or_else(|| bad("time")),
            Type::DateTime => split_dt(input)
                .and_then(|(d, t)| Some(Value::DateTime(Date::parse(d)?, TimeOfDay::parse(t)?)))
                .ok_or_else(|| bad("date time")),
            Type::DateTimeTz => split_tz(input)
                .and_then(|(dt, off)| {
                    let (d, t) = split_dt(dt)?;
                    Some(Value::DateTimeTz(Date::parse(d)?, TimeOfDay::parse(t)?, off))
                })
                .ok_or_else(|| bad("date time tz")),
        }
    }

    /// Canonical, lossless, human-readable form (also what Row View shows).
    pub fn display(&self) -> String {
        match self {
            Value::Integer(v) => v.to_dec_string(),
            Value::Unsigned(v) => v.to_dec_string(),
            Value::Decimal(v) => v.display(),
            Value::Str(s) => s.clone(),
            Value::Date(d) => d.display(),
            Value::DateTz(d, off) => {
                let mut s = d.display();
                display_offset(*off, &mut s);
                s
            }
            Value::Time(t) => t.display(),
            Value::DateTime(d, t) => {
                let mut s = d.display();
                s.push('T');
                s.push_str(&t.display());
                s
            }
            Value::DateTimeTz(d, t, off) => {
                let mut s = d.display();
                s.push('T');
                s.push_str(&t.display());
                display_offset(*off, &mut s);
                s
            }
        }
    }

    /// Type-ordinal used to give cross-type comparisons a total order. Within
    /// a single column all values share a type, so this is only a safety net.
    fn ord_tag(&self) -> u8 {
        self.type_of().tag()
    }
}

/// Split a trailing zone designator (`Z`, `+HH:MM`, `-HH:MM`) off the end.
fn split_tz(s: &str) -> Option<(&str, i16)> {
    if let Some(stripped) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        return Some((stripped, 0));
    }
    // Find the offset sign that starts the +HH:MM / -HH:MM suffix (6 chars).
    if s.len() >= 6 {
        let cut = s.len() - 6;
        let tail = &s[cut..];
        if (tail.starts_with('+') || tail.starts_with('-')) && tail.as_bytes()[3] == b':' {
            if let Some(off) = parse_offset(tail) {
                return Some((&s[..cut], off));
            }
        }
    }
    None
}

/// Split a date-time into its date and time halves (`T` or space separator).
fn split_dt(s: &str) -> Option<(&str, &str)> {
    s.split_once('T').or_else(|| s.split_once(' '))
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.ord_tag() != other.ord_tag() {
            return self.ord_tag().cmp(&other.ord_tag());
        }
        use Value::*;
        match (self, other) {
            (Integer(a), Integer(b)) => a.cmp(b),
            (Unsigned(a), Unsigned(b)) => a.cmp(b),
            (Decimal(a), Decimal(b)) => a.cmp(b),
            (Str(a), Str(b)) => a.cmp(b),
            (Date(a), Date(b)) => a.cmp(b),
            (DateTz(a, ao), DateTz(b, bo)) => a.cmp(b).then(ao.cmp(bo)),
            (Time(a), Time(b)) => a.cmp(b),
            (DateTime(da, ta), DateTime(db, tb)) => da.cmp(db).then(ta.cmp(tb)),
            (DateTimeTz(da, ta, oa), DateTimeTz(db, tb, ob)) => {
                da.cmp(db).then(ta.cmp(tb)).then(oa.cmp(ob))
            }
            // Unreachable: equal tags imply the same variant.
            _ => Ordering::Equal,
        }
    }
}
impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(t: Type, s: &str) -> Value {
        Value::parse(t, s).unwrap()
    }

    #[test]
    fn numeric_roundtrip_and_order() {
        assert_eq!(p(Type::Integer, "-007").display(), "-7");
        assert_eq!(p(Type::Decimal, "-3.1400").display(), "-3.14");
        assert_eq!(p(Type::Decimal, "5.").display(), "5");
        assert_eq!(p(Type::Decimal, ".50").display(), "0.5");
        assert!(p(Type::Decimal, "-0.0") == p(Type::Decimal, "0"));
        assert!(p(Type::Decimal, "-2.5") < p(Type::Decimal, "-2.49"));
        assert!(p(Type::Decimal, "10.0") > p(Type::Decimal, "9.99999"));
        assert!(Value::parse(Type::UnsignedInteger, "-1").is_err());
    }

    #[test]
    fn date_validation_and_leap() {
        assert!(Value::parse(Type::Date, "2024-02-29").is_ok());
        assert!(Value::parse(Type::Date, "2023-02-29").is_err());
        assert!(Value::parse(Type::Date, "2023-13-01").is_err());
        assert!(Value::parse(Type::Date, "2023-00-10").is_err());
        // Unlimited, signed year.
        let big = "1".to_string() + &"0".repeat(40) + "-01-01";
        assert!(Value::parse(Type::Date, &big).is_ok());
        assert_eq!(p(Type::Date, "-44-03-15").display(), "-0044-03-15");
        assert!(p(Type::Date, "0001-01-01") < p(Type::Date, "2024-01-01"));
        assert!(p(Type::Date, "-1-01-01") < p(Type::Date, "0001-01-01"));
    }

    #[test]
    fn time_and_zoned() {
        assert_eq!(p(Type::Time, "01:02:03.4500").display(), "01:02:03.45");
        assert!(Value::parse(Type::Time, "24:00:00").is_err());
        assert_eq!(
            p(Type::DateTimeTz, "2024-01-01T00:00:00Z").display(),
            "2024-01-01T00:00:00+00:00"
        );
        assert_eq!(
            p(Type::DateTz, "2024-06-01-05:30").display(),
            "2024-06-01-05:30"
        );
        // Literal (not instant) equality, as documented.
        assert!(
            p(Type::DateTimeTz, "2024-01-01T12:00:00+00:00")
                != p(Type::DateTimeTz, "2024-01-01T13:00:00+01:00")
        );
    }
}
