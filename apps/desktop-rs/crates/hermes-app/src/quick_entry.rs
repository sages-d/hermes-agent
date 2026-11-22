//! Floating Quick Entry mini-composer window (`Cmd+Shift+K`).
//!
//! Captures a prompt fast and dispatches it through the main companion's command channel,
//! dismissing immediately on `Enter` or `Esc`.

use gpui::{
    ClipboardItem, Context, FocusHandle, Focusable, FontWeight, HighlightStyle, KeyDownEvent,
    StyledText, Window, div, prelude::*,
};
use tokio::sync::mpsc;

use crate::app_view::UiCommand;
use crate::composer::TextBuffer;
use crate::theme::HermesTheme;

pub struct QuickEntryView {
    buffer: TextBuffer,
    theme: HermesTheme,
    focus_handle: FocusHandle,
    target_session_id: Option<String>,
    target_title: String,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
}

impl QuickEntryView {
    pub fn new(
        theme: HermesTheme,
        target_session_id: Option<String>,
        target_title: String,
        cmd_tx: mpsc::UnboundedSender<UiCommand>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            buffer: TextBuffer::default(),
            theme,
            focus_handle: cx.focus_handle(),
            target_session_id,
            target_title,
            cmd_tx,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.buffer.commit_submission() else {
            return;
        };
        if text.starts_with('/') {
            let _ = self.cmd_tx.send(UiCommand::RunSlash {
                session_id: self.target_session_id.clone(),
                command: text,
            });
        } else {
            let _ = self.cmd_tx.send(UiCommand::SubmitPrompt {
                session_id: self.target_session_id.clone(),
                text,
            });
        }
        window.remove_window();
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;
        let primary_mod = mods.platform || mods.control;

        if primary_mod {
            match key {
                "a" => {
                    self.buffer.select_all();
                    cx.notify();
                    return;
                }
                "c" => {
                    if let Some(sel) = self.buffer.selected_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(sel.to_string()));
                    }
                    return;
                }
                "x" => {
                    if let Some(cut) = self.buffer.cut() {
                        cx.write_to_clipboard(ClipboardItem::new_string(cut));
                        cx.notify();
                    }
                    return;
                }
                "v" => {
                    if let Some(item) = cx.read_from_clipboard()
                        && let Some(text) = item.text()
                    {
                        self.buffer.insert(&text);
                        cx.notify();
                    }
                    return;
                }
                "left" => {
                    self.buffer.move_home(mods.shift);
                    cx.notify();
                    return;
                }
                "right" => {
                    self.buffer.move_end(mods.shift);
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }

        match key {
            "escape" => {
                window.remove_window();
            }
            "enter" => {
                if mods.shift {
                    self.buffer.insert("\n");
                    cx.notify();
                } else {
                    self.submit(window, cx);
                }
            }
            "backspace" => {
                if mods.alt {
                    self.buffer.delete_word_backward();
                } else {
                    self.buffer.backspace();
                }
                cx.notify();
            }
            "delete" => {
                self.buffer.delete_forward();
                cx.notify();
            }
            "left" => {
                if mods.alt {
                    self.buffer.move_word_left(mods.shift);
                } else {
                    self.buffer.move_left(mods.shift);
                }
                cx.notify();
            }
            "right" => {
                if mods.alt {
                    self.buffer.move_word_right(mods.shift);
                } else {
                    self.buffer.move_right(mods.shift);
                }
                cx.notify();
            }
            _ => {
                if !primary_mod
                    && !mods.alt
                    && let Some(ch) = &event.keystroke.key_char
                    && !ch.chars().all(|c| c.is_control())
                {
                    self.buffer.insert(ch);
                    cx.notify();
                }
            }
        }
    }
}

impl Focusable for QuickEntryView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QuickEntryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle);
        }

        let theme = &self.theme;
        let input_content = if self.buffer.is_empty() {
            StyledText::new("Ask Hermes anything… (Enter to send · Esc to dismiss)")
        } else {
            let (text, sel) = self.buffer.render_parts();
            if let Some(range) = sel {
                StyledText::new(text).with_highlights([(
                    range,
                    HighlightStyle {
                        background_color: Some(theme.accent_subtle),
                        ..Default::default()
                    },
                )])
            } else {
                StyledText::new(text)
            }
        };

        div()
            .id("quick-entry-root")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_col()
            .justify_between()
            .size_full()
            .p_4()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.accent)
            .text_color(theme.fg)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.accent)
                                    .child("QUICK ENTRY"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.fg_muted)
                                    .child(format!("→ {}", self.target_title)),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.fg_muted)
                            .child("Esc to close"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .mt_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(if self.buffer.is_empty() {
                                theme.fg_muted
                            } else {
                                theme.fg
                            })
                            .child(input_content),
                    )
                    .child(
                        div()
                            .id("quick-entry-send")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(theme.accent)
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.accent_fg)
                            .cursor_pointer()
                            .child("Send ↵")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit(window, cx);
                            })),
                    ),
            )
    }
}
