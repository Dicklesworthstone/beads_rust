use crate::error::{BeadsError, Result};
use crate::storage::SqliteStorage;
use std::fs;
use std::path::Path;

/// Execute the init command.
///
/// # Errors
///
/// Returns an error if the directory or database cannot be created.
pub fn execute(prefix: Option<String>, force: bool, root_dir: Option<&Path>) -> Result<()> {
    let base_dir = root_dir.unwrap_or_else(|| Path::new("."));
    let beads_dir = base_dir.join(".beads");

    if beads_dir.exists() {
        // Check if DB exists
        let db_path = beads_dir.join("beads.db");
        if db_path.exists() && !force {
            return Err(BeadsError::AlreadyInitialized { path: db_path });
        }
    } else {
        fs::create_dir(&beads_dir)?;
    }

    let db_path = beads_dir.join("beads.db");

    // Initialize DB (creates file and applies schema)
    let mut storage = SqliteStorage::open(&db_path)?;

    // Set prefix in config table if provided
    if let Some(p) = prefix {
        storage.set_config("issue_prefix", &p)?;
        println!("Prefix set to: {p}");
    }

    // Write metadata.json
    let metadata_path = beads_dir.join("metadata.json");
    if !metadata_path.exists() || force {
        let metadata = r#"{
  "database": "beads.db",
  "jsonl_export": "issues.jsonl"
}"#;
        fs::write(metadata_path, metadata)?;
    }

    // Write config.yaml template
    let config_path = beads_dir.join("config.yaml");
    if !config_path.exists() {
        let config = r"# Beads Project Configuration
# issue_prefix: bd
# default_priority: 2
# default_type: task
";
        fs::write(config_path, config)?;
    }

    // Write .gitignore
    let gitignore_path = beads_dir.join(".gitignore");
    if !gitignore_path.exists() {
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

    println!("Initialized beads workspace in .beads/");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_init_creates_beads_directory() {
        let temp_dir = TempDir::new().unwrap();
        let result = execute(None, false, Some(temp_dir.path()));

        assert!(result.is_ok());
        assert!(temp_dir.path().join(".beads").exists());
        assert!(temp_dir.path().join(".beads/beads.db").exists());
        assert!(temp_dir.path().join(".beads/metadata.json").exists());
        assert!(temp_dir.path().join(".beads/config.yaml").exists());
        assert!(temp_dir.path().join(".beads/.gitignore").exists());
    }

    #[test]
    fn test_init_with_prefix() {
        let temp_dir = TempDir::new().unwrap();
        let result = execute(Some("test".to_string()), false, Some(temp_dir.path()));

        assert!(result.is_ok());

        // Verify prefix was stored
        let db_path = temp_dir.path().join(".beads/beads.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let prefix = storage.get_config("issue_prefix").unwrap();
        assert_eq!(prefix, Some("test".to_string()));
    }

    #[test]
    fn test_init_fails_if_already_initialized() {
        let temp_dir = TempDir::new().unwrap();

        // First init should succeed
        let result1 = execute(None, false, Some(temp_dir.path()));
        assert!(result1.is_ok());

        // Second init without force should fail
        let result2 = execute(None, false, Some(temp_dir.path()));

        assert!(result2.is_err());
        assert!(matches!(
            result2.unwrap_err(),
            BeadsError::AlreadyInitialized { .. }
        ));
    }

    #[test]
    fn test_init_force_overwrites_existing() {
        let temp_dir = TempDir::new().unwrap();

        // First init
        execute(Some("first".to_string()), false, Some(temp_dir.path())).unwrap();

        // Second init with force
        let result = execute(Some("second".to_string()), true, Some(temp_dir.path()));

        assert!(result.is_ok());

        // Verify new prefix
        let db_path = temp_dir.path().join(".beads/beads.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let prefix = storage.get_config("issue_prefix").unwrap();
        assert_eq!(prefix, Some("second".to_string()));
    }

    #[test]
    fn test_metadata_json_content() {
        let temp_dir = TempDir::new().unwrap();
        execute(None, false, Some(temp_dir.path())).unwrap();

        let metadata_path = temp_dir.path().join(".beads/metadata.json");
        let content = fs::read_to_string(metadata_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert_eq!(parsed["database"], "beads.db");
        assert_eq!(parsed["jsonl_export"], "issues.jsonl");
    }

    #[test]
    fn test_gitignore_excludes_db_files() {
        let temp_dir = TempDir::new().unwrap();
        execute(None, false, Some(temp_dir.path())).unwrap();

        let gitignore_path = temp_dir.path().join(".beads/.gitignore");
        let content = fs::read_to_string(gitignore_path).unwrap();

        assert!(content.contains("*.db"));
        assert!(content.contains("*.db-wal"));
        assert!(content.contains("*.db-shm"));
        assert!(content.contains("*.lock"));
    }
}
