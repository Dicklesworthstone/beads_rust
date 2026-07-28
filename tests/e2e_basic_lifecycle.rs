mod common;

use beads_rust::model::{Comment, Dependency, DependencyType, Issue, IssueType, Priority, Status};
use beads_rust::storage::SqliteStorage;
#[cfg(target_os = "linux")]
use beads_rust::sync::{blocking_jsonl_family_write_lock_with_timeout, blocking_write_lock};
use chrono::Utc;
use common::cli::{
    BrRun, BrWorkspace, extract_json_payload, parse_json_value, parse_list_issues, run_br,
    run_br_smoke_at_root_with_env,
};
use common::isolated_workspace_failure_fixture;
#[cfg(target_os = "linux")]
use fsqlite::Connection;
#[cfg(target_os = "linux")]
use fsqlite_types::SqliteValue;
use serde_json::Value;
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
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
        source_repo_path: None,
        agent_context: None,
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

#[cfg(target_os = "linux")]
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

/// GitHub #391: `br dep cycles` must agree with the add-time gate — a
/// `related` edge accepted without a cycle check can never fail the cycle
/// health report (which exits nonzero on active cycles since #368).
#[test]
fn e2e_dep_cycles_agrees_with_add_time_related_semantics() {
    let _log = common::test_log("e2e_dep_cycles_agrees_with_add_time_related_semantics");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "cyc_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut ids: Vec<String> = Vec::new();
    for title in ["epic E", "sub S", "grandchild E2", "H", "R", "A", "M"] {
        let create = run_br(&workspace, ["create", title], "cyc_create");
        assert!(create.status.success(), "create failed: {}", create.stderr);
        ids.push(parse_created_id(&create.stdout));
    }
    let (epic, sub, grandchild, blocker_h, blocker_r, blocker_a, blocker_m) = (
        &ids[0], &ids[1], &ids[2], &ids[3], &ids[4], &ids[5], &ids[6],
    );
    for (from, to, dep_type) in [
        (sub, epic, "parent-child"),
        (grandchild, sub, "parent-child"),
        (blocker_h, epic, "blocks"),
        (blocker_r, blocker_h, "blocks"),
        (blocker_a, epic, "blocks"),
        (blocker_m, blocker_a, "blocks"),
    ] {
        let add = run_br(
            &workspace,
            ["dep", "add", from, to, "--type", dep_type],
            "cyc_dep_add",
        );
        assert!(add.status.success(), "dep add failed: {}", add.stderr);
    }

    // Documented containment rule: the descendant's blocks-edge back into a
    // chain reaching the epic is rejected, and the hint explains that epic
    // containment participates.
    let rejected = run_br(
        &workspace,
        ["dep", "add", grandchild, blocker_r],
        "cyc_rejected",
    );
    assert!(
        !rejected.status.success(),
        "containment-induced cycle must still reject: {}",
        rejected.stdout
    );
    let combined = format!("{}{}", rejected.stdout, rejected.stderr);
    assert!(
        combined.contains("epic containment"),
        "rejection hint must explain containment participation: {combined}"
    );

    // A `related` edge is accepted unchecked and must not fail the report.
    let related = run_br(
        &workspace,
        ["dep", "add", grandchild, blocker_m, "--type", "related"],
        "cyc_related",
    );
    assert!(
        related.status.success(),
        "related add failed: {}",
        related.stderr
    );
    for args in [
        vec!["dep", "cycles"],
        vec!["dep", "cycles", "--blocking-only"],
    ] {
        let cycles = run_br(&workspace, args.clone(), "cyc_report");
        assert!(
            cycles.status.success(),
            "{args:?} must exit 0 when the only 'cycle' is a related edge \
             the add path allowed: {}{}",
            cycles.stdout,
            cycles.stderr
        );
    }
}

#[test]
fn e2e_list_and_count_status_all_matches_every_status() {
    let _log = common::test_log("e2e_list_and_count_status_all_matches_every_status");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "status_all_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // One open, one in_progress, one closed issue.
    let mut ids = Vec::new();
    for title in ["Open one", "Working one", "Closed one"] {
        let create = run_br(&workspace, ["create", title], "status_all_create");
        assert!(create.status.success(), "create failed: {}", create.stderr);
        ids.push(parse_created_id(&create.stdout));
    }
    let claim = run_br(
        &workspace,
        ["update", &ids[1], "--status", "in_progress"],
        "status_all_claim",
    );
    assert!(claim.status.success(), "claim failed: {}", claim.stderr);
    let close = run_br(
        &workspace,
        ["close", &ids[2], "--reason", "done"],
        "status_all_close",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);

    // `--status all` must return every issue (beads_rust-6ilv: it used to
    // parse as the literal custom status "all" and silently match nothing).
    let list = run_br(
        &workspace,
        ["list", "--status", "all", "--json"],
        "status_all_list",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let issues = parse_list_issues(&list.stdout);
    assert_eq!(
        issues.len(),
        3,
        "--status all must match every status: {issues:?}"
    );

    let count = run_br(
        &workspace,
        ["count", "--status", "all", "--json"],
        "status_all_count",
    );
    assert!(count.status.success(), "count failed: {}", count.stderr);
    let count_json: Value =
        serde_json::from_str(common::cli::extract_json_payload(&count.stdout).as_str())
            .expect("count JSON");
    let total = count_json
        .get("count")
        .or_else(|| count_json.get("total"))
        .and_then(Value::as_u64)
        .expect("count total");
    assert_eq!(total, 3, "count --status all must match every status");

    let search = run_br(
        &workspace,
        ["search", "one", "--status", "all", "--json"],
        "status_all_search",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);
    assert!(
        search.stdout.contains(&ids[2]),
        "search --status all must include closed issues: {}",
        search.stdout
    );
}

#[cfg(target_os = "linux")]
fn publication_temp_path_for_child(jsonl_path: &Path, child_pid: u32, attempt: u32) -> PathBuf {
    if attempt == 0 {
        return jsonl_path.with_extension(format!("jsonl.{child_pid}.tmp"));
    }

    let retry_suffix = u64::from(child_pid)
        .saturating_mul(100)
        .saturating_add(u64::from(attempt));
    jsonl_path.with_extension(format!("jsonl.{retry_suffix}.tmp"))
}

#[cfg(target_os = "linux")]
fn run_sync_merge_with_exhausted_publication_names(
    workspace: &BrWorkspace,
    jsonl_path: &Path,
    label: &str,
) -> BrRun {
    const PUBLICATION_NAME_ATTEMPTS: u32 = 64;

    let beads_dir = workspace.root.join(".beads");
    let write_lock =
        blocking_write_lock(&beads_dir).expect("hold workspace write lock while arming fixture");

    let mut cmd = StdCommand::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(&workspace.root);
    cmd.args(["sync", "--merge", "--allow-external-jsonl", "--json"]);
    clear_br_env_for_std_command(&mut cmd);
    cmd.env("BR_HISTORY_MIN_INTERVAL_SECS", "0");
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_LOG", "beads_rust=debug");
    cmd.env("RUST_BACKTRACE", "1");
    cmd.env("HOME", &workspace.root);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let start = std::time::Instant::now();
    let child = cmd
        .spawn()
        .expect("spawn sync merge for publication denial");
    let child_pid = child.id();
    let marker = format!("reserved publication namespace for child {child_pid}\n");
    let collision_paths = (0..PUBLICATION_NAME_ATTEMPTS)
        .map(|attempt| publication_temp_path_for_child(jsonl_path, child_pid, attempt))
        .collect::<Vec<_>>();
    for collision_path in &collision_paths {
        fs::write(collision_path, marker.as_bytes())
            .expect("reserve PID-scoped publication namespace");
    }

    // The child can now acquire the workspace authority and commit its
    // database transaction. Its subsequent atomic JSONL publication must
    // exhaust the real create-new namespace rather than relying on Unix mode
    // bits, which privileged RCH workers may legitimately bypass.
    drop(write_lock);
    let output = child
        .wait_with_output()
        .expect("collect interrupted sync merge");
    let duration = start.elapsed();

    let quarantine = workspace
        .root
        .join(format!("publication-collisions-{child_pid}"));
    fs::create_dir_all(&quarantine).expect("create publication-collision quarantine");
    for (attempt, collision_path) in collision_paths.iter().enumerate() {
        assert_eq!(
            fs::read(collision_path).expect("read preserved publication collision"),
            marker.as_bytes(),
            "merge changed a pre-existing publication collision at {}",
            collision_path.display()
        );
        fs::rename(
            collision_path,
            quarantine.join(format!("{attempt}.collision")),
        )
        .expect("move publication collision aside for receipt resume");
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let log_path = workspace.log_dir.join(format!("{label}.log"));
    fs::write(
        &log_path,
        format!(
            "label: {label}\nduration: {duration:?}\nstatus: {}\nchild_pid: {child_pid}\ncwd: {}\n\nstdout:\n{stdout}\n\nstderr:\n{stderr}\n",
            output.status,
            workspace.root.display()
        ),
    )
    .expect("write publication-denial command log");

    BrRun {
        stdout,
        stderr,
        status: output.status,
        duration,
        log_path,
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

    // Terminal-state transitions must go through `br close` so close-policy
    // (close-reason / AC / attribution) is enforced; `update --status closed`
    // refuses by design (#301).
    let close_args = vec![
        "close".to_string(),
        id,
        "--reason".to_string(),
        "e2e lifecycle complete".to_string(),
    ];
    let close = run_br(&workspace, close_args, "close");
    assert!(close.status.success(), "close failed: {}", close.stderr);
}

#[test]
fn e2e_update_description_file_preserves_exact_content() {
    let _log = common::test_log("e2e_update_description_file_preserves_exact_content");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_update_description_file");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Description file target"],
        "create_update_description_file_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);
    assert!(!issue_id.is_empty(), "missing created id");

    let exact_description = "  leading spaces stay\n\n# Markdown heading\n\n- first\n- second\n\ntrailing newline stays\n";
    let description_path = workspace.root.join("description.md");
    fs::write(&description_path, exact_description).expect("write description file");

    let update = run_br(
        &workspace,
        vec![
            "update".to_string(),
            issue_id.clone(),
            "--description-file".to_string(),
            description_path.display().to_string(),
            "--json".to_string(),
        ],
        "update_description_from_file",
    );
    assert!(
        update.status.success(),
        "description-file update failed: stdout={} stderr={}",
        update.stdout,
        update.stderr
    );
    assert!(
        !update.stdout.contains("No updates specified"),
        "description-file must be treated as an update: {}",
        update.stdout
    );
    let updated = parse_json_array(&update.stdout, "parse description-file update json");
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0]["id"].as_str(), Some(issue_id.as_str()));

    assert_issue_description(&workspace, &issue_id, exact_description);

    let empty_path = workspace.root.join("empty-description.md");
    fs::write(&empty_path, "").expect("write empty description file");
    let clear_to_empty = run_br(
        &workspace,
        vec![
            "update".to_string(),
            issue_id.clone(),
            "--description-file".to_string(),
            empty_path.display().to_string(),
        ],
        "update_description_from_empty_file",
    );
    assert!(
        clear_to_empty.status.success(),
        "empty description-file update failed: stdout={} stderr={}",
        clear_to_empty.stdout,
        clear_to_empty.stderr
    );
    assert!(
        !clear_to_empty.stdout.contains("No updates specified"),
        "an empty file is an explicit empty-description update: {}",
        clear_to_empty.stdout
    );
    // A cleared description reads back as null: the storage layer normalizes
    // empty text to None on read (`get_non_empty_str`), so `Some("")` is
    // unrepresentable after a round-trip. The contract under test is that the
    // empty file CLEARS the previous description rather than being a no-op.
    let show = run_br(&workspace, ["show", &issue_id, "--json"], "show_cleared");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("parse show json");
    assert!(
        issues[0]["description"].is_null(),
        "description must be cleared to null, got: {}",
        issues[0]["description"]
    );
}

#[test]
fn e2e_update_description_file_conflicts_and_read_failures_do_not_mutate() {
    let _log =
        common::test_log("e2e_update_description_file_conflicts_and_read_failures_do_not_mutate");
    let workspace = BrWorkspace::new();

    let init = run_br(
        &workspace,
        ["init"],
        "init_update_description_file_failures",
    );
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        [
            "create",
            "Description file failure target",
            "--description",
            "original description",
        ],
        "create_update_description_file_failure_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);
    assert!(!issue_id.is_empty(), "missing created id");

    let description_path = workspace.root.join("replacement.md");
    fs::write(&description_path, "replacement description\n").expect("write replacement file");

    let conflict = run_br(
        &workspace,
        vec![
            "update".to_string(),
            issue_id.clone(),
            "--description".to_string(),
            "inline replacement".to_string(),
            "--description-file".to_string(),
            description_path.display().to_string(),
        ],
        "update_description_file_conflict",
    );
    assert!(
        !conflict.status.success(),
        "conflicting description inputs must fail"
    );
    assert!(
        conflict.stderr.contains("cannot be used with"),
        "expected clap conflict diagnostic: {}",
        conflict.stderr
    );
    assert_issue_description(&workspace, &issue_id, "original description");

    let missing_path = workspace.root.join("missing-description.md");
    let missing = run_br(
        &workspace,
        vec![
            "update".to_string(),
            issue_id.clone(),
            "--description-file".to_string(),
            missing_path.display().to_string(),
        ],
        "update_description_file_missing",
    );
    assert!(
        !missing.status.success(),
        "an unreadable description file must fail instead of silently no-oping"
    );
    assert!(
        missing.stderr.contains("failed to read description file"),
        "expected description-file read diagnostic: {}",
        missing.stderr
    );
    assert_issue_description(&workspace, &issue_id, "original description");
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

/// GitHub issue #393: the `--claim --json` echo must carry the resulting
/// assignee so an agent can confirm the claim landed without a follow-up
/// `br show`. The field is emitted unconditionally (null when unassigned) so
/// "not claimed" and "not reported" stay distinguishable.
#[test]
fn e2e_update_claim_json_echo_reports_assignee() {
    let _log = common::test_log("e2e_update_claim_json_echo_reports_assignee");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_claim_echo_assignee");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Claim echo target", "--json"],
        "create_claim_echo_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let id = created["id"].as_str().expect("issue id").to_string();

    let claim = run_br(
        &workspace,
        ["--actor", "testagent", "update", &id, "--claim", "--json"],
        "claim_echo_assignee",
    );
    assert!(claim.status.success(), "claim failed: {}", claim.stderr);

    let claimed: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&claim.stdout)).expect("claim echo json");
    assert_eq!(claimed.len(), 1, "expected one updated issue in the echo");
    assert_eq!(claimed[0]["id"].as_str(), Some(id.as_str()));
    assert_eq!(claimed[0]["status"].as_str(), Some("in_progress"));
    assert_eq!(
        claimed[0]["assignee"].as_str(),
        Some("testagent"),
        "claim echo must report the resulting assignee: {}",
        claim.stdout
    );

    // A non-claim update on an unassigned issue still carries the key, as
    // an explicit null rather than an omitted field.
    let create_plain = run_br(
        &workspace,
        ["create", "Unassigned target", "--json"],
        "create_unassigned_target",
    );
    assert!(
        create_plain.status.success(),
        "create failed: {}",
        create_plain.stderr
    );
    let plain: Value =
        serde_json::from_str(&extract_json_payload(&create_plain.stdout)).expect("create json");
    let plain_id = plain["id"].as_str().expect("issue id").to_string();

    let bump = run_br(
        &workspace,
        ["update", &plain_id, "--priority", "1", "--json"],
        "update_unassigned_priority",
    );
    assert!(bump.status.success(), "update failed: {}", bump.stderr);
    let bumped: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&bump.stdout)).expect("update echo json");
    assert!(
        bumped[0].get("assignee").is_some(),
        "assignee key must be present even when unassigned: {}",
        bump.stdout
    );
    assert!(bumped[0]["assignee"].is_null());
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

#[cfg(target_os = "linux")]
#[test]
fn e2e_sync_merge_resume_reuses_receipt_tombstone_cutoff() {
    let _log = common::test_log("e2e_sync_merge_resume_reuses_receipt_tombstone_cutoff");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init_merge_resume_cutoff");
    assert_br_success(&init, "merge-resume cutoff init");

    let beads_dir = workspace.root.join(".beads");
    let db_path = beads_dir.join("beads.db");
    let base_path = beads_dir.join("beads.base.jsonl");
    let external_dir = workspace.root.join("external-jsonl");
    let jsonl_path = external_dir.join("issues.jsonl");
    fs::create_dir_all(&external_dir).expect("create external JSONL directory");

    // The tombstone expires twelve seconds after this fixture is created.
    // That leaves comfortable headroom for the first merge to commit, while
    // keeping the regression bounded: resume happens only after wall clock has
    // crossed the exact one-day retention boundary.
    let deleted_at = Utc::now() - chrono::Duration::days(1) + chrono::Duration::seconds(12);
    let retention_boundary = deleted_at + chrono::Duration::days(1);
    let mut storage = SqliteStorage::open(&db_path).expect("open merge-resume database");
    let victim = make_issue(
        "bd-resume-cutoff-victim",
        "Deleted by the interrupted merge",
        deleted_at,
    );
    storage
        .create_issue(&victim, "merge-resume-fixture")
        .expect("seed merge deletion victim");

    let mut boundary_tombstone = make_issue(
        "bd-resume-cutoff-boundary",
        "Retained only at the receipt cutoff",
        deleted_at,
    );
    boundary_tombstone.status = Status::Tombstone;
    boundary_tombstone.updated_at = deleted_at;
    boundary_tombstone.deleted_at = Some(deleted_at);
    boundary_tombstone.deleted_by = Some("merge-resume-fixture".to_string());
    boundary_tombstone.delete_reason = Some("retention boundary fixture".to_string());
    boundary_tombstone.original_type = Some("task".to_string());
    storage
        .upsert_issue_for_import(&boundary_tombstone)
        .expect("seed boundary tombstone");

    let mut base_bytes = Vec::new();
    beads_rust::sync::export_to_writer(&storage, &mut base_bytes)
        .expect("export canonical merge base");
    drop(storage);
    fs::write(&base_path, &base_bytes).expect("write merge base");

    let current_jsonl = std::str::from_utf8(&base_bytes)
        .expect("canonical base is UTF-8")
        .lines()
        .filter(|line| {
            let issue: Value = serde_json::from_str(line).expect("parse canonical base issue");
            issue["id"].as_str() == Some(boundary_tombstone.id.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&jsonl_path, current_jsonl).expect("write external deletion generation");

    let metadata_path = beads_dir.join("metadata.json");
    let mut metadata: Value =
        serde_json::from_slice(&fs::read(&metadata_path).expect("read workspace metadata"))
            .expect("parse workspace metadata");
    metadata["jsonl_export"] = Value::String(jsonl_path.to_string_lossy().into_owned());
    metadata["deletions_retention_days"] = Value::from(1_u64);
    fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&metadata).expect("serialize workspace metadata"),
    )
    .expect("route sync to external JSONL with one-day retention");

    let interrupted = run_sync_merge_with_exhausted_publication_names(
        &workspace,
        &jsonl_path,
        "merge_interrupted_after_database_commit",
    );

    let read_pending_receipt = || {
        let connection =
            Connection::open(db_path.to_string_lossy().into_owned()).expect("open raw merge DB");
        let rows = connection
            .query_with_params(
                "SELECT value FROM metadata WHERE key = ? ORDER BY rowid DESC",
                &[SqliteValue::from("sync_merge_pending_v2")],
            )
            .expect("query pending merge receipt");
        let receipt = rows
            .first()
            .and_then(|row| row.get(0))
            .and_then(SqliteValue::as_text)
            .map(str::to_owned);
        connection
            .close()
            .expect("close raw receipt-inspection connection");
        receipt
            .as_deref()
            .map(|raw| serde_json::from_str::<Value>(raw).expect("parse pending merge receipt"))
    };

    assert!(
        !interrupted.status.success(),
        "post-commit publication interruption unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        interrupted.stdout,
        interrupted.stderr
    );
    let failure_output = format!("{}\n{}", interrupted.stdout, interrupted.stderr);
    assert!(
        failure_output.contains("committed_unwitnessed")
            || failure_output.contains("committed, but"),
        "failure did not report the committed-but-unpublished path:\n{failure_output}"
    );
    assert!(
        failure_output.contains("Failed to allocate pinned temporary export file"),
        "failure was not the injected post-commit publication denial:\n{failure_output}"
    );

    let committed_receipt =
        read_pending_receipt().expect("database commit must retain a resumable receipt");
    assert_eq!(committed_receipt["phase"], "database_committed");
    assert_eq!(committed_receipt["intent"]["retention_days"], 1);
    assert_eq!(committed_receipt["jsonl_after_issue_count"], 2);
    let receipt_id = committed_receipt["receipt_id"]
        .as_str()
        .expect("receipt ID")
        .to_string();
    let receipt_cutoff = chrono::DateTime::parse_from_rfc3339(
        committed_receipt["intent"]["export_as_of"]
            .as_str()
            .expect("receipt export cutoff"),
    )
    .expect("parse receipt export cutoff")
    .with_timezone(&Utc);
    assert!(
        retention_boundary - receipt_cutoff >= chrono::Duration::seconds(5),
        "fixture did not leave deterministic pre-boundary headroom: cutoff={receipt_cutoff} boundary={retention_boundary}"
    );

    let storage = SqliteStorage::open(&db_path).expect("open committed merge database");
    let persisted_boundary = storage
        .get_issue(&boundary_tombstone.id)
        .expect("read boundary tombstone")
        .expect("boundary tombstone persists");
    assert_eq!(persisted_boundary.deleted_at, Some(deleted_at));
    assert!(
        !persisted_boundary.is_expired_tombstone_at(Some(1), retention_boundary),
        "strict retention boundary must still retain the tombstone"
    );
    assert!(
        persisted_boundary.is_expired_tombstone_at(
            Some(1),
            retention_boundary + chrono::Duration::nanoseconds(1),
        ),
        "the first instant after the retention boundary must exclude the tombstone"
    );
    assert!(
        !persisted_boundary.is_expired_tombstone_at(Some(1), receipt_cutoff),
        "the receipt cutoff must retain the boundary tombstone"
    );

    let mut receipt_reviewed_bytes = Vec::new();
    beads_rust::sync::export_to_writer(&storage, &mut receipt_reviewed_bytes)
        .expect("reconstruct receipt-reviewed bytes from committed database");
    drop(storage);
    assert_eq!(
        receipt_reviewed_bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count(),
        committed_receipt["jsonl_after_issue_count"]
            .as_u64()
            .expect("receipt issue count") as usize
    );
    let reviewed_digest = Sha256::digest(&receipt_reviewed_bytes);
    assert_eq!(
        beads_rust::util::hex_encode(&reviewed_digest),
        committed_receipt["jsonl_after_raw_sha256"]
            .as_str()
            .expect("receipt raw hash"),
        "committed database must reproduce the exact receipt-reviewed bytes"
    );

    while Utc::now() <= retention_boundary {
        sleep(Duration::from_millis(50));
    }
    assert!(
        persisted_boundary.is_expired_tombstone(Some(1)),
        "wall clock must cross the boundary before resume"
    );

    let resumed = run_br(
        &workspace,
        ["sync", "--merge", "--allow-external-jsonl", "--json"],
        "resume_merge_with_persisted_cutoff",
    );
    assert!(
        resumed.status.success(),
        "receipt resume failed after wall clock crossed the boundary\nstdout:\n{}\nstderr:\n{}",
        resumed.stdout,
        resumed.stderr
    );
    let resumed_json: Value =
        serde_json::from_str(&extract_json_payload(&resumed.stdout)).expect("parse resume output");
    assert_eq!(resumed_json["status"], "resumed");
    assert_eq!(resumed_json["receipt_id"], receipt_id);
    assert_eq!(resumed_json["phase_before"], "database_committed");

    let published_bytes = fs::read(&jsonl_path).expect("read resumed JSONL");
    assert_eq!(
        published_bytes, receipt_reviewed_bytes,
        "resume must publish the exact bytes reviewed at the persisted receipt cutoff"
    );
    assert_eq!(
        fs::read(&base_path).expect("read resumed base"),
        published_bytes,
        "terminal base must adopt the exact resumed JSONL generation"
    );
    assert!(
        read_pending_receipt().is_none(),
        "terminal adoption must clear the pending receipt"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn e2e_pending_merge_gate_refuses_file_only_mutations_without_changing_witnesses() {
    let _log = common::test_log(
        "e2e_pending_merge_gate_refuses_file_only_mutations_without_changing_witnesses",
    );
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init_pending_file_mutation_gate");
    assert_br_success(&init, "pending file-mutation gate init");

    let create = run_br(
        &workspace,
        ["create", "Database title before interrupted merge"],
        "create_pending_file_mutation_gate_issue",
    );
    assert_br_success(&create, "seed pending file-mutation gate issue");
    let issue_id = parse_created_id(&create.stdout);
    assert!(
        !issue_id.is_empty(),
        "pending file-mutation gate fixture did not report a created issue ID"
    );
    let flush = run_br(
        &workspace,
        ["sync", "--flush-only"],
        "flush_pending_file_mutation_gate_issue",
    );
    assert_br_success(&flush, "flush pending file-mutation gate issue");

    let beads_dir = workspace.root.join(".beads");
    let db_path = beads_dir.join("beads.db");
    let metadata_path = beads_dir.join("metadata.json");
    let base_path = beads_dir.join("beads.base.jsonl");
    let internal_jsonl_path = beads_dir.join("issues.jsonl");
    let external_dir = workspace.root.join("external-jsonl");
    let jsonl_path = external_dir.join("issues.jsonl");
    let agents_path = workspace.root.join("AGENTS.md");
    let user_config_path = workspace
        .root
        .join(".config")
        .join("beads")
        .join("config.yaml");

    fs::write(
        &agents_path,
        b"# Workspace instructions\n\nPending-gate sentinel.\n",
    )
    .expect("seed AGENTS.md sentinel");
    fs::copy(&internal_jsonl_path, &base_path).expect("seed merge base");
    fs::create_dir_all(&external_dir).expect("create external JSONL directory");

    let mut external_issue: Value = serde_json::from_slice(
        &fs::read(&internal_jsonl_path).expect("read initial JSONL generation"),
    )
    .expect("parse initial JSONL issue");
    external_issue["title"] = Value::String("JSONL title committed by merge".to_string());
    external_issue["updated_at"] = Value::String("2999-01-01T00:00:00Z".to_string());
    fs::write(
        &jsonl_path,
        format!(
            "{}\n",
            serde_json::to_string(&external_issue).expect("serialize changed external issue")
        ),
    )
    .expect("write changed external JSONL generation");

    let mut metadata: Value =
        serde_json::from_slice(&fs::read(&metadata_path).expect("read workspace metadata"))
            .expect("parse workspace metadata");
    metadata["jsonl_export"] = Value::String(jsonl_path.to_string_lossy().into_owned());
    fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&metadata).expect("serialize workspace metadata"),
    )
    .expect("route sync to external JSONL");

    let interrupted = run_sync_merge_with_exhausted_publication_names(
        &workspace,
        &jsonl_path,
        "install_database_committed_pending_receipt",
    );

    assert!(
        !interrupted.status.success(),
        "post-commit publication interruption unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        interrupted.stdout,
        interrupted.stderr
    );
    let interruption_output = format!("{}\n{}", interrupted.stdout, interrupted.stderr);
    assert!(
        interruption_output.contains("committed_unwitnessed")
            || interruption_output.contains("committed, but"),
        "fixture did not stop after the database commit:\n{interruption_output}"
    );
    assert!(
        interruption_output.contains("Failed to allocate pinned temporary export file"),
        "fixture did not fail at the injected publication denial:\n{interruption_output}"
    );

    let read_pending_receipt = || {
        let connection =
            Connection::open(db_path.to_string_lossy().into_owned()).expect("open raw merge DB");
        let rows = connection
            .query_with_params(
                "SELECT value FROM metadata WHERE key = ? ORDER BY rowid DESC",
                &[SqliteValue::from("sync_merge_pending_v2")],
            )
            .expect("query pending merge receipt");
        let raw = rows
            .first()
            .and_then(|row| row.get(0))
            .and_then(SqliteValue::as_text)
            .expect("pending merge receipt row")
            .to_owned();
        connection
            .close()
            .expect("close raw receipt-inspection connection");
        let parsed = serde_json::from_str::<Value>(&raw).expect("parse pending merge receipt");
        (raw, parsed)
    };
    let read_issue_witness = || {
        let connection =
            Connection::open(db_path.to_string_lossy().into_owned()).expect("open raw merge DB");
        let rows = connection
            .query_with_params("SELECT id, title, status FROM issues ORDER BY id", &[])
            .expect("query issue logical witness");
        let witness = rows
            .iter()
            .map(|row| {
                [
                    row.get(0)
                        .and_then(SqliteValue::as_text)
                        .expect("issue ID")
                        .to_owned(),
                    row.get(1)
                        .and_then(SqliteValue::as_text)
                        .expect("issue title")
                        .to_owned(),
                    row.get(2)
                        .and_then(SqliteValue::as_text)
                        .expect("issue status")
                        .to_owned(),
                ]
            })
            .collect::<Vec<_>>();
        connection
            .close()
            .expect("close raw issue-inspection connection");
        witness
    };
    let database_family_snapshot = || {
        ["", "-wal", "-shm", "-journal"]
            .into_iter()
            .map(|suffix| {
                let path = PathBuf::from(format!("{}{suffix}", db_path.display()));
                let bytes = match fs::read(&path) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => panic!("read database-family member {}: {error}", path.display()),
                };
                (suffix, bytes)
            })
            .collect::<Vec<_>>()
    };

    let (receipt_raw_before, receipt_before) = read_pending_receipt();
    assert_eq!(receipt_before["schema_version"], 2);
    assert_eq!(receipt_before["phase"], "database_committed");
    let receipt_id = receipt_before["receipt_id"]
        .as_str()
        .expect("pending receipt ID")
        .to_owned();
    assert_eq!(
        receipt_id.len(),
        64,
        "valid receipt ID must be a SHA-256 digest"
    );
    let issue_witness_before = read_issue_witness();
    assert_eq!(
        issue_witness_before,
        vec![[
            issue_id,
            "JSONL title committed by merge".to_string(),
            "open".to_string(),
        ]],
        "fixture must expose the database-committed logical poststate"
    );

    let database_family_before = database_family_snapshot();
    let metadata_before = fs::read(&metadata_path).expect("read metadata baseline");
    let agents_before = fs::read(&agents_path).expect("read AGENTS baseline");
    let jsonl_before = fs::read(&jsonl_path).expect("read JSONL baseline");
    let base_before = fs::read(&base_path).expect("read merge-base baseline");
    assert!(
        !user_config_path.exists(),
        "fixture must begin without a user config so an ungated edit is observable"
    );

    let assert_refused = |action: &str, run: &BrRun| {
        assert!(
            !run.status.success(),
            "{action} unexpectedly crossed the pending-merge gate\nstdout:\n{}\nstderr:\n{}",
            run.stdout,
            run.stderr
        );
        let rendered = format!("{}\n{}", run.stdout, run.stderr);
        assert!(
            rendered.contains("Refusing non-merge mutation")
                && rendered.contains("pending sync-merge state is valid")
                && rendered.contains("phase=database_committed")
                && rendered.contains(&receipt_id)
                && rendered.contains("br sync --merge"),
            "{action} returned the wrong refusal diagnostic:\n{rendered}"
        );
    };
    let assert_unchanged = |action: &str| {
        assert_eq!(
            database_family_snapshot(),
            database_family_before,
            "{action} changed database-family bytes"
        );
        assert_eq!(
            fs::read(&metadata_path).expect("read metadata after refusal"),
            metadata_before,
            "{action} changed workspace metadata bytes"
        );
        assert_eq!(
            fs::read(&agents_path).expect("read AGENTS after refusal"),
            agents_before,
            "{action} changed AGENTS.md bytes"
        );
        assert_eq!(
            fs::read(&jsonl_path).expect("read JSONL after refusal"),
            jsonl_before,
            "{action} changed the merge-owned JSONL generation"
        );
        assert_eq!(
            fs::read(&base_path).expect("read merge base after refusal"),
            base_before,
            "{action} changed the merge-base generation"
        );
        assert!(
            !user_config_path.exists(),
            "{action} created the user config before receipt reconciliation"
        );
        let (receipt_raw_after, receipt_after) = read_pending_receipt();
        assert_eq!(
            receipt_raw_after, receipt_raw_before,
            "{action} changed the exact pending-receipt row"
        );
        assert_eq!(
            receipt_after, receipt_before,
            "{action} changed the parsed pending-receipt witness"
        );
        assert_eq!(
            read_issue_witness(),
            issue_witness_before,
            "{action} changed the committed issue poststate"
        );
    };

    let config_edit = common::cli::run_br_with_env(
        &workspace,
        ["config", "edit"],
        [("EDITOR", "true")],
        "pending_gate_config_edit",
    );
    assert_refused("br config edit", &config_edit);
    assert_unchanged("br config edit");

    let agents_add = run_br(
        &workspace,
        ["agents", "--add", "--force"],
        "pending_gate_agents_add",
    );
    assert_refused("br agents --add --force", &agents_add);
    assert_unchanged("br agents --add --force");
}

#[cfg(target_os = "linux")]
#[test]
fn e2e_sync_merge_capacity_warning_survives_receipt_resume_and_renders_human() {
    let _log = common::test_log(
        "e2e_sync_merge_capacity_warning_survives_receipt_resume_and_renders_human",
    );
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init_merge_capacity_warning");
    assert_br_success(&init, "merge-capacity warning init");

    let first_create = run_br(
        &workspace,
        ["create", "Receipt-bound soft-capacity transition"],
        "create_receipt_bound_capacity_issue",
    );
    assert_br_success(&first_create, "create receipt-bound capacity issue");
    let first_id = parse_created_id(&first_create.stdout);
    assert!(
        !first_id.is_empty(),
        "receipt-bound capacity fixture did not report a created issue ID"
    );
    let first_flush = run_br(
        &workspace,
        ["sync", "--flush-only"],
        "flush_receipt_bound_capacity_issue",
    );
    assert_br_success(&first_flush, "flush receipt-bound capacity issue");

    let beads_dir = workspace.root.join(".beads");
    let db_path = beads_dir.join("beads.db");
    let metadata_path = beads_dir.join("metadata.json");
    let policy_path = beads_dir.join("policy.yaml");
    let base_path = beads_dir.join("beads.base.jsonl");
    let internal_jsonl_path = beads_dir.join("issues.jsonl");
    let external_dir = workspace.root.join("external-jsonl");
    let jsonl_path = external_dir.join("issues.jsonl");
    fs::create_dir_all(&external_dir).expect("create external JSONL directory");
    fs::copy(&internal_jsonl_path, &base_path).expect("seed merge base");
    fs::copy(&internal_jsonl_path, &jsonl_path).expect("seed external JSONL");

    fs::write(
        &policy_path,
        r"
workflow:
  statuses: [open, in_progress, in_review, closed]
  capacity:
    statuses:
      in_progress:
        soft: 1
        hard: 2
",
    )
    .expect("write in-progress soft-capacity policy");

    let write_external_status = |issue_id: &str, status: &str| {
        let mut values = read_jsonl_values(&jsonl_path);
        let issue = values
            .iter_mut()
            .find(|issue| issue["id"].as_str() == Some(issue_id))
            .unwrap_or_else(|| panic!("external JSONL lacks issue {issue_id}"));
        issue["status"] = Value::String(status.to_string());
        issue["updated_at"] = Value::String("2999-01-01T00:00:00Z".to_string());
        let serialized = values
            .iter()
            .map(|issue| serde_json::to_string(issue).expect("serialize external issue"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&jsonl_path, serialized).expect("write changed external JSONL");
    };
    write_external_status(&first_id, "in_progress");

    let mut metadata: Value =
        serde_json::from_slice(&fs::read(&metadata_path).expect("read workspace metadata"))
            .expect("parse workspace metadata");
    metadata["jsonl_export"] = Value::String(jsonl_path.to_string_lossy().into_owned());
    fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&metadata).expect("serialize workspace metadata"),
    )
    .expect("route sync to external JSONL");

    // Force the real merge to stop after its database transaction has
    // committed the warning-bearing receipt but before JSONL publication.
    let interrupted = run_sync_merge_with_exhausted_publication_names(
        &workspace,
        &jsonl_path,
        "interrupt_capacity_warning_after_database_commit",
    );

    assert!(
        !interrupted.status.success(),
        "warning-bearing merge unexpectedly published instead of interrupting\nstdout:\n{}\nstderr:\n{}",
        interrupted.stdout,
        interrupted.stderr
    );
    let interruption_output = format!("{}\n{}", interrupted.stdout, interrupted.stderr);
    assert!(
        interruption_output.contains("committed_unwitnessed")
            || interruption_output.contains("committed, but"),
        "fixture did not stop after the warning-bearing database commit:\n{interruption_output}"
    );
    assert!(
        interruption_output.contains("Failed to allocate pinned temporary export file"),
        "fixture did not fail at the injected publication denial:\n{interruption_output}"
    );

    let read_pending_receipt = || {
        let connection =
            Connection::open(db_path.to_string_lossy().into_owned()).expect("open raw merge DB");
        let rows = connection
            .query_with_params(
                "SELECT value FROM metadata WHERE key = ? ORDER BY rowid DESC",
                &[SqliteValue::from("sync_merge_pending_v2")],
            )
            .expect("query pending merge receipt");
        let raw = rows
            .first()
            .and_then(|row| row.get(0))
            .and_then(SqliteValue::as_text)
            .map(str::to_owned);
        connection
            .close()
            .expect("close raw receipt-inspection connection");
        raw.map(|raw| {
            let parsed = serde_json::from_str::<Value>(&raw).expect("parse pending merge receipt");
            (raw, parsed)
        })
    };
    let (receipt_raw, committed_receipt) =
        read_pending_receipt().expect("database commit must retain a warning-bearing receipt");
    assert!(
        receipt_raw.contains("\"capacity_warnings\""),
        "exact raw receipt omitted capacity-warning evidence"
    );
    assert_eq!(committed_receipt["phase"], "database_committed");
    let receipt_id = committed_receipt["receipt_id"]
        .as_str()
        .expect("pending receipt ID")
        .to_owned();
    let receipt_warnings = committed_receipt["capacity_warnings"]
        .as_array()
        .expect("receipt capacity warnings");
    assert_eq!(
        receipt_warnings.len(),
        1,
        "one kept transition must produce one receipt-bound warning"
    );
    let receipt_warning = &receipt_warnings[0];
    assert_eq!(receipt_warning["issue_id"], first_id);
    assert_eq!(receipt_warning["from_status"], "open");
    assert_eq!(receipt_warning["to_status"], "in_progress");
    assert_eq!(receipt_warning["capacity_kind"], "status");
    assert_eq!(receipt_warning["capacity_name"], "in_progress");
    assert_eq!(receipt_warning["scope"], "repository");
    assert_eq!(receipt_warning["counting_mode"], "all");
    assert_eq!(receipt_warning["current"], 0);
    assert_eq!(receipt_warning["prospective"], 1);
    assert_eq!(receipt_warning["soft_limit"], 1);
    assert_eq!(receipt_warning["hard_limit"], 2);
    assert_eq!(
        receipt_warning["policy_path"],
        "workflow.capacity.statuses.in_progress"
    );
    let exact_receipt_warnings = committed_receipt["capacity_warnings"].clone();

    let resumed = run_br(
        &workspace,
        ["sync", "--merge", "--allow-external-jsonl", "--json"],
        "resume_receipt_bound_capacity_warning",
    );
    assert!(
        resumed.status.success(),
        "warning-bearing receipt resume failed\nstdout:\n{}\nstderr:\n{}",
        resumed.stdout,
        resumed.stderr
    );
    let resumed_json: Value =
        serde_json::from_str(&extract_json_payload(&resumed.stdout)).expect("parse resume JSON");
    assert_eq!(resumed_json["status"], "resumed");
    assert_eq!(resumed_json["receipt_id"], receipt_id);
    assert_eq!(resumed_json["phase_before"], "database_committed");
    assert_eq!(
        resumed_json["warnings"], exact_receipt_warnings,
        "machine output must replay the exact warning evidence bound into the committed receipt"
    );
    assert!(
        read_pending_receipt().is_none(),
        "successful warning-bearing resume must clear its receipt"
    );

    // Reuse the reconciled workspace for a second, independent soft-capacity
    // transition so the human warning path is covered without duplicating the
    // interruption fixture.
    //
    // Non-sync commands intentionally cannot opt into an external JSONL path.
    // Route only this genuine CLI create through the internal JSONL, with both
    // automatic import and export disabled, then restore the exact reviewed
    // external route before the explicitly authorized flush.
    let external_route_metadata =
        fs::read(&metadata_path).expect("save exact external-route metadata");
    let mut internal_route_metadata: Value =
        serde_json::from_slice(&external_route_metadata).expect("parse external-route metadata");
    internal_route_metadata["jsonl_export"] =
        Value::String(internal_jsonl_path.to_string_lossy().into_owned());
    fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&internal_route_metadata)
            .expect("serialize temporary internal-route metadata"),
    )
    .expect("temporarily route create to internal JSONL");
    let second_create = run_br(
        &workspace,
        [
            "create",
            "Human soft-capacity transition",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "create_human_capacity_issue",
    );
    assert_br_success(&second_create, "create human capacity issue");
    fs::write(&metadata_path, &external_route_metadata)
        .expect("restore exact external-route metadata");
    assert_eq!(
        fs::read(&metadata_path).expect("verify restored external-route metadata"),
        external_route_metadata,
        "second-phase create did not restore the exact reviewed external route"
    );
    let second_id = parse_created_id(&second_create.stdout);
    assert!(
        !second_id.is_empty(),
        "human capacity fixture did not report a created issue ID"
    );
    let second_flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--allow-external-jsonl"],
        "flush_human_capacity_issue",
    );
    assert_br_success(&second_flush, "flush human capacity issue");
    fs::copy(&jsonl_path, &base_path).expect("refresh base before human merge");
    fs::write(
        &policy_path,
        r"
workflow:
  statuses: [open, in_progress, in_review, closed]
  capacity:
    statuses:
      in_review:
        soft: 1
        hard: 2
",
    )
    .expect("write in-review soft-capacity policy");
    write_external_status(&second_id, "in_review");

    let human_merge = run_br(
        &workspace,
        ["sync", "--merge", "--allow-external-jsonl"],
        "merge_human_capacity_warning",
    );
    assert!(
        human_merge.status.success(),
        "human warning merge failed\nstdout:\n{}\nstderr:\n{}",
        human_merge.stdout,
        human_merge.stderr
    );
    assert!(
        human_merge.stdout.contains("Merge complete:")
            && human_merge.stdout.contains("JSONL exported."),
        "human merge success output is incomplete:\n{}",
        human_merge.stdout
    );
    let human_warning = &human_merge.stderr;
    assert!(
        human_warning.contains(&format!(
            "Warning: transitioned {second_id} from open to in_review"
        )) && human_warning.contains("repository status capacity 'in_review'")
            && human_warning.contains("current: 0, prospective: 1, soft: 1")
            && human_warning.contains("workflow.capacity.statuses.in_review")
            && human_warning.contains("Drain existing work before admitting more"),
        "human merge omitted actionable soft-capacity evidence:\n{human_warning}"
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

#[cfg(target_os = "linux")]
#[test]
fn e2e_no_db_sync_jsonl_rewriters_lock_before_loading_the_snapshot() {
    let _log = common::test_log("e2e_no_db_sync_jsonl_rewriters_lock_before_loading_the_snapshot");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init_no_db_sync_lock");
    assert_br_success(&init, "init failed");
    let create = run_br(
        &workspace,
        ["create", "No-DB sync lock seed"],
        "create_no_db_sync_lock",
    );
    assert_br_success(&create, "seed create failed");
    let initial_flush = run_br(
        &workspace,
        ["sync", "--flush-only"],
        "initial_no_db_sync_lock_flush",
    );
    assert_br_success(&initial_flush, "initial flush failed");

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let valid_before = fs::read(&jsonl_path).expect("read JSONL before lock contention");
    let authority = blocking_jsonl_family_write_lock_with_timeout(&jsonl_path, Some(1_000))
        .expect("hold cooperative JSONL-family authority");
    let malformed = b"{ definitely-not-valid-json\n";
    fs::write(&jsonl_path, malformed).expect("install malformed JSONL ordering witness");
    authority
        .verify_jsonl_authority()
        .expect("in-place malformed witness must retain the held inode authority");

    let mut modes = vec![vec!["sync", "--flush-only"], vec!["sync", "--merge"]];
    for resolution in ["--force", "--force-db", "--force-jsonl"] {
        modes.push(vec!["sync", "--merge", resolution]);
    }
    for (index, mode) in modes.into_iter().enumerate() {
        let mut args = vec!["--no-db", "--lock-timeout", "25"];
        args.extend(mode);
        let run = run_br(
            &workspace,
            args,
            &format!("contended_no_db_sync_mode_{index}"),
        );
        assert!(
            !run.status.success(),
            "contended no-DB JSONL rewriter unexpectedly succeeded: stdout={} stderr={}",
            run.stdout,
            run.stderr
        );
        assert!(
            run.stderr.contains("JSONL-family write lock")
                || run.stderr.contains("JSONL-family write authority"),
            "the JSONL authority must fail before malformed snapshot parsing: {}",
            run.stderr
        );
        assert!(
            !run.stderr.contains("Invalid JSON")
                && !run.stderr.contains("invalid JSON")
                && !run.stderr.contains("expected value"),
            "snapshot parsing ran before JSONL authority acquisition: {}",
            run.stderr
        );
        assert_eq!(
            fs::read(&jsonl_path).expect("read JSONL after contended command"),
            malformed,
            "a contended no-DB sync mode must not rewrite the JSONL family"
        );
    }

    authority
        .verify_jsonl_authority()
        .expect("held authority must remain valid after rejected competitors");
    drop(authority);

    let parse_after_release = run_br(
        &workspace,
        ["--no-db", "--lock-timeout", "1000", "sync", "--flush-only"],
        "malformed_no_db_sync_after_authority_release",
    );
    assert!(
        !parse_after_release.status.success(),
        "malformed JSONL unexpectedly parsed after authority release"
    );
    assert!(
        parse_after_release.stderr.contains("Invalid JSON")
            || parse_after_release.stderr.contains("invalid JSON")
            || parse_after_release.stderr.contains("expected value"),
        "after authority release the malformed snapshot should reach parsing: {}",
        parse_after_release.stderr
    );
    fs::write(&jsonl_path, &valid_before).expect("restore valid JSONL after ordering witness");
    let successful_after_release = run_br(
        &workspace,
        ["--no-db", "--lock-timeout", "1000", "sync", "--flush-only"],
        "valid_no_db_sync_after_authority_release",
    );
    assert_br_success(
        &successful_after_release,
        "valid no-DB flush failed after authority release",
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
fn dep_import_auto_flushes_imported_edges_to_jsonl() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "dep_import_auto_flush_init");
    assert_br_success(&init, "init failed");

    let source = run_br(
        &workspace,
        ["create", "Bulk import source"],
        "dep_import_auto_flush_source",
    );
    assert_br_success(&source, "source create failed");
    let source_id = parse_created_id(&source.stdout);

    let target = run_br(
        &workspace,
        ["create", "Bulk import target"],
        "dep_import_auto_flush_target",
    );
    assert_br_success(&target, "target create failed");
    let target_id = parse_created_id(&target.stdout);

    let import_path = workspace.root.join("edges.jsonl");
    fs::write(
        &import_path,
        format!(
            "{{\"issue_id\":\"{}\",\"depends_on_id\":\"{}\",\"type\":\"blocks\"}}\n",
            source_id, target_id
        ),
    )
    .expect("write dependency import jsonl");

    let import_arg = import_path.to_string_lossy().to_string();
    let import = run_br(
        &workspace,
        ["dep", "import", import_arg.as_str(), "--robot"],
        "dep_import_auto_flush_import",
    );
    assert_br_success(&import, "dep import failed");
    let import_result: Value =
        serde_json::from_str(&extract_json_payload(&import.stdout)).expect("parse import result");
    assert_eq!(import_result["imported"].as_u64(), Some(1));

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let exported = read_jsonl_values(&jsonl_path);
    let source_record = exported
        .iter()
        .find(|issue| issue["id"].as_str() == Some(source_id.as_str()))
        .expect("source issue exported after dep import");
    assert!(
        source_record["dependencies"]
            .as_array()
            .is_some_and(|deps| deps.iter().any(|dep| {
                dep["depends_on_id"].as_str() == Some(target_id.as_str())
                    && dep["type"].as_str() == Some("blocks")
            })),
        "dep import should auto-flush imported edges into issues.jsonl"
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
fn e2e_sync_additive_reconciliation_is_read_only_then_lossless_and_idempotent() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init_additive_reconciliation");
    assert_br_success(&init, "additive reconciliation init");

    let create = run_br(
        &workspace,
        [
            "create",
            "Database audit seed",
            "--id",
            "bd-db-seed",
            "--no-auto-flush",
        ],
        "create_additive_database_seed",
    );
    assert_br_success(&create, "create additive database seed");
    let create_db_only = run_br(
        &workspace,
        [
            "create",
            "Database-only preserved row",
            "--id",
            "bd-db-only",
            "--no-auto-flush",
        ],
        "create_additive_database_only_row",
    );
    assert_br_success(
        &create_db_only,
        "create additive database-only preserved row",
    );

    let beads_dir = workspace.root.join(".beads");
    let db_path = beads_dir.join("beads.db");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let storage = SqliteStorage::open(&db_path).expect("open additive database before plan");
    let database_seed = storage
        .get_issue("bd-db-seed")
        .expect("read database seed")
        .expect("database seed exists");
    let events_before = storage.get_all_events(0).expect("read events before plan");

    let jsonl_only = make_issue("bd-jsonl-only", "JSONL-only recovery row", Utc::now());
    let source = [&database_seed, &jsonl_only]
        .into_iter()
        .map(|issue| serde_json::to_string(issue).expect("serialize additive source issue"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&jsonl_path, source.as_bytes()).expect("write additive source JSONL");
    let source_before = fs::read(&jsonl_path).expect("read additive source before plan");
    let database_family_snapshot = || {
        ["", "-wal", "-shm", "-journal"]
            .into_iter()
            .filter_map(|suffix| {
                let path = PathBuf::from(format!("{}{}", db_path.display(), suffix));
                path.exists().then(|| {
                    (
                        path.file_name()
                            .expect("database-family filename")
                            .to_string_lossy()
                            .into_owned(),
                        fs::read(&path).expect("read database-family member"),
                    )
                })
            })
            .collect::<Vec<_>>()
    };
    let database_family_before_plan = database_family_snapshot();

    let plan = run_br(
        &workspace,
        ["sync", "--reconcile-additive", "--json"],
        "sync_additive_plan",
    );
    assert!(
        plan.status.success(),
        "additive dry-run failed\nstdout:\n{}\nstderr:\n{}",
        plan.stdout,
        plan.stderr
    );
    let plan_json: Value = serde_json::from_str(&extract_json_payload(&plan.stdout))
        .expect("parse additive dry-run receipt");
    assert_eq!(plan_json["schema"], "br.sync.additive-reconciliation.v2");
    assert_eq!(plan_json["status"], "ready");
    assert_eq!(plan_json["source_issues"].as_u64(), Some(2));
    assert_eq!(plan_json["created"].as_u64(), Some(1));
    assert_eq!(plan_json["skipped_equal"].as_u64(), Some(1));
    assert_eq!(plan_json["db_only_preserved"].as_u64(), Some(1));
    assert_eq!(plan_json["deleted"].as_u64(), Some(0));
    assert_eq!(plan_json["jsonl_written"], false);
    assert!(plan_json["target_after"].is_null());
    assert!(
        plan_json["expected_target_after"].is_object(),
        "dry-run must publish the complete expected typed poststate"
    );
    let reviewed_plan_sha256 = plan_json["plan_sha256"]
        .as_str()
        .expect("dry-run receipt plan_sha256")
        .to_string();
    assert_eq!(
        database_family_snapshot(),
        database_family_before_plan,
        "dry-run must leave every existing database-family file byte-identical"
    );

    assert!(
        storage
            .get_issue("bd-jsonl-only")
            .expect("probe JSONL-only issue after plan")
            .is_none(),
        "dry-run must not mutate the database"
    );
    assert_eq!(
        storage.get_all_events(0).expect("events after dry-run"),
        events_before,
        "dry-run must not mutate the audit event stream"
    );
    drop(storage);
    assert_eq!(
        fs::read(&jsonl_path).expect("read JSONL after dry-run"),
        source_before,
        "dry-run must not rewrite JSONL"
    );

    let mismatched_apply = run_br(
        &workspace,
        vec![
            "sync".to_string(),
            "--reconcile-additive".to_string(),
            "--apply".to_string(),
            "--expect-plan-sha256".to_string(),
            "0".repeat(64),
            "--json".to_string(),
        ],
        "sync_additive_apply_mismatched_token",
    );
    assert!(
        !mismatched_apply.status.success(),
        "mismatched plan token must fail closed"
    );
    assert_eq!(
        mismatched_apply.status.code(),
        Some(6),
        "stale reviewed tokens use the documented sync-conflict exit code"
    );
    assert_eq!(
        database_family_snapshot(),
        database_family_before_plan,
        "stale-token rejection must preserve every existing database-family byte"
    );
    let storage =
        SqliteStorage::open(&db_path).expect("open additive database after rejected apply");
    assert!(
        storage
            .get_issue("bd-jsonl-only")
            .expect("probe JSONL-only issue after rejected apply")
            .is_none(),
        "rejected apply must not create the planned issue"
    );
    assert_eq!(
        storage
            .get_all_events(0)
            .expect("events after rejected apply"),
        events_before
    );
    drop(storage);

    let apply = run_br(
        &workspace,
        vec![
            "sync".to_string(),
            "--reconcile-additive".to_string(),
            "--apply".to_string(),
            "--expect-plan-sha256".to_string(),
            reviewed_plan_sha256,
            "--json".to_string(),
        ],
        "sync_additive_apply",
    );
    assert!(
        apply.status.success(),
        "additive apply failed\nstdout:\n{}\nstderr:\n{}",
        apply.stdout,
        apply.stderr
    );
    let apply_json: Value = serde_json::from_str(&extract_json_payload(&apply.stdout))
        .expect("parse additive apply receipt");
    assert_eq!(apply_json["status"], "applied");
    assert_eq!(apply_json["created"].as_u64(), Some(1));
    assert_eq!(apply_json["db_only_preserved"].as_u64(), Some(1));
    assert_eq!(apply_json["deleted"].as_u64(), Some(0));
    assert_eq!(apply_json["events_before"], apply_json["events_after"]);
    assert_eq!(
        apply_json["event_payload_sha256_before"],
        apply_json["event_payload_sha256_after"]
    );
    assert_eq!(apply_json["cache_rebuild_performed"], true);
    assert_eq!(
        apply_json["export_hashes_updated"],
        apply_json["export_hash_updates_planned"]
    );
    assert_eq!(
        apply_json["dirty_markers_cleared"],
        apply_json["dirty_markers_clear_planned"]
    );
    assert_eq!(apply_json["jsonl_written"], false);
    assert_eq!(apply_json["base_snapshot_used"], false);
    assert_eq!(apply_json["merge_note_written"], false);
    assert_eq!(
        apply_json["target_after"], plan_json["expected_target_after"],
        "apply must land the complete poststate published by the reviewed dry-run"
    );

    let storage = SqliteStorage::open(&db_path).expect("open additive database after apply");
    assert_eq!(
        storage
            .get_issue("bd-jsonl-only")
            .expect("read recovered issue")
            .expect("recovered issue exists")
            .title,
        jsonl_only.title
    );
    assert!(
        storage
            .get_issue("bd-db-seed")
            .expect("read preserved database seed")
            .is_some(),
        "pre-existing database issue must be preserved"
    );
    assert!(
        storage
            .get_issue("bd-db-only")
            .expect("read database-only issue")
            .is_some(),
        "database-only issue must be preserved"
    );
    assert_eq!(
        storage.get_all_events(0).expect("events after apply"),
        events_before,
        "apply must preserve the audit event stream byte-for-byte"
    );
    drop(storage);
    assert_eq!(
        fs::read(&jsonl_path).expect("read JSONL after apply"),
        source_before,
        "apply must not rewrite its source JSONL"
    );
    assert!(!beads_dir.join("beads.base.jsonl").exists());
    assert!(!beads_dir.join("merge.json").exists());

    let idempotent_plan = run_br(
        &workspace,
        ["sync", "--reconcile-additive", "--json"],
        "sync_additive_idempotent_plan",
    );
    assert!(
        idempotent_plan.status.success(),
        "idempotent additive plan failed\nstdout:\n{}\nstderr:\n{}",
        idempotent_plan.stdout,
        idempotent_plan.stderr
    );
    let idempotent_json: Value =
        serde_json::from_str(&extract_json_payload(&idempotent_plan.stdout))
            .expect("parse idempotent additive receipt");
    assert_eq!(idempotent_json["status"], "no_changes");
    assert_eq!(idempotent_json["created"].as_u64(), Some(0));
    assert_eq!(idempotent_json["updated"].as_u64(), Some(0));
    assert_eq!(idempotent_json["deleted"].as_u64(), Some(0));
    assert_eq!(idempotent_json["metadata_update_planned"], false);
    assert_eq!(
        idempotent_json["export_hash_updates_planned"].as_u64(),
        Some(0)
    );
    assert_eq!(
        idempotent_json["dirty_markers_clear_planned"].as_u64(),
        Some(0)
    );
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
