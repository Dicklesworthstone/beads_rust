//! Page-before-hydration regressions and bounded-read fault controls.

use super::*;
use crate::model::{IssueType, Priority, Status};
use chrono::{Duration, TimeZone, Utc};

fn issue(id: &str, title: &str) -> Issue {
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    Issue {
        id: id.to_string(),
        title: title.to_string(),
        created_at: now,
        updated_at: now,
        ..Issue::default()
    }
}

fn ids(issues: &[Issue]) -> Vec<&str> {
    issues.iter().map(|item| item.id.as_str()).collect()
}

fn fixture() -> SqliteStorage {
    let mut storage = SqliteStorage::open_memory().unwrap();
    for (id, title, priority, day, description, closed) in [
        ("bd-a", "CAFÉ older", 1, 0, None, false),
        ("bd-b", "Description hit", 0, 0, Some("CAFÉ"), false),
        ("bd-c", "Comment-only hit", 1, 1, None, false),
        ("bd-d", "café tied", 1, 1, None, false),
        ("bd-control", "Not a match", 0, 2, None, false),
        ("bd-history", "CAFÉ closed", 0, 3, None, true),
    ] {
        let mut item = issue(id, title);
        item.priority = Priority(priority);
        item.created_at += Duration::days(day);
        item.updated_at = item.created_at;
        item.description = description.map(str::to_string);
        item.notes = Some(format!("Full notes for {id}").repeat(128));
        item.design = Some("Full design".repeat(128));
        item.acceptance_criteria = Some("- [ ] Still exact\r\n".to_string());
        if closed {
            item.status = Status::Closed;
            item.closed_at = Some(item.updated_at);
        }
        storage.create_issue(&item, "tester").unwrap();
        storage.add_label(id, "selected", "tester").unwrap();
    }
    for body in ["CAFÉ old evidence", "café duplicate hit", "Newest unrelated note"] {
        storage.add_comment("bd-c", "tester", body).unwrap();
    }
    storage
}

#[test]
fn default_unicode_pages_match_full_records_after_matching_and_ordering() {
    let storage = fixture();
    let filters = ListFilters::default();
    let full = storage
        .search_unicode_issues_unpaginated("café", &filters)
        .unwrap();
    // Priority, descending creation time, then ASCENDING ID for equal ranks.
    // The high-priority control does not consume any offset.
    assert_eq!(ids(&full), ["bd-b", "bd-c", "bd-d", "bd-a"]);
    for (offset, limit) in [
        (0, None),
        (0, Some(0)),
        (0, Some(1)),
        (1, Some(2)),
        (3, Some(2)),
        (4, Some(1)),
        (5, Some(0)),
        (usize::MAX, Some(usize::MAX)),
    ] {
        let page = storage
            .search_unicode_issues_default_page(
                " CaFé ",
                &ListFilters {
                    offset: Some(offset),
                    limit,
                    ..filters.clone()
                },
            )
            .unwrap();
        let expected = full
            .iter()
            .skip(offset)
            .take(limit.filter(|value| *value > 0).unwrap_or(usize::MAX))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            serde_json::to_value(&page).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "offset={offset}, limit={limit:?}"
        );
        assert!(page.iter().all(|item| item.comments.is_empty()));
        assert!(page.iter().all(|item| item.notes.is_some() && item.design.is_some()));
    }
}

#[test]
fn explicit_default_sort_and_sql_filter_fallbacks_preserve_page_membership() {
    let storage = fixture();
    for filters in [
        ListFilters {
            sort: Some("priority".to_string()),
            ..ListFilters::default()
        },
        ListFilters {
            labels: Some(vec!["selected".to_string()]),
            priorities: Some(vec![Priority::HIGH]),
            ..ListFilters::default()
        },
        ListFilters {
            statuses: Some(vec![Status::Closed]),
            include_closed: true,
            ..ListFilters::default()
        },
        ListFilters {
            labels: Some(vec!["no-such-label".to_string()]),
            ..ListFilters::default()
        },
    ] {
        let full = storage
            .search_unicode_issues_unpaginated("CAFÉ", &filters)
            .unwrap();
        for offset in [0, 1, 9] {
            let page = storage
                .search_unicode_issues_default_page(
                    "café",
                    &ListFilters {
                        offset: Some(offset),
                        limit: Some(2),
                        ..filters.clone()
                    },
                )
                .unwrap();
            let expected = full.iter().skip(offset).take(2).cloned().collect::<Vec<_>>();
            assert_eq!(
                serde_json::to_value(page).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
    }
}

#[test]
fn page_hydrates_only_selected_rows_and_stops_later_comment_batches() {
    let matcher = RegexBuilder::new("café").case_insensitive(true).build().unwrap();
    let candidates = (0..SEARCH_BATCH_SIZE * 3)
        .map(|index| {
            issue(
                &format!("bd-{index:04}"),
                if index % 4 == 0 { "CAFÉ" } else { "Unrelated" },
            )
        })
        .collect::<Vec<_>>();
    let mut comment_reads = Vec::new();
    let selected = select_matching_window(candidates, &matcher, 70, 3, |batch| {
        assert!(batch.len() <= SEARCH_BATCH_SIZE);
        comment_reads.push(batch.to_vec());
        Ok(HashMap::new())
    })
    .unwrap();
    assert_eq!(ids(&selected), ["bd-0280", "bd-0284", "bd-0288"]);
    assert_eq!(comment_reads.len(), 2, "the third batch must not be read");
    assert!(comment_reads.iter().flatten().all(|id| {
        let index = id.strip_prefix("bd-").unwrap().parse::<usize>().unwrap();
        index < SEARCH_BATCH_SIZE * 2 && index % 4 != 0
    }));
    let mut hydration_reads = Vec::new();
    let hydrated = hydrate_matches(&selected, |batch| {
        hydration_reads.push(batch.to_vec());
        Ok(batch.iter().rev().map(|id| issue(id, "CAFÉ")).collect())
    })
    .unwrap();
    assert_eq!(ids(&hydrated), ids(&selected));
    assert_eq!(hydration_reads, vec![vec![
        "bd-0280".to_string(), "bd-0284".to_string(), "bd-0288".to_string(),
    ]]);
}

#[test]
fn unlimited_and_out_of_range_windows_do_not_overflow_or_invent_rows() {
    let matcher = Regex::new("CAFÉ").unwrap();
    for (offset, limit, expected) in [(1, 0, 2), (0, usize::MAX, 3), (usize::MAX, 1, 0)] {
        let selected = select_matching_window(
            vec![issue("bd-a", "CAFÉ"), issue("bd-b", "CAFÉ"), issue("bd-c", "CAFÉ")],
            &matcher,
            offset,
            limit,
            |_| panic!("field hits must not read comments"),
        )
        .unwrap();
        assert_eq!(selected.len(), expected);
    }
    let empty = select_matching_window(Vec::new(), &matcher, 0, 1, |_| {
        panic!("empty corpus must not read comments")
    })
    .unwrap();
    assert!(hydrate_matches(&empty, |_| panic!("empty page must not hydrate")).unwrap().is_empty());
}

#[test]
fn comment_failures_and_unexpected_rows_fail_before_a_page_is_returned() {
    let matcher = Regex::new("CAFÉ").unwrap();
    let candidates = vec![issue("bd-a", "CAFÉ"), issue("bd-b", "Needs comments")];
    assert!(select_matching_window(candidates.clone(), &matcher, 0, 2, |_| {
        Err(BeadsError::Internal { message: "comment read failed".to_string() })
    }).is_err());
    assert!(select_matching_window(candidates, &matcher, 0, 2, |_| {
        Ok(HashMap::from([("bd-unrequested".to_string(), Vec::new())]))
    }).is_err());
}

#[test]
fn hydration_rejects_changed_ranking_or_membership_fields() {
    let candidate = issue("bd-a", "CAFÉ");
    for change in 0..4 {
        let mut changed = candidate.clone();
        match change {
            0 => changed.created_at += Duration::seconds(1),
            1 => changed.updated_at += Duration::seconds(1),
            2 => changed.status = Status::Deferred,
            _ => changed.issue_type = IssueType::Bug,
        }
        assert!(hydrate_matches(std::slice::from_ref(&candidate), |_| {
            Ok(vec![changed.clone()])
        }).is_err(), "changed field {change}");
    }
}

#[test]
fn default_page_api_rejects_unsupported_order_and_invalid_queries() {
    let storage = SqliteStorage::open_memory().unwrap();
    for filters in [
        ListFilters { reverse: true, ..ListFilters::default() },
        ListFilters { sort: Some("title".to_string()), ..ListFilters::default() },
    ] {
        assert!(matches!(
            storage.search_unicode_issues_default_page("café", &filters),
            Err(BeadsError::Validation { field, .. }) if field == "sort"
        ));
    }
    for query in ["", " \r\n\t"] {
        assert!(matches!(
            storage.search_unicode_issues_default_page(query, &ListFilters::default()),
            Err(BeadsError::Validation { field, .. }) if field == "query"
        ));
    }
}
