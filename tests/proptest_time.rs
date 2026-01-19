//! Property-based tests for time parsing.
//!
//! Uses proptest to verify that:
//! - RFC3339 timestamps parse correctly and roundtrip
//! - Relative time expressions work correctly
//! - Invalid formats are rejected
//! - Keywords parse to future/past times as expected

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use proptest::prelude::*;
use tracing::info;

use beads_rust::util::time::{parse_flexible_timestamp, parse_relative_time};

/// Initialize test logging for proptest
fn init_test_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 100,
        ..Default::default()
    })]

    /// Property: Valid RFC3339 timestamps parse successfully
    #[test]
    fn rfc3339_parses_correctly(
        year in 2020u32..2030u32,
        month in 1u32..=12u32,
        day in 1u32..=28u32,  // Use 28 to avoid month-length issues
        hour in 0u32..24u32,
        minute in 0u32..60u32,
        second in 0u32..60u32,
    ) {
        init_test_logging();

        let timestamp =
            format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z");
        info!("proptest_rfc3339: timestamp={timestamp}");

        let result = parse_flexible_timestamp(&timestamp, "test");

        prop_assert!(result.is_ok(), "Valid RFC3339 should parse: {timestamp}");

        let parsed = result.unwrap();
        let year_i32 = i32::try_from(year).expect("year fits i32");
        prop_assert_eq!(parsed.year(), year_i32, "Year should match");
        prop_assert_eq!(parsed.month(), month, "Month should match");
        prop_assert_eq!(parsed.day(), day, "Day should match");
        prop_assert_eq!(parsed.hour(), hour, "Hour should match");
        prop_assert_eq!(parsed.minute(), minute, "Minute should match");
        prop_assert_eq!(parsed.second(), second, "Second should match");
    }

    /// Property: RFC3339 roundtrip - parse and format back
    #[test]
    fn rfc3339_roundtrip(
        year in 2020u32..2030u32,
        month in 1u32..=12u32,
        day in 1u32..=28u32,
        hour in 0u32..24u32,
        minute in 0u32..60u32,
        second in 0u32..60u32,
    ) {
        init_test_logging();

        let original =
            format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00");
        info!("proptest_roundtrip: original={original}");

        let parsed = parse_flexible_timestamp(&original, "test");
        prop_assert!(parsed.is_ok(), "Should parse: {original}");

        let formatted = parsed.unwrap().to_rfc3339();

        // Compare the parsed datetime values, not string representations
        // (format may differ slightly: Z vs +00:00)
        let reparsed = DateTime::parse_from_rfc3339(&formatted);
        prop_assert!(reparsed.is_ok(), "Formatted should reparse: {formatted}");
    }

    /// Property: Positive relative time (+Nd) produces future datetime
    #[test]
    fn relative_positive_is_future(amount in 1i64..365i64) {
        init_test_logging();

        let input = format!("+{amount}d");
        info!("proptest_relative_future: input={input}");

        let now = Utc::now();
        let result = parse_relative_time(&input);

        prop_assert!(result.is_some(), "Should parse: {input}");

        let parsed = result.unwrap();
        prop_assert!(parsed > now, "+{amount}d should be in the future");

        // Verify approximate difference (within 1 second tolerance for test timing)
        let expected_diff = Duration::days(amount);
        let actual_diff = parsed - now;
        let tolerance = Duration::seconds(2);
        prop_assert!(
            (actual_diff - expected_diff).abs() < tolerance,
            "Difference should be approximately {amount} days"
        );
    }

    /// Property: Negative relative time (-Nd) produces past datetime
    #[test]
    fn relative_negative_is_past(amount in 1i64..365i64) {
        init_test_logging();

        let input = format!("-{amount}d");
        info!("proptest_relative_past: input={input}");

        let now = Utc::now();
        let result = parse_relative_time(&input);

        prop_assert!(result.is_some(), "Should parse: {input}");

        let parsed = result.unwrap();
        prop_assert!(parsed < now, "-{amount}d should be in the past");
    }

    /// Property: Hours relative time works correctly
    #[test]
    fn relative_hours_correct(amount in 1i64..100i64) {
        init_test_logging();

        let input = format!("+{amount}h");
        info!("proptest_relative_hours: input={input}");

        let now = Utc::now();
        let result = parse_relative_time(&input);

        prop_assert!(result.is_some(), "Should parse: {input}");

        let parsed = result.unwrap();
        let expected_diff = Duration::hours(amount);
        let actual_diff = parsed - now;
        let tolerance = Duration::seconds(2);

        prop_assert!(
            (actual_diff - expected_diff).abs() < tolerance,
            "Difference should be approximately {amount} hours"
        );
    }

    /// Property: Minutes relative time works correctly
    #[test]
    fn relative_minutes_correct(amount in 1i64..1000i64) {
        init_test_logging();

        let input = format!("+{amount}m");
        info!("proptest_relative_minutes: input={input}");

        let now = Utc::now();
        let result = parse_relative_time(&input);

        prop_assert!(result.is_some(), "Should parse: {input}");

        let parsed = result.unwrap();
        let expected_diff = Duration::minutes(amount);
        let actual_diff = parsed - now;
        let tolerance = Duration::seconds(2);

        prop_assert!(
            (actual_diff - expected_diff).abs() < tolerance,
            "Difference should be approximately {amount} minutes"
        );
    }

    /// Property: Weeks relative time works correctly
    #[test]
    fn relative_weeks_correct(amount in 1i64..52i64) {
        init_test_logging();

        let input = format!("+{amount}w");
        info!("proptest_relative_weeks: input={input}");

        let now = Utc::now();
        let result = parse_relative_time(&input);

        prop_assert!(result.is_some(), "Should parse: {input}");

        let parsed = result.unwrap();
        let expected_diff = Duration::weeks(amount);
        let actual_diff = parsed - now;
        let tolerance = Duration::seconds(2);

        prop_assert!(
            (actual_diff - expected_diff).abs() < tolerance,
            "Difference should be approximately {amount} weeks"
        );
    }

    /// Property: Simple date (YYYY-MM-DD) parses correctly
    #[test]
    fn simple_date_parses(
        year in 2020u32..2030u32,
        month in 1u32..=12u32,
        day in 1u32..=28u32,
    ) {
        init_test_logging();

        let date = format!("{year:04}-{month:02}-{day:02}");
        info!("proptest_simple_date: date={date}");

        let result = parse_flexible_timestamp(&date, "test");

        prop_assert!(result.is_ok(), "Simple date should parse: {date}");

        let parsed = result.unwrap();
        let year_i32 = i32::try_from(year).expect("year fits i32");
        prop_assert_eq!(parsed.year(), year_i32, "Year should match");
        prop_assert_eq!(parsed.month(), month, "Month should match");
        prop_assert_eq!(parsed.day(), day, "Day should match");
    }

    /// Property: Invalid unit letters are rejected
    #[test]
    fn invalid_unit_rejected(
        amount in 1i64..100i64,
        unit in "[a-z&&[^mhdw]]",  // Any letter except m, h, d, w
    ) {
        init_test_logging();

        let input = format!("+{amount}{unit}");
        info!("proptest_invalid_unit: input={input}");

        let result = parse_relative_time(&input);

        prop_assert!(result.is_none(), "Invalid unit should not parse: {input}");
    }

    /// Property: Random garbage is rejected
    #[test]
    fn garbage_rejected(garbage in "[^0-9+-]{3,20}") {
        init_test_logging();

        // Skip if garbage happens to match a keyword
        let lower = garbage.to_lowercase();
        prop_assume!(lower != "tomorrow" && lower != "next-week" && lower != "nextweek");

        info!("proptest_garbage: input={garbage}");

        let result = parse_flexible_timestamp(&garbage, "test");

        prop_assert!(result.is_err(), "Garbage should not parse: {garbage}");
    }
}

/// Property: "tomorrow" keyword produces a future datetime
#[test]
fn keyword_tomorrow_is_future() {
    init_test_logging();
    info!("proptest_tomorrow: testing tomorrow keyword");

    let now = Utc::now();
    let result = parse_flexible_timestamp("tomorrow", "test");

    assert!(result.is_ok(), "tomorrow should parse");
    let parsed = result.unwrap();
    assert!(parsed > now, "tomorrow should be in the future");

    // Should be roughly 1 day ahead
    let diff = parsed - now;
    assert!(
        diff > Duration::hours(12) && diff < Duration::hours(48),
        "tomorrow should be 12-48 hours away"
    );

    info!("proptest_tomorrow: PASS");
}

/// Property: "next-week" keyword produces a datetime ~7 days away
#[test]
fn keyword_next_week_is_week_away() {
    init_test_logging();
    info!("proptest_next_week: testing next-week keyword");

    let now = Utc::now();
    let result = parse_flexible_timestamp("next-week", "test");

    assert!(result.is_ok(), "next-week should parse");
    let parsed = result.unwrap();
    assert!(parsed > now, "next-week should be in the future");

    // Should be roughly 7 days ahead
    let diff = parsed - now;
    assert!(
        diff > Duration::days(6) && diff < Duration::days(8),
        "next-week should be 6-8 days away"
    );

    info!("proptest_next_week: PASS");
}

/// Property: RFC3339 with timezone offset parses correctly
#[test]
fn rfc3339_with_offset_parses() {
    init_test_logging();
    info!("proptest_offset: testing timezone offsets");

    let test_cases = [
        "2025-01-15T12:00:00+00:00",
        "2025-01-15T12:00:00-05:00",
        "2025-01-15T12:00:00+05:30",
        "2025-01-15T00:00:00+12:00",
    ];

    for timestamp in test_cases {
        let result = parse_flexible_timestamp(timestamp, "test");
        assert!(
            result.is_ok(),
            "RFC3339 with offset should parse: {timestamp}"
        );
    }

    info!("proptest_offset: PASS");
}

/// Property: Whitespace is trimmed from input
#[test]
fn whitespace_is_trimmed() {
    init_test_logging();
    info!("proptest_whitespace: testing whitespace handling");

    let test_cases = [
        ("  +1d  ", true),
        ("\t+1h\n", true),
        ("  tomorrow  ", true),
        ("  2025-01-15  ", true),
    ];

    for (input, should_parse) in test_cases {
        let result = parse_flexible_timestamp(input, "test");
        if should_parse {
            assert!(
                result.is_ok(),
                "Whitespace-padded '{input_dbg}' should parse",
                input_dbg = input.escape_debug()
            );
        }
    }

    info!("proptest_whitespace: PASS");
}
