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
use std::collections::{BTreeMap, HashMap, HashSet};

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
        if table.table != spec.name || table.columns != spec.names() {
            return Err(invalid(format!(
                "noncanonical history table {}",
                table.table
            )));
        }
        let mut sequences = HashMap::<&str, i64>::new();
        let mut previous: Option<Vec<String>> = None;
        for row in &table.rows {
            if row.len() != spec.columns.len() {
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
        let indexes: Vec<SnapshotIndex<'_>> = sources
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
        let own = SnapshotIndex::new(self);
        let indexes: Vec<SnapshotIndex<'_>> = earlier
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
        let mut history = self.clone();
        history.take_issues_from(&picks)?;
        Ok(PortableHistoryCarry {
            history,
            used,
            diverged,
        })
    }
}

impl SqliteStorage {
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
    /// another revision, and a diverged history, are left alone and reported.
    pub fn restore_portable_history(
        &mut self,
        incoming: &PortableHistorySnapshot,
    ) -> Result<PortableHistoryReplay> {
        validate(incoming)?;
        let ids = incoming.issues.keys().cloned().collect::<HashSet<_>>();
        self.with_write_transaction(|storage| {
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
                    Lineage::Diverged => report.diverged.push(id.clone()),
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
        })
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
