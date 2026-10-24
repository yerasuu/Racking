//! Racking service: an inbox per agent for jobs, attachments and board links,
//! served as HTTP/JSON. Blocking I/O with a thread per request, which is enough
//! for a handful of agents and keeps the binary free of an async runtime.

mod api;
mod clock;
pub mod config;
mod error;
mod store;
mod types;

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread::JoinHandle;

pub use api::guide;
pub use config::Config;

/// A started server. Dropping it leaves the server running; call `stop` to end it.
pub struct Running {
    server: Arc<tiny_http::Server>,
    addr: SocketAddr,
    acceptor: JoinHandle<()>,
}

impl Running {
    /// The bound address, useful when the config asked for port 0.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Blocks until the server stops accepting requests.
    pub fn join(self) {
        if self.acceptor.join().is_err() {
            tracing::error!(target: "racking::server", "acceptor thread panicked");
        }
    }

    /// Stops accepting requests; requests already being served finish on their own threads.
    pub fn stop(self) {
        self.server.unblock();
        self.join();
    }
}

/// Opens the store and starts serving on `config.bind`.
///
/// # Errors
///
/// When the data directory or database cannot be opened, or the address cannot be bound.
pub fn start(config: &Config) -> Result<Running, String> {
    let store = store::Store::open(&config.data_dir)?;
    let app = Arc::new(api::App::new(config, store));
    let server = tiny_http::Server::http(config.bind).map_err(|e| format!("cannot listen on {}: {e}", config.bind))?;
    let addr = server
        .server_addr()
        .to_ip()
        .ok_or("server is not bound to an IP address")?;
    let server = Arc::new(server);
    let acceptor = {
        let server = Arc::clone(&server);
        std::thread::Builder::new()
            .name("accept".to_owned())
            .spawn(move || accept_loop(&server, &app))
            .map_err(|e| format!("cannot start the acceptor thread: {e}"))?
    };
    Ok(Running { server, addr, acceptor })
}

fn accept_loop(server: &tiny_http::Server, app: &Arc<api::App>) {
    // `recv` fails only once `unblock` was called or the listener died; either way serving is over.
    while let Ok(request) = server.recv() {
        let app = Arc::clone(app);
        let spawned = std::thread::Builder::new()
            .name("request".to_owned())
            .spawn(move || api::handle(&app, request));
        if let Err(e) = spawned {
            tracing::error!(target: "racking::server", error = %e, "cannot start a request thread; request dropped");
        }
    }
}
