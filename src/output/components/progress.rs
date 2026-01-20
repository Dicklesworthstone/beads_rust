use rich_rust::prelude::*;
use std::io::{self, Write};
use crate::output::Theme;

/// Progress tracker for long operations (sync, import, export).
pub struct ProgressTracker<'a> {
    theme: &'a Theme,
    total: usize,
    current: usize,
    description: String,
    bar: ProgressBar,
}

impl<'a> ProgressTracker<'a> {
    pub fn new(theme: &'a Theme, total: usize, description: impl Into<String>) -> Self {
        let bar = ProgressBar::new();

        Self {
            theme,
            total,
            current: 0,
            description: description.into(),
            bar,
        }
    }

    pub fn tick(&mut self) {
        self.current += 1;
        self.bar.set_progress(self.current as f64);
    }

    pub fn set(&mut self, current: usize) {
        self.current = current;
        self.bar.set_progress(current as f64);
    }

    pub fn render(&self, console: &Console) {
        // Clear line and render progress
        print!("\r");
        console.print(&format!(
            "[bold]{}[/]: ",
            self.description
        ));
        console.print_renderable(&self.bar);
        print!(" {}/{}", self.current, self.total);
        io::stdout().flush().ok();
    }

    pub fn finish(&self, console: &Console) {
        println!();
        console.print(&format!(
            "[bold green]✓[/] {} complete ({} items)",
            self.description,
            self.total
        ));
    }
}