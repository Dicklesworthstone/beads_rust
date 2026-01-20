//! Output abstraction layer that routes to rich or plain output based on mode.

pub mod components;
pub mod context;
pub mod theme;

pub use components::*;
pub use context::{OutputContext, OutputMode};
pub use theme::Theme;
