//! Responses API WebSocket transport.
//!
//! The module separates WebSocket pooling from single-connection frame handling:
//! [`pool`] owns idle leases, warmup, close, reconnect, and handshake rate
//! limiting, while [`connection`] owns frame I/O for one socket. The public pool
//! contract is deterministic for the current harness requirements:
//!
//! - one [`ResponsesWsPool`] is scoped to one provider, auth source, and default
//!   Codex identity header set;
//! - request-level [`harness_responses_api::CodexHeaders`] do not partition idle sockets;
//! - idle sockets are leased in FIFO order and returned to the back of the idle
//!   queue;
//! - an idle queue at capacity closes the returned connection instead of
//!   evicting an older socket;
//! - new handshakes are reserved one at a time by the pool rate window.
//!
//! Sockets are intentionally not durable. Callers store only scheduling metadata
//! for future opens; this module owns only live transport state.

mod connection;
mod pool;

#[cfg(test)]
pub(crate) use connection::ConnectionContext;
pub use pool::{ResponsesWsPool, WsPoolConfig};
