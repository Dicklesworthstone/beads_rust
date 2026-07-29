mod common;

use common::cli::{
    BrWorkspace, extract_issues_array, extract_json_payload, run_br, run_br_with_env,
};
use serde_json::Value;

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    // Handle both formats: "Created bd-xxx: title" and "✓ Created bd-xxx: title"
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    let id_part = normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

#[test]
#[allow(clippy::similar_names, clippy::too_many_lines)]
fn e2e_queries_ready_stale_count_search() {
    let _log = common::test_log("e2e_queries_ready_stale_count_search");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let blocker = run_br(
        &workspace,
        ["create", "Blocker issue", "-p", "1"],
        "create_blocker",
    );
    assert!(
        blocker.status.success(),
        "blocker create failed: {}",
        blocker.stderr
    );
    let blocker_id = parse_created_id(&blocker.stdout);

    let blocked = run_br(
        &workspace,
        ["create", "Blocked issue", "-p", "2"],
        "create_blocked",
    );
    assert!(
        blocked.status.success(),
        "blocked create failed: {}",
        blocked.stderr
    );
    let blocked_id = parse_created_id(&blocked.stdout);

    let deferred = run_br(
        &workspace,
        ["create", "Deferred issue", "-p", "3"],
        "create_deferred",
    );
    assert!(
        deferred.status.success(),
        "deferred create failed: {}",
        deferred.stderr
    );
    let deferred_id = parse_created_id(&deferred.stdout);

    let closed = run_br(
        &workspace,
        ["create", "Closed issue", "-p", "0"],
        "create_closed",
    );
    assert!(
        closed.status.success(),
        "closed create failed: {}",
        closed.stderr
    );
    let closed_id = parse_created_id(&closed.stdout);

    let label_blocker = run_br(
        &workspace,
        ["update", &blocker_id, "--add-label", "core"],
        "label_blocker",
    );
    assert!(
        label_blocker.status.success(),
        "label update failed: {}",
        label_blocker.stderr
    );

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &blocked_id, &blocker_id],
        "dep_add",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let defer_issue = run_br(
        &workspace,
        [
            "update",
            &deferred_id,
            "--status",
            "deferred",
            "--defer",
            "2100-01-01T00:00:00Z",
        ],
        "defer_issue",
    );
    assert!(
        defer_issue.status.success(),
        "defer update failed: {}",
        defer_issue.stderr
    );

    // beads_rust#301: `br update --status closed` is rejected; use the
    // dedicated `br close` command so close-policy is enforced uniformly.
    let close_issue = run_br(
        &workspace,
        [
            "close",
            &closed_id,
            "--reason",
            "fixture: ready-after-close",
        ],
        "close_issue",
    );
    assert!(
        close_issue.status.success(),
        "close failed: {}",
        close_issue.stderr
    );

    let ready = run_br(&workspace, ["ready", "--json"], "ready");
    assert!(ready.status.success(), "ready failed: {}", ready.stderr);
    let ready_payload = extract_json_payload(&ready.stdout);
    let ready_json: Vec<Value> = serde_json::from_str(&ready_payload).expect("ready json");
    assert!(ready_json.iter().any(|item| item["id"] == blocker_id));
    assert!(!ready_json.iter().any(|item| item["id"] == blocked_id));
    assert!(!ready_json.iter().any(|item| item["id"] == deferred_id));

    let ready_text = run_br(&workspace, ["ready"], "ready_text");
    assert!(
        ready_text.status.success(),
        "ready text failed: {}",
        ready_text.stderr
    );
    assert!(
        ready_text.stdout.contains("Ready work"),
        "ready text missing header"
    );

    let ready_core = run_br(
        &workspace,
        ["ready", "--json", "--label", "core"],
        "ready_label",
    );
    assert!(
        ready_core.status.success(),
        "ready label failed: {}",
        ready_core.stderr
    );
    let ready_core_payload = extract_json_payload(&ready_core.stdout);
    let ready_core_json: Vec<Value> =
        serde_json::from_str(&ready_core_payload).expect("ready label json");
    assert_eq!(ready_core_json.len(), 1);
    assert_eq!(ready_core_json[0]["id"], blocker_id);

    let blocked = run_br(&workspace, ["blocked", "--json"], "blocked");
    assert!(
        blocked.status.success(),
        "blocked failed: {}",
        blocked.stderr
    );
    let blocked_payload = extract_json_payload(&blocked.stdout);
    let blocked_json: Vec<Value> = serde_json::from_str(&blocked_payload).expect("blocked json");
    assert!(blocked_json.iter().any(|item| item["id"] == blocked_id));

    let blocked_text = run_br(&workspace, ["blocked"], "blocked_text");
    assert!(
        blocked_text.status.success(),
        "blocked text failed: {}",
        blocked_text.stderr
    );
    assert!(
        blocked_text.stdout.contains("Blocked issues"),
        "blocked text missing header"
    );

    let search = run_br(
        &workspace,
        ["search", "Blocker", "--status", "open", "--json"],
        "search",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);
    let search_payload = extract_json_payload(&search.stdout);
    let search_json: Vec<Value> = serde_json::from_str(&search_payload).expect("search json");
    assert!(search_json.iter().any(|item| item["id"] == blocker_id));

    let search_text = run_br(&workspace, ["search", "Blocker"], "search_text");
    assert!(
        search_text.status.success(),
        "search text failed: {}",
        search_text.stderr
    );
    assert!(
        search_text.stdout.contains("Blocker issue"),
        "search text missing issue title"
    );

    let count = run_br(
        &workspace,
        ["count", "--by", "status", "--include-closed", "--json"],
        "count",
    );
    assert!(count.status.success(), "count failed: {}", count.stderr);
    let count_payload = extract_json_payload(&count.stdout);
    let count_json: Value = serde_json::from_str(&count_payload).expect("count json");
    assert_eq!(count_json["total"], 4);

    let groups = count_json["groups"].as_array().expect("count groups array");
    let mut counts = std::collections::BTreeMap::new();
    for group in groups {
        let key = group["group"].as_str().unwrap_or("").to_string();
        let value = group["count"].as_u64().unwrap_or(0);
        counts.insert(key, value);
    }
    assert_eq!(counts.get("open"), Some(&2));
    assert_eq!(counts.get("deferred"), Some(&1));
    assert_eq!(counts.get("closed"), Some(&1));

    let count_text = run_br(
        &workspace,
        ["count", "--by", "status", "--include-closed"],
        "count_text",
    );
    assert!(
        count_text.status.success(),
        "count text failed: {}",
        count_text.stderr
    );
    assert!(
        count_text.stdout.contains("Total:"),
        "count text missing total"
    );

    let count_priority = run_br(
        &workspace,
        [
            "count",
            "--by",
            "priority",
            "--priority",
            "0",
            "--include-closed",
            "--json",
        ],
        "count_priority",
    );
    assert!(
        count_priority.status.success(),
        "count priority failed: {}",
        count_priority.stderr
    );
    let count_priority_payload = extract_json_payload(&count_priority.stdout);
    let count_priority_json: Value =
        serde_json::from_str(&count_priority_payload).expect("count priority json");
    assert_eq!(count_priority_json["total"], 1);

    let deferred_count = run_br(
        &workspace,
        ["count", "--status", "deferred", "--json"],
        "count_deferred",
    );
    assert!(
        deferred_count.status.success(),
        "count deferred failed: {}",
        deferred_count.stderr
    );
    let deferred_count_payload = extract_json_payload(&deferred_count.stdout);
    let deferred_count_json: Value =
        serde_json::from_str(&deferred_count_payload).expect("count deferred json");
    assert_eq!(deferred_count_json["count"], 1);

    let stale = run_br(&workspace, ["stale", "--days", "0", "--json"], "stale");
    assert!(stale.status.success(), "stale failed: {}", stale.stderr);
    let stale_payload = extract_json_payload(&stale.stdout);
    let stale_json: Vec<Value> = serde_json::from_str(&stale_payload).expect("stale json");
    assert!(stale_json.len() >= 2);
    assert!(stale_json.iter().any(|item| item["id"] == blocker_id));
    assert!(stale_json.iter().any(|item| item["id"] == blocked_id));
}

#[test]
fn e2e_query_run_inherits_env_json_output() {
    let _log = common::test_log("e2e_query_run_inherits_env_json_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Env query bug"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let save = run_br(
        &workspace,
        ["query", "save", "env-open", "--status", "open"],
        "query_save_env_open",
    );
    assert!(save.status.success(), "query save failed: {}", save.stderr);

    let run = run_br_with_env(
        &workspace,
        ["query", "run", "env-open"],
        [("BR_OUTPUT_FORMAT", "json")],
        "query_run_env_json",
    );
    assert!(
        run.status.success(),
        "query run with BR_OUTPUT_FORMAT=json failed: {}",
        run.stderr
    );

    let json = extract_issues_array(&run.stdout);
    assert_eq!(json.len(), 1);
    assert_eq!(json[0]["title"], "Env query bug");
}

/// E2E tests for stats command - text and JSON output.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_stats_command() {
    let _log = common::test_log("e2e_stats_command");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "stats_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create a few issues with different types and priorities
    let task1 = run_br(
        &workspace,
        ["create", "Task one", "-t", "task", "-p", "1"],
        "stats_create_task1",
    );
    assert!(task1.status.success(), "task1 failed: {}", task1.stderr);

    let bug1 = run_br(
        &workspace,
        ["create", "Bug one", "-t", "bug", "-p", "0"],
        "stats_create_bug1",
    );
    assert!(bug1.status.success(), "bug1 failed: {}", bug1.stderr);

    let feature1 = run_br(
        &workspace,
        ["create", "Feature one", "-t", "feature", "-p", "2"],
        "stats_create_feature1",
    );
    assert!(
        feature1.status.success(),
        "feature1 failed: {}",
        feature1.stderr
    );

    // Test stats text output
    let stats_text = run_br(&workspace, ["stats"], "stats_text");
    assert!(
        stats_text.status.success(),
        "stats text failed: {}",
        stats_text.stderr
    );
    assert!(
        stats_text.stdout.contains("Issue Database Status"),
        "stats text missing header"
    );
    assert!(
        stats_text.stdout.contains("Total Issues:"),
        "stats text missing total"
    );
    assert!(
        stats_text.stdout.contains("Open:"),
        "stats text missing open count"
    );

    // Test stats JSON output
    let stats_json = run_br(&workspace, ["stats", "--json"], "stats_json");
    assert!(
        stats_json.status.success(),
        "stats json failed: {}",
        stats_json.stderr
    );
    let stats_payload = extract_json_payload(&stats_json.stdout);
    let stats_parsed: Value = serde_json::from_str(&stats_payload).expect("stats json parse");
    assert!(stats_parsed["summary"]["total_issues"].as_u64().is_some());
    assert_eq!(stats_parsed["summary"]["total_issues"], 3);
    assert!(stats_parsed["summary"]["open_issues"].as_u64().is_some());

    // Test stats with --by-type
    let stats_by_type = run_br(&workspace, ["stats", "--by-type"], "stats_by_type");
    assert!(
        stats_by_type.status.success(),
        "stats by-type failed: {}",
        stats_by_type.stderr
    );
    assert!(
        stats_by_type.stdout.contains("By type:"),
        "stats by-type missing breakdown header"
    );
    assert!(
        stats_by_type.stdout.contains("task:") || stats_by_type.stdout.contains("task"),
        "stats by-type missing task type"
    );

    // Test stats with --by-priority
    let stats_by_priority = run_br(&workspace, ["stats", "--by-priority"], "stats_by_priority");
    assert!(
        stats_by_priority.status.success(),
        "stats by-priority failed: {}",
        stats_by_priority.stderr
    );
    assert!(
        stats_by_priority.stdout.contains("By priority:"),
        "stats by-priority missing breakdown header"
    );
    assert!(
        stats_by_priority.stdout.contains("P0:") || stats_by_priority.stdout.contains("P1:"),
        "stats by-priority missing priority levels"
    );

    // Test stats with multiple breakdowns
    let stats_combined = run_br(
        &workspace,
        ["stats", "--by-type", "--by-priority", "--json"],
        "stats_combined",
    );
    assert!(
        stats_combined.status.success(),
        "stats combined failed: {}",
        stats_combined.stderr
    );
    let combined_payload = extract_json_payload(&stats_combined.stdout);
    let combined_parsed: Value =
        serde_json::from_str(&combined_payload).expect("stats combined json parse");
    assert!(combined_parsed["summary"].is_object());

    // Check breakdowns array
    let breakdowns = combined_parsed["breakdowns"]
        .as_array()
        .expect("breakdowns array");
    assert!(!breakdowns.is_empty());

    // Verify specific breakdowns are present
    let has_type = breakdowns.iter().any(|b| b["dimension"] == "type");
    let has_priority = breakdowns.iter().any(|b| b["dimension"] == "priority");

    assert!(has_type, "missing type breakdown");
    assert!(has_priority, "missing priority breakdown");
}

/// E2E tests for config command - list, get, path.
#[test]
fn e2e_config_command() {
    let _log = common::test_log("e2e_config_command");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "config_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Test config list subcommand
    let config_list = run_br(&workspace, ["config", "list"], "config_list");
    assert!(
        config_list.status.success(),
        "config list failed: {}",
        config_list.stderr
    );
    // Config list output contains various settings sections
    assert!(
        config_list.stdout.contains("prefix")
            || config_list.stdout.contains("issue_prefix")
            || config_list.stdout.contains("Configuration"),
        "config list missing expected keys"
    );
    // Should show settings sections
    assert!(
        config_list.stdout.contains("settings")
            || config_list.stdout.contains("Current configuration"),
        "config list missing settings section"
    );

    // Test config get subcommand - use json key which is a startup setting
    let config_get = run_br(&workspace, ["config", "get", "json"], "config_get");
    // Config get for existing key should either succeed or return a structured error
    // (exit 1 = general error, exit 7 = config error). Verify it doesn't crash.
    assert!(
        matches!(config_get.status.code(), Some(0 | 1 | 7)),
        "config get returned unexpected exit code: {:?}",
        config_get.status.code()
    );

    // Test config path subcommand
    let config_path = run_br(&workspace, ["config", "path"], "config_path");
    assert!(
        config_path.status.success(),
        "config path failed: {}",
        config_path.stderr
    );
    assert!(
        config_path.stdout.contains("config.yaml")
            || config_path.stdout.contains("Config file paths"),
        "config path missing expected output"
    );

    // Test config list with --json output
    let config_json = run_br(&workspace, ["config", "list", "--json"], "config_json");
    assert!(
        config_json.status.success(),
        "config json failed: {}",
        config_json.stderr
    );
    // Should output valid JSON
    let config_payload = extract_json_payload(&config_json.stdout);
    let _: Value = serde_json::from_str(&config_payload).expect("config json parse");
}

/// E2E tests for reopen command.
#[test]
fn e2e_reopen_command() {
    let _log = common::test_log("e2e_reopen_command");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "reopen_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(
        &workspace,
        ["create", "Issue to reopen", "-p", "2"],
        "reopen_create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);
    assert!(!issue_id.is_empty(), "failed to parse created ID");

    // Close the issue
    let close = run_br(
        &workspace,
        ["close", &issue_id, "--reason", "Testing reopen"],
        "reopen_close",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);

    // Verify it's closed
    let show_closed = run_br(
        &workspace,
        ["show", &issue_id, "--json"],
        "reopen_show_closed",
    );
    assert!(
        show_closed.status.success(),
        "show closed failed: {}",
        show_closed.stderr
    );
    let show_closed_payload = extract_json_payload(&show_closed.stdout);
    let show_closed_json: Value =
        serde_json::from_str(&show_closed_payload).expect("show closed json");

    // br show returns a list, so we access the first element
    if show_closed_json.is_array() {
        assert_eq!(show_closed_json[0]["status"], "closed");
    } else {
        // Fallback if behavior changes to return object for single ID
        assert_eq!(show_closed_json["status"], "closed");
    }

    // Reopen the issue
    let reopen = run_br(
        &workspace,
        ["reopen", &issue_id, "--reason", "Need more work"],
        "reopen_reopen",
    );
    assert!(reopen.status.success(), "reopen failed: {}", reopen.stderr);
    assert!(
        reopen.stdout.contains("Reopened") || reopen.stdout.contains(&issue_id),
        "reopen text missing confirmation"
    );

    // Verify it's open again
    let show_reopened = run_br(
        &workspace,
        ["show", &issue_id, "--json"],
        "reopen_show_reopened",
    );
    assert!(
        show_reopened.status.success(),
        "show reopened failed: {}",
        show_reopened.stderr
    );
    let show_reopened_payload = extract_json_payload(&show_reopened.stdout);
    let show_reopened_json: Value =
        serde_json::from_str(&show_reopened_payload).expect("show reopened json");

    if show_reopened_json.is_array() {
        assert_eq!(show_reopened_json[0]["status"], "open");
    } else {
        assert_eq!(show_reopened_json["status"], "open");
    }

    // Test reopen with JSON output
    let close_again = run_br(&workspace, ["close", &issue_id], "reopen_close_again");
    assert!(
        close_again.status.success(),
        "close again failed: {}",
        close_again.stderr
    );

    let reopen_json = run_br(
        &workspace,
        ["reopen", &issue_id, "--json"],
        "reopen_reopen_json",
    );
    assert!(
        reopen_json.status.success(),
        "reopen json failed: {}",
        reopen_json.stderr
    );
    let reopen_payload = extract_json_payload(&reopen_json.stdout);
    let reopen_parsed: Value = serde_json::from_str(&reopen_payload).expect("reopen json parse");

    // Check reopened array
    let reopened = reopen_parsed["reopened"]
        .as_array()
        .expect("reopened array");
    assert_eq!(reopened.len(), 1);
    assert_eq!(reopened[0]["id"], issue_id);
    assert_eq!(reopened[0]["status"], "open");
}

#[test]
fn e2e_reopen_honors_env_json_mode() {
    let _log = common::test_log("e2e_reopen_honors_env_json_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "reopen_env_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Issue to reopen via env", "--json"],
        "reopen_env_create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value = serde_json::from_str(&extract_json_payload(&create.stdout))
        .expect("create should emit json");
    let issue_id = created["id"].as_str().expect("issue id");

    let close = run_br(&workspace, ["close", issue_id], "reopen_env_close");
    assert!(close.status.success(), "close failed: {}", close.stderr);

    let reopen = run_br_with_env(
        &workspace,
        ["reopen", issue_id],
        [("BR_OUTPUT_FORMAT", "json")],
        "reopen_env_json",
    );
    assert!(
        reopen.status.success(),
        "reopen with env json failed: {}",
        reopen.stderr
    );

    let payload = extract_json_payload(&reopen.stdout);
    let parsed: Value = serde_json::from_str(&payload).expect("reopen env json parse");
    let reopened = parsed["reopened"].as_array().expect("reopened array");
    assert_eq!(reopened.len(), 1);
    assert_eq!(reopened[0]["id"], issue_id);
    assert_eq!(reopened[0]["status"], "open");
}

#[test]
fn e2e_reopen_tombstone_skips_without_resurrection() {
    let _log = common::test_log("e2e_reopen_tombstone_skips_without_resurrection");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "reopen_tombstone_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Issue to tombstone", "--json"],
        "reopen_tombstone_create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let issue_id = created["id"].as_str().expect("issue id");

    let delete = run_br(
        &workspace,
        [
            "delete",
            issue_id,
            "--force",
            "--reason",
            "Testing reopen tombstone",
        ],
        "reopen_tombstone_delete",
    );
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);

    let reopen = run_br(
        &workspace,
        ["reopen", issue_id, "--json"],
        "reopen_tombstone_reopen",
    );
    assert!(
        reopen.status.success(),
        "reopen tombstone failed: {}",
        reopen.stderr
    );

    let payload = extract_json_payload(&reopen.stdout);
    let parsed: Value = serde_json::from_str(&payload).expect("reopen tombstone json");
    let reopened = parsed["reopened"].as_array().cloned().unwrap_or_default();
    let skipped = parsed["skipped"].as_array().cloned().unwrap_or_default();

    assert!(
        reopened.is_empty(),
        "tombstone should not be reported as reopened"
    );
    assert_eq!(skipped.len(), 1, "tombstone should be reported as skipped");
    assert_eq!(skipped[0]["id"], issue_id);
    assert!(
        skipped[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("tombstone")),
        "skip reason should explain that tombstones cannot be reopened"
    );

    let show = run_br(
        &workspace,
        ["show", issue_id, "--json"],
        "reopen_tombstone_show",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let show_payload = extract_json_payload(&show.stdout);
    let show_json: Value = serde_json::from_str(&show_payload).expect("show json");

    if show_json.is_array() {
        assert_eq!(show_json[0]["status"], "tombstone");
    } else {
        assert_eq!(show_json["status"], "tombstone");
    }
}

/// E2E tests for saved queries: query save/run/list/delete.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_saved_queries_lifecycle() {
    let _log = common::test_log("e2e_saved_queries_lifecycle");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "saved_query_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create test issues with different types and priorities
    let bug = run_br(
        &workspace,
        ["create", "Critical bug", "-t", "bug", "-p", "0"],
        "saved_query_create_bug",
    );
    assert!(bug.status.success(), "bug create failed: {}", bug.stderr);

    let task = run_br(
        &workspace,
        ["create", "Normal task", "-t", "task", "-p", "2"],
        "saved_query_create_task",
    );
    assert!(task.status.success(), "task create failed: {}", task.stderr);

    let feature = run_br(
        &workspace,
        ["create", "New feature", "-t", "feature", "-p", "1"],
        "saved_query_create_feature",
    );
    assert!(
        feature.status.success(),
        "feature create failed: {}",
        feature.stderr
    );

    // Test query save - save a query for bugs only
    let save_bugs = run_br(
        &workspace,
        [
            "query",
            "save",
            "my-bugs",
            "--type",
            "bug",
            "--description",
            "All bug issues",
        ],
        "saved_query_save_bugs",
    );
    assert!(
        save_bugs.status.success(),
        "query save failed: {}",
        save_bugs.stderr
    );
    assert!(
        save_bugs.stdout.contains("Saved query 'my-bugs'"),
        "save output missing confirmation"
    );

    // Test query save with JSON output
    let save_p0 = run_br(
        &workspace,
        ["query", "save", "critical", "--priority", "0", "--json"],
        "saved_query_save_p0",
    );
    assert!(
        save_p0.status.success(),
        "query save critical failed: {}",
        save_p0.stderr
    );
    let save_p0_payload = extract_json_payload(&save_p0.stdout);
    let save_p0_json: Value = serde_json::from_str(&save_p0_payload).expect("save json");
    assert_eq!(save_p0_json["status"], "ok");
    assert_eq!(save_p0_json["name"], "critical");
    assert_eq!(save_p0_json["action"], "saved");

    // Test query list - text output
    let list_text = run_br(&workspace, ["query", "list"], "saved_query_list_text");
    assert!(
        list_text.status.success(),
        "query list failed: {}",
        list_text.stderr
    );
    assert!(list_text.stdout.contains("my-bugs"), "list missing my-bugs");
    assert!(
        list_text.stdout.contains("critical"),
        "list missing critical"
    );
    assert!(
        list_text.stdout.contains("All bug issues"),
        "list missing description"
    );

    // Test query list - JSON output
    let list_json = run_br(
        &workspace,
        ["query", "list", "--json"],
        "saved_query_list_json",
    );
    assert!(
        list_json.status.success(),
        "query list json failed: {}",
        list_json.stderr
    );
    let list_payload = extract_json_payload(&list_json.stdout);
    assert!(
        list_payload.starts_with("{\"queries\":["),
        "query list JSON should preserve queries-first object shape: {list_payload}"
    );
    assert!(
        list_payload.ends_with(",\"count\":2}"),
        "query list JSON should preserve count trailer: {list_payload}"
    );
    let list_parsed: Value = serde_json::from_str(&list_payload).expect("list json");
    assert_eq!(list_parsed["count"], 2);
    let queries = list_parsed["queries"].as_array().expect("queries array");
    assert!(queries.iter().any(|q| q["name"] == "my-bugs"));
    assert!(queries.iter().any(|q| q["name"] == "critical"));

    // Test query run - run the bugs query
    let run_bugs = run_br(
        &workspace,
        ["query", "run", "my-bugs", "--json"],
        "saved_query_run_bugs",
    );
    assert!(
        run_bugs.status.success(),
        "query run bugs failed: {}",
        run_bugs.stderr
    );
    let run_bugs_json = extract_issues_array(&run_bugs.stdout);
    // Should only return bug type issues
    assert_eq!(run_bugs_json.len(), 1);
    assert_eq!(run_bugs_json[0]["issue_type"], "bug");
    assert!(
        run_bugs_json[0]["title"]
            .as_str()
            .unwrap()
            .contains("Critical bug")
    );

    // Test query run - run critical priority query
    let run_critical = run_br(
        &workspace,
        ["query", "run", "critical", "--json"],
        "saved_query_run_critical",
    );
    assert!(
        run_critical.status.success(),
        "query run critical failed: {}",
        run_critical.stderr
    );
    let run_critical_json = extract_issues_array(&run_critical.stdout);
    // Should only return P0 issues
    assert_eq!(run_critical_json.len(), 1);
    assert_eq!(run_critical_json[0]["priority"], 0);

    // Test CLI override - run bugs query but filter further by priority
    // (The bug has P0, so filtering by P1 should return empty)
    let run_override = run_br(
        &workspace,
        ["query", "run", "my-bugs", "--priority", "1", "--json"],
        "saved_query_run_override",
    );
    assert!(
        run_override.status.success(),
        "query run override failed: {}",
        run_override.stderr
    );
    let run_override_json = extract_issues_array(&run_override.stdout);
    // CLI priority filter (P1) overrides, so no P0 bugs returned
    assert!(
        run_override_json.is_empty(),
        "expected empty result when CLI priority overrides saved"
    );

    // Test query delete - text output
    let delete_text = run_br(
        &workspace,
        ["query", "delete", "my-bugs"],
        "saved_query_delete_text",
    );
    assert!(
        delete_text.status.success(),
        "query delete failed: {}",
        delete_text.stderr
    );
    assert!(
        delete_text.stdout.contains("Deleted query 'my-bugs'"),
        "delete output missing confirmation"
    );

    // Test query delete - JSON output
    let delete_json = run_br(
        &workspace,
        ["query", "delete", "critical", "--json"],
        "saved_query_delete_json",
    );
    assert!(
        delete_json.status.success(),
        "query delete json failed: {}",
        delete_json.stderr
    );
    let delete_payload = extract_json_payload(&delete_json.stdout);
    let delete_parsed: Value = serde_json::from_str(&delete_payload).expect("delete json");
    assert_eq!(delete_parsed["status"], "ok");
    assert_eq!(delete_parsed["name"], "critical");
    assert_eq!(delete_parsed["action"], "deleted");

    // Verify queries are deleted
    let list_empty = run_br(&workspace, ["query", "list"], "saved_query_list_empty");
    assert!(
        list_empty.status.success(),
        "query list empty failed: {}",
        list_empty.stderr
    );
    assert!(
        list_empty.stdout.contains("No saved queries"),
        "expected no saved queries after deletion"
    );
}

/// Regression: `query run` must not replace saved pagination when the operator
/// did not pass run-time pagination flags.
#[test]
fn e2e_query_run_preserves_saved_limit_without_override() {
    let _log = common::test_log("e2e_query_run_preserves_saved_limit_without_override");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "query_limit_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    for title in ["Limit issue A", "Limit issue B", "Limit issue C"] {
        let created = run_br(&workspace, ["create", title], title);
        assert!(
            created.status.success(),
            "create failed: {}",
            created.stderr
        );
    }

    let save = run_br(
        &workspace,
        [
            "query", "save", "one-open", "--status", "open", "--limit", "1", "--offset", "1",
            "--sort", "title",
        ],
        "query_limit_save",
    );
    assert!(save.status.success(), "query save failed: {}", save.stderr);

    let saved_run = run_br(
        &workspace,
        ["query", "run", "one-open", "--json"],
        "query_limit_run_saved",
    );
    assert!(
        saved_run.status.success(),
        "query run failed: {}",
        saved_run.stderr
    );
    let saved_payload = extract_json_payload(&saved_run.stdout);
    let saved_json: Value = serde_json::from_str(&saved_payload).expect("saved run json");
    assert_eq!(saved_json["limit"], 1);
    assert_eq!(saved_json["offset"], 1);
    assert_eq!(
        saved_json["issues"].as_array().expect("issues array").len(),
        1
    );
    assert_eq!(saved_json["issues"][0]["title"], "Limit issue B");

    let override_run = run_br(
        &workspace,
        ["query", "run", "one-open", "--limit", "2", "--json"],
        "query_limit_run_override",
    );
    assert!(
        override_run.status.success(),
        "query run override failed: {}",
        override_run.stderr
    );
    let override_payload = extract_json_payload(&override_run.stdout);
    let override_json: Value = serde_json::from_str(&override_payload).expect("override run json");
    assert_eq!(override_json["limit"], 2);
    assert_eq!(override_json["offset"], 1);
    assert_eq!(
        override_json["issues"]
            .as_array()
            .expect("issues array")
            .len(),
        2
    );
    assert_eq!(override_json["issues"][0]["title"], "Limit issue B");
    assert_eq!(override_json["issues"][1]["title"], "Limit issue C");
}

/// E2E tests for saved query error cases.
#[test]
fn e2e_saved_queries_errors() {
    let _log = common::test_log("e2e_saved_queries_errors");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "saved_query_error_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create a query first
    let save = run_br(
        &workspace,
        ["query", "save", "test-query", "--status", "open"],
        "saved_query_error_save",
    );
    assert!(save.status.success(), "query save failed: {}", save.stderr);

    // Test duplicate name error
    let save_dup = run_br(
        &workspace,
        ["query", "save", "test-query", "--status", "closed"],
        "saved_query_error_dup",
    );
    assert!(!save_dup.status.success(), "duplicate save should fail");
    assert!(
        save_dup.stderr.contains("already exists"),
        "error should mention query already exists"
    );

    // Test run nonexistent query
    let run_missing = run_br(
        &workspace,
        ["query", "run", "nonexistent"],
        "saved_query_error_run_missing",
    );
    assert!(!run_missing.status.success(), "run nonexistent should fail");
    assert!(
        run_missing.stderr.contains("not found"),
        "error should mention query not found"
    );

    // Test delete nonexistent query
    let delete_missing = run_br(
        &workspace,
        ["query", "delete", "nonexistent"],
        "saved_query_error_delete_missing",
    );
    assert!(
        !delete_missing.status.success(),
        "delete nonexistent should fail"
    );
    assert!(
        delete_missing.stderr.contains("not found"),
        "error should mention query not found"
    );

    // Test invalid query name (contains ':')
    let save_invalid = run_br(
        &workspace,
        ["query", "save", "bad:name", "--status", "open"],
        "saved_query_error_invalid_name",
    );
    assert!(!save_invalid.status.success(), "invalid name should fail");
    assert!(
        save_invalid.stderr.contains("cannot contain"),
        "error should mention invalid characters"
    );
}

#[test]
fn e2e_saved_queries_reject_no_db_mode() {
    let _log = common::test_log("e2e_saved_queries_reject_no_db_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "saved_query_no_db_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let save = run_br(
        &workspace,
        ["query", "save", "existing-query", "--status", "open"],
        "saved_query_no_db_seed",
    );
    assert!(
        save.status.success(),
        "seed query save failed: {}",
        save.stderr
    );

    for (name, args) in [
        (
            "saved_query_no_db_save",
            vec!["--no-db", "query", "save", "temp-query", "--status", "open"],
        ),
        ("saved_query_no_db_list", vec!["--no-db", "query", "list"]),
        (
            "saved_query_no_db_run",
            vec!["--no-db", "query", "run", "existing-query"],
        ),
        (
            "saved_query_no_db_delete",
            vec!["--no-db", "query", "delete", "existing-query"],
        ),
    ] {
        let result = run_br(&workspace, args, name);
        assert!(!result.status.success(), "{name} should fail");
        assert!(
            result
                .stderr
                .contains("--no-db is not supported for query commands"),
            "{name} should explain that query commands require the database: {}",
            result.stderr
        );
    }
}

/// E2E test: CLI args override saved query filters at run time.
#[test]
fn e2e_saved_queries_run_with_overrides() {
    let _log = common::test_log("e2e_saved_queries_run_with_overrides");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "override_init");
    assert!(init.status.success());

    // Create issues with different types
    let id_bug = {
        let c = run_br(&workspace, ["create", "Login crash"], "override_bug");
        assert!(c.status.success());
        parse_created_id(&c.stdout)
    };
    let _ = run_br(
        &workspace,
        ["update", &id_bug, "--type", "bug"],
        "override_set_bug",
    );

    let id_feat = {
        let c = run_br(&workspace, ["create", "Add theme"], "override_feat");
        assert!(c.status.success());
        parse_created_id(&c.stdout)
    };
    let _ = run_br(
        &workspace,
        ["update", &id_feat, "--type", "feature"],
        "override_set_feat",
    );

    // Save a query that filters by type=bug
    let save = run_br(
        &workspace,
        ["query", "save", "bugs-only", "--type", "bug"],
        "override_save",
    );
    assert!(save.status.success(), "save failed: {}", save.stderr);

    // Run saved query - should return only bugs
    let run_default = run_br(
        &workspace,
        ["query", "run", "bugs-only", "--json"],
        "override_run_default",
    );
    assert!(run_default.status.success());

    let issues = extract_issues_array(&run_default.stdout);
    assert!(
        issues.iter().all(|i| i["issue_type"] == "bug"),
        "saved query should only return bugs"
    );

    // Run with CLI override: type=feature should override saved type=bug
    let run_override = run_br(
        &workspace,
        ["query", "run", "bugs-only", "--type", "feature", "--json"],
        "override_run_override",
    );
    assert!(run_override.status.success());

    let overridden = extract_issues_array(&run_override.stdout);
    assert!(
        overridden.iter().all(|i| i["issue_type"] == "feature"),
        "CLI --type should override saved query filter, got: {:?}",
        overridden
            .iter()
            .map(|i| i["issue_type"].as_str().unwrap_or("?"))
            .collect::<Vec<_>>()
    );
}

/// E2E test: explicit assignee override clears a saved unassigned filter.
#[test]
fn e2e_saved_queries_assignee_override_clears_unassigned() {
    let _log = common::test_log("e2e_saved_queries_assignee_override_clears_unassigned");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "override_assignee_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let assigned_id = {
        let create = run_br(
            &workspace,
            ["create", "Assigned issue"],
            "override_assignee_create_assigned",
        );
        assert!(create.status.success(), "create failed: {}", create.stderr);
        parse_created_id(&create.stdout)
    };
    let assign = run_br(
        &workspace,
        ["update", &assigned_id, "--assignee", "alice"],
        "override_assignee_set_assigned",
    );
    assert!(assign.status.success(), "assign failed: {}", assign.stderr);

    let unassigned_id = {
        let create = run_br(
            &workspace,
            ["create", "Unassigned issue"],
            "override_assignee_create_unassigned",
        );
        assert!(create.status.success(), "create failed: {}", create.stderr);
        parse_created_id(&create.stdout)
    };

    let save = run_br(
        &workspace,
        ["query", "save", "free-only", "--unassigned"],
        "override_assignee_save",
    );
    assert!(save.status.success(), "save failed: {}", save.stderr);

    let run_default = run_br(
        &workspace,
        ["query", "run", "free-only", "--json"],
        "override_assignee_run_default",
    );
    assert!(
        run_default.status.success(),
        "default run failed: {}",
        run_default.stderr
    );
    let issues = extract_issues_array(&run_default.stdout);
    assert_eq!(
        issues.len(),
        1,
        "saved query should return only unassigned work"
    );
    assert_eq!(issues[0]["id"], unassigned_id);

    let run_override = run_br(
        &workspace,
        ["query", "run", "free-only", "--assignee", "alice", "--json"],
        "override_assignee_run_override",
    );
    assert!(
        run_override.status.success(),
        "override run failed: {}",
        run_override.stderr
    );
    let issues = extract_issues_array(&run_override.stdout);
    assert_eq!(
        issues.len(),
        1,
        "assignee override should narrow results cleanly"
    );
    assert_eq!(issues[0]["id"], assigned_id);
}
