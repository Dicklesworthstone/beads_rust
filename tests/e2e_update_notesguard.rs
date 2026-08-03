//! E2E tests for the destructive-update guard on `br update`.
//!
//! Background (beads1-1euci / beads1-21y5o): `br update --notes` and its
//! siblings REPLACE the whole field, and the success line was identical
//! whether the caller appended or annihilated. Four agents lost content to
//! this in a single day, with no recovery path — the database keeps no prior
//! row version.
//!
//! Two behaviours are asserted here:
//!
//! 1. A write that SHRINKS a non-empty free-text field is REFUSED. Not warned
//!    about — refused: nothing is written and the process exits non-zero with
//!    a structured `DESTRUCTIVE_UPDATE` error. `--replace` is the opt-in.
//! 2. Every allowed write REPORTS the before/after size of each free-text
//!    field it touched, plus whether the prior content survives verbatim
//!    inside the new value.
//!
//! READBACK DISCIPLINE, which is how this bug went unnoticed for so long:
//! every readback here compares the WHOLE stored field against the exact
//! expected value. Grepping for the phrase you just wrote passes on a field
//! you just destroyed, so no assertion in this file does that alone.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

/// A payload long enough that any shrink is unambiguous.
const BLOCK_ONE: &str = "FIRST BLOCK: operator ruling, do not revoke. Evidence A, B, C.";
/// Strictly shorter than `BLOCK_ONE` — the exact shape of the reported bug.
const BLOCK_TWO: &str = "SECOND BLOCK: handoff note.";

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let normalized = line.strip_prefix("\u{2713} ").unwrap_or(line);
    normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn workspace_with_issue(title: &str) -> (BrWorkspace, String) {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create = run_br(&workspace, ["create", title], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);
    assert!(
        !id.is_empty(),
        "could not parse id from {:?}",
        create.stdout
    );
    (workspace, id)
}

/// Read one field back in full. Returns `""` when the field is unset.
fn stored_field(workspace: &BrWorkspace, id: &str, field: &str) -> String {
    let show = run_br(workspace, ["show", id, "--json"], "readback");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let value: Value = serde_json::from_str(&payload).expect("show json");
    let issue = value.get(0).unwrap_or(&value);
    issue
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Assert the whole field survived a refused write, byte for byte.
///
/// Deliberately not a `contains` check: a `contains` of the text you just
/// tried to write is precisely the readback that passed while the data was
/// being destroyed.
fn assert_field_intact(workspace: &BrWorkspace, id: &str, field: &str, expected: &str) {
    let actual = stored_field(workspace, id, field);
    assert_eq!(
        actual, expected,
        "{field} was modified by a write that should have been refused"
    );
    assert_eq!(
        actual.chars().count(),
        expected.chars().count(),
        "{field} changed length: any decrease means content was destroyed"
    );
}

/// The structured error payload br writes to stderr when stdout is a pipe.
///
/// Returns the inner object, i.e. the `error` member of the envelope, which
/// is the same shape `INVALID_STATUS` and friends are reported in.
fn structured_error(stderr: &str) -> Value {
    let start = stderr.find('{').unwrap_or_else(|| {
        panic!("no JSON error envelope in stderr: {stderr}");
    });
    let envelope: Value = serde_json::from_str(&stderr[start..]).expect("structured error json");
    let inner = envelope["error"].clone();
    assert!(
        inner.is_object(),
        "expected an `error` member in the envelope: {envelope}"
    );
    inner
}

fn seed_notes(workspace: &BrWorkspace, id: &str, text: &str) {
    let out = run_br(workspace, ["update", id, "--notes", text], "seed_notes");
    assert!(out.status.success(), "seeding notes failed: {}", out.stderr);
    assert_eq!(stored_field(workspace, id, "notes"), text, "seed readback");
}

// =============================================================================
// Does the storage layer normalize? (measured, not assumed)
// =============================================================================

/// EMPIRICAL PRECONDITION for the "write did not land as sent" report.
///
/// That report compares the value bd was handed against the value bd stored,
/// which is only meaningful if the storage layer stores free text verbatim.
/// If anything trimmed a trailing newline, folded CRLF, collapsed whitespace
/// or NFC-normalized, the comparison would fire on ordinary correct writes
/// and the whole delta line would be trained into background noise within a
/// week.
///
/// So it is measured here rather than assumed, over the transformations a
/// storage or serde layer performs in passing, and the result is pinned: a
/// later well-meaning normalization has to fail this test before it can make
/// the mismatch report lie.
///
/// NUL bytes are not covered — they cannot be passed through argv at all, so
/// no caller can reach the field with one.
#[test]
fn storage_stores_free_text_verbatim() {
    common::init_test_logging();
    let payloads = [
        ("trailing newline", "body of the note\n"),
        ("several trailing newlines", "body of the note\n\n\n"),
        ("leading newline", "\nbody of the note"),
        ("crlf line endings", "first line\r\nsecond line\r\n"),
        ("surrounding spaces", "   body of the note   "),
        ("internal runs of spaces", "a     b\t\tc"),
        // Combining acute vs precomposed: identical to a reader, different
        // bytes. NFC normalization anywhere would collapse the first to the
        // second.
        ("decomposed grapheme", "cafe\u{301} au lait"),
        ("precomposed grapheme", "caf\u{e9} au lait"),
        (
            "astral plane",
            "rocket \u{1f680} and flag \u{1f1f3}\u{1f1f4}",
        ),
        ("quotes and backslashes", "a \"quoted\" \\ path"),
    ];

    let mut checked = 0_usize;
    for (label, payload) in payloads {
        let (workspace, id) = workspace_with_issue("verbatim storage probe");
        let out = run_br(&workspace, ["update", &id, "--notes", payload], label);
        assert!(out.status.success(), "{label} write failed: {}", out.stderr);

        let stored = stored_field(&workspace, &id, "notes");
        assert_eq!(
            stored.as_bytes(),
            payload.as_bytes(),
            "{label}: storage altered the value; the landed-as-sent report \
             would fire on ordinary writes and must be compared against the \
             post-normalization value instead"
        );
        checked += 1;
    }

    // Count the comparisons that actually ran. A loop that silently iterated
    // zero times reports exactly like a loop where nothing failed.
    assert_eq!(
        checked,
        payloads.len(),
        "not every payload was checked: an absent check is not a passing one"
    );
}

// =============================================================================
// Part 1 — the refusal
// =============================================================================

/// The headline case: a perfectly valid, smaller payload annihilating the
/// field. No failure of any kind is required to trigger the original bug.
#[test]
fn notes_shrink_is_refused_and_field_survives() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("notes replacement probe");
    seed_notes(&workspace, &id, BLOCK_ONE);

    let out = run_br(&workspace, ["update", &id, "--notes", BLOCK_TWO], "shrink");

    assert!(
        !out.status.success(),
        "shrinking --notes must be refused, got success: {}",
        out.stdout
    );
    assert_eq!(
        out.status.code(),
        Some(4),
        "expected the validation exit code, stderr: {}",
        out.stderr
    );
    assert_field_intact(&workspace, &id, "notes", BLOCK_ONE);
}

/// beads1-21y5o: a failed `$(cat missing)` expands to the empty string, and
/// the field goes non-empty -> empty. Same transition family, same refusal.
#[test]
fn notes_cleared_to_empty_is_refused() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("empty substitution probe");
    seed_notes(&workspace, &id, BLOCK_ONE);

    let out = run_br(&workspace, ["update", &id, "--notes", ""], "wipe");

    assert!(
        !out.status.success(),
        "wiping --notes must be refused, got: {}",
        out.stdout
    );
    assert_field_intact(&workspace, &id, "notes", BLOCK_ONE);
}

/// NO LENGTH THRESHOLD ANYWHERE. Three preservation checks in this fleet
/// were found to skip their comparison when the sample was "too short to be
/// meaningful", which turned a full clobber into a verified pass. A field
/// holding four characters is still someone's content, so the guard is
/// exercised here at the smallest sizes that can shrink at all.
#[test]
fn tiny_fields_are_guarded_at_every_size() {
    common::init_test_logging();
    // (stored, incoming) pairs, from a 4-char field down to a 1-char one.
    let cases = [("abcd", "abc"), ("ab", "a"), ("x", "")];

    for (old, new) in cases {
        let (workspace, id) = workspace_with_issue("tiny field probe");
        seed_notes(&workspace, &id, old);

        let out = run_br(&workspace, ["update", &id, "--notes", new], "tiny");

        assert!(
            !out.status.success(),
            "{old:?} -> {new:?} must be refused regardless of size, got: {}",
            out.stdout
        );
        let err = structured_error(&out.stderr);
        assert_eq!(err["code"], "DESTRUCTIVE_UPDATE");
        assert_eq!(err["context"]["old_chars"], old.chars().count());
        assert_eq!(err["context"]["new_chars"], new.chars().count());
        assert_field_intact(&workspace, &id, "notes", old);
    }
}

/// Sizes are counted in characters, not bytes, so a multi-byte field is not
/// judged by its encoding. Shrinking from three characters to two is a
/// refusal even though the byte count is larger than the character count.
#[test]
fn multibyte_fields_are_measured_in_characters() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("unicode probe");
    seed_notes(&workspace, &id, "\u{4f60}\u{597d}\u{4e16}");

    let out = run_br(
        &workspace,
        ["update", &id, "--notes", "\u{4f60}\u{597d}"],
        "unicode",
    );

    assert!(
        !out.status.success(),
        "a 3-char -> 2-char shrink must be refused: {}",
        out.stdout
    );
    let err = structured_error(&out.stderr);
    assert_eq!(err["context"]["old_chars"], 3);
    assert_eq!(err["context"]["new_chars"], 2);
    assert_field_intact(&workspace, &id, "notes", "\u{4f60}\u{597d}\u{4e16}");
}

/// Every wholesale-settable free-text field reaches the guard, not just the
/// one the bug was reported against. Each flag is exercised end to end
/// through the real binary rather than assumed to share a code path.
#[test]
fn every_free_text_field_is_gated() {
    common::init_test_logging();
    let cases = [
        ("--description", "description"),
        ("--design", "design"),
        ("--acceptance-criteria", "acceptance_criteria"),
        ("--notes", "notes"),
    ];

    for (flag, field) in cases {
        let (workspace, id) = workspace_with_issue("field coverage probe");
        let seed = run_br(&workspace, ["update", &id, flag, BLOCK_ONE], "seed");
        assert!(
            seed.status.success(),
            "seeding {field} failed: {}",
            seed.stderr
        );
        assert_eq!(stored_field(&workspace, &id, field), BLOCK_ONE);

        let out = run_br(&workspace, ["update", &id, flag, BLOCK_TWO], "shrink");
        assert!(
            !out.status.success(),
            "shrinking {flag} must be refused, got: {}",
            out.stdout
        );
        let err = structured_error(&out.stderr);
        assert_eq!(err["code"], "DESTRUCTIVE_UPDATE", "wrong code for {flag}");
        assert_eq!(err["context"]["field"], field);
        assert_field_intact(&workspace, &id, field, BLOCK_ONE);
    }
}

/// The title is a free-text field too, and it is set wholesale by one flag.
#[test]
fn title_shrink_is_refused() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("a reasonably descriptive title");

    let out = run_br(&workspace, ["update", &id, "--title", "short"], "shrink");

    assert!(
        !out.status.success(),
        "shrinking --title must be refused, got: {}",
        out.stdout
    );
    assert_field_intact(&workspace, &id, "title", "a reasonably descriptive title");
}

/// The refusal must hand back everything needed to proceed: which field,
/// both sizes, and the exact flag. The caller's payload is never echoed as
/// lost — nothing was written, so it is still in their hands.
#[test]
fn refusal_is_actionable_and_structured() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("structured error probe");
    seed_notes(&workspace, &id, BLOCK_ONE);

    let out = run_br(&workspace, ["update", &id, "--notes", BLOCK_TWO], "shrink");
    assert!(!out.status.success());

    let err = structured_error(&out.stderr);
    assert_eq!(err["code"], "DESTRUCTIVE_UPDATE");
    assert_eq!(err["context"]["field"], "notes");
    assert_eq!(err["context"]["flag"], "--notes");
    assert_eq!(err["context"]["old_chars"], 62);
    assert_eq!(err["context"]["new_chars"], 27);
    assert_eq!(err["context"]["removed_chars"], 35);
    assert_eq!(err["context"]["override_flag"], "--replace");

    let message = err["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("notes") && message.contains("62") && message.contains("27"),
        "message must name the field and both sizes: {message}"
    );
    let hint = err["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("--replace"),
        "hint must name the exact flag to proceed: {hint}"
    );
    assert!(
        hint.contains("br comments add"),
        "hint must point at the append-only channel: {hint}"
    );
}

/// `--replace` is the opt-in, and it is the ONLY opt-in.
#[test]
fn replace_flag_allows_the_shrink() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("opt-in probe");
    seed_notes(&workspace, &id, BLOCK_ONE);

    let out = run_br(
        &workspace,
        ["update", &id, "--notes", BLOCK_TWO, "--replace"],
        "replace",
    );

    assert!(
        out.status.success(),
        "--replace should proceed: {}",
        out.stderr
    );
    assert_eq!(stored_field(&workspace, &id, "notes"), BLOCK_TWO);
    assert!(
        out.stdout.contains("notes: 62 \u{2192} 27 chars"),
        "an authorized shrink must still report its delta: {}",
        out.stdout
    );
}

/// `--force` on `br update` means "claim this issue even though it is
/// blocked". It must NOT double as authorization to destroy field content:
/// an agent bypassing a blocker check has said nothing about their notes.
#[test]
fn force_does_not_authorize_replacement() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("flag separation probe");
    seed_notes(&workspace, &id, BLOCK_ONE);

    let out = run_br(
        &workspace,
        ["update", &id, "--notes", BLOCK_TWO, "--force"],
        "force",
    );

    assert!(
        !out.status.success(),
        "--force must not authorize a destructive replace: {}",
        out.stdout
    );
    assert_field_intact(&workspace, &id, "notes", BLOCK_ONE);
}

/// A multi-id update either applies everywhere or nowhere. Refusing after
/// the first issue was already rewritten would still destroy data.
#[test]
fn multi_id_update_refuses_before_writing_anything() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut ids = Vec::new();
    for label in ["first target", "second target"] {
        let create = run_br(&workspace, ["create", label], "create");
        assert!(create.status.success(), "create failed: {}", create.stderr);
        ids.push(parse_created_id(&create.stdout));
    }
    // Only the SECOND issue has notes, so only it would shrink.
    seed_notes(&workspace, &ids[1], BLOCK_ONE);

    let out = run_br(
        &workspace,
        ["update", &ids[0], &ids[1], "--notes", BLOCK_TWO],
        "multi",
    );

    assert!(
        !out.status.success(),
        "the batch must be refused as a whole: {}",
        out.stdout
    );
    assert_field_intact(&workspace, &ids[1], "notes", BLOCK_ONE);
    assert_eq!(
        stored_field(&workspace, &ids[0], "notes"),
        "",
        "the first issue must not have been written before the refusal"
    );
}

// =============================================================================
// Part 1 — the transitions that must stay free
// =============================================================================

/// empty -> empty is a no-op and must pass silently. A guard that fires here
/// would be firing on a write that destroys nothing.
#[test]
fn empty_to_empty_is_allowed() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("no-op probe");

    let out = run_br(&workspace, ["update", &id, "--notes", ""], "noop");

    assert!(
        out.status.success(),
        "empty -> empty must be allowed: {}",
        out.stderr
    );
    assert_eq!(stored_field(&workspace, &id, "notes"), "");
}

/// empty -> non-empty is the first write of any field and must be untouched.
#[test]
fn empty_to_non_empty_is_allowed() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("first write probe");

    let out = run_br(&workspace, ["update", &id, "--notes", BLOCK_ONE], "first");

    assert!(out.status.success(), "first write failed: {}", out.stderr);
    assert_eq!(stored_field(&workspace, &id, "notes"), BLOCK_ONE);
}

/// Growth is every legitimate call. The guard must cost it nothing.
#[test]
fn growth_is_allowed() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("growth probe");
    seed_notes(&workspace, &id, BLOCK_ONE);
    let grown = format!("{BLOCK_ONE}\n\n{BLOCK_TWO}");

    let out = run_br(&workspace, ["update", &id, "--notes", &grown], "grow");

    assert!(
        out.status.success(),
        "growth must be allowed: {}",
        out.stderr
    );
    assert_eq!(stored_field(&workspace, &id, "notes"), grown);
}

/// Same-size-different-content is allowed by the agreed rule (it gates on the
/// transition, not on identity) but is reported as not retaining prior
/// content. Pinned so the asymmetry is deliberate rather than accidental.
#[test]
fn same_size_rewrite_is_allowed_but_flagged() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("same size probe");
    seed_notes(&workspace, &id, "aaaaaaaaaa");

    let out = run_br(&workspace, ["update", &id, "--notes", "bbbbbbbbbb"], "same");

    assert!(
        out.status.success(),
        "same-size write must be allowed: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("PRIOR CONTENT NOT RETAINED"),
        "a same-size rewrite drops content and must say so: {}",
        out.stdout
    );
}

// =============================================================================
// Part 2 — the delta on writes that ARE allowed
// =============================================================================

/// The success line must stop being identical between an append and an
/// annihilation. The number is present on growing writes too, so it is always
/// there to be checked.
#[test]
fn success_line_reports_delta_and_retention_on_growth() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("delta probe");
    seed_notes(&workspace, &id, BLOCK_ONE);
    let grown = format!("{BLOCK_ONE}\n\n{BLOCK_TWO}");

    let out = run_br(&workspace, ["update", &id, "--notes", &grown], "grow");

    assert!(out.status.success(), "growth failed: {}", out.stderr);
    assert!(
        out.stdout.contains("notes: 62 \u{2192} 91 chars"),
        "missing size delta: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("prior content retained"),
        "missing retention verdict: {}",
        out.stdout
    );
}

/// leader3's case, which the shrink guard CANNOT catch: a read-modify-write
/// whose preimage capture failed writes a LARGER value that dropped
/// everything. Allowed by design; reported so it is visible at the moment it
/// happens.
#[test]
fn growth_that_dropped_prior_content_is_flagged_on_the_success_line() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("failed preimage probe");
    let old = "x".repeat(300);
    let new = "y".repeat(400);
    seed_notes(&workspace, &id, &old);

    let out = run_br(&workspace, ["update", &id, "--notes", &new], "clobber");

    assert!(
        out.status.success(),
        "a growing write is allowed: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("notes: 300 \u{2192} 400 chars"),
        "missing size delta: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("PRIOR CONTENT NOT RETAINED"),
        "a write that grew while dropping prior content must say so: {}",
        out.stdout
    );
}

/// Agents parse JSON and never read the human line, so the same facts have to
/// be in both.
#[test]
fn json_output_carries_text_deltas() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("json delta probe");
    seed_notes(&workspace, &id, BLOCK_ONE);
    let grown = format!("{BLOCK_ONE}\n\n{BLOCK_TWO}");

    let out = run_br(
        &workspace,
        ["update", &id, "--notes", &grown, "--json"],
        "grow_json",
    );
    assert!(out.status.success(), "growth failed: {}", out.stderr);

    let payload = extract_json_payload(&out.stdout);
    let value: Value = serde_json::from_str(&payload).expect("update json");
    let deltas = value[0]["text_deltas"]
        .as_array()
        .unwrap_or_else(|| panic!("no text_deltas in {value}"));
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0]["field"], "notes");
    assert_eq!(deltas[0]["old_chars"], 62);
    assert_eq!(deltas[0]["new_chars"], 91);
    assert_eq!(deltas[0]["prior_content_retained"], true);
}

/// A non-text update keeps the old, terse success line: no delta clause is
/// invented for a write that touched no free-text field.
#[test]
fn non_text_updates_are_unchanged() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("status only probe");

    let out = run_br(
        &workspace,
        ["update", &id, "--status", "in_progress"],
        "status",
    );

    assert!(out.status.success(), "status update failed: {}", out.stderr);
    assert!(
        !out.stdout.contains("chars"),
        "no field delta should be reported: {}",
        out.stdout
    );
}

/// Several free-text fields in one command each get their own delta.
#[test]
fn multiple_fields_each_report_a_delta() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("multi field probe");

    let out = run_br(
        &workspace,
        [
            "update",
            &id,
            "--notes",
            BLOCK_ONE,
            "--description",
            BLOCK_TWO,
        ],
        "multi_field",
    );

    assert!(out.status.success(), "update failed: {}", out.stderr);
    assert!(
        out.stdout.contains("description: 0 \u{2192} 27 chars"),
        "missing description delta: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("notes: 0 \u{2192} 62 chars"),
        "missing notes delta: {}",
        out.stdout
    );
}
