//! Regression coverage for high-risk release workflow shell fragments.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use serde::Deserialize;
use sha2::{Digest, Sha256};

const RELEASE_WORKFLOW: &str = ".github/workflows/release.yml";
const README: &str = "README.md";
const CURRENT_MINISIGN_PUBLIC_KEY: &str =
    "RWS7nGFfBYC+MWeZLEaowkjNi77w5FEOk49fEhX2jZ6gpd9uQ4vzVIrF";
const RETIRED_MINISIGN_PUBLIC_KEY: &str =
    "RWSp4vEOdKsY8e95W9/4eLrSJ2B2GHv4U+CKMBXqRX3JhPrPn8J0DWBG";
const REQUIRED_PLATFORMS: &[&str] = &[
    "linux_amd64",
    "linux_musl_amd64",
    "linux_arm64",
    "linux_musl_arm64",
    "darwin_amd64",
    "darwin_arm64",
    "windows_amd64",
];

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
    #[serde(rename = "with")]
    action_inputs: Option<ActionInputs>,
}

#[derive(Debug, Deserialize)]
struct ActionInputs {
    #[serde(rename = "ref")]
    checkout_ref: Option<String>,
}

struct ShellOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

#[test]
fn release_workflow_exposes_expected_fragment_steps() -> Result<(), String> {
    for step_name in [
        "Validate reliability override",
        "Validate required artifacts present",
        "Generate combined checksums",
        "Verify all checksums",
        "Create archive (tar.gz)",
        "Create archive (zip)",
        "Sign release archive with Ed25519",
        "Generate changelog",
    ] {
        release_step_script(step_name)?;
    }

    Ok(())
}

#[test]
fn release_workflow_uses_tagless_asset_file_names() -> Result<(), String> {
    let workflow = read_to_string(Path::new(RELEASE_WORKFLOW))?;

    // The tag may arrive from a tag push (GITHUB_REF_NAME) or the
    // workflow_dispatch `tag` input; either way the asset version strips
    // the leading `v` before any file name is built.
    require_contains(&workflow, r#"TAG="${INPUT_TAG:-$GITHUB_REF_NAME}""#)?;
    require_contains(&workflow, r#"ASSET_VERSION="${TAG#v}""#)?;
    require_contains(
        &workflow,
        "br-${{ steps.asset_version.outputs.asset_version }}-${{ matrix.name }}",
    )?;
    require_contains(
        &workflow,
        "artifacts/br-${ASSET_VERSION}-linux_amd64.tar.gz",
    )?;
    require_contains(
        &workflow,
        "artifacts/br-${ASSET_VERSION}-windows_amd64.zip",
    )?;
    require_not_contains(&workflow, "artifacts/br-${ASSET_VERSION}-${platform}.*")?;
    require_not_contains(&workflow, "br-${{ github.ref_name }}-${{ matrix.name }}")?;
    require_not_contains(&workflow, "artifacts/br-${{ github.ref_name }}-*")?;

    Ok(())
}

#[test]
fn release_workflow_checkout_refs_are_unambiguous() -> Result<(), String> {
    let workflow = parse_release_workflow()?;
    let mut checkout_steps = 0;

    for step in workflow.jobs.values().flat_map(|job| &job.steps) {
        let Some(action) = step.uses.as_deref() else {
            continue;
        };
        if !action.starts_with("actions/checkout@") {
            continue;
        }

        checkout_steps += 1;
        let checkout_ref = step
            .action_inputs
            .as_ref()
            .and_then(|inputs| inputs.checkout_ref.as_deref());
        if checkout_ref != Some("${{ github.event.inputs.tag || github.ref }}") {
            return Err(format!(
                "checkout step must have exactly one release-tag ref, found {checkout_ref:?}"
            ));
        }
    }

    if checkout_steps == 5 {
        Ok(())
    } else {
        Err(format!(
            "expected five release checkout steps, found {checkout_steps}"
        ))
    }
}

#[test]
fn release_signatures_use_the_documented_current_trust_anchor() -> Result<(), String> {
    let workflow = read_to_string(Path::new(RELEASE_WORKFLOW))?;
    let readme = read_to_string(Path::new(README))?;
    let changelog_script = release_step_script("Generate changelog")?;
    let signing_script = release_step_script("Sign release archive with Ed25519")?;

    require_contains(&readme, CURRENT_MINISIGN_PUBLIC_KEY)?;
    let public_key_env = format!("MINISIGN_PUBLIC_KEY: '{CURRENT_MINISIGN_PUBLIC_KEY}'");
    require_contains(&workflow, &public_key_env)?;
    require_not_contains(&workflow, RETIRED_MINISIGN_PUBLIC_KEY)?;
    require_contains(
        &changelog_script,
        "# Public key: ${{ env.MINISIGN_PUBLIC_KEY }}",
    )?;
    require_contains(
        &changelog_script,
        "-P '${{ env.MINISIGN_PUBLIC_KEY }}'",
    )?;
    require_contains(
        &signing_script,
        "minisign -Vm \"$archive\" -x \"$signature\" -P \"$MINISIGN_PUBLIC_KEY\"",
    )
}

#[test]
fn release_archives_include_the_repository_license() -> Result<(), String> {
    let tar_script = release_step_script("Create archive (tar.gz)")?;
    let zip_script = release_step_script("Create archive (zip)")?;

    for script in [&tar_script, &zip_script] {
        require_contains(script, "cp ../../../LICENSE LICENSE")?;
    }
    require_contains(&tar_script, "tar -czvf")?;
    require_contains(&tar_script, "br LICENSE")?;
    require_contains(&zip_script, "zip -j")?;
    require_contains(&zip_script, "br.exe LICENSE")
}

#[test]
fn reliability_override_fragment_requires_reason_and_records_summary() -> Result<(), String> {
    let script = release_step_script("Validate reliability override")?;
    let fixture = WorkflowFixture::new()?;
    let summary_path = fixture.root().join("summary.md");
    let summary_path_text = path_string(&summary_path);

    let missing_reason = run_bash_step(
        &script,
        fixture.root(),
        &[
            ("GITHUB_STEP_SUMMARY", summary_path_text.as_str()),
            ("RELIABILITY_OVERRIDE_REASON", ""),
        ],
    )?;
    require_failure(&missing_reason, "empty override reason should fail")?;
    require_contains(
        &missing_reason.stdout,
        "reliability_override_reason is required",
    )?;

    let accepted = run_bash_step(
        &script,
        fixture.root(),
        &[
            ("GITHUB_STEP_SUMMARY", summary_path_text.as_str()),
            (
                "RELIABILITY_OVERRIDE_REASON",
                "documented operator emergency",
            ),
        ],
    )?;
    require_success(&accepted)?;
    let summary = read_to_string(&summary_path)?;
    require_contains(&summary, "Reliability gates were explicitly skipped")?;
    require_contains(&summary, "documented operator emergency")
}

#[test]
fn required_artifact_fragment_reports_missing_platforms() -> Result<(), String> {
    // The step reads its version from the `asset_version` step output — a
    // GitHub expression bash cannot evaluate — so substitute the fixture's
    // known version before running the fragment.
    let script = release_step_script("Validate required artifacts present")?
        .replace("${{ steps.asset_version.outputs.asset_version }}", "9.9.9");
    let fixture = WorkflowFixture::new()?;
    fixture.create_artifacts_dir()?;
    for platform in REQUIRED_PLATFORMS {
        fixture.write_release_artifact_set(platform, b"binary")?;
    }

    let complete = run_bash_step(&script, fixture.root(), &[])?;
    require_success(&complete)?;
    require_contains(
        &complete.stdout,
        "All required release archives, checksums, and signatures are present",
    )?;

    let missing = WorkflowFixture::new()?;
    missing.create_artifacts_dir()?;
    for platform in REQUIRED_PLATFORMS
        .iter()
        .copied()
        .filter(|platform| *platform != "windows_amd64")
    {
        missing.write_release_artifact_set(platform, b"binary")?;
    }

    let result = run_bash_step(&script, missing.root(), &[])?;
    require_failure(&result, "missing platform should fail")?;
    require_contains(&result.stdout, "br-9.9.9-windows_amd64.zip")?;

    let missing_signature = WorkflowFixture::new()?;
    missing_signature.create_artifacts_dir()?;
    for platform in REQUIRED_PLATFORMS {
        if *platform == "windows_amd64" {
            missing_signature.write_release_archive_and_checksum(platform, b"binary")?;
        } else {
            missing_signature.write_release_artifact_set(platform, b"binary")?;
        }
    }

    let result = run_bash_step(&script, missing_signature.root(), &[])?;
    require_failure(&result, "missing signature sidecar should fail")?;
    require_contains(
        &result.stdout,
        "br-9.9.9-windows_amd64.zip.minisig",
    )
}

#[test]
fn combined_checksums_fragment_is_null_safe_and_replaces_existing_file() -> Result<(), String> {
    let script = release_step_script("Generate combined checksums")?;
    let fixture = WorkflowFixture::new()?;
    fixture.create_artifacts_dir()?;
    fixture.write_artifact("br-9.9.9-linux_amd64.tar.gz.sha256", b"linux\n")?;
    fixture.write_artifact("br-9.9.9-darwin amd64.tar.gz.sha256", b"darwin\n")?;
    fixture.write_artifact("--leading-name.sha256", b"leading\n")?;
    fixture.write_artifact("checksums.sha256", b"stale\n")?;

    let result = run_bash_step(&script, fixture.root(), &[])?;
    require_success(&result)?;
    let combined = fixture.read_artifact("checksums.sha256")?;
    require_contains(&combined, "linux")?;
    require_contains(&combined, "darwin")?;
    require_contains(&combined, "leading")?;
    require_not_contains(&combined, "stale")
}

#[test]
fn verify_checksums_fragment_accepts_spaces_and_leading_dashes() -> Result<(), String> {
    let script = release_step_script("Verify all checksums")?;
    if script.matches("=== Verifying all checksums ===").count() != 1 {
        return Err("checksum verification banner must appear exactly once".to_owned());
    }
    let fixture = WorkflowFixture::new()?;
    fixture.create_artifacts_dir()?;
    fixture.write_artifact_with_checksum("artifact with spaces.tar.gz", b"space-safe")?;
    fixture.write_artifact_with_checksum("--leading-artifact.tar.gz", b"dash-safe")?;
    fixture.write_artifact("checksums.sha256", b"combined file should be skipped\n")?;

    let result = run_bash_step(&script, fixture.root(), &[])?;
    require_success(&result)?;
    require_contains(&result.stdout, "artifact with spaces.tar.gz: OK")?;
    require_contains(&result.stdout, "--leading-artifact.tar.gz: OK")
}

#[test]
fn verify_checksums_fragment_fails_on_corrupt_checksum() -> Result<(), String> {
    let script = release_step_script("Verify all checksums")?;
    let fixture = WorkflowFixture::new()?;
    fixture.create_artifacts_dir()?;
    fixture.write_artifact("br-9.9.9-linux_amd64.tar.gz", b"actual bytes")?;
    fixture.write_artifact(
        "br-9.9.9-linux_amd64.tar.gz.sha256",
        b"0000000000000000000000000000000000000000000000000000000000000000  br-9.9.9-linux_amd64.tar.gz\n",
    )?;

    let result = run_bash_step(&script, fixture.root(), &[])?;
    require_failure(&result, "corrupt checksum should fail release verification")
}

#[test]
fn signing_fragment_uses_private_ephemeral_key_file() -> Result<(), String> {
    let step_name = "Sign release archive with Ed25519";
    let script = release_step_script(step_name)?;
    let condition = release_step_condition(step_name)?;

    if let Some(condition) = condition {
        return Err(format!(
            "release signing must not be conditionally skipped, found: {condition}"
        ));
    }

    require_contains(&script, "MINISIGN_SECRET_KEY is required")?;
    require_contains(&script, "mktemp")?;
    require_contains(&script, "RUNNER_TEMP")?;
    require_contains(&script, "chmod 600 \"$signing_key\"")?;
    require_contains(&script, "trap 'rm -f \"$signing_key\"' EXIT")?;
    require_contains(&script, "printf '%s\\n' \"$MINISIGN_SECRET_KEY\"")?;
    require_contains(&script, "-s \"$signing_key\"")?;
    require_contains(&script, "if [ ! -s \"$signature\" ]")?;
    require_not_contains(&script, "/tmp/minisign.key")?;
    require_not_contains(&script, "echo \"$MINISIGN_SECRET_KEY\"")?;

    let fixture = WorkflowFixture::new()?;
    let missing_secret = run_bash_step(
        &script,
        fixture.root(),
        &[("MINISIGN_SECRET_KEY", "")],
    )?;
    require_failure(&missing_secret, "missing signing secret should fail closed")?;
    require_contains(
        &missing_secret.stdout,
        "MINISIGN_SECRET_KEY is required for every release archive",
    )
}

#[test]
fn changelog_fragment_keeps_previous_tag_and_reliability_paths() -> Result<(), String> {
    let script = release_step_script("Generate changelog")?;

    require_contains(&script, "git describe --tags --abbrev=0 HEAD^")?;
    require_contains(&script, "No previous tag found")?;
    require_contains(&script, "HEAD~20..HEAD")?;
    require_contains(&script, "Reliability gates were explicitly skipped")?;
    require_contains(
        &script,
        "Release reliability gates completed before artifacts were built",
    )
}

fn release_step_script(step_name: &str) -> Result<String, String> {
    let workflow = parse_release_workflow()?;

    let Some(step) = workflow
        .jobs
        .values()
        .flat_map(|job| &job.steps)
        .find(|step| step.name.as_deref() == Some(step_name))
    else {
        return Err(format!("release workflow step not found: {step_name}"));
    };

    let Some(run) = step.run.as_deref() else {
        return Err(format!("step {step_name:?} has no run script"));
    };

    Ok(run.to_owned())
}

fn release_step_condition(step_name: &str) -> Result<Option<String>, String> {
    let workflow = parse_release_workflow()?;

    let Some(step) = workflow
        .jobs
        .values()
        .flat_map(|job| &job.steps)
        .find(|step| step.name.as_deref() == Some(step_name))
    else {
        return Err(format!("release workflow step not found: {step_name}"));
    };

    Ok(step.condition.clone())
}

fn parse_release_workflow() -> Result<Workflow, String> {
    let raw = read_to_string(Path::new(RELEASE_WORKFLOW))?;
    serde_yml::from_str(&raw)
        .map_err(|error| format!("failed to parse {RELEASE_WORKFLOW}: {error}"))
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

fn require_failure(output: &ShellOutput, context: &str) -> Result<(), String> {
    if output.status.success() {
        Err(format!(
            "{context}; fragment unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
            output.stdout, output.stderr
        ))
    } else {
        Ok(())
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

struct WorkflowFixture {
    temp_dir: tempfile::TempDir,
}

impl WorkflowFixture {
    fn new() -> Result<Self, String> {
        Ok(Self {
            temp_dir: tempfile::TempDir::new()
                .map_err(|error| format!("failed to create temp fixture: {error}"))?,
        })
    }

    fn root(&self) -> &Path {
        self.temp_dir.path()
    }

    fn artifacts_dir(&self) -> PathBuf {
        self.root().join("artifacts")
    }

    fn create_artifacts_dir(&self) -> Result<(), String> {
        fs::create_dir_all(self.artifacts_dir())
            .map_err(|error| format!("failed to create artifacts fixture: {error}"))
    }

    fn write_artifact(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let path = self.artifacts_dir().join(name);
        fs::write(&path, bytes)
            .map_err(|error| format!("failed to write {}: {error}", path.display()))
    }

    fn write_release_archive_and_checksum(
        &self,
        platform: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        let name = release_archive_name(platform);
        self.write_artifact(&name, bytes)?;
        self.write_artifact(&format!("{name}.sha256"), b"checksum")
    }

    fn write_release_artifact_set(&self, platform: &str, bytes: &[u8]) -> Result<(), String> {
        let name = release_archive_name(platform);
        self.write_release_archive_and_checksum(platform, bytes)?;
        self.write_artifact(&format!("{name}.minisig"), b"signature")
    }

    fn read_artifact(&self, name: &str) -> Result<String, String> {
        read_to_string(&self.artifacts_dir().join(name))
    }

    fn write_artifact_with_checksum(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        self.write_artifact(name, bytes)?;
        let digest = sha256_hex(bytes);
        self.write_artifact(
            &format!("{name}.sha256"),
            format!("{digest}  {name}\n").as_bytes(),
        )
    }
}

fn release_archive_name(platform: &str) -> String {
    let extension = if platform == "windows_amd64" {
        "zip"
    } else {
        "tar.gz"
    };
    format!("br-9.9.9-{platform}.{extension}")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}
