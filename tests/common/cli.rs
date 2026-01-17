use assert_cmd::Command;
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;

#[derive(Debug)]
pub struct BrRun {
    pub stdout: String,
    pub stderr: String,
    pub status: std::process::ExitStatus,
    pub duration: Duration,
    pub log_path: PathBuf,
}

pub struct BrWorkspace {
    pub temp_dir: TempDir,
    pub root: PathBuf,
    pub log_dir: PathBuf,
}

impl BrWorkspace {
    pub fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp dir");
        let root = temp_dir.path().to_path_buf();
        let log_dir = root.join("logs");
        fs::create_dir_all(&log_dir).expect("log dir");
        Self {
            temp_dir,
            root,
            log_dir,
        }
    }
}

pub fn run_br<I, S>(workspace: &BrWorkspace, args: I, label: &str) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(&workspace.root);
    cmd.args(args);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_LOG", "beads_rust=debug");
    cmd.env("RUST_BACKTRACE", "1");

    let start = Instant::now();
    let output = cmd.output().expect("run br");
    let duration = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let log_path = workspace.log_dir.join(format!("{label}.log"));
    let timestamp = SystemTime::now();
    let log_body = format!(
        "label: {label}\nstarted: {:?}\nduration: {:?}\nstatus: {}\nargs: {:?}\ncwd: {}\n\nstdout:\n{}\n\nstderr:\n{}\n",
        timestamp,
        duration,
        output.status,
        cmd.get_args().collect::<Vec<_>>(),
        workspace.root.display(),
        stdout,
        stderr
    );
    fs::write(&log_path, log_body).expect("write log");

    BrRun {
        stdout,
        stderr,
        status: output.status,
        duration,
        log_path,
    }
}

pub fn extract_json_payload(stdout: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            return lines[idx..].join("\n").trim().to_string();
        }
    }
    stdout.trim().to_string()
}
