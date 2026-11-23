//! Entry point for `hermes-companion` — macOS-first native GPUI companion for Hermes Agent.
//!
//! Usage:
//!   hermes-companion                     # Attach locally via `hermes gateway attach --json --start`
//!   hermes-companion --profile work      # Attach to a named profile
//!   hermes-companion --url ws://.../ws   # Connect directly to a gateway WebSocket URL
//!   hermes-companion --demo              # Launch offline interactive demo (no gateway needed)

mod app_view;
mod composer;
mod highlight;
mod inspector;
mod management;
mod markdown;
mod quick_entry;
mod state;
mod theme;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, prelude::*, px,
    size,
};
use hermes_gateway::{
    AttachOptions, DEFAULT_TRANSCRIPT_PAGE_LIMIT, GatewayClient, GatewayDialTarget,
    IncomingServerRequest, ReconnectPolicy, ReplayOutcome,
};
use hermes_protocol::ServerRequestPayload;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::app_view::{CompanionView, UiCommand, WorkerEvent};
use crate::management::ManagementData;
use crate::state::{
    CompanionState, ConnectionStatus, PendingPrompt, PendingPromptKind, SlashCommandItem,
};

#[derive(Debug, Clone)]
struct CliConfig {
    demo: bool,
    url: Option<String>,
    profile: Option<String>,
    hermes_home: Option<PathBuf>,
    hermes_bin: Option<String>,
    workspace_dir: PathBuf,
}

impl CliConfig {
    fn from_args() -> Self {
        let mut cfg = Self {
            demo: false,
            url: None,
            profile: None,
            hermes_home: None,
            hermes_bin: None,
            workspace_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--demo" => cfg.demo = true,
                "--url" => cfg.url = args.next(),
                "--profile" | "-p" => cfg.profile = args.next(),
                "--hermes-home" => cfg.hermes_home = args.next().map(PathBuf::from),
                "--hermes-bin" => cfg.hermes_bin = args.next(),
                "--cwd" => {
                    if let Some(p) = args.next() {
                        cfg.workspace_dir = PathBuf::from(p);
                    }
                }
                "--help" | "-h" => {
                    println!(
                        "hermes-companion (GPUI Milestone 2)\n\n\
                         Options:\n\
                           --demo                 Run offline interactive demo\n\
                           --url <ws://...>       Connect directly to a gateway WebSocket URL\n\
                           -p, --profile <name>   Attach to a specific Hermes profile\n\
                           --cwd <path>           Workspace directory for Terminal, Git, and Files\n\
                           --hermes-home <path>   Override HERMES_HOME\n\
                           --hermes-bin <path>    Path to the `hermes` executable\n"
                    );
                    std::process::exit(0);
                }
                _ => {}
            }
        }
        cfg
    }
}

#[derive(Debug, Default, Clone)]
struct ReplayCursor {
    session_id: Option<String>,
    last_seq: u64,
    replay_epoch: Option<String>,
}

fn spawn_gateway_worker(
    cfg: CliConfig,
    mut cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    ui_tx: mpsc::UnboundedSender<WorkerEvent>,
) {
    std::thread::Builder::new()
        .name("hermes-gateway-rt".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build Tokio runtime");

            rt.block_on(async move {
                run_gateway_loop(cfg, &mut cmd_rx, &ui_tx).await;
            });
        })
        .expect("failed to spawn gateway runtime thread");
}

async fn connect_once(
    cfg: &CliConfig,
    start: bool,
    recover: bool,
) -> Result<(GatewayClient, mpsc::Receiver<IncomingServerRequest>), hermes_gateway::GatewayError> {
    if let Some(url) = &cfg.url {
        GatewayClient::connect(GatewayDialTarget::from_url(url.clone())).await
    } else {
        let mut opts = AttachOptions::default();
        opts.profile = cfg.profile.clone();
        opts.hermes_home = cfg.hermes_home.clone();
        opts.start = start;
        opts.recover = recover;
        if let Some(bin) = &cfg.hermes_bin {
            opts.hermes_bin = bin.clone();
        }
        GatewayClient::connect_local(&opts).await
    }
}

async fn run_gateway_loop(
    mut cfg: CliConfig,
    cmd_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
    ui_tx: &mpsc::UnboundedSender<WorkerEvent>,
) {
    let mut client: Option<GatewayClient> = None;
    let mut reconnect_policy = ReconnectPolicy::default();
    let mut cursor = ReplayCursor::default();
    let mut pending_server_reqs: HashMap<String, IncomingServerRequest> = HashMap::new();
    let (internal_req_tx, mut internal_req_rx) = mpsc::unbounded_channel::<IncomingServerRequest>();
    let (reconnect_tx, mut reconnect_rx) = mpsc::unbounded_channel::<()>();

    // Initial connection attempt (`--start` enabled on first launch)
    let _ = ui_tx.send(WorkerEvent::Status(ConnectionStatus::Connecting));
    match connect_once(&cfg, true, false).await {
        Ok((c, server_rx)) => {
            reconnect_policy.reset_backoff();
            attach_disconnect_watcher(&c, &reconnect_tx);
            hydrate_after_connect(&c, server_rx, ui_tx, &internal_req_tx, None).await;
            client = Some(c);
        }
        Err(err) => {
            warn!(error = %err, "initial gateway attach failed");
            let _ = ui_tx.send(WorkerEvent::Status(ConnectionStatus::Disconnected(
                err.to_string(),
            )));
            let _ = ui_tx.send(WorkerEvent::Notice {
                text: format!(
                    "Could not attach to local Hermes gateway ({err}). Click the status pill to retry, or launch with `--demo`."
                ),
                is_error: true,
            });
        }
    }

    loop {
        tokio::select! {
            Some(()) = reconnect_rx.recv() => {
                // Only auto-reconnect if we previously had a live client that dropped.
                if client.as_ref().is_some_and(GatewayClient::is_connected) {
                    continue;
                }
                client = None;
                let delay = reconnect_policy.next_delay();
                let attempt = reconnect_policy.attempts();
                let _ = ui_tx.send(WorkerEvent::Status(ConnectionStatus::Reconnecting {
                    attempt,
                    delay_secs: delay.as_secs(),
                }));
                tokio::time::sleep(delay).await;

                let recover = reconnect_policy.allow_owner_recovery(Instant::now());
                match connect_once(&cfg, false, recover).await {
                    Ok((c, server_rx)) => {
                        reconnect_policy.reset_backoff();
                        attach_disconnect_watcher(&c, &reconnect_tx);
                        hydrate_after_connect(&c, server_rx, ui_tx, &internal_req_tx, Some(&cursor)).await;
                        client = Some(c);
                    }
                    Err(err) => {
                        warn!(attempt, error = %err, "auto-reconnect attempt failed");
                        let _ = ui_tx.send(WorkerEvent::Status(ConnectionStatus::Disconnected(
                            err.to_string(),
                        )));
                        // Schedule next backoff retry automatically
                        let _ = reconnect_tx.send(());
                    }
                }
            }
            Some(req) = internal_req_rx.recv() => {
                let req_id = req.id.to_string();
                let maybe_prompt = match &req.payload {
                    ServerRequestPayload::Approval(a) => Some(PendingPrompt {
                        id: req_id.clone(),
                        session_id: Some(a.session_id.clone()),
                        kind: PendingPromptKind::Approval {
                            command: a.command.clone().unwrap_or_default(),
                            description: a.description.clone().unwrap_or_default(),
                            choices: a
                                .choices
                                .as_ref()
                                .map(|list| {
                                    list.iter()
                                        .map(|c| match c {
                                            hermes_protocol::ApprovalChoice::Once => "once",
                                            hermes_protocol::ApprovalChoice::Session => "session",
                                            hermes_protocol::ApprovalChoice::Always => "always",
                                            hermes_protocol::ApprovalChoice::Deny => "deny",
                                        })
                                        .map(String::from)
                                        .collect()
                                })
                                .unwrap_or_else(|| {
                                    vec!["once".into(), "session".into(), "deny".into()]
                                }),
                        },
                    }),
                    ServerRequestPayload::Clarify(c) => {
                        let first = c.questions.first();
                        Some(PendingPrompt {
                            id: req_id.clone(),
                            session_id: Some(c.session_id.clone()),
                            kind: PendingPromptKind::Clarify {
                                question: first
                                    .map(|q| q.question.clone())
                                    .unwrap_or_else(|| "Clarification requested".into()),
                                choices: first.and_then(|q| q.choices.clone()).unwrap_or_default(),
                            },
                        })
                    }
                    _ => None,
                };
                if let Some(prompt) = maybe_prompt {
                    pending_server_reqs.insert(req_id, req);
                    let _ = ui_tx.send(WorkerEvent::ServerPrompt(prompt));
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    UiCommand::SyncReplayCursor { session_id, last_seq, replay_epoch } => {
                        cursor.session_id = session_id;
                        cursor.last_seq = last_seq;
                        cursor.replay_epoch = replay_epoch;
                    }
                    UiCommand::Reconnect => {
                        reconnect_policy.reset_backoff();
                        let _ = ui_tx.send(WorkerEvent::Status(ConnectionStatus::Connecting));
                        match connect_once(&cfg, true, true).await {
                            Ok((c, server_rx)) => {
                                attach_disconnect_watcher(&c, &reconnect_tx);
                                hydrate_after_connect(&c, server_rx, ui_tx, &internal_req_tx, Some(&cursor)).await;
                                client = Some(c);
                            }
                            Err(err) => {
                                let _ = ui_tx.send(WorkerEvent::Status(ConnectionStatus::Disconnected(err.to_string())));
                            }
                        }
                    }
                    UiCommand::SwitchProfile(next_profile) => {
                        cfg.profile = Some(next_profile);
                        client = None;
                        cursor = ReplayCursor::default();
                        pending_server_reqs.clear();
                        reconnect_policy.reset_backoff();
                        let _ = ui_tx.send(WorkerEvent::Status(ConnectionStatus::Connecting));
                        match connect_once(&cfg, true, false).await {
                            Ok((c, server_rx)) => {
                                attach_disconnect_watcher(&c, &reconnect_tx);
                                hydrate_after_connect(&c, server_rx, ui_tx, &internal_req_tx, None).await;
                                client = Some(c);
                            }
                            Err(err) => {
                                let _ = ui_tx.send(WorkerEvent::Status(ConnectionStatus::Disconnected(err.to_string())));
                                let _ = ui_tx.send(WorkerEvent::Notice {
                                    text: format!("Profile switch failed: {err}"),
                                    is_error: true,
                                });
                            }
                        }
                    }
                    UiCommand::RefreshManagement => {
                        if let Some(c) = &client {
                            let timeout = Duration::from_secs(5);
                            let config_json = c.call_raw("config.get", Some(json!({})), timeout).await.ok();
                            let cron_json = c.call_raw("cron.status", Some(json!({})), timeout).await.ok();
                            let usage_json = c
                                .call_raw("session.account_usage", Some(json!({})), timeout)
                                .await
                                .ok();
                            let _ = ui_tx.send(WorkerEvent::Management(ManagementData {
                                config_json,
                                cron_json,
                                usage_json,
                            }));
                        }
                    }
                    UiCommand::NewSession => {
                        if let Some(c) = &client {
                            match c.create_session(None, None).await {
                                Ok(snap) => {
                                    let _ = ui_tx.send(WorkerEvent::Snapshot(snap));
                                    if let Ok(list) = c.list_sessions(Some(50), false).await {
                                        let _ = ui_tx.send(WorkerEvent::SessionList(list));
                                    }
                                }
                                Err(err) => {
                                    let _ = ui_tx.send(WorkerEvent::Notice {
                                        text: format!("Failed to create session: {err}"),
                                        is_error: true,
                                    });
                                }
                            }
                        }
                    }
                    UiCommand::OpenBotChat => {
                        if let Some(c) = &client {
                            match c.open_or_create_bot_chat().await {
                                Ok(loaded) => {
                                    let sid = loaded.snapshot.session_id.clone();
                                    let _ = ui_tx.send(WorkerEvent::SessionLoaded(Box::new(loaded)));
                                    if let Ok(subs) = c.list_subagents(sid).await {
                                        let _ = ui_tx.send(WorkerEvent::Subagents(subs));
                                    }
                                }
                                Err(err) => {
                                    let _ = ui_tx.send(WorkerEvent::Notice {
                                        text: format!("Failed to open Bot Chat: {err}"),
                                        is_error: true,
                                    });
                                }
                            }
                        }
                    }
                    UiCommand::SelectSession(sid) => {
                        if let Some(c) = &client {
                            match c
                                .resume_session_tail(sid.clone(), DEFAULT_TRANSCRIPT_PAGE_LIMIT)
                                .await
                            {
                                Ok(loaded) => {
                                    let _ = ui_tx.send(WorkerEvent::SessionLoaded(Box::new(loaded)));
                                    if let Ok(subs) = c.list_subagents(sid).await {
                                        let _ = ui_tx.send(WorkerEvent::Subagents(subs));
                                    }
                                }
                                Err(err) => {
                                    let _ = ui_tx.send(WorkerEvent::Notice {
                                        text: format!("Failed to resume session: {err}"),
                                        is_error: true,
                                    });
                                }
                            }
                        }
                    }
                    UiCommand::LoadOlderMessages { session_id, cursor } => {
                        if let Some(c) = &client {
                            match c
                                .transcript_page(
                                    &session_id,
                                    Some(cursor),
                                    DEFAULT_TRANSCRIPT_PAGE_LIMIT,
                                )
                                .await
                            {
                                Ok(page) => {
                                    let _ = ui_tx.send(WorkerEvent::OlderPage {
                                        session_id,
                                        rows: page.rows,
                                        next_cursor: page.next_cursor,
                                        reset: page.reset,
                                    });
                                }
                                Err(err) => {
                                    let _ = ui_tx.send(WorkerEvent::Notice {
                                        text: format!("Failed to load earlier messages: {err}"),
                                        is_error: true,
                                    });
                                }
                            }
                        }
                    }
                    UiCommand::SubmitPrompt { session_id, text } => {
                        if let Some(c) = &client {
                            let target_sid = match session_id {
                                Some(sid) => Ok(sid),
                                None => c.create_session(None, None).await.map(|snap| {
                                    let sid = snap.session_id.clone();
                                    let _ = ui_tx.send(WorkerEvent::Snapshot(snap));
                                    sid
                                }),
                            };
                            match target_sid {
                                Ok(sid) => {
                                    if let Err(err) = c.submit_prompt(sid, text).await {
                                        let _ = ui_tx.send(WorkerEvent::Notice {
                                            text: format!("Submit failed: {err}"),
                                            is_error: true,
                                        });
                                    }
                                }
                                Err(err) => {
                                    let _ = ui_tx.send(WorkerEvent::Notice {
                                        text: format!("Could not start session: {err}"),
                                        is_error: true,
                                    });
                                }
                            }
                        }
                    }
                    UiCommand::RunSlash { session_id, command } => {
                        if let Some(c) = &client {
                            let sid = session_id.unwrap_or_default();
                            match c.slash_exec(sid, command).await {
                                Ok(res) => {
                                    if let Some(out) = res.output {
                                        let _ = ui_tx.send(WorkerEvent::Notice {
                                            text: out,
                                            is_error: false,
                                        });
                                    }
                                }
                                Err(err) => {
                                    let _ = ui_tx.send(WorkerEvent::Notice {
                                        text: format!("Slash command error: {err}"),
                                        is_error: true,
                                    });
                                }
                            }
                        }
                    }
                    UiCommand::CancelPrompt(sid) => {
                        if let Some(c) = &client {
                            let _ = c.cancel_prompt(sid).await;
                        }
                    }
                    UiCommand::AnswerPrompt { request_id, choice } => {
                        if let Some(req) = pending_server_reqs.remove(&request_id) {
                            let body = match req.method.as_str() {
                                "clarify" => json!({ "answers": { "0": choice } }),
                                _ => json!({ "choice": choice }),
                            };
                            if let Err(err) = req.respond(body) {
                                error!(error = %err, "failed to answer server request");
                            }
                        }
                    }
                }
            }
            else => break,
        }
    }
}

fn attach_disconnect_watcher(
    client: &GatewayClient,
    reconnect_tx: &mpsc::UnboundedSender<()>,
) {
    let watched = client.clone();
    let tx = reconnect_tx.clone();
    tokio::spawn(async move {
        watched.wait_closed().await;
        let _ = tx.send(());
    });
}

async fn hydrate_after_connect(
    client: &GatewayClient,
    mut server_rx: mpsc::Receiver<IncomingServerRequest>,
    ui_tx: &mpsc::UnboundedSender<WorkerEvent>,
    internal_req_tx: &mpsc::UnboundedSender<IncomingServerRequest>,
    replay_cursor: Option<&ReplayCursor>,
) {
    let hs = client.handshake();
    if let Some(skin) = hs.runtime.as_ref().and_then(|r| r.skin.clone()) {
        let _ = ui_tx.send(WorkerEvent::Skin(skin));
    }

    let profile = hs
        .runtime
        .as_ref()
        .map(|r| r.profile_id.clone())
        .or_else(|| hs.target.endpoint.as_ref().map(|e| e.profile_id.clone()))
        .unwrap_or_else(|| "default".to_string());
    let instance_id = hs
        .runtime
        .as_ref()
        .map(|r| r.instance_id.clone())
        .unwrap_or_else(|| "local".to_string());
    let protocol = hs
        .negotiated_subprotocol
        .unwrap_or_else(|| "hermes-gateway-v1".to_string());

    info!(%profile, %instance_id, %protocol, "connected to Hermes gateway");
    let _ = ui_tx.send(WorkerEvent::Status(ConnectionStatus::Connected {
        profile,
        instance_id,
        protocol,
    }));

    // Forward live gateway events to GPUI.
    let mut events_rx = client.subscribe_events();
    let ev_ui_tx = ui_tx.clone();
    tokio::spawn(async move {
        loop {
            match events_rx.recv().await {
                Ok(env) => {
                    if ev_ui_tx.send(WorkerEvent::GatewayEvent(env)).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(dropped = n, "UI event stream lagged behind gateway");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Forward server->client requests into the worker loop.
    let req_tx = internal_req_tx.clone();
    tokio::spawn(async move {
        while let Some(req) = server_rx.recv().await {
            if req_tx.send(req).is_err() {
                break;
            }
        }
    });

    // Hydrate profile list.
    if let Ok(profiles) = client.list_profiles().await {
        let _ = ui_tx.send(WorkerEvent::Profiles(profiles));
    }

    // Hydrate session list and either replay active session or resume most recent session.
    if let Ok(list) = client.list_sessions(Some(50), false).await {
        let first_id = list.sessions.first().map(|s| s.session_id.clone());
        let _ = ui_tx.send(WorkerEvent::SessionList(list));

        if let Some(cur) = replay_cursor
            && let Some(sid) = cur.session_id.as_deref()
        {
            match client
                .replay_or_resume(sid, cur.last_seq, cur.replay_epoch.clone())
                .await
            {
                Ok(ReplayOutcome::Events { events, .. }) => {
                    for ev in events {
                        let _ = ui_tx.send(WorkerEvent::GatewayEvent(ev));
                    }
                }
                Ok(ReplayOutcome::Snapshot(loaded)) => {
                    let _ = ui_tx.send(WorkerEvent::SessionLoaded(loaded));
                }
                Err(err) => {
                    warn!(error = %err, "reconnect catchup failed");
                }
            }
        } else if let Some(sid) = first_id
            && let Ok(loaded) = client
                .resume_session_tail(sid.clone(), DEFAULT_TRANSCRIPT_PAGE_LIMIT)
                .await
        {
            let _ = ui_tx.send(WorkerEvent::SessionLoaded(Box::new(loaded)));
            if let Ok(subs) = client.list_subagents(sid).await {
                let _ = ui_tx.send(WorkerEvent::Subagents(subs));
            }
        }
    }

    if let Ok(catalog) = client.commands_catalog().await {
        let meta = catalog.commands.unwrap_or_default();
        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();

        if let Some(categories) = catalog.categories {
            for cat in categories {
                for pair in cat.pairs.unwrap_or_default() {
                    if let Some(name) = pair.first() {
                        let hidden = meta
                            .get(name)
                            .and_then(|m| m.desktop.as_deref())
                            .is_some_and(|d| {
                                matches!(d, "hidden" | "terminal-only" | "messaging-only")
                            });
                        if !hidden && seen.insert(name.clone()) {
                            items.push(SlashCommandItem {
                                name: name.clone(),
                                description: pair.get(1).cloned().unwrap_or_default(),
                                category: cat.name.clone(),
                            });
                        }
                    }
                }
            }
        } else if let Some(pairs) = catalog.pairs {
            for pair in pairs {
                if let Some(name) = pair.first()
                    && seen.insert(name.clone())
                {
                    items.push(SlashCommandItem {
                        name: name.clone(),
                        description: pair.get(1).cloned().unwrap_or_default(),
                        category: "Command".into(),
                    });
                }
            }
        }

        // Always keep skill-derived slash commands discoverable (apps/desktop/src/AGENTS.md).
        if let Some(skills) = catalog.skills {
            for (skill_name, entry) in skills {
                let slash_name = if skill_name.starts_with('/') {
                    skill_name
                } else {
                    format!("/{skill_name}")
                };
                if seen.insert(slash_name.clone()) {
                    items.push(SlashCommandItem {
                        name: slash_name,
                        description: entry.origin.unwrap_or_else(|| "Skill command".into()),
                        category: "Skills".into(),
                    });
                }
            }
        }

        if !items.is_empty() {
            let _ = ui_tx.send(WorkerEvent::SlashCatalog(items));
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hermes_companion=info,hermes_gateway=info".into()),
        )
        .init();

    let cfg = CliConfig::from_args();
    let workspace_dir = cfg.workspace_dir.clone();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<WorkerEvent>();

    let initial_state = if cfg.demo {
        CompanionState::demo()
    } else {
        spawn_gateway_worker(cfg, cmd_rx, ui_tx);
        CompanionState::default()
    };

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1240.0), px(800.0)), cx);
        let mut initial_state = Some(initial_state);
        let mut ui_rx = Some(ui_rx);
        let cmd_tx = cmd_tx.clone();
        let workspace_dir = workspace_dir.clone();

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Hermes".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.0), px(12.0))),
                }),
                ..Default::default()
            },
            move |_, cx| {
                let state = initial_state.take().unwrap_or_default();
                let rx = ui_rx
                    .take()
                    .unwrap_or_else(|| mpsc::unbounded_channel().1);
                cx.new(|cx| {
                    CompanionView::new(state, workspace_dir.clone(), cmd_tx.clone(), rx, cx)
                })
            },
        )
        .expect("failed to open Hermes companion window");

        cx.activate(true);
    });
}
