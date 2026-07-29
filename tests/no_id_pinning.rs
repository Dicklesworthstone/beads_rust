//! ID-pinning anti-pattern lint as a cargo test.
//!
//! Created 2026-05-09 for beads_rust-s5se (audit-driven cleanup).
//!
//! Detects the anti-pattern uncovered by `beads_rust-jsgu`: tests that
//! `assert_eq!` on generated content-hash IDs (e.g. `"test-c75c9ac8"`)
//! instead of asserting on the relative ordering / invariants. These
//! tests break whenever the ID generator's hash function changes.
//!
//! ## Policy (from `beads_rust-lnqc`'s audit doc)
//!
//! The current codebase has 0 true ID-pinning hits. This lint is preventive:
//! it fails CI if a NEW test introduces the anti-pattern.
//!
//! ## Escape hatch
//!
//! Some legitimate patterns look like ID-pinning under a naive grep:
//! parser tests, normalization-rule placeholders (e.g. `"bd-HASH"`), and
//! fixture round-trips. These are documented in
//! `docs/audit_id_pinning_2026_05_09.md`. To exempt a line, add a comment
//! containing `invariant:` on the same line:
//!
//! ```rust,ignore
//! assert_eq!(parsed_id, "bd-abc123"); // invariant: parser test, not ID-pinning
//! ```
//!
//! See also `tests/common/ordering.rs` for the recommended replacement
//! helpers (`assert_priority_ordered`, `assert_no_duplicate_ids`, etc.).

use regex::Regex;
use std::path::Path;
use walkdir::WalkDir;

fn id_pinning_pattern() -> Regex {
    // Matches: `assert_eq!(<lhs>, "<prefix>-<suffix>")`
    // where prefix ∈ {test, tmp, br, bd, proj} and suffix has 4+ alphanumeric chars.
    Regex::new(r#"assert_(eq|ne)!\([^,]*,\s*"(test|tmp|br|bd|proj)-[a-zA-Z0-9]{4,}""#)
        .expect("compile lint regex")
}

/// True if `line_text` contains an `invariant:` annotation marking the
/// match as deliberate. The 10 known-legitimate hits documented in
/// `docs/audit_id_pinning_2026_05_09.md` are all annotated this way; any
/// new annotation is the recommended escape hatch.
fn has_invariant_annotation(line_text: &str) -> bool {
    line_text.contains("invariant:")
}

fn scan_tests_for_id_pinning() -> Vec<String> {
    let pattern = id_pinning_pattern();
    let tests_root = Path::new("tests");
    let mut violations = Vec::new();

    if !tests_root.exists() {
        return vec![format!(
            "no_id_pinning lint cannot find tests/ directory at {:?}; cwd may be wrong",
            std::env::current_dir().ok()
        )];
    }

    for entry in WalkDir::new(tests_root).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        // Skip ourselves
        if path.file_name().and_then(|n| n.to_str()) == Some("no_id_pinning.rs") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        for (idx, line) in src.lines().enumerate() {
            let line_no = idx + 1;
            if pattern.is_match(line) && !has_invariant_annotation(line) {
                violations.push(format!("{}:{}: {}", path.display(), line_no, line.trim()));
            }
        }
    }

    violations
}

/// Lint: scan tests/ for the ID-pinning anti-pattern. Fails if any line
/// matches the regex AND is NOT in the known-legitimate allowlist AND
/// does NOT carry an `invariant:` annotation comment.
#[test]
fn tests_must_not_assert_eq_on_generated_short_ids() {
    let violations = scan_tests_for_id_pinning();
    assert!(
        violations.is_empty(),
        "ID-pinning anti-pattern detected (annotate with `// invariant: <reason>` if intentional, \
         or migrate to invariant-based assertions per tests/common/ordering.rs):\n\n{}\n\n\
         See docs/audit_id_pinning_2026_05_09.md for the audit-2026-05-09 baseline.",
        violations.join("\n")
    );
}

/// Smoke test that the lint regex actually catches the canonical example.
/// Pure inline string match — no filesystem access. Proves the lint can
/// detect a fresh violation if one is introduced.
#[test]
fn lint_regex_catches_canonical_violation_inline() {
    let pattern = id_pinning_pattern();
    let offending = r#"    assert_eq!(actual_id, "test-c75c9ac8");"#;
    assert!(
        pattern.is_match(offending),
        "lint regex must match the canonical violation example; got pattern={:?}",
        pattern.as_str()
    );

    // Also confirm the non-violation form does NOT match
    let safe = "    assert_eq!(parsed.priority, Priority::CRITICAL);";
    assert!(
        !pattern.is_match(safe),
        "lint regex must NOT match safe assertions; falsely matched: {safe:?}"
    );

    // And confirm the invariant-annotated form is detected by the regex
    // (the annotation check is the second filter).
    let annotated = r#"    assert_eq!(id, "bd-abc123"); // invariant: parser test, not ID-pinning"#;
    assert!(
        pattern.is_match(annotated),
        "lint regex must match the annotated form (annotation check is a separate filter); got: {annotated:?}"
    );
    assert!(
        has_invariant_annotation(annotated),
        "annotation detector must recognize the documented form"
    );
}

/// Smoke test that the audit-baseline annotation count hasn't regressed.
/// Per `docs/audit_id_pinning_2026_05_09.md`, exactly 10 lines in the
/// codebase carry the `// invariant: ...` annotation as legitimate
/// exceptions. If this count changes:
///   - Decreased: someone removed an annotation; either fix the test
///     (it'll fail the main lint) OR migrate the line to invariant-based
///     assertions and update the audit doc.
///   - Increased: a new exception was added; review whether it's truly
///     legitimate per the audit doc's classification taxonomy and update
///     the count + audit doc.
#[test]
fn invariant_annotation_count_matches_2026_05_09_audit_baseline() {
    let pattern = id_pinning_pattern();
    let tests_root = Path::new("tests");
    if !tests_root.exists() {
        return;
    }
    let mut annotated = 0;
    for entry in WalkDir::new(tests_root).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("no_id_pinning.rs") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in src.lines() {
            if pattern.is_match(line) && has_invariant_annotation(line) {
                annotated += 1;
            }
        }
    }
    // Exactly 10 known-legitimate hits per docs/audit_id_pinning_2026_05_09.md
    assert_eq!(
        annotated, 10,
        "audit baseline drift: expected 10 lines with `// invariant:` annotation \
         (per docs/audit_id_pinning_2026_05_09.md), found {annotated}. If you \
         intentionally added/removed an annotated exception, update both this \
         constant and the audit doc."
    );
}
