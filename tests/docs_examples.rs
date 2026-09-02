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
