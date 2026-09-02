#![allow(clippy::module_name_repetitions, clippy::trivial_regex, dead_code)]

#[path = "../common/mod.rs"]
mod common;

use common::cli::{BrWorkspace, run_br};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::{self, Write};
use std::sync::LazyLock;

pub fn init_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "bd"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    workspace
}

pub fn create_issue(workspace: &BrWorkspace, title: &str, label: &str) -> String {
    let output = run_br(workspace, ["create", title], label);
    assert!(output.status.success(), "create failed: {}", output.stderr);
    parse_created_id(&output.stdout)
}

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    // Handle both formats: "Created bd-xxx: title" and "✓ Created bd-xxx: title"
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    let id_part = normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

// ============================================================================
// Golden Text Snapshot System (beads_rust-hdc0)
// ============================================================================
//
// Provides deterministic text output capture and comparison for CLI commands.
// Normalizes platform-specific differences (colors, paths, line endings) to
// enable cross-platform snapshot testing.

// Pre-compiled regex patterns for performance
static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*m").expect("ansi regex"));
static ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[a-zA-Z0-9_-]+-[a-z0-9]{3,}\b").expect("id regex"));
static TS_FULL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?")
        .expect("full timestamp regex")
});
static DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{4}-\d{2}-\d{2}").expect("date regex"));
static VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s+\((?:HEAD|[A-Za-z0-9._/-]+)@[a-f0-9]+\)").expect("version regex")
});
/// The build profile label embedded in `br --version` output, e.g., `(dev)`
/// or `(release)`.  Snapshot tests may run under either profile depending on
/// `cargo test` vs `cargo test --release`, so mask to a stable placeholder.
static BUILD_PROFILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\((dev|release)\)").expect("build profile regex"));
static OWNER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Owner: [a-zA-Z0-9_-]+").expect("owner regex"));
/// Version numbers in human-readable output.
///
/// Covers both the `version 0.1.7` form and the bare `br 0.2.19` form that
/// `br doctor`'s `binary_version` check prints. Without the latter the doctor
/// snapshot carries the crate version verbatim and breaks on every release.
static VERSION_NUM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(version|br) \d+\.\d+\.\d+(?:-[A-Za-z0-9.]+)?").expect("version number regex")
});
/// The `ENGINE ...` summary line `br doctor` prints: engine version, the
/// sidecars present, and the opener-lease state all depend on the engine
/// build and the host, so the whole line is masked.
static ENGINE_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^ENGINE .*$").expect("engine line regex"));
static LINE_NUM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.rs:\d+:").expect("line number regex"));
/// `tracing` source-location annotation that appears in dev builds after the
/// target-and-colon, e.g., `beads_rust::sync::path: src/sync/path.rs:123:`.
/// Release builds omit it.  Normalize by deleting the segment entirely so the
/// dev-vs-release formatter delta does not cause snapshot drift.
static TRACING_SRC_LOC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r": src/[^\s]+\.rs:(\d+|LINE):").expect("tracing src loc regex"));
static PATH_SEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\").expect("path separator regex"));
static TRAILING_WS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[ \t]+$").expect("trailing whitespace regex"));
static MULTIPLE_BLANK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("multiple blank lines regex"));
static HOME_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/home/[a-zA-Z0-9_-]+").expect("home path regex"));
static USERS_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/Users/[a-zA-Z0-9_-]+").expect("users path regex"));
static TMP_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:/data)?/tmp(?:/[A-Za-z0-9._-]+)*/\.tmp[a-zA-Z0-9]+|(?:/[A-Za-z0-9._-]+)+/\.rch-target-[A-Za-z0-9._-]+/\.tmp[a-zA-Z0-9]+|/var/folders/[a-zA-Z0-9/_-]+",
    )
    .expect("tmp path regex")
});
/// Compact timestamp format used in backup filenames: `YYYYMMDD_HHMMSS_nano`.
/// Produced by `sync::history` when writing rotation backups; differs run-to-run
/// so snapshot tests mask it to a stable placeholder.
static TS_COMPACT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{8}_\d{6}_\d+").expect("compact timestamp regex"));
/// PID-suffixed temp file segments (e.g., `issues.jsonl.3676561.tmp`).  The
/// PID varies run-to-run; mask it so snapshot output is stable.
static TMP_PID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\.jsonl|\.db)\.\d+\.tmp").expect("pid tmp file regex"));
static DURATION_MS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+(\.\d+)?\s*(ms|µs|ns|s)").expect("duration regex"));
/// `br doctor` checks whose result is determined by the machine the test runs
/// on rather than by anything br did (`beads_rust-alur`). Both made the doctor
/// golden pass on a developer box and fail on an rch remote worker:
///
/// - `sqlite3.integrity_check` shells out to the system `sqlite3` binary and
///   degrades to a WARN when it is absent. Workers have no `sqlite3`.
/// - `binary_version` reports either "matches (or is ahead of) Cargo.toml at
///   <path> (<version>)" or "no beads_rust Cargo.toml reachable from .beads/ —
///   not flagging", depending on whether a `beads_rust` manifest happens to sit
///   above the temp workspace. rch sets `TMPDIR` *inside* the synced repo, so
///   the worker takes the first branch and a developer box the second. It also
///   leaks a bare version number that `VERSION_NUM_RE` does not catch, since
///   that pattern requires a `version `/`br ` prefix.
///
/// Masking the whole result line — rather than skipping the test — keeps the
/// other ~50 doctor checks under exact comparison everywhere. The check name is
/// preserved, so the snapshot still fails if a check disappears or is renamed.
static HOST_DEPENDENT_CHECK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^(\s*)(?:OK|WARN|ERROR)\s+(sqlite3\.integrity_check|binary_version)\b.*$")
        .expect("host-dependent check regex")
});

/// Configuration for text normalization.
///
/// Controls which normalization rules are applied during snapshot comparison.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default)]
pub struct TextNormConfig {
    /// Strip ANSI color/formatting escape sequences
    pub strip_ansi: bool,
    /// Redact issue IDs (e.g., bd-abc → ID-REDACTED)
    pub redact_ids: bool,
    /// Mask timestamps with placeholders
    pub mask_timestamps: bool,
    /// Mask dates with placeholders
    pub mask_dates: bool,
    /// Mask git hashes in version strings
    pub mask_git_hashes: bool,
    /// Normalize line numbers in stack traces/logs
    pub normalize_line_numbers: bool,
    /// Normalize path separators (backslash → forward slash)
    pub normalize_paths: bool,
    /// Normalize line endings (CRLF → LF)
    pub normalize_line_endings: bool,
    /// Strip trailing whitespace from lines
    pub strip_trailing_whitespace: bool,
    /// Collapse multiple blank lines to single
    pub collapse_blank_lines: bool,
    /// Mask home directory paths (/home/user → /HOME)
    pub mask_home_paths: bool,
    /// Mask temp directory paths
    pub mask_temp_paths: bool,
    /// Mask duration values (for timing-sensitive output)
    pub mask_durations: bool,
    /// Mask owner/username in output (e.g., "Owner: user" → "Owner: USERNAME")
    pub mask_usernames: bool,
    /// Mask version numbers (e.g., "version 0.1.7" → "version X.Y.Z")
    pub mask_version_numbers: bool,
    /// Mask check results whose status depends on optional tooling being
    /// installed on the host rather than on anything br did
    /// (`beads_rust-alur`).
    pub mask_host_tooling: bool,
}

impl TextNormConfig {
    /// Standard configuration for golden text snapshots.
    ///
    /// Applies all normalizations needed for deterministic cross-platform output.
    pub const fn golden() -> Self {
        Self {
            strip_ansi: true,
            redact_ids: true,
            mask_timestamps: true,
            mask_dates: true,
            mask_git_hashes: true,
            normalize_line_numbers: true,
            normalize_paths: true,
            normalize_line_endings: true,
            strip_trailing_whitespace: true,
            collapse_blank_lines: true,
            mask_home_paths: true,
            mask_temp_paths: true,
            mask_durations: false, // Keep durations by default
            mask_usernames: true,
            mask_version_numbers: true,
            mask_host_tooling: true,
        }
    }

    /// Minimal configuration that preserves most output.
    ///
    /// Only normalizes platform-critical differences.
    pub fn minimal() -> Self {
        Self {
            strip_ansi: true,
            normalize_line_endings: true,
            normalize_paths: true,
            ..Default::default()
        }
    }

    /// Configuration for timing-sensitive snapshots.
    ///
    /// Masks durations in addition to standard normalization.
    pub const fn with_duration_masking() -> Self {
        Self {
            mask_durations: true,
            ..Self::golden()
        }
    }
}

/// A captured text snapshot with normalization metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextSnapshot {
    /// The raw, unnormalized output
    pub raw: String,
    /// The normalized output for comparison
    pub normalized: String,
    /// What normalizations were applied
    pub normalizations_applied: Vec<String>,
    /// Configuration used for normalization
    #[serde(skip)]
    config: TextNormConfig,
}

impl TextSnapshot {
    /// Create a new text snapshot with the given configuration.
    pub fn new(raw: impl Into<String>, config: TextNormConfig) -> Self {
        let raw = raw.into();
        let (normalized, normalizations) = normalize_text_with_log(&raw, &config);
        Self {
            raw,
            normalized,
            normalizations_applied: normalizations,
            config,
        }
    }

    /// Create a golden text snapshot (standard normalization).
    pub fn golden(raw: impl Into<String>) -> Self {
        Self::new(raw, TextNormConfig::golden())
    }

    /// Create a minimal snapshot (preserves most output).
    pub fn minimal(raw: impl Into<String>) -> Self {
        Self::new(raw, TextNormConfig::minimal())
    }

    /// Get the normalized output for snapshot comparison.
    pub fn as_normalized(&self) -> &str {
        &self.normalized
    }

    /// Get the raw output.
    pub fn as_raw(&self) -> &str {
        &self.raw
    }

    /// Check if any normalizations were applied.
    pub fn was_normalized(&self) -> bool {
        !self.normalizations_applied.is_empty()
    }

    /// Serialize to JSON for artifact logging.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "raw_length": self.raw.len(),
            "normalized_length": self.normalized.len(),
            "normalizations_applied": self.normalizations_applied,
            "was_normalized": self.was_normalized(),
        })
    }
}

impl fmt::Display for TextSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.normalized)
    }
}

/// Result of comparing two text snapshots.
#[derive(Debug, Clone)]
pub struct TextDiff {
    /// Whether the snapshots match after normalization
    pub matches: bool,
    /// Lines only in the expected output
    pub missing_lines: Vec<String>,
    /// Lines only in the actual output
    pub extra_lines: Vec<String>,
    /// Lines that differ (expected, actual)
    pub different_lines: Vec<(String, String)>,
    /// Summary of the comparison
    pub summary: String,
}

impl TextDiff {
    /// Compare two text snapshots and produce a diff.
    pub fn compare(expected: &TextSnapshot, actual: &TextSnapshot) -> Self {
        let expected_lines: Vec<&str> = expected.normalized.lines().collect();
        let actual_lines: Vec<&str> = actual.normalized.lines().collect();

        let mut missing = Vec::new();
        let mut extra = Vec::new();
        let mut different = Vec::new();

        let max_len = expected_lines.len().max(actual_lines.len());

        for i in 0..max_len {
            match (expected_lines.get(i), actual_lines.get(i)) {
                (Some(exp), Some(act)) if exp != act => {
                    different.push(((*exp).to_string(), (*act).to_string()));
                }
                (Some(exp), None) => {
                    missing.push((*exp).to_string());
                }
                (None, Some(act)) => {
                    extra.push((*act).to_string());
                }
                _ => {}
            }
        }

        let matches = missing.is_empty() && extra.is_empty() && different.is_empty();

        let summary = if matches {
            "Snapshots match".to_string()
        } else {
            format!(
                "{} missing, {} extra, {} different lines",
                missing.len(),
                extra.len(),
                different.len()
            )
        };

        Self {
            matches,
            missing_lines: missing,
            extra_lines: extra,
            different_lines: different,
            summary,
        }
    }

    /// Format the diff for display.
    pub fn format_diff(&self) -> String {
        if self.matches {
            return "✓ Snapshots match\n".to_string();
        }

        let mut output = String::new();
        let _ = write!(output, "✗ {}\n\n", self.summary);

        if !self.missing_lines.is_empty() {
            output.push_str("Missing lines (expected but not found):\n");
            for line in &self.missing_lines {
                let _ = writeln!(output, "  - {line}");
            }
            output.push('\n');
        }

        if !self.extra_lines.is_empty() {
            output.push_str("Extra lines (found but not expected):\n");
            for line in &self.extra_lines {
                let _ = writeln!(output, "  + {line}");
            }
            output.push('\n');
        }

        if !self.different_lines.is_empty() {
            output.push_str("Different lines:\n");
            for (exp, act) in &self.different_lines {
                let _ = writeln!(output, "  expected: {exp}");
                let _ = writeln!(output, "  actual:   {act}");
                output.push('\n');
            }
        }

        output
    }

    /// Serialize to JSON for artifact logging.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "matches": self.matches,
            "summary": self.summary,
            "missing_count": self.missing_lines.len(),
            "extra_count": self.extra_lines.len(),
            "different_count": self.different_lines.len(),
        })
    }
}

/// Apply normalization with logging of what was changed.
#[allow(clippy::too_many_lines)]
fn normalize_text_with_log(text: &str, config: &TextNormConfig) -> (String, Vec<String>) {
    const VCS_STATUS_SENTINEL: &str = "BR_VCS_STATUS_COMMAND_SENTINEL";

    let mut normalized = text.to_string();
    let mut log = Vec::new();

    // 1. Normalize line endings first (CRLF → LF)
    if config.normalize_line_endings && normalized.contains("\r\n") {
        normalized = normalized.replace("\r\n", "\n");
        log.push("line_endings".to_string());
    }

    // 2. Strip ANSI escape sequences
    if config.strip_ansi && ANSI_RE.is_match(&normalized) {
        normalized = ANSI_RE.replace_all(&normalized, "").to_string();
        log.push("ansi_codes".to_string());
    }

    // 3. Normalize path separators (Windows → Unix)
    if config.normalize_paths && normalized.contains('\\') {
        normalized = PATH_SEP_RE.replace_all(&normalized, "/").to_string();
        log.push("path_separators".to_string());
    }

    // 4. Mask home directory paths
    if config.mask_home_paths {
        if HOME_PATH_RE.is_match(&normalized) {
            normalized = HOME_PATH_RE.replace_all(&normalized, "/HOME").to_string();
            log.push("home_paths".to_string());
        }
        if USERS_PATH_RE.is_match(&normalized) {
            normalized = USERS_PATH_RE.replace_all(&normalized, "/HOME").to_string();
            log.push("users_paths".to_string());
        }
    }

    // 5. Mask temp directory paths
    if config.mask_temp_paths && TMP_PATH_RE.is_match(&normalized) {
        normalized = TMP_PATH_RE.replace_all(&normalized, "/TMP").to_string();
        log.push("temp_paths".to_string());
    }

    // 6. Redact issue IDs
    if config.redact_ids && ID_RE.is_match(&normalized) {
        // `vcs-status` has the same lexical shape as a configurable issue ID,
        // but it is a stable command name and must remain visible in CLI help
        // snapshots. Protect it across the deliberately broad legacy ID
        // redactor rather than weakening ID coverage for letter-only IDs.
        normalized = normalized.replace("vcs-status", VCS_STATUS_SENTINEL);
        normalized = ID_RE.replace_all(&normalized, "ID-REDACTED").to_string();
        normalized = normalized.replace(VCS_STATUS_SENTINEL, "vcs-status");
        log.push("issue_ids".to_string());
    }

    // 7. Mask full timestamps
    if config.mask_timestamps && TS_FULL_RE.is_match(&normalized) {
        normalized = TS_FULL_RE
            .replace_all(&normalized, "YYYY-MM-DDTHH:MM:SS")
            .to_string();
        log.push("timestamps".to_string());
    }
    // 7b. Mask compact backup timestamps (YYYYMMDD_HHMMSS_nano) and PID
    // suffixes on temp-file intermediates.  These are run-to-run variable
    // strings that the golden snapshots need to treat as stable.
    if config.mask_timestamps && TS_COMPACT_RE.is_match(&normalized) {
        normalized = TS_COMPACT_RE
            .replace_all(&normalized, "YYYYMMDD_HHMMSS_NANO")
            .to_string();
        log.push("compact_timestamps".to_string());
    }
    if config.mask_temp_paths && TMP_PID_RE.is_match(&normalized) {
        normalized = TMP_PID_RE
            .replace_all(&normalized, "$1.PID.tmp")
            .to_string();
        log.push("tmp_pid".to_string());
    }

    // 8. Mask dates (after timestamps to avoid double-masking)
    if config.mask_dates && DATE_RE.is_match(&normalized) {
        normalized = DATE_RE.replace_all(&normalized, "YYYY-MM-DD").to_string();
        log.push("dates".to_string());
    }

    // 9. Mask git hashes and build-profile labels (dev / release)
    if config.mask_git_hashes && VERSION_RE.is_match(&normalized) {
        normalized = VERSION_RE.replace_all(&normalized, "").to_string();
        log.push("git_hashes".to_string());
    }
    if config.mask_git_hashes && BUILD_PROFILE_RE.is_match(&normalized) {
        normalized = BUILD_PROFILE_RE
            .replace_all(&normalized, "(BUILD)")
            .to_string();
        log.push("build_profile".to_string());
    }

    // 10. Normalize line numbers.  Drop the tracing source-location segment
    // first (so we stay dev/release invariant) and then normalize any
    // remaining `file.rs:123:` references to `file.rs:LINE:`.
    if config.normalize_line_numbers && TRACING_SRC_LOC_RE.is_match(&normalized) {
        normalized = TRACING_SRC_LOC_RE.replace_all(&normalized, ":").to_string();
        log.push("tracing_src_loc".to_string());
    }
    if config.normalize_line_numbers && LINE_NUM_RE.is_match(&normalized) {
        normalized = LINE_NUM_RE
            .replace_all(&normalized, ".rs:LINE:")
            .to_string();
        log.push("line_numbers".to_string());
    }

    // 11. Mask durations
    if config.mask_durations && DURATION_MS_RE.is_match(&normalized) {
        normalized = DURATION_MS_RE
            .replace_all(&normalized, "DURATION")
            .to_string();
        log.push("durations".to_string());
    }

    // 12. Mask owner/usernames
    if config.mask_usernames && OWNER_RE.is_match(&normalized) {
        normalized = OWNER_RE
            .replace_all(&normalized, "Owner: USERNAME")
            .to_string();
        log.push("usernames".to_string());
    }

    // 13. Mask version numbers. (Whole doctor `binary_version` result lines
    // are replaced outright by the host-tooling rule in step 13b, which
    // subsumes the older OK-line version mask that lived here.)
    if config.mask_version_numbers && VERSION_NUM_RE.is_match(&normalized) {
        normalized = VERSION_NUM_RE
            .replace_all(&normalized, "$1 X.Y.Z")
            .to_string();
        log.push("version_numbers".to_string());
    }

    // 13a. Mask the doctor engine summary line (engine version + host state).
    if config.mask_version_numbers && ENGINE_LINE_RE.is_match(&normalized) {
        normalized = ENGINE_LINE_RE
            .replace_all(&normalized, "ENGINE ENGINE-REDACTED")
            .to_string();
        log.push("engine_line".to_string());
    }

    // 13b. Mask check results determined by the host rather than by br.
    if config.mask_host_tooling && HOST_DEPENDENT_CHECK_RE.is_match(&normalized) {
        normalized = HOST_DEPENDENT_CHECK_RE
            .replace_all(&normalized, "${1}HOST-DEPENDENT ${2}")
            .to_string();
        log.push("host_tooling".to_string());
    }

    // 14. Strip trailing whitespace (per line)
    if config.strip_trailing_whitespace {
        let lines: Vec<&str> = normalized.lines().collect();
        let trimmed: Vec<String> = lines
            .iter()
            .map(|line| TRAILING_WS_RE.replace_all(line, "").to_string())
            .collect();
        let new_text = trimmed.join("\n");
        if new_text != normalized {
            normalized = new_text;
            log.push("trailing_whitespace".to_string());
        }
    }

    // 15. Collapse multiple blank lines
    if config.collapse_blank_lines && MULTIPLE_BLANK_RE.is_match(&normalized) {
        normalized = MULTIPLE_BLANK_RE
            .replace_all(&normalized, "\n\n")
            .to_string();
        log.push("blank_lines".to_string());
    }

    (normalized, log)
}

/// Legacy `normalize_output` function for backward compatibility.
///
/// Uses golden configuration for full normalization.
pub fn normalize_output(output: &str) -> String {
    let (normalized, _) = normalize_text_with_log(output, &TextNormConfig::golden());
    normalized
}

/// Strip only ANSI codes while preserving other content.
pub fn strip_ansi(text: &str) -> String {
    ANSI_RE.replace_all(text, "").to_string()
}

/// Normalize for minimal cross-platform compatibility.
pub fn normalize_minimal(output: &str) -> String {
    let (normalized, _) = normalize_text_with_log(output, &TextNormConfig::minimal());
    normalized
}

fn normalize_id_string(s: &str) -> String {
    // Normalize strings that contain issue IDs like "bd-abc:open" or "bd-xyz"
    let id_re = Regex::new(r"\b[a-zA-Z0-9_]+-[a-z0-9]{3,}\b").expect("id regex");
    id_re.replace_all(s, "ISSUE_ID").to_string()
}

/// insta-safe JSON snapshot wrapper.
///
/// fsqlite-ext-json 0.3.6+ turns on serde_json's `arbitrary_precision`
/// feature for the whole build (Cargo feature unification). Under that
/// feature `serde_json::Number` serializes through *foreign* serializers —
/// like insta's Content model — as a private newtype struct, leaking
/// `{"$serde_json::private::Number": "2"}` into snapshots even though
/// serde_json's own output bytes (what `br --json` actually prints) are
/// unchanged. Re-emit every number as a primitive i64/u64/f64 so the
/// goldens stay byte-identical regardless of that feature flag.
pub struct SnapshotJson<'a>(pub &'a Value);

impl serde::Serialize for SnapshotJson<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self.0 {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    serializer.serialize_i64(i)
                } else if let Some(u) = n.as_u64() {
                    serializer.serialize_u64(u)
                } else {
                    serializer.serialize_f64(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            Value::String(s) => serializer.serialize_str(s),
            Value::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(&SnapshotJson(item))?;
                }
                seq.end()
            }
            Value::Object(map) => {
                let mut m = serializer.serialize_map(Some(map.len()))?;
                for (key, value) in map {
                    m.serialize_entry(key, &SnapshotJson(value))?;
                }
                m.end()
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
pub fn normalize_json(json: &Value) -> Value {
    match json {
        Value::Object(map) => {
            let mut new_map = serde_json::Map::new();
            for (key, value) in map {
                let normalized_value = match key.as_str() {
                    "id" | "issue_id" | "depends_on_id" | "blocks_id" => {
                        Value::String("ISSUE_ID".to_string())
                    }
                    "root" => Value::String("ISSUE_ID".to_string()),
                    "created_at" | "updated_at" | "closed_at" | "due_at" | "defer_until"
                    | "deleted_at" | "marked_at" | "exported_at" => {
                        Value::String("TIMESTAMP".to_string())
                    }
                    "content_hash" => Value::String("HASH".to_string()),
                    // Normalize source_repo/source_repo_path: br resolves "." to the absolute
                    // path of the workspace; under tempdir-based tests this is
                    // a randomly-named ".tmpXXXXXX" path, so the snapshot must
                    // collapse it back to a stable token. (Issue surfaced by
                    // beads_rust-l6xl audit; PC-RECOVERY-adjacent: not a
                    // safety problem, just a snapshot determinism gap.)
                    "source_repo" | "source_repo_path" => {
                        if let Value::String(_) = value {
                            Value::String("SOURCE_REPO".to_string())
                        } else {
                            normalize_json(value)
                        }
                    }
                    // Normalize actor/user fields that vary by system
                    "created_by" | "assignee" | "owner" | "author" | "deleted_by"
                    | "closed_by_session" | "actor" => {
                        // Only normalize if the value is a non-empty string
                        if let Value::String(s) = value {
                            if s.is_empty() {
                                Value::String(String::new())
                            } else {
                                Value::String("ACTOR".to_string())
                            }
                        } else if value.is_null() {
                            Value::Null
                        } else {
                            normalize_json(value)
                        }
                    }
                    // Handle blocked_by array which contains ID:status strings
                    "blocked_by" | "blocks" | "depends_on" => {
                        if let Value::Array(items) = value {
                            Value::Array(
                                items
                                    .iter()
                                    .map(|v| {
                                        if let Value::String(s) = v {
                                            Value::String(normalize_id_string(s))
                                        } else {
                                            normalize_json(v)
                                        }
                                    })
                                    .collect(),
                            )
                        } else {
                            normalize_json(value)
                        }
                    }
                    "roots" => {
                        if let Value::Array(items) = value {
                            Value::Array(
                                items
                                    .iter()
                                    .map(|v| {
                                        if matches!(v, Value::String(_)) {
                                            Value::String("ISSUE_ID".to_string())
                                        } else {
                                            normalize_json(v)
                                        }
                                    })
                                    .collect(),
                            )
                        } else {
                            normalize_json(value)
                        }
                    }
                    "edges" => {
                        if let Value::Array(items) = value {
                            Value::Array(
                                items
                                    .iter()
                                    .map(|edge| match edge {
                                        Value::Array(pair) => Value::Array(
                                            pair.iter()
                                                .map(|v| {
                                                    if matches!(v, Value::String(_)) {
                                                        Value::String("ISSUE_ID".to_string())
                                                    } else {
                                                        normalize_json(v)
                                                    }
                                                })
                                                .collect(),
                                        ),
                                        _ => normalize_json(edge),
                                    })
                                    .collect(),
                            )
                        } else {
                            normalize_json(value)
                        }
                    }
                    _ => normalize_json(value),
                };
                new_map.insert(key.clone(), normalized_value);
            }
            Value::Object(new_map)
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize_json).collect()),
        other => other.clone(),
    }
}

pub fn normalize_jsonl(contents: &str) -> String {
    let mut lines = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).expect("jsonl line");
        let normalized = normalize_json(&value);
        lines.push(serde_json::to_string(&normalized).expect("jsonl normalize"));
    }
    // Sort lines to ensure deterministic output (IDs are content-hash based and vary)
    lines.sort();
    lines.join("\n")
}

mod cli_output;
mod error_messages;
mod history_diff_output;
mod json_output;
mod jsonl_format;
mod robot_output;
mod schema_output;
mod toon_output;

// ============================================================================
// Tests for Golden Text Snapshot System
// ============================================================================

#[cfg(test)]
mod golden_snapshot_tests {
    use super::*;

    #[test]
    fn test_strip_ansi_codes() {
        let input = "\x1b[31mRed text\x1b[0m normal \x1b[1;32mgreen bold\x1b[0m";
        let result = strip_ansi(input);
        assert_eq!(result, "Red text normal green bold");
    }

    #[test]
    fn test_strip_ansi_preserves_unicode() {
        let input = "\x1b[31m✓ Success\x1b[0m ○ Open ● Closed";
        let result = strip_ansi(input);
        assert_eq!(result, "✓ Success ○ Open ● Closed");
    }

    #[test]
    fn test_normalize_line_endings() {
        let input = "line1\r\nline2\r\nline3";
        let snapshot = TextSnapshot::golden(input);
        assert!(snapshot.normalized.contains("line1\nline2\nline3"));
        assert!(!snapshot.normalized.contains("\r\n"));
    }

    #[test]
    fn test_normalize_paths_windows_to_unix() {
        let input = r"C:\Users\test\project\.beads\issues.jsonl";
        let config = TextNormConfig {
            normalize_paths: true,
            mask_home_paths: false,
            ..Default::default()
        };
        let (normalized, _) = normalize_text_with_log(input, &config);
        assert_eq!(normalized, "C:/Users/test/project/.beads/issues.jsonl");
    }

    #[test]
    fn test_redact_issue_ids() {
        let input = "Issue bd-abc123 depends on beads_rust-xyz789";
        let snapshot = TextSnapshot::golden(input);
        assert!(snapshot.normalized.contains("ID-REDACTED"));
        assert!(!snapshot.normalized.contains("bd-abc123"));
        assert!(!snapshot.normalized.contains("beads_rust-xyz789"));
    }

    #[test]
    fn test_mask_timestamps() {
        let input = "Created at 2026-01-17T12:30:45.123456Z, updated 2026-01-18T09:15:00+05:00";
        let snapshot = TextSnapshot::golden(input);
        assert!(snapshot.normalized.contains("YYYY-MM-DDTHH:MM:SS"));
        assert!(!snapshot.normalized.contains("2026-01-17"));
    }

    #[test]
    fn test_mask_git_hash() {
        let input = "br version 0.1.0 (dev) (main@abc1234)";
        let snapshot = TextSnapshot::golden(input);
        assert_eq!(snapshot.normalized, "br version X.Y.Z (BUILD)");
        assert!(!snapshot.normalized.contains("abc1234"));
    }

    /// `br doctor`'s `binary_version` check prints the bare `br <semver>` form
    /// rather than `version <semver>`. Leaving it unmasked pins the crate
    /// version into the doctor golden and breaks it on every release.
    ///
    /// The whole `binary_version` result line is now replaced outright, because
    /// its *message* is host-dependent too and not just its version number
    /// (`beads_rust-alur`) — which subsumes the original concern: no version can
    /// survive if the line does not. Both properties are asserted here.
    #[test]
    fn test_mask_bare_br_version() {
        let input = "OK binary_version: Running br 0.2.19; no beads_rust Cargo.toml reachable";
        let snapshot = TextSnapshot::golden(input);
        assert_eq!(snapshot.normalized, "HOST-DEPENDENT binary_version");
        assert!(!snapshot.normalized.contains("0.2.19"));

        // The bare `br <semver>` form is still masked wherever it appears
        // outside a check-result line, which is what VERSION_NUM_RE is for.
        let loose = TextSnapshot::golden("Running br 0.2.19 from /usr/local/bin");
        assert_eq!(loose.normalized, "Running br X.Y.Z from /usr/local/bin");
    }

    /// Prerelease suffixes must be masked with the version they belong to,
    /// otherwise a `-rc.1` build leaves a dangling fragment in the golden.
    #[test]
    fn test_mask_prerelease_version() {
        let input = "br version 0.2.19-rc.1";
        let snapshot = TextSnapshot::golden(input);
        assert_eq!(snapshot.normalized, "br version X.Y.Z");
    }

    #[test]
    fn test_mask_home_paths_linux() {
        let input = "Config at /home/testuser/.config/br/config.yaml";
        let snapshot = TextSnapshot::golden(input);
        assert!(snapshot.normalized.contains("/HOME/.config/br/config.yaml"));
        assert!(!snapshot.normalized.contains("testuser"));
    }

    #[test]
    fn test_mask_home_paths_macos() {
        let input = "Config at /Users/testuser/.config/br/config.yaml";
        let snapshot = TextSnapshot::golden(input);
        assert!(snapshot.normalized.contains("/HOME/.config/br/config.yaml"));
        assert!(!snapshot.normalized.contains("testuser"));
    }

    #[test]
    fn test_mask_temp_paths() {
        let input = "Temp file at /tmp/.tmpABC123XYZ";
        let snapshot = TextSnapshot::golden(input);
        assert!(snapshot.normalized.contains("/TMP"));
    }

    #[test]
    fn test_mask_nested_temp_paths() {
        let input =
            "Temp file at /data/projects/beads_rust/.rch-target-worker/.tmpABC123XYZ/.beads";
        let snapshot = TextSnapshot::golden(input);
        assert_eq!(snapshot.normalized, "Temp file at /TMP/.beads");
    }

    #[test]
    fn test_preserve_arbitrary_dot_tmp_paths() {
        let input = "State file at /etc/app/.tmpCorrupt";
        let snapshot = TextSnapshot::golden(input);
        assert_eq!(snapshot.normalized, input);
    }

    #[test]
    fn test_mask_doctor_binary_version_ok_variants() {
        // Both OK-line variants collapse to the same host-independent form
        // (the whole check-result line is host-dependent, so step 13b
        // replaces it outright); no version number may survive.
        let outside_tree = TextSnapshot::golden(
            "OK binary_version: Running br 0.2.15; no beads_rust Cargo.toml reachable from .beads/ — not flagging",
        );
        let inside_tree = TextSnapshot::golden(
            "OK binary_version: Running br 0.2.19; matches (or is ahead of) Cargo.toml at /data/projects/beads_rust/Cargo.toml (0.2.19)",
        );
        assert_eq!(outside_tree.normalized, "HOST-DEPENDENT binary_version");
        assert_eq!(inside_tree.normalized, outside_tree.normalized);
        assert!(!outside_tree.normalized.contains("0.2.15"));
        assert!(!inside_tree.normalized.contains("0.2.19"));
    }

    #[test]
    fn test_normalize_line_numbers() {
        let input = "Error at src/storage/sqlite.rs:1234: connection failed";
        let snapshot = TextSnapshot::golden(input);
        assert!(snapshot.normalized.contains(".rs:LINE:"));
        assert!(!snapshot.normalized.contains(":1234:"));
    }

    #[test]
    fn test_strip_trailing_whitespace() {
        let input = "line1   \nline2\t\t\nline3";
        let snapshot = TextSnapshot::golden(input);
        assert!(!snapshot.normalized.contains("   \n"));
        assert!(!snapshot.normalized.contains("\t\t\n"));
    }

    #[test]
    fn test_collapse_blank_lines() {
        let input = "line1\n\n\n\n\nline2";
        let snapshot = TextSnapshot::golden(input);
        // Should collapse to max 2 newlines (one blank line)
        assert!(!snapshot.normalized.contains("\n\n\n"));
    }

    #[test]
    fn test_minimal_config_preserves_ids() {
        let input = "Issue bd-abc123 is ready";
        let snapshot = TextSnapshot::minimal(input);
        // Minimal config doesn't redact IDs
        assert!(snapshot.normalized.contains("bd-abc123"));
    }

    #[test]
    fn test_duration_masking() {
        let input = "Completed in 123.45ms, total 5s";
        let config = TextNormConfig::with_duration_masking();
        let (normalized, _) = normalize_text_with_log(input, &config);
        assert!(normalized.contains("DURATION"));
        assert!(!normalized.contains("123.45ms"));
    }

    #[test]
    fn test_text_snapshot_metadata() {
        let input = "\x1b[31mbd-abc\x1b[0m 2026-01-17";
        let snapshot = TextSnapshot::golden(input);

        assert!(snapshot.was_normalized());
        assert!(
            snapshot
                .normalizations_applied
                .contains(&"ansi_codes".to_string())
        );
        assert!(
            snapshot
                .normalizations_applied
                .contains(&"issue_ids".to_string())
        );

        let json = snapshot.to_json();
        assert!(json["was_normalized"].as_bool().unwrap());
    }

    #[test]
    fn test_text_diff_matches() {
        let text = "line1\nline2\nline3";
        let snap1 = TextSnapshot::golden(text);
        let snap2 = TextSnapshot::golden(text);

        let diff = TextDiff::compare(&snap1, &snap2);
        assert!(diff.matches);
        assert!(diff.missing_lines.is_empty());
        assert!(diff.extra_lines.is_empty());
        assert!(diff.different_lines.is_empty());
    }

    #[test]
    fn test_text_diff_detects_differences() {
        let expected = "line1\nline2\nline3";
        let actual = "line1\nmodified\nline3";

        let snap_expected = TextSnapshot::golden(expected);
        let snap_actual = TextSnapshot::golden(actual);

        let diff = TextDiff::compare(&snap_expected, &snap_actual);
        assert!(!diff.matches);
        assert_eq!(diff.different_lines.len(), 1);
        assert_eq!(diff.different_lines[0].0, "line2");
        assert_eq!(diff.different_lines[0].1, "modified");
    }

    #[test]
    fn test_text_diff_detects_missing_lines() {
        let expected = "line1\nline2\nline3";
        let actual = "line1\nline2";

        let snap_expected = TextSnapshot::golden(expected);
        let snap_actual = TextSnapshot::golden(actual);

        let diff = TextDiff::compare(&snap_expected, &snap_actual);
        assert!(!diff.matches);
        assert_eq!(diff.missing_lines.len(), 1);
        assert_eq!(diff.missing_lines[0], "line3");
    }

    #[test]
    fn test_text_diff_detects_extra_lines() {
        let expected = "line1\nline2";
        let actual = "line1\nline2\nextra";

        let snap_expected = TextSnapshot::golden(expected);
        let snap_actual = TextSnapshot::golden(actual);

        let diff = TextDiff::compare(&snap_expected, &snap_actual);
        assert!(!diff.matches);
        assert_eq!(diff.extra_lines.len(), 1);
        assert_eq!(diff.extra_lines[0], "extra");
    }

    #[test]
    fn test_text_diff_format() {
        let expected = "line1\nline2";
        let actual = "line1\nmodified";

        let snap_expected = TextSnapshot::golden(expected);
        let snap_actual = TextSnapshot::golden(actual);

        let diff = TextDiff::compare(&snap_expected, &snap_actual);
        let formatted = diff.format_diff();

        assert!(formatted.contains("Different lines"));
        assert!(formatted.contains("expected: line2"));
        assert!(formatted.contains("actual:   modified"));
    }

    #[test]
    fn test_normalize_output_backward_compat() {
        // Verify the legacy function still works
        let input = "\x1b[31mbd-abc\x1b[0m 2026-01-17T12:00:00Z";
        let result = normalize_output(input);

        assert!(!result.contains("\x1b["));
        assert!(result.contains("ID-REDACTED"));
        assert!(result.contains("YYYY-MM-DDTHH:MM:SS"));
    }

    #[test]
    fn test_comprehensive_normalization() {
        let input = r"
Issue bd-abc123 created
  Path: C:\Users\developer\project\.beads\issues.jsonl
  Created: 2026-01-17T15:30:45.123Z
  Version: br 0.1.0 (main@deadbeef)
  Log: src/cli/create.rs:42: success
  Temp: /tmp/.tmpABC123
";
        let snapshot = TextSnapshot::golden(input);

        // All volatile content should be normalized
        assert!(!snapshot.normalized.contains("bd-abc123"));
        assert!(!snapshot.normalized.contains("developer"));
        assert!(!snapshot.normalized.contains("deadbeef"));
        assert!(!snapshot.normalized.contains(":42:"));
        assert!(!snapshot.normalized.contains("2026-01-17"));

        // Structural content should be preserved
        assert!(snapshot.normalized.contains("Issue"));
        assert!(snapshot.normalized.contains("created"));
        assert!(snapshot.normalized.contains("Path:"));
    }
}

#[cfg(test)]
mod host_tooling_masking_tests {
    use super::{TextNormConfig, normalize_output, normalize_text_with_log};

    /// `beads_rust-alur`: `sqlite3.integrity_check` reports OK where the system
    /// `sqlite3` binary exists and WARN where it does not, so the golden must
    /// not encode either. Both host states must normalize to the same text.
    #[test]
    fn sqlite3_integrity_check_is_host_independent() {
        let with_sqlite3 = "OK db.write_probe\nOK sqlite3.integrity_check\n";
        let without_sqlite3 = "OK db.write_probe\n\
             WARN sqlite3.integrity_check: sqlite3 not available; skipping orthogonal integrity validation\n";

        assert_eq!(
            normalize_output(with_sqlite3),
            normalize_output(without_sqlite3),
            "the golden must not encode whether the host has sqlite3 installed"
        );
        assert!(normalize_output(with_sqlite3).contains("HOST-DEPENDENT sqlite3.integrity_check"));
    }

    /// `beads_rust-alur`: `binary_version`'s message depends on whether a
    /// `beads_rust` Cargo.toml sits above the temp workspace — true on rch
    /// workers, which put `TMPDIR` inside the synced repo, false on a developer
    /// box. Both forms must normalize to the same text.
    #[test]
    fn binary_version_message_is_host_independent() {
        let in_repo = "OK binary_version: Running br 0.2.19; matches (or is ahead of) Cargo.toml at /data/projects/beads_rust/Cargo.toml (0.2.19)\n";
        let outside_repo = "OK binary_version: Running br 0.2.19; no beads_rust Cargo.toml reachable from .beads/ — not flagging\n";

        assert_eq!(
            normalize_output(in_repo),
            normalize_output(outside_repo),
            "the golden must not encode where the temp workspace happened to live"
        );
        assert!(normalize_output(in_repo).contains("HOST-DEPENDENT binary_version"));
        // The bare trailing version that VERSION_NUM_RE cannot catch must not
        // survive into the golden.
        assert!(!normalize_output(in_repo).contains("0.2.19"));
    }

    #[test]
    fn masking_preserves_neighbouring_checks_and_the_check_name() {
        let (normalized, log) = normalize_text_with_log(
            "OK sqlite.integrity_check\nWARN sqlite3.integrity_check: sqlite3 not available\nOK db.sidecars\n",
            &TextNormConfig::golden(),
        );
        // The engine's own integrity check is NOT host-dependent and must keep
        // its real status.
        assert!(normalized.contains("OK sqlite.integrity_check"));
        assert!(normalized.contains("OK db.sidecars"));
        // The check name survives, so removing or renaming the check still
        // fails the snapshot.
        assert!(normalized.contains("HOST-DEPENDENT sqlite3.integrity_check"));
        assert!(log.contains(&"host_tooling".to_string()));
    }

    #[test]
    fn masking_is_opt_out() {
        // `minimal()` leaves the input untouched apart from ANSI/line-ending
        // normalization — including the trailing newline, since it does not
        // strip trailing whitespace.
        let (normalized, log) =
            normalize_text_with_log("OK sqlite3.integrity_check\n", &TextNormConfig::minimal());
        assert_eq!(normalized, "OK sqlite3.integrity_check\n");
        assert!(!log.contains(&"host_tooling".to_string()));
    }
}
