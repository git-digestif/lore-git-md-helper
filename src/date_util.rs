//! Day-level date helpers for "YYYY/MM/DD" day strings.

/// Extract the day prefix ("YYYY/MM/DD") from a date-key like
/// "2025/01/03/14-46-37".
pub fn day_of(dk: &str) -> &str {
    &dk[..dk.rfind('/').unwrap_or(dk.len())]
}

/// Extract "YYYY/MM" from a "YYYY/MM/DD" day string.
pub fn month_of(day: &str) -> &str {
    &day[..7]
}

/// Parse a "YYYY/MM/DD" day string into a `time::Date`.
pub fn parse_day(s: &str) -> Option<time::Date> {
    let p: Vec<&str> = s.split('/').collect();
    if p.len() < 3 {
        return None;
    }
    let y: i32 = p[0].parse().ok()?;
    let m: u8 = p[1].parse().ok()?;
    let d: u8 = p[2].parse().ok()?;
    time::Date::from_calendar_date(y, time::Month::try_from(m).ok()?, d).ok()
}

/// Format a `time::Date` as "YYYY/MM/DD".
pub fn format_day(d: time::Date) -> String {
    format!("{:04}/{:02}/{:02}", d.year(), d.month() as u8, d.day())
}

/// Compute the number of days between two "YYYY/MM/DD" day strings.
pub fn days_between(earlier: &str, later: &str) -> Option<i64> {
    let d1 = parse_day(earlier)?;
    let d2 = parse_day(later)?;
    Some((d2 - d1).whole_days())
}

/// Return the Monday of the ISO week containing `day` as "YYYY/MM/DD".
pub fn iso_monday(day: &str) -> Option<String> {
    let d = parse_day(day)?;
    let wd = d.weekday().number_days_from_monday();
    Some(format_day(d - time::Duration::days(wd as i64)))
}

/// Return the Sunday of the ISO week containing `day` as "YYYY/MM/DD".
pub fn iso_sunday(day: &str) -> Option<String> {
    let d = parse_day(day)?;
    let wd = d.weekday().number_days_from_monday();
    Some(format_day(d + time::Duration::days(6 - wd as i64)))
}

/// Add `n` days to a "YYYY/MM/DD" day string.
pub fn add_days(day: &str, n: i64) -> Option<String> {
    let d = parse_day(day)?;
    Some(format_day(d + time::Duration::days(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_day_of() {
        assert_eq!(day_of("2025/01/03/14-46-37"), "2025/01/03");
        assert_eq!(day_of("2025/12/31/23-59-59"), "2025/12/31");
    }

    #[test]
    fn test_month_of() {
        assert_eq!(month_of("2025/01/03"), "2025/01");
        assert_eq!(month_of("2025/12/31"), "2025/12");
    }

    #[test]
    fn test_days_between() {
        assert_eq!(days_between("2025/01/01", "2025/01/01"), Some(0));
        assert_eq!(days_between("2025/01/01", "2025/01/02"), Some(1));
        assert_eq!(days_between("2025/01/01", "2025/01/08"), Some(7));
        assert_eq!(days_between("2024/12/31", "2025/01/01"), Some(1));
        assert_eq!(days_between("2025/01/01", "2025/02/01"), Some(31));
    }

    #[test]
    fn test_iso_monday() {
        // 2025-01-06 is a Monday
        assert_eq!(iso_monday("2025/01/06"), Some("2025/01/06".into()));
        // 2025-01-08 is a Wednesday
        assert_eq!(iso_monday("2025/01/08"), Some("2025/01/06".into()));
        // 2025-01-12 is a Sunday
        assert_eq!(iso_monday("2025/01/12"), Some("2025/01/06".into()));
        // 2025-01-01 is a Wednesday, Monday is 2024-12-30
        assert_eq!(iso_monday("2025/01/01"), Some("2024/12/30".into()));
    }

    #[test]
    fn test_add_days() {
        assert_eq!(add_days("2025/01/01", 0), Some("2025/01/01".into()));
        assert_eq!(add_days("2025/01/01", 6), Some("2025/01/07".into()));
        assert_eq!(add_days("2025/01/31", 1), Some("2025/02/01".into()));
        assert_eq!(add_days("2025/02/28", 1), Some("2025/03/01".into()));
        assert_eq!(add_days("2024/02/28", 1), Some("2024/02/29".into())); // leap year
    }
}
