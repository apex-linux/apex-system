//! Result of `bootc upgrade --download-only`.
//!
//! bootc exits 0 in every case below and reports on stdout what it did (bootc
//! `crates/lib/src/cli.rs`, `upgrade`, and `crates/lib/src/deploy.rs`, `stage`, on `main`
//! at commit `41049ccada2bf4468b9087d45832f9168d458ef2`). It always pulls the channel tag
//! first, which fetches the manifest from the registry, and then compares the pulled
//! manifest digest with the staged image and then with the booted image:
//!
//! | Pulled image | stdout | Effect | Outcome |
//! |---|---|---|---|
//! | the staged image, stage unlocked | `Image downloaded, but will not be applied on reboot` | stage locked again | [`DownloadOutcome::Relocked`] |
//! | the staged image, stage locked | `Staged update present, not changed` | none | [`DownloadOutcome::StageUnchanged`] |
//! | the booted image | `No update available.` | none: an existing stage is left as it was, **locked or unlocked** | [`DownloadOutcome::NoUpdate`] |
//! | another image | `Staged but not queued for next boot: <ref>`, then `  Version:` and `  Digest:` | that image is staged locked and replaces any stage | [`DownloadOutcome::Staged`] |
//!
//! So re-locking an applied (unlocked) stage with this command is not guaranteed. It
//! works only while the tag still points at the staged digest. It needs the registry, so
//! it fails offline. If the tag has moved to a third image, it downloads and stages that
//! whole image instead. If the tag has moved back to the booted image, it changes nothing
//! and still exits 0.

use crate::check::version_and_digest;
use crate::digest::Digest;

/// What `bootc upgrade --download-only` did, from its stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadOutcome {
    /// A new staged deployment was created from the tag's image and replaced any earlier
    /// stage (`Staged but not queued for next boot:`, or `Queued for next boot:` if a
    /// future bootc stopped honouring `--download-only`).
    Staged {
        /// The new stage is locked (download-only) and will not be finalized at shutdown.
        locked: bool,
        /// Its version, if the image carries one.
        version: Option<String>,
        /// Its manifest digest, if it was printed in the canonical form.
        digest: Option<Digest>,
    },
    /// The tag still points at the staged image, which was unlocked: bootc locked it
    /// again (`Image downloaded, but will not be applied on reboot`).
    Relocked,
    /// The tag still points at the staged image, which was already locked
    /// (`Staged update present, not changed`).
    StageUnchanged,
    /// The tag points at the booted image (`No update available.`). Nothing was staged
    /// or changed; an existing stage keeps its lock state, so an applied stage **stays
    /// applied**.
    NoUpdate,
    /// None of the lines above was found (a future bootc changed the wording).
    Unknown,
}

impl DownloadOutcome {
    /// Whether bootc reports that a staged deployment exists and is locked after the
    /// command: [`Self::Relocked`], [`Self::StageUnchanged`] and a locked
    /// [`Self::Staged`].
    ///
    /// `false` for [`Self::NoUpdate`] (an existing stage was not touched and may still be
    /// unlocked) and [`Self::Unknown`]. A caller that needs the stage locked, such as
    /// cancelling an applied update, must treat `false` as failure, and should still
    /// confirm the lock with `bootc status` before relying on it.
    pub fn stage_locked(&self) -> bool {
        matches!(
            self,
            Self::Relocked | Self::StageUnchanged | Self::Staged { locked: true, .. }
        )
    }
}

/// Parses the stdout of `bootc upgrade --download-only`.
///
/// Lines before the result (layer status, `No changes in … => …` from the pull, fetch
/// statistics) and the manifest diff after it are ignored.
pub fn parse_download_output(stdout: &str) -> DownloadOutcome {
    let mut lines = stdout.lines().map(str::trim);
    while let Some(line) = lines.next() {
        match line {
            "Image downloaded, but will not be applied on reboot" => {
                return DownloadOutcome::Relocked;
            }
            "Staged update present, not changed" => return DownloadOutcome::StageUnchanged,
            "No update available." => return DownloadOutcome::NoUpdate,
            _ => {}
        }
        let locked = if line.starts_with("Staged but not queued for next boot:") {
            true
        } else if line.starts_with("Queued for next boot:") {
            false
        } else {
            continue;
        };
        let (version, digest) = version_and_digest(lines);
        return DownloadOutcome::Staged {
            locked,
            version,
            digest,
        };
    }
    DownloadOutcome::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:0b4c83fba9a0e3e2e5a7e9dbb5f4c1f1d7b5d1c6e0a3f9e8d7c6b5a4f3e2d1c0";

    /// stdout of a pull that fetched layers and staged a new image, as bootc prints it
    /// (`print_layer_status`, `Fetched layers`, `deploy::stage`, then the manifest diff).
    fn new_stage(prefix: &str) -> String {
        format!(
            "layers already present: 2; layers needed: 3 (300.0 MB)\n\
             Fetched layers: 300.0 MiB in 41 seconds (7.3 MiB/s)\n\
             {prefix} ghcr.io/sekirokenjii/apex:45\n  Version: 45.20260924.117\n  Digest: {D}\n\
             Total new layers: 5    Size: 350.0 MB\nRemoved layers:   3    Size: 300.0 MB\n\
             Added layers:     3    Size: 300.0 MB\n"
        )
    }

    #[test]
    fn a_new_locked_stage() {
        let outcome = parse_download_output(&new_stage("Staged but not queued for next boot:"));
        assert_eq!(
            outcome,
            DownloadOutcome::Staged {
                locked: true,
                version: Some("45.20260924.117".into()),
                digest: Some(Digest::parse(D).unwrap()),
            }
        );
        assert!(outcome.stage_locked());
    }

    #[test]
    fn a_new_unlocked_stage_is_not_locked() {
        let outcome = parse_download_output(&new_stage("Queued for next boot:"));
        assert!(matches!(
            outcome,
            DownloadOutcome::Staged { locked: false, .. }
        ));
        assert!(!outcome.stage_locked());
    }

    #[test]
    fn the_staged_image_again() {
        let relocked = parse_download_output(&format!(
            "No changes in ghcr.io/sekirokenjii/apex:45 => {D}\n\
             Image downloaded, but will not be applied on reboot\n"
        ));
        assert_eq!(relocked, DownloadOutcome::Relocked);
        assert!(relocked.stage_locked());

        let unchanged = parse_download_output(&format!(
            "No changes in ghcr.io/sekirokenjii/apex:45 => {D}\nStaged update present, not changed\n"
        ));
        assert_eq!(unchanged, DownloadOutcome::StageUnchanged);
        assert!(unchanged.stage_locked());
    }

    /// The tag points at the booted image again: bootc leaves an applied stage applied
    /// and still exits 0, so this must never count as locked.
    #[test]
    fn the_booted_image_leaves_the_stage_alone() {
        let outcome = parse_download_output(
            "layers already present: 5; layers needed: 0 (0 bytes)\n\
             Fetched layers: 0 B in 0 seconds (0 B/s)\nNo update available.\n",
        );
        assert_eq!(outcome, DownloadOutcome::NoUpdate);
        assert!(!outcome.stage_locked());
    }

    #[test]
    fn unrecognised_output_is_not_locked() {
        for stdout in ["", "\n\n", "Something else\n", "Image downloaded\n"] {
            let outcome = parse_download_output(stdout);
            assert_eq!(outcome, DownloadOutcome::Unknown, "{stdout:?}");
            assert!(!outcome.stage_locked());
        }
    }
}
