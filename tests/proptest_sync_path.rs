//! Property-based tests for `src/sync/path.rs::validate_sync_path` invariants.
//!
//! Covers SYNC_SAFETY_INVARIANTS.md PC-1, PC-3, PC-RECOVERY, and the hard
//! invariant NGI-3 (git-path rejection).
//!
//! Created 2026-05-09 for beads_rust-yyxo (audit-driven test cleanup).
//!
//! Property summary:
//!   For ANY filename `f` and any path `p` constructed under the workspace,
//!   `validate_sync_path(p, beads_dir)` MUST return `Allowed` iff
//!   `p` is `.beads/`-rooted (lexically and after canonicalization) AND
//!   `f` matches the documented allowlist (extension or exact-name).
//!
//! The tests below cherry-pick the most-load-bearing properties:
//!   - prop_validate_rejects_arbitrary_dotgit_descendants (NGI-3)
//!   - prop_validate_rejects_traversal_outside_beads (PC-3)
//!   - prop_validate_accepts_canonical_jsonl (PC-1 happy path)
//!   - prop_validate_with_external_rejects_dotgit_unconditionally (NGI-3 even with allow_external)
//!
//! Note: this is a *test crate* — no production-code dependency on proptest.

use beads_rust::sync::path::{
    PathValidation, validate_sync_path, validate_sync_path_with_external,
};
use proptest::prelude::*;
use std::path::PathBuf;
use tempfile::TempDir;

fn fresh_workspace() -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("temp dir");
    let beads = temp.path().join(".beads");
    std::fs::create_dir_all(&beads).expect("create .beads");
    (temp, beads)
}

// PC-3 + NGI-3: any filename whose path component contains `.git` ANYWHERE
// in the .beads/-rooted ancestor chain MUST be rejected. The invariant is
// hard — even subdirectories whose names *contain* `.git` as a substring
// are not necessarily git, but exact-match `.git` components are.
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..ProptestConfig::default()
    })]

    // Property: any path of the form .beads/<segment>/.git/<inner> MUST
    // be rejected. Tests with random alphanumeric segments ensure the
    // rejection is structural, not a special-case for one filename.
    #[test]
    fn prop_validate_rejects_arbitrary_dotgit_descendants(
        outer in "[a-z0-9_]{1,8}",
        inner in "[A-Za-z0-9_.-]{1,16}",
    ) {
        let (_temp, beads) = fresh_workspace();
        let path = beads.join(&outer).join(".git").join(&inner);

        let result = validate_sync_path(&path, &beads);
        prop_assert!(
            !matches!(result, PathValidation::Allowed),
            "outer={outer:?} inner={inner:?} got {result:?} (must be GitPathAttempt-equivalent)"
        );
    }

    // Property: a path explicitly constructed with `..` traversing outside
    // .beads/ MUST be rejected, regardless of the suffix.
    // We use existing `.beads/` and a sibling external dir to make the
    // canonicalization meaningful.
    #[test]
    fn prop_validate_rejects_traversal_outside_beads(
        external_seg in "[a-z]{1,6}",
        suffix in "[a-z0-9]{1,6}\\.(jsonl|db|json|txt)",
    ) {
        let (temp, beads) = fresh_workspace();
        let external_dir = temp.path().join(format!("ext_{external_seg}"));
        std::fs::create_dir_all(&external_dir).expect("create external");
        // Path that lexically is .beads/../ext_<seg>/<suffix>
        let escape = beads.join("..").join(format!("ext_{external_seg}")).join(&suffix);

        let result = validate_sync_path(&escape, &beads);
        prop_assert!(
            !matches!(result, PathValidation::Allowed),
            "escape={escape:?} got {result:?} (traversal must be rejected)"
        );
    }

    // Property: a freshly-written `.jsonl` file under .beads/ with any
    // well-formed alphanumeric name MUST be accepted.
    #[test]
    fn prop_validate_accepts_canonical_jsonl(
        stem in "[a-z][a-z0-9_]{0,15}",
    ) {
        let (_temp, beads) = fresh_workspace();
        let path = beads.join(format!("{stem}.jsonl"));
        std::fs::write(&path, "{}\n").expect("write");

        let result = validate_sync_path(&path, &beads);
        prop_assert!(
            matches!(result, PathValidation::Allowed),
            "stem={stem:?} got {result:?} (must be Allowed)"
        );
    }

    // Property: even with `allow_external=true`, the explicit-external
    // validator MUST STILL reject any `.git/*` path. NGI-3 is hard.
    #[test]
    fn prop_validate_with_external_rejects_dotgit_unconditionally(
        outer in "[a-z]{1,6}",
        inner in "[a-z0-9]{1,8}",
    ) {
        let (temp, beads) = fresh_workspace();
        let external = temp.path().join("custom-jsonl-store");
        std::fs::create_dir_all(&external).expect("create external");
        let dotgit_path = external.join(&outer).join(".git").join(&inner);

        let result = validate_sync_path_with_external(&dotgit_path, &beads, true);
        prop_assert!(
            result.is_err(),
            "outer={outer:?} inner={inner:?} got {result:?} (must be rejected even with allow_external=true)"
        );
    }
}
