use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use toml::value::Datetime;

use crate::scale::{Scale, supported_scales};

pub const PROFILE_SCHEMA: u32 = 1;

pub const DEFAULT_PROFILE: &str = "hardware/m7400qc/profile.toml";

/// Keeps the firmware comment lines these strings are copied into under the kernel's limit.
pub const MAX_PROVENANCE_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub schema: u32,
    pub machine: Machine,
    pub cpu: Cpu,
    pub panel: Panel,
    pub audio: Audio,
    pub fingerprint: Fingerprint,
    pub gpu: Gpus,
    pub firmware: Firmware,
    pub battery: Battery,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    pub id: String,
    pub dmi_sys_vendor: String,
    pub dmi_product_name: String,
    pub dmi_board_name: String,
    pub bios_versions_seen: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cpu {
    pub model: String,
    pub march: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    pub connector: String,
    pub edid_vendor: String,
    pub edid_product: String,
    pub edid_serial: String,
    pub edid_name: String,
    pub width_px: u32,
    pub height_px: u32,
    pub width_mm: u32,
    pub height_mm: u32,
    pub refresh_hz: Vec<f64>,
    pub default_scale: Scale,
    pub alternative_scales: Vec<Scale>,
}

impl Panel {
    pub fn preferred_rate(&self) -> Option<f64> {
        self.refresh_hz.iter().copied().max_by(f64::total_cmp)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audio {
    pub codec: AudioCodec,
    pub fix: AudioFix,
    pub measurements: Option<Measurements>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioCodec {
    pub controller_pci: String,
    pub vendor_id: u32,
    pub subsystem_id: u32,
    pub address: u8,
    pub coef_nid: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioFix {
    pub rules: Vec<FixRule>,
    pub restore_if_windows_differs: Vec<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixRule {
    pub index: u16,
    pub set: u16,
    pub clear: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurements {
    pub date: Datetime,
    pub bios: String,
    pub kernel: String,
    pub cold_boot: Vec<CoefValue>,
    pub after_windows: Option<Vec<CoefValue>>,
    // Informative only: validated, never rendered.
    pub with_fix_loaded: Option<Vec<CoefValue>>,
    pub with_fix_after_suspend: Option<Vec<CoefValue>>,
    pub with_fix_cold_boot: Option<Vec<CoefValue>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoefValue {
    pub index: u16,
    pub value: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fingerprint {
    pub usb_vendor: u16,
    pub usb_product: u16,
    pub libfprint_driver: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gpus {
    pub integrated: IntegratedGpu,
    pub discrete: DiscreteGpu,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegratedGpu {
    pub pci: String,
    pub vendor: u16,
    pub device: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscreteGpu {
    pub pci: String,
    pub vendor: u16,
    pub device: u16,
    pub drives_displays: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Firmware {
    pub packages: Vec<String>,
    pub required_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Battery {
    pub name: String,
    pub charge_end_threshold: u8,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("cannot read {path}: {error}")]
    Read { path: String, error: std::io::Error },
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error("profile schema {found} is not supported (this apexctl reads schema {PROFILE_SCHEMA})")]
    UnsupportedSchema { found: u32 },
    #[error("{field}: {reason}")]
    Invalid { field: String, reason: String },
}

fn invalid(field: impl Into<String>, reason: impl Into<String>) -> ProfileError {
    ProfileError::Invalid {
        field: field.into(),
        reason: reason.into(),
    }
}

/// Read first so a newer profile reports its schema rather than a key error.
#[derive(Deserialize)]
struct SchemaOnly {
    schema: u32,
}

impl Profile {
    pub fn parse(text: &str) -> Result<Self, ProfileError> {
        let SchemaOnly { schema } = toml::from_str(text)?;
        if schema != PROFILE_SCHEMA {
            return Err(ProfileError::UnsupportedSchema { found: schema });
        }
        let profile: Self = toml::from_str(text)?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn load(path: &Path) -> Result<Self, ProfileError> {
        let text = std::fs::read_to_string(path).map_err(|error| ProfileError::Read {
            path: path.display().to_string(),
            error,
        })?;
        Self::parse(&text)
    }

    pub fn validate(&self) -> Result<(), ProfileError> {
        validate_id(&self.machine.id)?;
        validate_comment_text("machine.dmi_board_name", &self.machine.dmi_board_name)?;
        self.validate_panel()?;
        self.validate_audio()?;
        for (i, glob) in self.firmware.required_files.iter().enumerate() {
            if glob.trim().is_empty() || glob.contains('#') || glob.trim() != glob {
                return Err(invalid(
                    format!("firmware.required_files[{i}]"),
                    "must be a non-empty glob without '#' or surrounding spaces",
                ));
            }
        }
        if !(1..=100).contains(&self.battery.charge_end_threshold) {
            return Err(invalid(
                "battery.charge_end_threshold",
                "must be between 1 and 100",
            ));
        }
        if !is_simple_token(&self.battery.name) {
            return Err(invalid("battery.name", "must be a sysfs name such as BAT0"));
        }
        Ok(())
    }

    fn validate_panel(&self) -> Result<(), ProfileError> {
        let panel = &self.panel;
        for (field, value) in [
            ("panel.connector", &panel.connector),
            ("panel.edid_serial", &panel.edid_serial),
        ] {
            if !is_simple_token(value) {
                return Err(invalid(field, "must be letters, digits, '-', '_' or '.'"));
            }
        }
        if panel.edid_vendor.len() != 3
            || !panel.edid_vendor.bytes().all(|b| b.is_ascii_uppercase())
        {
            return Err(invalid("panel.edid_vendor", "must be a 3-letter PNP id"));
        }
        let product_ok = panel.edid_product.len() == 6
            && panel.edid_product.starts_with("0x")
            && panel.edid_product[2..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !product_ok {
            return Err(invalid(
                "panel.edid_product",
                "must be written like mutter prints it: 0x plus 4 lowercase hex digits",
            ));
        }
        if panel.width_px == 0 || panel.height_px == 0 {
            return Err(invalid("panel", "width_px and height_px must be positive"));
        }
        if panel.refresh_hz.is_empty()
            || panel
                .refresh_hz
                .iter()
                .any(|rate| !rate.is_finite() || *rate <= 0.0)
        {
            return Err(invalid(
                "panel.refresh_hz",
                "must list at least one positive rate",
            ));
        }
        let offered = supported_scales(panel.width_px, panel.height_px);
        let scales = std::iter::once(("panel.default_scale".to_owned(), panel.default_scale))
            .chain(
                panel
                    .alternative_scales
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (format!("panel.alternative_scales[{i}]"), *s)),
            );
        for (field, scale) in scales {
            if scale.den == 0 || !offered.contains(&scale) {
                let list: Vec<String> = offered.iter().map(ToString::to_string).collect();
                return Err(invalid(
                    field,
                    format!(
                        "mutter 51 does not offer {}/{} for {}x{} and would ignore it; offered: {}",
                        scale.num,
                        scale.den,
                        panel.width_px,
                        panel.height_px,
                        list.join(", ")
                    ),
                ));
            }
        }
        Ok(())
    }

    fn validate_audio(&self) -> Result<(), ProfileError> {
        let codec = &self.audio.codec;
        // The kernel scans the [codec] line into `int`s.
        for (field, value) in [
            ("audio.codec.vendor_id", codec.vendor_id),
            ("audio.codec.subsystem_id", codec.subsystem_id),
        ] {
            if value == 0 || i32::try_from(value).is_err() {
                return Err(invalid(field, "must be a non-zero 31-bit value"));
            }
        }
        if codec.vendor_id >> 16 != 0x10ec {
            return Err(invalid(
                "audio.codec.vendor_id",
                "the COEF fix is for Realtek codecs (vendor 0x10ec)",
            ));
        }
        if codec.address > crate::hda::MAX_CODEC_ADDRESS {
            return Err(invalid("audio.codec.address", "must be 0 to 15"));
        }
        if codec.coef_nid == 0 || u16::from(codec.coef_nid) > crate::hda::MAX_NID {
            return Err(invalid("audio.codec.coef_nid", "must be 0x01 to 0x7f"));
        }

        let fix = &self.audio.fix;
        let mut seen = std::collections::BTreeSet::new();
        for (i, rule) in fix.rules.iter().enumerate() {
            let field = format!("audio.fix.rules[{i}]");
            if !seen.insert(rule.index) {
                return Err(invalid(
                    field,
                    format!("COEF {:#04x} has two rules", rule.index),
                ));
            }
            if rule.set & rule.clear != 0 {
                return Err(invalid(field, "set and clear overlap"));
            }
            if rule.set == 0 && rule.clear == 0 {
                return Err(invalid(field, "sets and clears nothing"));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for index in &fix.restore_if_windows_differs {
            if !seen.insert(*index) {
                return Err(invalid(
                    "audio.fix.restore_if_windows_differs",
                    format!("COEF {index:#04x} is listed twice"),
                ));
            }
        }

        if let Some(measurements) = &self.audio.measurements {
            validate_measurements(measurements, fix)?;
        }
        Ok(())
    }
}

pub fn coef_map(field: &str, values: &[CoefValue]) -> Result<BTreeMap<u16, u16>, ProfileError> {
    let mut map = BTreeMap::new();
    for coef in values {
        if map.insert(coef.index, coef.value).is_some() {
            return Err(invalid(
                field,
                format!("COEF {:#04x} is listed twice", coef.index),
            ));
        }
    }
    Ok(map)
}

fn validate_measurements(m: &Measurements, fix: &AudioFix) -> Result<(), ProfileError> {
    if m.date.date.is_none() || m.date.time.is_some() || m.date.offset.is_some() {
        return Err(invalid(
            "audio.measurements.date",
            "must be a plain date such as 2026-09-25",
        ));
    }
    for (field, value) in [
        ("audio.measurements.bios", &m.bios),
        ("audio.measurements.kernel", &m.kernel),
    ] {
        validate_comment_text(field, value)?;
    }
    if m.cold_boot.is_empty() {
        return Err(invalid("audio.measurements.cold_boot", "is empty"));
    }
    let cold = coef_map("audio.measurements.cold_boot", &m.cold_boot)?;
    let after = m
        .after_windows
        .as_deref()
        .map(|list| coef_map("audio.measurements.after_windows", list))
        .transpose()?;
    for (field, list) in [
        ("audio.measurements.with_fix_loaded", &m.with_fix_loaded),
        (
            "audio.measurements.with_fix_after_suspend",
            &m.with_fix_after_suspend,
        ),
        (
            "audio.measurements.with_fix_cold_boot",
            &m.with_fix_cold_boot,
        ),
    ] {
        if let Some(list) = list {
            coef_map(field, list)?;
        }
    }
    for rule in &fix.rules {
        if !cold.contains_key(&rule.index) {
            return Err(invalid(
                "audio.measurements.cold_boot",
                format!("COEF {:#04x} (a fix rule) was not measured", rule.index),
            ));
        }
    }
    if !fix.restore_if_windows_differs.is_empty() {
        let Some(after) = after else {
            return Err(invalid(
                "audio.measurements.after_windows",
                "is missing; restore_if_windows_differs needs the phase-B dump",
            ));
        };
        for index in &fix.restore_if_windows_differs {
            for (field, map) in [
                ("audio.measurements.cold_boot", &cold),
                ("audio.measurements.after_windows", &after),
            ] {
                if !map.contains_key(index) {
                    return Err(invalid(
                        field,
                        format!("COEF {index:#04x} (restore_if_windows_differs) was not measured"),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Copied into a firmware comment line: a line feed would start a line the kernel parses,
/// and a NUL hangs its patch loader.
fn validate_comment_text(field: &str, value: &str) -> Result<(), ProfileError> {
    let printable = value.bytes().all(|b| (0x20..0x7f).contains(&b));
    if value.is_empty() || value.len() > MAX_PROVENANCE_LEN || !printable {
        return Err(invalid(
            field,
            format!("must be 1 to {MAX_PROVENANCE_LEN} printable ASCII characters"),
        ));
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), ProfileError> {
    let ok = !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "machine.id",
            "must be lowercase letters, digits and '-' (it is used in file names)",
        ))
    }
}

/// Safe in XML text, sysfs paths and file names.
fn is_simple_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const BASE: &str = r#"
schema = 1

[machine]
id = "m7400qc"
dmi_sys_vendor = "ASUSTeK COMPUTER INC."
dmi_product_name = "Vivobook_ASUSLaptop M7400QC_M7400QC"
dmi_board_name = "M7400QC"
bios_versions_seen = ["M7400QC.311"]

[cpu]
model = "AMD Ryzen 5 5600H with Radeon Graphics"
march = "znver3"

[panel]
connector = "eDP-1"
edid_vendor = "SDC"
edid_product = "0x4154"
edid_serial = "0x00000000"
edid_name = "ATNA40YK04-0"
width_px = 2880
height_px = 1800
width_mm = 302
height_mm = 189
refresh_hz = [90.001434326171875, 60.000957489013672]
default_scale = { num = 2, den = 1 }
alternative_scales = [{ num = 5, den = 3 }]

[audio.codec]
controller_pci = "0000:04:00.6"
vendor_id = 0x10ec0294
subsystem_id = 0x10431ab2
address = 0
coef_nid = 0x20

[audio.fix]
rules = [{ index = 0x1b, set = 0x0400, clear = 0x0000 }]
restore_if_windows_differs = [0x0f, 0x40]

[fingerprint]
usb_vendor = 0x04f3
usb_product = 0x0c6e
libfprint_driver = "elan"

[gpu.integrated]
pci = "0000:04:00.0"
vendor = 0x1002
device = 0x1638

[gpu.discrete]
pci = "0000:01:00.0"
vendor = 0x10de
device = 0x25a2
drives_displays = false

[firmware]
packages = ["linux-firmware"]
required_files = ["amdgpu/green_sardine_*"]

[battery]
name = "BAT0"
charge_end_threshold = 80
"#;

    /// Placeholders, not this laptop's values.
    pub(crate) const MEASURED: &str = r"
[audio.measurements]
date = 2026-09-25
bios = 'M7400QC.311'
kernel = '7.0.0-31-generic'
cold_boot = [ { index = 0x0f, value = 0x7770 }, { index = 0x1b, value = 0x4a4b }, { index = 0x40, value = 0x8800 }, { index = 0x2a, value = 0x1234 } ]
after_windows = [ { index = 0x0f, value = 0x7774 }, { index = 0x1b, value = 0x4a4b }, { index = 0x40, value = 0x8800 } ]
with_fix_loaded = [ { index = 0x1b, value = 0x4e4b } ]
";

    fn with(extra: &str) -> String {
        format!("{BASE}{extra}")
    }

    fn reason(text: &str) -> String {
        match Profile::parse(text) {
            Err(error) => error.to_string(),
            Ok(_) => "accepted".to_owned(),
        }
    }

    #[test]
    fn base_and_measured_profiles_parse() {
        let profile = Profile::parse(BASE).unwrap();
        assert!(profile.audio.measurements.is_none());
        let ninety: f64 = "90.001434326171875".parse().unwrap();
        assert_eq!(profile.panel.preferred_rate(), Some(ninety));
        let measured = Profile::parse(&with(MEASURED)).unwrap();
        let m = measured.audio.measurements.unwrap();
        assert_eq!(m.date.to_string(), "2026-09-25");
        assert_eq!(m.cold_boot.len(), 4);
    }

    #[test]
    fn typos_and_bad_values_are_rejected() {
        assert!(reason(&BASE.replace("coef_nid", "coef_nod")).contains("coef_nod"));
        assert!(reason(&with(&MEASURED.replace("cold_boot", "cold_bot"))).contains("cold_bot"));
        assert_ne!(
            reason(&with(
                &MEASURED.replace("value = 0x7770", "value = 0x17770")
            )),
            "accepted"
        );
        assert!(reason(&BASE.replace("schema = 1", "schema = 2")).contains("schema 2"));
        assert!(
            reason(&BASE.replace("num = 2, den = 1", "num = 9, den = 5"))
                .contains("does not offer 9/5")
        );
        assert!(
            reason(&BASE.replace(
                "set = 0x0400, clear = 0x0000",
                "set = 0x0400, clear = 0x0400"
            ))
            .contains("overlap")
        );
        assert!(
            reason(&BASE.replace("id = \"m7400qc\"", "id = \"M7400QC\"")).contains("machine.id")
        );
        assert!(reason(&BASE.replace("\"0x4154\"", "\"0x4154 \"")).contains("edid_product"));
        assert!(
            reason(&BASE.replace("vendor_id = 0x10ec0294", "vendor_id = 0x80ec0294"))
                .contains("31-bit")
        );
        assert!(
            reason(&BASE.replace("charge_end_threshold = 80", "charge_end_threshold = 0"))
                .contains("between 1 and 100")
        );
    }

    #[test]
    fn measurements_must_cover_the_fix() {
        let no_1b = MEASURED.replace(
            "{ index = 0x1b, value = 0x4a4b }, { index = 0x40",
            "{ index = 0x40",
        );
        assert!(reason(&with(&no_1b)).contains("0x1b (a fix rule) was not measured"));
        let no_after = MEASURED
            .lines()
            .filter(|l| !l.starts_with("after_windows"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(reason(&with(&no_after)).contains("phase-B"));
        let dup = MEASURED.replace("{ index = 0x2a", "{ index = 0x0f");
        assert!(reason(&with(&dup)).contains("listed twice"));
        let newline = MEASURED.replace("'M7400QC.311'", "\"M7400QC.311\\n[verb]\"");
        assert!(reason(&with(&newline)).contains("printable ASCII"));
        let timed = MEASURED.replace("date = 2026-09-25", "date = 2026-09-25T10:00:00");
        assert!(reason(&with(&timed)).contains("plain date"));
    }

    #[test]
    fn the_board_name_cannot_smuggle_lines_or_nul_into_the_firmware() {
        for board in [
            r#""M7400QC\u0000""#,
            r#""M7400QC\n[pincfg]\n0x19 0x411111f0""#,
            r#""M7400QC\t""#,
            r#""""#,
            &format!("\"{}\"", "M".repeat(MAX_PROVENANCE_LEN + 1)),
        ] {
            let text = BASE.replace(
                r#"dmi_board_name = "M7400QC""#,
                &format!("dmi_board_name = {board}"),
            );
            assert!(
                reason(&with_measured(&text))
                    .starts_with("machine.dmi_board_name: must be 1 to 64 printable ASCII"),
                "{board}: {}",
                reason(&with_measured(&text))
            );
            assert!(
                reason(&text).starts_with("machine.dmi_board_name"),
                "{board}"
            );
        }
        let longest = BASE.replace(
            r#"dmi_board_name = "M7400QC""#,
            &format!("dmi_board_name = \"{}\"", "M".repeat(MAX_PROVENANCE_LEN)),
        );
        assert_eq!(reason(&with_measured(&longest)), "accepted");
    }

    fn with_measured(base: &str) -> String {
        format!("{base}{MEASURED}")
    }

    #[test]
    fn a_read_error_names_its_cause_once() {
        let error = Profile::load(Path::new("/nonexistent/apex/profile.toml")).unwrap_err();
        assert!(matches!(error, ProfileError::Read { .. }));
        assert_eq!(
            printed_error_chain(&error).matches("os error 2").count(),
            1,
            "{}",
            printed_error_chain(&error)
        );
        let toml_error = Profile::parse(&BASE.replace("coef_nid", "coef_nod")).unwrap_err();
        assert_eq!(
            printed_error_chain(&toml_error)
                .matches("unknown field")
                .count(),
            1
        );
    }

    pub(crate) fn printed_error_chain(error: &dyn std::error::Error) -> String {
        let mut text = error.to_string();
        let mut cause = error.source();
        while let Some(inner) = cause {
            text.push_str(": ");
            text.push_str(&inner.to_string());
            cause = inner.source();
        }
        text
    }
}
