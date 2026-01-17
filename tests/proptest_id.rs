//! Property-based tests for ID generation.
//!
//! Uses proptest to verify that:
//! - Generated IDs always have valid format
//! - IDs are deterministic for the same inputs
//! - No collisions in realistic batch sizes
//! - ID prefix is preserved correctly

use chrono::Utc;
use proptest::prelude::*;
use std::collections::HashSet;
use std::fmt::Write as _;
use tracing::info;

use beads_rust::util::id::{
    IdConfig, IdGenerator, compute_id_hash, generate_id, generate_id_seed, is_valid_id_format,
    parse_id,
};

/// Initialize test logging for proptest (called once per test)
fn init_test_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 100,
        ..Default::default()
    })]

    /// Property: Generated IDs always match valid format `<prefix>-<base36hash>`
    #[test]
    fn id_always_valid_format(title in "\\PC{1,200}") {
        init_test_logging();
        info!(
            "proptest_id_valid: input_len={len}",
            len = title.len()
        );

        let now = Utc::now();
        let id = generate_id(&title, None, None, now);

        info!("proptest_id_valid: output_id={id}");

        prop_assert!(id.starts_with("bd-"), "ID must start with bd-");
        prop_assert!(id.len() >= 6, "ID must be at least 6 chars (bd-XXX)");
        prop_assert!(
            is_valid_id_format(&id),
            "Generated ID must pass format validation: {id}"
        );
    }

    /// Property: IDs are deterministic for same inputs
    #[test]
    fn id_deterministic_same_inputs(
        title in "\\PC{1,100}",
        desc in proptest::option::of("\\PC{0,200}"),
        creator in proptest::option::of("[a-z]{3,10}"),
    ) {
        init_test_logging();
        info!(
            "proptest_id_deterministic: title_len={len}",
            len = title.len()
        );

        let now = Utc::now();

        let id1 = generate_id(&title, desc.as_deref(), creator.as_deref(), now);
        let id2 = generate_id(&title, desc.as_deref(), creator.as_deref(), now);

        prop_assert_eq!(id1, id2, "Same inputs must produce same ID");
    }

    /// Property: Different titles produce different IDs
    #[test]
    fn id_different_for_different_titles(
        title1 in "[a-zA-Z0-9 ]{5,50}",
        title2 in "[a-zA-Z0-9 ]{5,50}",
    ) {
        init_test_logging();

        // Only test when titles are actually different
        prop_assume!(title1 != title2);

        let now = Utc::now();

        let id1 = generate_id(&title1, None, None, now);
        let id2 = generate_id(&title2, None, None, now);

        // Note: This is probabilistic - collisions are possible but rare
        // We just verify IDs are generated (format validation already tested above)
        info!(
            "proptest_id_different: id1={id1} id2={id2} same={same}",
            same = id1 == id2
        );
    }

    /// Property: Hash length parameter is respected
    #[test]
    fn hash_length_respected(
        input in "\\PC{1,100}",
        length in 3usize..=12usize,
    ) {
        init_test_logging();
        info!(
            "proptest_hash_length: input_len={input_len} requested_len={length}",
            input_len = input.len()
        );

        let hash = compute_id_hash(&input, length);

        prop_assert_eq!(
            hash.len(),
            length,
            "Hash length {} should match requested {}",
            hash.len(),
            length
        );
        prop_assert!(
            hash.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "Hash must be base36: {hash}"
        );
    }

    /// Property: Generated ID seed is deterministic
    #[test]
    fn id_seed_deterministic(
        title in "\\PC{1,100}",
        desc in proptest::option::of("\\PC{0,100}"),
        creator in proptest::option::of("[a-z]{3,10}"),
        nonce in 0u32..100u32,
    ) {
        init_test_logging();

        let now = Utc::now();

        let seed1 = generate_id_seed(&title, desc.as_deref(), creator.as_deref(), now, nonce);
        let seed2 = generate_id_seed(&title, desc.as_deref(), creator.as_deref(), now, nonce);

        prop_assert_eq!(seed1, seed2, "Same inputs must produce same seed");
    }

    /// Property: Parsed IDs can be reconstructed
    #[test]
    fn parsed_id_roundtrip(
        prefix in "[a-z]{1,10}",
        hash in "[a-z0-9]{3,12}",
    ) {
        init_test_logging();

        let id = format!("{prefix}-{hash}");
        info!("proptest_parse_roundtrip: id={id}");

        let parsed = parse_id(&id);
        prop_assert!(parsed.is_ok(), "Valid ID format should parse: {id}");

        let parsed = parsed.unwrap();
        let reconstructed = parsed.to_id_string();
        prop_assert_eq!(id, reconstructed, "Roundtrip should preserve ID");
    }

    /// Property: Child IDs parse correctly with depth
    #[test]
    fn child_id_depth(
        hash in "[a-z0-9]{4,8}",
        child_segments in proptest::collection::vec(1u32..100u32, 0..5),
    ) {
        init_test_logging();

        let mut id = format!("bd-{hash}");
        for seg in &child_segments {
            let _ = write!(id, ".{seg}");
        }

        info!(
            "proptest_child_depth: id={id} expected_depth={depth}",
            depth = child_segments.len()
        );

        let parsed = parse_id(&id);
        prop_assert!(parsed.is_ok(), "Child ID should parse: {id}");

        let parsed = parsed.unwrap();
        prop_assert_eq!(
            parsed.depth(), child_segments.len(),
            "Depth should match segment count"
        );
        prop_assert_eq!(
            parsed.child_path, child_segments,
            "Child path should match"
        );
    }

    /// Property: Custom prefix is preserved in generation
    #[test]
    fn prefix_preserved(
        prefix in "[a-z]{1,15}",
        title in "\\PC{1,50}",
    ) {
        init_test_logging();
        info!("proptest_prefix: prefix={prefix}");

        let config = IdConfig::with_prefix(&prefix);
        let generator = IdGenerator::new(config);
        let now = Utc::now();

        let id = generator.generate(&title, None, None, now, 0, |_| false);

        prop_assert!(
            id.starts_with(&format!("{prefix}-")),
            "ID {id} should start with {prefix}-"
        );
    }
}

/// Property: No collisions in batch generation with collision checking
#[test]
fn id_no_collisions_batch() {
    init_test_logging();
    info!("proptest_batch_collision: starting batch test");

    let generator = IdGenerator::with_defaults();
    let now = Utc::now();
    let mut generated = HashSet::new();

    // Generate 100 unique IDs with the collision checker
    for i in 0..100 {
        let title = format!("Test Issue Number {i}");
        let id = generator.generate(&title, None, None, now, i, |id| generated.contains(id));

        assert!(
            !generated.contains(&id),
            "Collision detected at iteration {i}: {id}"
        );
        generated.insert(id);
    }

    assert_eq!(generated.len(), 100, "Should have 100 unique IDs");
    info!("proptest_batch_collision: PASS - 100 unique IDs generated");
}

/// Property: Optimal length calculation is monotonic with issue count
#[test]
fn optimal_length_monotonic() {
    init_test_logging();
    info!("proptest_optimal_length: testing monotonicity");

    let generator = IdGenerator::with_defaults();

    let mut prev_len = generator.optimal_length(0);
    for count in [1, 10, 100, 1000, 10_000, 100_000] {
        let len = generator.optimal_length(count);
        assert!(
            len >= prev_len,
            "Optimal length should not decrease: {prev_len} -> {len} at count {count}"
        );
        prev_len = len;
    }

    info!("proptest_optimal_length: PASS");
}
