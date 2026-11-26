//! Git plumbing for explicitly checking or staging an export pair.

use super::vcs::hardened_git_command;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Run a selective export against the resolved workspace, without opening SQLite.
pub fn execute(
    args: &crate::cli::SyncArgs,
    cli: &crate::config::CliOverrides,
    ctx: &crate::output::OutputContext,
) -> Result<()> {
    super::sync::validate_sync_mode_args(args)?;
    let issues = super::sync::export_pair_path(args, cli)?;
    let history = crate::portable_history::sidecar_path(&issues);
    let render = |value: &serde_json::Value, text: &str| {
        if ctx.is_json() {
            ctx.json(value);
        } else if ctx.is_toon() {
            ctx.toon(value);
        } else {
            ctx.print_line(text);
        }
    };
    if let Some(revision) = &args.check {
        let (rows, companion) = read_export_pair(&issues, &history, revision.as_deref())?;
        crate::portable_history::check_export_pair(&rows, &companion)?;
        render(
            &serde_json::json!({"valid": true, "revision": revision}),
            "Export pair is valid.",
        );
        return Ok(());
    }
    if args.stage.is_empty() {
        return Err(BeadsError::validation(
            "stage",
            "expected at least one issue ID",
        ));
    }

    let revision = args
        .merge_base
        .as_deref()
        .map(|base| {
            base.to_str()
                .ok_or_else(|| BeadsError::validation("base", "expected a UTF-8 Git revision"))
        })
        .transpose()?;
    // A flush publishes the companion before the issue rows while holding
    // this same lock. Capture both from one completed publication.
    let authority =
        crate::sync::blocking_jsonl_family_write_lock_with_timeout(&issues, cli.lock_timeout)?;
    stage_export_pair(&issues, &history, revision, |base| {
        let working = std::io::read_to_string(authority.capture_target()?.reader())?;
        let checkpoint = crate::sync::path::capture_jsonl_source_snapshot(&history)?;
        let companion = std::io::read_to_string(checkpoint.reader())?;
        authority.verify_jsonl_authority()?;
        crate::portable_history::compose_selected_export(base, (&working, &companion), &args.stage)
    })?;
    drop(authority);
    render(
        &serde_json::json!({"staged": &args.stage}),
        &format!("Staged {} issue(s) and their history.", args.stage.len()),
    );
    Ok(())
}

fn git(dir: &Path) -> Command {
    let mut command = hardened_git_command(OsStr::new("git"), dir, true, true);
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
    command
}

fn stdout(operation: &str, output: Output) -> Result<Vec<u8>> {
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        return Err(BeadsError::external_command(
            "git",
            format!(
                "{operation}: {}",
                sanitize_terminal_inline(diagnostic.trim())
            ),
        ));
    }
    Ok(output.stdout)
}

fn utf8(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| BeadsError::validation("export", "expected UTF-8"))
}

fn output_path(mut bytes: Vec<u8>) -> Result<PathBuf> {
    if bytes.pop() != Some(b'\n') || bytes.is_empty() {
        return Err(BeadsError::validation(
            "git path",
            "missing repository path",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(utf8(bytes)?))
    }
}

struct Target {
    root: PathBuf,
    paths: [OsString; 2],
    index: PathBuf,
}

impl Target {
    fn locate(issues: &Path, history: &Path) -> Result<Self> {
        let parent = issues
            .parent()
            .filter(|p| Some(*p) == history.parent())
            .ok_or_else(|| BeadsError::validation("export paths", "expected sibling exports"))?;
        let parent = fs::canonicalize(parent)?;
        let root = output_path(stdout(
            "locate worktree",
            git(&parent)
                .args(["rev-parse", "--show-toplevel"])
                .output()?,
        )?)?
        .canonicalize()?;
        let relative = parent
            .strip_prefix(&root)
            .map_err(|_| BeadsError::validation("export paths", "outside Git worktree"))?;
        let pathspec = |path: &Path| -> Result<OsString> {
            let mut spec = OsString::new();
            for part in relative {
                spec.push(part);
                spec.push("/");
            }
            spec.push(
                path.file_name()
                    .ok_or_else(|| BeadsError::validation("export", "missing filename"))?,
            );
            Ok(spec)
        };
        // In particular, honor an index selected by Git when invoking br from a
        // hook. A relative GIT_INDEX_FILE is relative to the caller's cwd.
        let index = if let Some(path) = std::env::var_os("GIT_INDEX_FILE") {
            std::env::current_dir()?.join(path)
        } else {
            output_path(stdout(
                "locate index",
                git(&root)
                    .args(["rev-parse", "--path-format=absolute", "--git-path", "index"])
                    .output()?,
            )?)?
        };
        let paths = [pathspec(issues)?, pathspec(history)?];
        Ok(Self { root, paths, index })
    }

    /// Read both object IDs in one Git invocation. Later blob reads address
    /// immutable objects even if a concurrent writer replaces the index or ref.
    fn entries(&self, revision: Option<&str>) -> Result<[Option<Entry>; 2]> {
        let mut command = git(&self.root);
        if let Some(rev) = revision {
            let tree = utf8(stdout(
                "resolve base revision",
                git(&self.root)
                    .args([
                        "rev-parse",
                        "--verify",
                        "--end-of-options",
                        &format!("{rev}^{{tree}}"),
                    ])
                    .output()?,
            )?)?;
            command.args(["ls-tree", "-z", tree.trim_end(), "--"]);
        } else {
            command
                .env("GIT_INDEX_FILE", &self.index)
                .args(["ls-files", "--stage", "-z", "--"]);
        }
        let output = stdout("read export entries", command.args(&self.paths).output()?)?;
        let mut entries = [None, None];
        for row in output.split(|&byte| byte == 0).filter(|r| !r.is_empty()) {
            let invalid = || {
                BeadsError::validation("git export pair", "expected regular, resolved file entries")
            };
            let tab = row.iter().position(|&b| b == b'\t').ok_or_else(invalid)?;
            let path = &row[tab + 1..];
            let i = self
                .paths
                .iter()
                .position(|p| p.as_encoded_bytes() == path)
                .ok_or_else(invalid)?;
            let header = std::str::from_utf8(&row[..tab]).map_err(|_| invalid())?;
            let fields: Vec<_> = header.split(' ').collect();
            let (mode, oid) = match fields.as_slice() {
                [mode, "blob", oid] if revision.is_some() => (*mode, *oid),
                [mode, oid, "0"] if revision.is_none() => (*mode, *oid),
                _ => return Err(invalid()),
            };
            if entries[i].is_some() || !matches!(mode, "100644" | "100755") {
                return Err(invalid());
            }
            entries[i] = Some(Entry {
                mode: mode.into(),
                oid: oid.into(),
            });
        }
        Ok(entries)
    }

    fn read_pair(&self, entries: &[Option<Entry>; 2]) -> Result<Option<(String, String)>> {
        match entries {
            [Some(issues), Some(history)] => {
                let read = |entry: &Entry| {
                    let output = git(&self.root)
                        .args(["cat-file", "blob", &entry.oid])
                        .output()?;
                    utf8(stdout("read export blob", output)?)
                };
                Ok(Some((read(issues)?, read(history)?)))
            }
            [None, None] => Ok(None),
            _ => Err(BeadsError::validation(
                "git export pair",
                "one export file is missing",
            )),
        }
    }
}

struct Entry {
    mode: String,
    oid: String,
}

fn read_export_pair(
    issues: &Path,
    history: &Path,
    revision: Option<&str>,
) -> Result<(String, String)> {
    let target = Target::locate(issues, history)?;
    target
        .read_pair(&target.entries(revision)?)?
        .ok_or_else(|| BeadsError::validation("git export pair", "both export files are missing"))
}

fn write_blob(root: &Path, contents: &str) -> Result<String> {
    let mut child = git(root)
        .args(["hash-object", "-w", "--stdin", "--no-filters"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().expect("requested piped stdin");
    let written = input.write_all(contents.as_bytes());
    drop(input);
    let oid = utf8(stdout("write export blob", child.wait_with_output()?)?)?;
    written?;
    Ok(oid.trim_end().to_owned())
}

fn stage_export_pair(
    issues: &Path,
    history: &Path,
    base: Option<&str>,
    compose: impl FnOnce(Option<(&str, &str)>) -> Result<(String, String)>,
) -> Result<()> {
    let target = Target::locate(issues, history)?;
    let index = &target.index;
    let index_dir = index
        .parent()
        .ok_or_else(|| BeadsError::validation("index", "invalid index path"))?;
    let mut name = index
        .file_name()
        .ok_or_else(|| BeadsError::validation("index", "missing index filename"))?
        .to_owned();
    name.push(".lock");
    // Git also uses exclusive creation of this exact sibling as its index
    // lock. Keep it continuously present from the base read to the rename.
    let lock = tempfile::Builder::new()
        .prefix(&name)
        .rand_bytes(0)
        .tempfile_in(index_dir)
        .map_err(|e| {
            BeadsError::external_command("git", format!("cannot acquire index lock: {e}"))
        })?
        .into_temp_path();
    let index_entries = target.entries(None)?;
    let base_entries = if base.is_some() {
        target.entries(base)?
    } else {
        index_entries
    };
    let pair = target.read_pair(&base_entries)?;
    let contents: [String; 2] =
        compose(pair.as_ref().map(|(a, b)| (a.as_str(), b.as_str())))?.into();
    let lock_path: &Path = lock.as_ref();
    let permissions = match fs::symlink_metadata(index) {
        Ok(meta) if meta.is_file() => {
            fs::copy(index, lock_path)?;
            Some(meta.permissions())
        }
        Ok(_) => {
            return Err(BeadsError::validation(
                "git index",
                "expected a regular file",
            ));
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            stdout(
                "initialize index",
                git(&target.root)
                    .env("GIT_INDEX_FILE", lock_path)
                    .args(["read-tree", "--empty"])
                    .output()?,
            )?;
            None
        }
        Err(e) => return Err(e.into()),
    };
    // Git writes each alternate-index change via index.lock.lock and
    // renames it to index.lock. Other Git writers still see our lock.
    for ((path, entry), content) in target.paths.iter().zip(&base_entries).zip(contents) {
        let oid = write_blob(&target.root, &content)?;
        let mode = entry.as_ref().map_or("100644", |e| &e.mode);
        stdout(
            "stage export blob",
            git(&target.root)
                .env("GIT_INDEX_FILE", lock_path)
                .args(["update-index", "--add", "--cacheinfo", mode, &oid])
                .arg(path)
                .output()?,
        )?;
    }
    fs::OpenOptions::new()
        .write(true)
        .open(lock_path)?
        .sync_all()?;
    if let Some(permissions) = permissions {
        fs::set_permissions(lock_path, permissions)?;
    }
    lock.persist(index).map_err(|error| error.error)?;
    Ok(())
}
