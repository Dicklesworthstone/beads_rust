//! Explicit, receipt-bound schema migration lifecycle.
//!
//! Ordinary storage opens never cross a schema-version boundary. This module
//! is the sole operator-facing path for a reviewed migration:
//!
//! 1. `plan` observes the complete logical database plus the raw SQLite file
//!    family and emits a deterministic token over the logical state. Raw
//!    sidecar bytes are reported but not token-bound because checkpoint and
//!    close can rewrite them without changing database semantics.
//! 2. `apply` recomputes that logical plan under database-family write
//!    authority, refuses semantic drift, writes a verified recovery bundle of
//!    the then-current raw family, and then runs only the reviewed migration
//!    steps in one `BEGIN IMMEDIATE` transaction. After commit it checkpoints,
//!    rebuilds indexes, rewrites database pages, closes the writer, and requires
//!    a clean all-row integrity result from a fresh connection. A current-schema
//!    database with only known page-layout diagnostics can use the same
//!    receipt-bound path for maintenance without replaying schema steps.
//! 3. `undo` verifies that the live logical state is still the exact applied
//!    state, quarantines every current family member, and restores every
//!    pre-migration byte without deleting anything.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use fsqlite::Connection;
use fsqlite::compat::{OpenFlags, open_with_flags};
use fsqlite_types::SqliteValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::{
    DoctorMigrateSchemaApplyArgs, DoctorMigrateSchemaArgs, DoctorMigrateSchemaCommand,
    DoctorMigrateSchemaPlanArgs, DoctorMigrateSchemaUndoArgs,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::output::OutputContext;
use crate::storage::schema::{
    CURRENT_SCHEMA_VERSION, REVIEWED_MIGRATION_SOURCE_VERSIONS, ReviewedSchemaMigrationEffects,
    run_reviewed_schema_migration_steps_in_transaction, runtime_schema_compatible,
};
use crate::sync::DatabaseFamilyWriteLock;

const PLAN_SCHEMA: &str = "br.doctor.schema_migration.plan.v1";
const PREPARED_SCHEMA: &str = "br.doctor.schema_migration.prepared.v1";
const APPLIED_SCHEMA: &str = "br.doctor.schema_migration.applied.v1";
const FAILED_SCHEMA: &str = "br.doctor.schema_migration.failed.v1";
const UNDO_SCHEMA: &str = "br.doctor.schema_migration.undo.v1";
const FAMILY_SUFFIXES: &[&str] = &["", "-wal", "-shm", "-journal"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RawComponentWitness {
    suffix: String,
    present: bool,
    length: Option<u64>,
    sha256: Option<String>,
    unix_mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RawFamilyWitness {
    components: Vec<RawComponentWitness>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LogicalTableWitness {
    name: String,
    row_count: u64,
    rows_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LogicalDatabaseWitness {
    user_version: u32,
    integrity_check: String,
    schema_sha256: String,
    contents_sha256: String,
    tables: Vec<LogicalTableWitness>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct MigrationForecast {
    from_version: u32,
    to_version: u32,
    content_hash_rows_rebuilt: usize,
    gate_result_history_created: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    post_migration_maintenance: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MigrationPlanReceipt {
    schema_version: String,
    eligible: bool,
    database_path: String,
    from_version: u32,
    to_version: u32,
    raw_witness: RawFamilyWitness,
    logical_witness: LogicalDatabaseWitness,
    forecast: Option<MigrationForecast>,
    plan_token: Option<String>,
    apply_command: Option<String>,
    note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PreparedMigrationReceipt {
    schema_version: String,
    run_id: String,
    database_path: String,
    plan_token: String,
    marked_at: String,
    forecast: MigrationForecast,
    raw_before: RawFamilyWitness,
    logical_before: LogicalDatabaseWitness,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppliedMigrationReceipt {
    schema_version: String,
    run_id: String,
    database_path: String,
    plan_token: String,
    prepared_receipt_sha256: String,
    marked_at: String,
    forecast: MigrationForecast,
    effects: ReviewedSchemaMigrationEffectsReceipt,
    raw_before: RawFamilyWitness,
    logical_before: LogicalDatabaseWitness,
    raw_after: Option<RawFamilyWitness>,
    logical_after: Option<LogicalDatabaseWitness>,
    attested: bool,
    attestation_errors: Vec<String>,
    undo_command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FailedMigrationReceipt {
    schema_version: String,
    run_id: String,
    database_path: String,
    plan_token: String,
    marked_at: String,
    error: String,
    raw_before: RawFamilyWitness,
    logical_before: LogicalDatabaseWitness,
    raw_observed_after_failure: Option<RawFamilyWitness>,
    logical_observed_after_failure: Option<LogicalDatabaseWitness>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct ReviewedSchemaMigrationEffectsReceipt {
    from_version: u32,
    to_version: u32,
    content_hash_rows_rebuilt: usize,
    gate_result_history_created: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    post_migration_maintenance_completed: bool,
}

impl From<ReviewedSchemaMigrationEffects> for ReviewedSchemaMigrationEffectsReceipt {
    fn from(value: ReviewedSchemaMigrationEffects) -> Self {
        Self {
            from_version: value.from_version,
            to_version: value.to_version,
            content_hash_rows_rebuilt: value.content_hash_rows_rebuilt,
            gate_result_history_created: value.gate_result_history_created,
            post_migration_maintenance_completed: false,
        }
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UndoReceipt {
    schema_version: String,
    run_id: String,
    dry_run: bool,
    database_path: String,
    quarantine_path: String,
    applied_receipt_sha256: String,
    raw_expected_before: RawFamilyWitness,
    logical_expected_before: LogicalDatabaseWitness,
    raw_live_before_undo: RawFamilyWitness,
    logical_live_before_undo: Option<LogicalDatabaseWitness>,
    raw_restored: Option<RawFamilyWitness>,
    logical_restored: Option<LogicalDatabaseWitness>,
}

#[derive(Serialize)]
struct PlanTokenMaterial<'a> {
    contract: &'static str,
    database_path: &'a str,
    from_version: u32,
    to_version: u32,
    logical_witness: &'a LogicalDatabaseWitness,
    forecast: &'a MigrationForecast,
}

struct MigrationContext {
    beads_dir: PathBuf,
    db_path: PathBuf,
    _write_authority: Arc<DatabaseFamilyWriteLock>,
}

/// Execute `br doctor migrate-schema ...`.
///
/// # Errors
///
/// Returns a fail-closed diagnostic when authority, plan-token, recovery
/// bundle, migration, or restore verification fails.
pub fn execute(
    args: &DoctorMigrateSchemaArgs,
    cli: &config::CliOverrides,
    _ctx: &OutputContext,
) -> Result<()> {
    let migration = resolve_context(cli)?;
    match &args.command {
        DoctorMigrateSchemaCommand::Plan(plan) => execute_plan(plan, &migration),
        DoctorMigrateSchemaCommand::Apply(apply) => execute_apply(apply, &migration),
        DoctorMigrateSchemaCommand::Undo(undo) => execute_undo(undo, &migration),
    }
}

fn resolve_context(cli: &config::CliOverrides) -> Result<MigrationContext> {
    let beads_dir =
        config::discover_optional_beads_dir_with_cli(cli)?.ok_or(BeadsError::NotInitialized)?;
    let paths = config::resolve_paths(&beads_dir, cli.db.as_ref())?;
    let write_authority = if let Some(authority) =
        cli.database_family_write_authority_for(&beads_dir, &paths.db_path)
    {
        authority.verify_database_authority()?;
        Arc::clone(authority)
    } else {
        Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                &beads_dir,
                &paths.db_path,
                cli.lock_timeout,
            )?,
        )
    };
    write_authority.verify_database_authority()?;
    Ok(MigrationContext {
        beads_dir,
        db_path: paths.db_path,
        _write_authority: write_authority,
    })
}

fn execute_plan(args: &DoctorMigrateSchemaPlanArgs, migration: &MigrationContext) -> Result<()> {
    let plan = build_plan(&migration.db_path)?;
    emit_plan(&plan, args.json)?;
    Ok(())
}

fn build_plan(db_path: &Path) -> Result<MigrationPlanReceipt> {
    refuse_non_regular_component(db_path)?;
    let logical_witness = logical_witness(db_path)?;
    let raw_witness = raw_family_witness(db_path)?;
    let target = current_schema_version()?;
    let from = logical_witness.user_version;
    let database_path = db_path.display().to_string();
    let integrity_clean = integrity_check_is_clean(&logical_witness.integrity_check);

    if from == target {
        if !current_runtime_shape_is_canonical(db_path)? {
            return Err(BeadsError::internal(
                "database declares the current schema version but its runtime shape is not \
                 canonical; refusing to issue a migration no-op receipt",
            ));
        }
        if !integrity_clean {
            if !integrity_check_is_repairable(&logical_witness.integrity_check) {
                return Err(BeadsError::internal(format!(
                    "database declares the current schema version but its integrity diagnostics \
                     are not eligible for reviewed page-layout maintenance: {:?}",
                    logical_witness.integrity_check
                )));
            }
            let forecast = MigrationForecast {
                from_version: from,
                to_version: target,
                content_hash_rows_rebuilt: 0,
                gate_result_history_created: false,
                post_migration_maintenance: true,
            };
            let plan_token = compute_plan_token(&database_path, &logical_witness, &forecast)?;
            return Ok(MigrationPlanReceipt {
                schema_version: PLAN_SCHEMA.to_string(),
                eligible: true,
                database_path,
                from_version: from,
                to_version: target,
                raw_witness,
                logical_witness,
                forecast: Some(forecast),
                apply_command: Some(format!(
                    "br doctor migrate-schema apply --plan-token {plan_token}"
                )),
                plan_token: Some(plan_token),
                note: "the current schema has a repairable page-layout diagnostic; apply will \
                       preserve a complete recovery bundle, checkpoint, rebuild indexes, rewrite \
                       the database pages, and require a clean fresh-connection integrity check"
                    .to_string(),
            });
        }
        return Ok(MigrationPlanReceipt {
            schema_version: PLAN_SCHEMA.to_string(),
            eligible: false,
            database_path,
            from_version: from,
            to_version: target,
            raw_witness,
            logical_witness,
            forecast: None,
            plan_token: None,
            apply_command: None,
            note: "database already has the current canonical schema; no migration is needed"
                .to_string(),
        });
    }
    if !REVIEWED_MIGRATION_SOURCE_VERSIONS.contains(&from) {
        return Err(BeadsError::internal(format!(
            "reviewed schema migration is available only from source schemas 13, 14, 15, and 16 \
             to {target}; observed unsupported source version {from}"
        )));
    }
    if !integrity_clean && !integrity_check_is_repairable(&logical_witness.integrity_check) {
        return Err(BeadsError::internal(format!(
            "schema migration refused because PRAGMA integrity_check returned diagnostics that \
             are not eligible for reviewed page-layout maintenance: {:?}",
            logical_witness.integrity_check
        )));
    }

    let conn = open_read_only(db_path)?;
    require_source_tables(&conn, from)?;
    let issue_count = query_count(&conn, "SELECT COUNT(*) FROM issues")?;
    let gate_result_history_created = !named_table_exists(&conn, "gate_result_history")?;
    close_connection(conn)?;

    let forecast = MigrationForecast {
        from_version: from,
        to_version: target,
        content_hash_rows_rebuilt: if from == 13 {
            usize::try_from(issue_count).map_err(|_| {
                BeadsError::internal(format!(
                    "issue count {issue_count} cannot be represented on this platform"
                ))
            })?
        } else {
            0
        },
        gate_result_history_created,
        post_migration_maintenance: true,
    };
    let plan_token = compute_plan_token(&database_path, &logical_witness, &forecast)?;

    Ok(MigrationPlanReceipt {
        schema_version: PLAN_SCHEMA.to_string(),
        eligible: true,
        database_path,
        from_version: from,
        to_version: target,
        raw_witness,
        logical_witness,
        forecast: Some(forecast),
        apply_command: Some(format!(
            "br doctor migrate-schema apply --plan-token {plan_token}"
        )),
        plan_token: Some(plan_token),
        note: if integrity_clean {
            "review the forecast and retain this receipt; apply will recompute the complete \
             logical witness and refuse semantic drift, then back up the current raw SQLite \
             family before migration and mandatory post-migration page maintenance"
                .to_string()
        } else {
            "the supported source schema has a repairable page-layout diagnostic; review the \
             forecast and retain this receipt. Apply will recompute the complete logical witness, \
             back up the current raw SQLite family, migrate it, run mandatory page-layout \
             maintenance, and require a clean fresh-connection integrity check"
                .to_string()
        },
    })
}

fn emit_plan(plan: &MigrationPlanReceipt, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(plan).map_err(BeadsError::Json)?
        );
        return Ok(());
    }
    if plan.eligible {
        println!(
            "Reviewed schema migration: {} -> {}",
            plan.from_version, plan.to_version
        );
        println!(
            "Plan token: {}",
            plan.plan_token.as_deref().unwrap_or_default()
        );
        println!(
            "Apply: {}",
            plan.apply_command.as_deref().unwrap_or_default()
        );
    } else {
        println!("{}", plan.note);
    }
    Ok(())
}

fn execute_apply(args: &DoctorMigrateSchemaApplyArgs, migration: &MigrationContext) -> Result<()> {
    if args.plan_token.trim().is_empty() {
        return Err(BeadsError::internal(
            "schema migration apply requires a non-empty --plan-token",
        ));
    }
    let plan = build_plan(&migration.db_path)?;
    let Some(recomputed_token) = plan.plan_token.as_deref() else {
        return Err(BeadsError::internal(
            "schema migration apply refused because no migration is currently eligible",
        ));
    };
    if !constant_time_text_eq(recomputed_token, args.plan_token.trim()) {
        return Err(BeadsError::internal(format!(
            "schema migration plan token is stale or belongs to a different database state \
             (provided {}, recomputed {}); run `br doctor migrate-schema plan` again",
            args.plan_token.trim(),
            recomputed_token
        )));
    }
    let forecast = plan
        .forecast
        .clone()
        .ok_or_else(|| BeadsError::internal("eligible migration plan omitted its forecast"))?;

    let run_id = allocate_run_id(&migration.beads_dir)?;
    let run_dir = migration_runs_root(&migration.beads_dir).join(&run_id);
    let before_dir = run_dir.join("before");
    ensure_new_directory(&before_dir)?;
    copy_family_to_backup(&migration.db_path, &before_dir, &plan.raw_witness)?;
    verify_backup_family(&migration.db_path, &before_dir, &plan.raw_witness)?;

    let marked_at = Utc::now().to_rfc3339();
    let prepared = PreparedMigrationReceipt {
        schema_version: PREPARED_SCHEMA.to_string(),
        run_id: run_id.clone(),
        database_path: plan.database_path.clone(),
        plan_token: recomputed_token.to_string(),
        marked_at: marked_at.clone(),
        forecast: forecast.clone(),
        raw_before: plan.raw_witness.clone(),
        logical_before: plan.logical_witness.clone(),
    };
    write_json_new(&run_dir.join("prepared.json"), &prepared)?;
    let prepared_receipt_sha256 = file_sha256(&run_dir.join("prepared.json"))?;
    sync_directory(&run_dir)?;

    migration._write_authority.verify_database_authority()?;
    let migration_result = apply_reviewed_migration(
        &migration.db_path,
        forecast.from_version,
        forecast.to_version,
        &marked_at,
        &run_dir,
        &migration._write_authority,
    );
    migration._write_authority.verify_database_authority()?;
    let effects = match migration_result {
        Ok(effects) => effects,
        Err(error) => {
            let failed = FailedMigrationReceipt {
                schema_version: FAILED_SCHEMA.to_string(),
                run_id: run_id.clone(),
                database_path: plan.database_path,
                plan_token: recomputed_token.to_string(),
                marked_at,
                error: error.to_string(),
                raw_before: plan.raw_witness,
                logical_before: plan.logical_witness,
                raw_observed_after_failure: raw_family_witness(&migration.db_path).ok(),
                logical_observed_after_failure: logical_witness(&migration.db_path).ok(),
            };
            write_json_new(&run_dir.join("failed.json"), &failed)?;
            return Err(BeadsError::WithContext {
                context: format!(
                    "reviewed schema migration run {run_id} failed; the verified pre-state \
                     remains at {}",
                    before_dir.display()
                ),
                source: Box::new(error),
            });
        }
    };

    let mut attestation_errors = Vec::new();
    if effects.from_version != forecast.from_version
        || effects.to_version != forecast.to_version
        || effects.content_hash_rows_rebuilt != forecast.content_hash_rows_rebuilt
        || effects.gate_result_history_created != forecast.gate_result_history_created
        || effects.post_migration_maintenance_completed != forecast.post_migration_maintenance
    {
        attestation_errors.push(format!(
            "committed effects differ from the reviewed forecast \
             (forecast={forecast:?}, effects={effects:?})"
        ));
    }

    let logical_after = match logical_witness(&migration.db_path) {
        Ok(witness) => Some(witness),
        Err(error) => {
            attestation_errors.push(format!(
                "could not capture the committed logical witness: {error}"
            ));
            None
        }
    };
    let raw_after = match raw_family_witness(&migration.db_path) {
        Ok(witness) => Some(witness),
        Err(error) => {
            attestation_errors.push(format!(
                "could not capture the committed raw witness: {error}"
            ));
            None
        }
    };
    if let Some(logical_after) = logical_after.as_ref()
        && (logical_after.user_version != forecast.to_version
            || !integrity_check_is_clean(&logical_after.integrity_check))
    {
        attestation_errors.push(format!(
            "committed logical witness did not attest target version {} and integrity=ok",
            forecast.to_version
        ));
    }
    match current_runtime_shape_is_canonical(&migration.db_path) {
        Ok(true) => {}
        Ok(false) => {
            attestation_errors.push(
                "committed database does not have the canonical current runtime shape".to_string(),
            );
        }
        Err(error) => {
            attestation_errors.push(format!(
                "could not attest the committed canonical runtime shape: {error}"
            ));
        }
    }
    let attested = attestation_errors.is_empty();
    let applied = AppliedMigrationReceipt {
        schema_version: APPLIED_SCHEMA.to_string(),
        run_id: run_id.clone(),
        database_path: plan.database_path,
        plan_token: recomputed_token.to_string(),
        prepared_receipt_sha256,
        marked_at,
        forecast,
        effects,
        raw_before: plan.raw_witness,
        logical_before: plan.logical_witness,
        raw_after,
        logical_after,
        attested,
        attestation_errors,
        undo_command: format!("br doctor migrate-schema undo {run_id}"),
    };
    write_json_new(&run_dir.join("applied.json"), &applied)?;
    sync_directory(&run_dir)?;

    if !applied.attested {
        return Err(BeadsError::internal(format!(
            "schema migration run {run_id} committed but failed post-commit attestation: {}; \
             an undo-capable applied receipt was persisted at {}; run `{}` before further \
             tracker writes",
            applied.attestation_errors.join("; "),
            run_dir.join("applied.json").display(),
            applied.undo_command
        )));
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&applied).map_err(BeadsError::Json)?
        );
    } else {
        println!(
            "Applied reviewed schema migration {} -> {} (run {})",
            applied.forecast.from_version, applied.forecast.to_version, applied.run_id
        );
        println!("Undo: {}", applied.undo_command);
        println!("Recovery bundle: {}", before_dir.display());
    }
    Ok(())
}

fn apply_reviewed_migration(
    db_path: &Path,
    from: u32,
    to: u32,
    marked_at: &str,
    run_dir: &Path,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
) -> Result<ReviewedSchemaMigrationEffectsReceipt> {
    run_post_migration_maintenance(db_path, from, to, marked_at, run_dir, write_authority)
}

fn run_post_migration_maintenance(
    db_path: &Path,
    from: u32,
    to: u32,
    marked_at: &str,
    run_dir: &Path,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
) -> Result<ReviewedSchemaMigrationEffectsReceipt> {
    write_authority.verify_database_authority()?;
    let source_logical = logical_witness(db_path)?;
    let source_permissions_witness = raw_family_witness(db_path)?;
    let source_unix_mode = component_for_suffix(&source_permissions_witness, "")?.unix_mode;
    let candidate_path = maintenance_candidate_path(db_path, run_dir)?;
    require_absent_family(&candidate_path)?;

    // Build and migrate a replacement database without mutating the live
    // family.  The live main file and sidecars remain the rollback authority
    // until the fully attested candidate is atomically installed below.
    let source_conn = Connection::open(db_path.to_string_lossy().into_owned())?;
    let escaped_path = candidate_path.to_string_lossy().replace('\'', "''");
    let candidate_result = source_conn
        .execute(&format!("VACUUM INTO '{escaped_path}'"))
        .map(|_| ())
        .map_err(BeadsError::Database);
    let close_result = close_connection(source_conn);
    match (candidate_result, close_result) {
        (Err(error), _) => return Err(error),
        (Ok(()), Err(error)) => return Err(error),
        (Ok(()), Ok(())) => {}
    }

    let mut effects = if from == to {
        ReviewedSchemaMigrationEffectsReceipt {
            from_version: from,
            to_version: to,
            content_hash_rows_rebuilt: 0,
            gate_result_history_created: false,
            post_migration_maintenance_completed: false,
        }
    } else {
        let conn = Connection::open(candidate_path.to_string_lossy().into_owned())?;
        conn.execute("PRAGMA foreign_keys = ON")?;
        conn.execute("BEGIN IMMEDIATE")?;
        let result = run_reviewed_schema_migration_steps_in_transaction(&conn, from, to, marked_at);
        let result = match result {
            Ok(effects) => match conn.execute("COMMIT") {
                Ok(_) => Ok(effects),
                Err(error) => {
                    let _ = conn.execute("ROLLBACK");
                    Err(BeadsError::Database(error))
                }
            },
            Err(error) => {
                let _ = conn.execute("ROLLBACK");
                Err(error)
            }
        };
        close_connection(conn)?;
        ReviewedSchemaMigrationEffectsReceipt::from(result?)
    };

    let candidate_conn = Connection::open(candidate_path.to_string_lossy().into_owned())?;
    let maintenance_result = (|| {
        candidate_conn.execute("REINDEX")?;
        candidate_conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    })();
    let close_result = close_connection(candidate_conn);
    match (maintenance_result, close_result) {
        (Err(error), _) => return Err(error),
        (Ok(()), Err(error)) => return Err(error),
        (Ok(()), Ok(())) => {}
    }

    set_file_permissions(&candidate_path, source_unix_mode)?;
    File::open(&candidate_path)
        .and_then(|file| file.sync_all())
        .map_err(BeadsError::Io)?;
    if let Some(parent) = candidate_path.parent() {
        sync_directory(parent)?;
    }

    let candidate_logical = logical_witness(&candidate_path)?;
    let candidate_matches_reviewed_operation = if from == to {
        logical_witnesses_match_except_integrity(&source_logical, &candidate_logical)
    } else {
        candidate_logical.user_version == to && current_runtime_shape_is_canonical(&candidate_path)?
    };
    if !integrity_check_is_clean(&candidate_logical.integrity_check)
        || !candidate_matches_reviewed_operation
    {
        return Err(BeadsError::internal(format!(
            "copy-on-write migration candidate did not attest the reviewed operation \
             (from={from}, to={to}, source integrity={:?}, candidate integrity={:?}, source \
             contents={}, candidate contents={}); the candidate is retained at {}",
            source_logical.integrity_check,
            candidate_logical.integrity_check,
            source_logical.contents_sha256,
            candidate_logical.contents_sha256,
            candidate_path.display()
        )));
    }

    let source_logical_after = logical_witness(db_path)?;
    if source_logical_after != source_logical {
        return Err(BeadsError::internal(format!(
            "live database changed while the copy-on-write migration candidate was prepared; \
             refusing installation and retaining the candidate at {}",
            candidate_path.display()
        )));
    }
    let source_raw = raw_family_witness(db_path)?;
    let candidate_raw = raw_family_witness(&candidate_path)?;
    let candidate_sidecars_dir = run_dir.join("maintenance-candidate-sidecars");
    move_present_sidecars_new(&candidate_path, &candidate_sidecars_dir, &candidate_raw)?;

    let replacement_lock = write_authority.lock_database_replacement_candidate(&candidate_path)?;
    let displaced_dir = run_dir.join("maintenance-displaced");
    ensure_new_directory(&displaced_dir)?;
    move_present_sidecars_new(db_path, &displaced_dir, &source_raw)?;
    let displaced_main = backup_component_path(&displaced_dir, db_path, "")?;
    if let Err(error) = install_compacted_candidate(
        &candidate_path,
        db_path,
        &displaced_main,
        replacement_lock,
        write_authority,
    ) {
        let rollback = restore_present_sidecars(db_path, &displaced_dir, &source_raw);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(BeadsError::internal(format!(
                "compacted database installation failed ({error}); restoring retained sidecars \
                 also failed ({rollback_error})"
            ))),
        };
    }
    verify_backup_family(db_path, &displaced_dir, &source_raw)?;

    let installed_logical = logical_witness(db_path);
    let installed_is_exact = installed_logical
        .as_ref()
        .is_ok_and(|witness| witness == &candidate_logical);
    if !installed_is_exact {
        let failed_dir = run_dir.join("maintenance-failed-new-family");
        ensure_new_directory(&failed_dir)?;
        if let Ok(installed_raw) = raw_family_witness(db_path) {
            move_present_sidecars_new(db_path, &failed_dir, &installed_raw)?;
        }
        rollback_compacted_install(db_path, &displaced_main, &failed_dir, write_authority)?;
        restore_present_sidecars(db_path, &displaced_dir, &source_raw)?;
        return Err(BeadsError::internal(format!(
            "installed compacted database failed its fresh logical attestation ({:?}); the \
             original database family was restored and the rejected compacted family is retained \
             at {}",
            installed_logical
                .as_ref()
                .map(|witness| witness.integrity_check.as_str())
                .unwrap_or("logical witness unavailable"),
            failed_dir.display()
        )));
    }

    write_authority.finalize_database_replacement()?;
    write_authority.verify_database_authority()?;
    sync_directory(run_dir)?;
    effects.post_migration_maintenance_completed = true;
    Ok(effects)
}

fn logical_witnesses_match_except_integrity(
    left: &LogicalDatabaseWitness,
    right: &LogicalDatabaseWitness,
) -> bool {
    left.user_version == right.user_version
        && left.schema_sha256 == right.schema_sha256
        && left.contents_sha256 == right.contents_sha256
        && left.tables == right.tables
}

fn maintenance_candidate_path(db_path: &Path, run_dir: &Path) -> Result<PathBuf> {
    let run_id = run_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| BeadsError::internal("schema migration run directory has no UTF-8 name"))?;
    validate_run_id(run_id)?;
    let database_name = db_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| BeadsError::internal("database path has no UTF-8 file name"))?;
    Ok(db_path.with_file_name(format!(".{database_name}.schema-migration-{run_id}.vacuum")))
}

fn require_absent_family(base_path: &Path) -> Result<()> {
    for suffix in FAMILY_SUFFIXES {
        let path = family_component_path(base_path, suffix);
        if secure_file_metadata(&path)?.is_some() {
            return Err(BeadsError::internal(format!(
                "refusing to overwrite retained schema-migration candidate {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn move_present_sidecars_new(
    source_base: &Path,
    destination_dir: &Path,
    expected: &RawFamilyWitness,
) -> Result<()> {
    let present_sidecars = expected
        .components
        .iter()
        .filter(|component| component.present && !component.suffix.is_empty())
        .collect::<Vec<_>>();
    if present_sidecars.is_empty() {
        return Ok(());
    }
    ensure_directory(destination_dir)?;
    let mut moved = Vec::with_capacity(present_sidecars.len());
    for component in present_sidecars {
        let source = family_component_path(source_base, &component.suffix);
        let destination = backup_component_path(destination_dir, source_base, &component.suffix)?;
        if secure_file_metadata(&destination)?.is_some() {
            let rollback = restore_moved_components(source_base, destination_dir, &moved);
            return match rollback {
                Ok(()) => Err(BeadsError::internal(format!(
                    "refusing to overwrite retained schema-migration sidecar {}",
                    destination.display()
                ))),
                Err(rollback_error) => Err(BeadsError::internal(format!(
                    "refusing to overwrite retained schema-migration sidecar {}; rollback of \
                     prior sidecar moves also failed: {rollback_error}",
                    destination.display()
                ))),
            };
        }
        let metadata = secure_file_metadata(&source)?.ok_or_else(|| {
            BeadsError::internal(format!(
                "schema-migration sidecar disappeared before retention: {}",
                source.display()
            ))
        })?;
        verify_component_bytes(&source, &metadata, component)?;
        if let Err(error) = fs::rename(&source, &destination) {
            let rollback = restore_moved_components(source_base, destination_dir, &moved);
            return match rollback {
                Ok(()) => Err(BeadsError::Io(error)),
                Err(rollback_error) => Err(BeadsError::internal(format!(
                    "could not retain schema-migration sidecar {} ({error}); rollback of prior \
                     sidecar moves also failed: {rollback_error}",
                    source.display()
                ))),
            };
        }
        moved.push(component.suffix.clone());
    }
    if let Some(parent) = source_base.parent() {
        sync_directory(parent)?;
    }
    sync_directory(destination_dir)
}

fn restore_moved_components(
    destination_base: &Path,
    source_dir: &Path,
    suffixes: &[String],
) -> Result<()> {
    for suffix in suffixes.iter().rev() {
        let source = backup_component_path(source_dir, destination_base, suffix)?;
        let destination = family_component_path(destination_base, suffix);
        if secure_file_metadata(&destination)?.is_some() {
            return Err(BeadsError::internal(format!(
                "cannot restore retained schema-migration component because its live path exists: \
                 {}",
                destination.display()
            )));
        }
        fs::rename(&source, &destination).map_err(BeadsError::Io)?;
    }
    if let Some(parent) = destination_base.parent() {
        sync_directory(parent)?;
    }
    sync_directory(source_dir)
}

fn restore_present_sidecars(
    destination_base: &Path,
    source_dir: &Path,
    expected: &RawFamilyWitness,
) -> Result<()> {
    let suffixes = expected
        .components
        .iter()
        .filter(|component| component.present && !component.suffix.is_empty())
        .map(|component| component.suffix.clone())
        .collect::<Vec<_>>();
    restore_moved_components(destination_base, source_dir, &suffixes)
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn exchange_database_paths(left: &Path, right: &Path) -> Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};

    renameat_with(CWD, left, CWD, right, RenameFlags::EXCHANGE)
        .map_err(|error| BeadsError::Io(std::io::Error::from(error)))
}

fn install_compacted_candidate(
    candidate_path: &Path,
    db_path: &Path,
    displaced_main: &Path,
    replacement_lock: File,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
) -> Result<()> {
    if secure_file_metadata(displaced_main)?.is_some() {
        return Err(BeadsError::internal(format!(
            "refusing to overwrite retained pre-compaction database {}",
            displaced_main.display()
        )));
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        exchange_database_paths(candidate_path, db_path)?;
        if let Err(error) = write_authority.adopt_locked_database_replacement(replacement_lock) {
            let rollback = exchange_database_paths(candidate_path, db_path);
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(BeadsError::internal(format!(
                    "database replacement authority adoption failed ({error}); atomic exchange \
                     rollback also failed ({rollback_error})"
                ))),
            };
        }
        if let Err(error) = fs::rename(candidate_path, displaced_main) {
            let exchange_rollback = exchange_database_paths(db_path, candidate_path);
            let authority_rollback =
                write_authority.restore_retained_database_inode_after_authorized_replace();
            return match (exchange_rollback, authority_rollback) {
                (Ok(()), Ok(())) => Err(BeadsError::Io(error)),
                (exchange_result, authority_result) => Err(BeadsError::internal(format!(
                    "could not retain exchanged pre-compaction database ({error}); exchange \
                     rollback={exchange_result:?}, authority rollback={authority_result:?}"
                ))),
            };
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        fs::rename(db_path, displaced_main).map_err(BeadsError::Io)?;
        if let Err(error) = fs::rename(candidate_path, db_path) {
            let rollback = fs::rename(displaced_main, db_path).map_err(BeadsError::Io);
            return match rollback {
                Ok(()) => Err(BeadsError::Io(error)),
                Err(rollback_error) => Err(BeadsError::internal(format!(
                    "could not install compacted database ({error}); restoring the retained \
                     original also failed ({rollback_error})"
                ))),
            };
        }
        if let Err(error) = write_authority.adopt_locked_database_replacement(replacement_lock) {
            let candidate_restore = fs::rename(db_path, candidate_path).map_err(BeadsError::Io);
            let original_restore = fs::rename(displaced_main, db_path).map_err(BeadsError::Io);
            return match (candidate_restore, original_restore) {
                (Ok(()), Ok(())) => Err(error),
                (candidate_result, original_result) => Err(BeadsError::internal(format!(
                    "database replacement authority adoption failed ({error}); candidate \
                     rollback={candidate_result:?}, original rollback={original_result:?}"
                ))),
            };
        }
    }

    if let Some(parent) = db_path.parent() {
        sync_directory(parent)?;
    }
    if let Some(parent) = displaced_main.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn rollback_compacted_install(
    db_path: &Path,
    displaced_main: &Path,
    failed_dir: &Path,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
) -> Result<()> {
    ensure_directory(failed_dir)?;
    let failed_main = backup_component_path(failed_dir, db_path, "")?;
    if secure_file_metadata(&failed_main)?.is_some() {
        return Err(BeadsError::internal(format!(
            "refusing to overwrite retained failed compacted database {}",
            failed_main.display()
        )));
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        exchange_database_paths(db_path, displaced_main)?;
        write_authority.restore_retained_database_inode_after_authorized_replace()?;
        fs::rename(displaced_main, &failed_main).map_err(BeadsError::Io)?;
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        fs::rename(db_path, &failed_main).map_err(BeadsError::Io)?;
        fs::rename(displaced_main, db_path).map_err(BeadsError::Io)?;
        write_authority.restore_retained_database_inode_after_authorized_replace()?;
    }

    if let Some(parent) = db_path.parent() {
        sync_directory(parent)?;
    }
    sync_directory(failed_dir)
}

fn execute_undo(args: &DoctorMigrateSchemaUndoArgs, migration: &MigrationContext) -> Result<()> {
    validate_run_id(&args.run_id)?;
    let run_dir = migration_runs_root(&migration.beads_dir).join(&args.run_id);
    if run_dir.join("undone.json").exists() {
        let receipt: UndoReceipt = read_json(&run_dir.join("undone.json"))?;
        validate_completed_undo_receipt(&receipt, args, migration)?;
        return emit_undo(&receipt, args.json);
    }
    let applied_path = run_dir.join("applied.json");
    let applied_receipt_sha256 = file_sha256(&applied_path)?;
    let applied: AppliedMigrationReceipt = read_json(&applied_path)?;
    validate_applied_receipt(&applied, args, migration, &run_dir)?;

    let before_dir = run_dir.join("before");
    verify_backup_family(&migration.db_path, &before_dir, &applied.raw_before)?;

    let undo_prepared_path = run_dir.join("undo-prepared.json");
    let mut receipt = if undo_prepared_path.exists() {
        if args.dry_run {
            return Err(BeadsError::internal(format!(
                "schema migration undo for run {} is already prepared; rerun without \
                 --dry-run to resume it",
                args.run_id
            )));
        }
        let receipt: UndoReceipt = read_json(&undo_prepared_path)?;
        validate_prepared_undo_receipt(
            &receipt,
            args,
            migration,
            &run_dir,
            &applied_receipt_sha256,
            &applied,
        )?;
        receipt
    } else {
        let raw_live = raw_family_witness(&migration.db_path)?;
        let logical_live = logical_witness(&migration.db_path).ok();
        require_unchanged_applied_state(&applied, &raw_live, logical_live.as_ref(), &args.run_id)?;

        let quarantine_id = format!(
            "undo-{}-{}",
            Utc::now().format("%Y%m%dT%H%M%S%.6fZ"),
            std::process::id()
        );
        let quarantine_dir = run_dir.join("undo-quarantine").join(quarantine_id);
        let receipt = UndoReceipt {
            schema_version: UNDO_SCHEMA.to_string(),
            run_id: args.run_id.clone(),
            dry_run: args.dry_run,
            database_path: migration.db_path.display().to_string(),
            quarantine_path: quarantine_dir.display().to_string(),
            applied_receipt_sha256: applied_receipt_sha256.clone(),
            raw_expected_before: applied.raw_before.clone(),
            logical_expected_before: applied.logical_before.clone(),
            raw_live_before_undo: raw_live,
            logical_live_before_undo: logical_live,
            raw_restored: None,
            logical_restored: None,
        };
        if args.dry_run {
            emit_undo(&receipt, args.json)?;
            return Ok(());
        }

        let quarantine_parent = quarantine_dir.parent().ok_or_else(|| {
            BeadsError::internal("schema migration undo quarantine path has no parent")
        })?;
        ensure_directory(quarantine_parent)?;
        set_private_directory_permissions(quarantine_parent)?;
        write_json_new(&undo_prepared_path, &receipt)?;
        receipt
    };

    let quarantine_dir = validate_quarantine_path(&receipt, &run_dir)?;
    if quarantine_dir.exists() {
        ensure_directory(&quarantine_dir)?;
    } else {
        ensure_new_directory(&quarantine_dir)?;
    }
    quarantine_live_family_resuming(
        &migration.db_path,
        &quarantine_dir,
        &receipt.raw_live_before_undo,
        &applied.raw_before,
    )?;
    restore_backup_family_resuming(&migration.db_path, &before_dir, &applied.raw_before)?;

    let logical_restored = logical_witness(&migration.db_path)?;
    let raw_restored = raw_family_witness(&migration.db_path)?;
    if logical_restored != applied.logical_before
        || !stable_raw_eq(&raw_restored, &applied.raw_before)
    {
        return Err(BeadsError::internal(format!(
            "schema migration undo for run {} did not reproduce the verified pre-state; \
             the displaced applied state remains quarantined at {}",
            args.run_id,
            quarantine_dir.display()
        )));
    }
    receipt.raw_restored = Some(raw_restored);
    receipt.logical_restored = Some(logical_restored);
    receipt.dry_run = false;
    write_json_new(&run_dir.join("undone.json"), &receipt)?;
    sync_directory(&run_dir)?;
    emit_undo(&receipt, args.json)
}

fn validate_applied_receipt(
    applied: &AppliedMigrationReceipt,
    args: &DoctorMigrateSchemaUndoArgs,
    migration: &MigrationContext,
    run_dir: &Path,
) -> Result<()> {
    validate_raw_family_witness(&applied.raw_before)?;
    if let Some(raw_after) = applied.raw_after.as_ref() {
        validate_raw_family_witness(raw_after)?;
    }
    if applied.schema_version != APPLIED_SCHEMA {
        return Err(BeadsError::internal(format!(
            "unsupported applied schema-migration receipt contract {:?}",
            applied.schema_version
        )));
    }
    if applied.run_id != args.run_id {
        return Err(BeadsError::internal(format!(
            "applied receipt run-id mismatch (path={}, receipt={})",
            args.run_id, applied.run_id
        )));
    }
    if applied.database_path != migration.db_path.display().to_string() {
        return Err(BeadsError::internal(format!(
            "schema migration run {} belongs to {}, not {}",
            args.run_id,
            applied.database_path,
            migration.db_path.display()
        )));
    }
    if applied.attested != applied.attestation_errors.is_empty() {
        return Err(BeadsError::internal(format!(
            "schema migration run {} has an internally inconsistent attestation receipt",
            args.run_id
        )));
    }
    if applied.raw_after.is_none() && applied.logical_after.is_none() {
        return Err(BeadsError::internal(format!(
            "schema migration run {} has no committed-state witness and cannot be safely undone",
            args.run_id
        )));
    }

    let recomputed_token = compute_plan_token(
        &applied.database_path,
        &applied.logical_before,
        &applied.forecast,
    )?;
    if !constant_time_text_eq(&recomputed_token, &applied.plan_token) {
        return Err(BeadsError::internal(format!(
            "schema migration run {} has a plan token that does not bind its recorded pre-state",
            args.run_id
        )));
    }

    let prepared_path = run_dir.join("prepared.json");
    let prepared_sha256 = file_sha256(&prepared_path)?;
    if !constant_time_text_eq(&prepared_sha256, &applied.prepared_receipt_sha256) {
        return Err(BeadsError::internal(format!(
            "schema migration run {} failed its prepared-to-applied receipt hash chain",
            args.run_id
        )));
    }
    let prepared: PreparedMigrationReceipt = read_json(&prepared_path)?;
    if prepared.schema_version != PREPARED_SCHEMA
        || prepared.run_id != applied.run_id
        || prepared.database_path != applied.database_path
        || !constant_time_text_eq(&prepared.plan_token, &applied.plan_token)
        || prepared.marked_at != applied.marked_at
        || prepared.forecast != applied.forecast
        || prepared.raw_before != applied.raw_before
        || prepared.logical_before != applied.logical_before
    {
        return Err(BeadsError::internal(format!(
            "schema migration run {} has inconsistent prepared and applied receipts",
            args.run_id
        )));
    }
    Ok(())
}

fn validate_completed_undo_receipt(
    receipt: &UndoReceipt,
    args: &DoctorMigrateSchemaUndoArgs,
    migration: &MigrationContext,
) -> Result<()> {
    validate_raw_family_witness(&receipt.raw_expected_before)?;
    validate_raw_family_witness(&receipt.raw_live_before_undo)?;
    let run_dir = migration_runs_root(&migration.beads_dir).join(&args.run_id);
    if receipt.schema_version != UNDO_SCHEMA
        || receipt.run_id != args.run_id
        || receipt.database_path != migration.db_path.display().to_string()
        || receipt.dry_run
    {
        return Err(BeadsError::internal(format!(
            "schema migration run {} has an invalid completed undo receipt",
            args.run_id
        )));
    }
    let applied_sha256 = file_sha256(&run_dir.join("applied.json"))?;
    if !constant_time_text_eq(&applied_sha256, &receipt.applied_receipt_sha256) {
        return Err(BeadsError::internal(format!(
            "schema migration run {} failed its applied-to-undo receipt hash chain",
            args.run_id
        )));
    }
    let _ = validate_quarantine_path(receipt, &run_dir)?;
    let logical_restored = receipt.logical_restored.as_ref().ok_or_else(|| {
        BeadsError::internal(format!(
            "schema migration run {} completed undo receipt omits its logical witness",
            args.run_id
        ))
    })?;
    let raw_restored = receipt.raw_restored.as_ref().ok_or_else(|| {
        BeadsError::internal(format!(
            "schema migration run {} completed undo receipt omits its raw witness",
            args.run_id
        ))
    })?;
    validate_raw_family_witness(raw_restored)?;
    if logical_witness(&migration.db_path)? != *logical_restored {
        return Err(BeadsError::internal(format!(
            "schema migration run {} was previously undone, but the live database has since changed",
            args.run_id
        )));
    }
    Ok(())
}

fn validate_prepared_undo_receipt(
    receipt: &UndoReceipt,
    args: &DoctorMigrateSchemaUndoArgs,
    migration: &MigrationContext,
    run_dir: &Path,
    applied_receipt_sha256: &str,
    applied: &AppliedMigrationReceipt,
) -> Result<()> {
    validate_raw_family_witness(&receipt.raw_expected_before)?;
    validate_raw_family_witness(&receipt.raw_live_before_undo)?;
    if receipt.schema_version != UNDO_SCHEMA
        || receipt.run_id != args.run_id
        || receipt.dry_run
        || receipt.database_path != migration.db_path.display().to_string()
        || !constant_time_text_eq(&receipt.applied_receipt_sha256, applied_receipt_sha256)
        || receipt.raw_expected_before != applied.raw_before
        || receipt.logical_expected_before != applied.logical_before
        || receipt.raw_restored.is_some()
        || receipt.logical_restored.is_some()
    {
        return Err(BeadsError::internal(format!(
            "schema migration run {} has an invalid or inconsistent prepared undo receipt",
            args.run_id
        )));
    }
    if let Some(expected) = applied.logical_after.as_ref() {
        if receipt.logical_live_before_undo.as_ref() != Some(expected) {
            return Err(BeadsError::internal(format!(
                "schema migration run {} prepared undo does not bind the applied logical state",
                args.run_id
            )));
        }
    } else if let Some(expected) = applied.raw_after.as_ref()
        && !stable_raw_eq(&receipt.raw_live_before_undo, expected)
    {
        return Err(BeadsError::internal(format!(
            "schema migration run {} prepared undo does not bind the fallback applied raw state",
            args.run_id
        )));
    }
    let _ = validate_quarantine_path(receipt, run_dir)?;
    Ok(())
}

fn require_unchanged_applied_state(
    applied: &AppliedMigrationReceipt,
    raw_live: &RawFamilyWitness,
    logical_live: Option<&LogicalDatabaseWitness>,
    run_id: &str,
) -> Result<()> {
    let unchanged = if let Some(expected) = applied.logical_after.as_ref() {
        logical_live == Some(expected)
    } else {
        applied
            .raw_after
            .as_ref()
            .is_some_and(|expected| stable_raw_eq(raw_live, expected))
    };
    if !unchanged {
        return Err(BeadsError::internal(format!(
            "schema migration undo refused because the live database has changed since run \
             {run_id}; preserving both states is safer than overwriting newer tracker work"
        )));
    }
    Ok(())
}

fn validate_quarantine_path(receipt: &UndoReceipt, run_dir: &Path) -> Result<PathBuf> {
    let quarantine_dir = PathBuf::from(&receipt.quarantine_path);
    let expected_parent = run_dir.join("undo-quarantine");
    if quarantine_dir.parent() != Some(expected_parent.as_path()) {
        return Err(BeadsError::internal(format!(
            "schema migration undo quarantine path escapes its run directory: {}",
            quarantine_dir.display()
        )));
    }
    let quarantine_id = quarantine_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| BeadsError::internal("schema migration undo quarantine id is not UTF-8"))?;
    validate_run_id(quarantine_id)?;
    Ok(quarantine_dir)
}

fn emit_undo(receipt: &UndoReceipt, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(receipt).map_err(BeadsError::Json)?
        );
    } else if receipt.dry_run {
        println!(
            "Undo preconditions verified for schema migration run {}",
            receipt.run_id
        );
        println!(
            "Current state would be quarantined at {}",
            receipt.quarantine_path
        );
    } else {
        println!("Undid schema migration run {}", receipt.run_id);
        println!(
            "Displaced applied state retained at {}",
            receipt.quarantine_path
        );
    }
    Ok(())
}

fn current_schema_version() -> Result<u32> {
    u32::try_from(CURRENT_SCHEMA_VERSION).map_err(|_| {
        BeadsError::internal(format!(
            "current schema version {CURRENT_SCHEMA_VERSION} cannot be represented as u32"
        ))
    })
}

fn open_read_only(path: &Path) -> Result<Connection> {
    open_with_flags(
        path.to_string_lossy().as_ref(),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(BeadsError::Database)
}

fn close_connection(conn: Connection) -> Result<()> {
    conn.close().map_err(BeadsError::Database)
}

fn query_user_version(conn: &Connection) -> Result<u32> {
    let row = conn.query_row("PRAGMA user_version")?;
    row.get(0)
        .and_then(SqliteValue::as_integer)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| BeadsError::internal("PRAGMA user_version was not a nonnegative u32"))
}

fn named_table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let escaped = table.replace('\'', "''");
    let rows = conn.query(&format!(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='{escaped}' LIMIT 1"
    ))?;
    Ok(!rows.is_empty())
}

fn require_source_tables(conn: &Connection, from: u32) -> Result<()> {
    for table in ["issues", "dirty_issues", "export_hashes"] {
        if !named_table_exists(conn, table)? {
            return Err(BeadsError::internal(format!(
                "reviewed {from}->{} migration requires table {table}, but it is absent",
                CURRENT_SCHEMA_VERSION
            )));
        }
    }
    Ok(())
}

fn query_count(conn: &Connection, sql: &str) -> Result<u64> {
    let row = conn.query_row(sql)?;
    row.get(0)
        .and_then(SqliteValue::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| {
            BeadsError::internal(format!("count query returned an invalid value: {sql}"))
        })
}

fn current_runtime_shape_is_canonical(db_path: &Path) -> Result<bool> {
    let conn = open_read_only(db_path)?;
    let canonical =
        query_user_version(&conn)? == current_schema_version()? && runtime_schema_compatible(&conn);
    close_connection(conn)?;
    Ok(canonical)
}

fn logical_witness(db_path: &Path) -> Result<LogicalDatabaseWitness> {
    let conn = open_read_only(db_path)?;
    // FrankenSQLite's integrity walker intentionally switches to the
    // transaction's live freelist projection while an explicit transaction is
    // active.  A read-only DEFERRED transaction can therefore report a false
    // orphan for a healthy file whose committed freelist is non-empty.  Keep
    // the schema/content reads in one stable snapshot, but attest integrity on
    // both sides of that snapshot from autocommit state.  The caller already
    // holds the database-family write authority for the whole operation.
    let integrity_before = match integrity_check_messages(&conn) {
        Ok(messages) => messages.join("\n"),
        Err(error) => {
            let _ = close_connection(conn);
            return Err(error);
        }
    };
    if let Err(error) = conn.execute("BEGIN DEFERRED TRANSACTION") {
        let _ = close_connection(conn);
        return Err(BeadsError::Database(error));
    }
    let result = logical_witness_from_connection(&conn, integrity_before.clone());
    let transaction_result = conn
        .execute("ROLLBACK")
        .map(|_| ())
        .map_err(BeadsError::Database);
    let integrity_after = if transaction_result.is_ok() {
        integrity_check_messages(&conn).map(|messages| messages.join("\n"))
    } else {
        Ok(String::new())
    };
    let close_result = close_connection(conn);
    let mut witness = result?;
    transaction_result?;
    let integrity_after = integrity_after?;
    close_result?;
    if integrity_before != integrity_after {
        return Err(BeadsError::internal(format!(
            "database integrity changed while capturing the migration witness \
             (before={integrity_before:?}, after={integrity_after:?})"
        )));
    }
    witness.integrity_check = integrity_after;
    Ok(witness)
}

fn logical_witness_from_connection(
    conn: &Connection,
    integrity_check: String,
) -> Result<LogicalDatabaseWitness> {
    let user_version = query_user_version(conn)?;

    let schema_rows = conn.query(
        "SELECT type, name, tbl_name, COALESCE(sql, '') \
         FROM sqlite_master \
         ORDER BY type, name, tbl_name, COALESCE(sql, '')",
    )?;
    let mut schema_hasher = Sha256::new();
    for row in &schema_rows {
        for index in 0..4 {
            hash_sqlite_value(
                &mut schema_hasher,
                row.get(index).unwrap_or(&SqliteValue::Null),
            );
        }
    }
    let schema_sha256 = hex_digest(schema_hasher.finalize().as_slice());

    let table_rows = conn.query(
        "SELECT name FROM sqlite_master \
         WHERE type='table' \
         ORDER BY name",
    )?;
    let mut tables = Vec::with_capacity(table_rows.len());
    let mut contents_hasher = Sha256::new();
    for table_row in table_rows {
        let name = table_row
            .get(0)
            .and_then(SqliteValue::as_text)
            .ok_or_else(|| BeadsError::internal("sqlite_master table name was not text"))?
            .to_string();
        let quoted = quote_identifier(&name);
        let columns = conn.query(&format!("PRAGMA table_info({quoted})"))?.len();
        let rows = conn.query(&format!("SELECT * FROM {quoted}"))?;
        let mut encoded_rows = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut encoded = Vec::new();
            for index in 0..columns {
                encode_sqlite_value(&mut encoded, row.get(index).unwrap_or(&SqliteValue::Null));
            }
            encoded_rows.push(encoded);
        }
        encoded_rows.sort();
        let mut table_hasher = Sha256::new();
        hash_len_prefixed(&mut table_hasher, name.as_bytes());
        for encoded in &encoded_rows {
            hash_len_prefixed(&mut table_hasher, encoded);
            hash_len_prefixed(&mut contents_hasher, name.as_bytes());
            hash_len_prefixed(&mut contents_hasher, encoded);
        }
        tables.push(LogicalTableWitness {
            name,
            row_count: u64::try_from(encoded_rows.len()).map_err(|_| {
                BeadsError::internal("logical witness row count does not fit in u64")
            })?,
            rows_sha256: hex_digest(table_hasher.finalize().as_slice()),
        });
    }

    Ok(LogicalDatabaseWitness {
        user_version,
        integrity_check,
        schema_sha256,
        contents_sha256: hex_digest(contents_hasher.finalize().as_slice()),
        tables,
    })
}

fn integrity_check_messages(conn: &Connection) -> Result<Vec<String>> {
    let rows = conn.query("PRAGMA integrity_check")?;
    let mut messages = Vec::new();
    for row in rows {
        for value in row.values() {
            if let Some(text) = value.as_text() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    messages.push(trimmed.to_string());
                }
            }
        }
    }
    if messages.is_empty() {
        messages.push("integrity_check returned no diagnostic rows".to_string());
    }
    Ok(messages)
}

fn integrity_check_is_clean(integrity_check: &str) -> bool {
    integrity_check.trim().eq_ignore_ascii_case("ok")
}

fn integrity_check_is_repairable(integrity_check: &str) -> bool {
    let mut saw_repairable = false;
    for message in integrity_check
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let lower = message.to_ascii_lowercase();
        if lower.contains("never used")
            || lower.contains("missing from index")
            || lower.contains("out of order")
        {
            saw_repairable = true;
            continue;
        }
        if lower.contains("*** in database") {
            continue;
        }
        return false;
    }
    saw_repairable
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn hash_sqlite_value(hasher: &mut Sha256, value: &SqliteValue) {
    let mut encoded = Vec::new();
    encode_sqlite_value(&mut encoded, value);
    hash_len_prefixed(hasher, &encoded);
}

fn encode_sqlite_value(output: &mut Vec<u8>, value: &SqliteValue) {
    match value {
        SqliteValue::Null => output.push(0),
        SqliteValue::Integer(value) => {
            output.push(1);
            output.extend_from_slice(&value.to_le_bytes());
        }
        SqliteValue::Float(value) => {
            output.push(2);
            output.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        SqliteValue::Text(value) => {
            output.push(3);
            append_len_prefixed(output, value.as_bytes());
        }
        SqliteValue::Blob(value) => {
            output.push(4);
            append_len_prefixed(output, value.as_ref());
        }
    }
}

fn append_len_prefixed(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
    output.extend_from_slice(bytes);
}

fn hash_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(bytes);
}

fn raw_family_witness(db_path: &Path) -> Result<RawFamilyWitness> {
    let mut components = Vec::with_capacity(FAMILY_SUFFIXES.len());
    for suffix in FAMILY_SUFFIXES {
        let path = family_component_path(db_path, suffix);
        match secure_file_metadata(&path)? {
            Some(metadata) => {
                let (length, sha256) = hash_regular_file(&path, &metadata)?;
                components.push(RawComponentWitness {
                    suffix: (*suffix).to_string(),
                    present: true,
                    length: Some(length),
                    sha256: Some(sha256),
                    unix_mode: unix_file_mode(&metadata),
                });
            }
            None => components.push(RawComponentWitness {
                suffix: (*suffix).to_string(),
                present: false,
                length: None,
                sha256: None,
                unix_mode: None,
            }),
        }
    }
    Ok(RawFamilyWitness { components })
}

fn family_component_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(db_path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

fn backup_component_path(before_dir: &Path, db_path: &Path, suffix: &str) -> Result<PathBuf> {
    let file_name = db_path.file_name().ok_or_else(|| {
        BeadsError::internal(format!(
            "database path has no file name: {}",
            db_path.display()
        ))
    })?;
    let mut backup_name = OsString::from(file_name);
    backup_name.push(suffix);
    Ok(before_dir.join(backup_name))
}

fn secure_file_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(BeadsError::internal(format!(
                    "refusing schema migration file-family symlink {}",
                    path.display()
                )));
            }
            if !metadata.is_file() {
                return Err(BeadsError::internal(format!(
                    "schema migration family member is not a regular file: {}",
                    path.display()
                )));
            }
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(BeadsError::Io(error)),
    }
}

fn refuse_non_regular_component(db_path: &Path) -> Result<()> {
    if secure_file_metadata(db_path)?.is_none() {
        return Err(BeadsError::DatabaseNotFound {
            path: db_path.to_path_buf(),
        });
    }
    for suffix in &FAMILY_SUFFIXES[1..] {
        let _ = secure_file_metadata(&family_component_path(db_path, suffix))?;
    }
    Ok(())
}

fn hash_regular_file(path: &Path, expected: &fs::Metadata) -> Result<(u64, String)> {
    let mut file = File::open(path).map_err(BeadsError::Io)?;
    let opened = file.metadata().map_err(BeadsError::Io)?;
    if !same_file_identity(expected, &opened) {
        return Err(BeadsError::internal(format!(
            "schema migration file changed identity while opening {}",
            path.display()
        )));
    }
    let mut hasher = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(BeadsError::Io)?;
        if read == 0 {
            break;
        }
        length = length
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or_else(|| BeadsError::internal("schema migration file length overflow"))?;
        hasher.update(&buffer[..read]);
    }
    Ok((length, hex_digest(hasher.finalize().as_slice())))
}

#[cfg(unix)]
fn same_file_identity(expected: &fs::Metadata, opened: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    expected.dev() == opened.dev()
        && expected.ino() == opened.ino()
        && expected.len() == opened.len()
}

#[cfg(not(unix))]
fn same_file_identity(expected: &fs::Metadata, opened: &fs::Metadata) -> bool {
    expected.len() == opened.len()
}

#[cfg(unix)]
fn unix_file_mode(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode())
}

#[cfg(not(unix))]
fn unix_file_mode(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn compute_plan_token(
    database_path: &str,
    logical: &LogicalDatabaseWitness,
    forecast: &MigrationForecast,
) -> Result<String> {
    let material = PlanTokenMaterial {
        contract: PLAN_SCHEMA,
        database_path,
        from_version: forecast.from_version,
        to_version: forecast.to_version,
        logical_witness: logical,
        forecast,
    };
    let encoded = serde_json::to_vec(&material).map_err(BeadsError::Json)?;
    Ok(hex_digest(Sha256::digest(encoded).as_slice()))
}

fn constant_time_text_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

fn migration_runs_root(beads_dir: &Path) -> PathBuf {
    beads_dir.join(".br_recovery").join("schema-migrations")
}

fn allocate_run_id(beads_dir: &Path) -> Result<String> {
    let recovery_root = beads_dir.join(".br_recovery");
    ensure_directory(&recovery_root)?;
    set_private_directory_permissions(&recovery_root)?;
    let root = migration_runs_root(beads_dir);
    ensure_directory(&root)?;
    set_private_directory_permissions(&root)?;
    for counter in 0_u32..1000 {
        let run_id = format!(
            "{}-{}-{counter}",
            Utc::now().format("%Y%m%dT%H%M%S%.6fZ"),
            std::process::id()
        );
        let candidate = root.join(&run_id);
        match fs::create_dir(&candidate) {
            Ok(()) => {
                set_private_directory_permissions(&candidate)?;
                sync_directory(&root)?;
                return Ok(run_id);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(BeadsError::Io(error)),
        }
    }
    Err(BeadsError::internal(
        "could not allocate a unique schema-migration run id",
    ))
}

fn ensure_directory(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path).map_err(BeadsError::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(BeadsError::internal(format!(
                "schema migration artifact path is not a real directory: {}",
                path.display()
            )));
        }
        return Ok(());
    }
    let parent = path.parent().ok_or_else(|| {
        BeadsError::internal(format!(
            "schema migration artifact directory has no parent: {}",
            path.display()
        ))
    })?;
    ensure_directory(parent)?;
    match fs::create_dir(path) {
        Ok(()) => {
            set_private_directory_permissions(path)?;
            sync_directory(parent)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).map_err(BeadsError::Io)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                Err(BeadsError::internal(format!(
                    "schema migration artifact path raced to a non-directory: {}",
                    path.display()
                )))
            } else {
                Ok(())
            }
        }
        Err(error) => Err(BeadsError::Io(error)),
    }
}

fn ensure_new_directory(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        BeadsError::internal(format!(
            "schema migration directory has no parent: {}",
            path.display()
        ))
    })?;
    ensure_directory(parent)?;
    fs::create_dir(path).map_err(BeadsError::Io)?;
    set_private_directory_permissions(path)?;
    sync_directory(parent)
}

fn copy_family_to_backup(
    db_path: &Path,
    before_dir: &Path,
    expected: &RawFamilyWitness,
) -> Result<()> {
    for component in &expected.components {
        if !component.present {
            continue;
        }
        let source = family_component_path(db_path, &component.suffix);
        let destination = backup_component_path(before_dir, db_path, &component.suffix)?;
        copy_regular_file_new(&source, &destination, None)?;
    }
    sync_directory(before_dir)
}

fn copy_regular_file_new(
    source: &Path,
    destination: &Path,
    restored_unix_mode: Option<u32>,
) -> Result<()> {
    let source_metadata = secure_file_metadata(source)?.ok_or_else(|| {
        BeadsError::internal(format!(
            "schema migration source disappeared before copy: {}",
            source.display()
        ))
    })?;
    let mut source_file = File::open(source).map_err(BeadsError::Io)?;
    let opened_metadata = source_file.metadata().map_err(BeadsError::Io)?;
    if !same_file_identity(&source_metadata, &opened_metadata) {
        return Err(BeadsError::internal(format!(
            "schema migration source changed identity before copy: {}",
            source.display()
        )));
    }
    let mut destination_file = open_private_file_new(destination)?;
    std::io::copy(&mut source_file, &mut destination_file).map_err(BeadsError::Io)?;
    destination_file.sync_all().map_err(BeadsError::Io)?;
    set_file_permissions(destination, restored_unix_mode)?;
    Ok(())
}

fn verify_backup_family(
    db_path: &Path,
    before_dir: &Path,
    expected: &RawFamilyWitness,
) -> Result<()> {
    for component in &expected.components {
        let backup = backup_component_path(before_dir, db_path, &component.suffix)?;
        let metadata = secure_file_metadata(&backup)?;
        if !component.present {
            if metadata.is_some() {
                return Err(BeadsError::internal(format!(
                    "unexpected backup exists for absent family member {}",
                    backup.display()
                )));
            }
            continue;
        }
        let metadata = metadata.ok_or_else(|| {
            BeadsError::internal(format!(
                "required schema migration backup is missing: {}",
                backup.display()
            ))
        })?;
        let (length, sha256) = hash_regular_file(&backup, &metadata)?;
        if component.length != Some(length) || component.sha256.as_deref() != Some(&sha256) {
            return Err(BeadsError::internal(format!(
                "schema migration backup hash mismatch for {}",
                backup.display()
            )));
        }
    }
    Ok(())
}

fn write_json_new<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let encoded = serde_json::to_vec_pretty(value).map_err(BeadsError::Json)?;
    let mut file = open_private_file_new(path)?;
    file.write_all(&encoded).map_err(BeadsError::Io)?;
    file.write_all(b"\n").map_err(BeadsError::Io)?;
    file.sync_all().map_err(BeadsError::Io)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String> {
    let metadata = secure_file_metadata(path)?.ok_or_else(|| {
        BeadsError::internal(format!(
            "schema migration artifact does not exist: {}",
            path.display()
        ))
    })?;
    let (_, sha256) = hash_regular_file(path, &metadata)?;
    Ok(sha256)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let metadata = secure_file_metadata(path)?.ok_or_else(|| {
        BeadsError::internal(format!(
            "schema migration receipt does not exist: {}",
            path.display()
        ))
    })?;
    let (length, _) = hash_regular_file(path, &metadata)?;
    if length > 16 * 1024 * 1024 {
        return Err(BeadsError::internal(format!(
            "schema migration receipt exceeds 16 MiB: {}",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(BeadsError::Io)?;
    serde_json::from_slice(&bytes).map_err(BeadsError::Json)
}

fn stable_raw_eq(left: &RawFamilyWitness, right: &RawFamilyWitness) -> bool {
    left.components
        .iter()
        .filter(|component| component.suffix != "-shm")
        .eq(right
            .components
            .iter()
            .filter(|component| component.suffix != "-shm"))
}

fn quarantine_live_family_resuming(
    db_path: &Path,
    quarantine_dir: &Path,
    applied: &RawFamilyWitness,
    restored: &RawFamilyWitness,
) -> Result<()> {
    for component in &applied.components {
        let source = family_component_path(db_path, &component.suffix);
        let destination = backup_component_path(quarantine_dir, db_path, &component.suffix)?;
        let source_metadata = secure_file_metadata(&source)?;
        let destination_metadata = secure_file_metadata(&destination)?;

        if component.present {
            if let Some(metadata) = destination_metadata {
                verify_component_bytes(&destination, &metadata, component)?;
                if let Some(source_metadata) = source_metadata {
                    let restored_component = component_for_suffix(restored, &component.suffix)?;
                    if !restored_component.present {
                        return Err(BeadsError::internal(format!(
                            "schema migration undo found both quarantined and unexpected live \
                             copies for {}",
                            source.display()
                        )));
                    }
                    verify_component_bytes(&source, &source_metadata, restored_component)?;
                }
                continue;
            }
            let source_metadata = source_metadata.ok_or_else(|| {
                BeadsError::internal(format!(
                    "schema migration undo cannot resume because both live and quarantined \
                     applied components are missing for {}",
                    source.display()
                ))
            })?;
            verify_component_bytes(&source, &source_metadata, component)?;
            fs::rename(&source, &destination).map_err(BeadsError::Io)?;
            set_file_permissions(&destination, None)?;
            continue;
        }

        if destination_metadata.is_some() {
            return Err(BeadsError::internal(format!(
                "schema migration undo quarantine contains an unexpected component {}",
                destination.display()
            )));
        }
        if let Some(source_metadata) = source_metadata {
            let restored_component = component_for_suffix(restored, &component.suffix)?;
            if !restored_component.present {
                return Err(BeadsError::internal(format!(
                    "schema migration undo found an unexpected live component {}",
                    source.display()
                )));
            }
            verify_component_bytes(&source, &source_metadata, restored_component)?;
        }
    }
    if let Some(parent) = db_path.parent() {
        sync_directory(parent)?;
    }
    sync_directory(quarantine_dir)
}

fn restore_backup_family_resuming(
    db_path: &Path,
    before_dir: &Path,
    expected: &RawFamilyWitness,
) -> Result<()> {
    for component in &expected.components {
        let destination = family_component_path(db_path, &component.suffix);
        let destination_metadata = secure_file_metadata(&destination)?;
        if !component.present {
            if destination_metadata.is_some() {
                return Err(BeadsError::internal(format!(
                    "schema migration restore found a live component that should be absent: {}",
                    destination.display()
                )));
            }
            continue;
        }
        let source = backup_component_path(before_dir, db_path, &component.suffix)?;
        if let Some(metadata) = destination_metadata {
            verify_component_bytes(&destination, &metadata, component)?;
            set_file_permissions(&destination, component.unix_mode)?;
        } else {
            copy_regular_file_new(&source, &destination, component.unix_mode)?;
        }
    }
    if let Some(parent) = db_path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn component_for_suffix<'a>(
    family: &'a RawFamilyWitness,
    suffix: &str,
) -> Result<&'a RawComponentWitness> {
    family
        .components
        .iter()
        .find(|component| component.suffix == suffix)
        .ok_or_else(|| {
            BeadsError::internal(format!(
                "schema migration witness omits required family suffix {suffix:?}"
            ))
        })
}

fn validate_raw_family_witness(family: &RawFamilyWitness) -> Result<()> {
    if family.components.len() != FAMILY_SUFFIXES.len() {
        return Err(BeadsError::internal(format!(
            "schema migration raw witness has {} components, expected {}",
            family.components.len(),
            FAMILY_SUFFIXES.len()
        )));
    }
    for (component, expected_suffix) in family.components.iter().zip(FAMILY_SUFFIXES) {
        let has_valid_hash = component.sha256.as_deref().is_some_and(|hash| {
            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
        if component.suffix != *expected_suffix
            || (component.present && (component.length.is_none() || !has_valid_hash))
            || (!component.present
                && (component.length.is_some()
                    || component.sha256.is_some()
                    || component.unix_mode.is_some()))
        {
            return Err(BeadsError::internal(format!(
                "schema migration raw witness has an invalid component for suffix \
                 {expected_suffix:?}"
            )));
        }
    }
    Ok(())
}

fn verify_component_bytes(
    path: &Path,
    metadata: &fs::Metadata,
    expected: &RawComponentWitness,
) -> Result<()> {
    let (length, sha256) = hash_regular_file(path, metadata)?;
    if !expected.present
        || expected.length != Some(length)
        || expected.sha256.as_deref() != Some(&sha256)
    {
        return Err(BeadsError::internal(format!(
            "schema migration component does not match its receipt: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.is_empty()
        || run_id == "."
        || run_id == ".."
        || run_id.contains('/')
        || run_id.contains('\\')
    {
        return Err(BeadsError::internal(format!(
            "invalid schema migration run id {run_id:?}"
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn open_private_file_new(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(BeadsError::Io)
}

#[cfg(not(unix))]
fn open_private_file_new(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(BeadsError::Io)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(BeadsError::Io)
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path, restored_unix_mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = restored_unix_mode.unwrap_or(0o600) & 0o7777;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(BeadsError::Io)
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path, _restored_unix_mode: Option<u32>) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(BeadsError::Io)
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn plan_token_binds_logical_state_not_sqlite_file_layout() {
        let mut logical = LogicalDatabaseWitness {
            user_version: 14,
            integrity_check: "ok".to_string(),
            schema_sha256: "schema".to_string(),
            contents_sha256: "contents".to_string(),
            tables: Vec::new(),
        };
        let forecast = MigrationForecast {
            from_version: 14,
            to_version: 15,
            content_hash_rows_rebuilt: 0,
            gate_result_history_created: true,
            post_migration_maintenance: true,
        };
        let first = compute_plan_token("db", &logical, &forecast).unwrap();
        assert_eq!(
            first,
            compute_plan_token("db", &logical, &forecast).unwrap(),
            "a repeated logical observation must be stable even when SQLite checkpoints or \
             retires sidecars between CLI processes"
        );
        logical.contents_sha256 = "changed".to_string();
        assert_ne!(
            first,
            compute_plan_token("db", &logical, &forecast).unwrap(),
            "any logical content change must stale the token"
        );
        logical.contents_sha256 = "contents".to_string();
        assert_ne!(
            first,
            compute_plan_token("different-db", &logical, &forecast).unwrap(),
            "the absolute database route remains token-bound"
        );
    }

    #[test]
    fn constant_time_text_comparison_covers_length_and_content() {
        assert!(constant_time_text_eq("abc", "abc"));
        assert!(!constant_time_text_eq("abc", "abd"));
        assert!(!constant_time_text_eq("abc", "ab"));
        assert!(!constant_time_text_eq("", "x"));
    }

    #[test]
    fn integrity_classification_allows_only_known_page_layout_artifacts() {
        assert!(integrity_check_is_clean("ok"));
        assert!(!integrity_check_is_repairable("ok"));
        assert!(integrity_check_is_repairable(
            "database disk image is malformed: page 54 is never used"
        ));
        assert!(integrity_check_is_repairable(
            "*** in database main ***\nrow 3 missing from index idx_issues_ready"
        ));
        assert!(!integrity_check_is_repairable(
            "database disk image is malformed: btree page 12 cell 4"
        ));
        assert!(!integrity_check_is_repairable(
            "page 54 is never used\nforeign key constraint failed"
        ));
    }

    #[test]
    fn run_id_validation_refuses_path_traversal() {
        for invalid in ["", ".", "..", "../run", "a/b", "a\\b"] {
            assert!(validate_run_id(invalid).is_err(), "{invalid:?}");
        }
        assert!(validate_run_id("20260727T120000.000000Z-1-0").is_ok());
    }

    fn reviewed_v14_migration_context() -> (TempDir, MigrationContext) {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir(&beads_dir).expect("create beads dir");
        let db_path = beads_dir.join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open db");
        crate::storage::schema::apply_schema(&conn).expect("create current schema");
        conn.execute(
            "INSERT INTO issues (
                id, title, status, priority, issue_type, created_at, updated_at
             ) VALUES (
                'bd-schema-rehearsal', 'Schema rehearsal', 'open', 2, 'task',
                '2026-07-27T12:00:00Z', '2026-07-27T12:00:00Z'
             )",
        )
        .expect("seed issue");
        conn.execute("DROP TABLE gate_result_history")
            .expect("restore v14 shape");
        conn.execute("PRAGMA user_version = 14").expect("stamp v14");
        close_connection(conn).expect("close fixture");

        let authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                &beads_dir,
                &db_path,
                Some(1000),
            )
            .expect("acquire test authority"),
        );
        (
            temp,
            MigrationContext {
                beads_dir,
                db_path,
                _write_authority: authority,
            },
        )
    }

    #[test]
    fn reviewed_plan_apply_and_undo_round_trip_exact_logical_state() {
        let (_temp, migration) = reviewed_v14_migration_context();
        let plan = build_plan(&migration.db_path).expect("build plan");
        assert!(plan.eligible);
        assert_eq!(plan.from_version, 14);
        assert_eq!(
            plan.to_version,
            crate::storage::schema::CURRENT_SCHEMA_VERSION as u32
        );
        let original_logical = plan.logical_witness.clone();
        let token = plan.plan_token.expect("plan token");

        execute_apply(
            &DoctorMigrateSchemaApplyArgs {
                plan_token: token,
                json: false,
            },
            &migration,
        )
        .expect("apply reviewed migration");

        let runs_root = migration_runs_root(&migration.beads_dir);
        let run_ids: Vec<String> = fs::read_dir(&runs_root)
            .expect("read runs")
            .map(|entry| {
                entry
                    .expect("run entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(run_ids.len(), 1, "one migration run should be recorded");
        let run_id = &run_ids[0];
        let applied: AppliedMigrationReceipt =
            read_json(&runs_root.join(run_id).join("applied.json")).expect("applied receipt");
        assert_eq!(
            applied
                .logical_after
                .as_ref()
                .expect("attested logical state")
                .user_version,
            crate::storage::schema::CURRENT_SCHEMA_VERSION as u32
        );
        assert!(applied.attested);
        assert!(applied.forecast.post_migration_maintenance);
        assert!(applied.effects.post_migration_maintenance_completed);
        assert_eq!(
            applied
                .logical_after
                .as_ref()
                .expect("attested logical state")
                .integrity_check,
            "ok"
        );
        assert!(runs_root.join(run_id).join("before/beads.db").is_file());

        execute_undo(
            &DoctorMigrateSchemaUndoArgs {
                run_id: run_id.clone(),
                dry_run: false,
                json: false,
            },
            &migration,
        )
        .expect("undo reviewed migration");
        assert_eq!(
            logical_witness(&migration.db_path).expect("restored witness"),
            original_logical
        );
        assert!(runs_root.join(run_id).join("undone.json").is_file());
        let quarantine = runs_root.join(run_id).join("undo-quarantine");
        assert!(
            fs::read_dir(quarantine)
                .expect("read quarantine")
                .next()
                .is_some(),
            "undo must retain the displaced applied state"
        );
        execute_undo(
            &DoctorMigrateSchemaUndoArgs {
                run_id: run_id.clone(),
                dry_run: false,
                json: false,
            },
            &migration,
        )
        .expect("completed undo must be idempotent");
    }

    #[test]
    fn reviewed_apply_refuses_stale_plan_before_creating_a_run() {
        let (_temp, migration) = reviewed_v14_migration_context();
        let plan = build_plan(&migration.db_path).expect("build plan");
        let token = plan.plan_token.expect("plan token");

        let conn =
            Connection::open(migration.db_path.to_string_lossy().into_owned()).expect("open db");
        conn.execute(
            "UPDATE issues SET title = 'Changed after plan' WHERE id = 'bd-schema-rehearsal'",
        )
        .expect("change planned state");
        close_connection(conn).expect("close changed db");

        let error = execute_apply(
            &DoctorMigrateSchemaApplyArgs {
                plan_token: token,
                json: false,
            },
            &migration,
        )
        .expect_err("stale token must be refused");
        assert!(error.to_string().contains("plan token is stale"), "{error}");
        assert!(
            !migration_runs_root(&migration.beads_dir).exists(),
            "a stale token must be refused before allocating recovery artifacts"
        );
    }

    #[test]
    fn reviewed_undo_refuses_a_changed_post_migration_database() {
        let (_temp, migration) = reviewed_v14_migration_context();
        let plan = build_plan(&migration.db_path).expect("build plan");
        execute_apply(
            &DoctorMigrateSchemaApplyArgs {
                plan_token: plan.plan_token.expect("plan token"),
                json: false,
            },
            &migration,
        )
        .expect("apply reviewed migration");

        let run_dir = fs::read_dir(migration_runs_root(&migration.beads_dir))
            .expect("read migration runs")
            .next()
            .expect("one run")
            .expect("run entry")
            .path();
        let run_id = run_dir
            .file_name()
            .and_then(|value| value.to_str())
            .expect("UTF-8 run id")
            .to_string();
        let conn =
            Connection::open(migration.db_path.to_string_lossy().into_owned()).expect("open db");
        conn.execute(
            "UPDATE issues SET title = 'New work after migration' \
             WHERE id = 'bd-schema-rehearsal'",
        )
        .expect("mutate post-migration database");
        close_connection(conn).expect("close changed db");

        let error = execute_undo(
            &DoctorMigrateSchemaUndoArgs {
                run_id,
                dry_run: false,
                json: false,
            },
            &migration,
        )
        .expect_err("undo must refuse newer tracker work");
        assert!(
            error.to_string().contains("live database has changed"),
            "{error}"
        );
        assert!(
            !run_dir.join("undo-prepared.json").exists(),
            "refused undo must not start a recovery state machine"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reviewed_undo_accepts_raw_layout_churn_when_logical_state_is_exact() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, migration) = reviewed_v14_migration_context();
        let plan = build_plan(&migration.db_path).expect("build plan");
        execute_apply(
            &DoctorMigrateSchemaApplyArgs {
                plan_token: plan.plan_token.expect("plan token"),
                json: false,
            },
            &migration,
        )
        .expect("apply reviewed migration");
        let run_dir = fs::read_dir(migration_runs_root(&migration.beads_dir))
            .expect("read migration runs")
            .next()
            .expect("one run")
            .expect("run entry")
            .path();
        let run_id = run_dir
            .file_name()
            .and_then(|value| value.to_str())
            .expect("UTF-8 run id")
            .to_string();

        fs::set_permissions(&migration.db_path, fs::Permissions::from_mode(0o640))
            .expect("change only raw file metadata");
        execute_undo(
            &DoctorMigrateSchemaUndoArgs {
                run_id,
                dry_run: true,
                json: false,
            },
            &migration,
        )
        .expect("logical equality must outrank raw layout churn");
        assert_eq!(
            logical_witness(&migration.db_path)
                .expect("post-migration logical witness")
                .user_version,
            crate::storage::schema::CURRENT_SCHEMA_VERSION as u32
        );
    }

    #[test]
    fn reviewed_undo_resumes_after_partial_quarantine() {
        let (_temp, migration) = reviewed_v14_migration_context();
        let original = build_plan(&migration.db_path)
            .expect("build plan")
            .logical_witness;
        let plan = build_plan(&migration.db_path).expect("build plan");
        execute_apply(
            &DoctorMigrateSchemaApplyArgs {
                plan_token: plan.plan_token.expect("plan token"),
                json: false,
            },
            &migration,
        )
        .expect("apply reviewed migration");

        let run_dir = fs::read_dir(migration_runs_root(&migration.beads_dir))
            .expect("read migration runs")
            .next()
            .expect("one run")
            .expect("run entry")
            .path();
        let run_id = run_dir
            .file_name()
            .and_then(|value| value.to_str())
            .expect("UTF-8 run id")
            .to_string();
        let applied_path = run_dir.join("applied.json");
        let applied: AppliedMigrationReceipt =
            read_json(&applied_path).expect("read applied receipt");
        let raw_live = raw_family_witness(&migration.db_path).expect("raw live state");
        let logical_live = logical_witness(&migration.db_path).expect("logical live state");
        let quarantine_dir = run_dir.join("undo-quarantine/undo-resume-fixture");
        let receipt = UndoReceipt {
            schema_version: UNDO_SCHEMA.to_string(),
            run_id: run_id.clone(),
            dry_run: false,
            database_path: migration.db_path.display().to_string(),
            quarantine_path: quarantine_dir.display().to_string(),
            applied_receipt_sha256: file_sha256(&applied_path).expect("applied hash"),
            raw_expected_before: applied.raw_before.clone(),
            logical_expected_before: applied.logical_before.clone(),
            raw_live_before_undo: raw_live,
            logical_live_before_undo: Some(logical_live),
            raw_restored: None,
            logical_restored: None,
        };
        ensure_directory(quarantine_dir.parent().expect("quarantine parent"))
            .expect("create quarantine parent");
        write_json_new(&run_dir.join("undo-prepared.json"), &receipt).expect("write prepared undo");
        ensure_new_directory(&quarantine_dir).expect("create quarantine");

        let quarantined_main =
            backup_component_path(&quarantine_dir, &migration.db_path, "").expect("backup path");
        fs::rename(&migration.db_path, &quarantined_main).expect("simulate first quarantine move");
        set_file_permissions(&quarantined_main, None).expect("harden quarantined file");

        execute_undo(
            &DoctorMigrateSchemaUndoArgs {
                run_id,
                dry_run: false,
                json: false,
            },
            &migration,
        )
        .expect("resume interrupted undo");
        assert_eq!(
            logical_witness(&migration.db_path).expect("restored logical state"),
            original
        );
        assert!(run_dir.join("undone.json").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn migration_recovery_artifacts_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, migration) = reviewed_v14_migration_context();
        let plan = build_plan(&migration.db_path).expect("build plan");
        execute_apply(
            &DoctorMigrateSchemaApplyArgs {
                plan_token: plan.plan_token.expect("plan token"),
                json: false,
            },
            &migration,
        )
        .expect("apply reviewed migration");
        let run_dir = fs::read_dir(migration_runs_root(&migration.beads_dir))
            .expect("read migration runs")
            .next()
            .expect("one run")
            .expect("run entry")
            .path();
        for directory in [
            &migration.beads_dir.join(".br_recovery"),
            &migration_runs_root(&migration.beads_dir),
            &run_dir,
            &run_dir.join("before"),
        ] {
            assert_eq!(
                fs::metadata(directory)
                    .expect("artifact directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        for file in [
            run_dir.join("prepared.json"),
            run_dir.join("applied.json"),
            run_dir.join("before/beads.db"),
        ] {
            assert_eq!(
                fs::metadata(file)
                    .expect("artifact file metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
