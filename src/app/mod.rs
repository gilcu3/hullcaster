use anyhow::{Result, anyhow};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use sanitize_filename::{Options, sanitize_with_options};

use crate::gpodder::{EpisodeAction, GpodderMsg};
use crate::{
    config::{Config, MAX_DURATION},
    db::{Database, SyncResult},
    downloads::{self, DownloadError, DownloadMsg, EpData},
    feeds::{self, FeedMsg, PodcastFeed},
    gpodder::{Action, GpodderRequest},
    play_file,
    types::{
        Episode, FilterStatus, FilterType, Filters, LockVec, Menuable, Message, Podcast,
        PodcastNoId, ShareableRwLock, SyncProgress,
    },
    ui::UiMsg,
    utils::{current_time_ms, get_unplayed_episodes, normalize_url, resolve_redirection},
};

mod download_mgr;
mod playback;
mod sync;

/// Enum used for communicating with other tasks.
#[derive(Debug)]
pub enum MainMessage {
    SpawnNotif(String, u64, bool),
    SpawnPersistentNotif(String, bool),
    ClearPersistentNotif,
    PlayCurrent(i64),
    TearDown,
}

/// Main application controller, holding the main application state and
/// mechanisms for communicating with the rest of the app.
pub struct App {
    config: Arc<Config>,
    db: Database,
    semaphore: Arc<Semaphore>,
    podcasts: LockVec<Podcast>,
    queue: LockVec<Episode>,
    unplayed: LockVec<Episode>,
    filters: Filters,
    sync_counter: usize,
    sync_progress: ShareableRwLock<SyncProgress>,
    sync_tracker: Vec<SyncResult>,
    download_tracker: HashSet<i64>,
    last_filter_time_ms: Cell<u128>,
    pub tx_to_ui: mpsc::Sender<MainMessage>,
    pub tx_to_main: mpsc::Sender<Message>,
    pub rx_to_main: mpsc::Receiver<Message>,
    pub tx_to_gpodder: mpsc::Sender<GpodderRequest>,
}

impl App {
    /// Instantiates the main controller (used during app startup), which sets
    /// up the connection to the database, download manager, and UI thread, and
    /// reads the list of podcasts from the database.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<Config>, db_inst: Database, tx_to_main: mpsc::Sender<Message>,
        rx_to_main: mpsc::Receiver<Message>, tx_to_gpodder: mpsc::Sender<GpodderRequest>,
        tx_to_ui: mpsc::Sender<MainMessage>, podcast_list: LockVec<Podcast>,
        queue_items: LockVec<Episode>, unplayed_items: LockVec<Episode>,
        sync_progress: ShareableRwLock<SyncProgress>,
    ) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.simultaneous_downloads));

        Self {
            config,
            db: db_inst,
            semaphore,
            podcasts: podcast_list,
            queue: queue_items,
            unplayed: unplayed_items,
            filters: Filters::default(),
            sync_counter: 0,
            sync_progress,
            sync_tracker: Vec::new(),
            download_tracker: HashSet::new(),
            last_filter_time_ms: 0.into(),
            tx_to_ui,
            tx_to_main,
            rx_to_main,
            tx_to_gpodder,
        }
    }

    /// Initiates the main loop where the controller waits for messages coming
    /// in from the UI and other threads, and processes them.
    #[allow(clippy::too_many_lines)]
    pub fn run(&mut self) {
        if self.config.sync_on_start {
            self.sync(None);
        }

        let sync_interval = self
            .config
            .sync_interval_minutes
            .map(|m| Duration::from_secs(u64::from(m) * 60));
        let mut last_sync = Instant::now();
        let recv_timeout = sync_interval.unwrap_or(Duration::from_secs(3600));

        loop {
            let message = match self.rx_to_main.recv_timeout(recv_timeout) {
                Ok(msg) => msg,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if sync_interval.is_some() && last_sync.elapsed() >= recv_timeout {
                        self.sync(None);
                        last_sync = Instant::now();
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let result = match message {
                Message::Ui(UiMsg::Quit) => break,

                Message::Ui(UiMsg::AddFeed(url)) => {
                    self.add_podcast(url);
                    Ok(())
                }

                Message::Feed(FeedMsg::NewData(pod)) => self.add_or_sync_data(&pod, None),

                Message::Feed(FeedMsg::Error(feed)) => {
                    match feed.title {
                        Some(t) => {
                            self.sync_counter -= 1;
                            self.sync_progress
                                .write()
                                .expect("RwLock write should not fail")
                                .completed += 1;
                            self.update_tracker_notif();
                            if self.sync_counter == 0 {
                                self.pos_sync_counter();
                            }

                            self.notif_to_ui(format!("Error retrieving RSS feed for {t}"), true);
                        }
                        None => self.notif_to_ui(
                            "Error retrieving RSS feed for (no_title)".to_string(),
                            true,
                        ),
                    }
                    Ok(())
                }

                Message::Ui(UiMsg::Sync(pod_id)) => {
                    self.sync(Some(pod_id));
                    Ok(())
                }

                Message::Feed(FeedMsg::SyncData((id, pod))) => {
                    self.add_or_sync_data(&pod, Some(id))
                }

                Message::Ui(UiMsg::SyncAll) => {
                    self.sync(None);
                    last_sync = Instant::now();
                    Ok(())
                }

                Message::Ui(UiMsg::SyncGpodder) => self.gpodder_sync_pre(),

                Message::Ui(UiMsg::Play(pod_id, ep_id, external)) => {
                    self.play_file(pod_id, ep_id, external)
                }

                Message::Ui(UiMsg::MarkPlayed(pod_id, ep_id, played)) => {
                    self.mark_played(pod_id, ep_id, played)
                }

                Message::Ui(UiMsg::MarkAllPlayed(pod_id, played)) => {
                    self.mark_all_played(pod_id, played)
                }

                Message::Ui(UiMsg::UpdatePosition(pod_id, ep_id, position)) => {
                    self.update_position(pod_id, ep_id, position)
                }

                Message::Ui(UiMsg::Download(pod_id, ep_id)) => self.download(pod_id, Some(ep_id)),

                Message::Ui(UiMsg::DownloadAll(pod_id)) => self.download(pod_id, None),

                Message::Dl(msg) => match msg {
                    DownloadMsg::Complete(ep_data) => self.download_complete(ep_data),
                    DownloadMsg::Error(ep, err) => {
                        let msg = match err {
                            DownloadError::Response => {
                                format!("Error sending download request. {}", ep.url)
                            }
                            DownloadError::FileCreate => {
                                format!("Error creating file. {}", ep.title)
                            }
                            DownloadError::FileWrite => {
                                format!("Error downloading episode. {}", ep.title)
                            }
                        };
                        self.notif_to_ui(msg, true);
                        Ok(())
                    }
                },

                Message::Ui(UiMsg::Delete(pod_id, ep_id)) => self.delete_file(pod_id, ep_id),
                Message::Ui(UiMsg::DeleteAll(pod_id)) => self.delete_files(pod_id),

                Message::Ui(UiMsg::RemovePodcast(pod_id, delete_files)) => {
                    self.remove_podcast(pod_id, delete_files)
                }

                Message::Ui(UiMsg::FilterChange(filter_type)) => {
                    let new_filter;
                    let message;
                    match filter_type {
                        // We need to handle these separately because the order
                        // that makes the most sense to me is different: played
                        // goes from all -> neg -> pos. Downloaded goes from all
                        // -> pos -> neg; this is purely based on the idea that
                        // people are most likely to want to specifically find
                        // unplayed episodes, or downloaded episodes
                        FilterType::Played => {
                            match self.filters.played {
                                FilterStatus::All => {
                                    new_filter = FilterStatus::NegativeCases;
                                    message = "Unplayed only";
                                }
                                FilterStatus::NegativeCases => {
                                    new_filter = FilterStatus::PositiveCases;
                                    message = "Played only";
                                }
                                FilterStatus::PositiveCases => {
                                    new_filter = FilterStatus::All;
                                    message = "Played and unplayed";
                                }
                            }
                            self.filters.played = new_filter;
                        }
                        FilterType::Downloaded => {
                            match self.filters.downloaded {
                                FilterStatus::All => {
                                    new_filter = FilterStatus::PositiveCases;
                                    message = "Downloaded only";
                                }
                                FilterStatus::PositiveCases => {
                                    new_filter = FilterStatus::NegativeCases;
                                    message = "Undownloaded only";
                                }
                                FilterStatus::NegativeCases => {
                                    new_filter = FilterStatus::All;
                                    message = "Downloaded and undownloaded";
                                }
                            }
                            self.filters.downloaded = new_filter;
                        }
                    }
                    // TODO: filters update filtered_order in LockVec but the UI
                    // renders using the unfiltered order (map(..., false)). The UI
                    // needs to use map(..., true) and len(true) to actually apply filters.
                    self.notif_to_ui(format!("Filter: {message}"), false);
                    self.update_filters(self.filters, false);
                    Ok(())
                }
                Message::Ui(UiMsg::QueueModified) => self.write_queue(),
                Message::Ui(UiMsg::Noop) => Ok(()),
                Message::Gpodder(GpodderMsg::SubscriptionChanges(
                    subscription_changes,
                    episode_actions,
                    timestamp,
                )) => self.gpodder_sync_pos(subscription_changes, episode_actions, timestamp),
                Message::Gpodder(GpodderMsg::Error(msg)) => {
                    self.notif_to_ui(msg, true);
                    Ok(())
                }
            };
            match result {
                Ok(()) => {}
                Err(err) => log::warn!("Error in app loop: {err}"),
            }
        }
    }

    // sync queue back to database
    pub fn write_queue(&mut self) -> Result<()> {
        let queue = self.queue.borrow_order().clone();
        self.db.set_queue(queue)
    }

    /// Sends the specified notification to the UI, which will display at the
    /// bottom of the screen.
    pub fn notif_to_ui(&self, message: String, error: bool) {
        if self
            .tx_to_ui
            .send(MainMessage::SpawnNotif(
                message,
                crate::config::MESSAGE_TIME,
                error,
            ))
            .is_err()
        {
            log::error!("Failed to send notification to UI: channel closed");
        }
    }

    /// Sends a persistent notification to the UI, which will display at the
    /// bottom of the screen until cleared.
    pub fn persistent_notif_to_ui(&self, message: String, error: bool) {
        if self
            .tx_to_ui
            .send(MainMessage::SpawnPersistentNotif(message, error))
            .is_err()
        {
            log::error!("Failed to send persistent notification to UI: channel closed");
        }
    }

    /// Clears persistent notifications in the UI.
    pub fn clear_persistent_notif(&self) {
        if self
            .tx_to_ui
            .send(MainMessage::ClearPersistentNotif)
            .is_err()
        {
            log::error!("Failed to clear persistent notification: channel closed");
        }
    }

    /// Updates the persistent notification about downloading files.
    pub fn update_tracker_notif(&self) {
        let dl_len = self.download_tracker.len();
        let dl_plural = if dl_len > 1 { "s" } else { "" };

        if dl_len > 0 {
            let notif = format!("Downloading {dl_len} episode{dl_plural}...");
            self.persistent_notif_to_ui(notif, false);
        } else {
            self.clear_persistent_notif();
        }
    }

    fn update_unplayed(&self, full: bool) {
        if full {
            let cur_unplayed = get_unplayed_episodes(&self.podcasts);
            self.unplayed.replace_all_arc(cur_unplayed);
        }
        self.unplayed.sort();
        self.unplayed.reverse();
    }

    // Updates the user-selected filters to show only played/unplayed or
    // downloaded/not downloaded episodes.
    // TODO: this needs to be optimized, I think it is provoking screen issues
    pub fn update_filters(&self, filters: Filters, in_loop: bool) {
        {
            let current_time = current_time_ms();
            if in_loop && current_time - self.last_filter_time_ms.get() < 200 {
                return;
            }
            self.last_filter_time_ms.set(current_time);

            let pod_map = self.podcasts.borrow_map();
            for pod in pod_map.values() {
                let episodes = &pod.read().expect("RwLock read should not fail").episodes;
                let new_filter = episodes.filter_map(|ep| {
                    let ep = ep.read().expect("RwLock read should not fail");
                    let play_filter = match filters.played {
                        FilterStatus::All => false,
                        FilterStatus::PositiveCases => !ep.is_played(),
                        FilterStatus::NegativeCases => ep.is_played(),
                    };
                    let download_filter = match filters.downloaded {
                        FilterStatus::All => false,
                        FilterStatus::PositiveCases => ep.path.is_none(),
                        FilterStatus::NegativeCases => ep.path.is_some(),
                    };
                    if play_filter | download_filter {
                        None
                    } else {
                        Some(ep.id)
                    }
                });
                let mut filtered_order = episodes.borrow_filtered_order();
                *filtered_order = new_filter;
            }
        }
    }

    fn update_queue(&self) {
        let order = self.queue.borrow_order();
        let mut quemap = self.queue.borrow_map();
        let epmap = self.podcasts.get_episodes_map();
        for id in order.iter() {
            if let Some(ep) = epmap.get(id) {
                quemap.insert(*id, ep.clone());
            }
        }
    }
}
