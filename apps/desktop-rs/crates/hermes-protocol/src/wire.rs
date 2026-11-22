//! JSON-RPC 2.0 wire framing for `hermes-gateway-v1`.
//!
//! Mirrors `apps/shared/src/json-rpc-channel.ts` and `apps/shared/src/gateway-events.ts`:
//! - Client -> Server calls: `{"jsonrpc":"2.0","id":<id>,"method":"...","params":{...}}`
//! - Server -> Client responses: `{"id":<id>,"result":...}` or `{"id":<id>,"error":{...}}`
//! - Server -> Client events: `{"method":"event","params":{"type":"...","session_id":"...","seq":1,"payload":{...}}}`
//! - Server -> Client requests: `{"id":"srq-1","method":"approval","params":{...}}`

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::generated::{GatewayNotification, ServerRequestPayload};

/// Standard JSON-RPC "method not found" code (`tui_gateway/server.py`).
pub const JSON_RPC_METHOD_NOT_FOUND: i64 = -32601;
/// Standard JSON-RPC "internal error" code.
pub const JSON_RPC_INTERNAL_ERROR: i64 = -32603;
/// Window-owned request whose session is not shown by any window (`NOT_SHOWN_CODE`).
pub const JSON_RPC_SESSION_NOT_SHOWN: i64 = 4404;

/// JSON-RPC request identifier (numeric or string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Num(u64),
    Str(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Num(n) => write!(f, "{n}"),
            Self::Str(s) => f.write_str(s),
        }
    }
}

/// Outbound JSON-RPC request frame.
#[derive(Debug, Clone, Serialize)]
pub struct OutboundRequest<'a, P: Serialize> {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<P>,
}

impl<'a, P: Serialize> OutboundRequest<'a, P> {
    pub fn new(id: RequestId, method: &'a str, params: Option<P>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// Outbound JSON-RPC response frame (used when answering server->client requests).
#[derive(Debug, Clone, Serialize)]
pub struct OutboundResponse {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorPayload>,
}

impl OutboundResponse {
    pub fn ok(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: RequestId, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcErrorPayload {
                code: Some(code),
                message: Some(message.into()),
                data: None,
            }),
        }
    }
}

/// Structured JSON-RPC error payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorPayload {
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
}

/// Raw inbound JSON-RPC frame before classification.
#[derive(Debug, Clone, Deserialize)]
pub struct RawInboundFrame {
    #[serde(default)]
    pub id: Option<RequestId>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcErrorPayload>,
}

/// Envelope carried in `params` of a `{"method": "event", "params": ...}` frame.
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayEventEnvelope {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub seq: Option<u64>,
    #[serde(default)]
    pub replay_epoch: Option<String>,
    #[serde(default)]
    pub authority_epoch: Option<u64>,
    #[serde(default)]
    pub execution_generation: Option<u64>,
    #[serde(default)]
    pub admission_id: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub replayed: bool,
    #[serde(default)]
    pub payload: Value,
}

impl GatewayEventEnvelope {
    /// Parse `payload` into the strongly-typed generated notification enum.
    /// Falls back to `GatewayNotification::Unknown` if the schema doesn't match strictly.
    pub fn typed(&self) -> GatewayNotification {
        let payload = if self.payload.is_null() {
            Value::Object(serde_json::Map::new())
        } else {
            self.payload.clone()
        };
        GatewayNotification::parse(&self.event_type, payload).unwrap_or_else(|_| {
            GatewayNotification::Unknown {
                event_type: self.event_type.clone(),
                payload: self.payload.clone(),
            }
        })
    }
}

/// Classified inbound frame off the WebSocket transport.
#[derive(Debug, Clone)]
pub enum InboundFrame {
    /// Response to an outbound client->server RPC call.
    Response {
        id: RequestId,
        result: Result<Value, JsonRpcErrorPayload>,
    },
    /// Server->client event notification (`method == "event"`).
    Event(GatewayEventEnvelope),
    /// Server->client interactive request (`approval`, `clarify`, `sudo`, `secret`, ...).
    ServerRequest {
        id: RequestId,
        method: String,
        params: Value,
        parsed: ServerRequestPayload,
    },
    /// Error frame without a correlating request id (e.g., `-32700` parse error).
    UncorrelatedError(JsonRpcErrorPayload),
}

impl RawInboundFrame {
    /// Classify a raw frame according to `apps/shared/src/json-rpc-channel.ts`.
    pub fn classify(self) -> Option<InboundFrame> {
        // Server->client interactive request: string id + method != "event"
        if let (Some(id @ RequestId::Str(_)), Some(method)) = (&self.id, &self.method)
            && method != "event"
        {
            let params = self.params.unwrap_or_else(|| Value::Object(Default::default()));
            let parsed = ServerRequestPayload::parse(method, params.clone()).unwrap_or_else(|_| {
                ServerRequestPayload::Unknown {
                    method: method.clone(),
                    params: params.clone(),
                }
            });
            return Some(InboundFrame::ServerRequest {
                id: id.clone(),
                method: method.clone(),
                params,
                parsed,
            });
        }

        // Server->client event notification
        if self.method.as_deref() == Some("event") {
            let params = self.params?;
            let env: GatewayEventEnvelope = serde_json::from_value(params).ok()?;
            return Some(InboundFrame::Event(env));
        }

        // Correlated RPC response
        if let Some(id) = self.id {
            if let Some(err) = self.error {
                return Some(InboundFrame::Response {
                    id,
                    result: Err(err),
                });
            }
            return Some(InboundFrame::Response {
                id,
                result: Ok(self.result.unwrap_or(Value::Null)),
            });
        }

        // Uncorrelated error
        if let Some(err) = self.error {
            return Some(InboundFrame::UncorrelatedError(err));
        }

        None
    }
}
