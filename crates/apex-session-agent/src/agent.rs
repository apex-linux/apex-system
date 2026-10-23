use std::cell::RefCell;
use std::rc::Rc;

use gio::prelude::*;

use crate::files;
use crate::guard::{Effect, Offer};
use crate::paths::Paths;
use crate::{LAYER_KEY, RADIUS_KEY};

/// The reply of a `Notify` call: the notification id, or `None` when it failed.
pub type Posted = Box<dyn FnOnce(Option<u32>)>;

pub trait Notifier {
    fn post(&self, posted: Posted);
    fn withdraw(&self, id: u32);
}

/// Every feature reads the same settings object; the Ptyxis palette joins here once the
/// reload spike passes.
pub struct Agent {
    settings: gio::Settings,
    paths: Paths,
    offer: RefCell<Offer>,
    notifier: Box<dyn Notifier>,
    watches: RefCell<Vec<glib::SignalHandlerId>>,
    monitor: RefCell<Option<gio::FileMonitor>>,
}

impl Agent {
    pub fn start(settings: gio::Settings, paths: Paths, notifier: Box<dyn Notifier>) -> Rc<Self> {
        let agent = Rc::new(Self {
            settings,
            paths,
            offer: RefCell::new(Offer::new()),
            notifier,
            watches: RefCell::new(Vec::new()),
            monitor: RefCell::new(None),
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
        self.watches.borrow_mut().push(id);
        self.write_radius();
    }

    fn write_radius(&self) {
        let radius = self.settings.uint(RADIUS_KEY);
        match files::write_apex_css(&self.paths, radius) {
            Ok(true) => tracing::info!(radius, "wrote apex.css"),
            Ok(false) => {}
            Err(error) => tracing::warn!(error = %error_chain(&error), "cannot write apex.css"),
        }
        match files::ensure_import(&self.paths) {
            Ok(true) => tracing::info!("gtk.css now imports apex.css"),
            Ok(false) => {}
            Err(error) => tracing::warn!(error = %error_chain(&error), "cannot edit gtk.css"),
        }
    }

    /// The directory exists before the monitor, so inotify watches it from the start.
    fn start_offer(self: &Rc<Self>) {
        let state_dir = self.paths.state_dir();
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

    pub fn action_invoked(self: &Rc<Self>, id: u32, key: &str) {
        let effect = self.offer.borrow_mut().action(id, key);
        self.run(effect);
    }

    pub fn notification_closed(&self, id: u32) {
        self.offer.borrow_mut().closed(id);
    }

    pub fn offered(&self) -> Option<u32> {
        self.offer.borrow().shown()
    }

    fn run(self: &Rc<Self>, effect: Option<Effect>) {
        match effect {
            None => {}
            Some(Effect::Post) => {
                let weak = Rc::downgrade(self);
                self.notifier.post(Box::new(move |id| {
                    let Some(agent) = weak.upgrade() else { return };
                    match id {
                        Some(id) => {
                            let effect = agent.offer.borrow_mut().posted(id);
                            agent.run(effect);
                        }
                        None => agent.offer.borrow_mut().post_failed(),
                    }
                }));
            }
            Some(Effect::Withdraw(id)) => self.notifier.withdraw(id),
            Some(Effect::TurnOn) => self.turn_on(),
        }
    }

    /// The marker goes only after the setting is written, so a failed write is offered
    /// again at the next login.
    fn turn_on(&self) {
        if let Err(error) = self.settings.set_boolean(LAYER_KEY, true) {
            tracing::warn!(%error, "cannot turn the Apex desktop back on");
            return;
        }
        gio::Settings::sync();
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
        for id in self.watches.take() {
            self.settings.disconnect(id);
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
