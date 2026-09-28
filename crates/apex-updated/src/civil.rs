//! Calendar arithmetic for the `Stale` property: the build date inside an Apex version
//! (`45.YYYYMMDD.RUN`) as Unix seconds.
//!
//! `last-check.json` stores the remote version but not its build time (contract §4), so after
//! a restart the date field of the version is the only build time the daemon has for the
//! remote image. The conversion is Howard Hinnant's `days_from_civil`, exact for the
//! proleptic Gregorian calendar.

/// Seconds in a day.
const DAY: u64 = 86_400;

/// Days from 1970-01-01 to `year-month-day` (proleptic Gregorian), or `None` for an invalid
/// date or one before 1970.
pub fn days_from_civil(year: u64, month: u64, day: u64) -> Option<u64> {
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    // Shift the year so it starts in March: the leap day is then the last day of the year.
    let y = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    // 719_468 is the day number of 1970-01-01 counted from 0000-03-01.
    (era * 146_097 + doe).checked_sub(719_468)
}

fn is_leap(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Midnight UTC of the date `yyyymmdd` in Unix seconds, or `None` if it is not a valid date
/// on or after 1970-01-01.
pub fn unix_from_yyyymmdd(date: u64) -> Option<u64> {
    let days = days_from_civil(date / 10_000, (date / 100) % 100, date % 100)?;
    days.checked_mul(DAY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The inverse (`civil_from_days`), only needed to test the conversion.
    fn civil_from_days(days: u64) -> (u64, u64, u64) {
        let z = days + 719_468;
        let era = z / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + u64::from(month <= 2);
        (year, month, day)
    }

    #[test]
    fn known_dates() {
        assert_eq!(unix_from_yyyymmdd(19_700_101), Some(0));
        assert_eq!(unix_from_yyyymmdd(20_000_301), Some(951_868_800));
        assert_eq!(unix_from_yyyymmdd(20_260_924), Some(1_790_208_000));
        assert_eq!(unix_from_yyyymmdd(20_240_229), Some(1_709_164_800));
    }

    #[test]
    fn invalid_dates() {
        for date in [
            20_260_230, 20_250_229, 20_261_301, 20_260_000, 20_260_100, 19_691_231, 0, 20_261_131,
        ] {
            assert_eq!(unix_from_yyyymmdd(date), None, "{date}");
        }
    }

    proptest! {
        #[test]
        fn round_trips_through_the_inverse(days in 0u64..3_000_000) {
            let (y, m, d) = civil_from_days(days);
            prop_assert_eq!(days_from_civil(y, m, d), Some(days));
            prop_assert_eq!(unix_from_yyyymmdd(y * 10_000 + m * 100 + d), Some(days * DAY));
        }

        #[test]
        fn never_panics(date in any::<u64>()) {
            let _ = unix_from_yyyymmdd(date);
        }
    }
}
