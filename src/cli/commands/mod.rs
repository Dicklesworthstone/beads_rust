use crate::config::OpenStorageResult;
use crate::error::BeadsError;
use crate::format::sanitize_terminal_text;
use crate::model::Issue;
use crate::output::OutputContext;
use crate::storage::{IssueUpdate, SqliteStorage};
use crate::sync::auto_import_if_stale;
use crate::util::id::IdResolver;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod agents;
pub mod audit;
pub mod blocked;
pub mod capabilities;
pub mod capacity;
pub mod changelog;
pub mod close;
pub mod comments;
pub mod completions;
pub mod config;
pub mod coordination;
pub mod count;
pub mod create;
pub mod defer;
pub mod delete;
pub mod dep;
pub mod doctor;
pub mod doctor_subsystems;
pub mod epic;
pub mod gate;
pub mod graph;
pub mod history;
pub mod info;
pub mod init;
pub mod label;
pub mod lint;
pub mod list;
pub mod orphans;
pub mod q;
pub mod query;
pub mod ready;
pub mod reopen;
pub mod robot_docs;
pub mod scheduler;
pub mod schema;
pub mod search;
pub mod show;
pub mod stale;
pub mod stats;
pub mod sync;
pub mod update;
pub mod vcs;
pub mod version;
pub mod r#where;

#[cfg(feature = "self_update")]
pub mod upgrade;

pub(crate) const GITHUB_REPO_OWNER: &str = "Dicklesworthstone";
pub(crate) const GITHUB_REPO_NAME: &str = "beads_rust";

#[must_use]
pub(crate) fn github_latest_release_api_url() -> String {
    format!("https://api.github.com/repos/{GITHUB_REPO_OWNER}/{GITHUB_REPO_NAME}/releases/latest")
}

#[cfg(feature = "self_update")]
#[must_use]
pub(crate) fn github_releases_url() -> String {
    format!("https://github.com/{GITHUB_REPO_OWNER}/{GITHUB_REPO_NAME}/releases")
}

#[cfg(feature = "self_update")]
#[must_use]
pub(crate) fn github_raw_main_url(path: &str) -> String {
    format!("https://raw.githubusercontent.com/{GITHUB_REPO_OWNER}/{GITHUB_REPO_NAME}/main/{path}")
}

/// Report a post-mutation auto-flush failure without corrupting command stdout.
///
/// The data mutation has already succeeded by the time this is called. The
/// safest remaining action is to make the sync debt visible on stderr and leave
/// the operator with an explicit `sync --flush-only` recovery path.
pub fn report_auto_flush_failure(
    ctx: &OutputContext,
    beads_dir: &Path,
    jsonl_path: &Path,
    error: &BeadsError,
) {
    tracing::warn!(
        beads_dir = %beads_dir.display(),
        jsonl_path = %jsonl_path.display(),
        error = %error,
        "Mutation succeeded but auto-flush failed"
    );

    if ctx.is_quiet() {
        return;
    }

    let message = "Mutation succeeded, but automatic JSONL export failed. \
                   Fix the export problem, run `br sync --flush-only`, then commit \
                   the updated .beads/issues.jsonl.";
    let error_text = error.to_string();

    if ctx.is_json() || ctx.is_toon() {
        let payload = serde_json::json!({
            "warning": {
                "code": "AUTO_FLUSH_FAILED",
                "message": message,
                "beads_dir": beads_dir.display().to_string(),
                "jsonl_path": jsonl_path.display().to_string(),
                "error": error_text,
                "recovery": "Run br sync --flush-only after fixing the export problem before committing .beads/issues.jsonl"
            }
        });
        eprintln!(
            "{}",
            serde_json::to_string(&payload).unwrap_or_else(|_| {
                "{\"warning\":{\"code\":\"AUTO_FLUSH_FAILED\"}}".to_string()
            })
        );
        return;
    }

    let warning = format!(
        "Warning: {message} JSONL path: {}. Error: {error_text}",
        jsonl_path.display()
    );
    eprintln!("{}", sanitize_terminal_text(&warning));
}

/// Resolve an issue ID from a potentially partial input.
pub(super) fn resolve_issue_id(
    storage: &SqliteStorage,
    resolver: &IdResolver,
    input: &str,
) -> crate::Result<String> {
    resolver
        .resolve_fallible(
            input,
            |id| storage.id_exists(id),
            |hash| storage.find_ids_by_hash(hash),
        )
        .map(|resolved| resolved.id)
}

pub(super) fn resolve_issue_ids(
    storage: &SqliteStorage,
    resolver: &IdResolver,
    inputs: &[String],
) -> crate::Result<Vec<String>> {
    resolver
        .resolve_all_fallible(
            inputs,
            |id| storage.id_exists(id),
            |hash| storage.find_ids_by_hash(hash),
        )
        .map(|resolved| resolved.into_iter().map(|entry| entry.id).collect())
}

pub(super) fn rebuild_blocked_cache_after_partial_mutation(
    storage: &mut SqliteStorage,
    cache_dirty: bool,
    command: &str,
) -> crate::Result<()> {
    if !cache_dirty {
        return Ok(());
    }

    match storage.mark_blocked_cache_stale() {
        Ok(()) => {
            tracing::debug!(
                command = command,
                "Blocked cache repair deferred after partial mutation; cache remains marked stale"
            );
            Ok(())
        }
        Err(mark_error) => {
            tracing::warn!(
                command = command,
                error = %mark_error,
                "Failed to pre-mark blocked cache stale before rebuilding after partial mutation"
            );
            storage
                .rebuild_blocked_cache(true)
                .map(|_| ())
                .map_err(|rebuild_err| crate::error::BeadsError::WithContext {
                    context: format!(
                        "failed to rebuild blocked cache after partial {command} mutation; \
                         pre-marking it stale also failed: {mark_error}"
                    ),
                    source: Box::new(rebuild_err),
                })
        }
    }
}

pub(super) fn preserve_blocked_cache_on_error<T>(
    storage: &mut SqliteStorage,
    cache_dirty: bool,
    command: &str,
    result: crate::Result<T>,
) -> crate::Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(operation_err) => {
            if let Err(rebuild_err) =
                rebuild_blocked_cache_after_partial_mutation(storage, cache_dirty, command)
            {
                return Err(crate::error::BeadsError::WithContext {
                    context: format!(
                        "failed to preserve blocked cache after partial {command} mutation; original operation error: {operation_err}"
                    ),
                    source: Box::new(rebuild_err),
                });
            }
            Err(operation_err)
        }
    }
}

pub(super) fn finalize_batched_blocked_cache_refresh(
    storage: &mut SqliteStorage,
    cache_dirty: bool,
    command: &str,
) -> crate::Result<()> {
    if !cache_dirty {
        return Ok(());
    }

    if !storage.blocked_cache_marked_stale().unwrap_or(false)
        && let Err(mark_error) = storage.mark_blocked_cache_stale()
    {
        tracing::warn!(
            command = command,
            error = %mark_error,
            "Failed to pre-mark blocked cache stale before batched refresh"
        );
        return storage
            .rebuild_blocked_cache(true)
            .map(|_| ())
            .map_err(|rebuild_err| crate::error::BeadsError::WithContext {
                context: format!(
                    "failed to rebuild blocked cache after successful batched {command} mutation; \
                     leaving the cache stale also failed first: {mark_error}"
                ),
                source: Box::new(rebuild_err),
            });
    }

    match storage.ensure_blocked_cache_fresh() {
        Ok(rebuilt) => {
            tracing::debug!(
                command = command,
                rebuilt = rebuilt,
                "Blocked cache refreshed after successful batched mutation"
            );
            Ok(())
        }
        Err(rebuild_error) => {
            tracing::warn!(
                command = command,
                error = %rebuild_error,
                "Blocked cache refresh failed after successful batched mutation; preserving stale marker"
            );
            storage.mark_blocked_cache_stale().map_err(|mark_error| {
                crate::error::BeadsError::WithContext {
                    context: format!(
                        "failed to preserve blocked cache stale marker after successful batched {command} mutation; \
                         original refresh error: {rebuild_error}"
                    ),
                    source: Box::new(mark_error),
                }
            })
        }
    }
}

/// Whether a CLI status filter contains the `all` meta-value
/// (case-insensitive), meaning "every status" — the same convention
/// `br lint --status all` uses. Without this, `all` would parse as the
/// custom status literal `Custom("all")` and silently match nothing
/// (beads_rust-6ilv).
pub(super) fn status_filter_requests_all(statuses: &[String]) -> bool {
    statuses
        .iter()
        .any(|status| status.trim().eq_ignore_ascii_case("all"))
}

/// Self-reported session identity for capacity occupancy and the `session`
/// capacity scope (GitHub #384 phase 5). Env-only (`BR_SESSION`): a CLI flag
/// would collide with `br close --session`, which records close metadata,
/// and session identity is a harness-level property anyway.
pub(super) fn session_attribution_from_env() -> Option<String> {
    std::env::var("BR_SESSION")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(super) fn update_issues_atomically_with_recovery(
    storage_ctx: &mut OpenStorageResult,
    allow_recovery: bool,
    command: &str,
    updates: &[(String, IssueUpdate)],
    actor: &str,
) -> crate::Result<Vec<Issue>> {
    retry_mutation_with_jsonl_recovery(storage_ctx, allow_recovery, command, None, |storage| {
        storage.update_issues_atomically(updates, actor)
    })
}

fn should_attempt_mutation_jsonl_recovery(
    storage_ctx: &OpenStorageResult,
    operation_err: &BeadsError,
    probe_err: Option<&BeadsError>,
) -> bool {
    matches!(operation_err, BeadsError::Database(_))
        && (storage_ctx.should_attempt_jsonl_recovery(operation_err)
            || probe_err.is_some_and(|err| storage_ctx.should_attempt_jsonl_recovery(err)))
}

pub(super) fn auto_import_storage_ctx_if_stale(
    storage_ctx: &mut OpenStorageResult,
    cli: &crate::config::CliOverrides,
) -> crate::Result<()> {
    // Issue #229: skip auto-import in --no-db mode.  The in-memory database
    // was just populated from the JSONL file during `open_storage_with_cli`,
    // so there is no staleness to detect.  Running the staleness probe here
    // is actively harmful because `compute_staleness_refreshing_witnesses`
    // calls `get_metadata` via `query_row_with_params`, which routes through
    // frankensqlite's prepared-statement fast path.  On in-memory databases
    // that fast path can warm up cached root-page references that become
    // stale after the bulk import's DELETE + INSERT cycle, causing subsequent
    // `get_issue_from_conn` calls inside write transactions to silently
    // return zero rows — the mechanism behind the "Issue not found" errors
    // on `br --no-db update`.
    if storage_ctx.no_db {
        return Ok(());
    }

    let config_layer = storage_ctx.load_config(cli)?;
    let no_auto_import = crate::config::no_auto_import_from_layer(&config_layer).unwrap_or(false);
    let allow_external_jsonl = crate::config::implicit_external_jsonl_allowed(
        &storage_ctx.paths.beads_dir,
        &storage_ctx.paths.db_path,
        &storage_ctx.paths.jsonl_path,
    );
    let expected_prefix = crate::config::id_config_from_layer(&config_layer).prefix;

    auto_import_if_stale(
        &mut storage_ctx.storage,
        &storage_ctx.paths.beads_dir,
        &storage_ctx.paths.jsonl_path,
        Some(expected_prefix.as_str()),
        allow_external_jsonl,
        cli.allow_stale.unwrap_or(false),
        no_auto_import,
    )
    .map(|_| ())
}

pub(super) fn cli_for_routed_workspace(
    cli: &crate::config::CliOverrides,
    is_external: bool,
) -> crate::config::CliOverrides {
    let mut route_cli = cli.clone();
    if is_external {
        route_cli.db = None;
        route_cli.read_only_fast_open = false;
    }
    route_cli
}

pub(super) fn auto_import_external_projects_if_stale(
    config_layer: &crate::config::ConfigLayer,
    local_beads_dir: &Path,
    cli: &crate::config::CliOverrides,
) {
    if cli.allow_stale.unwrap_or(false)
        || cli.no_auto_import.unwrap_or(false)
        || cli.no_db.unwrap_or(false)
        || crate::config::no_db_from_layer(config_layer).unwrap_or(false)
        || crate::config::no_auto_import_from_layer(config_layer).unwrap_or(false)
    {
        return;
    }

    for (project, beads_dir) in
        crate::config::external_project_beads_dirs(config_layer, local_beads_dir)
    {
        let paths = match crate::config::ConfigPaths::resolve(&beads_dir, None) {
            Ok(paths) => paths,
            Err(error) => {
                tracing::warn!(
                    project = %project,
                    path = %beads_dir.display(),
                    error = %error,
                    "Skipping external project auto-import because path resolution failed"
                );
                continue;
            }
        };

        if !paths.db_path.is_file() && !paths.jsonl_path.is_file() {
            continue;
        }

        let mut route_cli = cli_for_routed_workspace(cli, true);
        let routed_write_lock = match acquire_routed_workspace_write_lock(
            &beads_dir,
            true,
            route_cli.lock_timeout,
        ) {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(
                    project = %project,
                    path = %beads_dir.display(),
                    error = %error,
                    "Skipping external project auto-import because the workspace write lock could not be acquired"
                );
                continue;
            }
        };
        routed_write_lock.mark_cli_write_lock_held(&mut route_cli);

        let mut storage_ctx = match crate::config::open_storage_with_cli(&beads_dir, &route_cli) {
            Ok(storage_ctx) => storage_ctx,
            Err(error) => {
                tracing::warn!(
                    project = %project,
                    path = %beads_dir.display(),
                    error = %error,
                    "Skipping external project auto-import because storage could not be opened"
                );
                continue;
            }
        };

        if let Err(error) = auto_import_storage_ctx_if_stale(&mut storage_ctx, &route_cli) {
            tracing::warn!(
                project = %project,
                path = %beads_dir.display(),
                error = %error,
                "External project auto-import failed; dependency status will use the current database state"
            );
        }
    }
}

pub(super) fn external_project_db_paths_after_auto_import_if_needed(
    storage: &SqliteStorage,
    config_layer: &crate::config::ConfigLayer,
    beads_dir: &Path,
    cli: &crate::config::CliOverrides,
) -> crate::Result<HashMap<String, PathBuf>> {
    if !storage.has_external_dependencies(true)? {
        return Ok(HashMap::new());
    }

    auto_import_external_projects_if_stale(config_layer, beads_dir, cli);
    Ok(crate::config::external_project_db_paths(
        config_layer,
        beads_dir,
    ))
}

pub(super) struct RoutedWorkspaceWriteLock {
    lock: Option<Arc<crate::sync::DatabaseFamilyWriteLock>>,
    beads_dir: Option<PathBuf>,
}

/// What a routed mutation leaves behind after its writes: the connection that
/// performed them and the routed workspace's write lock. The storage is
/// declared first so it (and the WAL checkpoint its drop performs, #270) is
/// released before the family write lock, exactly as when both were locals of
/// the route function. A command that borrowed `main`'s startup connection
/// hands `storage_ctx` back through this so the startup auto-flush runs
/// through the connection that performed the write (bead naul5).
pub(super) struct RouteResources {
    pub(super) storage_ctx: OpenStorageResult,
    pub(super) _routed_write_lock: RoutedWorkspaceWriteLock,
}

/// Whether two `.beads` directories name the same workspace, tolerating
/// symlinked or relative spellings of one path.
pub(super) fn same_workspace(left: &Path, right: &Path) -> bool {
    let canonical = |path: &Path| dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(left) == canonical(right)
}

impl RoutedWorkspaceWriteLock {
    #[must_use]
    pub(super) const fn local() -> Self {
        Self {
            lock: None,
            beads_dir: None,
        }
    }

    pub(super) fn mark_cli_write_lock_held(&self, cli: &mut crate::config::CliOverrides) {
        if let (Some(beads_dir), Some(lock)) = (&self.beads_dir, &self.lock) {
            cli.mark_database_family_lock_held(beads_dir, lock);
        }
    }
}

pub(super) fn acquire_routed_workspace_write_lock(
    beads_dir: &Path,
    is_external: bool,
    lock_timeout_ms: Option<u64>,
) -> crate::Result<RoutedWorkspaceWriteLock> {
    if !is_external {
        return Ok(RoutedWorkspaceWriteLock::local());
    }

    let startup = crate::config::load_startup_config_with_paths(beads_dir, None)?;
    let lock_path = beads_dir.join(".write.lock");
    let lock = crate::sync::blocking_database_family_write_lock_with_timeout(
        beads_dir,
        &startup.paths.db_path,
        lock_timeout_ms,
    )
    .map_err(|err| {
            BeadsError::Config(format!(
                "Routed external workspace is busy: target write lock at {} could not be acquired: {err}",
                lock_path.display()
            ))
        })?;
    Ok(RoutedWorkspaceWriteLock {
        lock: Some(Arc::new(lock)),
        beads_dir: Some(beads_dir.to_path_buf()),
    })
}

fn retry_after_stale_wal_certificate<T, F>(
    storage_ctx: &mut OpenStorageResult,
    command: &str,
    operation_err: &BeadsError,
    operation: &mut F,
) -> Option<crate::Result<T>>
where
    F: FnMut(&mut SqliteStorage) -> crate::Result<T>,
{
    if !crate::config::is_stale_wal_certificate_version_error(operation_err)
        || !storage_ctx.paths.db_path.is_file()
        || storage_ctx.no_db
    {
        return None;
    }

    match storage_ctx.quarantine_stale_wal_certificate_sidecars_and_reopen() {
        Ok(quarantined_paths) => {
            tracing::warn!(
                command = command,
                original_error = %operation_err,
                db_path = %storage_ctx.paths.db_path.display(),
                quarantined_paths = ?quarantined_paths,
                "Write hit a stale parallel-WAL certificate sidecar; quarantined the certificate sidecars and retrying the write once"
            );
            Some(operation(&mut storage_ctx.storage).map_err(|retry_err| {
                BeadsError::WithContext {
                    context: format!(
                        "retry after quarantining stale WAL-certificate sidecars ({}) failed during {command} write; original write error: {operation_err}",
                        quarantined_paths
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    source: Box::new(retry_err),
                }
            }))
        }
        Err(quarantine_err) if storage_ctx.database_transition_is_fail_closed() => {
            tracing::error!(
                command = command,
                original_error = %operation_err,
                quarantine_error = %quarantine_err,
                db_path = %storage_ctx.paths.db_path.display(),
                "Stale parallel-WAL certificate recovery left storage fail-closed; refusing JSONL recovery"
            );
            Some(Err(BeadsError::WithContext {
                context: format!(
                    "stale WAL-certificate recovery failed during {command} write and left storage without a verified persistent database handle; refusing JSONL recovery; original write error: {operation_err}"
                ),
                source: Box::new(quarantine_err),
            }))
        }
        Err(quarantine_err) => {
            tracing::warn!(
                command = command,
                original_error = %operation_err,
                quarantine_error = %quarantine_err,
                db_path = %storage_ctx.paths.db_path.display(),
                "Failed to quarantine stale parallel-WAL certificate sidecars after restoring persistent storage; evaluating JSONL recovery"
            );
            None
        }
    }
}

pub(super) fn retry_mutation_with_jsonl_recovery<T, F>(
    storage_ctx: &mut OpenStorageResult,
    allow_recovery: bool,
    command: &str,
    probe_issue_id: Option<&str>,
    mut operation: F,
) -> crate::Result<T>
where
    F: FnMut(&mut SqliteStorage) -> crate::Result<T>,
{
    match operation(&mut storage_ctx.storage) {
        Ok(value) => Ok(value),
        Err(operation_err) => {
            if !allow_recovery || !matches!(operation_err, BeadsError::Database(_)) {
                return Err(operation_err);
            }

            // GitHub #441: a `-wal-cert` sidecar left behind by a different
            // fsqlite engine generation fails every cert-regenerating write
            // while reads stay healthy. The certificate is derived state, so
            // the fix is to quarantine the sidecar pair (verified backups in
            // `.br_recovery`; the `-wal` file is real data and is never
            // touched), reopen the healthy database in place, and retry the
            // write once — without the JSONL rebuild below, which is heavier
            // and wedges when the JSONL merge anchor is stale. Only attempted
            // while the main database file is present: a missing database is
            // the orphaned-sidecar open path's job, not this one's.
            if let Some(result) = retry_after_stale_wal_certificate(
                storage_ctx,
                command,
                &operation_err,
                &mut operation,
            ) {
                return result;
            }

            let mut recovery_signal =
                should_attempt_mutation_jsonl_recovery(storage_ctx, &operation_err, None);
            let mut probe_error: Option<BeadsError> = None;

            if !recovery_signal && let Some(issue_id) = probe_issue_id {
                match storage_ctx
                    .storage
                    .probe_issue_mutation_write_path(issue_id)
                {
                    Ok(()) => return Err(operation_err),
                    Err(probe_err) => {
                        recovery_signal = should_attempt_mutation_jsonl_recovery(
                            storage_ctx,
                            &operation_err,
                            Some(&probe_err),
                        );
                        probe_error = Some(probe_err);
                    }
                }
            }

            if !recovery_signal {
                return Err(operation_err);
            }

            let issue_id_label = probe_issue_id.unwrap_or("<none>");
            let probe_error_display = probe_error
                .as_ref()
                .map_or_else(|| "n/a".to_string(), std::string::ToString::to_string);
            tracing::warn!(
                command = command,
                issue_id = issue_id_label,
                original_error = %operation_err,
                probe_error = %probe_error_display,
                db_path = %storage_ctx.paths.db_path.display(),
                jsonl_path = %storage_ctx.paths.jsonl_path.display(),
                "Mutation hit a recoverable database corruption path; rebuilding from JSONL and retrying once"
            );

            let original_error = operation_err.to_string();
            storage_ctx.recover_database_from_jsonl().map_err(|recovery_err| {
                BeadsError::WithContext {
                    context: probe_issue_id.map_or_else(
                        || {
                            format!(
                                "automatic database recovery failed after {command} write; original write error: {original_error}"
                            )
                        },
                        |issue_id| {
                        format!(
                            "automatic database recovery failed after {command} write for issue '{issue_id}'; original write error: {original_error}"
                        )
                        },
                    ),
                    source: Box::new(recovery_err),
                }
            })?;

            operation(&mut storage_ctx.storage)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_routed_workspace_write_lock, finalize_batched_blocked_cache_refresh,
        preserve_blocked_cache_on_error, rebuild_blocked_cache_after_partial_mutation,
        retry_mutation_with_jsonl_recovery, should_attempt_mutation_jsonl_recovery,
    };
    use crate::config::{CliOverrides, OpenStorageResult, open_storage_with_cli};
    use crate::error::BeadsError;
    use crate::franken_sync::Connection;
    use crate::model::Issue;
    use crate::storage::SqliteStorage;
    use crate::sync::{ExportConfig, export_to_jsonl_with_policy};
    use chrono::Utc;
    use fsqlite_error::FrankenError;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn storage_ctx_with_exported_issue() -> (TempDir, OpenStorageResult) {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Scope the initial storage so the connection is closed before
        // recovery opens a new one at the same path.  fsqlite tracks pages
        // by file path, so an older connection causes BusySnapshot.
        {
            let mut storage = SqliteStorage::open(&db_path).expect("storage");
            let issue = Issue {
                id: "bd-1".to_string(),
                title: "test".to_string(),
                ..Issue::default()
            };
            storage
                .create_issue(&issue, "tester")
                .expect("create issue");
            let export_config = ExportConfig {
                beads_dir: Some(beads_dir.clone()),
                ..Default::default()
            };
            export_to_jsonl_with_policy(&storage, &jsonl_path, &export_config)
                .expect("export jsonl");
        }

        let storage_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage ctx");
        (temp, storage_ctx)
    }

    fn write_single_issue_jsonl(path: &Path, id: &str, title: &str) {
        let now = Utc::now();
        let issue = Issue {
            id: id.to_string(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            ..Issue::default()
        };
        let json = serde_json::to_string(&issue).expect("serialize issue");
        fs::write(path, format!("{json}\n")).expect("write jsonl");
    }

    #[test]
    fn routed_workspace_write_lock_respects_external_timeout() -> std::result::Result<(), String> {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let _held = crate::sync::blocking_write_lock(&beads_dir).expect("hold write lock");
        let result = acquire_routed_workspace_write_lock(&beads_dir, true, Some(1));
        let err = result.err().ok_or_else(|| {
            "external routed lock should wait for and time out on held lock".to_string()
        })?;
        let message = err.to_string();
        assert!(
            message.contains("Routed external workspace is busy")
                && message.contains("target write lock")
                && message.contains("Timed out after 1ms waiting for write lock"),
            "{message}"
        );
        Ok(())
    }

    #[test]
    fn routed_workspace_write_lock_marks_cli_for_fast_open_recovery() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        write_single_issue_jsonl(
            &jsonl_path,
            "bd-routed",
            "Recovered under routed write lock",
        );

        let routed_write_lock =
            acquire_routed_workspace_write_lock(&beads_dir, true, Some(1)).expect("routed lock");
        let mut cli = CliOverrides {
            lock_timeout: Some(1),
            read_only_fast_open: true,
            ..CliOverrides::default()
        };
        routed_write_lock.mark_cli_write_lock_held(&mut cli);

        let storage_ctx =
            open_storage_with_cli(&beads_dir, &cli).expect("recovery should reuse routed lock");
        let issue = storage_ctx
            .storage
            .get_issue("bd-routed")
            .expect("query issue")
            .expect("issue should be rebuilt from JSONL");

        assert_eq!(issue.title, "Recovered under routed write lock");
        assert!(db_path.is_file(), "database should be rebuilt from JSONL");
    }

    #[test]
    fn partial_mutation_rebuild_skips_clean_state() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        rebuild_blocked_cache_after_partial_mutation(&mut storage, false, "close")
            .expect("clean state should not rebuild");
    }

    #[test]
    fn preserve_returns_original_error_when_cache_is_marked_stale() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        let result: crate::Result<()> = Err(BeadsError::validation("ids", "boom"));
        let err = preserve_blocked_cache_on_error::<()>(&mut storage, true, "close", result)
            .expect_err("operation should still fail");

        assert!(matches!(err, BeadsError::Validation { .. }));
    }

    #[test]
    fn preserve_surfaces_rebuild_failure_when_stale_marker_write_also_fails() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("conn");
        conn.execute("DROP TABLE blocked_issues_cache")
            .expect("drop blocked cache table");
        conn.execute("DROP TABLE metadata")
            .expect("drop metadata table");

        let result: crate::Result<()> = Err(BeadsError::validation("ids", "boom"));
        let err = preserve_blocked_cache_on_error::<()>(&mut storage, true, "reopen", result)
            .expect_err("rebuild failure should be surfaced");

        assert!(
            matches!(err, BeadsError::WithContext { .. }),
            "expected WithContext, got {err:?}"
        );
        if let BeadsError::WithContext { context, .. } = err {
            assert!(context.contains("partial reopen mutation"));
            assert!(context.contains("Validation failed: ids: boom"));
        }

        let metadata_probe = storage.get_metadata("blocked_cache_state");
        assert!(
            metadata_probe.is_err(),
            "metadata lookup should fail once the metadata table has been dropped"
        );
    }

    #[test]
    fn finalize_batched_refresh_rebuilds_when_cache_table_is_missing() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("conn");
        conn.execute("DROP TABLE blocked_issues_cache")
            .expect("drop blocked cache table");

        finalize_batched_blocked_cache_refresh(&mut storage, true, "close")
            .expect("batched refresh should recreate missing cache table");

        assert!(
            !storage.blocked_cache_marked_stale().unwrap(),
            "successful finalization should clear the stale marker"
        );
        let table_exists = storage
            .execute_raw_query(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'blocked_issues_cache'",
            )
            .expect("query sqlite_master");
        assert_eq!(
            table_exists.len(),
            1,
            "blocked cache table should be recreated"
        );
    }

    #[test]
    fn finalize_batched_refresh_clears_preexisting_stale_marker() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .mark_blocked_cache_stale()
            .expect("mark cache stale before finalization");

        finalize_batched_blocked_cache_refresh(&mut storage, true, "close")
            .expect("pre-marked stale cache should be rebuilt cleanly");

        assert!(
            !storage.blocked_cache_marked_stale().unwrap(),
            "successful finalization should clear a preexisting stale marker"
        );
    }

    #[test]
    fn retry_mutation_recovers_from_recoverable_database_error() {
        let (_temp, mut storage_ctx) = storage_ctx_with_exported_issue();
        let mut attempts = 0;

        let result = retry_mutation_with_jsonl_recovery(
            &mut storage_ctx,
            true,
            "test-mutation",
            Some("bd-1"),
            |_storage| {
                attempts += 1;
                if attempts == 1 {
                    Err(BeadsError::Database(FrankenError::DatabaseCorrupt {
                        detail: "synthetic corruption".to_string(),
                    }))
                } else {
                    Ok("recovered")
                }
            },
        )
        .expect("recovered mutation");

        assert_eq!(result, "recovered");
        assert_eq!(attempts, 2);
        assert!(
            storage_ctx
                .storage
                .get_issue("bd-1")
                .expect("load issue")
                .is_some()
        );
    }

    #[test]
    fn stale_wal_certificate_recovery_surfaces_fail_closed_transition() {
        let (_temp, mut storage_ctx) = storage_ctx_with_exported_issue();
        storage_ctx.mark_database_transition_failed_closed_for_test();
        let mut attempts = 0;

        let error = retry_mutation_with_jsonl_recovery(
            &mut storage_ctx,
            true,
            "update",
            Some("bd-1"),
            |_storage| -> crate::Result<()> {
                attempts += 1;
                Err(BeadsError::Database(FrankenError::WalCorrupt {
                    detail: "parallel WAL certificate suffix has unsupported record version 999"
                        .to_string(),
                }))
            },
        )
        .expect_err("a fail-closed storage transition must stop mutation recovery");

        assert_eq!(attempts, 1, "fail-closed recovery must not retry the write");
        let BeadsError::WithContext { context, source } = error else {
            panic!("expected transition context, got {error:?}");
        };
        assert!(
            context.contains("without a verified persistent database handle")
                && context.contains("refusing JSONL recovery")
                && context.contains("unsupported record version 999"),
            "unexpected retry context: {context}"
        );
        assert!(
            source
                .to_string()
                .contains("prior database-handle transition"),
            "the causal transition error must be preserved: {source}"
        );
    }

    #[test]
    fn retry_preserves_staged_attribution_across_jsonl_recovery() {
        // #312 hardening (F1): attribution staged ONCE before the retry helper
        // must survive a recoverable first-attempt failure and still be stamped
        // by the post-recovery retry. The attribution is staged exactly once,
        // OUTSIDE the operation closure, so this genuinely exercises the
        // take-after-commit fix (not a re-stage workaround).
        let (_temp, mut storage_ctx) = storage_ctx_with_exported_issue();
        let mut attempts = 0;

        storage_ctx
            .storage
            .set_pending_event_attribution(crate::storage::EventAttribution::new(
                Some("agent-recovered"),
                None,
                Some("opus-4"),
                None,
            ));

        retry_mutation_with_jsonl_recovery(
            &mut storage_ctx,
            true,
            "update_issue",
            Some("bd-1"),
            |storage| {
                attempts += 1;
                if attempts == 1 {
                    // First attempt: a recoverable corruption error that does NOT
                    // commit. The staged attribution must NOT be consumed.
                    Err(BeadsError::Database(FrankenError::DatabaseCorrupt {
                        detail: "synthetic corruption".to_string(),
                    }))
                } else {
                    // Post-recovery retry: a real committing mutation that should
                    // stamp the still-staged attribution onto its event.
                    let update = crate::storage::IssueUpdate {
                        status: Some(crate::model::Status::InProgress),
                        ..Default::default()
                    };
                    storage.update_issue("bd-1", &update, "tester").map(|_| ())
                }
            },
        )
        .expect("recovered mutation should stamp staged attribution");

        assert_eq!(attempts, 2, "should recover and retry exactly once");

        let events = storage_ctx
            .storage
            .get_events("bd-1", 0)
            .expect("load events");
        let status_event = events
            .iter()
            .find(|e| e.event_type == crate::model::EventType::StatusChanged)
            .expect("status_changed event present after recovery");
        assert_eq!(
            status_event.agent_name.as_deref(),
            Some("agent-recovered"),
            "attribution must survive the JSONL-recovery retry (F1)"
        );
        assert_eq!(status_event.model.as_deref(), Some("opus-4"));
    }

    #[test]
    fn stale_wal_certificate_sidecar_write_quarantines_and_retries() {
        // GitHub #441: a `-wal-cert` sidecar left behind by a different
        // fsqlite engine generation fails every cert-regenerating write with
        // `WAL file is corrupt: parallel WAL certificate suffix has
        // unsupported record version N` while reads stay healthy. The retry
        // helper must quarantine the certificate sidecar pair into
        // `.br_recovery` (verified backups, bytes preserved), reopen the
        // healthy database in place, and retry the write — never touching
        // the `-wal` file itself and never depending on a JSONL rebuild.
        let (_temp, mut storage_ctx) = storage_ctx_with_exported_issue();
        let db_path = storage_ctx.paths.db_path.clone();
        let beads_dir = storage_ctx.paths.beads_dir.clone();
        let wal_path = std::path::PathBuf::from(format!("{}-wal", db_path.display()));
        let cert_path = std::path::PathBuf::from(format!("{}-wal-cert", db_path.display()));
        let head_path = std::path::PathBuf::from(format!("{}-wal-cert-head", db_path.display()));

        // Fabricate a durable-certificate sidecar whose envelope version is
        // strictly newer than this build's
        // `PARALLEL_WAL_DURABLE_CERTIFICATE_RECORD_VERSION`: fsqlite's
        // append-boundary repair (`validate_incomplete_certificate_suffix`)
        // rejects it as `WalCorrupt { .. unsupported record version .. }`.
        // Layout per fsqlite-wal 0.3.9: 8-byte `FSQLCERT` magic, then a
        // little-endian u16 record version at bytes 8..10.
        let mut stale_cert = Vec::from(*b"FSQLCERT");
        stale_cert.extend_from_slice(&999_u16.to_le_bytes());
        fs::write(&cert_path, &stale_cert).expect("write stale wal-cert sidecar");
        fs::write(&head_path, b"stale-cert-head").expect("write stale wal-cert-head sidecar");
        let wal_existed_before = wal_path.exists();

        let mut attempts = 0;
        retry_mutation_with_jsonl_recovery(
            &mut storage_ctx,
            true,
            "update",
            Some("bd-1"),
            |storage| {
                attempts += 1;
                let update = crate::storage::IssueUpdate {
                    status: Some(crate::model::Status::InProgress),
                    ..Default::default()
                };
                storage.update_issue("bd-1", &update, "tester").map(|_| ())
            },
        )
        .expect("write should succeed after stale wal-cert quarantine");
        assert_eq!(
            attempts, 2,
            "the stale certificate should fail exactly one attempt before the quarantined retry"
        );

        let updated = storage_ctx
            .storage
            .get_issue("bd-1")
            .expect("load issue after retry")
            .expect("issue exists after retry");
        assert_eq!(updated.status, crate::model::Status::InProgress);

        // Both certificate sidecars were quarantined into `.br_recovery`
        // with their original bytes preserved.
        let recovery_dir = beads_dir.join(".br_recovery");
        let backups: Vec<_> = fs::read_dir(&recovery_dir)
            .expect("recovery dir exists after quarantine")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .collect();
        let backup_named = |prefix: &str| {
            backups.iter().find(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(prefix) && name.ends_with(".stale-wal-cert")
                })
            })
        };
        let cert_backup = backup_named("beads.db-wal-cert.")
            .expect("quarantined -wal-cert backup present in .br_recovery");
        assert_eq!(
            fs::read(cert_backup).expect("read quarantined -wal-cert backup"),
            stale_cert,
            "-wal-cert bytes must be preserved in the recovery backup"
        );
        let head_backup = backup_named("beads.db-wal-cert-head.")
            .expect("quarantined -wal-cert-head backup present in .br_recovery");
        assert_eq!(
            fs::read(head_backup).expect("read quarantined -wal-cert-head backup"),
            b"stale-cert-head",
            "-wal-cert-head bytes must be preserved in the recovery backup"
        );

        // The `-wal` file itself is real data, never derived state: it must
        // not be quarantined (the engine may still legitimately rewrite it).
        assert!(
            !backups.iter().any(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("beads.db-wal."))
            }),
            "the -wal file must never be quarantined by the stale-cert path"
        );
        if wal_existed_before {
            assert!(
                wal_path.exists(),
                "the -wal file must remain in the live database family"
            );
        }

        // The stale sidecar bytes are gone from the live family: either the
        // path no longer exists or the engine regenerated fresh contents.
        if let Ok(live_cert) = fs::read(&cert_path) {
            assert_ne!(
                live_cert, stale_cert,
                "the live -wal-cert must no longer hold the stale-generation record"
            );
        }
    }

    #[test]
    fn mutation_recovery_can_be_signaled_by_probe_after_constraint_style_error() {
        let (_temp, storage_ctx) = storage_ctx_with_exported_issue();
        let operation_err = BeadsError::Database(FrankenError::Internal(
            "constraint verification failed".to_string(),
        ));
        let probe_err = BeadsError::Database(FrankenError::Internal(
            "database disk image is malformed".to_string(),
        ));

        assert!(
            !should_attempt_mutation_jsonl_recovery(&storage_ctx, &operation_err, None),
            "constraint-style write errors should not recover without a corruption probe"
        );
        assert!(
            should_attempt_mutation_jsonl_recovery(&storage_ctx, &operation_err, Some(&probe_err)),
            "a recoverable rollback-only write probe should trigger JSONL recovery"
        );
    }
}
