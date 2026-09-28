//! `apexctl profile render`: the files derived from the hardware profile (contract §7).
//!
//! Into `hardware/<machine>/generated/`:
//!
//! - [`MONITORS_XML`], always: `/etc/xdg/monitors.xml` with the panel's default scale
//!   (ADR 0003).
//! - `alc294-<machine>.fw` and [`AUDIO_CONF`], only when `[audio.measurements]` exists
//!   (ADR 0004). Without measurements both must be absent, and rendering removes stale
//!   copies.
//!
//! [`render`] is pure; [`write_outputs`] and [`check_outputs`] do the file I/O. Output is
//! deterministic (it never contains the path the profile was read from, the time or the
//! user), so `--check` can compare bytes.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::hda::{InitVerb, Verb};
use crate::patch::{CodecLine, MAX_LINE, PatchError};
use crate::profile::{Profile, ProfileError, coef_map};

/// Name of the rendered `monitors.xml`.
pub const MONITORS_XML: &str = "monitors.xml";
/// Name of the rendered modprobe drop-in (installed as `/usr/lib/modprobe.d/apex-audio.conf`).
pub const AUDIO_CONF: &str = "apex-audio.conf";
/// Directory under `/usr/lib/firmware` that holds the patch firmware.
pub const FIRMWARE_SUBDIR: &str = "apex";
/// `snd-hda-intel` assigns `patch=` entries by controller probe order, not by PCI address,
/// so every slot names the same file (ADR 0004).
pub const PATCH_SLOTS: usize = 4;
/// Name of the output directory next to the profile.
pub const GENERATED_DIR: &str = "generated";
/// The firmware list the image check reads, relative to the repository root.
pub const FIRMWARE_LIST: &str = "image/checks/firmware-files.txt";

/// File name of the patch firmware, e.g. `alc294-m7400qc.fw` (from the codec's device id).
pub fn firmware_file_name(profile: &Profile) -> String {
    format!(
        "alc{:x}-{}.fw",
        profile.audio.codec.vendor_id & 0xffff,
        profile.machine.id
    )
}

/// The `patch=` entry: the firmware path relative to `/usr/lib/firmware`.
///
/// `request_firmware` joins the name to each firmware directory with `"%s/%s"` and only
/// refuses names with a `..` component (`drivers/base/firmware_loader/main.c`), so a
/// subdirectory is fine.
pub fn firmware_reference(profile: &Profile) -> String {
    format!("{FIRMWARE_SUBDIR}/{}", firmware_file_name(profile))
}

/// Why a COEF is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteReason {
    /// An `[audio.fix].rules` entry: `(cold & !clear) | set`.
    Rule {
        /// Cold-boot value.
        cold: u16,
        /// Bits set.
        set: u16,
        /// Bits cleared.
        clear: u16,
    },
    /// A `restore_if_windows_differs` index whose after-Windows value differs.
    RestoreCold {
        /// Cold-boot value (written).
        cold: u16,
        /// After-Windows value.
        after_windows: u16,
    },
}

/// One COEF the patch firmware writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoefWrite {
    /// COEF index.
    pub index: u16,
    /// Value written.
    pub value: u16,
    /// Where the value comes from.
    pub reason: WriteReason,
}

/// A `restore_if_windows_differs` index that did not change after Windows (not written).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unchanged {
    /// COEF index.
    pub index: u16,
    /// The value in both dumps.
    pub value: u16,
}

/// The COEF writes of the audio fix and the checked indexes that stay untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoefPlan {
    /// Writes, in ascending index order, one per index.
    pub writes: Vec<CoefWrite>,
    /// `restore_if_windows_differs` indexes that did not change, in index order.
    pub unchanged: Vec<Unchanged>,
}

/// Plans the COEF writes of the audio fix. `None` without `[audio.measurements]`.
///
/// Each index is written at most once; a rule wins over a restore of the same index (the
/// rule starts from the cold value anyway).
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

    // BTreeMap: ascending index order and one entry per index.
    let mut writes = std::collections::BTreeMap::new();
    for rule in &fix.rules {
        let cold_value = *cold
            .get(&rule.index)
            .ok_or_else(|| missing("audio.measurements.cold_boot", rule.index))?;
        writes.insert(
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
        if writes.contains_key(&index) {
            continue;
        }
        if after_value == cold_value {
            unchanged.push(Unchanged {
                index,
                value: cold_value,
            });
        } else {
            writes.insert(
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
        writes: writes.into_values().collect(),
        unchanged,
    }))
}

/// The `[verb]` triples for `writes` on the COEF widget: index, then value, per COEF.
pub fn coef_verbs(profile: &Profile, writes: &[CoefWrite]) -> Vec<InitVerb> {
    let nid = u16::from(profile.audio.codec.coef_nid);
    writes
        .iter()
        .flat_map(|w| [Verb::SetCoefIndex(w.index), Verb::SetProcCoef(w.value)])
        .filter_map(|verb| InitVerb::new(nid, verb).ok())
        .collect()
}

/// One file of the output directory: its contents, or `None` when it must not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    /// File name inside the output directory.
    pub name: String,
    /// Contents, or `None` for a file that must be absent.
    pub contents: Option<String>,
}

/// What happened to the audio fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioOutcome {
    /// Rendered with these writes.
    Rendered(Vec<CoefWrite>),
    /// No `[audio.measurements]`: no firmware (the image is labelled `degraded=audio`).
    NotMeasured,
}

/// Everything `profile render` produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// Every file the renderer owns, in a fixed order.
    pub files: Vec<OutputFile>,
    /// The audio part.
    pub audio: AudioOutcome,
}

/// Why rendering failed.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// The profile is invalid.
    #[error(transparent)]
    Profile(#[from] ProfileError),
    /// A patch firmware line would exceed the kernel's limit.
    #[error(
        "{file} line {line} has {length} characters; the kernel's patch loader stops at {MAX_LINE}"
    )]
    LineTooLong {
        /// File name.
        file: String,
        /// Line number.
        line: usize,
        /// Its length.
        length: usize,
    },
    /// The measurements and the fix produce no verb at all.
    #[error("[audio.fix] writes no COEF with these measurements; nothing to render")]
    NoVerbs,
    /// The rendered patch firmware is not a file the kernel would load as intended.
    #[error("{file} would not load as rendered: {error}")]
    InvalidFirmware {
        /// File name.
        file: String,
        /// Why [`crate::patch::parse`] rejects it (part of the message, not a `source()`).
        error: PatchError,
    },
    /// The rendered patch firmware parses, but not to the profile's codec and the planned
    /// verbs: text copied from the profile changed what the kernel would read.
    #[error("{file} does not parse back to the profile's [codec] line and the planned verbs")]
    FirmwareMismatch {
        /// File name.
        file: String,
    },
}

/// Renders every output file from a profile. Validates the profile first.
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
    Ok(Rendered { files, audio })
}

/// Reads the rendered firmware back the way the kernel will ([`crate::patch::parse`]
/// mirrors `snd_hda_load_patch`) and checks that it names exactly the profile's codec and
/// carries exactly the planned verbs, in order. Profile text is copied into comment lines;
/// whatever it contains, it must not change what the kernel reads or make it hang.
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

/// Escapes XML text (the profile validation already limits the characters).
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

/// `monitors.xml` (layout mode logical, version 2) with the panel alone at the default
/// scale and the highest refresh rate. mutter matches the monitor by connector, vendor,
/// product and serial, and the mode rate within 0.001 Hz, so both are written exactly as
/// mutter reports them (`%.17g`).
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

/// The HDA patch firmware: provenance comments, the `[codec]` line and one index/value
/// pair per COEF. Verb lines use the kernel's `init_verbs` format, so they compare
/// byte-for-byte with `/sys/class/sound/hwC*D*/init_verbs`.
pub fn patch_firmware(profile: &Profile, writes: &[CoefWrite], unchanged: &[Unchanged]) -> String {
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

/// The modprobe drop-in that makes `snd-hda-intel` load the patch firmware.
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

/// Where rendered files go by default: `generated/` next to the profile.
pub fn default_output_dir(profile_path: &Path) -> PathBuf {
    profile_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(GENERATED_DIR)
}

/// The repository's firmware list for a profile at `<root>/hardware/<machine>/profile.toml`,
/// or `None` when the profile is not inside such a tree.
pub fn firmware_list_path(profile_path: &Path) -> Option<PathBuf> {
    let machine_dir = profile_path.parent()?;
    let hardware_dir = machine_dir.parent()?;
    if hardware_dir.file_name()? != "hardware" {
        return None;
    }
    Some(hardware_dir.parent()?.join(FIRMWARE_LIST))
}

/// The globs of `image/checks/firmware-files.txt`: one per line, `#` starts a comment.
pub fn parse_firmware_list(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A unified diff between the profile's `[firmware].required_files` and the list file,
/// or `None` when they are identical (same globs, same order).
pub fn compare_firmware_list(profile: &Profile, list_text: &str) -> Option<String> {
    let listed = parse_firmware_list(list_text);
    if listed == profile.firmware.required_files {
        return None;
    }
    let mut from_profile = profile.firmware.required_files.join("\n");
    from_profile.push('\n');
    let mut from_list = listed.join("\n");
    from_list.push('\n');
    Some(unified_diff(
        &from_profile,
        &from_list,
        "profile.toml [firmware].required_files",
        FIRMWARE_LIST,
    ))
}

/// A unified diff with three lines of context.
pub fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(old_label, new_label)
        .to_string()
}

/// A file I/O failure while writing or checking outputs.
#[derive(Debug, thiserror::Error)]
#[error("cannot {action} {path}: {error}")]
pub struct OutputError {
    /// What was attempted.
    pub action: &'static str,
    /// The path.
    pub path: PathBuf,
    /// The I/O error. It is part of the message, so it is not also the `source()`.
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

/// What [`write_outputs`] did with one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    /// Created or replaced.
    Written,
    /// Already had the rendered contents; left alone.
    Unchanged,
    /// A stale file was deleted.
    Removed,
    /// Must be absent and was.
    Absent,
}

fn read_existing(path: &Path) -> Result<Option<Vec<u8>>, OutputError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error("read", path)(e)),
    }
}

/// Writes `contents` to `path` atomically (temporary file, `fsync`, rename), mode 0644.
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
        // Best effort: the original error is what matters.
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Writes the rendered files into `dir` (created if missing), removes files that must be
/// absent, and leaves files with identical contents untouched.
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

/// One output file whose committed state differs from the rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    /// File name.
    pub name: String,
    /// Unified diff from the committed file to the rendered one.
    pub diff: String,
}

/// Compares `dir` with the rendering without changing anything.
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
        assert_eq!(
            firmware_list_path(Path::new("hardware/m7400qc/profile.toml")),
            Some(PathBuf::from("image/checks/firmware-files.txt"))
        );
        assert_eq!(
            firmware_list_path(Path::new("/r/hardware/m7400qc/profile.toml")),
            Some(PathBuf::from("/r/image/checks/firmware-files.txt"))
        );
        assert_eq!(firmware_list_path(Path::new("profile.toml")), None);
        assert_eq!(firmware_list_path(Path::new("x/m/profile.toml")), None);
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
                ("apex-audio.conf", false)
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
            [Unchanged {
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

        // What an unvalidated profile string could have done to the first comment line.
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
        // Parses, but not to the plan: an extra verb, or another codec.
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
        // `render` validates on its own, so a profile built or edited in code cannot slip a
        // line feed or a NUL into the firmware's comment lines either.
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
    fn monitors_xml_never_contains_a_double_hyphen_inside_the_comment() {
        let xml = monitors_xml(&measured());
        let comment_end = xml.find("-->").unwrap();
        assert!(!xml[4..comment_end].contains("--"));
    }

    #[test]
    fn firmware_list_comparison() {
        let profile = Profile::parse(BASE).unwrap();
        assert_eq!(
            compare_firmware_list(&profile, "# c\n\namdgpu/green_sardine_*   # the iGPU\n"),
            None
        );
        let diff = compare_firmware_list(&profile, "amdgpu/renoir_*\n").unwrap();
        assert!(diff.contains("-amdgpu/green_sardine_*"));
        assert!(diff.contains("+amdgpu/renoir_*"));
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

        // The profile loses its measurements: the audio files become stale.
        let bare = render(&Profile::parse(BASE).unwrap()).unwrap();
        let differences = check_outputs(dir.path(), &bare).unwrap();
        let names: Vec<&str> = differences.iter().map(|d| d.name.as_str()).collect();
        // monitors.xml does not depend on the audio measurements.
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
            // Every index is followed by its value, and 0x1b always gets bit 10.
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
