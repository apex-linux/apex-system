//! Decisions: what the automatic run does, and whether a user's download, apply or
//! discard may run.
//!
//! Each function reads only the snapshot (plus, for apply, the version the daemon read
//! from the staged deployment's own manifest) and performs no I/O.

use crate::gates::{
    Deferral, Gate, Operation, Trigger, bootc_flags, check_download_gates,
    check_download_resources, gate_boot_space, gate_bootc_flags, gate_denylist, gate_disk_headroom,
    gate_intent, gate_power,
};
use crate::snapshot::{RemoteView, SystemSnapshot};
use crate::version::Version;

/// What the automatic run (the systemd timer) should do before it knows the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomaticPlan {
    /// Do nothing at all, not even a check: the system state or the user's pending
    /// apply must not be touched. The deferral says why.
    Skip(Deferral),
    /// Check for a newer image, but do not download: a gate that does not depend on
    /// the remote image already fails. The deferral is the reason to publish if the
    /// check finds an update.
    Check(Deferral),
    /// Check, then ask [`plan_after_check`] whether to download.
    CheckThenDownload,
}

/// What the automatic run should do after a check updated the snapshot's remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterCheck {
    /// Nothing to download: no remote image is known, or it is already booted or
    /// staged.
    NothingToDo,
    /// Download and stage the remote image, locked.
    Download,
    /// There is an update, but a gate defers it.
    Defer(Deferral),
}

/// Plans the automatic run.
///
/// - [`AutomaticPlan::Skip`] when the bootc flags block updates, or while an intent
///   exists or a staged deployment is unlocked (the automatic run must never re-lock or
///   replace what the user applied).
/// - [`AutomaticPlan::Check`] when space, power or metered already fail: checking only
///   fetches a manifest, and knowing about the update lets the UI explain the deferral.
/// - [`AutomaticPlan::CheckThenDownload`] otherwise.
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

/// The remote image if it is neither booted nor staged.
fn candidate(snapshot: &SystemSnapshot) -> Option<&RemoteView> {
    let remote = snapshot.remote.as_ref()?;
    let host = &snapshot.host;
    let digest = Some(remote.digest.as_str());
    (digest != host.booted_digest() && digest != host.staged_digest()).then_some(remote)
}

/// Decides the automatic download once the check is done. Every gate is evaluated with
/// [`Trigger::Automatic`] and no override: bootc flags, intent, denylist, version,
/// space, power, metered, in that order.
pub fn plan_after_check(snapshot: &SystemSnapshot) -> AfterCheck {
    let Some(remote) = candidate(snapshot) else {
        return AfterCheck::NothingToDo;
    };
    match check_download_gates(snapshot, remote, Trigger::Automatic, false) {
        Ok(()) => AfterCheck::Download,
        Err(deferral) => AfterCheck::Defer(deferral),
    }
}

/// Why a user's download cannot run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DownloadError {
    /// No newer image is known, or it is already booted or staged
    /// (D-Bus `NothingToDo`).
    #[error("There is no update to download.")]
    NothingToDownload,
    /// A gate refuses it (D-Bus `PolicyDenied`, with the reason as message).
    #[error(transparent)]
    Gate(#[from] Deferral),
}

/// Validates a user's `Download(override_soft_gates)`.
///
/// Uses [`Trigger::User`]: an unknown version does not block (apply re-checks it from the
/// staged manifest), and an existing intent blocks only while its stage is unlocked.
/// `override_soft_gates` skips power and metered, never the hard gates.
pub fn validate_download(
    snapshot: &SystemSnapshot,
    override_soft_gates: bool,
) -> Result<(), DownloadError> {
    let remote = candidate(snapshot).ok_or(DownloadError::NothingToDownload)?;
    check_download_gates(snapshot, remote, Trigger::User, override_soft_gates)?;
    Ok(())
}

/// Why `Apply(expected_digest)` cannot run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    /// There is no staged deployment (D-Bus `NothingToDo`).
    #[error("There is no downloaded update to install.")]
    NothingStaged,
    /// The staged digest is not the one the user reviewed (D-Bus `DigestMismatch`).
    #[error("The downloaded update changed since it was shown. Review it again.")]
    DigestMismatch {
        /// The digest the caller reviewed.
        expected: String,
        /// The digest actually staged.
        staged: String,
    },
    /// The staged deployment is already unlocked (D-Bus `NothingToDo`).
    #[error("The update is already set to install at the next restart.")]
    AlreadyApplied,
    /// The staged image is not newer than the booted one, by the versions in the
    /// images' own manifests (D-Bus `PolicyDenied`). Equal versions count as not newer.
    #[error("The downloaded version {staged} is not newer than the installed version {booted}.")]
    Downgrade {
        /// Booted version.
        booted: Version,
        /// Staged version, from the staged deployment's manifest.
        staged: Version,
    },
    /// A gate refuses it (D-Bus `PolicyDenied`).
    #[error(transparent)]
    Gate(#[from] Deferral),
}

/// Validates `Apply(expected_digest)`.
///
/// `staged_manifest_version` is the version the daemon read from
/// `/usr/share/apex/manifest.json` inside the staged deployment: verified content, not a
/// registry label.
///
/// In order:
/// 1. a staged deployment exists ([`ApplyError::NothingStaged`]);
/// 2. its digest is `expected_digest` ([`ApplyError::DigestMismatch`]);
/// 3. it is still download-only ([`ApplyError::AlreadyApplied`]);
/// 4. the bootc flags do not block;
/// 5. the digest is not denylisted;
/// 6. the staged version is newer than the booted version ([`ApplyError::Downgrade`]).
///    A staged image without a valid manifest version is refused. An unknown booted
///    version is accepted, since there is nothing to compare with (for example the first
///    update after installing from an image built before manifests existed);
/// 7. `/boot` has room for 1.2 × the kernel and initramfs;
/// 8. `/sysroot` keeps its margin;
/// 9. power: on AC or battery at least 20 %, because finalization runs at shutdown.
///    This is a soft gate, but `Apply` has no override, so it is enforced.
///
/// Arming the boot counter requires GRUB entry 1 to be a real deployment. After
/// finalization the booted deployment becomes entry 1, so this does not depend on an
/// existing rollback deployment and is not checked here.
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
    gate_power(snapshot.power, Operation::Apply)?;
    Ok(())
}

/// Why `DiscardStaged` cannot run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscardError {
    /// There is no staged deployment (D-Bus `NothingToDo`).
    #[error("There is no update to cancel.")]
    NothingStaged,
    /// The staged deployment is still locked, so it will not be installed anyway
    /// (D-Bus `NothingToDo`).
    #[error("The update is not set to install, so there is nothing to cancel.")]
    NotApplied,
    /// The bootc flags block staging again, or cancelling would download a newer
    /// update that a download gate refuses now (D-Bus `PolicyDenied`).
    #[error(transparent)]
    Gate(#[from] Deferral),
}

/// Validates `DiscardStaged`: re-lock an applied but not yet booted update.
///
/// Requires an unlocked staged deployment. The only re-lock the contract allows is
/// `bootc upgrade --download-only`, which is **not local**: it pulls the channel tag
/// again, so it needs the registry and fails offline (the stage then stays applied), and
/// what it does depends on where the tag points now:
///
/// - at the staged digest: bootc locks the stage again, fetching only the manifest;
/// - at the booted digest: bootc changes nothing, exits 0 and the stage **stays
///   applied**;
/// - at another image: bootc downloads that whole image and stages it locked in place of
///   the applied one.
///
/// So the daemon must check afterwards that no unlocked stage remains
/// ([`crate::HostView::has_unlocked_stage`] on a fresh status) before it disarms the boot
/// counter or forgets the intent, and report a failure otherwise.
///
/// This function checks what can be known beforehand:
///
/// 1. `incompatible` and `rollbackQueued` block, because bootc refuses on an
///    incompatible host and a new stage would reorder deployments under a queued
///    rollback. A `/usr` overlay does not block: restarting to clear it would install the
///    very update the user is cancelling.
/// 2. When the last known remote digest is neither the staged nor the booted one, or no
///    remote is known, bootc may download a whole image, so the download resource gates
///    apply: space, then power and metered, without override. The reason says that
///    cancelling downloads the newer update.
pub fn validate_discard(snapshot: &SystemSnapshot) -> Result<(), DiscardError> {
    let host = &snapshot.host;
    let staged = host.staged.as_ref().ok_or(DiscardError::NothingStaged)?;
    if staged.download_only {
        return Err(DiscardError::NotApplied);
    }
    bootc_flags(host.incompatible, false, host.rollback_queued)?;
    let remote = snapshot.remote.as_ref().map(|r| r.digest.as_str());
    let may_download =
        remote.is_none_or(|digest| digest != staged.digest && Some(digest) != host.booted_digest());
    if may_download {
        check_download_resources(snapshot, false).map_err(|deferral| {
            let context = if remote.is_some() {
                "The update channel has a newer update, and cancelling downloads it."
            } else {
                "Cancelling may download a newer update from the update channel."
            };
            Deferral::new(deferral.gate, format!("{context} {}", deferral.reason))
        })?;
    }
    Ok(())
}

/// Whether the intent can no longer come true and should be cleared by the daemon.
///
/// An intent is live while its digest is staged and unlocked (waiting for the restart)
/// or booted (waiting for, or failed, its boot assessment). Anything else is orphaned:
/// finalization failed, GRUB fell back, or the stage was replaced outside Apex. Because
/// [`plan_automatic`] skips while any intent exists, an orphaned intent would stop
/// automatic updates for good, so the daemon clears it when this returns `true`.
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
    use crate::snapshot::{Power, RemoteView};
    use crate::test_support::{
        BOOTED, REMOTE, STAGED, deny, deployment, healthy, intent, ver, with_locked_stage,
    };

    /// A change to a snapshot and the gate it must trip.
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

    // plan_automatic

    #[test]
    fn automatic_checks_then_downloads_when_healthy() {
        assert_eq!(plan_automatic(&healthy()), AutomaticPlan::CheckThenDownload);
        // A locked stage without intent may be replaced by a newer one.
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

    // plan_after_check

    #[test]
    fn after_check_downloads_a_newer_image() {
        assert_eq!(plan_after_check(&healthy()), AfterCheck::Download);
        // Replacing an older locked stage with a newer remote.
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

        // Nothing to do wins over a blocking flag: no deferral is published.
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

    // validate_download

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

    // validate_apply

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
        // The label says newer, the verified manifest says older: refused.
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

    // validate_discard

    fn applied() -> SystemSnapshot {
        let mut s = with_locked_stage();
        s.host.staged.as_mut().expect("staged").download_only = false;
        s.intent = Some(intent(REMOTE));
        s
    }

    #[test]
    fn discard_re_locks_an_applied_stage() {
        assert_eq!(validate_discard(&applied()), Ok(()));
    }

    #[test]
    fn discard_needs_an_applied_stage() {
        assert_eq!(
            validate_discard(&healthy()),
            Err(DiscardError::NothingStaged)
        );
        assert_eq!(
            validate_discard(&with_locked_stage()),
            Err(DiscardError::NotApplied)
        );
    }

    #[test]
    fn discard_is_blocked_by_incompatible_and_rollback_queued_only() {
        for set in [
            |s: &mut SystemSnapshot| s.host.incompatible = true,
            |s: &mut SystemSnapshot| s.host.rollback_queued = true,
        ] {
            let mut s = applied();
            set(&mut s);
            assert!(matches!(
                validate_discard(&s),
                Err(DiscardError::Gate(Deferral {
                    gate: Gate::BootcFlags,
                    ..
                }))
            ));
        }
        let mut s = applied();
        s.host.usr_overlay = true;
        assert_eq!(validate_discard(&s), Ok(()));
    }

    /// The tag still points at the applied stage: bootc re-locks it after fetching only
    /// the manifest, so power, metered and space do not matter.
    #[test]
    fn discard_of_the_current_remote_needs_no_resources() {
        let mut s = applied();
        s.power = on_battery(1);
        s.metered = Some(true);
        s.boot_free = 0;
        s.sysroot_free = 0;
        assert_eq!(validate_discard(&s), Ok(()));
        // The tag moved back to the booted image: nothing is downloaded either (bootc
        // then leaves the stage applied, which the daemon must detect afterwards).
        s.remote.as_mut().expect("remote").digest = BOOTED.into();
        assert_eq!(validate_discard(&s), Ok(()));
    }

    fn discard_gate(s: &SystemSnapshot) -> Option<(Gate, String)> {
        match validate_discard(s) {
            Err(DiscardError::Gate(d)) => Some((d.gate, d.reason)),
            _ => None,
        }
    }

    /// The tag moved on to V3 while V2 is applied: `bootc upgrade --download-only`
    /// downloads and stages V3, so the download gates apply, without override.
    #[test]
    fn discard_after_the_channel_moved_applies_the_download_gates() {
        let moved = || {
            let mut s = applied();
            s.remote = Some(RemoteView {
                digest: "sha256:newer".into(),
                version: Some(ver("45.20260926.1")),
            });
            s
        };
        assert_eq!(validate_discard(&moved()), Ok(()));
        let cases: [Case; 4] = [
            (|s| s.power = on_battery(10), Gate::Power),
            (|s| s.metered = Some(true), Gate::Metered),
            (|s| s.sysroot_free = GIB, Gate::DiskSpace),
            (|s| s.boot_free = 0, Gate::BootSpace),
        ];
        for (set, gate) in cases {
            let mut s = moved();
            set(&mut s);
            let (got, reason) = discard_gate(&s).expect("deferred");
            assert_eq!(got, gate);
            assert!(
                reason.starts_with(
                    "The update channel has a newer update, and cancelling downloads it. "
                ),
                "{reason}"
            );
        }
    }

    #[test]
    fn discard_without_a_known_remote_applies_the_download_gates() {
        let mut s = applied();
        s.remote = None;
        assert_eq!(validate_discard(&s), Ok(()));
        s.metered = Some(true);
        let (gate, reason) = discard_gate(&s).expect("deferred");
        assert_eq!(gate, Gate::Metered);
        assert!(
            reason.starts_with("Cancelling may download a newer update from the update channel. "),
            "{reason}"
        );
    }

    // intent_is_orphaned

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
