use std::{collections::HashMap, hash::BuildHasher};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use zbus::{Optional, OwnedVariant, Signature, Str, Type, Variant};

/// One held lane family, `(sstb)` on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Hold {
    pub family: String,
    /// The newer Fedora builds it holds back from, comma-separated; `""` when unknown.
    pub newer: String,
    /// Unix seconds; 0 when unknown.
    pub since: u64,
    /// False when the newer builds carry no security update, or when that is unknown.
    pub security: bool,
}

type HoldTuple = (String, String, u64, bool);

use crate::values::ValueError;

/// The empty dictionary means "no such deployment". Unknown keys are ignored, so a newer
/// daemon can add keys; a known key of the wrong D-Bus type is an error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Deployment {
    pub image: Option<String>,
    pub digest: Option<String>,
    pub version: Option<String>,
    /// Build time, Unix seconds.
    pub timestamp: Option<u64>,
    pub pinned: Option<bool>,
    /// Staged only: the stage is locked and will not boot.
    pub download_only: Option<bool>,
    /// From the deployment's own manifest, so absent for `Available`.
    pub degraded: Option<Vec<String>>,
    /// Lane families kept at their last good build, from the deployment's own manifest.
    pub held: Option<Vec<String>>,
    /// From the deployment's lane lock, so absent for `Available`.
    pub holds: Option<Vec<Hold>>,
    /// `Available` and `Staged` only: bytes still to download as the check found them, 0
    /// when unknown.
    pub download_size: Option<u64>,
}

const DICT_SIGNATURE: &Signature = <HashMap<String, OwnedVariant> as Type>::SIGNATURE;

impl Deployment {
    pub const IMAGE: &'static str = "image";
    pub const DIGEST: &'static str = "digest";
    pub const VERSION: &'static str = "version";
    pub const TIMESTAMP: &'static str = "timestamp";
    pub const PINNED: &'static str = "pinned";
    pub const DOWNLOAD_ONLY: &'static str = "download-only";
    pub const DEGRADED: &'static str = "degraded";
    pub const HELD: &'static str = "held";
    pub const HOLDS: &'static str = "holds";
    pub const DOWNLOAD_SIZE: &'static str = "download-size";

    pub const KEYS: [(&'static str, &'static Signature); 10] = [
        (Self::IMAGE, <String as Type>::SIGNATURE),
        (Self::DIGEST, <String as Type>::SIGNATURE),
        (Self::VERSION, <String as Type>::SIGNATURE),
        (Self::TIMESTAMP, <u64 as Type>::SIGNATURE),
        (Self::PINNED, <bool as Type>::SIGNATURE),
        (Self::DOWNLOAD_ONLY, <bool as Type>::SIGNATURE),
        (Self::DEGRADED, <Vec<String> as Type>::SIGNATURE),
        (Self::HELD, <Vec<String> as Type>::SIGNATURE),
        (Self::HOLDS, <Vec<HoldTuple> as Type>::SIGNATURE),
        (Self::DOWNLOAD_SIZE, <u64 as Type>::SIGNATURE),
    ];

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn into_option(self) -> Option<Self> {
        (!self.is_empty()).then_some(self)
    }

    /// Each value arrives already unwrapped from its variant.
    fn from_entries<'v>(
        entries: impl IntoIterator<Item = (&'v str, &'v Variant<'v>)>,
    ) -> Result<Self, ValueError> {
        let mut deployment = Self::default();
        for (key, value) in entries {
            match key {
                Self::IMAGE => deployment.image = Some(string(key, value)?),
                Self::DIGEST => deployment.digest = Some(string(key, value)?),
                Self::VERSION => deployment.version = Some(string(key, value)?),
                Self::TIMESTAMP => deployment.timestamp = Some(uint64(key, value)?),
                Self::PINNED => deployment.pinned = Some(boolean(key, value)?),
                Self::DOWNLOAD_ONLY => deployment.download_only = Some(boolean(key, value)?),
                Self::DEGRADED => deployment.degraded = Some(string_array(key, value)?),
                Self::HELD => deployment.held = Some(string_array(key, value)?),
                Self::HOLDS => deployment.holds = Some(holds(key, value)?),
                Self::DOWNLOAD_SIZE => deployment.download_size = Some(uint64(key, value)?),
                _ => {}
            }
        }
        Ok(deployment)
    }
}

fn field(key: &str) -> String {
    format!("deployment key {key:?}")
}

fn string(key: &str, value: &Variant<'_>) -> Result<String, ValueError> {
    match value {
        Variant::Str(text) => Ok(text.as_str().to_owned()),
        other => Err(ValueError::wrong_type(
            field(key),
            <String as Type>::SIGNATURE,
            other,
        )),
    }
}

fn uint64(key: &str, value: &Variant<'_>) -> Result<u64, ValueError> {
    match value {
        Variant::U64(number) => Ok(*number),
        other => Err(ValueError::wrong_type(
            field(key),
            <u64 as Type>::SIGNATURE,
            other,
        )),
    }
}

fn boolean(key: &str, value: &Variant<'_>) -> Result<bool, ValueError> {
    match value {
        Variant::Bool(flag) => Ok(*flag),
        other => Err(ValueError::wrong_type(
            field(key),
            <bool as Type>::SIGNATURE,
            other,
        )),
    }
}

fn string_array(key: &str, value: &Variant<'_>) -> Result<Vec<String>, ValueError> {
    let expected = <Vec<String> as Type>::SIGNATURE;
    let Variant::Array(array) = value else {
        return Err(ValueError::wrong_type(field(key), expected, value));
    };
    // Every element has the array's element signature, so this makes the match below total.
    if array.signature() != expected {
        return Err(ValueError::wrong_type(field(key), expected, value));
    }
    array
        .iter()
        .map(|element| match element {
            Variant::Str(text) => Ok(text.as_str().to_owned()),
            other => Err(ValueError::wrong_type(field(key), expected, other)),
        })
        .collect()
}

fn holds(key: &str, value: &Variant<'_>) -> Result<Vec<Hold>, ValueError> {
    let expected = <Vec<HoldTuple> as Type>::SIGNATURE;
    let wrong = || ValueError::wrong_type(field(key), expected, value);
    let Variant::Array(array) = value else {
        return Err(wrong());
    };
    if array.signature() != expected {
        return Err(wrong());
    }
    array
        .iter()
        .map(|element| {
            let Variant::Structure(record) = element else {
                return Err(wrong());
            };
            match record.fields() {
                [
                    Variant::Str(family),
                    Variant::Str(newer),
                    Variant::U64(since),
                    Variant::Bool(security),
                ] => Ok(Hold {
                    family: family.as_str().to_owned(),
                    newer: newer.as_str().to_owned(),
                    since: *since,
                    security: *security,
                }),
                _ => Err(wrong()),
            }
        })
        .collect()
}

fn owned_holds(holds: Vec<Hold>) -> OwnedVariant {
    let tuples: Vec<HoldTuple> = holds
        .into_iter()
        .map(|h| (h.family, h.newer, h.since, h.security))
        .collect();
    OwnedVariant::from(Optional::from(Some(tuples)))
}

/// zbus converts arrays only fallibly (duplicating file descriptors can fail), but its
/// infallible `From<Optional<V>> for OwnedVariant` covers `Vec<String>`.
fn owned_string_array(items: Vec<String>) -> OwnedVariant {
    OwnedVariant::from(Optional::from(Some(items)))
}

impl<S: BuildHasher + Default> From<Deployment> for HashMap<String, OwnedVariant, S> {
    fn from(deployment: Deployment) -> Self {
        let Deployment {
            image,
            digest,
            version,
            timestamp,
            pinned,
            download_only,
            degraded,
            held,
            holds,
            download_size,
        } = deployment;
        let text = |value: String| OwnedVariant::from(Str::from(value));
        let entries = [
            (Deployment::IMAGE, image.map(text)),
            (Deployment::DIGEST, digest.map(text)),
            (Deployment::VERSION, version.map(text)),
            (Deployment::TIMESTAMP, timestamp.map(OwnedVariant::from)),
            (Deployment::PINNED, pinned.map(OwnedVariant::from)),
            (
                Deployment::DOWNLOAD_ONLY,
                download_only.map(OwnedVariant::from),
            ),
            (Deployment::DEGRADED, degraded.map(owned_string_array)),
            (Deployment::HELD, held.map(owned_string_array)),
            (Deployment::HOLDS, holds.map(owned_holds)),
            (
                Deployment::DOWNLOAD_SIZE,
                download_size.map(OwnedVariant::from),
            ),
        ];
        entries
            .into_iter()
            .filter_map(|(key, value)| Some((key.to_owned(), value?)))
            .collect()
    }
}

impl<S: BuildHasher> TryFrom<&HashMap<String, OwnedVariant, S>> for Deployment {
    type Error = ValueError;

    fn try_from(map: &HashMap<String, OwnedVariant, S>) -> Result<Self, Self::Error> {
        Self::from_entries(map.iter().map(|(key, value)| (key.as_str(), &**value)))
    }
}

impl<S: BuildHasher> TryFrom<HashMap<String, OwnedVariant, S>> for Deployment {
    type Error = ValueError;

    fn try_from(map: HashMap<String, OwnedVariant, S>) -> Result<Self, Self::Error> {
        Self::try_from(&map)
    }
}

impl TryFrom<&Variant<'_>> for Deployment {
    type Error = ValueError;

    fn try_from(value: &Variant<'_>) -> Result<Self, Self::Error> {
        let dict = match value {
            Variant::Dict(dict) if dict.signature() == DICT_SIGNATURE => dict,
            other => return Err(ValueError::wrong_type("Deployment", DICT_SIGNATURE, other)),
        };
        let entries = dict.iter().map(|(key, value)| {
            // In an a{sv} dict each value is a variant wrapping the real value.
            let key = match key {
                Variant::Str(text) => text.as_str(),
                _ => "",
            };
            let value = match value {
                Variant::Variant(inner) => &**inner,
                plain => plain,
            };
            (key, value)
        });
        Self::from_entries(entries)
    }
}

/// For a `PropertiesChanged` value read by hand.
impl TryFrom<OwnedVariant> for Deployment {
    type Error = ValueError;

    fn try_from(value: OwnedVariant) -> Result<Self, Self::Error> {
        Self::try_from(&*value)
    }
}

impl From<Deployment> for OwnedVariant {
    fn from(deployment: Deployment) -> Self {
        OwnedVariant::from(HashMap::<String, OwnedVariant>::from(deployment))
    }
}

impl From<Deployment> for Variant<'static> {
    fn from(deployment: Deployment) -> Self {
        Variant::from(HashMap::<String, OwnedVariant>::from(deployment))
    }
}

impl Type for Deployment {
    const SIGNATURE: &'static Signature = DICT_SIGNATURE;
}

/// What the proxy and the daemon's `#[zbus::interface]` properties use, as the `a{sv}` dictionary.
impl Serialize for Deployment {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        HashMap::<String, OwnedVariant>::from(self.clone()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Deployment {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = HashMap::<String, OwnedVariant>::deserialize(deserializer)?;
        Self::try_from(map).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::{collection::vec, option, prelude::*};
    use zbus::Dict;

    fn full() -> Deployment {
        Deployment {
            image: Some("ghcr.io/apex-linux/apex:45".to_owned()),
            digest: Some(format!("sha256:{}", "ab".repeat(32))),
            version: Some("45.20260924.1".to_owned()),
            timestamp: Some(1_790_000_000),
            pinned: Some(false),
            download_only: Some(true),
            degraded: Some(vec!["audio".to_owned()]),
            held: Some(vec!["gnome-shell".to_owned()]),
            holds: Some(vec![Hold {
                family: "gnome-shell".to_owned(),
                newer: "gnome-shell-51.1-1.fc45,mutter-51.1-1.fc45".to_owned(),
                since: 1_791_000_000,
                security: true,
            }]),
            download_size: Some(1_200_000_000),
        }
    }

    fn hold_strategy() -> impl Strategy<Value = Hold> {
        (TEXT, TEXT, any::<u64>(), any::<bool>()).prop_map(|(family, newer, since, security)| {
            Hold {
                family,
                newer,
                since,
                security,
            }
        })
    }

    /// D-Bus strings cannot contain NUL.
    const TEXT: &str = "[^\\x00]{0,40}";

    fn deployments() -> impl Strategy<Value = Deployment> {
        (
            option::of(TEXT),
            option::of("sha256:[0-9a-f]{64}"),
            option::of("[0-9]{2}\\.[0-9]{8}\\.[0-9]{1,4}"),
            option::of(any::<u64>()),
            option::of(any::<bool>()),
            option::of(any::<bool>()),
            option::of(vec(TEXT, 0..4)),
            option::of(vec(TEXT, 0..4)),
            option::of(vec(hold_strategy(), 0..3)),
            option::of(any::<u64>()),
        )
            .prop_map(
                |(
                    image,
                    digest,
                    version,
                    timestamp,
                    pinned,
                    download_only,
                    degraded,
                    held,
                    holds,
                    download_size,
                )| Deployment {
                    image,
                    digest,
                    version,
                    timestamp,
                    pinned,
                    download_only,
                    degraded,
                    held,
                    holds,
                    download_size,
                },
            )
    }

    #[test]
    fn keys_and_signatures_match_the_map() {
        let map = HashMap::<String, OwnedVariant>::from(full());
        assert_eq!(map.len(), Deployment::KEYS.len());
        for (key, signature) in Deployment::KEYS {
            let value = map.get(key).unwrap_or_else(|| panic!("{key} missing"));
            assert_eq!(value.value_signature(), signature, "{key}");
        }
        let rendered: Vec<String> = Deployment::KEYS
            .iter()
            .map(|(key, signature)| format!("{key}:{signature}"))
            .collect();
        assert_eq!(
            rendered,
            [
                "image:s",
                "digest:s",
                "version:s",
                "timestamp:t",
                "pinned:b",
                "download-only:b",
                "degraded:as",
                "held:as",
                "holds:a(sstb)",
                "download-size:t"
            ]
        );
        assert_eq!(Deployment::SIGNATURE.to_string(), "a{sv}");
    }

    #[test]
    fn empty_dictionary_is_no_deployment() {
        let empty = Deployment::try_from(HashMap::<String, OwnedVariant>::new()).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.into_option(), None);
        assert!(HashMap::<String, OwnedVariant>::from(Deployment::default()).is_empty());
        let only_pinned = Deployment {
            pinned: Some(false),
            ..Deployment::default()
        };
        assert!(!only_pinned.is_empty());
        assert_eq!(only_pinned.clone().into_option(), Some(only_pinned));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let mut map = HashMap::<String, OwnedVariant>::from(full());
        map.insert("soft-reboot-capable".to_owned(), OwnedVariant::from(true));
        map.insert("size".to_owned(), OwnedVariant::from(12_u32));
        assert_eq!(Deployment::try_from(map), Ok(full()));
    }

    #[test]
    fn wrong_types_are_errors() {
        let cases = [
            (Deployment::IMAGE, OwnedVariant::from(1_u64), "s", "t"),
            (Deployment::TIMESTAMP, OwnedVariant::from(1_u32), "t", "u"),
            (Deployment::TIMESTAMP, OwnedVariant::from(1_i64), "t", "x"),
            (
                Deployment::PINNED,
                OwnedVariant::from(Str::from_static("yes")),
                "b",
                "s",
            ),
            (
                Deployment::DEGRADED,
                OwnedVariant::from(Str::from_static("audio")),
                "as",
                "s",
            ),
            (
                Deployment::DEGRADED,
                OwnedVariant::from(Optional::from(Some(vec![1_u32]))),
                "as",
                "au",
            ),
        ];
        for (key, value, expected, found) in cases {
            let map = HashMap::from([(key.to_owned(), value)]);
            let error = Deployment::try_from(map).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("deployment key {key:?}: expected D-Bus type {expected}, found {found}")
            );
        }
    }

    #[test]
    fn non_dictionary_value_is_an_error() {
        let error = Deployment::try_from(OwnedVariant::from(true)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Deployment: expected D-Bus type a{sv}, found b"
        );
        let mut dict = Dict::new(<String as Type>::SIGNATURE, <String as Type>::SIGNATURE);
        dict.append(Variant::from("image"), Variant::from("x"))
            .unwrap();
        let error = Deployment::try_from(&Variant::Dict(dict)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Deployment: expected D-Bus type a{sv}, found a{ss}"
        );
    }

    #[test]
    fn holds_of_the_wrong_shape_are_refused() {
        let mut map = HashMap::<String, OwnedVariant>::new();
        let wrong: Vec<(String, String, u32, bool)> = vec![("a".into(), "b".into(), 1, true)];
        map.insert(
            Deployment::HOLDS.to_owned(),
            OwnedVariant::from(Optional::from(Some(wrong))),
        );
        let error = Deployment::try_from(map).unwrap_err();
        assert!(error.to_string().contains("a(sstb)"), "{error}");
    }

    #[test]
    fn a_dict_without_held_reads_as_unknown() {
        let mut map = HashMap::<String, OwnedVariant>::from(full());
        map.remove(Deployment::HELD);
        let deployment = Deployment::try_from(map).unwrap();
        assert_eq!(deployment.held, None);
        assert_eq!(deployment.degraded, full().degraded);
    }

    #[test]
    fn empty_degraded_list_keeps_its_type() {
        let deployment = Deployment {
            degraded: Some(Vec::new()),
            ..Deployment::default()
        };
        let map = HashMap::<String, OwnedVariant>::from(deployment.clone());
        assert_eq!(
            map[Deployment::DEGRADED].value_signature().to_string(),
            "as"
        );
        assert_eq!(Deployment::try_from(map), Ok(deployment));
    }

    proptest! {
        #[test]
        fn map_round_trip(deployment in deployments()) {
            let map = HashMap::<String, OwnedVariant>::from(deployment.clone());
            prop_assert_eq!(Deployment::try_from(map), Ok(deployment));
        }

        #[test]
        fn map_is_preserved(deployment in deployments()) {
            let map = HashMap::<String, OwnedVariant>::from(deployment);
            let again = HashMap::<String, OwnedVariant>::from(Deployment::try_from(&map).unwrap());
            prop_assert_eq!(again, map);
        }

        /// As the daemon's property getter produces it and the proxy's reads it.
        #[test]
        fn property_value_round_trip(deployment in deployments()) {
            let value = Variant::from(deployment.clone());
            prop_assert_eq!(value.value_signature(), DICT_SIGNATURE);
            let owned = OwnedVariant::try_from(value).unwrap();
            prop_assert_eq!(Deployment::try_from(owned), Ok(deployment.clone()));
            let direct = OwnedVariant::from(deployment.clone());
            prop_assert_eq!(Deployment::try_from(direct), Ok(deployment));
        }

        #[test]
        fn dbus_encoding_round_trip(deployment in deployments()) {
            let ctxt = zbus::wire::serialized::Context::new(zbus::wire::LE, 0);
            let encoded = zbus::wire::to_bytes(ctxt, &Variant::from(deployment.clone())).unwrap();
            let (decoded, _): (OwnedVariant, _) = encoded.deserialize().unwrap();
            prop_assert_eq!(Deployment::try_from(decoded), Ok(deployment));
        }
    }
}
