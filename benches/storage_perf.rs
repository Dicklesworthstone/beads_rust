// Storage and sync performance benchmarks.
//
// Run with: cargo bench --bench storage_perf
//
// Performance Targets:
// | Operation           | Target    | Description                      |
// |---------------------|-----------|----------------------------------|
// | Create              | < 1ms     | Single issue creation            |
// | List (1k)           | < 10ms    | List 1000 issues                 |
// | List (10k)          | < 100ms   | List 10000 issues                |
// | Ready (1k/2k)       | < 5ms     | Ready query: 1k issues, 2k deps  |
// | Ready (10k/20k)     | < 50ms    | Ready query: 10k issues, 20k deps|
// | Export (10k)        | < 500ms   | Export 10k issues to JSONL       |
// | Import (10k)        | < 1s      | Import 10k issues from JSONL     |

#![allow(
    clippy::significant_drop_tightening,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

use beads_rust::model::{Dependency, DependencyType, Issue, IssueType, Priority, Status};
use beads_rust::storage::{IssueUpdate, ListFilters, ReadyFilters, ReadySortPolicy, SqliteStorage};
use chrono::Utc;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::io::Cursor;
use std::sync::Once;
use std::time::Instant;
use tempfile::TempDir;
use tracing::info;

/// Create a test issue with the given index.
fn create_test_issue(i: usize) -> Issue {
    Issue {
        id: format!("bench-{i:06}"),
        content_hash: None,
        title: format!("Benchmark issue {i}"),
        description: Some(format!("Description for benchmark issue {i}")),
        design: None,
        acceptance_criteria: None,
        notes: None,
        status: Status::Open,
        priority: Priority(i32::try_from(i % 5).expect("priority fits i32")),
        issue_type: match i % 4 {
            0 => IssueType::Bug,
            1 => IssueType::Feature,
            2 => IssueType::Task,
            _ => IssueType::Chore,
        },
        assignee: if i.is_multiple_of(3) {
            Some(format!("user{}", i % 10))
        } else {
            None
        },
        owner: Some("benchmark@test.com".to_string()),
        estimated_minutes: Some(i32::try_from(i % 60 + 30).expect("estimate fits i32")),
        created_at: Utc::now(),
        created_by: Some("benchmark".to_string()),
        updated_at: Utc::now(),
        closed_at: None,
        close_reason: None,
        closed_by_session: None,
        due_at: None,
        defer_until: None,
        external_ref: None,
        source_system: None,
        source_repo: None,
        source_repo_path: None,
        agent_context: None,
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
        labels: vec![format!("label-{}", i % 5)],
        dependencies: vec![],
        comments: vec![],
    }
}

fn init_bench_logging() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = beads_rust::logging::init_logging(0, false, None);
    });
}

fn log_group_start(name: &str) {
    info!("benchmark_group_start: name={name}");
}

fn log_group_end(name: &str) {
    info!("benchmark_group_end: name={name}");
}

fn log_bench_start(name: &str) -> Instant {
    info!("benchmark_start: {name}");
    Instant::now()
}

fn log_bench_end(name: &str, started_at: Instant) {
    info!("benchmark_end: {name} duration={:?}", started_at.elapsed());
}

/// Set up a database with a given number of issues.
fn setup_db_with_issues(count: usize) -> (TempDir, SqliteStorage) {
    let dir = TempDir::new().expect("Failed to create temp dir");
    let db_path = dir.path().join("bench.db");
    let mut storage = SqliteStorage::open(&db_path).expect("Failed to open db");

    for i in 0..count {
        let issue = create_test_issue(i);
        storage
            .create_issue(&issue, "benchmark")
            .expect("Failed to create issue");
    }

    (dir, storage)
}

fn setup_db_with_multi_label_issues(count: usize) -> (TempDir, SqliteStorage) {
    let dir = TempDir::new().expect("Failed to create temp dir");
    let db_path = dir.path().join("bench.db");
    let mut storage = SqliteStorage::open(&db_path).expect("Failed to open db");

    for i in 0..count {
        let mut issue = create_test_issue(i);
        if i.is_multiple_of(2) {
            issue.labels.push("label-even".to_string());
        }
        storage
            .create_issue(&issue, "benchmark")
            .expect("Failed to create issue");
    }

    (dir, storage)
}

/// Set up a database with issues and dependencies.
fn setup_db_with_deps(issue_count: usize, dep_count: usize) -> (TempDir, SqliteStorage) {
    let dir = TempDir::new().expect("Failed to create temp dir");
    let db_path = dir.path().join("bench.db");
    let mut storage = SqliteStorage::open(&db_path).expect("Failed to open db");

    // Create issues
    for i in 0..issue_count {
        let issue = create_test_issue(i);
        storage
            .create_issue(&issue, "benchmark")
            .expect("Failed to create issue");
    }

    // Create unique acyclic dependencies. Higher-index issues depend on
    // lower-index issues, so this can scale to deep DAGs without cycles.
    // Load them through the import helper so setup does not dominate
    // `--quick` runs with per-edge cycle detection.
    let mut added = 0usize;
    'deps: for from_idx in 1..issue_count {
        let from_id = format!("bench-{from_idx:06}");
        let mut dependencies = Vec::new();
        for to_idx in 0..from_idx {
            let to_id = format!("bench-{to_idx:06}");
            dependencies.push(Dependency {
                issue_id: from_id.clone(),
                depends_on_id: to_id,
                dep_type: DependencyType::Blocks,
                created_at: Utc::now(),
                created_by: Some("benchmark".to_string()),
                metadata: None,
                thread_id: None,
            });
            added += 1;
            if added == dep_count {
                break;
            }
        }
        if !dependencies.is_empty() {
            storage
                .sync_dependencies_for_import(&from_id, &dependencies)
                .expect("benchmark dependency import should succeed");
        }
        if added == dep_count {
            break 'deps;
        }
    }

    storage
        .rebuild_blocked_cache(true)
        .expect("benchmark blocked cache rebuild should succeed");

    (dir, storage)
}

fn configure_group<M: criterion::measurement::Measurement>(
    group: &mut criterion::BenchmarkGroup<'_, M>,
) {
    if let Ok(val) = std::env::var("BENCH_NOISE_THRESHOLD")
        && let Ok(f) = val.parse()
    {
        group.noise_threshold(f);
    }
    if let Ok(val) = std::env::var("BENCH_SIGNIFICANCE_LEVEL")
        && let Ok(f) = val.parse()
    {
        group.significance_level(f);
    }
    if let Ok(val) = std::env::var("BENCH_SAMPLE_SIZE")
        && let Ok(n) = val.parse()
    {
        group.sample_size(n);
    }
}

fn should_skip_group(group_name: &str) -> bool {
    std::env::var("BENCH_ONLY_GROUP").is_ok_and(|only| group_name != only)
}

fn configured_sizes(defaults: &[usize], env_var: &str) -> Vec<usize> {
    let Ok(raw) = std::env::var(env_var) else {
        return defaults.to_vec();
    };
    let sizes: Vec<usize> = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter_map(|value| value.parse().ok())
        .collect();
    if sizes.is_empty() {
        defaults.to_vec()
    } else {
        sizes
    }
}

// =============================================================================
// Storage Operation Benchmarks
// =============================================================================

/// Benchmark single issue creation.
fn bench_create_single(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "storage/create";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    group.bench_function("single", |b| {
        let bench_name = "storage/create/single";
        let bench_start = log_bench_start(bench_name);
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("bench.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let mut counter = 0usize;

        b.iter(|| {
            let issue = create_test_issue(counter);
            storage
                .create_issue(black_box(&issue), "benchmark")
                .unwrap();
            counter += 1;
        });
        log_bench_end(bench_name, bench_start);
    });

    group.finish();
    log_group_end(group_name);
}

/// Benchmark batch issue creation.
fn bench_create_batch(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "storage/create_batch";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    for size in [10, 100, 500] {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            let bench_name = format!("storage/create_batch/size={size}");
            let bench_start = log_bench_start(&bench_name);
            b.iter_with_setup(
                || {
                    let dir = TempDir::new().unwrap();
                    let db_path = dir.path().join("bench.db");
                    let storage = SqliteStorage::open(&db_path).unwrap();
                    (dir, storage)
                },
                |(dir, mut storage)| {
                    for i in 0..size {
                        let issue = create_test_issue(i);
                        storage.create_issue(&issue, "benchmark").unwrap();
                    }
                    // Keep dir alive
                    drop(dir);
                },
            );
            log_bench_end(&bench_name, bench_start);
        });
    }

    group.finish();
    log_group_end(group_name);
}

/// Benchmark updating an issue.
fn bench_update_issue(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "storage/update";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    // Pre-populate database with issues
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("bench.db");
    let mut storage = SqliteStorage::open(&db_path).unwrap();

    for i in 0..100 {
        let issue = create_test_issue(i);
        storage.create_issue(&issue, "benchmark").unwrap();
    }

    let mut counter = 0usize;
    group.bench_function("single", |b| {
        let bench_name = "storage/update/single";
        let bench_start = log_bench_start(bench_name);
        b.iter(|| {
            let id = format!("bench-{:06}", counter % 100);
            let update = IssueUpdate {
                title: Some(format!("Updated title {counter}")),
                priority: Some(Priority(((counter % 4) + 1) as i32)),
                ..IssueUpdate::default()
            };
            let _ = storage.update_issue(black_box(&id), black_box(&update), "benchmark");
            counter += 1;
        });
        log_bench_end(bench_name, bench_start);
    });

    group.finish();
    log_group_end(group_name);
    drop(dir);
}

/// Benchmark closing an issue with a reason.
fn bench_close_issue_with_reason(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "storage/close";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("bench.db");
    let mut storage = SqliteStorage::open(&db_path).unwrap();

    for i in 0..100 {
        let issue = create_test_issue(i);
        storage.create_issue(&issue, "benchmark").unwrap();
    }

    let mut counter = 0usize;
    group.bench_function("with_reason", |b| {
        let bench_name = "storage/close/with_reason";
        let bench_start = log_bench_start(bench_name);
        b.iter(|| {
            let id = format!("bench-{:06}", counter % 100);
            let update = IssueUpdate {
                status: Some(Status::Closed),
                closed_at: Some(Some(Utc::now())),
                close_reason: Some(Some("benchmark close".to_string())),
                closed_by_session: Some(Some("bench-session".to_string())),
                ..IssueUpdate::default()
            };
            let _ = storage.update_issue(black_box(&id), black_box(&update), "benchmark");
            counter += 1;
        });
        log_bench_end(bench_name, bench_start);
    });

    group.finish();
    log_group_end(group_name);
    drop(dir);
}

/// Benchmark deleting an issue (soft delete / tombstone).
fn bench_delete_issue(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "storage/delete";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    group.bench_function("single", |b| {
        let bench_name = "storage/delete/single";
        let bench_start = log_bench_start(bench_name);
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("bench.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();

        // Create a large pool of issues to delete
        for i in 0..10000 {
            let issue = create_test_issue(i);
            storage.create_issue(&issue, "benchmark").unwrap();
        }

        let mut counter = 0usize;
        b.iter(|| {
            let id = format!("bench-{:06}", counter % 10000);
            let _ = storage.delete_issue(black_box(&id), "benchmark", "benchmark deletion", None);
            counter += 1;
        });

        drop(dir);
        log_bench_end(bench_name, bench_start);
    });

    group.finish();
    log_group_end(group_name);
}

// =============================================================================
// Query Operation Benchmarks
// =============================================================================

/// Benchmark listing issues.
fn bench_list_issues(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "storage/list";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    for size in [100, 500, 1000, 2000, 5000] {
        let (_dir, storage) = setup_db_with_issues(size);

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &storage, |b, storage| {
            let bench_name = format!("storage/list/size={size}");
            let bench_start = log_bench_start(&bench_name);
            b.iter(|| {
                let filters = ListFilters::default();
                let issues = storage.list_issues(&filters).unwrap();
                black_box(issues)
            });
            log_bench_end(&bench_name, bench_start);
        });
    }

    group.finish();
    log_group_end(group_name);
}

/// Benchmark listing issues with filters applied.
fn bench_list_issues_filtered(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "storage/list_filtered";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    let (_dir, storage) = setup_db_with_issues(1000);
    let filters = ListFilters {
        statuses: Some(vec![Status::Open]),
        priorities: Some(vec![Priority::HIGH, Priority::MEDIUM]),
        types: Some(vec![IssueType::Task, IssueType::Bug]),
        labels: Some(vec!["label-1".to_string()]),
        ..ListFilters::default()
    };

    group.bench_function("filtered", |b| {
        let bench_name = "storage/list_filtered/filtered";
        let bench_start = log_bench_start(bench_name);
        b.iter(|| {
            let issues = storage.list_issues(black_box(&filters)).unwrap();
            black_box(issues)
        });
        log_bench_end(bench_name, bench_start);
    });

    let (_multi_label_dir, multi_label_storage) = setup_db_with_multi_label_issues(1000);
    let multi_label_filters = ListFilters {
        labels: Some(vec!["label-2".to_string(), "label-even".to_string()]),
        ..ListFilters::default()
    };

    group.bench_function("filtered_multi_label_and", |b| {
        let bench_name = "storage/list_filtered/filtered_multi_label_and";
        let bench_start = log_bench_start(bench_name);
        b.iter(|| {
            let issues = multi_label_storage
                .list_issues(black_box(&multi_label_filters))
                .unwrap();
            black_box(issues)
        });
        log_bench_end(bench_name, bench_start);
    });

    group.finish();
    log_group_end(group_name);
}

/// Benchmark ready query with dependencies.
fn bench_ready_query(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "storage/ready";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    for (issues, deps) in [(100, 200), (500, 1000), (1000, 2000), (10000, 20000)] {
        let (_dir, storage) = setup_db_with_deps(issues, deps);
        let label = format!("{issues}i_{deps}d");

        group.bench_with_input(
            BenchmarkId::new("issues_deps", &label),
            &storage,
            |b, storage| {
                let bench_name = format!("storage/ready/{label}");
                let bench_start = log_bench_start(&bench_name);
                b.iter(|| {
                    let filters = ReadyFilters::default();
                    let ready = storage
                        .get_ready_issues(&filters, ReadySortPolicy::default())
                        .unwrap();
                    black_box(ready)
                });
                log_bench_end(&bench_name, bench_start);
            },
        );
    }

    group.finish();
    log_group_end(group_name);
}

/// Benchmark blocked issues query.
fn bench_blocked_query(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "storage/blocked";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    for (issues, deps) in [(100, 200), (500, 1000)] {
        let (_dir, storage) = setup_db_with_deps(issues, deps);
        let label = format!("{issues}i_{deps}d");

        group.bench_with_input(
            BenchmarkId::new("issues_deps", &label),
            &storage,
            |b, storage| {
                let bench_name = format!("storage/blocked/{label}");
                let bench_start = log_bench_start(&bench_name);
                b.iter(|| {
                    let blocked = storage.get_blocked_issues().unwrap();
                    black_box(blocked)
                });
                log_bench_end(&bench_name, bench_start);
            },
        );
    }

    group.finish();
    log_group_end(group_name);
}

// =============================================================================
// Sync Operation Benchmarks
// =============================================================================

/// Benchmark JSONL export.
fn bench_export(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "sync/export";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    for size in configured_sizes(&[100, 500, 1000, 2000, 5000, 10000], "BENCH_SYNC_SIZES") {
        let (_dir, storage) = setup_db_with_issues(size);

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &storage, |b, storage| {
            let bench_name = format!("sync/export/size={size}");
            let bench_start = log_bench_start(&bench_name);
            b.iter(|| {
                let mut buffer = Cursor::new(Vec::new());
                beads_rust::sync::export_to_writer(storage, &mut buffer).unwrap();
                black_box(buffer.into_inner())
            });
            log_bench_end(&bench_name, bench_start);
        });
    }

    group.finish();
    log_group_end(group_name);
}

/// Benchmark JSONL import.
fn bench_import(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "sync/import";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    for size in configured_sizes(&[100, 500, 1000, 2000, 5000, 10000], "BENCH_SYNC_SIZES") {
        // Create source data
        let (_src_dir, src_storage) = setup_db_with_issues(size);
        let mut buffer = Cursor::new(Vec::new());
        beads_rust::sync::export_to_writer(&src_storage, &mut buffer).unwrap();
        let jsonl_data = buffer.into_inner();

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &jsonl_data, |b, data| {
            let bench_name = format!("sync/import/size={size}");
            let bench_start = log_bench_start(&bench_name);
            b.iter_with_setup(
                || {
                    // Create temp file with JSONL data. Canonicalize the temp
                    // root the way br's config routing does before handing a
                    // path to sync: on macOS the raw temp root is `/var/...`
                    // (a symlink to `/private/var/...`), which the pinned
                    // no-follow JSONL traversal rejects as a symlinked
                    // ancestor. br never hits that because it routes canonical
                    // paths; the bench must not be stricter than br.
                    let dir = TempDir::new().unwrap();
                    let base = dunce::canonicalize(dir.path()).unwrap();
                    let jsonl_path = base.join("issues.jsonl");
                    std::fs::write(&jsonl_path, data).unwrap();

                    let db_path = base.join("import.db");
                    let storage = SqliteStorage::open(&db_path).unwrap();
                    (dir, storage, jsonl_path)
                },
                |(dir, mut storage, jsonl_path)| {
                    let config = beads_rust::sync::ImportConfig::default();
                    beads_rust::sync::import_from_jsonl(&mut storage, &jsonl_path, &config, None)
                        .unwrap();
                    drop(dir);
                },
            );
            log_bench_end(&bench_name, bench_start);
        });
    }

    group.finish();
    log_group_end(group_name);
}

/// Benchmark dirty tracking mark (updates that mark issues dirty).
fn bench_dirty_tracking_mark(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "sync/dirty_mark";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("bench.db");
    let mut storage = SqliteStorage::open(&db_path).unwrap();

    for i in 0..100 {
        let issue = create_test_issue(i);
        storage.create_issue(&issue, "benchmark").unwrap();
    }

    let mut counter = 0usize;
    group.bench_function("mark_100", |b| {
        let bench_name = "sync/dirty_mark/mark_100";
        let bench_start = log_bench_start(bench_name);
        b.iter(|| {
            let id = format!("bench-{:06}", counter % 100);
            let update = IssueUpdate {
                notes: Some(Some(format!("dirty-note-{counter}"))),
                ..IssueUpdate::default()
            };
            let _ = storage.update_issue(black_box(&id), black_box(&update), "benchmark");
            counter += 1;
        });
        log_bench_end(bench_name, bench_start);
    });

    group.finish();
    log_group_end(group_name);
    drop(dir);
}

/// Benchmark dirty tracking query (fetch dirty IDs).
fn bench_dirty_tracking_query(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "sync/dirty_query";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("bench.db");
    let mut storage = SqliteStorage::open(&db_path).unwrap();

    for i in 0..100 {
        let issue = create_test_issue(i);
        storage.create_issue(&issue, "benchmark").unwrap();
    }

    for i in 0..100 {
        let id = format!("bench-{i:06}");
        let update = IssueUpdate {
            notes: Some(Some(format!("dirty-note-{i}"))),
            ..IssueUpdate::default()
        };
        let _ = storage.update_issue(&id, &update, "benchmark");
    }

    group.bench_function("dirty_ids_100", |b| {
        let bench_name = "sync/dirty_query/dirty_ids_100";
        let bench_start = log_bench_start(bench_name);
        b.iter(|| {
            let ids = storage.get_dirty_issue_ids().unwrap();
            black_box(ids)
        });
        log_bench_end(bench_name, bench_start);
    });

    group.finish();
    log_group_end(group_name);
    drop(dir);
}

// =============================================================================
// Dependency Operation Benchmarks
// =============================================================================

/// Benchmark adding dependencies.
fn bench_add_dependency(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "storage/add_dep";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    group.bench_function("single", |b| {
        let bench_name = "storage/add_dep/single";
        let bench_start = log_bench_start(bench_name);
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("bench.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();

        // Create issues first
        for i in 0..100 {
            let issue = create_test_issue(i);
            storage.create_issue(&issue, "benchmark").unwrap();
        }

        let mut counter = 0usize;
        b.iter(|| {
            let from_idx = (counter * 2 + 1) % 50 + 50; // 50-99
            let to_idx = counter % 50; // 0-49
            let from_id = format!("bench-{from_idx:06}");
            let to_id = format!("bench-{to_idx:06}");

            // Ignore duplicate errors
            let _ = storage.add_dependency(
                black_box(&from_id),
                black_box(&to_id),
                "blocks",
                "benchmark",
            );
            counter += 1;
        });
        log_bench_end(bench_name, bench_start);
    });

    group.finish();
    log_group_end(group_name);
}

/// Benchmark cycle detection.
fn bench_cycle_detection(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "storage/cycle_detection";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    // Create a database with complex dependency graph
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("bench.db");
    let mut storage = SqliteStorage::open(&db_path).unwrap();

    // Create a chain of issues: 0 <- 1 <- 2 <- 3 <- ... <- 99
    for i in 0..100 {
        let issue = create_test_issue(i);
        storage.create_issue(&issue, "benchmark").unwrap();
    }

    // Create a long dependency chain
    for i in 1..100 {
        let from_id = format!("bench-{i:06}");
        let to_id = format!("bench-{:06}", i - 1);
        storage
            .add_dependency(&from_id, &to_id, "blocks", "benchmark")
            .ok();
    }

    group.bench_function("would_create_cycle_true", |b| {
        let bench_name = "storage/cycle_detection/would_create_cycle_true";
        let bench_start = log_bench_start(bench_name);
        b.iter(|| {
            // This would create a cycle: 0 -> 99 when 99 -> ... -> 0 exists
            let result = storage.would_create_cycle(
                black_box("bench-000000"),
                black_box("bench-000099"),
                true,
            );
            black_box(result)
        });
        log_bench_end(bench_name, bench_start);
    });

    group.bench_function("would_create_cycle_false", |b| {
        let bench_name = "storage/cycle_detection/would_create_cycle_false";
        let bench_start = log_bench_start(bench_name);
        b.iter(|| {
            // This wouldn't create a cycle: checking a non-existent edge
            let result = storage.would_create_cycle(
                black_box("bench-000099"),
                black_box("bench-000000"),
                true,
            );
            black_box(result)
        });
        log_bench_end(bench_name, bench_start);
    });

    group.finish();
    log_group_end(group_name);
    drop(dir);
}

// =============================================================================
// ID Operation Benchmarks
// =============================================================================

/// Benchmark ID generation.
fn bench_generate_id(c: &mut Criterion) {
    use beads_rust::util::id::{IdConfig, IdGenerator};
    use std::collections::HashSet;

    init_bench_logging();
    let group_name = "id/generate";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    group.bench_function("single", |b| {
        let bench_name = "id/generate/single";
        let bench_start = log_bench_start(bench_name);
        let generator =
            IdGenerator::new(IdConfig::with_prefix("bench").expect("valid benchmark prefix"));
        let now = Utc::now();
        let mut counter = 0usize;

        b.iter(|| {
            let title = format!("Benchmark issue {counter}");
            let id = generator
                .generate(black_box(&title), None, None, now, counter, |_| Ok(false))
                .expect("in-memory collision lookup succeeds");
            counter += 1;
            black_box(id)
        });
        log_bench_end(bench_name, bench_start);
    });

    // Benchmark with collision checking
    group.bench_function("with_collision_check", |b| {
        let bench_name = "id/generate/with_collision_check";
        let bench_start = log_bench_start(bench_name);
        let generator =
            IdGenerator::new(IdConfig::with_prefix("bench").expect("valid benchmark prefix"));
        let now = Utc::now();
        let mut existing: HashSet<String> = HashSet::new();
        let mut counter = 0usize;

        b.iter(|| {
            let title = format!("Benchmark issue {counter}");
            let id = generator
                .generate(black_box(&title), None, None, now, counter, |id| {
                    Ok(existing.contains(id))
                })
                .expect("in-memory collision lookup succeeds");
            existing.insert(id.clone());
            counter += 1;
            black_box(id)
        });
        log_bench_end(bench_name, bench_start);
    });

    group.finish();
    log_group_end(group_name);
}

/// Benchmark ID prefix resolution against 100 known IDs.
fn bench_resolve_id_prefix(c: &mut Criterion) {
    use beads_rust::util::id::{IdResolver, ResolverConfig, find_matching_ids};
    use std::collections::HashSet;

    init_bench_logging();
    let group_name = "id/resolve_prefix";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    let all_ids: Vec<String> = (0..100).map(|i| format!("bd-{i:06}")).collect();
    let id_set: HashSet<String> = all_ids.iter().cloned().collect();
    let resolver = IdResolver::new(ResolverConfig::with_prefix("bd"));
    let mut counter = 0usize;

    group.bench_function("prefix_100", |b| {
        let bench_name = "id/resolve_prefix/prefix_100";
        let bench_start = log_bench_start(bench_name);
        b.iter(|| {
            let full_id = &all_ids[counter % all_ids.len()];
            let partial = full_id
                .split_once('-')
                .map_or(full_id.as_str(), |(_, hash)| hash);
            let resolution = resolver.resolve(
                black_box(partial),
                |id| id_set.contains(id),
                |hash| find_matching_ids(&all_ids, hash),
            );
            counter += 1;
            black_box(resolution)
        });
        log_bench_end(bench_name, bench_start);
    });

    group.finish();
    log_group_end(group_name);
}

/// Benchmark ID hash computation.
fn bench_id_hash(c: &mut Criterion) {
    use beads_rust::util::id::compute_id_hash;

    init_bench_logging();
    let group_name = "id/hash";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    for len in [4, 6, 8, 12] {
        group.bench_with_input(BenchmarkId::new("length", len), &len, |b, &len| {
            let bench_name = format!("id/hash/length={len}");
            let bench_start = log_bench_start(&bench_name);
            let input = "Benchmark issue title for hashing performance test";
            b.iter(|| {
                let hash = compute_id_hash(black_box(input), len);
                black_box(hash)
            });
            log_bench_end(&bench_name, bench_start);
        });
    }

    group.finish();
    log_group_end(group_name);
}

/// Benchmark content hashing.
fn bench_content_hash(c: &mut Criterion) {
    use beads_rust::util::content_hash;

    init_bench_logging();
    let group_name = "id/content_hash";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    // Single issue hash
    group.bench_function("single", |b| {
        let bench_name = "id/content_hash/single";
        let bench_start = log_bench_start(bench_name);
        let issue = create_test_issue(0);
        b.iter(|| {
            let hash = content_hash(black_box(&issue));
            black_box(hash)
        });
        log_bench_end(bench_name, bench_start);
    });

    // Batch hashing
    for size in [10, 100, 500] {
        let issues: Vec<_> = (0..size).map(create_test_issue).collect();
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("batch", size), &issues, |b, issues| {
            let bench_name = format!("id/content_hash/batch={size}");
            let bench_start = log_bench_start(&bench_name);
            b.iter(|| {
                let hashes: Vec<_> = issues.iter().map(content_hash).collect();
                black_box(hashes)
            });
            log_bench_end(&bench_name, bench_start);
        });
    }

    group.finish();
    log_group_end(group_name);
}

// =============================================================================
// Search Operation Benchmarks
// =============================================================================

/// Benchmark search operations.
fn bench_search(c: &mut Criterion) {
    init_bench_logging();
    let group_name = "storage/search";
    if should_skip_group(group_name) {
        return;
    }
    log_group_start(group_name);
    let mut group = c.benchmark_group(group_name);
    configure_group(&mut group);

    for size in [100, 500, 1000] {
        let (_dir, storage) = setup_db_with_issues(size);
        let filters = ListFilters::default();

        group.bench_with_input(
            BenchmarkId::new("title_match", size),
            &storage,
            |b, storage| {
                let bench_name = format!("storage/search/title_match/size={size}");
                let bench_start = log_bench_start(&bench_name);
                b.iter(|| {
                    let results = storage
                        .search_issues(black_box("Benchmark"), &filters)
                        .unwrap();
                    black_box(results)
                });
                log_bench_end(&bench_name, bench_start);
            },
        );

        group.bench_with_input(
            BenchmarkId::new("description_match", size),
            &storage,
            |b, storage| {
                let bench_name = format!("storage/search/description_match/size={size}");
                let bench_start = log_bench_start(&bench_name);
                b.iter(|| {
                    let results = storage
                        .search_issues(black_box("Description"), &filters)
                        .unwrap();
                    black_box(results)
                });
                log_bench_end(&bench_name, bench_start);
            },
        );
    }

    group.finish();
    log_group_end(group_name);
}

// =============================================================================
// Criterion Groups
// =============================================================================

criterion_group!(
    storage_benches,
    bench_create_single,
    bench_create_batch,
    bench_update_issue,
    bench_close_issue_with_reason,
    bench_delete_issue,
    bench_list_issues,
    bench_list_issues_filtered,
    bench_ready_query,
    bench_blocked_query,
    bench_add_dependency,
    bench_cycle_detection,
    bench_search,
);

criterion_group!(
    sync_benches,
    bench_export,
    bench_import,
    bench_dirty_tracking_mark,
    bench_dirty_tracking_query,
);

criterion_group!(
    id_benches,
    bench_generate_id,
    bench_resolve_id_prefix,
    bench_id_hash,
    bench_content_hash,
);

criterion_main!(storage_benches, sync_benches, id_benches);
