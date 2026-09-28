//! `apex-boot-assess`: judges each boot once and acts on the verdict (ADR 0009).
//!
//! Run by `apex-boot-assess.service` after `graphical.target`. It
//!
//! 1. waits (bounded) for a greeter or user session;
//! 2. runs the required checks (session, `NetworkManager` and logind active, `apex-updated`
//!    answers `Ping`, no required unit failed, the grub.cfg snippet marker) and the
//!    file-based reported ones in one concurrent batch through the library's runner (30 s per
//!    check), while `bootc status` names the booted digest and the image source;
//! 3. adds the failed check `forced-red` when the qualification hook
//!    `/var/lib/apex/force-red` names the booted digest (see `--help`);
//! 4. reads the Apex variables from grubenv, takes the boot order from the same `bootc
//!    status`, and asks logind whether a user may be logged in (assumed when logind cannot
//!    answer);
//! 5. applies [`apex_boot::decide()`] and performs its steps: grubenv through `grub2-editenv`
//!    (in a private mount namespace in which the read-only `/boot` is remounted read-write,
//!    [`apex_boot::Editenv::system`]), `/var/lib/apex/health-failure.json`, `ReportBoot` to
//!    `apex-updated`, after a failed fallback report a second `bootc status` to reconcile
//!    grubenv with the boot order, and `systemctl reboot` only for a failed update while
//!    nobody is logged in.
//!
//! The whole run is bounded by [`apex_boot::ASSESS_TIME_BUDGET`].
//!
//! Exit status: 0 when every step succeeded; 1 when grubenv could not be interpreted
//! (nothing was changed) or any step failed (the others were still carried out as far as
//! the library's failure policy allows).
//!
//! With `--emergency-watchdog` (run by the `emergency.service` and `rescue.service`
//! drop-ins) it does none of that: it runs [`apex_boot::emergency::run`], which schedules a
//! reboot in 60 s only when GRUB counted this boot of an update on trial, and prints to the
//! console (standard output). No tokio runtime, no D-Bus, no logging to stderr; bounded by
//! [`apex_boot::emergency::WATCHDOG_TIME_BUDGET`]. Exit status 1 when grubenv could not be
//! read or the reboot could not be scheduled (the drop-in ignores it).
//!
//! Threads: a tokio `current_thread` runtime on the main thread drives zbus and apex-bootc;
//! the assessment itself runs on a blocking thread and reaches the runtime through its
//! handle (see [`system`]).

mod assess;
mod cli;
#[cfg(test)]
mod fakes;
mod ping;
mod ports;
mod record;
mod reported;
mod session;
mod system;
mod view;

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context as _;
use apex_boot::checks::grub_cfg::GrubCfgMarkerCheck;
use apex_boot::checks::units::{ActiveUnitsCheck, FailedUnitsCheck};
use apex_boot::{ASSESS_TIME_BUDGET, Editenv, decide, emergency};
use clap::Parser as _;
use tokio::runtime::Handle;

use crate::assess::{Effects, LocalChecks, Ports};
use crate::cli::Args;
use crate::record::FileStore;
use crate::system::{BootcStatus, Bus, Daemon, Logind, SystemClock, Systemctl};
use crate::view::DryRun;

fn main() -> ExitCode {
    let args = Args::parse();
    if args.emergency_watchdog {
        return emergency_watchdog(&args);
    }
    init_logging();
    match run(args) {
        Ok(code) => code,
        Err(err) => {
            tracing::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

/// The early-boot watchdog: everything it says goes to standard output, which the shell
/// units connect to the console.
fn emergency_watchdog(args: &Args) -> ExitCode {
    let paths = args.paths();
    let host = emergency::Host {
        grubenv: paths.grubenv,
        initrd_release: paths.initrd_release,
        ..emergency::Host::system()
    };
    let outcome = emergency::run(&host, args.dry_run, &mut std::io::stdout().lock());
    if outcome.succeeded() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Logs to stderr, which systemd sends to the journal with its own timestamps.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .init();
}

fn run(args: Args) -> anyhow::Result<ExitCode> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the tokio runtime")?;
    let handle = runtime.handle().clone();
    // The main thread drives the runtime (I/O, timers, zbus's tasks) while the assessment
    // blocks on its own thread.
    let result = runtime.block_on(async move {
        tokio::task::spawn_blocking(move || assess_system(&args, handle)).await
    });
    // Checks that timed out may still hold runtime work; do not wait for them.
    runtime.shutdown_background();
    result.context("the assessment thread panicked")?
}

fn assess_system(args: &Args, handle: Handle) -> anyhow::Result<ExitCode> {
    let paths = args.paths();
    let bus = Arc::new(Bus::new(handle.clone()));
    let ports = Ports {
        sessions: Arc::new(Logind::new(Arc::clone(&bus))),
        updater: Arc::new(Daemon::new(bus)),
        host: Arc::new(BootcStatus::new(handle)),
        clock: Arc::new(SystemClock),
    };
    let local = LocalChecks {
        core_units: Box::new(ActiveUnitsCheck::system()),
        failed_units: Box::new(FailedUnitsCheck::system()),
        grub_cfg: Box::new(GrubCfgMarkerCheck::new(&paths.grub_cfg)),
        reported: reported::file_checks(&paths.root),
        force_red: paths.force_red.clone(),
    };

    tracing::info!(
        "assessing this boot (at most {} s){}",
        ASSESS_TIME_BUDGET.as_secs(),
        if args.dry_run { "; dry run" } else { "" }
    );
    let gathered = assess::gather(&ports, local, &paths.grubenv)
        .context("cannot assess this boot; grubenv was left unchanged")?;
    let decision = decide(&gathered.input);
    tracing::info!("decision: {decision}");

    if args.dry_run {
        let json = DryRun::new(&gathered, &decision)
            .to_json()
            .context("cannot encode the decision")?;
        println!("{json}");
        return Ok(ExitCode::SUCCESS);
    }

    // Without --dry-run the paths are the system's (--grubenv and --root require it), so
    // this is /boot/grub2/grubenv, written in a private mount namespace with /boot
    // remounted read-write.
    let mut writer = Editenv::for_file(&paths.grubenv);
    let store = FileStore::new(paths.health_failure.clone());
    let outcomes = assess::perform(
        &decision.steps(),
        &ports,
        &mut Effects {
            writer: &mut writer,
            store: &store,
            rebooter: &Systemctl,
        },
    );
    if assess::any_failed(&outcomes) {
        tracing::error!("the assessment finished with failed steps (see above)");
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}
