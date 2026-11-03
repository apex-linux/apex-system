use std::{fmt, marker::PhantomData, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use zbus::{OwnedVariant, Signature, Str, Type, Variant};

/// One entry per RPM architecture. D-Bus has no null string, so an added package has an
/// empty `from_evr` and a removed one an empty `to_evr`.
pub type Change = (String, String, String, String, String);

pub const CHANGE_FIELDS: [&str; 5] = ["name", "arch", "kind", "from_evr", "to_evr"];

/// A read-only `/var` snapshot `MajorUpgrade` made: path, the Fedora major it was taken on,
/// Unix seconds.
pub type Snapshot = (String, u32, u64);

pub const SNAPSHOT_FIELDS: [&str; 3] = ["path", "major", "taken_at"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{value:?} is not a valid {what}")]
pub struct UnknownValueError {
    what: &'static str,
    value: String,
}

impl UnknownValueError {
    fn new(what: &'static str, value: &str) -> Self {
        Self {
            what,
            value: value.to_owned(),
        }
    }

    pub fn what(&self) -> &'static str {
        self.what
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValueError {
    #[error("{field}: expected D-Bus type {expected}, found {found}")]
    WrongType {
        field: String,
        expected: &'static Signature,
        found: Signature,
    },
    #[error(transparent)]
    Unknown(#[from] UnknownValueError),
}

impl ValueError {
    pub(crate) fn wrong_type(
        field: impl Into<String>,
        expected: &'static Signature,
        found: &Variant<'_>,
    ) -> Self {
        Self::WrongType {
            field: field.into(),
            expected,
            found: found.value_signature().clone(),
        }
    }
}

/// So `?` turns a `ValueError` from the `TryFrom<OwnedVariant>` conversions into a zbus error.
impl From<ValueError> for zbus::Error {
    fn from(error: ValueError) -> Self {
        zbus::Error::Failure(error.to_string())
    }
}

struct StrVisitor<T>(PhantomData<T>);

impl<T> de::Visitor<'_> for StrVisitor<T>
where
    T: FromStr<Err = UnknownValueError>,
{
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a contract string")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<T, E> {
        value.parse().map_err(E::custom)
    }
}

/// Serde and variant conversions go through `as_str` and `FromStr`, so the wire form is the
/// contract string whatever serde or zbus do with enum variants.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $value:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value,)+
                }
            }
        }

        impl FromStr for $name {
            type Err = UnknownValueError;

            fn from_str(value: &str) -> Result<Self, UnknownValueError> {
                match value {
                    $($value => Ok(Self::$variant),)+
                    _ => Err(UnknownValueError::new(stringify!($name), value)),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_str(StrVisitor(PhantomData))
            }
        }

        impl Type for $name {
            const SIGNATURE: &'static Signature = <&str as Type>::SIGNATURE;
        }

        impl From<$name> for Variant<'static> {
            fn from(value: $name) -> Self {
                Variant::from(value.as_str())
            }
        }

        impl From<$name> for OwnedVariant {
            fn from(value: $name) -> Self {
                OwnedVariant::from(Str::from_static(value.as_str()))
            }
        }

        impl TryFrom<&Variant<'_>> for $name {
            type Error = ValueError;

            // `ValueError` is spelled out: `Self::Error` would be ambiguous with `State::Error`.
            fn try_from(value: &Variant<'_>) -> Result<Self, ValueError> {
                match value {
                    Variant::Str(text) => Ok(text.as_str().parse()?),
                    other => Err(ValueError::wrong_type(
                        stringify!($name),
                        <Self as Type>::SIGNATURE,
                        other,
                    )),
                }
            }
        }

        impl TryFrom<OwnedVariant> for $name {
            type Error = ValueError;

            fn try_from(value: OwnedVariant) -> Result<Self, ValueError> {
                Self::try_from(&*value)
            }
        }
    };
}

string_enum! {
    pub enum State {
        UpToDate = "up-to-date",
        Available = "available",
        /// Any operation is running; see the `Progress` property.
        Downloading = "downloading",
        /// Locked: `Apply` makes it the next boot.
        Staged = "staged",
        ReadyForReboot = "ready-for-reboot",
        RollbackQueued = "rollback-queued",
        Incompatible = "incompatible",
        UsrOverlay = "usr-overlay",
        FinalizeFailed = "finalize-failed",
        HealthFailure = "health-failure",
        /// See the `LastError` property.
        Error = "error",
        Unknown = "unknown",
    }
}

string_enum! {
    pub enum Channel {
        /// Signed with the release key.
        Stable = "stable",
        /// Signed with the CI key.
        Testing = "testing",
    }
}

string_enum! {
    pub enum BootVerdict {
        Green = "green",
        /// An armed boot failed during a user session: the daemon publishes `HealthFailure`
        /// and never reboots.
        Red = "red",
        /// Report only.
        RedUnarmed = "red-unarmed",
        /// GRUB fell back; the daemon denylists the failed target.
        Fallback = "fallback",
    }
}

string_enum! {
    pub enum ChangeKind {
        Added = "added",
        Removed = "removed",
        Upgraded = "upgraded",
        Downgraded = "downgraded",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn all_strings() -> Vec<(&'static str, &'static str)> {
        let mut all = Vec::new();
        all.extend(State::ALL.iter().map(|v| ("State", v.as_str())));
        all.extend(Channel::ALL.iter().map(|v| ("Channel", v.as_str())));
        all.extend(BootVerdict::ALL.iter().map(|v| ("BootVerdict", v.as_str())));
        all.extend(ChangeKind::ALL.iter().map(|v| ("ChangeKind", v.as_str())));
        all
    }

    fn parses(what: &str, text: &str) -> bool {
        match what {
            "State" => text.parse::<State>().is_ok(),
            "Channel" => text.parse::<Channel>().is_ok(),
            "BootVerdict" => text.parse::<BootVerdict>().is_ok(),
            "ChangeKind" => text.parse::<ChangeKind>().is_ok(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn contract_strings_round_trip() {
        for state in State::ALL {
            assert_eq!(state.as_str().parse::<State>(), Ok(*state));
            assert_eq!(state.to_string(), state.as_str());
            let owned = OwnedVariant::from(*state);
            assert_eq!(State::try_from(owned), Ok(*state));
            assert_eq!(State::try_from(&Variant::from(*state)), Ok(*state));
        }
        for channel in Channel::ALL {
            assert_eq!(channel.as_str().parse::<Channel>(), Ok(*channel));
        }
        for verdict in BootVerdict::ALL {
            assert_eq!(verdict.as_str().parse::<BootVerdict>(), Ok(*verdict));
        }
        for kind in ChangeKind::ALL {
            assert_eq!(kind.as_str().parse::<ChangeKind>(), Ok(*kind));
        }
    }

    #[test]
    fn strings_are_unique_per_enum() {
        let all = all_strings();
        for (i, (what, text)) in all.iter().enumerate() {
            assert!(
                !all[i + 1..].contains(&(*what, *text)),
                "{what} lists {text} twice"
            );
        }
    }

    #[test]
    fn parsing_is_exact() {
        for text in [
            "Up-To-Date",
            " staged",
            "staged ",
            "",
            "ready_for_reboot",
            "STABLE",
        ] {
            assert!(text.parse::<State>().is_err(), "{text:?}");
            assert!(text.parse::<Channel>().is_err(), "{text:?}");
        }
        let error = "red_unarmed".parse::<BootVerdict>().unwrap_err();
        assert_eq!(error.what(), "BootVerdict");
        assert_eq!(error.value(), "red_unarmed");
        assert_eq!(
            error.to_string(),
            "\"red_unarmed\" is not a valid BootVerdict"
        );
    }

    #[test]
    fn wrong_dbus_type_is_reported() {
        let error = State::try_from(OwnedVariant::from(7_u32)).unwrap_err();
        assert_eq!(error.to_string(), "State: expected D-Bus type s, found u");
        let error = Channel::try_from(OwnedVariant::from(Str::from_static("beta"))).unwrap_err();
        assert!(matches!(error, ValueError::Unknown(_)));
        let zbus_error = zbus::Error::from(error);
        assert!(
            zbus_error
                .to_string()
                .contains("\"beta\" is not a valid Channel")
        );
    }

    #[test]
    fn signature_is_string() {
        assert_eq!(State::SIGNATURE.to_string(), "s");
        assert_eq!(Channel::SIGNATURE.to_string(), "s");
        assert_eq!(BootVerdict::SIGNATURE.to_string(), "s");
        assert_eq!(<Change as Type>::SIGNATURE.to_string(), "(sssss)");
        assert_eq!(<Vec<Change> as Type>::SIGNATURE.to_string(), "a(sssss)");
        assert_eq!(CHANGE_FIELDS.len(), 5, "one name per tuple field");
        assert_eq!(<Vec<Snapshot> as Type>::SIGNATURE.to_string(), "a(sut)");
        assert_eq!(SNAPSHOT_FIELDS.len(), 3, "one name per tuple field");
    }

    #[test]
    fn serde_uses_contract_strings() {
        let ctxt = zbus::wire::serialized::Context::new(zbus::wire::LE, 0);
        let encoded = zbus::wire::to_bytes(ctxt, &State::ReadyForReboot).unwrap();
        let (text, _): (String, _) = encoded.deserialize().unwrap();
        assert_eq!(text, "ready-for-reboot");
        let (state, _): (State, _) = encoded.deserialize().unwrap();
        assert_eq!(state, State::ReadyForReboot);
        let bad = zbus::wire::to_bytes(ctxt, &"Staged").unwrap();
        assert!(bad.deserialize::<State>().is_err());
    }

    fn candidate() -> impl Strategy<Value = String> {
        prop_oneof![
            proptest::sample::select(all_strings()).prop_map(|(_, text)| text.to_owned()),
            "[a-z-]{0,20}",
            "\\PC{0,20}",
        ]
    }

    proptest! {
        #[test]
        fn from_str_accepts_only_contract_strings(text in candidate()) {
            for what in ["State", "Channel", "BootVerdict", "ChangeKind"] {
                let listed = all_strings().contains(&(what, text.as_str()));
                prop_assert_eq!(parses(what, &text), listed);
            }
        }

        #[test]
        fn near_misses_are_rejected(
            index in 0_usize..64,
            position in 0_usize..32,
            c in any::<char>(),
        ) {
            let all = all_strings();
            let (what, text) = all[index % all.len()];
            let mut chars: Vec<char> = text.chars().collect();
            let position = position % chars.len();
            prop_assume!(chars[position] != c);
            chars[position] = c;
            let mutated: String = chars.into_iter().collect();
            prop_assume!(!all.contains(&(what, mutated.as_str())));
            prop_assert!(!parses(what, &mutated));
        }
    }
}
