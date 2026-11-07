//! Versioned v13 hash semantics and an exact projected issue-row witness.
//!
//! Old receipts remain readable for recognizing and undoing an installed
//! generation. Every fresh apply requires the profile that retains dirty and
//! export records; an old pre-install token cannot select the new plan.

use std::collections::BTreeMap;

use super::{
    BeadsError, Connection, LegacyTableProjection, MigrationForecast, Result, SqliteValue,
    legacy_projection_rowid, preservation_text, quote_identifier, reviewed_rows_projection,
};
use crate::storage::schema::hash_refresh as schema;

#[derive(Debug)]
pub(super) struct CandidateIssueProjection {
    pub(super) columns: Vec<String>,
    pub(super) projection: LegacyTableProjection,
}

fn refused(detail: impl std::fmt::Display) -> BeadsError {
    BeadsError::Config(format!(
        "reviewed schema13 hash refresh refused: {detail}; preserve the original database \
         and run `br doctor migrate-schema plan` again"
    ))
}

pub(super) fn plan_profile(conn: &Connection, from: u32) -> Result<Option<String>> {
    if from != 13 {
        return Ok(None);
    }
    // Decode before a token or recovery run is issued, not after candidate
    // construction. No unsupported value may silently become a hash default.
    schema::content_hashes(conn)?;
    Ok(Some(schema::PROFILE.to_owned()))
}

pub(super) fn preserves_bookkeeping(forecast: &MigrationForecast) -> Result<bool> {
    match (
        forecast.from_version,
        forecast.content_hash_refresh_profile.as_deref(),
    ) {
        (13, Some(schema::PROFILE)) => Ok(true),
        (_, None) => Ok(false),
        _ => Err(refused(
            "the receipt has an unknown or misplaced hash profile",
        )),
    }
}

pub(super) fn require_fresh_profile(forecast: &MigrationForecast) -> Result<()> {
    if !preserves_bookkeeping(forecast)? && forecast.from_version == 13 {
        return Err(refused(
            "an old hash-refresh receipt cannot authorize installing a new candidate",
        ));
    }
    Ok(())
}

/// The source proof keeps the original `content_hash` too. Only the candidate
/// proof substitutes its derived value and the promised empty prerequisite
/// default, while preserving all other SQL values and the hidden rowid.
pub(super) fn project_source_issues(
    conn: &Connection,
    source_columns: &[String],
    prerequisites_added: bool,
) -> Result<CandidateIssueProjection> {
    let hashes: BTreeMap<_, _> = schema::content_hashes(conn)?.into_iter().collect();
    let rowid = legacy_projection_rowid(conn, "issues")?;
    let prefix = rowid.map_or_else(String::new, |alias| format!("{alias}, "));
    let selected = source_columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    let position = |name| {
        source_columns
            .iter()
            .position(|column| column == name)
            .map(|index| index + usize::from(rowid.is_some()))
            .ok_or_else(|| {
                refused(format!(
                    "issues.{name} is absent from the source projection"
                ))
            })
    };
    let id_column = position("id")?;
    let hash_column = position("content_hash")?;
    let mut columns = source_columns.to_vec();
    if prerequisites_added {
        if columns.iter().any(|column| column == "prerequisites") {
            return Err(refused("the promised prerequisite addition already exists"));
        }
        columns.push("prerequisites".to_owned());
    }
    let mut rows = Vec::new();
    for row in conn.query(&format!("SELECT {prefix}{selected} FROM main.issues"))? {
        let mut values = row.values().to_vec();
        let id = preservation_text(values.get(id_column), "issue id")?;
        let hash = hashes
            .get(&id)
            .ok_or_else(|| refused(format!("issue {id} has no decoded destination hash")))?;
        let value = values
            .get_mut(hash_column)
            .ok_or_else(|| refused(format!("issue {id} has no projected hash column")))?;
        *value = SqliteValue::from(hash.as_str());
        if prerequisites_added {
            values.push(SqliteValue::from(""));
        }
        rows.push(values);
    }
    Ok(CandidateIssueProjection {
        columns,
        projection: reviewed_rows_projection("issues", rowid.is_some(), &rows)?,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{self as lifecycle, tests::reviewed_source_migration_context};
    use super::*;

    #[test]
    fn preserving_profile_changes_the_token_without_reinterpreting_old_receipts() {
        let (_temp, migration) = reviewed_source_migration_context("beads.db", 13);
        let plan = lifecycle::build_plan(&migration.db_path).unwrap();
        let mut forecast = plan.forecast.unwrap();
        assert_eq!(
            forecast.content_hash_refresh_profile.as_deref(),
            Some(schema::PROFILE)
        );
        require_fresh_profile(&forecast).unwrap();
        let encoded = serde_json::to_value(&forecast).unwrap();
        assert_eq!(encoded["content_hash_refresh_profile"], schema::PROFILE);

        forecast.content_hash_refresh_profile = None;
        let old_token =
            lifecycle::compute_plan_token(&plan.database_path, &plan.logical_witness, &forecast)
                .unwrap();
        assert_ne!(plan.plan_token.as_deref(), Some(old_token.as_str()));
        let encoded = serde_json::to_value(&forecast).unwrap();
        assert!(encoded.get("content_hash_refresh_profile").is_none());
        let restored: MigrationForecast = serde_json::from_value(encoded).unwrap();
        assert!(!preserves_bookkeeping(&restored).unwrap());
        assert!(require_fresh_profile(&restored).is_err());

        forecast.content_hash_refresh_profile = Some("unreviewed-v2".to_owned());
        assert!(preserves_bookkeeping(&forecast).is_err());
        forecast.content_hash_refresh_profile = Some(schema::PROFILE.to_owned());
        forecast.from_version = 14;
        assert!(preserves_bookkeeping(&forecast).is_err());
    }

    fn migrate_test_candidate(path: &std::path::Path) {
        let conn = Connection::open(path.to_string_lossy().into_owned()).unwrap();
        conn.execute("BEGIN IMMEDIATE").unwrap();
        lifecycle::run_reviewed_schema_migration_steps_in_transaction(
            &conn,
            13,
            lifecycle::current_schema_version().unwrap(),
            "receipt-time",
        )
        .unwrap();
        conn.execute("COMMIT").unwrap();
        lifecycle::close_connection(conn).unwrap();
    }

    #[test]
    fn new_profile_proves_exact_hashes_bookkeeping_and_original_rowids() {
        for (table, mutation) in [
            (
                "issues",
                "UPDATE issues SET content_hash='unreviewed-derived-hash'",
            ),
            ("issues", "UPDATE issues SET rowid=901"),
            (
                "issues",
                "UPDATE issues SET prerequisites='invented prerequisite'",
            ),
            ("dirty_issues", "DELETE FROM dirty_issues"),
            (
                "dirty_issues",
                "UPDATE dirty_issues SET marked_at='replacement-mark'",
            ),
            ("dirty_issues", "UPDATE dirty_issues SET rowid=902"),
            ("export_hashes", "DELETE FROM export_hashes"),
            (
                "export_hashes",
                "UPDATE export_hashes SET content_hash=X'00FF'",
            ),
            ("export_hashes", "UPDATE export_hashes SET rowid=903"),
        ] {
            let (_temp, migration) = reviewed_source_migration_context("beads.db", 13);
            let forecast = lifecycle::build_plan(&migration.db_path)
                .unwrap()
                .forecast
                .unwrap();
            let proof =
                lifecycle::capture_reviewed_state_preservation(&migration.db_path, &forecast)
                    .unwrap();
            migrate_test_candidate(&migration.db_path);
            lifecycle::verify_reviewed_state_preservation(&migration.db_path, proof.as_ref(), true)
                .expect("the exact projected upgrade retains every original record");
            let conn = Connection::open(migration.db_path.to_string_lossy().into_owned()).unwrap();
            conn.execute(mutation).unwrap();
            lifecycle::close_connection(conn).unwrap();
            let error = lifecycle::verify_reviewed_state_preservation(
                &migration.db_path,
                proof.as_ref(),
                true,
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("table {table} changed")),
                "{mutation}: {error}"
            );
        }
    }

    #[test]
    fn source_recheck_keeps_original_hashes_bound_before_candidate_installation() {
        let (_temp, migration) = reviewed_source_migration_context("beads.db", 13);
        let forecast = lifecycle::build_plan(&migration.db_path)
            .unwrap()
            .forecast
            .unwrap();
        let proof =
            lifecycle::capture_reviewed_state_preservation(&migration.db_path, &forecast).unwrap();
        let conn = Connection::open(migration.db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute("UPDATE issues SET content_hash='concurrent-identity-change'")
            .unwrap();
        lifecycle::close_connection(conn).unwrap();
        let error = lifecycle::verify_reviewed_state_preservation(
            &migration.db_path,
            proof.as_ref(),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("table issues changed"));
    }

    #[test]
    fn malformed_hash_input_refuses_before_a_plan_or_recovery_run() {
        let (_temp, migration) = reviewed_source_migration_context("beads.db", 13);
        let conn = Connection::open(migration.db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute("UPDATE issues SET assignee=X'00FF'").unwrap();
        lifecycle::close_connection(conn).unwrap();
        let before = crate::storage::sqlite::database_family_snapshot(&migration.db_path).unwrap();
        let error = lifecycle::build_observational_plan(&migration.db_path).unwrap_err();
        assert!(error.to_string().contains("content hash refresh refused"));
        assert_eq!(
            crate::storage::sqlite::database_family_snapshot(&migration.db_path).unwrap(),
            before
        );
        assert!(!lifecycle::migration_runs_root(&migration.beads_dir).exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    mod legacy_receipts {
        use std::fs;
        use std::path::PathBuf;
        use std::sync::Arc;

        use super::*;

        struct LegacyRun {
            run_dir: PathBuf,
            candidate: PathBuf,
            prepared: lifecycle::PreparedMigrationReceipt,
        }

        fn old_candidate_effects(
            path: &std::path::Path,
        ) -> lifecycle::ReviewedSchemaMigrationEffectsReceipt {
            let conn = Connection::open(path.to_string_lossy().into_owned()).unwrap();
            conn.execute("BEGIN IMMEDIATE").unwrap();
            let effects = lifecycle::run_reviewed_schema_migration_steps_in_transaction(
                &conn,
                13,
                lifecycle::current_schema_version().unwrap(),
                "old-receipt-time",
            )
            .unwrap();
            // Reproduce the old binary's durable after-state explicitly. These
            // writes belong only to the archived-receipt fixture, never to apply.
            crate::storage::schema::execute_batch(
                &conn,
                r"
            DELETE FROM dirty_issues WHERE issue_id IN (SELECT id FROM issues);
            INSERT INTO dirty_issues (issue_id, marked_at)
                SELECT id, 'old-receipt-time' FROM issues ORDER BY id;
            DELETE FROM export_hashes;
        ",
            )
            .unwrap();
            conn.execute("COMMIT").unwrap();
            if !crate::storage::schema::issues_required_checks_canonical(&conn) {
                crate::storage::schema::rebuild_issues_table(&conn).unwrap();
            }
            conn.execute("REINDEX").unwrap();
            conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
            assert!(lifecycle::runtime_schema_compatible(&conn));
            lifecycle::close_connection(conn).unwrap();
            let mut effects = lifecycle::ReviewedSchemaMigrationEffectsReceipt::from(effects);
            effects.post_migration_maintenance_completed = true;
            effects
        }

        fn legacy_commit_ready(migration: &lifecycle::MigrationContext) -> LegacyRun {
            let plan = lifecycle::build_plan(&migration.db_path).unwrap();
            let mut forecast = plan.forecast.unwrap();
            forecast.content_hash_refresh_profile = None;
            let plan_token = lifecycle::compute_plan_token(
                &plan.database_path,
                &plan.logical_witness,
                &forecast,
            )
            .unwrap();
            let run_id = lifecycle::allocate_run_id(&migration.beads_dir).unwrap();
            let run_dir = lifecycle::migration_runs_root(&migration.beads_dir).join(&run_id);
            let before = run_dir.join("before");
            lifecycle::ensure_new_directory(&before).unwrap();
            lifecycle::copy_family_to_backup(&migration.db_path, &before, &plan.raw_witness)
                .unwrap();
            lifecycle::verify_backup_family(&migration.db_path, &before, &plan.raw_witness)
                .unwrap();
            let prepared = lifecycle::PreparedMigrationReceipt {
                schema_version: lifecycle::PREPARED_SCHEMA.to_owned(),
                run_id,
                database_path: plan.database_path,
                plan_token,
                marked_at: "old-receipt-time".to_owned(),
                forecast,
                raw_before: plan.raw_witness,
                logical_before: plan.logical_witness,
            };
            lifecycle::write_json_new(&run_dir.join("prepared.json"), &prepared).unwrap();
            let candidate =
                lifecycle::maintenance_candidate_path(&migration.db_path, &run_dir).unwrap();
            for component in &prepared.raw_before.components {
                if component.present {
                    let source = lifecycle::backup_component_path(
                        &before,
                        &migration.db_path,
                        &component.suffix,
                    )
                    .unwrap();
                    let destination =
                        lifecycle::family_component_path(&candidate, &component.suffix);
                    lifecycle::copy_regular_file_new(&source, &destination, component.unix_mode)
                        .unwrap();
                }
            }
            let effects = old_candidate_effects(&candidate);
            let after = lifecycle::logical_witness(&candidate).unwrap();
            lifecycle::persist_commit_ready_marker(&run_dir, &migration.db_path, &after, effects)
                .unwrap();
            LegacyRun {
                run_dir,
                candidate,
                prepared,
            }
        }

        #[test]
        fn old_preinstall_v13_receipt_keeps_original_and_cannot_install_destructive_candidate() {
            let (_temp, migration) = reviewed_source_migration_context("beads.db", 13);
            let legacy = legacy_commit_ready(&migration);
            let original_bytes = fs::read(&migration.db_path).unwrap();
            let candidate_bytes = fs::read(&legacy.candidate).unwrap();
            let error = lifecycle::apply_with_plan_token(&legacy.prepared.plan_token, &migration)
                .unwrap_err();
            assert!(error.to_string().contains("plan token is stale"), "{error}");
            assert_eq!(fs::read(&migration.db_path).unwrap(), original_bytes);
            assert_eq!(fs::read(&legacy.candidate).unwrap(), candidate_bytes);
            assert_eq!(
                lifecycle::logical_witness(&migration.db_path).unwrap(),
                legacy.prepared.logical_before
            );
            lifecycle::verify_backup_family(
                &migration.db_path,
                &legacy.run_dir.join("before"),
                &legacy.prepared.raw_before,
            )
            .unwrap();
            assert!(legacy.run_dir.join("failed.json").is_file());
            assert!(!legacy.run_dir.join("applied.json").exists());
            assert_eq!(
                fs::read_dir(lifecycle::migration_runs_root(&migration.beads_dir))
                    .unwrap()
                    .count(),
                1
            );
            let fresh = lifecycle::build_plan(&migration.db_path).unwrap();
            assert_ne!(
                fresh.plan_token.as_deref(),
                Some(legacy.prepared.plan_token.as_str())
            );
            assert!(preserves_bookkeeping(fresh.forecast.as_ref().unwrap()).unwrap());
        }

        fn install_archived_candidate(migration: &lifecycle::MigrationContext, legacy: &LegacyRun) {
            let source = lifecycle::raw_family_witness(&migration.db_path).unwrap();
            let candidate = lifecycle::raw_family_witness(&legacy.candidate).unwrap();
            lifecycle::move_present_sidecars_new(
                &legacy.candidate,
                &legacy.run_dir.join("maintenance-candidate-sidecars"),
                &candidate,
                "candidate-sidecar-retention",
            )
            .unwrap();
            let displaced = legacy.run_dir.join("maintenance-displaced");
            lifecycle::ensure_new_directory(&displaced).unwrap();
            lifecycle::move_present_sidecars_new(
                &migration.db_path,
                &displaced,
                &source,
                "displace-live-sidecars",
            )
            .unwrap();
            let replacement = migration
                .write_authority
                .lock_database_replacement_candidate(&legacy.candidate)
                .unwrap();
            let displaced_main =
                lifecycle::backup_component_path(&displaced, &migration.db_path, "").unwrap();
            lifecycle::install_compacted_candidate(
                &legacy.candidate,
                &migration.db_path,
                &displaced_main,
                replacement,
                &migration.write_authority,
            )
            .unwrap();
        }

        #[test]
        fn old_installed_v13_receipt_can_resume_and_restore_its_original_records() {
            let (_temp, migration) = reviewed_source_migration_context("beads.db", 13);
            let legacy = legacy_commit_ready(&migration);
            let original_bytes = fs::read(&migration.db_path).unwrap();
            install_archived_candidate(&migration, &legacy);
            let beads_dir = migration.beads_dir.clone();
            let db_path = migration.db_path.clone();
            drop(migration);
            let migration = lifecycle::MigrationContext {
                write_authority: Arc::new(
                    crate::sync::blocking_database_family_write_lock_with_timeout(
                        &beads_dir,
                        &db_path,
                        Some(1000),
                    )
                    .unwrap(),
                ),
                beads_dir,
                db_path,
            };
            let (applied, _) = {
                let mut exclusion = lifecycle::exclude_migration_openers(&migration).unwrap();
                lifecycle::resume_commit_ready_migration(
                    &legacy.prepared.plan_token,
                    &migration,
                    &mut exclusion,
                )
                .unwrap()
                .expect("an already-installed historical generation remains recoverable")
            };
            assert!(applied.attested);
            assert!(applied.forecast.content_hash_refresh_profile.is_none());
            lifecycle::execute_undo(
                &lifecycle::DoctorMigrateSchemaUndoArgs {
                    run_id: applied.run_id,
                    dry_run: false,
                    json: false,
                },
                &migration,
            )
            .unwrap();
            assert_eq!(fs::read(&migration.db_path).unwrap(), original_bytes);
            assert_eq!(
                lifecycle::logical_witness(&migration.db_path).unwrap(),
                legacy.prepared.logical_before
            );
            assert!(legacy.run_dir.join("undone.json").is_file());
        }
    }
}
