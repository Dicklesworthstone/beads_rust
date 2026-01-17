//! `SQLite` storage layer for `beads_rust`.
//!
//! This module provides the persistence layer using `SQLite` with:
//! - WAL mode for concurrent reads
//! - Transaction discipline for atomic writes
//! - Dirty tracking for JSONL export
//! - Blocked cache for ready/blocked queries
//!
//! # Submodules
//!
//! - [`events`] - Audit event storage (insertion, retrieval)
//! - [`schema`] - Database schema definitions
//! - [`sqlite`] - Main `SQLite` storage implementation

pub mod events;
pub mod schema;
pub mod sqlite;

pub use sqlite::{IssueUpdate, ListFilters, ReadyFilters, ReadySortPolicy, SqliteStorage};
