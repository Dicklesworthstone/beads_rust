//! `br doctor --bundle <out.tar.gz>`: the incident-evidence capture that
//! `docs/reliability/HEALTH_CONTRACT.md` and `docs/TROUBLESHOOTING.md` ask
//! reporters to assemble by hand (bead v7o2.4).
//!
//! The bundle is a gzip'd tar with:
//!
//! - `manifest.json` — schema, br version, engine block, platform, member
//!   list with the exit code of every captured command;
//! - `doctor.json`, `doctor-repair-dry-run.json`, `health.json`,
//!   `sync-status.json`, `where.json`, `config-list.txt`, `version.txt` —
//!   this binary re-invoked on the same workspace (each command's stderr
//!   lands next to it as `<name>.stderr.txt` when non-empty);
//! - `listings.json` — names, sizes, and mtimes under `.beads/`,
//!   `.beads/.br_recovery/`, and `.beads/.br_history/`;
//! - `db-family.json` — presence, size, mtime, and SHA-256 of every engine
//!   family member (`beads.db`, `-wal`, `-shm`, `-journal`, certificates,
//!   namespace files);
//! - `db-dump.json` — the `metadata` table, `sqlite_master`, and the most
//!   recent events, read through a read-only engine connection (an open or
//!   query failure is recorded in the member, never fatal);
//! - `metadata.json` and `config.yaml` copies.
//!
//! Database bytes are never included unless `--include-db` is passed, and
//! `issues.jsonl` only with `--include-jsonl`; without them the bundle
//! carries their SHA-256 and size instead. Every text member has e-mail
//! addresses replaced with `<redacted-email>`.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::{Instant, SystemTime};

use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::write::GzEncoder;
use fsqlite_types::SqliteValue;
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::cli::DoctorArgs;
use crate::cli::commands::doctor_subsystems::engine::engine_block;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::franken_sync::compat::{OpenFlags, open_with_flags};
use crate::output::OutputContext;

/// Schema id of the receipt printed after a bundle is written and of
/// `manifest.json` inside it.
pub const BUNDLE_SCHEMA: &str = "br.doctor.bundle.v1";

/// Engine family members inventoried (and, with `--include-db`, copied).
const DB_FAMILY_SUFFIXES: [&str; 8] = [
    "",
    "-wal",
    "-shm",
    "-journal",
    "-wal-cert",
    "-wal-cert-head",
    "-fsqlite-ns-gate",
    "-fsqlite-ns-use",
];

/// Family members whose bytes `--include-db` copies (the certificate and
/// namespace files are derived state and stay inventory-only).
const DB_FAMILY_COPIED: [&str; 4] = ["", "-wal", "-shm", "-journal"];

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}")
        .expect("static e-mail pattern compiles")
});

/// One entry of the bundle, as listed in `manifest.json` and the receipt.
#[derive(Debug, Clone, Serialize)]
pub struct BundleMember {
    pub name: String,
    pub bytes: u64,
    /// Exit code of the captured command, for members that are command
    /// output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// What `br doctor --bundle` prints (JSON) after writing the archive.
#[derive(Debug, Serialize)]
pub struct BundleReceipt {
    pub schema: &'static str,
    pub path: String,
    pub bytes: u64,
    pub member_count: usize,
    pub members: Vec<BundleMember>,
    pub include_db: bool,
    pub include_jsonl: bool,
    pub redacted_emails: usize,
    pub elapsed_ms: u128,
}

struct Bundle {
    builder: tar::Builder<GzEncoder<File>>,
    members: Vec<BundleMember>,
    redacted_emails: usize,
    mtime: u64,
}

impl Bundle {
    fn create(path: &Path) -> Result<Self> {
        let file = File::create_new(path).map_err(|err| {
            BeadsError::validation(
                "bundle",
                format!(
                    "cannot create {} (an existing file is never overwritten): {err}",
                    path.display()
                ),
            )
        })?;
        let mtime = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        Ok(Self {
            builder: tar::Builder::new(GzEncoder::new(file, Compression::default())),
            members: Vec::new(),
            redacted_emails: 0,
            mtime,
        })
    }

    /// Add a text member with e-mail addresses redacted.
    fn add_text(&mut self, name: &str, text: &str, exit_code: Option<i32>) -> Result<()> {
        let redacted = EMAIL.replace_all(text, "<redacted-email>");
        self.redacted_emails += EMAIL.find_iter(text).count();
        self.add_bytes(name, redacted.as_bytes(), exit_code)
    }

    fn add_json(&mut self, name: &str, value: &Value) -> Result<()> {
        let text = serde_json::to_string_pretty(value)?;
        self.add_text(name, &text, None)
    }

    /// Add raw bytes verbatim (database files; never redacted).
    fn add_bytes(&mut self, name: &str, bytes: &[u8], exit_code: Option<i32>) -> Result<()> {
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        header.set_mode(0o644);
        header.set_mtime(self.mtime);
        header.set_cksum();
        self.builder.append_data(&mut header, name, bytes)?;
        self.members.push(BundleMember {
            name: name.to_string(),
            bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            exit_code,
        });
        Ok(())
    }

    fn finish(self) -> Result<(Vec<BundleMember>, usize)> {
        let encoder = self.builder.into_inner()?;
        let file = encoder.finish()?;
        file.sync_all()?;
        Ok((self.members, self.redacted_emails))
    }
}

/// Run `br doctor --bundle`.
///
/// # Errors
///
/// Returns an error when the workspace cannot be discovered, the output
/// path already exists or cannot be written, or the archive cannot be
/// finished. Failures of the captured commands or of the read-only database
/// dump are recorded inside the bundle instead.
pub fn execute(args: &DoctorArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let started = Instant::now();
    let out_path = args
        .bundle
        .as_ref()
        .ok_or_else(|| BeadsError::validation("bundle", "no output path"))?;
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let paths = config::resolve_paths(&beads_dir, cli.db.as_ref())?;
    let root = beads_dir
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let exe = std::env::current_exe().map_err(|err| {
        BeadsError::validation("bundle", format!("cannot locate the br executable: {err}"))
    })?;
    let display_path = out_path.display().to_string();
    let gzip_extension = out_path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gz") || ext.eq_ignore_ascii_case("tgz"));
    if !gzip_extension {
        tracing::warn!(path = %display_path, "bundle path does not end in .tar.gz");
    }

    let mut bundle = Bundle::create(out_path)?;
    capture_commands(&mut bundle, &exe, &root, &beads_dir)?;
    bundle.add_json("listings.json", &listings(&beads_dir))?;
    let family = db_family(&paths.db_path);
    bundle.add_json("db-family.json", &family)?;
    bundle.add_json(
        "db-dump.json",
        &read_only_dump(&paths.db_path, args.bundle_events),
    )?;
    copy_text_member(
        &mut bundle,
        "metadata.json",
        &beads_dir.join("metadata.json"),
    )?;
    copy_text_member(&mut bundle, "config.yaml", &beads_dir.join("config.yaml"))?;
    add_jsonl(&mut bundle, &paths.jsonl_path, args.include_jsonl)?;
    if args.include_db {
        add_db_bytes(&mut bundle, &paths.db_path)?;
    }

    let manifest = json!({
        "schema": BUNDLE_SCHEMA,
        "created_at": Utc::now().to_rfc3339(),
        "br_version": env!("CARGO_PKG_VERSION"),
        "git_sha": option_env!("VERGEN_GIT_SHA"),
        "platform": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "family": std::env::consts::FAMILY,
        },
        "engine": engine_block(&beads_dir, &paths.db_path),
        "beads_dir": beads_dir.display().to_string(),
        "workspace_root": root.display().to_string(),
        "include_db": args.include_db,
        "include_jsonl": args.include_jsonl,
        "events_requested": args.bundle_events,
        "redaction": "e-mail addresses replaced with <redacted-email> in every text member",
        "members": bundle.members,
    });
    bundle.add_json("manifest.json", &manifest)?;
    let (members, redacted_emails) = bundle.finish()?;
    let bytes = fs::metadata(out_path).map(|meta| meta.len()).unwrap_or(0);

    let receipt = BundleReceipt {
        schema: BUNDLE_SCHEMA,
        path: display_path,
        bytes,
        member_count: members.len(),
        members,
        include_db: args.include_db,
        include_jsonl: args.include_jsonl,
        redacted_emails,
        elapsed_ms: started.elapsed().as_millis(),
    };
    tracing::info!(path = %receipt.path, bytes, members = receipt.member_count, "bundle.written");
    print_receipt(&receipt, ctx);
    Ok(())
}

fn print_receipt(receipt: &BundleReceipt, ctx: &OutputContext) {
    if ctx.is_json() {
        ctx.json_pretty(receipt);
        return;
    }
    if ctx.is_quiet() {
        return;
    }
    println!(
        "Wrote {} ({} members, {} bytes, {} e-mail addresses redacted)",
        receipt.path, receipt.member_count, receipt.bytes, receipt.redacted_emails
    );
    println!(
        "  database bytes: {}; issues.jsonl: {}",
        if receipt.include_db {
            "included (--include-db)"
        } else {
            "omitted (SHA-256 and size recorded; pass --include-db to attach them)"
        },
        if receipt.include_jsonl {
            "included (--include-jsonl)"
        } else {
            "omitted (SHA-256 and line count recorded; pass --include-jsonl to attach it)"
        }
    );
    for member in &receipt.members {
        match member.exit_code {
            Some(code) => println!("  {:<32} {:>10} B  exit {code}", member.name, member.bytes),
            None => println!("  {:<32} {:>10} B", member.name, member.bytes),
        }
    }
}

/// Re-invoke this binary for every diagnostic the troubleshooting guide asks
/// for, capturing stdout as the member and stderr beside it.
fn capture_commands(bundle: &mut Bundle, exe: &Path, root: &Path, beads_dir: &Path) -> Result<()> {
    let captures: [(&str, &[&str]); 7] = [
        ("version.txt", &["--version"]),
        ("doctor.json", &["doctor", "--json"]),
        (
            "doctor-repair-dry-run.json",
            &["doctor", "--repair", "--dry-run", "--json"],
        ),
        ("health.json", &["doctor", "health", "--json"]),
        ("sync-status.json", &["sync", "--status", "--json"]),
        ("where.json", &["where", "--json"]),
        ("config-list.txt", &["config", "list", "-v"]),
    ];
    for (name, argv) in captures {
        let output = Command::new(exe)
            .current_dir(root)
            .args(argv)
            .env("BEADS_DIR", beads_dir)
            .env("NO_COLOR", "1")
            .env_remove("BR_OUTPUT_FORMAT")
            .env_remove("TOON_DEFAULT_FORMAT")
            .output();
        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                bundle.add_text(name, &stdout, output.status.code())?;
                if !output.stderr.is_empty() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    bundle.add_text(&format!("{name}.stderr.txt"), &stderr, None)?;
                }
            }
            Err(err) => {
                bundle.add_text(
                    &format!("{name}.stderr.txt"),
                    &format!("could not run br {}: {err}\n", argv.join(" ")),
                    None,
                )?;
            }
        }
    }
    Ok(())
}

fn modified_rfc3339(meta: &fs::Metadata) -> Option<String> {
    meta.modified()
        .ok()
        .map(|time| DateTime::<Utc>::from(time).to_rfc3339())
}

fn listing(dir: &Path) -> Value {
    let Ok(entries) = fs::read_dir(dir) else {
        return json!({ "path": dir.display().to_string(), "present": dir.exists(), "entries": [] });
    };
    let mut rows: Vec<Value> = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata().ok();
            json!({
                "name": name,
                "kind": meta.as_ref().map(|meta| {
                    if meta.file_type().is_symlink() { "symlink" }
                    else if meta.is_dir() { "dir" }
                    else { "file" }
                }),
                "bytes": meta.as_ref().map(fs::Metadata::len),
                "modified": meta.as_ref().and_then(modified_rfc3339),
            })
        })
        .collect();
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    json!({ "path": dir.display().to_string(), "present": true, "entries": rows })
}

fn listings(beads_dir: &Path) -> Value {
    json!({
        "beads_dir": listing(beads_dir),
        "recovery": listing(&beads_dir.join(".br_recovery")),
        "history": listing(&beads_dir.join(".br_history")),
    })
}

fn sha256_of_file(path: &Path) -> Option<(u64, String)> {
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += u64::try_from(read).unwrap_or(0);
    }
    Some((total, crate::util::hex_encode(&hasher.finalize())))
}

fn db_family(db_path: &Path) -> Value {
    let base = db_path.to_string_lossy();
    let members: Vec<Value> = DB_FAMILY_SUFFIXES
        .iter()
        .map(|suffix| {
            let path = PathBuf::from(format!("{base}{suffix}"));
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() => {
                    let digest = sha256_of_file(&path);
                    json!({
                        "name": name,
                        "present": true,
                        "bytes": meta.len(),
                        "modified": modified_rfc3339(&meta),
                        "sha256": digest.map(|(_, hex)| hex),
                    })
                }
                Ok(meta) => json!({
                    "name": name,
                    "present": true,
                    "kind": if meta.is_dir() { "dir" } else { "symlink" },
                }),
                Err(_) => json!({ "name": name, "present": false }),
            }
        })
        .collect();
    json!({ "db_path": db_path.display().to_string(), "members": members })
}

fn sqlite_value_json(value: &SqliteValue) -> Value {
    match value {
        SqliteValue::Null => Value::Null,
        SqliteValue::Integer(value) => json!(value),
        SqliteValue::Float(value) => json!(value),
        SqliteValue::Text(value) => {
            Value::String(String::from_utf8_lossy(value.as_bytes()).into_owned())
        }
        SqliteValue::Blob(value) => json!({ "blob_bytes": value.as_ref().len() }),
    }
}

fn query_rows(
    conn: &crate::franken_sync::Connection,
    sql: &str,
    columns: &[&str],
) -> std::result::Result<Vec<Value>, String> {
    let rows = conn.query(sql).map_err(|err| err.to_string())?;
    Ok(rows
        .iter()
        .map(|row| {
            let object: serde_json::Map<String, Value> = columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    (
                        (*column).to_string(),
                        row.get(index).map_or(Value::Null, sqlite_value_json),
                    )
                })
                .collect();
            Value::Object(object)
        })
        .collect())
}

/// The `metadata` table, `sqlite_master`, and the newest `events` rows via a
/// read-only engine connection. Every failure is recorded in the value.
fn read_only_dump(db_path: &Path, events: usize) -> Value {
    if !db_path.is_file() {
        return json!({ "opened": false, "error": "database file absent" });
    }
    let conn = match open_with_flags(&db_path.to_string_lossy(), OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(err) => return json!({ "opened": false, "error": err.to_string() }),
    };
    let metadata = query_rows(
        &conn,
        "SELECT key, value FROM metadata ORDER BY key",
        &["key", "value"],
    );
    let schema = query_rows(
        &conn,
        "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name",
        &["type", "name", "tbl_name", "sql"],
    );
    let recent_events = query_rows(
        &conn,
        &format!(
            "SELECT id, issue_id, event_type, actor, old_value, new_value, comment, created_at \
             FROM events ORDER BY id DESC LIMIT {events}"
        ),
        &[
            "id",
            "issue_id",
            "event_type",
            "actor",
            "old_value",
            "new_value",
            "comment",
            "created_at",
        ],
    );
    let _ = conn.close();
    let section = |result: std::result::Result<Vec<Value>, String>| match result {
        Ok(rows) => json!({ "rows": rows }),
        Err(error) => json!({ "error": error }),
    };
    json!({
        "opened": true,
        "metadata": section(metadata),
        "sqlite_master": section(schema),
        "recent_events": section(recent_events),
    })
}

fn copy_text_member(bundle: &mut Bundle, name: &str, path: &Path) -> Result<()> {
    match fs::read_to_string(path) {
        Ok(text) => bundle.add_text(name, &text, None),
        Err(err) => bundle.add_text(
            &format!("{name}.missing.txt"),
            &format!("{}: {err}\n", path.display()),
            None,
        ),
    }
}

fn add_jsonl(bundle: &mut Bundle, jsonl_path: &Path, include: bool) -> Result<()> {
    if include {
        return copy_text_member(bundle, "issues.jsonl", jsonl_path);
    }
    let summary = match sha256_of_file(jsonl_path) {
        Some((bytes, sha256)) => {
            let lines = fs::read_to_string(jsonl_path)
                .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
                .ok();
            json!({
                "path": jsonl_path.display().to_string(),
                "present": true,
                "bytes": bytes,
                "sha256": sha256,
                "records": lines,
                "note": "pass --include-jsonl to attach the file",
            })
        }
        None => json!({ "path": jsonl_path.display().to_string(), "present": false }),
    };
    bundle.add_json("issues.jsonl.summary.json", &summary)
}

fn add_db_bytes(bundle: &mut Bundle, db_path: &Path) -> Result<()> {
    let base = db_path.to_string_lossy();
    for suffix in DB_FAMILY_COPIED {
        let path = PathBuf::from(format!("{base}{suffix}"));
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        tracing::warn!(
            path = %path.display(),
            bytes = meta.len(),
            "attaching database bytes to the bundle (--include-db)"
        );
        let bytes = fs::read(&path)?;
        bundle.add_bytes(&format!("db/{name}"), &bytes, None)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_redaction_replaces_every_address_and_counts_them() {
        let text = "owner: alice@example.com, cc bob.smith+tag@mail.example.org; not-an-email @x";
        let redacted = EMAIL.replace_all(text, "<redacted-email>");
        assert_eq!(
            redacted,
            "owner: <redacted-email>, cc <redacted-email>; not-an-email @x"
        );
        assert_eq!(EMAIL.find_iter(text).count(), 2);
    }

    #[test]
    fn db_family_reports_absent_members_without_hashes() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        fs::write(&db_path, b"not really a database").expect("write db");
        let family = db_family(&db_path);
        let members = family["members"].as_array().expect("members");
        assert_eq!(members.len(), DB_FAMILY_SUFFIXES.len());
        assert_eq!(members[0]["name"], "beads.db");
        assert_eq!(members[0]["present"], true);
        assert_eq!(members[0]["bytes"], 21);
        assert!(
            members[0]["sha256"]
                .as_str()
                .is_some_and(|hex| hex.len() == 64)
        );
        assert_eq!(members[1]["name"], "beads.db-wal");
        assert_eq!(members[1]["present"], false);
        assert!(members[1].get("sha256").is_none());
    }

    #[test]
    fn read_only_dump_records_an_unreadable_database_instead_of_failing() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let missing = read_only_dump(&temp.path().join("absent.db"), 5);
        assert_eq!(missing["opened"], false);
        let db_path = temp.path().join("beads.db");
        drop(crate::storage::SqliteStorage::open(&db_path).expect("create schema"));
        let dump = read_only_dump(&db_path, 5);
        assert_eq!(dump["opened"], true, "{dump}");
        assert!(
            dump["sqlite_master"]["rows"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row["name"] == "issues")),
            "{dump}"
        );
    }
}
