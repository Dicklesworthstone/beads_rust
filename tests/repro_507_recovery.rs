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
use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_env};
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
    current_workspace_with_damage(pending, None)
}

/// Main-file damage for [`current_workspace_with_damage`]. The WAL-only edit
/// never rewrites these pages, so the damage stays visible through the WAL
/// (GH #523).
#[derive(Clone, Copy)]
enum Damage {
    /// The single-page B-tree with this name drops the last cell of its leaf.
    DropLastLeafCell(&'static str),
    /// The comments table page reverts to its version before the second of
    /// two comments was inserted, while both comments indexes keep that row:
    /// a row lost from its table, indistinguishable from stale index entries.
    LostCommentRow,
}

/// [`current_workspace`], optionally damaging the main file afterwards.
fn current_workspace_with_damage(pending: bool, damage: Option<Damage>) -> BrWorkspace {
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
    let damaged_page = match damage {
        Some(Damage::DropLastLeafCell(name)) => Some(tree_root_page(&connection, name)),
        _ => None,
    };
    let stale_comments_page = matches!(damage, Some(Damage::LostCommentRow))
        .then(|| stage_lost_comment_row(&connection, &db, &ids[0]));
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

    if let Some((root, page_size)) = damaged_page {
        drop_last_leaf_cell(&db, root, page_size);
    }
    if let Some((offset, page)) = stale_comments_page {
        write_stale_page(&db, offset, &page);
    }

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

/// Checkpoint one comment on `issue_id` into the main file, capture the
/// comments table page (offset and bytes), then checkpoint a second comment.
/// Writing the captured page back loses that row from the table while both
/// comments indexes still hold it ([`Damage::LostCommentRow`]).
fn stage_lost_comment_row(connection: &Connection, db: &Path, issue_id: &str) -> (usize, Vec<u8>) {
    let insert_comment = |text: &str| {
        connection
            .execute_with_params(
                "INSERT INTO comments (issue_id, author, text) VALUES (?1, 'fixture', ?2)",
                &[SqliteValue::from(issue_id), SqliteValue::from(text)],
            )
            .unwrap();
        connection
            .execute("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
    };
    insert_comment("kept comment");
    let (root, page_size) = tree_root_page(connection, "comments");
    let offset = (root - 1) * page_size;
    let page = fs::read(db).unwrap()[offset..][..page_size].to_vec();
    insert_comment("comment lost from its table");
    (offset, page)
}

/// Put a page captured by [`stage_lost_comment_row`] back into the main file.
fn write_stale_page(db: &Path, offset: usize, page: &[u8]) {
    let mut main = fs::read(db).unwrap();
    let current = &mut main[offset..][..page.len()];
    assert_ne!(current, page, "the second comment must have reached main");
    current.copy_from_slice(page);
    fs::write(db, main).unwrap();
}

/// Root page number and page size of the B-tree named `name`.
fn tree_root_page(connection: &Connection, name: &str) -> (usize, usize) {
    let integer = |row: beads_rust::franken_sync::Row| {
        row.get(0)
            .and_then(SqliteValue::as_integer)
            .and_then(|value| usize::try_from(value).ok())
            .expect("integer result")
    };
    let root = integer(
        connection
            .query_row_with_params(
                "SELECT rootpage FROM sqlite_master WHERE name = ?1",
                &[SqliteValue::from(name)],
            )
            .unwrap(),
    );
    (
        root,
        integer(connection.query_row("PRAGMA page_size").unwrap()),
    )
}

/// Damage a single-leaf B-tree in the main file by dropping its last cell.
fn drop_last_leaf_cell(db: &Path, root: usize, page_size: usize) {
    let mut main = fs::read(db).unwrap();
    let header = (root - 1) * page_size + if root == 1 { 100 } else { 0 };
    assert!(
        matches!(main[header], 0x0a | 0x0d),
        "page {root} must be a single leaf page, found type {:#x}",
        main[header]
    );
    let cells = u16::from_be_bytes([main[header + 3], main[header + 4]]);
    assert!(cells >= 2, "page {root} has {cells} cells");
    main[header + 3..header + 5].copy_from_slice(&(cells - 1).to_be_bytes());
    fs::write(db, main).unwrap();
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
    let report: Value =
        serde_json::from_str(&extract_json_payload(&doctor.stdout)).expect("doctor JSON");
    let check = |name: &str| {
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("{name} check"))
            .clone()
    };
    // Read-only inspection reads the WAL-only state behind the index through
    // a private snapshot, so the pending-merge verdict is reachable.
    assert_eq!(check("sync.merge_pending")["status"], "ok");
    let sidecars = check("db.sidecars")["message"]
        .as_str()
        .expect("db.sidecars message")
        .to_owned();
    assert!(sidecars.contains("zero-page"), "{sidecars}");
    assert!(sidecars.contains("migrate-schema recover"), "{sidecars}");
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

/// GH #532: a peer lease must refuse before creating a failure receipt, then
/// allow automatic recovery on the next invocation after the peer leaves.
/// Use committed WAL-only records, not the header-only fast-path fixture.
#[test]
fn sole_opener_contention_does_not_latch_startup_recovery_532() {
    if isolated_test("sole_opener_contention_does_not_latch_startup_recovery_532") {
        return;
    }
    let workspace = current_workspace(false);
    let poisoned = poison_index(&workspace);
    let db = workspace.root.join(".beads/beads.db");
    let peer = beads_rust::sync::DatabaseOpenerLease::register(&db).unwrap();
    let before = protected_payload(&workspace);
    assert!(recovery_runs(&workspace).is_empty());
    for attempt in 0..3 {
        let run = run_br(
            &workspace,
            ["list", "--json"],
            &format!("held_opener_532_{attempt}"),
        );
        let error = format!("{}{}", run.stdout, run.stderr);
        assert!(!run.status.success(), "{error}");
        assert!(error.contains("sole opener"), "{error}");
        assert!(!error.contains("already failed"), "{error}");
        assert_eq!(protected_payload(&workspace), before);
        assert_eq!(fs::read(db.with_file_name("beads.db-shm")).unwrap(), poisoned);
        assert!(recovery_runs(&workspace).is_empty());
    }
    drop(peer);

    // No explicit recover, receipt removal, or sidecar replacement intervenes.
    let list = succeeds(&workspace, &["list", "--json"], "released_opener_532");
    let issues = list["issues"].as_array().expect("issue list");
    assert_eq!(issues.len(), 3);
    assert!(issues.iter().all(|issue| issue["title"] == WAL_ONLY_TITLE));
    let runs = recovery_runs(&workspace);
    assert_eq!(runs.len(), 1, "one successful automatic recovery");
    let receipt: Value =
        serde_json::from_slice(&fs::read(runs[0].join("recovery-complete.json")).unwrap()).unwrap();
    assert_eq!(receipt["backup_scope"], "complete-family");
    let backup = Path::new(receipt["backup_path"].as_str().unwrap());
    assert_eq!(read_optional(&backup.join("beads.db-wal")), before[1]);
    assert_eq!(fs::read(backup.join("beads.db-shm")).unwrap(), poisoned);

    let created = succeeds(
        &workspace,
        &["create", "after sole opener release", "--json"],
        "released_opener_create_532",
    );
    let listed = succeeds(&workspace, &["list", "--json"], "released_opener_final_532");
    let issues = listed["issues"].as_array().expect("issue list");
    assert_eq!(issues.len(), 4);
    assert!(issues.iter().any(|issue| issue["id"] == created["id"]));
    assert_eq!(
        issues
            .iter()
            .filter(|issue| issue["title"] == WAL_ONLY_TITLE)
            .count(),
        3,
        "every original WAL-only record must survive the later mutation"
    );
}

/// GH #532 must not disable the incident latch for actual byte damage. Real
/// startup must retain one complete backup and refuse later reads AND writes
/// over the same corrupt family without accumulating copies or losing evidence.
#[test]
fn deterministic_recovery_failure_still_latches_without_new_copies_532() {
    if isolated_test("deterministic_recovery_failure_still_latches_without_new_copies_532") {
        return;
    }
    let workspace = current_workspace(false);
    let poisoned = poison_index(&workspace);
    let wal_path = workspace.root.join(".beads/beads.db-wal");
    let mut wal = fs::read(&wal_path).unwrap();
    wal[48] ^= 1; // First frame checksum; keep the valid WAL header intact.
    fs::write(&wal_path, wal).unwrap();
    let before = protected_payload(&workspace);
    let first = run_br(&workspace, ["list", "--json"], "bad_wal_532");
    let error = format!("{}{}", first.stdout, first.stderr);
    assert!(!first.status.success(), "{error}");
    assert!(error.contains("checksum"), "{error}");
    let runs = recovery_runs(&workspace);
    assert_eq!(runs.len(), 1, "one retained incident, not a snapshot storm");
    let failure_path = runs[0].join("recovery-failed.json");
    let failure_bytes = fs::read(&failure_path).unwrap();
    let receipt: Value = serde_json::from_slice(&failure_bytes).unwrap();
    assert_eq!(receipt["failure_kind"], "fail-closed");
    assert_eq!(receipt["backup_scope"], "complete-family");
    let backup = Path::new(receipt["backup_path"].as_str().unwrap());
    let retained = [
        "beads.db",
        "beads.db-wal",
        "beads.db-journal",
        "beads.db-shm",
    ]
    .map(|name| read_optional(&backup.join(name)));
    assert_eq!(retained[0], before[0]);
    assert_eq!(retained[1], before[1]);
    assert_eq!(retained[2], before[2]);
    assert_eq!(retained[3].as_ref(), Some(&poisoned));
    for (attempt, args) in [
        &["list", "--json"][..],
        &["create", "must remain refused", "--json"][..],
        &["list", "--json"][..],
    ]
    .into_iter()
    .enumerate()
    {
        let run = run_br(&workspace, args, &format!("bad_wal_retry_532_{attempt}"));
        let error = format!("{}{}", run.stdout, run.stderr);
        assert_eq!(run.status.code(), Some(6), "{error}");
        assert!(
            error.contains("already failed for this exact database family"),
            "{error}"
        );
        assert_eq!(recovery_runs(&workspace), runs, "no new recovery runs");
        assert_eq!(fs::read(&failure_path).unwrap(), failure_bytes);
        assert_eq!(
            [
                "beads.db",
                "beads.db-wal",
                "beads.db-journal",
                "beads.db-shm",
            ]
            .map(|name| read_optional(&backup.join(name))),
            retained,
            "all retained pre-recovery components must stay byte-identical"
        );
        assert_eq!(protected_payload(&workspace), before);
        assert_eq!(
            fs::read(workspace.root.join(".beads/beads.db-shm")).unwrap(),
            poisoned
        );
    }
}

fn integrity_status(workspace: &BrWorkspace, label: &str) -> Value {
    let doctor = run_br(
        workspace,
        ["doctor", "--json", "--no-auto-import", "--no-auto-flush"],
        label,
    );
    let report: Value =
        serde_json::from_str(&extract_json_payload(&doctor.stdout)).expect("doctor JSON");
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "sqlite.integrity_check")
        .expect("sqlite.integrity_check")
        .clone()
}

/// GH #523: a poisoned WAL index on a database whose secondary indexes were
/// already damaged locked every path out. Ordinary commands refused (their
/// recovery rehearsal failed integrity), `migrate-schema recover` refused for
/// the same reason, and `--repair-indexes` refused behind the pending-merge
/// gate that only recovery clears. Explicit recovery now restores admission
/// when a private rehearsal proves the damage is index-only, and the index
/// repair then clears it without losing the WAL-only records.
#[test]
fn poisoned_index_with_prior_index_damage_recovers_then_reindexes() {
    if isolated_test("poisoned_index_with_prior_index_damage_recovers_then_reindexes") {
        return;
    }
    let workspace = current_workspace_with_damage(
        false,
        Some(Damage::DropLastLeafCell("idx_issues_updated_at")),
    );
    let poisoned = poison_index(&workspace);
    let before = protected_payload(&workspace);

    // Automatic startup recovery (an ordinary command that may auto-import)
    // still refuses to run on damaged indexes, but now names the path out
    // and touches nothing live.
    let list = run_br(&workspace, ["list", "--json"], "damaged_list_refused");
    assert!(!list.status.success());
    let error = format!("{}{}", list.stdout, list.stderr);
    assert!(error.contains("corrupt indexes"), "{error}");
    assert!(
        error.contains("br doctor migrate-schema recover"),
        "{error}"
    );
    assert!(error.contains("br doctor --repair-indexes"), "{error}");
    assert_eq!(protected_payload(&workspace), before);
    assert_eq!(
        fs::read(workspace.root.join(".beads/beads.db-shm")).unwrap(),
        poisoned
    );

    // Explicit recovery restores admission without changing a durable byte
    // and reports the damage it found.
    let receipt = recover(&workspace, "damaged_recovery");
    assert_eq!(
        receipt["index_corruption"]["remediation"],
        "br doctor --repair-indexes"
    );
    assert_eq!(
        receipt["index_corruption"]["index_entries_without_table_rows"],
        false
    );
    assert!(
        !receipt["index_corruption"]["integrity_check"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert_eq!(protected_payload(&workspace), before);
    let backup = Path::new(receipt["backup_path"].as_str().unwrap());
    assert_eq!(fs::read(backup.join("beads.db-shm")).unwrap(), poisoned);

    let damaged = integrity_status(&workspace, "damaged_doctor");
    assert_ne!(damaged["status"], "ok", "{damaged}");
    assert!(
        damaged["message"]
            .as_str()
            .unwrap()
            .contains("br doctor --repair-indexes"),
        "{damaged}"
    );

    let repair = run_br(
        &workspace,
        [
            "doctor",
            "--repair-indexes",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "damaged_repair_indexes",
    );
    assert!(
        repair.status.success(),
        "{} {}",
        repair.stdout,
        repair.stderr
    );
    let repaired = integrity_status(&workspace, "repaired_doctor");
    assert_eq!(repaired["status"], "ok", "{repaired}");

    let list = succeeds(
        &workspace,
        &[
            "list",
            "--all",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "repaired_list",
    );
    let issues = list["issues"]
        .as_array()
        .expect("list --json returns an issues array");
    assert_eq!(issues.len(), 3, "{issues:?}");
    assert!(issues.iter().all(|issue| issue["title"] == WAL_ONLY_TITLE));
}

/// GH #523, the other side: damage that rebuilding the indexes cannot repair
/// (a table page lost a row the indexes still hold) keeps recovery closed and
/// the live family untouched.
#[test]
fn poisoned_index_with_table_damage_keeps_recovery_closed() {
    if isolated_test("poisoned_index_with_table_damage_keeps_recovery_closed") {
        return;
    }
    let workspace =
        current_workspace_with_damage(false, Some(Damage::DropLastLeafCell("dependencies")));
    let poisoned = poison_index(&workspace);
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
        "table_damage_refused",
    );
    assert!(!refused.status.success());
    let error = format!("{}{}", refused.stdout, refused.stderr);
    assert!(error.contains("not confined to indexes"), "{error}");
    assert_eq!(protected_payload(&workspace), before);
    assert_eq!(
        fs::read(workspace.root.join(".beads/beads.db-shm")).unwrap(),
        poisoned
    );
}

/// GH #523 review: index entries whose table row is missing may be the only
/// trace of a row lost from its table, and rebuilding the indexes deletes
/// them. Recovery still admits (refusing would recreate the lockout), but it
/// must not present the damage as harmless: both the automatic refusal and
/// the recovery receipt say rows may be missing and to check the JSONL first.
#[test]
fn poisoned_index_with_rows_missing_from_their_table_warns_before_reindex() {
    if isolated_test("poisoned_index_with_rows_missing_from_their_table_warns_before_reindex") {
        return;
    }
    let workspace = current_workspace_with_damage(false, Some(Damage::LostCommentRow));
    poison_index(&workspace);
    let before = protected_payload(&workspace);

    let list = run_br(&workspace, ["list", "--json"], "lost_row_list_refused");
    assert!(!list.status.success());
    let error = format!("{}{}", list.stdout, list.stderr);
    assert!(error.contains("br doctor --repair-indexes"), "{error}");
    assert!(
        error.contains("may have been lost from the table"),
        "{error}"
    );
    assert_eq!(protected_payload(&workspace), before);

    let receipt = recover(&workspace, "lost_row_recovery");
    assert_eq!(
        receipt["index_corruption"]["index_entries_without_table_rows"], true,
        "{receipt}"
    );
    let comments = receipt["logical_after"]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|witness| witness["name"] == "comments")
        .unwrap();
    assert_eq!(comments["row_count"], 1, "the table lost one of two rows");
    assert_eq!(protected_payload(&workspace), before);
}

/// Read-only commands cannot quarantine the live index, but they must still
/// read committed WAL-only rows behind it: a private snapshot rebuilds its own
/// index from the validated WAL. A WAL that fails validation is refused rather
/// than read without its frames. Neither case may touch the live family.
#[test]
fn read_only_commands_read_wal_only_rows_behind_a_poisoned_index() {
    if isolated_test("read_only_commands_read_wal_only_rows_behind_a_poisoned_index") {
        return;
    }
    let workspace = current_workspace(false);
    let poisoned = poison_index(&workspace);
    let before = protected_payload(&workspace);
    let list = succeeds(
        &workspace,
        &[
            "list",
            "--all",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "poisoned_read_only_list",
    );
    let issues = list["issues"]
        .as_array()
        .expect("list --json returns an issues array");
    assert_eq!(issues.len(), 3);
    assert!(issues.iter().all(|issue| issue["title"] == WAL_ONLY_TITLE));
    let id = issues[0]["id"].as_str().expect("issue id").to_owned();
    let db = workspace.root.join(".beads/beads.db");
    let show = succeeds(
        &workspace,
        &[
            "--db",
            db.to_str().expect("utf-8 path"),
            "--no-auto-import",
            "--no-auto-flush",
            "show",
            "--json",
            "--",
            &id,
        ],
        "poisoned_read_only_show",
    );
    assert_eq!(show[0]["title"], WAL_ONLY_TITLE);
    // This poison is not stock SQLite's admitted empty index (its WAL holds
    // frames), so the read really does go through the private snapshot.
    let probed = run_br_with_env(
        &workspace,
        [
            "list",
            "--all",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        [("RUST_LOG", "error,br::read_snapshot=info")],
        "poisoned_read_only_snapshot_probe",
    );
    assert!(probed.status.success(), "{}", probed.stderr);
    assert!(
        probed.stderr.contains("reading a private snapshot"),
        "{}",
        probed.stderr
    );
    assert_eq!(protected_payload(&workspace), before);
    assert_eq!(
        fs::read(workspace.root.join(".beads/beads.db-shm")).unwrap(),
        poisoned
    );

    let wal_path = workspace.root.join(".beads/beads.db-wal");
    let mut wal = fs::read(&wal_path).unwrap();
    wal[48] ^= 1; // First frame checksum, leaving the valid header intact.
    fs::write(&wal_path, wal).unwrap();
    let corrupt = protected_payload(&workspace);
    let refused = run_br(
        &workspace,
        [
            "list",
            "--all",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "poisoned_corrupt_wal_read_only_list",
    );
    assert!(
        !refused.status.success(),
        "a corrupt WAL must not be read as if it had no frames: {}",
        refused.stdout
    );
    assert!(
        !refused.stdout.contains(WAL_ONLY_TITLE),
        "{}",
        refused.stdout
    );
    assert_eq!(protected_payload(&workspace), corrupt);
    assert_eq!(
        fs::read(workspace.root.join(".beads/beads.db-shm")).unwrap(),
        poisoned
    );
}

/// Run directories under `.beads/.br_recovery/schema-migrations`.
fn recovery_runs(workspace: &BrWorkspace) -> Vec<std::path::PathBuf> {
    let root = workspace.root.join(".beads/.br_recovery/schema-migrations");
    let mut runs: Vec<_> = fs::read_dir(root)
        .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
        .unwrap_or_default();
    runs.sort();
    runs
}

/// The stock SQLite shape: #507's zero-page index beside a header-only WAL.
/// There only the index needs rebuilding, so recovery keeps only the index
/// instead of two copies of the database. When the database already had
/// damaged indexes, that cheap path must still end where the full path does:
/// the original index goes back byte for byte, and the complete-backup
/// rehearsal refuses automatic recovery and admits explicit recovery, as in
/// `poisoned_index_with_prior_index_damage_recovers_then_reindexes`.
#[test]
fn header_only_wal_poison_with_index_damage_falls_back_to_the_full_rehearsal() {
    if isolated_test("header_only_wal_poison_with_index_damage_falls_back_to_the_full_rehearsal") {
        return;
    }
    let workspace = BrWorkspace::new();
    succeeds(&workspace, &["init", "--prefix", "hw", "--json"], "init");
    for ordinal in 0..3 {
        succeeds(
            &workspace,
            &["create", &format!("settled issue {ordinal}"), "--json"],
            &format!("create_{ordinal}"),
        );
    }
    let db = workspace.root.join(".beads/beads.db");
    assert_eq!(
        fs::read(workspace.root.join(".beads/beads.db-wal"))
            .unwrap()
            .len(),
        32,
        "fixture precondition: header-only WAL, so main holds every row"
    );
    // Read the index's root page from a private copy; the live family is
    // only ever changed by the damage itself.
    let scratch = tempfile::tempdir().unwrap();
    let copy = scratch.path().join("copy.db");
    fs::copy(&db, &copy).unwrap();
    let connection = Connection::open(copy.to_string_lossy().into_owned()).unwrap();
    let (root, page_size) = tree_root_page(&connection, "idx_issues_updated_at");
    connection.close().unwrap();
    drop_last_leaf_cell(&db, root, page_size);
    let poisoned = poison_index(&workspace);
    let before = protected_payload(&workspace);

    let list = run_br(&workspace, ["list", "--json"], "damaged_header_only_list");
    assert!(!list.status.success());
    let error = format!("{}{}", list.stdout, list.stderr);
    assert!(error.contains("corrupt indexes"), "{error}");
    assert!(error.contains("br doctor --repair-indexes"), "{error}");
    assert_eq!(protected_payload(&workspace), before);
    assert_eq!(
        fs::read(workspace.root.join(".beads/beads.db-shm")).unwrap(),
        poisoned,
        "the original index is back in place"
    );
    let runs = recovery_runs(&workspace);
    assert_eq!(runs.len(), 2, "{runs:?}");
    let superseded = runs
        .iter()
        .find(|run| run.join("recovery-superseded.json").is_file())
        .expect("the index-only attempt records that it handed over");
    let retained: Vec<_> = fs::read_dir(superseded.join("recovery-before"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(retained, ["beads.db-shm"]);
    let full = runs
        .iter()
        .find(|run| run.join("recovery-failed.json").is_file())
        .expect("the complete-backup rehearsal refused");
    assert_eq!(
        fs::read(full.join("recovery-before/beads.db-shm")).unwrap(),
        poisoned
    );
    assert!(full.join("recovery-before/beads.db").is_file());

    let receipt = succeeds(
        &workspace,
        &[
            "doctor",
            "migrate-schema",
            "recover",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "damaged_header_only_recovery",
    );
    assert_eq!(receipt["stage"], "complete", "{receipt}");
    assert_eq!(receipt["backup_scope"], "complete-family", "{receipt}");
    assert_eq!(
        receipt["index_corruption"]["remediation"],
        "br doctor --repair-indexes"
    );
    assert_eq!(protected_payload(&workspace), before);
}
