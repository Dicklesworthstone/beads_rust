//! E2E coverage for CLI read-only fast-open behavior.
//!
//! These tests compare the optimized current-schema read-only path against the
//! conservative locked path, then prove representative read commands still run
//! while another process holds `.beads/.write.lock`.

mod common;

use beads_rust::franken_sync::Connection;
use common::cli::{BrRun, BrWorkspace, parse_created_id, run_br, run_br_with_env};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::time::{Duration, Instant};

const DISABLE_FAST_OPEN_ENV: (&str, &str) = ("BR_DISABLE_READ_ONLY_FAST_OPEN", "1");

struct SeededWorkspace {
    workspace: BrWorkspace,
    blocker_id: String,
    blocked_id: String,
}

#[derive(Clone, Copy)]
enum CompareMode {
    Exact,
    JsonWithoutKeys(&'static [&'static str]),
}

struct MatrixCommand {
    label: &'static str,
    args: Vec<String>,
    compare_mode: CompareMode,
}

fn assert_success(run: &BrRun, label: &str) {
    assert!(
        run.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
}

fn run_success(workspace: &BrWorkspace, args: &[&str], label: &str) -> BrRun {
    let run = run_br(workspace, args.iter().copied(), label);
    assert_success(&run, label);
    run
}

fn create_issue(workspace: &BrWorkspace, args: &[&str], label: &str) -> String {
    parse_created_id(&run_success(workspace, args, label).stdout)
}

fn seed_workspace() -> SeededWorkspace {
    let workspace = BrWorkspace::new();

    run_success(&workspace, &["init"], "init");
    let epic_id = create_issue(
        &workspace,
        &[
            "create",
            "Fast-open roadmap epic",
            "-p",
            "0",
            "--type",
            "epic",
            "-l",
            "roadmap,fast-open",
        ],
        "create_epic",
    );
    let blocker_id = create_issue(
        &workspace,
        &[
            "create",
            "Fast-open blocker issue",
            "-p",
            "1",
            "--type",
            "bug",
            "-l",
            "backend,fast-open",
        ],
        "create_blocker",
    );
    let blocked_id = create_issue(
        &workspace,
        &[
            "create",
            "Fast-open blocked issue",
            "-p",
            "2",
            "--type",
            "task",
            "-l",
            "backend",
            "--parent",
            &epic_id,
        ],
        "create_blocked",
    );
    create_issue(
        &workspace,
        &[
            "create",
            "Fast-open ready issue",
            "-p",
            "0",
            "--type",
            "feature",
            "-l",
            "ready,fast-open",
            "--parent",
            &epic_id,
        ],
        "create_ready",
    );
    run_success(
        &workspace,
        &[
            "comments",
            "add",
            &blocker_id,
            "--author",
            "fast-open-test",
            "Snapshot matrix comment",
        ],
        "add_comment",
    );
    run_success(
        &workspace,
        &["dep", "add", &blocked_id, &blocker_id],
        "dep_add",
    );
    run_success(
        &workspace,
        &["query", "save", "fast-open-p1", "--priority", "1"],
        "query_save",
    );
    run_success(
        &workspace,
        &["sync", "--flush-only", "--json"],
        "sync_flush",
    );

    SeededWorkspace {
        workspace,
        blocker_id,
        blocked_id,
    }
}

fn matrix_commands(seed: &SeededWorkspace) -> Vec<MatrixCommand> {
    let mut commands = Vec::new();
    commands.extend(core_read_commands(seed));
    commands.extend(status_and_report_commands());
    commands.extend(relation_and_query_commands(seed));
    commands
}

fn core_read_commands(seed: &SeededWorkspace) -> Vec<MatrixCommand> {
    vec![
        exact_command("list_json", strings(["list", "--json", "--limit", "5"])),
        exact_command(
            "show_json",
            vec![
                "show".into(),
                seed.blocker_id.clone(),
                "--format".into(),
                "json".into(),
            ],
        ),
        exact_command(
            "search_json",
            strings(["search", "Fast-open", "--format", "json", "--limit", "5"]),
        ),
        exact_command("ready_json", strings(["ready", "--json", "--limit", "5"])),
        normalized_json_command(
            "scheduler_json",
            strings([
                "scheduler",
                "--json",
                "--limit",
                "5",
                "--candidate-limit",
                "10",
            ]),
            &["generated_at"],
        ),
        exact_command(
            "blocked_json",
            strings(["blocked", "--json", "--limit", "5"]),
        ),
    ]
}

fn status_and_report_commands() -> Vec<MatrixCommand> {
    vec![
        exact_command("count_json", strings(["count", "--json"])),
        exact_command(
            "count_by_label_json",
            strings(["count", "--by", "label", "--json"]),
        ),
        exact_command("stale_json", strings(["stale", "--days", "0", "--json"])),
        exact_command("lint_json", strings(["lint", "--json"])),
        exact_command("sync_status_json", strings(["sync", "--status", "--json"])),
        exact_command(
            "stats_no_activity_json",
            strings(["stats", "--no-activity", "--json"]),
        ),
        exact_command(
            "status_no_activity_json",
            strings(["status", "--no-activity", "--json"]),
        ),
        normalized_json_command(
            "changelog_robot",
            strings(["changelog", "--since", "2100-01-01", "--robot"]),
            &["until"],
        ),
        exact_command(
            "graph_all_compact",
            strings(["graph", "--all", "--compact"]),
        ),
        exact_command(
            "orphans_robot_explicit_stale_opt_out",
            strings(["--no-auto-import", "--no-auto-flush", "orphans", "--robot"]),
        ),
    ]
}

fn relation_and_query_commands(seed: &SeededWorkspace) -> Vec<MatrixCommand> {
    vec![
        exact_command(
            "comments_json",
            vec![
                "comments".into(),
                "list".into(),
                seed.blocker_id.clone(),
                "--json".into(),
            ],
        ),
        exact_command(
            "comments_shorthand_json",
            vec!["comments".into(), seed.blocker_id.clone(), "--json".into()],
        ),
        exact_command("epic_status_json", strings(["epic", "status", "--json"])),
        exact_command("label_list_unique", strings(["label", "list"])),
        exact_command(
            "label_list_all_json",
            strings(["label", "list-all", "--json"]),
        ),
        exact_command(
            "dep_list_json",
            vec![
                "dep".into(),
                "list".into(),
                seed.blocked_id.clone(),
                "--format".into(),
                "json".into(),
            ],
        ),
        exact_command(
            "dep_tree_json",
            vec![
                "dep".into(),
                "tree".into(),
                seed.blocked_id.clone(),
                "--json".into(),
            ],
        ),
        exact_command("dep_cycles_json", strings(["dep", "cycles", "--json"])),
        exact_command(
            "query_run_json",
            strings(["query", "run", "fast-open-p1", "--format", "json"]),
        ),
        exact_command("query_list_json", strings(["query", "list", "--json"])),
    ]
}

fn exact_command(label: &'static str, args: Vec<String>) -> MatrixCommand {
    MatrixCommand {
        label,
        args,
        compare_mode: CompareMode::Exact,
    }
}

fn normalized_json_command(
    label: &'static str,
    args: Vec<String>,
    ignored_keys: &'static [&'static str],
) -> MatrixCommand {
    MatrixCommand {
        label,
        args,
        compare_mode: CompareMode::JsonWithoutKeys(ignored_keys),
    }
}

fn strings<const N: usize>(values: [&str; N]) -> Vec<String> {
    values.into_iter().map(str::to_string).collect()
}

fn run_command(workspace: &BrWorkspace, command: &MatrixCommand, disable_fast_open: bool) -> BrRun {
    if disable_fast_open {
        return run_br_with_env(
            workspace,
            command.args.iter().map(String::as_str),
            [DISABLE_FAST_OPEN_ENV],
            &format!("{}_conservative", command.label),
        );
    }

    // Exercise the default synchronized-probe path. Individual matrix entries
    // carry explicit opt-outs only when that command intentionally owns a
    // separate auto-import contract (currently bare `orphans`).
    run_br(
        workspace,
        command.args.iter().map(String::as_str),
        &format!("{}_fast", command.label),
    )
}

fn assert_outputs_match(command: &MatrixCommand, fast: &BrRun, conservative: &BrRun) {
    match command.compare_mode {
        CompareMode::Exact => assert_eq!(
            fast.stdout, conservative.stdout,
            "{} stdout changed between read-only fast-open and conservative locked path",
            command.label
        ),
        CompareMode::JsonWithoutKeys(keys) => {
            let mut fast_json: Value = serde_json::from_str(&fast.stdout).unwrap_or_else(|err| {
                panic!("{} fast-open stdout is not JSON: {err}", command.label)
            });
            let mut conservative_json: Value = serde_json::from_str(&conservative.stdout)
                .unwrap_or_else(|err| {
                    panic!("{} conservative stdout is not JSON: {err}", command.label)
                });

            remove_json_keys(&mut fast_json, keys);
            remove_json_keys(&mut conservative_json, keys);

            assert_eq!(
                fast_json, conservative_json,
                "{} normalized JSON changed between read-only fast-open and conservative locked path",
                command.label
            );
        }
    }
}

fn remove_json_keys(value: &mut Value, ignored_keys: &[&str]) {
    match value {
        Value::Object(object) => {
            for key in ignored_keys {
                object.remove(*key);
            }
            for nested in object.values_mut() {
                remove_json_keys(nested, ignored_keys);
            }
        }
        Value::Array(items) => {
            for item in items {
                remove_json_keys(item, ignored_keys);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[test]
fn cli_read_only_fast_open_matrix_matches_conservative_outputs() {
    let _log = common::test_log("cli_read_only_fast_open_matrix_matches_conservative_outputs");
    let seed = seed_workspace();

    for command in matrix_commands(&seed) {
        let conservative = run_command(&seed.workspace, &command, true);
        assert_success(&conservative, command.label);

        let fast = run_command(&seed.workspace, &command, false);
        assert_success(&fast, command.label);

        assert_outputs_match(&command, &fast, &conservative);
    }
}

#[test]
fn cli_read_only_fast_open_matrix_bypasses_held_write_lock() {
    let _log = common::test_log("cli_read_only_fast_open_matrix_bypasses_held_write_lock");
    let seed = seed_workspace();
    let lock_path = seed.workspace.root.join(".beads/.write.lock");
    let write_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open write lock");
    write_lock.lock().expect("hold write lock");

    for command in matrix_commands(&seed) {
        let fast = run_command(&seed.workspace, &command, false);
        assert_success(&fast, command.label);
    }

    let blocked_conservative = run_command(
        &seed.workspace,
        &exact_command(
            "list_json_locked_conservative",
            strings(["--lock-timeout", "50", "list", "--json", "--limit", "1"]),
        ),
        true,
    );
    assert!(
        !blocked_conservative.status.success(),
        "disabled fast-open should wait for the held write lock and time out"
    );
    let combined = format!(
        "{} {}",
        blocked_conservative.stdout, blocked_conservative.stderr
    )
    .to_ascii_lowercase();
    assert!(
        combined.contains("lock") || combined.contains("timed out"),
        "conservative failure should mention lock contention, got: {combined}"
    );
}

#[test]
fn cli_read_only_fast_open_fails_when_the_authoritative_jsonl_probe_fails() {
    let _log =
        common::test_log("cli_read_only_fast_open_fails_when_the_authoritative_jsonl_probe_fails");
    let seed = seed_workspace();
    let jsonl_path = seed.workspace.root.join(".beads/issues.jsonl");
    let preserved_jsonl_path = seed.workspace.root.join(".beads/issues.preserved.jsonl");
    fs::rename(&jsonl_path, &preserved_jsonl_path).expect("preserve regular JSONL fixture");
    fs::create_dir(&jsonl_path).expect("plant a non-regular JSONL path");

    let run = run_br(&seed.workspace, ["list", "--json"], "fast_probe_error");
    assert!(
        !run.status.success(),
        "an authoritative JSONL probe error must fail instead of serving stale DB state\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    let combined = format!("{} {}", run.stdout, run.stderr).to_ascii_lowercase();
    assert!(
        combined.contains("jsonl") || combined.contains("sync path"),
        "probe failure should identify the rejected JSONL path: {combined}"
    );
}

#[test]
fn cli_fast_open_healing_reuses_its_authority_for_a_newer_jsonl_import() {
    let _log =
        common::test_log("cli_fast_open_healing_reuses_its_authority_for_a_newer_jsonl_import");
    let workspace = BrWorkspace::new();
    run_success(&workspace, &["init"], "init");
    let issue_id = create_issue(
        &workspace,
        &["create", "Database-side title"],
        "create_issue",
    );

    let db_path = workspace.root.join(".beads/beads.db");
    let connection =
        Connection::open(db_path.to_string_lossy().into_owned()).expect("open database fixture");
    connection
        .execute("DROP TABLE capacity_occupancy")
        .expect("make the current-version runtime schema incomplete");
    connection.close().expect("close database fixture");

    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read current JSONL");
    let rewritten = contents
        .lines()
        .map(|line| {
            let mut issue: Value = serde_json::from_str(line).expect("parse JSONL issue");
            if issue["id"].as_str() == Some(issue_id.as_str()) {
                issue["title"] = Value::String("JSONL-side title".to_string());
                issue["updated_at"] = Value::String("2099-01-01T00:00:00Z".to_string());
                issue["content_hash"] = Value::Null;
            }
            serde_json::to_string(&issue).expect("serialize JSONL issue")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&jsonl_path, format!("{rewritten}\n")).expect("write newer JSONL");

    let run = run_br(
        &workspace,
        ["--lock-timeout", "50", "list", "--json"],
        "heal_then_import",
    );
    assert!(
        run.status.success(),
        "fast-open healing must reuse its retained authority for auto-import\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("JSONL-side title"),
        "the newer JSONL generation must be imported: {}",
        run.stdout
    );
}

fn run_matrix_round(workspace: &BrWorkspace, commands: &[MatrixCommand], disable_fast_open: bool) {
    for command in commands {
        let run = run_command(workspace, command, disable_fast_open);
        assert_success(&run, command.label);
    }
}

fn duration_ns_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[test]
#[ignore = "perf probe for CLI read-only fast-open matrix evidence"]
fn cli_read_only_fast_open_matrix_perf_probe() {
    let seed = seed_workspace();
    let commands = matrix_commands(&seed);
    let rounds = 5_u32;

    let conservative_start = Instant::now();
    for _ in 0..rounds {
        run_matrix_round(&seed.workspace, &commands, true);
    }
    let conservative = conservative_start.elapsed();

    let fast_start = Instant::now();
    for _ in 0..rounds {
        run_matrix_round(&seed.workspace, &commands, false);
    }
    let fast = fast_start.elapsed();

    let conservative_ns = duration_ns_u64(conservative);
    let fast_ns = duration_ns_u64(fast);
    println!(
        "{}",
        json!({
            "commands": commands.iter().map(|command| command.label).collect::<Vec<_>>(),
            "rounds": rounds,
            "conservative_total_ns": conservative_ns,
            "fast_open_total_ns": fast_ns,
            "speedup_milli": conservative_ns.saturating_mul(1000) / fast_ns.max(1),
            "equality": "routine matrix test asserts byte-identical stdout per command",
        })
    );
}
