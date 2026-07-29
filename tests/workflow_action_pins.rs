//! Regression coverage for immutable GitHub Actions pins.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;

const INVENTORY_PATH: &str = ".github/action-pins.jsonl";
const UPSTREAMS_PATH: &str = ".github/action-pin-upstreams.jsonl";
const WORKFLOW_DIR: &str = ".github/workflows";
const WORKFLOW_DIR_PREFIX: &str = ".github/workflows/";

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct InventoryKey {
    workflow: String,
    action: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryRecord {
    workflow: String,
    action: String,
    #[serde(rename = "sha")]
    expected_revision: String,
    tag: String,
    source: String,
}

#[derive(Debug)]
struct InventoryEntry {
    expected_revision: String,
}

#[derive(Debug)]
struct WorkflowUse {
    key: InventoryKey,
    revision: String,
    line: usize,
}

#[test]
fn repository_workflow_action_pins_are_inventory_backed() -> Result<(), String> {
    verify_action_pins(Path::new("."), Path::new(INVENTORY_PATH))
        .map_err(|errors| errors.join("\n"))
}

#[test]
fn clean_fixture_passes() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_workflow(&format!(
        r"
name: fixture
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@{PIN_A}
      - uses: ./local-action
"
    ))?;
    fixture.write_inventory(&[inventory_line(
        ".github/workflows/example.yml",
        "actions/checkout",
        PIN_A,
    )])?;

    verify_action_pins(fixture.root(), &fixture.inventory_path())
        .map_err(|errors| errors.join("\n"))
}

#[test]
fn rejects_mutable_action_ref() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_workflow(
        r"
name: fixture
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
",
    )?;
    fixture.write_inventory(&[inventory_line(
        ".github/workflows/example.yml",
        "actions/checkout",
        PIN_A,
    )])?;

    let errors = expect_verification_errors(&fixture)?;
    require_error_contains(&errors, "not pinned to a 40-character SHA")
}

#[test]
fn rejects_missing_inventory_entry() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_workflow(&format!(
        r"
name: fixture
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@{PIN_A}
"
    ))?;
    fixture.write_inventory(&[inventory_line(
        ".github/workflows/example.yml",
        "actions/setup-go",
        PIN_B,
    )])?;

    let errors = expect_verification_errors(&fixture)?;
    require_error_contains(&errors, "missing inventory entry")
}

#[test]
fn rejects_mismatched_sha() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_workflow(&format!(
        r"
name: fixture
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@{PIN_A}
"
    ))?;
    fixture.write_inventory(&[inventory_line(
        ".github/workflows/example.yml",
        "actions/checkout",
        PIN_B,
    )])?;

    let errors = expect_verification_errors(&fixture)?;
    require_error_contains(&errors, "inventory SHA mismatch")
}

#[test]
fn rejects_malformed_inventory_sha() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_workflow(&format!(
        r"
name: fixture
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@{PIN_A}
"
    ))?;
    fixture.write_inventory(&[inventory_line(
        ".github/workflows/example.yml",
        "actions/checkout",
        "v4",
    )])?;

    let errors = expect_verification_errors(&fixture)?;
    require_error_contains(&errors, "inventory SHA is not a 40-character hex value")
}

#[test]
fn rejects_duplicate_inventory_entry() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_workflow(&format!(
        r"
name: fixture
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@{PIN_A}
"
    ))?;
    fixture.write_inventory(&[
        inventory_line(".github/workflows/example.yml", "actions/checkout", PIN_A),
        inventory_line(".github/workflows/example.yml", "actions/checkout", PIN_A),
    ])?;

    let errors = expect_verification_errors(&fixture)?;
    require_error_contains(&errors, "duplicate inventory entry")
}

#[test]
fn rejects_stale_inventory_entry() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_workflow(&format!(
        r"
name: fixture
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@{PIN_A}
"
    ))?;
    fixture.write_inventory(&[
        inventory_line(".github/workflows/example.yml", "actions/checkout", PIN_A),
        inventory_line(".github/workflows/old.yml", "actions/setup-go", PIN_B),
    ])?;

    let errors = expect_verification_errors(&fixture)?;
    require_error_contains(&errors, "stale inventory entry")
}

#[test]
fn rejects_inventory_path_outside_workflow_dir() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_workflow(&format!(
        r"
name: fixture
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@{PIN_A}
"
    ))?;
    fixture.write_inventory(&[inventory_line(
        ".github/workflows-old/example.yml",
        "actions/checkout",
        PIN_A,
    )])?;

    let errors = expect_verification_errors(&fixture)?;
    require_error_contains(&errors, "workflow must live under")
}

#[test]
fn audit_report_marks_up_to_date_actions() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_inventory(&[inventory_line_with_tag(
        ".github/workflows/example.yml",
        "actions/checkout",
        PIN_A,
        "v1",
    )])?;
    fixture.write_upstreams(&[upstream_line("actions/checkout", "v1", PIN_A)])?;

    let report = run_update_audit_json(&fixture)?;
    require_entry_status(&report, "actions/checkout", "up_to_date")?;
    require_summary_count(&report, "up_to_date", 1)
}

#[test]
fn audit_report_marks_update_available_actions() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_inventory(&[inventory_line_with_tag(
        ".github/workflows/example.yml",
        "actions/checkout",
        PIN_A,
        "v1",
    )])?;
    fixture.write_upstreams(&[upstream_line("actions/checkout", "v2", PIN_B)])?;

    let report = run_update_audit_json(&fixture)?;
    require_entry_status(&report, "actions/checkout", "update_available")?;
    require_entry_contains_step(
        &report,
        "actions/checkout",
        "Update .github/action-pins.jsonl",
    )
}

#[test]
fn audit_report_records_upstream_unreachable_without_failing() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_inventory(&[inventory_line_with_tag(
        ".github/workflows/example.yml",
        "actions/checkout",
        PIN_A,
        "v1",
    )])?;
    fixture.write_upstreams(&[upstream_line_with_lookup_status(
        "actions/checkout",
        "v2",
        PIN_B,
        "upstream_unreachable",
    )])?;

    let report = run_update_audit_json(&fixture)?;
    require_entry_status(&report, "actions/checkout", "upstream_unreachable")
}

#[test]
fn audit_report_records_missing_tag_without_failing() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_inventory(&[inventory_line_with_tag(
        ".github/workflows/example.yml",
        "actions/checkout",
        PIN_A,
        "v1",
    )])?;
    fixture.write_upstreams(&[upstream_line_with_lookup_status(
        "actions/checkout",
        "v9",
        PIN_B,
        "missing_tag",
    )])?;

    let report = run_update_audit_json(&fixture)?;
    require_entry_status(&report, "actions/checkout", "missing_tag")
}

#[test]
fn audit_report_rejects_disallowed_downgrades() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_inventory(&[inventory_line_with_tag(
        ".github/workflows/example.yml",
        "actions/checkout",
        PIN_B,
        "v2",
    )])?;
    fixture.write_upstreams(&[upstream_line("actions/checkout", "v1", PIN_A)])?;

    let report = run_update_audit_json(&fixture)?;
    require_entry_status(&report, "actions/checkout", "disallowed_downgrade")
}

#[test]
fn audit_text_report_is_concise_human_output() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_inventory(&[inventory_line_with_tag(
        ".github/workflows/example.yml",
        "actions/checkout",
        PIN_A,
        "v1",
    )])?;
    fixture.write_upstreams(&[upstream_line("actions/checkout", "v2", PIN_B)])?;

    let text = run_update_audit_text(&fixture)?;
    require_text_contains(&text, "Action pin update audit")?;
    require_text_contains(&text, "update_available")?;
    if text.contains("\"entries\"") {
        return Err(format!("text report should not contain raw JSON: {text}"));
    }
    Ok(())
}

#[test]
fn audit_text_report_suppresses_current_rows_by_default() -> Result<(), String> {
    let fixture = PinFixture::new()?;
    fixture.write_inventory(&[inventory_line_with_tag(
        ".github/workflows/example.yml",
        "actions/checkout",
        PIN_A,
        "v1",
    )])?;
    fixture.write_upstreams(&[upstream_line("actions/checkout", "v1", PIN_A)])?;

    let text = run_update_audit_text(&fixture)?;
    require_text_contains(&text, "All action pins match configured upstream refs.")?;
    if text.contains("- up_to_date:") {
        return Err(format!(
            "text report should hide up-to-date rows unless --all is used: {text}"
        ));
    }
    Ok(())
}

fn verify_action_pins(repo_root: &Path, inventory_path: &Path) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let inventory = match load_inventory(inventory_path) {
        Ok(inventory) => inventory,
        Err(mut inventory_errors) => {
            errors.append(&mut inventory_errors);
            BTreeMap::new()
        }
    };
    let workflow_uses = match scan_workflows(repo_root) {
        Ok(workflow_uses) => workflow_uses,
        Err(mut scan_errors) => {
            errors.append(&mut scan_errors);
            Vec::new()
        }
    };

    if !errors.is_empty() {
        return Err(errors);
    }

    let mut seen = BTreeSet::new();
    for workflow_use in workflow_uses {
        match inventory.get(&workflow_use.key) {
            Some(record) if record.expected_revision.as_str().eq(&workflow_use.revision) => {
                seen.insert(workflow_use.key);
            }
            Some(record) => errors.push(format!(
                "{}:{} {} inventory SHA mismatch: workflow has {}, inventory has {}",
                workflow_use.key.workflow,
                workflow_use.line,
                workflow_use.key.action,
                workflow_use.revision,
                record.expected_revision
            )),
            None => errors.push(format!(
                "{}:{} {} missing inventory entry",
                workflow_use.key.workflow, workflow_use.line, workflow_use.key.action
            )),
        }
    }

    for key in inventory.keys() {
        if !seen.contains(key) {
            errors.push(format!(
                "{} {} stale inventory entry",
                key.workflow, key.action
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn load_inventory(path: &Path) -> Result<BTreeMap<InventoryKey, InventoryEntry>, Vec<String>> {
    let content = fs::read_to_string(path)
        .map_err(|error| vec![format!("failed to read {}: {error}", path.display())])?;
    let mut errors = Vec::new();
    let mut records = BTreeMap::new();

    for (index, raw_line) in content.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let record = match serde_json::from_str::<InventoryRecord>(line) {
            Ok(record) => record,
            Err(error) => {
                errors.push(format!(
                    "{}:{line_number} invalid inventory JSON: {error}",
                    path.display()
                ));
                continue;
            }
        };

        errors.extend(validate_inventory_record(path, line_number, &record));
        let InventoryRecord {
            workflow,
            action,
            expected_revision,
            tag: _,
            source: _,
        } = record;
        let key = InventoryKey { workflow, action };
        let inventory_entry = InventoryEntry { expected_revision };

        match records.entry(key) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(inventory_entry);
            }
            std::collections::btree_map::Entry::Occupied(entry) => errors.push(format!(
                "{}:{line_number} duplicate inventory entry for {} in {}",
                path.display(),
                entry.key().action,
                entry.key().workflow
            )),
        }
    }

    if records.is_empty() {
        errors.push(format!("{} has no action pin entries", path.display()));
    }

    if errors.is_empty() {
        Ok(records)
    } else {
        Err(errors)
    }
}

fn validate_inventory_record(
    path: &Path,
    line_number: usize,
    record: &InventoryRecord,
) -> Vec<String> {
    let mut errors = Vec::new();

    if !record.workflow.starts_with(WORKFLOW_DIR_PREFIX) {
        errors.push(format!(
            "{}:{line_number} workflow must live under {WORKFLOW_DIR_PREFIX}: {}",
            path.display(),
            record.workflow
        ));
    }
    if !is_workflow_file(Path::new(&record.workflow)) {
        errors.push(format!(
            "{}:{line_number} workflow must be a .yml or .yaml file: {}",
            path.display(),
            record.workflow
        ));
    }
    if record.action.is_empty()
        || record.action.contains('@')
        || record.action.starts_with('.')
        || !record.action.contains('/')
    {
        errors.push(format!(
            "{}:{line_number} action must be an external owner/repo action: {}",
            path.display(),
            record.action
        ));
    }
    if !is_sha40_hex(&record.expected_revision) {
        errors.push(format!(
            "{}:{line_number} inventory SHA is not a 40-character hex value: {}",
            path.display(),
            record.expected_revision
        ));
    }
    if record.tag.trim().is_empty() {
        errors.push(format!(
            "{}:{line_number} tag must not be empty",
            path.display()
        ));
    }
    if record.source.trim().is_empty() {
        errors.push(format!(
            "{}:{line_number} source must not be empty",
            path.display()
        ));
    }

    errors
}

fn scan_workflows(repo_root: &Path) -> Result<Vec<WorkflowUse>, Vec<String>> {
    let workflows_dir = repo_root.join(WORKFLOW_DIR);
    let workflow_files = collect_workflow_files(&workflows_dir)?;
    let mut errors = Vec::new();
    let mut workflow_uses = Vec::new();

    for workflow_file in workflow_files {
        let workflow = workflow_file
            .strip_prefix(repo_root)
            .unwrap_or(&workflow_file)
            .to_string_lossy()
            .replace('\\', "/");
        let content = match fs::read_to_string(&workflow_file) {
            Ok(content) => content,
            Err(error) => {
                errors.push(format!(
                    "failed to read {}: {error}",
                    workflow_file.display()
                ));
                continue;
            }
        };

        for (index, line) in content.lines().enumerate() {
            let line_number = index + 1;
            let Some(value) = uses_value(line) else {
                continue;
            };
            if is_local_action_ref(value) {
                continue;
            }

            match parse_external_action_ref(value) {
                Ok((action, sha)) => workflow_uses.push(WorkflowUse {
                    key: inventory_key(&workflow, action),
                    revision: sha.to_owned(),
                    line: line_number,
                }),
                Err(error) => errors.push(format!("{workflow}:{line_number} {error}: {value}")),
            }
        }
    }

    if errors.is_empty() {
        Ok(workflow_uses)
    } else {
        Err(errors)
    }
}

fn collect_workflow_files(workflows_dir: &Path) -> Result<Vec<PathBuf>, Vec<String>> {
    let entries = fs::read_dir(workflows_dir).map_err(|error| {
        vec![format!(
            "failed to read workflow directory {}: {error}",
            workflows_dir.display()
        )]
    })?;
    let mut files = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|error| {
            vec![format!(
                "failed to inspect workflow directory {}: {error}",
                workflows_dir.display()
            )]
        })?;
        let workflow_file = entry.path();
        if is_workflow_file(&workflow_file) {
            files.extend(std::iter::once(workflow_file));
        }
    }

    files.sort();
    Ok(files)
}

fn uses_value(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let value = trimmed
        .strip_prefix("- uses:")
        .or_else(|| trimmed.strip_prefix("uses:"))?
        .trim();
    let value = value
        .split_once('#')
        .map_or(value, |(before_comment, _)| before_comment);
    let value = value.split_whitespace().next().unwrap_or("").trim();

    Some(strip_matching_quotes(value))
}

fn strip_matching_quotes(value: &str) -> &str {
    if let Some(value) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        return value;
    }
    if let Some(value) = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        return value;
    }
    value
}

fn is_local_action_ref(value: &str) -> bool {
    value.starts_with("./") || value.starts_with("../")
}

fn parse_external_action_ref(value: &str) -> Result<(&str, &str), &'static str> {
    let (action, reference) = value
        .rsplit_once('@')
        .ok_or("external action is missing an @ reference")?;

    if action.is_empty() || reference.is_empty() || !action.contains('/') {
        return Err("external action must use owner/repo@sha syntax");
    }
    if !is_sha40_hex(reference) {
        return Err("external action is not pinned to a 40-character SHA");
    }

    Ok((action, reference))
}

fn inventory_key(workflow: &str, action: &str) -> InventoryKey {
    InventoryKey {
        workflow: workflow.to_owned(),
        action: action.to_owned(),
    }
}

fn is_workflow_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(std::ffi::OsStr::to_str),
        Some("yml" | "yaml")
    )
}

fn is_sha40_hex(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
const PIN_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
#[cfg(test)]
const PIN_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[cfg(test)]
struct PinFixture {
    temp_dir: tempfile::TempDir,
}

#[cfg(test)]
impl PinFixture {
    fn new() -> Result<Self, String> {
        Ok(Self {
            temp_dir: tempfile::TempDir::new()
                .map_err(|error| format!("failed to create temp fixture: {error}"))?,
        })
    }

    fn root(&self) -> &Path {
        self.temp_dir.path()
    }

    fn inventory_path(&self) -> PathBuf {
        self.root().join(INVENTORY_PATH)
    }

    fn upstream_path(&self) -> PathBuf {
        self.root().join(UPSTREAMS_PATH)
    }

    fn write_workflow(&self, content: &str) -> Result<(), String> {
        let workflow_path = self.root().join(".github/workflows/example.yml");
        let parent = workflow_path
            .parent()
            .ok_or_else(|| "workflow path has no parent".to_owned())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create workflow fixture directory: {error}"))?;
        fs::write(workflow_path, content)
            .map_err(|error| format!("failed to write workflow fixture: {error}"))
    }

    fn write_inventory(&self, lines: &[String]) -> Result<(), String> {
        let inventory_path = self.inventory_path();
        let parent = inventory_path
            .parent()
            .ok_or_else(|| "inventory path has no parent".to_owned())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create inventory fixture directory: {error}"))?;
        fs::write(inventory_path, format!("{}\n", lines.join("\n")))
            .map_err(|error| format!("failed to write inventory fixture: {error}"))
    }

    fn write_upstreams(&self, lines: &[String]) -> Result<(), String> {
        let upstream_path = self.upstream_path();
        let parent = upstream_path
            .parent()
            .ok_or_else(|| "upstream policy path has no parent".to_owned())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create upstream fixture directory: {error}"))?;
        fs::write(upstream_path, format!("{}\n", lines.join("\n")))
            .map_err(|error| format!("failed to write upstream fixture: {error}"))
    }
}

#[cfg(test)]
fn inventory_line(workflow: &str, action: &str, sha: &str) -> String {
    inventory_line_with_tag(workflow, action, sha, "fixture-tag")
}

#[cfg(test)]
fn inventory_line_with_tag(workflow: &str, action: &str, sha: &str, tag: &str) -> String {
    serde_json::json!({
        "workflow": workflow,
        "action": action,
        "sha": sha,
        "tag": tag,
        "source": "fixture-source"
    })
    .to_string()
}

#[cfg(test)]
fn upstream_line(action: &str, latest_allowed_tag: &str, latest_allowed_sha: &str) -> String {
    upstream_line_with_lookup_status(action, latest_allowed_tag, latest_allowed_sha, "ok")
}

#[cfg(test)]
fn upstream_line_with_lookup_status(
    action: &str,
    latest_allowed_tag: &str,
    latest_allowed_sha: &str,
    lookup_status: &str,
) -> String {
    serde_json::json!({
        "action": action,
        "repo": format!("https://github.com/{action}.git"),
        "latest_allowed_tag": latest_allowed_tag,
        "latest_allowed_sha": latest_allowed_sha,
        "lookup_status": lookup_status,
        "source": "fixture-source"
    })
    .to_string()
}

#[cfg(test)]
fn expect_verification_errors(fixture: &PinFixture) -> Result<Vec<String>, String> {
    match verify_action_pins(fixture.root(), &fixture.inventory_path()) {
        Ok(()) => Err("fixture should fail verification".to_owned()),
        Err(errors) => Ok(errors),
    }
}

#[cfg(test)]
fn require_error_contains(errors: &[String], needle: &str) -> Result<(), String> {
    if errors.iter().any(|error| error.contains(needle)) {
        Ok(())
    } else {
        Err(format!(
            "expected an error containing {needle:?}, got {errors:#?}"
        ))
    }
}

#[cfg(test)]
fn run_update_audit_json(fixture: &PinFixture) -> Result<Value, String> {
    let output = Command::new("bash")
        .arg("scripts/audit-workflow-action-pins.sh")
        .arg("--inventory")
        .arg(fixture.inventory_path())
        .arg("--upstreams")
        .arg(fixture.upstream_path())
        .arg("--format")
        .arg("json")
        .output()
        .map_err(|error| format!("failed to run update audit script: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "update audit script failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "failed to parse update audit JSON: {error}\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[cfg(test)]
fn run_update_audit_text(fixture: &PinFixture) -> Result<String, String> {
    let output = Command::new("bash")
        .arg("scripts/audit-workflow-action-pins.sh")
        .arg("--inventory")
        .arg(fixture.inventory_path())
        .arg("--upstreams")
        .arg(fixture.upstream_path())
        .arg("--format")
        .arg("text")
        .output()
        .map_err(|error| format!("failed to run update audit script: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "update audit script failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(test)]
fn require_entry_status(report: &Value, action: &str, expected_status: &str) -> Result<(), String> {
    let entry = find_report_entry(report, action)?;
    let status = entry
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("report entry for {action} has no string status: {entry}"))?;
    if status == expected_status {
        Ok(())
    } else {
        Err(format!(
            "expected {action} status {expected_status:?}, got {status:?}: {entry}"
        ))
    }
}

#[cfg(test)]
fn require_entry_contains_step(report: &Value, action: &str, needle: &str) -> Result<(), String> {
    let entry = find_report_entry(report, action)?;
    let steps = entry
        .get("manual_update_steps")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("report entry for {action} has no manual steps: {entry}"))?;
    if steps
        .iter()
        .filter_map(Value::as_str)
        .any(|step| step.contains(needle))
    {
        Ok(())
    } else {
        Err(format!(
            "expected {action} manual steps to contain {needle:?}: {entry}"
        ))
    }
}

#[cfg(test)]
fn require_summary_count(report: &Value, key: &str, expected: u64) -> Result<(), String> {
    let summary = report
        .get("summary")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("report has no summary object: {report}"))?;
    let actual = summary
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("report summary has no numeric {key:?}: {report}"))?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "expected summary {key}={expected}, got {actual}: {report}"
        ))
    }
}

#[cfg(test)]
fn find_report_entry<'a>(report: &'a Value, action: &str) -> Result<&'a Value, String> {
    let entries = report
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("report has no entries array: {report}"))?;
    entries
        .iter()
        .find(|entry| entry.get("action").and_then(Value::as_str) == Some(action))
        .ok_or_else(|| format!("report has no entry for {action}: {report}"))
}

#[cfg(test)]
fn require_text_contains(text: &str, needle: &str) -> Result<(), String> {
    if text.contains(needle) {
        Ok(())
    } else {
        Err(format!("expected text to contain {needle:?}:\n{text}"))
    }
}
