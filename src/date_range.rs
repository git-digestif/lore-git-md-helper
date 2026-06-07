//! Parse human-friendly date specifications used by query filters.
//!
//! A spec is resolved to a half-open UTC day range `[start, end)`,
//! formatted as `YYYY/MM/DD` strings.  Because the email tree is laid
//! out as `YYYY/MM/DD/HH-MM-SS.md`, these strings act directly as
//! lexicographic bounds on the `emails.path` column.
//!
//! Supported forms:
//!
//! * `today` or `now`             – the current UTC day.
//! * `yesterday`                  – the previous UTC day.
//! * a `humantime` duration       – that long before now (rounded to
//!   the enclosing UTC day); e.g. `2w`, `3d`, `1month`, `2years`.
//! * `YYYY`                       – the calendar year.
//! * `YYYY-MM` or `YYYY/MM`       – the calendar month.
//! * `YYYY-MM-DD` or `YYYY/MM/DD` – the calendar day.
//!
//! [`parse_lower_bound`] returns the inclusive start of the period
//! and [`parse_upper_bound_exclusive`] returns the day just past
//! the end, so callers can write `path >= since AND path < until_excl`
//! without worrying about period length or month rollover.

use anyhow::{Context, Result, anyhow};
use time::{Date, Duration, Month, OffsetDateTime};

/// Inclusive start of the period denoted by `spec`, as `YYYY/MM/DD`.
pub fn parse_lower_bound(spec: &str) -> Result<String> {
    let (start, _) = parse_period(spec)?;
    Ok(format_day(start))
}

/// Day just past the end of the period denoted by `spec`,
/// as `YYYY/MM/DD`.  Suitable for a strict `<` comparison.
pub fn parse_upper_bound_exclusive(spec: &str) -> Result<String> {
    let (_, end_excl) = parse_period(spec)?;
    Ok(format_day(end_excl))
}

fn today_utc() -> Date {
    OffsetDateTime::now_utc().date()
}

fn format_day(d: Date) -> String {
    format!("{:04}/{:02}/{:02}", d.year(), d.month() as u8, d.day())
}

fn parse_period(spec: &str) -> Result<(Date, Date)> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty date spec"));
    }

    match trimmed.to_ascii_lowercase().as_str() {
        "today" | "now" => {
            let d = today_utc();
            return Ok((d, d + Duration::days(1)));
        }
        "yesterday" => {
            let d = today_utc() - Duration::days(1);
            return Ok((d, d + Duration::days(1)));
        }
        _ => {}
    }

    if let Some(range) = parse_calendar(trimmed)? {
        return Ok(range);
    }

    if let Ok(dur) = humantime::parse_duration(trimmed) {
        let secs =
            i64::try_from(dur.as_secs()).map_err(|_| anyhow!("duration too large: {trimmed}"))?;
        let dt = OffsetDateTime::now_utc() - Duration::seconds(secs);
        let d = dt.date();
        return Ok((d, d + Duration::days(1)));
    }

    Err(anyhow!(
        "could not parse date spec `{spec}`; expected YYYY[-MM[-DD]] \
         (with `-` or `/` separators), a duration like `2w`/`3d`/`1month`, \
         or one of: today, now, yesterday"
    ))
}

/// Try to parse `spec` as an absolute calendar date.  Returns
/// `Ok(None)` if the input is not a plain digit-and-separator form
/// (so callers can fall through to other parsers).
fn parse_calendar(spec: &str) -> Result<Option<(Date, Date)>> {
    let normalised = spec.replace('/', "-");
    let parts: Vec<&str> = normalised.split('-').collect();
    let all_digits = parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if !all_digits || parts[0].len() != 4 {
        return Ok(None);
    }
    let y: i32 = parts[0].parse().context("invalid year")?;
    match parts.as_slice() {
        [_] => {
            let start = Date::from_calendar_date(y, Month::January, 1)?;
            let end = Date::from_calendar_date(y + 1, Month::January, 1)?;
            Ok(Some((start, end)))
        }
        [_, mm] => {
            let m: u8 = mm.parse().context("invalid month")?;
            let mon = Month::try_from(m).context("invalid month")?;
            let start = Date::from_calendar_date(y, mon, 1)?;
            let end = if m == 12 {
                Date::from_calendar_date(y + 1, Month::January, 1)?
            } else {
                Date::from_calendar_date(y, Month::try_from(m + 1)?, 1)?
            };
            Ok(Some((start, end)))
        }
        [_, mm, dd] => {
            let m: u8 = mm.parse().context("invalid month")?;
            let d: u8 = dd.parse().context("invalid day")?;
            let day = Date::from_calendar_date(y, Month::try_from(m)?, d)?;
            Ok(Some((day, day + Duration::days(1))))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_lower_and_upper() {
        assert_eq!(parse_lower_bound("2021").unwrap(), "2021/01/01");
        assert_eq!(parse_upper_bound_exclusive("2021").unwrap(), "2022/01/01");
    }

    #[test]
    fn month_lower_and_upper() {
        assert_eq!(parse_lower_bound("2021-07").unwrap(), "2021/07/01");
        assert_eq!(parse_lower_bound("2021/07").unwrap(), "2021/07/01");
        assert_eq!(
            parse_upper_bound_exclusive("2021-07").unwrap(),
            "2021/08/01"
        );
    }

    #[test]
    fn december_rolls_into_next_year() {
        assert_eq!(
            parse_upper_bound_exclusive("2021-12").unwrap(),
            "2022/01/01"
        );
    }

    #[test]
    fn day_lower_and_upper() {
        assert_eq!(parse_lower_bound("2021-07-15").unwrap(), "2021/07/15");
        assert_eq!(
            parse_upper_bound_exclusive("2021-07-15").unwrap(),
            "2021/07/16"
        );
        assert_eq!(parse_lower_bound("2021/07/15").unwrap(), "2021/07/15");
    }

    #[test]
    fn day_upper_rolls_into_next_month() {
        assert_eq!(
            parse_upper_bound_exclusive("2021-07-31").unwrap(),
            "2021/08/01"
        );
    }

    #[test]
    fn today_and_yesterday_and_now() {
        let today = today_utc();
        let tomorrow = today + Duration::days(1);
        let yesterday = today - Duration::days(1);
        assert_eq!(parse_lower_bound("today").unwrap(), format_day(today));
        assert_eq!(
            parse_upper_bound_exclusive("today").unwrap(),
            format_day(tomorrow)
        );
        assert_eq!(
            parse_lower_bound("YESTERDAY").unwrap(),
            format_day(yesterday)
        );
        assert_eq!(parse_lower_bound("now").unwrap(), format_day(today));
    }

    #[test]
    fn duration_resolves_to_that_long_ago() {
        let expected = format_day(today_utc() - Duration::days(14));
        assert_eq!(parse_lower_bound("2w").unwrap(), expected);
        assert_eq!(parse_lower_bound("14days").unwrap(), expected);
    }

    #[test]
    fn rejects_garbage_and_empty() {
        assert!(parse_lower_bound("").is_err());
        assert!(parse_lower_bound("   ").is_err());
        assert!(parse_lower_bound("not-a-date").is_err());
        assert!(parse_lower_bound("99").is_err());
    }

    #[test]
    fn rejects_out_of_range_components() {
        assert!(parse_lower_bound("2021-13").is_err());
        assert!(parse_lower_bound("2021-02-30").is_err());
    }

    #[test]
    fn bounds_act_as_path_filter_for_second_half_of_year() {
        // The motivating example: `--since 2021-07 --until 2021-12`
        // selects every email under `2021/07/..` through `2021/12/..`.
        let since = parse_lower_bound("2021-07").unwrap();
        let until_excl = parse_upper_bound_exclusive("2021-12").unwrap();
        assert!("2021/07/01/00-00-00.md" >= since.as_str());
        assert!("2021/12/31/23-59-59.md" < until_excl.as_str());
        assert!("2022/01/01/00-00-00.md" >= until_excl.as_str());
        assert!("2021/06/30/23-59-59.md" < since.as_str());
    }
}
