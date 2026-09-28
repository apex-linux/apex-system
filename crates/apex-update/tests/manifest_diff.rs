//! Package diff: properties over generated manifests, and snapshots of a realistic pair.

#![allow(clippy::expect_used, reason = "test helpers panic on broken fixtures")]

use std::cmp::Ordering;
use std::collections::BTreeMap;

use apex_update::{ChangeKind, Evr, Manifest, Package, Version, diff, summarize};
use proptest::prelude::*;

fn fixture(name: &str) -> Manifest {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).expect("fixture exists");
    Manifest::from_json(&bytes).expect("fixture parses")
}

#[test]
fn fixture_diff_snapshot() {
    let booted = fixture("manifest-booted.json");
    let staged = fixture("manifest-staged.json");
    assert_eq!(booted.parsed_version(), Version::parse("45.20260920.3"));
    assert_eq!(staged.parsed_version(), Version::parse("45.20260924.117"));
    let changes = diff(&booted.packages, &staged.packages);
    let lines: Vec<String> = changes
        .iter()
        .map(|c| {
            format!(
                "{:<10} {}.{} {} -> {}",
                c.kind,
                c.name,
                c.arch,
                c.from_evr
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), Evr::to_string),
                c.to_evr
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), Evr::to_string),
            )
        })
        .collect();
    insta::assert_snapshot!("fixture_diff", lines.join("\n"));
    insta::assert_json_snapshot!("fixture_diff_json", &changes[..3]);
}

#[test]
fn fixture_summary_snapshot() {
    let changes = diff(
        &fixture("manifest-booted.json").packages,
        &fixture("manifest-staged.json").packages,
    );
    insta::assert_snapshot!(
        summarize(&changes).to_string(),
        @"GNOME 51.0 → 51.1, Linux 7.2.7 → 7.2.9, Mesa 26.1.4 → 26.1.5, 8 other packages"
    );
}

// Generated manifests: a sorted, unique package list built from a map keyed by
// (name, arch), like `apexctl build manifest` writes.

type Key = (String, String);

fn evr() -> impl Strategy<Value = Evr> {
    (
        prop_oneof![8 => Just(0u32), 1 => 1u32..3],
        "[0-9]{1,2}(\\.[0-9]{1,2}){0,2}(~rc[0-9]|\\^git[0-9])?",
        "[0-9]{1,2}\\.fc45",
    )
        .prop_map(|(epoch, version, release)| Evr::new(epoch, version, release))
}

fn package_map() -> impl Strategy<Value = BTreeMap<Key, Evr>> {
    proptest::collection::btree_map(
        (
            prop_oneof![
                Just("bash"),
                Just("gnome-shell"),
                Just("kernel"),
                Just("mesa-dri-drivers"),
                Just("systemd"),
                Just("zlib-ng"),
                Just("a"),
                Just("a-b"),
                Just("ab"),
            ]
            .prop_map(str::to_owned),
            prop_oneof![Just("x86_64"), Just("i686"), Just("noarch")].prop_map(str::to_owned),
        ),
        evr(),
        0..20,
    )
}

fn to_packages(map: &BTreeMap<Key, Evr>) -> Vec<Package> {
    map.iter()
        .map(|((name, arch), evr)| Package {
            name: name.clone(),
            epoch: evr.epoch,
            version: evr.version.clone(),
            release: evr.release.clone(),
            arch: arch.clone(),
        })
        .collect()
}

/// A pair where `b` is `a` with some packages changed, removed and added, so that
/// shared keys are common.
fn manifest_pair() -> impl Strategy<Value = (BTreeMap<Key, Evr>, BTreeMap<Key, Evr>)> {
    (
        package_map(),
        package_map(),
        proptest::collection::vec(any::<u8>(), 20),
    )
        .prop_map(|(a, extra, choices)| {
            let mut b = BTreeMap::new();
            for (i, (key, evr)) in a.iter().enumerate() {
                match choices[i % choices.len()] % 4 {
                    0 => {} // removed
                    1 => {
                        // changed: take a generated EVR for the same key if there is one
                        let new = extra.get(key).cloned().unwrap_or_else(|| {
                            Evr::new(evr.epoch, format!("{}.1", evr.version), evr.release.clone())
                        });
                        b.insert(key.clone(), new);
                    }
                    _ => {
                        b.insert(key.clone(), evr.clone());
                    }
                }
            }
            for (key, evr) in extra {
                b.entry(key).or_insert(evr);
            }
            (a, b)
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn diff_of_identical_lists_is_empty(a in package_map()) {
        let packages = to_packages(&a);
        prop_assert!(diff(&packages, &packages).is_empty());
    }

    #[test]
    fn applying_the_changes_reproduces_the_new_list((a, b) in manifest_pair()) {
        let changes = diff(&to_packages(&a), &to_packages(&b));
        let mut applied = a.clone();
        for c in &changes {
            let key = (c.name.clone(), c.arch.clone());
            match c.kind {
                ChangeKind::Removed => {
                    prop_assert!(applied.remove(&key).is_some());
                }
                ChangeKind::Added => {
                    prop_assert!(applied.insert(key, c.to_evr.clone().expect("to")).is_none());
                }
                ChangeKind::Upgraded | ChangeKind::Downgraded => {
                    let old = applied.insert(key, c.to_evr.clone().expect("to"));
                    prop_assert_eq!(old.as_ref(), c.from_evr.as_ref());
                }
            }
        }
        // Equal up to rpm equivalence: EVRs rpm considers equal are not reported.
        prop_assert_eq!(applied.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>());
        for (key, evr) in &applied {
            prop_assert_eq!(evr.rpm_cmp(&b[key]), Ordering::Equal, "{:?}", key);
        }
    }

    #[test]
    fn kinds_agree_with_rpmvercmp((a, b) in manifest_pair()) {
        for c in diff(&to_packages(&a), &to_packages(&b)) {
            let key = (c.name.clone(), c.arch.clone());
            match c.kind {
                ChangeKind::Added => {
                    prop_assert!(!a.contains_key(&key) && b.contains_key(&key));
                    prop_assert!(c.from_evr.is_none());
                }
                ChangeKind::Removed => {
                    prop_assert!(a.contains_key(&key) && !b.contains_key(&key));
                    prop_assert!(c.to_evr.is_none());
                }
                ChangeKind::Upgraded => {
                    let (from, to) = (c.from_evr.expect("from"), c.to_evr.expect("to"));
                    prop_assert_eq!(from.rpm_cmp(&to), Ordering::Less);
                }
                ChangeKind::Downgraded => {
                    let (from, to) = (c.from_evr.expect("from"), c.to_evr.expect("to"));
                    prop_assert_eq!(from.rpm_cmp(&to), Ordering::Greater);
                }
            }
        }
    }

    #[test]
    fn output_is_sorted_and_unique_by_key((a, b) in manifest_pair()) {
        let changes = diff(&to_packages(&a), &to_packages(&b));
        for w in changes.windows(2) {
            prop_assert!((&w[0].name, &w[0].arch) < (&w[1].name, &w[1].arch));
        }
    }

    #[test]
    fn unsorted_input_gives_the_same_diff(
        (a, b) in manifest_pair(),
        seed_a in any::<u64>(),
        seed_b in any::<u64>(),
    ) {
        let sorted = diff(&to_packages(&a), &to_packages(&b));
        let mut pa = to_packages(&a);
        let mut pb = to_packages(&b);
        pseudo_shuffle(&mut pa, seed_a);
        pseudo_shuffle(&mut pb, seed_b);
        prop_assert_eq!(diff(&pa, &pb), sorted);
    }

    #[test]
    fn reversed_diff_mirrors_kinds((a, b) in manifest_pair()) {
        let forward = diff(&to_packages(&a), &to_packages(&b));
        let backward = diff(&to_packages(&b), &to_packages(&a));
        prop_assert_eq!(forward.len(), backward.len());
        for (f, r) in forward.iter().zip(&backward) {
            let mirrored = match f.kind {
                ChangeKind::Added => ChangeKind::Removed,
                ChangeKind::Removed => ChangeKind::Added,
                ChangeKind::Upgraded => ChangeKind::Downgraded,
                ChangeKind::Downgraded => ChangeKind::Upgraded,
            };
            prop_assert_eq!((&f.name, &f.arch, mirrored), (&r.name, &r.arch, r.kind));
            prop_assert_eq!(&f.from_evr, &r.to_evr);
        }
    }

    #[test]
    fn summary_accounts_for_every_change((a, b) in manifest_pair()) {
        let changes = diff(&to_packages(&a), &to_packages(&b));
        let summary = summarize(&changes);
        prop_assert_eq!(summary.highlights.len() + summary.other_changes, changes.len());
    }
}

/// A deterministic Fisher-Yates shuffle driven by a xorshift generator.
fn pseudo_shuffle<T>(items: &mut [T], mut seed: u64) {
    seed |= 1;
    for i in (1..items.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = (seed % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

#[test]
fn duplicated_keys_keep_the_newest_evr() {
    let p = |v: &str| Package {
        name: "kernel".into(),
        epoch: 0,
        version: v.into(),
        release: "200.fc45".into(),
        arch: "x86_64".into(),
    };
    // A malformed manifest with two kernels; the newest one is compared.
    let changes = diff(&[p("7.2.9"), p("7.2.7")], &[p("7.2.9")]);
    assert!(changes.is_empty(), "{changes:?}");
}
