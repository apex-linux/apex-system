pub const MAX_RADIUS: u32 = 20;

pub const IMPORT_LINE: &str = "@import url(\"apex.css\");";

const FILE_NAME: &str = "apex.css";

const BOM: char = '\u{feff}';

const BOM_BYTES: &[u8] = b"\xef\xbb\xbf";

/// A user-priority rule beats libadwaita's own, so windows that must stay square say so.
const SQUARE_WINDOWS: [&str; 8] = [
    "window.maximized",
    "window.fullscreen",
    "window.tiled",
    "window.tiled-top",
    "window.tiled-right",
    "window.tiled-bottom",
    "window.tiled-left",
    "window.solid-csd",
];

pub fn radius_css(radius: u32) -> String {
    let radius = radius.min(MAX_RADIUS);
    format!(
        "/* Written by apex-session-agent from window-corner-radius; edits are replaced. */\n\
         :root {{ --window-radius: {radius}px; }}\n\
         \n\
         {} {{\n  --window-radius: 0px;\n}}\n",
        SQUARE_WINDOWS.join(",\n")
    )
}

/// The `@import` target of one statement, without `url()` and quotes.
fn import_target(statement: &str) -> Option<&str> {
    let rule = statement.trim().strip_prefix("@import")?.trim_start();
    let rule = rule.strip_suffix(';')?.trim_end();
    let target = rule
        .strip_prefix("url(")
        .and_then(|r| r.strip_suffix(')'))
        .map_or(rule, str::trim);
    let unquoted = ['"', '\'']
        .iter()
        .find_map(|q| target.strip_prefix(*q).and_then(|t| t.strip_suffix(*q)));
    Some(unquoted.unwrap_or(target))
}

/// An unterminated comment runs to the end, as in CSS.
fn without_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        out.push(' ');
        rest = rest[start + 2..]
            .find("*/")
            .map_or("", |end| &rest[start + 2 + end + 2..]);
    }
    out.push_str(rest);
    out
}

/// Only the statements in front of the first rule count: an `@import` after a rule, or in a
/// comment, imports nothing.
pub fn imports_apex_css(gtk_css: &[u8]) -> bool {
    let text = without_comments(&String::from_utf8_lossy(gtk_css));
    let mut rest = text.trim_start_matches(BOM);
    loop {
        rest = rest.trim_start();
        if !rest.starts_with('@') {
            return false;
        }
        let Some(end) = rest
            .find([';', '{'])
            .filter(|&i| rest[i..].starts_with(';'))
        else {
            return false;
        };
        if import_target(&rest[..=end]) == Some(FILE_NAME) {
            return true;
        }
        rest = &rest[end + 1..];
    }
}

/// The import in front of every rule but a BOM and `@charset`, or `None` when it is there.
/// The other bytes are kept as they are, so a file in another encoding survives.
pub fn with_import(gtk_css: &[u8]) -> Option<Vec<u8>> {
    if imports_apex_css(gtk_css) {
        return None;
    }
    let newline: &[u8] = if gtk_css.windows(2).any(|w| w == b"\r\n") {
        b"\r\n"
    } else {
        b"\n"
    };
    let bom_len = if gtk_css.starts_with(BOM_BYTES) {
        BOM_BYTES.len()
    } else {
        0
    };
    let body = &gtk_css[bom_len..];
    let charset_len = if body.starts_with(b"@charset") {
        body.iter()
            .position(|&b| b == b'\n')
            .map_or(body.len(), |end| end + 1)
    } else {
        0
    };
    let (head, rest) = gtk_css.split_at(bom_len + charset_len);
    let mut out = Vec::with_capacity(gtk_css.len() + IMPORT_LINE.len() + 2);
    out.extend_from_slice(head);
    if charset_len > 0 && !head.ends_with(b"\n") {
        out.extend_from_slice(newline);
    }
    out.extend_from_slice(IMPORT_LINE.as_bytes());
    out.extend_from_slice(newline);
    out.extend_from_slice(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn added(gtk_css: &str) -> String {
        String::from_utf8(with_import(gtk_css.as_bytes()).expect("added")).unwrap()
    }

    #[test]
    fn the_radius_and_the_square_resets() {
        let css = radius_css(14);
        assert!(css.contains(":root { --window-radius: 14px; }\n"), "{css}");
        for selector in SQUARE_WINDOWS {
            assert!(css.contains(selector), "{selector}");
        }
        assert!(css.ends_with("{\n  --window-radius: 0px;\n}\n"), "{css}");
        assert_eq!(css.matches("--window-radius").count(), 2);
    }

    #[test]
    fn the_radius_is_clamped_to_the_schema_range() {
        assert!(radius_css(0).contains("--window-radius: 0px; }"));
        assert!(radius_css(20).contains("--window-radius: 20px; }"));
        assert!(radius_css(u32::MAX).contains("--window-radius: 20px; }"));
    }

    #[test]
    fn an_empty_file_gets_the_import_alone() {
        assert_eq!(added(""), "@import url(\"apex.css\");\n");
    }

    #[test]
    fn the_users_rules_stay_as_they_are_below_the_import() {
        let user = "/* mine */\nwindow { color: red; }\n@import url(\"other.css\");";
        assert_eq!(added(user), format!("@import url(\"apex.css\");\n{user}"));
    }

    #[test]
    fn a_leading_import_is_left_alone() {
        for present in [
            "@import url(\"apex.css\");\n",
            "  @import url('apex.css');  \nwindow {}\n",
            "@import \"apex.css\";",
            "@import url(apex.css);\r\n",
            "\u{feff}@import 'apex.css';\n",
            "@charset \"UTF-8\";\n@import url(\"mine.css\");\n/* x */ @import url(\"apex.css\");",
        ] {
            assert!(imports_apex_css(present.as_bytes()), "{present:?}");
            assert_eq!(with_import(present.as_bytes()), None, "{present:?}");
        }
    }

    #[test]
    fn an_import_in_a_comment_or_after_a_rule_imports_nothing() {
        for absent in [
            "/* @import url(\"apex.css\"); */\nwindow {}\n",
            "/* unterminated @import url(\"apex.css\");",
            "window {}\n@import url(\"apex.css\");\n",
            "@media screen { }\n@import url(\"apex.css\");\n",
            "@import url(\"apex.css.bak\");",
            "@import url(\"other/apex.css\");",
            "@import url(\"apex.css\")",
        ] {
            assert!(!imports_apex_css(absent.as_bytes()), "{absent:?}");
        }
        assert_eq!(
            added("/* @import url(\"apex.css\"); */\n"),
            "@import url(\"apex.css\");\n/* @import url(\"apex.css\"); */\n"
        );
    }

    #[test]
    fn crlf_files_keep_crlf() {
        let user = "window { color: red; }\r\nlabel {}\r\n";
        assert_eq!(added(user), format!("@import url(\"apex.css\");\r\n{user}"));
    }

    #[test]
    fn a_bom_and_a_charset_stay_first() {
        assert_eq!(
            added("\u{feff}@charset \"UTF-8\";\nwindow {}\n"),
            "\u{feff}@charset \"UTF-8\";\n@import url(\"apex.css\");\nwindow {}\n"
        );
        assert_eq!(
            added("@charset \"UTF-8\";"),
            "@charset \"UTF-8\";\n@import url(\"apex.css\");\n"
        );
    }

    #[test]
    fn bytes_in_another_encoding_are_kept() {
        let latin1 = b"/* caf\xe9 */\nwindow { color: red; }\n";
        let edited = with_import(latin1).expect("added");
        assert_eq!(
            &edited[..=IMPORT_LINE.len()],
            b"@import url(\"apex.css\");\n"
        );
        assert_eq!(&edited[IMPORT_LINE.len() + 1..], latin1);
    }

    #[test]
    fn adding_twice_adds_once() {
        let once = with_import(b"label {}\n").unwrap();
        assert_eq!(with_import(&once), None);
    }
}
