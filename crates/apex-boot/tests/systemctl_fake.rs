//! Everything is in one test function: no other test thread may fork while a fake script is
//! open for writing, or its exec fails with ETXTBSY.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on broken fixtures"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use apex_boot::checks::units::{
    ActiveUnitsCheck, FailedUnitsCheck, REQUIRED_ACTIVE_UNITS, REQUIRED_UNITS,
};
use apex_boot::checks::{Check, CheckOutcome, names};
use apex_boot::run_checks;

struct Fake {
    list_units: &'static str,
    list_units_rc: i32,
    is_active: &'static str,
    is_active_rc: i32,
    stderr: &'static str,
}

fn write_fake(dir: &Path, name: &str, fake: &Fake) -> PathBuf {
    let data = dir.join(format!("{name}.d"));
    std::fs::create_dir(&data).unwrap();
    std::fs::write(data.join("list"), fake.list_units).unwrap();
    std::fs::write(data.join("active"), fake.is_active).unwrap();
    std::fs::write(data.join("stderr"), fake.stderr).unwrap();
    let data = data.display();
    let active_units = REQUIRED_ACTIVE_UNITS.join(" ");
    let script = format!(
        r#"#!/bin/sh
[ "$PATH" = "/usr/bin:/usr/sbin" ] || {{ echo "bad PATH $PATH" >&2; exit 97; }}
[ -z "${{HOME+x}}" ] || {{ echo "environment not cleared" >&2; exit 98; }}
cat '{data}/stderr' >&2
case "$*" in
  "list-units --state=failed --plain --no-legend --no-pager --full")
    cat '{data}/list'; exit {list_rc} ;;
  "is-active -- {active_units}")
    cat '{data}/active'; exit {active_rc} ;;
  *) echo "unexpected arguments: $*" >&2; exit 99 ;;
esac
"#,
        list_rc = fake.list_units_rc,
        active_rc = fake.is_active_rc,
    );
    let path = dir.join(name);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn checks(systemctl: &Path) -> (FailedUnitsCheck, ActiveUnitsCheck) {
    (
        FailedUnitsCheck::new(systemctl, REQUIRED_UNITS),
        ActiveUnitsCheck::new(systemctl, REQUIRED_ACTIVE_UNITS),
    )
}

#[test]
fn unit_checks_against_a_fake_systemctl() {
    let dir = tempfile::tempdir().unwrap();

    let healthy = write_fake(
        dir.path(),
        "healthy",
        &Fake {
            list_units: "swap.img.swap loaded failed failed /swap.img\n",
            list_units_rc: 0,
            is_active: "active\nactive\n",
            is_active_rc: 0,
            stderr: "",
        },
    );
    let (failed, active) = checks(&healthy);
    assert_eq!(failed.run(), Ok(()));
    assert_eq!(active.run(), Ok(()));
    let results = run_checks(vec![Box::new(failed), Box::new(active)]);
    assert_eq!(
        results
            .iter()
            .map(|r| (r.name.as_str(), r.outcome.clone()))
            .collect::<Vec<_>>(),
        [
            (names::FAILED_UNITS, CheckOutcome::Pass),
            (names::CORE_UNITS_ACTIVE, CheckOutcome::Pass)
        ]
    );

    // is-active exits 3 when a unit is not active.
    let broken = write_fake(
        dir.path(),
        "broken",
        &Fake {
            list_units: "gdm.service loaded failed failed GNOME Display Manager\n\
                         polkit.service loaded failed failed Authorization Manager\n",
            list_units_rc: 0,
            is_active: "active\ninactive\n",
            is_active_rc: 3,
            stderr: "",
        },
    );
    let (failed, active) = checks(&broken);
    assert_eq!(
        failed.run(),
        Err("failed units: gdm.service, polkit.service".to_owned())
    );
    assert_eq!(
        active.run(),
        Err("not active: systemd-logind.service (inactive)".to_owned())
    );

    let unreachable = write_fake(
        dir.path(),
        "unreachable",
        &Fake {
            list_units: "",
            list_units_rc: 1,
            is_active: "",
            is_active_rc: 1,
            stderr: "Failed to connect to bus: No such file or directory\n",
        },
    );
    let (failed, active) = checks(&unreachable);
    let err = failed.run().unwrap_err();
    assert!(err.contains("Failed to connect to bus"), "{err}");
    let err = active.run().unwrap_err();
    assert!(err.contains("Failed to connect to bus"), "{err}");

    let truncated = write_fake(
        dir.path(),
        "truncated",
        &Fake {
            list_units: "",
            list_units_rc: 0,
            is_active: "active\n",
            is_active_rc: 0,
            stderr: "",
        },
    );
    let (_, active) = checks(&truncated);
    let err = active.run().unwrap_err();
    assert!(err.contains("expected 2 unit states"), "{err}");

    let (failed, _) = checks(&dir.path().join("missing"));
    let err = failed.run().unwrap_err();
    assert!(err.contains("cannot start"), "{err}");
}
