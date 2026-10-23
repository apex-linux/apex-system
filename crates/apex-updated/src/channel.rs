use apex_bootc::APEX_TESTING_REPOSITORY;
use apex_dbus::Channel;

/// Without `@digest` and `:tag`; a `:` before the last `/` belongs to a registry port.
pub fn repository_of(image: &str) -> &str {
    let without_digest = image.split_once('@').map_or(image, |(repo, _)| repo);
    let last_slash = without_digest.rfind('/').map_or(0, |i| i + 1);
    match without_digest[last_slash..].find(':') {
        Some(colon) => &without_digest[..last_slash + colon],
        None => without_digest,
    }
}

/// Anything but the testing repository is `stable`: the signature policy rejects foreign
/// repositories anyway, and `Booted` shows the actual reference.
pub fn channel_of_image(image: &str) -> Channel {
    if repository_of(image) == APEX_TESTING_REPOSITORY {
        Channel::Testing
    } else {
        Channel::Stable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn repositories() {
        let cases = [
            ("ghcr.io/apex-linux/apex:45", "ghcr.io/apex-linux/apex"),
            (
                "ghcr.io/apex-linux/apex-testing:45",
                "ghcr.io/apex-linux/apex-testing",
            ),
            (
                "ghcr.io/apex-linux/apex-testing@sha256:00",
                "ghcr.io/apex-linux/apex-testing",
            ),
            (
                "ghcr.io/apex-linux/apex:45@sha256:00",
                "ghcr.io/apex-linux/apex",
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
            channel_of_image("ghcr.io/apex-linux/apex:45"),
            Channel::Stable
        );
        assert_eq!(
            channel_of_image("ghcr.io/apex-linux/apex-testing:45"),
            Channel::Testing
        );
        assert_eq!(
            channel_of_image("ghcr.io/apex-linux/apex-testing:sha-0123456789ab-7"),
            Channel::Testing
        );
        assert_eq!(
            channel_of_image("ghcr.io/apex-linux/apex-testing-evil:45"),
            Channel::Stable
        );
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
