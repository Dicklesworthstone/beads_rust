//! Compose a Git export from two independent, validated sources. Issue rows
//! keep their original JSON (including fields this version doesn't know).
//! Workflow rows always come from the same source as their issue, even for
//! evidence-only edits that leave the anchor unchanged.

use super::{Checkpoint, conflict, parse_checkpoints, row_anchor, used_warnings};
use crate::error::Result;
use crate::model::Status;
use crate::storage::{PortableHistoryIssue, PortableHistorySnapshot};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Default)]
struct ExportPair<'a> {
    lines: BTreeMap<String, &'a str>,
    anchors: BTreeMap<String, PortableHistoryIssue>,
    checkpoints: Vec<Checkpoint>,
}

impl<'a> ExportPair<'a> {
    fn read(issues: &'a str, history: &str) -> Result<Self> {
        let mut lines = BTreeMap::<String, &str>::new();
        let mut anchors = BTreeMap::new();
        let mut checkpoints = parse_checkpoints(history)?;
        checkpoints.reverse(); // Evidence selection tries the newest generation first.
        let default_status = Status::default();
        for (index, line) in issues.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let row = crate::sync::parse_unique_json(line)
                .map_err(|e| conflict(format!("invalid issue at line {}: {e}", index + 1)))?;
            let (id, anchor) = row_anchor(&row, default_status.as_str())
                .filter(|_| row.get("status").is_none_or(Value::is_string))
                .ok_or_else(|| conflict(format!("invalid issue anchor at line {}", index + 1)))?;
            if let Some((previous, _)) = lines.last_key_value()
                && previous.as_str() >= id
            {
                return Err(conflict(format!(
                    "issue IDs must be unique and sorted: {id} follows {previous} at line {}",
                    index + 1
                )));
            }
            // The incremental exporter can legitimately leave a row at the
            // older retained generation. This is the same per-row coverage
            // rule as finalize_export, with no permissive import fallback.
            if !checkpoints
                .iter()
                .any(|cp| cp.history.issues.get(id) == Some(&anchor))
            {
                return Err(conflict(format!(
                    "stale or missing history anchor for {id}"
                )));
            }
            anchors.insert(id.to_owned(), anchor);
            lines.insert(id.to_owned(), line);
        }
        // The previous generation can describe issues that have since been
        // removed. The newest can also retain aged-out tombstones, but a
        // live issue present only in that generation is an unpaired export.
        if let Some(newest) = checkpoints.first()
            && let Some((id, _)) = newest
                .history
                .issues
                .iter()
                .find(|(id, anchor)| anchor.status != "tombstone" && !lines.contains_key(*id))
        {
            return Err(conflict(format!(
                "history anchor for {id} has no matching issue in the export"
            )));
        }
        Ok(Self {
            lines,
            anchors,
            checkpoints,
        })
    }
}

fn project(
    checkpoints: &[&Checkpoint],
    anchors: &BTreeMap<String, PortableHistoryIssue>,
) -> Result<Checkpoint> {
    let histories: Vec<_> = checkpoints.iter().map(|cp| &cp.history).collect();
    let selection = PortableHistorySnapshot::select(&histories, anchors)?;
    if let Some(id) = selection.unmatched.first() {
        return Err(conflict(format!("missing workflow evidence for {id}")));
    }
    Checkpoint::new(
        selection.history,
        used_warnings(checkpoints, &selection.used),
    )
}

/// Verify a staged or committed issues JSONL and its v1 history companion.
pub fn check_export_pair(issues: &str, history: &str) -> Result<()> {
    ExportPair::read(issues, history).map(|_| ())
}

/// Start from the staged (or explicit base) pair and copy only named issues
/// from the working pair. A single checkpoint suffices: Git publishes both
/// blobs in one index transaction, so there is no interrupted half-generation.
pub fn compose_selected_export(
    base: Option<(&str, &str)>,
    working: (&str, &str),
    ids: &[String],
) -> Result<(String, String)> {
    if ids.is_empty() {
        return Err(conflict("specify at least one issue ID to stage"));
    }
    let working = ExportPair::read(working.0, working.1)?;
    let ExportPair {
        mut lines,
        mut anchors,
        checkpoints,
    } = base
        .map(|(issues, history)| ExportPair::read(issues, history))
        .transpose()?
        .unwrap_or_default();
    let mut selected = BTreeMap::new();
    for id in ids {
        let anchor = working
            .anchors
            .get(id)
            .ok_or_else(|| conflict(format!("issue {id} is absent from the working export")))?;
        if selected.insert(id.clone(), anchor.clone()).is_some() {
            return Err(conflict(format!("duplicate selected issue ID {id}")));
        }
        anchors.insert(id.clone(), anchor.clone());
        lines.insert(id.clone(), working.lines[id]);
    }
    let working_sources: Vec<_> = working.checkpoints.iter().collect();
    let overlay = project(&working_sources, &selected)?;
    let mut sources = vec![&overlay];
    sources.extend(&checkpoints);
    let checkpoint = project(&sources, &anchors)?;
    let mut history = serde_json::to_string(&checkpoint)?;
    history.push('\n');
    let mut issues = String::new();
    for line in lines.into_values() {
        issues.push_str(line);
        issues.push('\n');
    }
    Ok((issues, history))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const WHEN: &str = "2026-01-02T03:04:05Z";

    fn checkpoint(ids: &[&str], source: &str) -> Result<String> {
        let anchor = PortableHistoryIssue::canonical("open", WHEN, WHEN).unwrap();
        let anchors = ids
            .iter()
            .map(|id| ((*id).to_owned(), anchor.clone()))
            .collect();
        let mut snapshot = PortableHistorySnapshot::empty_for_issues(anchors);
        for &id in ids {
            let actor = format!("{source}-{id}");
            let rows = [
                json!([
                    1,
                    id,
                    "status_changed",
                    actor,
                    "draft",
                    "open",
                    null,
                    WHEN,
                    null,
                    null,
                    null
                ]),
                json!([
                    1, id, "open", "closed", 1, "ci", "runner", 1, null, actor, WHEN, null, null
                ]),
                json!([
                    1, id, "limit", "wip", "grant", "ops", null, actor, null, WHEN
                ]),
                json!([id, "ci", "runner", 1, null, actor, WHEN]),
                json!([id, actor, null, null, 0, null, null, WHEN]),
                json!([id, actor, null, null, null, WHEN]),
                json!([
                    id, "limit", "wip", "ops", "coverage", actor, WHEN, null, null, null, null
                ]),
            ];
            for (table, row) in snapshot.tables.iter_mut().zip(rows) {
                table.rows.push(row.as_array().unwrap().clone());
            }
        }
        Ok(serde_json::to_string(&Checkpoint::new(snapshot, vec![])?)?)
    }

    fn issue(id: &str) -> String {
        json!({"id": id, "title": id, "created_at": WHEN, "updated_at": WHEN}).to_string() + "\n"
    }

    #[test]
    fn evidence_only_edits_follow_issue_ownership_in_every_table() -> Result<()> {
        let a = issue("br-a");
        let b = issue("br-b");
        let c = issue("br-c");
        let base_issues = format!("{a}{b}");
        let working_issues = format!("{a}{b}{c}");
        let base_history = checkpoint(&["br-a", "br-b"], "base")?;
        let work_history = checkpoint(&["br-a", "br-b", "br-c"], "work")?;
        let (issues, history) = compose_selected_export(
            Some((&base_issues, &base_history)),
            (&working_issues, &work_history),
            &["br-a".into()],
        )?;
        assert_eq!(issues, base_issues);
        check_export_pair(&issues, &history)?;
        let cp: Checkpoint = serde_json::from_str(&history)?;
        for table in cp.history.tables {
            assert_eq!(table.rows.len(), 2, "{}", table.table);
            assert!(
                table.rows[0].contains(&Value::from("work-br-a")),
                "{}",
                table.table
            );
            assert!(
                table.rows[1].contains(&Value::from("base-br-b")),
                "{}",
                table.table
            );
        }
        assert!(!history.contains("br-c"));
        Ok(())
    }

    #[test]
    fn previous_generations_and_omitted_tombstones_remain_valid() -> Result<()> {
        let old = checkpoint(&["br-a", "br-b"], "base")?;
        let mut newest: Checkpoint = serde_json::from_str(&checkpoint(&["br-a"], "work")?)?;
        newest.history.issues.get_mut("br-a").unwrap().updated_at =
            "2026-01-03T03:04:05Z".to_owned();
        let current = serde_json::to_string(&Checkpoint::new(newest.history, vec![])?)?;
        let issues = issue("br-a");
        check_export_pair(&issues, &format!("{old}\n{current}"))?;

        let mut stale: Checkpoint = serde_json::from_str(&old)?;
        stale.history.issues.get_mut("br-b").unwrap().status = "tombstone".to_string();
        let archived = Checkpoint::new(stale.history, vec![])?;
        check_export_pair(&issues, &serde_json::to_string(&archived)?)?;
        Ok(())
    }

    #[test]
    fn checks_each_checksum_and_requires_an_anchor_for_every_row() -> Result<()> {
        let a = issue("br-a");
        let b = issue("br-b");
        let old = checkpoint(&["br-a"], "base")?;
        let current = checkpoint(&["br-a", "br-b"], "work")?;
        let history = format!("{old}\n{current}\n");
        check_export_pair(&format!("{a}{b}"), &history)?;
        assert!(check_export_pair(&format!("{b}{a}"), &history).is_err());
        assert!(check_export_pair(&format!("{a}{a}"), &history).is_err());
        assert!(check_export_pair(&a.replace("2026-01", "2026-02"), &history).is_err());
        assert!(check_export_pair(&a, &history).is_err());
        let repeated_key = a.replacen('{', r#"{"id":"hidden","#, 1);
        assert!(check_export_pair(&format!("{repeated_key}{b}"), &history).is_err());
        let damaged = history.replace("base-br-a", "damaged");
        assert!(check_export_pair(&format!("{a}{b}"), &damaged).is_err());
        Ok(())
    }
}
