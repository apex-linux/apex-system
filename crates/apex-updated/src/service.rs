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
use zbus::Variant;
use zbus::message::Header;
use zbus::names::InterfaceName;
use zbus::object_server::SignalEmitter;

use crate::auth::{Authorizer, require_root};
use crate::daemon::Daemon;
use crate::properties::Properties;
use crate::queue::Op;
use crate::text::display_text;

/// bootc reports progress at up to 5 Hz, and clients need no more.
pub const EMIT_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug)]
pub struct Update1Service<R> {
    daemon: Daemon<R>,
    auth: Arc<dyn Authorizer>,
    properties: watch::Receiver<Properties>,
}

impl<R: CommandRunner + 'static> Update1Service<R> {
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
        self.daemon.submit(op).await
    }

    /// The refresh lets readers see changes made outside the daemon, such as by root running
    /// bootc.
    async fn read_property<T>(&self, field: impl FnOnce(&Properties) -> T) -> T {
        self.daemon.engine().first_refresh().await;
        self.daemon.refresh_if_due();
        field(&self.properties.borrow())
    }
}

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

// zbus derives the D-Bus names from these method, argument and property names: renaming one
// changes the interface.
#[allow(
    clippy::needless_pass_by_value,
    reason = "zbus hands the header over by value"
)]
#[zbus::interface(name = "io.github.apex_linux.Apex.Update1")]
impl<R: CommandRunner + 'static> Update1Service<R> {
    async fn ping(&self) -> Result<(), ApexError> {
        reply("Ping", self.daemon.ping().await)
    }

    async fn check(&self, #[zbus(header)] header: Header<'_>) -> Result<(), ApexError> {
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
        let result = async {
            // Arguments first, so an invalid call never shows an authentication prompt.
            digest_argument("expected digest", expected_digest)?;
            self.auth.authorize(&header, PolkitAction::Apply).await?;
            self.daemon.engine().precheck_apply(expected_digest)?;
            self.daemon
                .submit(Op::Apply {
                    expected_digest: expected_digest.to_owned(),
                })
                .await
        };
        reply("Apply", result.await)
    }

    async fn rollback(
        &self,
        mark_bad: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        reply(
            "Rollback",
            self.queued(&header, PolkitAction::Rollback, Op::Rollback { mark_bad })
                .await,
        )
    }

    #[zbus(out_args("changes"))]
    async fn get_changes(&self) -> Result<Vec<Change>, ApexError> {
        self.daemon.engine().first_refresh().await;
        self.daemon.ensure_fresh().await;
        reply("GetChanges", self.daemon.engine().get_changes().await)
    }

    #[zbus(out_args("changes"))]
    async fn get_changes_for(&self, digest: &str) -> Result<Vec<Change>, ApexError> {
        let result = async {
            digest_argument("digest", digest)?;
            self.daemon.engine().first_refresh().await;
            self.daemon.ensure_fresh().await;
            self.daemon.engine().get_changes_for(digest).await
        };
        reply("GetChangesFor", result.await)
    }

    async fn acknowledge_health_failure(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        let result = async {
            // Not `apply`: dismissing the warning must stay possible for someone who may not
            // install updates.
            self.auth.authorize(&header, PolkitAction::Discard).await?;
            self.daemon.engine().acknowledge_health_failure()
        };
        reply("AcknowledgeHealthFailure", result.await)
    }

    async fn automatic(&self, #[zbus(header)] header: Header<'_>) -> Result<(), ApexError> {
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

    async fn set_automatic_downloads(
        &self,
        enabled: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        let result = async {
            self.auth
                .authorize(&header, PolkitAction::Configure)
                .await?;
            self.daemon.engine().set_automatic_downloads(enabled)
        };
        reply("SetAutomaticDownloads", result.await)
    }

    #[zbus(property)]
    async fn state(&self) -> State {
        self.read_property(|p| p.state).await
    }

    #[zbus(property)]
    async fn progress(&self) -> f64 {
        self.read_property(|p| p.progress).await
    }

    #[zbus(property)]
    async fn booted(&self) -> Deployment {
        self.read_property(|p| p.booted.clone()).await
    }

    #[zbus(property)]
    async fn staged(&self) -> Deployment {
        self.read_property(|p| p.staged.clone()).await
    }

    #[zbus(property, name = "Rollback")]
    async fn rollback_deployment(&self) -> Deployment {
        self.read_property(|p| p.rollback.clone()).await
    }

    #[zbus(property)]
    async fn available(&self) -> Deployment {
        self.read_property(|p| p.available.clone()).await
    }

    #[zbus(property)]
    async fn channel(&self) -> Channel {
        self.read_property(|p| p.channel).await
    }

    #[zbus(property)]
    async fn last_check(&self) -> u64 {
        self.read_property(|p| p.last_check).await
    }

    #[zbus(property)]
    async fn deferred_reason(&self) -> String {
        self.read_property(|p| p.deferred_reason.clone()).await
    }

    #[zbus(property)]
    async fn last_error(&self) -> String {
        self.read_property(|p| p.last_error.clone()).await
    }

    #[zbus(property)]
    async fn health_failure(&self) -> String {
        self.read_property(|p| p.health_failure.clone()).await
    }

    #[zbus(property)]
    async fn health_failure_checks(&self) -> Vec<String> {
        self.read_property(|p| p.health_failure_checks.clone())
            .await
    }

    #[zbus(property)]
    async fn last_fallback(&self) -> String {
        self.read_property(|p| p.last_fallback.clone()).await
    }

    #[zbus(property)]
    async fn automatic_downloads(&self) -> bool {
        self.read_property(|p| p.automatic_downloads).await
    }

    #[zbus(property)]
    async fn kept(&self) -> Vec<Deployment> {
        self.read_property(|p| p.kept.clone()).await
    }
}

/// First signals every property: client caches survive a restart. Built from the
/// published values, so it never waits for the interface lock a long method call holds.
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
    // Marks this version seen, so the loop starts from exactly these values.
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
    changed: HashMap<&'static str, Variant<'static>>,
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
