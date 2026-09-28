//! `apexctl hw report`: the hardware checks ARCHITECTURE §6 and ADR 0004 report on (never
//! rollback triggers).
//!
//! [`gather`] reads the facts (I/O through a [`SysRoot`]); [`assess`] turns them into
//! verdicts (pure); [`render`] prints them.
//!
//! # `apexctl hw report --json` (schema 1)
//!
//! ```text
//! {
//!   "schema": 1, "machine": "m7400qc",
//!   "audio":       { "status", "summary", "hwdep": "hwC1D0" | null,
//!                    "patch_firmware": "/usr/lib/firmware/apex/alc294-m7400qc.fw" | null,
//!                    "expected_verbs": ["0x20 0x500 0x001b", …], "init_verbs": [...],
//!                    "patch_parameter": "apex/alc294-m7400qc.fw,…" | null },
//!   "battery":     { "status", "summary", "battery": "BAT0", "expected_end_threshold": 80,
//!                    "end_threshold": 80 | null },
//!   "upower":      { "status", "summary", "threshold_supported": bool | null,
//!                    "threshold_enabled": bool | null, "end_threshold": 80 | null },
//!   "dgpu":        { "status", "summary", "pci": "0000:01:00.0", "present": bool,
//!                    "driver": "nvidia" | null, "runtime_status": "suspended" | null,
//!                    "power_state": "D3cold" | null, "control": "auto" | null,
//!                    "runtime_active_ms": n | null, "runtime_suspended_ms": n | null,
//!                    "suspended_percent": 97.5 | null },
//!   "fingerprint": { "status", "summary", "usb_id": "04f3:0c6e", "devices": ["3-1"] },
//!   "secure_boot": { "status", "summary", "uefi": bool, "enabled": bool | null,
//!                    "setup_mode": bool | null }
//! }
//! status = "pass" | "warn" | "fail" | "skip" | "unknown"
//! ```

use std::fmt::Write as _;

use serde::Serialize;

use super::machine::{
    ALC294_SUBSYSTEM_ID, ALC294_VENDOR_ID, BATTERY, CHARGE_END_THRESHOLD, DGPU_DEVICE, DGPU_PCI,
    DGPU_VENDOR, EFI_GLOBAL_GUID, FINGERPRINT_PRODUCT, FINGERPRINT_VENDOR, MACHINE_ID,
    PATCH_FIRMWARE_PATH, PATCH_PARAMETER_PATH,
};
use super::upower::UpowerFacts;
use super::{Attr, SysRoot, parse_hex};
use crate::hda::InitVerb;

/// Schema number of `hw report --json`.
pub const REPORT_SCHEMA: u32 = 1;

/// A check's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// As expected.
    Pass,
    /// Works, but not as Apex intends.
    Warn,
    /// Broken.
    Fail,
    /// Not applicable to this image.
    Skip,
    /// Could not be determined.
    Unknown,
}

impl Status {
    /// Upper-case label for the text report.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
            Self::Unknown => "????",
        }
    }
}

/// The ALC294 hwdep and its `init_verbs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hwdep {
    /// Name, e.g. `hwC1D0`.
    pub name: String,
    /// Contents of `init_verbs`.
    pub init_verbs: Attr,
}

/// Raw audio facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFacts {
    /// The ALC294 hwdep, if found.
    pub hwdep: Option<Hwdep>,
    /// The installed patch firmware file.
    pub firmware: Attr,
    /// `snd_hda_intel` `patch=` parameter.
    pub patch_parameter: Attr,
}

/// Raw dGPU facts (sysfs attributes of the PCI device).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DgpuFacts {
    /// The PCI device directory exists.
    pub present: bool,
    /// `vendor`.
    pub vendor: Attr,
    /// `device`.
    pub device: Attr,
    /// Bound driver name.
    pub driver: Option<String>,
    /// `power/runtime_status`.
    pub runtime_status: Attr,
    /// `power_state`.
    pub power_state: Attr,
    /// `power/control`.
    pub control: Attr,
    /// `power/runtime_active_time` (ms).
    pub active_ms: Attr,
    /// `power/runtime_suspended_time` (ms).
    pub suspended_ms: Attr,
}

/// Raw EFI facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureBootFacts {
    /// `/sys/firmware/efi` exists.
    pub uefi: bool,
    /// The `SecureBoot` variable (4 attribute bytes, then the value).
    pub secure_boot: Result<Option<Vec<u8>>, String>,
    /// The `SetupMode` variable.
    pub setup_mode: Result<Option<Vec<u8>>, String>,
}

/// Everything [`assess`] looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HwFacts {
    /// Audio.
    pub audio: AudioFacts,
    /// `charge_control_end_threshold` of the battery.
    pub end_threshold: Attr,
    /// The view of `UPower`; `None` when it could not be asked.
    pub upower: Option<UpowerFacts>,
    /// dGPU.
    pub dgpu: DgpuFacts,
    /// USB device names (`3-1`) with the fingerprint reader's ids.
    pub fingerprint_devices: Vec<String>,
    /// Secure Boot.
    pub secure_boot: SecureBootFacts,
}

/// Finds the ALC294 hwdep under `/sys/class/sound` by vendor and subsystem id.
pub fn find_alc294_hwdep(root: &SysRoot) -> Option<String> {
    root.list("/sys/class/sound").into_iter().find(|name| {
        let is_hwdep = name
            .strip_prefix("hwC")
            .and_then(|rest| rest.split_once('D'))
            .is_some_and(|(card, dev)| {
                !card.is_empty()
                    && !dev.is_empty()
                    && card.bytes().all(|b| b.is_ascii_digit())
                    && dev.bytes().all(|b| b.is_ascii_digit())
            });
        let id = |attr: &str| {
            root.read(&format!("/sys/class/sound/{name}/{attr}"))
                .ok()
                .flatten()
                .as_deref()
                .and_then(parse_hex)
        };
        is_hwdep
            && id("vendor_id") == Some(ALC294_VENDOR_ID)
            && id("subsystem_id") == Some(ALC294_SUBSYSTEM_ID)
    })
}

/// Reads every fact from the system (except `UPower`, which needs D-Bus).
pub fn gather(root: &SysRoot) -> HwFacts {
    let hwdep = find_alc294_hwdep(root).map(|name| Hwdep {
        init_verbs: root.read(&format!("/sys/class/sound/{name}/init_verbs")),
        name,
    });
    let pci = format!("/sys/bus/pci/devices/{DGPU_PCI}");
    let dgpu = DgpuFacts {
        present: root.path(&pci).is_dir(),
        vendor: root.read(&format!("{pci}/vendor")),
        device: root.read(&format!("{pci}/device")),
        driver: std::fs::read_link(root.path(&format!("{pci}/driver")))
            .ok()
            .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned())),
        runtime_status: root.read(&format!("{pci}/power/runtime_status")),
        power_state: root.read(&format!("{pci}/power_state")),
        control: root.read(&format!("{pci}/power/control")),
        active_ms: root.read(&format!("{pci}/power/runtime_active_time")),
        suspended_ms: root.read(&format!("{pci}/power/runtime_suspended_time")),
    };
    let fingerprint_devices = root
        .list("/sys/bus/usb/devices")
        .into_iter()
        .filter(|name| {
            let id = |attr: &str| {
                root.read(&format!("/sys/bus/usb/devices/{name}/{attr}"))
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(parse_hex)
            };
            id("idVendor") == Some(u32::from(FINGERPRINT_VENDOR))
                && id("idProduct") == Some(u32::from(FINGERPRINT_PRODUCT))
        })
        .collect();
    let efivar = |name: &str| {
        root.read_bytes(&format!(
            "/sys/firmware/efi/efivars/{name}-{EFI_GLOBAL_GUID}"
        ))
    };
    HwFacts {
        audio: AudioFacts {
            hwdep,
            firmware: root.read(PATCH_FIRMWARE_PATH),
            patch_parameter: root.read(PATCH_PARAMETER_PATH),
        },
        end_threshold: root.read(&format!(
            "/sys/class/power_supply/{BATTERY}/charge_control_end_threshold"
        )),
        upower: None,
        dgpu,
        fingerprint_devices,
        secure_boot: SecureBootFacts {
            uefi: root.path("/sys/firmware/efi").is_dir(),
            secure_boot: efivar("SecureBoot"),
            setup_mode: efivar("SetupMode"),
        },
    }
}

/// Audio section of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioCheck {
    /// Verdict.
    pub status: Status,
    /// One line.
    pub summary: String,
    /// The ALC294 hwdep.
    pub hwdep: Option<String>,
    /// The installed patch firmware.
    pub patch_firmware: Option<String>,
    /// Verbs of the installed firmware.
    pub expected_verbs: Vec<String>,
    /// Lines of `init_verbs`.
    pub init_verbs: Vec<String>,
    /// `snd_hda_intel` `patch=`, without the unset slots.
    pub patch_parameter: Option<String>,
}

/// Battery section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatteryCheck {
    /// Verdict.
    pub status: Status,
    /// One line.
    pub summary: String,
    /// Battery name.
    pub battery: String,
    /// The limit Apex expects.
    pub expected_end_threshold: u8,
    /// The current limit.
    pub end_threshold: Option<u8>,
}

/// `UPower` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpowerCheck {
    /// Verdict.
    pub status: Status,
    /// One line.
    pub summary: String,
    /// `ChargeThresholdSupported`.
    pub threshold_supported: Option<bool>,
    /// `ChargeThresholdEnabled`.
    pub threshold_enabled: Option<bool>,
    /// `ChargeEndThreshold`.
    pub end_threshold: Option<u32>,
}

/// dGPU section.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DgpuCheck {
    /// Verdict.
    pub status: Status,
    /// One line.
    pub summary: String,
    /// PCI address.
    pub pci: String,
    /// The device exists.
    pub present: bool,
    /// Bound driver.
    pub driver: Option<String>,
    /// `power/runtime_status`.
    pub runtime_status: Option<String>,
    /// `power_state`.
    pub power_state: Option<String>,
    /// `power/control`.
    pub control: Option<String>,
    /// Milliseconds active since boot.
    pub runtime_active_ms: Option<u64>,
    /// Milliseconds suspended since boot.
    pub runtime_suspended_ms: Option<u64>,
    /// Suspended share since boot, in percent.
    pub suspended_percent: Option<f64>,
}

/// Fingerprint section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FingerprintCheck {
    /// Verdict.
    pub status: Status,
    /// One line.
    pub summary: String,
    /// `vvvv:pppp`.
    pub usb_id: String,
    /// USB device names with that id.
    pub devices: Vec<String>,
}

/// Secure Boot section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecureBootCheck {
    /// Verdict.
    pub status: Status,
    /// One line.
    pub summary: String,
    /// Booted through UEFI.
    pub uefi: bool,
    /// `SecureBoot` is 1.
    pub enabled: Option<bool>,
    /// `SetupMode` is 1.
    pub setup_mode: Option<bool>,
}

/// The whole report; see the module documentation for the JSON schema.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HwReport {
    /// [`REPORT_SCHEMA`].
    pub schema: u32,
    /// Machine id.
    pub machine: &'static str,
    /// Audio fix.
    pub audio: AudioCheck,
    /// Charge limit in sysfs.
    pub battery: BatteryCheck,
    /// Charge limit in `UPower`.
    pub upower: UpowerCheck,
    /// dGPU runtime power management.
    pub dgpu: DgpuCheck,
    /// Fingerprint reader.
    pub fingerprint: FingerprintCheck,
    /// Secure Boot.
    pub secure_boot: SecureBootCheck,
}

/// `(null),(null)` slots removed; `None` when no slot is set.
fn patch_slots(parameter: &str) -> Option<String> {
    let set: Vec<&str> = parameter
        .split(',')
        .map(str::trim)
        .filter(|slot| !slot.is_empty() && *slot != "(null)")
        .collect();
    (!set.is_empty()).then(|| set.join(","))
}

/// `true` when `needle` occurs as a contiguous run in `haystack` (the patch loader appends
/// a file's verbs in order, so the rendered pairs must stay adjacent).
fn contains_run(haystack: &[Option<InitVerb>], needle: &[InitVerb]) -> bool {
    needle.is_empty()
        || haystack.windows(needle.len()).any(|window| {
            window
                .iter()
                .zip(needle)
                .all(|(have, want)| have.as_ref() == Some(want))
        })
}

fn assess_audio(facts: &AudioFacts) -> AudioCheck {
    let mut check = AudioCheck {
        status: Status::Unknown,
        summary: String::new(),
        hwdep: facts.hwdep.as_ref().map(|h| h.name.clone()),
        patch_firmware: None,
        expected_verbs: Vec::new(),
        init_verbs: Vec::new(),
        patch_parameter: facts
            .patch_parameter
            .clone()
            .ok()
            .flatten()
            .as_deref()
            .and_then(patch_slots),
    };
    let Some(hwdep) = &facts.hwdep else {
        check.status = Status::Fail;
        check.summary = format!(
            "no ALC294 codec ({ALC294_VENDOR_ID:#010x} {ALC294_SUBSYSTEM_ID:#010x}) under /sys/class/sound"
        );
        return check;
    };
    let init_verbs = match &hwdep.init_verbs {
        Ok(text) => text.clone().unwrap_or_default(),
        Err(error) => {
            check.summary = format!("cannot read {}/init_verbs: {error}", hwdep.name);
            return check;
        }
    };
    check.init_verbs = init_verbs
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    let firmware = match &facts.firmware {
        Ok(Some(text)) => text,
        Ok(None) => {
            check.status = Status::Skip;
            check.summary = format!(
                "no patch firmware in this image (degraded=audio); {} has {} init verbs",
                hwdep.name,
                check.init_verbs.len()
            );
            return check;
        }
        Err(error) => {
            check.summary = format!("cannot read {PATCH_FIRMWARE_PATH}: {error}");
            return check;
        }
    };
    check.patch_firmware = Some(PATCH_FIRMWARE_PATH.to_owned());
    let patch = match crate::patch::parse(firmware) {
        Ok(patch) => patch,
        Err(error) => {
            check.status = Status::Fail;
            check.summary = format!("{PATCH_FIRMWARE_PATH} is not a valid patch file: {error}");
            return check;
        }
    };
    check.expected_verbs = patch.verbs.iter().map(ToString::to_string).collect();
    let loaded: Vec<Option<InitVerb>> = check
        .init_verbs
        .iter()
        .map(|line| InitVerb::parse(line).ok())
        .collect();
    if contains_run(&loaded, &patch.verbs) {
        check.status = Status::Pass;
        check.summary = format!(
            "{} holds the {} verbs of the patch firmware",
            hwdep.name,
            patch.verbs.len()
        );
    } else {
        check.status = Status::Fail;
        check.summary = format!(
            "{} does not hold the patch firmware's verbs: the kernel did not load it \
             (journalctl -k | grep 'patch firmware')",
            hwdep.name
        );
    }
    check
}

fn assess_battery(end_threshold: &Attr) -> BatteryCheck {
    let mut check = BatteryCheck {
        status: Status::Unknown,
        summary: String::new(),
        battery: BATTERY.to_owned(),
        expected_end_threshold: CHARGE_END_THRESHOLD,
        end_threshold: None,
    };
    match end_threshold {
        Ok(None) => {
            check.status = Status::Fail;
            check.summary =
                format!("{BATTERY} has no charge_control_end_threshold (asus-wmi did not add it)");
        }
        Err(error) => check.summary = format!("cannot read the charge limit: {error}"),
        Ok(Some(text)) => match text.parse::<u8>() {
            Err(_) => check.summary = format!("unexpected charge limit {text:?}"),
            Ok(value) => {
                check.end_threshold = Some(value);
                (check.status, check.summary) = if value == CHARGE_END_THRESHOLD {
                    (
                        Status::Pass,
                        format!("{BATTERY} stops charging at {value} %"),
                    )
                } else if value >= 100 {
                    (
                        Status::Warn,
                        format!(
                            "{BATTERY} charges to 100 %: the limit is off (Settings > Power > Preserve Battery Health)"
                        ),
                    )
                } else {
                    (
                        Status::Warn,
                        format!("{BATTERY} stops at {value} %; Apex sets {CHARGE_END_THRESHOLD} %"),
                    )
                };
            }
        },
    }
    check
}

fn assess_upower(facts: Option<&UpowerFacts>) -> UpowerCheck {
    let mut check = UpowerCheck {
        status: Status::Unknown,
        summary: "UPower could not be asked (no system bus or no UPower)".to_owned(),
        threshold_supported: None,
        threshold_enabled: None,
        end_threshold: None,
    };
    let Some(facts) = facts else {
        return check;
    };
    check.threshold_supported = facts.supported;
    check.threshold_enabled = facts.enabled;
    check.end_threshold = facts.end_threshold;
    (check.status, check.summary) = match (facts.supported, facts.enabled) {
        (Some(true), Some(true)) => (
            Status::Pass,
            "UPower sees the charge limit and it is on".to_owned(),
        ),
        (Some(true), _) => (
            Status::Warn,
            "UPower supports the charge limit but it is off (Settings > Power > Preserve Battery Health)"
                .to_owned(),
        ),
        (Some(false), _) => (
            Status::Fail,
            "UPower does not see the charge limit (the BAT0 udev re-trigger did not run)".to_owned(),
        ),
        (None, _) => (
            Status::Unknown,
            "this UPower has no charge-threshold properties".to_owned(),
        ),
    };
    check
}

/// Share of time suspended below which the dGPU line warns (hardware acceptance: at least
/// 95 % over 10 minutes idle; since boot is only an indication).
pub const MIN_SUSPENDED_PERCENT: f64 = 95.0;

fn number(attr: &Attr) -> Option<u64> {
    attr.clone().ok().flatten()?.parse().ok()
}

fn assess_dgpu(facts: &DgpuFacts) -> DgpuCheck {
    let text = |attr: &Attr| attr.clone().ok().flatten();
    let active = number(&facts.active_ms);
    let suspended = number(&facts.suspended_ms);
    let suspended_percent = match (active, suspended) {
        (Some(a), Some(s)) if a + s > 0 => {
            Some((s as f64 * 1000.0 / (a + s) as f64).round() / 10.0)
        }
        _ => None,
    };
    let mut check = DgpuCheck {
        status: Status::Unknown,
        summary: String::new(),
        pci: DGPU_PCI.to_owned(),
        present: facts.present,
        driver: facts.driver.clone(),
        runtime_status: text(&facts.runtime_status),
        power_state: text(&facts.power_state),
        control: text(&facts.control),
        runtime_active_ms: active,
        runtime_suspended_ms: suspended,
        suspended_percent,
    };
    if !facts.present {
        check.summary = format!("no PCI device at {DGPU_PCI}");
        return check;
    }
    let ids = (
        text(&facts.vendor).as_deref().and_then(parse_hex),
        text(&facts.device).as_deref().and_then(parse_hex),
    );
    if ids != (Some(u32::from(DGPU_VENDOR)), Some(u32::from(DGPU_DEVICE))) {
        check.status = Status::Warn;
        check.summary =
            format!("{DGPU_PCI} is not the expected {DGPU_VENDOR:04x}:{DGPU_DEVICE:04x}");
        return check;
    }
    let driver = check
        .driver
        .as_ref()
        .map_or_else(|| "no driver".to_owned(), |name| format!("driver {name}"));
    let share = suspended_percent
        .map(|p| format!("; suspended {p:.1} % since boot"))
        .unwrap_or_default();
    let state = check
        .power_state
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    (check.status, check.summary) = match (
        check.control.as_deref(),
        check.runtime_status.as_deref(),
    ) {
        (Some(control), _) if control != "auto" => (
            Status::Fail,
            format!(
                "runtime power management is off (power/control = {control}); the dGPU never sleeps"
            ),
        ),
        // Suspended now, but mostly awake since boot: something keeps waking it.
        (_, Some("suspended"))
            if state == "D3cold"
                && suspended_percent.is_some_and(|p| p < MIN_SUSPENDED_PERCENT) =>
        {
            (
                Status::Warn,
                format!(
                    "suspended in D3cold now, {driver}{share} (the acceptance test wants \
                     {MIN_SUSPENDED_PERCENT:.0} % while idle)"
                ),
            )
        }
        (_, Some("suspended")) if state == "D3cold" => (
            Status::Pass,
            format!("suspended in D3cold, {driver}{share}"),
        ),
        (_, Some("suspended")) => (
            Status::Warn,
            format!("suspended but in {state}, not D3cold, {driver}{share}"),
        ),
        (_, Some("active" | "resuming" | "suspending")) => (
            Status::Warn,
            format!("awake right now ({state}), {driver}: something is using it{share}"),
        ),
        (_, Some(other)) => (Status::Fail, format!("runtime status {other:?}, {driver}")),
        (_, None) => (Status::Unknown, "no runtime power status".to_owned()),
    };
    check
}

fn assess_fingerprint(devices: &[String]) -> FingerprintCheck {
    let usb_id = format!("{FINGERPRINT_VENDOR:04x}:{FINGERPRINT_PRODUCT:04x}");
    let (status, summary) = if devices.is_empty() {
        (
            Status::Fail,
            format!("ELAN reader {usb_id} is not on the USB bus"),
        )
    } else {
        (
            Status::Pass,
            format!("ELAN reader {usb_id} present at {}", devices.join(", ")),
        )
    };
    FingerprintCheck {
        status,
        summary,
        usb_id,
        devices: devices.to_vec(),
    }
}

/// The value byte of an EFI variable (after the 4 attribute bytes).
fn efi_flag(variable: &Result<Option<Vec<u8>>, String>) -> Option<bool> {
    match variable {
        Ok(Some(bytes)) => bytes.get(4).map(|&value| value == 1),
        _ => None,
    }
}

fn assess_secure_boot(facts: &SecureBootFacts) -> SecureBootCheck {
    let enabled = efi_flag(&facts.secure_boot);
    let setup_mode = efi_flag(&facts.setup_mode);
    let (status, summary) = if facts.uefi {
        match (enabled, setup_mode) {
            (Some(true), _) => (Status::Pass, "Secure Boot is enabled".to_owned()),
            (Some(false), Some(true)) => (
                Status::Warn,
                "Secure Boot is off: the firmware is in setup mode".to_owned(),
            ),
            (Some(false), _) => (Status::Warn, "Secure Boot is disabled".to_owned()),
            (None, _) => match &facts.secure_boot {
                Err(error) => (Status::Unknown, format!("cannot read SecureBoot: {error}")),
                _ => (Status::Unknown, "no SecureBoot variable".to_owned()),
            },
        }
    } else {
        (Status::Warn, "not booted through UEFI".to_owned())
    };
    SecureBootCheck {
        status,
        summary,
        uefi: facts.uefi,
        enabled,
        setup_mode,
    }
}

/// Turns facts into verdicts.
pub fn assess(facts: &HwFacts) -> HwReport {
    HwReport {
        schema: REPORT_SCHEMA,
        machine: MACHINE_ID,
        audio: assess_audio(&facts.audio),
        battery: assess_battery(&facts.end_threshold),
        upower: assess_upower(facts.upower.as_ref()),
        dgpu: assess_dgpu(&facts.dgpu),
        fingerprint: assess_fingerprint(&facts.fingerprint_devices),
        secure_boot: assess_secure_boot(&facts.secure_boot),
    }
}

/// The text report.
pub fn render(report: &HwReport) -> String {
    let mut out = String::new();
    let rows = [
        ("Audio fix", report.audio.status, &report.audio.summary),
        (
            "Charge limit",
            report.battery.status,
            &report.battery.summary,
        ),
        ("UPower", report.upower.status, &report.upower.summary),
        ("dGPU", report.dgpu.status, &report.dgpu.summary),
        (
            "Fingerprint",
            report.fingerprint.status,
            &report.fingerprint.summary,
        ),
        (
            "Secure Boot",
            report.secure_boot.status,
            &report.secure_boot.summary,
        ),
    ];
    for (label, status, summary) in rows {
        let _ = writeln!(out, "{}  {label:<13} {summary}", status.label());
    }
    let audio = &report.audio;
    if audio.status == Status::Fail && !audio.expected_verbs.is_empty() {
        let _ = writeln!(out, "\nExpected in init_verbs:");
        for verb in &audio.expected_verbs {
            let _ = writeln!(out, "  {verb}");
        }
        let _ = writeln!(out, "Loaded:");
        if audio.init_verbs.is_empty() {
            let _ = writeln!(out, "  (none)");
        }
        for verb in &audio.init_verbs {
            let _ = writeln!(out, "  {verb}");
        }
        if let Some(parameter) = &audio.patch_parameter {
            let _ = writeln!(out, "snd_hda_intel patch={parameter}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_parameter_drops_unset_slots() {
        assert_eq!(patch_slots("(null),(null),(null)"), None);
        assert_eq!(
            patch_slots("apex/a.fw,apex/a.fw,(null),(null)").as_deref(),
            Some("apex/a.fw,apex/a.fw")
        );
    }

    #[test]
    fn verbs_must_stay_adjacent() {
        let v = |line: &str| InitVerb::parse(line).unwrap();
        let want = [v("0x20 0x500 0x1b"), v("0x20 0x400 0x4e4b")];
        let have = |lines: &[&str]| -> Vec<Option<InitVerb>> {
            lines.iter().map(|l| InitVerb::parse(l).ok()).collect()
        };
        assert!(contains_run(
            &have(&["0x19 0x707 0x20", "0x20 0x500 0x001b", "0x20 0x400 0x4e4b"]),
            &want
        ));
        assert!(!contains_run(
            &have(&[
                "0x20 0x500 0x001b",
                "0x20 0x500 0x000f",
                "0x20 0x400 0x4e4b"
            ]),
            &want
        ));
        assert!(!contains_run(&have(&["0x20 0x500 0x001b"]), &want));
    }

    #[test]
    fn efi_flags() {
        assert_eq!(efi_flag(&Ok(Some(vec![6, 0, 0, 0, 1]))), Some(true));
        assert_eq!(efi_flag(&Ok(Some(vec![6, 0, 0, 0, 0]))), Some(false));
        assert_eq!(efi_flag(&Ok(Some(vec![6, 0, 0]))), None);
        assert_eq!(efi_flag(&Ok(None)), None);
    }
}
