use ratatui::{Frame, layout::Rect, style::Stylize, text::Line};
use std::time::{Duration, Instant};

use super::colors::AppColors;

#[derive(Debug, Clone, PartialEq)]
struct Notification {
    message: String,
    error: bool,
    expiry: Option<Instant>,
}

impl Notification {
    /// Creates a new Notification. The `expiry` is optional, and is
    /// used to create timed notifications -- `Instant` should refer
    /// to the timestamp when the message should disappear.
    pub const fn new(message: String, error: bool, expiry: Option<Instant>) -> Self {
        Self {
            message,
            error,
            expiry,
        }
    }
}

impl Default for Notification {
    fn default() -> Self {
        Self {
            message: "".into(),
            error: false,
            expiry: None,
        }
    }
}

#[derive(Debug)]
pub struct NotificationManager {
    msg_stack: Vec<Notification>,
    persistent_msg: Option<Notification>,
    current_msg: Option<Notification>,
}

impl NotificationManager {
    pub const fn new() -> Self {
        Self {
            msg_stack: Vec::new(),
            persistent_msg: None,
            current_msg: None,
        }
    }

    /// Checks if the current notification needs to be changed, and
    /// updates the message window accordingly.
    pub fn check_notifs(&mut self) {
        if !self.msg_stack.is_empty() {
            // compare expiry times of all notifications to current
            // time, remove expired ones
            let now = Instant::now();
            self.msg_stack
                .retain(|x| x.expiry.is_none_or(|exp| now < exp));

            if !self.msg_stack.is_empty() {
                // check if last item changed, and update screen if it has
                let last_item = &self.msg_stack[self.msg_stack.len() - 1];
                self.current_msg = Some(last_item.clone());
            } else if let Some(msg) = &self.persistent_msg {
                // if no other timed notifications exist, display a
                // persistent notification if there is one
                self.current_msg = Some(msg.clone());
            } else {
                // otherwise, there was a notification before but there
                // isn't now, so erase
                self.current_msg = None;
            }
        }
    }

    /// Adds a notification to the user. `duration` indicates how long
    /// (in milliseconds) this message will remain on screen. Useful for
    /// presenting error messages, among other things.
    pub fn timed_notif(&mut self, message: String, duration: u64, error: bool) {
        let expiry = Instant::now() + Duration::from_millis(duration);
        self.msg_stack
            .push(Notification::new(message, error, Some(expiry)));
    }

    /// Adds a notification that will stay on screen indefinitely. Must
    /// use `clear_persistent_notif()` to erase. If a persistent
    /// notification is already being displayed, this method will
    /// overwrite that message.
    pub fn persistent_notif(&mut self, message: String, error: bool) {
        log::debug!("{message}");
        let notif = Notification::new(message, error, None);
        self.persistent_msg = Some(notif.clone());
        if self.msg_stack.is_empty() {
            self.current_msg = Some(notif);
        }
    }

    /// Clears any persistent notification that is being displayed. Does
    /// not affect timed notifications, user input notifications, etc.
    pub fn clear_persistent_notif(&mut self) {
        self.persistent_msg = None;
        if self.msg_stack.is_empty() {
            self.current_msg = None;
        }
    }
}

pub fn render_notification_line(
    frame: &mut Frame, area: Rect, notification: &NotificationManager, colors: &AppColors,
) {
    let cur_notif = if notification.persistent_msg.is_some() {
        &notification.persistent_msg
    } else {
        &notification.current_msg
    };
    let line = cur_notif.as_ref().map_or_else(
        || Line::from(" ").style(colors.normal),
        |notif| {
            if notif.error {
                Line::from(notif.message.clone()).style(colors.error).bold()
            } else {
                Line::from(notif.message.clone()).style(colors.normal)
            }
        },
    );
    frame.render_widget(line, area);
}
