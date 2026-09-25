//! Process lifecycle: SIGTERM handling and the drain-then-close sequence.
//!
//! This lives in `util` rather than in one server module because signal
//! disposition and process exit are properties of the *process*, not of any one
//! protocol. `main.rs` dispatches the same binary into the swift, ws and
//! confirmation servers; today only the ws server calls [`install`], but the
//! next one to need it wires the same channel rather than growing a second copy
//! of the phase machine.

use {
    std::{env, sync::OnceLock, time::Duration},
    tokio::{
        signal::unix::{signal, SignalKind},
        sync::watch,
    },
};

/// Shutdown phase of the process.
///
/// SIGTERM walks it `Running -> Draining -> Closing`. The `Draining` step is the
/// point of the whole thing: it fails health checks while the server keeps
/// serving, so the load balancer deregisters this pod *before* its connections
/// are torn down. Without that window a client's reconnect races endpoint
/// removal and can land straight back on the pod that is about to die, which is
/// what turns a routine node consolidation into a visible outage for
/// subscribers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lifecycle {
    Running,
    Draining,
    Closing,
}

static LIFECYCLE: OnceLock<watch::Sender<Lifecycle>> = OnceLock::new();

fn channel() -> &'static watch::Sender<Lifecycle> {
    LIFECYCLE.get_or_init(|| watch::channel(Lifecycle::Running).0)
}

/// Observe the phase. Callers that park on a shutdown should hold one receiver
/// for the life of the task rather than resubscribing per loop iteration: a
/// `wait_for` future registers a waiter under a process-shared mutex every time
/// it is polled from scratch.
pub fn subscribe() -> watch::Receiver<Lifecycle> {
    channel().subscribe()
}

/// False from the moment SIGTERM lands, so health handlers can fail readiness
/// while still serving in-flight work.
pub fn is_serving() -> bool {
    *channel().borrow() == Lifecycle::Running
}

fn duration_from_env(key: &str, default_secs: u64) -> Duration {
    Duration::from_secs(
        env::var(key)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default_secs),
    )
}

/// Install the SIGTERM/SIGINT handler that drives the phases above.
///
/// Without this the process keeps the default signal disposition: SIGTERM kills
/// it outright, every connection dies as an unannounced TCP reset, and the
/// health endpoint never gets the chance to say anything.
///
/// Two windows, both env-tunable:
///
/// - `SHUTDOWN_DRAIN_SECS` (default 15) — how long health checks report
///   unhealthy before connections are closed. Set it above the fronting load
///   balancer's deregistration delay, and above twice the readiness probe
///   period so at least one probe is guaranteed to observe the failure.
/// - `SHUTDOWN_CLOSE_SECS` (default 5) — how long tasks get to flush their
///   goodbyes before the process exits.
///
/// `terminationGracePeriodSeconds` on the pod must exceed the sum, or the
/// kubelet SIGKILLs the process mid-drain and nothing has been gained.
pub fn install() {
    tokio::spawn(async move {
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler installs");
        let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler installs");
        tokio::select! {
            _ = sigterm.recv() => log::info!(target: "shutdown", "SIGTERM received"),
            _ = sigint.recv() => log::info!(target: "shutdown", "SIGINT received"),
        }

        let drain = duration_from_env("SHUTDOWN_DRAIN_SECS", 15);
        let close = duration_from_env("SHUTDOWN_CLOSE_SECS", 5);
        let tx = channel();

        log::info!(target: "shutdown", "draining for {drain:?}: health checks now fail");
        let _ = tx.send(Lifecycle::Draining);
        tokio::time::sleep(drain).await;

        log::info!(target: "shutdown", "drain elapsed, closing connections with {close:?} grace");
        let _ = tx.send(Lifecycle::Closing);
        tokio::time::sleep(close).await;

        log::info!(target: "shutdown", "shutdown complete");
        std::process::exit(0);
    });
}
