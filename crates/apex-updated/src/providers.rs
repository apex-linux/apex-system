use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use apex_boot::GrubEnv;
use apex_boot::grubenv::{self, ReadError as GrubEnvReadError};
use apex_boot::writer::GRUBENV_PATH;
use apex_bootc::DeploymentLocation;
use apex_update::manifest::MANIFEST_PATH;

use crate::lane_lock::LANE_LOCK_PATH;
use apex_update::{Manifest, ManifestError, Power};
use tokio::io::AsyncReadExt;
use zbus::Connection;
use zbus::OwnedObjectPath;
use zbus::proxy::CacheProperties;

use crate::BoxFuture;

pub const SYSROOT: &str = "/sysroot";
pub const BOOT: &str = "/boot";
/// Written by `ostree-finalize-staged` when finalization at shutdown failed; removed (and
/// turned into a failed unit) by `ostree-boot-complete.service` on the next boot.
pub const FINALIZE_FAILURE_STAMP: &str = "/boot/ostree/finalize-failure.stamp";
pub const BOOT_COMPLETE_UNIT: &str = "ostree-boot-complete.service";
pub const ASSESS_UNIT: &str = "apex-boot-assess.service";
/// The running image's own, never the one in `/etc`.
pub const OSTREE_PROGRAM: &str = "/usr/bin/ostree";
pub const OSTREE_REPO: &str = "/sysroot/ostree/repo";
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
pub const OSTREE_CAT_TIMEOUT: Duration = Duration::from_secs(30);
/// A full desktop image's manifest is about 200 kB.
pub const MANIFEST_MAX_SIZE: u64 = 16 * 1024 * 1024;
const OSTREE_STDERR_MAX_SIZE: u64 = 64 * 1024;

pub trait Clock: Send + Sync {
    /// Unix seconds.
    fn now(&self) -> u64;
}

pub trait PowerSource: Send + Sync {
    fn power(&self) -> BoxFuture<'_, Power>;
}

pub trait MeteredSource: Send + Sync {
    fn metered(&self) -> BoxFuture<'_, Option<bool>>;
}

pub trait DiskSpace: Send + Sync {
    /// Bytes available to an unprivileged writer on the filesystem holding `path`.
    fn free_bytes(&self, path: &Path) -> Option<u64>;
}

/// Size of what finalization copies to `/boot`.
pub trait BootPayload: Send + Sync {
    fn boot_payload(&self, root: &Path) -> Option<u64>;
}

pub trait UnitStates: Send + Sync {
    fn active_state<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, Option<String>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestSource {
    Running,
    Deployment(DeploymentLocation),
}

pub trait DeploymentFiles: Send + Sync {
    fn manifest<'a>(
        &'a self,
        source: &'a ManifestSource,
    ) -> BoxFuture<'a, Result<Option<Manifest>, FileError>>;

    /// The image's lane lock (`/usr/share/apex/rpms.lock`), when it has one.
    fn lane_lock<'a>(
        &'a self,
        source: &'a ManifestSource,
    ) -> BoxFuture<'a, Result<Option<String>, FileError>>;

    fn finalize_failure_stamp(&self) -> bool;
}

#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("cannot read {}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is larger than {limit} bytes", .path.display())]
    TooLarge { path: PathBuf, limit: u64 },
    #[error("{} is not a valid image manifest", .path.display())]
    Manifest {
        path: PathBuf,
        #[source]
        source: ManifestError,
    },
    #[error("`ostree cat` failed: {detail}")]
    Ostree { detail: String },
}

pub trait GrubEnvSource: Send + Sync {
    /// `Ok(None)` when the file does not exist: GRUB then has no variables.
    fn read(&self) -> Result<Option<GrubEnv>, GrubEnvReadError>;
}

#[derive(Debug, Clone)]
pub struct GrubEnvFile {
    path: PathBuf,
}

impl GrubEnvFile {
    pub fn system() -> Self {
        Self::at(GRUBENV_PATH)
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl GrubEnvSource for GrubEnvFile {
    fn read(&self) -> Result<Option<GrubEnv>, GrubEnvReadError> {
        match grubenv::read_file(&self.path) {
            Ok(env) => Ok(Some(env)),
            Err(GrubEnvReadError::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

#[derive(Clone)]
pub struct Providers {
    pub clock: Arc<dyn Clock>,
    pub power: Arc<dyn PowerSource>,
    pub metered: Arc<dyn MeteredSource>,
    pub space: Arc<dyn DiskSpace>,
    pub payload: Arc<dyn BootPayload>,
    pub units: Arc<dyn UnitStates>,
    pub files: Arc<dyn DeploymentFiles>,
}

impl fmt::Debug for Providers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Providers").finish_non_exhaustive()
    }
}

impl Providers {
    pub fn system(connection: &Connection) -> Self {
        Self {
            clock: Arc::new(SystemClock),
            power: Arc::new(UPower::new(connection.clone())),
            metered: Arc::new(NetworkManager::new(connection.clone())),
            space: Arc::new(Statvfs),
            payload: Arc::new(KernelFiles),
            units: Arc::new(Systemd::new(connection.clone())),
            files: Arc::new(HostFiles::system()),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }
}

/// `battery` is the display device's `(IsPresent, Percentage)`.
pub fn power_from_upower(on_battery: bool, battery: Option<(bool, f64)>) -> Power {
    let battery_percent = battery.and_then(|(present, percent)| {
        (present && (0.0..=100.0).contains(&percent)).then(|| percent.round() as u8)
    });
    Power {
        on_ac: !on_battery,
        battery_percent,
    }
}

const NM_METERED_YES: u32 = 1;
const NM_METERED_NO: u32 = 2;
const NM_METERED_GUESS_YES: u32 = 3;
const NM_METERED_GUESS_NO: u32 = 4;

pub fn metered_from_nm(value: u32) -> Option<bool> {
    match value {
        NM_METERED_YES | NM_METERED_GUESS_YES => Some(true),
        NM_METERED_NO | NM_METERED_GUESS_NO => Some(false),
        _ => None,
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
trait UPowerManager {
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/devices/DisplayDevice"
)]
trait UPowerDevice {
    #[zbus(property)]
    fn is_present(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<f64>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManagerRoot {
    #[zbus(property)]
    fn metered(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait SystemdManager {
    fn get_unit(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1"
)]
trait SystemdUnit {
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
}

#[derive(Debug, Clone)]
pub struct UPower {
    connection: Connection,
}

impl UPower {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }

    async fn read(&self) -> zbus::Result<Power> {
        let manager = UPowerManagerProxy::builder(&self.connection)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let on_battery = manager.on_battery().await?;
        let device = UPowerDeviceProxy::builder(&self.connection)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        // A missing display device only loses the percentage.
        let battery = match (device.is_present().await, device.percentage().await) {
            (Ok(present), Ok(percent)) => Some((present, percent)),
            _ => None,
        };
        Ok(power_from_upower(on_battery, battery))
    }
}

impl PowerSource for UPower {
    fn power(&self) -> BoxFuture<'_, Power> {
        Box::pin(async move {
            match tokio::time::timeout(PROBE_TIMEOUT, self.read()).await {
                Ok(Ok(power)) => power,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "cannot read the power state from UPower");
                    Power::default()
                }
                Err(_) => {
                    tracing::warn!("UPower did not answer in time");
                    Power::default()
                }
            }
        })
    }
}

#[derive(Debug, Clone)]
pub struct NetworkManager {
    connection: Connection,
}

impl NetworkManager {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }

    async fn read(&self) -> zbus::Result<u32> {
        NetworkManagerRootProxy::builder(&self.connection)
            .cache_properties(CacheProperties::No)
            .build()
            .await?
            .metered()
            .await
    }
}

impl MeteredSource for NetworkManager {
    fn metered(&self) -> BoxFuture<'_, Option<bool>> {
        Box::pin(async move {
            match tokio::time::timeout(PROBE_TIMEOUT, self.read()).await {
                Ok(Ok(value)) => metered_from_nm(value),
                Ok(Err(error)) => {
                    tracing::debug!(%error, "cannot read the metered flag from NetworkManager");
                    None
                }
                Err(_) => None,
            }
        })
    }
}

#[derive(Debug, Clone)]
pub struct Systemd {
    connection: Connection,
}

impl Systemd {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }

    async fn read(&self, unit: &str) -> zbus::Result<String> {
        let path = SystemdManagerProxy::builder(&self.connection)
            .cache_properties(CacheProperties::No)
            .build()
            .await?
            .get_unit(unit)
            .await?;
        SystemdUnitProxy::builder(&self.connection)
            .path(path)
            .cache_properties(CacheProperties::No)
            .build()
            .await?
            .active_state()
            .await
    }
}

impl UnitStates for Systemd {
    fn active_state<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            match tokio::time::timeout(PROBE_TIMEOUT, self.read(unit)).await {
                Ok(Ok(state)) => Some(state),
                // `NoSuchUnit` is the common case: units that did not run are not loaded.
                Ok(Err(error)) => {
                    tracing::debug!(unit, %error, "unit state unavailable");
                    None
                }
                Err(_) => None,
            }
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Statvfs;

impl DiskSpace for Statvfs {
    fn free_bytes(&self, path: &Path) -> Option<u64> {
        let stat = nix::sys::statvfs::statvfs(path).ok()?;
        // f_bavail counts fragments of f_frsize bytes (statvfs(3)).
        let blocks: u64 = stat.blocks_available();
        let size: u64 = stat.fragment_size();
        Some(blocks.saturating_mul(size))
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KernelFiles;

impl BootPayload for KernelFiles {
    fn boot_payload(&self, root: &Path) -> Option<u64> {
        let modules = std::fs::read_dir(root.join("usr/lib/modules")).ok()?;
        modules
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let dir = entry.path();
                let vmlinuz = std::fs::metadata(dir.join("vmlinuz")).ok()?;
                let initramfs = std::fs::metadata(dir.join("initramfs.img")).ok()?;
                Some(vmlinuz.len().saturating_add(initramfs.len()))
            })
            .max()
    }
}

#[derive(Debug, Clone)]
pub struct HostFiles {
    root: PathBuf,
    sysroot: PathBuf,
    finalize_stamp: PathBuf,
    ostree: PathBuf,
    ostree_repo: PathBuf,
}

impl HostFiles {
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/"),
            sysroot: PathBuf::from(SYSROOT),
            finalize_stamp: PathBuf::from(FINALIZE_FAILURE_STAMP),
            ostree: PathBuf::from(OSTREE_PROGRAM),
            ostree_repo: PathBuf::from(OSTREE_REPO),
        }
    }

    pub fn under(root: &Path, sysroot: &Path, ostree: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            sysroot: sysroot.to_path_buf(),
            finalize_stamp: root.join("boot/ostree/finalize-failure.stamp"),
            ostree: ostree.to_path_buf(),
            ostree_repo: sysroot.join("ostree/repo"),
        }
    }

    async fn read_file(
        &self,
        source: &ManifestSource,
        absolute: &str,
    ) -> Result<(PathBuf, Option<Vec<u8>>), FileError> {
        let relative = absolute.trim_start_matches('/');
        Ok(match source {
            ManifestSource::Running => {
                let path = self.root.join(relative);
                let bytes = read_capped(&path)?;
                (path, bytes)
            }
            ManifestSource::Deployment(location) => {
                let path = location.root_under(&self.sysroot).join(relative);
                match read_capped(&path)? {
                    Some(bytes) => (path, Some(bytes)),
                    None => (path, self.ostree_cat(location.checksum(), absolute).await?),
                }
            }
        })
    }

    async fn read_manifest(&self, source: &ManifestSource) -> Result<Option<Manifest>, FileError> {
        let (path, bytes) = self.read_file(source, MANIFEST_PATH).await?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        Manifest::from_json(&bytes)
            .map(Some)
            .map_err(|source| FileError::Manifest { path, source })
    }

    async fn read_lane_lock(&self, source: &ManifestSource) -> Result<Option<String>, FileError> {
        let (_, bytes) = self.read_file(source, LANE_LOCK_PATH).await?;
        Ok(bytes.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// [`DeploymentLocation`] validated the checksum as hex, so ostree cannot read it as an
    /// option.
    async fn ostree_cat(&self, checksum: &str, path: &str) -> Result<Option<Vec<u8>>, FileError> {
        let mut repo = std::ffi::OsString::from("--repo=");
        repo.push(&self.ostree_repo);
        let mut child = tokio::process::Command::new(&self.ostree)
            .arg("cat")
            .arg(repo)
            .arg(checksum)
            .arg(path)
            .env_clear()
            .env("PATH", "/usr/bin:/usr/sbin")
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| FileError::Ostree {
                detail: format!("cannot run {}: {e}", self.ostree.display()),
            })?;
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let run = async {
            let mut out = Vec::new();
            let mut err = Vec::new();
            // Both pipes at once, so a chatty stderr cannot block ostree on a full pipe.
            let read_out = async {
                match stdout.as_mut() {
                    Some(pipe) => pipe
                        .take(MANIFEST_MAX_SIZE + 1)
                        .read_to_end(&mut out)
                        .await
                        .map(drop),
                    None => Ok(()),
                }
            };
            let read_err = async {
                match stderr.as_mut() {
                    Some(pipe) => pipe
                        .take(OSTREE_STDERR_MAX_SIZE)
                        .read_to_end(&mut err)
                        .await
                        .map(drop),
                    None => Ok(()),
                }
            };
            let (read_out, read_err) = tokio::join!(read_out, read_err);
            read_out?;
            read_err?;
            let status = child.wait().await?;
            Ok::<_, io::Error>((status, out, err))
        };
        let (status, out, err) = tokio::time::timeout(OSTREE_CAT_TIMEOUT, run)
            .await
            .map_err(|_| FileError::Ostree {
                detail: "timed out".to_owned(),
            })?
            .map_err(|e| FileError::Ostree {
                detail: e.to_string(),
            })?;
        if out.len() as u64 > MANIFEST_MAX_SIZE {
            return Err(FileError::TooLarge {
                path: PathBuf::from(path),
                limit: MANIFEST_MAX_SIZE,
            });
        }
        if status.success() {
            return Ok(Some(out));
        }
        let stderr = String::from_utf8_lossy(&err);
        if stderr.contains("No such file") || stderr.contains("not found") {
            Ok(None)
        } else {
            Err(FileError::Ostree {
                detail: format!("{status}: {}", stderr.trim()),
            })
        }
    }
}

fn read_capped(path: &Path) -> Result<Option<Vec<u8>>, FileError> {
    let io_error = |source| FileError::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(path)
    {
        Ok(file) => file,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(io_error(e)),
    };
    let mut bytes = Vec::new();
    file.take(MANIFEST_MAX_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MANIFEST_MAX_SIZE {
        return Err(FileError::TooLarge {
            path: path.to_path_buf(),
            limit: MANIFEST_MAX_SIZE,
        });
    }
    Ok(Some(bytes))
}

impl DeploymentFiles for HostFiles {
    fn manifest<'a>(
        &'a self,
        source: &'a ManifestSource,
    ) -> BoxFuture<'a, Result<Option<Manifest>, FileError>> {
        Box::pin(self.read_manifest(source))
    }

    fn lane_lock<'a>(
        &'a self,
        source: &'a ManifestSource,
    ) -> BoxFuture<'a, Result<Option<String>, FileError>> {
        Box::pin(self.read_lane_lock(source))
    }

    fn finalize_failure_stamp(&self) -> bool {
        self.finalize_stamp.try_exists().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const CSUM: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

    fn manifest_json(version: &str) -> String {
        format!(
            r#"{{"schema":1,"name":"apex","version":"{version}","fedora":45,"degraded":["audio"],"packages":[]}}"#
        )
    }

    #[test]
    fn upower_mapping() {
        assert_eq!(
            power_from_upower(false, Some((true, 42.4))),
            Power {
                on_ac: true,
                battery_percent: Some(42)
            }
        );
        assert_eq!(
            power_from_upower(true, Some((true, 79.6))),
            Power {
                on_ac: false,
                battery_percent: Some(80)
            }
        );
        assert_eq!(
            power_from_upower(true, Some((false, 50.0))).battery_percent,
            None
        );
        assert_eq!(
            power_from_upower(true, Some((true, f64::NAN))).battery_percent,
            None
        );
        assert_eq!(
            power_from_upower(true, Some((true, 101.0))).battery_percent,
            None
        );
        assert_eq!(power_from_upower(true, None).battery_percent, None);
    }

    #[test]
    fn metered_mapping() {
        assert_eq!(metered_from_nm(0), None);
        assert_eq!(metered_from_nm(1), Some(true));
        assert_eq!(metered_from_nm(2), Some(false));
        assert_eq!(metered_from_nm(3), Some(true));
        assert_eq!(metered_from_nm(4), Some(false));
        assert_eq!(metered_from_nm(99), None);
    }

    proptest! {
        #[test]
        fn upower_percent_is_bounded(on_battery: bool, present: bool, percent in any::<f64>()) {
            let power = power_from_upower(on_battery, Some((present, percent)));
            prop_assert_eq!(power.on_ac, !on_battery);
            if let Some(p) = power.battery_percent {
                prop_assert!(p <= 100);
            }
        }
    }

    #[test]
    fn kernel_payload_takes_the_largest_complete_pair() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(KernelFiles.boot_payload(dir.path()), None);
        let modules = dir.path().join("usr/lib/modules");
        for (version, vmlinuz, initramfs) in [("6.17.1", 10, 20), ("6.17.2", 15, 30)] {
            let k = modules.join(version);
            std::fs::create_dir_all(&k).unwrap();
            std::fs::write(k.join("vmlinuz"), vec![0; vmlinuz]).unwrap();
            std::fs::write(k.join("initramfs.img"), vec![0; initramfs]).unwrap();
        }
        std::fs::create_dir_all(modules.join("incomplete")).unwrap();
        std::fs::write(modules.join("incomplete/vmlinuz"), vec![0; 1000]).unwrap();
        assert_eq!(KernelFiles.boot_payload(dir.path()), Some(45));
    }

    #[test]
    fn statvfs_reports_something_for_the_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Statvfs.free_bytes(dir.path()).is_some());
        assert_eq!(Statvfs.free_bytes(&dir.path().join("missing")), None);
    }

    fn files(dir: &Path) -> HostFiles {
        HostFiles::under(
            &dir.join("root"),
            &dir.join("sysroot"),
            &dir.join("no-ostree"),
        )
    }

    #[tokio::test]
    async fn reads_the_running_and_deployment_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(dir.path());
        assert_eq!(
            files.manifest(&ManifestSource::Running).await.unwrap(),
            None
        );

        let running = dir.path().join("root/usr/share/apex");
        std::fs::create_dir_all(&running).unwrap();
        std::fs::write(
            running.join("manifest.json"),
            manifest_json("45.20260920.3"),
        )
        .unwrap();
        let m = files
            .manifest(&ManifestSource::Running)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.version, "45.20260920.3");
        assert_eq!(m.degraded, ["audio"]);

        let location = DeploymentLocation::new("default", CSUM, 0).unwrap();
        let deployed = location
            .root_under(&dir.path().join("sysroot"))
            .join("usr/share/apex");
        std::fs::create_dir_all(&deployed).unwrap();
        std::fs::write(
            deployed.join("manifest.json"),
            manifest_json("45.20260924.1"),
        )
        .unwrap();
        let m = files
            .manifest(&ManifestSource::Deployment(location))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.version, "45.20260924.1");
    }

    #[tokio::test]
    async fn symlinks_and_garbage_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(dir.path());
        let running = dir.path().join("root/usr/share/apex");
        std::fs::create_dir_all(&running).unwrap();
        std::fs::write(dir.path().join("secret"), "root:x:0:0").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), running.join("manifest.json"))
            .unwrap();
        assert!(matches!(
            files.manifest(&ManifestSource::Running).await,
            Err(FileError::Io { .. })
        ));
        std::fs::remove_file(running.join("manifest.json")).unwrap();
        std::fs::write(running.join("manifest.json"), "not json").unwrap();
        assert!(matches!(
            files.manifest(&ManifestSource::Running).await,
            Err(FileError::Manifest { .. })
        ));
    }

    #[tokio::test]
    async fn missing_checkout_falls_back_to_ostree_cat() {
        let dir = tempfile::tempdir().unwrap();
        // A stand-in for ostree that prints a manifest for our checksum and fails otherwise.
        let script = dir.path().join("ostree");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n[ \"$1\" = cat ] || exit 9\ncase \"$2\" in --repo=*/sysroot/ostree/repo) ;; *) exit 8;; esac\n[ \"$3\" = {CSUM} ] || {{ echo \"error: No such file or directory\" >&2; exit 1; }}\n[ \"$4\" = /usr/share/apex/manifest.json ] || exit 7\nprintf '%s' '{}'\n",
                manifest_json("45.20260924.2")
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        // A test forking concurrently can hold our write descriptor until its child execs,
        // making exec fail with ETXTBSY.
        for _ in 0..200 {
            match std::process::Command::new(&script).output() {
                Err(e) if e.raw_os_error() == Some(nix::errno::Errno::ETXTBSY as i32) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                _ => break,
            }
        }

        let files = HostFiles::under(
            &dir.path().join("root"),
            &dir.path().join("sysroot"),
            &script,
        );
        let location = DeploymentLocation::new("default", CSUM, 0).unwrap();
        let m = files
            .manifest(&ManifestSource::Deployment(location))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.version, "45.20260924.2");

        let other = DeploymentLocation::new("default", &"a".repeat(64), 0).unwrap();
        assert_eq!(
            files
                .manifest(&ManifestSource::Deployment(other))
                .await
                .unwrap(),
            None
        );
    }

    #[test]
    fn finalize_failure_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(dir.path());
        assert!(!files.finalize_failure_stamp());
        std::fs::create_dir_all(dir.path().join("root/boot/ostree")).unwrap();
        std::fs::write(
            dir.path().join("root/boot/ostree/finalize-failure.stamp"),
            "x",
        )
        .unwrap();
        assert!(files.finalize_failure_stamp());
    }

    #[test]
    fn grubenv_is_read_directly_and_a_missing_file_holds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grubenv");
        let file = GrubEnvFile::at(&path);
        assert!(file.read().unwrap().is_none());

        let mut env = GrubEnv::new();
        env.set("apex_target", "sha256:aa").unwrap();
        env.set("apex_counter", "2").unwrap();
        std::fs::write(&path, env.to_bytes()).unwrap();
        let read = file.read().unwrap().expect("present");
        assert_eq!(read.get_str("apex_counter").unwrap(), Some("2"));
        assert_eq!(read, env);

        std::fs::write(&path, b"not an environment block").unwrap();
        assert!(matches!(file.read(), Err(GrubEnvReadError::Parse { .. })));
        assert_eq!(GrubEnvFile::system().path, Path::new("/boot/grub2/grubenv"));
    }
}
