//! Channels: which one the booted image follows, and the compile-time image reference
//! `SwitchChannel` switches to.
//!
//! The references are the `apex-bootc` constants for Fedora 45 (contract §1). The Fedora
//! release comes from the running system (its manifest, else os-release), never from the
//! client, and the table only has the releases this build knows, so a client can never
//! make the daemon follow anything but the two signed Apex repositories.

use apex_bootc::{APEX_STABLE, APEX_TESTING, APEX_TESTING_REPOSITORY, Host, ImageRef};
use apex_dbus::Channel;

/// The channel references of one Fedora release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRefs {
    /// The Fedora release, e.g. 45.
    pub fedora: u32,
    /// The stable (release-key) reference.
    pub stable: ImageRef,
    /// The testing (CI-key) reference.
    pub testing: ImageRef,
}

/// Every Fedora release this build can switch channels on. Major upgrades are a manual
/// runbook until Fedora 46 branches (ARCHITECTURE §5.2), so there is one row.
pub static CHANNEL_REFS: [ChannelRefs; 1] = [ChannelRefs {
    fedora: 45,
    stable: APEX_STABLE,
    testing: APEX_TESTING,
}];

/// The reference to switch to for `channel` on Fedora `fedora`, if this build knows it.
pub fn channel_target(channel: Channel, fedora: u32) -> Option<ImageRef> {
    let refs = CHANNEL_REFS.iter().find(|refs| refs.fedora == fedora)?;
    Some(match channel {
        Channel::Stable => refs.stable.clone(),
        Channel::Testing => refs.testing.clone(),
    })
}

/// Whether `image` is the channel reference of a known Fedora release (what
/// `SwitchChannel` switches to).
pub fn is_channel_reference(image: &str) -> bool {
    CHANNEL_REFS
        .iter()
        .any(|refs| refs.stable.to_string() == image || refs.testing.to_string() == image)
}

/// Whether the staged deployment comes from a channel switch: it follows a channel
/// reference, and a different one than the booted deployment.
///
/// `bootc upgrade` keeps the booted deployment's reference, so only `bootc switch` makes
/// them differ: `SwitchChannel` (polkit `switch-channel`, `auth_admin`), or root running
/// bootc by hand. `Apply` relies on that: it installs such a stage even when it holds the
/// running build or an older one.
pub fn is_channel_switch(host: &Host) -> bool {
    let booted = host
        .booted()
        .and_then(apex_bootc::BootEntry::image_reference);
    let staged = host
        .staged()
        .and_then(apex_bootc::BootEntry::image_reference);
    match (booted, staged) {
        (Some(booted), Some(staged)) => booted != staged && is_channel_reference(staged),
        _ => false,
    }
}

/// The repository part of an image reference: without `@digest` and without the `:tag` of
/// the last path component (a `:` before the first `/` is a registry port).
pub fn repository_of(image: &str) -> &str {
    let without_digest = image.split_once('@').map_or(image, |(repo, _)| repo);
    let last_slash = without_digest.rfind('/').map_or(0, |i| i + 1);
    match without_digest[last_slash..].find(':') {
        Some(colon) => &without_digest[..last_slash + colon],
        None => without_digest,
    }
}

/// The channel an image reference belongs to.
///
/// The contract has two values, so anything that is not the testing repository reports
/// `stable`. A foreign reference cannot be updated anyway: the strict signature policy
/// rejects every repository but the two Apex ones, and the `Booted` property shows the
/// actual reference.
pub fn channel_of_image(image: &str) -> Channel {
    if repository_of(image) == APEX_TESTING_REPOSITORY {
        Channel::Testing
    } else {
        Channel::Stable
    }
}

/// `VERSION_ID` of an os-release file, as a number (`45`, `"45"`); `None` when missing or
/// not a plain integer (for example `rawhide`).
pub fn os_release_version_id(os_release: &str) -> Option<u32> {
    os_release.lines().find_map(|line| {
        let value = line.trim().strip_prefix("VERSION_ID=")?;
        let value = value.trim_matches(|c| c == '"' || c == '\'');
        value.parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn repositories() {
        let cases = [
            ("ghcr.io/sekirokenjii/apex:45", "ghcr.io/sekirokenjii/apex"),
            (
                "ghcr.io/sekirokenjii/apex-testing:45",
                "ghcr.io/sekirokenjii/apex-testing",
            ),
            (
                "ghcr.io/sekirokenjii/apex-testing@sha256:00",
                "ghcr.io/sekirokenjii/apex-testing",
            ),
            (
                "ghcr.io/sekirokenjii/apex:45@sha256:00",
                "ghcr.io/sekirokenjii/apex",
            ),
            ("localhost:5000/apex", "localhost:5000/apex"),
            ("localhost:5000/apex:dev", "localhost:5000/apex"),
            ("apex", "apex"),
            ("", ""),
        ];
        for (image, repo) in cases {
            assert_eq!(repository_of(image), repo, "{image}");
        }
    }

    #[test]
    fn channels() {
        assert_eq!(
            channel_of_image("ghcr.io/sekirokenjii/apex:45"),
            Channel::Stable
        );
        assert_eq!(
            channel_of_image("ghcr.io/sekirokenjii/apex-testing:45"),
            Channel::Testing
        );
        assert_eq!(
            channel_of_image("ghcr.io/sekirokenjii/apex-testing:sha-0123456789ab-7"),
            Channel::Testing
        );
        assert_eq!(
            channel_of_image("ghcr.io/sekirokenjii/apex-testing-evil:45"),
            Channel::Stable
        );
    }

    fn following(image: &str) -> apex_bootc::BootEntry {
        apex_bootc::BootEntry {
            image: Some(apex_bootc::ImageStatus {
                image: apex_bootc::ImageReference {
                    image: image.into(),
                    transport: "registry".into(),
                    signature: None,
                },
                version: None,
                timestamp: None,
                image_digest: "sha256:d".into(),
                architecture: "amd64".into(),
            }),
            ..apex_bootc::BootEntry::default()
        }
    }

    fn host(booted: &str, staged: Option<&str>) -> Host {
        let mut host = Host::default();
        host.status.booted = Some(following(booted));
        host.status.staged = staged.map(following);
        host
    }

    #[test]
    fn channel_switch_stages() {
        let stable = APEX_STABLE.to_string();
        let testing = APEX_TESTING.to_string();
        assert!(is_channel_switch(&host(&stable, Some(&testing))));
        assert!(is_channel_switch(&host(&testing, Some(&stable))));
        // An ordinary upgrade keeps the reference.
        assert!(!is_channel_switch(&host(&stable, Some(&stable))));
        assert!(!is_channel_switch(&host(&stable, None)));
        // Only the two channel references count, not anything root switched to by hand.
        assert!(!is_channel_switch(&host(
            &stable,
            Some("ghcr.io/sekirokenjii/apex-testing:sha-0123456789ab-7")
        )));
        assert!(!is_channel_switch(&host(
            &stable,
            Some("quay.io/other/os:45")
        )));
        assert!(is_channel_reference(&stable));
        assert!(!is_channel_reference("ghcr.io/sekirokenjii/apex:46"));
    }

    #[test]
    fn targets_are_the_compile_time_constants() {
        assert_eq!(
            channel_target(Channel::Stable, 45).map(|r| r.to_string()),
            Some("ghcr.io/sekirokenjii/apex:45".to_owned())
        );
        assert_eq!(
            channel_target(Channel::Testing, 45).map(|r| r.to_string()),
            Some("ghcr.io/sekirokenjii/apex-testing:45".to_owned())
        );
        assert_eq!(channel_target(Channel::Stable, 46), None);
        assert_eq!(channel_target(Channel::Testing, 0), None);
        for refs in &CHANNEL_REFS {
            assert!(
                refs.stable
                    .to_string()
                    .ends_with(&format!(":{}", refs.fedora))
            );
            assert!(
                refs.testing
                    .to_string()
                    .ends_with(&format!(":{}", refs.fedora))
            );
            assert_eq!(
                channel_of_image(&refs.testing.to_string()),
                Channel::Testing
            );
            assert_eq!(channel_of_image(&refs.stable.to_string()), Channel::Stable);
        }
    }

    #[test]
    fn os_release() {
        assert_eq!(
            os_release_version_id("NAME=\"Fedora Linux\"\nVERSION_ID=45\nID=fedora\n"),
            Some(45)
        );
        assert_eq!(os_release_version_id("VERSION_ID=\"45\""), Some(45));
        assert_eq!(os_release_version_id("VERSION_ID=rawhide"), None);
        assert_eq!(os_release_version_id("ID=fedora"), None);
        assert_eq!(os_release_version_id("VARIANT_VERSION_ID=45"), None);
    }

    proptest! {
        #[test]
        fn repository_is_a_prefix(image in "[a-z0-9.:/@-]{0,40}") {
            let repo = repository_of(&image);
            prop_assert!(image.starts_with(repo));
            prop_assert!(!repo.contains('@'));
        }
    }
}
