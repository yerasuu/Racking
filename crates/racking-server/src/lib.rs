//! Racking service: an inbox per agent for jobs, attachments and board links,
//! served as HTTP/JSON. Blocking I/O with a thread per request, which is enough
//! for a handful of agents and keeps the binary free of an async runtime.

mod accounts;
mod api;
mod clock;
pub mod config;
mod dist;
mod error;
mod invite;
mod net;
mod pairing;
mod policy;
mod presence;
mod rate;
mod registry;
pub mod release;
mod store;
mod types;

use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub use api::guide;
pub use clock::Clock;
pub use config::Config;
pub use pairing::{Bootstrap, Redeemed, Waited};

/// A started server. Dropping it leaves the server running; call `stop` or
/// `shutdown` to end it.
pub struct Running {
    server: Arc<tiny_http::Server>,
    app: Arc<api::App>,
    addr: SocketAddr,
    grace: Duration,
    acceptor: JoinHandle<()>,
}

/// Starts a shutdown from another thread, such as the one that waits for signals.
/// It holds the listener weakly, so that dropping `Running` still closes it.
#[derive(Clone)]
pub struct Shutdown {
    server: Weak<tiny_http::Server>,
    app: Arc<api::App>,
}

impl Shutdown {
    /// Stops taking requests and wakes every long poll so it answers now.
    /// `Running::shutdown` does the rest. Calling it again does nothing more.
    pub fn begin(&self) {
        tracing::info!(target: "racking::server", "shutting down: no new requests, long polls answer now");
        self.app.close();
        if let Some(server) = self.server.upgrade() {
            server.unblock();
        }
    }
}

/// How a shutdown ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct Stopped {
    /// Requests still running when the grace period ran out; they are cut off at exit.
    pub abandoned: usize,
    /// Whether the WAL was moved into the database and emptied.
    pub checkpointed: bool,
}

impl Running {
    /// The bound address, useful when the config asked for port 0.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown_handle(&self) -> Shutdown {
        Shutdown {
            server: Arc::downgrade(&self.server),
            app: Arc::clone(&self.app),
        }
    }

    /// Stops accepting requests; requests already being served finish on their own threads.
    pub fn stop(self) {
        self.shutdown_handle().begin();
        Self::join(self.acceptor);
    }

    /// Blocks until `Shutdown::begin` (or a dead listener) ends the acceptor, then
    /// closes the listener and the connections' read side, lets the requests in
    /// flight finish for up to the config's `shutdown_grace_secs`, and checkpoints
    /// the database. The process should exit right after: requests still running
    /// then, such as a long upload, are cut off and not resumed.
    pub fn shutdown(self) -> Stopped {
        let Self {
            server,
            app,
            grace,
            acceptor,
            ..
        } = self;
        Self::join(acceptor);
        // Again, in case the listener died on its own and nobody called `begin`.
        app.close();
        // Dropping the last strong reference closes the listening socket and stops
        // tiny_http from reading further requests on kept-alive connections.
        drop(server);
        let deadline = Instant::now() + grace;
        let mut waiting = app.in_flight();
        if waiting > 0 {
            tracing::info!(target: "racking::server", requests = waiting, grace_secs = grace.as_secs(), "waiting for requests in flight");
        }
        while waiting > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            waiting = app.in_flight();
        }
        if waiting > 0 {
            tracing::warn!(target: "racking::server", requests = waiting, "grace period over; cutting off the requests still running");
        } else {
            tracing::info!(target: "racking::server", "no requests in flight");
        }
        let checkpointed = match app.store.checkpoint() {
            Ok((false, pages)) => {
                tracing::info!(target: "racking::server", pages, "database checkpointed, WAL emptied");
                true
            }
            Ok((true, pages)) => {
                tracing::warn!(target: "racking::server", pages, "database checkpoint blocked; the WAL is kept and replayed at the next start");
                false
            }
            Err(e) => {
                tracing::error!(target: "racking::server", error = %e, "database checkpoint failed; the WAL is replayed at the next start");
                false
            }
        };
        Stopped {
            abandoned: waiting,
            checkpointed,
        }
    }

    fn join(acceptor: JoinHandle<()>) {
        if acceptor.join().is_err() {
            tracing::error!(target: "racking::server", "acceptor thread panicked");
        }
    }
}

/// Opens the store and starts serving on `config.bind`.
///
/// # Errors
///
/// When the data directory or database cannot be opened, or the address cannot be bound.
pub fn start(config: &Config) -> Result<Running, String> {
    start_with_clock(config, Clock::system())
}

/// Like `start`, with the clock presence (`GET /agents`) reads; tests pass a manual one.
///
/// # Errors
///
/// As `start`.
pub fn start_with_clock(config: &Config, clock: Clock) -> Result<Running, String> {
    let store = store::Store::open(&config.data_dir)?
        .with_open_task_limit(config.limits.open_tasks_per_pair)
        .with_upload_rules(store::UploadRules::new(&config.limits, &config.quota));
    let dist = dist::Dist::open(&config.data_dir)?;
    let app = Arc::new(api::App::new(config, store, dist, clock)?);
    if config.public_bind {
        tracing::warn!(
            target: "racking::server",
            bind = %config.bind,
            "BINDING A PUBLIC ADDRESS (allow_public_bind = true): Racking has no TLS and is meant for a LAN; \
             only allow_from keeps other networks out"
        );
    }
    let server = tiny_http::Server::http(config.bind).map_err(|e| format!("cannot listen on {}: {e}", config.bind))?;
    let addr = server
        .server_addr()
        .to_ip()
        .ok_or("server is not bound to an IP address")?;
    let server = Arc::new(server);
    let acceptor = {
        let server = Arc::clone(&server);
        let app = Arc::clone(&app);
        std::thread::Builder::new()
            .name("accept".to_owned())
            .spawn(move || accept_loop(&server, &app))
            .map_err(|e| format!("cannot start the acceptor thread: {e}"))?
    };
    Ok(Running {
        server,
        app,
        addr,
        grace: config.shutdown_grace,
        acceptor,
    })
}

fn accept_loop(server: &tiny_http::Server, app: &Arc<api::App>) {
    // `recv` fails only once `unblock` was called or the listener died; either way serving is over.
    while let Ok(request) = server.recv() {
        let in_flight = app.enter();
        let worker = Arc::clone(app);
        let spawned = std::thread::Builder::new()
            .name("request".to_owned())
            .spawn(move || api::handle(&worker, request, in_flight));
        if let Err(e) = spawned {
            app.leave();
            tracing::error!(target: "racking::server", error = %e, "cannot start a request thread; request dropped");
        }
    }
    tracing::info!(target: "racking::server", "stopped accepting requests");
}
