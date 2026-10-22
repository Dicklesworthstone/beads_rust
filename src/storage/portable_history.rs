//! Portable workflow audit evidence.
//!
//! The repository format uses per-issue sequence numbers instead of SQLite row
//! ids, so a fresh clone can replay events and gate revisions deterministically.

use super::SqliteStorage;
use crate::error::{BeadsError, Result};
use crate::franken_sync::SqliteValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortableHistoryIssue {
    pub(crate) status: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortableHistoryTable {
    pub(crate) table: String,
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortableHistorySnapshot {
    pub(crate) issues: BTreeMap<String, PortableHistoryIssue>,
    pub(crate) tables: Vec<PortableHistoryTable>,
}

#[derive(Clone, Copy)]
enum Kind { Text, OptionalText, Integer, Boolean, Timestamp, OptionalTimestamp }

struct Spec {
    name: &'static str,
    columns: &'static [(&'static str, Kind)],
    key: &'static [usize],
    append: bool,
}
impl Spec {
    fn owner(&self) -> usize { usize::from(self.append) }
    fn names(&self) -> Vec<String> {
        self.columns.iter().map(|(name, _)| (*name).to_owned()).collect()
    }
    fn key(&self, row: &[Value]) -> Vec<String> {
        self.key.iter().map(|&i| row[i].as_str().unwrap_or_default().to_owned()).collect()
    }
}

const SPECS: &[Spec] = &[
    Spec { name:"events", columns:&[
        ("id",Kind::Integer),("issue_id",Kind::Text),("event_type",Kind::Text),
        ("actor",Kind::Text),("old_value",Kind::OptionalText),("new_value",Kind::OptionalText),
        ("comment",Kind::OptionalText),("created_at",Kind::Timestamp),
        ("agent_name",Kind::OptionalText),("harness",Kind::OptionalText),("model",Kind::OptionalText),
    ], key:&[1], append:true },
    Spec { name:"gate_result_history", columns:&[
        ("id",Kind::Integer),("issue_id",Kind::Text),("from_status",Kind::Text),
        ("to_status",Kind::Text),("status_revision",Kind::Integer),("gate",Kind::Text),
        ("provider",Kind::Text),("passed",Kind::Boolean),("note",Kind::OptionalText),
        ("recorded_by",Kind::OptionalText),("recorded_at",Kind::Timestamp),
    ], key:&[1], append:true },
    Spec { name:"capacity_exemption_history", columns:&[
        ("id",Kind::Integer),("issue_id",Kind::Text),("capacity_kind",Kind::Text),
        ("capacity_name",Kind::Text),("action",Kind::Text),("provider",Kind::Text),
        ("reason",Kind::OptionalText),("actor",Kind::Text),("expires_at",Kind::OptionalTimestamp),
        ("recorded_at",Kind::Timestamp),
    ], key:&[1], append:true },
    Spec { name:"gate_results", columns:&[
        ("issue_id",Kind::Text),("gate",Kind::Text),("provider",Kind::Text),
        ("passed",Kind::Boolean),("note",Kind::OptionalText),("recorded_by",Kind::OptionalText),
        ("recorded_at",Kind::Timestamp),
    ], key:&[0,1,2], append:false },
    Spec { name:"close_metadata", columns:&[
        ("issue_id",Kind::Text),("closed_by_agent_name",Kind::OptionalText),
        ("closed_by_harness",Kind::OptionalText),("closed_by_model",Kind::OptionalText),
        ("bypassed_policy",Kind::Boolean),("bypass_reason",Kind::OptionalText),
        ("policy_gates_fired",Kind::OptionalText),("recorded_at",Kind::Timestamp),
    ], key:&[0], append:false },
    Spec { name:"capacity_occupancy", columns:&[
        ("issue_id",Kind::Text),("actor",Kind::OptionalText),("agent_name",Kind::OptionalText),
        ("harness",Kind::OptionalText),("session",Kind::OptionalText),("recorded_at",Kind::Timestamp),
    ], key:&[0], append:false },
    Spec { name:"capacity_exemptions", columns:&[
        ("issue_id",Kind::Text),("capacity_kind",Kind::Text),("capacity_name",Kind::Text),
        ("provider",Kind::Text),("reason",Kind::Text),("granted_by",Kind::Text),
        ("granted_at",Kind::Timestamp),("expires_at",Kind::OptionalTimestamp),
        ("ended_at",Kind::OptionalTimestamp),("ended_action",Kind::OptionalText),
        ("ended_by",Kind::OptionalText),
    ], key:&[0,1,2], append:false },
];

fn invalid(message: impl std::fmt::Display) -> BeadsError {
    BeadsError::Config(format!("Portable workflow history: {message}"))
}
fn parse_time(value:&str)->Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value).map(|v|v.with_timezone(&chrono::Utc)).ok()
        .or_else(|| chrono::NaiveDateTime::parse_from_str(value,"%Y-%m-%d %H:%M:%S%.f").ok().map(|v|v.and_utc()))
}
fn valid(kind:Kind,value:&Value)->bool {
    match kind {
        Kind::Text=>value.is_string(),
        Kind::OptionalText=>value.is_null()||value.is_string(),
        Kind::Integer=>value.as_i64().is_some(),
        Kind::Boolean=>matches!(value.as_i64(),Some(0|1)),
        Kind::Timestamp=>value.as_str().and_then(parse_time).is_some(),
        Kind::OptionalTimestamp=>value.is_null()||value.as_str().and_then(parse_time).is_some(),
    }
}
fn owner<'a>(spec:&Spec,row:&'a[Value])->&'a str {
    row[spec.owner()].as_str().unwrap_or_default()
}
fn from_sql(value:&SqliteValue)->Result<Value> {
    match value {
        SqliteValue::Null=>Ok(Value::Null),
        SqliteValue::Integer(v)=>Ok(Value::from(*v)),
        SqliteValue::Text(v)=>Ok(Value::String(v.to_string())),
        _=>Err(invalid("unsupported audit column type")),
    }
}
fn sql_literal(value:&Value)->Result<String> {
    if value.is_null() { Ok("NULL".to_owned()) }
    else if let Some(v)=value.as_i64() { Ok(v.to_string()) }
    else if let Some(v)=value.as_str() { Ok(format!("'{}'",v.replace(''',"''"))) }
    else { Err(invalid("unsupported audit value")) }
}

pub(crate) fn validate(snapshot:&PortableHistorySnapshot)->Result<()> {
    if snapshot.tables.len()!=SPECS.len() {
        return Err(invalid("checkpoint must contain all seven history tables"));
    }
    for (id,anchor) in &snapshot.issues {
        if id.is_empty()||anchor.status.is_empty()
            ||parse_time(&anchor.created_at).is_none()||parse_time(&anchor.updated_at).is_none() {
            return Err(invalid(format!("invalid issue anchor {id}")));
        }
    }
    let mut events=HashMap::new();
    for (spec,table) in SPECS.iter().zip(&snapshot.tables) {
        if table.table!=spec.name||table.columns!=spec.names() {
            return Err(invalid(format!("noncanonical history table {}",table.table)));
        }
        let mut sequences=HashMap::<&str,i64>::new();
        let mut previous:Option<Vec<String>>=None;
        for row in &table.rows {
            if row.len()!=spec.columns.len() { return Err(invalid(format!("{} row has wrong column count",spec.name))); }
            for ((name,kind),value) in spec.columns.iter().zip(row) {
                if !valid(*kind,value) { return Err(invalid(format!("invalid {}.{name}",spec.name))); }
            }
            let issue=owner(spec,row);
            if !snapshot.issues.contains_key(issue) { return Err(invalid(format!("{} references unknown issue {issue}",spec.name))); }
            let key=spec.key(row);
            if previous.as_ref().is_some_and(|p| if spec.append {p>&key}else{p>=&key}) {
                return Err(invalid(format!("{} rows have duplicate keys or noncanonical order",spec.name)));
            }
            previous=Some(key);
            if spec.append {
                let seq=sequences.entry(issue).or_default(); *seq+=1;
                if row[0].as_i64()!=Some(*seq) { return Err(invalid(format!("{} sequence for {issue} is incomplete",spec.name))); }
                if spec.name=="events" { events.insert((issue,*seq),row); }
            }
            if spec.name=="gate_result_history" {
                let revision=row[4].as_i64().unwrap_or(-1);
                if revision<0 { return Err(invalid("negative gate status revision")); }
                if revision>0 {
                    let event=events.get(&(issue,revision)).ok_or_else(||invalid(format!("gate history for {issue} references absent event {revision}")))?;
                    if event[2].as_str()!=Some("status_changed")||event[5]!=row[2] {
                        return Err(invalid(format!("gate history for {issue} references the wrong status transition")));
                    }
                }
            }
        }
    }
    Ok(())
}

fn normalize(snapshot:&mut PortableHistorySnapshot)->Result<()> {
    let mut event_ids=HashMap::<(String,i64),i64>::new();
    for (spec,table) in SPECS.iter().zip(&mut snapshot.tables) {
        table.rows.sort_by(|a,b|spec.key(a).cmp(&spec.key(b)).then_with(||{
            if spec.append {a[0].as_i64().cmp(&b[0].as_i64())} else {std::cmp::Ordering::Equal}
        }));
        let mut seq=HashMap::<String,i64>::new();
        for row in &mut table.rows {
            let issue=owner(spec,row).to_owned();
            if spec.append {
                let raw=row[0].as_i64().ok_or_else(||invalid("non-integer history id"))?;
                let next=seq.entry(issue.clone()).or_default(); *next+=1;
                row[0]=Value::from(*next);
                if spec.name=="events" { event_ids.insert((issue.clone(),raw),*next); }
            }
            if spec.name=="gate_result_history" {
                let raw=row[4].as_i64().unwrap_or_default();
                if raw!=0 {
                    row[4]=Value::from(*event_ids.get(&(issue.clone(),raw)).ok_or_else(||invalid(format!("gate history for {issue} references missing event {raw}")))?) ;
                }
            }
        }
    }
    validate(snapshot)
}

fn group<'a>(spec:&Spec,rows:&'a[Vec<Value>])->HashMap<&'a str,Vec<&'a Vec<Value>>> {
    let mut out=HashMap::new();
    for row in rows { out.entry(owner(spec,row)).or_insert_with(Vec::new).push(row); }
    out
}

impl PortableHistorySnapshot {
    pub(crate) fn empty_for_issues(issues: BTreeMap<String, PortableHistoryIssue>) -> Self {
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

    pub(crate) fn retain_issues(&mut self,ids:&HashSet<String>)->Result<()> {
        self.issues.retain(|id,_|ids.contains(id));
        for (spec,table) in SPECS.iter().zip(&mut self.tables) {
            table.rows.retain(|row|ids.contains(owner(spec,row)));
        }
        validate(self)
    }
    pub(crate) fn merge(&self,other:&Self)->Result<Self> {
        validate(self)?; validate(other)?;
        let mut merged=self.clone();
        for (id,anchor) in &other.issues {
            let Some(base)=self.issues.get(id) else {
                merged.issues.insert(id.clone(),anchor.clone());
                for (spec,(dst,src)) in SPECS.iter().zip(merged.tables.iter_mut().zip(&other.tables)) {
                    dst.rows.extend(src.rows.iter().filter(|r|owner(spec,r)==id).cloned());
                }
                continue;
            };
            if parse_time(&base.created_at)!=parse_time(&anchor.created_at) {
                return Err(invalid(format!("issue {id} has a different creation identity")));
            }
            let mut incoming_newer=false; let mut local_newer=false;
            for ((spec,a),b) in SPECS.iter().zip(&self.tables).zip(&other.tables) {
                if !spec.append { continue; }
                let ga=group(spec,&a.rows); let gb=group(spec,&b.rows);
                let left=ga.get(id.as_str()).map_or(&[][..],Vec::as_slice);
                let right=gb.get(id.as_str()).map_or(&[][..],Vec::as_slice);
                if left.iter().zip(right).any(|(x,y)|x!=y) { return Err(invalid(format!("divergent {} for issue {id}",spec.name))); }
                incoming_newer|=right.len()>left.len(); local_newer|=left.len()>right.len();
            }
            if incoming_newer&&local_newer { return Err(invalid(format!("incomparable audit histories for issue {id}"))); }
            if incoming_newer || (!local_newer&&parse_time(&anchor.updated_at)>parse_time(&base.updated_at)) {
                merged.issues.insert(id.clone(),anchor.clone());
            }
            if incoming_newer {
                for (spec,(dst,src)) in SPECS.iter().zip(merged.tables.iter_mut().zip(&other.tables)) {
                    dst.rows.retain(|r|owner(spec,r)!=id);
                    dst.rows.extend(src.rows.iter().filter(|r|owner(spec,r)==id).cloned());
                }
            }
        }
        for (spec,table) in SPECS.iter().zip(&mut merged.tables) {
            table.rows.sort_by(|a,b|spec.key(a).cmp(&spec.key(b)).then_with(||if spec.append{a[0].as_i64().cmp(&b[0].as_i64())}else{std::cmp::Ordering::Equal}));
        }
        validate(&merged)?; Ok(merged)
    }
}

impl SqliteStorage {
    pub(crate) fn snapshot_portable_history(&self,ids:&HashSet<String>)->Result<PortableHistorySnapshot> {
        let mut issues=BTreeMap::new();
        for row in self.execute_raw_query("SELECT id, status, updated_at, created_at FROM issues")? {
            let id=row.first().and_then(SqliteValue::as_text).ok_or_else(||invalid("issue has no id"))?;
            if !ids.contains(id) { continue; }
            let status=row.get(1).and_then(SqliteValue::as_text).ok_or_else(||invalid("issue has no status"))?;
            let updated=row.get(2).and_then(SqliteValue::as_text).ok_or_else(||invalid("issue has no updated_at"))?;
            let created=row.get(3).and_then(SqliteValue::as_text).ok_or_else(||invalid("issue has no created_at"))?;
            issues.insert(id.to_owned(),PortableHistoryIssue{status:status.to_owned(),updated_at:updated.to_owned(),created_at:created.to_owned()});
        }
        let mut tables=Vec::new();
        for spec in SPECS {
            let columns=spec.names();
            let mut rows=Vec::new();
            for row in self.execute_raw_query(&format!("SELECT {} FROM {} ORDER BY {}",columns.join(", "),spec.name,if spec.append{"id"}else{spec.columns[spec.key[0]].0}))? {
                let id=row.get(spec.owner()).and_then(SqliteValue::as_text).ok_or_else(||invalid("history row has no issue id"))?;
                if issues.contains_key(id) { rows.push(row.iter().map(from_sql).collect::<Result<Vec<_>>>()?); }
            }
            tables.push(PortableHistoryTable{table:spec.name.to_owned(),columns,rows});
        }
        let mut snapshot=PortableHistorySnapshot{issues,tables};
        normalize(&mut snapshot)?;
        Ok(snapshot)
    }

    pub(crate) fn all_portable_history(&self)->Result<PortableHistorySnapshot> {
        let ids=self.execute_raw_query("SELECT id FROM issues")?.into_iter()
            .filter_map(|row|row.first().and_then(SqliteValue::as_text).map(str::to_owned))
            .collect();
        self.snapshot_portable_history(&ids)
    }

    pub(crate) fn restore_portable_history(&mut self,incoming:&PortableHistorySnapshot)->Result<()> {
        validate(incoming)?;
        let ids=incoming.issues.keys().cloned().collect::<HashSet<_>>();
        let local=self.snapshot_portable_history(&ids)?;
        if local.issues.len()!=incoming.issues.len() { return Err(invalid("history owner missing after issue import")); }
        for (id,anchor) in &incoming.issues {
            if parse_time(&local.issues[id].created_at)!=parse_time(&anchor.created_at) {
                return Err(invalid(format!("issue {id} has a different creation identity")));
            }
        }
        let mut ancestry=HashMap::<String,(bool,bool)>::new();
        for ((spec,src),dst) in SPECS.iter().zip(&incoming.tables).zip(&local.tables) {
            if !spec.append { continue; }
            let ga=group(spec,&dst.rows); let gb=group(spec,&src.rows);
            for id in incoming.issues.keys() {
                let before=ga.get(id.as_str()).map_or(&[][..],Vec::as_slice);
                let after=gb.get(id.as_str()).map_or(&[][..],Vec::as_slice);
                if before.iter().zip(after).any(|(a,b)|a!=b) { return Err(invalid(format!("divergent {} for issue {id}",spec.name))); }
                let relation=ancestry.entry(id.clone()).or_default();
                relation.0|=after.len()>before.len(); relation.1|=before.len()>after.len();
                if relation.0&&relation.1 { return Err(invalid(format!("incomparable audit histories for issue {id}"))); }
            }
        }
        let incoming=incoming.clone();
        self.with_write_transaction(|storage| {
            let current=storage.snapshot_portable_history(&ids)?;
            let mut event_ids=HashMap::<(String,i64),i64>::new();
            for row in storage.execute_raw_query("SELECT id, issue_id FROM events ORDER BY id")? {
                let dbid=row.first().and_then(SqliteValue::as_integer).unwrap_or_default();
                let issue=row.get(1).and_then(SqliteValue::as_text).unwrap_or_default().to_owned();
                let next=event_ids.keys().filter(|(owner,_)|owner==&issue).count() as i64+1;
                event_ids.insert((issue,next),dbid);
            }
            for (index,spec) in SPECS.iter().enumerate() {
                let before=group(spec,&current.tables[index].rows);
                if spec.append {
                    for row in &incoming.tables[index].rows {
                        let issue=owner(spec,row);
                        let seq=row[0].as_i64().unwrap_or_default();
                        if usize::try_from(seq).is_ok_and(|s|s<=before.get(issue).map_or(0,Vec::len)) { continue; }
                        let mut values=row[1..].to_vec();
                        if spec.name=="gate_result_history" {
                            let rev=row[4].as_i64().unwrap_or_default();
                            if rev!=0 { values[3]=Value::from(*event_ids.get(&(issue.to_owned(),rev)).ok_or_else(||invalid("gate revision mapping is absent"))?); }
                        }
                        insert(storage,spec,&values,true)?;
                        if spec.name=="events" {
                            let dbid=storage.execute_raw_query("SELECT last_insert_rowid()")?.first()
                                .and_then(|r|r.first()).and_then(SqliteValue::as_integer).ok_or_else(||invalid("event insert returned no id"))?;
                            event_ids.insert((issue.to_owned(),seq),dbid);
                        }
                    }
                } else if !ancestry.values().any(|(_,local_newer)|*local_newer) {
                    for row in &incoming.tables[index].rows { insert(storage,spec,row,false)?; }
                }
            }
            Ok(())
        })
    }
}

fn insert(storage:&SqliteStorage,spec:&Spec,row:&[Value],omit_id:bool)->Result<()> {
    let offset=usize::from(omit_id);
    let cols=spec.columns[offset..].iter().map(|(name,_)|*name).collect::<Vec<_>>().join(", ");
    let vals=row.iter().map(sql_literal).collect::<Result<Vec<_>>>()?.join(", ");
    let replace=if omit_id{""}else{" OR REPLACE"};
    storage.execute_raw(&format!("INSERT{replace} INTO {} ({cols}) VALUES ({vals})",spec.name))
}
