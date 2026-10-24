//! The Racking HTTP client as a library, so `racking-mcp` reuses it instead of
//! keeping a second copy; the `rack` binary is built on the same module.

pub mod client;
pub mod text;
