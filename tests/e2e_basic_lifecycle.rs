mod common;

use beads_rust::model::{Issue, IssueType, Priority, Status};
use chrono::Utc;
use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;
use std::thread::sleep;
use std::time::Duration;

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let id_part = line
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

fn make_issue(id: &str, title: &str, now: chrono::DateTime<Utc>) -> Issue {
    Issue {
        id: id.to_string(),
        title: title.to_string(),
        status: Status::Open,
        priority: Priority::MEDIUM,
        issue_type: IssueType::Task,
        created_at: now,
        updated_at: now,
        content_hash: None,
        description: None,
        design: None,
        acceptance_criteria: None,
        notes: None,
        assignee: None,
        owner: None,
        estimated_minutes: None,
        created_by: None,
        closed_at: None,
        close_reason: None,
        closed_by_session: None,
        due_at: None,
        defer_until: None,
        external_ref: None,
        source_system: None,
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
fn e2e_basic_lifecycle() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);
    assert!(!id.is_empty(), "missing created id");

    let update_args = vec![
        "update".to_string(),
        id.clone(),
        "--status".to_string(),
        "in_progress".to_string(),
        "--priority".to_string(),
        "1".to_string(),
        "--assignee".to_string(),
        "alice".to_string(),
    ];
    let update = run_br(&workspace, update_args, "update");
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let list = run_br(&workspace, ["list", "--json"], "list");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let list_payload = extract_json_payload(&list.stdout);
    let list_json: Vec<Value> = serde_json::from_str(&list_payload).expect("list json");
    assert!(
        list_json
            .iter()
            .any(|item| item["id"] == id && item["status"] == "in_progress"),
        "updated issue not found in list"
    );

    let list_text = run_br(&workspace, ["list"], "list_text");
    assert!(
        list_text.status.success(),
        "list text failed: {}",
        list_text.stderr
    );
    assert!(
        list_text.stdout.contains("Test issue"),
        "list text missing issue title"
    );

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let show_payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&show_payload).expect("show json");
    assert_eq!(show_json[0]["id"], id);

    let show_text = run_br(&workspace, ["show", &id], "show_text");
    assert!(
        show_text.status.success(),
        "show text failed: {}",
        show_text.stderr
    );
    assert!(
        show_text.stdout.contains("Test issue"),
        "show text missing title"
    );

    let close_args = vec![
        "update".to_string(),
        id,
        "--status".to_string(),
        "closed".to_string(),
    ];
    let close = run_br(&workspace, close_args, "close");
    assert!(close.status.success(), "close failed: {}", close.stderr);
}

#[test]
fn e2e_quick_capture() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(&workspace, ["q", "Quick", "issue"], "quick");
    assert!(quick.status.success(), "quick failed: {}", quick.stderr);

    let quick_id = quick.stdout.lines().next().unwrap_or("").trim().to_string();
    assert!(!quick_id.is_empty(), "missing quick id");
    assert!(quick_id.contains('-'), "unexpected quick id format");
}

#[test]
fn e2e_sync_roundtrip() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Original title", "--no-auto-flush"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);
    assert!(!id.is_empty(), "missing created id");

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);
    assert!(
        sync.stdout.contains("Exported"),
        "sync flush text missing export message"
    );

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    assert!(jsonl_path.exists(), "issues.jsonl missing after flush");
    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    // Parse and update the issue properly (title + timestamp for last-write-wins)
    let mut updated_lines = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut issue: Value = serde_json::from_str(line).expect("parse issue");
        if issue["title"] == "Original title" {
            issue["title"] = Value::String("Modified title".to_string());
            // Bump updated_at to ensure import sees it as newer
            issue["updated_at"] = Value::String(Utc::now().to_rfc3339());
        }
        updated_lines.push(serde_json::to_string(&issue).expect("serialize issue"));
    }
    fs::write(&jsonl_path, updated_lines.join("\n") + "\n").expect("write jsonl");

    sleep(Duration::from_millis(50));

    let sync_import = run_br(&workspace, ["sync", "--import-only"], "sync_import");
    assert!(
        sync_import.status.success(),
        "sync import failed: {}",
        sync_import.stderr
    );

    let show = run_br(&workspace, ["show", &id, "--json"], "show_after_import");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&payload).expect("show json");
    assert_eq!(show_json[0]["title"], "Modified title");
}

#[test]
fn e2e_sync_import_staleness_and_force() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Stale issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush_stale");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    let import_first = run_br(&workspace, ["sync", "--import-only"], "sync_import_first");
    assert!(
        import_first.status.success(),
        "sync import first failed: {}",
        import_first.stderr
    );

    let import_skip = run_br(&workspace, ["sync", "--import-only"], "sync_import_skip");
    assert!(
        import_skip.status.success(),
        "sync import skip failed: {}",
        import_skip.stderr
    );
    assert!(
        import_skip
            .stdout
            .contains("JSONL is current (hash unchanged since last import)"),
        "sync import skip missing current message"
    );

    let import_force = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "sync_import_force",
    );
    assert!(
        import_force.status.success(),
        "sync import force failed: {}",
        import_force.stderr
    );
    assert!(
        import_force.stdout.contains("Imported from JSONL"),
        "sync import force missing header"
    );
    assert!(
        import_force.stdout.contains("Processed: 1 issues"),
        "sync import force missing processed count"
    );
}

#[test]
fn e2e_no_db_read_write() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Seed issue"], "create_seed");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    assert!(jsonl_path.exists(), "issues.jsonl missing");

    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let mut issues: Vec<Value> = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("parse jsonl"))
        .collect();
    assert!(!issues.is_empty(), "seed jsonl empty");

    let now = Utc::now().to_rfc3339();
    let mut injected = issues[0].clone();
    injected["id"] = Value::String("bd-nodb1".to_string());
    injected["title"] = Value::String("Injected no-db".to_string());
    injected["created_at"] = Value::String(now.clone());
    injected["updated_at"] = Value::String(now);
    issues.push(injected);

    let rewritten: Vec<String> = issues
        .into_iter()
        .map(|issue| serde_json::to_string(&issue).expect("serialize jsonl"))
        .collect();
    fs::write(&jsonl_path, rewritten.join("\n") + "\n").expect("write jsonl");

    let list = run_br(&workspace, ["--no-db", "list", "--json"], "list_no_db");
    assert!(
        list.status.success(),
        "list --no-db failed: {}",
        list.stderr
    );
    let list_payload = extract_json_payload(&list.stdout);
    let list_json: Vec<Value> = serde_json::from_str(&list_payload).expect("list json");
    assert!(
        list_json.iter().any(|item| item["id"] == "bd-nodb1"),
        "no-db list missing injected issue"
    );

    let create_no_db = run_br(
        &workspace,
        ["--no-db", "create", "No DB create"],
        "create_no_db",
    );
    assert!(
        create_no_db.status.success(),
        "create --no-db failed: {}",
        create_no_db.stderr
    );
    let created_id = parse_created_id(&create_no_db.stdout);
    assert!(!created_id.is_empty(), "no-db create missing id");

    let updated = fs::read_to_string(&jsonl_path).expect("read jsonl after no-db");
    assert!(
        updated.contains("No DB create"),
        "no-db create did not update JSONL"
    );
}

#[test]
fn e2e_no_db_mixed_prefix_error() {
    let workspace = BrWorkspace::new();
    let beads_dir = workspace.root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");
    let jsonl_path = beads_dir.join("issues.jsonl");

    let now = Utc::now();
    let issue_a = make_issue("aa-abc", "Alpha issue", now);
    let issue_b = make_issue("bb-def", "Beta issue", now);
    let lines = [
        serde_json::to_string(&issue_a).expect("serialize issue a"),
        serde_json::to_string(&issue_b).expect("serialize issue b"),
    ];
    fs::write(&jsonl_path, lines.join("\n") + "\n").expect("write jsonl");

    let list = run_br(
        &workspace,
        ["--no-db", "list", "--json"],
        "list_no_db_mixed",
    );
    assert!(
        !list.status.success(),
        "list --no-db should fail with mixed prefixes"
    );
    assert!(
        list.stderr.contains("Mixed issue prefixes"),
        "missing mixed prefix error: {}",
        list.stderr
    );
}

#[test]
fn e2e_sync_manifest() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Manifest issue", "--no-auto-flush"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(
        &workspace,
        ["sync", "--flush-only", "--manifest"],
        "sync_manifest",
    );
    assert!(
        sync.status.success(),
        "sync manifest failed: {}",
        sync.stderr
    );

    let manifest_path = workspace.root.join(".beads").join(".manifest.json");
    assert!(manifest_path.exists(), "manifest not created");
}

#[test]
fn e2e_sync_status_json() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Status issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let status = run_br(&workspace, ["sync", "--status", "--json"], "sync_status");
    assert!(
        status.status.success(),
        "sync status failed: {}",
        status.stderr
    );
    let payload = extract_json_payload(&status.stdout);
    let status_json: Value = serde_json::from_str(&payload).expect("sync status json");
    assert!(status_json["dirty_count"].is_number());
}

#[test]
fn e2e_version_text() {
    let workspace = BrWorkspace::new();

    let version = run_br(&workspace, ["version"], "version");
    assert!(
        version.status.success(),
        "version failed: {}",
        version.stderr
    );
    assert!(
        version.stdout.contains("br version"),
        "version output missing header"
    );
}

#[test]
fn e2e_doctor_json() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let doctor = run_br(&workspace, ["doctor", "--json"], "doctor_json");
    assert!(doctor.status.success(), "doctor failed: {}", doctor.stderr);
    let payload = extract_json_payload(&doctor.stdout);
    let doctor_json: Value = serde_json::from_str(&payload).expect("doctor json");
    assert!(doctor_json["checks"].is_array(), "doctor checks missing");
}

#[test]
fn e2e_sync_status_text() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let status = run_br(&workspace, ["sync", "--status"], "sync_status_text");
    assert!(
        status.status.success(),
        "sync status text failed: {}",
        status.stderr
    );
    assert!(
        status.stdout.contains("Sync Status"),
        "sync status text missing header"
    );
}

#[test]
fn e2e_version_json() {
    let workspace = BrWorkspace::new();

    let version = run_br(&workspace, ["version", "--json"], "version_json");
    assert!(
        version.status.success(),
        "version json failed: {}",
        version.stderr
    );
    let payload = extract_json_payload(&version.stdout);
    let version_json: Value = serde_json::from_str(&payload).expect("version json");
    assert!(version_json["version"].is_string());
}

#[test]
fn e2e_sync_conflict_markers_aborts_import() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create initial issue and export
    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    // Inject conflict markers into JSONL
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let original = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let conflicted = format!(
        "<<<<<<< HEAD\n{}\n=======\n{}\n>>>>>>> feature-branch\n",
        original.trim(),
        original.trim()
    );
    fs::write(&jsonl_path, conflicted).expect("write conflicted jsonl");

    // Import should fail due to conflict markers
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "sync_import_conflict",
    );
    assert!(
        !import.status.success(),
        "import should fail with conflict markers"
    );
    assert!(
        import.stderr.contains("Merge conflict markers detected")
            || import.stdout.contains("Merge conflict markers detected"),
        "error message should mention conflict markers: stdout={}, stderr={}",
        import.stdout,
        import.stderr
    );
}

#[test]
fn e2e_sync_tombstone_preservation() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create and then delete an issue (creates tombstone)
    let create = run_br(&workspace, ["create", "Issue to delete"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    let delete = run_br(
        &workspace,
        ["delete", &id, "--force", "--reason", "Testing tombstone"],
        "delete",
    );
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);

    // Verify issue is now a tombstone
    let show = run_br(&workspace, ["show", &id, "--json"], "show_tombstone");
    assert!(
        show.status.success(),
        "show tombstone failed: {}",
        show.stderr
    );
    let payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&payload).expect("show json");
    assert_eq!(
        show_json[0]["status"], "tombstone",
        "issue should be tombstone"
    );

    // Export to JSONL
    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    // Read the JSONL and verify tombstone is present
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    assert!(
        contents.contains("\"status\":\"tombstone\""),
        "JSONL should contain tombstone status"
    );

    // Create a new workspace to simulate importing into fresh database
    let workspace2 = BrWorkspace::new();
    let init2 = run_br(&workspace2, ["init"], "init2");
    assert!(init2.status.success(), "init2 failed: {}", init2.stderr);

    // Copy the JSONL to new workspace
    let jsonl_path2 = workspace2.root.join(".beads").join("issues.jsonl");
    fs::copy(&jsonl_path, &jsonl_path2).expect("copy jsonl");

    // Import
    let import = run_br(
        &workspace2,
        ["sync", "--import-only", "--force"],
        "sync_import",
    );
    assert!(import.status.success(), "import failed: {}", import.stderr);

    // Verify tombstone was imported
    let show2 = run_br(&workspace2, ["show", &id, "--json"], "show_after_import");
    assert!(
        show2.status.success(),
        "show after import failed: {}",
        show2.stderr
    );
    let payload2 = extract_json_payload(&show2.stdout);
    let show_json2: Vec<Value> = serde_json::from_str(&payload2).expect("show json after import");
    assert_eq!(
        show_json2[0]["status"], "tombstone",
        "tombstone should be preserved after import"
    );
}

#[test]
fn e2e_sync_tombstone_protection() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create and delete an issue
    let create = run_br(&workspace, ["create", "Protected issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    let delete = run_br(
        &workspace,
        ["delete", &id, "--force", "--reason", "Tombstone test"],
        "delete",
    );
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);

    // Export tombstone to JSONL
    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    // Modify JSONL to try to resurrect the tombstone (change status to open)
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let mut modified_lines = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut issue: Value = serde_json::from_str(line).expect("parse issue");
        if issue["status"] == "tombstone" {
            // Try to resurrect it
            issue["status"] = Value::String("open".to_string());
            issue["updated_at"] = Value::String(Utc::now().to_rfc3339());
        }
        modified_lines.push(serde_json::to_string(&issue).expect("serialize"));
    }
    fs::write(&jsonl_path, modified_lines.join("\n") + "\n").expect("write modified jsonl");

    sleep(Duration::from_millis(50));

    // Import - tombstone should be protected (resurrection blocked)
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "sync_import_resurrect",
    );
    assert!(import.status.success(), "import failed: {}", import.stderr);

    // Verify the issue is still a tombstone (not resurrected)
    let show = run_br(
        &workspace,
        ["show", &id, "--json"],
        "show_after_resurrect_attempt",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&payload).expect("show json");
    assert_eq!(
        show_json[0]["status"], "tombstone",
        "tombstone protection should prevent resurrection"
    );
}

#[test]
fn e2e_sync_content_hash_consistency() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create issues
    let create1 = run_br(
        &workspace,
        ["create", "Issue A", "--no-auto-flush"],
        "create1",
    );
    assert!(
        create1.status.success(),
        "create1 failed: {}",
        create1.stderr
    );
    let create2 = run_br(
        &workspace,
        ["create", "Issue B", "--no-auto-flush"],
        "create2",
    );
    assert!(
        create2.status.success(),
        "create2 failed: {}",
        create2.stderr
    );

    // Export and get hash
    let flush1 = run_br(
        &workspace,
        ["sync", "--flush-only", "--json"],
        "sync_flush1",
    );
    assert!(
        flush1.status.success(),
        "sync flush1 failed: {}",
        flush1.stderr
    );
    let payload1 = extract_json_payload(&flush1.stdout);
    let flush_json1: Value = serde_json::from_str(&payload1).expect("flush json1");
    let hash1 = flush_json1["content_hash"].as_str().expect("content_hash1");

    // Export again without changes (force to re-export)
    let flush2 = run_br(
        &workspace,
        ["sync", "--flush-only", "--force", "--json"],
        "sync_flush2",
    );
    assert!(
        flush2.status.success(),
        "sync flush2 failed: {}",
        flush2.stderr
    );
    let payload2 = extract_json_payload(&flush2.stdout);
    let flush_json2: Value = serde_json::from_str(&payload2).expect("flush json2");
    let hash2 = flush_json2["content_hash"].as_str().expect("content_hash2");

    // Content hash should be consistent for same content
    assert_eq!(
        hash1, hash2,
        "content hash should be consistent for same content"
    );

    // Verify status shows the hash
    let status = run_br(&workspace, ["sync", "--status", "--json"], "sync_status");
    assert!(
        status.status.success(),
        "sync status failed: {}",
        status.stderr
    );
    let status_payload = extract_json_payload(&status.stdout);
    let status_json: Value = serde_json::from_str(&status_payload).expect("status json");
    let stored_hash = status_json["jsonl_content_hash"]
        .as_str()
        .expect("stored hash");
    assert_eq!(stored_hash, hash2, "stored hash should match export hash");
}

#[test]
fn e2e_jsonl_discovery_prefers_issues() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue and export
    let create = run_br(&workspace, ["create", "Discovery test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    // Verify issues.jsonl was created (default)
    let issues_path = workspace.root.join(".beads").join("issues.jsonl");
    assert!(issues_path.exists(), "issues.jsonl should be created");

    // Create a legacy beads.jsonl with different content
    let beads_path = workspace.root.join(".beads").join("beads.jsonl");
    fs::write(&beads_path, "{\"id\": \"fake-id\", \"title\": \"Legacy issue\", \"status\": \"open\", \"issue_type\": \"task\", \"priority\": 2, \"labels\": [], \"created_at\": \"2026-01-01T00:00:00Z\", \"updated_at\": \"2026-01-01T00:00:00Z\", \"ephemeral\": false, \"pinned\": false, \"is_template\": false, \"dependencies\": [], \"comments\": []}\n").expect("write legacy");

    // When both exist, import should use issues.jsonl (the issue we created)
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "sync_import",
    );
    assert!(import.status.success(), "import failed: {}", import.stderr);

    // Verify our issue exists (from issues.jsonl), not the fake one
    let show = run_br(&workspace, ["show", &id, "--json"], "show_original");
    assert!(
        show.status.success(),
        "show original failed: {}",
        show.stderr
    );

    // Verify fake-id doesn't exist (wasn't imported from beads.jsonl)
    let show_fake = run_br(&workspace, ["show", "fake-id", "--json"], "show_fake");
    // Should fail or return empty since fake-id shouldn't exist
    let fake_payload = extract_json_payload(&show_fake.stdout);
    let fake_json: Vec<Value> = serde_json::from_str(&fake_payload).unwrap_or_default();
    assert!(
        fake_json.is_empty() || show_fake.stderr.contains("not found"),
        "fake issue from beads.jsonl should not be imported when issues.jsonl exists"
    );
}

#[test]
fn e2e_jsonl_discovery_uses_legacy_when_no_issues() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Remove issues.jsonl if it exists
    let issues_path = workspace.root.join(".beads").join("issues.jsonl");
    if issues_path.exists() {
        fs::remove_file(&issues_path).expect("remove issues.jsonl");
    }

    // Create a legacy beads.jsonl with an issue (using bd- prefix)
    let beads_path = workspace.root.join(".beads").join("beads.jsonl");
    fs::write(&beads_path, "{\"id\": \"bd-legacy1\", \"title\": \"Legacy issue\", \"status\": \"open\", \"issue_type\": \"task\", \"priority\": 2, \"labels\": [], \"created_at\": \"2026-01-01T00:00:00Z\", \"updated_at\": \"2026-01-01T00:00:00Z\", \"ephemeral\": false, \"pinned\": false, \"is_template\": false, \"dependencies\": [], \"comments\": []}\n").expect("write legacy");

    // Import should use beads.jsonl since issues.jsonl doesn't exist
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "sync_import_legacy",
    );
    assert!(
        import.status.success(),
        "import legacy failed: {}",
        import.stderr
    );

    // Verify the legacy issue was imported
    let show = run_br(&workspace, ["show", "bd-legacy1", "--json"], "show_legacy");
    assert!(show.status.success(), "show legacy failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&payload).expect("show json");
    assert_eq!(
        show_json[0]["title"], "Legacy issue",
        "legacy issue should be imported from beads.jsonl"
    );
}
