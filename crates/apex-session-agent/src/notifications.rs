use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gio::prelude::*;

use crate::agent::{Agent, Notifier, Posted};
use crate::guard;

const BUS_NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const INTERFACE: &str = "org.freedesktop.Notifications";
const CALL_TIMEOUT_MS: i32 = 25_000;

/// `org.freedesktop.Notifications` on the session bus, which GNOME Shell serves.
#[derive(Debug)]
pub struct DbusNotifier {
    connection: gio::DBusConnection,
}

impl DbusNotifier {
    pub fn new(connection: gio::DBusConnection) -> Self {
        Self { connection }
    }

    /// Kept alive by the caller: dropping it ends the subscriptions and the name watch.
    pub fn subscribe(&self, agent: &Rc<Agent>) -> Subscription {
        let on_action = Rc::downgrade(agent);
        let on_close: Weak<Agent> = Rc::downgrade(agent);
        let subscribe = |member, callback: Box<dyn Fn(&glib::Variant)>| {
            self.connection.subscribe_to_signal(
                Some(BUS_NAME),
                Some(INTERFACE),
                Some(member),
                Some(PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |signal| callback(signal.parameters),
            )
        };
        let signals = vec![
            subscribe(
                "ActionInvoked",
                Box::new(move |parameters| {
                    let Some((id, key)) = parameters.get::<(u32, String)>() else {
                        return;
                    };
                    if let Some(agent) = on_action.upgrade() {
                        agent.action_invoked(id, &key);
                    }
                }),
            ),
            subscribe(
                "NotificationClosed",
                Box::new(move |parameters| {
                    let Some((id, _reason)) = parameters.get::<(u32, u32)>() else {
                        return;
                    };
                    if let Some(agent) = on_close.upgrade() {
                        agent.notification_closed(id);
                    }
                }),
            ),
        ];
        let appeared = Rc::downgrade(agent);
        let vanished = Rc::downgrade(agent);
        let watcher = gio::bus_watch_name_on_connection(
            &self.connection,
            BUS_NAME,
            gio::BusNameWatcherFlags::NONE,
            move |_, _, _| {
                if let Some(agent) = appeared.upgrade() {
                    agent.notification_server(true);
                }
            },
            move |_, _| {
                if let Some(agent) = vanished.upgrade() {
                    agent.notification_server(false);
                }
            },
        );
        Subscription {
            _signals: signals,
            unwatch: Some(Box::new(move || gio::bus_unwatch_name(watcher))),
        }
    }
}

/// The signal subscriptions and the watch on the notification server's name.
pub struct Subscription {
    _signals: Vec<gio::SignalSubscription>,
    unwatch: Option<Box<dyn FnOnce()>>,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription").finish_non_exhaustive()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(unwatch) = self.unwatch.take() {
            unwatch();
        }
    }
}

fn notify_parameters() -> glib::Variant {
    let hints: HashMap<String, glib::Variant> = HashMap::new();
    (
        "Apex",
        0u32,
        "",
        guard::SUMMARY,
        guard::BODY,
        vec![guard::ACTION_KEY, guard::ACTION_LABEL],
        hints,
        -1i32,
    )
        .to_variant()
}

impl Notifier for DbusNotifier {
    fn post(&self, posted: Posted) {
        self.connection.call(
            Some(BUS_NAME),
            PATH,
            INTERFACE,
            "Notify",
            Some(&notify_parameters()),
            Some(&<(u32,)>::static_variant_type()),
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            gio::Cancellable::NONE,
            move |reply| match reply {
                Ok(reply) => posted(reply.get::<(u32,)>().map(|(id,)| id)),
                Err(error) => {
                    tracing::warn!(%error, "cannot post the crash-guard offer");
                    posted(None);
                }
            },
        );
    }

    fn withdraw(&self, id: u32) {
        self.connection.call(
            Some(BUS_NAME),
            PATH,
            INTERFACE,
            "CloseNotification",
            Some(&(id,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            gio::Cancellable::NONE,
            |reply| {
                if let Err(error) = reply {
                    tracing::debug!(%error, "cannot withdraw the crash-guard offer");
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_has_the_specs_signature_and_the_action() {
        let parameters = notify_parameters();
        assert_eq!(parameters.type_().as_str(), "(susssasa{sv}i)");
        let actions = parameters.child_value(5).get::<Vec<String>>().unwrap();
        assert_eq!(
            actions,
            [guard::ACTION_KEY, "Turn the Apex desktop back on"]
        );
    }
}
