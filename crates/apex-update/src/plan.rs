use crate::gates::{
    Deferral, Gate, Operation, Trigger, check_download_gates, check_download_resources,
    gate_boot_space, gate_bootc_flags, gate_denylist, gate_disk_headroom, gate_intent, gate_power,
};
use crate::snapshot::{RemoteView, SystemSnapshot};
use crate::version::Version;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomaticPlan {
    /// Not even a check.
    Skip(Deferral),
    /// No download; publish the deferral if the check finds an update.
    Check(Deferral),
    /// Then ask [`plan_after_check`].
    CheckThenDownload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterCheck {
    NothingToDo,
    Download,
    Defer(Deferral),
}

/// A failing resource gate still allows the check: it fetches only a manifest, and knowing
/// about the update lets the UI explain the deferral.
pub fn plan_automatic(snapshot: &SystemSnapshot) -> AutomaticPlan {
    if let Err(deferral) = gate_bootc_flags(&snapshot.host)
        .and_then(|()| gate_intent(snapshot.intent.as_ref(), &snapshot.host, Trigger::Automatic))
    {
        return AutomaticPlan::Skip(deferral);
    }
    match check_download_resources(snapshot, false) {
        Ok(()) => AutomaticPlan::CheckThenDownload,
        Err(deferral) => AutomaticPlan::Check(deferral),
    }
}

fn remote_neither_booted_nor_staged(snapshot: &SystemSnapshot) -> Option<&RemoteView> {
    let remote = snapshot.remote.as_ref()?;
    let host = &snapshot.host;
    let digest = Some(remote.digest.as_str());
    (digest != host.booted_digest() && digest != host.staged_digest()).then_some(remote)
}

pub fn plan_after_check(snapshot: &SystemSnapshot) -> AfterCheck {
    let Some(remote) = remote_neither_booted_nor_staged(snapshot) else {
        return AfterCheck::NothingToDo;
    };
    match check_download_gates(snapshot, remote, Trigger::Automatic, false) {
        Ok(()) => AfterCheck::Download,
        Err(deferral) => AfterCheck::Defer(deferral),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DownloadError {
    #[error("There is no update to download.")]
    NothingToDownload,
    #[error(transparent)]
    Gate(#[from] Deferral),
}

pub fn validate_download(
    snapshot: &SystemSnapshot,
    override_soft_gates: bool,
) -> Result<(), DownloadError> {
    let remote =
        remote_neither_booted_nor_staged(snapshot).ok_or(DownloadError::NothingToDownload)?;
    check_download_gates(snapshot, remote, Trigger::User, override_soft_gates)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error("There is no downloaded update to install.")]
    NothingStaged,
    #[error("The downloaded update changed since it was shown. Review it again.")]
    DigestMismatch { expected: String, staged: String },
    #[error("The update is already set to install at the next restart.")]
    AlreadyApplied,
    #[error("The downloaded version {staged} is not newer than the installed version {booted}.")]
    Downgrade { booted: Version, staged: Version },
    #[error(transparent)]
    Gate(#[from] Deferral),
}

/// `staged_manifest_version` comes from the staged deployment's own manifest: verified
/// content, not a registry label. An unknown booted version leaves nothing to compare.
pub fn validate_apply(
    snapshot: &SystemSnapshot,
    expected_digest: &str,
    staged_manifest_version: Option<&Version>,
) -> Result<(), ApplyError> {
    let host = &snapshot.host;
    let staged = host.staged.as_ref().ok_or(ApplyError::NothingStaged)?;
    if staged.digest != expected_digest {
        return Err(ApplyError::DigestMismatch {
            expected: expected_digest.to_owned(),
            staged: staged.digest.clone(),
        });
    }
    if !staged.download_only {
        return Err(ApplyError::AlreadyApplied);
    }
    gate_bootc_flags(host)?;
    gate_denylist(&snapshot.denylist, &staged.digest)?;
    let staged_version = staged_manifest_version.ok_or_else(|| {
        Deferral::new(
            Gate::Version,
            "The downloaded update has no version number, so it cannot be installed.",
        )
    })?;
    if let Some(booted) = host.booted_version()
        && staged_version <= booted
    {
        return Err(ApplyError::Downgrade {
            booted: booted.clone(),
            staged: staged_version.clone(),
        });
    }
    gate_boot_space(snapshot.boot_free, snapshot.boot_payload)?;
    gate_disk_headroom(snapshot.sysroot_free)?;
    // A soft gate, but `Apply` has no override.
    gate_power(snapshot.power, Operation::Apply)?;
    Ok(())
}

/// The daemon must clear an orphaned intent, or [`plan_automatic`] would skip for good.
pub fn intent_is_orphaned(snapshot: &SystemSnapshot) -> bool {
    let Some(intent) = &snapshot.intent else {
        return false;
    };
    let host = &snapshot.host;
    let staged_unlocked = host
        .staged
        .as_ref()
        .is_some_and(|s| !s.download_only && s.digest == intent.digest);
    let booted = host.booted_digest() == Some(intent.digest.as_str());
    !(staged_unlocked || booted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gates::GIB;
    use crate::snapshot::Power;
    use crate::test_support::{
        BOOTED, REMOTE, STAGED, deny, deployment, healthy, intent, ver, with_locked_stage,
    };

    type Case = (fn(&mut SystemSnapshot), Gate);

    fn gate_of_plan(plan: &AutomaticPlan) -> Option<Gate> {
        match plan {
            AutomaticPlan::Skip(d) | AutomaticPlan::Check(d) => Some(d.gate),
            AutomaticPlan::CheckThenDownload => None,
        }
    }

    fn deferred_by(after: &AfterCheck) -> Option<Gate> {
        match after {
            AfterCheck::Defer(d) => Some(d.gate),
            _ => None,
        }
    }

    fn on_battery(percent: u8) -> Power {
        Power {
            on_ac: false,
            battery_percent: Some(percent),
        }
    }

    #[test]
    fn automatic_checks_then_downloads_when_healthy() {
        assert_eq!(plan_automatic(&healthy()), AutomaticPlan::CheckThenDownload);
        assert_eq!(
            plan_automatic(&with_locked_stage()),
            AutomaticPlan::CheckThenDownload
        );
    }

    #[test]
    fn automatic_skips_on_each_bootc_flag() {
        for set in [
            |s: &mut SystemSnapshot| s.host.incompatible = true,
            |s: &mut SystemSnapshot| s.host.usr_overlay = true,
            |s: &mut SystemSnapshot| s.host.rollback_queued = true,
        ] {
            let mut s = healthy();
            set(&mut s);
            let plan = plan_automatic(&s);
            assert!(matches!(plan, AutomaticPlan::Skip(_)), "{plan:?}");
            assert_eq!(gate_of_plan(&plan), Some(Gate::BootcFlags));
        }
    }

    #[test]
    fn automatic_skips_while_an_intent_exists_or_a_stage_is_unlocked() {
        let mut s = healthy();
        s.intent = Some(intent(STAGED));
        let plan = plan_automatic(&s);
        assert!(matches!(plan, AutomaticPlan::Skip(_)));
        assert_eq!(gate_of_plan(&plan), Some(Gate::Intent));

        let mut s = healthy();
        s.host.staged = Some(deployment(STAGED, "45.20260924.1", false));
        assert_eq!(gate_of_plan(&plan_automatic(&s)), Some(Gate::Intent));
    }

    #[test]
    fn automatic_only_checks_when_a_resource_gate_fails() {
        let cases: [Case; 5] = [
            (|s| s.sysroot_free = GIB, Gate::DiskSpace),
            (|s| s.boot_free = 1, Gate::BootSpace),
            (|s| s.power = on_battery(49), Gate::Power),
            (|s| s.metered = Some(true), Gate::Metered),
            (|s| s.power = Power::default(), Gate::Power),
        ];
        for (set, gate) in cases {
            let mut s = healthy();
            set(&mut s);
            let plan = plan_automatic(&s);
            assert!(matches!(plan, AutomaticPlan::Check(_)), "{plan:?}");
            assert_eq!(gate_of_plan(&plan), Some(gate));
        }
    }

    #[test]
    fn automatic_skip_wins_over_check() {
        let mut s = healthy();
        s.metered = Some(true);
        s.intent = Some(intent(STAGED));
        assert!(matches!(plan_automatic(&s), AutomaticPlan::Skip(_)));
    }

    #[test]
    fn after_check_downloads_a_newer_image() {
        assert_eq!(plan_after_check(&healthy()), AfterCheck::Download);
        let mut s = healthy();
        s.host.staged = Some(deployment(STAGED, "45.20260922.1", true));
        assert_eq!(plan_after_check(&s), AfterCheck::Download);
    }

    #[test]
    fn after_check_nothing_to_do() {
        let mut s = healthy();
        s.remote = None;
        assert_eq!(plan_after_check(&s), AfterCheck::NothingToDo);

        let mut s = healthy();
        s.remote.as_mut().expect("remote").digest = BOOTED.into();
        assert_eq!(plan_after_check(&s), AfterCheck::NothingToDo);

        assert_eq!(
            plan_after_check(&with_locked_stage()),
            AfterCheck::NothingToDo
        );

        let mut s = healthy();
        s.remote = None;
        s.host.incompatible = true;
        assert_eq!(plan_after_check(&s), AfterCheck::NothingToDo);
    }

    #[test]
    fn after_check_defers_on_every_gate() {
        let cases: [Case; 11] = [
            (|s| s.host.incompatible = true, Gate::BootcFlags),
            (|s| s.host.usr_overlay = true, Gate::BootcFlags),
            (|s| s.host.rollback_queued = true, Gate::BootcFlags),
            (|s| s.intent = Some(intent("sha256:other")), Gate::Intent),
            (
                |s| s.denylist = [deny(REMOTE)].into_iter().collect(),
                Gate::Denylist,
            ),
            (
                |s| s.remote.as_mut().expect("remote").version = Some(ver("45.20260901.1")),
                Gate::Version,
            ),
            (
                |s| s.remote.as_mut().expect("remote").version = None,
                Gate::Version,
            ),
            (|s| s.sysroot_free = 0, Gate::DiskSpace),
            (|s| s.boot_free = 0, Gate::BootSpace),
            (|s| s.power = on_battery(10), Gate::Power),
            (|s| s.metered = Some(true), Gate::Metered),
        ];
        for (set, gate) in cases {
            let mut s = healthy();
            set(&mut s);
            assert_eq!(deferred_by(&plan_after_check(&s)), Some(gate), "{gate:?}");
        }
    }

    #[test]
    fn after_check_reports_hard_gates_before_soft_ones() {
        let mut s = healthy();
        s.power = on_battery(5);
        s.metered = Some(true);
        s.denylist = [deny(REMOTE)].into_iter().collect();
        assert_eq!(deferred_by(&plan_after_check(&s)), Some(Gate::Denylist));
        s.denylist = crate::records::Denylist::new();
        assert_eq!(deferred_by(&plan_after_check(&s)), Some(Gate::Power));
    }

    #[test]
    fn after_check_defers_automatic_when_booted_version_is_unknown() {
        let mut s = healthy();
        s.host.booted.as_mut().expect("booted").version = None;
        assert_eq!(deferred_by(&plan_after_check(&s)), Some(Gate::Version));
    }

    #[test]
    fn user_download_passes_when_healthy() {
        assert_eq!(validate_download(&healthy(), false), Ok(()));
    }

    #[test]
    fn user_download_needs_something_to_download() {
        let mut s = healthy();
        s.remote = None;
        assert_eq!(
            validate_download(&s, true),
            Err(DownloadError::NothingToDownload)
        );
        assert_eq!(
            validate_download(&with_locked_stage(), true),
            Err(DownloadError::NothingToDownload)
        );
    }

    #[test]
    fn user_override_skips_power_and_metered_only() {
        let mut s = healthy();
        s.power = on_battery(3);
        s.metered = Some(true);
        assert!(matches!(
            validate_download(&s, false),
            Err(DownloadError::Gate(Deferral {
                gate: Gate::Power,
                ..
            }))
        ));
        assert_eq!(validate_download(&s, true), Ok(()));

        s.power.on_ac = true;
        assert!(matches!(
            validate_download(&s, false),
            Err(DownloadError::Gate(Deferral {
                gate: Gate::Metered,
                ..
            }))
        ));

        s.sysroot_free = 0;
        assert!(matches!(
            validate_download(&s, true),
            Err(DownloadError::Gate(Deferral {
                gate: Gate::DiskSpace,
                ..
            }))
        ));
    }

    #[test]
    fn user_override_never_skips_hard_gates() {
        let cases: [Case; 5] = [
            (|s| s.host.usr_overlay = true, Gate::BootcFlags),
            (
                |s| s.host.staged = Some(deployment(STAGED, "45.20260922.1", false)),
                Gate::Intent,
            ),
            (
                |s| s.denylist = [deny(REMOTE)].into_iter().collect(),
                Gate::Denylist,
            ),
            (
                |s| s.remote.as_mut().expect("remote").version = Some(ver("45.20260920.1")),
                Gate::Version,
            ),
            (|s| s.boot_free = 0, Gate::BootSpace),
        ];
        for (set, gate) in cases {
            let mut s = healthy();
            set(&mut s);
            let err = validate_download(&s, true).expect_err("hard gate");
            assert!(
                matches!(&err, DownloadError::Gate(d) if d.gate == gate),
                "{err:?}"
            );
        }
    }

    #[test]
    fn user_download_allows_unknown_versions_and_a_leftover_intent() {
        let mut s = healthy();
        s.remote.as_mut().expect("remote").version = None;
        s.host.booted.as_mut().expect("booted").version = None;
        s.intent = Some(intent("sha256:gone"));
        assert_eq!(validate_download(&s, false), Ok(()));
    }

    fn apply_ready() -> SystemSnapshot {
        let mut s = with_locked_stage();
        s.power = on_battery(25);
        s
    }

    fn staged_version() -> Version {
        ver("45.20260924.1")
    }

    #[test]
    fn apply_passes_for_the_reviewed_newer_locked_stage() {
        assert_eq!(
            validate_apply(&apply_ready(), REMOTE, Some(&staged_version())),
            Ok(())
        );
    }

    #[test]
    fn apply_needs_a_stage() {
        assert_eq!(
            validate_apply(&healthy(), REMOTE, Some(&staged_version())),
            Err(ApplyError::NothingStaged)
        );
    }

    #[test]
    fn apply_refuses_a_different_digest() {
        assert_eq!(
            validate_apply(&apply_ready(), "sha256:reviewed", Some(&staged_version())),
            Err(ApplyError::DigestMismatch {
                expected: "sha256:reviewed".into(),
                staged: REMOTE.into(),
            })
        );
    }

    #[test]
    fn apply_refuses_an_unlocked_stage() {
        let mut s = apply_ready();
        s.host.staged.as_mut().expect("staged").download_only = false;
        assert_eq!(
            validate_apply(&s, REMOTE, Some(&staged_version())),
            Err(ApplyError::AlreadyApplied)
        );
    }

    fn apply_gate(s: &SystemSnapshot, version: Option<&Version>) -> Option<Gate> {
        match validate_apply(s, REMOTE, version) {
            Err(ApplyError::Gate(d)) => Some(d.gate),
            _ => None,
        }
    }

    #[test]
    fn apply_checks_flags_denylist_space_and_power() {
        let v = staged_version();
        let cases: [Case; 7] = [
            (|s| s.host.incompatible = true, Gate::BootcFlags),
            (|s| s.host.usr_overlay = true, Gate::BootcFlags),
            (|s| s.host.rollback_queued = true, Gate::BootcFlags),
            (
                |s| s.denylist = [deny(REMOTE)].into_iter().collect(),
                Gate::Denylist,
            ),
            (|s| s.boot_free = 0, Gate::BootSpace),
            (|s| s.sysroot_free = GIB, Gate::DiskSpace),
            (|s| s.power = on_battery(19), Gate::Power),
        ];
        for (set, gate) in cases {
            let mut s = apply_ready();
            set(&mut s);
            assert_eq!(apply_gate(&s, Some(&v)), Some(gate), "{gate:?}");
        }
    }

    #[test]
    fn apply_ignores_download_only_gates() {
        let mut s = apply_ready();
        s.metered = Some(true);
        s.image_size = Some(u64::MAX);
        s.intent = Some(intent("sha256:older-apply"));
        s.remote = None;
        assert_eq!(validate_apply(&s, REMOTE, Some(&staged_version())), Ok(()));
    }

    #[test]
    fn apply_refuses_a_stage_without_manifest_version() {
        let s = apply_ready();
        assert_eq!(apply_gate(&s, None), Some(Gate::Version));
    }

    #[test]
    fn apply_refuses_older_or_equal_versions() {
        let s = apply_ready();
        for staged in ["45.20260901.1", "45.20260920.1"] {
            assert_eq!(
                validate_apply(&s, REMOTE, Some(&ver(staged))),
                Err(ApplyError::Downgrade {
                    booted: ver("45.20260920.1"),
                    staged: ver(staged),
                })
            );
        }
    }

    #[test]
    fn apply_uses_the_manifest_version_not_the_label() {
        let s = apply_ready();
        assert!(matches!(
            validate_apply(&s, REMOTE, Some(&ver("45.20260919.9"))),
            Err(ApplyError::Downgrade { .. })
        ));
    }

    #[test]
    fn apply_accepts_an_unknown_booted_version() {
        let mut s = apply_ready();
        s.host.booted.as_mut().expect("booted").version = None;
        assert_eq!(validate_apply(&s, REMOTE, Some(&staged_version())), Ok(()));
        s.host.booted = None;
        assert_eq!(validate_apply(&s, REMOTE, Some(&staged_version())), Ok(()));
    }

    #[test]
    fn apply_does_not_require_a_rollback_deployment() {
        let mut s = apply_ready();
        s.host.rollback = None;
        assert_eq!(validate_apply(&s, REMOTE, Some(&staged_version())), Ok(()));
    }

    #[test]
    fn apply_errors_read_well() {
        assert_eq!(
            ApplyError::Downgrade {
                booted: ver("45.20260920.1"),
                staged: ver("45.20260901.1"),
            }
            .to_string(),
            "The downloaded version 45.20260901.1 is not newer than the installed version \
             45.20260920.1."
        );
        assert_eq!(
            ApplyError::Gate(Deferral::new(Gate::Power, "Plug in.")).to_string(),
            "Plug in."
        );
    }

    fn applied() -> SystemSnapshot {
        let mut s = with_locked_stage();
        s.host.staged.as_mut().expect("staged").download_only = false;
        s.intent = Some(intent(REMOTE));
        s
    }

    #[test]
    fn intent_orphan_detection() {
        assert!(!intent_is_orphaned(&healthy()), "no intent");
        assert!(!intent_is_orphaned(&applied()), "waiting for the restart");

        let mut booted = healthy();
        booted.intent = Some(intent(BOOTED));
        assert!(!intent_is_orphaned(&booted), "waiting for the assessment");

        let mut relocked = with_locked_stage();
        relocked.intent = Some(intent(REMOTE));
        assert!(
            intent_is_orphaned(&relocked),
            "stage locked again outside Apex"
        );

        let mut lost = healthy();
        lost.intent = Some(intent(REMOTE));
        lost.host.finalize_failed = true;
        assert!(intent_is_orphaned(&lost), "finalization failed");

        let mut replaced = applied();
        replaced.host.staged.as_mut().expect("staged").digest = STAGED.into();
        assert!(intent_is_orphaned(&replaced), "stage replaced");
    }
}
