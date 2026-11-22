//! Local gateway discovery and single-use ticket attachment via `hermes gateway attach --json`.
//!
//! Mirrors `hermes_cli/gateway_runtime_cli.py::cmd_gateway_attach` and
//! `apps/shared/src/node/gateway-client.ts::GatewayConnection`.
//! Tickets are credentials for a single WebSocket dial and are redacted from all Debug/Display output.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use hermes_protocol::{CANONICAL_GATEWAY_PROTOCOL, TICKET_PROTOCOL_PREFIX};
use serde::Deserialize;
use thiserror::Error;
use tokio::process::Command;

/// Error returned while attaching to a local or remote Hermes gateway.
#[derive(Debug, Error)]
pub enum AttachError {
    #[error("failed to invoke `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`hermes gateway attach` timed out after {0:?}")]
    Timeout(Duration),
    #[error("invalid JSON from `hermes gateway attach`: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("gateway not attachable ({state}: {reason}): {message}")]
    Refused {
        state: String,
        reason: String,
        message: String,
    },
    #[error("gateway attach returned no WebSocket URL")]
    MissingUrl,
}

/// Options for invoking `hermes gateway attach --json`.
#[derive(Debug, Clone)]
pub struct AttachOptions {
    /// Executable name or path for the `hermes` CLI (defaults to `"hermes"`).
    pub hermes_bin: String,
    /// Optional profile name (`-p <profile>`).
    pub profile: Option<String>,
    /// Optional `HERMES_HOME` override.
    pub hermes_home: Option<PathBuf>,
    /// Ensure/start the local gateway owner if not yet running (`--start`).
    pub start: bool,
    /// Bounded recovery of a stopped/crashed owner (`--recover`).
    pub recover: bool,
    /// Timeout passed to `hermes gateway attach --timeout`.
    pub timeout: Duration,
}

impl Default for AttachOptions {
    fn default() -> Self {
        Self {
            hermes_bin: std::env::var("HERMES_BIN").unwrap_or_else(|_| "hermes".to_string()),
            profile: None,
            hermes_home: None,
            start: true,
            recover: false,
            timeout: Duration::from_secs(20),
        }
    }
}

/// Raw JSON output of `hermes gateway attach --json`.
#[derive(Deserialize)]
struct RawAttachPayload {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    reason_code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    protocols: Vec<String>,
    #[serde(default)]
    endpoint: Option<RawEndpoint>,
    #[serde(default)]
    client_code_sha: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayEndpointInfo {
    #[serde(default)]
    pub profile_id: String,
    #[serde(default)]
    pub instance_id: String,
    #[serde(default)]
    pub authority_epoch: u64,
    #[serde(default)]
    pub api_origin: Option<String>,
    #[serde(default)]
    pub code_sha: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

type RawEndpoint = GatewayEndpointInfo;

/// One single-use attach grant ready for WebSocket dialing.
///
/// `Debug` redacts any `hermes-gateway-ticket.*` subprotocol entry so credentials
/// can never leak into logs or crash dumps.
#[derive(Clone)]
pub struct GatewayDialTarget {
    pub url: String,
    pub protocols: Vec<String>,
    pub endpoint: Option<GatewayEndpointInfo>,
    pub client_code_sha: Option<String>,
}

impl GatewayDialTarget {
    /// Construct a dial target for an explicit WebSocket URL (e.g., `--url ws://127.0.0.1:9119/api/ws`).
    pub fn from_url(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            protocols: vec![CANONICAL_GATEWAY_PROTOCOL.to_string()],
            endpoint: None,
            client_code_sha: None,
        }
    }

    /// Return protocols with ticket values redacted for safe logging.
    pub fn redacted_protocols(&self) -> Vec<String> {
        self.protocols
            .iter()
            .map(|p| {
                if p.starts_with(TICKET_PROTOCOL_PREFIX) {
                    format!("{TICKET_PROTOCOL_PREFIX}***")
                } else {
                    p.clone()
                }
            })
            .collect()
    }

    /// Return the URL with query parameters and userinfo stripped for safe logging.
    pub fn redacted_url(&self) -> String {
        match url::Url::parse(&self.url) {
            Ok(mut parsed) => {
                let had_query = parsed.query().is_some();
                parsed.set_query(None);
                if !parsed.username().is_empty() || parsed.password().is_some() {
                    let _ = parsed.set_username("***");
                    let _ = parsed.set_password(None);
                }
                if had_query {
                    format!("{parsed}?***")
                } else {
                    parsed.to_string()
                }
            }
            Err(_) => self.url.split('?').next().unwrap_or(&self.url).to_string(),
        }
    }
}

impl fmt::Debug for GatewayDialTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayDialTarget")
            .field("url", &self.redacted_url())
            .field("protocols", &self.redacted_protocols())
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

/// Run `hermes gateway attach --json` and return a single-use dial target.
pub async fn attach_local_gateway(opts: &AttachOptions) -> Result<GatewayDialTarget, AttachError> {
    let mut cmd = Command::new(&opts.hermes_bin);
    if let Some(profile) = &opts.profile {
        cmd.arg("-p").arg(profile);
    }
    cmd.args(["gateway", "attach", "--json"]);
    if opts.start {
        cmd.arg("--start");
    }
    if opts.recover {
        cmd.arg("--recover");
    }
    cmd.arg("--timeout")
        .arg(format!("{:.1}", opts.timeout.as_secs_f64()));

    if let Some(home) = &opts.hermes_home {
        cmd.env("HERMES_HOME", home);
    }

    let wait_budget = opts.timeout + Duration::from_secs(5);
    let output = tokio::time::timeout(wait_budget, cmd.output())
        .await
        .map_err(|_| AttachError::Timeout(wait_budget))?
        .map_err(|source| AttachError::Spawn {
            program: opts.hermes_bin.clone(),
            source,
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let raw: RawAttachPayload = serde_json::from_str(stdout.trim())?;

    if !output.status.success() || raw.url.is_none() {
        return Err(AttachError::Refused {
            state: raw.state.unwrap_or_else(|| "unknown".to_string()),
            reason: raw.reason_code.unwrap_or_else(|| "attach_failed".to_string()),
            message: raw
                .message
                .unwrap_or_else(|| String::from_utf8_lossy(&output.stderr).trim().to_string()),
        });
    }

    let url = raw.url.ok_or(AttachError::MissingUrl)?;
    let protocols = if raw.protocols.is_empty() {
        vec![CANONICAL_GATEWAY_PROTOCOL.to_string()]
    } else {
        raw.protocols
    };

    Ok(GatewayDialTarget {
        url,
        protocols,
        endpoint: raw.endpoint,
        client_code_sha: raw.client_code_sha,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_ticket_and_query_tokens() {
        let target = GatewayDialTarget {
            url: "ws://127.0.0.1:9119/api/ws?token=supersecret".to_string(),
            protocols: vec![
                CANONICAL_GATEWAY_PROTOCOL.to_string(),
                format!("{TICKET_PROTOCOL_PREFIX}secret-ticket-123"),
            ],
            endpoint: None,
            client_code_sha: None,
        };
        let rendered = format!("{target:?}");
        assert!(!rendered.contains("supersecret"), "query token leaked: {rendered}");
        assert!(!rendered.contains("secret-ticket-123"), "ticket leaked: {rendered}");
        assert!(rendered.contains("hermes-gateway-ticket.***"));
    }
}
