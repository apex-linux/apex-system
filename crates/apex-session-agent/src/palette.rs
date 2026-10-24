/// libadwaita's accent colours, drawn raw in light and dark (DESIGN.md).
const ACCENTS: [(&str, &str); 9] = [
    ("blue", "#3584e4"),
    ("teal", "#2190a4"),
    ("green", "#3a944a"),
    ("yellow", "#c88800"),
    ("orange", "#ed5b00"),
    ("red", "#e62d42"),
    ("pink", "#d56199"),
    ("purple", "#9141ac"),
    ("slate", "#6f8396"),
];

const DEFAULT_ACCENT: &str = ACCENTS[0].1;

const ON_ACCENT: &str = "#ffffff";

/// Ptyxis's stock `gnome` palette, so only the caret differs from a stock terminal.
const LIGHT: &str = "\
Foreground=#1d1d20
Background=#ffffff
TitlebarBackground=#ffffff
TitlebarForeground=#333333
Color0=#1d1d20
Color1=#c01c28
Color2=#26a269
Color3=#a2734c
Color4=#12488b
Color5=#a347ba
Color6=#2aa1b3
Color7=#cfcfcf
Color8=#5d5d5d
Color9=#f66151
Color10=#33d17a
Color11=#e9ad0c
Color12=#2a7bde
Color13=#c061cb
Color14=#33c7de
Color15=#ffffff
";

const DARK: &str = "\
Foreground=#ffffff
Background=#1c1c1f
TitlebarBackground=#2e2e32
TitlebarForeground=#ffffff
Color0=#241f31
Color1=#c01c28
Color2=#2ec27e
Color3=#f5c211
Color4=#1e78e4
Color5=#9841bb
Color6=#0ab9dc
Color7=#c0bfbc
Color8=#5e5c64
Color9=#ed333b
Color10=#57e389
Color11=#f8e45c
Color12=#51a1ff
Color13=#c061cb
Color14=#4fd2fd
Color15=#f6f5f4
";

/// An accent this list does not know, from a newer GNOME, falls back to GNOME's default.
pub fn accent_hex(accent: &str) -> &'static str {
    ACCENTS
        .iter()
        .find(|(name, _)| *name == accent)
        .map_or(DEFAULT_ACCENT, |(_, hex)| hex)
}

pub fn palette(accent: &str) -> String {
    let caret = accent_hex(accent);
    let face = |group: &str, colours: &str| {
        format!("[{group}]\nCursor={caret}\nCursorForeground={ON_ACCENT}\n{colours}")
    };
    format!(
        "# Written by apex-session-agent from accent-color; edits are replaced.\n\
         [Palette]\nName=Apex\nPrimary=true\nUseSystemAccent=true\n\n{}\n{}",
        face("Light", LIGHT),
        face("Dark", DARK)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
        let header = format!("[{name}]");
        text.lines()
            .skip_while(|line| *line != header)
            .skip(1)
            .take_while(|line| !line.starts_with('['))
            .filter(|line| !line.is_empty())
            .collect()
    }

    #[test]
    fn the_caret_is_the_accent_in_both_faces() {
        let text = palette("teal");
        for face in ["Light", "Dark"] {
            let lines = group(&text, face);
            assert!(lines.contains(&"Cursor=#2190a4"), "{face}: {lines:?}");
            assert!(lines.contains(&"CursorForeground=#ffffff"), "{face}");
            assert_eq!(lines.iter().filter(|l| l.starts_with("Cursor=")).count(), 1);
        }
    }

    #[test]
    fn every_face_has_what_ptyxis_requires() {
        let text = palette("blue");
        assert_eq!(group(&text, "Palette")[0], "Name=Apex");
        for face in ["Light", "Dark"] {
            let lines = group(&text, face);
            let keys: Vec<&str> = lines.iter().filter_map(|l| l.split('=').next()).collect();
            for key in ["Foreground", "Background"] {
                assert!(keys.contains(&key), "{face} lacks {key}");
            }
            for i in 0..16 {
                assert!(
                    keys.contains(&format!("Color{i}").as_str()),
                    "{face} Color{i}"
                );
            }
            for line in lines {
                let (_, value) = line.split_once('=').expect("key=value");
                assert!(value.len() == 7 && value.starts_with('#'), "{face}: {line}");
            }
        }
    }

    #[test]
    fn the_nine_accents_and_an_unknown_one() {
        assert_eq!(accent_hex("blue"), "#3584e4");
        assert_eq!(accent_hex("slate"), "#6f8396");
        assert_eq!(accent_hex("magenta"), DEFAULT_ACCENT);
        assert_eq!(accent_hex(""), DEFAULT_ACCENT);
        let mut hexes: Vec<_> = ACCENTS.iter().map(|(_, hex)| *hex).collect();
        hexes.sort_unstable();
        hexes.dedup();
        assert_eq!(hexes.len(), 9);
    }

    #[test]
    fn the_palette_is_stock_but_for_the_caret() {
        let blue = palette("blue");
        let red = palette("red");
        let differ: Vec<_> = blue
            .lines()
            .zip(red.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(
            differ,
            [
                ("Cursor=#3584e4", "Cursor=#e62d42"),
                ("Cursor=#3584e4", "Cursor=#e62d42")
            ]
        );
    }
}
