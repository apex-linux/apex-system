//! Apex image versions: `<fedora>.<yyyymmdd>.<run>`, e.g. `45.20260924.117`.
//!
//! The version is the `org.opencontainers.image.version` label and the `version` field of
//! `/usr/share/apex/manifest.json` (contract §1 and §3). Versions compare numerically
//! field by field. Extra numeric fields are tolerated (`45.20260924.117.2`), and a longer
//! version whose leading fields equal a shorter one is newer, so the order is plain
//! lexicographic order over the numbers.

use std::fmt;

/// A parsed Apex image version.
///
/// The fields are kept as a boxed slice: a version is built once and never grows, and
/// the derived lexicographic `Ord` on `[u64]` is exactly the required order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    fields: Box<[u64]>,
}

/// The fewest fields a valid version has: Fedora release, build date and run number.
pub const MIN_VERSION_FIELDS: usize = 3;

impl Version {
    /// Parses `<fedora>.<yyyymmdd>.<run>[.<more>...]`.
    ///
    /// Every field must be a non-empty run of ASCII digits that fits in a `u64`, and there
    /// must be at least [`MIN_VERSION_FIELDS`] of them. Anything else (signs, spaces,
    /// letters, empty fields, a leading `v`) returns `None`. The date is not checked
    /// against the calendar: ordering only needs the numbers.
    pub fn parse(s: &str) -> Option<Self> {
        let fields = s
            .split('.')
            .map(|field| {
                if field.is_empty() || !field.bytes().all(|b| b.is_ascii_digit()) {
                    None
                } else {
                    field.parse::<u64>().ok()
                }
            })
            .collect::<Option<Box<[u64]>>>()?;
        (fields.len() >= MIN_VERSION_FIELDS).then_some(Self { fields })
    }

    /// The Fedora release the image is built on, e.g. 45.
    pub fn fedora(&self) -> u64 {
        self.fields[0]
    }

    /// The build date as the number `yyyymmdd`.
    pub fn date(&self) -> u64 {
        self.fields[1]
    }

    /// The CI run number.
    pub fn run(&self) -> u64 {
        self.fields[2]
    }

    /// All numeric fields, at least [`MIN_VERSION_FIELDS`] of them.
    pub fn fields(&self) -> &[u64] {
        &self.fields
    }
}

impl fmt::Display for Version {
    /// Formats the canonical form: the fields joined by `.`, without leading zeros.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, field) in self.fields.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{field}")?;
        }
        Ok(())
    }
}

impl serde::Serialize for Version {
    /// Serializes as the canonical string, e.g. `"45.20260924.117"`.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Version {
    /// Deserializes from a version string; an invalid string is an error.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(&text),
                &"a version like 45.20260924.117",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn serde_uses_the_string_form() {
        let version = v("45.20260924.117");
        let json = serde_json::to_string(&version).expect("serialize");
        assert_eq!(json, "\"45.20260924.117\"");
        assert_eq!(
            serde_json::from_str::<Version>(&json).expect("deserialize"),
            version
        );
        assert!(serde_json::from_str::<Version>("\"45.x.1\"").is_err());
        assert!(serde_json::from_str::<Version>("45").is_err());
    }

    fn v(s: &str) -> Version {
        Version::parse(s).expect("valid version")
    }

    #[test]
    fn parses_the_contract_example() {
        let version = v("45.20260924.117");
        assert_eq!(version.fedora(), 45);
        assert_eq!(version.date(), 20_260_924);
        assert_eq!(version.run(), 117);
        assert_eq!(version.fields(), &[45, 20_260_924, 117]);
        assert_eq!(version.to_string(), "45.20260924.117");
    }

    #[test]
    fn tolerates_more_numeric_fields() {
        assert_eq!(v("45.20260924.117.3").fields(), &[45, 20_260_924, 117, 3]);
    }

    #[test]
    fn rejects_invalid_strings() {
        for bad in [
            "",
            "45",
            "45.20260924",
            "45.20260924.",
            ".45.20260924.1",
            "45..1",
            "v45.20260924.1",
            "45.20260924.1a",
            "45.20260924.+1",
            "45.20260924.-1",
            " 45.20260924.1",
            "45.20260924.1 ",
            "45.2026-09-24.1",
            "45.20260924.99999999999999999999999",
            "٤٥.20260924.1",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?} must be invalid");
        }
    }

    #[test]
    fn compares_numerically_not_textually() {
        assert!(v("45.20260924.9") < v("45.20260924.10"));
        assert!(v("45.20260924.117") < v("45.20260925.1"));
        assert!(v("45.20261231.500") < v("46.20260101.1"));
        assert_eq!(v("45.020260924.1"), v("45.20260924.1"));
    }

    #[test]
    fn a_longer_version_with_the_same_prefix_is_newer() {
        assert!(v("45.20260924.117") < v("45.20260924.117.0"));
        assert!(v("45.20260924.117.5") < v("45.20260924.118"));
    }

    fn fields() -> impl Strategy<Value = Vec<u64>> {
        proptest::collection::vec(
            prop_oneof![0u64..3, 20_260_101u64..20_261_231, any::<u64>()],
            3..6,
        )
    }

    proptest! {
        #[test]
        fn display_round_trips(f in fields()) {
            let text = f.iter().map(u64::to_string).collect::<Vec<_>>().join(".");
            let parsed = Version::parse(&text).expect("generated versions are valid");
            prop_assert_eq!(parsed.fields(), f.as_slice());
            prop_assert_eq!(parsed.to_string(), text);
        }

        #[test]
        fn order_is_lexicographic_over_the_numbers(a in fields(), b in fields()) {
            let va = Version::parse(&a.iter().map(u64::to_string).collect::<Vec<_>>().join(".")).expect("valid");
            let vb = Version::parse(&b.iter().map(u64::to_string).collect::<Vec<_>>().join(".")).expect("valid");
            prop_assert_eq!(va.cmp(&vb), a.cmp(&b));
        }

        #[test]
        fn never_panics(s in ".*") {
            let _ = Version::parse(&s);
        }
    }
}
