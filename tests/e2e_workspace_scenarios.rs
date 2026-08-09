//! E2E scenarios for workspace initialization and diagnostic commands.
//!
//! Coverage:
//! - init (new workspace, re-init handling)
//! - config get/set/list (validate precedence)
//! - doctor (read-only diagnostics)
//! - info + where (paths + metadata)
//! - version (json + text)
//!
//! Uses the new harness infrastructure for artifact logging.
//!
//! Task: beads_rust-6esx

mod common;

use common::cli::{BrWorkspace, parse_list_issues, run_br};
use common::harness::{TestWorkspace, extract_json_payload};
use common::scenarios::{WorkspaceEvolutionEventKind, catalog};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn parse_json_stdout(stdout: &str, context: &str) -> Value {
    let payload = extract_json_payload(stdout);
    let message = format!("parse {context} json payload: {payload}");
    serde_json::from_str(&payload).expect(&message)
}

fn assert_doctor_json_has_healthy_checks(json: &Value) {
    let checks = json
        .get("checks")
        .and_then(Value::as_array)
        .or_else(|| json.as_array())
        .expect("doctor JSON should contain checks array");
    assert!(
        !checks.is_empty(),
        "doctor should report at least one check"
    );
    assert!(
        checks
            .iter()
            .all(|check| check["status"].as_str() != Some("error")),
        "healthy workspace doctor output should not contain errors: {checks:?}"
    );
}

fn regular_file_contents(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn collect(root: &Path, current: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(current).expect("read workspace directory") {
            let entry = entry.expect("read workspace entry");
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).expect("inspect workspace entry");
            if metadata.is_dir() {
                collect(root, &path, files);
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .expect("workspace entry should remain below root")
                    .to_string_lossy()
                    .into_owned();
                files.insert(relative, fs::read(path).expect("read workspace file"));
            }
        }
    }

    let mut files = BTreeMap::new();
    collect(root, root, &mut files);
    files
}

fn run_isolated_br_process(root: &Path, args: &[&str]) -> Output {
    run_isolated_br_process_with_env(root, args, std::iter::empty::<(&str, &Path)>())
}

fn run_isolated_br_process_with_env<'a, I>(root: &Path, args: &[&str], env: I) -> Output
where
    I: IntoIterator<Item = (&'a str, &'a Path)>,
{
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("br"));
    command
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "error")
        .args(args);
    command.envs(env);
    command.output().expect("run isolated br process")
}

fn run_git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("run git fixture setup");
    assert!(
        output.status.success(),
        "git fixture setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// =============================================================================
// Init Scenarios
// =============================================================================

#[test]
fn scenario_init_new_workspace() {
    let mut ws = TestWorkspace::new("e2e_workspace", "init_new");

    // Initialize a fresh workspace
    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Verify .beads directory was created
    let beads_dir = ws.root.join(".beads");
    assert!(
        beads_dir.exists(),
        ".beads directory should exist after init"
    );

    // Verify database was created
    let db_path = beads_dir.join("beads.db");
    assert!(db_path.exists(), "beads.db should exist after init");

    // Verify init output contains expected text
    assert!(
        init.stdout.contains("Initialized") || init.stdout.contains("initialized"),
        "init should confirm initialization: {}",
        init.stdout
    );

    ws.finish(true);
}

#[test]
fn scenario_init_reinit_rejected_without_force() {
    let mut ws = TestWorkspace::new("e2e_workspace", "init_reinit");

    // First init
    let init1 = ws.run_br(["init"], "init_first");
    init1.assert_success();

    // Create an issue to have some data
    let create = ws.run_br(["create", "Test issue"], "create");
    create.assert_success();

    // Second init without --force should fail (already initialized)
    let init2 = ws.run_br(["init"], "init_second");
    init2.assert_failure();
    assert!(
        init2.stderr.to_lowercase().contains("already")
            || init2.stderr.contains("ALREADY_INITIALIZED"),
        "re-init should report already initialized: stdout='{}' stderr='{}'",
        init2.stdout,
        init2.stderr
    );

    // Data should be preserved
    let list = ws.run_br(["list", "--json"], "list_after_reinit");
    list.assert_success();

    let issues = parse_list_issues(&list.stdout);
    assert!(
        !issues.is_empty(),
        "issues should be preserved after re-init"
    );

    ws.finish(true);
}

#[test]
fn scenario_init_json_output() {
    let mut ws = TestWorkspace::new("e2e_workspace", "init_json");

    // Init with JSON output
    let init = ws.run_br(["init", "--json"], "init_json");
    init.assert_success();

    let payload = extract_json_payload(&init.stdout);
    if !payload.is_empty() && (payload.starts_with('{') || payload.starts_with('[')) {
        let json: Value = serde_json::from_str(&payload).expect("parse init json");
        assert!(
            json.get("path").is_some() || json.get("workspace").is_some(),
            "init JSON should contain path or workspace field"
        );
    }

    ws.finish(true);
}

#[test]
fn scenario_init_redirect_explicit_target_routes_without_mutating_target() {
    let canonical = BrWorkspace::new();
    let canonical_init = run_br(&canonical, ["init"], "init_canonical");
    assert!(
        canonical_init.status.success(),
        "canonical init failed: {}",
        canonical_init.stderr
    );
    let canonical_beads = canonical
        .root
        .join(".beads")
        .canonicalize()
        .expect("canonicalize target workspace");
    let canonical_before = regular_file_contents(&canonical_beads);

    let secondary = BrWorkspace::new();
    let target = canonical_beads.to_string_lossy().into_owned();
    let setup = run_br(
        &secondary,
        ["init", "--redirect", target.as_str(), "--json"],
        "init_redirect_explicit",
    );
    assert!(
        setup.status.success(),
        "redirect init failed: {}",
        setup.stderr
    );

    let receipt = parse_json_stdout(&setup.stdout, "redirect init receipt");
    assert_eq!(receipt["schema"], "br.redirect.v1");
    assert_eq!(receipt["target_mode"], "explicit");
    assert_eq!(
        receipt["source_workspace"],
        secondary.root.join(".beads").to_string_lossy().as_ref()
    );
    assert_eq!(receipt["requested_target"], target);
    assert_eq!(
        receipt["final_target"],
        canonical_beads.to_string_lossy().as_ref()
    );
    assert_eq!(receipt["disposition"], "created");
    assert_eq!(receipt["changed"], true);
    assert_eq!(receipt["primary_worktree"], false);
    assert_eq!(receipt["dormant_artifacts"], serde_json::json!([]));

    let secondary_beads = secondary.root.join(".beads");
    assert_eq!(
        fs::read_to_string(secondary_beads.join("redirect")).expect("read redirect"),
        format!("{}\n", canonical_beads.display())
    );
    assert!(!secondary_beads.join("beads.db").exists());
    assert!(!secondary_beads.join("issues.jsonl").exists());
    assert_eq!(
        regular_file_contents(&canonical_beads),
        canonical_before,
        "redirect setup must not mutate the canonical tracker"
    );

    let created = run_br(
        &secondary,
        ["create", "Created through redirect"],
        "create_redirected",
    );
    assert!(
        created.status.success(),
        "redirected create failed: {}",
        created.stderr
    );
    let listed = run_br(&canonical, ["list", "--json"], "list_canonical");
    assert!(
        listed.status.success(),
        "canonical list failed: {}",
        listed.stderr
    );
    assert!(
        parse_list_issues(&listed.stdout)
            .iter()
            .any(|issue| issue["title"] == "Created through redirect"),
        "writes through the secondary workspace must land in the canonical tracker"
    );
}

#[test]
fn scenario_init_redirect_preserves_recognized_non_material_siblings() {
    let canonical = BrWorkspace::new();
    let canonical_init = run_br(&canonical, ["init"], "init_non_material_canonical");
    assert!(
        canonical_init.status.success(),
        "canonical init failed: {}",
        canonical_init.stderr
    );
    let canonical_beads = canonical.root.join(".beads").canonicalize().unwrap();
    let target = canonical_beads.to_string_lossy().into_owned();

    let secondary = BrWorkspace::new();
    let secondary_beads = secondary.root.join(".beads");
    fs::create_dir(&secondary_beads).unwrap();
    for (name, contents) in [
        (".gitignore", b"*.db\n".as_slice()),
        ("config.yaml", b"issue-prefix: dormant\n".as_slice()),
        ("metadata.json", br#"{"database":"beads.db"}"#.as_slice()),
        ("issues.jsonl", b"{\"id\":\"dormant-1\"}\n".as_slice()),
        ("interactions.jsonl", b"{\"kind\":\"dormant\"}\n".as_slice()),
        ("README.md", b"# Dormant tracker documentation\n".as_slice()),
    ] {
        fs::write(secondary_beads.join(name), contents).unwrap();
    }
    let dormant_before = regular_file_contents(&secondary_beads);

    let setup = run_br(
        &secondary,
        ["init", "--redirect", target.as_str(), "--json"],
        "init_redirect_non_material_siblings",
    );
    assert!(
        setup.status.success(),
        "redirect init failed: stdout={} stderr={}",
        setup.stdout,
        setup.stderr
    );
    let receipt = parse_json_stdout(&setup.stdout, "non-material redirect receipt");
    assert_eq!(receipt["disposition"], "created");
    assert_eq!(receipt["existing_state_acknowledged"], false);
    let dormant = receipt["dormant_artifacts"].as_array().unwrap();
    for expected in [
        ".gitignore",
        "config.yaml",
        "metadata.json",
        "issues.jsonl",
        "interactions.jsonl",
        "README.md",
    ] {
        assert!(
            dormant
                .iter()
                .filter_map(Value::as_str)
                .any(|path| path.ends_with(expected)),
            "receipt must inventory {expected}: {dormant:?}"
        );
    }

    let mut dormant_after = regular_file_contents(&secondary_beads);
    dormant_after.remove("redirect");
    assert_eq!(dormant_after, dormant_before);

    let repeated = run_br(
        &secondary,
        ["init", "--redirect", target.as_str()],
        "init_redirect_non_material_siblings_human_noop",
    );
    assert!(repeated.status.success(), "{}", repeated.stderr);
    for expected in ["issues.jsonl", "interactions.jsonl", "README.md"] {
        assert!(
            repeated.stdout.contains(expected),
            "human no-op receipt must inventory {expected}: {}",
            repeated.stdout
        );
    }
}

#[test]
fn scenario_init_redirect_concurrent_same_target_converges() {
    let canonical = BrWorkspace::new();
    let canonical_init = run_br(&canonical, ["init"], "init_canonical_concurrent");
    assert!(canonical_init.status.success(), "{}", canonical_init.stderr);
    let target = canonical
        .root
        .join(".beads")
        .canonicalize()
        .expect("canonicalize concurrent target");
    let target_text = target.to_string_lossy().into_owned();

    let secondary = BrWorkspace::new();
    let first_root = secondary.root.clone();
    let first_target = target_text.clone();
    let first = std::thread::spawn(move || {
        run_isolated_br_process(
            &first_root,
            &["init", "--redirect", first_target.as_str(), "--json"],
        )
    });
    let second_root = secondary.root.clone();
    let second_target = target_text.clone();
    let second = std::thread::spawn(move || {
        run_isolated_br_process(
            &second_root,
            &["init", "--redirect", second_target.as_str(), "--json"],
        )
    });

    let outputs = [
        first.join().expect("first redirect setup"),
        second.join().expect("second redirect setup"),
    ];
    for output in &outputs {
        assert!(
            output.status.success(),
            "same-target redirect setup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut dispositions = outputs
        .iter()
        .map(|output| {
            serde_json::from_slice::<Value>(&output.stdout)
                .expect("parse concurrent redirect receipt")["disposition"]
                .as_str()
                .expect("redirect disposition")
                .to_string()
        })
        .collect::<Vec<_>>();
    dispositions.sort();
    assert_eq!(dispositions, ["created", "unchanged"]);
    assert_eq!(
        fs::read_to_string(secondary.root.join(".beads/redirect")).expect("read redirect"),
        format!("{}\n", target.display())
    );
}

#[test]
fn scenario_init_redirect_concurrent_conflicting_targets_preserve_one_winner() {
    let first = BrWorkspace::new();
    let second = BrWorkspace::new();
    assert!(
        run_br(&first, ["init"], "init_concurrent_first")
            .status
            .success()
    );
    assert!(
        run_br(&second, ["init"], "init_concurrent_second")
            .status
            .success()
    );
    let first_target = first.root.join(".beads").canonicalize().unwrap();
    let second_target = second.root.join(".beads").canonicalize().unwrap();
    let first_text = first_target.to_string_lossy().into_owned();
    let second_text = second_target.to_string_lossy().into_owned();

    let secondary = BrWorkspace::new();
    let first_root = secondary.root.clone();
    let first_call = std::thread::spawn(move || {
        run_isolated_br_process(
            &first_root,
            &["init", "--redirect", first_text.as_str(), "--json"],
        )
    });
    let second_root = secondary.root.clone();
    let second_call = std::thread::spawn(move || {
        run_isolated_br_process(
            &second_root,
            &["init", "--redirect", second_text.as_str(), "--json"],
        )
    });
    let outputs = [first_call.join().unwrap(), second_call.join().unwrap()];
    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.status.success())
            .count(),
        1,
        "exactly one conflicting authority must win: {outputs:?}"
    );

    let redirect = fs::read_to_string(secondary.root.join(".beads/redirect")).unwrap();
    assert!(
        redirect == format!("{}\n", first_target.display())
            || redirect == format!("{}\n", second_target.display()),
        "redirect must preserve exactly one complete authority: {redirect:?}"
    );
}

#[test]
fn scenario_init_redirect_same_target_is_byte_preserving_noop() {
    let canonical = BrWorkspace::new();
    assert!(
        run_br(&canonical, ["init"], "init_canonical_noop")
            .status
            .success()
    );
    let target = canonical.root.join(".beads").canonicalize().unwrap();
    let target_text = target.to_string_lossy().into_owned();
    let secondary = BrWorkspace::new();

    let created = run_br(
        &secondary,
        ["init", "--redirect", target_text.as_str(), "--json"],
        "redirect_created",
    );
    assert!(created.status.success(), "{}", created.stderr);
    let redirect_path = secondary.root.join(".beads/redirect");
    let bytes_before = fs::read(&redirect_path).unwrap();
    let modified_before = fs::metadata(&redirect_path).unwrap().modified().unwrap();

    let repeated = run_br(
        &secondary,
        ["init", "--redirect", target_text.as_str(), "--json"],
        "redirect_repeated",
    );
    assert!(repeated.status.success(), "{}", repeated.stderr);
    let receipt = parse_json_stdout(&repeated.stdout, "repeated redirect receipt");
    assert_eq!(receipt["disposition"], "unchanged");
    assert_eq!(receipt["changed"], false);
    assert_eq!(fs::read(&redirect_path).unwrap(), bytes_before);
    assert_eq!(
        fs::metadata(&redirect_path).unwrap().modified().unwrap(),
        modified_before,
        "same-target setup must not rewrite redirect metadata"
    );
}

#[test]
fn scenario_init_redirect_primary_target_is_successful_owner_noop() {
    let primary = BrWorkspace::new();
    assert!(run_br(&primary, ["init"], "init_primary").status.success());
    let target = primary.root.join(".beads").canonicalize().unwrap();
    let before = regular_file_contents(&target);
    let target_text = target.to_string_lossy().into_owned();

    let setup = run_br(
        &primary,
        ["init", "--redirect", target_text.as_str(), "--json"],
        "redirect_primary",
    );
    assert!(setup.status.success(), "{}", setup.stderr);
    let receipt = parse_json_stdout(&setup.stdout, "primary redirect receipt");
    assert_eq!(receipt["disposition"], "primary_owner");
    assert_eq!(receipt["changed"], false);
    assert_eq!(receipt["primary_worktree"], true);
    assert!(!target.join("redirect").exists());
    assert_eq!(regular_file_contents(&target), before);
}

#[test]
fn scenario_init_redirect_rejects_invalid_targets_and_init_options_without_local_changes() {
    let secondary = BrWorkspace::new();
    let not_beads = BrWorkspace::new();
    let not_beads_text = not_beads.root.to_string_lossy().into_owned();
    let invalid_shape = run_br(
        &secondary,
        ["init", "--redirect", not_beads_text.as_str(), "--json"],
        "redirect_invalid_shape",
    );
    assert!(!invalid_shape.status.success());
    assert!(!secondary.root.join(".beads").exists());

    let unusable = BrWorkspace::new();
    fs::create_dir(unusable.root.join(".beads")).unwrap();
    let unusable_text = unusable.root.join(".beads").to_string_lossy().into_owned();
    let invalid_tracker = run_br(
        &secondary,
        ["init", "--redirect", unusable_text.as_str(), "--json"],
        "redirect_unusable_tracker",
    );
    assert!(!invalid_tracker.status.success());
    assert!(!secondary.root.join(".beads").exists());

    let missing = secondary.root.join("missing/.beads");
    let missing_text = missing.to_string_lossy().into_owned();
    let missing_target = run_br(
        &secondary,
        ["init", "--redirect", missing_text.as_str(), "--json"],
        "redirect_missing_target",
    );
    assert!(!missing_target.status.success());
    assert!(!secondary.root.join(".beads").exists());

    let canonical = BrWorkspace::new();
    assert!(
        run_br(&canonical, ["init"], "init_for_option_conflict")
            .status
            .success()
    );
    let target = canonical.root.join(".beads").canonicalize().unwrap();
    let target_text = target.to_string_lossy().into_owned();
    for args in [
        vec!["init", "--redirect", target_text.as_str(), "--force"],
        vec![
            "init",
            "--redirect",
            target_text.as_str(),
            "--prefix",
            "other",
        ],
        vec![
            "init",
            "--redirect",
            target_text.as_str(),
            "--backend",
            "sqlite",
        ],
    ] {
        let output = run_isolated_br_process(&secondary.root, &args);
        assert!(
            !output.status.success(),
            "incompatible init flags must fail"
        );
        assert!(!secondary.root.join(".beads").exists());
    }
}

#[test]
fn scenario_init_redirect_preserves_conflicting_authority_and_local_state() {
    let first = BrWorkspace::new();
    let second = BrWorkspace::new();
    assert!(
        run_br(&first, ["init"], "init_first_authority")
            .status
            .success()
    );
    assert!(
        run_br(&second, ["init"], "init_second_authority")
            .status
            .success()
    );
    let first_target = first.root.join(".beads").canonicalize().unwrap();
    let second_target = second.root.join(".beads").canonicalize().unwrap();
    let first_text = first_target.to_string_lossy().into_owned();
    let second_text = second_target.to_string_lossy().into_owned();

    let secondary = BrWorkspace::new();
    let initial = run_br(
        &secondary,
        ["init", "--redirect", first_text.as_str(), "--json"],
        "redirect_first_authority",
    );
    assert!(initial.status.success(), "{}", initial.stderr);
    let redirect_path = secondary.root.join(".beads/redirect");
    let redirect_before = fs::read(&redirect_path).unwrap();

    let conflict = run_br(
        &secondary,
        ["init", "--redirect", second_text.as_str(), "--json"],
        "redirect_conflicting_authority",
    );
    assert!(!conflict.status.success());
    let conflict_receipt = parse_json_stdout(&conflict.stdout, "redirect conflict envelope");
    assert_eq!(
        conflict_receipt["error"]["context"]["schema"],
        "br.redirect.v1"
    );
    assert_eq!(
        conflict_receipt["error"]["context"]["disposition"],
        "refused"
    );
    assert_eq!(
        conflict_receipt["error"]["context"]["final_target"],
        second_target.to_string_lossy().as_ref()
    );
    assert_eq!(fs::read(&redirect_path).unwrap(), redirect_before);

    let local_state = BrWorkspace::new();
    fs::create_dir(local_state.root.join(".beads")).unwrap();
    fs::write(
        local_state.root.join(".beads/preserved.txt"),
        b"do not shadow",
    )
    .unwrap();
    let refused = run_br(
        &local_state,
        ["init", "--redirect", first_text.as_str(), "--json"],
        "redirect_existing_local_state",
    );
    assert!(!refused.status.success());
    let refusal_receipt = parse_json_stdout(&refused.stdout, "fresh-state refusal envelope");
    assert_eq!(
        refusal_receipt["error"]["context"]["disposition"],
        "refused"
    );
    assert!(
        refusal_receipt["error"]["context"]["dormant_artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .any(|path| path.ends_with("preserved.txt"))
    );
    assert_eq!(
        fs::read(local_state.root.join(".beads/preserved.txt")).unwrap(),
        b"do not shadow"
    );
    assert!(!local_state.root.join(".beads/redirect").exists());
}

#[test]
fn scenario_init_redirect_chain_uses_terminal_canonical_authority() {
    let canonical = BrWorkspace::new();
    assert!(
        run_br(&canonical, ["init"], "init_chain_canonical")
            .status
            .success()
    );
    let canonical_target = canonical.root.join(".beads").canonicalize().unwrap();

    let intermediate = BrWorkspace::new();
    let intermediate_beads = intermediate.root.join(".beads");
    fs::create_dir(&intermediate_beads).unwrap();
    fs::write(
        intermediate_beads.join("redirect"),
        format!("{}\n", canonical_target.display()),
    )
    .unwrap();
    let intermediate_target = intermediate_beads.canonicalize().unwrap();
    let intermediate_text = intermediate_target.to_string_lossy().into_owned();

    let secondary = BrWorkspace::new();
    let setup = run_br(
        &secondary,
        ["init", "--redirect", intermediate_text.as_str(), "--json"],
        "redirect_chain",
    );
    assert!(setup.status.success(), "{}", setup.stderr);
    let receipt = parse_json_stdout(&setup.stdout, "redirect chain receipt");
    assert_eq!(
        receipt["requested_target"],
        intermediate_target.to_string_lossy().as_ref()
    );
    assert_eq!(
        receipt["final_target"],
        canonical_target.to_string_lossy().as_ref()
    );
    assert_eq!(
        fs::read_to_string(secondary.root.join(".beads/redirect")).unwrap(),
        format!("{}\n", canonical_target.display())
    );
}

#[test]
fn scenario_init_redirect_rejects_loops_and_excessive_chain_depth() {
    let loop_fixture = TempDir::new_in(common::cli::isolated_temp_root()).unwrap();
    let first = loop_fixture.path().join("first/.beads");
    let second = loop_fixture.path().join("second/.beads");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    let first = first.canonicalize().unwrap();
    let second = second.canonicalize().unwrap();
    fs::write(first.join("redirect"), format!("{}\n", second.display())).unwrap();
    fs::write(second.join("redirect"), format!("{}\n", first.display())).unwrap();

    let loop_source = BrWorkspace::new();
    let first_text = first.to_string_lossy().into_owned();
    let looped = run_br(
        &loop_source,
        ["init", "--redirect", first_text.as_str(), "--json"],
        "redirect_loop",
    );
    assert!(!looped.status.success());
    assert!(looped.stdout.contains("Redirect loop detected"));
    assert!(!loop_source.root.join(".beads").exists());

    let depth_fixture = TempDir::new_in(common::cli::isolated_temp_root()).unwrap();
    let chain = (0..11)
        .map(|index| depth_fixture.path().join(format!("chain-{index}/.beads")))
        .collect::<Vec<_>>();
    for workspace in &chain {
        fs::create_dir_all(workspace).unwrap();
    }
    for pair in chain.windows(2) {
        let target = pair[1].canonicalize().unwrap();
        fs::write(pair[0].join("redirect"), format!("{}\n", target.display())).unwrap();
    }
    let terminal = depth_fixture.path().join("terminal/.beads");
    fs::create_dir_all(&terminal).unwrap();
    fs::write(
        chain.last().unwrap().join("redirect"),
        format!("{}\n", terminal.canonicalize().unwrap().display()),
    )
    .unwrap();

    let depth_source = BrWorkspace::new();
    let chain_start = chain[0].canonicalize().unwrap();
    let chain_start_text = chain_start.to_string_lossy().into_owned();
    let too_deep = run_br(
        &depth_source,
        ["init", "--redirect", chain_start_text.as_str(), "--json"],
        "redirect_depth",
    );
    assert!(!too_deep.status.success());
    assert!(too_deep.stdout.contains("max depth"));
    assert!(!depth_source.root.join(".beads").exists());
}

#[cfg(unix)]
#[test]
fn scenario_init_redirect_automatic_discovers_primary_without_spawning_git() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = TempDir::new_in(common::cli::isolated_temp_root()).unwrap();
    let primary = fixture.path().join("primary");
    let secondary = fixture.path().join("secondary");
    fs::create_dir(&primary).unwrap();
    run_git(&primary, &["init", "-q"]);
    run_git(&primary, &["config", "user.name", "Redirect Test"]);
    run_git(
        &primary,
        &["config", "user.email", "redirect-test@example.invalid"],
    );
    run_git(&primary, &["commit", "--allow-empty", "-qm", "initial"]);
    let primary_init = run_isolated_br_process(&primary, &["init"]);
    assert!(
        primary_init.status.success(),
        "primary br init failed: {}",
        String::from_utf8_lossy(&primary_init.stderr)
    );
    run_git(
        &primary,
        &["worktree", "add", "-q", secondary.to_str().unwrap()],
    );

    let trap_dir = fixture.path().join("trap-bin");
    fs::create_dir(&trap_dir).unwrap();
    let sentinel = fixture.path().join("git-was-spawned");
    let fake_git = trap_dir.join("git");
    fs::write(
        &fake_git,
        format!("#!/bin/sh\n: > '{}'\nexit 99\n", sentinel.display()),
    )
    .unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();
    let bogus_beads = fixture.path().join("ambient/.beads");
    let bogus_db = fixture.path().join("ambient/beads.db");
    let nested_secondary = secondary.join("nested/working/directory");
    fs::create_dir_all(&nested_secondary).unwrap();

    let setup = run_isolated_br_process_with_env(
        &nested_secondary,
        &["init", "--redirect", "--json"],
        [
            ("PATH", trap_dir.as_path()),
            ("BEADS_DIR", bogus_beads.as_path()),
            ("BD_DB", bogus_db.as_path()),
        ],
    );
    assert!(
        setup.status.success(),
        "automatic redirect failed: stdout={} stderr={}",
        String::from_utf8_lossy(&setup.stdout),
        String::from_utf8_lossy(&setup.stderr)
    );
    assert!(!sentinel.exists(), "br must not spawn Git during discovery");

    let receipt: Value = serde_json::from_slice(&setup.stdout).unwrap();
    let canonical_target = primary.join(".beads").canonicalize().unwrap();
    assert_eq!(receipt["target_mode"], "automatic");
    assert_eq!(receipt["requested_target"], Value::Null);
    assert_eq!(
        receipt["source_workspace"],
        secondary.join(".beads").to_string_lossy().as_ref()
    );
    assert_eq!(
        receipt["final_target"],
        canonical_target.to_string_lossy().as_ref()
    );
    assert_eq!(
        fs::read_to_string(secondary.join(".beads/redirect")).unwrap(),
        format!("{}\n", canonical_target.display())
    );

    let primary_noop = run_isolated_br_process(&primary, &["init", "--redirect", "--json"]);
    assert!(primary_noop.status.success());
    let primary_receipt: Value = serde_json::from_slice(&primary_noop.stdout).unwrap();
    assert_eq!(primary_receipt["target_mode"], "automatic");
    assert_eq!(primary_receipt["disposition"], "primary_owner");
    assert!(!primary.join(".beads/redirect").exists());
}

#[test]
fn scenario_init_redirect_automatic_refuses_unsupported_and_separated_git_layouts() {
    let unsupported = BrWorkspace::new();
    let no_git = run_br(
        &unsupported,
        ["init", "--redirect", "--json"],
        "redirect_without_git",
    );
    assert!(!no_git.status.success());
    assert!(
        no_git.stdout.contains("provide the exact .beads path")
            || no_git.stderr.contains("provide the exact .beads path")
    );
    assert!(!unsupported.root.join(".beads").exists());

    let malformed = BrWorkspace::new();
    fs::write(
        malformed.root.join(".git"),
        "gitdir: /first/location\ngitdir: /second/location\n",
    )
    .unwrap();
    let ambiguous = run_br(
        &malformed,
        ["init", "--redirect", "--json"],
        "redirect_ambiguous_git_file",
    );
    assert!(!ambiguous.status.success());
    assert!(!malformed.root.join(".beads").exists());

    let bare = BrWorkspace::new();
    run_git(&bare.root, &["init", "--bare", "-q"]);
    let bare_result = run_br(
        &bare,
        ["init", "--redirect", "--json"],
        "redirect_bare_repository",
    );
    assert!(!bare_result.status.success());
    assert!(!bare.root.join(".beads").exists());

    let fixture = TempDir::new_in(common::cli::isolated_temp_root()).unwrap();
    let separated_root = fixture.path().join("separated-worktree");
    let separated_git = fixture.path().join("separated-admin");
    fs::create_dir(&separated_root).unwrap();
    run_git(
        fixture.path(),
        &[
            "init",
            "-q",
            "--separate-git-dir",
            separated_git.to_str().unwrap(),
            separated_root.to_str().unwrap(),
        ],
    );
    let init = run_isolated_br_process(&separated_root, &["init"]);
    assert!(init.status.success());
    let before = regular_file_contents(&separated_root.join(".beads"));

    let separated = run_isolated_br_process(&separated_root, &["init", "--redirect", "--json"]);
    assert!(!separated.status.success());
    assert!(
        String::from_utf8_lossy(&separated.stdout).contains("provide the exact .beads path")
            || String::from_utf8_lossy(&separated.stderr).contains("provide the exact .beads path")
    );
    assert!(!separated_root.join(".beads/redirect").exists());
    assert_eq!(
        regular_file_contents(&separated_root.join(".beads")),
        before
    );
}

#[cfg(unix)]
#[test]
fn scenario_redirect_set_requires_acknowledgement_and_preserves_dormant_state() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let canonical = BrWorkspace::new();
    assert!(
        run_br(&canonical, ["init"], "init_adoption_canonical")
            .status
            .success()
    );
    let canonical_target = canonical.root.join(".beads").canonicalize().unwrap();
    let target_text = canonical_target.to_string_lossy().into_owned();

    let local = BrWorkspace::new();
    assert!(
        run_br(&local, ["init"], "init_adoption_local")
            .status
            .success()
    );
    assert!(
        run_br(
            &local,
            ["create", "Dormant local issue"],
            "create_dormant_local"
        )
        .status
        .success()
    );
    let local_beads = local.root.join(".beads");
    let policy_path = local_beads.join("policy.yaml");
    fs::write(&policy_path, b"close_requires_reason: true\n").unwrap();
    fs::set_permissions(&policy_path, fs::Permissions::from_mode(0o640)).unwrap();
    let external = local.root.join("external-policy");
    fs::write(&external, b"external sentinel\n").unwrap();
    let symlink_path = local_beads.join("policy-link");
    symlink(&external, &symlink_path).unwrap();

    let dormant_before = regular_file_contents(&local_beads);
    let policy_metadata_before = fs::symlink_metadata(&policy_path).unwrap();
    let symlink_target_before = fs::read_link(&symlink_path).unwrap();

    let refused = run_br(
        &local,
        ["redirect", "set", target_text.as_str(), "--json"],
        "redirect_set_refused",
    );
    assert!(!refused.status.success());
    let refusal = parse_json_stdout(&refused.stdout, "redirect refusal envelope");
    assert_eq!(refusal["error"]["code"], "CONFIG_ERROR");
    assert_eq!(refusal["error"]["context"]["schema"], "br.redirect.v1");
    assert_eq!(refusal["error"]["context"]["disposition"], "refused");
    assert_eq!(refusal["error"]["context"]["changed"], false);
    let refused_dormant = refusal["error"]["context"]["dormant_artifacts"]
        .as_array()
        .unwrap();
    for expected in ["beads.db", "issues.jsonl", "policy.yaml", "policy-link"] {
        assert!(
            refused_dormant
                .iter()
                .filter_map(Value::as_str)
                .any(|path| path.ends_with(expected)),
            "refusal inventory must include {expected}: {refused_dormant:?}"
        );
    }
    assert!(!local_beads.join("redirect").exists());
    assert_eq!(regular_file_contents(&local_beads), dormant_before);
    assert_eq!(fs::read_link(&symlink_path).unwrap(), symlink_target_before);

    let human_refusal = run_br(
        &local,
        ["redirect", "set", target_text.as_str()],
        "redirect_set_human_refused",
    );
    assert!(!human_refusal.status.success());
    for expected in ["beads.db", "issues.jsonl", "policy.yaml", "policy-link"] {
        assert!(
            human_refusal.stderr.contains(expected),
            "human refusal must inventory {expected}: {}",
            human_refusal.stderr
        );
    }

    let adopted = run_br(
        &local,
        [
            "redirect",
            "set",
            target_text.as_str(),
            "--allow-existing",
            "--json",
        ],
        "redirect_set_acknowledged",
    );
    assert!(adopted.status.success(), "{}", adopted.stderr);
    let receipt = parse_json_stdout(&adopted.stdout, "redirect adoption receipt");
    assert_eq!(receipt["schema"], "br.redirect.v1");
    assert_eq!(receipt["target_mode"], "explicit");
    assert_eq!(receipt["disposition"], "created");
    assert_eq!(receipt["existing_state_acknowledged"], true);
    let dormant = receipt["dormant_artifacts"].as_array().unwrap();
    for expected in ["beads.db", "issues.jsonl", "policy.yaml", "policy-link"] {
        assert!(
            dormant
                .iter()
                .filter_map(Value::as_str)
                .any(|path| path.ends_with(expected)),
            "dormant inventory must include {expected}: {dormant:?}"
        );
    }

    let mut dormant_after = regular_file_contents(&local_beads);
    dormant_after.remove("redirect");
    assert_eq!(dormant_after, dormant_before);
    let policy_metadata_after = fs::symlink_metadata(&policy_path).unwrap();
    assert_eq!(
        policy_metadata_after.permissions().mode(),
        policy_metadata_before.permissions().mode()
    );
    assert_eq!(
        policy_metadata_after.modified().unwrap(),
        policy_metadata_before.modified().unwrap()
    );
    assert_eq!(fs::read_link(&symlink_path).unwrap(), symlink_target_before);

    let local_before_redirected_create = regular_file_contents(&local_beads);
    let redirected_create = run_br(
        &local,
        ["create", "Created after adoption"],
        "create_after_adoption",
    );
    assert!(
        redirected_create.status.success(),
        "{}",
        redirected_create.stderr
    );
    assert_eq!(
        regular_file_contents(&local_beads),
        local_before_redirected_create,
        "redirected mutation must not touch dormant local artifacts"
    );
    let canonical_list = run_br(&canonical, ["list", "--json"], "list_after_adoption");
    assert!(canonical_list.status.success());
    assert!(
        parse_list_issues(&canonical_list.stdout)
            .iter()
            .any(|issue| issue["title"] == "Created after adoption")
    );

    let human_local = BrWorkspace::new();
    let human_beads = human_local.root.join(".beads");
    fs::create_dir(&human_beads).unwrap();
    fs::write(human_beads.join("beads.db"), b"dormant database").unwrap();
    fs::write(human_beads.join("policy.yaml"), b"policy: preserved\n").unwrap();
    let human_adoption = run_br(
        &human_local,
        ["redirect", "set", target_text.as_str(), "--allow-existing"],
        "redirect_set_human_acknowledged",
    );
    assert!(human_adoption.status.success(), "{}", human_adoption.stderr);
    assert!(human_adoption.stdout.contains("beads.db"));
    assert!(human_adoption.stdout.contains("policy.yaml"));
}

#[test]
fn scenario_redirect_set_allows_tracked_siblings_without_independent_database() {
    let canonical = BrWorkspace::new();
    assert!(
        run_br(&canonical, ["init"], "init_tracked_canonical")
            .status
            .success()
    );
    let target = canonical.root.join(".beads").canonicalize().unwrap();
    let target_text = target.to_string_lossy().into_owned();

    let local = BrWorkspace::new();
    let local_beads = local.root.join(".beads");
    fs::create_dir(&local_beads).unwrap();
    fs::write(local_beads.join(".gitignore"), b"*.db\nredirect\n").unwrap();
    fs::write(
        local_beads.join("metadata.json"),
        b"{\"database\":\"beads.db\",\"jsonl_export\":\"issues.jsonl\"}\n",
    )
    .unwrap();
    fs::write(local_beads.join("config.yaml"), b"default_priority: 2\n").unwrap();
    fs::write(
        local_beads.join("policy.yaml"),
        b"close_requires_reason: true\n",
    )
    .unwrap();
    fs::write(local_beads.join("README.md"), b"tracked workspace notes\n").unwrap();
    fs::write(local_beads.join("issues.jsonl"), b"\n").unwrap();
    let before = regular_file_contents(&local_beads);

    let setup = run_br(
        &local,
        ["redirect", "set", target_text.as_str(), "--json"],
        "redirect_set_tracked_siblings",
    );
    assert!(setup.status.success(), "{}", setup.stderr);
    let receipt = parse_json_stdout(&setup.stdout, "tracked sibling receipt");
    assert_eq!(receipt["existing_state_acknowledged"], false);
    assert_eq!(
        receipt["dormant_artifacts"].as_array().unwrap().len(),
        before.len()
    );
    let mut after = regular_file_contents(&local_beads);
    after.remove("redirect");
    assert_eq!(after, before);
}

#[test]
fn scenario_redirect_set_automatic_adopts_linked_worktree_and_repeats_without_flag() {
    let fixture = TempDir::new_in(common::cli::isolated_temp_root()).unwrap();
    let primary = fixture.path().join("primary");
    let secondary = fixture.path().join("secondary");
    fs::create_dir(&primary).unwrap();
    run_git(&primary, &["init", "-q"]);
    run_git(&primary, &["config", "user.name", "Redirect Test"]);
    run_git(
        &primary,
        &["config", "user.email", "redirect-test@example.invalid"],
    );
    run_git(&primary, &["commit", "--allow-empty", "-qm", "initial"]);
    assert!(
        run_isolated_br_process(&primary, &["init"])
            .status
            .success()
    );
    run_git(
        &primary,
        &["worktree", "add", "-q", secondary.to_str().unwrap()],
    );
    assert!(
        run_isolated_br_process(&secondary, &["init"])
            .status
            .success()
    );
    assert!(
        run_isolated_br_process(&secondary, &["create", "Independent secondary issue"])
            .status
            .success()
    );

    let adopted = run_isolated_br_process(
        &secondary,
        &["redirect", "set", "--allow-existing", "--json"],
    );
    assert!(
        adopted.status.success(),
        "automatic adoption failed: {}",
        String::from_utf8_lossy(&adopted.stderr)
    );
    let receipt: Value = serde_json::from_slice(&adopted.stdout).unwrap();
    assert_eq!(receipt["target_mode"], "automatic");
    assert_eq!(receipt["existing_state_acknowledged"], true);
    assert_eq!(
        receipt["final_target"],
        primary
            .join(".beads")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );

    let repeated = run_isolated_br_process(&secondary, &["redirect", "set", "--json"]);
    assert!(
        repeated.status.success(),
        "same automatic adoption should be idempotent without another acknowledgement: {}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    let repeated_receipt: Value = serde_json::from_slice(&repeated.stdout).unwrap();
    assert_eq!(repeated_receipt["disposition"], "unchanged");
    assert_eq!(repeated_receipt["changed"], false);
    assert_eq!(repeated_receipt["existing_state_acknowledged"], false);
}

// =============================================================================
// Config Scenarios
// =============================================================================

#[test]
fn scenario_config_list() {
    let mut ws = TestWorkspace::new("e2e_workspace", "config_list");

    // Init first
    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // List configuration
    let list = ws.run_br(["config", "list"], "config_list");
    list.assert_success();

    // Should contain configuration output
    assert!(!list.stdout.is_empty(), "config list should produce output");

    ws.finish(true);
}

#[test]
fn scenario_config_list_json() {
    let mut ws = TestWorkspace::new("e2e_workspace", "config_list_json");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    let list = ws.run_br(["config", "list", "--json"], "config_list_json");
    list.assert_success();

    let payload = extract_json_payload(&list.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse config list json");
    assert!(json.is_object(), "config list --json should return object");

    ws.finish(true);
}

#[test]
fn scenario_config_set_and_get() {
    let mut ws = TestWorkspace::new("e2e_workspace", "config_set_get");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Set a config value
    let set = ws.run_br(["config", "set", "issue_prefix=test_prefix"], "config_set");
    set.assert_success();

    // Get the value back
    let get = ws.run_br(["config", "get", "issue_prefix"], "config_get");
    get.assert_success();
    assert!(
        get.stdout.contains("test_prefix"),
        "config get should show set value: {}",
        get.stdout
    );

    ws.finish(true);
}

#[test]
fn scenario_config_get_json() {
    let mut ws = TestWorkspace::new("e2e_workspace", "config_get_json");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Set a value first
    let set = ws.run_br(["config", "set", "json=true"], "config_set");
    set.assert_success();

    // Get with JSON output
    let get = ws.run_br(["config", "get", "json", "--json"], "config_get_json");
    get.assert_success();

    let json = parse_json_stdout(&get.stdout, "config get");
    assert_eq!(json["key"].as_str(), Some("json"));
    assert_eq!(json["value"].as_str(), Some("true"));

    ws.finish(true);
}

#[test]
fn scenario_config_path() {
    let mut ws = TestWorkspace::new("e2e_workspace", "config_path");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Get config file path
    let path = ws.run_br(["config", "path"], "config_path");
    path.assert_success();

    // Should contain a path
    let stdout = &path.stdout;
    assert!(
        stdout.contains("beads") || stdout.contains('.'),
        "config path should output a path: {stdout}"
    );

    ws.finish(true);
}

// =============================================================================
// Doctor Scenarios
// =============================================================================

#[test]
fn scenario_doctor_healthy_workspace() {
    let mut ws = TestWorkspace::new("e2e_workspace", "doctor_healthy");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Doctor on healthy workspace should pass
    let doctor = ws.run_br(["doctor"], "doctor");
    doctor.assert_success();
    let stdout = doctor.stdout.to_ascii_lowercase();
    assert!(
        stdout.contains("ok") || stdout.contains("healthy"),
        "doctor should report healthy checks: {}",
        doctor.stdout
    );

    ws.finish(true);
}

#[test]
fn scenario_doctor_json_output() {
    let mut ws = TestWorkspace::new("e2e_workspace", "doctor_json");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    let doctor = ws.run_br(["doctor", "--json"], "doctor_json");
    doctor.assert_success();

    let json = parse_json_stdout(&doctor.stdout, "doctor");
    assert_doctor_json_has_healthy_checks(&json);

    ws.finish(true);
}

#[test]
fn scenario_doctor_no_workspace() {
    let mut ws = TestWorkspace::new("e2e_workspace", "doctor_no_workspace");
    // Do NOT init

    let doctor = ws.run_br(["doctor"], "doctor_no_init");
    // Should fail or warn about missing workspace
    // (behavior may vary - just verify it doesn't crash)
    assert!(
        !doctor.success || doctor.stderr.contains("not initialized"),
        "doctor should indicate missing workspace"
    );

    ws.finish(true);
}

// =============================================================================
// Info Scenarios
// =============================================================================

#[test]
fn scenario_info_shows_paths() {
    let mut ws = TestWorkspace::new("e2e_workspace", "info_paths");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    let info = ws.run_br(["info"], "info");
    info.assert_success();

    // Should contain workspace path info
    assert!(!info.stdout.is_empty(), "info should produce output");

    ws.finish(true);
}

#[test]
fn scenario_info_json_output() {
    let mut ws = TestWorkspace::new("e2e_workspace", "info_json");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    let info = ws.run_br(["info", "--json"], "info_json");
    info.assert_success();

    let payload = extract_json_payload(&info.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse info json");
    assert!(json.is_object(), "info --json should return object");

    ws.finish(true);
}

// =============================================================================
// Where Scenarios
// =============================================================================

#[test]
fn scenario_where_shows_workspace_path() {
    let mut ws = TestWorkspace::new("e2e_workspace", "where_path");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    let where_cmd = ws.run_br(["where"], "where");
    where_cmd.assert_success();

    // Should show a path to the workspace
    let stdout = &where_cmd.stdout;
    assert!(
        stdout.contains('/') || stdout.contains('\\'),
        "where should output a path: {stdout}"
    );

    ws.finish(true);
}

#[test]
fn scenario_where_no_workspace() {
    let mut ws = TestWorkspace::new("e2e_workspace", "where_no_workspace");
    // Do NOT init

    let where_cmd = ws.run_br(["where"], "where_no_init");
    // Should fail or indicate no workspace
    assert!(
        !where_cmd.success || where_cmd.stderr.contains("not"),
        "where should indicate missing workspace"
    );

    ws.finish(true);
}

// =============================================================================
// Version Scenarios
// =============================================================================

#[test]
fn scenario_version_text() {
    let mut ws = TestWorkspace::new("e2e_workspace", "version_text");
    // Version doesn't require init

    let version = ws.run_br(["version"], "version");
    version.assert_success();

    // Should contain version info
    assert!(
        version.stdout.contains("br") || version.stdout.contains("version"),
        "version should show version info: {}",
        version.stdout
    );

    ws.finish(true);
}

#[test]
fn scenario_version_json() {
    let mut ws = TestWorkspace::new("e2e_workspace", "version_json");

    let version = ws.run_br(["version", "--json"], "version_json");
    version.assert_success();

    let payload = extract_json_payload(&version.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse version json");

    // Check expected fields
    assert!(
        json.get("version").is_some(),
        "version JSON should have 'version' field"
    );

    ws.finish(true);
}

#[test]
fn scenario_version_no_workspace_required() {
    let mut ws = TestWorkspace::new("e2e_workspace", "version_no_workspace");
    // Do NOT init - version should still work

    let version = ws.run_br(["version"], "version");
    version.assert_success();
    assert!(
        version.stdout.contains("br") || version.stdout.contains("version"),
        "version should work without a workspace and show version info: {}",
        version.stdout
    );

    ws.finish(true);
}

// =============================================================================
// Cross-command Scenarios
// =============================================================================

#[test]
fn scenario_workspace_lifecycle() {
    let mut ws = TestWorkspace::new("e2e_workspace", "lifecycle");

    // 1. Check version (no workspace needed)
    let version = ws.run_br(["version", "--json"], "version");
    version.assert_success();
    let version_json = parse_json_stdout(&version.stdout, "version");
    assert!(
        version_json["version"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "version JSON should contain a non-empty version: {version_json:?}"
    );

    // 2. Initialize workspace
    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // 3. Check workspace location
    let where_cmd = ws.run_br(["where"], "where");
    where_cmd.assert_success();
    assert!(
        where_cmd.stdout.contains(".beads"),
        "where should identify the beads directory: {}",
        where_cmd.stdout
    );

    // 4. Get workspace info
    let info = ws.run_br(["info", "--json"], "info");
    info.assert_success();
    let info_json = parse_json_stdout(&info.stdout, "info");
    assert!(
        info_json["beads_dir"]
            .as_str()
            .is_some_and(|path| path.contains(".beads")),
        "info JSON should include beads_dir: {info_json:?}"
    );
    assert_eq!(info_json["mode"].as_str(), Some("direct"));

    // 5. Check configuration
    let config = ws.run_br(["config", "list", "--json"], "config");
    config.assert_success();
    let config_json = parse_json_stdout(&config.stdout, "config list");
    assert!(
        config_json.is_object(),
        "config list JSON should be an object: {config_json:?}"
    );

    // 6. Run doctor
    let doctor = ws.run_br(["doctor", "--json"], "doctor");
    doctor.assert_success();
    let doctor_json = parse_json_stdout(&doctor.stdout, "doctor");
    assert_doctor_json_has_healthy_checks(&doctor_json);

    // 7. Re-init without --force should be rejected
    let reinit = ws.run_br(["init"], "reinit");
    reinit.assert_failure();
    assert!(
        reinit.stderr.to_lowercase().contains("already")
            || reinit.stderr.contains("ALREADY_INITIALIZED"),
        "re-init should report already initialized: stdout='{}' stderr='{}'",
        reinit.stdout,
        reinit.stderr
    );

    // 8. Doctor still passes
    let doctor2 = ws.run_br(["doctor"], "doctor_after_reinit");
    doctor2.assert_success();
    let doctor2_stdout = doctor2.stdout.to_ascii_lowercase();
    assert!(
        doctor2_stdout.contains("ok") || doctor2_stdout.contains("healthy"),
        "doctor should remain healthy after rejected re-init: {}",
        doctor2.stdout
    );

    ws.finish(true);
}

#[test]
#[allow(clippy::too_many_lines)]
fn scenario_long_lived_single_workspace_stress_suite() {
    let iterations = std::env::var("BR_LONG_STRESS_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(8);

    let materialized = catalog::long_lived_mixed_order_stress(41, iterations)
        .execute()
        .expect("long-lived stress plan should execute");

    let command_events: Vec<_> = materialized
        .events
        .iter()
        .filter(|event| event.kind == WorkspaceEvolutionEventKind::Command)
        .collect();
    assert!(
        command_events.len() >= iterations.saturating_mul(8),
        "stress suite should run a meaningful command volume: {} events for {iterations} iterations",
        command_events.len()
    );

    let expected_failures: Vec<_> = command_events
        .iter()
        .filter_map(|event| {
            event
                .command_result
                .as_ref()
                .filter(|result| !result.success)
                .map(|result| (*event, result))
        })
        .collect();
    assert!(
        !expected_failures.is_empty(),
        "stress suite should include expected intermittent failure probes"
    );
    for (event, result) in expected_failures {
        assert!(
            event.matched_expectation,
            "expected failure should still match its declared outcome: {}",
            event.label
        );
        assert!(
            result.log_path.exists(),
            "expected failure should leave a replay log at {}",
            result.log_path.display()
        );
    }

    let final_doctor = materialized
        .event("doctor_after_stress")
        .and_then(|event| event.command_result.as_ref())
        .expect("final doctor event");
    // Per the post-#292 doctor contract (commits 96c3fad2, 1c3c4fe1):
    // any non-OK check — WARN or ERROR — now flips top-level `ok` to
    // false and exits 1. The stress harness legitimately produces a
    // benign WARN finding (the test runner sets
    // `RUST_LOG=beads_rust=debug`, which trips `rust_log`). Since #378,
    // `sync --flush-only` also refreshes the merge anchor
    // `beads.base.jsonl` and `base_jsonl.missing_post_flush` no longer
    // warns for verifiably in-sync workspaces. None of
    // this does not degrade the workspace's semantic health, so we assert on
    // the JSON payload's `workspace_health`/`reliability_audit.health`
    // rather than the now-broader-than-necessary exit-code contract.
    let doctor_json = parse_json_stdout(&final_doctor.stdout, "doctor_after_stress");
    let workspace_health = doctor_json
        .get("workspace_health")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        workspace_health, "healthy",
        "stress workspace should finish workspace_health=healthy: \
         exit={} stdout={} stderr={}",
        final_doctor.exit_code, final_doctor.stdout, final_doctor.stderr
    );
    let audit_health = doctor_json
        .pointer("/reliability_audit/health")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        audit_health, "healthy",
        "stress workspace should finish reliability_audit.health=healthy: \
         exit={} stdout={} stderr={}",
        final_doctor.exit_code, final_doctor.stdout, final_doctor.stderr
    );
    let anomaly_count = doctor_json
        .pointer("/reliability_audit/anomaly_count")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    assert_eq!(
        anomaly_count, 0,
        "stress workspace should finish with zero reliability anomalies: \
         exit={} stdout={} stderr={}",
        final_doctor.exit_code, final_doctor.stdout, final_doctor.stderr
    );
    // Belt-and-suspenders: assert no `error`-level check leaks through
    // (WARN is acceptable for the benign findings catalogued above).
    let checks = doctor_json
        .get("checks")
        .and_then(Value::as_array)
        .expect("doctor JSON should contain checks array");
    assert!(
        checks
            .iter()
            .all(|check| check["status"].as_str() != Some("error")),
        "stress workspace doctor output should not contain any error-status checks: \
         exit={} stdout={} stderr={}",
        final_doctor.exit_code,
        final_doctor.stdout,
        final_doctor.stderr
    );

    let replay_target = TempDir::new().expect("replay target");
    materialized
        .materialize_into(replay_target.path())
        .expect("copy materialized stress workspace");
    assert!(
        replay_target
            .path()
            .join(".beads")
            .join("issues.jsonl")
            .exists(),
        "materialized stress workspace should retain the JSONL export"
    );
    assert!(
        replay_target.path().join("logs").exists(),
        "materialized stress workspace should retain command logs"
    );
}
