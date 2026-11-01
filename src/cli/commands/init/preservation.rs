//! Init may create a new database or reuse a current one, never upgrade an old
//! authoritative tracker through the generic schema initializer.

use crate::config;
use crate::error::{BeadsError, Result};
use crate::franken_sync::Connection;
use crate::franken_sync::compat::{OpenFlags, open_with_flags};
use crate::storage::{SqliteStorage, schema};
use crate::sync::DatabaseFamilyWriteLock;
use fsqlite_types::SqliteValue;
use std::fs;
use std::path::Path;
use std::sync::Arc;

fn refusal(db_path: &Path, reason: impl std::fmt::Display) -> BeadsError {
    BeadsError::SyncConflict {
        message: format!(
            "Refusing init for database '{}': {reason}. Init only creates a missing database or reuses a supported current-schema database; it does not upgrade an existing tracker. Preserve the database family and run `br doctor --json`, then `br doctor migrate-schema plan` for a reviewed lossless upgrade",
            db_path.display()
        ),
    }
}

/// Return whether the exact init target already exists, without opening or
/// locking the original family. The copied WAL determines the effective version.
pub(super) fn preflight(db_path: &Path) -> Result<bool> {
    match fs::symlink_metadata(db_path) {
        Ok(metadata) if metadata.is_file() => {
            config::with_database_family_snapshot(db_path, inspect_current_snapshot)
                .map_err(|error| refusal(db_path, error))?;
            Ok(true)
        }
        Ok(_) => Err(refusal(db_path, "the database path is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let warnings = config::missing_database_preservation_warnings(db_path);
            if !warnings.is_empty() {
                return Err(refusal(db_path, warnings.join("; ")));
            }
            Ok(false)
        }
        Err(error) => Err(refusal(db_path, error)),
    }
}

fn inspect_current_snapshot(path: &Path) -> Result<()> {
    let conn = open_with_flags(
        path.to_string_lossy().as_ref(),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let result = inspect_current_connection(&conn);
    let closed = conn.close().map_err(BeadsError::Database);
    result.and(closed)
}

fn inspect_current_connection(conn: &Connection) -> Result<()> {
    let row = conn.query_row("PRAGMA user_version")?;
    let version = row
        .get(0)
        .and_then(SqliteValue::as_integer)
        .ok_or_else(|| {
            BeadsError::Config("effective database schema version is unreadable".into())
        })?;
    if version != i64::from(schema::CURRENT_SCHEMA_VERSION) {
        return Err(BeadsError::Config(format!(
            "effective database schema version {version} requires reviewed migration; expected {}",
            schema::CURRENT_SCHEMA_VERSION
        )));
    }
    if !schema::runtime_schema_compatible(conn) {
        return Err(BeadsError::Config(
            "existing database does not match the supported current schema".into(),
        ));
    }
    if schema::has_pending_sync_merge_metadata(conn)? {
        return Err(BeadsError::Config(
            "existing database has pending sync-merge state; run `br sync --merge` before init"
                .into(),
        ));
    }
    Ok(())
}

pub(super) fn verify_authority_target(
    db_path: &Path,
    authority: Option<&Arc<DatabaseFamilyWriteLock>>,
) -> Result<()> {
    if let Some(authority) = authority {
        if crate::sync::database_write_authority_sha256(db_path)?
            != authority.authority_path_sha256()
        {
            return Err(refusal(
                db_path,
                format!(
                    "startup's write authority protects '{}' instead of init's actual target; align the database routing before retrying",
                    authority.canonical_database_path().display()
                ),
            ));
        }
        authority.verify_database_authority()?;
    }
    Ok(())
}

/// Repeat admission while retaining exact inode and path authority. Only the
/// locked fresh inode may reach an opener that can apply a schema.
pub(super) fn open_storage(
    beads_dir: &Path,
    db_path: &Path,
    expected_existing: bool,
    supplied_authority: Option<&Arc<DatabaseFamilyWriteLock>>,
    lock_timeout_ms: Option<u64>,
) -> Result<SqliteStorage> {
    verify_authority_target(db_path, supplied_authority)?;
    let authority = match supplied_authority {
        Some(authority) => Arc::clone(authority),
        None => Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                beads_dir,
                db_path,
                lock_timeout_ms,
            )?,
        ),
    };
    authority.verify_database_authority()?;
    if preflight(db_path)? != expected_existing {
        return Err(refusal(
            db_path,
            "the database appeared or disappeared during init admission",
        ));
    }
    authority.verify_database_authority()?;
    let mut storage = if expected_existing {
        // This opener never creates, migrates, repairs schema, or seeds metadata.
        // A stale generation cannot reach legacy migrations even if admission
        // is invalidated by a writer outside the cooperative authority protocol.
        SqliteStorage::open_current_for_reconcile(db_path, lock_timeout_ms)?
            .ok_or_else(|| refusal(db_path, "the admitted current schema changed before open"))?
    } else {
        // Frames or rollback state already caused refusal above. Retain the
        // existing quarantine behavior for empty and derived orphan sidecars.
        config::quarantine_orphaned_sidecars(db_path, beads_dir);
        authority.verify_database_authority()?;
        let fresh = authority.install_empty_database_replacement_and_bind()?;
        authority.verify_fresh_database_replacement_witness(&fresh)?;
        let storage = SqliteStorage::open_with_timeout_under_write_authority(
            db_path,
            lock_timeout_ms,
            &authority,
        )?;
        authority.verify_fresh_database_replacement_witness(&fresh)?;
        storage
    };
    authority.verify_database_authority()?;
    storage.attach_write_authority(authority);
    Ok(storage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn family_bytes(directory: &Path) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn changed_schema_after_initial_admission_cannot_reach_legacy_migrations() {
        let root = tempfile::tempdir().unwrap();
        let beads = root.path().join(".beads");
        fs::create_dir(&beads).unwrap();
        let db = beads.join("beads.db");
        drop(SqliteStorage::open(&db).unwrap());
        let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
        conn.execute(
            "INSERT INTO dirty_issues (rowid, issue_id, marked_at) VALUES (71, 'local', X'00ff')",
        )
        .unwrap();
        conn.execute("INSERT INTO export_hashes (rowid, issue_id, content_hash, exported_at) VALUES (83, 'local', X'0102', 'original')")
            .unwrap();
        conn.close().unwrap();
        assert!(preflight(&db).unwrap());

        let authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(&beads, &db, None)
                .unwrap(),
        );
        // Simulate a noncooperating writer invalidating the earlier admission
        // without changing the locked inode. Rechecking the header alone is
        // insufficient when the committed version still lives in WAL.
        let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
        conn.execute("PRAGMA user_version=13").unwrap();
        conn.close().unwrap();
        let before = family_bytes(&beads);
        let error = open_storage(&beads, &db, true, Some(&authority), None).unwrap_err();
        assert!(error.to_string().contains("schema version 13"), "{error}");
        assert_eq!(family_bytes(&beads), before);
    }

    #[test]
    fn unrelated_startup_authority_is_refused_before_the_target_is_opened() {
        let root = tempfile::tempdir().unwrap();
        let beads = root.path().join(".beads");
        fs::create_dir(&beads).unwrap();
        let db = beads.join("beads.db");
        let alternate = beads.join("alternate.db");
        drop(SqliteStorage::open(&db).unwrap());
        drop(SqliteStorage::open(&alternate).unwrap());
        let authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(&beads, &alternate, None)
                .unwrap(),
        );
        let before = family_bytes(&beads);
        let error = open_storage(&beads, &db, true, Some(&authority), None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("init's actual target"), "{message}");
        assert!(message.contains("alternate.db"), "{message}");
        assert!(message.contains("beads.db"), "{message}");
        assert_eq!(family_bytes(&beads), before);
    }
}
