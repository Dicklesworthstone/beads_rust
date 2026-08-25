//! Explicit, bounded VCS diagnostics.
//!
//! This module is intentionally isolated from every sync path. `br sync` is
//! VCS-agnostic by contract; only a direct `br vcs-status` invocation reaches
//! the process capability below. The selected Git executable is trusted.
//! Search/attribute probes neutralize hooks, filters, prompts, fsmonitor,
//! untracked-cache writes, and inherited Git redirections; fixed-key config
//! probes intentionally observe effective system/global/common/worktree
//! settings. This is not a process sandbox and makes no claim to terminate
//! arbitrary daemonized descendants.

use crate::cli::VcsStatusArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::output::OutputContext;
use crate::sync::{JsonlSourceSnapshot, capture_optional_jsonl_source_until};
use schemars::JsonSchema;
use serde::Serialize;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tracing::debug;

const STATUS_SCHEMA: &str = "br.vcs-export-status.v2";
const MAX_CAPTURE_BYTES_PER_STREAM: usize = 32 * 1024;
const MIN_TIMEOUT_MS: u64 = 25;
const MAX_TIMEOUT_MS: u64 = 30_000;
const EXPLICIT_COMMAND: &str = "br vcs-status --json";

/// Explicit Git visibility for the configured JSONL export.
///
/// Repository and index evidence remains available when a path-local
/// worktree comparison is unsafe or unsupported. Raw worktree identities are
/// computed in-process from one immutable, no-follow JSONL snapshot; Git
/// filters and text conversions are never invoked.
#[derive(Debug, Serialize, JsonSchema)]
pub struct VcsExportStatus {
    pub schema: &'static str,
    pub requested: bool,
    pub available: bool,
    pub vcs: &'static str,
    /// False because HEAD, index, config, and worktree evidence are observed
    /// sequentially rather than under one transactional Git snapshot.
    pub observation_atomic: bool,
    pub path_scope: &'static str,
    pub path: String,
    pub timeout_ms: u64,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_format: Option<GitObjectFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<GitPathIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<GitPathIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmerged_index_stages: Option<Vec<GitIndexStage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_clean: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_state: Option<WorktreeState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_clean: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_comparison_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_raw_git_blob_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_raw_sha256: Option<String>,
}

/// Object format used by the selected Git repository.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GitObjectFormat {
    Sha1,
    Sha256,
}

impl GitObjectFormat {
    const fn object_id_hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }
}

/// Exact identity of one path in a Git tree or stage-zero index entry.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, JsonSchema)]
pub struct GitPathIdentity {
    pub mode: String,
    pub object_type: &'static str,
    pub object_id: String,
}

/// Exact identity of one unmerged index stage.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, JsonSchema)]
pub struct GitIndexStage {
    pub stage: u8,
    pub mode: String,
    pub object_type: &'static str,
    pub object_id: String,
}

/// Relationship between the securely captured JSONL leaf and the Git index.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeState {
    Clean,
    Modified,
    Deleted,
    Untracked,
    Ignored,
    Unmerged,
    ComparisonUnavailable,
    Absent,
}

impl VcsExportStatus {
    fn unavailable(
        target: &ResolvedGitTarget,
        timeout_ms: u64,
        started: Instant,
        reason: &'static str,
    ) -> Self {
        Self {
            schema: STATUS_SCHEMA,
            requested: true,
            available: false,
            vcs: "git",
            observation_atomic: false,
            path_scope: target.scope.as_str(),
            path: target.path_label.clone(),
            timeout_ms,
            duration_ms: elapsed_millis(started),
            reason: Some(reason),
            object_format: None,
            tracked: None,
            head: None,
            index: None,
            unmerged_index_stages: None,
            index_clean: None,
            worktree_state: None,
            worktree_clean: None,
            worktree_comparison_reason: None,
            worktree_raw_git_blob_hash: None,
            worktree_raw_sha256: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum PathScope {
    Workspace,
    External,
}

impl PathScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::External => "external",
        }
    }
}

#[derive(Debug)]
struct ResolvedGitTarget {
    path: PathBuf,
    parent: PathBuf,
    file_name: OsString,
    source: Option<JsonlSourceSnapshot>,
    scope: PathScope,
    path_label: String,
    source_capture_timed_out: bool,
}

/// Execute the explicit VCS diagnostic.
///
/// # Errors
///
/// Returns an error when workspace/path resolution fails or the caller requests
/// an unsafe/ambiguous target. Missing Git, non-repository targets, timeouts,
/// and bounded probe failures are successful diagnostic results with
/// `available: false` and a machine-readable `reason`.
pub fn execute(
    args: &VcsStatusArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    validate_timeout(args.timeout_ms)?;
    let started = Instant::now();
    let deadline = started + Duration::from_millis(args.timeout_ms);
    let target = resolve_target(args, cli, deadline)?;
    let status = collect_git_export_status(&target, args.timeout_ms, started, deadline);

    if ctx.is_quiet() && !ctx.is_json() && !ctx.is_toon() && !args.robot {
        return Ok(());
    }
    if ctx.is_json() || args.robot {
        ctx.json_pretty(&status);
    } else if ctx.is_toon() {
        ctx.toon(&status);
    } else {
        render_human(&status);
    }
    Ok(())
}

fn validate_timeout(timeout_ms: u64) -> Result<()> {
    if (MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Ok(());
    }
    Err(BeadsError::Validation {
        field: "timeout_ms".to_string(),
        reason: format!("must be between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS} milliseconds"),
    })
}

fn resolve_target(
    args: &VcsStatusArgs,
    cli: &config::CliOverrides,
    deadline: Instant,
) -> Result<ResolvedGitTarget> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let configured = config::resolve_paths(&beads_dir, cli.db.as_ref())?;
    let requested = args
        .jsonl
        .clone()
        .unwrap_or_else(|| configured.jsonl_path.clone());
    let anchored = if requested.is_absolute() {
        requested
    } else {
        std::env::current_dir()
            .map_err(|error| {
                BeadsError::Config(format!(
                    "cannot resolve the requested JSONL directory: {error}"
                ))
            })?
            .join(requested)
    };

    if contains_git_component(&anchored) {
        return Err(unsafe_target_error(
            "VCS diagnostics refuse targets inside Git metadata",
        ));
    }
    if anchored.extension() != Some(OsStr::new("jsonl")) {
        return Err(unsafe_target_error(
            "the diagnostic target must have a .jsonl extension",
        ));
    }

    let file_name = anchored
        .file_name()
        .ok_or_else(|| unsafe_target_error("the diagnostic target must include a filename"))?
        .to_os_string();
    let lexical_parent = anchored
        .parent()
        .ok_or_else(|| unsafe_target_error("the diagnostic target must include a parent"))?;
    let parent = if lexical_parent.is_dir() {
        dunce::canonicalize(lexical_parent)
            .map_err(|_| unsafe_target_error("the diagnostic target directory is not accessible"))?
    } else {
        lexical_parent.to_path_buf()
    };
    let path = parent.join(&file_name);
    if contains_git_component(&path) {
        return Err(unsafe_target_error(
            "VCS diagnostics refuse targets inside Git metadata",
        ));
    }
    let canonical_beads = dunce::canonicalize(&beads_dir).unwrap_or_else(|_| beads_dir.clone());
    let scope = if path.starts_with(&canonical_beads) {
        PathScope::Workspace
    } else {
        PathScope::External
    };
    if scope == PathScope::External && !args.allow_external_jsonl {
        return Err(BeadsError::Validation {
            field: "jsonl".to_string(),
            reason: "external JSONL diagnostics require --allow-external-jsonl".to_string(),
        });
    }

    let path_label = match scope {
        PathScope::Workspace => workspace_path_label(&path, &canonical_beads),
        PathScope::External => external_path_descriptor(&path),
    };
    let (source, source_capture_timed_out) = if parent.is_dir() {
        match capture_optional_jsonl_source_until(&path, deadline) {
            Ok(source) => (source, false),
            Err(BeadsError::Io(error)) if error.kind() == io::ErrorKind::TimedOut => (None, true),
            Err(error) => return Err(error),
        }
    } else {
        (None, false)
    };
    Ok(ResolvedGitTarget {
        path,
        parent,
        file_name,
        source,
        scope,
        path_label,
        source_capture_timed_out,
    })
}

fn unsafe_target_error(reason: &str) -> BeadsError {
    BeadsError::Validation {
        field: "jsonl".to_string(),
        reason: reason.to_string(),
    }
}

fn contains_git_component(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name) if name.to_string_lossy().eq_ignore_ascii_case(".git")
        )
    })
}

fn workspace_path_label(path: &Path, beads_dir: &Path) -> String {
    let workspace_root = beads_dir.parent().unwrap_or(beads_dir);
    let relative = path.strip_prefix(workspace_root).unwrap_or(path);
    relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .map(|components| components.join("/"))
        .unwrap_or_else(|| {
            format!(
                "<workspace-jsonl sha256={}>",
                raw_os_str_sha256(relative.as_os_str())
            )
        })
}

fn external_path_descriptor(path: &Path) -> String {
    format!(
        "<external-jsonl sha256={}>",
        raw_os_str_sha256(path.as_os_str())
    )
}

fn raw_os_str_sha256(value: &OsStr) -> String {
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(value.as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in value.encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(value.to_string_lossy().as_bytes());
    crate::util::hex_encode(&hasher.finalize())
}

fn collect_git_export_status(
    target: &ResolvedGitTarget,
    timeout_ms: u64,
    started: Instant,
    deadline: Instant,
) -> VcsExportStatus {
    if target.source_capture_timed_out {
        return VcsExportStatus::unavailable(target, timeout_ms, started, "probe_timed_out");
    }
    if !target.parent.is_dir() {
        return VcsExportStatus::unavailable(target, timeout_ms, started, "path_unavailable");
    }
    match collect_git_export_status_inner(target, timeout_ms, started, deadline) {
        Ok(status) => status,
        Err(failure) => VcsExportStatus::unavailable(target, timeout_ms, started, failure.reason()),
    }
}

fn os_args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CollectionFailure {
    Probe(ProbeFailure),
    Semantic(&'static str),
}

impl CollectionFailure {
    const fn reason(self) -> &'static str {
        match self {
            Self::Probe(failure) => failure.reason(),
            Self::Semantic(reason) => reason,
        }
    }
}

impl From<ProbeFailure> for CollectionFailure {
    fn from(failure: ProbeFailure) -> Self {
        Self::Probe(failure)
    }
}

#[derive(Debug)]
struct ParsedIndex {
    stage_zero: Option<GitPathIdentity>,
    unmerged: Vec<GitIndexStage>,
}

#[derive(Debug)]
struct WorktreeEvidence {
    state: WorktreeState,
    clean: Option<bool>,
    reason: Option<&'static str>,
}

fn collect_git_export_status_inner(
    target: &ResolvedGitTarget,
    timeout_ms: u64,
    started: Instant,
    deadline: Instant,
) -> std::result::Result<VcsExportStatus, CollectionFailure> {
    verify_repository(target, deadline)?;
    let object_format = read_object_format(target, deadline)?;
    let head = read_head_identity(target, object_format, deadline)?;
    let parsed_index = read_index(target, object_format, deadline)?;
    let tracked = parsed_index.stage_zero.is_some() || !parsed_index.unmerged.is_empty();
    let index_clean = parsed_index.unmerged.is_empty() && head == parsed_index.stage_zero;
    let worktree_raw_git_blob_hash = target
        .source
        .as_ref()
        .map(|source| hash_snapshot_as_git_blob(source, object_format, deadline))
        .transpose()?;
    let worktree_raw_sha256 = target
        .source
        .as_ref()
        .map(|source| source.raw_sha256().to_string());
    let worktree = determine_worktree_evidence(
        target,
        parsed_index.stage_zero.as_ref(),
        &parsed_index.unmerged,
        worktree_raw_git_blob_hash.as_deref(),
        deadline,
    )?;

    Ok(VcsExportStatus {
        schema: STATUS_SCHEMA,
        requested: true,
        available: true,
        vcs: "git",
        observation_atomic: false,
        path_scope: target.scope.as_str(),
        path: target.path_label.clone(),
        timeout_ms,
        duration_ms: elapsed_millis(started),
        reason: None,
        object_format: Some(object_format),
        tracked: Some(tracked),
        head,
        index: parsed_index.stage_zero,
        unmerged_index_stages: (!parsed_index.unmerged.is_empty()).then_some(parsed_index.unmerged),
        index_clean: Some(index_clean),
        worktree_state: Some(worktree.state),
        worktree_clean: worktree.clean,
        worktree_comparison_reason: worktree.reason,
        worktree_raw_git_blob_hash,
        worktree_raw_sha256,
    })
}

fn verify_repository(
    target: &ResolvedGitTarget,
    deadline: Instant,
) -> std::result::Result<(), CollectionFailure> {
    let output = run_named_probe(
        "repository",
        OsStr::new("git"),
        &target.parent,
        &os_args(&["rev-parse", "--is-inside-work-tree"]),
        deadline,
    )?;
    if output.status.success() {
        return match trim_ascii(&output.stdout) {
            b"true" => Ok(()),
            b"false" => Err(CollectionFailure::Semantic("not_git_repository")),
            _ => Err(CollectionFailure::Semantic("probe_failed")),
        };
    }

    match git_marker_presence(&target.parent) {
        GitMarkerPresence::Absent => Err(CollectionFailure::Semantic("not_git_repository")),
        GitMarkerPresence::Present | GitMarkerPresence::Indeterminate => {
            Err(CollectionFailure::Semantic("probe_failed"))
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum GitMarkerPresence {
    Present,
    Absent,
    Indeterminate,
}

fn git_marker_presence(start: &Path) -> GitMarkerPresence {
    let mut directory = Some(start);
    while let Some(current) = directory {
        match std::fs::symlink_metadata(current.join(".git")) {
            Ok(_) => return GitMarkerPresence::Present,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return GitMarkerPresence::Indeterminate,
        }
        directory = current.parent();
    }
    GitMarkerPresence::Absent
}

fn read_object_format(
    target: &ResolvedGitTarget,
    deadline: Instant,
) -> std::result::Result<GitObjectFormat, CollectionFailure> {
    let output = run_named_probe(
        "object_format",
        OsStr::new("git"),
        &target.parent,
        &os_args(&["rev-parse", "--show-object-format"]),
        deadline,
    )?;
    if !output.status.success() {
        return Err(CollectionFailure::Semantic("probe_failed"));
    }
    match trim_ascii(&output.stdout) {
        b"sha1" => Ok(GitObjectFormat::Sha1),
        b"sha256" => Ok(GitObjectFormat::Sha256),
        _ => Err(CollectionFailure::Semantic("probe_failed")),
    }
}

fn read_head_identity(
    target: &ResolvedGitTarget,
    object_format: GitObjectFormat,
    deadline: Instant,
) -> std::result::Result<Option<GitPathIdentity>, CollectionFailure> {
    let head = run_named_probe(
        "head",
        OsStr::new("git"),
        &target.parent,
        &os_args(&["rev-parse", "--verify", "--quiet", "HEAD"]),
        deadline,
    )?;
    match head.status.code() {
        Some(0) => {}
        Some(1) => {
            let symbolic = run_named_probe(
                "symbolic_head",
                OsStr::new("git"),
                &target.parent,
                &os_args(&["symbolic-ref", "-q", "HEAD"]),
                deadline,
            )?;
            if symbolic.status.success()
                && trim_ascii(&symbolic.stdout).starts_with(b"refs/heads/")
                && trim_ascii(&symbolic.stdout).len() > b"refs/heads/".len()
            {
                return Ok(None);
            }
            return Err(CollectionFailure::Semantic("probe_failed"));
        }
        _ => return Err(CollectionFailure::Semantic("probe_failed")),
    }

    let mut args = os_args(&["ls-tree", "-z", "HEAD", "--"]);
    args.push(target.file_name.clone());
    let output = run_named_probe(
        "head_path",
        OsStr::new("git"),
        &target.parent,
        &args,
        deadline,
    )?;
    if !output.status.success() {
        return Err(CollectionFailure::Semantic("probe_failed"));
    }
    parse_head_identity(&output.stdout, object_format)
}

fn parse_head_identity(
    output: &[u8],
    object_format: GitObjectFormat,
) -> std::result::Result<Option<GitPathIdentity>, CollectionFailure> {
    const FAILURE: CollectionFailure = CollectionFailure::Semantic("probe_failed");
    if output.is_empty() {
        return Ok(None);
    }
    let records = split_nul_records(output).ok_or(FAILURE)?;
    if records.len() != 1 {
        return Err(FAILURE);
    }
    let (metadata, path) = records[0].split_once_byte(b'\t').ok_or(FAILURE)?;
    if path.is_empty() {
        return Err(FAILURE);
    }
    let fields = metadata.split(|byte| *byte == b' ').collect::<Vec<_>>();
    if fields.len() != 3 {
        return Err(FAILURE);
    }
    Ok(Some(GitPathIdentity {
        mode: parse_git_mode(fields[0]).ok_or(FAILURE)?,
        object_type: parse_object_type(fields[1]).ok_or(FAILURE)?,
        object_id: parse_object_id(fields[2], object_format).ok_or(FAILURE)?,
    }))
}

fn read_index(
    target: &ResolvedGitTarget,
    object_format: GitObjectFormat,
    deadline: Instant,
) -> std::result::Result<ParsedIndex, CollectionFailure> {
    let mut args = os_args(&["ls-files", "--stage", "-z", "--"]);
    args.push(target.file_name.clone());
    let output = run_named_probe(
        "index_path",
        OsStr::new("git"),
        &target.parent,
        &args,
        deadline,
    )?;
    if !output.status.success() {
        return Err(CollectionFailure::Semantic("probe_failed"));
    }
    parse_index_entries(&output.stdout, object_format)
        .ok_or(CollectionFailure::Semantic("probe_failed"))
}

fn parse_index_entries(output: &[u8], object_format: GitObjectFormat) -> Option<ParsedIndex> {
    if output.is_empty() {
        return Some(ParsedIndex {
            stage_zero: None,
            unmerged: Vec::new(),
        });
    }
    let records = split_nul_records(output)?;
    let mut stage_zero = None;
    let mut unmerged = Vec::new();
    for record in records {
        let (metadata, path) = record.split_once_byte(b'\t')?;
        if path.is_empty() {
            return None;
        }
        let fields = metadata.split(|byte| *byte == b' ').collect::<Vec<_>>();
        if fields.len() != 3 {
            return None;
        }
        let mode = parse_git_mode(fields[0])?;
        let object_id = parse_object_id(fields[1], object_format)?;
        let stage = std::str::from_utf8(fields[2]).ok()?.parse::<u8>().ok()?;
        let object_type = object_type_for_mode(&mode)?;
        if stage == 0 {
            if stage_zero.is_some() || !unmerged.is_empty() {
                return None;
            }
            stage_zero = Some(GitPathIdentity {
                mode,
                object_type,
                object_id,
            });
        } else if (1..=3).contains(&stage) {
            if stage_zero.is_some() {
                return None;
            }
            unmerged.push(GitIndexStage {
                stage,
                mode,
                object_type,
                object_id,
            });
        } else {
            return None;
        }
    }
    if !unmerged.is_empty() {
        unmerged.sort_by_key(|entry| entry.stage);
        if unmerged
            .windows(2)
            .any(|pair| pair[0].stage == pair[1].stage)
        {
            return None;
        }
    }
    Some(ParsedIndex {
        stage_zero,
        unmerged,
    })
}

trait ByteSliceExt {
    fn split_once_byte(&self, delimiter: u8) -> Option<(&[u8], &[u8])>;
}

impl ByteSliceExt for [u8] {
    fn split_once_byte(&self, delimiter: u8) -> Option<(&[u8], &[u8])> {
        let index = self.iter().position(|byte| *byte == delimiter)?;
        Some((&self[..index], &self[index + 1..]))
    }
}

fn split_nul_records(output: &[u8]) -> Option<Vec<&[u8]>> {
    if !output.ends_with(&[0]) {
        return None;
    }
    Some(
        output[..output.len() - 1]
            .split(|byte| *byte == 0)
            .collect(),
    )
}

fn parse_git_mode(bytes: &[u8]) -> Option<String> {
    if bytes.len() != 6 || !bytes.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
        return None;
    }
    std::str::from_utf8(bytes).ok().map(str::to_string)
}

fn parse_object_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        b"blob" => Some("blob"),
        b"tree" => Some("tree"),
        b"commit" => Some("commit"),
        _ => None,
    }
}

fn object_type_for_mode(mode: &str) -> Option<&'static str> {
    match mode {
        "100644" | "100755" | "120000" => Some("blob"),
        "160000" => Some("commit"),
        _ => None,
    }
}

fn parse_object_id(bytes: &[u8], object_format: GitObjectFormat) -> Option<String> {
    if bytes.len() != object_format.object_id_hex_len() || !bytes.iter().all(u8::is_ascii_hexdigit)
    {
        return None;
    }
    std::str::from_utf8(bytes).ok().map(str::to_ascii_lowercase)
}

fn hash_snapshot_as_git_blob(
    source: &JsonlSourceSnapshot,
    object_format: GitObjectFormat,
    deadline: Instant,
) -> std::result::Result<String, ProbeFailure> {
    match object_format {
        GitObjectFormat::Sha1 => hash_snapshot_with::<Sha1>(source, deadline),
        GitObjectFormat::Sha256 => hash_snapshot_with::<Sha256>(source, deadline),
    }
}

fn hash_snapshot_with<D: Digest>(
    source: &JsonlSourceSnapshot,
    deadline: Instant,
) -> std::result::Result<String, ProbeFailure> {
    if Instant::now() >= deadline {
        return Err(ProbeFailure::TimedOut);
    }
    let mut hasher = D::new();
    hasher.update(format!("blob {}\0", source.size()).as_bytes());
    let mut reader = source.reader();
    let mut remaining = source.size();
    let mut buffer = vec![0_u8; 64 * 1024];
    while remaining > 0 {
        if Instant::now() >= deadline {
            return Err(ProbeFailure::TimedOut);
        }
        let wanted = usize::try_from(remaining.min(buffer.len() as u64))
            .expect("bounded snapshot read length fits usize");
        let read = reader
            .read(&mut buffer[..wanted])
            .map_err(|_| ProbeFailure::ReadFailed)?;
        if read == 0 {
            return Err(ProbeFailure::ReadFailed);
        }
        hasher.update(&buffer[..read]);
        remaining -= u64::try_from(read).expect("read length fits u64");
    }
    if Instant::now() >= deadline {
        return Err(ProbeFailure::TimedOut);
    }
    let mut extra = [0_u8; 1];
    if reader
        .read(&mut extra)
        .map_err(|_| ProbeFailure::ReadFailed)?
        != 0
    {
        return Err(ProbeFailure::ReadFailed);
    }
    if Instant::now() >= deadline {
        return Err(ProbeFailure::TimedOut);
    }
    Ok(crate::util::hex_encode(&hasher.finalize()))
}

fn determine_worktree_evidence(
    target: &ResolvedGitTarget,
    index: Option<&GitPathIdentity>,
    unmerged: &[GitIndexStage],
    raw_blob_hash: Option<&str>,
    deadline: Instant,
) -> std::result::Result<WorktreeEvidence, CollectionFailure> {
    if !unmerged.is_empty() {
        return Ok(comparison_unavailable(
            WorktreeState::Unmerged,
            "git_unmerged_index",
        ));
    }

    let Some(source) = target.source.as_ref() else {
        return Ok(if index.is_some() {
            WorktreeEvidence {
                state: WorktreeState::Deleted,
                clean: Some(false),
                reason: None,
            }
        } else {
            WorktreeEvidence {
                state: WorktreeState::Absent,
                clean: Some(true),
                reason: None,
            }
        });
    };

    let Some(index) = index else {
        let ignored = read_ignored_state(target, deadline)?;
        return Ok(WorktreeEvidence {
            state: if ignored {
                WorktreeState::Ignored
            } else {
                WorktreeState::Untracked
            },
            clean: Some(false),
            reason: None,
        });
    };

    if read_index_flag_state(target, deadline)? != IndexFlagState::Normal {
        return Ok(comparison_unavailable(
            WorktreeState::ComparisonUnavailable,
            "git_index_flags_unsupported",
        ));
    }
    if path_has_content_transform(target, deadline)? {
        return Ok(comparison_unavailable(
            WorktreeState::ComparisonUnavailable,
            "git_content_transform_required",
        ));
    }
    if !matches!(index.mode.as_str(), "100644" | "100755") {
        return Ok(comparison_unavailable(
            WorktreeState::ComparisonUnavailable,
            "git_index_mode_unsupported",
        ));
    }

    let core_filemode = read_core_filemode(target, deadline)?;
    let mode_matches = match worktree_mode_matches(target, source, &index.mode, core_filemode) {
        Ok(matches) => matches,
        Err(reason) => {
            return Ok(comparison_unavailable(
                WorktreeState::ComparisonUnavailable,
                reason,
            ));
        }
    };
    let blob_matches = raw_blob_hash.is_some_and(|hash| hash == index.object_id.as_str());
    let clean = blob_matches && mode_matches;
    Ok(WorktreeEvidence {
        state: if clean {
            WorktreeState::Clean
        } else {
            WorktreeState::Modified
        },
        clean: Some(clean),
        reason: None,
    })
}

const fn comparison_unavailable(state: WorktreeState, reason: &'static str) -> WorktreeEvidence {
    WorktreeEvidence {
        state,
        clean: None,
        reason: Some(reason),
    }
}

fn read_ignored_state(
    target: &ResolvedGitTarget,
    deadline: Instant,
) -> std::result::Result<bool, CollectionFailure> {
    // `git check-ignore` rejects `GIT_LITERAL_PATHSPECS=1`, even for ordinary
    // filenames. `ls-files` provides the same ignored/untracked distinction
    // while retaining the probe runner's mandatory literal-path hardening.
    let mut args = os_args(&[
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "-z",
        "--",
    ]);
    args.push(target.file_name.clone());
    let output = run_named_probe(
        "ignored_path",
        OsStr::new("git"),
        &target.parent,
        &args,
        deadline,
    )?;
    if !output.status.success() {
        return Err(CollectionFailure::Semantic("probe_failed"));
    }
    parse_single_ignored_match(&output.stdout).ok_or(CollectionFailure::Semantic("probe_failed"))
}

fn parse_single_ignored_match(output: &[u8]) -> Option<bool> {
    if output.is_empty() {
        return Some(false);
    }
    let records = split_nul_records(output)?;
    // The command supplies exactly one literal pathspec, so one record is the
    // requested leaf. Deliberately validate cardinality rather than comparing
    // Git's platform-specific output encoding with `OsStr` (notably WTF-16 on
    // Windows).
    if records.len() != 1 || records[0].is_empty() {
        return None;
    }
    Some(true)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum IndexFlagState {
    Normal,
    AssumeUnchanged,
    SkipWorktree,
}

fn read_index_flag_state(
    target: &ResolvedGitTarget,
    deadline: Instant,
) -> std::result::Result<IndexFlagState, CollectionFailure> {
    let mut args = os_args(&["ls-files", "-v", "-z", "--"]);
    args.push(target.file_name.clone());
    let output = run_named_probe(
        "index_flags",
        OsStr::new("git"),
        &target.parent,
        &args,
        deadline,
    )?;
    if !output.status.success() {
        return Err(CollectionFailure::Semantic("probe_failed"));
    }
    let records =
        split_nul_records(&output.stdout).ok_or(CollectionFailure::Semantic("probe_failed"))?;
    if records.len() != 1 || records[0].len() < 3 || records[0][1] != b' ' {
        return Err(CollectionFailure::Semantic("probe_failed"));
    }
    let tag = records[0][0];
    if tag.eq_ignore_ascii_case(&b'S') {
        Ok(IndexFlagState::SkipWorktree)
    } else if tag.is_ascii_lowercase() {
        Ok(IndexFlagState::AssumeUnchanged)
    } else {
        Ok(IndexFlagState::Normal)
    }
}

fn path_has_content_transform(
    target: &ResolvedGitTarget,
    deadline: Instant,
) -> std::result::Result<bool, CollectionFailure> {
    if read_effective_attributes_file(target, deadline)? || read_core_autocrlf(target, deadline)? {
        return Ok(true);
    }

    let mut args = os_args(&[
        "check-attr",
        "-z",
        "filter",
        "text",
        "eol",
        "working-tree-encoding",
        "ident",
        "crlf",
        "--",
    ]);
    args.push(target.file_name.clone());
    let output = run_named_probe(
        "path_attributes",
        OsStr::new("git"),
        &target.parent,
        &args,
        deadline,
    )?;
    if !output.status.success() {
        return Err(CollectionFailure::Semantic("probe_failed"));
    }
    attributes_require_transform(&output.stdout).ok_or(CollectionFailure::Semantic("probe_failed"))
}

fn read_effective_attributes_file(
    target: &ResolvedGitTarget,
    deadline: Instant,
) -> std::result::Result<bool, CollectionFailure> {
    let output = run_named_effective_config_probe(
        "core_attributes_file",
        &target.parent,
        "core.attributesFile",
        deadline,
    )?;
    match output.status.code() {
        Some(0) => Ok(!trim_ascii(&output.stdout).is_empty()),
        Some(1) => Ok(false),
        _ => Err(CollectionFailure::Semantic("probe_failed")),
    }
}

fn attributes_require_transform(output: &[u8]) -> Option<bool> {
    let records = split_nul_records(output)?;
    if records.len() % 3 != 0 {
        return None;
    }
    let mut transform = false;
    for triple in records.as_chunks::<3>().0 {
        if triple[0].is_empty() || triple[1].is_empty() {
            return None;
        }
        let value = triple[2];
        if value != b"unspecified" && value != b"unset" {
            transform = true;
        }
    }
    Some(transform)
}

fn read_core_autocrlf(
    target: &ResolvedGitTarget,
    deadline: Instant,
) -> std::result::Result<bool, CollectionFailure> {
    let output = run_named_effective_config_probe(
        "core_autocrlf",
        &target.parent,
        "core.autocrlf",
        deadline,
    )?;
    match output.status.code() {
        Some(1) => Ok(false),
        Some(0) => match trim_ascii(&output.stdout).to_ascii_lowercase().as_slice() {
            b"false" | b"no" | b"off" | b"0" => Ok(false),
            // "true", "yes", "on", "1", "input", and any unrecognized value
            // all imply checkout/commit transformation.
            _ => Ok(true),
        },
        _ => Err(CollectionFailure::Semantic("probe_failed")),
    }
}

fn read_core_filemode(
    target: &ResolvedGitTarget,
    deadline: Instant,
) -> std::result::Result<bool, CollectionFailure> {
    let output = run_named_effective_config_probe(
        "core_filemode",
        &target.parent,
        "core.filemode",
        deadline,
    )?;
    match output.status.code() {
        Some(1) => Ok(cfg!(unix)),
        Some(0) => match trim_ascii(&output.stdout).to_ascii_lowercase().as_slice() {
            b"true" | b"yes" | b"on" | b"1" => Ok(true),
            b"false" | b"no" | b"off" | b"0" => Ok(false),
            _ => Err(CollectionFailure::Semantic("probe_failed")),
        },
        _ => Err(CollectionFailure::Semantic("probe_failed")),
    }
}

#[cfg(unix)]
fn worktree_mode_matches(
    target: &ResolvedGitTarget,
    source: &JsonlSourceSnapshot,
    index_mode: &str,
    core_filemode: bool,
) -> std::result::Result<bool, &'static str> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if !core_filemode {
        return Ok(true);
    }
    let metadata =
        std::fs::symlink_metadata(&target.path).map_err(|_| "git_worktree_metadata_unavailable")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("git_worktree_identity_changed");
    }
    let identity = source.identity();
    if metadata.dev() != identity.device_id() || metadata.ino() != identity.inode() {
        return Err("git_worktree_identity_changed");
    }
    let mode = if metadata.permissions().mode() & 0o111 == 0 {
        "100644"
    } else {
        "100755"
    };
    Ok(mode == index_mode)
}

#[cfg(windows)]
fn worktree_mode_matches(
    _target: &ResolvedGitTarget,
    _source: &JsonlSourceSnapshot,
    index_mode: &str,
    core_filemode: bool,
) -> std::result::Result<bool, &'static str> {
    if !core_filemode || index_mode == "100644" {
        Ok(true)
    } else {
        Err("git_worktree_mode_unsupported")
    }
}

#[cfg(not(any(unix, windows)))]
fn worktree_mode_matches(
    _target: &ResolvedGitTarget,
    _source: &JsonlSourceSnapshot,
    _index_mode: &str,
    core_filemode: bool,
) -> std::result::Result<bool, &'static str> {
    if core_filemode {
        Err("git_worktree_mode_unsupported")
    } else {
        Ok(true)
    }
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug)]
struct ProbeOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ProbeFailure {
    GitUnavailable,
    SpawnFailed,
    TimedOut,
    OutputLimit,
    ReadFailed,
    WaitFailed,
    ReapFailed,
}

impl ProbeFailure {
    const fn reason(self) -> &'static str {
        match self {
            Self::GitUnavailable => "git_unavailable",
            Self::TimedOut => "probe_timed_out",
            Self::OutputLimit => "probe_output_limit",
            Self::SpawnFailed | Self::ReadFailed | Self::WaitFailed | Self::ReapFailed => {
                "probe_failed"
            }
        }
    }
}

fn run_named_probe(
    probe: &'static str,
    program: &OsStr,
    dir: &Path,
    args: &[OsString],
    deadline: Instant,
) -> std::result::Result<ProbeOutput, ProbeFailure> {
    run_named_probe_with_options(probe, program, dir, args, deadline, true, true)
}

fn run_named_effective_config_probe(
    probe: &'static str,
    dir: &Path,
    key: &str,
    deadline: Instant,
) -> std::result::Result<ProbeOutput, ProbeFailure> {
    run_named_probe_with_options(
        probe,
        OsStr::new("git"),
        dir,
        &os_args(&["config", "--get", key]),
        deadline,
        false,
        false,
    )
}

fn run_named_probe_with_options(
    probe: &'static str,
    program: &OsStr,
    dir: &Path,
    args: &[OsString],
    deadline: Instant,
    neutralize_attributes_file: bool,
    isolate_external_config: bool,
) -> std::result::Result<ProbeOutput, ProbeFailure> {
    let started = Instant::now();
    let result = run_git_probe_with_program_options(
        program,
        dir,
        args,
        deadline,
        neutralize_attributes_file,
        isolate_external_config,
    );
    debug!(
        probe,
        duration_ms = elapsed_millis(started),
        failure = ?result.as_ref().err(),
        outcome = match &result {
            Ok(output) if output.status.success() => "ok",
            Ok(_) => "nonzero",
            Err(failure) => failure.reason(),
        },
        "Completed explicit bounded VCS probe"
    );
    result
}

#[cfg(all(test, unix))]
fn run_git_probe_with_program(
    program: &OsStr,
    dir: &Path,
    args: &[OsString],
    deadline: Instant,
) -> std::result::Result<ProbeOutput, ProbeFailure> {
    run_git_probe_with_program_options(program, dir, args, deadline, true, true)
}

fn run_git_probe_with_program_options(
    program: &OsStr,
    dir: &Path,
    args: &[OsString],
    deadline: Instant,
    neutralize_attributes_file: bool,
    isolate_external_config: bool,
) -> std::result::Result<ProbeOutput, ProbeFailure> {
    if Instant::now() >= deadline {
        return Err(ProbeFailure::TimedOut);
    }
    let mut command = hardened_git_command(
        program,
        dir,
        neutralize_attributes_file,
        isolate_external_config,
    );
    command.args(args);
    run_bounded_capture(&mut command, deadline)
}

fn hardened_git_command(
    program: &OsStr,
    dir: &Path,
    neutralize_attributes_file: bool,
    isolate_external_config: bool,
) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(dir)
        .arg("--no-optional-locks")
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "core.untrackedCache=false"])
        .args(["-c", git_hooks_path_override()])
        .stdin(Stdio::null());
    if neutralize_attributes_file {
        command.args(["-c", git_attributes_file_override()]);
    }

    for (key, _) in std::env::vars_os() {
        if is_git_process_environment_key(&key) {
            command.env_remove(key);
        }
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_PAGER", "")
        .env("PAGER", "")
        .env("GCM_INTERACTIVE", "Never")
        .env("LC_ALL", "C");
    if isolate_external_config {
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device());
    } else {
        // Effective-config probes exist to observe the same configuration
        // Git itself would honor for the caller. The hardening sweep above
        // removed every `GIT_*` variable, including Git's documented
        // read-location overrides for the system/global config files. That
        // made external neutralization such as `GIT_CONFIG_SYSTEM=/dev/null`
        // silently ineffective while the host's real /etc/gitconfig still
        // leaked into the probes. Restore exactly the location keys from the
        // caller environment: they only select which *existing files* are
        // read (the caller already controls global scope via `HOME`), unlike
        // `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_*` value injection, which stays
        // stripped.
        for key in EFFECTIVE_CONFIG_SCOPE_ENV_KEYS {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
    }
    command
}

/// Git's read-location overrides for configuration scope, preserved for
/// effective-config probes so `br` observes the same configuration files the
/// caller's own `git` invocations would read.
const EFFECTIVE_CONFIG_SCOPE_ENV_KEYS: [&str; 3] = [
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_NOSYSTEM",
];

fn is_git_process_environment_key(key: &OsStr) -> bool {
    let uppercase = key.to_string_lossy().to_ascii_uppercase();
    uppercase.starts_with("GIT_")
        || matches!(
            uppercase.as_str(),
            "SSH_ASKPASS" | "SSH_ASKPASS_REQUIRE" | "GCM_INTERACTIVE"
        )
}

fn git_attributes_file_override() -> &'static str {
    if cfg!(windows) {
        "core.attributesFile=NUL"
    } else {
        "core.attributesFile=/dev/null"
    }
}

fn git_hooks_path_override() -> &'static str {
    if cfg!(windows) {
        "core.hooksPath=NUL"
    } else {
        "core.hooksPath=/dev/null"
    }
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

#[derive(Debug)]
struct BoundedRead {
    bytes: Vec<u8>,
    truncated: bool,
}

fn anonymous_capture_file() -> std::result::Result<File, ProbeFailure> {
    tempfile::tempfile().map_err(|_| ProbeFailure::ReadFailed)
}

fn read_bounded_capture(
    file: &mut File,
    deadline: Instant,
) -> std::result::Result<BoundedRead, ProbeFailure> {
    if Instant::now() >= deadline {
        return Err(ProbeFailure::TimedOut);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| ProbeFailure::ReadFailed)?;
    if Instant::now() >= deadline {
        return Err(ProbeFailure::TimedOut);
    }
    let mut bytes = Vec::with_capacity(MAX_CAPTURE_BYTES_PER_STREAM + 1);
    file.take(
        u64::try_from(MAX_CAPTURE_BYTES_PER_STREAM)
            .unwrap_or(u64::MAX)
            .saturating_add(1),
    )
    .read_to_end(&mut bytes)
    .map_err(|_| ProbeFailure::ReadFailed)?;
    if Instant::now() >= deadline {
        return Err(ProbeFailure::TimedOut);
    }
    let truncated = bytes.len() > MAX_CAPTURE_BYTES_PER_STREAM;
    bytes.truncate(MAX_CAPTURE_BYTES_PER_STREAM);
    Ok(BoundedRead { bytes, truncated })
}

fn capture_exceeds_limit(file: &File) -> std::result::Result<bool, ProbeFailure> {
    let length = file.metadata().map_err(|_| ProbeFailure::ReadFailed)?.len();
    Ok(length
        > u64::try_from(MAX_CAPTURE_BYTES_PER_STREAM).expect("capture byte limit must fit in u64"))
}

fn terminate_and_reap(child: &mut Child) -> std::result::Result<ExitStatus, ProbeFailure> {
    match child.try_wait() {
        Ok(Some(status)) => return Ok(status),
        Ok(None) => {}
        Err(_) => return Err(ProbeFailure::ReapFailed),
    }
    if child.kill().is_err() {
        return match child.try_wait() {
            Ok(Some(status)) => Ok(status),
            Ok(None) | Err(_) => wait_direct_child(child),
        };
    }
    wait_direct_child(child)
}

fn wait_direct_child(child: &mut Child) -> std::result::Result<ExitStatus, ProbeFailure> {
    loop {
        match child.wait() {
            Ok(status) => return Ok(status),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(ProbeFailure::ReapFailed),
        }
    }
}

fn read_both_captures(
    stdout: &mut File,
    stderr: &mut File,
    deadline: Instant,
) -> std::result::Result<(BoundedRead, BoundedRead), ProbeFailure> {
    let stdout = read_bounded_capture(stdout, deadline)?;
    if Instant::now() >= deadline {
        return Err(ProbeFailure::TimedOut);
    }
    let stderr = read_bounded_capture(stderr, deadline)?;
    if Instant::now() >= deadline {
        return Err(ProbeFailure::TimedOut);
    }
    Ok((stdout, stderr))
}

fn finish_failed_probe(
    child: &mut Child,
    failure: ProbeFailure,
) -> std::result::Result<ProbeOutput, ProbeFailure> {
    terminate_and_reap(child)?;
    Err(failure)
}

fn run_bounded_capture(
    command: &mut Command,
    deadline: Instant,
) -> std::result::Result<ProbeOutput, ProbeFailure> {
    // Anonymous regular files avoid pipe-EOF waits when the trusted direct
    // executable exits after a descendant inherits its output descriptors.
    // We still bound retained bytes, poll on-disk lengths, and kill/reap the
    // direct child on every observed timeout or runner error. Reaping is
    // mandatory cleanup and may extend beyond the probe execution deadline.
    if Instant::now() >= deadline {
        return Err(ProbeFailure::TimedOut);
    }
    let mut stdout = anonymous_capture_file()?;
    let mut stderr = anonymous_capture_file()?;
    let child_stdout = stdout.try_clone().map_err(|_| ProbeFailure::ReadFailed)?;
    let child_stderr = stderr.try_clone().map_err(|_| ProbeFailure::ReadFailed)?;
    command
        .stdout(Stdio::from(child_stdout))
        .stderr(Stdio::from(child_stderr));

    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ProbeFailure::GitUnavailable
        } else {
            ProbeFailure::SpawnFailed
        }
    })?;

    let status = loop {
        if Instant::now() >= deadline {
            return finish_failed_probe(&mut child, ProbeFailure::TimedOut);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if Instant::now() >= deadline {
                    return finish_failed_probe(&mut child, ProbeFailure::TimedOut);
                }
                break status;
            }
            Ok(None) => {
                let output_limited = match (
                    capture_exceeds_limit(&stdout),
                    capture_exceeds_limit(&stderr),
                ) {
                    (Ok(stdout_limited), Ok(stderr_limited)) => stdout_limited || stderr_limited,
                    _ => {
                        return finish_failed_probe(&mut child, ProbeFailure::ReadFailed);
                    }
                };
                if Instant::now() >= deadline {
                    return finish_failed_probe(&mut child, ProbeFailure::TimedOut);
                }
                if output_limited {
                    return finish_failed_probe(&mut child, ProbeFailure::OutputLimit);
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Err(_) => {
                return finish_failed_probe(&mut child, ProbeFailure::WaitFailed);
            }
        }
    };

    if Instant::now() >= deadline {
        return Err(ProbeFailure::TimedOut);
    }
    let (stdout, stderr) = read_both_captures(&mut stdout, &mut stderr, deadline)?;
    if Instant::now() >= deadline {
        return Err(ProbeFailure::TimedOut);
    }
    if stdout.truncated || stderr.truncated {
        return Err(ProbeFailure::OutputLimit);
    }
    Ok(ProbeOutput {
        status,
        stdout: stdout.bytes,
    })
}

fn render_human(status: &VcsExportStatus) {
    println!("Git export status");
    println!("  JSONL: {}", sanitize_terminal_inline(&status.path));
    println!("  Path scope: {}", status.path_scope);
    println!("  Atomic observation: no (sequential evidence)");
    if !status.available {
        println!("  Available: no");
        println!("  Reason: {}", status.reason.unwrap_or("probe_failed"));
        println!("  Retry: {EXPLICIT_COMMAND}");
        return;
    }
    println!("  Available: yes");
    println!(
        "  Object format: {}",
        object_format_label(status.object_format)
    );
    println!("  Tracked: {}", optional_yes_no(status.tracked));
    println!("  HEAD path: {}", identity_label(status.head.as_ref()));
    println!("  Index path: {}", identity_label(status.index.as_ref()));
    println!("  Index clean: {}", optional_yes_no(status.index_clean));
    println!(
        "  Worktree state: {}",
        worktree_state_label(status.worktree_state)
    );
    println!(
        "  Worktree clean: {}",
        optional_yes_no(status.worktree_clean)
    );
    if let Some(reason) = status.worktree_comparison_reason {
        println!("  Worktree comparison: unavailable ({reason})");
    }
    println!(
        "  Worktree raw Git blob: {}",
        status
            .worktree_raw_git_blob_hash
            .as_deref()
            .unwrap_or("not present")
    );
    println!(
        "  Worktree raw SHA-256: {}",
        status
            .worktree_raw_sha256
            .as_deref()
            .unwrap_or("not present")
    );
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn optional_yes_no(value: Option<bool>) -> &'static str {
    value.map_or("unavailable", yes_no)
}

fn object_format_label(value: Option<GitObjectFormat>) -> &'static str {
    match value {
        Some(GitObjectFormat::Sha1) => "sha1",
        Some(GitObjectFormat::Sha256) => "sha256",
        None => "unavailable",
    }
}

fn identity_label(identity: Option<&GitPathIdentity>) -> String {
    identity.map_or_else(
        || "not present".to_string(),
        |identity| {
            format!(
                "{} {} {}",
                identity.mode, identity.object_type, identity.object_id
            )
        },
    )
}

fn worktree_state_label(value: Option<WorktreeState>) -> &'static str {
    match value {
        Some(WorktreeState::Clean) => "clean",
        Some(WorktreeState::Modified) => "modified",
        Some(WorktreeState::Deleted) => "deleted",
        Some(WorktreeState::Untracked) => "untracked",
        Some(WorktreeState::Ignored) => "ignored",
        Some(WorktreeState::Unmerged) => "unmerged",
        Some(WorktreeState::ComparisonUnavailable) => "comparison unavailable",
        Some(WorktreeState::Absent) => "absent",
        None => "unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GitObjectFormat, ProbeFailure, attributes_require_transform, external_path_descriptor,
        hardened_git_command, hash_snapshot_as_git_blob, is_git_process_environment_key,
        parse_head_identity, parse_index_entries, parse_object_id, parse_single_ignored_match,
        workspace_path_label,
    };
    #[cfg(unix)]
    use super::{
        MAX_CAPTURE_BYTES_PER_STREAM, PathScope, ResolvedGitTarget, WorktreeState,
        collect_git_export_status, run_git_probe_with_program,
    };
    use std::ffi::OsStr;
    #[cfg(unix)]
    use std::ffi::OsString;
    use std::fs;
    use std::path::Path;
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(unix)]
    use std::time::Duration;
    use std::time::Instant;
    use tempfile::TempDir;

    #[test]
    fn object_id_parser_obeys_the_selected_repository_format() {
        let sha1 = b"0123456789012345678901234567890123456789";
        assert!(
            parse_object_id(sha1, GitObjectFormat::Sha1).is_some(),
            "SHA-1 repositories accept forty hexadecimal digits"
        );
        assert!(parse_object_id(sha1, GitObjectFormat::Sha256).is_none());
        let sha256 = b"0123456789012345678901234567890123456789012345678901234567890123";
        assert!(parse_object_id(sha256, GitObjectFormat::Sha256).is_some());
        assert!(parse_object_id(sha256, GitObjectFormat::Sha1).is_none());
        assert!(parse_object_id(b"not-a-hash", GitObjectFormat::Sha1).is_none());
    }

    #[test]
    fn tree_and_index_parsers_preserve_exact_modes_ids_and_unmerged_stages() {
        let oid = b"0123456789012345678901234567890123456789";
        let mut tree = b"100755 blob ".to_vec();
        tree.extend_from_slice(oid);
        tree.extend_from_slice(b"\t.beads/issues.jsonl\0");
        let head = parse_head_identity(&tree, GitObjectFormat::Sha1)
            .expect("valid tree record")
            .expect("present tree record");
        assert_eq!(head.mode, "100755");
        assert_eq!(head.object_type, "blob");

        let mut index = b"100644 ".to_vec();
        index.extend_from_slice(oid);
        index.extend_from_slice(b" 1\t.beads/issues.jsonl\x00100755 ");
        index.extend_from_slice(oid);
        index.extend_from_slice(b" 2\t.beads/issues.jsonl\0");
        let parsed =
            parse_index_entries(&index, GitObjectFormat::Sha1).expect("valid unmerged index");
        assert!(parsed.stage_zero.is_none());
        assert_eq!(
            parsed
                .unmerged
                .iter()
                .map(|entry| (entry.stage, entry.mode.as_str()))
                .collect::<Vec<_>>(),
            [(1, "100644"), (2, "100755")]
        );
    }

    #[test]
    fn transform_attribute_parser_is_fail_closed() {
        assert_eq!(
            attributes_require_transform(
                b".beads/issues.jsonl\0filter\0unspecified\0\
                  .beads/issues.jsonl\0text\0unset\0"
            ),
            Some(false)
        );
        assert_eq!(
            attributes_require_transform(b".beads/issues.jsonl\0filter\0sentinel\0"),
            Some(true)
        );
        assert_eq!(
            attributes_require_transform(b".beads/issues.jsonl\0text\0\0"),
            Some(true)
        );
        assert_eq!(attributes_require_transform(b"truncated\0filter\0"), None);
    }

    #[test]
    fn ignored_path_parser_distinguishes_no_match_and_fails_closed() {
        assert_eq!(parse_single_ignored_match(b""), Some(false));
        assert_eq!(parse_single_ignored_match(b"issues.jsonl\0"), Some(true));
        assert_eq!(parse_single_ignored_match(b"issues.jsonl"), None);
        assert_eq!(
            parse_single_ignored_match(b"first.jsonl\0second.jsonl\0"),
            None
        );
        assert_eq!(parse_single_ignored_match(b"\0"), None);
    }

    #[test]
    fn hardened_command_disables_prompts_optional_locks_and_pathspec_magic() {
        let command = hardened_git_command(OsStr::new("git"), Path::new("."), true, true);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|arg| arg == "--no-optional-locks"));
        assert!(args.iter().any(|arg| arg == "core.fsmonitor=false"));
        assert!(
            args.iter()
                .any(|arg| arg.starts_with("core.attributesFile="))
        );
        assert!(args.iter().any(|arg| arg.starts_with("core.hooksPath=")));

        let env: std::collections::BTreeMap<_, _> = command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((
                    key.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(env.get("GIT_OPTIONAL_LOCKS").map(String::as_str), Some("0"));
        assert_eq!(
            env.get("GIT_TERMINAL_PROMPT").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            env.get("GIT_LITERAL_PATHSPECS").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            env.get("GIT_CONFIG_NOSYSTEM").map(String::as_str),
            Some("1")
        );
        assert_eq!(env.get("GIT_ATTR_NOSYSTEM").map(String::as_str), Some("1"));
        assert_eq!(env.get("GIT_NO_LAZY_FETCH").map(String::as_str), Some("1"));
        assert_eq!(env.get("GIT_PAGER").map(String::as_str), Some(""));
        assert_eq!(env.get("PAGER").map(String::as_str), Some(""));
    }

    #[test]
    fn effective_config_probe_keeps_default_config_precedence_without_path_disclosure() {
        let command = hardened_git_command(OsStr::new("git"), Path::new("."), false, false);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("core.attributesFile=")),
            "the fixed-key query must not mask the effective attributesFile value"
        );
        let env = command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((
                    key.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        // The effective-config probe must never force its own isolation
        // values; the only permitted entries for these keys mirror what the
        // caller environment already carries (Git's documented read-location
        // overrides), so external neutralization such as
        // `GIT_CONFIG_SYSTEM=/dev/null` keeps working while a caller without
        // those variables retains Git's normal system/global precedence.
        for key in super::EFFECTIVE_CONFIG_SCOPE_ENV_KEYS {
            assert_eq!(
                env.get(key).map(std::ffi::OsString::from),
                std::env::var_os(key),
                "effective config probe must mirror the caller's {key}"
            );
        }
    }

    #[test]
    fn inherited_git_and_askpass_keys_are_removed_case_insensitively() {
        for key in [
            "GIT_DIR",
            "git_config_count",
            "Git_AskPass",
            "SSH_ASKPASS",
            "ssh_askpass_require",
            "Gcm_Interactive",
        ] {
            assert!(
                is_git_process_environment_key(OsStr::new(key)),
                "{key} must be stripped"
            );
        }
        assert!(!is_git_process_environment_key(OsStr::new("PATH")));
    }

    #[test]
    fn workspace_labels_use_forward_slashes() {
        let label = workspace_path_label(
            Path::new("workspace/.beads/nested/issues.jsonl"),
            Path::new("workspace/.beads"),
        );
        assert_eq!(label, ".beads/nested/issues.jsonl");
    }

    #[test]
    fn external_descriptor_never_contains_the_path() {
        let path = Path::new("/private/customer/project/.beads/issues.jsonl");
        let descriptor = external_path_descriptor(path);
        assert!(descriptor.starts_with("<external-jsonl sha256="));
        assert!(!descriptor.contains("private"));
        assert!(!descriptor.contains("customer"));
    }

    #[test]
    fn raw_blob_hash_refuses_work_after_its_deadline() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("large.jsonl");
        fs::write(&path, vec![b'x'; 2 * 1024 * 1024]).expect("large source fixture");
        let source = crate::sync::capture_optional_jsonl_source(&path)
            .expect("capture source")
            .expect("source present");
        let result = hash_snapshot_as_git_blob(&source, GitObjectFormat::Sha1, Instant::now());
        assert_eq!(
            result.expect_err("expired hash deadline must fail before hashing"),
            ProbeFailure::TimedOut
        );
    }

    #[cfg(unix)]
    fn executable_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, body).expect("write script");
        let mut permissions = fs::metadata(&path).expect("script metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).expect("make script executable");
        path
    }

    #[cfg(unix)]
    #[test]
    fn probe_deadline_terminates_and_reaps_direct_child_with_bounded_capture() {
        let temp = TempDir::new().expect("temp dir");
        let script = executable_script(
            temp.path(),
            "git-timeout",
            "#!/bin/sh\nprintf 'started'\nprintf 'started' >&2\nwhile :; do :; done\n",
        );
        let started = Instant::now();
        let result = run_git_probe_with_program(
            script.as_os_str(),
            temp.path(),
            &[],
            started + Duration::from_millis(75),
        );
        assert_eq!(
            result.expect_err("flood must time out"),
            ProbeFailure::TimedOut
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "bounded runner failed to reap the direct child promptly: {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn deadline_wins_when_success_is_observed_only_after_expiry() {
        let temp = TempDir::new().expect("temp dir");
        let script = executable_script(
            temp.path(),
            "git-late-success",
            "#!/bin/sh\nsleep 0.05\nexit 0\n",
        );
        let result = run_git_probe_with_program(
            script.as_os_str(),
            temp.path(),
            &[],
            Instant::now() + Duration::from_millis(20),
        );
        assert_eq!(
            result.expect_err("late observation must not be accepted as success"),
            ProbeFailure::TimedOut
        );
    }

    #[cfg(unix)]
    #[test]
    fn finite_oversized_output_is_rejected_by_hard_capture_cap() {
        let temp = TempDir::new().expect("temp dir");
        let script = executable_script(
            temp.path(),
            "git-oversized",
            "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 4096 ]; do\n  printf '0123456789abcdef0123456789abcdef'\n  i=$((i + 1))\ndone\n",
        );
        let result = run_git_probe_with_program(
            script.as_os_str(),
            temp.path(),
            &[],
            Instant::now() + Duration::from_secs(2),
        );
        assert_eq!(
            result.expect_err("oversized output must be rejected"),
            ProbeFailure::OutputLimit
        );
        const { assert!(MAX_CAPTURE_BYTES_PER_STREAM < 4096 * 32) };
    }

    #[cfg(unix)]
    #[test]
    fn inherited_descendant_output_descriptors_do_not_delay_return() {
        let temp = TempDir::new().expect("temp dir");
        let marker = temp.path().join("descendant-finished");
        let marker_text = marker
            .to_str()
            .expect("temporary test path must be UTF-8 for shell fixture");
        assert!(
            !marker_text.contains('\''),
            "temporary test path must not require shell quote escaping"
        );
        let script_body = format!(
            "#!/bin/sh\n(sleep 0.8; printf 'done' > '{marker_text}') &\nprintf 'direct child done'\nexit 0\n"
        );
        let script = executable_script(temp.path(), "git-inherited-descriptors", &script_body);

        let started = Instant::now();
        let output = run_git_probe_with_program(
            script.as_os_str(),
            temp.path(),
            &[],
            started + Duration::from_secs(2),
        )
        .expect("direct child should complete successfully");
        let elapsed = started.elapsed();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"direct child done");
        assert!(
            elapsed < Duration::from_millis(500),
            "capture waited for the descendant's inherited descriptors: {elapsed:?}"
        );

        let marker_deadline = Instant::now() + Duration::from_secs(2);
        while !marker.is_file() && Instant::now() < marker_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            marker.is_file(),
            "background fixture did not self-terminate as expected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_jsonl_leaf_is_observed_without_lossy_argument_conversion() {
        use std::os::unix::ffi::OsStringExt;

        let temp = TempDir::new().expect("temp dir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("beads dir");
        let file_name = OsString::from_vec(b"issues-\xff.jsonl".to_vec());
        let path = beads_dir.join(&file_name);
        fs::write(&path, b"{\"id\":\"bd-x\"}\n").expect("jsonl");

        let init = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(temp.path())
            .output()
            .expect("git init");
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        let add = Command::new("git")
            .arg("add")
            .arg("--")
            .arg(Path::new(".beads").join(&file_name))
            .current_dir(temp.path())
            .output()
            .expect("git add");
        assert!(
            add.status.success(),
            "{}",
            String::from_utf8_lossy(&add.stderr)
        );
        let commit = Command::new("git")
            .args([
                "-c",
                "user.name=br-test",
                "-c",
                "user.email=br-test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "track non-utf8 jsonl",
            ])
            .current_dir(temp.path())
            .output()
            .expect("git commit");
        assert!(
            commit.status.success(),
            "{}",
            String::from_utf8_lossy(&commit.stderr)
        );

        let target = ResolvedGitTarget {
            path: path.clone(),
            parent: beads_dir.clone(),
            file_name,
            source: Some(
                crate::sync::capture_optional_jsonl_source(&path)
                    .expect("capture JSONL")
                    .expect("JSONL source must be present"),
            ),
            scope: PathScope::Workspace,
            path_label: workspace_path_label(&path, &beads_dir),
            source_capture_timed_out: false,
        };
        let started = Instant::now();
        let status = collect_git_export_status(
            &target,
            2_000,
            started,
            started + Duration::from_millis(2_000),
        );
        assert!(status.available, "{status:?}");
        assert_eq!(status.tracked, Some(true));
        assert_eq!(status.worktree_clean, Some(true));
        assert_eq!(status.index_clean, Some(true));
        assert_eq!(status.worktree_state, Some(WorktreeState::Clean));
        assert_eq!(
            status.head.as_ref().map(|head| head.object_id.as_str()),
            status.worktree_raw_git_blob_hash.as_deref()
        );
        assert!(status.path.starts_with("<workspace-jsonl sha256="));
    }
}
