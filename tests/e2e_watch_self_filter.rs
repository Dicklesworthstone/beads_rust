//! e2e coverage for `bd watch`'s self-event filter, exercised through
//! *real actor resolution* rather than a hand-built comparison.
//!
//! This is the regression test for the `--include-self`-off default
//! being silently inoperative (bead `beads1-s36s7`): `created_by` is
//! written by `config::resolve_actor_with_storage` (agent identity
//! spliced in ahead of `$USER`), while `watch`'s self-filter used to
//! resolve the side it compares against via plain
//! `config::resolve_actor` (`$USER`). The two never matched under an
//! agent identity, so every agent was woken by its own writes.
//!
//! The unit test `is_self_requires_no_sender_and_matching_creator` in
//! `watch.rs` cannot catch that: it hardcodes both sides of the `==`
//! and so passes in a world where the two sides *resolve*
//! differently. The seam that regressed is actor resolution, and the
//! only way to cross it is end to end — a real `br watch` process
//! resolving its own actor from the environment/config/DB, against
//! beads whose `created_by` was written by a real `br create`.
//!
//! Shape of each test:
//!   1. spawn `br watch --prefix <p>` with `BD_AGENT_ID=<agent>`,
//!      streaming (`--debounce 0`) JSON events;
//!   2. while it runs, create one bead as that same agent (self) and
//!      one bead as a different agent (foreign);
//!   3. assert on the emitted event stream.
//!
//! The foreign bead is the in-test positive control: it proves the
//! watcher really observed the creation window, so "the self bead is
//! absent" cannot pass vacuously (e.g. because nothing was polled at
//! all).

use assert_cmd::prelude::*;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

fn bin() -> std::path::PathBuf {
    assert_cmd::cargo::cargo_bin!("br").to_path_buf()
}

fn init(path: &std::path::Path) {
    Command::new(bin())
        .current_dir(path)
        .arg("init")
        .assert()
        .success();
}

/// Create one bead under `prefix`, attributed to `agent` via
/// `BD_AGENT_ID` (which is what `created_by` records). Returns its id.
fn create_as(path: &std::path::Path, agent: &str, prefix: &str, title: &str) -> String {
    let out = Command::new(bin())
        .current_dir(path)
        .env("BD_AGENT_ID", agent)
        .env_remove("BD_ACTOR")
        .args(["create", title, "--prefix", prefix, "--json"])
        .output()
        .expect("create bead");
    assert!(out.status.success(), "create failed: {out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("create json");
    assert_eq!(
        json["created_by"].as_str(),
        Some(agent),
        "precondition: created_by must record the agent identity, not $USER: {json}"
    );
    json["id"].as_str().expect("id").to_string()
}

/// Spawn `br watch` as the agent whose identity *is* `prefix` (the
/// fleet-realistic shape: every agent watches its own bead
/// namespace), create a self-authored and a foreign-authored bead
/// while it polls, and return `(self_id, foreign_id, stdout)`.
///
/// The agent id must equal the watched prefix to reproduce
/// production: `br create` only stamps `sender` when the calling
/// agent's identity differs from the target prefix, and `is_self`
/// requires `sender.is_none()`. An agent filing into its own prefix
/// therefore produces exactly the shape the filter must catch —
/// `sender: None`, `created_by: <agent identity>` — which is the
/// shape the plain-`resolve_actor` comparison failed to recognise.
fn watch_run(prefix: &str, include_self: bool) -> (String, String, String) {
    let agent = prefix;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();
    init(path);

    let mut args = vec![
        "watch",
        "--prefix",
        prefix,
        "--interval",
        "1",
        "--max-ticks",
        "6",
        // Streaming mode: no debounce batching, so each diff is emitted
        // the moment it is seen and the test does not have to wait out a
        // 120s window.
        "--debounce",
        "0",
        "--debounce-max",
        "0",
        "--no-inbox",
        "--format",
        "json",
    ];
    if include_self {
        args.push("--include-self");
    }

    let mut child = Command::new(bin())
        .current_dir(path)
        .env("BD_AGENT_ID", agent)
        .env_remove("BD_ACTOR")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn br watch");

    // Let the watcher take its startup snapshot (which must not contain
    // these beads, or they would never look "created").
    std::thread::sleep(Duration::from_millis(1200));

    let self_id = create_as(path, agent, prefix, "bead created by the watcher itself");
    let foreign_id = create_as(path, "someone-else", prefix, "bead created by another agent");

    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_string(&mut stdout)
        .expect("read watch stdout");
    let status = child.wait().expect("wait for br watch");
    assert!(status.success(), "br watch exited non-zero: {status:?}");

    (self_id, foreign_id, stdout)
}

/// Collect the ids `bd watch` reported as `created`.
fn created_ids(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|ev| ev["event"].as_str() == Some("created"))
        .filter_map(|ev| ev["id"].as_str().map(str::to_string))
        .collect()
}

/// Default (`--include-self` absent): a bead this agent created itself
/// is filtered, while another agent's bead under the same prefix still
/// surfaces.
///
/// This fails on the pre-fix code: the watcher resolved its own actor
/// as `$USER` while `created_by` held the agent identity, so the
/// self-authored bead was surfaced too.
#[test]
fn watch_filters_self_created_bead_resolved_through_agent_identity() {
    let (self_id, foreign_id, stdout) = watch_run("wself", false);
    let created = created_ids(&stdout);

    assert!(
        created.contains(&foreign_id),
        "positive control: a foreign-created bead must surface, else the watcher \
         never observed the window. created={created:?} stdout=\n{stdout}"
    );
    assert!(
        !created.contains(&self_id),
        "self-created bead {self_id} must be filtered when --include-self is off \
         (created_by is written with the agent identity, so the filter must \
         resolve the comparison actor the same way). created={created:?} stdout=\n{stdout}"
    );
}

/// `--include-self` still surfaces the agent's own writes — the fix
/// tightens the default without disabling the opt-in.
#[test]
fn watch_include_self_surfaces_own_created_bead() {
    let (self_id, foreign_id, stdout) = watch_run("wboth", true);
    let created = created_ids(&stdout);

    assert!(
        created.contains(&foreign_id),
        "foreign bead must surface with --include-self too. \
         created={created:?} stdout=\n{stdout}"
    );
    assert!(
        created.contains(&self_id),
        "--include-self must surface the watcher's own bead. \
         created={created:?} stdout=\n{stdout}"
    );
}
