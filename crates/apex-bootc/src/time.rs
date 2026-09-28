//! RFC 3339 timestamps as bootc prints them (chrono's `DateTime<Utc>` serialisation,
//! e.g. `2023-10-14T19:22:15.42Z`), converted to Unix seconds for the D-Bus
//! `timestamp` (`t`) key.

/// Parses an RFC 3339 date-time and returns whole seconds since the Unix epoch.
///
/// Accepts `T`/`t`/space as the separator, an optional fraction (truncated), and `Z`/`z`
/// or a `±HH:MM` offset. A leap second (`:60`) counts as `:59`. Returns `None` for
/// anything else, including impossible dates such as February 30.
pub fn parse_rfc3339_unix(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let year = digits(b, 0, 4)?;
    let month = digits(b, 5, 2)?;
    let day = digits(b, 8, 2)?;
    let hour = digits(b, 11, 2)?;
    let minute = digits(b, 14, 2)?;
    let second = digits(b, 17, 2)?;
    if b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    let offset_seconds = match b.get(i..)? {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), rest @ ..] if rest.len() == 5 && rest[2] == b':' => {
            let oh = digits(rest, 0, 2)?;
            let om = digits(rest, 3, 2)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let magnitude = i64::from(oh * 3600 + om * 60);
            if *sign == b'+' { magnitude } else { -magnitude }
        }
        _ => return None,
    };
    let days = days_from_civil(i64::from(year), month, day);
    let local =
        days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second.min(59));
    Some(local - offset_seconds)
}

/// Reads `len` ASCII digits at `at` as a number.
fn digits(b: &[u8], at: usize, len: usize) -> Option<u32> {
    let slice = b.get(at..at + len)?;
    slice.iter().try_fold(0u32, |acc, &c| {
        c.is_ascii_digit().then(|| acc * 10 + u32::from(c - b'0'))
    })
}

fn is_leap(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Inverse of `days_from_civil`, only needed to generate test input.
    fn civil_from_days(z: i64) -> (i64, u32, u32) {
        let z = z + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    #[test]
    fn known_values() {
        assert_eq!(parse_rfc3339_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_unix("2024-08-07T12:00:00Z"),
            Some(1_723_032_000)
        );
        assert_eq!(
            parse_rfc3339_unix("2023-10-14T19:22:15.42Z"),
            Some(1_697_311_335)
        );
        assert_eq!(
            parse_rfc3339_unix("2023-10-14T21:22:15+02:00"),
            Some(1_697_311_335)
        );
        assert_eq!(
            parse_rfc3339_unix("2023-10-14t19:22:15z"),
            Some(1_697_311_335)
        );
        assert_eq!(
            parse_rfc3339_unix("2016-12-31T23:59:60Z"),
            Some(1_483_228_799)
        );
        assert_eq!(
            parse_rfc3339_unix("2000-02-29T00:00:00Z"),
            Some(951_782_400)
        );
    }

    #[test]
    fn rejects_invalid() {
        for s in [
            "",
            "2023-10-14",
            "2023-10-14T19:22:15",
            "2023-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2023-13-01T00:00:00Z",
            "2023-10-14T24:00:00Z",
            "2023-10-14T19:22:15.Z",
            "2023-10-14T19:22:15+0200",
            "2023-10-14T19:22:15Zjunk",
            "２０２３-10-14T19:22:15Z",
        ] {
            assert_eq!(parse_rfc3339_unix(s), None, "{s}");
        }
    }

    proptest! {
        #[test]
        fn round_trips_every_second(secs in 0i64..253_402_300_800, frac in proptest::option::of("[0-9]{1,9}")) {
            let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
            let rem = secs.rem_euclid(86_400);
            let frac = frac.map(|f| format!(".{f}")).unwrap_or_default();
            let s = format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{frac}Z", rem / 3600, rem / 60 % 60, rem % 60);
            prop_assert_eq!(parse_rfc3339_unix(&s), Some(secs));
        }

        #[test]
        fn never_panics(s in "\\PC{0,40}") {
            let _ = parse_rfc3339_unix(&s);
        }
    }
}
