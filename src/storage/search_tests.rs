//! Storage-backed and fault-injection coverage for narrow Unicode discovery.

use super::*;
use crate::model::{Priority, Status};
use chrono::{TimeZone, Utc};

fn issue(id: &str, title: &str, description: Option<&str>) -> Issue {
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    Issue {
        id: id.to_string(),
        title: title.to_string(),
        description: description.map(str::to_string),
        created_at: now,
        updated_at: now,
        ..Issue::default()
    }
}

fn ids(issues: &[Issue]) -> Vec<&str> {
    issues.iter().map(|issue| issue.id.as_str()).collect()
}

#[test]
fn narrow_candidates_skip_unrelated_fields_but_results_preserve_full_records() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let mut field = issue("bd-a", "CAFÉ title", None);
    field.design = Some("Design details".repeat(512));
    field.notes = Some("Required by later client filters".repeat(512));
    field.acceptance_criteria = Some("- [ ] Exact criteria\r\n".repeat(512));
    field.prerequisites = Some("- [x] Ready to start".to_string());
    field.owner = Some("owner".to_string());
    field.sender = Some("cli".to_string());
    let mut control = field.clone();
    control.id = "bd-control".to_string();
    control.title = "Not a match".to_string();
    for item in [
        field,
        issue("bd-b", "Description", Some("café evidence")),
        issue("bd-c", "Comment history", None),
        control,
    ] {
        storage.create_issue(&item, "tester").unwrap();
    }
    storage
        .add_comment("bd-c", "tester", "CAFÉ old handoff")
        .unwrap();
    storage
        .add_comment("bd-c", "tester", "café repeated hit")
        .unwrap();
    storage
        .add_comment("bd-c", "tester", "Newest nonmatching handoff")
        .unwrap();

    let filters = ListFilters::default();
    let candidates = storage.unicode_search_candidates("café", &filters).unwrap();
    assert_eq!(ids(&candidates), vec!["bd-a", "bd-b", "bd-c"]);
    for candidate in &candidates {
        assert!(candidate.design.is_none());
        assert!(candidate.notes.is_none());
        assert!(candidate.acceptance_criteria.is_none());
        assert!(candidate.prerequisites.is_none());
        assert!(candidate.owner.is_none());
        assert!(candidate.sender.is_none());
    }
    for query in ["café", "CAFÉ", " CaFé "] {
        let actual = storage
            .search_unicode_issues_unpaginated(query, &filters)
            .unwrap();
        assert_eq!(ids(&actual), vec!["bd-a", "bd-b", "bd-c"]);
        assert!(actual.iter().all(|item| item.comments.is_empty()));
        for item in actual {
            let expected = storage.get_issue(&item.id).unwrap().unwrap();
            assert_eq!(
                serde_json::to_value(item).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
        assert_eq!(
            storage
                .count_unicode_search_matches_unpaginated(query, &filters)
                .unwrap(),
            3
        );
    }
}

#[test]
fn unicode_scan_preserves_filters_and_ignores_pagination_before_matching() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    for (id, status, template, label) in [
        ("bd-open", Status::Open, false, "selected"),
        ("bd-closed", Status::Closed, false, "selected"),
        ("bd-other", Status::Closed, false, "other"),
        ("bd-template", Status::Closed, true, "selected"),
    ] {
        let mut item = issue(id, "CAFÉ", Some("body"));
        item.status = status;
        item.is_template = template;
        item.priority = Priority::HIGH;
        if item.status == Status::Closed {
            item.closed_at = Some(item.updated_at);
        }
        storage.create_issue(&item, "tester").unwrap();
        storage.add_label(id, label, "tester").unwrap();
    }
    // A label makes the underlying narrow projection use its full-row fallback.
    // Correct filtering still wins over the optimization in that case.
    let filters = ListFilters {
        statuses: Some(vec![Status::Closed]),
        include_closed: true,
        include_templates: false,
        labels: Some(vec!["selected".to_string()]),
        priorities: Some(vec![Priority::HIGH]),
        limit: Some(1),
        offset: Some(99),
        ..ListFilters::default()
    };
    let result = storage
        .search_unicode_issues_unpaginated("café", &filters)
        .unwrap();
    assert_eq!(ids(&result), vec!["bd-closed"]);
    assert_eq!(
        storage
            .count_unicode_search_matches_unpaginated("CAFÉ", &filters)
            .unwrap(),
        1
    );
    let no_match = ListFilters {
        priorities: Some(vec![Priority::CRITICAL]),
        ..filters
    };
    assert!(
        storage
            .search_unicode_issues_unpaginated("café", &no_match)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn literal_queries_and_empty_queries_have_explicit_behavior() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    for item in [
        issue("bd-literal", "CAFÉ.[X]%_", None),
        issue("bd-lookalike", "CAFÉZxanythingQ", None),
        issue("bd-greek", "Σ", None),
    ] {
        storage.create_issue(&item, "tester").unwrap();
    }
    let filters = ListFilters::default();
    assert_eq!(
        ids(&storage
            .search_unicode_issues_unpaginated("café.[x]%_", &filters)
            .unwrap()),
        vec!["bd-literal"]
    );
    assert_eq!(
        ids(&storage
            .search_unicode_issues_unpaginated("ς", &filters)
            .unwrap()),
        vec!["bd-greek"]
    );
    for query in ["", " \t\r\n"] {
        assert!(matches!(
            storage.search_unicode_issues_unpaginated(query, &filters),
            Err(BeadsError::Validation { .. })
        ));
        assert!(
            storage
                .count_unicode_search_matches_unpaginated(query, &filters)
                .is_err()
        );
    }
}

#[test]
fn hydration_is_bounded_restores_order_and_skips_empty_input() {
    for count in [0, 1, SEARCH_BATCH_SIZE, SEARCH_BATCH_SIZE + 1, 777] {
        let candidates = (0..count)
            .rev()
            .map(|index| issue(&format!("bd-{index}"), "CAFÉ", None))
            .collect::<Vec<_>>();
        let mut calls = Vec::new();
        let actual = hydrate_matches(&candidates, |batch| {
            calls.push(batch.len());
            assert!(batch.len() <= SEARCH_BATCH_SIZE);
            Ok(batch
                .iter()
                .rev()
                .map(|id| {
                    let mut item = issue(id, "CAFÉ", None);
                    item.notes = Some(format!("notes for {id}"));
                    item
                })
                .collect())
        })
        .unwrap();
        assert_eq!(ids(&actual), ids(&candidates));
        assert_eq!(calls.len(), count.div_ceil(SEARCH_BATCH_SIZE));
        assert_eq!(calls.iter().sum::<usize>(), count);
        assert!(
            actual
                .iter()
                .all(|item| item.notes == Some(format!("notes for {}", item.id)))
        );
    }
}

#[test]
fn failed_or_inconsistent_hydration_never_becomes_a_successful_partial_page() {
    let candidates = vec![issue("bd-a", "CAFÉ", None), issue("bd-b", "CAFÉ", None)];
    assert!(
        hydrate_matches(&candidates, |_| {
            Err(BeadsError::Internal {
                message: "read failed".to_string(),
            })
        })
        .is_err()
    );
    for rows in [
        vec![candidates[0].clone()],
        vec![
            candidates[0].clone(),
            candidates[0].clone(),
            candidates[1].clone(),
        ],
        vec![
            candidates[0].clone(),
            candidates[1].clone(),
            issue("bd-extra", "CAFÉ", None),
        ],
        vec![
            candidates[0].clone(),
            issue("bd-b", "Changed to unrelated text", None),
        ],
        vec![
            candidates[0].clone(),
            issue("bd-b", "CAFÉ", Some("Changed description")),
        ],
    ] {
        assert!(hydrate_matches(&candidates, |_| Ok(rows.clone())).is_err());
    }
}
