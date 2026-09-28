//! Keeps `data/apex-boot-assess.service` in line with the M1 contract, ADR 0009 and the
//! timing constants of the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on broken fixtures"
)]

use std::path::Path;

use apex_boot::{ASSESS_BINARY_PATH, ASSESS_TIME_BUDGET, SERVICE_UNIT};

/// `(section, key, value)` for every assignment, in file order.
fn assignments(unit: &str) -> Vec<(String, String, String)> {
    let mut section = String::new();
    let mut out = Vec::new();
    for line in unit.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            name.clone_into(&mut section);
            continue;
        }
        let (key, value) = line.split_once('=').expect("key=value");
        assert!(
            !value.ends_with('\\'),
            "continuation lines are not expected"
        );
        out.push((
            section.clone(),
            key.trim().to_owned(),
            value.trim().to_owned(),
        ));
    }
    out
}

fn values<'a>(all: &'a [(String, String, String)], section: &str, key: &str) -> Vec<&'a str> {
    all.iter()
        .filter(|(s, k, _)| s == section && k == key)
        .flat_map(|(_, _, v)| v.split_whitespace())
        .collect()
}

fn single<'a>(all: &'a [(String, String, String)], section: &str, key: &str) -> &'a str {
    let found: Vec<&str> = all
        .iter()
        .filter(|(s, k, _)| s == section && k == key)
        .map(|(_, _, v)| v.as_str())
        .collect();
    assert_eq!(found.len(), 1, "{section}.{key}: {found:?}");
    found[0]
}

#[test]
fn embedded_copy_is_the_installed_file() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/apex-boot-assess.service");
    assert_eq!(std::fs::read_to_string(file).unwrap(), SERVICE_UNIT);
}

#[test]
fn service_runs_the_contract_binary_once() {
    let all = assignments(SERVICE_UNIT);
    assert_eq!(single(&all, "Service", "Type"), "oneshot");
    assert_eq!(single(&all, "Service", "ExecStart"), ASSESS_BINARY_PATH);
    assert_eq!(single(&all, "Service", "RemainAfterExit"), "yes");
    assert_eq!(single(&all, "Service", "Restart"), "no");
}

#[test]
fn ordering_never_blocks_boot_complete() {
    let all = assignments(SERVICE_UNIT);
    assert!(values(&all, "Unit", "After").contains(&"graphical.target"));
    assert_eq!(values(&all, "Install", "WantedBy"), ["graphical.target"]);
    // Nothing may wait for the assessment.
    for key in ["Before", "RequiredBy", "UpheldBy"] {
        assert!(values(&all, "Unit", key).is_empty(), "{key}");
        assert!(values(&all, "Install", key).is_empty(), "{key}");
    }
    assert!(values(&all, "Unit", "RequiresMountsFor").contains(&"/boot"));
}

/// `apex_boot::ASSESS_TIME_BUDGET` is the one statement of the worst case; the unit's
/// timeout covers it with a bounded margin, and its comment states the same figure.
#[test]
fn timeout_covers_the_time_budget() {
    let all = assignments(SERVICE_UNIT);
    let timeout: u64 = single(&all, "Service", "TimeoutStartSec")
        .parse()
        .expect("TimeoutStartSec in plain seconds");
    let budget = ASSESS_TIME_BUDGET.as_secs();
    assert!(
        timeout >= budget,
        "TimeoutStartSec={timeout} < budget {budget} s"
    );
    // A margin, not a second, silently growing budget.
    assert!(
        timeout - budget <= 60,
        "TimeoutStartSec={timeout} is far above the budget {budget} s"
    );
    // The comment in the unit spells out the budget; keep it honest.
    assert!(
        SERVICE_UNIT.contains(&format!("These add up to {budget} s.")),
        "the unit comment does not state {budget} s"
    );
    assert!(SERVICE_UNIT.contains("apex_boot::ASSESS_TIME_BUDGET"));
}

#[test]
fn hardening_keeps_the_needed_writes_and_no_network() {
    let all = assignments(SERVICE_UNIT);
    assert_eq!(single(&all, "Service", "ProtectSystem"), "strict");
    // grubenv is written only in the writer's private mount namespace (the /boot
    // filesystem is read-only on bootc), so /boot is never made writable for the unit.
    for key in ["ReadWritePaths", "BindPaths"] {
        let paths = values(&all, "Service", key);
        assert!(
            paths
                .iter()
                .all(|p| !p.trim_start_matches(['-', '+']).starts_with("/boot")),
            "{key}={paths:?}"
        );
    }
    assert_eq!(single(&all, "Service", "StateDirectory"), "apex");
    assert_eq!(single(&all, "Service", "StateDirectoryMode"), "0700");
    // bootc writes a temporary copy of itself to /tmp, read-only under strict otherwise.
    assert_eq!(single(&all, "Service", "PrivateTmp"), "yes");
    assert_eq!(single(&all, "Service", "PrivateNetwork"), "yes");
    assert_eq!(
        single(&all, "Service", "RestrictAddressFamilies"),
        "AF_UNIX"
    );
    // These would close /dev, lock the inherited mounts read-only, or hide /proc/asound.
    for key in [
        "PrivateDevices",
        "ProtectClock",
        "DevicePolicy",
        "DeviceAllow",
        "PrivateUsers",
        "ProcSubset",
        "User",
        "DynamicUser",
    ] {
        assert!(values(&all, "Service", key).is_empty(), "{key}");
    }
}

/// The grubenv writer runs `unshare --mount` and `mount -o remount,rw /boot`, and `bootc
/// status` remounts /sysroot and /boot for the ostree lock and reads /proc/1/ns/mnt.
#[test]
fn the_sandbox_allows_the_namespace_and_remounts() {
    let all = assignments(SERVICE_UNIT);
    let caps = values(&all, "Service", "CapabilityBoundingSet");
    assert!(!caps.iter().any(|c| c.starts_with('~')), "{caps:?}");
    for needed in ["CAP_SYS_ADMIN", "CAP_SYS_PTRACE"] {
        assert!(caps.contains(&needed), "{needed} missing from {caps:?}");
    }
    for dropped in [
        "CAP_SYS_MODULE",
        "CAP_SYS_RAWIO",
        "CAP_SYS_TIME",
        "CAP_SYS_BOOT",
        "CAP_NET_ADMIN",
        "CAP_SETUID",
        "CAP_MKNOD",
        "CAP_MAC_ADMIN",
        "CAP_BPF",
    ] {
        assert!(!caps.contains(&dropped), "{dropped} is not needed");
    }
    // Only mount namespaces may be created, and they may.
    assert_eq!(values(&all, "Service", "RestrictNamespaces"), ["mnt"]);
    // bootc joins the host IPC namespace when it runs in a private one; with only mnt
    // allowed that setns fails, so the unit must stay in the host IPC namespace.
    assert!(
        values(&all, "Service", "PrivateIPC")
            .iter()
            .all(|v| *v != "yes"),
        "PrivateIPC=yes makes `bootc status` fail: setns(ipc) is not allowed"
    );
    // The filter allows unshare (in @system-service's @process) and the mount calls.
    let filter = values(&all, "Service", "SystemCallFilter");
    assert!(filter.contains(&"@system-service"), "{filter:?}");
    assert!(filter.contains(&"@mount"), "{filter:?}");
    // An allow list only: no deny entry (for example ~@mount) can take them away again.
    assert!(filter.iter().all(|f| !f.starts_with('~')), "{filter:?}");
    assert_eq!(single(&all, "Service", "SystemCallErrorNumber"), "EPERM");
    assert_eq!(single(&all, "Service", "NoNewPrivileges"), "yes");
}
