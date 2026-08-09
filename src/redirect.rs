//! Safe filesystem redirects between beads workspaces.
//!
//! Redirect setup is deliberately independent from ordinary workspace
//! discovery. The source is anchored to the caller's current directory and
//! target validation runs against an isolated database-family snapshot.

use crate::config::{self, Metadata};
use crate::error::{BeadsError, Result};
use crate::storage::SqliteStorage;
use schemars::JsonSchema;
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const REDIRECT_SCHEMA: &str = "br.redirect.v1";
const MAX_REDIRECT_DEPTH: usize = 10;

/// Durable result of a redirect setup request.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct RedirectReceipt {
    /// Machine-contract identifier.
    pub schema: &'static str,
    /// Local workspace that owns the redirect file.
    pub source_workspace: PathBuf,
    /// Exact target supplied by the operator, or null for automatic discovery.
    pub requested_target: Option<PathBuf>,
    /// Whether the target was explicit or discovered from Git metadata.
    pub target_mode: RedirectTargetMode,
    /// Terminal canonical workspace after following redirect chains.
    pub final_target: PathBuf,
    /// Local redirect file that was created or inspected.
    pub redirect_path: PathBuf,
    /// `created`, `unchanged`, `primary_owner`, or `refused`.
    pub disposition: RedirectDisposition,
    /// Whether this invocation created the redirect.
    pub changed: bool,
    /// Whether the caller already owns the terminal canonical workspace.
    pub primary_worktree: bool,
    /// Whether material local state was explicitly acknowledged.
    pub existing_state_acknowledged: bool,
    /// Preserved local artifacts that become dormant behind the redirect.
    pub dormant_artifacts: Vec<PathBuf>,
}

/// Observable redirect setup outcome.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RedirectDisposition {
    /// A new redirect was published.
    Created,
    /// An existing redirect already selected the requested authority.
    Unchanged,
    /// The local workspace is itself the selected authority.
    PrimaryOwner,
    /// Setup was refused without changing local routing.
    Refused,
}

/// How a redirect target was selected.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RedirectTargetMode {
    /// The operator supplied the exact beads directory.
    Explicit,
    /// The primary worktree was discovered from Git administrative metadata.
    Automatic,
}

struct ResolvedRedirectTarget {
    source_root: PathBuf,
    mode: RedirectTargetMode,
    requested: Option<PathBuf>,
    final_target: PathBuf,
}

fn resolve_redirect_target(
    base_dir: &Path,
    requested_target: Option<&Path>,
) -> Result<ResolvedRedirectTarget> {
    if let Some(requested_target) = requested_target {
        return Ok(ResolvedRedirectTarget {
            source_root: anchor_source_root(base_dir)?,
            mode: RedirectTargetMode::Explicit,
            requested: Some(requested_target.to_path_buf()),
            final_target: validate_explicit_target(requested_target)?,
        });
    }

    let layout = discover_standard_git_worktree(base_dir)?;
    let primary_target = layout.primary_root.join(".beads");
    let final_target = validate_explicit_target(&primary_target).map_err(|error| {
        BeadsError::WithContext {
            context: format!(
                "Automatic redirect discovery found primary worktree '{}', but its tracker is unusable; provide the exact .beads path if another authority is intended",
                layout.primary_root.display()
            ),
            source: Box::new(error),
        }
    })?;
    Ok(ResolvedRedirectTarget {
        source_root: layout.worktree_root,
        mode: RedirectTargetMode::Automatic,
        requested: None,
        final_target,
    })
}

fn redirect_refusal(
    reason: String,
    source_workspace: &Path,
    requested_target: &Option<PathBuf>,
    target_mode: RedirectTargetMode,
    final_target: &Path,
    dormant_artifacts: &[PathBuf],
) -> BeadsError {
    BeadsError::RedirectRefused {
        reason,
        receipt: Box::new(RedirectReceipt {
            schema: REDIRECT_SCHEMA,
            source_workspace: source_workspace.to_path_buf(),
            requested_target: requested_target.clone(),
            target_mode,
            final_target: final_target.to_path_buf(),
            redirect_path: source_workspace.join("redirect"),
            disposition: RedirectDisposition::Refused,
            changed: false,
            primary_worktree: false,
            existing_state_acknowledged: false,
            dormant_artifacts: dormant_artifacts.to_vec(),
        }),
    }
}

fn map_deliberate_refusal(
    error: BeadsError,
    source_workspace: &Path,
    requested_target: &Option<PathBuf>,
    target_mode: RedirectTargetMode,
    final_target: &Path,
    dormant_artifacts: &[PathBuf],
) -> BeadsError {
    match error {
        BeadsError::Config(reason) => redirect_refusal(
            reason,
            source_workspace,
            requested_target,
            target_mode,
            final_target,
            dormant_artifacts,
        ),
        other => other,
    }
}

fn safely_inventory_refused_fresh_source(source_workspace: &Path) -> Vec<PathBuf> {
    let Ok(metadata) = fs::symlink_metadata(source_workspace) else {
        return Vec::new();
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return vec![source_workspace.to_path_buf()];
    }
    inventory_dormant_artifacts(source_workspace).unwrap_or_default()
}

/// Create a redirect for a fresh workspace using an exact target path.
///
/// # Errors
///
/// Returns an error when the target is not an initialized current workspace,
/// the local workspace already contains independent state, or an existing
/// redirect selects another authority.
pub fn init_with_explicit_target(
    base_dir: &Path,
    requested_target: &Path,
) -> Result<RedirectReceipt> {
    init_with_target(base_dir, Some(requested_target))
}

/// Create a redirect using the primary worktree discovered from Git metadata.
///
/// # Errors
///
/// Returns an error when the current directory is not in a supported standard
/// Git worktree layout or the primary worktree is not an initialized tracker.
pub fn init_with_automatic_target(base_dir: &Path) -> Result<RedirectReceipt> {
    init_with_target(base_dir, None)
}

fn init_with_target(base_dir: &Path, requested_target: Option<&Path>) -> Result<RedirectReceipt> {
    let ResolvedRedirectTarget {
        source_root,
        mode: target_mode,
        requested: requested_target,
        final_target,
    } = resolve_redirect_target(base_dir, requested_target)?;
    let source_workspace = source_root.join(".beads");

    if source_workspace == final_target {
        return Ok(RedirectReceipt {
            schema: REDIRECT_SCHEMA,
            source_workspace: source_workspace.clone(),
            requested_target,
            target_mode,
            final_target,
            redirect_path: source_workspace.join("redirect"),
            disposition: RedirectDisposition::PrimaryOwner,
            changed: false,
            primary_worktree: true,
            existing_state_acknowledged: false,
            dormant_artifacts: Vec::new(),
        });
    }

    let dormant_artifacts = prepare_fresh_source_workspace(&source_workspace).map_err(|error| {
        let dormant_artifacts = safely_inventory_refused_fresh_source(&source_workspace);
        map_deliberate_refusal(
            error,
            &source_workspace,
            &requested_target,
            target_mode,
            &final_target,
            &dormant_artifacts,
        )
    })?;
    let redirect_path = source_workspace.join("redirect");
    let changed =
        publish_redirect(&source_workspace, &redirect_path, &final_target).map_err(|error| {
            map_deliberate_refusal(
                error,
                &source_workspace,
                &requested_target,
                target_mode,
                &final_target,
                &dormant_artifacts,
            )
        })?;

    Ok(RedirectReceipt {
        schema: REDIRECT_SCHEMA,
        source_workspace,
        requested_target,
        target_mode,
        final_target,
        redirect_path,
        disposition: if changed {
            RedirectDisposition::Created
        } else {
            RedirectDisposition::Unchanged
        },
        changed,
        primary_worktree: false,
        existing_state_acknowledged: false,
        dormant_artifacts,
    })
}

/// Set a redirect for an existing local workspace.
///
/// # Errors
///
/// Returns an error when material local state would become dormant without
/// acknowledgement, or when target/source validation or publication fails.
pub fn set_redirect(
    base_dir: &Path,
    requested_target: Option<&Path>,
    allow_existing: bool,
) -> Result<RedirectReceipt> {
    let ResolvedRedirectTarget {
        source_root,
        mode: target_mode,
        requested: requested_target,
        final_target,
    } = resolve_redirect_target(base_dir, requested_target)?;
    let source_workspace = source_root.join(".beads");
    let redirect_path = source_workspace.join("redirect");

    if source_workspace == final_target {
        return Ok(RedirectReceipt {
            schema: REDIRECT_SCHEMA,
            source_workspace: source_workspace.clone(),
            requested_target,
            target_mode,
            final_target,
            redirect_path,
            disposition: RedirectDisposition::PrimaryOwner,
            changed: false,
            primary_worktree: true,
            existing_state_acknowledged: false,
            dormant_artifacts: Vec::new(),
        });
    }

    validate_adoption_source(&source_workspace)?;
    let dormant_artifacts = inventory_dormant_artifacts(&source_workspace)?;
    if path_entry_exists(&redirect_path)? {
        ensure_existing_redirect_matches(&source_workspace, &redirect_path, &final_target)
            .map_err(|error| {
                map_deliberate_refusal(
                    error,
                    &source_workspace,
                    &requested_target,
                    target_mode,
                    &final_target,
                    &dormant_artifacts,
                )
            })?;
        return Ok(RedirectReceipt {
            schema: REDIRECT_SCHEMA,
            source_workspace,
            requested_target,
            target_mode,
            final_target,
            redirect_path,
            disposition: RedirectDisposition::Unchanged,
            changed: false,
            primary_worktree: false,
            existing_state_acknowledged: false,
            dormant_artifacts,
        });
    }

    let material_artifacts = dormant_artifacts
        .iter()
        .filter(|path| is_material_local_artifact(path))
        .collect::<Vec<_>>();
    if !allow_existing && !material_artifacts.is_empty() {
        let reason = format!(
            "refusing to make material local tracker state dormant in '{}': {}. All preserved dormant artifacts: {}. Re-run with --allow-existing to acknowledge that every local artifact will be preserved but shadowed",
            source_workspace.display(),
            material_artifacts
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            dormant_artifacts
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Err(redirect_refusal(
            reason,
            &source_workspace,
            &requested_target,
            target_mode,
            &final_target,
            &dormant_artifacts,
        ));
    }

    let changed =
        publish_redirect(&source_workspace, &redirect_path, &final_target).map_err(|error| {
            map_deliberate_refusal(
                error,
                &source_workspace,
                &requested_target,
                target_mode,
                &final_target,
                &dormant_artifacts,
            )
        })?;
    Ok(RedirectReceipt {
        schema: REDIRECT_SCHEMA,
        source_workspace,
        requested_target,
        target_mode,
        final_target,
        redirect_path,
        disposition: if changed {
            RedirectDisposition::Created
        } else {
            RedirectDisposition::Unchanged
        },
        changed,
        primary_worktree: false,
        existing_state_acknowledged: allow_existing && !dormant_artifacts.is_empty(),
        dormant_artifacts,
    })
}

fn validate_adoption_source(source_workspace: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source_workspace).map_err(|error| {
        BeadsError::WithContext {
            context: format!(
                "Redirect adoption requires an existing local workspace at '{}'; use `br init --redirect` for a fresh worktree",
                source_workspace.display()
            ),
            source: Box::new(error),
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(BeadsError::Config(format!(
            "Redirect source must be a real directory: {}",
            source_workspace.display()
        )));
    }
    Ok(())
}

fn inventory_dormant_artifacts(source_workspace: &Path) -> Result<Vec<PathBuf>> {
    let mut artifacts = Vec::new();
    for entry in fs::read_dir(source_workspace)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let is_redirect_staging_file =
            file_name.starts_with(".redirect.") && file_name.ends_with(".tmp");
        if file_name != "redirect" && !is_redirect_staging_file {
            artifacts.push(entry.path());
        }
    }
    artifacts.sort();
    Ok(artifacts)
}

fn is_material_local_artifact(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return true;
    };
    if metadata.file_type().is_symlink() || metadata.is_dir() {
        return true;
    }

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if name == ".gitignore"
        || matches!(
            name,
            "config.yaml" | "metadata.json" | "issues.jsonl" | "beads.jsonl" | "interactions.jsonl"
        )
        || matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("md" | "yaml" | "yml")
        )
    {
        return false;
    }
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitWorktreeLayout {
    worktree_root: PathBuf,
    primary_root: PathBuf,
}

fn anchor_source_root(base_dir: &Path) -> Result<PathBuf> {
    let canonical_base = canonicalize_source_base(base_dir)?;
    Ok(find_git_marker_root(&canonical_base).unwrap_or(canonical_base))
}

fn discover_standard_git_worktree(base_dir: &Path) -> Result<GitWorktreeLayout> {
    let canonical_base = canonicalize_source_base(base_dir)?;
    let worktree_root = find_git_marker_root(&canonical_base).ok_or_else(discovery_error)?;
    let dot_git = worktree_root.join(".git");
    let metadata = fs::symlink_metadata(&dot_git)?;
    if metadata.file_type().is_symlink() {
        return Err(discovery_error());
    }

    if metadata.is_dir() {
        return Ok(GitWorktreeLayout {
            worktree_root: worktree_root.clone(),
            primary_root: worktree_root,
        });
    }
    if !metadata.is_file() {
        return Err(discovery_error());
    }

    let gitdir_value = read_single_path_value(&dot_git, Some("gitdir:"))?;
    let git_admin_dir = resolve_metadata_path(&worktree_root, &gitdir_value)?;
    let commondir_file = git_admin_dir.join("commondir");
    let common_value = read_single_path_value(&commondir_file, None)?;
    let common_git_dir = resolve_metadata_path(&git_admin_dir, &common_value)?;
    if common_git_dir.file_name() != Some(std::ffi::OsStr::new(".git")) {
        return Err(discovery_error());
    }
    let primary_root = common_git_dir
        .parent()
        .ok_or_else(discovery_error)?
        .to_path_buf();
    let linked_admin_root = common_git_dir.join("worktrees");
    if git_admin_dir.parent() != Some(linked_admin_root.as_path()) {
        return Err(discovery_error());
    }

    let backlink_file = git_admin_dir.join("gitdir");
    let backlink_value = read_single_path_value(&backlink_file, None)?;
    let backlink = resolve_metadata_path(&git_admin_dir, &backlink_value)?;
    let canonical_dot_git = dunce::canonicalize(&dot_git)?;
    if backlink != canonical_dot_git {
        return Err(discovery_error());
    }

    Ok(GitWorktreeLayout {
        worktree_root,
        primary_root,
    })
}

fn canonicalize_source_base(base_dir: &Path) -> Result<PathBuf> {
    dunce::canonicalize(base_dir).map_err(|error| BeadsError::WithContext {
        context: format!(
            "Cannot anchor redirect setup to current worktree directory '{}'",
            base_dir.display()
        ),
        source: Box::new(error),
    })
}

fn find_git_marker_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| fs::symlink_metadata(candidate.join(".git")).is_ok())
        .map(Path::to_path_buf)
}

fn read_single_path_value(path: &Path, prefix: Option<&str>) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path).map_err(|error| BeadsError::WithContext {
        context: format!(
            "Unsupported or ambiguous Git worktree metadata at '{}'; provide the exact .beads path",
            path.display()
        ),
        source: Box::new(error),
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 4096 {
        return Err(discovery_error());
    }
    let content = fs::read_to_string(path)?;
    let mut lines = content.lines();
    let line = lines.next().map(str::trim).filter(|line| !line.is_empty());
    if lines.any(|line| !line.trim().is_empty()) {
        return Err(discovery_error());
    }
    let value = match (line, prefix) {
        (Some(line), Some(prefix)) => line.strip_prefix(prefix).map(str::trim),
        (Some(line), None) => Some(line),
        (None, _) => None,
    }
    .filter(|value| !value.is_empty())
    .ok_or_else(discovery_error)?;
    Ok(PathBuf::from(value))
}

fn resolve_metadata_path(base: &Path, value: &Path) -> Result<PathBuf> {
    let candidate = if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    };
    dunce::canonicalize(&candidate).map_err(|error| BeadsError::WithContext {
        context: format!(
            "Unsupported or ambiguous Git worktree metadata target '{}'; provide the exact .beads path",
            candidate.display()
        ),
        source: Box::new(error),
    })
}

fn discovery_error() -> BeadsError {
    BeadsError::Config(
        "Automatic redirect discovery requires a standard non-bare Git primary or linked worktree; provide the exact .beads path"
            .to_string(),
    )
}

fn validate_explicit_target(requested_target: &Path) -> Result<PathBuf> {
    if requested_target
        .file_name()
        .is_none_or(|name| !config::is_beads_dir_name(name))
    {
        return Err(BeadsError::validation(
            "redirect",
            format!(
                "target must name an exact .beads or _beads directory: {}",
                requested_target.display()
            ),
        ));
    }

    let canonical_requested =
        dunce::canonicalize(requested_target).map_err(|error| BeadsError::WithContext {
            context: format!(
                "Redirect target does not exist or cannot be resolved: {}",
                requested_target.display()
            ),
            source: Box::new(error),
        })?;
    let final_target = config::routing::follow_redirects(&canonical_requested, MAX_REDIRECT_DEPTH)?;
    validate_initialized_target(&final_target)?;
    Ok(final_target)
}

fn validate_initialized_target(target: &Path) -> Result<()> {
    let metadata = Metadata::load(target)?;
    let configured_db = PathBuf::from(metadata.database);
    let db_path = if configured_db.is_absolute() {
        configured_db
    } else {
        target.join(configured_db)
    };

    config::with_database_family_snapshot(&db_path, |snapshot_db_path| {
        let Some(storage) = SqliteStorage::open_current_read_only(snapshot_db_path)? else {
            return Err(BeadsError::Config(format!(
                "Redirect target is not an initialized usable current-schema tracker: {}",
                target.display()
            )));
        };
        storage.get_config("issue_prefix")?;
        Ok(())
    })
    .map_err(|error| BeadsError::WithContext {
        context: format!(
            "Redirect target is not an initialized usable tracker: {}",
            target.display()
        ),
        source: Box::new(error),
    })
}

fn prepare_fresh_source_workspace(source_workspace: &Path) -> Result<Vec<PathBuf>> {
    match fs::symlink_metadata(source_workspace) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(BeadsError::Config(format!(
                "Redirect source must be a real directory: {}",
                source_workspace.display()
            )));
        }
        Ok(_) => {
            let dormant_artifacts = inventory_dormant_artifacts(source_workspace)?;
            let material_artifacts = dormant_artifacts
                .iter()
                .filter(|path| is_material_local_artifact(path))
                .collect::<Vec<_>>();
            if !material_artifacts.is_empty() {
                return Err(BeadsError::Config(format!(
                    "Redirect initialization requires a fresh workspace without material local tracker state; '{}' already contains: {}. All preserved dormant artifacts: {}. Use `br redirect set --allow-existing` to acknowledge preserved local state",
                    source_workspace.display(),
                    material_artifacts
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    dormant_artifacts
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            Ok(dormant_artifacts)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::create_dir(source_workspace) {
                Ok(()) => Ok(Vec::new()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    prepare_fresh_source_workspace(source_workspace)
                }
                Err(error) => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn publish_redirect(
    source_workspace: &Path,
    redirect_path: &Path,
    final_target: &Path,
) -> Result<bool> {
    if path_entry_exists(redirect_path)? {
        ensure_existing_redirect_matches(source_workspace, redirect_path, final_target)?;
        return Ok(false);
    }

    let mut staged = tempfile::Builder::new()
        .prefix(".redirect.")
        .suffix(".tmp")
        .tempfile_in(source_workspace)?;
    writeln!(staged, "{}", final_target.display())?;
    staged.as_file().sync_all()?;

    match staged.persist_noclobber(redirect_path) {
        Ok(file) => {
            file.sync_all()?;
            sync_directory(source_workspace)?;
            Ok(true)
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure_existing_redirect_matches(source_workspace, redirect_path, final_target)?;
            Ok(false)
        }
        Err(error) => Err(error.error.into()),
    }
}

fn ensure_existing_redirect_matches(
    source_workspace: &Path,
    redirect_path: &Path,
    final_target: &Path,
) -> Result<()> {
    let metadata = fs::symlink_metadata(redirect_path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(BeadsError::Config(format!(
            "Refusing to replace unsafe redirect entry: {}",
            redirect_path.display()
        )));
    }

    let existing_target = config::routing::follow_redirects(source_workspace, MAX_REDIRECT_DEPTH)?;
    if existing_target != final_target {
        return Err(BeadsError::Config(format!(
            "Conflicting redirect is preserved at '{}': existing authority '{}', requested authority '{}'",
            redirect_path.display(),
            existing_target.display(),
            final_target.display()
        )));
    }
    Ok(())
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StructuredError;

    #[test]
    fn dormant_artifact_classification_keeps_tracked_siblings_non_material() {
        let fixture = tempfile::tempdir().unwrap();
        let config = fixture.path().join("config.yaml");
        let issues = fixture.path().join("issues.jsonl");
        let database = fixture.path().join("beads.db");
        let unknown = fixture.path().join("unknown.bin");
        fs::write(&config, b"issue-prefix: br\n").unwrap();
        fs::write(&issues, b"{\"id\":\"br-1\"}\n").unwrap();
        fs::write(&database, b"sqlite").unwrap();
        fs::write(&unknown, b"unknown").unwrap();

        assert!(!is_material_local_artifact(&config));
        assert!(!is_material_local_artifact(&issues));
        assert!(is_material_local_artifact(&database));
        assert!(is_material_local_artifact(&unknown));
    }

    #[test]
    fn explicit_target_requires_an_exact_beads_directory_leaf() {
        let error = validate_explicit_target(Path::new("/tmp/not-a-workspace"))
            .expect_err("non-beads leaf must be rejected before filesystem resolution");
        assert!(error.to_string().contains("exact .beads or _beads"));
    }

    #[test]
    fn redirect_refusal_serializes_the_stable_receipt_context() {
        let receipt = RedirectReceipt {
            schema: REDIRECT_SCHEMA,
            source_workspace: PathBuf::from("/secondary/.beads"),
            requested_target: Some(PathBuf::from("/primary/.beads")),
            target_mode: RedirectTargetMode::Explicit,
            final_target: PathBuf::from("/primary/.beads"),
            redirect_path: PathBuf::from("/secondary/.beads/redirect"),
            disposition: RedirectDisposition::Refused,
            changed: false,
            primary_worktree: false,
            existing_state_acknowledged: false,
            dormant_artifacts: vec![PathBuf::from("/secondary/.beads/beads.db")],
        };
        let structured = StructuredError::from_error(&BeadsError::RedirectRefused {
            reason: "material local state".to_string(),
            receipt: Box::new(receipt),
        });
        let context = structured.context.expect("redirect refusal context");

        assert_eq!(context["schema"], REDIRECT_SCHEMA);
        assert_eq!(context["disposition"], "refused");
        assert_eq!(context["changed"], false);
        assert_eq!(context["refusal_reason"], "material local state");
    }
}
