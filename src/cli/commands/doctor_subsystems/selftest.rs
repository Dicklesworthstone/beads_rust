//! `br doctor --selftest`: drive the installed binary through a full issue
//! lifecycle in a throwaway workspace and report a platform receipt.
//!
//! Platform-specific breakage (Windows panicking on every command, GitHub
//! #438 and #439; the WSL2 DrvFS `renameat2` refusal, #419; the glibc floor on
//! Debian 12, #444) reached users because nothing exercised the shipped binary
//! end to end on the host it was installed on. Every step here spawns the
//! current executable, so what passes is exactly what the user runs, and the
//! receipt names the platform and filesystem facts those bugs depended on.
//!
//! The selftest never reads or writes the caller's `.beads/`: every command
//! runs inside a fresh temporary workspace with the workspace-selecting
//! environment variables removed, and the directory is deleted afterwards
//! unless `--keep` is given.

use crate::cli::DoctorArgs;
use crate::cli::commands::doctor_subsystems::exit_codes::DoctorExitCode;
use crate::error::{BeadsError, Result};
use crate::output::OutputContext;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

/// Receipt schema identifier.
pub const SCHEMA_VERSION: &str = "br.doctor.selftest.v1";

/// Everything `br doctor --selftest --json` prints.
#[derive(Debug, Serialize)]
pub struct SelftestReceipt {
    pub schema_version: &'static str,
    pub ok: bool,
    pub elapsed_ms: u128,
    pub workspace: String,
    pub kept: bool,
    pub platform: PlatformFacts,
    pub fs: FilesystemFacts,
    pub engine: EngineFacts,
    pub steps: Vec<StepReceipt>,
}

/// The storage engine this binary was built against.
#[derive(Debug, Serialize)]
pub struct EngineFacts {
    pub name: &'static str,
    pub version: &'static str,
}

/// Host facts that platform bugs have depended on.
#[derive(Debug, Serialize)]
pub struct PlatformFacts {
    pub os: &'static str,
    pub arch: &'static str,
    pub family: &'static str,
    pub br_version: &'static str,
    pub executable: String,
}

/// Filesystem behaviour probed in the temporary workspace.
#[derive(Debug, Serialize)]
pub struct FilesystemFacts {
    pub temp_root: String,
    /// `None` when the probe itself failed.
    pub case_sensitive: Option<bool>,
    /// `supported`, `unsupported` (the witness-checked fallback path is in
    /// use, as on WSL2 DrvFS), `replaced` (the flag was silently ignored),
    /// `unknown` on platforms without a probe, or `error: ...`.
    pub rename_noreplace: String,
}

/// One executed step.
#[derive(Debug, Serialize)]
pub struct StepReceipt {
    pub name: String,
    pub ok: bool,
    pub ms: u128,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

struct Runner {
    exe: PathBuf,
    root: PathBuf,
    steps: Vec<StepReceipt>,
}

impl Runner {
    fn spawn(&self, args: &[&str]) -> std::io::Result<Output> {
        let mut cmd = Command::new(&self.exe);
        cmd.args(args)
            .current_dir(&self.root)
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "error");
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
        // Pin discovery to the throwaway workspace. The temp root can sit
        // inside a real workspace (a TMPDIR under a checkout), and without
        // this the child `br init` would walk up to that tracker and refuse
        // to touch it, failing the selftest for a reason that has nothing to
        // do with this binary or filesystem.
        cmd.env("BEADS_DIR", self.root.join(".beads"));
        cmd.output()
    }

    /// Run one step; `check` inspects the finished process and describes the
    /// failure when the step did not do what the lifecycle expects.
    fn step(
        &mut self,
        name: &str,
        args: &[&str],
        check: impl FnOnce(&Output) -> std::result::Result<(), String>,
    ) -> Option<Output> {
        let started = Instant::now();
        let command = format!("br {}", args.join(" "));
        let outcome = self.spawn(args);
        let ms = started.elapsed().as_millis();
        let (ok, detail, output) = match outcome {
            Ok(output) => match check(&output) {
                Ok(()) => (true, None, Some(output)),
                Err(reason) => {
                    let detail = format!(
                        "{reason}; exit={}; stdout: {}; stderr: {}",
                        output
                            .status
                            .code()
                            .map_or_else(|| "signal".to_string(), |code| code.to_string()),
                        tail(&output.stdout),
                        tail(&output.stderr)
                    );
                    (false, Some(detail), Some(output))
                }
            },
            Err(err) => (false, Some(format!("spawn failed: {err}")), None),
        };
        tracing::info!(step = name, ok, ms, "selftest.step");
        self.steps.push(StepReceipt {
            name: name.to_string(),
            ok,
            ms,
            command,
            detail,
        });
        output.filter(|_| ok)
    }

    /// A step that only has to exit successfully.
    fn ok(&mut self, name: &str, args: &[&str]) -> Option<Output> {
        self.step(name, args, exit_success)
    }

    /// A step that must exit successfully and print JSON.
    fn json(&mut self, name: &str, args: &[&str]) -> Option<Value> {
        self.step(name, args, |output| {
            exit_success(output)?;
            parse_json(&output.stdout).map(|_| ())
        })
        .and_then(|output| parse_json(&output.stdout).ok())
    }

    fn all_ok(&self) -> bool {
        self.steps.iter().all(|step| step.ok)
    }
}

fn exit_success(output: &Output) -> std::result::Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        Err("expected exit 0".to_string())
    }
}

fn parse_json(stdout: &[u8]) -> std::result::Result<Value, String> {
    serde_json::from_str(String::from_utf8_lossy(stdout).trim())
        .map_err(|err| format!("stdout is not JSON: {err}"))
}

fn tail(bytes: &[u8]) -> String {
    const LIMIT: usize = 240;
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let start = text
        .char_indices()
        .rev()
        .nth(LIMIT - 1)
        .map_or(0, |(index, _)| index);
    format!("...{}", &text[start..])
}

/// Every `id` string reachable from the value, in any nesting.
fn collect_ids(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("id") {
                out.insert(id.clone());
            }
            for child in map.values() {
                collect_ids(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_ids(item, out);
            }
        }
        _ => {}
    }
}

fn ids_in(value: &Value) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    collect_ids(value, &mut ids);
    ids
}

/// The id of a freshly created issue, whatever envelope `create --json`
/// used.
fn created_id(value: &Value) -> Option<String> {
    ids_in(value).into_iter().next()
}

fn first_string_array(value: &Value, key: &str) -> Option<Vec<String>> {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(items)) = map.get(key) {
                return Some(
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect(),
                );
            }
            map.values()
                .find_map(|child| first_string_array(child, key))
        }
        Value::Array(items) => items.iter().find_map(|item| first_string_array(item, key)),
        _ => None,
    }
}

fn probe_case_sensitivity(root: &Path) -> Option<bool> {
    let upper = root.join("Case-Probe.txt");
    std::fs::write(&upper, b"probe").ok()?;
    let lower_exists = root.join("case-probe.txt").exists();
    let _ = std::fs::remove_file(&upper);
    Some(!lower_exists)
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn probe_rename_noreplace(root: &Path) -> String {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    use rustix::io::Errno;

    let from = root.join("rename-probe-from");
    let to = root.join("rename-probe-to");
    if let Err(err) = std::fs::write(&from, b"from").and_then(|()| std::fs::write(&to, b"to")) {
        return format!("error: cannot create probe files: {err}");
    }
    let verdict = match renameat_with(CWD, &from, CWD, &to, RenameFlags::NOREPLACE) {
        Err(Errno::EXIST) => "supported".to_string(),
        // `NOTSUP` and `OPNOTSUPP` share a value on Linux, so a guard instead
        // of an or-pattern keeps the arm reachable on every target.
        Err(err)
            if err == Errno::INVAL
                || err == Errno::NOSYS
                || err == Errno::NOTSUP
                || err == Errno::OPNOTSUPP =>
        {
            "unsupported".to_string()
        }
        Err(err) => format!("error: {err}"),
        Ok(()) => "replaced".to_string(),
    };
    let _ = std::fs::remove_file(&from);
    let _ = std::fs::remove_file(&to);
    verdict
}

#[cfg(windows)]
fn probe_rename_noreplace(root: &Path) -> String {
    let from = root.join("rename-probe-from");
    let to = root.join("rename-probe-to");
    if let Err(err) = std::fs::write(&from, b"from").and_then(|()| std::fs::write(&to, b"to")) {
        return format!("error: cannot create probe files: {err}");
    }
    let verdict = match crate::sync::rename_path_no_replace_windows(&from, &to) {
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => "supported".to_string(),
        Err(err) => format!("error: {err}"),
        Ok(()) => "replaced".to_string(),
    };
    let _ = std::fs::remove_file(&from);
    let _ = std::fs::remove_file(&to);
    verdict
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
fn probe_rename_noreplace(_root: &Path) -> String {
    "unknown".to_string()
}

fn probe_filesystem(root: &Path) -> FilesystemFacts {
    FilesystemFacts {
        temp_root: root.display().to_string(),
        case_sensitive: probe_case_sensitivity(root),
        rename_noreplace: probe_rename_noreplace(root),
    }
}

/// The lifecycle every supported platform must be able to run.
#[allow(clippy::too_many_lines)]
fn run_lifecycle(runner: &mut Runner) {
    runner.ok("version", &["--version"]);
    if runner.ok("init", &["init", "--prefix", "self"]).is_none() {
        return;
    }
    runner.step("init wrote the database", &["where"], |output| {
        exit_success(output)?;
        let beads = runner_root_from_where(output);
        if beads.join("beads.db").is_file() {
            Ok(())
        } else {
            Err(format!("no beads.db under {}", beads.display()))
        }
    });

    let Some(a) = runner
        .json(
            "create A",
            &[
                "create",
                "Alpha selftest issue",
                "--priority",
                "1",
                "--json",
            ],
        )
        .and_then(|value| created_id(&value))
    else {
        return;
    };
    let Some(b) = runner
        .json("create B", &["create", "Beta selftest issue", "--json"])
        .and_then(|value| created_id(&value))
    else {
        return;
    };
    let Some(epic) = runner
        .json(
            "create epic",
            &["create", "Selftest epic", "--type", "epic", "--json"],
        )
        .and_then(|value| created_id(&value))
    else {
        return;
    };
    runner.step("ids carry the prefix", &["show", &a, "--json"], |output| {
        exit_success(output)?;
        if a.starts_with("self-") && b.starts_with("self-") {
            Ok(())
        } else {
            Err(format!("ids {a} / {b} do not start with `self-`"))
        }
    });
    runner.ok("quick capture", &["q", "Quick selftest note"]);

    runner.ok("dep add blocks", &["dep", "add", &b, &a]);
    runner.ok(
        "dep add parent-child",
        &["dep", "add", &b, &epic, "--type", "parent-child"],
    );
    runner.step("ready shows A, not B", &["ready", "--json"], |output| {
        exit_success(output)?;
        let ids = ids_in(&parse_json(&output.stdout)?);
        if ids.contains(&a) && !ids.contains(&b) {
            Ok(())
        } else {
            Err(format!("ready ids: {ids:?}"))
        }
    });
    runner.step("blocked shows B", &["blocked", "--json"], |output| {
        exit_success(output)?;
        let ids = ids_in(&parse_json(&output.stdout)?);
        if ids.contains(&b) {
            Ok(())
        } else {
            Err(format!("blocked ids: {ids:?}"))
        }
    });
    runner.ok("dep tree", &["dep", "tree", &b]);
    runner.ok("dep cycles", &["dep", "cycles"]);

    runner.ok("label add", &["label", "add", &a, "backend", "urgent"]);
    runner.ok("label remove", &["label", "remove", &a, "urgent"]);
    runner.step("labels persisted", &["show", &a, "--json"], |output| {
        exit_success(output)?;
        let labels = first_string_array(&parse_json(&output.stdout)?, "labels").unwrap_or_default();
        if labels == ["backend"] {
            Ok(())
        } else {
            Err(format!("labels: {labels:?}"))
        }
    });

    runner.ok(
        "comments add",
        &["comments", "add", &a, "selftest comment body"],
    );
    runner.step(
        "comments list",
        &["comments", "list", &a, "--json"],
        |output| {
            exit_success(output)?;
            if String::from_utf8_lossy(&output.stdout).contains("selftest comment body") {
                Ok(())
            } else {
                Err("comment body missing".to_string())
            }
        },
    );

    runner.ok(
        "update claim",
        &[
            "update",
            &a,
            "--status",
            "in_progress",
            "--assignee",
            "selftest",
        ],
    );
    runner.ok("update --claim", &["update", &epic, "--claim"]);
    runner.step("list envelope", &["list", "--json"], |output| {
        exit_success(output)?;
        let value = parse_json(&output.stdout)?;
        if value.get("issues").is_some() && value.get("total").is_some() {
            Ok(())
        } else {
            Err("list --json lacks issues/total".to_string())
        }
    });
    runner.step("search", &["search", "Alpha", "--json"], |output| {
        exit_success(output)?;
        if ids_in(&parse_json(&output.stdout)?).contains(&a) {
            Ok(())
        } else {
            Err("search did not return A".to_string())
        }
    });
    runner.ok("count", &["count", "--by", "status"]);
    runner.ok(
        "list priority range",
        &["list", "--status", "open", "--priority", "0-1"],
    );

    runner.ok("defer", &["defer", &b, "--until", "tomorrow"]);
    runner.ok("undefer", &["undefer", &b]);
    runner.ok("close A", &["close", &a, "--reason", "selftest done"]);
    runner.step(
        "ready shows B after close",
        &["ready", "--json"],
        |output| {
            exit_success(output)?;
            if ids_in(&parse_json(&output.stdout)?).contains(&b) {
                Ok(())
            } else {
                Err("B still not ready after closing its blocker".to_string())
            }
        },
    );
    runner.ok("reopen A", &["reopen", &a, "--reason", "selftest reopen"]);
    runner.ok("close A again", &["close", &a, "--reason", "selftest done"]);
    runner.ok("epic status", &["epic", "status"]);
    runner.ok("stats", &["stats"]);

    runner.step("auto-flush wrote issues.jsonl", &["where"], |output| {
        exit_success(output)?;
        let jsonl = runner_root_from_where(output).join("issues.jsonl");
        match std::fs::read_to_string(&jsonl) {
            Ok(text) if text.contains(&a) => Ok(()),
            Ok(_) => Err(format!("{} does not mention {a}", jsonl.display())),
            Err(err) => Err(format!("{}: {err}", jsonl.display())),
        }
    });
    runner.ok("sync --flush-only", &["sync", "--flush-only"]);
    runner.ok("sync --status", &["sync", "--status"]);
    runner.ok("sync --import-only", &["sync", "--import-only"]);
    runner.ok("sync --witness", &["sync", "--witness", "--robot"]);
    runner.step("bare sync refused", &["sync"], |output| {
        if output.status.success() {
            Err("bare `br sync` must exit non-zero".to_string())
        } else {
            Ok(())
        }
    });
    runner.ok("history list", &["history", "list"]);
    runner.step(
        "doctor --json",
        &["doctor", "--json"],
        |output| match output.status.code() {
            Some(0 | 1) => parse_json(&output.stdout).map(|_| ()),
            other => Err(format!("doctor exit {other:?}")),
        },
    );

    runner.ok("config set", &["config", "set", "default_priority", "1"]);
    runner.step(
        "config get",
        &["config", "get", "default_priority"],
        |output| {
            exit_success(output)?;
            if String::from_utf8_lossy(&output.stdout).contains('1') {
                Ok(())
            } else {
                Err("config get did not echo the value".to_string())
            }
        },
    );
    runner.ok("delete epic", &["delete", &epic, "--force"]);
    runner.step(
        "deleted issue gone from list",
        &["list", "--json"],
        |output| {
            exit_success(output)?;
            if ids_in(&parse_json(&output.stdout)?).contains(&epic) {
                Err("deleted issue still listed".to_string())
            } else {
                Ok(())
            }
        },
    );
}

/// The `.beads` directory `br where` printed (first non-empty stdout line).
fn runner_root_from_where(output: &Output) -> PathBuf {
    let text = String::from_utf8_lossy(&output.stdout);
    PathBuf::from(
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or(".beads"),
    )
}

fn render_text(receipt: &SelftestReceipt, ctx: &OutputContext) {
    if !ctx.is_quiet() {
        for step in &receipt.steps {
            let mark = if step.ok { "ok  " } else { "FAIL" };
            println!("{mark} {:<36} {:>5} ms", step.name, step.ms);
            if let Some(detail) = &step.detail {
                println!("     {detail}");
            }
        }
        println!(
            "platform: {}/{} ({}); br {}; fs: case_sensitive={} rename_noreplace={}",
            receipt.platform.os,
            receipt.platform.arch,
            receipt.platform.family,
            receipt.platform.br_version,
            receipt
                .fs
                .case_sensitive
                .map_or_else(|| "unknown".to_string(), |v| v.to_string()),
            receipt.fs.rename_noreplace
        );
    }
    let passed = receipt.steps.iter().filter(|step| step.ok).count();
    let state = if receipt.ok { "ok" } else { "FAILED" };
    println!(
        "selftest {state}: {passed}/{} steps in {} ms; workspace {} ({})",
        receipt.steps.len(),
        receipt.elapsed_ms,
        receipt.workspace,
        if receipt.kept { "kept" } else { "removed" }
    );
}

/// Run the selftest and print its receipt.
///
/// Exits with [`DoctorExitCode::FindingsPresent`] when any step failed, so a
/// canary can gate on the exit code alone.
///
/// # Errors
///
/// Returns an error when the throwaway workspace cannot be created or the
/// current executable cannot be located; step failures are reported in the
/// receipt instead.
pub fn execute(args: &DoctorArgs, ctx: &OutputContext) -> Result<()> {
    let started = Instant::now();
    let exe = std::env::current_exe().map_err(|err| {
        BeadsError::validation(
            "selftest",
            format!("cannot locate the br executable: {err}"),
        )
    })?;
    let mut builder = tempfile::Builder::new();
    builder.prefix("br-selftest-");
    let temp = match &args.selftest_dir {
        Some(dir) => builder.tempdir_in(dir),
        None => builder.tempdir(),
    }
    .map_err(|err| {
        BeadsError::validation(
            "selftest",
            format!("cannot create a throwaway workspace: {err}"),
        )
    })?;
    let root = temp.path().to_path_buf();
    tracing::info!(workspace = %root.display(), "selftest.start");

    let fs = probe_filesystem(&root);
    let mut runner = Runner {
        exe: exe.clone(),
        root: root.clone(),
        steps: Vec::new(),
    };
    run_lifecycle(&mut runner);
    let ok = runner.all_ok();

    let kept = args.keep;
    let workspace = if kept {
        temp.keep().display().to_string()
    } else {
        let path = root.display().to_string();
        drop(temp);
        path
    };

    let receipt = SelftestReceipt {
        schema_version: SCHEMA_VERSION,
        ok,
        elapsed_ms: started.elapsed().as_millis(),
        workspace,
        kept,
        platform: PlatformFacts {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            family: std::env::consts::FAMILY,
            br_version: env!("CARGO_PKG_VERSION"),
            executable: exe.display().to_string(),
        },
        fs,
        engine: EngineFacts {
            name: "frankensqlite",
            version: crate::cli::commands::doctor_subsystems::engine::engine_version(),
        },
        steps: runner.steps,
    };

    if ctx.is_json() {
        ctx.json_pretty(&receipt);
    } else {
        render_text(&receipt, ctx);
    }
    if ok {
        Ok(())
    } else {
        crate::shutdown::exit_process(DoctorExitCode::FindingsPresent.as_i32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_ids_walks_every_envelope_shape() {
        let list = serde_json::json!({"issues": [{"id": "x-1"}, {"id": "x-2"}], "total": 2});
        assert_eq!(
            ids_in(&list).into_iter().collect::<Vec<_>>(),
            vec!["x-1", "x-2"]
        );
        let array = serde_json::json!([{"id": "y-1", "nested": {"id": "y-2"}}]);
        assert_eq!(ids_in(&array).len(), 2);
        assert_eq!(
            created_id(&serde_json::json!({"issue": {"id": "z-9"}})),
            Some("z-9".into())
        );
        assert_eq!(created_id(&serde_json::json!({"ok": true})), None);
    }

    #[test]
    fn first_string_array_finds_labels_in_show_output() {
        let show = serde_json::json!([{"id": "a", "labels": ["backend"], "comments": []}]);
        assert_eq!(
            first_string_array(&show, "labels"),
            Some(vec!["backend".to_string()])
        );
        assert_eq!(first_string_array(&serde_json::json!({}), "labels"), None);
    }

    #[test]
    fn tail_keeps_the_end_of_long_output() {
        let long = "x".repeat(300) + "END";
        let tailed = tail(long.as_bytes());
        assert!(tailed.starts_with("..."));
        assert!(tailed.ends_with("END"));
        assert_eq!(tail(b"short"), "short");
    }

    #[test]
    fn filesystem_probe_reports_a_verdict() {
        let temp = tempfile::tempdir().expect("tempdir");
        let facts = probe_filesystem(temp.path());
        assert!(facts.case_sensitive.is_some());
        assert!(
            ["supported", "unsupported", "replaced", "unknown"]
                .contains(&facts.rename_noreplace.as_str()),
            "unexpected verdict: {}",
            facts.rename_noreplace
        );
        // The probe cleans up after itself.
        assert!(std::fs::read_dir(temp.path()).unwrap().next().is_none());
    }
}
