/// One deployment as `ostree admin status` lists it; `index` is what `ostree admin pin` takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OstreeDeployment {
    pub index: u32,
    pub checksum: String,
    pub serial: u32,
    pub pinned: bool,
}

/// ostree prints each deployment as `"%c %s %s.%d%s"` (marker, stateroot, checksum, serial,
/// state) followed by detail lines indented by four spaces (`ot-admin-builtin-status.c`).
pub fn parse_ostree_status(text: &str) -> Vec<OstreeDeployment> {
    let mut deployments: Vec<OstreeDeployment> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("* ").or_else(|| line.strip_prefix("  "))
            && !rest.starts_with(char::is_whitespace)
        {
            let mut words = rest.split_whitespace();
            let Some((checksum, serial)) = words.nth(1).and_then(|id| id.rsplit_once('.')) else {
                continue;
            };
            let (Ok(serial), Ok(index)) = (serial.parse(), u32::try_from(deployments.len())) else {
                continue;
            };
            deployments.push(OstreeDeployment {
                index,
                checksum: checksum.to_owned(),
                serial,
                pinned: false,
            });
        } else if line.trim() == "Pinned: yes"
            && let Some(last) = deployments.last_mut()
        {
            last.pinned = true;
        }
    }
    deployments
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "  default 2222222222222222222222222222222222222222222222222222222222222222.0 (finalization locked)
    Version: 45.20260924.3
    origin: <unknown origin type>
* default 1111111111111111111111111111111111111111111111111111111111111111.0
    Version: 45.20260920.1
    origin: <unknown origin type>
  default 3333333333333333333333333333333333333333333333333333333333333333.1 (rollback)
    Version: 45.20260918.1
    Pinned: yes
    origin: <unknown origin type>
  default 3333333333333333333333333333333333333333333333333333333333333333.0
    Pinned: yes
    GPG: Signature made Thu 24 Sep 2026
";

    #[test]
    fn indexes_count_every_deployment_the_stage_first() {
        let listed = parse_ostree_status(STATUS);
        let summary: Vec<(u32, &str, u32, bool)> = listed
            .iter()
            .map(|d| (d.index, &d.checksum[..2], d.serial, d.pinned))
            .collect();
        assert_eq!(
            summary,
            [
                (0, "22", 0, false),
                (1, "11", 0, false),
                (2, "33", 1, true),
                (3, "33", 0, true),
            ]
        );
    }

    #[test]
    fn nothing_listed() {
        assert!(parse_ostree_status("No deployments.\n").is_empty());
        assert!(parse_ostree_status("").is_empty());
        assert!(
            parse_ostree_status("  default nodot\n    Pinned: yes\n").is_empty(),
            "a detail line before any deployment"
        );
    }
}
