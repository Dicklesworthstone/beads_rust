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

use crate::franken_sync::Connection;
use crate::franken_sync::compat::{OpenFlags, open_with_flags};
use chrono::Utc;
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
use crate::sync::{DatabaseFamilyWriteLock, DatabaseTargetAuthorityState};

const PLAN_SCHEMA: &str = "br.doctor.schema_migration.plan.v1";
const PREPARED_SCHEMA: &str = "br.doctor.schema_migration.prepared.v1";
const COMMIT_READY_SCHEMA: &str = "br.doctor.schema_migration.commit_ready.v1";
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CommitReadyMigrationReceipt {
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
    logical_after: LogicalDatabaseWitness,
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
    // Additive diagnostic marker naming the maintenance/install stage that was
    // in progress when the failure surfaced (issue #443/#446 made Windows
    // failures undiagnosable without it). Optional so pre-existing receipts
    // keep deserializing and the schema string stays `failed.v1`; nothing
    // validates receipts strictly against unknown or missing fields
    // (`validate_failed_against_commit_ready` compares only identity fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failed_stage: Option<String>,
    raw_before: RawFamilyWitness,
    logical_before: LogicalDatabaseWitness,
    raw_observed_after_failure: Option<RawFamilyWitness>,
    logical_observed_after_failure: Option<LogicalDatabaseWitness>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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

// serde's `skip_serializing_if` contract requires `fn(&T) -> bool`.
#[allow(clippy::trivially_copy_pass_by_ref)]
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
    write_authority: Arc<DatabaseFamilyWriteLock>,
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
        write_authority,
    })
}

fn execute_plan(args: &DoctorMigrateSchemaPlanArgs, migration: &MigrationContext) -> Result<()> {
    let plan = build_plan(&migration.db_path)?;
    emit_plan(&plan, args.json)?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
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

#[allow(clippy::too_many_lines)]
fn execute_apply(args: &DoctorMigrateSchemaApplyArgs, migration: &MigrationContext) -> Result<()> {
    if args.plan_token.trim().is_empty() {
        return Err(BeadsError::internal(
            "schema migration apply requires a non-empty --plan-token",
        ));
    }
    if let Some((applied, before_dir)) = resume_commit_ready_migration(args, migration)? {
        return emit_applied(&applied, args.json, &before_dir);
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

    migration.write_authority.verify_database_authority()?;
    let mut failed_stage: Option<String> = None;
    let migration_result = apply_reviewed_migration(
        &migration.db_path,
        forecast.from_version,
        forecast.to_version,
        &marked_at,
        &run_dir,
        &migration.write_authority,
        &mut failed_stage,
    );
    let authority_result = migration.write_authority.verify_database_authority();
    let authority_verified_after = authority_result.is_ok();
    let migration_result = match (migration_result, authority_result) {
        (Ok(effects), Ok(())) => Ok(effects),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(authority_error)) => {
            // The maintenance pipeline itself completed; the stage it last
            // entered would misattribute this post-apply authority failure.
            mark_stage(&mut failed_stage, "post-apply-authority-verification");
            Err(authority_error)
        }
        (Err(error), Err(authority_error)) => Err(BeadsError::WithContext {
            context: format!(
                "reviewed schema migration failed and its database authority could not be \
                 re-verified afterward ({authority_error})"
            ),
            source: Box::new(error),
        }),
    };
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
                failed_stage,
                raw_before: plan.raw_witness,
                logical_before: plan.logical_witness,
                raw_observed_after_failure: raw_family_witness(&migration.db_path).ok(),
                logical_observed_after_failure: authority_verified_after
                    .then(|| logical_witness(&migration.db_path).ok())
                    .flatten(),
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
    // Deliberate cross-type comparison: the forecast promises maintenance
    // (`post_migration_maintenance`) and the effects attest completion
    // (`post_migration_maintenance_completed`).
    let maintenance_matches_forecast =
        effects.post_migration_maintenance_completed == forecast.post_migration_maintenance;
    if effects.from_version != forecast.from_version
        || effects.to_version != forecast.to_version
        || effects.content_hash_rows_rebuilt != forecast.content_hash_rows_rebuilt
        || effects.gate_result_history_created != forecast.gate_result_history_created
        || !maintenance_matches_forecast
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
    let commit_ready: CommitReadyMigrationReceipt = read_json(&run_dir.join("commit-ready.json"))?;
    validate_applied_against_commit_ready(&applied, &commit_ready, &run_dir)?;
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

    emit_applied(&applied, args.json, &before_dir)
}

fn emit_applied(applied: &AppliedMigrationReceipt, json: bool, before_dir: &Path) -> Result<()> {
    if json {
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

fn persist_commit_ready_marker(
    run_dir: &Path,
    db_path: &Path,
    logical_after: &LogicalDatabaseWitness,
    effects: ReviewedSchemaMigrationEffectsReceipt,
) -> Result<()> {
    let prepared_path = run_dir.join("prepared.json");
    let prepared_receipt_sha256 = file_sha256(&prepared_path)?;
    let prepared: PreparedMigrationReceipt = read_json(&prepared_path)?;
    let run_id = run_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| BeadsError::internal("schema migration run directory has no UTF-8 name"))?;
    if prepared.run_id != run_id || prepared.database_path != db_path.display().to_string() {
        return Err(BeadsError::internal(
            "prepared schema-migration receipt does not name its commit-ready run and database",
        ));
    }
    let marker = CommitReadyMigrationReceipt {
        schema_version: COMMIT_READY_SCHEMA.to_string(),
        run_id: prepared.run_id.clone(),
        database_path: prepared.database_path.clone(),
        plan_token: prepared.plan_token.clone(),
        prepared_receipt_sha256,
        marked_at: prepared.marked_at.clone(),
        forecast: prepared.forecast.clone(),
        effects,
        raw_before: prepared.raw_before.clone(),
        logical_before: prepared.logical_before.clone(),
        logical_after: logical_after.clone(),
    };
    validate_commit_ready_marker(&marker, run_dir)?;
    let marker_path = run_dir.join("commit-ready.json");
    write_json_new(&marker_path, &marker)?;
    let persisted: CommitReadyMigrationReceipt = read_json(&marker_path)?;
    validate_commit_ready_marker(&persisted, run_dir)?;
    if persisted != marker {
        return Err(BeadsError::internal(
            "persisted schema-migration commit-ready marker changed during read-back",
        ));
    }
    Ok(())
}

fn validate_commit_ready_marker(
    marker: &CommitReadyMigrationReceipt,
    run_dir: &Path,
) -> Result<()> {
    validate_raw_family_witness(&marker.raw_before)?;
    let run_id = run_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| BeadsError::internal("schema migration run directory has no UTF-8 name"))?;
    if marker.schema_version != COMMIT_READY_SCHEMA || marker.run_id != run_id {
        return Err(BeadsError::internal(format!(
            "invalid schema-migration commit-ready marker in {}",
            run_dir.display()
        )));
    }
    let prepared_path = run_dir.join("prepared.json");
    let prepared_sha256 = file_sha256(&prepared_path)?;
    let prepared: PreparedMigrationReceipt = read_json(&prepared_path)?;
    if prepared.schema_version != PREPARED_SCHEMA
        || !constant_time_text_eq(&prepared_sha256, &marker.prepared_receipt_sha256)
        || prepared.run_id != marker.run_id
        || prepared.database_path != marker.database_path
        || !constant_time_text_eq(&prepared.plan_token, &marker.plan_token)
        || prepared.marked_at != marker.marked_at
        || prepared.forecast != marker.forecast
        || prepared.raw_before != marker.raw_before
        || prepared.logical_before != marker.logical_before
    {
        return Err(BeadsError::internal(format!(
            "schema-migration commit-ready marker in {} is not bound to its prepared receipt",
            run_dir.display()
        )));
    }
    let recomputed_token = compute_plan_token(
        &marker.database_path,
        &marker.logical_before,
        &marker.forecast,
    )?;
    // The effects receipt records whether maintenance completed, while the
    // forecast records whether that maintenance was required.
    let maintenance_completion_disagrees_with_forecast =
        marker.effects.post_migration_maintenance_completed
            != marker.forecast.post_migration_maintenance;
    if !constant_time_text_eq(&recomputed_token, &marker.plan_token)
        || marker.effects.from_version != marker.forecast.from_version
        || marker.effects.to_version != marker.forecast.to_version
        || marker.effects.content_hash_rows_rebuilt != marker.forecast.content_hash_rows_rebuilt
        || marker.effects.gate_result_history_created != marker.forecast.gate_result_history_created
        || maintenance_completion_disagrees_with_forecast
        || marker.logical_after.user_version != marker.forecast.to_version
        || !integrity_check_is_clean(&marker.logical_after.integrity_check)
    {
        return Err(BeadsError::internal(format!(
            "schema-migration commit-ready marker in {} has inconsistent effects or witnesses",
            run_dir.display()
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn resume_commit_ready_migration(
    args: &DoctorMigrateSchemaApplyArgs,
    migration: &MigrationContext,
) -> Result<Option<(AppliedMigrationReceipt, PathBuf)>> {
    let root = migration_runs_root(&migration.beads_dir);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(BeadsError::Io(error)),
    };
    let database_path = migration.db_path.display().to_string();
    let mut matched = Vec::new();
    for entry in entries {
        let entry = entry.map_err(BeadsError::Io)?;
        let file_type = entry.file_type().map_err(BeadsError::Io)?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let run_dir = entry.path();
        let marker_path = run_dir.join("commit-ready.json");
        if secure_file_metadata(&marker_path)?.is_none() {
            continue;
        }
        let marker: CommitReadyMigrationReceipt = read_json(&marker_path)?;
        validate_commit_ready_marker(&marker, &run_dir)?;
        if marker.database_path != database_path
            || !constant_time_text_eq(&marker.plan_token, args.plan_token.trim())
        {
            continue;
        }
        matched.push((run_dir, marker));
    }
    matched.sort_by(|left, right| left.1.run_id.cmp(&right.1.run_id));
    let Some((run_dir, marker)) = matched.pop() else {
        return Ok(None);
    };
    let before_dir = run_dir.join("before");
    verify_backup_family(&migration.db_path, &before_dir, &marker.raw_before)?;

    let applied_path = run_dir.join("applied.json");
    if secure_file_metadata(&applied_path)?.is_some() {
        // A completed run has consumed its plan token: the token binds a plan
        // to the database state it was computed from, and that state no
        // longer exists after the apply. Re-reporting success here would let
        // a scripted re-apply believe it performed a migration, so while the
        // live database still carries this run's applied generation the
        // re-apply refuses and directs the caller to re-plan. After an undo
        // the live database is a different generation again, the token
        // legitimately re-derives from the restored state, and the completed
        // run no longer speaks for it — fall through to a fresh apply.
        let applied: AppliedMigrationReceipt = read_json(&applied_path)?;
        validate_applied_against_commit_ready(&applied, &marker, &run_dir)?;
        let logical_live = logical_witness(&migration.db_path).ok();
        // Capture the fallback raw witness after the SQLite probe so a
        // legitimate shared-memory sidecar rewrite cannot stale it immediately.
        let raw_live = raw_family_witness(&migration.db_path)?;
        if require_unchanged_applied_state(
            &applied,
            &raw_live,
            logical_live.as_ref(),
            &applied.run_id,
        )
        .is_ok()
        {
            return Err(BeadsError::internal(format!(
                "schema migration plan token was already consumed by completed run {}; \
                 run `br doctor migrate-schema plan` again for the current database state",
                applied.run_id
            )));
        }
        return Ok(None);
    }

    migration.write_authority.verify_database_authority()?;
    let raw_live_before_logical_probe = raw_family_witness(&migration.db_path)?;
    let original_main = component_for_suffix(&marker.raw_before, "")?;
    let live_main = component_for_suffix(&raw_live_before_logical_probe, "")?;
    let main_still_matches_original = live_main.present == original_main.present
        && live_main.length == original_main.length
        && live_main.sha256 == original_main.sha256;
    if raw_live_before_logical_probe == marker.raw_before || main_still_matches_original {
        let raw_live = restore_interrupted_preinstall_family(
            &migration.db_path,
            &before_dir,
            &run_dir.join("maintenance-displaced"),
            &marker.raw_before,
            &migration.write_authority,
        )?;
        let logical_live = logical_witness(&migration.db_path)?;
        migration.write_authority.verify_database_authority()?;
        let raw_live_after_probe = raw_family_witness(&migration.db_path)?;
        if logical_live != marker.logical_before
            || !stable_raw_eq(&raw_live_after_probe, &marker.raw_before)
            || raw_live != marker.raw_before
        {
            return Err(BeadsError::internal(format!(
                "commit-ready schema migration {} could not prove the exact original family \
                 after reconciling an interrupted pre-install state; retained recovery \
                 artifacts at {}",
                marker.run_id,
                run_dir.display()
            )));
        }
        let failed = FailedMigrationReceipt {
            schema_version: FAILED_SCHEMA.to_string(),
            run_id: marker.run_id.clone(),
            database_path: marker.database_path.clone(),
            plan_token: marker.plan_token.clone(),
            marked_at: marker.marked_at.clone(),
            error: "commit-ready migration intent was interrupted before the replacement became live; the original logical generation remains authoritative"
                .to_string(),
            // The interrupting process died without recording its stage; a
            // fabricated marker here would misattribute the interruption.
            failed_stage: None,
            raw_before: marker.raw_before.clone(),
            logical_before: marker.logical_before.clone(),
            raw_observed_after_failure: Some(raw_live_after_probe),
            logical_observed_after_failure: Some(logical_live),
        };
        let failed_path = run_dir.join("failed.json");
        if secure_file_metadata(&failed_path)?.is_some() {
            let existing: FailedMigrationReceipt = read_json(&failed_path)?;
            validate_failed_against_commit_ready(&existing, &marker, &run_dir)?;
        } else {
            write_json_new(&failed_path, &failed)?;
        }
        return Ok(None);
    }
    let logical_live = logical_witness(&migration.db_path)?;
    if logical_live != marker.logical_after {
        return Err(BeadsError::internal(format!(
            "commit-ready schema migration {} cannot be resumed because the live database is \
             an unrecognized generation; retained all recovery artifacts at {}",
            marker.run_id,
            run_dir.display()
        )));
    }
    if !current_runtime_shape_is_canonical(&migration.db_path)? {
        return Err(BeadsError::internal(format!(
            "commit-ready schema migration {} matches logically but not the canonical runtime shape",
            marker.run_id
        )));
    }
    let raw_live = raw_family_witness(&migration.db_path)?;
    let retained_original_guard = certify_resumed_installed_generation(
        &migration.db_path,
        &before_dir,
        &run_dir,
        &marker,
        &migration.write_authority,
    )?;
    migration.write_authority.finalize_database_replacement()?;
    let applied = AppliedMigrationReceipt {
        schema_version: APPLIED_SCHEMA.to_string(),
        run_id: marker.run_id.clone(),
        database_path: marker.database_path.clone(),
        plan_token: marker.plan_token.clone(),
        prepared_receipt_sha256: marker.prepared_receipt_sha256.clone(),
        marked_at: marker.marked_at.clone(),
        forecast: marker.forecast.clone(),
        effects: marker.effects,
        raw_before: marker.raw_before.clone(),
        logical_before: marker.logical_before.clone(),
        raw_after: Some(raw_live),
        logical_after: Some(logical_live),
        attested: true,
        attestation_errors: Vec::new(),
        undo_command: format!("br doctor migrate-schema undo {}", marker.run_id),
    };
    validate_applied_against_commit_ready(&applied, &marker, &run_dir)?;
    write_json_new(&applied_path, &applied)?;
    sync_directory(&run_dir)?;
    drop(retained_original_guard);
    Ok(Some((applied, before_dir)))
}

fn certify_resumed_installed_generation(
    db_path: &Path,
    before_dir: &Path,
    run_dir: &Path,
    marker: &CommitReadyMigrationReceipt,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
) -> Result<File> {
    verify_backup_family(db_path, before_dir, &marker.raw_before)?;
    write_authority.verify_database_authority()?;
    let displaced_dir = run_dir.join("maintenance-displaced");
    let displaced_metadata = fs::symlink_metadata(&displaced_dir).map_err(BeadsError::Io)?;
    if !displaced_metadata.is_dir() || displaced_metadata.file_type().is_symlink() {
        return Err(BeadsError::internal(format!(
            "schema-migration displaced path is not a real directory: {}",
            displaced_dir.display()
        )));
    }
    let displaced_main = backup_component_path(&displaced_dir, db_path, "")?;
    let candidate_path = maintenance_candidate_path(db_path, run_dir)?;
    let displaced_exists = secure_file_metadata(&displaced_main)?.is_some();
    let candidate_exists = secure_file_metadata(&candidate_path)?.is_some();
    if displaced_exists && candidate_exists {
        return Err(BeadsError::internal(format!(
            "resumed schema migration {} found two possible retained original mains; refusing to choose between {} and {}",
            marker.run_id,
            displaced_main.display(),
            candidate_path.display()
        )));
    }
    let guard = if displaced_exists {
        write_authority.lock_database_replacement_candidate(&displaced_main)?
    } else if candidate_exists {
        let guard = write_authority.lock_database_replacement_candidate(&candidate_path)?;
        write_authority.verify_locked_database_replacement_candidate(&candidate_path, &guard)?;
        rename_path_no_replace(&candidate_path, &displaced_main)?;
        write_authority.verify_locked_database_replacement_candidate(&displaced_main, &guard)?;
        guard
    } else {
        return Err(BeadsError::internal(format!(
            "resumed schema migration {} has no retained original main database",
            marker.run_id
        )));
    };
    write_authority.verify_locked_database_replacement_candidate(&displaced_main, &guard)?;
    let retained_logical = logical_witness(&displaced_main)?;
    if retained_logical != marker.logical_before {
        return Err(BeadsError::internal(format!(
            "resumed schema migration {} retained an unrecognized displaced generation at {}",
            marker.run_id,
            displaced_main.display()
        )));
    }
    sync_directory(before_dir)?;
    sync_directory(&displaced_dir)?;
    if let Some(parent) = db_path.parent() {
        sync_directory(parent)?;
    }
    sync_directory(run_dir)?;
    write_authority.verify_locked_database_replacement_candidate(&displaced_main, &guard)?;
    write_authority.verify_database_authority()?;
    Ok(guard)
}

fn restore_interrupted_preinstall_family(
    db_path: &Path,
    before_dir: &Path,
    displaced_dir: &Path,
    expected: &RawFamilyWitness,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
) -> Result<RawFamilyWitness> {
    write_authority.verify_database_authority()?;
    let expected_main = component_for_suffix(expected, "")?;
    let live_main = family_component_path(db_path, "");
    let live_main_metadata = secure_file_metadata(&live_main)?.ok_or_else(|| {
        BeadsError::internal("interrupted schema migration no longer has a live main database")
    })?;
    verify_component_bytes(&live_main, &live_main_metadata, expected_main)?;
    let displaced_main = backup_component_path(displaced_dir, db_path, "")?;
    if secure_file_metadata(&displaced_main)?.is_some() {
        return Err(BeadsError::internal(format!(
            "interrupted schema migration already displaced its original main database to {}; \
             refusing to classify it as a pre-install state",
            displaced_main.display()
        )));
    }

    for component in expected
        .components
        .iter()
        .filter(|component| !component.suffix.is_empty())
    {
        let live = family_component_path(db_path, &component.suffix);
        let displaced = backup_component_path(displaced_dir, db_path, &component.suffix)?;
        let live_metadata = secure_file_metadata(&live)?;
        let displaced_metadata = secure_file_metadata(&displaced)?;
        if !component.present {
            if live_metadata.is_some() {
                return Err(BeadsError::internal(format!(
                    "interrupted schema migration found an unexpected live sidecar {}; \
                     refusing to remove or overwrite it",
                    live.display()
                )));
            }
            continue;
        }
        if let Some(metadata) = live_metadata {
            verify_component_bytes(&live, &metadata, component)?;
            continue;
        }
        if let Some(metadata) = displaced_metadata
            && verify_component_bytes(&displaced, &metadata, component).is_ok()
        {
            rename_path_no_replace(&displaced, &live)?;
            let restored_metadata = secure_file_metadata(&live)?.ok_or_else(|| {
                BeadsError::internal(format!(
                    "interrupted schema-migration sidecar disappeared after restoration: {}",
                    live.display()
                ))
            })?;
            verify_component_bytes(&live, &restored_metadata, component)?;
            set_file_permissions(&live, component.unix_mode)?;
            continue;
        }

        // The live engine may have rewritten a sidecar after prepared.json was
        // captured. Preserve that displaced evidence and restore the exact
        // receipt-bound bytes from the immutable recovery bundle.
        let backup = backup_component_path(before_dir, db_path, &component.suffix)?;
        copy_regular_file_new(&backup, &live, component.unix_mode)?;
    }
    if let Some(parent) = db_path.parent() {
        sync_directory(parent)?;
    }
    match fs::symlink_metadata(displaced_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            sync_directory(displaced_dir)?;
        }
        Ok(_) => {
            return Err(BeadsError::internal(format!(
                "schema-migration displaced path is not a real directory: {}",
                displaced_dir.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(BeadsError::Io(error)),
    }
    write_authority.verify_database_authority()?;
    let restored = raw_family_witness(db_path)?;
    if restored != *expected {
        return Err(BeadsError::internal(
            "interrupted schema migration did not restore the exact receipt-bound raw family",
        ));
    }
    Ok(restored)
}

fn validate_failed_against_commit_ready(
    failed: &FailedMigrationReceipt,
    marker: &CommitReadyMigrationReceipt,
    run_dir: &Path,
) -> Result<()> {
    if failed.schema_version != FAILED_SCHEMA
        || failed.run_id != marker.run_id
        || failed.database_path != marker.database_path
        || !constant_time_text_eq(&failed.plan_token, &marker.plan_token)
        || failed.marked_at != marker.marked_at
        || failed.raw_before != marker.raw_before
        || failed.logical_before != marker.logical_before
    {
        return Err(BeadsError::internal(format!(
            "failed schema-migration receipt in {} is inconsistent with its commit-ready marker",
            run_dir.display()
        )));
    }
    Ok(())
}

fn validate_applied_against_commit_ready(
    applied: &AppliedMigrationReceipt,
    marker: &CommitReadyMigrationReceipt,
    run_dir: &Path,
) -> Result<()> {
    validate_commit_ready_marker(marker, run_dir)?;
    let raw_after = applied.raw_after.as_ref().ok_or_else(|| {
        BeadsError::internal(format!(
            "applied schema-migration receipt in {} omits its raw committed witness",
            run_dir.display()
        ))
    })?;
    validate_raw_family_witness(raw_after)?;
    if applied.schema_version != APPLIED_SCHEMA
        || applied.run_id != marker.run_id
        || applied.database_path != marker.database_path
        || !constant_time_text_eq(&applied.plan_token, &marker.plan_token)
        || !constant_time_text_eq(
            &applied.prepared_receipt_sha256,
            &marker.prepared_receipt_sha256,
        )
        || applied.marked_at != marker.marked_at
        || applied.forecast != marker.forecast
        || applied.effects != marker.effects
        || applied.raw_before != marker.raw_before
        || applied.logical_before != marker.logical_before
        || applied.logical_after.as_ref() != Some(&marker.logical_after)
    {
        return Err(BeadsError::internal(format!(
            "applied schema-migration receipt in {} is inconsistent with its commit-ready marker",
            run_dir.display()
        )));
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
    failed_stage: &mut Option<String>,
) -> Result<ReviewedSchemaMigrationEffectsReceipt> {
    run_post_migration_maintenance(
        db_path,
        from,
        to,
        marked_at,
        run_dir,
        write_authority,
        failed_stage,
    )
}

/// Record the maintenance/install stage now being entered so a failure
/// anywhere inside it lands in the failure receipt as `failed_stage`.
fn mark_stage(failed_stage: &mut Option<String>, stage: &str) {
    *failed_stage = Some(stage.to_string());
}

/// Extract the unquoted canonical index name from one statement returned by
/// `canonical_index_creation_statements`.
///
/// Every current canonical index uses the fail-closed
/// `CREATE [UNIQUE] INDEX IF NOT EXISTS <name>` spelling. Keeping this parser
/// next to the reviewed migration makes the ownership boundary explicit: the
/// maintenance pass may re-spell only those named indexes and must leave every
/// other explicit index (including extension-schema indexes) untouched.
fn canonical_index_name(statement: &str) -> Result<&str> {
    let create_line = statement
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("CREATE INDEX ") || line.starts_with("CREATE UNIQUE INDEX "))
        .ok_or_else(|| {
            BeadsError::internal(
                "canonical index statement did not contain a CREATE INDEX declaration",
            )
        })?;
    let mut tokens = create_line.split_ascii_whitespace();
    if tokens.next() != Some("CREATE") {
        return Err(BeadsError::internal(
            "canonical index statement did not begin with CREATE",
        ));
    }
    let mut token = tokens.next();
    if token == Some("UNIQUE") {
        token = tokens.next();
    }
    if token != Some("INDEX")
        || tokens.next() != Some("IF")
        || tokens.next() != Some("NOT")
        || tokens.next() != Some("EXISTS")
    {
        return Err(BeadsError::internal(
            "canonical index statement did not use CREATE [UNIQUE] INDEX IF NOT EXISTS",
        ));
    }
    let name = tokens
        .next()
        .ok_or_else(|| BeadsError::internal("canonical index statement omitted its index name"))?;
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(BeadsError::internal(format!(
            "canonical index statement used an unsupported quoted or non-ASCII name: {name}"
        )));
    }
    Ok(name)
}

#[allow(clippy::too_many_lines)]
fn run_post_migration_maintenance(
    db_path: &Path,
    from: u32,
    to: u32,
    marked_at: &str,
    run_dir: &Path,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
    failed_stage: &mut Option<String>,
) -> Result<ReviewedSchemaMigrationEffectsReceipt> {
    mark_stage(failed_stage, "source-witness");
    write_authority.verify_database_authority()?;
    let source_logical = logical_witness(db_path)?;
    let source_permissions_witness = raw_family_witness(db_path)?;
    let source_unix_mode = component_for_suffix(&source_permissions_witness, "")?.unix_mode;
    // Build and migrate a replacement database without mutating the live
    // family.  The live main file and sidecars remain the rollback authority
    // until the fully attested candidate is atomically installed below.
    mark_stage(failed_stage, "vacuum-candidate");
    let candidate_path = maintenance_candidate_path(db_path, run_dir)?;
    require_absent_family(&candidate_path)?;
    let source_conn = Connection::open(db_path.to_string_lossy().into_owned())?;
    let escaped_path = candidate_path.to_string_lossy().replace('\'', "''");
    let candidate_result = source_conn
        .execute(&format!("VACUUM INTO '{escaped_path}'"))
        .map(|_| ())
        .map_err(BeadsError::Database);
    let close_result = close_connection(source_conn);
    match (candidate_result, close_result) {
        (Err(error), _) | (Ok(()), Err(error)) => return Err(error),
        (Ok(()), Ok(())) => {}
    }

    let effects = if from == to {
        ReviewedSchemaMigrationEffectsReceipt {
            from_version: from,
            to_version: to,
            content_hash_rows_rebuilt: 0,
            gate_result_history_created: false,
            post_migration_maintenance_completed: false,
        }
    } else {
        mark_stage(failed_stage, "candidate-schema-migration");
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

    mark_stage(failed_stage, "candidate-maintenance");
    let candidate_conn = Connection::open(candidate_path.to_string_lossy().into_owned())?;
    let maintenance_result = (|| {
        // The engine's VACUUM INTO re-serializes DDL text while rebuilding
        // sqlite_master (comments stripped, predicates re-parenthesized), so
        // the candidate's index spellings no longer token-match the canonical
        // schema and the shape attestation below would refuse an otherwise
        // correct migration. Index DDL is derived state: drop each canonical
        // index and re-execute its CREATE INDEX statement so the
        // candidate carries the exact SCHEMA_SQL spelling. Do not sweep every
        // explicit index in sqlite_master: indexes on non-br extension tables
        // are part of the operator's schema and must survive this migration
        // with both their DDL and enforcement intact.
        // The same re-serialization also mangles the issues table's CHECK
        // constraint spellings, which `issues_required_checks_canonical`
        // token-compares. A table's declaration text can only be corrected by
        // rebuilding it; the canonical rebuild is the same machinery runtime
        // schema repair uses, gated so a candidate whose text survived intact
        // skips the extra copy.
        if !crate::storage::schema::issues_required_checks_canonical(&candidate_conn) {
            crate::storage::schema::rebuild_issues_table(&candidate_conn)?;
        }
        let canonical_index_statements =
            crate::storage::schema::canonical_index_creation_statements();
        for statement in &canonical_index_statements {
            let name = canonical_index_name(statement)?;
            candidate_conn.execute(&format!("DROP INDEX IF EXISTS {}", quote_identifier(name)))?;
        }
        for statement in canonical_index_statements {
            candidate_conn.execute(statement)?;
        }
        candidate_conn.execute("REINDEX")?;
        candidate_conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    })();
    let close_result = close_connection(candidate_conn);
    match (maintenance_result, close_result) {
        (Err(error), _) | (Ok(()), Err(error)) => return Err(error),
        (Ok(()), Ok(())) => {}
    }

    mark_stage(failed_stage, "candidate-durability");
    set_file_permissions(&candidate_path, source_unix_mode)?;
    // The durability barrier must hold a WRITABLE handle: Windows
    // `FlushFileBuffers` demands write access, so syncing through a read-only
    // `File::open` handle fails deterministically with `ERROR_ACCESS_DENIED`
    // (os error 5) on NTFS — the issue #443/#446 failure. POSIX fsync accepts
    // either mode, so opening read+write changes nothing elsewhere.
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&candidate_path)
        .and_then(|file| file.sync_all())
        .map_err(|error| BeadsError::WithContext {
            context: format!(
                "could not make the schema-migration candidate durable at {}",
                candidate_path.display()
            ),
            source: Box::new(BeadsError::Io(error)),
        })?;
    if let Some(parent) = candidate_path.parent() {
        sync_directory(parent)?;
    }

    mark_stage(failed_stage, "candidate-attestation");
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
    mark_stage(failed_stage, "candidate-sidecar-retention");
    let candidate_sidecars_dir = run_dir.join("maintenance-candidate-sidecars");
    move_present_sidecars_new(
        &candidate_path,
        &candidate_sidecars_dir,
        &candidate_raw,
        "candidate-sidecar-retention",
    )?;
    mark_stage(failed_stage, "commit-ready-marker");
    let mut committed_effects = effects;
    committed_effects.post_migration_maintenance_completed = true;
    persist_commit_ready_marker(run_dir, db_path, &candidate_logical, committed_effects)?;

    mark_stage(failed_stage, "lock-replacement-candidate");
    let replacement_lock = write_authority.lock_database_replacement_candidate(&candidate_path)?;
    mark_stage(failed_stage, "displace-live-sidecars");
    let displaced_dir = run_dir.join("maintenance-displaced");
    ensure_new_directory(&displaced_dir)?;
    move_present_sidecars_new(
        db_path,
        &displaced_dir,
        &source_raw,
        "displace-live-sidecars",
    )?;
    let displaced_main = backup_component_path(&displaced_dir, db_path, "")?;
    mark_stage(failed_stage, "install-compacted-candidate");
    if let Err(failure) = install_compacted_candidate(
        &candidate_path,
        db_path,
        &displaced_main,
        replacement_lock,
        write_authority,
    ) {
        let CompactedInstallFailure {
            disposition,
            error,
            stage,
        } = failure;
        if let Some(stage) = stage {
            mark_stage(failed_stage, stage);
        }
        if disposition == CompactedInstallFailureDisposition::LiveStateUncertain {
            return Err(BeadsError::WithContext {
                context: format!(
                    "compacted database installation stopped with an uncertain live-main \
                     disposition; retained the original sidecars at {} instead of mixing \
                     database generations",
                    displaced_dir.display()
                ),
                source: Box::new(error),
            });
        }
        let rollback = restore_present_sidecars(
            db_path,
            &displaced_dir,
            &source_raw,
            "rollback-restore-live-sidecars",
        );
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(BeadsError::internal(format!(
                "compacted database installation failed ({error}); restoring retained sidecars \
                 also failed ({rollback_error})"
            ))),
        };
    }
    mark_stage(failed_stage, "post-install-verification");
    let post_install_result = (|| {
        verify_backup_family(db_path, &displaced_dir, &source_raw)?;
        let installed_logical = logical_witness(db_path)?;
        if installed_logical != candidate_logical {
            return Err(BeadsError::internal(format!(
                "installed compacted database did not match its attested candidate \
                 (installed integrity={:?}, candidate integrity={:?})",
                installed_logical.integrity_check, candidate_logical.integrity_check
            )));
        }
        // Every child directory has already synced its own entries. This final
        // parent barrier must still precede replacement finalization so a
        // failure remains rollback-capable under the retained original lock.
        sync_directory(run_dir)
    })();
    if let Err(error) = post_install_result {
        return Err(rollback_after_compacted_install_failure(
            db_path,
            &displaced_main,
            &displaced_dir,
            &source_raw,
            run_dir,
            write_authority,
            error,
        ));
    }

    mark_stage(failed_stage, "finalize-replacement");
    if let Err(error) = write_authority.finalize_database_replacement() {
        return Err(rollback_after_compacted_install_failure(
            db_path,
            &displaced_main,
            &displaced_dir,
            &source_raw,
            run_dir,
            write_authority,
            error,
        ));
    }
    Ok(committed_effects)
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

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn rename_path_no_replace(source: &Path, destination: &Path) -> Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};

    match renameat_with(CWD, source, CWD, destination, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::EXIST => Err(BeadsError::SyncConflict {
            message: format!(
                "refusing to overwrite schema-migration artifact {}",
                destination.display()
            ),
        }),
        Err(error) => Err(BeadsError::Io(std::io::Error::from(error))),
    }
}

#[cfg(windows)]
fn rename_path_no_replace(source: &Path, destination: &Path) -> Result<()> {
    match crate::sync::rename_path_no_replace_windows(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(BeadsError::SyncConflict {
                message: format!(
                    "refusing to overwrite schema-migration artifact {}",
                    destination.display()
                ),
            })
        }
        Err(error) => Err(BeadsError::Io(error)),
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
fn rename_path_no_replace(_source: &Path, _destination: &Path) -> Result<()> {
    Err(BeadsError::Config(
        "this platform does not provide the atomic no-replace rename required by reviewed schema migration"
            .to_string(),
    ))
}

/// Retry budget for Windows open-handle rename contention: ten attempts with
/// linearly growing pauses of `45ms * attempt` (~2s total sleep).
const FILE_CONTENTION_RETRY_ATTEMPTS: u32 = 10;
const FILE_CONTENTION_RETRY_STEP: std::time::Duration = std::time::Duration::from_millis(45);

/// Whether an error is the Windows-specific transient refusal to rename a
/// file that another process still holds open without `FILE_SHARE_DELETE`:
/// `ERROR_ACCESS_DENIED` (os error 5) or `ERROR_SHARING_VIOLATION` (os error
/// 32), typically from an antivirus scanner, search indexer, or backup agent
/// briefly touching the SQLite family (issue #443/#446). Only these two codes
/// are retriable; everything else is a real failure.
#[cfg(windows)]
fn is_windows_file_contention(error: &BeadsError) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    match error {
        BeadsError::Io(io_error) => matches!(
            io_error.raw_os_error(),
            Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION)
        ),
        _ => false,
    }
}

#[cfg(not(windows))]
const fn is_windows_file_contention(_error: &BeadsError) -> bool {
    false
}

/// No-replace rename hardened against transient Windows sharing conflicts.
///
/// On non-Windows platforms the contention classifier never matches, so this
/// is a transparent passthrough to [`rename_path_no_replace`] with behavior
/// identical to calling it directly.
fn rename_stage_no_replace(stage: &str, source: &Path, destination: &Path) -> Result<()> {
    retry_through_file_contention(
        stage,
        source,
        destination,
        std::thread::sleep,
        is_windows_file_contention,
        || rename_path_no_replace(source, destination),
    )
}

/// Run `operation` up to [`FILE_CONTENTION_RETRY_ATTEMPTS`] times, sleeping
/// between attempts, while `is_transient` classifies the failure as Windows
/// open-handle contention. Any other error returns unchanged on the attempt
/// that produced it; an exhausted budget names the stage and both paths so
/// the failure receipt pinpoints the blocked rename.
fn retry_through_file_contention<S, C, F>(
    stage: &str,
    source: &Path,
    destination: &Path,
    mut sleep: S,
    is_transient: C,
    mut operation: F,
) -> Result<()>
where
    S: FnMut(std::time::Duration),
    C: Fn(&BeadsError) -> bool,
    F: FnMut() -> Result<()>,
{
    let mut attempt: u32 = 1;
    loop {
        let error = match operation() {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        if !is_transient(&error) {
            return Err(error);
        }
        if attempt >= FILE_CONTENTION_RETRY_ATTEMPTS {
            return Err(BeadsError::WithContext {
                context: format!(
                    "schema-migration stage {stage} could not rename {} to {} after {attempt} \
                     attempts; another process still holds the file open without delete sharing",
                    source.display(),
                    destination.display()
                ),
                source: Box::new(error),
            });
        }
        sleep(FILE_CONTENTION_RETRY_STEP.saturating_mul(attempt));
        attempt += 1;
    }
}

fn move_present_sidecars_new(
    source_base: &Path,
    destination_dir: &Path,
    expected: &RawFamilyWitness,
    stage: &str,
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
    let rollback_stage = format!("{stage}-rollback");
    let mut moved = Vec::with_capacity(present_sidecars.len());
    for component in present_sidecars {
        let source = family_component_path(source_base, &component.suffix);
        let prepared_move = (|| {
            let destination =
                backup_component_path(destination_dir, source_base, &component.suffix)?;
            let metadata = secure_file_metadata(&source)?.ok_or_else(|| {
                BeadsError::internal(format!(
                    "schema-migration sidecar disappeared before retention: {}",
                    source.display()
                ))
            })?;
            verify_component_bytes(&source, &metadata, component)?;
            Ok(destination)
        })();
        let destination = match prepared_move {
            Ok(destination) => destination,
            Err(error) => {
                let rollback =
                    restore_moved_components(source_base, destination_dir, &moved, &rollback_stage);
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(BeadsError::internal(format!(
                        "could not prepare schema-migration sidecar move for {} ({error}); \
                         rollback of prior moves also failed ({rollback_error})",
                        source.display()
                    ))),
                };
            }
        };
        if let Err(error) = rename_stage_no_replace(stage, &source, &destination) {
            let rollback =
                restore_moved_components(source_base, destination_dir, &moved, &rollback_stage);
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(BeadsError::internal(format!(
                    "could not retain schema-migration sidecar {} ({error}); rollback of prior \
                     sidecar moves also failed: {rollback_error}",
                    source.display()
                ))),
            };
        }
        moved.push((*component).clone());
        let moved_verification = (|| {
            let moved_metadata = secure_file_metadata(&destination)?.ok_or_else(|| {
                BeadsError::internal(format!(
                    "schema-migration sidecar disappeared after retention: {}",
                    destination.display()
                ))
            })?;
            verify_component_bytes(&destination, &moved_metadata, component)
        })();
        if let Err(error) = moved_verification {
            let rollback =
                restore_moved_components(source_base, destination_dir, &moved, &rollback_stage);
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(BeadsError::internal(format!(
                    "retained schema-migration sidecar {} failed its post-move witness \
                     ({error}); rollback also failed ({rollback_error})",
                    destination.display()
                ))),
            };
        }
    }
    let sync_result = if let Some(parent) = source_base.parent() {
        sync_directory(parent).and_then(|()| sync_directory(destination_dir))
    } else {
        sync_directory(destination_dir)
    };
    if let Err(error) = sync_result {
        let rollback =
            restore_moved_components(source_base, destination_dir, &moved, &rollback_stage);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(BeadsError::internal(format!(
                "retained schema-migration sidecars could not be made durable ({error}); \
                 rollback also failed ({rollback_error})"
            ))),
        };
    }
    Ok(())
}

fn restore_moved_components(
    destination_base: &Path,
    source_dir: &Path,
    components: &[RawComponentWitness],
    stage: &str,
) -> Result<()> {
    for component in components.iter().rev() {
        let source = backup_component_path(source_dir, destination_base, &component.suffix)?;
        let destination = family_component_path(destination_base, &component.suffix);
        let source_metadata = secure_file_metadata(&source)?.ok_or_else(|| {
            BeadsError::internal(format!(
                "retained schema-migration component disappeared before restoration: {}",
                source.display()
            ))
        })?;
        verify_component_bytes(&source, &source_metadata, component)?;
        rename_stage_no_replace(stage, &source, &destination)?;
        let restored_metadata = secure_file_metadata(&destination)?.ok_or_else(|| {
            BeadsError::internal(format!(
                "schema-migration component disappeared after restoration: {}",
                destination.display()
            ))
        })?;
        verify_component_bytes(&destination, &restored_metadata, component)?;
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
    stage: &str,
) -> Result<()> {
    let components = expected
        .components
        .iter()
        .filter(|component| component.present && !component.suffix.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    restore_moved_components(destination_base, source_dir, &components, stage)
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn exchange_database_paths(left: &Path, right: &Path) -> Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};

    renameat_with(CWD, left, CWD, right, RenameFlags::EXCHANGE)
        .map_err(|error| BeadsError::Io(std::io::Error::from(error)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactedInstallFailureDisposition {
    OriginalRestored,
    LiveStateUncertain,
}

#[derive(Debug)]
struct CompactedInstallFailure {
    disposition: CompactedInstallFailureDisposition,
    error: BeadsError,
    /// The installation-state-machine step whose failure produced this value;
    /// surfaced as `failed_stage` in the failure receipt.
    stage: Option<&'static str>,
}

impl CompactedInstallFailure {
    fn restored(error: BeadsError) -> Self {
        Self {
            disposition: CompactedInstallFailureDisposition::OriginalRestored,
            error,
            stage: None,
        }
    }

    fn uncertain(error: BeadsError) -> Self {
        Self {
            disposition: CompactedInstallFailureDisposition::LiveStateUncertain,
            error,
            stage: None,
        }
    }

    fn at_stage(mut self, stage: &'static str) -> Self {
        self.stage = Some(stage);
        self
    }

    fn from_original_state(
        error: BeadsError,
        write_authority: &Arc<DatabaseFamilyWriteLock>,
    ) -> Self {
        match write_authority.verify_database_authority() {
            Ok(()) => Self::restored(error),
            Err(authority_error) => Self::uncertain(BeadsError::WithContext {
                context: format!(
                    "schema-migration installation failed before replacement, but the original \
                     database authority could not be re-verified ({authority_error})"
                ),
                source: Box::new(error),
            }),
        }
    }

    fn after_rollback(error: BeadsError, rollback: Result<()>) -> Self {
        match rollback {
            Ok(()) => Self::restored(error),
            Err(rollback_error) => Self::uncertain(BeadsError::WithContext {
                context: format!(
                    "schema-migration installation failed and rollback could not prove the \
                     original database generation durable and authoritative ({rollback_error})"
                ),
                source: Box::new(error),
            }),
        }
    }
}

impl std::fmt::Display for CompactedInstallFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, formatter)
    }
}

fn rollback_installed_main_to_original<F>(
    _candidate_path: &Path,
    db_path: &Path,
    displaced_main: &Path,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
    sync: &mut F,
) -> Result<()>
where
    F: FnMut(&Path) -> Result<()>,
{
    if write_authority.database_target_authority_state()? != DatabaseTargetAuthorityState::Held {
        return Err(BeadsError::SyncConflict {
            message: "refusing schema-migration rollback because the live database is no longer the authority-held replacement"
                .to_string(),
        });
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        exchange_database_paths(db_path, displaced_main)?;
        write_authority.restore_retained_database_inode_after_authorized_replace()?;
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        rename_stage_no_replace(
            "install-rollback-rename-live-to-candidate",
            db_path,
            _candidate_path,
        )?;
        rename_stage_no_replace(
            "install-rollback-rename-displaced-to-live",
            displaced_main,
            db_path,
        )?;
        write_authority.restore_retained_database_inode_after_authorized_replace()?;
    }

    write_authority.verify_database_authority()?;
    if let Some(parent) = db_path.parent() {
        sync(parent)?;
    }
    if let Some(parent) = displaced_main.parent() {
        sync(parent)?;
    }
    Ok(())
}

fn install_compacted_candidate(
    candidate_path: &Path,
    db_path: &Path,
    displaced_main: &Path,
    replacement_lock: File,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
) -> std::result::Result<(), CompactedInstallFailure> {
    install_compacted_candidate_with_sync(
        candidate_path,
        db_path,
        displaced_main,
        replacement_lock,
        write_authority,
        sync_directory,
    )
}

// The branch structure is the installation state machine: each fallible point
// must preserve whether the original generation is proven restored.
#[allow(clippy::too_many_lines)]
fn install_compacted_candidate_with_sync<F>(
    candidate_path: &Path,
    db_path: &Path,
    displaced_main: &Path,
    replacement_lock: File,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
    mut sync: F,
) -> std::result::Result<(), CompactedInstallFailure>
where
    F: FnMut(&Path) -> Result<()>,
{
    let displaced_metadata = match secure_file_metadata(displaced_main) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(
                CompactedInstallFailure::from_original_state(error, write_authority)
                    .at_stage("install-preflight"),
            );
        }
    };
    if displaced_metadata.is_some() {
        return Err(CompactedInstallFailure::from_original_state(
            BeadsError::internal(format!(
                "refusing to overwrite retained pre-compaction database {}",
                displaced_main.display()
            )),
            write_authority,
        )
        .at_stage("install-preflight"));
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        if let Err(error) = exchange_database_paths(candidate_path, db_path) {
            return Err(
                CompactedInstallFailure::from_original_state(error, write_authority)
                    .at_stage("install-exchange-candidate-with-live"),
            );
        }
        if let Err(error) =
            write_authority.verify_staged_database_recovery_authority(candidate_path)
        {
            let rollback = exchange_database_paths(candidate_path, db_path)
                .and_then(|()| write_authority.verify_database_authority())
                .and_then(|()| db_path.parent().map_or(Ok(()), &mut sync));
            return Err(CompactedInstallFailure::after_rollback(error, rollback)
                .at_stage("install-verify-staged-authority"));
        }
        if let Err(error) = write_authority.adopt_locked_database_replacement(replacement_lock) {
            let rollback = exchange_database_paths(candidate_path, db_path)
                .and_then(|()| {
                    write_authority.restore_retained_database_inode_after_authorized_replace()
                })
                .and_then(|()| write_authority.verify_database_authority())
                .and_then(|()| db_path.parent().map_or(Ok(()), &mut sync));
            return Err(CompactedInstallFailure::after_rollback(error, rollback)
                .at_stage("install-adopt-replacement"));
        }
        if let Err(error) = rename_stage_no_replace(
            "install-rename-candidate-to-displaced",
            candidate_path,
            displaced_main,
        ) {
            let rollback = exchange_database_paths(db_path, candidate_path)
                .and_then(|()| {
                    write_authority.restore_retained_database_inode_after_authorized_replace()
                })
                .and_then(|()| write_authority.verify_database_authority())
                .and_then(|()| db_path.parent().map_or(Ok(()), &mut sync));
            return Err(CompactedInstallFailure::after_rollback(error, rollback)
                .at_stage("install-rename-candidate-to-displaced"));
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        if let Err(error) =
            rename_stage_no_replace("install-rename-live-to-displaced", db_path, displaced_main)
        {
            return Err(
                CompactedInstallFailure::from_original_state(error, write_authority)
                    .at_stage("install-rename-live-to-displaced"),
            );
        }
        if let Err(error) =
            write_authority.verify_staged_database_recovery_authority(displaced_main)
        {
            let rollback = rename_stage_no_replace(
                "install-rollback-rename-displaced-to-live",
                displaced_main,
                db_path,
            )
            .and_then(|()| write_authority.verify_database_authority())
            .and_then(|()| db_path.parent().map_or(Ok(()), &mut sync));
            return Err(CompactedInstallFailure::after_rollback(error, rollback)
                .at_stage("install-verify-staged-authority"));
        }
        if let Err(error) =
            rename_stage_no_replace("install-rename-candidate-to-live", candidate_path, db_path)
        {
            let rollback = rename_stage_no_replace(
                "install-rollback-rename-displaced-to-live",
                displaced_main,
                db_path,
            )
            .and_then(|()| write_authority.verify_database_authority())
            .and_then(|()| db_path.parent().map_or(Ok(()), &mut sync));
            return Err(CompactedInstallFailure::after_rollback(error, rollback)
                .at_stage("install-rename-candidate-to-live"));
        }
        if let Err(error) = write_authority.adopt_locked_database_replacement(replacement_lock) {
            let rollback = rename_stage_no_replace(
                "install-rollback-rename-live-to-candidate",
                db_path,
                candidate_path,
            )
            .and_then(|()| {
                rename_stage_no_replace(
                    "install-rollback-rename-displaced-to-live",
                    displaced_main,
                    db_path,
                )
            })
            .and_then(|()| {
                write_authority.restore_retained_database_inode_after_authorized_replace()
            })
            .and_then(|()| write_authority.verify_database_authority())
            .and_then(|()| db_path.parent().map_or(Ok(()), &mut sync));
            return Err(CompactedInstallFailure::after_rollback(error, rollback)
                .at_stage("install-adopt-replacement"));
        }
    }

    let durability_result = db_path
        .parent()
        .map_or(Ok(()), &mut sync)
        .and_then(|()| displaced_main.parent().map_or(Ok(()), &mut sync));
    if let Err(error) = durability_result {
        let rollback = rollback_installed_main_to_original(
            candidate_path,
            db_path,
            displaced_main,
            write_authority,
            &mut sync,
        );
        return Err(
            CompactedInstallFailure::after_rollback(error, rollback).at_stage("install-durability")
        );
    }
    Ok(())
}

fn rollback_after_compacted_install_failure(
    db_path: &Path,
    displaced_main: &Path,
    displaced_dir: &Path,
    source_raw: &RawFamilyWitness,
    run_dir: &Path,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
    operation_error: BeadsError,
) -> BeadsError {
    let target_state = match write_authority.database_target_authority_state() {
        Ok(state) => state,
        Err(authority_error) => {
            return BeadsError::WithContext {
                context: format!(
                    "schema-migration post-installation validation failed, and the live database \
                     authority could not be classified ({authority_error}); retained the original \
                     family at {} and did not mix generations",
                    displaced_dir.display()
                ),
                source: Box::new(operation_error),
            };
        }
    };
    if target_state != DatabaseTargetAuthorityState::Held {
        return BeadsError::WithContext {
            context: format!(
                "schema-migration post-installation validation failed while the live database \
                 had authority state {target_state:?}; retained the original family at {} and \
                 left the live generation untouched",
                displaced_dir.display()
            ),
            source: Box::new(operation_error),
        };
    }

    let failed_dir = run_dir.join("maintenance-failed-new-family");
    let rollback_result = (|| {
        ensure_new_directory(&failed_dir)?;
        let installed_raw = raw_family_witness(db_path)?;
        move_present_sidecars_new(
            db_path,
            &failed_dir,
            &installed_raw,
            "rollback-quarantine-installed-sidecars",
        )?;
        rollback_compacted_install(db_path, displaced_main, &failed_dir, write_authority)?;
        restore_present_sidecars(
            db_path,
            displaced_dir,
            source_raw,
            "rollback-restore-live-sidecars",
        )?;
        write_authority.verify_database_authority()?;
        let restored_raw = raw_family_witness(db_path)?;
        if restored_raw != *source_raw {
            return Err(BeadsError::internal(
                "schema-migration rollback did not reproduce the attested original database family",
            ));
        }
        Ok(())
    })();

    match rollback_result {
        Ok(()) => BeadsError::WithContext {
            context: format!(
                "schema-migration post-installation validation failed; restored the original \
                 database family and retained the rejected replacement at {}",
                failed_dir.display()
            ),
            source: Box::new(operation_error),
        },
        Err(rollback_error) => BeadsError::WithContext {
            context: format!(
                "schema-migration post-installation validation failed and rollback could not \
                 prove the original family restored ({rollback_error}); recovery artifacts remain \
                 under {}",
                run_dir.display()
            ),
            source: Box::new(operation_error),
        },
    }
}

fn rollback_compacted_install(
    db_path: &Path,
    displaced_main: &Path,
    failed_dir: &Path,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
) -> Result<()> {
    if write_authority.database_target_authority_state()? != DatabaseTargetAuthorityState::Held {
        return Err(BeadsError::SyncConflict {
            message: "refusing to roll back a compacted database that is no longer the authority-held live generation"
                .to_string(),
        });
    }
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
        rename_path_no_replace(displaced_main, &failed_main)?;
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        rename_stage_no_replace("rollback-quarantine-installed-main", db_path, &failed_main)?;
        rename_stage_no_replace("rollback-restore-displaced-main", displaced_main, db_path)?;
        write_authority.restore_retained_database_inode_after_authorized_replace()?;
    }

    if let Some(parent) = db_path.parent() {
        sync_directory(parent)?;
    }
    sync_directory(failed_dir)
}

#[allow(
    clippy::too_many_lines,
    reason = "undo is a linear crash-recovery protocol whose operation ordering is safety-critical"
)]
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
        let logical_live = logical_witness(&migration.db_path).ok();
        // The receipt later governs exact quarantine moves. Capture it after
        // the SQLite probe, which may legitimately rewrite the volatile -shm
        // sidecar even though the logical database is unchanged.
        let raw_live = raw_family_witness(&migration.db_path)?;
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
    require_atomic_undo_exchange()?;
    let restored_candidate_dir = run_dir.join("undo-restored-candidate");
    ensure_directory(&restored_candidate_dir)?;
    set_private_directory_permissions(&restored_candidate_dir)?;
    let restored_candidate =
        backup_component_path(&restored_candidate_dir, &migration.db_path, "")?;
    let quarantined_main_guard = install_undo_restored_main_resuming(
        &migration.db_path,
        &before_dir,
        &restored_candidate,
        &quarantine_dir,
        &receipt.raw_live_before_undo,
        &applied.raw_before,
        &migration.write_authority,
    )?;
    quarantine_live_family_resuming(
        &migration.db_path,
        &quarantine_dir,
        &receipt.raw_live_before_undo,
        &applied.raw_before,
    )?;
    restore_backup_family_resuming(&migration.db_path, &before_dir, &applied.raw_before)?;

    // Prove the byte-for-byte restore before opening SQLite: a read-only open
    // may legitimately create or rewrite the volatile shared-memory sidecar.
    let raw_restored_exact = raw_family_witness(&migration.db_path)?;
    if raw_restored_exact != applied.raw_before {
        return Err(BeadsError::internal(format!(
            "schema migration undo for run {} did not reproduce the exact receipt-bound raw \
             pre-state; the displaced applied state remains quarantined at {}",
            args.run_id,
            quarantine_dir.display()
        )));
    }
    let logical_restored = logical_witness(&migration.db_path)?;
    let raw_restored_after_probe = raw_family_witness(&migration.db_path)?;
    if logical_restored != applied.logical_before
        || !stable_raw_eq(&raw_restored_after_probe, &raw_restored_exact)
    {
        return Err(BeadsError::internal(format!(
            "schema migration undo for run {} did not reproduce the verified pre-state; \
             the displaced applied state remains quarantined at {}",
            args.run_id,
            quarantine_dir.display()
        )));
    }
    receipt.raw_restored = Some(raw_restored_exact);
    receipt.logical_restored = Some(logical_restored);
    receipt.dry_run = false;
    migration.write_authority.verify_database_authority()?;
    if let Some(parent) = migration.db_path.parent() {
        sync_directory(parent)?;
    }
    sync_directory(&quarantine_dir)?;
    sync_directory(&restored_candidate_dir)?;
    sync_directory(&run_dir)?;
    migration.write_authority.verify_database_authority()?;
    migration.write_authority.finalize_database_replacement()?;
    drop(quarantined_main_guard);
    write_json_new(&run_dir.join("undone.json"), &receipt)?;
    sync_directory(&run_dir)?;
    emit_undo(&receipt, args.json)
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
// Keep one fallible cross-platform call site; unsupported builds fail before
// creating a staged restore or mutating the live namespace.
#[allow(clippy::unnecessary_wraps)]
fn require_atomic_undo_exchange() -> Result<()> {
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn require_atomic_undo_exchange() -> Result<()> {
    Err(BeadsError::Config(
        "schema-migration undo requires an atomic path-exchange primitive on this platform"
            .to_string(),
    ))
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn install_undo_restored_main_resuming(
    db_path: &Path,
    before_dir: &Path,
    restored_candidate: &Path,
    quarantine_dir: &Path,
    applied: &RawFamilyWitness,
    restored: &RawFamilyWitness,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
) -> Result<Option<File>> {
    let applied_main = component_for_suffix(applied, "")?;
    let restored_main = component_for_suffix(restored, "")?;
    if !applied_main.present || !restored_main.present {
        return Err(BeadsError::internal(
            "schema-migration undo receipts must witness both main database files",
        ));
    }
    let quarantined_main = backup_component_path(quarantine_dir, db_path, "")?;
    let live_metadata = secure_file_metadata(db_path)?
        .ok_or_else(|| BeadsError::internal("schema-migration undo found no live main database"))?;
    let live_matches_applied = component_bytes_match(db_path, &live_metadata, applied_main)?;
    let live_matches_restored = component_bytes_match(db_path, &live_metadata, restored_main)?;

    if let Some(quarantined_metadata) = secure_file_metadata(&quarantined_main)? {
        verify_component_bytes(&quarantined_main, &quarantined_metadata, applied_main)?;
        if !live_matches_restored {
            return Err(BeadsError::internal(
                "schema-migration undo has a quarantined applied main but the live main is not the receipt-bound original",
            ));
        }
        let guard = write_authority.lock_database_replacement_candidate(&quarantined_main)?;
        write_authority.verify_locked_database_replacement_candidate(&quarantined_main, &guard)?;
        write_authority.verify_database_authority()?;
        return Ok(Some(guard));
    }

    if let Some(candidate_metadata) = secure_file_metadata(restored_candidate)? {
        if component_bytes_match(restored_candidate, &candidate_metadata, applied_main)?
            && live_matches_restored
        {
            let guard = write_authority.lock_database_replacement_candidate(restored_candidate)?;
            write_authority
                .verify_locked_database_replacement_candidate(restored_candidate, &guard)?;
            write_authority.verify_database_authority()?;
            rename_path_no_replace(restored_candidate, &quarantined_main)?;
            write_authority
                .verify_locked_database_replacement_candidate(&quarantined_main, &guard)?;
            sync_directory(quarantine_dir)?;
            if let Some(parent) = db_path.parent() {
                sync_directory(parent)?;
            }
            return Ok(Some(guard));
        }
        if !component_bytes_match(restored_candidate, &candidate_metadata, restored_main)? {
            return Err(BeadsError::internal(format!(
                "schema-migration undo staging path contains an unrecognized generation: {}",
                restored_candidate.display()
            )));
        }
    } else {
        if !live_matches_applied {
            return Err(BeadsError::internal(
                "schema-migration undo cannot classify the live main database before staging",
            ));
        }
        let backup_main = backup_component_path(before_dir, db_path, "")?;
        copy_regular_file_new(&backup_main, restored_candidate, restored_main.unix_mode)?;
        if let Some(parent) = restored_candidate.parent() {
            sync_directory(parent)?;
        }
    }

    if !live_matches_applied {
        return Err(BeadsError::internal(
            "schema-migration undo staging is prepared, but the live main is no longer the applied generation",
        ));
    }
    write_authority.verify_database_authority()?;
    let replacement_lock =
        write_authority.lock_database_replacement_candidate(restored_candidate)?;
    write_authority
        .verify_locked_database_replacement_candidate(restored_candidate, &replacement_lock)?;
    exchange_database_paths(restored_candidate, db_path)?;
    if let Err(error) =
        write_authority.verify_staged_database_recovery_authority(restored_candidate)
    {
        let rollback = exchange_database_paths(restored_candidate, db_path)
            .and_then(|()| write_authority.verify_database_authority());
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(BeadsError::WithContext {
                context: format!(
                    "schema-migration undo rejected a foreign displaced main and rollback could not re-prove the applied authority ({rollback_error})"
                ),
                source: Box::new(error),
            }),
        };
    }
    if let Err(error) = write_authority.adopt_locked_database_replacement(replacement_lock) {
        let rollback = exchange_database_paths(restored_candidate, db_path)
            .and_then(|()| {
                write_authority.restore_retained_database_inode_after_authorized_replace()
            })
            .and_then(|()| write_authority.verify_database_authority());
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(BeadsError::WithContext {
                context: format!(
                    "schema-migration undo could not adopt its prelocked original and rollback could not re-prove the applied authority ({rollback_error})"
                ),
                source: Box::new(error),
            }),
        };
    }
    rename_path_no_replace(restored_candidate, &quarantined_main)?;
    if let Some(parent) = db_path.parent() {
        sync_directory(parent)?;
    }
    sync_directory(quarantine_dir)?;
    write_authority.verify_database_authority()?;
    Ok(None)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
#[allow(clippy::too_many_arguments)]
fn install_undo_restored_main_resuming(
    _db_path: &Path,
    _before_dir: &Path,
    _restored_candidate: &Path,
    _quarantine_dir: &Path,
    _applied: &RawFamilyWitness,
    _restored: &RawFamilyWitness,
    _write_authority: &Arc<DatabaseFamilyWriteLock>,
) -> Result<Option<File>> {
    require_atomic_undo_exchange()?;
    unreachable!("unsupported schema-migration undo exchange returned success")
}

fn component_bytes_match(
    path: &Path,
    metadata: &fs::Metadata,
    expected: &RawComponentWitness,
) -> Result<bool> {
    let (length, sha256) = hash_regular_file(path, metadata)?;
    Ok(expected.present
        && expected.length == Some(length)
        && expected.sha256.as_deref() == Some(&sha256))
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
    let marker: CommitReadyMigrationReceipt = read_json(&run_dir.join("commit-ready.json"))?;
    validate_applied_against_commit_ready(applied, &marker, run_dir)?;
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
    let applied_path = run_dir.join("applied.json");
    let applied_sha256 = file_sha256(&applied_path)?;
    if !constant_time_text_eq(&applied_sha256, &receipt.applied_receipt_sha256) {
        return Err(BeadsError::internal(format!(
            "schema migration run {} failed its applied-to-undo receipt hash chain",
            args.run_id
        )));
    }
    let applied: AppliedMigrationReceipt = read_json(&applied_path)?;
    validate_applied_receipt(&applied, args, migration, &run_dir)?;
    let quarantine_dir = validate_quarantine_path(receipt, &run_dir)?;
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
    if receipt.raw_expected_before != applied.raw_before
        || receipt.logical_expected_before != applied.logical_before
        || receipt.logical_live_before_undo.as_ref() != applied.logical_after.as_ref()
        || raw_restored != &applied.raw_before
        || logical_restored != &applied.logical_before
    {
        return Err(BeadsError::internal(format!(
            "schema migration run {} has an internally inconsistent completed undo receipt",
            args.run_id
        )));
    }
    verify_backup_family(
        &migration.db_path,
        &quarantine_dir,
        &receipt.raw_live_before_undo,
    )?;
    if logical_witness(&migration.db_path)? != *logical_restored
        || !stable_raw_eq(&raw_family_witness(&migration.db_path)?, raw_restored)
    {
        return Err(BeadsError::internal(format!(
            "schema migration run {} was previously undone, but the live database family has since changed",
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
    let mut buffer = vec![0_u8; 1024 * 1024];
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

// The `Option` is required by the shared signature: the `#[cfg(not(unix))]`
// twin below returns `None`.
#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
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
    let parent = path.parent().ok_or_else(|| {
        BeadsError::internal(format!(
            "schema migration receipt path has no parent: {}",
            path.display()
        ))
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        BeadsError::internal(format!(
            "schema migration receipt path has no file name: {}",
            path.display()
        ))
    })?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| {
            BeadsError::internal(format!("system clock precedes Unix epoch: {error}"))
        })?
        .as_nanos();
    let mut staged = None;
    for attempt in 0_u16..=u16::MAX {
        let mut staged_name = OsString::from(".");
        staged_name.push(file_name);
        staged_name.push(format!(
            ".write-{}-{nonce}-{attempt}.tmp",
            std::process::id()
        ));
        let staged_path = parent.join(staged_name);
        match open_private_file_new(&staged_path) {
            Ok(file) => {
                staged = Some((staged_path, file));
                break;
            }
            Err(BeadsError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let (staged_path, mut file) = staged.ok_or_else(|| {
        BeadsError::internal(format!(
            "could not allocate a unique staged receipt beside {}",
            path.display()
        ))
    })?;
    file.write_all(&encoded).map_err(BeadsError::Io)?;
    file.write_all(b"\n").map_err(BeadsError::Io)?;
    file.sync_all().map_err(BeadsError::Io)?;
    drop(file);
    rename_path_no_replace(&staged_path, path)?;
    sync_directory(parent)?;
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
            rename_path_no_replace(&source, &destination)?;
            let quarantined_metadata = secure_file_metadata(&destination)?.ok_or_else(|| {
                BeadsError::internal(format!(
                    "schema migration undo component disappeared after quarantine: {}",
                    destination.display()
                ))
            })?;
            verify_component_bytes(&destination, &quarantined_metadata, component)?;
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
    fn stable_raw_comparison_explicitly_classifies_present_shm_as_volatile() {
        let component = |suffix: &str, sha256: &str| RawComponentWitness {
            suffix: suffix.to_string(),
            present: true,
            length: Some(4096),
            sha256: Some(sha256.repeat(64)),
            unix_mode: None,
        };
        let absent = |suffix: &str| RawComponentWitness {
            suffix: suffix.to_string(),
            present: false,
            length: None,
            sha256: None,
            unix_mode: None,
        };
        let before_probe = RawFamilyWitness {
            components: vec![
                component("", "a"),
                absent("-wal"),
                component("-shm", "b"),
                absent("-journal"),
            ],
        };
        let after_probe = RawFamilyWitness {
            components: vec![
                component("", "a"),
                absent("-wal"),
                component("-shm", "c"),
                absent("-journal"),
            ],
        };
        assert_ne!(
            before_probe, after_probe,
            "the exact pre-open witness must still record the shm bytes that were restored"
        );
        assert!(
            stable_raw_eq(&before_probe, &after_probe),
            "post-open validation must explicitly exclude only the volatile shm component"
        );
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

    fn reviewed_v14_migration_context_with_database_name(
        database_name: &str,
    ) -> (TempDir, MigrationContext) {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir(&beads_dir).expect("create beads dir");
        let db_path = beads_dir.join(database_name);
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
                write_authority: authority,
            },
        )
    }

    fn reviewed_v14_migration_context() -> (TempDir, MigrationContext) {
        reviewed_v14_migration_context_with_database_name("beads.db")
    }

    #[test]
    fn reviewed_migration_preserves_extension_unique_index_ddl_and_enforcement() {
        const EXTENSION_INDEX_NAME: &str = "extension_records_external_key_unique";
        const EXTENSION_INDEX_DDL: &str = "CREATE UNIQUE INDEX extension_records_external_key_unique ON extension_records(external_key)";

        let (_temp, migration) = reviewed_v14_migration_context();
        let source_conn =
            Connection::open(migration.db_path.to_string_lossy().into_owned()).expect("open db");
        source_conn
            .execute(
                "CREATE TABLE extension_records (
                    external_key TEXT NOT NULL,
                    payload TEXT NOT NULL
                )",
            )
            .expect("create extension table");
        source_conn
            .execute(EXTENSION_INDEX_DDL)
            .expect("create extension unique index");
        source_conn
            .execute(
                "INSERT INTO extension_records (external_key, payload)
                 VALUES ('stable-key', 'before-migration')",
            )
            .expect("seed extension row");
        let source_index_sql = source_conn
            .query_row(&format!(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = \
                 '{EXTENSION_INDEX_NAME}'"
            ))
            .expect("read source extension index DDL")
            .get(0)
            .and_then(SqliteValue::as_text)
            .expect("extension index DDL is text")
            .to_string();
        assert_eq!(source_index_sql, EXTENSION_INDEX_DDL);
        close_connection(source_conn).expect("close extension fixture");

        let plan = build_plan(&migration.db_path).expect("build reviewed migration plan");
        assert_eq!(
            plan.from_version, 14,
            "fixture must exercise a real migration"
        );
        assert!(plan.to_version > plan.from_version);
        execute_apply(
            &DoctorMigrateSchemaApplyArgs {
                plan_token: plan.plan_token.expect("plan token"),
                json: false,
            },
            &migration,
        )
        .expect("apply reviewed migration with extension index");

        let migrated_conn =
            Connection::open(migration.db_path.to_string_lossy().into_owned()).expect("open db");
        let migrated_index_sql = migrated_conn
            .query_row(&format!(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = \
                 '{EXTENSION_INDEX_NAME}'"
            ))
            .expect("extension index survives migration")
            .get(0)
            .and_then(SqliteValue::as_text)
            .expect("migrated extension index DDL is text")
            .to_string();
        assert_eq!(
            migrated_index_sql, source_index_sql,
            "reviewed migration must preserve non-br index DDL exactly"
        );

        let duplicate_error = migrated_conn
            .execute(
                "INSERT INTO extension_records (external_key, payload)
                 VALUES ('stable-key', 'duplicate')",
            )
            .expect_err("extension unique index must remain enforced");
        let duplicate_message = duplicate_error.to_string().to_ascii_lowercase();
        assert!(
            duplicate_message.contains("unique") || duplicate_message.contains("constraint"),
            "expected unique/constraint failure, got: {duplicate_error}"
        );
        let row_count = migrated_conn
            .query_row("SELECT COUNT(*) FROM extension_records")
            .expect("count extension rows")
            .get(0)
            .and_then(SqliteValue::as_integer)
            .expect("extension row count is integer");
        assert_eq!(row_count, 1, "failed duplicate insert must not add a row");
        close_connection(migrated_conn).expect("close migrated extension fixture");
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the end-to-end apply-and-undo proof is clearest as one ordered scenario"
    )]
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
        let marker: CommitReadyMigrationReceipt =
            read_json(&runs_root.join(run_id).join("commit-ready.json"))
                .expect("commit-ready receipt");
        let mut malformed_applied = applied.clone();
        malformed_applied
            .raw_after
            .as_mut()
            .expect("raw committed witness")
            .components
            .pop();
        let malformed_error = validate_applied_against_commit_ready(
            &malformed_applied,
            &marker,
            &runs_root.join(run_id),
        )
        .expect_err("malformed raw committed witness must be rejected");
        assert!(
            malformed_error.to_string().contains("raw witness"),
            "causal raw-witness validation error must remain visible: {malformed_error}"
        );
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
        let completed: UndoReceipt =
            read_json(&runs_root.join(run_id).join("undone.json")).expect("completed undo receipt");
        let mut inconsistent = completed.clone();
        inconsistent.logical_expected_before.user_version += 1;
        let inconsistent_error = validate_completed_undo_receipt(
            &inconsistent,
            &DoctorMigrateSchemaUndoArgs {
                run_id: run_id.clone(),
                dry_run: false,
                json: false,
            },
            &migration,
        )
        .expect_err("completed undo receipt must remain bound to the applied receipt");
        assert!(
            inconsistent_error
                .to_string()
                .contains("internally inconsistent"),
            "causal completed-receipt error must remain visible: {inconsistent_error}"
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

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn undo_atomically_displaces_the_authority_held_main_with_hardlink_aliases() {
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
        let applied_alias = migration.beads_dir.join("applied-generation-alias.db");
        fs::hard_link(&migration.db_path, &applied_alias).expect("create applied hardlink alias");
        let applied_identity = fs::metadata(&applied_alias).expect("alias metadata");

        execute_undo(
            &DoctorMigrateSchemaUndoArgs {
                run_id,
                dry_run: false,
                json: false,
            },
            &migration,
        )
        .expect("undo through atomic prelocked exchange");

        let quarantine_root = run_dir.join("undo-quarantine");
        let quarantine_dir = fs::read_dir(&quarantine_root)
            .expect("read quarantine root")
            .next()
            .expect("one quarantine")
            .expect("quarantine entry")
            .path();
        let quarantined_main = backup_component_path(&quarantine_dir, &migration.db_path, "")
            .expect("quarantine main");
        let quarantined_identity = fs::metadata(&quarantined_main).expect("quarantined metadata");
        let restored_identity = fs::metadata(&migration.db_path).expect("restored metadata");
        assert!(
            same_file_identity(&applied_identity, &quarantined_identity),
            "the alias-visible applied inode must be retained, not copied or overwritten"
        );
        assert!(
            !same_file_identity(&applied_identity, &restored_identity),
            "the canonical path must atomically adopt the prelocked original inode"
        );
        migration
            .write_authority
            .verify_database_authority()
            .expect("restored canonical inode remains authoritative");
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
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        windows
    ))]
    fn failed_post_adoption_verification_restores_database_and_authority() {
        let (_temp, migration) = reviewed_v14_migration_context();
        let raw_before = raw_family_witness(&migration.db_path).expect("original raw witness");
        let candidate_path = migration.beads_dir.join("candidate.db");
        let locked_stale_path = migration.beads_dir.join("locked-stale-candidate.db");
        let displaced_main = migration.beads_dir.join("displaced-original.db");
        fs::write(&candidate_path, b"candidate generation").expect("write candidate");
        fs::write(&locked_stale_path, b"stale locked generation")
            .expect("write stale locked generation");

        // Model a candidate-path replacement after the candidate inode was
        // locked but before installation. Adoption records the stale locked
        // inode, then its post-adoption verification rejects the different
        // inode that was actually installed.
        let replacement_lock = migration
            .write_authority
            .lock_database_replacement_candidate(&locked_stale_path)
            .expect("lock stale candidate inode");
        let error = install_compacted_candidate(
            &candidate_path,
            &migration.db_path,
            &displaced_main,
            replacement_lock,
            &migration.write_authority,
        )
        .expect_err("post-adoption identity mismatch must fail");

        assert!(
            error.to_string().contains("authority"),
            "failure should identify replacement authority: {error}"
        );
        assert_eq!(
            raw_family_witness(&migration.db_path).expect("restored raw witness"),
            raw_before,
            "the original database family must be restored exactly"
        );
        assert_eq!(
            fs::read(&candidate_path).expect("restored candidate"),
            b"candidate generation",
            "the rejected candidate must return to its staging path"
        );
        assert!(
            !displaced_main.exists(),
            "failure before original retention must not fabricate a displaced database"
        );
        migration
            .write_authority
            .verify_database_authority()
            .expect("restored original inode authority");
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    fn pre_adoption_fence_rejects_same_byte_foreign_canonical_inode() {
        let (_temp, migration) = reviewed_v14_migration_context();
        let original_metadata = fs::metadata(&migration.db_path).expect("original metadata");
        let foreign = migration.beads_dir.join("same-byte-foreign.db");
        let retained_original = migration.beads_dir.join("externally-retained-original.db");
        let candidate = migration.beads_dir.join("fenced-candidate.db");
        let displaced = migration.beads_dir.join("must-remain-absent.db");
        fs::copy(&migration.db_path, &foreign).expect("copy same-byte foreign inode");
        fs::write(&candidate, b"candidate generation").expect("write candidate");
        let replacement_lock = migration
            .write_authority
            .lock_database_replacement_candidate(&candidate)
            .expect("lock candidate");

        rename_path_no_replace(&migration.db_path, &retained_original)
            .expect("stage original outside canonical path");
        rename_path_no_replace(&foreign, &migration.db_path)
            .expect("install same-byte foreign canonical inode");
        let foreign_metadata = fs::metadata(&migration.db_path).expect("foreign metadata");
        assert!(
            !same_file_identity(&original_metadata, &foreign_metadata),
            "fixture must use a distinct inode with identical bytes"
        );

        let failure = install_compacted_candidate(
            &candidate,
            &migration.db_path,
            &displaced,
            replacement_lock,
            &migration.write_authority,
        )
        .expect_err("staged-original inode fence must reject a same-byte foreign target");
        assert_eq!(
            failure.disposition,
            CompactedInstallFailureDisposition::LiveStateUncertain
        );
        assert!(
            failure.to_string().contains("generation changed")
                || failure.to_string().contains("authority"),
            "causal inode mismatch must remain visible: {failure}"
        );
        assert_eq!(
            fs::read(&candidate).expect("candidate restored by compensating exchange"),
            b"candidate generation"
        );
        assert!(
            !displaced.exists(),
            "a rejected foreign target must never be accepted as the displaced original"
        );
        assert!(
            retained_original.is_file(),
            "the true original stays retained"
        );
    }

    #[test]
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        windows
    ))]
    fn post_install_directory_sync_failure_restores_original_main_and_authority() {
        let (_temp, migration) = reviewed_v14_migration_context();
        let raw_before = raw_family_witness(&migration.db_path).expect("original raw witness");
        let candidate_path = migration.beads_dir.join("sync-failure-candidate.db");
        let displaced_dir = migration.beads_dir.join("sync-failure-displaced");
        fs::create_dir(&displaced_dir).expect("create displaced directory");
        let displaced_main = displaced_dir.join("beads.db");
        fs::write(&candidate_path, b"candidate generation").expect("write candidate");
        let replacement_lock = migration
            .write_authority
            .lock_database_replacement_candidate(&candidate_path)
            .expect("lock candidate inode");
        let sync_calls = std::cell::Cell::new(0_u32);

        let failure = install_compacted_candidate_with_sync(
            &candidate_path,
            &migration.db_path,
            &displaced_main,
            replacement_lock,
            &migration.write_authority,
            |_| {
                let call = sync_calls.get();
                sync_calls.set(call + 1);
                if call == 0 {
                    Err(BeadsError::Io(std::io::Error::other(
                        "forced post-install directory sync failure",
                    )))
                } else {
                    Ok(())
                }
            },
        )
        .expect_err("post-install sync failure must fail the installation");

        assert_eq!(
            failure.disposition,
            CompactedInstallFailureDisposition::OriginalRestored,
            "a successful compensating exchange and sync must prove restoration"
        );
        assert!(
            failure
                .to_string()
                .contains("forced post-install directory sync failure"),
            "the causal durability failure must remain visible: {failure}"
        );
        assert_eq!(
            raw_family_witness(&migration.db_path).expect("restored raw witness"),
            raw_before,
            "the original main and sidecars must remain byte-exact"
        );
        migration
            .write_authority
            .verify_database_authority()
            .expect("restored original inode authority");
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        assert_eq!(
            fs::read(&displaced_main).expect("retained rejected candidate"),
            b"candidate generation",
            "the rejected replacement must be retained without overwriting another path"
        );
        #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
        assert_eq!(
            fs::read(&candidate_path).expect("restored rejected candidate"),
            b"candidate generation",
            "the rejected replacement must return to its no-replace staging path"
        );
    }

    #[test]
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        windows
    ))]
    fn repeated_directory_sync_failure_is_reported_as_uncertain() {
        let (_temp, migration) = reviewed_v14_migration_context();
        let candidate_path = migration.beads_dir.join("uncertain-sync-candidate.db");
        let displaced_dir = migration.beads_dir.join("uncertain-sync-displaced");
        fs::create_dir(&displaced_dir).expect("create displaced directory");
        let displaced_main = displaced_dir.join("beads.db");
        fs::write(&candidate_path, b"candidate generation").expect("write candidate");
        let replacement_lock = migration
            .write_authority
            .lock_database_replacement_candidate(&candidate_path)
            .expect("lock candidate inode");

        let failure = install_compacted_candidate_with_sync(
            &candidate_path,
            &migration.db_path,
            &displaced_main,
            replacement_lock,
            &migration.write_authority,
            |_| {
                Err(BeadsError::Io(std::io::Error::other(
                    "forced persistent directory sync failure",
                )))
            },
        )
        .expect_err("an unproved rollback must fail closed");

        assert_eq!(
            failure.disposition,
            CompactedInstallFailureDisposition::LiveStateUncertain,
            "the caller must not restore original sidecars without a durable rollback proof"
        );
        assert!(
            failure.to_string().contains("could not prove"),
            "uncertain disposition must explain the missing proof: {failure}"
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[allow(
        clippy::too_many_lines,
        reason = "the crash-resume fixture keeps each persisted state transition visible in order"
    )]
    fn commit_ready_marker_resumes_applied_receipt_and_normal_undo() {
        let (_temp, migration) = reviewed_v14_migration_context();
        let original_logical = logical_witness(&migration.db_path).expect("original witness");
        let plan = build_plan(&migration.db_path).expect("build migration plan");
        let forecast = plan.forecast.clone().expect("eligible forecast");
        let plan_token = plan.plan_token.clone().expect("plan token");
        let run_id = allocate_run_id(&migration.beads_dir).expect("allocate run");
        let run_dir = migration_runs_root(&migration.beads_dir).join(&run_id);
        let before_dir = run_dir.join("before");
        ensure_new_directory(&before_dir).expect("create before directory");
        copy_family_to_backup(&migration.db_path, &before_dir, &plan.raw_witness)
            .expect("copy recovery family");
        verify_backup_family(&migration.db_path, &before_dir, &plan.raw_witness)
            .expect("verify recovery family");
        let marked_at = Utc::now().to_rfc3339();
        let prepared = PreparedMigrationReceipt {
            schema_version: PREPARED_SCHEMA.to_string(),
            run_id: run_id.clone(),
            database_path: plan.database_path.clone(),
            plan_token: plan_token.clone(),
            marked_at: marked_at.clone(),
            forecast: forecast.clone(),
            raw_before: plan.raw_witness,
            logical_before: plan.logical_witness,
        };
        write_json_new(&run_dir.join("prepared.json"), &prepared).expect("write prepared receipt");

        // Build the reviewed generation and stop at the actual crash point:
        // the namespace exchange has completed, but none of the post-install
        // directory barriers or authority finalization has run.
        let candidate_path =
            maintenance_candidate_path(&migration.db_path, &run_dir).expect("candidate path");
        let source_conn = Connection::open(migration.db_path.to_string_lossy().into_owned())
            .expect("open source");
        let escaped_candidate = candidate_path.to_string_lossy().replace('\'', "''");
        source_conn
            .execute(&format!("VACUUM INTO '{escaped_candidate}'"))
            .expect("build candidate");
        close_connection(source_conn).expect("close source");
        let candidate_conn = Connection::open(candidate_path.to_string_lossy().into_owned())
            .expect("open candidate");
        candidate_conn
            .execute("BEGIN IMMEDIATE")
            .expect("begin migration");
        let reviewed_effects = run_reviewed_schema_migration_steps_in_transaction(
            &candidate_conn,
            forecast.from_version,
            forecast.to_version,
            &marked_at,
        )
        .expect("run reviewed migration");
        candidate_conn.execute("COMMIT").expect("commit candidate");
        candidate_conn
            .execute("REINDEX")
            .expect("reindex candidate");
        candidate_conn
            .execute("PRAGMA wal_checkpoint(TRUNCATE)")
            .expect("checkpoint candidate");
        close_connection(candidate_conn).expect("close candidate");
        let candidate_logical = logical_witness(&candidate_path).expect("candidate witness");
        let mut effects = ReviewedSchemaMigrationEffectsReceipt::from(reviewed_effects);
        effects.post_migration_maintenance_completed = true;
        persist_commit_ready_marker(&run_dir, &migration.db_path, &candidate_logical, effects)
            .expect("persist commit-ready marker");

        let marker: CommitReadyMigrationReceipt =
            read_json(&run_dir.join("commit-ready.json")).expect("read marker");
        let failed = FailedMigrationReceipt {
            schema_version: FAILED_SCHEMA.to_string(),
            run_id: marker.run_id.clone(),
            database_path: marker.database_path.clone(),
            plan_token: marker.plan_token.clone(),
            marked_at: marker.marked_at.clone(),
            error: "simulated process failure after namespace installation".to_string(),
            failed_stage: None,
            raw_before: marker.raw_before.clone(),
            logical_before: marker.logical_before.clone(),
            raw_observed_after_failure: None,
            logical_observed_after_failure: None,
        };
        write_json_new(&run_dir.join("failed.json"), &failed).expect("write failed receipt");

        let source_raw = raw_family_witness(&migration.db_path).expect("source raw");
        let candidate_raw = raw_family_witness(&candidate_path).expect("candidate raw");
        move_present_sidecars_new(
            &candidate_path,
            &run_dir.join("maintenance-candidate-sidecars"),
            &candidate_raw,
            "candidate-sidecar-retention",
        )
        .expect("retain candidate sidecars");
        let displaced_dir = run_dir.join("maintenance-displaced");
        ensure_new_directory(&displaced_dir).expect("create displaced directory");
        move_present_sidecars_new(
            &migration.db_path,
            &displaced_dir,
            &source_raw,
            "displace-live-sidecars",
        )
        .expect("retain source sidecars");
        let replacement_lock = migration
            .write_authority
            .lock_database_replacement_candidate(&candidate_path)
            .expect("lock candidate");
        exchange_database_paths(&candidate_path, &migration.db_path)
            .expect("exchange candidate and live main");
        migration
            .write_authority
            .verify_staged_database_recovery_authority(&candidate_path)
            .expect("prove displaced original authority");
        migration
            .write_authority
            .adopt_locked_database_replacement(replacement_lock)
            .expect("adopt installed candidate");
        let displaced_main =
            backup_component_path(&displaced_dir, &migration.db_path, "").expect("displaced main");
        rename_path_no_replace(&candidate_path, &displaced_main)
            .expect("retain original main without running barriers");

        assert!(run_dir.join("commit-ready.json").is_file());
        assert!(
            !run_dir.join("applied.json").exists(),
            "the fixture must stop at the crash boundary before applied receipt publication"
        );

        let beads_dir = migration.beads_dir.clone();
        let db_path = migration.db_path.clone();
        drop(migration);
        let migration = MigrationContext {
            write_authority: Arc::new(
                crate::sync::blocking_database_family_write_lock_with_timeout(
                    &beads_dir,
                    &db_path,
                    Some(1000),
                )
                .expect("reacquire authority after simulated crash"),
            ),
            beads_dir,
            db_path,
        };
        let (applied, resumed_before_dir) = resume_commit_ready_migration(
            &DoctorMigrateSchemaApplyArgs {
                plan_token,
                json: false,
            },
            &migration,
        )
        .expect("resume commit-ready migration")
        .expect("committed generation must materialize applied receipt");
        assert_eq!(resumed_before_dir, before_dir);
        assert!(applied.attested);
        assert!(run_dir.join("applied.json").is_file());
        assert!(
            run_dir.join("failed.json").is_file(),
            "a pre-existing failure receipt must not suppress promotion of the exact live after-state"
        );

        execute_undo(
            &DoctorMigrateSchemaUndoArgs {
                run_id,
                dry_run: false,
                json: false,
            },
            &migration,
        )
        .expect("normal undo after resumed receipt");
        assert_eq!(
            logical_witness(&migration.db_path).expect("restored original witness"),
            original_logical
        );
    }

    #[test]
    fn pre_install_commit_ready_marker_classifies_original_generation_for_retry() {
        let (_temp, migration) = reviewed_v14_migration_context();
        let plan = build_plan(&migration.db_path).expect("build migration plan");
        let forecast = plan.forecast.clone().expect("eligible forecast");
        let plan_token = plan.plan_token.clone().expect("plan token");
        let run_id = allocate_run_id(&migration.beads_dir).expect("allocate run");
        let run_dir = migration_runs_root(&migration.beads_dir).join(&run_id);
        let before_dir = run_dir.join("before");
        ensure_new_directory(&before_dir).expect("create before directory");
        copy_family_to_backup(&migration.db_path, &before_dir, &plan.raw_witness)
            .expect("copy recovery family");
        let marked_at = Utc::now().to_rfc3339();
        let prepared = PreparedMigrationReceipt {
            schema_version: PREPARED_SCHEMA.to_string(),
            run_id: run_id.clone(),
            database_path: plan.database_path,
            plan_token: plan_token.clone(),
            marked_at,
            forecast: forecast.clone(),
            raw_before: plan.raw_witness,
            logical_before: plan.logical_witness.clone(),
        };
        write_json_new(&run_dir.join("prepared.json"), &prepared).expect("write prepared receipt");
        let mut expected_after = plan.logical_witness;
        expected_after.user_version = forecast.to_version;
        expected_after.integrity_check = "ok".to_string();
        persist_commit_ready_marker(
            &run_dir,
            &migration.db_path,
            &expected_after,
            ReviewedSchemaMigrationEffectsReceipt {
                from_version: forecast.from_version,
                to_version: forecast.to_version,
                content_hash_rows_rebuilt: forecast.content_hash_rows_rebuilt,
                gate_result_history_created: forecast.gate_result_history_created,
                post_migration_maintenance_completed: true,
            },
        )
        .expect("persist pre-install commit-ready marker");

        let resumed = resume_commit_ready_migration(
            &DoctorMigrateSchemaApplyArgs {
                plan_token,
                json: false,
            },
            &migration,
        )
        .expect("classify interrupted pre-install intent");
        assert!(
            resumed.is_none(),
            "an unchanged original generation must be retried, never called applied"
        );
        assert!(run_dir.join("failed.json").is_file());
        assert!(!run_dir.join("applied.json").exists());
        assert_eq!(
            logical_witness(&migration.db_path).expect("unchanged original witness"),
            prepared.logical_before
        );
    }

    #[test]
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        windows
    ))]
    fn interrupted_preinstall_sidecar_move_restores_exact_receipt_family() {
        let (_temp, migration) = reviewed_v14_migration_context();
        let journal = family_component_path(&migration.db_path, "-journal");
        fs::write(&journal, b"receipt-bound journal bytes").expect("write journal fixture");
        let expected = raw_family_witness(&migration.db_path).expect("expected raw family");
        let run_dir = migration.beads_dir.join("preinstall-sidecar-crash");
        let before_dir = run_dir.join("before");
        let displaced_dir = run_dir.join("maintenance-displaced");
        ensure_new_directory(&before_dir).expect("create before dir");
        copy_family_to_backup(&migration.db_path, &before_dir, &expected)
            .expect("copy exact family");
        ensure_new_directory(&displaced_dir).expect("create displaced dir");
        let displaced_journal =
            backup_component_path(&displaced_dir, &migration.db_path, "-journal")
                .expect("displaced journal path");
        rename_path_no_replace(&journal, &displaced_journal)
            .expect("simulate sidecar move before main exchange");
        assert!(!journal.exists(), "fixture must expose the sidecar gap");

        let restored = restore_interrupted_preinstall_family(
            &migration.db_path,
            &before_dir,
            &displaced_dir,
            &expected,
            &migration.write_authority,
        )
        .expect("reconcile interrupted sidecar move");
        assert_eq!(restored, expected);
        assert_eq!(
            raw_family_witness(&migration.db_path).expect("live exact family"),
            expected
        );
        migration
            .write_authority
            .verify_database_authority()
            .expect("original main remains authoritative");
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
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
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
        let logical_live = logical_witness(&migration.db_path).expect("logical live state");
        let raw_live = raw_family_witness(&migration.db_path).expect("raw live state");
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

        let restored_candidate_dir = run_dir.join("undo-restored-candidate");
        ensure_directory(&restored_candidate_dir).expect("create restore candidate dir");
        let restored_candidate =
            backup_component_path(&restored_candidate_dir, &migration.db_path, "")
                .expect("restored candidate path");
        let before_main = backup_component_path(&run_dir.join("before"), &migration.db_path, "")
            .expect("before main");
        let restored_main = component_for_suffix(&applied.raw_before, "").expect("main witness");
        copy_regular_file_new(&before_main, &restored_candidate, restored_main.unix_mode)
            .expect("stage restored main");
        exchange_database_paths(&restored_candidate, &migration.db_path)
            .expect("simulate atomic undo exchange before quarantine retention");

        let beads_dir = migration.beads_dir.clone();
        let db_path = migration.db_path.clone();
        drop(migration);
        let migration = MigrationContext {
            write_authority: Arc::new(
                crate::sync::blocking_database_family_write_lock_with_timeout(
                    &beads_dir,
                    &db_path,
                    Some(1000),
                )
                .expect("reacquire authority after simulated undo crash"),
            ),
            beads_dir,
            db_path,
        };

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

    #[test]
    fn contention_retry_recovers_after_transient_sharing_conflicts() {
        let sleeps = std::cell::RefCell::new(Vec::new());
        let attempts = std::cell::Cell::new(0_u32);
        retry_through_file_contention(
            "candidate-sidecar-retention",
            Path::new("candidate.db-wal"),
            Path::new("retained.db-wal"),
            |delay| sleeps.borrow_mut().push(delay),
            |_error| true,
            || {
                let attempt = attempts.get();
                attempts.set(attempt + 1);
                if attempt < 2 {
                    Err(BeadsError::Io(std::io::Error::other(
                        "injected transient sharing conflict",
                    )))
                } else {
                    Ok(())
                }
            },
        )
        .expect("the attempt after the transient conflicts clear must succeed");
        assert_eq!(attempts.get(), 3, "two failures then one success");
        assert_eq!(
            sleeps.borrow().as_slice(),
            &[
                FILE_CONTENTION_RETRY_STEP,
                FILE_CONTENTION_RETRY_STEP.saturating_mul(2),
            ],
            "backoff must grow linearly with the attempt number"
        );
    }

    #[test]
    fn contention_retry_exhaustion_names_stage_and_both_paths() {
        let sleeps = std::cell::Cell::new(0_u32);
        let attempts = std::cell::Cell::new(0_u32);
        let error = retry_through_file_contention(
            "install-rename-candidate-to-live",
            Path::new("pinned-source.db"),
            Path::new("blocked-destination.db"),
            |_delay| sleeps.set(sleeps.get() + 1),
            |_error| true,
            || {
                attempts.set(attempts.get() + 1);
                Err(BeadsError::Io(std::io::Error::other(
                    "injected persistent sharing conflict",
                )))
            },
        )
        .expect_err("a persistent sharing conflict must exhaust the retry budget");
        assert_eq!(attempts.get(), FILE_CONTENTION_RETRY_ATTEMPTS);
        assert_eq!(
            sleeps.get(),
            FILE_CONTENTION_RETRY_ATTEMPTS - 1,
            "no sleep may follow the final attempt"
        );
        let message = error.to_string();
        for needle in [
            "install-rename-candidate-to-live",
            "pinned-source.db",
            "blocked-destination.db",
            "injected persistent sharing conflict",
        ] {
            assert!(
                message.contains(needle),
                "exhaustion error must name {needle}: {message}"
            );
        }
    }

    #[test]
    fn non_contention_rename_errors_pass_through_without_retry() {
        let attempts = std::cell::Cell::new(0_u32);
        let error = retry_through_file_contention(
            "displace-live-sidecars",
            Path::new("source.db-journal"),
            Path::new("destination.db-journal"),
            |_delay| panic!("a non-contention error must never sleep"),
            // The real classifier: non-Io errors are never contention, and on
            // non-Windows platforms nothing is, so behavior stays byte-identical.
            is_windows_file_contention,
            || {
                attempts.set(attempts.get() + 1);
                Err(BeadsError::internal("hard rename failure"))
            },
        )
        .expect_err("a non-contention error must surface unchanged");
        assert_eq!(attempts.get(), 1, "no retry may follow a hard failure");
        assert!(
            error.to_string().contains("hard rename failure"),
            "the causal error must pass through unchanged: {error}"
        );
    }

    #[test]
    fn failed_receipt_stage_marker_is_additive_and_optional() {
        let witness = LogicalDatabaseWitness {
            user_version: 14,
            integrity_check: "ok".to_string(),
            schema_sha256: String::new(),
            contents_sha256: String::new(),
            tables: Vec::new(),
        };
        let mut receipt = FailedMigrationReceipt {
            schema_version: FAILED_SCHEMA.to_string(),
            run_id: "run".to_string(),
            database_path: "beads.db".to_string(),
            plan_token: "token".to_string(),
            marked_at: "2026-08-25T00:00:00Z".to_string(),
            error: "boom".to_string(),
            failed_stage: None,
            raw_before: RawFamilyWitness {
                components: Vec::new(),
            },
            logical_before: witness,
            raw_observed_after_failure: None,
            logical_observed_after_failure: None,
        };
        let without_stage = serde_json::to_string(&receipt).expect("serialize stage-less receipt");
        assert!(
            !without_stage.contains("failed_stage"),
            "an absent stage must not change the receipt encoding: {without_stage}"
        );
        let decoded: FailedMigrationReceipt =
            serde_json::from_str(&without_stage).expect("pre-marker receipts must deserialize");
        assert_eq!(decoded.failed_stage, None);

        receipt.failed_stage = Some("candidate-sidecar-retention".to_string());
        let with_stage = serde_json::to_string(&receipt).expect("serialize staged receipt");
        assert!(
            with_stage.contains("\"failed_stage\":\"candidate-sidecar-retention\""),
            "a recorded stage must round-trip: {with_stage}"
        );
    }

    #[test]
    fn maintenance_failure_records_the_stage_that_was_in_progress() {
        let (_temp, migration) = reviewed_v14_migration_context();
        let plan = build_plan(&migration.db_path).expect("build plan");
        let forecast = plan.forecast.clone().expect("eligible forecast");
        let run_id = allocate_run_id(&migration.beads_dir).expect("allocate run");
        let run_dir = migration_runs_root(&migration.beads_dir).join(&run_id);
        let marked_at = Utc::now().to_rfc3339();
        let prepared = PreparedMigrationReceipt {
            schema_version: PREPARED_SCHEMA.to_string(),
            run_id: run_id.clone(),
            database_path: plan.database_path,
            plan_token: plan.plan_token.expect("plan token"),
            marked_at: marked_at.clone(),
            forecast: forecast.clone(),
            raw_before: plan.raw_witness,
            logical_before: plan.logical_witness,
        };
        write_json_new(&run_dir.join("prepared.json"), &prepared).expect("write prepared receipt");
        // Occupy the marker path so the pipeline fails exactly when it tries
        // to persist the commit-ready marker: the candidate is fully built and
        // attested, and no earlier stage has any reason to fail.
        fs::write(run_dir.join("commit-ready.json"), b"{}").expect("occupy marker path");

        let mut failed_stage = None;
        run_post_migration_maintenance(
            &migration.db_path,
            forecast.from_version,
            forecast.to_version,
            &marked_at,
            &run_dir,
            &migration.write_authority,
            &mut failed_stage,
        )
        .expect_err("an occupied commit-ready marker path must fail the pipeline");
        assert_eq!(
            failed_stage.as_deref(),
            Some("commit-ready-marker"),
            "the marker must name the stage that was in progress"
        );
    }

    #[cfg(unix)]
    #[test]
    fn forced_apply_failure_records_failed_stage_in_failure_receipt() {
        // The live database name itself fits within the usual Unix NAME_MAX,
        // while the run-id-bearing VACUUM candidate name does not.  This
        // forces the candidate stage to fail even for root, unlike directory
        // mode-bit tests whose write restriction root legitimately bypasses.
        let database_name = format!("{}.db", "b".repeat(226));
        let (_temp, migration) = reviewed_v14_migration_context_with_database_name(&database_name);
        let plan = build_plan(&migration.db_path).expect("build plan");
        let runs_root = migration_runs_root(&migration.beads_dir);
        let result = execute_apply(
            &DoctorMigrateSchemaApplyArgs {
                plan_token: plan.plan_token.expect("plan token"),
                json: false,
            },
            &migration,
        );
        result.expect_err("an overlong VACUUM candidate name must fail the apply");

        let run_dir = fs::read_dir(&runs_root)
            .expect("read migration runs")
            .next()
            .expect("one run")
            .expect("run entry")
            .path();
        let failed: FailedMigrationReceipt =
            read_json(&run_dir.join("failed.json")).expect("read failure receipt");
        assert_eq!(
            failed.failed_stage.as_deref(),
            Some("vacuum-candidate"),
            "the receipt must pinpoint the stage that could not proceed"
        );
        assert!(
            !run_dir.join("maintenance-candidate-sidecars").exists(),
            "the failure precedes candidate sidecar retention"
        );
    }
}
