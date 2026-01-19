//! Output context and mode detection.
//!
//! Determines whether to use Rich, Plain, JSON, or Quiet output
//! based on CLI flags and terminal detection.
//!
//! # Mode Selection Logic
//!
//! 1. `--json` flag → JSON mode (machine-readable)
//! 2. `--quiet` flag → Quiet mode (minimal output)
//! 3. `--no-color` flag or not a TTY → Plain mode
//! 4. TTY with colors → Rich mode
//!
//! # Integration with CLI
//!
//! The [`OutputContext`] works with CLI flags:
//!
//! ```ignore
//! let ctx = OutputContext::from_flags(json, quiet, no_color);
//! match ctx.mode() {
//!     OutputMode::Json => print_json(&data),
//!     OutputMode::Rich => print_rich(&data),
//!     OutputMode::Plain => print_plain(&data),
//!     OutputMode::Quiet => { /* minimal or no output */ }
//! }
//! ```

use std::io::IsTerminal;

use super::text::terminal_width;

/// Output mode determining formatting strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// Rich terminal output with colors, tables, panels, etc.
    /// Used when stdout is a TTY and colors are enabled.
    #[default]
    Rich,

    /// Plain text output without ANSI escape codes.
    /// Used when `--no-color` is specified or not a TTY.
    Plain,

    /// JSON output for machine consumption.
    /// Used when `--json` flag is specified.
    Json,

    /// Quiet mode with minimal output.
    /// Used when `--quiet` flag is specified.
    Quiet,
}

impl OutputMode {
    /// Returns true if this mode supports ANSI colors.
    #[must_use]
    pub const fn supports_color(&self) -> bool {
        matches!(self, Self::Rich)
    }

    /// Returns true if this mode produces structured data (JSON).
    #[must_use]
    pub const fn is_structured(&self) -> bool {
        matches!(self, Self::Json)
    }

    /// Returns true if this mode should minimize output.
    #[must_use]
    pub const fn is_quiet(&self) -> bool {
        matches!(self, Self::Quiet)
    }

    /// Returns true if this mode is for human reading.
    #[must_use]
    pub const fn is_human_readable(&self) -> bool {
        matches!(self, Self::Rich | Self::Plain)
    }
}

/// Output context providing mode detection and terminal info.
///
/// # Example
///
/// ```ignore
/// let ctx = OutputContext::detect();
/// if ctx.mode().supports_color() {
///     // Use rich formatting
/// }
/// ```
#[derive(Debug, Clone)]
pub struct OutputContext {
    mode: OutputMode,
    width: usize,
    height: Option<usize>,
    is_tty: bool,
}

impl Default for OutputContext {
    fn default() -> Self {
        Self::detect()
    }
}

impl OutputContext {
    /// Create a new output context with specified mode and dimensions.
    #[must_use]
    pub const fn new(mode: OutputMode, width: usize, height: Option<usize>, is_tty: bool) -> Self {
        Self {
            mode,
            width,
            height,
            is_tty,
        }
    }

    /// Detect output context from environment.
    ///
    /// Checks:
    /// - `stdout.is_terminal()` for TTY detection
    /// - `COLUMNS` environment variable for width
    /// - `NO_COLOR` environment variable
    #[must_use]
    pub fn detect() -> Self {
        let is_tty = std::io::stdout().is_terminal();
        let width = terminal_width();
        let height = terminal_height();

        // Check for NO_COLOR environment variable
        let no_color = std::env::var("NO_COLOR").is_ok();

        let mode = if no_color || !is_tty {
            OutputMode::Plain
        } else {
            OutputMode::Rich
        };

        Self {
            mode,
            width,
            height,
            is_tty,
        }
    }

    /// Create context for JSON output mode.
    #[must_use]
    pub fn json() -> Self {
        Self {
            mode: OutputMode::Json,
            ..Self::detect()
        }
    }

    /// Create context for quiet output mode.
    #[must_use]
    pub fn quiet() -> Self {
        Self {
            mode: OutputMode::Quiet,
            ..Self::detect()
        }
    }

    /// Create context with explicit mode override.
    #[must_use]
    pub fn with_mode(mode: OutputMode) -> Self {
        Self {
            mode,
            ..Self::detect()
        }
    }

    /// Create context from CLI flags.
    ///
    /// Priority order:
    /// 1. `json` → JSON mode
    /// 2. `quiet` → Quiet mode
    /// 3. `no_color` or not TTY → Plain mode
    /// 4. Otherwise → Rich mode
    #[must_use]
    pub fn from_flags(json: bool, quiet: bool, no_color: bool) -> Self {
        let is_tty = std::io::stdout().is_terminal();
        let width = terminal_width();
        let height = terminal_height();

        let mode = if json {
            OutputMode::Json
        } else if quiet {
            OutputMode::Quiet
        } else if no_color || !is_tty {
            OutputMode::Plain
        } else {
            OutputMode::Rich
        };

        Self {
            mode,
            width,
            height,
            is_tty,
        }
    }

    /// Get the current output mode.
    #[must_use]
    pub const fn mode(&self) -> OutputMode {
        self.mode
    }

    /// Get terminal width in columns.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Get terminal height in rows, if available.
    #[must_use]
    pub const fn height(&self) -> Option<usize> {
        self.height
    }

    /// Check if output is going to a TTY.
    #[must_use]
    pub const fn is_tty(&self) -> bool {
        self.is_tty
    }

    /// Check if colors are supported.
    #[must_use]
    pub const fn supports_color(&self) -> bool {
        self.mode.supports_color()
    }

    /// Override the output mode.
    #[must_use]
    pub const fn with_mode_override(mut self, mode: OutputMode) -> Self {
        self.mode = mode;
        self
    }

    /// Override the terminal width.
    #[must_use]
    pub const fn with_width(mut self, width: usize) -> Self {
        self.width = width;
        self
    }
}

/// Determine terminal height from environment.
#[must_use]
pub fn terminal_height() -> Option<usize> {
    // Try LINES first
    if let Ok(lines) = std::env::var("LINES") {
        if let Ok(value) = lines.trim().parse::<usize>() {
            if value > 0 {
                return Some(value);
            }
        }
    }

    // Try crossterm for actual terminal size
    if let Ok((_, rows)) = crossterm::terminal::size() {
        if rows > 0 {
            return Some(rows as usize);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_output_mode_supports_color() {
        assert!(OutputMode::Rich.supports_color());
        assert!(!OutputMode::Plain.supports_color());
        assert!(!OutputMode::Json.supports_color());
        assert!(!OutputMode::Quiet.supports_color());
    }

    #[test]
    fn test_output_mode_is_structured() {
        assert!(!OutputMode::Rich.is_structured());
        assert!(!OutputMode::Plain.is_structured());
        assert!(OutputMode::Json.is_structured());
        assert!(!OutputMode::Quiet.is_structured());
    }

    #[test]
    fn test_output_mode_is_human_readable() {
        assert!(OutputMode::Rich.is_human_readable());
        assert!(OutputMode::Plain.is_human_readable());
        assert!(!OutputMode::Json.is_human_readable());
        assert!(!OutputMode::Quiet.is_human_readable());
    }

    #[test]
    fn test_context_from_flags_json() {
        let ctx = OutputContext::from_flags(true, false, false);
        assert_eq!(ctx.mode(), OutputMode::Json);
    }

    #[test]
    fn test_context_from_flags_quiet() {
        let ctx = OutputContext::from_flags(false, true, false);
        assert_eq!(ctx.mode(), OutputMode::Quiet);
    }

    #[test]
    fn test_context_from_flags_no_color() {
        let ctx = OutputContext::from_flags(false, false, true);
        assert_eq!(ctx.mode(), OutputMode::Plain);
    }

    #[test]
    fn test_context_json_constructor() {
        let ctx = OutputContext::json();
        assert_eq!(ctx.mode(), OutputMode::Json);
    }

    #[test]
    fn test_context_quiet_constructor() {
        let ctx = OutputContext::quiet();
        assert_eq!(ctx.mode(), OutputMode::Quiet);
    }

    #[test]
    fn test_context_width_override() {
        let ctx = OutputContext::detect().with_width(120);
        assert_eq!(ctx.width(), 120);
    }

    #[test]
    fn test_context_mode_override() {
        let ctx = OutputContext::detect().with_mode_override(OutputMode::Plain);
        assert_eq!(ctx.mode(), OutputMode::Plain);
    }

    #[test]
    fn test_terminal_width_fallback() {
        // Should return at least 80 (the fallback)
        let width = terminal_width();
        assert!(width >= 80);
    }
}
