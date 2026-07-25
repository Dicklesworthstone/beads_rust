use assert_cmd::Command;
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;

const SMOKE_PRESERVED_ENV_KEYS: &[&str] = &[
    "BEADS_DIR",
    "BEADS_JSONL",
    "BEADS_CACHE_DIR",
    "BR_OUTPUT_FORMAT",
    "TOON_DEFAULT_FORMAT",
    "TOON_STATS",
];

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

fn should_preserve_smoke_env(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    SMOKE_PRESERVED_ENV_KEYS.contains(&key.as_ref())
}

fn clear_inherited_br_env(cmd: &mut Command) {
    clear_inherited_br_env_except(cmd, &[]);
}

/// `$PATH` with every directory after the first that holds a `br` executable
/// removed.
///
/// `br doctor` reports `br_path_dupes` when more than one `br` is reachable on
/// `$PATH`, and since #292 any WARN flips `ok` to false and exits 1. A
/// developer who has both `~/.local/bin/br` (install script) and
/// `~/.cargo/bin/br` (`cargo install`) — the exact combination the README warns
/// about — would therefore fail every "healthy workspace" doctor test for
/// reasons that have nothing to do with the workspace under test.
///
/// Only later duplicates are dropped, so the first such directory survives and
/// siblings living beside it (notably the Go `bd` binary used by the
/// conformance suite) stay reachable. The dedicated
/// `tests/doctor_fixtures/multiple_br_in_path` fixture builds its own `$PATH`
/// and so still exercises the detection deliberately.
pub fn deduplicated_br_path() -> std::ffi::OsString {
    let Some(path) = std::env::var_os("PATH") else {
        return std::ffi::OsString::new();
    };
    let mut seen_br_dir = false;
    let kept: Vec<std::path::PathBuf> = std::env::split_paths(&path)
        .filter(|dir| {
            let has_br = dir.join("br").is_file();
            if !has_br {
                return true;
            }
            if seen_br_dir {
                return false;
            }
            seen_br_dir = true;
            true
        })
        .collect();
    std::env::join_paths(kept).unwrap_or(path)
}

fn clear_inherited_br_env_except(cmd: &mut Command, preserve: &[&str]) {
    for (key, _) in std::env::vars_os() {
        let key_str = key.to_string_lossy();
        let should_preserve = preserve.contains(&key_str.as_ref());
        if should_clear_inherited_br_env(&key) && !should_preserve {
            cmd.env_remove(&key);
        }
    }
}

#[derive(Debug)]
pub struct BrRun {
    pub stdout: String,
    pub stderr: String,
    pub status: std::process::ExitStatus,
    pub duration: Duration,
    pub log_path: PathBuf,
}

pub struct BrWorkspace {
    pub temp_dir: TempDir,
    pub root: PathBuf,
    pub log_dir: PathBuf,
}

impl BrWorkspace {
    pub fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp dir");
        let root = temp_dir.path().to_path_buf();
        let log_dir = root.join("logs");
        fs::create_dir_all(&log_dir).expect("log dir");
        Self {
            temp_dir,
            root,
            log_dir,
        }
    }
}

pub fn run_br<I, S>(workspace: &BrWorkspace, args: I, label: &str) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // Reuse run_br_with_env with empty env vars
    run_br_with_env(
        workspace,
        args,
        std::iter::empty::<(String, String)>(),
        label,
    )
}

pub fn run_br_with_env<I, S, E, K, V>(
    workspace: &BrWorkspace,
    args: I,
    env_vars: E,
    label: &str,
) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    run_br_full(workspace, args, env_vars, None, label)
}

pub fn run_br_with_stdin<I, S>(workspace: &BrWorkspace, args: I, input: &str, label: &str) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_br_full(
        workspace,
        args,
        std::iter::empty::<(String, String)>(),
        Some(input),
        label,
    )
}

pub fn run_br_smoke_at_root_with_env<I, S, E, K, V>(
    root: &Path,
    args: I,
    env_vars: E,
    label: &str,
) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let log_dir = root.join("logs");
    run_br_full_in_root(root, &log_dir, args, env_vars, None, label, true)
}

fn run_br_full<I, S, E, K, V>(
    workspace: &BrWorkspace,
    args: I,
    env_vars: E,
    stdin_input: Option<&str>,
    label: &str,
) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    run_br_full_in_root(
        &workspace.root,
        &workspace.log_dir,
        args,
        env_vars,
        stdin_input,
        label,
        false,
    )
}

fn run_br_full_in_root<I, S, E, K, V>(
    root: &Path,
    log_dir: &Path,
    args: I,
    env_vars: E,
    stdin_input: Option<&str>,
    label: &str,
    preserve_smoke_env: bool,
) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    fs::create_dir_all(log_dir).expect("log dir");

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(root);
    cmd.args(args);
    if preserve_smoke_env {
        clear_inherited_br_env_except(&mut cmd, SMOKE_PRESERVED_ENV_KEYS);
    } else {
        clear_inherited_br_env(&mut cmd);
    }
    // Default e2e runs un-throttled so history-mechanics tests (backup
    // chronology, prune, restore) observe one `.br_history` snapshot per
    // mutation. Set before caller `env_vars` so a test can override this to
    // exercise the #313 snapshot throttle.
    cmd.env("BR_HISTORY_MIN_INTERVAL_SECS", "0");
    cmd.envs(env_vars);
    cmd.env("NO_COLOR", "1");
    // `error`, not `beads_rust=debug`. Debug tracing goes to stderr, which
    // (a) `br doctor`'s own `rust_log` check flags as an agent-hostile
    // setting — so every "healthy workspace" doctor assertion failed purely
    // because the harness set it — and (b) drowns the assertions that match
    // on stderr contents. Tests that specifically want verbose tracing pass
    // RUST_LOG through `run_br_with_env`, which is applied above and so still
    // wins over this default.
    cmd.env("RUST_LOG", "error");
    cmd.env("RUST_BACKTRACE", "1");
    cmd.env("PATH", deduplicated_br_path());
    cmd.env("HOME", root);

    if let Some(input) = stdin_input {
        cmd.write_stdin(input);
    }

    let start = Instant::now();
    let output = cmd.output().expect("run br");
    let duration = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let log_path = log_dir.join(format!("{label}.log"));
    let timestamp = SystemTime::now();
    let log_body = format!(
        "label: {label}\nstarted: {:?}\nduration: {:?}\nstatus: {}\nargs: {:?}\ncwd: {}\n\nstdout:\n{}\n\nstderr:\n{}\n",
        timestamp,
        duration,
        output.status,
        cmd.get_args().collect::<Vec<_>>(),
        root.display(),
        stdout,
        stderr
    );
    fs::write(&log_path, log_body).expect("write log");

    BrRun {
        stdout,
        stderr,
        status: output.status,
        duration,
        log_path,
    }
}

/// Extract the issue ID from `br create` stdout.
///
/// Handles both formats: `"Created pfx-xxx: title"` and `"✓ Created pfx-xxx: title"`.
pub fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string()
}

pub fn extract_json_payload(stdout: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            return lines[idx..].join("\n").trim().to_string();
        }
    }
    stdout.trim().to_string()
}

pub fn parse_json_value(stdout: &str) -> Value {
    let payload = extract_json_payload(stdout);
    serde_json::from_str(&payload).expect("valid JSON payload")
}

/// Extract an issue array from JSON stdout, handling both formats:
/// - Paginated: `{"issues": [...], "total": N, ...}` → returns the inner array
/// - Bare array: `[...]` → returns it directly
pub fn extract_issues_array(stdout: &str) -> Vec<Value> {
    let json = parse_json_value(stdout);
    if let Some(arr) = json.as_array() {
        return arr.clone();
    }
    if let Some(issues) = json.get("issues").and_then(Value::as_array) {
        return issues.clone();
    }
    panic!(
        "JSON output is neither a bare array nor an object with 'issues': {}",
        &stdout[..stdout.len().min(200)]
    );
}

pub fn parse_list_page(stdout: &str) -> Value {
    let json = parse_json_value(stdout);
    assert!(
        json.is_object(),
        "list JSON should be an object with pagination metadata"
    );
    assert!(
        json.get("issues").is_some(),
        "list JSON should contain an issues field"
    );
    json
}

pub fn parse_list_issues(stdout: &str) -> Vec<Value> {
    parse_list_page(stdout)
        .get("issues")
        .and_then(Value::as_array)
        .cloned()
        .expect("list JSON should contain an issues array")
}

#[cfg(test)]
mod tests {
    use super::{should_clear_inherited_br_env, should_preserve_smoke_env};
    use std::ffi::OsStr;

    #[test]
    fn inherited_beads_and_toon_env_are_cleared() {
        for key in [
            "BD_ACTOR",
            "BEADS_CACHE_DIR",
            "BEADS_JSONL",
            "BR_DISABLE_READ_ONLY_FAST_OPEN",
            "BR_OUTPUT_FORMAT",
            "TOON_DEFAULT_FORMAT",
            "TOON_STATS",
        ] {
            assert!(
                should_clear_inherited_br_env(OsStr::new(key)),
                "{key} should be cleared for hermetic br tests"
            );
        }
    }

    #[test]
    fn unrelated_env_are_preserved() {
        for key in ["HOME", "PATH", "RUST_LOG", "NO_COLOR"] {
            assert!(
                !should_clear_inherited_br_env(OsStr::new(key)),
                "{key} should not be blanket-cleared"
            );
        }
    }

    #[test]
    fn smoke_profile_preserves_selected_routing_and_output_env() {
        for key in [
            "BEADS_DIR",
            "BEADS_CACHE_DIR",
            "BEADS_JSONL",
            "BR_OUTPUT_FORMAT",
            "TOON_DEFAULT_FORMAT",
            "TOON_STATS",
        ] {
            assert!(
                should_preserve_smoke_env(OsStr::new(key)),
                "{key} should be preserved for non-hermetic smoke coverage"
            );
        }
    }

    #[test]
    fn smoke_profile_still_clears_unrelated_legacy_beads_env() {
        for key in ["BD_ACTOR", "BD_DB", "BD_CONFIG", "BEADS_DEBUG"] {
            assert!(
                !should_preserve_smoke_env(OsStr::new(key)),
                "{key} should still be scrubbed in smoke mode"
            );
        }
    }
}
