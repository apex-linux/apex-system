use std::ops::RangeInclusive;

pub const MAX_CHARS: usize = 2048;

const BIDI_EMBEDDINGS_AND_OVERRIDES: RangeInclusive<char> = '\u{202a}'..='\u{202e}';
const BIDI_ISOLATES: RangeInclusive<char> = '\u{2066}'..='\u{2069}';

/// A NUL in a D-Bus string breaks zbus clients' decoding, and control and bidi characters from a
/// registry would rewrite the line `apexctl` prints. U+FFFD keeps visible that something was there.
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

fn unsafe_char(c: char) -> bool {
    c.is_control() || BIDI_EMBEDDINGS_AND_OVERRIDES.contains(&c) || BIDI_ISOLATES.contains(&c)
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
