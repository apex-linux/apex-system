use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::machine::{ALC294_SUBSYSTEM_ID, ALC294_VENDOR_ID, COEF_NID};
use super::report::find_alc294_hwdep;
use super::{SysRoot, parse_hex, read_trimmed};
use crate::codec::{self, CodecDump, CodecParseError};
use crate::hda::InitVerb;

pub const WATCHED_COEFS: &[(u16, &str)] = &[
    (0x0f, "EAPD coefficient (ALC294_FIXUP_ASUS_HPE)"),
    (0x1b, "bit 10 is the suspected Windows fault"),
    (0x40, "written by ALC294_FIXUP_ASUS_SPK"),
];

const COEF_1B_BIT10: u16 = 0x0400;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("cannot read {path}: {reason}")]
    Read { path: PathBuf, reason: String },
    #[error("{path}: {error}")]
    Parse {
        path: PathBuf,
        error: CodecParseError,
    },
    #[error(
        "no ALC294 ({ALC294_VENDOR_ID:#010x}, subsystem {ALC294_SUBSYSTEM_ID:#010x}) under {searched}; codecs seen: {seen}. To parse a saved codec dump instead: apexctl hw audio snapshot --proc FILE"
    )]
    NotFound { searched: String, seen: String },
    #[error(
        "{path} describes codec {vendor_id:#010x} (subsystem {subsystem_id:#010x}), not the ALC294 that {hwdep} names"
    )]
    WrongCodec {
        path: PathBuf,
        hwdep: String,
        vendor_id: u32,
        subsystem_id: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioSnapshot {
    pub source: String,
    pub card: Option<u32>,
    pub codec: CodecDump,
    pub hwdep: Option<String>,
    pub init_verbs: Option<Vec<String>>,
}

fn hwdep_card_and_address(name: &str) -> Option<(u32, u32)> {
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

pub fn snapshot_file(path: &Path) -> Result<AudioSnapshot, SnapshotError> {
    Ok(AudioSnapshot {
        source: path.display().to_string(),
        card: None,
        codec: read_dump(path)?,
        hwdep: None,
        init_verbs: None,
    })
}

fn hwdeps_seen(root: &SysRoot) -> String {
    let seen: Vec<String> = root
        .list("/sys/class/sound")
        .into_iter()
        .filter(|name| hwdep_card_and_address(name).is_some())
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

/// Opens only the ALC294's codec file, found by its hwdep's cached ids: reading a codec file
/// powers that codec and its controller up, and an HDMI codec would wake the GPU.
pub fn snapshot(root: &SysRoot) -> Result<AudioSnapshot, SnapshotError> {
    let found = find_alc294_hwdep(root)
        .and_then(|hwdep| hwdep_card_and_address(&hwdep).map(|numbers| (hwdep, numbers)));
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
