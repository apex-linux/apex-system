#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; clippy's allow-unwrap-in-tests does not cover them"
)]

use std::fmt::Write as _;
use std::time::Duration;

use apex_bootc::{
    APEX_STABLE, APEX_STABLE_REPOSITORY, APEX_TESTING, BOOTC_PROGRAM, BootcCommand, CommandKind,
    ENVIRONMENT, ImageRef, ImageRefError, OSTREE_PROGRAM, Program,
};

const PINNED: &str = "sha256:0b4c83fba9a0e3e2e5a7e9dbb5f4c1f1d7b5d1c6e0a3f9e8d7c6b5a4f3e2d1c0";

const LOCAL_TEST_REF: ImageRef = ImageRef::from_static("localhost:5000/apex:45");

fn all_commands() -> Vec<BootcCommand> {
    vec![
        BootcCommand::Status,
        BootcCommand::UpgradeCheck,
        BootcCommand::UpgradeDownloadOnly { progress: false },
        BootcCommand::UpgradeDownloadOnly { progress: true },
        BootcCommand::UpgradeFromDownloaded,
        BootcCommand::Rollback,
        BootcCommand::SwitchDownloadOnly {
            target: APEX_STABLE,
        },
        BootcCommand::SwitchDownloadOnly {
            target: APEX_TESTING,
        },
        BootcCommand::SwitchDownloadOnly {
            target: ImageRef::at_digest(APEX_STABLE_REPOSITORY, PINNED).unwrap(),
        },
        BootcCommand::SwitchDownloadOnly {
            target: LOCAL_TEST_REF,
        },
        BootcCommand::PinRollback,
        BootcCommand::OstreeStatus,
        BootcCommand::Unpin { index: 2 },
    ]
}

fn command_line(c: &BootcCommand) -> String {
    let args: Vec<String> = c
        .argv()
        .iter()
        .map(|a| a.to_str().unwrap().to_owned())
        .collect();
    let program = match c.kind().program() {
        Program::Bootc => BOOTC_PROGRAM,
        Program::Ostree => OSTREE_PROGRAM,
    };
    format!("{program} {}", args.join(" "))
}

#[test]
fn argv_snapshot() {
    let mut out = String::new();
    for c in all_commands() {
        let fd3 = if c.uses_progress_fd() {
            "  [fd 3 = progress pipe]"
        } else {
            ""
        };
        writeln!(out, "{}{fd3}", command_line(&c)).unwrap();
    }
    insta::assert_snapshot!(out);
}

#[test]
fn environment_snapshot() {
    let mut out = String::from("env_clear(), then:\n");
    for (k, v) in ENVIRONMENT {
        writeln!(out, "{k}={v}").unwrap();
    }
    insta::assert_snapshot!(out);
}

#[test]
fn timeout_snapshot() {
    let mut out = String::new();
    for c in all_commands() {
        writeln!(
            out,
            "{:<34} {:>5} s",
            c.kind().to_string(),
            c.timeout().as_secs()
        )
        .unwrap();
    }
    insta::assert_snapshot!(out);
}

#[test]
fn status_always_requests_format_version_1() {
    let argv = BootcCommand::Status.argv();
    assert!(argv.iter().any(|a| a == "--format-version=1"));
    assert!(argv.iter().any(|a| a == "--format=json"));
}

#[test]
fn argv_is_one_token_per_argument_and_never_injects_options() {
    for c in all_commands() {
        let argv = c.argv();
        assert!(!argv.is_empty());
        for a in &argv {
            let a = a.to_str().unwrap();
            assert!(!a.is_empty() && !a.contains(char::is_whitespace), "{a:?}");
        }
        if let BootcCommand::SwitchDownloadOnly { target } = &c {
            let last = argv.last().unwrap().to_str().unwrap();
            assert_eq!(last, target.to_string());
            assert!(!last.starts_with('-'));
        }
    }
}

#[test]
fn contract_timeouts() {
    assert_eq!(CommandKind::Status.timeout(), Duration::from_secs(30));
    assert_eq!(CommandKind::UpgradeCheck.timeout(), Duration::from_mins(5));
    assert_eq!(
        CommandKind::UpgradeDownloadOnly.timeout(),
        Duration::from_hours(2)
    );
    assert_eq!(
        CommandKind::SwitchDownloadOnly.timeout(),
        Duration::from_hours(2)
    );
    assert_eq!(
        CommandKind::UpgradeFromDownloaded.timeout(),
        Duration::from_mins(10)
    );
    assert_eq!(CommandKind::Rollback.timeout(), Duration::from_mins(10));
    assert_eq!(CommandKind::PinRollback.timeout(), Duration::from_mins(10));
    assert_eq!(CommandKind::OstreeStatus.timeout(), Duration::from_secs(30));
    assert_eq!(CommandKind::Unpin.timeout(), Duration::from_mins(10));
}

#[test]
fn runtime_input_is_limited_to_a_validated_digest() {
    assert!(matches!(
        ImageRef::at_digest(APEX_STABLE_REPOSITORY, "sha256:../../etc"),
        Err(ImageRefError::Digest(_))
    ));
    assert!(matches!(
        ImageRef::at_digest(APEX_STABLE_REPOSITORY, &format!("{PINNED} --apply")),
        Err(ImageRefError::Digest(_))
    ));
    assert_eq!(
        ImageRef::try_from_static("--apply/x"),
        Err(ImageRefError::BadStart)
    );
}
