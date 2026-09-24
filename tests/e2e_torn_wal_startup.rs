//! A WAL sidecar shorter than its 32-byte header (a crash before the header
//! was fully written) holds no frames under SQLite's recovery rule. It must
//! never lock the workspace: ordinary commands keep working, the torn bytes
//! are retained under `.br_recovery/`, and nothing committed is lost. Empty
//! and complete header-only WALs are healthy and must be left alone.
//!
//! Separately, a WAL that automatic index recovery refuses (here: a complete
//! but invalid header) must not make every retried command copy the whole
//! database family into a fresh `.br_recovery` run.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const KEPT_TITLE: &str = "committed before the torn WAL";

fn wal_path(workspace: &BrWorkspace) -> PathBuf {
    workspace.root.join(".beads/beads.db-wal")
}

fn shm_path(workspace: &BrWorkspace) -> PathBuf {
    workspace.root.join(".beads/beads.db-shm")
}

fn recovery_dir(workspace: &BrWorkspace) -> PathBuf {
    workspace.root.join(".beads/.br_recovery")
}

/// Every regular file under `.br_recovery`, recursively, as relative paths.
fn recovery_files(workspace: &BrWorkspace) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("recovery entry").path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push(path.strip_prefix(root).expect("relative").to_path_buf());
            }
        }
    }
    let root = recovery_dir(workspace);
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out.sort();
    out
}

fn initialized_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "tw"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create = run_br(&workspace, ["create", KEPT_TITLE], "create_kept");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    workspace
}

fn listed_titles(workspace: &BrWorkspace, label: &str) -> Vec<String> {
    let list = run_br(workspace, ["list", "--all", "--json"], label);
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

fn remove_shm(workspace: &BrWorkspace) {
    match fs::remove_file(shm_path(workspace)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove fixture SHM: {error}"),
    }
}

#[test]
fn torn_wal_never_locks_the_workspace_and_is_retained() {
    for length in [1_usize, 31] {
        for keep_shm in [false, true] {
            let label = format!("{length}b_shm_{keep_shm}");
            let workspace = initialized_workspace();
            let torn: Vec<u8> = (0..length)
                .map(|index| u8::try_from(index % 251).unwrap() ^ 0x5A)
                .collect();
            fs::write(wal_path(&workspace), &torn).expect("plant torn WAL");
            if !keep_shm {
                remove_shm(&workspace);
            }

            let list = run_br(
                &workspace,
                ["list", "--all", "--json"],
                &format!("{label}_list"),
            );
            assert!(
                list.status.success(),
                "{label}: a torn WAL must not lock reads: stdout={} stderr={}",
                list.stdout,
                list.stderr
            );
            assert!(
                list.stdout.contains(KEPT_TITLE),
                "{label}: committed issue missing: {}",
                list.stdout
            );
            assert!(
                list.stderr.contains("torn") && list.stderr.contains(&format!("{length}-byte")),
                "{label}: quarantine must be reported: {}",
                list.stderr
            );

            let retained: Vec<PathBuf> = recovery_files(&workspace)
                .into_iter()
                .filter(|path| {
                    let name = path.to_string_lossy();
                    name.contains("beads.db-wal") && name.ends_with("truncated-wal")
                })
                .collect();
            assert_eq!(retained.len(), 1, "{label}: {retained:?}");
            assert_eq!(
                fs::read(recovery_dir(&workspace).join(&retained[0])).unwrap(),
                torn,
                "{label}: the torn bytes must be retained verbatim"
            );
            assert!(
                fs::metadata(wal_path(&workspace))
                    .map_or(true, |meta| meta.len() == 0 || meta.len() >= 32),
                "{label}: the live family must no longer carry a torn WAL"
            );

            let create = run_br(
                &workspace,
                ["create", "written after quarantine"],
                &format!("{label}_create"),
            );
            assert!(
                create.status.success(),
                "{label}: writes must work: {} {}",
                create.stdout,
                create.stderr
            );
            let titles = listed_titles(&workspace, &format!("{label}_relist"));
            assert!(titles.iter().any(|title| title == KEPT_TITLE), "{titles:?}");
            assert!(
                titles
                    .iter()
                    .any(|title| title == "written after quarantine"),
                "{titles:?}"
            );
            let before_repeat = recovery_files(&workspace);
            let _ = listed_titles(&workspace, &format!("{label}_repeat"));
            assert_eq!(
                recovery_files(&workspace),
                before_repeat,
                "{label}: a healthy family must not add recovery artifacts"
            );
        }
    }
}

#[test]
fn torn_wal_is_repairable_through_doctor_without_prior_commands() {
    let workspace = initialized_workspace();
    fs::write(wal_path(&workspace), b"synthetic orphan wal").expect("plant torn WAL");
    remove_shm(&workspace);

    let repair = run_br(
        &workspace,
        ["doctor", "--repair", "--json"],
        "doctor_repair",
    );
    assert!(
        repair.status.success(),
        "doctor --repair must handle a torn WAL: stdout={} stderr={}",
        repair.stdout,
        repair.stderr
    );
    let titles = listed_titles(&workspace, "after_doctor_repair");
    assert!(titles.iter().any(|title| title == KEPT_TITLE), "{titles:?}");
}

#[test]
fn empty_and_header_only_wals_are_left_alone() {
    // An empty WAL is the normal post-truncate state.
    for keep_shm in [false, true] {
        let label = format!("empty_shm_{keep_shm}");
        let workspace = initialized_workspace();
        fs::write(wal_path(&workspace), b"").expect("empty WAL");
        if !keep_shm {
            remove_shm(&workspace);
        }
        let titles = listed_titles(&workspace, &format!("{label}_list"));
        assert!(titles.iter().any(|title| title == KEPT_TITLE), "{titles:?}");
        assert!(
            !recovery_files(&workspace)
                .iter()
                .any(|path| path.to_string_lossy().contains("truncated-wal")),
            "{label}: an empty WAL is not torn"
        );
    }

    // The engine leaves a complete, frame-free 32-byte header after a
    // committing command followed by its close-time checkpoint.
    let workspace = initialized_workspace();
    let header = fs::read(wal_path(&workspace)).expect("live WAL");
    assert_eq!(
        header.len(),
        32,
        "fixture precondition: the settled WAL is exactly one header"
    );
    let titles = listed_titles(&workspace, "header_only_list");
    assert!(titles.iter().any(|title| title == KEPT_TITLE), "{titles:?}");
    assert!(
        !recovery_files(&workspace)
            .iter()
            .any(|path| path.to_string_lossy().contains("truncated-wal")),
        "a complete 32-byte header is not torn"
    );
}

#[test]
fn refused_automatic_recovery_keeps_one_snapshot_per_incident() {
    let workspace = initialized_workspace();
    // A complete-length header with invalid magic: automatic index recovery
    // refuses it (fail-closed), which is the case that used to copy the whole
    // family into a new `.br_recovery` run on every retried command.
    fs::write(wal_path(&workspace), [0xEE_u8; 32]).expect("plant invalid WAL");
    remove_shm(&workspace);

    let runs_root = recovery_dir(&workspace).join("schema-migrations");
    let run_count =
        || fs::read_dir(&runs_root).map_or(0, |entries| entries.filter_map(Result::ok).count());

    let first = run_br(&workspace, ["list", "--all", "--json"], "first_refusal");
    assert!(!first.status.success(), "fixture must be refused");
    assert_eq!(
        run_count(),
        1,
        "the first refusal retains one pre-state copy"
    );
    let files_after_first = recovery_files(&workspace);

    for attempt in 0..3 {
        let again = run_br(
            &workspace,
            ["create", "must not be written"],
            &format!("repeat_{attempt}"),
        );
        assert!(!again.status.success());
        let output = format!("{}{}", again.stdout, again.stderr);
        assert!(
            output.contains("already failed for this exact database family"),
            "repeat {attempt} must name the retained incident: {output}"
        );
    }
    assert_eq!(run_count(), 1, "repeated refusals must not add runs");
    assert_eq!(
        recovery_files(&workspace),
        files_after_first,
        "repeated refusals must not add recovery artifacts"
    );
}
