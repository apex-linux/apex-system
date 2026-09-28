//! Command line: `apex-boot-assess [--emergency-watchdog] [--dry-run] [--grubenv PATH]
//! [--root PATH]`.
//!
//! `apex-boot-assess.service` runs it without arguments. The drop-ins for
//! `emergency.service` and `rescue.service` run it with `--emergency-watchdog`
//! ([`apex_boot::emergency`]). The other flags exist for inspection on a running system
//! (`--dry-run`) and for tests (`--grubenv`, `--root`). The two redirecting flags require
//! `--dry-run`: they only redirect files, while `ReportBoot`, `systemctl reboot` and the
//! watchdog's `systemd-run` would still reach the real `apex-updated` and systemd, so a
//! fixture could otherwise make the daemon denylist and roll back a real deployment, or
//! reboot the machine.

use std::path::{Path, PathBuf};

use apex_boot::checks::force_red::FORCE_RED_PATH;
use apex_boot::checks::grub_cfg::GRUB_CFG_PATH;
use apex_boot::emergency::INITRD_RELEASE_PATH;
use apex_boot::health::HEALTH_FAILURE_PATH;
use apex_boot::writer::GRUBENV_PATH;
use clap::Parser;

/// Shown after the options in `--help`.
const AFTER_HELP: &str = "\
Qualification hook (ADR 0009 testing):
  If /var/lib/apex/force-red (below --root) exists and its content, with surrounding
  whitespace removed, equals the booted image digest, the assessment adds the failed
  required check \"forced-red\". The boot is then judged red like any other, so the KVM
  qualification can exercise the counted rollback. /var/lib/apex is root-only (0700);
  the file is ignored unless it is a regular file that neither it nor its directory lets
  group or others write. It changes nothing when it is absent, names another digest, or
  the booted digest is unknown.

Writes to /boot/grub2/grubenv run grub2-editenv in a private mount namespace in which
/boot is remounted read-write; the host keeps its read-only /boot.

Early-boot watchdog (--emergency-watchdog, run by the emergency.service and
rescue.service drop-ins before the shell starts):
  When GRUB counted this boot of an update on trial (apex_target set, apex_ok not 1,
  apex_counter 2, 1 or 0), schedule `systemctl reboot` in 60 s with systemd-run
  (units apex-emergency-reboot.timer/.service) and say so on the console, so the boot
  counter moves on and a failing update is rolled back. The reboot happens only if the
  emergency or rescue shell is still running then; `systemctl stop
  apex-emergency-reboot.timer` cancels it. Otherwise nothing is done or printed, apart
  from one line when grubenv cannot be read. It only reads grubenv, never writes it,
  and finishes within 35 s.";

/// Judges this boot once (ADR 0009) and acts on the verdict: confirms the boot or clears the
/// boot counter in grubenv, records a failed update, reports to apex-updated, and reboots
/// only for a failed update while nobody is logged in.
#[derive(Debug, Clone, Parser)]
#[command(name = "apex-boot-assess", version, after_help = AFTER_HELP)]
pub(crate) struct Args {
    /// Instead of the assessment, run the early-boot watchdog for an emergency or rescue
    /// shell: schedule a reboot in 60 s only while GRUB is counting an update on trial.
    #[arg(long)]
    pub(crate) emergency_watchdog: bool,

    /// Run the checks and print the decision as JSON (with --emergency-watchdog: print
    /// the watchdog's decision and the command it would run); change nothing, report
    /// nothing and never reboot.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// The GRUB environment block to read [default: boot/grub2/grubenv below --root].
    /// Only with --dry-run.
    #[arg(long, value_name = "PATH", requires = "dry_run")]
    pub(crate) grubenv: Option<PathBuf>,

    /// Directory that stands for / for the files the assessment reads (grub.cfg,
    /// /var/lib/apex, /sys, /usr/lib/firmware; for the watchdog grubenv and
    /// /etc/initrd-release) [default: /]. systemctl, logind, bootc and apex-updated are
    /// always the running system's. Only with --dry-run.
    #[arg(long, value_name = "PATH", requires = "dry_run")]
    pub(crate) root: Option<PathBuf>,
}

/// Every file path the assessment uses, resolved against `--root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Paths {
    /// `--root`.
    pub(crate) root: PathBuf,
    /// The grubenv to read and edit.
    pub(crate) grubenv: PathBuf,
    /// The GRUB config that must carry the snippet marker.
    pub(crate) grub_cfg: PathBuf,
    /// `health-failure.json`.
    pub(crate) health_failure: PathBuf,
    /// The qualification hook file (`force-red`).
    pub(crate) force_red: PathBuf,
    /// `/etc/initrd-release`, whose existence stops the watchdog.
    pub(crate) initrd_release: PathBuf,
}

impl Args {
    /// The paths for these arguments.
    pub(crate) fn paths(&self) -> Paths {
        let root = self.root.clone().unwrap_or_else(|| PathBuf::from("/"));
        Paths {
            grubenv: self
                .grubenv
                .clone()
                .unwrap_or_else(|| under(&root, GRUBENV_PATH)),
            grub_cfg: under(&root, GRUB_CFG_PATH),
            health_failure: under(&root, HEALTH_FAILURE_PATH),
            force_red: under(&root, FORCE_RED_PATH),
            initrd_release: under(&root, INITRD_RELEASE_PATH),
            root,
        }
    }
}

/// `absolute` (a path from the contract, starting with `/`) below `root`.
pub(crate) fn under(root: &Path, absolute: &str) -> PathBuf {
    root.join(absolute.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn no_arguments_means_the_system_paths() {
        let args = Args::try_parse_from(["apex-boot-assess"]).unwrap();
        assert!(!args.dry_run);
        assert_eq!(
            args.paths(),
            Paths {
                root: "/".into(),
                grubenv: "/boot/grub2/grubenv".into(),
                grub_cfg: "/boot/grub2/grub.cfg".into(),
                health_failure: "/var/lib/apex/health-failure.json".into(),
                force_red: "/var/lib/apex/force-red".into(),
                initrd_release: "/etc/initrd-release".into(),
            }
        );
        assert!(!args.emergency_watchdog);
    }

    #[test]
    fn root_and_grubenv_redirect_the_files() {
        let args =
            Args::try_parse_from(["apex-boot-assess", "--dry-run", "--root", "/tmp/r"]).unwrap();
        assert!(args.dry_run);
        let paths = args.paths();
        assert_eq!(paths.grubenv, Path::new("/tmp/r/boot/grub2/grubenv"));
        assert_eq!(paths.grub_cfg, Path::new("/tmp/r/boot/grub2/grub.cfg"));
        assert_eq!(
            paths.health_failure,
            Path::new("/tmp/r/var/lib/apex/health-failure.json")
        );
        assert_eq!(paths.force_red, Path::new("/tmp/r/var/lib/apex/force-red"));
        assert_eq!(paths.initrd_release, Path::new("/tmp/r/etc/initrd-release"));
        let args = Args::try_parse_from([
            "apex-boot-assess",
            "--dry-run",
            "--root",
            "/tmp/r",
            "--grubenv",
            "env",
        ])
        .unwrap();
        assert_eq!(args.paths().grubenv, Path::new("env"));
    }

    /// Finding: fixture files must never drive the real `ReportBoot` or reboot.
    #[test]
    fn redirected_files_need_a_dry_run() {
        for args in [
            &["apex-boot-assess", "--grubenv", "/tmp/fixture"][..],
            &["apex-boot-assess", "--root", "/tmp/r"][..],
            &["apex-boot-assess", "--root", "/tmp/r", "--grubenv", "env"][..],
        ] {
            let err = Args::try_parse_from(args).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{args:?}"
            );
            assert!(err.to_string().contains("--dry-run"), "{err}");
        }
    }

    /// The drop-ins pass exactly `--emergency-watchdog`; tests redirect its files only in
    /// a dry run, like the assessment's.
    #[test]
    fn the_watchdog_flag_and_its_dry_run() {
        let args = Args::try_parse_from(["apex-boot-assess", apex_boot::emergency::WATCHDOG_FLAG])
            .unwrap();
        assert!(args.emergency_watchdog);
        assert!(!args.dry_run);
        assert_eq!(args.paths().grubenv, Path::new("/boot/grub2/grubenv"));
        let args = Args::try_parse_from([
            "apex-boot-assess",
            "--emergency-watchdog",
            "--dry-run",
            "--root",
            "/tmp/r",
        ])
        .unwrap();
        assert!(args.emergency_watchdog && args.dry_run);
        assert_eq!(
            args.paths().initrd_release,
            Path::new("/tmp/r/etc/initrd-release")
        );
        let err = Args::try_parse_from([
            "apex-boot-assess",
            "--emergency-watchdog",
            "--grubenv",
            "/tmp/fixture",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn help_documents_the_qualification_hook() {
        let help = Args::command().render_help().to_string();
        assert!(help.contains("/var/lib/apex/force-red"), "{help}");
        assert!(help.contains("\"forced-red\""), "{help}");
        assert!(help.contains("names another digest"), "{help}");
        assert!(help.contains("private mount namespace"), "{help}");
    }

    /// The watchdog paragraph states the library's figures, not its own.
    #[test]
    fn help_documents_the_watchdog() {
        use apex_boot::emergency::{
            CANCEL_COMMAND, REBOOT_DELAY, REBOOT_UNIT, WATCHDOG_FLAG, WATCHDOG_TIME_BUDGET,
        };
        let help = Args::command().render_help().to_string();
        assert!(help.contains(WATCHDOG_FLAG), "{help}");
        assert!(
            AFTER_HELP.contains(&format!("in {} s", REBOOT_DELAY.as_secs())),
            "{AFTER_HELP}"
        );
        assert!(
            AFTER_HELP.contains(&format!("within {} s", WATCHDOG_TIME_BUDGET.as_secs())),
            "{AFTER_HELP}"
        );
        assert!(AFTER_HELP.contains(&format!("units {REBOOT_UNIT}.timer")));
        // The cancel command is wrapped over two lines in the help text.
        let words: Vec<&str> = AFTER_HELP.split_whitespace().collect();
        let cancel: Vec<&str> = CANCEL_COMMAND.split_whitespace().collect();
        assert!(
            words.windows(cancel.len()).any(|w| w
                .iter()
                .map(|s| s.trim_matches(['`', ';']))
                .eq(cancel.iter().copied())),
            "{AFTER_HELP}"
        );
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(Args::try_parse_from(["apex-boot-assess", "--reboot"]).is_err());
        assert!(Args::try_parse_from(["apex-boot-assess", "extra"]).is_err());
        Args::command().debug_assert();
    }
}
