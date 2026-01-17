//! Time and date parsing utilities.

use crate::error::{BeadsError, Result};
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc};

/// Parse a flexible time specification into a `DateTime<Utc>`.
///
/// Supports:
/// - RFC3339: `2025-01-15T12:00:00Z`, `2025-01-15T12:00:00+00:00`
/// - Simple date: `2025-01-15` (defaults to 9:00 AM local time)
/// - Relative duration: `+1h`, `+2d`, `+1w`, `+30m`
/// - Keywords: `tomorrow`, `next-week`
///
/// # Errors
///
/// Returns an error if:
/// - The time format is invalid or unrecognized
/// - A relative duration has an invalid unit (only m, h, d, w supported)
/// - The local time is ambiguous (e.g., during DST transitions)
///
/// # Panics
///
/// This function does not panic. The internal `unwrap()` calls on `from_hms_opt(9, 0, 0)`
/// are safe because 9:00:00 is always a valid time.
pub fn parse_flexible_timestamp(s: &str, field_name: &str) -> Result<DateTime<Utc>> {
    let s = s.trim();

    // Try RFC3339 first
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    // Try simple date (YYYY-MM-DD) - default to 9:00 AM local time
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let naive_dt = date.and_time(time);
        let local_dt = Local
            .from_local_datetime(&naive_dt)
            .single()
            .ok_or_else(|| BeadsError::validation(field_name, "ambiguous local time"))?;
        return Ok(local_dt.with_timezone(&Utc));
    }

    // Try relative duration (+1h, +2d, +1w, +30m, -7d)
    if let Some(rest) = s.strip_prefix(['+', '-'].as_ref()) {
        let is_negative = s.starts_with('-');
        if let Some(unit_char) = rest.chars().last() {
            let amount_str = &rest[..rest.len() - unit_char.len_utf8()];
            if let Ok(amount) = amount_str.parse::<i64>() {
                let amount = if is_negative { -amount } else { amount };
                let duration = match unit_char {
                    'm' => Duration::minutes(amount),
                    'h' => Duration::hours(amount),
                    'd' => Duration::days(amount),
                    'w' => Duration::weeks(amount),
                    _ => {
                        return Err(BeadsError::validation(
                            field_name,
                            "invalid unit (use m, h, d, w)",
                        ));
                    }
                };
                return Ok(Utc::now() + duration);
            }
        }
    }

    // Try keywords
    let now = Local::now();
    match s.to_lowercase().as_str() {
        "tomorrow" => {
            let tomorrow = now.date_naive() + Duration::days(1);
            let time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
            let naive_dt = tomorrow.and_time(time);
            let local_dt = Local
                .from_local_datetime(&naive_dt)
                .single()
                .ok_or_else(|| BeadsError::validation(field_name, "ambiguous local time"))?;
            Ok(local_dt.with_timezone(&Utc))
        }
        "next-week" | "nextweek" => {
            let next_week = now.date_naive() + Duration::weeks(1);
            let time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
            let naive_dt = next_week.and_time(time);
            let local_dt = Local
                .from_local_datetime(&naive_dt)
                .single()
                .ok_or_else(|| BeadsError::validation(field_name, "ambiguous local time"))?;
            Ok(local_dt.with_timezone(&Utc))
        }
        _ => Err(BeadsError::validation(
            field_name,
            "invalid time format (try: +1h, -7d, tomorrow, next-week, or 2025-01-15)",
        )),
    }
}

/// Parse a relative time expression into a `DateTime<Utc>`.
///
/// Supports:
/// - Relative duration: `+1h`, `+2d`, `+1w`, `+30m`, `-7d`
/// - Keywords: `tomorrow`, `next-week`
///
/// Returns `None` if the input cannot be parsed as a relative time.
#[must_use]
pub fn parse_relative_time(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();

    // Try relative duration (+1h, -2d, +1w, -30m)
    if let Some(rest) = s.strip_prefix(['+', '-'].as_ref()) {
        let is_negative = s.starts_with('-');
        if let Some(unit_char) = rest.chars().last() {
            let amount_str = &rest[..rest.len() - unit_char.len_utf8()];
            if let Ok(amount) = amount_str.parse::<i64>() {
                let amount = if is_negative { -amount } else { amount };
                let duration = match unit_char {
                    'm' => Duration::minutes(amount),
                    'h' => Duration::hours(amount),
                    'd' => Duration::days(amount),
                    'w' => Duration::weeks(amount),
                    _ => return None,
                };
                return Some(Utc::now() + duration);
            }
        }
    }

    // Try keywords
    let now = Local::now();
    match s.to_lowercase().as_str() {
        "tomorrow" => {
            let tomorrow = now.date_naive() + Duration::days(1);
            let time = NaiveTime::from_hms_opt(9, 0, 0)?;
            let naive_dt = tomorrow.and_time(time);
            Local
                .from_local_datetime(&naive_dt)
                .single()
                .map(|dt| dt.with_timezone(&Utc))
        }
        "next-week" | "nextweek" => {
            let next_week = now.date_naive() + Duration::weeks(1);
            let time = NaiveTime::from_hms_opt(9, 0, 0)?;
            let naive_dt = next_week.and_time(time);
            Local
                .from_local_datetime(&naive_dt)
                .single()
                .map(|dt| dt.with_timezone(&Utc))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    #[test]
    fn test_parse_flexible_rfc3339() {
        let result = parse_flexible_timestamp("2025-01-15T12:00:00Z", "test").unwrap();
        assert_eq!(result.year(), 2025);
    }

    #[test]
    fn test_parse_flexible_simple_date() {
        let result = parse_flexible_timestamp("2025-06-20", "test").unwrap();
        assert_eq!(result.year(), 2025);
        assert_eq!(result.month(), 6);
        assert_eq!(result.day(), 20);
    }

    #[test]
    fn test_parse_flexible_relative() {
        let result = parse_flexible_timestamp("+1h", "test").unwrap();
        assert!(result > Utc::now());
    }

    #[test]
    fn test_parse_flexible_keywords() {
        let result = parse_flexible_timestamp("tomorrow", "test").unwrap();
        assert!(result > Utc::now());
    }

    #[test]
    fn test_parse_relative_time_positive() {
        let result = parse_relative_time("+1h").unwrap();
        assert!(result > Utc::now());
    }

    #[test]
    fn test_parse_relative_time_negative() {
        let result = parse_relative_time("-7d").unwrap();
        assert!(result < Utc::now());
    }

    #[test]
    fn test_parse_relative_time_invalid() {
        assert!(parse_relative_time("invalid").is_none());
        assert!(parse_relative_time("2025-01-15").is_none());
    }
}
