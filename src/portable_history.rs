//! Repository-managed workflow-history companion for issue JSONL.
//!
//! This is deliberately outside the classic issue interchange payload. A
//! checkpoint carries workflow evidence bound, issue by issue, to that issue's
//! identity/status/update anchor. Two generations are retained so publishing
//! history before issues is recoverable after interruption.
//!
//! The companion never blocks issue sync. Evidence is replayed or carried
//! forward only for a row at the anchor it describes; a row it does not
//! describe (a hand-edited or older-release issues.jsonl, or a row import kept
//! over a newer one the workflow policy could not reach, GitHub #540) simply
//! gets no evidence, and the gap is recorded as a coverage warning instead of
//! being fabricated. Only a damaged companion file is an error.

use crate::error::{BeadsError, Result};
use crate::storage::{
    PortableHistoryIssue, PortableHistoryReplay, PortableHistorySnapshot, SqliteStorage,
    validate_portable_history,
};
use crate::util::hex_encode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

// Items here are `pub`, not `pub(crate)`: this module is `pub(crate)` in
// src/lib.rs, so they stay crate-visible either way and `pub(crate)` is what
// `clippy::redundant_pub_crate` rejects.

pub const WARNINGS_KEY: &str = "portable_history_warnings";
const LEGACY_WARNING: &str = "Portable workflow history is unavailable for this issue source. Earlier events, gate results, close metadata, and capacity evidence cannot be verified or reconstructed from issues.jsonl alone. Existing local evidence is preserved; missing evidence has not been fabricated.";
const ENROLLMENT_WARNING: &str = "This tracker predates portable workflow history. Available database evidence was exported, but completeness of earlier audit history cannot be verified.";
const DIVERGED_WARNING: &str = "Portable workflow history for some issues diverged between clones. Each clone kept its own evidence for those issues; the other clone's diverging events, gate results, close metadata, or capacity evidence were not merged.";
const DAMAGED_HINT: &str = "restore .beads/issues.history.jsonl from version control, or move it aside to re-enroll (missing evidence is then reported, never fabricated)";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    version: u32,
    anchor_sha256: String,
    history_sha256: String,
    history: PortableHistorySnapshot,
    warnings: Vec<String>,
}

fn conflict(message: impl Into<String>) -> BeadsError {
    BeadsError::SyncConflict {
        message: format!("Portable workflow history: {}", message.into()),
    }
}

fn damaged(message: impl std::fmt::Display) -> BeadsError {
    conflict(format!("{message}; {DAMAGED_HINT}"))
}

#[must_use]
pub fn sidecar_path(jsonl: &Path) -> PathBuf {
    jsonl.with_extension("history.jsonl")
}

fn digest_history(
    version: u32,
    anchor: &str,
    history: &PortableHistorySnapshot,
    warnings: &[String],
) -> Result<String> {
    Ok(hex_encode(&Sha256::digest(serde_json::to_vec(&(
        version, anchor, history, warnings,
    ))?)))
}

fn anchor_digest(issues: &BTreeMap<String, PortableHistoryIssue>) -> Result<String> {
    Ok(hex_encode(&Sha256::digest(serde_json::to_vec(issues)?)))
}

impl Checkpoint {
    fn new(history: PortableHistorySnapshot, warnings: Vec<String>) -> Result<Self> {
        validate_portable_history(&history)?;
        let anchor_sha256 = anchor_digest(&history.issues)?;
        let mut checkpoint = Self {
            version: 1,
            anchor_sha256,
            history_sha256: String::new(),
            history,
            warnings,
        };
        checkpoint.history_sha256 = digest_history(
            checkpoint.version,
            &checkpoint.anchor_sha256,
            &checkpoint.history,
            &checkpoint.warnings,
        )?;
        Ok(checkpoint)
    }
    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(damaged(format!(
                "unsupported format version {}",
                self.version
            )));
        }
        validate_portable_history(&self.history).map_err(damaged)?;
        if self.anchor_sha256 != anchor_digest(&self.history.issues)?
            || self.history_sha256
                != digest_history(
                    self.version,
                    &self.anchor_sha256,
                    &self.history,
                    &self.warnings,
                )?
        {
            return Err(damaged("checkpoint checksum mismatch"));
        }
        Ok(())
    }
}

/// The checkpoints of a companion file, oldest first.
fn read_lines(path: &Path) -> Result<Vec<Checkpoint>> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(damaged("history companion must be a regular file"));
    }
    let text = fs::read_to_string(path)?;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let cp: Checkpoint = serde_json::from_str(line)
            .map_err(|e| damaged(format!("malformed checkpoint at line {}: {e}", index + 1)))?;
        cp.validate()?;
        if !seen.insert(cp.anchor_sha256.clone()) {
            return Err(damaged("duplicate issue generation in history companion"));
        }
        out.push(cp);
        if out.len() > 2 {
            return Err(damaged(
                "history companion contains more than two generations",
            ));
        }
    }
    if out.is_empty() {
        return Err(damaged("history companion is empty"));
    }
    Ok(out)
}

/// The checkpoints of the companion beside `jsonl`, oldest first, or none.
fn existing_checkpoints(jsonl: &Path) -> Result<Vec<Checkpoint>> {
    let sidecar = sidecar_path(jsonl);
    if sidecar.exists() {
        read_lines(&sidecar)
    } else {
        Ok(Vec::new())
    }
}

/// The text of the issues.jsonl at `path`, or `None` when there is none.
fn read_source(path: &Path) -> Result<Option<String>> {
    if path.exists() {
        Ok(Some(fs::read_to_string(path)?))
    } else {
        Ok(None)
    }
}

/// The anchor of every issue row in the issues.jsonl at `path`.
fn source_anchors(path: &Path) -> Result<BTreeMap<String, PortableHistoryIssue>> {
    Ok(read_source(path)?
        .as_deref()
        .map(anchors_of)
        .unwrap_or_default())
}

/// The anchor of every issue row in `text` that has one, keyed by id. A row
/// that is not a JSON object, lacks an id or a timestamp, or shares its id
/// with another row gets no anchor, so no evidence is ever bound to it;
/// validating the issue rows themselves is import's job.
fn anchors_of(text: &str) -> BTreeMap<String, PortableHistoryIssue> {
    let default_status = crate::model::Status::default();
    let mut anchors = BTreeMap::new();
    let mut duplicates = HashSet::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let field = |name: &str| value.get(name).and_then(Value::as_str);
        let (Some(id), Some(created_at), Some(updated_at)) =
            (field("id"), field("created_at"), field("updated_at"))
        else {
            continue;
        };
        // Import reads a missing status as the default one.
        let status = field("status").unwrap_or(default_status.as_str());
        let Some(anchor) = PortableHistoryIssue::canonical(status, created_at, updated_at) else {
            continue;
        };
        if anchors.insert(id.to_owned(), anchor).is_some() {
            duplicates.insert(id.to_owned());
        }
    }
    for id in duplicates {
        anchors.remove(&id);
    }
    anchors
}

pub fn stored_warnings(storage: &SqliteStorage) -> Result<Vec<String>> {
    storage.get_metadata(WARNINGS_KEY)?.map_or_else(
        || Ok(Vec::new()),
        |value| {
            serde_json::from_str(&value)
                .map_err(|e| conflict(format!("invalid persisted coverage warning: {e}")))
        },
    )
}
fn union(mut a: Vec<String>, b: &[String]) -> Vec<String> {
    a.extend_from_slice(b);
    a.sort();
    a.dedup();
    a
}

/// Warnings of the checkpoints `used` marks.
fn used_warnings(checkpoints: &[&Checkpoint], used: &[bool]) -> Vec<String> {
    checkpoints
        .iter()
        .zip(used)
        .filter(|(_, used)| **used)
        .fold(Vec::new(), |warnings, (cp, _)| {
            union(warnings, &cp.warnings)
        })
}

fn database_warnings(
    storage: &SqliteStorage,
    history: &PortableHistorySnapshot,
) -> Result<Vec<String>> {
    Ok(if storage.get_metadata(WARNINGS_KEY)?.is_some() {
        stored_warnings(storage)?
    } else if history.issues.is_empty() {
        Vec::new()
    } else {
        vec![ENROLLMENT_WARNING.to_owned()]
    })
}

fn write_atomic(path: &Path, checkpoints: &[Checkpoint]) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && (!meta.is_file() || meta.file_type().is_symlink())
    {
        return Err(conflict(
            "refusing to replace a non-regular history companion",
        ));
    }
    let dir = path
        .parent()
        .ok_or_else(|| conflict("history companion has no parent"))?;
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or_else(|| conflict("invalid history filename"))?;
    let mut temp = None;
    for index in 0..64u32 {
        let candidate = dir.join(format!(".{name}.tmp.{}.{}", std::process::id(), index));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temp = Some((candidate, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let (temp_path, mut file) =
        temp.ok_or_else(|| conflict("could not allocate history temp file"))?;
    for cp in checkpoints {
        serde_json::to_writer(&mut file, cp)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    let reread = read_lines(&temp_path)?;
    if reread.len() != checkpoints.len() {
        return Err(conflict("staged history verification failed"));
    }
    fs::rename(&temp_path, path)?;
    if let Ok(parent) = OpenOptions::new().read(true).open(dir) {
        let _ = parent.sync_all();
    }
    Ok(())
}

/// Publish history before the corresponding issue export.
///
/// The new generation holds this database's evidence for every exportable
/// issue, plus evidence the companion already carried for an issue at the
/// same anchor that this database never replayed. The previous generation
/// describes the issues.jsonl currently on disk, so an interrupted export
/// still finds its evidence; rows of that file that no evidence describes are
/// kept as bare anchors under a coverage warning.
pub fn publish_before_export(storage: &SqliteStorage, jsonl: &Path) -> Result<()> {
    publish(storage, jsonl, read_source(jsonl)?.as_deref())
}

/// [`publish_before_export`] for an automatic export. That export never
/// overwrites an issues.jsonl holding merge-conflict markers, so the
/// companion is left alone too: describing the conflicted file would drop the
/// evidence of every conflicted row, while the companion, often conflicted as
/// well, waits for the same resolution.
pub fn publish_before_automatic_export(storage: &SqliteStorage, jsonl: &Path) -> Result<()> {
    let source = read_source(jsonl)?;
    if let Some(text) = source.as_deref()
        && !crate::sync::scan_conflict_markers_from_reader(jsonl, text.as_bytes())?.is_empty()
    {
        tracing::debug!(
            jsonl = %jsonl.display(),
            "issues.jsonl holds merge-conflict markers; workflow history not published"
        );
        return Ok(());
    }
    publish(storage, jsonl, source.as_deref())
}

/// Publish a generation for this database before an export replaces
/// `source`, the text of the issues.jsonl at `jsonl` (`None` when missing).
fn publish(storage: &SqliteStorage, jsonl: &Path, source: Option<&str>) -> Result<()> {
    let old = existing_checkpoints(jsonl)?;
    let old_newest_first: Vec<&Checkpoint> = old.iter().rev().collect();
    let old_histories: Vec<&PortableHistorySnapshot> =
        old_newest_first.iter().map(|cp| &cp.history).collect();

    let database = storage.all_portable_history()?;
    let database_warnings = database_warnings(storage, &database)?;
    let carried = database.carry_forward(&old_histories)?;
    let mut next_warnings = union(
        database_warnings,
        &used_warnings(&old_newest_first, &carried.used),
    );
    if !carried.diverged.is_empty() {
        tracing::warn!(
            issues = ?carried.diverged,
            "Workflow history diverged from the published companion; this database's evidence is published for these issues"
        );
        next_warnings = union(next_warnings, &[DIVERGED_WARNING.to_owned()]);
    }
    let next = Checkpoint::new(carried.history, next_warnings)?;

    let current_rows = source.map(anchors_of).unwrap_or_default();
    let previous = if current_rows.is_empty() {
        None
    } else {
        let mut sources = vec![&next.history];
        sources.extend(old_histories.iter().copied());
        let selection = PortableHistorySnapshot::select(&sources, &current_rows)?;
        let mut warnings = if selection.used[0] {
            next.warnings.clone()
        } else {
            Vec::new()
        };
        warnings = union(
            warnings,
            &used_warnings(&old_newest_first, &selection.used[1..]),
        );
        let mut history = selection.history;
        if !selection.unmatched.is_empty() {
            warnings = union(warnings, &[LEGACY_WARNING.to_owned()]);
            for id in &selection.unmatched {
                history.issues.insert(id.clone(), current_rows[id].clone());
            }
        }
        Some(Checkpoint::new(history, warnings)?)
    };

    let mut checkpoints = Vec::with_capacity(2);
    if let Some(previous) = previous
        && previous.anchor_sha256 != next.anchor_sha256
    {
        checkpoints.push(previous);
    }
    checkpoints.push(next);
    write_atomic(&sidecar_path(jsonl), &checkpoints)
}

/// Verify that the companion describes every row of the issues.jsonl just
/// published and that its newest generation holds all of this database's
/// evidence, then persist that generation's coverage warnings.
///
/// A row the export wrote from this database is described by the newest
/// generation. An incremental export can leave a row untouched that differs
/// from the database (a peer's row still pending import); the generation
/// published for the file it replaced describes that row.
pub fn finalize_export(storage: &mut SqliteStorage, jsonl: &Path) -> Result<()> {
    let checkpoints = existing_checkpoints(jsonl)?;
    let newest = checkpoints
        .last()
        .ok_or_else(|| conflict("export completed without a history companion"))?;
    let exported = source_anchors(jsonl)?;
    if let Some(id) = exported
        .iter()
        .find(|(id, anchor)| {
            !checkpoints
                .iter()
                .any(|cp| cp.history.issues.get(*id) == Some(*anchor))
        })
        .map(|(id, _)| id)
    {
        return Err(conflict(format!(
            "export completed without a matching history checkpoint for {id}; retry the flush"
        )));
    }
    let local = storage.all_portable_history()?;
    if local.carry_forward(&[&newest.history])?.history != newest.history {
        return Err(conflict(
            "workflow evidence changed during export; retry the flush",
        ));
    }
    storage.set_metadata(WARNINGS_KEY, &serde_json::to_string(&newest.warnings)?)?;
    Ok(())
}

/// Validate an incoming companion before issue mutation, so a damaged file
/// stops the import before it changes anything. A companion that merely does
/// not describe the incoming rows is not an error: those rows import without
/// replayed evidence.
pub fn preflight_import(jsonl: &Path) -> Result<()> {
    existing_checkpoints(jsonl).map(|_| ())
}

/// Replay the evidence the companion holds for the rows import just applied.
///
/// Evidence is replayed per issue, and only onto a database row at the
/// anchor it describes, so a row import kept (for example one the workflow
/// policy protected, GitHub #540) keeps its own evidence untouched. Applied
/// rows without evidence, and diverged histories, become durable coverage
/// warnings rather than fabricated events or gate approvals.
pub fn replay_after_import(storage: &mut SqliteStorage, jsonl: &Path) -> Result<Vec<String>> {
    let rows = source_anchors(jsonl)?;
    if rows.is_empty() {
        return stored_warnings(storage);
    }
    let checkpoints = existing_checkpoints(jsonl)?;
    let newest_first: Vec<&Checkpoint> = checkpoints.iter().rev().collect();
    let histories: Vec<&PortableHistorySnapshot> =
        newest_first.iter().map(|cp| &cp.history).collect();
    let selection = PortableHistorySnapshot::select(&histories, &rows)?;
    let replay = if selection.history.issues.is_empty() {
        PortableHistoryReplay::default()
    } else {
        storage.restore_portable_history(&selection.history)?
    };

    let mut warnings = union(
        stored_warnings(storage)?,
        &used_warnings(&newest_first, &selection.used),
    );
    // Rows the database now holds at the JSONL anchor but that no evidence
    // describes. A row import kept at its local revision is not a gap.
    let unmatched: HashSet<String> = selection.unmatched.into_iter().collect();
    if !unmatched.is_empty() {
        let held = storage.snapshot_portable_history(Some(&unmatched))?;
        if held
            .issues
            .iter()
            .any(|(id, anchor)| rows.get(id) == Some(anchor))
        {
            warnings = union(warnings, &[LEGACY_WARNING.to_owned()]);
        }
    }
    if !replay.diverged.is_empty() {
        tracing::warn!(
            issues = ?replay.diverged,
            "Workflow history in the companion diverged from this database; local evidence kept"
        );
        warnings = union(warnings, &[DIVERGED_WARNING.to_owned()]);
    }
    if !replay.other_revision.is_empty() {
        tracing::debug!(
            issues = ?replay.other_revision,
            "Workflow history not replayed for rows kept at their local revision"
        );
    }
    storage.set_metadata(WARNINGS_KEY, &serde_json::to_string(&warnings)?)?;
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, Status};
    use crate::storage::IssueUpdate;
    use tempfile::TempDir;

    #[test]
    fn sidecar_keeps_previous_generation_and_replays_gate_history() -> Result<()> {
        let temp = TempDir::new()?;
        let jsonl = temp.path().join("issues.jsonl");
        let mut storage = SqliteStorage::open_memory()?;
        let issue: Issue = serde_json::from_value(serde_json::json!({
            "id":"ph-a","title":"portable","created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:00Z"
        }))?;
        storage.create_issue(&issue, "alice")?;
        let open = storage.get_issue("ph-a")?.unwrap();
        fs::write(&jsonl, format!("{}\n", serde_json::to_string(&open)?))?;
        storage.set_metadata(WARNINGS_KEY, "[]")?;
        publish_before_export(&storage, &jsonl)?;
        storage.update_issue(
            "ph-a",
            &IssueUpdate {
                status: Some(Status::InProgress),
                ..Default::default()
            },
            "alice",
        )?;
        let revision = storage.status_revision("ph-a")?;
        storage.record_scoped_gate_result(
            "ph-a",
            "in_progress",
            revision,
            "closed",
            "ci",
            "runner",
            true,
            None,
            "bot",
        )?;
        publish_before_export(&storage, &jsonl)?;
        let active = storage.get_issue("ph-a")?.unwrap();
        fs::write(&jsonl, format!("{}\n", serde_json::to_string(&active)?))?;
        let checkpoints = read_lines(&sidecar_path(&jsonl))?;
        assert_eq!(checkpoints.len(), 2);
        let histories: Vec<&PortableHistorySnapshot> =
            checkpoints.iter().rev().map(|cp| &cp.history).collect();
        let selected = PortableHistorySnapshot::select(&histories, &source_anchors(&jsonl)?)?;
        assert!(selected.unmatched.is_empty(), "{:?}", selected.unmatched);
        assert_eq!(selected.history.tables[1].rows.len(), 1);
        Ok(())
    }

    const CREATED: &str = "2026-01-01T00:00:00Z";

    fn issue(id: &str, extra: &serde_json::Value) -> Issue {
        let mut value = serde_json::json!({
            "id": id, "title": id, "created_at": CREATED, "updated_at": CREATED
        });
        for (key, field) in extra.as_object().expect("extra fields") {
            value[key] = field.clone();
        }
        serde_json::from_value(value).expect("test issue")
    }

    fn write_rows(path: &Path, rows: &[Issue]) {
        let mut text = String::new();
        for row in rows {
            text.push_str(&serde_json::to_string(row).expect("issue JSON"));
            text.push('\n');
        }
        fs::write(path, text).expect("write issues.jsonl");
    }

    fn event_rows(storage: &SqliteStorage, id: &str) -> Vec<Vec<Value>> {
        storage.all_portable_history().expect("history").tables[0]
            .rows
            .iter()
            .filter(|row| row[1] == id)
            .cloned()
            .collect()
    }

    /// A peer that edited two open issues publishes them with its companion.
    fn peer_publication(temp: &TempDir) -> (SqliteStorage, PathBuf, Vec<Issue>) {
        let jsonl = temp.path().join("issues.jsonl");
        let mut peer = SqliteStorage::open_memory().expect("peer database");
        peer.set_metadata(WARNINGS_KEY, "[]").expect("warnings");
        for (id, title) in [("ph-kept", "Edited while open"), ("ph-applied", "Retitled")] {
            peer.create_issue(&issue(id, &serde_json::json!({})), "peer")
                .expect("create");
            peer.update_issue(
                id,
                &IssueUpdate {
                    title: Some(title.to_string()),
                    ..Default::default()
                },
                "peer",
            )
            .expect("edit");
        }
        let rows = ["ph-kept", "ph-applied"]
            .map(|id| peer.get_issue(id).expect("read").expect("row"))
            .to_vec();
        write_rows(&jsonl, &rows);
        publish_before_export(&peer, &jsonl).expect("publish the peer's history");
        (peer, jsonl, rows)
    }

    /// The database stores `+00:00` offsets and the export writes `Z`: one
    /// instant, two spellings. Publication must still bind them, and an
    /// ephemeral issue's evidence must not leave the database.
    #[test]
    fn publication_binds_database_and_jsonl_spellings_of_one_instant() -> Result<()> {
        let temp = TempDir::new()?;
        let jsonl = temp.path().join("issues.jsonl");
        let mut storage = SqliteStorage::open_memory()?;
        storage.set_metadata(WARNINGS_KEY, "[]")?;
        storage.create_issue(&issue("ph-a", &serde_json::json!({})), "alice")?;
        storage.create_issue(
            &issue("ph-local", &serde_json::json!({ "ephemeral": true })),
            "alice",
        )?;
        let stored =
            storage.execute_raw_query("SELECT updated_at FROM issues WHERE id = 'ph-a'")?;
        let stored = stored[0][0].as_text().expect("stored timestamp").to_owned();
        let exported = storage.get_issue("ph-a")?.expect("issue");
        let exported_text = serde_json::to_value(&exported)?["updated_at"].clone();
        assert_ne!(Some(stored.as_str()), exported_text.as_str());

        publish_before_export(&storage, &jsonl)?;
        write_rows(&jsonl, &[exported]);
        finalize_export(&mut storage, &jsonl)?;
        let companion = fs::read_to_string(sidecar_path(&jsonl))?;
        assert!(companion.contains("ph-a"));
        assert!(!companion.contains("ph-local"), "{companion}");
        Ok(())
    }

    /// GitHub #540 with the companion: import kept `ph-kept` at its local
    /// (closed) revision and applied the peer's `ph-applied`. The peer's
    /// history for `ph-kept` extends this clone's (the close arrived by
    /// import, which records no event), so only the anchor keeps it off the
    /// kept close. Replay must not fail, must leave the kept row's history
    /// alone, and must give the applied row the peer's history, without a
    /// coverage warning.
    #[test]
    fn replay_leaves_a_row_import_kept_with_its_own_history() -> Result<()> {
        let temp = TempDir::new()?;
        let jsonl = temp.path().join("issues.jsonl");
        let ids = ["ph-kept", "ph-applied"];
        let rows_of = |storage: &SqliteStorage| -> Vec<Issue> {
            ids.iter()
                .map(|id| storage.get_issue(id).expect("read").expect("row"))
                .collect()
        };

        // The peer publishes two new issues, and this clone imports them
        // with their history.
        let mut peer = SqliteStorage::open_memory()?;
        peer.set_metadata(WARNINGS_KEY, "[]")?;
        for id in ids {
            peer.create_issue(&issue(id, &serde_json::json!({})), "peer")?;
        }
        publish_before_export(&peer, &jsonl)?;
        write_rows(&jsonl, &rows_of(&peer));
        let mut local = SqliteStorage::open_memory()?;
        local.set_metadata(WARNINGS_KEY, "[]")?;
        for row in rows_of(&peer) {
            local.upsert_issue_for_import(&row)?;
        }
        assert!(replay_after_import(&mut local, &jsonl)?.is_empty());
        assert_eq!(event_rows(&local, "ph-kept"), event_rows(&peer, "ph-kept"));

        // A third clone's close of `ph-kept` arrives here by import.
        let closed_at = chrono::DateTime::parse_from_rfc3339("2026-02-01T00:00:00Z")
            .expect("time")
            .with_timezone(&chrono::Utc);
        let mut closed = local.get_issue("ph-kept")?.expect("row");
        closed.status = Status::Closed;
        closed.closed_at = Some(closed_at);
        closed.updated_at = closed_at;
        local.upsert_issue_for_import(&closed)?;
        let kept_events = event_rows(&local, "ph-kept");

        // The peer edits both issues before it sees the close.
        for id in ids {
            peer.update_issue(
                id,
                &IssueUpdate {
                    title: Some(format!("{id} edited")),
                    ..Default::default()
                },
                "peer",
            )?;
        }
        publish_before_export(&peer, &jsonl)?;
        let edited = rows_of(&peer);
        write_rows(&jsonl, &edited);
        assert!(event_rows(&peer, "ph-kept").len() > kept_events.len());

        // Explicit import kept the close and applied the other row.
        local.upsert_issue_for_import(&edited[1])?;
        let warnings = replay_after_import(&mut local, &jsonl)?;
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(event_rows(&local, "ph-kept"), kept_events);
        assert_eq!(
            event_rows(&local, "ph-applied"),
            event_rows(&peer, "ph-applied")
        );
        // Replaying the same generation again changes nothing.
        replay_after_import(&mut local, &jsonl)?;
        assert_eq!(event_rows(&local, "ph-kept"), kept_events);
        assert_eq!(
            event_rows(&local, "ph-applied"),
            event_rows(&peer, "ph-applied")
        );
        Ok(())
    }

    /// Evidence at the same anchor that diverged from the database's own is
    /// not merged and does not fail the import; it is reported instead. A row
    /// the companion does not describe gets no evidence and a coverage
    /// warning.
    #[test]
    fn replay_reports_divergence_and_undescribed_rows_without_failing() -> Result<()> {
        let temp = TempDir::new()?;
        let (_peer, jsonl, mut rows) = peer_publication(&temp);

        let mut local = SqliteStorage::open_memory()?;
        local.set_metadata(WARNINGS_KEY, "[]")?;
        local.upsert_issue_for_import(&rows[1])?;
        local.execute_raw(
            "INSERT INTO events (issue_id, event_type, actor, created_at) \
             VALUES ('ph-applied', 'commented', 'local', '2026-02-01T00:00:00Z')",
        )?;
        let local_events = event_rows(&local, "ph-applied");
        // A third row arrived without evidence (an older release wrote it).
        let undescribed = issue(
            "ph-new",
            &serde_json::json!({ "updated_at": "2026-03-01T00:00:00Z" }),
        );
        local.upsert_issue_for_import(&undescribed)?;
        rows.push(undescribed);
        write_rows(&jsonl, &rows);

        let warnings = replay_after_import(&mut local, &jsonl)?;
        assert_eq!(
            warnings,
            union(
                vec![DIVERGED_WARNING.to_owned()],
                &[LEGACY_WARNING.to_owned()]
            )
        );
        assert_eq!(event_rows(&local, "ph-applied"), local_events);
        Ok(())
    }

    /// A gate verdict's status revision is the latest status-change event,
    /// but an imported status change records no event, so the verdict's
    /// `from_status` can differ from that event's new status. Such a
    /// database must still publish its history.
    #[test]
    fn publication_accepts_a_gate_verdict_after_an_imported_status_change() -> Result<()> {
        let temp = TempDir::new()?;
        let jsonl = temp.path().join("issues.jsonl");
        let mut storage = SqliteStorage::open_memory()?;
        storage.set_metadata(WARNINGS_KEY, "[]")?;
        storage.create_issue(&issue("ph-a", &serde_json::json!({})), "alice")?;
        storage.update_issue(
            "ph-a",
            &IssueUpdate {
                status: Some(Status::InProgress),
                ..Default::default()
            },
            "alice",
        )?;
        let mut imported = storage.get_issue("ph-a")?.expect("issue");
        imported.status = Status::Blocked;
        imported.updated_at = chrono::Utc::now();
        storage.upsert_issue_for_import(&imported)?;
        let revision = storage.status_revision("ph-a")?;
        storage.record_scoped_gate_result(
            "ph-a", "blocked", revision, "closed", "ci", "runner", true, None, "bot",
        )?;

        publish_before_export(&storage, &jsonl)?;
        write_rows(&jsonl, &[storage.get_issue("ph-a")?.expect("issue")]);
        finalize_export(&mut storage, &jsonl)?;
        Ok(())
    }

    /// An incremental export rewrites only this database's dirty rows and can
    /// leave a peer's newer row in place while it is still pending import.
    /// The generation published for the replaced file describes that row, so
    /// verification must accept it, and later publications keep describing
    /// it until the row changes.
    #[test]
    fn finalize_accepts_an_untouched_row_pending_import() -> Result<()> {
        let temp = TempDir::new()?;
        let jsonl = temp.path().join("issues.jsonl");
        let mut local = SqliteStorage::open_memory()?;
        local.set_metadata(WARNINGS_KEY, "[]")?;
        for id in ["ph-mine", "ph-theirs"] {
            local.create_issue(&issue(id, &serde_json::json!({})), "local")?;
        }
        let row =
            |storage: &SqliteStorage, id: &str| storage.get_issue(id).expect("read").expect("row");
        publish_before_export(&local, &jsonl)?;
        write_rows(&jsonl, &[row(&local, "ph-mine"), row(&local, "ph-theirs")]);
        finalize_export(&mut local, &jsonl)?;

        // A peer's edit of `ph-theirs` arrives in issues.jsonl, and this clone
        // edits `ph-mine` without importing it first.
        let mut theirs = row(&local, "ph-theirs");
        theirs.title = "Retitled by the peer".to_string();
        theirs.updated_at = chrono::Utc::now();
        write_rows(&jsonl, &[row(&local, "ph-mine"), theirs.clone()]);
        local.update_issue(
            "ph-mine",
            &IssueUpdate {
                title: Some("Edited here".to_string()),
                ..Default::default()
            },
            "local",
        )?;

        for _ in 0..2 {
            publish_before_export(&local, &jsonl)?;
            // The incremental export rewrites `ph-mine` and keeps the peer's row.
            write_rows(&jsonl, &[row(&local, "ph-mine"), theirs.clone()]);
            finalize_export(&mut local, &jsonl)?;
        }
        assert_eq!(stored_warnings(&local)?, Vec::<String>::new());
        let checkpoints = read_lines(&sidecar_path(&jsonl))?;
        let histories: Vec<&PortableHistorySnapshot> =
            checkpoints.iter().rev().map(|cp| &cp.history).collect();
        let selected = PortableHistorySnapshot::select(&histories, &source_anchors(&jsonl)?)?;
        assert!(selected.unmatched.is_empty(), "{:?}", selected.unmatched);
        Ok(())
    }

    /// An automatic export skips an issues.jsonl holding merge-conflict
    /// markers, and its publication leaves the companion alone, even when
    /// the companion is conflicted too. Once issues.jsonl is resolved, a
    /// damaged companion is reported again.
    #[test]
    fn automatic_publication_leaves_the_companion_alone_during_a_conflict() -> Result<()> {
        let temp = TempDir::new()?;
        let (mut peer, jsonl, rows) = peer_publication(&temp);
        let sidecar = sidecar_path(&jsonl);
        let published = fs::read(&sidecar)?;

        // This database changed an issue since, and a merge left both
        // versions of that row in issues.jsonl.
        peer.update_issue(
            "ph-applied",
            &IssueUpdate {
                title: Some("Changed again".to_string()),
                ..Default::default()
            },
            "peer",
        )?;
        let ours = serde_json::to_string(&peer.get_issue("ph-applied")?.expect("row"))?;
        let theirs = serde_json::to_string(&rows[1])?;
        let unchanged = serde_json::to_string(&rows[0])?;
        fs::write(
            &jsonl,
            format!("{unchanged}\n<<<<<<< HEAD\n{ours}\n=======\n{theirs}\n>>>>>>> peer\n"),
        )?;
        publish_before_automatic_export(&peer, &jsonl)?;
        assert_eq!(fs::read(&sidecar)?, published);

        fs::write(&sidecar, "<<<<<<< HEAD\n")?;
        publish_before_automatic_export(&peer, &jsonl)?;
        assert_eq!(fs::read_to_string(&sidecar)?, "<<<<<<< HEAD\n");

        write_rows(&jsonl, &rows);
        let error = publish_before_automatic_export(&peer, &jsonl)
            .expect_err("a damaged companion stops publication");
        assert!(
            error
                .to_string()
                .contains("restore .beads/issues.history.jsonl"),
            "{error}"
        );
        Ok(())
    }

    /// Only a damaged companion stops an import. One that describes another
    /// generation of issues.jsonl is not an error.
    #[test]
    fn only_a_damaged_companion_stops_an_import() -> Result<()> {
        let temp = TempDir::new()?;
        let (_peer, jsonl, _rows) = peer_publication(&temp);
        preflight_import(&jsonl)?;
        write_rows(
            &jsonl,
            &[issue(
                "ph-other",
                &serde_json::json!({ "updated_at": "2026-04-01T00:00:00Z" }),
            )],
        );
        preflight_import(&jsonl)?;

        fs::write(sidecar_path(&jsonl), "<<<<<<< ours\n")?;
        let error = preflight_import(&jsonl).expect_err("a damaged companion is refused");
        assert!(
            error
                .to_string()
                .contains("restore .beads/issues.history.jsonl"),
            "{error}"
        );
        Ok(())
    }
}
