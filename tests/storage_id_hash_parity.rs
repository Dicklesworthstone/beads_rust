//! ID generation and content hash parity tests.
//!
//! These tests verify:
//! - Deterministic ID generation with known inputs
//! - Content hash behavior matches bd spec
//! - Hash length strategy produces expected lengths
//! - Prefix handling is correct
//!
//! The fixtures in this file can be compared against legacy bd outputs
//! to verify compatibility.

use chrono::{TimeZone, Utc};

use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::util::id::{
    IdConfig, IdGenerator, compute_id_hash, generate_id_seed, is_valid_id_format, parse_id,
};
use beads_rust::util::{ContentHashable, content_hash, content_hash_from_parts};

// =============================================================================
// ID GENERATION FIXTURES
// =============================================================================

/// Test that ID seed generation is deterministic with known inputs.
#[test]
fn id_seed_deterministic_fixture() {
    let title = "Fix authentication bug";
    let description = Some("Users are getting logged out unexpectedly");
    let creator = Some("alice");
    // Use a fixed timestamp for reproducibility
    let created_at = Utc.with_ymd_and_hms(2026, 1, 15, 10, 30, 0).unwrap();
    let nonce = 0u32;

    let seed = generate_id_seed(title, description, creator, created_at, nonce);

    // Verify seed format: title|description|creator|timestamp_nanos|nonce
    assert!(
        seed.starts_with("Fix authentication bug|"),
        "Seed should start with title"
    );
    assert!(
        seed.contains("|Users are getting logged out unexpectedly|"),
        "Seed should contain description"
    );
    assert!(seed.contains("|alice|"), "Seed should contain creator");
    assert!(seed.ends_with("|0"), "Seed should end with nonce 0");

    // Verify determinism - same inputs produce same seed
    let seed2 = generate_id_seed(title, description, creator, created_at, nonce);
    assert_eq!(seed, seed2, "Seeds must be deterministic");

    // Verify different nonce produces different seed
    let seed_nonce1 = generate_id_seed(title, description, creator, created_at, 1);
    assert_ne!(
        seed, seed_nonce1,
        "Different nonce should produce different seed"
    );
}

/// Test hash computation produces consistent base36 output.
#[test]
fn hash_computation_base36_fixture() {
    let input = "Fix authentication bug|Users are getting logged out unexpectedly|alice|1736936800000000000|0";

    // Test various lengths
    let hash3 = compute_id_hash(input, 3);
    let hash4 = compute_id_hash(input, 4);
    let hash8 = compute_id_hash(input, 8);

    // Verify lengths
    assert_eq!(hash3.len(), 3);
    assert_eq!(hash4.len(), 4);
    assert_eq!(hash8.len(), 8);

    // Verify all are base36 (lowercase alphanumeric)
    assert!(
        hash3
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    );
    assert!(
        hash4
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    );
    assert!(
        hash8
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    );

    // Verify determinism
    assert_eq!(compute_id_hash(input, 3), hash3);
    assert_eq!(compute_id_hash(input, 8), hash8);

    // Verify prefix relationship (longer hashes should be extensions)
    assert!(hash4.starts_with(&hash3), "hash4 should extend hash3");
    assert!(hash8.starts_with(&hash4), "hash8 should extend hash4");
}

/// Test optimal length calculation follows birthday problem math.
#[test]
fn optimal_length_birthday_problem() {
    let generator = IdGenerator::with_defaults();

    // For small DBs, minimum length (3) should suffice
    assert_eq!(generator.optimal_length(0), 3, "Empty DB uses min length");
    assert_eq!(generator.optimal_length(10), 3, "10 issues uses min length");
    assert_eq!(
        generator.optimal_length(50),
        3,
        "50 issues still uses min length"
    );

    // As DB grows, length should increase
    let len_100 = generator.optimal_length(100);
    let len_1000 = generator.optimal_length(1000);
    let len_10000 = generator.optimal_length(10000);

    assert!(len_100 >= 3, "100 issues needs at least 3 chars");
    assert!(len_1000 >= len_100, "1000 issues needs >= length of 100");
    assert!(
        len_10000 >= len_1000,
        "10000 issues needs >= length of 1000"
    );

    // Very large DBs should use max length (8)
    let len_million = generator.optimal_length(1_000_000);
    assert!(len_million <= 8, "Length should not exceed max (8)");
}

/// Test prefix configuration and ID format.
#[test]
fn prefix_configuration_fixture() {
    let created_at = Utc.with_ymd_and_hms(2026, 1, 15, 10, 30, 0).unwrap();

    // Explicit "bd" prefix (there is no ambient default prefix anymore;
    // every generator must be constructed with an explicit prefix).
    let default_config = IdConfig::with_prefix("bd");
    let default_gen = IdGenerator::new(default_config);
    let id_default = default_gen.generate("Test", None, None, created_at, 0, |_| false);
    assert!(
        id_default.starts_with("bd-"),
        "Explicit bd prefix should produce bd-"
    );
    assert!(is_valid_id_format(&id_default));

    // Custom prefix
    let custom_config = IdConfig::with_prefix("myproject");
    let custom_gen = IdGenerator::new(custom_config);
    let id_custom = custom_gen.generate("Test", None, None, created_at, 0, |_| false);
    assert!(
        id_custom.starts_with("myproject-"),
        "Custom prefix should be myproject-"
    );
    assert!(is_valid_id_format(&id_custom));

    // Hyphenated prefix
    let hyphen_config = IdConfig::with_prefix("my-project");
    let hyphen_gen = IdGenerator::new(hyphen_config);
    let id_hyphen = hyphen_gen.generate("Test", None, None, created_at, 0, |_| false);
    assert!(
        id_hyphen.starts_with("my-project-"),
        "Hyphenated prefix should work"
    );
    assert!(is_valid_id_format(&id_hyphen));
}

/// Test collision handling increases nonce and length.
#[test]
fn collision_handling_fixture() {
    let generator = IdGenerator::new(IdConfig::with_prefix("bd"));
    let created_at = Utc.with_ymd_and_hms(2026, 1, 15, 10, 30, 0).unwrap();

    let mut generated: Vec<String> = Vec::new();
    let exists = |id: &str| generated.contains(&id.to_string());

    // Generate first ID
    let id1 = generator.generate("Test Issue", None, None, created_at, 0, exists);
    generated.push(id1.clone());

    // Generate second ID with same inputs - collision checker should force different ID
    let id2 = generator.generate("Test Issue", None, None, created_at, 0, |id| {
        generated.contains(&id.to_string())
    });
    generated.push(id2.clone());

    // They should be different due to nonce increment
    assert_ne!(id1, id2, "Collision handling should produce unique IDs");

    // Both should be valid
    assert!(is_valid_id_format(&id1));
    assert!(is_valid_id_format(&id2));
}

/// Test ID parsing with various formats.
#[test]
fn id_parsing_fixtures() {
    // Simple ID
    let parsed = parse_id("bd-abc123").unwrap();
    assert_eq!(parsed.prefix, "bd");
    assert_eq!(parsed.hash, "abc123");
    assert!(parsed.child_path.is_empty());
    assert!(parsed.is_root());

    // Child ID
    let child = parse_id("bd-abc123.1").unwrap();
    assert_eq!(child.prefix, "bd");
    assert_eq!(child.hash, "abc123");
    assert_eq!(child.child_path, vec![1]);
    assert!(!child.is_root());
    assert_eq!(child.depth(), 1);

    // Grandchild ID
    let grandchild = parse_id("bd-abc123.1.2.3").unwrap();
    assert_eq!(grandchild.child_path, vec![1, 2, 3]);
    assert_eq!(grandchild.depth(), 3);

    // Hyphenated prefix
    let hyphen = parse_id("my-project-abc123").unwrap();
    assert_eq!(hyphen.prefix, "my-project");
    assert_eq!(hyphen.hash, "abc123");

    // Project-style prefix (common in real usage)
    let project = parse_id("beads_rust-3ea7").unwrap();
    assert_eq!(project.prefix, "beads_rust");
    assert_eq!(project.hash, "3ea7");

    // Invalid IDs should fail
    assert!(parse_id("nohash").is_err());
    assert!(parse_id("bd-").is_err());
    assert!(parse_id("bd-ABC").is_err()); // Uppercase not allowed
}

// =============================================================================
// CONTENT HASH FIXTURES
// =============================================================================

/// Test content hash determinism with known inputs.
#[test]
fn content_hash_deterministic_fixture() {
    let hash1 = content_hash_from_parts(
        "Fix authentication bug",
        Some("Users are getting logged out unexpectedly"),
        None,
        None,
        None,
        &Status::Open,
        &Priority::HIGH,
        &IssueType::Bug,
        None,
        None,
        Some("alice"),
        None,
        None,
        false,
        false,
    );

    let hash2 = content_hash_from_parts(
        "Fix authentication bug",
        Some("Users are getting logged out unexpectedly"),
        None,
        None,
        None,
        &Status::Open,
        &Priority::HIGH,
        &IssueType::Bug,
        None,
        None,
        Some("alice"),
        None,
        None,
        false,
        false,
    );

    assert_eq!(hash1, hash2, "Content hash must be deterministic");
    assert_eq!(hash1.len(), 64, "SHA256 hash should be 64 hex chars");
    assert!(
        hash1.chars().all(|c| c.is_ascii_hexdigit()),
        "Hash must be hex"
    );
}

/// Test content hash changes with title.
#[test]
fn content_hash_title_sensitivity() {
    let hash1 = content_hash_from_parts(
        "Title One",
        None,
        None,
        None,
        None,
        &Status::Open,
        &Priority::MEDIUM,
        &IssueType::Task,
        None,
        None,
        None,
        None,
        None,
        false,
        false,
    );

    let hash2 = content_hash_from_parts(
        "Title Two",
        None,
        None,
        None,
        None,
        &Status::Open,
        &Priority::MEDIUM,
        &IssueType::Task,
        None,
        None,
        None,
        None,
        None,
        false,
        false,
    );

    assert_ne!(
        hash1, hash2,
        "Different titles should produce different hashes"
    );
}

/// Test content hash changes with status.
#[test]
fn content_hash_status_sensitivity() {
    let base_args = (
        "Test Issue",
        None::<&str>,
        None::<&str>,
        None::<&str>,
        None::<&str>,
        &Priority::MEDIUM,
        &IssueType::Task,
        None::<&str>,
        None::<&str>,
        None::<&str>,
        None::<&str>,
        None::<&str>,
        false,
        false,
    );

    let hash_open = content_hash_from_parts(
        base_args.0,
        base_args.1,
        base_args.2,
        base_args.3,
        base_args.4,
        &Status::Open,
        base_args.5,
        base_args.6,
        base_args.7,
        base_args.8,
        base_args.9,
        base_args.10,
        base_args.11,
        base_args.12,
        base_args.13,
    );

    let hash_closed = content_hash_from_parts(
        base_args.0,
        base_args.1,
        base_args.2,
        base_args.3,
        base_args.4,
        &Status::Closed,
        base_args.5,
        base_args.6,
        base_args.7,
        base_args.8,
        base_args.9,
        base_args.10,
        base_args.11,
        base_args.12,
        base_args.13,
    );

    assert_ne!(
        hash_open, hash_closed,
        "Different status should produce different hashes"
    );
}

/// Test content hash changes with priority.
#[test]
fn content_hash_priority_sensitivity() {
    let hash_p1 = content_hash_from_parts(
        "Test",
        None,
        None,
        None,
        None,
        &Status::Open,
        &Priority::HIGH,
        &IssueType::Task,
        None,
        None,
        None,
        None,
        None,
        false,
        false,
    );

    let hash_p3 = content_hash_from_parts(
        "Test",
        None,
        None,
        None,
        None,
        &Status::Open,
        &Priority::LOW,
        &IssueType::Task,
        None,
        None,
        None,
        None,
        None,
        false,
        false,
    );

    assert_ne!(
        hash_p1, hash_p3,
        "Different priority should produce different hashes"
    );
}

/// Test content hash changes with issue type.
#[test]
fn content_hash_type_sensitivity() {
    let hash_bug = content_hash_from_parts(
        "Test",
        None,
        None,
        None,
        None,
        &Status::Open,
        &Priority::MEDIUM,
        &IssueType::Bug,
        None,
        None,
        None,
        None,
        None,
        false,
        false,
    );

    let hash_feature = content_hash_from_parts(
        "Test",
        None,
        None,
        None,
        None,
        &Status::Open,
        &Priority::MEDIUM,
        &IssueType::Feature,
        None,
        None,
        None,
        None,
        None,
        false,
        false,
    );

    assert_ne!(
        hash_bug, hash_feature,
        "Different type should produce different hashes"
    );
}

/// Test content hash includes boolean flags.
#[test]
fn content_hash_boolean_sensitivity() {
    let hash_default = content_hash_from_parts(
        "Test",
        None,
        None,
        None,
        None,
        &Status::Open,
        &Priority::MEDIUM,
        &IssueType::Task,
        None,
        None,
        None,
        None,
        None,
        false,
        false,
    );

    let hash_pinned = content_hash_from_parts(
        "Test",
        None,
        None,
        None,
        None,
        &Status::Open,
        &Priority::MEDIUM,
        &IssueType::Task,
        None,
        None,
        None,
        None,
        None,
        true,
        false, // pinned=true
    );

    let hash_template = content_hash_from_parts(
        "Test",
        None,
        None,
        None,
        None,
        &Status::Open,
        &Priority::MEDIUM,
        &IssueType::Task,
        None,
        None,
        None,
        None,
        None,
        false,
        true, // is_template=true
    );

    assert_ne!(hash_default, hash_pinned, "pinned flag should affect hash");
    assert_ne!(
        hash_default, hash_template,
        "is_template flag should affect hash"
    );
    assert_ne!(
        hash_pinned, hash_template,
        "Different flags should produce different hashes"
    );
}

/// Test content hash via Issue trait.
#[test]
fn content_hash_trait_implementation() {
    let mut issue = Issue {
        id: "bd-test123".to_string(),
        content_hash: None,
        title: "Test Issue".to_string(),
        description: Some("Description".to_string()),
        design: None,
        acceptance_criteria: None,
        notes: None,
        status: Status::Open,
        priority: Priority::MEDIUM,
        issue_type: IssueType::Task,
        assignee: None,
        owner: None,
        estimated_minutes: None,
        created_at: Utc::now(),
        created_by: Some("tester".to_string()),
        updated_at: Utc::now(),
        closed_at: None,
        close_reason: None,
        closed_by_session: None,
        due_at: None,
        defer_until: None,
        external_ref: None,
        source_system: None,
        source_repo: None,
        deleted_at: None,
        deleted_by: None,
        delete_reason: None,
        original_type: None,
        compaction_level: None,
        compacted_at: None,
        compacted_at_commit: None,
        original_size: None,
        sender: None,
        ephemeral: false,
        pinned: false,
        is_template: false,
        labels: vec![],
        dependencies: vec![],
        comments: vec![],
    };

    let hash_trait = issue.content_hash();
    let hash_direct = content_hash(&issue);

    assert_eq!(
        hash_trait, hash_direct,
        "Trait impl should match direct call"
    );

    // Verify ID doesn't affect content hash
    let hash_before = issue.content_hash();
    issue.id = "bd-different".to_string();
    let hash_after = issue.content_hash();

    assert_eq!(hash_before, hash_after, "ID should not affect content hash");

    // Verify timestamps don't affect content hash
    let hash_t1 = issue.content_hash();
    issue.updated_at = Utc::now();
    issue.created_at = Utc::now();
    let hash_t2 = issue.content_hash();

    assert_eq!(
        hash_t1, hash_t2,
        "Timestamps should not affect content hash"
    );
}

/// Test content hash with optional fields.
#[test]
fn content_hash_optional_fields() {
    // Base hash with no optional fields
    let hash_none = content_hash_from_parts(
        "Test",
        None,
        None,
        None,
        None,
        &Status::Open,
        &Priority::MEDIUM,
        &IssueType::Task,
        None,
        None,
        None,
        None,
        None,
        false,
        false,
    );

    // With description
    let hash_desc = content_hash_from_parts(
        "Test",
        Some("Description"),
        None,
        None,
        None,
        &Status::Open,
        &Priority::MEDIUM,
        &IssueType::Task,
        None,
        None,
        None,
        None,
        None,
        false,
        false,
    );

    // With design
    let hash_design = content_hash_from_parts(
        "Test",
        None,
        Some("Design notes"),
        None,
        None,
        &Status::Open,
        &Priority::MEDIUM,
        &IssueType::Task,
        None,
        None,
        None,
        None,
        None,
        false,
        false,
    );

    // With external_ref
    let hash_ext = content_hash_from_parts(
        "Test",
        None,
        None,
        None,
        None,
        &Status::Open,
        &Priority::MEDIUM,
        &IssueType::Task,
        None,
        None,
        None,
        Some("github:org/repo#123"),
        None,
        false,
        false,
    );

    assert_ne!(hash_none, hash_desc);
    assert_ne!(hash_none, hash_design);
    assert_ne!(hash_none, hash_ext);
}

// =============================================================================
// PREFIX CHANGE TESTS
// =============================================================================

/// Test that prefix changes are handled correctly.
#[test]
fn prefix_change_id_generation() {
    let created_at = Utc.with_ymd_and_hms(2026, 1, 15, 10, 30, 0).unwrap();

    // Generate IDs with different prefixes
    let gen_bd = IdGenerator::new(IdConfig::with_prefix("bd"));
    let gen_proj = IdGenerator::new(IdConfig::with_prefix("myproject"));

    let id_bd = gen_bd.generate("Test", None, None, created_at, 0, |_| false);
    let id_proj = gen_proj.generate("Test", None, None, created_at, 0, |_| false);

    // Same content but different prefixes
    assert!(id_bd.starts_with("bd-"));
    assert!(id_proj.starts_with("myproject-"));

    // The hash portion should be the same (same inputs)
    let bd_hash = id_bd.strip_prefix("bd-").unwrap();
    let proj_hash = id_proj.strip_prefix("myproject-").unwrap();
    assert_eq!(
        bd_hash, proj_hash,
        "Same inputs should produce same hash regardless of prefix"
    );
}

/// Test prefix validation in parsing.
#[test]
fn prefix_validation_parsing() {
    use beads_rust::util::id::validate_prefix;

    // Matching prefix
    assert!(validate_prefix("bd-abc123", "bd", &[]).is_ok());

    // Prefix in allowed list
    assert!(validate_prefix("other-abc123", "bd", &["other".to_string()]).is_ok());

    // Prefix mismatch
    let err = validate_prefix("wrong-abc123", "bd", &[]).unwrap_err();
    assert!(matches!(
        err,
        beads_rust::error::BeadsError::PrefixMismatch { .. }
    ));
}
