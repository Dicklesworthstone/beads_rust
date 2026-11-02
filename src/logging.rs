//! Logging configuration and initialization.
//!
//! Uses tracing with environment-based filtering and optional JSON file output.

use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once};

use crate::error::{BeadsError, Result};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Bound verbose engine diagnostics without making a stalled stderr consumer
/// part of database-family authority admission or teardown (GH #586).
const READ_STARTUP_DIAGNOSTIC_LIMIT: usize = 256 * 1024;
const READ_STARTUP_DIAGNOSTIC_HEAD: usize = READ_STARTUP_DIAGNOSTIC_LIMIT / 2;
const READ_STARTUP_DIAGNOSTIC_TAIL: usize =
    READ_STARTUP_DIAGNOSTIC_LIMIT - READ_STARTUP_DIAGNOSTIC_HEAD;

struct DeferredDiagnostics {
    scopes: Arc<()>,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    discarded_bytes: usize,
}

impl DeferredDiagnostics {
    fn record(&mut self, bytes: &[u8]) {
        let head_length = (READ_STARTUP_DIAGNOSTIC_HEAD - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_length]);
        let bytes = &bytes[head_length..];
        // Keep both the initial admission reason and the final import/close
        // outcome. A verbose per-row trace must not displace either boundary.
        if bytes.len() >= READ_STARTUP_DIAGNOSTIC_TAIL {
            self.discarded_bytes = self
                .discarded_bytes
                .saturating_add(self.tail.len())
                .saturating_add(bytes.len() - READ_STARTUP_DIAGNOSTIC_TAIL);
            self.tail.clear();
            self.tail.extend(
                bytes[bytes.len() - READ_STARTUP_DIAGNOSTIC_TAIL..]
                    .iter()
                    .copied(),
            );
        } else {
            let displaced =
                (self.tail.len() + bytes.len()).saturating_sub(READ_STARTUP_DIAGNOSTIC_TAIL);
            self.tail.drain(..displaced);
            self.discarded_bytes = self.discarded_bytes.saturating_add(displaced);
            self.tail.extend(bytes.iter().copied());
        }
    }
}

struct DiagnosticBufferState {
    deferred: Option<DeferredDiagnostics>,
    active_outputs: usize,
}

struct DiagnosticState {
    buffer: Mutex<DiagnosticBufferState>,
    output_idle: Condvar,
}

impl DiagnosticState {
    const fn new() -> Self {
        Self {
            buffer: Mutex::new(DiagnosticBufferState {
                deferred: None,
                active_outputs: 0,
            }),
            output_idle: Condvar::new(),
        }
    }
}

static READ_STARTUP_DIAGNOSTICS: DiagnosticState = DiagnosticState::new();

fn diagnostic_state(state: &DiagnosticState) -> MutexGuard<'_, DiagnosticBufferState> {
    // Logging must remain usable while another scope unwinds. A poisoned
    // diagnostics buffer is not authority to panic during storage cleanup.
    state
        .buffer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct ActiveDiagnosticOutput<'a> {
    state: &'a DiagnosticState,
}

impl Drop for ActiveDiagnosticOutput<'_> {
    fn drop(&mut self) {
        let mut state = diagnostic_state(self.state);
        state.active_outputs = state.active_outputs.saturating_sub(1);
        drop(state);
        self.state.output_idle.notify_all();
    }
}

/// Defer configured tracing stderr output and routed operator diagnostics until
/// every read-startup owner has released its database authority. Nested/overlapping
/// scopes share a bounded buffer, including records emitted by engine threads.
///
/// Declare this guard before storage and authority owners, so error unwinding
/// closes them before the guard flushes. Call [`Self::finish`] only after their
/// successful read-only handoff. Unrouted stdout/stderr writes and the optional
/// JSON file layer are outside this stderr contract; the CLI configures no file
/// layer. Diagnostic I/O remains best-effort, like ordinary tracing.
#[must_use = "keep the diagnostic guard alive until read startup releases authority"]
pub struct ReadStartupDiagnostics<'a, W: Write = io::Stderr> {
    state: &'a DiagnosticState,
    scope: Option<Arc<()>>,
    output: W,
}

impl<'a, W: Write> ReadStartupDiagnostics<'a, W> {
    fn start(state: &'a DiagnosticState, output: W) -> Self {
        let scope = {
            let mut buffer = diagnostic_state(state);
            let pending = buffer.deferred.get_or_insert_with(|| DeferredDiagnostics {
                scopes: Arc::new(()),
                head: Vec::new(),
                tail: VecDeque::new(),
                discarded_bytes: 0,
            });
            let scope = Arc::clone(&pending.scopes);
            // A writer may already have selected direct output before this
            // scope activated buffering. Wait before the caller can acquire
            // authority; new writers now append to the buffer instead. Every
            // joining scope waits too, including while the first one waits.
            while buffer.active_outputs > 0 {
                buffer = state
                    .output_idle
                    .wait(buffer)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            drop(buffer);
            scope
        };
        Self {
            state,
            scope: Some(scope),
            output,
        }
    }

    /// End this scope and publish its retained diagnostics if no other
    /// read-startup scope remains. Output may block, so authority must already
    /// have been released.
    pub fn finish(self) {
        drop(self);
    }

    fn flush_deferred(&mut self) -> io::Result<()> {
        let Some(scope) = self.scope.take() else {
            return Ok(());
        };
        let pending = {
            let mut buffer = diagnostic_state(self.state);
            let same_scope = buffer
                .deferred
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(&pending.scopes, &scope));
            // Retire this token under the mutex as well as checking it.
            // Concurrent final guards must not each count the other's token
            // and leave a buffer with no guard responsible for flushing it.
            drop(scope);
            if same_scope
                && buffer
                    .deferred
                    .as_ref()
                    .is_some_and(|pending| Arc::strong_count(&pending.scopes) == 1)
            {
                buffer.deferred.take().map(|pending| {
                    buffer.active_outputs += 1;
                    (pending, ActiveDiagnosticOutput { state: self.state })
                })
            } else {
                None
            }
        };
        let Some((mut pending, _active_output)) = pending else {
            return Ok(());
        };

        if pending.discarded_bytes == 0 {
            // Without a gap, a code point may cross the head/tail boundary.
            // Rejoin those original bytes before validating UTF-8.
            pending.head.extend(pending.tail.drain(..));
        }
        // Either edge of an omitted middle can split a UTF-8 code point.
        // Remove incomplete fragments and include them in the byte notice.
        let head_length =
            std::str::from_utf8(&pending.head).map_or_else(|error| error.valid_up_to(), str::len);
        let tail = pending.tail.make_contiguous();
        let tail_start = tail
            .iter()
            .position(|byte| byte & 0xc0 != 0x80)
            .unwrap_or(tail.len());
        let tail_length = std::str::from_utf8(&tail[tail_start..])
            .map_or_else(|error| error.valid_up_to(), str::len);
        let discarded_bytes = pending
            .discarded_bytes
            .saturating_add(pending.head.len() - head_length)
            .saturating_add(tail.len() - tail_length);
        self.output.write_all(&pending.head[..head_length])?;
        if discarded_bytes > 0 {
            writeln!(
                self.output,
                "\nwarning: {discarded_bytes} diagnostic bytes were omitted while read startup \
                 diagnostics were deferred (first and last {} bytes retained)\n",
                READ_STARTUP_DIAGNOSTIC_HEAD,
            )?;
        }
        self.output
            .write_all(&tail[tail_start..tail_start + tail_length])?;
        self.output.flush()
    }
}

impl<W: Write> Drop for ReadStartupDiagnostics<'_, W> {
    fn drop(&mut self) {
        if let Err(error) = self.flush_deferred() {
            // Match tracing's best-effort internal error reporting without
            // changing the command result or panicking during an error unwind.
            let _ = writeln!(
                StartupDiagnosticWriter {
                    state: self.state,
                    output: io::stderr(),
                },
                "Unable to write deferred read startup diagnostics: {error}"
            );
        }
    }
}

/// Start bounded stderr-diagnostic deferral for a read command's startup.
pub fn defer_read_startup_diagnostics() -> ReadStartupDiagnostics<'static> {
    ReadStartupDiagnostics::start(&READ_STARTUP_DIAGNOSTICS, io::stderr())
}

struct StartupDiagnosticWriter<'a, W> {
    state: &'a DiagnosticState,
    output: W,
}

impl<W: Write> Write for StartupDiagnosticWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _active_output = {
            let mut buffer = diagnostic_state(self.state);
            if let Some(pending) = buffer.deferred.as_mut() {
                pending.record(bytes);
                return Ok(bytes.len());
            }
            buffer.active_outputs += 1;
            let active_output = ActiveDiagnosticOutput { state: self.state };
            drop(buffer);
            active_output
        };
        self.output.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        let _active_output = {
            let mut buffer = diagnostic_state(self.state);
            if buffer.deferred.is_some() {
                return Ok(());
            }
            buffer.active_outputs += 1;
            let active_output = ActiveDiagnosticOutput { state: self.state };
            drop(buffer);
            active_output
        };
        self.output.flush()
    }
}

fn startup_diagnostic_writer() -> StartupDiagnosticWriter<'static, io::Stderr> {
    StartupDiagnosticWriter {
        state: &READ_STARTUP_DIAGNOSTICS,
        output: io::stderr(),
    }
}

/// Emit an operator-facing stderr diagnostic independently of tracing filters.
/// Read startup defers these bytes through the same bounded buffer as tracing,
/// so a reloaded policy warning cannot block while writer authority is held.
/// Outside read startup, delivery is immediate and best-effort.
pub(crate) fn emit_stderr_diagnostic(message: &str) {
    let _ = startup_diagnostic_writer().write_all(message.as_bytes());
}

/// Initialize logging for the CLI.
///
/// Logging honors `RUST_LOG` if set; otherwise a default filter is used based
/// on verbosity and quiet flags. ANSI styling also honors `--no-color`,
/// nonempty `NO_COLOR`, and `TERM=dumb` without suppressing log messages.
///
/// # Errors
///
/// Returns an error if logging initialization fails.
pub fn init_logging(
    verbosity: u8,
    quiet: bool,
    no_color: bool,
    log_file: Option<&Path>,
) -> Result<()> {
    let env_filter = resolve_env_filter(verbosity, quiet)?;
    let color = !no_color
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && std::env::var("TERM").as_deref() != Ok("dumb")
        && std::io::stderr().is_terminal();

    let fmt_layer = fmt::layer()
        .with_writer(startup_diagnostic_writer)
        .with_target(true)
        .with_level(true)
        .with_file(cfg!(debug_assertions))
        .with_line_number(cfg!(debug_assertions))
        .with_ansi(color);

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer);

    if let Some(path) = log_file {
        let file = std::fs::File::create(path)?;
        let file_layer = fmt::layer()
            .with_writer(Mutex::new(file))
            .with_ansi(false)
            .json();
        tracing::subscriber::set_global_default(subscriber.with(file_layer)).map_err(|err| {
            BeadsError::internal(format!("failed to initialize tracing subscriber: {err}"))
        })?;
    } else {
        tracing::subscriber::set_global_default(subscriber).map_err(|err| {
            BeadsError::internal(format!("failed to initialize tracing subscriber: {err}"))
        })?;
    }

    Ok(())
}

fn resolve_env_filter(verbosity: u8, quiet: bool) -> Result<EnvFilter> {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(default_filter(verbosity, quiet)))
        .map_err(|err| BeadsError::Config(format!("failed to build log filter: {err}")))?;
    Ok(filter)
}

#[cfg(test)]
fn resolve_env_filter_with_override(
    verbosity: u8,
    quiet: bool,
    env_override: Option<&str>,
) -> Result<EnvFilter> {
    if let Some(value) = env_override {
        let filter = EnvFilter::try_new(value)
            .or_else(|_| EnvFilter::try_new(default_filter(verbosity, quiet)))
            .map_err(|err| BeadsError::Config(format!("failed to build log filter: {err}")))?;
        return Ok(filter);
    }
    resolve_env_filter(verbosity, quiet)
}

fn default_filter(verbosity: u8, quiet: bool) -> String {
    if quiet {
        return "error".to_string();
    }

    // fsqlite's internal submodules (btree cells, VDBE steps, cx checkpoints,
    // pager I/O) fire at `debug` for every row and page touched — enabling
    // them unfiltered drowns out beads_rust's own logs by many orders of
    // magnitude on bulk imports. Keep fsqlite at `error` for default debug
    // builds; `-v` raises it to `warn`, and `-vv`/higher opt into more detail.
    match verbosity {
        0 => {
            if cfg!(debug_assertions) {
                "beads_rust=debug,fsqlite=error".to_string()
            } else {
                "error".to_string()
            }
        }
        1 => "beads_rust=debug,fsqlite=warn".to_string(),
        2 => {
            "beads_rust=debug,fsqlite=info,fsqlite_btree=warn,fsqlite_vdbe=warn,fsqlite_pager=warn"
                .to_string()
        }
        _ => "beads_rust=trace,fsqlite=debug,fsqlite_btree=info,fsqlite_vdbe=info".to_string(),
    }
}

/// Initialize logging for tests with the test writer.
pub fn init_test_logging() {
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter("beads_rust=debug,test=debug")
            .with_test_writer()
            .try_init()
            .ok();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, Once, TryLockError, mpsc};
    use std::time::{Duration, Instant};

    static INIT_LOGGING: Once = Once::new();

    #[derive(Clone)]
    struct RecordedOutput<'a> {
        state: &'a DiagnosticState,
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl RecordedOutput<'_> {
        fn assert_buffer_released(&self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match self.state.buffer.try_lock() {
                    Ok(_) | Err(TryLockError::Poisoned(_)) => return,
                    Err(TryLockError::WouldBlock) => {
                        // Another scope may briefly own the mutex. Output on
                        // this thread must never retain it across sink I/O.
                        assert!(Instant::now() < deadline, "buffer mutex held during output");
                        std::thread::yield_now();
                    }
                }
            }
        }
    }

    impl Write for RecordedOutput<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.assert_buffer_released();
            self.bytes
                .lock()
                .expect("recorded output")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.assert_buffer_released();
            Ok(())
        }
    }

    struct ReleaseOnDrop(Arc<(Mutex<bool>, Condvar)>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let (released, changed) = &*self.0;
            *released.lock().expect("release state") = true;
            changed.notify_all();
        }
    }

    struct BlockingOutput {
        entered: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Write for BlockingOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.entered.send(()).expect("output barrier");
            let (release, changed) = &*self.release;
            let mut released = release.lock().expect("release state");
            while !*released {
                released = changed.wait(released).expect("release wait");
            }
            drop(released);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn startup_diagnostics_pass_through_outside_scope_and_defer_existing_writer() {
        let state = DiagnosticState::new();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = RecordedOutput {
            state: &state,
            bytes: Arc::clone(&bytes),
        };
        let mut writer = StartupDiagnosticWriter {
            state: &state,
            output: output.clone(),
        };
        writer.write_all(b"before\n").expect("ordinary output");
        writer.flush().expect("ordinary flush");
        let guard = ReadStartupDiagnostics::start(&state, output);
        writer.write_all(b"protected\n").expect("deferred output");
        writer.flush().expect("deferred flush");
        assert_eq!(*bytes.lock().expect("bytes"), b"before\n");
        guard.finish();
        writer.write_all(b"after\n").expect("resumed output");
        assert_eq!(*bytes.lock().expect("bytes"), b"before\nprotected\nafter\n");
    }

    #[test]
    fn startup_diagnostics_share_nested_scope_across_threads() {
        let state = DiagnosticState::new();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = RecordedOutput {
            state: &state,
            bytes: Arc::clone(&bytes),
        };
        let outer = ReadStartupDiagnostics::start(&state, output.clone());
        let inner = ReadStartupDiagnostics::start(&state, output.clone());
        std::thread::scope(|threads| {
            threads
                .spawn(|| {
                    StartupDiagnosticWriter {
                        state: &state,
                        output,
                    }
                    .write_all(b"engine thread\n")
                    .expect("deferred worker output");
                })
                .join()
                .expect("worker");
        });
        outer.finish();
        assert!(bytes.lock().expect("bytes").is_empty());
        inner.finish();
        assert_eq!(*bytes.lock().expect("bytes"), b"engine thread\n");
        assert!(diagnostic_state(&state).deferred.is_none());
    }

    #[test]
    fn startup_diagnostics_concurrent_final_scopes_flush_once() {
        for _ in 0..32 {
            let state = DiagnosticState::new();
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let output = RecordedOutput {
                state: &state,
                bytes: Arc::clone(&bytes),
            };
            let first = ReadStartupDiagnostics::start(&state, output.clone());
            let second = ReadStartupDiagnostics::start(&state, output.clone());
            StartupDiagnosticWriter {
                state: &state,
                output,
            }
            .write_all(b"one retained event\n")
            .expect("deferred output");
            let finish = Barrier::new(3);
            std::thread::scope(|threads| {
                let finish = &finish;
                threads.spawn(move || {
                    finish.wait();
                    first.finish();
                });
                threads.spawn(move || {
                    finish.wait();
                    second.finish();
                });
                finish.wait();
            });
            assert_eq!(*bytes.lock().expect("bytes"), b"one retained event\n");
            let state = diagnostic_state(&state);
            assert!(state.deferred.is_none());
            assert_eq!(state.active_outputs, 0);
        }
    }

    #[test]
    fn startup_diagnostics_bound_bytes_without_splitting_utf8() {
        const EARLY: &[u8] = b"early startup marker\n";
        const LATE: &[u8] = b"final startup marker\n";
        let state = DiagnosticState::new();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = RecordedOutput {
            state: &state,
            bytes: Arc::clone(&bytes),
        };
        let guard = ReadStartupDiagnostics::start(&state, output.clone());
        let mut writer = StartupDiagnosticWriter {
            state: &state,
            output,
        };
        writer.write_all(EARLY).expect("early marker");
        writer
            .write_all(&vec![b'a'; READ_STARTUP_DIAGNOSTIC_HEAD - EARLY.len() - 1])
            .expect("head padding");
        writer.write_all("é".as_bytes()).expect("split head edge");
        writer.write_all(&[b'x'; 13]).expect("omitted middle");
        writer.write_all("é".as_bytes()).expect("split tail edge");
        writer
            .write_all(&vec![b'b'; READ_STARTUP_DIAGNOSTIC_TAIL - LATE.len() - 1])
            .expect("tail padding");
        writer.write_all(LATE).expect("final marker");
        {
            let state = diagnostic_state(&state);
            let pending = state.deferred.as_ref().expect("pending buffer");
            assert_eq!(pending.head.len(), READ_STARTUP_DIAGNOSTIC_HEAD);
            assert_eq!(pending.tail.len(), READ_STARTUP_DIAGNOSTIC_TAIL);
            assert_eq!(pending.discarded_bytes, 15);
            drop(state);
        }
        assert!(bytes.lock().expect("bytes").is_empty());
        guard.finish();
        let bytes = bytes.lock().expect("bytes");
        let text = std::str::from_utf8(&bytes).expect("complete UTF-8 output");
        assert!(bytes.starts_with(EARLY));
        assert!(bytes.ends_with(LATE));
        assert!(text.contains("17 diagnostic bytes were omitted"));
        assert!(!text.contains('�'));
        drop(bytes);
    }

    #[test]
    fn startup_diagnostics_preserve_utf8_across_an_uncut_head_tail_boundary() {
        let state = DiagnosticState::new();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = RecordedOutput {
            state: &state,
            bytes: Arc::clone(&bytes),
        };
        let guard = ReadStartupDiagnostics::start(&state, output.clone());
        let mut original = vec![b'a'; READ_STARTUP_DIAGNOSTIC_HEAD - 1];
        original.extend("é".as_bytes());
        StartupDiagnosticWriter {
            state: &state,
            output,
        }
        .write_all(&original)
        .expect("uncut diagnostic");
        guard.finish();
        assert_eq!(*bytes.lock().expect("bytes"), original);
    }

    #[test]
    fn startup_diagnostics_error_unwind_flushes_after_owner_cleanup() {
        struct Owner<'a> {
            output: RecordedOutput<'a>,
        }

        impl Drop for Owner<'_> {
            fn drop(&mut self) {
                assert!(self.output.bytes.lock().expect("bytes").is_empty());
                StartupDiagnosticWriter {
                    state: self.output.state,
                    output: self.output.clone(),
                }
                .write_all(b"protected teardown\n")
                .expect("teardown diagnostic");
            }
        }

        fn fail_after_open(output: RecordedOutput<'_>) -> io::Result<()> {
            let _diagnostics = ReadStartupDiagnostics::start(output.state, output.clone());
            let _owner = Owner { output };
            Err(io::Error::other("startup refused"))
        }

        let state = DiagnosticState::new();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let error = fail_after_open(RecordedOutput {
            state: &state,
            bytes: Arc::clone(&bytes),
        })
        .expect_err("startup must refuse");
        assert_eq!(error.to_string(), "startup refused");
        assert_eq!(*bytes.lock().expect("bytes"), b"protected teardown\n");
        assert!(diagnostic_state(&state).deferred.is_none());
    }

    #[test]
    fn startup_diagnostics_activation_waits_for_existing_output_in_every_scope() {
        let state = DiagnosticState::new();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = RecordedOutput {
            state: &state,
            bytes,
        };
        let (entered, entry) = mpsc::channel();
        let (started, starts) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let finish = Arc::new((Mutex::new(false), Condvar::new()));
        std::thread::scope(|threads| {
            // Failure must unblock both sets of child threads before the
            // scope joins them; otherwise a regression would hang its test.
            let release_output = ReleaseOnDrop(Arc::clone(&release));
            let release_scopes = ReleaseOnDrop(Arc::clone(&finish));
            let state = &state;
            let sink_release = Arc::clone(&release);
            threads.spawn(move || {
                StartupDiagnosticWriter {
                    state,
                    output: BlockingOutput {
                        entered,
                        release: sink_release,
                    },
                }
                .write_all(b"pre-existing output\n")
                .expect("unblocked output");
            });
            entry
                .recv_timeout(Duration::from_secs(5))
                .expect("writer entered sink");
            for _ in 0..2 {
                let output = output.clone();
                let started = started.clone();
                let finish = Arc::clone(&finish);
                threads.spawn(move || {
                    let guard = ReadStartupDiagnostics::start(state, output);
                    started.send(()).expect("scope started");
                    let (finished, changed) = &*finish;
                    let mut finished = finished.lock().expect("finish state");
                    while !*finished {
                        finished = changed.wait(finished).expect("finish wait");
                    }
                    drop(finished);
                    guard.finish();
                });
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let joined = diagnostic_state(state)
                    .deferred
                    .as_ref()
                    .is_some_and(|pending| Arc::strong_count(&pending.scopes) == 3);
                if joined {
                    break;
                }
                assert!(Instant::now() < deadline, "both scopes must join");
                std::thread::yield_now();
            }
            assert!(matches!(starts.try_recv(), Err(mpsc::TryRecvError::Empty)));
            drop(release_output);
            for _ in 0..2 {
                starts
                    .recv_timeout(Duration::from_secs(5))
                    .expect("scope starts after output finishes");
            }
            drop(release_scopes);
        });
        let state = diagnostic_state(&state);
        assert!(state.deferred.is_none());
        assert_eq!(state.active_outputs, 0);
    }

    #[test]
    fn startup_diagnostics_recovers_poisoned_state_without_panicking_on_drop() {
        let state = DiagnosticState::new();
        assert!(
            std::panic::catch_unwind(|| {
                let _locked = state.buffer.lock().expect("initial state");
                panic!("poison diagnostic state");
            })
            .is_err()
        );
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = RecordedOutput {
            state: &state,
            bytes: Arc::clone(&bytes),
        };
        let guard = ReadStartupDiagnostics::start(&state, output.clone());
        StartupDiagnosticWriter {
            state: &state,
            output,
        }
        .write_all(b"after poison\n")
        .expect("poisoned-state logging");
        drop(guard);
        assert_eq!(*bytes.lock().expect("bytes"), b"after poison\n");
        assert_eq!(diagnostic_state(&state).active_outputs, 0);
    }

    #[test]
    fn startup_diagnostics_output_panic_does_not_strand_activation() {
        struct PanickingOutput;

        impl Write for PanickingOutput {
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                panic!("output failed");
            }

            fn flush(&mut self) -> io::Result<()> {
                panic!("flush failed");
            }
        }

        let state = DiagnosticState::new();
        for flush in [false, true] {
            assert!(
                std::panic::catch_unwind(|| {
                    let mut writer = StartupDiagnosticWriter {
                        state: &state,
                        output: PanickingOutput,
                    };
                    if flush {
                        writer.flush().expect("flush");
                    } else {
                        writer.write_all(b"event\n").expect("write");
                    }
                })
                .is_err()
            );
            assert_eq!(diagnostic_state(&state).active_outputs, 0);
        }
        let guard = ReadStartupDiagnostics::start(&state, Vec::new());
        guard.finish();
        assert!(diagnostic_state(&state).deferred.is_none());
    }

    #[test]
    fn default_filter_respects_quiet() {
        assert_eq!(default_filter(0, true), "error");
    }

    #[test]
    fn default_filter_varies_with_verbosity() {
        assert_eq!(default_filter(1, false), "beads_rust=debug,fsqlite=warn");
        #[cfg(debug_assertions)]
        assert_eq!(default_filter(0, false), "beads_rust=debug,fsqlite=error");
        #[cfg(not(debug_assertions))]
        assert_eq!(default_filter(0, false), "error");
        assert_eq!(
            default_filter(2, false),
            "beads_rust=debug,fsqlite=info,fsqlite_btree=warn,fsqlite_vdbe=warn,fsqlite_pager=warn"
        );
        assert_eq!(
            default_filter(3, false),
            "beads_rust=trace,fsqlite=debug,fsqlite_btree=info,fsqlite_vdbe=info"
        );
    }

    #[test]
    fn resolve_env_filter_prefers_rust_log() {
        let filter =
            resolve_env_filter_with_override(0, false, Some("beads_rust=trace")).expect("filter");
        let rendered = filter.to_string();
        assert!(
            rendered.contains("beads_rust=trace"),
            "expected env override to include trace, got {rendered}"
        );
    }

    #[test]
    fn resolve_env_filter_falls_back_on_invalid_env() {
        // Use a string that definitely fails to parse (unbalanced brackets)
        let filter =
            resolve_env_filter_with_override(1, false, Some("[invalid")).expect("fallback filter");
        let rendered = filter.to_string();
        assert!(
            rendered.contains("beads_rust=debug"),
            "expected fallback filter, got {rendered}"
        );
    }

    #[test]
    fn init_test_logging_is_idempotent() {
        init_test_logging();
        init_test_logging();
    }

    #[test]
    fn init_logging_does_not_panic() {
        let result = std::panic::catch_unwind(|| {
            INIT_LOGGING.call_once(|| {
                let temp = tempfile::NamedTempFile::new().expect("temp log file");
                let result = init_logging(0, false, false, Some(temp.path()));
                if let Err(err) = result {
                    let message = err.to_string();
                    let is_already_set = message.contains("global")
                        || message.contains("already")
                        || message.contains("set");
                    assert!(is_already_set, "unexpected init_logging error: {message}");
                }
            });
        });
        assert!(result.is_ok());
    }
}
