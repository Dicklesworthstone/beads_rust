mod common;

use beads_rust::model::{Comment, Dependency, DependencyType, Issue, IssueType, Priority, Status};
use beads_rust::storage::SqliteStorage;
use chrono::Utc;
use common::cli::{
    BrRun, BrWorkspace, extract_json_payload, parse_json_value, parse_list_issues, run_br,
    run_br_smoke_at_root_with_env,
};
use common::isolated_workspace_failure_fixture;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::thread::sleep;
use std::time::Duration;

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    // Handle both formats: "Created bd-xxx: title" and "✓ Created bd-xxx: title"
    let normalized = line
        .strip_prefix("✓ ")
        .or_else(|| line.strip_prefix("✗ "))
        .unwrap_or(line);
    let id_part = normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

fn make_issue(id: &str, title: &str, now: chrono::DateTime<Utc>) -> Issue {
    Issue {
        id: id.to_string(),
        title: title.to_string(),
        status: Status::Open,
        priority: Priority::MEDIUM,
        issue_type: IssueType::Task,
        created_at: now,
        updated_at: now,
        content_hash: None,
        description: None,
        design: None,
        acceptance_criteria: None,
        notes: None,
        assignee: None,
        owner: None,
        estimated_minutes: None,
        created_by: None,
        closed_at: None,
        close_reason: None,
        closed_by_session: None,
        due_at: None,
        defer_until: None,
        external_ref: None,
        source_system: None,
        source_repo: None,
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

fn dotted_parent_child_dependency(
    issue_id: &str,
    depends_on_id: &str,
    now: chrono::DateTime<Utc>,
) -> Dependency {
    Dependency {
        issue_id: issue_id.to_string(),
        depends_on_id: depends_on_id.to_string(),
        dep_type: DependencyType::ParentChild,
        created_at: now,
        created_by: Some("tester".to_string()),
        metadata: Some("{}".to_string()),
        thread_id: None,
    }
}

fn write_dotted_jsonl_fixture(workspace: &BrWorkspace) -> PathBuf {
    let beads_dir = workspace.root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let now = Utc::now();

    let parent = make_issue("bd-rchk0.5", "Dotted parent", now);
    let mut target = make_issue("bd-rchk0.5.6", "Dotted target", now);
    target
        .dependencies
        .push(dotted_parent_child_dependency(&target.id, &parent.id, now));
    let mut child = make_issue("bd-rchk0.5.6.1", "Dotted child", now);
    child
        .dependencies
        .push(dotted_parent_child_dependency(&child.id, &target.id, now));
    let blocker = make_issue("bd-blocker7", "Dotted blocker", now);

    let records = [&parent, &target, &child, &blocker]
        .into_iter()
        .map(|issue| serde_json::to_string(issue).expect("serialize dotted fixture"))
        .collect::<Vec<_>>();
    fs::write(&jsonl_path, records.join("\n") + "\n").expect("write dotted jsonl");
    jsonl_path
}

fn assert_br_success(run: &BrRun, context: &str) {
    assert!(run.status.success(), "{context}: {}", run.stderr);
}

fn parse_json_array(stdout: &str, context: &str) -> Vec<Value> {
    serde_json::from_str(&extract_json_payload(stdout)).expect(context)
}

fn read_jsonl_values(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("read jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("parse exported issue"))
        .collect()
}

fn prepare_merge_conflict_workspace() -> (BrWorkspace, String) {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_merge_conflict");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Merge conflict"],
        "create_merge_seed",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush_merge_conflict");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let base_snapshot_path = workspace.root.join(".beads").join("beads.base.jsonl");
    fs::copy(&jsonl_path, &base_snapshot_path).expect("seed base snapshot");

    let local_update = run_br(
        &workspace,
        [
            "update",
            &issue_id,
            "--description",
            "Local description",
            "--no-auto-flush",
        ],
        "local_merge_update",
    );
    assert!(
        local_update.status.success(),
        "local update failed: {}",
        local_update.stderr
    );

    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let mut rewritten = Vec::new();
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        let mut issue: Value = serde_json::from_str(line).expect("parse issue jsonl");
        if issue["id"].as_str() == Some(issue_id.as_str()) {
            issue["description"] = Value::String("External description".to_string());
            issue["updated_at"] = Value::String("2999-01-01T00:00:00Z".to_string());
        }
        rewritten.push(serde_json::to_string(&issue).expect("serialize issue jsonl"));
    }
    fs::write(&jsonl_path, rewritten.join("\n") + "\n").expect("write jsonl");

    (workspace, issue_id)
}

fn assert_issue_description(workspace: &BrWorkspace, issue_id: &str, expected: &str) {
    let show = run_br(workspace, ["show", issue_id, "--json"], "show_merge_result");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("parse show json");
    assert_eq!(issues[0]["description"].as_str(), Some(expected));
}

fn clear_br_env_for_std_command(cmd: &mut StdCommand) {
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy();
        if key.starts_with("BD_")
            || key.starts_with("BEADS_")
            || matches!(
                key.as_ref(),
                "BR_DISABLE_READ_ONLY_FAST_OPEN"
                    | "BR_OUTPUT_FORMAT"
                    | "TOON_DEFAULT_FORMAT"
                    | "TOON_STATS"
            )
        {
            cmd.env_remove(key.as_ref());
        }
    }
}

#[test]
fn e2e_basic_lifecycle() {
    let _log = common::test_log("e2e_basic_lifecycle");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);
    assert!(!id.is_empty(), "missing created id");

    let update_args = vec![
        "update".to_string(),
        id.clone(),
        "--status".to_string(),
        "in_progress".to_string(),
        "--priority".to_string(),
        "1".to_string(),
        "--assignee".to_string(),
        "alice".to_string(),
    ];
    let update = run_br(&workspace, update_args, "update");
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let list = run_br(&workspace, ["list", "--json"], "list");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let list_json = parse_list_issues(&list.stdout);
    assert!(
        list_json
            .iter()
            .any(|item| item["id"] == id && item["status"] == "in_progress"),
        "updated issue not found in list"
    );

    let list_text = run_br(&workspace, ["list"], "list_text");
    assert!(
        list_text.status.success(),
        "list text failed: {}",
        list_text.stderr
    );
    assert!(
        list_text.stdout.contains("Test issue"),
        "list text missing issue title"
    );

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let show_payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&show_payload).expect("show json");
    assert_eq!(show_json[0]["id"], id);

    let show_text = run_br(&workspace, ["show", &id], "show_text");
    assert!(
        show_text.status.success(),
        "show text failed: {}",
        show_text.stderr
    );
    assert!(
        show_text.stdout.contains("Test issue"),
        "show text missing title"
    );

    let close_args = vec![
        "update".to_string(),
        id,
        "--status".to_string(),
        "closed".to_string(),
    ];
    let close = run_br(&workspace, close_args, "close");
    assert!(close.status.success(), "close failed: {}", close.stderr);
}

#[test]
#[cfg(target_os = "linux")]
fn json_stdout_write_failure_exits_with_io_error() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_stdout_failure");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "stdout failure probe"],
        "create_stdout_failure",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let dev_full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("open /dev/full");
    let mut cmd = StdCommand::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(&workspace.root);
    cmd.args(["list", "--json", "--no-auto-import", "--no-auto-flush"]);
    clear_br_env_for_std_command(&mut cmd);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_LOG", "beads_rust=debug");
    cmd.env("RUST_BACKTRACE", "1");
    cmd.env("HOME", &workspace.root);
    cmd.stdout(Stdio::from(dev_full));
    cmd.stderr(Stdio::piped());

    let output = cmd.output().expect("run br with /dev/full stdout");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(8),
        "stdout write failure should exit as I/O error; stderr={stderr}"
    );
    assert!(
        stderr.contains("failed to serialize JSON output"),
        "stderr should report the output serialization failure: {stderr}"
    );
}

#[test]
fn e2e_non_hermetic_smoke_existing_workspace_preserves_env_sensitive_paths() {
    let _log =
        common::test_log("e2e_non_hermetic_smoke_existing_workspace_preserves_env_sensitive_paths");
    let fixture = isolated_workspace_failure_fixture("metadata_custom_paths")
        .expect("metadata_custom_paths fixture");

    let runner_root = fixture.root.join("ambient-env-smoke");
    fs::create_dir_all(&runner_root).expect("create smoke runner root");

    let external_beads_dir = fixture.root.join(".beads");
    let external_beads_dir_str = external_beads_dir.display().to_string();
    let custom_db_str = external_beads_dir.join("custom.db").display().to_string();
    let custom_jsonl_str = external_beads_dir
        .join("custom.jsonl")
        .display()
        .to_string();
    let smoke_env = || {
        vec![
            ("BEADS_DIR".to_string(), external_beads_dir_str.clone()),
            ("BR_OUTPUT_FORMAT".to_string(), "json".to_string()),
        ]
    };

    let where_cmd = run_br_smoke_at_root_with_env(
        &runner_root,
        ["where"],
        smoke_env(),
        "non_hermetic_where_existing_workspace",
    );
    assert!(
        where_cmd.status.success(),
        "where smoke failed: {}",
        where_cmd.stderr
    );
    let where_json: Value =
        serde_json::from_str(&extract_json_payload(&where_cmd.stdout)).expect("where smoke json");
    assert_eq!(
        where_json["path"].as_str(),
        Some(external_beads_dir_str.as_str())
    );
    assert_eq!(
        where_json["database_path"].as_str(),
        Some(custom_db_str.as_str())
    );
    assert_eq!(
        where_json["jsonl_path"].as_str(),
        Some(custom_jsonl_str.as_str())
    );

    let info_cmd = run_br_smoke_at_root_with_env(
        &runner_root,
        ["info"],
        smoke_env(),
        "non_hermetic_info_existing_workspace",
    );
    assert!(
        info_cmd.status.success(),
        "info smoke failed: {}",
        info_cmd.stderr
    );
    let info_json: Value =
        serde_json::from_str(&extract_json_payload(&info_cmd.stdout)).expect("info smoke json");
    assert_eq!(
        info_json["beads_dir"].as_str(),
        Some(external_beads_dir_str.as_str())
    );
    assert_eq!(
        info_json["database_path"].as_str(),
        Some(custom_db_str.as_str())
    );
    assert_eq!(
        info_json["jsonl_path"].as_str(),
        Some(custom_jsonl_str.as_str())
    );
    assert!(
        info_json["issue_count"].as_u64().is_some(),
        "info smoke should report issue_count: {info_json}"
    );

    let sync_status_cmd = run_br_smoke_at_root_with_env(
        &runner_root,
        ["sync", "--status"],
        smoke_env(),
        "non_hermetic_sync_status_existing_workspace",
    );
    assert!(
        sync_status_cmd.status.success(),
        "sync --status smoke failed: {}",
        sync_status_cmd.stderr
    );
    let sync_status_json: Value =
        serde_json::from_str(&extract_json_payload(&sync_status_cmd.stdout))
            .expect("sync status smoke json");
    assert_eq!(sync_status_json["jsonl_exists"].as_bool(), Some(true));
}

#[test]
fn e2e_update_claim_multiple_ids_is_all_or_nothing() {
    let _log = common::test_log("e2e_update_claim_multiple_ids_is_all_or_nothing");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_claim_multiple_ids");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create_first = run_br(
        &workspace,
        ["create", "First claim target", "--json"],
        "create_first_claim_target",
    );
    assert!(
        create_first.status.success(),
        "first create failed: {}",
        create_first.stderr
    );
    let first_issue: Value = serde_json::from_str(&extract_json_payload(&create_first.stdout))
        .expect("first create json");
    let first_id = first_issue["id"]
        .as_str()
        .expect("first issue id")
        .to_string();

    let create_second = run_br(
        &workspace,
        ["create", "Second claim target", "--json"],
        "create_second_claim_target",
    );
    assert!(
        create_second.status.success(),
        "second create failed: {}",
        create_second.stderr
    );
    let second_issue: Value = serde_json::from_str(&extract_json_payload(&create_second.stdout))
        .expect("second create json");
    let second_id = second_issue["id"]
        .as_str()
        .expect("second issue id")
        .to_string();

    let claim_second = run_br(
        &workspace,
        ["--actor", "bob", "update", &second_id, "--claim", "--json"],
        "claim_second_issue_bob",
    );
    assert!(
        claim_second.status.success(),
        "claim second failed: {}",
        claim_second.stderr
    );

    let claim_both = run_br(
        &workspace,
        [
            "--actor", "alice", "update", &first_id, &second_id, "--claim", "--json",
        ],
        "claim_multiple_ids_atomic",
    );
    assert!(
        !claim_both.status.success(),
        "expected multi-id claim to fail when one issue is already assigned"
    );

    let show_first = run_br(
        &workspace,
        ["show", &first_id, "--json"],
        "show_first_after_failed_multi_claim",
    );
    assert!(
        show_first.status.success(),
        "show first failed: {}",
        show_first.stderr
    );
    let first_after: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_first.stdout)).expect("show first json");
    assert_eq!(first_after[0]["status"].as_str(), Some("open"));
    assert!(first_after[0]["assignee"].is_null());

    let show_second = run_br(
        &workspace,
        ["show", &second_id, "--json"],
        "show_second_after_failed_multi_claim",
    );
    assert!(
        show_second.status.success(),
        "show second failed: {}",
        show_second.stderr
    );
    let second_after: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_second.stdout)).expect("show second json");
    assert_eq!(second_after[0]["status"].as_str(), Some("in_progress"));
    assert_eq!(second_after[0]["assignee"].as_str(), Some("bob"));
}

#[test]
fn e2e_create_updates_last_touched_context() {
    let _log = common::test_log("e2e_create_updates_last_touched_context");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_create_last_touched");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Create updates last touched"],
        "create_last_touched",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created_id = parse_created_id(&create.stdout);
    assert!(!created_id.is_empty(), "missing created id");

    let update = run_br(
        &workspace,
        ["update", "--status", "in_progress"],
        "update_last_touched_after_create",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let show = run_br(
        &workspace,
        ["show", &created_id, "--json"],
        "show_last_touched_after_create",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("show json");
    assert_eq!(json[0]["status"], "in_progress");
}

#[test]
fn e2e_create_dry_run_does_not_update_last_touched_context() {
    let _log = common::test_log("e2e_create_dry_run_does_not_update_last_touched_context");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_create_dry_run_last_touched");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let seed = run_br(
        &workspace,
        ["create", "Seed for dry-run last touched"],
        "seed_create_dry_run_last_touched",
    );
    assert!(seed.status.success(), "seed create failed: {}", seed.stderr);
    let seed_id = parse_created_id(&seed.stdout);
    assert!(!seed_id.is_empty(), "missing seed id");

    let dry_run = run_br(
        &workspace,
        [
            "create",
            "Dry-run should not move last touched",
            "--dry-run",
        ],
        "create_dry_run_last_touched",
    );
    assert!(
        dry_run.status.success(),
        "dry-run create failed: {}",
        dry_run.stderr
    );

    let update = run_br(
        &workspace,
        ["update", "--status", "in_progress"],
        "update_after_create_dry_run",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let show = run_br(
        &workspace,
        ["show", &seed_id, "--json"],
        "show_after_create_dry_run",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("show json");
    assert_eq!(json[0]["status"], "in_progress");
}

#[test]
fn e2e_no_db_create_updates_last_touched_after_flush() {
    let _log = common::test_log("e2e_no_db_create_updates_last_touched_after_flush");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_no_db_create_last_touched");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let seed = run_br(
        &workspace,
        ["create", "Seed issue"],
        "seed_no_db_create_last_touched",
    );
    assert!(seed.status.success(), "seed create failed: {}", seed.stderr);

    let sync = run_br(
        &workspace,
        ["sync", "--flush-only"],
        "sync_no_db_create_last_touched",
    );
    assert!(sync.status.success(), "sync failed: {}", sync.stderr);

    let create = run_br(
        &workspace,
        ["--no-db", "create", "No DB create updates last touched"],
        "create_no_db_last_touched",
    );
    assert!(
        create.status.success(),
        "no-db create failed: {}",
        create.stderr
    );
    let created_id = parse_created_id(&create.stdout);
    assert!(!created_id.is_empty(), "missing created id");

    let update = run_br(
        &workspace,
        ["update", "--status", "in_progress"],
        "update_last_touched_after_no_db_create",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let show = run_br(
        &workspace,
        ["show", &created_id, "--json"],
        "show_last_touched_after_no_db_create",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("show json");
    assert_eq!(json[0]["status"], "in_progress");
}

#[test]
fn e2e_quick_capture() {
    let _log = common::test_log("e2e_quick_capture");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(&workspace, ["q", "Quick", "issue"], "quick");
    assert!(quick.status.success(), "quick failed: {}", quick.stderr);

    let quick_id = quick.stdout.lines().next().unwrap_or("").trim().to_string();
    assert!(!quick_id.is_empty(), "missing quick id");
    assert!(quick_id.contains('-'), "unexpected quick id format");
}

#[test]
fn e2e_sync_roundtrip() {
    let _log = common::test_log("e2e_sync_roundtrip");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Original title", "--no-auto-flush"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);
    assert!(!id.is_empty(), "missing created id");

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);
    assert!(
        sync.stdout.contains("Exported"),
        "sync flush text missing export message"
    );

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    assert!(jsonl_path.exists(), "issues.jsonl missing after flush");
    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    // Parse and update the issue properly (title + timestamp for last-write-wins)
    let mut updated_lines = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut issue: Value = serde_json::from_str(line).expect("parse issue");
        if issue["title"] == "Original title" {
            issue["title"] = Value::String("Modified title".to_string());
            // Bump updated_at to ensure import sees it as newer
            issue["updated_at"] = Value::String(Utc::now().to_rfc3339());
        }
        updated_lines.push(serde_json::to_string(&issue).expect("serialize issue"));
    }
    fs::write(&jsonl_path, updated_lines.join("\n") + "\n").expect("write jsonl");
    let expected_jsonl = fs::read(&jsonl_path).expect("read edited jsonl bytes");

    sleep(Duration::from_millis(50));

    let sync_import = run_br(&workspace, ["sync", "--import-only"], "sync_import");
    assert!(
        sync_import.status.success(),
        "sync import failed: {}",
        sync_import.stderr
    );
    let post_import_jsonl = fs::read(&jsonl_path).expect("read jsonl after import");
    assert_eq!(
        post_import_jsonl, expected_jsonl,
        "sync --import-only must not rewrite issues.jsonl"
    );

    let show = run_br(&workspace, ["show", &id, "--json"], "show_after_import");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&payload).expect("show json");
    assert_eq!(show_json[0]["title"], "Modified title");
}

#[test]
fn e2e_sync_import_staleness_and_force() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Stale issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush_stale");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    let import_first = run_br(&workspace, ["sync", "--import-only"], "sync_import_first");
    assert!(
        import_first.status.success(),
        "sync import first failed: {}",
        import_first.stderr
    );

    let import_skip = run_br(&workspace, ["sync", "--import-only"], "sync_import_skip");
    assert!(
        import_skip.status.success(),
        "sync import skip failed: {}",
        import_skip.stderr
    );
    assert!(
        import_skip
            .stdout
            .contains("JSONL is current (hash unchanged since last import)"),
        "sync import skip missing current message"
    );

    let import_force = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "sync_import_force",
    );
    assert!(
        import_force.status.success(),
        "sync import force failed: {}",
        import_force.stderr
    );
    assert!(
        import_force.stdout.contains("Imported from JSONL"),
        "sync import force missing header"
    );
    assert!(
        import_force.stdout.contains("Processed: 1 issues"),
        "sync import force missing processed count"
    );
}

#[test]
fn e2e_sync_merge_resolution_flags_choose_db_or_jsonl() {
    let (jsonl_workspace, jsonl_issue_id) = prepare_merge_conflict_workspace();
    let manual = run_br(&jsonl_workspace, ["sync", "--merge"], "merge_manual");
    assert!(
        !manual.status.success(),
        "manual merge should report conflict: stdout={} stderr={}",
        manual.stdout,
        manual.stderr
    );
    assert!(
        manual.stderr.contains("BothModified")
            && manual.stderr.contains("--force-db")
            && manual.stderr.contains("--force-jsonl"),
        "manual conflict should explain explicit resolution flags: {}",
        manual.stderr
    );

    let force_jsonl = run_br(
        &jsonl_workspace,
        ["sync", "--merge", "--force-jsonl", "--json"],
        "merge_force_jsonl",
    );
    assert!(
        force_jsonl.status.success(),
        "force-jsonl merge failed: {}",
        force_jsonl.stderr
    );
    assert_issue_description(&jsonl_workspace, &jsonl_issue_id, "External description");

    let (db_workspace, db_issue_id) = prepare_merge_conflict_workspace();
    let force_db = run_br(
        &db_workspace,
        ["sync", "--merge", "--force-db", "--json"],
        "merge_force_db",
    );
    assert!(
        force_db.status.success(),
        "force-db merge failed: {}",
        force_db.stderr
    );
    assert_issue_description(&db_workspace, &db_issue_id, "Local description");
}

#[test]
fn e2e_sync_merge_from_jsonl_imports_worker_file_into_no_db_workspace() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_no_db_merge_from");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let beads_dir = workspace.root.join(".beads");
    fs::write(beads_dir.join("config.yaml"), "no-db: true\n").expect("write no-db config");

    let create = run_br(
        &workspace,
        ["create", "Canonical issue before worker merge"],
        "create_no_db_canonical_issue",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let jsonl_path = beads_dir.join("issues.jsonl");
    let local_jsonl_before = fs::read_to_string(&jsonl_path).expect("read local jsonl");
    let first_line = local_jsonl_before
        .lines()
        .find(|line| !line.trim().is_empty())
        .expect("local jsonl issue");
    let mut worker_issue: Value = serde_json::from_str(first_line).expect("parse issue");
    let original_id = worker_issue["id"].as_str().expect("issue id");
    let worker_id = format!("{original_id}w");
    worker_issue["id"] = Value::String(worker_id.clone());
    worker_issue["title"] = Value::String("Worker-created bead".to_string());
    worker_issue["content_hash"] = Value::Null;
    worker_issue["created_at"] = Value::String(Utc::now().to_rfc3339());
    worker_issue["updated_at"] = Value::String(Utc::now().to_rfc3339());

    let worker_jsonl_path = workspace.root.join("worker-issues.jsonl");
    let worker_issue_line = serde_json::to_string(&worker_issue).expect("serialize worker issue");
    let worker_jsonl = format!("{local_jsonl_before}{worker_issue_line}\n");
    fs::write(&worker_jsonl_path, &worker_jsonl).expect("write worker jsonl");

    let worker_jsonl_arg = worker_jsonl_path.to_string_lossy().to_string();
    let rejected = run_br(
        &workspace,
        vec![
            "sync".to_string(),
            "--merge".to_string(),
            "--merge-from-jsonl".to_string(),
            worker_jsonl_arg.clone(),
            "--json".to_string(),
        ],
        "merge_from_external_without_opt_in",
    );
    assert!(
        !rejected.status.success(),
        "external merge source should require explicit opt-in"
    );
    assert!(
        rejected.stderr.contains("outside .beads") || rejected.stdout.contains("outside .beads"),
        "rejection should mention external path: stdout={} stderr={}",
        rejected.stdout,
        rejected.stderr
    );

    let merge = run_br(
        &workspace,
        vec![
            "sync".to_string(),
            "--merge".to_string(),
            "--merge-from-jsonl".to_string(),
            worker_jsonl_arg,
            "--allow-external-jsonl".to_string(),
            "--json".to_string(),
        ],
        "merge_from_worker_jsonl",
    );
    assert!(merge.status.success(), "merge failed: {}", merge.stderr);
    let merge_payload = extract_json_payload(&merge.stdout);
    let merge_json: Value = serde_json::from_str(&merge_payload).expect("merge json");
    assert_eq!(merge_json["status"], "success");
    assert_eq!(
        fs::canonicalize(PathBuf::from(
            merge_json["target_jsonl"].as_str().expect("target_jsonl")
        ))
        .expect("canonical target jsonl"),
        fs::canonicalize(&jsonl_path).expect("canonical expected target")
    );
    assert_eq!(
        fs::canonicalize(PathBuf::from(
            merge_json["merge_input"].as_str().expect("merge_input")
        ))
        .expect("canonical merge input"),
        fs::canonicalize(&worker_jsonl_path).expect("canonical expected input")
    );

    let local_jsonl_after = fs::read_to_string(&jsonl_path).expect("read merged local jsonl");
    assert!(
        local_jsonl_after
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|issue| issue["id"].as_str() == Some(worker_id.as_str())),
        "worker issue should be merged into the local JSONL"
    );
    assert_eq!(
        fs::read_to_string(&worker_jsonl_path).expect("read worker jsonl"),
        worker_jsonl,
        "merge source must not be overwritten"
    );

    let show = run_br(
        &workspace,
        ["show", worker_id.as_str(), "--json"],
        "show_worker_issue_after_merge",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let show_payload = extract_json_payload(&show.stdout);
    let shown: Vec<Value> = serde_json::from_str(&show_payload).expect("show json");
    assert_eq!(shown[0]["title"], "Worker-created bead");
}

#[test]
fn e2e_sync_force_jsonl_merge_does_not_resurrect_local_tombstone() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_tombstone_merge");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Merge tombstone seed"],
        "create_tombstone_merge",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);
    assert!(!issue_id.is_empty(), "missing created id");

    let flush = run_br(
        &workspace,
        ["sync", "--flush-only"],
        "flush_tombstone_merge",
    );
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let base_snapshot_path = beads_dir.join("beads.base.jsonl");
    fs::copy(&jsonl_path, &base_snapshot_path).expect("seed base snapshot");

    let delete = run_br(
        &workspace,
        [
            "delete",
            &issue_id,
            "--force",
            "--reason",
            "local tombstone before merge",
            "--no-auto-flush",
        ],
        "delete_local_tombstone",
    );
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);

    let jsonl = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let mut issue: Value = serde_json::from_str(jsonl.trim()).expect("parse jsonl issue");
    issue["title"] = Value::String("JSONL resurrection attempt".to_string());
    issue["status"] = Value::String("open".to_string());
    issue["updated_at"] = Value::String("2999-01-01T00:00:00Z".to_string());
    fs::write(
        &jsonl_path,
        format!(
            "{}\n",
            serde_json::to_string(&issue).expect("serialize issue")
        ),
    )
    .expect("write resurrection jsonl");

    let merge = run_br(
        &workspace,
        ["sync", "--merge", "--force-jsonl", "--json"],
        "merge_force_jsonl_tombstone",
    );
    assert!(
        merge.status.success(),
        "force-jsonl merge failed: stdout={} stderr={}",
        merge.stdout,
        merge.stderr
    );

    let show = run_br(
        &workspace,
        ["show", &issue_id, "--json"],
        "show_tombstone_merge",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("parse show json");
    assert_eq!(
        issues[0]["status"].as_str(),
        Some("tombstone"),
        "force-jsonl merge must not resurrect a local tombstone"
    );
    assert_ne!(
        issues[0]["title"].as_str(),
        Some("JSONL resurrection attempt"),
        "resurrection attempt should not win the merge"
    );

    let merged_jsonl = fs::read_to_string(&jsonl_path).expect("read merged jsonl");
    assert!(
        merged_jsonl.contains("\"status\":\"tombstone\""),
        "merged JSONL should export the protected tombstone: {merged_jsonl}"
    );
}

#[test]
fn e2e_no_db_read_write() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Seed issue"], "create_seed");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    assert!(jsonl_path.exists(), "issues.jsonl missing");

    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let mut issues: Vec<Value> = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("parse jsonl"))
        .collect();
    assert!(!issues.is_empty(), "seed jsonl empty");

    let now = Utc::now().to_rfc3339();
    let mut injected = issues[0].clone();
    injected["id"] = Value::String("bd-nodb1".to_string());
    injected["title"] = Value::String("Injected no-db".to_string());
    injected["created_at"] = Value::String(now.clone());
    injected["updated_at"] = Value::String(now);
    issues.push(injected);

    let rewritten: Vec<String> = issues
        .into_iter()
        .map(|issue| serde_json::to_string(&issue).expect("serialize jsonl"))
        .collect();
    fs::write(&jsonl_path, rewritten.join("\n") + "\n").expect("write jsonl");

    let list = run_br(&workspace, ["--no-db", "list", "--json"], "list_no_db");
    assert!(
        list.status.success(),
        "list --no-db failed: {}",
        list.stderr
    );
    let list_json = parse_list_issues(&list.stdout);
    assert!(
        list_json.iter().any(|item| item["id"] == "bd-nodb1"),
        "no-db list missing injected issue"
    );

    let create_no_db = run_br(
        &workspace,
        ["--no-db", "create", "No DB create"],
        "create_no_db",
    );
    assert!(
        create_no_db.status.success(),
        "create --no-db failed: {}",
        create_no_db.stderr
    );
    let created_id = parse_created_id(&create_no_db.stdout);
    assert!(!created_id.is_empty(), "no-db create missing id");

    let updated = fs::read_to_string(&jsonl_path).expect("read jsonl after no-db");
    assert!(
        updated.contains("No DB create"),
        "no-db create did not update JSONL"
    );
}

#[test]
fn e2e_no_db_mixed_prefixes_are_supported() {
    let workspace = BrWorkspace::new();
    let beads_dir = workspace.root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");
    let jsonl_path = beads_dir.join("issues.jsonl");

    let now = Utc::now();
    let issue_a = make_issue("aa-abc", "Alpha issue", now);
    let issue_b = make_issue("bb-def", "Beta issue", now);
    let lines = [
        serde_json::to_string(&issue_a).expect("serialize issue a"),
        serde_json::to_string(&issue_b).expect("serialize issue b"),
    ];
    fs::write(&jsonl_path, lines.join("\n") + "\n").expect("write jsonl");

    let list = run_br(
        &workspace,
        ["--no-db", "list", "--json"],
        "list_no_db_mixed",
    );
    assert!(
        list.status.success(),
        "list --no-db should accept mixed prefixes: {}",
        list.stderr
    );

    let issues = parse_list_issues(&list.stdout);
    let ids: Vec<&str> = issues
        .iter()
        .filter_map(|issue| issue["id"].as_str())
        .collect();
    assert!(ids.contains(&"aa-abc"), "expected aa-abc in {ids:?}");
    assert!(ids.contains(&"bb-def"), "expected bb-def in {ids:?}");
}

#[test]
fn e2e_dotted_ids_survive_no_db_import_update_dep_and_flush() {
    let workspace = BrWorkspace::new();
    let jsonl_path = write_dotted_jsonl_fixture(&workspace);

    let no_db_show = run_br(
        &workspace,
        ["--no-db", "show", "bd-rchk0.5.6", "--json"],
        "dotted_no_db_show",
    );
    assert_br_success(&no_db_show, "no-db show failed for dotted id");
    let shown = parse_json_array(&no_db_show.stdout, "show json");
    assert_eq!(shown[0]["id"].as_str(), Some("bd-rchk0.5.6"));

    let no_db_update = run_br(
        &workspace,
        [
            "--no-db",
            "update",
            "bd-rchk0.5.6",
            "--priority",
            "1",
            "--json",
        ],
        "dotted_no_db_update",
    );
    assert_br_success(&no_db_update, "no-db update failed for dotted id");
    let updated = parse_json_array(&no_db_update.stdout, "update json");
    assert_eq!(updated[0]["id"].as_str(), Some("bd-rchk0.5.6"));
    assert_eq!(updated[0]["priority"].as_i64(), Some(1));

    let imported = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "dotted_import",
    );
    assert_br_success(&imported, "import failed for dotted ids");
    let import_json = parse_json_value(&imported.stdout);
    assert_eq!(import_json["created"].as_i64(), Some(4));

    let db_show = run_br(
        &workspace,
        ["show", "bd-rchk0.5.6", "--json"],
        "dotted_db_show",
    );
    assert_br_success(&db_show, "db show failed for dotted id");
    let db_show_json = parse_json_array(&db_show.stdout, "db show json");
    assert_eq!(db_show_json[0]["id"].as_str(), Some("bd-rchk0.5.6"));
    assert!(
        db_show_json[0]["dependents"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(|item| item["id"].as_str() == Some("bd-rchk0.5.6.1"))),
        "dotted child dependent should resolve to the exact parent"
    );

    let db_update = run_br(
        &workspace,
        [
            "--no-auto-flush",
            "update",
            "bd-rchk0.5.6",
            "--priority",
            "0",
            "--json",
        ],
        "dotted_db_update",
    );
    assert_br_success(&db_update, "db update failed for dotted id");
    let db_update_json = parse_json_array(&db_update.stdout, "db update json");
    assert_eq!(db_update_json[0]["id"].as_str(), Some("bd-rchk0.5.6"));
    assert_eq!(db_update_json[0]["priority"].as_i64(), Some(0));

    let dep_add = run_br(
        &workspace,
        [
            "--no-auto-flush",
            "dep",
            "add",
            "bd-rchk0.5.6",
            "bd-blocker7",
            "--json",
        ],
        "dotted_dep_add",
    );
    assert_br_success(&dep_add, "dep add failed for dotted id");

    let flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--json"],
        "dotted_flush",
    );
    assert_br_success(&flush, "flush failed after dotted mutations");

    let exported_issues = read_jsonl_values(&jsonl_path);
    assert_eq!(exported_issues.len(), 4);
    let exported_target = exported_issues
        .iter()
        .find(|issue| issue["id"].as_str() == Some("bd-rchk0.5.6"))
        .expect("exported dotted target");
    assert_eq!(exported_target["priority"].as_i64(), Some(0));
    assert!(
        exported_target["dependencies"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(|item| item["depends_on_id"].as_str() == Some("bd-blocker7"))),
        "exported dotted target should retain the added dependency"
    );
}

#[test]
fn e2e_no_db_mutations_succeed_with_large_export_hash_batches() {
    let _log = common::test_log("e2e_no_db_mutations_succeed_with_large_export_hash_batches");
    let workspace = BrWorkspace::new();
    let beads_dir = workspace.root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let now = Utc::now();

    let seed_records: Vec<String> = (0..33)
        .map(|idx| {
            serde_json::to_string(&make_issue(
                &format!("bd-a{idx:02}"),
                &format!("Seed issue {idx}"),
                now,
            ))
            .expect("serialize seed issue")
        })
        .collect();
    fs::write(&jsonl_path, seed_records.join("\n") + "\n").expect("write seed jsonl");

    let create = run_br(
        &workspace,
        ["--no-db", "create", "Large no-db create"],
        "create_no_db_large_hash_batch",
    );
    assert!(
        create.status.success(),
        "create --no-db should succeed when export_hashes rewrite spans many rows: {}",
        create.stderr
    );
    let created_id = parse_created_id(&create.stdout);
    assert!(
        !created_id.is_empty(),
        "missing created id after no-db create"
    );

    let add_comment = run_br(
        &workspace,
        [
            "--no-db",
            "comments",
            "add",
            &created_id,
            "Large no-db comment",
            "--json",
        ],
        "comment_no_db_large_hash_batch",
    );
    assert!(
        add_comment.status.success(),
        "comments add --no-db should succeed after large export_hash rewrite: {}",
        add_comment.stderr
    );

    let add_dependency = run_br(
        &workspace,
        ["--no-db", "dep", "add", &created_id, "bd-a00", "--json"],
        "dep_add_no_db_large_hash_batch",
    );
    assert!(
        add_dependency.status.success(),
        "dep add --no-db should succeed after large export_hash rewrite: {}",
        add_dependency.stderr
    );

    let created_record = fs::read_to_string(&jsonl_path)
        .expect("read issues.jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("parse issue json"))
        .find(|record| record["id"].as_str() == Some(created_id.as_str()))
        .expect("created issue record in issues.jsonl");

    assert_eq!(created_record["title"], "Large no-db create");
    assert!(
        created_record["comments"]
            .as_array()
            .is_some_and(|comments| comments
                .iter()
                .any(|comment| { comment["text"].as_str() == Some("Large no-db comment") })),
        "created issue should retain the no-db comment mutation"
    );
    assert!(
        created_record["dependencies"]
            .as_array()
            .is_some_and(|dependencies| dependencies
                .iter()
                .any(|dependency| { dependency["depends_on_id"].as_str() == Some("bd-a00") })),
        "created issue should retain the no-db dependency mutation"
    );
}

#[test]
fn e2e_sync_flush_only_succeeds_with_large_mixed_prefix_export_hash_rewrite() {
    let _log = common::test_log(
        "e2e_sync_flush_only_succeeds_with_large_mixed_prefix_export_hash_rewrite",
    );
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let db_path = workspace.root.join(".beads").join("beads.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open workspace db");
    let now = Utc::now();

    let seeded_hashes: Vec<(String, String)> = (0..160)
        .map(|idx| {
            let prefix = if idx % 2 == 0 { "bd" } else { "br" };
            let issue_id = format!("{prefix}-sync-{idx:03}");
            let issue = make_issue(&issue_id, &format!("Seed issue {idx}"), now);
            storage.create_issue(&issue, "tester").expect("seed issue");
            (issue_id, format!("seed-hash-{idx:03}"))
        })
        .collect();
    storage
        .set_export_hashes(&seeded_hashes)
        .expect("seed export hashes");

    let flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--no-auto-import"],
        "sync_flush_large_mixed_export_hash_rewrite",
    );
    assert!(
        flush.status.success(),
        "sync --flush-only should succeed when rewriting many existing mixed-prefix export hashes: {}",
        flush.stderr
    );

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let exported_count = fs::read_to_string(&jsonl_path)
        .expect("read issues.jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    assert_eq!(exported_count, seeded_hashes.len());
}

#[test]
fn e2e_sync_manifest() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Manifest issue", "--no-auto-flush"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(
        &workspace,
        ["sync", "--flush-only", "--manifest"],
        "sync_manifest",
    );
    assert!(
        sync.status.success(),
        "sync manifest failed: {}",
        sync.stderr
    );

    let manifest_path = workspace.root.join(".beads").join(".manifest.json");
    assert!(manifest_path.exists(), "manifest not created");
}

#[test]
fn e2e_sync_status_json() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Status issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let status = run_br(&workspace, ["sync", "--status", "--json"], "sync_status");
    assert!(
        status.status.success(),
        "sync status failed: {}",
        status.stderr
    );
    let payload = extract_json_payload(&status.stdout);
    let status_json: Value = serde_json::from_str(&payload).expect("sync status json");
    assert!(status_json["dirty_count"].is_number());
}

#[test]
fn e2e_sync_witness_json_is_deterministic_and_read_only() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let first = run_br(&workspace, ["create", "Witness issue A"], "create_a");
    assert!(first.status.success(), "create A failed: {}", first.stderr);

    let second = run_br(&workspace, ["create", "Witness issue B"], "create_b");
    assert!(
        second.status.success(),
        "create B failed: {}",
        second.stderr
    );

    let flush = run_br(&workspace, ["sync", "--flush-only", "--json"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    let status_before = run_br(
        &workspace,
        ["sync", "--status", "--json"],
        "status_before_witness",
    );
    assert!(
        status_before.status.success(),
        "pre-witness status failed: {}",
        status_before.stderr
    );
    let status_before_json: Value =
        serde_json::from_str(&extract_json_payload(&status_before.stdout))
            .expect("pre-witness status json");
    assert_eq!(status_before_json["dirty_count"].as_u64(), Some(0));

    let witness = run_br(
        &workspace,
        ["sync", "--witness", "--witness-chunk-lines", "1", "--json"],
        "sync_witness",
    );
    assert!(
        witness.status.success(),
        "sync witness failed: {}",
        witness.stderr
    );
    let witness_json: Value =
        serde_json::from_str(&extract_json_payload(&witness.stdout)).expect("sync witness json");

    assert!(
        witness_json["jsonl_path"]
            .as_str()
            .is_some_and(|path| path.ends_with(".beads/issues.jsonl")),
        "unexpected witness path: {witness_json}"
    );
    let witness_body = &witness_json["witness"];
    assert_eq!(witness_body["schema_version"], "br.jsonl-witness.v1");
    assert_eq!(witness_body["chunk_size_lines"].as_u64(), Some(1));
    assert_eq!(witness_body["line_count"].as_u64(), Some(2));
    assert!(witness_body["byte_count"].as_u64().is_some_and(|n| n > 0));
    assert_eq!(witness_body["root_hash"].as_str().map(str::len), Some(64));
    assert_eq!(witness_body["chunks"].as_array().map(Vec::len), Some(2));

    let witness_again = run_br(
        &workspace,
        ["sync", "--witness", "--witness-chunk-lines", "1", "--json"],
        "sync_witness_again",
    );
    assert!(
        witness_again.status.success(),
        "second sync witness failed: {}",
        witness_again.stderr
    );
    let witness_again_json: Value =
        serde_json::from_str(&extract_json_payload(&witness_again.stdout))
            .expect("second sync witness json");
    assert_eq!(
        witness_body["root_hash"],
        witness_again_json["witness"]["root_hash"]
    );

    let status_after = run_br(
        &workspace,
        ["sync", "--status", "--json"],
        "status_after_witness",
    );
    assert!(
        status_after.status.success(),
        "post-witness status failed: {}",
        status_after.stderr
    );
    let status_after_json: Value =
        serde_json::from_str(&extract_json_payload(&status_after.stdout))
            .expect("post-witness status json");
    assert_eq!(status_after_json["dirty_count"].as_u64(), Some(0));
}

fn assert_base_witness_reuse_plan(witness_json: &Value) {
    let reuse_plan = &witness_json["base_reuse_plan"];
    assert_eq!(
        reuse_plan["comparison"]["safe_reuse_prefix_chunks"].as_u64(),
        Some(1)
    );
    let schedule = &reuse_plan["schedule"];
    assert_eq!(schedule["candidate_output_actions"].as_u64(), Some(2));
    assert_eq!(schedule["metadata_only_drop_actions"].as_u64(), Some(0));
    assert_eq!(schedule["reusable_actions"].as_u64(), Some(1));
    assert_eq!(schedule["read_added_actions"].as_u64(), Some(1));
    assert_eq!(schedule["max_parallel_candidate_actions"].as_u64(), Some(2));
    assert_eq!(
        schedule["deterministic_candidate_order"].as_bool(),
        Some(true)
    );
    let actions = reuse_plan["actions"].as_array().expect("reuse actions");
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0]["action"].as_str(), Some("reuse_unchanged"));
    assert_eq!(actions[0]["base_index"].as_u64(), Some(0));
    assert_eq!(actions[0]["candidate_index"].as_u64(), Some(0));
    assert_eq!(actions[1]["action"].as_str(), Some("read_added"));
    assert!(actions[1]["base_index"].is_null());
    assert_eq!(actions[1]["candidate_index"].as_u64(), Some(1));

    let work_plan = &witness_json["base_parallel_work_plan"];
    assert_eq!(work_plan["max_parallelism"].as_u64(), Some(1));
    assert_eq!(work_plan["total_batches"].as_u64(), Some(2));
    assert_eq!(work_plan["candidate_output_batches"].as_u64(), Some(2));
    assert_eq!(work_plan["metadata_only_drop_batches"].as_u64(), Some(0));
    assert_eq!(work_plan["deterministic_batch_order"].as_bool(), Some(true));
    let batches = work_plan["batches"].as_array().expect("work batches");
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0]["kind"].as_str(), Some("candidate_output"));
    assert_eq!(batches[0]["candidate_start_index"].as_u64(), Some(0));
    assert_eq!(batches[0]["candidate_end_index"].as_u64(), Some(1));
    assert_eq!(batches[0]["action_count"].as_u64(), Some(1));
    assert_eq!(batches[0]["actions"].as_array().map(Vec::len), Some(1));
    assert_eq!(batches[1]["kind"].as_str(), Some("candidate_output"));
    assert_eq!(batches[1]["candidate_start_index"].as_u64(), Some(1));
    assert_eq!(batches[1]["candidate_end_index"].as_u64(), Some(2));
    assert_eq!(batches[1]["action_count"].as_u64(), Some(1));

    let materialization = &witness_json["base_reuse_materialization"];
    assert_eq!(materialization["reused_chunks"].as_u64(), Some(1));
    assert_eq!(materialization["rebuilt_chunks"].as_u64(), Some(0));
    assert_eq!(materialization["read_added_chunks"].as_u64(), Some(1));
    assert_eq!(materialization["dropped_chunks"].as_u64(), Some(0));
    assert_eq!(
        materialization["output_byte_count"].as_u64(),
        witness_json["witness"]["byte_count"].as_u64()
    );
    assert_eq!(
        materialization["reused_byte_count"].as_u64(),
        reuse_plan["schedule"]["reusable_byte_count"].as_u64()
    );
    assert_eq!(
        materialization["read_added_byte_count"].as_u64(),
        reuse_plan["schedule"]["read_added_byte_count"].as_u64()
    );
}

#[test]
fn e2e_sync_flush_export_parallelism_preserves_jsonl_bytes() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_parallel_export");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let now = Utc::now();
    let records = (0..300)
        .map(|index| {
            let id = format!("bd-pex{index:04}");
            let mut issue = make_issue(&id, &format!("Parallel export issue {index:04}"), now);
            issue.description = Some(format!(
                "Synthetic JSONL export payload {index:04} with enough stable text to exercise ordered line preparation."
            ));
            issue.assignee = Some(format!("agent-{:03}", index % 64));
            issue.labels = vec![
                "parallel-export".to_string(),
                "jsonl".to_string(),
                format!("lane-{:02}", index % 16),
            ];
            issue.comments.push(Comment {
                id: i64::from(index) + 1,
                issue_id: id,
                author: format!("agent-{:03}", index % 64),
                body: format!(
                    "Deterministic comment payload {index:04} for serde_json export parity."
                ),
                created_at: now,
            });
            serde_json::to_string(&issue).expect("serialize parallel export fixture issue")
        })
        .collect::<Vec<_>>();
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    fs::write(&jsonl_path, records.join("\n") + "\n").expect("write parallel export fixture");

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force", "--json"],
        "import_parallel_export_fixture",
    );
    assert!(
        import.status.success(),
        "import fixture failed: {}",
        import.stderr
    );

    let serial = run_br(
        &workspace,
        [
            "sync",
            "--flush-only",
            "--force",
            "--export-parallelism",
            "1",
            "--json",
        ],
        "flush_parallel_export_serial",
    );
    assert!(
        serial.status.success(),
        "serial export failed: {}",
        serial.stderr
    );
    let serial_json: Value =
        serde_json::from_str(&extract_json_payload(&serial.stdout)).expect("serial flush json");
    let serial_bytes = fs::read(&jsonl_path).expect("read serial jsonl");

    let parallel = run_br(
        &workspace,
        [
            "sync",
            "--flush-only",
            "--force",
            "--export-parallelism",
            "4",
            "--json",
        ],
        "flush_parallel_export_parallel",
    );
    assert!(
        parallel.status.success(),
        "parallel export failed: {}",
        parallel.stderr
    );
    let parallel_json: Value =
        serde_json::from_str(&extract_json_payload(&parallel.stdout)).expect("parallel flush json");
    let parallel_bytes = fs::read(&jsonl_path).expect("read parallel jsonl");

    assert_eq!(parallel_bytes, serial_bytes);
    assert_eq!(serial_json["exported_issues"].as_u64(), Some(300));
    assert_eq!(parallel_json["exported_issues"].as_u64(), Some(300));
    assert_eq!(parallel_json["content_hash"], serial_json["content_hash"]);
}

#[test]
fn e2e_sync_witness_reports_base_snapshot_drift() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_base_witness");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let first = run_br(
        &workspace,
        ["create", "Base witness issue A"],
        "create_base_witness_a",
    );
    assert!(first.status.success(), "create A failed: {}", first.stderr);

    let first_flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--json"],
        "sync_flush_base_witness_a",
    );
    assert!(
        first_flush.status.success(),
        "first sync flush failed: {}",
        first_flush.stderr
    );

    let second = run_br(
        &workspace,
        ["create", "Base witness issue B"],
        "create_base_witness_b",
    );
    assert!(
        second.status.success(),
        "create B failed: {}",
        second.stderr
    );

    let second_flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--json"],
        "sync_flush_base_witness_b",
    );
    assert!(
        second_flush.status.success(),
        "second sync flush failed: {}",
        second_flush.stderr
    );

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let base_snapshot_path = workspace.root.join(".beads").join("beads.base.jsonl");
    let current_jsonl = fs::read_to_string(&jsonl_path).expect("read current jsonl");
    let first_candidate_line = current_jsonl
        .lines()
        .next()
        .expect("candidate jsonl should contain at least one issue");
    fs::write(&base_snapshot_path, format!("{first_candidate_line}\n"))
        .expect("seed base witness snapshot");

    let witness = run_br(
        &workspace,
        [
            "sync",
            "--witness",
            "--witness-chunk-lines",
            "1",
            "--witness-parallelism",
            "1",
            "--json",
        ],
        "sync_witness_base_compare",
    );
    assert!(
        witness.status.success(),
        "sync witness failed: {}",
        witness.stderr
    );
    let witness_json: Value =
        serde_json::from_str(&extract_json_payload(&witness.stdout)).expect("sync witness json");

    assert!(
        witness_json["base_jsonl_path"]
            .as_str()
            .is_some_and(|path| path.ends_with(".beads/beads.base.jsonl")),
        "unexpected base witness path: {witness_json}"
    );
    let comparison = &witness_json["base_comparison"];
    assert_eq!(comparison["schema_versions_match"].as_bool(), Some(true));
    assert_eq!(comparison["chunk_size_lines_match"].as_bool(), Some(true));
    assert_eq!(comparison["drift_detected"].as_bool(), Some(true));
    assert_eq!(comparison["base_line_count"].as_u64(), Some(1));
    assert_eq!(comparison["candidate_line_count"].as_u64(), Some(2));
    assert_eq!(comparison["unchanged_chunks"].as_u64(), Some(1));
    assert_eq!(comparison["changed_chunks"].as_u64(), Some(0));
    assert_eq!(comparison["added_chunks"].as_u64(), Some(1));
    assert_eq!(comparison["removed_chunks"].as_u64(), Some(0));
    assert_eq!(comparison["safe_reuse_prefix_chunks"].as_u64(), Some(1));
    assert_eq!(comparison["first_changed_chunk_index"].as_u64(), Some(1));
    assert_base_witness_reuse_plan(&witness_json);
}

#[test]
fn e2e_version_text() {
    let workspace = BrWorkspace::new();

    let version = run_br(&workspace, ["version"], "version");
    assert!(
        version.status.success(),
        "version failed: {}",
        version.stderr
    );
    assert!(
        version.stdout.contains("br version"),
        "version output missing header"
    );
}

#[test]
fn e2e_doctor_json() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let doctor = run_br(&workspace, ["doctor", "--json"], "doctor_json");
    assert!(doctor.status.success(), "doctor failed: {}", doctor.stderr);
    let payload = extract_json_payload(&doctor.stdout);
    let doctor_json: Value = serde_json::from_str(&payload).expect("doctor json");
    assert!(doctor_json["checks"].is_array(), "doctor checks missing");
}

#[test]
fn e2e_sync_status_text() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let status = run_br(&workspace, ["sync", "--status"], "sync_status_text");
    assert!(
        status.status.success(),
        "sync status text failed: {}",
        status.stderr
    );
    assert!(
        status.stdout.contains("Sync Status"),
        "sync status text missing header"
    );
}

#[test]
fn e2e_version_json() {
    let workspace = BrWorkspace::new();

    let version = run_br(&workspace, ["version", "--json"], "version_json");
    assert!(
        version.status.success(),
        "version json failed: {}",
        version.stderr
    );
    let payload = extract_json_payload(&version.stdout);
    let version_json: Value = serde_json::from_str(&payload).expect("version json");
    assert!(version_json["version"].is_string());
}

#[test]
fn e2e_sync_conflict_markers_aborts_import() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create initial issue and export
    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    // Inject conflict markers into JSONL
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let original = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let conflicted = format!(
        "<<<<<<< HEAD\n{}\n=======\n{}\n>>>>>>> feature-branch\n",
        original.trim(),
        original.trim()
    );
    fs::write(&jsonl_path, conflicted).expect("write conflicted jsonl");

    // Import should fail due to conflict markers
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "sync_import_conflict",
    );
    assert!(
        !import.status.success(),
        "import should fail with conflict markers"
    );
    assert!(
        import.stderr.contains("Merge conflict markers detected")
            || import.stdout.contains("Merge conflict markers detected"),
        "error message should mention conflict markers: stdout={}, stderr={}",
        import.stdout,
        import.stderr
    );
}

#[test]
fn e2e_sync_tombstone_preservation() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create and then delete an issue (creates tombstone)
    let create = run_br(&workspace, ["create", "Issue to delete"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    let delete = run_br(
        &workspace,
        ["delete", &id, "--force", "--reason", "Testing tombstone"],
        "delete",
    );
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);

    // Verify issue is now a tombstone
    let show = run_br(&workspace, ["show", &id, "--json"], "show_tombstone");
    assert!(
        show.status.success(),
        "show tombstone failed: {}",
        show.stderr
    );
    let payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&payload).expect("show json");
    assert_eq!(
        show_json[0]["status"], "tombstone",
        "issue should be tombstone"
    );

    // Export to JSONL
    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    // Read the JSONL and verify tombstone is present
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    assert!(
        contents.contains("\"status\":\"tombstone\""),
        "JSONL should contain tombstone status"
    );

    // Create a new workspace to simulate importing into fresh database
    let workspace2 = BrWorkspace::new();
    let init2 = run_br(&workspace2, ["init"], "init2");
    assert!(init2.status.success(), "init2 failed: {}", init2.stderr);

    // Copy the JSONL to new workspace
    let jsonl_path2 = workspace2.root.join(".beads").join("issues.jsonl");
    fs::copy(&jsonl_path, &jsonl_path2).expect("copy jsonl");

    // Import
    let import = run_br(
        &workspace2,
        ["sync", "--import-only", "--force"],
        "sync_import",
    );
    assert!(import.status.success(), "import failed: {}", import.stderr);

    // Verify tombstone was imported
    let show2 = run_br(&workspace2, ["show", &id, "--json"], "show_after_import");
    assert!(
        show2.status.success(),
        "show after import failed: {}",
        show2.stderr
    );
    let payload2 = extract_json_payload(&show2.stdout);
    let show_json2: Vec<Value> = serde_json::from_str(&payload2).expect("show json after import");
    assert_eq!(
        show_json2[0]["status"], "tombstone",
        "tombstone should be preserved after import"
    );
}

#[test]
fn e2e_sync_tombstone_protection() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create and delete an issue
    let create = run_br(&workspace, ["create", "Protected issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    let delete = run_br(
        &workspace,
        ["delete", &id, "--force", "--reason", "Tombstone test"],
        "delete",
    );
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);

    // Export tombstone to JSONL
    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    // Modify JSONL to try to resurrect the tombstone (change status to open)
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let mut modified_lines = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut issue: Value = serde_json::from_str(line).expect("parse issue");
        if issue["status"] == "tombstone" {
            // Try to resurrect it
            issue["status"] = Value::String("open".to_string());
            issue["updated_at"] = Value::String(Utc::now().to_rfc3339());
        }
        modified_lines.push(serde_json::to_string(&issue).expect("serialize"));
    }
    fs::write(&jsonl_path, modified_lines.join("\n") + "\n").expect("write modified jsonl");

    sleep(Duration::from_millis(50));

    // Import - tombstone should be protected (resurrection blocked)
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "sync_import_resurrect",
    );
    assert!(import.status.success(), "import failed: {}", import.stderr);

    // Verify the issue is still a tombstone (not resurrected)
    let show = run_br(
        &workspace,
        ["show", &id, "--json"],
        "show_after_resurrect_attempt",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&payload).expect("show json");
    assert_eq!(
        show_json[0]["status"], "tombstone",
        "tombstone protection should prevent resurrection"
    );
}

#[test]
fn e2e_sync_content_hash_consistency() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create issues
    let create1 = run_br(
        &workspace,
        ["create", "Issue A", "--no-auto-flush"],
        "create1",
    );
    assert!(
        create1.status.success(),
        "create1 failed: {}",
        create1.stderr
    );
    let create2 = run_br(
        &workspace,
        ["create", "Issue B", "--no-auto-flush"],
        "create2",
    );
    assert!(
        create2.status.success(),
        "create2 failed: {}",
        create2.stderr
    );

    // Export and get hash
    let flush1 = run_br(
        &workspace,
        ["sync", "--flush-only", "--json"],
        "sync_flush1",
    );
    assert!(
        flush1.status.success(),
        "sync flush1 failed: {}",
        flush1.stderr
    );
    let payload1 = extract_json_payload(&flush1.stdout);
    let flush_json1: Value = serde_json::from_str(&payload1).expect("flush json1");
    let hash1 = flush_json1["content_hash"].as_str().expect("content_hash1");

    // Export again without changes (force to re-export)
    let flush2 = run_br(
        &workspace,
        ["sync", "--flush-only", "--force", "--json"],
        "sync_flush2",
    );
    assert!(
        flush2.status.success(),
        "sync flush2 failed: {}",
        flush2.stderr
    );
    let payload2 = extract_json_payload(&flush2.stdout);
    let flush_json2: Value = serde_json::from_str(&payload2).expect("flush json2");
    let hash2 = flush_json2["content_hash"].as_str().expect("content_hash2");

    // Content hash should be consistent for same content
    assert_eq!(
        hash1, hash2,
        "content hash should be consistent for same content"
    );

    // Verify status shows the hash
    let status = run_br(&workspace, ["sync", "--status", "--json"], "sync_status");
    assert!(
        status.status.success(),
        "sync status failed: {}",
        status.stderr
    );
    let status_payload = extract_json_payload(&status.stdout);
    let status_json: Value = serde_json::from_str(&status_payload).expect("status json");
    let stored_hash = status_json["jsonl_content_hash"]
        .as_str()
        .expect("stored hash");
    assert_eq!(stored_hash, hash2, "stored hash should match export hash");
}

#[test]
fn e2e_jsonl_discovery_prefers_issues() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue and export
    let create = run_br(&workspace, ["create", "Discovery test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    // Verify issues.jsonl was created (default)
    let issues_path = workspace.root.join(".beads").join("issues.jsonl");
    assert!(issues_path.exists(), "issues.jsonl should be created");

    // Create a legacy beads.jsonl with different content
    let beads_path = workspace.root.join(".beads").join("beads.jsonl");
    fs::write(&beads_path, "{\"id\": \"fake-id\", \"title\": \"Legacy issue\", \"status\": \"open\", \"issue_type\": \"task\", \"priority\": 2, \"labels\": [], \"created_at\": \"2026-01-01T00:00:00Z\", \"updated_at\": \"2026-01-01T00:00:00Z\", \"ephemeral\": false, \"pinned\": false, \"is_template\": false, \"dependencies\": [], \"comments\": []}\n").expect("write legacy");

    // When both exist, import should use issues.jsonl (the issue we created)
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "sync_import",
    );
    assert!(import.status.success(), "import failed: {}", import.stderr);

    // Verify our issue exists (from issues.jsonl), not the fake one
    let show = run_br(&workspace, ["show", &id, "--json"], "show_original");
    assert!(
        show.status.success(),
        "show original failed: {}",
        show.stderr
    );

    // Verify fake-id doesn't exist (wasn't imported from beads.jsonl)
    let show_fake = run_br(&workspace, ["show", "fake-id", "--json"], "show_fake");
    // Should fail or return empty since fake-id shouldn't exist
    let fake_payload = extract_json_payload(&show_fake.stdout);
    let fake_json: Vec<Value> = serde_json::from_str(&fake_payload).unwrap_or_default();
    assert!(
        fake_json.is_empty() || show_fake.stderr.contains("not found"),
        "fake issue from beads.jsonl should not be imported when issues.jsonl exists"
    );
}

#[test]
fn e2e_jsonl_discovery_uses_legacy_when_no_issues() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Remove issues.jsonl if it exists
    let issues_path = workspace.root.join(".beads").join("issues.jsonl");
    if issues_path.exists() {
        fs::remove_file(&issues_path).expect("remove issues.jsonl");
    }

    // Create a legacy beads.jsonl with an issue (using bd- prefix)
    let beads_path = workspace.root.join(".beads").join("beads.jsonl");
    fs::write(&beads_path, "{\"id\": \"bd-legacy1\", \"title\": \"Legacy issue\", \"status\": \"open\", \"issue_type\": \"task\", \"priority\": 2, \"labels\": [], \"created_at\": \"2026-01-01T00:00:00Z\", \"updated_at\": \"2026-01-01T00:00:00Z\", \"ephemeral\": false, \"pinned\": false, \"is_template\": false, \"dependencies\": [], \"comments\": []}\n").expect("write legacy");

    // Import should use beads.jsonl since issues.jsonl doesn't exist
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "sync_import_legacy",
    );
    assert!(
        import.status.success(),
        "import legacy failed: {}",
        import.stderr
    );

    // Verify the legacy issue was imported
    let show = run_br(&workspace, ["show", "bd-legacy1", "--json"], "show_legacy");
    assert!(show.status.success(), "show legacy failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&payload).expect("show json");
    assert_eq!(
        show_json[0]["title"], "Legacy issue",
        "legacy issue should be imported from beads.jsonl"
    );
}
