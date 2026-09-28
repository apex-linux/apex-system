use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::rpmvercmp::{Evr, EvrRef};
use crate::version::Version;

pub const MANIFEST_SCHEMA: u32 = 1;

pub const MANIFEST_PATH: &str = "/usr/share/apex/manifest.json";

/// Read leniently, since other `apexctl` versions build the images; only a newer `schema`,
/// which is bumped on breaking change, is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default = "default_schema")]
    pub schema: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub fedora: u32,
    #[serde(default)]
    pub revision: String,
    #[serde(default)]
    pub base_image: String,
    #[serde(default)]
    pub degraded: Vec<String>,
    /// Rebuild-lane families held at their last good build.
    #[serde(default)]
    pub held: Vec<String>,
    /// Sorted by `(name, arch)` in byte order, unique on that key.
    #[serde(default)]
    pub packages: Vec<Package>,
}

const fn default_schema() -> u32 {
    MANIFEST_SCHEMA
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Package {
    #[serde(default)]
    pub name: String,
    #[serde(default, deserialize_with = "epoch_or_zero")]
    pub epoch: u32,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub release: String,
    #[serde(default)]
    pub arch: String,
}

fn epoch_or_zero<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    Ok(Option::<u32>::deserialize(deserializer)?.unwrap_or_default())
}

impl Package {
    pub fn evr(&self) -> EvrRef<'_> {
        EvrRef {
            epoch: self.epoch,
            version: &self.version,
            release: &self.release,
        }
    }

    /// What the manifest is sorted and diffed by.
    pub fn key(&self) -> (&str, &str) {
        (&self.name, &self.arch)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("manifest schema {found} is newer than the supported schema {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
}

impl Manifest {
    pub fn from_json(bytes: &[u8]) -> Result<Self, ManifestError> {
        let manifest: Self = serde_json::from_slice(bytes)?;
        if manifest.schema > MANIFEST_SCHEMA {
            return Err(ManifestError::UnsupportedSchema {
                found: manifest.schema,
                supported: MANIFEST_SCHEMA,
            });
        }
        Ok(manifest)
    }

    pub fn to_json(&self) -> Result<Vec<u8>, ManifestError> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn parsed_version(&self) -> Option<Version> {
        Version::parse(&self.version)
    }

    /// Writers call this before serializing.
    pub fn normalize(&mut self) {
        self.packages.sort_by(by_key_then_newest_evr);
        self.packages
            .dedup_by(|later, kept| later.key() == kept.key());
        self.degraded.sort();
        self.degraded.dedup();
        self.held.sort();
        self.held.dedup();
    }
}

fn by_key_then_newest_evr(a: &Package, b: &Package) -> Ordering {
    a.key()
        .cmp(&b.key())
        .then_with(|| b.evr().rpm_cmp(&a.evr()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeKind {
    Added,
    Removed,
    Upgraded,
    Downgraded,
}

impl ChangeKind {
    /// The `GetChanges` and JSON wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Upgraded => "upgraded",
            Self::Downgraded => "downgraded",
        }
    }
}

impl fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Change {
    pub name: String,
    pub arch: String,
    pub kind: ChangeKind,
    pub from_evr: Option<Evr>,
    pub to_evr: Option<Evr>,
}

/// A malformed manifest is sorted and deduplicated too, so its diff stays well-defined.
fn sorted_unique(packages: &[Package]) -> Vec<&Package> {
    let mut refs: Vec<&Package> = packages.iter().collect();
    let strictly_sorted = refs.windows(2).all(|w| w[0].key() < w[1].key());
    if !strictly_sorted {
        refs.sort_by(|a, b| by_key_then_newest_evr(a, b));
        refs.dedup_by(|later, kept| later.key() == kept.key());
    }
    refs
}

/// EVRs rpm considers equal (`1.0` and `1_0`) are no change: rpm would not update them either.
pub fn diff(booted: &[Package], staged: &[Package]) -> Vec<Change> {
    let old = sorted_unique(booted);
    let new = sorted_unique(staged);
    let mut changes = Vec::new();
    let (mut i, mut j) = (0, 0);

    while i < old.len() && j < new.len() {
        let (a, b) = (old[i], new[j]);
        match a.key().cmp(&b.key()) {
            Ordering::Less => {
                changes.push(removed(a));
                i += 1;
            }
            Ordering::Greater => {
                changes.push(added(b));
                j += 1;
            }
            Ordering::Equal => {
                let kind = match a.evr().rpm_cmp(&b.evr()) {
                    Ordering::Less => Some(ChangeKind::Upgraded),
                    Ordering::Greater => Some(ChangeKind::Downgraded),
                    Ordering::Equal => None,
                };
                if let Some(kind) = kind {
                    changes.push(Change {
                        name: a.name.clone(),
                        arch: a.arch.clone(),
                        kind,
                        from_evr: Some(a.evr().to_evr()),
                        to_evr: Some(b.evr().to_evr()),
                    });
                }
                i += 1;
                j += 1;
            }
        }
    }
    changes.extend(old[i..].iter().map(|p| removed(p)));
    changes.extend(new[j..].iter().map(|p| added(p)));
    changes
}

fn removed(p: &Package) -> Change {
    Change {
        name: p.name.clone(),
        arch: p.arch.clone(),
        kind: ChangeKind::Removed,
        from_evr: Some(p.evr().to_evr()),
        to_evr: None,
    }
}

fn added(p: &Package) -> Change {
    Change {
        name: p.name.clone(),
        arch: p.arch.clone(),
        kind: ChangeKind::Added,
        from_evr: None,
        to_evr: Some(p.evr().to_evr()),
    }
}

/// Firefox is a Flatpak on Apex, so not part of the image.
pub const HIGHLIGHTED_PACKAGES: &[(&str, &str)] = &[
    ("gnome-shell", "GNOME"),
    ("kernel", "Linux"),
    ("mesa-dri-drivers", "Mesa"),
    ("systemd", "systemd"),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Highlight {
    pub label: &'static str,
    pub package: &'static str,
    pub kind: ChangeKind,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// `Display` is English; a translated UI builds its own text from the fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub highlights: Vec<Highlight>,
    pub other_changes: usize,
}

/// A highlight shows the native architecture; the others count as other changes.
fn arch_rank(arch: &str) -> u8 {
    match arch {
        "x86_64" | "aarch64" | "noarch" => 0,
        _ => 1,
    }
}

fn shown_versions(from: &Evr, to: &Evr) -> (String, String) {
    if from.version != to.version {
        (from.version.clone(), to.version.clone())
    } else if from.release != to.release {
        (
            format!("{}-{}", from.version, from.release),
            format!("{}-{}", to.version, to.release),
        )
    } else {
        (from.to_string(), to.to_string())
    }
}

pub fn summarize(changes: &[Change]) -> Summary {
    let mut highlights = Vec::new();
    let mut covered = 0;
    for &(package, label) in HIGHLIGHTED_PACKAGES {
        let Some(change) = changes
            .iter()
            .filter(|c| c.name == package)
            .min_by_key(|c| arch_rank(&c.arch))
        else {
            continue;
        };
        covered += 1;
        let (from, to) = match (&change.from_evr, &change.to_evr) {
            (Some(from), Some(to)) => {
                let (from, to) = shown_versions(from, to);
                (Some(from), Some(to))
            }
            (from, to) => (
                from.as_ref().map(|e| e.version.clone()),
                to.as_ref().map(|e| e.version.clone()),
            ),
        };
        highlights.push(Highlight {
            label,
            package,
            kind: change.kind,
            from,
            to,
        });
    }
    Summary {
        highlights,
        other_changes: changes.len() - covered,
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.highlights.is_empty() && self.other_changes == 0 {
            return f.write_str("No package changes");
        }
        let mut first = true;
        for h in &self.highlights {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            match (&h.from, &h.to) {
                (Some(from), Some(to)) => write!(f, "{} {from} → {to}", h.label)?,
                (None, Some(to)) => write!(f, "{} {to} added", h.label)?,
                (Some(_), None) => write!(f, "{} removed", h.label)?,
                (None, None) => f.write_str(h.label)?,
            }
        }
        let n = self.other_changes;
        match (first, n) {
            (_, 0) => Ok(()),
            (true, 1) => f.write_str("1 package changed"),
            (true, n) => write!(f, "{n} packages changed"),
            (false, 1) => f.write_str(", 1 other package"),
            (false, n) => write!(f, ", {n} other packages"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, evr: (u32, &str, &str), arch: &str) -> Package {
        Package {
            name: name.to_owned(),
            epoch: evr.0,
            version: evr.1.to_owned(),
            release: evr.2.to_owned(),
            arch: arch.to_owned(),
        }
    }

    #[test]
    fn parses_the_contract_example_and_ignores_unknown_fields() {
        let json = br#"{
          "schema": 1,
          "name": "apex",
          "version": "45.20260924.117",
          "fedora": 45,
          "revision": "0123456789abcdef0123456789abcdef01234567",
          "base_image": "quay.io/fedora/fedora-bootc@sha256:00",
          "degraded": ["audio"],
          "held": ["gnome-shell"],
          "packages": [
            { "name": "bash", "epoch": 0, "version": "5.3.0", "release": "2.fc45", "arch": "x86_64", "sha256": "x" }
          ],
          "future_field": {"nested": true}
        }"#;
        let m = Manifest::from_json(json).expect("valid manifest");
        assert_eq!(m.parsed_version(), Version::parse("45.20260924.117"));
        assert_eq!(m.degraded, ["audio"]);
        assert_eq!(m.held, ["gnome-shell"]);
        assert_eq!(m.packages, [pkg("bash", (0, "5.3.0", "2.fc45"), "x86_64")]);
    }

    #[test]
    fn missing_fields_default_and_null_epoch_is_zero() {
        let m = Manifest::from_json(br#"{"packages":[{"name":"a","epoch":null,"version":"1","release":"1","arch":"noarch"}]}"#)
            .expect("lenient");
        assert_eq!(m.schema, MANIFEST_SCHEMA);
        assert_eq!(m.version, "");
        assert_eq!(m.parsed_version(), None);
        assert!(m.held.is_empty(), "a manifest from before the held field");
        assert_eq!(m.packages[0].epoch, 0);
    }

    #[test]
    fn refuses_a_newer_schema_and_bad_json() {
        assert!(matches!(
            Manifest::from_json(br#"{"schema": 2}"#),
            Err(ManifestError::UnsupportedSchema {
                found: 2,
                supported: 1
            })
        ));
        assert!(matches!(
            Manifest::from_json(b"{"),
            Err(ManifestError::Json(_))
        ));
        assert!(matches!(
            Manifest::from_json(br#"{"packages": 3}"#),
            Err(ManifestError::Json(_))
        ));
    }

    #[test]
    fn to_json_round_trips() {
        let mut m =
            Manifest::from_json(br#"{"version":"45.20260924.1","packages":[]}"#).expect("ok");
        m.packages.push(pkg("zsh", (0, "5.9", "1.fc45"), "x86_64"));
        let bytes = m.to_json().expect("serialize");
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(Manifest::from_json(&bytes).expect("parse"), m);
    }

    #[test]
    fn normalize_sorts_and_keeps_the_newest_duplicate() {
        let mut m = Manifest::from_json(b"{}").expect("ok");
        m.packages = vec![
            pkg("b", (0, "1", "1"), "x86_64"),
            pkg("a", (0, "1", "1"), "x86_64"),
            pkg("b", (0, "2", "1"), "x86_64"),
            pkg("a", (0, "1", "1"), "i686"),
        ];
        m.degraded = vec!["fingerprint".into(), "audio".into(), "audio".into()];
        m.held = vec!["nautilus".into(), "gnome-shell".into(), "nautilus".into()];
        m.normalize();
        assert_eq!(
            m.packages,
            [
                pkg("a", (0, "1", "1"), "i686"),
                pkg("a", (0, "1", "1"), "x86_64"),
                pkg("b", (0, "2", "1"), "x86_64"),
            ]
        );
        assert_eq!(m.degraded, ["audio", "fingerprint"]);
        assert_eq!(m.held, ["gnome-shell", "nautilus"]);
    }

    #[test]
    fn diff_reports_every_kind_in_key_order() {
        let booted = [
            pkg("bash", (0, "5.3.0", "2.fc45"), "x86_64"),
            pkg("gone", (0, "1", "1"), "noarch"),
            pkg("kernel", (0, "7.2.7", "200.fc45"), "x86_64"),
            pkg("same", (0, "1.0", "1"), "x86_64"),
            pkg("zed", (1, "2", "1"), "x86_64"),
        ];
        let staged = [
            pkg("bash", (0, "5.3.0", "2.fc45"), "x86_64"),
            pkg("kernel", (0, "7.2.9", "200.fc45"), "x86_64"),
            pkg("new", (0, "3", "1"), "noarch"),
            pkg("same", (0, "1_0", "1"), "x86_64"),
            pkg("zed", (0, "9", "1"), "x86_64"),
        ];
        let changes = diff(&booted, &staged);
        let summary: Vec<_> = changes.iter().map(|c| (c.name.as_str(), c.kind)).collect();
        assert_eq!(
            summary,
            [
                ("gone", ChangeKind::Removed),
                ("kernel", ChangeKind::Upgraded),
                ("new", ChangeKind::Added),
                ("zed", ChangeKind::Downgraded),
            ]
        );
        assert_eq!(changes[0].to_evr, None);
        assert_eq!(changes[2].from_evr, None);
        assert_eq!(changes[1].from_evr, Some(Evr::new(0, "7.2.7", "200.fc45")));
    }

    #[test]
    fn diff_sorts_unsorted_input() {
        let booted = [pkg("b", (0, "1", "1"), "x"), pkg("a", (0, "1", "1"), "x")];
        let staged = [pkg("a", (0, "2", "1"), "x"), pkg("b", (0, "1", "1"), "x")];
        let changes = diff(&booted, &staged);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "a");
        assert_eq!(changes[0].kind, ChangeKind::Upgraded);
    }

    #[test]
    fn diff_of_empty_lists() {
        assert!(diff(&[], &[]).is_empty());
        assert_eq!(
            diff(&[], &[pkg("a", (0, "1", "1"), "x")])[0].kind,
            ChangeKind::Added
        );
        assert_eq!(
            diff(&[pkg("a", (0, "1", "1"), "x")], &[])[0].kind,
            ChangeKind::Removed
        );
    }

    fn change(
        name: &str,
        arch: &str,
        from: Option<Evr>,
        to: Option<Evr>,
        kind: ChangeKind,
    ) -> Change {
        Change {
            name: name.into(),
            arch: arch.into(),
            kind,
            from_evr: from,
            to_evr: to,
        }
    }

    #[test]
    fn summary_matches_the_ui_example() {
        let mut changes = vec![
            change(
                "gnome-shell",
                "x86_64",
                Some(Evr::new(0, "51.0", "1.fc45")),
                Some(Evr::new(0, "51.1", "1.fc45")),
                ChangeKind::Upgraded,
            ),
            change(
                "kernel",
                "x86_64",
                Some(Evr::new(0, "7.2.7", "200.fc45")),
                Some(Evr::new(0, "7.2.9", "200.fc45")),
                ChangeKind::Upgraded,
            ),
        ];
        for i in 0..212 {
            changes.push(change(
                &format!("pkg{i}"),
                "x86_64",
                Some(Evr::new(0, "1", "1")),
                Some(Evr::new(0, "2", "1")),
                ChangeKind::Upgraded,
            ));
        }
        assert_eq!(
            summarize(&changes).to_string(),
            "GNOME 51.0 → 51.1, Linux 7.2.7 → 7.2.9, 212 other packages"
        );
    }

    #[test]
    fn summary_edge_cases() {
        assert_eq!(summarize(&[]).to_string(), "No package changes");
        let one = [change(
            "a",
            "x86_64",
            None,
            Some(Evr::new(0, "1", "1")),
            ChangeKind::Added,
        )];
        assert_eq!(summarize(&one).to_string(), "1 package changed");
        let two = [one[0].clone(), one[0].clone()];
        assert_eq!(summarize(&two).to_string(), "2 packages changed");

        let rel = [change(
            "kernel",
            "x86_64",
            Some(Evr::new(0, "7.2.9", "200.fc45")),
            Some(Evr::new(0, "7.2.9", "201.fc45")),
            ChangeKind::Upgraded,
        )];
        assert_eq!(
            summarize(&rel).to_string(),
            "Linux 7.2.9-200.fc45 → 7.2.9-201.fc45"
        );
        let epoch = [change(
            "systemd",
            "x86_64",
            Some(Evr::new(0, "259", "1")),
            Some(Evr::new(1, "259", "1")),
            ChangeKind::Upgraded,
        )];
        assert_eq!(summarize(&epoch).to_string(), "systemd 259-1 → 1:259-1");

        let mixed = [
            change(
                "mesa-dri-drivers",
                "x86_64",
                None,
                Some(Evr::new(0, "26.1.0", "1")),
                ChangeKind::Added,
            ),
            change(
                "systemd",
                "x86_64",
                Some(Evr::new(0, "259", "1")),
                None,
                ChangeKind::Removed,
            ),
            change(
                "zlib-ng",
                "x86_64",
                None,
                Some(Evr::new(0, "2", "1")),
                ChangeKind::Added,
            ),
        ];
        assert_eq!(
            summarize(&mixed).to_string(),
            "Mesa 26.1.0 added, systemd removed, 1 other package"
        );
    }

    #[test]
    fn summary_prefers_the_native_arch_and_counts_the_rest() {
        let changes = [
            change(
                "mesa-dri-drivers",
                "i686",
                Some(Evr::new(0, "26.0", "1")),
                Some(Evr::new(0, "26.1", "1")),
                ChangeKind::Upgraded,
            ),
            change(
                "mesa-dri-drivers",
                "x86_64",
                Some(Evr::new(0, "26.0", "1")),
                Some(Evr::new(0, "26.2", "1")),
                ChangeKind::Upgraded,
            ),
        ];
        let summary = summarize(&changes);
        assert_eq!(summary.highlights.len(), 1);
        assert_eq!(summary.highlights[0].to.as_deref(), Some("26.2"));
        assert_eq!(summary.other_changes, 1);
        assert_eq!(summary.to_string(), "Mesa 26.0 → 26.2, 1 other package");
    }

    #[test]
    fn change_kind_names() {
        assert_eq!(ChangeKind::Added.to_string(), "added");
        assert_eq!(ChangeKind::Removed.as_str(), "removed");
        assert_eq!(ChangeKind::Upgraded.as_str(), "upgraded");
        assert_eq!(ChangeKind::Downgraded.as_str(), "downgraded");
        assert_eq!(
            serde_json::to_string(&ChangeKind::Downgraded).expect("json"),
            "\"downgraded\""
        );
    }
}
