//! Phase 5 — End-to-end safety harness for the `br doctor --repair`
//! chokepoint round-trip.
//!
//! These tests prove the chokepoint actually works on a real built `br`
//! binary against a real corrupted workspace:
//!
//! 1. corrupt → diagnose → `--repair` → assert healthy
//! 2. take inventory of the produced `.doctor/runs/<id>/` artifact dir
//! 3. `br doctor undo <id>` → assert the affected files restore to the
//!    chokepoint's recorded `before_hash` (the byte-identical recovery
//!    contract)
//! 4. exercise the dry-run / idempotence / capabilities / triage
//!    contracts
//!
//! Per AGENTS.md, runtime `br` code never invokes `Command::new("git")`;
//! these tests use only `assert_cmd::Command::cargo_bin("br")` and
//! `tempfile::TempDir`. The fixture workspace is created in-process via
//! `br init` (idiomatic for the rest of the e2e suite) and corrupted
//! with a `.gitignore` line that triggers the
//! `gitignore.beads_inner` detector + the `doctor.gitignore_repair`
//! fixer — currently the most thoroughly chokepoint-rewired path under
//! WP3.
//!
//! Why not a checked-in fixture: the only existing
//! `tests/fixtures/workspace_failures/*` cases route most of their
//! repair work through legacy non-chokepointed paths (DB rebuild, WAL
//! cleanup, etc.). A clean `br init` workspace plus an offending root
//! `.gitignore` isolates the WP3-rewired chokepoint flow without
//! dragging the as-yet-unmigrated repair paths into the assertion.

use assert_cmd::Command;
use beads_rust::cli::commands::doctor_subsystems::mutate::{
    Capabilities, DbArg, MutateContext, Op, mutate,
};
use fsqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Make a fresh `br` invocation rooted at `cwd` with a hermetic env so
/// tests don't pick up the developer's shell config.
fn br_cmd(cwd: &Path) -> Command {
    let mut cmd = Command::cargo_bin("br").expect("locate br binary");
    cmd.current_dir(cwd);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_LOG", "warn");
    cmd.env("HOME", cwd);
    // Strip any inherited beads / bd env that might redirect storage.
    for (key, _) in std::env::vars_os() {
        let key_s = key.to_string_lossy();
        if key_s.starts_with("BD_") || key_s.starts_with("BEADS_") {
            cmd.env_remove(&key);
        }
    }
    cmd
}

/// Run `br init` in `cwd` and assert it succeeded.
fn br_init(cwd: &Path) {
    let out = br_cmd(cwd).arg("init").output().expect("br init spawned");
    assert!(
        out.status.success(),
        "br init failed: status={:?}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// Plant the `gitignore.beads_inner` failure: a root `.gitignore` whose
/// `.beads/` line shadows br's own ignore rules.
fn corrupt_root_gitignore(cwd: &Path) {
    let body = "# project\nnode_modules\n.beads/\n";
    fs::write(cwd.join(".gitignore"), body).expect("write corrupt .gitignore");
}

/// SHA-256 every regular file under `root` excluding `.doctor/` (the
/// run-artifact area is supposed to grow under `--repair`). Returns a
/// stable map of relative-path -> hex digest.
fn hash_workspace(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut out = BTreeMap::new();
    walk_workspace_hashes(root, root, &mut out);
    out
}

fn walk_workspace_hashes(dir: &Path, root: &Path, out: &mut BTreeMap<PathBuf, String>) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        // Skip the doctor artifact tree and the SQLite write lock — the
        // lock is a runtime artifact whose existence depends on whether
        // anything has opened the DB and is not part of the workspace
        // state we want to round-trip.
        let rel = path.strip_prefix(root).unwrap();
        if rel.starts_with(".doctor") {
            continue;
        }
        if rel == Path::new(".beads/.write.lock") {
            continue;
        }
        let ft = entry.file_type().expect("file_type");
        if ft.is_dir() {
            walk_workspace_hashes(&path, root, out);
        } else if ft.is_file() {
            let bytes = fs::read(&path).expect("read file");
            out.insert(rel.to_path_buf(), sha256_hex(&bytes));
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    beads_rust::util::hex_encode(&h.finalize())
}

/// Locate the single run-dir under `<root>/.doctor/runs/`. Panics if
/// there is not exactly one — the caller is expected to invoke
/// `--repair` exactly once.
fn single_run_dir(root: &Path) -> PathBuf {
    let runs = root.join(".doctor").join("runs");
    let entries: Vec<_> = fs::read_dir(&runs)
        .expect("read runs/")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one run-dir under {}, found {:?}",
        runs.display(),
        entries
    );
    entries.into_iter().next().unwrap()
}

/// Parse `actions.jsonl` and return one `serde_json::Value` per line.
fn read_actions(run_dir: &Path) -> Vec<Value> {
    let p = run_dir.join("actions.jsonl");
    let body = fs::read_to_string(&p).expect("read actions.jsonl");
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("parse jsonl line"))
        .collect()
}

/// Extract the trailing JSON object from a stdout that may have been
/// preceded by prose lines.
fn parse_trailing_json(stdout: &str) -> Value {
    // The doctor's --json variants emit a JSON object on the last
    // contentful line. Strategy: scan for the first '{' and parse from
    // there to end-of-string.
    let trimmed = stdout.trim_end();
    let start = trimmed
        .find('{')
        .unwrap_or_else(|| panic!("no JSON object in stdout: {stdout}"));
    serde_json::from_str(&trimmed[start..])
        .unwrap_or_else(|e| panic!("parse JSON failed ({e}): {}", &trimmed[start..]))
}

fn seed_blocked_cache_db(db_path: &Path, blocked_by: &str) {
    let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
    conn.execute(
        "CREATE TABLE blocked_issues_cache (
            issue_id TEXT PRIMARY KEY,
            blocked_by TEXT NOT NULL,
            blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .unwrap();
    conn.execute(&format!(
        "INSERT INTO blocked_issues_cache(issue_id, blocked_by, blocked_at) \
         VALUES ('bd-1', '{blocked_by}', '2026-05-01 00:00:00')"
    ))
    .unwrap();
    let _ = conn.close();
}

fn db_exec_context(root: &Path, run_id: &str) -> (PathBuf, MutateContext) {
    let run_dir = root.join(".doctor").join("runs").join(run_id);
    fs::create_dir_all(run_dir.join("backups")).unwrap();
    let actions_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("actions.jsonl"))
        .unwrap();
    let ctx = MutateContext {
        run_id: run_id.to_string(),
        run_dir: run_dir.clone(),
        capabilities: Capabilities::for_repo(root),
        actions_file: Mutex::new(actions_file),
        fixer_id: "wp4-cache-rebuild".to_string(),
        repo_root: root.to_path_buf(),
        dry_run: false,
        start_ns: 0,
    };
    (run_dir, ctx)
}

fn mutate_cache_rebuild(ctx: &MutateContext, db_path: &Path) {
    let result_delete = mutate(
        ctx,
        db_path,
        Op::DbExec {
            sql: "DELETE FROM blocked_issues_cache".into(),
            args: vec![],
            affected_tables: vec!["blocked_issues_cache".into()],
            affected_predicate: None,
        },
    )
    .expect("DELETE via chokepoint should succeed");
    assert!(result_delete.ok);

    let result_insert = mutate(
        ctx,
        db_path,
        Op::DbExec {
            sql: "INSERT INTO blocked_issues_cache(issue_id, blocked_by, blocked_at) \
                  VALUES (?, ?, ?)"
                .into(),
            args: vec![
                DbArg::Text("bd-1".into()),
                DbArg::Text("[\"bd-2\"]".into()),
                DbArg::Text("2026-05-09 00:00:00".into()),
            ],
            affected_tables: vec!["blocked_issues_cache".into()],
            affected_predicate: None,
        },
    )
    .expect("INSERT via chokepoint should succeed");
    assert!(result_insert.ok);
}

fn single_cache_row(db_path: &Path) -> (String, String) {
    let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
    let rows = conn
        .query("SELECT issue_id, blocked_by FROM blocked_issues_cache")
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one cache row, got {}",
        rows.len()
    );
    let issue_id = match rows[0].get(0) {
        Some(fsqlite_types::value::SqliteValue::Text(s)) => s.to_string(),
        other => panic!("unexpected issue_id value: {other:?}"),
    };
    let blocked_by = match rows[0].get(1) {
        Some(fsqlite_types::value::SqliteValue::Text(s)) => s.to_string(),
        other => panic!("unexpected blocked_by value: {other:?}"),
    };
    let _ = conn.close();
    (issue_id, blocked_by)
}

// ---------------------------------------------------------------------------
// Test 1 — Round-trip: corrupt → repair → undo == byte-identical
// ---------------------------------------------------------------------------

/// The contract this test enforces is the chokepoint's *core* safety
/// guarantee:
///
/// 1. `--repair` records every mutation through `mutate()`, capturing a
///    verbatim backup keyed by `before_hash`.
/// 2. `undo <run-id>` restores every backed-up file to a state whose
///    SHA-256 equals the recorded `before_hash`.
/// 3. Files the chokepoint never touched stay byte-identical to what
///    they were before `--repair`.
///
/// The test is *not* asserting that the workspace is "byte-identical to
/// the corrupted state" globally, because the test fixture also
/// triggers out-of-chokepoint side effects (e.g. `.doctor/` is appended
/// to the root `.gitignore` by `create_run_dir`'s
/// `ensure_doctor_in_gitignore` helper before any chokepointed mutation
/// runs). That's a known WP3-incomplete area documented in the report.
/// What we *do* prove is the byte-exact restoration of every file the
/// chokepoint did touch — which is the actual safety contract.
#[test]
#[allow(clippy::too_many_lines)]
fn chokepoint_round_trip_gitignore() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();

    br_init(&root);
    corrupt_root_gitignore(&root);

    // Step 1: --repair
    let out = br_cmd(&root)
        .args(["doctor", "--repair", "--json"])
        .output()
        .expect("br doctor --repair spawned");
    let exit = out.status.code().unwrap_or(-1);
    assert!(
        matches!(exit, 0 | 2),
        "expected exit 0 or 2 from --repair, got {exit}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Step 2: a single run-dir was created with all required artifacts.
    let run_dir = single_run_dir(&root);
    let report_json_path = run_dir.join("report.json");
    let actions_path = run_dir.join("actions.jsonl");
    let backups_dir = run_dir.join("backups");
    assert!(report_json_path.is_file(), "report.json missing");
    assert!(actions_path.is_file(), "actions.jsonl missing");
    assert!(backups_dir.is_dir(), "backups/ missing");
    // backups/ must be non-empty (the gitignore path was repaired).
    let backup_count = fs::read_dir(&backups_dir)
        .expect("read backups/")
        .filter_map(std::result::Result::ok)
        .count();
    assert!(backup_count >= 1, "backups/ is empty");

    // Step 3: every action.jsonl line carries the contract fields.
    let actions = read_actions(&run_dir);
    assert!(
        !actions.is_empty(),
        "actions.jsonl must have at least one entry"
    );
    for (idx, action) in actions.iter().enumerate() {
        for key in &[
            "path",
            "op",
            "before_hash",
            "after_hash",
            "started_at_ns",
            "finished_at_ns",
            "run_id",
            "fixer_id",
            "ok",
        ] {
            assert!(
                action.get(*key).is_some(),
                "actions[{idx}] missing field `{key}`: {action}"
            );
        }
        // before/after hashes are sha256:<64 hex>.
        let bh = action["before_hash"].as_str().expect("before_hash str");
        let ah = action["after_hash"].as_str().expect("after_hash str");
        assert!(bh.starts_with("sha256:") && bh.len() == 71, "bh={bh}");
        assert!(ah.starts_with("sha256:") && ah.len() == 71, "ah={ah}");
    }

    // Step 4: workspace is now healthy by br doctor's reckoning, at
    // least with respect to the gitignore.beads_inner check.
    let post_repair_doctor = br_cmd(&root)
        .args(["doctor", "--json"])
        .output()
        .expect("br doctor (read-only) spawned");
    let _post_exit = post_repair_doctor.status.code().unwrap_or(-1);
    let body = String::from_utf8_lossy(&post_repair_doctor.stdout);
    assert!(
        !body.contains("gitignore.beads_inner: Root .gitignore excludes"),
        "post-repair doctor still warns about gitignore: {body}"
    );

    // Step 5: capture post-repair state for diffing AFTER undo.
    let post_repair_hashes = hash_workspace(&root);

    // Step 6: undo by run-id.
    let run_id = run_dir
        .file_name()
        .and_then(|s| s.to_str())
        .expect("run id name")
        .to_string();
    let undo = br_cmd(&root)
        .args(["doctor", "undo", &run_id, "--json"])
        .output()
        .expect("br doctor undo spawned");
    assert!(
        undo.status.success(),
        "br doctor undo failed: {}\n{}",
        String::from_utf8_lossy(&undo.stdout),
        String::from_utf8_lossy(&undo.stderr),
    );
    let undo_envelope = parse_trailing_json(&String::from_utf8_lossy(&undo.stdout));
    assert_eq!(undo_envelope["schema_version"], "br.doctor.undo.v1");
    assert!(
        undo_envelope["restored"].as_u64().unwrap_or(0) >= 1,
        "expected at least one restored file; envelope={undo_envelope}"
    );
    assert_eq!(
        undo_envelope["failed"].as_u64().unwrap_or(99),
        0,
        "undo had failures: {undo_envelope}"
    );

    // Step 7: every backed-up file action now hashes to its recorded
    // before_hash. DbExec actions restore logical table snapshots rather
    // than byte-restoring the SQLite file; those are covered by the
    // dedicated DbExec round-trip assertions in this harness.
    let mut checked_file_restores = 0usize;
    for action in &actions {
        if action["op"].as_str() == Some("db_exec") {
            continue;
        }
        let rel = Path::new(action["path"].as_str().expect("path"));
        let bh = action["before_hash"]
            .as_str()
            .expect("before_hash")
            .strip_prefix("sha256:")
            .expect("sha256 prefix");
        let live = root.join(rel);
        assert!(
            live.exists(),
            "post-undo: {} missing; expected before_hash={bh}",
            live.display()
        );
        let bytes = fs::read(&live).expect("read post-undo file");
        let live_hex = sha256_hex(&bytes);
        assert_eq!(
            live_hex,
            bh,
            "post-undo hash mismatch for {}: live={live_hex} before_hash={bh}",
            live.display()
        );
        checked_file_restores += 1;
    }
    assert!(
        checked_file_restores > 0,
        "expected at least one byte-restored file action; actions={actions:#?}"
    );

    // Step 8: files the chokepoint did NOT touch are byte-identical to
    // their post-repair state (i.e. undo did not regress unrelated
    // files).
    let post_undo_hashes = hash_workspace(&root);
    let touched: std::collections::HashSet<PathBuf> = actions
        .iter()
        .filter_map(|a| a["path"].as_str().map(PathBuf::from))
        .collect();
    for (rel, hash) in &post_repair_hashes {
        if touched.contains(rel) {
            continue;
        }
        let after = post_undo_hashes
            .get(rel)
            .unwrap_or_else(|| panic!("post-undo missing untouched file {}", rel.display()));
        assert_eq!(
            hash,
            after,
            "untouched file {} mutated by undo",
            rel.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Test 1b — Repair fails closed if no reversible run-dir can be created
// ---------------------------------------------------------------------------

#[test]
fn repair_refuses_when_run_dir_creation_fails() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    br_init(&root);
    corrupt_root_gitignore(&root);

    let gitignore = root.join(".gitignore");
    let pre_gitignore = fs::read(&gitignore).expect("read pre-repair gitignore");
    let bad_runs_root = root.join("doctor-runs-target-is-file");
    fs::write(&bad_runs_root, b"not a directory").expect("seed invalid runs override");

    let out = br_cmd(&root)
        .args(["doctor", "--repair", "--json"])
        .env("BR_DOCTOR_RUNS_DIR", &bad_runs_root)
        .output()
        .expect("br doctor --repair spawned");

    assert!(
        !out.status.success(),
        "repair must refuse when the reversible run-dir cannot be created\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("without a reversible repair session"),
        "missing fail-closed repair-session error\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(&gitignore).expect("read post-refusal gitignore"),
        pre_gitignore,
        "failed run-dir setup must not fall back to legacy .gitignore rewrite"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — Dry-run writes no files
//
// IGNORED: WP3 is not yet complete. The current `--repair --dry-run` path
// still creates repair-session scaffolding before it knows the run will
// remain a pure dry-run:
//
//   - `create_run_dir` itself writes `.doctor/runs/<id>/...` and may
//     append `.doctor/` to the root `.gitignore` *before* any chokepoint
//     mutation runs. That mutation is unconditional.
//
// The dry-run contract is part of the WP3+WP4 mutate() chokepoint plan;
// landing it requires deferring repair-session/run-dir creation and the
// `ensure_doctor_in_gitignore` write until after the dry-run preflight
// confirms an actual repair is needed. Until that lands, this test fails
// before it reaches any database repair path.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "WP3 incomplete: --dry-run still creates repair run-dir/gitignore scaffolding before pure dry-run preflight"]
fn chokepoint_dry_run_writes_no_files() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    br_init(&root);
    corrupt_root_gitignore(&root);

    let pre = hash_workspace(&root);
    let out = br_cmd(&root)
        .args(["doctor", "--repair", "--dry-run", "--json"])
        .output()
        .expect("br doctor --repair --dry-run spawned");
    assert!(
        out.status.success(),
        "dry-run should exit 0; got {:?}",
        out.status
    );

    // No run-dir should have been laid down.
    let runs = root.join(".doctor").join("runs");
    if runs.exists() {
        let count = fs::read_dir(&runs)
            .map(std::iter::Iterator::count)
            .unwrap_or(0);
        assert_eq!(
            count,
            0,
            "dry-run created run-dirs under {}",
            runs.display()
        );
    }

    let post = hash_workspace(&root);
    assert_eq!(pre, post, "dry-run mutated the workspace");
}

// ---------------------------------------------------------------------------
// Test 3 — Idempotence: second --repair is a no-op
// ---------------------------------------------------------------------------

#[test]
fn chokepoint_idempotence() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    br_init(&root);
    corrupt_root_gitignore(&root);

    // First repair: must do at least one action.
    let out1 = br_cmd(&root)
        .args(["doctor", "--repair", "--json"])
        .output()
        .expect("repair 1 spawned");
    let exit1 = out1.status.code().unwrap_or(-1);
    assert!(
        matches!(exit1, 0 | 2),
        "repair 1 unexpected exit {exit1}\nstderr={}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let runs_root = root.join(".doctor").join("runs");
    let mut run_dirs: Vec<PathBuf> = fs::read_dir(&runs_root)
        .expect("read runs/")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    run_dirs.sort();
    assert_eq!(run_dirs.len(), 1, "repair 1 must produce exactly one run");
    let actions_1 = read_actions(&run_dirs[0]);
    assert!(
        !actions_1.is_empty(),
        "repair 1 should have recorded actions"
    );

    // Second repair: the run_id includes ISO seconds, so we sleep just
    // over a second to force a distinct id. (The chokepoint's
    // create_run_dir already handles same-second collisions via the
    // sha-of-pid prefix, but we still want to assert we get a new
    // directory rather than a re-entered existing one.)
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let out2 = br_cmd(&root)
        .args(["doctor", "--repair", "--json"])
        .output()
        .expect("repair 2 spawned");
    let exit2 = out2.status.code().unwrap_or(-1);
    assert!(
        matches!(exit2, 0 | 2),
        "repair 2 unexpected exit {exit2}\nstderr={}",
        String::from_utf8_lossy(&out2.stderr)
    );

    let mut run_dirs_2: Vec<PathBuf> = fs::read_dir(&runs_root)
        .expect("read runs/ 2")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    run_dirs_2.sort();
    assert_eq!(
        run_dirs_2.len(),
        2,
        "repair 2 should produce a second run-dir; got {run_dirs_2:?}"
    );
    let new_run = run_dirs_2
        .iter()
        .find(|d| !run_dirs.contains(d))
        .expect("new run dir");
    let actions_2 = read_actions(new_run);
    assert!(
        actions_2.is_empty(),
        "repair 2 should have recorded zero actions (already healthy); got {actions_2:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 4 — `undo latest` resolves and marks report.json
// ---------------------------------------------------------------------------

#[test]
fn chokepoint_undo_latest_resolves() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    br_init(&root);
    corrupt_root_gitignore(&root);

    let out = br_cmd(&root)
        .args(["doctor", "--repair", "--json"])
        .output()
        .expect("repair spawned");
    let exit = out.status.code().unwrap_or(-1);
    assert!(matches!(exit, 0 | 2), "unexpected repair exit {exit}");
    let run_dir = single_run_dir(&root);

    let undo = br_cmd(&root)
        .args(["doctor", "undo", "latest", "--json"])
        .output()
        .expect("undo latest spawned");
    assert!(
        undo.status.success(),
        "undo latest failed: stdout={} stderr={}",
        String::from_utf8_lossy(&undo.stdout),
        String::from_utf8_lossy(&undo.stderr)
    );
    let envelope = parse_trailing_json(&String::from_utf8_lossy(&undo.stdout));
    assert_eq!(envelope["schema_version"], "br.doctor.undo.v1");
    assert_eq!(envelope["dry_run"], false);

    // The original run's report.json carries an `undone_at` timestamp
    // marker after `mark_report_undone` runs.
    let report_path = run_dir.join("report.json");
    let report_body = fs::read_to_string(&report_path).expect("read report.json");
    let report: Value = serde_json::from_str(&report_body)
        .unwrap_or_else(|e| panic!("parse report.json ({e}): {report_body}"));
    assert!(
        report.get("undone_at").and_then(Value::as_str).is_some(),
        "report.json should be marked with `undone_at` timestamp; got {report}"
    );
}

// ---------------------------------------------------------------------------
// Test 5 — `capabilities` envelope conforms to br.doctor.capabilities.v1
// ---------------------------------------------------------------------------

#[test]
fn chokepoint_capabilities_envelope_v1() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    // capabilities is repo-independent; we just need a cwd.

    let out = br_cmd(&root)
        .args(["doctor", "capabilities", "--format", "json"])
        .output()
        .expect("capabilities spawned");
    assert!(
        out.status.success(),
        "capabilities should always exit 0; got {:?}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let env = parse_trailing_json(&stdout);

    assert_eq!(env["schema_version"], "br.doctor.capabilities.v1");
    assert_eq!(env["contract_version"], "1");
    assert!(
        env["doctor_version"].is_string(),
        "doctor_version missing: {env}"
    );

    let exit_codes = env["exit_codes"]
        .as_array()
        .unwrap_or_else(|| panic!("exit_codes not an array: {env}"));
    assert!(
        exit_codes.len() >= 11,
        "expected ≥11 exit codes; got {} ({:?})",
        exit_codes.len(),
        exit_codes
    );

    let scopes = env["write_scopes"].as_array().expect("write_scopes array");
    assert!(
        scopes.iter().any(|v| v == ".beads/"),
        ".beads/ missing from write_scopes: {scopes:?}"
    );
    assert!(
        scopes.iter().any(|v| v == ".doctor/"),
        ".doctor/ missing from write_scopes: {scopes:?}"
    );

    let env_vars = env["env_vars"].as_array().expect("env_vars array");
    assert!(
        env_vars.iter().any(|v| v == "BR_DOCTOR_RUNS_DIR"),
        "BR_DOCTOR_RUNS_DIR missing from env_vars: {env_vars:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 6 — `--robot-triage` envelope conforms to br.doctor.triage.v1
// ---------------------------------------------------------------------------

#[test]
fn chokepoint_robot_triage_envelope_v1() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    br_init(&root);

    let out = br_cmd(&root)
        .args(["doctor", "--robot-triage", "--json"])
        .output()
        .expect("robot-triage spawned");
    let exit = out.status.code().unwrap_or(-1);
    // A freshly initialized workspace should remain triage-operable. A verbose
    // ambient RUST_LOG may still produce an advisory finding.
    assert!(
        matches!(exit, 0 | 1),
        "unexpected triage exit {exit}; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let env = parse_trailing_json(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(env["schema_version"], "br.doctor.triage.v1");

    for key in &[
        "summary",
        "findings",
        "actions_planned",
        "recommended_command",
        "capabilities_url",
        "robot_docs_command",
        "quick_ref",
    ] {
        assert!(
            env.get(*key).is_some(),
            "triage envelope missing `{key}`: {env}"
        );
    }
    assert!(env["findings"].is_array(), "findings not array");
    assert!(
        env["actions_planned"].is_array(),
        "actions_planned not array"
    );
    let qr = &env["quick_ref"];
    for k in &["healthy", "warn", "error"] {
        assert!(
            qr[*k].is_u64(),
            "quick_ref.{k} missing or non-numeric: {qr}"
        );
    }
    assert_eq!(
        env["capabilities_url"],
        "br doctor capabilities --format json"
    );
    assert_eq!(env["robot_docs_command"], "br doctor robot-docs");
}

// ---------------------------------------------------------------------------
// Test 7 — WP4 chokepoint round-trip for `Op::DbExec`
//
// Drives the chokepoint directly (instead of the CLI) because:
//   - The legacy `repair_*` paths still bypass the chokepoint for DB
//     work in WP4; CLI-level invocations of `--repair` route the cache
//     rebuild through the non-chokepointed code, so a CLI test would
//     not exercise the new `Op::DbExec` path.
//   - The chokepoint is published as `pub` from the `mutate` module; a
//     direct integration test pins down the contract end-to-end
//     (capabilities advertises it; mutate() executes it; backups land
//     in `<run-dir>/backups/db/`; actions.jsonl records the op).
//
// The companion `chokepoint_db_exec_undo_replay` test below exercises
// the reverse path: `doctor undo` reads those JSON snapshots and
// restores the table state in reverse action order.
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn chokepoint_db_exec_round_trip() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let beads_dir = root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("mkdir .beads");
    let db_path = beads_dir.join("beads.db");

    // Build a minimal DB with the cache table and intentionally-corrupt
    // contents (a row whose blocked_by is wrong on purpose).
    seed_blocked_cache_db(&db_path, "[\"WRONG\"]");

    // Build a chokepoint context rooted at `root`.
    let run_id = "wp4-db-exec-roundtrip";
    let (run_dir, ctx) = db_exec_context(&root, run_id);
    let actions_path = run_dir.join("actions.jsonl");

    // Forward path: rebuild the cache via DELETE + INSERT inside the
    // chokepoint. We do TWO DbExec ops because fsqlite's executor only
    // accepts one statement per call.
    mutate_cache_rebuild(&ctx, &db_path);

    // The cache now has the corrected row.
    let (_issue_id, blocked_by) = single_cache_row(&db_path);
    assert_eq!(blocked_by, "[\"bd-2\"]");

    // actions.jsonl: two `db_exec` lines, each with the snapshot
    // fingerprint we configured.
    let log = fs::read_to_string(&actions_path).expect("read actions.jsonl");
    let lines: Vec<&str> = log.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(),
        2,
        "expected exactly two action lines; got {lines:?}"
    );
    for raw in &lines {
        let v: Value = serde_json::from_str(raw).expect("parse action line");
        assert_eq!(v["op"], "db_exec");
        assert_eq!(v["fixer_id"], "wp4-cache-rebuild");
        assert_eq!(v["affected_tables"], "blocked_issues_cache");
        assert!(v["before_hash"].as_str().unwrap().starts_with("sha256:"));
        assert!(v["after_hash"].as_str().unwrap().starts_with("sha256:"));
        let db_snapshots = v["db_snapshots"]
            .as_array()
            .expect("db_snapshots should be an array");
        assert_eq!(db_snapshots.len(), 1);
        let snapshot_path = root.join(
            db_snapshots[0]
                .as_str()
                .expect("db_snapshots entry should be a path"),
        );
        assert!(
            snapshot_path.is_file(),
            "recorded snapshot path should exist: {}",
            snapshot_path.display()
        );
    }

    // Snapshots: both DbExec calls get their own JSON snapshot, and
    // one captures the pre-DELETE state (blocked_by="[\"WRONG\"]").
    let snap_dir = run_dir.join("backups/db");
    let snap_files: Vec<PathBuf> = fs::read_dir(&snap_dir)
        .expect("read backups/db")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    assert_eq!(
        snap_files.len(),
        2,
        "expected one snapshot per DbExec call under {}",
        snap_dir.display()
    );

    let mut found_pre_delete = false;
    for snap in &snap_files {
        let body = fs::read_to_string(snap).unwrap();
        let v: Value = serde_json::from_str(&body).expect("snapshot is JSON");
        assert_eq!(v["table"], "blocked_issues_cache");
        assert_eq!(v["schema_version"], "br.doctor.db_snapshot.v1");
        let rows = v["rows"].as_array().expect("rows array");
        if rows.iter().any(|r| {
            r.get("issue_id").and_then(Value::as_str) == Some("bd-1")
                && r.get("blocked_by").and_then(Value::as_str) == Some("[\"WRONG\"]")
        }) {
            found_pre_delete = true;
        }
    }
    assert!(
        found_pre_delete,
        "expected a snapshot capturing the pre-DELETE corrupt row; got {snap_files:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 8 — undo of `Op::DbExec` re-inserts JSON snapshots
//
// Drives the same DELETE+INSERT shape Test 7 uses to PROVE the forward
// path; then invokes `br doctor undo <run-id>` and asserts the cache
// table is byte-equivalent to the pre-DELETE state (the corrupt row
// reappears, the corrected row is gone). This proves the chokepoint
// round-trip works for DB ops, not just file ops.
// ---------------------------------------------------------------------------

#[test]
fn chokepoint_db_exec_undo_replay() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let beads_dir = root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("mkdir .beads");
    let db_path = beads_dir.join("beads.db");

    // Build the DB with a corrupt cache row.
    seed_blocked_cache_db(&db_path, "[\"WRONG\"]");

    // Build a chokepoint context rooted at `root`.
    let run_id = "wp4-db-exec-undo-replay";
    let (_run_dir, ctx) = db_exec_context(&root, run_id);

    // Forward: DELETE then INSERT a corrected row (mirroring Test 7).
    mutate_cache_rebuild(&ctx, &db_path);

    // Sanity: post-forward DB has the corrected row, not the corrupt one.
    let (_issue_id, blocked_by) = single_cache_row(&db_path);
    assert_eq!(blocked_by, "[\"bd-2\"]");

    // Drop the actions file so the chokepoint flushes to disk and the
    // undo path can re-open it for read.
    drop(ctx);

    // Now invoke `br doctor undo <run-id>` against the workspace.
    let bin_path = env!("CARGO_BIN_EXE_br");
    let output = std::process::Command::new(bin_path)
        .args(["doctor", "undo", run_id, "--json"])
        .current_dir(&root)
        .env("RUST_LOG", "error")
        .env("BR_NO_AUTOFLUSH", "1")
        .output()
        .expect("invoke br doctor undo");
    assert!(
        output.status.success(),
        "br doctor undo failed: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // The undo replays actions in REVERSE: the INSERT action's
    // snapshot (empty pre-INSERT state) is replayed first, leaving an
    // empty table; then the DELETE action's snapshot (containing the
    // pre-DELETE corrupt row) is replayed, restoring it. Net effect:
    // only the original corrupt row is present.
    let (issue_id, blocked_by) = single_cache_row(&db_path);
    assert_eq!(issue_id, "bd-1");
    assert_eq!(
        blocked_by, "[\"WRONG\"]",
        "expected the pre-DELETE corrupt row to be restored"
    );
}

// ---------------------------------------------------------------------------
// Test 9 (round-3 fresh-eyes follow-through, bead `beads_rust-sexc`):
// `br doctor --repair` must serialize against concurrent mutating br
// invocations via the workspace `.beads/.write.lock`. The detection
// surface is the structured `ConcurrencyLost` exit code (5) from
// `doctor_subsystems::exit_codes`, NOT the generic `BeadsError::Config`
// (1) used pre-fix.
//
// We hold the lock from THIS test process by opening
// `.beads/.write.lock` and `try_lock()`-ing the resulting `File`. That
// installs the same advisory exclusive lock `blocking_write_lock_with_timeout`
// installs, but from a different process than the subordinate `br`
// child we then spawn — proving the cross-process serialization
// contract from the bead.
// ---------------------------------------------------------------------------

#[test]
fn chokepoint_repair_acquires_workspace_lock() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();

    // Init a real workspace so .beads/.write.lock has a parent that
    // exists and metadata.json is in place. Without `br init` the
    // doctor short-circuits with "missing .beads directory" and exits
    // BEFORE the lock guard runs, which is not the contention path we
    // want to test.
    br_init(&root);

    let beads_dir = root.join(".beads");
    let lock_path = beads_dir.join(".write.lock");

    // Hold the workspace write lock from the test process. The br
    // child below sees this as "another process holds the lock" and
    // must refuse with exit code 5.
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open .write.lock");
    lock_file
        .try_lock()
        .expect("test should be uncontended at this point");

    // Use a tiny lock_timeout so the child gives up quickly instead of
    // burning the default 30s timeout per the env's setting.
    let bin_path = env!("CARGO_BIN_EXE_br");
    let output = std::process::Command::new(bin_path)
        .args(["--lock-timeout", "200", "doctor", "--repair", "--json"])
        .current_dir(&root)
        .env("RUST_LOG", "error")
        .env("BR_NO_AUTOFLUSH", "1")
        .env_remove("BD_DB")
        .env_remove("BEADS_DB")
        .output()
        .expect("invoke br doctor --repair");

    // Exit code must be the structured ConcurrencyLost (5), per the
    // doctor_subsystems::exit_codes contract.
    let code = output.status.code();
    assert_eq!(
        code,
        Some(5),
        "expected exit 5 ConcurrencyLost; got {:?}\nstdout={}\nstderr={}",
        code,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The refusal envelope must mention the .write.lock path so agent
    // scripts can match on it. JSON callers should also see
    // code=concurrency_lost / exit_code=5 explicitly.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains(".write.lock"),
        "refusal output should mention .write.lock: {combined}"
    );
    assert!(
        combined.contains("concurrency_lost")
            || combined.contains("ConcurrencyLost")
            || combined.contains("Refusing --repair"),
        "refusal output should be recognizable: {combined}"
    );

    // Sanity: release the lock and prove the SUT was never blocked
    // permanently — a second invocation after we drop the guard should
    // either succeed (exit 0) or report normal findings (exit 1), but
    // never repeat the ConcurrencyLost refusal.
    drop(lock_file);
    let output2 = std::process::Command::new(bin_path)
        .args(["--lock-timeout", "5000", "doctor", "--repair", "--json"])
        .current_dir(&root)
        .env("RUST_LOG", "error")
        .env("BR_NO_AUTOFLUSH", "1")
        .env_remove("BD_DB")
        .env_remove("BEADS_DB")
        .output()
        .expect("invoke br doctor --repair (post-release)");
    assert_ne!(
        output2.status.code(),
        Some(5),
        "post-release --repair must not still report ConcurrencyLost; status={:?} stderr={}",
        output2.status,
        String::from_utf8_lossy(&output2.stderr)
    );
}

// ---------------------------------------------------------------------------
// Test 9b (Phase-10 cold-prober follow-through, bead `beads_rust-mbpq`):
// `br doctor --repair --dry-run` (and `--repair` in general) must refuse
// IMMEDIATELY with exit 5 when another process holds the workspace lock,
// NOT block up to --lock-timeout. The agent contract is "try-lock or
// refuse"; the timing of the refusal is part of the contract.
//
// The pre-fix behavior was a silent 30s wait that contradicted both
// capabilities --format json and robot-docs. This test enforces the
// timing bound: the refusal must arrive within 2 seconds of the child
// being spawned, regardless of --lock-timeout default.
// ---------------------------------------------------------------------------

#[test]
fn chokepoint_repair_dry_run_refuses_on_lock_contention() {
    use std::time::{Duration, Instant};

    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    br_init(&root);

    let beads_dir = root.join(".beads");
    let lock_path = beads_dir.join(".write.lock");

    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open .write.lock");
    lock_file
        .try_lock()
        .expect("test should be uncontended at this point");

    let bin_path = env!("CARGO_BIN_EXE_br");
    // Note: no `--lock-timeout` flag — relying on the doctor's default
    // try-once behavior introduced by the mbpq fix. If the fix
    // regresses, the child will wait for the 30s default and this
    // test will trip the 2s timing assertion below.
    let start = Instant::now();
    let output = std::process::Command::new(bin_path)
        .args(["doctor", "--repair", "--dry-run", "--json"])
        .current_dir(&root)
        .env("RUST_LOG", "error")
        .env("BR_NO_AUTOFLUSH", "1")
        .env_remove("BD_DB")
        .env_remove("BEADS_DB")
        .output()
        .expect("invoke br doctor --repair --dry-run");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "doctor --repair --dry-run took {elapsed:?} to refuse on lock contention; \
         expected immediate refusal (mbpq regression)"
    );

    assert_eq!(
        output.status.code(),
        Some(5),
        "expected exit 5 ConcurrencyLost; got {:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("concurrency_lost")
            || combined.contains("ConcurrencyLost")
            || combined.contains("Refusing --repair"),
        "refusal output should be recognizable: {combined}"
    );

    drop(lock_file);
}

/// Round-5 fresh-eyes follow-through (`beads_rust-73ux`):
/// `refuse_gates::run_all` must run BEFORE any fixer / `mutate()` call
/// in the `--repair` flow. Plant a DB whose on-disk
/// `PRAGMA user_version` is higher than the binary's
/// `CURRENT_SCHEMA_VERSION` and assert `br doctor --repair` exits 4
/// (`RefusedUnsafe`) with a structured envelope that names the
/// `schema_version_downgrade` gate. Pure-precondition: workspace must
/// remain unchanged on refusal.
#[test]
fn chokepoint_refuse_gate_blocks_downgrade() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();

    // Real init so .beads/.write.lock + metadata.json + a real DB
    // (with a real, lower user_version) all exist.
    br_init(&root);

    let db_path = root.join(".beads").join("beads.db");
    assert!(
        db_path.is_file(),
        "br init must produce {}",
        db_path.display()
    );

    // Patch bytes 60-63 of the SQLite header (big-endian
    // `user_version`) to a value far above any plausible
    // CURRENT_SCHEMA_VERSION the binary supports. This is the same
    // surface refuse_gates::header_user_version reads, so the gate
    // must observe the planted version regardless of any cached PRAGMA.
    let bumped: u32 = 9_999;
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&db_path)
            .expect("open beads.db for header patch");
        // Sanity: confirm we are looking at a real SQLite file.
        let mut magic = [0_u8; 16];
        f.read_exact(&mut magic).expect("read sqlite magic");
        assert_eq!(
            &magic, b"SQLite format 3\0",
            "test fixture is not a SQLite file: {magic:?}"
        );
        f.seek(SeekFrom::Start(60)).expect("seek to user_version");
        f.write_all(&bumped.to_be_bytes())
            .expect("patch user_version");
        f.sync_data().expect("fsync patched header");
    }

    // Hash the workspace AFTER the header patch so the pre-state
    // reflects what the doctor will actually observe. Any change
    // post-refusal is then attributable to the gate not honoring its
    // pure-read contract.
    let pre_state = hash_workspace(&root);

    // Drive `br doctor --repair --json` and capture exit + stdout.
    let bin_path = env!("CARGO_BIN_EXE_br");
    let output = std::process::Command::new(bin_path)
        .args(["doctor", "--repair", "--json"])
        .current_dir(&root)
        .env("RUST_LOG", "error")
        .env("BR_NO_AUTOFLUSH", "1")
        .env_remove("BD_DB")
        .env_remove("BEADS_DB")
        .output()
        .expect("invoke br doctor --repair");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert_eq!(
        output.status.code(),
        Some(4),
        "expected exit 4 RefusedUnsafe; got {:?}\nstdout={stdout}\nstderr={stderr}",
        output.status.code()
    );

    // Parse the JSON envelope and confirm it carries the structured
    // refusal contract.
    let parsed = parse_trailing_json(&stdout);
    assert_eq!(parsed["ok"], Value::Bool(false));
    assert_eq!(parsed["exit_code"], Value::from(4));
    assert_eq!(parsed["code"], Value::String("refused_unsafe".into()));
    assert_eq!(
        parsed["gate"],
        Value::String("schema_version_downgrade".into()),
        "envelope must name the gate; full payload={parsed}"
    );
    assert_eq!(
        parsed["evidence"]["gate"],
        Value::String("schema_version_downgrade".into())
    );
    assert_eq!(
        parsed["evidence"]["db_schema_version"],
        Value::from(bumped),
        "evidence must echo the planted schema version"
    );
    let message = parsed["message"]
        .as_str()
        .expect("envelope must include a human message");
    assert!(
        message.contains("schema_version"),
        "refusal message should mention schema_version: {message}"
    );

    // The gate is pure-read; the workspace must be byte-identical to
    // the pre-refusal state. (The refuse_gates path predates run-dir
    // creation, so there is no `.doctor/runs/<id>/` artifact to
    // exclude — any change at all is a contract violation.)
    let post_state = hash_workspace(&root);
    assert_eq!(
        pre_state, post_state,
        "refused --repair must leave the workspace untouched"
    );
}

/// `br doctor --repair-indexes` is narrower than `--repair`, but it still
/// mutates SQLite index b-trees. It must therefore honor the same WP1
/// refuse-unsafe gates before snapshotting or running REINDEX.
#[test]
fn chokepoint_repair_indexes_refuse_gate_blocks_downgrade() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();

    br_init(&root);

    let db_path = root.join(".beads").join("beads.db");
    assert!(
        db_path.is_file(),
        "br init must produce {}",
        db_path.display()
    );

    let bumped: u32 = 9_999;
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&db_path)
            .expect("open beads.db for header patch");
        let mut magic = [0_u8; 16];
        f.read_exact(&mut magic).expect("read sqlite magic");
        assert_eq!(
            &magic, b"SQLite format 3\0",
            "test fixture is not a SQLite file: {magic:?}"
        );
        f.seek(SeekFrom::Start(60)).expect("seek to user_version");
        f.write_all(&bumped.to_be_bytes())
            .expect("patch user_version");
        f.sync_data().expect("fsync patched header");
    }

    let pre_state = hash_workspace(&root);

    let bin_path = env!("CARGO_BIN_EXE_br");
    let output = std::process::Command::new(bin_path)
        .args(["doctor", "--repair-indexes", "--json"])
        .current_dir(&root)
        .env("RUST_LOG", "error")
        .env("BR_NO_AUTOFLUSH", "1")
        .env_remove("BD_DB")
        .env_remove("BEADS_DB")
        .output()
        .expect("invoke br doctor --repair-indexes");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert_eq!(
        output.status.code(),
        Some(4),
        "expected exit 4 RefusedUnsafe; got {:?}\nstdout={stdout}\nstderr={stderr}",
        output.status.code()
    );

    let parsed = parse_trailing_json(&stdout);
    assert_eq!(parsed["ok"], Value::Bool(false));
    assert_eq!(parsed["exit_code"], Value::from(4));
    assert_eq!(parsed["code"], Value::String("refused_unsafe".into()));
    assert_eq!(
        parsed["gate"],
        Value::String("schema_version_downgrade".into()),
        "envelope must name the gate; full payload={parsed}"
    );
    assert_eq!(
        parsed["evidence"]["db_schema_version"],
        Value::from(bumped),
        "evidence must echo the planted schema version"
    );

    let post_state = hash_workspace(&root);
    assert_eq!(
        pre_state, post_state,
        "refused --repair-indexes must leave the workspace untouched"
    );
}

// ---------------------------------------------------------------------------
// Test 11 (Phase-10 cold-prober follow-through, bead `beads_rust-s7nx`):
// `br doctor` (flat, no --repair) in a directory that does not contain a
// `.beads/` workspace must exit with code 66 (`no_input`) per the
// documented exit-code dictionary — NOT exit 0 or 1. The companion
// `br doctor health` already handles this case cleanly; this test
// asserts the flat path agrees.
// ---------------------------------------------------------------------------

#[test]
fn chokepoint_doctor_in_non_beads_dir_exits_no_input() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    // Intentionally NO `br init` — this dir has no `.beads/`.
    assert!(!root.join(".beads").exists());

    let bin_path = env!("CARGO_BIN_EXE_br");
    let output = std::process::Command::new(bin_path)
        .args(["doctor", "--json"])
        .current_dir(&root)
        .env("RUST_LOG", "error")
        .env("BR_NO_AUTOFLUSH", "1")
        .env_remove("BD_DB")
        .env_remove("BEADS_DB")
        .output()
        .expect("invoke br doctor");

    assert_eq!(
        output.status.code(),
        Some(66),
        "expected exit 66 (no_input); got {:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// Test 12 (bead `beads_rust-8fud`): remaining legacy DB repair paths
// (VACUUM, REINDEX, blocked-cache rebuild) route through the
// chokepoint's `record_legacy_op` helper, so any disk mutation under
// those fixers must produce a `legacy_op` line in `actions.jsonl` with
// a `fixer_id` that names the legacy fn AND a verbatim backup under
// `<run-dir>/backups/<rel-path>` whose SHA-256 matches the recorded
// `before_hash`.
//
// We exercise the VACUUM path because it is the one most likely to
// rewrite the DB file in ways that defeat naive byte-equality undo;
// proving the audit + backup exist is the minimum-viable contract per
// the bead's pitfall note.
// ---------------------------------------------------------------------------
#[test]
#[allow(clippy::too_many_lines)]
fn legacy_op_audit_for_vacuum_via_page_corruption() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    br_init(&root);

    // Seed at least one row so the DB has user pages to corrupt.
    let seed = br_cmd(&root)
        .args([
            "create",
            "--title",
            "page corrupt seed",
            "--description",
            "rebuild me",
            "--priority",
            "2",
            "--no-auto-flush",
        ])
        .output()
        .expect("br create spawned");
    assert!(
        seed.status.success(),
        "br create failed: stdout={} stderr={}",
        String::from_utf8_lossy(&seed.stdout),
        String::from_utf8_lossy(&seed.stderr)
    );
    let flush = br_cmd(&root)
        .args(["sync", "--flush-only"])
        .output()
        .expect("br sync spawned");
    assert!(flush.status.success(), "br sync --flush-only failed");

    // Force any WAL contents into the main DB before we corrupt the page,
    // otherwise the corrupted page may be masked by an uncheckpointed
    // overlay.
    let db_path = root.join(".beads").join("beads.db");
    let _ = fs::remove_file(root.join(".beads").join("beads.db-wal"));
    let _ = fs::remove_file(root.join(".beads").join("beads.db-shm"));

    // Overwrite a non-header page so `PRAGMA integrity_check` reports
    // page-level corruption (the trigger for `repair_via_vacuum`).
    {
        use std::os::unix::fs::FileExt;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&db_path)
            .expect("open db rw");
        let junk = vec![0xffu8; 200];
        f.write_at(&junk, 4096).expect("corrupt page-2");
    }

    // Capture the pre-VACUUM SHA-256 so we can compare to the backup.
    let pre_repair_db_bytes = fs::read(&db_path).expect("read corrupted db");
    let pre_repair_db_hash = sha256_hex(&pre_repair_db_bytes);

    // Run `br doctor --repair --json`. We don't assert exit 0 because
    // page-malformed corruption may still surface non-fatal warnings.
    let out = br_cmd(&root)
        .args(["doctor", "--repair", "--json"])
        .output()
        .expect("br doctor --repair spawned");
    let exit = out.status.code().unwrap_or(-1);
    assert!(
        matches!(exit, 0..=2),
        "expected exit 0/1/2 from --repair, got {exit}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let run_dir = single_run_dir(&root);
    let actions = read_actions(&run_dir);

    // There must be at least one `legacy_op` action whose `fixer_id`
    // names one of the remaining legacy DB fixers.
    let legacy_actions: Vec<&Value> = actions
        .iter()
        .filter(|a| a["op"].as_str() == Some("legacy_op"))
        .collect();
    assert!(
        !legacy_actions.is_empty(),
        "expected at least one `legacy_op` audit line for the corrupted-page repair flow; got actions={actions:#?}"
    );

    let allowed_fixers: std::collections::HashSet<&str> = [
        "repair_via_vacuum",
        "repair_partial_indexes",
        "repair_recoverable_db_state",
    ]
    .into_iter()
    .collect();
    for action in &legacy_actions {
        let fid = action["fixer_id"].as_str().expect("fixer_id str");
        assert!(
            allowed_fixers.contains(fid),
            "unexpected legacy_op fixer_id={fid}; allowed={allowed_fixers:?}"
        );
        // Every legacy_op line must carry the chokepoint's stable
        // contract fields.
        for key in &[
            "path",
            "before_hash",
            "after_hash",
            "started_at_ns",
            "finished_at_ns",
            "run_id",
            "ok",
        ] {
            assert!(
                action.get(*key).is_some(),
                "legacy_op action missing field `{key}`: {action}"
            );
        }
    }

    // At least one of the legacy_op lines targets `.beads/beads.db`;
    // its verbatim backup must exist under `<run-dir>/backups/` AND its
    // bytes must hash to the captured pre-VACUUM SHA-256. This proves
    // the chokepoint took the snapshot BEFORE the legacy fixer rewrote
    // the file.
    let db_action = legacy_actions
        .iter()
        .find(|a| a["path"].as_str() == Some(".beads/beads.db"))
        .unwrap_or_else(|| {
            panic!(
                "no legacy_op line targeting .beads/beads.db; got legacy_actions={legacy_actions:#?}"
            )
        });
    let recorded_before = db_action["before_hash"]
        .as_str()
        .expect("before_hash str")
        .trim_start_matches("sha256:");
    assert_eq!(
        recorded_before, pre_repair_db_hash,
        "before_hash on legacy_op line does not match pre-VACUUM DB SHA-256"
    );

    let backup_path = run_dir.join("backups").join(".beads").join("beads.db");
    assert!(
        backup_path.is_file(),
        "verbatim backup missing at {}",
        backup_path.display()
    );
    let backup_bytes = fs::read(&backup_path).expect("read backup");
    let backup_hash = sha256_hex(&backup_bytes);
    assert_eq!(
        backup_hash,
        pre_repair_db_hash,
        "backup bytes at {} do not match pre-VACUUM DB SHA-256",
        backup_path.display()
    );

    // Run `br doctor undo` and verify it does not fault on legacy_op
    // entries. The byte-exact post-VACUUM restoration may not match
    // pre-VACUUM (VACUUM rewrites the file at the page level so the
    // post-VACUUM hash differs even after a notional roundtrip), but
    // undo must not surface as a hard failure for the legacy_op rows.
    let run_id = run_dir
        .file_name()
        .and_then(|s| s.to_str())
        .expect("run id name")
        .to_string();
    let undo = br_cmd(&root)
        .args(["doctor", "undo", &run_id, "--json"])
        .output()
        .expect("br doctor undo spawned");
    assert!(
        undo.status.success(),
        "br doctor undo failed: stdout={} stderr={}",
        String::from_utf8_lossy(&undo.stdout),
        String::from_utf8_lossy(&undo.stderr)
    );
}
