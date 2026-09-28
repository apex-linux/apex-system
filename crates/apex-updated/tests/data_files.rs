//! The data files in `data/` (M1 contract §2) against the contract and the code:
//! the polkit defaults equal `apex_dbus::PolkitAction`, the bus policy allows exactly the
//! documented messages, the unit, activation and timer files name the contract's paths and
//! keep the hardening rules, and, when `systemd-analyze` is installed, the units verify.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests: a failed expectation is the test failure"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use apex_dbus::{BUS_NAME, INTERFACE, PolkitAction};
use apex_updated::BINARY_PATH;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

fn data(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

type Element = (String, BTreeMap<String, String>, String);

fn element(start: &BytesStart<'_>) -> Element {
    let name = start.name().as_ref().to_owned();
    let attributes = start
        .attributes()
        .with_checks(true)
        .map(|a| {
            let a = a.expect("attribute");
            (
                a.key.as_ref().to_owned(),
                a.normalized_value(XmlVersion::Implicit1_0)
                    .expect("value")
                    .into_owned(),
            )
        })
        .collect();
    (name, attributes, String::new())
}

/// Every element with its attributes and its own text, in document order; fails on XML
/// that is not well-formed.
fn elements(xml: &str) -> Vec<Element> {
    let mut reader = Reader::from_str(xml);
    let mut out: Vec<Element> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    loop {
        match reader.read_event().expect("well-formed XML") {
            Event::Start(start) => {
                out.push(element(&start));
                open.push(out.len() - 1);
            }
            Event::Empty(start) => out.push(element(&start)),
            Event::Text(text) => {
                if let Some(&index) = open.last() {
                    out[index]
                        .2
                        .push_str(text.xml_content(XmlVersion::Implicit1_0).trim());
                }
            }
            Event::End(_) => {
                open.pop().expect("balanced");
            }
            Event::Eof => break,
            _ => {}
        }
    }
    assert!(open.is_empty(), "unbalanced XML");
    out
}

#[test]
fn polkit_policy_matches_the_contract_table() {
    let xml = data("io.github.sekirokenjii.apex.update.policy");
    assert!(xml.contains("-//freedesktop//DTD PolicyKit Policy Configuration 1.0//EN"));
    let elements = elements(&xml);
    let mut actions: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current = None;
    for (name, attributes, text) in &elements {
        match name.as_str() {
            "action" => {
                let id = attributes["id"].clone();
                current = Some(id.clone());
                actions.insert(id, BTreeMap::new());
            }
            "description" | "message" | "allow_any" | "allow_inactive" | "allow_active" => {
                let id = current.as_ref().expect("inside an action");
                actions
                    .get_mut(id)
                    .unwrap()
                    .insert(name.clone(), text.clone());
            }
            _ => {}
        }
    }
    let expected: BTreeSet<String> = PolkitAction::ALL
        .iter()
        .map(|a| a.id().to_owned())
        .collect();
    assert_eq!(actions.keys().cloned().collect::<BTreeSet<_>>(), expected);
    for action in PolkitAction::ALL {
        let fields = &actions[action.id()];
        assert_eq!(
            fields["allow_any"],
            PolkitAction::ALLOW_ANY.as_str(),
            "{action}"
        );
        assert_eq!(
            fields["allow_inactive"],
            PolkitAction::ALLOW_INACTIVE.as_str(),
            "{action}"
        );
        assert_eq!(
            fields["allow_active"],
            action.allow_active().as_str(),
            "{action}"
        );
        assert!(!fields["description"].is_empty(), "{action}");
        assert!(
            fields["message"].starts_with("Authentication is required to "),
            "{action}"
        );
    }
}

#[test]
fn bus_policy_allows_exactly_the_contract() {
    let xml = data("io.github.sekirokenjii.Apex.Update1.conf");
    assert!(xml.contains("-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"));
    let mut owners = Vec::new();
    let mut sends = BTreeSet::new();
    let mut policy = BTreeMap::new();
    for (name, attributes, _) in elements(&xml) {
        match name.as_str() {
            "policy" => policy = attributes,
            "allow" if attributes.contains_key("own") => {
                owners.push((policy.clone(), attributes["own"].clone()));
            }
            "allow" => {
                assert_eq!(policy.get("context").map(String::as_str), Some("default"));
                assert_eq!(attributes["send_destination"], BUS_NAME);
                sends.insert((
                    attributes["send_interface"].clone(),
                    attributes.get("send_member").cloned().unwrap_or_default(),
                ));
            }
            "deny" => panic!("the policy only allows"),
            _ => {}
        }
    }
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].0.get("user").map(String::as_str), Some("root"));
    assert_eq!(owners[0].1, BUS_NAME);
    let expected: BTreeSet<(String, String)> = [
        (INTERFACE, ""),
        ("org.freedesktop.DBus.Properties", "Get"),
        ("org.freedesktop.DBus.Properties", "GetAll"),
        ("org.freedesktop.DBus.Introspectable", ""),
        ("org.freedesktop.DBus.Peer", ""),
    ]
    .into_iter()
    .map(|(i, m)| (i.to_owned(), m.to_owned()))
    .collect();
    assert_eq!(sends, expected);
}

/// `key=value` lines of an ini-style file, by section (comments and blanks skipped).
fn ini(text: &str) -> BTreeMap<String, Vec<(String, String)>> {
    let mut sections: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut section = String::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            name.clone_into(&mut section);
            continue;
        }
        let (key, value) = line.split_once('=').expect("key=value");
        sections
            .entry(section.clone())
            .or_default()
            .push((key.trim().to_owned(), value.trim().to_owned()));
    }
    sections
}

fn value<'a>(
    file: &'a BTreeMap<String, Vec<(String, String)>>,
    section: &str,
    key: &str,
) -> Option<&'a str> {
    file.get(section)?
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

#[test]
fn activation_file_hands_over_to_systemd() {
    let file = ini(&data("io.github.sekirokenjii.Apex.Update1.service"));
    let section = "D-BUS Service";
    assert_eq!(value(&file, section, "Name"), Some(BUS_NAME));
    assert_eq!(value(&file, section, "Exec"), Some("/bin/false"));
    assert_eq!(value(&file, section, "User"), Some("root"));
    assert_eq!(
        value(&file, section, "SystemdService"),
        Some("apex-updated.service")
    );
}

/// The directives the unit's header lists as deliberately not set (`#  - Name=, Other=: why`).
fn documented_as_not_set(unit: &str) -> BTreeSet<String> {
    let (_, rest) = unit
        .split_once("deliberately NOT set")
        .expect("the unit header lists what it does not set");
    // One item per "#  - " line, joined with its indented continuation lines.
    let mut items: Vec<String> = Vec::new();
    for line in rest
        .lines()
        .skip(1)
        .take_while(|line| line.starts_with('#'))
    {
        if let Some(item) = line.strip_prefix("#  - ") {
            items.push(item.to_owned());
        } else if let (Some(continued), Some(item)) = (line.strip_prefix("#    "), items.last_mut())
        {
            item.push(' ');
            item.push_str(continued);
        }
    }
    items
        .iter()
        .flat_map(|item| {
            let names = item
                .split_once(':')
                .map_or(item.as_str(), |(names, _)| names);
            names
                .split(',')
                .filter_map(|name| name.trim().strip_suffix('='))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn service_unit_is_dbus_activated_and_hardened_within_what_bootc_and_the_grubenv_writer_need() {
    let text = data("apex-updated.service");
    let unit = ini(&text);
    assert_eq!(value(&unit, "Service", "Type"), Some("dbus"));
    assert_eq!(value(&unit, "Service", "BusName"), Some(BUS_NAME));
    assert_eq!(value(&unit, "Service", "ExecStart"), Some(BINARY_PATH));
    assert!(!unit.contains_key("Install"), "D-Bus activated only");
    // bootc stages into /boot and the grubenv writer remounts it.
    let mounts: Vec<&str> = value(&unit, "Unit", "RequiresMountsFor")
        .expect("RequiresMountsFor")
        .split_whitespace()
        .collect();
    for path in ["/var/lib/apex", "/sysroot", "/boot"] {
        assert!(mounts.contains(&path), "{path}");
    }
    for (key, expected) in [
        ("ProtectHome", "yes"),
        ("PrivateTmp", "yes"),
        ("ProtectControlGroups", "yes"),
        (
            "RestrictAddressFamilies",
            "AF_UNIX AF_INET AF_INET6 AF_NETLINK",
        ),
        ("SystemCallArchitectures", "native"),
        ("LockPersonality", "yes"),
        ("RestrictRealtime", "yes"),
        ("ProtectClock", "yes"),
        ("ProtectHostname", "yes"),
        ("ProtectKernelLogs", "yes"),
    ] {
        assert_eq!(value(&unit, "Service", key), Some(expected), "{key}");
    }
    // Each of these breaks bootc or the grubenv writer (`unshare --mount` plus
    // `mount -o remount,rw /boot` needs CAP_SYS_ADMIN, mount namespaces and the @mount
    // system calls); see the comments in the unit, which must list exactly these.
    let forbidden: BTreeSet<String> = [
        "ProtectSystem",
        "ReadOnlyPaths",
        "PrivateMounts",
        "MountFlags",
        "RestrictNamespaces",
        "SystemCallFilter",
        "CapabilityBoundingSet",
        "AmbientCapabilities",
        "NoNewPrivileges",
        "RestrictSUIDSGID",
        "ProtectKernelModules",
        "PrivateDevices",
        "ProtectKernelTunables",
        "MemoryDenyWriteExecute",
        "PrivateNetwork",
        "IPAddressDeny",
    ]
    .map(str::to_owned)
    .into();
    for key in &forbidden {
        for section in ["Unit", "Service"] {
            assert_eq!(value(&unit, section, key), None, "{key}");
        }
    }
    assert_eq!(documented_as_not_set(&text), forbidden);
    // The service runs as root: the writer needs CAP_SYS_ADMIN, bootc much more.
    assert_eq!(value(&unit, "Service", "User"), None);
    assert_eq!(value(&unit, "Service", "DynamicUser"), None);
}

/// Every directive of the unit has a comment right above it, except the self-describing
/// `Description=` and `Documentation=`.
#[test]
fn every_unit_directive_is_documented() {
    let text = data("apex-updated.service");
    let mut previous = "";
    let mut directives = 0;
    for line in text.lines().map(str::trim) {
        let is_directive = !line.is_empty() && !line.starts_with('#') && !line.starts_with('[');
        if is_directive {
            directives += 1;
            let key = line.split_once('=').map_or(line, |(key, _)| key);
            assert!(
                matches!(key, "Description" | "Documentation") || previous.starts_with('#'),
                "{key}= has no comment of its own"
            );
        }
        previous = line;
    }
    assert!(directives >= 18, "{directives} directives");
}

#[test]
fn timer_and_automatic_run() {
    let timer = ini(&data("apex-update-auto.timer"));
    for (key, expected) in [
        ("OnBootSec", "15min"),
        ("OnCalendar", "daily"),
        ("RandomizedDelaySec", "1h"),
        ("Persistent", "true"),
    ] {
        assert_eq!(value(&timer, "Timer", key), Some(expected), "{key}");
    }
    let service = ini(&data("apex-update-auto.service"));
    assert_eq!(value(&service, "Service", "Type"), Some("oneshot"));
    assert_eq!(
        value(&service, "Service", "ExecStart"),
        Some("/usr/bin/apexctl update automatic")
    );
    assert_eq!(
        value(&service, "Unit", "After"),
        Some("network-online.target")
    );
    assert_eq!(
        value(&service, "Service", "User"),
        None,
        "Automatic needs uid 0"
    );
}

#[test]
fn tmpfiles_creates_the_private_state_directory() {
    let lines: Vec<String> = data("apex.tmpfiles.conf")
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(lines, ["d /var/lib/apex 0700 root root -"]);
}

/// `systemd-analyze verify` on the three units inside a scratch root with stub binaries,
/// as `make check-data` does. Skipped when systemd-analyze is not installed.
#[test]
fn units_verify_with_systemd_analyze() {
    if Command::new("systemd-analyze")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skip: systemd-analyze not found");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let units = root.path().join("usr/lib/systemd/system");
    std::fs::create_dir_all(&units).unwrap();
    let names = [
        "apex-updated.service",
        "apex-update-auto.service",
        "apex-update-auto.timer",
    ];
    for name in names {
        std::fs::write(units.join(name), data(name)).unwrap();
    }
    for binary in [BINARY_PATH, "/usr/bin/apexctl"] {
        let path: PathBuf = root.path().join(binary.trim_start_matches('/'));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
    let output = Command::new("systemd-analyze")
        .args(["verify", "--man=no", "--recursive-errors=no"])
        .arg(format!("--root={}", root.path().display()))
        .args(names)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "systemd-analyze verify failed:\n{stderr}"
    );
    let ours: Vec<&str> = stderr
        .lines()
        .filter(|l| names.iter().any(|n| l.contains(n)))
        .collect();
    assert!(
        ours.is_empty(),
        "warnings about our units:\n{}",
        ours.join("\n")
    );
}
