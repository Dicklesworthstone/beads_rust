use crate::error::{BeadsError, Result};
use crate::output::{OutputContext, OutputMode};
use crate::storage::SqliteStorage;
use rich_rust::prelude::*;
use std::fs;
use std::path::Path;

/// Execute the init command.
///
/// # Errors
///
/// Returns an error if the directory or database cannot be created.
pub fn execute(
    prefix: Option<String>,
    force: bool,
    root_dir: Option<&Path>,
    ctx: &OutputContext,
) -> Result<()> {
    let base_dir = root_dir.unwrap_or_else(|| Path::new("."));
    let beads_dir = base_dir.join(".beads");

    let mut created_dir = false;
    if beads_dir.exists() {
        // Check if DB exists
        let db_path = beads_dir.join("beads.db");
        if db_path.exists() && !force {
            return Err(BeadsError::AlreadyInitialized { path: db_path });
        }
    } else {
        fs::create_dir(&beads_dir)?;
        created_dir = true;
    }

    let db_path = beads_dir.join("beads.db");
    let db_existed = db_path.exists();

    // Initialize DB (creates file and applies schema)
    let mut storage = SqliteStorage::open(&db_path)?;

    // Set prefix in config table if provided
    let mut prefix_set = None;
    if let Some(p) = prefix {
        storage.set_config("issue_prefix", &p)?;
        prefix_set = Some(p);
    }

    // Write metadata.json
    let metadata_path = beads_dir.join("metadata.json");
    let metadata_existed = metadata_path.exists();
    if !metadata_existed || force {
        let metadata = r#"{
  "database": "beads.db",
  "jsonl_export": "issues.jsonl"
}"#;
        fs::write(metadata_path, metadata)?;
    }

    // Write config.yaml template
    let config_path = beads_dir.join("config.yaml");
    let config_existed = config_path.exists();
    if !config_existed {
        let config = r"# Beads Project Configuration
# issue_prefix: bd
# default_priority: 2
# default_type: task
";
        fs::write(config_path, config)?;
    }

    // Write .gitignore
    let gitignore_path = beads_dir.join(".gitignore");
    let gitignore_existed = gitignore_path.exists();
    if !gitignore_existed {
        let gitignore = r"# Database
*.db
*.db-shm
*.db-wal

# Lock files
*.lock

# Temporary
last-touched
*.tmp
";
        fs::write(gitignore_path, gitignore)?;
    }

    // Write empty issues.jsonl for compatibility with bv (beads_viewer)
    // bv expects this file to exist even if there are no issues yet
    let jsonl_path = beads_dir.join("issues.jsonl");
    let jsonl_existed = jsonl_path.exists();
    if !jsonl_existed {
        fs::write(&jsonl_path, "")?;
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
        println!("Initialized beads workspace in .beads/");
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

#[allow(clippy::fn_params_excessive_bools)]
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
        assert!(content.contains("*.db-wal"));
        assert!(content.contains("*.db-shm"));
        assert!(content.contains("*.lock"));
        info!("test_gitignore_excludes_db_files: assertions passed");
    }
}
