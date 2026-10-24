use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gio::prelude::*;

use crate::agent::Agent;
use crate::notifications::{CALL_TIMEOUT_MS, Subscription};
use crate::updates::{Downloadable, Staged};

pub const BUS_NAME: &str = "io.github.apex_linux.Apex.Update1";
pub const PATH: &str = "/io/github/apex_linux/Apex/Update1";
pub const INTERFACE: &str = "io.github.apex_linux.Apex.Update1";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const STAGED: &str = "Staged";
const BOOTED: &str = "Booted";
const AVAILABLE: &str = "Available";
const AUTOMATIC_DOWNLOADS: &str = "AutomaticDownloads";
const HEALTH_FAILURE: &str = "HealthFailure";
const LAST_FALLBACK: &str = "LastFallback";
const WATCHED: [&str; 6] = [
    STAGED,
    BOOTED,
    AVAILABLE,
    AUTOMATIC_DOWNLOADS,
    HEALTH_FAILURE,
    LAST_FALLBACK,
];

fn version_of(dict: &glib::Variant) -> Option<String> {
    dict.get::<HashMap<String, glib::Variant>>()?
        .get("version")?
        .get::<String>()
}

/// Update1's `Available` dictionary; an empty one means nothing is available.
pub fn available_from(dict: &glib::Variant) -> Option<Downloadable> {
    let map = dict.get::<HashMap<String, glib::Variant>>()?;
    let digest = map
        .get("digest")
        .and_then(glib::Variant::get::<String>)
        .filter(|digest| !digest.is_empty())?;
    Some(Downloadable {
        digest,
        version: version_of(dict),
    })
}

/// Update1's `Staged` dictionary, when it holds an update the user has not applied: an
/// applied stage (`download-only` false) is already the user's choice.
pub fn staged_from(dict: &glib::Variant) -> Option<Staged> {
    let map = dict.get::<HashMap<String, glib::Variant>>()?;
    let text = |key: &str| map.get(key).and_then(glib::Variant::get::<String>);
    let digest = text("digest").filter(|digest| !digest.is_empty())?;
    let applied = map
        .get("download-only")
        .and_then(glib::Variant::get::<bool>)
        == Some(false);
    (!applied).then(|| Staged {
        digest,
        version: text("version"),
    })
}

/// Hands the agent each watched property found in `properties`; an older daemon may lack some.
/// `Booted` and `AutomaticDownloads` go first: the notices built from the others read them.
fn deliver(agent: &Rc<Agent>, properties: &HashMap<String, glib::Variant>) {
    if let Some(dict) = properties.get(BOOTED) {
        agent.booted_version(version_of(dict));
    }
    if let Some(enabled) = properties
        .get(AUTOMATIC_DOWNLOADS)
        .and_then(glib::Variant::get::<bool>)
    {
        agent.automatic_downloads(enabled);
    }
    if let Some(dict) = properties.get(STAGED) {
        agent.staged(staged_from(dict));
    }
    if let Some(dict) = properties.get(AVAILABLE) {
        agent.available(available_from(dict));
    }
    let text = |name: &str| properties.get(name).and_then(glib::Variant::get::<String>);
    if let Some(digest) = text(HEALTH_FAILURE) {
        agent.health_failure(&digest);
    }
    if let Some(digest) = text(LAST_FALLBACK) {
        agent.last_fallback(&digest);
    }
}

/// Watches the properties in `WATCHED` on the system bus. It never starts the daemon: the
/// name appears when the daily check or a front end starts it.
pub fn subscribe(connection: &gio::DBusConnection, agent: &Rc<Agent>) -> Subscription {
    let on_change = Rc::downgrade(agent);
    let fetch_connection = connection.clone();
    let signal = connection.subscribe_to_signal(
        Some(BUS_NAME),
        Some(PROPERTIES),
        Some("PropertiesChanged"),
        Some(PATH),
        Some(INTERFACE),
        gio::DBusSignalFlags::NONE,
        move |signal| {
            let Some((_, changed, invalidated)) =
                signal
                    .parameters
                    .get::<(String, HashMap<String, glib::Variant>, Vec<String>)>()
            else {
                return;
            };
            if let Some(agent) = on_change.upgrade() {
                deliver(&agent, &changed);
            }
            if invalidated
                .iter()
                .any(|name| WATCHED.contains(&name.as_str()))
            {
                fetch(&fetch_connection, on_change.clone());
            }
        },
    );
    let appeared = Rc::downgrade(agent);
    let watcher = gio::bus_watch_name_on_connection(
        connection,
        BUS_NAME,
        gio::BusNameWatcherFlags::NONE,
        move |connection, _, _| fetch(&connection, appeared.clone()),
        |_, _| {},
    );
    Subscription::new(
        vec![signal],
        Box::new(move || gio::bus_unwatch_name(watcher)),
    )
}

fn fetch(connection: &gio::DBusConnection, agent: Weak<Agent>) {
    connection.call(
        Some(BUS_NAME),
        PATH,
        PROPERTIES,
        "GetAll",
        Some(&(INTERFACE,).to_variant()),
        Some(&<(HashMap<String, glib::Variant>,)>::static_variant_type()),
        gio::DBusCallFlags::NO_AUTO_START,
        CALL_TIMEOUT_MS,
        gio::Cancellable::NONE,
        move |reply| match reply {
            Ok(reply) => {
                let Some((properties,)) = reply.get::<(HashMap<String, glib::Variant>,)>() else {
                    return;
                };
                if let Some(agent) = agent.upgrade() {
                    deliver(&agent, &properties);
                }
            }
            Err(error) => tracing::warn!(%error, "cannot read Update1's properties"),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(entries: &[(&str, glib::Variant)]) -> glib::Variant {
        let map: HashMap<String, glib::Variant> = entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect();
        map.to_variant()
    }

    #[test]
    fn a_locked_stage_is_an_update_to_offer() {
        let staged = staged_from(&dict(&[
            ("digest", "sha256:b".to_variant()),
            ("version", "45.20260925.118".to_variant()),
            ("download-only", true.to_variant()),
        ]));
        assert_eq!(
            staged,
            Some(Staged {
                digest: "sha256:b".to_owned(),
                version: Some("45.20260925.118".to_owned()),
            })
        );
        let without_flag = staged_from(&dict(&[("digest", "sha256:b".to_variant())]));
        assert_eq!(without_flag.map(|s| s.version), Some(None));
    }

    #[test]
    fn an_available_update_is_its_digest_and_version() {
        let found = available_from(&dict(&[
            ("digest", "sha256:d".to_variant()),
            ("version", "45.20260927.120".to_variant()),
        ]));
        assert_eq!(
            found,
            Some(Downloadable {
                digest: "sha256:d".to_owned(),
                version: Some("45.20260927.120".to_owned()),
            })
        );
        assert_eq!(available_from(&dict(&[])), None, "nothing available");
        assert_eq!(
            version_of(&dict(&[("version", "45.1".to_variant())])).as_deref(),
            Some("45.1")
        );
    }

    #[test]
    fn nothing_staged_an_applied_stage_or_a_bad_value_is_none() {
        assert_eq!(staged_from(&dict(&[])), None);
        assert_eq!(
            staged_from(&dict(&[
                ("digest", "sha256:b".to_variant()),
                ("download-only", false.to_variant()),
            ])),
            None
        );
        assert_eq!(staged_from(&dict(&[("digest", 7u32.to_variant())])), None);
        assert_eq!(staged_from(&"text".to_variant()), None);
    }
}
