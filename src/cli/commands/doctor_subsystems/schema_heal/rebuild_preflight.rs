//! Observational inventory for schema-heal preservation diagnostics.
//!
//! A JSONL replay rehearsal is not a crash-safe database migration. The old
//! implementation installed the reconstructed database before restoring local
//! rows, so interruption could leave authoritative records only in backups.
//! Schema healing now requires the reviewed migration installer. These checks
//! explain which state a JSONL reconstruction could not preserve; they never
//! open a writable database, build a candidate, or authorize replacement.

use std::collections::HashSet;

use crate::error::Result;
use crate::franken_sync::Connection;
use crate::model::{Comment, Issue};

use super::{
    BOOL_FIELDS, DbOnlyReason, INT_FIELDS, STRING_FIELDS, TIMESTAMP_FIELDS, column_names,
    dependency_payload_key, multiset_is_subset, quote_identifier, required_legacy_text,
};

/// A winning JSONL timestamp is sync precedence, not evidence that a schema
/// migration can erase local relations or machine-local provenance. Diagnose
/// records that the historical issue-only replay plan failed to carry.
pub(super) fn audit_local_replay_coverage(
    local: &Issue,
    imported: Option<&Issue>,
    verdict: Option<(DbOnlyReason, bool)>,
    blockers: &mut Vec<String>,
) {
    if local.bypassed_policy.is_some() || local.bypass_reason.is_some() {
        blockers.push(format!(
            "issue {} has local policy attribution without a verified replay path",
            local.id
        ));
    }
    let full_restore =
        matches!(verdict, Some((reason, true)) if reason != DbOnlyReason::CommentsAbsentFromJsonl);
    if full_restore {
        return;
    }
    let Some(imported) = imported else {
        return;
    };
    for (kind, represented) in [
        (
            "labels",
            multiset_is_subset(local.labels.iter(), imported.labels.iter()),
        ),
        (
            "dependencies",
            multiset_is_subset(
                local.dependencies.iter().map(dependency_payload_key),
                imported.dependencies.iter().map(dependency_payload_key),
            ),
        ),
    ] {
        if !represented {
            blockers.push(format!(
                "issue {} has local {kind} absent from its proposed replay",
                local.id
            ));
        }
    }
    let comments_only = verdict == Some((DbOnlyReason::CommentsAbsentFromJsonl, true));
    if !comments_only {
        if local.source_repo_path.is_some() {
            blockers.push(format!(
                "issue {} has local source_repo_path without a verified restore plan",
                local.id
            ));
        }
        if !multiset_is_subset(
            local.comments.iter().map(Comment::sync_key),
            imported.comments.iter().map(Comment::sync_key),
        ) {
            blockers.push(format!(
                "issue {} has local comments absent from its proposed replay",
                local.id
            ));
        }
    }
}

/// Inventory everything outside the issue decoder's replay surface. Matching
/// issue counts cannot prove configuration, history, or extension tables are
/// disposable. This diagnostic runs only for explicit heal/audit; reviewed
/// migrations retain their raw-row preservation witnesses and fast path.
pub(super) fn audit_unrepresented_state(
    conn: &Connection,
    tables: &HashSet<String>,
    issue_columns: &[String],
) -> Result<Vec<String>> {
    let mut blockers = Vec::new();
    for table in tables {
        let columns = if table == "issues" {
            issue_columns.to_vec()
        } else {
            column_names(conn, table)?
        };
        if table == "comments"
            && columns.iter().any(|column| column == "text")
            && columns.iter().any(|column| column == "body")
        {
            blockers.push(
                "comments has both text and body columns; replay cannot preserve both payloads"
                    .to_owned(),
            );
        }
        if let Some(expected) = replayed_columns(table) {
            for column in columns {
                let represented = if table == "issues" {
                    TIMESTAMP_FIELDS.contains(&column.as_str())
                        || BOOL_FIELDS.contains(&column.as_str())
                        || INT_FIELDS.contains(&column.as_str())
                        || STRING_FIELDS.contains(&column.as_str())
                        || column == "content_hash"
                } else {
                    expected.contains(&column.as_str())
                };
                if !represented {
                    blockers.push(format!(
                        "column {table}.{column} has no verified JSONL replay path"
                    ));
                }
            }
        } else {
            let populated = !conn
                .query(&format!(
                    "SELECT 1 FROM {} LIMIT 1",
                    quote_identifier(table)
                ))?
                .is_empty();
            if populated {
                blockers.push(format!(
                    "table {table} contains records outside the verified JSONL replay surface"
                ));
            } else if !known_empty_auxiliary_table(table) {
                blockers.push(format!(
                    "table {table} has no verified schema-preserving replay path"
                ));
            }
        }
    }
    // Definitions can carry constraints or operator-owned behavior even when
    // their tables are empty. Implicit SQLite autoindexes have NULL sql and
    // belong to their table declaration.
    for row in conn.query(
        "SELECT type, name FROM sqlite_master \
         WHERE type IN ('index', 'trigger', 'view') AND sql IS NOT NULL",
    )? {
        let kind = required_legacy_text(row.get(0), "sqlite_master", "type")?;
        let name = required_legacy_text(row.get(1), "sqlite_master", "name")?;
        blockers.push(format!(
            "{kind} {name} has no verified schema-preserving replay path"
        ));
    }
    blockers.sort_unstable();
    Ok(blockers)
}

/// Known decoded issue/relation fields and derived cache fields. Unknown
/// columns cannot be silently classified as cache data.
fn replayed_columns(table: &str) -> Option<&'static [&'static str]> {
    match table {
        "issues" => Some(&[]),
        "labels" => Some(&["issue_id", "label"]),
        "comments" => Some(&["id", "issue_id", "author", "text", "body", "created_at"]),
        "dependencies" => Some(&[
            "issue_id",
            "depends_on_id",
            "type",
            "created_at",
            "created_by",
            "metadata",
            "thread_id",
        ]),
        "dirty_issues" => Some(&["issue_id", "marked_at", "content_hash", "dirty_hash"]),
        "export_hashes" => Some(&["issue_id", "content_hash", "exported_at"]),
        "blocked_issues_cache" => Some(&["issue_id", "blocked_by", "blocked_at"]),
        "jsonl_row_receipts" => Some(&["issue_id", "row_hash"]),
        _ => None,
    }
}

fn known_empty_auxiliary_table(table: &str) -> bool {
    matches!(
        table,
        "events"
            | "config"
            | "metadata"
            | "child_counters"
            | "close_metadata"
            | "gate_results"
            | "gate_result_history"
            | "capacity_exemptions"
            | "capacity_exemption_history"
            | "capacity_occupancy"
            | "sqlite_sequence"
    )
}
