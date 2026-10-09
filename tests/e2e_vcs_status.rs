//! End-to-end contract tests for the explicit, bounded `br vcs-status`
//! diagnostic. These tests intentionally live outside sync safety coverage:
//! VCS process authority is opt-in through this command or
//! `br doctor --git-runtime-files`.
//! Runtime-ignore tests also use Git as a fixture oracle for init/doctor;
//! ordinary init/doctor paths never invoke Git or change the index.

#![allow(clippy::too_many_lines)]

mod common;

use common::cli::{
    BrWorkspace, extract_json_payload, run_br, run_br_smoke_at_root_with_env, run_br_with_env,
};
use serde_json::Value;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const JSONL: &str = ".beads/issues.jsonl";

fn git(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args([
            "-c",
            "user.name=br-e2e",
            "-c",
            "user.email=br-e2e@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .env("HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // Repository setup must not inherit host-level global git
        // configuration (outer GIT_CONFIG_GLOBAL or XDG_CONFIG_HOME), which
        // could otherwise register content filters or line-ending transforms
        // that change what these fixtures check out.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("run git")
}

fn git_ok(root: &Path, args: &[&str]) {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(root: &Path, args: &[&str]) -> String {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Git plumbing output must be UTF-8")
        .trim()
        .to_string()
}

fn git_with_stdin_ok(root: &Path, args: &[&str], input: &str) {
    let mut child = Command::new("git")
        .args([
            "-c",
            "user.name=br-e2e",
            "-c",
            "user.email=br-e2e@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .env("HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .take()
        .expect("Git stdin")
        .write_all(input.as_bytes())
        .expect("write Git stdin");
    let output = child.wait_with_output().expect("wait for Git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn export_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    git_ok(&workspace.root, &["init", "--initial-branch=main"]);
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create = run_br(&workspace, ["create", "VCS status issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);
    workspace
}

fn tracked_workspace() -> BrWorkspace {
    let workspace = export_workspace();
    git_ok(&workspace.root, &["add", JSONL]);
    git_ok(&workspace.root, &["commit", "-m", "track JSONL export"]);
    workspace
}

fn head_then_untracked_export_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    git_ok(&workspace.root, &["init", "--initial-branch=main"]);
    std::fs::write(workspace.root.join("README.md"), "fixture\n").expect("README");
    git_ok(&workspace.root, &["add", "README.md"]);
    git_ok(&workspace.root, &["commit", "-m", "initial HEAD"]);
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create = run_br(&workspace, ["create", "VCS status issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);
    workspace
}

fn append_jsonl(workspace: &BrWorkspace, id: &str) {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(workspace.root.join(JSONL))
        .expect("open JSONL");
    writeln!(file, "{{\"id\":\"{id}\"}}").expect("append JSONL");
}

fn vcs_status_json(workspace: &BrWorkspace, label: &str) -> Value {
    let output = run_br(workspace, ["vcs-status", "--json"], label);
    assert!(
        output.status.success(),
        "vcs-status failed: {}",
        output.stderr
    );
    let status: Value =
        serde_json::from_str(&extract_json_payload(&output.stdout)).expect("vcs-status JSON");
    status
}

fn assert_common_contract(value: &Value, available: bool) {
    assert_eq!(value["schema"], "br.vcs-export-status.v2", "{value}");
    assert_eq!(value["requested"], true, "{value}");
    assert_eq!(value["available"], available, "{value}");
    assert_eq!(value["vcs"], "git", "{value}");
    assert_eq!(value["observation_atomic"], false, "{value}");
    assert_eq!(value["path_scope"], "workspace", "{value}");
    assert_eq!(value["path"], ".beads/issues.jsonl", "{value}");
    assert!(value["timeout_ms"].as_u64().is_some(), "{value}");
    assert!(value["duration_ms"].as_u64().is_some(), "{value}");
}

fn runtime_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    git_ok(&workspace.root, &["init", "--initial-branch=main"]);
    let metadata = workspace.root.join(".beads");
    std::fs::create_dir(&metadata).expect("runtime metadata directory");
    std::fs::write(
        metadata.join("metadata.json"),
        r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
    )
    .expect("tracker metadata");
    std::fs::write(
        metadata.join(".gitignore"),
        "*.db*\n.br-wal-index-*/\n.br_recovery/\n.br_history/\n.write.lock\n.write-waiters.lock/\n",
    )
    .expect("runtime ignore rules");
    // The audit must be useful precisely when opening the engine or importing
    // JSONL is unsafe. Neither fixture is valid input to those paths.
    std::fs::write(
        metadata.join("beads.db"),
        b"invalid SQLite database\0retained",
    )
    .expect("unopenable database");
    std::fs::write(
        metadata.join("issues.jsonl"),
        "<<<<<<< unresolved export\nnot JSON\n",
    )
    .expect("unparseable export");
    workspace
}

#[derive(Debug, PartialEq, Eq)]
enum RuntimeFixtureEntry {
    Directory,
    File(Vec<u8>),
    Symlink(std::path::PathBuf),
}

fn runtime_metadata_snapshot(
    workspace: &BrWorkspace,
) -> std::collections::BTreeMap<std::path::PathBuf, RuntimeFixtureEntry> {
    runtime_directory_snapshot(&workspace.root.join(".beads"))
}

fn runtime_directory_snapshot(
    root: &Path,
) -> std::collections::BTreeMap<std::path::PathBuf, RuntimeFixtureEntry> {
    let mut pending = vec![root.to_path_buf()];
    let mut snapshot = std::collections::BTreeMap::new();
    while let Some(path) = pending.pop() {
        let kind = std::fs::symlink_metadata(&path).expect("fixture metadata");
        let relative = path
            .strip_prefix(root)
            .expect("metadata-local fixture")
            .to_path_buf();
        if kind.is_dir() {
            snapshot.insert(relative, RuntimeFixtureEntry::Directory);
            for entry in std::fs::read_dir(&path).expect("fixture directory") {
                pending.push(entry.expect("fixture entry").path());
            }
        } else if kind.file_type().is_symlink() {
            snapshot.insert(
                relative,
                RuntimeFixtureEntry::Symlink(std::fs::read_link(&path).expect("fixture link")),
            );
        } else {
            assert!(
                kind.is_file(),
                "unexpected fixture type: {}",
                path.display()
            );
            snapshot.insert(
                relative,
                RuntimeFixtureEntry::File(std::fs::read(&path).expect("fixture bytes")),
            );
        }
    }
    snapshot
}

fn runtime_status_json(workspace: &BrWorkspace, label: &str) -> Value {
    let output = run_br(
        workspace,
        ["vcs-status", "--runtime-files", "--json"],
        label,
    );
    assert!(
        output.status.success(),
        "runtime audit failed: {}",
        output.stderr
    );
    serde_json::from_str(&extract_json_payload(&output.stdout)).expect("runtime audit JSON")
}

fn assert_runtime_contract(value: &Value, available: bool) {
    assert_eq!(value["schema"], "br.vcs-runtime-status.v1", "{value}");
    assert_eq!(value["requested"], true, "{value}");
    assert_eq!(value["available"], available, "{value}");
    assert_eq!(value["vcs"], "git", "{value}");
    assert_eq!(value["observation_atomic"], false, "{value}");
    assert_eq!(value["path_scope"], "workspace_metadata", "{value}");
    assert_eq!(value["metadata_path"], ".beads", "{value}");
    assert_eq!(value["index_only"], true, "{value}");
    assert_eq!(value["automatic_remediation_available"], false, "{value}");
    assert!(value["timeout_ms"].as_u64().is_some(), "{value}");
    assert!(value["duration_ms"].as_u64().is_some(), "{value}");
    if !available {
        assert!(value.get("tracked_count").is_none(), "{value}");
        assert!(value.get("files").is_none(), "{value}");
    }
}

#[test]
fn e2e_vcs_runtime_audit_finds_ignored_tracked_files_without_opening_or_repairing_data() {
    let _log = common::test_log(
        "e2e_vcs_runtime_audit_finds_ignored_tracked_files_without_opening_or_repairing_data",
    );
    let workspace = runtime_workspace();
    let runtime = [
        ("beads.db", "database"),
        ("beads.db-wal", "wal"),
        ("beads.db-shm", "shared_memory"),
        ("beads.db-journal", "journal"),
        ("beads.db-wal-cert", "wal_certificate"),
        ("beads.db-fsqlite-ns-gate", "namespace_state"),
        ("beads.db-fsqlite-ns-use", "namespace_state"),
        ("beads.db.fsqlite-migration-state", "migration_state"),
        ("beads.db.bad_20261007", "recovery_evidence"),
        ("beads.db.corrupt_20261007", "recovery_evidence"),
        ("beads.db.stale_20261007", "recovery_evidence"),
        (".br-wal-index-fixture/poisoned-shm", "recovery_evidence"),
        (".br-wal-index-fixture/prepared.json", "recovery_evidence"),
        (
            ".br_recovery/fixture/recovery-failed.json",
            "recovery_evidence",
        ),
        (".br_history/export.jsonl", "history"),
        (".write.lock", "writer_coordination"),
        (
            ".write-waiters.lock/registered.waiter",
            "writer_coordination",
        ),
    ];
    for (relative, _) in runtime {
        let path = workspace.root.join(".beads").join(relative);
        std::fs::create_dir_all(path.parent().expect("runtime parent")).expect("runtime directory");
        if relative != "beads.db" {
            std::fs::write(&path, format!("retained {relative}\0evidence\n"))
                .expect("runtime evidence");
        }
        let ignored = git(
            &workspace.root,
            &[
                "check-ignore",
                "--no-index",
                "-q",
                "--",
                &format!(".beads/{relative}"),
            ],
        );
        assert_eq!(ignored.status.code(), Some(0), "{relative}: {ignored:?}");
    }
    for relative in [
        ".beads/config.yaml",
        ".beads/ordinary-notes.md",
        ".beads/beads.db.notes.md",
        ".beads/docs/recovery-failed.json",
        ".beads/docs/beads.db-wal-cert.md",
        "adjacent/.beads/beads.db-wal-cert",
        "beads.db-wal-cert",
    ] {
        let path = workspace.root.join(relative);
        std::fs::create_dir_all(path.parent().expect("ordinary parent"))
            .expect("ordinary directory");
        let contents = if relative.ends_with("config.yaml") {
            "issue-prefix: runtime\n"
        } else {
            "ordinary tracked data\n"
        };
        std::fs::write(path, contents).expect("non-runtime fixture");
    }
    git_ok(
        &workspace.root,
        &[
            "add",
            "--force",
            "--",
            ".beads",
            "adjacent",
            "beads.db-wal-cert",
        ],
    );
    git_ok(
        &workspace.root,
        &["commit", "-m", "retain tracked runtime fixtures"],
    );

    // Add a genuine index entry without ever creating its worktree leaf.
    // An inventory of existing files would silently miss this tracked risk.
    let missing = "beads.db-wal-cert-head";
    let blob = git_stdout(
        &workspace.root,
        &["rev-parse", "HEAD:.beads/beads.db-wal-cert"],
    );
    git_ok(
        &workspace.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},.beads/{missing}"),
        ],
    );
    assert!(!workspace.root.join(".beads").join(missing).exists());
    let metadata_before = runtime_metadata_snapshot(&workspace);
    let index_before =
        std::fs::read(workspace.root.join(".git/index")).expect("index before audit");
    let head_before = git_stdout(&workspace.root, &["rev-parse", "HEAD"]);
    let head_file_before =
        std::fs::read(workspace.root.join(".git/HEAD")).expect("HEAD before audit");

    for label in [
        "runtime_tracked_inventory",
        "runtime_tracked_inventory_again",
    ] {
        let status = runtime_status_json(&workspace, label);
        assert_runtime_contract(&status, true);
        assert!(status.get("reason").is_none(), "{status}");
        let files = status["files"]
            .as_array()
            .expect("complete runtime inventory");
        assert_eq!(status["tracked_count"], runtime.len() + 1, "{status}");
        assert_eq!(files.len(), runtime.len() + 1, "{status}");
        let observed = files
            .iter()
            .map(|file| {
                (
                    file["path"].as_str().expect("runtime path"),
                    file["kind"].as_str().expect("runtime kind"),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let expected = runtime
            .into_iter()
            .chain([(missing, "wal_certificate")])
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(observed, expected, "{status}");
        assert_eq!(
            runtime_metadata_snapshot(&workspace),
            metadata_before,
            "audit changed data or recovery evidence"
        );
        assert_eq!(
            std::fs::read(workspace.root.join(".git/index")).expect("index after audit"),
            index_before
        );
        assert_eq!(
            std::fs::read(workspace.root.join(".git/HEAD")).expect("HEAD after audit"),
            head_file_before
        );
        assert_eq!(
            git_stdout(&workspace.root, &["rev-parse", "HEAD"]),
            head_before
        );
        assert!(
            !workspace.root.join(".beads").join(missing).exists(),
            "audit rebuilt a missing sidecar"
        );
    }
}

#[test]
fn e2e_vcs_runtime_audit_distinguishes_an_empty_index_from_a_configured_nested_family() {
    let _log = common::test_log(
        "e2e_vcs_runtime_audit_distinguishes_an_empty_index_from_a_configured_nested_family",
    );
    let workspace = runtime_workspace();
    let docs = workspace.root.join(".beads/docs");
    std::fs::create_dir(&docs).expect("ordinary documentation directory");
    std::fs::write(docs.join("beads.db"), "documented database fixture\n")
        .expect("ordinary nested data");
    git_ok(
        &workspace.root,
        &[
            "add",
            "--force",
            "--",
            JSONL,
            ".beads/metadata.json",
            ".beads/.gitignore",
            ".beads/docs/beads.db",
        ],
    );
    git_ok(
        &workspace.root,
        &["commit", "-m", "track ordinary metadata"],
    );
    let empty = runtime_status_json(&workspace, "runtime_empty_inventory");
    assert_runtime_contract(&empty, true);
    assert_eq!(empty["tracked_count"], 0, "{empty}");
    assert_eq!(empty["files"], serde_json::json!([]), "{empty}");
    assert!(empty.get("reason").is_none(), "{empty}");

    std::fs::write(
        workspace.root.join(".beads/metadata.json"),
        r#"{"database":"nested/tracker.sqlite","jsonl_export":"missing.jsonl"}"#,
    )
    .expect("configured nested database");
    let nested = workspace.root.join(".beads/nested");
    std::fs::create_dir(&nested).expect("configured database directory");
    for leaf in [
        "tracker.sqlite",
        "tracker.sqlite-wal-cert",
        "tracker.sqlite-fsqlite-ns-gate",
    ] {
        std::fs::write(nested.join(leaf), format!("invalid but retained {leaf}\0"))
            .expect("configured runtime file");
    }
    // Recovery is retained beside the configured database, which can itself
    // live below the metadata root. JSON-looking leaves in those reserved
    // namespaces remain recovery evidence rather than shared export data.
    for relative in [
        ".br_recovery/run/recovery-failed.json",
        ".br-wal-index-run/prepared.json",
    ] {
        let path = nested.join(relative);
        std::fs::create_dir_all(path.parent().expect("nested recovery parent"))
            .expect("nested recovery directory");
        std::fs::write(
            path,
            r#"{"retained":"recovery evidence","export":"issues.jsonl"}"#,
        )
        .expect("nested recovery receipt");
    }
    git_ok(
        &workspace.root,
        &[
            "add",
            "--force",
            "--",
            ".beads/metadata.json",
            ".beads/nested",
        ],
    );
    let metadata_before = runtime_metadata_snapshot(&workspace);
    let index_before =
        std::fs::read(workspace.root.join(".git/index")).expect("configured-family index");
    let head_before = git_stdout(&workspace.root, &["rev-parse", "HEAD"]);
    let status = runtime_status_json(&workspace, "runtime_configured_nested_family");
    assert_runtime_contract(&status, true);
    assert_eq!(status["tracked_count"], 5, "{status}");
    assert_eq!(
        status["files"],
        serde_json::json!([
            {"path": "nested/.br-wal-index-run/prepared.json", "kind": "recovery_evidence"},
            {"path": "nested/.br_recovery/run/recovery-failed.json", "kind": "recovery_evidence"},
            {"path": "nested/tracker.sqlite", "kind": "database"},
            {"path": "nested/tracker.sqlite-fsqlite-ns-gate", "kind": "namespace_state"},
            {"path": "nested/tracker.sqlite-wal-cert", "kind": "wal_certificate"},
        ]),
        "{status}"
    );
    assert_eq!(runtime_metadata_snapshot(&workspace), metadata_before);
    assert_eq!(
        std::fs::read(workspace.root.join(".git/index")).expect("preserved configured index"),
        index_before
    );
    assert_eq!(
        git_stdout(&workspace.root, &["rev-parse", "HEAD"]),
        head_before
    );
}

#[test]
fn e2e_vcs_runtime_audit_refuses_gitlink_ancestors_that_hide_the_selected_family() {
    let _log = common::test_log(
        "e2e_vcs_runtime_audit_refuses_gitlink_ancestors_that_hide_the_selected_family",
    );
    for (metadata_relative, database_relative, boundary, expected_listing, label) in [
        (
            "nested/tracker/.beads",
            "beads.db",
            "nested",
            b"".as_slice(),
            "runtime_metadata_below_gitlink",
        ),
        (
            ".beads",
            "nested/tracker.sqlite",
            ".beads/nested",
            b"nested\0".as_slice(),
            "runtime_database_below_gitlink",
        ),
    ] {
        let workspace = BrWorkspace::new();
        git_ok(&workspace.root, &["init", "--initial-branch=main"]);
        std::fs::write(
            workspace.root.join("README.md"),
            "ancestor-boundary fixture\n",
        )
        .expect("fixture README");
        git_ok(&workspace.root, &["add", "README.md"]);
        git_ok(
            &workspace.root,
            &["commit", "-m", "real commit for gitlink identity"],
        );
        let head_before = git_stdout(&workspace.root, &["rev-parse", "HEAD"]);
        let metadata = workspace.root.join(metadata_relative);
        let database = metadata.join(database_relative);
        std::fs::create_dir_all(database.parent().expect("database parent"))
            .expect("physically present nested family");
        std::fs::write(
            metadata.join("metadata.json"),
            serde_json::json!({"database": database_relative, "jsonl_export": "issues.jsonl"})
                .to_string(),
        )
        .expect("selected metadata");
        std::fs::write(metadata.join("issues.jsonl"), "not importable JSONL\n")
            .expect("retained invalid export");
        std::fs::write(&database, "invalid SQLite database\0retained")
            .expect("retained invalid database");
        let recovery = database
            .parent()
            .expect("recovery parent")
            .join(".br_recovery/run/recovery-failed.json");
        std::fs::create_dir_all(recovery.parent().expect("receipt parent"))
            .expect("retained recovery directory");
        std::fs::write(recovery, "retained failure evidence\n").expect("retained receipt");

        // A gitlink is one index entry even though a real directory containing
        // the selected tracker exists below it. Plain ls-files at that tracker
        // can therefore look empty or omit its entire configured DB subtree.
        git_ok(
            &workspace.root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{head_before},{boundary}"),
            ],
        );
        let listing = git(&metadata, &["ls-files", "--cached", "-z", "--", "."]);
        assert!(listing.status.success(), "{label}: {listing:?}");
        assert_eq!(
            listing.stdout, expected_listing,
            "{label}: fixture lost its hidden scope"
        );
        let metadata_before = runtime_directory_snapshot(&metadata);
        let index_before =
            std::fs::read(workspace.root.join(".git/index")).expect("index with gitlink boundary");
        let output = run_br_with_env(
            &workspace,
            ["vcs-status", "--runtime-files", "--json"],
            [("BEADS_DIR", metadata.as_os_str())],
            label,
        );
        assert!(output.status.success(), "{label}: {}", output.stderr);
        let status: Value = serde_json::from_str(&extract_json_payload(&output.stdout))
            .expect("ancestor-boundary inventory JSON");
        assert_runtime_contract(&status, false);
        assert_eq!(
            status["reason"], "scope_crosses_tracked_non_directory",
            "{status}"
        );
        assert_eq!(runtime_directory_snapshot(&metadata), metadata_before);
        assert_eq!(
            std::fs::read(workspace.root.join(".git/index")).expect("preserved gitlink index"),
            index_before
        );
        assert_eq!(
            git_stdout(&workspace.root, &["rev-parse", "HEAD"]),
            head_before
        );
        assert!(
            workspace.root.join(boundary).is_dir(),
            "audit displaced a working directory"
        );
    }
}

#[test]
fn e2e_vcs_runtime_audit_counts_real_unmerged_index_paths_once() {
    let _log = common::test_log("e2e_vcs_runtime_audit_counts_real_unmerged_index_paths_once");
    let workspace = runtime_workspace();
    git_ok(
        &workspace.root,
        &["add", "--", ".beads/metadata.json", JSONL],
    );
    git_ok(
        &workspace.root,
        &["commit", "-m", "base for runtime conflict"],
    );
    let path = ".beads/beads.db-wal-cert";
    std::fs::write(workspace.root.join(path), "first certificate generation\n")
        .expect("first blob");
    let first = git_stdout(&workspace.root, &["hash-object", "-w", path]);
    std::fs::write(workspace.root.join(path), "second certificate generation\n")
        .expect("second blob");
    let second = git_stdout(&workspace.root, &["hash-object", "-w", path]);
    assert_ne!(first, second);
    git_with_stdin_ok(
        &workspace.root,
        &["update-index", "--index-info"],
        &format!("100644 {first} 1\t{path}\n100644 {second} 2\t{path}\n100644 {first} 3\t{path}\n"),
    );
    let stages = git(
        &workspace.root,
        &["ls-files", "--unmerged", "-z", "--", path],
    );
    assert!(stages.status.success(), "{stages:?}");
    // `ls-files -z` terminates every record, the last one included, with NUL.
    let stage_records = stages
        .stdout
        .strip_suffix(b"\0")
        .expect("NUL-terminated unmerged records")
        .split(|byte| *byte == 0)
        .count();
    assert_eq!(stage_records, 3, "three actual conflict stages");
    let metadata_before = runtime_metadata_snapshot(&workspace);
    let index_before = std::fs::read(workspace.root.join(".git/index")).expect("unmerged index");
    let head_before = git_stdout(&workspace.root, &["rev-parse", "HEAD"]);

    let status = runtime_status_json(&workspace, "runtime_unmerged_inventory");
    assert_runtime_contract(&status, true);
    assert_eq!(status["tracked_count"], 1, "{status}");
    assert_eq!(
        status["files"],
        serde_json::json!([{"path": "beads.db-wal-cert", "kind": "wal_certificate"}]),
        "{status}"
    );
    assert_eq!(runtime_metadata_snapshot(&workspace), metadata_before);
    assert_eq!(
        std::fs::read(workspace.root.join(".git/index")).expect("preserved unmerged index"),
        index_before
    );
    assert_eq!(
        git_stdout(&workspace.root, &["rev-parse", "HEAD"]),
        head_before
    );
}

#[test]
fn e2e_vcs_runtime_audit_refuses_truncated_index_output_without_a_partial_count() {
    let _log = common::test_log(
        "e2e_vcs_runtime_audit_refuses_truncated_index_output_without_a_partial_count",
    );
    let workspace = runtime_workspace();
    let blob = git_stdout(&workspace.root, &["hash-object", "-w", ".beads/beads.db"]);
    let mut entries = String::new();
    for index in 0..512 {
        entries.push_str(&format!(
            "100644 {blob}\t.beads/.br_recovery/retained-generation-{index:04}-with-a-long-diagnostic-name/original.db\n"
        ));
    }
    git_with_stdin_ok(&workspace.root, &["update-index", "--index-info"], &entries);
    let actual = git(
        &workspace.root.join(".beads"),
        &["ls-files", "--cached", "-z", "--", "."],
    );
    assert!(actual.status.success(), "{actual:?}");
    assert!(
        actual.stdout.len() > 32 * 1024,
        "fixture must exceed the retained-output cap"
    );
    let index_before = std::fs::read(workspace.root.join(".git/index")).expect("large index");
    let metadata_before = runtime_metadata_snapshot(&workspace);
    let output = run_br(
        &workspace,
        [
            "vcs-status",
            "--runtime-files",
            "--timeout-ms",
            "30000",
            "--json",
        ],
        "runtime_inventory_output_limit",
    );
    assert!(
        output.status.success(),
        "bounded refusal is a diagnostic result: {}",
        output.stderr
    );
    let status: Value = serde_json::from_str(&extract_json_payload(&output.stdout))
        .expect("bounded inventory JSON");
    assert_runtime_contract(&status, false);
    assert_eq!(status["reason"], "probe_output_limit", "{status}");
    assert_eq!(
        std::fs::read(workspace.root.join(".git/index")).expect("preserved large index"),
        index_before
    );
    assert_eq!(runtime_metadata_snapshot(&workspace), metadata_before);
}

#[cfg(unix)]
#[test]
fn e2e_vcs_runtime_audit_keeps_a_selected_symlink_alias_inside_its_metadata_scope() {
    use std::os::unix::fs::{MetadataExt, symlink};

    let _log = common::test_log(
        "e2e_vcs_runtime_audit_keeps_a_selected_symlink_alias_inside_its_metadata_scope",
    );
    let workspace = runtime_workspace();
    let foreign = workspace.root.join("outside-runtime");
    std::fs::create_dir(&foreign).expect("foreign database directory");
    for (leaf, bytes) in [
        ("private.sqlite", "foreign invalid database\0"),
        ("private.sqlite-wal", "foreign WAL bytes\0"),
        ("private.sqlite-wal-cert", "foreign certificate bytes\0"),
        (
            "private.sqlite-fsqlite-ns-gate",
            "foreign namespace bytes\0",
        ),
    ] {
        std::fs::write(foreign.join(leaf), bytes).expect("foreign runtime fixture");
    }
    let selected = workspace.root.join(".beads/custom.sqlite");
    symlink(foreign.join("private.sqlite"), &selected).expect("custom database symlink");
    for leaf in ["custom.sqlite-wal-cert", "custom.sqlite-fsqlite-ns-gate"] {
        std::fs::write(
            workspace.root.join(".beads").join(leaf),
            format!("retained local alias {leaf}\0"),
        )
        .expect("local alias sidecar");
    }
    git_ok(
        &workspace.root,
        &[
            "add",
            "--force",
            "--",
            ".beads/metadata.json",
            JSONL,
            ".beads/custom.sqlite",
            ".beads/custom.sqlite-wal-cert",
            ".beads/custom.sqlite-fsqlite-ns-gate",
            "outside-runtime",
        ],
    );
    git_ok(
        &workspace.root,
        &[
            "commit",
            "-m",
            "track selected alias and foreign runtime fixtures",
        ],
    );
    let alias_index = git_stdout(
        &workspace.root,
        &["ls-files", "--stage", "--", ".beads/custom.sqlite"],
    );
    assert!(alias_index.starts_with("120000 "), "{alias_index}");
    let foreign_snapshot = || {
        std::fs::read_dir(&foreign)
            .expect("foreign directory")
            .map(|entry| {
                let entry = entry.expect("foreign entry");
                let metadata = std::fs::symlink_metadata(entry.path()).expect("foreign metadata");
                assert!(metadata.is_file());
                (
                    entry.file_name(),
                    (
                        metadata.dev(),
                        metadata.ino(),
                        metadata.mode(),
                        metadata.len(),
                        std::fs::read(entry.path()).expect("foreign bytes"),
                    ),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let foreign_before = foreign_snapshot();
    let index_before = std::fs::read(workspace.root.join(".git/index")).expect("alias index");
    let head_before = git_stdout(&workspace.root, &["rev-parse", "HEAD"]);

    for (args, label) in [
        (
            vec![
                "--db",
                ".beads/custom.sqlite",
                "vcs-status",
                "--runtime-files",
                "--json",
            ],
            "runtime_cli_selected_symlink",
        ),
        (
            vec!["vcs-status", "--runtime-files", "--json"],
            "runtime_metadata_selected_symlink",
        ),
        (
            vec!["vcs-status", "--runtime-files", "--json"],
            "runtime_project_selected_symlink",
        ),
    ] {
        if label == "runtime_metadata_selected_symlink" {
            std::fs::write(
                workspace.root.join(".beads/metadata.json"),
                r#"{"database":"custom.sqlite","jsonl_export":"missing.jsonl"}"#,
            )
            .expect("metadata-selected alias");
        } else if label == "runtime_project_selected_symlink" {
            std::fs::write(
                workspace.root.join(".beads/metadata.json"),
                r#"{"database":"beads.db","jsonl_export":"missing.jsonl"}"#,
            )
            .expect("metadata fallback differs from the selected alias");
            std::fs::write(
                workspace.root.join(".beads/config.yaml"),
                "db: custom.sqlite\n",
            )
            .expect("startup-layer-selected alias");
        }
        let metadata_before = runtime_metadata_snapshot(&workspace);
        let alias_before = std::fs::symlink_metadata(&selected).expect("alias identity");
        let output = run_br(&workspace, args, label);
        assert!(output.status.success(), "{label}: {}", output.stderr);
        let status: Value = serde_json::from_str(&extract_json_payload(&output.stdout))
            .expect("alias inventory JSON");
        assert_runtime_contract(&status, true);
        assert_eq!(status["tracked_count"], 3, "{status}");
        assert_eq!(
            status["files"],
            serde_json::json!([
                {"path": "custom.sqlite", "kind": "database"},
                {"path": "custom.sqlite-fsqlite-ns-gate", "kind": "namespace_state"},
                {"path": "custom.sqlite-wal-cert", "kind": "wal_certificate"},
            ]),
            "{status}"
        );
        assert_eq!(runtime_metadata_snapshot(&workspace), metadata_before);
        let alias_after = std::fs::symlink_metadata(&selected).expect("preserved alias identity");
        assert_eq!(
            (
                alias_after.dev(),
                alias_after.ino(),
                alias_after.mode(),
                alias_after.len()
            ),
            (
                alias_before.dev(),
                alias_before.ino(),
                alias_before.mode(),
                alias_before.len()
            ),
            "audit replaced the selected symlink"
        );
        assert_eq!(
            foreign_snapshot(),
            foreign_before,
            "audit touched the foreign family"
        );
        assert_eq!(
            std::fs::read(workspace.root.join(".git/index")).expect("preserved alias index"),
            index_before
        );
        assert_eq!(
            git_stdout(&workspace.root, &["rev-parse", "HEAD"]),
            head_before
        );
    }
}

#[cfg(unix)]
#[test]
fn e2e_vcs_runtime_audit_refuses_non_utf8_paths_without_lossy_or_partial_results() {
    use std::os::unix::ffi::OsStringExt;

    let _log = common::test_log(
        "e2e_vcs_runtime_audit_refuses_non_utf8_paths_without_lossy_or_partial_results",
    );
    let workspace = runtime_workspace();
    let relative = std::path::PathBuf::from(std::ffi::OsString::from_vec(
        b".beads/.br_recovery/invalid-\xff/original.db".to_vec(),
    ));
    let path = workspace.root.join(&relative);
    std::fs::create_dir_all(path.parent().expect("non-UTF-8 parent")).expect("non-UTF-8 directory");
    std::fs::write(&path, "retained recovery bytes\0").expect("non-UTF-8 evidence");
    // A valid finding sorts before the unsupported record. Even this known
    // prefix must not be presented as a complete or empty inventory.
    std::fs::write(
        workspace.root.join(".beads/.br_recovery/aaa.db"),
        "retained earlier recovery evidence\n",
    )
    .expect("valid finding before unsupported path");
    let staged = Command::new("git")
        .current_dir(&workspace.root)
        .args(["add", "--force", "--"])
        .arg(".beads/.br_recovery/aaa.db")
        .arg(&relative)
        .env("HOME", &workspace.root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("stage raw path");
    assert!(staged.status.success(), "{staged:?}");
    let index_before = std::fs::read(workspace.root.join(".git/index")).expect("raw-path index");
    let metadata_before = runtime_metadata_snapshot(&workspace);
    let status = runtime_status_json(&workspace, "runtime_non_utf8_inventory");
    assert_runtime_contract(&status, false);
    assert_eq!(status["reason"], "unsupported_path_encoding", "{status}");
    assert_eq!(
        std::fs::read(workspace.root.join(".git/index")).expect("preserved raw-path index"),
        index_before
    );
    assert_eq!(runtime_metadata_snapshot(&workspace), metadata_before);
}

#[cfg(unix)]
#[test]
fn e2e_vcs_runtime_audit_times_out_an_effective_git_probe_without_repairing_data() {
    use std::os::unix::fs::PermissionsExt;

    let _log = common::test_log(
        "e2e_vcs_runtime_audit_times_out_an_effective_git_probe_without_repairing_data",
    );
    let workspace = runtime_workspace();
    let probe_dir = workspace.root.join("runtime-timeout-git");
    std::fs::create_dir(&probe_dir).expect("timeout probe directory");
    let sentinel = workspace.root.join("runtime-probe-invoked");
    let script = probe_dir.join("git");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf invoked > \"$RUNTIME_AUDIT_PROBE_SENTINEL\"\nwhile :; do :; done\n",
    )
    .expect("timeout probe");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        .expect("executable timeout probe");
    let metadata_before = runtime_metadata_snapshot(&workspace);
    let head_before = std::fs::read(workspace.root.join(".git/HEAD")).expect("unborn HEAD");
    assert!(!workspace.root.join(".git/index").exists());

    // The shared runner rewrites PATH after caller overrides. Set it last on
    // the actual command so this test cannot accidentally use ordinary Git.
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("br"));
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("BD_")
            || name.starts_with("BEADS_")
            || matches!(
                name.as_ref(),
                "BR_DISABLE_READ_ONLY_FAST_OPEN"
                    | "BR_OUTPUT_FORMAT"
                    | "TOON_DEFAULT_FORMAT"
                    | "TOON_STATS"
            )
        {
            command.env_remove(key);
        }
    }
    command
        .current_dir(&workspace.root)
        .args([
            "vcs-status",
            "--runtime-files",
            "--timeout-ms",
            "500",
            "--json",
        ])
        .env("HOME", &workspace.root)
        .env("RUST_LOG", "error")
        .env("NO_COLOR", "1")
        .env("RUST_BACKTRACE", "1")
        .env("RUNTIME_AUDIT_PROBE_SENTINEL", &sentinel)
        .env("PATH", &probe_dir);
    let started = std::time::Instant::now();
    let output = command.output().expect("runtime audit with timed-out Git");
    let duration = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    std::fs::write(
        workspace.log_dir.join("runtime_probe_timeout.log"),
        format!(
            "duration: {duration:?}\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}\n",
            output.status
        ),
    )
    .expect("timeout diagnostic log");
    assert!(
        output.status.success(),
        "bounded timeout is a diagnostic result: {stderr}"
    );
    assert!(sentinel.exists(), "the test did not exercise its Git probe");
    assert!(
        duration < std::time::Duration::from_secs(5),
        "probe did not stop near its budget: {duration:?}"
    );
    let status: Value =
        serde_json::from_str(&extract_json_payload(&stdout)).expect("timeout inventory JSON");
    assert_runtime_contract(&status, false);
    assert_eq!(status["reason"], "probe_timed_out", "{status}");
    assert_eq!(runtime_metadata_snapshot(&workspace), metadata_before);
    assert_eq!(
        std::fs::read(workspace.root.join(".git/HEAD")).expect("preserved HEAD"),
        head_before
    );
    assert!(
        !workspace.root.join(".git/index").exists(),
        "audit created an index"
    );
}

#[test]
fn e2e_vcs_runtime_audit_rejects_jsonl_scope_flags_before_inspecting_data() {
    let _log =
        common::test_log("e2e_vcs_runtime_audit_rejects_jsonl_scope_flags_before_inspecting_data");
    let workspace = runtime_workspace();
    let metadata_before = runtime_metadata_snapshot(&workspace);
    for (args, label) in [
        (
            vec![
                "vcs-status",
                "--runtime-files",
                "--jsonl",
                ".beads/issues.jsonl",
                "--json",
            ],
            "runtime_jsonl_conflict",
        ),
        (
            vec![
                "vcs-status",
                "--runtime-files",
                "--allow-external-jsonl",
                "--json",
            ],
            "runtime_external_conflict",
        ),
    ] {
        let output = run_br(&workspace, args, label);
        assert!(
            !output.status.success(),
            "scope conflict was accepted: {output:?}"
        );
        let diagnostic = format!("{}{}", output.stdout, output.stderr);
        assert!(diagnostic.contains("--runtime-files"), "{diagnostic}");
        assert!(diagnostic.contains("cannot be used with"), "{diagnostic}");
        assert_eq!(runtime_metadata_snapshot(&workspace), metadata_before);
    }
}

#[test]
fn e2e_doctor_git_runtime_files_is_explicit_and_preserves_tracked_evidence() {
    let _log =
        common::test_log("e2e_doctor_git_runtime_files_is_explicit_and_preserves_tracked_evidence");
    let workspace = tracked_workspace();
    let metadata = workspace.root.join(".beads");
    let evidence_dir = metadata.join(".br-wal-index-doctor-opt-in");
    std::fs::create_dir(&evidence_dir).expect("retained recovery directory");
    let retained = ".br-wal-index-doctor-opt-in/poisoned-shm";
    std::fs::write(metadata.join(retained), b"retained recovery evidence\0\n")
        .expect("retained recovery bytes");
    git_ok(
        &workspace.root,
        &[
            "add",
            "--force",
            "--",
            ".beads/.br-wal-index-doctor-opt-in/poisoned-shm",
            ".beads/metadata.json",
        ],
    );
    // A genuine tracked entry with no worktree leaf proves the detector
    // inventories the index instead of listing existing runtime files.
    let missing = ".br_recovery/doctor-opt-in-missing/original.db";
    let blob = git_stdout(
        &workspace.root,
        &[
            "hash-object",
            "-w",
            ".beads/.br-wal-index-doctor-opt-in/poisoned-shm",
        ],
    );
    git_ok(
        &workspace.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},.beads/{missing}"),
        ],
    );
    let expected_paths = serde_json::json!([retained, missing]);
    let git_dir = workspace.root.join(".git");
    let git_before = runtime_directory_snapshot(&git_dir);
    let evidence_before = runtime_directory_snapshot(&evidence_dir);
    let database_before = std::fs::read(metadata.join("beads.db")).expect("database bytes");
    let assert_preserved = || {
        assert_eq!(runtime_directory_snapshot(&git_dir), git_before);
        assert_eq!(runtime_directory_snapshot(&evidence_dir), evidence_before);
        assert!(
            !metadata.join(missing).exists(),
            "doctor created missing evidence"
        );
        assert_eq!(
            std::fs::read(metadata.join("beads.db")).expect("database remains readable"),
            database_before
        );
    };
    let assert_inventory = |report: &Value| {
        let check = report["checks"]
            .as_array()
            .expect("doctor checks")
            .iter()
            .find(|check| check["name"] == "git.tracked_runtime_files")
            .expect("explicit runtime inventory");
        assert_eq!(check["status"], "ok", "{report}");
        assert_eq!(check["details"]["inspected"], true, "{check}");
        assert_eq!(check["details"]["tracked_count"], 2, "{check}");
        assert_eq!(check["details"]["tracked_files"], expected_paths, "{check}");
    };
    for (args, label) in [
        (vec!["doctor", "--no-db", "--json"], "full"),
        (vec!["doctor", "--no-db", "--quick", "--json"], "quick"),
    ] {
        let baseline = run_br(
            &workspace,
            args.clone(),
            &format!("doctor_git_{label}_baseline"),
        );
        assert!(
            matches!(baseline.status.code(), Some(0 | 1)),
            "{baseline:?}"
        );
        let baseline_report: Value =
            serde_json::from_str(&extract_json_payload(&baseline.stdout)).expect("doctor JSON");
        assert!(
            baseline_report["checks"]
                .as_array()
                .expect("baseline checks")
                .iter()
                .all(|check| check["name"] != "git.tracked_runtime_files"),
            "{baseline_report}"
        );
        assert_preserved();
        let mut explicit_args = args;
        explicit_args.push("--git-runtime-files");
        let explicit = run_br(
            &workspace,
            explicit_args,
            &format!("doctor_git_{label}_explicit"),
        );
        assert_eq!(
            explicit.status.code(),
            baseline.status.code(),
            "informational inventory changed doctor exit status: {explicit:?}"
        );
        let report: Value =
            serde_json::from_str(&extract_json_payload(&explicit.stdout)).expect("explicit JSON");
        assert_inventory(&report);
        assert_preserved();
    }

    // Force a real early repair and report recollection. The explicit
    // inventory must survive in the emitted post-repair report as well.
    let ignore_path = metadata.join(".gitignore");
    let original = std::fs::read_to_string(&ignore_path).expect("canonical ignore rules");
    assert!(original.lines().any(|line| line == ".br-wal-index-*/"));
    let legacy = original
        .lines()
        .filter(|line| *line != ".br-wal-index-*/")
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&ignore_path, format!("{legacy}\n")).expect("legacy ignore rules");
    let repaired = run_br(
        &workspace,
        [
            "doctor",
            "--no-db",
            "--git-runtime-files",
            "--repair",
            "--only",
            "fm-configs-gitignore-leaking-beads",
            "--json",
        ],
        "doctor_git_explicit_repair",
    );
    assert!(
        matches!(repaired.status.code(), Some(0 | 2)),
        "{repaired:?}"
    );
    let repair: Value =
        serde_json::from_str(&extract_json_payload(&repaired.stdout)).expect("repair JSON");
    assert_eq!(repair["repaired"], true, "{repair}");
    assert_inventory(&repair["report"]);
    assert_inventory(&repair["post_repair"]);
    assert!(
        std::fs::read_to_string(&ignore_path)
            .expect("repaired ignore rules")
            .lines()
            .any(|line| line == ".br-wal-index-*/")
    );
    assert_preserved();
}

#[test]
fn e2e_doctor_git_runtime_files_keeps_unavailable_inventory_unknown() {
    let _log = common::test_log("e2e_doctor_git_runtime_files_keeps_unavailable_inventory_unknown");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "doctor_git_unavailable_init");
    assert!(init.status.success(), "{init:?}");
    let output = run_br(
        &workspace,
        ["doctor", "--no-db", "--git-runtime-files", "--json"],
        "doctor_git_not_a_repository",
    );
    assert!(matches!(output.status.code(), Some(0 | 1)), "{output:?}");
    let report: Value =
        serde_json::from_str(&extract_json_payload(&output.stdout)).expect("doctor JSON");
    let check = report["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["name"] == "git.tracked_runtime_files")
        .expect("requested but unavailable inventory");
    assert_eq!(check["status"], "ok", "{check}");
    assert_eq!(check["details"]["inspected"], false, "{check}");
    assert!(check["details"].get("tracked_count").is_none(), "{check}");
    assert!(check["details"].get("tracked_files").is_none(), "{check}");

    let unsupported = run_br(
        &workspace,
        ["doctor", "--git-runtime-files", "health", "--json"],
        "doctor_git_rejects_ignored_subcommand_option",
    );
    assert!(!unsupported.status.success(), "{unsupported:?}");
    assert!(
        format!("{}{}", unsupported.stdout, unsupported.stderr).contains("--git-runtime-files"),
        "{unsupported:?}"
    );

    let unsupported_triage = run_br(
        &workspace,
        ["doctor", "--git-runtime-files", "--robot-triage", "--json"],
        "doctor_git_rejects_discarded_triage_inventory",
    );
    assert_eq!(
        unsupported_triage.status.code(),
        Some(2),
        "{unsupported_triage:?}"
    );
    let diagnostic = format!("{}{}", unsupported_triage.stdout, unsupported_triage.stderr);
    assert!(diagnostic.contains("cannot be used with"), "{diagnostic}");
    assert!(diagnostic.contains("--git-runtime-files"), "{diagnostic}");
    assert!(diagnostic.contains("--robot-triage"), "{diagnostic}");
}

#[cfg(unix)]
#[test]
fn e2e_runtime_recovery_directories_are_ignored_by_init_and_repair() {
    use std::os::unix::fs::PermissionsExt;

    let _log = common::test_log("e2e_runtime_recovery_directories_are_ignored_by_init_and_repair");
    let workspace = tracked_workspace();
    let ignore_path = workspace.root.join(".beads/.gitignore");
    let canonical = std::fs::read_to_string(&ignore_path).expect("canonical ignore file");
    let directories = [
        ".br-wal-index-*/",
        ".br_recovery/",
        ".br_history/",
        ".write-waiters.lock/",
    ];
    let evidence = [
        (
            ".beads/.br-wal-index-fixture/poisoned-shm",
            "retained\0original WAL index",
        ),
        (
            ".beads/.br-wal-index-fixture/prepared.json",
            "retained quarantine receipt\n",
        ),
        (
            ".beads/.br_recovery/fixture/original.bin",
            "retained recovery bytes\n",
        ),
        (
            ".beads/.br_history/fixture.jsonl",
            "retained local history\n",
        ),
        (
            ".beads/.write-waiters.lock/00000000000000000001-0123456789abcdef.waiter",
            // A valid, unlocked peer registration is retained by the queue
            // scanner; only an owning WorkspaceWriteWaiter removes its leaf.
            "retained abandoned registration\n",
        ),
    ];
    let assert_ignored = |relative: &str, ignored: bool| {
        let result = git(
            &workspace.root,
            &["check-ignore", "--no-index", "-q", "--", relative],
        );
        // `check-ignore -q` exits 0 for an ignored path and 1 otherwise.
        assert_eq!(
            result.status.code(),
            Some(i32::from(!ignored)),
            "wrong Git ignore result for {relative}: {result:?}"
        );
    };
    for (relative, contents) in evidence {
        let path = workspace.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).expect("runtime directory");
        std::fs::write(path, contents).expect("retained evidence");
        assert_ignored(relative, true);
    }
    for relative in [JSONL, ".beads/config.yaml", ".beads/.gitignore"] {
        assert_ignored(relative, false);
    }

    // Reproduce an older file-only ignore set. Keep the DB lock files
    // covered explicitly so *.lock cannot conceal missing waiter coverage.
    let legacy = canonical
        .lines()
        .filter(|line| !directories.contains(line) && *line != "*.lock")
        .chain([
            ".br-db-openers-*.lock",
            ".br-db-write-*.lock",
            "# retain operator rules verbatim",
            "local-only/",
        ])
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&ignore_path, &legacy).expect("legacy ignore file");
    for (relative, _) in evidence {
        assert_ignored(relative, false);
    }

    // Git is an independent test oracle above and below, never an implicit
    // doctor subprocess. Reject any attempt to invoke it during the command.
    let sentinel_dir = workspace.root.join("git-sentinel");
    std::fs::create_dir(&sentinel_dir).expect("sentinel directory");
    let sentinel = workspace.root.join("doctor-invoked-git");
    let script = sentinel_dir.join("git");
    std::fs::write(
        &script,
        "#!/bin/sh\n: > \"$DOCTOR_TEST_GIT_SENTINEL\"\nexit 97\n",
    )
    .expect("Git sentinel");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        .expect("executable sentinel");
    // The shared runner deduplicates PATH after caller overrides. Run these
    // probes directly so the rejecting sentinel is the effective Git binary,
    // with the same fixture isolation and per-invocation logs as that runner.
    let run_doctor = |args: &[&str], label: &str| {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("br"));
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("BD_")
                || name.starts_with("BEADS_")
                || matches!(
                    name.as_ref(),
                    "BR_DISABLE_READ_ONLY_FAST_OPEN"
                        | "BR_OUTPUT_FORMAT"
                        | "TOON_DEFAULT_FORMAT"
                        | "TOON_STATS"
                )
            {
                command.env_remove(key);
            }
        }
        command
            .current_dir(&workspace.root)
            .args(args)
            .env("BR_HISTORY_MIN_INTERVAL_SECS", "0")
            .env("HOME", &workspace.root)
            .env("RUST_LOG", "error")
            .env("NO_COLOR", "1")
            .env("RUST_BACKTRACE", "1")
            .env("DOCTOR_TEST_GIT_SENTINEL", &sentinel)
            .env("PATH", &sentinel_dir);
        let start = std::time::Instant::now();
        let output = command.output().expect("run doctor with Git sentinel");
        let duration = start.elapsed();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let log_path = workspace.log_dir.join(format!("{label}.log"));
        std::fs::write(
            &log_path,
            format!(
                "label: {label}\nduration: {duration:?}\nstatus: {}\nargs: {args:?}\n\nstdout:\n{stdout}\n\nstderr:\n{stderr}\n",
                output.status
            ),
        )
        .expect("write doctor invocation log");
        eprintln!(
            "{}",
            serde_json::json!({"kind": "cli_harness", "workspace": workspace.root,
                "label": label, "binary": assert_cmd::cargo::cargo_bin!("br"),
                "args": args, "exit": output.status.code(),
                "duration_ms": duration.as_millis(), "stdout": stdout, "stderr": stderr})
        );
        common::cli::BrRun {
            stdout,
            stderr,
            status: output.status,
            duration,
            log_path,
        }
    };
    let index_before = std::fs::read(workspace.root.join(".git/index")).expect("Git index");
    let diagnosed = run_doctor(&["doctor", "--no-db", "--json"], "runtime_ignore_before");
    let report: Value =
        serde_json::from_str(&extract_json_payload(&diagnosed.stdout)).expect("doctor report JSON");
    let check = report["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["name"] == "gitignore.beads_inner_present")
        .expect("inner ignore finding");
    assert_eq!(check["status"], "warn", "{report}");
    assert_eq!(
        check["details"]["missing_patterns"],
        serde_json::json!(directories)
    );

    let expected = format!("{legacy}\n{}\n", directories.join("\n"));
    for label in ["runtime_ignore_repair", "runtime_ignore_repeat"] {
        let repaired = run_doctor(
            &[
                "doctor",
                "--no-db",
                "--repair",
                "--only",
                "fm-configs-gitignore-leaking-beads",
                "--json",
            ],
            label,
        );
        assert!(
            matches!(repaired.status.code(), Some(0 | 2)),
            "doctor repair failed: {repaired:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&ignore_path).expect("repaired ignore file"),
            expected,
            "repair must preserve existing bytes and append each missing rule once"
        );
        assert!(!sentinel.exists(), "doctor invoked Git");
        assert_eq!(
            std::fs::read(workspace.root.join(".git/index")).expect("unchanged Git index"),
            index_before
        );
        for (relative, contents) in evidence {
            assert_eq!(
                std::fs::read(workspace.root.join(relative)).expect("preserved evidence"),
                contents.as_bytes(),
                "repair changed {relative}"
            );
            assert_ignored(relative, true);
        }
        for relative in [JSONL, ".beads/config.yaml", ".beads/.gitignore"] {
            assert_ignored(relative, false);
        }
    }
}

#[test]
fn e2e_vcs_status_tracks_untracked_clean_unstaged_staged_and_double_dirty_states() {
    let _log = common::test_log(
        "e2e_vcs_status_tracks_untracked_clean_unstaged_staged_and_double_dirty_states",
    );
    let workspace = export_workspace();
    let untracked = vcs_status_json(&workspace, "untracked");
    assert_common_contract(&untracked, true);
    assert!(untracked.get("reason").is_none(), "{untracked}");
    assert_eq!(untracked["object_format"], "sha1", "{untracked}");
    assert_eq!(untracked["tracked"], false, "{untracked}");
    assert_eq!(untracked["index_clean"], true, "{untracked}");
    assert_eq!(untracked["worktree_state"], "untracked", "{untracked}");
    assert_eq!(untracked["worktree_clean"], false, "{untracked}");
    assert!(untracked.get("head").is_none(), "{untracked}");
    assert!(untracked.get("index").is_none(), "{untracked}");
    assert_eq!(
        untracked["worktree_raw_git_blob_hash"]
            .as_str()
            .expect("raw Git blob hash")
            .len(),
        40,
        "{untracked}"
    );
    assert_eq!(
        untracked["worktree_raw_sha256"]
            .as_str()
            .expect("raw SHA-256")
            .len(),
        64,
        "{untracked}"
    );

    git_ok(&workspace.root, &["add", JSONL]);
    git_ok(&workspace.root, &["commit", "-m", "track JSONL export"]);
    let committed = vcs_status_json(&workspace, "committed");
    assert_common_contract(&committed, true);
    assert_eq!(committed["tracked"], true, "{committed}");
    assert_eq!(committed["index_clean"], true, "{committed}");
    assert_eq!(committed["worktree_state"], "clean", "{committed}");
    assert_eq!(committed["worktree_clean"], true, "{committed}");
    assert_eq!(
        committed["head"]["object_id"], committed["worktree_raw_git_blob_hash"],
        "{committed}"
    );
    assert_eq!(committed["head"], committed["index"], "{committed}");

    append_jsonl(&workspace, "bd-unstaged");
    let unstaged = vcs_status_json(&workspace, "unstaged");
    assert_common_contract(&unstaged, true);
    assert_eq!(unstaged["tracked"], true, "{unstaged}");
    assert_eq!(unstaged["index_clean"], true, "{unstaged}");
    assert_eq!(unstaged["worktree_state"], "modified", "{unstaged}");
    assert_eq!(unstaged["worktree_clean"], false, "{unstaged}");
    assert_ne!(
        unstaged["index"]["object_id"], unstaged["worktree_raw_git_blob_hash"],
        "{unstaged}"
    );

    git_ok(&workspace.root, &["add", JSONL]);
    let staged = vcs_status_json(&workspace, "staged_matching");
    assert_common_contract(&staged, true);
    assert_eq!(staged["index_clean"], false, "{staged}");
    assert_eq!(staged["worktree_state"], "clean", "{staged}");
    assert_eq!(staged["worktree_clean"], true, "{staged}");
    assert_ne!(staged["head"], staged["index"], "{staged}");
    assert_eq!(
        staged["index"]["object_id"], staged["worktree_raw_git_blob_hash"],
        "{staged}"
    );

    append_jsonl(&workspace, "bd-after-stage");
    let double_dirty = vcs_status_json(&workspace, "staged_then_modified");
    assert_common_contract(&double_dirty, true);
    assert_eq!(double_dirty["index_clean"], false, "{double_dirty}");
    assert_eq!(double_dirty["worktree_state"], "modified", "{double_dirty}");
    assert_eq!(double_dirty["worktree_clean"], false, "{double_dirty}");
    assert_ne!(
        double_dirty["index"]["object_id"], double_dirty["worktree_raw_git_blob_hash"],
        "{double_dirty}"
    );
}

#[test]
fn e2e_vcs_status_computes_sha256_repository_blob_ids_in_process() {
    let _log = common::test_log("e2e_vcs_status_computes_sha256_repository_blob_ids_in_process");
    let workspace = BrWorkspace::new();
    git_ok(
        &workspace.root,
        &["init", "--initial-branch=main", "--object-format=sha256"],
    );
    let init = run_br(&workspace, ["init"], "sha256_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create = run_br(&workspace, ["create", "SHA-256 VCS issue"], "sha256_create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let flush = run_br(&workspace, ["sync", "--flush-only"], "sha256_flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);
    git_ok(&workspace.root, &["add", JSONL]);
    git_ok(&workspace.root, &["commit", "-m", "track SHA-256 export"]);

    let status = vcs_status_json(&workspace, "sha256_status");
    assert_common_contract(&status, true);
    assert_eq!(status["object_format"], "sha256", "{status}");
    assert_eq!(
        status["worktree_raw_git_blob_hash"]
            .as_str()
            .expect("raw Git blob")
            .len(),
        64,
        "{status}"
    );
    assert_eq!(
        status["head"]["object_id"], status["worktree_raw_git_blob_hash"],
        "{status}"
    );
    assert_eq!(status["worktree_state"], "clean", "{status}");
}

#[test]
fn e2e_vcs_status_distinguishes_staged_add_delete_recreate_and_unstaged_delete() {
    let _log = common::test_log(
        "e2e_vcs_status_distinguishes_staged_add_delete_recreate_and_unstaged_delete",
    );

    let staged_add_workspace = head_then_untracked_export_workspace();
    git_ok(&staged_add_workspace.root, &["add", JSONL]);
    let staged_add = vcs_status_json(&staged_add_workspace, "staged_add");
    assert_common_contract(&staged_add, true);
    assert_eq!(staged_add["tracked"], true, "{staged_add}");
    assert!(staged_add.get("head").is_none(), "{staged_add}");
    assert!(staged_add.get("index").is_some(), "{staged_add}");
    assert_eq!(staged_add["index_clean"], false, "{staged_add}");
    assert_eq!(staged_add["worktree_state"], "clean", "{staged_add}");
    assert_eq!(staged_add["worktree_clean"], true, "{staged_add}");

    let workspace = tracked_workspace();
    let path = workspace.root.join(JSONL);
    let retained = workspace.root.join(".beads/retained-export.jsonl");
    std::fs::rename(&path, &retained).expect("retain JSONL outside its tracked name");

    let unstaged_delete = vcs_status_json(&workspace, "unstaged_delete");
    assert_common_contract(&unstaged_delete, true);
    assert_eq!(unstaged_delete["tracked"], true, "{unstaged_delete}");
    assert_eq!(unstaged_delete["index_clean"], true, "{unstaged_delete}");
    assert_eq!(
        unstaged_delete["worktree_state"], "deleted",
        "{unstaged_delete}"
    );
    assert_eq!(
        unstaged_delete["worktree_clean"], false,
        "{unstaged_delete}"
    );

    git_ok(&workspace.root, &["add", "-u", "--", JSONL]);
    let staged_delete = vcs_status_json(&workspace, "staged_delete");
    assert_common_contract(&staged_delete, true);
    assert_eq!(staged_delete["tracked"], false, "{staged_delete}");
    assert!(staged_delete.get("index").is_none(), "{staged_delete}");
    assert!(staged_delete.get("head").is_some(), "{staged_delete}");
    assert_eq!(staged_delete["index_clean"], false, "{staged_delete}");
    assert_eq!(staged_delete["worktree_state"], "absent", "{staged_delete}");
    assert_eq!(staged_delete["worktree_clean"], true, "{staged_delete}");

    std::fs::copy(&retained, &path).expect("recreate staged-deleted JSONL");
    let recreated = vcs_status_json(&workspace, "staged_delete_recreated");
    assert_common_contract(&recreated, true);
    assert_eq!(recreated["tracked"], false, "{recreated}");
    assert_eq!(recreated["index_clean"], false, "{recreated}");
    assert_eq!(recreated["worktree_state"], "untracked", "{recreated}");
    assert_eq!(recreated["worktree_clean"], false, "{recreated}");
}

#[test]
fn e2e_vcs_status_handles_unborn_ignored_intent_to_add_and_unmerged_index() {
    let _log =
        common::test_log("e2e_vcs_status_handles_unborn_ignored_intent_to_add_and_unmerged_index");

    let unborn = export_workspace();
    let unborn_status = vcs_status_json(&unborn, "unborn_head");
    assert_common_contract(&unborn_status, true);
    assert!(unborn_status.get("head").is_none(), "{unborn_status}");
    assert_eq!(unborn_status["index_clean"], true, "{unborn_status}");
    assert_eq!(
        unborn_status["worktree_state"], "untracked",
        "{unborn_status}"
    );

    let ignored = export_workspace();
    std::fs::write(ignored.root.join(".gitignore"), ".beads/issues.jsonl\n").expect("gitignore");
    git_ok(&ignored.root, &["add", ".gitignore"]);
    git_ok(&ignored.root, &["commit", "-m", "ignore export"]);
    let ignored_status = vcs_status_json(&ignored, "ignored_untracked");
    assert_common_contract(&ignored_status, true);
    assert_eq!(ignored_status["tracked"], false, "{ignored_status}");
    assert_eq!(
        ignored_status["worktree_state"], "ignored",
        "{ignored_status}"
    );
    assert_eq!(ignored_status["worktree_clean"], false, "{ignored_status}");

    let intent = head_then_untracked_export_workspace();
    git_ok(&intent.root, &["add", "--intent-to-add", JSONL]);
    let intent_status = vcs_status_json(&intent, "intent_to_add");
    assert_common_contract(&intent_status, true);
    assert_eq!(intent_status["tracked"], true, "{intent_status}");
    assert_eq!(intent_status["index_clean"], false, "{intent_status}");
    assert_eq!(
        intent_status["worktree_state"], "modified",
        "{intent_status}"
    );
    assert_eq!(intent_status["worktree_clean"], false, "{intent_status}");

    let unmerged = tracked_workspace();
    let head_oid = git_stdout(&unmerged.root, &["rev-parse", &format!("HEAD:{JSONL}")]);
    let alternate_path = unmerged.root.join(".beads/alternate.jsonl");
    std::fs::write(&alternate_path, "{\"id\":\"alternate\"}\n").expect("alternate blob");
    let alternate_text = alternate_path.to_string_lossy();
    let alternate_oid = git_stdout(
        &unmerged.root,
        &["hash-object", "-w", alternate_text.as_ref()],
    );
    git_ok(&unmerged.root, &["update-index", "--force-remove", JSONL]);
    let index_info = format!(
        "100644 {head_oid} 1\t{JSONL}\n\
         100644 {alternate_oid} 2\t{JSONL}\n\
         100644 {head_oid} 3\t{JSONL}\n"
    );
    git_with_stdin_ok(
        &unmerged.root,
        &["update-index", "--index-info"],
        &index_info,
    );
    let unmerged_status = vcs_status_json(&unmerged, "unmerged_index");
    assert_common_contract(&unmerged_status, true);
    assert_eq!(unmerged_status["tracked"], true, "{unmerged_status}");
    assert_eq!(unmerged_status["index_clean"], false, "{unmerged_status}");
    assert_eq!(
        unmerged_status["worktree_state"], "unmerged",
        "{unmerged_status}"
    );
    assert!(
        unmerged_status.get("worktree_clean").is_none(),
        "{unmerged_status}"
    );
    assert_eq!(
        unmerged_status["worktree_comparison_reason"], "git_unmerged_index",
        "{unmerged_status}"
    );
    assert_eq!(
        unmerged_status["unmerged_index_stages"]
            .as_array()
            .expect("unmerged stages")
            .iter()
            .map(|stage| stage["stage"].as_u64().expect("stage"))
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
}

#[test]
fn e2e_vcs_status_preserves_index_evidence_for_assume_unchanged_and_skip_worktree() {
    let _log = common::test_log(
        "e2e_vcs_status_preserves_index_evidence_for_assume_unchanged_and_skip_worktree",
    );
    for (label, flag) in [
        ("assume_unchanged", "--assume-unchanged"),
        ("skip_worktree", "--skip-worktree"),
    ] {
        let workspace = tracked_workspace();
        git_ok(&workspace.root, &["update-index", flag, JSONL]);
        let status = vcs_status_json(&workspace, label);
        assert_common_contract(&status, true);
        assert_eq!(status["tracked"], true, "{label}: {status}");
        assert_eq!(status["index_clean"], true, "{label}: {status}");
        assert_eq!(
            status["worktree_state"], "comparison_unavailable",
            "{label}: {status}"
        );
        assert!(status.get("worktree_clean").is_none(), "{label}: {status}");
        assert_eq!(
            status["worktree_comparison_reason"], "git_index_flags_unsupported",
            "{label}: {status}"
        );
        assert!(status.get("head").is_some(), "{label}: {status}");
        assert!(status.get("index").is_some(), "{label}: {status}");
    }
}

#[derive(Clone, Copy)]
enum TransformFixture {
    TextEol,
    WorkingTreeEncoding,
    Ident,
    CoreAutocrlf,
    CoreAttributesFile,
    InfoAttributes,
}

#[test]
fn e2e_vcs_status_refuses_every_configured_content_transform_without_losing_index_evidence() {
    let _log = common::test_log(
        "e2e_vcs_status_refuses_every_configured_content_transform_without_losing_index_evidence",
    );
    for (label, fixture) in [
        ("text_eol", TransformFixture::TextEol),
        (
            "working_tree_encoding",
            TransformFixture::WorkingTreeEncoding,
        ),
        ("ident", TransformFixture::Ident),
        ("core_autocrlf", TransformFixture::CoreAutocrlf),
        ("core_attributes_file", TransformFixture::CoreAttributesFile),
        ("info_attributes", TransformFixture::InfoAttributes),
    ] {
        let workspace = tracked_workspace();
        match fixture {
            TransformFixture::TextEol => {
                std::fs::write(
                    workspace.root.join(".gitattributes"),
                    ".beads/issues.jsonl text eol=crlf\n",
                )
                .expect("text/eol attributes");
                std::fs::write(workspace.root.join(JSONL), b"{\"id\":\"crlf\"}\r\n")
                    .expect("CRLF worktree content");
            }
            TransformFixture::WorkingTreeEncoding => {
                std::fs::write(
                    workspace.root.join(".gitattributes"),
                    ".beads/issues.jsonl working-tree-encoding=UTF-16\n",
                )
                .expect("working-tree-encoding attribute");
            }
            TransformFixture::Ident => {
                std::fs::write(
                    workspace.root.join(".gitattributes"),
                    ".beads/issues.jsonl ident\n",
                )
                .expect("ident attribute");
            }
            TransformFixture::CoreAutocrlf => {
                git_ok(
                    &workspace.root,
                    &["config", "--local", "core.autocrlf", "true"],
                );
            }
            TransformFixture::CoreAttributesFile => {
                std::fs::write(
                    workspace.root.join(".custom-attributes"),
                    ".beads/issues.jsonl text\n",
                )
                .expect("custom attributes");
                git_ok(
                    &workspace.root,
                    &[
                        "config",
                        "--local",
                        "core.attributesFile",
                        ".custom-attributes",
                    ],
                );
            }
            TransformFixture::InfoAttributes => {
                std::fs::write(
                    workspace.root.join(".git/info/attributes"),
                    ".beads/issues.jsonl text\n",
                )
                .expect("repository-local info attributes");
            }
        }

        let status = vcs_status_json(&workspace, label);
        assert_common_contract(&status, true);
        assert_eq!(status["tracked"], true, "{label}: {status}");
        assert!(status.get("head").is_some(), "{label}: {status}");
        assert!(status.get("index").is_some(), "{label}: {status}");
        assert_eq!(
            status["worktree_state"], "comparison_unavailable",
            "{label}: {status}"
        );
        assert!(status.get("worktree_clean").is_none(), "{label}: {status}");
        assert_eq!(
            status["worktree_comparison_reason"], "git_content_transform_required",
            "{label}: {status}"
        );
        assert!(
            status.get("worktree_raw_git_blob_hash").is_some(),
            "{label}: {status}"
        );
        assert!(
            status.get("worktree_raw_sha256").is_some(),
            "{label}: {status}"
        );
    }
}

#[cfg(unix)]
#[test]
fn e2e_vcs_status_honors_linked_worktree_effective_config() {
    use std::os::unix::fs::PermissionsExt;

    let _log = common::test_log("e2e_vcs_status_honors_linked_worktree_effective_config");
    let workspace = tracked_workspace();
    git_ok(
        &workspace.root,
        &["config", "extensions.worktreeConfig", "true"],
    );
    let linked = workspace.root.join("linked-config-worktree");
    let linked_text = linked
        .to_str()
        .expect("temporary linked-worktree path must be UTF-8");
    git_ok(
        &workspace.root,
        &["worktree", "add", "-b", "linked-config", linked_text],
    );
    // Give the linked worktree its own local store. Since #429, workspace
    // discovery resolves a linked worktree whose `.beads` holds only tracked
    // artifacts to the PRIMARY checkout's `.beads`; the probes would then run
    // in the primary worktree, where the `--worktree` git configuration this
    // test exercises never applies (and the global `core.autocrlf = true`
    // below would surface as `git_content_transform_required` instead).
    for artifact in ["beads.db", "metadata.json"] {
        let source = workspace.root.join(".beads").join(artifact);
        if source.is_file() {
            std::fs::copy(&source, linked.join(".beads").join(artifact))
                .expect("copy primary workspace store into linked worktree");
        }
    }
    let linked_home = workspace.root.join("linked-global-home");
    std::fs::create_dir(&linked_home).expect("linked-worktree global config HOME");
    let linked_global = linked_home.join(".gitconfig");
    std::fs::write(
        &linked_global,
        "[core]\n\tautocrlf = true\n\tfilemode = true\n",
    )
    .expect("linked-worktree global config");
    // Pin every git configuration scope br's effective-config probes can
    // observe. `HOME` alone is not hermetic: without these overrides the
    // probes still read the host's /etc/gitconfig (or the git prefix's
    // system config) and any `$XDG_CONFIG_HOME/git/config`, so host-level
    // entries such as registered git-lfs filters or `core.autocrlf` leak
    // into the assertions. `hardened_git_command` propagates exactly these
    // read-location keys to its effective-config probes.
    let hermetic_env = [
        ("HOME", linked_home.as_os_str()),
        ("GIT_CONFIG_NOSYSTEM", std::ffi::OsStr::new("1")),
        ("GIT_CONFIG_GLOBAL", linked_global.as_os_str()),
    ];

    git_ok(&linked, &["config", "--worktree", "core.filemode", "false"]);
    git_ok(&linked, &["config", "--worktree", "core.autocrlf", "false"]);
    let jsonl = linked.join(JSONL);
    let mut permissions = std::fs::metadata(&jsonl)
        .expect("linked JSONL metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&jsonl, permissions).expect("change linked JSONL executable bits");

    let filemode = run_br_smoke_at_root_with_env(
        &linked,
        ["vcs-status", "--json"],
        hermetic_env,
        "linked_worktree_filemode",
    );
    assert!(filemode.status.success(), "{}", filemode.stderr);
    let filemode: Value =
        serde_json::from_str(&extract_json_payload(&filemode.stdout)).expect("filemode JSON");
    assert_eq!(filemode["worktree_state"], "clean", "{filemode}");
    assert_eq!(filemode["worktree_clean"], true, "{filemode}");

    git_ok(
        &linked,
        &[
            "config",
            "--worktree",
            "core.attributesFile",
            ".linked-attributes",
        ],
    );
    let attributes = run_br_smoke_at_root_with_env(
        &linked,
        ["vcs-status", "--json"],
        hermetic_env,
        "linked_worktree_attributes",
    );
    assert!(attributes.status.success(), "{}", attributes.stderr);
    let attributes: Value =
        serde_json::from_str(&extract_json_payload(&attributes.stdout)).expect("attributes JSON");
    assert_eq!(
        attributes["worktree_state"], "comparison_unavailable",
        "{attributes}"
    );
    assert_eq!(
        attributes["worktree_comparison_reason"], "git_content_transform_required",
        "{attributes}"
    );

    git_ok(
        &linked,
        &["config", "--worktree", "--unset", "core.attributesFile"],
    );
    git_ok(&linked, &["config", "--worktree", "core.autocrlf", "true"]);
    let autocrlf = run_br_smoke_at_root_with_env(
        &linked,
        ["vcs-status", "--json"],
        hermetic_env,
        "linked_worktree_autocrlf",
    );
    assert!(autocrlf.status.success(), "{}", autocrlf.stderr);
    let autocrlf: Value =
        serde_json::from_str(&extract_json_payload(&autocrlf.stdout)).expect("autocrlf JSON");
    assert_eq!(
        autocrlf["worktree_state"], "comparison_unavailable",
        "{autocrlf}"
    );
    assert_eq!(
        autocrlf["worktree_comparison_reason"], "git_content_transform_required",
        "{autocrlf}"
    );
}

#[test]
fn e2e_vcs_status_honors_effective_global_transform_configuration() {
    let _log = common::test_log("e2e_vcs_status_honors_effective_global_transform_configuration");
    for (label, config) in [
        ("global_autocrlf", "[core]\n\tautocrlf = true\n"),
        (
            "global_attributes",
            "[core]\n\tattributesFile = /definitely/not/exposed\n",
        ),
    ] {
        let workspace = tracked_workspace();
        let isolated_home = workspace.root.join("effective-global-config");
        std::fs::create_dir(&isolated_home).expect("isolated HOME");
        std::fs::write(isolated_home.join(".gitconfig"), config).expect("global config");

        let output = run_br_with_env(
            &workspace,
            ["vcs-status", "--json"],
            [("HOME", isolated_home.as_os_str())],
            label,
        );
        assert!(output.status.success(), "{label}: {}", output.stderr);
        assert!(
            !output.stdout.contains("/definitely/not/exposed"),
            "{label}: {}",
            output.stdout
        );
        assert!(
            !output.stderr.contains("/definitely/not/exposed"),
            "{label}: {}",
            output.stderr
        );
        let status: Value =
            serde_json::from_str(&extract_json_payload(&output.stdout)).expect("status JSON");
        assert_eq!(
            status["worktree_state"], "comparison_unavailable",
            "{label}: {status}"
        );
        assert_eq!(
            status["worktree_comparison_reason"], "git_content_transform_required",
            "{label}: {status}"
        );
        assert!(status.get("worktree_clean").is_none(), "{label}: {status}");
    }
}

#[cfg(unix)]
#[test]
fn e2e_vcs_status_never_executes_a_configured_clean_filter() {
    use std::os::unix::fs::PermissionsExt;

    let _log = common::test_log("e2e_vcs_status_never_executes_a_configured_clean_filter");
    let workspace = tracked_workspace();
    let sentinel = workspace.root.join("filter-was-executed");
    let filter = workspace.root.join("hostile-clean-filter");
    let sentinel_text = sentinel
        .to_str()
        .expect("temporary sentinel path must be UTF-8");
    assert!(
        !sentinel_text.contains('\''),
        "temporary sentinel path must not need shell escaping"
    );
    std::fs::write(
        &filter,
        format!("#!/bin/sh\nprintf invoked > '{sentinel_text}'\ncat\n"),
    )
    .expect("filter script");
    let mut permissions = std::fs::metadata(&filter)
        .expect("filter metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&filter, permissions).expect("make filter executable");
    let filter_text = filter
        .to_str()
        .expect("temporary filter path must be UTF-8");
    git_ok(
        &workspace.root,
        &["config", "--local", "filter.sentinel.clean", filter_text],
    );
    std::fs::write(
        workspace.root.join(".gitattributes"),
        ".beads/issues.jsonl filter=sentinel\n",
    )
    .expect("filter attribute");

    let status = vcs_status_json(&workspace, "hostile_filter");
    assert_common_contract(&status, true);
    assert_eq!(
        status["worktree_state"], "comparison_unavailable",
        "{status}"
    );
    assert_eq!(
        status["worktree_comparison_reason"], "git_content_transform_required",
        "{status}"
    );
    assert!(
        !sentinel.exists(),
        "the configured clean filter executed during a read-only diagnostic"
    );

    let human = run_br(&workspace, ["vcs-status"], "hostile_filter_human");
    assert!(human.status.success(), "{}", human.stderr);
    assert!(
        human.stdout.contains("Worktree clean: unavailable"),
        "{}",
        human.stdout
    );
    assert!(
        human.stdout.contains("git_content_transform_required"),
        "{}",
        human.stdout
    );
    assert!(
        !sentinel.exists(),
        "human rendering must not execute the configured clean filter"
    );
}

#[cfg(unix)]
#[test]
fn e2e_vcs_status_detects_executable_changes_and_refuses_index_type_comparison() {
    use std::os::unix::fs::PermissionsExt;

    let _log = common::test_log(
        "e2e_vcs_status_detects_executable_changes_and_refuses_index_type_comparison",
    );
    let executable = tracked_workspace();
    let path = executable.root.join(JSONL);
    let mut permissions = std::fs::metadata(&path)
        .expect("JSONL metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("make JSONL executable");
    let executable_status = vcs_status_json(&executable, "executable_change");
    assert_common_contract(&executable_status, true);
    assert_eq!(
        executable_status["worktree_state"], "modified",
        "{executable_status}"
    );
    assert_eq!(
        executable_status["worktree_clean"], false,
        "{executable_status}"
    );

    let type_change = tracked_workspace();
    let link_blob = type_change.root.join(".beads/link-target.txt");
    std::fs::write(&link_blob, "elsewhere.jsonl").expect("symlink blob content");
    let link_blob_text = link_blob.to_string_lossy();
    let oid = git_stdout(
        &type_change.root,
        &["hash-object", "-w", link_blob_text.as_ref()],
    );
    let cache_info = format!("120000,{oid},{JSONL}");
    git_ok(
        &type_change.root,
        &["update-index", "--cacheinfo", &cache_info],
    );
    let type_status = vcs_status_json(&type_change, "index_type_change");
    assert_common_contract(&type_status, true);
    assert_eq!(type_status["index"]["mode"], "120000", "{type_status}");
    assert_eq!(type_status["index_clean"], false, "{type_status}");
    assert_eq!(
        type_status["worktree_state"], "comparison_unavailable",
        "{type_status}"
    );
    assert_eq!(
        type_status["worktree_comparison_reason"], "git_index_mode_unsupported",
        "{type_status}"
    );
}

#[test]
fn e2e_vcs_status_reports_non_repo_and_missing_git_without_failing() {
    let _log = common::test_log("e2e_vcs_status_reports_non_repo_and_missing_git_without_failing");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let outside_repo = vcs_status_json(&workspace, "outside_repo");
    assert_common_contract(&outside_repo, false);
    assert_eq!(
        outside_repo["reason"], "not_git_repository",
        "{outside_repo}"
    );
    for absent in [
        "object_format",
        "tracked",
        "head",
        "index",
        "unmerged_index_stages",
        "index_clean",
        "worktree_state",
        "worktree_clean",
        "worktree_comparison_reason",
        "worktree_raw_git_blob_hash",
        "worktree_raw_sha256",
    ] {
        assert!(outside_repo.get(absent).is_none(), "{outside_repo}");
    }

    let empty_path = workspace.root.join("empty-path");
    std::fs::create_dir(&empty_path).expect("empty PATH directory");
    let missing = run_br_with_env(
        &workspace,
        ["vcs-status", "--json"],
        [("PATH", empty_path.as_os_str())],
        "git_missing",
    );
    assert!(
        missing.status.success(),
        "missing Git is a diagnostic result, not an execution failure: {}",
        missing.stderr
    );
    let missing: Value =
        serde_json::from_str(&extract_json_payload(&missing.stdout)).expect("missing-Git JSON");
    assert_common_contract(&missing, false);
    // This workspace was never `git init`ed, so the filesystem marker probe
    // classifies it as not-a-repository before the (absent) git binary is
    // ever needed — the more truthful diagnosis of the two (#409).
    assert_eq!(missing["reason"], "not_git_repository", "{missing}");
}

#[test]
fn e2e_vcs_status_distinguishes_missing_leaf_parent_and_corrupt_repository() {
    let _log =
        common::test_log("e2e_vcs_status_distinguishes_missing_leaf_parent_and_corrupt_repository");
    let workspace = tracked_workspace();

    let absent = run_br(
        &workspace,
        [
            "vcs-status",
            "--jsonl",
            ".beads/not-created.jsonl",
            "--json",
        ],
        "missing_leaf",
    );
    assert!(absent.status.success(), "{}", absent.stderr);
    let absent: Value =
        serde_json::from_str(&extract_json_payload(&absent.stdout)).expect("missing-leaf JSON");
    assert_eq!(absent["available"], true, "{absent}");
    assert_eq!(absent["worktree_state"], "absent", "{absent}");
    assert_eq!(absent["worktree_clean"], true, "{absent}");

    let missing_parent = run_br(
        &workspace,
        [
            "vcs-status",
            "--jsonl",
            ".beads/not-created/issues.jsonl",
            "--json",
        ],
        "missing_parent",
    );
    assert!(missing_parent.status.success(), "{}", missing_parent.stderr);
    let missing_parent: Value = serde_json::from_str(&extract_json_payload(&missing_parent.stdout))
        .expect("missing-parent JSON");
    assert_eq!(missing_parent["available"], false, "{missing_parent}");
    assert_eq!(
        missing_parent["reason"], "path_unavailable",
        "{missing_parent}"
    );

    let head = workspace.root.join(".git/HEAD");
    let retained_head = workspace.root.join(".git/HEAD.retained");
    std::fs::rename(&head, &retained_head).expect("retain HEAD while corrupting repository");
    let corrupt = vcs_status_json(&workspace, "corrupt_repository");
    assert_common_contract(&corrupt, false);
    assert_eq!(corrupt["reason"], "probe_failed", "{corrupt}");
}

#[test]
fn e2e_vcs_status_large_source_capture_honors_probe_deadline() {
    let _log = common::test_log("e2e_vcs_status_large_source_capture_honors_probe_deadline");
    let workspace = tracked_workspace();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(workspace.root.join(JSONL))
        .expect("open large-source fixture");
    file.set_len(256 * 1024 * 1024)
        .expect("create sparse large-source fixture");

    let output = run_br(
        &workspace,
        ["vcs-status", "--timeout-ms", "25", "--json"],
        "large_source_deadline",
    );
    assert!(
        output.status.success(),
        "deadline expiry is a diagnostic result: {}",
        output.stderr
    );
    let status: Value =
        serde_json::from_str(&extract_json_payload(&output.stdout)).expect("deadline JSON");
    assert_common_contract(&status, false);
    assert_eq!(status["reason"], "probe_timed_out", "{status}");
}

#[test]
fn e2e_vcs_status_enforces_external_opt_in_before_inspecting_the_leaf() {
    let _log =
        common::test_log("e2e_vcs_status_enforces_external_opt_in_before_inspecting_the_leaf");
    let workspace = tracked_workspace();
    let external_directory = workspace.root.join("external-directory.jsonl");
    std::fs::create_dir(&external_directory).expect("non-regular external leaf");
    let external_text = external_directory.to_string_lossy();
    let output = run_br(
        &workspace,
        ["vcs-status", "--jsonl", external_text.as_ref(), "--json"],
        "external_without_opt_in",
    );
    assert!(
        !output.status.success(),
        "external target must require opt-in"
    );
    assert!(
        output.stdout.contains("--allow-external-jsonl"),
        "structured JSON errors are emitted on stdout: {}",
        output.stdout
    );
    assert!(
        !output.stdout.contains("regular file") && !output.stderr.contains("regular file"),
        "the leaf was inspected before external opt-in: stdout={} stderr={}",
        output.stdout,
        output.stderr
    );
}

#[cfg(unix)]
#[test]
fn e2e_vcs_status_rejects_a_jsonl_leaf_symlink() {
    use std::os::unix::fs::symlink;

    let _log = common::test_log("e2e_vcs_status_rejects_a_jsonl_leaf_symlink");
    let workspace = tracked_workspace();
    let target = workspace.root.join(".beads/symlink-target.jsonl");
    std::fs::write(&target, "{\"protected\":true}\n").expect("symlink target");
    let leaf = workspace.root.join(".beads/linked.jsonl");
    symlink(&target, &leaf).expect("JSONL leaf symlink");
    let output = run_br(
        &workspace,
        ["vcs-status", "--jsonl", ".beads/linked.jsonl", "--json"],
        "symlink_leaf",
    );
    assert!(!output.status.success(), "symlink leaf must be rejected");
    assert!(
        output.stdout.contains("symlink"),
        "structured rejection should identify the symlink: {}",
        output.stdout
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("read protected target"),
        "{\"protected\":true}\n"
    );
}

#[test]
fn e2e_vcs_status_redacts_external_paths_from_machine_and_human_output() {
    let _log =
        common::test_log("e2e_vcs_status_redacts_external_paths_from_machine_and_human_output");
    let workspace = BrWorkspace::new();
    git_ok(&workspace.root, &["init", "--initial-branch=main"]);
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let secret_name = "tenant-very-secret-export.jsonl";
    let external = workspace.root.join(secret_name);
    std::fs::write(&external, "{\"id\":\"bd-redacted\"}\n").expect("external JSONL");
    let external_text = external.to_string_lossy().into_owned();

    let machine = run_br(
        &workspace,
        [
            "vcs-status",
            "--jsonl",
            external_text.as_str(),
            "--allow-external-jsonl",
            "--json",
        ],
        "external_machine",
    );
    assert!(machine.status.success(), "{}", machine.stderr);
    assert!(!machine.stdout.contains(secret_name), "{}", machine.stdout);
    assert!(!machine.stderr.contains(secret_name), "{}", machine.stderr);
    let value: Value =
        serde_json::from_str(&extract_json_payload(&machine.stdout)).expect("external JSON");
    assert_eq!(value["path_scope"], "external", "{value}");
    let label = value["path"].as_str().expect("redacted path label");
    assert!(label.starts_with("<external-jsonl sha256="), "{value}");
    assert!(label.ends_with('>'), "{value}");

    let human = run_br(
        &workspace,
        [
            "vcs-status",
            "--jsonl",
            external_text.as_str(),
            "--allow-external-jsonl",
        ],
        "external_human",
    );
    assert!(human.status.success(), "{}", human.stderr);
    assert!(!human.stdout.contains(secret_name), "{}", human.stdout);
    assert!(!human.stderr.contains(secret_name), "{}", human.stderr);
    assert!(
        human.stdout.contains("Path scope: external"),
        "{}",
        human.stdout
    );
    assert!(
        human.stdout.contains("<external-jsonl sha256="),
        "{}",
        human.stdout
    );
}

fn assert_external_capture_failure_is_redacted(
    workspace: &BrWorkspace,
    path: &Path,
    secret_fragment: &str,
    label: &str,
) {
    let path_text = path.to_string_lossy().into_owned();
    let output = run_br(
        workspace,
        [
            "vcs-status",
            "--jsonl",
            path_text.as_str(),
            "--allow-external-jsonl",
            "--json",
        ],
        label,
    );
    assert!(
        !output.status.success(),
        "{label} must reject an unsafe source leaf"
    );
    assert!(
        !output.stdout.contains(secret_fragment),
        "{label} leaked the external basename in stdout: {}",
        output.stdout
    );
    assert!(
        !output.stderr.contains(secret_fragment),
        "{label} leaked the external basename in stderr: {}",
        output.stderr
    );
    assert!(
        output.stdout.contains("<external-path sha256="),
        "{label} omitted the redacted path fingerprint from the structured error: {}",
        output.stdout
    );
}

#[test]
fn e2e_vcs_status_redacts_authorized_external_directory_capture_failure() {
    let _log =
        common::test_log("e2e_vcs_status_redacts_authorized_external_directory_capture_failure");
    let workspace = tracked_workspace();
    let secret = "tenant-secret-directory.jsonl";
    let directory = workspace.root.join(secret);
    std::fs::create_dir(&directory).expect("external directory fixture");
    assert_external_capture_failure_is_redacted(
        &workspace,
        &directory,
        secret,
        "external_directory_redaction",
    );
}

#[cfg(unix)]
#[test]
fn e2e_vcs_status_redacts_authorized_external_symlink_capture_failure() {
    use std::os::unix::fs::symlink;

    let _log =
        common::test_log("e2e_vcs_status_redacts_authorized_external_symlink_capture_failure");
    let workspace = tracked_workspace();
    let target = workspace.root.join("external-symlink-target");
    std::fs::write(&target, "protected\n").expect("external symlink target");
    let secret = "tenant-secret-symlink.jsonl";
    let link = workspace.root.join(secret);
    symlink(&target, &link).expect("external symlink fixture");
    assert_external_capture_failure_is_redacted(
        &workspace,
        &link,
        secret,
        "external_symlink_redaction",
    );
}

#[cfg(unix)]
#[test]
fn e2e_vcs_status_redacts_authorized_external_unreadable_capture_failure_when_enforced() {
    use std::os::unix::fs::PermissionsExt;

    let _log = common::test_log(
        "e2e_vcs_status_redacts_authorized_external_unreadable_capture_failure_when_enforced",
    );
    let workspace = tracked_workspace();
    let secret = "tenant-secret-unreadable.jsonl";
    let path = workspace.root.join(secret);
    std::fs::write(&path, "{\"protected\":true}\n").expect("external unreadable fixture");
    let mut permissions = std::fs::metadata(&path)
        .expect("unreadable fixture metadata")
        .permissions();
    permissions.set_mode(0o000);
    std::fs::set_permissions(&path, permissions).expect("remove fixture read permissions");

    if std::fs::File::open(&path).is_err() {
        assert_external_capture_failure_is_redacted(
            &workspace,
            &path,
            secret,
            "external_unreadable_redaction",
        );
    }

    let mut permissions = std::fs::metadata(&path)
        .expect("unreadable fixture metadata after probe")
        .permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(&path, permissions).expect("restore fixture permissions");
}
