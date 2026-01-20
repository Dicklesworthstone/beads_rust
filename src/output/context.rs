use crate::cli::Cli;
use rich_rust::prelude::*;
use rich_rust::renderables::Renderable;
use super::Theme;
use std::io::IsTerminal;

/// Central output coordinator that respects robot/json/quiet modes.
pub struct OutputContext {
    /// Rich console for human-readable output
    console: Console,
    /// Theme for consistent styling
    theme: Theme,
    /// Output mode
    mode: OutputMode,
    /// Terminal width (cached)
    width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Full rich formatting (tables, colors, panels)
    Rich,
    /// Plain text, no ANSI codes (for piping)
    Plain,
    /// JSON output only
    Json,
    /// Minimal output (quiet mode)
    Quiet,
}

impl OutputContext {
    /// Create from CLI global args
    pub fn from_args(args: &Cli) -> Self {
        let mode = Self::detect_mode(args);
        let console = Self::create_console(mode);
        let width = console.width();

        Self {
            console,
            theme: Theme::default(),
            mode,
            width,
        }
    }

    /// Create from explicit flags (legacy/partial migration support)
    pub fn from_flags(json: bool, quiet: bool, plain: bool) -> Self {
        let mode = if json {
            OutputMode::Json
        } else if quiet {
            OutputMode::Quiet
        } else if plain || !std::io::stdout().is_terminal() {
            OutputMode::Plain
        } else {
            OutputMode::Rich
        };
        
        let console = Self::create_console(mode);
        let width = console.width();

        Self {
            console,
            theme: Theme::default(),
            mode,
            width,
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

    fn create_console(mode: OutputMode) -> Console {
        match mode {
            OutputMode::Rich => Console::new(),
            OutputMode::Plain | OutputMode::Quiet => {
                Console::builder()
                    .color_system(ColorSystem::Standard)
                    .force_terminal(false)
                    .build()
            }
            OutputMode::Json => {
                Console::builder()
                    .color_system(ColorSystem::Standard)
                    .force_terminal(false)
                    .build()
            }
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Mode Checks
    // ─────────────────────────────────────────────────────────────

    pub fn mode(&self) -> OutputMode { self.mode }
    pub fn is_rich(&self) -> bool { self.mode == OutputMode::Rich }
    pub fn is_json(&self) -> bool { self.mode == OutputMode::Json }
    pub fn is_quiet(&self) -> bool { self.mode == OutputMode::Quiet }
    pub fn is_plain(&self) -> bool { self.mode == OutputMode::Plain }
    pub fn width(&self) -> usize { self.width }
    pub fn theme(&self) -> &Theme { &self.theme }

    // ─────────────────────────────────────────────────────────────
    // Output Methods
    // ─────────────────────────────────────────────────────────────

    pub fn print(&self, content: &str) {
        match self.mode {
            OutputMode::Rich => self.console.print(content),
            OutputMode::Plain => {
                self.console.print(content);
            }
            OutputMode::Quiet => { } //
            OutputMode::Json => { } //
        }
    }

    pub fn render<R: Renderable>(&self, renderable: &R) {
        if self.is_rich() {
            self.console.print_renderable(renderable);
        }
    }

    pub fn json<T: serde::Serialize>(&self, value: &T) {
        if self.is_json() {
            println!("{}", serde_json::to_string(value).unwrap());
        }
    }

    pub fn json_pretty<T: serde::Serialize>(&self, value: &T) {
        if self.is_rich() {
            let json = rich_rust::renderables::Json::new(
                serde_json::to_value(value).unwrap()
            );
            self.console.print_renderable(&json);
        } else if self.is_json() {
            println!("{}", serde_json::to_string_pretty(value).unwrap());
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Semantic Output Methods
    // ─────────────────────────────────────────────────────────────

    pub fn success(&self, message: &str) {
        match self.mode {
            OutputMode::Rich => {
                self.console.print(&format!(
                    "[bold green]✓[/] {}",
                    message
                ));
            }
            OutputMode::Plain => println!("✓ {}", message),
            OutputMode::Quiet | OutputMode::Json => {} //
        }
    }

    pub fn error(&self, message: &str) {
        match self.mode {
            OutputMode::Rich => {
                let panel = Panel::from_text(message)
                    .title(Text::new("Error"));
                    // .border_style(self.theme.error.clone()); // border_style missing?
                self.console.print_renderable(&panel);
            }
            OutputMode::Plain => eprintln!("Error: {}", message),
            OutputMode::Quiet => eprintln!("Error: {}", message),
            OutputMode::Json => {} //
        }
    }

    pub fn warning(&self, message: &str) {
        match self.mode {
            OutputMode::Rich => {
                self.console.print(&format!(
                    "[bold yellow]⚠[/] [yellow]{}[/]",
                    message
                ));
            }
            OutputMode::Plain => eprintln!("Warning: {}", message),
            OutputMode::Quiet => {} //
            OutputMode::Json => {}  //
        }
    }

    pub fn info(&self, message: &str) {
        match self.mode {
            OutputMode::Rich => {
                self.console.print(&format!(
                    "[blue]ℹ[/] {}",
                    message
                ));
            }
            OutputMode::Plain => println!("{}", message),
            OutputMode::Quiet | OutputMode::Json => {} //
        }
    }

    pub fn section(&self, title: &str) {
        if self.is_rich() {
            let rule = Rule::with_title(Text::new(title))
                // .style(self.theme.section.clone())
                ;
            self.console.print_renderable(&rule);
        } else if self.is_plain() {
            println!("\n─── {} ───\n", title);
        }
    }

    pub fn newline(&self) {
        if !self.is_quiet() && !self.is_json() {
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
                
                let panel = Panel::from_rich_text(&text, self.width)
                    .title(Text::new(title));
                    // .border_style(self.theme.error.clone());
                self.console.print_renderable(&panel);
            }
             OutputMode::Plain => {
                 eprintln!("Error: {} - {}", title, description);
                 for suggestion in suggestions {
                     eprintln!("  Suggestion: {}", suggestion);
                 }
             }
             OutputMode::Quiet => eprintln!("Error: {}", description),
             OutputMode::Json => {} //
         }
    }
}
