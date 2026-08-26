//! Library crate for termshot.
//!
//! The modules are exposed as a library (in addition to the `termshot` binary)
//! so integration tests can drive the MCP tool handlers and rendering pipeline
//! directly.

pub mod capture;
pub mod config;
pub mod executor;
pub mod redaction;
pub mod renderer;
pub mod server;
