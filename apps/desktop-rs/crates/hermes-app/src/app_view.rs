//! Root GPUI view for `hermes-companion` (Milestone 1 — Daily-Driver Native Companion).
//!
//! Layout follows `apps/desktop/DESIGN.md`:
//! - Integrated macOS titlebar with traffic-light clearance, sidebar toggle, session title,
//!   connection status pill, Quick Entry trigger (`Cmd+Shift+K`), Command Palette (`Cmd+K`),
//!   theme toggle, and New Chat (`Cmd+N`).
//! - Collapsible session sidebar with canonical **Bot Chat** door, live session search (`Cmd+F`),
//!   and multi-profile switcher footer.
//! - Flat scrollable transcript with `syntect`-highlighted code blocks + Copy button,
//!   subagent monitor bar, tool cards, and session-scoped inline Approval/Clarify cards.
//! - Full `TextBuffer` composer (cursor, selection, word jump, clipboard, prompt history)
//!   with `/` slash command popup + `Cmd+K` Command Palette overlay.

use gpui::{
    AnyElement, Bounds, ClipboardItem, Context, FocusHandle, Focusable, FontWeight, HighlightStyle,
    KeyDownEvent, SharedString, StyledText, TitlebarOptions, Window, WindowBounds, WindowKind,
    WindowOptions, div, prelude::*, px, size,
};
use tokio::sync::mpsc;

use crate::composer::TextBuffer;
use crate::markdown::render_markdown;
use crate::quick_entry::QuickEntryView;
use crate::state::{
    ChatItem, CompanionState, ConnectionStatus, PaletteAction, PendingPrompt, PendingPromptKind,
    SessionSummary, SlashCommandItem, ToolStatus,
};
use crate::theme::{HermesTheme, ThemeMode};

/// Actions emitted from the GPUI view (or Quick Entry window) to the Tokio gateway worker.
#[derive(Debug, Clone)]
pub enum UiCommand {
    Reconnect,
    NewSession,
    OpenBotChat,
    SelectSession(String),
    SwitchProfile(String),
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
    SyncReplayCursor {
        session_id: Option<String>,
        last_seq: u64,
        replay_epoch: Option<String>,
    },
}

/// Messages pushed from the background Tokio gateway worker into the GPUI view.
#[derive(Debug)]
pub enum WorkerEvent {
    Status(ConnectionStatus),
    Skin(hermes_protocol::SkinPayload),
    Profiles(hermes_protocol::CanonicalProfilesResult),
    SessionList(hermes_protocol::CanonicalListResult),
    Snapshot(hermes_protocol::CanonicalSnapshot),
    Subagents(hermes_protocol::SubagentListResult),
    GatewayEvent(hermes_protocol::GatewayEventEnvelope),
    SlashCatalog(Vec<SlashCommandItem>),
    ServerPrompt(PendingPrompt),
    Notice { text: String, is_error: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveInput {
    Composer,
    SessionSearch,
    CommandPalette,
}

pub struct CompanionView {
    state: CompanionState,
    theme: HermesTheme,
    composer: TextBuffer,
    session_search: TextBuffer,
    palette_query: TextBuffer,
    palette_selected_idx: usize,
    active_input: ActiveInput,
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

        // Zero-polling async bridge: tokio::sync::mpsc wakes GPUI directly on arrival.
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
            composer: TextBuffer::default(),
            session_search: TextBuffer::default(),
            palette_query: TextBuffer::default(),
            palette_selected_idx: 0,
            active_input: ActiveInput::Composer,
            focus_handle,
            cmd_tx,
        }
    }

    fn handle_worker_event(&mut self, ev: WorkerEvent) {
        match ev {
            WorkerEvent::Status(status) => {
                if let ConnectionStatus::Connected { ref profile, .. } = status {
                    self.state.active_profile = profile.clone();
                }
                self.state.status = status;
            }
            WorkerEvent::Skin(skin) => {
                self.theme.apply_skin(&skin);
            }
            WorkerEvent::Profiles(profiles) => {
                self.state.load_profiles(profiles);
            }
            WorkerEvent::SessionList(list) => {
                self.state.load_session_list(list);
            }
            WorkerEvent::Snapshot(snap) => {
                self.state.load_snapshot(snap);
                self.sync_replay_cursor();
            }
            WorkerEvent::Subagents(subagents) => {
                self.state.load_subagents(subagents);
            }
            WorkerEvent::GatewayEvent(env) => {
                self.state.apply_event(env);
                self.sync_replay_cursor();
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

    fn sync_replay_cursor(&self) {
        let _ = self.cmd_tx.send(UiCommand::SyncReplayCursor {
            session_id: self.state.active_session_id.clone(),
            last_seq: self.state.last_seq,
            replay_epoch: self.state.replay_epoch.clone(),
        });
    }

    fn open_quick_entry(&self, cx: &mut Context<Self>) {
        let bounds = Bounds::centered(None, size(px(620.0), px(124.0)), cx);
        let theme = self.theme;
        let target_sid = self.state.active_session_id.clone();
        let target_title = self.state.active_title.clone();
        let cmd_tx = self.cmd_tx.clone();

        let _ = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                kind: WindowKind::PopUp,
                titlebar: Some(TitlebarOptions {
                    title: Some("Hermes Quick Entry".into()),
                    appears_transparent: true,
                    traffic_light_position: None,
                }),
                focus: true,
                show: true,
                ..Default::default()
            },
            move |_, cx| {
                cx.new(|cx| {
                    QuickEntryView::new(theme, target_sid.clone(), target_title.clone(), cmd_tx.clone(), cx)
                })
            },
        );
    }

    fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.composer.commit_submission() else {
            return;
        };
        self.state.selected_slash_idx = 0;

        if text == "/clear" {
            self.state.messages.clear();
            cx.notify();
            return;
        }
        if text == "/new" {
            self.start_new_chat(cx);
            return;
        }

        if matches!(self.state.status, ConnectionStatus::Demo) {
            self.state.messages.push(ChatItem::User { text: text.clone() });
            let reply = if text.starts_with('/') {
                format!("Executed `{text}` in offline demo mode.\n\nRun without `--demo` to attach to `hermes gateway attach --json`.")
            } else {
                format!(
                    "Echoing from **offline demo mode** (profile `{}`):\n\n> {text}\n\n```rust\nprintln!(\"hello from GPUI Milestone 1\");\n```",
                    self.state.active_profile
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

        self.state.messages.push(ChatItem::User { text: text.clone() });
        if text.starts_with('/') {
            let _ = self.cmd_tx.send(UiCommand::RunSlash {
                session_id: self.state.active_session_id.clone(),
                command: text,
            });
        } else {
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
        self.state.subagents.clear();
        self.state.is_streaming = false;
        self.state.last_seq = 0;
        self.state.replay_epoch = None;
        self.active_input = ActiveInput::Composer;
        self.sync_replay_cursor();
        if !matches!(self.state.status, ConnectionStatus::Demo) {
            let _ = self.cmd_tx.send(UiCommand::NewSession);
        }
        cx.notify();
    }

    fn open_bot_chat(&mut self, cx: &mut Context<Self>) {
        self.active_input = ActiveInput::Composer;
        if matches!(self.state.status, ConnectionStatus::Demo) {
            self.select_session("demo-bot".to_string(), cx);
            return;
        }
        self.state.active_title = "Bot Chat".to_string();
        self.state.messages.clear();
        let _ = self.cmd_tx.send(UiCommand::OpenBotChat);
        cx.notify();
    }

    fn select_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        self.active_input = ActiveInput::Composer;
        if self.state.active_session_id.as_deref() == Some(session_id.as_str()) {
            cx.notify();
            return;
        }
        if let Some(s) = self.state.sessions.iter().find(|s| s.session_id == session_id) {
            self.state.active_title = s.title.clone();
        }
        self.state.active_session_id = Some(session_id.clone());
        if !matches!(self.state.status, ConnectionStatus::Demo) {
            self.state.messages.clear();
            self.state.subagents.clear();
            let _ = self.cmd_tx.send(UiCommand::SelectSession(session_id));
        }
        cx.notify();
    }

    fn switch_profile(&mut self, profile: String, cx: &mut Context<Self>) {
        self.state.profile_menu_open = false;
        self.active_input = ActiveInput::Composer;
        if self.state.active_profile == profile {
            cx.notify();
            return;
        }
        if matches!(self.state.status, ConnectionStatus::Demo) {
            self.state.active_profile = profile.clone();
            self.state.messages.push(ChatItem::Notice {
                text: format!("Switched demo profile to `{profile}`."),
                is_error: false,
            });
            cx.notify();
            return;
        }
        self.state.reset_for_profile_switch(profile.clone());
        self.state.status = ConnectionStatus::Connecting;
        let _ = self.cmd_tx.send(UiCommand::SwitchProfile(profile));
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

    fn execute_palette_action(&mut self, action: PaletteAction, cx: &mut Context<Self>) {
        self.active_input = ActiveInput::Composer;
        self.palette_query.clear();
        self.palette_selected_idx = 0;

        match action {
            PaletteAction::NewChat => self.start_new_chat(cx),
            PaletteAction::OpenBotChat => self.open_bot_chat(cx),
            PaletteAction::OpenQuickEntry => {
                self.open_quick_entry(cx);
                cx.notify();
            }
            PaletteAction::ToggleTheme => {
                self.theme = self.theme.toggle();
                cx.notify();
            }
            PaletteAction::ToggleSidebar => {
                self.state.sidebar_open = !self.state.sidebar_open;
                cx.notify();
            }
            PaletteAction::ToggleReasoning => {
                self.state.show_reasoning = !self.state.show_reasoning;
                cx.notify();
            }
            PaletteAction::SelectSession(sid) => self.select_session(sid, cx),
            PaletteAction::SwitchProfile(profile) => self.switch_profile(profile, cx),
            PaletteAction::InsertSlash(cmd) => {
                self.composer.set_text(format!("{cmd} "));
                cx.notify();
            }
        }
    }

    fn active_buffer_mut(&mut self) -> &mut TextBuffer {
        match self.active_input {
            ActiveInput::Composer => &mut self.composer,
            ActiveInput::SessionSearch => &mut self.session_search,
            ActiveInput::CommandPalette => &mut self.palette_query,
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;
        let primary_mod = mods.platform || mods.control;

        // Global shortcuts (Cmd on macOS, Ctrl elsewhere)
        if primary_mod {
            match key {
                "k" if mods.shift => {
                    self.open_quick_entry(cx);
                    return;
                }
                "k" => {
                    if self.active_input == ActiveInput::CommandPalette {
                        self.active_input = ActiveInput::Composer;
                        self.palette_query.clear();
                    } else {
                        self.active_input = ActiveInput::CommandPalette;
                        self.palette_query.clear();
                        self.palette_selected_idx = 0;
                    }
                    cx.notify();
                    return;
                }
                "f" => {
                    self.state.sidebar_open = true;
                    self.active_input = if self.active_input == ActiveInput::SessionSearch {
                        ActiveInput::Composer
                    } else {
                        ActiveInput::SessionSearch
                    };
                    cx.notify();
                    return;
                }
                "n" => {
                    self.start_new_chat(cx);
                    return;
                }
                "b" => {
                    self.state.sidebar_open = !self.state.sidebar_open;
                    cx.notify();
                    return;
                }
                "a" => {
                    self.active_buffer_mut().select_all();
                    cx.notify();
                    return;
                }
                "c" => {
                    if let Some(sel) = self.active_buffer_mut().selected_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(sel.to_string()));
                    }
                    return;
                }
                "x" => {
                    if let Some(cut) = self.active_buffer_mut().cut() {
                        cx.write_to_clipboard(ClipboardItem::new_string(cut));
                        cx.notify();
                    }
                    return;
                }
                "v" => {
                    if let Some(item) = cx.read_from_clipboard()
                        && let Some(text) = item.text()
                    {
                        self.active_buffer_mut().insert(&text);
                        cx.notify();
                    }
                    return;
                }
                "left" => {
                    self.active_buffer_mut().move_home(mods.shift);
                    cx.notify();
                    return;
                }
                "right" => {
                    self.active_buffer_mut().move_end(mods.shift);
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }

        // Command Palette modal navigation
        if self.active_input == ActiveInput::CommandPalette {
            let items = self.state.palette_items(self.palette_query.as_str());
            match key {
                "escape" => {
                    self.active_input = ActiveInput::Composer;
                    self.palette_query.clear();
                    cx.notify();
                    return;
                }
                "up" if !items.is_empty() => {
                    self.palette_selected_idx =
                        (self.palette_selected_idx + items.len() - 1) % items.len();
                    cx.notify();
                    return;
                }
                "down" if !items.is_empty() => {
                    self.palette_selected_idx = (self.palette_selected_idx + 1) % items.len();
                    cx.notify();
                    return;
                }
                "enter" => {
                    if let Some(item) = items.get(self.palette_selected_idx % items.len().max(1)) {
                        self.execute_palette_action(item.action.clone(), cx);
                    }
                    return;
                }
                _ => {}
            }
        }

        // Session search exit keys
        if self.active_input == ActiveInput::SessionSearch {
            match key {
                "escape" => {
                    self.session_search.clear();
                    self.active_input = ActiveInput::Composer;
                    cx.notify();
                    return;
                }
                "enter" => {
                    let first = self
                        .state
                        .filtered_sessions(self.session_search.as_str())
                        .first()
                        .map(|s| s.session_id.clone());
                    if let Some(sid) = first {
                        self.select_session(sid, cx);
                    } else {
                        self.active_input = ActiveInput::Composer;
                        cx.notify();
                    }
                    return;
                }
                _ => {}
            }
        }

        // Composer slash palette & prompt history navigation
        if self.active_input == ActiveInput::Composer {
            let matches_len = self.state.matching_slash_commands(self.composer.as_str()).len();
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
                            .matching_slash_commands(self.composer.as_str())
                            .get(self.state.selected_slash_idx % matches_len)
                            .map(|c| c.name.clone());
                        if let Some(name) = chosen {
                            self.composer.set_text(format!("{name} "));
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
            } else {
                match key {
                    "up" if self.composer.history_prev() => {
                        cx.notify();
                        return;
                    }
                    "down" if self.composer.history_next() => {
                        cx.notify();
                        return;
                    }
                    "enter" => {
                        if mods.shift {
                            self.composer.insert("\n");
                            cx.notify();
                        } else {
                            self.submit_composer(cx);
                        }
                        return;
                    }
                    "escape" => {
                        if self.state.profile_menu_open {
                            self.state.profile_menu_open = false;
                            cx.notify();
                        } else if self.state.is_streaming
                            && let Some(sid) = self.state.active_session_id.clone()
                        {
                            let _ = self.cmd_tx.send(UiCommand::CancelPrompt(sid));
                        }
                        return;
                    }
                    _ => {}
                }
            }
        }

        // Standard editing keys routed to the active TextBuffer
        match key {
            "backspace" => {
                if mods.alt {
                    self.active_buffer_mut().delete_word_backward();
                } else {
                    self.active_buffer_mut().backspace();
                }
                self.palette_selected_idx = 0;
                self.state.selected_slash_idx = 0;
                cx.notify();
            }
            "delete" => {
                self.active_buffer_mut().delete_forward();
                cx.notify();
            }
            "left" => {
                if mods.alt {
                    self.active_buffer_mut().move_word_left(mods.shift);
                } else {
                    self.active_buffer_mut().move_left(mods.shift);
                }
                cx.notify();
            }
            "right" => {
                if mods.alt {
                    self.active_buffer_mut().move_word_right(mods.shift);
                } else {
                    self.active_buffer_mut().move_right(mods.shift);
                }
                cx.notify();
            }
            "home" => {
                self.active_buffer_mut().move_home(mods.shift);
                cx.notify();
            }
            "end" => {
                self.active_buffer_mut().move_end(mods.shift);
                cx.notify();
            }
            _ => {
                if !primary_mod
                    && !mods.alt
                    && let Some(ch) = &event.keystroke.key_char
                    && !ch.chars().all(|c| c.is_control())
                {
                    self.active_buffer_mut().insert(ch);
                    self.palette_selected_idx = 0;
                    self.state.selected_slash_idx = 0;
                    cx.notify();
                }
            }
        }
    }

    fn render_buffer_text(&self, buf: &TextBuffer, placeholder: &str, focused: bool) -> AnyElement {
        let theme = &self.theme;
        if buf.is_empty() {
            let text = if focused {
                format!("▎{placeholder}")
            } else {
                placeholder.to_string()
            };
            return div()
                .text_color(theme.fg_muted)
                .child(text)
                .into_any_element();
        }
        if !focused {
            return div()
                .text_color(theme.fg)
                .child(buf.as_str().to_string())
                .into_any_element();
        }
        let (rendered, sel) = buf.render_parts();
        let styled = if let Some(range) = sel {
            StyledText::new(rendered).with_highlights([(
                range,
                HighlightStyle {
                    background_color: Some(theme.accent_subtle),
                    ..Default::default()
                },
            )])
        } else {
            StyledText::new(rendered)
        };
        div().text_color(theme.fg).child(styled).into_any_element()
    }

    fn render_titlebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let status_color = match &self.state.status {
            ConnectionStatus::Connected { .. } => theme.success,
            ConnectionStatus::Connecting | ConnectionStatus::Reconnecting { .. } => theme.warning,
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
                            .child(if self.state.sidebar_open { "Hide" } else { "Sidebar" })
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
                    .gap_1p5()
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
                            .id("open-palette-btn")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme.fg_muted)
                            .hover(|s| s.bg(theme.surface_hover).text_color(theme.fg))
                            .child("⌘K")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.active_input = ActiveInput::CommandPalette;
                                this.palette_query.clear();
                                this.palette_selected_idx = 0;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("quick-entry-btn")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme.fg_muted)
                            .hover(|s| s.bg(theme.surface_hover).text_color(theme.fg))
                            .child("Quick Entry")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_quick_entry(cx);
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
                            .child("+ New")
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
        let filtered = self.state.filtered_sessions(self.session_search.as_str());
        let search_focused = self.active_input == ActiveInput::SessionSearch;

        let mut list = div()
            .id("sidebar-sessions")
            .flex()
            .flex_col()
            .flex_1()
            .gap_1()
            .px_2()
            .py_1p5()
            .overflow_y_scroll();

        if filtered.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py_4()
                    .text_xs()
                    .text_color(theme.fg_muted)
                    .child(if self.session_search.is_empty() {
                        "No sessions yet. Press Cmd+N or start typing."
                    } else {
                        "No matching sessions."
                    }),
            );
        } else {
            for (idx, session) in filtered.iter().enumerate() {
                list = list.child(self.render_session_row(idx, session, active_id, cx));
            }
        }

        // Profile switcher popup menu (opens upward above footer)
        let mut footer = div()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.border)
            .p_2()
            .gap_1();

        if self.state.profile_menu_open {
            let mut menu = div()
                .flex()
                .flex_col()
                .gap_0p5()
                .p_1()
                .rounded_md()
                .bg(theme.popover)
                .border_1()
                .border_color(theme.border)
                .shadow_md();
            for (i, prof) in self.state.profiles.iter().enumerate() {
                let pname = prof.name.clone();
                let is_cur = prof.name == self.state.active_profile;
                menu = menu.child(
                    div()
                        .id(SharedString::from(format!("profile-opt-{i}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(is_cur, |d| d.bg(theme.accent_subtle))
                        .when(!is_cur, |d| d.hover(|s| s.bg(theme.surface_hover)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.switch_profile(pname.clone(), cx);
                        }))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(if is_cur {
                                    FontWeight::SEMIBOLD
                                } else {
                                    FontWeight::NORMAL
                                })
                                .text_color(theme.fg)
                                .child(prof.display_name.clone()),
                        )
                        .when_some(prof.model.as_ref(), |d, m| {
                            d.child(div().text_xs().text_color(theme.fg_muted).child(m.clone()))
                        }),
                );
            }
            footer = footer.child(menu);
        }

        footer = footer.child(
            div()
                .id("profile-switcher-btn")
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px_2()
                .py_1p5()
                .rounded_md()
                .bg(theme.surface)
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .hover(|s| s.bg(theme.surface_hover))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.state.profile_menu_open = !this.state.profile_menu_open;
                    cx.notify();
                }))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .child(div().text_xs().text_color(theme.fg_muted).child("Profile:"))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.fg)
                                .child(self.state.active_profile.clone()),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.fg_muted)
                        .child(if self.state.profile_menu_open { "▾" } else { "▴" }),
                ),
        );

        div()
            .flex()
            .flex_col()
            .w(px(260.0))
            .h_full()
            .flex_none()
            .bg(theme.sidebar_bg)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .child(
                        // Canonical Bot Chat door (resolved by NAME, never pinned)
                        div()
                            .id("open-bot-chat-btn")
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .px_2p5()
                            .py_1p5()
                            .rounded_md()
                            .bg(theme.surface)
                            .border_1()
                            .border_color(theme.border)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.surface_hover))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_bot_chat(cx);
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.fg)
                                    .child("✦ Bot Chat"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.accent)
                                    .child("Canonical"),
                            ),
                    )
                    .child(
                        // Live session search box (`Cmd+F`)
                        div()
                            .id("session-search-box")
                            .px_2p5()
                            .py_1()
                            .rounded_md()
                            .bg(theme.surface)
                            .border_1()
                            .border_color(if search_focused {
                                theme.accent
                            } else {
                                theme.border
                            })
                            .text_xs()
                            .cursor_text()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.active_input = ActiveInput::SessionSearch;
                                cx.notify();
                            }))
                            .child(self.render_buffer_text(
                                &self.session_search,
                                "Search sessions… (⌘F)",
                                search_focused,
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .pt_1p5()
                    .pb_0p5()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.fg_muted)
                    .child("SESSIONS")
                    .child(format!("{}", filtered.len())),
            )
            .child(list)
            .child(footer)
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
                            .child("Ask anything · / for commands · ⌘K palette · ⌘⇧K Quick Entry"),
                    ),
            );
        } else {
            for item in &self.state.messages {
                column = column.child(self.render_chat_item(item));
            }
        }

        // Inline approval / clarify cards scoped to active session.
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
                    vec!["once".into(), "deny".into()]
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

    fn render_subagent_strip(&self) -> Option<AnyElement> {
        if self.state.subagents.is_empty() {
            return None;
        }
        let theme = &self.theme;
        let mut strip = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_1p5()
            .w_full();

        strip = strip.child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.fg_muted)
                .child("Subagents:"),
        );

        for sub in &self.state.subagents {
            let dot = if sub.running {
                theme.warning
            } else {
                theme.success
            };
            strip = strip.child(
                div()
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
                    .child(div().size(px(6.0)).rounded_full().bg(dot))
                    .child(div().text_color(theme.fg).child(sub.goal.clone()))
                    .child(
                        div()
                            .text_color(theme.fg_muted)
                            .child(format!("({})", sub.status_label)),
                    ),
            );
        }
        Some(strip.into_any_element())
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let composer_focused = self.active_input == ActiveInput::Composer;
        let slash_matches = if composer_focused {
            self.state.matching_slash_commands(self.composer.as_str())
        } else {
            Vec::new()
        };

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

        if let Some(strip) = self.render_subagent_strip() {
            wrapper = wrapper.child(strip);
        }

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
                            this.composer.set_text(format!("{cmd_name} "));
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
                    .id("composer-box")
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap_2()
                    .px_3p5()
                    .py_2p5()
                    .rounded_lg()
                    .bg(theme.surface)
                    .border_1()
                    .border_color(if composer_focused {
                        theme.accent
                    } else {
                        theme.border
                    })
                    .cursor_text()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.active_input = ActiveInput::Composer;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(22.0))
                            .text_sm()
                            .line_height(px(20.0))
                            .child(self.render_buffer_text(
                                &self.composer,
                                "Message Hermes… (/ commands · ⌘K palette · ↑/↓ history)",
                                composer_focused,
                            )),
                    )
                    .child(send_btn),
            )
            .into_any_element()
    }

    fn render_command_palette(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let items = self.state.palette_items(self.palette_query.as_str());
        let sel = if items.is_empty() {
            0
        } else {
            self.palette_selected_idx % items.len()
        };

        let mut list = div()
            .id("command-palette-list")
            .flex()
            .flex_col()
            .gap_0p5()
            .max_h(px(340.0))
            .overflow_y_scroll()
            .p_1p5();

        if items.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_4()
                    .text_xs()
                    .text_color(theme.fg_muted)
                    .child("No matching commands, sessions, or profiles."),
            );
        } else {
            for (i, item) in items.into_iter().enumerate() {
                let action = item.action.clone();
                let is_sel = i == sel;
                list = list.child(
                    div()
                        .id(SharedString::from(format!("palette-row-{i}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .cursor_pointer()
                        .when(is_sel, |d| d.bg(theme.accent_subtle))
                        .when(!is_sel, |d| d.hover(|s| s.bg(theme.surface_hover)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.execute_palette_action(action.clone(), cx);
                        }))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme.fg)
                                        .child(item.title),
                                )
                                .when(!item.subtitle.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.fg_muted)
                                            .child(item.subtitle),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded_sm()
                                .bg(theme.surface)
                                .border_1()
                                .border_color(theme.border)
                                .text_xs()
                                .text_color(theme.fg_muted)
                                .child(item.badge),
                        ),
                );
            }
        }

        div()
            .id("command-palette-scrim")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(84.0))
            .bg(gpui::rgba(0x010409aa))
            .on_click(cx.listener(|this, _, _, cx| {
                this.active_input = ActiveInput::Composer;
                this.palette_query.clear();
                cx.notify();
            }))
            .child(
                div()
                    .id("command-palette-card")
                    .w(px(560.0))
                    .rounded_lg()
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .overflow_hidden()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .px_3p5()
                            .py_3()
                            .border_b_1()
                            .border_color(theme.border)
                            .text_sm()
                            .child(self.render_buffer_text(
                                &self.palette_query,
                                "Type a command, session, or profile… (Esc to close)",
                                true,
                            )),
                    )
                    .child(list),
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
            .relative()
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
            .when(self.active_input == ActiveInput::CommandPalette, |d| {
                d.child(self.render_command_palette(cx))
            })
    }
}
