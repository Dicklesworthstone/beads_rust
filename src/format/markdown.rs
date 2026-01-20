//! Markdown rendering for descriptions and comments.
//!
//! Provides mode-aware markdown rendering for issue descriptions and comments.
//! Uses `rich_rust`'s Markdown component when available.
//!
//! # Mode Behavior
//!
//! - **Rich**: Full styled markdown rendering via pulldown-cmark
//! - **Plain**: Stripped markdown, plain text only
//! - **JSON**: Raw markdown string unchanged
//! - **Quiet**: No output
//!
//! # Example
//!
//! ```ignore
//! use beads_rust::format::markdown::render_markdown;
//! use beads_rust::format::OutputContext;
//!
//! let content = "# Heading\n\nThis is **bold** and *italic*.";
//! let ctx = OutputContext::detect();
//! let rendered = render_markdown(content, &ctx);
//! ```

use crate::format::context::{OutputContext, OutputMode};
use rich_rust::color::ColorSystem;
use rich_rust::renderables::markdown::Markdown;

/// Render markdown content based on output mode.
///
/// # Arguments
///
/// * `content` - The markdown source text
/// * `ctx` - The output context determining rendering mode
///
/// # Returns
///
/// A string with the rendered content. In Rich mode, includes ANSI escape codes.
/// In Plain mode, returns stripped markdown. In JSON mode, returns raw markdown.
///
/// # Supported Markdown Elements
///
/// - **Headings**: H1-H6 with distinct styles
/// - **Emphasis**: *italic*, **bold**, ~~strikethrough~~
/// - **Code**: `inline code` and fenced code blocks (with syntax highlighting)
/// - **Lists**: Ordered (1. 2. 3.) and unordered (- * +)
/// - **Links**: `[text](url)` displayed with URLs
/// - **Blockquotes**: `> quoted text`
/// - **Tables**: GitHub Flavored Markdown tables
/// - **Horizontal rules**: `---` or `***`
#[must_use]
pub fn render_markdown(content: &str, ctx: &OutputContext) -> String {
    match ctx.mode() {
        OutputMode::Quiet => String::new(),
        OutputMode::Json => content.to_string(),
        OutputMode::Plain => strip_markdown(content),
        OutputMode::Rich => render_rich_markdown(content, ctx.width()),
    }
}

/// Strip markdown formatting and return plain text.
///
/// Removes markdown syntax while preserving the underlying text content.
fn strip_markdown(content: &str) -> String {
    let mut result = String::new();
    let mut in_code_block = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Handle fenced code blocks
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }

        // Inside code block, preserve as-is (indented)
        if in_code_block {
            result.push_str("    ");
            result.push_str(line);
            result.push('\n');
            continue;
        }

        // Check for horizontal rules first
        if is_horizontal_rule(trimmed) {
            result.push_str("---\n");
            continue;
        }

        // Process the line to strip markdown
        let processed = strip_line_markdown(line);
        result.push_str(&processed);
        result.push('\n');
    }

    // Remove trailing newline
    result.trim_end().to_string()
}

/// Check if a line is a horizontal rule.
fn is_horizontal_rule(trimmed: &str) -> bool {
    let hr_stripped: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    (hr_stripped.chars().all(|c| c == '-') || hr_stripped.chars().all(|c| c == '*'))
        && hr_stripped.len() >= 3
}

/// Strip markdown formatting from a single line.
fn strip_line_markdown(line: &str) -> String {
    let mut processed = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut in_inline_code = false;

    while i < chars.len() {
        let c = chars[i];

        // Handle inline code
        if c == '`' {
            in_inline_code = !in_inline_code;
            i += 1;
            continue;
        }

        // Skip markdown formatting characters (only outside inline code)
        if !in_inline_code {
            if let Some(skip) = try_skip_formatting(&chars, i, &mut processed) {
                i = skip;
                continue;
            }
        }

        processed.push(c);
        i += 1;
    }

    processed
}

/// Try to skip markdown formatting at the current position.
/// Returns the new index if formatting was skipped, None otherwise.
fn try_skip_formatting(chars: &[char], i: usize, processed: &mut String) -> Option<usize> {
    let c = chars[i];

    // Bold/italic markers
    if c == '*' || c == '_' {
        let mut j = i;
        while j < chars.len() && (chars[j] == '*' || chars[j] == '_') {
            j += 1;
        }
        return Some(j);
    }

    // Strikethrough
    if c == '~' && i + 1 < chars.len() && chars[i + 1] == '~' {
        return Some(i + 2);
    }

    // Headers at start of line
    if processed.is_empty() && c == '#' {
        let mut j = i;
        while j < chars.len() && chars[j] == '#' {
            j += 1;
        }
        // Skip space after header markers
        if j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        return Some(j);
    }

    // Links: [text](url) -> text
    if c == '[' {
        if let Some(new_i) = try_extract_link(chars, i, processed) {
            return Some(new_i);
        }
    }

    // Image: ![alt](url) -> [Image: alt]
    if c == '!' && i + 1 < chars.len() && chars[i + 1] == '[' {
        if let Some(new_i) = try_extract_image(chars, i, processed) {
            return Some(new_i);
        }
    }

    // Blockquote marker at start
    if processed.is_empty() && c == '>' {
        let mut j = i + 1;
        // Skip space after >
        if j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        processed.push_str("  "); // Indent blockquotes
        return Some(j);
    }

    None
}

/// Try to extract link text from [text](url) format.
fn try_extract_link(chars: &[char], i: usize, processed: &mut String) -> Option<usize> {
    let start = i + 1;
    let bracket_end = find_matching_bracket(chars, start)?;

    // Extract link text
    let text: String = chars[start..bracket_end].iter().collect();
    processed.push_str(&text);

    // Skip past ](url)
    let mut j = bracket_end + 1;
    if j < chars.len() && chars[j] == '(' {
        j = skip_parentheses(chars, j);
    }
    Some(j)
}

/// Try to extract image alt text from ![alt](url) format.
fn try_extract_image(chars: &[char], i: usize, processed: &mut String) -> Option<usize> {
    let start = i + 2; // Skip ![
    let bracket_end = find_closing_bracket(chars, start)?;

    let alt: String = chars[start..bracket_end].iter().collect();
    if !alt.is_empty() {
        processed.push_str("[Image: ");
        processed.push_str(&alt);
        processed.push(']');
    }

    // Skip past ](url)
    let mut j = bracket_end + 1;
    if j < chars.len() && chars[j] == '(' {
        j = skip_parentheses(chars, j);
    }
    Some(j)
}

/// Find the matching closing bracket for an opening bracket.
fn find_matching_bracket(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 1;
    for (offset, &ch) in chars[start..].iter().enumerate() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Find the closing bracket (simple, no nesting).
fn find_closing_bracket(chars: &[char], start: usize) -> Option<usize> {
    for (offset, &ch) in chars[start..].iter().enumerate() {
        if ch == ']' {
            return Some(start + offset);
        }
    }
    None
}

/// Skip over parentheses including nested ones.
fn skip_parentheses(chars: &[char], start: usize) -> usize {
    let mut j = start + 1;
    let mut paren_depth = 1;
    while j < chars.len() && paren_depth > 0 {
        if chars[j] == '(' {
            paren_depth += 1;
        } else if chars[j] == ')' {
            paren_depth -= 1;
        }
        j += 1;
    }
    j
}

/// Render markdown using rich_rust's Markdown component.
fn render_rich_markdown(content: &str, width: usize) -> String {
    let md = Markdown::new(content).show_links(true);

    let segments = md.render(width);

    // Render segments to a string with ANSI codes
    let mut result = String::new();
    for segment in segments {
        if let Some(style) = &segment.style {
            result.push_str(&style.render(&segment.text, ColorSystem::TrueColor));
        } else {
            result.push_str(&segment.text);
        }
    }

    result
}

/// Check if a string contains markdown formatting.
///
/// Useful for deciding whether to apply markdown rendering.
#[must_use]
pub fn contains_markdown(content: &str) -> bool {
    // Check for common markdown patterns
    let patterns = [
        "**",  // Bold
        "__",  // Bold (alternate)
        "*",   // Italic (single)
        "_",   // Italic (alternate, single)
        "~~",  // Strikethrough
        "`",   // Code
        "```", // Code block
        "[",   // Link start
        "](",  // Link middle
        "# ",  // Header
        "##",  // Header
        "> ",  // Blockquote
        "- ",  // Unordered list
        "* ",  // Unordered list (alternate)
        "1.",  // Ordered list
        "---", // Horizontal rule
        "***", // Horizontal rule (alternate)
    ];

    patterns.iter().any(|pattern| content.contains(pattern))
}

/// Escape markdown special characters in a string.
///
/// Use this when inserting user content that should not be interpreted as markdown.
#[must_use]
pub fn escape_markdown(content: &str) -> String {
    let mut result = String::with_capacity(content.len() * 2);

    for c in content.chars() {
        match c {
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '#' | '+' | '-' | '.'
            | '!' | '|' | '~' | '>' => {
                result.push('\\');
                result.push(c);
            }
            _ => result.push(c),
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_ctx() -> OutputContext {
        OutputContext::with_mode(OutputMode::Plain)
    }

    fn json_ctx() -> OutputContext {
        OutputContext::with_mode(OutputMode::Json)
    }

    fn quiet_ctx() -> OutputContext {
        OutputContext::with_mode(OutputMode::Quiet)
    }

    fn rich_ctx() -> OutputContext {
        OutputContext::with_mode(OutputMode::Rich)
    }

    #[test]
    fn test_render_markdown_plain_strips_formatting() {
        let content = "# Heading\n\nThis is **bold** and *italic*.";
        let result = render_markdown(content, &plain_ctx());
        assert!(result.contains("Heading"));
        assert!(result.contains("bold"));
        assert!(result.contains("italic"));
        assert!(!result.contains("**"));
        assert!(!result.contains('#'));
    }

    #[test]
    fn test_render_markdown_json_unchanged() {
        let content = "# Heading\n\n**bold** text";
        let result = render_markdown(content, &json_ctx());
        assert_eq!(result, content);
    }

    #[test]
    fn test_render_markdown_quiet_empty() {
        let content = "# Heading\n\nSome content";
        let result = render_markdown(content, &quiet_ctx());
        assert!(result.is_empty());
    }

    #[test]
    fn test_render_markdown_rich_contains_content() {
        let content = "# Heading\n\nThis is **bold** text.";
        let result = render_markdown(content, &rich_ctx());
        assert!(result.contains("Heading"));
        assert!(result.contains("bold"));
        assert!(result.contains("text"));
    }

    #[test]
    fn test_strip_markdown_headers() {
        assert!(strip_markdown("# H1").contains("H1"));
        assert!(strip_markdown("## H2").contains("H2"));
        assert!(strip_markdown("### H3").contains("H3"));
        assert!(!strip_markdown("# H1").contains('#'));
    }

    #[test]
    fn test_strip_markdown_emphasis() {
        assert_eq!(strip_markdown("**bold**"), "bold");
        assert_eq!(strip_markdown("*italic*"), "italic");
        assert_eq!(strip_markdown("__bold__"), "bold");
        assert_eq!(strip_markdown("_italic_"), "italic");
        assert_eq!(strip_markdown("~~strikethrough~~"), "strikethrough");
    }

    #[test]
    fn test_strip_markdown_links() {
        assert_eq!(strip_markdown("[text](https://example.com)"), "text");
        assert!(strip_markdown("[link text](url)").contains("link text"));
        assert!(!strip_markdown("[link](url)").contains("url"));
    }

    #[test]
    fn test_strip_markdown_code() {
        let result = strip_markdown("`inline code`");
        assert!(result.contains("inline code"));
        assert!(!result.contains('`'));
    }

    #[test]
    fn test_strip_markdown_code_blocks() {
        let content = "```rust\nfn main() {}\n```";
        let result = strip_markdown(content);
        assert!(result.contains("fn main()"));
        assert!(!result.contains("```"));
    }

    #[test]
    fn test_strip_markdown_blockquotes() {
        let result = strip_markdown("> quoted text");
        assert!(result.contains("quoted text"));
        // Blockquotes are indented
        assert!(result.starts_with("  "));
    }

    #[test]
    fn test_strip_markdown_horizontal_rule() {
        assert!(strip_markdown("---").contains("---"));
        assert!(strip_markdown("***").contains("---"));
    }

    #[test]
    fn test_contains_markdown_detection() {
        assert!(contains_markdown("**bold**"));
        assert!(contains_markdown("*italic*"));
        assert!(contains_markdown("[link](url)"));
        assert!(contains_markdown("# Header"));
        assert!(contains_markdown("```code```"));
        assert!(!contains_markdown("plain text without formatting"));
    }

    #[test]
    fn test_escape_markdown() {
        assert_eq!(escape_markdown("**bold**"), "\\*\\*bold\\*\\*");
        assert_eq!(escape_markdown("[link]"), "\\[link\\]");
        assert_eq!(escape_markdown("# header"), "\\# header");
        assert_eq!(escape_markdown("plain text"), "plain text");
    }

    #[test]
    fn test_strip_markdown_images() {
        let result = strip_markdown("![alt text](image.png)");
        assert!(result.contains("[Image: alt text]"));
        assert!(!result.contains("image.png"));
    }

    #[test]
    fn test_strip_markdown_nested_formatting() {
        let content = "**bold with *italic* inside**";
        let result = strip_markdown(content);
        assert!(result.contains("bold with"));
        assert!(result.contains("italic"));
        assert!(result.contains("inside"));
    }

    #[test]
    fn test_strip_markdown_empty() {
        assert!(strip_markdown("").is_empty());
    }

    #[test]
    fn test_render_markdown_multiline() {
        let content = "# Title\n\nParagraph one.\n\nParagraph two.";
        let result = render_markdown(content, &plain_ctx());
        assert!(result.contains("Title"));
        assert!(result.contains("Paragraph one"));
        assert!(result.contains("Paragraph two"));
    }
}
