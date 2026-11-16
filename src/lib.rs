//! LocalHarness: a local-first agent harness for coding and security work.
//!
//! The library exposes the harness core for embedding and forking. The binary
//! in `main.rs` is a thin wrapper.

pub mod acp;
pub mod agent;
pub mod app;
pub mod cli;
pub mod config;
pub mod context;
pub mod doctor;
pub mod events;
pub mod external;
pub mod git;
pub mod hooks;
pub mod host;
pub mod html_text;
pub mod mcp;
pub mod media;
pub mod paths;
pub mod policy;
pub mod provenance;
pub mod provider;
pub mod sandbox;
pub mod sanitize;
pub mod security;
pub mod serve;
pub mod session;
pub mod skills;
pub mod text;
pub mod tools;
pub mod ui;
pub mod verify_setup;
pub mod workflows;

pub use agent::{Core, run_turn};
pub use config::Config;
pub use paths::Paths;
pub use provider::{OpenAiCompatProvider, Provider};
pub use session::Session;
pub use tools::{Tool, ToolContext, ToolRegistry, ToolResult};
