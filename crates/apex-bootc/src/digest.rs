//! Validated OCI manifest digests.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A manifest digest of the form `sha256:` followed by exactly 64 **lowercase** hex digits.
///
/// Lowercase only, as the OCI image spec requires for sha256. Accepting uppercase would
/// give one image two spellings, and digests are compared for equality everywhere
/// (staged vs. expected, denylist membership).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);

/// Why a string is not a [`Digest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DigestError {
    /// The string does not start with `sha256:`.
    #[error("digest must start with \"sha256:\"")]
    MissingPrefix,
    /// The hex part does not have 64 characters.
    #[error("digest must have 64 hex digits after \"sha256:\", found {0} characters")]
    Length(usize),
    /// The hex part contains something other than `0-9a-f`.
    #[error("digest must contain only lowercase hex digits after \"sha256:\"")]
    NotLowercaseHex,
}

impl Digest {
    /// The only algorithm prefix accepted.
    pub const PREFIX: &'static str = "sha256:";
    /// Number of hex digits after the prefix.
    pub const HEX_LEN: usize = 64;

    /// Validates `s` as a sha256 digest.
    pub fn parse(s: &str) -> Result<Self, DigestError> {
        validate(s)?;
        Ok(Self(s.to_owned()))
    }

    /// The full digest, `sha256:…`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The 64 hex digits without the algorithm prefix.
    pub fn hex(&self) -> &str {
        &self.0[Self::PREFIX.len()..]
    }
}

/// Validation shared by [`Digest::parse`] and the `const` image reference checks.
pub(crate) const fn validate(s: &str) -> Result<(), DigestError> {
    let bytes = s.as_bytes();
    let prefix = Digest::PREFIX.as_bytes();
    if bytes.len() < prefix.len() {
        return Err(DigestError::MissingPrefix);
    }
    let mut i = 0;
    while i < prefix.len() {
        if bytes[i] != prefix[i] {
            return Err(DigestError::MissingPrefix);
        }
        i += 1;
    }
    let hex_len = bytes.len() - prefix.len();
    if hex_len != Digest::HEX_LEN {
        return Err(DigestError::Length(hex_len));
    }
    while i < bytes.len() {
        if !matches!(bytes[i], b'0'..=b'9' | b'a'..=b'f') {
            return Err(DigestError::NotLowercaseHex);
        }
        i += 1;
    }
    Ok(())
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
        validate(&s)?;
        Ok(Self(s))
    }
}

impl From<Digest> for String {
    fn from(d: Digest) -> Self {
        d.0
    }
}

impl AsRef<str> for Digest {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const GOOD: &str = "sha256:47e5ed613a970b6574bfa954ab25bb6e85656552899aa518b5961d9645102b38";

    #[test]
    fn accepts_a_real_digest() {
        let d = Digest::parse(GOOD).unwrap();
        assert_eq!(d.as_str(), GOOD);
        assert_eq!(d.hex().len(), 64);
        assert_eq!(d.to_string(), GOOD);
    }

    #[test]
    fn rejects_malformed_digests() {
        assert_eq!(Digest::parse(""), Err(DigestError::MissingPrefix));
        assert_eq!(Digest::parse("sha512:00"), Err(DigestError::MissingPrefix));
        assert_eq!(Digest::parse("sha256:abc"), Err(DigestError::Length(3)));
        assert_eq!(
            Digest::parse(&GOOD.to_uppercase().replace("SHA256", "sha256")),
            Err(DigestError::NotLowercaseHex)
        );
        assert_eq!(
            Digest::parse(&format!("{GOOD}0")),
            Err(DigestError::Length(65))
        );
        let with_space = format!("sha256:{}", " ".repeat(64));
        assert_eq!(
            Digest::parse(&with_space),
            Err(DigestError::NotLowercaseHex)
        );
    }

    #[test]
    fn serde_round_trip_validates() {
        let d: Digest = serde_json::from_str(&format!("\"{GOOD}\"")).unwrap();
        assert_eq!(serde_json::to_string(&d).unwrap(), format!("\"{GOOD}\""));
        assert!(serde_json::from_str::<Digest>("\"sha256:zz\"").is_err());
    }

    proptest! {
        #[test]
        fn valid_hex_always_parses(hex in "[0-9a-f]{64}") {
            let s = format!("sha256:{hex}");
            let d = Digest::parse(&s).unwrap();
            prop_assert_eq!(d.hex(), hex.as_str());
        }

        #[test]
        fn parse_never_panics_and_only_accepts_canonical_form(s in "\\PC{0,80}") {
            if let Ok(d) = Digest::parse(&s) {
                prop_assert!(d.as_str().starts_with("sha256:"));
                prop_assert_eq!(d.hex().len(), 64);
                prop_assert!(d.hex().bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
            }
        }
    }
}
