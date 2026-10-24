//! `racking-mcp`: a Model Context Protocol server over stdio that exposes the
//! Racking API as tools, so an agent in an MCP client (Claude Code) can send and
//! receive jobs without `kb` or `curl`. Stdout carries only protocol messages;
//! logs go to stderr and never contain the token or job text.

mod args;
mod paths;
mod rpc;
mod server;
mod tools;

// Ties the build to `tag-changelog/vX.Y.Z.md` of the version being built, test builds X.Y.Z-N
// included (see rack::release).
const _: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tag-changelog/v",
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR"),
    ".",
    env!("CARGO_PKG_VERSION_PATCH"),
    ".md"
));

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use rack::client::Client;

use crate::paths::Root;
use crate::tools::Ctx;

const USAGE: &str = "\
racking-mcp: MCP server (stdio) exposing a Racking server as tools

Started by an MCP client, not by hand. Environment:
  RACK_URL       server URL, e.g. http://127.0.0.1:7878
  RACK_TOKEN     this agent's token
  RACK_MCP_ROOT  directory that attachments are read from and saved under
                 (default: CLAUDE_PROJECT_DIR, else the working directory)
";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .init();
    // Flags only print and exit; in server mode stdout belongs to the protocol.
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("--version") => {
            println!("racking-mcp {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h") => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(_) => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    }
    let ctx = match config() {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::error!(target: "racking_mcp", error = %e, "cannot start");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(target: "racking_mcp", root = %ctx.root.path().display(), "serving MCP on stdio");
    match server::serve(io::stdin().lock(), io::stdout(), Arc::new(ctx)) {
        // The writer thread is not joined: once the client closes stdin, nobody reads the replies.
        Ok(_writer) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(target: "racking_mcp", error = %e, "cannot start the stdout writer");
            ExitCode::FAILURE
        }
    }
}

fn config() -> Result<Ctx, String> {
    let (url, token) = rack::config::credentials().map_err(|e| tools::describe(&e))?;
    let client = Client::new(&url, &token)
        .map_err(|e| tools::describe(&e))?
        .identified(env!("CARGO_PKG_VERSION"));
    // Claude Code sets CLAUDE_PROJECT_DIR in a stdio server's environment and does
    // not document the working directory it starts the server in.
    let root_dir = match ["RACK_MCP_ROOT", "CLAUDE_PROJECT_DIR"]
        .into_iter()
        .find_map(|name| std::env::var_os(name).filter(|v| !v.is_empty()))
    {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir().map_err(|e| format!("cannot read the working directory: {e}"))?,
    };
    let root = Root::new(&root_dir).map_err(|e| format!("root directory {}: {e}", root_dir.display()))?;
    Ok(Ctx { client, root })
}
