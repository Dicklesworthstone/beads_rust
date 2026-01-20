use crate::output::Theme;
use rich_rust::prelude::*;
use std::io::{self, Write};

/// Progress tracker for long operations (sync, import, export).
pub struct ProgressTracker {
    total: usize,
    current: usize,
    description: String,
    bar: ProgressBar,
}

impl ProgressTracker {
    pub fn new(theme: &Theme, total: usize, description: impl Into<String>) -> Self {
        let bar = ProgressBar::with_total(total as u64)
            .width(40)
            .bar_style(BarStyle::Block)
            .completed_style(theme.accent.clone())
            .remaining_style(theme.dimmed.clone());

        Self {
            total,
            current: 0,
            description: description.into(),
            bar,
        }
    }

    pub fn tick(&mut self) {
        self.current += 1;
        self.bar.set_progress(self.progress_ratio());
    }

    pub fn set(&mut self, current: usize) {
        self.current = current;
        self.bar.set_progress(self.progress_ratio());
    }

    fn progress_ratio(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.current as f64 / self.total as f64).min(1.0)
        }
    }

    pub fn render(&self, console: &Console) {
        // Clear line and render progress
        print!("\r");
        console.print(&format!("[bold]{}[/]: ", self.description));
        console.print_renderable(&self.bar);
        print!(" {}/{}", self.current, self.total);
        io::stdout().flush().ok();
    }

    pub fn finish(&self, console: &Console) {
        println!();
        console.print(&format!(
            "[bold green]✓[/] {} complete ({} items)",
            self.description, self.total
        ));
    }
}
