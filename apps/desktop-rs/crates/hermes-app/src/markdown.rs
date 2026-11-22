//! Fast native streaming Markdown renderer for GPUI powered by `pulldown-cmark`.
//!
//! Renders headings, inline bold/italic/code via GPUI `StyledText` + `HighlightStyle`,
//! bullet/numbered lists, blockquotes, and fenced code blocks with language badges.

use std::ops::Range;

use gpui::{
    AnyElement, ClipboardItem, FontStyle, FontWeight, HighlightStyle, IntoElement, SharedString,
    StyledText, div, prelude::*, px,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::highlight::highlighted_code;
use crate::theme::HermesTheme;

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

    fn render(self, theme: &HermesTheme) -> StyledText {
        let highlights = self
            .spans
            .into_iter()
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
                (range, style)
            })
            .collect::<Vec<_>>();

        if highlights.is_empty() {
            StyledText::new(self.text)
        } else {
            StyledText::new(self.text).with_highlights(highlights)
        }
    }
}

#[derive(Debug, Clone)]
enum Block {
    Heading { level: u8, content: RichParagraph },
    Paragraph(RichParagraph),
    ListItem { bullet: String, content: RichParagraph },
    Blockquote(RichParagraph),
    CodeBlock { lang: String, code: String },
}

/// Parse Markdown into renderable blocks (kept separate from GPUI for fast unit testing).
fn parse_blocks(markdown: &str) -> Vec<Block> {
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
                         blocks: &mut Vec<Block>,
                         heading: Option<u8>,
                         blockquote: bool,
                         list_item: Option<String>| {
        if cur.is_empty() {
            *cur = RichParagraph::new();
            return;
        }
        let taken = std::mem::replace(cur, RichParagraph::new());
        if let Some(level) = heading {
            blocks.push(Block::Heading {
                level,
                content: taken,
            });
        } else if let Some(bullet) = list_item {
            blocks.push(Block::ListItem {
                bullet,
                content: taken,
            });
        } else if blockquote {
            blocks.push(Block::Blockquote(taken));
        } else {
            blocks.push(Block::Paragraph(taken));
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
                blocks.push(Block::CodeBlock {
                    lang: std::mem::take(&mut code_lang),
                    code,
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
        blocks.push(Block::CodeBlock {
            lang: code_lang,
            code: code_buf.trim_end_matches('\n').to_string(),
        });
    } else {
        flush_current(&mut current, &mut blocks, heading_level, in_blockquote, None);
    }

    blocks
}

/// Render Markdown text into a vertical stack of GPUI elements.
pub fn render_markdown(markdown: &str, theme: &HermesTheme) -> AnyElement {
    let blocks = parse_blocks(markdown);
    if blocks.is_empty() {
        return div()
            .text_sm()
            .text_color(theme.fg)
            .child(markdown.to_string())
            .into_any_element();
    }

    let mut container = div().flex().flex_col().gap_2().w_full();
    for (block_idx, block) in blocks.into_iter().enumerate() {
        let elem: AnyElement = match block {
            Block::Heading { level, content } => {
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
            Block::Paragraph(content) => div()
                .text_sm()
                .line_height(px(21.0))
                .text_color(theme.fg)
                .child(content.render(theme))
                .into_any_element(),
            Block::ListItem { bullet, content } => div()
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
                        .child(bullet),
                )
                .child(div().flex_1().child(content.render(theme)))
                .into_any_element(),
            Block::Blockquote(content) => div()
                .border_l_2()
                .border_color(theme.accent)
                .pl_3()
                .py_0p5()
                .text_sm()
                .text_color(theme.fg_muted)
                .child(content.render(theme))
                .into_any_element(),
            Block::CodeBlock { lang, code } => {
                let label = if lang.is_empty() {
                    "code".to_string()
                } else {
                    lang.clone()
                };
                let copy_text = code.clone();
                let highlighted = highlighted_code(&code, &lang, theme.mode);
                let copy_id = SharedString::from(format!("copy-code-{block_idx}-{}", code.len()));
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
                            .child(highlighted),
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
        let blocks = parse_blocks(md);
        assert!(matches!(blocks[0], Block::Heading { level: 3, .. }));
        assert!(matches!(blocks[1], Block::Paragraph(_)));
        assert!(matches!(blocks[2], Block::ListItem { .. }));
        assert!(matches!(blocks[3], Block::ListItem { .. }));
        assert!(matches!(blocks[4], Block::CodeBlock { ref lang, .. } if lang == "rust"));
    }

    #[test]
    fn handles_unclosed_streaming_fence() {
        let md = "Streaming:\n\n```python\nprint(1 + 2)";
        let blocks = parse_blocks(md);
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[1], Block::CodeBlock { ref lang, .. } if lang == "python"));
    }
}
