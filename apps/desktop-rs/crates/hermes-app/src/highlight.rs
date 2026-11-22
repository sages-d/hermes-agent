//! Pure-Rust syntax highlighting for fenced Markdown code blocks via `syntect` (`default-fancy`).
//!
//! Syntax & theme sets are loaded once in `OnceLock` so streaming frames stay allocation-light.

use std::ops::Range;
use std::sync::OnceLock;

use gpui::{FontWeight, HighlightStyle, Rgba, StyledText};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle as SynFontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::theme::ThemeMode;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_for(mode: ThemeMode) -> &'static Theme {
    let ts = THEME_SET.get_or_init(ThemeSet::load_defaults);
    let name = match mode {
        ThemeMode::Dark => "base16-ocean.dark",
        ThemeMode::Light => "InspiredGitHub",
    };
    ts.themes
        .get(name)
        .or_else(|| ts.themes.values().next())
        .expect("syntect default themes missing")
}

/// Compute GPUI `(Range<usize>, HighlightStyle)` spans for a code block.
pub fn highlight_spans(code: &str, lang: &str, mode: ThemeMode) -> Vec<(Range<usize>, HighlightStyle)> {
    let ss = syntax_set();
    let token = lang.trim().split_whitespace().next().unwrap_or("");
    let syntax = if token.is_empty() {
        ss.find_syntax_plain_text()
    } else {
        ss.find_syntax_by_token(token)
            .or_else(|| ss.find_syntax_by_extension(token))
            .unwrap_or_else(|| ss.find_syntax_plain_text())
    };

    let mut highlighter = HighlightLines::new(syntax, theme_for(mode));
    let mut highlights = Vec::new();
    let mut offset = 0usize;

    for line in LinesWithEndings::from(code) {
        if let Ok(ranges) = highlighter.highlight_line(line, ss) {
            for (style, slice) in ranges {
                let len = slice.len();
                let end = offset + len;
                if len > 0 && !slice.trim().is_empty() {
                    let fg = style.foreground;
                    let color = Rgba {
                        r: f32::from(fg.r) / 255.0,
                        g: f32::from(fg.g) / 255.0,
                        b: f32::from(fg.b) / 255.0,
                        a: f32::from(fg.a) / 255.0,
                    };
                    let font_weight = if style.font_style.contains(SynFontStyle::BOLD) {
                        Some(FontWeight::SEMIBOLD)
                    } else {
                        None
                    };
                    let font_style = if style.font_style.contains(SynFontStyle::ITALIC) {
                        Some(gpui::FontStyle::Italic)
                    } else {
                        None
                    };
                    highlights.push((
                        offset..end,
                        HighlightStyle {
                            color: Some(color.into()),
                            font_weight,
                            font_style,
                            ..Default::default()
                        },
                    ));
                }
                offset = end;
            }
        } else {
            offset += line.len();
        }
    }

    highlights
}

/// Render a syntax-highlighted GPUI `StyledText` for `code`.
pub fn highlighted_code(code: &str, lang: &str, mode: ThemeMode) -> StyledText {
    let spans = highlight_spans(code, lang, mode);
    if spans.is_empty() {
        StyledText::new(code.to_string())
    } else {
        StyledText::new(code.to_string()).with_highlights(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust_and_python_code_without_panicking() {
        let rust_spans = highlight_spans("fn main() {\n    let x = 42;\n}\n", "rust", ThemeMode::Dark);
        assert!(!rust_spans.is_empty());

        let py_spans = highlight_spans("def greet(name: str) -> str:\n    return f'hi {name}'\n", "py", ThemeMode::Light);
        assert!(!py_spans.is_empty());

        // Unknown languages gracefully degrade to plain text without erroring.
        let _ = highlight_spans("some raw text", "nonexistent_lang_xyz", ThemeMode::Dark);
    }
}
