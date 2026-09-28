//! Result of `bootc upgrade --check`.
//!
//! bootc exits 0 whether or not an update exists and reports the result on stdout
//! (bootc `crates/lib/src/cli.rs`, `upgrade`, the `opts.check` branch):
//!
//! ```text
//! No changes in: <image reference>
//! ```
//!
//! or
//!
//! ```text
//! Update available for: <image reference>
//!   Version: <version>          (only when the image has one)
//!   Digest: sha256:<hex>
//! <manifest diff>
//! ```
//!
//! # What bootc compares
//!
//! The manifest the channel tag points at now is compared with the image **last pulled**
//! for that image reference (ostree-ext `ImageImporter::prepare_internal`, which reads the
//! reference's ostree ref through `try_query_image`), not with the booted or the staged
//! deployment. The last pulled image is usually the staged one, or the booted one when
//! nothing was pulled since it was staged. It can also be a download-only stage that a
//! reboot dropped and that no deployment uses any more.
//!
//! # Where the result is
//!
//! The digest printed after `Update available for:` is the primary source. bootc also
//! stores the manifest and config it found as detached metadata on the last pulled
//! image's commit (ostree-ext `cache_pending`), and `bootc status` shows it as that
//! deployment's `cachedUpdate`. That copy is written only when an update is found, is
//! not cleared by later checks or downloads, and lives on whichever commit the ref
//! pointed at, so it is often stale or missing. Use it only for the metadata of a digest learned here
//! ([`crate::Host::cached_update_for`]), never as the remote image itself.

use crate::digest::Digest;

/// What `bootc upgrade --check` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// `No changes in:` the channel tag's image is already in the local ostree
    /// repository, because it is the image last pulled for this reference: normally the
    /// booted or staged image, but possibly a download-only stage that a reboot dropped.
    ///
    /// bootc prints no digest here, and nothing in `bootc status` says which image that
    /// was. The caller has to remember the digest of its last successful check or
    /// download; without that record the remote image is unknown.
    UpToDate,
    /// `Update available for:` the tag's image differs from the one last pulled for this
    /// reference. It is not necessarily new to this host (the tag may have moved back to
    /// the booted image), so compare the digest with the booted and staged digests.
    Available {
        /// Its version, if the image carries one.
        version: Option<String>,
        /// Its manifest digest, if it was printed in the canonical form.
        digest: Option<Digest>,
    },
    /// The output matched neither form (a future bootc changed the wording).
    Unknown,
}

/// Parses the stdout of `bootc upgrade --check`.
pub fn parse_check_output(stdout: &str) -> CheckOutcome {
    let mut lines = stdout.lines().map(str::trim);
    let Some(first) = lines.by_ref().find(|l| !l.is_empty()) else {
        return CheckOutcome::Unknown;
    };
    if first.starts_with("No changes in:") {
        return CheckOutcome::UpToDate;
    }
    if !first.starts_with("Update available for:") {
        return CheckOutcome::Unknown;
    }
    let (version, digest) = version_and_digest(lines);
    CheckOutcome::Available { version, digest }
}

/// The first `Version:` and the first well-formed `Digest:` among trimmed `lines`, as
/// bootc prints them after announcing an image (`upgrade --check` and `deploy::stage`).
pub(crate) fn version_and_digest<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> (Option<String>, Option<Digest>) {
    let mut version = None;
    let mut digest = None;
    for line in lines {
        if let Some(v) = line.strip_prefix("Version:") {
            version.get_or_insert_with(|| v.trim().to_owned());
        } else if let Some(d) = line.strip_prefix("Digest:")
            && digest.is_none()
        {
            digest = Digest::parse(d.trim()).ok();
        }
    }
    (version, digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0";

    #[test]
    fn up_to_date() {
        assert_eq!(
            parse_check_output(
                "No changes in: ostree-image-signed:docker://ghcr.io/sekirokenjii/apex:45\n"
            ),
            CheckOutcome::UpToDate
        );
    }

    #[test]
    fn available_with_version_and_diff() {
        let out = format!(
            "Update available for: ostree-image-signed:docker://ghcr.io/sekirokenjii/apex:45\n  Version: 45.20260924.117\n  Digest: {D}\nTotal new layers: 3    Size: 1.2 GB\nRemoved layers:   2    Size: 1.1 GB\n"
        );
        assert_eq!(
            parse_check_output(&out),
            CheckOutcome::Available {
                version: Some("45.20260924.117".into()),
                digest: Some(Digest::parse(D).unwrap()),
            }
        );
    }

    #[test]
    fn available_without_version_or_valid_digest() {
        assert_eq!(
            parse_check_output("Update available for: x\n  Digest: sha256:nothex\n"),
            CheckOutcome::Available {
                version: None,
                digest: None
            }
        );
    }

    #[test]
    fn unknown_output() {
        assert_eq!(parse_check_output(""), CheckOutcome::Unknown);
        assert_eq!(
            parse_check_output("\n\nSomething else\n"),
            CheckOutcome::Unknown
        );
    }
}
