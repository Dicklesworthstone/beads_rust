//! E2E Report Generation Tests
//!
//! Tests for the artifact report indexer that generates HTML/Markdown reports
//! from test artifacts for faster triage.
//!
//! Usage:
//!   # Generate reports after running tests with artifacts
//!   `HARNESS_ARTIFACTS=1` cargo test `e2e_sync`
//!   `REPORT_ARTIFACTS_DIR=target/test-artifacts` \
//!   `REPORT_OUTPUT_DIR=target/reports` \
//!   cargo test --test `e2e_report_generation` -- --nocapture
//!
//! Task: beads_rust-x7on

mod common;

use common::report_indexer::{
    ArtifactIndexer, IndexerConfig, generate_html_report, generate_markdown_report, write_reports,
};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Create sample test artifacts for testing report generation
fn create_sample_artifacts(base_dir: &std::path::Path) -> std::io::Result<()> {
    // Create a passing test suite
    let pass_dir = base_dir.join("e2e_basic").join("test_create_issue");
    fs::create_dir_all(&pass_dir)?;

    fs::write(
        pass_dir.join("summary.json"),
        r#"{"suite":"e2e_basic","test":"test_create_issue","passed":true,"run_count":3,"timestamp":"2026-01-17T12:00:00Z"}"#,
    )?;

    fs::write(
        pass_dir.join("events.jsonl"),
        r#"{"timestamp":"2026-01-17T12:00:00Z","event_type":"command","label":"init","binary":"br","args":["init"],"cwd":"/tmp/test1","exit_code":0,"success":true,"duration_ms":50,"stdout_len":100,"stderr_len":0}
{"timestamp":"2026-01-17T12:00:01Z","event_type":"command","label":"create","binary":"br","args":["create","--title","Test Issue"],"cwd":"/tmp/test1","exit_code":0,"success":true,"duration_ms":120,"stdout_len":200,"stderr_len":0}"#,
    )?;

    // Create another passing test
    let pass_dir2 = base_dir.join("e2e_basic").join("test_list_issues");
    fs::create_dir_all(&pass_dir2)?;

    fs::write(
        pass_dir2.join("summary.json"),
        r#"{"suite":"e2e_basic","test":"test_list_issues","passed":true,"run_count":1,"timestamp":"2026-01-17T12:01:00Z"}"#,
    )?;

    fs::write(
        pass_dir2.join("events.jsonl"),
        r#"{"timestamp":"2026-01-17T12:01:00Z","event_type":"command","label":"list","binary":"br","args":["list","--json"],"cwd":"/tmp/test2","exit_code":0,"success":true,"duration_ms":80,"stdout_len":500,"stderr_len":0}"#,
    )?;

    // Create a failing test
    let fail_dir = base_dir.join("e2e_sync").join("test_sync_conflict");
    fs::create_dir_all(&fail_dir)?;

    fs::write(
        fail_dir.join("summary.json"),
        r#"{"suite":"e2e_sync","test":"test_sync_conflict","passed":false,"run_count":1,"timestamp":"2026-01-17T12:02:00Z"}"#,
    )?;

    fs::write(
        fail_dir.join("events.jsonl"),
        r#"{"timestamp":"2026-01-17T12:02:00Z","event_type":"command","label":"sync","binary":"br","args":["sync","--import-only"],"cwd":"/tmp/test3","exit_code":1,"success":false,"duration_ms":250,"stdout_len":50,"stderr_len":100,"stderr_path":"0001_sync.stderr"}"#,
    )?;

    fs::write(
        fail_dir.join("0001_sync.stderr"),
        "Error: Conflict detected in beads.jsonl\nConflict markers found at lines 42-48\nPlease resolve conflicts and retry",
    )?;

    Ok(())
}

#[test]
fn test_report_indexer_basic() {
    let temp_dir = TempDir::new().unwrap();
    create_sample_artifacts(temp_dir.path()).unwrap();

    let indexer = ArtifactIndexer::new(temp_dir.path());
    let report = indexer.generate_report().unwrap();

    assert_eq!(report.total_tests, 3, "Should have 3 tests total");
    assert_eq!(report.total_passed, 2, "Should have 2 passed");
    assert_eq!(report.total_failed, 1, "Should have 1 failed");
    assert_eq!(report.suites.len(), 2, "Should have 2 suites");

    // Check suite breakdown
    let basic = report.tests_by_suite("e2e_basic").unwrap();
    assert_eq!(basic.tests.len(), 2);
    assert_eq!(basic.passed_count, 2);

    let sync = report.tests_by_suite("e2e_sync").unwrap();
    assert_eq!(sync.tests.len(), 1);
    assert_eq!(sync.failed_count, 1);

    // Check failed test has failure reason
    let failed = report.failed_tests();
    assert_eq!(failed.len(), 1);
    assert!(failed[0].failure_reason.is_some());
    assert!(
        failed[0]
            .failure_reason
            .as_ref()
            .unwrap()
            .contains("Conflict")
    );
}

#[test]
fn test_markdown_report() {
    let temp_dir = TempDir::new().unwrap();
    create_sample_artifacts(temp_dir.path()).unwrap();

    let indexer = ArtifactIndexer::new(temp_dir.path());
    let report = indexer.generate_report().unwrap();
    let md = generate_markdown_report(&report);

    // Check structure
    assert!(md.contains("# Test Artifact Report"), "Missing header");
    assert!(md.contains("## Summary"), "Missing summary");
    assert!(md.contains("## Suites"), "Missing suites");
    assert!(
        md.contains("## Failed Tests Detail"),
        "Missing failed tests"
    );
    assert!(md.contains("## Slowest Tests"), "Missing slowest tests");

    // Check content
    assert!(md.contains("e2e_basic"), "Missing e2e_basic suite");
    assert!(md.contains("e2e_sync"), "Missing e2e_sync suite");
    assert!(
        md.contains("test_sync_conflict"),
        "Missing failed test name"
    );
    assert!(md.contains("Conflict detected"), "Missing failure reason");

    // Check pass/fail indicators
    assert!(md.contains("✅"), "Missing pass indicator");
    assert!(md.contains("❌"), "Missing fail indicator");
}

#[test]
fn test_html_report() {
    let temp_dir = TempDir::new().unwrap();
    create_sample_artifacts(temp_dir.path()).unwrap();

    let indexer = ArtifactIndexer::new(temp_dir.path());
    let report = indexer.generate_report().unwrap();
    let html = generate_html_report(&report);

    // Check HTML structure
    assert!(html.contains("<!DOCTYPE html>"), "Missing doctype");
    assert!(html.contains("<html>"), "Missing html tag");
    assert!(html.contains("Test Artifact Report"), "Missing title");
    assert!(html.contains("<style>"), "Missing styles");

    // Check content
    assert!(html.contains("e2e_basic"), "Missing e2e_basic");
    assert!(html.contains("e2e_sync"), "Missing e2e_sync");
    assert!(html.contains("test_sync_conflict"), "Missing failed test");

    // Check CSS classes
    assert!(html.contains("status-pass"), "Missing pass class");
    assert!(html.contains("status-fail"), "Missing fail class");
    assert!(
        html.contains("failure-detail"),
        "Missing failure detail class"
    );
}

#[test]
fn test_write_reports() {
    let temp_dir = TempDir::new().unwrap();
    let artifacts_dir = temp_dir.path().join("artifacts");
    let output_dir = temp_dir.path().join("output");

    fs::create_dir_all(&artifacts_dir).unwrap();
    create_sample_artifacts(&artifacts_dir).unwrap();

    let indexer = ArtifactIndexer::new(&artifacts_dir);
    let report = indexer.generate_report().unwrap();
    let (md_path, html_path) = write_reports(&report, &output_dir).unwrap();

    // Verify files exist
    assert!(md_path.exists(), "Markdown report not created");
    assert!(html_path.exists(), "HTML report not created");

    // Verify content
    let md_content = fs::read_to_string(&md_path).unwrap();
    assert!(md_content.contains("# Test Artifact Report"));

    let html_content = fs::read_to_string(&html_path).unwrap();
    assert!(html_content.contains("<!DOCTYPE html>"));
}

#[test]
fn test_failures_only_filter() {
    let temp_dir = TempDir::new().unwrap();
    create_sample_artifacts(temp_dir.path()).unwrap();

    let config = IndexerConfig {
        artifact_root: temp_dir.path().to_path_buf(),
        failures_only: true,
        ..Default::default()
    };

    let indexer = ArtifactIndexer::with_config(config);
    let report = indexer.generate_report().unwrap();

    // Should only include the failed test
    assert_eq!(report.total_tests, 1);
    assert_eq!(report.total_failed, 1);
    assert!(report.tests_by_suite("e2e_basic").is_none());
}

/// Generate and save report from actual test artifacts
///
/// This test is meant to be run manually after running tests with `HARNESS_ARTIFACTS=1`.
/// It generates reports to the specified output directory.
///
/// Usage:
///   `REPORT_ARTIFACTS_DIR=target/test-artifacts` \
///   `REPORT_OUTPUT_DIR=target/reports` \
///   cargo test --test `e2e_report_generation` -- --nocapture `generate_and_save_report`
#[test]
#[ignore = "Run manually after running tests with HARNESS_ARTIFACTS=1"]
fn generate_and_save_report() {
    let artifacts_dir = std::env::var("REPORT_ARTIFACTS_DIR")
        .map_or_else(|_| PathBuf::from("target/test-artifacts"), PathBuf::from);

    let output_dir = std::env::var("REPORT_OUTPUT_DIR")
        .map_or_else(|_| PathBuf::from("target/reports"), PathBuf::from);

    let failures_only = std::env::var("REPORT_FAILURES_ONLY").is_ok_and(|v| v == "1");

    println!("=== Artifact Report Generator ===");
    println!("Artifacts: {}", artifacts_dir.display());
    println!("Output: {}", output_dir.display());
    println!("Failures only: {failures_only}");
    println!();

    if !artifacts_dir.exists() {
        eprintln!(
            "Error: Artifacts directory not found: {}",
            artifacts_dir.display()
        );
        eprintln!();
        eprintln!("Run tests with HARNESS_ARTIFACTS=1 first:");
        eprintln!("  HARNESS_ARTIFACTS=1 cargo test e2e_sync");
        panic!("Artifacts directory not found");
    }

    let config = IndexerConfig {
        artifact_root: artifacts_dir,
        failures_only,
        ..Default::default()
    };

    let indexer = ArtifactIndexer::with_config(config);
    let report = indexer
        .generate_report()
        .expect("Failed to generate report");

    println!("Report Summary:");
    println!("  Total tests: {}", report.total_tests);
    println!(
        "  Passed: {} ({:.1}%)",
        report.total_passed,
        report.pass_rate()
    );
    println!("  Failed: {}", report.total_failed);
    let duration_secs = report.total_duration_ms / 1000;
    let duration_ms = report.total_duration_ms % 1000;
    println!("  Duration: {duration_secs}.{duration_ms:03}s");
    println!("  Suites: {}", report.suites.len());
    println!();

    let (md_path, html_path) =
        write_reports(&report, &output_dir).expect("Failed to write reports");

    println!("Reports generated:");
    println!("  Markdown: {}", md_path.display());
    println!("  HTML: {}", html_path.display());
    println!();
    println!("Open in browser:");
    println!("  open {}", html_path.display());
}
