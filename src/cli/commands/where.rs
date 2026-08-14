//! Where command implementation.

use crate::config;
use crate::config::routing::follow_redirects;
use crate::error::BeadsError;
use crate::error::Result;
use crate::format::sanitize_terminal_inline;
use crate::franken_sync::Connection;
use crate::output::OutputContext;
use crate::util::parse_id;
use fsqlite_types::SqliteValue;
use rich_rust::prelude::*;
use serde::Serialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize)]
struct WhereOutput {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    redirected_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    database_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jsonl_path: Option<String>,
}

/// Execute the where command.
///
/// # Errors
///
/// Returns an error if redirect resolution fails.
pub fn execute(cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let Some(output) = resolve_where_output(cli)? else {
        return Err(BeadsError::NotInitialized);
    };

    if matches!(ctx.mode(), crate::output::OutputMode::Quiet) {
        return Ok(());
    }

    if ctx.is_json() {
        ctx.json_pretty(&output);
    } else if ctx.is_toon() {
        ctx.toon(&output);
    } else if ctx.is_rich() {
        render_where_rich(&output, ctx);
    } else {
        print_human(&output);
    }

    Ok(())
}

fn resolve_where_output(cli: &config::CliOverrides) -> Result<Option<WhereOutput>> {
    let Some(source_dir) = config::discover_optional_beads_dir_candidate_with_cli(cli)? else {
        return Ok(None);
    };

    let final_dir = follow_redirects(&source_dir, 10)?;
    let redirected_from = if final_dir == source_dir {
        None
    } else {
        Some(canonicalize_lossy(&source_dir).display().to_string())
    };

    let paths = config::resolve_paths(&final_dir, cli.db.as_ref())?;
    let database_path = canonicalize_lossy(&paths.db_path).display().to_string();
    let jsonl_path = canonicalize_lossy(&paths.jsonl_path).display().to_string();
    let prefix = detect_prefix(&final_dir, &paths.db_path, &paths.jsonl_path, cli);

    Ok(Some(WhereOutput {
        path: canonicalize_lossy(&final_dir).display().to_string(),
        redirected_from,
        prefix,
        database_path: Some(database_path),
        jsonl_path: Some(jsonl_path),
    }))
}

fn detect_prefix(
    beads_dir: &Path,
    db_path: &Path,
    jsonl_path: &Path,
    cli: &config::CliOverrides,
) -> Option<String> {
    if let Ok(startup) = config::load_startup_config_with_paths(beads_dir, cli.db.as_ref())
        && let Some(prefix) =
            config::configured_issue_prefix_from_map(&startup.merged_config.runtime)
    {
        return Some(prefix);
    }

    match inspect_jsonl_prefix(jsonl_path) {
        JsonlPrefixState::Detected(prefix) => {
            return configured_prefix_from_db_without_recovery(db_path).or(Some(prefix));
        }
        JsonlPrefixState::Mixed => return configured_prefix_from_db_without_recovery(db_path),
        JsonlPrefixState::Missing => {}
    }

    prefix_from_db_without_recovery(db_path)
}

fn configured_prefix_from_db_without_recovery(db_path: &Path) -> Option<String> {
    if !db_path.is_file() {
        return None;
    }

    config::with_database_family_snapshot(db_path, |snapshot_db_path| {
        let conn = Connection::open(snapshot_db_path.to_string_lossy().into_owned())?;

        let prefix = conn
            .query(
                "SELECT value FROM config \
                 WHERE key IN ('issue_prefix', 'issue-prefix', 'prefix') \
                 ORDER BY CASE key \
                     WHEN 'issue_prefix' THEN 0 \
                     WHEN 'issue-prefix' THEN 1 \
                     ELSE 2 \
                 END \
                 LIMIT 1",
            )
            .ok()
            .and_then(|rows| rows.first().cloned())
            .and_then(|row| {
                row.get(0)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string)
            })
            .map(|prefix| prefix.trim().to_string())
            .filter(|prefix| !prefix.is_empty());
        conn.close()?;
        Ok(prefix)
    })
    .ok()
    .flatten()
}

fn prefix_from_db_without_recovery(db_path: &Path) -> Option<String> {
    configured_prefix_from_db_without_recovery(db_path).or_else(|| {
        if !db_path.is_file() {
            return None;
        }

        config::with_database_family_snapshot(db_path, |snapshot_db_path| {
            let conn = Connection::open(snapshot_db_path.to_string_lossy().into_owned())?;
            let prefix = conn
                .query("SELECT id FROM issues ORDER BY rowid LIMIT 1")
                .ok()
                .and_then(|rows| rows.first().cloned())
                .and_then(|row| {
                    row.get(0)
                        .and_then(SqliteValue::as_text)
                        .map(str::to_string)
                })
                .and_then(|id| parse_id(&id).ok().map(|parsed| parsed.prefix));
            conn.close()?;
            Ok(prefix)
        })
        .ok()
        .flatten()
    })
}

#[cfg(test)]
fn prefix_from_jsonl(path: &Path) -> Option<String> {
    match inspect_jsonl_prefix(path) {
        JsonlPrefixState::Detected(prefix) => Some(prefix),
        JsonlPrefixState::Missing | JsonlPrefixState::Mixed => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JsonlPrefixState {
    Missing,
    Detected(String),
    Mixed,
}

fn inspect_jsonl_prefix(path: &Path) -> JsonlPrefixState {
    if !path.is_file() {
        return JsonlPrefixState::Missing;
    }

    let Ok(file) = File::open(path) else {
        return JsonlPrefixState::Missing;
    };
    let reader = BufReader::new(file);
    let mut detected_prefix: Option<String> = None;

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

        // Skip tombstones — they may retain a foreign prefix from before
        // a prefix migration and should not cause mixed-prefix detection.
        if value
            .get("status")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == "tombstone")
        {
            continue;
        }

        let Some(id) = value.get("id").and_then(|value| value.as_str()) else {
            continue;
        };

        let Ok(parsed) = parse_id(id) else {
            continue;
        };
        match &detected_prefix {
            None => detected_prefix = Some(parsed.prefix),
            Some(prefix) if *prefix == parsed.prefix => {}
            Some(_) => return JsonlPrefixState::Mixed,
        }
    }

    detected_prefix.map_or(JsonlPrefixState::Missing, JsonlPrefixState::Detected)
}

fn print_human(output: &WhereOutput) {
    println!("{}", where_display_text(&output.path));
    if let Some(origin) = &output.redirected_from {
        println!("  (via redirect from {})", where_display_text(origin));
    }
    if let Some(prefix) = &output.prefix {
        println!("  prefix: {}", where_display_text(prefix));
    }
    if let Some(db_path) = &output.database_path {
        println!("  database: {}", where_display_text(db_path));
    }
    if let Some(jsonl_path) = &output.jsonl_path {
        println!("  jsonl: {}", where_display_text(jsonl_path));
    }
}

fn where_display_text(value: &str) -> String {
    sanitize_terminal_inline(value).into_owned()
}

/// Render location info as a rich panel.
fn render_where_rich(output: &WhereOutput, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    // Main path
    content.append_styled("Directory   ", theme.dimmed.clone());
    content.append_styled(&where_display_text(&output.path), theme.accent.clone());
    content.append("\n");

    // Redirect info
    if let Some(origin) = &output.redirected_from {
        content.append_styled("            ", theme.dimmed.clone());
        content.append_styled("(via redirect from ", theme.muted.clone());
        content.append_styled(&where_display_text(origin), theme.accent.clone());
        content.append_styled(")\n", theme.muted.clone());
    }

    // Prefix
    if let Some(prefix) = &output.prefix {
        content.append_styled("Prefix      ", theme.dimmed.clone());
        content.append_styled(&where_display_text(prefix), theme.issue_id.clone());
        content.append("\n");
    }

    // Database path
    if let Some(db_path) = &output.database_path {
        content.append_styled("Database    ", theme.dimmed.clone());
        content.append_styled(&where_display_text(db_path), theme.accent.clone());
        content.append("\n");
    }

    // JSONL path
    if let Some(jsonl_path) = &output.jsonl_path {
        content.append_styled("JSONL       ", theme.dimmed.clone());
        content.append_styled(&where_display_text(jsonl_path), theme.accent.clone());
        content.append("\n");
    }

    let title = output.prefix.as_ref().map_or_else(
        || "Beads Location".to_string(),
        |p| format!("{} Location", where_display_text(p)),
    );

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(&title, theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

fn canonicalize_lossy(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CliOverrides;
    use crate::storage::SqliteStorage;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    struct DirGuard {
        previous: PathBuf,
    }

    impl DirGuard {
        fn new(target: &Path) -> Self {
            let previous = env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));
            env::set_current_dir(target).expect("set current dir");
            Self { previous }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.previous);
        }
    }

    #[test]
    fn where_display_text_sanitizes_terminal_controls() {
        let rendered = where_display_text("/tmp/ws\x1b[2J/.beads\rhidden\x08\nnext\x07\u{9b}");

        assert!(!rendered.chars().any(char::is_control));
        assert!(rendered.contains("\\u{1b}[2J"));
        assert!(rendered.contains("\\r"));
        assert!(rendered.contains("\\u{8}"));
        assert!(rendered.contains("\\n"));
        assert!(rendered.contains("\\u{7}"));
        assert!(rendered.contains("\\u{9b}"));
    }

    #[test]
    fn resolve_where_output_uses_explicit_db_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join("external").join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::write(&jsonl_path, r#"{"id":"proj-abc12","title":"Example"}"#).expect("write jsonl");

        let cli = CliOverrides {
            db: Some(db_path.clone()),
            ..CliOverrides::default()
        };

        let output = resolve_where_output(&cli)
            .expect("where output")
            .expect("workspace output");

        assert_eq!(
            output.path,
            canonicalize_lossy(&beads_dir).display().to_string()
        );
        assert_eq!(
            output.database_path,
            Some(canonicalize_lossy(&db_path).display().to_string())
        );
        assert_eq!(
            output.jsonl_path,
            Some(canonicalize_lossy(&jsonl_path).display().to_string())
        );
        assert_eq!(output.prefix.as_deref(), Some("proj"));
    }

    #[test]
    fn resolve_where_output_preserves_redirect_origin() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let source_root = temp.path().join("source");
        let target_root = temp.path().join("target");
        let source_beads = source_root.join(".beads");
        let target_beads = target_root.join(".beads");

        fs::create_dir_all(&source_beads).expect("create source beads dir");
        fs::create_dir_all(&target_beads).expect("create target beads dir");
        fs::write(source_beads.join("redirect"), "../../target/.beads").expect("write redirect");
        fs::write(
            target_beads.join("issues.jsonl"),
            r#"{"id":"proj-abc12","title":"Example"}"#,
        )
        .expect("write jsonl");

        let _guard = DirGuard::new(&source_root);
        let output = resolve_where_output(&CliOverrides::default())
            .expect("where output")
            .expect("workspace output");

        assert_eq!(
            output.path,
            canonicalize_lossy(&target_beads).display().to_string()
        );
        assert_eq!(
            output.redirected_from,
            Some(canonicalize_lossy(&source_beads).display().to_string())
        );
        assert_eq!(
            output.database_path,
            Some(
                canonicalize_lossy(&target_beads.join("beads.db"))
                    .display()
                    .to_string()
            )
        );
        assert_eq!(
            output.jsonl_path,
            Some(
                canonicalize_lossy(&target_beads.join("issues.jsonl"))
                    .display()
                    .to_string()
            )
        );
        assert_eq!(output.prefix.as_deref(), Some("proj"));
    }

    #[test]
    fn resolve_where_output_does_not_create_missing_db() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("workspace");
        let beads_dir = root.join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let _guard = DirGuard::new(&root);
        let output = resolve_where_output(&CliOverrides::default())
            .expect("where output")
            .expect("workspace output");

        assert_eq!(
            output.database_path,
            Some(
                canonicalize_lossy(&beads_dir.join("beads.db"))
                    .display()
                    .to_string()
            )
        );
        assert!(
            !beads_dir.join("beads.db").exists(),
            "where must not create the database as a side effect"
        );
    }

    #[test]
    fn resolve_where_output_falls_back_for_external_db_override() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("workspace");
        let beads_dir = root.join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let external_db = temp.path().join("cache").join("custom.db");
        let cli = CliOverrides {
            db: Some(external_db.clone()),
            ..CliOverrides::default()
        };

        let _guard = DirGuard::new(&root);
        let output = resolve_where_output(&cli)
            .expect("where output")
            .expect("workspace output");

        assert_eq!(
            output.path,
            canonicalize_lossy(&beads_dir).display().to_string()
        );
        assert_eq!(
            output.database_path,
            Some(canonicalize_lossy(&external_db).display().to_string())
        );
        assert_eq!(
            output.jsonl_path,
            Some(
                canonicalize_lossy(&temp.path().join("cache").join("issues.jsonl"))
                    .display()
                    .to_string()
            )
        );
    }

    #[test]
    fn prefix_from_jsonl_returns_none_for_mixed_prefixes() {
        let temp = TempDir::new().expect("tempdir");
        let jsonl_path = temp.path().join("issues.jsonl");
        fs::write(
            &jsonl_path,
            concat!(
                r#"{"id":"proj-abc12","title":"Example"}"#,
                "\n",
                r#"{"id":"other-def34","title":"Second"}"#,
                "\n",
            ),
        )
        .expect("write jsonl");

        assert_eq!(prefix_from_jsonl(&jsonl_path), None);
    }

    #[test]
    fn prefix_from_jsonl_skips_invalid_json_lines() {
        let temp = TempDir::new().expect("tempdir");
        let jsonl_path = temp.path().join("issues.jsonl");
        fs::write(
            &jsonl_path,
            concat!(
                r#"{"id":"proj-abc12","title":"Example"}"#,
                "\n",
                "{invalid json}\n",
            ),
        )
        .expect("write jsonl");

        assert_eq!(prefix_from_jsonl(&jsonl_path), Some("proj".to_string()));
    }

    #[test]
    fn detect_prefix_prefers_configured_storage_prefix_when_jsonl_prefixes_conflict() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("open db");
        storage
            .set_config("issue_prefix", "proj")
            .expect("set issue prefix");

        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::write(
            &jsonl_path,
            concat!(
                r#"{"id":"proj-abc12","title":"Example"}"#,
                "\n",
                r#"{"id":"other-def34","title":"Second"}"#,
                "\n",
            ),
        )
        .expect("write mixed-prefix jsonl");

        assert_eq!(
            detect_prefix(&beads_dir, &db_path, &jsonl_path, &CliOverrides::default()),
            Some("proj".to_string())
        );
    }

    #[test]
    fn resolve_where_output_accepts_startup_prefix_alias() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("workspace");
        let beads_dir = root.join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(beads_dir.join("config.yaml"), "prefix: proj\n").expect("write config");

        let _guard = DirGuard::new(&root);
        let output = resolve_where_output(&CliOverrides::default())
            .expect("where output")
            .expect("workspace output");

        assert_eq!(output.prefix.as_deref(), Some("proj"));
    }

    #[test]
    fn detect_prefix_prefers_db_prefix_over_jsonl_inference() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("open db");
        storage.set_config("prefix", "dbpref").expect("set prefix");

        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::write(&jsonl_path, r#"{"id":"jsonl-abc12","title":"Example"}"#).expect("write jsonl");

        assert_eq!(
            detect_prefix(&beads_dir, &db_path, &jsonl_path, &CliOverrides::default()),
            Some("dbpref".to_string())
        );
    }
}
