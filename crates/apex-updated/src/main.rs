//! `apex-updated`: the Apex update service (`/usr/libexec/apex/apex-updated`).
//!
//! Started by D-Bus activation (`apex-updated.service`, `Type=dbus`). It serves the
//! interface, refreshes its state, requests `io.github.sekirokenjii.Apex.Update1`, signals
//! every property once, tells systemd it is ready, and exits after five idle minutes or on
//! `SIGTERM`/`SIGINT`. It exits with a failure if the operation worker dies.

use std::io::IsTerminal as _;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use apex_bootc::TokioRunner;
use apex_dbus::{BUS_NAME, OBJECT_PATH};
use apex_update::StateDir;
use apex_updated::auth::PolkitAuthorizer;
use apex_updated::daemon::Daemon;
use apex_updated::engine::{Engine, EngineParts, grubenv_writer};
use apex_updated::providers::{GrubEnvFile, Providers};
use apex_updated::queue::Op;
use apex_updated::service::{Update1Service, emit_property_changes};
use apex_updated::store::Store as _;
use apex_updated::{IDLE_TIMEOUT, properties::Properties};
use sd_notify::NotifyState;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;
use zbus::fdo::{RequestNameFlags, RequestNameReply};

/// How often the idle rule is evaluated.
const IDLE_POLL: Duration = Duration::from_secs(15);
/// After releasing the name, how long calls that were already on their way may take.
const DRAIN: Duration = Duration::from_secs(2);
/// How long a cancelled operation may take to stop (bootc gets SIGTERM, then SIGKILL after
/// 5 s).
const STOP_GRACE: Duration = Duration::from_secs(15);

fn main() -> ExitCode {
    init_logging();
    // One thread: zbus on tokio spawns no executor of its own, and the few blocking calls
    // (grub2-editenv) go to tokio's blocking pool on demand.
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the tokio runtime")
        .and_then(|runtime| runtime.block_on(run()));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

/// The journal when started by systemd with stderr connected to it (`JOURNAL_STREAM`),
/// stderr otherwise. `RUST_LOG` overrides the default level (`info`).
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);
    if stderr_is_journal() {
        match tracing_journald::layer() {
            Ok(journald) => {
                registry.with(journald).init();
                return;
            }
            Err(error) => eprintln!("apex-updated: journald unavailable ({error}); using stderr"),
        }
    }
    registry
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal()),
        )
        .init();
}

/// Whether stderr is the journal stream systemd set up (systemd.exec(5), `JOURNAL_STREAM`:
/// compare its `device:inode` with stderr's, so a variable inherited by a process whose
/// stderr was redirected does not count).
fn stderr_is_journal() -> bool {
    let Some(value) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let Ok(stat) = nix::sys::stat::fstat(std::io::stderr()) else {
        return false;
    };
    value
        .to_str()
        .is_some_and(|v| journal_stream_is(v, stat.st_dev, stat.st_ino))
}

/// Whether a `JOURNAL_STREAM` value (`<device>:<inode>`, decimal) names this device and
/// inode.
fn journal_stream_is(value: &str, device: u64, inode: u64) -> bool {
    value.split_once(':').is_some_and(|(d, i)| {
        d.parse::<u64>().is_ok_and(|d| d == device) && i.parse::<u64>().is_ok_and(|i| i == inode)
    })
}

fn notify(states: &[NotifyState<'_>]) {
    if let Err(error) = sd_notify::notify(states) {
        tracing::debug!(%error, "sd_notify failed");
    }
}

async fn run() -> anyhow::Result<()> {
    let connection = zbus::connection::Builder::system()
        .context("cannot configure the system bus connection")?
        .build()
        .await
        .context("cannot connect to the system bus")?;

    let store = StateDir::default();
    // Temporary files of writers that died before their rename; each write cleans up after
    // earlier writes of its own file, this covers files that are rarely written.
    let removed = store.remove_stale_temp_files();
    if removed > 0 {
        tracing::info!(
            removed,
            "removed temporary files left by interrupted writes"
        );
    }
    let engine = Engine::new(EngineParts {
        runner: TokioRunner::new(),
        store: Arc::new(store),
        grub: Box::new(grubenv_writer()),
        grubenv: Arc::new(GrubEnvFile::system()),
        providers: Providers::system(&connection),
    });
    let daemon = Daemon::new(engine);
    let mut worker = daemon.spawn_worker();
    drop(daemon.queue().submit(Op::Refresh));

    let service = Update1Service::new(
        daemon.clone(),
        Arc::new(PolkitAuthorizer::new(connection.clone())),
    );
    connection
        .object_server()
        .at(OBJECT_PATH, service)
        .await
        .context("cannot serve the update interface")?;
    // Own the name only once the state is known (or PING_WAIT has passed): the bus delivers
    // the call that activated this instance right after RequestName, and it must find the
    // real state, not placeholders (the handlers wait for it too). With Type=dbus, systemd
    // considers the service started at that point as well.
    daemon.engine().first_refresh().await;
    // Never queue behind another owner: a second instance must not run at all.
    let reply = connection
        .request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
        .await
        .with_context(|| format!("cannot own {BUS_NAME}"))?;
    if !matches!(
        reply,
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner
    ) {
        daemon.shutdown();
        anyhow::bail!("{BUS_NAME} is already owned by another process ({reply:?})");
    }
    // Started once the name is owned, so its first signal (every property, see
    // emit_property_changes) reaches clients subscribed by the well-known name and replaces
    // what they cached from an earlier instance.
    let emitter = tokio::spawn(emit_property_changes(
        connection.clone(),
        daemon.engine().subscribe(),
    ));
    notify(&[NotifyState::Ready, NotifyState::Status("Serving")]);
    tracing::info!("serving {BUS_NAME}");

    let mut status = daemon.engine().subscribe();
    let mut terminate = signal(SignalKind::terminate()).context("SIGTERM handler")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("SIGINT handler")?;
    let mut idle_poll = tokio::time::interval(IDLE_POLL);
    let reason = loop {
        tokio::select! {
            _ = terminate.recv() => break Stop::Signal("SIGTERM"),
            _ = interrupt.recv() => break Stop::Signal("SIGINT"),
            _ = idle_poll.tick() => {
                if daemon.is_idle(IDLE_TIMEOUT) {
                    break Stop::Idle;
                }
            }
            ended = &mut worker => break Stop::WorkerEnded(ended),
            changed = status.changed() => {
                if changed.is_ok() {
                    let text = status_text(&status.borrow_and_update());
                    notify(&[NotifyState::Status(&text)]);
                }
            }
        }
    };
    tracing::info!(reason = %reason, "stopping");
    notify(&[NotifyState::Stopping]);

    // Release the name first: new callers then activate a fresh instance instead of
    // reaching one that is going away. Calls already dispatched still finish.
    if let Err(error) = connection.release_name(BUS_NAME).await {
        tracing::debug!(%error, "cannot release the bus name");
    }
    if matches!(reason, Stop::Idle) {
        tokio::time::sleep(DRAIN).await;
    }
    daemon.shutdown();
    if let Stop::WorkerEnded(ended) = reason {
        emitter.abort();
        // A worker that ends while the queue is open has panicked (it only returns after
        // shutdown): exit with a failure, so systemd records it and the next call activates
        // a fresh instance.
        return Err(match ended {
            Err(join) if join.is_panic() => anyhow::anyhow!("an update operation panicked"),
            Err(join) => anyhow::anyhow!("the operation worker stopped: {join}"),
            Ok(()) => anyhow::anyhow!("the operation worker stopped unexpectedly"),
        });
    }
    if tokio::time::timeout(STOP_GRACE, worker).await.is_err() {
        tracing::warn!("the running operation did not stop in time");
    }
    emitter.abort();
    Ok(())
}

/// Why the service stops.
enum Stop {
    /// `SIGTERM` or `SIGINT`.
    Signal(&'static str),
    /// No activity for [`IDLE_TIMEOUT`].
    Idle,
    /// The operation worker ended while the service was running.
    WorkerEnded(Result<(), tokio::task::JoinError>),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Signal(name) => f.write_str(name),
            Self::Idle => f.write_str("idle"),
            Self::WorkerEnded(_) => f.write_str("the operation worker ended"),
        }
    }
}

/// A one-line status for `systemctl status`.
fn status_text(properties: &Properties) -> String {
    if properties.progress >= 0.0 {
        format!(
            "State: {} ({:.0} %)",
            properties.state,
            properties.progress * 100.0
        )
    } else {
        format!("State: {}", properties.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_stream_values() {
        assert!(journal_stream_is("10:31609", 10, 31_609));
        assert!(!journal_stream_is("10:31609", 10, 31_610));
        assert!(!journal_stream_is("11:31609", 10, 31_609));
        assert!(!journal_stream_is("", 0, 0));
        assert!(!journal_stream_is("10", 10, 0));
        assert!(!journal_stream_is("a:b", 0, 0));
    }

    #[test]
    fn status_lines() {
        let mut properties = Properties::default();
        assert_eq!(status_text(&properties), "State: unknown");
        properties.state = apex_dbus::State::Downloading;
        properties.progress = 0.426;
        assert_eq!(status_text(&properties), "State: downloading (43 %)");
    }
}
