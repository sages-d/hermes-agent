//! Async Rust client for the Hermes `hermes-gateway-v1` gateway.

pub mod attach;
pub mod client;

pub use attach::{AttachError, AttachOptions, GatewayDialTarget, GatewayEndpointInfo, attach_local_gateway};
pub use client::{GatewayClient, GatewayError, GatewayHandshake, IncomingServerRequest};
