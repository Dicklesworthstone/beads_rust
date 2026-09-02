//! beads_rust-avhq: engine sidecars left behind without their database file
//! (`beads.db-wal-cert`, `beads.db-fsqlite-ns-gate`, `beads.db-wal`, ...)
//! must not wedge `br`. Both the writable recovery path (a mutation
//! re-installs a fresh database from `issues.jsonl`) and `br init` move the
//! orphans into `.beads/.br_recovery/` first, keeping their bytes for
//! inspection, and then proceed normally.
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
        let backup = quarantined
            .iter()
            .find(|file| {
                file.starts_with(&format!("{name}.")) && file.ends_with(".orphaned-sidecar")
            })
            .unwrap_or_else(|| panic!("{name} missing from {recovery:?}: {quarantined:?}"));
        assert_eq!(
            fs::read(recovery.join(backup)).expect("read quarantined sidecar"),
            bytes,
            "{name} bytes must survive quarantine"
        );
    }
}

#[test]
fn mutation_reinstalls_database_when_only_orphaned_sidecars_remain() {
    let _log = common::test_log("mutation_reinstalls_database_when_only_orphaned_sidecars_remain");
    let temp = TempDir::new_in(common::cli::isolated_temp_root()).expect("tempdir");
    let root = temp.path();
    let init = br(root, &["init"]);
    assert!(init.status.success(), "{}", rendered(&init));
    let created = br(root, &["create", "survives the lost database", "--json"]);
    assert!(created.status.success(), "{}", rendered(&created));
    let beads_dir = root.join(".beads");

    // Lose the database file and its legitimate sidecars, then plant stale
    // engine sidecars as a crashed engine or a partial restore would leave.
    for entry in fs::read_dir(&beads_dir)
        .expect("list .beads")
        .filter_map(Result::ok)
    {
        if entry.file_name().to_string_lossy().starts_with("beads.db") {
            fs::remove_file(entry.path()).expect("remove database family member");
        }
    }
    plant_orphans(&beads_dir);

    let recovered = br(root, &["create", "written after recovery", "--json"]);
    assert!(
        recovered.status.success(),
        "a mutation should install a fresh database instead of wedging on the orphans:\n{}",
        rendered(&recovered)
    );
    assert!(
        beads_dir.join("beads.db").is_file(),
        "fresh database should have been installed"
    );
    assert_orphans_quarantined(&beads_dir);

    let list = br(root, &["list", "--json"]);
    assert!(list.status.success(), "{}", rendered(&list));
    let listed = String::from_utf8_lossy(&list.stdout);
    assert!(
        listed.contains("survives the lost database") && listed.contains("written after recovery"),
        "recovered database should hold the JSONL issue and the new one:\n{listed}"
    );
}

#[test]
fn init_quarantines_orphaned_sidecars_before_creating_the_database() {
    let _log = common::test_log("init_quarantines_orphaned_sidecars_before_creating_the_database");
    let temp = TempDir::new_in(common::cli::isolated_temp_root()).expect("tempdir");
    let root = temp.path();
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
