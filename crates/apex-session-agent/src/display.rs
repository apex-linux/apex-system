use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gio::prelude::*;

use crate::notifications::{CALL_TIMEOUT_MS, Subscription};
use crate::refresh::{DisplayState, Event, Policy};

pub const MUTTER_NAME: &str = "org.gnome.Mutter.DisplayConfig";
pub const MUTTER_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
pub const MUTTER_INTERFACE: &str = "org.gnome.Mutter.DisplayConfig";
pub const UPOWER_NAME: &str = "org.freedesktop.UPower";
pub const UPOWER_PATH: &str = "/org/freedesktop/UPower";
pub const UPOWER_INTERFACE: &str = "org.freedesktop.UPower";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Lowers the built-in panel's refresh rate on battery while the Apex key allows it.
pub struct RefreshRate {
    settings: gio::Settings,
    session: gio::DBusConnection,
    policy: RefCell<Policy>,
    subscriptions: RefCell<Vec<Subscription>>,
    watch: RefCell<Option<glib::SignalHandlerId>>,
}

impl RefreshRate {
    /// Without a system bus there is no power source, and the panel is never touched.
    pub fn start(
        settings: gio::Settings,
        key: &'static str,
        session: gio::DBusConnection,
        system: Option<&gio::DBusConnection>,
    ) -> Rc<Self> {
        let this = Rc::new(Self {
            policy: RefCell::new(Policy::new(settings.boolean(key))),
            settings,
            session,
            subscriptions: RefCell::new(Vec::new()),
            watch: RefCell::new(None),
        });
        let weak = Rc::downgrade(&this);
        let id = this
            .settings
            .connect_changed(Some(key), move |settings, key| {
                if let Some(this) = weak.upgrade() {
                    this.policy.borrow_mut().set_enabled(settings.boolean(key));
                    this.refresh(Event::Transition);
                }
            });
        *this.watch.borrow_mut() = Some(id);
        let mut subscriptions = vec![this.watch_mutter()];
        if let Some(system) = system {
            subscriptions.push(this.watch_upower(system));
        }
        *this.subscriptions.borrow_mut() = subscriptions;
        this
    }

    fn watch_mutter(self: &Rc<Self>) -> Subscription {
        let on_change = Rc::downgrade(self);
        let signal = self.session.subscribe_to_signal(
            Some(MUTTER_NAME),
            Some(MUTTER_INTERFACE),
            Some("MonitorsChanged"),
            Some(MUTTER_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_| {
                if let Some(this) = on_change.upgrade() {
                    this.refresh(Event::MonitorsChanged);
                }
            },
        );
        let appeared = Rc::downgrade(self);
        let watcher = gio::bus_watch_name_on_connection(
            &self.session,
            MUTTER_NAME,
            gio::BusNameWatcherFlags::NONE,
            move |_, _, _| {
                if let Some(this) = appeared.upgrade() {
                    this.refresh(Event::Transition);
                }
            },
            |_, _| {},
        );
        Subscription::new(
            vec![signal],
            Box::new(move || gio::bus_unwatch_name(watcher)),
        )
    }

    fn watch_upower(self: &Rc<Self>, system: &gio::DBusConnection) -> Subscription {
        let on_change = Rc::downgrade(self);
        let signal = system.subscribe_to_signal(
            Some(UPOWER_NAME),
            Some(PROPERTIES),
            Some("PropertiesChanged"),
            Some(UPOWER_PATH),
            Some(UPOWER_INTERFACE),
            gio::DBusSignalFlags::NONE,
            move |signal| {
                let Some((_, changed, _)) =
                    signal
                        .parameters
                        .get::<(String, HashMap<String, glib::Variant>, Vec<String>)>()
                else {
                    return;
                };
                let on_battery = changed
                    .get("OnBattery")
                    .and_then(glib::Variant::get::<bool>);
                if let (Some(on_battery), Some(this)) = (on_battery, on_change.upgrade()) {
                    this.power(on_battery);
                }
            },
        );
        let appeared = Rc::downgrade(self);
        let watcher = gio::bus_watch_name_on_connection(
            system,
            UPOWER_NAME,
            gio::BusNameWatcherFlags::NONE,
            move |connection, _, _| read_on_battery(&connection, appeared.clone()),
            |_, _| {},
        );
        Subscription::new(
            vec![signal],
            Box::new(move || gio::bus_unwatch_name(watcher)),
        )
    }

    fn power(self: &Rc<Self>, on_battery: bool) {
        self.policy.borrow_mut().set_on_battery(on_battery);
        self.refresh(Event::Transition);
    }

    fn refresh(self: &Rc<Self>, event: Event) {
        let weak = Rc::downgrade(self);
        self.session.call(
            Some(MUTTER_NAME),
            MUTTER_PATH,
            MUTTER_INTERFACE,
            "GetCurrentState",
            None,
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            CALL_TIMEOUT_MS,
            gio::Cancellable::NONE,
            move |reply| {
                let Some(this) = weak.upgrade() else { return };
                match reply {
                    Ok(reply) => {
                        if let Some(state) = DisplayState::from_variant(&reply) {
                            this.decided(&state, event);
                        } else {
                            tracing::warn!("mutter's GetCurrentState has an unknown shape");
                        }
                    }
                    Err(error) => tracing::debug!(%error, "cannot read the display state"),
                }
            },
        );
    }

    fn decided(&self, state: &DisplayState, event: Event) {
        let Some(mode) = self.policy.borrow_mut().decide(state, event) else {
            return;
        };
        let Some(parameters) = state.apply_parameters(&mode) else {
            return;
        };
        tracing::info!(%mode, "setting the built-in panel's mode");
        self.session.call(
            Some(MUTTER_NAME),
            MUTTER_PATH,
            MUTTER_INTERFACE,
            "ApplyMonitorsConfig",
            Some(&parameters),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            CALL_TIMEOUT_MS,
            gio::Cancellable::NONE,
            move |reply| {
                if let Err(error) = reply {
                    tracing::warn!(%error, %mode, "cannot set the built-in panel's mode");
                }
            },
        );
    }
}

fn read_on_battery(connection: &gio::DBusConnection, this: Weak<RefreshRate>) {
    connection.call(
        Some(UPOWER_NAME),
        UPOWER_PATH,
        PROPERTIES,
        "Get",
        Some(&(UPOWER_INTERFACE, "OnBattery").to_variant()),
        Some(&<(glib::Variant,)>::static_variant_type()),
        gio::DBusCallFlags::NO_AUTO_START,
        CALL_TIMEOUT_MS,
        gio::Cancellable::NONE,
        move |reply| {
            let on_battery = reply
                .ok()
                .and_then(|reply| reply.get::<(glib::Variant,)>())
                .and_then(|(value,)| value.get::<bool>());
            match (on_battery, this.upgrade()) {
                (Some(on_battery), Some(this)) => this.power(on_battery),
                (None, _) => tracing::warn!("cannot read UPower's OnBattery"),
                _ => {}
            }
        },
    );
}

impl std::fmt::Debug for RefreshRate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshRate")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl Drop for RefreshRate {
    fn drop(&mut self) {
        if let Some(id) = self.watch.take() {
            self.settings.disconnect(id);
        }
    }
}
