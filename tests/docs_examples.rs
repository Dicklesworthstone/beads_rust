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
//! Behavioral checks separately exercise automatic local JSONL import and its
//! opt-outs; matching example syntax alone does not verify prose semantics.
#![allow(clippy::pedantic, clippy::nursery)]

mod common;

use assert_cmd::Command;
use common::cli::{BrWorkspace, run_br};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;

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

fn run_documented_command(workspace: &BrWorkspace, command: &str) -> Value {
    let words = shell_words::split(command)
        .unwrap_or_else(|err| panic!("cannot split documented command `{command}`: {err}"));
    assert_eq!(
        words.first().map(String::as_str),
        Some("br"),
        "documented commands must start with `br`"
    );
    let output = run_br(workspace, &words[1..], "documented_command");
    let live: Value = serde_json::from_str(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "`{command}` did not print JSON: {err}\nstdout:\n{}\nstderr:\n{}",
            output.stdout, output.stderr
        )
    });
    assert_eq!(
        output.status.success(),
        live.get("error").is_none(),
        "`{command}` exit status disagrees with its result: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.stdout,
        output.stderr
    );
    live
}

#[test]
fn documented_json_examples_match_live_key_structure() {
    let examples = collect_examples();
    assert!(
        !examples.is_empty(),
        "no `<!-- from: ... -->` markers found in {DOCS:?}; the docs lost their captured examples"
    );

    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "doc"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut failures = Vec::new();
    for example in &examples {
        let live = run_documented_command(&workspace, &example.command);
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

#[test]
fn documented_auto_import_reads_local_edits_and_honors_opt_outs() {
    for disable_with_config in [false, true] {
        let workspace = BrWorkspace::new();
        let init = run_br(&workspace, ["init", "--prefix", "doc"], "init");
        assert!(init.status.success(), "init failed: {}", init.stderr);
        let created = run_documented_command(&workspace, "br create 'Original local title' --json");
        let id = created["id"].as_str().expect("created issue id");
        let jsonl_path = workspace.root.join(".beads/issues.jsonl");
        let exported = fs::read_to_string(&jsonl_path).expect("create auto-flushed JSONL");
        let mut row: Value = serde_json::from_str(&exported).expect("one exported issue");
        assert_eq!(row["id"], id);
        row["title"] = Value::String("Changed through the local interchange file".to_string());
        row["updated_at"] =
            Value::String((chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339());
        let edited = format!(
            "{}\n",
            serde_json::to_string(&row).expect("encode local edit")
        );
        fs::write(&jsonl_path, &edited).expect("write external local JSONL edit");

        // A deliberate real content change must remain absent from the DB when
        // import is disabled. Rewriting identical bytes would not test this.
        let list_command = if disable_with_config {
            let config = run_br(
                &workspace,
                ["config", "set", "sync.auto_import", "false"],
                "disable_auto_import",
            );
            assert!(config.status.success(), "config failed: {}", config.stderr);
            "br list --json"
        } else {
            "br list --no-auto-import --json"
        };
        let before = run_documented_command(&workspace, list_command);
        assert_eq!(before["total"], 1);
        assert_eq!(before["issues"][0]["id"], id);
        assert_eq!(before["issues"][0]["title"], "Original local title");
        assert_eq!(fs::read_to_string(&jsonl_path).unwrap(), edited);

        if disable_with_config {
            let config = run_br(
                &workspace,
                ["config", "set", "sync.auto_import", "true"],
                "enable_auto_import",
            );
            assert!(config.status.success(), "config failed: {}", config.stderr);
        }
        let after = run_documented_command(&workspace, "br list --json");
        assert_eq!(after["total"], 1);
        assert_eq!(after["issues"][0]["id"], id);
        assert_eq!(after["issues"][0]["title"], row["title"]);

        // A second command with import disabled proves the first imported the
        // value durably, rather than rendering directly from the JSONL file.
        let persisted = run_documented_command(&workspace, "br list --no-auto-import --json");
        assert_eq!(persisted["issues"][0]["title"], row["title"]);
        assert_eq!(fs::read_to_string(&jsonl_path).unwrap(), edited);
        eprintln!(
            "[docs_auto_import] config_opt_out={disable_with_config} id={id}: skipped edit, imported edit, persisted DB value, JSONL bytes preserved"
        );
    }
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
            .rfind(|cell| !cell.is_empty())
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
        let first_cell = line
            .trim()
            .trim_matches('|')
            .split('|')
            .next()
            .unwrap_or("");
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
            eprintln!(
                "[readme_examples] README.md:{line} `{command}` skipped: feature-gated subcommand not built"
            );
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
