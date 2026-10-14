//! Packaged operator recovery, deliberately independent of ordinary br startup.
//!
//! This companion binary embeds the one reviewed Python helper. It never opens
//! storage, discovers a workspace, imports JSONL, or holds a parent-side lease.
//! On supported Linux hosts, exec transfers the process to the helper so signal
//! delivery and the recovery exit status are not obscured by a waiting wrapper.
//! Existing br commands and the frame-bearing-WAL admission guard are unchanged.

#![deny(unsafe_code)]

use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

use clap::Parser;

const HELPER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/recover_orphaned_wal.py"
));
const DEFAULT_MAX_BYTES: u64 = 1_073_741_824;

#[derive(Debug, Parser)]
#[command(
    name = "br-wal-recover",
    version,
    about = "Recover committed WAL data without rewriting existing database files",
    long_about = "Recover committed WAL data into a NEW evidence directory.\n\
        Requires Python 3.10+ with its standard sqlite3 module on 64-bit Linux.\n\
        No source checkout or sqlite3 shell is needed at runtime.\n\n\
        Stop ALL br and external SQLite users first and keep them stopped.\n\
        The default creates a verified copy and leaves the source family unchanged.\n\
        --restore-shm additionally installs only an absent shared-memory index.\n\
        --resume-from revalidates a prior installation without replacing its index.\n\
        No mode checkpoints or replaces the source main file or WAL.\n\n\
        --copy-last-commit copies through the last validated commit despite a crashed\n\
        transaction or damaged tail; it never restores a source index. Unverified\n\
        tail bytes may contain later changes not present in the recovered copy.\n\n\
        Success emits the helper's JSON completion receipt. On error or interruption,\n\
        retain all evidence and any installed SHM; do not delete sidecars to retry.\n\
        This is explicit operator recovery, not automatic startup repair."
)]
struct Args {
    /// Original database. Ambient BEADS_DIR/BEADS_DB and JSONL are not consulted.
    #[arg(long, value_name = "DATABASE")]
    db: PathBuf,

    /// NEW recovery/evidence directory; an existing destination is never replaced.
    #[arg(long, value_name = "NEW_DIRECTORY")]
    output: PathBuf,

    /// Also install a verified absent SHM at the source; refuse any existing SHM.
    #[arg(long)]
    restore_shm: bool,

    /// Prior restoration evidence; revalidate its existing SHM, never reinstall it.
    #[arg(long, value_name = "PRIOR_EVIDENCE", conflicts_with = "restore_shm")]
    resume_from: Option<PathBuf>,

    /// Copy through the last validated WAL commit; retain but do not replay damaged tails.
    #[arg(long, conflicts_with_all = ["restore_shm", "resume_from"])]
    copy_last_commit: bool,

    /// Maximum bytes per input family component and reconstructed database.
    #[arg(
        long,
        value_name = "BYTES",
        default_value_t = DEFAULT_MAX_BYTES,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    max_bytes: u64,
}

fn helper_command(args: &Args) -> io::Result<Command> {
    if args.copy_last_commit && (args.restore_shm || args.resume_from.is_some()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--copy-last-commit is copy-only; it cannot restore or resume a source index",
        ));
    }
    if args.restore_shm && args.resume_from.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--restore-shm and --resume-from are mutually exclusive",
        ));
    }
    // Absolute spellings cannot become Python option tokens even if a basename
    // starts with '-'. No lossy path conversion, shell, or runtime file lookup.
    let source = std::path::absolute(&args.db)?;
    let destination = std::path::absolute(&args.output)?;
    let mut command = Command::new("python3");
    command
        .args(["-I", "-S", "-u", "-c", HELPER, "--db"])
        .arg(source)
        .arg("--output")
        .arg(destination)
        .arg("--max-bytes")
        .arg(args.max_bytes.to_string())
        .stdin(Stdio::null());
    // -I ignores Python environment variables, user site and the current
    // directory. -S also disables system sitecustomize startup hooks.
    if args.restore_shm {
        command.arg("--restore-shm");
    }
    if args.copy_last_commit {
        command.arg("--copy-last-commit");
    }
    if let Some(evidence) = &args.resume_from {
        command
            .arg("--resume-from")
            .arg(std::path::absolute(evidence)?);
    }
    Ok(command)
}

#[cfg(unix)]
fn replace_process(command: &mut Command) -> io::Error {
    use std::os::unix::process::CommandExt;
    command.exec()
}

#[cfg(not(unix))]
fn replace_process(_command: &mut Command) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix process replacement unavailable",
    )
}

fn launch(args: &Args) -> io::Error {
    if !cfg!(all(target_os = "linux", target_pointer_width = "64")) {
        return io::Error::new(
            io::ErrorKind::Unsupported,
            "recovery requires 64-bit Linux OFD locks; no unsafe fallback is used",
        );
    }
    match helper_command(args) {
        Ok(mut command) => replace_process(&mut command),
        Err(error) => error,
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    // On success exec does not return. No database handles or recovery state
    // exist in this process before it, and no output is reinterpreted afterward.
    let error = launch(&args);
    eprintln!(
        "{}",
        serde_json::json!({
            "ok": false,
            "error": format!("could not start packaged WAL recovery: {error}"),
            "recovery_started": false,
            "hint": "Use 64-bit Linux and install Python 3.10+ with its standard sqlite3 module. The launcher did not open the source database."
        })
    );
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn arguments() -> Args {
        Args::try_parse_from([
            "br-wal-recover",
            "--db",
            "./-source ' λ.db",
            "--output",
            "./new $HOME ; output",
        ])
        .unwrap()
    }

    #[test]
    fn copy_is_default_and_restore_requires_explicit_consent() {
        let args = arguments();
        assert!(!args.restore_shm);
        assert!(!args.copy_last_commit);
        assert!(args.resume_from.is_none());
        assert_eq!(args.max_bytes, DEFAULT_MAX_BYTES);
        let restored = Args::try_parse_from([
            "br-wal-recover",
            "--db",
            "source",
            "--output",
            "new",
            "--restore-shm",
        ])
        .unwrap();
        assert!(restored.restore_shm);
    }

    #[test]
    fn explicit_paths_and_positive_limits_are_required() {
        for argv in [
            vec!["br-wal-recover", "--output", "new"],
            vec!["br-wal-recover", "--db", "source"],
            vec![
                "br-wal-recover", "--db", "source", "--output", "new", "--max-bytes", "0",
            ],
            vec![
                "br-wal-recover", "--db", "source", "--output", "new", "--max-bytes", "-1",
            ],
        ] {
            assert!(Args::try_parse_from(argv).is_err());
        }
    }

    #[test]
    fn launch_is_isolated_and_preserves_argument_boundaries() {
        let args = arguments();
        let command = helper_command(&args).unwrap();
        assert_eq!(command.get_program(), OsStr::new("python3"));
        let values: Vec<_> = command.get_args().collect();
        assert_eq!(values.len(), 11);
        assert_eq!(values[0], OsStr::new("-I"));
        assert_eq!(values[1], OsStr::new("-S"));
        assert_eq!(values[2], OsStr::new("-u"));
        assert_eq!(values[3], OsStr::new("-c"));
        assert_eq!(values[4], HELPER);
        assert_eq!(values[5], "--db");
        assert_eq!(values[6], std::path::absolute(&args.db).unwrap().as_os_str());
        assert_eq!(values[7], "--output");
        assert_eq!(
            values[8],
            std::path::absolute(&args.output).unwrap().as_os_str()
        );
        assert_eq!(values[9], "--max-bytes");
        assert_eq!(values[10], "1073741824");
    }

    #[test]
    fn restoration_flag_and_limit_are_forwarded_exactly_once() {
        let mut args = arguments();
        args.restore_shm = true;
        args.max_bytes = 1234;
        let command = helper_command(&args).unwrap();
        let values: Vec<_> = command.get_args().collect();
        assert_eq!(values.len(), 12);
        assert_eq!(values[10], "1234");
        assert_eq!(values[11], "--restore-shm");
    }

    #[test]
    fn resume_is_explicit_and_conflicts_with_restoration() {
        let argv = [
            "br-wal-recover", "--db", "source", "--output", "new", "--resume-from", "prior",
        ];
        let args = Args::try_parse_from(argv).unwrap();
        assert_eq!(args.resume_from, Some(PathBuf::from("prior")));
        assert!(!args.restore_shm);
        assert!(Args::try_parse_from(argv.into_iter().chain(["--restore-shm"])).is_err());
        let mut conflicting = args;
        conflicting.restore_shm = true;
        assert_eq!(
            helper_command(&conflicting).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn resume_evidence_is_one_lossless_absolute_argument() {
        let mut args = arguments();
        let evidence = PathBuf::from("./-prior ' λ $HOME evidence");
        args.resume_from = Some(evidence.clone());
        let command = helper_command(&args).unwrap();
        let values: Vec<_> = command.get_args().collect();
        assert_eq!(values.len(), 13);
        assert_eq!(values[11], "--resume-from");
        assert_eq!(values[12], std::path::absolute(&evidence).unwrap().as_os_str());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_not_lossily_retargeted() {
        use std::os::unix::ffi::OsStringExt;
        let mut args = arguments();
        args.db = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/source-\xff.db".to_vec()));
        let command = helper_command(&args).unwrap();
        assert_eq!(command.get_args().nth(6).unwrap(), args.db.as_os_str());
    }

    #[test]
    fn last_commit_copy_is_explicit_and_forwarded_once() {
        let args = Args::try_parse_from([
            "br-wal-recover", "--db", "source", "--output", "new", "--copy-last-commit",
        ])
        .unwrap();
        assert!(args.copy_last_commit);
        let command = helper_command(&args).unwrap();
        let values: Vec<_> = command.get_args().collect();
        assert_eq!(values.len(), 12);
        assert_eq!(values[11], "--copy-last-commit");
    }

    #[test]
    fn last_commit_copy_cannot_enter_live_restoration_or_resume() {
        for mode in [vec!["--restore-shm"], vec!["--resume-from", "prior"]] {
            let mut argv = vec![
                "br-wal-recover", "--db", "source", "--output", "new", "--copy-last-commit",
            ];
            argv.extend(mode);
            assert!(Args::try_parse_from(argv).is_err());
        }
        let mut args = arguments();
        args.copy_last_commit = true;
        args.restore_shm = true;
        assert!(helper_command(&args).is_err());
        args.restore_shm = false;
        args.resume_from = Some(PathBuf::from("prior"));
        assert!(helper_command(&args).is_err());
    }
}
