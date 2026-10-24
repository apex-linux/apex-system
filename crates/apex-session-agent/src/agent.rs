use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use gio::prelude::*;

use crate::failure::{self, Failure};
use crate::files;
use crate::guard::{self, Effect, Offer};
use crate::paths::Paths;
use crate::updates::{self, Digested, Downloadable, Ready, Staged};
use crate::{ACCENT_KEY, LAYER_KEY, RADIUS_KEY};

/// The reply of a `Notify` call: the notification id, or `None` when it failed.
pub type Posted = Box<dyn FnOnce(Option<u32>)>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The application line the notification shows.
    pub app: &'static str,
    pub icon: &'static str,
    pub summary: String,
    pub body: String,
    /// Pairs of action key and label.
    pub actions: Vec<(&'static str, &'static str)>,
}

pub trait Notifier {
    fn post(&self, notification: &Notification, posted: Posted);
    fn withdraw(&self, id: u32);
    /// `page` is the panel's argument, as in `gnome-control-center PANEL PAGE`.
    fn open_settings(&self, panel: &str, page: Option<&str>);
}

/// One once-per-digest notification: where its machine lives, what it says, where its
/// digest is recorded, and what its action opens.
struct Once<T> {
    machine: fn(&Agent) -> &RefCell<Ready<T>>,
    notification: fn(&T) -> Notification,
    file: fn(&Paths) -> PathBuf,
    panel: &'static str,
    page: Option<&'static str>,
    what: &'static str,
}

const UPDATES_READY: Once<Staged> = Once {
    machine: |agent| &agent.ready,
    notification: |staged| Notification {
        app: updates::APP_NAME,
        icon: updates::ICON,
        summary: updates::SUMMARY.to_owned(),
        body: staged.body(),
        actions: vec![
            (updates::DEFAULT_ACTION_KEY, updates::ACTION_LABEL),
            (updates::ACTION_KEY, updates::ACTION_LABEL),
        ],
    },
    file: Paths::updates_notified,
    panel: updates::SETTINGS_PANEL,
    page: None,
    what: "cannot record the notified update",
};

const UPDATE_AVAILABLE: Once<Downloadable> = Once {
    machine: |agent| &agent.available,
    notification: |update| Notification {
        app: updates::APP_NAME,
        icon: updates::ICON,
        summary: updates::AVAILABLE_SUMMARY.to_owned(),
        body: update.body(),
        actions: vec![
            (updates::DEFAULT_ACTION_KEY, failure::ACTION_LABEL),
            (updates::ACTION_KEY, failure::ACTION_LABEL),
        ],
    },
    file: Paths::available_notified,
    panel: failure::SETTINGS_PANEL,
    page: Some(updates::UPDATES_PAGE),
    what: "cannot record the notified available update",
};

const UPDATE_FAILED: Once<Failure> = Once {
    machine: |agent| &agent.failure,
    notification: Failure::notification,
    file: Paths::failure_notified,
    panel: failure::SETTINGS_PANEL,
    page: Some(failure::SETTINGS_PAGE),
    what: "cannot record the notified update failure",
};

/// What the agent last read from Update1, for the notices built from several properties.
#[derive(Debug, Default)]
struct Update1View {
    health_failure: String,
    last_fallback: String,
    booted_version: Option<String>,
    /// `None` until read; a daemon without the property never turns the notice on.
    automatic_downloads: Option<bool>,
    available: Option<Downloadable>,
}

/// The Apex schema drives every feature but the palette, which follows the stock accent.
pub struct Agent {
    settings: gio::Settings,
    interface: Option<gio::Settings>,
    paths: Paths,
    offer: RefCell<Offer>,
    ready: RefCell<Ready<Staged>>,
    failure: RefCell<Ready<Failure>>,
    available: RefCell<Ready<Downloadable>>,
    view: RefCell<Update1View>,
    notifier: Box<dyn Notifier>,
    watches: RefCell<Vec<(gio::Settings, glib::SignalHandlerId)>>,
    monitor: RefCell<Option<gio::FileMonitor>>,
    warned: RefCell<HashSet<String>>,
}

impl Agent {
    pub fn start(
        settings: gio::Settings,
        interface: Option<gio::Settings>,
        paths: Paths,
        notifier: Box<dyn Notifier>,
    ) -> Rc<Self> {
        let recorded = files::read_notified(&paths, &paths.updates_notified());
        let recorded_failure = files::read_notified(&paths, &paths.failure_notified());
        let recorded_available = files::read_notified(&paths, &paths.available_notified());
        let agent = Rc::new(Self {
            settings,
            interface,
            paths,
            offer: RefCell::new(Offer::new()),
            ready: RefCell::new(Ready::new(recorded)),
            failure: RefCell::new(Ready::new(recorded_failure)),
            available: RefCell::new(Ready::new(recorded_available)),
            view: RefCell::new(Update1View::default()),
            notifier,
            watches: RefCell::new(Vec::new()),
            monitor: RefCell::new(None),
            warned: RefCell::new(HashSet::new()),
        });
        let schema = agent.settings.settings_schema();
        let has = |key: &str| schema.as_ref().is_some_and(|s| s.has_key(key));
        if has(RADIUS_KEY) {
            agent.start_radius();
        } else {
            tracing::warn!("the Apex schema has no {RADIUS_KEY}: the window radius stays GTK's");
        }
        if has(LAYER_KEY) {
            agent.start_offer();
        } else {
            tracing::warn!("the Apex schema has no {LAYER_KEY}: the crash-guard offer is off");
        }
        let has_accent = agent
            .interface
            .as_ref()
            .and_then(gio::Settings::settings_schema)
            .is_some_and(|s| s.has_key(ACCENT_KEY));
        if has_accent {
            agent.start_palette();
        } else {
            tracing::warn!(
                "the interface schema has no {ACCENT_KEY}: the terminal caret stays stock"
            );
        }
        agent
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    fn start_radius(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let id = self
            .settings
            .connect_changed(Some(RADIUS_KEY), move |_, _| {
                if let Some(agent) = weak.upgrade() {
                    agent.write_radius();
                }
            });
        self.watches.borrow_mut().push((self.settings.clone(), id));
        self.write_radius();
    }

    fn start_palette(self: &Rc<Self>) {
        let Some(interface) = &self.interface else {
            return;
        };
        let weak = Rc::downgrade(self);
        let id = interface.connect_changed(Some(ACCENT_KEY), move |_, _| {
            if let Some(agent) = weak.upgrade() {
                agent.write_palette();
            }
        });
        self.watches.borrow_mut().push((interface.clone(), id));
        self.write_palette();
    }

    fn write_palette(&self) {
        let Some(interface) = &self.interface else {
            return;
        };
        let accent = interface.string(ACCENT_KEY);
        match files::write_ptyxis_palette(&self.paths, &accent) {
            Ok(true) => tracing::info!(%accent, "wrote apex.palette"),
            Ok(false) => {}
            Err(error) => self.warn_once("cannot write apex.palette", &error),
        }
    }

    /// A refusal repeats with every radius change; the journal needs it once.
    fn warn_once(&self, what: &str, error: &files::FileError) {
        let text = format!("{what}: {}", error_chain(error));
        if self.warned.borrow_mut().insert(text.clone()) {
            tracing::warn!("{text}");
        }
    }

    fn write_radius(&self) {
        let radius = self.settings.uint(RADIUS_KEY);
        match files::write_apex_css(&self.paths, radius) {
            Ok(true) => tracing::info!(radius, "wrote apex.css"),
            Ok(false) => {}
            Err(error) => self.warn_once("cannot write apex.css", &error),
        }
        match files::ensure_import(&self.paths) {
            Ok(true) => tracing::info!("gtk.css now imports apex.css"),
            Ok(false) => {}
            Err(error) => self.warn_once("cannot edit gtk.css", &error),
        }
    }

    /// The directory exists before the monitor, so inotify watches it from the start.
    fn start_offer(self: &Rc<Self>) {
        let state_dir = self.paths.marker_dir();
        if let Err(error) = std::fs::create_dir_all(&state_dir) {
            tracing::warn!(%error, dir = %state_dir.display(), "cannot create the state directory");
        }
        let marker = gio::File::for_path(self.paths.guard_marker());
        match marker.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) {
            Ok(monitor) => {
                let weak = Rc::downgrade(self);
                monitor.connect_changed(move |_, _, _, event| {
                    if let Some(agent) = weak.upgrade() {
                        if event == gio::FileMonitorEvent::Created {
                            agent.marker_created();
                        } else {
                            agent.marker_changed();
                        }
                    }
                });
                *self.monitor.borrow_mut() = Some(monitor);
            }
            Err(error) => tracing::warn!(%error, "cannot watch the crash-guard marker"),
        }
        self.marker_changed();
    }

    pub fn marker_changed(self: &Rc<Self>) {
        let present = self.paths.guard_marker().exists();
        let effect = self.offer.borrow_mut().marker(present);
        self.run(effect);
    }

    pub fn marker_created(self: &Rc<Self>) {
        let effect = self.offer.borrow_mut().tripped();
        self.run(effect);
    }

    pub fn notification_server(self: &Rc<Self>, present: bool) {
        let effect = self.offer.borrow_mut().server(present);
        self.run(effect);
        let effect = self.ready.borrow_mut().server(present);
        self.run_once(&UPDATES_READY, effect);
        let effect = self.failure.borrow_mut().server(present);
        self.run_once(&UPDATE_FAILED, effect);
        let effect = self.available.borrow_mut().server(present);
        self.run_once(&UPDATE_AVAILABLE, effect);
    }

    pub fn action_invoked(self: &Rc<Self>, id: u32, key: &str) {
        let effect = self.offer.borrow_mut().action(id, key);
        self.run(effect);
        let effect = self.ready.borrow_mut().action(id, key);
        self.run_once(&UPDATES_READY, effect);
        let effect = self.failure.borrow_mut().action(id, key);
        self.run_once(&UPDATE_FAILED, effect);
        let effect = self.available.borrow_mut().action(id, key);
        self.run_once(&UPDATE_AVAILABLE, effect);
    }

    pub fn notification_closed(&self, id: u32) {
        self.offer.borrow_mut().closed(id);
        self.ready.borrow_mut().closed(id);
        self.failure.borrow_mut().closed(id);
        self.available.borrow_mut().closed(id);
    }

    pub fn offered(&self) -> Option<u32> {
        self.offer.borrow().shown()
    }

    /// What Update1 reports as staged and not applied yet.
    pub fn staged(self: &Rc<Self>, staged: Option<Staged>) {
        let effects = self.ready.borrow_mut().update(staged);
        self.run_once(&UPDATES_READY, effects);
    }

    pub fn updates_ready(&self) -> Option<u32> {
        self.ready.borrow().shown()
    }

    /// Update1's `HealthFailure`, `""` when there is none.
    pub fn health_failure(self: &Rc<Self>, digest: &str) {
        digest.clone_into(&mut self.view.borrow_mut().health_failure);
        self.failure_changed();
    }

    /// Update1's `LastFallback`, `""` when there is none.
    pub fn last_fallback(self: &Rc<Self>, digest: &str) {
        digest.clone_into(&mut self.view.borrow_mut().last_fallback);
        self.failure_changed();
    }

    /// The version in Update1's `Booted`, which the health-failure text names.
    pub fn booted_version(self: &Rc<Self>, version: Option<String>) {
        self.view.borrow_mut().booted_version = version;
        self.failure_changed();
    }

    pub fn automatic_downloads(self: &Rc<Self>, enabled: bool) {
        self.view.borrow_mut().automatic_downloads = Some(enabled);
        self.available_changed();
    }

    /// What Update1 reports as `Available`: found by a check and not staged yet.
    pub fn available(self: &Rc<Self>, update: Option<Downloadable>) {
        self.view.borrow_mut().available = update;
        self.available_changed();
    }

    fn failure_changed(self: &Rc<Self>) {
        let current = {
            let view = self.view.borrow();
            Failure::from_properties(
                &view.health_failure,
                &view.last_fallback,
                view.booted_version.as_deref(),
            )
        };
        let effects = self.failure.borrow_mut().update(current);
        self.run_once(&UPDATE_FAILED, effects);
    }

    /// Only while automatic downloads are off: otherwise Updates Ready follows the stage.
    fn available_changed(self: &Rc<Self>) {
        let current = {
            let view = self.view.borrow();
            view.available
                .clone()
                .filter(|_| view.automatic_downloads == Some(false))
        };
        let effects = self.available.borrow_mut().update(current);
        self.run_once(&UPDATE_AVAILABLE, effects);
    }

    pub fn update_available(&self) -> Option<u32> {
        self.available.borrow().shown()
    }

    pub fn update_failed(&self) -> Option<u32> {
        self.failure.borrow().shown()
    }

    fn run_once<T: Digested + 'static>(
        self: &Rc<Self>,
        once: &'static Once<T>,
        effects: impl IntoIterator<Item = updates::Effect<T>>,
    ) {
        for effect in effects {
            match effect {
                updates::Effect::Post(subject) => {
                    let weak = Rc::downgrade(self);
                    self.notifier.post(
                        &(once.notification)(&subject),
                        Box::new(move |id| {
                            let Some(agent) = weak.upgrade() else { return };
                            match id {
                                Some(id) => {
                                    let effects = (once.machine)(&agent).borrow_mut().posted(id);
                                    agent.run_once(once, effects);
                                }
                                None => (once.machine)(&agent).borrow_mut().post_failed(),
                            }
                        }),
                    );
                }
                updates::Effect::Withdraw(id) => self.notifier.withdraw(id),
                updates::Effect::Record(digest) => {
                    let path = (once.file)(&self.paths);
                    if let Err(error) = files::write_notified(&self.paths, &path, &digest) {
                        self.warn_once(once.what, &error);
                    }
                }
                updates::Effect::OpenSettings => self.notifier.open_settings(once.panel, once.page),
            }
        }
    }

    fn run(self: &Rc<Self>, effect: Option<Effect>) {
        match effect {
            None => {}
            Some(Effect::Post) => {
                let weak = Rc::downgrade(self);
                self.notifier.post(
                    &guard::notification(),
                    Box::new(move |id| {
                        let Some(agent) = weak.upgrade() else { return };
                        match id {
                            Some(id) => {
                                let effect = agent.offer.borrow_mut().posted(id);
                                agent.run(effect);
                            }
                            None => agent.offer.borrow_mut().post_failed(),
                        }
                    }),
                );
            }
            Some(Effect::Withdraw(id)) => self.notifier.withdraw(id),
            Some(Effect::TurnOn) => self.turn_on(),
        }
    }

    /// The marker goes only once the setting reads back true: dconf can accept a write and
    /// lose it, and then the offer must come back at the next login.
    fn turn_on(&self) {
        if let Err(error) = self.settings.set_boolean(LAYER_KEY, true) {
            tracing::warn!(%error, "cannot turn the Apex desktop back on");
            return;
        }
        gio::Settings::sync();
        if !self.settings.boolean(LAYER_KEY) {
            tracing::warn!("{LAYER_KEY} did not stay true; the offer comes back at the next login");
            return;
        }
        tracing::info!("the Apex desktop is on again from the next login");
        if let Err(error) = files::remove_marker(&self.paths) {
            tracing::warn!(error = %error_chain(&error), "cannot remove the crash-guard marker");
        }
    }
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("paths", &self.paths)
            .field("offer", &self.offer)
            .finish_non_exhaustive()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        for (settings, id) in self.watches.take() {
            settings.disconnect(id);
        }
        if let Some(monitor) = self.monitor.take() {
            monitor.cancel();
        }
    }
}

pub fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
