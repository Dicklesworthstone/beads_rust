//! Fresh clone (tracked `.beads/*` but no `beads.db`) whose first command is a
//! mutation (GitHub #487).
//!
//! Opening storage in such a workspace rebuilds the database from the JSONL and
//! *retains* the JSONL-family write authority in the [`OpenStorageResult`] for
//! the rest of the command. The post-mutation auto-flush used to ignore that
//! retained authority and acquire the same sidecar through a fresh descriptor —
//! and `flock` treats two descriptors of one file in one process as independent
//! holders, so the flush blocked on the lock its own process already held until
//! the 30 s default write-lock timeout expired. The mutation still exited 0 with
//! `.beads/issues.jsonl` missing the new record (silently, under `-q`).
//!
//! These tests assert the flush now completes promptly under the retained
//! authority, for the fresh-export shape as well as the incremental one.

mod common;

use common::cli::{BrWorkspace, run_br};
use std::fs;
use std::path::Path;
use std::time::Duration;

/// A generous bound: the real defect parked here for exactly 30 s, and an
/// honest flush of a two-record ledger is milliseconds. Anything under this
/// cannot be the write-lock timeout.
const FLUSH_TIMEOUT_HEADROOM: Duration = Duration::from_secs(15);

/// The files `git` actually tracks in a beads workspace. A real `git clone`
/// hands a teammate exactly these — notably *not* `beads.db`, which
/// `.beads/.gitignore` excludes.
const TRACKED_BEADS_FILES: &[&str] = &["issues.jsonl", "config.yaml", "metadata.json", ".gitignore"];

/// Build a donor workspace with `seed_titles` issues and return it.
fn donor_workspace(prefix: &str, seed_titles: &[&str]) -> BrWorkspace {
    let donor = BrWorkspace::new();
    let init = run_br(&donor, ["init", "--prefix", prefix, "-q"], "donor-init");
    assert!(
        init.status.success(),
        "donor init failed: {}",
        init.stderr.trim()
    );
    for (index, title) in seed_titles.iter().enumerate() {
        let created = run_br(
            &donor,
            ["create", title, "-t", "task", "-p", "2", "-q"],
            &format!("donor-create-{index}"),
        );
        assert!(
            created.status.success(),
            "donor create failed: {}",
            created.stderr.trim()
        );
    }
    donor
}

/// Copy only the tracked `.beads` files into a fresh workspace, the way a
/// `git clone` of the donor repository would.
fn clone_tracked_beads(donor: &BrWorkspace, clone: &BrWorkspace) {
    let src = donor.root.join(".beads");
    let dst = clone.root.join(".beads");
    fs::create_dir_all(&dst).expect("clone .beads");
    for name in TRACKED_BEADS_FILES {
        let from = src.join(name);
        if from.is_file() {
            fs::copy(&from, dst.join(name)).expect("copy tracked beads file");
        }
    }
    assert!(
        !dst.join("beads.db").exists(),
        "a fresh clone must not carry beads.db"
    );
    assert!(
        dst.join("issues.jsonl").is_file(),
        "a fresh clone must carry the tracked issues.jsonl"
    );
}

fn jsonl_record_count(beads_dir: &Path) -> usize {
    let body = fs::read_to_string(beads_dir.join("issues.jsonl")).expect("read issues.jsonl");
    body.lines().filter(|line| !line.trim().is_empty()).count()
}

/// `br create` as the very first command in a fresh clone must flush.
#[test]
fn fresh_clone_first_mutation_flushes_without_self_deadlock() {
    let donor = donor_workspace("fcm", &["seed"]);
    let clone = BrWorkspace::new();
    clone_tracked_beads(&donor, &clone);

    let created = run_br(
        &clone,
        ["create", "first", "-t", "task", "-p", "2", "--json"],
        "clone-first-create",
    );

    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr.trim()
    );
    assert!(
        !created.stderr.contains("AUTO_FLUSH_FAILED"),
        "auto-flush failed on the first mutation in a fresh clone: {}",
        created.stderr.trim()
    );
    assert!(
        !created.stderr.contains("waiting for write lock"),
        "auto-flush contended with its own process: {}",
        created.stderr.trim()
    );
    assert!(
        created.duration < FLUSH_TIMEOUT_HEADROOM,
        "first mutation took {:?}, which is the write-lock timeout, not work",
        created.duration
    );
    assert_eq!(
        jsonl_record_count(&clone.root.join(".beads")),
        2,
        "the new record was not exported to the tracked JSONL"
    );
}

/// The `-q` shape is the dangerous one: the failure warning is emitted on
/// stderr, so quiet must not be the difference between a flushed and a stale
/// ledger. Assert the ledger, not the chatter.
#[test]
fn fresh_clone_first_quiet_mutation_flushes() {
    let donor = donor_workspace("fcq", &["seed"]);
    let clone = BrWorkspace::new();
    clone_tracked_beads(&donor, &clone);

    let created = run_br(
        &clone,
        ["create", "first", "-t", "task", "-p", "2", "-q"],
        "clone-first-create-quiet",
    );

    assert!(
        created.status.success(),
        "quiet create failed: {}",
        created.stderr.trim()
    );
    assert!(
        created.duration < FLUSH_TIMEOUT_HEADROOM,
        "quiet first mutation took {:?}",
        created.duration
    );
    assert_eq!(
        jsonl_record_count(&clone.root.join(".beads")),
        2,
        "quiet mutation left the tracked JSONL stale"
    );
}

/// `br close` takes the same rebuild-then-flush path through the incremental
/// exporter, which acquires the JSONL-family authority at a different call
/// site than the fresh-export path.
#[test]
fn fresh_clone_first_close_flushes_without_self_deadlock() {
    let donor = donor_workspace("fcc", &["seed"]);
    let seed_id = {
        let listed = run_br(&donor, ["list", "--json"], "donor-list");
        assert!(listed.status.success(), "donor list failed");
        let issues = common::cli::extract_issues_array(&listed.stdout);
        issues
            .first()
            .and_then(|issue| issue.get("id"))
            .and_then(serde_json::Value::as_str)
            .expect("seed issue id")
            .to_string()
    };

    let clone = BrWorkspace::new();
    clone_tracked_beads(&donor, &clone);

    let closed = run_br(&clone, ["close", &seed_id, "--json"], "clone-first-close");
    assert!(
        closed.status.success(),
        "close failed: {}",
        closed.stderr.trim()
    );
    assert!(
        !closed.stderr.contains("AUTO_FLUSH_FAILED"),
        "auto-flush failed on the first close in a fresh clone: {}",
        closed.stderr.trim()
    );
    assert!(
        closed.duration < FLUSH_TIMEOUT_HEADROOM,
        "first close took {:?}",
        closed.duration
    );

    let body =
        fs::read_to_string(clone.root.join(".beads").join("issues.jsonl")).expect("read jsonl");
    assert!(
        body.contains("\"status\":\"closed\""),
        "the close was not exported to the tracked JSONL: {body}"
    );
}

/// The control arm from the report: a read first builds the database and exits,
/// so the following mutation opens an existing DB and never retains an
/// authority. It must stay fast — this guards against a fix that "solves" the
/// deadlock by making every flush slow.
#[test]
fn fresh_clone_read_first_then_mutation_still_flushes() {
    let donor = donor_workspace("fcr", &["seed"]);
    let clone = BrWorkspace::new();
    clone_tracked_beads(&donor, &clone);

    let listed = run_br(&clone, ["list", "-q"], "clone-read-first");
    assert!(
        listed.status.success(),
        "read-first list failed: {}",
        listed.stderr.trim()
    );

    let created = run_br(
        &clone,
        ["create", "second", "-t", "task", "-p", "2", "--json"],
        "clone-read-first-create",
    );
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr.trim()
    );
    assert!(
        !created.stderr.contains("AUTO_FLUSH_FAILED"),
        "control arm regressed: {}",
        created.stderr.trim()
    );
    assert!(
        created.duration < FLUSH_TIMEOUT_HEADROOM,
        "control arm took {:?}",
        created.duration
    );
    assert_eq!(jsonl_record_count(&clone.root.join(".beads")), 2);
}
