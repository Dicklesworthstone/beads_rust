#![cfg(all(unix, feature = "mcp"))]

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn should_clear_inherited_br_env(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    key.starts_with("BD_")
        || key.starts_with("BEADS_")
        || matches!(
            key.as_ref(),
            "BR_DISABLE_READ_ONLY_FAST_OPEN"
                | "BR_OUTPUT_FORMAT"
                | "TOON_DEFAULT_FORMAT"
                | "TOON_STATS"
        )
}

fn br_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_br"));
    command.current_dir(root);
    for (key, _) in std::env::vars_os() {
        if should_clear_inherited_br_env(&key) {
            command.env_remove(key);
        }
    }
    command.env("HOME", root);
    command.env("NO_COLOR", "1");
    command.env("RUST_LOG", "error");
    // Pin discovery to this workspace: on hosts whose TMPDIR sits inside a
    // checkout (RCH workers), `br init` would otherwise walk up to that
    // repository's own tracker and refuse.
    command.env("BEADS_DIR", root.join(".beads"));
    command
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn send_sigint(pid: u32) {
    let status = Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .expect("send SIGINT with kill");
    assert!(status.success(), "kill -INT {pid} failed: {status}");
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Output {
    let start = Instant::now();
    loop {
        if child.try_wait().expect("poll child").is_some() {
            return child.wait_with_output().expect("collect child output");
        }
        if start.elapsed() >= timeout {
            let pid = child.id();
            let status = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .expect("send SIGTERM with kill");
            assert!(status.success(), "kill -TERM {pid} failed: {status}");

            let cleanup_start = Instant::now();
            while cleanup_start.elapsed() < Duration::from_secs(2) {
                if child
                    .try_wait()
                    .expect("poll child after SIGTERM")
                    .is_some()
                {
                    let output = child
                        .wait_with_output()
                        .expect("collect timed-out child output");
                    assert_eq!(
                        output.status.code(),
                        Some(130),
                        "br serve did not exit within {timeout:?} after SIGINT and stdin close; \
                         after SIGTERM it exited with {}\nstdout:\n{}\nstderr:\n{}",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    return output;
                }
                thread::sleep(Duration::from_millis(20));
            }

            let status = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status()
                .expect("send SIGKILL with kill");
            assert!(status.success(), "kill -KILL {pid} failed: {status}");
            let output = child
                .wait_with_output()
                .expect("collect force-killed child output");
            assert_eq!(
                output.status.code(),
                Some(130),
                "br serve did not exit within {timeout:?} after SIGINT and stdin close; \
                 forced cleanup ended with {}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return output;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn serve_sigint_returns_through_main_and_preserves_reopenable_db() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    assert_success(
        &br_command(root)
            .args(["init", "--prefix", "mcp"])
            .output()
            .expect("run br init"),
        "init",
    );
    assert_success(
        &br_command(root)
            .args(["create", "shutdown checkpoint proof", "--json"])
            .output()
            .expect("run br create"),
        "create",
    );

    let mut child = br_command(root)
        .args(["serve", "--actor", "mcp-shutdown-test"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn br serve");

    thread::sleep(Duration::from_millis(200));
    assert!(
        child.try_wait().expect("poll br serve").is_none(),
        "br serve exited before SIGINT"
    );

    send_sigint(child.id());
    thread::sleep(Duration::from_millis(200));
    if let Some(mut stdin) = child.stdin.take() {
        stdin.flush().expect("flush serve stdin");
    }
    let output = wait_with_timeout(child, Duration::from_secs(5));

    assert_eq!(
        output.status.code(),
        Some(130),
        "serve must return through br main's cooperative shutdown path\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_success(
        &br_command(root)
            .args(["list", "--json"])
            .output()
            .expect("reopen DB after serve shutdown"),
        "list after shutdown",
    );
    assert_success(
        &br_command(root)
            .args(["sync", "--status", "--json"])
            .output()
            .expect("check sync health after serve shutdown"),
        "sync status after shutdown",
    );
}
