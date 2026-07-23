//! e2e coverage for identity inference (`BD_AGENT_ID` fallback: infer
//! the caller's prefix from a live `bd watch` in this process's
//! ancestry — see `src/config/identity.rs`).
//!
//! `test_msg_infers_identity_from_live_watch` is the "true live-watch"
//! case: it spawns a real `bd watch --prefix <p>` as a background
//! child of *this test process*, then runs `bd msg` (also a child of
//! this same test process, with `BD_AGENT_ID` unset) and asserts the
//! message lands with `from_prefix` equal to the watch's prefix. This
//! works without any special harness support because both child
//! processes share this test binary's own pid as their immediate
//! parent — exactly the "own-agent" deepest-match scenario the unit
//! tests in `config::identity` exercise on synthetic data.

use assert_cmd::prelude::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> std::path::PathBuf {
    assert_cmd::cargo::cargo_bin!("br").to_path_buf()
}

/// Polls `bd who --json` until `prefix` shows up (or times out),
/// proving the background watch has completed its first heartbeat.
fn wait_for_watcher(path: &std::path::Path, prefix: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let out = Command::new(bin())
            .current_dir(path)
            .args(["who", "--json"])
            .output()
            .expect("bd who");
        if out.status.success() {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                if let Some(rows) = json.as_array() {
                    if rows
                        .iter()
                        .any(|r| r.get("prefix").and_then(|p| p.as_str()) == Some(prefix))
                    {
                        return true;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// `bd msg` with `BD_AGENT_ID` unset and no live `bd watch` anywhere in
/// this process's ancestry (a fresh temp dir has never had one) must
/// still fail with an error that mentions `BD_AGENT_ID`, exactly as it
/// did before the inference fallback existed.
#[test]
fn test_msg_errors_without_env_or_live_watch() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();

    Command::new(bin())
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    let output = Command::new(bin())
        .current_dir(path)
        .env_remove("BD_AGENT_ID")
        .args(["msg", "operator", "hello from nobody"])
        .output()
        .expect("bd msg");

    assert!(
        !output.status.success(),
        "bd msg must fail with no BD_AGENT_ID and no live watch"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("BD_AGENT_ID"),
        "error should mention BD_AGENT_ID: {stderr}"
    );
}

/// True live-watch case: a `bd watch --prefix myprefix` running as a
/// sibling child process registers a fresh watcher row; a `bd msg`
/// invocation (also `BD_AGENT_ID`-less, also a sibling child) must
/// infer `myprefix` from it and print the audit note to stderr.
#[test]
fn test_msg_infers_identity_from_live_watch() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();

    Command::new(bin())
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    let watch_child = Command::new(bin())
        .current_dir(path)
        .env_remove("BD_AGENT_ID")
        .args(["watch", "--prefix", "myprefix", "--interval", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bd watch");

    let registered = wait_for_watcher(path, "myprefix", Duration::from_secs(10));

    // Regardless of outcome, make sure the background watch is reaped
    // before we assert (a leaked child would linger in CI otherwise).
    let cleanup = |mut child: std::process::Child| {
        let _ = child.kill();
        let _ = child.wait();
    };

    if !registered {
        cleanup(watch_child);
        panic!("bd watch never registered a watcher row for 'myprefix' in time");
    }

    let output = Command::new(bin())
        .current_dir(path)
        .env_remove("BD_AGENT_ID")
        .args(["msg", "operator", "hello via inference", "--json"])
        .output()
        .expect("bd msg");

    cleanup(watch_child);

    assert!(
        output.status.success(),
        "bd msg should succeed via inferred identity: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("identity: inferred 'myprefix'"),
        "expected the inference audit note on stderr, got: {stderr}"
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("bd msg --json output");
    assert_eq!(
        json.get("from_prefix").and_then(|v| v.as_str()),
        Some("myprefix"),
        "message sender should be the inferred prefix: {json}"
    );
}
