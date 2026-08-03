//! Recorded-output ("golden"/snapshot) tests for the `br` CLI.
//!
//! READ THIS BEFORE RE-RECORDING ANYTHING.
//!
//! A snapshot test knows exactly one thing: whether output CHANGED. It knows
//! nothing about whether output is CORRECT. Whatever was in the buffer on the
//! day someone ran `cargo insta accept` becomes ground truth, and every run
//! afterwards defends it — including any defect it happened to contain. So a
//! green snapshot suite is not evidence that the output is right; it is
//! evidence that the output is the same as it was.
//!
//! That is not hypothetical here. `cli_output__search_output.snap` once
//! recorded a `br search` line with its `[task]` type badge missing and the
//! title truncated, because the console-markup parser was eating bracketed
//! text. The test passed for the entire life of the bug, and it FAILED the
//! fix: restoring the badge changed the bytes. The obvious response —
//! re-record — would have enshrined the corruption a second time.
//!
//! WHAT `cargo insta accept` PROVES: that the new bytes are the bytes the
//! code currently emits. WHAT IT DOES NOT PROVE: that those bytes are right.
//! When a snapshot fails, the question is never "does the code still produce
//! the old value?" but "which of these two values SHOULD the command print?"
//! — and the answer has to come from the command's intent, not from the diff.
//!
//! CONVENTIONS THIS MODULE FOLLOWS, each of which exists because it was
//! violated:
//!
//! 1. A recorded value must be reviewable as CORRECT ON ITS FACE. If reading
//!    the `.snap` cannot tell you whether the command behaved, the fixture is
//!    too thin — see `snapshot_show_output`, which asserted a description
//!    render path with an issue that had no description.
//!
//! 2. An empty or trivially-satisfiable value must be EARNED. `[]`, `0` and a
//!    zero-byte file are what a command prints when it works and also when it
//!    is completely broken. Either populate the fixture so the empty answer
//!    comes from filtering, or compose the value with its surrounding facts
//!    (`compose_invocation`). `tests/snapshot_hygiene.rs` enforces this.
//!
//! 3. THE NORMALIZER IS PART OF THE ORACLE. Everything here passes through
//!    `normalize_output`/`normalize_json` before being recorded, so an
//!    over-broad masking rule destroys content BEFORE it reaches the expected
//!    value — and unlike a rendering bug, nothing downstream can ever detect
//!    it. A blanket `\w+-[a-z0-9]{3,}` ID rule silently rewrote `--dry-run`,
//!    `--external-ref` and `Auto-import` as `ID-REDACTED`, leaving
//!    `create_help.snap` unable to notice a rename of the flags it exists to
//!    document. Any masking rule added here needs a test that it does NOT eat
//!    ordinary text (see `test_redaction_preserves_hyphenated_words_and_flag_names`)
//!    as well as one that it still masks what it is for.
//!
//! 4. Structural invariants over the corpus live in
//!    `tests/snapshot_hygiene.rs`. They catch the corruption class without
//!    needing to be re-recorded when output legitimately changes, which is
//!    the only thing here that can catch a FUTURE corruption.

#![allow(clippy::module_name_repetitions, clippy::trivial_regex, dead_code)]

#[path = "../common/mod.rs"]
mod common;

use common::cli::{BrWorkspace, run_br};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::{self, Write};
use std::process::ExitStatus;
use std::sync::LazyLock;

pub fn init_workspace() -> BrWorkspace {
    // `init` no longer accepts `--prefix` (issue prefixes are always
    // explicit at creation time, never a project-wide default — see
    // docs/PLAN_REMOVE_BD_ISSUE_PREFIX.md). `create_issue` below relies on
    // the test harness's `--prefix bd` convenience shim.
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    workspace
}

/// Turn the workspace into a git repository.
///
/// Identity is set locally (never `--global`) and `HOME` is already pointed
/// at the workspace by the harness, so nothing here reads or writes the
/// developer's own git configuration.
pub fn git_init(workspace: &BrWorkspace) {
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "snapshots@example.invalid"][..],
        &["config", "user.name", "Snapshot Fixture"][..],
    ] {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&workspace.root)
            .output()
            .expect(
                "git must be available: fixtures that need a repository \
                     cannot silently degrade to an empty result",
            );
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Record an empty commit carrying `message`.
///
/// Empty on purpose: the fixtures using this care about what the commit
/// MESSAGE says (it names an issue ID), not about any file it touches.
pub fn git_commit(workspace: &BrWorkspace, message: &str) {
    let out = std::process::Command::new("git")
        .args(["commit", "-q", "--allow-empty", "-m", message])
        .current_dir(&workspace.root)
        .output()
        .expect("git commit runs");
    assert!(
        out.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

pub fn create_issue(workspace: &BrWorkspace, title: &str, label: &str) -> String {
    let output = run_br(workspace, ["create", title], label);
    assert!(output.status.success(), "create failed: {}", output.stderr);
    parse_created_id(&output.stdout)
}

pub fn parse_created_id(stdout: &str) -> String {
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
/// Candidate `prefix-suffix` tokens for issue-ID redaction.
///
/// This deliberately over-matches; [`looks_like_issue_id`] decides. The
/// pattern this replaced was `\b[a-zA-Z0-9_-]+-[a-z0-9]{3,}\b`, which is to
/// say ANY hyphenated word, and it silently destroyed content on its way
/// into the recorded snapshots: `--dry-run`, `--no-color`, `--external-ref`,
/// `parent-child`, `auto-discover`, `append-only`, `Agent-first`,
/// `Power-user` and `Auto-import` were all replaced by `ID-REDACTED`.
/// `create_help.snap` therefore recorded `--ID-REDACTED <EXTERNAL_REF>` and
/// could not have failed if the flag had been renamed — the one test
/// guarding the CLI surface was blind to exactly the thing it guards.
///
/// Redaction is normalization, and normalization runs BEFORE recording, so
/// an over-broad rule here is worse than an over-broad rule anywhere else in
/// the suite: it damages the oracle rather than the output. Anything added
/// to this pattern must be justified against that.
static ID_CANDIDATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[a-zA-Z0-9_]+-[a-zA-Z0-9]{3,}\b").expect("id candidate regex"));

/// Issue-ID prefixes this suite actually mints.
///
/// The harness appends `--prefix bd` to every `create`/`q` (see
/// `apply_default_test_prefix_shim`), so `bd-` is the only prefix that
/// reaches a text snapshot. Tokens with any other prefix are redacted only
/// when their suffix carries a digit (see [`looks_like_issue_id`]).
///
/// If a fixture ever mints a different prefix AND draws an all-letter hash,
/// the raw ID reaches the snapshot and the test fails loudly on the next
/// run. That is the intended failure mode: a visibly unstable snapshot says
/// "add your prefix here", whereas the old catch-all silently ate English.
const KNOWN_TEST_ID_PREFIXES: &[&str] = &["bd"];

/// Whether a `prefix-suffix` token is an issue ID rather than a hyphenated
/// English word or a CLI flag.
///
/// Mirrors the product's own `is_likely_hash_segment` (`src/util/id.rs`):
/// IDs are `<prefix>-<base36 hash>`, hashes are lowercase base36, and a hash
/// of 4+ characters always contains a digit — the generator enforces that
/// precisely to avoid colliding with words. Three-character hashes may be
/// all letters (`bd-abc`), so for those the prefix must be one we mint.
fn looks_like_issue_id(token: &str) -> bool {
    let Some((prefix, hash)) = token.rsplit_once('-') else {
        return false;
    };
    if hash.len() < 3
        || !hash
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return false;
    }
    KNOWN_TEST_ID_PREFIXES.contains(&prefix) || hash.chars().any(|c| c.is_ascii_digit())
}
static TS_FULL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?")
        .expect("full timestamp regex")
});
static DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{4}-\d{2}-\d{2}").expect("date regex"));
static VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Matches the `(branch@shorthash)` suffix `br version` emits, e.g.
    // `(feat-testclean@f050a2a)` or `(main@abc1234)`. Previously this only
    // matched the literal branch names `main`/`master`/`HEAD`, so running
    // the suite from any feature branch (the normal case for a dev
    // workflow, and the ONLY case in this environment) left the real
    // branch name and commit hash unmasked, which then got mangled by the
    // generic issue-ID redaction pass instead of being cleanly replaced by
    // `(BRANCH@GIT_HASH)`. Widened to match any branch-like token.
    Regex::new(r"\([^\s@()]+@[0-9a-f]{4,40}\)").expect("version regex")
});
static OWNER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Owner: [a-zA-Z0-9_-]+").expect("owner regex"));
/// `bd show`'s creator line carries the resolving agent identity (or
/// the unix user when no agent identity is available), so it varies by
/// whoever/whatever runs the suite — mask it exactly like `Owner:`.
static CREATOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Creator: [a-zA-Z0-9_-]+").expect("creator regex"));
static VERSION_NUM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"version \d+\.\d+\.\d+").expect("version number regex"));
static LINE_NUM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.rs:\d+:").expect("line number regex"));
/// A backslash together with the character it precedes.
///
/// Captured as a pair so the replacement can spare `\[` and `\]`: those are
/// not path separators, they are the signature of a markup escape that
/// reached a sink which does not parse markup (`br show` shipped
/// `use \[bold] for headings` for a body reading `use [bold] for headings`).
/// Blanket-rewriting every backslash to `/` turned that artifact into a
/// plausible-looking `/[bold]` on its way into the recorded value — damage
/// disguised as normalization. Leave the backslash visible so a reviewer
/// sees it and `snapshot_hygiene` can fail on it.
static PATH_SEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\(.)").expect("path separator regex"));
static TRAILING_WS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[ \t]+$").expect("trailing whitespace regex"));
static MULTIPLE_BLANK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("multiple blank lines regex"));
static HOME_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/home/[a-zA-Z0-9_-]+").expect("home path regex"));
static USERS_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/Users/[a-zA-Z0-9_-]+").expect("users path regex"));
static TMP_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/tmp/\.tmp[a-zA-Z0-9]+|/var/folders/[a-zA-Z0-9/_-]+").expect("tmp path regex")
});
static DURATION_MS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+(\.\d+)?\s*(ms|µs|ns|s)").expect("duration regex"));
/// Matches `bd list`/`bd search`'s compact relative-age field, e.g.
/// `0s`, `5d`, `3w`, or the combined `created/updated` form `5d/2h`.
/// Distinct from `DURATION_MS_RE` (which is scoped to ms/µs/ns/s
/// timing output): ages also use m/h/d/w and the `A/B` combined form,
/// and unlike a timing duration an issue's age is inherently
/// time-dependent (relative to "now" at test-run time), so tests that
/// create an issue and immediately list it need this masked rather
/// than asserting on a literal "0s" that would flake under load.
static AGE_FIELD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b\d+(?:mo|s|m|h|d|w)(?:/\d+(?:mo|s|m|h|d|w))?\b").expect("age field regex")
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
    /// Mask `bd list`/`bd search`'s compact relative-age field (e.g.
    /// `0s`, `5d/2h`) — see [`AGE_FIELD_RE`]. Off by default in
    /// [`Self::golden`] since most golden snapshots don't render
    /// ages; opt in via [`Self::with_age_masking`] for the ones that
    /// do, rather than asserting a literal age that's only stable
    /// because the fixture and the assertion run within the same
    /// second.
    pub mask_ages: bool,
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
            mask_ages: false,
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

    /// Configuration for snapshots of `bd list`/`bd search` output
    /// where an issue was just created and immediately listed, so its
    /// rendered age (`0s`, or occasionally `1s` under load) is
    /// otherwise a source of snapshot flakiness.
    pub const fn with_age_masking() -> Self {
        Self {
            mask_ages: true,
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
// Long by construction: one normalization step per config flag, applied in
// a fixed order. Splitting it would scatter the order across helpers.
#[allow(clippy::too_many_lines)]
fn normalize_text_with_log(text: &str, config: &TextNormConfig) -> (String, Vec<String>) {
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

    // 3. Normalize path separators (Windows → Unix), sparing `\[`/`\]`.
    if config.normalize_paths && normalized.contains('\\') {
        let replaced = PATH_SEP_RE.replace_all(&normalized, |caps: &regex::Captures<'_>| {
            let next = &caps[1];
            if next == "[" || next == "]" {
                format!("\\{next}")
            } else {
                format!("/{next}")
            }
        });
        if replaced != normalized {
            normalized = replaced.into_owned();
            log.push("path_separators".to_string());
        }
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

    // 6. Mask git hashes. This runs before issue-ID redaction only because
    // `(branch@hash)` is a shape of its own; the redaction pass below no
    // longer mistakes a branch name like `feat-testclean` for an ID (its
    // suffix carries no digit), but the dedicated placeholder is clearer.
    if config.mask_git_hashes && VERSION_RE.is_match(&normalized) {
        normalized = VERSION_RE
            .replace_all(&normalized, "(BRANCH@GIT_HASH)")
            .to_string();
        log.push("git_hashes".to_string());
    }

    // 7. Redact issue IDs. `ID_CANDIDATE_RE` over-matches on purpose;
    // `looks_like_issue_id` is what decides, so that a hyphenated word or a
    // CLI flag is left intact instead of being replaced by a placeholder
    // that no later reviewer can undo.
    if config.redact_ids {
        let replaced = ID_CANDIDATE_RE.replace_all(&normalized, |caps: &regex::Captures<'_>| {
            let token = &caps[0];
            if looks_like_issue_id(token) {
                "ID-REDACTED".to_string()
            } else {
                token.to_string()
            }
        });
        if replaced != normalized {
            normalized = replaced.into_owned();
            log.push("issue_ids".to_string());
        }
    }

    // 8. Mask full timestamps
    if config.mask_timestamps && TS_FULL_RE.is_match(&normalized) {
        normalized = TS_FULL_RE
            .replace_all(&normalized, "YYYY-MM-DDTHH:MM:SS")
            .to_string();
        log.push("timestamps".to_string());
    }

    // 9. Mask dates (after timestamps to avoid double-masking)
    if config.mask_dates && DATE_RE.is_match(&normalized) {
        normalized = DATE_RE.replace_all(&normalized, "YYYY-MM-DD").to_string();
        log.push("dates".to_string());
    }

    // 10. Normalize line numbers
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

    // 11b. Mask compact relative ages (bd list/search's age column)
    if config.mask_ages && AGE_FIELD_RE.is_match(&normalized) {
        normalized = AGE_FIELD_RE.replace_all(&normalized, "AGE").to_string();
        log.push("ages".to_string());
    }

    // 12. Mask owner/usernames
    if config.mask_usernames && OWNER_RE.is_match(&normalized) {
        normalized = OWNER_RE
            .replace_all(&normalized, "Owner: USERNAME")
            .to_string();
        log.push("usernames".to_string());
    }

    // 12b. Mask the creator line the same way (same rationale: the
    // value is whoever ran the suite, agent identity or unix user).
    if config.mask_usernames && CREATOR_RE.is_match(&normalized) {
        normalized = CREATOR_RE
            .replace_all(&normalized, "Creator: USERNAME")
            .to_string();
        log.push("creator".to_string());
    }

    // 13. Mask version numbers
    if config.mask_version_numbers && VERSION_NUM_RE.is_match(&normalized) {
        normalized = VERSION_NUM_RE
            .replace_all(&normalized, "version X.Y.Z")
            .to_string();
        log.push("version_numbers".to_string());
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

/// Render a whole invocation — exit status, stdout AND stderr — as one
/// reviewable block.
///
/// WHY THIS EXISTS. A snapshot whose expected value is empty cannot fail.
/// A zero-byte `.snap` is satisfied by the command behaving correctly, by
/// the command crashing before it printed anything, by its output going to
/// the wrong stream, and by the command being deleted — as long as the exit
/// status stays zero. `list_empty.snap` was exactly that: nothing at all,
/// recorded as ground truth, counted among the tests guarding `br list`.
///
/// Some commands genuinely print nothing (`br list` on an empty workspace
/// is silent on purpose — see `src/cli/commands/list.rs`, where the silence
/// is a deliberate conformance contract with the Go `bd` implementation, not
/// an oversight). For those, the fix is not to make the command speak; it is
/// to state the expectation in a form that CAN fail. This composes the three
/// facts a reader needs, each explicitly labelled, so that "printed nothing"
/// is an assertion rather than an absence — and so a reader can tell WHICH
/// stream was empty.
///
/// Streams are labelled `<empty>` rather than left blank, deliberately: a
/// composed value that renders as a blank region has reproduced the original
/// problem in a fancier wrapper.
pub fn compose_invocation(command: &str, stdout: &str, stderr: &str, status: ExitStatus) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "$ {command}");
    let _ = writeln!(
        out,
        "exit: {}",
        status
            .code()
            .map_or_else(|| "signal".to_string(), |c| c.to_string())
    );
    for (label, stream) in [("stdout", stdout), ("stderr", stderr)] {
        let normalized = normalize_output(stream);
        let trimmed = normalized.trim_end();
        if trimmed.is_empty() {
            let _ = writeln!(out, "{label}: <empty>");
        } else {
            let _ = writeln!(out, "{label}:");
            for line in trimmed.lines() {
                let _ = writeln!(out, "  {line}");
            }
        }
    }
    out.trim_end().to_string()
}

/// Like [`normalize_output`], but additionally masks the compact
/// relative-age field so snapshots of freshly-created-then-listed
/// issues don't embed a time-dependent `0s`/`1s`.
pub fn normalize_output_with_age_masking(output: &str) -> String {
    let (normalized, _) = normalize_text_with_log(output, &TextNormConfig::with_age_masking());
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

/// Replace issue IDs inside a string ("bd-abc:open", "fix(bd-2p7): ...").
///
/// Uses the same [`looks_like_issue_id`] decision as the text normalizer,
/// deliberately: this function is also applied to FREE TEXT (a commit
/// message), and the blanket `\w+-[a-z0-9]{3,}` pattern it used to carry is
/// the one that turned `--dry-run` into `ID-REDACTED` on the text side. One
/// definition of "is an issue ID", two call sites — otherwise the bug just
/// moves to whichever copy is not being looked at.
fn normalize_id_string(s: &str) -> String {
    ID_CANDIDATE_RE
        .replace_all(s, |caps: &regex::Captures| {
            let token = &caps[0];
            if looks_like_issue_id(token) {
                "ISSUE_ID".to_string()
            } else {
                token.to_string()
            }
        })
        .to_string()
}

/// Map `f` over an array value, leaving non-arrays to the generic pass.
fn normalize_array(value: &Value, f: impl Fn(&Value) -> Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(f).collect()),
        other => normalize_json(other),
    }
}

/// An element that is itself a bare issue ID.
fn normalize_id_element(value: &Value) -> Value {
    if matches!(value, Value::String(_)) {
        Value::String("ISSUE_ID".to_string())
    } else {
        normalize_json(value)
    }
}

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
                    // A git object name is different on every run of the
                    // fixture that produces it (empty commits differ by
                    // committer timestamp), so it is masked rather than
                    // recorded — but masked to a NAMED placeholder, never
                    // dropped: the field's presence is part of the contract
                    // `br orphans` owes its caller.
                    "latest_commit" => Value::String("COMMIT_HASH".to_string()),
                    // Free text that legitimately quotes an issue ID (a
                    // commit message naming the bead it closed). The ID is
                    // volatile; the rest of the sentence is the assertion.
                    "latest_commit_message" => {
                        if let Value::String(s) = value {
                            Value::String(normalize_id_string(s))
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
                    "blocked_by" | "blocks" | "depends_on" => normalize_array(value, |v| match v {
                        Value::String(s) => Value::String(normalize_id_string(s)),
                        other => normalize_json(other),
                    }),
                    "roots" => normalize_array(value, normalize_id_element),
                    "edges" => normalize_array(value, |edge| match edge {
                        Value::Array(_) => normalize_array(edge, normalize_id_element),
                        other => normalize_json(other),
                    }),
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
mod json_output;
mod jsonl_format;

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

    /// The redactor must not eat text that is not an ID.
    ///
    /// EVERY other test in this module asserts that a volatile value IS
    /// masked; not one asserted that a stable value is NOT. That asymmetry is
    /// why the old catch-all pattern survived: it was only ever observed
    /// passing. It replaced `--dry-run`, `--external-ref`, `--no-color`,
    /// `parent-child`, `auto-discover` and `Auto-import` with `ID-REDACTED`
    /// on the way into `create_help.snap`, `help_output.snap` and three
    /// error snapshots, so those recordings could not have failed if the
    /// flags they document had been renamed.
    ///
    /// Each string below is real text from `br --help`, `br create --help`
    /// or a `beads_rust::sync` log line that the recorded snapshots lost.
    #[test]
    fn test_redaction_preserves_hyphenated_words_and_flag_names() {
        for stable in [
            "--dry-run",
            "--no-color",
            "--external-ref <EXTERNAL_REF>",
            "Parent issue ID (creates parent-child dep)",
            "Database path (auto-discover .beads/*.db if not set)",
            "Agent-first issue tracker (SQLite + JSONL)",
            "Read or append an issue's comments (append-only attributed history)",
            "Power-user / diagnostic commands",
            "Auto-import completed imported_count=0",
            "Auto-flush: exporting dirty issues",
            "Auto-flush complete exported=1",
        ] {
            let normalized = normalize_output(stable);
            assert_eq!(
                normalized, stable,
                "the redactor altered text that is not an issue ID; a snapshot \
                 recorded from this can no longer fail when the text changes"
            );
        }
    }

    /// The inverse guard: tightening the rule until it stops eating English
    /// can trivially be overshot into redacting nothing, and that failure
    /// would surface much later as an unstable snapshot blamed on something
    /// else. Every shape the harness actually mints must still be masked.
    #[test]
    fn test_redaction_still_masks_real_issue_ids() {
        for (input, must_not_contain) in [
            // The harness mints `bd-<3-char base36>`; all-letter draws are
            // ~38% of the space, so both must go.
            ("Created bd-2p7: a title", "bd-2p7"),
            ("Created bd-abc: a title", "bd-abc"),
            ("Issue not found: bd-nonexistent", "bd-nonexistent"),
            // Longer hashes always carry a digit (src/util/id.rs), which is
            // what lets an unknown prefix still be recognised.
            ("Issue bd-abc123 depends on beads_rust-xyz789", "bd-abc123"),
            (
                "Issue bd-abc123 depends on beads_rust-xyz789",
                "beads_rust-xyz789",
            ),
            ("Cycle detected: bd-1af -> bd-3m9", "bd-1af"),
        ] {
            let normalized = normalize_output(input);
            assert!(
                !normalized.contains(must_not_contain),
                "{must_not_contain:?} survived redaction in {normalized:?} \
                 \u{2014} a live issue ID in a snapshot makes it unstable"
            );
            assert!(
                normalized.contains("ID-REDACTED"),
                "expected a redaction placeholder in {normalized:?}"
            );
        }
    }

    /// Path normalization must not disguise a markup-escape artifact.
    ///
    /// `br show` shipped `use \[bold] for headings` for a stored body of
    /// `use [bold] for headings`. Blanket backslash-to-slash rewriting would
    /// have recorded that as `use /[bold] for headings` — still wrong, but
    /// now shaped like a path, which is exactly the disguise that gets a bad
    /// value waved through review. The backslash must reach the recorded
    /// value so a human (and `tests/snapshot_hygiene.rs`) can see it.
    #[test]
    fn test_path_normalization_preserves_escaped_brackets() {
        let escaped = r"use \[bold] for headings and \[red\]for errors";
        let normalized = normalize_output(escaped);
        assert_eq!(
            normalized, escaped,
            "a markup escape must survive normalization intact so it can be \
             recognised as corruption rather than as a Windows path"
        );
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
        let input = "Version 0.1.0 (main@abc1234)";
        let snapshot = TextSnapshot::golden(input);
        assert!(snapshot.normalized.contains("(BRANCH@GIT_HASH)"));
        assert!(!snapshot.normalized.contains("abc1234"));
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
