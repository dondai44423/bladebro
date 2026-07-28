//! The MCP server module — exposes Bladebro's tools over stdio JSON-RPC.

pub mod server;
pub mod tools;

pub use server::{run, run_pipe};
