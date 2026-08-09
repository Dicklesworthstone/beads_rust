//! Shell-boundary tests for repository-owned worktree redirect adapters.

use serde_json::Value as JsonValue;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;
use toml::Value as TomlValue;

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn br_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_br"))
}

fn activation_utility() -> PathBuf {
    repository_root().join("scripts/activate-worktree-redirect-hook.sh")
}

fn isolated_home(repository: &Path) -> PathBuf {
    let home = repository.join("test-home");
    fs::create_dir_all(&home).unwrap();
    home
}

fn run_git<I, S>(repository: &Path, args: I, path: Option<&OsStr>) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("/usr/bin/git");
    command
        .args(args)
        .current_dir(repository)
        .env("HOME", isolated_home(repository))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.output().expect("run git")
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn setup_git_repository() -> (TempDir, String) {
    let fixture = TempDir::new().unwrap();
    let repository = fixture.path();
    assert_success(
        &run_git(repository, ["init", "-b", "main"], None),
        "git init",
    );
    assert_success(
        &run_git(repository, ["config", "user.name", "Redirect Test"], None),
        "configure user name",
    );
    assert_success(
        &run_git(
            repository,
            ["config", "user.email", "redirect@example.invalid"],
            None,
        ),
        "configure user email",
    );
    let initialized = Command::new(br_binary())
        .args(["init", "--prefix", "shared"])
        .current_dir(repository)
        .env("HOME", isolated_home(repository))
        .env("BEADS_DIR", repository.join(".beads"))
        .output()
        .expect("initialize canonical tracker");
    assert_success(&initialized, "initialize canonical tracker");
    fs::write(
        repository.join(".beads/interactions.jsonl"),
        "{\"kind\":\"tracked-history\"}\n",
    )
    .unwrap();
    fs::write(
        repository.join(".beads/README.md"),
        "# Tracked tracker documentation\n",
    )
    .unwrap();
    fs::write(repository.join("tracked.txt"), "historical\n").unwrap();
    assert_success(
        &run_git(repository, ["add", "tracked.txt", ".beads"], None),
        "stage historical fixture",
    );
    assert_success(
        &run_git(repository, ["commit", "-m", "historical"], None),
        "commit historical fixture",
    );
    let revision = run_git(repository, ["rev-parse", "HEAD"], None);
    assert_success(&revision, "resolve historical revision");
    let historical = String::from_utf8(revision.stdout)
        .unwrap()
        .trim()
        .to_string();

    fs::write(repository.join("tracked.txt"), "current\n").unwrap();
    assert_success(
        &run_git(repository, ["add", "tracked.txt"], None),
        "stage current fixture",
    );
    assert_success(
        &run_git(repository, ["commit", "-m", "current"], None),
        "commit current fixture",
    );
    (fixture, historical)
}

fn run_activation(repository: &Path) -> Output {
    Command::new(activation_utility())
        .current_dir(repository)
        .env("HOME", isolated_home(repository))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run worktree redirect hook activation")
}

fn br_path() -> std::ffi::OsString {
    let binary = br_binary();
    let directory = binary.parent().expect("br binary directory");
    let mut path = directory.as_os_str().to_os_string();
    path.push(":/usr/bin:/bin");
    path
}

fn assert_redirect_target(worktree: &Path, canonical_beads: &Path) {
    let redirect =
        fs::read_to_string(worktree.join(".beads/redirect")).expect("worktree redirect must exist");
    assert_eq!(
        PathBuf::from(redirect.trim()),
        fs::canonicalize(canonical_beads).unwrap()
    );
}

fn run_real_adapter(command: &str, worktree: &Path, home: &Path) -> Output {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(worktree)
        .env("PATH", br_path())
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run repository lifecycle adapter")
}

fn worktrunk_command() -> String {
    let contents = fs::read_to_string(repository_root().join(".config/wt.toml"))
        .expect("read repository Worktrunk config");
    let parsed: TomlValue = toml::from_str(&contents).expect("parse Worktrunk TOML");
    parsed["pre-start"]
        .as_str()
        .expect("pre-start must be one direct command")
        .to_string()
}

fn claude_command() -> String {
    let contents = fs::read_to_string(repository_root().join(".claude/settings.json"))
        .expect("read repository Claude settings");
    let parsed: JsonValue = serde_json::from_str(&contents).expect("parse Claude settings JSON");
    parsed["hooks"]["PostToolUse"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["matcher"] == "EnterWorktree")
        })
        .and_then(|entry| entry["hooks"].as_array())
        .and_then(|hooks| hooks.first())
        .and_then(|hook| hook["command"].as_str())
        .expect("Claude EnterWorktree command hook")
        .to_string()
}

#[cfg(unix)]
fn run_adapter(command: &str, cwd: &Path, fake_status: i32) -> (Output, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let fake_bin = cwd.join("fake-bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let invocation_log = cwd.join("br-invocation");
    let fake_br = fake_bin.join("br");
    fs::write(
        &fake_br,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$PWD" > "${{FAKE_BR_LOG}}.cwd"
printf '%s\n' "$@" > "${{FAKE_BR_LOG}}.args"
exit "${{FAKE_BR_STATUS:-0}}"
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_br, fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .env("FAKE_BR_LOG", &invocation_log)
        .env("FAKE_BR_STATUS", fake_status.to_string())
        .output()
        .expect("run lifecycle adapter");
    (output, invocation_log)
}

#[cfg(unix)]
fn assert_adapter_contract(command: &str) {
    assert!(command.contains("br init --redirect"));
    assert!(!command.contains("setup_br_worktree"));
    assert!(!command.contains("setup-br-worktree"));
    for unrelated in [".env", "cargo ", "git ", "make "] {
        assert!(
            !command.contains(unrelated),
            "adapter must not perform unrelated setup: {command}"
        );
    }

    let fixture = TempDir::new().unwrap();
    let worktree = fixture.path().join("created-worktree");
    fs::create_dir(&worktree).unwrap();
    let (success, success_log) = run_adapter(command, &worktree, 0);
    assert!(success.status.success());
    assert!(
        success.stdout.is_empty(),
        "successful lifecycle setup should be quiet"
    );
    assert!(
        success.stderr.is_empty(),
        "successful lifecycle setup should be quiet"
    );
    assert_eq!(
        fs::read_to_string(success_log.with_extension("cwd"))
            .unwrap()
            .trim(),
        worktree.to_string_lossy()
    );
    assert_eq!(
        fs::read_to_string(success_log.with_extension("args")).unwrap(),
        "init\n--redirect\n"
    );

    let (failure, _) = run_adapter(command, &worktree, 7);
    assert!(
        failure.status.success(),
        "lifecycle adapter must fail open after worktree creation"
    );
    let warning = String::from_utf8_lossy(&failure.stderr);
    assert!(warning.to_ascii_lowercase().contains("warning"));
    assert!(warning.contains("br init --redirect"));
    assert!(warning.contains("exact .beads path"));
}

#[cfg(unix)]
#[test]
fn worktrunk_pre_start_invokes_native_redirect_setup_and_fails_open() {
    assert_adapter_contract(&worktrunk_command());
}

#[cfg(unix)]
#[test]
fn claude_enter_worktree_invokes_native_redirect_setup_and_fails_open() {
    assert_adapter_contract(&claude_command());
}

#[cfg(unix)]
#[test]
fn native_git_activation_provisions_historical_worktrees_and_is_idempotent() {
    let (fixture, historical) = setup_git_repository();
    let repository = fixture.path();
    let first = run_activation(repository);
    assert_success(&first, "activate post-checkout dispatcher");
    let second = run_activation(repository);
    assert_success(&second, "repeat post-checkout activation");
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("already active"),
        "repeated activation should report an idempotent success"
    );

    let configured_path = run_git(repository, ["config", "--get", "core.hooksPath"], None);
    assert_eq!(configured_path.status.code(), Some(1));

    let primary_hook = repository.join(".git/hooks/post-checkout");
    let primary_noop = Command::new(&primary_hook)
        .args([&historical, &historical, "1"])
        .current_dir(repository)
        .env("PATH", br_path())
        .output()
        .expect("run dispatcher in primary worktree");
    assert_success(&primary_noop, "primary-owner dispatcher no-op");
    assert!(!repository.join(".beads/redirect").exists());

    let linked = repository.join("historical-worktree");
    let created = run_git(
        repository,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            linked.as_os_str(),
            OsStr::new(&historical),
        ],
        Some(&br_path()),
    );
    assert_success(&created, "create historical linked worktree");
    assert_redirect_target(&linked, &repository.join(".beads"));
}

#[cfg(unix)]
#[test]
fn native_git_activation_preserves_hook_collisions_and_hooks_path_configuration() {
    let (collision_fixture, _) = setup_git_repository();
    let collision_repository = collision_fixture.path();
    let hook = collision_repository.join(".git/hooks/post-checkout");
    let custom_hook = b"#!/bin/sh\nprintf 'custom hook\\n'\n";
    fs::write(&hook, custom_hook).unwrap();
    let collision = run_activation(collision_repository);
    assert!(!collision.status.success());
    assert_eq!(fs::read(&hook).unwrap(), custom_hook);
    let collision_warning = String::from_utf8_lossy(&collision.stderr);
    assert!(collision_warning.contains("preserving existing post-checkout"));
    assert!(collision_warning.contains("chain"));

    let (configured_fixture, _) = setup_git_repository();
    let configured_repository = configured_fixture.path();
    assert_success(
        &run_git(
            configured_repository,
            ["config", "core.hooksPath", "custom-hooks"],
            None,
        ),
        "configure alternate hooks path",
    );
    let configured = run_activation(configured_repository);
    assert!(!configured.status.success());
    assert!(String::from_utf8_lossy(&configured.stderr).contains("core.hooksPath"));
    assert_eq!(
        String::from_utf8_lossy(
            &run_git(
                configured_repository,
                ["config", "--get", "core.hooksPath"],
                None,
            )
            .stdout
        )
        .trim(),
        "custom-hooks"
    );
    assert!(
        !configured_repository
            .join(".git/hooks/post-checkout")
            .exists()
    );

    let (symlink_fixture, _) = setup_git_repository();
    let symlink_repository = symlink_fixture.path();
    let external_hooks = symlink_repository.join("external-hooks");
    fs::create_dir(&external_hooks).unwrap();
    let hooks_dir = symlink_repository.join(".git/hooks");
    fs::rename(&hooks_dir, symlink_repository.join(".git/hooks-original")).unwrap();
    std::os::unix::fs::symlink(&external_hooks, &hooks_dir).unwrap();

    let symlinked = run_activation(symlink_repository);
    assert!(!symlinked.status.success());
    assert!(String::from_utf8_lossy(&symlinked.stderr).contains("hooks directory is a symlink"));
    assert!(!external_hooks.join("post-checkout").exists());
}

#[cfg(unix)]
#[test]
fn native_git_dispatcher_warns_open_and_no_checkout_supports_manual_recovery() {
    let (fixture, historical) = setup_git_repository();
    let repository = fixture.path();
    assert_success(
        &run_activation(repository),
        "activate post-checkout dispatcher",
    );

    let missing_br = repository.join("missing-br-worktree");
    let warned = run_git(
        repository,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            missing_br.as_os_str(),
            OsStr::new(&historical),
        ],
        Some(OsStr::new("/usr/bin:/bin")),
    );
    assert_success(&warned, "worktree creation must survive redirect failure");
    let warning = String::from_utf8_lossy(&warned.stderr);
    assert!(warning.contains("WARNING"));
    assert!(warning.contains("br init --redirect"));
    assert!(warning.contains("exact .beads path"));
    assert!(!missing_br.join(".beads/redirect").exists());

    let current = String::from_utf8(run_git(repository, ["rev-parse", "HEAD"], None).stdout)
        .unwrap()
        .trim()
        .to_string();
    let ordinary_checkout = run_git(
        &missing_br,
        ["checkout", "--detach", current.as_str()],
        Some(OsStr::new("/usr/bin:/bin")),
    );
    assert_success(&ordinary_checkout, "ordinary checkout in existing worktree");
    assert!(
        !String::from_utf8_lossy(&ordinary_checkout.stderr).contains("br init --redirect"),
        "ordinary branch checkout must not invoke redirect provisioning"
    );
    assert!(!missing_br.join(".beads/redirect").exists());

    let bypass = repository.join("bypass-worktree");
    let bypassed = run_git(
        repository,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--no-checkout"),
            OsStr::new("--detach"),
            bypass.as_os_str(),
            OsStr::new(&historical),
        ],
        Some(OsStr::new("/usr/bin:/bin")),
    );
    assert_success(&bypassed, "create bypassed worktree");
    assert!(!bypass.join(".beads/redirect").exists());

    let canonical = fs::canonicalize(repository.join(".beads")).unwrap();
    let recovered = Command::new(br_binary())
        .args(["init", "--redirect"])
        .arg(&canonical)
        .current_dir(&bypass)
        .env("HOME", isolated_home(repository))
        .output()
        .expect("recover bypassed worktree manually");
    assert_success(&recovered, "manual redirect recovery");
    assert_redirect_target(&bypass, &canonical);
}

#[cfg(unix)]
#[test]
fn git_worktrunk_and_claude_converge_on_one_mutable_tracker_authority() {
    let (fixture, historical) = setup_git_repository();
    let repository = fixture.path();
    assert_success(
        &run_activation(repository),
        "activate post-checkout dispatcher",
    );

    let linked = repository.join("combined-lifecycle-worktree");
    let created = run_git(
        repository,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            linked.as_os_str(),
            OsStr::new(&historical),
        ],
        Some(&br_path()),
    );
    assert_success(&created, "create linked worktree through native Git hook");

    let redirect_path = linked.join(".beads/redirect");
    let redirect_bytes = fs::read(&redirect_path).unwrap();
    let redirect_modified = fs::metadata(&redirect_path).unwrap().modified().unwrap();
    let worktrunk = worktrunk_command();
    let claude = claude_command();
    let home = isolated_home(repository);
    let (worktrunk_result, claude_result) = std::thread::scope(|scope| {
        let worktrunk_run = scope.spawn(|| run_real_adapter(&worktrunk, &linked, &home));
        let claude_run = scope.spawn(|| run_real_adapter(&claude, &linked, &home));
        (worktrunk_run.join().unwrap(), claude_run.join().unwrap())
    });
    for (name, output) in [("Worktrunk", worktrunk_result), ("Claude", claude_result)] {
        assert_success(&output, &format!("{name} duplicate lifecycle adapter"));
        assert!(output.stdout.is_empty(), "{name} success must be quiet");
        assert!(output.stderr.is_empty(), "{name} success must be quiet");
    }
    assert_eq!(fs::read(&redirect_path).unwrap(), redirect_bytes);
    assert_eq!(
        fs::metadata(&redirect_path).unwrap().modified().unwrap(),
        redirect_modified,
        "duplicate lifecycle setup must not rewrite the redirect"
    );

    let created_issue = Command::new(br_binary())
        .args(["create", "Unified worktree authority", "--json"])
        .current_dir(&linked)
        .env("HOME", &home)
        .output()
        .expect("create issue through redirected worktree");
    assert_success(
        &created_issue,
        "mutate canonical tracker from linked worktree",
    );

    let listed = Command::new(br_binary())
        .args(["list", "--json"])
        .current_dir(repository)
        .env("HOME", &home)
        .env("BEADS_DIR", repository.join(".beads"))
        .output()
        .expect("read canonical tracker from primary worktree");
    assert_success(&listed, "read canonical tracker");
    let payload: JsonValue = serde_json::from_slice(&listed.stdout).unwrap();
    assert!(
        payload["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["title"] == "Unified worktree authority")
    );

    let mut local_entries = fs::read_dir(linked.join(".beads"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    local_entries.sort();
    assert_eq!(
        local_entries,
        [
            OsStr::new(".gitignore"),
            OsStr::new("README.md"),
            OsStr::new("config.yaml"),
            OsStr::new("interactions.jsonl"),
            OsStr::new("issues.jsonl"),
            OsStr::new("metadata.json"),
            OsStr::new("redirect"),
        ]
    );
    assert_redirect_target(&linked, &repository.join(".beads"));
}

#[test]
fn redirect_help_schema_capabilities_and_completions_are_discoverable() {
    for arguments in [
        ["init", "--help"].as_slice(),
        ["redirect", "set", "--help"].as_slice(),
    ] {
        let help = Command::new(br_binary())
            .args(arguments)
            .output()
            .expect("render redirect help");
        assert_success(&help, "render redirect help");
        let help = String::from_utf8(help.stdout).unwrap();
        assert!(help.contains("redirect"));
        assert!(help.contains("BEADS_DIR"));
    }

    let schema = Command::new(br_binary())
        .args(["schema", "redirect-receipt", "--format", "json"])
        .output()
        .expect("render redirect receipt schema");
    assert_success(&schema, "render redirect receipt schema");
    let schema: JsonValue = serde_json::from_slice(&schema.stdout).unwrap();
    assert!(schema["schemas"]["RedirectReceipt"].is_object());

    let capabilities = Command::new(br_binary())
        .args([
            "capabilities",
            "--command",
            "redirect set",
            "--format",
            "json",
        ])
        .output()
        .expect("render redirect capabilities");
    assert_success(&capabilities, "render redirect capabilities");
    let capabilities: JsonValue = serde_json::from_slice(&capabilities.stdout).unwrap();
    assert_eq!(capabilities["command_detail"]["path"], "redirect set");
    assert!(
        capabilities["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature["name"] == "shared_worktree_workspace")
    );

    let completions = Command::new(br_binary())
        .args(["--", "br", ""])
        .env("COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", "1")
        .env("_CLAP_COMPLETE_COMP_TYPE", "9")
        .env("_CLAP_COMPLETE_SPACE", "true")
        .output()
        .expect("query dynamic completions");
    assert_success(&completions, "query dynamic completions");
    assert!(
        String::from_utf8_lossy(&completions.stdout)
            .lines()
            .any(|candidate| candidate == "redirect")
    );
}
