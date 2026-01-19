use beads_rust::sync::history::{HistoryConfig, backup_before_export, list_backups};
use std::fs::File;
use std::io::Write;
use tempfile::TempDir;

#[test]
fn test_backup_stem_collision() {
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    std::fs::create_dir_all(&beads_dir).unwrap();

    let config = HistoryConfig {
        enabled: true,
        max_count: 5,
        max_age_days: 30,
    };

    // 1. Create "issues_archive.jsonl" and back it up
    let archive_path = beads_dir.join("issues_archive.jsonl");
    let mut f = File::create(&archive_path).unwrap();
    f.write_all(b"archive data").unwrap();

    backup_before_export(&beads_dir, &config, &archive_path).unwrap();

    // Verify we have 1 backup
    let history_dir = beads_dir.join(".br_history");
    let backups = list_backups(&history_dir, None).unwrap();
    assert_eq!(backups.len(), 1);
    assert!(backups[0].path.to_string_lossy().contains("issues_archive"));

    // 2. Create "issues.jsonl" (prefix of issues_archive) and back it up
    // Wait a second to ensure different timestamp if needed, or just rely on name
    std::thread::sleep(std::time::Duration::from_secs(1));

    let issues_path = beads_dir.join("issues.jsonl");
    let mut f = File::create(&issues_path).unwrap();
    f.write_all(b"current data").unwrap();

    backup_before_export(&beads_dir, &config, &issues_path).unwrap();

    // We should now have 2 backups: one for archive, one for issues
    let backups = list_backups(&history_dir, None).unwrap();
    assert_eq!(backups.len(), 2, "Should have 2 backups total");

    // 3. Check specific filtering for "issues"
    // The bug is likely in get_latest_backup or how prune works if they rely on loose matching

    // Let's verify what `get_latest_backup` returns for "issues"
    // We can't call get_latest_backup directly as it's private, but we can infer behavior
    // from backup_before_export's deduplication logic.

    // If we try to backup "issues.jsonl" again with SAME content as "issues_archive.jsonl"
    // (collide content), but DIFFERENT content from "issues.jsonl" (previous backup).

    // Actually, simpler test:
    // If I prune backups for "issues", does it delete "issues_archive"?
    // The prune function `prune_backups` prunes *globally* in the directory based on count/age.
    // It doesn't seem to be per-file.

    // Wait, `backup_before_export` calls `rotate_history`.
    // `rotate_history` calls `list_backups` (all files) and deletes oldest > max_count.
    // If I have max_count=1.
    // I backup issues_archive (newest).
    // I backup issues (newer).
    // `rotate_history` sees 2 files. 2 > 1. It deletes the oldest.
    // It deletes issues_archive backup!
    // This is BAD. Backups for different files shouldn't rotate each other out.

    // Let's verify this rotation behavior.
    let config_strict = HistoryConfig {
        enabled: true,
        max_count: 1, // Only keep 1 backup
        max_age_days: 30,
    };

    // Clean up
    std::fs::remove_dir_all(&history_dir).unwrap();
    std::fs::create_dir_all(&history_dir).unwrap();

    // Backup archive
    backup_before_export(&beads_dir, &config_strict, &archive_path).unwrap();
    assert_eq!(list_backups(&history_dir, None).unwrap().len(), 1);

    std::thread::sleep(std::time::Duration::from_secs(1));

    // Backup issues
    backup_before_export(&beads_dir, &config_strict, &issues_path).unwrap();

    // If rotation is global, we have 1 file (the issues backup). Archive backup is gone.
    // If rotation is per-file (as it should be?), we should have 2 files (1 for each).
    let backups = list_backups(&history_dir, None).unwrap();

    // If this assertion fails (len == 1), it confirms that rotation is global and cross-file destructive.
    // If len == 2, then rotation handles files separately?
    // Looking at code: `rotate_history` just calls `list_backups` (which lists EVERYTHING) and sorts by time.
    // So yes, it's global rotation.

    assert_eq!(
        backups.len(),
        2,
        "Backups for different files should effectively have separate quotas, or at least not delete each other immediately"
    );
}
