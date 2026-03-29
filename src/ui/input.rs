use std::time::Duration;

use ratatui::{
    crossterm::event::{self, Event, KeyCode},
    widgets::ListState,
};
use tui_input::backend::crossterm::EventHandler;

use crate::{
    config::{SEEK_LENGTH, TICK_RATE},
    keymap::UserAction,
    media_control::ControlMessage,
    player::PlayerMessage,
    types::FilterType,
};

use super::{Panel, Popup, UiMsg, UiState};

impl UiState {
    /// Waits for user input and, where necessary, provides `UiMsgs` back to the
    /// main controller.
    ///
    /// Anything UI-related (e.g., scrolling up and down menus) is handled
    /// internally, producing an empty `UiMsg`. This allows for some greater
    /// degree of abstraction; for example, input to add a new podcast feed
    /// spawns a UI window to capture the feed URL, and only then passes this
    /// data back to the main controller.
    #[allow(clippy::too_many_lines)]
    pub(super) fn getch(&mut self) -> Vec<UiMsg> {
        if event::poll(Duration::from_millis(TICK_RATE)).expect("Can't poll for inputs")
            && let Event::Key(input) = event::read().expect("Can't read inputs")
        {
            let action = self.keymap.get_from_input(input).copied();
            if let Some(popup) = self.active_popup.clone() {
                if action == Some(UserAction::Back) {
                    self.active_popup = None;
                } else {
                    match popup {
                        Popup::Welcome | Popup::Details | Popup::Help => match action {
                            Some(
                                a @ (UserAction::Down
                                | UserAction::Up
                                | UserAction::PageUp
                                | UserAction::PageDown
                                | UserAction::GoTop
                                | UserAction::GoBot),
                            ) => {
                                self.move_cursor(a);
                            }
                            Some(UserAction::Help) => {
                                self.active_popup = Some(Popup::Help);
                            }
                            _ => {}
                        },
                        Popup::AddPodcast => match input.code {
                            KeyCode::Enter => {
                                self.active_popup = None;
                                return vec![UiMsg::AddFeed(self.input.value().to_string())];
                            }
                            _ => {
                                self.input.handle_event(&Event::Key(input));
                            }
                        },
                        Popup::ConfirmRemovePodcast => match input.code {
                            KeyCode::Char('y') => {
                                self.active_popup = None;
                                if let Some(msg) = self.remove_podcast() {
                                    return vec![msg];
                                }
                            }
                            KeyCode::Char('n') => {
                                self.active_popup = None;
                            }
                            _ => {}
                        },
                        Popup::ConfirmQuit => match input.code {
                            KeyCode::Char('y') => {
                                self.active_popup = None;
                                return vec![UiMsg::Quit];
                            }
                            KeyCode::Char('n') => {
                                self.active_popup = None;
                            }
                            _ => {}
                        },
                    }
                }
            } else {
                match action {
                    Some(
                        a @ (UserAction::Down
                        | UserAction::Up
                        | UserAction::PageUp
                        | UserAction::PageDown
                        | UserAction::GoTop
                        | UserAction::GoBot),
                    ) => {
                        self.move_cursor(a);
                    }

                    Some(UserAction::Left) => {
                        self.tx_to_player
                            .send(PlayerMessage::Seek(SEEK_LENGTH, false))
                            .inspect_err(|err| {
                                log::error!("Failed to send PlayerMessage::Seek to player: {err}");
                            })
                            .ok();
                    }

                    Some(UserAction::Right) => {
                        self.tx_to_player
                            .send(PlayerMessage::Seek(SEEK_LENGTH, true))
                            .inspect_err(|err| {
                                log::error!("Failed to send PlayerMessage::Seek to player: {err}");
                            })
                            .ok();
                    }

                    Some(UserAction::ResetPlayer) => {
                        self.tx_to_player
                            .send(PlayerMessage::ResetSink)
                            .inspect_err(|err| {
                                log::error!(
                                    "Failed to send PlayerMessage::ResetSink to player: {err}"
                                );
                            })
                            .ok();
                    }

                    Some(a @ (UserAction::MoveUp | UserAction::MoveDown)) => {
                        if self.active_panel == Panel::Queue
                            && let Some(ui_msg) = self.move_eps(a)
                        {
                            return vec![ui_msg];
                        }
                    }

                    Some(UserAction::AddFeed) => {
                        self.input.reset();
                        self.active_popup = Some(Popup::AddPodcast);
                    }

                    Some(UserAction::Sync) => {
                        if self.active_panel == Panel::Podcasts
                            && let Some(pod_id) = self.get_podcast_id()
                        {
                            return vec![UiMsg::Sync(pod_id)];
                        }
                    }
                    Some(UserAction::SyncAll) => {
                        return vec![UiMsg::SyncAll];
                    }

                    Some(UserAction::SyncGpodder) => {
                        return vec![UiMsg::SyncGpodder];
                    }

                    Some(UserAction::Enter) => match self.active_panel {
                        Panel::Podcasts => {
                            if let Some(pod_id) = self.get_podcast_id() {
                                self.select_panel(&Panel::Episodes);

                                if let Some(items) = self
                                    .podcasts
                                    .items
                                    .map_single(pod_id, |x| x.episodes.clone())
                                {
                                    self.episodes.items = items;
                                    self.episodes.state =
                                        ListState::default().with_selected(Some(0));
                                }
                            }
                        }
                        Panel::Queue | Panel::Episodes | Panel::Unplayed => {
                            return self.play_selected_episode();
                        }
                    },

                    Some(UserAction::PlayExternal) => match self.active_panel {
                        Panel::Queue | Panel::Episodes | Panel::Unplayed => {
                            if let Some(pod_id) = self.get_podcast_id()
                                && let Some(ep_id) = self.get_episode_id()
                            {
                                self.construct_current_episode(ep_id);
                                return vec![UiMsg::Play(pod_id, ep_id, true)];
                            }
                        }
                        Panel::Podcasts => {}
                    },

                    Some(UserAction::Enqueue) => match self.active_panel {
                        Panel::Episodes | Panel::Unplayed => {
                            if let Some(ep_id) = self.get_episode_id()
                                && !self.queue.items.contains_key(ep_id)
                            {
                                if self.left_panel == Panel::Episodes {
                                    if let Some(ep) = self.episodes.items.get(ep_id) {
                                        self.queue.items.push_arc(ep);
                                        return vec![UiMsg::QueueModified];
                                    }
                                } else if self.left_panel == Panel::Unplayed
                                    && let Some(ep) = self.unplayed.items.get(ep_id)
                                {
                                    self.queue.items.push_arc(ep);
                                    return vec![UiMsg::QueueModified];
                                }
                            }
                        }
                        Panel::Queue | Panel::Podcasts => {}
                    },
                    Some(UserAction::PlayPause) => {
                        if let Some(msg) = self.play_pause() {
                            return vec![msg];
                        }
                    }
                    Some(UserAction::MarkPlayed) => match self.active_panel {
                        Panel::Episodes | Panel::Unplayed | Panel::Queue => {
                            if let Some(ui_msg) = self.mark_played() {
                                return vec![ui_msg];
                            }
                        }
                        Panel::Podcasts => {}
                    },
                    Some(UserAction::MarkAllPlayed) => {
                        if self.active_panel == Panel::Episodes
                            && let Some(ui_msg) = self.mark_all_played()
                        {
                            return vec![ui_msg];
                        }
                    }

                    Some(UserAction::Download) => match self.active_panel {
                        Panel::Episodes | Panel::Unplayed | Panel::Queue => {
                            if let Some(pod_id) = self.get_podcast_id()
                                && let Some(ep_id) = self.get_episode_id()
                            {
                                return vec![UiMsg::Download(pod_id, ep_id)];
                            }
                        }
                        Panel::Podcasts => {}
                    },
                    Some(UserAction::DownloadAll) => {
                        if self.active_panel == Panel::Podcasts
                            && let Some(pod_id) = self.get_podcast_id()
                        {
                            return vec![UiMsg::DownloadAll(pod_id)];
                        }
                    }

                    Some(UserAction::Delete) => match self.active_panel {
                        Panel::Episodes | Panel::Queue | Panel::Unplayed => {
                            if let Some(pod_id) = self.get_podcast_id()
                                && let Some(ep_id) = self.get_episode_id()
                            {
                                return vec![UiMsg::Delete(pod_id, ep_id)];
                            }
                        }
                        Panel::Podcasts => {}
                    },
                    Some(UserAction::DeleteAll) => {
                        if self.active_panel == Panel::Podcasts
                            && let Some(pod_id) = self.get_podcast_id()
                        {
                            return vec![UiMsg::DeleteAll(pod_id)];
                        }
                    }

                    Some(UserAction::Remove) => match self.active_panel {
                        Panel::Podcasts => {
                            self.active_popup = Some(Popup::ConfirmRemovePodcast);
                        }
                        Panel::Queue => {
                            if let Some(ep_id) = self.get_episode_id() {
                                self.queue.items.remove(ep_id);
                                return vec![UiMsg::QueueModified];
                            }
                        }
                        _ => {}
                    },

                    Some(UserAction::FilterPlayed) => {
                        return vec![UiMsg::FilterChange(FilterType::Played)];
                    }
                    Some(UserAction::FilterDownloaded) => {
                        return vec![UiMsg::FilterChange(FilterType::Downloaded)];
                    }

                    Some(UserAction::Help) => {
                        self.active_popup = Some(Popup::Help);
                    }

                    Some(UserAction::Quit) => {
                        if self.active_popup.is_some() {
                            self.active_popup = None;
                        } else if self.confirm_quit {
                            self.active_popup = Some(Popup::ConfirmQuit);
                        } else {
                            return vec![UiMsg::Quit];
                        }
                    }

                    Some(UserAction::UnplayedList) => {
                        if self.active_popup.is_none() {
                            match self.active_panel {
                                Panel::Podcasts => {
                                    self.select_panel(&Panel::Unplayed);
                                }
                                Panel::Unplayed => {
                                    self.select_panel(&Panel::Podcasts);
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(UserAction::Information) => {
                        match self.active_panel {
                            Panel::Episodes | Panel::Queue | Panel::Unplayed => {
                                self.construct_details_episode();
                            }
                            Panel::Podcasts => {
                                self.construct_details_podcast();
                            }
                        }
                        self.active_popup = Some(Popup::Details);
                    }
                    Some(UserAction::Back) => {
                        if self.active_panel == Panel::Episodes {
                            self.select_panel(&Panel::Podcasts);
                        }
                    }
                    Some(UserAction::Switch) => match self.active_panel {
                        Panel::Episodes | Panel::Podcasts | Panel::Unplayed => {
                            self.select_panel(&Panel::Queue);
                        }
                        Panel::Queue => {
                            self.select_panel(&self.left_panel.clone());
                        }
                    },
                    None => (),
                }
            }
        }
        vec![UiMsg::Noop]
    }

    pub(super) fn getcontrol(&self) -> Option<UiMsg> {
        let mut control_message_iter = self.rx_from_control.try_iter();
        let message = control_message_iter.next()?;
        match message {
            ControlMessage::PlayPause => self.play_pause(),
        }
    }
}
