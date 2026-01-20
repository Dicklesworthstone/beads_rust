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
