//! Golden snapshots for Rich-mode panel/table widths.
//!
//! The usual CLI test helpers force `NO_COLOR=1`, which makes `br` select plain
//! output. These tests run `br` under `script(1)` so stdout is a pseudo-terminal
//! and the Rich renderer observes the requested terminal width.

mod common;

use assert_cmd::Command;
use insta::assert_snapshot;
use regex::Regex;
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tempfile::TempDir;

struct RichFixture {
    _temp_dir: TempDir,
    root: PathBuf,
    show_id: String,
}

fn should_clear_inherited_br_env(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    key.starts_with("BD_")
        || key.starts_with("BEADS_")
        || matches!(
            key.as_ref(),
            "BR_OUTPUT_FORMAT" | "TOON_DEFAULT_FORMAT" | "TOON_STATS" | "NO_COLOR"
        )
}

fn clear_inherited_br_env(cmd: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if should_clear_inherited_br_env(&key) {
            cmd.env_remove(key);
        }
    }
}

fn br_cmd() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("br"))
}

fn run_setup_br(root: &Path, args: &[&str]) -> String {
    let mut cmd = br_cmd();
    cmd.current_dir(root);
    cmd.args(args);
    clear_inherited_br_env(&mut cmd);
    cmd.env("HOME", root);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_LOG", "error");
    cmd.env("RUST_BACKTRACE", "1");

    let output = cmd.output().expect("run setup br command");
    assert!(
        output.status.success(),
        "br setup command failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn extract_json_payload(stdout: &str) -> &str {
    let start = stdout
        .find('{')
        .or_else(|| stdout.find('['))
        .expect("JSON payload in stdout");
    stdout[start..].trim()
}

fn create_issue(
    root: &Path,
    title: &str,
    issue_type: &str,
    priority: &str,
    description: &str,
    labels: &str,
) -> String {
    let stdout = run_setup_br(
        root,
        &[
            "create",
            title,
            "--type",
            issue_type,
            "--priority",
            priority,
            "--description",
            description,
            "--labels",
            labels,
            "--json",
        ],
    );
    let parsed: Value =
        serde_json::from_str(extract_json_payload(&stdout)).expect("create JSON output");
    parsed["id"].as_str().expect("created issue id").to_string()
}

fn init_fixture() -> RichFixture {
    let temp_dir =
        TempDir::new_in(common::cli::isolated_temp_root()).expect("create isolated temp dir");
    let root = temp_dir.path().to_path_buf();

    run_setup_br(&root, &["init", "--prefix", "rich"]);

    let show_id = create_issue(
        &root,
        "Alpha layout regression with a medium length title",
        "bug",
        "1",
        "A deterministic issue used to freeze Rich-mode show panel wrapping and field alignment.",
        "ui,regression",
    );
    let blocked_id = create_issue(
        &root,
        "Beta table row exercises dependency columns",
        "feature",
        "2",
        "Second fixture issue with dependency metadata for list and stats rendering.",
        "backend,triage",
    );
    let closed_id = create_issue(
        &root,
        "Gamma closed work contributes status counts",
        "task",
        "3",
        "Closed fixture issue so the statistics panel contains mixed status data.",
        "done,metrics",
    );

    run_setup_br(
        &root,
        &[
            "comments",
            "add",
            &show_id,
            "--author",
            "ubuntu",
            "A stable comment keeps the show panel exercising comment rendering.",
        ],
    );
    run_setup_br(&root, &["dep", "add", &blocked_id, &show_id]);
    run_setup_br(
        &root,
        &[
            "close",
            &closed_id,
            "--reason",
            "Completed for golden snapshot coverage",
        ],
    );

    RichFixture {
        _temp_dir: temp_dir,
        root,
        show_id,
    }
}

fn sh_quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn run_rich_br(root: &Path, width: usize, args: &[&str]) -> String {
    run_rich_br_with_env(root, width, &[], args)
}

/// Run br under a pseudo-terminal with extra `KEY=VALUE` assignments placed
/// in front of the command (so they reach br, not `script`).
fn run_rich_br_with_env(
    root: &Path,
    width: usize,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> String {
    let br_bin = assert_cmd::cargo::cargo_bin!("br");
    let mut command_parts = vec![sh_quote(br_bin.as_os_str())];
    command_parts.extend(args.iter().map(|arg| sh_quote(OsStr::new(arg))));
    let env_prefix = extra_env
        .iter()
        .fold(String::new(), |mut acc, (key, value)| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{key}={} ", sh_quote(OsStr::new(value)));
            acc
        });
    let command_line = format!(
        "stty cols {width} rows 40 && COLUMNS={width} {env_prefix}{}",
        command_parts.join(" ")
    );

    let mut cmd = Command::new("script");
    cmd.current_dir(root);
    cmd.args(["-q", "-e", "-c", &command_line, "/dev/null"]);
    clear_inherited_br_env(&mut cmd);
    cmd.env("HOME", root);
    cmd.env("COLUMNS", width.to_string());
    cmd.env("RUST_LOG", "error");
    cmd.env("RUST_BACKTRACE", "1");

    let output = cmd.output().expect("run br under pseudo-terminal");
    assert!(
        output.status.success(),
        "rich br command failed at width {width}: {:?}\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    normalize_rich_output(&String::from_utf8_lossy(&output.stdout))
}

fn issue_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\brich-[a-z0-9]{3,}\b").expect("issue id regex"))
}

fn timestamp_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\d{4}-\d{2}-\d{2}(?:[ T]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z| UTC)?)?")
            .expect("timestamp regex")
    })
}

fn relative_time_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:just now|\d+(?:\.\d+)?(?:ns|us|µs|ms|s|m|h|d) ago)\b")
            .expect("relative time regex")
    })
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            output.push(ch);
            continue;
        }

        if chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if ('@'..='~').contains(&code) {
                    break;
                }
            }
        }
    }

    output
}

fn replace_preserving_width(input: &str, regex: &Regex, placeholder: &str) -> String {
    regex
        .replace_all(input, |captures: &regex::Captures<'_>| {
            let matched_width = captures[0].chars().count();
            let placeholder_width = placeholder.chars().count();
            if placeholder_width >= matched_width {
                placeholder.to_string()
            } else {
                format!(
                    "{placeholder}{}",
                    " ".repeat(matched_width - placeholder_width)
                )
            }
        })
        .into_owned()
}

fn normalize_rich_output(raw: &str) -> String {
    let normalized_newlines = raw.replace("\r\n", "\n").replace('\r', "\n");
    let without_script_markers = normalized_newlines
        .lines()
        .filter(|line| !line.starts_with("Script started") && !line.starts_with("Script done"))
        .collect::<Vec<_>>()
        .join("\n");
    let without_ansi = strip_ansi(&without_script_markers);
    let without_ids = replace_preserving_width(&without_ansi, issue_id_re(), "rich-ID");
    let without_timestamps = replace_preserving_width(&without_ids, timestamp_re(), "TIMESTAMP");
    let without_relative_times =
        replace_preserving_width(&without_timestamps, relative_time_re(), "TIME_AGO");
    without_relative_times
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

fn assert_rich_frame(output: &str, command: &str, width: usize) {
    assert!(
        output.contains('┌')
            || output.contains('┏')
            || output.contains('╭')
            || output.contains('╔'),
        "expected Rich frame characters for {command} at width {width}, got:\n{output}"
    );
}

#[test]
fn golden_list_rich_widths() {
    let fixture = init_fixture();

    let width_80 = run_rich_br(&fixture.root, 80, &["list", "--limit", "3"]);
    assert_rich_frame(&width_80, "list", 80);
    assert_snapshot!("list_width_80", width_80);

    let width_120 = run_rich_br(&fixture.root, 120, &["list", "--limit", "3"]);
    assert_rich_frame(&width_120, "list", 120);
    assert_snapshot!("list_width_120", width_120);
}

#[test]
fn golden_show_rich_widths() {
    let fixture = init_fixture();

    let width_80 = run_rich_br(&fixture.root, 80, &["show", &fixture.show_id]);
    assert_rich_frame(&width_80, "show", 80);
    assert_snapshot!("show_width_80", width_80);

    let width_120 = run_rich_br(&fixture.root, 120, &["show", &fixture.show_id]);
    assert_rich_frame(&width_120, "show", 120);
    assert_snapshot!("show_width_120", width_120);
}

#[test]
fn golden_stats_rich_widths() {
    let fixture = init_fixture();

    let width_80 = run_rich_br(&fixture.root, 80, &["stats"]);
    assert_rich_frame(&width_80, "stats", 80);
    assert_snapshot!("stats_width_80", width_80);

    let width_120 = run_rich_br(&fixture.root, 120, &["stats"]);
    assert_rich_frame(&width_120, "stats", 120);
    assert_snapshot!("stats_width_120", width_120);
}

/// Rich-mode `show` renders a markdown description (headings, emphasis,
/// inline code, lists) inside the panel instead of printing the raw markup.
/// The other goldens use prose descriptions and stay byte-identical.
#[test]
fn golden_show_rich_markdown_description() {
    let fixture = init_fixture();
    let md_id = create_issue(
        &fixture.root,
        "Delta issue with a markdown body",
        "task",
        "2",
        "# Plan\n\nUse **bold** and `inline code` in the body.\n\n- first item\n- second item\n\n> a quoted note",
        "docs",
    );

    let width_80 = run_rich_br(&fixture.root, 80, &["show", &md_id]);
    assert_rich_frame(&width_80, "show", 80);
    assert!(
        !width_80.contains("**bold**") && !width_80.contains("# Plan"),
        "markdown markup must be rendered, not printed raw:\n{width_80}"
    );
    assert!(
        width_80.contains("Plan") && width_80.contains("bold"),
        "content must survive rendering:\n{width_80}"
    );
    assert_snapshot!("show_markdown_width_80", width_80);
}

/// `TERM=dumb` on a real pseudo-terminal must select Plain mode: no ANSI
/// styling and no box-drawing panel, while the same command without it
/// renders the Rich panel (proved by the frame assertion in the other tests).
#[test]
fn term_dumb_pty_selects_plain_mode() {
    let fixture = init_fixture();

    let rich = run_rich_br(&fixture.root, 80, &["show", &fixture.show_id]);
    assert_rich_frame(&rich, "show", 80);

    let dumb = run_rich_br_with_env(
        &fixture.root,
        80,
        &[("TERM", "dumb")],
        &["show", &fixture.show_id],
    );
    assert!(
        !dumb.contains("\x1b["),
        "TERM=dumb output must not contain ANSI escape codes:\n{dumb}"
    );
    assert!(
        !dumb.contains('│') && !dumb.contains('╭') && !dumb.contains('─'),
        "TERM=dumb output must not contain box drawing:\n{dumb}"
    );
    assert!(
        dumb.contains("Alpha layout regression"),
        "plain output must still show the issue:\n{dumb}"
    );
}
