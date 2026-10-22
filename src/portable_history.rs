//! Repository-managed workflow-history companion for issue JSONL.
//!
//! This is deliberately outside the classic issue interchange payload. A
//! checkpoint is bound to stable issue identity/status/update anchors and
//! carries all available workflow evidence. Two generations are retained so
//! publishing history before issues is recoverable after interruption.

use crate::error::{BeadsError, Result};
use crate::storage::{
    PortableHistoryIssue, PortableHistorySnapshot, SqliteStorage, validate_portable_history,
};
use crate::util::hex_encode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) const WARNINGS_KEY: &str = "portable_history_warnings";
const SOURCE_KEY: &str = "portable_history_source_sha256";
const LEGACY_WARNING: &str = "Portable workflow history is unavailable for this issue source. Earlier events, gate results, close metadata, and capacity evidence cannot be verified or reconstructed from issues.jsonl alone. Existing local evidence is preserved; missing evidence has not been fabricated.";
const ENROLLMENT_WARNING: &str = "This tracker predates portable workflow history. Available database evidence was exported, but completeness of earlier audit history cannot be verified.";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    version: u32,
    anchor_sha256: String,
    history_sha256: String,
    history: PortableHistorySnapshot,
    warnings: Vec<String>,
}

fn conflict(message: impl Into<String>) -> BeadsError {
    BeadsError::SyncConflict { message: format!("Portable workflow history: {}", message.into()) }
}

pub(crate) fn sidecar_path(jsonl:&Path)->PathBuf {
    jsonl.with_extension("history.jsonl")
}

fn digest_history(version:u32,anchor:&str,history:&PortableHistorySnapshot,warnings:&[String])->Result<String> {
    Ok(hex_encode(&Sha256::digest(serde_json::to_vec(&(version,anchor,history,warnings))?)))
}

fn anchor_digest(issues:&BTreeMap<String,PortableHistoryIssue>)->Result<String> {
    Ok(hex_encode(&Sha256::digest(serde_json::to_vec(issues)?)))
}

impl Checkpoint {
    fn new(history:PortableHistorySnapshot,warnings:Vec<String>)->Result<Self> {
        validate_portable_history(&history)?;
        let anchor_sha256=anchor_digest(&history.issues)?;
        let mut checkpoint=Self{version:1,anchor_sha256,history_sha256:String::new(),history,warnings};
        checkpoint.history_sha256=digest_history(checkpoint.version,&checkpoint.anchor_sha256,&checkpoint.history,&checkpoint.warnings)?;
        Ok(checkpoint)
    }
    fn validate(&self)->Result<()> {
        if self.version!=1 { return Err(conflict(format!("unsupported format version {}",self.version))); }
        validate_portable_history(&self.history)?;
        if self.anchor_sha256!=anchor_digest(&self.history.issues)?
            ||self.history_sha256!=digest_history(self.version,&self.anchor_sha256,&self.history,&self.warnings)? {
            return Err(conflict("checkpoint checksum mismatch"));
        }
        Ok(())
    }
}

fn read_lines(path:&Path)->Result<Vec<Checkpoint>> {
    let meta=fs::symlink_metadata(path)?;
    if !meta.is_file()||meta.file_type().is_symlink() {
        return Err(conflict("history companion must be a regular file"));
    }
    let text=fs::read_to_string(path)?;
    let mut out=Vec::new(); let mut seen=HashSet::new();
    for (index,line) in text.lines().enumerate() {
        if line.trim().is_empty(){continue;}
        let cp:Checkpoint=serde_json::from_str(line)
            .map_err(|e|conflict(format!("malformed checkpoint at line {}: {e}",index+1)))?;
        cp.validate()?;
        if !seen.insert(cp.anchor_sha256.clone()) {
            return Err(conflict("duplicate issue generation in history companion"));
        }
        out.push(cp);
        if out.len()>2 { return Err(conflict("history companion contains more than two generations")); }
    }
    if out.is_empty(){return Err(conflict("history companion is empty"));}
    Ok(out)
}

fn source_anchors(path:&Path)->Result<BTreeMap<String,PortableHistoryIssue>> {
    if !path.exists(){return Ok(BTreeMap::new());}
    let text=fs::read_to_string(path)?;
    let mut out=BTreeMap::new();
    for (index,line) in text.lines().enumerate() {
        if line.trim().is_empty(){continue;}
        let value:Value=serde_json::from_str(line)
            .map_err(|e|conflict(format!("cannot inspect issue record {} for history binding: {e}",index+1)))?;
        let object=value.as_object().ok_or_else(||conflict("issue record is not an object"))?;
        let field=|name:&str|->Result<String>{
            object.get(name).and_then(Value::as_str).map(str::to_owned)
                .ok_or_else(||conflict(format!("issue record lacks string field {name}")))
        };
        let id=field("id")?;
        let anchor=PortableHistoryIssue{
            status:field("status")?,
            created_at:field("created_at")?,
            updated_at:field("updated_at")?,
        };
        if out.insert(id.clone(),anchor).is_some(){return Err(conflict(format!("duplicate issue id {id}")));}
    }
    Ok(out)
}

fn matches_source(cp:&Checkpoint,source:&BTreeMap<String,PortableHistoryIssue>)->bool {
    cp.history.issues==*source
}

fn selected(path:&Path)->Result<Option<Checkpoint>> {
    let sidecar=sidecar_path(path);
    if !sidecar.exists(){return Ok(None);}
    let anchors=source_anchors(path)?;
    read_lines(&sidecar)?.into_iter().find(|cp|matches_source(cp,&anchors))
        .map(Some).ok_or_else(||conflict("no checkpoint matches the selected issues.jsonl generation"))
}

pub(crate) fn stored_warnings(storage:&SqliteStorage)->Result<Vec<String>> {
    storage.get_metadata(WARNINGS_KEY)?.map_or_else(
        ||Ok(Vec::new()),
        |value|serde_json::from_str(&value).map_err(|e|conflict(format!("invalid persisted coverage warning: {e}"))),
    )
}
fn union(mut a:Vec<String>,b:&[String])->Vec<String>{
    a.extend_from_slice(b);a.sort();a.dedup();a
}

fn current_checkpoint(storage:&SqliteStorage)->Result<Checkpoint> {
    let history=storage.all_portable_history()?;
    let warnings=if storage.get_metadata(WARNINGS_KEY)?.is_some() {
        stored_warnings(storage)?
    } else if history.issues.is_empty() {
        Vec::new()
    } else {
        vec![ENROLLMENT_WARNING.to_owned()]
    };
    Checkpoint::new(history,warnings)
}

fn write_atomic(path:&Path,checkpoints:&[Checkpoint])->Result<()> {
    if let Ok(meta)=fs::symlink_metadata(path)
        && (!meta.is_file()||meta.file_type().is_symlink()) {
        return Err(conflict("refusing to replace a non-regular history companion"));
    }
    let dir=path.parent().ok_or_else(||conflict("history companion has no parent"))?;
    let name=path.file_name().and_then(|v|v.to_str()).ok_or_else(||conflict("invalid history filename"))?;
    let mut temp=None;
    for index in 0..64u32 {
        let candidate=dir.join(format!(".{name}.tmp.{}.{}",std::process::id(),index));
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(file)=>{temp=Some((candidate,file));break;}
            Err(e) if e.kind()==std::io::ErrorKind::AlreadyExists=>continue,
            Err(e)=>return Err(e.into()),
        }
    }
    let (temp_path,mut file)=temp.ok_or_else(||conflict("could not allocate history temp file"))?;
    for cp in checkpoints {
        serde_json::to_writer(&mut file,cp)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    let reread=read_lines(&temp_path)?;
    if reread.len()!=checkpoints.len(){return Err(conflict("staged history verification failed"));}
    fs::rename(&temp_path,path)?;
    if let Ok(parent)=OpenOptions::new().read(true).open(dir){let _=parent.sync_all();}
    Ok(())
}

/// Publish history before the corresponding issue export. If the issue export
/// is interrupted, the older matching checkpoint remains selectable.
pub(crate) fn publish_before_export(storage:&SqliteStorage,jsonl:&Path)->Result<()> {
    let sidecar=sidecar_path(jsonl);
    let previous=if sidecar.exists() {
        let old=read_lines(&sidecar)?;
        if jsonl.exists() {
            let anchors=source_anchors(jsonl)?;
            Some(old.into_iter().find(|cp|matches_source(cp,&anchors))
                .ok_or_else(||conflict("existing history has no checkpoint for current issues.jsonl"))?)
        } else {
            return Err(conflict("history companion exists while issue source is missing"));
        }
    } else if jsonl.exists() && fs::metadata(jsonl)?.len() > 0 {
        // First enrollment cannot prove history that was never exported.
        // Preserve a matching, explicitly incomplete generation so a crash
        // before issues.jsonl publication still has an honest checkpoint.
        let anchors=source_anchors(jsonl)?;
        Some(Checkpoint::new(
            PortableHistorySnapshot::empty_for_issues(anchors),
            vec![LEGACY_WARNING.to_owned()],
        )?)
    } else { None };
    let mut next=current_checkpoint(storage)?;
    if let Some(prev)=previous.as_ref() {
        let merged=prev.history.merge(&next.history)?;
        next=Checkpoint::new(merged,union(next.warnings.clone(),&prev.warnings))?;
    }
    let mut checkpoints=Vec::new();
    if let Some(prev)=previous {
        if prev.anchor_sha256!=next.anchor_sha256 {checkpoints.push(prev);}
    }
    checkpoints.push(next);
    write_atomic(&sidecar,&checkpoints)
}

/// Verify the just-published issue generation can select the current evidence
/// and record its sidecar digest for cheap no-op detection.
pub(crate) fn finalize_export(storage:&mut SqliteStorage,jsonl:&Path)->Result<()> {
    let cp=selected(jsonl)?.ok_or_else(||conflict("export completed without a matching history checkpoint"))?;
    let local=storage.all_portable_history()?;
    if cp.history.merge(&local)?!=cp.history {
        return Err(conflict("workflow evidence changed during export; retry the flush"));
    }
    storage.set_metadata(WARNINGS_KEY,&serde_json::to_string(&cp.warnings)?)?;
    let bytes=fs::read(sidecar_path(jsonl))?;
    storage.set_metadata(SOURCE_KEY,&hex_encode(&Sha256::digest(bytes)))?;
    Ok(())
}

/// Validate an incoming companion before issue mutation.
pub(crate) fn preflight_import(jsonl:&Path)->Result<()> {
    if sidecar_path(jsonl).exists(){let _=selected(jsonl)?;}
    Ok(())
}

/// Replay matching evidence. Legacy sources stay usable but acquire a durable
/// warning rather than fabricated events or gate approvals.
pub(crate) fn replay_after_import(storage:&mut SqliteStorage,jsonl:&Path)->Result<Vec<String>> {
    let warnings=if let Some(cp)=selected(jsonl)? {
        storage.restore_portable_history(&cp.history)?;
        union(stored_warnings(storage)?,&cp.warnings)
    } else if jsonl.exists()&&fs::metadata(jsonl)?.len()>0 {
        union(stored_warnings(storage)?,&[LEGACY_WARNING.to_owned()])
    } else {
        stored_warnings(storage)?
    };
    storage.set_metadata(WARNINGS_KEY,&serde_json::to_string(&warnings)?)?;
    if sidecar_path(jsonl).exists() {
        let bytes=fs::read(sidecar_path(jsonl))?;
        storage.set_metadata(SOURCE_KEY,&hex_encode(&Sha256::digest(bytes)))?;
    } else {
        storage.set_metadata(SOURCE_KEY,"missing")?;
    }
    Ok(warnings)
}

pub(crate) fn needs_import(storage:&SqliteStorage,jsonl:&Path)->Result<bool> {
    let sidecar=sidecar_path(jsonl);
    if !sidecar.exists(){return Ok(storage.get_metadata(SOURCE_KEY)?.as_deref()!=Some("missing"));}
    let hash=hex_encode(&Sha256::digest(fs::read(sidecar)?));
    Ok(storage.get_metadata(SOURCE_KEY)?.as_deref()!=Some(hash.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue,Status};
    use crate::storage::IssueUpdate;
    use tempfile::TempDir;

    #[test]
    fn sidecar_keeps_previous_generation_and_replays_gate_history()->Result<()> {
        let temp=TempDir::new()?;
        let jsonl=temp.path().join("issues.jsonl");
        let mut storage=SqliteStorage::open_memory()?;
        let issue:Issue=serde_json::from_value(serde_json::json!({
            "id":"ph-a","title":"portable","created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:00Z"
        }))?;
        storage.create_issue(&issue,"alice")?;
        let open=storage.get_issue("ph-a")?.unwrap();
        fs::write(&jsonl,format!("{}\n",serde_json::to_string(&open)?))?;
        storage.set_metadata(WARNINGS_KEY,"[]")?;
        publish_before_export(&storage,&jsonl)?;
        storage.update_issue("ph-a",&IssueUpdate{status:Some(Status::InProgress),..Default::default()},"alice")?;
        let revision=storage.status_revision("ph-a")?;
        storage.record_scoped_gate_result("ph-a","in_progress",revision,"closed","ci","runner",true,None,"bot")?;
        publish_before_export(&storage,&jsonl)?;
        let active=storage.get_issue("ph-a")?.unwrap();
        fs::write(&jsonl,format!("{}\n",serde_json::to_string(&active)?))?;
        let selected=selected(&jsonl)?.expect("matching checkpoint");
        assert_eq!(selected.history.tables[1].rows.len(),1);
        assert_eq!(read_lines(&sidecar_path(&jsonl))?.len(),2);
        Ok(())
    }
}
