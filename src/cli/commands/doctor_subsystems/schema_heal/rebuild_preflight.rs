//! Rehearse the complete rebuild/restore before touching the live family.
//!
//! JSONL can be valid on its own while a recoverable local issue conflicts
//! with it (for example, two different IDs claiming one unique external_ref).
//! The audit proves decodability, not that replay into the current schema
//! will succeed. Never discover a deterministic replay failure only after
//! replacing the old database and marking its schema current.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::Arc;

use crate::config::{self, ConfigLayer};
use crate::error::{BeadsError, Result};
use crate::model::Issue;
use crate::storage::SqliteStorage;
use crate::sync::{self, JsonlSourceSnapshot, PreservedIssue};

/// The normalized imported state, overlaid with the exact audited local
/// payloads. Check both the rehearsal and the actual replay against it.
pub(super) struct RebuildWitness {
    issues: BTreeMap<String, Issue>,
    restored_ids: BTreeSet<String>,
}

impl RebuildWitness {
    pub(super) fn verify(&self, storage: &SqliteStorage) -> Result<()> {
        let actual = read_payloads(storage)?;
        if actual.keys().ne(self.issues.keys()) {
            return Err(BeadsError::Config(
                "schema-heal preservation failed: rebuilt issue IDs differ from the combined JSONL/local restore plan"
                    .to_owned(),
            ));
        }
        for (id, expected) in &self.issues {
            let mut expected = expected.clone();
            sync::canonicalize_persisted_issue_defaults(&mut expected);
            if !sync::persisted_import_issue_equals(&actual[id], &expected) {
                let fields = sync::sync_mismatch_field_names(&actual[id], &expected);
                return Err(BeadsError::Config(format!(
                    "schema-heal preservation failed: issue {id} differs from its planned payload (fields: {})",
                    fields.join(", ")
                )));
            }
        }
        self.verify_local_provenance(storage)?;
        let dirty: BTreeSet<String> = storage
            .execute_raw_query("SELECT issue_id FROM dirty_issues")?
            .iter()
            .map(|row| {
                row.first()
                    .and_then(|value| value.as_text())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        BeadsError::Config(
                            "schema-heal preservation failed: unreadable dirty issue ID".to_owned(),
                        )
                    })
            })
            .collect::<Result<_>>()?;
        if !self.restored_ids.is_subset(&dirty) {
            return Err(BeadsError::Config(
                "schema-heal preservation failed: restored issues are not all marked for export"
                    .to_owned(),
            ));
        }
        let integrity = storage.integrity_check_messages()?;
        if integrity.len() != 1 || !integrity[0].eq_ignore_ascii_case("ok") {
            return Err(BeadsError::Config(format!(
                "schema-heal preservation failed: rebuilt integrity is not clean: {integrity:?}"
            )));
        }
        Ok(())
    }

    /// Local provenance is not a cross-clone sync key. Read the actual SQL
    /// column rather than relying on an export projection to retain it.
    fn verify_local_provenance(&self, storage: &SqliteStorage) -> Result<()> {
        let expected: BTreeMap<&str, &str> = self
            .issues
            .iter()
            .filter(|(id, _)| self.restored_ids.contains(*id))
            .filter_map(|(id, issue)| {
                issue
                    .source_repo_path
                    .as_deref()
                    .map(|path| (id.as_str(), path))
            })
            .collect();
        if expected.is_empty() {
            // An absent local value makes no claim about an imported value.
            return Ok(());
        }
        let mut unseen: BTreeSet<&str> = expected.keys().copied().collect();
        for row in storage.execute_raw_query("SELECT id, source_repo_path FROM issues")? {
            let id = row
                .first()
                .and_then(|value| value.as_text())
                .ok_or_else(|| {
                    BeadsError::Config(
                        "schema-heal preservation failed: unreadable provenance issue ID"
                            .to_owned(),
                    )
                })?;
            let Some(path) = expected.get(id) else {
                continue;
            };
            if row.get(1).and_then(|value| value.as_text()) != Some(*path) {
                return Err(BeadsError::Config(format!(
                    "schema-heal preservation failed: restored issue {id} lost its local source_repo_path"
                )));
            }
            unseen.remove(id);
        }
        if !unseen.is_empty() {
            return Err(BeadsError::Config(format!(
                "schema-heal preservation failed: restored provenance rows are missing: {unseen:?}"
            )));
        }
        Ok(())
    }
}

/// Rehearse against an isolated database and an isolated copy of the pinned
/// source. No original database path or original write authority is passed
/// to this function. The temporary workspace contains no unique forensic
/// state and remains owned by TempDir, including on every error path.
///
/// This catches deterministic import/replay failures before live mutation;
/// it is not a substitute for crash-safe installation or I/O-error rollback.
pub(super) fn rehearse(
    source: &JsonlSourceSnapshot,
    restorable: &[PreservedIssue],
    bootstrap: &ConfigLayer,
    lock_timeout: Option<u64>,
) -> Result<Option<RebuildWitness>> {
    if restorable.is_empty() {
        // The shared rebuild already verifies JSONL-only imports. An extra
        // full import is needed only when there is additional local state.
        return Ok(None);
    }
    let result: Result<Option<RebuildWitness>> = (|| {
        let scratch = tempfile::Builder::new()
            .prefix("br-schema-heal-")
            .tempdir()?;
        // Resolve platform temp aliases before invoking route safety checks.
        let beads_dir = scratch.path().canonicalize()?.join(".beads");
        fs::create_dir(&beads_dir)?;
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&jsonl_path)?;
            std::io::copy(&mut source.reader(), &mut file)?;
            file.sync_all()?;
        }
        let write_authority = Arc::new(sync::blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &db_path,
            lock_timeout,
        )?);
        let jsonl_authority =
            sync::blocking_jsonl_family_write_lock_with_timeout(&jsonl_path, lock_timeout)?;
        let private_source = sync::capture_jsonl_source_snapshot(&jsonl_path)?;
        let (mut storage, _, _) =
            config::repair_database_from_jsonl_snapshot_under_write_authority(
                &beads_dir,
                &db_path,
                lock_timeout,
                bootstrap,
                false,
                false,
                &private_source,
                &jsonl_authority,
                &write_authority,
            )?;
        // The shared import has verified these normalized persisted payloads,
        // including its tombstone/default/dangling-edge rules. Protect every
        // imported row, not just the local issues about to be restored.
        let mut witness = RebuildWitness {
            issues: read_payloads(&storage)?,
            restored_ids: BTreeSet::new(),
        };
        for preserved in restorable {
            let mut expected = preserved.issue.clone();
            if let Some(labels) = &preserved.labels {
                expected.labels.clone_from(labels);
            }
            if let Some(dependencies) = &preserved.dependencies {
                expected.dependencies.clone_from(dependencies);
            }
            if let Some(comments) = &preserved.comments {
                expected.comments.clone_from(comments);
            }
            witness.restored_ids.insert(expected.id.clone());
            witness.issues.insert(expected.id.clone(), expected);
        }
        sync::restore_dirty_issues_after_rebuild(&mut storage, restorable)?;
        storage.checkpoint_full()?;
        witness.verify(&storage)?;
        // Storage, snapshots and locks are dropped before the TempDir. No
        // keep() is appropriate: the pinned export and original DB still exist.
        Ok(Some(witness))
    })();
    result.map_err(|source| BeadsError::WithContext {
        context: "schema-heal restore rehearsal failed; the live database rebuild was not started"
            .to_owned(),
        source: Box::new(source),
    })
}

fn read_payloads(storage: &SqliteStorage) -> Result<BTreeMap<String, Issue>> {
    let ids: Vec<String> = storage
        .execute_raw_query("SELECT id FROM issues")?
        .iter()
        .map(|row| {
            row.first()
                .and_then(|value| value.as_text())
                .map(str::to_owned)
                .ok_or_else(|| {
                    BeadsError::Config(
                        "schema-heal preservation failed: unreadable issue ID".to_owned(),
                    )
                })
        })
        .collect::<Result<_>>()?;
    let payloads: BTreeMap<String, Issue> = storage
        .get_issues_for_export(&ids)?
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect();
    if payloads.len() != ids.len() || ids.iter().any(|id| !payloads.contains_key(id)) {
        return Err(BeadsError::Config(
            "schema-heal preservation failed: not every stored issue could be exported".to_owned(),
        ));
    }
    Ok(payloads)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn issue(id: &str) -> Issue {
        let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        Issue {
            id: id.to_owned(),
            title: format!("title {id}"),
            created_at: at,
            updated_at: at,
            ..Issue::default()
        }
    }

    fn preserved(issue: Issue) -> PreservedIssue {
        PreservedIssue {
            labels: Some(issue.labels.clone()),
            dependencies: Some(issue.dependencies.clone()),
            comments: Some(issue.comments.clone()),
            issue,
        }
    }

    #[test]
    fn rehearsal_rejects_a_conflict_between_individually_valid_payloads() {
        let original = tempfile::tempdir().unwrap();
        let path = original.path().join("issues.jsonl");
        let mut imported = issue("pf-abc");
        imported.external_ref = Some("one-external-reference".to_owned());
        let bytes = format!("{}\n", serde_json::to_string(&imported).unwrap());
        fs::write(&path, &bytes).unwrap();
        let source = sync::capture_jsonl_source_snapshot(&path).unwrap();
        let mut local = issue("pf-def");
        local.external_ref.clone_from(&imported.external_ref);

        let result = rehearse(
            &source,
            &[preserved(local)],
            &ConfigLayer::default(),
            Some(5_000),
        );
        let Err(error) = result else {
            panic!("replay must not silently discard either claimant");
        };
        assert!(
            error
                .to_string()
                .contains("live database rebuild was not started")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
        assert!(!original.path().join("beads.db").exists());
    }

    #[test]
    fn rehearsal_preserves_the_import_and_the_audited_local_payload() {
        let original = tempfile::tempdir().unwrap();
        let path = original.path().join("issues.jsonl");
        let imported = issue("pf-abc");
        let bytes = format!("{}\n", serde_json::to_string(&imported).unwrap());
        fs::write(&path, &bytes).unwrap();
        let source = sync::capture_jsonl_source_snapshot(&path).unwrap();
        let mut local = issue("pf-def");
        local.labels.push("unflushed".to_owned());
        local.agent_context = Some("{\"constraints\":[\"retain governance\"]}".to_owned());
        local.source_repo_path = Some("/original/workspace".to_owned());
        let witness = rehearse(
            &source,
            &[preserved(local)],
            &ConfigLayer::default(),
            Some(5_000),
        )
        .unwrap()
        .unwrap();
        assert_eq!(witness.issues.len(), 2);
        assert_eq!(witness.issues["pf-abc"].title, imported.title);
        assert_eq!(witness.issues["pf-def"].labels, ["unflushed"]);
        assert_eq!(
            witness.issues["pf-def"].agent_context.as_deref(),
            Some("{\"constraints\":[\"retain governance\"]}")
        );
        assert_eq!(
            witness.issues["pf-def"].source_repo_path.as_deref(),
            Some("/original/workspace")
        );
        assert_eq!(witness.restored_ids, BTreeSet::from(["pf-def".to_owned()]));
        assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
    }

    #[test]
    fn replay_witness_rejects_dropped_governance_and_known_local_provenance() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let mut local = issue("pf-local");
        local.agent_context = Some("{\"constraints\":[\"retain\"]}".to_owned());
        local.source_repo_path = Some("/original/workspace".to_owned());
        storage.create_issue(&local, "test").unwrap();
        let mut expected = read_payloads(&storage).unwrap();
        expected
            .get_mut(&local.id)
            .unwrap()
            .source_repo_path
            .clone_from(&local.source_repo_path);
        let witness = RebuildWitness {
            issues: expected,
            restored_ids: BTreeSet::from([local.id.clone()]),
        };
        witness.verify(&storage).unwrap();

        storage
            .execute_raw("UPDATE issues SET source_repo_path = NULL WHERE id = 'pf-local'")
            .unwrap();
        let error = witness.verify(&storage).unwrap_err();
        assert!(error.to_string().contains("source_repo_path"), "{error}");
        storage
            .execute_raw(
                "UPDATE issues SET source_repo_path = '/original/workspace', \
                 agent_context = NULL WHERE id = 'pf-local'",
            )
            .unwrap();
        let error = witness.verify(&storage).unwrap_err();
        assert!(error.to_string().contains("planned payload"), "{error}");
    }

    #[test]
    fn imported_paths_and_absent_local_paths_are_not_cross_clone_sync_keys() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let imported = issue("pf-imported");
        storage.create_issue(&imported, "test").unwrap();
        let mut witness = RebuildWitness {
            issues: read_payloads(&storage).unwrap(),
            restored_ids: BTreeSet::new(),
        };
        storage
            .execute_raw(
                "UPDATE issues SET source_repo_path = '/this/clone' WHERE id = 'pf-imported'",
            )
            .unwrap();
        witness.verify(&storage).unwrap();
        // With no saved local path, preserving a path supplied by import is
        // valid even when the issue is in the restore set for another field.
        witness.restored_ids.insert(imported.id);
        witness.verify(&storage).unwrap();
    }
}
