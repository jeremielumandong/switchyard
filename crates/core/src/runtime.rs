//! The runtime bridge: a multi-thread tokio runtime owned by core, a command channel in and
//! an event channel out. The UI thread never does I/O; it sends [`Command`]s and awaits
//! [`Event`]s.

use std::future::Future;
use std::sync::Arc;

use futures::channel::mpsc as fmpsc;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tracing::error;

use crate::bus::{Command, Event};
use crate::error::{CoreError, Result};
use crate::service::{Service, ServiceConfig};

/// Receiver of [`Event`]s. Executor-agnostic, so a GPUI task can await it directly.
pub type EventReceiver = fmpsc::UnboundedReceiver<Event>;

/// Sends events to the UI.
#[derive(Clone)]
pub struct EventSender(fmpsc::UnboundedSender<Event>);

impl EventSender {
    /// A sender over `tx`.
    pub(crate) fn new(tx: fmpsc::UnboundedSender<Event>) -> Self {
        Self(tx)
    }

    /// Emit an event (dropped silently if the UI is gone).
    pub fn emit(&self, event: Event) {
        let _ = self.0.unbounded_send(event);
    }
}

/// A cheap, clonable handle to the runtime.
#[derive(Clone)]
pub struct RuntimeHandle {
    commands: mpsc::UnboundedSender<Command>,
    handle: tokio::runtime::Handle,
    secrets: Arc<dyn switchyard_store::SecretStore>,
}

impl RuntimeHandle {
    /// Send a command. Never blocks.
    pub fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            error!("runtime is gone; command dropped");
        }
    }

    /// Spawn arbitrary work on the runtime.
    pub fn spawn<F>(&self, fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle.spawn(fut)
    }

    /// Run blocking work (disk, synchronous network) on the runtime's blocking pool. The
    /// UI awaits the returned handle from a GPUI task; it never blocks the UI thread.
    pub fn spawn_blocking<F, T>(&self, f: F) -> tokio::task::JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.handle.spawn_blocking(f)
    }

    /// The API workspace's secret store (the app's keychain or vault).
    pub fn api_secrets(&self) -> Arc<dyn switchyard_api::SecretStore> {
        Arc::new(crate::api_secrets::ApiSecrets::new(self.secrets.clone()))
    }

    /// The tokio runtime handle (for libraries that drive async work from blocking code).
    pub fn tokio(&self) -> tokio::runtime::Handle {
        self.handle.clone()
    }
}

/// Owns the tokio runtime. Drop it to shut everything down.
pub struct Core {
    runtime: Option<Runtime>,
    handle: RuntimeHandle,
}

impl Core {
    /// Start the runtime and the core service.
    pub fn start(config: ServiceConfig) -> Result<(Self, EventReceiver)> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("switchyard-io")
            .enable_all()
            .build()
            .map_err(|e| CoreError::Startup(e.to_string()))?;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = fmpsc::unbounded();
        let events = EventSender::new(ev_tx);
        let service = {
            let _guard = runtime.enter();
            Service::new(config, events.clone())?
        };
        let service = Arc::new(service);
        let secrets = service.secret_backend();
        runtime.spawn(Service::run(service, cmd_rx));
        let handle = RuntimeHandle {
            commands: cmd_tx,
            handle: runtime.handle().clone(),
            secrets,
        };
        Ok((
            Self {
                runtime: Some(runtime),
                handle,
            },
            ev_rx,
        ))
    }

    /// The handle to send commands with.
    pub fn handle(&self) -> RuntimeHandle {
        self.handle.clone()
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use futures::StreamExt;

    use super::*;

    #[test]
    fn slow_command_does_not_block_sender() {
        let (core, mut events) = Core::start(ServiceConfig::in_memory()).unwrap();
        let h = core.handle();
        let started = Instant::now();
        h.send(Command::Ping {
            id: 1,
            delay: Duration::from_secs(2),
        });
        h.send(Command::Ping {
            id: 2,
            delay: Duration::from_millis(10),
        });
        // Sending returned immediately.
        assert!(started.elapsed() < Duration::from_millis(100));
        let order: Vec<u64> = futures::executor::block_on(async {
            let mut ids = Vec::new();
            while ids.len() < 2 {
                if let Some(Event::Pong { id }) = events.next().await {
                    ids.push(id);
                }
            }
            ids
        });
        // The fast command finished first: commands run concurrently.
        assert_eq!(order, [2, 1]);
        assert!(started.elapsed() >= Duration::from_secs(2));
    }
}
