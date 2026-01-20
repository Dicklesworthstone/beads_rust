use crate::output::Theme;
use rich_rust::prelude::*;

/// Renders statistics as a formatted panel with counts.
pub struct StatsPanel<'a> {
    title: String,
    stats: Vec<(&'a str, usize, Style)>,
    theme: &'a Theme,
}

impl<'a> StatsPanel<'a> {
    pub fn new(title: impl Into<String>, theme: &'a Theme) -> Self {
        Self {
            title: title.into(),
            stats: vec![],
            theme,
        }
    }

    pub fn add(&mut self, label: &'a str, count: usize, style: Style) -> &mut Self {
        self.stats.push((label, count, style));
        self
    }

    #[must_use]
    pub fn build(&self) -> Table {
        let mut table = Table::new()
            .box_style(self.theme.box_style)
            .border_style(self.theme.panel_border.clone())
            .title(Text::styled(&self.title, self.theme.panel_title.clone()));

        table = table
            .with_column(Column::new("Label").min_width(15))
            .with_column(
                Column::new("Count")
                    .justify(JustifyMethod::Right)
                    .min_width(6),
            );

        for (label, count, style) in &self.stats {
            let label_cell = Cell::new(Text::styled(*label, style.clone()));
            table.add_row(Row::new(vec![
                label_cell,
                Cell::new(Text::new(count.to_string())),
            ]));
        }

        table
    }
}
