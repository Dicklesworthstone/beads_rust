//! Refuse-unsafe gates that run BEFORE any `--repair` execution.
//!
//! These gates are precondition checks. They never mutate; if any gate
//! returns [`GateOutcome::Refuse`] the doctor must exit with
//! [`super::exit_codes::DoctorExitCode::RefusedUnsafe`] (= 4) without
//! running any fixer.
//!
//! ## WP1 gates
//!
//! 1. [`gate_schema_version_downgrade`] — refuses if the on-disk
//!    `PRAGMA user_version` is newer than the binary's
//!    [`CURRENT_SCHEMA_VERSION`]. A doctor that doesn't understand the
//!    schema must not "repair" it.
//! 2. [`gate_recovery_fingerprint_integrity`] — walks
//!    the active database family's `.br_recovery/` directory and refuses
//!    if any backup artifact has diverged from its recorded fingerprint.
//!    Diverged backups are a
//!    sign of out-of-band tampering and the doctor must not overwrite
//!    them.
//!
//! Both gates are pure-read.

#![allow(dead_code)] // WP1 foundation; consumed by WP3-WP12.

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::exit_codes::DoctorExitCode;
use crate::util::hex_encode;

/// A gate's verdict.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum GateOutcome {
    /// Gate passes. `--repair` may proceed.
    Allow,
    /// Gate refuses. Doctor must exit with `code` and surface
    /// `reason` + `evidence`.
    Refuse {
        code: i32,
        reason: String,
        evidence: serde_json::Value,
    },
}

impl GateOutcome {
    /// True if the gate refuses.
    #[must_use]
    pub const fn is_refused(&self) -> bool {
        matches!(self, Self::Refuse { .. })
    }
}

/// Read the on-disk SQLite header `user_version` for `path`. Returns
/// `None` when the file is missing or doesn't have the SQLite magic
/// header. Mirrors the private helper in `storage::sqlite`.
fn header_user_version(path: &Path) -> Option<u32> {
    use std::io::Read;
    if path == Path::new(":memory:") || !path.is_file() {
        return None;
    }
    let mut file = fs::File::open(path).ok()?;
    let mut header = [0_u8; 100];
    file.read_exact(&mut header).ok()?;
    if &header[..16] != b"SQLite format 3\0" {
        return None;
    }
    Some(u32::from_be_bytes([
        header[60], header[61], header[62], header[63],
    ]))
}

/// Refuse if the on-disk database was written by a newer binary than
/// the one currently running.
///
/// `db_path` is typically `<repo>/.beads/beads.db`.
#[must_use]
pub fn gate_schema_version_downgrade(db_path: &Path) -> GateOutcome {
    use crate::storage::schema::CURRENT_SCHEMA_VERSION;

    let Some(on_disk) = header_user_version(db_path) else {
        // No DB or non-SQLite file — not a downgrade; leave detection
        // to other gates. (A missing DB is exit 66 / not-initialized,
        // not a refusal here.)
        return GateOutcome::Allow;
    };
    let binary = u32::try_from(CURRENT_SCHEMA_VERSION).unwrap_or(0);
    if on_disk > binary {
        return GateOutcome::Refuse {
            code: DoctorExitCode::RefusedUnsafe.as_i32(),
            reason: format!(
                "doctor: database schema_version={on_disk} > binary schema_version={binary} \
                 (running an older br against a newer db is unsafe; upgrade br first)"
            ),
            evidence: serde_json::json!({
                "gate": "schema_version_downgrade",
                "db_path": db_path.display().to_string(),
                "db_schema_version": on_disk,
                "binary_schema_version": binary,
            }),
        };
    }
    GateOutcome::Allow
}

/// Refuse if any recovery backup artifact has diverged from its
/// recorded fingerprint. Walks the active database family's recovery
/// directory and any `*.fingerprint.json` files emitted by the recovery
/// primitives.
///
/// In WP1 this gate is conservative: if there is no recovery directory
/// (the common case for healthy workspaces), the gate allows.
#[must_use]
pub fn gate_recovery_fingerprint_integrity(beads_dir: &Path, db_path: &Path) -> GateOutcome {
    let recovery_dir = crate::config::recovery_dir_for_db_path(db_path, beads_dir);
    gate_recovery_fingerprint_integrity_in_dir(&recovery_dir)
}

#[must_use]
fn gate_recovery_fingerprint_integrity_in_dir(recovery_dir: &Path) -> GateOutcome {
    if !recovery_dir.exists() {
        return GateOutcome::Allow;
    }
    let mismatches = match scan_fingerprints(recovery_dir) {
        Ok(v) => v,
        Err(e) => {
            return GateOutcome::Refuse {
                code: DoctorExitCode::RefusedUnsafe.as_i32(),
                reason: format!(
                    "doctor: could not enumerate recovery fingerprints under {}: {e}",
                    recovery_dir.display()
                ),
                evidence: serde_json::json!({
                    "gate": "recovery_fingerprint_integrity",
                    "error": e.to_string(),
                    "recovery_dir": recovery_dir.display().to_string(),
                }),
            };
        }
    };
    if mismatches.is_empty() {
        return GateOutcome::Allow;
    }
    GateOutcome::Refuse {
        code: DoctorExitCode::RefusedUnsafe.as_i32(),
        reason: format!(
            "doctor: {} recovery backup(s) diverged from recorded fingerprints; refusing --repair",
            mismatches.len()
        ),
        evidence: serde_json::json!({
            "gate": "recovery_fingerprint_integrity",
            "recovery_dir": recovery_dir.display().to_string(),
            "mismatched_artifacts": mismatches,
        }),
    }
}

#[derive(Debug, Clone, Serialize)]
struct FingerprintMismatch {
    artifact: String,
    fingerprint: String,
    reason: String,
}

fn push_fingerprint_mismatch(
    out: &mut Vec<FingerprintMismatch>,
    artifact: impl Into<String>,
    fingerprint: &Path,
    reason: impl Into<String>,
) {
    out.push(FingerprintMismatch {
        artifact: artifact.into(),
        fingerprint: fingerprint.display().to_string(),
        reason: reason.into(),
    });
}

fn recovery_relative_artifact_path(recovery_dir: &Path, target_rel: &str) -> Option<PathBuf> {
    let target_rel_path = Path::new(target_rel);
    if target_rel_path.is_absolute()
        || target_rel_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return None;
    }
    Some(recovery_dir.join(target_rel_path))
}

fn is_regular_hashed_artifact(
    out: &mut Vec<FingerprintMismatch>,
    target: &Path,
    fingerprint: &Path,
) -> bool {
    match fs::symlink_metadata(target) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() || !file_type.is_file() {
                push_fingerprint_mismatch(
                    out,
                    target.display().to_string(),
                    fingerprint,
                    "fingerprint artifact with sha256 must be a regular file",
                );
                return false;
            }
            true
        }
        Err(e) => {
            push_fingerprint_mismatch(
                out,
                target.display().to_string(),
                fingerprint,
                format!("could not inspect artifact: {e}"),
            );
            false
        }
    }
}

fn scan_fingerprint_file(
    out: &mut Vec<FingerprintMismatch>,
    recovery_dir: &Path,
    path: &Path,
) -> std::io::Result<()> {
    let fp_text = fs::read_to_string(path)?;
    let fp: serde_json::Value = match serde_json::from_str(&fp_text) {
        Ok(v) => v,
        Err(e) => {
            push_fingerprint_mismatch(
                out,
                path.display().to_string(),
                path,
                format!("could not parse fingerprint json: {e}"),
            );
            return Ok(());
        }
    };
    let Some(target_rel) = fp.get("artifact").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let Some(target) = recovery_relative_artifact_path(recovery_dir, target_rel) else {
        push_fingerprint_mismatch(
            out,
            target_rel.to_string(),
            path,
            "fingerprint artifact path must stay inside the recovery directory",
        );
        return Ok(());
    };
    let expected_sha = fp
        .get("sha256")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if expected_sha.is_empty() {
        // Symlink / dir fingerprints have no sha; skip.
        return Ok(());
    }
    if !is_regular_hashed_artifact(out, &target, path) {
        return Ok(());
    }
    let bytes = match fs::read(&target) {
        Ok(b) => b,
        Err(e) => {
            push_fingerprint_mismatch(
                out,
                target.display().to_string(),
                path,
                format!("could not read artifact: {e}"),
            );
            return Ok(());
        }
    };
    let actual = hex_encode(&Sha256::digest(&bytes));
    if actual.as_str().ne(expected_sha.as_str()) {
        push_fingerprint_mismatch(
            out,
            target.display().to_string(),
            path,
            format!("sha256 mismatch (expected {expected_sha}, found {actual})"),
        );
    }
    Ok(())
}

fn scan_fingerprints(recovery_dir: &Path) -> std::io::Result<Vec<FingerprintMismatch>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(recovery_dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        // Recurse one level for nested recovery dirs.
        if path.is_dir() {
            out.extend(scan_fingerprints(&path)?);
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.ends_with(".fingerprint.json") {
            scan_fingerprint_file(&mut out, recovery_dir, &path)?;
        }
    }
    Ok(out)
}

/// Bundle of every WP1 gate. Returns `Allow` if all pass, otherwise
/// the first refusal verdict.
#[must_use]
pub fn run_all(beads_dir: &Path, db_path: &Path) -> GateOutcome {
    let downgrade = gate_schema_version_downgrade(db_path);
    if downgrade.is_refused() {
        return downgrade;
    }
    gate_recovery_fingerprint_integrity(beads_dir, db_path)
}

/// Surfaces the recovery directory used by the integrity gate. Useful
/// for callers that want to advertise what the gate inspected.
#[must_use]
pub fn recovery_dir_for(beads_dir: &Path, db_path: &Path) -> PathBuf {
    crate::config::recovery_dir_for_db_path(db_path, beads_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    /// Build a minimal SQLite-magic-header file with a chosen
    /// `user_version` value. Returns the temp dir to keep it alive.
    fn write_fake_sqlite_db(path: &Path, user_version: u32) {
        let mut header = [0_u8; 100];
        header[..16].copy_from_slice(b"SQLite format 3\0");
        header[60..64].copy_from_slice(&user_version.to_be_bytes());
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&header).unwrap();
    }

    #[test]
    fn schema_version_downgrade_refuses_when_db_is_newer() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("beads.db");
        write_fake_sqlite_db(&db_path, 9999);

        let outcome = gate_schema_version_downgrade(&db_path);
        assert!(
            matches!(outcome, GateOutcome::Refuse { .. }),
            "must refuse newer-on-disk schema"
        );
        let GateOutcome::Refuse {
            code,
            reason,
            evidence,
        } = outcome
        else {
            return;
        };
        assert_eq!(code, DoctorExitCode::RefusedUnsafe.as_i32());
        assert!(reason.contains("schema_version"));
        assert_eq!(evidence["gate"], "schema_version_downgrade");
        assert_eq!(evidence["db_schema_version"], 9999);
    }

    #[test]
    fn schema_version_downgrade_allows_when_db_matches_or_is_older() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("beads.db");
        // Set user_version to 0 — guaranteed <= CURRENT_SCHEMA_VERSION.
        write_fake_sqlite_db(&db_path, 0);
        assert!(matches!(
            gate_schema_version_downgrade(&db_path),
            GateOutcome::Allow
        ));
    }

    #[test]
    fn schema_version_downgrade_allows_when_db_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("does-not-exist.db");
        assert!(matches!(
            gate_schema_version_downgrade(&db_path),
            GateOutcome::Allow
        ));
    }

    #[test]
    fn recovery_fingerprint_integrity_allows_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        fs::create_dir_all(&beads_dir).unwrap();
        assert!(matches!(
            gate_recovery_fingerprint_integrity(&beads_dir, &db_path),
            GateOutcome::Allow
        ));
    }

    #[test]
    fn recovery_fingerprint_integrity_refuses_on_sha_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let recovery = beads_dir.join(".br_recovery");
        fs::create_dir_all(&recovery).unwrap();

        // Drop a backup artifact + a fingerprint that disagrees with
        // its actual content.
        let artifact = recovery.join("backup.bin");
        fs::write(&artifact, b"actual bytes").unwrap();
        let fingerprint = serde_json::json!({
            "artifact": "backup.bin",
            "sha256": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        });
        fs::write(
            recovery.join("backup.bin.fingerprint.json"),
            serde_json::to_string_pretty(&fingerprint).unwrap(),
        )
        .unwrap();

        let outcome = gate_recovery_fingerprint_integrity(&beads_dir, &db_path);
        assert!(
            matches!(outcome, GateOutcome::Refuse { .. }),
            "must refuse on fingerprint mismatch"
        );
        let GateOutcome::Refuse { code, evidence, .. } = outcome else {
            return;
        };
        assert_eq!(code, DoctorExitCode::RefusedUnsafe.as_i32());
        assert_eq!(evidence["gate"], "recovery_fingerprint_integrity");
        let arr = evidence["mismatched_artifacts"].as_array().expect("array");
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn recovery_fingerprint_integrity_scans_active_db_path() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        let external_db_dir = tmp.path().join("external-db");
        let external_db = external_db_dir.join("custom.db");
        let recovery = external_db_dir.join(".br_recovery");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&recovery).unwrap();
        write_fake_sqlite_db(&external_db, 0);

        let artifact = recovery.join("custom.db.bak");
        fs::write(&artifact, b"actual bytes").unwrap();
        let fingerprint = serde_json::json!({
            "artifact": "custom.db.bak",
            "sha256": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        });
        fs::write(
            recovery.join("custom.db.bak.fingerprint.json"),
            serde_json::to_string_pretty(&fingerprint).unwrap(),
        )
        .unwrap();

        let outcome = gate_recovery_fingerprint_integrity(&beads_dir, &external_db);
        assert!(
            matches!(outcome, GateOutcome::Refuse { .. }),
            "direct recovery gate must inspect the active db path"
        );
        let GateOutcome::Refuse { evidence, .. } = outcome else {
            return;
        };
        assert_eq!(evidence["recovery_dir"], recovery.display().to_string());
    }

    #[test]
    fn run_all_scans_recovery_dir_for_active_db_path() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        let external_db_dir = tmp.path().join("external-db");
        let external_db = external_db_dir.join("custom.db");
        let recovery = external_db_dir.join(".br_recovery");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&recovery).unwrap();
        write_fake_sqlite_db(&external_db, 0);

        let artifact = recovery.join("custom.db.bak");
        fs::write(&artifact, b"actual bytes").unwrap();
        let fingerprint = serde_json::json!({
            "artifact": "custom.db.bak",
            "sha256": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        });
        fs::write(
            recovery.join("custom.db.bak.fingerprint.json"),
            serde_json::to_string_pretty(&fingerprint).unwrap(),
        )
        .unwrap();

        let outcome = run_all(&beads_dir, &external_db);
        assert!(
            matches!(outcome, GateOutcome::Refuse { .. }),
            "must refuse mismatched fingerprints beside the active db path"
        );
        let GateOutcome::Refuse { evidence, .. } = outcome else {
            return;
        };
        assert_eq!(evidence["gate"], "recovery_fingerprint_integrity");
        assert_eq!(evidence["recovery_dir"], recovery.display().to_string());
    }

    #[test]
    fn recovery_dir_for_returns_active_db_recovery_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        let db_path = tmp.path().join("configured-db").join("custom.db");

        assert_eq!(
            recovery_dir_for(&beads_dir, &db_path),
            db_path.parent().unwrap().join(".br_recovery")
        );
    }

    #[test]
    fn recovery_fingerprint_integrity_refuses_artifact_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let recovery = beads_dir.join(".br_recovery");
        fs::create_dir_all(&recovery).unwrap();

        let outside = beads_dir.join("outside.bin");
        fs::write(&outside, b"outside bytes").unwrap();
        let outside_sha = hex_encode(&Sha256::digest(b"outside bytes"));
        let fingerprint = serde_json::json!({
            "artifact": "../outside.bin",
            "sha256": outside_sha,
        });
        fs::write(
            recovery.join("escape.fingerprint.json"),
            serde_json::to_string_pretty(&fingerprint).unwrap(),
        )
        .unwrap();

        let outcome = gate_recovery_fingerprint_integrity(&beads_dir, &db_path);
        assert!(
            matches!(outcome, GateOutcome::Refuse { .. }),
            "must refuse artifact path traversal"
        );
        let GateOutcome::Refuse { evidence, .. } = outcome else {
            return;
        };
        assert_eq!(evidence["gate"], "recovery_fingerprint_integrity");
        let arr = evidence["mismatched_artifacts"].as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert!(
            arr[0]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("inside the recovery directory")),
            "unexpected evidence: {evidence}"
        );
    }
}
