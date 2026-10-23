use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_home: PathBuf,
    pub state_home: PathBuf,
}

/// The XDG Base Directory rule: a relative or empty value is ignored.
fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

impl Paths {
    pub fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        let home = absolute(var("HOME"));
        let or_home = |name: &str, fallback: &str| {
            absolute(var(name)).or_else(|| home.as_ref().map(|home| home.join(fallback)))
        };
        Some(Self {
            config_home: or_home("XDG_CONFIG_HOME", ".config")?,
            state_home: or_home("XDG_STATE_HOME", ".local/state")?,
        })
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

    pub fn state_dir(&self) -> PathBuf {
        self.state_home.join("apex")
    }

    /// Left by `apex-shell-safe.service` (apex-desktop) when the crash guard trips.
    pub fn guard_marker(&self) -> PathBuf {
        self.state_dir().join("shell-guard-tripped")
    }

    pub fn is_ours(&self, path: &Path) -> bool {
        path.starts_with(&self.config_home) || path.starts_with(&self.state_home)
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
            ("XDG_STATE_HOME", "/st"),
        ])
        .unwrap();
        assert_eq!(p.apex_css(), Path::new("/cfg/gtk-4.0/apex.css"));
        assert_eq!(p.gtk_css(), Path::new("/cfg/gtk-4.0/gtk.css"));
        assert_eq!(p.guard_marker(), Path::new("/st/apex/shell-guard-tripped"));
    }

    #[test]
    fn relative_or_empty_values_fall_back_to_home() {
        let p = paths(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "cfg"),
            ("XDG_STATE_HOME", ""),
        ])
        .unwrap();
        assert_eq!(p.config_home, Path::new("/home/u/.config"));
        assert_eq!(p.state_home, Path::new("/home/u/.local/state"));
    }

    #[test]
    fn without_a_usable_home_only_absolute_xdg_values_do() {
        assert_eq!(paths(&[]), None);
        assert_eq!(paths(&[("HOME", "relative")]), None);
        assert!(paths(&[("XDG_CONFIG_HOME", "/c"), ("XDG_STATE_HOME", "/s")]).is_some());
        assert_eq!(paths(&[("XDG_CONFIG_HOME", "/c")]), None);
    }

    #[test]
    fn only_the_two_homes_are_ours() {
        let p = paths(&[("HOME", "/home/u")]).unwrap();
        assert!(p.is_ours(&p.apex_css()));
        assert!(p.is_ours(&p.guard_marker()));
        assert!(!p.is_ours(Path::new("/home/u/Documents/gtk.css")));
        assert!(!p.is_ours(Path::new("/etc/gtk-4.0/gtk.css")));
    }
}
