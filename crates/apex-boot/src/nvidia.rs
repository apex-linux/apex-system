//! S1c's hardware report (ARCHITECTURE §5.3): whether the NVIDIA driver the image ships is
//! bound to the dGPU at its version. Reads only cached sysfs attributes, so the dGPU sleeps on.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Built in: the image does not install the profile.
pub const PROFILE: &str = include_str!("../../../hardware/m7400qc/profile.toml");

pub const MANIFEST_PATH: &str = "/usr/share/apex/manifest.json";

/// apex-image's `image/checks/nvidia.sh`: the pair's kmod is `kmod-nvidia-<kver>`.
const KMOD_PREFIX: &str = "kmod-nvidia-";

const DRIVER: &str = "nvidia";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DiscreteGpu {
    pub pci: String,
    pub vendor: u16,
    pub device: u16,
}

#[derive(Deserialize)]
struct Profile {
    gpu: Gpus,
}

#[derive(Deserialize)]
struct Gpus {
    discrete: DiscreteGpu,
}

impl DiscreteGpu {
    pub fn from_profile(text: &str) -> Result<Self, String> {
        toml::from_str::<Profile>(text)
            .map(|profile| profile.gpu.discrete)
            .map_err(|error| format!("the hardware profile has no usable [gpu.discrete]: {error}"))
    }

    pub fn of_this_machine() -> Result<Self, String> {
        Self::from_profile(PROFILE)
    }
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    degraded: Vec<String>,
    #[serde(default)]
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NvidiaReport {
    /// The image ships no NVIDIA set and does not claim one.
    NotShipped,
    Degraded,
    Ready {
        version: String,
    },
    NoDevice {
        gpu: DiscreteGpu,
    },
    /// `driver` is `None` when nothing is bound.
    NotBound {
        gpu: DiscreteGpu,
        driver: Option<String>,
    },
    /// `loaded` is `None` when the module is not loaded.
    WrongVersion {
        shipped: String,
        loaded: Option<String>,
    },
    Unreadable(String),
}

impl NvidiaReport {
    /// What the boot assessment logs as a pass: nothing is wrong with what the image ships.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::NotShipped | Self::Ready { .. })
    }
}

impl fmt::Display for NvidiaReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotShipped => f.write_str("the image ships no NVIDIA driver"),
            Self::Degraded => f.write_str("no NVIDIA driver (degraded)"),
            Self::Ready { version } => {
                write!(
                    f,
                    "NVIDIA driver {version} is bound to the dGPU, as the image ships it"
                )
            }
            Self::NoDevice { gpu } => write!(
                f,
                "no dGPU {:04x}:{:04x} at {}",
                gpu.vendor, gpu.device, gpu.pci
            ),
            Self::NotBound { gpu, driver } => write!(
                f,
                "the dGPU {:04x}:{:04x} is bound to {}, not nvidia",
                gpu.vendor,
                gpu.device,
                driver.as_deref().unwrap_or("no driver")
            ),
            Self::WrongVersion { shipped, loaded } => match loaded {
                Some(loaded) => write!(
                    f,
                    "the loaded NVIDIA driver is {loaded}, but the image ships {shipped}"
                ),
                None => write!(
                    f,
                    "the NVIDIA module is not loaded, but the image ships {shipped}"
                ),
            },
            Self::Unreadable(why) => f.write_str(why),
        }
    }
}

fn under(root: &Path, absolute: &str) -> PathBuf {
    root.join(absolute.trim_start_matches('/'))
}

fn read_trimmed(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn hex(text: &str) -> Option<u16> {
    u16::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
}

/// Everything is read below `root`, which is `/` on the machine.
pub fn report(root: &Path, gpu: &DiscreteGpu) -> NvidiaReport {
    let manifest_path = under(root, MANIFEST_PATH);
    let manifest: Manifest = match fs::read(&manifest_path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                return NvidiaReport::Unreadable(format!(
                    "{} is not valid: {error}",
                    manifest_path.display()
                ));
            }
        },
        Err(error) => {
            return NvidiaReport::Unreadable(format!(
                "cannot read {}: {error}",
                manifest_path.display()
            ));
        }
    };
    if manifest
        .degraded
        .iter()
        .any(|component| component == DRIVER)
    {
        return NvidiaReport::Degraded;
    }
    let Some(shipped) = manifest
        .packages
        .iter()
        .find(|package| package.name.starts_with(KMOD_PREFIX))
        .map(|package| package.version.clone())
    else {
        return NvidiaReport::NotShipped;
    };
    let device = under(root, &format!("/sys/bus/pci/devices/{}", gpu.pci));
    let id = |name: &str| {
        read_trimmed(&device.join(name))
            .ok()
            .flatten()
            .and_then(|text| hex(&text))
    };
    if id("vendor") != Some(gpu.vendor) || id("device") != Some(gpu.device) {
        return NvidiaReport::NoDevice { gpu: gpu.clone() };
    }
    let driver = fs::read_link(device.join("driver"))
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
    if driver.as_deref() != Some(DRIVER) {
        return NvidiaReport::NotBound {
            gpu: gpu.clone(),
            driver,
        };
    }
    let version_path = under(root, "/sys/module/nvidia/version");
    match read_trimmed(&version_path) {
        Ok(Some(loaded)) if loaded == shipped => NvidiaReport::Ready { version: shipped },
        Ok(loaded) => NvidiaReport::WrongVersion { shipped, loaded },
        Err(error) => {
            NvidiaReport::Unreadable(format!("cannot read {}: {error}", version_path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED: &str = "615.78.08";

    fn gpu() -> DiscreteGpu {
        DiscreteGpu::of_this_machine().unwrap()
    }

    struct FakeRoot(tempfile::TempDir);

    impl FakeRoot {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }

        fn write(&self, absolute: &str, text: &str) -> &Self {
            let path = under(self.0.path(), absolute);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
            self
        }

        fn manifest(&self, degraded: &[&str], kmod: Option<&str>) -> &Self {
            let packages: Vec<serde_json::Value> = kmod
                .map(|version| {
                    serde_json::json!({
                        "name": "kmod-nvidia-7.3.0-4.cachyos1.fc45.x86_64",
                        "epoch": 3,
                        "version": version,
                        "release": "1.fc45",
                        "arch": "x86_64",
                    })
                })
                .into_iter()
                .chain([serde_json::json!({"name": "bash", "version": "5.3.0"})])
                .collect();
            let manifest = serde_json::json!({
                "schema": 1,
                "degraded": degraded,
                "packages": packages,
            });
            self.write(MANIFEST_PATH, &manifest.to_string())
        }

        fn dgpu(&self, vendor: &str, device: &str, driver: Option<&str>) -> &Self {
            let dir = format!("/sys/bus/pci/devices/{}", gpu().pci);
            self.write(&format!("{dir}/vendor"), &format!("{vendor}\n"));
            self.write(&format!("{dir}/device"), &format!("{device}\n"));
            if let Some(driver) = driver {
                let target = under(self.0.path(), &format!("/sys/bus/pci/drivers/{driver}"));
                fs::create_dir_all(&target).unwrap();
                std::os::unix::fs::symlink(target, under(self.0.path(), &format!("{dir}/driver")))
                    .unwrap();
            }
            self
        }

        fn report(&self) -> NvidiaReport {
            report(self.0.path(), &gpu())
        }
    }

    #[test]
    fn the_profile_names_the_dgpu() {
        assert_eq!(
            gpu(),
            DiscreteGpu {
                pci: "0000:01:00.0".to_owned(),
                vendor: 0x10de,
                device: 0x25a2,
            }
        );
        assert!(DiscreteGpu::from_profile("[gpu]\n").is_err());
    }

    #[test]
    fn a_bound_driver_at_the_shipped_version_is_ready() {
        let root = FakeRoot::new();
        root.manifest(&["audio"], Some(SHIPPED))
            .dgpu("0x10de", "0x25a2", Some("nvidia"))
            .write("/sys/module/nvidia/version", "615.78.08\n");
        let report = root.report();
        assert_eq!(
            report,
            NvidiaReport::Ready {
                version: SHIPPED.to_owned()
            }
        );
        assert!(report.is_ok());
        assert_eq!(
            report.to_string(),
            "NVIDIA driver 615.78.08 is bound to the dGPU, as the image ships it"
        );
    }

    #[test]
    fn a_degraded_image_says_so_without_looking_at_the_device() {
        let root = FakeRoot::new();
        root.manifest(&["nvidia"], None);
        let report = root.report();
        assert_eq!(report, NvidiaReport::Degraded);
        assert!(!report.is_ok());
        assert_eq!(report.to_string(), "no NVIDIA driver (degraded)");
    }

    #[test]
    fn an_image_without_the_set_has_nothing_to_check() {
        let root = FakeRoot::new();
        root.manifest(&[], None);
        assert_eq!(root.report(), NvidiaReport::NotShipped);
        assert!(root.report().is_ok());
    }

    #[test]
    fn the_wrong_driver_or_none_is_named() {
        let root = FakeRoot::new();
        root.manifest(&[], Some(SHIPPED))
            .dgpu("0x10de", "0x25a2", Some("nouveau"));
        assert_eq!(
            root.report().to_string(),
            "the dGPU 10de:25a2 is bound to nouveau, not nvidia"
        );
        let root = FakeRoot::new();
        root.manifest(&[], Some(SHIPPED))
            .dgpu("0x10de", "0x25a2", None);
        assert_eq!(
            root.report().to_string(),
            "the dGPU 10de:25a2 is bound to no driver, not nvidia"
        );
    }

    #[test]
    fn another_device_or_none_at_the_address_is_no_device() {
        let root = FakeRoot::new();
        root.manifest(&[], Some(SHIPPED));
        assert_eq!(
            root.report().to_string(),
            "no dGPU 10de:25a2 at 0000:01:00.0"
        );
        root.dgpu("0x10de", "0x2520", Some("nvidia"));
        assert!(matches!(root.report(), NvidiaReport::NoDevice { .. }));
    }

    #[test]
    fn a_version_other_than_the_shipped_one_is_named() {
        let root = FakeRoot::new();
        root.manifest(&[], Some(SHIPPED))
            .dgpu("0x10de", "0x25a2", Some("nvidia"));
        assert_eq!(
            root.report().to_string(),
            "the NVIDIA module is not loaded, but the image ships 615.78.08"
        );
        root.write("/sys/module/nvidia/version", "615.80.01\n");
        assert_eq!(
            root.report().to_string(),
            "the loaded NVIDIA driver is 615.80.01, but the image ships 615.78.08"
        );
    }

    #[test]
    fn a_missing_or_broken_manifest_is_unreadable() {
        let root = FakeRoot::new();
        assert!(matches!(root.report(), NvidiaReport::Unreadable(_)));
        root.write(MANIFEST_PATH, "{");
        assert!(root.report().to_string().contains("is not valid"));
    }
}
