//! UI-agnostic state & event reducer for the Hermes companion app.
//!
//! Kept free of GPUI types so all transcript accumulation, streaming delta handling,
//! Bot Mode canonical chat labeling, approval prompts, and slash palette filtering
//! are unit-testable headlessly.

use hermes_protocol::{
    CanonicalListResult, CanonicalSnapshot, GatewayEventEnvelope, GatewayNotification,
    transcript_content_text,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting,
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
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    pub is_bot_chat: bool,
    pub running: bool,
    pub preview: String,
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
    /// Correlation key: either a server->client JSON-RPC request id (`srq-...`)
    /// or a canonical `prompt_id`.
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

/// Complete Milestone 0 companion workspace state.
#[derive(Debug, Clone)]
pub struct CompanionState {
    pub status: ConnectionStatus,
    pub sidebar_open: bool,
    pub sessions: Vec<SessionSummary>,
    pub active_session_id: Option<String>,
    pub active_title: String,
    pub last_seq: u64,
    pub messages: Vec<ChatItem>,
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
            sessions: Vec::new(),
            active_session_id: None,
            active_title: "New Chat".to_string(),
            last_seq: 0,
            messages: Vec::new(),
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
    /// streaming Markdown, tool calls, slash palette, and approval cards.
    pub fn demo() -> Self {
        let mut state = Self::default();
        state.status = ConnectionStatus::Demo;
        state.sessions = vec![
            SessionSummary {
                session_id: "demo-1".to_string(),
                title: "GPUI Companion Milestone 0".to_string(),
                is_bot_chat: false,
                running: false,
                preview: "Native Rust shell connected to hermes-gateway-v1".to_string(),
            },
            SessionSummary {
                session_id: "demo-bot".to_string(),
                title: "Bot Chat".to_string(),
                is_bot_chat: true,
                running: false,
                preview: "Canonical forever-chat for default profile".to_string(),
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
        state.active_title = "GPUI Companion Milestone 0".to_string();
        state.messages = vec![
            ChatItem::User {
                text: "Summarize how this Rust GPUI companion talks to the Hermes gateway.".to_string(),
            },
            ChatItem::ToolCall {
                name: "read_file".to_string(),
                summary: "tui_gateway/AGENTS.md (lines 1-95)".to_string(),
                status: ToolStatus::Completed,
                output: Some("Loaded canonical OpenRPC contract (54 canonical methods, 79 events).".to_string()),
            },
            ChatItem::Assistant {
                reasoning: "Inspecting the attach contract and OpenRPC schema before summarizing.".to_string(),
                text: "### Native GPUI Companion (`hermes-companion`)\n\n\
                       This companion attaches directly to the canonical Hermes gateway without spawning or owning Python:\n\n\
                       - **Local attach:** invokes `hermes gateway attach --json` and redeems the single-use ticket via `Sec-WebSocket-Protocol: hermes-gateway-v1, hermes-gateway-ticket.<t>`.\n\
                       - **Typed wire protocol:** `hermes-protocol` is generated straight from `apps/shared/src/gateway-contract.openrpc.json`.\n\
                       - **Interactive controls:** advertises `client.capabilities { server_requests: true }` so `approval` and `clarify` cards appear inline.\n\n\
                       ```rust\n\
                       let (client, server_reqs) = GatewayClient::connect_local(&AttachOptions::default()).await?;\n\
                       let list = client.list_sessions(Some(50), false).await?;\n\
                       ```\n\n\
                       Try typing `/` below to test the slash palette, or send a message to watch streaming Markdown render natively at 120 fps."
                    .to_string(),
                streaming: false,
            },
        ];
        state.pending_prompts.push(PendingPrompt {
            id: "demo-srq-1".to_string(),
            session_id: Some("demo-1".to_string()),
            kind: PendingPromptKind::Approval {
                command: "cargo test --workspace".to_string(),
                description: "Demo approval card — click Allow or Deny to test inline settlement".to_string(),
                choices: vec![
                    "once".to_string(),
                    "session".to_string(),
                    "deny".to_string(),
                ],
            },
        });
        state
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
        if let Some(seq) = env.seq {
            if seq <= self.last_seq && env.replayed {
                return;
            }
            self.last_seq = self.last_seq.max(seq);
        }

        // Ignore session-scoped events that belong to a background session.
        if let (Some(ev_sid), Some(active_sid)) = (&env.session_id, &self.active_session_id)
            && ev_sid != active_sid
        {
            if env.event_type == "session.title" {
                if let GatewayNotification::SessionTitle(t) = env.typed()
                    && let Some(row) = self.sessions.iter_mut().find(|s| s.session_id == *ev_sid)
                {
                    row.title = t.title;
                }
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

    /// Filter the slash command palette when the composer starts with `/`.
    pub fn matching_slash_commands(&self, input: &str) -> Vec<&SlashCommandItem> {
        let trimmed = input.trim_start();
        if !trimmed.starts_with('/') || trimmed.contains('\n') {
            return Vec::new();
        }
        let query = trimmed[1..].split_whitespace().next().unwrap_or("").to_lowercase();
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
                replay_epoch: None,
                authority_epoch: None,
                execution_generation: None,
                admission_id: None,
                profile: None,
                replayed: false,
                payload: json!({ "text": chunk, "session_id": "s1" }),
            });
        }
        assert!(state.is_streaming);
        assert_eq!(state.messages.len(), 1);

        state.apply_event(GatewayEventEnvelope {
            event_type: "message.complete".into(),
            session_id: Some("s1".into()),
            seq: Some(3),
            replay_epoch: None,
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
    fn filters_slash_commands_by_prefix_and_description() {
        let state = CompanionState::default();
        let all = state.matching_slash_commands("/");
        assert!(!all.is_empty());

        let comp = state.matching_slash_commands("/comp");
        assert_eq!(comp.len(), 1);
        assert_eq!(comp[0].name, "/compact");

        assert!(state.matching_slash_commands("hello /compact").is_empty());
    }
}
