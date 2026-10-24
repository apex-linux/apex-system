use std::future::Future;
use std::io::IsTerminal as _;
use std::time::Duration;

use apex_dbus::zbus;
use apex_dbus::{ApexError, Change, Channel, Deployment, ErrorKind, State, Update1Proxy};

use crate::exit::Exit;

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceStatus {
    pub state: State,
    /// `[0, 1]` while downloading, `-1` when unknown.
    pub progress: f64,
    pub booted: Deployment,
    pub staged: Deployment,
    pub rollback: Deployment,
    pub available: Deployment,
    pub channel: Channel,
    /// Unix seconds; 0 if never.
    pub last_check: u64,
    pub deferred_reason: String,
    pub last_error: String,
    pub health_failure: String,
    /// `""` when none, or when the service predates the property.
    pub last_fallback: String,
    /// `None` when the service predates the property.
    pub automatic_downloads: Option<bool>,
    /// Pinned by the service and in the boot menu only; empty when the service predates it.
    pub kept: Vec<Deployment>,
}

// `impl Future` rather than `async fn`: public async trait methods trip the
// `async_fn_in_trait` lint.
pub trait UpdateService {
    fn status(&self) -> impl Future<Output = Result<ServiceStatus, ClientError>>;
    fn check(&self) -> impl Future<Output = Result<(), ClientError>>;
    fn download(&self, override_soft_gates: bool) -> impl Future<Output = Result<(), ClientError>>;
    fn apply(&self, expected_digest: &str) -> impl Future<Output = Result<(), ClientError>>;
    fn rollback(&self, mark_bad: bool) -> impl Future<Output = Result<(), ClientError>>;
    fn changes(&self) -> impl Future<Output = Result<Vec<Change>, ClientError>>;
    fn acknowledge_health_failure(&self) -> impl Future<Output = Result<(), ClientError>>;
    fn automatic(&self) -> impl Future<Output = Result<(), ClientError>>;
    fn set_automatic_downloads(
        &self,
        enabled: bool,
    ) -> impl Future<Output = Result<(), ClientError>>;
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot connect to the system D-Bus: {0}")]
    NoBus(String),
    #[error("the Apex update service (apex-updated) is not available: {0}")]
    Unavailable(String),
    #[error(
        "apex-updated does not understand this request ({0}); apexctl and apex-updated are from different versions"
    )]
    Mismatch(String),
    #[error("{}", daemon_message(.0))]
    Daemon(ApexError),
    #[error("D-Bus error: {0}")]
    DBus(String),
    #[error("{0}")]
    NothingToDo(String),
    #[error("{0}")]
    InvalidArgument(String),
    #[error("{0}")]
    ChangedWhileReading(String),
    #[error("{0}")]
    NotReady(String),
}

fn daemon_message(error: &ApexError) -> String {
    let message = error.message();
    match error.kind() {
        Some(ErrorKind::PolicyDenied) => format!("not now: {message}"),
        Some(ErrorKind::NotAuthorized) => format!("not authorized: {message}"),
        Some(ErrorKind::Busy) => format!("another update operation is running: {message}"),
        Some(ErrorKind::DigestMismatch) => {
            format!("the staged update changed since you looked at it: {message}")
        }
        Some(ErrorKind::Network) => format!("the image registry cannot be reached: {message}"),
        Some(ErrorKind::Bootc) => format!("bootc failed: {message}"),
        Some(ErrorKind::InvalidArgument) => format!("invalid argument: {message}"),
        Some(ErrorKind::Internal) => format!("internal error in apex-updated: {message}"),
        Some(ErrorKind::NothingToDo) | None => message,
    }
}

impl ClientError {
    pub fn exit(&self) -> Exit {
        match self {
            Self::Daemon(error) => match error.kind() {
                Some(ErrorKind::PolicyDenied) => Exit::PolicyDenied,
                Some(ErrorKind::NothingToDo) => Exit::NothingToDo,
                _ => Exit::Error,
            },
            Self::NothingToDo(_) => Exit::NothingToDo,
            _ => Exit::Error,
        }
    }

    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::NoBus(_) => {
                Some("Is D-Bus running? apexctl talks to apex-updated on the system bus.")
            }
            Self::Unavailable(_) => Some(
                "Check the service with: systemctl status apex-updated.service (journalctl -u apex-updated for its log).",
            ),
            Self::Daemon(error) => match error.kind() {
                Some(ErrorKind::NotAuthorized) => Some(
                    "Run the command from the logged-in desktop session (polkit asks for a password when needed), or as root.",
                ),
                Some(ErrorKind::PolicyDenied) => Some(
                    "For power or metered-network reasons only: apexctl update download --force.",
                ),
                Some(ErrorKind::Busy) => {
                    Some("Wait for it to finish; apexctl status shows the progress.")
                }
                Some(ErrorKind::DigestMismatch) => {
                    Some("Look at apexctl status again, then apply the digest it shows.")
                }
                Some(ErrorKind::Internal) => {
                    Some("The service's log may say more: journalctl -u apex-updated")
                }
                _ => None,
            },
            _ => None,
        }
    }
}

fn is_unavailable_name(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner"
            | "org.freedesktop.DBus.Error.NoReply"
            | "org.freedesktop.DBus.Error.TimedOut"
            | "org.freedesktop.systemd1.NoSuchUnit"
            | "org.freedesktop.systemd1.UnitMasked"
    ) || name.starts_with("org.freedesktop.DBus.Error.Spawn.")
}

fn is_mismatch_name(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.DBus.Error.UnknownMethod"
            | "org.freedesktop.DBus.Error.UnknownInterface"
            | "org.freedesktop.DBus.Error.UnknownObject"
            | "org.freedesktop.DBus.Error.UnknownProperty"
            | "org.freedesktop.DBus.Error.InvalidArgs"
    )
}

pub fn classify(error: &zbus::Error) -> ClientError {
    use zbus::DBusError as _;
    let named = match error {
        zbus::Error::MethodError(name, description, _) => {
            Some((name.as_str().to_owned(), description.clone()))
        }
        zbus::Error::FDO(fdo) => Some((
            fdo.name().as_str().to_owned(),
            fdo.description().map(str::to_owned),
        )),
        _ => None,
    };
    if let Some((name, description)) = named {
        let detail = description.unwrap_or_else(|| name.clone());
        return if is_unavailable_name(&name) {
            ClientError::Unavailable(detail)
        } else if is_mismatch_name(&name) {
            ClientError::Mismatch(detail)
        } else if name == "org.freedesktop.DBus.Error.AccessDenied" {
            ClientError::DBus(format!("the bus policy refused the call: {detail}"))
        } else {
            ClientError::DBus(format!("{name}: {detail}"))
        };
    }
    match error {
        zbus::Error::InputOutput(_)
        | zbus::Error::Connection(..)
        | zbus::Error::Address(_)
        | zbus::Error::Handshake(_) => ClientError::NoBus(error.to_string()),
        other => ClientError::DBus(other.to_string()),
    }
}

impl From<ApexError> for ClientError {
    fn from(error: ApexError) -> Self {
        match error {
            ApexError::ZBus(inner) => classify(&inner),
            update1_reply => Self::Daemon(update1_reply),
        }
    }
}

impl From<zbus::Error> for ClientError {
    fn from(error: zbus::Error) -> Self {
        // Property reads can carry an Update1 error reply too.
        ApexError::from(error).into()
    }
}

#[derive(Debug, Clone)]
pub struct DbusService {
    proxy: Update1Proxy<'static>,
    show_progress: bool,
}

impl DbusService {
    /// Must run inside the tokio runtime.
    pub async fn connect() -> Result<Self, ClientError> {
        let connection = zbus::Connection::system().await.map_err(|e| classify(&e))?;
        let proxy = Update1Proxy::new(&connection)
            .await
            .map_err(|e| classify(&e))?;
        Ok(Self::from_proxy(proxy, std::io::stderr().is_terminal()))
    }

    pub fn from_proxy(proxy: Update1Proxy<'static>, show_progress: bool) -> Self {
        Self {
            proxy,
            show_progress,
        }
    }
}

const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const ERASE_LINE: &str = "\r\x1b[2K";

/// Only while downloading, so nothing is drawn over a polkit password prompt.
pub fn progress_display(state: State, progress: f64) -> Option<String> {
    (state == State::Downloading).then(|| crate::status::progress_line(progress))
}

impl DbusService {
    /// With the default property cache of [`DbusService::connect`], each poll is a cache read
    /// kept current by `PropertiesChanged`, not a bus call.
    async fn with_progress(
        &self,
        call: impl Future<Output = Result<(), ApexError>>,
    ) -> Result<(), ClientError> {
        if !self.show_progress {
            return Ok(call.await?);
        }
        tokio::pin!(call);
        let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
        let mut drawn = false;
        let result = loop {
            tokio::select! {
                result = &mut call => break result,
                _ = ticker.tick() => {
                    let state = self.proxy.state().await;
                    let progress = self.proxy.progress().await;
                    if let (Ok(state), Ok(progress)) = (state, progress)
                        && let Some(line) = progress_display(state, progress)
                    {
                        eprint!("{ERASE_LINE}{line}");
                        drawn = true;
                    }
                }
            }
        };
        if drawn {
            eprint!("{ERASE_LINE}");
        }
        Ok(result?)
    }
}

impl UpdateService for DbusService {
    async fn status(&self) -> Result<ServiceStatus, ClientError> {
        let p = &self.proxy;
        Ok(ServiceStatus {
            state: p.state().await?,
            progress: p.progress().await?,
            booted: p.booted().await?,
            staged: p.staged().await?,
            rollback: p.rollback_deployment().await?,
            available: p.available().await?,
            channel: p.channel().await?,
            last_check: p.last_check().await?,
            deferred_reason: p.deferred_reason().await?,
            last_error: p.last_error().await?,
            health_failure: p.health_failure().await?,
            last_fallback: p.last_fallback().await.unwrap_or_default(),
            automatic_downloads: p.automatic_downloads().await.ok(),
            kept: p.kept().await.unwrap_or_default(),
        })
    }

    async fn check(&self) -> Result<(), ClientError> {
        Ok(self.proxy.check().await?)
    }

    async fn download(&self, override_soft_gates: bool) -> Result<(), ClientError> {
        self.with_progress(self.proxy.download(override_soft_gates))
            .await
    }

    async fn apply(&self, expected_digest: &str) -> Result<(), ClientError> {
        Ok(self.proxy.apply(expected_digest).await?)
    }

    async fn rollback(&self, mark_bad: bool) -> Result<(), ClientError> {
        Ok(self.proxy.rollback(mark_bad).await?)
    }

    async fn changes(&self) -> Result<Vec<Change>, ClientError> {
        Ok(self.proxy.get_changes().await?)
    }

    async fn acknowledge_health_failure(&self) -> Result<(), ClientError> {
        Ok(self.proxy.acknowledge_health_failure().await?)
    }

    async fn automatic(&self) -> Result<(), ClientError> {
        Ok(self.proxy.automatic().await?)
    }

    async fn set_automatic_downloads(&self, enabled: bool) -> Result<(), ClientError> {
        Ok(self.proxy.set_automatic_downloads(enabled).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::names::ErrorName;

    fn method_error(name: &str, description: &str) -> zbus::Error {
        let message = zbus::message::Message::method_call("/", "Ping")
            .build(&())
            .unwrap();
        zbus::Error::MethodError(
            ErrorName::try_from(name).unwrap().into(),
            Some(description.to_owned()),
            message,
        )
    }

    #[test]
    fn exit_codes_follow_the_contract() {
        let daemon = |kind| ClientError::Daemon(ApexError::new(kind, "m"));
        assert_eq!(daemon(ErrorKind::PolicyDenied).exit(), Exit::PolicyDenied);
        assert_eq!(daemon(ErrorKind::NothingToDo).exit(), Exit::NothingToDo);
        assert_eq!(daemon(ErrorKind::NotAuthorized).exit(), Exit::Error);
        assert_eq!(daemon(ErrorKind::Busy).exit(), Exit::Error);
        assert_eq!(
            ClientError::NothingToDo("x".into()).exit(),
            Exit::NothingToDo
        );
        assert_eq!(ClientError::NoBus("x".into()).exit(), Exit::Error);
    }

    #[test]
    fn a_service_that_cannot_answer_yet_is_an_error_without_a_hint() {
        let error = ClientError::NotReady("Cannot list the package changes now.".into());
        assert_eq!(error.exit(), Exit::Error);
        assert_eq!(error.to_string(), "Cannot list the package changes now.");
        assert_eq!(error.hint(), None);
    }

    #[test]
    fn internal_errors_point_to_the_service_log() {
        let error = ClientError::Daemon(ApexError::new(ErrorKind::Internal, "m"));
        assert_eq!(error.exit(), Exit::Error);
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("journalctl -u apex-updated")),
            "{error}"
        );
    }

    #[test]
    fn unreachable_daemon_gets_a_friendly_message() {
        let error = ClientError::from(method_error(
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "The name is not activatable",
        ));
        assert!(matches!(error, ClientError::Unavailable(_)));
        assert!(error.to_string().contains("apex-updated"));
        assert!(
            error
                .hint()
                .unwrap()
                .contains("systemctl status apex-updated")
        );
        let spawn = ClientError::from(method_error(
            "org.freedesktop.DBus.Error.Spawn.ChildExited",
            "exited 1",
        ));
        assert!(matches!(spawn, ClientError::Unavailable(_)));
        let mismatch = ClientError::from(method_error(
            "org.freedesktop.DBus.Error.UnknownMethod",
            "no Frobnicate",
        ));
        assert!(matches!(mismatch, ClientError::Mismatch(_)));
    }

    #[test]
    fn contract_replies_keep_their_kind_and_message() {
        let error = ClientError::from(method_error(
            "io.github.apex_linux.Apex.Update1.Error.PolicyDenied",
            "On battery below 50 %",
        ));
        assert_eq!(error.exit(), Exit::PolicyDenied);
        assert_eq!(error.to_string(), "not now: On battery below 50 %");
        let nothing = ClientError::from(method_error(
            "io.github.apex_linux.Apex.Update1.Error.NothingToDo",
            "Nothing is staged.",
        ));
        assert_eq!(nothing.to_string(), "Nothing is staged.");
        assert_eq!(nothing.exit(), Exit::NothingToDo);
    }

    #[test]
    fn progress_is_drawn_only_while_downloading() {
        assert_eq!(
            progress_display(State::Downloading, 0.42).as_deref(),
            Some("Downloading  42 %")
        );
        assert_eq!(
            progress_display(State::Downloading, -1.0).as_deref(),
            Some("Downloading…")
        );
        for state in [
            State::Available,
            State::Staged,
            State::UpToDate,
            State::Error,
        ] {
            assert_eq!(progress_display(state, 0.5), None, "{state}");
        }
    }

    #[test]
    fn transport_errors_mean_no_bus() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no socket");
        let error = classify(&zbus::Error::InputOutput(io.into()));
        assert!(matches!(error, ClientError::NoBus(_)));
    }
}
