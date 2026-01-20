//! Output abstraction layer that routes to rich or plain output based on mode.

pub mod context;
pub mod theme;
pub mod components;

pub use context::{OutputContext, OutputMode};
pub use theme::Theme;
pub use components::*;