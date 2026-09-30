//! GH #521: upgrading br 0.6.0 to 0.7.0 wedged every workspace with
//! "database is busy (recovery in progress)".
//!
//! br 0.6.0's engine kept its WAL index in process memory and never wrote the
//! `-shm` header, so a settled 0.6.0 family is a header-only WAL beside a
//! 32 KiB `-shm` whose header is all zeroes. A newer engine reads that file,
//! finds an index it must rebuild, and a read-only open cannot rebuild it.
//! Running 0.6.0 again after a newer br leaves the same kind of index: one
//! whose salts describe a WAL generation 0.6.0 has since restarted.
//!
//! Ordinary commands must rebuild such an index (main and WAL untouched,
//! pre-state retained) and keep working; explicit read-only invocations must
//! read without writing the live family.

// Automatic index recovery runs only where the engine maps `-shm` and the
// quarantine lock exists (STARTUP_WAL_INDEX_RECOVERY).
#![cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_env};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

const KEPT_TITLE: &str = "created before the upgrade";

fn beads_file(workspace: &BrWorkspace, name: &str) -> PathBuf {
    workspace.root.join(".beads").join(name)
}

fn migration_run_count(workspace: &BrWorkspace) -> usize {
    fs::read_dir(beads_file(workspace, ".br_recovery/schema-migrations"))
        .map_or(0, |entries| entries.filter_map(Result::ok).count())
}

fn settled_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "lw"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create = run_br(&workspace, ["create", KEPT_TITLE], "create_kept");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let wal = fs::read(beads_file(&workspace, "beads.db-wal")).expect("settled WAL");
    assert_eq!(wal.len(), 32, "fixture precondition: header-only WAL");
    workspace
}

/// The exact `-shm` a settled br 0.6.0 family carries: 32 KiB, a WAL-index
/// header that was never written, read marks 2..4 unused.
fn install_legacy_index(workspace: &BrWorkspace) {
    let mut legacy = vec![0_u8; 32 * 1024];
    legacy[108..120].fill(0xFF);
    fs::write(beads_file(workspace, "beads.db-shm"), legacy).expect("legacy -shm");
}

/// An initialized index whose salts belong to an earlier WAL generation.
fn install_other_generation_index(workspace: &BrWorkspace) {
    let path = beads_file(workspace, "beads.db-shm");
    let mut shm = fs::read(&path).expect("live -shm");
    assert_eq!(shm[12], 1, "fixture precondition: an initialized index");
    shm[32] ^= 0x01;
    shm[80] ^= 0x01;
    fs::write(&path, shm).expect("rewrite -shm");
}

fn listed_titles(workspace: &BrWorkspace, args: &[&str], label: &str) -> Vec<String> {
    let list = run_br(workspace, args.iter().copied(), label);
    assert!(
        list.status.success(),
        "{label}: list must succeed: stdout={} stderr={}",
        list.stdout,
        list.stderr
    );
    let json: Value = serde_json::from_str(&extract_json_payload(&list.stdout)).expect("list JSON");
    json["issues"]
        .as_array()
        .expect("issues array")
        .iter()
        .map(|issue| issue["title"].as_str().expect("title").to_owned())
        .collect()
}

fn assert_recovers(workspace: &BrWorkspace, label: &str) {
    let main_before = fs::read(beads_file(workspace, "beads.db")).expect("main");
    let wal_before = fs::read(beads_file(workspace, "beads.db-wal")).expect("wal");

    let ready = run_br(workspace, ["ready"], &format!("{label}_ready"));
    assert!(
        ready.status.success(),
        "{label}: ready must succeed after the upgrade: stdout={} stderr={}",
        ready.stdout,
        ready.stderr
    );
    assert!(
        !format!("{}{}", ready.stdout, ready.stderr).contains("recovery in progress"),
        "{label}: {}",
        ready.stderr
    );
    assert!(
        ready.stdout.contains(KEPT_TITLE),
        "{label}: {}",
        ready.stdout
    );
    assert_eq!(
        migration_run_count(workspace),
        1,
        "{label}: exactly one retained pre-recovery family"
    );
    let retained = fs::read_dir(beads_file(workspace, ".br_recovery/schema-migrations"))
        .expect("runs")
        .next()
        .expect("one run")
        .expect("run entry")
        .path()
        .join("recovery-before");
    assert_eq!(
        fs::read(retained.join("beads.db")).expect("retained main"),
        main_before,
        "{label}: the pre-state is kept byte-for-byte"
    );
    assert_eq!(
        fs::read(retained.join("beads.db-wal")).expect("retained WAL"),
        wal_before,
        "{label}: the pre-state WAL is kept byte-for-byte"
    );

    // Rebuilt once: later commands neither fail nor copy the family again.
    let create = run_br(
        workspace,
        ["create", "written after the upgrade"],
        &format!("{label}_create"),
    );
    assert!(create.status.success(), "{label}: {}", create.stderr);
    let titles = listed_titles(workspace, &["list", "--all", "--json"], label);
    assert!(titles.iter().any(|title| title == KEPT_TITLE), "{titles:?}");
    assert!(
        titles
            .iter()
            .any(|title| title == "written after the upgrade"),
        "{titles:?}"
    );
    assert_eq!(
        migration_run_count(workspace),
        1,
        "{label}: a rebuilt index must not trigger recovery again"
    );
}

#[test]
fn legacy_index_from_br_060_is_rebuilt_by_ordinary_commands() {
    let workspace = settled_workspace();
    install_legacy_index(&workspace);
    assert_recovers(&workspace, "legacy_060");
}

#[test]
fn index_from_another_wal_generation_is_rebuilt_by_ordinary_commands() {
    let workspace = settled_workspace();
    install_other_generation_index(&workspace);
    assert_recovers(&workspace, "other_generation");
}

#[test]
fn explicit_read_only_reads_the_legacy_family_without_writing_it() {
    let workspace = settled_workspace();
    install_legacy_index(&workspace);
    let family = ["beads.db", "beads.db-wal", "beads.db-shm"]
        .map(|name| fs::read(beads_file(&workspace, name)).expect("family member"));

    let titles = listed_titles(
        &workspace,
        &[
            "--no-auto-import",
            "--no-auto-flush",
            "list",
            "--all",
            "--json",
        ],
        "read_only",
    );
    assert!(titles.iter().any(|title| title == KEPT_TITLE), "{titles:?}");
    assert_eq!(
        ["beads.db", "beads.db-wal", "beads.db-shm"]
            .map(|name| fs::read(beads_file(&workspace, name)).expect("family member")),
        family,
        "an explicit read-only command must not rewrite the live family"
    );
    assert_eq!(migration_run_count(&workspace), 0);
}

/// The exact `-shm` stock SQLite leaves when it is the first connection to a
/// family whose WAL holds no frames, as any SQLite reader of the tracker (bv,
/// the sqlite3 shell) is once br has exited: it rebuilds the index and, with no
/// frame to take a page size from, leaves `szPage`, the frame count and the
/// salts zero under a valid native-order header checksum (#507's shape).
fn install_stock_sqlite_index(workspace: &BrWorkspace) -> Vec<u8> {
    let mut header = [0_u8; 48];
    header[..4].copy_from_slice(&3_007_000_u32.to_ne_bytes());
    header[12] = 1;
    header[13] = u8::from(cfg!(target_endian = "big"));
    let (mut first, mut second) = (0_u32, 0_u32);
    for pair in header[..40].as_chunks::<8>().0 {
        let word = |bytes: &[u8]| u32::from_ne_bytes(bytes.try_into().expect("checksum word"));
        first = first.wrapping_add(word(&pair[..4])).wrapping_add(second);
        second = second.wrapping_add(word(&pair[4..])).wrapping_add(first);
    }
    header[40..44].copy_from_slice(&first.to_ne_bytes());
    header[44..48].copy_from_slice(&second.to_ne_bytes());
    let mut shm = vec![0_u8; 32 * 1024];
    shm[..48].copy_from_slice(&header);
    shm[48..96].copy_from_slice(&header);
    shm[104..120].fill(0xFF); // aReadMark[1..=4] unused
    fs::write(beads_file(workspace, "beads.db-shm"), &shm).expect("stock SQLite -shm");
    shm
}

fn family_bytes(workspace: &BrWorkspace) -> [Vec<u8>; 3] {
    ["beads.db", "beads.db-wal", "beads.db-shm"]
        .map(|name| fs::read(beads_file(workspace, name)).expect("family member"))
}

/// bv reads the tracker with SQLite and then hands agents
/// `br --db <path> --no-auto-import --no-auto-flush show --json -- <id>`.
/// With br 0.7.0 through 0.7.2 every such read-only command failed with
/// "database is busy (recovery in progress)" until some writable command ran.
#[test]
fn read_only_commands_work_after_a_stock_sqlite_reader() {
    let workspace = settled_workspace();
    let listed = run_br(
        &workspace,
        ["--no-auto-import", "--no-auto-flush", "list", "--json"],
        "healthy_list",
    );
    assert!(listed.status.success(), "{}", listed.stderr);
    let json: Value = serde_json::from_str(&extract_json_payload(&listed.stdout)).expect("JSON");
    let id = json["issues"][0]["id"]
        .as_str()
        .expect("issue id")
        .to_owned();
    let installed = install_stock_sqlite_index(&workspace);
    let family = family_bytes(&workspace);
    assert_eq!(family[1].len(), 32, "fixture precondition: header-only WAL");
    assert_eq!(family[2], installed);

    let db = beads_file(&workspace, "beads.db");
    let absolute = db.to_str().expect("utf-8 path");
    let invocations: [&[&str]; 5] = [
        &[
            "--db",
            absolute,
            "--no-auto-import",
            "--no-auto-flush",
            "show",
            "--json",
            "--",
            &id,
        ],
        &[
            "--db",
            ".beads/beads.db",
            "--no-auto-import",
            "--no-auto-flush",
            "show",
            &id,
            "--json",
        ],
        &[
            "--no-auto-import",
            "--no-auto-flush",
            "list",
            "--all",
            "--json",
        ],
        &["--no-auto-import", "--no-auto-flush", "ready", "--json"],
        &["--no-auto-import", "--no-auto-flush", "doctor", "--json"],
    ];
    for (index, args) in invocations.iter().enumerate() {
        let label = format!("stock_sqlite_read_only_{index}");
        // Doctor reports a caller's verbose RUST_LOG as a finding of its own,
        // so only the data commands carry the snapshot probe.
        let run = if index == 4 {
            run_br(&workspace, args.iter().copied(), &label)
        } else {
            run_br_with_env(
                &workspace,
                args.iter().copied(),
                [("RUST_LOG", SNAPSHOT_PROBE_LOG)],
                &label,
            )
        };
        let output = format!("{}{}", run.stdout, run.stderr);
        assert!(run.status.success(), "{label} {args:?}: {output}");
        assert!(
            !output.contains("recovery in progress"),
            "{label}: {output}"
        );
        // FrankenSQLite 0.4.6+ admits stock SQLite's empty index (fsqlite
        // GH#431): the engine reads the live family, with no private copy.
        assert!(
            !output.contains(SNAPSHOT_MARKER),
            "{label}: the engine must read the family directly: {output}"
        );
        assert!(
            output.contains(KEPT_TITLE) || index == 4,
            "{label}: {output}"
        );
        assert_eq!(
            family_bytes(&workspace),
            family,
            "{label}: a read-only command must not rewrite the live family"
        );
    }
    assert_eq!(migration_run_count(&workspace), 0);
    assert_eq!(wal_index_quarantine_count(&workspace), 0);
}

/// `RUST_LOG` that surfaces only the private-snapshot event.
const SNAPSHOT_PROBE_LOG: &str = "error,br::read_snapshot=info";
/// The event a read-only open logs when it copies the family to a snapshot.
const SNAPSHOT_MARKER: &str = "reading a private snapshot";

fn wal_index_quarantine_count(workspace: &BrWorkspace) -> usize {
    fs::read_dir(workspace.root.join(".beads"))
        .expect(".beads")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".br-wal-index-")
        })
        .count()
}

/// An ordinary command still rebuilds the stock SQLite index in place, after
/// which reads take the engine's own path again. FrankenSQLite 0.4.7 reads
/// through that index but refuses the first commit through it, so writes keep
/// this index-only recovery.
#[test]
fn ordinary_command_rebuilds_a_stock_sqlite_index() {
    let workspace = settled_workspace();
    let installed = install_stock_sqlite_index(&workspace);
    let create = run_br(&workspace, ["create", "after the SQLite reader"], "create");
    assert!(
        create.status.success(),
        "{} {}",
        create.stdout,
        create.stderr
    );
    let rebuilt = fs::read(beads_file(&workspace, "beads.db-shm")).expect("rebuilt -shm");
    assert_ne!(rebuilt[..96], installed[..96], "the live index was rebuilt");

    // The WAL held no frames, so the index was all recovery had to rebuild:
    // the run retains that index and its receipts, not two copies of the
    // whole database (a backup plus a private rehearsal).
    assert_eq!(migration_run_count(&workspace), 1);
    let run = fs::read_dir(beads_file(&workspace, ".br_recovery/schema-migrations"))
        .expect("runs")
        .next()
        .expect("one run")
        .expect("run entry")
        .path();
    let mut retained: Vec<String> = fs::read_dir(run.join("recovery-before"))
        .expect("retained pre-state")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    retained.sort();
    assert_eq!(retained, ["beads.db-shm"]);
    assert_eq!(
        fs::read(run.join("recovery-before/beads.db-shm")).expect("retained index"),
        installed,
        "the retained index is the one the SQLite reader left"
    );
    assert!(!run.join("recovery-probe").exists());
    let receipt: Value = serde_json::from_slice(
        &fs::read(run.join("recovery-complete.json")).expect("completion receipt"),
    )
    .expect("receipt JSON");
    assert_eq!(receipt["backup_scope"], "wal-index-only");
    assert_eq!(receipt["stage"], "complete");
    assert_eq!(receipt["logical_after"]["integrity_check"], "ok");

    // Another stock SQLite read and write repeats the cheap path.
    let second = install_stock_sqlite_index(&workspace);
    let again = run_br(
        &workspace,
        ["create", "after a second read"],
        "create_again",
    );
    assert!(again.status.success(), "{} {}", again.stdout, again.stderr);
    assert_eq!(migration_run_count(&workspace), 2);
    assert_ne!(
        fs::read(beads_file(&workspace, "beads.db-shm")).expect("rebuilt -shm")[..96],
        second[..96]
    );
    let titles = listed_titles(
        &workspace,
        &[
            "--no-auto-import",
            "--no-auto-flush",
            "list",
            "--all",
            "--json",
        ],
        "after_rebuild",
    );
    assert!(titles.iter().any(|title| title == KEPT_TITLE), "{titles:?}");
    assert!(
        titles
            .iter()
            .any(|title| title == "after the SQLite reader"),
        "{titles:?}"
    );

    // Each rebuild set the live index aside once, and marked it finished so
    // retention can remove it once it is old enough.
    let quarantines: Vec<PathBuf> = fs::read_dir(workspace.root.join(".beads"))
        .expect(".beads")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".br-wal-index-"))
        })
        .collect();
    assert_eq!(quarantines.len(), 2, "{quarantines:?}");
    for quarantine in &quarantines {
        assert!(
            quarantine.join("quarantine-complete.json").is_file(),
            "{}",
            quarantine.display()
        );
    }
}

/// Index damage outside the shapes read-only commands read around (#507,
/// #521) still refuses reads with "database is busy (recovery in progress)",
/// and ordinary commands refuse on the same engine error. The error has to
/// name the command that rebuilds the index, and that command has to work.
#[test]
fn unrecognized_index_damage_names_and_accepts_explicit_recovery() {
    let workspace = settled_workspace();
    let shm_path = beads_file(&workspace, "beads.db-shm");
    let mut shm = fs::read(&shm_path).expect("live -shm");
    // Tear the index header: its two copies no longer agree.
    shm[16] ^= 0xFF;
    fs::write(&shm_path, &shm).expect("torn -shm");

    let read_only = run_br(
        &workspace,
        ["--no-auto-import", "--no-auto-flush", "list", "--json"],
        "torn_read_only",
    );
    assert!(!read_only.status.success(), "{}", read_only.stdout);
    let output = format!("{}{}", read_only.stdout, read_only.stderr);
    assert!(output.contains("recovery in progress"), "{output}");
    // Structured output: a warning object may precede the error object.
    let hint = [&read_only.stdout, &read_only.stderr]
        .into_iter()
        .flat_map(|stream| {
            serde_json::Deserializer::from_str(stream)
                .into_iter::<Value>()
                .map_while(Result::ok)
                .collect::<Vec<_>>()
        })
        .find_map(|value| value["error"]["hint"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("the error JSON carries a hint: {output}"));
    assert!(hint.contains("br doctor migrate-schema recover"), "{hint}");

    let create = run_br(
        &workspace,
        ["create", "blocked by the index"],
        "torn_create",
    );
    assert!(!create.status.success());
    let output = format!("{}{}", create.stdout, create.stderr);
    assert!(
        output.contains("br doctor migrate-schema recover"),
        "{output}"
    );

    let recover = run_br(
        &workspace,
        ["doctor", "migrate-schema", "recover", "--json"],
        "torn_recover",
    );
    assert!(
        recover.status.success(),
        "{} {}",
        recover.stdout,
        recover.stderr
    );
    let titles = listed_titles(
        &workspace,
        &[
            "--no-auto-import",
            "--no-auto-flush",
            "list",
            "--all",
            "--json",
        ],
        "torn_after_recover",
    );
    assert!(titles.iter().any(|title| title == KEPT_TITLE), "{titles:?}");
}
