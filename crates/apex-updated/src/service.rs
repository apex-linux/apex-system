//! The `io.github.sekirokenjii.Apex.Update1` interface (`crates/apex-dbus/data/…xml` is
//! authoritative) and the task that emits `PropertiesChanged`.
//!
//! Every method:
//! 1. counts as activity for the idle exit;
//! 2. validates its arguments (`InvalidArgument`) before any authentication prompt;
//! 3. authorizes: polkit for the user methods, uid 0 for `Automatic` and `ReportBoot`,
//!    nothing for `Ping` and `GetChanges` (read-only, like the properties);
//! 4. runs through the queue or directly (see [`crate::queue`]); queued operations read
//!    bootc status first, and `GetChanges` and property reads refresh the state when it is
//!    older than [`crate::REFRESH_MAX_AGE`];
//! 5. replies only with contract error names ([`ApexError::into_contract`]) and messages
//!    without control characters ([`crate::text`]).
//!
//! `Ping`, `GetChanges` and every property read first wait for the first refresh of this
//! instance ([`crate::engine::Engine::first_refresh`]): until then the published values are
//! placeholders, and an empty dictionary or an empty change list would claim "no such
//! deployment" or "nothing staged".
//!
//! Each polkit-guarded method asks for the action contract §8 assigns it
//! ([`PolkitAction::guarding`] is the table; the wiring tests hold the handlers to it). In
//! particular `AcknowledgeHealthFailure` is authorized with the `discard` action, not
//! `apply`: dismissing the warning is a routine decision like cancelling an update, and it
//! must stay possible for someone who may not install updates.
//!
//! Property changes: every property emits `PropertiesChanged` when its value changes (the
//! XML header). All twelve are read from one published [`Properties`] value, and
//! [`emit_property_changes`] first signals all of them, then every field that differs from
//! the last signalled one, so a getter can never return a value that was not, or will not
//! be, signalled.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use apex_bootc::CommandRunner;
use apex_dbus::{
    ApexError, BootVerdict, Change, Channel, Deployment, ErrorKind, INTERFACE, OBJECT_PATH,
    PolkitAction, State, UnknownValueError,
};
use tokio::sync::watch;
use zbus::Connection;
use zbus::message::Header;
use zbus::names::InterfaceName;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::Value;

use crate::auth::{Authorizer, require_root};
use crate::daemon::Daemon;
use crate::properties::Properties;
use crate::queue::Op;
use crate::text::display_text;

/// Minimum time between two `PropertiesChanged` signals; bootc reports progress at up to
/// 5 Hz, and clients need no more.
pub const EMIT_INTERVAL: Duration = Duration::from_millis(200);

/// The D-Bus object served at [`OBJECT_PATH`].
#[derive(Debug)]
pub struct Update1Service<R> {
    daemon: Daemon<R>,
    auth: Arc<dyn Authorizer>,
    properties: watch::Receiver<Properties>,
}

impl<R: CommandRunner + 'static> Update1Service<R> {
    /// Serves `daemon`, authorizing callers with `auth`.
    pub fn new(daemon: Daemon<R>, auth: Arc<dyn Authorizer>) -> Self {
        let properties = daemon.engine().subscribe();
        Self {
            daemon,
            auth,
            properties,
        }
    }

    async fn queued(
        &self,
        header: &Header<'_>,
        action: PolkitAction,
        op: Op,
    ) -> Result<(), ApexError> {
        self.auth.authorize(header, action).await?;
        // Every queued operation reads bootc status before it decides.
        self.daemon.submit(op).await
    }

    /// A property read: counts as activity, waits for the first refresh of this instance,
    /// and queues a refresh when the published state is older than
    /// [`crate::REFRESH_MAX_AGE`], so a client reading properties sees changes made outside
    /// the daemon (for example by an administrator running bootc) soon.
    async fn read<T>(&self, field: impl FnOnce(&Properties) -> T) -> T {
        self.daemon.activity().touch();
        self.daemon.engine().first_refresh().await;
        self.daemon.refresh_if_due();
        field(&self.properties.borrow())
    }
}

/// Logs a failed call and keeps only contract error names on the wire, with a message
/// that is a valid D-Bus string and safe to print ([`display_text`]).
fn reply<T>(member: &str, result: Result<T, ApexError>) -> Result<T, ApexError> {
    result.map_err(|error| {
        let error = error.into_contract();
        tracing::debug!(member, error = %error, "method call failed");
        ApexError::new(
            error.kind().unwrap_or(ErrorKind::Internal),
            display_text(&error.message()),
        )
    })
}

fn invalid(error: &UnknownValueError) -> ApexError {
    ApexError::InvalidArgument(format!("{error}."))
}

fn digest_argument(name: &str, value: &str) -> Result<(), ApexError> {
    apex_bootc::Digest::parse(value).map(drop).map_err(|e| {
        ApexError::InvalidArgument(format!(
            "The {name} {value:?} is not an image digest (sha256:<64 hex digits>): {e}."
        ))
    })
}

// The method, argument and property names below are the contract's: zbus derives the D-Bus
// names from them, and the wiring test compares the introspection with the XML.
#[allow(
    clippy::needless_pass_by_value,
    reason = "zbus hands the header over by value"
)]
#[zbus::interface(name = "io.github.sekirokenjii.Apex.Update1")]
impl<R: CommandRunner + 'static> Update1Service<R> {
    async fn ping(&self) -> Result<(), ApexError> {
        let _call = self.daemon.activity().call();
        reply("Ping", self.daemon.ping().await)
    }

    async fn check(&self, #[zbus(header)] header: Header<'_>) -> Result<(), ApexError> {
        let _call = self.daemon.activity().call();
        reply(
            "Check",
            self.queued(&header, PolkitAction::Check, Op::Check).await,
        )
    }

    async fn download(
        &self,
        override_soft_gates: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        let _call = self.daemon.activity().call();
        let op = Op::Download {
            override_soft_gates,
        };
        reply(
            "Download",
            self.queued(&header, PolkitAction::Download, op).await,
        )
    }

    async fn apply(
        &self,
        expected_digest: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        let _call = self.daemon.activity().call();
        let result = async {
            digest_argument("expected digest", expected_digest)?;
            self.auth.authorize(&header, PolkitAction::Apply).await?;
            // A digest the fresh state already refutes never reaches the queue.
            self.daemon.engine().precheck_apply(expected_digest)?;
            self.daemon
                .submit(Op::Apply {
                    expected_digest: expected_digest.to_owned(),
                })
                .await
        };
        reply("Apply", result.await)
    }

    async fn discard_staged(&self, #[zbus(header)] header: Header<'_>) -> Result<(), ApexError> {
        let _call = self.daemon.activity().call();
        reply(
            "DiscardStaged",
            self.queued(&header, PolkitAction::Discard, Op::Discard)
                .await,
        )
    }

    async fn rollback(
        &self,
        mark_bad: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        let _call = self.daemon.activity().call();
        reply(
            "Rollback",
            self.queued(&header, PolkitAction::Rollback, Op::Rollback { mark_bad })
                .await,
        )
    }

    #[zbus(out_args("changes"))]
    async fn get_changes(&self) -> Result<Vec<Change>, ApexError> {
        let _call = self.daemon.activity().call();
        self.daemon.engine().first_refresh().await;
        self.daemon.ensure_fresh().await;
        reply("GetChanges", self.daemon.engine().get_changes().await)
    }

    async fn switch_channel(
        &self,
        channel: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        let _call = self.daemon.activity().call();
        let result = async {
            let channel: Channel = channel.parse().map_err(|e| invalid(&e))?;
            self.queued(
                &header,
                PolkitAction::SwitchChannel,
                Op::SwitchChannel(channel),
            )
            .await
        };
        reply("SwitchChannel", result.await)
    }

    async fn acknowledge_health_failure(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        let _call = self.daemon.activity().call();
        let result = async {
            // Contract §8: the discard action guards this method.
            self.auth.authorize(&header, PolkitAction::Discard).await?;
            self.daemon.engine().acknowledge_health_failure()
        };
        reply("AcknowledgeHealthFailure", result.await)
    }

    async fn automatic(&self, #[zbus(header)] header: Header<'_>) -> Result<(), ApexError> {
        let _call = self.daemon.activity().call();
        let result = async {
            require_root(self.auth.as_ref(), &header).await?;
            self.daemon.submit(Op::Automatic).await
        };
        reply("Automatic", result.await)
    }

    async fn report_boot(
        &self,
        verdict: &str,
        digest: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        let _call = self.daemon.activity().call();
        let result = async {
            let verdict: BootVerdict = verdict.parse().map_err(|e| invalid(&e))?;
            let digest = if digest.is_empty() {
                None
            } else {
                digest_argument("digest", digest)?;
                Some(digest)
            };
            require_root(self.auth.as_ref(), &header).await?;
            tracing::info!(%verdict, digest, "boot assessment reported");
            match (verdict, digest) {
                (BootVerdict::Fallback, Some(digest)) => {
                    self.daemon
                        .submit(Op::Fallback {
                            digest: digest.to_owned(),
                        })
                        .await
                }
                (BootVerdict::Fallback, None) => Err(ApexError::InvalidArgument(
                    "A fallback report needs the digest of the update that failed.".to_owned(),
                )),
                (verdict, digest) => self.daemon.engine().report_boot(verdict, digest),
            }
        };
        reply("ReportBoot", result.await)
    }

    #[zbus(property)]
    async fn state(&self) -> State {
        self.read(|p| p.state).await
    }

    #[zbus(property)]
    async fn progress(&self) -> f64 {
        self.read(|p| p.progress).await
    }

    #[zbus(property)]
    async fn booted(&self) -> Deployment {
        self.read(|p| p.booted.clone()).await
    }

    #[zbus(property)]
    async fn staged(&self) -> Deployment {
        self.read(|p| p.staged.clone()).await
    }

    #[zbus(property, name = "Rollback")]
    async fn rollback_deployment(&self) -> Deployment {
        self.read(|p| p.rollback.clone()).await
    }

    #[zbus(property)]
    async fn available(&self) -> Deployment {
        self.read(|p| p.available.clone()).await
    }

    #[zbus(property)]
    async fn channel(&self) -> Channel {
        self.read(|p| p.channel).await
    }

    #[zbus(property)]
    async fn last_check(&self) -> u64 {
        self.read(|p| p.last_check).await
    }

    #[zbus(property)]
    async fn deferred_reason(&self) -> String {
        self.read(|p| p.deferred_reason.clone()).await
    }

    #[zbus(property)]
    async fn last_error(&self) -> String {
        self.read(|p| p.last_error.clone()).await
    }

    #[zbus(property)]
    async fn stale(&self) -> bool {
        self.read(|p| p.stale).await
    }

    #[zbus(property)]
    async fn health_failure(&self) -> String {
        self.read(|p| p.health_failure.clone()).await
    }
}

/// Emits `PropertiesChanged` for the published properties until the engine is dropped:
/// first one signal with **all** twelve properties and their current values, then one
/// signal per change with the properties that differ from the last signalled values (at
/// most one signal per [`EMIT_INTERVAL`]).
///
/// The first signal is for clients that outlive a service instance. This service exits when
/// idle, and a client's property cache (zbus, `GLib`) is updated only by `PropertiesChanged`
/// and is not reset when the name changes owner. The new instance starts from placeholder
/// values, so a value that went back to its default meanwhile (`LastError` cleared by a
/// successful call, `HealthFailure` cleared at boot, `Staged` emptied by a reboot) would
/// otherwise never be signalled, and the client would keep showing the old one. `main`
/// starts this after it owns the bus name, so the bus delivers the first signal to clients
/// that subscribed by the well-known name.
///
/// The signal is built here from the published values rather than through the interface
/// object, so emitting never waits for the interface lock that a long method call holds.
pub fn emit_property_changes(
    connection: Connection,
    properties: watch::Receiver<Properties>,
) -> impl Future<Output = ()> + Send + 'static {
    emit_from(connection, properties)
}

async fn emit_from(connection: Connection, mut properties: watch::Receiver<Properties>) {
    let emitter = match SignalEmitter::new(&connection, OBJECT_PATH) {
        Ok(emitter) => emitter,
        Err(error) => {
            tracing::error!(%error, "cannot emit property changes");
            return;
        }
    };
    let interface = InterfaceName::from_static_str_unchecked(INTERFACE);
    // Marks the current version seen, so the loop below starts from exactly these values.
    let mut last = properties.borrow_and_update().clone();
    signal(&emitter, &interface, last.all()).await;
    tokio::time::sleep(EMIT_INTERVAL).await;
    while properties.changed().await.is_ok() {
        let current = properties.borrow_and_update().clone();
        let changed = current.changed_since(&last);
        if !changed.is_empty() {
            signal(&emitter, &interface, changed).await;
        }
        last = current;
        tokio::time::sleep(EMIT_INTERVAL).await;
    }
}

async fn signal(
    emitter: &SignalEmitter<'_>,
    interface: &InterfaceName<'static>,
    changed: HashMap<&'static str, Value<'static>>,
) {
    let signal = zbus::fdo::Properties::properties_changed(
        emitter,
        interface.clone(),
        changed,
        Cow::Borrowed(&[]),
    );
    if let Err(error) = signal.await {
        tracing::debug!(%error, "cannot emit PropertiesChanged");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_carry_contract_names_and_clean_messages() {
        let error = reply::<()>(
            "Check",
            Err(ApexError::Network(
                "error: x\0y\r\u{9b}2J unreachable".to_owned(),
            )),
        )
        .unwrap_err();
        assert_eq!(error.kind(), Some(ErrorKind::Network));
        assert_eq!(error.message(), "error: x\u{fffd}y \u{fffd}2J unreachable");
        let foreign = reply::<()>(
            "Ping",
            Err(ApexError::ZBus(zbus::Error::Failure("gone".to_owned()))),
        )
        .unwrap_err();
        assert_eq!(foreign.kind(), Some(ErrorKind::Internal));
        assert!(foreign.message().contains("gone"));
    }
}
