pub const ACTION_KEY: &str = "open-settings";

/// GNOME Shell sends it when the notification itself is clicked.
pub const DEFAULT_ACTION_KEY: &str = "default";

pub const ACTION_LABEL: &str = "Open Settings";

pub const SUMMARY: &str = "Updates Ready";

/// The application line of every update notification.
pub const APP_NAME: &str = "Apex Updates";

pub const AVAILABLE_SUMMARY: &str = "Update Available";

/// Software Updates, where an update found but not downloaded can be downloaded.
pub const UPDATES_PAGE: &str = "software-updates";

pub const ICON: &str = "software-update-available-symbolic";

/// The Settings panel the action opens until the Software Updates page exists (S2d).
pub const SETTINGS_PANEL: &str = "system";

/// What a once-per-digest notification is about.
pub trait Digested: Clone {
    fn digest(&self) -> &str;
}

/// A staged update the user has not applied yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub digest: String,
    pub version: Option<String>,
}

/// An update the daily check found while automatic downloads are off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloadable {
    pub digest: String,
    pub version: Option<String>,
}

impl Digested for Downloadable {
    fn digest(&self) -> &str {
        &self.digest
    }
}

impl Downloadable {
    pub fn body(&self) -> String {
        match &self.version {
            Some(version) => format!("Apex {version} is ready to download."),
            None => "An Apex update is ready to download.".to_owned(),
        }
    }
}

impl Digested for Staged {
    fn digest(&self) -> &str {
        &self.digest
    }
}

impl Staged {
    pub fn body(&self) -> String {
        match &self.version {
            Some(version) => format!("Apex {version} is downloaded and ready to install."),
            None => "An Apex update is downloaded and ready to install.".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect<T> {
    Post(T),
    Withdraw(u32),
    /// Persist the digest, so the agent never posts it again, even after a restart.
    Record(String),
    OpenSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Notification {
    #[default]
    None,
    Posting(String),
    Shown(u32, String),
}

/// A notification posted once per digest, as a pure state machine: "Updates Ready" for a
/// staged update, or the health-failure notice.
#[derive(Debug)]
pub struct Ready<T> {
    current: Option<T>,
    notified: Option<String>,
    notification: Notification,
    server_gone: bool,
}

impl<T: Digested> Ready<T> {
    pub fn new(notified: Option<String>) -> Self {
        Self {
            current: None,
            notified,
            notification: Notification::None,
            server_gone: false,
        }
    }

    fn staged_digest(&self) -> Option<&str> {
        self.current.as_ref().map(Digested::digest)
    }

    fn next(&mut self) -> Option<Effect<T>> {
        let current = self.current.as_ref()?;
        if self.notification != Notification::None
            || self.server_gone
            || self.notified.as_deref() == Some(current.digest())
        {
            return None;
        }
        self.notification = Notification::Posting(current.digest().to_owned());
        Some(Effect::Post(current.clone()))
    }

    /// What Update1 reports now; `None` once it is gone (applied, discarded, acknowledged).
    pub fn update(&mut self, current: Option<T>) -> Vec<Effect<T>> {
        self.current = current;
        let mut effects = Vec::new();
        if let Notification::Shown(id, digest) = &self.notification
            && self.staged_digest() != Some(digest.as_str())
        {
            effects.push(Effect::Withdraw(*id));
            self.notification = Notification::None;
        }
        effects.extend(self.next());
        effects
    }

    /// A shell restart takes the notification along; its digest stays recorded.
    pub fn server(&mut self, present: bool) -> Option<Effect<T>> {
        self.server_gone = !present;
        if !present {
            self.notification = Notification::None;
            return None;
        }
        self.next()
    }

    pub fn posted(&mut self, id: u32) -> Vec<Effect<T>> {
        let Notification::Posting(digest) = &self.notification else {
            return vec![Effect::Withdraw(id)];
        };
        if self.staged_digest() == Some(digest.as_str()) {
            let digest = digest.clone();
            self.notified = Some(digest.clone());
            self.notification = Notification::Shown(id, digest.clone());
            return vec![Effect::Record(digest)];
        }
        self.notification = Notification::None;
        let mut effects = vec![Effect::Withdraw(id)];
        effects.extend(self.next());
        effects
    }

    pub fn post_failed(&mut self) {
        if matches!(self.notification, Notification::Posting(_)) {
            self.notification = Notification::None;
        }
    }

    pub fn action(&mut self, id: u32, key: &str) -> Option<Effect<T>> {
        let Notification::Shown(shown, _) = self.notification else {
            return None;
        };
        if shown != id || (key != ACTION_KEY && key != DEFAULT_ACTION_KEY) {
            return None;
        }
        self.notification = Notification::None;
        Some(Effect::OpenSettings)
    }

    pub fn closed(&mut self, id: u32) {
        if matches!(self.notification, Notification::Shown(shown, _) if shown == id) {
            self.notification = Notification::None;
        }
    }

    pub fn shown(&self) -> Option<u32> {
        match self.notification {
            Notification::Shown(id, _) => Some(id),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(digest: &str) -> Staged {
        Staged {
            digest: digest.to_owned(),
            version: Some("45.20260925.118".to_owned()),
        }
    }

    fn shown(id: u32) -> Ready<Staged> {
        let mut ready = Ready::<Staged>::new(None);
        assert_eq!(ready.update(Some(staged("b"))), [Effect::Post(staged("b"))]);
        assert_eq!(ready.posted(id), [Effect::Record("b".to_owned())]);
        ready
    }

    #[test]
    fn nothing_staged_posts_nothing() {
        let mut ready = Ready::<Staged>::new(None);
        assert_eq!(ready.update(None), []);
        assert_eq!(ready.server(true), None);
    }

    #[test]
    fn a_staged_digest_is_posted_once() {
        let mut ready = shown(7);
        assert_eq!(ready.shown(), Some(7));
        assert_eq!(ready.update(Some(staged("b"))), [], "the same digest again");
        ready.closed(7);
        assert_eq!(ready.update(Some(staged("b"))), [], "dismissed");
        assert_eq!(ready.server(false), None);
        assert_eq!(ready.server(true), None, "a shell restart");
    }

    #[test]
    fn a_recorded_digest_is_not_posted_after_a_restart() {
        let mut ready = Ready::<Staged>::new(Some("b".to_owned()));
        assert_eq!(ready.update(Some(staged("b"))), []);
        assert_eq!(ready.update(Some(staged("c"))), [Effect::Post(staged("c"))]);
    }

    #[test]
    fn a_newer_digest_replaces_the_shown_notification() {
        let mut ready = shown(7);
        assert_eq!(
            ready.update(Some(staged("c"))),
            [Effect::Withdraw(7), Effect::Post(staged("c"))]
        );
        assert_eq!(ready.posted(8), [Effect::Record("c".to_owned())]);
        assert_eq!(ready.shown(), Some(8));
    }

    #[test]
    fn an_applied_or_discarded_update_withdraws_the_notification() {
        let mut ready = shown(7);
        assert_eq!(ready.update(None), [Effect::Withdraw(7)]);
        assert_eq!(ready.shown(), None);
        assert_eq!(ready.update(Some(staged("b"))), [], "still recorded");
    }

    #[test]
    fn the_action_opens_settings() {
        let mut ready = shown(7);
        assert_eq!(ready.action(8, ACTION_KEY), None, "another notification");
        assert_eq!(ready.action(7, "turn-on"), None);
        assert_eq!(ready.action(7, ACTION_KEY), Some(Effect::OpenSettings));
        assert_eq!(ready.action(7, ACTION_KEY), None);
        let mut ready = shown(7);
        assert_eq!(
            ready.action(7, DEFAULT_ACTION_KEY),
            Some(Effect::OpenSettings)
        );
    }

    #[test]
    fn the_post_waits_for_the_notification_server() {
        let mut ready = Ready::<Staged>::new(None);
        assert_eq!(ready.server(false), None);
        assert_eq!(ready.update(Some(staged("b"))), [], "nobody to show it");
        assert_eq!(ready.server(true), Some(Effect::Post(staged("b"))));
    }

    #[test]
    fn a_failed_post_is_retried_and_not_recorded() {
        let mut ready = Ready::<Staged>::new(None);
        assert_eq!(ready.update(Some(staged("b"))).len(), 1);
        ready.post_failed();
        assert_eq!(ready.server(true), Some(Effect::Post(staged("b"))));
    }

    #[test]
    fn a_reply_for_a_digest_no_longer_staged_is_withdrawn() {
        let mut ready = Ready::<Staged>::new(None);
        assert_eq!(ready.update(Some(staged("b"))).len(), 1);
        assert_eq!(ready.update(Some(staged("c"))), [], "still posting b");
        assert_eq!(
            ready.posted(7),
            [Effect::Withdraw(7), Effect::Post(staged("c"))]
        );
        assert_eq!(ready.posted(8), [Effect::Record("c".to_owned())]);
    }

    #[test]
    fn a_stray_reply_is_withdrawn() {
        let mut ready = shown(7);
        assert_eq!(ready.posted(9), [Effect::Withdraw(9)]);
        assert_eq!(ready.shown(), Some(7));
    }

    #[test]
    fn the_body_names_the_version() {
        assert_eq!(
            staged("b").body(),
            "Apex 45.20260925.118 is downloaded and ready to install."
        );
        let unknown = Staged {
            digest: "b".to_owned(),
            version: None,
        };
        assert_eq!(
            unknown.body(),
            "An Apex update is downloaded and ready to install."
        );
    }
}
