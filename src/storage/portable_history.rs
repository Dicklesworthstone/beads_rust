//! Portable workflow audit evidence.
//!
//! The repository format uses per-issue sequence numbers instead of SQLite row
//! ids, so a fresh clone can replay events and gate revisions deterministically.
//! Each issue's evidence is bound to that issue's anchor (status, creation and
//! update time) and is only applied to, or carried forward for, a row at that
//! same anchor. Nothing here picks a winner between two diverging histories:
//! the database keeps its own evidence and the divergence is reported.

use super::SqliteStorage;
use crate::error::{BeadsError, Result};
use crate::franken_sync::SqliteValue;
use chrono::SecondsFormat;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

// Items here are `pub`, not `pub(crate)`: this module is `pub(crate)` in
// src/storage/mod.rs, so they stay crate-visible either way and `pub(crate)`
// is what `clippy::redundant_pub_crate` rejects.

/// The issue revision a piece of evidence describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableHistoryIssue {
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

impl PortableHistoryIssue {
    /// An anchor whose timestamps use one canonical RFC 3339 spelling. The
    /// database stores `+00:00` offsets and the JSONL export writes `Z`, so
    /// the raw strings of one instant differ; both sides are canonicalized
    /// before they are compared. `None` when a timestamp does not parse.
    #[must_use]
    pub fn canonical(status: &str, created_at: &str, updated_at: &str) -> Option<Self> {
        Some(Self {
            status: status.to_owned(),
            created_at: canonical_time(created_at)?,
            updated_at: canonical_time(updated_at)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableHistoryTable {
    pub table: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableHistorySnapshot {
    pub issues: BTreeMap<String, PortableHistoryIssue>,
    pub tables: Vec<PortableHistoryTable>,
}

/// An audit receipt for incompatible verification branches, not a verifier's
/// verdict. Existing event portability carries these receipts between clones.
pub const GATE_HISTORY_CONFLICT_EVENT: &str = "workflow_gate_history_conflict";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateHistoryConflictScope {
    pub from_status: String,
    pub to_status: String,
    pub gate: String,
}

/// The incoming branch is retained verbatim in portable sequence space. Only
/// referenced status events are embedded: conflict receipts never recursively
/// contain other receipts or invent authentication for recorded actor names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateHistoryConflictEvidence {
    pub anchor: PortableHistoryIssue,
    pub gate_history: PortableHistoryTable,
    pub status_events: PortableHistoryTable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateHistoryConflict {
    pub version: u32,
    pub source_digest: String,
    /// False archives evidence that its source had already resolved by a real
    /// status transition. Moving that receipt to a new local event ID must not
    /// reactivate it.
    pub blocking: bool,
    pub scopes: Vec<GateHistoryConflictScope>,
    pub evidence: GateHistoryConflictEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateHistoryConflictRecord {
    pub id: i64,
    pub issue_id: String,
    pub active: bool,
    #[serde(flatten)]
    pub conflict: GateHistoryConflict,
}

impl GateHistoryConflictRecord {
    #[must_use]
    pub fn affects(&self, from: &str, to: &str, gate: &str) -> bool {
        self.active
            && self.conflict.scopes.iter().any(|scope| {
                scope.from_status == canonical_status(from)
                    && scope.to_status == canonical_status(to)
                    && scope.gate == gate.trim().to_ascii_lowercase()
            })
    }
}

/// Decode typed conflict evidence. A malformed active receipt must not become
/// an absent restriction at the protected report/transition chokepoints.
///
/// # Errors
/// Returns an error for malformed, mismatched, or unsupported audit evidence.
pub fn decode_gate_history_conflict(
    id: i64,
    issue_id: &str,
    comment: &str,
    status_revision: i64,
) -> Result<GateHistoryConflictRecord> {
    let conflict: GateHistoryConflict = serde_json::from_str(comment)
        .map_err(|error| invalid(format!("invalid gate-history conflict receipt: {error}")))?;
    validate_gate_history_conflict(issue_id, &conflict)?;
    if id <= 0 {
        return Err(invalid("gate-history conflict event has no positive id"));
    }
    Ok(GateHistoryConflictRecord {
        id,
        issue_id: issue_id.to_owned(),
        active: conflict.blocking && id > status_revision,
        conflict,
    })
}

/// Evidence chosen per issue from several snapshots.
#[derive(Debug)]
pub struct PortableHistorySelection {
    /// The chosen evidence, holding only the issues some source described.
    pub history: PortableHistorySnapshot,
    /// Requested issues that no source described at the requested anchor.
    pub unmatched: Vec<String>,
    /// For each source, whether it supplied at least one issue.
    pub used: Vec<bool>,
}

/// A database's evidence with evidence carried forward from earlier
/// snapshots.
#[derive(Debug)]
pub struct PortableHistoryCarry {
    pub history: PortableHistorySnapshot,
    /// For each earlier snapshot, whether it supplied at least one issue.
    pub used: Vec<bool>,
    /// Issues an earlier snapshot described at the same anchor with evidence
    /// that diverged from the database's; the database's evidence was kept.
    pub diverged: Vec<String>,
}

/// What replaying evidence into the database did, issue by issue.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PortableHistoryReplay {
    /// Issues whose evidence is now present in the database (replayed, or
    /// already there).
    pub replayed: Vec<String>,
    /// Issues left alone because the database row is a different revision
    /// than the one the evidence describes, such as a row that import kept
    /// over a newer one the workflow policy could not reach (GitHub #540).
    pub other_revision: Vec<String>,
    /// Issues whose evidence diverged from the database's own; the database
    /// kept its evidence.
    pub diverged: Vec<String>,
}

#[derive(Clone, Copy)]
enum Kind {
    Text,
    OptionalText,
    Integer,
    Boolean,
    Timestamp,
    OptionalTimestamp,
}

struct Spec {
    name: &'static str,
    columns: &'static [(&'static str, Kind)],
    key: &'static [usize],
    append: bool,
}
impl Spec {
    fn owner(&self) -> usize {
        usize::from(self.append)
    }
    fn names(&self) -> Vec<String> {
        self.columns
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect()
    }
    fn key(&self, row: &[Value]) -> Vec<String> {
        self.key
            .iter()
            .map(|&i| row[i].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

const SPECS: &[Spec] = &[
    Spec {
        name: "events",
        columns: &[
            ("id", Kind::Integer),
            ("issue_id", Kind::Text),
            ("event_type", Kind::Text),
            ("actor", Kind::Text),
            ("old_value", Kind::OptionalText),
            ("new_value", Kind::OptionalText),
            ("comment", Kind::OptionalText),
            ("created_at", Kind::Timestamp),
            ("agent_name", Kind::OptionalText),
            ("harness", Kind::OptionalText),
            ("model", Kind::OptionalText),
        ],
        key: &[1],
        append: true,
    },
    Spec {
        name: "gate_result_history",
        columns: &[
            ("id", Kind::Integer),
            ("issue_id", Kind::Text),
            ("from_status", Kind::Text),
            ("to_status", Kind::Text),
            ("status_revision", Kind::Integer),
            ("gate", Kind::Text),
            ("provider", Kind::Text),
            ("passed", Kind::Boolean),
            ("note", Kind::OptionalText),
            ("recorded_by", Kind::OptionalText),
            ("recorded_at", Kind::Timestamp),
            ("assignee_at_report", Kind::OptionalText),
            ("override_reason", Kind::OptionalText),
        ],
        key: &[1],
        append: true,
    },
    Spec {
        name: "capacity_exemption_history",
        columns: &[
            ("id", Kind::Integer),
            ("issue_id", Kind::Text),
            ("capacity_kind", Kind::Text),
            ("capacity_name", Kind::Text),
            ("action", Kind::Text),
            ("provider", Kind::Text),
            ("reason", Kind::OptionalText),
            ("actor", Kind::Text),
            ("expires_at", Kind::OptionalTimestamp),
            ("recorded_at", Kind::Timestamp),
        ],
        key: &[1],
        append: true,
    },
    Spec {
        name: "gate_results",
        columns: &[
            ("issue_id", Kind::Text),
            ("gate", Kind::Text),
            ("provider", Kind::Text),
            ("passed", Kind::Boolean),
            ("note", Kind::OptionalText),
            ("recorded_by", Kind::OptionalText),
            ("recorded_at", Kind::Timestamp),
        ],
        key: &[0, 1, 2],
        append: false,
    },
    Spec {
        name: "close_metadata",
        // Keep the v1 companion schema stable. Durable `closed_by` belongs to
        // the primary issue JSONL; replay preserves it and any known primary
        // agent attribution even when an older companion omits that evidence.
        columns: &[
            ("issue_id", Kind::Text),
            ("closed_by_agent_name", Kind::OptionalText),
            ("closed_by_harness", Kind::OptionalText),
            ("closed_by_model", Kind::OptionalText),
            ("bypassed_policy", Kind::Boolean),
            ("bypass_reason", Kind::OptionalText),
            ("policy_gates_fired", Kind::OptionalText),
            ("recorded_at", Kind::Timestamp),
        ],
        key: &[0],
        append: false,
    },
    Spec {
        name: "capacity_occupancy",
        columns: &[
            ("issue_id", Kind::Text),
            ("actor", Kind::OptionalText),
            ("agent_name", Kind::OptionalText),
            ("harness", Kind::OptionalText),
            ("session", Kind::OptionalText),
            ("recorded_at", Kind::Timestamp),
        ],
        key: &[0],
        append: false,
    },
    Spec {
        name: "capacity_exemptions",
        columns: &[
            ("issue_id", Kind::Text),
            ("capacity_kind", Kind::Text),
            ("capacity_name", Kind::Text),
            ("provider", Kind::Text),
            ("reason", Kind::Text),
            ("granted_by", Kind::Text),
            ("granted_at", Kind::Timestamp),
            ("expires_at", Kind::OptionalTimestamp),
            ("ended_at", Kind::OptionalTimestamp),
            ("ended_action", Kind::OptionalText),
            ("ended_by", Kind::OptionalText),
        ],
        key: &[0, 1, 2],
        append: false,
    },
];

/// Issues whose evidence may leave this database: the rows the JSONL export
/// writes. Ephemeral issues and wisps stay local, and so does their evidence.
const EXPORTABLE_ISSUES: &str = "SELECT id, status, updated_at, created_at FROM issues \
     WHERE (ephemeral = 0 OR ephemeral IS NULL) AND id NOT LIKE '%-wisp-%'";

fn invalid(message: impl std::fmt::Display) -> BeadsError {
    BeadsError::Config(format!("Portable workflow history: {message}"))
}
fn parse_time(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|v| v.with_timezone(&chrono::Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|v| v.and_utc())
        })
}
fn canonical_time(value: &str) -> Option<String> {
    parse_time(value).map(|time| time.to_rfc3339_opts(SecondsFormat::AutoSi, true))
}
fn valid(kind: Kind, value: &Value) -> bool {
    match kind {
        Kind::Text => value.is_string(),
        Kind::OptionalText => value.is_null() || value.is_string(),
        Kind::Integer => value.as_i64().is_some(),
        Kind::Boolean => matches!(value.as_i64(), Some(0 | 1)),
        Kind::Timestamp => value.as_str().and_then(parse_time).is_some(),
        Kind::OptionalTimestamp => value.is_null() || value.as_str().and_then(parse_time).is_some(),
    }
}
fn owner<'a>(spec: &Spec, row: &'a [Value]) -> &'a str {
    row[spec.owner()].as_str().unwrap_or_default()
}
fn from_sql(value: &SqliteValue) -> Result<Value> {
    match value {
        SqliteValue::Null => Ok(Value::Null),
        SqliteValue::Integer(v) => Ok(Value::from(*v)),
        SqliteValue::Text(v) => Ok(Value::String(v.to_string())),
        _ => Err(invalid("unsupported audit column type")),
    }
}
fn to_sql(value: &Value) -> Result<SqliteValue> {
    match value {
        Value::Null => Ok(SqliteValue::Null),
        Value::String(text) => Ok(SqliteValue::from(text.as_str())),
        _ => value
            .as_i64()
            .map(SqliteValue::from)
            .ok_or_else(|| invalid("unsupported audit value")),
    }
}

fn canonical_status(value: &str) -> String {
    value.trim().parse::<crate::model::Status>().map_or_else(
        |_| value.trim().to_ascii_lowercase(),
        |status| status.as_str().to_ascii_lowercase(),
    )
}

fn conflict_scopes(evidence: &GateHistoryConflictEvidence) -> Vec<GateHistoryConflictScope> {
    let from = canonical_status(&evidence.anchor.status);
    evidence
        .gate_history
        .rows
        .iter()
        .filter_map(|row| {
            let from_status = canonical_status(row.get(2)?.as_str()?);
            let to_status = canonical_status(row.get(3)?.as_str()?);
            let gate = row.get(5)?.as_str()?.trim().to_ascii_lowercase();
            (from_status == from && !to_status.is_empty() && !gate.is_empty()).then_some(
                GateHistoryConflictScope {
                    from_status,
                    to_status,
                    gate,
                },
            )
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn conflict_digest(evidence: &GateHistoryConflictEvidence) -> Result<String> {
    Ok(crate::util::hex_encode(&Sha256::digest(
        serde_json::to_vec(&(
            &evidence.anchor.created_at,
            &evidence.gate_history,
            &evidence.status_events,
        ))?,
    )))
}

fn validate_conflict_table<'a>(
    issue_id: &str,
    table: &'a PortableHistoryTable,
    spec: &Spec,
) -> Result<Vec<&'a Vec<Value>>> {
    if table.table != spec.name || table.columns != spec.names() {
        return Err(invalid("noncanonical gate-history conflict evidence table"));
    }
    for row in &table.rows {
        if row.len() != spec.columns.len()
            || spec
                .columns
                .iter()
                .zip(row)
                .any(|((_, kind), value)| !valid(*kind, value))
            || owner(spec, row) != issue_id
        {
            return Err(invalid("invalid gate-history conflict evidence row"));
        }
    }
    Ok(table.rows.iter().collect())
}

fn validate_gate_history_conflict(issue_id: &str, conflict: &GateHistoryConflict) -> Result<()> {
    let evidence = &conflict.evidence;
    if conflict.version != 1
        || issue_id.is_empty()
        || canonical_time(&evidence.anchor.created_at).as_deref()
            != Some(evidence.anchor.created_at.as_str())
        || canonical_time(&evidence.anchor.updated_at).as_deref()
            != Some(evidence.anchor.updated_at.as_str())
    {
        return Err(invalid("invalid gate-history conflict version or anchor"));
    }
    let events = validate_conflict_table(issue_id, &evidence.status_events, &SPECS[0])?;
    let mut event_ids = BTreeSet::new();
    let mut previous = 0;
    for row in events {
        let id = row[0].as_i64().unwrap_or_default();
        if id <= previous || row[2].as_str() != Some("status_changed") {
            return Err(invalid("invalid conflict status-event reference"));
        }
        previous = id;
        event_ids.insert(id);
    }
    let gates = validate_conflict_table(issue_id, &evidence.gate_history, &SPECS[1])?;
    let mut referenced = BTreeSet::new();
    for (index, row) in gates.iter().enumerate() {
        let expected = i64::try_from(index).unwrap_or(i64::MAX).saturating_add(1);
        let revision = row[4].as_i64().unwrap_or(-1);
        if row[0].as_i64() != Some(expected)
            || revision < 0
            || (revision > 0 && !event_ids.contains(&revision))
        {
            return Err(invalid("invalid conflict gate sequence or status revision"));
        }
        if revision > 0 {
            referenced.insert(revision);
        }
    }
    if referenced != event_ids
        || (conflict.blocking && conflict.scopes.is_empty())
        || conflict.scopes != conflict_scopes(evidence)
        || conflict.source_digest != conflict_digest(evidence)?
    {
        return Err(invalid("gate-history conflict evidence or digest mismatch"));
    }
    Ok(())
}

pub fn validate(snapshot: &PortableHistorySnapshot) -> Result<()> {
    if snapshot.tables.len() != SPECS.len() {
        return Err(invalid("checkpoint must contain all seven history tables"));
    }
    for (id, anchor) in &snapshot.issues {
        // Anchors are compared as strings, so only the canonical spelling of
        // each instant is accepted. Any status is: import stores an empty
        // custom status as it is, and refusing that anchor would stop every
        // export of the database holding it.
        if id.is_empty()
            || canonical_time(&anchor.created_at).as_deref() != Some(anchor.created_at.as_str())
            || canonical_time(&anchor.updated_at).as_deref() != Some(anchor.updated_at.as_str())
        {
            return Err(invalid(format!("invalid issue anchor {id}")));
        }
    }
    let mut events = HashMap::new();
    for (spec, table) in SPECS.iter().zip(&snapshot.tables) {
        let legacy_gate_history =
            spec.name == "gate_result_history" && table.columns == spec.names()[..11];
        if table.table != spec.name || (table.columns != spec.names() && !legacy_gate_history) {
            return Err(invalid(format!(
                "noncanonical history table {}",
                table.table
            )));
        }
        let mut sequences = HashMap::<&str, i64>::new();
        let mut previous: Option<Vec<String>> = None;
        for row in &table.rows {
            if row.len() != table.columns.len() {
                return Err(invalid(format!("{} row has wrong column count", spec.name)));
            }
            for ((name, kind), value) in spec.columns.iter().zip(row) {
                if !valid(*kind, value) {
                    return Err(invalid(format!("invalid {}.{name}", spec.name)));
                }
            }
            let issue = owner(spec, row);
            if !snapshot.issues.contains_key(issue) {
                return Err(invalid(format!(
                    "{} references unknown issue {issue}",
                    spec.name
                )));
            }
            let key = spec.key(row);
            if previous
                .as_ref()
                .is_some_and(|p| if spec.append { p > &key } else { p >= &key })
            {
                return Err(invalid(format!(
                    "{} rows have duplicate keys or noncanonical order",
                    spec.name
                )));
            }
            previous = Some(key);
            if spec.append {
                let seq = sequences.entry(issue).or_default();
                *seq += 1;
                if row[0].as_i64() != Some(*seq) {
                    return Err(invalid(format!(
                        "{} sequence for {issue} is incomplete",
                        spec.name
                    )));
                }
                if spec.name == "events" {
                    events.insert((issue, *seq), row);
                    if row[2].as_str() == Some(GATE_HISTORY_CONFLICT_EVENT) {
                        decode_gate_history_conflict(
                            *seq,
                            issue,
                            row[6].as_str().ok_or_else(|| {
                                invalid("gate-history conflict event has no receipt")
                            })?,
                            0,
                        )?;
                    }
                }
            }
            if spec.name == "gate_result_history" {
                let revision = row[4].as_i64().unwrap_or(-1);
                if revision < 0 {
                    return Err(invalid("negative gate status revision"));
                }
                if revision > 0 {
                    let event = events.get(&(issue, revision)).ok_or_else(|| {
                        invalid(format!(
                            "gate history for {issue} references absent event {revision}"
                        ))
                    })?;
                    // The revision is the issue's latest status change when
                    // the verdict was recorded. Its new status need not equal
                    // `from_status`: an imported status change records no
                    // event, so a database legitimately holds such rows.
                    if event[2].as_str() != Some("status_changed") {
                        return Err(invalid(format!(
                            "gate history for {issue} references an event that is not a status change"
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Check old companion bytes before adding the two nullable v20 fields for
/// comparisons/replay. Checkpoint checksum validation therefore still hashes
/// the original v1 representation; absent evidence stays unknown rather than
/// being inferred from a mutable issue or from the reporting actor.
fn canonical_snapshot(
    snapshot: &PortableHistorySnapshot,
) -> Result<Cow<'_, PortableHistorySnapshot>> {
    validate(snapshot)?;
    let Some(index) = snapshot
        .tables
        .iter()
        .position(|table| table.table == "gate_result_history" && table.columns.len() == 11)
    else {
        return Ok(Cow::Borrowed(snapshot));
    };
    let mut canonical = snapshot.clone();
    let table = &mut canonical.tables[index];
    table.columns.extend([
        "assignee_at_report".to_owned(),
        "override_reason".to_owned(),
    ]);
    for row in &mut table.rows {
        row.extend([Value::Null, Value::Null]);
    }
    Ok(Cow::Owned(canonical))
}

/// Order each table by its key and, for append-only tables, by sequence.
fn sort_rows(snapshot: &mut PortableHistorySnapshot) {
    for (spec, table) in SPECS.iter().zip(&mut snapshot.tables) {
        table.rows.sort_by(|a, b| {
            spec.key(a).cmp(&spec.key(b)).then_with(|| {
                if spec.append {
                    a[0].as_i64().cmp(&b[0].as_i64())
                } else {
                    std::cmp::Ordering::Equal
                }
            })
        });
    }
}

fn normalize(snapshot: &mut PortableHistorySnapshot) -> Result<()> {
    sort_rows(snapshot);
    let mut event_ids = HashMap::<(String, i64), i64>::new();
    for (spec, table) in SPECS.iter().zip(&mut snapshot.tables) {
        let mut seq = HashMap::<String, i64>::new();
        for row in &mut table.rows {
            let issue = owner(spec, row).to_owned();
            if spec.append {
                let raw = row[0]
                    .as_i64()
                    .ok_or_else(|| invalid("non-integer history id"))?;
                let next = seq.entry(issue.clone()).or_default();
                *next += 1;
                row[0] = Value::from(*next);
                if spec.name == "events" {
                    event_ids.insert((issue.clone(), raw), *next);
                }
            }
            if spec.name == "gate_result_history" {
                let raw = row[4].as_i64().unwrap_or_default();
                if raw != 0 {
                    row[4] =
                        Value::from(*event_ids.get(&(issue.clone(), raw)).ok_or_else(|| {
                            invalid(format!(
                                "gate history for {issue} references missing event {raw}"
                            ))
                        })?);
                }
            }
        }
    }
    validate(snapshot)
}

/// How one issue's append-only evidence in two snapshots relates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lineage {
    /// Identical rows.
    Same,
    /// The first snapshot's rows extend the second's.
    Ahead,
    /// The second snapshot's rows extend the first's.
    Behind,
    /// Neither is a prefix of the other.
    Diverged,
}

/// Per-table rows of one snapshot, grouped by owning issue.
struct SnapshotIndex<'a> {
    snapshot: &'a PortableHistorySnapshot,
    by_issue: Vec<HashMap<&'a str, Vec<&'a Vec<Value>>>>,
}

impl<'a> SnapshotIndex<'a> {
    fn new(snapshot: &'a PortableHistorySnapshot) -> Self {
        let by_issue = SPECS
            .iter()
            .zip(&snapshot.tables)
            .map(|(spec, table)| {
                let mut grouped = HashMap::<&str, Vec<&Vec<Value>>>::new();
                for row in &table.rows {
                    grouped.entry(owner(spec, row)).or_default().push(row);
                }
                grouped
            })
            .collect();
        Self { snapshot, by_issue }
    }

    fn rows(&self, table: usize, id: &str) -> &[&'a Vec<Value>] {
        self.by_issue[table].get(id).map_or(&[][..], Vec::as_slice)
    }

    /// How this snapshot's evidence for `id` relates to `other`'s.
    fn lineage(&self, other: &SnapshotIndex<'_>, id: &str) -> Lineage {
        let mut ahead = false;
        let mut behind = false;
        for (index, spec) in SPECS.iter().enumerate() {
            if !spec.append {
                continue;
            }
            let mine = self.rows(index, id);
            let theirs = other.rows(index, id);
            if mine.iter().zip(theirs).any(|(a, b)| a != b) {
                return Lineage::Diverged;
            }
            ahead |= mine.len() > theirs.len();
            behind |= theirs.len() > mine.len();
        }
        match (ahead, behind) {
            (false, false) => Lineage::Same,
            (true, false) => Lineage::Ahead,
            (false, true) => Lineage::Behind,
            (true, true) => Lineage::Diverged,
        }
    }

    fn gate_conflict_evidence(&self, id: &str) -> Result<GateHistoryConflictEvidence> {
        let gate_rows = self.rows(1, id);
        let revisions: HashSet<i64> = gate_rows
            .iter()
            .filter_map(|row| row[4].as_i64().filter(|revision| *revision > 0))
            .collect();
        Ok(GateHistoryConflictEvidence {
            anchor: self
                .snapshot
                .issues
                .get(id)
                .ok_or_else(|| invalid("gate-history conflict has no issue anchor"))?
                .clone(),
            gate_history: PortableHistoryTable {
                table: SPECS[1].name.to_owned(),
                columns: SPECS[1].names(),
                rows: gate_rows.iter().map(|row| (*row).clone()).collect(),
            },
            status_events: PortableHistoryTable {
                table: SPECS[0].name.to_owned(),
                columns: SPECS[0].names(),
                rows: self
                    .rows(0, id)
                    .iter()
                    .filter(|row| row[0].as_i64().is_some_and(|id| revisions.contains(&id)))
                    .map(|row| (*row).clone())
                    .collect(),
            },
        })
    }

    /// A known prefix adds no new verification evidence. Compare referenced
    /// status events too: equal numeric revisions on divergent event branches
    /// are not evidence of the same review attempt.
    fn gate_branch_known_by(&self, other: &SnapshotIndex<'_>, id: &str) -> bool {
        let incoming = self.rows(1, id);
        let local = other.rows(1, id);
        incoming.len() <= local.len()
            && incoming.iter().zip(local).all(|(a, b)| a == b)
            && incoming.iter().all(|row| {
                let revision = row[4].as_i64().unwrap_or_default();
                revision == 0
                    || self
                        .rows(0, id)
                        .iter()
                        .find(|row| row[0].as_i64() == Some(revision))
                        .is_some_and(|event| other.rows(0, id).contains(event))
            })
    }

    /// Carry receipts separately from branch evidence to avoid recursively
    /// nesting snapshots. A receipt resolved in its source remains archival
    /// even if its copied local event ID is newer than local status events.
    fn gate_conflict_receipts(&self, id: &str) -> Result<Vec<GateHistoryConflict>> {
        let status_revision = self
            .rows(0, id)
            .iter()
            .filter(|row| row[2].as_str() == Some("status_changed"))
            .filter_map(|row| row[0].as_i64())
            .max()
            .unwrap_or_default();
        self.rows(0, id)
            .iter()
            .filter(|row| row[2].as_str() == Some(GATE_HISTORY_CONFLICT_EVENT))
            .map(|row| {
                let mut record = decode_gate_history_conflict(
                    row[0].as_i64().unwrap_or_default(),
                    id,
                    row[6]
                        .as_str()
                        .ok_or_else(|| invalid("gate-history conflict event has no receipt"))?,
                    status_revision,
                )?;
                record.conflict.blocking = record.active;
                Ok(record.conflict)
            })
            .collect()
    }
}

fn gate_history_conflicts_to_retain(
    id: &str,
    incoming: &SnapshotIndex<'_>,
    local: &SnapshotIndex<'_>,
) -> Result<Vec<GateHistoryConflict>> {
    let mut known = local
        .gate_conflict_receipts(id)?
        .into_iter()
        .map(|receipt| receipt.source_digest)
        .collect::<HashSet<_>>();
    let mut candidates = incoming.gate_conflict_receipts(id)?;
    if !incoming.gate_branch_known_by(local, id) && !incoming.rows(1, id).is_empty() {
        let evidence = incoming.gate_conflict_evidence(id)?;
        let scopes = conflict_scopes(&evidence);
        candidates.push(GateHistoryConflict {
            version: 1,
            source_digest: conflict_digest(&evidence)?,
            blocking: !scopes.is_empty(),
            scopes,
            evidence,
        });
    }
    let mut retained = Vec::new();
    for receipt in candidates {
        validate_gate_history_conflict(id, &receipt)?;
        if known.insert(receipt.source_digest.clone()) {
            retained.push(receipt);
        }
    }
    Ok(retained)
}

/// A pending merge's database witness is frozen. Even an idempotent REPLACE
/// can change row IDs, so publication must take a read-only path when all
/// evidence is already present and refuse before any otherwise-needed write.
fn preflight_pending_history_observation(
    incoming: &SnapshotIndex<'_>,
    local: &SnapshotIndex<'_>,
    protected: bool,
) -> Result<Vec<String>> {
    let mut diverged = Vec::new();
    for (id, anchor) in &incoming.snapshot.issues {
        if local.snapshot.issues.get(id) != Some(anchor) {
            continue;
        }
        let needs_write = match incoming.lineage(local, id) {
            Lineage::Behind => false,
            Lineage::Same => {
                protected
                    && (0..SPECS.len())
                        .any(|index| incoming.rows(index, id) != local.rows(index, id))
            }
            Lineage::Ahead => protected,
            Lineage::Diverged => {
                diverged.push(id.clone());
                !gate_history_conflicts_to_retain(id, incoming, local)?.is_empty()
            }
        };
        if needs_write {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Pending sync merge owns the database witness; newly observed workflow history for {id} cannot be applied during publication. The companion and pending receipt were left intact. Complete or diagnose the pending merge against its original evidence before importing the new history"
                ),
            });
        }
    }
    Ok(diverged)
}

impl PortableHistorySnapshot {
    #[must_use]
    pub fn empty_for_issues(issues: BTreeMap<String, PortableHistoryIssue>) -> Self {
        Self {
            issues,
            tables: SPECS
                .iter()
                .map(|spec| PortableHistoryTable {
                    table: spec.name.to_owned(),
                    columns: spec.names(),
                    rows: Vec::new(),
                })
                .collect(),
        }
    }

    /// For each `(id, source)` pick, replace (or add) the issue with the
    /// anchor and every row `source` holds for it, then restore canonical
    /// order.
    fn take_issues_from(&mut self, picks: &[(&str, &SnapshotIndex<'_>)]) -> Result<()> {
        if picks.is_empty() {
            return Ok(());
        }
        let picked: HashSet<&str> = picks.iter().map(|(id, _)| *id).collect();
        for (spec, table) in SPECS.iter().zip(&mut self.tables) {
            table.rows.retain(|row| !picked.contains(owner(spec, row)));
        }
        for (id, source) in picks {
            let anchor = source
                .snapshot
                .issues
                .get(*id)
                .ok_or_else(|| invalid(format!("no anchor for issue {id}")))?;
            self.issues.insert((*id).to_owned(), anchor.clone());
            for (index, table) in self.tables.iter_mut().enumerate() {
                table
                    .rows
                    .extend(source.rows(index, id).iter().map(|row| (*row).clone()));
            }
        }
        sort_rows(self);
        validate(self)
    }

    /// Evidence for the issue rows in `anchors`, each taken from the first of
    /// `sources` (newest first) that describes that issue at the same anchor.
    /// An issue no source describes at that anchor is left out and reported
    /// as unmatched.
    pub fn select(
        sources: &[&Self],
        anchors: &BTreeMap<String, PortableHistoryIssue>,
    ) -> Result<PortableHistorySelection> {
        let canonical = sources
            .iter()
            .map(|source| canonical_snapshot(source))
            .collect::<Result<Vec<_>>>()?;
        let indexes: Vec<SnapshotIndex<'_>> = canonical
            .iter()
            .map(|source| SnapshotIndex::new(source))
            .collect();
        let mut used = vec![false; sources.len()];
        let mut unmatched = Vec::new();
        let mut picks = Vec::new();
        for (id, anchor) in anchors {
            match indexes
                .iter()
                .position(|index| index.snapshot.issues.get(id) == Some(anchor))
            {
                Some(position) => {
                    used[position] = true;
                    picks.push((id.as_str(), &indexes[position]));
                }
                None => unmatched.push(id.clone()),
            }
        }
        let mut history = Self::empty_for_issues(BTreeMap::new());
        history.take_issues_from(&picks)?;
        Ok(PortableHistorySelection {
            history,
            unmatched,
            used,
        })
    }

    /// This snapshot (a database's evidence) where, for each of its issues,
    /// the first of `earlier` (newest first) that describes the same anchor
    /// and whose append-only evidence extends this snapshot's supplies the
    /// issue's rows instead. Evidence an earlier publication carried, which
    /// this database never replayed, is therefore kept, but only for the
    /// revision it describes. A stale or diverged history leaves this
    /// snapshot's own rows in place, and the issue set and anchors are always
    /// this snapshot's.
    pub fn carry_forward(&self, earlier: &[&Self]) -> Result<PortableHistoryCarry> {
        let canonical_own = canonical_snapshot(self)?;
        let own = SnapshotIndex::new(&canonical_own);
        let canonical_earlier = earlier
            .iter()
            .map(|source| canonical_snapshot(source))
            .collect::<Result<Vec<_>>>()?;
        let indexes: Vec<SnapshotIndex<'_>> = canonical_earlier
            .iter()
            .map(|source| SnapshotIndex::new(source))
            .collect();
        let mut used = vec![false; earlier.len()];
        let mut diverged = Vec::new();
        let mut picks = Vec::new();
        for (id, anchor) in &self.issues {
            let mut picked = false;
            let mut saw_divergence = false;
            for (position, index) in indexes.iter().enumerate() {
                if index.snapshot.issues.get(id) != Some(anchor) {
                    continue;
                }
                match index.lineage(&own, id) {
                    Lineage::Ahead => {
                        used[position] = true;
                        picks.push((id.as_str(), index));
                        picked = true;
                        break;
                    }
                    Lineage::Diverged => saw_divergence = true,
                    Lineage::Same | Lineage::Behind => {}
                }
            }
            if saw_divergence && !picked {
                diverged.push(id.clone());
            }
        }
        let mut history = canonical_own.into_owned();
        history.take_issues_from(&picks)?;
        Ok(PortableHistoryCarry {
            history,
            used,
            diverged,
        })
    }
}

impl SqliteStorage {
    /// Retain observed incompatible gate branches before a publication can
    /// replace the companion that contains them. This uses the same writer
    /// transaction as import quarantine and never applies incoming issue rows.
    /// With protected gates, compatible evidence is replayed too: a peer's
    /// later FAIL must reach enforcement before publication acknowledges it.
    /// Unprotected publication keeps its existing current-state behavior.
    ///
    /// # Errors
    /// Returns an error if evidence is malformed or its audit write fails.
    pub fn retain_portable_gate_conflicts(
        &self,
        incoming: &PortableHistorySnapshot,
    ) -> Result<Vec<String>> {
        let canonical = canonical_snapshot(incoming)?;
        let incoming = canonical.as_ref();
        let ids = incoming.issues.keys().cloned().collect::<HashSet<_>>();
        self.with_connection_write_transaction(|conn| {
            let protected = !self.workflow_policy().gate_policies.is_empty();
            if crate::storage::schema::has_pending_sync_merge_metadata(conn)? {
                let local = self.snapshot_portable_history(Some(&ids))?;
                return preflight_pending_history_observation(
                    &SnapshotIndex::new(incoming),
                    &SnapshotIndex::new(&local),
                    protected,
                );
            }
            if protected {
                return Ok(self.restore_portable_history_in_tx(incoming)?.diverged);
            }
            let local = self.snapshot_portable_history(Some(&ids))?;
            let local_index = SnapshotIndex::new(&local);
            let incoming_index = SnapshotIndex::new(incoming);
            let mut diverged = Vec::new();
            for (id, anchor) in &incoming.issues {
                if local.issues.get(id) == Some(anchor)
                    && incoming_index.lineage(&local_index, id) == Lineage::Diverged
                {
                    self.retain_gate_history_conflicts(id, &incoming_index, &local_index)?;
                    diverged.push(id.clone());
                }
            }
            Ok(diverged)
        })
    }

    /// Keep an incompatible branch as typed audit evidence. No imported PASS
    /// or override is made authoritative by choosing an arbitrary ordering of
    /// concurrent results. Protected gates inspect these events transactionally;
    /// unprotected gates retain their existing verdict projection.
    fn retain_gate_history_conflicts(
        &self,
        id: &str,
        incoming: &SnapshotIndex<'_>,
        local: &SnapshotIndex<'_>,
    ) -> Result<()> {
        for receipt in gate_history_conflicts_to_retain(id, incoming, local)? {
            self.execute_raw_with_params(
                "INSERT INTO events (issue_id, event_type, actor, comment, created_at) \
                 VALUES (?, ?, 'br:portable-history', ?, CURRENT_TIMESTAMP)",
                &[
                    SqliteValue::from(id),
                    SqliteValue::from(GATE_HISTORY_CONFLICT_EVENT),
                    SqliteValue::from(serde_json::to_string(&receipt)?),
                ],
            )?;
            self.set_metadata_in_tx("needs_flush", "true")?;
        }
        Ok(())
    }

    /// The evidence of every exportable issue, or of the exportable issues
    /// in `ids`, with per-issue sequence numbers and canonical anchors.
    pub fn snapshot_portable_history(
        &self,
        ids: Option<&HashSet<String>>,
    ) -> Result<PortableHistorySnapshot> {
        let mut issues = BTreeMap::new();
        for row in self.execute_raw_query(EXPORTABLE_ISSUES)? {
            let id = row
                .first()
                .and_then(SqliteValue::as_text)
                .ok_or_else(|| invalid("issue has no id"))?;
            if ids.is_some_and(|ids| !ids.contains(id)) {
                continue;
            }
            let status = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .ok_or_else(|| invalid("issue has no status"))?;
            let updated = row
                .get(2)
                .and_then(SqliteValue::as_text)
                .ok_or_else(|| invalid("issue has no updated_at"))?;
            let created = row
                .get(3)
                .and_then(SqliteValue::as_text)
                .ok_or_else(|| invalid("issue has no created_at"))?;
            let anchor = PortableHistoryIssue::canonical(status, created, updated)
                .ok_or_else(|| invalid(format!("invalid issue anchor {id}")))?;
            issues.insert(id.to_owned(), anchor);
        }
        let mut tables = Vec::new();
        for spec in SPECS {
            let columns = spec.names();
            let mut rows = Vec::new();
            for row in self.execute_raw_query(&format!(
                "SELECT {} FROM {} ORDER BY {}",
                columns.join(", "),
                spec.name,
                if spec.append {
                    "id"
                } else {
                    spec.columns[spec.key[0]].0
                }
            ))? {
                let id = row
                    .get(spec.owner())
                    .and_then(SqliteValue::as_text)
                    .ok_or_else(|| invalid("history row has no issue id"))?;
                if issues.contains_key(id) {
                    rows.push(row.iter().map(from_sql).collect::<Result<Vec<_>>>()?);
                }
            }
            tables.push(PortableHistoryTable {
                table: spec.name.to_owned(),
                columns,
                rows,
            });
        }
        let mut snapshot = PortableHistorySnapshot { issues, tables };
        normalize(&mut snapshot)?;
        Ok(snapshot)
    }

    pub fn all_portable_history(&self) -> Result<PortableHistorySnapshot> {
        self.snapshot_portable_history(None)
    }

    /// Replay `incoming` evidence, issue by issue, in one write transaction.
    ///
    /// Evidence is applied only to a database row at the anchor it describes,
    /// and only when it extends (or equals) the database's own append-only
    /// history: missing rows are appended with gate revisions remapped to the
    /// local event ids, and the current-state rows are replaced. A row at
    /// another revision is left alone. A diverged history retains local rows
    /// and records incompatible incoming gate evidence as typed conflict
    /// events, so acknowledging a remote failure cannot silently retain an
    /// effective local PASS. This does not provide consensus for unseen peers.
    pub fn restore_portable_history(
        &mut self,
        incoming: &PortableHistorySnapshot,
    ) -> Result<PortableHistoryReplay> {
        let canonical = canonical_snapshot(incoming)?;
        self.with_write_transaction(|storage| {
            storage.restore_portable_history_in_tx(canonical.as_ref())
        })
    }

    fn restore_portable_history_in_tx(
        &self,
        incoming: &PortableHistorySnapshot,
    ) -> Result<PortableHistoryReplay> {
        let storage = self;
        let ids = incoming.issues.keys().cloned().collect::<HashSet<_>>();
        let local = storage.snapshot_portable_history(Some(&ids))?;
        let local_index = SnapshotIndex::new(&local);
        let incoming_index = SnapshotIndex::new(incoming);
        let mut report = PortableHistoryReplay::default();
        let mut eligible = HashSet::new();
        for (id, anchor) in &incoming.issues {
            if local.issues.get(id) != Some(anchor) {
                report.other_revision.push(id.clone());
                continue;
            }
            match incoming_index.lineage(&local_index, id) {
                Lineage::Diverged => {
                    storage.retain_gate_history_conflicts(id, &incoming_index, &local_index)?;
                    report.diverged.push(id.clone());
                }
                // The database already holds everything and more; its
                // current-state rows are the newer ones.
                Lineage::Behind => report.replayed.push(id.clone()),
                Lineage::Same | Lineage::Ahead => {
                    eligible.insert(id.as_str());
                    report.replayed.push(id.clone());
                }
            }
        }
        if eligible.is_empty() {
            return Ok(report);
        }

        // Per-issue sequence -> local event id, numbered exactly as
        // `snapshot_portable_history` numbers them.
        let mut event_ids = HashMap::<(String, i64), i64>::new();
        let mut sequences = HashMap::<String, i64>::new();
        for row in storage.execute_raw_query("SELECT id, issue_id FROM events ORDER BY id")? {
            let (Some(event_id), Some(issue)) = (
                row.first().and_then(SqliteValue::as_integer),
                row.get(1).and_then(SqliteValue::as_text),
            ) else {
                continue;
            };
            if !eligible.contains(issue) {
                continue;
            }
            let seq = sequences.entry(issue.to_owned()).or_default();
            *seq += 1;
            event_ids.insert((issue.to_owned(), *seq), event_id);
        }

        for (index, spec) in SPECS.iter().enumerate() {
            for row in &incoming.tables[index].rows {
                let issue = owner(spec, row);
                if !eligible.contains(issue) {
                    continue;
                }
                if !spec.append {
                    insert(storage, spec, row, false)?;
                    continue;
                }
                let seq = row[0]
                    .as_i64()
                    .ok_or_else(|| invalid("non-integer history id"))?;
                let present = local_index.rows(index, issue).len();
                if usize::try_from(seq).is_ok_and(|seq| seq <= present) {
                    continue;
                }
                let mut values = row[1..].to_vec();
                if spec.name == "gate_result_history" {
                    let revision = row[4].as_i64().unwrap_or_default();
                    if revision != 0 {
                        let event_id = event_ids
                            .get(&(issue.to_owned(), revision))
                            .ok_or_else(|| invalid("gate revision mapping is absent"))?;
                        values[3] = Value::from(*event_id);
                    }
                }
                insert(storage, spec, &values, true)?;
                if spec.name == "events" {
                    let event_id = storage
                        .execute_raw_query("SELECT last_insert_rowid()")?
                        .first()
                        .and_then(|row| row.first())
                        .and_then(SqliteValue::as_integer)
                        .ok_or_else(|| invalid("event insert returned no id"))?;
                    event_ids.insert((issue.to_owned(), seq), event_id);
                }
            }
        }
        Ok(report)
    }
}

/// Insert one history row with bound parameters. Append-only rows omit their
/// portable sequence so the database assigns a local id; current-state rows
/// replace the row with the same key.
fn insert(storage: &SqliteStorage, spec: &Spec, row: &[Value], omit_id: bool) -> Result<()> {
    let columns = &spec.columns[usize::from(omit_id)..];
    let names = columns
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = vec!["?"; columns.len()].join(", ");
    let params = row.iter().map(to_sql).collect::<Result<Vec<_>>>()?;
    if spec.name == "close_metadata" && !omit_id {
        // The primary issue import has already installed any durable closure
        // attribution. REPLACE would clear `closed_by`, which old companion
        // records do not contain, and could erase captured agent fields with
        // historical NULLs. Retain known primary values; a companion can fill
        // absent agent fields from its recorded metadata but never invent an
        // actor from an event. All other audit columns retain replay semantics.
        storage.execute_raw_with_params(
            &format!("INSERT OR IGNORE INTO close_metadata ({names}) VALUES ({placeholders})"),
            &params,
        )?;
        let assignments = columns[1..]
            .iter()
            .map(|(name, _)| match *name {
                "closed_by_agent_name" | "closed_by_harness" | "closed_by_model" => format!(
                    "{name} = CASE WHEN {name} IS NULL OR TRIM({name}) = '' THEN ? ELSE {name} END"
                ),
                _ => format!("{name} = ?"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut update_params = params[1..].to_vec();
        update_params.push(params[0].clone());
        storage.execute_raw_with_params(
            &format!("UPDATE close_metadata SET {assignments} WHERE issue_id = ?"),
            &update_params,
        )?;
        return Ok(());
    }
    let verb = if omit_id {
        "INSERT"
    } else {
        "INSERT OR REPLACE"
    };
    storage.execute_raw_with_params(
        &format!(
            "{verb} INTO {} ({names}) VALUES ({placeholders})",
            spec.name
        ),
        &params,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, Status};
    use crate::storage::{EventAttribution, IssueUpdate};

    fn gate_source() -> (SqliteStorage, crate::close_policy::Workflow) {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let workflow: crate::close_policy::Workflow = serde_yml::from_str(
            "strict: true\ngates:\n  'in_progress -> closed':\n    require_all: [validation]\ngate_policies:\n  validation:\n    allowed_reporters: [verifier, lead]\n    forbid_self_approval: true\n    override_reporters: [lead]\n",
        ).unwrap();
        storage.set_workflow_policy(workflow.clone());
        let issue = Issue {
            id: "bd-portable-gate".to_owned(),
            title: "Portable independent verification".to_owned(),
            assignee: Some("worker".to_owned()),
            ..Issue::default()
        };
        storage.create_issue(&issue, "worker").unwrap();
        storage
            .update_issue(
                &issue.id,
                &IssueUpdate {
                    status: Some(Status::InProgress),
                    ..IssueUpdate::default()
                },
                "worker",
            )
            .unwrap();
        let revision = storage.status_revision(&issue.id).unwrap();
        let failed = storage
            .record_scoped_gate_result(
                &issue.id,
                "in_progress",
                revision,
                "closed",
                "validation",
                "verifier",
                false,
                Some("original failure"),
                "verifier",
                None,
            )
            .unwrap();
        storage
            .record_scoped_gate_result(
                &issue.id,
                "in_progress",
                revision,
                "closed",
                "validation",
                "lead",
                true,
                Some("independent override evidence"),
                "lead",
                Some(&crate::close_policy::GateOverride {
                    expected_history_id: failed.id,
                    reason: "failure reviewed and explicitly accepted".to_owned(),
                }),
            )
            .unwrap();
        (storage, workflow)
    }

    const GATE_ISSUE: &str = "bd-portable-gate";

    fn gate_transition(storage: &mut SqliteStorage, status: Status) -> Result<Issue> {
        storage.update_issue(
            GATE_ISSUE,
            &IssueUpdate {
                status: Some(status),
                ..IssueUpdate::default()
            },
            "worker",
        )
    }

    fn forkable_gate_source() -> (SqliteStorage, crate::close_policy::Workflow) {
        let (mut storage, workflow) = gate_source();
        gate_transition(&mut storage, Status::Open).unwrap();
        gate_transition(&mut storage, Status::InProgress).unwrap();
        (storage, workflow)
    }

    fn fork_gate_store(
        source: &SqliteStorage,
        workflow: &crate::close_policy::Workflow,
    ) -> SqliteStorage {
        let mut storage = SqliteStorage::open_memory().unwrap();
        storage
            .upsert_issue_for_import(&source.get_issue(GATE_ISSUE).unwrap().unwrap())
            .unwrap();
        storage
            .restore_portable_history(&source.all_portable_history().unwrap())
            .unwrap();
        storage.set_workflow_policy(workflow.clone());
        storage
    }

    fn fork_gate_report(
        storage: &SqliteStorage,
        actor: &str,
        passed: bool,
    ) -> Result<crate::close_policy::GateResultRecord> {
        storage.record_scoped_gate_result(
            GATE_ISSUE,
            "in_progress",
            storage.status_revision(GATE_ISSUE)?,
            "closed",
            "validation",
            actor,
            passed,
            Some("independent branch verdict"),
            actor,
            None,
        )
    }

    fn commit_pending_gate_merge(
        storage: &mut SqliteStorage,
    ) -> crate::sync::SyncMergePendingReceipt {
        let intent = crate::sync::SyncMergeIntent {
            schema_version: 2,
            database_authority_sha256: "11".repeat(32),
            jsonl_authority_sha256: "22".repeat(32),
            jsonl_path_sha256: "33".repeat(32),
            jsonl_before: crate::sync::JsonlSourceStateWitness::Missing,
            jsonl_before_content_sha256: None,
            base_authority_sha256: "44".repeat(32),
            base_before: crate::sync::JsonlSourceStateWitness::Missing,
            base_before_content_sha256: None,
            resolution: "manual".to_owned(),
            actor: "merge-agent".to_owned(),
            event_attribution: storage.pending_event_attribution_for_review(),
            capacity_policy: storage.workflow_capacity_policy_for_review(),
            retention_days: None,
            export_as_of: chrono::DateTime::parse_from_rfc3339("2026-11-12T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            changed_kept_issue_ids: Vec::new(),
            kept_issue_witnesses: Vec::new(),
            deleted_issue_ids: Vec::new(),
            note_witnesses: Vec::new(),
            database_before: crate::sync::capture_sync_database_witness(storage).unwrap(),
        };
        storage
            .apply_sync_merge_atomically(&[], &[], &[], &intent)
            .unwrap()
    }

    #[test]
    fn portable_gate_divergence_blocks_pass_and_preserves_both_branches() {
        for local_passed in [true, false] {
            let (remote, workflow) = forkable_gate_source();
            let mut local = fork_gate_store(&remote, &workflow);
            fork_gate_report(&remote, "verifier", !local_passed).unwrap();
            fork_gate_report(&local, "lead", local_passed).unwrap();
            let incoming = remote.all_portable_history().unwrap();
            let original_local = local.get_gate_result_history(GATE_ISSUE).unwrap();
            let revision = local.status_revision(GATE_ISSUE).unwrap();

            // JSONL-only startup installs policy after replay. Capturing the
            // evidence must not depend on the policy already being loaded.
            local.set_workflow_policy(crate::close_policy::Workflow::default());
            let replay = local.restore_portable_history(&incoming).unwrap();
            local.set_workflow_policy(workflow);
            assert_eq!(replay.diverged, [GATE_ISSUE]);
            assert_eq!(
                local.get_gate_result_history(GATE_ISSUE).unwrap(),
                original_local,
                "neither local results nor imported overrides are rewritten"
            );
            assert_eq!(local.status_revision(GATE_ISSUE).unwrap(), revision);
            let conflicts = local.get_gate_history_conflicts(GATE_ISSUE).unwrap();
            assert_eq!(conflicts.len(), 1);
            assert!(conflicts[0].affects("IN_PROGRESS", "closed", "validation"));
            assert!(!conflicts[0].affects("in_progress", "open", "validation"));
            assert_eq!(
                conflicts[0].conflict.evidence.gate_history,
                incoming.tables[1]
            );
            assert!(!conflicts[0].conflict.evidence.status_events.rows.is_empty());
            assert_eq!(
                local.get_metadata("needs_flush").unwrap().as_deref(),
                Some("true")
            );

            let error = fork_gate_report(&local, "lead", true).unwrap_err();
            assert!(error.to_string().contains("divergent verification history"));
            let error = gate_transition(&mut local, Status::Closed).unwrap_err();
            assert!(error.to_string().contains("divergent verification history"));
            assert_eq!(
                local.get_gate_history_conflicts(GATE_ISSUE).unwrap(),
                conflicts
            );
            assert_eq!(
                local.get_gate_result_history(GATE_ISSUE).unwrap(),
                original_local
            );
        }
    }

    #[test]
    fn portable_gate_divergence_deduplicates_old_evidence_after_real_rework() {
        let (remote, workflow) = forkable_gate_source();
        let mut local = fork_gate_store(&remote, &workflow);
        fork_gate_report(&remote, "verifier", false).unwrap();
        fork_gate_report(&local, "lead", true).unwrap();
        let incoming = remote.all_portable_history().unwrap();
        local.restore_portable_history(&incoming).unwrap();
        let original_conflict = local.get_gate_history_conflicts(GATE_ISSUE).unwrap();
        local.restore_portable_history(&incoming).unwrap();
        local.retain_portable_gate_conflicts(&incoming).unwrap();
        assert_eq!(
            local.get_gate_history_conflicts(GATE_ISSUE).unwrap(),
            original_conflict
        );

        gate_transition(&mut local, Status::Open).unwrap();
        let reworked = gate_transition(&mut local, Status::InProgress).unwrap();
        // A peer can receive the reworked issue row without reconciling its
        // old branch. Rebinding the same verdicts to that mutable anchor is
        // not new evidence and must not reactivate the acknowledged conflict.
        remote.upsert_issue_for_import(&reworked).unwrap();
        let rebound = remote.all_portable_history().unwrap();
        local.restore_portable_history(&rebound).unwrap();
        let historical = local.get_gate_history_conflicts(GATE_ISSUE).unwrap();
        assert_eq!(historical.len(), 1);
        assert!(!historical[0].active);
        assert_eq!(historical[0].conflict, original_conflict[0].conflict);
        fork_gate_report(&local, "verifier", true).unwrap();
        gate_transition(&mut local, Status::Closed).unwrap();
        assert_eq!(
            local.get_gate_history_conflicts(GATE_ISSUE).unwrap().len(),
            1
        );
    }

    #[test]
    fn portable_gate_divergence_does_not_authorize_a_remote_override() {
        let (common, workflow) = forkable_gate_source();
        let failed = fork_gate_report(&common, "verifier", false).unwrap();
        let remote = fork_gate_store(&common, &workflow);
        let mut local = fork_gate_store(&common, &workflow);
        let remote_failure = remote
            .get_gate_result_history(GATE_ISSUE)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(remote_failure.note, failed.note);
        remote
            .record_scoped_gate_result(
                GATE_ISSUE,
                "in_progress",
                remote.status_revision(GATE_ISSUE).unwrap(),
                "closed",
                "validation",
                "lead",
                true,
                None,
                "lead",
                Some(&crate::close_policy::GateOverride {
                    expected_history_id: remote_failure.id,
                    reason: "reviewed the shared failure only".to_owned(),
                }),
            )
            .unwrap();
        let later_failure = fork_gate_report(&local, "verifier", false).unwrap();
        let local_before = local.get_gate_result_history(GATE_ISSUE).unwrap();
        local
            .restore_portable_history(&remote.all_portable_history().unwrap())
            .unwrap();
        let conflicts = local.get_gate_history_conflicts(GATE_ISSUE).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts[0]
                .conflict
                .evidence
                .gate_history
                .rows
                .last()
                .unwrap()[12],
            "reviewed the shared failure only"
        );
        assert_eq!(
            local.get_gate_result_history(GATE_ISSUE).unwrap(),
            local_before
        );
        let error = local
            .record_scoped_gate_result(
                GATE_ISSUE,
                "in_progress",
                local.status_revision(GATE_ISSUE).unwrap(),
                "closed",
                "validation",
                "lead",
                true,
                None,
                "lead",
                Some(&crate::close_policy::GateOverride {
                    expected_history_id: later_failure.id,
                    reason: "ordinary override cannot resolve an ambiguous fork".to_owned(),
                }),
            )
            .unwrap_err();
        assert!(error.to_string().contains("divergent verification history"));
    }

    #[test]
    fn portable_gate_conflict_roundtrip_keeps_active_and_resolved_evidence_distinct() {
        let (remote, workflow) = forkable_gate_source();
        let mut local = fork_gate_store(&remote, &workflow);
        fork_gate_report(&remote, "verifier", false).unwrap();
        fork_gate_report(&local, "lead", true).unwrap();
        let mut diverged = fork_gate_store(&local, &workflow);
        diverged
            .update_issue(
                GATE_ISSUE,
                &IssueUpdate {
                    title: Some("independent local edit".to_owned()),
                    ..IssueUpdate::default()
                },
                "worker",
            )
            .unwrap();
        local
            .restore_portable_history(&remote.all_portable_history().unwrap())
            .unwrap();
        let source_conflict = local
            .get_gate_history_conflicts(GATE_ISSUE)
            .unwrap()
            .remove(0);
        let mut third = fork_gate_store(&local, &workflow);
        let copied = third.get_gate_history_conflicts(GATE_ISSUE).unwrap();
        assert_eq!(copied.len(), 1);
        assert!(copied[0].active);
        assert_eq!(copied[0].conflict, source_conflict.conflict);
        assert!(fork_gate_report(&third, "lead", true).is_err());

        gate_transition(&mut third, Status::Open).unwrap();
        let reworked = gate_transition(&mut third, Status::InProgress).unwrap();
        let resolved = third.all_portable_history().unwrap();
        diverged.upsert_issue_for_import(&reworked).unwrap();
        let replay = diverged.restore_portable_history(&resolved).unwrap();
        assert_eq!(replay.diverged, [GATE_ISSUE]);
        let archived = diverged.get_gate_history_conflicts(GATE_ISSUE).unwrap();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].id > diverged.status_revision(GATE_ISSUE).unwrap());
        assert!(
            !archived[0].active,
            "new local event order must not reactivate resolved peer evidence"
        );
        assert!(!archived[0].conflict.blocking);
        assert_eq!(
            archived[0].conflict.source_digest,
            source_conflict.conflict.source_digest
        );
        assert_eq!(
            archived[0].conflict.evidence,
            source_conflict.conflict.evidence
        );
        diverged.restore_portable_history(&resolved).unwrap();
        assert_eq!(
            diverged.get_gate_history_conflicts(GATE_ISSUE).unwrap(),
            archived
        );
        validate(&diverged.all_portable_history().unwrap()).unwrap();
    }

    #[test]
    fn portable_gate_divergence_does_not_change_unprotected_gate_evaluation() {
        let (remote, mut workflow) = forkable_gate_source();
        let mut local = fork_gate_store(&remote, &workflow);
        fork_gate_report(&remote, "verifier", false).unwrap();
        fork_gate_report(&local, "lead", true).unwrap();
        workflow.gate_policies.clear();
        local.set_workflow_policy(workflow);
        let local_before = local.get_gate_result_history(GATE_ISSUE).unwrap();
        local
            .restore_portable_history(&remote.all_portable_history().unwrap())
            .unwrap();
        assert_eq!(
            local.get_gate_result_history(GATE_ISSUE).unwrap(),
            local_before
        );
        assert!(local.get_gate_history_conflicts(GATE_ISSUE).unwrap()[0].active);
        gate_transition(&mut local, Status::Closed).unwrap();
    }

    #[test]
    fn portable_gate_publication_observation_replays_compatible_failure() {
        for protected in [true, false] {
            let (mut local, mut workflow) = forkable_gate_source();
            fork_gate_report(&local, "lead", true).unwrap();
            let remote = fork_gate_store(&local, &workflow);
            fork_gate_report(&remote, "verifier", false).unwrap();
            let incoming = remote.all_portable_history().unwrap();
            let before = local.all_portable_history().unwrap();
            if !protected {
                workflow.gate_policies.clear();
                local.set_workflow_policy(workflow);
            }
            assert!(
                local
                    .retain_portable_gate_conflicts(&incoming)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                local
                    .get_gate_history_conflicts(GATE_ISSUE)
                    .unwrap()
                    .is_empty()
            );
            if protected {
                let history = local.get_gate_result_history(GATE_ISSUE).unwrap();
                let failed = history.last().unwrap();
                assert!(!failed.passed);
                assert_eq!(failed.recorded_by.as_deref(), Some("verifier"));
                assert_eq!(
                    failed.status_revision,
                    local.status_revision(GATE_ISSUE).unwrap()
                );
                let error = gate_transition(&mut local, Status::Closed).unwrap_err();
                assert!(error.to_string().contains("failed verification"));
                // This branch is causally compatible, so the existing explicit
                // override remains available after inspecting the real FAIL.
                local
                    .record_scoped_gate_result(
                        GATE_ISSUE,
                        "in_progress",
                        failed.status_revision,
                        "closed",
                        "validation",
                        "lead",
                        true,
                        None,
                        "lead",
                        Some(&crate::close_policy::GateOverride {
                            expected_history_id: failed.id,
                            reason: "reviewed the subsequently imported failure".to_owned(),
                        }),
                    )
                    .unwrap();
            } else {
                assert_eq!(local.all_portable_history().unwrap(), before);
            }
            gate_transition(&mut local, Status::Closed).unwrap();
        }
    }

    #[test]
    fn pending_merge_history_observation_preserves_witness_or_refuses_new_evidence() {
        for compatible in [true, false] {
            let (mut local, workflow) = forkable_gate_source();
            let remote = if compatible {
                fork_gate_report(&local, "lead", true).unwrap();
                fork_gate_store(&local, &workflow)
            } else {
                let remote = fork_gate_store(&local, &workflow);
                fork_gate_report(&local, "lead", true).unwrap();
                remote
            };
            fork_gate_report(&remote, "verifier", false).unwrap();
            let incoming = remote.all_portable_history().unwrap();
            let known = local.all_portable_history().unwrap();
            let receipt = commit_pending_gate_merge(&mut local);
            let before = crate::sync::capture_sync_database_witness(&local).unwrap();
            let mirror_before = local
                .execute_raw_query("SELECT rowid, * FROM gate_results ORDER BY rowid")
                .unwrap();
            assert!(
                local
                    .retain_portable_gate_conflicts(&known)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                crate::sync::capture_sync_database_witness(&local).unwrap(),
                before
            );
            assert_eq!(
                local
                    .execute_raw_query("SELECT rowid, * FROM gate_results ORDER BY rowid")
                    .unwrap(),
                mirror_before
            );

            let error = local.retain_portable_gate_conflicts(&incoming).unwrap_err();
            assert!(matches!(error, BeadsError::SyncConflict { .. }));
            assert!(error.to_string().contains("Pending sync merge owns"));
            assert_eq!(
                crate::sync::capture_sync_database_witness(&local).unwrap(),
                before
            );
            assert_eq!(local.pending_sync_merge_receipt().unwrap(), Some(receipt));
            assert!(
                local
                    .get_gate_history_conflicts(GATE_ISSUE)
                    .unwrap()
                    .is_empty()
            );

            // Equal append-only rows do not authorize replacing different
            // current-state rows while the pending receipt freezes them.
            let mut changed_projection = known.clone();
            changed_projection.tables[3].rows[0][3] = Value::from(1);
            if changed_projection.tables[3] == known.tables[3] {
                changed_projection.tables[3].rows[0][3] = Value::from(0);
            }
            assert!(
                local
                    .retain_portable_gate_conflicts(&changed_projection)
                    .is_err()
            );
            assert_eq!(
                crate::sync::capture_sync_database_witness(&local).unwrap(),
                before
            );
        }
    }

    #[test]
    fn pending_merge_history_observation_deduplicates_known_conflicts_and_guards_opaque_markers() {
        for marker in [
            crate::sync::METADATA_SYNC_MERGE_PENDING,
            crate::sync::METADATA_SYNC_MERGE_PENDING_LEGACY,
        ] {
            let (remote, workflow) = forkable_gate_source();
            let mut local = fork_gate_store(&remote, &workflow);
            fork_gate_report(&remote, "verifier", false).unwrap();
            fork_gate_report(&local, "lead", true).unwrap();
            let incoming = remote.all_portable_history().unwrap();
            local
                .set_metadata(marker, "opaque pending authority")
                .unwrap();
            let before = crate::sync::capture_sync_database_witness(&local).unwrap();
            assert!(local.retain_portable_gate_conflicts(&incoming).is_err());
            assert_eq!(
                crate::sync::capture_sync_database_witness(&local).unwrap(),
                before
            );
            assert_eq!(
                local.get_metadata(marker).unwrap().as_deref(),
                Some("opaque pending authority")
            );

            // A second database captures the conflict before the merge. The
            // same already-retained evidence must remain a no-op while pending.
            let mut known_local = fork_gate_store(&local, &workflow);
            known_local.restore_portable_history(&incoming).unwrap();
            assert!(
                !known_local
                    .get_gate_history_conflicts(GATE_ISSUE)
                    .unwrap()
                    .is_empty()
            );
            known_local
                .set_metadata(marker, "opaque pending authority")
                .unwrap();
            let before = crate::sync::capture_sync_database_witness(&known_local).unwrap();
            known_local
                .retain_portable_gate_conflicts(&incoming)
                .unwrap();
            assert_eq!(
                crate::sync::capture_sync_database_witness(&known_local).unwrap(),
                before
            );
        }
    }

    #[test]
    fn portable_gate_evidence_preserves_independence_and_override_across_id_remapping() {
        let (source, workflow) = gate_source();
        let issue = source.get_issue("bd-portable-gate").unwrap().unwrap();
        let original = source.get_gate_result_history(&issue.id).unwrap();
        let snapshot = source.all_portable_history().unwrap();
        let mut target = SqliteStorage::open_memory().unwrap();
        target
            .create_issue(
                &Issue {
                    id: "bd-existing".to_owned(),
                    title: "Shift local event sequence".to_owned(),
                    ..Issue::default()
                },
                "other",
            )
            .unwrap();
        target.upsert_issue_for_import(&issue).unwrap();
        target.set_workflow_policy(workflow.clone());
        let replay = target.restore_portable_history(&snapshot).unwrap();
        assert_eq!(replay.replayed, [issue.id.clone()]);
        let restored = target
            .get_scoped_gate_result_history(&issue.id, "in_progress", "closed")
            .unwrap();
        assert_eq!(restored.len(), 2);
        assert_ne!(restored[0].status_revision, original[0].status_revision);
        assert_eq!(
            restored[0].status_revision,
            target.status_revision(&issue.id).unwrap()
        );
        for (restored, original) in restored.iter().zip(&original) {
            assert_eq!(restored.recorded_by, original.recorded_by);
            assert_eq!(restored.assignee_at_report, original.assignee_at_report);
            assert_eq!(restored.override_reason, original.override_reason);
            assert_eq!(restored.note, original.note);
            assert_eq!(restored.passed, original.passed);
        }
        let state = crate::close_policy::protected_gate_state(
            workflow.gate_policy("validation").unwrap(),
            "validation",
            &restored,
            &["worker"],
        );
        assert!(!state.unresolved_failure);
        assert_eq!(state.passing_reporters, ["lead"]);
        target
            .update_issue(
                &issue.id,
                &IssueUpdate {
                    status: Some(Status::Closed),
                    ..IssueUpdate::default()
                },
                "worker",
            )
            .unwrap();
    }

    #[test]
    fn legacy_portable_gate_columns_remain_unknown_and_do_not_invent_override_authority() {
        let (source, workflow) = gate_source();
        let issue = source.get_issue("bd-portable-gate").unwrap().unwrap();
        let mut legacy = source.all_portable_history().unwrap();
        let table = legacy
            .tables
            .iter_mut()
            .find(|table| table.table == "gate_result_history")
            .unwrap();
        table.columns.truncate(11);
        for row in &mut table.rows {
            row.truncate(11);
        }
        validate(&legacy).unwrap();
        let original_bytes = serde_json::to_vec(&legacy).unwrap();
        let mut target = SqliteStorage::open_memory().unwrap();
        target.upsert_issue_for_import(&issue).unwrap();
        target.set_workflow_policy(workflow.clone());
        target.restore_portable_history(&legacy).unwrap();
        let restored = target
            .get_scoped_gate_result_history(&issue.id, "in_progress", "closed")
            .unwrap();
        assert_eq!(restored.len(), 2);
        assert!(
            restored
                .iter()
                .all(|record| record.assignee_at_report.is_none()
                    && record.override_reason.is_none())
        );
        let state = crate::close_policy::protected_gate_state(
            workflow.gate_policy("validation").unwrap(),
            "validation",
            &restored,
            &["worker"],
        );
        assert!(state.unresolved_failure);
        assert!(state.passing_reporters.is_empty());
        assert!(
            target
                .update_issue(
                    &issue.id,
                    &IssueUpdate {
                        status: Some(Status::Closed),
                        ..IssueUpdate::default()
                    },
                    "worker"
                )
                .is_err()
        );
        let current = target.all_portable_history().unwrap();
        let selected = PortableHistorySnapshot::select(&[&legacy], &current.issues).unwrap();
        assert_eq!(selected.history, current);
        assert!(
            current
                .carry_forward(&[&legacy])
                .unwrap()
                .diverged
                .is_empty()
        );
        assert_eq!(
            serde_json::to_vec(&legacy).unwrap(),
            original_bytes,
            "compatibility normalization must not change the bytes covered by an old checkpoint checksum"
        );
        let mut malformed = current;
        malformed.tables[1].columns.truncate(12);
        assert!(
            validate(&malformed).is_err(),
            "only the exact released and current shapes are supported"
        );
    }

    fn closure_source() -> SqliteStorage {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = Issue {
            id: "bd-portable-closure".to_string(),
            title: "Portable closure attribution".to_string(),
            ..Issue::default()
        };
        storage.create_issue(&issue, "creator").unwrap();
        storage.set_pending_event_attribution(EventAttribution::new(
            Some("primary-seat"),
            Some("primary-harness"),
            Some("primary-model"),
            None,
        ));
        storage
            .update_issue(
                &issue.id,
                &IssueUpdate {
                    status: Some(Status::Closed),
                    close_reason: Some(Some("verified".to_string())),
                    ..IssueUpdate::default()
                },
                "primary-closer",
            )
            .unwrap();
        storage
    }

    #[test]
    fn portable_history_preserves_primary_closure_attribution() {
        let source = closure_source();
        let issue = source.get_issue("bd-portable-closure").unwrap().unwrap();
        for companion_value in [Value::Null, Value::String("older-companion".to_string())] {
            let mut history = source.all_portable_history().unwrap();
            let close_table = history
                .tables
                .iter_mut()
                .find(|table| table.table == "close_metadata")
                .unwrap();
            assert!(
                !close_table
                    .columns
                    .iter()
                    .any(|column| column == "closed_by")
            );
            for value in &mut close_table.rows[0][1..=3] {
                *value = companion_value.clone();
            }
            validate(&history).unwrap();

            let mut target = SqliteStorage::open_memory().unwrap();
            target.upsert_issue_for_import(&issue).unwrap();
            for _ in 0..2 {
                let replay = target.restore_portable_history(&history).unwrap();
                assert_eq!(replay.replayed, vec![issue.id.clone()]);
                let restored = target.get_issue(&issue.id).unwrap().unwrap();
                assert_eq!(restored.closed_by.as_deref(), Some("primary-closer"));
                assert_eq!(
                    restored.closed_by_agent_name.as_deref(),
                    Some("primary-seat")
                );
                assert_eq!(
                    restored.closed_by_harness.as_deref(),
                    Some("primary-harness")
                );
                assert_eq!(restored.closed_by_model.as_deref(), Some("primary-model"));
            }
        }
    }

    #[test]
    fn portable_history_does_not_infer_legacy_closer_from_events() {
        let source = closure_source();
        let mut legacy_issue = source.get_issue("bd-portable-closure").unwrap().unwrap();
        legacy_issue.closed_by = None;
        legacy_issue.closed_by_agent_name = None;
        legacy_issue.closed_by_harness = None;
        legacy_issue.closed_by_model = None;

        for captured_agent in [false, true] {
            let mut history = source.all_portable_history().unwrap();
            if !captured_agent {
                let close_table = history
                    .tables
                    .iter_mut()
                    .find(|table| table.table == "close_metadata")
                    .unwrap();
                close_table.rows[0][1..=3].fill(Value::Null);
            }
            let mut target = SqliteStorage::open_memory().unwrap();
            target.upsert_issue_for_import(&legacy_issue).unwrap();
            assert!(
                target
                    .get_close_metadata(&legacy_issue.id)
                    .unwrap()
                    .is_none()
            );
            target.restore_portable_history(&history).unwrap();
            let restored = target.get_issue(&legacy_issue.id).unwrap().unwrap();
            assert!(
                restored.closed_by.is_none(),
                "event actors are not inferred closers"
            );
            assert_eq!(
                restored.closed_by_agent_name.as_deref(),
                captured_agent.then_some("primary-seat")
            );
            assert_eq!(
                restored.closed_by_harness.as_deref(),
                captured_agent.then_some("primary-harness")
            );
            assert_eq!(
                restored.closed_by_model.as_deref(),
                captured_agent.then_some("primary-model")
            );
            assert!(
                target
                    .get_events(&legacy_issue.id, 0)
                    .unwrap()
                    .iter()
                    .any(|event| event.actor == "primary-closer")
            );
        }
    }
}
