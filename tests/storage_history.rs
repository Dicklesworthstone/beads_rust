//! Storage-level tests for history backup module.
//!
//! Tests edge cases and invariants not covered by the e2e CLI tests:
//! - Disabled config skips backup
//! - Non-existent target skips backup (no crash)
//! - Changed content creates a new backup (non-dedup)
//! - Rotation by age deletes old backups
//! - Mixed file stems rotate independently
//! - list_backups with prefix filter only returns matching files
//! - Confinement: backup only for files inside .beads/
//! - Prune with zero keep deletes everything
//! - Rapid backups with distinct content all preserved
//!
//! Related bead: beads_rust-2xbh

use beads_rust::sync::history::{
    BackupEntry, HistoryConfig, backup_before_export, list_backups, prune_backups,
};
use chrono::{Duration, Utc};
use std::fs::{self, File};
use std::io::Write;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn setup_beads_dir(temp: &TempDir) -> std::path::PathBuf {
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    beads_dir
}

fn write_file(path: &std::path::Path, content: &[u8]) {
    File::create(path).unwrap().write_all(content).unwrap();
}

fn history_dir(beads_dir: &std::path::Path) -> std::path::PathBuf {
    beads_dir.join(".br_history")
}

// ===========================================================================
// 1. Disabled config skips backup
// ===========================================================================

#[test]
fn disabled_config_skips_backup() {
    let temp = TempDir::new().unwrap();
    let beads_dir = setup_beads_dir(&temp);
    let target = beads_dir.join("issues.jsonl");
    write_file(&target, b"some content");

    let config = HistoryConfig {
        enabled: false,
        max_count: 100,
        max_age_days: 30,
        min_interval_secs: 0,
    };

    backup_before_export(&beads_dir, &config, &target).unwrap();

    // History directory should not even be created
    assert!(
        !history_dir(&beads_dir).exists(),
        "disabled config should not create history directory"
    );
}

// ===========================================================================
// 2. Non-existent target skips backup gracefully
// ===========================================================================

#[test]
fn nonexistent_target_skips_backup() {
    let temp = TempDir::new().unwrap();
    let beads_dir = setup_beads_dir(&temp);
    let target = beads_dir.join("does_not_exist.jsonl");

    let config = HistoryConfig::default();

    // Should succeed without error
    backup_before_export(&beads_dir, &config, &target).unwrap();

    // No history directory created
    assert!(!history_dir(&beads_dir).exists());
}

// ===========================================================================
// 3. Changed content creates new backup (non-dedup)
// ===========================================================================

#[test]
fn changed_content_creates_new_backup() {
    let temp = TempDir::new().unwrap();
    let beads_dir = setup_beads_dir(&temp);
    let target = beads_dir.join("issues.jsonl");

    // Disable the snapshot throttle: this test validates that *changed* content
    // produces a new (non-deduped) backup, independent of the #313 throttle.
    let config = HistoryConfig {
        min_interval_secs: 0,
        ..HistoryConfig::default()
    };

    // First backup
    write_file(&target, b"version 1");
    backup_before_export(&beads_dir, &config, &target).unwrap();

    // Change content
    write_file(&target, b"version 2");

    // Need a small delay so timestamp differs
    std::thread::sleep(std::time::Duration::from_secs(1));
    backup_before_export(&beads_dir, &config, &target).unwrap();

    let backups = list_backups(&history_dir(&beads_dir), None).unwrap();
    assert_eq!(
        backups.len(),
        2,
        "changed content should create a second backup"
    );
}

// ===========================================================================
// 4. Identical content is deduplicated
// ===========================================================================

#[test]
fn identical_content_is_deduplicated() {
    let temp = TempDir::new().unwrap();
    let beads_dir = setup_beads_dir(&temp);
    let target = beads_dir.join("issues.jsonl");
    write_file(&target, b"identical content");

    let config = HistoryConfig::default();

    backup_before_export(&beads_dir, &config, &target).unwrap();
    backup_before_export(&beads_dir, &config, &target).unwrap();
    backup_before_export(&beads_dir, &config, &target).unwrap();

    let backups = list_backups(&history_dir(&beads_dir), None).unwrap();
    assert_eq!(
        backups.len(),
        1,
        "identical content should be deduplicated to one backup"
    );
}

// ===========================================================================
// 5. Rotation by count keeps newest
// ===========================================================================

#[test]
fn rotation_by_count_keeps_newest() {
    let temp = TempDir::new().unwrap();
    let beads_dir = setup_beads_dir(&temp);
    let hdir = history_dir(&beads_dir);
    fs::create_dir_all(&hdir).unwrap();

    let target = beads_dir.join("issues.jsonl");

    let config = HistoryConfig {
        enabled: true,
        max_count: 2,
        max_age_days: 365, // Don't trigger age-based rotation
        min_interval_secs: 0,
    };

    // Create 4 backups with distinct content
    for i in 0..4 {
        write_file(&target, format!("version {i}").as_bytes());
        std::thread::sleep(std::time::Duration::from_secs(1));
        backup_before_export(&beads_dir, &config, &target).unwrap();
    }

    let backups = list_backups(&hdir, None).unwrap();
    assert_eq!(backups.len(), 2, "rotation should keep only 2 newest");

    // Verify the kept backups are the newest ones (largest size = latest versions)
    // Since list_backups returns newest first, first entry should be the most recent
    assert!(backups[0].timestamp > backups[1].timestamp);
}

// ===========================================================================
// 6. Rotation by age deletes old backups
// ===========================================================================

#[test]
fn rotation_by_age_deletes_old() {
    let temp = TempDir::new().unwrap();
    let hdir = temp.path().to_path_buf();

    let now = Utc::now();

    // Create backups: one recent (1 hour ago), one old (40 days ago)
    let recent_ts = (now - Duration::hours(1)).format("%Y%m%d_%H%M%S");
    let old_ts = (now - Duration::days(40)).format("%Y%m%d_%H%M%S");

    write_file(&hdir.join(format!("issues.{recent_ts}.jsonl")), b"recent");
    write_file(&hdir.join(format!("issues.{old_ts}.jsonl")), b"old");

    // Prune: keep 100, older than 30 days
    let deleted = prune_backups(&hdir, 100, Some(30)).unwrap();
    assert_eq!(deleted, 1, "should delete 1 old backup");

    let remaining = list_backups(&hdir, None).unwrap();
    assert_eq!(remaining.len(), 1);
    assert!(
        remaining[0]
            .path
            .to_string_lossy()
            .contains(&recent_ts.to_string()),
        "recent backup should remain"
    );
}

// ===========================================================================
// 7. Mixed file stems rotate independently
// ===========================================================================

#[test]
fn mixed_stems_rotate_independently() {
    let temp = TempDir::new().unwrap();
    let beads_dir = setup_beads_dir(&temp);
    let hdir = history_dir(&beads_dir);
    fs::create_dir_all(&hdir).unwrap();

    let config = HistoryConfig {
        enabled: true,
        max_count: 1,
        max_age_days: 365,
        min_interval_secs: 0,
    };

    // Create backups for "issues" stem
    let issues_target = beads_dir.join("issues.jsonl");
    write_file(&issues_target, b"issues v1");
    backup_before_export(&beads_dir, &config, &issues_target).unwrap();

    std::thread::sleep(std::time::Duration::from_secs(1));
    write_file(&issues_target, b"issues v2");
    backup_before_export(&beads_dir, &config, &issues_target).unwrap();

    // Create backups for "archive" stem
    let archive_target = beads_dir.join("archive.jsonl");
    write_file(&archive_target, b"archive v1");
    backup_before_export(&beads_dir, &config, &archive_target).unwrap();

    std::thread::sleep(std::time::Duration::from_secs(1));
    write_file(&archive_target, b"archive v2");
    backup_before_export(&beads_dir, &config, &archive_target).unwrap();

    // Each stem should have max_count=1 backups
    let issues_backups = list_backups(&hdir, Some("issues.")).unwrap();
    let archive_backups = list_backups(&hdir, Some("archive.")).unwrap();

    assert_eq!(
        issues_backups.len(),
        1,
        "issues should have exactly 1 backup after rotation"
    );
    assert_eq!(
        archive_backups.len(),
        1,
        "archive should have exactly 1 backup after rotation"
    );

    // Total backups = 2 (one per stem)
    let all_backups = list_backups(&hdir, None).unwrap();
    assert_eq!(all_backups.len(), 2);
}

// ===========================================================================
// 8. list_backups with prefix filter
// ===========================================================================

#[test]
fn list_backups_prefix_filter() {
    let temp = TempDir::new().unwrap();
    let hdir = temp.path();

    let now = Utc::now();
    let ts = now.format("%Y%m%d_%H%M%S");

    write_file(&hdir.join(format!("issues.{ts}.jsonl")), b"a");
    write_file(&hdir.join(format!("archive.{ts}.jsonl")), b"b");
    write_file(&hdir.join(format!("tasks.{ts}.jsonl")), b"c");

    // No filter: all 3
    assert_eq!(list_backups(hdir, None).unwrap().len(), 3);

    // Filter by "issues."
    let filtered = list_backups(hdir, Some("issues.")).unwrap();
    assert_eq!(filtered.len(), 1);
    assert!(filtered[0].path.to_string_lossy().contains("issues."));

    // Filter by "archive."
    let filtered = list_backups(hdir, Some("archive.")).unwrap();
    assert_eq!(filtered.len(), 1);

    // Filter with non-matching prefix
    let filtered = list_backups(hdir, Some("nonexistent.")).unwrap();
    assert_eq!(filtered.len(), 0);
}

// ===========================================================================
// 9. list_backups on nonexistent directory returns empty
// ===========================================================================

#[test]
fn list_backups_nonexistent_dir_returns_empty() {
    let temp = TempDir::new().unwrap();
    let fake_dir = temp.path().join("does_not_exist");

    let backups = list_backups(&fake_dir, None).unwrap();
    assert!(backups.is_empty());
}

// ===========================================================================
// 10. list_backups skips invalid filenames
// ===========================================================================

#[test]
fn list_backups_skips_invalid_filenames() {
    let temp = TempDir::new().unwrap();
    let hdir = temp.path();

    let now = Utc::now();
    let ts = now.format("%Y%m%d_%H%M%S");

    // Valid backup
    write_file(&hdir.join(format!("issues.{ts}.jsonl")), b"valid");

    // Invalid filenames
    write_file(&hdir.join("issues.not_a_timestamp.jsonl"), b"invalid");
    write_file(&hdir.join("random_file.txt"), b"ignored");
    write_file(&hdir.join("issues.jsonl"), b"no timestamp");

    let backups = list_backups(hdir, None).unwrap();
    assert_eq!(
        backups.len(),
        1,
        "should only list validly-timestamped backup files"
    );
}

// ===========================================================================
// 11. Prune with zero keep deletes all
// ===========================================================================

#[test]
fn prune_zero_keep_deletes_all() {
    let temp = TempDir::new().unwrap();
    let hdir = temp.path();

    let now = Utc::now();
    for i in 0..5 {
        let ts = (now - Duration::hours(i)).format("%Y%m%d_%H%M%S");
        write_file(&hdir.join(format!("issues.{ts}.jsonl")), b"data");
    }

    assert_eq!(list_backups(hdir, None).unwrap().len(), 5);

    let deleted = prune_backups(hdir, 0, None).unwrap();
    assert_eq!(deleted, 5);
    assert!(list_backups(hdir, None).unwrap().is_empty());
}

// ===========================================================================
// 12. Prune on empty directory is a no-op
// ===========================================================================

#[test]
fn prune_empty_dir_is_noop() {
    let temp = TempDir::new().unwrap();
    let deleted = prune_backups(temp.path(), 10, None).unwrap();
    assert_eq!(deleted, 0);
}

// ===========================================================================
// 13. BackupEntry fields are populated correctly
// ===========================================================================

#[test]
fn backup_entry_fields_populated() {
    let temp = TempDir::new().unwrap();
    let beads_dir = setup_beads_dir(&temp);
    let target = beads_dir.join("issues.jsonl");
    write_file(&target, b"hello world");

    let config = HistoryConfig::default();
    backup_before_export(&beads_dir, &config, &target).unwrap();

    let backups = list_backups(&history_dir(&beads_dir), None).unwrap();
    assert_eq!(backups.len(), 1);

    let entry: &BackupEntry = &backups[0];
    assert!(entry.path.exists(), "backup file should exist");
    assert!(entry.size > 0, "backup should have non-zero size");
    assert_eq!(entry.size, 11, "size should match 'hello world'");

    // Timestamp should be recent (within last 60 seconds)
    let age = Utc::now() - entry.timestamp;
    assert!(
        age.num_seconds() < 60,
        "backup timestamp should be recent, got age: {age}"
    );
}

// ===========================================================================
// 14. Backup preserves content exactly
// ===========================================================================

#[test]
fn backup_preserves_content_exactly() {
    let temp = TempDir::new().unwrap();
    let beads_dir = setup_beads_dir(&temp);
    let target = beads_dir.join("issues.jsonl");

    let content =
        b"{\"id\":\"test-1\",\"title\":\"Hello\"}\n{\"id\":\"test-2\",\"title\":\"World\"}\n";
    write_file(&target, content);

    let config = HistoryConfig::default();
    backup_before_export(&beads_dir, &config, &target).unwrap();

    let backups = list_backups(&history_dir(&beads_dir), None).unwrap();
    let backup_content = fs::read(&backups[0].path).unwrap();
    assert_eq!(
        backup_content, content,
        "backup content should be byte-identical to source"
    );
}

// ===========================================================================
// 15. Backups sorted newest first
// ===========================================================================

#[test]
fn backups_sorted_newest_first() {
    let temp = TempDir::new().unwrap();
    let hdir = temp.path();

    let now = Utc::now();
    // Create backups with explicit timestamps in reverse order
    let old_ts = (now - Duration::hours(3)).format("%Y%m%d_%H%M%S");
    let mid_ts = (now - Duration::hours(2)).format("%Y%m%d_%H%M%S");
    let new_ts = (now - Duration::hours(1)).format("%Y%m%d_%H%M%S");

    // Write in scrambled order
    write_file(&hdir.join(format!("issues.{mid_ts}.jsonl")), b"mid");
    write_file(&hdir.join(format!("issues.{old_ts}.jsonl")), b"old");
    write_file(&hdir.join(format!("issues.{new_ts}.jsonl")), b"new");

    let backups = list_backups(hdir, None).unwrap();
    assert_eq!(backups.len(), 3);

    // Should be sorted newest → oldest
    assert!(backups[0].timestamp > backups[1].timestamp);
    assert!(backups[1].timestamp > backups[2].timestamp);
}
