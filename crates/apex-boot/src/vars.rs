//! The three grubenv variables Apex owns (M1 contract §5), their meaning, and the operations
//! that arm and disarm the boot counter.
//!
//! The names are Apex's own on purpose. Fedora's `grub2-set-bootflag` (run by
//! `grub-boot-success.timer` after a login, and by GNOME on a user-initiated restart) sets
//! `boot_success=1`, and greenboot uses `boot_counter`. Sharing either name would let a
//! login mark a failed armed boot as good, or let a foreign counter move the default entry.

use std::fmt;

use crate::digest::{Digest, DigestError};
use crate::grubenv::{EditError, GrubEnv, NotUtf8};

/// Boots a newly applied deployment gets before GRUB falls back: the value `Apply` writes
/// to `apex_counter`.
pub const BOOT_ATTEMPTS: i32 = 3;

/// A grubenv variable owned by Apex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[expect(
    clippy::enum_variant_names,
    reason = "each variant is named after its grubenv variable (apex_target, …)"
)]
pub enum BootVar {
    /// `apex_target`: digest of the deployment that must prove itself. Set by `apex-updated`
    /// on `Apply`.
    ApexTarget,
    /// `apex_counter`: attempts left. Set to 3 on `Apply`, decremented by the GRUB snippet.
    ApexCounter,
    /// `apex_ok`: 1 once `apex-boot-assess` judged the boot green; reset to 0 by the
    /// GRUB snippet on every boot.
    ApexOk,
}

/// Why a value may not be written to an Apex variable (see [`BootVar::check_value`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValueError {
    /// The name is not one of the Apex variables.
    #[error("{name:?} is not an Apex grubenv variable")]
    UnknownVariable {
        /// The rejected name.
        name: String,
    },
    /// The value is not what the variable holds.
    #[error("{value:?} is not a valid value for {var}: expected {expected}")]
    InvalidValue {
        /// The variable.
        var: BootVar,
        /// The rejected value.
        value: String,
        /// What the variable holds, for the message.
        expected: &'static str,
    },
}

impl BootVar {
    /// Every Apex variable.
    pub const ALL: [BootVar; 3] = [BootVar::ApexTarget, BootVar::ApexCounter, BootVar::ApexOk];

    /// The variable's name in grubenv.
    pub const fn name(self) -> &'static str {
        match self {
            BootVar::ApexTarget => "apex_target",
            BootVar::ApexCounter => "apex_counter",
            BootVar::ApexOk => "apex_ok",
        }
    }

    /// The variable called `name`, if it is one of Apex's.
    pub fn from_name(name: &str) -> Result<Self, ValueError> {
        Self::ALL
            .into_iter()
            .find(|var| var.name() == name)
            .ok_or_else(|| ValueError::UnknownVariable {
                name: name.to_owned(),
            })
    }

    /// Checks that `value` is something Apex writes to this variable: a digest for
    /// `apex_target` (`sha256:` and 64 lowercase hex digits), a canonical decimal `i32` for
    /// `apex_counter`, and `0` or `1` for `apex_ok`.
    ///
    /// The production writer refuses everything else, so no other text ever reaches the
    /// `grub2-editenv` command line.
    pub fn check_value(self, value: &str) -> Result<(), ValueError> {
        let (valid, expected) = match self {
            BootVar::ApexTarget => (Digest::parse(value).is_ok(), "an image digest"),
            BootVar::ApexCounter => (parse_counter(value).is_some(), "a decimal integer"),
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

/// One grubenv edit, as `grub2-editenv … set name=value` or `… unset name` performs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvOp {
    /// Set the variable to the value.
    Set(BootVar, String),
    /// Remove the variable.
    Unset(BootVar),
}

impl EnvOp {
    /// The variable this operation touches.
    pub fn var(&self) -> BootVar {
        match self {
            EnvOp::Set(var, _) | EnvOp::Unset(var) => *var,
        }
    }

    /// Applies the operation to an in-memory block with GRUB's semantics.
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

/// The operations `apex-updated` runs on `Apply` to arm the counter for `target`:
/// `apex_target=<digest> apex_counter=3 apex_ok=0`.
///
/// All three are `Set`s, so [`crate::writer::apply_ops`] writes them with one
/// `grub2-editenv set` call: GRUB never sees a half-armed block.
///
/// Preconditions that are the caller's job (ADR 0009, M1 contract §5):
///
/// - The booted deployment comes from an image and a different image is staged. When the
///   staged deployment is finalized at shutdown, ostree makes it entry 0 and keeps the
///   booted deployment as entry 1, so entry 1 is a real deployment when the armed target
///   boots, never the firmware-setup entry.
/// - The booted deployment is known good: [`check_arming`]. Finalization keeps only the new
///   deployment, the booted (merge) one and pinned ones; an older rollback deployment is
///   pruned. Entry 1, where GRUB falls back to, is therefore always the booted deployment,
///   and the last good one is gone if the booted one is itself an unconfirmed or failed
///   update.
/// - The boot assessment of the current boot has finished (see [`mod@crate::decide`]), so
///   it cannot clear the fresh arming.
pub fn arm_ops(target: &Digest) -> Vec<EnvOp> {
    vec![
        EnvOp::Set(BootVar::ApexTarget, target.as_str().to_owned()),
        EnvOp::Set(BootVar::ApexCounter, BOOT_ATTEMPTS.to_string()),
        EnvOp::Set(BootVar::ApexOk, "0".to_owned()),
    ]
}

/// Why `Apply` must not arm the counter now (see [`check_arming`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArmingRefused {
    /// The staged image is the running one: GRUB would fall back to the same image.
    #[error("the downloaded update is the version that is already running")]
    StagedIsBooted,
    /// grubenv is still armed for the running deployment: its own boot checks have not
    /// passed yet. Arming now would drop the last confirmed version when the stage is
    /// finalized, and a failing update would fall back to this unconfirmed one.
    #[error(
        "the running version {booted} has not passed its startup checks yet; \
         installing another update now would remove the last version known to work"
    )]
    BootedUnconfirmed {
        /// The running deployment.
        booted: Digest,
    },
    /// The running deployment failed its boot checks (`health-failure.json` names it).
    #[error(
        "the running version {booted} failed its startup checks; roll back first, so a \
         version known to work stays installed"
    )]
    BootedFailed {
        /// The running deployment.
        booted: Digest,
    },
}

/// The part of `Apply`'s arming precondition that only grubenv and the health record can
/// answer (ADR 0009, M1 contract §5). The caller also checks that the booted deployment
/// comes from an image.
///
/// - `staged` must differ from `booted`.
/// - grubenv must not be armed for `booted` (`vars`, read from grubenv just before): an
///   armed boot is confirmed only by a green assessment, which clears the arming. A
///   pending red boot (`HealthFailure`) keeps its arming too.
/// - `health_failure`, the digest in `health-failure.json` if there is one, must not be
///   `booted`.
///
/// Why: finalizing the stage keeps only the new deployment and the booted one
/// ([`arm_ops`]). Arming on top of an unconfirmed or failed deployment would make that
/// deployment the only fallback, with the last good one pruned.
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

/// The operations that remove an arming: `unset apex_target apex_counter` (one
/// `grub2-editenv unset` call). Used by `DiscardStaged`, and by the decision table whenever
/// it clears the variables. `apex_ok` is left alone: GRUB resets it on every boot and
/// ignores it while the counter is disarmed.
pub fn disarm_ops() -> Vec<EnvOp> {
    vec![
        EnvOp::Unset(BootVar::ApexTarget),
        EnvOp::Unset(BootVar::ApexCounter),
    ]
}

/// The Apex variables as GRUB loaded them.
///
/// Empty values count as unset, because the GRUB snippet tests them with `-n`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[expect(
    clippy::struct_field_names,
    reason = "each field is named after its grubenv variable (apex_target, …)"
)]
pub struct BootVars {
    /// `apex_target`: the deployment being tried, if the counter is armed.
    pub apex_target: Option<Digest>,
    /// `apex_counter`, if set.
    pub apex_counter: Option<i32>,
    /// `apex_ok`: `Some(true)` for exactly `1`; `Some(false)` for any other value,
    /// because the snippet only compares against `1`.
    pub apex_ok: Option<bool>,
}

/// Why the Apex variables in grubenv cannot be interpreted.
///
/// Only Apex writes these variables, so any of these means corruption or a foreign writer.
/// The safe reaction is to change nothing: GRUB keeps applying its own rules (a counter it
/// does not recognise makes it fall back to the previous deployment), and the failure is
/// visible as a failed `apex-boot-assess.service`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BootVarsError {
    /// A value is not UTF-8.
    #[error(transparent)]
    NotUtf8(#[from] NotUtf8),
    /// `apex_target` is set but is not a digest.
    #[error("apex_target is not a valid digest: {value:?}")]
    InvalidTarget {
        /// The stored value.
        value: String,
        /// Why it is not a digest.
        #[source]
        source: DigestError,
    },
    /// `apex_counter` is set but is not a canonical decimal `i32` (the snippet compares
    /// strings, so `03` or `+3` would not mean 3 to GRUB).
    #[error("apex_counter is not a canonical integer: {value:?}")]
    InvalidCounter {
        /// The stored value.
        value: String,
    },
}

/// What GRUB did with `apex_counter`, derived from the value userspace reads after boot.
///
/// The GRUB snippet (`data/08_apex-bootcount.cfg`, modelled by [`crate::snippet`]) acts on
/// an armed, unconfirmed boot like this: `3 → 2`, `2 → 1`, `1 → 0` and boot the default
/// entry; any other value → boot entry 1 and store `-1`. So a target gets exactly
/// [`BOOT_ATTEMPTS`] boots, and userspace reads 2, 1, 0 during them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterState {
    /// No counter: GRUB does not count.
    Unset,
    /// Still [`BOOT_ATTEMPTS`]: GRUB has not counted a boot since the counter was armed. On
    /// the current boot this means the arming happened after GRUB ran (an `Apply` during
    /// this boot), or GRUB did not count (`apex_ok` was 1, or the snippet or
    /// `save_env` does not work).
    NotYetCounted,
    /// GRUB counted this boot and booted the default entry; `remaining` more unconfirmed
    /// boots of it are allowed (0: the next one falls back).
    Counted {
        /// 2, 1 or 0.
        remaining: u8,
    },
    /// Counter exhausted: GRUB selected entry 1, the previous deployment. The snippet
    /// writes -1; every other value it does not count down from is treated the same way.
    Exhausted,
}

impl CounterState {
    /// Classifies a stored counter.
    pub fn from_counter(counter: Option<i32>) -> Self {
        match counter {
            None => Self::Unset,
            Some(BOOT_ATTEMPTS) => Self::NotYetCounted,
            Some(n @ 0..=2) => Self::Counted {
                // In 0..=2 by the pattern, so the conversion is lossless.
                remaining: n.unsigned_abs() as u8,
            },
            Some(_) => Self::Exhausted,
        }
    }

    /// Whether GRUB fell back to the previous deployment (the "counter exhausted" of the
    /// ADR 0009 table).
    pub fn is_exhausted(self) -> bool {
        self == Self::Exhausted
    }

    /// Whether a reboot is guaranteed to move the counter: GRUB demonstrably counted this
    /// boot, so the snippet and `save_env` work, and the next unconfirmed boot either uses
    /// another attempt or falls back. Rebooting in any other state could repeat forever.
    pub fn reboot_makes_progress(self) -> bool {
        matches!(self, Self::Counted { .. })
    }
}

impl BootVars {
    /// Reads the Apex variables from a parsed block (last line wins, as in GRUB).
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
            Some(value) => {
                Some(
                    parse_counter(value).ok_or_else(|| BootVarsError::InvalidCounter {
                        value: value.to_owned(),
                    })?,
                )
            }
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

    /// Whether the counter is armed for a target (`apex_target` is set).
    pub fn is_armed(&self) -> bool {
        self.apex_target.is_some()
    }

    /// What GRUB did with the counter.
    pub fn counter_state(&self) -> CounterState {
        CounterState::from_counter(self.apex_counter)
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// A decimal `i32` in the one spelling GRUB's string comparisons agree with.
fn parse_counter(value: &str) -> Option<i32> {
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
        // Fedora's grub2-set-bootflag and greenboot own these; Apex must never use them.
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
        // Everything the arming writes passes.
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

    /// Finding: `Apply` on top of an unconfirmed or failed deployment would leave only that
    /// deployment to fall back to, because finalization prunes the older one.
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
        // T1 runs and grubenv is still armed for it: not confirmed yet (or red).
        let armed_for_t1 = BootVars {
            apex_target: Some(t1.clone()),
            apex_counter: Some(2),
            apex_ok: Some(false),
        };
        assert_eq!(
            check_arming(&armed_for_t1, &t1, &t2, None),
            Err(ArmingRefused::BootedUnconfirmed { booted: t1.clone() })
        );
        // Armed for another target (P runs while T1 is still the default): P is fine.
        assert_eq!(check_arming(&armed_for_t1, &p, &t2, None), Ok(()));
        // A recorded health failure of the running version.
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
