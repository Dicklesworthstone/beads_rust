//! Guard against accidental destruction of wholesale-settable free-text fields.
//!
//! `br update --notes/--description/--design/--acceptance-criteria/--title`
//! all have REPLACE semantics: the value supplied on the command line becomes
//! the whole field. That is the right semantics — `br comments` is the
//! append-only channel — but it means a caller who *intended* to append
//! silently annihilates whatever was there, and the success line looks exactly
//! the same either way.
//!
//! This module supplies the two primitives that fix that:
//!
//! 1. [`TextChange::is_destructive_shrink`] — the TRANSITION test the update
//!    command refuses on. Gating on the transition rather than on the value is
//!    deliberate: it fires only on callers actively removing content and costs
//!    every ordinary write nothing.
//!
//!    | transition                  | verdict                        |
//!    |-----------------------------|--------------------------------|
//!    | non-empty -> empty          | refuse without the opt-in      |
//!    | non-empty -> strictly smaller | refuse without the opt-in    |
//!    | empty -> empty              | allow (no-op)                  |
//!    | anything -> same-or-larger  | allow                          |
//!
//! 2. [`TextChange::prior_retained`] — a containment test used purely for
//!    REPORTING. The shrink test is a cheap PROXY for destruction, not a
//!    detector of it: a read-modify-write whose preimage capture failed writes
//!    a *larger* value that still dropped everything that was there. That case
//!    grows and is therefore allowed; the containment flag makes it visible on
//!    the success line at the moment it happens.
//!
//!    Containment is deliberately NOT a refusal: `--notes` exists for curated
//!    current state, and legitimately rewording that state produces a value
//!    that does not contain the old one. It is high-signal as reporting and
//!    unusable as a gate.
//!
//! 3. [`TextChange::landed_as_sent`] — after the write, whether what is now
//!    stored is what bd was asked to store. Found by mutation-testing the
//!    write path: with a write that truncated its input, a 5-char field
//!    handed 16 chars reported `5 -> 5 chars, prior content retained` and
//!    exited 0. Every word of that was true and the caller's new content was
//!    gone. bd holds both the requested value and the stored result, so this
//!    comparison is free; it is reporting only, because the write has already
//!    happened by the time it can be made.

/// A free-text field that `br update` can set wholesale.
///
/// Every variant here is a field whose entire contents are replaced by a
/// single CLI flag, and therefore a field that can be destroyed by one
/// well-formed command.
///
/// # Why this list and not every settable string
///
/// THE SHRINK TEST IS ONLY MEANINGFUL FOR FIELDS WHOSE CONTENT ACCUMULATES.
/// It is a proxy for "prose that was built up over time got destroyed". An
/// identifier does not accumulate — it is replaced by nature, and a shorter
/// replacement is a normal correction. `--external-ref` (a long tracker URL
/// swapped for a short canonical one) and `--session` are therefore
/// deliberately absent: not because they matter less, but because on an
/// identifier "smaller" carries no information about destruction, and a guard
/// that cries wolf on legitimate corrections teaches callers to pass
/// `--replace` reflexively — which would cost the protection on the five
/// fields where it IS meaningful.
///
/// `--assignee`, `--owner`, `--due` and `--defer` are absent for a related
/// reason: they already document "empty string clears" as an intentional
/// affordance. That precedent is what this guard generalises — an explicit
/// clear, where clearing prose used to be implicit and silent. They remain
/// reachable by the same shell accident (`--assignee "$(cmd-that-fails)"`
/// unassigns), which is a KNOWN AND ACCEPTED residual: a single recoverable
/// word, visible in the next `br show`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextField {
    /// `--title`
    Title,
    /// `--description` (alias `--body`)
    Description,
    /// `--design`
    Design,
    /// `--acceptance-criteria` (alias `--acceptance`)
    AcceptanceCriteria,
    /// `--notes`
    Notes,
}

impl TextField {
    /// Every wholesale-settable free-text field, in CLI declaration order.
    pub const ALL: [Self; 5] = [
        Self::Title,
        Self::Description,
        Self::Design,
        Self::AcceptanceCriteria,
        Self::Notes,
    ];

    /// Stable field name, as used in JSON output and error context.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Description => "description",
            Self::Design => "design",
            Self::AcceptanceCriteria => "acceptance_criteria",
            Self::Notes => "notes",
        }
    }

    /// The CLI flag that sets this field, including leading dashes.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Title => "--title",
            Self::Description => "--description",
            Self::Design => "--design",
            Self::AcceptanceCriteria => "--acceptance-criteria",
            Self::Notes => "--notes",
        }
    }

    /// Recover a field from its stable [`name`](Self::name).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|field| field.name() == name)
    }

    /// Whether "append instead of replacing" is sensible advice for this field.
    ///
    /// False for the title: a title is a one-line label, not a running record,
    /// so pointing a caller who is retitling an issue at `br comments add` is
    /// advice they cannot use. Every other field here is prose that a caller
    /// may well have meant to add to.
    #[must_use]
    pub const fn has_append_alternative(self) -> bool {
        !matches!(self, Self::Title)
    }
}

/// The measured transition of one free-text field from an old to a new value.
///
/// Sizes are counted in `char`s (Unicode scalar values), not bytes, so the
/// numbers on the success line match what a human counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextChange {
    /// Which field this describes.
    pub field: TextField,
    /// Size of the stored value before the write.
    pub old_chars: usize,
    /// Size of the value that would be (or was) stored.
    pub new_chars: usize,
    /// Whether the old value had any non-whitespace content.
    pub had_content: bool,
    /// Whether the old value appears verbatim inside the new one.
    ///
    /// `None` when the old value had no content to retain.
    pub prior_retained: Option<bool>,
    /// Size of the value bd was ASKED to store, when that is known.
    ///
    /// `None` before the write, where the requested value and the new value
    /// are the same thing.
    pub requested_chars: Option<usize>,
    /// Whether the stored value equals the value bd was asked to store.
    ///
    /// `None` before the write. `Some(false)` means the write did not land as
    /// sent — the caller's content was altered or dropped between the command
    /// line and the database.
    pub landed_as_sent: Option<bool>,
}

impl TextChange {
    /// Measure the transition from `old` to `new` for `field`.
    #[must_use]
    pub fn measure(field: TextField, old: &str, new: &str) -> Self {
        let had_content = !old.trim().is_empty();
        Self {
            field,
            old_chars: old.chars().count(),
            new_chars: new.chars().count(),
            had_content,
            // Containment is a plain substring search; both operands are
            // single fields (the largest observed in this fleet is ~122KB),
            // so this is a two-way search over a few hundred KB at worst.
            prior_retained: if had_content {
                Some(new.contains(old))
            } else {
                None
            },
            requested_chars: None,
            landed_as_sent: None,
        }
    }

    /// Measure a write that has already happened.
    ///
    /// `stored` is what the database now holds; `requested` is what bd was
    /// asked to store. They differ when something between the command line and
    /// the row altered the value, which the size delta alone can report as a
    /// perfectly healthy no-op.
    #[must_use]
    pub fn measure_write(field: TextField, old: &str, stored: &str, requested: &str) -> Self {
        Self {
            requested_chars: Some(requested.chars().count()),
            landed_as_sent: Some(stored == requested),
            ..Self::measure(field, old, stored)
        }
    }

    /// Whether this write would shrink a field that currently has content.
    ///
    /// This is the transition `br update` refuses without an explicit opt-in.
    /// It covers both `non-empty -> empty` and `non-empty -> strictly smaller`.
    #[must_use]
    pub const fn is_destructive_shrink(&self) -> bool {
        self.had_content && self.new_chars < self.old_chars
    }

    /// How many characters this write removes, or 0 if it does not shrink.
    #[must_use]
    pub const fn removed_chars(&self) -> usize {
        self.old_chars.saturating_sub(self.new_chars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measure(old: &str, new: &str) -> TextChange {
        TextChange::measure(TextField::Notes, old, new)
    }

    #[test]
    fn non_empty_to_empty_is_destructive() {
        // beads1-21y5o: a failed `$(cat missing)` expands to the empty string.
        let change = measure("FIRST BLOCK: operator ruling, do not revoke.", "");
        assert!(change.is_destructive_shrink());
        assert_eq!(change.new_chars, 0);
        assert_eq!(change.removed_chars(), 44);
        assert_eq!(change.prior_retained, Some(false));
    }

    #[test]
    fn non_empty_to_strictly_smaller_is_destructive() {
        // beads1-1euci: a perfectly valid payload that happens to be smaller.
        let change = measure(
            "FIRST BLOCK: operator ruling, do not revoke. Evidence A, B, C.",
            "SECOND BLOCK: handoff note.",
        );
        assert!(change.is_destructive_shrink());
        assert_eq!(change.old_chars, 62);
        assert_eq!(change.new_chars, 27);
        assert_eq!(change.removed_chars(), 35);
    }

    #[test]
    fn empty_to_empty_is_allowed() {
        let change = measure("", "");
        assert!(!change.is_destructive_shrink());
        assert!(!change.had_content);
        assert_eq!(change.prior_retained, None);
    }

    #[test]
    fn whitespace_only_old_value_has_nothing_to_destroy() {
        let change = measure("   \n\t", "");
        assert!(!change.is_destructive_shrink());
        assert_eq!(change.prior_retained, None);
    }

    #[test]
    fn empty_to_non_empty_is_allowed() {
        let change = measure("", "brand new content");
        assert!(!change.is_destructive_shrink());
        assert_eq!(change.new_chars, 17);
    }

    #[test]
    fn same_size_is_allowed_even_when_content_differs() {
        // Deliberate: the agreed rule gates on size, not on identity. A
        // same-size rewrite is reported as not retaining prior content but is
        // not refused.
        let change = measure("aaaa", "bbbb");
        assert!(!change.is_destructive_shrink());
        assert_eq!(change.prior_retained, Some(false));
    }

    #[test]
    fn growth_is_allowed_and_reports_retention() {
        let change = measure("FIRST BLOCK", "FIRST BLOCK\n\nSECOND BLOCK");
        assert!(!change.is_destructive_shrink());
        assert_eq!(change.prior_retained, Some(true));
    }

    #[test]
    fn growth_that_dropped_prior_content_is_reported_but_allowed() {
        // leader3's case: the preimage capture failed, so the "append" was
        // built on an empty base. Bigger, and everything that was there is
        // gone. The shrink guard cannot see this; the containment flag can.
        let old = "x".repeat(3000);
        let new = "y".repeat(4000);
        let change = measure(&old, &new);
        assert!(!change.is_destructive_shrink());
        assert_eq!(change.prior_retained, Some(false));
    }

    #[test]
    fn sizes_are_counted_in_chars_not_bytes() {
        // Four 3-byte chars replaced by three 3-byte chars: a shrink of one
        // char, not of three bytes.
        let change = measure("日本語版", "日本語");
        assert_eq!(change.old_chars, 4);
        assert_eq!(change.new_chars, 3);
        assert!(change.is_destructive_shrink());
    }

    #[test]
    fn a_write_that_landed_verbatim_is_reported_as_such() {
        let change = TextChange::measure_write(TextField::Notes, "abcde", "abcdefg", "abcdefg");
        assert_eq!(change.landed_as_sent, Some(true));
        assert_eq!(change.requested_chars, Some(7));
    }

    #[test]
    fn a_write_that_did_not_land_as_sent_is_visible() {
        // Mutation-test finding: the write path truncated its input, so the
        // field neither shrank nor lost prior content, and 11 of the caller's
        // characters silently never arrived. Sizes alone call this healthy.
        let change =
            TextChange::measure_write(TextField::Notes, "abcde", "abcde", "abcdefghijKLMNOP");
        assert!(!change.is_destructive_shrink());
        assert_eq!(change.prior_retained, Some(true));
        assert_eq!(change.old_chars, 5);
        assert_eq!(change.new_chars, 5);
        assert_eq!(change.landed_as_sent, Some(false));
        assert_eq!(change.requested_chars, Some(16));
    }

    #[test]
    fn pre_write_measurements_make_no_claim_about_landing() {
        let change = measure("abcde", "abcdefg");
        assert_eq!(change.landed_as_sent, None);
        assert_eq!(change.requested_chars, None);
    }

    #[test]
    fn field_names_and_flags_are_stable() {
        assert_eq!(TextField::Notes.name(), "notes");
        assert_eq!(TextField::Notes.flag(), "--notes");
        assert_eq!(
            TextField::AcceptanceCriteria.flag(),
            "--acceptance-criteria"
        );
        assert_eq!(TextField::ALL.len(), 5);
    }
}
