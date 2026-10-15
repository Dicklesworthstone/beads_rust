//! beads_rust-avhq: engine sidecars left behind without their database file
//! (`beads.db-wal-cert`, `beads.db-fsqlite-ns-gate`, `beads.db-wal`, ...)
//! retain their bytes. An ordinary mutation must refuse an orphan WAL that
//! holds frames because its unflushed state cannot be established without
//! the main database; a header-only WAL holds none, so the rebuild from
//! JSONL proceeds. Explicit `br init` still quarantines the orphans before
//! creating a fresh database.
mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const ORPHANS: [(&str, &[u8]); 3] = [
    ("beads.db-wal-cert", b"stale certificate"),
    ("beads.db-fsqlite-ns-gate", b"stale namespace gate"),
    (
        "beads.db-wal",
        b"stale wal frames that belong to a database that is gone",
    ),
];

fn br(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_br"))
        .current_dir(root)
        .args(args)
        .env("HOME", root)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "error")
        .output()
        .expect("run br")
}

fn rendered(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn plant_orphans(beads_dir: &Path) {
    for (name, bytes) in ORPHANS {
        fs::write(beads_dir.join(name), bytes).expect("write orphan sidecar");
    }
}

fn assert_orphans_quarantined(beads_dir: &Path) {
    let recovery = beads_dir.join(".br_recovery");
    let quarantined: Vec<String> = fs::read_dir(&recovery)
        .unwrap_or_else(|err| panic!("{} missing: {err}", recovery.display()))
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect();
    for (name, bytes) in ORPHANS {
        // The engine recreates its own sidecars (`-wal-cert`, namespace
        // files, `-wal`) for the fresh database, so the live name may exist
        // again; what must be gone is the stale content.
        if let Ok(live) = fs::read(beads_dir.join(name)) {
            assert_ne!(
                live, bytes,
                "{name} still carries the stale orphan bytes in the live family"
            );
        }
        // The stale bytes must be preserved under `.br_recovery/`, whether
        // br's own quarantine moved them (`<name>.<stamp>.orphaned-sidecar`)
        // or the JSONL rebuild's family backup did (`<name>.vacuum-*.bak`).
        let preserved = quarantined
            .iter()
            .filter(|file| file.starts_with(&format!("{name}.")))
            .any(|file| fs::read(recovery.join(file)).is_ok_and(|copy| copy == bytes));
        assert!(
            preserved,
            "{name} stale bytes missing from {}: {quarantined:?}",
            recovery.display()
        );
    }
}

fn assert_preservation_refusal(refused: &Output) {
    assert_eq!(refused.status.code(), Some(6), "{}", rendered(refused));
    let error: serde_json::Value =
        serde_json::from_slice(&refused.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout must be one JSON error: {error}; {}",
                rendered(refused)
            )
        });
    assert_eq!(error["error"]["code"], "SYNC_CONFLICT", "{error}");
    let message = error["error"]["message"]
        .as_str()
        .expect("preservation error message");
    for detail in [
        "Automatic database recovery refused",
        "unflushed",
        "beads.db-wal",
        ".br_recovery",
        "br doctor --repair",
    ] {
        assert!(message.contains(detail), "missing {detail:?}: {message}");
    }
}

#[test]
fn mutation_refuses_unreadable_orphaned_wal_before_reinstalling_database() {
    let _log =
        common::test_log("mutation_refuses_unreadable_orphaned_wal_before_reinstalling_database");
    let retained_root = TempDir::new_in(common::cli::isolated_temp_root())
        .expect("tempdir")
        .keep();
    let root = retained_root.as_path();
    let init = br(root, &["init"]);
    assert!(init.status.success(), "{}", rendered(&init));
    let created = br(root, &["create", "survives the lost database", "--json"]);
    assert!(created.status.success(), "{}", rendered(&created));
    let beads_dir = root.join(".beads");
    let jsonl = fs::read(beads_dir.join("issues.jsonl")).expect("original JSONL");
    let retained_family = root.join("retained-original-database");
    fs::create_dir(&retained_family).expect("retained original family directory");
    let mut original_family = Vec::new();

    // Retain the original database family outside the live namespace, then
    // plant the sidecars left by a partial restore. Their WAL bytes cannot
    // be classified as safely flushed when the main file is absent.
    for entry in fs::read_dir(&beads_dir).expect("list .beads") {
        let entry = entry.expect("original family entry");
        if entry.file_name().to_string_lossy().starts_with("beads.db") {
            let retained_path = retained_family.join(entry.file_name());
            let bytes = fs::read(entry.path()).expect("read original family member");
            fs::rename(entry.path(), &retained_path)
                .expect("retain original database family member");
            original_family.push((retained_path, bytes));
        }
    }
    plant_orphans(&beads_dir);
    assert!(!beads_dir.join("beads.db").exists(), "missing-main fixture");
    assert!(
        !beads_dir.join(".br_recovery").exists(),
        "new fixture should have no recovery files"
    );

    let refused = br(root, &["create", "must wait for explicit repair", "--json"]);
    fs::write(root.join("startup-refusal.log"), rendered(&refused)).expect("retain refusal output");
    assert_preservation_refusal(&refused);
    assert!(
        !beads_dir.join("beads.db").exists(),
        "refusal must not create a database"
    );
    assert!(
        !beads_dir.join(".br_recovery").exists(),
        "refusal must not quarantine the orphan family"
    );
    for (name, bytes) in ORPHANS {
        assert_eq!(
            fs::read(beads_dir.join(name)).expect("retained orphan"),
            bytes,
            "orphan changed: {name}"
        );
    }
    for (path, bytes) in original_family {
        assert_eq!(
            fs::read(&path).expect("retained original family"),
            bytes,
            "original fixture changed: {}",
            path.display()
        );
    }
    assert_eq!(
        fs::read(beads_dir.join("issues.jsonl")).expect("retained JSONL"),
        jsonl
    );
}

/// A WAL holding only its 32-byte header and no frames: the shape a WAL
/// keeps after a passive checkpoint (big-endian checksum variant).
fn header_only_wal() -> [u8; 32] {
    let fields: [u32; 6] = [0x377f_0683, 3_007_000, 4096, 0, 0x1234_5678, 0x9abc_def0];
    let (mut s0, mut s1) = (0_u32, 0_u32);
    for [first, second] in fields.as_chunks::<2>().0 {
        s0 = s0.wrapping_add(*first).wrapping_add(s1);
        s1 = s1.wrapping_add(*second).wrapping_add(s0);
    }
    let mut header = [0_u8; 32];
    for (word, value) in header
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(fields.into_iter().chain([s0, s1]))
    {
        *word = value.to_be_bytes();
    }
    header
}

#[test]
fn missing_database_with_header_only_wal_is_rebuilt_from_jsonl() {
    let _log = common::test_log("missing_database_with_header_only_wal_is_rebuilt_from_jsonl");
    let retained_root = TempDir::new_in(common::cli::isolated_temp_root())
        .expect("tempdir")
        .keep();
    let root = retained_root.as_path();
    let init = br(root, &["init"]);
    assert!(init.status.success(), "{}", rendered(&init));
    let created = br(root, &["create", "survives the lost database", "--json"]);
    assert!(created.status.success(), "{}", rendered(&created));
    let flushed = br(root, &["sync", "--flush-only"]);
    assert!(flushed.status.success(), "{}", rendered(&flushed));
    let beads_dir = root.join(".beads");
    let retained_family = root.join("retained-original-database");
    fs::create_dir(&retained_family).expect("retained original family directory");

    // Move the database family aside (as `mv .beads/beads.db ...` does) and
    // leave the frame-less WAL a checkpointed workspace keeps. No page can
    // live in it, so nothing needs preserving and the rebuild may proceed.
    for entry in fs::read_dir(&beads_dir).expect("list .beads") {
        let entry = entry.expect("original family entry");
        if entry.file_name().to_string_lossy().starts_with("beads.db") {
            fs::rename(entry.path(), retained_family.join(entry.file_name()))
                .expect("retain original database family member");
        }
    }
    let wal = header_only_wal();
    fs::write(beads_dir.join("beads.db-wal"), wal).expect("plant header-only wal");

    let listed = br(root, &["list", "--json"]);
    fs::write(root.join("list.log"), rendered(&listed)).expect("retain list output");
    assert!(listed.status.success(), "{}", rendered(&listed));
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("survives the lost database"),
        "rebuilt database lost the flushed issue: {}",
        rendered(&listed)
    );
    assert!(
        beads_dir.join("beads.db").is_file(),
        "the database was not rebuilt"
    );

    // The header bytes are quarantined, not discarded.
    let recovery = beads_dir.join(".br_recovery");
    let quarantined: Vec<String> = fs::read_dir(&recovery)
        .unwrap_or_else(|err| panic!("{} missing: {err}", recovery.display()))
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect();
    assert!(
        quarantined
            .iter()
            .any(|file| file.starts_with("beads.db-wal.")
                && fs::read(recovery.join(file)).is_ok_and(|copy| copy == wal)),
        "header-only wal bytes missing from {}: {quarantined:?}",
        recovery.display()
    );

    let created = br(root, &["create", "usable after the rebuild", "--json"]);
    assert!(created.status.success(), "{}", rendered(&created));
}

#[test]
fn init_quarantines_orphaned_sidecars_before_creating_the_database() {
    let _log = common::test_log("init_quarantines_orphaned_sidecars_before_creating_the_database");
    let retained_root = TempDir::new_in(common::cli::isolated_temp_root())
        .expect("tempdir")
        .keep();
    let root = retained_root.as_path();
    let beads_dir = root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");
    plant_orphans(&beads_dir);

    let init = br(root, &["init"]);
    assert!(
        init.status.success(),
        "init should quarantine the orphans and create the database:\n{}",
        rendered(&init)
    );
    assert!(
        beads_dir.join("beads.db").is_file(),
        "init created no database"
    );
    assert_orphans_quarantined(&beads_dir);

    let created = br(root, &["create", "usable after init", "--json"]);
    assert!(created.status.success(), "{}", rendered(&created));
}
