//! Avro values as text and back: the forms XML Schema gives the same values (`xs:date`,
//! `xs:time`, `xs:dateTime`, `xs:decimal`, `xs:duration`, `xs:base64Binary`, `xs:double`), so
//! text written by one tool reads back in the other and in any XML or JSON consumer.
//!
//! Readers are lenient where it costs nothing (time-zone offsets, `+` signs, exponents in
//! decimals, whitespace in base64); writers always produce the one canonical form.

use base64::Engine as _;

use crate::binary::{read_long_at, write_bytes, write_long};

/// Most digits a decimal may have, either way: longer ones would take quadratic time to convert.
pub const MAX_DECIMAL_DIGITS: usize = 10_000;

/// The unit of a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Millis,
    Micros,
    Nanos,
}

impl Unit {
    pub fn per_second(self) -> i64 {
        match self {
            Unit::Millis => 1_000,
            Unit::Micros => 1_000_000,
            Unit::Nanos => 1_000_000_000,
        }
    }

    /// Fraction digits the unit can hold.
    pub fn digits(self) -> usize {
        match self {
            Unit::Millis => 3,
            Unit::Micros => 6,
            Unit::Nanos => 9,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Numbers

/// An integer: digits with an optional sign, or a decimal with a zero fraction (`12.0`).
pub fn parse_integer(t: &str) -> Option<i64> {
    // Rust's parse takes one `+` or `-` itself.
    if let Ok(v) = t.parse::<i64>() {
        return Some(v);
    }
    let (int, frac) = t.split_once('.')?;
    (frac.chars().all(|c| c == '0') && !int.is_empty()).then(|| int.parse().ok()).flatten()
}

/// A float or double as XML Schema or JSON writes it, including `INF`, `-INF`, `NaN` and the
/// JSON encoding's `Infinity`.
pub fn parse_float(t: &str) -> Option<f64> {
    match t {
        "INF" | "+INF" | "Infinity" | "inf" => Some(f64::INFINITY),
        "-INF" | "-Infinity" | "-inf" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        t if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E')) => t.parse().ok(),
        _ => None,
    }
}

/// A float or double as XML Schema writes it. `display` is Rust's shortest form of the value
/// (`v.to_string()`, or `(v as f32).to_string()` for a float, whose digits are fewer).
pub fn float_text(v: f64, display: String) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "INF".into() } else { "-INF".into() };
    }
    let a = v.abs();
    if a != 0.0 && !(1e-6..1e21).contains(&a) {
        // An exponent where the plain form gets long, keeping the shortest digits.
        let plain = display.trim_start_matches('-');
        let (int, frac) = plain.split_once('.').unwrap_or((plain, ""));
        // The exponent comes from the same digits (log10 rounds up near powers of ten).
        let int_digits = int.trim_start_matches('0');
        let exp = if int_digits.is_empty() { -((frac.len() - frac.trim_start_matches('0').len()) as i64 + 1) } else { int_digits.len() as i64 - 1 };
        let digits = format!("{int}{frac}");
        let digits = digits.trim_start_matches('0').trim_end_matches('0');
        let (first, rest) = digits.split_at(1.min(digits.len()));
        let sign = if v < 0.0 { "-" } else { "" };
        return if rest.is_empty() { format!("{sign}{first}E{exp}") } else { format!("{sign}{first}.{rest}E{exp}") };
    }
    display
}

// ---------------------------------------------------------------------------------------------
// Bytes

pub fn base64_text(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// Base64, whitespace ignored.
pub fn parse_base64(t: &str) -> Option<Vec<u8>> {
    let clean: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(clean).ok()
}

/// Bytes as Avro's JSON encoding writes them: a string with one character (U+0000 to U+00FF) per
/// byte.
pub fn json_bytes_text(b: &[u8]) -> String {
    b.iter().map(|&x| char::from(x)).collect()
}

/// The bytes of a JSON-encoding string, if every character is U+0000 to U+00FF.
pub fn parse_json_bytes(t: &str) -> Option<Vec<u8>> {
    t.chars().map(|c| u8::try_from(u32::from(c)).ok()).collect()
}

// ---------------------------------------------------------------------------------------------
// Dates and times

/// Days since 1970-01-01 of a proleptic Gregorian date (year 0 is 1 BCE), for any year.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The year, month and day of a number of days since 1970-01-01, for any number.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    // In 128 bits: the shift to 0000-03-01 would overflow near i64's ends.
    let z = i128::from(z) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i128::from(m <= 2);
    (y as i64, m as i64, d as i64)
}

/// A date (days since 1970-01-01) as `xs:date`: at least four year digits, a minus sign before
/// years before 1 CE.
pub fn date_text(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    let year = if y < 0 { format!("-{:04}", y.unsigned_abs()) } else { format!("{y:04}") };
    format!("{year}-{m:02}-{d:02}")
}

/// `.` and the first `digits` of a fraction of a second, or nothing for none.
fn fraction(nanos: i64, digits: usize) -> String {
    if nanos == 0 {
        return String::new();
    }
    let s = format!("{nanos:09}");
    format!(".{}", &s[..digits.min(9)])
}

/// A time of day (nanoseconds after midnight) as `xs:time`, with up to `digits` fraction digits.
pub fn time_text(nanos: i64, digits: usize) -> Result<String, String> {
    if !(0..86_400_000_000_000).contains(&nanos) {
        return Err(format!("time {nanos} ns after midnight is out of range"));
    }
    let secs = nanos / 1_000_000_000;
    Ok(format!("{:02}:{:02}:{:02}{}", secs / 3600, secs / 60 % 60, secs % 60, fraction(nanos % 1_000_000_000, digits)))
}

/// A timestamp (`v` units since the epoch) as `xs:dateTime`: UTC with `Z` for an instant, no
/// time zone for a local timestamp.
pub fn timestamp_text(v: i64, u: Unit, utc: bool) -> String {
    let per = u.per_second();
    let (secs, sub) = (v.div_euclid(per), v.rem_euclid(per));
    let nanos = sub * (1_000_000_000 / per);
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    format!("{}T{:02}:{:02}:{:02}{}{}", date_text(days), sod / 3600, sod / 60 % 60, sod % 60, fraction(nanos, u.digits()), if utc { "Z" } else { "" })
}

/// Text without a trailing time zone (`Z`, `+01:00`).
fn strip_zone(t: &str) -> &str {
    if let Some(s) = t.strip_suffix('Z') {
        return s;
    }
    let b = t.as_bytes();
    if b.len() > 6 && matches!(b[b.len() - 6], b'+' | b'-') && b[b.len() - 3] == b':' {
        return &t[..t.len() - 6];
    }
    t
}

/// Days since 1970-01-01 of `[-]YYYY-MM-DD` (four or more year digits).
fn civil_days(t: &str) -> Option<i64> {
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t),
    };
    let mut parts = t.splitn(3, '-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    let digits = |x: &str, n: std::ops::RangeInclusive<usize>| n.contains(&x.len()) && x.bytes().all(|c| c.is_ascii_digit());
    if !digits(y, 4..=12) || !digits(m, 2..=2) || !digits(d, 2..=2) {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    let y = if neg { -y } else { y };
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&m) || d < 1 || d > month_days[(m - 1) as usize] {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Days since 1970-01-01 of an `xs:date` (or the date of an `xs:dateTime`).
pub fn parse_date(t: &str) -> Option<i64> {
    let d = t.trim().split(['T', ' ']).next()?;
    civil_days(if d.len() > 10 { strip_zone(d) } else { d })
}

/// Hours, minutes, optional seconds and fraction (`hh:mm`, `hh:mm:ss`, `hh:mm:ss.fff`) as seconds
/// and nanoseconds. A leap second (`:60`) reads as the last instant of the minute before.
fn clock(time: &str, max_hour: i64) -> Option<(i64, i64)> {
    let (hms, frac) = match time.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (time, None),
    };
    let two = |x: &str| (x.len() == 2 && x.bytes().all(|c| c.is_ascii_digit())).then(|| x.parse::<i64>().ok()).flatten();
    let p: Vec<&str> = hms.split(':').collect();
    let (h, m, s) = match p.as_slice() {
        [h, m] if frac.is_none() => (two(h)?, two(m)?, 0),
        [h, m, s] => (two(h)?, two(m)?, two(s)?),
        _ => return None,
    };
    let frac = frac.unwrap_or("");
    if h > max_hour || m > 59 || s > 60 || !frac.bytes().all(|c| c.is_ascii_digit()) || time.ends_with('.') {
        return None;
    }
    let digits: String = frac.chars().take(9).collect();
    let nanos: i64 = if digits.is_empty() { 0 } else { format!("{digits:0<9}").parse().ok()? };
    // 24:00:00 is the end of the day, and nothing after it.
    if h == 24 && (m, s, frac.bytes().any(|c| c != b'0')) != (0, 0, false) {
        return None;
    }
    if s == 60 {
        return Some((h * 3600 + m * 60 + 59, 999_999_999));
    }
    Some((h * 3600 + m * 60 + s, nanos))
}

/// Nanoseconds after midnight of an `xs:time` (a time zone is ignored).
pub fn parse_time(t: &str) -> Option<i64> {
    let (secs, nanos) = clock(strip_zone(t.trim()), 23)?;
    Some(secs * 1_000_000_000 + nanos)
}

/// Units since the epoch of an `xs:dateTime` (or `xs:date`). An instant (`utc`) converts a time
/// zone offset to UTC and reads a time without one as UTC; a local timestamp keeps the wall-clock
/// time as written.
pub fn parse_timestamp(t: &str, u: Unit, utc: bool) -> Option<i64> {
    let t = t.trim();
    // The zone, `Z` or `±hh:mm`, in seconds east of UTC.
    let (body, offset) = match t.strip_suffix('Z') {
        Some(b) => (b, 0),
        None => {
            let b = t.as_bytes();
            match t.len().checked_sub(6) {
                // `k >= 10`: after a whole date, which may be all there is (`2025-11-08+01:00`).
                Some(k) if k >= 10 && matches!(b[k], b'+' | b'-') && b[k + 3] == b':' => {
                    let two = |x: &str| (x.len() == 2 && x.bytes().all(|c| c.is_ascii_digit())).then(|| x.parse::<i64>().ok()).flatten();
                    let (h, m) = (two(&t[k + 1..k + 3])?, two(&t[k + 4..])?);
                    if m > 59 || (h, m) > (14, 0) {
                        return None;
                    }
                    let off = (h * 60 + m) * 60;
                    (&t[..k], if b[k] == b'-' { -off } else { off })
                }
                _ => (t, 0),
            }
        }
    };
    let (date, time) = match body.find(['T', ' ']) {
        Some(k) if k >= 10 => (&body[..k], Some(&body[k + 1..])),
        _ => (body, None),
    };
    let days = civil_days(date)?;
    let (secs, nanos) = match time {
        Some(time) => clock(time, 24)?,
        None => (0, 0),
    };
    let secs = i128::from(days) * 86_400 + i128::from(secs) - if utc { i128::from(offset) } else { 0 };
    let per = i128::from(u.per_second());
    let units = secs * per + i128::from(nanos) * per / 1_000_000_000;
    i64::try_from(units).ok()
}

// ---------------------------------------------------------------------------------------------
// UUIDs and durations

/// 16 bytes in the 8-4-4-4-12 hex form.
pub fn uuid_text(b: &[u8]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    if h.len() != 32 {
        return h;
    }
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// A UUID's 16 bytes, from 32 hex digits with or without the dashes.
pub fn parse_uuid(t: &str) -> Option<[u8; 16]> {
    hex16(&t.trim().chars().filter(|c| *c != '-').collect::<String>())
}

/// 16 bytes from exactly 32 hex digits. (`u8::from_str_radix` alone would take `+f`.)
pub(crate) fn hex16(h: &str) -> Option<[u8; 16]> {
    if h.len() != 32 || !h.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for (k, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&h[2 * k..2 * k + 2], 16).ok()?;
    }
    Some(out)
}

/// A duration (12 bytes: months, days and milliseconds, little-endian) as `xs:duration`.
pub fn duration_text(b: &[u8; 12]) -> String {
    let n = |k: usize| u32::from_le_bytes(b[4 * k..4 * k + 4].try_into().expect("4 bytes"));
    let (months, days, millis) = (n(0), n(1), n(2));
    let mut s = String::from("P");
    if months > 0 {
        s += &format!("{months}M");
    }
    if days > 0 {
        s += &format!("{days}D");
    }
    if millis > 0 {
        let frac = millis % 1000;
        let secs = millis / 1000;
        s += &if frac == 0 { format!("T{secs}S") } else { format!("T{secs}.{}S", format!("{frac:03}").trim_end_matches('0')) };
    }
    if s == "P" { "PT0S".into() } else { s }
}

/// Fraction digits kept of an hour, minute or second count: at most 3.6e-12 ms is dropped, too
/// little to move the rounding to whole milliseconds.
const DURATION_DIGITS: u32 = 18;

/// `num` (digits, perhaps with a fraction) times `unit` milliseconds, exactly, in units of
/// 10^-[`DURATION_DIGITS`] ms. `None` for what isn't a number; `Some(None)` for too large.
fn scaled_millis(num: &str, unit: u128) -> Option<Option<u128>> {
    let (int, frac) = num.split_once('.').unwrap_or((num, ""));
    if (int.is_empty() && frac.is_empty()) || !int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let frac: String = frac.chars().take(DURATION_DIGITS as usize).collect();
    let scale = 10u128.pow(DURATION_DIGITS);
    let int = int.trim_start_matches('0');
    // Anything with more digits than this is far past u32::MAX milliseconds.
    if int.len() > 20 {
        return Some(None);
    }
    let int: u128 = if int.is_empty() { 0 } else { int.parse().ok()? };
    let frac_value: u128 = if frac.is_empty() { 0 } else { frac.parse::<u128>().ok()? * 10u128.pow(DURATION_DIGITS - frac.len() as u32) };
    Some(int.checked_mul(scale).and_then(|i| i.checked_add(frac_value)).and_then(|v| v.checked_mul(unit)))
}

/// The 12 bytes of an `xs:duration`. Years become months and weeks days; hours, minutes and
/// seconds become milliseconds, rounded half away from zero. The error finishes a sentence about
/// the text ("is negative...").
pub fn parse_duration(t: &str) -> Result<[u8; 12], String> {
    let t = t.trim();
    if t.starts_with('-') {
        return Err("is negative; an Avro duration cannot be".into());
    }
    let body = t.strip_prefix('P').ok_or("is not a duration")?;
    let (date, time) = body.split_once('T').unwrap_or((body, ""));
    // A `T` needs a time after it (`P1DT` is not a duration).
    if body.contains('T') && time.is_empty() {
        return Err("is not a duration".into());
    }
    let too_long = "is too long for an Avro duration";
    let (mut months, mut days, mut scaled) = (0u64, 0u64, 0u128);
    let mut num = String::new();
    for c in date.chars() {
        match c {
            '0'..='9' => num.push(c),
            'Y' | 'M' | 'W' | 'D' => {
                let v: u64 = num.parse().map_err(|_| if num.is_empty() { "is not a duration" } else { too_long })?;
                num.clear();
                let (total, add) = match c {
                    'Y' => (&mut months, v.checked_mul(12)),
                    'M' => (&mut months, Some(v)),
                    'W' => (&mut days, v.checked_mul(7)),
                    _ => (&mut days, Some(v)),
                };
                *total = add.and_then(|a| total.checked_add(a)).ok_or(too_long)?;
            }
            _ => return Err("is not a duration".into()),
        }
    }
    for c in time.chars() {
        match c {
            '0'..='9' | '.' => num.push(c),
            'H' | 'M' | 'S' => {
                let unit = match c {
                    'H' => 3_600_000,
                    'M' => 60_000,
                    _ => 1000,
                };
                let v = scaled_millis(&num, unit).ok_or("is not a duration")?.ok_or(too_long)?;
                num.clear();
                scaled = scaled.checked_add(v).ok_or(too_long)?;
            }
            _ => return Err("is not a duration".into()),
        }
    }
    if !num.is_empty() || (date.is_empty() && time.is_empty()) {
        return Err("is not a duration".into());
    }
    let half = 10u128.pow(DURATION_DIGITS) / 2;
    let millis = scaled.checked_add(half).ok_or(too_long)? / 10u128.pow(DURATION_DIGITS);
    if months > u64::from(u32::MAX) || days > u64::from(u32::MAX) || millis > u128::from(u32::MAX) {
        return Err(too_long.into());
    }
    let mut out = [0u8; 12];
    out[..4].copy_from_slice(&(months as u32).to_le_bytes());
    out[4..8].copy_from_slice(&(days as u32).to_le_bytes());
    out[8..].copy_from_slice(&(millis as u32).to_le_bytes());
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Decimals: two's complement big-endian unscaled integers of any length

/// Decimal digits of a big-endian unsigned magnitude.
fn magnitude_digits(mag: &[u8]) -> String {
    let mut m: Vec<u8> = mag.iter().copied().skip_while(|b| *b == 0).collect();
    let mut digits = Vec::new();
    while !m.is_empty() {
        let mut rem = 0u32;
        for b in m.iter_mut() {
            let cur = rem * 256 + u32::from(*b);
            *b = (cur / 10) as u8;
            rem = cur % 10;
        }
        digits.push(b'0' + rem as u8);
        let lead = m.iter().take_while(|b| **b == 0).count();
        m.drain(..lead);
    }
    if digits.is_empty() {
        digits.push(b'0');
    }
    digits.reverse();
    String::from_utf8(digits).expect("ASCII digits")
}

/// The big-endian unsigned magnitude of decimal digits.
fn digits_magnitude(digits: &str) -> Vec<u8> {
    let mut mag: Vec<u8> = Vec::new();
    for d in digits.bytes() {
        let mut carry = u32::from(d - b'0');
        for b in mag.iter_mut().rev() {
            let v = u32::from(*b) * 10 + carry;
            *b = v as u8;
            carry = v >> 8;
        }
        while carry > 0 {
            mag.insert(0, carry as u8);
            carry >>= 8;
        }
    }
    mag
}

fn negate(b: &mut [u8]) {
    let mut carry = true;
    for x in b.iter_mut().rev() {
        *x = !*x;
        if carry {
            let (v, c) = x.overflowing_add(1);
            *x = v;
            carry = c;
        }
    }
}

fn add_one(mag: &[u8]) -> Vec<u8> {
    let mut m = mag.to_vec();
    for b in m.iter_mut().rev() {
        let (v, c) = b.overflowing_add(1);
        *b = v;
        if !c {
            return m;
        }
    }
    m.insert(0, 1);
    m
}

/// A decimal's unscaled value (two's complement, big-endian) as text with `scale` fraction
/// digits. A negative scale (as in big-decimal's `1E+3`) appends zeros.
pub fn decimal_text(b: &[u8], scale: i64) -> Result<String, String> {
    // 2.41 digits per byte
    if b.len() > MAX_DECIMAL_DIGITS * 10 / 24 || scale.unsigned_abs() > MAX_DECIMAL_DIGITS as u64 {
        return Err(format!("a decimal has more than {MAX_DECIMAL_DIGITS} digits"));
    }
    let negative = b.first().is_some_and(|x| x & 0x80 != 0);
    let mut mag = b.to_vec();
    if negative {
        negate(&mut mag);
    }
    let mut digits = magnitude_digits(&mag);
    let body = if scale <= 0 {
        if digits != "0" {
            digits.push_str(&"0".repeat(scale.unsigned_abs() as usize));
        }
        digits
    } else {
        let scale = scale as usize;
        if digits.len() <= scale {
            digits.insert_str(0, &"0".repeat(scale + 1 - digits.len()));
        }
        let (int, frac) = digits.split_at(digits.len() - scale);
        format!("{int}.{frac}")
    };
    Ok(if negative && body.bytes().any(|c| c.is_ascii_digit() && c != b'0') { format!("-{body}") } else { body })
}

/// Signed decimal digits (`-1234`) as the shortest two's complement big-endian bytes.
fn to_twos_complement(signed_digits: &str) -> Vec<u8> {
    let (negative, digits) = match signed_digits.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, signed_digits),
    };
    let mut b = digits_magnitude(digits);
    b.insert(0, 0);
    if negative {
        negate(&mut b);
        while b.len() > 1 && b[0] == 0xff && b[1] & 0x80 != 0 {
            b.remove(0);
        }
    } else {
        while b.len() > 1 && b[0] == 0 && b[1] & 0x80 == 0 {
            b.remove(0);
        }
    }
    b
}

/// The signed unscaled digits (`-1234`) and scale of decimal text (`-12.34`, `1.5E3`).
pub fn parse_decimal(t: &str) -> Option<(String, u32)> {
    let (negative, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (mant, exp) = match t.find(['e', 'E']) {
        Some(k) => (&t[..k], t[k + 1..].parse::<i64>().ok()?),
        None => (t, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if (int.is_empty() && frac.is_empty()) || !int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut digits = format!("{int}{frac}");
    // Scales beyond the digit limit are refused (which also keeps this arithmetic in range).
    let limit = MAX_DECIMAL_DIGITS as i64;
    let mut scale = (frac.len() as i64).checked_sub(exp).filter(|s| (-limit..=limit).contains(s))?;
    if scale < 0 {
        digits.push_str(&"0".repeat(scale.unsigned_abs() as usize));
        scale = 0;
    }
    let digits = digits.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let zero = digits == "0";
    Some((if negative && !zero { format!("-{digits}") } else { digits.to_string() }, scale as u32))
}

/// Decimal text rescaled to `scale` (rounding half away from zero) as two's complement bytes,
/// checked against `precision` and, for a fixed, sign-extended to `size`.
pub fn decimal_bytes(t: &str, scale: u32, precision: Option<u32>, size: Option<usize>) -> Result<Vec<u8>, String> {
    let (signed, from) = parse_decimal(t).ok_or_else(|| format!("{t:?} is not a decimal"))?;
    if signed.len() > MAX_DECIMAL_DIGITS + 1 || scale as usize > MAX_DECIMAL_DIGITS {
        return Err(format!("a decimal of more than {MAX_DECIMAL_DIGITS} digits is not supported"));
    }
    let (negative, mut digits) = match signed.strip_prefix('-') {
        Some(d) => (true, d.to_string()),
        None => (false, signed),
    };
    if from <= scale {
        if digits != "0" {
            digits.push_str(&"0".repeat((scale - from) as usize));
        }
    } else {
        let drop = (from - scale) as usize;
        let keep = digits.len().saturating_sub(drop);
        let round_up = digits.len() >= drop && digits.as_bytes().get(keep).is_some_and(|d| *d >= b'5');
        let mut kept: String = digits[..keep].to_string();
        if kept.is_empty() {
            kept.push('0');
        }
        if round_up {
            kept = magnitude_digits(&add_one(&digits_magnitude(&kept)));
        }
        digits = kept.trim_start_matches('0').to_string();
        if digits.is_empty() {
            digits.push('0');
        }
    }
    if digits.len() > MAX_DECIMAL_DIGITS {
        return Err(format!("a decimal of more than {MAX_DECIMAL_DIGITS} digits is not supported"));
    }
    if let Some(p) = precision
        && digits != "0"
        && digits.len() > p as usize
    {
        return Err(format!("{t:?} needs {} digits at scale {scale}, more than the precision of {p}", digits.len()));
    }
    let signed = if negative && digits != "0" { format!("-{digits}") } else { digits };
    let mut b = to_twos_complement(&signed);
    if let Some(n) = size {
        if b.len() > n {
            return Err(format!("{t:?} does not fit in {n} bytes"));
        }
        let fill = if b[0] & 0x80 != 0 { 0xff } else { 0 };
        let mut full = vec![fill; n - b.len()];
        full.extend(b);
        b = full;
    }
    Ok(b)
}

/// A `big-decimal` value (the unscaled bytes, length first, then the scale as a long) as text.
pub fn big_decimal_text(b: &[u8]) -> Result<String, String> {
    let mut pos = 0;
    let n = read_long_at(b, &mut pos)?;
    let end = usize::try_from(n).ok().and_then(|n| pos.checked_add(n)).filter(|e| *e <= b.len()).ok_or("a big-decimal's digits run past its bytes")?;
    let unscaled = &b[pos..end];
    pos = end;
    // A negative scale (Java's 1E+3) multiplies by a power of ten.
    let scale = read_long_at(b, &mut pos)?;
    if pos != b.len() {
        return Err("a big-decimal has bytes after its scale".into());
    }
    decimal_text(unscaled, scale)
}

/// Decimal text as a `big-decimal` value, keeping every digit and the scale as written.
pub fn big_decimal_bytes(t: &str) -> Result<Vec<u8>, String> {
    let (digits, scale) = parse_decimal(t).ok_or_else(|| format!("{t:?} is not a decimal"))?;
    if digits.len() > MAX_DECIMAL_DIGITS + 1 {
        return Err(format!("a decimal of more than {MAX_DECIMAL_DIGITS} digits is not supported"));
    }
    let mut out = Vec::new();
    write_bytes(&mut out, &to_twos_complement(&digits));
    write_long(&mut out, i64::from(scale));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_convert_offsets_and_far_dates() {
        let z = parse_timestamp("2025-11-08T13:45:00Z", Unit::Millis, true).unwrap();
        // Without seconds, the offset is still applied.
        assert_eq!(parse_timestamp("2025-11-08T14:45+01:00", Unit::Millis, true), Some(z));
        assert_eq!(parse_timestamp("2025-11-08T14:45:00+01:00", Unit::Millis, true), Some(z));
        assert_eq!(parse_timestamp("2025-11-08T08:45:00.000-05:00", Unit::Millis, true), Some(z));
        // A local timestamp keeps the wall-clock time.
        assert_eq!(parse_timestamp("2025-11-08T13:45+01:00", Unit::Millis, false), Some(z));
        assert_eq!(parse_timestamp("2025-02-30T00:00:00Z", Unit::Millis, true), None);
        assert_eq!(parse_timestamp("2025-01-01T24:00:00Z", Unit::Millis, true), parse_timestamp("2025-01-02", Unit::Millis, true));
        assert_eq!(parse_timestamp("10000-01-01T00:00:00Z", Unit::Millis, true), Some(253_402_300_800_000));
        for v in [i64::MAX, i64::MIN, 0, -1, 253_402_300_800_000] {
            let t = timestamp_text(v, Unit::Millis, true);
            assert_eq!(parse_timestamp(&t, Unit::Millis, true), Some(v), "{t}");
        }
        assert_eq!(date_text(-719_528), "0000-01-01");
        assert_eq!(date_text(-719_529), "-0001-12-31");
        for d in [i64::from(i32::MAX), i64::from(i32::MIN), -719_529, 0, 20765] {
            assert_eq!(parse_date(&date_text(d)), Some(d));
        }
    }

    #[test]
    fn times_and_durations() {
        assert_eq!(date_text(0), "1970-01-01");
        assert_eq!(parse_date("2026-11-08"), Some(20765));
        assert_eq!(date_text(20765), "2026-11-08");
        assert_eq!(time_text(49_500_123_000_000, 3).unwrap(), "13:45:00.123");
        assert_eq!(time_text(49_500_000_000_000, 3).unwrap(), "13:45:00");
        assert!(time_text(86_400_000_000_000, 3).is_err());
        assert_eq!(parse_time("13:45:00.123"), Some(49_500_123_000_000));
        assert_eq!(parse_time("13:45"), Some(49_500_000_000_000));
        assert_eq!(parse_time("13:45:00.123Z"), Some(49_500_123_000_000));
        assert_eq!(parse_time("23:59:60"), Some(86_399_999_999_999));
        for bad in ["24:00:00", "1:45", "13:45.5", "13:45:00.", "13:60", "noon"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
        assert_eq!(timestamp_text(1_762_609_500_123, Unit::Millis, true), "2025-11-08T13:45:00.123Z");
        assert_eq!(parse_timestamp("2025-11-08T14:45:00.123+01:00", Unit::Millis, true), Some(1_762_609_500_123));
        assert_eq!(parse_timestamp("2025-11-08T14:45:00.123+01:00", Unit::Millis, false), Some(1_762_613_100_123));
        assert_eq!(timestamp_text(-1, Unit::Micros, false), "1969-12-31T23:59:59.999999");
        let d = parse_duration("P1Y2M3DT4H5.5S").unwrap();
        assert_eq!(duration_text(&d), "P14M3DT14405.5S");
        assert!(parse_duration("-P1D").is_err());
        assert!(parse_duration("P10000000000000000000Y").is_err());
        assert!(parse_duration("P1537228672809129302Y").is_err());
        assert_eq!(duration_text(&[0; 12]), "PT0S");
    }

    #[test]
    fn decimals_are_exact() {
        assert_eq!(decimal_text(&[0x04, 0xd2], 2).unwrap(), "12.34");
        assert_eq!(decimal_text(&[0xfb, 0x2e], 2).unwrap(), "-12.34");
        assert_eq!(decimal_text(&[0x05], 3).unwrap(), "0.005");
        assert_eq!(decimal_text(&[], 0).unwrap(), "0");
        assert_eq!(decimal_bytes("12.34", 2, Some(4), None).unwrap(), [0x04, 0xd2]);
        assert_eq!(decimal_bytes("-12.34", 2, Some(4), None).unwrap(), [0xfb, 0x2e]);
        assert_eq!(decimal_bytes("1.005", 2, None, None).unwrap(), decimal_bytes("1.01", 2, None, None).unwrap());
        assert_eq!(decimal_bytes("-1.005", 2, None, None).unwrap(), decimal_bytes("-1.01", 2, None, None).unwrap());
        assert_eq!(decimal_bytes("1.5E1", 0, None, None).unwrap(), [15]);
        assert_eq!(decimal_bytes("-1", 0, None, Some(4)).unwrap(), [0xff; 4]);
        assert!(decimal_bytes("123.4", 1, Some(3), None).is_err());
        assert_eq!(decimal_bytes("128", 0, None, None).unwrap(), [0x00, 0x80]);
        assert_eq!(decimal_bytes("-128", 0, None, None).unwrap(), [0x80]);
        let wide = "-123456789012345678901234567890123456.78";
        assert_eq!(decimal_text(&decimal_bytes(wide, 2, Some(38), None).unwrap(), 2).unwrap(), wide);
        assert_eq!(big_decimal_text(&big_decimal_bytes("-0.00125").unwrap()).unwrap(), "-0.00125");
        assert_eq!(big_decimal_text(&big_decimal_bytes("1.5e3").unwrap()).unwrap(), "1500");
        assert!(big_decimal_text(&[0x02, 0x01, 0x00, 0x00]).is_err(), "bytes after the scale");
        assert!(big_decimal_text(&[0x7e]).is_err(), "digits past the end");
    }

    #[test]
    fn extreme_decimal_exponents_are_refused() {
        assert_eq!(parse_decimal("1e-9223372036854775808"), None);
        assert_eq!(parse_decimal("1e9223372036854775807"), None);
        assert_eq!(parse_decimal("1e10001"), None);
        assert_eq!(parse_decimal("1.5e3"), Some(("1500".into(), 0)));
        assert_eq!(parse_decimal("."), None);
    }

    #[test]
    fn floats_as_xml_schema_writes_them() {
        assert_eq!(float_text(0.1, 0.1f64.to_string()), "0.1");
        assert_eq!(float_text(1e300, 1e300f64.to_string()), "1E300");
        assert_eq!(float_text(-2.5e-9, (-2.5e-9f64).to_string()), "-2.5E-9");
        assert_eq!(float_text(f64::NAN, String::new()), "NaN");
        // log10 rounds these up; the exponent must come from the digits
        for p in [1e21f64, 1e22, 1e23, 1e-7, 1e-10, 1e300] {
            for v in [f64::from_bits(p.to_bits() - 1), p, f64::from_bits(p.to_bits() + 1)] {
                let t = float_text(v, v.to_string());
                assert_eq!(t.parse::<f64>().unwrap(), v, "{v:e} read as {t}");
            }
        }
        assert_eq!(float_text(1e-7, 1e-7f64.to_string()), "1E-7");
        assert_eq!(float_text(f64::from(1.5e-7f32), 1.5e-7f32.to_string()), "1.5E-7");
        assert_eq!(parse_float("INF"), Some(f64::INFINITY));
        assert_eq!(parse_float("-Infinity"), Some(f64::NEG_INFINITY));
        assert!(parse_float("NaN").unwrap().is_nan());
        assert_eq!(parse_float("1e3"), Some(1000.0));
        assert_eq!(parse_float("0x10"), None);
        assert_eq!(parse_integer("+12.000"), Some(12));
        assert_eq!(parse_integer("12.5"), None);
    }

    #[test]
    fn bytes_uuids() {
        assert_eq!(base64_text(b"hi!"), "aGkh");
        assert_eq!(parse_base64("aG k\nh"), Some(b"hi!".to_vec()));
        assert_eq!(parse_base64("***"), None);
        assert_eq!(json_bytes_text(&[0, 0xff, b'a']), "\u{0}\u{ff}a");
        assert_eq!(parse_json_bytes("\u{0}\u{ff}a"), Some(vec![0, 0xff, b'a']));
        assert_eq!(parse_json_bytes("\u{100}"), None);
        let u = parse_uuid("123e4567-e89b-12d3-a456-426614174000").unwrap();
        assert_eq!(uuid_text(&u), "123e4567-e89b-12d3-a456-426614174000");
        assert_eq!(parse_uuid("123e4567e89b12d3a456426614174000"), Some(u));
        assert_eq!(parse_uuid("123e4567"), None);
        assert_eq!(parse_uuid("ä23e4567e89b12d3a45642661417400"), None);
    }

    #[test]
    fn integers_take_one_sign() {
        assert_eq!(parse_integer("+5"), Some(5));
        assert_eq!(parse_integer("-5"), Some(-5));
        assert_eq!(parse_integer("+-5"), None);
        assert_eq!(parse_integer("++5"), None);
        assert_eq!(parse_integer("+-5.0"), None);
        assert_eq!(parse_integer("-12.00"), Some(-12));
    }

    #[test]
    fn dates_at_the_ends_of_i64_do_not_overflow() {
        for d in [i64::MAX, i64::MIN, i64::MAX - 719_468, i64::MIN + 1] {
            let t = date_text(d);
            assert!(t.len() > 10, "{t}");
        }
        assert_eq!(civil_from_days(-719_528), (0, 1, 1));
        assert_eq!(parse_date(" 2026-11-08 "), Some(20765));
    }

    #[test]
    fn the_end_of_the_day_has_no_fraction() {
        let midnight = parse_timestamp("2025-01-02T00:00:00Z", Unit::Nanos, true);
        assert_eq!(parse_timestamp("2025-01-01T24:00:00Z", Unit::Nanos, true), midnight);
        assert_eq!(parse_timestamp("2025-01-01T24:00:00.000Z", Unit::Nanos, true), midnight);
        assert_eq!(parse_timestamp("2025-01-01T24:00:00.5Z", Unit::Nanos, true), None);
        assert_eq!(parse_timestamp("2025-01-01T24:00:01Z", Unit::Nanos, true), None);
    }

    #[test]
    fn timestamp_zones() {
        // A date with an offset and no time.
        assert_eq!(parse_timestamp("2025-11-08+01:00", Unit::Millis, true), parse_timestamp("2025-11-07T23:00:00Z", Unit::Millis, true));
        assert_eq!(parse_timestamp("2025-11-08-05:00", Unit::Millis, false), parse_timestamp("2025-11-08", Unit::Millis, false));
        assert_eq!(parse_timestamp("-0001-12-31+14:00", Unit::Millis, true).map(|v| v + 14 * 3_600_000), parse_timestamp("-0001-12-31", Unit::Millis, true));
        // Offsets are two digits each, at most 14:00.
        assert_eq!(parse_timestamp("2025-11-08T00:00:00++1:00", Unit::Millis, true), None);
        assert_eq!(parse_timestamp("2025-11-08T00:00:00+1:000", Unit::Millis, true), None);
        assert_eq!(parse_timestamp("2025-11-08T00:00:00+14:30", Unit::Millis, true), None);
        assert!(parse_timestamp("2025-11-08T00:00:00-14:00", Unit::Millis, true).is_some());
        // A `T` needs a time.
        assert_eq!(parse_timestamp("2025-11-08T", Unit::Millis, true), None);
        assert_eq!(parse_timestamp("2025-11-08TZ", Unit::Millis, true), None);
        // A leap second is the last instant of the minute before.
        assert_eq!(parse_timestamp("2016-12-31T23:59:60Z", Unit::Millis, true), parse_timestamp("2016-12-31T23:59:59.999Z", Unit::Millis, true));
        assert_eq!(parse_time(" 13:45:00 "), Some(49_500_000_000_000));
    }

    #[test]
    fn uuids_are_hex_digits_only() {
        assert_eq!(parse_uuid("+23e4567-e89b-12d3-a456-426614174000"), None);
        assert_eq!(parse_uuid("123E4567-E89B-12D3-A456-426614174000"), parse_uuid("123e4567-e89b-12d3-a456-426614174000"));
    }

    #[test]
    fn durations_round_exactly() {
        let millis = |t: &str| parse_duration(t).map(|b| u32::from_le_bytes(b[8..].try_into().unwrap()));
        // 1.0005 is 1.000499999... as a double: an f64 sum rounded this to 1000.
        assert_eq!(millis("PT1.0005S"), Ok(1001));
        assert_eq!(millis("PT1.0004999S"), Ok(1000));
        assert_eq!(millis("PT0.0005S"), Ok(1));
        assert_eq!(millis("PT1.5H"), Ok(5_400_000));
        assert_eq!(millis("PT1H2M3.25S"), Ok(3_723_250));
        assert_eq!(millis("PT0.00000000000000000000001S"), Ok(0));
        assert_eq!(millis("PT1193H2M47.295S"), Ok(u32::MAX));
        assert_eq!(millis("PT1193H2M47.296S"), Err("is too long for an Avro duration".into()));
        assert_eq!(millis("PT99999999999999999999999999H"), Err("is too long for an Avro duration".into()));
        for bad in ["P1DT", "PT", "P", "PT1.2.3S", "PT.S", "PTS", "P1.5D", "P-1D", "-PT1S"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
        let d = parse_duration("P2W1D").unwrap();
        assert_eq!(u32::from_le_bytes(d[4..8].try_into().unwrap()), 15);
    }

    #[test]
    fn decimal_digits_are_limited_after_rescaling() {
        let digits = "9".repeat(MAX_DECIMAL_DIGITS - 5);
        assert!(decimal_bytes(&digits, 0, None, None).is_ok());
        assert!(decimal_bytes(&digits, 10, None, None).unwrap_err().contains("not supported"));
        // half away from zero, at the edge of the precision
        assert_eq!(decimal_bytes("9.995", 2, Some(3), None).unwrap_err(), r#""9.995" needs 4 digits at scale 2, more than the precision of 3"#);
        assert_eq!(decimal_bytes("-0.005", 2, Some(1), None).unwrap(), [0xff]);
        assert_eq!(decimal_bytes("-0.004", 2, Some(1), None).unwrap(), [0x00]);
        // a negative scale reads back as a power of ten
        let mut big = Vec::new();
        write_bytes(&mut big, &[0x0f]);
        write_long(&mut big, -2);
        assert_eq!(big_decimal_text(&big).unwrap(), "1500");
        assert_eq!(decimal_text(&[0x80, 0x00], 0).unwrap(), "-32768");
        assert_eq!(decimal_text(&[0xff, 0xff], 1).unwrap(), "-0.1");
    }
}
