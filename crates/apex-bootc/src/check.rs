use crate::digest::Digest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The tag's image is the one last pulled for this reference (maybe a stage a reboot
    /// dropped). No digest is printed: the caller must remember its last check or download.
    UpToDate,
    /// The tag's image differs from the one last pulled, not necessarily from the booted or
    /// staged one: compare the digest with theirs.
    Available {
        version: Option<String>,
        digest: Option<Digest>,
    },
    Unknown,
}

/// bootc exits 0 either way and reports on stdout (bootc `crates/lib/src/cli.rs`, `upgrade`).
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
                "No changes in: ostree-image-signed:docker://ghcr.io/apex-linux/apex:45\n"
            ),
            CheckOutcome::UpToDate
        );
    }

    #[test]
    fn available_with_version_and_diff() {
        let out = format!(
            "Update available for: ostree-image-signed:docker://ghcr.io/apex-linux/apex:45\n  Version: 45.20260924.117\n  Digest: {D}\nTotal new layers: 3    Size: 1.2 GB\nRemoved layers:   2    Size: 1.1 GB\n"
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
