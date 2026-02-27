//! Agent Protocol — shared types for NoBase agent ↔ gateway ↔ backend communication.
//!
//! All messages use JSON-RPC 2.0 envelopes over WebSocket.
//! This crate is compiled into both the agent and gateway binaries.

pub mod methods;
pub mod metrics;
pub mod rpc;
pub mod stream;

pub use methods::*;
pub use metrics::*;
pub use rpc::*;
pub use stream::*;
