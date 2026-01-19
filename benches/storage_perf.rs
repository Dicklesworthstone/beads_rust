//! Storage and sync performance benchmarks.
//!
//! Run with: cargo bench
//!
//! Performance Targets:
//! | Operation           | Target    | Description                      |
//! |---------------------|-----------|----------------------------------|
//! | Create              | < 1ms     | Single issue creation            |
//! | List (1k)           | < 10ms    | List 1000 issues                 |
//! | List (10k)          | < 100ms   | List 10000 issues                |
//! | Ready (1k/2k)       | < 5ms     | Ready query: 1k issues, 2k deps  |
//! | Ready (10k/20k)     | < 50ms    | Ready query: 10k issues, 20k deps|
//! | Export (10k)        | < 500ms   | Export 10k issues to JSONL       |
//! | Import (10k)        | < 1s      | Import 10k issues from JSONL     |

// Allow benign casts and drop order warnings in benchmark code
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::significant_drop_tightening
)]

use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::storage::{IssueUpdate, ListFilters, ReadyFilters, ReadySortPolicy, SqliteStorage};
use chrono::Utc;
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::io::Cursor;
use tempfile::TempDir;

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
        priority: Priority((i % 5) as i32),
        issue_type: match i % 4 {
            0 => IssueType::Bug,
            1 => IssueType::Feature,
            2 => IssueType::Task,
            _ => IssueType::Chore,
        },
        assignee: if i % 3 == 0 {
            Some(format!("user{}", i % 10))
        } else {
            None
        },
        owner: Some("benchmark@test.com".to_string()),
        estimated_minutes: Some((i % 60 + 30) as i32),
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

    // Create dependencies (avoiding cycles)
    for d in 0..dep_count {
        let from_idx = (d * 2 + 1) % issue_count;
        let to_idx = (d * 2) % issue_count;
        if from_idx != to_idx && from_idx > to_idx {
            let from_id = format!("bench-{from_idx:06}");
            let to_id = format!("bench-{to_idx:06}");
            // Ignore errors from duplicate dependencies
            let _ = storage.add_dependency(&from_id, &to_id, "blocks", "benchmark");
        }
    }

    (dir, storage)
}

// =============================================================================
// Storage Operation Benchmarks
// =============================================================================

/// Benchmark single issue creation.
fn bench_create_single(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/create");

    group.bench_function("single", |b| {
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
    });

    group.finish();
}

/// Benchmark batch issue creation.
fn bench_create_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/create_batch");

    for size in [10, 100, 500] {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
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
        });
    }

    group.finish();
}

/// Benchmark updating an issue.
fn bench_update_issue(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/update");

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
        b.iter(|| {
            let id = format!("bench-{:06}", counter % 100);
            let update = IssueUpdate {
                title: Some(format!("Updated title {counter}")),
                priority: Some(Priority(((counter % 4) + 1) as i32)),
                status: None,
                description: None,
                design: None,
                acceptance_criteria: None,
                notes: None,
                issue_type: None,
                assignee: None,
                owner: None,
                estimated_minutes: None,
                due_at: None,
                defer_until: None,
                external_ref: None,
                closed_at: None,
                close_reason: None,
                closed_by_session: None,
                deleted_at: None,
                deleted_by: None,
                delete_reason: None,
            };
            let _ = storage.update_issue(black_box(&id), black_box(&update), "benchmark");
            counter += 1;
        });
    });

    group.finish();
    drop(dir);
}

/// Benchmark deleting an issue (soft delete / tombstone).
fn bench_delete_issue(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/delete");

    group.bench_function("single", |b| {
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
    });

    group.finish();
}

// =============================================================================
// Query Operation Benchmarks
// =============================================================================

/// Benchmark listing issues.
fn bench_list_issues(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/list");

    for size in [100, 500, 1000, 2000, 5000] {
        let (_dir, storage) = setup_db_with_issues(size);

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &storage, |b, storage| {
            b.iter(|| {
                let filters = ListFilters::default();
                let issues = storage.list_issues(&filters).unwrap();
                black_box(issues)
            });
        });
    }

    group.finish();
}

/// Benchmark ready query with dependencies.
fn bench_ready_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/ready");

    for (issues, deps) in [(100, 200), (500, 1000), (1000, 2000)] {
        let (_dir, storage) = setup_db_with_deps(issues, deps);
        let label = format!("{issues}i_{deps}d");

        group.bench_with_input(
            BenchmarkId::new("issues_deps", &label),
            &storage,
            |b, storage| {
                b.iter(|| {
                    let filters = ReadyFilters::default();
                    let ready = storage
                        .get_ready_issues(&filters, ReadySortPolicy::default())
                        .unwrap();
                    black_box(ready)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark blocked issues query.
fn bench_blocked_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/blocked");

    for (issues, deps) in [(100, 200), (500, 1000)] {
        let (_dir, storage) = setup_db_with_deps(issues, deps);
        let label = format!("{issues}i_{deps}d");

        group.bench_with_input(
            BenchmarkId::new("issues_deps", &label),
            &storage,
            |b, storage| {
                b.iter(|| {
                    let blocked = storage.get_blocked_issues().unwrap();
                    black_box(blocked)
                });
            },
        );
    }

    group.finish();
}

// =============================================================================
// Sync Operation Benchmarks
// =============================================================================

/// Benchmark JSONL export.
fn bench_export(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync/export");

    for size in [100, 500, 1000, 2000, 5000] {
        let (_dir, storage) = setup_db_with_issues(size);

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &storage, |b, storage| {
            b.iter(|| {
                let mut buffer = Cursor::new(Vec::new());
                beads_rust::sync::export_to_writer(storage, &mut buffer).unwrap();
                black_box(buffer.into_inner())
            });
        });
    }

    group.finish();
}

/// Benchmark JSONL import.
fn bench_import(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync/import");

    for size in [100, 500, 1000, 2000, 5000] {
        // Create source data
        let (_src_dir, src_storage) = setup_db_with_issues(size);
        let mut buffer = Cursor::new(Vec::new());
        beads_rust::sync::export_to_writer(&src_storage, &mut buffer).unwrap();
        let jsonl_data = buffer.into_inner();

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &jsonl_data, |b, data| {
            b.iter_with_setup(
                || {
                    // Create temp file with JSONL data
                    let dir = TempDir::new().unwrap();
                    let jsonl_path = dir.path().join("issues.jsonl");
                    std::fs::write(&jsonl_path, data).unwrap();

                    let db_path = dir.path().join("import.db");
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
        });
    }

    group.finish();
}

// =============================================================================
// Dependency Operation Benchmarks
// =============================================================================

/// Benchmark adding dependencies.
fn bench_add_dependency(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/add_dep");

    group.bench_function("single", |b| {
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
    });

    group.finish();
}

/// Benchmark cycle detection.
fn bench_cycle_detection(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/cycle_detection");

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
        b.iter(|| {
            // This would create a cycle: 0 -> 99 when 99 -> ... -> 0 exists
            let result = storage.would_create_cycle(
                black_box("bench-000000"),
                black_box("bench-000099"),
                true,
            );
            black_box(result)
        });
    });

    group.bench_function("would_create_cycle_false", |b| {
        b.iter(|| {
            // This wouldn't create a cycle: checking a non-existent edge
            let result = storage.would_create_cycle(
                black_box("bench-000099"),
                black_box("bench-000000"),
                true,
            );
            black_box(result)
        });
    });

    group.finish();
    drop(dir);
}

// =============================================================================
// ID Operation Benchmarks
// =============================================================================

/// Benchmark ID generation.
fn bench_generate_id(c: &mut Criterion) {
    use beads_rust::util::id::{IdConfig, IdGenerator};
    use std::collections::HashSet;

    let mut group = c.benchmark_group("id/generate");

    group.bench_function("single", |b| {
        let generator = IdGenerator::new(IdConfig::with_prefix("bench"));
        let now = Utc::now();
        let mut counter = 0usize;

        b.iter(|| {
            let title = format!("Benchmark issue {counter}");
            let id = generator.generate(black_box(&title), None, None, now, counter, |_| false);
            counter += 1;
            black_box(id)
        });
    });

    // Benchmark with collision checking
    group.bench_function("with_collision_check", |b| {
        let generator = IdGenerator::new(IdConfig::with_prefix("bench"));
        let now = Utc::now();
        let mut existing: HashSet<String> = HashSet::new();
        let mut counter = 0usize;

        b.iter(|| {
            let title = format!("Benchmark issue {counter}");
            let id = generator.generate(black_box(&title), None, None, now, counter, |id| {
                existing.contains(id)
            });
            existing.insert(id.clone());
            counter += 1;
            black_box(id)
        });
    });

    group.finish();
}

/// Benchmark ID hash computation.
fn bench_id_hash(c: &mut Criterion) {
    use beads_rust::util::id::compute_id_hash;

    let mut group = c.benchmark_group("id/hash");

    for len in [4, 6, 8, 12] {
        group.bench_with_input(BenchmarkId::new("length", len), &len, |b, &len| {
            let input = "Benchmark issue title for hashing performance test";
            b.iter(|| {
                let hash = compute_id_hash(black_box(input), len);
                black_box(hash)
            });
        });
    }

    group.finish();
}

/// Benchmark content hashing.
fn bench_content_hash(c: &mut Criterion) {
    use beads_rust::util::content_hash;

    let mut group = c.benchmark_group("id/content_hash");

    // Single issue hash
    group.bench_function("single", |b| {
        let issue = create_test_issue(0);
        b.iter(|| {
            let hash = content_hash(black_box(&issue));
            black_box(hash)
        });
    });

    // Batch hashing
    for size in [10, 100, 500] {
        let issues: Vec<_> = (0..size).map(create_test_issue).collect();
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("batch", size), &issues, |b, issues| {
            b.iter(|| {
                let hashes: Vec<_> = issues.iter().map(content_hash).collect();
                black_box(hashes)
            });
        });
    }

    group.finish();
}

// =============================================================================
// Search Operation Benchmarks
// =============================================================================

/// Benchmark search operations.
fn bench_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage/search");

    for size in [100, 500, 1000] {
        let (_dir, storage) = setup_db_with_issues(size);
        let filters = ListFilters::default();

        group.bench_with_input(
            BenchmarkId::new("title_match", size),
            &storage,
            |b, storage| {
                b.iter(|| {
                    let results = storage
                        .search_issues(black_box("Benchmark"), &filters)
                        .unwrap();
                    black_box(results)
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("description_match", size),
            &storage,
            |b, storage| {
                b.iter(|| {
                    let results = storage
                        .search_issues(black_box("Description"), &filters)
                        .unwrap();
                    black_box(results)
                });
            },
        );
    }

    group.finish();
}

// =============================================================================
// Criterion Groups
// =============================================================================

criterion_group!(
    storage_benches,
    bench_create_single,
    bench_create_batch,
    bench_update_issue,
    bench_delete_issue,
    bench_list_issues,
    bench_ready_query,
    bench_blocked_query,
    bench_add_dependency,
    bench_cycle_detection,
    bench_search,
);

criterion_group!(sync_benches, bench_export, bench_import,);

criterion_group!(
    id_benches,
    bench_generate_id,
    bench_id_hash,
    bench_content_hash,
);

criterion_main!(storage_benches, sync_benches, id_benches);
