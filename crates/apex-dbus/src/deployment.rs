use std::{collections::HashMap, hash::BuildHasher};

use zbus::zvariant::{Optional, OwnedValue, Signature, Str, Type, Value};

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
}

const DICT_SIGNATURE: &Signature = <HashMap<String, OwnedValue> as Type>::SIGNATURE;

impl Deployment {
    pub const IMAGE: &'static str = "image";
    pub const DIGEST: &'static str = "digest";
    pub const VERSION: &'static str = "version";
    pub const TIMESTAMP: &'static str = "timestamp";
    pub const PINNED: &'static str = "pinned";
    pub const DOWNLOAD_ONLY: &'static str = "download-only";
    pub const DEGRADED: &'static str = "degraded";

    pub const KEYS: [(&'static str, &'static Signature); 7] = [
        (Self::IMAGE, <String as Type>::SIGNATURE),
        (Self::DIGEST, <String as Type>::SIGNATURE),
        (Self::VERSION, <String as Type>::SIGNATURE),
        (Self::TIMESTAMP, <u64 as Type>::SIGNATURE),
        (Self::PINNED, <bool as Type>::SIGNATURE),
        (Self::DOWNLOAD_ONLY, <bool as Type>::SIGNATURE),
        (Self::DEGRADED, <Vec<String> as Type>::SIGNATURE),
    ];

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn into_option(self) -> Option<Self> {
        (!self.is_empty()).then_some(self)
    }

    /// Each value arrives already unwrapped from its variant.
    fn from_entries<'v>(
        entries: impl IntoIterator<Item = (&'v str, &'v Value<'v>)>,
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
                _ => {}
            }
        }
        Ok(deployment)
    }
}

fn field(key: &str) -> String {
    format!("deployment key {key:?}")
}

fn string(key: &str, value: &Value<'_>) -> Result<String, ValueError> {
    match value {
        Value::Str(text) => Ok(text.as_str().to_owned()),
        other => Err(ValueError::wrong_type(
            field(key),
            <String as Type>::SIGNATURE,
            other,
        )),
    }
}

fn uint64(key: &str, value: &Value<'_>) -> Result<u64, ValueError> {
    match value {
        Value::U64(number) => Ok(*number),
        other => Err(ValueError::wrong_type(
            field(key),
            <u64 as Type>::SIGNATURE,
            other,
        )),
    }
}

fn boolean(key: &str, value: &Value<'_>) -> Result<bool, ValueError> {
    match value {
        Value::Bool(flag) => Ok(*flag),
        other => Err(ValueError::wrong_type(
            field(key),
            <bool as Type>::SIGNATURE,
            other,
        )),
    }
}

fn string_array(key: &str, value: &Value<'_>) -> Result<Vec<String>, ValueError> {
    let expected = <Vec<String> as Type>::SIGNATURE;
    let Value::Array(array) = value else {
        return Err(ValueError::wrong_type(field(key), expected, value));
    };
    // Every element has the array's element signature, so this makes the match below total.
    if array.signature() != expected {
        return Err(ValueError::wrong_type(field(key), expected, value));
    }
    array
        .iter()
        .map(|element| match element {
            Value::Str(text) => Ok(text.as_str().to_owned()),
            other => Err(ValueError::wrong_type(field(key), expected, other)),
        })
        .collect()
}

/// zvariant converts arrays only fallibly (duplicating file descriptors can fail), but its
/// infallible `From<Optional<V>> for OwnedValue` covers `Vec<String>`.
fn owned_string_array(items: Vec<String>) -> OwnedValue {
    OwnedValue::from(Optional::from(Some(items)))
}

impl<S: BuildHasher + Default> From<Deployment> for HashMap<String, OwnedValue, S> {
    fn from(deployment: Deployment) -> Self {
        let Deployment {
            image,
            digest,
            version,
            timestamp,
            pinned,
            download_only,
            degraded,
        } = deployment;
        let text = |value: String| OwnedValue::from(Str::from(value));
        let entries = [
            (Deployment::IMAGE, image.map(text)),
            (Deployment::DIGEST, digest.map(text)),
            (Deployment::VERSION, version.map(text)),
            (Deployment::TIMESTAMP, timestamp.map(OwnedValue::from)),
            (Deployment::PINNED, pinned.map(OwnedValue::from)),
            (
                Deployment::DOWNLOAD_ONLY,
                download_only.map(OwnedValue::from),
            ),
            (Deployment::DEGRADED, degraded.map(owned_string_array)),
        ];
        entries
            .into_iter()
            .filter_map(|(key, value)| Some((key.to_owned(), value?)))
            .collect()
    }
}

impl<S: BuildHasher> TryFrom<&HashMap<String, OwnedValue, S>> for Deployment {
    type Error = ValueError;

    fn try_from(map: &HashMap<String, OwnedValue, S>) -> Result<Self, Self::Error> {
        Self::from_entries(map.iter().map(|(key, value)| (key.as_str(), &**value)))
    }
}

impl<S: BuildHasher> TryFrom<HashMap<String, OwnedValue, S>> for Deployment {
    type Error = ValueError;

    fn try_from(map: HashMap<String, OwnedValue, S>) -> Result<Self, Self::Error> {
        Self::try_from(&map)
    }
}

impl TryFrom<&Value<'_>> for Deployment {
    type Error = ValueError;

    fn try_from(value: &Value<'_>) -> Result<Self, Self::Error> {
        let dict = match value {
            Value::Dict(dict) if dict.signature() == DICT_SIGNATURE => dict,
            other => return Err(ValueError::wrong_type("Deployment", DICT_SIGNATURE, other)),
        };
        let entries = dict.iter().map(|(key, value)| {
            // In an a{sv} dict each value is a variant wrapping the real value.
            let key = match key {
                Value::Str(text) => text.as_str(),
                _ => "",
            };
            let value = match value {
                Value::Value(inner) => &**inner,
                plain => plain,
            };
            (key, value)
        });
        Self::from_entries(entries)
    }
}

/// What the proxy's property getters use.
impl TryFrom<OwnedValue> for Deployment {
    type Error = ValueError;

    fn try_from(value: OwnedValue) -> Result<Self, Self::Error> {
        Self::try_from(&*value)
    }
}

impl From<Deployment> for OwnedValue {
    fn from(deployment: Deployment) -> Self {
        OwnedValue::from(HashMap::<String, OwnedValue>::from(deployment))
    }
}

/// What the daemon's `#[zbus::interface]` property getters use.
impl From<Deployment> for Value<'static> {
    fn from(deployment: Deployment) -> Self {
        Value::from(HashMap::<String, OwnedValue>::from(deployment))
    }
}

impl Type for Deployment {
    const SIGNATURE: &'static Signature = DICT_SIGNATURE;
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::{collection::vec, option, prelude::*};
    use zbus::zvariant::Dict;

    fn full() -> Deployment {
        Deployment {
            image: Some("ghcr.io/apex-linux/apex:45".to_owned()),
            digest: Some(format!("sha256:{}", "ab".repeat(32))),
            version: Some("45.20260924.1".to_owned()),
            timestamp: Some(1_790_000_000),
            pinned: Some(false),
            download_only: Some(true),
            degraded: Some(vec!["audio".to_owned()]),
        }
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
        )
            .prop_map(
                |(image, digest, version, timestamp, pinned, download_only, degraded)| Deployment {
                    image,
                    digest,
                    version,
                    timestamp,
                    pinned,
                    download_only,
                    degraded,
                },
            )
    }

    #[test]
    fn keys_and_signatures_match_the_map() {
        let map = HashMap::<String, OwnedValue>::from(full());
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
                "degraded:as"
            ]
        );
        assert_eq!(Deployment::SIGNATURE.to_string(), "a{sv}");
    }

    #[test]
    fn empty_dictionary_is_no_deployment() {
        let empty = Deployment::try_from(HashMap::<String, OwnedValue>::new()).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.into_option(), None);
        assert!(HashMap::<String, OwnedValue>::from(Deployment::default()).is_empty());
        let only_pinned = Deployment {
            pinned: Some(false),
            ..Deployment::default()
        };
        assert!(!only_pinned.is_empty());
        assert_eq!(only_pinned.clone().into_option(), Some(only_pinned));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let mut map = HashMap::<String, OwnedValue>::from(full());
        map.insert("soft-reboot-capable".to_owned(), OwnedValue::from(true));
        map.insert("size".to_owned(), OwnedValue::from(12_u32));
        assert_eq!(Deployment::try_from(map), Ok(full()));
    }

    #[test]
    fn wrong_types_are_errors() {
        let cases = [
            (Deployment::IMAGE, OwnedValue::from(1_u64), "s", "t"),
            (Deployment::TIMESTAMP, OwnedValue::from(1_u32), "t", "u"),
            (Deployment::TIMESTAMP, OwnedValue::from(1_i64), "t", "x"),
            (
                Deployment::PINNED,
                OwnedValue::from(Str::from_static("yes")),
                "b",
                "s",
            ),
            (
                Deployment::DEGRADED,
                OwnedValue::from(Str::from_static("audio")),
                "as",
                "s",
            ),
            (
                Deployment::DEGRADED,
                OwnedValue::from(Optional::from(Some(vec![1_u32]))),
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
        let error = Deployment::try_from(OwnedValue::from(true)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Deployment: expected D-Bus type a{sv}, found b"
        );
        let mut dict = Dict::new(<String as Type>::SIGNATURE, <String as Type>::SIGNATURE);
        dict.append(Value::from("image"), Value::from("x")).unwrap();
        let error = Deployment::try_from(&Value::Dict(dict)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Deployment: expected D-Bus type a{sv}, found a{ss}"
        );
    }

    #[test]
    fn empty_degraded_list_keeps_its_type() {
        let deployment = Deployment {
            degraded: Some(Vec::new()),
            ..Deployment::default()
        };
        let map = HashMap::<String, OwnedValue>::from(deployment.clone());
        assert_eq!(
            map[Deployment::DEGRADED].value_signature().to_string(),
            "as"
        );
        assert_eq!(Deployment::try_from(map), Ok(deployment));
    }

    proptest! {
        #[test]
        fn map_round_trip(deployment in deployments()) {
            let map = HashMap::<String, OwnedValue>::from(deployment.clone());
            prop_assert_eq!(Deployment::try_from(map), Ok(deployment));
        }

        #[test]
        fn map_is_preserved(deployment in deployments()) {
            let map = HashMap::<String, OwnedValue>::from(deployment);
            let again = HashMap::<String, OwnedValue>::from(Deployment::try_from(&map).unwrap());
            prop_assert_eq!(again, map);
        }

        /// As the daemon's property getter produces it and the proxy's reads it.
        #[test]
        fn property_value_round_trip(deployment in deployments()) {
            let value = Value::from(deployment.clone());
            prop_assert_eq!(value.value_signature(), DICT_SIGNATURE);
            let owned = OwnedValue::try_from(value).unwrap();
            prop_assert_eq!(Deployment::try_from(owned), Ok(deployment.clone()));
            let direct = OwnedValue::from(deployment.clone());
            prop_assert_eq!(Deployment::try_from(direct), Ok(deployment));
        }

        #[test]
        fn dbus_encoding_round_trip(deployment in deployments()) {
            let ctxt = zbus::zvariant::serialized::Context::new_dbus(zbus::zvariant::LE, 0);
            let encoded = zbus::zvariant::to_bytes(ctxt, &Value::from(deployment.clone())).unwrap();
            let (decoded, _): (OwnedValue, _) = encoded.deserialize().unwrap();
            prop_assert_eq!(Deployment::try_from(decoded), Ok(deployment));
        }
    }
}
