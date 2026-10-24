use crate::agent;

pub const ACTION_KEY: &str = "turn-on";

pub const SUMMARY: &str = "The Apex desktop is off";

pub const BODY: &str = "GNOME Shell failed to start with the Apex desktop, so this session runs \
the stock desktop. Turning it back on takes effect at the next login.";

pub const ACTION_LABEL: &str = "Turn the Apex desktop back on";

pub fn notification() -> agent::Notification {
    agent::Notification {
        icon: "",
        summary: SUMMARY.to_owned(),
        body: BODY.to_owned(),
        actions: vec![(ACTION_KEY, ACTION_LABEL)],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Post,
    Withdraw(u32),
    /// Set `layer-enabled` to true, then remove the marker.
    TurnOn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Notification {
    #[default]
    None,
    Posting,
    Shown(u32),
    /// Kept so the same trip is not offered twice in one session; the marker stays.
    Dismissed,
}

/// The offer for one marker, as a pure state machine: the caller does the effects and
/// reports what came of them.
#[derive(Debug, Default)]
pub struct Offer {
    marker: bool,
    notification: Notification,
    /// Only a watcher that saw the name vanish sets this; without one the agent tries.
    server_gone: bool,
}

impl Offer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn marker(&mut self, present: bool) -> Option<Effect> {
        self.marker = present;
        match (present, self.notification) {
            (true, Notification::None) if !self.server_gone => {
                self.notification = Notification::Posting;
                Some(Effect::Post)
            }
            (false, Notification::Shown(id)) => {
                self.notification = Notification::None;
                Some(Effect::Withdraw(id))
            }
            (false, Notification::Dismissed) => {
                self.notification = Notification::None;
                None
            }
            _ => None,
        }
    }

    /// The notification server came onto the bus or left it. A shell restart takes its
    /// notifications along, so a shown or pending offer is posted again on its return.
    pub fn server(&mut self, present: bool) -> Option<Effect> {
        self.server_gone = !present;
        if !present {
            if matches!(
                self.notification,
                Notification::Posting | Notification::Shown(_)
            ) {
                self.notification = Notification::None;
            }
            return None;
        }
        self.marker(self.marker)
    }

    /// The marker was created again: a new trip, offered even after a dismissal.
    pub fn tripped(&mut self) -> Option<Effect> {
        if self.notification == Notification::Dismissed {
            self.notification = Notification::None;
        }
        self.marker(true)
    }

    pub fn posted(&mut self, id: u32) -> Option<Effect> {
        if self.notification != Notification::Posting {
            return Some(Effect::Withdraw(id));
        }
        if self.marker {
            self.notification = Notification::Shown(id);
            None
        } else {
            self.notification = Notification::None;
            Some(Effect::Withdraw(id))
        }
    }

    pub fn post_failed(&mut self) {
        if self.notification == Notification::Posting {
            self.notification = Notification::None;
        }
    }

    pub fn action(&mut self, id: u32, key: &str) -> Option<Effect> {
        if self.notification != Notification::Shown(id) || key != ACTION_KEY {
            return None;
        }
        self.notification = Notification::None;
        Some(Effect::TurnOn)
    }

    pub fn closed(&mut self, id: u32) {
        if self.notification == Notification::Shown(id) {
            self.notification = Notification::Dismissed;
        }
    }

    pub fn shown(&self) -> Option<u32> {
        match self.notification {
            Notification::Shown(id) => Some(id),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(id: u32) -> Offer {
        let mut offer = Offer::new();
        assert_eq!(offer.marker(true), Some(Effect::Post));
        assert_eq!(offer.posted(id), None);
        offer
    }

    #[test]
    fn no_marker_no_offer() {
        let mut offer = Offer::new();
        assert_eq!(offer.marker(false), None);
        assert_eq!(offer.shown(), None);
    }

    #[test]
    fn a_marker_is_offered_once() {
        let mut offer = shown(7);
        assert_eq!(offer.shown(), Some(7));
        assert_eq!(
            offer.marker(true),
            None,
            "a second event for the same marker"
        );
    }

    #[test]
    fn the_action_turns_the_layer_on() {
        let mut offer = shown(7);
        assert_eq!(offer.action(8, ACTION_KEY), None, "another notification");
        assert_eq!(offer.action(7, "default"), None);
        assert_eq!(offer.action(7, ACTION_KEY), Some(Effect::TurnOn));
        offer.closed(7);
        assert_eq!(
            offer.marker(false),
            None,
            "the removed marker withdraws nothing"
        );
        assert_eq!(
            offer.marker(true),
            Some(Effect::Post),
            "a new trip is offered"
        );
    }

    #[test]
    fn dismissing_keeps_the_marker_and_does_not_offer_again() {
        let mut offer = shown(7);
        offer.closed(7);
        assert_eq!(offer.shown(), None);
        assert_eq!(offer.marker(true), None);
        assert_eq!(offer.action(7, ACTION_KEY), None);
        assert_eq!(offer.marker(false), None);
        assert_eq!(offer.marker(true), Some(Effect::Post));
    }

    #[test]
    fn a_marker_created_again_is_a_new_trip() {
        let mut offer = shown(7);
        assert_eq!(offer.tripped(), None, "still shown");
        offer.closed(7);
        assert_eq!(offer.tripped(), Some(Effect::Post));
    }

    #[test]
    fn a_marker_removed_elsewhere_withdraws_the_offer() {
        let mut offer = shown(7);
        assert_eq!(offer.marker(false), Some(Effect::Withdraw(7)));
        offer.closed(7);
        assert_eq!(offer.marker(true), Some(Effect::Post));
    }

    #[test]
    fn a_marker_removed_while_posting_withdraws_on_the_reply() {
        let mut offer = Offer::new();
        assert_eq!(offer.marker(true), Some(Effect::Post));
        assert_eq!(offer.marker(false), None);
        assert_eq!(offer.posted(3), Some(Effect::Withdraw(3)));
        assert_eq!(offer.shown(), None);
    }

    #[test]
    fn a_failed_post_is_retried_on_the_next_marker_event() {
        let mut offer = Offer::new();
        assert_eq!(offer.marker(true), Some(Effect::Post));
        offer.post_failed();
        assert_eq!(offer.marker(true), Some(Effect::Post));
    }

    #[test]
    fn an_offer_waits_for_the_notification_server() {
        let mut offer = Offer::new();
        assert_eq!(offer.server(false), None);
        assert_eq!(offer.marker(true), None, "nobody to show it");
        assert_eq!(offer.server(true), Some(Effect::Post));
        assert_eq!(offer.posted(5), None);
        assert_eq!(offer.shown(), Some(5));
    }

    #[test]
    fn a_post_that_failed_is_retried_when_the_server_appears() {
        let mut offer = Offer::new();
        assert_eq!(offer.marker(true), Some(Effect::Post));
        offer.post_failed();
        assert_eq!(offer.server(true), Some(Effect::Post));
    }

    #[test]
    fn a_shell_restart_posts_the_offer_again() {
        let mut offer = shown(7);
        assert_eq!(offer.server(false), None);
        assert_eq!(offer.shown(), None, "the old id is gone with the server");
        assert_eq!(offer.action(7, ACTION_KEY), None);
        assert_eq!(offer.server(true), Some(Effect::Post));
        assert_eq!(offer.posted(8), None);
        assert_eq!(offer.shown(), Some(8));
    }

    #[test]
    fn a_dismissed_offer_stays_dismissed_across_a_shell_restart() {
        let mut offer = shown(7);
        offer.closed(7);
        assert_eq!(offer.server(false), None);
        assert_eq!(offer.server(true), None);
    }

    #[test]
    fn without_a_marker_the_server_changes_nothing() {
        let mut offer = Offer::new();
        assert_eq!(offer.server(false), None);
        assert_eq!(offer.server(true), None);
    }

    #[test]
    fn a_stray_reply_is_withdrawn() {
        let mut offer = shown(7);
        assert_eq!(offer.posted(9), Some(Effect::Withdraw(9)));
        assert_eq!(offer.shown(), Some(7));
    }
}
