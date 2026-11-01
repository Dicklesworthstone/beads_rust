//! Init must inspect its own database target before opening it writable.
//!
//! Init's local/default/cache path can differ from startup's metadata/--db
//! route. A broken metadata file also bypasses normal initialized startup.
//! Neither route authorizes Init to run the generic legacy schema upgrader.

mod common;

use beads_rust::franken_sync::{Connection, SqliteValue};
use common::cli::{BrRun, BrWorkspace, extract_json_payload, run_br, run_br_with_env};
use flate2::read::GzDecoder;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

type Inventory = BTreeMap<PathBuf, Option<Vec<u8>>>;
type RawRows = BTreeMap<String, Vec<Vec<SqliteValue>>>;

fn payload(stdout: &str) -> Value {
    serde_json::from_str(&extract_json_payload(stdout)).expect("complete JSON stdout")
}

fn inventory(path: &Path) -> Inventory {
    fn visit(path: &Path, files: &mut Inventory) {
        let metadata = fs::symlink_metadata(path).unwrap();
        if metadata.is_dir() {
            files.insert(path.to_path_buf(), None);
            for entry in fs::read_dir(path).unwrap() {
                visit(&entry.unwrap().path(), files);
            }
        } else {
            assert!(
                metadata.is_file(),
                "unexpected fixture path: {}",
                path.display()
            );
            files.insert(path.to_path_buf(), Some(fs::read(path).unwrap()));
        }
    }
    let mut files = BTreeMap::new();
    if path.exists() {
        visit(path, &mut files);
    }
    files
}

fn database_family(db: &Path) -> Inventory {
    let mut files = BTreeMap::new();
    let prefix = db.file_name().unwrap().to_string_lossy();
    for entry in fs::read_dir(db.parent().unwrap()).unwrap() {
        let entry = entry.unwrap();
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(prefix.as_ref())
        {
            files.extend(inventory(&entry.path()));
        }
    }
    files
}

/// The routed startup may acquire coordination files for its alternate
/// current database. The actual Init source family and every durable
/// tracker/recovery artifact must nevertheless remain byte-identical.
fn source_artifacts(beads: &Path, db: &Path) -> Inventory {
    let mut files = database_family(db);
    for name in [
        "issues.jsonl",
        "issues.history.jsonl",
        "config.yaml",
        "metadata.json",
        ".gitignore",
        ".br_recovery",
    ] {
        files.extend(inventory(&beads.join(name)));
    }
    files.extend(inventory(&db.parent().unwrap().join(".br_recovery")));
    files
}

fn query_copy(db: &Path, sql: &str) -> Vec<Vec<SqliteValue>> {
    let scratch = tempfile::tempdir().unwrap();
    let copy = scratch.path().join("copy.db");
    copy_family(db, &copy);
    let connection = Connection::open(copy.to_string_lossy().into_owned()).unwrap();
    let rows = connection
        .query(sql)
        .unwrap()
        .iter()
        .map(|row| row.values().to_vec())
        .collect();
    connection.close().unwrap();
    rows
}

fn copy_family(source: &Path, destination: &Path) {
    fs::copy(source, destination).unwrap();
    for suffix in ["-wal", "-journal"] {
        let member = PathBuf::from(format!("{}{suffix}", source.display()));
        if member.exists() {
            fs::copy(
                member,
                PathBuf::from(format!("{}{suffix}", destination.display())),
            )
            .unwrap();
        }
    }
}

fn edit(db: &Path, statements: &[&str]) {
    let connection = Connection::open(db.to_string_lossy().into_owned()).unwrap();
    for statement in statements {
        connection.execute(statement).unwrap();
    }
    connection.close().unwrap();
}

fn header_version(db: &Path) -> u32 {
    u32::from_be_bytes(fs::read(db).unwrap()[60..64].try_into().unwrap())
}

fn current_version() -> u32 {
    u32::try_from(beads_rust::storage::schema::CURRENT_SCHEMA_VERSION).unwrap()
}

fn seed_durable_records(db: &Path, owner: &str) {
    edit(
        db,
        &[
            &format!(
                "INSERT INTO events (id, issue_id, event_type, actor, new_value, comment) VALUES (811, '{owner}', 'updated', 'original-operator', 'preserved value', 'database-only history')"
            ),
            "INSERT INTO config (rowid, key, value) VALUES (1001, 'compact_model', 'local-model')",
            "INSERT INTO metadata (rowid, key, value) VALUES (9001, 'operator_receipt', X'0001FF00')",
            &format!(
                "INSERT INTO child_counters (rowid, parent_id, last_child) VALUES (401, '{owner}', 47)"
            ),
            "CREATE TABLE operator_notes (id INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT NOT NULL, payload BLOB NOT NULL)",
            "INSERT INTO operator_notes VALUES (17, 'original operator evidence', X'0001FF00')",
            "UPDATE sqlite_sequence SET seq = 91 WHERE name = 'operator_notes'",
        ],
    );
}

fn install_legacy(version: u32) -> (BrWorkspace, PathBuf) {
    let workspace = BrWorkspace::new();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema_migration");
    let beads = workspace.root.join(".beads");
    fs::create_dir_all(&beads).unwrap();
    let (fixture, jsonl, prefix, owner) = if version == 4 {
        (
            "schema4_v0127_db_only.db.gz",
            "schema4_v0127.issues.jsonl",
            "leg",
            "leg-n57",
        )
    } else {
        assert_eq!(version, 13);
        (
            "schema16_v0219_release.db.gz",
            "schema16_issues.jsonl",
            "v15ws",
            "v15ws-9nx",
        )
    };
    let mut bytes = Vec::new();
    GzDecoder::new(fs::File::open(fixtures.join(fixture)).unwrap())
        .read_to_end(&mut bytes)
        .unwrap();
    let db = beads.join("beads.db");
    fs::write(&db, bytes).unwrap();
    fs::copy(fixtures.join(jsonl), beads.join("issues.jsonl")).unwrap();
    fs::write(
        beads.join("config.yaml"),
        format!("issue_prefix: {prefix}\n# retained initialization configuration\n"),
    )
    .unwrap();
    fs::write(
        beads.join("metadata.json"),
        "{\"database\":\"beads.db\",\"jsonl_export\":\"issues.jsonl\"}\n",
    )
    .unwrap();
    fs::write(
        beads.join(".gitignore"),
        "# retained operator ignore rules\n*.db\n",
    )
    .unwrap();
    if version == 13 {
        // Synthetic historical shape, not a claimed released schema-13 file.
        edit(
            &db,
            &[
                "DROP TABLE gate_result_history",
                "DROP TABLE capacity_exemption_history",
                "DROP TABLE capacity_exemptions",
                "PRAGMA user_version = 13",
                "UPDATE issues SET content_hash = 'legacy-hash-' || id",
                "INSERT INTO dirty_issues (rowid, issue_id, marked_at) VALUES (311, 'v15ws-9nx', '2021-02-03T04:05:06Z')",
            ],
        );
    }
    seed_durable_records(&db, owner);
    let recovery = beads.join(".br_recovery/previous-operator-run");
    fs::create_dir_all(&recovery).unwrap();
    fs::write(recovery.join("evidence.bin"), b"original recovery evidence").unwrap();
    assert_eq!(header_version(&db), version);
    (workspace, db)
}

fn move_to_nested(workspace: &mut BrWorkspace) {
    workspace.root = workspace.root.join("project/nested");
    fs::create_dir_all(&workspace.root).unwrap();
}

fn assert_init_refused(result: &BrRun, source_version: Option<u32>) {
    assert!(
        !result.status.success(),
        "{} {}",
        result.stdout,
        result.stderr
    );
    let error = payload(&result.stdout);
    let message = error["error"]["message"]
        .as_str()
        .expect("actionable init refusal");
    for detail in [
        "init",
        "beads.db",
        "br doctor --json",
        "br doctor migrate-schema plan",
    ] {
        assert!(message.contains(detail), "missing {detail}: {message}");
    }
    if let Some(version) = source_version {
        assert!(
            message.contains(&version.to_string()),
            "missing effective schema version: {message}"
        );
    }
    assert!(error.get("initialized").is_none());
}

#[test]
fn init_force_with_malformed_metadata_preserves_legacy_authority() {
    for version in [4, 13] {
        let (mut workspace, db) = install_legacy(version);
        let beads = workspace.root.join(".beads");
        fs::write(
            beads.join("metadata.json"),
            b"{ malformed original metadata\n",
        )
        .unwrap();
        let before = inventory(&beads);
        for nested in [false, true] {
            let mut environment = Vec::new();
            if nested {
                move_to_nested(&mut workspace);
                environment.push(("BEADS_DIR", beads.to_string_lossy().into_owned()));
            }
            let init = run_br_with_env(
                &workspace,
                ["init", "--force", "--prefix", "must-not-replace", "--json"],
                environment,
                &format!("init_malformed_metadata_{version}_{nested}"),
            );
            assert_init_refused(&init, Some(version));
            assert_eq!(
                inventory(&beads),
                before,
                "refused init changed legacy tracker files"
            );
            assert_eq!(header_version(&db), version);
            assert!(
                !workspace.root.join(".beads").exists() || !nested,
                "nested init created a different tracker despite BEADS_DIR"
            );
        }
    }
}

fn initialized_current_tracker() -> (BrWorkspace, PathBuf) {
    let workspace = BrWorkspace::new();
    let init = run_br(
        &workspace,
        ["init", "--prefix", "init", "--json"],
        "init_fresh_control",
    );
    assert!(init.status.success(), "{} {}", init.stdout, init.stderr);
    let receipt = payload(&init.stdout);
    assert_eq!(receipt["initialized"], true);
    assert_eq!(receipt["files"]["database"], "created");
    let db = workspace.root.join(".beads/beads.db");
    assert_eq!(header_version(&db), current_version());
    assert_eq!(
        Path::new(receipt["database_path"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        db.canonicalize().unwrap()
    );
    (workspace, db)
}

#[test]
fn init_checks_its_default_or_cache_target_despite_a_current_global_db_route() {
    for cache_target in [false, true] {
        let (mut workspace, default_db) = install_legacy(13);
        let beads = workspace.root.join(".beads");
        let (_alternate_workspace, current_db) = initialized_current_tracker();
        let alternate = beads.join("current-alternate.db");
        copy_family(&current_db, &alternate);
        let mut environment = vec![("BEADS_DIR", beads.to_string_lossy().into_owned())];
        let db = if cache_target {
            let cache = workspace.root.join("tracker-cache");
            fs::create_dir_all(&cache).unwrap();
            let cached = cache.join("beads.db");
            copy_family(&default_db, &cached);
            environment.push(("BEADS_CACHE_DIR", cache.to_string_lossy().into_owned()));
            cached
        } else {
            default_db
        };
        let before = source_artifacts(&beads, &db);
        let alternate_bytes = fs::read(&alternate).unwrap();
        for nested in [false, true] {
            if nested {
                move_to_nested(&mut workspace);
            }
            let init = run_br_with_env(
                &workspace,
                [
                    "--db",
                    alternate.to_str().unwrap(),
                    "init",
                    "--force",
                    "--prefix",
                    "must-not-replace",
                    "--json",
                ],
                environment.clone(),
                &format!("init_alternate_route_{cache_target}_{nested}"),
            );
            assert_init_refused(&init, None);
            assert_eq!(source_artifacts(&beads, &db), before);
            assert_eq!(fs::read(&alternate).unwrap(), alternate_bytes);
            assert_eq!(header_version(&db), 13);
            if nested {
                assert!(!workspace.root.join(".beads").exists());
            }
        }
    }
}

#[test]
fn init_uses_the_committed_wal_version_before_any_writable_open() {
    let (mut workspace, db) = initialized_current_tracker();
    let beads = workspace.root.join(".beads");
    let created = run_br(
        &workspace,
        ["create", "WAL version authority", "--json"],
        "init_wal_issue",
    );
    assert!(
        created.status.success(),
        "{} {}",
        created.stdout,
        created.stderr
    );
    let owner = payload(&created.stdout)["id"].as_str().unwrap().to_owned();
    seed_durable_records(&db, &owner);
    edit(&db, &["PRAGMA wal_checkpoint(TRUNCATE)"]);
    assert_eq!(header_version(&db), current_version());
    let mut writer = Connection::open(db.to_string_lossy().into_owned()).unwrap();
    writer.execute("PRAGMA journal_mode = WAL").unwrap();
    writer.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
    writer.execute("BEGIN IMMEDIATE").unwrap();
    // This is a version-precedence control on a current runtime shape. A
    // header-only check would admit it, while committed WAL declares 13.
    writer.execute("PRAGMA user_version = 13").unwrap();
    writer.execute(&format!("INSERT INTO events (issue_id, event_type, actor, comment) VALUES ('{owner}', 'updated', 'wal-operator', 'committed WAL-only history')")).unwrap();
    writer.execute("COMMIT").unwrap();
    writer.close_without_checkpoint_in_place().unwrap();
    drop(writer);
    assert_eq!(header_version(&db), current_version());
    let wal_only_marker = b"committed WAL-only history";
    assert!(
        !fs::read(&db)
            .unwrap()
            .windows(wal_only_marker.len())
            .any(|window| window == wal_only_marker),
        "fixture must keep the new history out of the main database"
    );
    assert_eq!(
        query_copy(&db, "PRAGMA user_version"),
        vec![vec![SqliteValue::Integer(13)]]
    );
    assert!(
        fs::metadata(PathBuf::from(format!("{}-wal", db.display())))
            .unwrap()
            .len()
            > 32
    );
    fs::write(
        beads.join("metadata.json"),
        b"{ malformed metadata keeps startup from selecting a different target\n",
    )
    .unwrap();
    let before = inventory(&beads);
    move_to_nested(&mut workspace);
    let init = run_br_with_env(
        &workspace,
        ["init", "--force", "--prefix", "must-not-replace", "--json"],
        [("BEADS_DIR", beads.to_string_lossy().into_owned())],
        "init_wal_effective_version",
    );
    assert_init_refused(&init, Some(13));
    assert_eq!(
        inventory(&beads),
        before,
        "refusal changed committed WAL authority"
    );
    assert_eq!(header_version(&db), current_version());
    assert_eq!(
        query_copy(
            &db,
            "SELECT COUNT(*) FROM events WHERE comment = 'committed WAL-only history'"
        ),
        vec![vec![SqliteValue::Integer(1)]]
    );
}

fn records_except_prefix(db: &Path) -> RawRows {
    let mut records = BTreeMap::new();
    records.insert(
        "schema".to_owned(),
        query_copy(
            db,
            "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name",
        ),
    );
    for row in query_copy(
        db,
        "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
    ) {
        let table = row[0].as_text().unwrap();
        let quoted = table.replace('"', "\"\"");
        let filter = if table == "config" {
            " WHERE key != 'issue_prefix'"
        } else {
            ""
        };
        records.insert(
            table.to_owned(),
            query_copy(
                db,
                &format!("SELECT rowid, * FROM \"{quoted}\"{filter} ORDER BY rowid"),
            ),
        );
    }
    records
}

#[test]
fn fresh_and_current_force_init_succeed_without_losing_durable_records() {
    let (mut workspace, db) = initialized_current_tracker();
    let beads = workspace.root.join(".beads");
    let created = run_br(
        &workspace,
        ["create", "Existing Init control", "--json"],
        "init_current_issue",
    );
    assert!(
        created.status.success(),
        "{} {}",
        created.stdout,
        created.stderr
    );
    let owner = payload(&created.stdout)["id"].as_str().unwrap().to_owned();
    seed_durable_records(&db, &owner);
    let before = records_except_prefix(&db);
    let retained: BTreeMap<_, _> = ["config.yaml", ".gitignore", "issues.jsonl"]
        .into_iter()
        .map(|name| (name, fs::read(beads.join(name)).unwrap()))
        .collect();
    let recovery = inventory(&beads.join(".br_recovery"));
    move_to_nested(&mut workspace);
    let init = run_br_with_env(
        &workspace,
        ["init", "--force", "--prefix", "REINITIALIZED", "--json"],
        [("BEADS_DIR", beads.to_string_lossy().into_owned())],
        "init_current_force_control",
    );
    assert!(init.status.success(), "{} {}", init.stdout, init.stderr);
    let receipt = payload(&init.stdout);
    assert_eq!(receipt["initialized"], true);
    assert_eq!(receipt["prefix"], "reinitialized");
    assert_eq!(receipt["files"]["database"], "existing");
    assert_eq!(records_except_prefix(&db), before);
    assert_eq!(
        query_copy(&db, "SELECT value FROM config WHERE key = 'issue_prefix'"),
        vec![vec![SqliteValue::from("reinitialized")]]
    );
    for (name, bytes) in retained {
        assert_eq!(fs::read(beads.join(name)).unwrap(), bytes, "changed {name}");
    }
    assert_eq!(inventory(&beads.join(".br_recovery")), recovery);
    let metadata: Value =
        serde_json::from_slice(&fs::read(beads.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(metadata["database"], "beads.db");
    assert_eq!(metadata["jsonl_export"], "issues.jsonl");
    assert_eq!(header_version(&db), current_version());
    assert!(!workspace.root.join(".beads").exists());
}
