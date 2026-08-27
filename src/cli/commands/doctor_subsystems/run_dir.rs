//! `.doctor/runs/<run-id>/` artifact directory (R-002).
//!
//! Every `br doctor --repair` run lays down an artifact directory:
//!
//! ```text
//! <repo>/.doctor/runs/<run-id>/
//!   actions.jsonl   # one line per mutate() call
//!   backups/        # verbatim pre-mutation copies
//!   report.json     # final report (written at end of run)
//!   undo.sh         # pure-bash fallback when br itself is broken
//! <repo>/.doctor/latest -> runs/<run-id>/   # best-effort convenience symlink
//! ```
//!
//! ## Run identifier
//!
//! `run_id` = `<UTC ISO 8601 seconds>__<short-hex>` where `short-hex` is
//! a SHA-256 truncation of repo identity plus per-process entropy. The
//! shape is human-sortable and unique-per-run.
//!
//! ## Escape hatch
//!
//! If `BR_DOCTOR_RUNS_DIR` is set in the environment, the run-dir is
//! placed under that path instead of `<repo>/.doctor/runs/`. This is
//! the documented escape hatch for read-only working trees and CI
//! sandboxes.
//!
//! ## Atomic symlink update
//!
//! `<repo>/.doctor/latest` is updated with a tmp-symlink + rename so
//! readers either see the previous run or the new run, never a torn
//! state. On Windows hosts where symlink creation specifically fails
//! with `ERROR_PRIVILEGE_NOT_HELD` (1314), the run remains successful:
//! `br doctor undo latest` also scans the validated run directories.
//!
//! ## .gitignore
//!
//! On creation we ensure `.doctor/` is in `<repo>/.gitignore`. The
//! existing `.beads/` ignore patterns and conventions are not touched.

#![allow(dead_code)] // WP1 foundation; consumed by WP3-WP12.

use std::fmt::Write as FmtWrite;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::error::BeadsError;

/// Environment variable that overrides the `.doctor/runs/` location.
pub const ENV_RUNS_DIR: &str = "BR_DOCTOR_RUNS_DIR";

static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
const WINDOWS_ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    let resolved_target = link
        .parent()
        .map_or_else(|| target.to_path_buf(), |parent| parent.join(target));
    if resolved_target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(all(not(unix), not(windows)))]
fn create_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "doctor: symlink creation is not supported on this platform",
    ))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Concrete handles for the artifact directory of a single run.
#[derive(Debug, Clone)]
pub struct RunDir {
    /// Stable run identifier (ISO-8601 + short hash).
    pub run_id: String,
    /// Workspace root whose files this run may restore.
    pub repo_root: PathBuf,
    /// `<runs_root>/<run-id>/`.
    pub root: PathBuf,
    /// `<root>/backups/`.
    pub backups: PathBuf,
    /// `<root>/actions.jsonl`.
    pub actions_file: PathBuf,
    /// `<root>/report.json`.
    pub report_file: PathBuf,
    /// `<root>/undo.sh` (only after [`write_undo_sh`] is called).
    pub undo_script: PathBuf,
    /// `<repo_root>/.doctor/latest` (or the `BR_DOCTOR_RUNS_DIR`
    /// equivalent). Best-effort symlink to `root`; it may be absent on
    /// Windows when the process lacks symlink privileges.
    pub latest_link: PathBuf,
}

/// Create a fresh run directory under `<repo>/.doctor/runs/` (or the
/// `BR_DOCTOR_RUNS_DIR` override).
///
/// On success:
/// - The run directory exists with `backups/`, `actions.jsonl`,
///   `report.json`.
/// - `<runs_root>/../latest` points at the new run dir when the platform
///   permits symlink creation.
/// - `<repo>/.gitignore` contains `.doctor/` (added if missing).
///
/// # Errors
///
/// Returns [`BeadsError`] for I/O faults or for the case where
/// `repo_root` does not exist.
pub fn create_run_dir(repo_root: &Path) -> Result<RunDir, BeadsError> {
    let (run, _actions_file) = create_run_dir_with_actions_file(repo_root)?;
    Ok(run)
}

/// Create a fresh repair run directory and return the already-open append
/// handle for its `actions.jsonl` audit log.
///
/// Keeping the initial handle avoids a close-then-immediate-reopen seam that
/// can be rejected by Windows antivirus or endpoint-security software.
///
/// # Errors
///
/// Returns [`BeadsError`] under the same conditions as [`create_run_dir`].
pub fn create_repair_run_dir(repo_root: &Path) -> Result<(RunDir, std::fs::File), BeadsError> {
    create_run_dir_with_actions_file(repo_root)
}

fn create_run_dir_with_actions_file(
    repo_root: &Path,
) -> Result<(RunDir, std::fs::File), BeadsError> {
    if !repo_root.exists() {
        return Err(BeadsError::internal(format!(
            "doctor: repo_root {} does not exist",
            repo_root.display()
        )));
    }

    // Round-5 fresh-eyes follow-through (`beads_rust-dfjs`): when
    // `BR_DOCTOR_RUNS_DIR` is set the run artifacts live OUTSIDE
    // `<repo>/.doctor/`, so adding `.doctor/` to `<repo>/.gitignore`
    // would be a surprise mutation against the parent tree without
    // any benefit. Test fixtures, CI sandboxes, and `br doctor undo`
    // (which builds a fresh run-dir purely to audit its own writes)
    // are the primary callers of the env-override path. Skip the
    // gitignore touch in that case so those callers cannot
    // accidentally mutate a host repo's `.gitignore` outside the
    // chokepoint.
    if std::env::var_os(ENV_RUNS_DIR).is_none() {
        ensure_doctor_in_gitignore(repo_root)?;
    }

    let runs_root = runs_root_for(repo_root);
    fs::create_dir_all(&runs_root).map_err(BeadsError::Io)?;

    let run_id = generate_run_id(repo_root);
    let root = runs_root.join(&run_id);
    let backups = root.join("backups");
    fs::create_dir_all(&backups).map_err(BeadsError::Io)?;

    let actions_file = root.join("actions.jsonl");
    let actions_handle = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&actions_file)
        .map_err(BeadsError::Io)?;

    let report_file = root.join("report.json");
    if !report_file.exists() {
        // Touch an empty placeholder so the file path is stable.
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&report_file)
            .map_err(BeadsError::Io)?;
    }

    let undo_script = root.join("undo.sh");

    // .doctor/latest symlink (points relative inside the runs root so
    // it survives moves of the repo root).
    let latest_link = runs_root.parent().unwrap_or(&runs_root).join("latest");
    update_latest_symlink(&latest_link, &root)?;

    Ok((
        RunDir {
            run_id,
            repo_root: repo_root.to_path_buf(),
            root,
            backups,
            actions_file,
            report_file,
            undo_script,
            latest_link,
        },
        actions_handle,
    ))
}

/// Resolve where `runs/` should live for a given repo, honoring the
/// `BR_DOCTOR_RUNS_DIR` env var.
fn runs_root_for(repo_root: &Path) -> PathBuf {
    runs_root_with_override(repo_root, std::env::var_os(ENV_RUNS_DIR).map(PathBuf::from))
}

/// Inner form of [`runs_root_for`] with an explicit override so tests
/// can exercise the redirect without mutating process-wide environment
/// state (the crate enforces `#![forbid(unsafe_code)]` so
/// `std::env::set_var` is unavailable).
fn runs_root_with_override(repo_root: &Path, env_override: Option<PathBuf>) -> PathBuf {
    if let Some(dir) = env_override {
        return dir.join("runs");
    }
    repo_root.join(".doctor").join("runs")
}

fn generate_run_id(repo_root: &Path) -> String {
    let now = Utc::now();
    let iso = now.format("%Y%m%dT%H%M%SZ").to_string();
    let nanos = now.timestamp_nanos_opt().unwrap_or_default();
    let ordinal = RUN_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(repo_root.to_string_lossy().as_bytes());
    hasher.update(iso.as_bytes());
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(ordinal.to_le_bytes());
    let mut short = String::with_capacity(6);
    for byte in hasher.finalize().iter().take(3) {
        write!(&mut short, "{byte:02x}").expect("writing to a String cannot fail");
    }
    format!("{iso}__{short}")
}

/// Atomically point `latest_link` at `target`. If the link already
/// exists, replace it via tmp-symlink + rename.
fn update_latest_symlink(latest_link: &Path, target: &Path) -> Result<(), BeadsError> {
    if let Some(parent) = latest_link.parent() {
        fs::create_dir_all(parent).map_err(BeadsError::Io)?;
    }

    // Use a relative target so the link stays valid if `<repo>` is
    // moved. The symlink lives under `<runs_root>/..`, the target lives
    // at `<runs_root>/<run-id>/`, so `runs/<run-id>/` is the right
    // relative path.
    let rel_target = target
        .strip_prefix(latest_link.parent().unwrap_or(target))
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| target.to_path_buf());

    let tmp = latest_link.with_file_name(format!(
        ".latest.doctor-tmp.{}.{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));

    // Clear stale tmp from a crashed run unconditionally — using
    // `is_ok()` then `remove_file` opens a TOCTOU window where a
    // concurrent process could delete the symlink between our check
    // and our remove, turning NotFound into a hard error. Treat
    // NotFound as success.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(BeadsError::Io(e)),
    }
    if let Err(error) = create_symlink(&rel_target, &tmp) {
        if cfg!(windows) && is_windows_symlink_privilege_error(&error) {
            tracing::warn!(
                path = %latest_link.display(),
                "doctor: Windows symlink privilege unavailable; latest run remains discoverable by directory scan"
            );
            return Ok(());
        }
        return Err(BeadsError::Io(error));
    }
    // `fs::rename` over an existing symlink atomically replaces it on
    // Unix.
    fs::rename(&tmp, latest_link).map_err(BeadsError::Io)?;
    fsync_dir(latest_link.parent().unwrap_or_else(|| Path::new(".")))?;
    Ok(())
}

fn is_windows_symlink_privilege_error(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(WINDOWS_ERROR_PRIVILEGE_NOT_HELD)
}

/// Ensure `<repo>/.gitignore` contains a `.doctor/` ignore rule. Adds
/// it idempotently; never removes or rewrites unrelated entries.
///
/// ## Chokepoint carveout (`beads_rust-dfjs`)
///
/// This is the **sole pre-chokepoint write** in the doctor pipeline.
/// The chokepoint requires a run-dir; the run-dir lives at
/// `<repo>/.doctor/runs/<run-id>/`; therefore `.doctor/` MUST be in
/// `.gitignore` before the chokepoint can record its first action,
/// otherwise the run-artifact directory itself would be checked in to
/// VCS. There is no chicken-and-egg-free option.
///
/// Mitigations layered on top of the carveout:
///
/// 1. **Idempotence**: if `.doctor/`, `.doctor`, or `/.doctor/` is
///    already present anywhere in `.gitignore`, this function is a
///    no-op — no rewrite, no fsync. Repeated `--repair`
///    invocations therefore do not pile up writes.
/// 2. **Atomic write**: tmp-file + persist + fsync. A concurrent
///    reader sees either the old or the new contents, never a torn
///    write. (TOCTOU between the read and the persist is bounded by
///    the workspace write lock that `--repair` holds; see
///    `beads_rust-sexc` round-4 wiring.)
/// 3. **Test isolation**: callers that set `BR_DOCTOR_RUNS_DIR` (CI,
///    `br doctor undo`, fixtures) cause `create_run_dir` to skip this
///    call entirely, so no parent-tree `.gitignore` is mutated when
///    the run artifacts are diverted out of `<repo>/.doctor/`.
///
/// Any *other* pre-chokepoint write is a contract violation; see the
/// `pre_chokepoint_writes_are_only_gitignore` regression test below.
fn ensure_doctor_in_gitignore(repo_root: &Path) -> Result<(), BeadsError> {
    let gitignore = repo_root.join(".gitignore");
    let needle = ".doctor/";
    let existing = match fs::read_to_string(&gitignore) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(BeadsError::Io(e)),
    };
    let already = existing.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == needle || trimmed == ".doctor" || trimmed == "/.doctor/"
    });
    if already {
        return Ok(());
    }

    // SACRED INVARIANT: if the operator has locked the repo-root
    // `.gitignore` read-only (compliance-controlled file, vendored
    // shared config), do NOT bypass that intent. The tmp+rename below
    // would otherwise replace the file even at mode 0o444, since rename
    // only needs parent-directory write permission. The
    // `permissions.root_gitignore` detector already surfaces the locked
    // file to the operator; here we skip the `.doctor/` addition rather
    // than silently overwriting it. Trade-off: `.doctor/` run artifacts
    // won't be auto-ignored on a locked repo, but respecting an explicit
    // operator chmod outranks the convenience of the carveout.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::symlink_metadata(&gitignore)
            && meta.file_type().is_file()
            && (meta.permissions().mode() & 0o200) == 0
        {
            tracing::warn!(
                path = %gitignore.display(),
                "doctor: repo-root .gitignore is not owner-writable; skipping .doctor/ ignore-rule addition (operator-locked, see permissions.root_gitignore)"
            );
            return Ok(());
        }
    }

    let mut new_contents = existing;
    if !new_contents.is_empty() && !new_contents.ends_with('\n') {
        new_contents.push('\n');
    }
    new_contents.push_str("# br doctor per-run artifacts\n");
    new_contents.push_str(needle);
    new_contents.push('\n');

    // Atomic write: tmp + rename.
    let parent = gitignore.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(BeadsError::Io)?;
    tmp.write_all(new_contents.as_bytes())
        .map_err(BeadsError::Io)?;
    tmp.as_file().sync_data().map_err(BeadsError::Io)?;
    tmp.persist(&gitignore)
        .map_err(|e| BeadsError::Io(e.error))?;
    fsync_dir(parent)?;
    Ok(())
}

fn fsync_dir(dir: &Path) -> Result<(), BeadsError> {
    let file = fs::File::open(dir).map_err(BeadsError::Io)?;
    match file.sync_all() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::InvalidInput => Ok(()),
        Err(e) => Err(BeadsError::Io(e)),
    }
}

/// Write `<run-dir>/undo.sh` — a pure-bash fallback that reads
/// `actions.jsonl` in reverse and restores files from `backups/`.
///
/// The script is intentionally stand-alone (depends only on bash, jq,
/// cp, mv) so it is recoverable even when the `br` binary itself is
/// broken.
///
/// # Errors
///
/// Returns [`BeadsError::Io`] for write/permission failures.
pub fn write_undo_sh(run: &RunDir) -> Result<(), BeadsError> {
    let repo_root_assignment = undo_repo_root_assignment(run);
    let script = format!(
        r#"#!/usr/bin/env bash
# br doctor undo — pure-bash fallback for run {run_id}
#
# Replays {{actions_jsonl}} in reverse, restoring the verbatim backups
# under {{backups_dir}}. Requires: bash, jq, cp, mv.
#
# This script is generated by br; do NOT hand-edit unless the live br
# binary is broken.
set -euo pipefail

run_dir="$(cd "$(dirname "$0")" && pwd)"
actions="${{run_dir}}/actions.jsonl"
backups="${{run_dir}}/backups"
{repo_root_assignment}

if [[ ! -s "${{actions}}" ]]; then
  echo "no actions.jsonl entries — nothing to undo" >&2
  exit 0
fi

# Reverse the actions and replay each one.
tac "${{actions}}" | while read -r line; do
  op=$(jq -r '.op' <<<"${{line}}")
  rel=$(jq -r '.path' <<<"${{line}}")
  rename_to=$(jq -r '.rename_to // empty' <<<"${{line}}")
  case "${{op}}" in
    write_file|append_file|chmod|symlink_atomic)
      # Restore from backup if one exists.
      backup="${{backups}}/${{rel}}"
      target="${{repo_root}}/${{rel}}"
      if [[ -e "${{backup}}" ]]; then
        mkdir -p "$(dirname "${{target}}")"
        cp -p "${{backup}}" "${{target}}"
      fi
      ;;
    rename)
      # Rename op moved <rel> -> <rename_to>; reverse it.
      rename_source="${{rename_to}}"
      if [[ -n "${{rename_source}}" && "${{rename_source}}" != /* ]]; then
        rename_source="${{repo_root}}/${{rename_source}}"
      fi
      if [[ -n "${{rename_source}}" && -e "${{rename_source}}" ]]; then
        mkdir -p "$(dirname "${{repo_root}}/${{rel}}")"
        mv "${{rename_source}}" "${{repo_root}}/${{rel}}"
      fi
      ;;
    db_exec|db_migrate)
      echo "[warn] cannot undo ${{op}} from bash — re-run br doctor undo" >&2
      ;;
    *)
      echo "[warn] unknown op ${{op}}; skipping" >&2
      ;;
  esac
done

echo "undo complete for run {run_id}" >&2
"#,
        run_id = run.run_id,
        repo_root_assignment = repo_root_assignment,
    );

    fs::write(&run.undo_script, script).map_err(BeadsError::Io)?;
    make_executable(&run.undo_script).map_err(BeadsError::Io)?;
    Ok(())
}

fn undo_repo_root_assignment(run: &RunDir) -> String {
    let default_runs_root = run.repo_root.join(".doctor").join("runs");
    if run.root.starts_with(default_runs_root) {
        return r#"repo_root="$(cd "${run_dir}/../../.." && pwd)""#.to_string();
    }

    format!("repo_root={}", shell_single_quote_path(&run.repo_root))
}

fn shell_single_quote_path(path: &Path) -> String {
    shell_single_quote(path.to_string_lossy().as_ref())
}

fn shell_single_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;

    fn unique_temp_root(label: &str) -> tempfile::TempDir {
        let prefix = format!("br-doctor-rundir-{label}-");
        tempfile::Builder::new()
            .prefix(prefix.as_str())
            .tempdir()
            .expect("tempdir")
    }

    #[test]
    fn create_run_dir_produces_stable_run_id_format() {
        let tmp = unique_temp_root("stable");
        let run = create_run_dir(tmp.path()).expect("create_run_dir");

        // Format: <YYYYMMDDTHHMMSSZ>__<6 hex chars>
        let parts: Vec<&str> = run.run_id.split("__").collect();
        assert_eq!(parts.len(), 2, "run_id must split into ts__hash");
        assert_eq!(parts[0].len(), 16, "iso ts must be 16 chars");
        assert_eq!(parts[1].len(), 6, "short hash must be 6 hex");
        assert!(parts[1].chars().all(|c| c.is_ascii_hexdigit()));

        // Directory layout exists.
        assert!(run.root.is_dir(), "root dir missing");
        assert!(run.backups.is_dir(), "backups dir missing");
        assert!(run.actions_file.is_file(), "actions.jsonl missing");
        assert!(run.report_file.is_file(), "report.json placeholder missing");

        // Symlink atomically updated. Note: the only env-controlled
        // path lives behind `runs_root_for`, which falls back to
        // `<repo>/.doctor/runs/`. If `BR_DOCTOR_RUNS_DIR` happens to be
        // set in the calling shell, the latest_link path will be under
        // that override, so we only assert that the symlink resolves
        // to a target containing run_id.
        let meta = fs::symlink_metadata(&run.latest_link).expect("latest");
        assert!(meta.file_type().is_symlink(), "latest must be symlink");
        let target = fs::read_link(&run.latest_link).unwrap();
        assert!(
            target.to_string_lossy().contains(&run.run_id),
            "symlink target {} must contain run_id {}",
            target.display(),
            run.run_id
        );

        // .gitignore now contains .doctor/.
        let gi = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(gi.contains(".doctor/"));
    }

    #[test]
    fn create_repair_run_dir_returns_initial_actions_handle() {
        let tmp = unique_temp_root("initial-actions-handle");
        let (run, mut actions_file) = create_repair_run_dir(tmp.path()).expect("create repair run");

        writeln!(actions_file, "{{\"op\":\"test\"}}").expect("append audit line");
        actions_file.sync_data().expect("sync audit line");

        assert_eq!(
            fs::read_to_string(&run.actions_file).expect("read actions"),
            "{\"op\":\"test\"}\n"
        );
    }

    #[test]
    fn second_run_replaces_latest_atomically() {
        let tmp = unique_temp_root("atomic");

        let run1 = create_run_dir(tmp.path()).expect("first run");
        // Sleep just over one second so the second run's iso-second
        // timestamp differs.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let run2 = create_run_dir(tmp.path()).expect("second run");
        assert_ne!(run1.run_id, run2.run_id);

        let target = fs::read_link(&run2.latest_link).unwrap();
        assert!(target.to_string_lossy().contains(&run2.run_id));
        // The first run's directory still exists — we don't delete it.
        assert!(run1.root.is_dir());
    }

    #[test]
    fn generated_run_ids_are_unique_inside_one_process_second() {
        let tmp = unique_temp_root("same-second-ids");
        let mut seen = HashSet::new();
        for _ in 0..8 {
            let run_id = generate_run_id(tmp.path());
            assert!(seen.insert(run_id), "run_id collision inside one process");
        }
    }

    #[test]
    fn fsync_dir_accepts_existing_directory() {
        let tmp = unique_temp_root("fsync-dir");
        fsync_dir(tmp.path()).expect("fsync temp dir");
    }

    #[test]
    fn windows_symlink_fallback_matches_only_privilege_error() {
        let privilege_error = std::io::Error::from_raw_os_error(1314);
        let ordinary_access_denied = std::io::Error::from_raw_os_error(5);

        assert!(is_windows_symlink_privilege_error(&privilege_error));
        assert!(!is_windows_symlink_privilege_error(&ordinary_access_denied));
    }

    #[test]
    fn write_undo_sh_emits_executable_script() {
        let tmp = unique_temp_root("undo");

        let run = create_run_dir(tmp.path()).expect("create run");
        write_undo_sh(&run).expect("write undo");
        assert!(run.undo_script.is_file());
        let meta = fs::metadata(&run.undo_script).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o755);
        let body = fs::read_to_string(&run.undo_script).unwrap();
        assert!(body.starts_with("#!/usr/bin/env bash"));
        assert!(body.contains(&run.run_id));
        assert!(
            body.contains(r#"rename_source="${rename_to}""#),
            "rename undo must normalize the recorded destination before mv"
        );
        assert!(
            body.contains(r#"rename_source="${repo_root}/${rename_source}""#),
            "relative rename destinations must be resolved from repo_root"
        );
    }

    #[test]
    fn write_undo_sh_keeps_relative_repo_root_for_default_layout() {
        let tmp = unique_temp_root("undo-default-root");

        let run = create_run_dir(tmp.path()).expect("create run");
        write_undo_sh(&run).expect("write undo");

        let body = fs::read_to_string(&run.undo_script).unwrap();
        assert!(
            body.contains(r#"repo_root="$(cd "${run_dir}/../../.." && pwd)""#),
            "default in-repo run dirs should keep the move-tolerant repo_root derivation"
        );
    }

    #[test]
    fn write_undo_sh_embeds_repo_root_for_redirected_run_dir() {
        let repo = unique_temp_root("undo-repo-root");
        let redirected = unique_temp_root("undo-redirected");
        let run_root = redirected.path().join("runs").join("run-one");
        fs::create_dir_all(&run_root).expect("create redirected run root");
        let run = RunDir {
            run_id: "run-one".to_string(),
            repo_root: repo.path().to_path_buf(),
            root: run_root.clone(),
            backups: run_root.join("backups"),
            actions_file: run_root.join("actions.jsonl"),
            report_file: run_root.join("report.json"),
            undo_script: run_root.join("undo.sh"),
            latest_link: redirected.path().join("latest"),
        };

        write_undo_sh(&run).expect("write undo");

        let body = fs::read_to_string(&run.undo_script).unwrap();
        assert!(
            body.contains(&format!(
                "repo_root={}",
                shell_single_quote_path(repo.path())
            )),
            "redirected run dirs must restore against the original workspace root"
        );
        assert!(
            !body.contains(r#"repo_root="$(cd "${run_dir}/../../.." && pwd)""#),
            "redirected run dirs cannot infer repo_root from the artifact path"
        );
    }

    /// Verifies the env-override path without mutating process env
    /// (the crate forbids `unsafe`, so `std::env::set_var` is
    /// unavailable; we drive the inner pure helper directly).
    #[test]
    fn runs_root_with_override_redirects_runs_root() {
        let outer = unique_temp_root("envouter");
        let override_dir = unique_temp_root("envoverride");
        let computed =
            runs_root_with_override(outer.path(), Some(override_dir.path().to_path_buf()));
        assert!(computed.starts_with(override_dir.path()));
        assert!(computed.ends_with("runs"));

        // And without an override, falls back to <repo>/.doctor/runs.
        let fallback = runs_root_with_override(outer.path(), None);
        assert_eq!(fallback, outer.path().join(".doctor").join("runs"));
    }

    /// Round-5 fresh-eyes follow-through (`beads_rust-dfjs`):
    /// `ensure_doctor_in_gitignore` is the SOLE pre-chokepoint write
    /// in the doctor pipeline. If `.doctor/` is already in
    /// `.gitignore`, the function must be a no-op — idempotent,
    /// re-entrant, and never producing a write that the chokepoint's
    /// audit trail won't see. Locks the carveout in regression-test
    /// form so that any future drift (e.g., adding a second
    /// pre-chokepoint write, or making this one non-idempotent) is
    /// caught by `cargo test --lib`.
    #[test]
    fn ensure_doctor_in_gitignore_is_noop_when_already_present() {
        let tmp = unique_temp_root("noop-gitignore");
        let gitignore = tmp.path().join(".gitignore");
        let initial = "node_modules\n.doctor/\nbuild/\n";
        fs::write(&gitignore, initial).expect("seed gitignore");
        let pre_meta = fs::metadata(&gitignore).expect("pre meta");
        let pre_mtime = pre_meta.modified().expect("pre mtime");

        ensure_doctor_in_gitignore(tmp.path()).expect("ensure");

        let post_bytes = fs::read_to_string(&gitignore).expect("read post");
        assert_eq!(
            post_bytes, initial,
            "idempotent path must not rewrite the file"
        );
        let post_meta = fs::metadata(&gitignore).expect("post meta");
        assert_eq!(
            post_meta.modified().expect("post mtime"),
            pre_mtime,
            "idempotent path must not even touch the inode mtime"
        );
    }

    /// Fresh-eyes follow-up on `beads_rust-dfjs`: if `.gitignore`
    /// cannot be read/written as a regular file, the doctor must not
    /// pretend run-dir creation succeeded. Otherwise the public
    /// success contract ("`.gitignore` contains `.doctor/`") is false,
    /// and `--repair` can proceed with unignored `.doctor/runs/*`
    /// artifacts.
    #[test]
    fn ensure_doctor_in_gitignore_rejects_non_file_gitignore() {
        let tmp = unique_temp_root("bad-gitignore");
        fs::create_dir(tmp.path().join(".gitignore")).expect("directory at .gitignore path");

        let err = ensure_doctor_in_gitignore(tmp.path()).expect_err("directory is not a gitignore");
        assert!(
            err.to_string().contains(".gitignore") || err.to_string().contains("directory"),
            "error should name the invalid gitignore surface: {err}"
        );
    }

    /// SACRED INVARIANT regression: a repo-root `.gitignore` that the
    /// operator has chmod'd read-only (0o444) must NOT be replaced by
    /// the `.doctor/` carveout. The tmp+rename write only needs
    /// parent-dir write permission, so without an explicit guard it
    /// would bypass the operator's lock. We assert the file is left
    /// byte- and mode-identical and that `ensure_doctor_in_gitignore`
    /// still reports success (best-effort skip, not a hard failure).
    #[cfg(unix)]
    #[test]
    fn ensure_doctor_in_gitignore_skips_readonly_gitignore() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = unique_temp_root("readonly-gitignore");
        let gitignore = tmp.path().join(".gitignore");
        let initial = "node_modules\nbuild/\n";
        fs::write(&gitignore, initial).expect("seed gitignore");
        fs::set_permissions(&gitignore, fs::Permissions::from_mode(0o444))
            .expect("lock gitignore read-only");

        ensure_doctor_in_gitignore(tmp.path())
            .expect("locked gitignore must be a best-effort skip, not an error");

        let post = fs::read_to_string(&gitignore).expect("read post");
        assert_eq!(
            post, initial,
            "read-only .gitignore must not be modified by the .doctor/ carveout"
        );
        let mode = fs::metadata(&gitignore)
            .expect("post meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o444, "operator's read-only mode must be preserved");
        assert!(
            !post.contains(".doctor"),
            ".doctor/ must NOT have been appended to the locked file"
        );
    }

    /// Fresh-eyes follow-up on `df923516`: propagating the
    /// `.gitignore` error is necessary but not sufficient. The
    /// pre-chokepoint write must happen before any `.doctor/` artifact
    /// is created; otherwise a failed run-dir setup leaves exactly the
    /// unignored skeleton it was trying to avoid.
    #[test]
    fn create_run_dir_fails_before_artifacts_when_gitignore_invalid() {
        let tmp = unique_temp_root("bad-gitignore-order");
        fs::create_dir(tmp.path().join(".gitignore")).expect("directory at .gitignore path");

        let err = create_run_dir(tmp.path()).expect_err("invalid gitignore must fail run setup");
        assert!(
            err.to_string().contains(".gitignore") || err.to_string().contains("directory"),
            "error should name the invalid gitignore surface: {err}"
        );
        assert!(
            !tmp.path().join(".doctor").exists(),
            "failed run-dir setup must not leave unignored .doctor artifacts"
        );
    }

    /// Round-5 fresh-eyes follow-through (`beads_rust-dfjs`): when
    /// `BR_DOCTOR_RUNS_DIR` redirects the runs directory, the
    /// gitignore touch must be skipped so that test fixtures, CI
    /// sandboxes, and `br doctor undo` (which builds a fresh run-dir
    /// purely to audit its own writes) cannot mutate the host repo's
    /// `.gitignore` outside the chokepoint. We exercise the redirect
    /// the same way `runs_root_with_override` does — by driving the
    /// pure helpers directly — because `#![forbid(unsafe_code)]`
    /// prevents us from setting process-wide env in tests.
    ///
    /// (We assert the documented contract by inspection: the only
    /// place `ensure_doctor_in_gitignore` is *called* from production
    /// code is `create_run_dir`, and that call site is gated on
    /// `std::env::var_os(ENV_RUNS_DIR).is_none()`. Adding any second
    /// caller would surface as a search hit on this test's assertion
    /// message and require a contract update.)
    #[test]
    fn create_run_dir_call_to_gitignore_is_gated_on_env_override() {
        // Self-document the carveout the production code relies on.
        // If the gating disappears, this string-search regression
        // catches it.
        let src = include_str!("run_dir.rs");
        let production_section = src
            .split("\nmod tests {")
            .next()
            .expect("run_dir.rs must have a non-test section");
        assert!(
            production_section.contains("if std::env::var_os(ENV_RUNS_DIR).is_none() {"),
            "create_run_dir's gitignore touch must be gated on \
             BR_DOCTOR_RUNS_DIR being unset; if you removed that gate, \
             update the chokepoint carveout doc on \
             ensure_doctor_in_gitignore and rewrite this test."
        );
        assert!(
            production_section.contains("ensure_doctor_in_gitignore(repo_root)?;"),
            "create_run_dir must propagate gitignore update failures; \
             otherwise its success contract can lie about `.doctor/` \
             being ignored."
        );
        assert_eq!(
            production_section
                .matches("ensure_doctor_in_gitignore(")
                .count(),
            2,
            "ensure_doctor_in_gitignore must have exactly two call sites in \
             production code: its own definition and the gated call from \
             create_run_dir. A third call is the contract violation \
             beads_rust-dfjs warned about."
        );
    }
}
