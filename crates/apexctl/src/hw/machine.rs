//! Facts about the M7400QC that the installed `apexctl` needs at run time.
//!
//! The hardware profile lives in the repository, not on the laptop, so these values are
//! compiled in. The integration test `tests/profile_repo.rs` fails when they drift from
//! `hardware/m7400qc/profile.toml` or from the renderer's file names.

/// Machine id.
pub const MACHINE_ID: &str = "m7400qc";
/// Realtek ALC294 vendor id.
pub const ALC294_VENDOR_ID: u32 = 0x10ec_0294;
/// Codec subsystem id (PCI SSID 1043:1ab2).
pub const ALC294_SUBSYSTEM_ID: u32 = 0x1043_1ab2;
/// NID of the widget that owns the COEFs.
pub const COEF_NID: u16 = 0x20;
/// Where the image installs the rendered patch firmware.
pub const PATCH_FIRMWARE_PATH: &str = "/usr/lib/firmware/apex/alc294-m7400qc.fw";
/// The `snd-hda-intel` `patch=` parameter.
pub const PATCH_PARAMETER_PATH: &str = "/sys/module/snd_hda_intel/parameters/patch";
/// The battery.
pub const BATTERY: &str = "BAT0";
/// The charge limit Apex expects, in percent.
pub const CHARGE_END_THRESHOLD: u8 = 80;
/// PCI address of the NVIDIA GA107 dGPU.
pub const DGPU_PCI: &str = "0000:01:00.0";
/// dGPU vendor id.
pub const DGPU_VENDOR: u16 = 0x10de;
/// dGPU device id.
pub const DGPU_DEVICE: u16 = 0x25a2;
/// ELAN fingerprint reader USB vendor id.
pub const FINGERPRINT_VENDOR: u16 = 0x04f3;
/// ELAN fingerprint reader USB product id.
pub const FINGERPRINT_PRODUCT: u16 = 0x0c6e;
/// EFI global variable GUID (`SecureBoot`, `SetupMode`).
pub const EFI_GLOBAL_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
