//! Progress indicator utilities for long-running operations.
//!
//! Provides:
//! - Determinate progress bars for known-count operations
//! - Spinners for indeterminate operations
//! - Multi-progress for parallel operations
//! - Conditional display based on terminal detection

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::io::{IsTerminal, stderr};
use std::time::Duration;

/// Check if we should show progress indicators.
///
/// Progress is shown only if stderr is an interactive terminal.
/// This respects piped output and non-interactive environments.
#[must_use]
pub fn should_show_progress() -> bool {
    stderr().is_terminal()
}

/// Create a determinate progress bar for operations with known total count.
///
/// # Arguments
/// * `total` - Total number of items to process
/// * `message` - Initial message to display
/// * `show` - Whether to actually show the progress bar (use `should_show_progress()`)
///
/// # Panics
/// Panics if the progress bar template string is invalid.
///
/// # Example
/// ```ignore
/// let pb = create_progress_bar(issues.len() as u64, "Exporting issues", should_show_progress());
/// for issue in issues {
///     // ... process issue
///     pb.inc(1);
/// }
/// pb.finish_with_message("Export complete");
/// ```
#[must_use]
pub fn create_progress_bar(total: u64, message: &str, show: bool) -> ProgressBar {
    let pb = ProgressBar::new(total);

    if show {
        let style = ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=>-");
        
        pb.set_style(style);
        pb.set_message(message.to_string());
    } else {
        pb.set_draw_target(ProgressDrawTarget::hidden());
    }

    pb
}

/// Create a spinner for indeterminate operations.
///
/// # Arguments
/// * `message` - Message to display alongside the spinner
/// * `show` - Whether to actually show the spinner (use `should_show_progress()`)
///
/// # Panics
/// Panics if the spinner template string is invalid.
///
/// # Example
/// ```ignore
/// let spinner = create_spinner("Scanning git history...", should_show_progress());
/// // ... long operation
/// spinner.finish_with_message("Scan complete");
/// ```
#[must_use]
pub fn create_spinner(message: &str, show: bool) -> ProgressBar {
    let pb = ProgressBar::new_spinner();

    if show {
        let style = ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner());
            
        pb.set_style(style);
        pb.set_message(message.to_string());
        pb.enable_steady_tick(Duration::from_millis(100));
    } else {
        pb.set_draw_target(ProgressDrawTarget::hidden());
    }

    pb
}

/// Create a multi-progress container for parallel operations.
///
/// # Arguments
/// * `show` - Whether to actually show progress (use `should_show_progress()`)
///
/// # Example
/// ```ignore
/// let multi = create_multi_progress(should_show_progress());
/// let pb1 = multi.add(create_progress_bar(100, "Task 1", true));
/// let pb2 = multi.add(create_progress_bar(50, "Task 2", true));
/// // ... run parallel operations
/// ```
#[must_use]
pub fn create_multi_progress(show: bool) -> MultiProgress {
    let multi = MultiProgress::new();

    if !show {
        multi.set_draw_target(ProgressDrawTarget::hidden());
    }

    multi
}

/// Progress bar wrapper that tracks whether we're showing output.
///
/// This is useful for conditionally showing progress without
/// checking `should_show_progress()` on every operation.
pub struct ProgressTracker {
    bar: ProgressBar,
    showing: bool,
}

impl ProgressTracker {
    /// Create a new progress tracker with a determinate total.
    #[must_use]
    pub fn new(total: u64, message: &str) -> Self {
        let showing = should_show_progress();
        Self {
            bar: create_progress_bar(total, message, showing),
            showing,
        }
    }

    /// Create a new spinner tracker for indeterminate operations.
    #[must_use]
    pub fn new_spinner(message: &str) -> Self {
        let showing = should_show_progress();
        Self {
            bar: create_spinner(message, showing),
            showing,
        }
    }

    /// Increment the progress by one.
    pub fn inc(&self, delta: u64) {
        self.bar.inc(delta);
    }

    /// Set the current position.
    pub fn set_position(&self, pos: u64) {
        self.bar.set_position(pos);
    }

    /// Update the message.
    pub fn set_message(&self, message: impl Into<String>) {
        self.bar.set_message(message.into());
    }

    /// Finish with a message.
    pub fn finish_with_message(&self, message: impl Into<String>) {
        self.bar.finish_with_message(message.into());
    }

    /// Finish and clear the progress bar.
    pub fn finish_and_clear(&self) {
        self.bar.finish_and_clear();
    }

    /// Check if we're actually showing progress.
    #[must_use]
    pub const fn is_showing(&self) -> bool {
        self.showing
    }

    /// Get the underlying progress bar.
    #[must_use]
    pub const fn bar(&self) -> &ProgressBar {
        &self.bar
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_bar_hidden_when_not_terminal() {
        // In tests, stderr is typically not a terminal
        let pb = create_progress_bar(100, "Test", false);
        pb.inc(50);
        pb.finish();
        // Should not panic or produce output
    }

    #[test]
    fn test_spinner_hidden_when_not_terminal() {
        let spinner = create_spinner("Testing...", false);
        spinner.finish();
        // Should not panic or produce output
    }

    #[test]
    fn test_progress_tracker_determinate() {
        let tracker = ProgressTracker::new(10, "Processing");
        for _ in 0..10 {
            tracker.inc(1);
        }
        tracker.finish_with_message("Done");
    }

    #[test]
    fn test_progress_tracker_spinner() {
        let tracker = ProgressTracker::new_spinner("Loading...");
        tracker.set_message("Still loading...");
        tracker.finish_and_clear();
    }

    #[test]
    fn test_multi_progress_hidden() {
        let multi = create_multi_progress(false);
        let pb = multi.add(create_progress_bar(10, "Test", false));
        pb.inc(5);
        pb.finish();
    }
}
