//! Facts copied from `hardware/m7400qc/profile.toml`, which the image does not install;
//! `tests/profile_repo.rs` keeps them in sync.

pub const MACHINE_ID: &str = "m7400qc";
pub const ALC294_VENDOR_ID: u32 = 0x10ec_0294;
pub const ALC294_SUBSYSTEM_ID: u32 = 0x1043_1ab2;
pub const COEF_NID: u16 = 0x20;
pub const PATCH_FIRMWARE_PATH: &str = "/usr/lib/firmware/apex/alc294-m7400qc.fw";
pub const PATCH_PARAMETER_PATH: &str = "/sys/module/snd_hda_intel/parameters/patch";
pub const BATTERY: &str = "BAT0";
pub const CHARGE_END_THRESHOLD: u8 = 80;
pub const DGPU_PCI: &str = "0000:01:00.0";
pub const DGPU_VENDOR: u16 = 0x10de;
pub const DGPU_DEVICE: u16 = 0x25a2;
pub const FINGERPRINT_VENDOR: u16 = 0x04f3;
pub const FINGERPRINT_PRODUCT: u16 = 0x0c6e;
pub const EFI_GLOBAL_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
