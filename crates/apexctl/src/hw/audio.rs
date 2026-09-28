//! `apexctl hw audio snapshot`: the ALC294's codec dump and loaded init verbs.
//!
//! Reading `/proc/asound/cardN/codec#M` powers that codec up (the kernel's
//! `print_codec_info` takes a runtime-PM reference), and with it its controller: an HDMI
//! codec on a GPU's HDA function would wake the GPU. So the ALC294 is found first through
//! the cached `vendor_id` and `subsystem_id` of its hwdep in `/sys/class/sound`, which
//! wake nothing, and only its own codec file is opened.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::machine::{ALC294_SUBSYSTEM_ID, ALC294_VENDOR_ID, COEF_NID};
use super::report::find_alc294_hwdep;
use super::{SysRoot, parse_hex, read_trimmed};
use crate::codec::{self, CodecDump, CodecParseError};
use crate::hda::InitVerb;

/// COEF indexes ADR 0004 watches, with what they are.
pub const WATCHED_COEFS: &[(u16, &str)] = &[
    (0x0f, "EAPD coefficient (ALC294_FIXUP_ASUS_HPE)"),
    (0x1b, "bit 10 is the suspected Windows fault"),
    (0x40, "written by ALC294_FIXUP_ASUS_SPK"),
];

/// Bit 10 of COEF 0x1b.
const COEF_1B_BIT10: u16 = 0x0400;

/// Why no snapshot could be taken.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// A file could not be read.
    #[error("cannot read {path}: {reason}")]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// A dump did not parse.
    #[error("{path}: {error}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// The parse error. It is part of the message, so it is not also the `source()`.
        error: CodecParseError,
    },
    /// No ALC294 among the codecs.
    #[error(
        "no ALC294 ({ALC294_VENDOR_ID:#010x}, subsystem {ALC294_SUBSYSTEM_ID:#010x}) under {searched}; codecs seen: {seen}. To parse a saved codec dump instead: apexctl hw audio snapshot --proc FILE"
    )]
    NotFound {
        /// The directory searched.
        searched: String,
        /// The codecs found instead.
        seen: String,
    },
    /// The codec file of the ALC294's hwdep describes another codec.
    #[error(
        "{path} describes codec {vendor_id:#010x} (subsystem {subsystem_id:#010x}), not the ALC294 that {hwdep} names"
    )]
    WrongCodec {
        /// The codec file.
        path: PathBuf,
        /// The hwdep, e.g. `hwC1D0`.
        hwdep: String,
        /// Vendor id in the file.
        vendor_id: u32,
        /// Subsystem id in the file.
        subsystem_id: u32,
    },
}

/// A snapshot of the codec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioSnapshot {
    /// The dump that was parsed.
    pub source: String,
    /// ALSA card number, from the hwdep (`None` with `--proc`).
    pub card: Option<u32>,
    /// The parsed dump.
    pub codec: CodecDump,
    /// The hwdep, e.g. `hwC1D0` (not looked up with `--proc`).
    pub hwdep: Option<String>,
    /// Lines of its `init_verbs` (`None`: not read).
    pub init_verbs: Option<Vec<String>>,
}

/// `hwC1D0` → card 1, codec address 0.
fn hwdep_numbers(name: &str) -> Option<(u32, u32)> {
    let (card, address) = name.strip_prefix("hwC")?.split_once('D')?;
    let number = |digits: &str| {
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse().ok())
            .flatten()
    };
    Some((number(card)?, number(address)?))
}

fn read_dump(path: &Path) -> Result<CodecDump, SnapshotError> {
    let text = match read_trimmed(path) {
        Ok(Some(text)) => text,
        Ok(None) => {
            return Err(SnapshotError::Read {
                path: path.to_path_buf(),
                reason: "no such file".to_owned(),
            });
        }
        Err(reason) => {
            return Err(SnapshotError::Read {
                path: path.to_path_buf(),
                reason,
            });
        }
    };
    codec::parse(&text).map_err(|error| SnapshotError::Parse {
        path: path.to_path_buf(),
        error,
    })
}

/// Parses a dump file given with `--proc`.
pub fn snapshot_file(path: &Path) -> Result<AudioSnapshot, SnapshotError> {
    Ok(AudioSnapshot {
        source: path.display().to_string(),
        card: None,
        codec: read_dump(path)?,
        hwdep: None,
        init_verbs: None,
    })
}

/// The hwdeps under `/sys/class/sound` with their cached ids, for the not-found message.
fn hwdeps_seen(root: &SysRoot) -> String {
    let seen: Vec<String> = root
        .list("/sys/class/sound")
        .into_iter()
        .filter(|name| hwdep_numbers(name).is_some())
        .map(|name| {
            let id = |attr: &str| {
                root.read(&format!("/sys/class/sound/{name}/{attr}"))
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(parse_hex)
                    .map_or_else(|| "?".to_owned(), |id| format!("{id:#010x}"))
            };
            format!("{name} {}/{}", id("vendor_id"), id("subsystem_id"))
        })
        .collect();
    if seen.is_empty() {
        "none".to_owned()
    } else {
        seen.join(", ")
    }
}

/// Finds the ALC294 by its hwdep's cached ids in `/sys/class/sound`, then parses only its
/// own codec file, `/proc/asound/card<N>/codec#<address>`, and reads its `init_verbs`.
///
/// No other codec file is opened: reading one powers that codec up (see the module
/// documentation). An ALC294 dump that does not parse is an error with the line, not a
/// missing codec.
pub fn snapshot(root: &SysRoot) -> Result<AudioSnapshot, SnapshotError> {
    let found = find_alc294_hwdep(root)
        .and_then(|hwdep| hwdep_numbers(&hwdep).map(|numbers| (hwdep, numbers)));
    let Some((hwdep, (card, address))) = found else {
        return Err(SnapshotError::NotFound {
            searched: root.path("/sys/class/sound").display().to_string(),
            seen: hwdeps_seen(root),
        });
    };
    let path = root.path(&format!("/proc/asound/card{card}/codec#{address}"));
    let dump = read_dump(&path)?;
    if dump.vendor_id != ALC294_VENDOR_ID || dump.subsystem_id != ALC294_SUBSYSTEM_ID {
        return Err(SnapshotError::WrongCodec {
            path,
            hwdep,
            vendor_id: dump.vendor_id,
            subsystem_id: dump.subsystem_id,
        });
    }
    let init_verbs = root
        .read(&format!("/sys/class/sound/{hwdep}/init_verbs"))
        .ok()
        .flatten()
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect()
        });
    Ok(AudioSnapshot {
        source: path.display().to_string(),
        card: Some(card),
        codec: dump,
        hwdep: Some(hwdep),
        init_verbs,
    })
}

/// The human-readable snapshot.
pub fn render(snapshot: &AudioSnapshot) -> String {
    let dump = &snapshot.codec;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} (vendor {:#010x}, subsystem {:#010x}, revision {:#x})",
        dump.codec, dump.vendor_id, dump.subsystem_id, dump.revision_id
    );
    let _ = write!(out, "Source: {}", snapshot.source);
    if let (Some(card), Some(address)) = (snapshot.card, dump.address) {
        let _ = write!(out, " (card {card}, codec address {address})");
    }
    let _ = writeln!(out, "\n");
    render_pins(&mut out, dump);
    let _ = writeln!(out);
    render_coefs(&mut out, dump);
    let _ = writeln!(out);
    render_init_verbs(&mut out, snapshot);
    out
}

/// One row per pin complex.
fn render_pins(out: &mut String, dump: &CodecDump) {
    let _ = writeln!(out, "Pins");
    let _ = writeln!(
        out,
        "  NID   Default     Pin-ctls           EAPD  Connections       Description"
    );
    for pin in dump.pins() {
        let default = pin
            .pin_default
            .as_ref()
            .map_or_else(|| "-".to_owned(), |d| format!("{:#010x}", d.value));
        let ctls = pin.pin_ctls.as_ref().map_or_else(
            || "-".to_owned(),
            |c| {
                format!("{:#04x} {}", c.value, c.flags.join(" "))
                    .trim_end()
                    .to_owned()
            },
        );
        let eapd = pin
            .eapd
            .as_ref()
            .map_or_else(|| "-".to_owned(), |e| format!("{:#x}", e.value));
        let connections = if pin.connections.is_empty() {
            "-".to_owned()
        } else {
            pin.connections
                .iter()
                .map(|nid| {
                    // The kernel marks the selected input only when there is a choice.
                    let selected =
                        pin.selected_connection == Some(*nid) && pin.connections.len() > 1;
                    format!("{nid:#04x}{}", if selected { "*" } else { "" })
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        let description = pin
            .pin_default
            .as_ref()
            .map_or("", |d| d.description.as_str());
        let _ = writeln!(
            out,
            "  {:#04x}  {default:<10}  {ctls:<17}  {eapd:<4}  {connections:<16}  {description}",
            pin.nid
        );
    }
}

/// The watched COEFs and the full table, or how to get them.
fn render_coefs(out: &mut String, dump: &CodecDump) {
    let Some(widget) = dump.node(COEF_NID) else {
        let _ = writeln!(out, "COEFs: no node {COEF_NID:#04x} in this dump");
        return;
    };
    if widget.coefs.is_empty() && widget.unreadable_coefs.is_empty() {
        let _ = writeln!(
            out,
            "COEFs (NID {COEF_NID:#04x}, {} slots): not in this dump. To include them:\n  \
             echo 1 | sudo tee /sys/module/snd_hda_codec/parameters/dump_coef",
            widget.ncoeff.unwrap_or(0)
        );
        return;
    }
    let _ = writeln!(
        out,
        "COEFs (NID {COEF_NID:#04x}, {} slots)",
        widget.coefs.len() + widget.unreadable_coefs.len()
    );
    for &(index, meaning) in WATCHED_COEFS {
        if let Some(raw) = widget.unreadable_coefs.get(&index) {
            let _ = writeln!(out, "  {index:#04x} = not readable ({raw:#x})  ({meaning})");
            continue;
        }
        match widget.coefs.get(&index) {
            Some(&value) if index == 0x1b => {
                let bit = if value & COEF_1B_BIT10 == 0 {
                    "CLEAR"
                } else {
                    "set"
                };
                let _ = writeln!(
                    out,
                    "  {index:#04x} = {value:#06x}  bit 10 {bit} ({meaning})"
                );
            }
            Some(value) => {
                let _ = writeln!(out, "  {index:#04x} = {value:#06x}  ({meaning})");
            }
            None => {}
        }
    }
    let _ = writeln!(out, "  All:");
    let entries: Vec<String> = widget
        .coefs
        .iter()
        .map(|(i, v)| format!("{i:#04x}={v:#06x}"))
        .collect();
    for row in entries.chunks(6) {
        let _ = writeln!(out, "    {}", row.join("  "));
    }
    if !widget.unreadable_coefs.is_empty() {
        let failed: Vec<String> = widget
            .unreadable_coefs
            .iter()
            .map(|(i, raw)| format!("{i:#04x} ({raw:#x})"))
            .collect();
        let _ = writeln!(
            out,
            "  Not readable (0xffffffff: the kernel's read of the slot failed): {}",
            failed.join(", ")
        );
    }
}

/// The loaded `init_verbs`, each with what it sends.
fn render_init_verbs(out: &mut String, snapshot: &AudioSnapshot) {
    match (&snapshot.hwdep, &snapshot.init_verbs) {
        (Some(hwdep), Some(verbs)) if verbs.is_empty() => {
            let _ = writeln!(
                out,
                "init_verbs of {hwdep}: empty (no patch firmware loaded)"
            );
        }
        (Some(hwdep), Some(verbs)) => {
            let _ = writeln!(out, "init_verbs of {hwdep}:");
            for line in verbs {
                match InitVerb::parse(line) {
                    Ok(verb) => {
                        let _ = writeln!(out, "  {line:<20}  {}", verb.effective());
                    }
                    Err(_) => {
                        let _ = writeln!(out, "  {line}");
                    }
                }
            }
        }
        (Some(hwdep), None) => {
            let _ = writeln!(out, "init_verbs of {hwdep}: not readable");
        }
        (None, _) => {
            let _ = writeln!(
                out,
                "init_verbs: not read (no matching hwdep, or --proc was given)"
            );
        }
    }
}
