use std::{
    path::PathBuf,
    sync::{mpsc, Arc, RwLock},
    thread,
    time::Duration,
};

use chrono::{DateTime, Utc};
use notification::render_notification_line;
use ratatui::{
    crossterm::event::{self, Event, KeyCode},
    layout::{Alignment, Constraint, Flex, Layout},
    prelude::Rect,
    style::{Style, Stylize},
    text::Line,
    widgets::{Block, Clear, Gauge, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

pub use types::UiMsg;

pub mod colors;
mod notification;
mod types;

use crate::{
    app::MainMessage,
    config::SEEK_LENGTH,
    player::{Player, PlayerMessage},
    types::{FilterType, LockVec, Menuable, Message},
    utils::{clean_html, format_duration},
};
use crate::{
    config::Config,
    keymap::{Keybindings, UserAction},
    types::{Episode, Podcast},
};

use self::colors::AppColors;
use self::notification::NotificationManager;
use crate::config::{SCROLL_AMOUNT, TICK_RATE};

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
struct PodcastList {
    title: String,
    items: LockVec<Podcast>,
    state: ListState,
}

#[derive(Debug)]
struct EpisodeList {
    title: String,
    items: LockVec<Episode>,
    state: ListState,
}

#[derive(Debug)]
struct CurrentEpisode {
    title: String,
    podcast_title: String,
    path: Option<PathBuf>,
    start_position: u64,
    duration: Option<u64>,
    pod_id: i64,
    ep_ip: i64,
}

#[derive(Debug)]
pub struct Details {
    pub pubdate: Option<DateTime<Utc>>,
    pub duration: Option<String>,
    pub explicit: Option<bool>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub last_checked: Option<DateTime<Utc>>,
    pub episode_title: Option<String>,
    pub podcast_title: Option<String>,
}

pub struct UiState {
    keymap: Keybindings,
    colors: AppColors,
    confirm_quit: bool,
    podcasts: PodcastList,
    episodes: EpisodeList,
    unplayed: EpisodeList,
    queue: EpisodeList,
    active_panel: Panel,
    left_panel: Panel,
    active_popup: Option<Popup>,
    scroll_popup: u16,
    notification: NotificationManager,
    current_episode: Option<CurrentEpisode>,
    current_details: Option<Details>,
    input: Input,
    pub tx_to_player: mpsc::Sender<PlayerMessage>,
    elapsed: Arc<RwLock<u64>>,
    playing: Arc<RwLock<bool>>,
}

impl UiState {
    pub fn spawn(
        config: Arc<Config>, items: LockVec<Podcast>, queue_items: LockVec<Episode>,
        unplayed_items: LockVec<Episode>, rx_from_main: mpsc::Receiver<MainMessage>,
        tx_to_main: mpsc::Sender<Message>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut ui = UiState::new(config, items, queue_items, unplayed_items);
            let mut terminal = ratatui::init();
            let mut message_iter = rx_from_main.try_iter();
            loop {
                ui.notification.check_notifs();
                if ui.playback_finished() {
                    if let Some(msg) = ui.update_position() {
                        let _ = tx_to_main.send(Message::Ui(msg));
                    }
                    ui.current_episode = None;
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

                if let Some(message) = message_iter.next() {
                    match message {
                        MainMessage::SpawnNotif(msg, duration, error) => {
                            ui.notification.timed_notif(msg, duration, error)
                        }
                        MainMessage::SpawnPersistentNotif(msg, error) => {
                            ui.notification.persistent_notif(msg, error)
                        }
                        MainMessage::ClearPersistentNotif => {
                            ui.notification.clear_persistent_notif()
                        }
                        MainMessage::PlayCurrent => {
                            ui.play_current();
                        }
                        MainMessage::TearDown => {
                            break;
                        }
                    }
                }
                let _ = terminal.draw(|frame| ui.draw(frame));
            }
            ratatui::restore();
        })
    }
    pub fn new(
        config: Arc<Config>, items: LockVec<Podcast>, queue_items: LockVec<Episode>,
        unplayed_items: LockVec<Episode>,
    ) -> UiState {
        let active_popup = if items.is_empty() {
            Some(Popup::Welcome)
        } else {
            None
        };

        let (tx_to_player, rx_from_ui) = mpsc::channel();
        let elapsed = Arc::new(RwLock::new(0));
        let playing = Arc::new(RwLock::new(false));
        // should store this somehow
        let _player_thread = Player::spawn(rx_from_ui, elapsed.clone(), playing.clone());

        Self {
            keymap: config.keybindings.clone(),
            colors: config.colors.clone(),
            confirm_quit: config.confirm_quit,
            podcasts: PodcastList {
                title: "Podcasts".to_string(),
                items,
                state: ListState::default().with_selected(Some(0)),
            },
            unplayed: EpisodeList {
                title: "Unplayed".to_string(),
                items: unplayed_items,
                state: ListState::default().with_selected(Some(0)),
            },
            episodes: EpisodeList {
                title: "Episodes".to_string(),
                items: LockVec::new(vec![]),
                state: ListState::default(),
            },
            queue: EpisodeList {
                title: "Queue".to_string(),
                items: queue_items,
                state: ListState::default().with_selected(Some(0)),
            },
            active_panel: Panel::Podcasts,
            left_panel: Panel::Podcasts,
            active_popup,
            scroll_popup: 0,
            notification: NotificationManager::new(),
            current_episode: None,
            current_details: None,
            input: Input::default(),
            tx_to_player,
            elapsed,
            playing,
        }
    }

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
            *self.elapsed.read().unwrap(),
            &self.colors,
        );
        match self.left_panel {
            Panel::Podcasts => render_podcast_area(
                frame,
                select_area,
                &mut self.podcasts,
                &self.colors,
                self.active_panel == Panel::Podcasts,
            ),
            Panel::Episodes => render_episode_area(
                frame,
                select_area,
                &mut self.episodes,
                &self.colors,
                self.active_panel == Panel::Episodes,
            ),
            Panel::Unplayed => render_episode_area(
                frame,
                select_area,
                &mut self.unplayed,
                &self.colors,
                self.active_panel == Panel::Unplayed,
            ),
            Panel::Queue => {}
        }
        render_episode_area(
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
                        &self.current_details,
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

    fn move_cursor(&mut self, action: &UserAction) {
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
                        self.select_panel(Panel::Podcasts);
                    }
                    Panel::Queue => {
                        self.queue.state.select(None);
                        self.select_panel(self.left_panel.clone());
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
        }
    }

    fn get_episode_id(&self) -> Option<i64> {
        match self.active_panel {
            Panel::Podcasts => None,
            Panel::Episodes => {
                if let Some(id) = self.episodes.state.selected() {
                    self.episodes.items.map_single_by_index(id, |x| x.id)
                } else {
                    None
                }
            }
            Panel::Unplayed => {
                if let Some(id) = self.unplayed.state.selected() {
                    self.unplayed.items.map_single_by_index(id, |x| x.id)
                } else {
                    None
                }
            }
            Panel::Queue => {
                if let Some(id) = self.queue.state.selected() {
                    self.queue.items.map_single_by_index(id, |x| x.id)
                } else {
                    None
                }
            }
        }
    }

    fn get_podcast_id(&self) -> Option<i64> {
        match self.active_panel {
            Panel::Podcasts => {
                if let Some(id) = self.podcasts.state.selected() {
                    self.podcasts.items.map_single_by_index(id, |x| x.id)
                } else {
                    None
                }
            }
            Panel::Episodes => {
                if let Some(id) = self.episodes.state.selected() {
                    self.episodes.items.map_single_by_index(id, |x| x.pod_id)
                } else {
                    None
                }
            }
            Panel::Unplayed => {
                if let Some(id) = self.unplayed.state.selected() {
                    self.unplayed.items.map_single_by_index(id, |x| x.pod_id)
                } else {
                    None
                }
            }
            Panel::Queue => {
                if let Some(id) = self.queue.state.selected() {
                    self.queue.items.map_single_by_index(id, |x| x.pod_id)
                } else {
                    None
                }
            }
        }
    }

    fn select_panel(&mut self, panel: Panel) {
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

    /// Waits for user input and, where necessary, provides UiMsgs back to the
    /// main controller.
    ///
    /// Anything UI-related (e.g., scrolling up and down menus) is handled
    /// internally, producing an empty UiMsg. This allows for some greater
    /// degree of abstraction; for example, input to add a new podcast feed
    /// spawns a UI window to capture the feed URL, and only then passes this
    /// data back to the main controller.
    fn getch(&mut self) -> Vec<UiMsg> {
        if event::poll(Duration::from_millis(TICK_RATE)).expect("Can't poll for inputs") {
            if let Event::Key(input) = event::read().expect("Can't read inputs") {
                let action = self.keymap.get_from_input(input).cloned();
                if let Some(popup) = self.active_popup.clone() {
                    if action == Some(UserAction::Back) {
                        self.active_popup = None;
                    } else {
                        match popup {
                            Popup::Welcome | Popup::Details | Popup::Help => match action {
                                Some(a @ UserAction::Down)
                                | Some(a @ UserAction::Up)
                                | Some(a @ UserAction::PageUp)
                                | Some(a @ UserAction::PageDown)
                                | Some(a @ UserAction::GoTop)
                                | Some(a @ UserAction::GoBot) => {
                                    self.move_cursor(&a);
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
                                    let _ = self.tx_to_player.send(PlayerMessage::Quit);
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
                        Some(a @ UserAction::Down)
                        | Some(a @ UserAction::Up)
                        | Some(a @ UserAction::PageUp)
                        | Some(a @ UserAction::PageDown)
                        | Some(a @ UserAction::GoTop)
                        | Some(a @ UserAction::GoBot) => {
                            self.move_cursor(&a);
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

                        Some(a @ UserAction::MoveUp) | Some(a @ UserAction::MoveDown) => {
                            if let Panel::Queue = self.active_panel {
                                if let Some(ui_msg) = self.move_eps(&a) {
                                    return vec![ui_msg];
                                }
                            }
                        }

                        Some(UserAction::AddFeed) => {
                            self.input.reset();
                            self.active_popup = Some(Popup::AddPodcast);
                        }

                        Some(UserAction::Sync) => {
                            if let Panel::Podcasts = self.active_panel {
                                if let Some(pod_id) = self.get_podcast_id() {
                                    return vec![UiMsg::Sync(pod_id)];
                                }
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
                                    self.select_panel(Panel::Episodes);

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
                                if let Some(pod_id) = self.get_podcast_id() {
                                    if let Some(ep_id) = self.get_episode_id() {
                                        let (same, prev, cur_ep_id, cur_pod_id) =
                                            if let Some(cur_ep) = &self.current_episode {
                                                (
                                                    cur_ep.ep_ip == ep_id
                                                        && cur_ep.pod_id == pod_id,
                                                    *self.playing.read().unwrap(),
                                                    cur_ep.ep_ip,
                                                    cur_ep.pod_id,
                                                )
                                            } else {
                                                (false, false, 0, 0)
                                            };
                                        if !same {
                                            if prev {
                                                let position = *self.elapsed.read().unwrap() as i64;
                                                self.construct_current_episode();
                                                return vec![
                                                    UiMsg::UpdatePosition(
                                                        cur_pod_id, cur_ep_id, position,
                                                    ),
                                                    UiMsg::Play(pod_id, ep_id, false),
                                                ];
                                            } else {
                                                self.construct_current_episode();
                                                return vec![UiMsg::Play(pod_id, ep_id, false)];
                                            }
                                        }
                                    }
                                }
                            }
                        },

                        Some(UserAction::PlayExternal) => match self.active_panel {
                            Panel::Queue | Panel::Episodes | Panel::Unplayed => {
                                if let Some(pod_id) = self.get_podcast_id() {
                                    if let Some(ep_id) = self.get_episode_id() {
                                        self.construct_current_episode();
                                        return vec![UiMsg::Play(pod_id, ep_id, true)];
                                    }
                                }
                            }
                            Panel::Podcasts => {}
                        },

                        Some(UserAction::Enqueue) => match self.active_panel {
                            Panel::Episodes | Panel::Unplayed => {
                                if let Some(ep_id) = self.get_episode_id() {
                                    if !self.queue.items.contains_key(ep_id) {
                                        if self.left_panel == Panel::Episodes {
                                            if let Some(ep) = self.episodes.items.get(ep_id) {
                                                self.queue.items.push_arc(ep);
                                                return vec![UiMsg::QueueModified];
                                            }
                                        } else if self.left_panel == Panel::Unplayed {
                                            if let Some(ep) = self.unplayed.items.get(ep_id) {
                                                self.queue.items.push_arc(ep);
                                                return vec![UiMsg::QueueModified];
                                            }
                                        }
                                    }
                                }
                            }
                            Panel::Queue | Panel::Podcasts => {}
                        },
                        Some(UserAction::PlayPause) => {
                            let _ = self.tx_to_player.send(PlayerMessage::PlayPause);
                            if let Some(ui_msg) = self.update_position() {
                                return vec![ui_msg];
                            }
                        }
                        Some(UserAction::MarkPlayed) => match self.active_panel {
                            Panel::Episodes | Panel::Unplayed | Panel::Queue => {
                                if let Some(ui_msg) = self.mark_played() {
                                    return vec![ui_msg];
                                }
                            }
                            _ => {}
                        },
                        Some(UserAction::MarkAllPlayed) => {
                            if let Panel::Episodes = self.active_panel {
                                if let Some(ui_msg) = self.mark_all_played() {
                                    return vec![ui_msg];
                                }
                            }
                        }

                        Some(UserAction::Download) => match self.active_panel {
                            Panel::Episodes | Panel::Unplayed | Panel::Queue => {
                                if let Some(pod_id) = self.get_podcast_id() {
                                    if let Some(ep_id) = self.get_episode_id() {
                                        return vec![UiMsg::Download(pod_id, ep_id)];
                                    }
                                }
                            }
                            _ => {}
                        },
                        Some(UserAction::DownloadAll) => {
                            if let Panel::Podcasts = self.active_panel {
                                if let Some(pod_id) = self.get_podcast_id() {
                                    return vec![UiMsg::DownloadAll(pod_id)];
                                }
                            }
                        }

                        Some(UserAction::Delete) => match self.active_panel {
                            Panel::Episodes | Panel::Queue | Panel::Unplayed => {
                                if let Some(pod_id) = self.get_podcast_id() {
                                    if let Some(ep_id) = self.get_episode_id() {
                                        return vec![UiMsg::Delete(pod_id, ep_id)];
                                    }
                                }
                            }
                            Panel::Podcasts => {}
                        },
                        Some(UserAction::DeleteAll) => {
                            if let Panel::Podcasts = self.active_panel {
                                if let Some(pod_id) = self.get_podcast_id() {
                                    return vec![UiMsg::DeleteAll(pod_id)];
                                }
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
                                        self.select_panel(Panel::Unplayed);
                                    }
                                    Panel::Unplayed => {
                                        self.select_panel(Panel::Podcasts);
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
                                self.select_panel(Panel::Podcasts);
                            }
                        }
                        Some(UserAction::Switch) => match self.active_panel {
                            Panel::Episodes | Panel::Podcasts | Panel::Unplayed => {
                                self.select_panel(Panel::Queue);
                            }
                            Panel::Queue => {
                                self.select_panel(self.left_panel.clone());
                            }
                        },
                        None => (),
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(TICK_RATE));
        vec![UiMsg::Noop]
    }

    fn mark_played(&mut self) -> Option<UiMsg> {
        if let Some(pod_id) = self.get_podcast_id() {
            if let Some(ep_id) = self.get_episode_id() {
                match self.active_panel {
                    Panel::Episodes => {
                        if let Some(played) =
                            self.episodes.items.map_single(ep_id, |ep| ep.is_played())
                        {
                            return Some(UiMsg::MarkPlayed(pod_id, ep_id, !played));
                        }
                    }
                    Panel::Unplayed => {
                        if let Some(played) =
                            self.unplayed.items.map_single(ep_id, |ep| ep.is_played())
                        {
                            return Some(UiMsg::MarkPlayed(pod_id, ep_id, !played));
                        }
                    }
                    Panel::Queue => {
                        if let Some(played) =
                            self.queue.items.map_single(ep_id, |ep| ep.is_played())
                        {
                            return Some(UiMsg::MarkPlayed(pod_id, ep_id, !played));
                        }
                    }
                    _ => {}
                }
            }
        }
        None
    }

    pub fn mark_all_played(&mut self) -> Option<UiMsg> {
        if let Some(pod_id) = self.get_podcast_id() {
            if let Some(played) = self
                .podcasts
                .items
                .map_single(pod_id, |pod| pod.is_played())
            {
                return Some(UiMsg::MarkAllPlayed(pod_id, !played));
            }
        }
        None
    }
    fn remove_podcast(&mut self) -> Option<UiMsg> {
        if let Some(pod_id) = self.get_podcast_id() {
            return Some(UiMsg::RemovePodcast(pod_id, true));
        }
        None
    }
    fn move_eps(&mut self, action: &UserAction) -> Option<UiMsg> {
        if let Some(selected) = self.queue.state.selected() {
            match action {
                UserAction::MoveDown => {
                    if selected + 1 < self.queue.items.len(false) {
                        {
                            let mut order_vec = self.queue.items.borrow_order();
                            order_vec.swap(selected, selected + 1);
                        }
                        self.queue.state.select(Some(selected + 1));
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
                        return Some(UiMsg::QueueModified);
                    }
                }
                _ => (),
            }
        }

        None
    }

    fn construct_details_episode(&mut self) {
        if let Some(ep_id) = self.get_episode_id() {
            let _ep = match self.active_panel {
                Panel::Episodes => self.episodes.items.get(ep_id),
                Panel::Queue => self.queue.items.get(ep_id),
                Panel::Unplayed => self.unplayed.items.get(ep_id),
                Panel::Podcasts => None,
            };
            if let Some(ep) = _ep {
                let ep = ep.read().unwrap();
                let desc = clean_html(&ep.description);
                let podcast_title = {
                    let pod_map = self.podcasts.items.borrow_map();
                    let pod = pod_map.get(&ep.pod_id);
                    pod.map(|pod| pod.read().unwrap().title.clone())
                };
                self.current_details = Some(Details {
                    pubdate: ep.pubdate,
                    duration: Some(format_duration(ep.duration.map(|x| x as u64))),
                    explicit: None,
                    description: Some(desc),
                    author: None,
                    last_checked: None,
                    episode_title: Some(ep.title.clone()),
                    podcast_title,
                });
            }
        }
    }

    fn construct_details_podcast(&mut self) {
        if let Some(pod_id) = self.get_podcast_id() {
            if let Some(pod) = self.podcasts.items.get(pod_id) {
                let pod = pod.read().unwrap();
                let desc = pod.description.clone().map(|desc| clean_html(&desc));
                self.current_details = Some(Details {
                    pubdate: None,
                    duration: None,
                    explicit: pod.explicit,
                    description: desc,
                    author: pod.author.clone(),
                    last_checked: Some(pod.last_checked),
                    episode_title: None,
                    podcast_title: Some(pod.title.clone()),
                });
            }
        }
    }
    fn construct_current_episode(&mut self) {
        if let Some(ep_id) = self.get_episode_id() {
            let _ep = match self.active_panel {
                Panel::Episodes => self.episodes.items.get(ep_id),
                Panel::Queue => self.queue.items.get(ep_id),
                Panel::Unplayed => self.unplayed.items.get(ep_id),
                Panel::Podcasts => None,
            };
            if let Some(ep) = _ep {
                let ep = ep.read().unwrap();
                let podcast_title = {
                    let pod_map = self.podcasts.items.borrow_map();
                    let pod = pod_map.get(&ep.pod_id);
                    pod.map(|pod| pod.read().unwrap().title.clone()).unwrap()
                };
                self.current_episode = Some(CurrentEpisode {
                    title: ep.title.clone(),
                    podcast_title,
                    path: ep.path.clone(),
                    start_position: ep.position as u64,
                    duration: ep.duration.map(|x| x as u64),
                    pod_id: ep.pod_id,
                    ep_ip: ep.id,
                })
            }
        }
    }

    fn play_current(&mut self) -> Option<()> {
        if let Some(ep) = &self.current_episode {
            if let Some(path) = &ep.path {
                self.tx_to_player
                    .send(PlayerMessage::PlayFile(
                        path.clone(),
                        ep.start_position,
                        ep.duration.unwrap(),
                    ))
                    .ok()?;
            }
        }
        None
    }
    fn playback_finished(&self) -> bool {
        if let Some(cur_ep) = &self.current_episode {
            return *self.elapsed.read().unwrap() == cur_ep.duration.unwrap();
        }
        false
    }

    fn update_position(&self) -> Option<UiMsg> {
        if let Some(cur_ep) = &self.current_episode {
            let position = *self.elapsed.read().unwrap() as i64;
            return Some(UiMsg::UpdatePosition(cur_ep.pod_id, cur_ep.ep_ip, position));
        }
        None
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
    frame.set_cursor_position((area.x + x as u16, input_area.y + 1))
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
                        0 => format!("{:>28} <missing>", action_str),
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
        Line::from(format!("Press {} to close this window.", back_key)).alignment(Alignment::Right);

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

    let line1 = format!("Your podcast list is currently empty. Press \"{}\" to add a new podcast feed, \"{}\" to quit, or see all available commands by typing \"{}\" to get help.", key_strs[0], key_strs[1], key_strs[2]);
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
    frame: &mut Frame, area: Rect, details: &Option<Details>, scroll: u16, colors: &AppColors,
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
                "Last checked: ".to_string() + format!("{}", last_checked).as_str(),
            ));
            v.push(Line::from(""));
        }

        if let Some(date) = details.pubdate {
            v.push(Line::from(
                "Published: ".to_string() + format!("{}", date).as_str(),
            ));
            v.push(Line::from(""));
        }

        if let Some(dur) = &details.duration {
            v.push(Line::from("Duration: ".to_string() + dur));
            v.push(Line::from(""));
        }

        if let Some(exp) = &details.explicit {
            v.push(Line::from(
                "Explicit: ".to_string() + {
                    if *exp {
                        "yes"
                    } else {
                        "no"
                    }
                },
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
    let mut cur_length = -3;
    let mut key_strs = Vec::new();
    for (action, action_str) in actions {
        if let Some(keys) = keymap.keys_for_action(action) {
            // longest prefix is 21 chars long
            let key_str = match keys.len() {
                0 => format!(":{}", action_str),
                _ => format!("{}:{}", &keys[0], action_str,),
            };
            if cur_length + key_str.len() as i16 + 3 > area.width as i16 {
                break;
            }
            cur_length += key_str.len() as i16 + 3;
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

fn render_podcast_area(
    frame: &mut Frame, area: Rect, podcasts: &mut PodcastList, colors: &AppColors, active: bool,
) {
    let block = Block::bordered().title({
        let line = Line::from(format!(" {} ", podcasts.title));
        if active {
            line.style(colors.highlighted)
        } else {
            line.style(colors.normal)
        }
    });
    let text_width = block.inner(area).width as usize;
    let items: Vec<ListItem> = podcasts.items.map(
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
    if !list.is_empty() && podcasts.state.selected().is_none() {
        podcasts.state.select_first();
    }

    frame.render_stateful_widget(list, area, &mut podcasts.state);
}

fn render_episode_area(
    frame: &mut Frame, area: Rect, episodes: &mut EpisodeList, colors: &AppColors, active: bool,
) {
    let block = Block::bordered().title({
        let line = Line::from(format!(" {} ", episodes.title));
        if active {
            line.style(colors.highlighted)
        } else {
            line.style(colors.normal)
        }
    });
    let text_width = block.inner(area).width as usize;

    let items: Vec<ListItem> = episodes.items.map(
        |x| ListItem::from(Line::from(x.get_title(text_width)).style(colors.normal)),
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
    if !list.is_empty() && episodes.state.selected().is_none() {
        episodes.state.select_first();
    }
    frame.render_stateful_widget(list, area, &mut episodes.state);
}

fn render_play_area(
    frame: &mut Frame, area: Rect, ep: &Option<CurrentEpisode>, elapsed: u64, colors: &AppColors,
) {
    let block = Block::bordered()
        .title(Line::from(" Playing "))
        .style(colors.normal);
    let mut ratio = 0.0;
    let mut title = "".to_string();
    let mut podcast_title = "".to_string();
    let mut label = "".to_string();

    if let Some(ep) = ep {
        if let Some(total) = ep.duration {
            ratio = (elapsed as f64 / total as f64).min(1.0);
        }
        let total_label = format_duration(ep.duration);
        title = ep.title.clone();
        podcast_title = ep.podcast_title.clone();
        label = format!("{}/{}", format_duration(Some(elapsed)), total_label);
    }
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
