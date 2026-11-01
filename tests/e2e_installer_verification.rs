//! Offline regression coverage for installer verification and source selection.
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

// These source-build fixtures execute real Git operations against a local
// repository. Cargo is deliberately a recorder that emits a shell executable:
// this proves installer orchestration, not compilation of the application.
const SOURCE_CARGO_STUB: &[u8] = br#"#!/bin/sh
set -eu
printf '%s\n' "$@" > "$BR_INSTALLER_SOURCE_PROOF/cargo-args"
pwd > "$BR_INSTALLER_SOURCE_PROOF/cargo-cwd"
printf '%s\n' "$CARGO_TARGET_DIR" > "$BR_INSTALLER_SOURCE_PROOF/target-dir"
git rev-parse HEAD > "$BR_INSTALLER_SOURCE_PROOF/commit"
cp SOURCE_VERSION "$BR_INSTALLER_SOURCE_PROOF/version"
cp Cargo.lock "$BR_INSTALLER_SOURCE_PROOF/lockfile"
cp rust-toolchain.toml "$BR_INSTALLER_SOURCE_PROOF/toolchain"
mkdir -p "$CARGO_TARGET_DIR/release"
printf '#!/bin/sh\nprintf "br %%s\\n" "%s"\n' "$(cat SOURCE_VERSION)" \
    > "$CARGO_TARGET_DIR/release/br"
chmod +x "$CARGO_TARGET_DIR/release/br"
"#;

const SOURCE_GUM_STUB: &[u8] = br#"#!/bin/sh
set -eu
if [ "$1" = spin ]; then
    printf '%s\n' "$*" >> "$BR_INSTALLER_SOURCE_PROOF/gum-spins"
    while [ "$#" -gt 0 ] && [ "$1" != -- ]; do shift; done
    [ "$#" -gt 0 ] || exit 93
    shift
    exec "$@"
fi
"#;

struct SourceFixture {
    temp: TempDir,
    release_commit: String,
    default_commit: String,
}

fn fixture_git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=Installer Fixture",
            "-c",
            "user.email=installer@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .current_dir(directory)
        .output()
        .expect("run local fixture Git");
    assert!(
        output.status.success(),
        "fixture Git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Git output is UTF-8")
        .trim()
        .to_owned()
}

fn write_source_revision(directory: &Path, version: &str, toolchain: &str) {
    fs::write(directory.join("SOURCE_VERSION"), format!("{version}\n"))
        .expect("write source identity");
    fs::write(
        directory.join("Cargo.toml"),
        format!(
            "[package]\nname = \"installer-fixture\"\nversion = \"{version}\"\nedition = \"2024\"\n\
             [[bin]]\nname = \"br\"\npath = \"fixture.rs\"\n"
        ),
    )
    .expect("write source manifest");
    fs::write(
        directory.join("Cargo.lock"),
        format!(
            "version = 4\n\n[[package]]\nname = \"installer-fixture\"\nversion = \"{version}\"\n"
        ),
    )
    .expect("write source lockfile");
    fs::write(
        directory.join("rust-toolchain.toml"),
        format!("[toolchain]\nchannel = \"{toolchain}\"\n"),
    )
    .expect("write source toolchain");
    fs::write(
        directory.join("fixture.rs"),
        "fn main() { println!(\"installer fixture\"); }\n",
    )
    .expect("write fixture source");
    fixture_git(directory, &["add", "."]);
    fixture_git(directory, &["commit", "-q", "-m", version]);
}

fn write_fixture_program(path: &Path, contents: &[u8]) {
    fs::write(path, contents).expect("write fixture executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make fixture executable");
}

impl SourceFixture {
    fn new(allow_obsolete_dependency: bool) -> Self {
        let temp_root = std::env::temp_dir()
            .canonicalize()
            .expect("resolve source fixture temporary directory");
        let temp = tempfile::Builder::new()
            .prefix("br-source-fixture-")
            .tempdir_in(temp_root)
            .expect("create source installer fixture");
        let root = temp.path();
        for name in [
            "upstream",
            "home",
            "temporary space's directory",
            "tools",
            "proof",
        ] {
            fs::create_dir(root.join(name)).expect("create source fixture directory");
        }
        let upstream = root.join("upstream");
        fixture_git(&upstream, &["init", "-q", "--initial-branch=main"]);
        write_source_revision(&upstream, "1.2.3", "nightly-2026-08-31");
        let release_commit = fixture_git(&upstream, &["rev-parse", "HEAD"]);
        fixture_git(&upstream, &["tag", "-a", "v1.2.3", "-m", "release fixture"]);
        fixture_git(&upstream, &["checkout", "-q", "-b", "development"]);
        write_source_revision(&upstream, "9.9.9", "stable");
        let default_commit = fixture_git(&upstream, &["rev-parse", "HEAD"]);
        // A release selection must not resolve a same-named branch, nor accept
        // a branch when the requested tag is absent.
        fixture_git(&upstream, &["branch", "v1.2.3"]);
        fixture_git(&upstream, &["branch", "v3.0.0"]);

        let rewrite = format!("url.file://{}.insteadOf", upstream.display());
        fixture_git(
            root,
            &[
                "config",
                "--file",
                "gitconfig",
                "--add",
                &rewrite,
                "https://github.com/installer-fixture/beads_rust.git",
            ],
        );
        if allow_obsolete_dependency {
            // Let the old installer reach its wrong-source behavior rather
            // than failing early because its unnecessary sibling is missing.
            fixture_git(
                root,
                &[
                    "config",
                    "--file",
                    "gitconfig",
                    "--add",
                    &rewrite,
                    "https://github.com/installer-fixture/frankensqlite.git",
                ],
            );
        }
        fixture_git(
            root,
            &[
                "config",
                "--file",
                "gitconfig",
                "core.hooksPath",
                "/dev/null",
            ],
        );
        fs::write(
            root.join("home/.curlrc"),
            "proto = \"=file\"\nproto-redir = \"=file\"\n",
        )
        .expect("restrict fixture curl to local files");
        write_fixture_program(&root.join("tools/cargo"), SOURCE_CARGO_STUB);
        write_fixture_program(&root.join("tools/gum"), SOURCE_GUM_STUB);
        Self {
            temp,
            release_commit,
            default_commit,
        }
    }

    fn run(&self, args: &[&str], use_gum: bool, unusable_artifact: bool) -> Output {
        let root = self.temp.path();
        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(root.join("tools")).chain(std::env::split_paths(&inherited_path)),
        )
        .expect("construct fixture PATH");
        let mut command = Command::new("bash");
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
            .arg("--dest")
            .arg(root.join("installed bin"))
            .arg("--skip-skills")
            .args(args)
            .env("PATH", path)
            .env("HOME", root.join("home"))
            .env("CURL_HOME", root.join("home"))
            .env("TMPDIR", root.join("temporary space's directory"))
            .env("OWNER", "installer-fixture")
            .env("REPO", "beads_rust")
            .env("GIT_CONFIG_GLOBAL", root.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_COUNT", "0")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_TRACE", root.join("proof/git-trace"))
            .env("CARGO_NET_OFFLINE", "true")
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("RUSTUP_INIT_SKIP", "1")
            .env("BR_INSTALLER_SOURCE_PROOF", root.join("proof"))
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("BR_INSTALL_DIR")
            .env_remove("VERSION")
            .env_remove("BR_INSTALLER_SELF_REEXEC")
            .current_dir(root);
        if !use_gum {
            command.arg("--no-gum");
        }
        if unusable_artifact {
            let archive = root.join("unusable.tar.gz");
            let contents = b"checksum-valid fixture that is not a gzip archive";
            fs::write(&archive, contents).expect("write unusable local artifact");
            let mut checksum = String::with_capacity(64);
            for byte in Sha256::digest(contents) {
                write!(&mut checksum, "{byte:02x}").expect("format artifact checksum");
            }
            command
                .arg("--artifact-url")
                .arg(format!("file://{}", archive.display()))
                .arg("--checksum")
                .arg(checksum);
        }
        command.output().expect("run offline source installer")
    }

    fn recorded(&self, name: &str) -> String {
        fs::read_to_string(self.temp.path().join("proof").join(name))
            .expect("read source installer proof")
    }

    fn require_build(&self, output: &Output, version: &str, commit: &str, toolchain: &str) {
        assert!(
            output.status.success(),
            "source installer failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(self.recorded("version"), format!("{version}\n"));
        assert_eq!(self.recorded("commit").trim(), commit);
        assert_eq!(
            self.recorded("lockfile"),
            format!(
                "version = 4\n\n[[package]]\nname = \"installer-fixture\"\nversion = \"{version}\"\n"
            )
        );
        assert_eq!(
            self.recorded("toolchain"),
            format!("[toolchain]\nchannel = \"{toolchain}\"\n")
        );
        assert_eq!(
            self.recorded("cargo-args"),
            "build\n--locked\n--release\n--bin\nbr\n"
        );
        let cwd = self.recorded("cargo-cwd");
        let target = self.recorded("target-dir");
        assert!(
            Path::new(cwd.trim()).starts_with(self.temp.path().join("temporary space's directory")),
            "Cargo escaped the private source directory: {cwd}"
        );
        assert_eq!(
            Path::new(cwd.trim()).parent(),
            Path::new(target.trim()).parent()
        );
        let version_output = Command::new(self.temp.path().join("installed bin/br"))
            .arg("--version")
            .output()
            .expect("execute installed source fixture");
        assert!(version_output.status.success());
        assert_eq!(
            String::from_utf8(version_output.stdout).expect("fixture version is UTF-8"),
            format!("br {version}\n")
        );
    }
}

#[test]
fn installer_source_uses_the_requested_tag_over_a_same_named_branch() {
    for version in ["v1.2.3", "1.2.3"] {
        let fixture = SourceFixture::new(true);
        let output = fixture.run(&["--from-source", "--version", version], false, false);
        fixture.require_build(
            &output,
            "1.2.3",
            &fixture.release_commit,
            "nightly-2026-08-31",
        );
    }
}

#[test]
fn installer_source_refuses_unknown_tags_and_branch_only_versions_before_building() {
    for version in ["v9.8.7", "v3.0.0"] {
        let fixture = SourceFixture::new(true);
        let output = fixture.run(&["--from-source", "--version", version], false, false);
        assert!(
            !output.status.success(),
            "source install ignored missing release tag {version}:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.temp.path().join("proof/cargo-args").exists());
        assert!(!fixture.temp.path().join("installed bin/br").exists());
    }
}

#[test]
fn installer_source_without_a_version_uses_the_remote_default_branch_and_its_lock() {
    let fixture = SourceFixture::new(true);
    let output = fixture.run(&["--from-source"], false, false);
    fixture.require_build(&output, "9.9.9", &fixture.default_commit, "stable");
}

#[test]
fn installer_source_does_not_require_an_unrelated_frankensqlite_repository() {
    let fixture = SourceFixture::new(false);
    let output = fixture.run(&["--from-source", "--version", "v1.2.3"], false, false);
    fixture.require_build(
        &output,
        "1.2.3",
        &fixture.release_commit,
        "nightly-2026-08-31",
    );
    let trace = fixture.recorded("git-trace");
    assert!(!trace.contains("frankensqlite"), "{trace}");
}

#[test]
fn installer_unusable_release_artifact_falls_back_to_that_release_source() {
    let fixture = SourceFixture::new(true);
    let output = fixture.run(&["--version", "v1.2.3"], false, true);
    fixture.require_build(
        &output,
        "1.2.3",
        &fixture.release_commit,
        "nightly-2026-08-31",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Binary download failed, building from source"),
        "{stderr}"
    );
}

#[test]
fn installer_source_spinner_preserves_quoted_paths_and_build_arguments() {
    let fixture = SourceFixture::new(true);
    let output = fixture.run(&["--from-source", "--version", "v1.2.3"], true, false);
    fixture.require_build(
        &output,
        "1.2.3",
        &fixture.release_commit,
        "nightly-2026-08-31",
    );
    let spins = fixture.recorded("gum-spins");
    assert!(
        spins.contains("cargo build --locked --release --bin br"),
        "{spins}"
    );
}
