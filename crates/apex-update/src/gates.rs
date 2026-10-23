use std::fmt;

use crate::format::format_size;
use crate::records::{Denylist, Intent};
use crate::snapshot::{HostView, Power, RemoteView, SystemSnapshot};
use crate::version::Version;

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;

pub const DOWNLOAD_MIN_BATTERY_PERCENT: u8 = 50;
/// Finalization copies the kernel and merges `/etc` at shutdown, which an empty battery
/// must not cut short.
pub const APPLY_MIN_BATTERY_PERCENT: u8 = 20;
pub const DEFAULT_IMAGE_SIZE: u64 = 8 * GIB;
/// Must stay free on `/sysroot` after a download, and before an apply.
pub const SYSROOT_MARGIN: u64 = 2 * GIB;
pub const DEFAULT_BOOT_PAYLOAD: u64 = 150 * MIB;
/// 1.2 as a ratio, so the check stays in integer arithmetic.
pub const BOOT_SPACE_FACTOR: (u64, u64) = (6, 5);

/// In the order the combined checks evaluate them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Gate {
    BootcFlags,
    Intent,
    Denylist,
    Version,
    DiskSpace,
    BootSpace,
    Power,
    Metered,
}

impl Gate {
    /// A user call may override a soft gate.
    pub const fn is_soft(self) -> bool {
        matches!(self, Self::Power | Self::Metered)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BootcFlags => "bootc-flags",
            Self::Intent => "intent",
            Self::Denylist => "denylist",
            Self::Version => "version",
            Self::DiskSpace => "disk-space",
            Self::BootSpace => "boot-space",
            Self::Power => "power",
            Self::Metered => "metered",
        }
    }
}

impl fmt::Display for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, thiserror::Error)]
#[error("{reason}")]
pub struct Deferral {
    pub gate: Gate,
    /// Shown to the user as is: a short sentence, in sentence case, without jargon.
    pub reason: String,
}

impl Deferral {
    pub fn new(gate: Gate, reason: impl Into<String>) -> Self {
        Self {
            gate,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    Download,
    Apply,
}

impl Operation {
    const fn verb(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Apply => "install",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    Automatic,
    User,
}

pub fn gate_power(power: Power, op: Operation) -> Result<(), Deferral> {
    if power.on_ac {
        return Ok(());
    }
    let minimum = match op {
        Operation::Download => DOWNLOAD_MIN_BATTERY_PERCENT,
        Operation::Apply => APPLY_MIN_BATTERY_PERCENT,
    };
    let verb = op.verb();
    match power.battery_percent {
        Some(percent) if percent >= minimum => Ok(()),
        Some(_) => Err(Deferral::new(
            Gate::Power,
            format!("The battery is below {minimum}%. Connect the charger to {verb} the update."),
        )),
        None => Err(Deferral::new(
            Gate::Power,
            format!("The battery level is unknown. Connect the charger to {verb} the update."),
        )),
    }
}

pub fn gate_metered(metered: Option<bool>) -> Result<(), Deferral> {
    if metered == Some(true) {
        Err(Deferral::new(
            Gate::Metered,
            "The network connection is metered. Connect to a network without data limits \
             to download the update.",
        ))
    } else {
        Ok(())
    }
}

pub fn gate_disk_space(sysroot_free: u64, image_size: Option<u64>) -> Result<(), Deferral> {
    let needed = image_size
        .unwrap_or(DEFAULT_IMAGE_SIZE)
        .saturating_add(SYSROOT_MARGIN);
    disk_shortfall(sysroot_free, needed, Operation::Download)
}

/// The image is on disk already, but finalization still writes the new `/etc` at shutdown.
pub fn gate_disk_headroom(sysroot_free: u64) -> Result<(), Deferral> {
    disk_shortfall(sysroot_free, SYSROOT_MARGIN, Operation::Apply)
}

fn disk_shortfall(free: u64, needed: u64, op: Operation) -> Result<(), Deferral> {
    if free >= needed {
        return Ok(());
    }
    Err(Deferral::new(
        Gate::DiskSpace,
        format!(
            "There is not enough free disk space. Free up {} to {} the update.",
            format_size(needed - free),
            op.verb()
        ),
    ))
}

pub fn gate_boot_space(boot_free: u64, boot_payload: Option<u64>) -> Result<(), Deferral> {
    let payload = u128::from(boot_payload.unwrap_or(DEFAULT_BOOT_PAYLOAD));
    let (num, den) = BOOT_SPACE_FACTOR;
    let needed = (payload * u128::from(num)).div_ceil(u128::from(den));
    let free = u128::from(boot_free);
    if free >= needed {
        return Ok(());
    }
    let shortfall = u64::try_from(needed - free).unwrap_or(u64::MAX);
    Err(Deferral::new(
        Gate::BootSpace,
        format!(
            "The boot partition is too full for this update. It needs {} more free space.",
            format_size(shortfall)
        ),
    ))
}

pub fn gate_denylist(denylist: &Denylist, digest: &str) -> Result<(), Deferral> {
    if denylist.contains(digest) {
        Err(Deferral::new(
            Gate::Denylist,
            "This update failed to start correctly before. Waiting for a newer update.",
        ))
    } else {
        Ok(())
    }
}

/// Same precedence as [`crate::derive_state`].
pub fn gate_bootc_flags(host: &HostView) -> Result<(), Deferral> {
    bootc_flags(host.incompatible, host.usr_overlay, host.rollback_queued)
}

/// Lets a caller leave a flag out.
pub(crate) fn bootc_flags(
    incompatible: bool,
    usr_overlay: bool,
    rollback_queued: bool,
) -> Result<(), Deferral> {
    let reason = if incompatible {
        "The system has local package changes that block updates."
    } else if usr_overlay {
        "System files are unlocked for temporary changes. Restart to lock them again."
    } else if rollback_queued {
        "The previous version is set to start at the next restart. Restart before updating."
    } else {
        return Ok(());
    };
    Err(Deferral::new(Gate::BootcFlags, reason))
}

/// Nothing downloads a downgrade; `Rollback` is the way back. A user may download an image of
/// unknown version: it is only staged, and [`crate::validate_apply`] checks its manifest.
pub fn gate_version(
    booted: Option<&Version>,
    candidate: Option<&Version>,
    trigger: Trigger,
) -> Result<(), Deferral> {
    match (booted, candidate) {
        (Some(booted), Some(candidate)) if candidate > booted => Ok(()),
        (Some(booted), Some(candidate)) => Err(Deferral::new(
            Gate::Version,
            format!(
                "The offered version {candidate} is not newer than the installed version \
                 {booted}."
            ),
        )),
        _ if trigger == Trigger::User => Ok(()),
        (_, None) => Err(Deferral::new(
            Gate::Version,
            "The update has no version number, so it is not downloaded automatically.",
        )),
        (None, Some(_)) => Err(Deferral::new(
            Gate::Version,
            "The installed version is unknown, so updates are not downloaded automatically.",
        )),
    }
}

/// Never replace or re-lock what was applied, even outside Apex.
pub fn gate_intent(
    intent: Option<&Intent>,
    host: &HostView,
    trigger: Trigger,
) -> Result<(), Deferral> {
    let blocked = match trigger {
        Trigger::Automatic => intent.is_some() || host.has_unlocked_stage(),
        Trigger::User => host.has_unlocked_stage(),
    };
    if !blocked {
        return Ok(());
    }
    let reason = match trigger {
        Trigger::Automatic => "An update is already set to install at the next restart.",
        Trigger::User => {
            "An update is already set to install at the next restart. Restart first to \
             download a different one."
        }
    };
    Err(Deferral::new(Gate::Intent, reason))
}

/// Hard gates first, so a deferral the user could override is reported only when nothing
/// else blocks.
pub fn check_download_gates(
    snapshot: &SystemSnapshot,
    candidate: &RemoteView,
    trigger: Trigger,
    override_soft_gates: bool,
) -> Result<(), Deferral> {
    gate_bootc_flags(&snapshot.host)?;
    gate_intent(snapshot.intent.as_ref(), &snapshot.host, trigger)?;
    gate_denylist(&snapshot.denylist, &candidate.digest)?;
    gate_version(
        snapshot.host.booted_version(),
        candidate.version.as_ref(),
        trigger,
    )?;
    check_download_resources(snapshot, override_soft_gates)
}

pub fn check_download_resources(
    snapshot: &SystemSnapshot,
    override_soft_gates: bool,
) -> Result<(), Deferral> {
    gate_disk_space(snapshot.sysroot_free, snapshot.image_size)?;
    gate_boot_space(snapshot.boot_free, snapshot.boot_payload)?;
    if !override_soft_gates {
        gate_power(snapshot.power, Operation::Download)?;
        gate_metered(snapshot.metered)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::DenyEntry;
    use crate::snapshot::DeploymentView;

    fn ver(s: &str) -> Version {
        Version::parse(s).expect("valid")
    }

    fn battery(on_ac: bool, percent: Option<u8>) -> Power {
        Power {
            on_ac,
            battery_percent: percent,
        }
    }

    #[test]
    fn gate_kinds() {
        let soft: Vec<_> = [
            Gate::BootcFlags,
            Gate::Intent,
            Gate::Denylist,
            Gate::Version,
            Gate::DiskSpace,
            Gate::BootSpace,
            Gate::Power,
            Gate::Metered,
        ]
        .into_iter()
        .filter(|g| g.is_soft())
        .collect();
        assert_eq!(soft, [Gate::Power, Gate::Metered]);
        assert_eq!(Gate::DiskSpace.to_string(), "disk-space");
        assert_eq!(
            Deferral::new(Gate::Power, "Plug in.").to_string(),
            "Plug in."
        );
    }

    #[test]
    fn power_on_ac_always_passes() {
        for op in [Operation::Download, Operation::Apply] {
            assert_eq!(gate_power(battery(true, None), op), Ok(()));
            assert_eq!(gate_power(battery(true, Some(1)), op), Ok(()));
        }
    }

    #[test]
    fn power_download_threshold_is_50_percent() {
        assert_eq!(
            gate_power(battery(false, Some(50)), Operation::Download),
            Ok(())
        );
        assert_eq!(
            gate_power(battery(false, Some(100)), Operation::Download),
            Ok(())
        );
        let err = gate_power(battery(false, Some(49)), Operation::Download).expect_err("defer");
        assert_eq!(err.gate, Gate::Power);
        assert_eq!(
            err.reason,
            "The battery is below 50%. Connect the charger to download the update."
        );
    }

    #[test]
    fn power_apply_threshold_is_20_percent() {
        assert_eq!(
            gate_power(battery(false, Some(20)), Operation::Apply),
            Ok(())
        );
        let err = gate_power(battery(false, Some(19)), Operation::Apply).expect_err("defer");
        assert_eq!(
            err.reason,
            "The battery is below 20%. Connect the charger to install the update."
        );
    }

    #[test]
    fn power_unknown_battery_off_ac_defers() {
        let err = gate_power(battery(false, None), Operation::Download).expect_err("defer");
        assert_eq!(err.gate, Gate::Power);
        assert!(err.reason.starts_with("The battery level is unknown."));
        let err = gate_power(battery(false, None), Operation::Apply).expect_err("defer");
        assert!(err.reason.ends_with("to install the update."));
    }

    #[test]
    fn metered_only_known_metered_defers() {
        assert_eq!(gate_metered(None), Ok(()));
        assert_eq!(gate_metered(Some(false)), Ok(()));
        assert_eq!(
            gate_metered(Some(true)).expect_err("defer").gate,
            Gate::Metered
        );
    }

    #[test]
    fn disk_space_needs_image_plus_margin() {
        let image = 3 * GIB;
        assert_eq!(gate_disk_space(image + SYSROOT_MARGIN, Some(image)), Ok(()));
        let err = gate_disk_space(image + SYSROOT_MARGIN - 1, Some(image)).expect_err("defer");
        assert_eq!(err.gate, Gate::DiskSpace);
        assert_eq!(
            err.reason,
            "There is not enough free disk space. Free up 1 byte to download the update."
        );
    }

    #[test]
    fn disk_space_defaults_to_8_gib_image() {
        assert_eq!(gate_disk_space(10 * GIB, None), Ok(()));
        let err = gate_disk_space(10 * GIB - 1, None).expect_err("defer");
        assert_eq!(err.gate, Gate::DiskSpace);
        let err = gate_disk_space(0, None).expect_err("defer");
        assert!(err.reason.contains("Free up 10.7 GB"), "{}", err.reason);
    }

    #[test]
    fn disk_space_saturates_on_huge_images() {
        assert!(gate_disk_space(u64::MAX, Some(u64::MAX)).is_ok());
        assert!(gate_disk_space(u64::MAX - 1, Some(u64::MAX)).is_err());
    }

    #[test]
    fn disk_headroom_for_apply() {
        assert_eq!(gate_disk_headroom(SYSROOT_MARGIN), Ok(()));
        let err = gate_disk_headroom(SYSROOT_MARGIN - 1000).expect_err("defer");
        assert_eq!(err.gate, Gate::DiskSpace);
        assert_eq!(
            err.reason,
            "There is not enough free disk space. Free up 1.0 kB to install the update."
        );
    }

    #[test]
    fn boot_space_needs_1_2_times_payload() {
        let payload = 100 * MIB;
        let needed = 120 * MIB;
        assert_eq!(gate_boot_space(needed, Some(payload)), Ok(()));
        let err = gate_boot_space(needed - 1, Some(payload)).expect_err("defer");
        assert_eq!(err.gate, Gate::BootSpace);
        assert_eq!(
            err.reason,
            "The boot partition is too full for this update. It needs 1 byte more free space."
        );
        assert!(gate_boot_space(1, Some(1)).is_err());
        assert!(gate_boot_space(2, Some(1)).is_ok());
        assert!(gate_boot_space(u64::MAX, Some(u64::MAX)).is_err());
        assert!(gate_boot_space(u64::MAX, Some(u64::MAX / 2)).is_ok());
    }

    #[test]
    fn boot_space_shortfall_saturates() {
        let err = gate_boot_space(0, Some(u64::MAX)).expect_err("defer");
        assert!(err.reason.contains("18.4 EB"), "{}", err.reason);
    }

    #[test]
    fn boot_space_defaults_to_150_mib_payload() {
        assert_eq!(gate_boot_space(180 * MIB, None), Ok(()));
        assert!(gate_boot_space(180 * MIB - 1, None).is_err());
    }

    #[test]
    fn denylist_gate() {
        let list: Denylist = [DenyEntry {
            digest: "sha256:bad".into(),
            reason: "red".into(),
            at: 1,
        }]
        .into_iter()
        .collect();
        assert_eq!(gate_denylist(&list, "sha256:good"), Ok(()));
        let err = gate_denylist(&list, "sha256:bad").expect_err("defer");
        assert_eq!(err.gate, Gate::Denylist);
        assert_eq!(
            err.reason,
            "This update failed to start correctly before. Waiting for a newer update."
        );
    }

    #[test]
    fn bootc_flags_each_block_in_precedence_order() {
        let clean = HostView::default();
        assert_eq!(gate_bootc_flags(&clean), Ok(()));

        let rollback = HostView {
            rollback_queued: true,
            ..HostView::default()
        };
        let err = gate_bootc_flags(&rollback).expect_err("defer");
        assert_eq!(err.gate, Gate::BootcFlags);
        assert!(
            err.reason
                .starts_with("The previous version is set to start")
        );

        let overlay = HostView {
            usr_overlay: true,
            rollback_queued: true,
            ..HostView::default()
        };
        assert!(
            gate_bootc_flags(&overlay)
                .expect_err("defer")
                .reason
                .starts_with("System files are unlocked")
        );

        let incompatible = HostView {
            incompatible: true,
            usr_overlay: true,
            rollback_queued: true,
            ..HostView::default()
        };
        assert_eq!(
            gate_bootc_flags(&incompatible).expect_err("defer").reason,
            "The system has local package changes that block updates."
        );

        let finalize = HostView {
            finalize_failed: true,
            ..HostView::default()
        };
        assert_eq!(gate_bootc_flags(&finalize), Ok(()));
    }

    #[test]
    fn version_newer_passes_for_both_triggers() {
        let (old, new) = (ver("45.20260920.1"), ver("45.20260924.1"));
        for trigger in [Trigger::Automatic, Trigger::User] {
            assert_eq!(gate_version(Some(&old), Some(&new), trigger), Ok(()));
        }
    }

    #[test]
    fn version_equal_or_older_defers_for_both_triggers() {
        let (old, new) = (ver("45.20260920.1"), ver("45.20260924.1"));
        for trigger in [Trigger::Automatic, Trigger::User] {
            let err = gate_version(Some(&new), Some(&old), trigger).expect_err("older");
            assert_eq!(err.gate, Gate::Version);
            assert_eq!(
                err.reason,
                "The offered version 45.20260920.1 is not newer than the installed version \
                 45.20260924.1."
            );
            assert!(
                gate_version(Some(&new), Some(&new), trigger).is_err(),
                "equal"
            );
        }
    }

    #[test]
    fn version_unknown_defers_automatic_only() {
        let known = ver("45.20260924.1");
        for (booted, candidate) in [(Some(&known), None), (None, Some(&known)), (None, None)] {
            assert_eq!(gate_version(booted, candidate, Trigger::User), Ok(()));
            let err = gate_version(booted, candidate, Trigger::Automatic).expect_err("defer");
            assert_eq!(err.gate, Gate::Version);
        }
        assert!(
            gate_version(Some(&known), None, Trigger::Automatic)
                .expect_err("defer")
                .reason
                .starts_with("The update has no version number")
        );
        assert!(
            gate_version(None, Some(&known), Trigger::Automatic)
                .expect_err("defer")
                .reason
                .starts_with("The installed version is unknown")
        );
        assert!(
            gate_version(None, None, Trigger::Automatic)
                .expect_err("defer")
                .reason
                .starts_with("The update has no version number")
        );
    }

    fn staged(download_only: bool) -> HostView {
        HostView {
            staged: Some(DeploymentView {
                digest: "sha256:s".into(),
                version: None,
                download_only,
                pinned: false,
            }),
            ..HostView::default()
        }
    }

    #[test]
    fn intent_blocks_automatic_operations() {
        let intent = Intent {
            digest: "sha256:s".into(),
            created: 1,
        };
        let host = HostView::default();
        assert_eq!(gate_intent(None, &host, Trigger::Automatic), Ok(()));
        let err = gate_intent(Some(&intent), &host, Trigger::Automatic).expect_err("defer");
        assert_eq!(err.gate, Gate::Intent);
        assert_eq!(
            err.reason,
            "An update is already set to install at the next restart."
        );
        let unlocked_outside_apex = staged(false);
        assert!(gate_intent(None, &unlocked_outside_apex, Trigger::Automatic).is_err());
        assert_eq!(gate_intent(None, &staged(true), Trigger::Automatic), Ok(()));
    }

    #[test]
    fn intent_blocks_user_download_only_while_a_stage_is_unlocked() {
        let intent = Intent {
            digest: "sha256:s".into(),
            created: 1,
        };
        assert_eq!(
            gate_intent(Some(&intent), &HostView::default(), Trigger::User),
            Ok(())
        );
        assert_eq!(
            gate_intent(Some(&intent), &staged(true), Trigger::User),
            Ok(())
        );
        let err = gate_intent(None, &staged(false), Trigger::User).expect_err("defer");
        assert_eq!(err.gate, Gate::Intent);
        assert!(
            err.reason
                .ends_with("Restart first to download a different one.")
        );
    }
}
