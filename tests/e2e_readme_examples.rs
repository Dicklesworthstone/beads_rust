//! Every `br` command in README.md's ```bash blocks runs in a scratch
//! workspace and must exit as documented (bead beads_rust-wqmw.2).
//!
//! README examples drifted from the binary without anyone noticing because
//! nothing executed them. This test parses the README, gives every fenced
//! bash block its own initialized workspace (a block that runs `br init`
//! itself gets an empty one), replaces placeholder issue ids such as
//! `br-abc123` with issues it creates on demand, and runs each `br` command
//! through the real binary. A command whose exit code differs from the
//! documented expectation fails the test with the README line number, the
//! command, and its output. It also asserts that the README's `config.yaml`
//! example changes behavior the way the prose claims.
//!
//! Recipe lines of the form `plan="$(br ... --robot)"` and
//! `plan_sha256="$(printf '%s\n' "$plan" | jq -r .plan_sha256)"` are
//! replayed (the br command runs, the jq step is evaluated in-process) so
//! the `--apply --expect-plan-sha256 "$plan_sha256"` examples run for real.
//! A block that uses `br capacity exempt` gets the README's own exemption
//! policy example installed first. Not executed: `br serve` (long-running)
//! and anything after a `|`.
mod common;

use common::cli::{BrRun, BrWorkspace, parse_created_id, run_br_with_env};
use regex::Regex;
use std::collections::BTreeMap;
use std::fs;
use std::sync::LazyLock;

const README: &str = include_str!("../README.md");
const POOL_TITLES: [&str; 4] = [
    "README example issue one",
    "README example issue two",
    "README example issue three",
    "README example issue four",
];

static PLACEHOLDER_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bbr-[A-Za-z0-9]{1,8}\b").expect("placeholder id regex"));
/// `$name` references to recipe variables.
static SHELL_VARIABLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$(\w+)\b").expect("shell variable regex"));
/// `name="$(br ...)"`: capture a br command's stdout into a shell variable.
static ASSIGN_FROM_BR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^(\w+)="\$\((br [^)]*)\)"$"#).expect("assignment regex"));
/// `name="$(printf '%s\n' "$other" | jq -r .field)"`: pull one JSON field out
/// of a captured variable.
static EXTRACT_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^(\w+)="\$\(printf '%s\\n' "\$(\w+)" \| jq -r \.(\w+)\)"$"#)
        .expect("extract regex")
});

/// One README command with the line it starts on.
#[derive(Debug, Clone)]
struct Example {
    line: usize,
    text: String,
    kind: ExampleKind,
}

/// README recipes assign a plan to a variable, pull its hash with jq, and
/// pass the hash to `--apply`; the test replays those steps.
#[derive(Debug, Clone)]
enum ExampleKind {
    Command,
    /// `var = stdout of the br command in `text``
    Assign {
        var: String,
    },
    /// `var = JSON field of another variable`
    Extract {
        var: String,
        from: String,
        field: String,
    },
}

/// Commands the README documents as non-zero, or that cannot run here.
fn expectation(command: &str) -> Expectation {
    // README examples may carry env assignments first (`RUST_LOG=error br
    // serve ...`); look at the command after them.
    let program = command
        .split_whitespace()
        .skip_while(|token| token.contains('=') && !token.starts_with('-'))
        .collect::<Vec<_>>()
        .join(" ");
    if program.starts_with("br serve") {
        return Expectation::Skip("br serve runs until stdin closes");
    }
    Expectation::Exit(0)
}

#[derive(Debug, Clone, Copy)]
enum Expectation {
    Exit(i32),
    Skip(&'static str),
}

/// Fenced bash blocks of the README as (block start line, commands).
fn readme_bash_blocks() -> Vec<(usize, Vec<Example>)> {
    let mut blocks = Vec::new();
    let mut current: Option<(usize, Vec<Example>)> = None;
    let mut pending: Option<Example> = None;
    for (index, raw) in README.lines().enumerate() {
        let line_no = index + 1;
        let trimmed = raw.trim();
        if let Some((_, commands)) = current.as_mut() {
            if trimmed.starts_with("```") {
                if let Some(example) = pending.take() {
                    commands.push(example);
                }
                let block = current.take().expect("open block");
                if !block.1.is_empty() {
                    blocks.push(block);
                }
                continue;
            }
            if let Some(example) = pending.as_mut() {
                let piece = trimmed.trim_end_matches('\\').trim();
                example.text.push(' ');
                example.text.push_str(piece);
                if !trimmed.ends_with('\\') {
                    commands.push(pending.take().expect("pending example"));
                }
                continue;
            }
            if let Some(captures) = ASSIGN_FROM_BR.captures(trimmed) {
                commands.push(Example {
                    line: line_no,
                    text: captures[2].to_string(),
                    kind: ExampleKind::Assign {
                        var: captures[1].to_string(),
                    },
                });
            } else if let Some(captures) = EXTRACT_FIELD.captures(trimmed) {
                commands.push(Example {
                    line: line_no,
                    text: trimmed.to_string(),
                    kind: ExampleKind::Extract {
                        var: captures[1].to_string(),
                        from: captures[2].to_string(),
                        field: captures[3].to_string(),
                    },
                });
            } else if is_br_command(trimmed) {
                let piece = trimmed.trim_end_matches('\\').trim().to_string();
                let example = Example {
                    line: line_no,
                    text: piece,
                    kind: ExampleKind::Command,
                };
                if trimmed.ends_with('\\') {
                    pending = Some(example);
                } else {
                    commands.push(example);
                }
            }
        } else if trimmed == "```bash" {
            current = Some((line_no, Vec::new()));
        }
    }
    blocks
}

/// A line whose command is `br` (optionally preceded by `KEY=VALUE` env
/// assignments). Assignments and `$(br ...)` substitutions are not commands.
fn is_br_command(line: &str) -> bool {
    let mut words = line.split_whitespace();
    loop {
        match words.next() {
            Some(word)
                if word.contains('=') && !word.starts_with('-') && !word.starts_with('"') =>
            {
                if word.starts_with("br") {
                    return false;
                }
            }
            Some("br") => return true,
            _ => return false,
        }
    }
}

/// Strip a trailing `# comment` and everything from the first pipe, both
/// outside quotes.
fn executable_part(text: &str) -> String {
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut previous = ' ';
    for ch in text.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '|' => break,
            None if ch == '#' && previous.is_whitespace() => break,
            _ => {}
        }
        out.push(ch);
        previous = ch;
    }
    out.trim().to_string()
}

struct BlockRunner {
    workspace: BrWorkspace,
    initialized: bool,
    pool: Vec<String>,
    placeholders: BTreeMap<String, String>,
    /// Shell variables set by README recipe lines (`plan`, `plan_sha256`).
    vars: BTreeMap<String, String>,
}

impl BlockRunner {
    fn new(needs_init: bool, label: &str) -> Self {
        let workspace = BrWorkspace::new();
        let mut runner = Self {
            workspace,
            initialized: false,
            pool: Vec::new(),
            placeholders: BTreeMap::new(),
            vars: BTreeMap::new(),
        };
        if needs_init {
            let init = runner.run(&["init", "--prefix", "br"], &[], &format!("{label}_init"));
            assert!(init.status.success(), "setup init failed: {}", init.stderr);
            runner.initialized = true;
        }
        runner
    }

    /// Install the README's own capacity-exemption policy so its
    /// `br capacity exempt` example runs in the context the prose gives it.
    fn install_exemption_policy(&self) {
        let policy = README
            .split("```yaml\n")
            .skip(1)
            .filter_map(|rest| rest.split("```").next())
            .find(|block| block.contains("exemptions:") && block.contains("providers:"))
            .expect("README has a capacity exemptions policy example");
        fs::write(
            self.workspace.root.join(".beads").join("policy.yaml"),
            policy,
        )
        .expect("write README policy example");
    }

    fn run(&self, args: &[&str], env: &[(String, String)], label: &str) -> BrRun {
        let env: Vec<(String, String)> = env.to_vec();
        run_br_with_env(&self.workspace, args.iter().copied(), env, label)
    }

    /// Map a README placeholder id to a real issue, creating the pool of
    /// example issues on first use.
    fn resolve_placeholder(&mut self, placeholder: &str, label: &str) -> String {
        if let Some(id) = self.placeholders.get(placeholder) {
            return id.clone();
        }
        assert!(
            self.initialized,
            "README uses {placeholder} before the workspace is initialized ({label})"
        );
        if self.pool.is_empty() {
            for (index, title) in POOL_TITLES.iter().enumerate() {
                let created = self.run(&["create", title], &[], &format!("{label}_pool{index}"));
                assert!(
                    created.status.success(),
                    "pool create failed: {}",
                    created.stderr
                );
                let id = parse_created_id(&created.stdout);
                assert!(
                    !id.is_empty(),
                    "pool create printed no id: {}",
                    created.stdout
                );
                self.pool.push(id);
            }
        }
        let next = self.placeholders.len() % self.pool.len();
        let id = self.pool[next].clone();
        self.placeholders
            .insert(placeholder.to_string(), id.clone());
        id
    }

    fn substitute(&mut self, text: &str, label: &str) -> String {
        let with_email = text.replace("$(git config user.email)", "alice@example.com");
        // Whole-name variable references only, so `$plan` never eats the
        // prefix of `$plan_sha256`.
        let with_email = SHELL_VARIABLE
            .replace_all(&with_email, |captures: &regex::Captures<'_>| {
                self.vars
                    .get(&captures[1])
                    .cloned()
                    .unwrap_or_else(|| captures[0].to_string())
            })
            .into_owned();
        let mut out = String::new();
        let mut last = 0;
        for found in PLACEHOLDER_ID.find_iter(&with_email) {
            out.push_str(&with_email[last..found.start()]);
            let real = self.resolve_placeholder(found.as_str(), label);
            out.push_str(&real);
            last = found.end();
        }
        out.push_str(&with_email[last..]);
        out
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn readme_bash_examples_exit_as_documented() {
    let _log = common::test_log("readme_bash_examples_exit_as_documented");
    let blocks = readme_bash_blocks();
    assert!(
        blocks.len() >= 20,
        "README should have many bash blocks with br commands, found {}",
        blocks.len()
    );

    let mut executed = 0_usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for (block_line, commands) in &blocks {
        let block_has_init = commands.iter().any(|c| c.text.starts_with("br init"));
        let label = format!("readme_block_{block_line}");
        let mut runner = BlockRunner::new(!block_has_init, &label);
        if commands
            .iter()
            .any(|c| c.text.starts_with("br capacity exempt"))
        {
            runner.install_exemption_policy();
        }
        for example in commands {
            if let ExampleKind::Extract { var, from, field } = &example.kind {
                let source = runner.vars.get(from).cloned().unwrap_or_default();
                let start = source.find(['{', '[']).unwrap_or(source.len());
                let parsed: serde_json::Value = serde_json::from_str(source[start..].trim())
                    .unwrap_or_else(|err| {
                        panic!(
                            "README.md:{}: `${from}` is not JSON ({err}):\n{source}",
                            example.line
                        )
                    });
                let value = parsed[field.as_str()]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| parsed[field.as_str()].to_string());
                runner.vars.insert(var.clone(), value);
                continue;
            }
            let text = executable_part(&example.text);
            let expectation = expectation(&text);
            if let Expectation::Skip(reason) = expectation {
                skipped.push(format!("README.md:{}: `{}` ({reason})", example.line, text));
                continue;
            }
            let substituted = runner.substitute(&text, &label);
            let words = shell_words::split(&substituted).unwrap_or_else(|err| {
                panic!("README.md:{}: cannot parse `{text}`: {err}", example.line)
            });
            let mut env: Vec<(String, String)> = vec![("EDITOR".to_string(), "true".to_string())];
            let mut argv: Vec<&str> = Vec::new();
            let mut seen_br = false;
            for word in &words {
                if !seen_br {
                    if word == "br" {
                        seen_br = true;
                    } else if let Some((key, value)) = word.split_once('=') {
                        env.push((key.to_string(), value.to_string()));
                    }
                    continue;
                }
                argv.push(word.as_str());
            }
            assert!(
                seen_br,
                "README.md:{}: no br command in `{text}`",
                example.line
            );
            let run = runner.run(&argv, &env, &format!("{label}_l{}", example.line));
            if argv.first() == Some(&"init") {
                runner.initialized = runner.initialized || run.status.success();
            }
            if let ExampleKind::Assign { var } = &example.kind {
                runner
                    .vars
                    .insert(var.clone(), run.stdout.trim().to_string());
            }
            executed += 1;
            let Expectation::Exit(expected) = expectation else {
                unreachable!()
            };
            if run.status.code() != Some(expected) {
                failures.push(format!(
                    "README.md:{}: `{}` exited {:?}, expected {expected}\n  stdout: {}\n  stderr: {}",
                    example.line,
                    substituted,
                    run.status.code(),
                    run.stdout.trim().lines().take(4).collect::<Vec<_>>().join(" | "),
                    run.stderr.trim().lines().take(6).collect::<Vec<_>>().join(" | ")
                ));
            }
        }
    }
    eprintln!(
        "[readme] executed {executed} commands; skipped {}",
        skipped.len()
    );
    for skip in &skipped {
        eprintln!("[readme] skipped {skip}");
    }
    assert!(
        failures.is_empty(),
        "{} README command(s) did not exit as documented:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        executed >= 60,
        "expected at least 60 README commands to run, executed {executed}"
    );
}

/// The README's `.beads/config.yaml` example must change behavior the way
/// the prose says: new ids take the prefix, and `br create` without
/// `--priority` uses `default_priority`.
#[test]
fn readme_config_example_changes_prefix_and_default_priority() {
    let _log = common::test_log("readme_config_example_changes_prefix_and_default_priority");
    let yaml_blocks: Vec<&str> = README
        .split("```yaml\n")
        .skip(1)
        .filter_map(|rest| rest.split("```").next())
        .collect();
    let config = yaml_blocks
        .iter()
        .find(|block| block.contains("issue_prefix:") && block.contains("default_priority:"))
        .expect("README has a config.yaml example with issue_prefix and default_priority");
    let expected_prefix = config
        .lines()
        .find_map(|line| line.trim().strip_prefix("issue_prefix:"))
        .map(|value| value.trim().trim_matches('"').to_string())
        .expect("issue_prefix value");
    let expected_priority: i64 = config
        .lines()
        .find_map(|line| line.trim().strip_prefix("default_priority:"))
        .and_then(|value| value.split('#').next())
        .and_then(|value| value.trim().parse().ok())
        .expect("default_priority value");

    let workspace = BrWorkspace::new();
    let env: Vec<(String, String)> = Vec::new();
    let init = run_br_with_env(&workspace, ["init"], env.clone(), "readme_config_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    fs::write(workspace.root.join(".beads").join("config.yaml"), config)
        .expect("write README config example");

    let created = run_br_with_env(
        &workspace,
        ["create", "Uses the documented config", "--json"],
        env,
        "readme_config_create",
    );
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );
    let payload = created.stdout.trim();
    let start = payload.find('{').expect("json object in create output");
    let value: serde_json::Value = serde_json::from_str(&payload[start..]).expect("create json");
    let id = value["id"].as_str().expect("created id");
    assert!(
        id.starts_with(&format!("{expected_prefix}-")),
        "README config example promises prefix `{expected_prefix}`, got id {id}"
    );
    assert_eq!(
        value["priority"].as_i64(),
        Some(expected_priority),
        "README config example promises default_priority {expected_priority}: {value}"
    );
}
