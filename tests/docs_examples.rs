//! Keep captured JSON examples in the docs honest.
//!
//! A doc may mark a fenced ```json block as captured from a real command:
//!
//! ```text
//! <!-- from: br show nope-123 --json -->
//! ```json
//! { ... }
//! ```
//! ```
//!
//! This test finds every such marker, runs the command in a scratch workspace,
//! and asserts that the JSON *key structure* (every object key path, in any
//! order) of the live output equals the documented block. Values are not
//! compared, so ids, timestamps, and messages may differ; renaming or dropping
//! a key fails. Add a marker whenever you paste command output into a doc.
//!
//! A second test checks that every `br ...` example in the README command
//! tables, and every flag in its Global Flags table, names a subcommand and
//! flags the built binary's `--help` actually lists.
#![allow(clippy::pedantic, clippy::nursery)]

use assert_cmd::Command;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

const DOCS: &[&str] = &["docs/ARCHITECTURE.md"];

struct CapturedExample {
    doc: &'static str,
    line: usize,
    command: String,
    json: Value,
}

fn collect_examples() -> Vec<CapturedExample> {
    let mut examples = Vec::new();
    for doc in DOCS {
        let text = fs::read_to_string(doc).unwrap_or_else(|err| panic!("read {doc}: {err}"));
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            let trimmed = lines[i].trim();
            if let Some(rest) = trimmed.strip_prefix("<!-- from:") {
                let command = rest.trim_end_matches("-->").trim().to_string();
                let marker_line = i + 1;
                let mut j = i + 1;
                while j < lines.len() && lines[j].trim().is_empty() {
                    j += 1;
                }
                assert!(
                    j < lines.len() && lines[j].trim().starts_with("```json"),
                    "{doc}:{marker_line}: `<!-- from: ... -->` must be followed by a ```json block"
                );
                let start = j + 1;
                let mut end = start;
                while end < lines.len() && !lines[end].trim().starts_with("```") {
                    end += 1;
                }
                let body = lines[start..end].join("\n");
                let json: Value = serde_json::from_str(&body).unwrap_or_else(|err| {
                    panic!("{doc}:{marker_line}: documented JSON does not parse: {err}")
                });
                examples.push(CapturedExample {
                    doc,
                    line: marker_line,
                    command,
                    json,
                });
                i = end;
            }
            i += 1;
        }
    }
    examples
}

/// Every object key path in a JSON value, e.g. `error.context.searched_id`.
fn key_paths(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                out.insert(path.clone());
                key_paths(child, &path, out);
            }
        }
        Value::Array(items) => {
            for item in items.iter().take(1) {
                key_paths(item, &format!("{prefix}[]"), out);
            }
        }
        _ => {}
    }
}

fn run_documented_command(workspace: &Path, command: &str) -> Value {
    let words = shell_words::split(command)
        .unwrap_or_else(|err| panic!("cannot split documented command `{command}`: {err}"));
    assert_eq!(
        words.first().map(String::as_str),
        Some("br"),
        "documented commands must start with `br`"
    );
    let mut cmd = Command::cargo_bin("br").expect("br binary");
    cmd.current_dir(workspace)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "error")
        .env("HOME", workspace)
        .args(&words[1..]);
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("BD_")
            || name.starts_with("BEADS_")
            || name.starts_with("BR_OUTPUT")
            || name.starts_with("TOON_")
        {
            cmd.env_remove(&key);
        }
    }
    let output = cmd.output().expect("run documented command");
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!(
            "`{command}` did not print JSON: {err}\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn documented_json_examples_match_live_key_structure() {
    let examples = collect_examples();
    assert!(
        !examples.is_empty(),
        "no `<!-- from: ... -->` markers found in {DOCS:?}; the docs lost their captured examples"
    );

    let temp = TempDir::new().expect("tempdir");
    Command::cargo_bin("br")
        .expect("br binary")
        .current_dir(temp.path())
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "error")
        .env("HOME", temp.path())
        .args(["init", "--prefix", "doc"])
        .assert()
        .success();

    let mut failures = Vec::new();
    for example in &examples {
        let live = run_documented_command(temp.path(), &example.command);
        let mut documented = BTreeSet::new();
        key_paths(&example.json, "", &mut documented);
        let mut observed = BTreeSet::new();
        key_paths(&live, "", &mut observed);
        if documented != observed {
            let missing: Vec<_> = documented.difference(&observed).cloned().collect();
            let extra: Vec<_> = observed.difference(&documented).cloned().collect();
            failures.push(format!(
                "{}:{} `{}`: documented keys missing from live output: {missing:?}; live keys not in doc: {extra:?}",
                example.doc, example.line, example.command
            ));
        }
        eprintln!(
            "[docs_examples] {}:{} `{}` -> {} key paths",
            example.doc,
            example.line,
            example.command,
            observed.len()
        );
    }
    assert!(
        failures.is_empty(),
        "documented examples drifted:\n{}",
        failures.join("\n")
    );
}

/// Subcommands that only exist in feature-gated builds; their README rows are
/// checked only when the built binary has them.
const FEATURE_GATED_SUBCOMMANDS: &[&str] = &["serve"];

fn br_help(args: &[&str]) -> Option<String> {
    let output = Command::cargo_bin("br")
        .expect("br binary")
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "error")
        .args(args)
        .arg("--help")
        .output()
        .expect("run br --help");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Every `br ...` example in the README command tables, as (line, command).
fn readme_table_examples(readme: &str) -> Vec<(usize, String)> {
    let mut examples = Vec::new();
    for (index, line) in readme.lines().enumerate() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') {
            continue;
        }
        let unescaped = trimmed.replace("\\|", "\u{1}");
        let Some(cell) = unescaped
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .last()
        else {
            continue;
        };
        let Some(code) = cell
            .strip_prefix('`')
            .and_then(|rest| rest.strip_suffix('`'))
        else {
            continue;
        };
        if !code.starts_with("br ") && code != "br" {
            continue;
        }
        // Keep only the br invocation in front of a shell pipe.
        let command = code.split(['\u{1}', '|']).next().unwrap_or("").trim();
        examples.push((index + 1, command.to_string()));
    }
    examples
}

/// `--flag` names mentioned in a README table's first column, e.g. the
/// Global Flags table, between `heading` and the next heading.
fn readme_flag_table(readme: &str, heading: &str) -> Vec<(usize, String)> {
    let mut flags = Vec::new();
    let mut inside = false;
    for (index, line) in readme.lines().enumerate() {
        if line.trim() == heading {
            inside = true;
            continue;
        }
        if inside && line.starts_with('#') {
            break;
        }
        if !inside || !line.trim().starts_with('|') {
            continue;
        }
        let first_cell = line.trim().trim_matches('|').split('|').next().unwrap_or("");
        for token in first_cell.split_whitespace() {
            let token = token.trim_matches('`');
            if let Some(flag) = token.strip_prefix("--") {
                let name: String = flag
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                // A table separator row (`|------|`) is not a flag.
                if name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
                    flags.push((index + 1, format!("--{name}")));
                }
            }
        }
    }
    flags
}

/// Every command and `--flag` the README tables show must exist in the built
/// binary's `--help`: the drift class this catches is a renamed or removed
/// subcommand or flag that a README row still advertises (the 2026-09-01
/// reality check found several).
#[test]
fn readme_table_examples_name_real_subcommands_and_flags() {
    let readme = fs::read_to_string("README.md").expect("read README.md");
    let examples = readme_table_examples(&readme);
    assert!(
        examples.len() > 40,
        "expected the README command tables to hold dozens of `br` examples, found {}",
        examples.len()
    );

    let mut failures = Vec::new();
    for (line, command) in &examples {
        let words = match shell_words::split(command) {
            Ok(words) => words,
            Err(err) => {
                failures.push(format!("README.md:{line} `{command}`: cannot split: {err}"));
                continue;
            }
        };
        let path: Vec<&str> = words
            .iter()
            .skip(1)
            .take_while(|word| !word.starts_with('-'))
            .take(3)
            .map(String::as_str)
            .collect();
        if path
            .first()
            .is_some_and(|first| FEATURE_GATED_SUBCOMMANDS.contains(first))
            && br_help(&path[..1]).is_none()
        {
            eprintln!("[readme_examples] README.md:{line} `{command}` skipped: feature-gated subcommand not built");
            continue;
        }
        // Longest subcommand prefix whose --help succeeds wins; positionals
        // such as issue ids are tolerated by clap when --help is present.
        let help = (1..=path.len())
            .rev()
            .find_map(|depth| br_help(&path[..depth]).map(|help| (depth, help)));
        let Some((depth, help)) = help else {
            failures.push(format!(
                "README.md:{line} `{command}`: no subcommand `{}` in the built binary",
                path.first().copied().unwrap_or("")
            ));
            continue;
        };
        for word in words.iter().skip(1) {
            let Some(flag) = word.strip_prefix("--") else {
                continue;
            };
            let name = flag.split('=').next().unwrap_or(flag);
            if !help.contains(&format!("--{name}")) {
                failures.push(format!(
                    "README.md:{line} `{command}`: `br {} --help` does not list --{name}",
                    path[..depth].join(" ")
                ));
            }
        }
        eprintln!(
            "[readme_examples] README.md:{line} `{command}` -> br {} --help ok",
            path[..depth].join(" ")
        );
    }

    let global_help = br_help(&[]).expect("br --help");
    let global_flags = readme_flag_table(&readme, "### Global Flags");
    assert!(
        global_flags.len() >= 5,
        "Global Flags table not found or empty in README.md"
    );
    for (line, flag) in &global_flags {
        if !global_help.contains(flag.as_str()) {
            failures.push(format!(
                "README.md:{line}: Global Flags table lists {flag}, which `br --help` does not"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "README command tables drifted from the binary:\n{}",
        failures.join("\n")
    );
}
