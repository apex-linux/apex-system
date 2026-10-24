use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::hda::{InitVerb, Verb};
use crate::patch::{CodecLine, MAX_LINE, PatchError};
use crate::profile::{Profile, ProfileError, coef_map};

pub const MONITORS_XML: &str = "monitors.xml";
/// Installed in `/usr/lib/udev/rules.d/`; mutter reads the tag at startup and on hotplug.
pub const MUTTER_IGNORE_RULES: &str = "61-apex-mutter-ignore-dgpu.rules";
pub const AUDIO_CONF: &str = "apex-audio.conf";
pub const FIRMWARE_SUBDIR: &str = "apex";
/// `snd-hda-intel` assigns `patch=` entries in controller probe order, not by PCI address,
/// so every slot names the same file.
pub const PATCH_SLOTS: usize = 4;
pub const GENERATED_DIR: &str = "generated";

pub fn firmware_file_name(profile: &Profile) -> String {
    format!(
        "alc{:x}-{}.fw",
        profile.audio.codec.vendor_id & 0xffff,
        profile.machine.id
    )
}

/// `request_firmware` accepts a subdirectory; it only refuses `..` components.
pub fn firmware_reference(profile: &Profile) -> String {
    format!("{FIRMWARE_SUBDIR}/{}", firmware_file_name(profile))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteReason {
    Rule { cold: u16, set: u16, clear: u16 },
    RestoreCold { cold: u16, after_windows: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoefWrite {
    pub index: u16,
    pub value: u16,
    pub reason: WriteReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnchangedAfterWindows {
    pub index: u16,
    pub value: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoefPlan {
    pub writes: Vec<CoefWrite>,
    pub unchanged: Vec<UnchangedAfterWindows>,
}

pub fn plan_coef_writes(profile: &Profile) -> Result<Option<CoefPlan>, ProfileError> {
    let Some(measurements) = &profile.audio.measurements else {
        return Ok(None);
    };
    let fix = &profile.audio.fix;
    let cold = coef_map("audio.measurements.cold_boot", &measurements.cold_boot)?;
    let after = coef_map(
        "audio.measurements.after_windows",
        measurements.after_windows.as_deref().unwrap_or_default(),
    )?;
    let missing = |field: &str, index: u16| ProfileError::Invalid {
        field: field.to_owned(),
        reason: format!("COEF {index:#04x} was not measured"),
    };

    let mut writes_by_index = std::collections::BTreeMap::new();
    for rule in &fix.rules {
        let cold_value = *cold
            .get(&rule.index)
            .ok_or_else(|| missing("audio.measurements.cold_boot", rule.index))?;
        writes_by_index.insert(
            rule.index,
            CoefWrite {
                index: rule.index,
                value: (cold_value & !rule.clear) | rule.set,
                reason: WriteReason::Rule {
                    cold: cold_value,
                    set: rule.set,
                    clear: rule.clear,
                },
            },
        );
    }
    let mut unchanged = Vec::new();
    for &index in &fix.restore_if_windows_differs {
        let cold_value = *cold
            .get(&index)
            .ok_or_else(|| missing("audio.measurements.cold_boot", index))?;
        let after_value = *after
            .get(&index)
            .ok_or_else(|| missing("audio.measurements.after_windows", index))?;
        // A rule wins: it starts from the cold value anyway.
        if writes_by_index.contains_key(&index) {
            continue;
        }
        if after_value == cold_value {
            unchanged.push(UnchangedAfterWindows {
                index,
                value: cold_value,
            });
        } else {
            writes_by_index.insert(
                index,
                CoefWrite {
                    index,
                    value: cold_value,
                    reason: WriteReason::RestoreCold {
                        cold: cold_value,
                        after_windows: after_value,
                    },
                },
            );
        }
    }
    unchanged.sort_by_key(|u| u.index);
    Ok(Some(CoefPlan {
        writes: writes_by_index.into_values().collect(),
        unchanged,
    }))
}

pub fn coef_verbs(profile: &Profile, writes: &[CoefWrite]) -> Vec<InitVerb> {
    let nid = u16::from(profile.audio.codec.coef_nid);
    writes
        .iter()
        .flat_map(|w| [Verb::SetCoefIndex(w.index), Verb::SetProcCoef(w.value)])
        .filter_map(|verb| InitVerb::new(nid, verb).ok())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    pub name: String,
    /// `None`: the file must not exist.
    pub contents: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioOutcome {
    Rendered(Vec<CoefWrite>),
    NotMeasured,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub files: Vec<OutputFile>,
    pub audio: AudioOutcome,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(
        "{file} line {line} has {length} characters; the kernel's patch loader stops at {MAX_LINE}"
    )]
    LineTooLong {
        file: String,
        line: usize,
        length: usize,
    },
    #[error("[audio.fix] writes no COEF with these measurements; nothing to render")]
    NoVerbs,
    #[error("{file} would not load as rendered: {error}")]
    InvalidFirmware { file: String, error: PatchError },
    #[error("{file} does not parse back to the profile's [codec] line and the planned verbs")]
    FirmwareMismatch { file: String },
}

/// Deterministic (no path, time or user in the output), so `--check` can compare bytes.
pub fn render(profile: &Profile) -> Result<Rendered, RenderError> {
    profile.validate()?;
    let mut files = vec![OutputFile {
        name: MONITORS_XML.to_owned(),
        contents: Some(monitors_xml(profile)),
    }];
    let fw_name = firmware_file_name(profile);
    let audio = match plan_coef_writes(profile)? {
        None => {
            files.push(OutputFile {
                name: fw_name,
                contents: None,
            });
            files.push(OutputFile {
                name: AUDIO_CONF.to_owned(),
                contents: None,
            });
            AudioOutcome::NotMeasured
        }
        Some(CoefPlan { writes, unchanged }) => {
            if writes.is_empty() {
                return Err(RenderError::NoVerbs);
            }
            let firmware = patch_firmware(profile, &writes, &unchanged);
            for (i, line) in firmware.lines().enumerate() {
                if line.len() > MAX_LINE {
                    return Err(RenderError::LineTooLong {
                        file: fw_name,
                        line: i + 1,
                        length: line.len(),
                    });
                }
            }
            verify_firmware(profile, &fw_name, &firmware, &writes)?;
            files.push(OutputFile {
                name: fw_name,
                contents: Some(firmware),
            });
            files.push(OutputFile {
                name: AUDIO_CONF.to_owned(),
                contents: Some(modprobe_conf(profile)),
            });
            AudioOutcome::Rendered(writes)
        }
    };
    files.push(OutputFile {
        name: MUTTER_IGNORE_RULES.to_owned(),
        contents: mutter_ignore_rules(profile),
    });
    Ok(Rendered { files, audio })
}

/// Profile text lands in comment lines; it must not change what the kernel reads.
fn verify_firmware(
    profile: &Profile,
    file: &str,
    firmware: &str,
    writes: &[CoefWrite],
) -> Result<(), RenderError> {
    let parsed = crate::patch::parse(firmware).map_err(|error| RenderError::InvalidFirmware {
        file: file.to_owned(),
        error,
    })?;
    let codec = &profile.audio.codec;
    let expected = CodecLine {
        vendor_id: codec.vendor_id,
        subsystem_id: codec.subsystem_id,
        address: u32::from(codec.address),
    };
    if parsed.codec != expected || parsed.verbs != coef_verbs(profile, writes) {
        return Err(RenderError::FirmwareMismatch {
            file: file.to_owned(),
        });
    }
    Ok(())
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// mutter's `meta-udev.c` skips a tagged card (ADR 0015 §6). PCI ids, not a card number,
/// keep the rule inert on a machine without that device.
pub fn mutter_ignore_rules(profile: &Profile) -> Option<String> {
    let gpu = &profile.gpu.discrete;
    if gpu.drives_displays {
        return None;
    }
    Some(format!(
        "# Generated by apexctl profile render from hardware/{id}/profile.toml. Do not edit.\n\
         # [gpu.discrete] drives_displays = false: mutter never opens this GPU's DRM card.\n\
         SUBSYSTEM==\"drm\", KERNEL==\"card[0-9]*\", ENV{{DEVTYPE}}==\"drm_minor\", \
         SUBSYSTEMS==\"pci\", \
         ATTRS{{vendor}}==\"0x{vendor:04x}\", ATTRS{{device}}==\"0x{device:04x}\", \
         TAG+=\"mutter-device-ignore\"\n",
        id = profile.machine.id,
        vendor = gpu.vendor,
        device = gpu.device,
    ))
}

/// mutter matches the monitor spec exactly and the rate within 0.001 Hz, so both are
/// written as mutter reports them.
pub fn monitors_xml(profile: &Profile) -> String {
    let panel = &profile.panel;
    let rate = panel.preferred_rate().unwrap_or_default();
    format!(
        "<!-- Generated by apexctl profile render from hardware/{id}/profile.toml. Do not edit.\n\
         \x20    Default scale {scale_label} for the built-in panel only (ADR 0003). A scale chosen in\n\
         \x20    Settings is saved to the user's own monitors.xml and takes precedence. -->\n\
         <monitors version=\"2\">\n\
         \x20 <configuration>\n\
         \x20   <layoutmode>logical</layoutmode>\n\
         \x20   <logicalmonitor>\n\
         \x20     <x>0</x>\n\
         \x20     <y>0</y>\n\
         \x20     <scale>{scale}</scale>\n\
         \x20     <primary>yes</primary>\n\
         \x20     <monitor>\n\
         \x20       <monitorspec>\n\
         \x20         <connector>{connector}</connector>\n\
         \x20         <vendor>{vendor}</vendor>\n\
         \x20         <product>{product}</product>\n\
         \x20         <serial>{serial}</serial>\n\
         \x20       </monitorspec>\n\
         \x20       <mode>\n\
         \x20         <width>{width}</width>\n\
         \x20         <height>{height}</height>\n\
         \x20         <rate>{rate}</rate>\n\
         \x20       </mode>\n\
         \x20     </monitor>\n\
         \x20   </logicalmonitor>\n\
         \x20 </configuration>\n\
         </monitors>\n",
        id = profile.machine.id,
        scale_label = panel.default_scale,
        scale = panel.default_scale.xml_text(),
        connector = xml_escape(&panel.connector),
        vendor = xml_escape(&panel.edid_vendor),
        product = xml_escape(&panel.edid_product),
        serial = xml_escape(&panel.edid_serial),
        width = panel.width_px,
        height = panel.height_px,
        rate = crate::scale::format_g17(rate),
    )
}

/// Verb lines use the kernel's `init_verbs` format, so they match the sysfs file byte for byte.
pub fn patch_firmware(
    profile: &Profile,
    writes: &[CoefWrite],
    unchanged: &[UnchangedAfterWindows],
) -> String {
    let codec = &profile.audio.codec;
    let chip = codec.vendor_id & 0xffff;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# HDA patch firmware for the Realtek ALC{chip:X} of the {} (ADR 0004).",
        profile.machine.dmi_board_name
    );
    let _ = writeln!(
        out,
        "# Generated by apexctl profile render from hardware/{}/profile.toml. Do not edit.",
        profile.machine.id
    );
    let _ = writeln!(
        out,
        "# Installed as /usr/lib/firmware/{}; {AUDIO_CONF} loads it.",
        firmware_reference(profile)
    );
    let _ = writeln!(
        out,
        "# The kernel replays the [verb] lines at probe and on every runtime and system resume."
    );
    if let Some(m) = &profile.audio.measurements {
        let _ = writeln!(out, "# Measured on {}, BIOS {}.", m.date, m.bios);
        let _ = writeln!(out, "# Kernel during the measurement: {}.", m.kernel);
    }
    let _ = writeln!(out, "[codec]");
    let _ = writeln!(
        out,
        "{:#010x} {:#010x} {}",
        codec.vendor_id, codec.subsystem_id, codec.address
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "[verb]");
    for u in unchanged {
        let _ = writeln!(
            out,
            "# COEF {:#04x}: {:#06x} after cold boot and after Windows; not written.",
            u.index, u.value
        );
    }
    for write in writes {
        match write.reason {
            WriteReason::Rule { cold, set, clear } => {
                let _ = writeln!(
                    out,
                    "# COEF {:#04x}: cold boot {cold:#06x}, clear {clear:#06x}, set {set:#06x} -> {:#06x}.",
                    write.index, write.value
                );
            }
            WriteReason::RestoreCold {
                cold,
                after_windows,
            } => {
                let _ = writeln!(
                    out,
                    "# COEF {:#04x}: restore the cold-boot value {cold:#06x} (after Windows: {after_windows:#06x}).",
                    write.index
                );
            }
        }
        for verb in coef_verbs(profile, std::slice::from_ref(write)) {
            let _ = writeln!(out, "{verb}");
        }
    }
    out
}

pub fn modprobe_conf(profile: &Profile) -> String {
    let reference = firmware_reference(profile);
    let entries = [reference.as_str(); PATCH_SLOTS].join(",");
    format!(
        "# Generated by apexctl profile render from hardware/{id}/profile.toml. Do not edit.\n\
         # Loads the ALC{chip:X} HDA patch firmware (ADR 0004). snd-hda-intel assigns patch=\n\
         # slots in controller probe order, not by PCI address, so every slot names the same\n\
         # file; its [codec] line makes the kernel apply it to the ALC{chip:X} only.\n\
         options snd-hda-intel patch={entries}\n",
        id = profile.machine.id,
        chip = profile.audio.codec.vendor_id & 0xffff,
    )
}

pub fn default_output_dir(profile_path: &Path) -> PathBuf {
    profile_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(GENERATED_DIR)
}

pub fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(old_label, new_label)
        .to_string()
}

#[derive(Debug, thiserror::Error)]
#[error("cannot {action} {path}: {error}")]
pub struct OutputError {
    pub action: &'static str,
    pub path: PathBuf,
    pub error: io::Error,
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> OutputError {
    let path = path.to_path_buf();
    move |error| OutputError {
        action,
        path,
        error,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    Written,
    Unchanged,
    Removed,
    Absent,
}

fn read_existing(path: &Path) -> Result<Option<Vec<u8>>, OutputError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error("read", path)(e)),
    }
}

pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), OutputError> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)
            .map_err(io_error("create", &tmp))?;
        // Explicit, so the result does not depend on the umask.
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(io_error("chmod", &tmp))?;
        file.write_all(contents).map_err(io_error("write", &tmp))?;
        file.sync_all().map_err(io_error("flush", &tmp))?;
        fs::rename(&tmp, path).map_err(io_error("rename", path))?;
        fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(io_error("flush", dir))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn write_outputs(
    dir: &Path,
    rendered: &Rendered,
) -> Result<Vec<(String, FileAction)>, OutputError> {
    fs::create_dir_all(dir).map_err(io_error("create", dir))?;
    let mut actions = Vec::new();
    for file in &rendered.files {
        let path = dir.join(&file.name);
        let existing = read_existing(&path)?;
        let action = match (&file.contents, existing) {
            (Some(contents), Some(old)) if old == contents.as_bytes() => FileAction::Unchanged,
            (Some(contents), _) => {
                write_atomic(&path, contents.as_bytes())?;
                FileAction::Written
            }
            (None, Some(_)) => {
                fs::remove_file(&path).map_err(io_error("remove", &path))?;
                FileAction::Removed
            }
            (None, None) => FileAction::Absent,
        };
        actions.push((file.name.clone(), action));
    }
    Ok(actions)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    pub name: String,
    pub diff: String,
}

pub fn check_outputs(dir: &Path, rendered: &Rendered) -> Result<Vec<Difference>, OutputError> {
    let mut differences = Vec::new();
    for file in &rendered.files {
        let path = dir.join(&file.name);
        let shown = path.display().to_string();
        let existing = read_existing(&path)?.map(|b| String::from_utf8_lossy(&b).into_owned());
        let diff = match (&file.contents, &existing) {
            (Some(new), Some(old)) if old == new => None,
            (Some(new), Some(old)) => Some(unified_diff(
                old,
                new,
                &format!("{shown} (committed)"),
                &format!("{shown} (rendered)"),
            )),
            (Some(new), None) => Some(unified_diff(
                "",
                new,
                "/dev/null",
                &format!("{shown} (rendered)"),
            )),
            (None, Some(old)) => Some(unified_diff(
                old,
                "",
                &format!("{shown} (committed, stale)"),
                "/dev/null",
            )),
            (None, None) => None,
        };
        if let Some(diff) = diff {
            differences.push(Difference {
                name: file.name.clone(),
                diff,
            });
        }
    }
    Ok(differences)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::tests::{BASE, MEASURED};
    use proptest::prelude::*;

    fn measured() -> Profile {
        Profile::parse(&format!("{BASE}{MEASURED}")).unwrap()
    }

    #[test]
    fn names_follow_the_contract() {
        let profile = Profile::parse(BASE).unwrap();
        assert_eq!(firmware_file_name(&profile), "alc294-m7400qc.fw");
        assert_eq!(firmware_reference(&profile), "apex/alc294-m7400qc.fw");
        assert_eq!(
            default_output_dir(Path::new("hardware/m7400qc/profile.toml")),
            Path::new("hardware/m7400qc/generated")
        );
    }

    #[test]
    fn without_measurements_there_is_no_audio_output() {
        let rendered = render(&Profile::parse(BASE).unwrap()).unwrap();
        assert_eq!(rendered.audio, AudioOutcome::NotMeasured);
        let names: Vec<(&str, bool)> = rendered
            .files
            .iter()
            .map(|f| (f.name.as_str(), f.contents.is_some()))
            .collect();
        assert_eq!(
            names,
            [
                ("monitors.xml", true),
                ("alc294-m7400qc.fw", false),
                ("apex-audio.conf", false),
                (MUTTER_IGNORE_RULES, true)
            ]
        );
    }

    #[test]
    fn coef_plan_uses_rules_and_restores_only_what_windows_changed() {
        let CoefPlan { writes, unchanged } = plan_coef_writes(&measured()).unwrap().unwrap();
        assert_eq!(
            writes,
            [
                CoefWrite {
                    index: 0x0f,
                    value: 0x7770,
                    reason: WriteReason::RestoreCold {
                        cold: 0x7770,
                        after_windows: 0x7774
                    }
                },
                CoefWrite {
                    index: 0x1b,
                    value: 0x4e4b,
                    reason: WriteReason::Rule {
                        cold: 0x4a4b,
                        set: 0x0400,
                        clear: 0
                    }
                },
            ]
        );
        assert_eq!(
            unchanged,
            [UnchangedAfterWindows {
                index: 0x40,
                value: 0x8800
            }]
        );
    }

    #[test]
    fn rendered_firmware_parses_back_to_the_planned_verbs() {
        let profile = measured();
        let rendered = render(&profile).unwrap();
        let firmware = rendered.files[1].contents.as_deref().unwrap();
        let parsed = crate::patch::parse(firmware).unwrap();
        assert_eq!(parsed.codec.to_string(), "0x10ec0294 0x10431ab2 0");
        let AudioOutcome::Rendered(writes) = &rendered.audio else {
            panic!("audio not rendered");
        };
        assert_eq!(parsed.verbs, coef_verbs(&profile, writes));
    }

    #[test]
    fn the_renderer_rereads_its_firmware_like_the_kernel() {
        let profile = measured();
        let CoefPlan { writes, unchanged } = plan_coef_writes(&profile).unwrap().unwrap();
        let good = patch_firmware(&profile, &writes, &unchanged);
        let check = |text: &str| verify_firmware(&profile, "f.fw", text, &writes);
        assert!(check(&good).is_ok());

        let (first, rest) = good.split_once('\n').unwrap();
        let injected = format!("{first}\n[pincfg]\n0x19 0x411111f0\n{rest}");
        assert!(matches!(
            check(&injected),
            Err(RenderError::InvalidFirmware {
                error: PatchError::UnsupportedSection { line: 2, .. },
                ..
            })
        ));
        let nul = good.replacen(" (ADR 0004)", "\0 (ADR 0004)", 1);
        assert!(matches!(
            check(&nul),
            Err(RenderError::InvalidFirmware {
                error: PatchError::ControlCharacter { line: 1, byte: 0 },
                ..
            })
        ));
        let extra = format!("{good}0x20 0x500 0x0001\n");
        assert!(matches!(
            check(&extra),
            Err(RenderError::FirmwareMismatch { .. })
        ));
        let other_codec = good.replace("0x10431ab2", "0x10431ab3");
        assert!(matches!(
            check(&other_codec),
            Err(RenderError::FirmwareMismatch { .. })
        ));
        let error = check(&nul).unwrap_err().to_string();
        assert!(
            error.starts_with("f.fw would not load as rendered: line 1: control character 0x00"),
            "{error}"
        );
    }

    #[test]
    fn a_profile_changed_after_parsing_is_validated_again() {
        for board in ["M7400QC\n[pincfg]\n0x19 0x411111f0", "M7400QC\0"] {
            let mut profile = measured();
            profile.machine.dmi_board_name = board.to_owned();
            let error = render(&profile).unwrap_err().to_string();
            assert!(error.starts_with("machine.dmi_board_name"), "{error}");
        }
    }

    #[test]
    fn modprobe_line_has_four_identical_slots() {
        let conf = modprobe_conf(&measured());
        let options: Vec<&str> = conf.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            options,
            [
                "options snd-hda-intel patch=apex/alc294-m7400qc.fw,apex/alc294-m7400qc.fw,apex/alc294-m7400qc.fw,apex/alc294-m7400qc.fw"
            ]
        );
    }

    #[test]
    fn the_dgpu_is_ignored_by_its_pci_ids_only_when_it_drives_no_display() {
        let mut profile = Profile::parse(BASE).unwrap();
        let rules = mutter_ignore_rules(&profile).unwrap();
        let rule = rules.lines().find(|l| !l.starts_with('#')).unwrap();
        assert_eq!(
            rule,
            "SUBSYSTEM==\"drm\", KERNEL==\"card[0-9]*\", ENV{DEVTYPE}==\"drm_minor\", \
             SUBSYSTEMS==\"pci\", \
             ATTRS{vendor}==\"0x10de\", ATTRS{device}==\"0x25a2\", TAG+=\"mutter-device-ignore\""
        );
        assert!(!rules.contains("card0") && !rules.contains("0000:01:00.0"));
        assert_eq!(rules.lines().count(), 3);
        profile.gpu.discrete.drives_displays = true;
        assert_eq!(mutter_ignore_rules(&profile), None);
        let rendered = render(&profile).unwrap();
        let file = rendered
            .files
            .iter()
            .find(|f| f.name == MUTTER_IGNORE_RULES)
            .unwrap();
        assert_eq!(file.contents, None, "a stale rule is removed");
    }

    #[test]
    fn monitors_xml_never_contains_a_double_hyphen_inside_the_comment() {
        let xml = monitors_xml(&measured());
        let comment_end = xml.find("-->").unwrap();
        assert!(!xml[4..comment_end].contains("--"));
    }

    #[test]
    fn write_then_check_is_clean_and_stale_audio_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let measured = render(&measured()).unwrap();
        let actions = write_outputs(dir.path(), &measured).unwrap();
        assert!(actions.iter().all(|(_, a)| *a == FileAction::Written));
        assert!(check_outputs(dir.path(), &measured).unwrap().is_empty());
        let again = write_outputs(dir.path(), &measured).unwrap();
        assert!(again.iter().all(|(_, a)| *a == FileAction::Unchanged));

        let bare = render(&Profile::parse(BASE).unwrap()).unwrap();
        let differences = check_outputs(dir.path(), &bare).unwrap();
        let names: Vec<&str> = differences.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["alc294-m7400qc.fw", "apex-audio.conf"]);
        assert!(differences[0].diff.contains("+++ /dev/null"));
        assert!(differences[0].diff.contains("-[codec]"));
        let actions = write_outputs(dir.path(), &bare).unwrap();
        assert_eq!(actions[0].1, FileAction::Unchanged);
        assert_eq!(actions[1].1, FileAction::Removed);
        assert_eq!(actions[2].1, FileAction::Removed);
        assert!(!dir.path().join("alc294-m7400qc.fw").exists());
        assert!(check_outputs(dir.path(), &bare).unwrap().is_empty());
        let mode = {
            use std::os::unix::fs::PermissionsExt as _;
            fs::metadata(dir.path().join(MONITORS_XML))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode, 0o644);
    }

    proptest! {
        #[test]
        fn every_measurement_renders_a_loadable_patch(
            cold in proptest::collection::btree_map(any::<u16>(), any::<u16>(), 0..6),
            c1b: u16, c0f: u16, c40: u16, a0f: u16, a40: u16,
        ) {
            let mut cold = cold;
            cold.insert(0x1b, c1b);
            cold.insert(0x0f, c0f);
            cold.insert(0x40, c40);
            let list = |m: &std::collections::BTreeMap<u16, u16>| {
                m.iter()
                    .map(|(i, v)| format!("{{ index = {i}, value = {v} }}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let after: std::collections::BTreeMap<u16, u16> =
                [(0x0f, a0f), (0x1b, c1b), (0x40, a40)].into_iter().collect();
            let toml = format!(
                "{BASE}\n[audio.measurements]\ndate = 2026-09-25\nbios = 'b'\nkernel = 'k'\n\
                 cold_boot = [{}]\nafter_windows = [{}]\n",
                list(&cold),
                list(&after)
            );
            let profile = Profile::parse(&toml).unwrap();
            let rendered = render(&profile).unwrap();
            let firmware = rendered.files[1].contents.clone().unwrap();
            let parsed = crate::patch::parse(&firmware).unwrap();
            let effective: Vec<Verb> = parsed.verbs.iter().map(|v| v.effective()).collect();
            prop_assert_eq!(effective.len() % 2, 0);
            let pos = effective.iter().position(|v| *v == Verb::SetCoefIndex(0x1b)).unwrap();
            prop_assert_eq!(effective[pos + 1], Verb::SetProcCoef(c1b | 0x0400));
            let expect_0f = a0f != c0f;
            prop_assert_eq!(effective.contains(&Verb::SetCoefIndex(0x0f)), expect_0f);
            let expect_40 = a40 != c40;
            prop_assert_eq!(effective.contains(&Verb::SetCoefIndex(0x40)), expect_40);
        }
    }
}
