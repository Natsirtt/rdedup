//! Shared startup boundary for the standalone server and the rdedup subcommand.
use crate::configuration::Configuration;
use crate::server::Server;
use std::path::PathBuf;

/// Server options; repository settings are resolved from file and environment.
#[derive(Debug, clap::Parser)]
#[clap(
    name = "rdedup-server",
    about = "Serve an rdedup repository over plain HTTP"
)]
pub struct Arguments {
    /// TOML settings file; explicit file values override environment values
    #[clap(long = "config", value_name = "FILE")]
    configuration: Option<PathBuf>,
}

/// Startup failure before the server can accept repository operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid or unreadable settings.
    #[error(transparent)]
    Configuration(#[from] crate::configuration::Error),
    /// The asynchronous runtime could not be created.
    #[error("could not create server runtime: {0}")]
    Runtime(#[source] std::io::Error),
    /// Shutdown signal registration failed.
    #[error("could not register server shutdown signals: {0}")]
    Signal(#[source] std::io::Error),
    /// Repository validation, binding or serving failed.
    #[error(transparent)]
    Server(#[from] crate::server::Error),
}

impl Arguments {
    /// Resolve settings and serve until a process shutdown signal arrives.
    ///
    /// # Errors
    /// Returns configuration, runtime, signal registration or server failures.
    pub fn run(self) -> Result<(), Error> {
        let configuration = Configuration::load(self.configuration.as_deref())?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(Error::Runtime)?;
        runtime.block_on(async {
            let shutdown = Shutdown::new().map_err(Error::Signal)?;
            let server = Server::bind(configuration).await?;
            eprintln!(
                "Serving plain HTTP on {}; expose only behind a trusted TLS reverse proxy",
                server.address()
            );
            server.run(shutdown.wait()).await?;
            Ok(())
        })
    }
}

#[cfg(unix)]
struct Shutdown {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Shutdown {
    fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn wait(mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}

#[cfg(windows)]
struct Shutdown {
    interrupt: tokio::signal::windows::CtrlC,
    terminate: tokio::signal::windows::CtrlBreak,
}

#[cfg(windows)]
impl Shutdown {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::windows::ctrl_c()?,
            terminate: tokio::signal::windows::ctrl_break()?,
        })
    }

    async fn wait(mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}
