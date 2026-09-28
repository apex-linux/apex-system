//! `apex-boot-assess`: judges each boot once and acts on the verdict, or with
//! `--emergency-watchdog` runs the early-boot watchdog of the emergency and rescue shells.

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

/// The shell units connect standard output to the console.
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

/// systemd adds its own timestamps in the journal.
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
    // blocks on its own thread and reaches the runtime through its handle.
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
        force_red_path: paths.force_red.clone(),
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

    // Without --dry-run this is always /boot/grub2/grubenv: --grubenv and --root require it.
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
