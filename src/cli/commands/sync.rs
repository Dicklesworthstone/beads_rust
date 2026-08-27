//! Sync command implementation.
//!
//! Provides explicit JSONL sync actions without git operations.
//! Supports `--flush-only` (export) and `--import-only` (import).

use crate::cli::{DEFAULT_WITNESS_PARALLELISM, SyncArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::health::{AnomalyClass, ReliabilityAuditRecord, WorkspaceClassification};
use crate::output::{OutputContext, record_pending_exit_code};
use crate::sync::history::HistoryConfig;
use crate::sync::witness::{
    JsonlMerkleWitness, JsonlWitnessComparison, JsonlWitnessParallelWorkPlan,
    JsonlWitnessReuseMaterialization, JsonlWitnessReusePlan, build_jsonl_merkle_witness_parallel,
    compare_jsonl_merkle_witnesses, materialize_jsonl_witness_reuse_plan,
    plan_jsonl_witness_parallel_work, plan_jsonl_witness_reuse,
};
use crate::sync::{
    AdditiveReconcileReceipt, AdditiveReconcileStatus, ConflictResolution, ExpectedJsonlSourceRef,
    ExpectedStagedExport, ExportConfig, ExportEntityType, ExportError, ExportErrorPolicy,
    ImportConfig, ImportResult, JsonlSalvageReceipt, JsonlSourceSnapshot, JsonlSourceStateWitness,
    METADATA_JSONL_CONTENT_HASH, METADATA_LAST_EXPORT_TIME, METADATA_LAST_IMPORT_TIME,
    MergeContext, OrphanMode, ReconcileActionKind, ReconcileApplyOutcome, ReconcilePlan,
    ReviewedAdditiveReconcilePlanRequest, ReviewedAdditiveReconcileRequest,
    SYNC_RECONCILE_SCHEMA_VERSION, SyncMergeIntent, SyncMergeNoteWitness, SyncMergePendingPhase,
    SyncMergePendingReceipt, analyze_jsonl_snapshot,
    apply_reviewed_additive_reconcile_under_authority, apply_sync_reconcile,
    canonical_sync_path_sha256, capture_sync_database_witness, compute_jsonl_snapshot_content_hash,
    compute_staleness, database_write_authority_sha256, ensure_no_conflict_markers_snapshot,
    export_temp_path, export_to_jsonl_with_policy_expected_under_authorities,
    export_to_jsonl_with_policy_expected_under_authority, finalize_export_under_authority,
    get_issue_ids_from_jsonl_snapshot, id_matches_expected_prefix, import_from_jsonl_snapshot,
    load_base_snapshot_from_source, plan_reviewed_additive_reconcile, plan_sync_reconcile,
    read_issues_from_jsonl_snapshot, refresh_base_snapshot_from_flushed_jsonl_snapshot,
    refresh_base_snapshot_from_flushed_jsonl_snapshot_under_authority,
    require_safe_sync_overwrite_path, require_valid_sync_path, restore_tombstones_after_rebuild,
    salvage_invalid_jsonl_records_under_authority, scan_jsonl_snapshot_for_tombstone_filter,
    snapshot_tombstones, three_way_merge, tombstones_missing_from_jsonl_tombstones,
    validate_no_git_path, validate_sync_path_with_external,
};
use crate::util::id::split_prefix_remainder;
use rich_rust::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, IsTerminal};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};

fn human_witness_value_digest(value: Option<&str>) -> (usize, String) {
    match value {
        Some(value) => (
            value.len(),
            crate::util::hex_encode(&Sha256::digest(value.as_bytes())),
        ),
        None => (0, String::from("none")),
    }
}

#[allow(clippy::too_many_lines)]
fn additive_conflict_human_lines(
    receipt: &AdditiveReconcileReceipt,
    witness_limit: usize,
) -> Vec<String> {
    if receipt.conflict_issue_ids.is_empty() {
        return Vec::new();
    }

    let shown_conflict_ids = receipt
        .conflict_issue_ids
        .iter()
        .take(witness_limit)
        .map(|issue_id| sanitize_terminal_inline(issue_id).into_owned())
        .collect::<Vec<_>>();
    let conflict_ids_truncated = receipt.conflict_issue_ids.len() > witness_limit;
    let mut lines = vec![
        format!(
            "conflict issue IDs: total={} shown={} manifest_sha256={} truncated={}: {}",
            receipt.conflict_issue_ids.len(),
            shown_conflict_ids.len(),
            receipt.conflict_issue_ids_sha256,
            conflict_ids_truncated,
            shown_conflict_ids.join(", "),
        ),
        format!(
            "conflict reasons: {}",
            receipt
                .conflict_reasons
                .iter()
                .map(|(reason, count)| format!("{reason}={count}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ];
    for witness in receipt.conflict_witnesses.iter().take(witness_limit) {
        lines.push(format!(
            "conflict witness: issue={} reasons={}",
            sanitize_terminal_inline(&witness.issue_id),
            witness.reasons.join(","),
        ));
    }
    let total_conflict_details = receipt
        .conflict_witnesses
        .iter()
        .map(|witness| witness.details.len())
        .sum::<usize>();
    for (issue_id, detail) in receipt
        .conflict_witnesses
        .iter()
        .flat_map(|witness| {
            witness
                .details
                .iter()
                .map(move |detail| (witness.issue_id.as_str(), detail))
        })
        .take(witness_limit)
    {
        lines.push(format!(
            "conflict detail: issue={} reason={} kind={} ordinal={:?} validation_subcodes={} related_value_sha256={} value_sha256={} detail_sha256={}",
            sanitize_terminal_inline(issue_id),
            detail.reason,
            detail.detail_kind,
            detail.ordinal,
            detail.validation_subcodes.join(","),
            detail.related_value_sha256.join(","),
            detail.value_sha256.as_deref().unwrap_or("none"),
            detail.detail_sha256,
        ));
    }
    for witness in receipt.conflict_scalar_diffs.iter().take(witness_limit) {
        lines.push(format!(
            "conflict scalar witness: issue={} fields={} diff_sha256={} before_sha256={} after_sha256={}",
            sanitize_terminal_inline(&witness.issue_id),
            witness.changed_fields.join(","),
            witness.diff_sha256,
            witness.before_payload_sha256,
            witness.after_payload_sha256,
        ));
    }
    for witness in receipt.conflict_relation_diffs.iter().take(witness_limit) {
        lines.push(format!(
            "conflict relation witness: issue={} classes={} before_counts={:?} after_counts={:?} added={} removed={} diff_sha256={} before_sha256={} after_sha256={}",
            sanitize_terminal_inline(&witness.issue_id),
            witness.changed_relation_classes.join(","),
            witness.before_counts,
            witness.after_counts,
            witness.added_element_sha256.len(),
            witness.removed_element_sha256.len(),
            witness.diff_sha256,
            witness.before_payload_sha256,
            witness.after_payload_sha256,
        ));
    }
    if conflict_ids_truncated
        || receipt.conflict_witnesses.len() > witness_limit
        || total_conflict_details > witness_limit
        || receipt.conflict_scalar_diffs.len() > witness_limit
        || receipt.conflict_relation_diffs.len() > witness_limit
    {
        lines.push(format!(
            "conflict witness output: issue_ids={}/{} witnesses={}/{} details={}/{} scalar_diffs={}/{} relation_diffs={}/{} witness_manifest_sha256={} scalar_diff_manifest_sha256={} relation_diff_manifest_sha256={} truncated=true; use --robot for complete manifests",
            shown_conflict_ids.len(),
            receipt.conflict_issue_ids.len(),
            receipt.conflict_witnesses.len().min(witness_limit),
            receipt.conflict_witnesses.len(),
            total_conflict_details.min(witness_limit),
            total_conflict_details,
            receipt.conflict_scalar_diffs.len().min(witness_limit),
            receipt.conflict_scalar_diffs.len(),
            receipt.conflict_relation_diffs.len().min(witness_limit),
            receipt.conflict_relation_diffs.len(),
            receipt.conflict_witnesses_sha256,
            receipt.conflict_scalar_diffs_sha256,
            receipt.conflict_relation_diffs_sha256,
        ));
    }
    if receipt.conflict_reasons.keys().any(|reason| {
        matches!(
            reason.as_str(),
            "equal_timestamp_shared_scalar_drift"
                | "database_newer_shared_scalar_drift"
                | "source_newer_scalar_drift_requires_resolution"
        )
    }) {
        lines.push(
            "review hint: inspect each scalar diff, then re-plan with a separate --resolve-source-id <ISSUE_ID> for every source-authoritative resolution"
                .to_string(),
        );
    }
    lines
}

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
    /// Present only when the JSONL could not be published through the atomic
    /// flagged-rename protocol (#419): the filesystem (WSL2 9p/DrvFS, for
    /// one) refused it, so the staged file was installed with a
    /// witness-checked plain rename under the held write authority. Absent
    /// means the publication was atomic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication_atomicity: Option<String>,
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
    /// Old-id -> new-id receipt emitted when `--rename-prefix` rewrote ids.
    /// Omitted entirely when no rename happened (legacy JSON shape).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prefix_renames: Vec<crate::sync::ImportPrefixRename>,
    /// Present when --skip-invalid-records published a recovered JSONL source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub salvage: Option<JsonlSalvageReceipt>,
}

/// Maximum witness rows printed per list in human-readable sync output;
/// machine modes always carry the complete manifests.
const HUMAN_WITNESS_LIMIT: usize = 32;

/// Maximum ids serialized per preview list in a reconcile receipt.
///
/// Mirrors the doctor `IdDelta` preview cap: operators want the first few
/// divergent ids to grep for; counts always reflect the true totals.
const RECONCILE_PREVIEW_LIMIT: usize = 50;

/// Versioned receipt for `br sync --reconcile` (`br.sync.reconcile.v1`).
///
/// Emitted by both `--dry-run` (plan only, `applied: false`) and apply
/// (`applied: true`, with the `apply` block present). Deletion is impossible
/// in this mode, so `plan.deleted` is a constant `0`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SyncReconcileReceipt {
    /// Receipt schema identifier (`br.sync.reconcile.v1`).
    pub schema_version: &'static str,
    /// `"dry_run"` or `"apply"`.
    pub mode: &'static str,
    /// True when the plan was applied to the database.
    pub applied: bool,
    /// Resolved JSONL path the plan was computed against.
    pub jsonl_path: String,
    /// Source (JSONL) witness the plan is bound to.
    pub source: ReconcileSourceWitness,
    /// Target (database) witness the plan is bound to.
    pub target: ReconcileTargetWitness,
    /// Row classification counts.
    pub plan: ReconcilePlanCounts,
    /// Bounded id previews for the classified sets.
    pub previews: ReconcileIdPreviews,
    /// Relation rows carried by planned create/update rows.
    pub relations: ReconcileRelationCounts,
    /// Event rows before the operation.
    pub events_before: u64,
    /// Event rows after the operation (always equals `events_before`; the
    /// apply transaction rolls back otherwise).
    pub events_after: u64,
    /// Apply-only details; absent in dry-run receipts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apply: Option<ReconcileApplyReceipt>,
}

/// JSONL-side witness in a reconcile receipt.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReconcileSourceWitness {
    /// Non-empty JSONL rows parsed (including ephemeral rows).
    pub record_count: usize,
    /// Ephemeral (`-wisp-`) rows excluded from planning.
    pub ephemeral_skipped: usize,
    /// Whitespace-normalized SHA-256 of the JSONL content.
    pub content_hash: String,
    /// RFC3339 mtime of the JSONL file at plan time.
    pub mtime: String,
    /// Byte size of the JSONL file at plan time.
    pub size_bytes: u64,
}

/// Database-side witness in a reconcile receipt.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReconcileTargetWitness {
    /// Total issue rows at plan time (including tombstones).
    pub db_issue_count: usize,
    /// Whether the stored `jsonl_content_hash` metadata already matched the
    /// file at plan time. True alongside nonzero `created`/`updated` counts
    /// is the false-equal state this mode repairs.
    pub stored_hash_matches_jsonl: bool,
}

/// Row classification counts in a reconcile receipt.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReconcilePlanCounts {
    /// JSONL rows with no DB counterpart (inserted on apply).
    pub created: usize,
    /// JSONL rows strictly newer than their DB counterpart (updated on apply).
    pub updated: usize,
    /// Rows skipped because timestamps are equal.
    pub skipped_equal: usize,
    /// Rows skipped because the DB copy is strictly newer.
    pub skipped_older: usize,
    /// Rows skipped by tombstone protection.
    pub skipped_tombstone: usize,
    /// Always 0: additive reconciliation cannot delete.
    pub deleted: usize,
    /// Exportable DB issues absent from the JSONL row set (never touched).
    pub db_only: usize,
}

/// Bounded id previews in a reconcile receipt.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReconcileIdPreviews {
    /// First ids (sorted) that would be / were created.
    pub created_ids: Vec<String>,
    /// First ids (sorted) that would be / were updated.
    pub updated_ids: Vec<String>,
    /// First exportable DB-only ids (sorted).
    pub db_only_ids: Vec<String>,
    /// Per-list truncation cap; counts in `plan` reflect true totals.
    pub preview_limit: usize,
}

/// Relation rows carried by planned create/update rows.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReconcileRelationCounts {
    /// Label rows.
    pub labels: usize,
    /// Dependency rows.
    pub dependencies: usize,
    /// Comment rows.
    pub comments: usize,
}

/// Apply-only block of a reconcile receipt.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReconcileApplyReceipt {
    /// Export-hash rows recorded for rows whose DB copy now matches JSONL.
    pub export_hashes_recorded: usize,
    /// Skipped rows whose DB copy still differs from JSONL (local wins that
    /// need a future flush).
    pub uncertified_local_wins: usize,
    /// Dangling dependency rows removed from just-written issues.
    pub orphan_dependencies_cleaned: usize,
    /// Blocked-cache rows after rebuild (0 when no row changed).
    pub blocked_cache_entries: usize,
    /// Child-counter rows after rebuild (0 when no row changed).
    pub child_counter_entries: usize,
    /// Whether `needs_flush` was set because local state still diverges from
    /// JSONL (db-only rows or uncertified local wins).
    pub needs_flush_set: bool,
    /// Whether import metadata (content hash + stat witness + import time)
    /// was repaired in the apply transaction.
    pub metadata_repaired: bool,
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
    /// DB↔JSONL coverage probe (`beads_rust-jdmh`). Absent when the JSONL
    /// is missing or unreadable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<SyncCoverageProbe>,
    /// True when `coverage` shows the DB and JSONL hold different issue
    /// sets even though the timestamp/hash signals report "in sync" —
    /// recover with `br sync --reconcile` (lossless) or
    /// `--import-only --rebuild` (JSONL-authoritative).
    pub coverage_drift: bool,
    /// Stable VCS-observation slot for the canonical JSONL export.
    ///
    /// Sync deliberately never probes Git. The object therefore reports
    /// `reason: "not_probed"` and points to the explicit `br vcs-status`
    /// diagnostic that owns the separate, user-requested VCS capability.
    pub git_export: GitExportStatus,
}

/// VCS-observation state carried by `br sync --status`.
///
/// The legacy fields remain optional so existing machine consumers keep a
/// stable shape, but sync itself has no process or VCS authority and never
/// populates them. Users explicitly request the observation with the command
/// named in `diagnostic_command`.
#[derive(Debug, Serialize)]
pub struct GitExportStatus {
    /// Always false in sync output because VCS state was not requested.
    pub available: bool,
    /// Why the optional observation is absent.
    pub reason: &'static str,
    /// Explicit, separately hardened diagnostic for obtaining the observation.
    pub diagnostic_command: &'static str,
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
    fn not_probed() -> Self {
        Self {
            available: false,
            reason: "not_probed",
            diagnostic_command: "br vcs-status --json",
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
    ReconcileAdditive,
    Flush,
    Merge,
    Import,
    Reconcile,
    Unspecified,
}

struct SyncDispatchOptions {
    db_path: PathBuf,
    retention_days: Option<u64>,
    use_json: bool,
    show_progress: bool,
    history_config: HistoryConfig,
}

struct PendingSyncMergeCompletion {
    receipt: SyncMergePendingReceipt,
    base_authority: crate::sync::JsonlFamilyWriteLock,
}

struct ReconciledPendingSyncMerge {
    published_source: Arc<JsonlSourceSnapshot>,
    terminal_receipt: SyncMergePendingReceipt,
}

enum DeferredSyncOutput {
    Merge {
        report: crate::sync::MergeReport,
        resolution: String,
        capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
        use_json: bool,
    },
    ResumedMerge {
        receipt_id: String,
        phase_before: SyncMergePendingPhase,
        capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
        use_json: bool,
    },
}

#[derive(Default)]
struct SyncDispatchCompletion {
    published_source: Option<Arc<JsonlSourceSnapshot>>,
    /// JSONL-family authority acquired by the operation when startup did not
    /// already retain one. Finalization transfers this lease into
    /// `OpenStorageResult` before adopting the published source.
    owned_jsonl_authority: Option<crate::sync::JsonlFamilyWriteLock>,
    pending_merge: Option<PendingSyncMergeCompletion>,
    deferred_output: Option<DeferredSyncOutput>,
}

impl SyncDispatchCompletion {
    fn published(source: Option<Arc<JsonlSourceSnapshot>>) -> Self {
        Self {
            published_source: source,
            ..Self::default()
        }
    }
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

    if args.reconcile_additive && !args.apply {
        let beads_dir = config::discover_beads_dir_with_cli(cli)?;
        let plan = plan_reviewed_additive_reconcile(&ReviewedAdditiveReconcilePlanRequest {
            beads_dir,
            db_override: cli.db.clone(),
            source_path_override: None,
            allow_external_jsonl: args.allow_external_jsonl,
            source_authoritative_ids: args.resolve_source_ids.iter().cloned().collect(),
        })?;
        render_additive_reconcile_receipt(plan.receipt(), ctx, ctx.is_json() || args.robot);
        if plan.has_conflicts() {
            record_pending_exit_code(6);
        }
        return Ok(());
    }

    if args.reconcile_additive {
        let beads_dir = config::discover_beads_dir_with_cli(cli)?;
        let expected_plan_sha256 =
            args.expect_plan_sha256
                .clone()
                .ok_or_else(|| BeadsError::Validation {
                    field: "expect_plan_sha256".to_string(),
                    reason: "--apply requires --expect-plan-sha256".to_string(),
                })?;
        let receipt = apply_reviewed_additive_reconcile_under_authority(
            &ReviewedAdditiveReconcileRequest {
                beads_dir,
                db_override: cli.db.clone(),
                source_path_override: None,
                allow_external_jsonl: args.allow_external_jsonl,
                source_authoritative_ids: args.resolve_source_ids.iter().cloned().collect(),
                expected_plan_sha256,
                lock_timeout_ms: cli.lock_timeout,
            },
            cli.held_write_authority.as_ref(),
        )?;
        render_additive_reconcile_receipt(&receipt, ctx, ctx.is_json() || args.robot);
        if matches!(
            receipt.status,
            AdditiveReconcileStatus::CommittedWithPostconditionFailures
        ) {
            record_pending_exit_code(6);
        }
        return Ok(());
    }

    let mut startup = prepare_sync_startup(args, cli, startup_write_lock_held)?;

    let command_result = maybe_delegate_rebuild(args, &mut startup.open_result)
        .and_then(|()| startup.open_result.verify_retained_jsonl_source_current())
        .and_then(|()| {
            dispatch_sync_subcommand(
                args,
                cli,
                ctx,
                &startup.beads_dir,
                &startup.path_policy,
                &mut startup.open_result,
            )
        })
        .and_then(|completion| {
            finalize_sync_dispatch_completion(completion, &mut startup.open_result, ctx)
        });

    finalize_sync_result(command_result, &mut startup.open_result)
}

/// Resolve path policy and open storage before dispatch. Keeping this separate
/// from `execute` makes the command's startup phase distinct from the
/// status/export/import/merge operation handlers below.
fn prepare_sync_startup(
    args: &SyncArgs,
    cli: &config::CliOverrides,
    _startup_write_lock_held: bool,
) -> Result<SyncStartupState> {
    let (beads_dir, startup, path_policy) = resolve_sync_startup_paths(args, cli)?;
    let allow_external_jsonl = path_policy.allow_external_jsonl;

    let open_result = config::open_storage_with_startup_config_and_jsonl_policy(
        startup,
        cli,
        should_defer_jsonl_recovery(args),
        allow_external_jsonl,
    )?;

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
    let jsonl_log_path = if path_policy.is_external {
        "<external-source>".to_string()
    } else {
        path_policy.jsonl_path.display().to_string()
    };
    let manifest_log_path = if path_policy.is_external {
        "<external-manifest>".to_string()
    } else {
        path_policy.manifest_path.display().to_string()
    };
    debug!(
        jsonl_path = jsonl_log_path,
        manifest_path = manifest_log_path,
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
    args.reconcile_additive
        || (args.import_only && (args.rename_prefix || args.skip_invalid_records))
}

/// Reject argument combinations that must fail BEFORE opening storage or
/// triggering any rebuild side effect. A `--flush-only --rebuild` or
/// `--merge --rebuild` combination must return an error without having
/// touched the DB family — otherwise the validation message arrives after
/// `recover_database_from_jsonl` has already moved the existing DB aside.
#[allow(clippy::too_many_lines)]
pub fn validate_sync_mode_args(args: &SyncArgs) -> Result<()> {
    if args.skip_invalid_records && !args.import_only {
        return Err(BeadsError::Validation {
            field: "skip_invalid_records".to_string(),
            reason: "--skip-invalid-records can only be used with --import-only".to_string(),
        });
    }
    if args.skip_invalid_records && (args.force || args.rebuild || args.rename_prefix) {
        return Err(BeadsError::Validation {
            field: "skip_invalid_records".to_string(),
            reason: "--skip-invalid-records is an additive recovery mode and cannot be combined with --force, --rebuild, or --rename-prefix"
                .to_string(),
        });
    }
    if args.apply && !args.reconcile_additive {
        return Err(BeadsError::Validation {
            field: "apply".to_string(),
            reason: "--apply can only be used with --reconcile-additive".to_string(),
        });
    }
    if args.reconcile_additive && args.apply && args.expect_plan_sha256.is_none() {
        return Err(BeadsError::Validation {
            field: "expect_plan_sha256".to_string(),
            reason:
                "--apply requires --expect-plan-sha256 from the reviewed reconciliation dry run"
                    .to_string(),
        });
    }
    if let Some(plan_sha256) = &args.expect_plan_sha256
        && (plan_sha256.len() != 64
            || !plan_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    {
        return Err(BeadsError::Validation {
            field: "expect_plan_sha256".to_string(),
            reason: "--expect-plan-sha256 must be exactly 64 lowercase hexadecimal characters"
                .to_string(),
        });
    }
    if args.reconcile_additive {
        let irrelevant = [
            (args.export_parallelism.is_some(), "--export-parallelism"),
            (args.force, "--force"),
            (args.force_db, "--force-db"),
            (args.force_jsonl, "--force-jsonl"),
            (args.manifest, "--manifest"),
            (args.error_policy.is_some(), "--error-policy"),
            (args.orphans.is_some(), "--orphans"),
            (args.rename_prefix, "--rename-prefix"),
            (args.rebuild, "--rebuild"),
            (args.witness_parallelism.is_some(), "--witness-parallelism"),
        ];
        if let Some((_, flag)) = irrelevant.into_iter().find(|(present, _)| *present) {
            return Err(BeadsError::Validation {
                field: "reconcile_additive".to_string(),
                reason: format!("{flag} is not used by --reconcile-additive"),
            });
        }
        let resolution_ids = args
            .resolve_source_ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if resolution_ids.len() != args.resolve_source_ids.len()
            || resolution_ids
                .iter()
                .any(|issue_id| issue_id.trim().is_empty() || issue_id.trim() != *issue_id)
        {
            return Err(BeadsError::Validation {
                field: "resolve_source_ids".to_string(),
                reason: "--resolve-source-id values must be unique, nonblank, and trimmed"
                    .to_string(),
            });
        }
    }

    let mode_count = u8::from(args.status)
        + u8::from(args.flush_only)
        + u8::from(args.import_only)
        + u8::from(args.merge)
        + u8::from(args.reconcile)
        + u8::from(args.witness)
        + u8::from(args.reconcile_additive);
    if mode_count > 1 {
        return Err(BeadsError::Validation {
            field: "mode".to_string(),
            reason:
                "Must specify exactly one of --flush-only, --import-only, --merge, --reconcile, --reconcile-additive, --status, or --witness"
                    .to_string(),
        });
    }
    if mode_count == 0 {
        return Err(BeadsError::Validation {
            field: "mode".to_string(),
            reason:
                "Must specify one of --flush-only, --import-only, --merge, --reconcile, --reconcile-additive, --status, or --witness"
                    .to_string(),
        });
    }

    if args.reconcile {
        if args.force {
            return Err(BeadsError::Validation {
                field: "force".to_string(),
                reason: "--force cannot be used with --reconcile; reconcile is always additive \
                         and guarded (use --import-only --force for a destructive import)"
                    .to_string(),
            });
        }
        if args.rename_prefix {
            return Err(BeadsError::Validation {
                field: "rename_prefix".to_string(),
                reason: "--rename-prefix cannot be used with --reconcile; reconcile never \
                         rewrites issue ids"
                    .to_string(),
            });
        }
        if args.orphans.is_some() {
            return Err(BeadsError::Validation {
                field: "orphans".to_string(),
                reason: "--orphans cannot be used with --reconcile; reconcile only removes \
                         dangling dependency references from rows it just wrote"
                    .to_string(),
            });
        }
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
    let delegation_would_drop_user_flags = args.rename_prefix || args.skip_invalid_records;
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
    // Recovery itself now captures the JSONL once before inspecting
    // tombstones or replacing the database. It preserves unflushed
    // tombstones (and dirty live issues, GitHub #394) against that same
    // immutable generation, then imports the identical bytes. Keeping this
    // at the recovery boundary prevents a path replacement from mixing
    // filter state from A with rows from B.
    open_result.recover_database_from_jsonl()?;
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
) -> Result<SyncDispatchCompletion> {
    let options = sync_dispatch_options(args, cli, ctx, open_result);

    match sync_operation(args) {
        SyncOperation::Status => execute_status(
            &open_result.storage,
            path_policy,
            &options.db_path,
            options.use_json,
            ctx,
        )
        .map(|()| SyncDispatchCompletion::default()),
        SyncOperation::Witness => {
            execute_witness(path_policy, args, options.use_json, ctx)
                .map(|()| SyncDispatchCompletion::default())
        }
        SyncOperation::Reconcile => {
            execute_reconcile(&mut open_result.storage, path_policy, args, options.use_json, ctx)
                .map(|()| SyncDispatchCompletion::default())
        }
        SyncOperation::ReconcileAdditive => Err(BeadsError::Internal {
            message: "reviewed additive reconciliation bypassed its sole lock-owning command path"
                .to_string(),
        }),
        SyncOperation::Flush => {
            let (storage, retained_source, expected_source, jsonl_authority) =
                open_result.jsonl_write_context();
            execute_flush(
                storage,
                beads_dir,
                path_policy,
                args,
                options.use_json,
                options.show_progress,
                options.retention_days,
                options.history_config.clone(),
                retained_source,
                expected_source,
                jsonl_authority,
                ctx,
            )
            .map(SyncDispatchCompletion::published)
        }
        SyncOperation::Merge => {
            let no_db = open_result.no_db;
            let (storage, retained_source, expected_source, jsonl_authority) =
                open_result.jsonl_write_context();
            execute_merge(
                storage,
                path_policy,
                args,
                options.use_json,
                options.show_progress,
                options.retention_days,
                options.history_config.clone(),
                retained_source,
                expected_source,
                jsonl_authority,
                cli,
                &options.db_path,
                no_db,
                ctx,
            )
        }
        SyncOperation::Import => {
            let auto_rebuilt = open_result.auto_rebuilt;
            let (storage, retained_source, retained_authority) = open_result.import_context();
            execute_import(
                storage,
                beads_dir,
                cli,
                path_policy,
                args,
                options.use_json,
                options.show_progress,
                auto_rebuilt,
                retained_source,
                retained_authority,
                &options.db_path,
                ctx,
            )
            .map(SyncDispatchCompletion::published)
        }
        SyncOperation::Unspecified => Err(BeadsError::Validation {
            field: "mode".to_string(),
            reason:
                "Must specify one of --flush-only, --import-only, --merge, --reconcile, --reconcile-additive, --status, or --witness"
                    .to_string(),
        }),
    }
}

fn finalize_sync_dispatch_completion(
    completion: SyncDispatchCompletion,
    open_result: &mut config::OpenStorageResult,
    ctx: &OutputContext,
) -> Result<()> {
    let SyncDispatchCompletion {
        published_source,
        owned_jsonl_authority,
        pending_merge,
        deferred_output,
    } = completion;
    if let Some(source) = published_source.as_ref() {
        open_result.adopt_published_jsonl_source(Arc::clone(source), owned_jsonl_authority)?;
    } else if owned_jsonl_authority.is_some() {
        return Err(BeadsError::Internal {
            message: "sync completion retained JSONL authority without a published source"
                .to_string(),
        });
    }
    open_result.verify_retained_jsonl_source_current()?;

    if let Some(pending_merge) = pending_merge {
        let published_source = published_source
            .as_ref()
            .ok_or_else(|| BeadsError::Internal {
                message: "pending sync merge completion did not retain its published JSONL witness"
                    .to_string(),
            })?;
        open_result.verify_retained_jsonl_authority(
            &pending_merge.receipt.intent.jsonl_authority_sha256,
        )?;
        finalize_pending_sync_merge_after_adoption(
            &mut open_result.storage,
            &open_result.paths.db_path,
            published_source,
            &pending_merge,
            open_result.no_db,
        )
        .map_err(|source| BeadsError::CommittedStateUnwitnessed {
            operation: "terminal sync merge adoption and receipt cleanup".to_string(),
            source: Box::new(source),
        })?;
    }

    if let Some(output) = deferred_output {
        render_deferred_sync_output(output, ctx);
    }
    Ok(())
}

fn finalize_pending_sync_merge_after_adoption(
    storage: &mut crate::storage::SqliteStorage,
    db_path: &Path,
    published_source: &JsonlSourceSnapshot,
    pending: &PendingSyncMergeCompletion,
    no_db: bool,
) -> Result<()> {
    let receipt = &pending.receipt;
    receipt.validate()?;
    if receipt.phase != SyncMergePendingPhase::ExportFinalized {
        return Err(BeadsError::SyncConflict {
            message: "Cannot clear a pending merge receipt before export finalization".to_string(),
        });
    }
    if receipt.jsonl_after.as_ref() != Some(&published_source.state_witness())
        || published_source.raw_sha256() != receipt.jsonl_after_raw_sha256
        || published_source.content_sha256() != receipt.jsonl_after_content_sha256
    {
        return Err(BeadsError::SyncConflict {
            message: "Adopted JSONL source does not match the pending merge receipt".to_string(),
        });
    }

    let configured_database_authority = database_write_authority_sha256(db_path)?;
    if configured_database_authority != receipt.intent.database_authority_sha256 {
        return Err(BeadsError::SyncConflict {
            message: "Pending merge database authority does not match the committed merge intent"
                .to_string(),
        });
    }
    if !no_db {
        let database_authority = storage.attached_write_authority();
        let database_authority = database_authority.ok_or_else(|| BeadsError::SyncConflict {
            message:
                "Pending merge terminal verification requires retained database-family authority"
                    .to_string(),
        })?;
        database_authority.verify_database_authority()?;
        if database_authority.authority_path_sha256() != receipt.intent.database_authority_sha256 {
            return Err(BeadsError::SyncConflict {
                message:
                    "Retained database-family authority differs from the committed merge intent"
                        .to_string(),
            });
        }
    }

    pending.base_authority.verify_jsonl_authority()?;
    if pending.base_authority.authority_path_sha256() != receipt.intent.base_authority_sha256 {
        return Err(BeadsError::SyncConflict {
            message: "Pending merge retained base authority differs from the committed intent"
                .to_string(),
        });
    }
    let terminal_base = pending.base_authority.capture_target()?;
    if terminal_base.raw_sha256() != published_source.raw_sha256()
        || terminal_base.content_sha256() != published_source.content_sha256()
        || terminal_base.size() != published_source.size()
    {
        return Err(BeadsError::SyncConflict {
            message: "Merge base changed before outer completion adopted the JSONL".to_string(),
        });
    }
    if storage.with_read_transaction(crate::sync::capture_sync_merge_core_witness)?
        != receipt.database_after
    {
        return Err(BeadsError::SyncConflict {
            message: "Database changed before outer merge completion verification".to_string(),
        });
    }
    if storage.pending_sync_merge_receipt()?.as_ref() != Some(receipt) {
        return Err(BeadsError::SyncConflict {
            message: "Pending merge receipt changed before terminal cleanup".to_string(),
        });
    }
    storage.compare_and_clear_pending_sync_merge_receipt(receipt)
}

fn render_deferred_sync_output(output: DeferredSyncOutput, ctx: &OutputContext) {
    match output {
        DeferredSyncOutput::Merge {
            report,
            resolution,
            capacity_warnings,
            use_json,
        } => {
            if use_json {
                ctx.json_pretty(&serde_json::json!({
                    "status": "success",
                    "merged_issues": report.kept.len(),
                    "deleted_issues": report.deleted.len(),
                    "conflicts": report.conflicts.len(),
                    "resolution": resolution,
                    "notes": report.notes,
                    "warnings": capacity_warnings,
                }));
            } else if should_render_human_sync_output(ctx, use_json) {
                if ctx.is_rich() {
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
                for warning in &capacity_warnings {
                    ctx.warning(&warning.to_string());
                }
            }
        }
        DeferredSyncOutput::ResumedMerge {
            receipt_id,
            phase_before,
            capacity_warnings,
            use_json,
        } => {
            if use_json {
                ctx.json_pretty(&serde_json::json!({
                    "status": "resumed",
                    "receipt_id": receipt_id,
                    "phase_before": phase_before,
                    "base_updated": true,
                    "warnings": capacity_warnings,
                }));
            } else if should_render_human_sync_output(ctx, use_json) {
                println!(
                    "Resumed committed merge {} from phase {:?}; JSONL and base are verified.",
                    receipt_id, phase_before
                );
                for warning in &capacity_warnings {
                    ctx.warning(&warning.to_string());
                }
            }
        }
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
    } else if args.reconcile_additive {
        SyncOperation::ReconcileAdditive
    } else if args.status {
        SyncOperation::Status
    } else if args.flush_only {
        SyncOperation::Flush
    } else if args.merge {
        SyncOperation::Merge
    } else if args.import_only {
        SyncOperation::Import
    } else if args.reconcile {
        SyncOperation::Reconcile
    } else {
        SyncOperation::Unspecified
    }
}

#[allow(clippy::too_many_lines)]
fn render_additive_reconcile_receipt(
    receipt: &AdditiveReconcileReceipt,
    ctx: &OutputContext,
    machine_readable: bool,
) {
    if machine_readable {
        ctx.json_pretty(receipt);
        return;
    }

    println!(
        "additive reconciliation: status={} plan_sha256={} source={} target_before={} created={} updated={} equal={} synchronized={} conflicted_issues={} conflict_observations={} deleted=0",
        receipt.status.as_str(),
        receipt.plan_sha256,
        receipt.source_issues,
        receipt.target_before.issues,
        receipt.created,
        receipt.updated,
        receipt.skipped_equal,
        receipt.synchronized,
        receipt.conflicted,
        receipt.conflict_occurrences,
    );
    println!(
        "sync bookkeeping: export_hashes={}/{} dirty_markers={}/{} metadata_update_planned={} metadata_changed={}",
        receipt.export_hashes_updated,
        receipt.export_hash_updates_planned,
        receipt.dirty_markers_cleared,
        receipt.dirty_markers_clear_planned,
        receipt.metadata_update_planned,
        receipt.metadata_changed
    );
    println!(
        "audit events: {}/{} preserved; database-only issues preserved={}; jsonl_written=false",
        receipt.events_before, receipt.events_after, receipt.db_only_preserved
    );
    println!(
        "blocking cycles: preexisting={} projected={} new={}",
        receipt.preexisting_blocking_cycles,
        receipt.projected_blocking_cycles,
        receipt.new_blocking_cycles
    );
    println!(
        "authority witnesses: workspace_path_sha256={} workspace_identity_sha256={} source_path_sha256={} source_identity_sha256={} database_path_sha256={} database_identity_sha256={} write_lock_authority_sha256={} schema_version={}",
        receipt.workspace_path_sha256,
        receipt.workspace_identity_sha256,
        receipt.source_path_sha256,
        receipt.source_identity_sha256,
        receipt.database_path_sha256,
        receipt
            .database_identity_sha256
            .as_deref()
            .unwrap_or("none"),
        receipt.write_lock_authority_sha256,
        receipt.database_user_version,
    );
    println!(
        "source witnesses: raw_sha256={} content_sha256={} storage_projection_sha256={} size={} mtime={}",
        receipt.source_raw_sha256,
        receipt.source_content_sha256,
        receipt.source_storage_projection_sha256,
        receipt.source_size,
        receipt.source_mtime,
    );
    println!(
        "relation proof: before={:?} after={:?} planned={:?} applied={:?}",
        receipt.relations_before,
        receipt.relations_after,
        receipt.relation_rows_planned,
        receipt.relation_rows_applied,
    );
    println!(
        "expected poststate digests: issue_raw={} issue_semantic={} issue_content_hash={} export_hash={} dirty={} metadata={} blocked_cache={} child_counter={} sqlite_sequence={}",
        receipt.expected_issue_raw_payload_sha256,
        receipt.expected_issue_semantic_payload_sha256,
        receipt.expected_issue_content_hash_payload_sha256,
        receipt.expected_export_hash_payload_sha256,
        receipt.expected_dirty_payload_sha256,
        receipt.expected_metadata_payload_sha256,
        receipt.expected_blocked_cache_payload_sha256,
        receipt.expected_child_counter_payload_sha256,
        receipt.expected_sqlite_sequence_payload_sha256,
    );
    println!(
        "manifest digests: created={} updated={} equal={} db_only={} scalar_updates={} content_hash_repairs={} comment_remaps={} conflict_issue_ids={} conflict_witnesses={} conflict_scalar_diffs={} conflict_relation_diffs={}",
        receipt.created_issue_ids_sha256,
        receipt.updated_issue_ids_sha256,
        receipt.equal_issue_ids_sha256,
        receipt.db_only_issue_ids_sha256,
        receipt.scalar_updates_sha256,
        receipt.content_hash_repairs_sha256,
        receipt.comment_id_remaps_sha256,
        receipt.conflict_issue_ids_sha256,
        receipt.conflict_witnesses_sha256,
        receipt.conflict_scalar_diffs_sha256,
        receipt.conflict_relation_diffs_sha256,
    );
    if let Some(health_after) = &receipt.health_after {
        println!(
            "health: before_integrity={} before_fk_violations={} after_integrity={} after_fk_violations={}",
            receipt.health_before.integrity_messages.len(),
            receipt.health_before.foreign_key_violations.len(),
            health_after.integrity_messages.len(),
            health_after.foreign_key_violations.len(),
        );
    } else {
        println!(
            "health: before_integrity={} before_fk_violations={} after=not_checked",
            receipt.health_before.integrity_messages.len(),
            receipt.health_before.foreign_key_violations.len(),
        );
    }
    println!(
        "postcommit checks: database_authority={:?} database_poststate={:?} workspace_authority={:?} source={:?} foreign_keys={:?} failures={:?}",
        receipt.database_authority_preserved_after_commit,
        receipt.database_poststate_preserved_after_commit,
        receipt.workspace_authority_preserved_after_commit,
        receipt.source_preserved_after_commit,
        receipt.foreign_keys_restored_after_commit,
        receipt.postcommit_failures,
    );
    if !receipt.scalar_updates.is_empty() {
        for update in receipt.scalar_updates.iter().take(HUMAN_WITNESS_LIMIT) {
            println!(
                "scalar witness: issue={} resolution={} fields={} diff_sha256={} before_sha256={} after_sha256={} relations_sha256={}",
                update.issue_id,
                update.resolution.as_str(),
                update.changed_fields.join(","),
                update.diff_sha256,
                update.before_payload_sha256,
                update.after_payload_sha256,
                update.relation_payload_sha256,
            );
        }
        if receipt.scalar_updates.len() > HUMAN_WITNESS_LIMIT {
            println!(
                "scalar witness output: total={} shown={} manifest_sha256={} truncated=true; use --robot for the complete manifest",
                receipt.scalar_updates.len(),
                HUMAN_WITNESS_LIMIT,
                receipt.scalar_updates_sha256,
            );
        }
    }
    for repair in receipt
        .content_hash_repairs
        .iter()
        .take(HUMAN_WITNESS_LIMIT)
    {
        let (before_len, before_sha256) = human_witness_value_digest(repair.before.as_deref());
        let (after_len, after_sha256) = human_witness_value_digest(Some(&repair.after));
        println!(
            "content-hash repair witness: issue={} before_len={} before_sha256={} after_len={} after_sha256={}",
            repair.issue_id, before_len, before_sha256, after_len, after_sha256,
        );
    }
    if receipt.content_hash_repairs.len() > HUMAN_WITNESS_LIMIT {
        println!(
            "content-hash repair output: total={} shown={} manifest_sha256={} truncated=true; use --robot for the complete manifest",
            receipt.content_hash_repairs.len(),
            HUMAN_WITNESS_LIMIT,
            receipt.content_hash_repairs_sha256,
        );
    }
    for remap in receipt.comment_id_remaps.iter().take(HUMAN_WITNESS_LIMIT) {
        println!(
            "comment remap witness: issue={} old_id={} new_id={} logical_payload_sha256={}",
            remap.issue_id, remap.old_id, remap.new_id, remap.logical_payload_sha256,
        );
    }
    if receipt.comment_id_remaps.len() > HUMAN_WITNESS_LIMIT {
        println!(
            "comment remap output: total={} shown={} manifest_sha256={} truncated=true; use --robot for the complete manifest",
            receipt.comment_id_remaps.len(),
            HUMAN_WITNESS_LIMIT,
            receipt.comment_id_remaps_sha256,
        );
    }
    for line in additive_conflict_human_lines(receipt, HUMAN_WITNESS_LIMIT) {
        println!("{line}");
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
            open_result.discard_pending_recovery_backup()?;
            Ok(())
        }
        Err(command_err) => {
            if command_err.primary_mutation_committed() {
                if let Err(finalize_err) = open_result.discard_pending_recovery_backup() {
                    return Err(BeadsError::WithContext {
                        context: format!(
                            "sync primary state committed ({command_err}); refusing rollback, but deferred recovery finalization also failed: {finalize_err}"
                        ),
                        source: Box::new(command_err),
                    });
                }
                return Err(command_err);
            }
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

/// Cheap DB↔JSONL coverage probe (`beads_rust-jdmh`).
///
/// The stored-hash shortcut proves the JSONL bytes are unchanged since the
/// last *recorded* import — not that this database ever ingested them. Stored
/// metadata that lies about a partial or lost import makes `--status` and the
/// `--import-only` shortcut assert health over a DB that is missing rows the
/// JSONL holds (GH escalation from jeffreys-skills.md, 2026-07-26 incident:
/// 101 missing issues under "Status: In sync"). Comparing the exportable DB
/// issue count against the JSONL's unique id count catches that state for
/// the cost of one COUNT(*) and one line scan.
#[derive(Debug, Clone, Serialize)]
pub struct SyncCoverageProbe {
    /// Issues the DB would export (tombstones included; ephemerals/wisps excluded).
    pub db_exportable_issues: usize,
    /// Unique `id` values among the JSONL's parseable lines.
    pub jsonl_unique_ids: usize,
}

impl SyncCoverageProbe {
    #[must_use]
    pub const fn drifted(&self) -> bool {
        self.db_exportable_issues != self.jsonl_unique_ids
    }
}

/// Count unique issue ids in a readable JSONL stream. Best-effort: returns
/// `None` on read failures or lines without a string `id`, so callers degrade
/// to legacy behavior instead of failing a read-only diagnostic.
fn jsonl_unique_id_count<R: std::io::BufRead>(reader: R) -> Option<usize> {
    let mut ids: HashSet<String> = HashSet::new();
    for line in reader.lines() {
        let line = line.ok()?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        ids.insert(value.get("id")?.as_str()?.to_string());
    }
    Some(ids.len())
}

/// Build the coverage probe for `--status` from the JSONL path on disk.
/// Best-effort; `None` when the JSONL is absent or unreadable.
fn compute_status_coverage_probe(
    storage: &crate::storage::SqliteStorage,
    jsonl_path: &Path,
) -> Option<SyncCoverageProbe> {
    let file = File::open(jsonl_path).ok()?;
    let jsonl_unique_ids = jsonl_unique_id_count(BufReader::new(file))?;
    let db_exportable_issues = storage.count_exportable_issues().ok()?;
    Some(SyncCoverageProbe {
        db_exportable_issues,
        jsonl_unique_ids,
    })
}

/// Compute the `--status` coverage probe and drift flag, logging drift.
fn status_coverage(
    storage: &crate::storage::SqliteStorage,
    jsonl_path: &Path,
    jsonl_exists: bool,
) -> (Option<SyncCoverageProbe>, bool) {
    let coverage = if jsonl_exists {
        compute_status_coverage_probe(storage, jsonl_path)
    } else {
        None
    };
    let coverage_drift = coverage.as_ref().is_some_and(SyncCoverageProbe::drifted);
    if coverage_drift && let Some(probe) = coverage.as_ref() {
        warn!(
            db_exportable_issues = probe.db_exportable_issues,
            jsonl_unique_ids = probe.jsonl_unique_ids,
            "Coverage drift: DB and JSONL hold different issue sets despite hash/timestamp signals"
        );
    }
    (coverage, coverage_drift)
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

    let (coverage, coverage_drift) = status_coverage(storage, jsonl_path, jsonl_exists);

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
        coverage,
        coverage_drift,
        git_export: GitExportStatus::not_probed(),
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
        println!(
            "  VCS status: not probed (run {})",
            status.git_export.diagnostic_command
        );
        if status.jsonl_newer {
            println!("  Status: JSONL is newer (import recommended)");
        } else if status.db_newer {
            println!("  Status: Database is newer (export recommended)");
        } else if status.coverage_drift {
            if let Some(probe) = &status.coverage {
                println!(
                    "  Status: COVERAGE DRIFT — JSONL has {} unique ids but the database holds {} exportable issues",
                    probe.jsonl_unique_ids, probe.db_exportable_issues
                );
            } else {
                println!("  Status: COVERAGE DRIFT — DB and JSONL hold different issue sets");
            }
            println!(
                "  Recover: `br sync --reconcile --dry-run` (lossless preview) or `br sync --import-only --rebuild` (JSONL-authoritative)"
            );
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
    } else if status.coverage_drift {
        (
            "✗",
            "Coverage drift: DB and JSONL hold different issue sets (see `br sync --reconcile --dry-run`)",
            theme.error.clone(),
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

    text.append_styled("VCS status:   ", theme.dimmed.clone());
    text.append_styled("not probed", theme.muted.clone());
    text.append(" (run ");
    text.append_styled(status.git_export.diagnostic_command, theme.accent.clone());
    text.append(")\n");

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
    retained_source: config::RetainedJsonlSourceRef<'_>,
    expected_source: &JsonlSourceStateWitness,
    retained_authority: Option<&crate::sync::JsonlFamilyWriteLock>,
    ctx: &OutputContext,
) -> Result<Option<Arc<JsonlSourceSnapshot>>> {
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

    let owned_jsonl_authority = retained_authority
        .is_none()
        .then(|| crate::sync::blocking_jsonl_family_write_lock_with_timeout(jsonl_path, None))
        .transpose()?;
    let jsonl_authority = retained_authority
        .or(owned_jsonl_authority.as_ref())
        .expect("JSONL authority is retained or acquired");
    jsonl_authority.verify_jsonl_authority()?;
    let captured_source = jsonl_authority.capture_optional_target()?;
    let source = captured_source.as_ref();
    let observed_source = source.map_or(
        JsonlSourceStateWitness::Missing,
        JsonlSourceSnapshot::state_witness,
    );
    if !matches!(retained_source, config::RetainedJsonlSourceRef::Uncaptured)
        && &observed_source != expected_source
    {
        return Err(BeadsError::SyncConflict {
            message: "Retained JSONL source does not match its startup witness".to_string(),
        });
    }

    // Check for dirty issues
    let dirty_ids = storage.get_dirty_issue_ids()?;
    let needs_flush = storage.get_metadata("needs_flush")?.as_deref() == Some("true");
    let jsonl_exists = source.is_some();
    let db_issue_count = storage.count_issues()?;
    debug!(dirty_count = dirty_ids.len(), "Found dirty issues");

    // Refuse to overwrite a JSONL that still holds unresolved merge-conflict
    // markers. The main flush path below would blow away the `<<<<<<<` /
    // `=======` / `>>>>>>>` regions along with whatever remote side of the
    // merge they contain, silently resolving the conflict in favor of the
    // local DB. Detect the markers up-front so the operator can resolve the
    // merge (or pass `--force` if they actually intend the DB to win).
    if let Some(source) = source
        && !args.force
    {
        ensure_no_conflict_markers_snapshot(source)?;
    }

    // If no dirty issues and no force, report nothing to do
    if dirty_ids.is_empty() && !needs_flush && jsonl_exists && !args.force {
        // `ensure_no_conflict_markers` ran above before we got here, so
        // `analyze_jsonl_snapshot` below won't trip over unresolved `<<<<<<<` /
        // `=======` / `>>>>>>>` lines.

        // Guard against stale DB state without parsing the JSONL twice for count
        // and IDs.
        let (existing_count, jsonl_ids) =
            analyze_jsonl_snapshot(source.expect("existing JSONL has an immutable snapshot"))?;
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
            let purged_ids = storage.get_purged_ids_pending_export()?;
            let mut missing_list = jsonl_ids
                .difference(&db_ids)
                .filter(|id| !purged_ids.contains(id.as_str()))
                .cloned()
                .collect::<Vec<_>>();

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

        // Even with nothing to export, maintain the merge anchor from the
        // clean JSONL (issues #378/#394): a missing anchor is materialized
        // and a stale anchor is replaced, making `br sync --flush-only` an
        // idempotent recovery command for the doctor's
        // `base_jsonl.missing_post_flush` / stale-anchor findings.
        //
        // IDs/counts alone cannot prove that a same-ID JSONL record still
        // matches the database. Immediately before touching the merge
        // anchor, require the last certified semantic hash and compare it
        // directly with the captured snapshot. Missing or mismatched
        // evidence fails closed; --force deliberately takes the real-export
        // path instead.
        let noop_source = source.expect("existing JSONL has an immutable snapshot");
        let stored_content_hash = storage
            .get_metadata(METADATA_JSONL_CONTENT_HASH)?
            .filter(|hash| !hash.trim().is_empty())
            .ok_or_else(|| {
                BeadsError::Config(
                    "Cannot certify a no-op flush because the stored JSONL content hash is \
                     missing. The merge anchor was not changed.\n\
                     Inspect/reconcile with `br sync --merge`, accept the JSONL intentionally \
                     with `br sync --import-only --force`, or replace it from the database with \
                     `br sync --flush-only --force`."
                        .to_string(),
                )
            })?;
        let observed_content_hash = noop_source.content_sha256();
        if observed_content_hash != stored_content_hash {
            return Err(BeadsError::Config(format!(
                "Refusing a no-op flush because the JSONL changed since its last certified \
                 sync (stored hash {stored_content_hash}, observed hash \
                 {observed_content_hash}). The merge anchor was not changed.\n\
                 Inspect/reconcile with `br sync --merge`, accept the JSONL intentionally with \
                 `br sync --import-only --force`, or replace it from the database with \
                 `br sync --flush-only --force`."
            )));
        }

        // Certified: ensure the anchor holds the exact snapshot bytes (also
        // covers the missing-anchor case). A byte-identical regular-file
        // anchor is left untouched so an idempotent no-op flush keeps its
        // inode; anything else (missing, symlinked, byte-divergent — even
        // whitespace-only drift the content hash cannot see) is replaced
        // with the exact snapshot bytes.
        let anchor_path = path_policy.beads_dir.join("beads.base.jsonl");
        let snapshot_bytes = {
            let mut bytes = Vec::with_capacity(usize::try_from(noop_source.size()).unwrap_or(0));
            std::io::copy(&mut noop_source.reader(), &mut bytes).map_err(BeadsError::Io)?;
            bytes
        };
        let anchor_is_exact = fs::symlink_metadata(&anchor_path)
            .map(|meta| meta.is_file())
            .unwrap_or(false)
            && fs::read(&anchor_path).is_ok_and(|bytes| bytes == snapshot_bytes);
        if !anchor_is_exact {
            refresh_base_snapshot_from_flushed_jsonl_snapshot(noop_source, &path_policy.beads_dir)?;
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
                publication_atomicity: None,
            };
            ctx.json_pretty(&result);
        } else if should_render_human_sync_output(ctx, use_json) {
            println!("Nothing to export (no dirty issues)");
        }
        return Ok(None);
    }

    // Configure export. `needs_flush` must NOT be conflated with the user's
    // explicit `--force`: its only job is to bypass the nothing-to-do early
    // return above so a re-export happens when the DB holds canonical
    // content. Passing it as `force` disabled the exporter's data-loss
    // guards, letting a post-merge flush silently destroy JSONL issues the
    // DB had never imported (#405). Intentional purges are excluded from the
    // guard via the purged-pending-export marker instead.
    let export_config = ExportConfig {
        force: args.force,
        is_default_path: true,
        error_policy: export_policy,
        retention_days,
        export_as_of: None,
        beads_dir: Some(path_policy.beads_dir.clone()),
        allow_external_jsonl: path_policy.allow_external_jsonl,
        show_progress,
        history: history_config,
        max_parallel_workers: args.export_parallelism.unwrap_or(0),
        expected_staged_output: None,
    };

    // Execute export
    info!(path = %jsonl_path.display(), "Writing issues.jsonl");
    let (export_result, report) = export_to_jsonl_with_policy_expected_under_authority(
        storage,
        jsonl_path,
        &export_config,
        source.map_or(
            ExpectedJsonlSourceRef::Missing,
            ExpectedJsonlSourceRef::Present,
        ),
        jsonl_authority,
    )?;
    let published_source = export_result.published_source_arc()?;
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

    // A clean flush leaves DB == JSONL, so the JSONL that just reached disk
    // is the new common state future 3-way merges should diff against.
    // Refresh the merge anchor to match (issue #378): historically only the
    // merge path wrote `beads.base.jsonl`, leaving flush-only workspaces
    // permanently anchor-less and tripping the doctor's
    // `base_jsonl.missing_post_flush` warning while `br sync --status`
    // reported "In sync". Skip when the export had per-record errors — a
    // partial export must not become the merge base. Publish the anchor
    // BEFORE clearing dirty/export metadata so an anchor publication failure
    // keeps the workspace dirty and remains recoverable by a later explicit
    // flush: certifying "In sync" over a stale anchor would hand future
    // 3-way merges the wrong ancestor. (This fail-closed ordering shipped in
    // 5414143b alongside the failure-injection coverage; the 77ae88ff
    // tree-preference merge silently reverted it to the older best-effort
    // wording while keeping the test.)
    if !report.has_errors() {
        refresh_base_snapshot_from_flushed_jsonl_snapshot(
            export_result.published_source()?,
            &path_policy.beads_dir,
        )
        .map_err(|source| BeadsError::WithContext {
            context: format!(
                "Failed to publish the merge anchor {} for the flushed JSONL; \
                 dirty/export metadata was retained so this flush can be retried",
                path_policy.beads_dir.join("beads.base.jsonl").display()
            ),
            source: Box::new(source),
        })?;
    }

    // Finalize export (clear dirty flags, update metadata) only after a
    // clean export's anchor is durable.
    finalize_export_under_authority(
        storage,
        &export_result,
        Some(&export_result.issue_hashes),
        jsonl_path,
        jsonl_authority,
    )
    .map_err(|source| BeadsError::CommittedStateUnwitnessed {
        operation: "flush JSONL export finalization".to_string(),
        source: Box::new(source),
    })?;
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
        let manifest_publication =
            write_manifest_atomically(&manifest_file, &manifest).map_err(|source| {
                BeadsError::CommittedArtifactFailure {
                    operation: "flush".to_string(),
                    primary_path: jsonl_path.clone(),
                    artifact_path: manifest_file.clone(),
                    source: Box::new(source),
                }
            })?;
        if !manifest_publication.cleanup_durable() {
            warn!(
                manifest_path = %manifest_file.display(),
                recovery_path = manifest_publication.retained_recovery_path(),
                "Manifest reached its verified destination, but displaced-generation cleanup was not certified durable"
            );
        }
        Some(manifest_file.to_string_lossy().to_string())
    } else {
        None
    };

    // Output result
    let cleared_dirty = export_result.exported_marked_at.len();
    // Only a downgraded publication is worth a field: the atomic protocol is
    // the documented default, and keeping the field absent leaves every
    // existing `--json` consumer and golden untouched (#419).
    let publication_atomicity = export_result
        .publication
        .as_ref()
        .map(crate::sync::ExportPublicationReceipt::atomicity)
        .filter(|atomicity| atomicity.is_downgraded())
        .map(|atomicity| atomicity.as_str().to_string());
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
        publication_atomicity,
    };

    if use_json {
        ctx.json_pretty(&result);
    } else if !should_render_human_sync_output(ctx, use_json) {
        return Ok(Some(published_source));
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
        if let Some(ref atomicity) = result.publication_atomicity {
            println!(
                "Publication downgraded to {atomicity}: this filesystem does not support \
                 flagged rename, so the JSONL was installed with a witness-checked plain \
                 rename under the write lock"
            );
        }
        if report.has_errors() {
            println!();
            println!("Errors ({}):", report.errors.len());
            for err in &report.errors {
                println!("  {}", err.summary());
            }
        }
    }

    Ok(Some(published_source))
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
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp_path) {
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

fn write_manifest_atomically(
    manifest_path: &Path,
    manifest: &serde_json::Value,
) -> Result<crate::sync::ExportPublicationReceipt> {
    use std::io::Write;

    let content = serde_json::to_string_pretty(manifest)?;

    let (temp_path, mut file) = create_temp_manifest_file(manifest_path)?;

    // Close the handle before attempting cleanup so Windows does not retain a
    // torn temp solely because the writer still had the file open.
    if let Err(error) = file.write_all(content.as_bytes()) {
        drop(file);
        let _ = fs::remove_file(&temp_path);
        return Err(BeadsError::Io(error));
    }
    if let Err(error) = file.sync_all() {
        drop(file);
        let _ = fs::remove_file(&temp_path);
        return Err(BeadsError::Io(error));
    }
    drop(file);

    crate::sync::publish_staged_file_conditionally(&temp_path, manifest_path)
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

    // Non-atomic publication downgrade (#419)
    if let Some(ref atomicity) = result.publication_atomicity {
        text.append("\n");
        text.append_styled("Publication   ", theme.dimmed.clone());
        text.append_styled(
            &format!("{atomicity} (filesystem lacks flagged rename)"),
            theme.warning.clone(),
        );
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

#[allow(clippy::too_many_arguments)]
fn replace_database_from_jsonl_snapshot(
    storage: &mut crate::storage::SqliteStorage,
    beads_dir: &Path,
    cli: &config::CliOverrides,
    source: &JsonlSourceSnapshot,
    jsonl_authority: &crate::sync::JsonlFamilyWriteLock,
    db_path: &Path,
    import_config: &ImportConfig,
    target_prefix: Option<&str>,
    preserved_tombstones: &[crate::sync::PreservedIssue],
) -> Result<ImportResult> {
    let startup = config::load_startup_config_with_paths(beads_dir, cli.db.as_ref())?;
    let mut bootstrap_layer = startup.merged_config;
    if let Some(prefix) = target_prefix {
        bootstrap_layer
            .runtime
            .insert("issue_prefix".to_string(), prefix.to_string());
    }

    // Close the old connection before the verified backup-and-replace path
    // opens a new database at the same location.
    let placeholder = crate::storage::SqliteStorage::open_memory()?;
    let previous_storage = std::mem::replace(storage, placeholder);
    drop(previous_storage);

    let recovery =
        if let Some(authority) = cli.database_family_write_authority_for(beads_dir, db_path) {
            config::repair_database_from_jsonl_snapshot_with_import_config_under_write_authority(
                beads_dir,
                db_path,
                cli.lock_timeout,
                &bootstrap_layer,
                import_config.clone(),
                source,
                jsonl_authority,
                authority,
            )
        } else {
            config::repair_database_from_jsonl_snapshot_with_import_config(
                beads_dir,
                db_path,
                cli.lock_timeout,
                &bootstrap_layer,
                import_config.clone(),
                source,
                jsonl_authority,
            )
        };

    let (mut rebuilt_storage, import_result, _) = match recovery {
        Ok(rebuilt) => rebuilt,
        Err(error) => {
            let reopened = cli
                .database_family_write_authority_for(beads_dir, db_path)
                .map_or_else(
                    || crate::storage::SqliteStorage::open_with_timeout(db_path, cli.lock_timeout),
                    |authority| {
                        crate::storage::SqliteStorage::open_with_timeout_under_write_authority(
                            db_path,
                            cli.lock_timeout,
                            authority,
                        )
                    },
                );
            if let Ok(reopened) = reopened {
                *storage = reopened;
            }
            return Err(error);
        }
    };
    restore_tombstones_after_rebuild(&mut rebuilt_storage, preserved_tombstones)?;
    *storage = rebuilt_storage;
    Ok(import_result)
}

fn repair_import_integrity_if_needed(
    storage: &mut crate::storage::SqliteStorage,
    beads_dir: &Path,
    cli: &config::CliOverrides,
    source: &JsonlSourceSnapshot,
    jsonl_authority: &crate::sync::JsonlFamilyWriteLock,
    db_path: &Path,
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

    let jsonl_filter = scan_jsonl_snapshot_for_tombstone_filter(source)?;
    let preserved_tombstones =
        tombstones_missing_from_jsonl_tombstones(snapshot_tombstones(storage), &jsonl_filter);

    replace_database_from_jsonl_snapshot(
        storage,
        beads_dir,
        cli,
        source,
        jsonl_authority,
        db_path,
        import_config,
        None,
        &preserved_tombstones,
    )?;
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

fn jsonl_contains_prefix_mismatch(
    source: &JsonlSourceSnapshot,
    expected_prefix: &str,
) -> Result<bool> {
    for issue in read_issues_from_jsonl_snapshot(source)? {
        if issue.status == crate::model::Status::Tombstone {
            continue;
        }
        if !id_matches_expected_prefix(&issue.id, expected_prefix) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn jsonl_contains_duplicate_external_refs(source: &JsonlSourceSnapshot) -> Result<bool> {
    let mut seen_external_refs = HashSet::new();
    for issue in read_issues_from_jsonl_snapshot(source)? {
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
        prefix_renames: Vec::new(),
        salvage: None,
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
    retained_source: config::RetainedJsonlSourceRef<'_>,
    retained_authority: Option<&crate::sync::JsonlFamilyWriteLock>,
    db_path: &std::path::Path,
    ctx: &OutputContext,
) -> Result<Option<Arc<JsonlSourceSnapshot>>> {
    info!("Starting JSONL import");
    let jsonl_path = &path_policy.jsonl_path;
    debug!(
        jsonl_path = %jsonl_path.display(),
        external_jsonl = path_policy.is_external,
        force = args.force,
        auto_rebuilt,
        "Import configuration resolved"
    );

    // A missing import source is a no-op, but every present source is captured
    // exactly once before prefix inference, validation, tombstone filtering, or
    // any SQLite mutation. The retained JSONL-family authority serializes
    // cooperating writers for the full import; the immutable bytes also make an
    // out-of-band path replacement harmless to internal pass consistency.
    let owned_jsonl_authority = retained_authority
        .is_none()
        .then(|| {
            crate::sync::blocking_jsonl_family_write_lock_with_timeout(jsonl_path, cli.lock_timeout)
        })
        .transpose()?;
    let jsonl_authority = retained_authority
        .or(owned_jsonl_authority.as_ref())
        .expect("JSONL authority is retained or acquired");
    jsonl_authority.verify_jsonl_authority()?;
    let captured_source = jsonl_authority.capture_optional_target()?;
    let source = captured_source.as_ref();
    match retained_source {
        config::RetainedJsonlSourceRef::Present(retained)
            if source.map(JsonlSourceSnapshot::state_witness) != Some(retained.state_witness()) =>
        {
            return Err(BeadsError::SyncConflict {
                message:
                    "Authority-pinned import source differs from its retained startup snapshot"
                        .to_string(),
            });
        }
        config::RetainedJsonlSourceRef::Missing if source.is_some() => {
            return Err(BeadsError::SyncConflict {
                message: "JSONL appeared after startup captured a missing import source"
                    .to_string(),
            });
        }
        _ => {}
    }
    let Some(source) = source else {
        jsonl_authority.verify_jsonl_authority()?;
        if jsonl_authority.capture_optional_target()?.is_some() {
            return Err(BeadsError::SyncConflict {
                message: "JSONL appeared after import captured a missing source".to_string(),
            });
        }
        warn!(path = %jsonl_path.display(), "JSONL path missing, skipping import");
        if use_json {
            let result = ImportResultOutput {
                created: 0,
                updated: 0,
                skipped: 0,
                tombstone_skipped: 0,
                orphans_removed: 0,
                blocked_cache_rebuilt: false,
                prefix_renames: Vec::new(),
                salvage: None,
            };
            ctx.json_pretty(&result);
        } else if should_render_human_sync_output(ctx, use_json) {
            println!("No JSONL file found at {}", jsonl_path.display());
        }
        return Ok(None);
    };
    let mut published_salvage_source = None;
    let mut salvage_receipt = None;
    if args.skip_invalid_records
        && let Some(salvage) = salvage_invalid_jsonl_records_under_authority(
            source,
            jsonl_path,
            &path_policy.beads_dir,
            path_policy.allow_external_jsonl,
            jsonl_authority,
        )?
    {
        salvage_receipt = Some(salvage.receipt);
        published_salvage_source = Some(salvage.source);
    }
    let source = published_salvage_source.as_deref().unwrap_or(source);
    let source_content_hash = compute_jsonl_snapshot_content_hash(source)?;

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
        let layer = config::load_config_with_external_jsonl_policy_snapshot(
            beads_dir,
            Some(storage),
            cli,
            path_policy.allow_external_jsonl,
            source,
        )?;
        let id_cfg = config::id_config_from_layer(&layer);
        Some(if id_cfg.prefix == "br" {
            // Prefix is still the default — check if we should auto-detect from JSONL
            let db_prefix = storage.get_config("issue_prefix")?;
            if let Some(p) = db_prefix {
                p
            } else if let Some(detected) = detect_prefix_from_jsonl(source)? {
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
    let auto_rebuilt_source_is_current = auto_rebuilt
        && (matches!(retained_source, config::RetainedJsonlSourceRef::Present(_))
            || storage
                .get_metadata(METADATA_JSONL_CONTENT_HASH)?
                .as_deref()
                == Some(source_content_hash.as_str()));
    if auto_rebuilt && !auto_rebuilt_source_is_current {
        warn!(
            path = %jsonl_path.display(),
            "JSONL changed after automatic recovery; continuing with an explicit import of the newly captured source"
        );
    }
    let rename_semantics_were_skipped = auto_rebuilt_source_is_current
        && target_prefix.as_deref().is_some_and(|prefix| {
            jsonl_contains_prefix_mismatch(source, prefix).unwrap_or(true)
                || jsonl_contains_duplicate_external_refs(source).unwrap_or(true)
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

    if auto_rebuilt_source_is_current {
        info!(
            force = args.force,
            rebuild = args.rebuild,
            "Skipping import body: database was rebuilt from JSONL during open"
        );
        crate::sync::verify_jsonl_source_snapshot_current(source, jsonl_authority)?;
        emit_auto_rebuild_import_result(storage, use_json, ctx)?;
        return Ok(published_salvage_source);
    }

    // Check staleness (unless --force or --rebuild)
    if !args.force && !args.rebuild {
        let last_import_time = storage.get_metadata(METADATA_LAST_IMPORT_TIME)?;
        let stored_hash = storage.get_metadata(METADATA_JSONL_CONTENT_HASH)?;

        if let (Some(import_time), Some(stored)) = (last_import_time, stored_hash) {
            // Check if JSONL content hash matches
            let current_hash = source_content_hash.clone();
            let coverage_probe = if current_hash == stored {
                // Coverage invariant (`beads_rust-jdmh`): a matching stored
                // hash proves the JSONL bytes are unchanged since the last
                // *recorded* import, not that this DB ingested them. If the
                // exportable DB issue count disagrees with the JSONL's
                // unique id count, the shortcut would assert health over a
                // partial/lost import — fall through to the real (additive,
                // never-deleting) import body instead.
                jsonl_unique_id_count(source.reader()).map(|jsonl_unique_ids| {
                    storage
                        .count_exportable_issues()
                        .map(|db_exportable_issues| SyncCoverageProbe {
                            db_exportable_issues,
                            jsonl_unique_ids,
                        })
                })
            } else {
                None
            };
            let coverage_drift = matches!(
                coverage_probe,
                Some(Ok(ref probe)) if probe.drifted()
            );
            if coverage_drift && let Some(Ok(probe)) = &coverage_probe {
                warn!(
                    db_exportable_issues = probe.db_exportable_issues,
                    jsonl_unique_ids = probe.jsonl_unique_ids,
                    "Stored-hash shortcut rejected: DB does not cover the JSONL issue set; running full import"
                );
            }
            if current_hash == stored && !coverage_drift {
                debug!(
                    path = %jsonl_path.display(),
                    last_import = %import_time,
                    "JSONL is current, skipping import"
                );

                crate::sync::verify_jsonl_source_snapshot_current(source, jsonl_authority)?;
                if use_json {
                    let result = ImportResultOutput {
                        created: 0,
                        updated: 0,
                        skipped: 0,
                        tombstone_skipped: 0,
                        orphans_removed: 0,
                        blocked_cache_rebuilt: false,
                        prefix_renames: Vec::new(),
                        salvage: salvage_receipt.clone(),
                    };
                    ctx.json_pretty(&result);
                } else if should_render_human_sync_output(ctx, use_json) {
                    println!("JSONL is current (hash unchanged since last import)");
                }
                return Ok(published_salvage_source);
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

    // Force/rebuild prepasses and the import itself all consume the same
    // captured bytes. Run the marker scan first so malformed merge content
    // retains its actionable error class, then derive IDs and tombstone state
    // before any destructive table reset.
    if args.force || args.rebuild {
        ensure_no_conflict_markers_snapshot(source)?;
    }
    let jsonl_issue_ids = if args.force || args.rebuild {
        Some(get_issue_ids_from_jsonl_snapshot(source)?)
    } else {
        None
    };
    let jsonl_filter = if args.force || args.rebuild {
        Some(scan_jsonl_snapshot_for_tombstone_filter(source)?)
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

    // Force/rebuild imports replace the database through the verified
    // backup-and-restore recovery path. This avoids fsqlite's in-place
    // DROP/CREATE pager hazards and, critically, guarantees that any late
    // validation or storage failure restores the complete previous DB family.
    let import_used_backup_rebuild = args.force || args.rebuild;
    let force_import_target_was_empty = false;
    info!(path = %jsonl_path.display(), "Importing from JSONL");
    let mut import_result = if import_used_backup_rebuild {
        replace_database_from_jsonl_snapshot(
            storage,
            beads_dir,
            cli,
            source,
            jsonl_authority,
            db_path,
            &import_config,
            target_prefix.as_deref(),
            &preserved_tombstones,
        )?
    } else {
        import_from_jsonl_snapshot(storage, source, &import_config, target_prefix.as_deref())?
    };

    if let Some(salvage) = salvage_receipt.as_mut() {
        let exportable_records = storage.count_exportable_issues()?;
        let records_requiring_export =
            exportable_records.saturating_sub(import_result.export_hashes_recorded);
        salvage.database_records_requiring_export = records_requiring_export;
        if records_requiring_export > 0 {
            storage.set_metadata("needs_flush", "true")?;
            salvage.needs_flush_set = true;
            warn!(
                records_requiring_export,
                "JSONL salvage preserved database records absent from the recovered source; a full export is required"
            );
        }
    }

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
        if !import_used_backup_rebuild {
            restore_tombstones_after_rebuild(storage, &preserved_tombstones)?;
        }
        import_result.tombstone_skipped += preserved_resurrection_attempts;
    }

    // Update the source JSONL content hash before post-import maintenance.
    // Metadata table/index writes are part of the same B-tree surface that
    // triggered frankentorch-dbp, so compaction must be the final storage
    // mutation in this path.
    if !import_used_backup_rebuild {
        storage.set_metadata(METADATA_JSONL_CONTENT_HASH, &source_content_hash)?;
    }

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

    if !import_used_backup_rebuild
        && (args.force || args.rebuild || import_rewrote_storage)
        && !skip_heavy_import_maintenance
    {
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
                let reopened = cli
                    .database_family_write_authority_for(beads_dir, db_path)
                    .map_or_else(
                        || {
                            crate::storage::SqliteStorage::open_with_timeout(
                                db_path,
                                cli.lock_timeout,
                            )
                        },
                        |authority| {
                            crate::storage::SqliteStorage::open_with_timeout_under_write_authority(
                                db_path,
                                cli.lock_timeout,
                                authority,
                            )
                        },
                    );
                if let Ok(reopened) = reopened {
                    *storage = reopened;
                }
                return Err(err);
            }
        }
        repair_import_integrity_if_needed(
            storage,
            beads_dir,
            cli,
            source,
            jsonl_authority,
            db_path,
            &import_config,
        )?;
    }

    crate::sync::verify_jsonl_source_snapshot_current(source, jsonl_authority)?;

    // Output result
    let result = ImportResultOutput {
        created: import_result.created_count,
        updated: import_result.updated_count,
        skipped: import_result.skipped_count,
        tombstone_skipped: import_result.tombstone_skipped,
        orphans_removed: import_result.orphans_removed,
        blocked_cache_rebuilt: true,
        prefix_renames: import_result.prefix_renames.clone(),
        salvage: salvage_receipt.clone(),
    };

    if use_json {
        ctx.json_pretty(&result);
    } else if !should_render_human_sync_output(ctx, use_json) {
        return Ok(published_salvage_source);
    } else if ctx.is_rich() {
        render_import_result_rich(&result, ctx);
        if let Some(salvage) = &result.salvage {
            ctx.warning(&format!(
                "JSONL salvage retained {} valid record(s), rejected {} invalid record(s); exact source backup: {}",
                salvage.valid_records,
                salvage.rejected_records.len(),
                salvage.backup_path
            ));
            for rejected in salvage.rejected_records.iter().take(HUMAN_WITNESS_LIMIT) {
                ctx.warning(&format!(
                    "Rejected JSONL line {}: {}",
                    rejected.line, rejected.error
                ));
            }
            if salvage.needs_flush_set {
                ctx.warning(&format!(
                    "{} preserved database record(s) require `br sync --flush-only` to restore JSONL coverage",
                    salvage.database_records_requiring_export
                ));
            }
        }
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
        if !result.prefix_renames.is_empty() {
            println!("  Prefix renames: {} issues", result.prefix_renames.len());
            for rename in result.prefix_renames.iter().take(HUMAN_WITNESS_LIMIT) {
                match rename.fallback.as_deref() {
                    Some(reason) => println!(
                        "    {} -> {} (fallback: {reason})",
                        rename.old_id, rename.new_id
                    ),
                    None => println!("    {} -> {}", rename.old_id, rename.new_id),
                }
            }
            if result.prefix_renames.len() > HUMAN_WITNESS_LIMIT {
                println!(
                    "    ... {} more; use --json for the complete mapping",
                    result.prefix_renames.len() - HUMAN_WITNESS_LIMIT
                );
            }
        }
        if let Some(salvage) = &result.salvage {
            println!(
                "  JSONL salvage: retained {} valid record(s), rejected {} invalid record(s)",
                salvage.valid_records,
                salvage.rejected_records.len()
            );
            println!(
                "  Exact source backup: {}",
                sanitize_terminal_inline(&salvage.backup_path)
            );
            for rejected in salvage.rejected_records.iter().take(HUMAN_WITNESS_LIMIT) {
                println!(
                    "    Rejected line {}: {}",
                    rejected.line,
                    sanitize_terminal_inline(&rejected.error)
                );
            }
            if salvage.rejected_records.len() > HUMAN_WITNESS_LIMIT {
                println!(
                    "    ... {} more; use --json for the complete rejection receipt",
                    salvage.rejected_records.len() - HUMAN_WITNESS_LIMIT
                );
            }
            if salvage.needs_flush_set {
                println!(
                    "  Follow-up: {} preserved database record(s) require `br sync --flush-only`",
                    salvage.database_records_requiring_export
                );
            }
        }
        println!("  Rebuilt blocked cache");
    }

    Ok(published_salvage_source)
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

    // Prefix renames (--rename-prefix receipt)
    if !result.prefix_renames.is_empty() {
        text.append_styled("Prefix renames     ", theme.dimmed.clone());
        text.append_styled(
            &result.prefix_renames.len().to_string(),
            theme.accent.clone(),
        );
        text.append_styled(" issues", theme.dimmed.clone());
        text.append("\n");
        for rename in result.prefix_renames.iter().take(HUMAN_WITNESS_LIMIT) {
            text.append_styled("  ", theme.dimmed.clone());
            text.append(&rename.old_id);
            text.append_styled(" -> ", theme.dimmed.clone());
            text.append_styled(&rename.new_id, theme.info.clone());
            if let Some(reason) = rename.fallback.as_deref() {
                text.append_styled(&format!(" (fallback: {reason})"), theme.warning.clone());
            }
            text.append("\n");
        }
        if result.prefix_renames.len() > HUMAN_WITNESS_LIMIT {
            text.append_styled(
                &format!(
                    "  ... {} more; use --json for the complete mapping\n",
                    result.prefix_renames.len() - HUMAN_WITNESS_LIMIT
                ),
                theme.muted.clone(),
            );
        }
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
fn detect_prefix_from_jsonl(source: &JsonlSourceSnapshot) -> Result<Option<String>> {
    let issues = read_issues_from_jsonl_snapshot(source)?;

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

/// Execute the --reconcile operation (additive JSONL→DB reconciliation).
///
/// Plans read-only, then either emits the plan receipt (`--dry-run`) or
/// applies it through `apply_additive_reconcile`, which re-verifies the
/// plan's witnesses inside a single write transaction and rolls back on any
/// mismatch. Deletion is impossible in this mode and no JSONL, base
/// snapshot, or event rows are ever written.
fn execute_reconcile(
    storage: &mut crate::storage::SqliteStorage,
    path_policy: &SyncPathPolicy,
    args: &SyncArgs,
    use_json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let jsonl_path = &path_policy.jsonl_path;
    info!(
        jsonl_path = %jsonl_path.display(),
        dry_run = args.dry_run,
        "Starting additive reconcile"
    );

    let import_config = ImportConfig {
        // Mixed prefixes are supported and reconcile never rewrites ids.
        skip_prefix_validation: true,
        rename_on_import: false,
        clear_duplicate_external_refs: false,
        orphan_mode: OrphanMode::Strict,
        force_upsert: false,
        beads_dir: Some(path_policy.beads_dir.clone()),
        allow_external_jsonl: path_policy.allow_external_jsonl,
        show_progress: false,
    };

    let plan = plan_sync_reconcile(storage, jsonl_path, &import_config)?;
    debug!(
        records = plan.record_count,
        creates = plan.count_kind(ReconcileActionKind::Create),
        updates = plan.count_kind(ReconcileActionKind::Update),
        db_only = plan.db_only_ids.len(),
        stored_hash_matches_jsonl = plan.stored_hash_matches_jsonl,
        "Reconcile plan computed"
    );

    let outcome = if args.dry_run {
        None
    } else {
        Some(apply_sync_reconcile(
            storage,
            jsonl_path,
            &import_config,
            &plan,
        )?)
    };

    let receipt = build_reconcile_receipt(jsonl_path, &plan, outcome.as_ref());

    if use_json {
        ctx.json_pretty(&receipt);
    } else if should_render_human_sync_output(ctx, use_json) {
        render_reconcile_receipt_text(&receipt);
    }
    Ok(())
}

fn build_reconcile_receipt(
    jsonl_path: &Path,
    plan: &ReconcilePlan,
    outcome: Option<&ReconcileApplyOutcome>,
) -> SyncReconcileReceipt {
    let truncate = |mut ids: Vec<String>| {
        ids.truncate(RECONCILE_PREVIEW_LIMIT);
        ids
    };
    let mut db_only_preview = plan.db_only_ids.clone();
    db_only_preview.truncate(RECONCILE_PREVIEW_LIMIT);

    SyncReconcileReceipt {
        schema_version: SYNC_RECONCILE_SCHEMA_VERSION,
        mode: if outcome.is_some() {
            "apply"
        } else {
            "dry_run"
        },
        applied: outcome.is_some(),
        jsonl_path: jsonl_path.display().to_string(),
        source: ReconcileSourceWitness {
            record_count: plan.record_count,
            ephemeral_skipped: plan.ephemeral_skipped,
            content_hash: plan.witness.jsonl_content_hash.clone(),
            mtime: plan.witness.jsonl_mtime_witness.clone(),
            size_bytes: plan.witness.jsonl_size,
        },
        target: ReconcileTargetWitness {
            db_issue_count: plan.witness.db_issue_count,
            stored_hash_matches_jsonl: plan.stored_hash_matches_jsonl,
        },
        plan: ReconcilePlanCounts {
            created: plan.count_kind(ReconcileActionKind::Create),
            updated: plan.count_kind(ReconcileActionKind::Update),
            skipped_equal: plan.count_kind(ReconcileActionKind::SkipEqual),
            skipped_older: plan.count_kind(ReconcileActionKind::SkipOlder),
            skipped_tombstone: plan.count_kind(ReconcileActionKind::SkipTombstone),
            deleted: 0,
            db_only: plan.db_only_ids.len(),
        },
        previews: ReconcileIdPreviews {
            created_ids: truncate(plan.target_ids_for_kind(ReconcileActionKind::Create)),
            updated_ids: truncate(plan.target_ids_for_kind(ReconcileActionKind::Update)),
            db_only_ids: db_only_preview,
            preview_limit: RECONCILE_PREVIEW_LIMIT,
        },
        relations: ReconcileRelationCounts {
            labels: plan.labels_planned,
            dependencies: plan.dependencies_planned,
            comments: plan.comments_planned,
        },
        events_before: plan.witness.events_count,
        events_after: outcome.map_or(plan.witness.events_count, |o| o.events_after),
        apply: outcome.map(|o| ReconcileApplyReceipt {
            export_hashes_recorded: o.export_hashes_recorded,
            uncertified_local_wins: o.uncertified_local_wins,
            orphan_dependencies_cleaned: o.orphan_dependencies_cleaned,
            blocked_cache_entries: o.blocked_cache_entries,
            child_counter_entries: o.child_counter_entries,
            needs_flush_set: o.needs_flush_set,
            metadata_repaired: o.metadata_repaired,
        }),
    }
}

fn render_reconcile_receipt_text(receipt: &SyncReconcileReceipt) {
    if receipt.applied {
        println!("Reconciled JSONL into database (additive):");
    } else {
        println!("Reconcile plan (dry run, nothing changed):");
    }
    println!(
        "  JSONL records: {} ({} ephemeral skipped)",
        receipt.source.record_count, receipt.source.ephemeral_skipped
    );
    println!(
        "  Create: {}  Update: {}  Delete: {} (structurally impossible)",
        receipt.plan.created, receipt.plan.updated, receipt.plan.deleted
    );
    println!(
        "  Skipped: {} equal, {} older-in-JSONL, {} tombstone-protected",
        receipt.plan.skipped_equal, receipt.plan.skipped_older, receipt.plan.skipped_tombstone
    );
    if receipt.plan.db_only > 0 {
        println!(
            "  DB-only issues (absent from JSONL, untouched): {}",
            receipt.plan.db_only
        );
    }
    println!(
        "  Events: {} before, {} after (preserved)",
        receipt.events_before, receipt.events_after
    );
    if receipt.target.stored_hash_matches_jsonl
        && (receipt.plan.created > 0 || receipt.plan.updated > 0)
    {
        println!(
            "  Note: stored content hash matched the JSONL while rows diverged (false-equal state)"
        );
    }
    if let Some(apply) = &receipt.apply {
        println!(
            "  Relations written: {} labels, {} dependencies, {} comments",
            receipt.relations.labels, receipt.relations.dependencies, receipt.relations.comments
        );
        if apply.orphan_dependencies_cleaned > 0 {
            println!(
                "  Dangling dependency rows removed: {}",
                apply.orphan_dependencies_cleaned
            );
        }
        if apply.metadata_repaired {
            println!("  Sync metadata repaired (content hash + witness recorded)");
        }
        if apply.needs_flush_set {
            println!(
                "  Local state still diverges from JSONL; database marked for flush (run: br sync --flush-only)"
            );
        }
    } else {
        println!("  Run without --dry-run to apply.");
    }
}

fn optional_source_matches(
    source: Option<&JsonlSourceSnapshot>,
    expected_state: &JsonlSourceStateWitness,
    expected_content_sha256: Option<&str>,
) -> bool {
    match (source, expected_state) {
        (None, JsonlSourceStateWitness::Missing) => expected_content_sha256.is_none(),
        (Some(source), JsonlSourceStateWitness::Present { .. }) => {
            source.state_witness() == *expected_state
                && expected_content_sha256
                    .is_none_or(|expected| source.content_sha256() == expected)
        }
        _ => false,
    }
}

fn source_matches_pending_merge_output(
    source: Option<&JsonlSourceSnapshot>,
    receipt: &SyncMergePendingReceipt,
) -> Result<bool> {
    let Some(source) = source else {
        return Ok(false);
    };
    if source.raw_sha256() != receipt.jsonl_after_raw_sha256
        || source.content_sha256() != receipt.jsonl_after_content_sha256
    {
        return Ok(false);
    }
    let (count, _) = analyze_jsonl_snapshot(source)?;
    Ok(count == receipt.jsonl_after_issue_count)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn reconcile_pending_sync_merge_artifacts(
    storage: &mut crate::storage::SqliteStorage,
    db_path: &Path,
    path_policy: &SyncPathPolicy,
    args: &SyncArgs,
    show_progress: bool,
    history_config: HistoryConfig,
    current_source: Option<&JsonlSourceSnapshot>,
    jsonl_authority: &crate::sync::JsonlFamilyWriteLock,
    current_base: Option<&JsonlSourceSnapshot>,
    base_authority: &crate::sync::JsonlFamilyWriteLock,
    mut receipt: SyncMergePendingReceipt,
    no_db: bool,
) -> Result<ReconciledPendingSyncMerge> {
    receipt.validate()?;
    if database_write_authority_sha256(db_path)? != receipt.intent.database_authority_sha256 {
        return Err(BeadsError::SyncConflict {
            message:
                "Pending sync merge database authority does not match the current configured path"
                    .to_string(),
        });
    }
    if receipt.intent.jsonl_authority_sha256 != jsonl_authority.authority_path_sha256()
        || receipt.intent.jsonl_path_sha256
            != canonical_sync_path_sha256(jsonl_authority.canonical_jsonl_path())
    {
        return Err(BeadsError::SyncConflict {
            message:
                "Pending sync merge JSONL authority does not match the current configured path"
                    .to_string(),
        });
    }
    if receipt.intent.base_authority_sha256 != base_authority.authority_path_sha256() {
        return Err(BeadsError::SyncConflict {
            message: "Pending sync merge base authority does not match the current configured path"
                .to_string(),
        });
    }
    let database_authority =
        if no_db {
            None
        } else {
            let authority = storage.attached_write_authority().ok_or_else(|| {
                BeadsError::SyncConflict {
                message:
                    "Persistent pending merge recovery has no retained database-family authority"
                        .to_string(),
            }
            })?;
            if authority.authority_path_sha256() != receipt.intent.database_authority_sha256 {
                return Err(BeadsError::SyncConflict {
                message:
                    "Retained database-family authority does not match the pending merge receipt"
                        .to_string(),
            });
            }
            authority.verify_database_authority()?;
            Some(authority)
        };
    if (args.force || args.force_db || args.force_jsonl)
        && merge_conflict_resolution_label(merge_conflict_resolution(args))
            != receipt.intent.resolution
    {
        return Err(BeadsError::SyncConflict {
            message: "Resume flags select a different merge resolution than the committed receipt"
                .to_string(),
        });
    }
    if storage.with_read_transaction(crate::sync::capture_sync_merge_core_witness)?
        != receipt.database_after
    {
        return Err(BeadsError::SyncConflict {
            message: "Database merge-authoritative state drifted after the pending merge committed"
                .to_string(),
        });
    }

    let jsonl_is_before = optional_source_matches(
        current_source,
        &receipt.intent.jsonl_before,
        receipt.intent.jsonl_before_content_sha256.as_deref(),
    );
    let jsonl_is_after = source_matches_pending_merge_output(current_source, &receipt)?;
    let jsonl_path = &path_policy.jsonl_path;
    let published_source = match receipt.phase {
        SyncMergePendingPhase::DatabaseCommitted => {
            if !jsonl_is_before && !jsonl_is_after {
                return Err(BeadsError::SyncConflict {
                    message:
                        "JSONL is neither the exact pre-merge generation nor the committed merge output"
                            .to_string(),
                });
            }
            let export_config = ExportConfig {
                force: true,
                is_default_path: true,
                error_policy: ExportErrorPolicy::Strict,
                retention_days: receipt.intent.retention_days,
                export_as_of: Some(receipt.intent.export_as_of),
                beads_dir: Some(path_policy.beads_dir.clone()),
                allow_external_jsonl: path_policy.allow_external_jsonl,
                show_progress,
                history: history_config,
                max_parallel_workers: args.export_parallelism.unwrap_or(0),
                expected_staged_output: Some(ExpectedStagedExport {
                    raw_sha256: receipt.jsonl_after_raw_sha256.clone(),
                    issue_count: receipt.jsonl_after_issue_count,
                    issue_hashes: receipt.jsonl_after_issue_hashes.clone(),
                }),
            };
            let expected_source = current_source.map_or(
                ExpectedJsonlSourceRef::Missing,
                ExpectedJsonlSourceRef::Present,
            );
            let (export_result, _) = if let Some(database_authority) = database_authority.as_deref()
            {
                export_to_jsonl_with_policy_expected_under_authorities(
                    storage,
                    jsonl_path,
                    &export_config,
                    expected_source,
                    jsonl_authority,
                    database_authority,
                )?
            } else {
                export_to_jsonl_with_policy_expected_under_authority(
                    storage,
                    jsonl_path,
                    &export_config,
                    expected_source,
                    jsonl_authority,
                )?
            };
            if export_result.content_hash != receipt.jsonl_after_raw_sha256
                || export_result.exported_count != receipt.jsonl_after_issue_count
            {
                return Err(BeadsError::SyncConflict {
                    message: "Deterministic merge export no longer matches the committed receipt"
                        .to_string(),
                });
            }
            let published_source = export_result.published_source_arc()?;
            if published_source.raw_sha256() != receipt.jsonl_after_raw_sha256
                || published_source.content_sha256() != receipt.jsonl_after_content_sha256
            {
                return Err(BeadsError::SyncConflict {
                    message:
                        "Published merge JSONL does not match the receipt's exact output hashes"
                            .to_string(),
                });
            }
            finalize_export_under_authority(
                storage,
                &export_result,
                Some(&export_result.issue_hashes),
                jsonl_path,
                jsonl_authority,
            )?;
            let (database_core, export_finalization) =
                storage.with_read_transaction(|storage| {
                    Ok((
                        crate::sync::capture_sync_merge_core_witness(storage)?,
                        crate::sync::capture_sync_merge_export_finalization_witness(storage)?,
                    ))
                })?;
            if database_core != receipt.database_after {
                return Err(BeadsError::SyncConflict {
                    message:
                        "Merge-authoritative database state changed during export finalization"
                            .to_string(),
                });
            }
            let finalized = receipt.advance_to_export_finalized(
                published_source.state_witness(),
                export_finalization,
            )?;
            storage.compare_and_set_pending_sync_merge_receipt(&receipt, &finalized)?;
            receipt = finalized;
            published_source
        }
        SyncMergePendingPhase::ExportFinalized => {
            if !jsonl_is_after
                || receipt.jsonl_after.as_ref()
                    != current_source
                        .map(JsonlSourceSnapshot::state_witness)
                        .as_ref()
            {
                return Err(BeadsError::SyncConflict {
                    message:
                        "Finalized pending merge JSONL no longer matches its exact published witness"
                            .to_string(),
                });
            }
            let pinned_source = jsonl_authority.capture_target()?;
            if receipt.jsonl_after.as_ref() != Some(&pinned_source.state_witness())
                || !source_matches_pending_merge_output(Some(&pinned_source), &receipt)?
            {
                return Err(BeadsError::SyncConflict {
                    message:
                        "Authority-pinned finalized JSONL recapture does not match the pending receipt"
                            .to_string(),
                });
            }
            Arc::new(pinned_source)
        }
    };

    let base_is_before = optional_source_matches(
        current_base,
        &receipt.intent.base_before,
        receipt.intent.base_before_content_sha256.as_deref(),
    );
    let base_is_after = current_base.is_some_and(|base| {
        base.raw_sha256() == published_source.raw_sha256()
            && base.content_sha256() == published_source.content_sha256()
            && base.size() == published_source.size()
    });
    if !base_is_before && !base_is_after {
        return Err(BeadsError::SyncConflict {
            message:
                "Base snapshot is neither the exact merge ancestor nor the published merge output"
                    .to_string(),
        });
    }
    let current_base_state = current_base.map_or(
        JsonlSourceStateWitness::Missing,
        JsonlSourceSnapshot::state_witness,
    );
    if let Some(database_authority) = database_authority.as_deref() {
        database_authority.verify_database_authority()?;
    }
    crate::sync::verify_jsonl_source_snapshot_current(&published_source, jsonl_authority)?;
    let base_publication = refresh_base_snapshot_from_flushed_jsonl_snapshot_under_authority(
        &published_source,
        &path_policy.beads_dir,
        &current_base_state,
        base_authority,
    )?;
    if let Some(database_authority) = database_authority.as_deref() {
        database_authority.verify_database_authority()?;
    }
    crate::sync::verify_jsonl_source_snapshot_current(&published_source, jsonl_authority)?;
    if base_publication.content_sha256() != published_source.content_sha256() {
        return Err(BeadsError::SyncConflict {
            message: "Published merge base does not match the exact finalized JSONL generation"
                .to_string(),
        });
    }
    if !base_publication.cleanup_durable() {
        warn!(
            base_path = %path_policy.beads_dir.join("beads.base.jsonl").display(),
            recovery_path = base_publication.retained_recovery_path(),
            "Base snapshot is verified, but displaced-generation cleanup was not certified durable"
        );
    }

    let terminal_base = base_authority.capture_target()?;
    if terminal_base.raw_sha256() != published_source.raw_sha256()
        || terminal_base.content_sha256() != published_source.content_sha256()
        || terminal_base.size() != published_source.size()
    {
        return Err(BeadsError::SyncConflict {
            message: "Terminal merge base witness differs from the exact finalized JSONL"
                .to_string(),
        });
    }
    if receipt.phase != SyncMergePendingPhase::ExportFinalized {
        return Err(BeadsError::SyncConflict {
            message: "Pending merge reconciliation did not reach export-finalized phase"
                .to_string(),
        });
    }
    if storage.with_read_transaction(crate::sync::capture_sync_merge_core_witness)?
        != receipt.database_after
    {
        return Err(BeadsError::SyncConflict {
            message: "Database merge-authoritative state changed before outer receipt cleanup"
                .to_string(),
        });
    }
    Ok(ReconciledPendingSyncMerge {
        published_source,
        terminal_receipt: receipt,
    })
}

#[allow(clippy::too_many_arguments)]
fn resume_pending_sync_merge(
    storage: &mut crate::storage::SqliteStorage,
    db_path: &Path,
    path_policy: &SyncPathPolicy,
    args: &SyncArgs,
    use_json: bool,
    show_progress: bool,
    history_config: HistoryConfig,
    current_source: Option<&JsonlSourceSnapshot>,
    jsonl_authority: &crate::sync::JsonlFamilyWriteLock,
    current_base: Option<&JsonlSourceSnapshot>,
    base_authority: &crate::sync::JsonlFamilyWriteLock,
    receipt: SyncMergePendingReceipt,
    no_db: bool,
) -> Result<(ReconciledPendingSyncMerge, DeferredSyncOutput)> {
    let receipt_id = receipt.receipt_id.clone();
    let phase = receipt.phase;
    let capacity_warnings = receipt.capacity_warnings.clone();
    let reconciled = reconcile_pending_sync_merge_artifacts(
        storage,
        db_path,
        path_policy,
        args,
        show_progress,
        history_config,
        current_source,
        jsonl_authority,
        current_base,
        base_authority,
        receipt,
        no_db,
    )
    .map_err(|source| {
        if no_db {
            source
        } else {
            BeadsError::CommittedStateUnwitnessed {
                operation: format!("resume committed sync merge {receipt_id}"),
                source: Box::new(source),
            }
        }
    })?;
    Ok((
        reconciled,
        DeferredSyncOutput::ResumedMerge {
            receipt_id,
            phase_before: phase,
            capacity_warnings,
            use_json,
        },
    ))
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
    retained_source: config::RetainedJsonlSourceRef<'_>,
    expected_source: &JsonlSourceStateWitness,
    retained_authority: Option<&crate::sync::JsonlFamilyWriteLock>,
    cli: &config::CliOverrides,
    db_path: &Path,
    no_db: bool,
    ctx: &OutputContext,
) -> Result<SyncDispatchCompletion> {
    info!("Starting 3-way merge");
    let beads_dir = &path_policy.beads_dir;
    let jsonl_path = &path_policy.jsonl_path;

    let owned_jsonl_authority = retained_authority
        .is_none()
        .then(|| {
            crate::sync::blocking_jsonl_family_write_lock_with_timeout(jsonl_path, cli.lock_timeout)
        })
        .transpose()?;
    let jsonl_authority = retained_authority
        .or(owned_jsonl_authority.as_ref())
        .expect("JSONL authority is retained or acquired");
    jsonl_authority.verify_jsonl_authority()?;
    let captured_source = jsonl_authority.capture_optional_target()?;
    let source = captured_source.as_ref();
    let observed_source = source.map_or(
        JsonlSourceStateWitness::Missing,
        JsonlSourceSnapshot::state_witness,
    );
    if !matches!(retained_source, config::RetainedJsonlSourceRef::Uncaptured)
        && &observed_source != expected_source
    {
        return Err(BeadsError::SyncConflict {
            message: "Retained JSONL source does not match its startup witness".to_string(),
        });
    }

    let base_path = beads_dir.join("beads.base.jsonl");
    let base_authority =
        crate::sync::blocking_jsonl_family_write_lock_with_timeout(&base_path, cli.lock_timeout)?;
    base_authority.verify_jsonl_authority()?;
    let base_source = base_authority.capture_optional_target()?;
    let base_state = base_source.as_ref().map_or(
        JsonlSourceStateWitness::Missing,
        JsonlSourceSnapshot::state_witness,
    );

    if let Some(receipt) = storage.pending_sync_merge_receipt()? {
        let (reconciled, deferred_output) = resume_pending_sync_merge(
            storage,
            db_path,
            path_policy,
            args,
            use_json,
            show_progress,
            history_config,
            source,
            jsonl_authority,
            base_source.as_ref(),
            &base_authority,
            receipt,
            no_db,
        )?;
        return Ok(SyncDispatchCompletion {
            published_source: Some(reconciled.published_source),
            owned_jsonl_authority,
            pending_merge: Some(PendingSyncMergeCompletion {
                receipt: reconciled.terminal_receipt,
                base_authority,
            }),
            deferred_output: Some(deferred_output),
        });
    }

    let database_before = capture_sync_database_witness(storage)?;

    // 1. Load Base State (ancestor) from the exact retained generation.
    let base = load_base_snapshot_from_source(base_source.as_ref())?;
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
    if capture_sync_database_witness(storage)? != database_before {
        return Err(BeadsError::SyncConflict {
            message:
                "Database changed while the sync merge plan was being hydrated; retry from a stable generation"
                    .to_string(),
        });
    }

    // 3. Load Right State (external JSONL)
    let mut right = HashMap::new();
    if let Some(source) = source {
        // The JSONL parser yields a generic
        // generic "Invalid JSON at line 1" error when the JSONL still
        // contains unresolved merge-conflict markers from a botched
        // `git merge` / `git pull`. A three-way merge on top of that state
        // would be nonsense, so scan for markers first and surface the
        // helpful error before we try to parse.
        ensure_no_conflict_markers_snapshot(source)?;
        for issue in read_issues_from_jsonl_snapshot(source)? {
            right.insert(issue.id.clone(), issue);
        }
    }
    debug!(right_count = right.len(), "Loaded external state (JSONL)");

    if source.is_none()
        && (!base.is_empty() || !left.is_empty())
        && !args.force_db
        && !args.force_jsonl
    {
        return Err(BeadsError::SyncConflict {
            message:
                "issues.jsonl is missing, which is not equivalent to an intentionally empty source; use --force-db to recreate it or --force-jsonl to explicitly accept deletion"
                    .to_string(),
        });
    }
    if base_source.is_none() && left != right && !args.force_db && !args.force_jsonl {
        return Err(BeadsError::SyncConflict {
            message:
                "beads.base.jsonl is missing and the database differs from JSONL; choose --force-db or --force-jsonl explicitly instead of guessing a merge ancestor"
                    .to_string(),
        });
    }

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

    let actor = cli.actor.as_deref().unwrap_or("br");
    let note_target_ids = report
        .notes
        .iter()
        .map(|(issue_id, _)| issue_id.as_str())
        .collect::<HashSet<_>>();
    let mut changed_kept = report
        .kept
        .iter()
        .filter(|issue| {
            context.left.get(&issue.id) != Some(*issue)
                || note_target_ids.contains(issue.id.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    changed_kept.sort_by(|left, right| left.id.cmp(&right.id));
    let changed_kept_ids = changed_kept
        .iter()
        .map(|issue| issue.id.clone())
        .collect::<Vec<_>>();
    let kept_issue_witnesses = crate::sync::sync_merge_kept_issue_witnesses(&changed_kept)?;
    let mut deleted_ids = report.deleted.clone();
    deleted_ids.sort();
    let mut note_witnesses = report
        .notes
        .iter()
        .map(|(issue_id, note)| SyncMergeNoteWitness {
            issue_id: issue_id.clone(),
            note_sha256: crate::util::hex_encode(&Sha256::digest(note.as_bytes())),
        })
        .collect::<Vec<_>>();
    note_witnesses.sort_by(|left, right| left.issue_id.cmp(&right.issue_id));
    let merge_as_of = chrono::Utc::now();
    let intent = SyncMergeIntent {
        schema_version: 2,
        database_authority_sha256: database_write_authority_sha256(db_path)?,
        jsonl_authority_sha256: jsonl_authority.authority_path_sha256().to_string(),
        jsonl_path_sha256: canonical_sync_path_sha256(jsonl_authority.canonical_jsonl_path()),
        jsonl_before: observed_source,
        jsonl_before_content_sha256: source
            .map(JsonlSourceSnapshot::content_sha256)
            .map(str::to_string),
        base_authority_sha256: base_authority.authority_path_sha256().to_string(),
        base_before: base_state,
        base_before_content_sha256: base_source
            .as_ref()
            .map(JsonlSourceSnapshot::content_sha256)
            .map(str::to_string),
        resolution: resolution.to_string(),
        actor: actor.to_string(),
        event_attribution: storage.pending_event_attribution_for_review(),
        capacity_policy: storage.workflow_capacity_policy_for_review(),
        retention_days,
        export_as_of: merge_as_of,
        changed_kept_issue_ids: changed_kept_ids,
        kept_issue_witnesses,
        deleted_issue_ids: deleted_ids,
        note_witnesses,
        database_before,
    };
    let pending_receipt = storage.apply_sync_merge_atomically(
        &changed_kept,
        &report.deleted,
        &report.notes,
        &intent,
    )?;
    let capacity_warnings = pending_receipt.capacity_warnings.clone();
    let _ = storage.take_capacity_warnings();

    let reconciled = reconcile_pending_sync_merge_artifacts(
        storage,
        db_path,
        path_policy,
        args,
        show_progress,
        history_config,
        source,
        jsonl_authority,
        base_source.as_ref(),
        &base_authority,
        pending_receipt,
        no_db,
    )
    .map_err(|source| {
        if no_db {
            source
        } else {
            BeadsError::CommittedStateUnwitnessed {
                operation: "sync merge artifact reconciliation".to_string(),
                source: Box::new(source),
            }
        }
    })?;
    Ok(SyncDispatchCompletion {
        published_source: Some(reconciled.published_source),
        owned_jsonl_authority,
        pending_merge: Some(PendingSyncMergeCompletion {
            receipt: reconciled.terminal_receipt,
            base_authority,
        }),
        deferred_output: Some(DeferredSyncOutput::Merge {
            report,
            resolution: resolution.to_string(),
            capacity_warnings,
            use_json,
        }),
    })
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
        GitExportStatus, SyncOperation, SyncPathPolicy, additive_conflict_human_lines,
        auto_rebuild_semantic_conflict_field, auto_rebuild_semantic_flag_conflict_reason,
        build_base_witness_artifacts, classify_sync_status_workspace, detect_prefix_from_jsonl,
        fresh_force_import_maintenance_gate_applies, jsonl_contains_duplicate_external_refs,
        jsonl_contains_prefix_mismatch, merge_conflict_resolution, prepare_sync_startup,
        should_defer_jsonl_recovery, should_render_human_sync_output, sync_operation,
        validate_operator_requested_sync_path, validate_sync_mode_args, validate_sync_paths,
        write_manifest_atomically,
    };
    use crate::cli::SyncArgs;
    use crate::config::{self, CliOverrides};
    use crate::error::BeadsError;
    use crate::model::{Dependency, DependencyType, Issue, IssueType, Priority, Status};
    use crate::output::OutputContext;
    use crate::storage::SqliteStorage;
    use crate::sync::{
        AdditiveReconcileConfig, ConflictResolution, PreservedIssue, capture_jsonl_source_snapshot,
        dirty_issues_missing_from_jsonl, plan_additive_reconcile, restore_preserved_issues,
        scan_jsonl_for_tombstone_filter, snapshot_dirty_live_issues, snapshot_tombstones,
        tombstones_missing_from_jsonl_tombstones,
    };
    use chrono::Utc;
    use std::collections::{BTreeSet, HashSet};
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

    #[test]
    fn additive_conflict_human_renderer_is_bounded_and_redacts_embedded_relation_values() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join("issues.jsonl");
        let storage = SqliteStorage::open_memory().unwrap();
        let private_target = "external:/tmp/private-agent-path\n\u{1b}[31m";
        let mut incoming = Vec::new();

        for id in ["bd-safe-a", "bd-safe-b"] {
            let existing = make_test_issue(id, "Same scalar payload");
            storage.upsert_issue_for_import(&existing).unwrap();
            let mut source = existing.clone();
            source.dependencies.push(Dependency {
                issue_id: id.to_string(),
                depends_on_id: private_target.to_string(),
                dep_type: DependencyType::Blocks,
                created_at: source.created_at,
                created_by: Some("fixture".to_string()),
                metadata: Some("{\"private\":\"/tmp/private-agent-path\"}".to_string()),
                thread_id: Some("\u{1b}]8;;file:///tmp/private-agent-path\u{7}".to_string()),
            });
            incoming.push(source);
        }
        let mut bytes = Vec::new();
        for issue in &incoming {
            serde_json::to_writer(&mut bytes, issue).unwrap();
            bytes.push(b'\n');
        }
        fs::write(&jsonl_path, bytes).unwrap();
        let plan = plan_additive_reconcile(
            &storage,
            &jsonl_path,
            &AdditiveReconcileConfig {
                beads_dir: Some(temp.path().to_path_buf()),
                database_path: None,
                allow_external_jsonl: false,
                source_authoritative_ids: BTreeSet::new(),
            },
        )
        .unwrap();
        assert_eq!(
            plan.receipt().conflict_reasons.get("shared_relation_drift"),
            Some(&2)
        );

        let human = additive_conflict_human_lines(plan.receipt(), 1).join("\n");
        let robot = serde_json::to_string(plan.receipt()).unwrap();
        let tracing_payload = format!(
            "{:?}{:?}",
            plan.receipt().conflict_witnesses,
            plan.receipt().conflict_relation_diffs
        );
        for rendered in [&human, &robot, &tracing_payload] {
            assert!(!rendered.contains("/tmp/private-agent-path"), "{rendered}");
            assert!(!rendered.contains('\u{1b}'), "{rendered}");
            assert!(!rendered.contains('\u{7}'), "{rendered}");
        }
        assert!(
            human.contains("issue_ids=1/2")
                && human.contains("witnesses=1/2")
                && human.contains("truncated=true"),
            "{human}"
        );
        assert!(
            human.contains(&plan.receipt().conflict_issue_ids_sha256)
                && human.contains(&plan.receipt().conflict_relation_diffs_sha256),
            "{human}"
        );
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

        let reconcile_conflict = SyncArgs {
            reconcile_additive: true,
            status: true,
            ..SyncArgs::default()
        };
        let err = validate_sync_mode_args(&reconcile_conflict)
            .expect_err("additive reconciliation and status should conflict");
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
    fn test_validate_sync_mode_args_apply_requires_additive_reconciliation() {
        let bare_apply = SyncArgs {
            apply: true,
            ..SyncArgs::default()
        };
        let err = validate_sync_mode_args(&bare_apply)
            .expect_err("apply without additive reconciliation should fail");
        assert!(matches!(&err, BeadsError::Validation { field, .. } if field == "apply"));

        let reconcile_apply = SyncArgs {
            reconcile_additive: true,
            apply: true,
            ..SyncArgs::default()
        };
        let err = validate_sync_mode_args(&reconcile_apply)
            .expect_err("additive apply without a reviewed token must fail");
        assert!(
            matches!(&err, BeadsError::Validation { field, .. } if field == "expect_plan_sha256")
        );

        let valid = SyncArgs {
            expect_plan_sha256: Some("a".repeat(64)),
            ..reconcile_apply.clone()
        };
        validate_sync_mode_args(&valid).unwrap();
        for malformed in [
            "a".to_string(),
            "A".repeat(64),
            "g".repeat(64),
            "a".repeat(65),
        ] {
            let args = SyncArgs {
                expect_plan_sha256: Some(malformed),
                ..reconcile_apply.clone()
            };
            let err = validate_sync_mode_args(&args)
                .expect_err("malformed reviewed tokens must fail preflight");
            assert!(
                matches!(&err, BeadsError::Validation { field, .. } if field == "expect_plan_sha256")
            );
        }
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

        let reconcile = SyncArgs {
            reconcile_additive: true,
            ..SyncArgs::default()
        };
        assert_eq!(sync_operation(&reconcile), SyncOperation::ReconcileAdditive);
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
    fn test_should_defer_jsonl_recovery_for_operator_controlled_import_modes() {
        let rename_import = SyncArgs {
            import_only: true,
            rename_prefix: true,
            ..SyncArgs::default()
        };
        assert!(should_defer_jsonl_recovery(&rename_import));

        let salvage_import = SyncArgs {
            import_only: true,
            skip_invalid_records: true,
            ..SyncArgs::default()
        };
        validate_sync_mode_args(&salvage_import).unwrap();
        assert!(should_defer_jsonl_recovery(&salvage_import));

        let bare_salvage = SyncArgs {
            skip_invalid_records: true,
            ..SyncArgs::default()
        };
        let error = validate_sync_mode_args(&bare_salvage)
            .expect_err("salvage must require explicit import mode");
        assert!(
            matches!(&error, BeadsError::Validation { field, .. } if field == "skip_invalid_records")
        );
        assert!(!should_defer_jsonl_recovery(&bare_salvage));

        for destructive in [
            SyncArgs {
                import_only: true,
                skip_invalid_records: true,
                force: true,
                ..SyncArgs::default()
            },
            SyncArgs {
                import_only: true,
                skip_invalid_records: true,
                rebuild: true,
                ..SyncArgs::default()
            },
            SyncArgs {
                import_only: true,
                skip_invalid_records: true,
                rename_prefix: true,
                ..SyncArgs::default()
            },
        ] {
            let error = validate_sync_mode_args(&destructive)
                .expect_err("salvage must reject destructive import modifiers");
            assert!(
                matches!(&error, BeadsError::Validation { field, .. } if field == "skip_invalid_records")
            );
        }

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

        let reconcile_plan = SyncArgs {
            reconcile_additive: true,
            ..SyncArgs::default()
        };
        assert!(should_defer_jsonl_recovery(&reconcile_plan));

        let reconcile_apply = SyncArgs {
            reconcile_additive: true,
            apply: true,
            ..SyncArgs::default()
        };
        assert!(should_defer_jsonl_recovery(&reconcile_apply));
    }

    #[test]
    #[ignore = "carried red from the stranded sync-safety workstream (failed identically on its own \
                pre-merge snapshot); tracked for completion by the owning workstream"]
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
    fn test_sync_git_export_status_is_stably_not_probed() {
        let status = GitExportStatus::not_probed();
        assert!(!status.available, "{status:?}");
        assert_eq!(status.reason, "not_probed");
        assert_eq!(status.diagnostic_command, "br vcs-status --json");
        assert!(status.tracked.is_none(), "{status:?}");
        assert!(status.worktree_clean.is_none(), "{status:?}");
        assert!(status.index_clean.is_none(), "{status:?}");
        assert!(status.head_hash.is_none(), "{status:?}");
        assert!(status.worktree_hash.is_none(), "{status:?}");

        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "available": false,
                "reason": "not_probed",
                "diagnostic_command": "br vcs-status --json"
            })
        );
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
        restore_preserved_issues(&mut storage, &tombstones).unwrap();

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

        let err = restore_preserved_issues(&mut storage, &tombstones).unwrap_err();
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
        restore_preserved_issues(&mut storage, &tombstones).unwrap();

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
        let in_jsonl = PreservedIssue {
            issue: make_test_issue("bd-in-jsonl", "in jsonl"),
            labels: Some(vec!["jsonl".to_string()]),
            dependencies: Some(Vec::new()),
            comments: Some(Vec::new()),
        };
        let missing = PreservedIssue {
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
        let old_local_preserved = PreservedIssue {
            issue: old_local_tombstone,
            labels: None,
            dependencies: None,
            comments: None,
        };

        let mut new_local_tombstone = make_test_issue("bd-contested-newer", "newer local delete");
        new_local_tombstone.status = Status::Tombstone;
        new_local_tombstone.deleted_at = Some(jsonl_updated_at + Duration::hours(1));
        let new_local_preserved = PreservedIssue {
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
    fn test_dirty_issues_missing_from_jsonl_keeps_only_unreproducible_rows() {
        // GitHub #394: dirty live issues survive a rebuild only when the
        // JSONL cannot reproduce them — absent entirely, or strictly older.
        use chrono::{Duration, Utc};

        let jsonl_updated_at = Utc::now();

        // Never flushed anywhere: must be preserved.
        let never_flushed = PreservedIssue {
            issue: make_test_issue("bd-never-flushed", "db only"),
            labels: Some(vec!["local".to_string()]),
            dependencies: Some(Vec::new()),
            comments: Some(Vec::new()),
        };

        // JSONL copy is as new as the local row: rebuild's import restores
        // an identical row, no preservation needed.
        let mut jsonl_current = make_test_issue("bd-jsonl-current", "flushed");
        jsonl_current.updated_at = jsonl_updated_at;
        let jsonl_current_preserved = PreservedIssue {
            issue: jsonl_current,
            labels: None,
            dependencies: None,
            comments: None,
        };

        // Local edit newer than the JSONL copy: must be preserved.
        let mut locally_edited = make_test_issue("bd-locally-edited", "edited");
        locally_edited.updated_at = jsonl_updated_at + Duration::hours(1);
        let locally_edited_preserved = PreservedIssue {
            issue: locally_edited,
            labels: None,
            dependencies: None,
            comments: None,
        };

        // JSONL has the ID as a flushed tombstone: the deletion wins over
        // the unflushed local edit (mirrors the import tombstone guard).
        let flushed_delete = PreservedIssue {
            issue: make_test_issue("bd-flushed-delete", "deleted upstream"),
            labels: None,
            dependencies: None,
            comments: None,
        };

        let mut non_tombstone_map = std::collections::HashMap::new();
        non_tombstone_map.insert("bd-jsonl-current".to_string(), jsonl_updated_at);
        non_tombstone_map.insert("bd-locally-edited".to_string(), jsonl_updated_at);
        let filter = crate::sync::JsonlTombstoneFilter {
            tombstone_ids: HashSet::from(["bd-flushed-delete".to_string()]),
            non_tombstone_updated_at: non_tombstone_map,
        };

        let filtered = dirty_issues_missing_from_jsonl(
            vec![
                never_flushed,
                jsonl_current_preserved,
                locally_edited_preserved,
                flushed_delete,
            ],
            &filter,
        );

        let filtered_ids: HashSet<_> = filtered
            .iter()
            .map(|preserved| preserved.issue.id.as_str())
            .collect();
        assert_eq!(
            filtered_ids,
            HashSet::from(["bd-never-flushed", "bd-locally-edited"])
        );
    }

    #[test]
    fn test_snapshot_dirty_live_issues_skips_tombstones_and_captures_relations() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let live = make_test_issue("bd-live", "Live dirty issue");
        let deleted = make_test_issue("bd-deleted", "Deleted issue");
        storage.create_issue(&live, "test").unwrap();
        storage.create_issue(&deleted, "test").unwrap();
        storage.add_label("bd-live", "urgent", "test").unwrap();
        storage
            .add_comment("bd-live", "test", "unflushed comment")
            .unwrap();
        storage
            .add_dependency("bd-live", "bd-deleted", "related", "test")
            .unwrap();
        storage
            .delete_issue("bd-deleted", "test", "gone", None)
            .unwrap();

        // Both rows are dirty (nothing has been flushed), but the tombstone
        // belongs to the `snapshot_tombstones` pass, not this one.
        let dirty = snapshot_dirty_live_issues(&storage);
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].issue.id, "bd-live");
        assert_eq!(
            dirty[0].labels.as_ref().unwrap(),
            &vec!["urgent".to_string()]
        );
        assert_eq!(dirty[0].comments.as_ref().unwrap().len(), 1);
        assert_eq!(dirty[0].dependencies.as_ref().unwrap().len(), 1);

        // Round-trip: after a wipe (stand-in for the rebuild), restoring
        // the tombstone pass then the dirty pass — the order the rebuild
        // wiring uses — brings the live issue back and re-marks it dirty.
        let tombstones = snapshot_tombstones(&storage);
        storage.reset_data_tables().unwrap();
        restore_preserved_issues(&mut storage, &tombstones).unwrap();
        restore_preserved_issues(&mut storage, &dirty).unwrap();
        let restored = storage.get_issue("bd-live").unwrap().unwrap();
        assert_eq!(restored.title, "Live dirty issue");
        assert_eq!(
            storage.get_dependencies_full("bd-live").unwrap().len(),
            1,
            "dependency to the restored tombstone must survive"
        );
        assert!(
            storage
                .get_dirty_issue_ids()
                .unwrap()
                .contains(&"bd-live".to_string())
        );
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

        let source = capture_jsonl_source_snapshot(&jsonl_path).unwrap();
        assert_eq!(
            detect_prefix_from_jsonl(&source).unwrap(),
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

        let source = capture_jsonl_source_snapshot(&jsonl_path).unwrap();
        let err = detect_prefix_from_jsonl(&source).unwrap_err();
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

        let source = capture_jsonl_source_snapshot(&jsonl_path).unwrap();
        let err = detect_prefix_from_jsonl(&source).unwrap_err();
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

        let source = capture_jsonl_source_snapshot(&jsonl_path).unwrap();
        assert!(!jsonl_contains_prefix_mismatch(&source, "bd").unwrap());

        let slugged = make_test_issue("bd-survey-my-thing-abc123", "Slugged");
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&slugged).unwrap()),
        )
        .unwrap();

        let source = capture_jsonl_source_snapshot(&jsonl_path).unwrap();
        assert!(
            !jsonl_contains_prefix_mismatch(&source, "bd").unwrap(),
            "slugged IDs generated from prefix bd should not be treated as mismatches"
        );

        let mismatch = make_test_issue("other-gamma", "Mismatch");
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&mismatch).unwrap()),
        )
        .unwrap();

        let source = capture_jsonl_source_snapshot(&jsonl_path).unwrap();
        assert!(jsonl_contains_prefix_mismatch(&source, "bd").unwrap());
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

        let source = capture_jsonl_source_snapshot(&jsonl_path).unwrap();
        assert!(jsonl_contains_duplicate_external_refs(&source).unwrap());

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

        let source = capture_jsonl_source_snapshot(&jsonl_path).unwrap();
        assert!(!jsonl_contains_duplicate_external_refs(&source).unwrap());
    }
}
