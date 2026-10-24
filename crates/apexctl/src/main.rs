use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use anyhow::Context as _;
use apexctl::cli::{
    AudioCommand, BuildCommand, Cli, Command, HwCommand, ProfileCommand, UpdateCommand,
};
use apexctl::exit::Exit;
use apexctl::hw::{self, SysRoot};
use apexctl::manifest::{self, ManifestInput};
use apexctl::profile::Profile;
use apexctl::render::{self, AudioOutcome, FileAction};
use apexctl::service::{ClientError, DbusService, UpdateService as _};
use apexctl::status::{StatusJson, render_status};
use apexctl::update;
use clap::Parser as _;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return usage(&error).into(),
    };
    match run(cli) {
        Ok(exit) => exit.into(),
        Err(error) => {
            eprintln!("apexctl: {error:#}");
            Exit::Error.into()
        }
    }
}

fn usage(error: &clap::Error) -> Exit {
    if error.print().is_err() {
        return Exit::Error;
    }
    if error.use_stderr() {
        Exit::Error
    } else {
        Exit::Ok
    }
}

fn emit(text: &str) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.context("cannot write to stdout"),
    }
}

fn emit_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    emit(&text)
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")
}

fn run(cli: Cli) -> anyhow::Result<Exit> {
    match cli.command {
        Command::Status { json } => runtime()?.block_on(status(json)),
        Command::Update(command) => runtime()?.block_on(update_command(command)),
        Command::Hw(HwCommand::Report { json, root }) => {
            hw_report(&SysRoot::new(root.sysroot), json)
        }
        Command::Hw(HwCommand::Audio(AudioCommand::Snapshot {
            proc_file,
            json,
            root,
        })) => audio_snapshot(&SysRoot::new(root.sysroot), proc_file.as_deref(), json),
        Command::Profile(ProfileCommand::Render {
            profile,
            out,
            check,
        }) => profile_render(&profile, out.as_deref(), check),
        Command::Build(BuildCommand::Manifest {
            root,
            version,
            revision,
            base,
            degraded,
            held,
        }) => build_manifest(
            &root,
            &ManifestInput {
                version,
                revision,
                base,
                degraded,
                held,
            },
        ),
    }
}

fn report_client_error(error: &ClientError, hint: Option<&str>) -> Exit {
    eprintln!("apexctl: {error}");
    if let Some(hint) = hint {
        eprintln!("{hint}");
    }
    error.exit()
}

async fn status(json: bool) -> anyhow::Result<Exit> {
    let result = async {
        let service = DbusService::connect().await?;
        service.status().await
    }
    .await;
    match result {
        Ok(status) if json => emit_json(&StatusJson::new(&status)).map(|()| Exit::Ok),
        Ok(status) => emit(&render_status(&status)).map(|()| Exit::Ok),
        Err(error) => Ok(report_client_error(&error, error.hint())),
    }
}

async fn update_command(command: UpdateCommand) -> anyhow::Result<Exit> {
    let service = match DbusService::connect().await {
        Ok(service) => service,
        Err(error) => return Ok(report_client_error(&error, error.hint())),
    };
    let result = match &command {
        UpdateCommand::Check => update::check(&service).await,
        UpdateCommand::Download { force } => update::download(&service, *force).await,
        UpdateCommand::Apply { digest } => update::apply(&service, digest.as_deref()).await,
        UpdateCommand::Rollback { mark_bad } => update::rollback(&service, *mark_bad).await,
        UpdateCommand::Changes => update::changes(&service).await,
        UpdateCommand::Acknowledge => update::acknowledge(&service).await,
        UpdateCommand::Automatic => update::automatic(&service).await,
        UpdateCommand::Auto { setting } => update::auto(&service, *setting).await,
    };
    match result {
        Ok(text) => emit(&text).map(|()| Exit::Ok),
        Err(error) => Ok(report_client_error(&error, update::hint(&command, &error))),
    }
}

fn hw_report(root: &SysRoot, json: bool) -> anyhow::Result<Exit> {
    let mut facts = hw::report::gather(root);
    if root.is_host() {
        facts.upower = runtime()?.block_on(async {
            let connection = apex_dbus::zbus::Connection::system().await.ok()?;
            hw::upower::read(&connection, hw::machine::BATTERY).await
        });
    }
    let report = hw::report::assess(&facts);
    if json {
        emit_json(&report)?;
    } else {
        emit(&hw::report::render(&report))?;
    }
    Ok(Exit::Ok)
}

fn audio_snapshot(root: &SysRoot, proc_file: Option<&Path>, json: bool) -> anyhow::Result<Exit> {
    let snapshot = match proc_file {
        Some(path) => hw::audio::snapshot_file(path)?,
        None => hw::audio::snapshot(root)?,
    };
    if json {
        emit_json(&snapshot)?;
    } else {
        emit(&hw::audio::render(&snapshot))?;
    }
    Ok(Exit::Ok)
}

fn profile_render(profile_path: &Path, out: Option<&Path>, check: bool) -> anyhow::Result<Exit> {
    let profile = Profile::load(profile_path)
        .with_context(|| format!("invalid hardware profile {}", profile_path.display()))?;
    let rendered = render::render(&profile)
        .with_context(|| format!("cannot render {}", profile_path.display()))?;
    let out_dir = out.map_or_else(
        || render::default_output_dir(profile_path),
        Path::to_path_buf,
    );

    if let AudioOutcome::NotMeasured = rendered.audio {
        eprintln!(
            "notice: {} has no [audio.measurements]: no ALC294 patch firmware is rendered and the \
             image is labelled degraded=audio. See docs/hardware/AUDIO.md to measure the COEFs.",
            profile_path.display()
        );
    }

    if check {
        let differences = render::check_outputs(&out_dir, &rendered)?;
        for difference in &differences {
            eprint!("{}", difference.diff);
        }
        if differences.is_empty() {
            emit(&format!("{} is up to date.\n", out_dir.display()))?;
            return Ok(Exit::Ok);
        }
        eprintln!(
            "apexctl: {} does not match {}; run `apexctl profile render` (or `just render`) \
             and commit the result.",
            out_dir.display(),
            profile_path.display()
        );
        return Ok(Exit::Error);
    }

    let mut text = String::new();
    for (name, action) in render::write_outputs(&out_dir, &rendered)? {
        let path = out_dir.join(&name);
        let line = match action {
            FileAction::Written => format!("wrote {}\n", path.display()),
            FileAction::Unchanged => format!("unchanged {}\n", path.display()),
            FileAction::Removed => format!("removed stale {}\n", path.display()),
            FileAction::Absent => continue,
        };
        text.push_str(&line);
    }
    emit(&text)?;
    Ok(Exit::Ok)
}

fn build_manifest(root: &Path, input: &ManifestInput) -> anyhow::Result<Exit> {
    let packages = manifest::query_rpm(root)?;
    let built = manifest::build(input, packages)?;
    for (name, arch) in &built.duplicates {
        eprintln!(
            "warning: {name}.{arch} is installed more than once; the manifest keeps the newest"
        );
    }
    let path = manifest::write(root, &built.manifest)?;
    emit(&format!(
        "wrote {} ({} packages, version {})\n",
        path.display(),
        built.manifest.packages.len(),
        built.manifest.version
    ))?;
    Ok(Exit::Ok)
}
