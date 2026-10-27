//! Same-user local transport for the Postino daemon.
//!
//! This crate is foundation: it knows JSON-RPC 2.0, a length bound, a Windows named pipe and a Unix
//! domain socket, and nothing about mail, the store or egui. The daemon owns the server half; the
//! CLI, the MCP server and (in P10a2) the desktop app own the client half.
//!
//! # Security model
//!
//! - The endpoint is per user and per store. On Windows it is a named pipe whose DACL grants the
//!   current user only; on Unix it is a socket with mode 0600 inside an owner-only directory. There
//!   is no TCP listener and no remote address anywhere in this crate.
//! - A fresh 256-bit token is written by the daemon to an owner-only file at every launch. The
//!   first frame must be `hello` with that token and a matching [`PROTOCOL_VERSION`]. A bad token is
//!   one generic refusal; the expected token is never echoed and never logged.
//! - Every frame is newline-delimited JSON, capped at [`MAX_FRAME_BYTES`] in both directions. A
//!   reader discards an over-long line without allocating it; a writer refuses to produce one.
//! - Queues are bounded. A slow subscriber loses events instead of growing without limit.
//!
//! # Lifecycle
//!
//! [`server::Server`] accepts connections until its [`CancellationToken`] fires. [`client::Client`]
//! is the async client; [`client::BlockingClient`] wraps it in a current-thread runtime for the
//! synchronous CLI and MCP server. A client that loses the connection must report a readable state
//! and reconnect; the daemon never retries on the client's behalf.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod client;
pub mod endpoint;
mod error;
mod events;
mod frame;
mod platform;
pub mod protocol;
pub mod server;
mod token;
mod transport;

pub use endpoint::{DaemonPaths, Endpoint};
pub use error::{IpcError, Result};
pub use events::EventHub;
pub use frame::{FrameReader, MAX_FRAME_BYTES, read_frame, write_frame};
pub use protocol::{
    CancelRequest, EventParams, EventsSubscribe, HelloParams, HelloResult, OpRequest, PROTOCOL_VERSION, RpcError, RpcRequest, error_code,
    is_known_method, method,
};
pub use server::{BoundServer, CancellationToken, DispatchRequest, Dispatcher, Server, ServerConfig, Session};
pub use token::{SharedToken, TokenFile};

#[cfg(windows)]
pub use platform::pipe_acl_is_owner_only;
