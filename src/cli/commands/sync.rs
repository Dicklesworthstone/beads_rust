//! Sync command implementation.
//!
//! Provides explicit JSONL sync actions without git operations.
//! Supports `--flush-only` (export) and `--import-only` (import).

use crate::cli::SyncArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::sync::history::HistoryConfig;
use crate::sync::{
    ConflictResolution, ExportConfig, ExportEntityType, ExportError, ExportErrorPolicy,
    ImportConfig, METADATA_JSONL_CONTENT_HASH, METADATA_LAST_EXPORT_TIME,
    METADATA_LAST_IMPORT_TIME, MergeContext, OrphanMode, compute_jsonl_hash, count_issues_in_jsonl,
    export_to_jsonl_with_policy, finalize_export, get_issue_ids_from_jsonl, import_from_jsonl,
    load_base_snapshot, read_issues_from_jsonl, require_safe_sync_overwrite_path,
    save_base_snapshot, three_way_merge,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, IsTerminal};
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
}

#[derive(Debug)]
#[allow(dead_code)] // Fields may be used in future sync enhancements
struct SyncPathPolicy {
    jsonl_path: PathBuf,
    jsonl_temp_path: PathBuf,
    manifest_path: PathBuf,
    beads_dir: PathBuf,
    is_external: bool,
}

/// Execute the sync command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or the sync operation fails.
pub fn execute(args: &SyncArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    // Open storage
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let config::OpenStorageResult {
        mut storage, paths, ..
    } = config::open_storage_with_cli(&beads_dir, cli)?;

    let jsonl_path = paths.jsonl_path;
    let retention_days = paths.metadata.deletions_retention_days;
    let use_json = json || args.robot;
    let quiet = cli.quiet.unwrap_or(false);
    let show_progress = should_show_progress(use_json, quiet);
    let path_policy = validate_sync_paths(&beads_dir, &jsonl_path, args.allow_external_jsonl)?;
    debug!(
        jsonl_path = %path_policy.jsonl_path.display(),
        manifest_path = %path_policy.manifest_path.display(),
        external_jsonl = path_policy.is_external,
        "Resolved sync path policy"
    );

    // Handle --status flag
    if args.status {
        return execute_status(&storage, &path_policy, use_json);
    }

    // Validate mutually exclusive modes
    let mode_count = u8::from(args.flush_only) + u8::from(args.import_only) + u8::from(args.merge);
    if mode_count > 1 {
        return Err(BeadsError::Validation {
            field: "mode".to_string(),
            reason: "Must specify exactly one of --flush-only, --import-only, or --merge"
                .to_string(),
        });
    }

    if args.flush_only {
        execute_flush(
            &mut storage,
            &beads_dir,
            &path_policy,
            args,
            use_json,
            show_progress,
            retention_days,
        )
    } else if args.merge {
        execute_merge(
            &mut storage,
            &path_policy,
            args,
            use_json,
            show_progress,
            retention_days,
            cli,
        )
    } else {
        // Default to import-only if no flag is specified (consistent with existing behavior)
        // or explicitly import-only
        execute_import(&mut storage, &path_policy, args, use_json, show_progress)
    }
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
    let canonical_beads = beads_dir.canonicalize().map_err(|e| {
        BeadsError::Config(format!(
            "Failed to resolve .beads directory {}: {e}",
            beads_dir.display()
        ))
    })?;

    let jsonl_parent = jsonl_path.parent().ok_or_else(|| {
        BeadsError::Config("JSONL path must include a parent directory".to_string())
    })?;
    let canonical_parent = jsonl_parent.canonicalize().map_err(|e| {
        BeadsError::Config(format!(
            "JSONL directory does not exist or is not accessible: {} ({e})",
            jsonl_parent.display()
        ))
    })?;

    let jsonl_path = if jsonl_path.exists() {
        jsonl_path.canonicalize().map_err(|e| {
            BeadsError::Config(format!(
                "Failed to resolve JSONL path {}: {e}",
                jsonl_path.display()
            ))
        })?
    } else {
        let file_name = jsonl_path
            .file_name()
            .ok_or_else(|| BeadsError::Config("JSONL path must include a filename".to_string()))?;
        canonical_parent.join(file_name)
    };

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
    let jsonl_temp_path = jsonl_path.with_extension("jsonl.tmp");

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
    })
}

fn contains_git_dir(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name == ".git",
        _ => false,
    })
}

/// Execute the --status subcommand.
fn execute_status(
    storage: &crate::storage::SqliteStorage,
    path_policy: &SyncPathPolicy,
    json: bool,
) -> Result<()> {
    let dirty_ids = storage.get_dirty_issue_ids()?;
    let dirty_count = dirty_ids.len();

    let last_export_time = storage.get_metadata(METADATA_LAST_EXPORT_TIME)?;
    let last_import_time = storage.get_metadata(METADATA_LAST_IMPORT_TIME)?;
    let jsonl_content_hash = storage.get_metadata(METADATA_JSONL_CONTENT_HASH)?;

    let jsonl_path = &path_policy.jsonl_path;
    let jsonl_exists = jsonl_path.exists();
    debug!(
        jsonl_path = %jsonl_path.display(),
        jsonl_exists,
        dirty_count,
        "Computed sync status inputs"
    );

    // Determine staleness using Lstat (symlink_metadata) to handle symlinks correctly
    let (jsonl_newer, db_newer) = if jsonl_exists {
        // Use symlink_metadata (Lstat) instead of metadata (stat) to get the mtime
        // of the symlink itself, not the target. This is important for detecting
        // when the JSONL file has been updated via a symlink.
        let jsonl_mtime = fs::symlink_metadata(jsonl_path)?.modified()?;

        // JSONL is newer if it was modified after last import
        let mtime_newer = last_import_time.as_ref().is_none_or(|import_time| {
            chrono::DateTime::parse_from_rfc3339(import_time).is_ok_and(|import_ts| {
                let import_sys_time = std::time::SystemTime::from(import_ts);
                jsonl_mtime > import_sys_time
            })
        });

        // Hash check prevents false staleness from `touch` - if mtime is newer but
        // content hash is the same, the file wasn't actually modified
        let jsonl_newer = if mtime_newer {
            // Check if content hash has changed to prevent false positives from touch
            jsonl_content_hash.as_ref().map_or_else(
                || {
                    // No stored hash (cold start), trust mtime
                    debug!("No stored hash (cold start), trusting mtime for staleness");
                    true
                },
                |stored_hash| match compute_jsonl_hash(jsonl_path) {
                    Ok(current_hash) => {
                        let hash_changed = &current_hash != stored_hash;
                        debug!(
                            mtime_newer,
                            hash_changed,
                            stored_hash,
                            current_hash,
                            "Staleness check: mtime newer but verifying hash"
                        );
                        hash_changed
                    }
                    Err(e) => {
                        // If we can't compute hash, fall back to mtime-based staleness
                        debug!(?e, "Failed to compute JSONL hash, falling back to mtime");
                        true
                    }
                },
            )
        } else {
            false
        };

        // DB is newer if there are dirty issues
        let db_newer = dirty_count > 0;

        (jsonl_newer, db_newer)
    } else {
        (false, dirty_count > 0)
    };

    let status = SyncStatus {
        dirty_count,
        last_export_time,
        last_import_time,
        jsonl_content_hash,
        jsonl_exists,
        jsonl_newer,
        db_newer,
    };
    debug!(jsonl_newer, db_newer, "Computed sync staleness");

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
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

/// Execute the --flush-only (export) operation.
#[allow(clippy::too_many_lines)]
fn execute_flush(
    storage: &mut crate::storage::SqliteStorage,
    _beads_dir: &Path,
    path_policy: &SyncPathPolicy,
    args: &SyncArgs,
    json: bool,
    show_progress: bool,
    retention_days: Option<u64>,
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
    debug!(dirty_count = dirty_ids.len(), "Found dirty issues");

    // If no dirty issues and no force, report nothing to do
    if dirty_ids.is_empty() && !args.force {
        // Guard against empty DB overwriting a non-empty JSONL.
        let existing_count = count_issues_in_jsonl(jsonl_path)?;
        if existing_count > 0 {
            let issues = storage.get_all_issues_for_export()?;
            if issues.is_empty() {
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

            let jsonl_ids = get_issue_ids_from_jsonl(jsonl_path)?;
            if !jsonl_ids.is_empty() {
                let db_ids: HashSet<String> = issues.iter().map(|i| i.id.clone()).collect();
                let missing: Vec<_> = jsonl_ids.difference(&db_ids).collect();

                if !missing.is_empty() {
                    warn!(
                        jsonl_count = jsonl_ids.len(),
                        db_count = issues.len(),
                        missing_count = missing.len(),
                        "Refusing export because DB is stale relative to JSONL"
                    );
                    let mut missing_list = missing.into_iter().cloned().collect::<Vec<_>>();
                    missing_list.sort();
                    let display_count = missing_list.len().min(10);
                    let preview: Vec<_> = missing_list.iter().take(display_count).collect();
                    let more = if missing_list.len() > 10 {
                        format!(" ... and {} more", missing_list.len() - 10)
                    } else {
                        String::new()
                    };

                    return Err(BeadsError::Config(format!(
                        "Refusing to export stale database that would lose issues.\n\
                         Database has {} issues, JSONL has {} issues.\n\
                         Export would lose {} issue(s): {}{}\n\
                         Hint: Run import first, or use --force to override.",
                        issues.len(),
                        jsonl_ids.len(),
                        missing_list.len(),
                        preview
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        more
                    )));
                }
            }
        }

        if json {
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
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!("Nothing to export (no dirty issues)");
        }
        return Ok(());
    }

    // Configure export
    let export_config = ExportConfig {
        force: args.force,
        is_default_path: true,
        error_policy: export_policy,
        retention_days,
        beads_dir: Some(path_policy.beads_dir.clone()),
        allow_external_jsonl: args.allow_external_jsonl,
        show_progress,
        history: HistoryConfig::default(),
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
    finalize_export(storage, &export_result, Some(&export_result.issue_hashes))?;
    info!("Export complete, cleared dirty flags");

    // Write manifest if requested
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
            args.allow_external_jsonl,
            "write manifest",
        )?;
        fs::write(&manifest_file, serde_json::to_string_pretty(&manifest)?)?;
        Some(manifest_file.to_string_lossy().to_string())
    } else {
        None
    };

    // Output result
    let cleared_dirty =
        export_result.exported_ids.len() + export_result.skipped_tombstone_ids.len();
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

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
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
    !json && !quiet && std::io::stderr().is_terminal()
}

/// Execute the --import-only operation.
#[allow(clippy::too_many_lines)]
fn execute_import(
    storage: &mut crate::storage::SqliteStorage,
    path_policy: &SyncPathPolicy,
    args: &SyncArgs,
    json: bool,
    show_progress: bool,
) -> Result<()> {
    info!("Starting JSONL import");
    let jsonl_path = &path_policy.jsonl_path;
    debug!(
        jsonl_path = %jsonl_path.display(),
        external_jsonl = path_policy.is_external,
        force = args.force,
        "Import configuration resolved"
    );

    // Check if JSONL exists
    if !jsonl_path.exists() {
        warn!(path = %jsonl_path.display(), "JSONL path missing, skipping import");
        if json {
            let result = ImportResultOutput {
                created: 0,
                updated: 0,
                skipped: 0,
                tombstone_skipped: 0,
                blocked_cache_rebuilt: false,
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!("No JSONL file found at {}", jsonl_path.display());
        }
        return Ok(());
    }

    // Check staleness (unless --force)
    if !args.force {
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

                if json {
                    let result = ImportResultOutput {
                        created: 0,
                        updated: 0,
                        skipped: 0,
                        tombstone_skipped: 0,
                        blocked_cache_rebuilt: false,
                    };
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
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
        skip_prefix_validation: false,
        rename_on_import: false,
        clear_duplicate_external_refs: false,
        orphan_mode,
        force_upsert: args.force,
        beads_dir: Some(path_policy.beads_dir.clone()),
        allow_external_jsonl: args.allow_external_jsonl,
        show_progress,
    };

    // Get expected prefix from config, or auto-detect from JSONL
    let configured_prefix = storage.get_config("issue_prefix")?;
    let prefix = if let Some(p) = configured_prefix {
        p
    } else {
        // No prefix configured - try to auto-detect from JSONL for migration scenarios
        if let Some(detected) = detect_prefix_from_jsonl(jsonl_path) {
            info!(detected_prefix = %detected, "Auto-detected prefix from JSONL (no prefix configured)");
            // Persist the detected prefix to config for future operations
            storage.set_config("issue_prefix", &detected)?;
            detected
        } else {
            "bd".to_string()
        }
    };

    // Execute import
    info!(path = %jsonl_path.display(), "Importing from JSONL");
    let import_result = import_from_jsonl(storage, jsonl_path, &import_config, Some(&prefix))?;

    info!(
        created_or_updated = import_result.imported_count,
        skipped = import_result.skipped_count,
        tombstone_skipped = import_result.tombstone_skipped,
        "Import complete"
    );

    // Update content hash
    let content_hash = compute_jsonl_hash(jsonl_path)?;
    storage.set_metadata(METADATA_JSONL_CONTENT_HASH, &content_hash)?;

    // Output result
    let result = ImportResultOutput {
        created: import_result.imported_count, // We don't distinguish created vs updated yet
        updated: 0,
        skipped: import_result.skipped_count,
        tombstone_skipped: import_result.tombstone_skipped,
        blocked_cache_rebuilt: true,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Imported from JSONL:");
        println!("  Processed: {} issues", result.created);
        if result.skipped > 0 {
            println!("  Skipped: {} issues (up-to-date)", result.skipped);
        }
        if result.tombstone_skipped > 0 {
            println!("  Tombstone protected: {} issues", result.tombstone_skipped);
        }
        println!("  Rebuilt blocked cache");
    }

    Ok(())
}

/// Detect the issue ID prefix from the first non-tombstone issue in a JSONL file.
///
/// Returns `None` if the file is empty or contains no issues with a recognizable prefix.
/// A prefix is the part before the first hyphen in the issue ID (e.g., "mcp" from "mcp-015c").
fn detect_prefix_from_jsonl(jsonl_path: &Path) -> Option<String> {
    let file = File::open(jsonl_path).ok()?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line.ok()?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse as JSON to get the issue ID
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
                // Skip tombstones (deleted issues)
                if let Some(status) = value.get("status").and_then(|v| v.as_str()) {
                    if status == "tombstone" {
                        continue;
                    }
                }

                // Extract prefix (part before first hyphen)
                if let Some(hyphen_pos) = id.find('-') {
                    let prefix = &id[..hyphen_pos];
                    if !prefix.is_empty() {
                        return Some(prefix.to_string());
                    }
                }
            }
        }
    }

    None
}

/// Execute the --merge operation.
#[allow(clippy::too_many_lines)]
fn execute_merge(
    storage: &mut crate::storage::SqliteStorage,
    path_policy: &SyncPathPolicy,
    args: &SyncArgs,
    json: bool,
    show_progress: bool,
    retention_days: Option<u64>,
    cli: &config::CliOverrides,
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
        for issue in read_issues_from_jsonl(jsonl_path)? {
            right.insert(issue.id.clone(), issue);
        }
    }
    debug!(right_count = right.len(), "Loaded external state (JSONL)");

    // 4. Perform Merge
    let context = MergeContext::new(base, left, right);
    // TODO: support configurable conflict strategy via CLI args if needed
    let strategy = ConflictResolution::PreferNewer;
    let tombstones = None;

    let report = three_way_merge(&context, strategy, tombstones);

    // 5. Apply Changes to DB
    info!(
        kept = report.kept.len(),
        deleted = report.deleted.len(),
        conflicts = report.conflicts.len(),
        "Merge calculated"
    );

    if report.has_conflicts() {
        // For now, fail on conflicts. Future: interactive resolution or force flags.
        let mut msg = String::from("Merge conflicts detected:\n");
        for (id, kind) in &report.conflicts {
            use std::fmt::Write;
            let _ = writeln!(msg, "  - {id}: {kind:?}");
        }
        return Err(BeadsError::Config(msg));
    }

    let _actor = cli.actor.as_deref().unwrap_or("br");

    // Apply deletions
    for id in &report.deleted {
        storage.delete_issue(id, "system", "merge deletion", Some(chrono::Utc::now()))?;
    }

    // Apply updates/creates (upsert)
    // We need to retrieve the actual Issue objects to upsert.
    for issue in &report.kept {
        storage.upsert_issue_for_import(issue)?;
        storage.sync_labels_for_import(&issue.id, &issue.labels)?;
        storage.sync_dependencies_for_import(&issue.id, &issue.dependencies)?;
        storage.sync_comments_for_import(&issue.id, &issue.comments)?;
    }

    // Rebuild cache
    storage.rebuild_blocked_cache(true)?;

    // Save Base Snapshot
    let new_base: HashMap<_, _> = report
        .kept
        .iter()
        .map(|i| (i.id.clone(), i.clone()))
        .collect();
    save_base_snapshot(&new_base, beads_dir)?;

    // Force Export to update JSONL (ensure sync)
    info!(path = %jsonl_path.display(), "Writing merged issues.jsonl");
    let export_config = ExportConfig {
        force: true, // Force export to ensure JSONL matches DB
        is_default_path: true,
        error_policy: ExportErrorPolicy::Strict,
        retention_days,
        beads_dir: Some(path_policy.beads_dir.clone()),
        allow_external_jsonl: args.allow_external_jsonl,
        show_progress,
        history: HistoryConfig::default(),
    };

    let (export_result, _) = export_to_jsonl_with_policy(storage, jsonl_path, &export_config)?;
    finalize_export(storage, &export_result, Some(&export_result.issue_hashes))?;

    // Output success message
    if json {
        let output = serde_json::json!({
            "status": "success",
            "merged_issues": report.kept.len(),
            "deleted_issues": report.deleted.len(),
            "conflicts": report.conflicts.len(),
            "notes": report.notes,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
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

#[cfg(test)]
mod tests {
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use chrono::Utc;
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
}
