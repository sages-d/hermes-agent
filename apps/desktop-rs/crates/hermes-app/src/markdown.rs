//! Fast, memoized native streaming Markdown renderer for GPUI (`pulldown-cmark` + `syntect`).
//!
//! Performance invariants for multi-thousand-message sessions (e.g. Bot Chat):
//! - Parsed Markdown blocks AND `syntect` highlight spans are cached by
//!   `(content_hash, content_len, theme_mode, highlight_code)` in a bounded cache (512 entries).
//!   Typing, hovering, or streaming deltas at the tail perform ZERO re-parsing or re-highlighting
//!   of historical messages.
//! - Actively streaming messages (`streaming = true`) skip `syntect` until complete so tokens
//!   render in sub-millisecond time.
//! - Huge code blocks (> 16 KiB) degrade cleanly to plain monospace so a single giant log dump
//!   never stalls a frame.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

use gpui::{
    AnyElement, ClipboardItem, FontStyle, FontWeight, HighlightStyle, IntoElement, SharedString,
    StyledText, div, prelude::*, px,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::highlight::highlight_spans;
use crate::theme::{HermesTheme, ThemeMode};

const MAX_CACHE_ENTRIES: usize = 512;
const MAX_HIGHLIGHT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone)]
enum InlineSpanKind {
    Bold,
    Italic,
    Code,
}

#[derive(Debug, Clone)]
struct RichParagraph {
    text: String,
    spans: Vec<(Range<usize>, InlineSpanKind)>,
}

impl RichParagraph {
    fn new() -> Self {
        Self {
            text: String::new(),
            spans: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    fn push_text(&mut self, s: &str, bold: bool, italic: bool) {
        let start = self.text.len();
        self.text.push_str(s);
        let end = self.text.len();
        if end > start {
            if bold {
                self.spans.push((start..end, InlineSpanKind::Bold));
            }
            if italic {
                self.spans.push((start..end, InlineSpanKind::Italic));
            }
        }
    }

    fn push_code(&mut self, s: &str) {
        let start = self.text.len();
        self.text.push_str(s);
        let end = self.text.len();
        if end > start {
            self.spans.push((start..end, InlineSpanKind::Code));
        }
    }

    fn render(&self, theme: &HermesTheme) -> StyledText {
        if self.spans.is_empty() {
            return StyledText::new(self.text.clone());
        }
        let highlights = self
            .spans
            .iter()
            .map(|(range, kind)| {
                let style = match kind {
                    InlineSpanKind::Bold => HighlightStyle {
                        font_weight: Some(FontWeight::SEMIBOLD),
                        ..Default::default()
                    },
                    InlineSpanKind::Italic => HighlightStyle {
                        font_style: Some(FontStyle::Italic),
                        ..Default::default()
                    },
                    InlineSpanKind::Code => HighlightStyle {
                        color: Some(theme.accent),
                        background_color: Some(theme.code_bg),
                        ..Default::default()
                    },
                };
                (range.clone(), style)
            })
            .collect::<Vec<_>>();

        StyledText::new(self.text.clone()).with_highlights(highlights)
    }
}

#[derive(Debug, Clone)]
enum CachedBlock {
    Heading {
        level: u8,
        content: RichParagraph,
    },
    Paragraph(RichParagraph),
    ListItem {
        bullet: String,
        content: RichParagraph,
    },
    Blockquote(RichParagraph),
    CodeBlock {
        lang: String,
        code: String,
        /// Pre-computed `syntect` spans for `(theme_mode, highlight_code)`.
        highlights: Vec<(Range<usize>, HighlightStyle)>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
    hash: u64,
    len: usize,
    mode: ThemeMode,
    highlight_code: bool,
}

fn cache() -> &'static Mutex<HashMap<CacheKey, Arc<Vec<CachedBlock>>>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, Arc<Vec<CachedBlock>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::with_capacity(128)))
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn cached_blocks(markdown: &str, mode: ThemeMode, highlight_code: bool) -> Arc<Vec<CachedBlock>> {
    // Do not churn the cache for actively streaming (mutating every delta) messages.
    if !highlight_code {
        return Arc::new(parse_blocks(markdown, mode, false));
    }

    let key = CacheKey {
        hash: hash_str(markdown),
        len: markdown.len(),
        mode,
        highlight_code,
    };

    if let Ok(guard) = cache().lock()
        && let Some(hit) = guard.get(&key)
    {
        return Arc::clone(hit);
    }

    let parsed = Arc::new(parse_blocks(markdown, mode, true));
    if let Ok(mut guard) = cache().lock() {
        if guard.len() >= MAX_CACHE_ENTRIES {
            guard.clear();
        }
        guard.insert(key, Arc::clone(&parsed));
    }
    parsed
}

/// Parse Markdown into cached renderable blocks (including pre-computed syntax highlight spans).
fn parse_blocks(markdown: &str, mode: ThemeMode, highlight_code: bool) -> Vec<CachedBlock> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(markdown, opts);
    let mut blocks = Vec::new();
    let mut current = RichParagraph::new();
    let mut bold = false;
    let mut italic = false;
    let mut heading_level: Option<u8> = None;
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    let mut in_blockquote = false;
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut in_list_item = false;

    let flush_current = |cur: &mut RichParagraph,
                         blocks: &mut Vec<CachedBlock>,
                         heading: Option<u8>,
                         blockquote: bool,
                         list_item: Option<String>| {
        if cur.is_empty() {
            *cur = RichParagraph::new();
            return;
        }
        let taken = std::mem::replace(cur, RichParagraph::new());
        if let Some(level) = heading {
            blocks.push(CachedBlock::Heading {
                level,
                content: taken,
            });
        } else if let Some(bullet) = list_item {
            blocks.push(CachedBlock::ListItem {
                bullet,
                content: taken,
            });
        } else if blockquote {
            blocks.push(CachedBlock::Blockquote(taken));
        } else {
            blocks.push(CachedBlock::Paragraph(taken));
        }
    };

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                flush_current(&mut current, &mut blocks, None, in_blockquote, None);
                heading_level = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    _ => 4,
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                flush_current(&mut current, &mut blocks, heading_level.take(), false, None);
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if !in_list_item {
                    flush_current(&mut current, &mut blocks, None, in_blockquote, None);
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                in_blockquote = true;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush_current(&mut current, &mut blocks, None, true, None);
                in_blockquote = false;
            }
            Event::Start(Tag::List(start)) => {
                list_stack.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
            }
            Event::Start(Tag::Item) => {
                in_list_item = true;
            }
            Event::End(TagEnd::Item) => {
                let bullet = match list_stack.last_mut() {
                    Some(Some(n)) => {
                        let label = format!("{n}.");
                        *n += 1;
                        label
                    }
                    _ => "•".to_string(),
                };
                flush_current(&mut current, &mut blocks, None, false, Some(bullet));
                in_list_item = false;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush_current(&mut current, &mut blocks, None, in_blockquote, None);
                in_code_block = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                let code = code_buf.trim_end_matches('\n').to_string();
                let lang = std::mem::take(&mut code_lang);
                let highlights = if highlight_code && code.len() <= MAX_HIGHLIGHT_BYTES {
                    highlight_spans(&code, &lang, mode)
                } else {
                    Vec::new()
                };
                blocks.push(CachedBlock::CodeBlock {
                    lang,
                    code,
                    highlights,
                });
                code_buf.clear();
            }
            Event::Start(Tag::Strong) => bold = true,
            Event::End(TagEnd::Strong) => bold = false,
            Event::Start(Tag::Emphasis) => italic = true,
            Event::End(TagEnd::Emphasis) => italic = false,
            Event::Text(t) => {
                if in_code_block {
                    code_buf.push_str(&t);
                } else {
                    current.push_text(&t, bold, italic);
                }
            }
            Event::Code(c) => {
                current.push_code(&c);
            }
            Event::SoftBreak => {
                if in_code_block {
                    code_buf.push('\n');
                } else {
                    current.push_text(" ", bold, italic);
                }
            }
            Event::HardBreak => {
                if in_code_block {
                    code_buf.push('\n');
                } else {
                    current.push_text("\n", bold, italic);
                }
            }
            _ => {}
        }
    }

    // Handle mid-stream unclosed code fences or trailing paragraphs gracefully.
    if in_code_block && !code_buf.is_empty() {
        let code = code_buf.trim_end_matches('\n').to_string();
        let highlights = if highlight_code && code.len() <= MAX_HIGHLIGHT_BYTES {
            highlight_spans(&code, &code_lang, mode)
        } else {
            Vec::new()
        };
        blocks.push(CachedBlock::CodeBlock {
            lang: code_lang,
            code,
            highlights,
        });
    } else {
        flush_current(&mut current, &mut blocks, heading_level, in_blockquote, None);
    }

    blocks
}

/// Render Markdown text into a vertical stack of GPUI elements using the memoized block cache.
/// Pass `streaming = true` for the active in-flight turn to skip expensive regex highlighting
/// until the turn settles.
pub fn render_markdown(
    msg_idx: usize,
    markdown: &str,
    theme: &HermesTheme,
    streaming: bool,
) -> AnyElement {
    let blocks = cached_blocks(markdown, theme.mode, !streaming);
    if blocks.is_empty() {
        return div()
            .text_sm()
            .text_color(theme.fg)
            .child(markdown.to_string())
            .into_any_element();
    }

    let mut container = div().flex().flex_col().gap_2().w_full();
    for (block_idx, block) in blocks.iter().enumerate() {
        let elem: AnyElement = match block {
            CachedBlock::Heading { level, content } => {
                let mut d = div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.fg)
                    .mt_1();
                d = match level {
                    1 => d.text_lg(),
                    2 => d.text_base(),
                    _ => d.text_sm(),
                };
                d.child(content.render(theme)).into_any_element()
            }
            CachedBlock::Paragraph(content) => div()
                .text_sm()
                .line_height(px(21.0))
                .text_color(theme.fg)
                .child(content.render(theme))
                .into_any_element(),
            CachedBlock::ListItem { bullet, content } => div()
                .flex()
                .flex_row()
                .gap_2()
                .pl_2()
                .text_sm()
                .line_height(px(21.0))
                .text_color(theme.fg)
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.fg_muted)
                        .child(bullet.clone()),
                )
                .child(div().flex_1().child(content.render(theme)))
                .into_any_element(),
            CachedBlock::Blockquote(content) => div()
                .border_l_2()
                .border_color(theme.accent)
                .pl_3()
                .py_0p5()
                .text_sm()
                .text_color(theme.fg_muted)
                .child(content.render(theme))
                .into_any_element(),
            CachedBlock::CodeBlock {
                lang,
                code,
                highlights,
            } => {
                let label = if lang.is_empty() {
                    "code".to_string()
                } else {
                    lang.clone()
                };
                let copy_text = code.clone();
                let styled_code = if highlights.is_empty() {
                    StyledText::new(code.clone())
                } else {
                    StyledText::new(code.clone()).with_highlights(highlights.clone())
                };
                let copy_id = SharedString::from(format!("copy-code-{msg_idx}-{block_idx}"));
                let hover_bg = theme.surface_hover;
                let fg_color = theme.fg;
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .rounded_md()
                    .bg(theme.code_bg)
                    .border_1()
                    .border_color(theme.border)
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px_3()
                            .py_1()
                            .border_b_1()
                            .border_color(theme.border)
                            .text_xs()
                            .text_color(theme.fg_muted)
                            .child(label)
                            .child(
                                div()
                                    .id(copy_id)
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(hover_bg).text_color(fg_color))
                                    .child("Copy")
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_text.clone(),
                                        ));
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .text_xs()
                            .line_height(px(18.0))
                            .font_family("Menlo")
                            .text_color(theme.fg)
                            .child(styled_code),
                    )
                    .into_any_element()
            }
        };
        container = container.child(elem);
    }

    container.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_lists_and_fenced_code() {
        let md = "### Title\n\nHello **world** with `code`.\n\n- item one\n- item two\n\n```rust\nfn main() {}\n```";
        let blocks = parse_blocks(md, ThemeMode::Dark, true);
        assert!(matches!(blocks[0], CachedBlock::Heading { level: 3, .. }));
        assert!(matches!(blocks[1], CachedBlock::Paragraph(_)));
        assert!(matches!(blocks[2], CachedBlock::ListItem { .. }));
        assert!(matches!(blocks[3], CachedBlock::ListItem { .. }));
        assert!(matches!(
            blocks[4],
            CachedBlock::CodeBlock { ref lang, ref highlights, .. }
            if lang == "rust" && !highlights.is_empty()
        ));
    }

    #[test]
    fn handles_unclosed_streaming_fence_without_blocking_highlight() {
        let md = "Streaming:\n\n```python\nprint(1 + 2)";
        let blocks = parse_blocks(md, ThemeMode::Dark, false);
        assert_eq!(blocks.len(), 2);
        assert!(matches!(
            blocks[1],
            CachedBlock::CodeBlock { ref lang, ref highlights, .. }
            if lang == "python" && highlights.is_empty()
        ));
    }

    #[test]
    fn memoizes_parsed_markdown_across_repeated_renders() {
        let md = "Cached **markdown** with ```rust\nlet x = 1;\n```";
        let first = cached_blocks(md, ThemeMode::Dark, true);
        let second = cached_blocks(md, ThemeMode::Dark, true);
        assert!(Arc::ptr_eq(&first, &second));
    }
}
