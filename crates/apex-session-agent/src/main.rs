use std::process::ExitCode;

use apex_session_agent::SCHEMA_ID;
use apex_session_agent::agent::Agent;
use apex_session_agent::notifications::DbusNotifier;
use apex_session_agent::paths::Paths;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .init();

    let Some(paths) = Paths::from_env(|name| std::env::var_os(name)) else {
        tracing::error!("neither HOME nor absolute XDG_CONFIG_HOME and XDG_STATE_HOME are set");
        return ExitCode::FAILURE;
    };
    let schema =
        gio::SettingsSchemaSource::default().and_then(|source| source.lookup(SCHEMA_ID, true));
    let Some(schema) = schema else {
        tracing::info!("the {SCHEMA_ID} schema is not installed (apex-desktop); nothing to do");
        return ExitCode::SUCCESS;
    };
    let connection = match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
        Ok(connection) => connection,
        Err(error) => {
            tracing::error!(%error, "cannot connect to the session bus");
            return ExitCode::FAILURE;
        }
    };
    let settings = gio::Settings::new_full(&schema, gio::SettingsBackend::NONE, None);
    let notifier = DbusNotifier::new(connection.clone());
    let agent = Agent::start(settings, paths, Box::new(DbusNotifier::new(connection)));
    let _subscriptions = notifier.subscribe(&agent);
    glib::MainLoop::new(None, false).run();
    ExitCode::SUCCESS
}
