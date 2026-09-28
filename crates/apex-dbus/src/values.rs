//! String-valued enums of the contract, the `GetChanges` entry type and conversion errors.

use std::{fmt, marker::PhantomData, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use zbus::zvariant::{OwnedValue, Signature, Str, Type, Value};

/// One entry of `GetChanges` (D-Bus `(sssss)`): `(name, arch, kind, from_evr, to_evr)`, the
/// fields [`CHANGE_FIELDS`] names.
///
/// - `arch` is the RPM architecture (`x86_64`, `i686`, `noarch`), so a multilib package
///   appears once per architecture.
/// - `kind` is one of the [`ChangeKind`] strings.
/// - D-Bus has no null string: `from_evr` is `""` for an added package and `to_evr` is `""`
///   for a removed one.
/// - The daemon sorts the list by `(name, arch)` in byte order.
pub type Change = (String, String, String, String, String);

/// The names of the [`Change`] fields in tuple order, as the contract XML spells them.
pub const CHANGE_FIELDS: [&str; 5] = ["name", "arch", "kind", "from_evr", "to_evr"];

/// A string that is not one of the values the contract allows.
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

    /// What was being parsed, e.g. `"State"`.
    pub fn what(&self) -> &'static str {
        self.what
    }

    /// The rejected string.
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// A D-Bus value that does not fit the contract type it was converted to.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValueError {
    /// The value has a different D-Bus type than the contract gives `field`.
    #[error("{field}: expected D-Bus type {expected}, found {found}")]
    WrongType {
        /// The property, dictionary key or type being converted.
        field: String,
        /// The contract signature.
        expected: &'static Signature,
        /// The signature of the value received.
        found: Signature,
    },
    /// A string outside the set the contract allows.
    #[error(transparent)]
    Unknown(#[from] UnknownValueError),
}

impl ValueError {
    pub(crate) fn wrong_type(
        field: impl Into<String>,
        expected: &'static Signature,
        found: &Value<'_>,
    ) -> Self {
        Self::WrongType {
            field: field.into(),
            expected,
            found: found.value_signature().clone(),
        }
    }
}

/// Lets proxy getters (`TryFrom<OwnedValue, Error = ValueError>`) report through zbus.
impl From<ValueError> for zbus::Error {
    fn from(error: ValueError) -> Self {
        zbus::Error::Variant(zbus::zvariant::Error::Message(error.to_string()))
    }
}

/// Deserializes a string enum from its contract string (works for D-Bus and JSON alike).
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

/// Defines an enum whose D-Bus form (signature `s`) is one of a fixed set of strings.
///
/// Serde and the zvariant conversions are written by hand through `as_str`/`FromStr` so the
/// wire form is exactly the contract string, whatever serde or zvariant do with enum variants.
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
            /// Every value, in contract order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The contract string.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value,)+
                }
            }
        }

        impl FromStr for $name {
            type Err = UnknownValueError;

            /// Parses exactly the contract strings (case-sensitive, no trimming).
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

        impl From<$name> for Value<'static> {
            fn from(value: $name) -> Self {
                Value::from(value.as_str())
            }
        }

        impl From<$name> for OwnedValue {
            fn from(value: $name) -> Self {
                OwnedValue::from(Str::from_static(value.as_str()))
            }
        }

        impl TryFrom<&Value<'_>> for $name {
            type Error = ValueError;

            // `ValueError` is spelled out: `Self::Error` would be ambiguous with `State::Error`.
            fn try_from(value: &Value<'_>) -> Result<Self, ValueError> {
                match value {
                    Value::Str(text) => Ok(text.as_str().parse()?),
                    other => Err(ValueError::wrong_type(
                        stringify!($name),
                        <Self as Type>::SIGNATURE,
                        other,
                    )),
                }
            }
        }

        impl TryFrom<OwnedValue> for $name {
            type Error = ValueError;

            fn try_from(value: OwnedValue) -> Result<Self, ValueError> {
                Self::try_from(&*value)
            }
        }
    };
}

string_enum! {
    /// The daemon's derived state (`State` property).
    pub enum State {
        /// The remote digest equals the booted or staged digest.
        UpToDate = "up-to-date",
        /// A newer, not denylisted image was found and is not staged yet.
        Available = "available",
        /// An operation is running; see the `Progress` property.
        Downloading = "downloading",
        /// A download-only (locked) stage exists; `Apply` makes it the next boot.
        Staged = "staged",
        /// The user applied the stage; it boots next.
        ReadyForReboot = "ready-for-reboot",
        /// bootc has a rollback queued; automatic operations stop.
        RollbackQueued = "rollback-queued",
        /// bootc reports the system as incompatible; automatic operations stop.
        Incompatible = "incompatible",
        /// A transient `/usr` overlay is active; automatic operations stop.
        UsrOverlay = "usr-overlay",
        /// Finalizing the last staged deployment failed.
        FinalizeFailed = "finalize-failed",
        /// The last boot of a newly applied digest failed its health checks.
        HealthFailure = "health-failure",
        /// The last operation failed; see the `LastError` property.
        Error = "error",
        /// The state could not be derived.
        Unknown = "unknown",
    }
}

string_enum! {
    /// The channel the booted deployment follows (`Channel` property, `SwitchChannel` argument).
    pub enum Channel {
        /// `ghcr.io/sekirokenjii/apex`, signed with the release key.
        Stable = "stable",
        /// `ghcr.io/sekirokenjii/apex-testing`, signed with the CI key.
        Testing = "testing",
    }
}

string_enum! {
    /// The outcome of a boot assessment (`ReportBoot` argument, sent by `apex-boot-assess`).
    pub enum BootVerdict {
        /// All required checks passed; the digest is the booted digest.
        Green = "green",
        /// An armed boot failed while a user session was active: the daemon publishes
        /// `HealthFailure` and waits for the user (no reboot).
        Red = "red",
        /// An ordinary (unarmed) boot failed a required check; report only.
        RedUnarmed = "red-unarmed",
        /// GRUB fell back to the previous deployment; the digest is the failed target, which
        /// the daemon denylists after making the running deployment the default.
        Fallback = "fallback",
    }
}

string_enum! {
    /// The `kind` field of a [`Change`].
    pub enum ChangeKind {
        /// The package exists only in the staged image.
        Added = "added",
        /// The package exists only in the booted image.
        Removed = "removed",
        /// The staged image has a newer EVR.
        Upgraded = "upgraded",
        /// The staged image has an older EVR.
        Downgraded = "downgraded",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Every (type name, contract string) pair of the four string enums.
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
            let owned = OwnedValue::from(*state);
            assert_eq!(State::try_from(owned), Ok(*state));
            assert_eq!(State::try_from(&Value::from(*state)), Ok(*state));
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
        let error = State::try_from(OwnedValue::from(7_u32)).unwrap_err();
        assert_eq!(error.to_string(), "State: expected D-Bus type s, found u");
        let error = Channel::try_from(OwnedValue::from(Str::from_static("beta"))).unwrap_err();
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
    }

    #[test]
    fn serde_uses_contract_strings() {
        // D-Bus encoding: the body is a plain string.
        let ctxt = zbus::zvariant::serialized::Context::new_dbus(zbus::zvariant::LE, 0);
        let encoded = zbus::zvariant::to_bytes(ctxt, &State::ReadyForReboot).unwrap();
        let (text, _): (String, _) = encoded.deserialize().unwrap();
        assert_eq!(text, "ready-for-reboot");
        let (state, _): (State, _) = encoded.deserialize().unwrap();
        assert_eq!(state, State::ReadyForReboot);
        let bad = zbus::zvariant::to_bytes(ctxt, &"Staged").unwrap();
        assert!(bad.deserialize::<State>().is_err());
    }

    /// Contract strings, look-alikes and arbitrary text.
    fn candidate() -> impl Strategy<Value = String> {
        prop_oneof![
            proptest::sample::select(all_strings()).prop_map(|(_, text)| text.to_owned()),
            "[a-z-]{0,20}",
            "\\PC{0,20}",
        ]
    }

    proptest! {
        /// `FromStr` accepts a string if and only if it is one of the contract strings.
        #[test]
        fn from_str_accepts_only_contract_strings(text in candidate()) {
            for what in ["State", "Channel", "BootVerdict", "ChangeKind"] {
                let listed = all_strings().contains(&(what, text.as_str()));
                prop_assert_eq!(parses(what, &text), listed);
            }
        }

        /// Near misses of real values (one character changed) never parse.
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
