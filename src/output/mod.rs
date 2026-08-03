//! # Output Module
//!
//! This module provides rich terminal output using the [`rich_rust`] library.
//! It automatically detects the output mode and renders accordingly.
//!
//! ## Mode Detection
//!
//! Output mode is determined by the following priority:
//!
//! 1. `--json` or `--robot` flags → **JSON mode** (machine-readable)
//! 2. `--quiet` flag → **Quiet mode** (minimal output)
//! 3. `NO_COLOR` env or `--no-color` → **Plain mode** (no ANSI codes)
//! 4. Non-TTY stdout → **Plain mode** (piped output)
//! 5. Otherwise → **Rich mode** (colors, tables, panels)
//!
//! ## Usage
//!
//! ```rust,ignore
//! use crate::output::{OutputContext, OutputMode};
//!
//! // Create from CLI args
//! let ctx = OutputContext::from_args(&cli);
//!
//! // Or from flags directly
//! let ctx = OutputContext::from_flags(json, quiet, no_color);
//!
//! // Mode-aware output
//! ctx.success("Operation completed");
//! ctx.error("Something went wrong");
//! ctx.json(&data);  // Only outputs in JSON mode
//!
//! // Rich rendering (only in Rich mode)
//! ctx.render(&table);
//! ctx.render(&panel);
//! ```
//!
//! ## Submodules
//!
//! - [`context`]: Core [`OutputContext`] struct and [`OutputMode`] enum
//! - [`theme`]: Visual styling with [`Theme`] struct (colors, borders)
//! - [`components`]: Reusable output components (tables, panels, etc.)
//!
//! ## Markup: which emit functions parse it, and what that means for data
//!
//! Exactly one thing in `rich_rust` parses markup: a **string** on its way
//! through `Console::print` (and the `Renderable` impl for `str`/`String`).
//! Its tag pattern is `[` followed by a letter, `#`, `/` or `@`, and a tag is
//! consumed whether or not it names a real style — `[bug]`, `[probe]` and
//! `[bold]` all vanish. Parsing happens in **Plain mode too**; turning color
//! off does not turn the parser off.
//!
//! So the question for any output path is only ever "what does it finally
//! emit?":
//!
//! | Final emit                                       | Parses markup? | Stored data must be… |
//! |--------------------------------------------------|----------------|----------------------|
//! | [`OutputContext::print`] → `Console::print`      | **yes**        | escaped — use [`OutputContext::print_data`] |
//! | `success`/`info`/`warning`, Rich branch          | **yes**        | escaped (done inside those methods) |
//! | `success`/`info`/`warning`, Plain branch         | no (`println!`)| raw |
//! | `error`/`error_panel`/`section` (`Panel`/`Text`) | no             | raw |
//! | [`OutputContext::render`] of a `Table`/`Panel`/`Text` | no        | raw |
//! | `IssueTable`, `IssuePanel`, `stale`/`blocked` line builders (`Text::append_styled`) | no | raw |
//! | bare `print!`/`println!`/`eprintln!`             | no             | raw |
//! | `json`/`json_pretty`/`toon`                     | no             | raw |
//!
//! This is why `bd list` was safe while `bd search` was not: both build the
//! same line with [`crate::format::format_issue_line_with`], but `list`
//! emits it with `println!` and `search` handed it to `ctx.print`, where the
//! `[bug]` type badge and any bracketed word in the title were eaten.
//!
//! Escaping on a path that does NOT parse markup is a bug in the other
//! direction: the backslash becomes visible. Both failures are caught by the
//! same kind of test — assert rendered output against the JSON ground truth
//! for a value containing `[bold]`.
//!
//! ### Composing markup yourself
//!
//! Some renderers build a style-tagged string and then need it as a `Text`
//! (`bd dep list`, `bd dep tree`). That string must be parsed exactly once —
//! `rich_rust::markup::render_or_plain` — and any stored data inside it must
//! be escaped BEFORE that parse. Handing markup straight to `Panel::from_text`
//! or `Text::new` prints the tags as visible text, which is how `bd dep tree`
//! came to display `mk-1 [green][open][/] some title`.
//!
//! Watch for brackets that are LABELS rather than styles. `[open]`, `[bug]`,
//! `[P2]` are meant to be read, and `[open]` is tag-shaped, so it must be
//! escaped even though this codebase wrote it: unescaped, the parser looks up
//! a style called `open`, finds none, and drops the label. `[2026-01-02]` and
//! `[● P2]` are safe only because a digit or a symbol follows the bracket —
//! that is an accident of the tag pattern, not a design.
//!
//! ## Design Principles
//!
//! - **Zero overhead in JSON/Quiet modes**: Console and theme are lazy-initialized
//! - **Automatic mode detection**: No manual configuration needed
//! - **Graceful degradation**: Rich → Plain → JSON → Quiet fallback chain
//! - **Consistent styling**: Theme provides unified look across commands

pub mod components;
pub mod context;
pub mod theme;

pub use components::*;
pub use context::{OutputContext, OutputMode};
pub use theme::Theme;
