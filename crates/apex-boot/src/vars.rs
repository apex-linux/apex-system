//! The grubenv variables Apex owns, and the operations that arm and disarm the boot counter.
//!
//! The names are Apex's own: Fedora's `grub2-set-bootflag` sets `boot_success=1` after a
//! login and greenboot uses `boot_counter`, so sharing either would let a login confirm a
//! failed armed boot.

use std::fmt;

use crate::digest::{Digest, DigestError};
use crate::grubenv::{EditError, GrubEnv, NotUtf8};

/// The value `Apply` writes to `apex_counter`.
pub const BOOT_ATTEMPTS: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[expect(
    clippy::enum_variant_names,
    reason = "each variant is named after its grubenv variable (apex_target, …)"
)]
pub enum BootVar {
    ApexTarget,
    ApexCounter,
    ApexOk,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValueError {
    #[error("{name:?} is not an Apex grubenv variable")]
    UnknownVariable { name: String },
    #[error("{value:?} is not a valid value for {var}: expected {expected}")]
    InvalidValue {
        var: BootVar,
        value: String,
        expected: &'static str,
    },
}

impl BootVar {
    pub const ALL: [BootVar; 3] = [BootVar::ApexTarget, BootVar::ApexCounter, BootVar::ApexOk];

    pub const fn name(self) -> &'static str {
        match self {
            BootVar::ApexTarget => "apex_target",
            BootVar::ApexCounter => "apex_counter",
            BootVar::ApexOk => "apex_ok",
        }
    }

    pub fn from_name(name: &str) -> Result<Self, ValueError> {
        Self::ALL
            .into_iter()
            .find(|var| var.name() == name)
            .ok_or_else(|| ValueError::UnknownVariable {
                name: name.to_owned(),
            })
    }

    /// The production writer refuses every other value, so no other text ever reaches the
    /// `grub2-editenv` command line.
    pub fn check_value(self, value: &str) -> Result<(), ValueError> {
        let (valid, expected) = match self {
            BootVar::ApexTarget => (Digest::parse(value).is_ok(), "an image digest"),
            BootVar::ApexCounter => (
                parse_canonical_counter(value).is_some(),
                "a decimal integer",
            ),
            BootVar::ApexOk => (matches!(value, "0" | "1"), "0 or 1"),
        };
        if valid {
            Ok(())
        } else {
            Err(ValueError::InvalidValue {
                var: self,
                value: value.to_owned(),
                expected,
            })
        }
    }
}

impl fmt::Display for BootVar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvOp {
    Set(BootVar, String),
    Unset(BootVar),
}

impl EnvOp {
    pub fn var(&self) -> BootVar {
        match self {
            EnvOp::Set(var, _) | EnvOp::Unset(var) => *var,
        }
    }

    pub fn apply_to(&self, env: &mut GrubEnv) -> Result<(), EditError> {
        match self {
            EnvOp::Set(var, value) => env.set(var.name(), value),
            EnvOp::Unset(var) => env.unset(var.name()),
        }
    }
}

impl fmt::Display for EnvOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvOp::Set(var, value) => write!(f, "set {var}={value}"),
            EnvOp::Unset(var) => write!(f, "unset {var}"),
        }
    }
}

/// All `Set`s, so [`crate::writer::apply_ops`] writes them in one `grub2-editenv` call and
/// GRUB never sees a half-armed block.
///
/// The caller must first pass [`check_arming`], and must wait until this boot's assessment
/// has finished, which could otherwise clear the fresh arming.
pub fn arm_ops(target: &Digest) -> Vec<EnvOp> {
    vec![
        EnvOp::Set(BootVar::ApexTarget, target.as_str().to_owned()),
        EnvOp::Set(BootVar::ApexCounter, BOOT_ATTEMPTS.to_string()),
        EnvOp::Set(BootVar::ApexOk, "0".to_owned()),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArmingRefused {
    #[error("the downloaded update is the version that is already running")]
    StagedIsBooted,
    #[error(
        "the running version {booted} has not passed its startup checks yet; \
         installing another update now would remove the last version known to work"
    )]
    BootedUnconfirmed { booted: Digest },
    #[error(
        "the running version {booted} failed its startup checks; roll back first, so a \
         version known to work stays installed"
    )]
    BootedFailed { booted: Digest },
}

/// Finalizing the stage keeps only the new deployment and the booted one, so arming on top
/// of an unconfirmed or failed booted deployment would leave it as the only fallback. The
/// caller also checks that the booted deployment comes from an image.
pub fn check_arming(
    vars: &BootVars,
    booted: &Digest,
    staged: &Digest,
    health_failure: Option<&Digest>,
) -> Result<(), ArmingRefused> {
    if staged == booted {
        return Err(ArmingRefused::StagedIsBooted);
    }
    if health_failure == Some(booted) {
        return Err(ArmingRefused::BootedFailed {
            booted: booted.clone(),
        });
    }
    if vars.apex_target.as_ref() == Some(booted) {
        return Err(ArmingRefused::BootedUnconfirmed {
            booted: booted.clone(),
        });
    }
    Ok(())
}

/// Leaves `apex_ok` alone: GRUB resets it on every boot and ignores it while disarmed.
pub fn disarm_ops() -> Vec<EnvOp> {
    vec![
        EnvOp::Unset(BootVar::ApexTarget),
        EnvOp::Unset(BootVar::ApexCounter),
    ]
}

/// Empty values count as unset, because the GRUB snippet tests them with `-n`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[expect(
    clippy::struct_field_names,
    reason = "each field is named after its grubenv variable (apex_target, …)"
)]
pub struct BootVars {
    pub apex_target: Option<Digest>,
    pub apex_counter: Option<i32>,
    /// `Some(true)` for exactly `1`, because the snippet only compares against `1`.
    pub apex_ok: Option<bool>,
}

/// Only Apex writes these variables, so any of these means corruption or a foreign writer:
/// callers change nothing and leave GRUB to its own rules.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BootVarsError {
    #[error(transparent)]
    NotUtf8(#[from] NotUtf8),
    #[error("apex_target is not a valid digest: {value:?}")]
    InvalidTarget {
        value: String,
        #[source]
        source: DigestError,
    },
    #[error("apex_counter is not a canonical integer: {value:?}")]
    InvalidCounter { value: String },
}

/// What GRUB did with `apex_counter`, as `data/08_apex-bootcount.cfg` counts it down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterState {
    Unset,
    /// Still [`BOOT_ATTEMPTS`]: armed after GRUB ran, or GRUB did not count this boot.
    NotYetCounted,
    Counted {
        remaining: u8,
    },
    /// GRUB booted entry 1, the previous deployment.
    Exhausted,
}

impl CounterState {
    pub fn from_counter(counter: Option<i32>) -> Self {
        match counter {
            None => Self::Unset,
            Some(BOOT_ATTEMPTS) => Self::NotYetCounted,
            Some(n @ 0..=2) => Self::Counted {
                remaining: n.unsigned_abs() as u8,
            },
            Some(_) => Self::Exhausted,
        }
    }

    pub fn is_exhausted(self) -> bool {
        self == Self::Exhausted
    }

    /// Only a boot GRUB demonstrably counted proves that the snippet and `save_env` work;
    /// rebooting in any other state could repeat forever.
    pub fn reboot_makes_progress(self) -> bool {
        matches!(self, Self::Counted { .. })
    }
}

impl BootVars {
    pub fn from_env(env: &GrubEnv) -> Result<Self, BootVarsError> {
        let apex_target = match non_empty(env.get_str(BootVar::ApexTarget.name())?) {
            None => None,
            Some(value) => {
                Some(
                    Digest::parse(value).map_err(|source| BootVarsError::InvalidTarget {
                        value: value.to_owned(),
                        source,
                    })?,
                )
            }
        };
        let apex_counter = match non_empty(env.get_str(BootVar::ApexCounter.name())?) {
            None => None,
            Some(value) => Some(parse_canonical_counter(value).ok_or_else(|| {
                BootVarsError::InvalidCounter {
                    value: value.to_owned(),
                }
            })?),
        };
        let apex_ok = env
            .get_str(BootVar::ApexOk.name())?
            .map(|value| value == "1");
        Ok(Self {
            apex_target,
            apex_counter,
            apex_ok,
        })
    }

    pub fn is_armed(&self) -> bool {
        self.apex_target.is_some()
    }

    pub fn counter_state(&self) -> CounterState {
        CounterState::from_counter(self.apex_counter)
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// GRUB compares strings, so `03` or `+3` would not mean 3 to it.
fn parse_canonical_counter(value: &str) -> Option<i32> {
    value.parse::<i32>().ok().filter(|n| n.to_string() == value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    fn env_with(vars: &[(&str, &str)]) -> GrubEnv {
        let mut env = GrubEnv::new();
        for (name, value) in vars {
            env.set(name, value).unwrap();
        }
        env
    }

    #[test]
    fn names_match_the_contract() {
        assert_eq!(BootVar::ApexTarget.name(), "apex_target");
        assert_eq!(BootVar::ApexCounter.name(), "apex_counter");
        assert_eq!(BootVar::ApexOk.name(), "apex_ok");
        assert!(
            BootVar::ALL
                .iter()
                .all(|v| crate::grubenv::is_valid_name(v.name()))
        );
        for foreign in ["boot_success", "boot_counter", "boot_indeterminate"] {
            assert!(
                BootVar::ALL.iter().all(|v| v.name() != foreign),
                "{foreign}"
            );
        }
    }

    #[test]
    fn only_apex_variables_have_names() {
        for var in BootVar::ALL {
            assert_eq!(BootVar::from_name(var.name()), Ok(var));
        }
        for other in [
            "boot_success",
            "boot_counter",
            "APEX_OK",
            "apex_ok ",
            "",
            "-v",
        ] {
            assert_eq!(
                BootVar::from_name(other),
                Err(ValueError::UnknownVariable {
                    name: other.to_owned()
                })
            );
        }
    }

    #[test]
    fn values_are_digests_and_integers_only() {
        BootVar::ApexTarget.check_value(TARGET).unwrap();
        BootVar::ApexCounter.check_value("3").unwrap();
        BootVar::ApexCounter.check_value("-1").unwrap();
        BootVar::ApexCounter.check_value("0").unwrap();
        BootVar::ApexOk.check_value("0").unwrap();
        BootVar::ApexOk.check_value("1").unwrap();
        let rejected = [
            (BootVar::ApexTarget, "sha256:abc"),
            (BootVar::ApexTarget, "latest"),
            (BootVar::ApexTarget, "\"; reboot; \""),
            (BootVar::ApexCounter, "03"),
            (BootVar::ApexCounter, "3 "),
            (BootVar::ApexCounter, "$(reboot)"),
            (BootVar::ApexCounter, ""),
            (BootVar::ApexOk, "2"),
            (BootVar::ApexOk, "yes"),
            (BootVar::ApexOk, ""),
        ];
        for (var, value) in rejected {
            assert!(
                matches!(
                    var.check_value(value),
                    Err(ValueError::InvalidValue { var: v, .. }) if v == var
                ),
                "{var}={value:?}"
            );
        }
        for op in arm_ops(&Digest::parse(TARGET).unwrap()) {
            if let EnvOp::Set(var, value) = op {
                var.check_value(&value).unwrap();
            }
        }
    }

    #[test]
    fn arm_and_disarm_ops() {
        let target = Digest::parse(TARGET).unwrap();
        assert_eq!(
            arm_ops(&target),
            vec![
                EnvOp::Set(BootVar::ApexTarget, TARGET.to_owned()),
                EnvOp::Set(BootVar::ApexCounter, "3".to_owned()),
                EnvOp::Set(BootVar::ApexOk, "0".to_owned()),
            ]
        );
        let mut env = GrubEnv::new();
        for op in arm_ops(&target) {
            op.apply_to(&mut env).unwrap();
        }
        let vars = BootVars::from_env(&env).unwrap();
        assert_eq!(
            vars,
            BootVars {
                apex_target: Some(target),
                apex_counter: Some(3),
                apex_ok: Some(false),
            }
        );
        assert_eq!(vars.counter_state(), CounterState::NotYetCounted);
        for op in disarm_ops() {
            op.apply_to(&mut env).unwrap();
        }
        let vars = BootVars::from_env(&env).unwrap();
        assert!(!vars.is_armed());
        assert_eq!(vars.apex_counter, None);
        assert_eq!(vars.apex_ok, Some(false));
    }

    #[test]
    fn arming_needs_a_confirmed_running_version() {
        let t1 = Digest::parse(TARGET).unwrap();
        let t2 = Digest::parse(
            "sha256:3333333333333333333333333333333333333333333333333333333333333333",
        )
        .unwrap();
        let p = Digest::parse(
            "sha256:4444444444444444444444444444444444444444444444444444444444444444",
        )
        .unwrap();
        let unarmed = BootVars::default();
        assert_eq!(check_arming(&unarmed, &p, &t1, None), Ok(()));
        assert_eq!(
            check_arming(&unarmed, &p, &p, None),
            Err(ArmingRefused::StagedIsBooted)
        );
        let armed_for_t1 = BootVars {
            apex_target: Some(t1.clone()),
            apex_counter: Some(2),
            apex_ok: Some(false),
        };
        assert_eq!(
            check_arming(&armed_for_t1, &t1, &t2, None),
            Err(ArmingRefused::BootedUnconfirmed { booted: t1.clone() })
        );
        assert_eq!(check_arming(&armed_for_t1, &p, &t2, None), Ok(()));
        assert_eq!(
            check_arming(&unarmed, &t1, &t2, Some(&t1)),
            Err(ArmingRefused::BootedFailed { booted: t1.clone() })
        );
        assert_eq!(check_arming(&unarmed, &t1, &t2, Some(&p)), Ok(()));
        assert!(
            ArmingRefused::BootedUnconfirmed { booted: t1 }
                .to_string()
                .contains("last version known to work")
        );
    }

    #[test]
    fn empty_values_are_unset() {
        let vars = BootVars::from_env(&env_with(&[
            ("apex_target", ""),
            ("apex_counter", ""),
            ("apex_ok", ""),
        ]))
        .unwrap();
        assert_eq!(
            vars,
            BootVars {
                apex_target: None,
                apex_counter: None,
                apex_ok: Some(false),
            }
        );
        assert_eq!(
            BootVars::from_env(&GrubEnv::new()).unwrap(),
            BootVars::default()
        );
    }

    #[test]
    fn apex_ok_is_true_only_for_exactly_one() {
        for (value, expected) in [("1", true), ("0", false), ("yes", false), ("01", false)] {
            let vars = BootVars::from_env(&env_with(&[("apex_ok", value)])).unwrap();
            assert_eq!(vars.apex_ok, Some(expected), "{value}");
        }
    }

    #[test]
    fn invalid_values_are_errors() {
        assert!(matches!(
            BootVars::from_env(&env_with(&[("apex_target", "sha256:abc")])),
            Err(BootVarsError::InvalidTarget { .. })
        ));
        for bad in ["03", "+3", "3 ", "three", "99999999999", "-0"] {
            assert_eq!(
                BootVars::from_env(&env_with(&[("apex_counter", bad)])),
                Err(BootVarsError::InvalidCounter {
                    value: bad.to_owned()
                }),
                "{bad:?}"
            );
        }
        let mut raw = crate::grubenv::SIGNATURE.to_vec();
        raw.extend_from_slice(b"apex_counter=\xff\n");
        raw.resize(64, b'#');
        assert!(matches!(
            BootVars::from_env(&GrubEnv::parse(&raw).unwrap()),
            Err(BootVarsError::NotUtf8(_))
        ));
    }

    #[test]
    fn counter_states_follow_the_snippet() {
        assert_eq!(CounterState::from_counter(None), CounterState::Unset);
        assert_eq!(
            CounterState::from_counter(Some(3)),
            CounterState::NotYetCounted
        );
        for n in 0..=2u8 {
            assert_eq!(
                CounterState::from_counter(Some(i32::from(n))),
                CounterState::Counted { remaining: n }
            );
        }
        for n in [-1, -2, 4, 7, i32::MIN, i32::MAX] {
            assert_eq!(CounterState::from_counter(Some(n)), CounterState::Exhausted);
        }
        assert!(CounterState::Exhausted.is_exhausted());
        assert!(!CounterState::Counted { remaining: 0 }.is_exhausted());
        assert!(CounterState::Counted { remaining: 0 }.reboot_makes_progress());
        assert!(!CounterState::NotYetCounted.reboot_makes_progress());
        assert!(!CounterState::Unset.reboot_makes_progress());
        assert!(!CounterState::Exhausted.reboot_makes_progress());
    }

    #[test]
    fn env_op_display() {
        assert_eq!(
            EnvOp::Set(BootVar::ApexOk, "1".into()).to_string(),
            "set apex_ok=1"
        );
        assert_eq!(
            EnvOp::Unset(BootVar::ApexTarget).to_string(),
            "unset apex_target"
        );
    }
}
