//! HDA patch firmware files as Apex writes them: one `[codec]` line and `[verb]` lines.
//!
//! The kernel reads these in `snd_hda_load_patch` (`sound/hda/common/sysfs.c`): each line
//! has its leading and trailing whitespace stripped, empty lines and lines starting with
//! `#` are skipped, a line starting with `[` switches the section, and data lines go to the
//! section's parser. Three traps make this parser stricter than the kernel:
//!
//! - Lines are read into a 128-byte buffer. A line longer than [`MAX_LINE`] characters
//!   is not truncated but ends parsing: `get_line_from_fw` stops advancing and consumes
//!   the rest of the file. Such a file loses every later verb without a message.
//! - A NUL byte anywhere hangs the kernel: `get_line_from_fw` stops at the NUL without
//!   consuming it and still reports a line, so `snd_hda_load_patch` loops forever in the
//!   firmware callback of `snd-hda-intel` and the sound cards never finish probing. Every
//!   control character except tab is therefore rejected ([`PatchError::ControlCharacter`]);
//!   Apex never writes one.
//! - Bad data lines are skipped silently. Here they are errors, because the only symptom
//!   on the laptop would be a missing verb in `init_verbs`.
//!
//! Only `[codec]` and `[verb]` (alias `[init_verbs]`) are accepted, as ADR 0004 requires:
//! a `[model]` or `[pincfg]` section would override a future upstream quirk.

use std::fmt;

use crate::hda::{HdaError, InitVerb, parse_c_int};

/// Longest line the kernel's patch loader handles (`char buf[128]`, one byte for NUL).
pub const MAX_LINE: usize = 127;

/// The `[codec]` line: which codec the following sections apply to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub struct CodecLine {
    /// Codec vendor ID, e.g. `0x10ec0294`.
    pub vendor_id: u32,
    /// Codec subsystem ID, e.g. `0x10431ab2`.
    pub subsystem_id: u32,
    /// Codec address on the HDA link.
    pub address: u32,
}

impl fmt::Display for CodecLine {
    /// `0x10ec0294 0x10431ab2 0`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:#010x} {:#010x} {}",
            self.vendor_id, self.subsystem_id, self.address
        )
    }
}

/// A parsed Apex patch file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PatchFile {
    /// The codec the verbs are for.
    pub codec: CodecLine,
    /// The `[verb]` lines, in file order (the order the kernel replays them).
    pub verbs: Vec<InitVerb>,
}

/// Why a patch file was rejected. `line` numbers start at 1.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    /// A control character other than tab (line feeds end lines). A NUL makes the kernel's
    /// loader loop forever; the others are never part of an Apex file.
    #[error(
        "line {line}: control character {byte:#04x}; the kernel's patch loader hangs on NUL \
         and Apex files never contain control characters"
    )]
    ControlCharacter {
        /// Line number.
        line: usize,
        /// The byte.
        byte: u8,
    },
    /// The line is longer than [`MAX_LINE`]; the kernel would stop reading there.
    #[error(
        "line {line}: longer than {MAX_LINE} characters; the kernel stops reading the file there"
    )]
    LineTooLong {
        /// Line number.
        line: usize,
    },
    /// A section other than `[codec]` and `[verb]`.
    #[error(
        "line {line}: section {name:?} is not allowed (ADR 0004 files have only [codec] and [verb])"
    )]
    UnsupportedSection {
        /// Line number.
        line: usize,
        /// The section header.
        name: String,
    },
    /// A second `[codec]` line.
    #[error("line {line}: a second codec line; Apex files target exactly one codec")]
    SecondCodec {
        /// Line number.
        line: usize,
    },
    /// The `[codec]` line is not three numbers that fit the kernel's `int`s.
    #[error("line {line}: expected \"VENDOR SUBSYSTEM ADDRESS\", found {text:?}")]
    BadCodecLine {
        /// Line number.
        line: usize,
        /// The line.
        text: String,
    },
    /// Data before any section, or a `[verb]` before the `[codec]` line.
    #[error("line {line}: data outside a section, or [verb] data before the [codec] line")]
    DataOutsideSection {
        /// Line number.
        line: usize,
    },
    /// A `[verb]` line the kernel would reject or misread.
    #[error("line {line}: {error}")]
    BadVerb {
        /// Line number.
        line: usize,
        /// What is wrong with it. Part of the message, so it is not also the `source()`.
        error: HdaError,
    },
    /// No `[codec]` line at all.
    #[error("no [codec] line")]
    NoCodec,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Codec,
    Verb,
}

fn parse_codec_line(line: usize, text: &str) -> Result<CodecLine, PatchError> {
    let bad = || PatchError::BadCodecLine {
        line,
        text: text.to_owned(),
    };
    let numbers: Vec<i64> = text
        .split_whitespace()
        .map(parse_c_int)
        .collect::<Result<_, _>>()
        .map_err(|_| bad())?;
    let [vendor_id, subsystem_id, address] = numbers.as_slice() else {
        return Err(bad());
    };
    // The kernel scans into `int`; anything above i32::MAX would not compare equal.
    let fit = |n: i64| {
        i32::try_from(n)
            .ok()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(bad)
    };
    Ok(CodecLine {
        vendor_id: fit(*vendor_id)?,
        subsystem_id: fit(*subsystem_id)?,
        address: fit(*address)?,
    })
}

/// A control character other than tab (DEL included): NUL hangs the kernel's loader, and
/// no Apex file has any of them.
const fn is_forbidden_control(byte: u8) -> bool {
    (byte < 0x20 && byte != b'\t') || byte == 0x7f
}

/// Parses an Apex patch file.
pub fn parse(text: &str) -> Result<PatchFile, PatchError> {
    // Before anything else: `lines()` would hide a CR before a line feed, and the kernel
    // stops at a NUL wherever it is, comments included.
    for (index, raw) in text.split('\n').enumerate() {
        if let Some(byte) = raw.bytes().find(|&b| is_forbidden_control(b)) {
            return Err(PatchError::ControlCharacter {
                line: index + 1,
                byte,
            });
        }
    }
    let mut section = Section::None;
    let mut codec = None;
    let mut verbs = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        // The kernel counts the bytes of the raw line (after leading whitespace).
        if raw.trim_start().len() > MAX_LINE {
            return Err(PatchError::LineTooLong { line });
        }
        let content = raw.trim();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        if content.starts_with('[') {
            section = match content.to_ascii_lowercase().as_str() {
                "[codec]" if codec.is_some() => return Err(PatchError::SecondCodec { line }),
                "[codec]" => Section::Codec,
                "[verb]" | "[init_verbs]" if codec.is_some() => Section::Verb,
                "[verb]" | "[init_verbs]" => return Err(PatchError::DataOutsideSection { line }),
                _ => {
                    return Err(PatchError::UnsupportedSection {
                        line,
                        name: content.to_owned(),
                    });
                }
            };
            continue;
        }
        match section {
            Section::None => return Err(PatchError::DataOutsideSection { line }),
            Section::Codec if codec.is_some() => return Err(PatchError::SecondCodec { line }),
            Section::Codec => codec = Some(parse_codec_line(line, content)?),
            Section::Verb => verbs.push(
                InitVerb::parse(content).map_err(|error| PatchError::BadVerb { line, error })?,
            ),
        }
    }
    Ok(PatchFile {
        codec: codec.ok_or(PatchError::NoCodec)?,
        verbs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# comment
[codec]
0x10ec0294 0x10431ab2 0

[verb]
# COEF 0x1b
0x20 0x500 0x001b
0x20 0x400 0x4e4b
";

    #[test]
    fn parses_the_apex_shape() {
        let patch = parse(SAMPLE).unwrap();
        assert_eq!(
            patch.codec,
            CodecLine {
                vendor_id: 0x10ec_0294,
                subsystem_id: 0x1043_1ab2,
                address: 0
            }
        );
        assert_eq!(patch.codec.to_string(), "0x10ec0294 0x10431ab2 0");
        let lines: Vec<String> = patch.verbs.iter().map(ToString::to_string).collect();
        assert_eq!(lines, ["0x20 0x500 0x001b", "0x20 0x400 0x4e4b"]);
    }

    #[test]
    fn rejects_what_the_kernel_would_skip_or_misread() {
        let long = format!(
            "[codec]\n0x10ec0294 0x10431ab2 0\n#{}\n",
            "x".repeat(MAX_LINE)
        );
        assert_eq!(parse(&long), Err(PatchError::LineTooLong { line: 3 }));
        let exactly = format!("#{}\n[codec]\n1 2 0\n", "x".repeat(MAX_LINE - 1));
        assert!(parse(&exactly).is_ok());

        assert!(matches!(
            parse("[codec]\n1 2 0\n[model]\nheadset-mic\n"),
            Err(PatchError::UnsupportedSection { line: 3, .. })
        ));
        assert!(matches!(
            parse("[verb]\n0x20 0x500 1\n"),
            Err(PatchError::DataOutsideSection { line: 1 })
        ));
        assert!(matches!(
            parse("0x20 0x500 1\n"),
            Err(PatchError::DataOutsideSection { line: 1 })
        ));
        assert!(matches!(
            parse("[codec]\n1 2 0\n3 4 0\n"),
            Err(PatchError::SecondCodec { line: 3 })
        ));
        assert!(matches!(
            parse("[codec]\n1 2\n"),
            Err(PatchError::BadCodecLine { line: 2, .. })
        ));
        assert!(matches!(
            parse("[codec]\n0x80000000 2 0\n"),
            Err(PatchError::BadCodecLine { .. })
        ));
        assert!(matches!(
            parse("[codec]\n1 2 0\n[verb]\n0x20 0x477\n"),
            Err(PatchError::BadVerb { line: 4, .. })
        ));
        assert_eq!(parse("# nothing\n"), Err(PatchError::NoCodec));
    }

    #[test]
    fn control_characters_are_rejected_because_nul_hangs_the_kernel() {
        // A NUL in a comment: the kernel's get_line_from_fw would loop forever on it.
        let nul = SAMPLE.replace("# comment", "# M7400QC\0");
        assert_eq!(
            parse(&nul),
            Err(PatchError::ControlCharacter { line: 1, byte: 0 })
        );
        // Also after the last line, where no section parser would ever look.
        assert_eq!(
            parse(&format!("{SAMPLE}\0")),
            Err(PatchError::ControlCharacter { line: 9, byte: 0 })
        );
        let crlf = SAMPLE.replace('\n', "\r\n");
        assert_eq!(
            parse(&crlf),
            Err(PatchError::ControlCharacter {
                line: 1,
                byte: b'\r'
            })
        );
        for byte in [0x01_u8, 0x0b, 0x0c, 0x1b, 0x7f] {
            let text = SAMPLE.replace("# COEF 0x1b", &format!("# COEF{}", char::from(byte)));
            assert_eq!(
                parse(&text),
                Err(PatchError::ControlCharacter { line: 6, byte }),
                "{byte:#04x}"
            );
        }
        // Tabs are ordinary whitespace for the kernel and for this parser.
        let tabs = SAMPLE.replace("0x20 0x500 0x001b", "0x20\t0x500\t0x001b");
        assert_eq!(parse(&tabs).unwrap().verbs.len(), 2);
        let message = parse(&nul).unwrap_err().to_string();
        assert!(
            message.starts_with("line 1: control character 0x00"),
            "{message}"
        );
    }

    #[test]
    fn sections_are_case_insensitive_and_init_verbs_is_an_alias() {
        let patch = parse("[CODEC]\n1 2 0\n[init_verbs]\n0x20 0x500 1\n").unwrap();
        assert_eq!(patch.verbs.len(), 1);
    }
}
