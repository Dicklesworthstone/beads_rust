use super::Theme;
use crate::cli::{Cli, OutputFormat};
use rich_rust::prelude::*;
use rich_rust::renderables::Renderable;
use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;
use toon_rust::options::KeyFoldingMode;
use toon_rust::{EncodeOptions, JsonValue, encode};

/// Central output coordinator that respects robot/json/quiet modes.
///
/// Uses lazy initialization for console and theme to ensure zero overhead
/// in JSON/Quiet modes where rich output is never used.
pub struct OutputContext {
    /// Output mode (always set eagerly - cheap)
    mode: OutputMode,
    /// Terminal width (cached, lazy)
    width: OnceLock<usize>,
    /// Rich console for human-readable output (lazy)
    console: OnceLock<Console>,
    /// Theme for consistent styling (lazy)
    theme: OnceLock<Theme>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Full rich formatting (tables, colors, panels)
    Rich,
    /// Plain text, no ANSI codes (for piping)
    Plain,
    /// JSON output only
    Json,
    /// TOON format (token-optimized object notation)
    Toon,
    /// Minimal output (quiet mode)
    Quiet,
}

impl OutputContext {
    /// Create from CLI global args.
    ///
    /// Only mode is set eagerly; console/theme/width are lazy-initialized
    /// on first access to ensure zero overhead in JSON/Quiet modes.
    #[must_use]
    pub fn from_args(args: &Cli) -> Self {
        Self {
            mode: Self::detect_mode(args),
            width: OnceLock::new(),
            console: OnceLock::new(),
            theme: OnceLock::new(),
        }
    }

    /// Create from CLI-style flags.
    ///
    /// Only mode is set eagerly; console/theme/width are lazy-initialized
    /// on first access to ensure zero overhead in JSON/Quiet modes.
    #[must_use]
    pub fn from_flags(json: bool, quiet: bool, no_color: bool) -> Self {
        let mode = if json {
            OutputMode::Json
        } else if quiet {
            OutputMode::Quiet
        } else if no_color || std::env::var("NO_COLOR").is_ok() || !std::io::stdout().is_terminal()
        {
            OutputMode::Plain
        } else {
            OutputMode::Rich
        };

        Self {
            mode,
            width: OnceLock::new(),
            console: OnceLock::new(),
            theme: OnceLock::new(),
        }
    }

    /// Create from an explicit output format.
    #[must_use]
    pub fn from_output_format(format: OutputFormat, quiet: bool, no_color: bool) -> Self {
        let mode = match format {
            OutputFormat::Json => OutputMode::Json,
            OutputFormat::Toon => OutputMode::Toon,
            OutputFormat::Text | OutputFormat::Csv => {
                if quiet {
                    OutputMode::Quiet
                } else if no_color
                    || std::env::var("NO_COLOR").is_ok()
                    || !std::io::stdout().is_terminal()
                {
                    OutputMode::Plain
                } else {
                    OutputMode::Rich
                }
            }
        };

        Self {
            mode,
            width: OnceLock::new(),
            console: OnceLock::new(),
            theme: OnceLock::new(),
        }
    }

    fn detect_mode(args: &Cli) -> OutputMode {
        if args.json {
            return OutputMode::Json;
        }
        if args.quiet {
            return OutputMode::Quiet;
        }
        if args.no_color || std::env::var("NO_COLOR").is_ok() {
            return OutputMode::Plain;
        }
        if !std::io::stdout().is_terminal() {
            return OutputMode::Plain;
        }
        OutputMode::Rich
    }

    /// Lazily create console based on mode.
    fn console(&self) -> &Console {
        self.console.get_or_init(|| match self.mode {
            OutputMode::Rich => Console::new(),
            OutputMode::Plain | OutputMode::Quiet | OutputMode::Json | OutputMode::Toon => {
                Console::builder().no_color().force_terminal(false).build()
            }
        })
    }

    // ─────────────────────────────────────────────────────────────
    // Mode Checks (no lazy initialization needed - mode is always set)
    // ─────────────────────────────────────────────────────────────

    pub fn mode(&self) -> OutputMode {
        self.mode
    }
    pub fn is_rich(&self) -> bool {
        self.mode == OutputMode::Rich
    }
    pub fn is_json(&self) -> bool {
        self.mode == OutputMode::Json
    }
    pub fn is_toon(&self) -> bool {
        self.mode == OutputMode::Toon
    }
    pub fn is_quiet(&self) -> bool {
        self.mode == OutputMode::Quiet
    }
    pub fn is_plain(&self) -> bool {
        self.mode == OutputMode::Plain
    }

    /// Get terminal width (lazy-initialized).
    pub fn width(&self) -> usize {
        *self.width.get_or_init(|| self.console().width())
    }

    /// Get theme (lazy-initialized).
    ///
    /// In JSON/Quiet modes, this is never called, so theme is never created.
    pub fn theme(&self) -> &Theme {
        self.theme.get_or_init(Theme::default)
    }

    // ─────────────────────────────────────────────────────────────
    // Output Methods
    // ─────────────────────────────────────────────────────────────

    /// Print a string that is MARKUP.
    ///
    /// The console parses what it is handed: `[` followed by a letter, `#`,
    /// `/` or `@` opens a style tag, and the tag is consumed whether or not
    /// it names a real style. That holds in `Plain` mode too — dropping
    /// color does not stop the parse — so this is not a
    /// "only-on-a-terminal" concern.
    ///
    /// Consequently anything stored or user-controlled must NOT be
    /// interpolated into the argument raw: a title reading
    /// `fix [bold] rendering` would print as `fix  rendering`, deleting part
    /// of what someone wrote with nothing on screen to say so. Use
    /// [`Self::print_data`] for that, which is the right choice for nearly
    /// every caller.
    pub fn print(&self, content: &str) {
        match self.mode {
            OutputMode::Rich | OutputMode::Plain => {
                self.console().print(content);
            }
            OutputMode::Quiet | OutputMode::Json | OutputMode::Toon => {} // No console access - zero overhead
        }
    }

    /// Print a string containing stored or user-controlled DATA verbatim.
    ///
    /// Escapes markup and then prints, so brackets in the data survive to
    /// the screen instead of being parsed away as style tags (see
    /// [`Self::print`] and [`crate::format::escape_markup`]). The escape is
    /// invisible: the console turns `\[` back into `[` on the way out, and
    /// it leaves the ANSI sequences that `format::text` writes alone,
    /// because those brackets are followed by digits and so are not
    /// tag-shaped.
    ///
    /// Escaping the WHOLE assembled line is deliberate rather than escaping
    /// each field: the brackets this codebase writes itself — `[● P2]`,
    /// `[bug]`, `[2026-01-02]` — are all literal text meant to be read, not
    /// styling, and `[bug]` is itself tag-shaped. One escape at the sink is
    /// also one place to be right, instead of one per interpolation.
    pub fn print_data(&self, content: &str) {
        self.print(&crate::format::escape_markup(content));
    }

    pub fn render<R: Renderable>(&self, renderable: &R) {
        if self.is_rich() {
            self.console().print_renderable(renderable);
        }
    }

    /// # Panics
    ///
    /// Panics if serialization fails (e.g., non-string map keys, recursive structures).
    pub fn json<T: serde::Serialize>(&self, value: &T) {
        if self.is_json() {
            // Stream to stdout to avoid allocating large JSON strings.
            let stdout = io::stdout();
            let mut out = io::BufWriter::new(stdout.lock());
            if let Err(err) = serde_json::to_writer(&mut out, value) {
                assert!(
                    err.is_io(),
                    "JSON serialization failed - value is not serializable"
                );
            }
            let _ = out.write_all(b"\n");
        }
    }

    /// # Panics
    ///
    /// Panics if serialization fails (e.g., non-string map keys, recursive structures).
    pub fn json_pretty<T: serde::Serialize>(&self, value: &T) {
        if self.is_rich() {
            let json = rich_rust::renderables::Json::new(
                serde_json::to_value(value)
                    .expect("JSON conversion failed - value is not serializable"),
            );
            self.console().print_renderable(&json);
        } else if self.is_json() {
            // Stream to stdout to avoid allocating large JSON strings.
            let stdout = io::stdout();
            let mut out = io::BufWriter::new(stdout.lock());
            if let Err(err) = serde_json::to_writer_pretty(&mut out, value) {
                assert!(
                    err.is_io(),
                    "JSON serialization failed - value is not serializable"
                );
            }
            let _ = out.write_all(b"\n");
        }
    }

    /// Output value as TOON format (token-optimized object notation).
    ///
    /// # Panics
    ///
    /// Panics if serialization to JSON fails.
    pub fn toon<T: serde::Serialize>(&self, value: &T) {
        if self.is_toon() {
            let json_value = serde_json::to_value(value)
                .expect("JSON conversion failed - value is not serializable");
            let toon_value: JsonValue = json_value.into();
            let options = Some(EncodeOptions {
                indent: Some(2),
                delimiter: None,
                key_folding: Some(KeyFoldingMode::Safe),
                flatten_depth: None,
                replacer: None,
            });
            let toon_output = encode(toon_value, options);
            println!("{toon_output}");
        }
    }

    /// Output value as TOON format with optional stats on stderr.
    ///
    /// # Panics
    ///
    /// Panics if serialization to JSON fails.
    pub fn toon_with_stats<T: serde::Serialize>(&self, value: &T, show_stats: bool) {
        if self.is_toon() {
            let json_value = serde_json::to_value(value)
                .expect("JSON conversion failed - value is not serializable");
            let json_str =
                serde_json::to_string_pretty(&json_value).expect("JSON serialization failed");
            let toon_value: JsonValue = json_value.into();
            let options = Some(EncodeOptions {
                indent: Some(2),
                delimiter: None,
                key_folding: Some(KeyFoldingMode::Safe),
                flatten_depth: None,
                replacer: None,
            });
            let toon_output = encode(toon_value, options);

            if show_stats || std::env::var("TOON_STATS").is_ok() {
                let json_chars = json_str.len();
                let toon_chars = toon_output.len();
                let savings = if json_chars > 0 {
                    let diff = json_chars.saturating_sub(toon_chars);
                    diff * 100 / json_chars
                } else {
                    0
                };
                eprintln!(
                    "[stats] JSON: {} chars, TOON: {} chars ({}% savings)",
                    json_chars, toon_chars, savings
                );
            }

            println!("{toon_output}");
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Semantic Output Methods
    // ─────────────────────────────────────────────────────────────

    /// Report success. `message` is DATA, not markup.
    ///
    /// The `Rich` branch composes markup (the green tick) and so escapes the
    /// message it interpolates; the `Plain` branch writes to stdout directly,
    /// where nothing parses markup and an escape would show up as a literal
    /// backslash. Hence the escape lives here, per-branch, rather than at the
    /// ~25 call sites — which is also why callers must not pre-escape.
    ///
    /// This is an invariant, not a coincidence that happens to hold: a
    /// caller that wants pre-styled output must compose the markup itself
    /// and use [`Self::print`], because anything handed to this method
    /// will have its brackets escaped.
    pub fn success(&self, message: &str) {
        match self.mode {
            OutputMode::Rich => {
                self.console()
                    .print(&rich_semantic_line("[bold green]✓[/]", None, message));
            }
            OutputMode::Plain => println!("✓ {}", message),
            OutputMode::Quiet | OutputMode::Json | OutputMode::Toon => {} //
        }
    }

    /// Report an error.
    ///
    /// Deliberately does NOT escape: `Panel::from_text` splits the message
    /// into segments without parsing markup, so an escape here would print a
    /// visible backslash. Same for [`Self::section`] (`Text::new`) and
    /// [`Self::error_panel`] (`Text::from`) — a `Text` being assembled is
    /// never markup.
    pub fn error(&self, message: &str) {
        match self.mode {
            OutputMode::Rich => {
                let panel = Panel::from_text(message).title(Text::new("Error"));
                // .border_style(self.theme.error.clone()); // border_style missing?
                self.console().print_renderable(&panel);
            }
            OutputMode::Plain | OutputMode::Quiet => eprintln!("Error: {}", message),
            OutputMode::Json | OutputMode::Toon => {} //
        }
    }

    /// Report a warning. `message` is DATA, not markup — see
    /// [`Self::success`] for why the escape is applied here and only in the
    /// `Rich` branch.
    pub fn warning(&self, message: &str) {
        match self.mode {
            OutputMode::Rich => {
                self.console().print(&rich_semantic_line(
                    "[bold yellow]⚠[/]",
                    Some("yellow"),
                    message,
                ));
            }
            OutputMode::Plain => eprintln!("Warning: {}", message),
            OutputMode::Quiet | OutputMode::Json | OutputMode::Toon => {} //
        }
    }

    /// Report information. `message` is DATA, not markup — see
    /// [`Self::success`] for why the escape is applied here and only in the
    /// `Rich` branch.
    pub fn info(&self, message: &str) {
        match self.mode {
            OutputMode::Rich => {
                self.console()
                    .print(&rich_semantic_line("[blue]ℹ[/]", None, message));
            }
            OutputMode::Plain => println!("{}", message),
            OutputMode::Quiet | OutputMode::Json | OutputMode::Toon => {} //
        }
    }

    pub fn section(&self, title: &str) {
        if self.is_rich() {
            let rule = Rule::with_title(Text::new(title))
                // .style(self.theme.section.clone())
                ;
            self.console().print_renderable(&rule);
        } else if self.is_plain() {
            println!("\n─── {} ───\n", title);
        }
    }

    pub fn newline(&self) {
        if !self.is_quiet() && !self.is_json() && !self.is_toon() {
            println!();
        }
    }

    pub fn error_panel(&self, title: &str, description: &str, suggestions: &[&str]) {
        match self.mode {
            OutputMode::Rich => {
                let mut text = Text::from(description);
                text.append("\n\nSuggestions:\n");
                for suggestion in suggestions {
                    text.append(&format!("• {}\n", suggestion));
                }

                let panel = Panel::from_rich_text(&text, self.width()).title(Text::new(title));
                // .border_style(self.theme.error.clone());
                self.console().print_renderable(&panel);
            }
            OutputMode::Plain => {
                eprintln!("Error: {} - {}", title, description);
                for suggestion in suggestions {
                    eprintln!("  Suggestion: {}", suggestion);
                }
            }
            OutputMode::Quiet => eprintln!("Error: {}", description),
            OutputMode::Json | OutputMode::Toon => {} //
        }
    }
}

/// Compose the markup for a semantic message line (`✓ …`, `⚠ …`, `ℹ …`).
///
/// Split out of [`OutputContext::success`], [`OutputContext::warning`] and
/// [`OutputContext::info`] so the escape can be asserted without a terminal:
/// the `Rich` branch only exists when stdout is a tty, so an e2e test running
/// under a pipe never reaches it, and the corruption it caused (`Created
/// bd-1: rich <ESC>[1m title` for a title reading `rich [bold] title`) went
/// unnoticed for exactly that reason.
///
/// `glyph_markup` is markup written by this codebase and passes through
/// as-is. `message` is DATA and is escaped. `message_style`, when given,
/// wraps the message in a style tag — placed OUTSIDE the escaped text, so it
/// styles the message without being able to eat any of it.
fn rich_semantic_line(glyph_markup: &str, message_style: Option<&str>, message: &str) -> String {
    let escaped = crate::format::escape_markup(message);
    match message_style {
        Some(style) => format!("{glyph_markup} [{style}]{escaped}[/]"),
        None => format!("{glyph_markup} {escaped}"),
    }
}

#[cfg(test)]
mod tests {
    use super::rich_semantic_line;

    /// Every hazard in one message: a real style name, a bare bracketed word
    /// that is not a style, and a closing tag. Unescaped, the parser eats the
    /// first two and errors on the third.
    const HAZARD: &str = "esc [bold] [probe] [/] x";

    /// The property, checked against the real parser rather than against the
    /// shape of the escape: whatever the caller passed as DATA comes out the
    /// other side intact, with the glyph in front of it.
    ///
    /// `plain()` is the rendered text with styling stripped — exactly what a
    /// reader sees on a terminal, minus the colour — so an eaten `[bold]` or
    /// a leaked backslash both fail here.
    #[test]
    fn success_line_renders_the_message_verbatim() {
        let rendered = rich_rust::markup::render_or_plain(&rich_semantic_line(
            "[bold green]✓[/]",
            None,
            HAZARD,
        ));
        assert_eq!(rendered.plain(), format!("✓ {HAZARD}"));
    }

    #[test]
    fn info_line_renders_the_message_verbatim() {
        let rendered =
            rich_rust::markup::render_or_plain(&rich_semantic_line("[blue]ℹ[/]", None, HAZARD));
        assert_eq!(rendered.plain(), format!("ℹ {HAZARD}"));
    }

    /// A styled message is the interesting case: the style tag has to sit
    /// outside the escaped data, or the data could close it early and take the
    /// rest of the line with it.
    #[test]
    fn warning_line_renders_the_message_verbatim_and_keeps_its_style() {
        let line = rich_semantic_line("[bold yellow]⚠[/]", Some("yellow"), HAZARD);
        let rendered = rich_rust::markup::render_or_plain(&line);
        assert_eq!(rendered.plain(), format!("⚠ {HAZARD}"));
        // The style survived as STYLE, not as visible text: `[yellow]` must
        // not appear in what the reader sees.
        assert!(
            !rendered.plain().contains("[yellow]"),
            "the wrapping style tag leaked into the text: {}",
            rendered.plain()
        );
    }

    /// The regression itself, stated as a test: without the escape the
    /// message is silently mutilated. If this ever stops holding, the console
    /// stopped parsing markup and every escape in this file can go.
    #[test]
    fn an_unescaped_message_would_be_mutilated() {
        let rendered = rich_rust::markup::render_or_plain(&format!("[bold green]✓[/] {HAZARD}"));
        assert_ne!(rendered.plain(), format!("✓ {HAZARD}"));
    }
}
