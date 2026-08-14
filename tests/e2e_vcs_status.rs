//! End-to-end contract tests for the explicit, bounded `br vcs-status`
//! diagnostic. These tests intentionally live outside sync safety coverage:
//! VCS process authority is opt-in and isolated to this command.

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
    let linked_home = workspace.root.join("linked-global-home");
    std::fs::create_dir(&linked_home).expect("linked-worktree global config HOME");
    std::fs::write(
        linked_home.join(".gitconfig"),
        "[core]\n\tautocrlf = true\n\tfilemode = true\n",
    )
    .expect("linked-worktree global config");

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
        [("HOME", linked_home.as_os_str())],
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
        [("HOME", linked_home.as_os_str())],
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
        [("HOME", linked_home.as_os_str())],
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
