use assert_cmd::Command;
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;

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

/// Like `run_br_with_stdin`, but also sets environment variables -- for
/// commands (like `bd msg`) whose identity resolution needs
/// `BD_AGENT_ID` and whose body also needs to come from stdin in the
/// same invocation.
pub fn run_br_with_env_and_stdin<I, S, E, K, V>(
    workspace: &BrWorkspace,
    args: I,
    env_vars: E,
    input: &str,
    label: &str,
) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    run_br_full(workspace, args, env_vars, Some(input), label)
}

/// Run `br` with args passed VERBATIM \u2014 no `--prefix bd` auto-injection.
///
/// Use this (never the shimmed helpers above) for regression tests that
/// specifically assert the mandatory-`--prefix` behavior: creation without
/// `--prefix` must error, and `BD_ISSUE_PREFIX` must have zero effect.
pub fn run_br_raw_with_env<I, S, E, K, V>(
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
    run_br_full_impl(workspace, args, env_vars, None, label, false)
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
    run_br_full_impl(workspace, args, env_vars, stdin_input, label, true)
}

fn run_br_full_impl<I, S, E, K, V>(
    workspace: &BrWorkspace,
    args: I,
    env_vars: E,
    stdin_input: Option<&str>,
    label: &str,
    apply_shim: bool,
) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(&workspace.root);
    let args_vec: Vec<String> = args
        .into_iter()
        .map(|a| a.as_ref().to_string_lossy().to_string())
        .collect();
    let args_vec = if apply_shim {
        super::apply_default_test_prefix_shim(args_vec)
    } else {
        args_vec
    };
    cmd.args(&args_vec);
    // Clear ambient BD_*/BEADS_* identity and routing env vars before
    // applying any test-specific overrides below. Without this, a
    // `BD_AGENT_ID` set in the *outer* shell (e.g. an agent's own identity
    // when running this suite interactively) leaks into fixtures via the
    // cross-prefix "sender" provenance field, making snapshots/tests
    // non-hermetic and dependent on who happens to run them.
    for key in [
        "BD_AGENT_ID",
        "BD_ISSUE_PREFIX",
        "BEADS_DIR",
        "BEADS_JSONL",
        "BEADS_AUTO_START_DAEMON",
        "BEADS_FLUSH_DEBOUNCE",
        "BEADS_REMOTE_SYNC_INTERVAL",
        "BR_OUTPUT_FORMAT",
        "TOON_DEFAULT_FORMAT",
    ] {
        cmd.env_remove(key);
    }
    cmd.envs(env_vars);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_LOG", "beads_rust=debug");
    cmd.env("RUST_BACKTRACE", "1");
    cmd.env("HOME", &workspace.root);

    if let Some(input) = stdin_input {
        cmd.write_stdin(input);
    }

    let start = Instant::now();
    let output = cmd.output().expect("run br");
    let duration = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let log_path = workspace.log_dir.join(format!("{label}.log"));
    let timestamp = SystemTime::now();
    let log_body = format!(
        "label: {label}\nstarted: {:?}\nduration: {:?}\nstatus: {}\nargs: {:?}\ncwd: {}\n\nstdout:\n{}\n\nstderr:\n{}\n",
        timestamp,
        duration,
        output.status,
        cmd.get_args().collect::<Vec<_>>(),
        workspace.root.display(),
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

/// Create an issue via markdown bulk import (`create --file`).
///
/// `create --labels` / `update --add-label` etc. were removed from the CLI
/// (the fields are `#[arg(skip)]`, kept only for back-compat); the only
/// surviving way to attach labels at creation time is the markdown
/// bulk-import grammar (a `### Labels` section). This helper builds a
/// one-issue markdown file and imports it, returning the created issue's ID
/// so callers can keep using the plain CLI for everything else (filtering,
/// updates that don't touch labels, etc).
///
/// `issue_type`/`priority`/`assignee` are optional convenience fields since
/// bulk import supports setting them in the same pass; pass `None` to leave
/// them at their defaults.
pub fn create_via_markdown(
    workspace: &BrWorkspace,
    label: &str,
    title: &str,
    issue_type: Option<&str>,
    priority: Option<&str>,
    assignee: Option<&str>,
    labels: &[&str],
) -> String {
    create_via_markdown_with_description(workspace, label, title, issue_type, priority, assignee, None, labels)
}

/// Like [`create_via_markdown`] but also allows setting a description, for
/// fixtures that need both labels and searchable description text.
#[allow(clippy::too_many_arguments)]
pub fn create_via_markdown_with_description(
    workspace: &BrWorkspace,
    label: &str,
    title: &str,
    issue_type: Option<&str>,
    priority: Option<&str>,
    assignee: Option<&str>,
    description: Option<&str>,
    labels: &[&str],
) -> String {
    let mut md = format!("## {title}\n");
    if let Some(t) = issue_type {
        md.push_str(&format!("### Type\n{t}\n"));
    }
    if let Some(p) = priority {
        md.push_str(&format!("### Priority\n{p}\n"));
    }
    if let Some(a) = assignee {
        md.push_str(&format!("### Assignee\n{a}\n"));
    }
    if let Some(d) = description {
        md.push_str(&format!("### Description\n{d}\n"));
    }
    if !labels.is_empty() {
        md.push_str("### Labels\n");
        md.push_str(&labels.join(", "));
        md.push('\n');
    }

    let file_path = workspace.root.join(format!("{label}.md"));
    fs::write(&file_path, md).expect("write markdown fixture");

    let run = run_br(
        workspace,
        [
            "create".to_string(),
            "--file".to_string(),
            file_path.to_string_lossy().to_string(),
            "--json".to_string(),
        ],
        label,
    );
    assert!(
        run.status.success(),
        "create --file (markdown import) failed for '{title}': {}",
        run.stderr
    );

    let payload = extract_json_payload(&run.stdout);
    let created: serde_json::Value =
        serde_json::from_str(&payload).expect("markdown import JSON parse");
    created
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|issue| issue["id"].as_str())
        .expect("markdown import should return created issue with id")
        .to_string()
}
