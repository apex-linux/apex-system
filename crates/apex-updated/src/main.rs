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
use apex_updated::properties::Properties;
use apex_updated::providers::{GrubEnvFile, Providers};
use apex_updated::queue::Op;
use apex_updated::service::{Update1Service, emit_property_changes};
use apex_updated::store::Store as _;
use sd_notify::NotifyState;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;
use zbus::fdo::{RequestNameFlags, RequestNameReply};

// bootc gets SIGTERM, then SIGKILL after 5 s.
const STOP_GRACE: Duration = Duration::from_secs(15);

fn main() -> ExitCode {
    init_logging();
    // zbus spawns no executor of its own, and grub2-editenv runs on the blocking pool.
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

/// systemd.exec(5): `JOURNAL_STREAM` is inherited, so it counts only when its
/// `device:inode` is stderr's.
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
        .build()
        .await
        .context("cannot connect to the system bus")?;

    let store = StateDir::default();
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
    // The bus delivers the activating call right after RequestName; it must find real state.
    daemon.engine().first_refresh().await;
    let reply = connection
        .request_name_with_flags(BUS_NAME, RequestNameFlags::DO_NOT_QUEUE)
        .await
        .with_context(|| format!("cannot own {BUS_NAME}"))?;
    if !matches!(
        reply,
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner
    ) {
        daemon.shutdown();
        anyhow::bail!("{BUS_NAME} is already owned by another process ({reply:?})");
    }
    // Once the name is owned, so the first signal reaches clients matching on that name.
    let emitter = tokio::spawn(emit_property_changes(
        connection.clone(),
        daemon.engine().subscribe(),
    ));
    notify(&[NotifyState::Ready, NotifyState::Status("Serving")]);
    tracing::info!("serving {BUS_NAME}");

    let mut status = daemon.engine().subscribe();
    let mut terminate = signal(SignalKind::terminate()).context("SIGTERM handler")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("SIGINT handler")?;
    let reason = loop {
        tokio::select! {
            _ = terminate.recv() => break Stop::Signal("SIGTERM"),
            _ = interrupt.recv() => break Stop::Signal("SIGINT"),
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

    // Released first, so new callers activate a fresh instance instead of reaching this one.
    if let Err(error) = connection.release_name(BUS_NAME).await {
        tracing::debug!(%error, "cannot release the bus name");
    }
    daemon.shutdown();
    if let Stop::WorkerEnded(ended) = reason {
        emitter.abort();
        // The worker only returns after shutdown: fail, so activation starts a fresh one.
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

enum Stop {
    Signal(&'static str),
    WorkerEnded(Result<(), tokio::task::JoinError>),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Signal(name) => f.write_str(name),
            Self::WorkerEnded(_) => f.write_str("the operation worker ended"),
        }
    }
}

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
