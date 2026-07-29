//! Regression coverage for the notify-ACFS workflow's local contract.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_yml::Value as YamlValue;
use sha2::{Digest, Sha256};

const WORKFLOW_PATH: &str = ".github/workflows/notify-acfs.yml";
const INSTALLER_PATH: &str = "install.sh";
const HAS_TOKEN_OUTPUT: &str = "steps.check_token.outputs.has_token"; // ubs:ignore - GitHub Actions output name in a test assertion, not a secret value
const TRUE_COMPARISON: &str = " == ";
const TRUE_LITERAL: &str = "'true'";

#[derive(Debug, Deserialize)]
struct Workflow {
    jobs: BTreeMap<String, Job>,
}

#[derive(Debug, Deserialize)]
struct Job {
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
struct Step {
    name: Option<String>,
    run: Option<String>,
    uses: Option<String>,
    #[serde(rename = "if")]
    condition: Option<String>,
    with: Option<BTreeMap<String, YamlValue>>,
}

struct ShellOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

#[test]
fn notify_workflow_exposes_expected_steps_and_main_trigger() -> Result<(), String> {
    let raw = read_to_string(Path::new(WORKFLOW_PATH))?;
    require_contains(&raw, "branches: [main]")?;
    let legacy_branch_trigger = format!("branches: [{}]", ["mas", "ter"].concat());
    require_not_contains(&raw, &legacy_branch_trigger)?;

    for step_name in [
        "Compute SHA256",
        "Get previous checksum",
        "Compare checksums",
        "Notify ACFS (dry run)",
        "Check for ACFS_NOTIFY_TOKEN",
        "Notify ACFS",
        "Summary",
    ] {
        workflow_step(step_name)?;
    }

    Ok(())
}

#[test]
fn checksum_fragment_records_current_installer_hash() -> Result<(), String> {
    let script = rendered_step_script(
        "Compute SHA256",
        &[("${{ env.INSTALLER_PATH }}", INSTALLER_PATH)],
    )?;
    let fixture = NotifyFixture::new()?;
    fixture.write_installer(b"current installer\n")?;
    let output_path = fixture.root().join("github-output");
    let output_path_text = path_string(&output_path);

    let result = run_bash_step(
        &script,
        fixture.root(),
        &[("GITHUB_OUTPUT", output_path_text.as_str())],
    )?;
    require_success(&result)?;
    let github_output = read_to_string(&output_path)?;
    require_contains(
        &github_output,
        &format!("sha256={}", sha256_hex(b"current installer\n")),
    )
}

#[test]
fn previous_checksum_fragment_handles_previous_and_missing_versions() -> Result<(), String> {
    let script = rendered_step_script(
        "Get previous checksum",
        &[("${{ env.INSTALLER_PATH }}", INSTALLER_PATH)],
    )?;

    let changed = NotifyFixture::new()?;
    changed.init_repo()?;
    changed.commit_installer("old", b"old installer\n")?;
    changed.commit_installer("new", b"new installer\n")?;
    let changed_output = changed.root().join("changed-output");
    let changed_output_text = path_string(&changed_output);
    let result = run_bash_step(
        &script,
        changed.root(),
        &[("GITHUB_OUTPUT", changed_output_text.as_str())],
    )?;
    require_success(&result)?;
    let github_output = read_to_string(&changed_output)?;
    require_contains(
        &github_output,
        &format!("prev_sha256={}", sha256_hex(b"old installer\n")),
    )?;

    let initial = NotifyFixture::new()?;
    initial.init_repo()?;
    initial.commit_installer("initial", b"first installer\n")?;
    let initial_output = initial.root().join("initial-output");
    let initial_output_text = path_string(&initial_output);
    let result = run_bash_step(
        &script,
        initial.root(),
        &[("GITHUB_OUTPUT", initial_output_text.as_str())],
    )?;
    require_success(&result)?;
    require_contains(&read_to_string(&initial_output)?, "prev_sha256=none")
}

#[test]
fn compare_fragment_reports_changed_and_unchanged_states() -> Result<(), String> {
    let changed_script = rendered_step_script(
        "Compare checksums",
        &[
            ("${{ steps.checksum.outputs.sha256 }}", "new"),
            ("${{ steps.previous.outputs.prev_sha256 }}", "old"),
        ],
    )?;
    let unchanged_script = rendered_step_script(
        "Compare checksums",
        &[
            ("${{ steps.checksum.outputs.sha256 }}", "same"),
            ("${{ steps.previous.outputs.prev_sha256 }}", "same"),
        ],
    )?;
    let fixture = NotifyFixture::new()?;
    let changed_output = fixture.root().join("changed");
    let unchanged_output = fixture.root().join("unchanged");
    let changed_output_text = path_string(&changed_output);
    let unchanged_output_text = path_string(&unchanged_output);

    require_success(&run_bash_step(
        &changed_script,
        fixture.root(),
        &[("GITHUB_OUTPUT", changed_output_text.as_str())],
    )?)?;
    require_success(&run_bash_step(
        &unchanged_script,
        fixture.root(),
        &[("GITHUB_OUTPUT", unchanged_output_text.as_str())],
    )?)?;
    require_contains(&read_to_string(&changed_output)?, "changed=true")?;
    require_contains(&read_to_string(&unchanged_output)?, "changed=false")
}

#[test]
fn dry_run_and_dispatch_conditions_cover_changed_force_and_token_paths() -> Result<(), String> {
    let dry_run = workflow_step("Notify ACFS (dry run)")?;
    let token = workflow_step("Check for ACFS_NOTIFY_TOKEN")?;
    let dispatch = workflow_step("Notify ACFS")?;

    let change_or_force =
        "(steps.compare.outputs.changed == 'true' || github.event.inputs.force == 'true')";
    require_contains(step_condition(&dry_run)?, change_or_force)?;
    require_contains(
        step_condition(&dry_run)?,
        "github.event.inputs.dry_run == 'true'",
    )?;
    require_contains(step_condition(&token)?, change_or_force)?;
    require_contains(
        step_condition(&token)?,
        "github.event.inputs.dry_run != 'true'",
    )?;
    require_contains(step_condition(&dispatch)?, change_or_force)?;
    require_contains(
        step_condition(&dispatch)?,
        "github.event.inputs.dry_run != 'true'",
    )?;
    let has_token_true = [HAS_TOKEN_OUTPUT, TRUE_COMPARISON, TRUE_LITERAL].concat();
    require_contains(step_condition(&dispatch)?, &has_token_true)
}

#[test]
fn dry_run_fragment_reports_intended_notification() -> Result<(), String> {
    let script = rendered_step_script(
        "Notify ACFS (dry run)",
        &[
            ("${{ env.TOOL_NAME }}", "br"),
            ("${{ steps.checksum.outputs.sha256 }}", "abc123"),
        ],
    )?;
    let fixture = NotifyFixture::new()?;

    let result = run_bash_step(&script, fixture.root(), &[])?;
    require_success(&result)?;
    require_contains(&result.stdout, "DRY RUN - Would send notification to ACFS")?;
    require_contains(&result.stdout, "Tool: br")?;
    require_contains(&result.stdout, "SHA256: abc123")
}

#[test]
fn missing_token_fragment_is_notice_not_failure() -> Result<(), String> {
    let script = workflow_step_script("Check for ACFS_NOTIFY_TOKEN")?;
    let fixture = NotifyFixture::new()?;
    let output_path = fixture.root().join("github-output");
    let output_path_text = path_string(&output_path);

    let result = run_bash_step(
        &script,
        fixture.root(),
        &[
            ("GITHUB_OUTPUT", output_path_text.as_str()),
            ("ACFS_NOTIFY_TOKEN", ""),
        ],
    )?;
    require_success(&result)?;
    require_contains(
        &result.stdout,
        "ACFS_NOTIFY_TOKEN is not configured; skipping notification.",
    )?;
    require_contains(&read_to_string(&output_path)?, "has_token=false")
}

#[test]
fn dispatch_payload_shape_is_secret_free_and_complete() -> Result<(), String> {
    let dispatch = workflow_step("Notify ACFS")?;
    let uses = dispatch
        .uses
        .as_deref()
        .ok_or_else(|| "Notify ACFS step must use repository-dispatch".to_owned())?;
    require_contains(uses, "peter-evans/repository-dispatch@")?;

    let with = dispatch
        .with
        .as_ref()
        .ok_or_else(|| "Notify ACFS step must have with block".to_owned())?;
    let repository = yaml_string(with, "repository")?;
    let event_type = yaml_string(with, "event-type")?;
    let payload = yaml_string(with, "client-payload")?;
    require_contains(repository, "${{ env.ACFS_REPO }}")?;
    require_contains(event_type, "installer-updated")?;

    let parsed: JsonValue = serde_json::from_str(payload)
        .map_err(|error| format!("client-payload must stay valid JSON: {error}\n{payload}"))?;
    require_json_string(&parsed, "tool", "${{ env.TOOL_NAME }}")?;
    require_json_string(
        &parsed,
        "new_sha256",
        "${{ steps.checksum.outputs.sha256 }}",
    )?;
    require_json_string(
        &parsed,
        "old_sha256",
        "${{ steps.previous.outputs.prev_sha256 }}",
    )?;
    require_json_string(&parsed, "repo", "${{ github.repository }}")?;
    require_json_string(&parsed, "commit", "${{ github.sha }}")?;
    require_not_contains(payload, "ACFS_NOTIFY_TOKEN")
}

#[test]
fn summary_fragment_records_workflow_outcome() -> Result<(), String> {
    let script = rendered_step_script(
        "Summary",
        &[
            ("${{ env.TOOL_NAME }}", "br"),
            ("${{ steps.checksum.outputs.sha256 }}", "abc123"),
            ("${{ steps.compare.outputs.changed }}", "true"),
        ],
    )?;
    let fixture = NotifyFixture::new()?;
    let summary_path = fixture.root().join("summary.md");
    let summary_path_text = path_string(&summary_path);

    let result = run_bash_step(
        &script,
        fixture.root(),
        &[("GITHUB_STEP_SUMMARY", summary_path_text.as_str())],
    )?;
    require_success(&result)?;
    let summary = read_to_string(&summary_path)?;
    require_contains(&summary, "## ACFS Notification Summary")?;
    require_contains(&summary, "| Tool | br |")?;
    require_contains(&summary, "| SHA256 | `abc123` |")?;
    require_contains(&summary, "| Changed | true |")
}

fn workflow_step(step_name: &str) -> Result<Step, String> {
    let raw = read_to_string(Path::new(WORKFLOW_PATH))?;
    let workflow: Workflow = serde_yml::from_str(&raw)
        .map_err(|error| format!("failed to parse {WORKFLOW_PATH}: {error}"))?;

    workflow
        .jobs
        .into_values()
        .flat_map(|job| job.steps)
        .find(|step| step.name.as_deref() == Some(step_name))
        .ok_or_else(|| format!("workflow step not found: {step_name}"))
}

fn workflow_step_script(step_name: &str) -> Result<String, String> {
    let step = workflow_step(step_name)?;
    step.run
        .ok_or_else(|| format!("workflow step {step_name:?} has no run script"))
}

fn rendered_step_script(step_name: &str, replacements: &[(&str, &str)]) -> Result<String, String> {
    let mut script = workflow_step_script(step_name)?;
    for (needle, replacement) in replacements {
        script = script.replace(needle, replacement);
    }
    require_not_contains(&script, "${{")?;
    Ok(script)
}

fn step_condition(step: &Step) -> Result<&str, String> {
    step.condition
        .as_deref()
        .ok_or_else(|| format!("step {:?} has no if condition", step.name))
}

fn yaml_string<'a>(values: &'a BTreeMap<String, YamlValue>, key: &str) -> Result<&'a str, String> {
    values
        .get(key)
        .and_then(YamlValue::as_str)
        .ok_or_else(|| format!("with block has no string value for {key:?}: {values:#?}"))
}

fn require_json_string(json: &JsonValue, key: &str, expected: &str) -> Result<(), String> {
    let actual = json
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| format!("payload has no string {key:?}: {json}"))?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "payload {key:?} mismatch: expected {expected:?}, got {actual:?}"
        ))
    }
}

fn run_bash_step(
    script: &str,
    working_dir: &Path,
    envs: &[(&str, &str)],
) -> Result<ShellOutput, String> {
    let mut command = Command::new("bash");
    command
        .arg("-euo")
        .arg("pipefail")
        .arg("-c")
        .arg(script)
        .current_dir(working_dir);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command
        .output()
        .map_err(|error| format!("failed to run bash fragment: {error}"))?;

    Ok(ShellOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn require_success(output: &ShellOutput) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "fragment failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            output.stdout,
            output.stderr
        ))
    }
}

fn require_contains(haystack: &str, needle: &str) -> Result<(), String> {
    if haystack.contains(needle) {
        Ok(())
    } else {
        Err(format!("expected to find {needle:?} in:\n{haystack}"))
    }
}

fn require_not_contains(haystack: &str, needle: &str) -> Result<(), String> {
    if haystack.contains(needle) {
        Err(format!("did not expect to find {needle:?} in:\n{haystack}"))
    } else {
        Ok(())
    }
}

fn read_to_string(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("failed to read {}: {error}", path.display()))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}

struct NotifyFixture {
    temp_dir: tempfile::TempDir,
}

impl NotifyFixture {
    fn new() -> Result<Self, String> {
        Ok(Self {
            temp_dir: tempfile::TempDir::new()
                .map_err(|error| format!("failed to create temp fixture: {error}"))?,
        })
    }

    fn root(&self) -> &Path {
        self.temp_dir.path()
    }

    fn installer_path(&self) -> PathBuf {
        self.root().join(INSTALLER_PATH)
    }

    fn write_installer(&self, bytes: &[u8]) -> Result<(), String> {
        fs::write(self.installer_path(), bytes)
            .map_err(|error| format!("failed to write installer fixture: {error}"))
    }

    fn init_repo(&self) -> Result<(), String> {
        run_git(self.root(), &["init", "-q"])?;
        run_git(
            self.root(),
            &["config", "user.email", "test@example.invalid"],
        )?;
        run_git(
            self.root(),
            &["config", "user.name", "Workflow Notify Test"],
        )
    }

    fn commit_installer(&self, message: &str, bytes: &[u8]) -> Result<(), String> {
        self.write_installer(bytes)?;
        run_git(self.root(), &["add", INSTALLER_PATH])?;
        run_git(self.root(), &["commit", "-q", "-m", message])
    }
}

fn run_git(working_dir: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(working_dir)
        .output()
        .map_err(|error| format!("failed to run git {args:?}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}
