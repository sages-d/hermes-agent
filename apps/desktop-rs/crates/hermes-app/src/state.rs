//! UI-agnostic state & event reducer for the Hermes companion app (Milestone 1).
//!
//! Kept free of GPUI types so transcript accumulation, streaming delta handling,
//! replay epoch tracking, Bot Mode labeling, subagents, session filtering,
//! and Command Palette search are unit-testable headlessly.

use hermes_protocol::{
    CanonicalListResult, CanonicalProfilesResult, CanonicalSnapshot, GatewayEventEnvelope,
    GatewayNotification, SubagentListResult, SubagentStatus, transcript_content_text,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting,
    Reconnecting {
        attempt: u32,
        delay_secs: u64,
    },
    Connected {
        profile: String,
        instance_id: String,
        protocol: String,
    },
    Demo,
    Disconnected(String),
}

impl ConnectionStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Connecting => "Connecting...".to_string(),
            Self::Reconnecting {
                attempt,
                delay_secs,
            } => format!("Reconnecting (#{attempt}, {delay_secs}s)..."),
            Self::Connected { profile, .. } => format!("Connected ({profile})"),
            Self::Demo => "Demo Mode (offline)".to_string(),
            Self::Disconnected(reason) => format!("Disconnected: {reason}"),
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileInfo {
    pub name: String,
    pub display_name: String,
    pub model: Option<String>,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    pub is_bot_chat: bool,
    pub running: bool,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSummary {
    pub id: String,
    pub goal: String,
    pub running: bool,
    pub status_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatItem {
    User {
        text: String,
    },
    Assistant {
        text: String,
        reasoning: String,
        streaming: bool,
    },
    ToolCall {
        name: String,
        summary: String,
        status: ToolStatus,
        output: Option<String>,
    },
    Notice {
        text: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone)]
pub enum PendingPromptKind {
    Approval {
        command: String,
        description: String,
        choices: Vec<String>,
    },
    Clarify {
        question: String,
        choices: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub struct PendingPrompt {
    pub id: String,
    pub session_id: Option<String>,
    pub kind: PendingPromptKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandItem {
    pub name: String,
    pub description: String,
    pub category: String,
}

/// Actions selectable from the `Cmd+K` Command Palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteAction {
    NewChat,
    OpenBotChat,
    OpenQuickEntry,
    OpenTerminal,
    OpenGitReview,
    OpenFiles,
    OpenSettings,
    ToggleTheme,
    ToggleSidebar,
    ToggleReasoning,
    SelectSession(String),
    SwitchProfile(String),
    InsertSlash(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteItem {
    pub title: String,
    pub subtitle: String,
    pub badge: String,
    pub action: PaletteAction,
}

/// Complete Milestone 1 companion workspace state.
#[derive(Debug, Clone)]
pub struct CompanionState {
    pub status: ConnectionStatus,
    pub sidebar_open: bool,
    pub profiles: Vec<ProfileInfo>,
    pub active_profile: String,
    pub profile_menu_open: bool,
    pub sessions: Vec<SessionSummary>,
    pub active_session_id: Option<String>,
    pub active_title: String,
    pub last_seq: u64,
    pub replay_epoch: Option<String>,
    pub messages: Vec<ChatItem>,
    pub subagents: Vec<SubagentSummary>,
    pub is_streaming: bool,
    pub pending_prompts: Vec<PendingPrompt>,
    pub slash_catalog: Vec<SlashCommandItem>,
    pub selected_slash_idx: usize,
    pub show_reasoning: bool,
}

impl Default for CompanionState {
    fn default() -> Self {
        Self {
            status: ConnectionStatus::Connecting,
            sidebar_open: true,
            profiles: vec![ProfileInfo {
                name: "default".into(),
                display_name: "default".into(),
                model: None,
                is_default: true,
            }],
            active_profile: "default".into(),
            profile_menu_open: false,
            sessions: Vec::new(),
            active_session_id: None,
            active_title: "New Chat".to_string(),
            last_seq: 0,
            replay_epoch: None,
            messages: Vec::new(),
            subagents: Vec::new(),
            is_streaming: false,
            pending_prompts: Vec::new(),
            slash_catalog: default_slash_catalog(),
            selected_slash_idx: 0,
            show_reasoning: true,
        }
    }
}

impl CompanionState {
    /// Populate an interactive offline demo workspace (`--demo`) showcasing
    /// Milestone 1 features (syntax highlighting, profiles, Bot Chat, subagents, approvals).
    pub fn demo() -> Self {
        let mut state = Self::default();
        state.status = ConnectionStatus::Demo;
        state.profiles = vec![
            ProfileInfo {
                name: "default".into(),
                display_name: "default".into(),
                model: Some("claude-sonnet-4".into()),
                is_default: true,
            },
            ProfileInfo {
                name: "work".into(),
                display_name: "work".into(),
                model: Some("gpt-5".into()),
                is_default: false,
            },
            ProfileInfo {
                name: "research".into(),
                display_name: "research".into(),
                model: Some("hermes-4".into()),
                is_default: false,
            },
        ];
        state.sessions = vec![
            SessionSummary {
                session_id: "demo-1".to_string(),
                title: "GPUI Companion Milestone 1".to_string(),
                is_bot_chat: false,
                running: false,
                preview: "Syntax highlighting, Cmd+K palette, Quick Entry, auto-reconnect".to_string(),
            },
            SessionSummary {
                session_id: "demo-bot".to_string(),
                title: "Bot Chat".to_string(),
                is_bot_chat: true,
                running: false,
                preview: "Canonical forever-chat resolved by name".to_string(),
            },
            SessionSummary {
                session_id: "demo-2".to_string(),
                title: "Benchmarking Rust vs Electron".to_string(),
                is_bot_chat: false,
                running: false,
                preview: "45 MB RSS, <100 ms cold launch".to_string(),
            },
        ];
        state.active_session_id = Some("demo-1".to_string());
        state.active_title = "GPUI Companion Milestone 1".to_string();
        state.subagents = vec![SubagentSummary {
            id: "sub-1".into(),
            goal: "Verify OpenRPC schema parity".into(),
            running: false,
            status_label: "completed".into(),
        }];
        state.messages = vec![
            ChatItem::User {
                text: "What's new in Milestone 1 of the Rust GPUI companion?".to_string(),
            },
            ChatItem::ToolCall {
                name: "read_file".to_string(),
                summary: "apps/desktop-rs/crates/hermes-gateway/src/reconnect.rs".to_string(),
                status: ToolStatus::Completed,
                output: Some("Loaded exponential backoff (1s..30s) + session.events.since catch-up.".to_string()),
            },
            ChatItem::Assistant {
                reasoning: "Summarizing Milestone 1 daily-driver upgrades across gateway, editor, and GPUI.".to_string(),
                text: "### Milestone 1 — Daily-Driver Native Companion\n\n\
                       - **Full Composer Editing:** cursor movement, Shift-selection, `Alt+Left/Right` word jump, `Cmd+A/C/X/V`, and `↑`/`↓` prompt history.\n\
                       - **Pure-Rust Syntax Highlighting:** fenced blocks highlight via `syntect` (`default-fancy`) with one-click **Copy**.\n\
                       - **Auto-Reconnect & Replay:** exponential backoff (`1s → 30s`) with `session.events.since` gap replay and automatic `session.resume` fallback.\n\
                       - **Multi-Profile + Canonical Bot Chat:** switch profiles from the sidebar footer or click **Bot Chat** (always resolved by name).\n\
                       - **`Cmd+K` Command Palette & `Cmd+Shift+K` Quick Entry:** jump to any session, profile, slash command, or floating mini-composer.\n\n\
                       ```rust\n\
                       // Auto-reconnect + gap replay in hermes-gateway\n\
                       match client.replay_or_resume(&sid, last_seq, replay_epoch).await? {\n\
                           ReplayOutcome::Events { events, .. } => apply_all(events),\n\
                           ReplayOutcome::Snapshot(snap) => hydrate_snapshot(*snap),\n\
                       }\n\
                       ```"
                    .to_string(),
                streaming: false,
            },
        ];
        state.pending_prompts.push(PendingPrompt {
            id: "demo-srq-1".to_string(),
            session_id: Some("demo-1".to_string()),
            kind: PendingPromptKind::Approval {
                command: "cargo test --workspace".to_string(),
                description: "Demo approval card — click once, session, or deny".to_string(),
                choices: vec!["once".into(), "session".into(), "deny".into()],
            },
        });
        state
    }

    /// Clear profile-scoped view state during a soft profile switch (re-home).
    pub fn reset_for_profile_switch(&mut self, next_profile: String) {
        self.active_profile = next_profile;
        self.profile_menu_open = false;
        self.sessions.clear();
        self.active_session_id = None;
        self.active_title = "New Chat".to_string();
        self.last_seq = 0;
        self.replay_epoch = None;
        self.messages.clear();
        self.subagents.clear();
        self.pending_prompts.clear();
        self.is_streaming = false;
    }

    pub fn load_profiles(&mut self, result: CanonicalProfilesResult) {
        if result.profiles.is_empty() {
            return;
        }
        self.profiles = result
            .profiles
            .into_iter()
            .map(|row| ProfileInfo {
                display_name: row
                    .display_name
                    .filter(|d| !d.trim().is_empty())
                    .unwrap_or_else(|| row.name.clone()),
                name: row.name,
                model: row.model,
                is_default: row.is_default.unwrap_or(false),
            })
            .collect();
    }

    pub fn load_subagents(&mut self, result: SubagentListResult) {
        let Some(rows) = result.subagents else {
            return;
        };
        self.subagents = rows
            .into_iter()
            .map(|s| {
                let running = matches!(
                    s.status,
                    Some(SubagentStatus::Running | SubagentStatus::Queued)
                );
                let status_label = match s.status {
                    Some(SubagentStatus::Queued) => "queued",
                    Some(SubagentStatus::Running) => "running",
                    Some(SubagentStatus::Completed) => "completed",
                    Some(SubagentStatus::Error) => "error",
                    Some(SubagentStatus::Failed) => "failed",
                    Some(SubagentStatus::Timeout) => "timeout",
                    Some(SubagentStatus::Interrupted) => "interrupted",
                    None => "active",
                }
                .to_string();
                SubagentSummary {
                    id: s.subagent_id,
                    goal: s.goal.unwrap_or_else(|| "Delegated task".into()),
                    running,
                    status_label,
                }
            })
            .collect();
    }

    pub fn load_session_list(&mut self, list: CanonicalListResult) {
        self.sessions = list
            .sessions
            .into_iter()
            .map(|row| {
                let title = if row.title.trim().is_empty() {
                    "Untitled session".to_string()
                } else {
                    row.title
                };
                let is_bot_chat = title == "Bot Chat";
                SessionSummary {
                    session_id: row.session_id,
                    title,
                    is_bot_chat,
                    running: row.running,
                    preview: row.preview.or(row.last_reply).unwrap_or_default(),
                }
            })
            .collect();
    }

    pub fn load_snapshot(&mut self, snap: CanonicalSnapshot) {
        self.active_session_id = Some(snap.session_id.clone());
        if let Some(s) = self.sessions.iter().find(|s| s.session_id == snap.session_id) {
            self.active_title = s.title.clone();
        } else {
            self.active_title = "New Chat".to_string();
        }

        self.is_streaming = snap.running;
        self.last_seq = snap.last_sequence.max(0) as u64;
        if !snap.replay_epoch.is_empty() {
            self.replay_epoch = Some(snap.replay_epoch);
        }
        self.subagents.clear();

        let rows = snap.display_messages.unwrap_or(snap.messages);
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let text = row.text.unwrap_or_else(|| {
                row.content
                    .as_ref()
                    .map(transcript_content_text)
                    .unwrap_or_default()
            });
            match row.role.as_str() {
                "user" => {
                    if !text.trim().is_empty() {
                        items.push(ChatItem::User { text });
                    }
                }
                "assistant" => {
                    if !text.trim().is_empty() {
                        items.push(ChatItem::Assistant {
                            text,
                            reasoning: String::new(),
                            streaming: false,
                        });
                    }
                }
                "tool" => {
                    let name = row.name.unwrap_or_else(|| "tool".to_string());
                    items.push(ChatItem::ToolCall {
                        name,
                        summary: text.lines().next().unwrap_or("").chars().take(90).collect(),
                        status: ToolStatus::Completed,
                        output: if text.is_empty() { None } else { Some(text) },
                    });
                }
                _ => {}
            }
        }
        self.messages = items;
    }

    /// Apply a live or replayed `GatewayEventEnvelope` to the active session view.
    pub fn apply_event(&mut self, env: GatewayEventEnvelope) {
        if let Some(ep) = &env.replay_epoch
            && !ep.is_empty()
        {
            self.replay_epoch = Some(ep.clone());
        }
        if let Some(seq) = env.seq {
            if seq <= self.last_seq && env.replayed {
                return;
            }
            self.last_seq = self.last_seq.max(seq);
        }

        // Ignore session-scoped events that belong to a background session (except sidebar metadata).
        if let (Some(ev_sid), Some(active_sid)) = (&env.session_id, &self.active_session_id)
            && ev_sid != active_sid
        {
            if env.event_type == "session.title"
                && let GatewayNotification::SessionTitle(t) = env.typed()
                && let Some(row) = self.sessions.iter_mut().find(|s| s.session_id == *ev_sid)
            {
                row.title = t.title;
            }
            return;
        }

        match env.typed() {
            GatewayNotification::MessageStart(_) => {
                self.is_streaming = true;
                self.ensure_streaming_assistant();
            }
            GatewayNotification::MessageDelta(delta) => {
                if let Some(chunk) = delta.text {
                    self.is_streaming = true;
                    let item = self.ensure_streaming_assistant();
                    if let ChatItem::Assistant { text, .. } = item {
                        text.push_str(&chunk);
                    }
                }
            }
            GatewayNotification::ThinkingDelta(delta) => {
                let chunk = delta.text;
                if !chunk.is_empty() {
                    self.is_streaming = true;
                    let item = self.ensure_streaming_assistant();
                    if let ChatItem::Assistant { reasoning, .. } = item {
                        reasoning.push_str(&chunk);
                    }
                }
            }
            GatewayNotification::ReasoningDelta(delta) => {
                let chunk = delta.text;
                if !chunk.is_empty() {
                    self.is_streaming = true;
                    let item = self.ensure_streaming_assistant();
                    if let ChatItem::Assistant { reasoning, .. } = item {
                        reasoning.push_str(&chunk);
                    }
                }
            }
            GatewayNotification::MessageComplete(done) => {
                self.is_streaming = false;
                let final_text = done.text.as_ref().map(transcript_content_text);
                if let Some(ChatItem::Assistant {
                    text,
                    reasoning,
                    streaming,
                }) = self.messages.last_mut()
                {
                    if let Some(t) = final_text
                        && !t.is_empty()
                    {
                        *text = t;
                    }
                    if let Some(r) = done.reasoning
                        && reasoning.is_empty()
                    {
                        *reasoning = r;
                    }
                    *streaming = false;
                } else if let Some(t) = final_text
                    && !t.is_empty()
                {
                    self.messages.push(ChatItem::Assistant {
                        text: t,
                        reasoning: done.reasoning.unwrap_or_default(),
                        streaming: false,
                    });
                }
            }
            GatewayNotification::ToolStart(ts) => {
                self.is_streaming = true;
                self.messages.push(ChatItem::ToolCall {
                    name: ts.name,
                    summary: ts.preview.or(ts.args_text).unwrap_or_default(),
                    status: ToolStatus::Running,
                    output: None,
                });
            }
            GatewayNotification::ToolComplete(tc) => {
                let failed = tc
                    .result
                    .as_ref()
                    .and_then(|v| v.get("error"))
                    .is_some_and(|e| !e.is_null());
                let status = if failed {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Completed
                };
                let preview = tc.summary.or(tc.result_text);
                if let Some(ChatItem::ToolCall {
                    status: st,
                    output,
                    summary,
                    ..
                }) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| matches!(m, ChatItem::ToolCall { name, status: ToolStatus::Running, .. } if *name == tc.name))
                {
                    *st = status;
                    if let Some(prev) = preview
                        && !prev.is_empty()
                    {
                        if summary.is_empty() {
                            *summary = prev.clone();
                        }
                        *output = Some(prev);
                    }
                } else {
                    self.messages.push(ChatItem::ToolCall {
                        name: tc.name,
                        summary: preview.clone().unwrap_or_default(),
                        status,
                        output: preview,
                    });
                }
            }
            GatewayNotification::SubagentStart(sub) => {
                let id = sub
                    .subagent_id
                    .unwrap_or_else(|| format!("task-{}", sub.task_index));
                let goal = if sub.goal.is_empty() {
                    format!("Subagent #{}", sub.task_index + 1)
                } else {
                    sub.goal
                };
                if let Some(existing) = self.subagents.iter_mut().find(|s| s.id == id) {
                    existing.goal = goal;
                    existing.running = true;
                    existing.status_label = "running".into();
                } else {
                    self.subagents.push(SubagentSummary {
                        id,
                        goal,
                        running: true,
                        status_label: "running".into(),
                    });
                }
            }
            GatewayNotification::SubagentProgress(sub) => {
                let id = sub
                    .subagent_id
                    .unwrap_or_else(|| format!("task-{}", sub.task_index));
                if let Some(existing) = self.subagents.iter_mut().find(|s| s.id == id) {
                    existing.running = true;
                    existing.status_label = "working".into();
                }
            }
            GatewayNotification::SubagentComplete(sub) => {
                let id = sub
                    .subagent_id
                    .unwrap_or_else(|| format!("task-{}", sub.task_index));
                if let Some(existing) = self.subagents.iter_mut().find(|s| s.id == id) {
                    existing.running = false;
                    existing.status_label = "completed".into();
                }
            }
            GatewayNotification::SessionTitle(t) => {
                self.active_title = t.title.clone();
                if let Some(active_sid) = &self.active_session_id
                    && let Some(row) = self
                        .sessions
                        .iter_mut()
                        .find(|s| s.session_id == *active_sid)
                {
                    row.title = t.title;
                }
            }
            GatewayNotification::Error(err) => {
                self.is_streaming = false;
                self.messages.push(ChatItem::Notice {
                    text: err.message,
                    is_error: true,
                });
            }
            GatewayNotification::Notice(notice) => {
                self.messages.push(ChatItem::Notice {
                    text: notice.message,
                    is_error: false,
                });
            }
            GatewayNotification::RequestCancel(cancel) => {
                self.pending_prompts.retain(|p| p.id != cancel.id);
            }
            _ => {}
        }
    }

    /// Filter sidebar sessions by title or preview text.
    pub fn filtered_sessions(&self, query: &str) -> Vec<&SessionSummary> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return self.sessions.iter().collect();
        }
        self.sessions
            .iter()
            .filter(|s| {
                s.title.to_lowercase().contains(&q) || s.preview.to_lowercase().contains(&q)
            })
            .collect()
    }

    /// Filter the slash command palette when the composer starts with `/`.
    pub fn matching_slash_commands(&self, input: &str) -> Vec<&SlashCommandItem> {
        let trimmed = input.trim_start();
        if !trimmed.starts_with('/') || trimmed.contains('\n') {
            return Vec::new();
        }
        let query = trimmed[1..]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase();
        self.slash_catalog
            .iter()
            .filter(|cmd| {
                let bare = cmd.name.trim_start_matches('/').to_lowercase();
                query.is_empty()
                    || bare.contains(&query)
                    || cmd.description.to_lowercase().contains(&query)
            })
            .take(8)
            .collect()
    }

    /// Build ranked items for the `Cmd+K` Command Palette.
    pub fn palette_items(&self, query: &str) -> Vec<PaletteItem> {
        let q = query.trim().to_lowercase();
        let mut items = vec![
            PaletteItem {
                title: "New Chat".into(),
                subtitle: "Start a fresh Hermes session (Cmd+N)".into(),
                badge: "Action".into(),
                action: PaletteAction::NewChat,
            },
            PaletteItem {
                title: "Open Bot Chat".into(),
                subtitle: "Jump to this profile's canonical forever-chat".into(),
                badge: "Bot".into(),
                action: PaletteAction::OpenBotChat,
            },
            PaletteItem {
                title: "Quick Entry Window".into(),
                subtitle: "Open floating mini-composer (Cmd+Shift+K)".into(),
                badge: "Window".into(),
                action: PaletteAction::OpenQuickEntry,
            },
            PaletteItem {
                title: "Toggle Native Terminal".into(),
                subtitle: "Open embedded PTY shell in workspace pane (Cmd+J)".into(),
                badge: "Workspace".into(),
                action: PaletteAction::OpenTerminal,
            },
            PaletteItem {
                title: "Git Review & Diffs".into(),
                subtitle: "Inspect branch, worktrees, stage/unstage, unified diffs".into(),
                badge: "Workspace".into(),
                action: PaletteAction::OpenGitReview,
            },
            PaletteItem {
                title: "Workspace File Explorer".into(),
                subtitle: "Browse workspace files with syntax-highlighted preview".into(),
                badge: "Workspace".into(),
                action: PaletteAction::OpenFiles,
            },
            PaletteItem {
                title: "Control Center (Config / Cron / Usage)".into(),
                subtitle: "Inspect gateway config, skills, cron jobs, and usage (Cmd+,)".into(),
                badge: "Settings".into(),
                action: PaletteAction::OpenSettings,
            },
            PaletteItem {
                title: "Toggle Dark / Light Theme".into(),
                subtitle: "Switch between Nous Dark and Nous Light palettes".into(),
                badge: "View".into(),
                action: PaletteAction::ToggleTheme,
            },
            PaletteItem {
                title: if self.show_reasoning {
                    "Hide Thinking / Reasoning".into()
                } else {
                    "Show Thinking / Reasoning".into()
                },
                subtitle: "Toggle inline reasoning blocks".into(),
                badge: "View".into(),
                action: PaletteAction::ToggleReasoning,
            },
            PaletteItem {
                title: "Toggle Sidebar".into(),
                subtitle: "Show or hide session sidebar (Cmd+B)".into(),
                badge: "View".into(),
                action: PaletteAction::ToggleSidebar,
            },
        ];

        for s in &self.sessions {
            items.push(PaletteItem {
                title: s.title.clone(),
                subtitle: s.preview.clone(),
                badge: if s.is_bot_chat { "Bot" } else { "Session" }.into(),
                action: PaletteAction::SelectSession(s.session_id.clone()),
            });
        }

        for p in &self.profiles {
            if p.name != self.active_profile {
                items.push(PaletteItem {
                    title: format!("Switch Profile: {}", p.display_name),
                    subtitle: p.model.clone().unwrap_or_else(|| p.name.clone()),
                    badge: "Profile".into(),
                    action: PaletteAction::SwitchProfile(p.name.clone()),
                });
            }
        }

        for cmd in &self.slash_catalog {
            items.push(PaletteItem {
                title: cmd.name.clone(),
                subtitle: cmd.description.clone(),
                badge: cmd.category.clone(),
                action: PaletteAction::InsertSlash(cmd.name.clone()),
            });
        }

        if q.is_empty() {
            items.truncate(12);
            return items;
        }

        items
            .into_iter()
            .filter(|item| {
                item.title.to_lowercase().contains(&q)
                    || item.subtitle.to_lowercase().contains(&q)
                    || item.badge.to_lowercase().contains(&q)
            })
            .take(12)
            .collect()
    }

    fn ensure_streaming_assistant(&mut self) -> &mut ChatItem {
        let needs_new = !matches!(
            self.messages.last(),
            Some(ChatItem::Assistant {
                streaming: true,
                ..
            })
        );
        if needs_new {
            self.messages.push(ChatItem::Assistant {
                text: String::new(),
                reasoning: String::new(),
                streaming: true,
            });
        }
        self.messages.last_mut().expect("just pushed")
    }
}

fn default_slash_catalog() -> Vec<SlashCommandItem> {
    vec![
        SlashCommandItem {
            name: "/new".into(),
            description: "Start a fresh session".into(),
            category: "Session".into(),
        },
        SlashCommandItem {
            name: "/compact".into(),
            description: "Compress conversation context".into(),
            category: "Session".into(),
        },
        SlashCommandItem {
            name: "/model".into(),
            description: "Inspect or switch the active model".into(),
            category: "Model".into(),
        },
        SlashCommandItem {
            name: "/skills".into(),
            description: "List or manage installed skills".into(),
            category: "Capabilities".into(),
        },
        SlashCommandItem {
            name: "/status".into(),
            description: "Show gateway & session status".into(),
            category: "Session".into(),
        },
        SlashCommandItem {
            name: "/clear".into(),
            description: "Clear local transcript view".into(),
            category: "View".into(),
        },
        SlashCommandItem {
            name: "/help".into(),
            description: "Show available commands".into(),
            category: "Help".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accumulates_streaming_deltas_and_completes_cleanly() {
        let mut state = CompanionState::default();
        state.active_session_id = Some("s1".into());

        for (seq, chunk) in [(1, "Hello "), (2, "world!")] {
            state.apply_event(GatewayEventEnvelope {
                event_type: "message.delta".into(),
                session_id: Some("s1".into()),
                seq: Some(seq),
                replay_epoch: Some("ep-1".into()),
                authority_epoch: None,
                execution_generation: None,
                admission_id: None,
                profile: None,
                replayed: false,
                payload: json!({ "text": chunk, "session_id": "s1" }),
            });
        }
        assert!(state.is_streaming);
        assert_eq!(state.replay_epoch.as_deref(), Some("ep-1"));
        assert_eq!(state.messages.len(), 1);

        state.apply_event(GatewayEventEnvelope {
            event_type: "message.complete".into(),
            session_id: Some("s1".into()),
            seq: Some(3),
            replay_epoch: Some("ep-1".into()),
            authority_epoch: None,
            execution_generation: None,
            admission_id: None,
            profile: None,
            replayed: false,
            payload: json!({ "text": "Hello world!", "session_id": "s1" }),
        });

        assert!(!state.is_streaming);
        assert_eq!(
            state.messages[0],
            ChatItem::Assistant {
                text: "Hello world!".into(),
                reasoning: String::new(),
                streaming: false,
            }
        );
    }

    #[test]
    fn filters_slash_commands_sessions_and_palette() {
        let state = CompanionState::demo();
        assert_eq!(state.matching_slash_commands("/comp").len(), 1);
        assert!(state.matching_slash_commands("hello /compact").is_empty());

        let filtered = state.filtered_sessions("benchmark");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].session_id, "demo-2");

        let palette = state.palette_items("work");
        assert!(palette
            .iter()
            .any(|p| p.action == PaletteAction::SwitchProfile("work".into())));
    }

    #[test]
    fn tracks_subagent_start_and_complete_events() {
        let mut state = CompanionState::default();
        state.active_session_id = Some("s1".into());
        state.apply_event(GatewayEventEnvelope {
            event_type: "subagent.start".into(),
            session_id: Some("s1".into()),
            seq: Some(1),
            replay_epoch: None,
            authority_epoch: None,
            execution_generation: None,
            admission_id: None,
            profile: None,
            replayed: false,
            payload: json!({ "subagent_id": "child-1", "goal": "Audit Cargo.toml", "task_index": 0, "task_count": 1 }),
        });
        assert_eq!(state.subagents.len(), 1);
        assert!(state.subagents[0].running);

        state.apply_event(GatewayEventEnvelope {
            event_type: "subagent.complete".into(),
            session_id: Some("s1".into()),
            seq: Some(2),
            replay_epoch: None,
            authority_epoch: None,
            execution_generation: None,
            admission_id: None,
            profile: None,
            replayed: false,
            payload: json!({ "subagent_id": "child-1", "goal": "Audit Cargo.toml", "task_index": 0, "task_count": 1 }),
        });
        assert!(!state.subagents[0].running);
        assert_eq!(state.subagents[0].status_label, "completed");
    }
}
