//! LocalHarness: a local-first agent harness for coding and security work.
//!
//! The library exposes the harness core for embedding and forking. The binary
//! in `main.rs` is a thin wrapper.

pub mod agent;
pub mod app;
pub mod cli;
pub mod config;
pub mod context;
pub mod external;
pub mod paths;
pub mod provider;
pub mod security;
pub mod session;
pub mod skills;
pub mod tools;
pub mod ui;

pub use agent::{Core, run_turn};
pub use config::Config;
pub use paths::Paths;
pub use provider::{OpenAiCompatProvider, Provider};
pub use session::Session;
pub use tools::{Tool, ToolContext, ToolRegistry, ToolResult};
