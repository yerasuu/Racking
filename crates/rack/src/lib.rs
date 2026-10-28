//! The Racking HTTP client as a library, so `racking-mcp` reuses it instead of
//! keeping a second copy; the `rack` binary is built on the same module.

pub mod ask;
pub mod client;
pub mod config;
pub mod guide;
pub mod history;
pub mod needs;
pub mod policy;
pub mod release;
pub mod text;
pub mod upload;
