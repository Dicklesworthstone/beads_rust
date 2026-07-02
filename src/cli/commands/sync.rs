//! Sync command implementation.
//!
//! Provides explicit JSONL sync actions without git operations.
//! Supports `--flush-only` (export) and `--import-only` (import).

use crate::cli::{DEFAULT_WITNESS_PARALLELISM, SyncArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::health::{AnomalyClass, ReliabilityAuditRecord, WorkspaceClassification};
use crate::output::OutputContext;
use crate::sync::history::HistoryConfig;
use crate::sync::witness::{
    JsonlMerkleWitness, JsonlWitnessComparison, JsonlWitnessParallelWorkPlan,
    JsonlWitnessReuseMaterialization, JsonlWitnessReusePlan, build_jsonl_merkle_witness_parallel,
    compare_jsonl_merkle_witnesses, materialize_jsonl_witness_reuse_plan,
    plan_jsonl_witness_parallel_work, plan_jsonl_witness_reuse,
};
use crate::sync::{
    ConflictResolution, ExportConfig, ExportEntityType, ExportError, ExportErrorPolicy,
    ImportConfig, METADATA_JSONL_CONTENT_HASH, METADATA_LAST_EXPORT_TIME,
    METADATA_LAST_IMPORT_TIME, MergeContext, OrphanMode, analyze_jsonl, compute_jsonl_hash,
    compute_staleness, export_temp_path, export_to_jsonl_with_policy, finalize_export,
    get_issue_ids_from_jsonl, id_matches_expected_prefix, import_from_jsonl, load_base_snapshot,
    read_issues_from_jsonl, require_safe_sync_overwrite_path, require_valid_sync_path,
    restore_tombstones_after_rebuild, save_base_snapshot_from_jsonl,
    scan_jsonl_for_tombstone_filter, snapshot_tombstones, three_way_merge,
    tombstones_missing_from_jsonl_tombstones, validate_no_git_path,
    validate_sync_path_with_external,
};
use crate::util::id::split_prefix_remainder;
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, IsTerminal};
use std::path::{Component, Path, PathBuf};
use tracing::{debug, info, warn};

/// Result of a flush (export) operation.
#[derive(Debug, Serialize)]
pub struct FlushResult {
    pub exported_issues: usize,
    pub exported_dependencies: usize,
    pub exported_labels: usize,
    pub exported_comments: usize,
    pub content_hash: String,
    pub cleared_dirty: usize,
    pub policy: ExportErrorPolicy,
    pub success_rate: f64,
    pub errors: Vec<ExportError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_path: Option<String>,
}

/// Result of an import operation.
#[derive(Debug, Serialize)]
pub struct ImportResultOutput {
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
    pub tombstone_skipped: usize,
    pub orphans_removed: usize,
    pub blocked_cache_rebuilt: bool,
}

/// Sync status information.
#[derive(Debug, Serialize)]
pub struct SyncStatus {
    pub dirty_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_export_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_import_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonl_content_hash: Option<String>,
    pub jsonl_exists: bool,
    pub jsonl_newer: bool,
    pub db_newer: bool,
    /// Workspace health classification (`healthy` / `degraded` /
    /// `recoverable` / `unsafe`) computed from the cheap signals this
    /// command already evaluates: file-state probes plus the
    /// DB↔JSONL drift booleans above. Same write-gate vocabulary as
    /// `br doctor --json` (beads_rust#334; docs/reliability/HEALTH_CONTRACT.md).
    pub workspace_health: String,
    /// Anomaly evidence backing `workspace_health`, in the same shape
    /// doctor emits (`anomalies[].code` / `severity` / `message`).
    pub reliability_audit: ReliabilityAuditRecord,
    /// Read-only git visibility for the canonical JSONL export
    /// (beads_rust#338). `{available:false}` when git or a repo is
    /// absent; never fails the command.
    pub git_export: GitExportStatus,
}

/// Read-only git visibility for the JSONL export (beads_rust#338).
///
/// `br sync --status` reports DB↔JSONL drift, but a dirty *tracked*
/// `issues.jsonl` was previously invisible: the DB and the worktree
/// file can agree while the committed copy lags behind. This block
/// surfaces exactly what git knows about the export file. All probes
/// are read-only (`git status --porcelain`, `git rev-parse`,
/// `git ls-files`, `git hash-object`); any git error — git missing,
/// not a repo, spawn failure — degrades to `{ "available": false }`
/// rather than failing the status command.
#[derive(Debug, Serialize)]
pub struct GitExportStatus {
    /// True when the JSONL sits inside a git work tree and the
    /// read-only probes answered.
    pub available: bool,
    /// True when the JSONL is tracked in the git index
    /// (`git ls-files --error-unmatch`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracked: Option<bool>,
    /// True when the worktree copy matches the index (untracked files
    /// report `false`: their content is invisible to git entirely).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_clean: Option<bool>,
    /// True when the index matches HEAD for this path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_clean: Option<bool>,
    /// Blob hash of the committed copy (`git rev-parse HEAD:<relpath>`);
    /// `None` when the file is absent from HEAD (or no commits exist).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_hash: Option<String>,
    /// Blob hash of the on-disk copy (`git hash-object <jsonl>`);
    /// `None` when the file is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_hash: Option<String>,
}

impl GitExportStatus {
    fn unavailable() -> Self {
        Self {
            available: false,
            tracked: None,
            worktree_clean: None,
            index_clean: None,
            head_hash: None,
            worktree_hash: None,
        }
    }
}

/// JSONL witness command output.
#[derive(Debug, Serialize)]
pub struct SyncWitnessResult {
    pub jsonl_path: String,
    pub witness: JsonlMerkleWitness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_jsonl_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_comparison: Option<JsonlWitnessComparison>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_reuse_plan: Option<JsonlWitnessReusePlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_parallel_work_plan: Option<JsonlWitnessParallelWorkPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_reuse_materialization: Option<JsonlWitnessReuseMaterialization>,
}

struct BaseWitnessArtifacts {
    jsonl_path: Option<String>,
    comparison: Option<JsonlWitnessComparison>,
    reuse_plan: Option<JsonlWitnessReusePlan>,
    parallel_work_plan: Option<JsonlWitnessParallelWorkPlan>,
    reuse_materialization: Option<JsonlWitnessReuseMaterialization>,
}

#[derive(Debug)]
#[allow(dead_code)] // Fields may be used in future sync enhancements
struct SyncPathPolicy {
    jsonl_path: PathBuf,
    jsonl_temp_path: PathBuf,
    manifest_path: PathBuf,
    beads_dir: PathBuf,
    is_external: bool,
    allow_external_jsonl: bool,
}

struct SyncStartupState {
    beads_dir: PathBuf,
    path_policy: SyncPathPolicy,
    open_result: config::OpenStorageResult,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncOperation {
    Status,
    Witness,
    Flush,
    Merge,
    Import,
    Unspecified,
}

struct SyncDispatchOptions {
    db_path: PathBuf,
    retention_days: Option<u64>,
    use_json: bool,
    show_progress: bool,
    history_config: HistoryConfig,
}

/// Execute the sync command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or the sync operation fails.
pub fn execute(
    args: &SyncArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    startup_write_lock_held: bool,
) -> Result<()> {
    validate_sync_mode_args(args)?;

    if args.witness {
        let (_, _, path_policy) = resolve_sync_startup_paths(args, cli)?;
        return execute_witness(&path_policy, args, ctx.is_json() || args.robot, ctx);
    }

    let mut startup = prepare_sync_startup(args, cli, startup_write_lock_held)?;

    maybe_delegate_rebuild(args, &mut startup.open_result)?;

    let command_result = dispatch_sync_subcommand(
        args,
        cli,
        ctx,
        &startup.beads_dir,
        &startup.path_policy,
        &mut startup.open_result,
    );

    finalize_sync_result(command_result, &mut startup.open_result)
}

/// Resolve path policy and open storage before dispatch. Keeping this separate
/// from `execute` makes the command's startup phase distinct from the
/// status/export/import/merge operation handlers below.
fn prepare_sync_startup(
    args: &SyncArgs,
    cli: &config::CliOverrides,
    startup_write_lock_held: bool,
) -> Result<SyncStartupState> {
    let (beads_dir, startup, path_policy) = resolve_sync_startup_paths(args, cli)?;
    let allow_external_jsonl = path_policy.allow_external_jsonl;

    let open_result = if startup_write_lock_held {
        config::open_storage_with_startup_config_under_write_lock_and_jsonl_policy(
            startup,
            cli,
            should_defer_jsonl_recovery(args),
            allow_external_jsonl,
        )?
    } else {
        config::open_storage_with_startup_config_and_jsonl_policy(
            startup,
            cli,
            should_defer_jsonl_recovery(args),
            allow_external_jsonl,
        )?
    };

    Ok(SyncStartupState {
        beads_dir,
        path_policy,
        open_result,
    })
}

fn resolve_sync_startup_paths(
    args: &SyncArgs,
    cli: &config::CliOverrides,
) -> Result<(PathBuf, config::StartupConfig, SyncPathPolicy)> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let startup = config::load_startup_config_with_paths(&beads_dir, cli.db.as_ref())?;
    let allow_external_jsonl = args.allow_external_jsonl
        || config::implicit_external_jsonl_allowed(
            &startup.paths.beads_dir,
            &startup.paths.db_path,
            &startup.paths.jsonl_path,
        );
    let path_policy =
        validate_sync_paths(&beads_dir, &startup.paths.jsonl_path, allow_external_jsonl)?;
    debug!(
        jsonl_path = %path_policy.jsonl_path.display(),
        manifest_path = %path_policy.manifest_path.display(),
        external_jsonl = path_policy.is_external,
        allow_external_jsonl = path_policy.allow_external_jsonl,
        "Resolved sync path policy"
    );

    Ok((beads_dir, startup, path_policy))
}

/// For `--rename-prefix` imports, defer any implicit JSONL recovery until the
/// explicit import path below so the command's import semantics (ID rewrites and
/// duplicate external_ref cleanup) are applied in the same invocation instead
/// of being skipped by open-time recovery.
fn should_defer_jsonl_recovery(args: &SyncArgs) -> bool {
    args.import_only && args.rename_prefix
}

/// Reject argument combinations that must fail BEFORE opening storage or
/// triggering any rebuild side effect. A `--flush-only --rebuild` or
/// `--merge --rebuild` combination must return an error without having
/// touched the DB family — otherwise the validation message arrives after
/// `recover_database_from_jsonl` has already moved the existing DB aside.
fn validate_sync_mode_args(args: &SyncArgs) -> Result<()> {
    let mode_count = u8::from(args.status)
        + u8::from(args.flush_only)
        + u8::from(args.import_only)
        + u8::from(args.merge)
        + u8::from(args.witness);
    if mode_count > 1 {
        return Err(BeadsError::Validation {
            field: "mode".to_string(),
            reason:
                "Must specify exactly one of --flush-only, --import-only, --merge, --status, or --witness"
                    .to_string(),
        });
    }
    if mode_count == 0 {
        return Err(BeadsError::Validation {
            field: "mode".to_string(),
            reason:
                "Must specify one of --flush-only, --import-only, --merge, --status, or --witness"
                    .to_string(),
        });
    }

    if args.witness && args.witness_chunk_lines == 0 {
        return Err(BeadsError::Validation {
            field: "witness_chunk_lines".to_string(),
            reason: "--witness-chunk-lines must be greater than zero".to_string(),
        });
    }

    if args.witness_parallelism == Some(0) {
        return Err(BeadsError::Validation {
            field: "witness_parallelism".to_string(),
            reason: "--witness-parallelism must be greater than zero".to_string(),
        });
    }
    if args.export_parallelism == Some(0) {
        return Err(BeadsError::Validation {
            field: "export_parallelism".to_string(),
            reason: "--export-parallelism must be greater than zero".to_string(),
        });
    }

    // --rebuild only makes sense with explicit import mode.
    if args.rebuild && !args.import_only {
        return Err(BeadsError::Validation {
            field: "rebuild".to_string(),
            reason: "--rebuild can only be used with --import-only".to_string(),
        });
    }

    if (args.force_db || args.force_jsonl) && !args.merge {
        return Err(BeadsError::Validation {
            field: "merge-resolution".to_string(),
            reason: "--force-db and --force-jsonl can only be used with --merge".to_string(),
        });
    }

    if args.force_db && args.force_jsonl {
        return Err(BeadsError::Validation {
            field: "merge-resolution".to_string(),
            reason: "--force-db conflicts with --force-jsonl; choose one merge winner".to_string(),
        });
    }

    if args.force && (args.force_db || args.force_jsonl) {
        return Err(BeadsError::Validation {
            field: "force".to_string(),
            reason: "--force conflicts with --force-db and --force-jsonl for --merge; choose one conflict resolution policy".to_string(),
        });
    }
    Ok(())
}

fn merge_conflict_resolution(args: &SyncArgs) -> ConflictResolution {
    if args.force_db {
        ConflictResolution::PreferLocal
    } else if args.force_jsonl {
        ConflictResolution::PreferExternal
    } else if args.force {
        ConflictResolution::PreferNewer
    } else {
        ConflictResolution::Manual
    }
}

fn merge_conflict_resolution_label(strategy: ConflictResolution) -> &'static str {
    match strategy {
        ConflictResolution::PreferLocal => "force-db",
        ConflictResolution::PreferExternal => "force-jsonl",
        ConflictResolution::PreferNewer => "force-newer",
        ConflictResolution::Manual => "manual",
    }
}

/// When `--rebuild` is requested against an existing (non-auto-rebuilt)
/// DB, delegate the actual rebuild to the same proven path that auto-
/// recovery uses: backup the DB family, open a fresh connection, import
/// JSONL, checkpoint, VACUUM/REINDEX. The in-place
/// `reset_data_tables`+`import_from_jsonl` code path inside
/// `execute_import` is fragile on fsqlite — it trips stale-pager/MVCC
/// bugs that leave "never used" pages and partial-index mismatches that
/// VACUUM can't always reclaim. Using `recover_database_from_jsonl`
/// sidesteps all of that, and `execute_import` then sees
/// `auto_rebuilt == true` and short-circuits.
///
/// Only fire this for the request that will actually go through
/// `execute_import`: `--rebuild --import-only`. All other mode pairings
/// are rejected before storage opens. Also require the JSONL to exist —
/// `recover_database_from_jsonl` runs a preflight that fails hard if the
/// file is missing, whereas `execute_import` already handles a missing
/// JSONL gracefully, so leave that case to the normal path.
///
/// Skip the delegation when the caller asked for behavior that the
/// auto-recovery path does not replicate: `--rename-prefix` rewrites
/// imported IDs into the configured prefix, while
/// `repair_database_from_jsonl` always runs with
/// `rename_on_import = false`. That means the delegation would silently
/// skip the requested rename behavior.
///
/// `--orphans` is intentionally *not* part of this guard today. The
/// current import engine parses `orphan_mode` into `ImportConfig`, but it
/// does not consult that field during import, so delegating does not
/// change effective behavior. If orphan-mode semantics become active in
/// the future, revisit this guard and the auto-rebuild conflict
/// detection below.
fn maybe_delegate_rebuild(
    args: &SyncArgs,
    open_result: &mut config::OpenStorageResult,
) -> Result<()> {
    let delegation_would_drop_user_flags = args.rename_prefix;
    let should_delegate = args.rebuild
        && args.import_only
        && !open_result.no_db
        && !open_result.auto_rebuilt
        && open_result.paths.jsonl_path.is_file()
        && !delegation_would_drop_user_flags;
    if !should_delegate {
        return Ok(());
    }

    info!(
        db_path = %open_result.paths.db_path.display(),
        jsonl_path = %open_result.paths.jsonl_path.display(),
        "--rebuild requested on existing DB: delegating to auto-recovery rebuild path"
    );
    // Snapshot tombstones before the delegation wipes the DB. The
    // in-place rebuild path inside `execute_import` preserves deletion-
    // retention state across `reset_data_tables` via
    // `snapshot_tombstones` + `restore_tombstones`; the auto-recovery
    // path opens a fresh DB and only imports what's in the JSONL, so
    // any tombstones that were in the old DB but not yet flushed would
    // be silently lost. Grab them here, restore them after the
    // delegated rebuild completes.
    //
    // `scan_jsonl_for_tombstone_filter` parses the JSONL, and that
    // parse fails with a generic "Invalid JSON at line 1" when the
    // file contains merge-conflict markers. Scan for markers first so
    // the operator gets the conflict-markers error class that
    // `recover_database_from_jsonl`'s preflight would have surfaced
    // otherwise.
    crate::sync::ensure_no_conflict_markers(&open_result.paths.jsonl_path)?;
    let jsonl_filter = scan_jsonl_for_tombstone_filter(&open_result.paths.jsonl_path)?;
    let preserved_pre_delegation_tombstones = tombstones_missing_from_jsonl_tombstones(
        snapshot_tombstones(&open_result.storage),
        &jsonl_filter,
    );
    // `recover_database_from_jsonl` sets `auto_rebuilt = true` on success,
    // which is what gates the short-circuit inside `execute_import` below.
    open_result.recover_database_from_jsonl()?;
    let restore_count = preserved_pre_delegation_tombstones.len();
    restore_tombstones_after_rebuild(
        &mut open_result.storage,
        &preserved_pre_delegation_tombstones,
    )?;
    if restore_count > 0 {
        debug!(
            count = restore_count,
            "Restored tombstones across delegated auto-recovery rebuild"
        );
    }
    Ok(())
}

/// Dispatch to the appropriate sync-subcommand implementation based on
/// the flag pattern (`--status` / `--flush-only` / `--merge` /
/// `--import-only`). The status branch is read-only; the
/// other three hold a `&mut` borrow on `open_result.storage` for the
/// duration of their execution. Any `Err` propagates back to
/// `finalize_sync_result`, which is the single place that decides how to
/// handle recovery-backup rollback.
fn dispatch_sync_subcommand(
    args: &SyncArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
    path_policy: &SyncPathPolicy,
    open_result: &mut config::OpenStorageResult,
) -> Result<()> {
    let options = sync_dispatch_options(args, cli, ctx, open_result);

    match sync_operation(args) {
        SyncOperation::Status => execute_status(
            &open_result.storage,
            path_policy,
            &options.db_path,
            options.use_json,
            ctx,
        ),
        SyncOperation::Witness => execute_witness(path_policy, args, options.use_json, ctx),
        SyncOperation::Flush => execute_flush(
            &mut open_result.storage,
            beads_dir,
            path_policy,
            args,
            options.use_json,
            options.show_progress,
            options.retention_days,
            options.history_config.clone(),
            ctx,
        ),
        SyncOperation::Merge => execute_merge(
            &mut open_result.storage,
            path_policy,
            args,
            options.use_json,
            options.show_progress,
            options.retention_days,
            options.history_config.clone(),
            cli,
            ctx,
        ),
        SyncOperation::Import => execute_import(
            &mut open_result.storage,
            beads_dir,
            cli,
            path_policy,
            args,
            options.use_json,
            options.show_progress,
            open_result.auto_rebuilt,
            &options.db_path,
            ctx,
        ),
        SyncOperation::Unspecified => Err(BeadsError::Validation {
            field: "mode".to_string(),
            reason:
                "Must specify one of --flush-only, --import-only, --merge, --status, or --witness"
                    .to_string(),
        }),
    }
}

fn sync_dispatch_options(
    args: &SyncArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    open_result: &config::OpenStorageResult,
) -> SyncDispatchOptions {
    let use_json = ctx.is_json() || args.robot;
    let quiet = cli.quiet.unwrap_or(false);
    SyncDispatchOptions {
        db_path: open_result.paths.db_path.clone(),
        retention_days: open_result.paths.metadata.deletions_retention_days,
        use_json,
        show_progress: should_show_progress(use_json, quiet),
        history_config: open_result.resolved_history_config(),
    }
}

fn sync_operation(args: &SyncArgs) -> SyncOperation {
    if args.witness {
        SyncOperation::Witness
    } else if args.status {
        SyncOperation::Status
    } else if args.flush_only {
        SyncOperation::Flush
    } else if args.merge {
        SyncOperation::Merge
    } else if args.import_only {
        SyncOperation::Import
    } else {
        SyncOperation::Unspecified
    }
}

/// Fold the subcommand result into the final command outcome, restoring
/// the pre-recovery backup on error (deferred-recovery paths only) and
/// discarding it on success.
fn finalize_sync_result(
    command_result: Result<()>,
    open_result: &mut config::OpenStorageResult,
) -> Result<()> {
    match command_result {
        Ok(()) => {
            open_result.discard_pending_recovery_backup();
            Ok(())
        }
        Err(command_err) => {
            let recovery_dir = open_result.pending_recovery_dir().map(PathBuf::from);
            if let Err(restore_err) = open_result.restore_pending_recovery_backup() {
                let context = recovery_dir.map_or_else(
                    || {
                        format!(
                            "sync command failed after deferred database recovery ({command_err}); original database restore also failed"
                        )
                    },
                    |dir| {
                        format!(
                            "sync command failed after deferred database recovery ({command_err}); original database restore from '{}' also failed",
                            dir.display()
                        )
                    },
                );
                return Err(BeadsError::WithContext {
                    context,
                    source: Box::new(restore_err),
                });
            }
            Err(command_err)
        }
    }
}

fn should_render_human_sync_output(ctx: &OutputContext, use_json: bool) -> bool {
    // Keep JSON/robot output paths alive even when quiet suppresses human text.
    !ctx.is_quiet() || use_json
}

fn validate_sync_paths(
    beads_dir: &Path,
    jsonl_path: &Path,
    allow_external_jsonl: bool,
) -> Result<SyncPathPolicy> {
    debug!(
        beads_dir = %beads_dir.display(),
        jsonl_path = %jsonl_path.display(),
        allow_external_jsonl,
        "Validating sync paths"
    );
    validate_operator_requested_sync_path(beads_dir, jsonl_path)?;

    let canonical_beads = dunce::canonicalize(beads_dir).map_err(|e| {
        BeadsError::Config(format!(
            "Failed to resolve .beads directory {}: {e}",
            beads_dir.display()
        ))
    })?;

    // Resolve the requested path to an absolute operator-facing location without
    // collapsing the final component. Raw-path validation must inspect the
    // actual path the operator asked sync to touch so symlink and `.git`
    // invariants cannot be bypassed by early canonicalization.
    let jsonl_path = resolve_requested_sync_path(jsonl_path)?;

    let extension = jsonl_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    if extension.as_deref() != Some("jsonl") {
        return Err(BeadsError::Config(format!(
            "JSONL path must end with .jsonl: {}",
            jsonl_path.display()
        )));
    }

    let is_external = !jsonl_path.starts_with(&canonical_beads);
    if is_external && !allow_external_jsonl {
        warn!(
            path = %jsonl_path.display(),
            "Rejected JSONL path outside .beads"
        );
        return Err(BeadsError::Config(format!(
            "Refusing to use JSONL path outside .beads: {}.\n\
             Hint: pass --allow-external-jsonl if this is intentional.",
            jsonl_path.display()
        )));
    }

    let manifest_path = canonical_beads.join(".manifest.json");
    let jsonl_temp_path = export_temp_path(&jsonl_path);

    if contains_git_dir(&jsonl_path) {
        warn!(
            path = %jsonl_path.display(),
            "Rejected JSONL path inside .git directory"
        );
        return Err(BeadsError::Config(format!(
            "Refusing to use JSONL path inside .git directory: {}.\n\
            Move the JSONL path outside .git to proceed.",
            jsonl_path.display()
        )));
    }

    validate_sync_path_with_external(&jsonl_path, &canonical_beads, allow_external_jsonl)?;

    debug!(
        jsonl_path = %jsonl_path.display(),
        jsonl_temp_path = %jsonl_temp_path.display(),
        manifest_path = %manifest_path.display(),
        is_external,
        "Sync path validation complete"
    );

    Ok(SyncPathPolicy {
        jsonl_path,
        jsonl_temp_path,
        manifest_path,
        beads_dir: canonical_beads,
        is_external,
        allow_external_jsonl,
    })
}

fn validate_operator_requested_sync_path(beads_dir: &Path, jsonl_path: &Path) -> Result<()> {
    let git_check = validate_no_git_path(jsonl_path);
    if !git_check.is_allowed() {
        return Err(BeadsError::Config(
            git_check
                .rejection_reason()
                .unwrap_or_else(|| "Git path access denied".to_string()),
        ));
    }

    let canonical_beads = dunce::canonicalize(beads_dir).map_err(|e| {
        BeadsError::Config(format!(
            "Failed to resolve .beads directory {}: {e}",
            beads_dir.display()
        ))
    })?;

    let operator_path = if jsonl_path.is_absolute() {
        jsonl_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(jsonl_path))
            .map_err(|e| {
                BeadsError::Config(format!(
                    "Failed to resolve current directory for JSONL path {}: {e}",
                    jsonl_path.display()
                ))
            })?
    };

    if !operator_path.starts_with(beads_dir) && !operator_path.starts_with(&canonical_beads) {
        return Ok(());
    }

    let mut candidate = PathBuf::new();
    for component in operator_path.components() {
        candidate.push(component.as_os_str());
        let Ok(metadata) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if !metadata.file_type().is_symlink() {
            continue;
        }

        let target = fs::read_link(&candidate).map_err(|e| {
            BeadsError::Config(format!(
                "Failed to inspect symlinked JSONL path component {}: {e}",
                candidate.display()
            ))
        })?;
        let absolute_target = if target.is_absolute() {
            target
        } else {
            candidate
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(target)
        };
        let canonical_target =
            dunce::canonicalize(&absolute_target).unwrap_or_else(|_| absolute_target.clone());
        if !canonical_target.starts_with(&canonical_beads) {
            return Err(BeadsError::Config(format!(
                "Refusing to use JSONL path through symlink escaping .beads: {} -> {}",
                candidate.display(),
                canonical_target.display()
            )));
        }
    }

    Ok(())
}

fn resolve_requested_sync_path(jsonl_path: &Path) -> Result<PathBuf> {
    if jsonl_path.is_absolute() {
        return Ok(jsonl_path.to_path_buf());
    }

    let file_name = jsonl_path
        .file_name()
        .ok_or_else(|| BeadsError::Config("JSONL path must include a filename".to_string()))?;
    let jsonl_parent = jsonl_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    Ok(resolve_sync_parent_path(jsonl_parent)?.join(file_name))
}

fn resolve_sync_parent_path(jsonl_parent: &Path) -> Result<PathBuf> {
    if jsonl_parent.exists() {
        return dunce::canonicalize(jsonl_parent).map_err(|e| {
            BeadsError::Config(format!(
                "JSONL directory is not accessible: {} ({e})",
                jsonl_parent.display()
            ))
        });
    }

    if jsonl_parent.is_absolute() {
        return Ok(jsonl_parent.to_path_buf());
    }

    let cwd = std::env::current_dir().map_err(|e| {
        BeadsError::Config(format!(
            "Failed to resolve current directory for JSONL path {}: {e}",
            jsonl_parent.display()
        ))
    })?;
    Ok(cwd.join(jsonl_parent))
}

fn contains_git_dir(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name == ".git",
        _ => false,
    })
}

/// Classify workspace health from the cheap signals available in
/// sync-status context (beads_rust#334): the file-state probes shared
/// with doctor (`classify_file_state`: DB header, sidecars, conflict
/// markers, orphaned locks) plus the DB↔JSONL drift booleans the
/// command already computed. This intentionally does NOT run the full
/// doctor checklist — only anomaly codes actually evaluated here are
/// emitted, so the audit record stays honest.
fn classify_sync_status_workspace(
    db_path: &Path,
    jsonl_path: &Path,
    jsonl_newer: bool,
    db_newer: bool,
) -> WorkspaceClassification {
    let mut anomalies = crate::health::classify_file_state(db_path, jsonl_path);
    if jsonl_newer {
        anomalies.push(AnomalyClass::JsonlNewer);
    }
    if db_newer {
        anomalies.push(AnomalyClass::DbNewer);
    }
    WorkspaceClassification::from_anomalies(anomalies)
}

/// Run a read-only git command in `dir`, returning trimmed stdout on
/// success and `None` on any failure (git absent, not a repo, non-zero
/// exit, non-UTF-8 paths). Mirrors the read-only probing pattern in
/// `changelog.rs` / `orphans.rs` / `stats.rs` — `br` never performs
/// git mutations.
fn run_git_capture(dir: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Interpret `git status --porcelain -- <path>` output for a single
/// path as `(index_clean, worktree_clean)`.
///
/// - empty output → both clean (or the path is unknown to git; the
///   separate `tracked` probe disambiguates that),
/// - `??` (untracked) → index clean (nothing staged), worktree dirty
///   (the content is invisible to git),
/// - otherwise the two porcelain status columns map directly: index
///   clean when `X == ' '`, worktree clean when `Y == ' '`.
fn porcelain_cleanliness(porcelain: &str) -> (bool, bool) {
    let Some(line) = porcelain.lines().next() else {
        return (true, true);
    };
    let mut chars = line.chars();
    let x = chars.next().unwrap_or(' ');
    let y = chars.next().unwrap_or(' ');
    if x == '?' && y == '?' {
        return (true, false);
    }
    (x == ' ', y == ' ')
}

/// Compute the read-only `git_export` block for `br sync --status`
/// (beads_rust#338). Never returns an error: any git failure collapses
/// to `GitExportStatus::unavailable()`.
fn git_export_status(jsonl_path: &Path) -> GitExportStatus {
    let Some(parent) = jsonl_path.parent().filter(|dir| dir.is_dir()) else {
        return GitExportStatus::unavailable();
    };
    let Some(file_name) = jsonl_path.file_name().and_then(std::ffi::OsStr::to_str) else {
        return GitExportStatus::unavailable();
    };

    if run_git_capture(parent, &["rev-parse", "--is-inside-work-tree"]).as_deref() != Some("true") {
        return GitExportStatus::unavailable();
    }
    let Some(porcelain) = run_git_capture(parent, &["status", "--porcelain", "--", file_name])
    else {
        return GitExportStatus::unavailable();
    };

    let (index_clean, worktree_clean) = porcelain_cleanliness(&porcelain);
    let tracked =
        run_git_capture(parent, &["ls-files", "--error-unmatch", "--", file_name]).is_some();
    // `HEAD:./<name>` resolves relative to the cwd we run git in, so no
    // toplevel-relative path computation is needed.
    let head_spec = format!("HEAD:./{file_name}");
    let head_hash = run_git_capture(parent, &["rev-parse", "--verify", "--quiet", &head_spec])
        .filter(|hash| !hash.is_empty());
    let worktree_hash = if jsonl_path.is_file() {
        run_git_capture(parent, &["hash-object", "--", file_name]).filter(|hash| !hash.is_empty())
    } else {
        None
    };

    GitExportStatus {
        available: true,
        tracked: Some(tracked),
        worktree_clean: Some(worktree_clean),
        index_clean: Some(index_clean),
        head_hash,
        worktree_hash,
    }
}

/// Execute the --status subcommand.
fn execute_status(
    storage: &crate::storage::SqliteStorage,
    path_policy: &SyncPathPolicy,
    db_path: &Path,
    use_json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let last_export_time = storage.get_metadata(METADATA_LAST_EXPORT_TIME)?;
    let last_import_time = storage.get_metadata(METADATA_LAST_IMPORT_TIME)?;
    let jsonl_content_hash = storage.get_metadata(METADATA_JSONL_CONTENT_HASH)?;

    let jsonl_path = &path_policy.jsonl_path;
    let staleness = compute_staleness(storage, jsonl_path)?;
    let dirty_count = staleness.dirty_count;
    let jsonl_exists = staleness.jsonl_exists;
    debug!(
        jsonl_path = %jsonl_path.display(),
        jsonl_exists,
        dirty_count,
        "Computed sync status inputs"
    );

    let classification = classify_sync_status_workspace(
        db_path,
        jsonl_path,
        staleness.jsonl_newer,
        staleness.db_newer,
    );
    let reliability_audit = classification.audit_record("sync.status");
    reliability_audit.emit_tracing(
        "status",
        if classification.anomalies.is_empty() {
            "ok"
        } else {
            "findings"
        },
    );

    let status = SyncStatus {
        dirty_count,
        last_export_time,
        last_import_time,
        jsonl_content_hash,
        jsonl_exists,
        jsonl_newer: staleness.jsonl_newer,
        db_newer: staleness.db_newer,
        workspace_health: classification.health.to_string(),
        reliability_audit,
        git_export: git_export_status(jsonl_path),
    };
    debug!(
        jsonl_newer = staleness.jsonl_newer,
        db_newer = staleness.db_newer,
        workspace_health = %status.workspace_health,
        "Computed sync staleness"
    );

    if !should_render_human_sync_output(ctx, use_json) {
        return Ok(());
    }

    if use_json {
        // Print JSON directly so --robot works even if OutputContext is non-JSON.
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else if ctx.is_rich() {
        render_status_rich(&status, ctx);
    } else {
        println!("Sync Status:");
        println!("  Dirty issues: {}", status.dirty_count);
        if let Some(ref t) = status.last_export_time {
            println!("  Last export: {t}");
        }
        if let Some(ref t) = status.last_import_time {
            println!("  Last import: {t}");
        }
        println!("  JSONL exists: {}", status.jsonl_exists);
        if status.jsonl_newer {
            println!("  Status: JSONL is newer (import recommended)");
        } else if status.db_newer {
            println!("  Status: Database is newer (export recommended)");
        } else {
            println!("  Status: In sync");
        }
    }

    Ok(())
}

/// Render sync status with rich formatting.
fn render_status_rich(status: &SyncStatus, ctx: &OutputContext) {
    let _console = Console::default();
    let theme = ctx.theme();

    // Determine sync state and color
    let (state_icon, state_text, state_style) = if status.jsonl_newer {
        (
            "⬇",
            "JSONL is newer (import recommended)",
            theme.info.clone(),
        )
    } else if status.db_newer {
        (
            "⬆",
            "Database is newer (export recommended)",
            theme.warning.clone(),
        )
    } else {
        ("✓", "In sync", theme.success.clone())
    };

    // Build status content
    let mut text = Text::new("");

    // State line
    text.append_styled(state_icon, state_style.clone());
    text.append(" ");
    text.append_styled(state_text, state_style);
    text.append("\n\n");

    // Dirty count
    text.append_styled("Dirty issues: ", theme.dimmed.clone());
    if status.dirty_count > 0 {
        text.append_styled(&status.dirty_count.to_string(), theme.warning.clone());
    } else {
        text.append_styled("0", theme.success.clone());
    }
    text.append("\n");

    // JSONL exists
    text.append_styled("JSONL exists: ", theme.dimmed.clone());
    text.append_styled(
        if status.jsonl_exists { "yes" } else { "no" },
        if status.jsonl_exists {
            theme.success.clone()
        } else {
            theme.muted.clone()
        },
    );
    text.append("\n");

    // Last export time
    if let Some(ref t) = status.last_export_time {
        text.append_styled("Last export:  ", theme.dimmed.clone());
        text.append_styled(t, theme.timestamp.clone());
        text.append("\n");
    }

    // Last import time
    if let Some(ref t) = status.last_import_time {
        text.append_styled("Last import:  ", theme.dimmed.clone());
        text.append_styled(t, theme.timestamp.clone());
        text.append("\n");
    }

    // Content hash (truncated)
    if let Some(ref hash) = status.jsonl_content_hash {
        text.append_styled("Content hash: ", theme.dimmed.clone());
        let display_hash = if hash.len() > 12 {
            format!("{}…", &hash[..12])
        } else {
            hash.clone()
        };
        text.append_styled(&display_hash, theme.muted.clone());
    }

    let panel = Panel::from_rich_text(&text, ctx.width())
        .title(Text::new("Sync Status"))
        .box_style(theme.box_style);
    ctx.render(&panel);
}

/// Execute the --witness operation.
fn execute_witness(
    path_policy: &SyncPathPolicy,
    args: &SyncArgs,
    use_json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let jsonl_path = &path_policy.jsonl_path;
    if !jsonl_path.is_file() {
        return Err(BeadsError::Config(format!(
            "JSONL file not found: {}",
            jsonl_path.display()
        )));
    }

    let witness_parallelism = effective_witness_parallelism(args);
    let witness =
        build_witness_for_path(jsonl_path, args.witness_chunk_lines, witness_parallelism)?;
    let base_artifacts = build_base_witness_artifacts(
        path_policy,
        args.witness_chunk_lines,
        witness_parallelism,
        &witness,
    )?;
    let result = SyncWitnessResult {
        jsonl_path: jsonl_path.display().to_string(),
        witness,
        base_jsonl_path: base_artifacts.jsonl_path,
        base_comparison: base_artifacts.comparison,
        base_reuse_plan: base_artifacts.reuse_plan,
        base_parallel_work_plan: base_artifacts.parallel_work_plan,
        base_reuse_materialization: base_artifacts.reuse_materialization,
    };

    if !should_render_human_sync_output(ctx, use_json) {
        return Ok(());
    }

    if use_json {
        ctx.json_pretty(&result);
    } else {
        render_witness_text(&result);
    }

    Ok(())
}

fn effective_witness_parallelism(args: &SyncArgs) -> usize {
    args.witness_parallelism
        .unwrap_or(DEFAULT_WITNESS_PARALLELISM)
}

fn build_witness_for_path(
    jsonl_path: &Path,
    chunk_size_lines: usize,
    max_parallelism: usize,
) -> Result<JsonlMerkleWitness> {
    crate::sync::ensure_no_conflict_markers(jsonl_path)?;
    let file = File::open(jsonl_path).map_err(|err| {
        BeadsError::Config(format!(
            "Failed to open JSONL file for witness {}: {err}",
            jsonl_path.display()
        ))
    })?;
    build_jsonl_merkle_witness_parallel(BufReader::new(file), chunk_size_lines, max_parallelism)
        .map_err(|err| {
            BeadsError::Config(format!(
                "Failed to build JSONL witness for {}: {err}",
                jsonl_path.display()
            ))
        })
}

fn build_base_witness_artifacts(
    path_policy: &SyncPathPolicy,
    chunk_size_lines: usize,
    max_parallelism: usize,
    current_witness: &JsonlMerkleWitness,
) -> Result<BaseWitnessArtifacts> {
    let base_jsonl_path = path_policy.beads_dir.join("beads.base.jsonl");
    match fs::symlink_metadata(&base_jsonl_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(BeadsError::Config(format!(
                "Base JSONL snapshot '{}' must not be a symlink",
                base_jsonl_path.display()
            )));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(BeadsError::Config(format!(
                "Base JSONL snapshot '{}' must be a regular file",
                base_jsonl_path.display()
            )));
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BaseWitnessArtifacts {
                jsonl_path: None,
                comparison: None,
                reuse_plan: None,
                parallel_work_plan: None,
                reuse_materialization: None,
            });
        }
        Err(err) => {
            return Err(BeadsError::Config(format!(
                "Failed to inspect base JSONL snapshot '{}': {err}",
                base_jsonl_path.display()
            )));
        }
    }

    require_valid_sync_path(&base_jsonl_path, &path_policy.beads_dir)?;

    let base_witness = build_witness_for_path(&base_jsonl_path, chunk_size_lines, max_parallelism)?;
    let comparison = compare_jsonl_merkle_witnesses(&base_witness, current_witness);
    let reuse_plan = plan_jsonl_witness_reuse(&base_witness, current_witness);
    let parallel_work_plan = plan_jsonl_witness_parallel_work(&reuse_plan, max_parallelism)
        .map_err(|err| BeadsError::Config(format!("Failed to plan JSONL witness work: {err}")))?;
    let reuse_materialization = materialize_base_reuse_plan(
        &base_jsonl_path,
        &path_policy.jsonl_path,
        &base_witness,
        current_witness,
        &reuse_plan,
    )?;

    Ok(BaseWitnessArtifacts {
        jsonl_path: Some(base_jsonl_path.display().to_string()),
        comparison: Some(comparison),
        reuse_plan: Some(reuse_plan),
        parallel_work_plan: Some(parallel_work_plan),
        reuse_materialization: Some(reuse_materialization),
    })
}

fn materialize_base_reuse_plan(
    base_jsonl_path: &Path,
    current_jsonl_path: &Path,
    base_witness: &JsonlMerkleWitness,
    current_witness: &JsonlMerkleWitness,
    reuse_plan: &JsonlWitnessReusePlan,
) -> Result<JsonlWitnessReuseMaterialization> {
    let mut base_file = File::open(base_jsonl_path).map_err(|err| {
        BeadsError::Config(format!(
            "Failed to open base JSONL file for reuse materialization {}: {err}",
            base_jsonl_path.display()
        ))
    })?;
    let mut current_file = File::open(current_jsonl_path).map_err(|err| {
        BeadsError::Config(format!(
            "Failed to open current JSONL file for reuse materialization {}: {err}",
            current_jsonl_path.display()
        ))
    })?;
    let mut sink = std::io::sink();

    materialize_jsonl_witness_reuse_plan(
        &mut base_file,
        &mut current_file,
        &mut sink,
        base_witness,
        current_witness,
        reuse_plan,
    )
    .map_err(|err| BeadsError::Config(format!("Failed to materialize JSONL reuse plan: {err}")))
}

fn render_witness_text(result: &SyncWitnessResult) {
    let witness = &result.witness;
    println!("JSONL Witness:");
    println!("  Path: {}", result.jsonl_path);
    println!("  Schema: {}", witness.schema_version);
    println!("  Lines: {}", witness.line_count);
    println!("  Bytes: {}", witness.byte_count);
    println!("  Chunk size: {} lines", witness.chunk_size_lines);
    println!("  Chunks: {}", witness.chunks.len());
    println!("  Root hash: {}", witness.root_hash);

    if let Some(comparison) = &result.base_comparison {
        if let Some(base_path) = &result.base_jsonl_path {
            println!("  Base path: {base_path}");
        }
        println!(
            "  Base comparison: drift={}, unchanged_chunks={}, changed_chunks={}, added_chunks={}, removed_chunks={}, safe_prefix_chunks={}",
            comparison.drift_detected,
            comparison.unchanged_chunks,
            comparison.changed_chunks,
            comparison.added_chunks,
            comparison.removed_chunks,
            comparison.safe_reuse_prefix_chunks
        );
        if let Some(index) = comparison.first_changed_chunk_index {
            println!("  First changed chunk: {index}");
        }
    }

    if let Some(plan) = &result.base_reuse_plan {
        println!("  Reuse plan actions: {}", plan.actions.len());
    }
    if let Some(plan) = &result.base_parallel_work_plan {
        println!(
            "  Parallel work batches: {} (max_parallelism={})",
            plan.total_batches, plan.max_parallelism
        );
    }
    if let Some(materialization) = &result.base_reuse_materialization {
        println!(
            "  Reuse materialization: output_bytes={}, reused_chunks={}, rebuilt_chunks={}, read_added_chunks={}, dropped_chunks={}",
            materialization.output_byte_count,
            materialization.reused_chunks,
            materialization.rebuilt_chunks,
            materialization.read_added_chunks,
            materialization.dropped_chunks
        );
    }
}

/// Execute the --flush-only (export) operation.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn execute_flush(
    storage: &mut crate::storage::SqliteStorage,
    _beads_dir: &Path,
    path_policy: &SyncPathPolicy,
    args: &SyncArgs,
    use_json: bool,
    show_progress: bool,
    retention_days: Option<u64>,
    history_config: HistoryConfig,
    ctx: &OutputContext,
) -> Result<()> {
    info!("Starting JSONL export");
    let export_policy = parse_export_policy(args)?;
    let jsonl_path = &path_policy.jsonl_path;
    debug!(
        jsonl_path = %jsonl_path.display(),
        external_jsonl = path_policy.is_external,
        export_policy = %export_policy,
        force = args.force,
        ?retention_days,
        "Export configuration resolved"
    );

    // Check for dirty issues
    let dirty_ids = storage.get_dirty_issue_ids()?;
    let needs_flush = storage.get_metadata("needs_flush")?.as_deref() == Some("true");
    let jsonl_exists = jsonl_path.exists();
    let db_issue_count = storage.count_issues()?;
    debug!(dirty_count = dirty_ids.len(), "Found dirty issues");

    // Refuse to overwrite a JSONL that still holds unresolved merge-conflict
    // markers. The main flush path below would blow away the `<<<<<<<` /
    // `=======` / `>>>>>>>` regions along with whatever remote side of the
    // merge they contain, silently resolving the conflict in favor of the
    // local DB. Detect the markers up-front so the operator can resolve the
    // merge (or pass `--force` if they actually intend the DB to win).
    if jsonl_exists && !args.force {
        crate::sync::ensure_no_conflict_markers(jsonl_path)?;
    }

    // If no dirty issues and no force, report nothing to do
    if dirty_ids.is_empty() && !needs_flush && jsonl_exists && !args.force {
        // `ensure_no_conflict_markers` ran above before we got here, so
        // `analyze_jsonl` below won't trip over unresolved `<<<<<<<` /
        // `=======` / `>>>>>>>` lines.

        // Guard against stale DB state without parsing the JSONL twice for count
        // and IDs.
        let (existing_count, jsonl_ids) = analyze_jsonl(jsonl_path)?;
        if existing_count > 0 && db_issue_count == 0 {
            warn!(
                jsonl_count = existing_count,
                "Refusing export of empty DB over non-empty JSONL"
            );
            return Err(BeadsError::Config(format!(
                "Refusing to export empty database over non-empty JSONL file.\n\
                     Database has 0 issues, JSONL has {existing_count} issues.\n\
                     This would result in data loss!\n\
                     Hint: Use --force to override this safety check."
            )));
        }

        if !jsonl_ids.is_empty() {
            let db_ids: HashSet<String> = storage.get_all_ids()?.into_iter().collect();
            let mut missing_list = jsonl_ids.difference(&db_ids).cloned().collect::<Vec<_>>();

            if !missing_list.is_empty() {
                missing_list.sort();
                let display_count = missing_list.len().min(10);
                let preview = missing_list
                    .iter()
                    .take(display_count)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = if missing_list.len() > 10 {
                    format!(" ... and {} more", missing_list.len() - 10)
                } else {
                    String::new()
                };

                return Err(BeadsError::Config(format!(
                    "Refusing to export stale database that would lose issues.\n\
                     Database has {} issues, JSONL has {} unique issues.\n\
                     Export would lose {} issue(s): {}{}\n\
                     Hint: Run import first, or use --force to override.",
                    db_issue_count,
                    jsonl_ids.len(),
                    missing_list.len(),
                    preview,
                    more
                )));
            }
        }

        if use_json {
            let result = FlushResult {
                exported_issues: 0,
                exported_dependencies: 0,
                exported_labels: 0,
                exported_comments: 0,
                content_hash: String::new(),
                cleared_dirty: 0,
                policy: export_policy,
                success_rate: 1.0,
                errors: Vec::new(),
                manifest_path: None,
            };
            ctx.json_pretty(&result);
        } else if should_render_human_sync_output(ctx, use_json) {
            println!("Nothing to export (no dirty issues)");
        }
        return Ok(());
    }

    // Configure export
    let export_config = ExportConfig {
        force: args.force || needs_flush,
        is_default_path: true,
        error_policy: export_policy,
        retention_days,
        beads_dir: Some(path_policy.beads_dir.clone()),
        allow_external_jsonl: path_policy.allow_external_jsonl,
        show_progress,
        history: history_config,
        max_parallel_workers: args.export_parallelism.unwrap_or(0),
    };

    // Execute export
    info!(path = %jsonl_path.display(), "Writing issues.jsonl");
    let (export_result, report) = export_to_jsonl_with_policy(storage, jsonl_path, &export_config)?;
    debug!(
        issues_exported = report.issues_exported,
        dependencies_exported = report.dependencies_exported,
        labels_exported = report.labels_exported,
        comments_exported = report.comments_exported,
        errors = report.errors.len(),
        "Export completed"
    );

    debug!(
        issues = export_result.exported_count,
        "Exported issues to JSONL"
    );

    // Finalize export (clear dirty flags, update metadata)
    finalize_export(
        storage,
        &export_result,
        Some(&export_result.issue_hashes),
        jsonl_path,
    )?;
    info!("Export complete, cleared dirty flags");

    // Write manifest if requested (atomic: temp + fsync + durable_rename)
    let manifest_path = if args.manifest {
        let manifest = serde_json::json!({
            "export_time": chrono::Utc::now().to_rfc3339(),
            "issues_count": export_result.exported_count,
            "content_hash": export_result.content_hash,
            "exported_ids": export_result.exported_ids,
            "policy": report.policy_used,
            "errors": &report.errors,
        });
        let manifest_file = path_policy.manifest_path.clone();
        require_safe_sync_overwrite_path(
            &manifest_file,
            &path_policy.beads_dir,
            path_policy.allow_external_jsonl,
            "write manifest",
        )?;
        write_manifest_atomically(&manifest_file, &manifest)?;
        Some(manifest_file.to_string_lossy().to_string())
    } else {
        None
    };

    // Output result
    let cleared_dirty = export_result.exported_marked_at.len();
    let result = FlushResult {
        exported_issues: report.issues_exported,
        exported_dependencies: report.dependencies_exported,
        exported_labels: report.labels_exported,
        exported_comments: report.comments_exported,
        content_hash: export_result.content_hash,
        cleared_dirty,
        policy: report.policy_used,
        success_rate: report.success_rate(),
        errors: report.errors.clone(),
        manifest_path,
    };

    if use_json {
        ctx.json_pretty(&result);
    } else if !should_render_human_sync_output(ctx, use_json) {
        return Ok(());
    } else if ctx.is_rich() {
        render_flush_result_rich(&result, &report.errors, ctx);
    } else {
        if report.policy_used != ExportErrorPolicy::Strict || report.has_errors() {
            println!("Export completed with policy: {}", report.policy_used);
        }
        println!("Exported:");
        println!(
            "  {} issue{}",
            result.exported_issues,
            if result.exported_issues == 1 { "" } else { "s" }
        );
        println!(
            "  {} dependenc{}{}",
            result.exported_dependencies,
            if result.exported_dependencies == 1 {
                "y"
            } else {
                "ies"
            },
            format_error_suffix(&report.errors, ExportEntityType::Dependency)
        );
        println!(
            "  {} label{}{}",
            result.exported_labels,
            if result.exported_labels == 1 { "" } else { "s" },
            format_error_suffix(&report.errors, ExportEntityType::Label)
        );
        println!(
            "  {} comment{}{}",
            result.exported_comments,
            if result.exported_comments == 1 {
                ""
            } else {
                "s"
            },
            format_error_suffix(&report.errors, ExportEntityType::Comment)
        );

        if result.cleared_dirty > 0 {
            println!(
                "Cleared dirty flag for {} issue{}",
                result.cleared_dirty,
                if result.cleared_dirty == 1 { "" } else { "s" }
            );
        }
        if let Some(ref path) = result.manifest_path {
            println!("Wrote manifest to {path}");
        }
        if report.has_errors() {
            println!();
            println!("Errors ({}):", report.errors.len());
            for err in &report.errors {
                println!("  {}", err.summary());
            }
        }
    }

    Ok(())
}

fn create_temp_manifest_file(manifest_path: &Path) -> Result<(PathBuf, File)> {
    let pid = std::process::id();

    for attempt in 0..64_u32 {
        let extension = if attempt == 0 {
            format!("json.{pid}.tmp")
        } else {
            format!("json.{pid}.{attempt}.tmp")
        };
        let temp_path = manifest_path.with_extension(extension);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if fs::symlink_metadata(&temp_path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(BeadsError::Config(format!(
                        "failed to create temp manifest file {}: {error}",
                        temp_path.display()
                    )));
                }
            }
            Err(error) => {
                return Err(BeadsError::Config(format!(
                    "failed to create temp manifest file {}: {error}",
                    temp_path.display()
                )));
            }
        }
    }

    Err(BeadsError::Config(format!(
        "failed to allocate temp manifest file for {}",
        manifest_path.display()
    )))
}

fn write_manifest_atomically(manifest_path: &Path, manifest: &serde_json::Value) -> Result<()> {
    use std::io::Write;

    let content = serde_json::to_string_pretty(manifest)?;

    let cleanup = |path: &Path| {
        let _ = fs::remove_file(path);
    };

    let (temp_path, mut file) = create_temp_manifest_file(manifest_path)?;

    // write_all / sync_all / durable_rename all must clean up the temp
    // file on failure; otherwise a torn manifest.json.<pid>.tmp can
    // accumulate on disk or, worse, confuse a concurrent attempt that
    // reuses the same PID-derived path after wraparound.
    file.write_all(content.as_bytes()).inspect_err(|_| {
        cleanup(&temp_path);
    })?;
    file.sync_all().inspect_err(|_| {
        cleanup(&temp_path);
    })?;
    drop(file);

    crate::util::durable_rename(&temp_path, manifest_path).inspect_err(|_| {
        cleanup(&temp_path);
    })?;

    Ok(())
}

/// Render flush (export) result with rich formatting.
fn render_flush_result_rich(result: &FlushResult, errors: &[ExportError], ctx: &OutputContext) {
    let _console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");

    // Success indicator
    if errors.is_empty() {
        text.append_styled("✓ ", theme.success.clone());
        text.append_styled("Export Complete", theme.success.clone());
    } else {
        text.append_styled("⚠ ", theme.warning.clone());
        text.append_styled("Export Complete (with errors)", theme.warning.clone());
    }
    text.append("\n\n");

    // Direction indicator
    text.append_styled("Direction     ", theme.dimmed.clone());
    text.append_styled("SQLite → JSONL", theme.info.clone());
    text.append("\n");

    // Exported counts
    text.append_styled("Issues        ", theme.dimmed.clone());
    text.append_styled(&result.exported_issues.to_string(), theme.accent.clone());
    text.append("\n");

    text.append_styled("Dependencies  ", theme.dimmed.clone());
    text.append(&result.exported_dependencies.to_string());
    text.append("\n");

    text.append_styled("Labels        ", theme.dimmed.clone());
    text.append(&result.exported_labels.to_string());
    text.append("\n");

    text.append_styled("Comments      ", theme.dimmed.clone());
    text.append(&result.exported_comments.to_string());
    text.append("\n");

    // Dirty flags cleared
    if result.cleared_dirty > 0 {
        text.append_styled("Dirty cleared ", theme.dimmed.clone());
        text.append_styled(&result.cleared_dirty.to_string(), theme.success.clone());
        text.append("\n");
    }

    // Content hash (truncated)
    if !result.content_hash.is_empty() {
        text.append("\n");
        text.append_styled("Content hash  ", theme.dimmed.clone());
        let display_hash = if result.content_hash.len() > 12 {
            format!("{}…", &result.content_hash[..12])
        } else {
            result.content_hash.clone()
        };
        text.append_styled(&display_hash, theme.muted.clone());
    }

    // Manifest path
    if let Some(ref path) = result.manifest_path {
        text.append("\n");
        text.append_styled("Manifest      ", theme.dimmed.clone());
        text.append_styled(path, theme.muted.clone());
    }

    let panel = Panel::from_rich_text(&text, ctx.width())
        .title(Text::new("Flush (Export)"))
        .box_style(theme.box_style);
    ctx.render(&panel);

    // Errors section if any
    if !errors.is_empty() {
        ctx.newline();
        render_errors_rich(errors, ctx);
    }
}

/// Render export errors with rich formatting.
fn render_errors_rich(errors: &[ExportError], ctx: &OutputContext) {
    let _console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled(
        &format!("{} error(s) during export:\n\n", errors.len()),
        theme.error.clone(),
    );

    for (i, err) in errors.iter().enumerate() {
        let prefix = if i == errors.len() - 1 {
            "└──"
        } else {
            "├──"
        };
        text.append_styled(prefix, theme.muted.clone());
        text.append(" ");
        text.append_styled(&err.summary(), theme.error.clone());
        text.append("\n");
    }

    let panel = Panel::from_rich_text(&text, ctx.width())
        .title(Text::new("⚠ Errors"))
        .box_style(theme.box_style);
    ctx.render(&panel);
}

fn parse_export_policy(args: &SyncArgs) -> Result<ExportErrorPolicy> {
    args.error_policy.as_deref().map_or_else(
        || Ok(ExportErrorPolicy::Strict),
        |value| {
            value.parse().map_err(|message| BeadsError::Validation {
                field: "error_policy".to_string(),
                reason: message,
            })
        },
    )
}

fn format_error_suffix(errors: &[ExportError], entity: ExportEntityType) -> String {
    let count = errors
        .iter()
        .filter(|err| err.entity_type == entity)
        .count();
    if count > 0 {
        format!(" ({count} error{})", if count == 1 { "" } else { "s" })
    } else {
        String::new()
    }
}

fn should_show_progress(json: bool, quiet: bool) -> bool {
    !json && !quiet && std::io::stdout().is_terminal()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn push_cli_rerun_overrides(rerun: &mut Vec<String>, cli: &config::CliOverrides) {
    if cli.json == Some(true) {
        rerun.push("--json".to_string());
    }
    if cli.quiet == Some(true) {
        rerun.push("--quiet".to_string());
    }
    // Preserve `--no-color` so the re-run inherits the caller's output
    // preference; dropping it silently flips colorized output back on.
    if cli.display_color == Some(false) {
        rerun.push("--no-color".to_string());
    }
    // Preserve `--actor` so audit-log entries from the re-run carry the
    // same identity the operator originally specified.
    if let Some(actor) = &cli.actor {
        rerun.push("--actor".to_string());
        rerun.push(shell_quote(actor));
    }
    if cli.allow_stale == Some(true) {
        rerun.push("--allow-stale".to_string());
    }
    if cli.no_daemon == Some(true) {
        rerun.push("--no-daemon".to_string());
    }
    if cli.no_auto_import == Some(true) {
        rerun.push("--no-auto-import".to_string());
    }
    if cli.no_auto_flush == Some(true) {
        rerun.push("--no-auto-flush".to_string());
    }
    if let Some(timeout) = cli.lock_timeout {
        rerun.push("--lock-timeout".to_string());
        rerun.push(timeout.to_string());
    }
}

fn integrity_check_is_clean(messages: &[String]) -> bool {
    matches!(messages, [message] if message.trim().eq_ignore_ascii_case("ok"))
}

fn fresh_force_import_maintenance_gate_applies(
    args: &SyncArgs,
    force_import_target_was_empty: bool,
    import_rewrote_storage: bool,
) -> bool {
    args.force
        && !args.rebuild
        && !args.rename_prefix
        && force_import_target_was_empty
        && import_rewrote_storage
}

fn repair_import_integrity_if_needed(
    storage: &mut crate::storage::SqliteStorage,
    beads_dir: &Path,
    cli: &config::CliOverrides,
    jsonl_path: &Path,
    db_path: &Path,
    show_progress: bool,
    allow_external_jsonl: bool,
) -> Result<()> {
    let messages = storage.integrity_check_messages()?;
    if integrity_check_is_clean(&messages) {
        return Ok(());
    }

    warn!(
        db_path = %db_path.display(),
        integrity_messages = ?messages,
        "Post-import maintenance left SQLite integrity warnings; rebuilding DB from JSONL"
    );

    let jsonl_filter = scan_jsonl_for_tombstone_filter(jsonl_path)?;
    let preserved_tombstones =
        tombstones_missing_from_jsonl_tombstones(snapshot_tombstones(storage), &jsonl_filter);

    // Close the dirty connection before rebuilding the same file path.
    let placeholder = crate::storage::SqliteStorage::open_memory()?;
    let dirty_storage = std::mem::replace(storage, placeholder);
    drop(dirty_storage);

    let startup = config::load_startup_config_with_paths(beads_dir, cli.db.as_ref())?;
    let (mut rebuilt_storage, _, _) = config::repair_database_from_jsonl(
        beads_dir,
        db_path,
        jsonl_path,
        cli.lock_timeout,
        &startup.merged_config,
        show_progress,
        allow_external_jsonl,
    )?;
    restore_tombstones_after_rebuild(&mut rebuilt_storage, &preserved_tombstones)?;
    *storage = rebuilt_storage;
    Ok(())
}

fn repair_import_integrity_with_import_config(
    storage: &mut crate::storage::SqliteStorage,
    beads_dir: &Path,
    cli: &config::CliOverrides,
    jsonl_path: &Path,
    db_path: &Path,
    show_progress: bool,
    import_config: &ImportConfig,
) -> Result<()> {
    let messages = storage.integrity_check_messages()?;
    if integrity_check_is_clean(&messages) {
        return Ok(());
    }

    warn!(
        db_path = %db_path.display(),
        integrity_messages = ?messages,
        "Post-import maintenance left SQLite integrity warnings; rebuilding DB from JSONL with original import semantics"
    );

    let jsonl_filter = scan_jsonl_for_tombstone_filter(jsonl_path)?;
    let preserved_tombstones =
        tombstones_missing_from_jsonl_tombstones(snapshot_tombstones(storage), &jsonl_filter);

    let placeholder = crate::storage::SqliteStorage::open_memory()?;
    let dirty_storage = std::mem::replace(storage, placeholder);
    drop(dirty_storage);

    let startup = config::load_startup_config_with_paths(beads_dir, cli.db.as_ref())?;
    let (mut rebuilt_storage, _, _) = config::repair_database_from_jsonl_with_import_config(
        beads_dir,
        db_path,
        jsonl_path,
        cli.lock_timeout,
        &startup.merged_config,
        show_progress,
        import_config.clone(),
    )?;
    restore_tombstones_after_rebuild(&mut rebuilt_storage, &preserved_tombstones)?;
    *storage = rebuilt_storage;
    Ok(())
}

fn auto_rebuild_semantic_flag_conflict_reason(
    args: &SyncArgs,
    cli: &config::CliOverrides,
    db_path: Option<&Path>,
) -> Option<String> {
    if !args.rename_prefix {
        return None;
    }

    let mut rerun = vec!["br".to_string()];
    if let Some(path) = db_path {
        rerun.push("--db".to_string());
        rerun.push(shell_quote(&path.display().to_string()));
    }
    push_cli_rerun_overrides(&mut rerun, cli);
    rerun.push("sync".to_string());
    rerun.push("--import-only".to_string());
    if args.allow_external_jsonl {
        rerun.push("--allow-external-jsonl".to_string());
    }
    if args.force {
        rerun.push("--force".to_string());
    }
    if args.rebuild {
        rerun.push("--rebuild".to_string());
    }
    rerun.push("--rename-prefix".to_string());

    Some(format!(
        "Open-time recovery rebuilt the database before import, so the requested import semantics (`--rename-prefix`) were not applied. Re-run `{}` now that the DB is healthy.",
        rerun.join(" ")
    ))
}

fn auto_rebuild_semantic_conflict_field(args: &SyncArgs) -> &'static str {
    if args.rebuild {
        "rebuild"
    } else if args.force {
        "force"
    } else {
        "rename_prefix"
    }
}

fn jsonl_contains_prefix_mismatch(jsonl_path: &Path, expected_prefix: &str) -> Result<bool> {
    for issue in read_issues_from_jsonl(jsonl_path)? {
        if issue.status == crate::model::Status::Tombstone {
            continue;
        }
        if !id_matches_expected_prefix(&issue.id, expected_prefix) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn jsonl_contains_duplicate_external_refs(jsonl_path: &Path) -> Result<bool> {
    let mut seen_external_refs = HashSet::new();
    for issue in read_issues_from_jsonl(jsonl_path)? {
        if let Some(external_ref) = issue.external_ref
            && !seen_external_refs.insert(external_ref)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn emit_auto_rebuild_import_result(
    storage: &crate::storage::SqliteStorage,
    use_json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let created = storage.count_all_issues()?;
    let result = ImportResultOutput {
        created,
        updated: 0,
        skipped: 0,
        tombstone_skipped: 0,
        orphans_removed: 0,
        blocked_cache_rebuilt: true,
    };
    if use_json {
        ctx.json_pretty(&result);
    } else if should_render_human_sync_output(ctx, use_json) {
        if ctx.is_rich() {
            render_import_result_rich(&result, ctx);
        } else {
            println!("Imported from JSONL (via automatic recovery):");
            println!("  Created: {} issues", result.created);
            println!("  Rebuilt blocked cache");
        }
    }
    Ok(())
}

/// Execute the --import-only operation.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn execute_import(
    storage: &mut crate::storage::SqliteStorage,
    beads_dir: &std::path::Path,
    cli: &config::CliOverrides,
    path_policy: &SyncPathPolicy,
    args: &SyncArgs,
    use_json: bool,
    show_progress: bool,
    auto_rebuilt: bool,
    db_path: &std::path::Path,
    ctx: &OutputContext,
) -> Result<()> {
    info!("Starting JSONL import");
    let jsonl_path = &path_policy.jsonl_path;
    debug!(
        jsonl_path = %jsonl_path.display(),
        external_jsonl = path_policy.is_external,
        force = args.force,
        auto_rebuilt,
        "Import configuration resolved"
    );

    // If the storage was just rebuilt from JSONL during the open sequence
    // (either the DB file did not exist or a recoverable anomaly triggered
    // `rebuild_database_from_jsonl`), the DB is already a clean import of the
    // JSONL. Re-running `--rebuild`/`--force` here would redo the import and
    // trigger fsqlite's stale-pager OpenRead bug ("could not open storage
    // cursor on root page N") because `reset_data_tables` + bulk INSERT within
    // the fresh connection exercises exactly the code path that just ran.
    // Prefix is a default for newly generated IDs, not a project-wide import
    // invariant. Only compute an expected prefix when the caller explicitly
    // asked to rename imported IDs into the configured prefix.
    let target_prefix = if args.rename_prefix {
        let layer = config::load_config_with_external_jsonl_policy(
            beads_dir,
            Some(storage),
            cli,
            path_policy.allow_external_jsonl,
        )?;
        let id_cfg = config::id_config_from_layer(&layer);
        Some(if id_cfg.prefix == "br" {
            // Prefix is still the default — check if we should auto-detect from JSONL
            let db_prefix = storage.get_config("issue_prefix")?;
            if let Some(p) = db_prefix {
                p
            } else if let Some(detected) = detect_prefix_from_jsonl(jsonl_path)? {
                info!(detected_prefix = %detected, "Auto-detected prefix from JSONL (no prefix configured)");
                // Persist the detected prefix to config for future operations
                storage.set_config("issue_prefix", &detected)?;
                detected
            } else {
                "br".to_string()
            }
        } else {
            // Config layer resolved a non-default prefix — use it
            id_cfg.prefix
        })
    } else {
        None
    };

    // When the caller requested semantics that auto-recovery could not honor
    // (`--rename-prefix`) *and* the JSONL actually contains mismatched IDs
    // that would have been renamed, fail explicitly so the operator can re-run
    // on the now-healthy DB. If the flag would have been a no-op, preserve the
    // happy-path short-circuit because the rebuild is already done. Skip the
    // whole check when there is no rename request (`target_prefix.is_none()`)
    // so we avoid the disk-touching `resolve_paths` call on the common path.
    let rename_semantics_were_skipped = auto_rebuilt
        && target_prefix.as_deref().is_some_and(|prefix| {
            jsonl_contains_prefix_mismatch(jsonl_path, prefix).unwrap_or(true)
                || jsonl_contains_duplicate_external_refs(jsonl_path).unwrap_or(true)
        });
    if rename_semantics_were_skipped {
        let rerun_db_path = config::resolve_paths(beads_dir, None)
            .ok()
            .filter(|paths| paths.db_path != *db_path)
            .map(|_| db_path);
        if let Some(reason) = auto_rebuild_semantic_flag_conflict_reason(args, cli, rerun_db_path) {
            return Err(BeadsError::Validation {
                field: auto_rebuild_semantic_conflict_field(args).to_string(),
                reason,
            });
        }
    }

    if auto_rebuilt {
        info!(
            force = args.force,
            rebuild = args.rebuild,
            "Skipping import body: database was rebuilt from JSONL during open"
        );
        emit_auto_rebuild_import_result(storage, use_json, ctx)?;
        return Ok(());
    }

    // Check if JSONL exists
    if !jsonl_path.exists() {
        warn!(path = %jsonl_path.display(), "JSONL path missing, skipping import");
        if use_json {
            let result = ImportResultOutput {
                created: 0,
                updated: 0,
                skipped: 0,
                tombstone_skipped: 0,
                orphans_removed: 0,
                blocked_cache_rebuilt: false,
            };
            ctx.json_pretty(&result);
        } else if should_render_human_sync_output(ctx, use_json) {
            println!("No JSONL file found at {}", jsonl_path.display());
        }
        return Ok(());
    }

    // Check staleness (unless --force or --rebuild)
    if !args.force && !args.rebuild {
        let last_import_time = storage.get_metadata(METADATA_LAST_IMPORT_TIME)?;
        let stored_hash = storage.get_metadata(METADATA_JSONL_CONTENT_HASH)?;

        if let (Some(import_time), Some(stored)) = (last_import_time, stored_hash) {
            // Check if JSONL content hash matches
            let current_hash = compute_jsonl_hash(jsonl_path)?;
            if current_hash == stored {
                debug!(
                    path = %jsonl_path.display(),
                    last_import = %import_time,
                    "JSONL is current, skipping import"
                );

                if use_json {
                    let result = ImportResultOutput {
                        created: 0,
                        updated: 0,
                        skipped: 0,
                        tombstone_skipped: 0,
                        orphans_removed: 0,
                        blocked_cache_rebuilt: false,
                    };
                    ctx.json_pretty(&result);
                } else if should_render_human_sync_output(ctx, use_json) {
                    println!("JSONL is current (hash unchanged since last import)");
                }
                return Ok(());
            }
        }
    }

    // Parse orphan mode
    let orphan_mode = match args.orphans.as_deref() {
        Some("strict") | None => OrphanMode::Strict,
        Some("resurrect") => OrphanMode::Resurrect,
        Some("skip") => OrphanMode::Skip,
        Some("allow") => OrphanMode::Allow,
        Some(other) => {
            return Err(BeadsError::Validation {
                field: "orphans".to_string(),
                reason: format!(
                    "Invalid orphan mode: {other}. Must be one of: strict, resurrect, skip, allow"
                ),
            });
        }
    };
    debug!(orphan_mode = ?orphan_mode, "Import orphan handling configured");

    // Configure import
    let import_config = ImportConfig {
        // Keep prefix validation when explicitly renaming prefixes.
        skip_prefix_validation: args.force && !args.rename_prefix,
        rename_on_import: args.rename_prefix,
        clear_duplicate_external_refs: args.rename_prefix,
        orphan_mode,
        force_upsert: args.force,
        beads_dir: Some(path_policy.beads_dir.clone()),
        allow_external_jsonl: path_policy.allow_external_jsonl,
        show_progress,
    };

    // For force/rebuild imports we read the JSONL twice before
    // `import_from_jsonl` is even called (once to collect issue IDs for the
    // orphan pass, once to precompute tombstone IDs for the preservation
    // filter). Those reads fail with a generic "Invalid JSON at line 1"
    // error when the JSONL contains merge-conflict markers, which buries
    // the much more actionable "merge conflict markers detected" message
    // that `import_from_jsonl` would surface later. Run the conflict-marker
    // scan up-front so the operator sees the right error class regardless
    // of which parse attempt fires first.
    if args.force || args.rebuild {
        crate::sync::ensure_no_conflict_markers(jsonl_path)?;
    }
    let jsonl_issue_ids = if args.force || args.rebuild {
        Some(get_issue_ids_from_jsonl(jsonl_path)?)
    } else {
        None
    };
    let jsonl_filter = if args.force || args.rebuild {
        Some(scan_jsonl_for_tombstone_filter(jsonl_path)?)
    } else {
        None
    };

    let preserved_tombstones = if args.force || args.rebuild {
        tombstones_missing_from_jsonl_tombstones(
            snapshot_tombstones(storage),
            jsonl_filter
                .as_ref()
                .expect("force/rebuild imports should precompute JSONL tombstone filter"),
        )
    } else {
        Vec::new()
    };
    let preserved_resurrection_attempts = jsonl_filter.as_ref().map_or(0, |filter| {
        preserved_tombstones
            .iter()
            .filter(|tombstone| {
                filter
                    .non_tombstone_updated_at
                    .contains_key(&tombstone.issue.id)
            })
            .count()
    });

    // For force imports and rebuilds, drop and recreate data tables to avoid
    // fsqlite btree cursor bugs on DELETE operations in large tables.
    // Config/metadata are preserved.  Without this, --rebuild on a corrupt DB
    // can hang indefinitely during orphan deletion (#245).
    //
    // Skip the reset when the `issues` table is already empty (e.g. right
    // after `br init` or `br init --force`): the DROP + CREATE sequence
    // generates "never used" freelist pages that fsqlite's VACUUM cannot
    // reclaim, which C sqlite3's integrity_check then flags as corruption
    // (issue #248). When the target is already empty, we can INSERT directly
    // and skip the leak entirely.
    let force_import_target_was_empty = if args.force || args.rebuild {
        let existing_issue_count = storage.count_all_issues()?;
        if existing_issue_count == 0 && preserved_tombstones.is_empty() {
            debug!(
                "Force/rebuild import: target DB already empty, skipping reset_data_tables to avoid fsqlite freelist leak"
            );
            true
        } else {
            debug!(
                existing_issue_count,
                preserved_tombstones = preserved_tombstones.len(),
                "Force/rebuild import: resetting data tables to avoid btree DELETE bugs; preserved tombstones will be restored atomically after import"
            );
            storage.reset_data_tables()?;
            false
        }
    } else {
        false
    };

    // Execute import
    info!(path = %jsonl_path.display(), "Importing from JSONL");
    let mut import_result = import_from_jsonl(
        storage,
        jsonl_path,
        &import_config,
        target_prefix.as_deref(),
    )?;

    info!(
        created_or_updated = import_result.imported_count,
        skipped = import_result.skipped_count,
        tombstone_skipped = import_result.tombstone_skipped,
        "Import complete"
    );

    // --rebuild: remove DB entries not present in JSONL.
    //
    // Skip this entirely when `--rename-prefix` is also set: the import just
    // rewrote every JSONL ID into the configured prefix, so `db_ids` are
    // post-rename (e.g. "newpref-xre") while `jsonl_ids` are pre-rename
    // (e.g. "oldpref-001"). The set-difference would classify every
    // newly-imported issue as an orphan and wipe the DB — exactly the
    // opposite of what the user asked for. With `reset_data_tables` having
    // cleared everything beforehand, the post-import DB contents already
    // mirror the JSONL (modulo the prefix rewrite), so the orphan pass has
    // nothing legitimate to remove anyway.
    //
    // Tombstones preserved across `reset_data_tables` via `snapshot_tombstones`
    // are NOT orphans — the whole point of preserving them was to keep
    // deletion-retention state alive across the rebuild. If the user has not
    // flushed to JSONL since deleting an issue, the tombstone is in the DB
    // but not in the JSONL, and a naïve set-difference would wipe it. Union
    // their IDs into the "acceptable" set so they survive the cleanup.
    if args.rebuild && !args.rename_prefix {
        let jsonl_ids = jsonl_issue_ids
            .as_ref()
            .expect("--rebuild should precompute JSONL issue IDs");
        let preserved_ids: HashSet<String> = preserved_tombstones
            .iter()
            .map(|t| t.issue.id.clone())
            .collect();
        let db_ids: HashSet<String> = storage.get_all_ids()?.into_iter().collect();
        let orphan_ids: Vec<String> = db_ids
            .iter()
            .filter(|id| !jsonl_ids.contains(*id) && !preserved_ids.contains(*id))
            .cloned()
            .collect();

        if !orphan_ids.is_empty() {
            info!(
                count = orphan_ids.len(),
                "Removing orphaned DB entries not present in JSONL"
            );
            for id in &orphan_ids {
                debug!(id = %id, "Removing orphaned issue");
                storage.delete_issue(id, "br-rebuild", "rebuild: not in JSONL", None)?;
            }
            import_result.orphans_removed = orphan_ids.len();
            // Rebuild blocked cache again after removals
            storage.rebuild_blocked_cache(true)?;
            info!(
                removed = orphan_ids.len(),
                "Rebuild orphan cleanup complete"
            );
        }
    } else if args.rebuild {
        debug!(
            "Skipping --rebuild orphan cleanup: --rename-prefix rewrote IDs, so JSONL IDs no longer match DB IDs and the set-difference would be incorrect"
        );
    }

    if args.force || args.rebuild {
        restore_tombstones_after_rebuild(storage, &preserved_tombstones)?;
        import_result.tombstone_skipped += preserved_resurrection_attempts;
    }

    // Update the source JSONL content hash before post-import maintenance.
    // Metadata table/index writes are part of the same B-tree surface that
    // triggered frankentorch-dbp, so compaction must be the final storage
    // mutation in this path.
    let content_hash = compute_jsonl_hash(jsonl_path)?;
    storage.set_metadata(METADATA_JSONL_CONTENT_HASH, &content_hash)?;

    // Post-import VACUUM + REINDEX to eliminate B-tree/index corruption
    // artifacts that frankensqlite's bulk-insert and metadata-update paths
    // can leave behind.  This mirrors what `rebuild_database_family` (used
    // by `br doctor --repair` and auto recovery) does at the equivalent
    // chokepoint.
    //
    // Without this, large `br sync --import-only` runs can produce a DB
    // where C sqlite3's `PRAGMA integrity_check` reports free-space or
    // index-entry corruption.  Force/rebuild imports hit this through
    // `reset_data_tables()` + bulk import (issue #248); the FrankenTorch
    // current-JSONL reproducer hit the plain import path through metadata
    // table/index churn after importing hundreds of rows.
    let import_rewrote_storage = import_result.imported_count > 0
        || import_result.blocked_cache_entries > 0
        || import_result.child_counter_entries > 0;
    let skip_heavy_import_maintenance = if fresh_force_import_maintenance_gate_applies(
        args,
        force_import_target_was_empty,
        import_rewrote_storage,
    ) {
        let messages = storage.integrity_check_messages()?;
        let clean = integrity_check_is_clean(&messages);
        if clean {
            debug!(
                db_path = %db_path.display(),
                "Skipping post-import VACUUM/REINDEX: fresh force import already passed integrity_check"
            );
        } else {
            warn!(
                db_path = %db_path.display(),
                integrity_messages = ?messages,
                "Fresh force import reported integrity warnings; running full post-import maintenance"
            );
        }
        clean
    } else {
        false
    };

    if (args.force || args.rebuild || import_rewrote_storage) && !skip_heavy_import_maintenance {
        // Drain the WAL before VACUUM/REINDEX so the snapshot they operate
        // on matches what's actually on disk. Without this, fsqlite's
        // post-import MVCC state lags behind and VACUUM fails silently with
        // "database is busy (snapshot conflict on pages)", leaving the
        // free-space / partial-index corruption that triggered issue #248
        // and frankentorch-dbp.
        if let Err(e) = storage.checkpoint_full() {
            warn!(
                error = %e,
                db_path = %db_path.display(),
                "Full WAL checkpoint after JSONL import failed (non-fatal)"
            );
        }
        if let Err(e) = storage.execute_raw("VACUUM") {
            warn!(error = %e, "VACUUM after JSONL import failed (non-fatal); DB may still contain free-space corruption");
        }
        if let Err(e) = storage.execute_raw("REINDEX") {
            warn!(error = %e, "REINDEX after JSONL import failed (non-fatal); partial-index entries may be inconsistent");
        }
        // Final compaction via `VACUUM INTO` + atomic rename. fsqlite's
        // in-place VACUUM does not truncate the trailing pages that its
        // REINDEX leaves orphaned, so upstream sqlite3's `PRAGMA
        // integrity_check` reports `Page N: never used` on the rebuilt
        // file (issue #248). `VACUUM INTO` sidesteps the bug because it
        // writes a brand-new compacted file from the reachable page set,
        // page count and layout matching what `sqlite3 "VACUUM INTO"`
        // would produce. The helper runs its own pre-VACUUM-INTO WAL
        // checkpoint to drain the frames the VACUUM/REINDEX above just
        // wrote. Once it closes the old handle, reopen failures must abort
        // this import rather than letting subsequent metadata updates run
        // against a throwaway placeholder.
        let placeholder = crate::storage::SqliteStorage::open_memory()?;
        let original_storage = std::mem::replace(storage, placeholder);
        match config::compact_database_via_vacuum_into_in_place(
            original_storage,
            db_path,
            cli.lock_timeout,
        ) {
            Ok(compacted_storage) => *storage = compacted_storage,
            Err(err) => {
                if let Ok(reopened) =
                    crate::storage::SqliteStorage::open_with_timeout(db_path, cli.lock_timeout)
                {
                    *storage = reopened;
                }
                return Err(err);
            }
        }
        if args.rename_prefix {
            repair_import_integrity_with_import_config(
                storage,
                beads_dir,
                cli,
                jsonl_path,
                db_path,
                show_progress,
                &import_config,
            )?;
        } else {
            repair_import_integrity_if_needed(
                storage,
                beads_dir,
                cli,
                jsonl_path,
                db_path,
                show_progress,
                path_policy.allow_external_jsonl,
            )?;
        }
    }

    // Output result
    let result = ImportResultOutput {
        created: import_result.created_count,
        updated: import_result.updated_count,
        skipped: import_result.skipped_count,
        tombstone_skipped: import_result.tombstone_skipped,
        orphans_removed: import_result.orphans_removed,
        blocked_cache_rebuilt: true,
    };

    if use_json {
        ctx.json_pretty(&result);
    } else if !should_render_human_sync_output(ctx, use_json) {
        return Ok(());
    } else if ctx.is_rich() {
        render_import_result_rich(&result, ctx);
    } else {
        let processed = import_result.imported_count
            + import_result.skipped_count
            + import_result.tombstone_skipped;
        println!("Imported from JSONL:");
        println!("  Processed: {processed} issues");
        println!("  Created: {} issues", result.created);
        println!("  Updated: {} issues", result.updated);
        if result.skipped > 0 {
            println!("  Skipped: {} issues (up-to-date)", result.skipped);
        }
        if result.tombstone_skipped > 0 {
            println!("  Tombstone protected: {} issues", result.tombstone_skipped);
        }
        if result.orphans_removed > 0 {
            println!(
                "  Orphans removed: {} issues (not in JSONL)",
                result.orphans_removed
            );
        }
        println!("  Rebuilt blocked cache");
    }

    Ok(())
}

/// Render import result with rich formatting.
fn render_import_result_rich(result: &ImportResultOutput, ctx: &OutputContext) {
    let _console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");

    // Success indicator
    text.append_styled("✓ ", theme.success.clone());
    text.append_styled("Import Complete", theme.success.clone());
    text.append("\n\n");

    // Direction indicator
    text.append_styled("Direction          ", theme.dimmed.clone());
    text.append_styled("JSONL → SQLite", theme.info.clone());
    text.append("\n");

    // Created count
    text.append_styled("Created            ", theme.dimmed.clone());
    text.append_styled(&result.created.to_string(), theme.accent.clone());
    text.append_styled(" issues", theme.dimmed.clone());
    text.append("\n");

    // Updated count
    text.append_styled("Updated            ", theme.dimmed.clone());
    text.append_styled(&result.updated.to_string(), theme.accent.clone());
    text.append_styled(" issues", theme.dimmed.clone());
    text.append("\n");

    // Skipped count
    if result.skipped > 0 {
        text.append_styled("Skipped            ", theme.dimmed.clone());
        text.append(&result.skipped.to_string());
        text.append_styled(" (up-to-date)", theme.muted.clone());
        text.append("\n");
    }

    // Tombstone protected
    if result.tombstone_skipped > 0 {
        text.append_styled("Tombstone protected ", theme.dimmed.clone());
        text.append(&result.tombstone_skipped.to_string());
        text.append("\n");
    }

    // Orphans removed
    if result.orphans_removed > 0 {
        text.append_styled("Orphans removed    ", theme.dimmed.clone());
        text.append_styled(&result.orphans_removed.to_string(), theme.warning.clone());
        text.append_styled(" (not in JSONL)", theme.muted.clone());
        text.append("\n");
    }

    // Cache rebuilt
    text.append("\n");
    text.append_styled("✓ ", theme.success.clone());
    text.append_styled("Blocked cache rebuilt", theme.muted.clone());

    let panel = Panel::from_rich_text(&text, ctx.width())
        .title(Text::new("Import"))
        .box_style(theme.box_style);
    ctx.render(&panel);
}

/// Detect the issue ID prefix from the first non-tombstone issue in a JSONL file.
///
/// Returns `None` if the file is empty or contains no issues with a recognizable prefix.
/// Supports hyphenated prefixes such as `document-intelligence-0sa`.
fn detect_prefix_from_jsonl(jsonl_path: &Path) -> Result<Option<String>> {
    let issues = read_issues_from_jsonl(jsonl_path)?;

    for issue in issues {
        if issue.status == crate::model::Status::Tombstone {
            continue;
        }

        if let Some((prefix, _)) = split_prefix_remainder(&issue.id) {
            return Ok(Some(prefix.to_string()));
        }
    }

    Ok(None)
}

/// Execute the --merge operation.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn execute_merge(
    storage: &mut crate::storage::SqliteStorage,
    path_policy: &SyncPathPolicy,
    args: &SyncArgs,
    use_json: bool,
    show_progress: bool,
    retention_days: Option<u64>,
    history_config: HistoryConfig,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    info!("Starting 3-way merge");
    let beads_dir = &path_policy.beads_dir;
    let jsonl_path = &path_policy.jsonl_path;

    // 1. Load Base State (ancestor)
    let base = load_base_snapshot(beads_dir)?;
    debug!(base_count = base.len(), "Loaded base snapshot");

    // 2. Load Left State (local DB)
    let mut left_issues = storage.get_all_issues_for_export()?;
    let all_deps = storage.get_all_dependency_records()?;
    let all_labels = storage.get_all_labels()?;
    let all_comments = storage.get_all_comments()?;

    for issue in &mut left_issues {
        if let Some(deps) = all_deps.get(&issue.id) {
            issue.dependencies = deps.clone();
        }
        if let Some(labels) = all_labels.get(&issue.id) {
            issue.labels = labels.clone();
        }
        if let Some(comments) = all_comments.get(&issue.id) {
            issue.comments = comments.clone();
        }
    }

    let mut left = HashMap::new();
    for issue in left_issues {
        left.insert(issue.id.clone(), issue);
    }
    debug!(left_count = left.len(), "Loaded local state (DB)");

    // 3. Load Right State (external JSONL)
    let mut right = HashMap::new();
    if jsonl_path.exists() {
        // `read_issues_from_jsonl` parses JSON line-by-line, which yields a
        // generic "Invalid JSON at line 1" error when the JSONL still
        // contains unresolved merge-conflict markers from a botched
        // `git merge` / `git pull`. A three-way merge on top of that state
        // would be nonsense, so scan for markers first and surface the
        // helpful error before we try to parse.
        crate::sync::ensure_no_conflict_markers(jsonl_path)?;
        for issue in read_issues_from_jsonl(jsonl_path)? {
            right.insert(issue.id.clone(), issue);
        }
    }
    debug!(right_count = right.len(), "Loaded external state (JSONL)");

    // 4. Perform Merge
    let context = MergeContext::new(base, left, right);
    let strategy = merge_conflict_resolution(args);
    let resolution = merge_conflict_resolution_label(strategy);
    let local_tombstones: HashSet<String> = context
        .left
        .values()
        .filter(|issue| issue.status == crate::model::Status::Tombstone)
        .map(|issue| issue.id.clone())
        .collect();
    let tombstones = if local_tombstones.is_empty() {
        None
    } else {
        Some(&local_tombstones)
    };

    let report = three_way_merge(&context, strategy, tombstones);

    // 5. Apply Changes to DB
    info!(
        kept = report.kept.len(),
        deleted = report.deleted.len(),
        conflicts = report.conflicts.len(),
        resolution,
        "Merge calculated"
    );

    if report.has_conflicts() {
        // Require an explicit merge winner instead of guessing when both sides changed.
        if ctx.is_rich() {
            render_merge_conflicts_rich(&report.conflicts, ctx);
        }
        let mut msg = String::from("Merge conflicts detected:\n");
        for (id, kind) in &report.conflicts {
            use std::fmt::Write;
            let _ = writeln!(msg, "  - {id}: {kind:?}");
        }
        msg.push_str("\nUse --force-db to keep local DB changes, --force-jsonl to keep JSONL changes, or --force to keep the newer timestamp.");
        return Err(BeadsError::Config(msg));
    }

    let _actor = cli.actor.as_deref().unwrap_or("br");

    // Apply deletions. Base snapshots can lag behind historical ID migrations, so a
    // merge may legitimately request deletion of an issue that is already absent from
    // the live database. Treat that as a no-op instead of aborting the whole merge.
    let existing_deleted_issues = storage.get_issues_by_ids(&report.deleted)?;
    let existing_deleted_ids: std::collections::HashSet<String> =
        existing_deleted_issues.into_iter().map(|i| i.id).collect();

    for id in &report.deleted {
        if existing_deleted_ids.contains(id) {
            storage.delete_issue(id, "system", "merge deletion", Some(chrono::Utc::now()))?;
        } else {
            tracing::debug!(
                issue_id = %id,
                "Skipping merge deletion for issue already absent from local database"
            );
        }
    }

    // Apply updates/creates (upsert)
    // We need to retrieve the actual Issue objects to upsert.
    for issue in &report.kept {
        storage.upsert_issue_for_import(issue)?;
        storage.sync_labels_for_import(&issue.id, &issue.labels)?;
        storage.sync_dependencies_for_import(&issue.id, &issue.dependencies)?;
        storage.sync_comments_for_import(&issue.id, &issue.comments)?;
    }

    // Add merge notes as comments
    for (id, note) in &report.notes {
        if let Err(e) = storage.add_comment(id, "br-sync", note) {
            tracing::warn!(issue_id = %id, error = %e, "Failed to add merge note to issue");
        } else {
            tracing::info!(issue_id = %id, note = %note, "Added merge resolution note");
        }
    }

    // Rebuild cache
    storage.rebuild_blocked_cache(true)?;
    // Merge can introduce hierarchical IDs via upsert; refresh counters before
    // the next child-ID allocation trusts them.
    storage.rebuild_child_counters_in_tx()?;

    // Force Export to update JSONL (ensure sync)
    info!(path = %jsonl_path.display(), "Writing merged issues.jsonl");
    let export_config = ExportConfig {
        force: true, // Force export to ensure JSONL matches DB
        is_default_path: true,
        error_policy: ExportErrorPolicy::Strict,
        retention_days,
        beads_dir: Some(path_policy.beads_dir.clone()),
        allow_external_jsonl: path_policy.allow_external_jsonl,
        show_progress,
        history: history_config,
        max_parallel_workers: args.export_parallelism.unwrap_or(0),
    };

    let (export_result, _) = export_to_jsonl_with_policy(storage, jsonl_path, &export_config)?;
    finalize_export(
        storage,
        &export_result,
        Some(&export_result.issue_hashes),
        jsonl_path,
    )?;
    save_base_snapshot_from_jsonl(jsonl_path, beads_dir)?;

    // Output success message
    if use_json {
        let output = serde_json::json!({
            "status": "success",
            "merged_issues": report.kept.len(),
            "deleted_issues": report.deleted.len(),
            "conflicts": report.conflicts.len(),
            "resolution": resolution,
            "notes": report.notes,
        });
        ctx.json_pretty(&output);
    } else if !should_render_human_sync_output(ctx, use_json) {
        return Ok(());
    } else if ctx.is_rich() {
        render_merge_result_rich(&report, ctx);
    } else {
        println!("Merge complete:");
        println!("  Kept/Updated: {} issues", report.kept.len());
        println!("  Deleted: {} issues", report.deleted.len());
        if !report.notes.is_empty() {
            println!("  Notes:");
            for (id, note) in &report.notes {
                println!("    - {id}: {note}");
            }
        }
        println!("  Base snapshot updated.");
        println!("  JSONL exported.");
    }

    Ok(())
}

/// Render merge conflicts with rich formatting.
fn render_merge_conflicts_rich(
    conflicts: &[(String, crate::sync::ConflictType)],
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("⚠ ", theme.error.clone());
    text.append_styled(
        &format!("{} merge conflict(s) detected:\n\n", conflicts.len()),
        theme.error.clone(),
    );

    for (i, (id, kind)) in conflicts.iter().enumerate() {
        let prefix = if i == conflicts.len() - 1 {
            "└──"
        } else {
            "├──"
        };
        text.append_styled(prefix, theme.muted.clone());
        text.append(" ");
        text.append_styled(id, theme.issue_id.clone());
        text.append(": ");
        text.append_styled(&format!("{kind:?}"), theme.error.clone());
        text.append("\n");
    }

    text.append("\n");
    text.append_styled("Hint: ", theme.dimmed.clone());
    text.append("Use --force-db to keep local DB changes, --force-jsonl to keep JSONL changes, or --force to keep the newer timestamp.");

    let panel = Panel::from_rich_text(&text, ctx.width())
        .title(Text::new("Merge Conflicts"))
        .box_style(theme.box_style);
    console.print_renderable(&panel);
}

/// Render merge result with rich formatting.
fn render_merge_result_rich(report: &crate::sync::MergeReport, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");

    // Success indicator
    text.append_styled("✓ ", theme.success.clone());
    text.append_styled("3-Way Merge Complete", theme.success.clone());
    text.append("\n\n");

    // Kept/Updated count
    text.append_styled("Kept/Updated  ", theme.dimmed.clone());
    text.append_styled(&report.kept.len().to_string(), theme.accent.clone());
    text.append_styled(" issues", theme.dimmed.clone());
    text.append("\n");

    // Deleted count
    text.append_styled("Deleted       ", theme.dimmed.clone());
    if report.deleted.is_empty() {
        text.append("0");
    } else {
        text.append_styled(&report.deleted.len().to_string(), theme.warning.clone());
    }
    text.append_styled(" issues", theme.dimmed.clone());
    text.append("\n");

    // Notes section
    if !report.notes.is_empty() {
        text.append("\n");
        text.append_styled("Notes:\n", theme.dimmed.clone());
        for (i, (id, note)) in report.notes.iter().enumerate() {
            let prefix = if i == report.notes.len() - 1 {
                "└──"
            } else {
                "├──"
            };
            text.append_styled(prefix, theme.muted.clone());
            text.append(" ");
            text.append_styled(id, theme.issue_id.clone());
            text.append(": ");
            text.append_styled(note, theme.muted.clone());
            text.append("\n");
        }
    }

    // Final status
    text.append("\n");
    text.append_styled("✓ ", theme.success.clone());
    text.append_styled("Base snapshot updated\n", theme.muted.clone());
    text.append_styled("✓ ", theme.success.clone());
    text.append_styled("JSONL exported", theme.muted.clone());

    let panel = Panel::from_rich_text(&text, ctx.width())
        .title(Text::new("Merge"))
        .box_style(theme.box_style);
    console.print_renderable(&panel);
}

#[cfg(test)]
mod tests {
    use super::{
        SyncOperation, SyncPathPolicy, auto_rebuild_semantic_conflict_field,
        auto_rebuild_semantic_flag_conflict_reason, build_base_witness_artifacts,
        classify_sync_status_workspace, detect_prefix_from_jsonl,
        fresh_force_import_maintenance_gate_applies, git_export_status,
        jsonl_contains_duplicate_external_refs, jsonl_contains_prefix_mismatch,
        merge_conflict_resolution, porcelain_cleanliness, prepare_sync_startup,
        should_defer_jsonl_recovery, should_render_human_sync_output, sync_operation,
        validate_operator_requested_sync_path, validate_sync_mode_args, validate_sync_paths,
        write_manifest_atomically,
    };
    use crate::cli::SyncArgs;
    use crate::config::{self, CliOverrides};
    use crate::error::BeadsError;
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::output::OutputContext;
    use crate::storage::SqliteStorage;
    use crate::sync::{
        ConflictResolution, PreservedTombstone, restore_tombstones,
        scan_jsonl_for_tombstone_filter, snapshot_tombstones,
        tombstones_missing_from_jsonl_tombstones,
    };
    use chrono::Utc;
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn make_test_issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc::now(),
            created_by: None,
            updated_at: Utc::now(),
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_write_manifest_atomically_rejects_existing_temp_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let manifest_path = beads_dir.join(".manifest.json");
        let temp_path = manifest_path.with_extension(format!("json.{}.tmp", std::process::id()));
        let outside_target = temp.path().join("outside.json");
        fs::write(&outside_target, "preserve").unwrap();
        symlink(&outside_target, &temp_path).unwrap();

        let err = write_manifest_atomically(&manifest_path, &serde_json::json!({ "ok": true }))
            .unwrap_err();

        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let message = if let BeadsError::Config(message) = &err {
            message.as_str()
        } else {
            ""
        };
        assert!(
            message.contains("failed to create temp manifest file"),
            "unexpected message: {message}"
        );
        assert_eq!(
            fs::read_to_string(&outside_target).unwrap(),
            "preserve",
            "manifest temp symlink target must not receive manifest bytes"
        );
        assert!(
            !manifest_path.exists(),
            "failed manifest write must not install a manifest"
        );
        assert!(
            fs::symlink_metadata(&temp_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "rejected pre-existing temp symlink should be left untouched"
        );
    }

    #[test]
    fn test_write_manifest_atomically_skips_stale_regular_temp_file() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let manifest_path = beads_dir.join(".manifest.json");
        let stale_temp_path =
            manifest_path.with_extension(format!("json.{}.tmp", std::process::id()));
        fs::write(&stale_temp_path, "stale temp").unwrap();

        write_manifest_atomically(&manifest_path, &serde_json::json!({ "ok": true })).unwrap();

        let manifest = fs::read_to_string(&manifest_path).unwrap();
        assert!(
            manifest.contains("\"ok\": true"),
            "manifest should be written through a collision-free temp path"
        );
        assert_eq!(
            fs::read_to_string(&stale_temp_path).unwrap(),
            "stale temp",
            "stale temp collision should be left untouched"
        );
    }

    #[test]
    fn should_render_human_sync_output_preserves_quiet_json_semantics() {
        let quiet_ctx = OutputContext::from_flags(false, true, true);
        let plain_ctx = OutputContext::from_flags(false, false, true);

        assert!(!should_render_human_sync_output(&quiet_ctx, false));
        assert!(should_render_human_sync_output(&quiet_ctx, true));
        assert!(should_render_human_sync_output(&plain_ctx, false));
        assert!(should_render_human_sync_output(&plain_ctx, true));
    }

    #[test]
    fn fresh_force_import_maintenance_gate_only_applies_to_clean_empty_force_loads() {
        let force_import = SyncArgs {
            import_only: true,
            force: true,
            ..SyncArgs::default()
        };
        assert!(fresh_force_import_maintenance_gate_applies(
            &force_import,
            true,
            true
        ));
        assert!(!fresh_force_import_maintenance_gate_applies(
            &force_import,
            false,
            true
        ));
        assert!(!fresh_force_import_maintenance_gate_applies(
            &force_import,
            true,
            false
        ));

        let rebuild = SyncArgs {
            import_only: true,
            force: true,
            rebuild: true,
            ..SyncArgs::default()
        };
        assert!(!fresh_force_import_maintenance_gate_applies(
            &rebuild, true, true
        ));

        let rename_prefix = SyncArgs {
            import_only: true,
            force: true,
            rename_prefix: true,
            ..SyncArgs::default()
        };
        assert!(!fresh_force_import_maintenance_gate_applies(
            &rename_prefix,
            true,
            true
        ));
    }

    #[test]
    fn sync_operation_witness_is_explicit_read_only_mode() {
        let args = SyncArgs {
            witness: true,
            witness_chunk_lines: 2,
            ..SyncArgs::default()
        };

        assert_eq!(sync_operation(&args), SyncOperation::Witness);
        assert!(!should_defer_jsonl_recovery(&args));
    }

    #[cfg(unix)]
    #[test]
    fn build_base_witness_artifacts_rejects_symlinked_base_snapshot() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();

        let current_jsonl_path = beads_dir.join("issues.jsonl");
        fs::write(&current_jsonl_path, "current\n").unwrap();
        let outside_base_path = outside_dir.join("beads.base.jsonl");
        fs::write(&outside_base_path, "outside\n").unwrap();
        symlink(&outside_base_path, beads_dir.join("beads.base.jsonl")).unwrap();

        let path_policy = SyncPathPolicy {
            jsonl_path: current_jsonl_path.clone(),
            jsonl_temp_path: current_jsonl_path.with_extension("jsonl.tmp"),
            manifest_path: beads_dir.join(".manifest.json"),
            beads_dir,
            is_external: false,
            allow_external_jsonl: false,
        };
        let current_witness = super::build_witness_for_path(&current_jsonl_path, 1, 1)
            .expect("current witness should build");

        let result = build_base_witness_artifacts(&path_policy, 1, 1, &current_witness);
        assert!(
            result.is_err(),
            "base witness should reject symlinked base snapshot"
        );
        let err = result.err().expect("checked error result");

        assert!(
            matches!(&err, BeadsError::Config(message) if message.contains("must not be a symlink")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_validate_sync_mode_args_rejects_mode_conflicts() {
        let status_conflict = SyncArgs {
            status: true,
            witness: true,
            witness_chunk_lines: 2,
            ..SyncArgs::default()
        };
        let err = validate_sync_mode_args(&status_conflict)
            .expect_err("status and witness should conflict");
        assert!(matches!(err, BeadsError::Validation { .. }));

        let status_flush_conflict = SyncArgs {
            status: true,
            flush_only: true,
            ..SyncArgs::default()
        };
        let err = validate_sync_mode_args(&status_flush_conflict)
            .expect_err("status and flush-only should conflict");
        assert!(matches!(err, BeadsError::Validation { .. }));

        let merge_conflict = SyncArgs {
            merge: true,
            witness: true,
            witness_chunk_lines: 2,
            ..SyncArgs::default()
        };
        let err = validate_sync_mode_args(&merge_conflict)
            .expect_err("merge and witness should conflict");
        assert!(matches!(err, BeadsError::Validation { .. }));
    }

    #[test]
    fn test_validate_sync_mode_args_rejects_zero_witness_chunk_lines() {
        let args = SyncArgs {
            witness: true,
            witness_chunk_lines: 0,
            ..SyncArgs::default()
        };

        let err = validate_sync_mode_args(&args).expect_err("zero witness chunk size should fail");
        assert!(matches!(err, BeadsError::Validation { .. }));
    }

    #[test]
    fn test_validate_sync_mode_args_rejects_zero_witness_parallelism() {
        let args = SyncArgs {
            witness: true,
            witness_chunk_lines: 2,
            witness_parallelism: Some(0),
            ..SyncArgs::default()
        };

        let err = validate_sync_mode_args(&args).expect_err("zero witness parallelism should fail");
        assert!(matches!(err, BeadsError::Validation { .. }));
    }

    #[test]
    fn test_validate_sync_mode_args_rejects_zero_export_parallelism() {
        let args = SyncArgs {
            flush_only: true,
            export_parallelism: Some(0),
            ..SyncArgs::default()
        };

        let err = validate_sync_mode_args(&args).expect_err("zero export parallelism should fail");
        assert!(matches!(err, BeadsError::Validation { .. }));
    }

    #[test]
    fn test_validate_sync_mode_args_rebuild_requires_import_only() {
        let status_rebuild = SyncArgs {
            status: true,
            rebuild: true,
            ..SyncArgs::default()
        };
        let err = validate_sync_mode_args(&status_rebuild)
            .expect_err("status rebuild should be rejected");
        assert!(matches!(&err, BeadsError::Validation { field, .. } if field == "rebuild"));
        assert!(err.to_string().contains("--import-only"));

        let witness_rebuild = SyncArgs {
            witness: true,
            rebuild: true,
            witness_chunk_lines: 2,
            ..SyncArgs::default()
        };
        let err = validate_sync_mode_args(&witness_rebuild)
            .expect_err("witness rebuild should be rejected");
        assert!(matches!(&err, BeadsError::Validation { field, .. } if field == "rebuild"));
        assert!(err.to_string().contains("--import-only"));

        let import_rebuild = SyncArgs {
            import_only: true,
            rebuild: true,
            ..SyncArgs::default()
        };
        validate_sync_mode_args(&import_rebuild).unwrap();
    }

    #[test]
    fn test_merge_conflict_resolution_defaults_to_manual() {
        let args = SyncArgs {
            merge: true,
            ..SyncArgs::default()
        };

        assert_eq!(merge_conflict_resolution(&args), ConflictResolution::Manual);
    }

    #[test]
    fn test_merge_conflict_resolution_supports_explicit_winners() {
        let force_db = SyncArgs {
            merge: true,
            force_db: true,
            ..SyncArgs::default()
        };
        let force_jsonl = SyncArgs {
            merge: true,
            force_jsonl: true,
            ..SyncArgs::default()
        };
        let force_newer = SyncArgs {
            merge: true,
            force: true,
            ..SyncArgs::default()
        };

        assert_eq!(
            merge_conflict_resolution(&force_db),
            ConflictResolution::PreferLocal
        );
        assert_eq!(
            merge_conflict_resolution(&force_jsonl),
            ConflictResolution::PreferExternal
        );
        assert_eq!(
            merge_conflict_resolution(&force_newer),
            ConflictResolution::PreferNewer
        );
    }

    #[test]
    fn test_merge_resolution_flags_require_merge_mode() {
        let args = SyncArgs {
            force_db: true,
            ..SyncArgs::default()
        };

        let err = validate_sync_mode_args(&args).expect_err("force-db should require merge");
        assert!(matches!(err, BeadsError::Validation { .. }));
        assert!(err.to_string().contains("--merge"));
    }

    #[test]
    fn test_sync_operation_selects_unspecified_and_explicit_modes() {
        assert_eq!(
            sync_operation(&SyncArgs::default()),
            SyncOperation::Unspecified
        );
        let bare_err = validate_sync_mode_args(&SyncArgs::default())
            .expect_err("bare sync should require an explicit mode");
        assert!(matches!(bare_err, BeadsError::Validation { .. }));
        assert!(bare_err.to_string().contains("--flush-only"));

        let flush = SyncArgs {
            flush_only: true,
            ..SyncArgs::default()
        };
        assert_eq!(sync_operation(&flush), SyncOperation::Flush);

        let merge = SyncArgs {
            merge: true,
            ..SyncArgs::default()
        };
        assert_eq!(sync_operation(&merge), SyncOperation::Merge);

        let import = SyncArgs {
            import_only: true,
            ..SyncArgs::default()
        };
        assert_eq!(sync_operation(&import), SyncOperation::Import);
    }

    #[test]
    fn test_sync_operation_status_is_explicit_read_only_mode() {
        let args = SyncArgs {
            status: true,
            ..SyncArgs::default()
        };

        validate_sync_mode_args(&args).unwrap();
        assert_eq!(sync_operation(&args), SyncOperation::Status);
    }

    #[test]
    fn test_should_defer_jsonl_recovery_only_for_rename_prefix_import() {
        let rename_import = SyncArgs {
            import_only: true,
            rename_prefix: true,
            ..SyncArgs::default()
        };
        assert!(should_defer_jsonl_recovery(&rename_import));

        let bare_rename = SyncArgs {
            rename_prefix: true,
            ..SyncArgs::default()
        };
        assert!(!should_defer_jsonl_recovery(&bare_rename));

        let status = SyncArgs {
            status: true,
            rename_prefix: true,
            ..SyncArgs::default()
        };
        assert!(!should_defer_jsonl_recovery(&status));

        let flush = SyncArgs {
            flush_only: true,
            rename_prefix: true,
            ..SyncArgs::default()
        };
        assert!(!should_defer_jsonl_recovery(&flush));

        let merge = SyncArgs {
            merge: true,
            rename_prefix: true,
            ..SyncArgs::default()
        };
        assert!(!should_defer_jsonl_recovery(&merge));
    }

    #[test]
    fn sync_status_fast_open_miss_reuses_caller_write_lock_for_rebuild() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let issue = make_test_issue("bd-sync-selflock", "Recovered while caller holds lock");
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();
        let _held_lock = crate::sync::blocking_write_lock(&beads_dir).unwrap();
        let args = SyncArgs {
            status: true,
            ..SyncArgs::default()
        };
        let cli = CliOverrides {
            db: Some(db_path.clone()),
            lock_timeout: Some(1),
            read_only_fast_open: true,
            ..CliOverrides::default()
        };

        let startup = prepare_sync_startup(&args, &cli, true)
            .expect("caller-held write lock should not be reacquired on fast-open miss");

        assert!(db_path.is_file(), "missing DB should rebuild from JSONL");
        assert!(
            startup
                .open_result
                .storage
                .id_exists("bd-sync-selflock")
                .unwrap()
        );
    }

    #[test]
    fn test_porcelain_cleanliness_maps_status_columns() {
        // Clean (no porcelain output).
        assert_eq!(porcelain_cleanliness(""), (true, true));
        // Untracked: nothing staged, but the content is invisible to git.
        assert_eq!(porcelain_cleanliness("?? issues.jsonl"), (true, false));
        // Worktree-dirty tracked file.
        assert_eq!(porcelain_cleanliness(" M issues.jsonl"), (true, false));
        // Staged change, clean worktree.
        assert_eq!(porcelain_cleanliness("M  issues.jsonl"), (false, true));
        // Staged + worktree-dirty.
        assert_eq!(porcelain_cleanliness("MM issues.jsonl"), (false, false));
        // Newly added to the index.
        assert_eq!(porcelain_cleanliness("A  issues.jsonl"), (false, true));
    }

    #[test]
    fn test_git_export_status_unavailable_outside_git_repo() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join("issues.jsonl");
        fs::write(&jsonl_path, "{\"id\":\"bd-x\"}\n").unwrap();

        let status = git_export_status(&jsonl_path);
        assert!(!status.available, "{status:?}");
        assert!(status.tracked.is_none(), "{status:?}");
        assert!(status.worktree_clean.is_none(), "{status:?}");
        assert!(status.index_clean.is_none(), "{status:?}");
        assert!(status.head_hash.is_none(), "{status:?}");
        assert!(status.worktree_hash.is_none(), "{status:?}");

        // The unavailable shape serializes to exactly {"available": false}.
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json, serde_json::json!({ "available": false }));
    }

    #[test]
    fn test_classify_sync_status_workspace_maps_drift_to_anomaly_codes() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        // Valid SQLite header so file-state probes stay quiet.
        let storage = SqliteStorage::open(&db_path).unwrap();
        drop(storage);
        fs::write(&jsonl_path, "{\"id\":\"bd-x\"}\n").unwrap();

        let healthy = classify_sync_status_workspace(&db_path, &jsonl_path, false, false);
        assert_eq!(healthy.health.as_str(), "healthy", "{healthy:?}");
        assert!(healthy.anomalies.is_empty(), "{healthy:?}");

        let pending_export = classify_sync_status_workspace(&db_path, &jsonl_path, false, true);
        assert_eq!(pending_export.health.as_str(), "degraded");
        let audit = pending_export.audit_record("sync.status");
        assert_eq!(audit.source, "sync.status");
        assert_eq!(audit.anomaly_codes_csv(), "db_newer");

        let diverged = classify_sync_status_workspace(&db_path, &jsonl_path, true, true);
        assert_eq!(diverged.health.as_str(), "degraded");
        assert_eq!(
            diverged.audit_record("sync.status").anomaly_codes_csv(),
            "jsonl_newer,db_newer"
        );
    }

    #[test]
    fn test_classify_sync_status_workspace_conflict_markers_are_unsafe() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let storage = SqliteStorage::open(&db_path).unwrap();
        drop(storage);
        fs::write(
            &jsonl_path,
            "<<<<<<< HEAD\n{\"id\":\"bd-a\"}\n=======\n{\"id\":\"bd-b\"}\n>>>>>>> theirs\n",
        )
        .unwrap();

        let classification = classify_sync_status_workspace(&db_path, &jsonl_path, false, false);
        assert_eq!(
            classification.health.as_str(),
            "unsafe",
            "{classification:?}"
        );
        assert!(
            classification
                .audit_record("sync.status")
                .anomaly_codes_csv()
                .contains("jsonl_conflict_markers"),
            "{classification:?}"
        );
    }

    #[test]
    fn test_sync_status_empty_db() {
        let storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let _jsonl_path = temp_dir.path().join("issues.jsonl");

        // Execute status (would need to serialize manually for test)
        let dirty_ids = storage.get_dirty_issue_ids().unwrap();
        assert!(dirty_ids.is_empty());
    }

    #[test]
    fn test_sync_status_with_dirty_issues() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue = make_test_issue("bd-test", "Test issue");
        storage.create_issue(&issue, "test").unwrap();

        let dirty_ids = storage.get_dirty_issue_ids().unwrap();
        assert!(!dirty_ids.is_empty());
    }

    #[test]
    fn test_restore_tombstones_preserves_relations_and_marks_dirty() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let keep = make_test_issue("bd-keep", "Keep");
        let delete = make_test_issue("bd-delete", "Delete");
        storage.create_issue(&keep, "test").unwrap();
        storage.create_issue(&delete, "test").unwrap();
        storage.add_label("bd-delete", "urgent", "test").unwrap();
        storage
            .add_comment("bd-delete", "test", "preserve this comment")
            .unwrap();
        storage
            .add_dependency("bd-delete", "bd-keep", "blocks", "test")
            .unwrap();
        storage
            .delete_issue("bd-delete", "test", "deleted for rebuild", None)
            .unwrap();

        let tombstones = snapshot_tombstones(&storage);
        assert_eq!(tombstones.len(), 1);
        assert_eq!(tombstones[0].issue.id, "bd-delete");
        assert_eq!(
            tombstones[0].labels.as_ref().unwrap(),
            &vec!["urgent".to_string()]
        );
        assert_eq!(tombstones[0].comments.as_ref().unwrap().len(), 1);
        assert_eq!(tombstones[0].dependencies.as_ref().unwrap().len(), 1);
        assert_eq!(
            tombstones[0].dependencies.as_ref().unwrap()[0].depends_on_id,
            "bd-keep"
        );

        storage.reset_data_tables().unwrap();
        storage.upsert_issue_for_import(&keep).unwrap();
        restore_tombstones(&mut storage, &tombstones).unwrap();

        let restored = storage.get_issue("bd-delete").unwrap().unwrap();
        assert_eq!(restored.status, Status::Tombstone);
        assert_eq!(
            storage.get_labels("bd-delete").unwrap(),
            vec!["urgent".to_string()]
        );
        assert_eq!(storage.get_comments("bd-delete").unwrap().len(), 1);
        let dependencies = storage.get_dependencies_full("bd-delete").unwrap();
        assert_eq!(dependencies.len(), 1);
        assert_eq!(dependencies[0].depends_on_id, "bd-keep");

        let dirty_ids = storage.get_dirty_issue_ids().unwrap();
        assert_eq!(dirty_ids, vec!["bd-delete".to_string()]);
    }

    #[test]
    fn test_restore_tombstones_rolls_back_when_relation_restore_fails() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let keep = make_test_issue("bd-keep", "Keep");
        let issue = make_test_issue("bd-delete", "Delete");
        storage.create_issue(&keep, "test").unwrap();
        storage.create_issue(&issue, "test").unwrap();
        storage.add_label("bd-delete", "urgent", "test").unwrap();
        storage
            .add_comment("bd-delete", "test", "preserve this comment")
            .unwrap();
        storage
            .add_dependency("bd-delete", "bd-keep", "blocks", "test")
            .unwrap();
        storage
            .delete_issue("bd-delete", "test", "deleted for rebuild", None)
            .unwrap();

        let tombstones = snapshot_tombstones(&storage);

        storage.reset_data_tables().unwrap();
        storage.upsert_issue_for_import(&keep).unwrap();
        storage.execute_raw("DROP TABLE comments").unwrap();

        let err = restore_tombstones(&mut storage, &tombstones).unwrap_err();
        assert!(
            err.to_string().contains("comments"),
            "unexpected restore failure: {err}"
        );
        assert!(storage.get_issue("bd-delete").unwrap().is_none());
        assert!(storage.get_labels("bd-delete").unwrap().is_empty());
        assert!(
            storage
                .get_dependencies_full("bd-delete")
                .unwrap()
                .is_empty()
        );
        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
    }

    #[test]
    fn test_restore_tombstones_restores_dependencies_between_preserved_tombstones() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let first = make_test_issue("bd-first", "First");
        let second = make_test_issue("bd-second", "Second");
        storage.create_issue(&first, "test").unwrap();
        storage.create_issue(&second, "test").unwrap();
        storage
            .add_dependency("bd-first", "bd-second", "blocks", "test")
            .unwrap();
        storage
            .delete_issue("bd-first", "test", "deleted for rebuild", None)
            .unwrap();
        storage
            .delete_issue("bd-second", "test", "deleted for rebuild", None)
            .unwrap();

        let tombstones = snapshot_tombstones(&storage);

        storage.reset_data_tables().unwrap();
        restore_tombstones(&mut storage, &tombstones).unwrap();

        let dependencies = storage.get_dependencies_full("bd-first").unwrap();
        assert_eq!(dependencies.len(), 1);
        assert_eq!(dependencies[0].depends_on_id, "bd-second");
        let mut dirty_ids = storage.get_dirty_issue_ids().unwrap();
        dirty_ids.sort();
        assert_eq!(
            dirty_ids,
            vec!["bd-first".to_string(), "bd-second".to_string()]
        );
    }

    #[test]
    fn test_tombstones_missing_from_jsonl_tombstones_only_skips_already_flushed_deletions() {
        let in_jsonl = PreservedTombstone {
            issue: make_test_issue("bd-in-jsonl", "in jsonl"),
            labels: Some(vec!["jsonl".to_string()]),
            dependencies: Some(Vec::new()),
            comments: Some(Vec::new()),
        };
        let missing = PreservedTombstone {
            issue: make_test_issue("bd-missing", "missing"),
            labels: Some(vec!["local".to_string()]),
            dependencies: Some(Vec::new()),
            comments: Some(Vec::new()),
        };

        let filter = crate::sync::JsonlTombstoneFilter {
            tombstone_ids: HashSet::from(["bd-in-jsonl".to_string()]),
            non_tombstone_updated_at: std::collections::HashMap::new(),
        };
        let filtered =
            tombstones_missing_from_jsonl_tombstones(vec![in_jsonl, missing.clone()], &filter);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].issue.id, "bd-missing");
        assert_eq!(filtered[0].labels, missing.labels);
        assert_eq!(filtered[0].dependencies, missing.dependencies);
        assert_eq!(filtered[0].comments, missing.comments);
    }

    #[test]
    fn test_tombstones_missing_from_jsonl_tombstones_blocks_resurrection() {
        // Regression: when the JSONL has an ID as a *non*-tombstone, the
        // preserved tombstone must still overwrite the imported open row.
        // Timestamp ordering cannot resurrect a tombstone; that requires
        // an explicit reopen operation.
        use crate::model::Status;
        use chrono::{Duration, Utc};

        let jsonl_updated_at = Utc::now();
        let mut old_local_tombstone = make_test_issue("bd-contested-older", "older local delete");
        old_local_tombstone.status = Status::Tombstone;
        old_local_tombstone.deleted_at = Some(jsonl_updated_at - Duration::hours(1));
        let old_local_preserved = PreservedTombstone {
            issue: old_local_tombstone,
            labels: None,
            dependencies: None,
            comments: None,
        };

        let mut new_local_tombstone = make_test_issue("bd-contested-newer", "newer local delete");
        new_local_tombstone.status = Status::Tombstone;
        new_local_tombstone.deleted_at = Some(jsonl_updated_at + Duration::hours(1));
        let new_local_preserved = PreservedTombstone {
            issue: new_local_tombstone,
            labels: None,
            dependencies: None,
            comments: None,
        };

        let mut non_tombstone_map = std::collections::HashMap::new();
        non_tombstone_map.insert("bd-contested-older".to_string(), jsonl_updated_at);
        non_tombstone_map.insert("bd-contested-newer".to_string(), jsonl_updated_at);

        let filter = crate::sync::JsonlTombstoneFilter {
            tombstone_ids: HashSet::new(),
            non_tombstone_updated_at: non_tombstone_map,
        };

        let filtered = tombstones_missing_from_jsonl_tombstones(
            vec![old_local_preserved, new_local_preserved],
            &filter,
        );

        assert_eq!(filtered.len(), 2);
        let filtered_ids: HashSet<_> = filtered
            .iter()
            .map(|tombstone| tombstone.issue.id.as_str())
            .collect();
        assert!(filtered_ids.contains("bd-contested-older"));
        assert!(filtered_ids.contains("bd-contested-newer"));
    }

    #[test]
    fn test_scan_jsonl_for_tombstone_filter_rejects_duplicate_issue_ids() {
        let temp_dir = tempfile::tempdir().unwrap();
        let jsonl_path = temp_dir.path().join("duplicate-tombstones.jsonl");
        let mut first = make_test_issue("bd-dup", "first");
        first.status = Status::Tombstone;
        let second = make_test_issue("bd-dup", "second");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let err = scan_jsonl_for_tombstone_filter(&jsonl_path).unwrap_err();
        assert!(
            matches!(
                &err,
                BeadsError::Config(message)
                    if message.contains("Duplicate issue id 'bd-dup'")
            ),
            "expected duplicate-id config error, got {err:?}"
        );
    }

    #[test]
    fn test_snapshot_tombstones_tolerates_broken_relation_tables() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue = make_test_issue("bd-delete", "Delete");
        storage.create_issue(&issue, "test").unwrap();
        storage
            .delete_issue("bd-delete", "test", "deleted for rebuild", None)
            .unwrap();

        storage.execute_raw("DROP TABLE comments").unwrap();
        storage.execute_raw("DROP TABLE labels").unwrap();
        storage.execute_raw("DROP TABLE dependencies").unwrap();

        let tombstones = snapshot_tombstones(&storage);
        assert_eq!(tombstones.len(), 1);
        assert_eq!(tombstones[0].issue.id, "bd-delete");
        assert_eq!(tombstones[0].issue.status, Status::Tombstone);
        assert!(tombstones[0].labels.is_none());
        assert!(tombstones[0].dependencies.is_none());
        assert!(tombstones[0].comments.is_none());
    }

    #[test]
    fn test_snapshot_tombstones_ignores_malformed_non_tombstone_rows() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let open_issue = make_test_issue("bd-open", "Open");
        let delete_issue = make_test_issue("bd-delete", "Delete");
        storage.create_issue(&open_issue, "test").unwrap();
        storage.create_issue(&delete_issue, "test").unwrap();
        storage
            .delete_issue("bd-delete", "test", "deleted for rebuild", None)
            .unwrap();

        storage
            .execute_raw("UPDATE issues SET updated_at = 'not-a-datetime' WHERE id = 'bd-open'")
            .unwrap();

        let tombstones = snapshot_tombstones(&storage);
        assert_eq!(tombstones.len(), 1);
        assert_eq!(tombstones[0].issue.id, "bd-delete");
        assert_eq!(tombstones[0].issue.status, Status::Tombstone);
    }

    #[test]
    fn test_snapshot_tombstones_tolerates_missing_issues_table() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue = make_test_issue("bd-delete", "Delete");
        storage.create_issue(&issue, "test").unwrap();
        storage
            .delete_issue("bd-delete", "test", "deleted for rebuild", None)
            .unwrap();

        storage.execute_raw("DROP TABLE issues").unwrap();

        let tombstones = snapshot_tombstones(&storage);
        assert!(tombstones.is_empty());
    }

    #[test]
    fn test_validate_sync_paths_allows_missing_internal_parent_directory() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let jsonl_path = beads_dir.join("nested").join("issues.jsonl");
        let policy = validate_sync_paths(&beads_dir, &jsonl_path, false).expect("path policy");

        assert_eq!(policy.jsonl_path, jsonl_path);
        assert!(!policy.is_external);
        assert!(!policy.allow_external_jsonl);
    }

    #[test]
    fn test_validate_sync_paths_allows_missing_external_parent_directory_with_opt_in() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let jsonl_path = temp
            .path()
            .join("external")
            .join("nested")
            .join("issues.jsonl");
        let policy = validate_sync_paths(&beads_dir, &jsonl_path, true).expect("path policy");

        assert_eq!(policy.jsonl_path, jsonl_path);
        assert!(policy.is_external);
        assert!(policy.allow_external_jsonl);
    }

    #[test]
    fn test_validate_sync_paths_allows_external_db_family_effective_policy() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();

        let db_path = external_dir.join("beads.db");
        let jsonl_path = external_dir.join("issues.jsonl");
        let allow_external_jsonl =
            config::implicit_external_jsonl_allowed(&beads_dir, &db_path, &jsonl_path);
        assert!(allow_external_jsonl);

        let policy = validate_sync_paths(&beads_dir, &jsonl_path, allow_external_jsonl)
            .expect("path policy");

        assert_eq!(policy.jsonl_path, jsonl_path);
        assert!(policy.is_external);
        assert!(policy.allow_external_jsonl);
    }

    #[test]
    fn test_validate_sync_paths_rejects_external_path_without_effective_policy() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();

        let jsonl_path = external_dir.join("issues.jsonl");
        let err = validate_sync_paths(&beads_dir, &jsonl_path, false).unwrap_err();

        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let message = if let BeadsError::Config(message) = &err {
            message.as_str()
        } else {
            ""
        };
        assert!(
            message.contains("--allow-external-jsonl"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn test_validate_operator_requested_sync_path_rejects_git_before_resolution() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let err =
            validate_operator_requested_sync_path(&beads_dir, Path::new(".git/../issues.jsonl"))
                .unwrap_err();

        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let message = if let BeadsError::Config(message) = &err {
            message.as_str()
        } else {
            ""
        };
        assert!(
            message.contains(".git") || message.contains("git"),
            "unexpected message: {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_sync_paths_rejects_internal_parent_symlink_escape_with_opt_in() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();

        let symlink_parent = beads_dir.join("external-link");
        symlink(&external_dir, &symlink_parent).unwrap();

        let jsonl_path = symlink_parent.join("issues.jsonl");
        let err = validate_sync_paths(&beads_dir, &jsonl_path, true).unwrap_err();

        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let message = if let BeadsError::Config(message) = &err {
            message.as_str()
        } else {
            ""
        };
        assert!(message.contains("symlink"), "unexpected message: {message}");
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_sync_paths_rejects_symlinked_git_parent_with_opt_in() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let git_dir = temp.path().join(".git");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&git_dir).unwrap();

        let git_link = temp.path().join("git-link");
        symlink(&git_dir, &git_link).unwrap();

        let jsonl_path = git_link.join("issues.jsonl");
        let err = validate_sync_paths(&beads_dir, &jsonl_path, true).unwrap_err();

        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let message = if let BeadsError::Config(message) = &err {
            message.as_str()
        } else {
            ""
        };
        assert!(
            message.contains(".git") || message.contains("git"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn test_validate_sync_paths_rejects_traversal_for_missing_external_parent() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let traversal_path = PathBuf::from("../outside/issues.jsonl");
        let err = validate_sync_paths(&beads_dir, &traversal_path, true).unwrap_err();

        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let message = if let BeadsError::Config(message) = &err {
            message.as_str()
        } else {
            ""
        };
        assert!(
            message.contains("traversal"),
            "unexpected message: {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_sync_paths_rejects_symlinked_external_jsonl_with_opt_in() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let outside_target = temp.path().join("outside.jsonl");
        fs::write(&outside_target, "{}\n").unwrap();

        let symlink_path = temp.path().join("linked.jsonl");
        symlink(&outside_target, &symlink_path).unwrap();

        let err = validate_sync_paths(&beads_dir, &symlink_path, true).unwrap_err();

        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let message = if let BeadsError::Config(message) = &err {
            message.as_str()
        } else {
            ""
        };
        assert!(message.contains("symlink"), "unexpected message: {message}");
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_sync_paths_rejects_git_symlinked_jsonl_even_with_opt_in() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let git_dir = temp.path().join(".git");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&git_dir).unwrap();

        let outside_target = temp.path().join("outside.jsonl");
        fs::write(&outside_target, "{}\n").unwrap();

        let git_link = git_dir.join("linked.jsonl");
        symlink(&outside_target, &git_link).unwrap();

        let err = validate_sync_paths(&beads_dir, &git_link, true).unwrap_err();

        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let message = if let BeadsError::Config(message) = &err {
            message.as_str()
        } else {
            ""
        };
        assert!(
            message.contains(".git") || message.contains("git"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn test_detect_prefix_from_jsonl_supports_hyphenated_prefixes() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join("issues.jsonl");
        let issue = make_test_issue("document-intelligence-0sa", "Hyphenated Prefix");
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        assert_eq!(
            detect_prefix_from_jsonl(&jsonl_path).unwrap(),
            Some("document-intelligence".to_string())
        );
    }

    #[test]
    fn test_detect_prefix_from_jsonl_rejects_malformed_before_prefix() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join("issues.jsonl");
        let issue = make_test_issue("foreign-0sa", "Foreign Prefix");
        fs::write(
            &jsonl_path,
            format!("{{not-json\n{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        let err = detect_prefix_from_jsonl(&jsonl_path).unwrap_err();
        assert!(
            matches!(err, BeadsError::Config(ref message) if message.contains("Invalid JSON at line 1")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_detect_prefix_from_jsonl_validates_entire_file_before_returning_prefix() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join("issues.jsonl");
        let issue = make_test_issue("foreign-0sa", "Foreign Prefix");
        fs::write(
            &jsonl_path,
            format!("{}\n{{not-json\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        let err = detect_prefix_from_jsonl(&jsonl_path).unwrap_err();
        assert!(
            matches!(err, BeadsError::Config(ref message) if message.contains("Invalid JSON at line 2")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_auto_rebuild_semantic_flag_conflict_reason_absent_for_default_import_semantics() {
        let args = SyncArgs::default();
        assert!(
            auto_rebuild_semantic_flag_conflict_reason(&args, &CliOverrides::default(), None)
                .is_none()
        );
    }

    #[test]
    fn test_auto_rebuild_semantic_flag_conflict_reason_mentions_rename_prefix_rerun() {
        let args = SyncArgs {
            force: true,
            rename_prefix: true,
            ..SyncArgs::default()
        };

        let reason =
            auto_rebuild_semantic_flag_conflict_reason(&args, &CliOverrides::default(), None)
                .expect("rename-prefix conflict");
        assert!(reason.contains("`--rename-prefix`"), "reason: {reason}");
        assert!(
            reason.contains("`br sync --import-only --force --rename-prefix`"),
            "reason: {reason}"
        );
    }

    #[test]
    fn test_auto_rebuild_semantic_flag_conflict_reason_ignores_orphans_only_request() {
        let args = SyncArgs {
            rebuild: true,
            orphans: Some("resurrect".to_string()),
            ..SyncArgs::default()
        };

        assert!(
            auto_rebuild_semantic_flag_conflict_reason(&args, &CliOverrides::default(), None)
                .is_none()
        );
    }

    #[test]
    fn test_auto_rebuild_semantic_flag_conflict_reason_mentions_both_flags() {
        let args = SyncArgs {
            force: true,
            rebuild: true,
            rename_prefix: true,
            orphans: Some("skip".to_string()),
            ..SyncArgs::default()
        };

        let reason =
            auto_rebuild_semantic_flag_conflict_reason(&args, &CliOverrides::default(), None)
                .expect("combined conflict");
        assert!(reason.contains("`--rename-prefix`"), "reason: {reason}");
        assert!(
            reason.contains("`br sync --import-only --force --rebuild --rename-prefix`"),
            "reason: {reason}"
        );
    }

    #[test]
    fn test_auto_rebuild_semantic_flag_conflict_reason_preserves_custom_db_override() {
        let args = SyncArgs {
            force: true,
            rename_prefix: true,
            ..SyncArgs::default()
        };

        let custom_db = Path::new("/tmp/custom db.sqlite");
        let reason = auto_rebuild_semantic_flag_conflict_reason(
            &args,
            &CliOverrides::default(),
            Some(custom_db),
        )
        .expect("rename-prefix conflict");
        assert!(
            reason.contains(
                "`br --db '/tmp/custom db.sqlite' sync --import-only --force --rename-prefix`"
            ),
            "reason: {reason}"
        );
    }

    #[test]
    fn test_auto_rebuild_semantic_flag_conflict_reason_preserves_external_jsonl_flag() {
        let args = SyncArgs {
            force: true,
            rename_prefix: true,
            allow_external_jsonl: true,
            ..SyncArgs::default()
        };

        let reason =
            auto_rebuild_semantic_flag_conflict_reason(&args, &CliOverrides::default(), None)
                .expect("rename-prefix conflict");
        assert!(
            reason
                .contains("`br sync --import-only --allow-external-jsonl --force --rename-prefix`"),
            "reason: {reason}"
        );
    }

    #[test]
    fn test_auto_rebuild_semantic_flag_conflict_reason_preserves_cli_startup_flags() {
        let args = SyncArgs {
            force: true,
            rename_prefix: true,
            ..SyncArgs::default()
        };
        let cli = CliOverrides {
            json: Some(true),
            allow_stale: Some(true),
            no_auto_import: Some(true),
            no_auto_flush: Some(true),
            lock_timeout: Some(17),
            ..CliOverrides::default()
        };

        let reason = auto_rebuild_semantic_flag_conflict_reason(&args, &cli, None)
            .expect("rename-prefix conflict");
        assert!(
            reason.contains(
                "`br --json --allow-stale --no-auto-import --no-auto-flush --lock-timeout 17 sync --import-only --force --rename-prefix`"
            ),
            "reason: {reason}"
        );
    }

    #[test]
    fn test_auto_rebuild_semantic_conflict_field_prefers_explicit_rebuild_then_force() {
        let plain = SyncArgs {
            rename_prefix: true,
            ..SyncArgs::default()
        };
        assert_eq!(
            auto_rebuild_semantic_conflict_field(&plain),
            "rename_prefix"
        );

        let force = SyncArgs {
            force: true,
            rename_prefix: true,
            ..SyncArgs::default()
        };
        assert_eq!(auto_rebuild_semantic_conflict_field(&force), "force");

        let rebuild = SyncArgs {
            force: true,
            rebuild: true,
            rename_prefix: true,
            ..SyncArgs::default()
        };
        assert_eq!(auto_rebuild_semantic_conflict_field(&rebuild), "rebuild");
    }

    #[test]
    fn test_jsonl_contains_prefix_mismatch_only_for_non_tombstone_ids() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join("issues.jsonl");

        let matching = make_test_issue("bd-alpha", "Matching");
        let mut tombstone = make_test_issue("other-beta", "Tombstone mismatch");
        tombstone.status = Status::Tombstone;

        fs::write(
            &jsonl_path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&matching).unwrap(),
                serde_json::to_string(&tombstone).unwrap()
            ),
        )
        .unwrap();

        assert!(!jsonl_contains_prefix_mismatch(&jsonl_path, "bd").unwrap());

        let slugged = make_test_issue("bd-survey-my-thing-abc123", "Slugged");
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&slugged).unwrap()),
        )
        .unwrap();

        assert!(
            !jsonl_contains_prefix_mismatch(&jsonl_path, "bd").unwrap(),
            "slugged IDs generated from prefix bd should not be treated as mismatches"
        );

        let mismatch = make_test_issue("other-gamma", "Mismatch");
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&mismatch).unwrap()),
        )
        .unwrap();

        assert!(jsonl_contains_prefix_mismatch(&jsonl_path, "bd").unwrap());
    }

    #[test]
    fn test_jsonl_contains_duplicate_external_refs_detects_duplicates() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join("issues.jsonl");

        let mut first = make_test_issue("bd-alpha", "First");
        first.external_ref = Some("EXT-123".to_string());
        let mut second = make_test_issue("bd-beta", "Second");
        second.external_ref = Some("EXT-123".to_string());

        fs::write(
            &jsonl_path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&first).unwrap(),
                serde_json::to_string(&second).unwrap()
            ),
        )
        .unwrap();

        assert!(jsonl_contains_duplicate_external_refs(&jsonl_path).unwrap());

        second.external_ref = Some("EXT-456".to_string());
        fs::write(
            &jsonl_path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&first).unwrap(),
                serde_json::to_string(&second).unwrap()
            ),
        )
        .unwrap();

        assert!(!jsonl_contains_duplicate_external_refs(&jsonl_path).unwrap());
    }
}
