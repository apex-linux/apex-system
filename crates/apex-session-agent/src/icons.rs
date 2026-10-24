pub const ICON_THEME_KEY: &str = "icon-theme";

/// apex-desktop's icon theme; its folders are blue.
pub const BASE_THEME: &str = "Apex";

/// libadwaita's `accent-color` nicks, one `Apex-<nick>` folder theme each in apex-desktop.
pub const ACCENTS: [&str; 9] = [
    "blue", "teal", "green", "yellow", "orange", "red", "pink", "purple", "slate",
];

fn is_apex(theme: &str) -> bool {
    theme == BASE_THEME
        || theme
            .strip_prefix("Apex-")
            .is_some_and(|accent| ACCENTS.contains(&accent))
}

/// The folder theme for `accent` when `current` is one of Apex's; `None` when it already is
/// that theme, or is a theme the user picked, which is never replaced.
pub fn folder_theme(current: &str, accent: &str) -> Option<String> {
    if !is_apex(current) {
        return None;
    }
    let wanted = if ACCENTS.contains(&accent) {
        format!("Apex-{accent}")
    } else {
        BASE_THEME.to_owned()
    };
    (wanted != current).then_some(wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apex_follows_the_accent() {
        assert_eq!(
            folder_theme("Apex", "orange").as_deref(),
            Some("Apex-orange")
        );
        assert_eq!(
            folder_theme("Apex-orange", "slate").as_deref(),
            Some("Apex-slate")
        );
        assert_eq!(folder_theme("Apex-slate", "slate"), None, "already set");
        for accent in ACCENTS {
            assert_eq!(
                folder_theme(BASE_THEME, accent),
                Some(format!("Apex-{accent}"))
            );
        }
    }

    #[test]
    fn an_unknown_accent_falls_back_to_the_blue_base_theme() {
        assert_eq!(folder_theme("Apex-red", "magenta").as_deref(), Some("Apex"));
        assert_eq!(folder_theme("Apex", "magenta"), None);
    }

    #[test]
    fn a_theme_the_user_picked_is_left_alone() {
        for theme in [
            "Adwaita",
            "Papirus-Dark",
            "Apex-custom",
            "apex",
            "",
            "Apex-",
        ] {
            assert_eq!(folder_theme(theme, "green"), None, "{theme}");
        }
    }
}
