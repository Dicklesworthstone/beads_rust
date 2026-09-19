//! Explicit column selection for structured list output.
//!
//! Selection changes columns, never the matching corpus or page boundaries.
//! Values use the model's serializers; selected absent optional fields are null.

use crate::error::{BeadsError, Result};
use crate::format::IssueWithCounts;
use crate::model::Issue;
use crate::output::{JsonArrayPageMeta, OutputContext};
use crate::storage::SqliteStorage;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use std::collections::HashMap;

const ALLOWED_FIELDS: &[&str] = &[
    "id",
    "title",
    "status",
    "priority",
    "issue_type",
    "assignee",
    "owner",
    "created_at",
    "updated_at",
    "created_by",
    "description",
    "design",
    "acceptance_criteria",
    "prerequisites",
    "notes",
    "closed_at",
    "close_reason",
    "due_at",
    "defer_until",
    "estimated_minutes",
    "external_ref",
    "source_repo",
    "source_repo_path",
    "labels",
    "dependency_count",
    "dependent_count",
];

#[derive(Debug)]
pub(super) struct FieldSelection {
    fields: Vec<&'static str>,
}

impl FieldSelection {
    pub(super) fn parse(raw: &str) -> Result<Self> {
        let mut fields = Vec::new();
        for token in raw.split(',') {
            let token = token.trim();
            let field = ALLOWED_FIELDS
                .iter()
                .copied()
                .find(|field| *field == token)
                .ok_or_else(|| BeadsError::Validation {
                    field: "fields".to_string(),
                    reason: format!(
                        "unknown or empty list field '{token}'; choose from: {}",
                        ALLOWED_FIELDS.join(",")
                    ),
                })?;
            if !fields.contains(&field) {
                fields.push(field);
            }
        }
        Ok(Self { fields })
    }

    /// Only columns guaranteed by the existing short-text projection may use it.
    /// All other selections retain full records; client-only filters also force
    /// full records in the caller so an omitted column cannot hide a match.
    pub(super) fn can_use_text_rows(&self) -> bool {
        self.fields.iter().all(|field| {
            matches!(
                *field,
                "id" | "title"
                    | "status"
                    | "priority"
                    | "issue_type"
                    | "labels"
                    | "dependency_count"
                    | "dependent_count"
            )
        })
    }

    fn needs_labels(&self) -> bool {
        self.fields.contains(&"labels")
    }

    fn needs_counts(&self) -> bool {
        self.fields.contains(&"dependency_count") || self.fields.contains(&"dependent_count")
    }

    fn project(&self, issue: IssueWithCounts) -> SelectedIssue<'_> {
        SelectedIssue {
            selection: self,
            issue,
        }
    }

    pub(super) fn render(
        &self,
        ctx: &OutputContext,
        storage: &SqliteStorage,
        issues: Vec<Issue>,
        meta: JsonArrayPageMeta,
        toon: bool,
        stats: bool,
    ) -> Result<()> {
        // Read all requested relation metadata before starting stdout. Failures
        // must not leave a successful-looking partially serialized page.
        let mut labels = HashMap::new();
        let mut dependencies = HashMap::new();
        let mut dependents = HashMap::new();
        if !issues.is_empty() && (self.needs_labels() || self.needs_counts()) {
            let ids = issues
                .iter()
                .map(|issue| issue.id.clone())
                .collect::<Vec<_>>();
            if self.needs_labels() {
                labels = storage.get_labels_for_issues(&ids)?;
            }
            if self.needs_counts() {
                (dependencies, dependents) = storage.count_relation_counts_for_issues(&ids)?;
            }
        }
        let rows = issues.into_iter().map(|mut issue| {
            if self.needs_labels() {
                issue.labels = labels.remove(&issue.id).unwrap_or_default();
            }
            let dependency_count = dependencies.get(&issue.id).copied().unwrap_or(0);
            let dependent_count = dependents.get(&issue.id).copied().unwrap_or(0);
            self.project(IssueWithCounts {
                issue,
                dependency_count,
                dependent_count,
            })
        });
        if toon {
            let page = SelectedPage {
                issues: rows.collect(),
                total: meta.total,
                limit: meta.limit,
                offset: meta.offset,
                has_more: meta.has_more,
            };
            ctx.toon_with_stats(&page, stats);
        } else {
            // Do not serialize full rows to intermediate JSON values and then
            // remove keys: long unselected text must never reach the serializer.
            ctx.json_array_page("issues", rows, meta);
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct SelectedPage<'a> {
    issues: Vec<SelectedIssue<'a>>,
    total: usize,
    limit: usize,
    offset: usize,
    has_more: bool,
}

struct SelectedIssue<'a> {
    selection: &'a FieldSelection,
    issue: IssueWithCounts,
}

impl Serialize for SelectedIssue<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.selection.fields.len()))?;
        let issue = &self.issue.issue;
        for &field in &self.selection.fields {
            match field {
                "id" => map.serialize_entry(field, &issue.id)?,
                "title" => map.serialize_entry(field, &issue.title)?,
                "status" => map.serialize_entry(field, &issue.status)?,
                "priority" => map.serialize_entry(field, &issue.priority)?,
                "issue_type" => map.serialize_entry(field, &issue.issue_type)?,
                "assignee" => map.serialize_entry(field, &issue.assignee)?,
                "owner" => map.serialize_entry(field, &issue.owner)?,
                "created_at" => map.serialize_entry(field, &issue.created_at)?,
                "updated_at" => map.serialize_entry(field, &issue.updated_at)?,
                "created_by" => map.serialize_entry(field, &issue.created_by)?,
                "description" => map.serialize_entry(field, &issue.description)?,
                "design" => map.serialize_entry(field, &issue.design)?,
                "acceptance_criteria" => map.serialize_entry(field, &issue.acceptance_criteria)?,
                "prerequisites" => map.serialize_entry(field, &issue.prerequisites)?,
                "notes" => map.serialize_entry(field, &issue.notes)?,
                "closed_at" => map.serialize_entry(field, &issue.closed_at)?,
                "close_reason" => map.serialize_entry(field, &issue.close_reason)?,
                "due_at" => map.serialize_entry(field, &issue.due_at)?,
                "defer_until" => map.serialize_entry(field, &issue.defer_until)?,
                "estimated_minutes" => map.serialize_entry(field, &issue.estimated_minutes)?,
                "external_ref" => map.serialize_entry(field, &issue.external_ref)?,
                "source_repo" => map.serialize_entry(field, &issue.source_repo)?,
                "source_repo_path" => map.serialize_entry(field, &issue.source_repo_path)?,
                "labels" => map.serialize_entry(field, &issue.labels)?,
                "dependency_count" => map.serialize_entry(field, &self.issue.dependency_count)?,
                "dependent_count" => map.serialize_entry(field, &self.issue.dependent_count)?,
                _ => return Err(serde::ser::Error::custom("invalid list projection field")),
            }
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IssueType, Priority, Status};
    use crate::storage::ListFilters;
    use serde_json::json;

    fn row(issue: Issue) -> IssueWithCounts {
        IssueWithCounts {
            issue,
            dependency_count: 2,
            dependent_count: 3,
        }
    }

    #[test]
    fn selection_is_strict_and_deduplicates_in_requested_order() {
        for raw in [
            "",
            " ",
            ",",
            "id,",
            ",id",
            "id,,title",
            "titel",
            "type",
            "ID",
        ] {
            assert!(
                matches!(
                    FieldSelection::parse(raw),
                    Err(BeadsError::Validation { field, .. }) if field == "fields"
                ),
                "{raw:?}"
            );
        }
        let fields = FieldSelection::parse(" title, id,title ,priority ").unwrap();
        assert_eq!(fields.fields, vec!["title", "id", "priority"]);
        let issue = Issue {
            id: "bd-1".into(),
            title: "A title".into(),
            ..Issue::default()
        };
        assert_eq!(
            serde_json::to_string(&fields.project(row(issue))).unwrap(),
            r#"{"title":"A title","id":"bd-1","priority":2}"#
        );
    }

    #[test]
    fn selected_values_keep_types_escaping_nulls_and_relation_counts() {
        let fields = FieldSelection::parse(
            "id,title,status,priority,issue_type,assignee,labels,dependency_count,dependent_count",
        )
        .unwrap();
        let issue = Issue {
            id: "bd-1".into(),
            title: "CAFÉ \"quoted\"\nline".into(),
            status: Status::InProgress,
            priority: Priority::HIGH,
            issue_type: IssueType::Bug,
            labels: vec!["backend".into()],
            notes: Some("must not be serialized".repeat(1024)),
            ..Issue::default()
        };
        assert_eq!(
            serde_json::to_value(fields.project(row(issue))).unwrap(),
            json!({
                "id": "bd-1", "title": "CAFÉ \"quoted\"\nline", "status": "in_progress",
                "priority": 1, "issue_type": "bug", "assignee": null, "labels": ["backend"],
                "dependency_count": 2, "dependent_count": 3
            })
        );
    }

    #[test]
    fn every_supported_field_has_a_serializer_and_full_row_value_parity() {
        let fields = FieldSelection::parse(&ALLOWED_FIELDS.join(",")).unwrap();
        let issue = Issue {
            id: "bd-values".into(),
            title: "All columns".into(),
            description: Some("Description".into()),
            design: Some("Design".into()),
            acceptance_criteria: Some("- [ ] Exact criterion\r\n".into()),
            prerequisites: Some("Ready".into()),
            notes: Some("Notes".into()),
            assignee: Some("alice".into()),
            owner: Some("bob".into()),
            created_by: Some("tester".into()),
            close_reason: Some("Reason".into()),
            estimated_minutes: Some(42),
            external_ref: Some("external".into()),
            source_repo: Some("project".into()),
            source_repo_path: Some("/project".into()),
            labels: vec!["tag".into()],
            ..Issue::default()
        };
        let original = row(issue);
        let full = serde_json::to_value(&original).unwrap();
        let selected = serde_json::to_value(fields.project(original)).unwrap();
        assert_eq!(selected.as_object().unwrap().len(), ALLOWED_FIELDS.len());
        for &field in ALLOWED_FIELDS {
            assert_eq!(selected[field], full[field], "{field}");
            assert!(
                selected.get(field).is_some(),
                "selected absent field must be null: {field}"
            );
        }
    }

    #[test]
    fn narrow_projection_is_conservative_and_relations_are_opt_in() {
        let basic = FieldSelection::parse("id,title,status,priority,issue_type").unwrap();
        assert!(basic.can_use_text_rows());
        assert!(!basic.needs_labels());
        assert!(!basic.needs_counts());
        let related = FieldSelection::parse("id,labels,dependency_count,dependent_count").unwrap();
        assert!(related.can_use_text_rows());
        assert!(related.needs_labels());
        assert!(related.needs_counts());
        for field in ALLOWED_FIELDS {
            let selection = FieldSelection::parse(field).unwrap();
            if !matches!(
                *field,
                "id" | "title"
                    | "status"
                    | "priority"
                    | "issue_type"
                    | "labels"
                    | "dependency_count"
                    | "dependent_count"
            ) {
                assert!(!selection.can_use_text_rows(), "{field}");
            }
        }
    }

    #[test]
    fn storage_text_projection_preserves_selected_columns_and_row_order() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        for (id, priority, description) in [
            ("bd-a", Priority::LOW, "Long A"),
            ("bd-b", Priority::HIGH, "Long B"),
        ] {
            storage
                .create_issue(
                    &Issue {
                        id: id.into(),
                        title: format!("Title {id}"),
                        priority,
                        description: Some(description.repeat(512)),
                        notes: Some("Long notes".repeat(512)),
                        ..Issue::default()
                    },
                    "tester",
                )
                .unwrap();
        }
        let fields = FieldSelection::parse("id,title,status,priority,issue_type").unwrap();
        // `include_deferred: true` is load-bearing, not incidental.
        // `list_text_issues_for_command_output` falls back to `list_issues`
        // whenever the filter shape leaves its narrow path, and
        // `!filters.include_deferred` is one of those conditions: the narrow
        // SQL filters only closed and tombstone rows, so it would wrongly keep
        // deferred issues when the caller asked to exclude them. With the
        // `ListFilters::default()` value of `false`, both cases below take the
        // fallback and return fully hydrated rows, so the projection this test
        // exists to check is never exercised and the assertion below fails on
        // the hydrated `description`.
        for filters in [
            ListFilters {
                include_deferred: true,
                ..ListFilters::default()
            },
            ListFilters {
                limit: Some(1),
                offset: Some(1),
                include_deferred: true,
                ..ListFilters::default()
            },
        ] {
            let full = storage.list_issues(&filters).unwrap();
            let narrow = storage
                .list_text_issues_for_command_output(&filters)
                .unwrap();
            assert!(
                narrow
                    .iter()
                    .all(|issue| issue.description.is_none() && issue.notes.is_none())
            );
            let selected = |issues: Vec<Issue>| {
                issues
                    .into_iter()
                    .map(|issue| serde_json::to_value(fields.project(row(issue))).unwrap())
                    .collect::<Vec<_>>()
            };
            assert_eq!(selected(full), selected(narrow));
        }
    }
}
