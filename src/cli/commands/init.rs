use crate::error::{BeadsError, Result};
use crate::output::{OutputContext, OutputMode};
use crate::storage::SqliteStorage;
use crate::util::db_path;
use rich_rust::prelude::*;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Resolve the beads directory for init.
///
/// Honors `BEADS_DIR` env var if set, otherwise falls back to `base_dir/.beads`.
fn resolve_init_beads_dir(base_dir: &Path) -> PathBuf {
    if let Ok(value) = std::env::var("BEADS_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    base_dir.join(".beads")
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn create_temp_init_file(path: &Path) -> Result<(PathBuf, File)> {
    let pid = std::process::id();
    let base_extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("tmp");

    for attempt in 0..64_u32 {
        let extension = if attempt == 0 {
            format!("{base_extension}.{pid}.tmp")
        } else {
            format!("{base_extension}.{pid}.{attempt}.tmp")
        };
        let temp_path = path.with_extension(extension);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }

    Err(BeadsError::Config(format!(
        "Failed to allocate temp init file for {}",
        path.display()
    )))
}

fn write_init_file_if_missing(path: &Path, contents: &[u8]) -> Result<bool> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            if let Err(error) = file.write_all(contents) {
                drop(file);
                let _ = fs::remove_file(path);
                return Err(error.into());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn write_init_file_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let existing_permissions = fs::symlink_metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|metadata| metadata.permissions());
    let (temp_path, mut temp_file) = create_temp_init_file(path)?;
    if let Some(permissions) = existing_permissions
        && let Err(error) = fs::set_permissions(&temp_path, permissions)
    {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "Failed to apply original init file permissions before atomic rewrite"
        );
    }
    if let Err(error) = temp_file
        .write_all(contents)
        .and_then(|()| temp_file.sync_all())
    {
        drop(temp_file);
        let _ = fs::remove_file(&temp_path);
        return Err(error.into());
    }
    drop(temp_file);
    crate::util::durable_rename(&temp_path, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp_path);
    })?;
    Ok(())
}

/// Execute the init command.
///
/// # Errors
///
/// Returns an error if the directory or database cannot be created.
#[allow(clippy::too_many_lines)]
pub fn execute(
    prefix: Option<String>,
    force: bool,
    root_dir: Option<&Path>,
    ctx: &OutputContext,
) -> Result<()> {
    let base_dir = root_dir.unwrap_or_else(|| Path::new("."));
    let beads_dir = resolve_init_beads_dir(base_dir);

    let mut created_dir = false;
    if beads_dir.exists() {
        // Check if DB exists (in cache dir if BEADS_CACHE_DIR is set)
        let effective_db_path = db_path(&beads_dir);
        if effective_db_path.exists() && !force {
            return Err(BeadsError::AlreadyInitialized {
                path: effective_db_path,
            });
        }
    } else {
        fs::create_dir(&beads_dir)?;
        created_dir = true;
    }

    let effective_db_path = db_path(&beads_dir);
    let db_existed = effective_db_path.exists();

    // Ensure cache directory exists if using BEADS_CACHE_DIR
    if let Some(parent) = effective_db_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Initialize DB (creates file and applies schema)
    let mut storage = SqliteStorage::open(&effective_db_path)?;

    // Set prefix in config table if provided, otherwise derive from directory name
    // Normalize to lowercase since ID validation requires lowercase prefixes
    let actual_prefix = prefix.unwrap_or_else(|| {
        let mut dir_name = "br".to_string();
        if let Ok(canon) = dunce::canonicalize(base_dir)
            && let Some(name) = canon.file_name().and_then(|n| n.to_str())
        {
            let cleaned: String = name
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !cleaned.is_empty() {
                dir_name = cleaned;
            }
        }
        dir_name
    });
    let normalized = actual_prefix.to_ascii_lowercase();
    storage.set_config("issue_prefix", &normalized)?;
    let prefix_set = Some(normalized.clone());

    // Write metadata.json
    let metadata_path = beads_dir.join("metadata.json");
    let metadata_existed = path_entry_exists(&metadata_path)?;
    let metadata = r#"{
  "database": "beads.db",
  "jsonl_export": "issues.jsonl"
}"#;
    if force {
        write_init_file_atomically(&metadata_path, metadata.as_bytes())?;
    } else if !metadata_existed {
        write_init_file_if_missing(&metadata_path, metadata.as_bytes())?;
    }

    // Write config.yaml template
    let config_path = beads_dir.join("config.yaml");
    let config_existed = path_entry_exists(&config_path)?;
    if !config_existed {
        let config = format!(
            "# Beads Project Configuration
# issue_prefix: {normalized}
# default_priority: 2
# default_type: task
"
        );
        write_init_file_if_missing(&config_path, config.as_bytes())?;
    }

    // Write .gitignore
    let gitignore_path = beads_dir.join(".gitignore");
    let gitignore_existed = path_entry_exists(&gitignore_path)?;
    if !gitignore_existed {
        let gitignore = r"# Database
*.db
*.db-journal
*.db-shm
# -wal plus fsqlite 0.2+'s -wal-cert / -wal-cert-head durability sidecars
*.db-wal*
# fsqlite multi-process namespace sidecars (-fsqlite-ns-gate / -fsqlite-ns-use),
# including the `.<db>.schema-migration-<run>.vacuum-*` copies left by
# `br doctor migrate-schema` (#427)
*-fsqlite-ns-gate
*-fsqlite-ns-use
*.vacuum-wal-cert*
# fsqlite 0.3.6+ engine-upgrade bookkeeping written beside the database
*.fsqlite-migration-state

# Lock files
.write.lock
*.lock

# Temporary
last-touched
*.tmp

# Local history backups
.br_history/

# DB-family recovery artifacts (truncated WAL/SHM, quarantined sidecars)
# — same lifecycle as .br_history/, written by recovery paths and
# `br doctor --repair`. Recovery artifacts live under their own directory
# rather than relying on the database globs above (#271).
.br_recovery/

# Sync state (local-only, per-machine)
.sync.lock
sync_base.jsonl

# Merge artifacts (temporary files from 3-way merge)
beads.base.jsonl
beads.base.meta.json
beads.left.jsonl
beads.left.meta.json
beads.right.jsonl
beads.right.meta.json

# Daemon runtime files
daemon.lock
daemon.log
daemon.pid
bd.sock
sync-state.json

# Worktree redirect file
redirect

# bv (beads viewer) lock file
.bv.lock
";
        write_init_file_if_missing(&gitignore_path, gitignore.as_bytes())?;
    }

    // Write empty issues.jsonl for compatibility with bv (beads_viewer)
    // bv expects this file to exist even if there are no issues yet
    let jsonl_path = beads_dir.join("issues.jsonl");
    let jsonl_existed = path_entry_exists(&jsonl_path)?;
    if !jsonl_existed {
        write_init_file_if_missing(&jsonl_path, b"")?;
    }

    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    if matches!(ctx.mode(), OutputMode::Rich) {
        let steps = build_init_steps(
            created_dir,
            db_existed,
            metadata_existed,
            force,
            config_existed,
            gitignore_existed,
            jsonl_existed,
            prefix_set.as_deref(),
        );
        render_init_rich(&beads_dir, &steps, prefix_set.as_deref(), ctx);
    } else {
        if let Some(p) = prefix_set.as_deref() {
            println!("Prefix set to: {p}");
        }
        println!("Initialized beads workspace in {}", beads_dir.display());
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum InitStepStatus {
    Created,
    Updated,
    Existing,
}

struct InitStep {
    label: String,
    status: InitStepStatus,
}

#[allow(clippy::fn_params_excessive_bools, clippy::too_many_arguments)]
fn build_init_steps(
    created_dir: bool,
    db_existed: bool,
    metadata_existed: bool,
    force: bool,
    config_existed: bool,
    gitignore_existed: bool,
    jsonl_existed: bool,
    prefix: Option<&str>,
) -> Vec<InitStep> {
    let mut steps = Vec::new();

    steps.push(InitStep {
        label: ".beads/ directory".to_string(),
        status: if created_dir {
            InitStepStatus::Created
        } else {
            InitStepStatus::Existing
        },
    });

    steps.push(InitStep {
        label: "SQLite database (beads.db)".to_string(),
        status: if db_existed {
            InitStepStatus::Existing
        } else {
            InitStepStatus::Created
        },
    });

    let metadata_status = if !metadata_existed {
        InitStepStatus::Created
    } else if force {
        InitStepStatus::Updated
    } else {
        InitStepStatus::Existing
    };
    steps.push(InitStep {
        label: "metadata.json".to_string(),
        status: metadata_status,
    });

    steps.push(InitStep {
        label: "config.yaml".to_string(),
        status: if config_existed {
            InitStepStatus::Existing
        } else {
            InitStepStatus::Created
        },
    });

    steps.push(InitStep {
        label: ".gitignore".to_string(),
        status: if gitignore_existed {
            InitStepStatus::Existing
        } else {
            InitStepStatus::Created
        },
    });

    steps.push(InitStep {
        label: "issues.jsonl (for bv compatibility)".to_string(),
        status: if jsonl_existed {
            InitStepStatus::Existing
        } else {
            InitStepStatus::Created
        },
    });

    if let Some(prefix) = prefix {
        steps.push(InitStep {
            label: format!("Issue prefix set to '{prefix}'"),
            status: InitStepStatus::Updated,
        });
    }

    steps
}

fn render_init_rich(
    beads_dir: &Path,
    steps: &[InitStep],
    prefix: Option<&str>,
    ctx: &OutputContext,
) {
    let theme = ctx.theme();
    let mut content = Text::new("");

    content.append_styled("Workspace initialized\n", theme.emphasis.clone());
    content.append("\n");

    content.append_styled("Location: ", theme.dimmed.clone());
    content.append_styled(&beads_dir.display().to_string(), theme.accent.clone());
    content.append("\n\n");

    content.append_styled("Steps:\n", theme.emphasis.clone());
    for step in steps {
        append_step(&mut content, step, theme);
    }

    content.append("\n");
    content.append_styled("Layout:\n", theme.emphasis.clone());
    content.append("  .beads/\n");
    content.append("    |-- beads.db\n");
    content.append("    |-- metadata.json\n");
    content.append("    |-- config.yaml\n");
    content.append("    |-- .gitignore\n");
    content.append("    `-- issues.jsonl\n");

    content.append("\n");
    content.append_styled("Next steps:\n", theme.emphasis.clone());
    content.append("  br create \"My first issue\"\n");
    content.append("  br list\n");

    if prefix.is_none() {
        content.append("\n");
        content.append_styled(
            "Tip: Set a custom prefix with `br init --prefix <name>`\n",
            theme.dimmed.clone(),
        );
    }

    let panel = Panel::from_rich_text(&content, ctx.width())
        .title(Text::new("Beads Initialized"))
        .box_style(theme.box_style)
        .border_style(theme.panel_border.clone());

    ctx.render(&panel);
}

fn append_step(content: &mut Text, step: &InitStep, theme: &crate::output::Theme) {
    let (icon, style) = match step.status {
        InitStepStatus::Created => ("[+]", theme.success.clone()),
        InitStepStatus::Updated => ("[*]", theme.warning.clone()),
        InitStepStatus::Existing => ("[=]", theme.dimmed.clone()),
    };
    content.append_styled(&format!("{icon} "), style);
    content.append_styled(&step.label, theme.issue_title.clone());
    content.append("\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tracing::info;

    fn init_logging() {
        crate::logging::init_test_logging();
    }

    #[test]
    fn test_init_creates_beads_directory() {
        init_logging();
        info!("test_init_creates_beads_directory: starting");
        let temp_dir = TempDir::new().unwrap();
        let ctx = OutputContext::from_flags(false, false, true);
        let result = execute(None, false, Some(temp_dir.path()), &ctx);

        assert!(result.is_ok());
        assert!(temp_dir.path().join(".beads").exists());
        assert!(temp_dir.path().join(".beads/beads.db").exists());
        assert!(temp_dir.path().join(".beads/metadata.json").exists());
        assert!(temp_dir.path().join(".beads/config.yaml").exists());
        assert!(temp_dir.path().join(".beads/.gitignore").exists());
        assert!(temp_dir.path().join(".beads/issues.jsonl").exists());
        info!("test_init_creates_beads_directory: assertions passed");
    }

    #[cfg(unix)]
    #[test]
    fn test_init_does_not_follow_dangling_config_symlink() {
        use std::os::unix::fs::symlink;

        init_logging();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir(&beads_dir).unwrap();
        let outside_target = temp_dir.path().join("outside-config.yaml");
        let config_path = beads_dir.join("config.yaml");
        symlink(&outside_target, &config_path).unwrap();

        let ctx = OutputContext::from_flags(false, false, true);
        let result = execute(None, false, Some(temp_dir.path()), &ctx);

        assert!(result.is_ok());
        assert!(
            !outside_target.exists(),
            "init must not create a dangling symlink target outside .beads"
        );
        assert!(
            fs::symlink_metadata(&config_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "create-only init files should leave existing symlink entries alone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_init_force_replaces_metadata_symlink_not_target() {
        use std::os::unix::fs::symlink;

        init_logging();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir(&beads_dir).unwrap();
        let outside_target = temp_dir.path().join("outside-metadata.json");
        let metadata_path = beads_dir.join("metadata.json");
        symlink(&outside_target, &metadata_path).unwrap();

        let ctx = OutputContext::from_flags(false, false, true);
        let result = execute(None, true, Some(temp_dir.path()), &ctx);

        assert!(result.is_ok());
        assert!(
            !outside_target.exists(),
            "force init must replace the metadata symlink entry, not write its target"
        );
        assert!(
            fs::metadata(&metadata_path).unwrap().is_file(),
            "metadata should be installed as a regular file"
        );
        assert!(
            !fs::symlink_metadata(&metadata_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "metadata symlink should be replaced by the forced rewrite"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_init_force_preserves_metadata_permissions() {
        use std::os::unix::fs::PermissionsExt;

        init_logging();
        let temp_dir = TempDir::new().unwrap();
        let ctx = OutputContext::from_flags(false, false, true);
        execute(None, false, Some(temp_dir.path()), &ctx).unwrap();

        let metadata_path = temp_dir.path().join(".beads/metadata.json");
        fs::set_permissions(&metadata_path, fs::Permissions::from_mode(0o600)).unwrap();

        execute(None, true, Some(temp_dir.path()), &ctx).unwrap();

        let mode = fs::metadata(&metadata_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "force init should preserve metadata mode");
    }

    #[test]
    fn test_init_with_prefix() {
        init_logging();
        info!("test_init_with_prefix: starting");
        let temp_dir = TempDir::new().unwrap();
        let ctx = OutputContext::from_flags(false, false, true);
        let result = execute(Some("test".to_string()), false, Some(temp_dir.path()), &ctx);

        assert!(result.is_ok());

        // Verify prefix was stored
        let db_path = temp_dir.path().join(".beads/beads.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let prefix = storage.get_config("issue_prefix").unwrap();
        assert_eq!(prefix, Some("test".to_string()));
        info!("test_init_with_prefix: assertions passed");
    }

    #[test]
    fn test_init_fails_if_already_initialized() {
        init_logging();
        info!("test_init_fails_if_already_initialized: starting");
        let temp_dir = TempDir::new().unwrap();
        let ctx = OutputContext::from_flags(false, false, true);

        // First init should succeed
        let result1 = execute(None, false, Some(temp_dir.path()), &ctx);
        assert!(result1.is_ok());

        // Second init without force should fail
        let result2 = execute(None, false, Some(temp_dir.path()), &ctx);

        assert!(result2.is_err());
        assert!(matches!(
            result2.unwrap_err(),
            BeadsError::AlreadyInitialized { .. }
        ));
        info!("test_init_fails_if_already_initialized: assertions passed");
    }

    #[test]
    fn test_init_force_overwrites_existing() {
        init_logging();
        info!("test_init_force_overwrites_existing: starting");
        let temp_dir = TempDir::new().unwrap();
        let ctx = OutputContext::from_flags(false, false, true);

        // First init
        execute(
            Some("first".to_string()),
            false,
            Some(temp_dir.path()),
            &ctx,
        )
        .unwrap();

        // Second init with force
        let result = execute(
            Some("second".to_string()),
            true,
            Some(temp_dir.path()),
            &ctx,
        );

        assert!(result.is_ok());

        // Verify new prefix
        let db_path = temp_dir.path().join(".beads/beads.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let prefix = storage.get_config("issue_prefix").unwrap();
        assert_eq!(prefix, Some("second".to_string()));
        info!("test_init_force_overwrites_existing: assertions passed");
    }

    #[test]
    fn test_metadata_json_content() {
        init_logging();
        info!("test_metadata_json_content: starting");
        let temp_dir = TempDir::new().unwrap();
        let ctx = OutputContext::from_flags(false, false, true);
        execute(None, false, Some(temp_dir.path()), &ctx).unwrap();

        let metadata_path = temp_dir.path().join(".beads/metadata.json");
        let content = fs::read_to_string(metadata_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert_eq!(parsed["database"], "beads.db");
        assert_eq!(parsed["jsonl_export"], "issues.jsonl");
        info!("test_metadata_json_content: assertions passed");
    }

    #[test]
    fn test_gitignore_excludes_db_files() {
        init_logging();
        info!("test_gitignore_excludes_db_files: starting");
        let temp_dir = TempDir::new().unwrap();
        let ctx = OutputContext::from_flags(false, false, true);
        execute(None, false, Some(temp_dir.path()), &ctx).unwrap();

        let gitignore_path = temp_dir.path().join(".beads/.gitignore");
        let content = fs::read_to_string(gitignore_path).unwrap();

        assert!(content.contains("*.db"));
        assert!(content.contains("*.db-journal"));
        assert!(
            content.lines().any(|line| line.trim() == "*.db-wal*"),
            "init must ignore classic WAL files and fsqlite WAL certificate sidecars: {content}"
        );
        assert!(content.contains("*.db-shm"));
        assert!(
            content.lines().any(|line| line.trim() == ".write.lock"),
            "init must emit the canonical write-lock rule expected by doctor: {content}"
        );
        assert!(content.contains("*.lock"));
        info!("test_gitignore_excludes_db_files: assertions passed");
    }
}
