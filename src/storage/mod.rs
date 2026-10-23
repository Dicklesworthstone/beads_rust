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
mod lint;
#[cfg(test)]
mod local_restore_tests;
pub(crate) mod portable_history;
pub mod schema;
mod search;
pub mod sqlite;

pub(crate) use portable_history::{
    PortableHistoryIssue, PortableHistoryReplay, PortableHistorySnapshot,
    validate as validate_portable_history,
};
#[cfg(test)]
pub(crate) use search::unicode_issue_fields_match;
pub(crate) use sqlite::{BulkDependencyInsert, ChangelogIssueRow};
pub use sqlite::{
    CloseMetadataRow, EventAttribution, IssueUpdate, LabelSetChanges, LabelUpdate, ListFilters,
    ReadyFilters, ReadySortPolicy, SqliteStorage, StatsIssueRow,
};
