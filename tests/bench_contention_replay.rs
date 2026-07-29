//! Deterministic contention and replay lab for swarm-scale `br` workflows.
//!
//! The CI profile intentionally stays small, but it emits the same trace schema
//! as the manual 64+ worker profile. This gives future lock-combining,
//! scheduler, cache, and snapshot work a replayable proof surface before those
//! features exist.

#![allow(
    clippy::cast_possible_truncation,
    clippy::incompatible_msrv,
    clippy::missing_const_for_fn,
    clippy::similar_names,
    clippy::too_many_lines
)]

mod common;

use beads_rust::util::hex_encode;
use beads_rust::write_combining::{
    BatchLimits, CombinedOutputMode, CompatibleMutation, MutationEnvelope, plan_batch,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TRACE_SCHEMA_VERSION: &str = "br.contention-trace.v1";
const REPLAY_REPORT_SCHEMA_VERSION: &str = "br.contention-replay-report.v1";
const WRITE_COMBINING_PROJECTION_SCHEMA_VERSION: &str = "br.write-combining-projection.v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ContentionProfile {
    name: String,
    worker_count: usize,
    replay_seed: u64,
    lock_hold_ms: u64,
    lock_timeout_ms: u64,
}

impl ContentionProfile {
    fn ci_profile(replay_seed: u64) -> Self {
        Self {
            name: "ci_4_worker_contention".to_string(),
            worker_count: 4,
            replay_seed,
            lock_hold_ms: 350,
            lock_timeout_ms: 5_000,
        }
    }

    fn manual_64_worker_profile(replay_seed: u64) -> Self {
        Self {
            name: "manual_64_worker_contention".to_string(),
            worker_count: 64,
            replay_seed,
            lock_hold_ms: 750,
            lock_timeout_ms: 20_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PlannedCommand {
    worker_id: String,
    event_index: usize,
    command_kind: CommandKind,
    command: Vec<String>,
    scheduled_at_ms: u64,
    replay_seed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CommandKind {
    CreateIssue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ContentionTrace {
    schema_version: String,
    profile: ContentionProfile,
    replay_seed: u64,
    plan_hash: String,
    lock_released_at_ms: u64,
    events: Vec<ContentionEvent>,
    summary: ContentionSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ContentionEvent {
    worker_id: String,
    event_index: usize,
    command_kind: CommandKind,
    command: Vec<String>,
    scheduled_at_ms: u64,
    started_at_ms: u64,
    ended_at_ms: u64,
    lock_wait_ms: u64,
    auto_import_event: AutoImportEvent,
    auto_flush_event: AutoFlushEvent,
    exit_code: i32,
    stdout_hash: String,
    stderr_hash: String,
    replay_seed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AutoImportEvent {
    NotApplicableForMutation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AutoFlushEvent {
    AttemptedAfterSuccessfulMutation,
    SkippedAfterFailedMutation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ContentionSummary {
    total_workers: usize,
    successful_commands: usize,
    failed_commands: usize,
    max_latency_ms: u64,
    max_lock_wait_ms: u64,
    doctor_ok: bool,
    sync_status_clean: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ReplayReport {
    schema_version: String,
    trace_schema_version: String,
    replay_seed: u64,
    plan_hash: String,
    events_replayed: usize,
    divergence: Option<ReplayDivergence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ReplayDivergence {
    worker_id: String,
    event_index: usize,
    field: String,
    expected: String,
    actual: String,
    command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WriteCombiningProjectionReport {
    schema_version: String,
    trace_schema_version: String,
    replay_seed: u64,
    plan_hash: String,
    batch_limit: BatchLimits,
    direct_lock_acquisitions: usize,
    projected_lock_acquisitions: usize,
    saved_lock_acquisitions: usize,
    accepted_envelopes: usize,
    skipped_envelopes: usize,
    direct_total_lock_wait_ms: u64,
    direct_max_lock_wait_ms: u64,
    projected_single_batch_wait_ms: u64,
    used_argument_bytes: usize,
}

struct CommandRun {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct ContentionRun {
    trace: ContentionTrace,
    trace_path: PathBuf,
}

fn build_plan(profile: &ContentionProfile) -> Vec<PlannedCommand> {
    (0..profile.worker_count)
        .map(|event_index| {
            let worker_id = format!("worker-{event_index:03}");
            let title = format!(
                "contention replay seed {} worker {event_index:03}",
                profile.replay_seed
            );
            PlannedCommand {
                worker_id,
                event_index,
                command_kind: CommandKind::CreateIssue,
                command: vec![
                    "--lock-timeout".to_string(),
                    profile.lock_timeout_ms.to_string(),
                    "--json".to_string(),
                    "create".to_string(),
                    title,
                ],
                scheduled_at_ms: 0,
                replay_seed: profile.replay_seed,
            }
        })
        .collect()
}

fn run_contention_lab(profile: ContentionProfile) -> io::Result<ContentionRun> {
    let temp_dir = TempDir::new()?;
    let root = temp_dir.path().to_path_buf();
    initialize_workspace(&root)?;

    let plan = build_plan(&profile);
    let plan_hash = hash_json(&plan)?;

    let lock_path = root.join(".beads").join(".write.lock");
    let write_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    write_lock.lock()?;

    let trace_start = Instant::now();
    let start_barrier = Arc::new(Barrier::new(plan.len() + 1));
    let lock_released_at_ms = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(plan.len());

    for command in plan {
        let root = root.clone();
        let barrier = Arc::clone(&start_barrier);
        let release_marker = Arc::clone(&lock_released_at_ms);
        let handle = thread::spawn(move || {
            barrier.wait();
            run_planned_command(&root, command, trace_start, &release_marker)
        });
        handles.push(handle);
    }

    start_barrier.wait();
    thread::sleep(Duration::from_millis(profile.lock_hold_ms));
    let release_ms = elapsed_ms(trace_start);
    lock_released_at_ms.store(release_ms, Ordering::SeqCst);
    drop(write_lock);

    let mut events = Vec::with_capacity(handles.len());
    for handle in handles {
        events.push(
            handle
                .join()
                .map_err(|_| io::Error::other("contention worker panicked"))??,
        );
    }
    events.sort_by_key(|event| event.event_index);

    let doctor_run = run_br_command(&root, ["doctor", "--json"])?;
    let doctor_ok = doctor_reports_no_errors(&doctor_run);
    let sync_status = run_br_command(&root, ["sync", "--status", "--json"])?;
    let sync_status_clean = sync_status.exit_code == 0 && sync_status_is_clean(&sync_status.stdout);

    let summary = summarize_events(&events, doctor_ok, sync_status_clean);
    let trace = ContentionTrace {
        schema_version: TRACE_SCHEMA_VERSION.to_string(),
        profile,
        replay_seed: events.first().map_or(0, |event| event.replay_seed),
        plan_hash,
        lock_released_at_ms: release_ms,
        events,
        summary,
    };
    let trace_path = write_trace_artifact(&trace)?;

    Ok(ContentionRun { trace, trace_path })
}

fn synthetic_successful_contention_trace(
    profile: ContentionProfile,
) -> io::Result<ContentionTrace> {
    let plan = build_plan(&profile);
    let plan_hash = hash_json(&plan)?;
    let lock_released_at_ms = profile.lock_hold_ms;
    let events = plan
        .into_iter()
        .map(|planned| ContentionEvent {
            worker_id: planned.worker_id,
            event_index: planned.event_index,
            command_kind: planned.command_kind,
            command: planned.command,
            scheduled_at_ms: planned.scheduled_at_ms,
            started_at_ms: 0,
            ended_at_ms: lock_released_at_ms.saturating_add(1),
            lock_wait_ms: lock_released_at_ms,
            auto_import_event: AutoImportEvent::NotApplicableForMutation,
            auto_flush_event: AutoFlushEvent::AttemptedAfterSuccessfulMutation,
            exit_code: 0,
            stdout_hash: hash_bytes(b"synthetic-success"),
            stderr_hash: hash_bytes(b""),
            replay_seed: planned.replay_seed,
        })
        .collect::<Vec<_>>();
    let summary = summarize_events(&events, true, true);

    Ok(ContentionTrace {
        schema_version: TRACE_SCHEMA_VERSION.to_string(),
        profile,
        replay_seed: events.first().map_or(0, |event| event.replay_seed),
        plan_hash,
        lock_released_at_ms,
        events,
        summary,
    })
}

fn run_planned_command(
    root: &Path,
    planned: PlannedCommand,
    trace_start: Instant,
    lock_released_at_ms: &AtomicU64,
) -> io::Result<ContentionEvent> {
    let started_at_ms = elapsed_ms(trace_start);
    let run = run_br_command(root, planned.command.iter().map(String::as_str))?;
    let ended_at_ms = elapsed_ms(trace_start);
    let release_ms = lock_released_at_ms.load(Ordering::SeqCst);
    let lock_wait_ms = release_ms.saturating_sub(started_at_ms);

    Ok(ContentionEvent {
        worker_id: planned.worker_id,
        event_index: planned.event_index,
        command_kind: planned.command_kind,
        command: planned.command,
        scheduled_at_ms: planned.scheduled_at_ms,
        started_at_ms,
        ended_at_ms,
        lock_wait_ms,
        auto_import_event: AutoImportEvent::NotApplicableForMutation,
        auto_flush_event: if run.exit_code == 0 {
            AutoFlushEvent::AttemptedAfterSuccessfulMutation
        } else {
            AutoFlushEvent::SkippedAfterFailedMutation
        },
        exit_code: run.exit_code,
        stdout_hash: hash_bytes(&run.stdout),
        stderr_hash: hash_bytes(&run.stderr),
        replay_seed: planned.replay_seed,
    })
}

fn replay_contention_trace(trace: &ContentionTrace) -> io::Result<ReplayReport> {
    if trace.schema_version != TRACE_SCHEMA_VERSION {
        return Ok(ReplayReport {
            schema_version: REPLAY_REPORT_SCHEMA_VERSION.to_string(),
            trace_schema_version: trace.schema_version.clone(),
            replay_seed: trace.replay_seed,
            plan_hash: trace.plan_hash.clone(),
            events_replayed: 0,
            divergence: Some(ReplayDivergence {
                worker_id: String::new(),
                event_index: 0,
                field: "schema_version".to_string(),
                expected: TRACE_SCHEMA_VERSION.to_string(),
                actual: trace.schema_version.clone(),
                command: Vec::new(),
            }),
        });
    }

    let temp_dir = TempDir::new()?;
    let root = temp_dir.path().to_path_buf();
    initialize_workspace(&root)?;

    let expected_plan_hash = hash_json(&build_plan(&trace.profile))?;
    if expected_plan_hash != trace.plan_hash {
        return Ok(replay_report_with_divergence(
            trace,
            0,
            "plan_hash",
            &trace.plan_hash,
            &expected_plan_hash,
            Vec::new(),
        ));
    }

    let mut events_replayed = 0;
    for event in &trace.events {
        let run = run_br_command(&root, event.command.iter().map(String::as_str))?;
        events_replayed += 1;
        if run.exit_code != event.exit_code {
            return Ok(ReplayReport {
                schema_version: REPLAY_REPORT_SCHEMA_VERSION.to_string(),
                trace_schema_version: trace.schema_version.clone(),
                replay_seed: trace.replay_seed,
                plan_hash: trace.plan_hash.clone(),
                events_replayed,
                divergence: Some(ReplayDivergence {
                    worker_id: event.worker_id.clone(),
                    event_index: event.event_index,
                    field: "exit_code".to_string(),
                    expected: event.exit_code.to_string(),
                    actual: run.exit_code.to_string(),
                    command: event.command.clone(),
                }),
            });
        }
    }

    let list = run_br_command(&root, ["--no-auto-import", "list", "--json"])?;
    if list.exit_code != 0 {
        return Ok(replay_report_with_divergence(
            trace,
            events_replayed,
            "list_exit_code",
            "0",
            &list.exit_code.to_string(),
            vec![
                "--no-auto-import".to_string(),
                "list".to_string(),
                "--json".to_string(),
            ],
        ));
    }
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    for event in &trace.events {
        if event.exit_code == 0
            && let Some(title) = create_title_from_command(&event.command)
            && !list_stdout.contains(title)
        {
            return Ok(ReplayReport {
                schema_version: REPLAY_REPORT_SCHEMA_VERSION.to_string(),
                trace_schema_version: trace.schema_version.clone(),
                replay_seed: trace.replay_seed,
                plan_hash: trace.plan_hash.clone(),
                events_replayed,
                divergence: Some(ReplayDivergence {
                    worker_id: event.worker_id.clone(),
                    event_index: event.event_index,
                    field: "created_issue_title".to_string(),
                    expected: title.to_string(),
                    actual: "missing from replayed list output".to_string(),
                    command: event.command.clone(),
                }),
            });
        }
    }

    Ok(ReplayReport {
        schema_version: REPLAY_REPORT_SCHEMA_VERSION.to_string(),
        trace_schema_version: trace.schema_version.clone(),
        replay_seed: trace.replay_seed,
        plan_hash: trace.plan_hash.clone(),
        events_replayed,
        divergence: None,
    })
}

fn project_write_combining_from_trace(
    trace: &ContentionTrace,
    batch_limit: BatchLimits,
) -> io::Result<WriteCombiningProjectionReport> {
    let envelopes = trace
        .events
        .iter()
        .filter_map(mutation_envelope_from_event)
        .collect::<Vec<_>>();
    let plan = plan_batch(&envelopes, batch_limit, trace.lock_released_at_ms)
        .map_err(|err| io::Error::other(format!("invalid batch limits: {err:?}")))?;
    let projected_lock_acquisitions = usize::from(plan.accepted_count() > 0);
    let direct_lock_acquisitions = envelopes.len();
    let direct_total_lock_wait_ms = trace
        .events
        .iter()
        .map(|event| event.lock_wait_ms)
        .sum::<u64>();

    Ok(WriteCombiningProjectionReport {
        schema_version: WRITE_COMBINING_PROJECTION_SCHEMA_VERSION.to_string(),
        trace_schema_version: trace.schema_version.clone(),
        replay_seed: trace.replay_seed,
        plan_hash: trace.plan_hash.clone(),
        batch_limit,
        direct_lock_acquisitions,
        projected_lock_acquisitions,
        saved_lock_acquisitions: direct_lock_acquisitions
            .saturating_sub(projected_lock_acquisitions),
        accepted_envelopes: plan.accepted_count(),
        skipped_envelopes: plan.skipped.len(),
        direct_total_lock_wait_ms,
        direct_max_lock_wait_ms: trace.summary.max_lock_wait_ms,
        projected_single_batch_wait_ms: trace.summary.max_lock_wait_ms,
        used_argument_bytes: plan.used_argument_bytes,
    })
}

fn mutation_envelope_from_event(event: &ContentionEvent) -> Option<MutationEnvelope> {
    if event.command_kind != CommandKind::CreateIssue || event.exit_code != 0 {
        return None;
    }
    let title = create_title_from_command(&event.command)?;
    Some(MutationEnvelope::new(
        format!("{}:{}", event.worker_id, event.event_index),
        event.worker_id.clone(),
        CompatibleMutation::CreateIssue,
        CombinedOutputMode::Json,
        event
            .started_at_ms
            .saturating_add(event.lock_wait_ms)
            .saturating_add(1),
        json!({ "title": title }),
    ))
}

fn replay_report_with_divergence(
    trace: &ContentionTrace,
    events_replayed: usize,
    field: &str,
    expected: &str,
    actual: &str,
    command: Vec<String>,
) -> ReplayReport {
    ReplayReport {
        schema_version: REPLAY_REPORT_SCHEMA_VERSION.to_string(),
        trace_schema_version: trace.schema_version.clone(),
        replay_seed: trace.replay_seed,
        plan_hash: trace.plan_hash.clone(),
        events_replayed,
        divergence: Some(ReplayDivergence {
            worker_id: trace
                .events
                .get(events_replayed)
                .map_or_else(String::new, |event| event.worker_id.clone()),
            event_index: events_replayed,
            field: field.to_string(),
            expected: expected.to_string(),
            actual: actual.to_string(),
            command,
        }),
    }
}

fn initialize_workspace(root: &Path) -> io::Result<()> {
    let init = run_br_command(root, ["init"])?;
    if init.exit_code == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "br init failed: stdout={} stderr={}",
            String::from_utf8_lossy(&init.stdout),
            String::from_utf8_lossy(&init.stderr)
        )))
    }
}

fn run_br_command<I, S>(root: &Path, args: I) -> io::Result<CommandRun>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command =
        assert_cmd::Command::cargo_bin("br").map_err(|err| io::Error::other(err.to_string()))?;
    command.current_dir(root);
    command.args(args);
    clear_inherited_br_env(&mut command);
    command.env("NO_COLOR", "1");
    command.env("RUST_BACKTRACE", "1");
    command.env("HOME", root);

    let output = command.output()?;

    Ok(CommandRun {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn clear_inherited_br_env(command: &mut assert_cmd::Command) {
    for (key, _) in std::env::vars_os() {
        if should_clear_inherited_br_env(&key) {
            command.env_remove(key);
        }
    }
}

fn should_clear_inherited_br_env(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    key.starts_with("BD_")
        || key.starts_with("BEADS_")
        || matches!(
            key.as_ref(),
            "BR_OUTPUT_FORMAT" | "TOON_DEFAULT_FORMAT" | "TOON_STATS"
        )
}

fn summarize_events(
    events: &[ContentionEvent],
    doctor_ok: bool,
    sync_status_clean: bool,
) -> ContentionSummary {
    let successful_commands = events.iter().filter(|event| event.exit_code == 0).count();
    ContentionSummary {
        total_workers: events.len(),
        successful_commands,
        failed_commands: events.len().saturating_sub(successful_commands),
        max_latency_ms: events
            .iter()
            .map(|event| event.ended_at_ms.saturating_sub(event.started_at_ms))
            .max()
            .unwrap_or(0),
        max_lock_wait_ms: events
            .iter()
            .map(|event| event.lock_wait_ms)
            .max()
            .unwrap_or(0),
        doctor_ok,
        sync_status_clean,
    }
}

fn sync_status_is_clean(stdout: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(stdout) else {
        return false;
    };
    value
        .get("dirty_count")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|dirty_count| dirty_count == 0)
        && value
            .get("jsonl_newer")
            .and_then(serde_json::Value::as_bool)
            .is_some_and(|jsonl_newer| !jsonl_newer)
        && value
            .get("db_newer")
            .and_then(serde_json::Value::as_bool)
            .is_some_and(|db_newer| !db_newer)
}

/// Decide whether `br doctor --json` reports a workspace that is healthy
/// *enough* for the contention-replay assertion.
///
/// `br doctor` intentionally exits `1` (`DoctorExitCode::FindingsPresent`)
/// whenever **any** finding is present, including benign `warn`-level ones
/// that are purely operator-environment conditions (duplicate `br` on
/// `$PATH`, unset `RUST_LOG`, expected frankensqlite WAL/SHM sidecar shape,
/// missing post-flush anchor, etc.). The exit-code contract is correct and
/// must not change — but the assertion here only cares that forced write
/// contention left no *error*-level integrity/sync damage. So we parse the
/// structured report and treat the run as ok unless a check is `error`.
///
/// Falls back to the raw exit code if the JSON cannot be parsed, so a hard
/// failure (panic, non-doctor error) still surfaces.
fn doctor_reports_no_errors(run: &CommandRun) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&run.stdout) else {
        return run.exit_code == 0;
    };
    let Some(checks) = value.get("checks").and_then(serde_json::Value::as_array) else {
        return run.exit_code == 0;
    };
    checks
        .iter()
        .all(|check| check.get("status").and_then(serde_json::Value::as_str) != Some("error"))
}

fn create_title_from_command(command: &[String]) -> Option<&str> {
    command
        .iter()
        .position(|arg| arg == "create")
        .and_then(|index| command.get(index + 1))
        .map(String::as_str)
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn hash_json<T: Serialize>(value: &T) -> io::Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|err| io::Error::other(err.to_string()))?;
    Ok(hash_bytes(&bytes))
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

fn write_trace_artifact(trace: &ContentionTrace) -> io::Result<PathBuf> {
    let artifact_dir = contention_artifact_dir(&trace.profile);
    fs::create_dir_all(&artifact_dir)?;
    let trace_path = artifact_dir.join("contention-trace.json");
    write_json_pretty(&trace_path, trace)?;
    Ok(trace_path)
}

fn write_replay_report_artifact(
    profile: &ContentionProfile,
    report: &ReplayReport,
) -> io::Result<PathBuf> {
    let artifact_dir = contention_artifact_dir(profile);
    fs::create_dir_all(&artifact_dir)?;
    let report_path = artifact_dir.join("contention-replay-report.json");
    write_json_pretty(&report_path, report)?;
    Ok(report_path)
}

fn write_write_combining_projection_artifact(
    profile: &ContentionProfile,
    report: &WriteCombiningProjectionReport,
) -> io::Result<PathBuf> {
    let artifact_dir = contention_artifact_dir(profile);
    fs::create_dir_all(&artifact_dir)?;
    let report_path = artifact_dir.join("write-combining-projection-report.json");
    write_json_pretty(&report_path, report)?;
    Ok(report_path)
}

fn contention_artifact_dir(profile: &ContentionProfile) -> PathBuf {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"));
    target_dir
        .join("test-artifacts")
        .join("contention-replay")
        .join(format!("{}-seed-{}", profile.name, profile.replay_seed))
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let file = File::create(path)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer_pretty(writer, value)?;
    Ok(())
}

#[test]
fn contention_profiles_share_trace_schema_and_seeded_plan() {
    let seed = 72_512;
    let ci = ContentionProfile::ci_profile(seed);
    let manual = ContentionProfile::manual_64_worker_profile(seed);

    assert!(manual.worker_count >= 64);
    assert_eq!(TRACE_SCHEMA_VERSION, "br.contention-trace.v1");
    assert_eq!(build_plan(&ci), build_plan(&ci));
    assert_eq!(build_plan(&manual), build_plan(&manual));
    assert_eq!(build_plan(&ci)[0].replay_seed, seed);
    assert_eq!(build_plan(&manual)[0].replay_seed, seed);
    assert_eq!(
        build_plan(&ci)[0].command_kind,
        build_plan(&manual)[0].command_kind
    );
}

#[test]
fn manual_64_worker_projection_fits_one_default_batch() -> io::Result<()> {
    let trace =
        synthetic_successful_contention_trace(ContentionProfile::manual_64_worker_profile(64_064))?;
    let projection = project_write_combining_from_trace(&trace, BatchLimits::default())?;

    assert_eq!(trace.schema_version, TRACE_SCHEMA_VERSION);
    assert_eq!(trace.summary.total_workers, 64);
    assert_eq!(trace.summary.successful_commands, 64);
    assert_eq!(trace.summary.failed_commands, 0);
    assert_eq!(
        projection.schema_version,
        WRITE_COMBINING_PROJECTION_SCHEMA_VERSION
    );
    assert_eq!(projection.batch_limit.max_envelopes, 64);
    assert_eq!(projection.direct_lock_acquisitions, 64);
    assert_eq!(projection.projected_lock_acquisitions, 1);
    assert_eq!(projection.saved_lock_acquisitions, 63);
    assert_eq!(projection.accepted_envelopes, 64);
    assert_eq!(projection.skipped_envelopes, 0);
    assert!(
        projection.used_argument_bytes <= projection.batch_limit.max_argument_bytes,
        "64 projected create envelopes should stay within the default argument budget"
    );
    assert!(
        projection.direct_total_lock_wait_ms > projection.projected_single_batch_wait_ms,
        "64-agent projection should reduce direct lock-wait exposure"
    );
    let projection_path = write_write_combining_projection_artifact(&trace.profile, &projection)?;
    assert!(
        projection_path.is_file(),
        "64-worker projection artifact should exist"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
#[allow(clippy::incompatible_msrv)]
fn contention_ci_profile_records_and_replays_trace() {
    let _log = common::test_log("contention_ci_profile_records_and_replays_trace");
    let run = run_contention_lab(ContentionProfile::ci_profile(72_512))
        .expect("contention lab should record a CI trace");

    assert_eq!(run.trace.schema_version, TRACE_SCHEMA_VERSION);
    assert_eq!(run.trace.summary.total_workers, 4);
    assert_eq!(run.trace.summary.failed_commands, 0);
    assert!(run.trace.summary.doctor_ok);
    assert!(run.trace.summary.sync_status_clean);
    assert!(
        run.trace.summary.max_lock_wait_ms > 0,
        "trace should record forced write-lock contention"
    );
    assert!(run.trace_path.is_file(), "trace artifact should be written");

    for event in &run.trace.events {
        assert!(!event.worker_id.is_empty());
        assert!(!event.command.is_empty());
        assert!(event.ended_at_ms >= event.started_at_ms);
        assert_eq!(
            event.auto_import_event,
            AutoImportEvent::NotApplicableForMutation
        );
        assert_eq!(
            event.auto_flush_event,
            AutoFlushEvent::AttemptedAfterSuccessfulMutation
        );
        assert_eq!(event.exit_code, 0);
        assert_eq!(event.replay_seed, run.trace.replay_seed);
    }

    let replay = replay_contention_trace(&run.trace).expect("trace should replay");
    assert_eq!(replay.events_replayed, run.trace.events.len());
    assert_eq!(replay.divergence, None);
    let report_path = write_replay_report_artifact(&run.trace.profile, &replay)
        .expect("replay report artifact should write");
    assert!(report_path.is_file(), "replay report artifact should exist");

    let projection = project_write_combining_from_trace(&run.trace, BatchLimits::default())
        .expect("write-combining projection should build from contention trace");
    assert_eq!(
        projection.schema_version,
        WRITE_COMBINING_PROJECTION_SCHEMA_VERSION
    );
    assert_eq!(projection.direct_lock_acquisitions, 4);
    assert_eq!(projection.projected_lock_acquisitions, 1);
    assert_eq!(projection.saved_lock_acquisitions, 3);
    assert_eq!(projection.accepted_envelopes, 4);
    assert_eq!(projection.skipped_envelopes, 0);
    assert!(
        projection.direct_total_lock_wait_ms > projection.projected_single_batch_wait_ms,
        "projection should show lower lock-wait exposure than one lock per worker"
    );
    let projection_path =
        write_write_combining_projection_artifact(&run.trace.profile, &projection)
            .expect("write-combining projection artifact should write");
    assert!(
        projection_path.is_file(),
        "write-combining projection artifact should exist"
    );
}

#[test]
#[cfg(unix)]
#[allow(clippy::incompatible_msrv)]
fn replay_report_identifies_first_divergent_worker_event() {
    let _log = common::test_log("replay_report_identifies_first_divergent_worker_event");
    let run = run_contention_lab(ContentionProfile::ci_profile(83_001))
        .expect("contention lab should record a CI trace");
    let mut divergent_trace = run.trace.clone();
    assert!(
        !divergent_trace.events.is_empty(),
        "contention trace should include at least one event"
    );
    let (expected_worker_id, expected_event_index) =
        if let Some(first_event) = divergent_trace.events.first_mut() {
            first_event.exit_code = 99;
            (first_event.worker_id.clone(), first_event.event_index)
        } else {
            return;
        };

    let replay = replay_contention_trace(&divergent_trace).expect("divergent trace should replay");
    let divergence = replay
        .divergence
        .expect("replay should report a divergence");

    assert_eq!(divergence.worker_id, expected_worker_id);
    assert_eq!(divergence.event_index, expected_event_index);
    assert_eq!(divergence.field, "exit_code");
    assert_eq!(divergence.expected, "99");
    assert_eq!(divergence.actual, "0");
}

#[test]
#[ignore = "manual 64-worker profile; run with BR_CONTENTION_64=1 on 64+ core hosts"]
#[cfg(unix)]
#[allow(clippy::incompatible_msrv)]
fn manual_64_worker_contention_profile_records_replayable_trace() {
    if std::env::var_os("BR_CONTENTION_64").is_none() {
        eprintln!("Skipping manual 64-worker run; set BR_CONTENTION_64=1 to execute it.");
        return;
    }

    let run = run_contention_lab(ContentionProfile::manual_64_worker_profile(64_064))
        .expect("manual 64-worker contention lab should record a trace");
    assert_eq!(run.trace.summary.total_workers, 64);
    assert!(run.trace.summary.doctor_ok);
    assert!(run.trace.summary.sync_status_clean);
    assert_eq!(
        replay_contention_trace(&run.trace)
            .expect("manual trace should replay")
            .divergence,
        None
    );
}
