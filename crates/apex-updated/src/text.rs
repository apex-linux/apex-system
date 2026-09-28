//! Text from outside the daemon, made fit for D-Bus strings and terminals.
//!
//! Error messages and string properties carry text the daemon does not control: bootc's
//! stderr (which relays a registry's JSON error messages before any signature check), image
//! labels, and the state files. D-Bus forbids NUL inside a string: zvariant writes it but
//! refuses to read it back, so a single NUL makes a zbus client unable to decode the reply
//! or the property, and `GLib` clients cut the string short. Other control characters (CR,
//! BEL, backspace, ESC and the C1 introducer U+009B) let that text rewrite the terminal line
//! `apexctl` prints it on, and the bidirectional overrides reorder what is shown.
//!
//! [`display_text`] is applied to every error reply (`service::reply`) and every published
//! property ([`crate::properties::Properties::sanitized`]), so it holds whatever the source.

/// Longest text published, in characters; longer text is cut and ends with `…`.
pub const MAX_CHARS: usize = 2048;

/// `text` without control characters, bidirectional formatting characters, or more than
/// [`MAX_CHARS`] characters.
///
/// Tab, line feed, carriage return and the other whitespace controls become a space; any
/// other control character (C0, DEL, C1) and the bidirectional embeddings, overrides and
/// isolates become U+FFFD, so it stays visible that something was there. Idempotent.
pub fn display_text(text: &str) -> String {
    let clean = !text.chars().any(unsafe_char) && text.chars().nth(MAX_CHARS).is_none();
    if clean {
        return text.to_owned();
    }
    let mut out: String = text
        .chars()
        .take(MAX_CHARS)
        .map(|c| match c {
            c if c.is_control() && c.is_whitespace() => ' ',
            c if unsafe_char(c) => char::REPLACEMENT_CHARACTER,
            c => c,
        })
        .collect();
    if text.chars().nth(MAX_CHARS).is_some() {
        out.pop();
        out.push('…');
    }
    out
}

/// Control characters and the bidirectional formatting characters (embeddings and
/// overrides U+202A–U+202E, isolates U+2066–U+2069).
fn unsafe_char(c: char) -> bool {
    c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn plain_text_is_unchanged() {
        let text =
            "Network error: dial tcp 140.82.114.34:443: connect: network is unreachable — café";
        assert_eq!(display_text(text), text);
        assert_eq!(display_text(""), "");
    }

    #[test]
    fn controls_are_removed() {
        assert_eq!(
            display_text("x\0y\rz\u{9b}2J\u{7}\u{8}\u{1b}[31mred\ttab\nline\u{7f}"),
            "x\u{fffd}y z\u{fffd}2J\u{fffd}\u{fffd}\u{fffd}[31mred tab line\u{fffd}"
        );
        assert_eq!(display_text("abc\u{202e}fed"), "abc\u{fffd}fed");
        assert_eq!(display_text("\u{2066}x\u{2069}"), "\u{fffd}x\u{fffd}");
    }

    #[test]
    fn long_text_is_cut() {
        let long = "a".repeat(MAX_CHARS + 10);
        let cut = display_text(&long);
        assert_eq!(cut.chars().count(), MAX_CHARS);
        assert!(cut.ends_with('…'));
        let exact = "b".repeat(MAX_CHARS);
        assert_eq!(display_text(&exact), exact);
    }

    proptest! {
        #[test]
        fn output_is_safe_and_stable(text in any::<String>()) {
            let once = display_text(&text);
            prop_assert!(!once.chars().any(unsafe_char));
            prop_assert!(once.chars().count() <= MAX_CHARS);
            prop_assert_eq!(display_text(&once), once.clone());
        }
    }
}
