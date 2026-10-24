use apex_dbus::Hold;

/// Written by apex-lane into every image (apex-image's packaging/README.md, "rpms.lock").
pub const LANE_LOCK_PATH: &str = "/usr/share/apex/rpms.lock";

/// The lock's `family NAME held KEY [since=UTC] [limit=UTC] [newer=NVR,...]
/// [security=0|1|?] [reason=TEXT]` lines; `reason` runs to the end of the line.
pub fn holds(lock: &str) -> Vec<Hold> {
    lock.lines().filter_map(hold).collect()
}

fn hold(line: &str) -> Option<Hold> {
    let mut words = line.split(' ').filter(|word| !word.is_empty());
    if words.next()? != "family" {
        return None;
    }
    let family = words.next()?;
    if words.next()? != "held" {
        return None;
    }
    words.next()?;
    let mut hold = Hold {
        family: family.to_owned(),
        ..Hold::default()
    };
    for word in words {
        let Some((key, value)) = word.split_once('=') else {
            continue;
        };
        match key {
            "since" => {
                hold.since = apex_bootc::parse_rfc3339_unix(value)
                    .and_then(|seconds| u64::try_from(seconds).ok())
                    .unwrap_or(0);
            }
            "newer" => value.clone_into(&mut hold.newer),
            "security" => hold.security = value == "1",
            "reason" => break,
            _ => {}
        }
    }
    Some(hold)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = "\
# apex rpms.lock 1: written by apex-lane (packaging/README.md, \"rpms.lock\")
#   family NAME STATE KEY [since=UTC] [limit=UTC] [newer=NVR,...] [security=0|1|?] [reason=TEXT]
family libfprint degraded - since=2026-10-01T00:00:00Z reason=y
family gnome-shell held gnome-shell-51.0-1.fc45-s1111111111aa since=2026-10-12T04:10:00Z \
limit=2026-10-26T04:10:00Z newer=gnome-shell-51.1-1.fc45,mutter-51.1-1.fc45 security=1 \
reason=0002-loader.patch does not apply to newer=x security=0
family nautilus held k1 since=2026-10-12T04:10:00Z reason=the Apex inputs themselves fail
family ptyxis held k2 security=? since=yesterday
family libadwaita built libadwaita-1.8-1.fc45-s22
rpm gnome-shell built gnome-shell gnome-shell-0:51.0-1.fc45.apex1.x86_64 aa pkgs/x.rpm
";

    #[test]
    fn held_families_with_what_the_lane_knew() {
        let holds = holds(LOCK);
        let families: Vec<&str> = holds.iter().map(|h| h.family.as_str()).collect();
        assert_eq!(families, ["gnome-shell", "nautilus", "ptyxis"]);
        assert_eq!(
            holds[0],
            Hold {
                family: "gnome-shell".to_owned(),
                newer: "gnome-shell-51.1-1.fc45,mutter-51.1-1.fc45".to_owned(),
                since: 1_791_778_200,
                security: true,
            },
            "the reason's own words are not fields"
        );
        assert_eq!(holds[1].newer, "", "no newer build is known");
        assert_eq!(holds[1].since, 1_791_778_200);
        assert!(!holds[1].security);
        assert_eq!((holds[2].since, holds[2].security), (0, false), "unknowns");
    }

    #[test]
    fn a_lock_without_holds() {
        assert!(holds("").is_empty());
        assert!(holds("family gnome-shell held\n").is_empty(), "no key");
    }
}
