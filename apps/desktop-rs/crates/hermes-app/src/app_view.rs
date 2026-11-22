//! Root GPUI view for `hermes-companion` (macOS-first Milestone 0).
//!
//! Layout follows `apps/desktop/DESIGN.md`:
//! - Integrated macOS titlebar with traffic-light clearance, sidebar toggle, session title,
//!   connection status pill, theme toggle, and New Chat button.
//! - Collapsible session sidebar with Bot Mode badge (`"Bot Chat"`) and live running dots.
//! - Flat, scrollable transcript pane with streaming Markdown, collapsible thinking strip,
//!   tool-call rows, and inline approval/clarify decision cards.
//! - Keyboard-driven composer with `/` slash command completion popup.

use gpui::{
    AnyElement, Context, FocusHandle, Focusable, FontWeight, KeyDownEvent, SharedString, Window,
    div, prelude::*, px,
};
use tokio::sync::mpsc;

use crate::markdown::render_markdown;
use crate::state::{
    ChatItem, CompanionState, ConnectionStatus, PendingPrompt, PendingPromptKind, SessionSummary,
    SlashCommandItem, ToolStatus,
};
use crate::theme::{HermesTheme, ThemeMode};

/// Actions emitted from the GPUI view to the background Tokio gateway worker.
#[derive(Debug, Clone)]
pub enum UiCommand {
    Reconnect,
    NewSession,
    SelectSession(String),
    SubmitPrompt {
        session_id: Option<String>,
        text: String,
    },
    RunSlash {
        session_id: Option<String>,
        command: String,
    },
    CancelPrompt(String),
    AnswerPrompt {
        request_id: String,
        choice: String,
    },
}

/// Messages pushed from the background Tokio gateway worker into the GPUI view.
#[derive(Debug)]
pub enum WorkerEvent {
    Status(ConnectionStatus),
    SessionList(hermes_protocol::CanonicalListResult),
    Snapshot(hermes_protocol::CanonicalSnapshot),
    GatewayEvent(hermes_protocol::GatewayEventEnvelope),
    SlashCatalog(Vec<SlashCommandItem>),
    ServerPrompt(PendingPrompt),
    Notice { text: String, is_error: bool },
}

pub struct CompanionView {
    state: CompanionState,
    theme: HermesTheme,
    composer: String,
    focus_handle: FocusHandle,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
}

impl CompanionView {
    pub fn new(
        initial_state: CompanionState,
        cmd_tx: mpsc::UnboundedSender<UiCommand>,
        mut worker_rx: mpsc::UnboundedReceiver<WorkerEvent>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        // Zero-polling async bridge: tokio::sync::mpsc is runtime-agnostic and wakes GPUI directly.
        cx.spawn(async move |this, cx| {
            while let Some(ev) = worker_rx.recv().await {
                let updated = this.update(cx, |view, cx| {
                    view.handle_worker_event(ev);
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            state: initial_state,
            theme: HermesTheme::nous_dark(),
            composer: String::new(),
            focus_handle,
            cmd_tx,
        }
    }

    fn handle_worker_event(&mut self, ev: WorkerEvent) {
        match ev {
            WorkerEvent::Status(status) => {
                self.state.status = status;
            }
            WorkerEvent::SessionList(list) => {
                self.state.load_session_list(list);
            }
            WorkerEvent::Snapshot(snap) => {
                self.state.load_snapshot(snap);
            }
            WorkerEvent::GatewayEvent(env) => {
                self.state.apply_event(env);
            }
            WorkerEvent::SlashCatalog(items) => {
                if !items.is_empty() {
                    self.state.slash_catalog = items;
                }
            }
            WorkerEvent::ServerPrompt(prompt) => {
                if !self.state.pending_prompts.iter().any(|p| p.id == prompt.id) {
                    self.state.pending_prompts.push(prompt);
                }
            }
            WorkerEvent::Notice { text, is_error } => {
                self.state.messages.push(ChatItem::Notice { text, is_error });
            }
        }
    }

    fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.composer.clear();
        self.state.selected_slash_idx = 0;

        // Handle local client-side built-ins immediately.
        if text == "/clear" {
            self.state.messages.clear();
            cx.notify();
            return;
        }
        if text == "/new" {
            self.start_new_chat(cx);
            return;
        }

        // Demo mode simulates a streaming response locally so the UI can be tested offline.
        if matches!(self.state.status, ConnectionStatus::Demo) {
            self.state.messages.push(ChatItem::User { text: text.clone() });
            let reply = if text.starts_with('/') {
                format!("Executed `{text}` in offline demo mode.\n\nRun without `--demo` to attach to `hermes gateway attach --json`.")
            } else {
                format!(
                    "Echoing from **offline demo mode**:\n\n> {text}\n\nLaunch `hermes-companion` without `--demo` (or pass `--url ws://127.0.0.1:9119/api/ws`) to stream live turns from your Hermes gateway."
                )
            };
            self.state.messages.push(ChatItem::Assistant {
                text: reply,
                reasoning: String::new(),
                streaming: false,
            });
            cx.notify();
            return;
        }

        if text.starts_with('/') {
            self.state.messages.push(ChatItem::User { text: text.clone() });
            let _ = self.cmd_tx.send(UiCommand::RunSlash {
                session_id: self.state.active_session_id.clone(),
                command: text,
            });
        } else {
            self.state.messages.push(ChatItem::User { text: text.clone() });
            self.state.is_streaming = true;
            let _ = self.cmd_tx.send(UiCommand::SubmitPrompt {
                session_id: self.state.active_session_id.clone(),
                text,
            });
        }
        cx.notify();
    }

    fn start_new_chat(&mut self, cx: &mut Context<Self>) {
        self.state.active_session_id = None;
        self.state.active_title = "New Chat".to_string();
        self.state.messages.clear();
        self.state.is_streaming = false;
        self.state.last_seq = 0;
        let _ = self.cmd_tx.send(UiCommand::NewSession);
        cx.notify();
    }

    fn select_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        if self.state.active_session_id.as_deref() == Some(session_id.as_str()) {
            return;
        }
        if let Some(s) = self.state.sessions.iter().find(|s| s.session_id == session_id) {
            self.state.active_title = s.title.clone();
        }
        self.state.active_session_id = Some(session_id.clone());
        if !matches!(self.state.status, ConnectionStatus::Demo) {
            self.state.messages.clear();
            let _ = self.cmd_tx.send(UiCommand::SelectSession(session_id));
        }
        cx.notify();
    }

    fn answer_prompt(&mut self, request_id: String, choice: String, cx: &mut Context<Self>) {
        self.state.pending_prompts.retain(|p| p.id != request_id);
        if matches!(self.state.status, ConnectionStatus::Demo) {
            self.state.messages.push(ChatItem::Notice {
                text: format!("Demo approval settled: {choice}"),
                is_error: false,
            });
        } else {
            let _ = self.cmd_tx.send(UiCommand::AnswerPrompt { request_id, choice });
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;

        // Global shortcuts (Cmd on macOS, Ctrl elsewhere)
        let primary_mod = mods.platform || mods.control;
        if primary_mod {
            match key {
                "n" => {
                    self.start_new_chat(cx);
                    return;
                }
                "b" => {
                    self.state.sidebar_open = !self.state.sidebar_open;
                    cx.notify();
                    return;
                }
                "v" => {
                    if let Some(item) = cx.read_from_clipboard()
                        && let Some(text) = item.text()
                    {
                        self.composer.push_str(&text);
                        cx.notify();
                    }
                    return;
                }
                _ => {}
            }
        }

        // Slash palette keyboard navigation when composer starts with `/`
        let matches_len = self.state.matching_slash_commands(&self.composer).len();
        if matches_len > 0 {
            match key {
                "up" => {
                    self.state.selected_slash_idx =
                        (self.state.selected_slash_idx + matches_len - 1) % matches_len;
                    cx.notify();
                    return;
                }
                "down" => {
                    self.state.selected_slash_idx =
                        (self.state.selected_slash_idx + 1) % matches_len;
                    cx.notify();
                    return;
                }
                "tab" => {
                    let chosen = self
                        .state
                        .matching_slash_commands(&self.composer)
                        .get(self.state.selected_slash_idx % matches_len)
                        .map(|c| c.name.clone());
                    if let Some(name) = chosen {
                        self.composer = format!("{name} ");
                        self.state.selected_slash_idx = 0;
                        cx.notify();
                    }
                    return;
                }
                "escape" => {
                    self.composer.clear();
                    self.state.selected_slash_idx = 0;
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }

        match key {
            "enter" => {
                if mods.shift {
                    self.composer.push('\n');
                    cx.notify();
                } else {
                    self.submit_composer(cx);
                }
            }
            "backspace" => {
                self.composer.pop();
                self.state.selected_slash_idx = 0;
                cx.notify();
            }
            "escape" => {
                if self.state.is_streaming
                    && let Some(sid) = self.state.active_session_id.clone()
                {
                    let _ = self.cmd_tx.send(UiCommand::CancelPrompt(sid));
                }
            }
            _ => {
                if !primary_mod
                    && !mods.alt
                    && let Some(ch) = &event.keystroke.key_char
                    && !ch.chars().all(|c| c.is_control())
                {
                    self.composer.push_str(ch);
                    self.state.selected_slash_idx = 0;
                    cx.notify();
                }
            }
        }
    }

    fn render_titlebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let status_color = match &self.state.status {
            ConnectionStatus::Connected { .. } => theme.success,
            ConnectionStatus::Connecting => theme.warning,
            ConnectionStatus::Demo => theme.accent,
            ConnectionStatus::Disconnected(_) => theme.destructive,
        };
        let theme_label = match theme.mode {
            ThemeMode::Dark => "Light",
            ThemeMode::Light => "Dark",
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(40.0))
            .w_full()
            // Leave room on the left for macOS traffic lights (`appears_transparent: true`)
            .pl(px(80.0))
            .pr_3()
            .bg(theme.sidebar_bg)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("toggle-sidebar")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme.fg_muted)
                            .hover(|s| s.bg(theme.surface_hover).text_color(theme.fg))
                            .child(if self.state.sidebar_open {
                                "Hide Sidebar"
                            } else {
                                "Sidebar"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.sidebar_open = !this.state.sidebar_open;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.fg)
                            .child(self.state.active_title.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("status-pill")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1p5()
                            .px_2()
                            .py_0p5()
                            .rounded_full()
                            .bg(theme.surface)
                            .border_1()
                            .border_color(theme.border)
                            .text_xs()
                            .text_color(theme.fg_muted)
                            .cursor_pointer()
                            .child(div().size(px(7.0)).rounded_full().bg(status_color))
                            .child(self.state.status.label())
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.state.status.is_live()
                                    && !matches!(this.state.status, ConnectionStatus::Demo)
                                {
                                    this.state.status = ConnectionStatus::Connecting;
                                    let _ = this.cmd_tx.send(UiCommand::Reconnect);
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        div()
                            .id("toggle-theme")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme.fg_muted)
                            .hover(|s| s.bg(theme.surface_hover).text_color(theme.fg))
                            .child(theme_label)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.theme = this.theme.toggle();
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("new-chat-btn")
                            .px_2p5()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(theme.accent)
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.accent_fg)
                            .hover(|s| s.opacity(0.9))
                            .child("+ New Chat")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_new_chat(cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let active_id = self.state.active_session_id.as_deref();

        let mut list = div()
            .id("sidebar-sessions")
            .flex()
            .flex_col()
            .flex_1()
            .gap_1()
            .px_2()
            .py_2()
            .overflow_y_scroll();

        if self.state.sessions.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py_4()
                    .text_xs()
                    .text_color(theme.fg_muted)
                    .child("No sessions yet. Press Cmd+N or start typing."),
            );
        } else {
            for (idx, session) in self.state.sessions.iter().enumerate() {
                list = list.child(self.render_session_row(idx, session, active_id, cx));
            }
        }

        div()
            .flex()
            .flex_col()
            .w(px(252.0))
            .h_full()
            .flex_none()
            .bg(theme.sidebar_bg)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.fg_muted)
                    .child("SESSIONS")
                    .child(format!("{}", self.state.sessions.len())),
            )
            .child(list)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.fg_muted)
                    .child("Hermes Companion · GPUI M0"),
            )
            .into_any_element()
    }

    fn render_session_row(
        &self,
        idx: usize,
        session: &SessionSummary,
        active_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = &self.theme;
        let is_active = active_id == Some(session.session_id.as_str());
        let sid = session.session_id.clone();

        div()
            .id(SharedString::from(format!("session-row-{idx}")))
            .flex()
            .flex_col()
            .gap_0p5()
            .px_2p5()
            .py_2()
            .rounded_md()
            .cursor_pointer()
            .when(is_active, |d| d.bg(theme.accent_subtle))
            .when(!is_active, |d| d.hover(|s| s.bg(theme.surface_hover)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_session(sid.clone(), cx);
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(if is_active {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::MEDIUM
                            })
                            .text_color(theme.fg)
                            .truncate()
                            .child(session.title.clone()),
                    )
                    .when(session.is_bot_chat, |d| {
                        d.child(
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded_sm()
                                .bg(theme.surface)
                                .border_1()
                                .border_color(theme.border)
                                .text_xs()
                                .text_color(theme.accent)
                                .child("Bot"),
                        )
                    })
                    .when(session.running, |d| {
                        d.child(div().size(px(6.0)).rounded_full().bg(theme.success))
                    }),
            )
            .when(!session.preview.is_empty(), |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme.fg_muted)
                        .truncate()
                        .child(session.preview.clone()),
                )
            })
            .into_any_element()
    }

    fn render_transcript(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let mut feed = div()
            .id("transcript-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .w_full()
            .overflow_y_scroll()
            .px_6()
            .py_5();

        let mut column = div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .max_w(px(780.0))
            .mx_auto();

        if self.state.messages.is_empty() {
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .py_12()
                    .gap_2()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.fg)
                            .child("Hermes Native Companion"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.fg_muted)
                            .child("Ask anything, type / for commands, or press Cmd+N for a new session."),
                    ),
            );
        } else {
            for item in &self.state.messages {
                column = column.child(self.render_chat_item(item));
            }
        }

        // Inline approval / clarify cards scoped to the active session (per DESIGN.md).
        let active_sid = self.state.active_session_id.as_deref();
        for prompt in self.state.pending_prompts.iter().filter(|p| {
            p.session_id.is_none() || active_sid.is_none() || p.session_id.as_deref() == active_sid
        }) {
            column = column.child(self.render_pending_prompt(prompt, cx));
        }

        feed = feed.child(column);
        feed.into_any_element()
    }

    fn render_chat_item(&self, item: &ChatItem) -> AnyElement {
        let theme = &self.theme;
        match item {
            ChatItem::User { text } => div()
                .flex()
                .flex_row()
                .justify_end()
                .w_full()
                .child(
                    div()
                        .max_w(px(620.0))
                        .px_3p5()
                        .py_2p5()
                        .rounded_lg()
                        .bg(theme.user_bubble)
                        .border_1()
                        .border_color(theme.user_bubble_border)
                        .text_sm()
                        .line_height(px(21.0))
                        .text_color(theme.fg)
                        .child(text.clone()),
                )
                .into_any_element(),
            ChatItem::Assistant {
                text,
                reasoning,
                streaming,
            } => div()
                .flex()
                .flex_col()
                .gap_2()
                .w_full()
                .when(self.state.show_reasoning && !reasoning.is_empty(), |d| {
                    d.child(
                        div()
                            .border_l_2()
                            .border_color(theme.border)
                            .pl_3()
                            .py_0p5()
                            .text_xs()
                            .text_color(theme.fg_muted)
                            .child(format!("Thinking: {reasoning}")),
                    )
                })
                .child(render_markdown(text, theme))
                .when(*streaming, |d| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(theme.accent)
                            .child("● Streaming..."),
                    )
                })
                .into_any_element(),
            ChatItem::ToolCall {
                name,
                summary,
                status,
                output,
            } => {
                let (dot_color, badge) = match status {
                    ToolStatus::Running => (theme.warning, "running"),
                    ToolStatus::Completed => (theme.success, "done"),
                    ToolStatus::Failed => (theme.destructive, "failed"),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(div().size(px(7.0)).rounded_full().bg(dot_color))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .font_family("Menlo")
                                            .text_color(theme.fg)
                                            .child(name.clone()),
                                    )
                                    .when(!summary.is_empty(), |d| {
                                        d.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.fg_muted)
                                                .truncate()
                                                .child(summary.clone()),
                                        )
                                    }),
                            )
                            .child(div().text_xs().text_color(theme.fg_muted).child(badge)),
                    )
                    .when_some(output.as_ref(), |d, out| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme.fg_muted)
                                .child(out.clone()),
                        )
                    })
                    .into_any_element()
            }
            ChatItem::Notice { text, is_error } => div()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(theme.surface)
                .border_1()
                .border_color(if *is_error {
                    theme.destructive
                } else {
                    theme.border
                })
                .text_xs()
                .text_color(if *is_error {
                    theme.destructive
                } else {
                    theme.fg_muted
                })
                .child(text.clone())
                .into_any_element(),
        }
    }

    fn render_pending_prompt(
        &self,
        prompt: &PendingPrompt,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = &self.theme;
        match &prompt.kind {
            PendingPromptKind::Approval {
                command,
                description,
                choices,
            } => {
                let mut buttons = div().flex().flex_row().gap_2().mt_1();
                let options: Vec<String> = if choices.is_empty() {
                    vec!["allow_once".into(), "deny".into()]
                } else {
                    choices.clone()
                };
                for (i, choice) in options.into_iter().enumerate() {
                    let req_id = prompt.id.clone();
                    let ch_val = choice.clone();
                    let is_deny = choice.contains("deny") || choice.contains("reject");
                    let bg = if is_deny {
                        theme.surface_hover
                    } else if i == 0 {
                        theme.accent
                    } else {
                        theme.surface
                    };
                    let fg = if !is_deny && i == 0 {
                        theme.accent_fg
                    } else if is_deny {
                        theme.destructive
                    } else {
                        theme.fg
                    };
                    buttons = buttons.child(
                        div()
                            .id(SharedString::from(format!("approval-{}-{i}", prompt.id)))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(bg)
                            .border_1()
                            .border_color(theme.border)
                            .cursor_pointer()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(fg)
                            .hover(|s| s.opacity(0.9))
                            .child(choice.replace('_', " "))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.answer_prompt(req_id.clone(), ch_val.clone(), cx);
                            })),
                    );
                }

                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3p5()
                    .rounded_lg()
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.warning)
                    .shadow_md()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.warning)
                            .child("APPROVAL REQUIRED"),
                    )
                    .when(!description.is_empty(), |d| {
                        d.child(
                            div()
                                .text_sm()
                                .text_color(theme.fg)
                                .child(description.clone()),
                        )
                    })
                    .when(!command.is_empty(), |d| {
                        d.child(
                            div()
                                .px_2p5()
                                .py_1p5()
                                .rounded_md()
                                .bg(theme.code_bg)
                                .font_family("Menlo")
                                .text_xs()
                                .text_color(theme.fg)
                                .child(command.clone()),
                        )
                    })
                    .child(buttons)
                    .into_any_element()
            }
            PendingPromptKind::Clarify { question, choices } => {
                let mut buttons = div().flex().flex_row().flex_wrap().gap_2().mt_1();
                for (i, choice) in choices.iter().enumerate() {
                    let req_id = prompt.id.clone();
                    let ans = choice.clone();
                    buttons = buttons.child(
                        div()
                            .id(SharedString::from(format!("clarify-{}-{i}", prompt.id)))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(theme.surface)
                            .border_1()
                            .border_color(theme.border)
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme.fg)
                            .hover(|s| s.bg(theme.accent_subtle))
                            .child(choice.clone())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.answer_prompt(req_id.clone(), ans.clone(), cx);
                            })),
                    );
                }
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3p5()
                    .rounded_lg()
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.accent)
                    .shadow_md()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.accent)
                            .child("CLARIFICATION"),
                    )
                    .child(div().text_sm().text_color(theme.fg).child(question.clone()))
                    .child(buttons)
                    .into_any_element()
            }
        }
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let slash_matches = self.state.matching_slash_commands(&self.composer);

        let mut wrapper = div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .max_w(px(780.0))
            .mx_auto()
            .px_6()
            .pb_4()
            .pt_2();

        // Floating slash command palette above composer
        if !slash_matches.is_empty() {
            let mut palette = div()
                .flex()
                .flex_col()
                .rounded_lg()
                .bg(theme.popover)
                .border_1()
                .border_color(theme.border)
                .shadow_md()
                .p_1();

            let sel = self.state.selected_slash_idx % slash_matches.len();
            for (i, item) in slash_matches.iter().enumerate() {
                let cmd_name = item.name.clone();
                let is_sel = i == sel;
                palette = palette.child(
                    div()
                        .id(SharedString::from(format!("slash-item-{i}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .px_2p5()
                        .py_1p5()
                        .rounded_md()
                        .cursor_pointer()
                        .when(is_sel, |d| d.bg(theme.accent_subtle))
                        .when(!is_sel, |d| d.hover(|s| s.bg(theme.surface_hover)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.composer = format!("{cmd_name} ");
                            this.state.selected_slash_idx = 0;
                            cx.notify();
                        }))
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
                                        .font_family("Menlo")
                                        .text_color(theme.accent)
                                        .child(item.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.fg_muted)
                                        .child(item.description.clone()),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.fg_muted)
                                .child(item.category.clone()),
                        ),
                );
            }
            wrapper = wrapper.child(palette);
        }

        let display_text = if self.composer.is_empty() {
            None
        } else {
            Some(format!("{}▎", self.composer))
        };

        let send_btn = if self.state.is_streaming {
            div()
                .id("stop-btn")
                .px_3()
                .py_1p5()
                .rounded_md()
                .bg(theme.destructive)
                .cursor_pointer()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(gpui::white())
                .child("Stop")
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(sid) = this.state.active_session_id.clone() {
                        let _ = this.cmd_tx.send(UiCommand::CancelPrompt(sid));
                        this.state.is_streaming = false;
                        cx.notify();
                    }
                }))
        } else {
            div()
                .id("send-btn")
                .px_3()
                .py_1p5()
                .rounded_md()
                .bg(theme.accent)
                .cursor_pointer()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.accent_fg)
                .hover(|s| s.opacity(0.9))
                .child("Send ↵")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.submit_composer(cx);
                }))
        };

        wrapper
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap_2()
                    .px_3p5()
                    .py_2p5()
                    .rounded_lg()
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(22.0))
                            .text_sm()
                            .line_height(px(20.0))
                            .text_color(if display_text.is_some() {
                                theme.fg
                            } else {
                                theme.fg_muted
                            })
                            .child(
                                display_text.unwrap_or_else(|| {
                                    "Message Hermes… (/ for commands · Shift+Enter newline · Cmd+B sidebar)".to_string()
                                }),
                            ),
                    )
                    .child(send_btn),
            )
            .into_any_element()
    }
}

impl Focusable for CompanionView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CompanionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle);
        }

        let theme = &self.theme;
        div()
            .id("hermes-companion-root")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg)
            .text_color(theme.fg)
            .child(self.render_titlebar(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .when(self.state.sidebar_open, |d| d.child(self.render_sidebar(cx)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_transcript(cx))
                            .child(self.render_composer(cx)),
                    ),
            )
    }
}
