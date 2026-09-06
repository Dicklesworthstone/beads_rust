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

/// These are transport and refusal controls for the actual CI shell fragments.
/// Their constructed receipts are not measurements of CLI performance.
mod scheduled_benchmarks {
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Output};

    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    const GATE: &str = "Enforce complete matched benchmark receipts";
    const FIRST: &str = "1000-version-default-auto-flush";
    const PLANTED: &str = "Measure planted ready slowdown control";
    const MEASURE: &str = "Measure release A-A control and A-B comparison";
    const VERIFY_BUNDLE: &str = "Verify benchmark artifact bundle";
    const READY: &str = "1000-ready-default-auto-flush";
    // Shell/serialization controls, not live timing evidence. The positive
    // full-matrix fixture explicitly declares enough blocks for finite p95 bounds.
    const CONTROL_BLOCKS: usize = 99;

    fn workflow() -> Value {
        serde_yml::from_str(&fs::read_to_string(".github/workflows/ci.yml").unwrap()).unwrap()
    }

    fn step(name: &str) -> Value {
        let workflow = workflow();
        let matches = ["bench-build", "bench", "bench-gate"]
            .into_iter()
            .flat_map(|job| workflow["jobs"][job]["steps"].as_array().unwrap())
            .filter(|step| step["name"] == name)
            .collect::<Vec<_>>();
        assert_eq!(
            matches.len(),
            1,
            "scheduled benchmark step must be unique: {name}"
        );
        matches[0].clone()
    }

    fn run_fragment(name: &str, root: &Path, variables: &[(&str, String)]) -> Output {
        let selected = step(name);
        Command::new("bash")
            .args(["-euo", "pipefail", "-c", selected["run"].as_str().unwrap()])
            .current_dir(root)
            .envs(variables.iter().map(|(key, value)| (*key, value)))
            .output()
            .unwrap()
    }

    fn expect_exit(output: &Output, expected: i32, diagnostic: &str) {
        assert_eq!(
            output.status.code(),
            Some(expected),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(diagnostic),
            "missing {diagnostic:?}: {output:?}"
        );
    }

    fn names() -> Vec<String> {
        [1000, 10000]
            .into_iter()
            .flat_map(|count| {
                ["default-auto-flush", "diagnostic-no-auto-flush"]
                    .into_iter()
                    .flat_map(move |flush| {
                        [
                            "version", "ready", "list", "show", "create", "update", "close",
                        ]
                        .into_iter()
                        .map(move |command| format!("{count}-{command}-{flush}"))
                    })
            })
            .collect()
    }

    const BUNDLE_FILES: [(&str, &str); 5] = [
        ("baseline-br", "BR_PERF_BASELINE_SHA256"),
        ("candidate-br", "BR_PERF_CANDIDATE_SHA256"),
        ("measurement-driver", "BR_PERF_HARNESS_SHA256"),
        ("baseline-Cargo.lock", "BR_PERF_BASELINE_LOCK_SHA256"),
        ("candidate-Cargo.lock", "BR_PERF_CANDIDATE_LOCK_SHA256"),
    ];

    struct BundleControl {
        root: tempfile::TempDir,
        pins: Value,
    }

    impl BundleControl {
        fn new(omitted: Option<&str>) -> Self {
            let root = tempfile::TempDir::new().unwrap();
            fs::create_dir(root.path().join("bundle")).unwrap();
            let mut pins = json!({"BR_PERF_BASELINE_SOURCE": "a".repeat(40),
                "BR_PERF_CANDIDATE_SOURCE": "b".repeat(40)});
            for (file, key) in BUNDLE_FILES {
                // Real bytes and SHA256 transport checks; these are explicitly
                // inert artifact fixtures, not compiled performance binaries.
                let bytes = format!("inert benchmark artifact control: {file}\n");
                pins[key] = json!(beads_rust::util::hex_encode(&Sha256::digest(
                    bytes.as_bytes()
                )));
                if omitted != Some(file) {
                    fs::write(root.path().join("bundle").join(file), bytes).unwrap();
                }
            }
            fs::write(
                root.path().join("github-env"),
                "CONTROL_SENTINEL=retained\n",
            )
            .unwrap();
            Self { root, pins }
        }

        fn run(&self) -> Output {
            run_fragment(
                VERIFY_BUNDLE,
                self.root.path(),
                &[
                    (
                        "BR_PERF_ARTIFACT_DIR",
                        self.root.path().join("bundle").display().to_string(),
                    ),
                    ("BR_PERF_ARTIFACT_PINS", self.pins.to_string()),
                    ("GITHUB_SHA", "b".repeat(40)),
                    (
                        "GITHUB_ENV",
                        self.root.path().join("github-env").display().to_string(),
                    ),
                ],
            )
        }

        fn assert_no_export(&self) {
            assert_eq!(
                fs::read_to_string(self.root.path().join("github-env")).unwrap(),
                "CONTROL_SENTINEL=retained\n"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn scheduled_bundle_fragment_verifies_real_bytes_before_exporting_pins_and_paths() {
        use std::os::unix::fs::PermissionsExt;

        let control = BundleControl::new(None);
        for (file, _) in BUNDLE_FILES {
            fs::set_permissions(
                control.root.path().join("bundle").join(file),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        expect_exit(&control.run(), 0, "all five file digests");
        let env = fs::read_to_string(control.root.path().join("github-env")).unwrap();
        let records = env
            .lines()
            .map(|line| line.split_once('=').unwrap())
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            records.len(),
            13,
            "one sentinel, seven pins and five paths: {env}"
        );
        assert_eq!(env.lines().count(), 13, "duplicate exports: {env}");
        for (key, expected) in control.pins.as_object().unwrap() {
            assert_eq!(records[key.as_str()], expected.as_str().unwrap());
        }
        for (file, key) in [
            ("baseline-br", "BR_PERF_BASELINE_BINARY"),
            ("candidate-br", "BR_PERF_CANDIDATE_BINARY"),
            ("measurement-driver", "BR_PERF_HARNESS"),
            ("baseline-Cargo.lock", "BR_PERF_BASELINE_LOCKFILE"),
            ("candidate-Cargo.lock", "BR_PERF_CANDIDATE_LOCKFILE"),
        ] {
            let path = fs::canonicalize(control.root.path().join("bundle"))
                .unwrap()
                .join(file);
            assert_eq!(records[key], path.display().to_string());
            let mode = fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o111 != 0,
                matches!(file, "baseline-br" | "candidate-br" | "measurement-driver")
            );
        }
    }

    #[test]
    fn scheduled_bundle_fragment_refuses_missing_corrupt_and_wrong_source_artifacts() {
        for (file, _) in BUNDLE_FILES {
            let missing = BundleControl::new(Some(file));
            expect_exit(&missing.run(), 2, "missing regular artifact");
            missing.assert_no_export();
            let corrupt = BundleControl::new(None);
            fs::write(
                corrupt.root.path().join("bundle").join(file),
                "changed bytes\n",
            )
            .unwrap();
            expect_exit(&corrupt.run(), 2, "artifact digest mismatch");
            corrupt.assert_no_export();
        }
        for (key, value) in [
            ("BR_PERF_CANDIDATE_SOURCE", json!("c".repeat(40))),
            ("BR_PERF_BASELINE_SOURCE", json!("0".repeat(40))),
            ("BR_PERF_BASELINE_SOURCE", json!("main")),
            ("BR_PERF_CANDIDATE_SOURCE", json!("B".repeat(40))),
            ("BR_PERF_HARNESS_SHA256", json!("0".repeat(64))),
            ("BR_PERF_HARNESS_SHA256", json!("malformed")),
        ] {
            let mut control = BundleControl::new(None);
            control.pins[key] = value;
            expect_exit(&control.run(), 2, "inconclusive");
            control.assert_no_export();
        }
        for pins in [json!({}), json!([]), json!(null)] {
            let mut control = BundleControl::new(None);
            control.pins = pins;
            expect_exit(&control.run(), 2, "inconclusive");
            control.assert_no_export();
        }
    }

    #[test]
    fn scheduled_package_and_loader_fragments_roundtrip_all_artifact_bytes_and_pins() {
        let control = BundleControl::new(None);
        let source = control.root.path().join("bundle");
        let packaged = control.root.path().join("packaged");
        let evidence = control.root.path().join("build-evidence");
        fs::create_dir(&evidence).unwrap();
        let build_log = "{\"fixture\":\"artifact transport only, not a compiled binary\"}\n";
        fs::write(evidence.join("build-artifacts.jsonl"), build_log).unwrap();
        let output_path = control.root.path().join("github-output");
        fs::write(&output_path, "CONTROL_SENTINEL=retained\n").unwrap();
        let mut variables = control
            .pins
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str().unwrap().to_string()))
            .collect::<Vec<_>>();
        variables.extend([
            ("BR_PERF_ARTIFACT_DIR", packaged.display().to_string()),
            ("BR_PERF_EVIDENCE", evidence.display().to_string()),
            ("GITHUB_OUTPUT", output_path.display().to_string()),
        ]);
        for (file, variable) in [
            ("baseline-br", "BR_PERF_BASELINE_BINARY"),
            ("candidate-br", "BR_PERF_CANDIDATE_BINARY"),
            ("measurement-driver", "BR_PERF_HARNESS"),
            ("baseline-Cargo.lock", "BR_PERF_BASELINE_LOCKFILE"),
            ("candidate-Cargo.lock", "BR_PERF_CANDIDATE_LOCKFILE"),
        ] {
            variables.push((variable, source.join(file).display().to_string()));
        }
        expect_exit(
            &run_fragment(
                "Package benchmark artifacts",
                control.root.path(),
                &variables,
            ),
            0,
            "",
        );
        let output = fs::read_to_string(output_path).unwrap();
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "CONTROL_SENTINEL=retained");
        let pins: Value = serde_json::from_str(lines[1].strip_prefix("pins=").unwrap()).unwrap();
        assert_eq!(pins, control.pins);
        for (file, _) in BUNDLE_FILES {
            assert_eq!(
                fs::read(packaged.join(file)).unwrap(),
                fs::read(source.join(file)).unwrap()
            );
        }
        assert_eq!(
            fs::read_to_string(packaged.join("build-artifacts.jsonl")).unwrap(),
            build_log
        );
        expect_exit(
            &run_fragment(
                VERIFY_BUNDLE,
                control.root.path(),
                &[
                    ("BR_PERF_ARTIFACT_DIR", packaged.display().to_string()),
                    ("BR_PERF_ARTIFACT_PINS", pins.to_string()),
                    ("GITHUB_SHA", "b".repeat(40)),
                    (
                        "GITHUB_ENV",
                        control.root.path().join("github-env").display().to_string(),
                    ),
                ],
            ),
            0,
            "all five file digests",
        );
        let env = fs::read_to_string(control.root.path().join("github-env")).unwrap();
        let harness = fs::canonicalize(packaged)
            .unwrap()
            .join("measurement-driver");
        assert!(env.contains(&format!("BR_PERF_HARNESS={}\n", harness.display())));
    }

    #[cfg(unix)]
    struct ShardControl {
        root: tempfile::TempDir,
    }

    #[cfg(unix)]
    impl ShardControl {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;

            let root = tempfile::TempDir::new().unwrap();
            let harness = root.path().join("mock-collector");
            // This mock records only the real shell transport. It makes no
            // admission, timing, statistical or complete-receipt claim.
            fs::write(
                &harness,
                r"#!/usr/bin/env python3
import json, os, pathlib, sys
root = pathlib.Path(os.environ['BR_PERF_EVIDENCE'])
relative = pathlib.Path(os.environ['BR_PERF_OUTPUT']).relative_to(root)
keys = ('OUTPUT', 'WORKLOAD', 'ABBA_BLOCKS', 'QUIET_RUNNER', 'NEGATIVE_EXTRA_WORK',
        'BUDGET_PCT', 'BUDGETS_JSON', 'BASELINE_BINARY', 'CANDIDATE_BINARY',
        'BASELINE_SHA256', 'CANDIDATE_SHA256', 'BASELINE_LOCKFILE', 'CANDIDATE_LOCKFILE',
        'BASELINE_LOCK_SHA256', 'CANDIDATE_LOCK_SHA256', 'BASELINE_SOURCE', 'CANDIDATE_SOURCE')
with (root / 'invocations.jsonl').open('a') as log:
    log.write(json.dumps({'args': sys.argv[1:], 'relative': str(relative),
        'env': {key: os.environ.get('BR_PERF_' + key) for key in keys}}) + '\n')
print('mock shard collector transport control: stdout')
print('mock shard collector transport control: stderr', file=sys.stderr)
raise SystemExit(37 if str(relative) == os.environ['BR_PERF_FIXTURE_FAILURE'] else 0)
",
            )
            .unwrap();
            fs::set_permissions(harness, fs::Permissions::from_mode(0o755)).unwrap();
            Self { root }
        }

        fn run(&self, count: &str, flush: &str, failure: &str) -> Output {
            run_fragment(
                MEASURE,
                self.root.path(),
                &[
                    ("BR_PERF_EVIDENCE", self.root.path().display().to_string()),
                    (
                        "BR_PERF_HARNESS",
                        self.root
                            .path()
                            .join("mock-collector")
                            .display()
                            .to_string(),
                    ),
                    ("BR_PERF_SHARD_ISSUE_COUNT", count.into()),
                    ("BR_PERF_SHARD_FLUSH", flush.into()),
                    ("BR_PERF_ABBA_BLOCKS", "99".into()),
                    ("BR_PERF_QUIET_RUNNER", "1".into()),
                    ("BR_PERF_BUDGETS_JSON", json!({READY: 7}).to_string()),
                    ("BR_PERF_BUDGET_PCT", "99999".into()),
                    ("BR_PERF_WORKLOAD", "wrong-inherited-selector".into()),
                    ("BR_PERF_OUTPUT", "wrong-inherited-output".into()),
                    ("BR_PERF_NEGATIVE_EXTRA_WORK", "20".into()),
                    ("BR_PERF_BASELINE_BINARY", "baseline-release-br".into()),
                    ("BR_PERF_CANDIDATE_BINARY", "candidate-release-br".into()),
                    ("BR_PERF_BASELINE_SHA256", "1".repeat(64)),
                    ("BR_PERF_CANDIDATE_SHA256", "2".repeat(64)),
                    ("BR_PERF_BASELINE_LOCKFILE", "baseline-Cargo.lock".into()),
                    ("BR_PERF_CANDIDATE_LOCKFILE", "candidate-Cargo.lock".into()),
                    ("BR_PERF_BASELINE_LOCK_SHA256", "3".repeat(64)),
                    ("BR_PERF_CANDIDATE_LOCK_SHA256", "4".repeat(64)),
                    ("BR_PERF_BASELINE_SOURCE", "a".repeat(40)),
                    ("BR_PERF_CANDIDATE_SOURCE", "b".repeat(40)),
                    ("BR_PERF_FIXTURE_FAILURE", failure.into()),
                ],
            )
        }

        fn invocations(&self) -> Vec<Value> {
            fs::read_to_string(self.root.path().join("invocations.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        fn assert_pipeline(&self, relative: &str, collector: i32, capture: i32) {
            let directory = self.root.path().join(relative);
            assert_eq!(
                fs::read_to_string(directory.join("collector.exit")).unwrap(),
                format!("{collector}\n")
            );
            assert_eq!(
                fs::read_to_string(directory.join("log-capture.exit")).unwrap(),
                format!("{capture}\n")
            );
            if capture == 0 {
                let log = fs::read_to_string(directory.join("collector.log")).unwrap();
                assert!(log.contains("transport control: stdout"));
                assert!(log.contains("transport control: stderr"));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn scheduled_shard_fragment_executes_all_56_workloads_with_exact_pins_and_order() {
        let control = ShardControl::new();
        for count in ["1000", "10000"] {
            for flush in ["default-auto-flush", "diagnostic-no-auto-flush"] {
                expect_exit(
                    &control.run(count, flush, ""),
                    0,
                    "mock shard collector transport control",
                );
            }
        }
        let invocations = control.invocations();
        assert_eq!(invocations.len(), 56);
        let expected = names()
            .into_iter()
            .flat_map(|name| [format!("aa/{name}"), format!("ab/{name}")])
            .collect::<Vec<_>>();
        for (invocation, relative) in invocations.iter().zip(expected) {
            assert_eq!(invocation["relative"], relative);
            assert_eq!(
                invocation["args"],
                json!([
                    "--exact",
                    "release_abba::release_cli_abba_1k_10k",
                    "--ignored",
                    "--nocapture"
                ])
            );
            let env = &invocation["env"];
            assert_eq!(env["WORKLOAD"], relative.split_once('/').unwrap().1);
            assert_eq!(
                env["OUTPUT"],
                control.root.path().join(&relative).display().to_string()
            );
            assert_eq!(env["ABBA_BLOCKS"], "99");
            assert_eq!(env["QUIET_RUNNER"], "1");
            assert_eq!(env["NEGATIVE_EXTRA_WORK"], "1");
            assert!(env["BUDGET_PCT"].is_null());
            assert_eq!(
                serde_json::from_str::<Value>(env["BUDGETS_JSON"].as_str().unwrap()).unwrap(),
                json!({READY: 7})
            );
            for (suffix, candidate, baseline) in [
                (
                    "BINARY",
                    "candidate-release-br".into(),
                    "baseline-release-br".into(),
                ),
                ("SHA256", "2".repeat(64), "1".repeat(64)),
                (
                    "LOCKFILE",
                    "candidate-Cargo.lock".into(),
                    "baseline-Cargo.lock".into(),
                ),
                ("LOCK_SHA256", "4".repeat(64), "3".repeat(64)),
                ("SOURCE", "b".repeat(40), "a".repeat(40)),
            ] {
                assert_eq!(env[format!("CANDIDATE_{suffix}")], candidate);
                assert_eq!(
                    env[format!("BASELINE_{suffix}")],
                    if relative.starts_with("aa/") {
                        candidate
                    } else {
                        baseline
                    }
                );
            }
            control.assert_pipeline(&relative, 0, 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn scheduled_shard_fragment_continues_after_collector_and_log_capture_failures() {
        let control = ShardControl::new();
        let first = format!("aa/{FIRST}");
        expect_exit(
            &control.run("1000", "default-auto-flush", &first),
            2,
            "mock shard collector transport control",
        );
        assert_eq!(control.invocations().len(), 14);
        control.assert_pipeline(&first, 37, 0);
        control.assert_pipeline("ab/1000-close-default-auto-flush", 0, 0);

        let control = ShardControl::new();
        // A real filesystem obstruction makes tee fail; the workflow still
        // records both pipeline exits and attempts every later workload.
        fs::create_dir_all(control.root.path().join(&first).join("collector.log")).unwrap();
        expect_exit(
            &control.run("1000", "default-auto-flush", ""),
            2,
            "mock shard collector transport control",
        );
        assert_eq!(control.invocations().len(), 14);
        control.assert_pipeline(&first, 0, 1);
        control.assert_pipeline("ab/1000-close-default-auto-flush", 0, 0);
    }

    #[cfg(unix)]
    #[test]
    fn scheduled_shard_fragment_refuses_invalid_selectors_before_launching() {
        for (count, flush) in [("100", "default-auto-flush"), ("1000", "unknown"), ("", "")] {
            let control = ShardControl::new();
            expect_exit(&control.run(count, flush, ""), 2, "invalid workload shard");
            assert!(!control.root.path().join("invocations.jsonl").exists());
        }
    }

    fn raw_control(name: &str, blocks: usize) -> Value {
        let mut parts = name.splitn(3, '-');
        let count = parts.next().unwrap();
        let command = parts.next().unwrap();
        let flush = parts.next().unwrap();
        let block_ids = (2..blocks + 2)
            .flat_map(|block| [block, block])
            .collect::<Vec<_>>();
        json!({"samples_ms": vec![10; 2 * blocks], "exit_codes": vec![0; 2 * blocks],
            "block_ids": block_ids, "metadata": {
            "command": command, "issue_count": count, "flush_mode": flush,
            "build_profile": "release", "dataset_sha256": "1".repeat(64),
            "binary_sha256": "2".repeat(64), "lockfile_sha256": "3".repeat(64),
            "source_revision": "arithmetic-control-source", "cache_protocol": "arithmetic-control-cache",
            "host": "arithmetic-control-host", "cpu": "arithmetic-control-cpu",
            "os": "arithmetic-control-os", "filesystem": "arithmetic-control-fs",
            "target": "arithmetic-control-target", "features": "arithmetic-control-features",
            "engine": "arithmetic-control-engine",
            "sampling_protocol": "abba_two_per_side_iid_blocks_assumed_v1"
        }})
    }

    fn timing_control(candidate: f64) -> Value {
        json!({"baseline_ms": 10, "candidate_ms": candidate,
            "delta_ms": candidate - 10.0, "delta_pct": (candidate / 10.0 - 1.0) * 100.0})
    }

    fn quantile_control(blocks: usize, ranks: [usize; 2], low: f64, high: f64) -> Value {
        let finite_upper = ranks[1] <= blocks;
        json!({"lower_rank": ranks[0], "upper_rank": ranks[1],
            "baseline_lower_ms": 10, "candidate_lower_ms": low,
            "baseline_upper_ms": finite_upper.then_some(10),
            "candidate_upper_ms": finite_upper.then_some(high),
            "lower": finite_upper.then(|| timing_control(low)),
            "upper": finite_upper.then(|| timing_control(high))})
    }

    fn bounded_summary_control(
        name: &str,
        blocks: usize,
        code: usize,
        low: f64,
        high: f64,
        budget: usize,
    ) -> Value {
        // Hand-checked ranks for these two serialization controls. No invocation
        // of the comparator under test, generated confidence, or live claim.
        let (median_ranks, p95_ranks) = match blocks {
            10 => ([1, 10], [7, 11]),
            99 => ([37, 63], [88, 99]),
            _ => panic!("no hand-checked fixture ranks for {blocks} blocks"),
        };
        json!({"gate_exit": code, "comparison": {
            "state": (["pass", "regression", "inconclusive"][code]), "budget_pct": budget,
            "command": name.split('-').nth(1).unwrap(),
            "diagnostic": format!("arithmetic transport control {name}: budget={budget}"),
            "median": timing_control(low + (high - low) / 2.0), "p95": timing_control(high),
            "observed_support": {"method": "observed_support_extrema_not_confidence_interval",
                "lower_ms": low - 10.0, "upper_ms": high - 10.0,
                "lower_pct": (low / 10.0 - 1.0) * 100.0, "upper_pct": (high / 10.0 - 1.0) * 100.0},
            "uncertainty": {
                "method": "binomial_order_statistics_of_block_minima_and_maxima",
                "assumption": "independent identically distributed whole ABBA blocks; dependence within a block allowed; runner load does not establish this assumption",
                "coverage_scope": "joint median and p95 for this comparison only; not simultaneous across workloads or repeated comparisons",
                "confidence_level": 0.95, "one_sided_error_probability": 0.00625, "block_count": blocks,
                "median": quantile_control(blocks, median_ranks, low, high),
                "p95": quantile_control(blocks, p95_ranks, low, high)
            }
        }})
    }

    fn summary_control(name: &str, code: usize) -> Value {
        let (low, high) = match code {
            0 => (10.0, 10.0),
            1 => (12.0, 12.0),
            2 => (10.0, 12.0),
            _ => panic!("invalid control exit {code}"),
        };
        bounded_summary_control(name, CONTROL_BLOCKS, code, low, high, 7)
    }

    fn abba_control(blocks: usize) -> Value {
        Value::Array(
            (0..4 * (blocks + 2))
                .map(|index| {
                    let block = index / 4;
                    let position = index % 4;
                    json!({
                        "side": ([0, 1, 1, 0][position]), "block": block, "position": position,
                        "warmup": block < 2, "elapsed_ms": 10, "exit_code": 0,
                        "invocations": 1, "args": ["version", "--json"], "stdout": "{}", "stderr": "",
                        "load_before": json!({"normalized_load_one": 0.1, "logical_cpus": 2, "compiler_processes": []}).to_string()
                    })
                })
                .collect(),
        )
    }

    struct ReceiptControl {
        root: tempfile::TempDir,
        budgets: Value,
        blocks: usize,
        shards_result: &'static str,
    }

    fn run_control(name: &str, code: usize) -> Value {
        json!({"state": "measurements_completed", "scope": "separate_exact_workload",
            "selected_workload": name, "completed_workloads": 1, "latency_gate_exits": [code]})
    }

    impl ReceiptControl {
        fn new(omitted: Option<&str>) -> Self {
            Self::with_blocks(omitted, CONTROL_BLOCKS)
        }

        fn with_blocks(omitted: Option<&str>, blocks: usize) -> Self {
            let control = Self {
                root: tempfile::TempDir::new().unwrap(),
                budgets: Value::Object(names().into_iter().map(|name| (name, json!(7))).collect()),
                blocks,
                shards_result: "success",
            };
            let code = usize::from(blocks == 10) * 2;
            for cohort in ["aa", "ab"] {
                for name in names() {
                    let relative_directory = format!("{cohort}/{name}");
                    if omitted == Some(relative_directory.as_str()) {
                        continue;
                    }
                    let directory = control.root.path().join(&relative_directory);
                    fs::create_dir_all(&directory).unwrap();
                    for exit in ["collector.exit", "log-capture.exit"] {
                        if omitted != Some(format!("{relative_directory}/{exit}").as_str()) {
                            fs::write(directory.join(exit), "0\n").unwrap();
                        }
                    }
                    let run_path = format!("{relative_directory}/run.json");
                    if omitted != Some(run_path.as_str()) {
                        control.write(&run_path, &run_control(&name, code));
                    }
                    control.write(
                        &format!("{cohort}/{name}/{name}-summary.json"),
                        &bounded_summary_control(&name, blocks, code, 10.0, 10.0, 7),
                    );
                    let raw_path = format!("{cohort}/{name}/{name}-raw.json");
                    if omitted != Some(raw_path.as_str()) {
                        control.write(&raw_path, &abba_control(blocks));
                    }
                    for side in 0..2 {
                        let path = format!("{cohort}/{name}/{name}-{side}.json");
                        if omitted != Some(path.as_str()) {
                            control.write(&path, &raw_control(&name, blocks));
                        }
                    }
                }
            }
            control
        }

        fn write(&self, path: &str, value: &Value) {
            fs::write(
                self.root.path().join(path),
                serde_json::to_vec(value).unwrap(),
            )
            .unwrap();
        }

        fn run(&self) -> Output {
            run_fragment(
                GATE,
                self.root.path(),
                &[
                    ("BR_PERF_EVIDENCE", self.root.path().display().to_string()),
                    ("BR_PERF_ABBA_BLOCKS", self.blocks.to_string()),
                    ("BR_PERF_SHARDS_RESULT", self.shards_result.to_string()),
                    ("BR_PERF_BUDGETS_JSON", self.budgets.to_string()),
                    ("BR_PERF_BASELINE_SHA256", "2".repeat(64)),
                    ("BR_PERF_CANDIDATE_SHA256", "2".repeat(64)),
                    ("BR_PERF_BASELINE_LOCK_SHA256", "3".repeat(64)),
                    ("BR_PERF_CANDIDATE_LOCK_SHA256", "3".repeat(64)),
                    (
                        "BR_PERF_BASELINE_SOURCE",
                        "arithmetic-control-source".into(),
                    ),
                    (
                        "BR_PERF_CANDIDATE_SOURCE",
                        "arithmetic-control-source".into(),
                    ),
                ],
            )
        }

        fn decide(&self, cohort: &str, code: usize) {
            self.write(
                &format!("{cohort}/{FIRST}/{FIRST}-summary.json"),
                &summary_control(FIRST, code),
            );
            let mut receipt = raw_control(FIRST, CONTROL_BLOCKS);
            let mut raw = abba_control(CONTROL_BLOCKS);
            let pair = match code {
                0 => [10, 10],
                1 => [12, 12],
                2 => [10, 12],
                _ => panic!("invalid control exit {code}"),
            };
            receipt["samples_ms"] = json!(pair.repeat(CONTROL_BLOCKS));
            for sample in raw.as_array_mut().unwrap() {
                if sample["side"] == 1 {
                    sample["elapsed_ms"] = json!(pair[usize::from(sample["position"] == 2)]);
                }
            }
            self.write(&format!("{cohort}/{FIRST}/{FIRST}-1.json"), &receipt);
            self.write(&format!("{cohort}/{FIRST}/{FIRST}-raw.json"), &raw);
            self.write(
                &format!("{cohort}/{FIRST}/run.json"),
                &run_control(FIRST, code),
            );
        }
    }

    #[cfg(unix)]
    fn planted_control() -> ReceiptControl {
        use std::os::unix::fs::PermissionsExt;

        let control = ReceiptControl {
            root: tempfile::TempDir::new().unwrap(),
            budgets: json!({}),
            blocks: 10,
            shards_result: "success",
        };
        fs::create_dir(control.root.path().join("planted-ready")).unwrap();
        control.write(
            "planted-ready/run.json",
            &json!({"state": "measurements_completed", "scope": "separate_exact_workload",
                "selected_workload": READY, "completed_workloads": 1, "latency_gate_exits": [1]}),
        );
        let mut summary = bounded_summary_control(READY, 10, 1, 200.0, 200.0, 100);
        summary["negative_control_ready_invocations"] = json!(20);
        summary["budget_origin"] = json!("global diagnostic control; not an accepted SLO");
        control.write(&format!("planted-ready/{READY}-summary.json"), &summary);
        let mut raw = abba_control(10);
        for sample in raw.as_array_mut().unwrap() {
            let invocations = if sample["side"] == 0 { 1 } else { 20 };
            sample["invocations"] = json!(invocations);
            sample["elapsed_ms"] = json!(10 * invocations);
            sample["args"] = json!(["ready", "--json"]);
            sample["stdout"] = json!("[]\n".repeat(invocations));
        }
        control.write(&format!("planted-ready/{READY}-raw.json"), &raw);
        for side in 0..2 {
            let mut receipt = raw_control(READY, 10);
            receipt["samples_ms"] = json!(vec![if side == 0 { 10 } else { 200 }; 20]);
            control.write(&format!("planted-ready/{READY}-{side}.json"), &receipt);
        }
        // This mock collector records the real shell boundary. Its prewritten
        // receipts exercise transport/refusal only, never live performance.
        let harness = control.root.path().join("mock-collector");
        fs::write(
            &harness,
            r"#!/usr/bin/env python3
import json, os, pathlib, sys
keys = ('OUTPUT', 'WORKLOAD', 'NEGATIVE_EXTRA_WORK', 'BUDGET_PCT', 'BUDGETS_JSON',
        'ABBA_BLOCKS', 'QUIET_RUNNER', 'BASELINE_BINARY', 'CANDIDATE_BINARY',
        'BASELINE_SHA256', 'CANDIDATE_SHA256', 'BASELINE_LOCKFILE', 'CANDIDATE_LOCKFILE',
        'BASELINE_LOCK_SHA256', 'CANDIDATE_LOCK_SHA256', 'BASELINE_SOURCE', 'CANDIDATE_SOURCE')
pathlib.Path(os.environ['BR_PERF_EVIDENCE'], 'invocation.json').write_text(json.dumps({
    'args': sys.argv[1:], 'env': {key: os.environ.get('BR_PERF_' + key) for key in keys}}))
print('mock collector transport control: stdout')
print('mock collector transport control: stderr', file=sys.stderr)
raise SystemExit(int(os.environ['BR_PERF_FIXTURE_EXIT']))
",
        )
        .unwrap();
        fs::set_permissions(harness, fs::Permissions::from_mode(0o755)).unwrap();
        control
    }

    #[cfg(unix)]
    fn run_planted(control: &ReceiptControl, collector_exit: i32) -> Output {
        let root = control.root.path();
        let mut variables = [
            ("BR_PERF_EVIDENCE", root.display().to_string()),
            (
                "BR_PERF_HARNESS",
                root.join("mock-collector").display().to_string(),
            ),
            ("BR_PERF_FIXTURE_EXIT", collector_exit.to_string()),
            ("BR_PERF_BUDGETS_JSON", json!({READY: 7}).to_string()),
            ("BR_PERF_WORKLOAD", "wrong-inherited-selector".into()),
        ]
        .map(|(key, value)| (key.to_string(), value))
        .to_vec();
        for (suffix, candidate) in [
            ("BINARY", "candidate-release-br".into()),
            ("SHA256", "2".repeat(64)),
            ("LOCKFILE", "candidate-Cargo.lock".into()),
            ("LOCK_SHA256", "3".repeat(64)),
            ("SOURCE", "arithmetic-control-source".into()),
        ] {
            variables.push((
                format!("BR_PERF_BASELINE_{suffix}"),
                "wrong-baseline-pin".into(),
            ));
            variables.push((format!("BR_PERF_CANDIDATE_{suffix}"), candidate));
        }
        let borrowed = variables
            .iter()
            .map(|(key, value)| (key.as_str(), value.clone()))
            .collect::<Vec<_>>();
        run_fragment(PLANTED, root, &borrowed)
    }

    #[cfg(unix)]
    #[test]
    fn scheduled_planted_fragment_selects_same_candidate_and_preserves_collector_failure() {
        let control = planted_control();
        expect_exit(&run_planted(&control, 0), 0, "Regression observed");
        let invocation: Value =
            serde_json::from_slice(&fs::read(control.root.path().join("invocation.json")).unwrap())
                .unwrap();
        assert_eq!(
            invocation["args"],
            json!([
                "--exact",
                "release_abba::release_cli_abba_1k_10k",
                "--ignored",
                "--nocapture"
            ])
        );
        let env = &invocation["env"];
        for (key, expected) in [
            ("WORKLOAD", READY),
            ("NEGATIVE_EXTRA_WORK", "20"),
            ("BUDGET_PCT", "100"),
            ("ABBA_BLOCKS", "10"),
            ("QUIET_RUNNER", "1"),
        ] {
            assert_eq!(env[key], expected, "{key}: {invocation}");
        }
        assert_eq!(env["BUDGETS_JSON"], Value::Null);
        assert_eq!(
            env["OUTPUT"],
            control
                .root
                .path()
                .join("planted-ready")
                .display()
                .to_string()
        );
        for suffix in ["BINARY", "SHA256", "LOCKFILE", "LOCK_SHA256", "SOURCE"] {
            assert_eq!(
                env[format!("BASELINE_{suffix}")],
                env[format!("CANDIDATE_{suffix}")]
            );
            assert_ne!(env[format!("BASELINE_{suffix}")], "wrong-baseline-pin");
        }
        expect_exit(
            &run_planted(&control, 37),
            37,
            "mock collector transport control",
        );
        let directory = control.root.path().join("planted-ready");
        assert_eq!(
            fs::read_to_string(directory.join("collector.exit")).unwrap(),
            "37\n"
        );
        assert_eq!(
            fs::read_to_string(directory.join("log-capture.exit")).unwrap(),
            "0\n"
        );
        let log = fs::read_to_string(directory.join("collector.log")).unwrap();
        assert!(log.contains("transport control: stdout"));
        assert!(log.contains("transport control: stderr"));
        assert!(directory.join(format!("{READY}-raw.json")).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn scheduled_planted_fragment_refuses_incomplete_or_wrong_control_receipts() {
        for (file, key, value, diagnostic) in [
            (
                "run",
                "selected_workload",
                json!(FIRST),
                "one completed selected Regression",
            ),
            (
                "summary",
                "gate_exit",
                json!(0),
                "did not establish Regression",
            ),
            (
                "summary",
                "negative_control_ready_invocations",
                json!(1),
                "did not establish Regression",
            ),
            (
                "run",
                "completed_workloads",
                json!(true),
                "invalid integer control completion",
            ),
            (
                "summary",
                "gate_exit",
                json!(true),
                "invalid numeric control decision",
            ),
        ] {
            let control = planted_control();
            let path = if file == "run" {
                "planted-ready/run.json".into()
            } else {
                format!("planted-ready/{READY}-summary.json")
            };
            let mut value_in_file: Value =
                serde_json::from_slice(&fs::read(control.root.path().join(&path)).unwrap())
                    .unwrap();
            value_in_file[key] = value;
            control.write(&path, &value_in_file);
            expect_exit(&run_planted(&control, 0), 2, diagnostic);
        }
        let control = planted_control();
        control.write(&format!("planted-ready/{READY}-raw.json"), &json!([]));
        expect_exit(
            &run_planted(&control, 0),
            2,
            "incomplete control raw ABBA log",
        );
        let control = planted_control();
        let mut receipt = raw_control(READY, 10);
        receipt["exit_codes"] = json!(vec![false; 20]);
        control.write(&format!("planted-ready/{READY}-0.json"), &receipt);
        expect_exit(
            &run_planted(&control, 0),
            2,
            "expected twenty successful retained samples per side",
        );
        let control = planted_control();
        let mut receipt = raw_control(READY, 10);
        receipt["metadata"]["binary_sha256"] = json!("4".repeat(64));
        control.write(&format!("planted-ready/{READY}-0.json"), &receipt);
        expect_exit(
            &run_planted(&control, 0),
            2,
            "differs from candidate artifact pin",
        );
    }

    #[test]
    fn scheduled_receipt_fragment_has_actual_pass_regression_and_unknown_exits() {
        let control = ReceiptControl::new(None);
        expect_exit(&control.run(), 0, "all 28 matched latency workloads");
        control.decide("ab", 1);
        let regression = control.run();
        expect_exit(&regression, 1, "benchmark_gate: regression");
        assert!(String::from_utf8_lossy(&regression.stdout).contains(FIRST));
        control.decide("ab", 2);
        expect_exit(&control.run(), 2, "candidate comparisons are inconclusive");
        control.decide("ab", 0);
        control.decide("aa", 1);
        expect_exit(&control.run(), 2, "A/A control did not establish stable");
    }

    #[test]
    fn scheduled_receipt_fragment_requires_every_shard_and_workload() {
        let mut control = ReceiptControl::new(None);
        expect_exit(&control.run(), 0, "all 28 matched latency workloads");
        for result in ["failure", "cancelled", "skipped", "", "unknown"] {
            control.shards_result = result;
            expect_exit(&control.run(), 2, "inconclusive");
        }
        for name in [
            "1000-close-default-auto-flush",
            "1000-close-diagnostic-no-auto-flush",
            "10000-close-default-auto-flush",
            "10000-close-diagnostic-no-auto-flush",
        ] {
            let control = ReceiptControl::new(Some(&format!("ab/{name}")));
            expect_exit(&control.run(), 2, "inconclusive");
        }
        let control = ReceiptControl::new(None);
        fs::create_dir(control.root.path().join("ab/duplicate-or-unknown-workload")).unwrap();
        expect_exit(
            &control.run(),
            2,
            "expected exactly all 28 workload directories",
        );
    }

    #[test]
    fn scheduled_receipt_fragment_requires_unique_exact_workload_scope() {
        let control = ReceiptControl::new(None);
        for (key, value) in [
            ("state", json!("incomplete")),
            ("scope", json!("full_matrix")),
            ("selected_workload", json!(READY)),
            ("completed_workloads", json!(0)),
            ("completed_workloads", json!(2)),
            ("completed_workloads", json!(true)),
            ("latency_gate_exits", json!([])),
            ("latency_gate_exits", json!([0, 0])),
            ("latency_gate_exits", json!([1])),
            ("latency_gate_exits", json!([true])),
        ] {
            let mut run = run_control(FIRST, 0);
            run[key] = value;
            control.write(&format!("ab/{FIRST}/run.json"), &run);
            expect_exit(&control.run(), 2, "inconclusive");
        }
        let control = ReceiptControl::new(Some(&format!("ab/{FIRST}/run.json")));
        expect_exit(&control.run(), 2, "inconclusive");
    }

    #[test]
    fn scheduled_receipt_fragment_requires_both_pipeline_exits() {
        for file in ["collector.exit", "log-capture.exit"] {
            let path = format!("ab/{FIRST}/{file}");
            let missing = ReceiptControl::new(Some(&path));
            expect_exit(&missing.run(), 2, "inconclusive");
            let control = ReceiptControl::new(None);
            for status in ["1\n", "101\n", "true\n", ""] {
                fs::write(control.root.path().join(&path), status).unwrap();
                expect_exit(&control.run(), 2, "inconclusive");
            }
        }
    }

    #[test]
    fn scheduled_receipt_fragment_requires_same_host_for_workload_aa_and_ab() {
        let control = ReceiptControl::new(None);
        // Each A/B pair remains internally matched. Its A/A control must have
        // observed the same environment, rather than a different shard host.
        for side in 0..2 {
            let mut receipt = raw_control(FIRST, CONTROL_BLOCKS);
            receipt["metadata"]["host"] = json!("different-shard-host");
            control.write(&format!("ab/{FIRST}/{FIRST}-{side}.json"), &receipt);
        }
        expect_exit(&control.run(), 2, "inconclusive");
    }

    #[test]
    fn scheduled_receipt_fragment_refuses_legacy_or_malformed_quantile_evidence() {
        let control = ReceiptControl::new(None);
        for (pointer, value) in [
            ("/command", json!("wrong-command")),
            ("/uncertainty", Value::Null),
            (
                "/uncertainty",
                json!({"lower_ms": 0, "upper_ms": 0, "lower_pct": 0, "upper_pct": 0}),
            ),
            ("/uncertainty/method", json!("observed_support")),
            ("/uncertainty/assumption", json!("runner load proves IID")),
            (
                "/uncertainty/coverage_scope",
                json!("simultaneous across all workloads"),
            ),
            ("/uncertainty/confidence_level", json!(0.99)),
            ("/uncertainty/one_sided_error_probability", json!(0.05)),
            ("/uncertainty/block_count", json!(98)),
            ("/uncertainty/block_count", json!(true)),
            ("/uncertainty/median", Value::Null),
            ("/uncertainty/p95", Value::Null),
            ("/uncertainty/median/lower_rank", json!(true)),
            ("/uncertainty/p95/upper_rank", json!(101)),
            ("/uncertainty/median/baseline_lower_ms", json!(11)),
            ("/uncertainty/p95/candidate_upper_ms", json!(11)),
            ("/uncertainty/median/lower/delta_ms", json!(1)),
            ("/uncertainty/p95/upper/delta_pct", json!(1)),
            ("/median/baseline_ms", json!(11)),
            ("/p95/candidate_ms", json!(11)),
        ] {
            let mut summary = summary_control(FIRST, 0);
            *summary["comparison"].pointer_mut(pointer).unwrap() = value;
            control.write(&format!("ab/{FIRST}/{FIRST}-summary.json"), &summary);
            expect_exit(&control.run(), 2, "benchmark_gate: inconclusive");
        }
        let mut summary = summary_control(FIRST, 0);
        summary["comparison"]["uncertainty"]
            .as_object_mut()
            .unwrap()
            .remove("p95");
        control.write(&format!("ab/{FIRST}/{FIRST}-summary.json"), &summary);
        expect_exit(&control.run(), 2, "benchmark_gate: inconclusive");
    }

    #[test]
    fn scheduled_receipt_fragment_requires_bounded_p95_even_with_equal_point_samples() {
        let control = ReceiptControl::new(None);
        let mut summary = summary_control(FIRST, 0);
        let p95 = &mut summary["comparison"]["uncertainty"]["p95"];
        p95["upper_rank"] = json!(100);
        for key in ["baseline_upper_ms", "candidate_upper_ms", "lower", "upper"] {
            p95[key] = Value::Null;
        }
        control.write(&format!("ab/{FIRST}/{FIRST}-summary.json"), &summary);
        expect_exit(
            &control.run(),
            2,
            "decision is not established by the quantile bounds",
        );
        // A planted median lower bound also cannot be fabricated from a state
        // label: the same observations only support Pass under this budget.
        let mut summary = summary_control(FIRST, 0);
        summary["gate_exit"] = json!(1);
        summary["comparison"]["state"] = json!("regression");
        control.write(&format!("ab/{FIRST}/{FIRST}-summary.json"), &summary);
        expect_exit(
            &control.run(),
            2,
            "decision is not established by the quantile bounds",
        );
    }

    #[test]
    fn scheduled_receipt_fragment_requires_explicit_aligned_blocks_and_declared_counts() {
        let control = ReceiptControl::new(None);
        for ids in [
            Value::Null,
            json!([]),
            json!(vec![2; 2 * CONTROL_BLOCKS]),
            json!(
                (0..CONTROL_BLOCKS)
                    .flat_map(|block| [block, block])
                    .collect::<Vec<_>>()
            ),
            json!(
                (2..CONTROL_BLOCKS + 2)
                    .flat_map(|block| [block, block + 1])
                    .collect::<Vec<_>>()
            ),
            json!(vec![true; 2 * CONTROL_BLOCKS]),
        ] {
            let mut receipt = raw_control(FIRST, CONTROL_BLOCKS);
            receipt["block_ids"] = ids;
            control.write(&format!("ab/{FIRST}/{FIRST}-1.json"), &receipt);
            expect_exit(&control.run(), 2, "explicit block IDs differ from raw ABBA");
        }
        for key in ["block_ids", "sampling_protocol"] {
            let mut receipt = raw_control(FIRST, CONTROL_BLOCKS);
            if key == "block_ids" {
                receipt.as_object_mut().unwrap().remove(key);
            } else {
                receipt["metadata"].as_object_mut().unwrap().remove(key);
            }
            control.write(&format!("ab/{FIRST}/{FIRST}-1.json"), &receipt);
            expect_exit(&control.run(), 2, "benchmark_gate: inconclusive");
        }
        control.write(
            &format!("ab/{FIRST}/{FIRST}-1.json"),
            &raw_control(FIRST, 10),
        );
        expect_exit(&control.run(), 2, "invalid raw samples");
    }

    #[test]
    fn scheduled_receipt_fragment_keeps_overflowed_support_descriptive() {
        let control = ReceiptControl::new(None);
        // One small baseline observation makes the support percentage overflow;
        // the selected median/p95 block ranks and both point estimates remain 10.
        let mut receipt = raw_control(FIRST, CONTROL_BLOCKS);
        receipt["samples_ms"][0] = json!(f64::MIN_POSITIVE);
        control.write(&format!("ab/{FIRST}/{FIRST}-0.json"), &receipt);
        let mut raw = abba_control(CONTROL_BLOCKS);
        raw[8]["elapsed_ms"] = json!(f64::MIN_POSITIVE);
        control.write(&format!("ab/{FIRST}/{FIRST}-raw.json"), &raw);
        let mut summary = summary_control(FIRST, 0);
        summary["comparison"]["observed_support"]["upper_ms"] = json!(10.0 - f64::MIN_POSITIVE);
        summary["comparison"]["observed_support"]["upper_pct"] = Value::Null;
        control.write(&format!("ab/{FIRST}/{FIRST}-summary.json"), &summary);
        expect_exit(&control.run(), 0, "all 28 matched latency workloads");
    }

    #[test]
    fn scheduled_receipt_fragment_ten_blocks_cannot_establish_a_full_pass() {
        let control = ReceiptControl::with_blocks(None, 10);
        expect_exit(
            &control.run(),
            2,
            "A/A control did not establish stable comparisons",
        );
        // Even a forged finite p95 bound using the observed maximum cannot make
        // the live ten-block default sufficient for this declared confidence.
        let mut summary = bounded_summary_control(FIRST, 10, 0, 10.0, 10.0, 7);
        summary["comparison"]["uncertainty"]["p95"] = quantile_control(10, [7, 10], 10.0, 10.0);
        control.write(&format!("aa/{FIRST}/{FIRST}-summary.json"), &summary);
        expect_exit(
            &control.run(),
            2,
            "insufficient blocks for a finite p95 upper endpoint",
        );
    }

    #[test]
    fn scheduled_receipt_fragment_refuses_overflowed_budget_allowance() {
        let mut control = ReceiptControl::new(None);
        control.budgets[FIRST] = json!(f64::MAX);
        let mut summary = summary_control(FIRST, 0);
        summary["comparison"]["budget_pct"] = json!(f64::MAX);
        let timing = json!({"baseline_ms": 1000, "candidate_ms": 1000,
            "delta_ms": 0, "delta_pct": 0});
        for quantile in ["median", "p95"] {
            summary["comparison"][quantile] = timing.clone();
            let interval = &mut summary["comparison"]["uncertainty"][quantile];
            for key in [
                "baseline_lower_ms",
                "baseline_upper_ms",
                "candidate_lower_ms",
                "candidate_upper_ms",
            ] {
                interval[key] = json!(1000);
            }
            interval["lower"] = timing.clone();
            interval["upper"] = timing.clone();
        }
        // Both cohorts use the calibrated budget; the A/A receipt is the first
        // attempted forged Pass. Equal samples do not make an infinite allowance valid.
        for cohort in ["aa", "ab"] {
            control.write(&format!("{cohort}/{FIRST}/{FIRST}-summary.json"), &summary);
            for side in 0..2 {
                let mut receipt = raw_control(FIRST, CONTROL_BLOCKS);
                receipt["samples_ms"] = json!(vec![1000; 2 * CONTROL_BLOCKS]);
                control.write(&format!("{cohort}/{FIRST}/{FIRST}-{side}.json"), &receipt);
            }
            let mut raw = abba_control(CONTROL_BLOCKS);
            for sample in raw.as_array_mut().unwrap() {
                sample["elapsed_ms"] = json!(1000);
            }
            control.write(&format!("{cohort}/{FIRST}/{FIRST}-raw.json"), &raw);
        }
        expect_exit(
            &control.run(),
            2,
            "decision is not established by the quantile bounds",
        );
    }

    #[cfg(unix)]
    #[test]
    fn scheduled_planted_fragment_requires_block_alignment_and_real_quantile_regression() {
        let control = planted_control();
        // Positive control is deliberately median-only inference: twenty
        // observations are ten blocks, so both p95 upper endpoints are null.
        let summary: Value = serde_json::from_slice(
            &fs::read(
                control
                    .root
                    .path()
                    .join(format!("planted-ready/{READY}-summary.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(summary["comparison"]["uncertainty"]["p95"]["upper"].is_null());
        expect_exit(&run_planted(&control, 0), 0, "Regression observed");
        for (pointer, value) in [
            ("/uncertainty", Value::Null),
            (
                "/uncertainty",
                json!({"lower_ms": 190, "upper_ms": 190, "lower_pct": 1900, "upper_pct": 1900}),
            ),
            ("/uncertainty/block_count", json!(99)),
            ("/uncertainty/confidence_level", json!(0.99)),
            ("/uncertainty/median/lower/delta_ms", json!(0)),
            ("/uncertainty/median/candidate_lower_ms", json!(10)),
            ("/uncertainty/p95/upper_rank", json!(10)),
            ("/uncertainty/p95/upper", timing_control(200.0)),
        ] {
            let mut malformed = summary.clone();
            *malformed["comparison"].pointer_mut(pointer).unwrap() = value;
            control.write(&format!("planted-ready/{READY}-summary.json"), &malformed);
            expect_exit(
                &run_planted(&control, 0),
                2,
                "benchmark_sensitivity: inconclusive",
            );
        }
        control.write(&format!("planted-ready/{READY}-summary.json"), &summary);
        for ids in [
            Value::Null,
            json!([]),
            json!(vec![2; 20]),
            json!((0..10).flat_map(|block| [block, block]).collect::<Vec<_>>()),
        ] {
            let mut receipt = raw_control(READY, 10);
            receipt["block_ids"] = ids;
            control.write(&format!("planted-ready/{READY}-0.json"), &receipt);
            expect_exit(
                &run_planted(&control, 0),
                2,
                "explicit control block IDs differ",
            );
        }
    }

    #[test]
    fn scheduled_receipt_fragment_refuses_missing_corrupt_and_failed_evidence() {
        let missing = ReceiptControl::new(Some(&format!("ab/{FIRST}/{FIRST}-0.json")));
        expect_exit(&missing.run(), 2, "inconclusive");
        let control = ReceiptControl::new(None);
        fs::write(
            control.root.path().join(format!("ab/{FIRST}/run.json")),
            "{broken",
        )
        .unwrap();
        expect_exit(&control.run(), 2, "inconclusive");
        control.decide("ab", 0);
        fs::write(
            control
                .root
                .path()
                .join(format!("ab/{FIRST}/collector.exit")),
            "101\n",
        )
        .unwrap();
        expect_exit(&control.run(), 2, "collector did not complete successfully");
    }

    #[test]
    fn scheduled_receipt_fragment_refuses_vacuous_or_mismatched_samples() {
        let control = ReceiptControl::new(None);
        for receipt in [
            json!({"samples_ms": [], "exit_codes": []}),
            json!({"samples_ms": vec![0; 20], "exit_codes": vec![0; 20]}),
            json!({"samples_ms": vec![10; 19], "exit_codes": vec![0; 19]}),
            json!({"samples_ms": vec![10; 2 * CONTROL_BLOCKS], "exit_codes": vec![1; 2 * CONTROL_BLOCKS]}),
            json!({"samples_ms": vec![10; 2 * CONTROL_BLOCKS], "exit_codes": vec![false; 2 * CONTROL_BLOCKS]}),
            json!({"samples_ms": vec![10; 21], "exit_codes": vec![0; 21]}),
        ] {
            control.write(&format!("ab/{FIRST}/{FIRST}-1.json"), &receipt);
            expect_exit(&control.run(), 2, "inconclusive");
        }
        let control = ReceiptControl::new(None);
        let mut receipt = raw_control(FIRST, CONTROL_BLOCKS);
        receipt["metadata"]["binary_sha256"] = json!("4".repeat(64));
        control.write(&format!("aa/{FIRST}/{FIRST}-1.json"), &receipt);
        expect_exit(
            &control.run(),
            2,
            "binary_sha256 differs from built artifact pin",
        );
    }

    #[test]
    fn scheduled_receipt_fragment_refuses_corrupt_or_mismatched_provenance() {
        let control = ReceiptControl::new(None);
        for (key, value, reason) in [
            ("binary_sha256", "malformed", "malformed binary_sha256"),
            ("source_revision", "unknown", "placeholder provenance"),
            ("build_profile", "debug", "workload identity mismatch"),
            ("command", "wrong-command", "workload identity mismatch"),
            ("host", "different-host", "matched metadata differs"),
        ] {
            let mut receipt = raw_control(FIRST, CONTROL_BLOCKS);
            receipt["metadata"][key] = json!(value);
            control.write(&format!("ab/{FIRST}/{FIRST}-1.json"), &receipt);
            expect_exit(&control.run(), 2, reason);
        }
    }

    #[test]
    fn scheduled_receipt_fragment_requires_every_calibrated_workload_budget() {
        let mut control = ReceiptControl::new(None);
        for budgets in [
            json!({}),
            json!({FIRST: 7}),
            json!({"unknown": 7}),
            json!(names()),
        ] {
            control.budgets = budgets;
            expect_exit(
                &control.run(),
                2,
                "missing or invalid calibrated per-workload budgets",
            );
        }
        let mut control = ReceiptControl::new(None);
        control.budgets[FIRST] = json!(-1);
        expect_exit(
            &control.run(),
            2,
            "missing or invalid calibrated per-workload budgets",
        );
        control.budgets[FIRST] = json!(8);
        expect_exit(&control.run(), 2, "comparison used a different budget");
    }

    #[test]
    fn scheduled_receipt_fragment_requires_complete_raw_abba_observations() {
        let missing = ReceiptControl::new(Some(&format!("ab/{FIRST}/{FIRST}-raw.json")));
        expect_exit(&missing.run(), 2, "inconclusive");
        let control = ReceiptControl::new(None);
        control.write(&format!("ab/{FIRST}/{FIRST}-raw.json"), &json!([]));
        expect_exit(&control.run(), 2, "incomplete raw ABBA log");
        for (key, value, diagnostic) in [
            ("side", json!(1), "invalid raw ABBA order"),
            ("warmup", json!(false), "invalid raw ABBA order"),
            ("exit_code", json!(1), "invalid raw ABBA order"),
            ("stdout", Value::Null, "missing raw process output"),
            ("elapsed_ms", json!(0), "invalid raw elapsed time"),
        ] {
            let mut raw = abba_control(CONTROL_BLOCKS);
            raw[0][key] = value;
            control.write(&format!("ab/{FIRST}/{FIRST}-raw.json"), &raw);
            expect_exit(&control.run(), 2, diagnostic);
        }
        let mut raw = abba_control(CONTROL_BLOCKS);
        raw[8]["elapsed_ms"] = json!(20);
        control.write(&format!("ab/{FIRST}/{FIRST}-raw.json"), &raw);
        expect_exit(
            &control.run(),
            2,
            "raw observations do not match comparison samples",
        );
    }

    #[test]
    fn scheduled_benchmark_job_preserves_failure_and_evidence() {
        let workflow = workflow();
        let bench = &workflow["jobs"]["bench"];
        assert_eq!(
            bench["if"],
            "github.event_name == 'schedule' || github.event_name == 'workflow_dispatch'"
        );
        for job in ["bench-build", "bench", "bench-gate"] {
            for step in workflow["jobs"][job]["steps"].as_array().unwrap() {
                assert_ne!(step["continue-on-error"], true);
                assert!(
                    !step["uses"]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("actions/cache/")
                );
            }
        }
        assert_eq!(step(GATE)["if"], "always()");
        let upload = step("Upload benchmark results");
        assert_eq!(upload["if"], "always()");
        assert_eq!(upload["with"]["if-no-files-found"], "error");
        let build = step("Build pinned release artifacts before measurement");
        assert_eq!(build["id"], "benchmark_build");
        let script = build["run"].as_str().unwrap();
        assert!(script.contains("--release --locked --bin br"));
        assert!(script.contains("--test bench_synthetic_scale --no-run"));
        assert!(!script.contains("cargo test --release"));
        let measure = step(MEASURE);
        assert_eq!(
            measure["if"],
            "${{ !cancelled() && steps.benchmark_artifacts.outcome == 'success' }}"
        );
        let planted = step(PLANTED);
        assert_eq!(
            planted["if"],
            "matrix.issue_count == 1000 && matrix.flush == 'default-auto-flush'"
        );
        let script = planted["run"].as_str().unwrap();
        assert!(script.contains("timeout --signal=TERM --kill-after=10s 1800s"));
        assert!(!script.contains("cargo "));
        let script = measure["run"].as_str().unwrap();
        assert!(script.contains("for cohort in aa ab"));
        assert!(script.contains("--exact release_abba::release_cli_abba_1k_10k --ignored"));
        assert!(script.contains("timeout --signal=TERM --kill-after=10s 1800s"));
        assert!(script.contains("collector.exit"));
        assert!(script.contains("log-capture.exit"));
        assert!(!script.contains("cargo "));
    }

    #[test]
    fn scheduled_benchmark_matrix_builds_once_and_gates_every_shard() {
        let workflow = workflow();
        let jobs = &workflow["jobs"];
        let build = &jobs["bench-build"];
        let bench = &jobs["bench"];
        let gate = &jobs["bench-gate"];
        assert_eq!(build["needs"], "check");
        assert_eq!(
            build["if"],
            "github.event_name == 'schedule' || github.event_name == 'workflow_dispatch'"
        );
        assert_eq!(build["timeout-minutes"], 120);
        assert_eq!(
            build["outputs"]["pins"],
            "${{ steps.benchmark_artifacts.outputs.pins }}"
        );
        assert_eq!(
            step("Package benchmark artifacts")["id"],
            "benchmark_artifacts"
        );
        assert_eq!(bench["needs"], "bench-build");
        assert_eq!(bench["timeout-minutes"], 120);
        assert_eq!(bench["strategy"]["fail-fast"], false);
        assert_eq!(
            bench["strategy"]["matrix"],
            json!({"issue_count": [1000, 10000], "flush": ["default-auto-flush", "diagnostic-no-auto-flush"]})
        );
        assert_eq!(
            bench["env"]["BR_PERF_SHARD_ISSUE_COUNT"],
            "${{ matrix.issue_count }}"
        );
        assert_eq!(bench["env"]["BR_PERF_SHARD_FLUSH"], "${{ matrix.flush }}");
        assert_eq!(bench["env"]["BR_PERF_ABBA_BLOCKS"], "99");
        assert_eq!(bench["env"]["BR_PERF_QUIET_RUNNER"], "1");
        assert_eq!(
            bench["env"]["BR_PERF_ARTIFACT_PINS"],
            "${{ needs.bench-build.outputs.pins }}"
        );
        for step in bench["steps"].as_array().unwrap() {
            assert!(!step["run"].as_str().unwrap_or("").contains("cargo "));
        }
        let upload_bundle = step("Upload benchmark artifacts");
        let download_bundle = step("Download benchmark artifacts");
        assert_eq!(upload_bundle["with"]["name"], "benchmark-artifacts");
        assert_eq!(
            download_bundle["with"]["name"],
            upload_bundle["with"]["name"]
        );
        assert_eq!(upload_bundle["with"]["if-no-files-found"], "error");
        assert_eq!(
            step("Upload benchmark results")["with"]["name"],
            "benchmark-results-${{ matrix.issue_count }}-${{ matrix.flush }}"
        );
        assert_eq!(gate["needs"], json!(["bench-build", "bench"]));
        assert_eq!(
            gate["if"],
            "${{ always() && needs.bench-build.result == 'success' }}"
        );
        assert_eq!(gate["env"]["BR_PERF_ABBA_BLOCKS"], "99");
        assert_eq!(
            gate["env"]["BR_PERF_SHARDS_RESULT"],
            "${{ needs.bench.result }}"
        );
        for side in ["BASELINE", "CANDIDATE"] {
            for suffix in ["SOURCE", "SHA256", "LOCK_SHA256"] {
                let key = format!("BR_PERF_{side}_{suffix}");
                assert_eq!(
                    gate["env"][&key],
                    format!("${{{{ fromJSON(needs.bench-build.outputs.pins).{key} }}}}")
                );
            }
        }
        let download = step("Download every benchmark shard");
        assert_eq!(download["with"]["pattern"], "benchmark-results-*");
        assert_eq!(download["with"]["merge-multiple"], true);
        assert_eq!(download["with"]["path"], "target/perf-evidence/");
        let step_names = bench["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            step_names,
            [
                "Download benchmark artifacts",
                VERIFY_BUNDLE,
                PLANTED,
                MEASURE,
                "Upload benchmark results"
            ]
        );
    }

    #[test]
    fn scheduled_source_fragment_refuses_unpinned_and_unavailable_baselines() {
        let root = tempfile::TempDir::new().unwrap();
        for revision in [
            "",
            "main",
            "081100e",
            "0000000000000000000000000000000000000000",
        ] {
            let output = run_fragment(
                "Validate pinned benchmark source",
                root.path(),
                &[("BR_PERF_BASELINE_REVISION", revision.to_string())],
            );
            expect_exit(&output, 2, "full nonzero commit SHA");
        }
        let output = run_fragment(
            "Validate pinned benchmark source",
            root.path(),
            &[("BR_PERF_BASELINE_REVISION", "1".repeat(40))],
        );
        expect_exit(&output, 2, "pinned baseline source is unavailable");

        let init = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(init.status.success(), "{init:?}");
        let commit = Command::new("git")
            .args([
                "-c",
                "user.name=WorkflowControl",
                "-c",
                "user.email=control@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "Source pin transport control",
            ])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(commit.status.success(), "{commit:?}");
        let revision = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(revision.status.success(), "{revision:?}");
        let revision = String::from_utf8(revision.stdout).unwrap();
        let output = run_fragment(
            "Validate pinned benchmark source",
            root.path(),
            &[("BR_PERF_BASELINE_REVISION", revision.trim().to_string())],
        );
        expect_exit(&output, 0, "");
    }
}
