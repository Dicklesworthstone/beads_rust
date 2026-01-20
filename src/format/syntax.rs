//! Syntax highlighting for code blocks.
//!
//! Provides mode-aware syntax highlighting for code embedded in issue descriptions
//! and comments. Uses `rich_rust`'s Syntax component when available.
//!
//! # Mode Behavior
//!
//! - **Rich**: Full color syntax highlighting via syntect
//! - **Plain**: Indented code block with no colors
//! - **JSON**: Raw code string unchanged
//! - **Quiet**: No output
//!
//! # Example
//!
//! ```ignore
//! use beads_rust::format::syntax::highlight_code;
//! use beads_rust::format::OutputContext;
//!
//! let code = r#"fn main() { println!("Hello!"); }"#;
//! let ctx = OutputContext::detect();
//! let highlighted = highlight_code(code, "rust", &ctx);
//! ```

use crate::format::context::{OutputContext, OutputMode};
use rich_rust::color::ColorSystem;
use rich_rust::renderables::syntax::{Syntax, SyntaxError};

/// Highlight code with syntax-aware coloring based on output mode.
///
/// # Arguments
///
/// * `code` - The source code to highlight
/// * `language` - The programming language (e.g., "rust", "python", "go")
/// * `ctx` - The output context determining rendering mode
///
/// # Returns
///
/// A string with the highlighted code. In Rich mode, includes ANSI escape codes.
/// In Plain mode, returns indented code. In JSON mode, returns raw code.
///
/// # Language Support
///
/// Supported languages include: rust, python, go, typescript, javascript, sql,
/// bash, yaml, json, toml, html, css, markdown, and 100+ others via syntect.
///
/// Unknown languages fall back to plain text rendering without highlighting.
#[must_use]
pub fn highlight_code(code: &str, language: &str, ctx: &OutputContext) -> String {
    match ctx.mode() {
        OutputMode::Quiet => String::new(),
        OutputMode::Json => code.to_string(),
        OutputMode::Plain => format_plain_code(code),
        OutputMode::Rich => highlight_rich(code, language, ctx.width()),
    }
}

/// Format code as a plain indented block (no colors).
fn format_plain_code(code: &str) -> String {
    code.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Highlight code using rich_rust's Syntax component.
fn highlight_rich(code: &str, language: &str, width: usize) -> String {
    // Normalize language name for syntect
    let lang = normalize_language(language);

    // Try to highlight with syntect, fall back to plain text on failure
    try_highlight(&lang, code, width).unwrap_or_else(|_| format_plain_code(code))
}

/// Attempt to highlight code, returning the rendered string.
fn try_highlight(language: &str, code: &str, _width: usize) -> Result<String, SyntaxError> {
    let line_count = code.lines().count();
    let show_line_numbers = line_count > 5;

    let syntax = Syntax::new(code, language)
        .line_numbers(show_line_numbers)
        .theme("base16-ocean.dark");

    let segments = syntax.render(None)?;

    // Render segments to a string with ANSI codes
    let mut result = String::new();
    for segment in segments {
        if let Some(style) = &segment.style {
            result.push_str(&style.render(&segment.text, ColorSystem::TrueColor));
        } else {
            result.push_str(&segment.text);
        }
    }

    Ok(result)
}

/// Normalize language name to syntect's expected format.
fn normalize_language(language: &str) -> String {
    match language.to_lowercase().as_str() {
        // Common aliases
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" | "tsx" | "jsx" => "typescript",
        "sh" | "shell" => "bash",
        "yml" => "yaml",
        "md" => "markdown",
        "rb" => "ruby",
        "kt" => "kotlin",
        "cpp" | "cxx" | "cc" | "c++" => "c++",
        "cs" => "c#",
        "dockerfile" => "docker",
        // Already normalized
        lang => lang,
    }
    .to_string()
}

/// Parse a code fence and extract the language and code content.
///
/// # Example
///
/// ```ignore
/// let fence = "```rust\nfn main() {}\n```";
/// let (lang, code) = parse_code_fence(fence);
/// assert_eq!(lang, "rust");
/// assert_eq!(code, "fn main() {}");
/// ```
#[must_use]
pub fn parse_code_fence(fence: &str) -> (String, String) {
    let lines: Vec<&str> = fence.lines().collect();

    if lines.is_empty() {
        return (String::from("text"), String::new());
    }

    // Check for opening fence
    let first_line = lines[0].trim();
    if !first_line.starts_with("```") {
        // Not a fenced code block, treat as plain text
        return (String::from("text"), fence.to_string());
    }

    // Extract language from opening fence
    let language = first_line.trim_start_matches("```").trim().to_lowercase();
    let lang = if language.is_empty() {
        String::from("text")
    } else {
        language
    };

    // Extract code (skip first and last line if it's a closing fence)
    let code_lines: Vec<&str> = if lines.len() > 1 {
        let end = if lines
            .last()
            .is_some_and(|last| last.trim().starts_with("```"))
        {
            lines.len() - 1
        } else {
            lines.len()
        };
        lines[1..end].to_vec()
    } else {
        Vec::new()
    };

    (lang, code_lines.join("\n"))
}

/// Detect language from a filename or extension.
#[must_use]
pub fn detect_language_from_filename(filename: &str) -> String {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();

    match ext.as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        "go" => "go",
        "rb" => "ruby",
        "java" => "java",
        "c" => "c",
        "cpp" | "cxx" | "cc" | "h" | "hpp" => "c++",
        "cs" => "c#",
        "php" => "php",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "scala" => "scala",
        "sh" | "bash" => "bash",
        "zsh" => "zsh",
        "ps1" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "dockerfile" => "docker",
        "makefile" => "makefile",
        _ => "text",
    }
    .to_string()
}

/// Get list of supported language names.
#[must_use]
pub fn supported_languages() -> Vec<String> {
    Syntax::available_languages()
}

/// Get list of available themes.
#[must_use]
pub fn available_themes() -> Vec<String> {
    Syntax::available_themes()
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
    fn test_highlight_code_plain_mode() {
        let code = "fn main() {}";
        let result = highlight_code(code, "rust", &plain_ctx());
        assert_eq!(result, "    fn main() {}");
        assert!(!result.contains("\x1b[")); // No ANSI codes
    }

    #[test]
    fn test_highlight_code_plain_multiline() {
        let code = "line1\nline2\nline3";
        let result = highlight_code(code, "text", &plain_ctx());
        assert_eq!(result, "    line1\n    line2\n    line3");
    }

    #[test]
    fn test_highlight_code_json_mode_unchanged() {
        let code = "fn main() { println!(\"test\"); }";
        let result = highlight_code(code, "rust", &json_ctx());
        assert_eq!(result, code); // Unchanged
    }

    #[test]
    fn test_highlight_code_quiet_mode_empty() {
        let code = "fn main() {}";
        let result = highlight_code(code, "rust", &quiet_ctx());
        assert!(result.is_empty());
    }

    #[test]
    fn test_highlight_code_rich_mode_rust() {
        let code = "fn main() { println!(\"Hello\"); }";
        let result = highlight_code(code, "rust", &rich_ctx());
        // Should contain the code text
        assert!(result.contains("fn"));
        assert!(result.contains("main"));
        assert!(result.contains("println"));
    }

    #[test]
    fn test_highlight_code_rich_mode_python() {
        let code = "def hello():\n    print('world')";
        let result = highlight_code(code, "python", &rich_ctx());
        assert!(result.contains("def"));
        assert!(result.contains("hello"));
    }

    #[test]
    fn test_highlight_code_unknown_language_fallback() {
        let code = "some random text";
        let result = highlight_code(code, "nonexistent_language_xyz", &rich_ctx());
        // Should fall back to plain formatting
        assert!(result.contains("some random text"));
    }

    #[test]
    fn test_highlight_code_empty() {
        let result = highlight_code("", "rust", &rich_ctx());
        assert!(result.is_empty() || result.chars().all(char::is_whitespace));
    }

    #[test]
    fn test_normalize_language() {
        assert_eq!(normalize_language("rs"), "rust");
        assert_eq!(normalize_language("RS"), "rust");
        assert_eq!(normalize_language("py"), "python");
        assert_eq!(normalize_language("js"), "javascript");
        assert_eq!(normalize_language("ts"), "typescript");
        assert_eq!(normalize_language("tsx"), "typescript");
        assert_eq!(normalize_language("sh"), "bash");
        assert_eq!(normalize_language("yml"), "yaml");
        assert_eq!(normalize_language("rust"), "rust"); // Already normalized
    }

    #[test]
    fn test_parse_code_fence_with_language() {
        let fence = "```rust\nfn main() {}\n```";
        let (lang, code) = parse_code_fence(fence);
        assert_eq!(lang, "rust");
        assert_eq!(code, "fn main() {}");
    }

    #[test]
    fn test_parse_code_fence_no_language() {
        let fence = "```\nsome code\n```";
        let (lang, code) = parse_code_fence(fence);
        assert_eq!(lang, "text");
        assert_eq!(code, "some code");
    }

    #[test]
    fn test_parse_code_fence_no_closing() {
        let fence = "```python\nprint('hello')\nprint('world')";
        let (lang, code) = parse_code_fence(fence);
        assert_eq!(lang, "python");
        assert_eq!(code, "print('hello')\nprint('world')");
    }

    #[test]
    fn test_parse_code_fence_not_fenced() {
        let text = "just some plain text";
        let (lang, code) = parse_code_fence(text);
        assert_eq!(lang, "text");
        assert_eq!(code, text);
    }

    #[test]
    fn test_parse_code_fence_empty() {
        let (lang, code) = parse_code_fence("");
        assert_eq!(lang, "text");
        assert!(code.is_empty());
    }

    #[test]
    fn test_detect_language_from_filename() {
        assert_eq!(detect_language_from_filename("main.rs"), "rust");
        assert_eq!(detect_language_from_filename("script.py"), "python");
        assert_eq!(detect_language_from_filename("app.tsx"), "typescript");
        assert_eq!(detect_language_from_filename("config.yaml"), "yaml");
        assert_eq!(detect_language_from_filename("Dockerfile"), "docker");
        assert_eq!(detect_language_from_filename("unknown.xyz"), "text");
    }

    #[test]
    fn test_supported_languages_not_empty() {
        let langs = supported_languages();
        assert!(!langs.is_empty());
    }

    #[test]
    fn test_available_themes_not_empty() {
        let themes = available_themes();
        assert!(!themes.is_empty());
    }

    #[test]
    fn test_highlight_long_code_with_line_numbers() {
        // Code with > 5 lines should show line numbers in rich mode
        let code = "line1\nline2\nline3\nline4\nline5\nline6\nline7";
        let result = highlight_code(code, "text", &rich_ctx());
        // Just verify it doesn't crash and contains the content
        assert!(result.contains("line1"));
        assert!(result.contains("line7"));
    }
}
