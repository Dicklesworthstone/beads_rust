//! Linear-time construction of Rich panels from one multi-line [`Text`].
//!
//! `Panel::from_rich_text` splits its content with `Text::split_lines`, which
//! walks every span once per line. Commands that render one panel line per
//! issue (`br graph`, `br dep list`, `br show` with many relations) build
//! texts with a few spans per line, so that split is quadratic: on an
//! 18,000-issue `br graph --all` it took most of the run (GitHub #580).
//!
//! [`split_rich_lines`] returns exactly what `Text::split_lines` returns, but
//! hands each span only to the lines it overlaps. [`rich_text_panel`] is a
//! drop-in replacement for `Panel::from_rich_text` built on it, and
//! [`wrap_rich_lines`] one for `Text::wrap`, which splits the same way.

use rich_rust::prelude::*;

/// Split `text` at `'\n'` into one [`Text`] per line.
///
/// The result is identical to `text.split_lines()`: the same plain text per
/// line, every span clipped to each line it overlaps (in the original span
/// order, dropping empty clips), the parent's style and settings on every
/// line, and a trailing empty line when `text` ends with a newline. Each span
/// costs a binary search plus one step per line it overlaps, instead of one
/// step per line of the whole text.
#[must_use]
pub fn split_rich_lines(text: &Text) -> Vec<Text> {
    let plain = text.plain();
    let total_chars = text.len();

    // (start byte, end byte, start char, end char) of every line.
    let mut bounds: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut start_byte = 0;
    let mut start_char = 0;
    for (char_idx, (byte_idx, c)) in plain.char_indices().enumerate() {
        if c == '\n' {
            bounds.push((start_byte, byte_idx, start_char, char_idx));
            start_byte = byte_idx + c.len_utf8();
            start_char = char_idx + 1;
        }
    }
    // `Text::split_lines` measures the last line against the cached length.
    bounds.push((start_byte, plain.len(), start_char, total_chars));

    let mut lines: Vec<Text> = bounds
        .iter()
        .map(|&(start_byte, end_byte, _, _)| {
            let mut line = Text::new(&plain[start_byte..end_byte]);
            line.set_style(text.style().clone());
            line.justify = text.justify;
            line.overflow = text.overflow;
            line.no_wrap = text.no_wrap;
            line.end.clone_from(&text.end);
            line.tab_size = text.tab_size;
            line
        })
        .collect();

    for span in text.spans() {
        // Lines are sorted and disjoint: skip the ones ending at or before the
        // span start, then visit lines until one starts at or after its end.
        let first = bounds.partition_point(|&(_, _, _, line_end)| line_end <= span.start);
        let candidates = lines[first..].iter_mut().zip(&bounds[first..]);
        for (line, &(_, _, line_start, line_end)) in candidates {
            if line_start >= span.end {
                break;
            }
            let clip_start = span.start.max(line_start) - line_start;
            let clip_end = span.end.min(line_end) - line_start;
            if clip_start < clip_end {
                line.stylize(clip_start, clip_end, span.style.clone());
            }
        }
    }

    lines
}

/// Wrap `text` to `width` cells: the same lines `text.wrap(width)` returns.
///
/// `Text::wrap` splits the whole text with `Text::split_lines` before it
/// wraps each line; this splits with [`split_rich_lines`] instead and wraps
/// only the lines that are too wide, one line at a time.
#[must_use]
pub fn wrap_rich_lines(text: &Text, width: usize) -> Vec<Text> {
    if width == 0 {
        return vec![Text::new("")];
    }
    let expanded = text.expand_tabs(text.tab_size);
    if expanded.no_wrap || expanded.cell_len() <= width {
        return vec![expanded];
    }

    let mut lines = Vec::new();
    for line in split_rich_lines(&expanded) {
        if line.cell_len() <= width {
            lines.push(line);
        } else {
            // `line` holds no newline or tab, so its own `wrap` splits it into
            // itself and wraps that: the pieces `Text::wrap` makes for it.
            lines.extend(line.wrap(width));
        }
    }
    lines
}

/// Drop-in replacement for `Panel::from_rich_text` that splits the content
/// with [`split_rich_lines`], so building the panel is linear in the number
/// of lines and spans. The rendered panel is identical.
#[must_use]
pub fn rich_text_panel(text: &Text, width: usize) -> Panel<'static> {
    let content_lines = split_rich_lines(text)
        .iter()
        .map(|line| {
            line.render("")
                .into_iter()
                .map(Segment::into_owned)
                .collect()
        })
        .collect();
    Panel::new(content_lines).width(width)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_same_lines(text: &Text) {
        let expected = text.split_lines();
        let actual = split_rich_lines(text);
        assert_eq!(actual.len(), expected.len(), "line count for {text:?}");
        for (index, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
            assert_eq!(actual.plain(), expected.plain(), "line {index} plain");
            assert_eq!(actual.spans(), expected.spans(), "line {index} spans");
            assert_eq!(actual.len(), expected.len(), "line {index} length");
            assert_eq!(actual.style(), expected.style(), "line {index} style");
            assert_eq!(actual.justify, expected.justify, "line {index} justify");
            assert_eq!(actual.overflow, expected.overflow, "line {index} overflow");
            assert_eq!(actual.no_wrap, expected.no_wrap, "line {index} no_wrap");
            assert_eq!(actual.end, expected.end, "line {index} end");
            assert_eq!(actual.tab_size, expected.tab_size, "line {index} tab_size");
        }
    }

    fn sample_text() -> Text {
        let bold = Style::new().bold();
        let red = Style::parse("red").unwrap_or_default();
        let mut text = Text::new("");
        // A styled run that ends with its own newline, like a section header.
        text.append_styled("Header line\n\n", bold.clone());
        text.append("  ");
        text.append_styled("bd-1", red.clone());
        text.append(" título ✓ ");
        text.append_styled("[P1]", bold.clone());
        text.append("\n");
        // A span that crosses two newlines and an empty line.
        text.append_styled("multi\n\nline span", red.clone());
        text.append("\nplain tail");
        // Out-of-order and overlapping spans, a span past the end, an empty span.
        text.stylize(3, 20, Style::new().italic());
        text.stylize(0, 2, Style::new().underline());
        text.stylize(text.len() - 2, text.len() + 10, bold);
        text.stylize(5, 5, red);
        text
    }

    #[test]
    fn split_matches_text_split_lines() {
        assert_same_lines(&sample_text());
    }

    #[test]
    fn split_matches_text_split_lines_at_the_edges() {
        assert_same_lines(&Text::new(""));
        assert_same_lines(&Text::new("\n"));
        assert_same_lines(&Text::new("\n\n\n"));
        assert_same_lines(&Text::new("no newline"));
        assert_same_lines(&Text::styled("whole\ntext styled\n", Style::new().bold()));

        let mut trailing = Text::new("");
        trailing.append_styled("ends with newline\n", Style::new().bold());
        assert_same_lines(&trailing);

        let mut settings = sample_text();
        settings.set_style(Style::new().dim());
        settings.justify = JustifyMethod::Center;
        settings.overflow = OverflowMethod::Ellipsis;
        settings.no_wrap = true;
        settings.end = String::new();
        settings.tab_size = 4;
        assert_same_lines(&settings);
    }

    #[test]
    fn wrap_matches_text_wrap() {
        let mut long = sample_text();
        long.append("\n");
        long.append_styled(
            "a long styled line that has to wrap at narrow widths, twice over",
            Style::new().bold(),
        );
        long.append("\n\tTabbed\tline with\ttabs that also wraps around\n");
        long.stylize(4, long.len() - 3, Style::new().italic());
        let mut no_wrap = long.clone();
        no_wrap.no_wrap = true;
        let mut crop = long.clone();
        crop.overflow = OverflowMethod::Crop;
        let mut ellipsis = long.clone();
        ellipsis.overflow = OverflowMethod::Ellipsis;

        for text in [&long, &no_wrap, &crop, &ellipsis, &Text::new("")] {
            for width in [0, 1, 2, 5, 13, 40, 400] {
                let expected = text.wrap(width);
                let actual = wrap_rich_lines(text, width);
                assert_eq!(actual.len(), expected.len(), "width {width}: {text:?}");
                for (actual, expected) in actual.iter().zip(&expected) {
                    assert_eq!(actual.plain(), expected.plain(), "width {width}");
                    assert_eq!(actual.spans(), expected.spans(), "width {width}");
                    assert_eq!(actual.style(), expected.style(), "width {width}");
                }
            }
        }
    }

    #[test]
    fn panel_renders_identically_to_from_rich_text() {
        let text = sample_text();
        for width in [12, 40, 120] {
            let expected = Panel::from_rich_text(&text, width).title(Text::new("Title"));
            let actual = rich_text_panel(&text, width).title(Text::new("Title"));
            assert_eq!(
                actual.render(width),
                expected.render(width),
                "width {width}"
            );
        }
    }
}
