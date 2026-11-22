//! Async `hermes-gateway-v1` WebSocket JSON-RPC client.
//!
//! Responsibilities:
//! - Dialing `/api/ws` with `Sec-WebSocket-Protocol: hermes-gateway-v1, hermes-gateway-ticket.<t>`
//! - Performing the `runtime.describe` + `client.capabilities` handshake
//! - Typed request/response multiplexing (`call::<M>(params)`) with timeouts
//! - `gateway.ping` keepalive heartbeat (15s interval / 45s liveness deadline)
//! - Event fanout (`tokio::sync::broadcast`) and idempotent server->client request answering

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use hermes_protocol::methods;
use hermes_protocol::{
    CANONICAL_GATEWAY_PROTOCOL, CanonicalCreateParams, CanonicalEventsParams, CanonicalEventsResult,
    CanonicalListParams, CanonicalListResult, CanonicalResumeParams, CanonicalSnapshot,
    CanonicalSubmitParams, CanonicalSubmitResult, ClientCapabilitiesParams, CommandsCatalogParams,
    CommandsCatalogResult, CompleteSlashParams, CompleteSlashResult, GatewayEventEnvelope,
    InboundFrame, JSON_RPC_METHOD_NOT_FOUND, JsonRpcErrorPayload, OutboundRequest,
    OutboundResponse, PromptCancelParams, PromptResponseResult, RawInboundFrame, RequestId,
    RpcMethod, RuntimeDescribeResult, ServerRequestPayload, SlashExecParams, SlashExecResult,
};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::attach::{AttachError, AttachOptions, GatewayDialTarget, attach_local_gateway};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const HEARTBEAT_DEADLINE: Duration = Duration::from_secs(45);
const EVENT_CHANNEL_CAPACITY: usize = 1024;
const SERVER_REQUEST_CHANNEL_CAPACITY: usize = 64;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error(transparent)]
    Attach(#[from] AttachError),
    #[error("WebSocket connection error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("invalid WebSocket request: {0}")]
    InvalidRequest(String),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("gateway RPC error ({code:?}): {message}")]
    Rpc {
        code: Option<i64>,
        message: String,
        data: Option<Value>,
    },
    #[error("RPC request `{method}` timed out after {timeout:?}")]
    Timeout { method: String, timeout: Duration },
    #[error("gateway transport closed")]
    Closed,
}

impl From<JsonRpcErrorPayload> for GatewayError {
    fn from(err: JsonRpcErrorPayload) -> Self {
        Self::Rpc {
            code: err.code,
            message: err
                .message
                .unwrap_or_else(|| "Hermes RPC failed".to_string()),
            data: err.data,
        }
    }
}

/// An inbound server->client request (`approval`, `clarify`, `sudo`, `secret`, ...).
///
/// Calling `respond` or `fail` is idempotent: the first reply wins, matching
/// `apps/shared/src/json-rpc-channel.ts::ServerRequest`.
pub struct IncomingServerRequest {
    pub id: RequestId,
    pub method: String,
    pub params: Value,
    pub payload: ServerRequestPayload,
    outbound_tx: mpsc::UnboundedSender<String>,
    answered: AtomicBool,
}

impl IncomingServerRequest {
    pub fn respond(&self, result: Value) -> Result<(), GatewayError> {
        if self.answered.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let frame = OutboundResponse::ok(self.id.clone(), result);
        let text = serde_json::to_string(&frame)?;
        self.outbound_tx
            .send(text)
            .map_err(|_| GatewayError::Closed)
    }

    pub fn fail(&self, code: i64, message: impl Into<String>) -> Result<(), GatewayError> {
        if self.answered.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let frame = OutboundResponse::err(self.id.clone(), code, message);
        let text = serde_json::to_string(&frame)?;
        self.outbound_tx
            .send(text)
            .map_err(|_| GatewayError::Closed)
    }
}

impl Drop for IncomingServerRequest {
    fn drop(&mut self) {
        // Never leave the backend waiting out a 3600s deadline if a request is dropped unanswered.
        if !self.answered.swap(true, Ordering::SeqCst) {
            let frame = OutboundResponse::err(
                self.id.clone(),
                JSON_RPC_METHOD_NOT_FOUND,
                format!("unhandled server request `{}`", self.method),
            );
            if let Ok(text) = serde_json::to_string(&frame) {
                let _ = self.outbound_tx.send(text);
            }
        }
    }
}

/// Connection metadata captured during the initial handshake.
#[derive(Debug, Clone)]
pub struct GatewayHandshake {
    pub target: GatewayDialTarget,
    pub negotiated_subprotocol: Option<String>,
    pub runtime: Option<RuntimeDescribeResult>,
}

type PendingMap = HashMap<RequestId, oneshot::Sender<Result<Value, JsonRpcErrorPayload>>>;

/// Cloneable async handle to a connected Hermes gateway.
#[derive(Clone)]
pub struct GatewayClient {
    inner: Arc<GatewayClientInner>,
}

struct GatewayClientInner {
    outbound_tx: mpsc::UnboundedSender<String>,
    pending: Mutex<PendingMap>,
    next_id: AtomicU64,
    events_tx: broadcast::Sender<GatewayEventEnvelope>,
    target: GatewayDialTarget,
    negotiated_subprotocol: Option<String>,
    runtime: std::sync::OnceLock<RuntimeDescribeResult>,
    alive: AtomicBool,
}

impl GatewayClient {
    /// Attach to the local Hermes gateway via `hermes gateway attach --json` and connect.
    pub async fn connect_local(
        opts: &AttachOptions,
    ) -> Result<(Self, mpsc::Receiver<IncomingServerRequest>), GatewayError> {
        let target = attach_local_gateway(opts).await?;
        Self::connect(target).await
    }

    /// Connect directly to a [`GatewayDialTarget`] (local ticketed target or explicit URL).
    pub async fn connect(
        target: GatewayDialTarget,
    ) -> Result<(Self, mpsc::Receiver<IncomingServerRequest>), GatewayError> {
        let mut request = target
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| GatewayError::InvalidRequest(e.to_string()))?;

        if !target.protocols.is_empty() {
            let header_val = target.protocols.join(", ");
            request.headers_mut().insert(
                SEC_WEBSOCKET_PROTOCOL,
                header_val
                    .parse()
                    .map_err(|e: tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue| {
                        GatewayError::InvalidRequest(e.to_string())
                    })?,
            );
        }

        debug!(target = ?target, "dialing Hermes gateway WebSocket");
        let (ws_stream, response) = tokio_tungstenite::connect_async(request).await?;
        let negotiated_subprotocol = response
            .headers()
            .get(SEC_WEBSOCKET_PROTOCOL)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let (mut ws_sink, mut ws_source) = ws_stream.split();
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<String>();
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (server_req_tx, server_req_rx) = mpsc::channel(SERVER_REQUEST_CHANNEL_CAPACITY);

        let inner = Arc::new(GatewayClientInner {
            outbound_tx: outbound_tx.clone(),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            events_tx,
            target,
            negotiated_subprotocol,
            runtime: std::sync::OnceLock::new(),
            alive: AtomicBool::new(true),
        });

        let last_inbound = Arc::new(Mutex::new(Instant::now()));

        // Writer task
        let writer_alive = Arc::clone(&inner);
        tokio::spawn(async move {
            while let Some(text) = outbound_rx.recv().await {
                if let Err(err) = ws_sink.send(Message::Text(text.into())).await {
                    warn!(error = %err, "gateway WebSocket send failed");
                    break;
                }
            }
            writer_alive.alive.store(false, Ordering::SeqCst);
            let _ = ws_sink.close().await;
        });

        // Reader task
        let reader_inner = Arc::clone(&inner);
        let reader_last_inbound = Arc::clone(&last_inbound);
        tokio::spawn(async move {
            while let Some(msg_res) = ws_source.next().await {
                let text = match msg_res {
                    Ok(Message::Text(s)) => s.to_string(),
                    Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                        *reader_last_inbound.lock().await = Instant::now();
                        continue;
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(Message::Frame(_)) => continue,
                    Err(err) => {
                        warn!(error = %err, "gateway WebSocket read failed");
                        break;
                    }
                };

                *reader_last_inbound.lock().await = Instant::now();

                // Frames may arrive newline-delimited or as single JSON objects.
                for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    let raw: RawInboundFrame = match serde_json::from_str(line) {
                        Ok(frame) => frame,
                        Err(err) => {
                            warn!(error = %err, "dropping unparseable gateway frame");
                            continue;
                        }
                    };
                    match raw.classify() {
                        Some(InboundFrame::Response { id, result }) => {
                            if let Some(tx) = reader_inner.pending.lock().await.remove(&id) {
                                let _ = tx.send(result);
                            }
                        }
                        Some(InboundFrame::Event(env)) => {
                            let _ = reader_inner.events_tx.send(env);
                        }
                        Some(InboundFrame::ServerRequest {
                            id,
                            method,
                            params,
                            parsed,
                        }) => {
                            let handle = IncomingServerRequest {
                                id,
                                method,
                                params,
                                payload: parsed,
                                outbound_tx: reader_inner.outbound_tx.clone(),
                                answered: AtomicBool::new(false),
                            };
                            if server_req_tx.send(handle).await.is_err() {
                                // Drop automatically responds with -32601 so backend never hangs.
                                debug!("server request receiver closed");
                            }
                        }
                        Some(InboundFrame::UncorrelatedError(err)) => {
                            warn!(code = ?err.code, msg = ?err.message, "uncorrelated gateway error");
                        }
                        None => {}
                    }
                }
            }

            reader_inner.alive.store(false, Ordering::SeqCst);
            let mut pending = reader_inner.pending.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(JsonRpcErrorPayload {
                    code: Some(-32000),
                    message: Some("gateway transport closed".to_string()),
                    data: None,
                }));
            }
        });

        let client = Self { inner };

        // Handshake: advertise server_requests capability and fetch `runtime.describe` if canonical.
        let _ = client
            .call_timeout::<methods::ClientCapabilities>(
                ClientCapabilitiesParams {
                    server_requests: Some(true),
                    ..Default::default()
                },
                Duration::from_secs(5),
            )
            .await;

        let runtime = if client.inner.negotiated_subprotocol.as_deref()
            == Some(CANONICAL_GATEWAY_PROTOCOL)
            || client
                .inner
                .target
                .protocols
                .iter()
                .any(|p| p == CANONICAL_GATEWAY_PROTOCOL)
        {
            client
                .call_timeout::<methods::RuntimeDescribe>(
                    hermes_protocol::RuntimeDescribeParams::default(),
                    Duration::from_secs(5),
                )
                .await
                .ok()
        } else {
            None
        };

        if let Some(rt) = runtime {
            let _ = client.inner.runtime.set(rt);
        }

        // Start background `gateway.ping` heartbeat matching `apps/shared/src/json-rpc-channel.ts`.
        let hb_client = client.clone();
        let hb_last_inbound = Arc::clone(&last_inbound);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            interval.tick().await; // skip immediate tick
            while hb_client.is_connected() {
                interval.tick().await;
                if hb_last_inbound.lock().await.elapsed() > HEARTBEAT_DEADLINE {
                    warn!("gateway heartbeat deadline exceeded; marking transport closed");
                    hb_client.inner.alive.store(false, Ordering::SeqCst);
                    break;
                }
                let _ = hb_client
                    .call_raw("gateway.ping", Some(json!({})), Duration::from_secs(10))
                    .await;
            }
        });

        Ok((client, server_req_rx))
    }

    pub fn is_connected(&self) -> bool {
        self.inner.alive.load(Ordering::SeqCst)
    }

    pub fn handshake(&self) -> GatewayHandshake {
        GatewayHandshake {
            target: self.inner.target.clone(),
            negotiated_subprotocol: self.inner.negotiated_subprotocol.clone(),
            runtime: self.inner.runtime.get().cloned(),
        }
    }

    /// Subscribe to the live broadcast stream of `GatewayEventEnvelope`s.
    pub fn subscribe_events(&self) -> broadcast::Receiver<GatewayEventEnvelope> {
        self.inner.events_tx.subscribe()
    }

    /// Invoke a strongly-typed generated RPC method with the default 120s timeout.
    pub async fn call<M: RpcMethod>(&self, params: M::Params) -> Result<M::Result, GatewayError> {
        self.call_timeout::<M>(params, DEFAULT_REQUEST_TIMEOUT).await
    }

    /// Invoke a strongly-typed generated RPC method with a custom timeout.
    pub async fn call_timeout<M: RpcMethod>(
        &self,
        params: M::Params,
        timeout: Duration,
    ) -> Result<M::Result, GatewayError> {
        let raw_params = serde_json::to_value(params)?;
        let raw_result = self.call_raw(M::NAME, Some(raw_params), timeout).await?;
        Ok(serde_json::from_value(raw_result)?)
    }

    /// Invoke any RPC method by name using raw JSON values.
    pub async fn call_raw(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, GatewayError> {
        if !self.is_connected() {
            return Err(GatewayError::Closed);
        }
        let seq = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let id = RequestId::Num(seq);
        let (tx, rx) = oneshot::channel();

        self.inner.pending.lock().await.insert(id.clone(), tx);
        let frame = OutboundRequest::new(id.clone(), method, params);
        let text = serde_json::to_string(&frame)?;

        if self.inner.outbound_tx.send(text).is_err() {
            self.inner.pending.lock().await.remove(&id);
            return Err(GatewayError::Closed);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(val))) => Ok(val),
            Ok(Ok(Err(err))) => Err(GatewayError::from(err)),
            Ok(Err(_)) => Err(GatewayError::Closed),
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                Err(GatewayError::Timeout {
                    method: method.to_string(),
                    timeout,
                })
            }
        }
    }

    // ── High-level Milestone 0 convenience helpers ────────────────────────────

    pub async fn list_sessions(
        &self,
        limit: Option<i64>,
        include_hidden: bool,
    ) -> Result<CanonicalListResult, GatewayError> {
        self.call::<methods::SessionList>(CanonicalListParams {
            limit,
            include_hidden: Some(include_hidden),
            ..Default::default()
        })
        .await
    }

    pub async fn create_session(
        &self,
        cwd: Option<String>,
        title: Option<String>,
    ) -> Result<CanonicalSnapshot, GatewayError> {
        // Pick a source accepted by `runtime.describe` (`desktop` preferred, else `tui`).
        let source = self
            .inner
            .runtime
            .get()
            .and_then(|rt| {
                if rt.session_create.sources.iter().any(|s| s == "desktop") {
                    Some("desktop".to_string())
                } else {
                    rt.session_create.sources.first().cloned()
                }
            })
            .or_else(|| Some("desktop".to_string()));

        self.call::<methods::SessionCreate>(CanonicalCreateParams {
            request_id: Some(Uuid::new_v4().to_string()),
            source,
            cwd,
            title,
            client: Some(hermes_protocol::CanonicalClientIdentity {
                name: "hermes-desktop-rs".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            ..Default::default()
        })
        .await
    }

    pub async fn resume_session(
        &self,
        session_id: impl Into<String>,
    ) -> Result<CanonicalSnapshot, GatewayError> {
        self.call::<methods::SessionResume>(CanonicalResumeParams {
            session_id: Some(session_id.into()),
            display_projection: Some(true),
            ..Default::default()
        })
        .await
    }

    pub async fn submit_prompt(
        &self,
        session_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<CanonicalSubmitResult, GatewayError> {
        self.call::<methods::PromptSubmit>(CanonicalSubmitParams {
            session_id: session_id.into(),
            text: text.into(),
            input_id: Some(Uuid::new_v4().to_string()),
            ..Default::default()
        })
        .await
    }

    pub async fn cancel_prompt(&self, session_id: impl Into<String>) -> Result<Value, GatewayError> {
        let res = self
            .call::<methods::PromptCancel>(PromptCancelParams {
                session_id: session_id.into(),
                ..Default::default()
            })
            .await?;
        Ok(serde_json::to_value(res)?)
    }

    pub async fn events_since(
        &self,
        session_id: impl Into<String>,
        last_sequence: i64,
    ) -> Result<CanonicalEventsResult, GatewayError> {
        self.call::<methods::SessionEventsSince>(CanonicalEventsParams {
            session_id: session_id.into(),
            last_sequence: Some(last_sequence),
            ..Default::default()
        })
        .await
    }

    pub async fn commands_catalog(&self) -> Result<CommandsCatalogResult, GatewayError> {
        self.call::<methods::CommandsCatalog>(CommandsCatalogParams::default())
            .await
    }

    pub async fn complete_slash(
        &self,
        text: impl Into<String>,
    ) -> Result<CompleteSlashResult, GatewayError> {
        self.call::<methods::CompleteSlash>(CompleteSlashParams {
            text: Some(text.into()),
            ..Default::default()
        })
        .await
    }

    pub async fn slash_exec(
        &self,
        session_id: impl Into<String>,
        command: impl Into<String>,
    ) -> Result<SlashExecResult, GatewayError> {
        self.call::<methods::SlashExec>(SlashExecParams {
            session_id: session_id.into(),
            command: command.into(),
            ..Default::default()
        })
        .await
    }

    pub async fn respond_approval(
        &self,
        session_id: impl Into<String>,
        prompt_id: impl Into<String>,
        execution_generation: i64,
        choice: impl Into<String>,
    ) -> Result<PromptResponseResult, GatewayError> {
        self.call::<methods::ApprovalRespond>(hermes_protocol::CanonicalApprovalParams {
            session_id: session_id.into(),
            prompt_id: prompt_id.into(),
            execution_generation,
            choice: choice.into(),
            ..Default::default()
        })
        .await
    }

    pub async fn respond_clarify(
        &self,
        session_id: impl Into<String>,
        prompt_id: impl Into<String>,
        execution_generation: i64,
        answer: impl Into<String>,
    ) -> Result<PromptResponseResult, GatewayError> {
        self.call::<methods::ClarifyRespond>(hermes_protocol::ClarifyRespondParams {
            session_id: session_id.into(),
            prompt_id: prompt_id.into(),
            execution_generation,
            answer: answer.into(),
            ..Default::default()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_protocol::{GatewayNotification, TICKET_PROTOCOL_PREFIX};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    #[tokio::test]
    async fn full_handshake_streaming_and_server_request_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (seen_subprotocols_tx, seen_subprotocols_rx) = oneshot::channel::<String>();
        let (approval_answer_tx, approval_answer_rx) = oneshot::channel::<Value>();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut seen_tx = Some(seen_subprotocols_tx);
            let ws = accept_hdr_async(stream, move |req: &Request, mut resp: Response| {
                if let Some(proto) = req.headers().get(SEC_WEBSOCKET_PROTOCOL) {
                    if let Some(tx) = seen_tx.take() {
                        let _ = tx.send(proto.to_str().unwrap_or_default().to_string());
                    }
                    resp.headers_mut()
                        .insert(SEC_WEBSOCKET_PROTOCOL, CANONICAL_GATEWAY_PROTOCOL.parse().unwrap());
                }
                Ok(resp)
            })
            .await
            .unwrap();

            let (mut sink, mut source) = ws.split();
            let mut approval_tx = Some(approval_answer_tx);

            while let Some(Ok(Message::Text(text))) = source.next().await {
                let frame: Value = serde_json::from_str(&text).unwrap();

                // Server->client request response from the client
                if frame.get("id").and_then(|v| v.as_str()) == Some("srq-1") {
                    if let Some(tx) = approval_tx.take() {
                        let _ = tx.send(frame["result"].clone());
                    }
                    continue;
                }

                let id = frame["id"].clone();
                let method = frame["method"].as_str().unwrap_or_default();
                match method {
                    "client.capabilities" => {
                        let reply = json!({ "id": id, "result": { "server_requests": true } });
                        sink.send(Message::Text(reply.to_string().into())).await.unwrap();
                    }
                    "runtime.describe" => {
                        let reply = json!({
                            "id": id,
                            "result": {
                                "instance_id": "inst-test",
                                "profile_id": "default",
                                "authority_epoch": 1,
                                "capabilities": ["session-authority-v1"],
                                "session_create": {
                                    "sources": ["desktop", "tui"],
                                    "parameters": ["request_id", "source", "cwd", "title", "client"]
                                }
                            }
                        });
                        sink.send(Message::Text(reply.to_string().into())).await.unwrap();
                    }
                    "session.list" => {
                        let reply = json!({
                            "id": id,
                            "result": {
                                "scope": "stored",
                                "sessions": [
                                    { "session_id": "sess-1", "title": "Milestone 0 Test" }
                                ]
                            }
                        });
                        sink.send(Message::Text(reply.to_string().into())).await.unwrap();
                    }
                    "prompt.submit" => {
                        let reply = json!({
                            "id": id,
                            "result": {
                                "admitted": true,
                                "admission_id": "adm-1",
                                "status": "accepted"
                            }
                        });
                        sink.send(Message::Text(reply.to_string().into())).await.unwrap();

                        // Emit a streaming delta, then ask for approval via server->client request.
                        let delta = json!({
                            "method": "event",
                            "params": {
                                "type": "message.delta",
                                "session_id": "sess-1",
                                "seq": 1,
                                "payload": { "text": "Hello from Rust!", "session_id": "sess-1" }
                            }
                        });
                        sink.send(Message::Text(delta.to_string().into())).await.unwrap();

                        let approval_req = json!({
                            "id": "srq-1",
                            "method": "approval",
                            "params": {
                                "session_id": "sess-1",
                                "command": "ls -la",
                                "description": "List directory"
                            }
                        });
                        sink.send(Message::Text(approval_req.to_string().into()))
                            .await
                            .unwrap();
                    }
                    _ => {}
                }
            }
        });

        let target = GatewayDialTarget {
            url: format!("ws://{addr}/api/ws"),
            protocols: vec![
                CANONICAL_GATEWAY_PROTOCOL.to_string(),
                format!("{TICKET_PROTOCOL_PREFIX}tok-abc"),
            ],
            endpoint: None,
            client_code_sha: None,
        };

        let (client, mut server_reqs) = GatewayClient::connect(target).await.unwrap();
        let mut events = client.subscribe_events();

        // 1. Subprotocol carried the single-use ticket and negotiated `hermes-gateway-v1`.
        let header = seen_subprotocols_rx.await.unwrap();
        assert!(header.contains("hermes-gateway-v1"));
        assert!(header.contains("hermes-gateway-ticket.tok-abc"));
        assert_eq!(
            client.handshake().negotiated_subprotocol.as_deref(),
            Some("hermes-gateway-v1")
        );
        assert_eq!(
            client
                .handshake()
                .runtime
                .as_ref()
                .map(|r| r.instance_id.as_str()),
            Some("inst-test")
        );

        // 2. Typed session.list call works.
        let list = client.list_sessions(Some(20), false).await.unwrap();
        assert_eq!(list.sessions.len(), 1);
        assert_eq!(list.sessions[0].title, "Milestone 0 Test");

        // 3. Submit prompt -> receive typed `message.delta`.
        let _ = client.submit_prompt("sess-1", "hi").await.unwrap();
        let ev = events.recv().await.unwrap();
        match ev.typed() {
            GatewayNotification::MessageDelta(d) => {
                assert_eq!(d.text.as_deref(), Some("Hello from Rust!"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // 4. Receive server->client `approval` request and answer it.
        let req = server_reqs.recv().await.unwrap();
        assert_eq!(req.method, "approval");
        req.respond(json!({ "choice": "allow_once" })).unwrap();

        let answer = approval_answer_rx.await.unwrap();
        assert_eq!(answer, json!({ "choice": "allow_once" }));
    }
}
