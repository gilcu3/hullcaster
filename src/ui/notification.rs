use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Stylize,
    text::Line,
};
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use crate::types::SyncProgress;

use super::colors::AppColors;

#[derive(Debug, Clone, PartialEq, Default, derive_more::Constructor)]
struct Notification {
    message: String,
    error: bool,
}

#[derive(Debug, Clone, PartialEq, Default, derive_more::Constructor)]
struct PendingNotification {
    message: String,
    error: bool,
    duration: Duration,
}

#[derive(Debug)]
pub struct NotificationManager {
    msg_stack: VecDeque<PendingNotification>,
    persistent_msg: Option<Notification>,
    current_msg: Option<(Notification, Instant)>,
}

impl From<PendingNotification> for Notification {
    fn from(value: PendingNotification) -> Self {
        Self::new(value.message, value.error)
    }
}

impl NotificationManager {
    pub const fn new() -> Self {
        Self {
            msg_stack: VecDeque::new(),
            persistent_msg: None,
            current_msg: None,
        }
    }

    /// Checks if the current notification needs to be changed, and
    /// updates the message window accordingly.
    pub fn check_notifs(&mut self) {
        if let Some((_current, expiry)) = &self.current_msg {
            if Instant::now() > *expiry {
                self.current_msg = None;
            }
        } else if let Some(next_item) = self.msg_stack.pop_front() {
            let expiry = Instant::now() + next_item.duration;
            self.current_msg = Some((next_item.into(), expiry));
        }
    }

    /// Adds a notification to the user. `duration` indicates how long
    /// (in milliseconds) this message will remain on screen. Useful for
    /// presenting error messages, among other things.
    pub fn timed_notif(&mut self, message: String, duration: u64, error: bool) {
        let duration = Duration::from_millis(duration);
        self.msg_stack
            .push_back(PendingNotification::new(message, error, duration));
    }

    /// Adds a notification that will stay on screen indefinitely. Must
    /// use `clear_persistent_notif()` to erase. If a persistent
    /// notification is already being displayed, this method will
    /// overwrite that message.
    pub fn persistent_notif(&mut self, message: String, error: bool) {
        self.persistent_msg = Some(Notification::new(message, error));
    }

    /// Clears any persistent notification that is being displayed. Does
    /// not affect timed notifications, user input notifications, etc.
    pub fn clear_persistent_notif(&mut self) {
        self.persistent_msg = None;
    }
}

pub fn render_notification_line(
    frame: &mut Frame, area: Rect, notification: &NotificationManager,
    sync_progress: &Arc<RwLock<SyncProgress>>, colors: &AppColors,
) {
    let sp = sync_progress.read().expect("RwLock read should not fail");
    let sync_text = if sp.is_active() {
        format!("Syncing {}/{} ", sp.completed, sp.total)
    } else {
        String::new()
    };
    drop(sp);

    let sync_width = u16::try_from(sync_text.len()).unwrap_or(0);
    let [notif_area, sync_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(sync_width)]).areas(area);

    let cur_notif = if notification.current_msg.is_some() {
        notification
            .current_msg
            .as_ref()
            .map(|(message, _)| message.clone())
    } else {
        notification.persistent_msg.clone()
    };
    let line = cur_notif.map_or_else(
        || Line::from(" ").style(colors.normal),
        |notif| {
            if notif.error {
                Line::from(notif.message).style(colors.error).bold()
            } else {
                Line::from(notif.message).style(colors.normal)
            }
        },
    );
    frame.render_widget(line, notif_area);

    if !sync_text.is_empty() {
        let sync_line = Line::from(sync_text)
            .style(colors.normal)
            .alignment(Alignment::Right);
        frame.render_widget(sync_line, sync_area);
    }
}
