pub const MAX_RADIUS: u32 = 20;

pub const IMPORT_LINE: &str = "@import url(\"apex.css\");";

const FILE_NAME: &str = "apex.css";

const BOM: char = '\u{feff}';

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

/// The `@import` target of one line, without `url()` and quotes.
fn import_target(line: &str) -> Option<&str> {
    let rule = line.trim().strip_prefix("@import")?.trim_start();
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

pub fn imports_apex_css(gtk_css: &str) -> bool {
    gtk_css
        .lines()
        .any(|line| import_target(line.trim_start_matches(BOM)) == Some(FILE_NAME))
}

/// `gtk_css` with the import added in front of every rule, or `None` when it is there.
/// Nothing of the user's text moves: only a BOM and an `@charset` rule stay ahead of it.
pub fn with_import(gtk_css: &str) -> Option<String> {
    if imports_apex_css(gtk_css) {
        return None;
    }
    let newline = if gtk_css.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let (bom, body) = match gtk_css.strip_prefix(BOM) {
        Some(body) => (&gtk_css[..BOM.len_utf8()], body),
        None => ("", gtk_css),
    };
    let charset_len = if body.starts_with("@charset") {
        body.find('\n').map_or(body.len(), |end| end + 1)
    } else {
        0
    };
    let (charset, rest) = body.split_at(charset_len);
    let charset_newline = if charset.is_empty() || charset.ends_with('\n') {
        ""
    } else {
        newline
    };
    Some(format!(
        "{bom}{charset}{charset_newline}{IMPORT_LINE}{newline}{rest}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(with_import("").unwrap(), "@import url(\"apex.css\");\n");
    }

    #[test]
    fn the_users_rules_stay_as_they_are_below_the_import() {
        let user = "/* mine */\nwindow { color: red; }\n@import url(\"other.css\");";
        assert_eq!(
            with_import(user).unwrap(),
            format!("@import url(\"apex.css\");\n{user}")
        );
    }

    #[test]
    fn an_import_that_is_there_is_left_alone() {
        for present in [
            "@import url(\"apex.css\");\n",
            "window {}\n  @import url('apex.css');  \n",
            "@import \"apex.css\";",
            "@import url(apex.css);\r\n",
            "\u{feff}@import 'apex.css';\n",
        ] {
            assert!(imports_apex_css(present), "{present:?}");
            assert_eq!(with_import(present), None, "{present:?}");
        }
        for absent in [
            "@import url(\"apex.css.bak\");",
            "@import url(\"other/apex.css\");",
            "/* apex.css */",
            "@import url(\"apex.css\")",
        ] {
            assert!(!imports_apex_css(absent), "{absent:?}");
        }
    }

    #[test]
    fn crlf_files_keep_crlf() {
        let user = "window { color: red; }\r\nlabel {}\r\n";
        assert_eq!(
            with_import(user).unwrap(),
            format!("@import url(\"apex.css\");\r\n{user}")
        );
    }

    #[test]
    fn a_bom_and_a_charset_stay_first() {
        assert_eq!(
            with_import("\u{feff}@charset \"UTF-8\";\nwindow {}\n").unwrap(),
            "\u{feff}@charset \"UTF-8\";\n@import url(\"apex.css\");\nwindow {}\n"
        );
        assert_eq!(
            with_import("@charset \"UTF-8\";").unwrap(),
            "@charset \"UTF-8\";\n@import url(\"apex.css\");\n"
        );
    }

    #[test]
    fn adding_twice_adds_once() {
        let once = with_import("label {}\n").unwrap();
        assert_eq!(with_import(&once), None);
        assert_eq!(once.matches("apex.css").count(), 1);
    }
}
