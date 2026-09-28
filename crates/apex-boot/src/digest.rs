//! The manifest digest of a deployment, as stored in `apex_target` and reported over D-Bus.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

const PREFIX: &str = "sha256:";
const HEX_LEN: usize = 64;

/// Only the canonical lowercase spelling is accepted, so two digests are equal exactly when
/// their strings are, which the decision table and grubenv rely on.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DigestError {
    #[error("digest must start with \"sha256:\"")]
    MissingPrefix,
    #[error("digest must have 64 hex digits after \"sha256:\", found {found}")]
    WrongLength { found: usize },
    #[error("digest has a character that is not lowercase hex at byte {position}")]
    NotLowercaseHex { position: usize },
}

impl Digest {
    pub fn parse(s: &str) -> Result<Self, DigestError> {
        let hex = s.strip_prefix(PREFIX).ok_or(DigestError::MissingPrefix)?;
        if hex.len() != HEX_LEN {
            return Err(DigestError::WrongLength { found: hex.len() });
        }
        if let Some(offset) = hex
            .bytes()
            .position(|b| !matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(DigestError::NotLowercaseHex {
                position: PREFIX.len() + offset,
            });
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn hex(&self) -> &str {
        &self.0[PREFIX.len()..]
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.0)
    }
}

impl FromStr for Digest {
    type Err = DigestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Digest {
    type Error = DigestError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Digest> for String {
    fn from(digest: Digest) -> Self {
        digest.0
    }
}

impl AsRef<str> for Digest {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const GOOD: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn accepts_canonical_digest() {
        let digest = Digest::parse(GOOD).unwrap();
        assert_eq!(digest.as_str(), GOOD);
        assert_eq!(digest.hex(), &GOOD[7..]);
        assert_eq!(digest.to_string(), GOOD);
    }

    #[test]
    fn rejects_other_algorithms_and_missing_prefix() {
        assert_eq!(
            Digest::parse(&GOOD.replace("sha256:", "sha512:")),
            Err(DigestError::MissingPrefix)
        );
        assert_eq!(Digest::parse(&GOOD[7..]), Err(DigestError::MissingPrefix));
        assert_eq!(Digest::parse(""), Err(DigestError::MissingPrefix));
        assert_eq!(
            Digest::parse(&format!("SHA256:{}", &GOOD[7..])),
            Err(DigestError::MissingPrefix)
        );
    }

    #[test]
    fn rejects_wrong_length() {
        assert_eq!(
            Digest::parse(&GOOD[..GOOD.len() - 1]),
            Err(DigestError::WrongLength { found: 63 })
        );
        assert_eq!(
            Digest::parse(&format!("{GOOD}0")),
            Err(DigestError::WrongLength { found: 65 })
        );
        assert_eq!(
            Digest::parse("sha256:"),
            Err(DigestError::WrongLength { found: 0 })
        );
    }

    #[test]
    fn rejects_uppercase_and_non_hex() {
        let upper = format!("sha256:{}", GOOD[7..].replace('a', "A"));
        assert_eq!(
            Digest::parse(&upper),
            Err(DigestError::NotLowercaseHex { position: 17 })
        );
        let mut spaced = GOOD.to_owned();
        spaced.replace_range(70.., " ");
        assert_eq!(
            Digest::parse(&spaced),
            Err(DigestError::NotLowercaseHex { position: 70 })
        );
        let unicode = format!("sha256:{}é", &GOOD[7..69]);
        assert_eq!(unicode.len() - 7, 64);
        assert!(matches!(
            Digest::parse(&unicode),
            Err(DigestError::NotLowercaseHex { .. })
        ));
    }

    #[test]
    fn serde_round_trip_validates() {
        let digest = Digest::parse(GOOD).unwrap();
        let json = serde_json::to_string(&digest).unwrap();
        assert_eq!(json, format!("\"{GOOD}\""));
        assert_eq!(serde_json::from_str::<Digest>(&json).unwrap(), digest);
        assert!(serde_json::from_str::<Digest>("\"sha256:abc\"").is_err());
    }

    proptest! {
        #[test]
        fn every_lowercase_hex_string_is_a_digest(hex in "[0-9a-f]{64}") {
            let text = format!("sha256:{hex}");
            let digest = Digest::parse(&text).unwrap();
            prop_assert_eq!(digest.as_str(), text.as_str());
            prop_assert_eq!(digest.hex(), hex.as_str());
            prop_assert_eq!(text.parse::<Digest>().unwrap(), digest);
        }

        #[test]
        fn parse_accepts_only_the_canonical_form(s in "\\PC{0,80}") {
            let valid = s.strip_prefix("sha256:").is_some_and(|hex| {
                hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            });
            prop_assert_eq!(Digest::parse(&s).is_ok(), valid);
        }
    }
}
