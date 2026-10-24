use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The unit names the marker as `apex-shell-safe.service` writes it, `%S/apex/…`.
pub const MARKER_ENV: &str = "APEX_SHELL_GUARD_MARKER";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_home: PathBuf,
    pub data_home: PathBuf,
    pub state_home: PathBuf,
    pub guard_marker: PathBuf,
}

/// The XDG Base Directory rule: a relative or empty value is ignored.
fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

impl Paths {
    pub fn new(config_home: PathBuf, data_home: PathBuf, state_home: PathBuf) -> Self {
        let guard_marker = state_home.join("apex/shell-guard-tripped");
        Self {
            config_home,
            data_home,
            state_home,
            guard_marker,
        }
    }

    pub fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        let home = absolute(var("HOME"));
        let or_home = |name: &str, fallback: &str| {
            absolute(var(name)).or_else(|| home.as_ref().map(|home| home.join(fallback)))
        };
        let mut paths = Self::new(
            or_home("XDG_CONFIG_HOME", ".config")?,
            or_home("XDG_DATA_HOME", ".local/share")?,
            or_home("XDG_STATE_HOME", ".local/state")?,
        );
        if let Some(marker) = absolute(var(MARKER_ENV)) {
            paths.guard_marker = marker;
        }
        Some(paths)
    }

    pub fn gtk_dir(&self) -> PathBuf {
        self.config_home.join("gtk-4.0")
    }

    pub fn apex_css(&self) -> PathBuf {
        self.gtk_dir().join("apex.css")
    }

    pub fn gtk_css(&self) -> PathBuf {
        self.gtk_dir().join("gtk.css")
    }

    /// Ptyxis watches this directory; the file name is the palette id, `apex`.
    pub fn ptyxis_palette(&self) -> PathBuf {
        self.data_home
            .join("org.gnome.Ptyxis/palettes/apex.palette")
    }

    /// Left by `apex-shell-safe.service` (apex-desktop) when the crash guard trips.
    pub fn guard_marker(&self) -> PathBuf {
        self.guard_marker.clone()
    }

    /// The digest of the last staged update the agent posted "Updates Ready" for.
    pub fn updates_notified(&self) -> PathBuf {
        self.state_home.join("apex/updates-notified")
    }

    /// The digest of the last failed update the agent posted a notification for.
    pub fn failure_notified(&self) -> PathBuf {
        self.state_home.join("apex/failure-notified")
    }

    pub fn marker_dir(&self) -> PathBuf {
        self.guard_marker
            .parent()
            .map_or_else(|| self.state_home.clone(), Path::to_owned)
    }

    pub fn is_ours(&self, path: &Path) -> bool {
        [&self.config_home, &self.data_home, &self.state_home]
            .iter()
            .any(|home| path.starts_with(home))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn paths(vars: &[(&str, &str)]) -> Option<Paths> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        Paths::from_env(|name| vars.get(name).cloned())
    }

    #[test]
    fn the_xdg_variables_win() {
        let p = paths(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_DATA_HOME", "/data"),
            ("XDG_STATE_HOME", "/st"),
        ])
        .unwrap();
        assert_eq!(p.apex_css(), Path::new("/cfg/gtk-4.0/apex.css"));
        assert_eq!(
            p.ptyxis_palette(),
            Path::new("/data/org.gnome.Ptyxis/palettes/apex.palette")
        );
        assert_eq!(p.gtk_css(), Path::new("/cfg/gtk-4.0/gtk.css"));
        assert_eq!(p.guard_marker(), Path::new("/st/apex/shell-guard-tripped"));
        assert_eq!(p.updates_notified(), Path::new("/st/apex/updates-notified"));
        assert_eq!(p.failure_notified(), Path::new("/st/apex/failure-notified"));
    }

    #[test]
    fn the_unit_names_the_marker() {
        let p = paths(&[
            ("HOME", "/home/u"),
            (MARKER_ENV, "/run/st/apex/shell-guard-tripped"),
        ])
        .unwrap();
        assert_eq!(
            p.guard_marker(),
            Path::new("/run/st/apex/shell-guard-tripped")
        );
        assert_eq!(p.marker_dir(), Path::new("/run/st/apex"));
        let relative = paths(&[("HOME", "/home/u"), (MARKER_ENV, "apex/x")]).unwrap();
        assert_eq!(
            relative.guard_marker(),
            Path::new("/home/u/.local/state/apex/shell-guard-tripped")
        );
    }

    #[test]
    fn relative_or_empty_values_fall_back_to_home() {
        let p = paths(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "cfg"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", ""),
        ])
        .unwrap();
        assert_eq!(p.config_home, Path::new("/home/u/.config"));
        assert_eq!(p.data_home, Path::new("/home/u/.local/share"));
        assert_eq!(p.state_home, Path::new("/home/u/.local/state"));
    }

    #[test]
    fn without_a_usable_home_only_absolute_xdg_values_do() {
        assert_eq!(paths(&[]), None);
        assert_eq!(paths(&[("HOME", "relative")]), None);
        let all = [
            ("XDG_CONFIG_HOME", "/c"),
            ("XDG_DATA_HOME", "/d"),
            ("XDG_STATE_HOME", "/s"),
        ];
        assert!(paths(&all).is_some());
        assert_eq!(paths(&all[..2]), None);
        assert_eq!(paths(&[("XDG_CONFIG_HOME", "/c")]), None);
    }

    #[test]
    fn only_the_two_homes_are_ours() {
        let p = paths(&[("HOME", "/home/u")]).unwrap();
        assert!(p.is_ours(&p.apex_css()));
        assert!(p.is_ours(&p.guard_marker()));
        assert!(p.is_ours(&p.ptyxis_palette()));
        assert!(p.is_ours(&p.updates_notified()));
        assert!(!p.is_ours(Path::new("/home/u/Documents/gtk.css")));
        assert!(!p.is_ours(Path::new("/etc/gtk-4.0/gtk.css")));
        assert!(!p.is_ours(Path::new("/home/u/.local/bin/x")));
    }
}
