use crate::check::version_and_digest;
use crate::digest::Digest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadOutcome {
    /// The tag's image replaced any earlier stage.
    Staged {
        /// `false` only if a future bootc stops honouring `--download-only`.
        locked: bool,
        version: Option<String>,
        digest: Option<Digest>,
    },
    Relocked,
    StageUnchanged,
    /// The tag points at the booted image: nothing changed, so an applied stage stays applied.
    NoUpdate,
    Unknown,
}

impl DownloadOutcome {
    /// A caller that needs the stage locked must treat `false` as failure, and still confirm
    /// the lock with `bootc status` before relying on `true`.
    pub fn stage_locked(&self) -> bool {
        matches!(
            self,
            Self::Relocked | Self::StageUnchanged | Self::Staged { locked: true, .. }
        )
    }
}

/// bootc exits 0 in every case and reports on stdout (bootc `crates/lib/src/cli.rs`
/// `upgrade` and `crates/lib/src/deploy.rs` `stage`, commit 41049ccada2b).
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
