use serde::{Deserialize, Serialize};

/// Present from `Apply` until the boot assessment of its digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Intent {
    pub digest: String,
    /// Unix seconds.
    #[serde(default)]
    pub created: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DenyEntry {
    pub digest: String,
    #[serde(default)]
    pub reason: String,
    /// Unix seconds.
    #[serde(default)]
    pub at: u64,
}

/// A sorted vector: the list is tiny, and it serializes in a stable order, so the file
/// changes only when an entry does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "DenylistFile", into = "DenylistFile")]
pub struct Denylist {
    entries: Vec<DenyEntry>,
}

#[derive(Serialize, Deserialize)]
struct DenylistFile {
    #[serde(default)]
    digests: Vec<DenyEntry>,
}

impl From<DenylistFile> for Denylist {
    /// A hand-edited file may be unsorted; the earliest duplicate wins, as it records the
    /// first failure.
    fn from(file: DenylistFile) -> Self {
        let mut entries = file.digests;
        entries.sort_by(|a, b| a.digest.cmp(&b.digest).then(a.at.cmp(&b.at)));
        entries.dedup_by(|later, kept| later.digest == kept.digest);
        Self { entries }
    }
}

impl From<Denylist> for DenylistFile {
    fn from(list: Denylist) -> Self {
        Self {
            digests: list.entries,
        }
    }
}

impl Denylist {
    pub fn new() -> Self {
        Self::default()
    }

    fn position(&self, digest: &str) -> Result<usize, usize> {
        self.entries
            .binary_search_by(|entry| entry.digest.as_str().cmp(digest))
    }

    pub fn contains(&self, digest: &str) -> bool {
        self.position(digest).is_ok()
    }

    pub fn get(&self, digest: &str) -> Option<&DenyEntry> {
        self.position(digest).ok().map(|i| &self.entries[i])
    }

    /// Keeps an existing entry, which records the first failure.
    pub fn insert(&mut self, entry: DenyEntry) -> bool {
        match self.position(&entry.digest) {
            Ok(_) => false,
            Err(i) => {
                self.entries.insert(i, entry);
                true
            }
        }
    }

    pub fn remove(&mut self, digest: &str) -> Option<DenyEntry> {
        self.position(digest).ok().map(|i| self.entries.remove(i))
    }

    pub fn entries(&self) -> &[DenyEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl FromIterator<DenyEntry> for Denylist {
    fn from_iter<I: IntoIterator<Item = DenyEntry>>(iter: I) -> Self {
        DenylistFile {
            digests: iter.into_iter().collect(),
        }
        .into()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LastCheck {
    /// Unix seconds; 0 when never.
    #[serde(default)]
    pub at: u64,
    #[serde(default)]
    pub remote_digest: Option<String>,
    /// As found: it may not be a valid version.
    #[serde(default)]
    pub remote_version: Option<String>,
    /// Bytes of the remote image's layers not present yet, as the check rounded them.
    #[serde(default)]
    pub remote_size: Option<u64>,
}

/// What the user chose in Settings; a missing file means the defaults.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UpdateSettings {
    #[serde(default = "enabled")]
    pub automatic_downloads: bool,
}

const fn enabled() -> bool {
    true
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            automatic_downloads: true,
        }
    }
}

/// The update a "fallback" boot report denylisted, kept until acknowledged.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FallbackRecord {
    pub digest: String,
    /// Unix seconds.
    #[serde(default)]
    pub at: u64,
}

/// Written by `apex-boot-assess` when an applied deployment failed its required checks
/// during a user session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HealthFailureRecord {
    pub digest: String,
    #[serde(default)]
    pub failed_checks: Vec<String>,
    /// Unix seconds.
    #[serde(default)]
    pub at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn settings_default_to_automatic_downloads() {
        assert!(UpdateSettings::default().automatic_downloads);
        let empty: UpdateSettings = serde_json::from_str("{}").unwrap();
        assert!(empty.automatic_downloads, "a file without the key");
        let off: UpdateSettings = serde_json::from_str(r#"{"automatic_downloads":false}"#).unwrap();
        assert!(!off.automatic_downloads);
    }
    use std::collections::BTreeMap;

    fn entry(digest: &str, at: u64) -> DenyEntry {
        DenyEntry {
            digest: digest.into(),
            reason: format!("failed at {at}"),
            at,
        }
    }

    #[test]
    fn denylist_insert_contains_remove() {
        let mut list = Denylist::new();
        assert!(list.is_empty());
        assert!(list.insert(entry("sha256:bb", 2)));
        assert!(list.insert(entry("sha256:aa", 1)));
        assert!(
            !list.insert(entry("sha256:bb", 9)),
            "duplicate keeps the first"
        );
        assert_eq!(list.get("sha256:bb").map(|e| e.at), Some(2));
        assert!(list.contains("sha256:aa"));
        assert!(!list.contains("sha256:cc"));
        assert_eq!(list.len(), 2);
        assert_eq!(
            list.entries()
                .iter()
                .map(|e| e.digest.as_str())
                .collect::<Vec<_>>(),
            ["sha256:aa", "sha256:bb"]
        );
        assert_eq!(list.remove("sha256:aa").map(|e| e.at), Some(1));
        assert_eq!(list.remove("sha256:aa"), None);
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn denylist_file_format_matches_the_contract() {
        let list: Denylist = [entry("sha256:bb", 2), entry("sha256:aa", 1)]
            .into_iter()
            .collect();
        let json = serde_json::to_value(&list).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({ "digests": [
                { "digest": "sha256:aa", "reason": "failed at 1", "at": 1 },
                { "digest": "sha256:bb", "reason": "failed at 2", "at": 2 },
            ]})
        );
    }

    #[test]
    fn denylist_reading_sorts_and_keeps_the_earliest_duplicate() {
        let list: Denylist = serde_json::from_str(
            r#"{"digests":[{"digest":"sha256:b","at":5},{"digest":"sha256:a"},{"digest":"sha256:b","at":3}],"extra":1}"#,
        )
        .expect("lenient");
        assert_eq!(list.len(), 2);
        assert_eq!(list.get("sha256:b").map(|e| e.at), Some(3));
        assert_eq!(list.get("sha256:a").map(|e| e.reason.as_str()), Some(""));
        assert!(
            serde_json::from_str::<Denylist>("{}")
                .expect("empty")
                .is_empty()
        );
    }

    #[test]
    fn intent_and_last_check_are_lenient() {
        let intent: Intent =
            serde_json::from_str(r#"{"digest":"sha256:a","mode":"next-boot"}"#).expect("lenient");
        assert_eq!(intent.created, 0);
        assert!(serde_json::from_str::<Intent>(r#"{"created":1}"#).is_err());
        let check: LastCheck = serde_json::from_str("{}").expect("lenient");
        assert_eq!(check, LastCheck::default());
        let health: HealthFailureRecord =
            serde_json::from_str(r#"{"digest":"sha256:a"}"#).expect("lenient");
        assert!(health.failed_checks.is_empty());
    }

    proptest! {
        #[test]
        fn denylist_matches_a_map_model(ops in proptest::collection::vec((any::<bool>(), 0u8..16, 0u64..100), 0..64)) {
            let mut list = Denylist::new();
            let mut model: BTreeMap<String, u64> = BTreeMap::new();
            for (insert, key, at) in ops {
                let digest = format!("sha256:{key:02x}");
                if insert {
                    let inserted = list.insert(entry(&digest, at));
                    prop_assert_eq!(inserted, !model.contains_key(&digest));
                    model.entry(digest).or_insert(at);
                } else {
                    prop_assert_eq!(list.remove(&digest).map(|e| e.at), model.remove(&digest));
                }
                let listed: Vec<(String, u64)> = list.entries().iter().map(|e| (e.digest.clone(), e.at)).collect();
                let modelled: Vec<(String, u64)> = model.iter().map(|(k, v)| (k.clone(), *v)).collect();
                prop_assert_eq!(listed, modelled);
            }
            let round_trip: Denylist = serde_json::from_str(&serde_json::to_string(&list).expect("ser")).expect("de");
            prop_assert_eq!(round_trip, list);
        }
    }
}
