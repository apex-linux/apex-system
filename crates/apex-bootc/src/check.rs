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
        /// Bytes of new layers, as bootc rounded them for display.
        size: Option<u64>,
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
    let lines: Vec<&str> = lines.collect();
    let (version, digest) = version_and_digest(lines.iter().copied());
    let size = lines
        .iter()
        .find_map(|line| line.strip_prefix("Total new layers:"))
        .and_then(|rest| rest.split_once("Size:"))
        .and_then(|(_, size)| parse_size(size));
    CheckOutcome::Available {
        version,
        digest,
        size,
    }
}

/// glib's `format_size`, which bootc prints: SI units by default, IEC with a flag.
fn parse_size(text: &str) -> Option<u64> {
    let (number, unit) = text.trim().split_once(char::is_whitespace)?;
    let number: f64 = number.replace(',', ".").parse().ok()?;
    let scale: f64 = match unit.trim() {
        "byte" | "bytes" | "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1_048_576.0,
        "GiB" => 1_073_741_824.0,
        "TiB" => 1_099_511_627_776.0,
        _ => return None,
    };
    let bytes = (number * scale).round();
    (0.0..1.8e19).contains(&bytes).then(|| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "checked to be a non-negative integer below u64::MAX"
        )]
        let bytes = bytes as u64;
        bytes
    })
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
                size: Some(1_200_000_000),
            }
        );
    }

    #[test]
    fn available_without_version_or_valid_digest() {
        assert_eq!(
            parse_check_output("Update available for: x\n  Digest: sha256:nothex\n"),
            CheckOutcome::Available {
                version: None,
                digest: None,
                size: None,
            }
        );
    }

    #[test]
    fn sizes_in_glib_units() {
        assert_eq!(parse_size("1.2 GB"), Some(1_200_000_000));
        assert_eq!(parse_size(" 512 bytes"), Some(512));
        assert_eq!(parse_size("1 byte"), Some(1));
        assert_eq!(parse_size("830.5 kB"), Some(830_500));
        assert_eq!(parse_size("1,5 MB"), Some(1_500_000), "a decimal comma");
        assert_eq!(parse_size("2.0 GiB"), Some(2_147_483_648));
        assert_eq!(parse_size("1.2"), None);
        assert_eq!(parse_size("lots GB"), None);
        assert_eq!(parse_size("-1 GB"), None);
        assert_eq!(parse_size("3 parsecs"), None);
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
