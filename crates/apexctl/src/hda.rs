use std::fmt;

pub const AC_VERB_SET_COEF_INDEX: u16 = 0x500;
pub const AC_VERB_SET_PROC_COEF: u16 = 0x400;
pub const AC_VERB_GET_COEF_INDEX: u16 = 0xd00;
pub const AC_VERB_GET_PROC_COEF: u16 = 0xc00;

pub const MAX_VERB_FIELD: u32 = 0xf_ffff;
pub const MAX_NID: u16 = 0x7f;
pub const MAX_CODEC_ADDRESS: u8 = 0xf;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HdaError {
    #[error("{what} {value:#x} is out of range (at most {max:#x})")]
    OutOfRange {
        what: &'static str,
        value: u64,
        max: u64,
    },
    #[error("{0:?} is not a number")]
    BadNumber(String),
    #[error("expected \"NID VERB PARAM\", found {0:?}")]
    BadLine(String),
    #[error("the kernel rejects a zero NID or verb: {0:?}")]
    Zero(String),
}

fn out_of_range(what: &'static str, value: impl Into<u64>, max: impl Into<u64>) -> HdaError {
    HdaError::OutOfRange {
        what,
        value: value.into(),
        max: max.into(),
    }
}

/// HDA spec: set/get converter format, amp gain, processing coefficient, coefficient index.
pub const fn is_four_bit_id(nibble: u32) -> bool {
    matches!(nibble, 0x2 | 0x3 | 0x4 | 0x5 | 0xa | 0xb | 0xc | 0xd)
}

/// Only [`Verb::decode`] builds one, which keeps every [`Verb`] canonical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OtherVerb(u32);

impl OtherVerb {
    pub const fn field(self) -> u32 {
        self.0
    }

    pub const fn is_four_bit(self) -> bool {
        is_four_bit_id(self.0 >> 16)
    }

    pub const fn id(self) -> u16 {
        if self.is_four_bit() {
            (self.0 >> 16) as u16
        } else {
            (self.0 >> 8) as u16
        }
    }

    pub const fn payload(self) -> u16 {
        if self.is_four_bit() {
            (self.0 & 0xffff) as u16
        } else {
            (self.0 & 0xff) as u16
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verb {
    SetCoefIndex(u16),
    SetProcCoef(u16),
    GetCoefIndex,
    GetProcCoef,
    Other(OtherVerb),
}

impl Verb {
    pub fn decode(field: u32) -> Result<Self, HdaError> {
        if field > MAX_VERB_FIELD {
            return Err(out_of_range("verb field", field, MAX_VERB_FIELD));
        }
        let payload = (field & 0xffff) as u16;
        Ok(match field >> 16 {
            0x5 => Self::SetCoefIndex(payload),
            0x4 => Self::SetProcCoef(payload),
            0xd if payload == 0 => Self::GetCoefIndex,
            0xc if payload == 0 => Self::GetProcCoef,
            _ => Self::Other(OtherVerb(field)),
        })
    }

    pub const fn field(self) -> u32 {
        match self {
            Self::SetCoefIndex(index) => 0x5_0000 | index as u32,
            Self::SetProcCoef(value) => 0x4_0000 | value as u32,
            Self::GetCoefIndex => 0xd_0000,
            Self::GetProcCoef => 0xc_0000,
            Self::Other(other) => other.field(),
        }
    }

    /// The kernel's `struct hda_verb` spelling: a 4-bit verb `V` is `0xV00`.
    pub const fn kernel_pair(self) -> (u16, u16) {
        let field = self.field();
        if is_four_bit_id(field >> 16) {
            (((field >> 16) << 8) as u16, (field & 0xffff) as u16)
        } else {
            ((field >> 8) as u16, (field & 0xff) as u16)
        }
    }

    pub fn from_kernel_pair(verb: u32, param: u32) -> Result<Self, HdaError> {
        if verb > 0xfff {
            return Err(out_of_range("verb", verb, 0xfff_u32));
        }
        if param > 0xffff {
            return Err(out_of_range("parameter", param, 0xffff_u32));
        }
        // OR-ed like `snd_hdac_make_cmd`: the param's high byte and the verb's low byte share
        // bits 15:8.
        Self::decode((verb << 8) | param)
    }
}

const fn four_bit_name(id: u16) -> &'static str {
    match id {
        0x2 => "SET_STREAM_FORMAT",
        0x3 => "SET_AMP_GAIN_MUTE",
        0x4 => "SET_PROC_COEF",
        0x5 => "SET_COEF_INDEX",
        0xa => "GET_STREAM_FORMAT",
        0xb => "GET_AMP_GAIN_MUTE",
        0xc => "GET_PROC_COEF",
        _ => "GET_COEF_INDEX",
    }
}

/// Names from `include/sound/hda_verbs.h`.
const fn twelve_bit_name(id: u16) -> Option<&'static str> {
    Some(match id {
        0xf00 => "PARAMETERS",
        0xf01 => "GET_CONNECT_SEL",
        0xf02 => "GET_CONNECT_LIST",
        0xf05 => "GET_POWER_STATE",
        0xf07 => "GET_PIN_WIDGET_CONTROL",
        0xf09 => "GET_PIN_SENSE",
        0xf0c => "GET_EAPD_BTLENABLE",
        0xf1c => "GET_CONFIG_DEFAULT",
        0xf20 => "GET_SUBSYSTEM_ID",
        0x701 => "SET_CONNECT_SEL",
        0x705 => "SET_POWER_STATE",
        0x707 => "SET_PIN_WIDGET_CONTROL",
        0x708 => "SET_UNSOLICITED_ENABLE",
        0x70c => "SET_EAPD_BTLENABLE",
        0x71c => "SET_CONFIG_DEFAULT_BYTES_0",
        0x71d => "SET_CONFIG_DEFAULT_BYTES_1",
        0x71e => "SET_CONFIG_DEFAULT_BYTES_2",
        0x71f => "SET_CONFIG_DEFAULT_BYTES_3",
        0x7ff => "SET_CODEC_RESET",
        _ => return None,
    })
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::SetCoefIndex(index) => write!(f, "SET_COEF_INDEX {index:#06x}"),
            Self::SetProcCoef(value) => write!(f, "SET_PROC_COEF {value:#06x}"),
            Self::GetCoefIndex => f.write_str("GET_COEF_INDEX"),
            Self::GetProcCoef => f.write_str("GET_PROC_COEF"),
            Self::Other(other) if other.is_four_bit() => {
                write!(f, "{} {:#06x}", four_bit_name(other.id()), other.payload())
            }
            Self::Other(other) => match twelve_bit_name(other.id()) {
                Some(name) => write!(f, "{name} {:#04x}", other.payload()),
                None => write!(f, "VERB {:#05x} {:#04x}", other.id(), other.payload()),
            },
        }
    }
}

/// Like `sscanf("%i")`, except that the whole token must be the number.
pub fn parse_c_int(token: &str) -> Result<i64, HdaError> {
    let bad = || HdaError::BadNumber(token.to_owned());
    let (negative, digits) = match token.as_bytes().first() {
        Some(b'-') => (true, &token[1..]),
        Some(b'+') => (false, &token[1..]),
        _ => (false, token),
    };
    let (radix, body) = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        (16, hex)
    } else if digits.len() > 1 && digits.starts_with('0') {
        (8, &digits[1..])
    } else {
        (10, digits)
    };
    // from_str_radix accepts a sign of its own; the sign was handled above.
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let magnitude = i64::from_str_radix(body, radix).map_err(|_| bad())?;
    Ok(if negative { -magnitude } else { magnitude })
}

/// The kernel's `struct hda_verb`: a patch file `[verb]` line and an `init_verbs` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub struct InitVerb {
    pub nid: u16,
    pub verb: u16,
    pub param: u16,
}

impl InitVerb {
    pub fn new(nid: u16, verb: Verb) -> Result<Self, HdaError> {
        if nid == 0 || nid > MAX_NID {
            return Err(out_of_range("NID", nid, MAX_NID));
        }
        let (verb, param) = verb.kernel_pair();
        Ok(Self { nid, verb, param })
    }

    /// Unlike the kernel's `sscanf`, trailing text is an error, so a typo cannot hide in a
    /// half-read line.
    pub fn parse(line: &str) -> Result<Self, HdaError> {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let [nid, verb, param] = tokens.as_slice() else {
            return Err(HdaError::BadLine(line.to_owned()));
        };
        let nid = parse_c_int(nid)?;
        let verb = parse_c_int(verb)?;
        let param = parse_c_int(param)?;
        if nid == 0 || verb == 0 {
            return Err(HdaError::Zero(line.to_owned()));
        }
        let nid = u16::try_from(nid)
            .ok()
            .filter(|&n| n <= MAX_NID)
            .ok_or_else(|| out_of_range("NID", nid.unsigned_abs(), MAX_NID))?;
        let verb = u16::try_from(verb)
            .ok()
            .filter(|&v| v <= 0xfff)
            .ok_or_else(|| out_of_range("verb", verb.unsigned_abs(), 0xfff_u32))?;
        let param = u16::try_from(param)
            .map_err(|_| out_of_range("parameter", param.unsigned_abs(), 0xffff_u32))?;
        Ok(Self { nid, verb, param })
    }

    pub fn effective(self) -> Verb {
        // Unreachable fallback: the fields were range-checked on construction.
        Verb::from_kernel_pair(u32::from(self.verb), u32::from(self.param))
            .unwrap_or(Verb::Other(OtherVerb(0)))
    }
}

impl fmt::Display for InitVerb {
    /// The kernel's `init_verbs_show` format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:#04x} {:#05x} {:#06x}",
            self.nid, self.verb, self.param
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// `snd_hdac_make_cmd`.
    fn command(verb: Verb, address: u8, nid: u16) -> Result<u32, HdaError> {
        if address > MAX_CODEC_ADDRESS {
            return Err(out_of_range("codec address", address, MAX_CODEC_ADDRESS));
        }
        if nid > MAX_NID {
            return Err(out_of_range("NID", nid, MAX_NID));
        }
        Ok((u32::from(address) << 28) | (u32::from(nid) << 20) | verb.field())
    }

    /// alsa-tools `hda-verb NID VERB PARAM`: `HDA_VERB` ORs the three into one word, which the
    /// kernel's hwdep `verb_write_ioctl` splits again.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct HdaVerbCommand {
        nid: u32,
        verb: u32,
        param: u32,
    }

    impl HdaVerbCommand {
        fn packed(self) -> Result<u32, HdaError> {
            if self.nid > 0xff {
                return Err(out_of_range("NID", self.nid, 0xff_u32));
            }
            if self.verb > 0xfff {
                return Err(out_of_range("verb", self.verb, 0xfff_u32));
            }
            if self.param > 0xffff {
                return Err(out_of_range("parameter", self.param, 0xffff_u32));
            }
            Ok((self.nid << 24) | (self.verb << 8) | self.param)
        }

        fn decode_packed(word: u32) -> Result<(u16, Verb), HdaError> {
            let nid = word >> 24;
            if nid > u32::from(MAX_NID) {
                return Err(out_of_range("NID", nid, MAX_NID));
            }
            let verb = Verb::from_kernel_pair((word >> 8) & 0xffff, word & 0xff)?;
            Ok((nid as u16, verb))
        }

        fn effective(self) -> Result<(u16, Verb), HdaError> {
            Self::decode_packed(self.packed()?)
        }
    }

    fn legacy(nid: u32, verb: u32, param: u32) -> HdaVerbCommand {
        HdaVerbCommand { nid, verb, param }
    }

    #[test]
    fn ask_ubuntu_workaround_decodes_to_what_the_codec_really_gets() {
        let cases = [
            (
                legacy(0x20, 0x500, 0x1b),
                0x2005_001b,
                Verb::SetCoefIndex(0x1b),
            ),
            (
                legacy(0x20, 0x477, 0x4a4b),
                0x2004_7f4b,
                Verb::SetProcCoef(0x7f4b),
            ),
            (
                legacy(0x20, 0x500, 0xf),
                0x2005_000f,
                Verb::SetCoefIndex(0x0f),
            ),
            (
                legacy(0x20, 0x477, 0x74),
                0x2004_7774,
                Verb::SetProcCoef(0x7774),
            ),
            (
                legacy(0x20, 0x400, 0x4a4b),
                0x2004_4a4b,
                Verb::SetProcCoef(0x4a4b),
            ),
        ];
        for (command, word, verb) in cases {
            assert_eq!(command.packed(), Ok(word), "{command:?}");
            assert_eq!(command.effective(), Ok((0x20, verb)), "{command:?}");
        }
        assert_eq!(
            Verb::SetProcCoef(0x7f4b).to_string(),
            "SET_PROC_COEF 0x7f4b"
        );
        assert_eq!(
            Verb::SetCoefIndex(0x1b).to_string(),
            "SET_COEF_INDEX 0x001b"
        );
    }

    #[test]
    fn command_words_follow_snd_hdac_make_cmd() {
        assert_eq!(command(Verb::SetCoefIndex(0x1b), 0, 0x20), Ok(0x0205_001b));
        assert_eq!(command(Verb::SetProcCoef(0x4e4b), 1, 0x20), Ok(0x1204_4e4b));
        assert_eq!(command(Verb::GetProcCoef, 0, 0x20), Ok(0x020c_0000));
        assert!(command(Verb::GetProcCoef, 0x10, 0x20).is_err());
        assert!(command(Verb::GetProcCoef, 0, 0x80).is_err());
        let pin = Verb::from_kernel_pair(0x707, 0x40).unwrap();
        assert_eq!(command(pin, 0, 0x14), Ok(0x0147_0740));
        assert_eq!(pin.to_string(), "SET_PIN_WIDGET_CONTROL 0x40");
        assert_eq!(pin.kernel_pair(), (0x707, 0x40));
    }

    #[test]
    fn named_verbs_use_the_kernel_constants() {
        assert_eq!(
            Verb::SetCoefIndex(7).kernel_pair(),
            (AC_VERB_SET_COEF_INDEX, 7)
        );
        assert_eq!(
            Verb::SetProcCoef(9).kernel_pair(),
            (AC_VERB_SET_PROC_COEF, 9)
        );
        assert_eq!(
            Verb::GetCoefIndex.kernel_pair(),
            (AC_VERB_GET_COEF_INDEX, 0)
        );
        assert_eq!(Verb::GetProcCoef.kernel_pair(), (AC_VERB_GET_PROC_COEF, 0));
        assert_eq!(
            Verb::decode(0xc_0001).unwrap().to_string(),
            "GET_PROC_COEF 0x0001"
        );
        assert_eq!(
            Verb::decode(0x7_a501).unwrap().to_string(),
            "VERB 0x7a5 0x01"
        );
        assert!(Verb::decode(0x10_0000).is_err());
    }

    #[test]
    fn init_verb_parses_like_sscanf_percent_i_and_prints_like_sysfs() {
        let verb = InitVerb::parse("0x20 0x500 0x1b").unwrap();
        assert_eq!(
            verb,
            InitVerb {
                nid: 0x20,
                verb: 0x500,
                param: 0x1b
            }
        );
        assert_eq!(verb.to_string(), "0x20 0x500 0x001b");
        assert_eq!(verb.effective(), Verb::SetCoefIndex(0x1b));
        assert_eq!(InitVerb::parse("32 1280 033").unwrap(), verb);
        assert_eq!(
            InitVerb::parse("  0x20\t0x400 0x4e4b  ").unwrap().param,
            0x4e4b
        );
        assert!(matches!(
            InitVerb::parse("0x20 0x500"),
            Err(HdaError::BadLine(_))
        ));
        assert!(matches!(
            InitVerb::parse("0x20 0x500 0x1b # comment"),
            Err(HdaError::BadLine(_))
        ));
        assert!(matches!(
            InitVerb::parse("0 0x500 1"),
            Err(HdaError::Zero(_))
        ));
        assert!(matches!(
            InitVerb::parse("0x20 0 1"),
            Err(HdaError::Zero(_))
        ));
        assert!(InitVerb::parse("0x80 0x500 1").is_err());
        assert!(InitVerb::parse("0x20 0x1000 1").is_err());
        assert!(InitVerb::parse("0x20 0x400 0x10000").is_err());
        assert!(InitVerb::parse("0x20 0x400 -1").is_err());
        assert!(InitVerb::parse("0x20 0x400 0xg").is_err());
        assert!(matches!(
            InitVerb::parse("0x20 0x400 09"),
            Err(HdaError::BadNumber(_))
        ));
    }

    #[test]
    fn c_integers() {
        assert_eq!(parse_c_int("0"), Ok(0));
        assert_eq!(parse_c_int("0x1B"), Ok(0x1b));
        assert_eq!(parse_c_int("0X1b"), Ok(0x1b));
        assert_eq!(parse_c_int("010"), Ok(8));
        assert_eq!(parse_c_int("-12"), Ok(-12));
        assert_eq!(parse_c_int("+12"), Ok(12));
        for bad in ["", "0x", "-", "1a", "0x-1", "--1", " 1"] {
            assert!(parse_c_int(bad).is_err(), "{bad:?}");
        }
    }

    proptest! {
        #[test]
        fn decode_is_the_inverse_of_field(field in 0..=MAX_VERB_FIELD) {
            let verb = Verb::decode(field).unwrap();
            prop_assert_eq!(verb.field(), field);
            prop_assert_eq!(Verb::decode(verb.field()).unwrap(), verb);
        }

        #[test]
        fn kernel_pair_round_trips(field in 0..=MAX_VERB_FIELD) {
            let verb = Verb::decode(field).unwrap();
            let (v, p) = verb.kernel_pair();
            prop_assert!(v <= 0xfff);
            prop_assert_eq!(Verb::from_kernel_pair(u32::from(v), u32::from(p)).unwrap(), verb);
        }

        #[test]
        fn hda_verb_and_patch_lines_mean_the_same(
            nid in 1_u32..=0x7f,
            verb in 1_u32..=0xfff,
            param in 0_u32..=0xffff,
        ) {
            let (effective_nid, effective) = legacy(nid, verb, param).effective().unwrap();
            prop_assert_eq!(u32::from(effective_nid), nid);
            prop_assert_eq!(effective, Verb::from_kernel_pair(verb, param).unwrap());
        }

        #[test]
        fn init_verb_display_parse_round_trips(
            nid in 1_u16..=0x7f,
            verb in 1_u16..=0xfff,
            param in 0_u16..=0xffff,
        ) {
            let line = InitVerb { nid, verb, param };
            prop_assert_eq!(InitVerb::parse(&line.to_string()).unwrap(), line);
        }

        #[test]
        fn coef_writes_survive_the_triple(nid in 1_u16..=0x7f, value: u16) {
            for verb in [Verb::SetCoefIndex(value), Verb::SetProcCoef(value)] {
                let line = InitVerb::new(nid, verb).unwrap();
                prop_assert_eq!(line.effective(), verb);
            }
        }

        #[test]
        fn parse_never_panics(line in "\\PC{0,40}") {
            let _ = InitVerb::parse(&line);
        }
    }
}
