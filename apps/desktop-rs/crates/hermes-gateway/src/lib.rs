//! Async Rust client for the Hermes `hermes-gateway-v1` gateway.

pub mod attach;
pub mod client;
pub mod reconnect;

pub use attach::{AttachError, AttachOptions, GatewayDialTarget, GatewayEndpointInfo, attach_local_gateway};
pub use client::{
    BOT_CHAT_TITLE, DEFAULT_TRANSCRIPT_PAGE_LIMIT, GatewayClient, GatewayError, GatewayHandshake,
    IncomingServerRequest, ReplayOutcome, SessionLoadResult,
};
pub use reconnect::ReconnectPolicy;
