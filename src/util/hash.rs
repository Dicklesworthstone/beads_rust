//! Content hashing for issue deduplication and sync.
//!
//! Uses SHA-256 over stable ordered fields with length-prefixed serialization.
//! Each field is encoded as an unsigned 64-bit little-endian byte length
//! followed by the raw UTF-8 bytes. This prevents the EXP-101 NUL-injection
//! field-boundary collision and intentionally breaks the old Go bd
//! NUL-separated `content_hash` byte format; schema v14 rebuilds stored hashes.

use sha2::{Digest, Sha256};

use crate::model::{Issue, IssueType, Priority, Status};

#[cfg(not(any(
    target_pointer_width = "16",
    target_pointer_width = "32",
    target_pointer_width = "64"
)))]
compile_error!("content hash field length prefixes require usize to fit in 64 bits");

/// Lowercase hex encoding for digest outputs (sha2 0.11 no longer impls `LowerHex`
/// on `Array<u8, _>`, so we format bytes directly).
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        s.push(hex_digit(byte >> 4));
        s.push(hex_digit(byte & 0x0f));
    }
    s
}

fn hex_digit(nibble: u8) -> char {
    if nibble < 10 {
        char::from(b'0' + nibble)
    } else {
        char::from(b'a' + (nibble - 10))
    }
}

/// Trait for types that can produce a deterministic content hash.
pub trait ContentHashable {
    /// Compute the content hash for this value.
    fn content_hash(&self) -> String;
}

impl ContentHashable for Issue {
    fn content_hash(&self) -> String {
        content_hash(self)
    }
}

/// Compute SHA256 content hash for an issue.
///
/// Fields included (stable order, each length-prefixed):
/// - title, description, design, `acceptance_criteria`, notes
/// - status, priority, `issue_type`
/// - assignee, owner, `created_by`
/// - `external_ref`, `source_system`
/// - pinned, `is_template`
/// - empty/default placeholders for reserved fields not represented in Rust
///
/// Fields excluded:
/// - id, `content_hash` (circular)
/// - labels, dependencies, comments, events (separate entities)
/// - timestamps (`created_at`, `updated_at`, `closed_at`, etc.)
/// - tombstone metadata (`deleted_at`, `deleted_by`, `delete_reason`)
/// - `estimated_minutes`, `due_at`, `defer_until`
/// - `close_reason`, `closed_by_session`
///
/// `status` is included, so a live issue and a `Status::Tombstone` issue do
/// not hash alike solely because deletion metadata is excluded. Tombstone
/// masking and resurrection protection must still key by `Issue.id`, not by
/// `content_hash`, because deletion metadata variants of the same tombstone
/// intentionally share a hash.
#[must_use]
pub fn content_hash(issue: &Issue) -> String {
    content_hash_from_parts(
        &issue.title,
        issue.description.as_deref(),
        issue.design.as_deref(),
        issue.acceptance_criteria.as_deref(),
        issue.notes.as_deref(),
        &issue.status,
        &issue.priority,
        &issue.issue_type,
        issue.assignee.as_deref(),
        issue.owner.as_deref(),
        issue.created_by.as_deref(),
        issue.external_ref.as_deref(),
        issue.source_system.as_deref(),
        issue.pinned,
        issue.is_template,
    )
}

/// Create a content hash from raw components (for import/validation).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn content_hash_from_parts(
    title: &str,
    description: Option<&str>,
    design: Option<&str>,
    acceptance_criteria: Option<&str>,
    notes: Option<&str>,
    status: &Status,
    priority: &Priority,
    issue_type: &IssueType,
    assignee: Option<&str>,
    owner: Option<&str>,
    created_by: Option<&str>,
    external_ref: Option<&str>,
    source_system: Option<&str>,
    pinned: bool,
    is_template: bool,
) -> String {
    let mut writer = HashFieldWriter::new();

    writer.field(title);
    writer.field_opt(description);
    writer.field_opt(design);
    writer.field_opt(acceptance_criteria);
    writer.field_opt(notes);
    writer.field(status.as_str());
    writer.field(&priority.0.to_string());
    writer.field(issue_type.as_str());
    writer.field_opt(assignee);
    writer.field_opt(owner);
    writer.field_opt(created_by);
    writer.field_opt(external_ref);
    writer.field_opt(source_system);
    writer.field_flag(pinned, "pinned");
    writer.field_flag(is_template, "template");

    // Keep placeholders for reserved fields so the field set remains explicit
    // and future additions have stable slots.
    writer.field(""); // quality_score nil
    writer.field_flag(false, "crystallizes");
    writer.field(""); // await_type
    writer.field(""); // await_id
    writer.field("0"); // timeout duration
    writer.field(""); // holder
    writer.field(""); // hook_bead
    writer.field(""); // role_bead
    writer.field(""); // agent_state
    writer.field(""); // role_type
    writer.field(""); // rig
    writer.field(""); // mol_type
    writer.field(""); // work_type
    writer.field(""); // event_kind
    writer.field(""); // actor
    writer.field(""); // target
    writer.field(""); // payload

    writer.finalize()
}

struct HashFieldWriter {
    hasher: Sha256,
}

impl HashFieldWriter {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
        }
    }

    fn field(&mut self, value: &str) {
        let bytes = value.as_bytes();
        self.hasher.update(field_len_prefix(bytes.len()));
        self.hasher.update(bytes);
    }

    fn field_opt(&mut self, value: Option<&str>) {
        self.field(value.unwrap_or(""));
    }

    fn field_flag(&mut self, value: bool, label: &str) {
        self.field(if value { label } else { "" });
    }

    fn finalize(self) -> String {
        hex_encode(&self.hasher.finalize())
    }
}

fn field_len_prefix(len: usize) -> [u8; 8] {
    let mut encoded = [0_u8; 8];
    for (destination, source) in encoded.iter_mut().zip(len.to_le_bytes()) {
        *destination = source;
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_issue() -> Issue {
        Issue {
            id: "bd-test123".to_string(),
            content_hash: None,
            title: "Test Issue".to_string(),
            description: Some("A test description".to_string()),
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: chrono::Utc::now(),
            created_by: None,
            updated_at: chrono::Utc::now(),
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            bypassed_policy: None,
            bypass_reason: None,
            policy_gates_fired: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
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
        }
    }

    #[test]
    fn test_content_hash_deterministic() {
        let issue = make_test_issue();
        let hash1 = content_hash(&issue);
        let hash2 = content_hash(&issue);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_is_hex() {
        let issue = make_test_issue();
        let hash = content_hash(&issue);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hash.len(), 64); // SHA256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn test_content_hash_changes_with_title() {
        let mut issue = make_test_issue();
        let hash1 = content_hash(&issue);

        issue.title = "Different Title".to_string();
        let hash2 = content_hash(&issue);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_distinguishes_embedded_nul_boundaries() {
        let hash_a = content_hash_from_parts(
            "x",
            Some("y\0z"),
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
        let hash_b = content_hash_from_parts(
            "x\0y",
            Some("z"),
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
            hash_a, hash_b,
            "embedded NULs must not let adjacent field boundaries collide"
        );
        assert_ne!(
            hash_a, "76dc81c2dbaddc1cddfa1f8c4674d06e018868f121f8c9385e0693fdf679e624",
            "old NUL-separated collision hash should not survive"
        );
        assert_ne!(
            hash_b, "76dc81c2dbaddc1cddfa1f8c4674d06e018868f121f8c9385e0693fdf679e624",
            "old NUL-separated collision hash should not survive"
        );
        assert_eq!(hash_a.len(), 64);
        assert_eq!(hash_b.len(), 64);
    }

    #[test]
    fn test_content_hash_ignores_timestamps() {
        let mut issue = make_test_issue();
        let hash1 = content_hash(&issue);

        issue.updated_at = chrono::Utc::now();
        let hash2 = content_hash(&issue);

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_includes_pinned() {
        let mut issue = make_test_issue();
        let hash1 = content_hash(&issue);

        issue.pinned = true;
        let hash2 = content_hash(&issue);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_includes_created_by() {
        let mut issue = make_test_issue();
        let hash1 = content_hash(&issue);

        issue.created_by = Some("tester@example.com".to_string());
        let hash2 = content_hash(&issue);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_includes_source_system() {
        let mut issue = make_test_issue();
        let hash1 = content_hash(&issue);

        issue.source_system = Some("imported".to_string());
        let hash2 = content_hash(&issue);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_from_parts() {
        let issue = make_test_issue();
        let direct = content_hash(&issue);
        let from_parts = content_hash_from_parts(
            &issue.title,
            issue.description.as_deref(),
            issue.design.as_deref(),
            issue.acceptance_criteria.as_deref(),
            issue.notes.as_deref(),
            &issue.status,
            &issue.priority,
            &issue.issue_type,
            issue.assignee.as_deref(),
            issue.owner.as_deref(),
            issue.created_by.as_deref(),
            issue.external_ref.as_deref(),
            issue.source_system.as_deref(),
            issue.pinned,
            issue.is_template,
        );
        assert_eq!(direct, from_parts);
    }

    #[test]
    fn test_field_len_prefix_is_u64_little_endian() {
        assert_eq!(field_len_prefix(0), [0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(field_len_prefix(0x0102), [0x02, 0x01, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn test_hex_encode_empty() {
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn test_hex_encode_single_byte() {
        assert_eq!(hex_encode(&[0x00]), "00");
        assert_eq!(hex_encode(&[0x0a]), "0a");
        assert_eq!(hex_encode(&[0xff]), "ff");
        assert_eq!(hex_encode(&[0x80]), "80");
        assert_eq!(hex_encode(&[0x7f]), "7f");
        assert_eq!(hex_encode(&[0x01]), "01");
    }

    #[test]
    fn test_hex_encode_length_invariant() {
        for len in [0, 1, 2, 16, 32, 64, 128, 255] {
            let bytes: Vec<u8> = (0..len)
                .map(|i: u32| u8::try_from(i).unwrap_or(0))
                .collect();
            let hex = hex_encode(&bytes);
            assert_eq!(
                hex.len(),
                bytes.len() * 2,
                "hex_encode output length should be 2*input for {} bytes",
                len
            );
        }
    }

    #[test]
    fn test_hex_encode_32_bytes_sha256_width() {
        let bytes: Vec<u8> = (0..32).collect();
        let hex = hex_encode(&bytes);
        assert_eq!(hex.len(), 64);
        assert_eq!(
            hex,
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
        );
    }

    #[test]
    fn test_hex_encode_high_bit_bytes() {
        assert_eq!(hex_encode(&[0x80, 0xff, 0xfe, 0x7f]), "80fffe7f");
    }

    #[test]
    fn test_hex_encode_is_lowercase() {
        let bytes: Vec<u8> = (0xa0..=0xaf).collect();
        let hex = hex_encode(&bytes);
        assert!(hex.chars().all(|c| !c.is_ascii_uppercase()));
        assert_eq!(hex, "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf");
    }

    #[test]
    fn test_hex_encode_all_zeros_32_bytes() {
        let bytes = [0u8; 32];
        let hex = hex_encode(&bytes);
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, "0".repeat(64));
    }

    #[test]
    fn test_hex_encode_all_ff_32_bytes() {
        let bytes = [0xffu8; 32];
        let hex = hex_encode(&bytes);
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, "f".repeat(64));
    }

    #[test]
    fn test_hex_encode_all_256_byte_values() {
        let bytes: Vec<u8> = (0..=255).collect();
        let hex = hex_encode(&bytes);
        assert_eq!(hex.len(), 512);
        let reference: String = bytes.iter().fold(String::new(), |mut acc, b| {
            use std::fmt::Write;
            write!(acc, "{b:02x}").unwrap();
            acc
        });
        assert_eq!(hex, reference);
    }

    #[test]
    fn test_hex_encode_matches_sha256_digest() {
        let mut hasher = Sha256::new();
        hasher.update(b"hello world");
        let digest = hasher.finalize();
        let hex = hex_encode(&digest);
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            hex,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }
}
