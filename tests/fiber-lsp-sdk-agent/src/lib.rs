//! Test-only hosted LSP client and auto-approving remote signer.
//!
//! This binary exercises the real `fiber-lsp-sdk` across an HTTP and process
//! boundary. It is an E2E fixture, not a production signing policy.

mod agent;
mod convert;
mod rpc;
mod store;

pub use agent::{Agent, AgentConfig, AgentStatus};
pub use rpc::{FiberRpc, HttpFiberRpc};
pub use store::FileSignerStore;
