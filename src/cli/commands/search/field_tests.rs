//! Search selection must change columns, never the matching work surface.

use super::*;
use chrono::{Duration, TimeZone};
use serde_json::{Value, json};

fn insert(storage: &mut SqliteStorage, number: i64, description: &str, notes: &str) -> String {
    let id = format!("bd-s{number:03}");
    let created = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap() + Duration::minutes(number);
    storage
        .create_issue(
            &Issue {
                id: id.clone(),
                title: format!("Record {number:03}"),
                description: Some(description.to_string()),
                notes: Some(notes.to_string()),
                priority: Priority(2),
                created_at: created,
                updated_at: created,
                ..Issue::default()
            },
            "tester",
        )
        .unwrap();
    id
}

fn as_json(storage: &SqliteStorage, query: &str, args: &ListArgs, format: OutputFormat) -> Value {
    let page = collect_search_results_for_output(storage, query, args, format).unwrap();
    let hidden_closed_count = count_hidden_closed_matches(storage, query, args).unwrap();
    if let Some(selection) = page.selection {
        serde_json::to_value(SelectedSearchResults {
            issues: selection.rows(storage, page.issues).unwrap().collect(),
            hidden_closed_count,
            limit: page.limit,
            offset: page.offset,
            has_more: page.has_more,
        })
        .unwrap()
    } else {
        let issues = attach_counts(storage, page.issues).unwrap();
        serde_json::to_value(SearchResults {
            issues: &issues,
            hidden_closed_count,
            limit: page.limit,
            offset: page.offset,
            has_more: page.has_more,
        })
        .unwrap()
    }
}

fn assert_projection(full: &Value, selected: &Value, fields: &str) {
    let mut expected = full.clone();
    expected["issues"] = Value::Array(
        full["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let mut projected = serde_json::Map::new();
                for field in fields.split(',').map(str::trim) {
                    let value = row.get(field).cloned().unwrap_or_else(|| {
                        if field == "labels" {
                            json!([])
                        } else {
                            Value::Null
                        }
                    });
                    projected.insert(field.to_string(), value);
                }
                Value::Object(projected)
            })
            .collect(),
    );
    // Comparing whole objects detects lost/new envelope keys as well as rows.
    assert_eq!(selected, &expected);
    assert!(selected.get("total").is_none(), "search is not a list page");
}

#[test]
fn search_fields_preserve_body_and_historical_comment_matches() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let body = insert(&mut storage, 1, "needle body", "Unselected notes");
    let comment = insert(&mut storage, 2, "unrelated", "Other notes");
    insert(&mut storage, 3, "negative control", "No match");
    storage
        .add_comment(&comment, "tester", "NEEDLE in an old comment")
        .unwrap();
    storage
        .add_comment(&comment, "tester", "Recent unrelated comment")
        .unwrap();
    let args = ListArgs {
        limit: Some(0),
        sort: Some("title".into()),
        ..ListArgs::default()
    };
    let full = as_json(&storage, "needle", &args, OutputFormat::Json);
    assert_eq!(full["issues"].as_array().unwrap().len(), 2);
    assert_eq!(full["issues"][0]["id"], body);
    assert_eq!(full["issues"][1]["id"], comment);
    for format in [OutputFormat::Json, OutputFormat::Toon] {
        for fields in [
            "id,title,status,priority,issue_type",
            " title,id,title ",
            "id,description,notes,assignee,created_at,updated_at",
            "id,labels,dependency_count,dependent_count",
        ] {
            let selected = as_json(
                &storage,
                "needle",
                &ListArgs {
                    fields: Some(fields.into()),
                    ..args.clone()
                },
                format,
            );
            assert_projection(&full, &selected, fields);
        }
    }
    assert_eq!(full, as_json(&storage, "needle", &args, OutputFormat::Json));
}

#[test]
fn search_fields_preserve_client_filters_sort_and_pagination() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    insert(&mut storage, 0, "needle but excluded", "skip");
    for number in 1..5 {
        insert(&mut storage, number, "needle KEEP", "CAFÉ handoff");
    }
    for sort in [
        None,
        Some("priority"),
        Some("title"),
        Some("created"),
        Some("updated"),
    ] {
        for reverse in [false, true] {
            for offset in [0, 1, 3, 4, 99] {
                let args = ListArgs {
                    desc_contains: Some("keep".into()),
                    notes_contains: Some("café".into()),
                    sort: sort.map(str::to_string),
                    reverse,
                    limit: Some(1),
                    offset: Some(offset),
                    ..ListArgs::default()
                };
                let full = as_json(&storage, "needle", &args, OutputFormat::Json);
                let selected = as_json(
                    &storage,
                    "needle",
                    &ListArgs {
                        fields: Some("id,title".into()),
                        ..args
                    },
                    OutputFormat::Json,
                );
                assert_projection(&full, &selected, "id,title");
                assert_eq!(selected["has_more"], offset < 3);
                assert_eq!(
                    selected["issues"].as_array().unwrap().len(),
                    if offset < 4 { 1 } else { 0 }
                );
            }
        }
    }
}

#[test]
fn search_fields_preserve_unicode_history_and_literal_punctuation() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let body = insert(&mut storage, 1, "CAFÉ.[X]%_ in the body", "note");
    let comment = insert(&mut storage, 2, "unrelated", "note");
    insert(&mut storage, 3, "CAFE no accent and no punctuation", "note");
    storage
        .add_comment(&comment, "tester", "CAFÉ.[X]%_ older evidence")
        .unwrap();
    storage
        .add_comment(&comment, "tester", "Recent unrelated handoff")
        .unwrap();
    let closed = Issue {
        id: "bd-history".into(),
        title: "Archived evidence".into(),
        description: Some("CAFÉ.[X]%_ archived body".into()),
        status: Status::Closed,
        created_at: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
        updated_at: Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap(),
        closed_at: Some(Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap()),
        ..Issue::default()
    };
    storage.create_issue(&closed, "tester").unwrap();
    for query in ["café.[x]%_", "CAFÉ.[X]%_"] {
        for (limit, offset, all) in [(1, 0, false), (1, 1, false), (1, 2, false), (0, 0, true)] {
            let args = ListArgs {
                limit: Some(limit),
                offset: Some(offset),
                all,
                sort: Some("title".into()),
                ..ListArgs::default()
            };
            let full = as_json(&storage, query, &args, OutputFormat::Json);
            let selected = as_json(
                &storage,
                query,
                &ListArgs {
                    fields: Some("id,status,description".into()),
                    ..args
                },
                OutputFormat::Json,
            );
            assert_projection(&full, &selected, "id,status,description");
            assert_eq!(selected["hidden_closed_count"], if all { 0 } else { 1 });
            if all {
                let ids = selected["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| row["id"].as_str().unwrap())
                    .collect::<HashSet<_>>();
                assert_eq!(
                    ids,
                    HashSet::from([body.as_str(), comment.as_str(), closed.id.as_str()])
                );
            }
        }
    }
}

#[test]
fn search_fields_do_not_consume_or_emit_the_extra_page_probe() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    for number in 0..53 {
        insert(&mut storage, number, "needle", "LONG_UNSELECTED_NOTE");
    }
    for (limit, offset, shown, more) in [
        (None, 0, 50, true),
        (Some(0), 0, 53, false),
        (Some(2), 50, 2, true),
        (Some(2), 52, 1, false),
        (Some(0), 51, 2, false),
        (Some(1), 99, 0, false),
    ] {
        let args = ListArgs {
            limit,
            offset: Some(offset),
            sort: Some("title".into()),
            ..ListArgs::default()
        };
        let full = as_json(&storage, "needle", &args, OutputFormat::Json);
        let selected = as_json(
            &storage,
            "needle",
            &ListArgs {
                fields: Some("id,title".into()),
                ..args
            },
            OutputFormat::Json,
        );
        assert_projection(&full, &selected, "id,title");
        assert_eq!(selected["issues"].as_array().unwrap().len(), shown);
        assert_eq!(selected["has_more"], more);
        assert!(!selected.to_string().contains("LONG_UNSELECTED_NOTE"));
    }
}

#[test]
fn search_fields_validate_empty_corpora_and_ignore_selection_in_legacy_formats() {
    let storage = SqliteStorage::open_memory().unwrap();
    for fields in ["", "id,", "id,,title", "unknown", "ID"] {
        let args = ListArgs {
            fields: Some(fields.into()),
            ..ListArgs::default()
        };
        for format in [OutputFormat::Json, OutputFormat::Toon] {
            let result = collect_search_results_for_output(&storage, "no-match", &args, format);
            assert!(matches!(result,
                Err(BeadsError::Validation { field, .. }) if field == "fields"
            ));
        }
        for format in [OutputFormat::Text, OutputFormat::Csv] {
            let page =
                collect_search_results_for_output(&storage, "no-match", &args, format).unwrap();
            assert!(page.selection.is_none());
        }
    }
    let selected = as_json(
        &storage,
        "no-match",
        &ListArgs {
            fields: Some("id,title".into()),
            ..ListArgs::default()
        },
        OutputFormat::Json,
    );
    assert_eq!(
        selected,
        json!({
            "issues": [], "hidden_closed_count": 0,
            "limit": 50, "offset": 0, "has_more": false
        })
    );
}

#[test]
fn search_fields_prepared_rows_own_their_data_and_keep_exact_length() {
    let selection = FieldSelection::parse("id,title,assignee").unwrap();
    let mut rows = {
        let storage = SqliteStorage::open_memory().unwrap();
        selection
            .rows(
                &storage,
                vec![Issue {
                    id: "bd-owned".into(),
                    title: "Owned row".into(),
                    ..Issue::default()
                }],
            )
            .unwrap()
    };
    assert_eq!(rows.len(), 1);
    let row = serde_json::to_value(rows.next().unwrap()).unwrap();
    assert_eq!(
        row,
        json!({"id": "bd-owned", "title": "Owned row", "assignee": null})
    );
    assert_eq!(rows.len(), 0);
    assert!(rows.next().is_none());
    assert!(rows.next().is_none());
}
