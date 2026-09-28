//! GH #521: upgrading br 0.6.0 to 0.7.0 wedged every workspace with
//! "database is busy (recovery in progress)".
//!
//! br 0.6.0's engine kept its WAL index in process memory and never wrote the
//! `-shm` header, so a settled 0.6.0 family is a header-only WAL beside a
//! 32 KiB `-shm` whose header is all zeroes. A newer engine reads that file,
//! finds an index it must rebuild, and a read-only open cannot rebuild it.
//! Running 0.6.0 again after a newer br leaves the same kind of index: one
//! whose salts describe a WAL generation 0.6.0 has since restarted.
//!
//! Ordinary commands must rebuild such an index (main and WAL untouched,
//! pre-state retained) and keep working; explicit read-only invocations must
//! read without writing the live family.

// Automatic index recovery runs only where the engine maps `-shm` and the
// quarantine lock exists (STARTUP_WAL_INDEX_RECOVERY).
#![cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

const KEPT_TITLE: &str = "created before the upgrade";

fn beads_file(workspace: &BrWorkspace, name: &str) -> PathBuf {
    workspace.root.join(".beads").join(name)
}

fn migration_run_count(workspace: &BrWorkspace) -> usize {
    fs::read_dir(beads_file(workspace, ".br_recovery/schema-migrations"))
        .map_or(0, |entries| entries.filter_map(Result::ok).count())
}

fn settled_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "lw"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create = run_br(&workspace, ["create", KEPT_TITLE], "create_kept");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let wal = fs::read(beads_file(&workspace, "beads.db-wal")).expect("settled WAL");
    assert_eq!(wal.len(), 32, "fixture precondition: header-only WAL");
    workspace
}

/// The exact `-shm` a settled br 0.6.0 family carries: 32 KiB, a WAL-index
/// header that was never written, read marks 2..4 unused.
fn install_legacy_index(workspace: &BrWorkspace) {
    let mut legacy = vec![0_u8; 32 * 1024];
    legacy[108..120].fill(0xFF);
    fs::write(beads_file(workspace, "beads.db-shm"), legacy).expect("legacy -shm");
}

/// An initialized index whose salts belong to an earlier WAL generation.
fn install_other_generation_index(workspace: &BrWorkspace) {
    let path = beads_file(workspace, "beads.db-shm");
    let mut shm = fs::read(&path).expect("live -shm");
    assert_eq!(shm[12], 1, "fixture precondition: an initialized index");
    shm[32] ^= 0x01;
    shm[80] ^= 0x01;
    fs::write(&path, shm).expect("rewrite -shm");
}

fn listed_titles(workspace: &BrWorkspace, args: &[&str], label: &str) -> Vec<String> {
    let list = run_br(workspace, args.iter().copied(), label);
    assert!(
        list.status.success(),
        "{label}: list must succeed: stdout={} stderr={}",
        list.stdout,
        list.stderr
    );
    let json: Value = serde_json::from_str(&extract_json_payload(&list.stdout)).expect("list JSON");
    json["issues"]
        .as_array()
        .expect("issues array")
        .iter()
        .map(|issue| issue["title"].as_str().expect("title").to_owned())
        .collect()
}

fn assert_recovers(workspace: &BrWorkspace, label: &str) {
    let main_before = fs::read(beads_file(workspace, "beads.db")).expect("main");
    let wal_before = fs::read(beads_file(workspace, "beads.db-wal")).expect("wal");

    let ready = run_br(workspace, ["ready"], &format!("{label}_ready"));
    assert!(
        ready.status.success(),
        "{label}: ready must succeed after the upgrade: stdout={} stderr={}",
        ready.stdout,
        ready.stderr
    );
    assert!(
        !format!("{}{}", ready.stdout, ready.stderr).contains("recovery in progress"),
        "{label}: {}",
        ready.stderr
    );
    assert!(
        ready.stdout.contains(KEPT_TITLE),
        "{label}: {}",
        ready.stdout
    );
    assert_eq!(
        migration_run_count(workspace),
        1,
        "{label}: exactly one retained pre-recovery family"
    );
    let retained = fs::read_dir(beads_file(workspace, ".br_recovery/schema-migrations"))
        .expect("runs")
        .next()
        .expect("one run")
        .expect("run entry")
        .path()
        .join("recovery-before");
    assert_eq!(
        fs::read(retained.join("beads.db")).expect("retained main"),
        main_before,
        "{label}: the pre-state is kept byte-for-byte"
    );
    assert_eq!(
        fs::read(retained.join("beads.db-wal")).expect("retained WAL"),
        wal_before,
        "{label}: the pre-state WAL is kept byte-for-byte"
    );

    // Rebuilt once: later commands neither fail nor copy the family again.
    let create = run_br(
        workspace,
        ["create", "written after the upgrade"],
        &format!("{label}_create"),
    );
    assert!(create.status.success(), "{label}: {}", create.stderr);
    let titles = listed_titles(workspace, &["list", "--all", "--json"], label);
    assert!(titles.iter().any(|title| title == KEPT_TITLE), "{titles:?}");
    assert!(
        titles
            .iter()
            .any(|title| title == "written after the upgrade"),
        "{titles:?}"
    );
    assert_eq!(
        migration_run_count(workspace),
        1,
        "{label}: a rebuilt index must not trigger recovery again"
    );
}

#[test]
fn legacy_index_from_br_060_is_rebuilt_by_ordinary_commands() {
    let workspace = settled_workspace();
    install_legacy_index(&workspace);
    assert_recovers(&workspace, "legacy_060");
}

#[test]
fn index_from_another_wal_generation_is_rebuilt_by_ordinary_commands() {
    let workspace = settled_workspace();
    install_other_generation_index(&workspace);
    assert_recovers(&workspace, "other_generation");
}

#[test]
fn explicit_read_only_reads_the_legacy_family_without_writing_it() {
    let workspace = settled_workspace();
    install_legacy_index(&workspace);
    let family = ["beads.db", "beads.db-wal", "beads.db-shm"]
        .map(|name| fs::read(beads_file(&workspace, name)).expect("family member"));

    let titles = listed_titles(
        &workspace,
        &[
            "--no-auto-import",
            "--no-auto-flush",
            "list",
            "--all",
            "--json",
        ],
        "read_only",
    );
    assert!(titles.iter().any(|title| title == KEPT_TITLE), "{titles:?}");
    assert_eq!(
        ["beads.db", "beads.db-wal", "beads.db-shm"]
            .map(|name| fs::read(beads_file(&workspace, name)).expect("family member")),
        family,
        "an explicit read-only command must not rewrite the live family"
    );
    assert_eq!(migration_run_count(&workspace), 0);
}
