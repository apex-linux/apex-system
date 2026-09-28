//! The command line (contract §7), defined with clap's derive API.

use std::path::PathBuf;

use apex_dbus::Channel;
use clap::builder::{PossibleValuesParser, TypedValueParser};
use clap::{Args, Parser, Subcommand};

use crate::profile::DEFAULT_PROFILE;
use crate::status::STATUS_JSON_HELP;

const AFTER_HELP: &str = "\
Exit status: 0 done, 1 error (including a command-line usage error), 2 refused by an
update gate (power, network, space, ...), 3 nothing to do.

apexctl never reboots the computer. The update commands talk to apex-updated on the
system bus; polkit may ask for a password.";

/// Control and inspect Apex: OS updates, hardware state and build-time helpers.
#[derive(Debug, Parser)]
#[command(name = "apexctl", version, after_help = AFTER_HELP, propagate_version = false)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show the update state: booted, staged and rollback images, degraded parts, staleness.
    #[command(after_long_help = STATUS_JSON_HELP)]
    Status {
        /// Print JSON with a stable schema (apexctl status --help describes it).
        #[arg(long)]
        json: bool,
    },
    /// Check for, download, apply or undo OS updates.
    #[command(subcommand)]
    Update(UpdateCommand),
    /// Read-only hardware diagnostics for this laptop.
    #[command(subcommand)]
    Hw(HwCommand),
    /// Render the files derived from the hardware profile.
    #[command(subcommand)]
    Profile(ProfileCommand),
    /// Helpers used while building the image.
    #[command(subcommand)]
    Build(BuildCommand),
}

/// `apexctl update …`.
#[derive(Debug, Subcommand)]
pub enum UpdateCommand {
    /// Check whether a newer version of Apex is available on the followed channel.
    ///
    /// Exits 0 without checking when the update service is busy, for example while an
    /// update is being downloaded (the download fetches from the channel itself); the
    /// output starts with "Not checked:" and says why.
    Check,
    /// Download the available update and keep it locked until it is applied.
    Download {
        /// Download even on battery or on a metered network (never overrides the space,
        /// denylist or bootc-state gates).
        #[arg(long)]
        force: bool,
    },
    /// Use the downloaded update at the next boot. Reboot to finish; apexctl never reboots.
    Apply {
        /// Digest to apply (sha256:…); defaults to the staged digest that `status` shows.
        /// The daemon refuses if the staged update is not this digest.
        #[arg(long, value_name = "DIGEST", value_parser = digest_arg)]
        digest: Option<String>,
    },
    /// Lock an applied update again so the next boot keeps the current version.
    ///
    /// Locking again downloads from the update channel, so it needs the network. When the
    /// channel has a newer build by then, that build is downloaded and locked in place of
    /// the applied one; the output names the build that is locked now.
    Discard,
    /// Boot the previous version next time (asks for an administrator password).
    Rollback {
        /// Also mark the running version as bad so it is never offered again.
        #[arg(long)]
        mark_bad: bool,
    },
    /// List the package changes from the running image to the downloaded one.
    ///
    /// Fails (exit 1) while the update service does not know the system's state yet, for
    /// example right after it started; try again in a moment.
    Changes,
    /// Clear the notice that an update failed its health checks, after reading it.
    ///
    /// Only the notice goes away: nothing is rolled back. Exits 3 when no update has failed
    /// its health checks.
    Acknowledge,
    /// Follow another update channel (asks for an administrator password).
    ///
    /// The channel's image is downloaded and kept locked, like `update download`. To finish
    /// the switch, run `apexctl update apply` and restart. The image is installed even when
    /// it is older than the running one; when it is the same build, only the channel
    /// changes and no boot check is armed. Exits 3 when the channel is already followed.
    Channel {
        /// stable: qualified builds signed with the release key; testing: every CI build,
        /// signed with the CI key.
        #[arg(value_name = "CHANNEL", value_parser = channel_parser())]
        channel: Channel,
    },
    /// Automatic run for the systemd timer: check, then download if allowed (root only).
    ///
    /// Exits 0 when the run only has to wait for the next one: another update operation
    /// is running, or the image registry cannot be reached.
    Automatic,
}

/// Validates `apply --digest` while parsing, so a typo is a usage error before the update
/// service is started (D-Bus activation runs `bootc status`, which can take 30 s).
fn digest_arg(text: &str) -> Result<String, apex_bootc::DigestError> {
    apex_bootc::Digest::parse(text).map(|digest| digest.as_str().to_owned())
}

/// Parses a channel name into the typed [`Channel`] that `SwitchChannel` takes, offering
/// exactly the contract strings (`stable`, `testing`) in `--help` and in errors.
fn channel_parser() -> impl TypedValueParser<Value = Channel> {
    PossibleValuesParser::new(Channel::ALL.iter().copied().map(Channel::as_str))
        .try_map(|name| name.parse::<Channel>())
}

/// `apexctl hw …`.
#[derive(Debug, Subcommand)]
pub enum HwCommand {
    /// Check audio init verbs, charge limit, dGPU power state, fingerprint reader and Secure
    /// Boot. Read-only.
    Report {
        /// Print JSON.
        #[arg(long)]
        json: bool,
        /// Hidden root prefix for tests.
        #[command(flatten)]
        root: RootArg,
    },
    /// Audio codec diagnostics.
    #[command(subcommand)]
    Audio(AudioCommand),
}

/// `apexctl hw audio …`.
#[derive(Debug, Subcommand)]
pub enum AudioCommand {
    /// Parse the ALC294's /proc/asound codec dump: pins, COEFs (when the kernel's COEF
    /// dump option is on) and the loaded init verbs. Read-only.
    Snapshot {
        /// Parse this codec dump instead of finding the ALC294 under /proc/asound.
        #[arg(long = "proc", value_name = "FILE")]
        proc_file: Option<PathBuf>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
        /// Hidden root prefix for tests.
        #[command(flatten)]
        root: RootArg,
    },
}

/// A hidden root prefix for `/sys` and `/proc`, for tests against a fake tree.
#[derive(Debug, Args)]
pub struct RootArg {
    /// Read /sys, /proc and /usr under this directory instead of /.
    #[arg(long = "sysroot", value_name = "DIR", default_value = "/", hide = true)]
    pub sysroot: PathBuf,
}

/// `apexctl profile …`.
#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// Render monitors.xml (always) and the ALC294 patch firmware and modprobe file (once
    /// the COEFs are measured) from the hardware profile.
    Render {
        /// The hardware profile.
        #[arg(long, value_name = "FILE", default_value = DEFAULT_PROFILE)]
        profile: PathBuf,
        /// Output directory [default: generated/ next to the profile].
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Change nothing; exit 1 and print a diff when the files in the output directory
        /// differ from the rendering (or the firmware list differs from the profile).
        #[arg(long)]
        check: bool,
    },
}

/// `apexctl build …`.
#[derive(Debug, Subcommand)]
pub enum BuildCommand {
    /// Write DIR/usr/share/apex/manifest.json from the rpm database under DIR.
    Manifest {
        /// Root of the image being built (`/` inside the Containerfile).
        #[arg(long, value_name = "DIR")]
        root: PathBuf,
        /// Image version FEDORA.YYYYMMDD.RUN, e.g. 45.20260924.117.
        #[arg(long, value_name = "V")]
        version: String,
        /// Git commit of the Apex repository (40 hex digits).
        #[arg(long, value_name = "R")]
        revision: String,
        /// Base image, pinned by digest (REPOSITORY@sha256:…).
        #[arg(long, value_name = "B")]
        base: String,
        /// A component shipped degraded (audio, fingerprint, nvidia, shell); repeat for several.
        #[arg(long = "degraded", value_name = "X")]
        degraded: Vec<String>,
        /// A rebuild-lane family held at its last good build (gnome-shell, ...); repeat
        /// for several.
        #[arg(long = "held", value_name = "FAMILY")]
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
            &["update", "discard"],
            &["update", "rollback", "--mark-bad"],
            &["update", "changes"],
            &["update", "acknowledge"],
            &["update", "channel", "stable"],
            &["update", "channel", "testing"],
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
    }

    #[test]
    fn the_channel_is_typed_and_limited_to_the_contract_values() {
        let channel = |args: &[&str]| {
            let args = ["apexctl", "update", "channel"]
                .into_iter()
                .chain(args.iter().copied());
            match Cli::try_parse_from(args).map(|cli| cli.command) {
                Ok(Command::Update(UpdateCommand::Channel { channel })) => Ok(channel),
                Ok(other) => panic!("parsed as {other:?}"),
                Err(error) => Err(error),
            }
        };
        assert_eq!(channel(&["stable"]).unwrap(), Channel::Stable);
        assert_eq!(channel(&["testing"]).unwrap(), Channel::Testing);
        for bad in [
            &["beta"][..],
            &["Stable"],
            &[" stable"],
            &[],
            &["stable", "testing"],
        ] {
            assert!(channel(bad).is_err(), "{bad:?}");
        }
        let error = channel(&["beta"]).unwrap_err().to_string();
        assert!(
            error.contains("[possible values: stable, testing]"),
            "{error}"
        );
        let help = Cli::command()
            .find_subcommand_mut("update")
            .and_then(|update| update.find_subcommand_mut("channel"))
            .map(|channel| channel.render_long_help().to_string())
            .unwrap();
        assert!(help.contains("administrator password"), "{help}");
        assert!(help.contains("stable"), "{help}");
        assert!(help.contains("testing"), "{help}");
    }

    /// A well-formed digest for the parser tests.
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
        // Every key of the real output, top level and deployment, has a line in the help.
        let json = serde_json::to_value(StatusJson::new(&staged_status())).unwrap();
        let keys = json
            .as_object()
            .unwrap()
            .keys()
            .chain(json["booted"].as_object().unwrap().keys());
        for key in keys {
            // A key line is "  key  description" or "  key, key, key".
            let documented = STATUS_JSON_HELP.lines().any(|line| {
                let line = line.trim_start();
                let names = line.split_once("  ").map_or(line, |(names, _)| names);
                names.split(", ").any(|name| name == key)
            });
            assert!(documented, "{key} is missing from STATUS_JSON_HELP");
        }
    }

    /// The long help of `apexctl update <name>`, with clap's line wrapping undone.
    fn update_help(name: &str) -> String {
        let help = Cli::command()
            .find_subcommand_mut("update")
            .and_then(|update| update.find_subcommand_mut(name))
            .map(|command| command.render_long_help().to_string())
            .unwrap();
        help.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn update_help_documents_the_busy_and_channel_behaviour() {
        // `update check` exits 0 when the service is busy (a download runs): the exit code
        // is part of the interface, so the help says it.
        let check = update_help("check");
        assert!(
            check.contains("Exits 0 without checking when the update service is busy"),
            "{check}"
        );
        let changes = update_help("changes");
        assert!(
            changes.contains("Fails (exit 1) while the update service"),
            "{changes}"
        );
        let channel = update_help("channel");
        assert!(
            channel.contains("run `apexctl update apply` and restart"),
            "{channel}"
        );
        assert!(channel.contains("no boot check is armed"), "{channel}");
        let discard = update_help("discard");
        assert!(discard.contains("needs the network"), "{discard}");
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
