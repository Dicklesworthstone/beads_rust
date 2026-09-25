//! Differential controls for the private-compaction failure in issue #508.
//!
//! A malformed live certificate, an absent certificate on a checkpointed
//! private copy, and a failed candidate build are different conditions. Keep
//! them separate: accepting arbitrary certificate corruption is not a fixture
//! repair. These tests do not relax the production recovery classifier.

use crate::config::{
    compact_database_via_vacuum_into_in_place, is_stale_wal_certificate_version_error,
};
use crate::error::BeadsError;
use crate::model::Issue;
use crate::storage::SqliteStorage;
use crate::sync::{DatabaseFamilyWriteLock, blocking_database_family_write_lock_with_timeout};
use chrono::Utc;
use fsqlite_error::FrankenError;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

const ISSUE_ID: &str = "bd-private-cert";
const WAL_ONLY_ISSUE_ID: &str = "bd-wal-only";
const MALFORMED_CERTIFICATE: &[u8] = b"live-sidecar-must-not-change";
const RECORD_BOUNDARY_ERROR: &str =
    "parallel WAL certificate suffix does not start at a record boundary";

struct Fixture {
    storage: SqliteStorage,
    authority: Arc<DatabaseFamilyWriteLock>,
    path: PathBuf,
    // Keep the directory until both the storage and authority have dropped.
    _directory: TempDir,
}

fn fixture() -> Fixture {
    let directory = TempDir::new().expect("fixture directory");
    let path = directory.path().join("beads.db");
    let mut storage = SqliteStorage::open(&path).expect("open fixture storage");
    storage
        .set_config("issue_prefix", "private-source")
        .expect("seed config");
    let now = Utc::now();
    storage
        .create_issue(
            &Issue {
                id: ISSUE_ID.to_string(),
                title: "Local issue that has never been exported".to_string(),
                description: Some(
                    "Preserve the issue, its audit event and dirty marker".to_string(),
                ),
                created_at: now,
                updated_at: now,
                ..Issue::default()
            },
            "compaction-test",
        )
        .expect("seed unflushed issue");
    let authority = Arc::new(
        blocking_database_family_write_lock_with_timeout(directory.path(), &path, Some(1_000))
            .expect("acquire database authority"),
    );
    authority
        .bind_database_inode_for_mutation()
        .expect("bind original database inode");
    storage.attach_write_authority(Arc::clone(&authority));
    storage
        .checkpoint_full()
        .expect("checkpoint healthy fixture");
    assert_eq!(storage.get_dirty_issue_count().expect("dirty count"), 1);
    Fixture {
        storage,
        authority,
        path,
        _directory: directory,
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PayloadWitness {
    device: u64,
    inode: u64,
    bytes: Vec<u8>,
}

fn payload_witness(path: &Path) -> Vec<(PathBuf, Option<PayloadWitness>)> {
    // SHM reader slots and namespace admission locks are derived coordination
    // state. Pin the durable payload and certificate evidence, including the
    // absence of a member, without confusing an admission change with data loss.
    [
        "",
        "-wal",
        "-wal-cert",
        "-wal-cert-head",
        ".fsqlite-migration-state",
    ]
    .into_iter()
    .map(|suffix| {
        let member = sidecar(path, suffix);
        let witness = match fs::symlink_metadata(&member) {
            Ok(metadata) => {
                assert!(
                    metadata.is_file(),
                    "non-file fixture member: {}",
                    member.display()
                );
                Some(PayloadWitness {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    bytes: fs::read(&member).expect("read fixture member"),
                })
            }
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => panic!(
                "cannot inspect fixture member {}: {error}",
                member.display()
            ),
        };
        (member, witness)
    })
    .collect()
}

/// A refused checkpoint can still reset the header of an empty WAL when its
/// storage handle drops. That header has no frames: bytes 12..32 are the
/// checkpoint sequence, salts and checksums. Require every other durable byte
/// and every member identity to stay exact, including a WAL with any frames.
fn assert_payload_preserved_after_checkpoint_refusal(
    before: &[(PathBuf, Option<PayloadWitness>)],
    after: &[(PathBuf, Option<PayloadWitness>)],
) {
    assert_eq!(before.len(), after.len());
    for (index, ((before_path, before_member), (after_path, after_member))) in
        before.iter().zip(after).enumerate()
    {
        assert_eq!(before_path, after_path);
        if index == 1 {
            assert!(before_path.to_string_lossy().ends_with("-wal"));
            if let (Some(before_wal), Some(after_wal)) = (before_member, after_member)
                && before_wal.bytes.len() == 32
                && after_wal.bytes.len() == 32
            {
                assert_eq!(before_wal.device, after_wal.device);
                assert_eq!(before_wal.inode, after_wal.inode);
                assert_eq!(&before_wal.bytes[..12], &after_wal.bytes[..12]);
                continue;
            }
        }
        assert_eq!(before_member, after_member, "{}", before_path.display());
    }
}

#[test]
fn checkpoint_refusal_witness_only_tolerates_an_empty_wal_header_reset() {
    let database = PathBuf::from("beads.db");
    let wal = sidecar(&database, "-wal");
    let before = vec![
        (
            database,
            Some(PayloadWitness {
                device: 1,
                inode: 2,
                bytes: vec![7],
            }),
        ),
        (
            wal,
            Some(PayloadWitness {
                device: 1,
                inode: 3,
                bytes: vec![0; 32],
            }),
        ),
    ];
    let mut after = before.clone();
    after[1].1.as_mut().unwrap().bytes[15] = 1;
    assert_payload_preserved_after_checkpoint_refusal(&before, &after);

    after[0].1.as_mut().unwrap().bytes[0] = 8;
    assert!(
        std::panic::catch_unwind(|| {
            assert_payload_preserved_after_checkpoint_refusal(&before, &after);
        })
        .is_err()
    );
    after[0] = before[0].clone();

    after[1].1.as_mut().unwrap().bytes.push(1);
    assert!(
        std::panic::catch_unwind(|| {
            assert_payload_preserved_after_checkpoint_refusal(&before, &after);
        })
        .is_err()
    );
}

fn logical_state(storage: &SqliteStorage) -> serde_json::Value {
    serde_json::to_value((
        storage
            .get_issue(ISSUE_ID)
            .expect("read issue")
            .expect("issue exists"),
        storage.get_events(ISSUE_ID, 0).expect("read audit events"),
        storage.get_dirty_issue_count().expect("read dirty count"),
        storage.get_config("issue_prefix").expect("read config"),
    ))
    .expect("serialize logical state")
}

fn wal_only_state(storage: &SqliteStorage) -> serde_json::Value {
    let events = storage
        .get_events(WAL_ONLY_ISSUE_ID, 0)
        .expect("read WAL-only audit events");
    assert!(
        !events.is_empty(),
        "the WAL-only issue must have an audit event"
    );
    serde_json::to_value((
        logical_state(storage),
        storage
            .get_issue(WAL_ONLY_ISSUE_ID)
            .expect("read WAL-only issue")
            .expect("committed WAL-only issue exists"),
        events,
    ))
    .expect("serialize WAL-only logical state")
}

/// Keep committed work out of both the main file and JSONL. A main-only copy
/// made without the live checkpoint must demonstrably lose this issue, its
/// event, its dirty marker, and the newer configuration value.
fn wal_only_fixture() -> Fixture {
    let mut fixture = fixture();
    fixture
        .storage
        .execute_raw("PRAGMA wal_autocheckpoint = 0")
        .expect("disable automatic checkpoints");
    let checkpointed_main = fs::read(&fixture.path).expect("read checkpointed main");
    let checkpointed_state = logical_state(&fixture.storage);
    let now = Utc::now();
    fixture
        .storage
        .create_issue(
            &Issue {
                id: WAL_ONLY_ISSUE_ID.to_string(),
                title: "Committed work absent from the main-only snapshot".to_string(),
                created_at: now,
                updated_at: now,
                ..Issue::default()
            },
            "wal-only-compaction-test",
        )
        .expect("commit the WAL-only issue");
    fixture
        .storage
        .set_config("issue_prefix", "wal-only")
        .expect("commit WAL-only configuration");
    assert_eq!(fixture.storage.get_dirty_issue_count().unwrap(), 2);
    assert_eq!(fs::read(&fixture.path).unwrap(), checkpointed_main);
    assert!(
        fs::metadata(sidecar(&fixture.path, "-wal")).unwrap().len() > 32,
        "the fixture must retain real WAL frames"
    );
    assert_ne!(logical_state(&fixture.storage), checkpointed_state);

    // Counterfactual control: a copy of just the current main really is stale.
    // Opening this independent copy must not checkpoint the live source.
    let before = payload_witness(&fixture.path);
    let private_directory = TempDir::new().expect("main-only control directory");
    let stale_path = private_directory.path().join("stale-main.db");
    fs::copy(&fixture.path, &stale_path).expect("copy uncheckpointed main only");
    let stale = SqliteStorage::open(&stale_path).expect("open main-only control");
    assert_eq!(logical_state(&stale), checkpointed_state);
    assert!(stale.get_issue(WAL_ONLY_ISSUE_ID).unwrap().is_none());
    assert!(stale.get_events(WAL_ONLY_ISSUE_ID, 0).unwrap().is_empty());
    drop(stale);
    assert_eq!(payload_witness(&fixture.path), before);
    assert_no_candidate_build(&fixture.path);
    assert_no_jsonl(&fixture.path);
    fixture
}

fn assert_no_jsonl(path: &Path) {
    assert_eq!(
        fs::symlink_metadata(path.with_file_name("issues.jsonl"))
            .expect_err("no JSONL may supply or replace the committed WAL-only state")
            .kind(),
        ErrorKind::NotFound
    );
}

fn assert_malformed_certificate(error: &BeadsError) {
    assert!(
        matches!(
            error,
            BeadsError::Database(FrankenError::WalCorrupt { detail })
                if detail.contains(RECORD_BOUNDARY_ERROR)
        ),
        "expected the malformed-certificate diagnostic, got: {error:?}"
    );
    assert!(
        !is_stale_wal_certificate_version_error(error),
        "record-boundary corruption must not authorize unsupported-version quarantine"
    );
}

fn assert_no_candidate_build(path: &Path) {
    let recovery = path.parent().expect("fixture parent").join(".br_recovery");
    assert_eq!(
        fs::symlink_metadata(&recovery)
            .expect_err("candidate construction must not create its recovery directory")
            .kind(),
        ErrorKind::NotFound
    );
}

fn run_isolated(name: &str) -> bool {
    // Match the existing compaction tests' sole-opener process isolation.
    // Otherwise parallel tests can fork while this fixture's lease is held.
    const CHILD_ENV: &str = "BR_TEST_ISOLATED_COMPACTION_CERTIFICATE";
    if std::env::var(CHILD_ENV).as_deref() == Ok(name) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, name)
        .output()
        .expect("run isolated compaction certificate test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains(&format!("test {name} ... ok")),
        "isolated test exited with {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[test]
fn checkpoint_rejects_malformed_live_certificate() {
    if run_isolated("compaction_certificate_tests::checkpoint_rejects_malformed_live_certificate") {
        return;
    }
    let mut fixture = fixture();
    fs::write(sidecar(&fixture.path, "-wal-cert"), MALFORMED_CERTIFICATE)
        .expect("plant the original malformed fixture");
    let before = payload_witness(&fixture.path);
    assert_no_candidate_build(&fixture.path);

    // No VACUUM, private copy or reopen is involved in this control.
    let error = fixture
        .storage
        .checkpoint_full()
        .expect_err("reject malformed certificate");
    assert_malformed_certificate(&error);
    assert_eq!(payload_witness(&fixture.path), before);
    fixture
        .authority
        .verify_database_authority()
        .expect("retain original authority");
    assert_no_candidate_build(&fixture.path);
}

#[test]
fn compaction_rejects_malformed_live_certificate_before_candidate_build() {
    if run_isolated(
        "compaction_certificate_tests::compaction_rejects_malformed_live_certificate_before_candidate_build",
    ) {
        return;
    }
    let fixture = fixture();
    fs::write(sidecar(&fixture.path, "-wal-cert"), MALFORMED_CERTIFICATE)
        .expect("plant the original malformed fixture");
    let before = payload_witness(&fixture.path);
    assert_no_candidate_build(&fixture.path);

    let error = compact_database_via_vacuum_into_in_place(fixture.storage, &fixture.path, Some(50))
        .expect_err("malformed live certificate must not produce compaction success");
    assert_malformed_certificate(&error);
    // This is after the consumed storage handle has also been dropped.
    assert_eq!(payload_witness(&fixture.path), before);
    fixture
        .authority
        .verify_database_authority()
        .expect("retain original authority");
    assert_no_candidate_build(&fixture.path);
}

#[test]
fn private_main_only_copy_does_not_inherit_malformed_live_certificate() {
    if run_isolated(
        "compaction_certificate_tests::private_main_only_copy_does_not_inherit_malformed_live_certificate",
    ) {
        return;
    }
    let fixture = fixture();
    let expected = logical_state(&fixture.storage);
    fs::write(sidecar(&fixture.path, "-wal-cert"), MALFORMED_CERTIFICATE)
        .expect("plant malformed live certificate after checkpoint");
    let before = payload_witness(&fixture.path);
    let private_directory = TempDir::new().expect("private source directory");
    let source = private_directory.path().join("source.db");
    let candidate = private_directory.path().join("candidate.db");
    fs::copy(&fixture.path, &source).expect("copy only the checkpointed main file");
    assert_eq!(
        fs::symlink_metadata(sidecar(&source, "-wal-cert"))
            .expect_err("private source starts without a certificate fixture")
            .kind(),
        ErrorKind::NotFound
    );

    let mut private = SqliteStorage::open(&source)
        .expect("checkpointed main-only private source must open without copied sidecars");
    // Production deliberately treats these two preliminary operations as
    // best-effort. Report their results, but keep the required checkpoint and
    // VACUUM INTO strict, just as the real candidate builder does.
    let vacuum = private.execute_raw("VACUUM");
    let reindex = private.execute_raw("REINDEX");
    eprintln!("private preliminary maintenance: VACUUM={vacuum:?}; REINDEX={reindex:?}");
    private
        .checkpoint_full()
        .expect("checkpoint private source");
    let escaped = candidate.display().to_string().replace('\'', "''");
    private
        .execute_raw(&format!("VACUUM INTO '{escaped}'"))
        .expect("build private candidate");
    drop(private);
    let candidate_storage = SqliteStorage::open(&candidate).expect("reopen private candidate");
    assert_eq!(logical_state(&candidate_storage), expected);
    drop(candidate_storage);

    assert_eq!(payload_witness(&fixture.path), before);
    fixture
        .authority
        .verify_database_authority()
        .expect("retain original authority");
    assert_no_candidate_build(&fixture.path);
}

#[test]
fn healthy_compaction_preserves_logical_state_without_jsonl() {
    if run_isolated(
        "compaction_certificate_tests::healthy_compaction_preserves_logical_state_without_jsonl",
    ) {
        return;
    }
    let fixture = fixture();
    let expected = logical_state(&fixture.storage);
    let original_inode = fs::metadata(&fixture.path)
        .expect("original metadata")
        .ino();
    let jsonl = fixture.path.with_file_name("issues.jsonl");
    assert_eq!(
        fs::symlink_metadata(&jsonl)
            .expect_err("no JSONL fixture")
            .kind(),
        ErrorKind::NotFound
    );

    // Do not synthesize certificate bytes: use the engine-created family.
    let compacted =
        compact_database_via_vacuum_into_in_place(fixture.storage, &fixture.path, Some(50))
            .expect("healthy source must build, install and reopen a real candidate");
    assert_eq!(logical_state(&compacted), expected);
    assert_ne!(
        fs::metadata(&fixture.path)
            .expect("compacted metadata")
            .ino(),
        original_inode,
        "success must install a candidate, not silently return the original storage"
    );
    fixture
        .authority
        .verify_database_authority()
        .expect("bind installed candidate authority");
    assert_eq!(
        fs::symlink_metadata(&jsonl)
            .expect_err("compaction must not create JSONL")
            .kind(),
        ErrorKind::NotFound
    );
}

/// Feed successful SQL result rows through the real storage checkpoint and
/// compaction entry point, rather than injecting a precomputed error. Reverting
/// execute() to discard those rows must make these controls fail.
fn checkpoint_row_refusal(status: [i64; 3]) -> BeadsError {
    let fixture = fixture();
    let expected = logical_state(&fixture.storage);
    let before = payload_witness(&fixture.path);
    assert_no_candidate_build(&fixture.path);
    let path = fixture.path.to_string_lossy().into_owned();

    let error = crate::franken_sync::checkpoint_fault::with_results(
        &path,
        vec![
            ("PRAGMA wal_checkpoint(TRUNCATE)", status),
            ("PRAGMA wal_checkpoint(PASSIVE)", status),
        ],
        || compact_database_via_vacuum_into_in_place(fixture.storage, &fixture.path, Some(50)),
    )
    .expect_err("an incomplete checkpoint must not authorize a main-only copy");

    // Both result rows must have been consumed, proving that the production
    // TRUNCATE-to-PASSIVE fallback, not an earlier fixture error, refused.
    assert_payload_preserved_after_checkpoint_refusal(&before, &payload_witness(&fixture.path));
    fixture
        .authority
        .verify_database_authority()
        .expect("checkpoint refusal must preserve original authority");
    assert_no_candidate_build(&fixture.path);
    let reopened = SqliteStorage::open_with_timeout_under_write_authority(
        &fixture.path,
        Some(50),
        &fixture.authority,
    )
    .expect("original database must remain reopenable");
    assert_eq!(logical_state(&reopened), expected);
    error
}

#[test]
fn compaction_rejects_incomplete_checkpoint_rows_before_candidate_build() {
    if run_isolated(
        "compaction_certificate_tests::compaction_rejects_incomplete_checkpoint_rows_before_candidate_build",
    ) {
        return;
    }
    for status in [[1, 23, 0], [0, 23, 7], [1, 23, 23]] {
        let error = checkpoint_row_refusal(status);
        assert!(
            matches!(error, BeadsError::Database(FrankenError::Busy)),
            "incomplete progress must remain contention, not corruption: {status:?}: {error}"
        );
    }
}

#[test]
fn compaction_rejects_invalid_checkpoint_values_before_candidate_build() {
    if run_isolated(
        "compaction_certificate_tests::compaction_rejects_invalid_checkpoint_values_before_candidate_build",
    ) {
        return;
    }
    for status in [[0, -1, 0], [2, 23, 0], [0, 1, 2]] {
        let error = checkpoint_row_refusal(status);
        assert!(
            matches!(error, BeadsError::Database(FrankenError::Internal(ref detail))
                if detail.contains("invalid completion values")),
            "malformed completion status must fail closed: {status:?}: {error}"
        );
    }
}

#[test]
fn compaction_partial_truncate_requires_a_real_completed_passive_fallback() {
    if run_isolated(
        "compaction_certificate_tests::compaction_partial_truncate_requires_a_real_completed_passive_fallback",
    ) {
        return;
    }
    let fixture = fixture();
    let expected = logical_state(&fixture.storage);
    let original_inode = fs::metadata(&fixture.path).unwrap().ino();
    let path = fixture.path.to_string_lossy().into_owned();
    // Fault only TRUNCATE. PASSIVE and all private-source maintenance must run
    // against the real engine and complete before installation can succeed.
    let compacted = crate::franken_sync::checkpoint_fault::with_results(
        &path,
        vec![("PRAGMA wal_checkpoint(TRUNCATE)", [1, 23, 0])],
        || compact_database_via_vacuum_into_in_place(fixture.storage, &fixture.path, Some(50)),
    )
    .expect("a genuinely completed PASSIVE fallback must permit compaction");
    assert_eq!(logical_state(&compacted), expected);
    assert_ne!(
        fs::metadata(&fixture.path).unwrap().ino(),
        original_inode,
        "successful fallback must install a candidate rather than skip maintenance"
    );
    fixture
        .authority
        .verify_database_authority()
        .expect("installed candidate must retain authority");
    assert_eq!(
        fs::symlink_metadata(fixture.path.with_file_name("issues.jsonl"))
            .expect_err("compaction must not require or create a JSONL fallback")
            .kind(),
        ErrorKind::NotFound
    );
}

fn compact_wal_only_state(passive_fallback: bool) {
    let fixture = wal_only_fixture();
    let expected = wal_only_state(&fixture.storage);
    let original_inode = fs::metadata(&fixture.path).unwrap().ino();
    let path = fixture.path.to_string_lossy().into_owned();
    let faults = if passive_fallback {
        vec![("PRAGMA wal_checkpoint(TRUNCATE)", [1, 23, 0])]
    } else {
        Vec::new()
    };
    // Only TRUNCATE may be faulted. Every successful checkpoint, private
    // source build, installation, and reopen runs against the actual engine.
    let compacted = crate::franken_sync::checkpoint_fault::with_results(&path, faults, || {
        compact_database_via_vacuum_into_in_place(fixture.storage, &fixture.path, Some(50))
    })
    .expect("compaction must include committed WAL-only work");
    assert_eq!(wal_only_state(&compacted), expected);
    assert_ne!(fs::metadata(&fixture.path).unwrap().ino(), original_inode);
    fixture.authority.verify_database_authority().unwrap();
    drop(compacted);

    // A returned handle's in-memory view alone is not durable success.
    let reopened = SqliteStorage::open_with_timeout_under_write_authority(
        &fixture.path,
        Some(50),
        &fixture.authority,
    )
    .expect("reopen installed candidate independently");
    assert_eq!(wal_only_state(&reopened), expected);
    assert_no_jsonl(&fixture.path);
}

#[test]
fn compaction_checkpoints_committed_wal_only_state_before_copying_main() {
    if run_isolated(
        "compaction_certificate_tests::compaction_checkpoints_committed_wal_only_state_before_copying_main",
    ) {
        return;
    }
    compact_wal_only_state(false);
}

#[test]
fn compaction_passive_fallback_checkpoints_committed_wal_only_state() {
    if run_isolated(
        "compaction_certificate_tests::compaction_passive_fallback_checkpoints_committed_wal_only_state",
    ) {
        return;
    }
    compact_wal_only_state(true);
}

#[test]
fn compaction_checkpoint_refusal_preserves_committed_wal_only_state() {
    if run_isolated(
        "compaction_certificate_tests::compaction_checkpoint_refusal_preserves_committed_wal_only_state",
    ) {
        return;
    }
    for status in [
        [1, 23, 0],
        [0, 23, 7],
        [1, 23, 23],
        [0, -1, 0],
        [2, 23, 0],
        [0, 1, 2],
    ] {
        let fixture = wal_only_fixture();
        let expected = wal_only_state(&fixture.storage);
        let original_main = fs::metadata(&fixture.path).expect("original main file");
        let path = fixture.path.to_string_lossy().into_owned();
        let error = crate::franken_sync::checkpoint_fault::with_results(
            &path,
            vec![
                ("PRAGMA wal_checkpoint(TRUNCATE)", status),
                ("PRAGMA wal_checkpoint(PASSIVE)", status),
            ],
            || compact_database_via_vacuum_into_in_place(fixture.storage, &fixture.path, Some(50)),
        )
        .expect_err("partial or invalid checkpoint cannot consume WAL-only state");
        // `clippy::unnested_or_patterns` wants these folded into something like
        // `[1, 23, 0 | 23] | [0, 23, 7]`. Keep them flat: each triple is one
        // named (busy, frames, backfilled) checkpoint status from the table
        // above, and nesting them stops the reader matching a row here against
        // a row there.
        #[allow(clippy::unnested_or_patterns)]
        let busy_status = matches!(status, [1, 23, 0] | [0, 23, 7] | [1, 23, 23]);
        if busy_status {
            assert!(matches!(error, BeadsError::Database(FrankenError::Busy)));
        } else {
            assert!(matches!(
                error,
                BeadsError::Database(FrankenError::Internal(ref detail))
                    if detail.contains("invalid completion values")
            ));
        }
        // The consumed storage handle can checkpoint committed WAL frames on
        // drop, legitimately changing main-file and WAL bytes after refusal.
        // It must not install a different database; the exact committed state
        // is checked again through the reopened storage below.
        let current_main = fs::metadata(&fixture.path).expect("live main file");
        assert_eq!(current_main.dev(), original_main.dev(), "status {status:?}");
        assert_eq!(current_main.ino(), original_main.ino(), "status {status:?}");
        fixture.authority.verify_database_authority().unwrap();
        assert_no_candidate_build(&fixture.path);
        assert_no_jsonl(&fixture.path);
        let reopened = SqliteStorage::open_with_timeout_under_write_authority(
            &fixture.path,
            Some(50),
            &fixture.authority,
        )
        .expect("refused compaction must leave WAL-only work recoverable");
        assert_eq!(wal_only_state(&reopened), expected);
    }
}

#[test]
fn compaction_certificate_refusal_preserves_committed_wal_only_state() {
    if run_isolated(
        "compaction_certificate_tests::compaction_certificate_refusal_preserves_committed_wal_only_state",
    ) {
        return;
    }
    let fixture = wal_only_fixture();
    assert!(
        fixture
            .storage
            .get_issue(WAL_ONLY_ISSUE_ID)
            .unwrap()
            .is_some()
    );
    fs::write(sidecar(&fixture.path, "-wal-cert"), MALFORMED_CERTIFICATE)
        .expect("corrupt certificate after committing WAL-only work");
    let before = payload_witness(&fixture.path);
    let error = compact_database_via_vacuum_into_in_place(fixture.storage, &fixture.path, Some(50))
        .expect_err("malformed certificate must not authorize a stale main-only replacement");
    assert_malformed_certificate(&error);
    assert_eq!(payload_witness(&fixture.path), before);
    fixture.authority.verify_database_authority().unwrap();
    assert_no_candidate_build(&fixture.path);
    assert_no_jsonl(&fixture.path);
}
