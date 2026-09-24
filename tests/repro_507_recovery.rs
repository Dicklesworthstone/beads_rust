//! Exercise #507 through the operator-facing recovery command, not just an
//! engine connection. Fixtures use the current canonical schema and retain
//! unexported records plus committed WAL-only changes. No JSONL rebuild is
//! acceptable, and repairing the index must not authorize a pending merge.

#![cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]

mod common;

use beads_rust::franken_sync::{Connection, SqliteValue};
use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

const WAL_ONLY_TITLE: &str = "GH507 committed title only in WAL";
const PENDING: &str = "GH507 WAL-only pending receipt";

fn isolated_test(name: &str) -> bool {
    const CHILD_ENV: &str = "BR_TEST_ISOLATED_507_CLI_RECOVERY";
    if std::env::var(CHILD_ENV).as_deref() == Ok(name) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            name,
            "--test-threads=1",
            "--format=pretty",
            "--color=never",
        ])
        .env(CHILD_ENV, name)
        .env_remove("RUST_TEST_NOCAPTURE")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let passed = format!("test {name} ... ok");
    assert!(
        output.status.success() && stdout.lines().any(|line| line == passed),
        "isolated recovery test failed or did not run: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn read_optional(path: &Path) -> Option<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("read {}: {error}", path.display()),
    }
}

fn protected_payload(workspace: &BrWorkspace) -> [Option<Vec<u8>>; 4] {
    [
        ".beads/beads.db",
        ".beads/beads.db-wal",
        ".beads/beads.db-journal",
        ".beads/issues.jsonl",
    ]
    .map(|name| read_optional(&workspace.root.join(name)))
}

fn succeeds(workspace: &BrWorkspace, args: &[&str], label: &str) -> Value {
    let run = run_br(workspace, args, label);
    assert!(
        run.status.success(),
        "{label}: {} {}",
        run.stdout,
        run.stderr
    );
    serde_json::from_str(&extract_json_payload(&run.stdout)).expect("JSON result")
}

fn current_workspace(pending: bool) -> BrWorkspace {
    let workspace = BrWorkspace::new();
    succeeds(&workspace, &["init", "--prefix", "wal", "--json"], "init");
    let mut ids = Vec::new();
    for ordinal in 0..3 {
        let title = format!("unexported issue {ordinal}");
        let issue = succeeds(
            &workspace,
            &[
                "create",
                &title,
                "--json",
                "--no-auto-flush",
                "--no-auto-import",
            ],
            &format!("create_{ordinal}"),
        );
        ids.push(issue["id"].as_str().expect("created issue id").to_owned());
    }
    for (ordinal, edge) in ids.windows(2).enumerate() {
        succeeds(
            &workspace,
            &[
                "dep",
                "add",
                &edge[0],
                &edge[1],
                "--json",
                "--no-auto-flush",
                "--no-auto-import",
            ],
            &format!("dep_{ordinal}"),
        );
    }
    let exported = read_optional(&workspace.root.join(".beads/issues.jsonl")).unwrap_or_default();
    for id in &ids {
        assert!(
            !exported
                .windows(id.len())
                .any(|bytes| bytes == id.as_bytes()),
            "fixture must contain records absent from JSONL"
        );
    }

    let db = workspace.root.join(".beads/beads.db");
    let mut connection = Connection::open(db.to_string_lossy().into_owned()).unwrap();
    connection.execute("PRAGMA journal_mode = WAL").unwrap();
    connection.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
    connection
        .execute("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    connection
        .execute_with_params(
            "UPDATE issues SET title = ?1",
            &[SqliteValue::from(WAL_ONLY_TITLE)],
        )
        .unwrap();
    if pending {
        // The old receipt shape is intentionally still a blocking gate. It
        // lives only in WAL and must survive index recovery unchanged.
        connection
            .execute_with_params(
                "INSERT INTO metadata (key, value) VALUES ('sync_merge_pending_v1', ?1)",
                &[SqliteValue::from(PENDING)],
            )
            .unwrap();
    }
    connection.close_without_checkpoint_in_place().unwrap();
    drop(connection);

    let main = fs::read(&db).unwrap();
    let wal = fs::read(workspace.root.join(".beads/beads.db-wal")).unwrap();
    assert_eq!(
        u32::from_be_bytes(main[60..64].try_into().unwrap()),
        u32::try_from(beads_rust::storage::schema::CURRENT_SCHEMA_VERSION).unwrap(),
        "#507 is independent of an old schema"
    );
    for sentinel in std::iter::once(WAL_ONLY_TITLE).chain(pending.then_some(PENDING)) {
        assert!(
            !main
                .windows(sentinel.len())
                .any(|bytes| bytes == sentinel.as_bytes())
        );
        assert!(
            wal.windows(sentinel.len())
                .any(|bytes| bytes == sentinel.as_bytes())
        );
    }
    workspace
}

fn poison_index(workspace: &BrWorkspace) -> Vec<u8> {
    let mut header = [0; 48];
    header[..4].copy_from_slice(&3_007_000_u32.to_ne_bytes());
    header[12] = 1;
    let path = workspace.root.join(".beads/beads.db-shm");
    let mut file = OpenOptions::new().write(true).open(&path).unwrap();
    file.write_all(&header).unwrap();
    file.write_all(&header).unwrap();
    file.sync_all().unwrap();
    fs::read(path).unwrap()
}

fn recover(workspace: &BrWorkspace, label: &str) -> Value {
    let receipt = succeeds(
        workspace,
        &[
            "doctor",
            "migrate-schema",
            "recover",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        label,
    );
    assert_eq!(receipt["stage"], "complete");
    assert!(receipt["error"].is_null());
    for (table, count) in [("issues", 3), ("dependencies", 2)] {
        let witness = receipt["logical_after"]["tables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|witness| witness["name"] == table)
            .unwrap();
        assert_eq!(witness["row_count"], count);
    }
    receipt
}

#[test]
fn poisoned_current_schema_recovers_without_jsonl_rebuild_and_is_repeatable() {
    if isolated_test("poisoned_current_schema_recovers_without_jsonl_rebuild_and_is_repeatable") {
        return;
    }
    let workspace = current_workspace(false);
    let poisoned = poison_index(&workspace);
    let before = protected_payload(&workspace);

    let first = recover(&workspace, "first_recovery");
    assert_eq!(protected_payload(&workspace), before);
    let backup = Path::new(first["backup_path"].as_str().unwrap());
    assert_eq!(fs::read(backup.join("beads.db-shm")).unwrap(), poisoned);

    // Previously the healthy second recovery checkpointed on close because
    // only the first invocation had actually quarantined an index.
    recover(&workspace, "repeated_recovery");
    assert_eq!(protected_payload(&workspace), before);
    let list = succeeds(
        &workspace,
        &[
            "list",
            "--all",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "recovered_list",
    );
    let issues = list["issues"]
        .as_array()
        .expect("list --json returns an issues array");
    assert_eq!(issues.len(), 3);
    assert!(issues.iter().all(|issue| issue["title"] == WAL_ONLY_TITLE));
    assert_eq!(protected_payload(&workspace), before);
    succeeds(
        &workspace,
        &[
            "create",
            "writes work again",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "recovered_write",
    );
}

#[test]
fn recovered_pending_merge_still_refuses_writes_without_touching_wal() {
    if isolated_test("recovered_pending_merge_still_refuses_writes_without_touching_wal") {
        return;
    }
    let workspace = current_workspace(true);
    poison_index(&workspace);
    let before = protected_payload(&workspace);
    recover(&workspace, "pending_recovery");
    assert_eq!(protected_payload(&workspace), before);

    let refused = run_br(
        &workspace,
        [
            "create",
            "must not be created",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "pending_write_refused",
    );
    assert!(!refused.status.success());
    let error = format!("{}{}", refused.stdout, refused.stderr);
    assert!(
        error.contains("pending") && error.contains("legacy"),
        "{error}"
    );
    assert!(!error.contains("recovery in progress"), "{error}");
    assert_eq!(protected_payload(&workspace), before);
}

#[test]
fn missing_index_recovery_is_restartable_without_checkpointing() {
    if isolated_test("missing_index_recovery_is_restartable_without_checkpointing") {
        return;
    }
    let workspace = current_workspace(false);
    // This is also the durable state after a prior recovery moved the poison
    // but was interrupted before engine admission. The raw boundary crash is
    // exercised by franken_sync::wal_index's subprocess regression.
    fs::rename(
        workspace.root.join(".beads/beads.db-shm"),
        workspace.root.join("retained-before-restart-shm"),
    )
    .unwrap();
    let before = protected_payload(&workspace);
    recover(&workspace, "missing_index_recovery");
    assert_eq!(protected_payload(&workspace), before);
    assert!(workspace.root.join(".beads/beads.db-shm").is_file());
    recover(&workspace, "healthy_index_recovery");
    assert_eq!(protected_payload(&workspace), before);
}

#[test]
fn doctor_names_poisoned_index_and_routes_to_explicit_recovery() {
    if isolated_test("doctor_names_poisoned_index_and_routes_to_explicit_recovery") {
        return;
    }
    let workspace = current_workspace(false);
    let poisoned = poison_index(&workspace);
    let before = protected_payload(&workspace);

    let doctor = run_br(
        &workspace,
        ["doctor", "--json", "--no-auto-import", "--no-auto-flush"],
        "poisoned_doctor",
    );
    assert!(!doctor.status.success());
    let report: Value =
        serde_json::from_str(&extract_json_payload(&doctor.stdout)).expect("doctor JSON");
    let pending = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "sync.merge_pending")
        .expect("sync.merge_pending check");
    assert_eq!(
        pending["details"]["wal_index_state"],
        "initialized_zero_page_poison"
    );
    assert_eq!(
        pending["details"]["recovery_command"],
        "br doctor migrate-schema recover"
    );
    let remediation = pending["details"]["remediation"].as_str().unwrap();
    assert!(
        remediation.contains("migrate-schema recover"),
        "{remediation}"
    );
    assert!(remediation.contains("Do not run generic"), "{remediation}");
    assert_eq!(protected_payload(&workspace), before);
    assert_eq!(
        fs::read(workspace.root.join(".beads/beads.db-shm")).unwrap(),
        poisoned
    );

    let repair = run_br(
        &workspace,
        [
            "doctor",
            "--repair",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "poisoned_generic_repair_refused",
    );
    assert!(!repair.status.success());
    let refusal: Value =
        serde_json::from_str(&extract_json_payload(&repair.stdout)).expect("repair refusal JSON");
    assert_eq!(
        refusal["evidence"]["wal_index_state"],
        "initialized_zero_page_poison"
    );
    assert_eq!(
        refusal["evidence"]["recovery_command"],
        "br doctor migrate-schema recover"
    );
    assert_eq!(refusal["evidence"]["generic_repair_safe"], false);
    assert_eq!(protected_payload(&workspace), before);
    assert_eq!(
        fs::read(workspace.root.join(".beads/beads.db-shm")).unwrap(),
        poisoned
    );

    recover(&workspace, "doctor_named_recovery");
    assert_eq!(protected_payload(&workspace), before);
}

#[test]
fn mutating_command_auto_recovers_poisoned_index_before_pending_gate() {
    if isolated_test("mutating_command_auto_recovers_poisoned_index_before_pending_gate") {
        return;
    }
    let workspace = current_workspace(false);
    let poisoned = poison_index(&workspace);

    let create = run_br(
        &workspace,
        [
            "create",
            "after automatic poisoned-index recovery",
            "--json",
        ],
        "auto_poison_recovery",
    );
    assert!(
        create.status.success(),
        "{} {}",
        create.stdout,
        create.stderr
    );
    assert!(
        !workspace.root.join(".beads/beads.db-shm").exists()
            || fs::read(workspace.root.join(".beads/beads.db-shm")).unwrap() != poisoned,
        "startup recovery must replace or rebuild the poisoned derived index"
    );
    // The command itself now commits a new issue, so WAL bytes are expected
    // to change. Verify the old WAL-only logical data survived recovery and
    // the new mutation instead of asserting byte neutrality after a write.
    let list = run_br(
        &workspace,
        [
            "list",
            "--all",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "auto_poison_recovery_list",
    );
    assert!(list.status.success(), "{} {}", list.stdout, list.stderr);
    // The fixture's three issues exist only in the database family (never
    // exported to JSONL), and their current titles exist only in committed
    // WAL frames. Recovery must keep every one of them, plus the new issue.
    let listed: Value =
        serde_json::from_str(&extract_json_payload(&list.stdout)).expect("list JSON");
    let issues = listed["issues"]
        .as_array()
        .expect("list --json returns an issues array");
    let titles: Vec<&str> = issues
        .iter()
        .map(|issue| issue["title"].as_str().expect("issue title"))
        .collect();
    assert_eq!(
        titles
            .iter()
            .filter(|title| **title == WAL_ONLY_TITLE)
            .count(),
        3,
        "every database-only issue must survive automatic recovery: {titles:?}"
    );
    assert_eq!(
        titles
            .iter()
            .filter(|title| **title == "after automatic poisoned-index recovery")
            .count(),
        1,
        "{titles:?}"
    );
    assert_eq!(issues.len(), 4, "{titles:?}");
}

#[test]
fn mutating_command_auto_recovers_poison_but_preserves_pending_merge_refusal() {
    if isolated_test("mutating_command_auto_recovers_poison_but_preserves_pending_merge_refusal") {
        return;
    }
    let workspace = current_workspace(true);
    let poisoned = poison_index(&workspace);
    let before = protected_payload(&workspace);
    let create = run_br(
        &workspace,
        ["create", "must remain refused", "--json"],
        "auto_poison_pending_refusal",
    );
    assert!(
        !create.status.success(),
        "{} {}",
        create.stdout,
        create.stderr
    );
    let error = format!("{}{}", create.stdout, create.stderr);
    assert!(error.contains("pending"), "{error}");
    assert_eq!(
        read_optional(&workspace.root.join(".beads/beads.db")),
        before[0]
    );
    assert_eq!(
        read_optional(&workspace.root.join(".beads/beads.db-wal")),
        before[1]
    );
    assert!(
        !workspace.root.join(".beads/beads.db-shm").exists()
            || fs::read(workspace.root.join(".beads/beads.db-shm")).unwrap() != poisoned,
        "derived index should recover before the real pending-merge gate refuses mutation"
    );
}

#[test]
fn corrupt_wal_and_live_peer_refuse_before_live_index_quarantine() {
    if isolated_test("corrupt_wal_and_live_peer_refuse_before_live_index_quarantine") {
        return;
    }
    for corrupt in [false, true] {
        let workspace = current_workspace(false);
        let poisoned = poison_index(&workspace);
        let db = workspace.root.join(".beads/beads.db");
        let peer = if corrupt {
            let wal_path = workspace.root.join(".beads/beads.db-wal");
            let mut wal = fs::read(&wal_path).unwrap();
            wal[48] ^= 1; // First frame checksum, leaving the valid header intact.
            fs::write(wal_path, wal).unwrap();
            None
        } else {
            Some(beads_rust::sync::DatabaseOpenerLease::register(&db).unwrap())
        };
        let before = protected_payload(&workspace);
        let refused = run_br(
            &workspace,
            [
                "doctor",
                "migrate-schema",
                "recover",
                "--json",
                "--no-auto-import",
                "--no-auto-flush",
            ],
            "recovery_refused",
        );
        assert!(!refused.status.success());
        let error = format!("{}{}", refused.stdout, refused.stderr);
        assert!(
            error.contains(if corrupt { "checksum" } else { "sole opener" }),
            "{error}"
        );
        assert_eq!(protected_payload(&workspace), before);
        assert_eq!(
            fs::read(workspace.root.join(".beads/beads.db-shm")).unwrap(),
            poisoned
        );
        drop(peer);
    }
}
