//! What the daemon learns from the rest of the system, each behind a trait so the engine
//! can be tested without a laptop: the clock, power (`UPower`), the metered flag
//! (`NetworkManager`), free space (`statvfs`), the kernel and initramfs size, systemd unit
//! states, and files inside deployments (the image manifest, os-release, the ostree
//! finalization stamp).
//!
//! The traits return [`BoxFuture`]s so they stay dyn-compatible: [`Providers`] holds them
//! as `Arc<dyn …>`, which keeps the engine generic over the bootc runner only.
//!
//! Every probe degrades instead of failing. Unknown power counts as "on battery, charge
//! unknown" (the power gate then defers, and a user can override it); unknown metered
//! counts as not metered (as `apex-update` specifies); unknown free space counts as zero.

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
use apex_update::{Manifest, ManifestError, Power};
use tokio::io::AsyncReadExt;
use zbus::Connection;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;

use crate::BoxFuture;
use crate::channel::os_release_version_id;

/// Where the physical root is mounted on an ostree system.
pub const SYSROOT: &str = "/sysroot";
/// The boot partition.
pub const BOOT: &str = "/boot";
/// Written by `ostree-finalize-staged` when finalization at shutdown failed; removed (and
/// turned into a failed unit) by `ostree-boot-complete.service` on the next boot.
pub const FINALIZE_FAILURE_STAMP: &str = "/boot/ostree/finalize-failure.stamp";
/// The unit that reports a failed finalization of the previous shutdown.
pub const BOOT_COMPLETE_UNIT: &str = "ostree-boot-complete.service";
/// The boot assessment; `Apply` waits until it is no longer running (see `apex_boot::decide`).
pub const ASSESS_UNIT: &str = "apex-boot-assess.service";
/// os-release of the running image (read from `/usr`, never from `/etc`).
pub const OS_RELEASE: &str = "/usr/lib/os-release";
/// The ostree executable, for the manifest fallback.
pub const OSTREE_PROGRAM: &str = "/usr/bin/ostree";
/// The system ostree repository.
pub const OSTREE_REPO: &str = "/sysroot/ostree/repo";
/// Longest a D-Bus probe (`UPower`, `NetworkManager`, systemd) may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest `ostree cat` may take.
pub const OSTREE_CAT_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest manifest read. A full desktop image lists about 2000 packages in roughly
/// 200 kB; anything much larger is not a manifest this build wrote.
pub const MANIFEST_MAX_SIZE: u64 = 16 * 1024 * 1024;

/// The current time.
pub trait Clock: Send + Sync {
    /// Seconds since the Unix epoch.
    fn now(&self) -> u64;
}

/// Power supply state.
pub trait PowerSource: Send + Sync {
    /// On AC, and the battery charge if a battery is present.
    fn power(&self) -> BoxFuture<'_, Power>;
}

/// Whether the network is metered.
pub trait MeteredSource: Send + Sync {
    /// `Some(true)` metered, `Some(false)` not metered, `None` unknown.
    fn metered(&self) -> BoxFuture<'_, Option<bool>>;
}

/// Free space of a filesystem.
pub trait DiskSpace: Send + Sync {
    /// Bytes available to an unprivileged writer on the filesystem holding `path`.
    fn free_bytes(&self, path: &Path) -> Option<u64>;
}

/// Size of what finalization copies to `/boot`.
pub trait BootPayload: Send + Sync {
    /// `vmlinuz` plus `initramfs.img` of the kernel in the tree at `root`
    /// (`<root>/usr/lib/modules/<version>/`). With several kernels, the largest pair.
    fn boot_payload(&self, root: &Path) -> Option<u64>;
}

/// systemd unit states.
pub trait UnitStates: Send + Sync {
    /// `ActiveState` of a loaded unit (`active`, `activating`, `failed`, …), `None` when
    /// the unit is not loaded or systemd cannot be asked.
    fn active_state<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, Option<String>>;
}

/// Which tree a manifest is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestSource {
    /// The running system's `/usr`.
    Running,
    /// A deployment on disk (the staged or the rollback one).
    Deployment(DeploymentLocation),
}

/// Files the daemon reads from the running image and from deployments.
pub trait DeploymentFiles: Send + Sync {
    /// The image manifest (`/usr/share/apex/manifest.json`) of `source`; `Ok(None)` when
    /// the image has none.
    fn manifest<'a>(
        &'a self,
        source: &'a ManifestSource,
    ) -> BoxFuture<'a, Result<Option<Manifest>, FileError>>;

    /// `VERSION_ID` of the running image's os-release.
    fn fedora_release(&self) -> Option<u32>;

    /// Whether [`FINALIZE_FAILURE_STAMP`] exists.
    fn finalize_failure_stamp(&self) -> bool;
}

/// Why a file inside an image could not be read.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    /// Opening or reading failed (a symbolic link at the path counts as a failure).
    #[error("cannot read {}", .path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// The file is larger than [`MANIFEST_MAX_SIZE`].
    #[error("{} is larger than {limit} bytes", .path.display())]
    TooLarge {
        /// The file.
        path: PathBuf,
        /// The limit.
        limit: u64,
    },
    /// The file is not a manifest this build understands.
    #[error("{} is not a valid image manifest", .path.display())]
    Manifest {
        /// The file.
        path: PathBuf,
        /// The parse error.
        #[source]
        source: ManifestError,
    },
    /// `ostree cat` failed.
    #[error("`ostree cat` failed: {detail}")]
    Ostree {
        /// What went wrong.
        detail: String,
    },
}

/// The GRUB environment block, as the boot counter's state is read before `Apply` arms it
/// and after a fallback (`apex_boot::check_arming`, M1 contract §5). Writes go through
/// `apex_boot::GrubEnvWriter` instead.
pub trait GrubEnvSource: Send + Sync {
    /// The block as it is now; `Ok(None)` when the file does not exist (GRUB then has no
    /// variables, and `grub2-editenv` creates it at the first write).
    fn read(&self) -> Result<Option<GrubEnv>, GrubEnvReadError>;
}

/// The GRUB environment block file, read directly as M1 contract §5 prescribes (the
/// signature line, `key=value` lines, `#` padding), with `apex_boot::grubenv`'s parser.
#[derive(Debug, Clone)]
pub struct GrubEnvFile {
    path: PathBuf,
}

impl GrubEnvFile {
    /// `/boot/grub2/grubenv` ([`GRUBENV_PATH`]).
    pub fn system() -> Self {
        Self::at(GRUBENV_PATH)
    }

    /// Another file, for tests.
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

/// Every provider the engine uses.
#[derive(Clone)]
pub struct Providers {
    /// The clock.
    pub clock: Arc<dyn Clock>,
    /// Power supply.
    pub power: Arc<dyn PowerSource>,
    /// Metered network.
    pub metered: Arc<dyn MeteredSource>,
    /// Free space.
    pub space: Arc<dyn DiskSpace>,
    /// Kernel plus initramfs size.
    pub payload: Arc<dyn BootPayload>,
    /// systemd unit states.
    pub units: Arc<dyn UnitStates>,
    /// Files in the image and in deployments.
    pub files: Arc<dyn DeploymentFiles>,
}

impl fmt::Debug for Providers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Providers").finish_non_exhaustive()
    }
}

impl Providers {
    /// The real providers, using `connection` (the system bus) for `UPower`,
    /// `NetworkManager` and systemd.
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

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }
}

/// Maps `UPower`'s `OnBattery` and its display device to [`Power`].
///
/// `battery` is the display device's `IsPresent` and `Percentage`. A percentage outside
/// 0-100 or not a number is treated as unknown.
pub fn power_from_upower(on_battery: bool, battery: Option<(bool, f64)>) -> Power {
    let battery_percent = battery.and_then(|(present, percent)| {
        (present && (0.0..=100.0).contains(&percent)).then(|| percent.round() as u8)
    });
    Power {
        on_ac: !on_battery,
        battery_percent,
    }
}

/// Maps `NetworkManager`'s `Metered` (`NMMetered`) to the engine's tri-state: `YES` (1) and
/// `GUESS_YES` (3) are metered, `NO` (2) and `GUESS_NO` (4) are not, anything else is
/// unknown.
pub fn metered_from_nm(value: u32) -> Option<bool> {
    match value {
        1 | 3 => Some(true),
        2 | 4 => Some(false),
        _ => None,
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower",
    gen_blocking = false
)]
trait UPowerManager {
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/devices/DisplayDevice",
    gen_blocking = false
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
    default_path = "/org/freedesktop/NetworkManager",
    gen_blocking = false
)]
trait NetworkManagerRoot {
    #[zbus(property)]
    fn metered(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1",
    gen_blocking = false
)]
trait SystemdManager {
    fn get_unit(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1",
    gen_blocking = false
)]
trait SystemdUnit {
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
}

/// Power from `UPower` on the system bus.
#[derive(Debug, Clone)]
pub struct UPower {
    connection: Connection,
}

impl UPower {
    /// Reads `UPower` over `connection`.
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

/// The metered flag from `NetworkManager` on the system bus.
#[derive(Debug, Clone)]
pub struct NetworkManager {
    connection: Connection,
}

impl NetworkManager {
    /// Reads `NetworkManager` over `connection`.
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

/// Unit states from systemd on the system bus.
#[derive(Debug, Clone)]
pub struct Systemd {
    connection: Connection,
}

impl Systemd {
    /// Asks systemd over `connection`.
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
            .path(path)?
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
                // Not loaded (`NoSuchUnit`) is the common case for units that did not run.
                Ok(Err(error)) => {
                    tracing::debug!(unit, %error, "unit state unavailable");
                    None
                }
                Err(_) => None,
            }
        })
    }
}

/// Free space with `statvfs(3)`.
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

/// The kernel and initramfs of a tree, from `usr/lib/modules`.
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

/// Files of the running image and of deployments under the sysroot.
#[derive(Debug, Clone)]
pub struct HostFiles {
    root: PathBuf,
    sysroot: PathBuf,
    finalize_stamp: PathBuf,
    ostree: PathBuf,
    ostree_repo: PathBuf,
}

impl HostFiles {
    /// The real paths: `/`, [`SYSROOT`], [`FINALIZE_FAILURE_STAMP`], [`OSTREE_PROGRAM`].
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/"),
            sysroot: PathBuf::from(SYSROOT),
            finalize_stamp: PathBuf::from(FINALIZE_FAILURE_STAMP),
            ostree: PathBuf::from(OSTREE_PROGRAM),
            ostree_repo: PathBuf::from(OSTREE_REPO),
        }
    }

    /// The same reader under other directories, for tests: `root` stands for `/` and
    /// `sysroot` for `/sysroot`; `ostree` is the program run for the fallback.
    pub fn under(root: &Path, sysroot: &Path, ostree: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            sysroot: sysroot.to_path_buf(),
            finalize_stamp: root.join("boot/ostree/finalize-failure.stamp"),
            ostree: ostree.to_path_buf(),
            ostree_repo: sysroot.join("ostree/repo"),
        }
    }

    async fn read_manifest(&self, source: &ManifestSource) -> Result<Option<Manifest>, FileError> {
        let relative = MANIFEST_PATH.trim_start_matches('/');
        let (path, bytes) = match source {
            ManifestSource::Running => {
                let path = self.root.join(relative);
                let bytes = read_capped(&path)?;
                (path, bytes)
            }
            ManifestSource::Deployment(location) => {
                let path = location.root_under(&self.sysroot).join(relative);
                match read_capped(&path)? {
                    Some(bytes) => (path, Some(bytes)),
                    None => (path, self.ostree_cat(location.checksum()).await?),
                }
            }
        };
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        Manifest::from_json(&bytes)
            .map(Some)
            .map_err(|source| FileError::Manifest { path, source })
    }

    /// `ostree cat --repo=<repo> <checksum> /usr/share/apex/manifest.json`, for a deployment
    /// whose checkout does not have the file. The checksum was validated (64 lowercase hex
    /// digits) by [`DeploymentLocation`], so it cannot be read as an option.
    async fn ostree_cat(&self, checksum: &str) -> Result<Option<Vec<u8>>, FileError> {
        let mut repo = std::ffi::OsString::from("--repo=");
        repo.push(&self.ostree_repo);
        let mut child = tokio::process::Command::new(&self.ostree)
            .arg("cat")
            .arg(repo)
            .arg(checksum)
            .arg(MANIFEST_PATH)
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
                    Some(pipe) => pipe.take(64 * 1024).read_to_end(&mut err).await.map(drop),
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
                path: PathBuf::from(MANIFEST_PATH),
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

/// Reads a regular file of at most [`MANIFEST_MAX_SIZE`] bytes without following a
/// symbolic link at its last component. A missing file (or a missing directory on the way)
/// is `Ok(None)`.
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

    fn fedora_release(&self) -> Option<u32> {
        let relative = OS_RELEASE.trim_start_matches('/');
        let text = std::fs::read_to_string(self.root.join(relative)).ok()?;
        os_release_version_id(&text)
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
    fn os_release_and_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(dir.path());
        assert_eq!(files.fedora_release(), None);
        assert!(!files.finalize_failure_stamp());
        std::fs::create_dir_all(dir.path().join("root/usr/lib")).unwrap();
        std::fs::write(
            dir.path().join("root/usr/lib/os-release"),
            "VERSION_ID=45\n",
        )
        .unwrap();
        assert_eq!(files.fedora_release(), Some(45));
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
