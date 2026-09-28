//! Parser for `/proc/asound/card*/codec#*`, the kernel's text dump of an HDA codec
//! (`sound/hda/common/proc.c`).
//!
//! Only what Apex needs is kept: the codec header (`Codec`, `Address`, `Vendor Id`,
//! `Subsystem Id`, `Revision Id`) and, per node, the widget type and capabilities, the pin
//! capabilities, `Pin Default`, `Pin-ctls`, `EAPD`, the connection list with the selected
//! entry, the processing capabilities and the `Coeff 0x..: 0x....` lines that appear when
//! `snd_hda_codec.dump_coef` is enabled. Every other line is ignored, so newer kernels
//! that print more do not break the parser. A line that *is* one of the parsed kinds but
//! does not have the kernel's format is an error with its line number.
//!
//! A `Coeff` value is the kernel's 32-bit `unsigned int` printed with `%04x`: a failed read
//! of that slot prints `0xffffffff` (`snd_hda_codec_read` returns -1). Such values are kept
//! in [`Node::unreadable_coefs`] instead of failing the dump, which is exactly what is
//! needed when the codec misbehaves.
//!
//! Reading the file with `dump_coef` enabled makes the kernel select every COEF slot in
//! turn (and restore the index afterwards); `apexctl` only reads the text.

use std::collections::BTreeMap;

use serde::{Serialize, Serializer};

/// The parsed dump of one codec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodecDump {
    /// `Codec:` line, e.g. `Realtek ALC294`.
    pub codec: String,
    /// `Address:` line: the codec address on the HDA link.
    pub address: Option<u8>,
    /// `Vendor Id:`, e.g. `0x10ec0294`.
    pub vendor_id: u32,
    /// `Subsystem Id:`, e.g. `0x10431ab2`.
    pub subsystem_id: u32,
    /// `Revision Id:`, e.g. `0x100004`.
    pub revision_id: u32,
    /// The widget nodes, in file order (ascending NID).
    pub nodes: Vec<Node>,
}

impl CodecDump {
    /// The node with `nid`.
    pub fn node(&self, nid: u16) -> Option<&Node> {
        self.nodes.iter().find(|node| node.nid == nid)
    }

    /// The pin complexes.
    pub fn pins(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(|node| node.is_pin())
    }
}

/// One `Node 0x.. [Type] wcaps 0x..:` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Node {
    /// Widget NID.
    pub nid: u16,
    /// Widget type as printed, e.g. `Pin Complex` or `Vendor Defined Widget`.
    pub kind: String,
    /// Widget capabilities.
    pub wcaps: u32,
    /// `Pincap 0x........`: pin capabilities.
    pub pin_caps: Option<u32>,
    /// `Pin Default 0x........: …`: the default configuration.
    pub pin_default: Option<PinDefault>,
    /// `Pin-ctls: 0x..: …`: the pin widget control.
    pub pin_ctls: Option<Flags>,
    /// `EAPD 0x.: …`: EAPD/BTL enable.
    pub eapd: Option<Flags>,
    /// `Connection: N` list, in order.
    pub connections: Vec<u16>,
    /// The entry marked `*` (the selected input), if any.
    pub selected_connection: Option<u16>,
    /// `Processing caps: … ncoeff=N`: number of COEF slots.
    pub ncoeff: Option<u32>,
    /// `Coeff 0x..: 0x....` lines, by COEF index (present only with `dump_coef`).
    ///
    /// A `BTreeMap` keeps the dump's index order and gives lookups by index for the
    /// summary; JSON gets a list of `{index, value}` objects.
    #[serde(serialize_with = "coefs_as_list")]
    pub coefs: BTreeMap<u16, u16>,
    /// `Coeff` lines whose value does not fit a 16-bit COEF, with the raw value the kernel
    /// printed. `0xffffffff` is the kernel's -1: reading that slot failed. JSON lists them
    /// as `{index, raw}` objects, and leaves the key out when there are none.
    #[serde(
        serialize_with = "raw_coefs_as_list",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub unreadable_coefs: BTreeMap<u16, u32>,
}

impl Node {
    /// `true` for a pin complex.
    pub fn is_pin(&self) -> bool {
        self.kind == "Pin Complex"
    }
}

/// A `Pin Default` value and the kernel's decoding of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PinDefault {
    /// The 32-bit default configuration.
    pub value: u32,
    /// The kernel's text, e.g. `[Fixed] Speaker at Int N/A`.
    pub description: String,
}

/// A register value followed by the kernel's flag words (`0x40: OUT`, `0x2: EAPD`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Flags {
    /// The value.
    pub value: u32,
    /// The flag words after the colon, e.g. `["IN", "VREF_HIZ"]`.
    pub flags: Vec<String>,
}

#[derive(Serialize)]
struct CoefEntry {
    index: u16,
    value: u16,
}

fn coefs_as_list<S: Serializer>(coefs: &BTreeMap<u16, u16>, s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(
        coefs
            .iter()
            .map(|(&index, &value)| CoefEntry { index, value }),
    )
}

#[derive(Serialize)]
struct RawCoefEntry {
    index: u16,
    raw: u32,
}

fn raw_coefs_as_list<S: Serializer>(coefs: &BTreeMap<u16, u32>, s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(
        coefs
            .iter()
            .map(|(&index, &raw)| RawCoefEntry { index, raw }),
    )
}

/// Why a dump could not be parsed. `line` numbers start at 1.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecParseError {
    /// A line of a parsed kind does not have the kernel's format.
    #[error("line {line}: malformed {what}: {text:?}")]
    Malformed {
        /// Line number.
        line: usize,
        /// What the line was taken to be.
        what: &'static str,
        /// The line.
        text: String,
    },
    /// A required header line is missing.
    #[error("not a codec dump: no {0:?} line")]
    Missing(&'static str),
    /// A connection list does not have the number of entries its header announced.
    #[error("line {line}: connection list has {found} entries, the header says {expected}")]
    ConnectionCount {
        /// Line number of the list.
        line: usize,
        /// Entries announced by `Connection: N`.
        expected: usize,
        /// Entries found.
        found: usize,
    },
}

fn hex(text: &str) -> Option<u32> {
    let digits = text.strip_prefix("0x")?;
    if digits.is_empty() {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

fn hex16(text: &str) -> Option<u16> {
    hex(text).and_then(|n| u16::try_from(n).ok())
}

/// What the next indented line is expected to be.
#[derive(Clone, Copy)]
enum Pending {
    Nothing,
    /// The list after `Connection: N`.
    Connections(usize),
    /// The list after `In-driver Connection: N`, which is skipped.
    DriverConnections,
}

/// Header fields collected while parsing.
#[derive(Default)]
struct Header {
    codec: Option<String>,
    address: Option<u8>,
    vendor_id: Option<u32>,
    subsystem_id: Option<u32>,
    revision_id: Option<u32>,
}

/// Parses the text of `/proc/asound/cardN/codec#M`.
pub fn parse(text: &str) -> Result<CodecDump, CodecParseError> {
    let mut header = Header::default();
    let mut nodes: Vec<Node> = Vec::new();
    let mut in_node = false;
    let mut pending = Pending::Nothing;

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let malformed = |what| CodecParseError::Malformed {
            line,
            what,
            text: raw.to_owned(),
        };
        if !raw.starts_with(' ') {
            pending = Pending::Nothing;
            in_node = false;
            parse_top_level(raw, &mut header, &mut nodes, &mut in_node).map_err(malformed)?;
            continue;
        }
        let content = raw.trim();
        match std::mem::replace(&mut pending, Pending::Nothing) {
            Pending::Connections(expected) => {
                if !content.starts_with("0x") {
                    return Err(CodecParseError::ConnectionCount {
                        line,
                        expected,
                        found: 0,
                    });
                }
                let node = nodes.last_mut().filter(|_| in_node);
                let node = node.ok_or_else(|| malformed("connection list"))?;
                for token in content.split_whitespace() {
                    let (nid, selected) = match token.strip_suffix('*') {
                        Some(nid) => (nid, true),
                        None => (token, false),
                    };
                    let nid = hex16(nid).ok_or_else(|| malformed("connection list"))?;
                    if selected {
                        node.selected_connection = Some(nid);
                    }
                    node.connections.push(nid);
                }
                if node.connections.len() != expected {
                    return Err(CodecParseError::ConnectionCount {
                        line,
                        expected,
                        found: node.connections.len(),
                    });
                }
                continue;
            }
            Pending::DriverConnections if content.starts_with("0x") => continue,
            Pending::DriverConnections | Pending::Nothing => {}
        }
        if !in_node {
            continue;
        }
        let Some(node) = nodes.last_mut() else {
            continue;
        };
        pending = parse_node_line(content, node).map_err(malformed)?;
    }

    Ok(CodecDump {
        codec: header.codec.ok_or(CodecParseError::Missing("Codec"))?,
        address: header.address,
        vendor_id: header
            .vendor_id
            .ok_or(CodecParseError::Missing("Vendor Id"))?,
        subsystem_id: header
            .subsystem_id
            .ok_or(CodecParseError::Missing("Subsystem Id"))?,
        revision_id: header
            .revision_id
            .ok_or(CodecParseError::Missing("Revision Id"))?,
        nodes,
    })
}

/// A line that starts in column 0. Returns the kind of line on a format error.
fn parse_top_level(
    raw: &str,
    header: &mut Header,
    nodes: &mut Vec<Node>,
    in_node: &mut bool,
) -> Result<(), &'static str> {
    if let Some(rest) = raw.strip_prefix("Codec: ") {
        header.codec = Some(rest.trim().to_owned());
    } else if let Some(rest) = raw.strip_prefix("Address: ") {
        header.address = Some(rest.trim().parse().map_err(|_| "Address")?);
    } else if let Some(rest) = raw.strip_prefix("Vendor Id: ") {
        header.vendor_id = Some(hex(rest.trim()).ok_or("Vendor Id")?);
    } else if let Some(rest) = raw.strip_prefix("Subsystem Id: ") {
        header.subsystem_id = Some(hex(rest.trim()).ok_or("Subsystem Id")?);
    } else if let Some(rest) = raw.strip_prefix("Revision Id: ") {
        header.revision_id = Some(hex(rest.trim()).ok_or("Revision Id")?);
    } else if let Some(rest) = raw.strip_prefix("Node ") {
        // Node 0x14 [Pin Complex] wcaps 0x40058d: Stereo Amp-Out
        let (nid, rest) = rest.split_once(" [").ok_or("Node")?;
        let (kind, rest) = rest.split_once("] wcaps ").ok_or("Node")?;
        let wcaps = rest.split(':').next().ok_or("Node")?;
        nodes.push(Node {
            nid: hex16(nid).ok_or("Node")?,
            kind: kind.to_owned(),
            wcaps: hex(wcaps).ok_or("Node")?,
            pin_caps: None,
            pin_default: None,
            pin_ctls: None,
            eapd: None,
            connections: Vec::new(),
            selected_connection: None,
            ncoeff: None,
            coefs: BTreeMap::new(),
            unreadable_coefs: BTreeMap::new(),
        });
        *in_node = true;
    }
    Ok(())
}

/// `0x40: OUT HP` → value and flag words.
fn flags(rest: &str) -> Option<Flags> {
    let (value, words) = rest.split_once(':')?;
    Some(Flags {
        value: hex(value.trim())?,
        flags: words.split_whitespace().map(str::to_owned).collect(),
    })
}

/// An indented line inside a node block (trimmed). Returns what the next line must be.
fn parse_node_line(content: &str, node: &mut Node) -> Result<Pending, &'static str> {
    if let Some(rest) = content.strip_prefix("Pincap ") {
        let value = rest.split(':').next().ok_or("Pincap")?;
        node.pin_caps = Some(hex(value.trim()).ok_or("Pincap")?);
    } else if let Some(rest) = content.strip_prefix("Pin Default ") {
        let (value, description) = rest.split_once(':').ok_or("Pin Default")?;
        node.pin_default = Some(PinDefault {
            value: hex(value.trim()).ok_or("Pin Default")?,
            description: description.trim().to_owned(),
        });
    } else if let Some(rest) = content.strip_prefix("Pin-ctls: ") {
        node.pin_ctls = Some(flags(rest).ok_or("Pin-ctls")?);
    } else if let Some(rest) = content.strip_prefix("EAPD ") {
        node.eapd = Some(flags(rest).ok_or("EAPD")?);
    } else if let Some(rest) = content.strip_prefix("Connection: ") {
        let count: usize = rest.trim().parse().map_err(|_| "Connection")?;
        node.connections.clear();
        node.selected_connection = None;
        if count > 0 {
            return Ok(Pending::Connections(count));
        }
    } else if let Some(rest) = content.strip_prefix("In-driver Connection: ") {
        let count: usize = rest.trim().parse().map_err(|_| "In-driver Connection")?;
        if count > 0 {
            return Ok(Pending::DriverConnections);
        }
    } else if let Some(rest) = content.strip_prefix("Processing caps: ") {
        let ncoeff = rest
            .split(',')
            .find_map(|part| part.trim().strip_prefix("ncoeff="))
            .ok_or("Processing caps")?;
        node.ncoeff = Some(ncoeff.trim().parse().map_err(|_| "Processing caps")?);
    } else if let Some(rest) = content.strip_prefix("Coeff ") {
        let (index, value) = rest.split_once(':').ok_or("Coeff")?;
        let index = hex16(index.trim()).ok_or("Coeff")?;
        let raw = hex(value.trim()).ok_or("Coeff")?;
        match u16::try_from(raw) {
            Ok(value) => {
                node.coefs.insert(index, value);
            }
            Err(_) => {
                node.unreadable_coefs.insert(index, raw);
            }
        }
    }
    Ok(Pending::Nothing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fmt::Write as _;

    /// Prints a dump in the kernel's formats (the subset the parser reads).
    fn to_proc_text(dump: &CodecDump) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Codec: {}", dump.codec);
        if let Some(address) = dump.address {
            let _ = writeln!(out, "Address: {address}");
        }
        let _ = writeln!(out, "AFG Function Id: 0x1 (unsol 1)");
        let _ = writeln!(out, "Vendor Id: 0x{:08x}", dump.vendor_id);
        let _ = writeln!(out, "Subsystem Id: 0x{:08x}", dump.subsystem_id);
        let _ = writeln!(out, "Revision Id: 0x{:x}", dump.revision_id);
        let _ = writeln!(
            out,
            "State of AFG node 0x01:\n  Power: setting=D0, actual=D0"
        );
        for node in &dump.nodes {
            let _ = writeln!(
                out,
                "Node 0x{:02x} [{}] wcaps 0x{:x}: Stereo",
                node.nid, node.kind, node.wcaps
            );
            if let Some(caps) = node.pin_caps {
                let _ = writeln!(out, "  Pincap 0x{caps:08x}: IN OUT");
            }
            if let Some(eapd) = &node.eapd {
                let _ = writeln!(out, "  EAPD 0x{:x}: {}", eapd.value, eapd.flags.join(" "));
            }
            if let Some(pin) = &node.pin_default {
                let _ = writeln!(
                    out,
                    "  Pin Default 0x{:08x}: {}",
                    pin.value, pin.description
                );
                let _ = writeln!(out, "    Conn = 1/8, Color = Black");
            }
            if let Some(ctls) = &node.pin_ctls {
                let _ = writeln!(
                    out,
                    "  Pin-ctls: 0x{:02x}: {}",
                    ctls.value,
                    ctls.flags.join(" ")
                );
            }
            if let Some(ncoeff) = node.ncoeff {
                let _ = writeln!(out, "  Processing caps: benign=0, ncoeff={ncoeff}");
            }
            let mut lines: BTreeMap<u16, u32> = node
                .coefs
                .iter()
                .map(|(&index, &value)| (index, u32::from(value)))
                .collect();
            lines.extend(&node.unreadable_coefs);
            for (index, value) in lines {
                let _ = writeln!(out, "    Coeff 0x{index:02x}: 0x{value:04x}");
            }
            if !node.connections.is_empty() {
                let _ = writeln!(out, "  Connection: {}", node.connections.len());
                out.push_str("    ");
                for nid in &node.connections {
                    let _ = write!(out, " 0x{nid:02x}");
                    if node.selected_connection == Some(*nid) {
                        out.push('*');
                    }
                }
                out.push('\n');
                let _ = writeln!(out, "  In-driver Connection: 1\n     0x7f");
            }
        }
        out
    }

    const COEF_DUMP: &str = "\
Codec: Realtek ALC294
Address: 0
Vendor Id: 0x10ec0294
Subsystem Id: 0x10431ab2
Revision Id: 0x100004
Node 0x20 [Vendor Defined Widget] wcaps 0xf00040: Mono
  Processing caps: benign=0, ncoeff=130
    Coeff 0x00: 0x0002
    Coeff 0x0f: 0x7774
    Coeff 0x1b: 0x4a4b
Node 0x21 [Pin Complex] wcaps 0x40058d: Stereo Amp-Out
  Pin-ctls: 0x00:
  Connection: 0
";

    #[test]
    fn reads_coefficients_when_dump_coef_is_enabled() {
        // Synthetic values: the real dump on this laptop has dump_coef disabled.
        let dump = parse(COEF_DUMP).unwrap();
        let widget = dump.node(0x20).unwrap();
        assert_eq!(widget.ncoeff, Some(130));
        assert_eq!(widget.coefs.get(&0x1b), Some(&0x4a4b));
        assert_eq!(widget.coefs.len(), 3);
        let pin = dump.node(0x21).unwrap();
        assert_eq!(pin.pin_ctls.as_ref().map(|f| f.value), Some(0));
        assert!(pin.pin_ctls.as_ref().unwrap().flags.is_empty());
        assert!(pin.connections.is_empty());
        let json = serde_json::to_value(widget).unwrap();
        assert_eq!(
            json["coefs"][2],
            serde_json::json!({"index": 27, "value": 19019})
        );
    }

    #[test]
    fn a_failed_coef_read_does_not_spoil_the_dump() {
        // dump_coef=1 with one failed read: the kernel prints its -1 as 0xffffffff.
        let dump = COEF_DUMP.replace("Coeff 0x0f: 0x7774", "Coeff 0x0f: 0xffffffff");
        let dump = parse(&dump).unwrap();
        let widget = dump.node(0x20).unwrap();
        assert_eq!(widget.coefs.get(&0x0f), None);
        assert_eq!(widget.coefs.get(&0x1b), Some(&0x4a4b));
        assert_eq!(widget.unreadable_coefs.get(&0x0f), Some(&0xffff_ffff));
        let json = serde_json::to_value(widget).unwrap();
        assert_eq!(
            json["unreadable_coefs"],
            serde_json::json!([{"index": 15, "raw": 0xffff_ffff_u32}])
        );
        // No failed read: no key.
        let clean = serde_json::to_value(parse(COEF_DUMP).unwrap().node(0x20).unwrap()).unwrap();
        assert!(clean.get("unreadable_coefs").is_none());
        // Wider than the kernel's unsigned int is still malformed.
        let wide = COEF_DUMP.replace("Coeff 0x0f: 0x7774", "Coeff 0x0f: 0x1ffffffff");
        assert!(matches!(
            parse(&wide),
            Err(CodecParseError::Malformed { what: "Coeff", .. })
        ));
    }

    #[test]
    fn errors_name_the_line() {
        let bad = COEF_DUMP.replace("Coeff 0x1b: 0x4a4b", "Coeff 0x1b: 4a4b");
        assert_eq!(
            parse(&bad),
            Err(CodecParseError::Malformed {
                line: 10,
                what: "Coeff",
                text: "    Coeff 0x1b: 4a4b".to_owned()
            })
        );
        assert_eq!(
            parse("Codec: X\nVendor Id: 0x1\n"),
            Err(CodecParseError::Missing("Subsystem Id"))
        );
        let short_list = "Codec: X\nVendor Id: 0x1\nSubsystem Id: 0x2\nRevision Id: 0x3\n\
            Node 0x14 [Pin Complex] wcaps 0x1: Mono\n  Connection: 2\n     0x02\n";
        assert_eq!(
            parse(short_list),
            Err(CodecParseError::ConnectionCount {
                line: 7,
                expected: 2,
                found: 1
            })
        );
    }

    fn node_strategy() -> impl Strategy<Value = Node> {
        (
            1_u16..0x80,
            prop_oneof![
                Just("Pin Complex"),
                Just("Audio Output"),
                Just("Vendor Defined Widget")
            ],
            any::<u32>(),
            proptest::option::of(any::<u32>()),
            proptest::option::of((any::<u32>(), "\\[[A-Za-z]{1,8}\\] [A-Za-z]{1,8} at Int")),
            proptest::option::of((0_u32..0x100, proptest::collection::vec("[A-Z_]{1,8}", 0..3))),
            proptest::option::of((0_u32..0x10, proptest::collection::vec("[A-Z]{1,5}", 0..2))),
            proptest::collection::btree_set(1_u16..0x80, 0..5),
            proptest::option::of(0_u32..256),
            proptest::collection::btree_map(
                any::<u16>(),
                prop_oneof![any::<u16>().prop_map(u32::from), 0x1_0000_u32..=u32::MAX],
                0..6,
            ),
        )
            .prop_map(
                |(
                    nid,
                    kind,
                    wcaps,
                    pin_caps,
                    pin_default,
                    pin_ctls,
                    eapd,
                    conns,
                    ncoeff,
                    coef_lines,
                )| {
                    let connections: Vec<u16> = conns.into_iter().collect();
                    let mut coefs = BTreeMap::new();
                    let mut unreadable_coefs = BTreeMap::new();
                    for (index, raw) in coef_lines {
                        match u16::try_from(raw) {
                            Ok(value) => {
                                coefs.insert(index, value);
                            }
                            Err(_) => {
                                unreadable_coefs.insert(index, raw);
                            }
                        }
                    }
                    Node {
                        nid,
                        kind: kind.to_owned(),
                        wcaps,
                        pin_caps,
                        pin_default: pin_default
                            .map(|(value, description)| PinDefault { value, description }),
                        pin_ctls: pin_ctls.map(|(value, flags)| Flags { value, flags }),
                        eapd: eapd.map(|(value, flags)| Flags { value, flags }),
                        selected_connection: connections.first().copied(),
                        connections,
                        ncoeff,
                        coefs,
                        unreadable_coefs,
                    }
                },
            )
    }

    proptest! {
        #[test]
        fn kernel_formatted_dumps_round_trip(
            codec in "[A-Za-z][A-Za-z0-9 ]{0,20}[A-Za-z0-9]",
            address in proptest::option::of(0_u8..16),
            vendor_id: u32,
            subsystem_id: u32,
            revision_id: u32,
            nodes in proptest::collection::vec(node_strategy(), 0..6),
        ) {
            let dump = CodecDump { codec, address, vendor_id, subsystem_id, revision_id, nodes };
            prop_assert_eq!(parse(&to_proc_text(&dump)).unwrap(), dump);
        }

        #[test]
        fn arbitrary_text_never_panics(text in "(\\PC{0,30}\n){0,8}") {
            let _ = parse(&text);
        }
    }
}
