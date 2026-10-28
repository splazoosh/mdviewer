//! Date formatting without a calendar crate: the app only ever needs
//! `YYYY-MM-DD` and `HH:MM` in UTC.

use std::time::SystemTime;

/// Seconds since the epoch as (`YYYY-MM-DD`, `HH:MM`), UTC.
#[must_use]
pub fn split_utc(secs: u64) -> (String, String) {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(i64::try_from(days).unwrap_or(i64::MAX / 2));
    (
        format!("{y:04}-{m:02}-{d:02}"),
        format!("{:02}:{:02}", rem / 3600, (rem % 3600) / 60),
    )
}

/// Today's date as `YYYY-MM-DD`, UTC.
#[must_use]
pub fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    split_utc(secs).0
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
///
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants_format_correctly() {
        assert_eq!(split_utc(0), ("1970-01-01".to_owned(), "00:00".to_owned()));
        assert_eq!(split_utc(951_868_800).0, "2000-03-01");
        assert_eq!(
            split_utc(1_789_825_500),
            ("2026-09-19".to_owned(), "13:45".to_owned())
        );
    }
}
