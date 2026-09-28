/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a Gregorian date.
pub const fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// UTC, labelled: local time would need the time zone database.
pub fn format_utc(unix: u64) -> String {
    let unix = i64::try_from(unix).unwrap_or(i64::MAX);
    let (days, seconds) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        seconds / 3600,
        (seconds % 3600) / 60
    )
}

pub fn format_utc_date(unix: u64) -> String {
    let unix = i64::try_from(unix).unwrap_or(i64::MAX);
    let (year, month, day) = civil_from_days(unix.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn known_dates() {
        assert_eq!(format_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(
            format_utc(1_727_136_000 + 3600 * 10 + 5 * 60),
            "2024-09-24 10:05 UTC"
        );
        assert_eq!(format_utc_date(1_790_208_000), "2026-09-24");
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let m = i64::from(m);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    proptest! {
        #[test]
        fn civil_round_trips(days in -1_000_000_i64..1_000_000) {
            let (y, m, d) = civil_from_days(days);
            prop_assert!((1..=12).contains(&m) && (1..=31).contains(&d));
            prop_assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
