//! Deterministic reproduction of the `bd watch` replay bursts
//! (bead `beads1-3435j`): every bead under the watched prefix
//! re-emitted as a fresh `created` event, some of them rendered
//! `created (closed)` — a "creation" carrying a status the bead only
//! reached later.
//!
//! Mechanism (confirmed by running these tests against the pre-fix
//! binary): `bd watch` is a *snapshot differ*, not an event-log reader.
//! On a cycle where `snapshot_state` returned `Err`, the loop
//! substituted an EMPTY snapshot and committed it as the new baseline.
//! So one transient read failure produced two lies:
//!
//!   * the failing cycle diffed `full -> empty` and recorded a
//!     `deleted` for every bead under the prefix;
//!   * the next successful cycle diffed `empty -> full` and recorded a
//!     `created` for every one of them, labelled with each bead's
//!     CURRENT status — hence `created (closed)`.
//!
//! Under a debounce window (the fleet default is 120s) `record_change`
//! collapses `(Deleted, Created)` into `Created`, so the delete half
//! never reaches the operator and the burst looks like an unexplained
//! replay of the whole prefix. That collapse is why the reported
//! symptom was creations only.
//!
//! The fault is injected by making the DB file unreadable (`chmod 000`)
//! for a couple of poll cycles — the same `Err` any transient cause
//! (SQLITE_BUSY beyond the 30s lock timeout, a failing auto-import
//! inside `open_storage`, a DB being replaced under the watcher)
//! delivers to the same code path. The tests assert on stderr that the
//! failure really was injected, so they cannot pass vacuously.
//!
//! Note that in production the watcher's stderr goes to /dev/null,
//! which is why the "bead snapshot failed" line that explains these
//! bursts was never seen.

use assert_cmd::prelude::*;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn bin() -> std::path::PathBuf {
    assert_cmd::cargo::cargo_bin!("br").to_path_buf()
}

fn run(path: &Path, agent: &str, args: &[&str]) -> std::process::Output {
    let out = Command::new(bin())
        .current_dir(path)
        .env("BD_AGENT_ID", agent)
        .env_remove("BD_ACTOR")
        .args(args)
        .output()
        .expect("run br");
    assert!(out.status.success(), "br {args:?} failed: {out:?}");
    out
}

/// Can this process be denied read access by mode bits? Root ignores
/// them, so the fault cannot be injected there and the test would pass
/// for the wrong reason.
fn can_deny_own_read(path: &Path) -> bool {
    let original = std::fs::metadata(path).expect("metadata").permissions();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
    let denied = std::fs::File::open(path).is_err();
    std::fs::set_permissions(path, original).expect("restore perms");
    denied
}

struct Fixture {
    temp: tempfile::TempDir,
    prefix: String,
}

impl Fixture {
    /// A repo with three beads under `prefix`, one of them closed so a
    /// spurious replay would render the tell-tale `created (closed)`.
    fn new(prefix: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();
        Command::new(bin())
            .current_dir(path)
            .arg("init")
            .assert()
            .success();

        let mut ids = Vec::new();
        for title in ["first", "second", "third"] {
            let out = run(
                path,
                prefix,
                &["create", title, "--prefix", prefix, "--json"],
            );
            let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
            ids.push(json["id"].as_str().expect("id").to_string());
        }
        run(path, prefix, &["close", &ids[0]]);

        Self {
            temp,
            prefix: prefix.to_string(),
        }
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }

    fn db(&self) -> std::path::PathBuf {
        self.path().join(".beads").join("beads.db")
    }

    fn spawn_watch(&self, extra: &[&str]) -> Child {
        let mut args = vec![
            "watch",
            "--prefix",
            &self.prefix,
            "--interval",
            "1",
            "--max-ticks",
            "12",
            "--no-inbox",
            // Self-filtering is deliberately DISABLED here: these beads
            // are self-authored, and with the filter on (the fixed
            // default) the replay would be silently swallowed. Masking
            // the symptom is not the same as not producing it, and this
            // test is about not producing it.
            "--include-self",
        ];
        args.extend_from_slice(extra);
        Command::new(bin())
            .current_dir(self.path())
            .env("BD_AGENT_ID", &self.prefix)
            .env_remove("BD_ACTOR")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn br watch")
    }

    /// Run a watcher through a window in which the DB is unreadable,
    /// returning its (stdout, stderr).
    fn watch_across_db_outage(&self, extra: &[&str]) -> (String, String) {
        let db = self.db();
        let original = std::fs::metadata(&db).expect("metadata").permissions();

        let mut child = self.spawn_watch(extra);
        // Let it take a good baseline snapshot first.
        std::thread::sleep(Duration::from_millis(2500));
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
        std::thread::sleep(Duration::from_millis(2500));
        std::fs::set_permissions(&db, original).expect("restore perms");

        let mut stdout = String::new();
        child
            .stdout
            .take()
            .expect("piped stdout")
            .read_to_string(&mut stdout)
            .expect("read stdout");
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("piped stderr")
            .read_to_string(&mut stderr)
            .expect("read stderr");
        child.wait().expect("wait");
        (stdout, stderr)
    }
}

/// Streaming mode (`--debounce 0`) shows both halves of the lie: on the
/// pre-fix binary this produced a `deleted` for all three beads
/// followed by a `created` for all three. A transient read failure must
/// produce no bead events at all.
#[test]
fn transient_snapshot_failure_does_not_replay_the_prefix() {
    let fx = Fixture::new("rpstr");
    if !can_deny_own_read(&fx.db()) {
        eprintln!("skipping: this process can read a 0o000 file (root?), fault not injectable");
        return;
    }

    let (stdout, stderr) =
        fx.watch_across_db_outage(&["--debounce", "0", "--debounce-max", "0", "--format", "json"]);

    // Positive control: prove the outage actually hit the snapshot.
    assert!(
        stderr.contains("bead snapshot failed"),
        "fault was not injected — the snapshot never failed, so this test \
         proves nothing. stderr=\n{stderr}"
    );

    let events: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let bead_events: Vec<&serde_json::Value> = events
        .iter()
        .filter(|ev| matches!(ev["event"].as_str(), Some("created" | "deleted" | "status")))
        .collect();
    assert!(
        bead_events.is_empty(),
        "a failed snapshot must not be treated as an empty one: nothing changed, \
         so no bead events are due. got={bead_events:#?}\nstdout=\n{stdout}"
    );
}

/// Debounced mode is the shape the fleet actually runs, and the shape
/// `beads1-3435j` was reported in: the `(Deleted, Created)` collapse
/// hides the delete half, so the pre-fix binary emitted exactly
/// `N beads from self` with `+ <id> created (closed)` for the bead that
/// had since been closed.
#[test]
fn transient_snapshot_failure_does_not_emit_a_debounced_replay_batch() {
    let fx = Fixture::new("rpdeb");
    if !can_deny_own_read(&fx.db()) {
        eprintln!("skipping: this process can read a 0o000 file (root?), fault not injectable");
        return;
    }

    let (stdout, stderr) = fx.watch_across_db_outage(&["--debounce", "2", "--debounce-max", "6"]);

    assert!(
        stderr.contains("bead snapshot failed"),
        "fault was not injected. stderr=\n{stderr}"
    );
    assert!(
        !stdout.contains("created"),
        "no bead was created during this window, so no batch may report a \
         creation — least of all `created (closed)`, which pairs a synthetic \
         creation with a status the bead reached long before. stdout=\n{stdout}"
    );
    assert!(
        stdout.trim().is_empty(),
        "nothing changed under the prefix; the watcher should have been silent. \
         stdout=\n{stdout}"
    );
}
