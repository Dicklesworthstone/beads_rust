//! Verify command implementation.
//!
//! Parses structured acceptance criteria and executes verification steps.

use crate::cli::VerifyArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::output::OutputContext;
use crate::util::id::{IdResolver, ResolverConfig};
use serde::{Deserialize, Serialize};
use serde_yaml::Value as YamlValue;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Deserialize)]
struct AcceptanceCriteria {
    #[serde(deserialize_with = "deserialize_schema")]
    schema: u32,
    items: Vec<AcceptanceItem>,
}

#[derive(Debug, Deserialize)]
struct AcceptanceItem {
    id: String,
    text: String,
    verify: Option<Vec<VerifyStep>>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum VerifyStep {
    Command { run: String },
    File { path: String },
    Manual { note: String },
}

#[derive(Debug, Serialize)]
struct VerifyReport {
    total: usize,
    passed: usize,
    failed: usize,
    manual: usize,
    issues: Vec<IssueVerifyReport>,
}

#[derive(Debug, Serialize)]
struct IssueVerifyReport {
    id: String,
    title: String,
    status: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    items: Vec<ItemResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ItemResult {
    id: String,
    text: String,
    status: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    steps: Vec<StepResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct StepResult {
    #[serde(rename = "type")]
    step_type: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Default)]
struct VerifySummary {
    total: usize,
    passed: usize,
    failed: usize,
    manual: usize,
}

impl VerifySummary {
    fn exit_code(&self, allow_manual: bool) -> i32 {
        if self.failed > 0 {
            return 1;
        }
        if !allow_manual && self.manual > 0 {
            return 1;
        }
        0
    }
}

/// Execute the verify command.
///
/// # Errors
///
/// Returns an error if issues are not found or the acceptance criteria are invalid.
pub fn execute(args: &VerifyArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let storage = &storage_ctx.storage;

    let mut target_ids = args.ids.clone();
    if target_ids.is_empty() {
        let last_touched = crate::util::get_last_touched_id(&beads_dir);
        if last_touched.is_empty() {
            return Err(BeadsError::validation(
                "ids",
                "no issue IDs provided and no last-touched issue",
            ));
        }
        target_ids.push(last_touched);
    }

    let config_layer = config::load_config(&beads_dir, Some(storage), cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let repo_root = beads_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| beads_dir.clone());

    let mut summary = VerifySummary::default();
    let mut issue_reports = Vec::new();

    for id_input in target_ids {
        let resolution = resolver.resolve(
            &id_input,
            |id| storage.id_exists(id).unwrap_or(false),
            |hash| storage.find_ids_by_hash(hash).unwrap_or_default(),
        )?;

        let Some(issue) = storage.get_issue(&resolution.id)? else {
            return Err(BeadsError::IssueNotFound { id: resolution.id });
        };

        summary.total += 1;

        let title = issue.title.clone();
        let mut report = IssueVerifyReport {
            id: issue.id.clone(),
            title,
            status: "unknown".to_string(),
            items: Vec::new(),
            error: None,
        };

        let Some(raw_ac) = issue.acceptance_criteria.clone() else {
            report.status = "missing".to_string();
            report.error = Some("acceptance_criteria is missing".to_string());
            summary.failed += 1;
            issue_reports.push(report);
            continue;
        };

        let ac = match parse_acceptance_criteria(&raw_ac) {
            Ok(parsed) => parsed,
            Err(err) => {
            report.status = "error".to_string();
            report.error = Some(err.to_string());
            summary.failed += 1;
            issue_reports.push(report);
            continue;
            }
        };

        if ac.items.is_empty() {
            report.status = "missing".to_string();
            report.error = Some("acceptance_criteria.items is empty".to_string());
            summary.failed += 1;
            issue_reports.push(report);
            continue;
        }

        let mut issue_failed = false;
        let mut issue_manual = false;
        let mut item_results = Vec::new();

        for item in ac.items {
            let mut item_result = ItemResult {
                id: item.id.clone(),
                text: item.text.clone(),
                status: "unknown".to_string(),
                steps: Vec::new(),
                error: None,
            };

            let steps = item.verify.unwrap_or_default();
            if steps.is_empty() {
                item_result.status = "manual".to_string();
                issue_manual = true;
                item_results.push(item_result);
                continue;
            }

            let mut item_failed = false;
            let mut item_manual = false;

            for step in steps {
                let step_result = match step {
                    VerifyStep::Command { run } => {
                        run_command_step(&run, &repo_root)
                    }
                    VerifyStep::File { path } => {
                        run_file_step(&path, &repo_root)
                    }
                    VerifyStep::Manual { note } => StepResult {
                        step_type: "manual".to_string(),
                        status: "manual".to_string(),
                        run: None,
                        path: None,
                        note: Some(note),
                        exit_code: None,
                        error: None,
                    },
                };

                match step_result.status.as_str() {
                    "failed" => item_failed = true,
                    "manual" => item_manual = true,
                    _ => {}
                }
                item_result.steps.push(step_result);
            }

            if item_failed {
                item_result.status = "failed".to_string();
                issue_failed = true;
            } else if item_manual {
                item_result.status = "manual".to_string();
                issue_manual = true;
            } else {
                item_result.status = "passed".to_string();
            }

            item_results.push(item_result);
        }

        report.items = item_results;
        report.status = if issue_failed {
            "failed".to_string()
        } else if issue_manual {
            "manual".to_string()
        } else {
            "passed".to_string()
        };

        match report.status.as_str() {
            "failed" => summary.failed += 1,
            "manual" => summary.manual += 1,
            "passed" => summary.passed += 1,
            _ => {}
        }

        issue_reports.push(report);
    }

    let output = VerifyReport {
        total: summary.total,
        passed: summary.passed,
        failed: summary.failed,
        manual: summary.manual,
        issues: issue_reports,
    };

    if ctx.is_toon() {
        ctx.toon(&output);
    } else if ctx.is_json() {
        ctx.json_pretty(&output);
    } else if ctx.is_quiet() {
        // No output
    } else {
        render_text_report(&output, ctx);
    }

    std::process::exit(summary.exit_code(args.allow_manual));
}

fn parse_acceptance_criteria(raw: &str) -> Result<AcceptanceCriteria> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(BeadsError::validation(
            "acceptance_criteria",
            "acceptance_criteria is empty",
        ));
    }

    if let Ok(parsed) = serde_yaml::from_str::<AcceptanceCriteria>(trimmed) {
        return validate_acceptance_schema(parsed);
    }

    let value: YamlValue = serde_yaml::from_str(trimmed)?;
    if let YamlValue::Mapping(map) = &value {
        let key = YamlValue::String("acceptance_criteria".to_string());
        if let Some(nested) = map.get(&key) {
            if let YamlValue::String(content) = nested {
                let parsed = serde_yaml::from_str::<AcceptanceCriteria>(content)?;
                return validate_acceptance_schema(parsed);
            }
            let parsed = serde_yaml::from_value::<AcceptanceCriteria>(nested.clone())?;
            return validate_acceptance_schema(parsed);
        }
    }

    let parsed = serde_yaml::from_value::<AcceptanceCriteria>(value)?;
    validate_acceptance_schema(parsed)
}

fn validate_acceptance_schema(parsed: AcceptanceCriteria) -> Result<AcceptanceCriteria> {
    if parsed.schema != 1 {
        return Err(BeadsError::validation(
            "acceptance_criteria.schema",
            "schema must be 1",
        ));
    }
    Ok(parsed)
}

fn deserialize_schema<'de, D>(deserializer: D) -> std::result::Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = YamlValue::deserialize(deserializer)?;
    match value {
        YamlValue::Number(n) => n
            .as_u64()
            .map(|v| v as u32)
            .ok_or_else(|| serde::de::Error::custom("schema must be a positive integer")),
        YamlValue::String(s) => s
            .parse::<u32>()
            .map_err(|_| serde::de::Error::custom("schema must be a positive integer")),
        _ => Err(serde::de::Error::custom("schema must be a number")),
    }
}

fn run_command_step(run: &str, cwd: &Path) -> StepResult {
    let (command, args) = if cfg!(windows) {
        ("cmd", vec!["/C", run])
    } else {
        ("sh", vec!["-lc", run])
    };

    let status = Command::new(command)
        .args(args)
        .current_dir(cwd)
        .status();

    match status {
        Ok(status) => {
            let code = status.code().unwrap_or(1);
            StepResult {
                step_type: "command".to_string(),
                status: if status.success() { "passed" } else { "failed" }.to_string(),
                run: Some(run.to_string()),
                path: None,
                note: None,
                exit_code: Some(code),
                error: if status.success() {
                    None
                } else {
                    Some(format!("command exited with code {code}"))
                },
            }
        }
        Err(err) => StepResult {
            step_type: "command".to_string(),
            status: "failed".to_string(),
            run: Some(run.to_string()),
            path: None,
            note: None,
            exit_code: None,
            error: Some(err.to_string()),
        },
    }
}

fn run_file_step(path: &str, root: &Path) -> StepResult {
    let target = resolve_path(path, root);
    let exists = target.exists();
    StepResult {
        step_type: "file".to_string(),
        status: if exists { "passed" } else { "failed" }.to_string(),
        run: None,
        path: Some(target.to_string_lossy().to_string()),
        note: None,
        exit_code: None,
        error: if exists {
            None
        } else {
            Some("file not found".to_string())
        },
    }
}

fn resolve_path(path: &str, root: &Path) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    }
}

fn render_text_report(report: &VerifyReport, ctx: &OutputContext) {
    if report.issues.is_empty() {
        ctx.warning("No issues verified.");
        return;
    }

    ctx.section("Acceptance Criteria Verification");
    ctx.info(&format!(
        "Checked: {}  Passed: {}  Manual: {}  Failed: {}",
        report.total, report.passed, report.manual, report.failed
    ));
    ctx.newline();

    for issue in &report.issues {
        ctx.info(&format!("{} · {} [{}]", issue.id, issue.title, issue.status));
        if let Some(error) = &issue.error {
            ctx.warning(error);
        }
        for item in &issue.items {
            ctx.info(&format!("  {}: {} [{}]", item.id, item.text, item.status));
            if let Some(error) = &item.error {
                ctx.warning(&format!("    {error}"));
            }
            for step in &item.steps {
                let mut detail = String::new();
                if let Some(run) = &step.run {
                    detail = format!("run: {run}");
                } else if let Some(path) = &step.path {
                    detail = format!("path: {path}");
                } else if let Some(note) = &step.note {
                    detail = format!("note: {note}");
                }
                ctx.info(&format!(
                    "    - {} [{}] {}",
                    step.step_type, step.status, detail
                ));
            }
        }
        ctx.newline();
    }
}
