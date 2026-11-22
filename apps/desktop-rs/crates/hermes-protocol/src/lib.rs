//! Strongly-typed wire protocol for the Hermes `hermes-gateway-v1` JSON-RPC gateway.
//!
//! Types in [`generated`] are mechanically derived from
//! `apps/shared/src/gateway-contract.openrpc.json` via `apps/desktop-rs/scripts/gen_protocol.py`.

pub mod generated;
pub mod wire;

pub use generated::*;
pub use wire::*;

/// Canonical WebSocket subprotocol negotiated with `tui_gateway` / session authority.
pub const CANONICAL_GATEWAY_PROTOCOL: &str = "hermes-gateway-v1";

/// Prefix for single-use WebSocket ticket subprotocols (`hermes-gateway-ticket.<token>`).
pub const TICKET_PROTOCOL_PREFIX: &str = "hermes-gateway-ticket.";

/// Extract human-readable display text from a `TranscriptMessage`'s `content` field.
///
/// Mirrors `apps/shared/src/gateway-canonical.ts::rowText` for multipart content arrays.
pub fn transcript_content_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                if let Some(s) = part.as_str() {
                    return Some(s.to_string());
                }
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    return Some(text.to_string());
                }
                if let Some(ty) = part.get("type").and_then(|v| v.as_str()) {
                    return Some(if ty.contains("image") {
                        "[image]".to_string()
                    } else {
                        format!("[{ty}]")
                    });
                }
                None
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_event_notification_into_typed_delta() {
        let raw: RawInboundFrame = serde_json::from_value(json!({
            "method": "event",
            "params": {
                "type": "message.delta",
                "session_id": "sess-1",
                "seq": 42,
                "payload": {
                    "text": "hello ",
                    "session_id": "sess-1"
                }
            }
        }))
        .unwrap();

        let Some(InboundFrame::Event(env)) = raw.classify() else {
            panic!("expected event frame");
        };
        assert_eq!(env.session_id.as_deref(), Some("sess-1"));
        assert_eq!(env.seq, Some(42));
        match env.typed() {
            GatewayNotification::MessageDelta(delta) => {
                assert_eq!(delta.text.as_deref(), Some("hello "));
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn classifies_server_request_approval() {
        let raw: RawInboundFrame = serde_json::from_value(json!({
            "id": "srq-7",
            "method": "approval",
            "params": {
                "session_id": "sess-1",
                "command": "rm -rf /tmp/foo",
                "description": "Delete temporary folder"
            }
        }))
        .unwrap();

        let Some(InboundFrame::ServerRequest {
            id,
            method,
            parsed,
            ..
        }) = raw.classify()
        else {
            panic!("expected server request");
        };
        assert_eq!(id, RequestId::Str("srq-7".to_string()));
        assert_eq!(method, "approval");
        assert!(matches!(parsed, ServerRequestPayload::Approval(_)));
    }

    #[test]
    fn extracts_multipart_transcript_text() {
        let content = json!([
            {"type": "text", "text": "Look at this"},
            {"type": "image_url", "url": "data:image/png;base64,..."}
        ]);
        assert_eq!(transcript_content_text(&content), "Look at this\n[image]");
    }
}
