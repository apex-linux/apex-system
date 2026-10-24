use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::profile::DEFAULT_PROFILE;
use crate::status::STATUS_JSON_HELP;

const AFTER_HELP: &str = "\
Exit status: 0 done, 1 error (including a command-line usage error), 2 refused by an
update gate (power, network, space, ...), 3 nothing to do.

apexctl never reboots the computer. The update commands talk to apex-updated on the
system bus; polkit may ask for a password.";

#[derive(Debug, Parser)]
#[command(
    name = "apexctl",
    version,
    about = "Control and inspect Apex: OS updates, hardware state and build-time helpers",
    after_help = AFTER_HELP,
    propagate_version = false
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(
        about = "Show the update state: booted, staged and rollback images, degraded parts",
        after_long_help = STATUS_JSON_HELP
    )]
    Status {
        #[arg(
            long,
            help = "Print JSON with a stable schema (apexctl status --help describes it)"
        )]
        json: bool,
    },
    #[command(subcommand, about = "Check for, download, apply or undo OS updates")]
    Update(UpdateCommand),
    #[command(subcommand, about = "Read-only hardware diagnostics for this laptop")]
    Hw(HwCommand),
    #[command(
        subcommand,
        about = "Render the files derived from the hardware profile"
    )]
    Profile(ProfileCommand),
    #[command(subcommand, about = "Helpers used while building the image")]
    Build(BuildCommand),
}

#[derive(Debug, Subcommand)]
pub enum UpdateCommand {
    #[command(
        about = "Check whether a newer version of Apex is available on the followed channel",
        long_about = concat!(
            "Check whether a newer version of Apex is available on the followed channel.\n\n",
            "Exits 0 without checking when the update service is busy, for example while an ",
            "update is being downloaded (the download fetches from the channel itself); the ",
            "output starts with \"Not checked:\" and says why."
        )
    )]
    Check,
    #[command(about = "Download the available update and keep it locked until it is applied")]
    Download {
        #[arg(
            long,
            help = concat!(
                "Download even on battery or on a metered network (never overrides the space, ",
                "denylist or bootc-state gates)"
            )
        )]
        force: bool,
    },
    #[command(about = concat!(
        "Use the downloaded update at the next boot. Reboot to finish; apexctl never ",
        "reboots"
    ))]
    Apply {
        #[arg(
            long,
            value_name = "DIGEST",
            value_parser = digest_arg,
            help = concat!(
                "Digest to apply (sha256:…); defaults to the staged digest that `status` shows. ",
                "The daemon refuses if the staged update is not this digest"
            )
        )]
        digest: Option<String>,
    },
    #[command(about = "Boot the previous version next time (asks for an administrator password)")]
    Rollback {
        #[arg(
            long,
            help = "Also mark the running version as bad so it is never offered again"
        )]
        mark_bad: bool,
    },
    #[command(
        about = "List the package changes from the running image to the downloaded one",
        long_about = concat!(
            "List the package changes from the running image to the downloaded one.\n\n",
            "Fails (exit 1) while the update service does not know the system's state yet, for ",
            "example right after it started; try again in a moment."
        )
    )]
    Changes,
    #[command(
        about = "Clear the notice that an update failed its health checks, after reading it",
        long_about = concat!(
            "Clear the notice that an update failed its health checks, after reading it.\n\n",
            "Only the notice goes away: nothing is rolled back. Exits 3 when no update has failed ",
            "its health checks."
        )
    )]
    Acknowledge,
    #[command(
        about = "Automatic run for the systemd timer: check, then download if allowed (root only)",
        long_about = concat!(
            "Automatic run for the systemd timer: check, then download if allowed (root only).\n\n",
            "Exits 0 when the run only has to wait for the next one: another update operation ",
            "is running, or the image registry cannot be reached."
        )
    )]
    Automatic,
}

/// Validated while parsing: D-Bus activation of the update service runs `bootc status`,
/// which can take 30 s before a typo would be reported.
fn digest_arg(text: &str) -> Result<String, apex_bootc::DigestError> {
    apex_bootc::Digest::parse(text).map(|digest| digest.as_str().to_owned())
}

#[derive(Debug, Subcommand)]
pub enum HwCommand {
    #[command(about = concat!(
        "Check audio init verbs, charge limit, dGPU power state, fingerprint reader and Secure ",
        "Boot. Read-only"
    ))]
    Report {
        #[arg(long, help = "Print JSON")]
        json: bool,
        #[command(flatten)]
        root: RootArg,
    },
    #[command(subcommand, about = "Audio codec diagnostics")]
    Audio(AudioCommand),
}

#[derive(Debug, Subcommand)]
pub enum AudioCommand {
    #[command(about = concat!(
        "Parse the ALC294's /proc/asound codec dump: pins, COEFs (when the kernel's COEF dump ",
        "option is on) and the loaded init verbs. Read-only"
    ))]
    Snapshot {
        #[arg(
            long = "proc",
            value_name = "FILE",
            help = "Parse this codec dump instead of finding the ALC294 under /proc/asound"
        )]
        proc_file: Option<PathBuf>,
        #[arg(long, help = "Print JSON")]
        json: bool,
        #[command(flatten)]
        root: RootArg,
    },
}

#[derive(Debug, Args)]
pub struct RootArg {
    #[arg(
        long = "sysroot",
        value_name = "DIR",
        default_value = "/",
        hide = true,
        help = "Read /sys, /proc and /usr under this directory instead of /"
    )]
    pub sysroot: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    #[command(about = concat!(
        "Render monitors.xml (always) and the ALC294 patch firmware and modprobe file (once the ",
        "COEFs are measured) from the hardware profile"
    ))]
    Render {
        #[arg(
            long,
            value_name = "FILE",
            default_value = DEFAULT_PROFILE,
            help = "The hardware profile"
        )]
        profile: PathBuf,
        #[arg(
            long,
            value_name = "DIR",
            help = "Output directory [default: generated/ next to the profile]"
        )]
        out: Option<PathBuf>,
        #[arg(
            long,
            help = concat!(
                "Change nothing; exit 1 and print a diff when the files in the output directory ",
                "differ from the rendering (or the firmware list differs from the profile)"
            )
        )]
        check: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum BuildCommand {
    #[command(about = "Write DIR/usr/share/apex/manifest.json from the rpm database under DIR")]
    Manifest {
        #[arg(
            long,
            value_name = "DIR",
            help = "Root of the image being built (`/` inside the Containerfile)"
        )]
        root: PathBuf,
        #[arg(
            long,
            value_name = "V",
            help = "Image version FEDORA.YYYYMMDD.RUN, e.g. 45.20260924.117"
        )]
        version: String,
        #[arg(
            long,
            value_name = "R",
            help = "Git commit of the Apex repository (40 hex digits)"
        )]
        revision: String,
        #[arg(
            long,
            value_name = "B",
            help = "Base image, pinned by digest (REPOSITORY@sha256:…)"
        )]
        base: String,
        #[arg(
            long = "degraded",
            value_name = "X",
            help = concat!(
                "A component shipped degraded (adwaita, audio, files, fingerprint, nvidia, settings, ",
                "shell, terminal); ",
                "repeat for several"
            )
        )]
        degraded: Vec<String>,
        #[arg(
            long = "held",
            value_name = "FAMILY",
            help = concat!(
                "A rebuild-lane family held at its last good build (gnome-shell, ...); repeat ",
                "for several"
            )
        )]
        held: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn contract_surface_parses() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("apexctl").chain(args.iter().copied()))
                .map(|cli| format!("{:?}", cli.command))
        };
        for args in [
            &["status"][..],
            &["status", "--json"],
            &["update", "check"],
            &["update", "download", "--force"],
            &["update", "apply"],
            &["update", "apply", "--digest", &VALID_DIGEST],
            &["update", "rollback", "--mark-bad"],
            &["update", "changes"],
            &["update", "acknowledge"],
            &["update", "automatic"],
            &["hw", "report", "--json"],
            &["hw", "audio", "snapshot", "--proc", "/tmp/codec", "--json"],
            &[
                "profile",
                "render",
                "--profile",
                "p.toml",
                "--out",
                "o",
                "--check",
            ],
            &[
                "build",
                "manifest",
                "--root",
                "/",
                "--version",
                "45.20260924.1",
                "--revision",
                "r",
                "--base",
                "b",
                "--degraded",
                "audio",
                "--degraded",
                "fingerprint",
            ],
        ] {
            assert!(parse(args).is_ok(), "{args:?}: {:?}", parse(args));
        }
        let manifest = parse(&[
            "build",
            "manifest",
            "--root",
            "/",
            "--version",
            "V",
            "--revision",
            "R",
            "--base",
            "B",
            "--degraded",
            "audio",
            "--degraded",
            "nvidia",
            "--held",
            "gnome-shell",
        ])
        .unwrap();
        assert!(manifest.contains(r#"degraded: ["audio", "nvidia"]"#));
        assert!(manifest.contains(r#"held: ["gnome-shell"]"#));
        let render = parse(&["profile", "render"]).unwrap();
        assert!(render.contains("hardware/m7400qc/profile.toml"));
        assert!(parse(&["update", "reboot"]).is_err());
        assert!(parse(&["update", "discard"]).is_err());
        assert!(parse(&["update", "channel", "stable"]).is_err());
    }

    static VALID_DIGEST: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| format!("sha256:{}", "5".repeat(64)));

    #[test]
    fn apply_digest_is_validated_while_parsing() {
        let digest = |value: &str| {
            Cli::try_parse_from(["apexctl", "update", "apply", "--digest", value])
                .map(|cli| cli.command)
        };
        match digest(&VALID_DIGEST) {
            Ok(Command::Update(UpdateCommand::Apply { digest })) => {
                assert_eq!(digest.as_deref(), Some(VALID_DIGEST.as_str()));
            }
            other => panic!("{other:?}"),
        }
        for bad in [
            "nope",
            "sha256:00",
            "5f70bf18a086",
            &VALID_DIGEST.to_uppercase(),
            &format!("{}0", *VALID_DIGEST),
        ] {
            let error = digest(bad).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{bad}"
            );
            assert!(error.to_string().contains("--digest"), "{error}");
        }
    }

    #[test]
    fn status_help_documents_every_json_key() {
        use crate::status::{STATUS_JSON_HELP, StatusJson, tests::staged_status};
        let help = Cli::command()
            .find_subcommand_mut("status")
            .map(|status| status.render_long_help().to_string())
            .unwrap();
        assert!(
            help.contains(STATUS_JSON_HELP.lines().next().unwrap()),
            "{help}"
        );
        let json = serde_json::to_value(StatusJson::new(&staged_status())).unwrap();
        let keys = json
            .as_object()
            .unwrap()
            .keys()
            .chain(json["booted"].as_object().unwrap().keys());
        for key in keys {
            let documented = STATUS_JSON_HELP.lines().any(|line| {
                let line = line.trim_start();
                let names = line.split_once("  ").map_or(line, |(names, _)| names);
                names.split(", ").any(|name| name == key)
            });
            assert!(documented, "{key} is missing from STATUS_JSON_HELP");
        }
    }

    fn unwrapped_update_help(name: &str) -> String {
        let help = Cli::command()
            .find_subcommand_mut("update")
            .and_then(|update| update.find_subcommand_mut(name))
            .map(|command| command.render_long_help().to_string())
            .unwrap();
        help.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn update_help_documents_the_busy_behaviour() {
        let check = unwrapped_update_help("check");
        assert!(
            check.contains("Exits 0 without checking when the update service is busy"),
            "{check}"
        );
        let changes = unwrapped_update_help("changes");
        assert!(
            changes.contains("Fails (exit 1) while the update service"),
            "{changes}"
        );
    }

    #[test]
    fn acknowledge_takes_no_arguments() {
        let parsed = Cli::try_parse_from(["apexctl", "update", "acknowledge"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Update(UpdateCommand::Acknowledge)
        ));
        assert!(Cli::try_parse_from(["apexctl", "update", "acknowledge", "sha256:00"]).is_err());
    }
}
