//! Offline regression coverage for the installer's opt-in runtime verification.
#![cfg(unix)]

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

const FIXTURE_BINARY: &[u8] = br#"#!/bin/sh
printf '%s\n' "$*" >> "$BR_INSTALLER_TEST_CALLS"
case "$*" in
    --version)
        printf '%s\n' 'br 0.0.0 (installer fixture)'
        ;;
    'doctor --selftest --json')
        printf '%s\n' '{"fixture":"selftest"}'
        exit "$BR_INSTALLER_TEST_SELFTEST_EXIT"
        ;;
    *)
        printf '%s\n' 'unexpected fixture invocation' >&2
        exit 92
        ;;
esac
"#;

fn run_installer_fixture(args: &[&str], binary: &[u8], selftest_exit: u8) -> (TempDir, Output) {
    let fixture = TempDir::new().expect("create installer fixture");
    let root = fixture.path();
    let payload = root.join("payload");
    let fixture_home = root.join("home");
    let fixture_tmp = root.join("tmp");
    for directory in [&payload, &fixture_home, &fixture_tmp] {
        fs::create_dir(directory).expect("create fixture directory");
    }
    let binary_path = payload.join("br");
    fs::write(&binary_path, binary).expect("write fixture binary");
    fs::set_permissions(&binary_path, fs::Permissions::from_mode(0o755))
        .expect("make fixture binary executable");
    let calls = root.join("calls");
    fs::write(&calls, "").expect("create invocation log");

    let archive = root.join("br-fixture.tar.gz");
    let tar = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&payload)
        .arg("br")
        .output()
        .expect("create fixture archive");
    assert!(
        tar.status.success(),
        "tar failed: {}",
        String::from_utf8_lossy(&tar.stderr)
    );
    let digest = Sha256::digest(fs::read(&archive).expect("read fixture archive"));
    let mut checksum = String::with_capacity(64);
    for byte in digest {
        write!(&mut checksum, "{byte:02x}").expect("format checksum");
    }

    let install_script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    let output = Command::new("bash")
        .arg(install_script)
        .args(["--version", "v0.0.0", "--dest"])
        .arg(root.join("install destination").join("bin"))
        .arg("--artifact-url")
        .arg(format!("file://{}", archive.display()))
        .arg("--checksum")
        .arg(checksum)
        .args(["--no-gum", "--skip-skills"])
        .args(args)
        .env("HOME", &fixture_home)
        .env("TMPDIR", &fixture_tmp)
        .env("BR_INSTALLER_TEST_CALLS", &calls)
        .env("BR_INSTALLER_TEST_SELFTEST_EXIT", selftest_exit.to_string())
        .env_remove("BR_INSTALL_DIR")
        .current_dir(root)
        .output()
        .expect("run installer against local archive");
    (fixture, output)
}

fn require_verification_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "installer falsely succeeded:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Installed binary failed its self-test"),
        "missing verification diagnostic: {stderr}"
    );
    assert!(!stderr.contains("Self-test complete"), "{stderr}");
    assert!(!stderr.contains("br installed successfully"), "{stderr}");
    assert!(!stdout.contains("br installed successfully"), "{stdout}");
}

#[test]
fn installer_verify_executes_the_installed_binary_selftest() {
    let (fixture, output) = run_installer_fixture(&["--verify"], FIXTURE_BINARY, 0);
    assert!(
        output.status.success(),
        "installer failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(fixture.path().join("calls")).expect("read invocation log");
    assert!(
        calls.lines().any(|call| call == "doctor --selftest --json"),
        "real self-test was not invoked: {calls}"
    );
    assert!(fixture.path().join("install destination/bin/br").is_file());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Self-test complete"), "{stderr}");
    assert!(stderr.contains("br installed successfully"), "{stderr}");
}

#[test]
fn installer_verify_rejects_a_failing_selftest_including_quiet_mode() {
    for args in [&["--verify"][..], &["--verify", "--quiet"][..]] {
        let (fixture, output) = run_installer_fixture(args, FIXTURE_BINARY, 47);
        require_verification_failure(&output);
        let calls = fs::read_to_string(fixture.path().join("calls")).expect("read invocation log");
        assert_eq!(calls, "doctor --selftest --json\n");
    }
}

#[test]
fn installer_verify_rejects_a_binary_that_cannot_execute() {
    let (_fixture, output) = run_installer_fixture(&["--verify"], b"\x7fELF\0invalid", 0);
    require_verification_failure(&output);
}

#[test]
fn installer_selftest_remains_opt_in() {
    let (fixture, output) = run_installer_fixture(&[], FIXTURE_BINARY, 47);
    assert!(
        output.status.success(),
        "ordinary installation failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(fixture.path().join("calls")).expect("read invocation log");
    assert!(!calls.contains("doctor --selftest"), "{calls}");
    assert!(calls.lines().any(|call| call == "--version"), "{calls}");
}
