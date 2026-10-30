//! `MajorUpgrade`: the next Fedora major's stable image, downloaded first so that a missing
//! or refused image fails before the pin and the `/var` snapshot (ARCHITECTURE §5.4).

use apex_bootc::{BootEntry, CancelToken, CommandRunner, ErrorKind as BootcErrorKind, Host};
use apex_dbus::ApexError;
use apex_update::gates::{check_download_resources, gate_bootc_flags};
use apex_update::{MajorPinRecord, MajorUpgrades, SnapshotRecord, SystemSnapshot};

use super::Engine;
use crate::errors::{self, from_bootc, from_deferral};
use crate::properties::Work;
use crate::store::unflushed_is_done;

/// Before any side effect: what `MajorUpgrade` refuses with, given the running major.
pub fn major_upgrade_precondition(
    booted_major: u32,
    target_major: u32,
    snapshot: &SystemSnapshot,
    var_is_subvolume: Result<bool, String>,
) -> Result<(), ApexError> {
    if target_major == booted_major {
        return Err(ApexError::NothingToDo(format!(
            "This computer already runs Apex {booted_major}."
        )));
    }
    if Some(target_major) != booted_major.checked_add(1) {
        return Err(ApexError::InvalidArgument(format!(
            "Apex {booted_major} moves to Apex {} only: one release at a time.",
            booted_major.saturating_add(1)
        )));
    }
    if snapshot.host.staged.is_some() {
        return Err(ApexError::PolicyDenied(
            "A downloaded update is waiting. Restart into it first, then start the major \
             upgrade again."
                .to_owned(),
        ));
    }
    match var_is_subvolume {
        Ok(true) => Ok(()),
        Ok(false) => Err(ApexError::PolicyDenied(
            "/var is not a btrfs subvolume, so it cannot be saved before the move. The major \
             upgrade is refused."
                .to_owned(),
        )),
        Err(error) => Err(ApexError::Internal(format!(
            "Whether /var can be saved before the move is not known: {error}"
        ))),
    }
}

impl<R: CommandRunner> Engine<R> {
    pub(super) async fn major_upgrade(
        &self,
        target_major: u32,
        override_soft_gates: bool,
        cancel: &CancelToken,
    ) -> Result<(), ApexError> {
        self.refresh(cancel).await?;
        let snapshot = self.snapshot()?;
        let host = self.host().ok_or_else(|| self.state_unknown())?;
        let running = self
            .running_manifest()
            .await
            .map_err(|e| errors::internal("The running image's manifest cannot be read", &e))?
            .ok_or_else(|| {
                ApexError::Internal(
                    "The running image has no manifest, so its Fedora release is not known."
                        .to_owned(),
                )
            })?;
        let from_major = running.fedora;
        major_upgrade_precondition(
            from_major,
            target_major,
            &snapshot,
            self.providers.var.is_btrfs_subvolume(),
        )?;
        gate_bootc_flags(&snapshot.host)
            .and_then(|()| check_download_resources(&snapshot, override_soft_gates))
            .map_err(|deferral| from_deferral(&deferral))?;

        let staged = self.switch_to(target_major, cancel).await?;
        self.keep_running_version(&host, from_major, target_major, cancel)
            .await?;
        if let Err(error) = self.save_var(from_major, target_major, cancel).await {
            self.refresh_after_acting(cancel).await;
            return Err(error);
        }
        self.apply(&staged, cancel).await
    }

    /// The digest staged, locked, for `target_major`.
    async fn switch_to(
        &self,
        target_major: u32,
        cancel: &CancelToken,
    ) -> Result<String, ApexError> {
        self.set_work(Some(Work::Downloading { progress: None }));
        let switched = self
            .marking_bootc_busy(
                self.bootc
                    .switch_download_only(apex_bootc::apex_stable(target_major), cancel),
            )
            .await;
        let refreshed = self.after_staging(None, cancel).await;
        let refused = if switched.is_ok() && refreshed {
            self.screen_stage(cancel).await
        } else {
            None
        };
        self.set_work(None);
        switched.map_err(|error| match error.kind() {
            BootcErrorKind::ImageNotFound => {
                ApexError::NothingToDo(format!("No Apex {target_major} image is published yet."))
            }
            _ => from_bootc(&error),
        })?;
        if let Some(reason) = refused {
            return Err(ApexError::PolicyDenied(reason));
        }
        self.host()
            .as_ref()
            .and_then(Host::staged)
            .and_then(BootEntry::digest)
            .map(str::to_owned)
            .ok_or_else(|| {
                ApexError::Internal(format!(
                    "bootc downloaded Apex {target_major}, but no update is staged."
                ))
            })
    }

    /// Recorded in major-upgrade.json, never pinned.json, so the daily unpin leaves it.
    async fn keep_running_version(
        &self,
        host: &Host,
        from_major: u32,
        to_major: u32,
        cancel: &CancelToken,
    ) -> Result<(), ApexError> {
        let booted = host.booted();
        if booted.is_some_and(|entry| entry.pinned) {
            tracing::info!("the running version is pinned already");
            return Ok(());
        }
        self.marking_bootc_busy(self.bootc.pin_booted(cancel))
            .await
            .map_err(|error| {
                let mapped = from_bootc(&error);
                ApexError::new(
                    mapped.kind().unwrap_or(apex_dbus::ErrorKind::Bootc),
                    format!(
                        "Apex {from_major} could not be kept, so Apex {to_major} was not \
                         installed; its download stays locked. {}",
                        mapped.message()
                    ),
                )
            })?;
        let Some((entry, ostree)) = booted.and_then(|e| Some((e, e.ostree.as_ref()?))) else {
            return Ok(());
        };
        let record = MajorPinRecord {
            checksum: ostree.checksum.clone(),
            serial: ostree.deploy_serial,
            digest: entry.digest().unwrap_or_default().to_owned(),
            from_major,
            to_major,
            at: self.now(),
        };
        self.update_major_upgrades(|record_file| record_file.pins.push(record));
        tracing::info!(
            digest = entry.digest(),
            "pinned the running version before the major upgrade"
        );
        Ok(())
    }

    async fn save_var(
        &self,
        from_major: u32,
        to_major: u32,
        cancel: &CancelToken,
    ) -> Result<(), ApexError> {
        let failed = |why: String| {
            ApexError::Internal(format!(
                "/var could not be saved, so Apex {to_major} was not installed. Apex \
                 {from_major} stays pinned ({}), and the download stays locked. {why}",
                super::major_pin_reason(to_major)
            ))
        };
        self.providers.var.create_snapshot_dir().map_err(failed)?;
        let at = self.now();
        self.marking_bootc_busy(self.bootc.snapshot_var(from_major, at, cancel))
            .await
            .map_err(|error| failed(from_bootc(&error).message()))?;
        let path = apex_bootc::var_snapshot_path(from_major, at);
        tracing::info!(path = %path.display(), "saved /var before the major upgrade");
        self.update_major_upgrades(|record_file| {
            record_file.snapshots.push(SnapshotRecord {
                path: path.display().to_string(),
                major: from_major,
                at,
            });
        });
        Ok(())
    }

    /// The pin and the snapshot exist whether or not the record is saved.
    fn update_major_upgrades(&self, change: impl FnOnce(&mut MajorUpgrades)) {
        let saved = self
            .store
            .read_major_upgrades()
            .and_then(|mut record_file| {
                change(&mut record_file);
                unflushed_is_done(
                    self.store.write_major_upgrades(&record_file),
                    "saving major-upgrade.json",
                )?;
                self.lock().major_upgrades = record_file;
                Ok(())
            });
        if let Err(error) = saved {
            tracing::error!(error = %errors::with_sources(&error), "cannot record the major upgrade");
        }
    }
}
