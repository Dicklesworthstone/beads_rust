//! Build script for `beads_rust`.
//!
//! Uses vergen-gix for stable build/rustc metadata and quiet git probes for
//! optional repository metadata.

use std::{env, path::Path, process::Command};
use vergen_gix::{Build, Cargo, Emitter, Rustc};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let build = Build::builder().build_timestamp(true).build();
    let cargo = Cargo::builder().target_triple(true).build();
    let rustc = Rustc::builder().semver(true).build();

    let mut emitter = Emitter::default();
    emitter
        .add_instructions(&build)?
        .add_instructions(&cargo)?
        .add_instructions(&rustc)?;

    emitter.emit()?;
    emit_git_metadata();
    emit_engine_version();

    Ok(())
}

fn emit_git_metadata() {
    if git_output(&["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
        && let Some(sha) = git_output(&["rev-parse", "HEAD"])
    {
        emit_git_rerun_triggers();
        emit_env("VERGEN_GIT_SHA", &sha);

        if let Some(branch) = git_output(&["rev-parse", "--abbrev-ref", "HEAD"]) {
            emit_env("VERGEN_GIT_BRANCH", &branch);
        }

        if let Some(timestamp) = git_output(&["log", "-1", "--format=%cI"]) {
            emit_env("VERGEN_GIT_COMMIT_TIMESTAMP", &timestamp);
        }

        if let Some(status) = git_output(&["status", "--porcelain"]) {
            emit_env(
                "VERGEN_GIT_DIRTY",
                if status.is_empty() { "false" } else { "true" },
            );
        }
        return;
    }

    if let Some(sha) = first_env(&[
        "VERGEN_GIT_SHA",
        "RCH_SOURCE_COMMIT",
        "RCH_GIT_SHA",
        "RCH_GIT_COMMIT",
        "GIT_COMMIT",
        "GITHUB_SHA",
        "CI_COMMIT_SHA",
        "BUILDKITE_COMMIT",
        "DRONE_COMMIT_SHA",
        "VERCEL_GIT_COMMIT_SHA",
    ]) {
        emit_env("VERGEN_GIT_SHA", &sha);
    }

    if let Some(branch) = first_env(&["VERGEN_GIT_BRANCH", "GITHUB_REF_NAME", "CI_COMMIT_REF_NAME"])
    {
        emit_env("VERGEN_GIT_BRANCH", &branch);
    }
}

/// Tell Cargo which git files the stamped metadata depends on (GH #514).
///
/// vergen emits `rerun-if-changed=build.rs`, which switches off Cargo's
/// default "rerun on any package file change", so without these triggers the
/// build script never reruns after a commit or pull and `br version` keeps
/// reporting the commit of an earlier build. Paths come from
/// `git rev-parse --git-path`, which resolves linked worktrees (per-worktree
/// `HEAD`/`index`, shared refs) correctly.
///
/// - `HEAD`: branch switches and detached-HEAD commits.
/// - the ref `HEAD` points at (e.g. `refs/heads/main`): new commits, pulls.
/// - `packed-refs`: refs that were packed by `git gc` / `git pack-refs`.
/// - `index`: staging and commits, which move the dirty flag.
/// - `reftable/`: ref storage for repositories using the reftable backend.
///
/// Edits to tracked files that are not yet staged do not touch any of these,
/// so the dirty flag stays best-effort for those; SHA, branch and commit
/// timestamp are always refreshed.
fn emit_git_rerun_triggers() {
    for name in ["HEAD", "packed-refs", "index", "reftable"] {
        watch_git_path(name);
    }
    if let Some(head_ref) = git_output(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git_output(&["rev-parse", "--git-path", &head_ref])
    {
        let path = Path::new(&path);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        } else if let Some(parent) = path.parent().filter(|parent| parent.is_dir()) {
            // The branch currently lives only in `packed-refs`; the next
            // commit recreates the loose ref file inside this directory.
            println!("cargo:rerun-if-changed={}", parent.display());
        }
    }
}

/// Emit `rerun-if-changed` for a git-internal path, but only when it exists:
/// Cargo treats a missing path as always stale, which would rerun the build
/// script (and recompile the crate) on every build.
fn watch_git_path(name: &str) {
    if let Some(path) = git_output(&["rev-parse", "--git-path", name])
        && Path::new(&path).exists()
    {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn emit_env(key: &str, value: &str) {
    println!("cargo:rustc-env={key}={value}");
}

/// Expose the locked `fsqlite` version as `BR_FSQLITE_VERSION` so `br info`
/// and `br doctor` can name the engine they were built against.
fn emit_engine_version() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let Ok(lock) = std::fs::read_to_string("Cargo.lock") else {
        return;
    };
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "name = \"fsqlite\""
            && let Some(version_line) = lines.next()
            && let Some(version) = version_line
                .trim()
                .strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        {
            emit_env("BR_FSQLITE_VERSION", version);
            return;
        }
    }
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;

    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let trimmed = value.trim();

    Some(trimmed.to_string())
}

fn first_env(names: &[&str]) -> Option<String> {
    for name in names {
        println!("cargo:rerun-if-env-changed={name}");
        if let Ok(value) = env::var(name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    None
}
