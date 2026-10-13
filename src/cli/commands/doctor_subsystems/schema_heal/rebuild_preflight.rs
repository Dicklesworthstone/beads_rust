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
use crate::model::{Comment, Issue, Status};
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
            let id = row.first().and_then(|value| value.as_text()).ok_or_else(|| {
                BeadsError::Config(
                    "schema-heal preservation failed: unreadable provenance issue ID".to_owned(),
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

/// Complete append-only histories for the audit's selected local restores.
/// This only builds an in-memory plan from the pinned source; it neither
/// changes scalar precedence nor modifies either original input. The same
/// result must be used by the rehearsal and the eventual live restoration.
pub(super) fn complete_comment_restore_plan(
    source: &JsonlSourceSnapshot,
    restorable: &[PreservedIssue],
) -> Result<Vec<PreservedIssue>> {
    if restorable.is_empty() {
        return Ok(Vec::new());
    }
    let imported = super::parse_jsonl(source.reader())?;
    let mut plan = restorable.to_vec();
    for preserved in &mut plan {
        let Some(peer) = imported.get(&preserved.issue.id) else {
            continue;
        };
        // The audit normally excludes these rows. Planning must not turn a
        // stale caller's restore list into permission to resurrect a deletion.
        if peer.status == Status::Tombstone {
            return Err(BeadsError::Config(format!(
                "schema-heal comment plan refuses to restore {} over an imported tombstone",
                peer.id
            )));
        }
        require_same_restore_identity(&preserved.issue, peer)?;
        let local = preserved.comments.as_ref().ok_or_else(|| {
            BeadsError::Config(format!(
                "schema-heal cannot combine unreadable local comments for {}; rebuild not started",
                peer.id
            ))
        })?;
        if local
            .iter()
            .chain(&peer.comments)
            .any(|comment| comment.issue_id != peer.id)
        {
            return Err(BeadsError::Config(format!(
                "schema-heal comment ownership mismatch for {}; rebuild not started",
                peer.id
            )));
        }
        let combined = merge_comment_histories(local, &peer.comments);
        // Keep the independent loss guard, including occurrence counts. The
        // rehearsal checks it again against the normalized imported baseline.
        require_imported_comments_preserved(peer, &combined)?;
        preserved.issue.comments.clone_from(&combined);
        preserved.comments = Some(combined);
    }
    Ok(plan)
}

/// Revision precedence applies only to copies of the same task. In particular,
/// a newer local row can reach this plan without the audit's comments-only
/// identity check. A comment union must not legitimize replacing a different
/// task that happens to have the same ID (#512).
fn require_same_restore_identity(local: &Issue, imported: &Issue) -> Result<()> {
    // Match the legacy audit's persisted microsecond precision, including its
    // comments-only identity check; updated_at and comment rowids are not IDs.
    if local.id != imported.id
        || super::micros(local.created_at) != super::micros(imported.created_at)
    {
        return Err(BeadsError::SyncConflict {
            message: format!(
                "schema-heal ID collision: local issue {} (created {}) and imported issue {} \
                 (created {}) are not the same task; refusing to replace an imported issue \
                 or combine its comments. Reconcile the identities before healing; \
                 the live database rebuild was not started",
                local.id, local.created_at, imported.id, imported.created_at
            ),
        });
    }
    Ok(())
}

/// Multiset union by portable identity, never by clone-local rowid. Shared
/// occurrences appear once, but the larger occurrence count from either
/// history survives. Conflicting positive rowids request fresh IDs on replay.
fn merge_comment_histories(local: &[Comment], imported: &[Comment]) -> Vec<Comment> {
    let mut unmatched = BTreeMap::<_, usize>::new();
    for comment in local {
        *unmatched.entry(comment.sync_key()).or_default() += 1;
    }
    let mut combined = local.to_vec();
    for comment in imported {
        match unmatched.get_mut(&comment.sync_key()) {
            Some(count) if *count > 0 => *count -= 1,
            _ => combined.push(comment.clone()),
        }
    }
    combined.sort_by(|left, right| {
        left.sync_key()
            .cmp(&right.sync_key())
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut ids = BTreeSet::new();
    for comment in &mut combined {
        if comment.id <= 0 || !ids.insert(comment.id) {
            comment.id = 0;
        }
    }
    combined
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
        let jsonl_authority = sync::blocking_jsonl_family_write_lock_with_timeout(
            &jsonl_path,
            lock_timeout,
        )?;
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
            if let Some(imported) = witness.issues.get(&expected.id) {
                // Validate the normalized import too, before replacing the
                // baseline that the final replay verifier will trust.
                require_same_restore_identity(&expected, imported)?;
            }
            if let Some(labels) = &preserved.labels {
                expected.labels.clone_from(labels);
            }
            if let Some(dependencies) = &preserved.dependencies {
                expected.dependencies.clone_from(dependencies);
            }
            if let Some(comments) = &preserved.comments {
                if let Some(imported) = witness.issues.get(&expected.id) {
                    require_imported_comments_preserved(imported, comments)?;
                }
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

/// Restoring a newer local issue replaces its comment rows, not just its
/// scalar fields. A newer timestamp is not authority to erase peer comments
/// that exist only in the pinned JSONL. Check the imported baseline before
/// overlaying it; otherwise replay and its witness can agree on the same loss.
fn require_imported_comments_preserved(imported: &Issue, restored: &[Comment]) -> Result<()> {
    if !super::multiset_is_subset(
        imported.comments.iter().map(Comment::sync_key),
        restored.iter().map(Comment::sync_key),
    ) {
        return Err(BeadsError::Config(format!(
            "schema-heal preservation failed: restoring local issue {} would discard imported \
             comments; reconcile the local and JSONL comment histories before healing. \
             The live database rebuild was not started",
            imported.id
        )));
    }
    Ok(())
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

    fn comment(id: i64, text: &str) -> Comment {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "issue_id": "pf-abc",
            "author": "peer",
            "text": text,
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap()
    }

    fn isolated_rehearsal(name: &str) -> bool {
        // As in #506, spawn before any fixture lease can be inherited by
        // unrelated tests. The nested rebuild performs sole-opener checkpoints.
        const CHILD_ENV: &str = "BR_TEST_ISOLATED_HEAL_COMMENTS";
        if std::env::var(CHILD_ENV).as_deref() == Ok(name) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                name,
                "--test-threads=1",
                "--format=pretty",
                "--color=never",
            ])
            .env(CHILD_ENV, name)
            .env_remove("RUST_TEST_NOCAPTURE")
            .output()
            .expect("run isolated heal rehearsal");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let passed = format!("test {name} ... ok");
        assert!(
            output.status.success() && stdout.lines().any(|line| line == passed),
            "isolated rehearsal failed or did not run: {}\n{stdout}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    #[test]
    fn imported_comment_guard_uses_payloads_and_preserves_multiplicity() {
        let mut imported = issue("pf-abc");
        imported.comments = vec![comment(1, "shared"), comment(2, "peer-only")];
        assert!(require_imported_comments_preserved(&imported, &[]).is_err());
        assert!(require_imported_comments_preserved(&imported, &[comment(1, "shared")]).is_err());
        // Equal rowids are not evidence of equal comments across clones.
        let collision = [comment(1, "shared"), comment(2, "different local comment")];
        assert!(require_imported_comments_preserved(&imported, &collision).is_err());
        let mut complete = vec![comment(99, "peer-only"), comment(98, "shared")];
        complete.push(comment(100, "local-only"));
        require_imported_comments_preserved(&imported, &complete).unwrap();
        for field in 0..3 {
            let mut changed = complete.clone();
            match field {
                0 => changed[0].author = "another author".to_owned(),
                1 => changed[0].created_at += chrono::Duration::seconds(1),
                _ => changed[0].issue_id = "pf-other".to_owned(),
            }
            assert!(require_imported_comments_preserved(&imported, &changed).is_err());
        }
        imported.comments = vec![comment(1, "same payload"), comment(2, "same payload")];
        assert!(
            require_imported_comments_preserved(&imported, &[comment(99, "same payload")]).is_err()
        );
        require_imported_comments_preserved(
            &imported,
            &[comment(99, "same payload"), comment(100, "same payload")],
        )
        .unwrap();
    }

    #[test]
    fn rehearsal_refuses_local_restore_that_would_drop_a_peer_comment() {
        if isolated_rehearsal(concat!(
            "cli::commands::doctor_subsystems::schema_heal::rebuild_preflight::tests::",
            "rehearsal_refuses_local_restore_that_would_drop_a_peer_comment"
        )) {
            return;
        }
        let original = tempfile::tempdir().unwrap();
        let path = original.path().join("issues.jsonl");
        let mut imported = issue("pf-abc");
        imported.comments = vec![comment(1, "shared"), comment(2, "peer-only")];
        let bytes = format!("{}\n", serde_json::to_string(&imported).unwrap());
        fs::write(&path, &bytes).unwrap();
        let source = sync::capture_jsonl_source_snapshot(&path).unwrap();
        let mut local = imported.clone();
        local.title = "newer unflushed local title".to_owned();
        local.updated_at += chrono::Duration::seconds(1);
        local.comments = vec![comment(1, "shared"), comment(2, "local-only")];
        // Both payloads are valid and the local scalar edit is newer, but
        // replacing the imported comments would lose the peer's addition.
        let result = rehearse(
            &source,
            &[preserved(local)],
            &ConfigLayer::default(),
            Some(5_000),
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("a matching replay witness must not certify lost peer comments"),
        };
        let diagnostic = format!("{error:?}");
        assert!(
            diagnostic.contains("would discard imported comments"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("pf-abc"), "{diagnostic}");
        assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
        assert!(!original.path().join("beads.db").exists());
    }

    #[test]
    fn rehearsal_accepts_complete_comment_history_with_renumbered_ids() {
        if isolated_rehearsal(concat!(
            "cli::commands::doctor_subsystems::schema_heal::rebuild_preflight::tests::",
            "rehearsal_accepts_complete_comment_history_with_renumbered_ids"
        )) {
            return;
        }
        let original = tempfile::tempdir().unwrap();
        let path = original.path().join("issues.jsonl");
        let mut imported = issue("pf-abc");
        imported.comments = vec![comment(1, "shared"), comment(2, "peer-only")];
        let bytes = format!("{}\n", serde_json::to_string(&imported).unwrap());
        fs::write(&path, &bytes).unwrap();
        let source = sync::capture_jsonl_source_snapshot(&path).unwrap();
        let mut local = imported.clone();
        local.title = "newer unflushed local title".to_owned();
        local.updated_at += chrono::Duration::seconds(1);
        local.comments = vec![
            comment(99, "peer-only"),
            comment(98, "shared"),
            comment(100, "local-only"),
        ];
        let witness = rehearse(
            &source,
            &[preserved(local.clone())],
            &ConfigLayer::default(),
            Some(5_000),
        )
        .unwrap()
        .unwrap();
        assert_eq!(witness.issues.len(), 1);
        assert_eq!(witness.issues[&local.id].title, local.title);
        assert_eq!(witness.issues[&local.id].comments, local.comments);
        assert_eq!(witness.restored_ids, BTreeSet::from([local.id]));
        assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
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
        assert!(error.to_string().contains("live database rebuild was not started"));
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

    #[test]
    fn comment_union_is_lossless_idempotent_and_keeps_occurrence_counts() {
        for counts in 0_usize..256 {
            let local_shared = counts % 4;
            let local_only = (counts / 4) % 4;
            let peer_shared = (counts / 16) % 4;
            let peer_only = counts / 64;
            let mut local = vec![comment(7, "shared"); local_shared];
            local.extend(vec![comment(7, "local-only"); local_only]);
            let mut peer = vec![comment(7, "shared"); peer_shared];
            peer.extend(vec![comment(7, "peer-only"); peer_only]);
            let combined = merge_comment_histories(&local, &peer);
            for (body, expected) in [
                ("shared", local_shared.max(peer_shared)),
                ("local-only", local_only),
                ("peer-only", peer_only),
            ] {
                assert_eq!(
                    combined.iter().filter(|comment| comment.body == body).count(),
                    expected,
                    "{counts}: {body}"
                );
            }
            let positive: Vec<_> = combined.iter().filter(|comment| comment.id > 0).collect();
            assert_eq!(
                positive
                    .iter()
                    .map(|comment| comment.id)
                    .collect::<BTreeSet<_>>()
                    .len(),
                positive.len(),
                "colliding rowids must request allocation, never overwrite another comment"
            );
            let again = merge_comment_histories(&combined, &peer);
            let reverse = merge_comment_histories(&peer, &local);
            let keys: Vec<_> = combined.iter().map(Comment::sync_key).collect();
            assert_eq!(keys, again.iter().map(Comment::sync_key).collect::<Vec<_>>());
            assert_eq!(keys, reverse.iter().map(Comment::sync_key).collect::<Vec<_>>());
        }
    }

    #[test]
    fn comment_plan_refuses_unknown_histories_foreign_owners_and_tombstones() {
        let original = tempfile::tempdir().unwrap();
        let path = original.path().join("issues.jsonl");
        for mode in 0..4 {
            let mut imported = issue("pf-abc");
            imported.comments = vec![comment(1, "peer-only")];
            let mut local = preserved(issue("pf-abc"));
            match mode {
                0 => local.comments = None,
                1 => {
                    local.comments = Some(vec![Comment {
                        issue_id: "pf-other".to_owned(),
                        ..comment(2, "local-only")
                    }]);
                }
                2 => imported.comments[0].issue_id = "pf-other".to_owned(),
                _ => imported.status = Status::Tombstone,
            }
            let bytes = format!("{}\n", serde_json::to_string(&imported).unwrap());
            fs::write(&path, &bytes).unwrap();
            let source = sync::capture_jsonl_source_snapshot(&path).unwrap();
            assert!(
                complete_comment_restore_plan(&source, &[local]).is_err(),
                "{mode}"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn restore_identity_uses_persisted_creation_not_revision() {
        let imported = issue("pf-abc");
        let mut local = imported.clone();
        local.title = "a legitimate later edit".to_owned();
        local.updated_at += chrono::Duration::days(1);
        local.created_at += chrono::Duration::nanoseconds(999);
        require_same_restore_identity(&local, &imported).unwrap();
        local.created_at += chrono::Duration::nanoseconds(1);
        assert!(matches!(
            require_same_restore_identity(&local, &imported),
            Err(BeadsError::SyncConflict { .. })
        ));
        local.created_at = imported.created_at;
        local.id = "pf-other".to_owned();
        assert!(require_same_restore_identity(&local, &imported).is_err());
    }

    #[test]
    fn comment_plan_rejects_reused_ids_even_when_no_comments_would_be_lost() {
        for comments in [Vec::new(), vec![comment(1, "same complete history")]] {
            let original = tempfile::tempdir().unwrap();
            let path = original.path().join("issues.jsonl");
            let mut imported = issue("pf-abc");
            imported.comments = comments;
            let bytes = format!("{}\n", serde_json::to_string(&imported).unwrap());
            fs::write(&path, &bytes).unwrap();
            let source = sync::capture_jsonl_source_snapshot(&path).unwrap();
            let mut local = imported.clone();
            local.title = "newer local task".to_owned();
            local.updated_at += chrono::Duration::seconds(2);
            // A real edit of the same task is still accepted, with or without
            // comments. The following failure must be identity-specific.
            complete_comment_restore_plan(&source, &[preserved(local.clone())]).unwrap();
            for delta in [-1, 1] {
                local.created_at = imported.created_at + chrono::Duration::microseconds(delta);
                let restored = [preserved(local.clone())];
                let error = match complete_comment_restore_plan(&source, &restored) {
                    Err(error) => error,
                    Ok(_) => panic!("a newer revision cannot authorize a reused task ID"),
                };
                assert!(matches!(&error, BeadsError::SyncConflict { message }
                    if message.contains("schema-heal ID collision") && message.contains("pf-abc")));
                assert_eq!(restored[0].issue, local);
                assert_eq!(restored[0].comments.as_ref(), Some(&local.comments));
                assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn rehearsal_rechecks_identity_before_replacing_the_imported_witness() {
        if isolated_rehearsal(concat!(
            "cli::commands::doctor_subsystems::schema_heal::rebuild_preflight::tests::",
            "rehearsal_rechecks_identity_before_replacing_the_imported_witness"
        )) {
            return;
        }
        let original = tempfile::tempdir().unwrap();
        let path = original.path().join("issues.jsonl");
        let mut imported = issue("pf-abc");
        imported.comments = vec![comment(1, "complete imported history")];
        let bytes = format!("{}\n", serde_json::to_string(&imported).unwrap());
        fs::write(&path, &bytes).unwrap();
        let source = sync::capture_jsonl_source_snapshot(&path).unwrap();
        let mut local = imported.clone();
        local.created_at += chrono::Duration::seconds(1);
        local.updated_at += chrono::Duration::seconds(2);
        local.title = "different task under the reused ID".to_owned();
        // Bypass the planner deliberately: complete comments and a matching
        // post-replay payload must not certify replacement of another task.
        let error = match rehearse(
            &source,
            &[preserved(local)],
            &ConfigLayer::default(),
            Some(5_000),
        ) {
            Err(error) => error,
            Ok(_) => panic!("a local overlay must not erase the imported task's identity"),
        };
        let BeadsError::WithContext { source, .. } = &error else {
            panic!("expected a rehearsal failure: {error:?}");
        };
        assert!(matches!(source.as_ref(), BeadsError::SyncConflict { message }
            if message.contains("schema-heal ID collision") && message.contains("pf-abc")));
        assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
        assert!(!original.path().join("beads.db").exists());
    }

    #[test]
    fn completed_comment_plan_survives_rehearsal_and_replay() {
        if isolated_rehearsal(concat!(
            "cli::commands::doctor_subsystems::schema_heal::rebuild_preflight::tests::",
            "completed_comment_plan_survives_rehearsal_and_replay"
        )) {
            return;
        }
        let original = tempfile::tempdir().unwrap();
        let path = original.path().join("issues.jsonl");
        let mut imported = issue("pf-abc");
        imported.comments = vec![comment(1, "shared"), comment(2, "peer-only")];
        let bytes = format!("{}\n", serde_json::to_string(&imported).unwrap());
        fs::write(&path, &bytes).unwrap();
        let source = sync::capture_jsonl_source_snapshot(&path).unwrap();
        let mut local = imported.clone();
        local.title = "newer unflushed local title".to_owned();
        local.updated_at += chrono::Duration::seconds(1);
        local.source_repo_path = Some("/original/workspace".to_owned());
        local.comments = vec![comment(1, "shared"), comment(2, "local-only")];
        let audited = vec![preserved(local.clone())];
        let plan = complete_comment_restore_plan(&source, &audited).unwrap();
        assert_eq!(audited[0].comments.as_ref().unwrap(), &local.comments);
        assert_eq!(plan[0].issue.title, local.title);
        assert_eq!(Some(&plan[0].issue.comments), plan[0].comments.as_ref());
        let bodies: BTreeSet<_> = plan[0]
            .issue
            .comments
            .iter()
            .map(|comment| comment.body.as_str())
            .collect();
        assert_eq!(bodies, BTreeSet::from(["shared", "peer-only", "local-only"]));
        let witness = rehearse(&source, &plan, &ConfigLayer::default(), Some(5_000))
            .unwrap()
            .unwrap();
        // Replay the identical plan into a separate destination. Its verifier
        // must detect a later loss; completing the plan is not a weaker proof.
        let mut storage = SqliteStorage::open_memory().unwrap();
        storage.create_issue(&imported, "test").unwrap();
        storage
            .sync_comments_for_import(&imported.id, &imported.comments)
            .unwrap();
        sync::restore_dirty_issues_after_rebuild(&mut storage, &plan).unwrap();
        witness.verify(&storage).unwrap();
        sync::restore_dirty_issues_after_rebuild(&mut storage, &plan).unwrap();
        witness.verify(&storage).unwrap();
        storage
            .sync_comments_for_import(&local.id, &local.comments)
            .unwrap();
        assert!(
            witness.verify(&storage).is_err(),
            "dropping the peer comment must fail verification"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
        assert!(!original.path().join("beads.db").exists());
    }
}
