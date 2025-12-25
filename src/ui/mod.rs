use anyhow::{Result, anyhow};
use std::{
    sync::{Arc, RwLock, mpsc},
    time::Duration,
};

use chrono::{DateTime, Utc};
use notification::render_notification_line;
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode},
    layout::{Alignment, Constraint, Flex, Layout},
    prelude::Rect,
    style::{Style, Stylize},
    text::Line,
    widgets::{Block, Clear, Gauge, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap},
};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::{
    app::MainMessage,
    config::{Config, SCROLL_AMOUNT, SEEK_LENGTH, TICK_RATE},
    keymap::{Keybindings, UserAction},
    media_control::ControlMessage,
    player::{PlaybackStatus, PlayerMessage},
    types::{Episode, FilterType, LockVec, Menuable, Message, Podcast, ShareableRwLock},
    utils::{clean_html, format_duration},
};

use self::colors::AppColors;
use self::notification::NotificationManager;

pub use types::UiMsg;
pub mod colors;
mod notification;
mod types;

#[derive(Debug, Clone, PartialEq)]
enum Panel {
    Podcasts,
    Episodes,
    Unplayed,
    Queue,
}

#[derive(Debug, Clone)]
enum Popup {
    Welcome,
    Details,
    Help,
    AddPodcast,
    ConfirmRemovePodcast,
    ConfirmQuit,
}
#[derive(Debug)]
struct MenuList<T: Menuable> {
    title: String,
    items: LockVec<T>,
    state: ListState,
    selected_item_id: Option<i64>,
}

#[derive(Debug)]
pub struct Details {
    pub pubdate: Option<DateTime<Utc>>,
    pub position: Option<String>,
    pub duration: Option<String>,
    pub explicit: Option<bool>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub last_checked: Option<DateTime<Utc>>,
    pub episode_title: Option<String>,
    pub podcast_title: Option<String>,
    url: String,
}

pub struct UiState {
    keymap: Keybindings,
    colors: AppColors,
    confirm_quit: bool,
    podcasts: MenuList<Podcast>,
    episodes: MenuList<Episode>,
    unplayed: MenuList<Episode>,
    queue: MenuList<Episode>,
    active_panel: Panel,
    left_panel: Panel,
    active_popup: Option<Popup>,
    scroll_popup: u16,
    notification: NotificationManager,
    current_episode: ShareableRwLock<Option<ShareableRwLock<Episode>>>,
    current_podcast_title: Option<String>,
    current_details: Option<Details>,
    input: Input,
    pub tx_to_player: mpsc::Sender<PlayerMessage>,
    elapsed: Arc<RwLock<u64>>,
    playing: Arc<RwLock<PlaybackStatus>>,
    pub rx_from_control: mpsc::Receiver<ControlMessage>,
}

impl<T: Menuable> MenuList<T> {
    fn sync_selected_with_state(&mut self) {
        self.selected_item_id = match self.state.selected() {
            Some(index) => self.items.get_id_by_index(index),
            None => None,
        };
    }
    fn sync_state_with_selected(&mut self) -> bool {
        match self.selected_item_id {
            None => false,
            Some(id) => {
                let Some(index) = self.items.get_index(id) else {
                    return false;
                };
                self.state.select(Some(index));
                true
            }
        }
    }
}

impl UiState {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_blocking(
        config: Arc<Config>, items: LockVec<Podcast>, queue_items: LockVec<Episode>,
        unplayed_items: LockVec<Episode>, rx_from_main: mpsc::Receiver<MainMessage>,
        tx_to_main: mpsc::Sender<Message>, tx_to_player: mpsc::Sender<PlayerMessage>,
        rx_from_control: mpsc::Receiver<ControlMessage>,
        current_episode: ShareableRwLock<Option<ShareableRwLock<Episode>>>,
        elapsed: ShareableRwLock<u64>, playing: ShareableRwLock<PlaybackStatus>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::task::spawn_blocking(move || {
            let mut ui = Self::new(
                &config,
                &items,
                &queue_items,
                &unplayed_items,
                tx_to_player,
                rx_from_control,
                current_episode,
                elapsed,
                playing,
            );
            let mut terminal = ratatui::init();
            let mut main_message_iter = rx_from_main.try_iter();
            loop {
                ui.notification.check_notifs();
                if ui.playback_finished() {
                    let mut msgs = vec![];
                    if let Some(msg) = ui.update_position() {
                        msgs.push(msg);
                    }
                    *ui.playing.write().unwrap() = PlaybackStatus::Ready;

                    // make it a config option
                    let mut clear_episode = true;
                    if let Some(ep) = ui.current_episode.read().unwrap().as_ref() {
                        let ep = ep.read().unwrap();
                        if let Some(queue_index) = ui.queue.items.get_index(ep.id) {
                            if let Some(next_ep) = ui.next_from_queue(queue_index) {
                                let next_ep = next_ep.read().unwrap();
                                let mut res = ui.play_episode(next_ep.pod_id, next_ep.id);
                                clear_episode = false;
                                msgs.append(&mut res);
                            }
                            ui.queue.items.remove(ep.id);
                            msgs.push(UiMsg::QueueModified);
                        }
                    }

                    if clear_episode {
                        *ui.current_episode.write().unwrap() = None;
                    }
                    for msg in msgs {
                        let _ = tx_to_main.send(Message::Ui(msg));
                    }
                }
                let msgs = ui.getch();
                for msg in msgs {
                    match msg {
                        UiMsg::Noop => (),
                        msg => tx_to_main
                            .send(Message::Ui(msg))
                            .expect("Thread messaging error"),
                    }
                }

                if let Some(msg) = ui.getcontrol() {
                    match msg {
                        UiMsg::Noop => (),
                        msg => tx_to_main
                            .send(Message::Ui(msg))
                            .expect("Thread messaging error"),
                    }
                }

                if let Some(message) = main_message_iter.next() {
                    match message {
                        MainMessage::SpawnNotif(msg, duration, error) => {
                            ui.notification.timed_notif(msg, duration, error);
                        }
                        MainMessage::SpawnPersistentNotif(msg, error) => {
                            ui.notification.persistent_notif(msg, error);
                        }
                        MainMessage::ClearPersistentNotif => {
                            ui.notification.clear_persistent_notif();
                        }
                        MainMessage::PlayCurrent(ep_id) => match ui.play_current(ep_id) {
                            Ok(()) => {}
                            Err(err) => {
                                log::warn!("Playing current episode failed: {err}");
                            }
                        },
                        MainMessage::TearDown => {
                            break;
                        }
                    }
                }
                let _ = terminal.draw(|frame| ui.draw(frame));
            }
            ratatui::restore();
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            std::process::exit(0);
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &Arc<Config>, podcast_items: &LockVec<Podcast>, queue_items: &LockVec<Episode>,
        unplayed_items: &LockVec<Episode>, tx_to_player: mpsc::Sender<PlayerMessage>,
        rx_from_control: mpsc::Receiver<ControlMessage>,
        current_episode: ShareableRwLock<Option<ShareableRwLock<Episode>>>,
        elapsed: ShareableRwLock<u64>, playing: ShareableRwLock<PlaybackStatus>,
    ) -> Self {
        let active_popup = if podcast_items.is_empty() {
            Some(Popup::Welcome)
        } else {
            None
        };

        Self {
            keymap: config.keybindings.clone(),
            colors: config.colors.clone(),
            confirm_quit: config.confirm_quit,
            podcasts: MenuList::<Podcast> {
                title: "Podcasts".to_string(),
                items: podcast_items.clone(),
                state: ListState::default().with_selected(Some(0)),
                selected_item_id: podcast_items.get_id_by_index(0),
            },
            unplayed: MenuList::<Episode> {
                title: "Unplayed".to_string(),
                items: unplayed_items.clone(),
                state: ListState::default().with_selected(Some(0)),
                selected_item_id: unplayed_items.get_id_by_index(0),
            },
            episodes: MenuList::<Episode> {
                title: "Episodes".to_string(),
                items: LockVec::new(vec![]),
                state: ListState::default(),
                selected_item_id: None,
            },
            queue: MenuList::<Episode> {
                title: "Queue".to_string(),
                items: queue_items.clone(),
                state: ListState::default().with_selected(Some(0)),
                selected_item_id: queue_items.get_id_by_index(0),
            },
            active_panel: Panel::Podcasts,
            left_panel: Panel::Podcasts,
            active_popup,
            scroll_popup: 0,
            notification: NotificationManager::new(),
            current_episode,
            current_podcast_title: None,
            current_details: None,
            input: Input::default(),
            tx_to_player,
            elapsed,
            playing,
            rx_from_control,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();

        let vertical_layout = Layout::vertical([
            Constraint::Length(6),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]);
        let horizontal_layout =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]);
        let [play_area, center_area, notif_area, help_area] = vertical_layout.areas(area);
        let [select_area, queue_area] = horizontal_layout.areas(center_area);

        render_play_area(
            frame,
            play_area,
            &self.current_episode,
            self.current_podcast_title.as_ref(),
            *self.elapsed.read().unwrap(),
            &self.colors,
        );
        match self.left_panel {
            Panel::Podcasts => render_menuable_area(
                frame,
                select_area,
                &mut self.podcasts,
                &self.colors,
                self.active_panel == Panel::Podcasts,
            ),
            Panel::Episodes => render_menuable_area(
                frame,
                select_area,
                &mut self.episodes,
                &self.colors,
                self.active_panel == Panel::Episodes,
            ),
            Panel::Unplayed => render_menuable_area(
                frame,
                select_area,
                &mut self.unplayed,
                &self.colors,
                self.active_panel == Panel::Unplayed,
            ),
            Panel::Queue => {}
        }
        render_menuable_area(
            frame,
            queue_area,
            &mut self.queue,
            &self.colors,
            self.active_panel == Panel::Queue,
        );

        render_notification_line(frame, notif_area, &self.notification, &self.colors);
        render_help_line(frame, help_area, &self.keymap, &self.colors);

        if let Some(active_popup) = &self.active_popup {
            match active_popup {
                Popup::Welcome => {
                    render_welcome_popup(
                        frame,
                        compute_popup_area(area, 30, 30),
                        self.scroll_popup,
                        &self.keymap,
                        &self.colors,
                    );
                }
                Popup::Details => {
                    render_details_popup(
                        frame,
                        compute_popup_area(area, 70, 70),
                        self.current_details.as_ref(),
                        self.scroll_popup,
                        &self.colors,
                    );
                }
                Popup::Help => {
                    render_shortcut_help_popup(
                        frame,
                        compute_popup_area(area, 40, 70),
                        self.scroll_popup,
                        &self.keymap,
                        &self.colors,
                    );
                }
                Popup::AddPodcast => {
                    render_add_podcast_popup(
                        frame,
                        compute_popup_area(area, 30, 80),
                        &self.input,
                        &self.colors,
                    );
                }
                Popup::ConfirmRemovePodcast => {
                    render_confirmation_popup(
                        frame,
                        compute_popup_area(area, 30, 70),
                        "Do you want to remove the podcast?".to_string(),
                        &self.colors,
                    );
                }
                Popup::ConfirmQuit => {
                    render_confirmation_popup(
                        frame,
                        compute_popup_area(area, 30, 70),
                        "Do you want to quit the app?".to_string(),
                        &self.colors,
                    );
                }
            }
        }
    }

    fn move_cursor(&mut self, action: UserAction) {
        if self.active_popup.is_some() {
            match action {
                UserAction::Down => {
                    self.scroll_popup = self.scroll_popup.saturating_add(1);
                }

                UserAction::Up => {
                    self.scroll_popup = self.scroll_popup.saturating_sub(1);
                }

                UserAction::PageUp => {
                    self.scroll_popup = self.scroll_popup.saturating_sub(SCROLL_AMOUNT);
                }

                UserAction::PageDown => {
                    self.scroll_popup = self.scroll_popup.saturating_add(SCROLL_AMOUNT);
                }

                UserAction::GoTop => {
                    self.scroll_popup = 0;
                }

                _ => (),
            }
        } else {
            let current_state = {
                match self.active_panel {
                    Panel::Podcasts => &mut self.podcasts.state,
                    Panel::Unplayed => &mut self.unplayed.state,
                    Panel::Episodes => &mut self.episodes.state,
                    Panel::Queue => &mut self.queue.state,
                }
            };
            match action {
                UserAction::Down => current_state.select_next(),

                UserAction::Up => current_state.select_previous(),

                UserAction::Left => match self.active_panel {
                    Panel::Podcasts | Panel::Unplayed => {}
                    Panel::Episodes => {
                        self.select_panel(&Panel::Podcasts);
                    }
                    Panel::Queue => {
                        self.queue.state.select(None);
                        self.select_panel(&self.left_panel.clone());
                    }
                },

                UserAction::Right => match self.active_panel {
                    Panel::Podcasts | Panel::Unplayed | Panel::Episodes => {
                        self.active_panel = Panel::Queue;
                        self.queue.state.select_first();
                    }
                    Panel::Queue => {}
                },

                UserAction::PageUp => current_state.scroll_up_by(SCROLL_AMOUNT),

                UserAction::PageDown => current_state.scroll_down_by(SCROLL_AMOUNT),

                UserAction::GoTop => current_state.select_first(),

                UserAction::GoBot => current_state.select_last(),

                _ => (),
            }

            match self.active_panel {
                Panel::Podcasts => self.podcasts.sync_selected_with_state(),
                Panel::Unplayed => self.unplayed.sync_selected_with_state(),
                Panel::Episodes => self.episodes.sync_selected_with_state(),
                Panel::Queue => self.queue.sync_selected_with_state(),
            }
        }
    }

    fn get_episode_id(&self) -> Option<i64> {
        match self.active_panel {
            Panel::Podcasts => None,
            Panel::Episodes => {
                let id = self.episodes.state.selected()?;
                self.episodes.items.map_single_by_index(id, |x| x.id)
            }
            Panel::Unplayed => {
                let id = self.unplayed.state.selected()?;
                self.unplayed.items.map_single_by_index(id, |x| x.id)
            }
            Panel::Queue => {
                let id = self.queue.state.selected()?;
                self.queue.items.map_single_by_index(id, |x| x.id)
            }
        }
    }

    fn get_podcast_id(&self) -> Option<i64> {
        match self.active_panel {
            Panel::Podcasts => {
                let id = self.podcasts.state.selected()?;
                self.podcasts.items.map_single_by_index(id, |x| x.id)
            }
            Panel::Episodes => {
                let id = self.episodes.state.selected()?;
                self.episodes.items.map_single_by_index(id, |x| x.pod_id)
            }
            Panel::Unplayed => {
                let id = self.unplayed.state.selected()?;
                self.unplayed.items.map_single_by_index(id, |x| x.pod_id)
            }
            Panel::Queue => {
                let id = self.queue.state.selected()?;
                self.queue.items.map_single_by_index(id, |x| x.pod_id)
            }
        }
    }

    const fn select_panel(&mut self, panel: &Panel) {
        match panel {
            Panel::Podcasts => {
                self.active_panel = Panel::Podcasts;
                self.left_panel = Panel::Podcasts;
            }
            Panel::Episodes => {
                self.active_panel = Panel::Episodes;
                self.left_panel = Panel::Episodes;
            }
            Panel::Unplayed => {
                self.active_panel = Panel::Unplayed;
                self.left_panel = Panel::Unplayed;
            }
            Panel::Queue => {
                self.active_panel = Panel::Queue;
            }
        }
    }

    /// Waits for user input and, where necessary, provides `UiMsgs` back to the
    /// main controller.
    ///
    /// Anything UI-related (e.g., scrolling up and down menus) is handled
    /// internally, producing an empty `UiMsg`. This allows for some greater
    /// degree of abstraction; for example, input to add a new podcast feed
    /// spawns a UI window to capture the feed URL, and only then passes this
    /// data back to the main controller.
    #[allow(clippy::too_many_lines)]
    fn getch(&mut self) -> Vec<UiMsg> {
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
                        let _ = self
                            .tx_to_player
                            .send(PlayerMessage::Seek(SEEK_LENGTH, false));
                    }

                    Some(UserAction::Right) => {
                        let _ = self
                            .tx_to_player
                            .send(PlayerMessage::Seek(SEEK_LENGTH, true));
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

    fn mark_played(&self) -> Option<UiMsg> {
        let pod_id = self.get_podcast_id()?;
        let ep_id = self.get_episode_id()?;
        match self.active_panel {
            Panel::Episodes => {
                let played = self
                    .episodes
                    .items
                    .map_single(ep_id, super::types::Menuable::is_played)?;
                Some(UiMsg::MarkPlayed(pod_id, ep_id, !played))
            }
            Panel::Unplayed => {
                let played = self
                    .unplayed
                    .items
                    .map_single(ep_id, super::types::Menuable::is_played)?;
                Some(UiMsg::MarkPlayed(pod_id, ep_id, !played))
            }
            Panel::Queue => {
                let played = self
                    .queue
                    .items
                    .map_single(ep_id, super::types::Menuable::is_played)?;
                Some(UiMsg::MarkPlayed(pod_id, ep_id, !played))
            }
            Panel::Podcasts => None,
        }
    }

    pub fn mark_all_played(&self) -> Option<UiMsg> {
        let pod_id = self.get_podcast_id()?;
        let played = self
            .podcasts
            .items
            .map_single(pod_id, super::types::Menuable::is_played)?;
        Some(UiMsg::MarkAllPlayed(pod_id, !played))
    }
    fn remove_podcast(&self) -> Option<UiMsg> {
        let pod_id = self.get_podcast_id()?;
        Some(UiMsg::RemovePodcast(pod_id, true))
    }
    fn move_eps(&mut self, action: UserAction) -> Option<UiMsg> {
        let selected = self.queue.state.selected()?;

        match action {
            UserAction::MoveDown => {
                if selected + 1 < self.queue.items.len(false) {
                    {
                        let mut order_vec = self.queue.items.borrow_order();
                        order_vec.swap(selected, selected + 1);
                    }
                    self.queue.state.select(Some(selected + 1));
                    self.queue.sync_selected_with_state();
                    return Some(UiMsg::QueueModified);
                }
            }
            UserAction::MoveUp => {
                if selected >= 1 {
                    {
                        let mut order_vec = self.queue.items.borrow_order();
                        order_vec.swap(selected, selected - 1);
                    }
                    self.queue.state.select(Some(selected - 1));
                    self.queue.sync_selected_with_state();
                    return Some(UiMsg::QueueModified);
                }
            }
            _ => (),
        }

        None
    }

    fn construct_details_episode(&mut self) {
        if let Some(ep_id) = self.get_episode_id() {
            let ep = match self.active_panel {
                Panel::Episodes => self.episodes.items.get(ep_id),
                Panel::Queue => self.queue.items.get(ep_id),
                Panel::Unplayed => self.unplayed.items.get(ep_id),
                Panel::Podcasts => None,
            };
            if let Some(ep) = ep {
                let ep = ep.read().unwrap();
                let desc = clean_html(&ep.description);
                let podcast_title = {
                    let pod_map = self.podcasts.items.borrow_map();
                    let pod = pod_map.get(&ep.pod_id);
                    pod.map(|pod| pod.read().unwrap().title.clone())
                };
                self.current_details = Some(Details {
                    pubdate: ep.pubdate,
                    position: Some(format_duration(Some(ep.position as u64))),
                    duration: Some(format_duration(ep.duration.map(|x| x as u64))),
                    explicit: None,
                    description: Some(desc),
                    author: None,
                    last_checked: None,
                    episode_title: Some(ep.title.clone()),
                    podcast_title,
                    url: ep.url.clone(),
                });
            }
        }
    }

    fn construct_details_podcast(&mut self) {
        if let Some(pod_id) = self.get_podcast_id()
            && let Some(pod) = self.podcasts.items.get(pod_id)
        {
            let pod = pod.read().unwrap();
            let desc = pod.description.clone().map(|desc| clean_html(&desc));
            self.current_details = Some(Details {
                pubdate: None,
                position: None,
                duration: None,
                explicit: pod.explicit,
                description: desc,
                author: pod.author.clone(),
                last_checked: Some(pod.last_checked),
                episode_title: None,
                podcast_title: Some(pod.title.clone()),
                url: pod.url.clone(),
            });
        }
    }
    fn construct_current_episode(&mut self, ep_id: i64) {
        let ep = match self.active_panel {
            Panel::Episodes => self.episodes.items.get(ep_id),
            Panel::Queue => self.queue.items.get(ep_id),
            Panel::Unplayed => self.unplayed.items.get(ep_id),
            Panel::Podcasts => None,
        };
        if let Some(ep_arc) = ep {
            let ep = ep_arc.read().unwrap();
            let podcast_title = {
                let pod_map = self.podcasts.items.borrow_map();
                let pod = pod_map.get(&ep.pod_id);
                pod.map(|pod| pod.read().unwrap().title.clone()).unwrap()
            };
            self.current_podcast_title = Some(podcast_title);
            *self.current_episode.write().unwrap() = Some(ep_arc.clone());
        }
    }

    fn play_current(&mut self, ep_id: i64) -> Result<()> {
        self.construct_current_episode(ep_id);
        let ep = self.current_episode.read().unwrap();
        let ep = ep
            .as_ref()
            .ok_or_else(|| anyhow!("Failed to get current episode"))?
            .read()
            .unwrap();
        *self.elapsed.write().unwrap() = ep.position as u64;
        if let Some(path) = &ep.path {
            self.tx_to_player.send(PlayerMessage::PlayFile(
                path.clone(),
                ep.position as u64,
                ep.duration.unwrap() as u64,
            ))?;
        } else {
            self.tx_to_player.send(PlayerMessage::PlayUrl(
                ep.url.clone(),
                ep.position as u64,
                ep.duration.unwrap_or(0) as u64,
            ))?;
        }
        Ok(())
    }
    fn playback_finished(&self) -> bool {
        self.current_episode.read().unwrap().is_some()
            && *self.playing.read().unwrap() == PlaybackStatus::Finished
    }

    fn update_position(&self) -> Option<UiMsg> {
        let cur_ep = self.current_episode.read().unwrap();
        let cur_ep = cur_ep.as_ref()?;
        let position = *self.elapsed.read().unwrap() as i64;
        let cur_ep = cur_ep.read().unwrap();
        Some(UiMsg::UpdatePosition(cur_ep.pod_id, cur_ep.id, position))
    }

    fn getcontrol(&self) -> Option<UiMsg> {
        let mut control_message_iter = self.rx_from_control.try_iter();
        let message = control_message_iter.next()?;
        match message {
            ControlMessage::PlayPause => self.play_pause(),
        }
    }

    fn play_pause(&self) -> Option<UiMsg> {
        let playing = self.playing.read().unwrap();
        self.tx_to_player.send(PlayerMessage::PlayPause).ok()?;
        // only updates position after Pause
        match *playing {
            PlaybackStatus::Playing => self.update_position(),
            _ => None,
        }
    }

    fn play_selected_episode(&self) -> Vec<UiMsg> {
        if let Some(pod_id) = self.get_podcast_id()
            && let Some(ep_id) = self.get_episode_id()
        {
            return self.play_episode(pod_id, ep_id);
        }
        vec![]
    }

    fn play_episode(&self, pod_id: i64, ep_id: i64) -> Vec<UiMsg> {
        let (same, playing, cur_ep_id, cur_pod_id) = self
            .current_episode
            .read()
            .unwrap()
            .as_ref()
            .map_or((false, false, 0, 0), |cur_ep| {
                let cur_ep = cur_ep.read().unwrap();
                (
                    cur_ep.id == ep_id && cur_ep.pod_id == pod_id,
                    *self.playing.read().unwrap() == PlaybackStatus::Playing,
                    cur_ep.id,
                    cur_ep.pod_id,
                )
            });
        if !same {
            if playing {
                let position = *self.elapsed.read().unwrap() as i64;
                return vec![
                    UiMsg::UpdatePosition(cur_pod_id, cur_ep_id, position),
                    UiMsg::Play(pod_id, ep_id, false),
                ];
            }
            return vec![UiMsg::Play(pod_id, ep_id, false)];
        } else if *self.playing.read().unwrap() == PlaybackStatus::Paused {
            let _ = self.tx_to_player.send(PlayerMessage::PlayPause);
        }
        vec![]
    }

    fn next_from_queue(&self, queue_index: usize) -> Option<ShareableRwLock<Episode>> {
        if queue_index + 1 < self.queue.items.len(false) {
            let order = self.queue.items.borrow_order();
            let ep_id = order.get(queue_index + 1)?;
            self.queue.items.get(*ep_id)
        } else {
            None
        }
    }
}

fn render_confirmation_popup(frame: &mut Frame, area: Rect, msg: String, colors: &AppColors) {
    let [_, mid_area, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(area);
    let input = Paragraph::new(msg + " y/n")
        .style(colors.error)
        .block(Block::bordered());
    frame.render_widget(Clear, mid_area);
    frame.render_widget(input, mid_area);
}

#[allow(clippy::cast_possible_truncation)]
fn render_add_podcast_popup(frame: &mut Frame, area: Rect, input: &Input, colors: &AppColors) {
    let [_, input_area, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(area);
    let width = area.width.max(3) - 3;
    let scroll = input.visual_scroll(width as usize);
    let input_text = Paragraph::new(input.value())
        .style(colors.normal)
        .scroll((0, scroll as u16))
        .block(Block::bordered().title("Podcast feed url:"));
    frame.render_widget(Clear, input_area);
    frame.render_widget(input_text, input_area);
    let x = input.visual_cursor().max(scroll) - scroll + 1;
    frame.set_cursor_position((area.x + x as u16, input_area.y + 1));
}

fn render_shortcut_help_popup(
    frame: &mut Frame, area: Rect, scroll: u16, keymap: &Keybindings, colors: &AppColors,
) {
    let actions = vec![
        (Some(UserAction::Up), "Up:"),
        (Some(UserAction::Down), "Down:"),
        (Some(UserAction::PageUp), "Page up:"),
        (Some(UserAction::PageDown), "Page down:"),
        (Some(UserAction::GoTop), "Go to top:"),
        (Some(UserAction::GoBot), "Go to bottom:"),
        //(None, ""),
        (Some(UserAction::AddFeed), "Add feed:"),
        (Some(UserAction::Sync), "Refresh podcast:"),
        (Some(UserAction::SyncAll), "Refresh all podcasts:"),
        (Some(UserAction::SyncGpodder), "Sync with gpodder:"),
        //(None, ""),
        (Some(UserAction::Enter), "Open podcast/Play episode:"),
        (Some(UserAction::PlayPause), "Play/Pause:"),
        (Some(UserAction::Left), "Seek backward:"),
        (Some(UserAction::Right), "Seek forward:"),
        (Some(UserAction::MarkPlayed), "Mark as played:"),
        (Some(UserAction::MarkAllPlayed), "Mark all as played:"),
        //(None, ""),
        (Some(UserAction::Enqueue), "Enqueue:"),
        (Some(UserAction::Remove), "Remove from queue:"),
        (Some(UserAction::Download), "Download:"),
        (Some(UserAction::DownloadAll), "Download all:"),
        (Some(UserAction::Delete), "Delete file:"),
        (Some(UserAction::DeleteAll), "Delete all files:"),
        (Some(UserAction::UnplayedList), "Show/Hide Unplayed Panel"),
        (Some(UserAction::Help), "Help:"),
        (Some(UserAction::Back), "Back:"),
        (Some(UserAction::Quit), "Quit:"),
    ];
    let mut key_strs = Vec::new();
    let mut back_key = "<missing>".to_string();
    for (action, action_str) in actions {
        match action {
            Some(action) => {
                if let Some(keys) = keymap.keys_for_action(action) {
                    // longest prefix is 21 chars long
                    let key_str = match keys.len() {
                        0 => format!("{action_str:>28} <missing>"),
                        1 => format!("{:>28} \"{}\"", action_str, &keys[0]),
                        _ => format!("{:>28} \"{}\" or \"{}\"", action_str, &keys[0], &keys[1]),
                    };
                    if action == UserAction::Back && !keys.is_empty() {
                        back_key = format!("\"{}\"", keys[0]);
                    }
                    key_strs.push(key_str);
                }
            }
            None => key_strs.push(" ".to_string()),
        }
    }
    let line: Vec<Line> = key_strs.iter().map(|s| Line::from(s.as_str())).collect();
    let paragraph = Paragraph::new(line).scroll((scroll, 0));
    let last_line =
        Line::from(format!("Press {back_key} to close this window.")).alignment(Alignment::Right);

    let vertical = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]);

    let block = Block::bordered()
        .title("Available keybindings")
        .style(colors.normal);
    let inner = block.inner(area);
    let [keys, last] = vertical.areas(inner);

    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(paragraph, keys);
    frame.render_widget(last_line, last);
}

fn render_welcome_popup(
    frame: &mut Frame, area: Rect, scroll: u16, keymap: &Keybindings, colors: &AppColors,
) {
    let actions = vec![UserAction::AddFeed, UserAction::Quit, UserAction::Help];
    let mut key_strs = Vec::new();
    for action in actions {
        key_strs.push(keymap.keys_for_action(action).unwrap()[0].clone());
    }

    let line1 = format!(
        "Your podcast list is currently empty. Press \"{}\" to add a new podcast feed, \"{}\" to quit, or see all available commands by typing \"{}\" to get help.",
        key_strs[0], key_strs[1], key_strs[2]
    );
    let line2 = "More details of how to customize hullcaster can be found on the Github repo readme: https://github.com/gilcu3/hullcaster";
    let paragraph = Paragraph::new(vec![
        Line::from(""),
        Line::from(line1),
        Line::from(""),
        Line::from(line2),
    ])
    .scroll((scroll, 0))
    .wrap(Wrap { trim: true })
    .centered();

    let block = Block::bordered().title(" Welcome ").style(colors.normal);
    let inner = block.inner(area);

    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(paragraph, inner);
}

fn render_details_popup(
    frame: &mut Frame, area: Rect, details: Option<&Details>, scroll: u16, colors: &AppColors,
) {
    if let Some(details) = details {
        let mut v = vec![];
        v.push(Line::from(""));

        if let Some(title) = &details.podcast_title {
            v.push(Line::from("Podcast: ".to_string() + title));
            v.push(Line::from(""));
        }

        if let Some(title) = &details.episode_title {
            v.push(Line::from("Episode: ".to_string() + title));
            v.push(Line::from(""));
        }

        if let Some(author) = &details.author {
            v.push(Line::from("Author: ".to_string() + author));
            v.push(Line::from(""));
        }

        if let Some(last_checked) = details.last_checked {
            v.push(Line::from(
                "Last checked: ".to_string() + format!("{last_checked}").as_str(),
            ));
            v.push(Line::from(""));
        }

        if let Some(date) = details.pubdate {
            v.push(Line::from(
                "Published: ".to_string() + format!("{date}").as_str(),
            ));
            v.push(Line::from(""));
        }

        if let Some(pos) = &details.position {
            v.push(Line::from("Elapsed: ".to_string() + pos));
            v.push(Line::from(""));
        }

        if let Some(dur) = &details.duration {
            v.push(Line::from("Duration: ".to_string() + dur));
            v.push(Line::from(""));
        }

        v.push(Line::from("URL: ".to_string() + &details.url));
        v.push(Line::from(""));

        if let Some(exp) = &details.explicit {
            v.push(Line::from(
                "Explicit: ".to_string() + { if *exp { "yes" } else { "no" } },
            ));
            v.push(Line::from(""));
        }

        match &details.description {
            Some(desc) => {
                v.push(Line::from("Description: "));
                for line in desc.lines() {
                    v.push(Line::from(line));
                }
            }
            None => {
                v.push(Line::from("No description."));
            }
        }
        let paragraph = Paragraph::new(v)
            .wrap(Wrap { trim: true })
            .scroll((scroll, 0));
        let block = Block::bordered().title(" Details ").style(colors.normal);
        let inner = block.inner(area);

        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        frame.render_widget(paragraph, inner);
    }
}

fn render_help_line(frame: &mut Frame, area: Rect, keymap: &Keybindings, colors: &AppColors) {
    let actions = vec![
        (UserAction::Quit, "Quit"),
        (UserAction::Back, "Back"),
        (UserAction::Help, "Help"),
        (UserAction::Switch, "Switch"),
        (UserAction::Enter, "Open podcast/Play episode"),
        (UserAction::SyncAll, "Refresh podcasts"),
        (UserAction::SyncGpodder, "Sync"),
        (UserAction::MarkPlayed, "Mark as played"),
        (UserAction::AddFeed, "Add podcast"),
        (UserAction::Enqueue, "Enqueue"),
        (UserAction::Remove, "Remove"),
        (UserAction::UnplayedList, "Show/Hide Unplayed"),
    ];
    let mut cur_length = 0;
    let mut key_strs = Vec::new();
    for (action, action_str) in actions {
        if let Some(keys) = keymap.keys_for_action(action) {
            // longest prefix is 21 chars long
            let key_str = match keys.len() {
                0 => format!(":{action_str}"),
                _ => format!("{}:{}", &keys[0], action_str,),
            };
            if cur_length + key_str.len() > area.width as usize {
                break;
            }
            cur_length += key_str.len() + 3;
            key_strs.push(key_str);
        }
    }
    let line = Line::from(key_strs.join(" | "))
        .bg(colors.normal.1)
        .fg(colors.normal.0);
    frame.render_widget(line, area);
}

fn compute_popup_area(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([Constraint::Percentage(percent_y)]).flex(Flex::Center);
    let horizontal = Layout::horizontal([Constraint::Percentage(percent_x)]).flex(Flex::Center);
    let [area] = vertical.areas(area);
    let [area] = horizontal.areas(area);
    area
}

fn render_menuable_area<T: Menuable>(
    frame: &mut Frame, area: Rect, menu: &mut MenuList<T>, colors: &AppColors, active: bool,
) {
    let block = Block::bordered().title({
        let line = Line::from(format!(" {} ", menu.title));
        if active {
            line.style(colors.highlighted)
        } else {
            line.style(colors.normal)
        }
    });
    let text_width = block.inner(area).width as usize;
    let items: Vec<ListItem> = menu.items.map(
        |x| ListItem::from(x.get_title(text_width)).style(colors.normal),
        false,
    );

    let list = List::new(items)
        .block(block)
        .style(colors.normal)
        .highlight_style({
            if active {
                colors.highlighted
            } else {
                colors.normal
            }
        })
        .highlight_spacing(HighlightSpacing::Always);
    if !list.is_empty() && !menu.sync_state_with_selected() && menu.state.selected().is_none() {
        menu.state.select_first();
        menu.sync_selected_with_state();
    }

    frame.render_stateful_widget(list, area, &mut menu.state);
}

// fn render_podcast_area(
//     frame: &mut Frame, area: Rect, podcasts: &mut MenuList<Podcast>, colors: &AppColors,
//     active: bool,
// ) {
//     let block = Block::bordered().title({
//         let line = Line::from(format!(" {} ", podcasts.title));
//         if active {
//             line.style(colors.highlighted)
//         } else {
//             line.style(colors.normal)
//         }
//     });
//     let text_width = block.inner(area).width as usize;
//     let items: Vec<ListItem> = podcasts.items.map(
//         |x| ListItem::from(x.get_title(text_width)).style(colors.normal),
//         false,
//     );

//     let list = List::new(items)
//         .block(block)
//         .style(colors.normal)
//         .highlight_style({
//             if active {
//                 colors.highlighted
//             } else {
//                 colors.normal
//             }
//         })
//         .highlight_spacing(HighlightSpacing::Always);
//     if !list.is_empty() && podcasts.state.selected().is_none() {
//         podcasts.state.select_first();
//     }

//     frame.render_stateful_widget(list, area, &mut podcasts.state);
// }

// fn render_episode_area(
//     frame: &mut Frame, area: Rect, episodes: &mut MenuList<Episode>, colors: &AppColors,
//     active: bool,
// ) {
//     let block = Block::bordered().title({
//         let line = Line::from(format!(" {} ", episodes.title));
//         if active {
//             line.style(colors.highlighted)
//         } else {
//             line.style(colors.normal)
//         }
//     });
//     let text_width = block.inner(area).width as usize;

//     let items: Vec<ListItem> = episodes.items.map(
//         |x| ListItem::from(Line::from(x.get_title(text_width)).style(colors.normal)),
//         false,
//     );
//     let list = List::new(items)
//         .block(block)
//         .style(colors.normal)
//         .highlight_style({
//             if active {
//                 colors.highlighted
//             } else {
//                 colors.normal
//             }
//         })
//         .highlight_spacing(HighlightSpacing::Always);
//     if !list.is_empty()
//         && !episodes.sync_state_with_selected()
//         && episodes.state.selected().is_none()
//     {
//         episodes.state.select_first();
//         episodes.sync_selected_with_state();
//     }
//     frame.render_stateful_widget(list, area, &mut episodes.state);
// }

#[allow(clippy::cast_precision_loss)]
fn compute_ratio(elapsed: u64, total: u64) -> f64 {
    (elapsed as f64 / total as f64).min(1.0)
}

fn render_play_area(
    frame: &mut Frame, area: Rect, ep: &ShareableRwLock<Option<ShareableRwLock<Episode>>>,
    pod_title: Option<&String>, elapsed: u64, colors: &AppColors,
) {
    let block = Block::bordered()
        .title(Line::from(" Playing "))
        .style(colors.normal);
    let mut ratio = 0.0;
    let mut title = String::new();
    let mut podcast_title = String::new();
    let label = ep.read().unwrap().as_ref().map_or_else(String::new, |ep| {
        let ep = ep.read().unwrap();
        if let Some(total) = ep.duration {
            ratio = compute_ratio(elapsed, total as u64);
        }
        let total_label = format_duration(ep.duration.map(|x| x as u64));
        title.clone_from(&ep.title);
        podcast_title = pod_title.map_or_else(String::new, std::clone::Clone::clone);
        format!("{}/{}", format_duration(Some(elapsed)), total_label)
    });
    let progress = Gauge::default()
        .gauge_style(Style::new().green().on_black())
        .label(label)
        .ratio(ratio);
    let inner_area = block.inner(area);
    let [episode_area, podcast_area, _, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner_area);
    frame.render_widget(block, area);
    frame.render_widget(Line::from(title), episode_area);
    frame.render_widget(Line::from(podcast_title), podcast_area);
    frame.render_widget(progress, bottom);
}
