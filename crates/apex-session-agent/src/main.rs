use std::process::ExitCode;

use apex_session_agent::agent::Agent;
use apex_session_agent::notifications::DbusNotifier;
use apex_session_agent::paths::Paths;
use apex_session_agent::update1;
use apex_session_agent::{INTERFACE_SCHEMA_ID, SCHEMA_ID};
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
        tracing::error!(
            "neither HOME nor absolute XDG_CONFIG_HOME, XDG_DATA_HOME and XDG_STATE_HOME are set"
        );
        return ExitCode::FAILURE;
    };
    let source = gio::SettingsSchemaSource::default();
    let lookup = |id| source.as_ref().and_then(|source| source.lookup(id, true));
    let Some(schema) = lookup(SCHEMA_ID) else {
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
    let interface = lookup(INTERFACE_SCHEMA_ID)
        .map(|schema| gio::Settings::new_full(&schema, gio::SettingsBackend::NONE, None));
    let notifier = DbusNotifier::new(connection.clone());
    let agent = Agent::start(
        settings,
        interface,
        paths,
        Box::new(DbusNotifier::new(connection)),
    );
    let _subscriptions = notifier.subscribe(&agent);
    let _update1 = match gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) {
        Ok(system) => Some(update1::subscribe(&system, &agent)),
        Err(error) => {
            tracing::warn!(%error, "cannot connect to the system bus: no \"Updates Ready\"");
            None
        }
    };
    glib::MainLoop::new(None, false).run();
    ExitCode::SUCCESS
}
