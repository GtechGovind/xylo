//! Lazy MCP sessions and native API adapters. Policy and durable action journaling
//! belong to the application tool registry; all native writes are labelled as such.
pub mod ide;
mod mcp;
mod native;
mod oauth;
pub use mcp::*;
pub use native::*;
pub use oauth::*;
