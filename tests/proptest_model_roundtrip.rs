//! Property-based tests for model serde round-trip and content_hash stability.
//!
//! Verifies that model enums deserialize case-insensitively, round-trip
//! correctly through JSON, and produce stable content hashes regardless of
//! input casing.

use proptest::prelude::*;
use serde::de::DeserializeOwned;

use beads_rust::model::{DependencyType, EventType, Issue, IssueType, Priority, Status};
use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{ExportConfig, ImportConfig, export_to_jsonl, import_from_jsonl};
use beads_rust::util::{content_hash, content_hash_from_parts};
use chrono::{TimeZone, Utc};
use std::fs;
use tempfile::TempDir;

fn arb_status_name() -> impl Strategy<Value = (&'static str, Status)> {
    prop_oneof![
        Just(("open", Status::Open)),
        Just(("in_progress", Status::InProgress)),
        Just(("blocked", Status::Blocked)),
        Just(("deferred", Status::Deferred)),
        Just(("draft", Status::Draft)),
        Just(("closed", Status::Closed)),
        Just(("tombstone", Status::Tombstone)),
        Just(("pinned", Status::Pinned)),
    ]
}

fn arb_issue_type_name() -> impl Strategy<Value = (&'static str, IssueType)> {
    prop_oneof![
        Just(("task", IssueType::Task)),
        Just(("bug", IssueType::Bug)),
        Just(("feature", IssueType::Feature)),
        Just(("epic", IssueType::Epic)),
        Just(("chore", IssueType::Chore)),
        Just(("docs", IssueType::Docs)),
        Just(("question", IssueType::Question)),
    ]
}

fn case_variants(s: &str) -> Vec<String> {
    vec![
        s.to_string(),
        s.to_uppercase(),
        {
            let mut c = s.chars();
            match c.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
            }
        },
        s.chars()
            .enumerate()
            .map(|(i, c)| {
                if i % 2 == 0 {
                    c.to_uppercase().next().unwrap_or(c)
                } else {
                    c
                }
            })
            .collect(),
    ]
}

fn deserialize_json_string<T>(value: &str) -> T
where
    T: DeserializeOwned,
{
    let json = serde_json::to_string(value).unwrap();
    serde_json::from_str(&json).unwrap()
}

#[derive(Debug)]
struct JsonlCaseVariant {
    status: Status,
    issue_type: IssueType,
    json_status: &'static str,
    json_issue_type: &'static str,
}

fn issue_for_jsonl_case(id: String, status: Status, issue_type: IssueType) -> Issue {
    let created_at = Utc.with_ymd_and_hms(2026, 4, 22, 10, 49, 8).unwrap();
    let closed_at = status.is_terminal().then_some(created_at);

    Issue {
        id,
        title: "Mixed case JSONL sync surface".to_string(),
        description: Some("Status and issue_type should normalize after import".to_string()),
        status,
        priority: Priority::MEDIUM,
        issue_type,
        created_at,
        updated_at: created_at,
        closed_at,
        close_reason: closed_at.map(|_| "closed for case test".to_string()),
        created_by: Some("proptest".to_string()),
        source_repo: Some(".".to_string()),
        ..Issue::default()
    }
}

#[test]
fn jsonl_import_mixed_case_status_issue_type_preserves_hash() {
    let variants = [
        JsonlCaseVariant {
            status: Status::Open,
            issue_type: IssueType::Bug,
            json_status: "Open",
            json_issue_type: "Bug",
        },
        JsonlCaseVariant {
            status: Status::Open,
            issue_type: IssueType::Bug,
            json_status: "OPEN",
            json_issue_type: "BUG",
        },
        JsonlCaseVariant {
            status: Status::Open,
            issue_type: IssueType::Bug,
            json_status: "OpEn",
            json_issue_type: "bUg",
        },
        JsonlCaseVariant {
            status: Status::InProgress,
            issue_type: IssueType::Feature,
            json_status: "INPROGRESS",
            json_issue_type: "FeAtUrE",
        },
        JsonlCaseVariant {
            status: Status::Draft,
            issue_type: IssueType::Docs,
            json_status: "DrAfT",
            json_issue_type: "DoCs",
        },
        JsonlCaseVariant {
            status: Status::Closed,
            issue_type: IssueType::Question,
            json_status: "CLOSED",
            json_issue_type: "QuEsTiOn",
        },
    ];

    for (index, variant) in variants.into_iter().enumerate() {
        let temp = TempDir::new().unwrap();
        let canonical_path = temp.path().join("canonical.jsonl");
        let mixed_path = temp.path().join("mixed.jsonl");
        let reexport_path = temp.path().join("reexport.jsonl");

        let issue = issue_for_jsonl_case(
            format!("bd-mixedcase{index}"),
            variant.status.clone(),
            variant.issue_type.clone(),
        );
        let expected_hash = content_hash(&issue);

        let mut original = SqliteStorage::open_memory().unwrap();
        original.create_issue(&issue, "proptest").unwrap();
        let canonical_export =
            export_to_jsonl(&original, &canonical_path, &ExportConfig::default()).unwrap();

        let canonical_text = fs::read_to_string(&canonical_path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&canonical_text).unwrap();
        value["status"] = serde_json::Value::String(variant.json_status.to_string());
        value["issue_type"] = serde_json::Value::String(variant.json_issue_type.to_string());
        let mixed_text = format!("{}\n", serde_json::to_string(&value).unwrap());
        assert!(mixed_text.contains(variant.json_status));
        assert!(mixed_text.contains(variant.json_issue_type));
        fs::write(&mixed_path, mixed_text).unwrap();

        let mut imported = SqliteStorage::open_memory().unwrap();
        let import_result = import_from_jsonl(
            &mut imported,
            &mixed_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();
        assert_eq!(import_result.imported_count, 1);

        let imported_issue = imported.get_issue(&issue.id).unwrap().unwrap();
        assert_eq!(imported_issue.status, variant.status);
        assert_eq!(imported_issue.issue_type, variant.issue_type);
        assert_eq!(
            imported_issue.content_hash.as_deref(),
            Some(expected_hash.as_str())
        );

        let reexport =
            export_to_jsonl(&imported, &reexport_path, &ExportConfig::default()).unwrap();
        assert_eq!(reexport.content_hash, canonical_export.content_hash);
        assert_eq!(
            reexport
                .issue_hashes
                .iter()
                .find(|(id, _)| id == &issue.id)
                .map(|(_, hash)| hash.as_str()),
            Some(expected_hash.as_str())
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn status_deserialize_matches_lowercase_for_any_string(value in "\\PC{0,64}") {
        let lowered = value.to_lowercase();
        let original: Status = deserialize_json_string(&value);
        let lowercase: Status = deserialize_json_string(&lowered);

        prop_assert_eq!(original, lowercase);
    }

    #[test]
    fn issue_type_deserialize_matches_lowercase_for_any_string(value in "\\PC{0,64}") {
        let lowered = value.to_lowercase();
        let original: IssueType = deserialize_json_string(&value);
        let lowercase: IssueType = deserialize_json_string(&lowered);

        prop_assert_eq!(original, lowercase);
    }

    #[test]
    fn dependency_type_deserialize_matches_lowercase_for_any_string(value in "\\PC{0,64}") {
        let lowered = value.to_lowercase();
        let original: DependencyType = deserialize_json_string(&value);
        let lowercase: DependencyType = deserialize_json_string(&lowered);

        prop_assert_eq!(original, lowercase);
    }

    #[test]
    fn event_type_deserialize_matches_lowercase_for_any_string(value in "\\PC{0,64}") {
        let lowered = value.to_lowercase();
        let original: EventType = deserialize_json_string(&value);
        let lowercase: EventType = deserialize_json_string(&lowered);

        prop_assert_eq!(original, lowercase);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn status_deser_case_insensitive((canonical, expected) in arb_status_name()) {
        for variant in case_variants(canonical) {
            let json = format!("\"{}\"", variant);
            let deserialized: Status = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("failed to deser Status from {json}: {e}"));
            prop_assert_eq!(
                &deserialized, &expected,
                "Status '{}' should deser to {:?}, got {:?}",
                variant, expected, deserialized
            );
        }
    }

    #[test]
    fn issue_type_deser_case_insensitive((canonical, expected) in arb_issue_type_name()) {
        for variant in case_variants(canonical) {
            let json = format!("\"{}\"", variant);
            let deserialized: IssueType = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("failed to deser IssueType from {json}: {e}"));
            prop_assert_eq!(
                &deserialized, &expected,
                "IssueType '{}' should deser to {:?}, got {:?}",
                variant, expected, deserialized
            );
        }
    }

    #[test]
    fn status_roundtrip((_, expected) in arb_status_name()) {
        let serialized = serde_json::to_string(&expected).unwrap();
        let deserialized: Status = serde_json::from_str(&serialized).unwrap();
        prop_assert_eq!(&deserialized, &expected);
    }

    #[test]
    fn issue_type_roundtrip((_, expected) in arb_issue_type_name()) {
        let serialized = serde_json::to_string(&expected).unwrap();
        let deserialized: IssueType = serde_json::from_str(&serialized).unwrap();
        prop_assert_eq!(&deserialized, &expected);
    }

    #[test]
    fn content_hash_stable_across_status_casing(
        (canonical, expected) in arb_status_name(),
        title in "[a-z]{1,20}",
    ) {
        let reference_hash = content_hash_from_parts(
            &title, None, None, None, None,
            &expected, &beads_rust::model::Priority::MEDIUM,
            &IssueType::Task, None, None, None, None, None, false, false,
        );
        for variant in case_variants(canonical) {
            let json = format!("\"{}\"", variant);
            let status: Status = serde_json::from_str(&json).unwrap();
            let hash = content_hash_from_parts(
                &title, None, None, None, None,
                &status, &beads_rust::model::Priority::MEDIUM,
                &IssueType::Task, None, None, None, None, None, false, false,
            );
            prop_assert_eq!(
                &hash, &reference_hash,
                "Hash mismatch for status variant '{}': {} vs {}",
                variant, hash, reference_hash
            );
        }
    }

    #[test]
    fn content_hash_stable_across_issue_type_casing(
        (canonical, expected) in arb_issue_type_name(),
        title in "[a-z]{1,20}",
    ) {
        let reference_hash = content_hash_from_parts(
            &title, None, None, None, None,
            &Status::Open, &beads_rust::model::Priority::MEDIUM,
            &expected, None, None, None, None, None, false, false,
        );
        for variant in case_variants(canonical) {
            let json = format!("\"{}\"", variant);
            let issue_type: IssueType = serde_json::from_str(&json).unwrap();
            let hash = content_hash_from_parts(
                &title, None, None, None, None,
                &Status::Open, &beads_rust::model::Priority::MEDIUM,
                &issue_type, None, None, None, None, None, false, false,
            );
            prop_assert_eq!(
                &hash, &reference_hash,
                "Hash mismatch for issue_type variant '{}': {} vs {}",
                variant, hash, reference_hash
            );
        }
    }

    #[test]
    fn custom_status_normalized_to_lowercase(name in "[A-Za-z_]{3,15}") {
        let known = ["open", "in_progress", "inprogress", "blocked", "deferred",
                      "draft", "closed", "tombstone", "pinned"];
        let normalized = name.to_lowercase();
        prop_assume!(!known.contains(&normalized.as_str()));
        let json = format!("\"{name}\"");
        let status: Status = serde_json::from_str(&json).unwrap();
        match &status {
            Status::Custom(v) => prop_assert_eq!(v, &normalized),
            other => prop_assert!(false, "expected Custom, got {:?}", other),
        }
    }

    #[test]
    fn custom_issue_type_normalized_to_lowercase(name in "[A-Za-z_]{3,15}") {
        let known = ["task", "bug", "feature", "epic", "chore", "docs", "question"];
        let normalized = name.to_lowercase();
        prop_assume!(!known.contains(&normalized.as_str()));
        let json = format!("\"{name}\"");
        let issue_type: IssueType = serde_json::from_str(&json).unwrap();
        match &issue_type {
            IssueType::Custom(v) => prop_assert_eq!(v, &normalized),
            other => prop_assert!(false, "expected Custom, got {:?}", other),
        }
    }

    #[test]
    fn dep_type_deser_case_insensitive(
        (canonical, expected) in prop_oneof![
            Just(("blocks", DependencyType::Blocks)),
            Just(("parent-child", DependencyType::ParentChild)),
            Just(("conditional-blocks", DependencyType::ConditionalBlocks)),
            Just(("waits-for", DependencyType::WaitsFor)),
            Just(("related", DependencyType::Related)),
            Just(("discovered-from", DependencyType::DiscoveredFrom)),
            Just(("replies-to", DependencyType::RepliesTo)),
            Just(("relates-to", DependencyType::RelatesTo)),
            Just(("duplicates", DependencyType::Duplicates)),
            Just(("supersedes", DependencyType::Supersedes)),
            Just(("caused-by", DependencyType::CausedBy)),
        ]
    ) {
        for variant in case_variants(canonical) {
            let json = format!("\"{}\"", variant);
            let deserialized: DependencyType = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("failed to deser DependencyType from {json}: {e}"));
            prop_assert_eq!(
                &deserialized, &expected,
                "DependencyType '{}' should deser to {:?}, got {:?}",
                variant, expected, deserialized
            );
        }
    }

    #[test]
    fn dep_type_roundtrip(
        (_, expected) in prop_oneof![
            Just(("blocks", DependencyType::Blocks)),
            Just(("parent-child", DependencyType::ParentChild)),
            Just(("conditional-blocks", DependencyType::ConditionalBlocks)),
            Just(("waits-for", DependencyType::WaitsFor)),
            Just(("related", DependencyType::Related)),
            Just(("discovered-from", DependencyType::DiscoveredFrom)),
            Just(("replies-to", DependencyType::RepliesTo)),
            Just(("relates-to", DependencyType::RelatesTo)),
            Just(("duplicates", DependencyType::Duplicates)),
            Just(("supersedes", DependencyType::Supersedes)),
            Just(("caused-by", DependencyType::CausedBy)),
        ]
    ) {
        let serialized = serde_json::to_string(&expected).unwrap();
        let deserialized: DependencyType = serde_json::from_str(&serialized).unwrap();
        prop_assert_eq!(&deserialized, &expected);
    }

    #[test]
    fn custom_dep_type_normalized_to_lowercase(name in "[A-Za-z_-]{3,20}") {
        let known = [
            "blocks",
            "parent-child",
            "conditional-blocks",
            "waits-for",
            "related",
            "discovered-from",
            "replies-to",
            "relates-to",
            "duplicates",
            "supersedes",
            "caused-by",
        ];
        let normalized = name.to_lowercase();
        prop_assume!(!known.contains(&normalized.as_str()));
        let json = format!("\"{name}\"");
        let dep_type: DependencyType = serde_json::from_str(&json).unwrap();
        match &dep_type {
            DependencyType::Custom(v) => prop_assert_eq!(v, &normalized),
            other => prop_assert!(false, "expected Custom, got {:?}", other),
        }
    }
}
