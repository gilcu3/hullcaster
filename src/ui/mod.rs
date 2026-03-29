use std::sync::{Arc, RwLock, mpsc};

use chrono::{DateTime, Utc};
use ratatui::widgets::ListState;
use tui_input::Input;

use crate::{
    app::MainMessage,
    config::Config,
    keymap::Keybindings,
    media_control::ControlMessage,
    player::{PlaybackStatus, PlayerMessage},
    types::{Episode, LockVec, Menuable, Message, Podcast, ShareableRwLock},
};

use self::colors::AppColors;
use self::notification::NotificationManager;

pub use types::UiMsg;
pub mod colors;
mod input;
mod navigation;
mod notification;
mod playback;
mod rendering;
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
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
                    *ui.playing.write().expect("RwLock write should not fail") =
                        PlaybackStatus::Ready;

                    // make it a config option
                    let mut clear_episode = true;
                    if let Some(ep) = ui
                        .current_episode
                        .read()
                        .expect("RwLock read should not fail")
                        .as_ref()
                    {
                        let ep_id = ep.read().expect("RwLock read should not fail").id;
                        if let Some(queue_index) = ui.queue.items.get_index(ep_id) {
                            if let Some(next_ep) = ui.next_from_queue(queue_index) {
                                let (pod_id, id) = {
                                    let next_ep =
                                        next_ep.read().expect("RwLock read should not fail");
                                    (next_ep.pod_id, next_ep.id)
                                };

                                let mut res = ui.play_episode(pod_id, id);
                                clear_episode = false;
                                msgs.append(&mut res);
                            }
                            ui.queue.items.remove(ep_id);
                            msgs.push(UiMsg::QueueModified);
                        }
                    }

                    if clear_episode {
                        *ui.current_episode
                            .write()
                            .expect("RwLock write should not fail") = None;
                    }
                    for msg in msgs {
                        tx_to_main
                            .send(Message::Ui(msg))
                            .inspect_err(|err| log::error!("Failed to send Message::Ui: {err}"))
                            .ok();
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
                terminal
                    .draw(|frame| ui.draw(frame))
                    .inspect_err(|err| log::warn!("terminal.draw failed: {err}"))
                    .ok();
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
}
