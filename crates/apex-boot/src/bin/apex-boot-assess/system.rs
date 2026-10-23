//! The checks run on plain threads and zbus and apex-bootc on tokio: each call here is
//! `Handle::block_on` from a non-runtime thread, while the main thread drives the runtime.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use apex_boot::checks::units::SYSTEMCTL;
use apex_boot::{
    BOOTC_STATUS_BOUND, DAEMON_QUERY_TIMEOUT, Digest, KILL_REAP_GRACE, LOGIND_TIMEOUT,
    REBOOT_TIMEOUT, REPORT_TIMEOUT, Verdict,
};
use apex_bootc::{Bootc, CancelToken, Host, TokioRunner};
use apex_dbus::{ApexError, BootVerdict, Update1Proxy};
use nix::time::{ClockId, clock_gettime};
use tokio::runtime::Handle;
use tokio::sync::OnceCell;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;

use crate::ports::{BootedImage, Clock, HostStatus, Rebooter, SessionInfo, Sessions, Updater};

/// A failed connection is not cached: the session wait polls for two minutes, and the bus
/// may come back.
#[derive(Debug)]
pub(crate) struct Bus {
    handle: Handle,
    connection: OnceCell<zbus::Connection>,
}

impl Bus {
    pub(crate) fn new(handle: Handle) -> Self {
        Self {
            handle,
            connection: OnceCell::new(),
        }
    }

    /// Blocks the calling thread, which must not be a runtime thread.
    fn call<T, E, F>(&self, what: &str, timeout: Duration, call: F) -> Result<T, String>
    where
        E: Into<ApexError>,
        F: AsyncFnOnce(zbus::Connection) -> Result<T, E>,
    {
        self.handle.block_on(async {
            let work = async {
                let connection = self
                    .connection
                    .get_or_try_init(zbus::Connection::system)
                    .await
                    .map_err(|err| format!("cannot connect to the system bus: {err}"))?
                    .clone();
                call(connection)
                    .await
                    .map_err(|err| format!("{what}: {}", describe(&err.into())))
            };
            tokio::time::timeout(timeout, work)
                .await
                .unwrap_or_else(|_| Err(format!("{what} timed out after {} s", timeout.as_secs())))
        })
    }
}

/// zbus's own text names the remote D-Bus error, which `ApexError`'s `Display` shortens to
/// "FDO error".
fn describe(err: &ApexError) -> String {
    match err {
        ApexError::ZBus(err) => err.to_string(),
        contract => contract.to_string(),
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    gen_blocking = false
)]
trait LoginManager {
    fn list_sessions(&self) -> zbus::Result<Vec<(String, u32, String, String, OwnedObjectPath)>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    gen_blocking = false
)]
trait LoginSession {
    #[zbus(property)]
    fn class(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn timestamp_monotonic(&self) -> zbus::Result<u64>;
}

#[derive(Debug, Clone)]
pub(crate) struct Logind {
    bus: Arc<Bus>,
}

impl Logind {
    pub(crate) fn new(bus: Arc<Bus>) -> Self {
        Self { bus }
    }
}

impl Sessions for Logind {
    fn list(&self) -> Result<Vec<SessionInfo>, String> {
        self.bus.call("logind", LOGIND_TIMEOUT, async |connection| {
            let manager = LoginManagerProxy::new(&connection).await?;
            let listed = manager.list_sessions().await?;
            let mut sessions = Vec::with_capacity(listed.len());
            for (id, _uid, _user, seat, path) in listed {
                let session = LoginSessionProxy::builder(&connection)
                    .path(path)?
                    .cache_properties(CacheProperties::No)
                    .build()
                    .await?;
                let (Ok(class), Ok(state), Ok(created)) = (
                    session.class().await,
                    session.state().await,
                    session.timestamp_monotonic().await,
                ) else {
                    sessions.push(SessionInfo {
                        id,
                        class: String::new(),
                        state: String::new(),
                        seat,
                        age: None,
                    });
                    continue;
                };
                sessions.push(SessionInfo {
                    id,
                    class,
                    state,
                    seat,
                    age: session_age(created, monotonic_now()),
                });
            }
            Ok::<_, zbus::Error>(sessions)
        })
    }
}

/// logind's realtime `Timestamp` jumps when NTP sets the clock during boot, which could make
/// a new session look old.
fn monotonic_now() -> Option<Duration> {
    clock_gettime(ClockId::CLOCK_MONOTONIC)
        .ok()
        .map(Duration::from)
}

/// `created_us` is 0 when unknown.
fn session_age(created_us: u64, now: Option<Duration>) -> Option<Duration> {
    (created_us > 0).then_some(())?;
    Some(now?.saturating_sub(Duration::from_micros(created_us)))
}

#[derive(Debug, Clone)]
pub(crate) struct Daemon {
    bus: Arc<Bus>,
}

impl Daemon {
    pub(crate) fn new(bus: Arc<Bus>) -> Self {
        Self { bus }
    }
}

async fn update1(connection: &zbus::Connection) -> zbus::Result<Update1Proxy<'static>> {
    Update1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

pub(crate) fn to_dbus(verdict: Verdict) -> BootVerdict {
    match verdict {
        Verdict::Green => BootVerdict::Green,
        Verdict::Red => BootVerdict::Red,
        Verdict::RedUnarmed => BootVerdict::RedUnarmed,
        Verdict::Fallback => BootVerdict::Fallback,
    }
}

impl Updater for Daemon {
    fn ping(&self, timeout: Duration) -> Result<(), String> {
        self.bus.call("Ping", timeout, async |connection| {
            update1(&connection).await?.ping().await
        })
    }

    fn report_boot(&self, verdict: Verdict, digest: Option<&Digest>) -> Result<(), String> {
        let digest = digest.map_or("", Digest::as_str);
        self.bus
            .call("ReportBoot", REPORT_TIMEOUT, async |connection| {
                update1(&connection)
                    .await?
                    .report_boot(to_dbus(verdict), digest)
                    .await
            })
    }

    fn booted_digest(&self) -> Result<Option<String>, String> {
        self.bus
            .call("Booted", DAEMON_QUERY_TIMEOUT, async |connection| {
                let booted = update1(&connection).await?.booted().await?;
                Ok::<_, ApexError>(booted.digest)
            })
    }
}

/// bootc takes the ostree sysroot lock even for `status`, remounting `/sysroot` and `/boot`
/// read-write in the unit's private mount namespace, never the host's.
#[derive(Debug, Clone)]
pub(crate) struct BootcStatus {
    handle: Handle,
}

impl BootcStatus {
    pub(crate) fn new(handle: Handle) -> Self {
        Self { handle }
    }
}

impl HostStatus for BootcStatus {
    fn booted(&self) -> Result<BootedImage, String> {
        self.handle
            .block_on(bounded("bootc status", BOOTC_STATUS_BOUND, async {
                let bootc = Bootc::new(TokioRunner::new());
                let host = bootc
                    .status(&CancelToken::new())
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(booted_image(&host))
            }))
    }
}

async fn bounded<T>(
    what: &str,
    bound: Duration,
    work: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(bound, work).await.unwrap_or_else(|_| {
        Err(format!(
            "{what} did not finish within {} s",
            bound.as_secs()
        ))
    })
}

pub(crate) fn booted_image(host: &Host) -> BootedImage {
    let image = host.booted().and_then(|entry| entry.image.as_ref());
    BootedImage {
        digest: host.booted_digest().map(str::to_owned),
        image: image.map(|i| i.image.image.clone()),
        transport: image.map(|i| i.image.transport.clone()),
        signature: image.and_then(|i| i.image.signature.clone()),
        rollback_digest: host.rollback_digest().map(str::to_owned),
        rollback_queued: host.rollback_queued(),
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Systemctl;

impl Rebooter for Systemctl {
    fn reboot(&self) -> Result<(), String> {
        let mut child = Command::new(SYSTEMCTL)
            .arg("reboot")
            .env_clear()
            .env("PATH", "/usr/bin:/usr/sbin")
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null())
            .spawn()
            .map_err(|err| format!("cannot run {SYSTEMCTL} reboot: {err}"))?;
        let deadline = Instant::now() + REBOOT_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => return Err(format!("{SYSTEMCTL} reboot failed ({status})")),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Ok(None) => {
                    let _ = child.kill();
                    let reap_by = Instant::now() + KILL_REAP_GRACE;
                    while matches!(child.try_wait(), Ok(None)) && Instant::now() < reap_by {
                        thread::sleep(Duration::from_millis(50));
                    }
                    return Err(format!(
                        "{SYSTEMCTL} reboot did not finish within {} s",
                        REBOOT_TIMEOUT.as_secs()
                    ));
                }
                Err(err) => return Err(format!("cannot wait for {SYSTEMCTL} reboot: {err}")),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }

    fn unix_now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_bootc::ImageSignature;

    #[test]
    fn errors_name_the_remote_error() {
        let unknown = ApexError::ZBus(zbus::Error::FDO(Box::new(
            zbus::fdo::Error::ServiceUnknown("The name is not activatable".to_owned()),
        )));
        assert_eq!(
            describe(&unknown),
            "org.freedesktop.DBus.Error.ServiceUnknown: The name is not activatable"
        );
        let busy = ApexError::Busy("downloading".to_owned());
        assert_eq!(
            describe(&busy),
            "io.github.apex_linux.Apex.Update1.Error.Busy: downloading"
        );
    }

    #[test]
    fn verdicts_keep_their_contract_spelling() {
        for verdict in Verdict::ALL {
            assert_eq!(to_dbus(verdict).as_str(), verdict.as_str());
        }
    }

    #[test]
    fn session_age_uses_the_monotonic_clock() {
        let now = Some(Duration::from_secs(100));
        assert_eq!(session_age(70_000_000, now), Some(Duration::from_secs(30)));
        assert_eq!(session_age(0, now), None);
        assert_eq!(session_age(70_000_000, None), None);
        assert_eq!(session_age(200_000_000, now), Some(Duration::ZERO));
        assert!(monotonic_now().is_some());
    }

    #[test]
    fn a_hanging_call_is_abandoned_at_its_bound() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let started = Instant::now();
        let result: Result<(), String> = runtime.block_on(bounded(
            "bootc status",
            Duration::from_millis(50),
            std::future::pending(),
        ));
        assert_eq!(
            result,
            Err("bootc status did not finish within 0 s".to_owned())
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let done: Result<u8, String> =
            runtime.block_on(bounded("x", Duration::from_secs(5), async { Ok(7) }));
        assert_eq!(done, Ok(7));
    }

    #[test]
    fn booted_image_from_status() {
        let json = r#"{
          "apiVersion": "org.containers.bootc/v1",
          "kind": "BootcHost",
          "metadata": { "name": "host" },
          "spec": { "image": { "image": "ghcr.io/apex-linux/apex:45", "transport": "registry",
                               "signature": "containerPolicy" } },
          "status": {
            "staged": null,
            "booted": {
              "image": {
                "image": { "image": "ghcr.io/apex-linux/apex:45", "transport": "registry",
                           "signature": "containerPolicy" },
                "version": "45.20260923.114",
                "timestamp": "2026-09-23T03:10:02Z",
                "imageDigest": "sha256:9f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0",
                "architecture": "amd64"
              },
              "cachedUpdate": null, "incompatible": false, "pinned": false,
              "downloadOnly": false,
              "ostree": { "checksum": "5d2c", "deploySerial": 0, "stateroot": "default" }
            },
            "rollback": null, "rollbackQueued": false, "type": "bootcHost"
          }
        }"#;
        let host = Host::from_json(json.as_bytes()).unwrap();
        assert_eq!(
            booted_image(&host),
            BootedImage {
                digest: Some(
                    "sha256:9f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0"
                        .to_owned()
                ),
                image: Some("ghcr.io/apex-linux/apex:45".to_owned()),
                transport: Some("registry".to_owned()),
                signature: Some(ImageSignature::ContainerPolicy),
                rollback_digest: None,
                rollback_queued: false,
            }
        );
        let fell_back = r#"{
          "apiVersion": "org.containers.bootc/v1",
          "kind": "BootcHost",
          "metadata": { "name": "host" },
          "spec": {},
          "status": {
            "staged": null,
            "booted": {
              "image": {
                "image": { "image": "ghcr.io/apex-linux/apex:45", "transport": "registry" },
                "imageDigest": "sha256:9f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0"
              },
              "cachedUpdate": null, "incompatible": false, "pinned": false,
              "ostree": { "checksum": "5d2c", "deploySerial": 0, "stateroot": "default" }
            },
            "rollback": {
              "image": {
                "image": { "image": "ghcr.io/apex-linux/apex:45", "transport": "registry" },
                "imageDigest": "sha256:1111111111111111111111111111111111111111111111111111111111111111"
              },
              "cachedUpdate": null, "incompatible": false, "pinned": false,
              "ostree": { "checksum": "77aa", "deploySerial": 0, "stateroot": "default" }
            },
            "rollbackQueued": true, "type": "bootcHost"
          }
        }"#;
        let image = booted_image(&Host::from_json(fell_back.as_bytes()).unwrap());
        assert!(image.rollback_queued);
        assert_eq!(
            image.rollback_digest.as_deref(),
            Some("sha256:1111111111111111111111111111111111111111111111111111111111111111")
        );
        let empty = Host::from_json(
            br#"{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost","metadata":{},"spec":{},"status":{"booted":null}}"#,
        )
        .unwrap();
        assert_eq!(booted_image(&empty), BootedImage::default());
    }
}
