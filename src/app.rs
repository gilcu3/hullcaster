// TODO: remove this exception
// #![allow(clippy::unwrap_used)]

use anyhow::{Result, anyhow};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

use sanitize_filename::{Options, sanitize_with_options};

use crate::gpodder::{EpisodeAction, GpodderMsg};
use crate::{
    config::{Config, MAX_DURATION},
    db::{Database, SyncResult},
    downloads::{self, DownloadMsg, EpData},
    feeds::{self, FeedMsg, PodcastFeed},
    gpodder::{Action, GpodderRequest},
    play_file,
    threadpool::Threadpool,
    types::{
        Episode, FilterStatus, FilterType, Filters, LockVec, Menuable, Message, Podcast,
        PodcastNoId,
    },
    ui::UiMsg,
    utils::{current_time_ms, get_unplayed_episodes, resolve_redirection},
};

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
    threadpool: Threadpool,
    podcasts: LockVec<Podcast>,
    queue: LockVec<Episode>,
    unplayed: LockVec<Episode>,
    filters: Filters,
    sync_counter: usize,
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
    ) -> Self {
        // set up threadpool
        let threadpool = Threadpool::new(config.simultaneous_downloads);

        Self {
            config,
            db: db_inst,
            threadpool,
            podcasts: podcast_list,
            queue: queue_items,
            unplayed: unplayed_items,
            filters: Filters::default(),
            sync_counter: 0,
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

        while let Some(message) = self.rx_to_main.iter().next() {
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

                // downloading can produce any one of these responses
                Message::Dl(msg) => match msg {
                    DownloadMsg::Complete(ep_data) => self.download_complete(ep_data),
                    DownloadMsg::ResponseError(ep) => {
                        self.notif_to_ui(
                            "Error sending download request. ".to_string() + &ep.url,
                            true,
                        );
                        Ok(())
                    }
                    DownloadMsg::FileCreateError(ep) => {
                        self.notif_to_ui("Error creating file. ".to_string() + &ep.title, true);
                        Ok(())
                    }
                    DownloadMsg::FileWriteError(ep) => {
                        self.notif_to_ui(
                            "Error downloading episode. ".to_string() + &ep.title,
                            true,
                        );
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
                    // TODO: "Use filters"
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
        self.tx_to_ui
            .send(MainMessage::SpawnNotif(
                message,
                crate::config::MESSAGE_TIME,
                error,
            ))
            .expect("Thread messaging error");
    }

    /// Sends a persistent notification to the UI, which will display at the
    /// bottom of the screen until cleared.
    pub fn persistent_notif_to_ui(&self, message: String, error: bool) {
        self.tx_to_ui
            .send(MainMessage::SpawnPersistentNotif(message, error))
            .expect("Thread messaging error");
    }

    /// Clears persistent notifications in the UI.
    pub fn clear_persistent_notif(&self) {
        self.tx_to_ui
            .send(MainMessage::ClearPersistentNotif)
            .expect("Thread messaging error");
    }

    /// Updates the persistent notification about syncing podcasts and
    /// downloading files.
    pub fn update_tracker_notif(&self) {
        let sync_len = self.sync_counter;
        let dl_len = self.download_tracker.len();
        let sync_plural = if sync_len > 1 { "s" } else { "" };
        let dl_plural = if dl_len > 1 { "s" } else { "" };

        if sync_len > 0 && dl_len > 0 {
            let notif = format!(
                "Syncing {sync_len} podcast{sync_plural}, downloading {dl_len} episode{dl_plural}..."
            );
            self.persistent_notif_to_ui(notif, false);
        } else if sync_len > 0 {
            let notif = format!("Syncing {sync_len} podcast{sync_plural}...");
            self.persistent_notif_to_ui(notif, false);
        } else if dl_len > 0 {
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

    /// Add a new podcast by fetching the RSS feed data.
    pub fn add_podcast(&self, url: String) {
        let feed = PodcastFeed::new(None, url, None);
        feeds::check_feed(
            feed,
            self.config.max_retries,
            &self.threadpool,
            self.tx_to_main.clone(),
        );
    }

    /// Synchronize RSS feed data for one or more podcasts.
    pub fn sync(&mut self, pod_id: Option<i64>) {
        // We pull out the data we need here first, so we can stop borrowing the
        // podcast list as quickly as possible. Slightly less efficient (two
        // loops instead of one), but then it won't block other tasks that need
        // to access the list.
        let mut pod_data = Vec::new();
        match pod_id {
            // just grab one podcast
            Some(id) => {
                let podcast = self.podcasts.map_single(id, |pod| {
                    PodcastFeed::new(Some(pod.id), pod.url.clone(), Some(pod.title.clone()))
                });

                if let Some(podcast) = podcast {
                    pod_data.push(podcast);
                } else {
                    log::warn!("Podcast with id {id} not found");
                }
            }
            // get all of 'em!
            None => {
                pod_data = self.podcasts.map(
                    |pod| PodcastFeed::new(Some(pod.id), pod.url.clone(), Some(pod.title.clone())),
                    false,
                );
            }
        }
        for feed in pod_data {
            self.sync_counter += 1;
            feeds::check_feed(
                feed,
                self.config.max_retries,
                &self.threadpool,
                self.tx_to_main.clone(),
            );
        }
        self.update_tracker_notif();
    }

    fn gpodder_sync_pre(&self) -> Result<()> {
        if self.config.enable_sync {
            self.tx_to_gpodder
                .send(GpodderRequest::GetSubscriptionChanges)?;
        }
        Ok(())
    }

    fn gpodder_sync_pos(
        &mut self, subscription_changes: (Vec<String>, Vec<String>),
        episode_actions: Vec<EpisodeAction>, timestamp: i64,
    ) -> Result<()> {
        let removed_pods = {
            let (added, deleted) = subscription_changes;
            let pod_map = self
                .podcasts
                .borrow_map()
                .iter()
                .map(|(id, pod)| {
                    let rpod = pod.read().expect("Failed to acquire read lock");
                    (rpod.url.clone(), *id)
                })
                .collect::<HashMap<String, i64>>();
            for url in added {
                let url_resolved = resolve_redirection(&url).unwrap_or(url);
                if !pod_map.contains_key(&url_resolved) {
                    self.add_podcast(url_resolved);
                }
            }
            let mut resolve_deleted = Vec::new();
            for url in deleted {
                let url_resolved = resolve_redirection(&url).unwrap_or(url);
                resolve_deleted.push(url_resolved);
            }
            let mut removed_pods = Vec::new();
            for url in resolve_deleted {
                if let Some(id) = pod_map.get(&url) {
                    removed_pods.push(*id);
                }
            }
            removed_pods
        };

        let pod_data = self
            .podcasts
            .map(
                |pod| {
                    (pod.url.clone(), {
                        (
                            pod.id,
                            pod.episodes
                                .map(|ep| (ep.url.clone(), ep.id), false)
                                .into_iter()
                                .collect::<HashMap<String, i64>>(),
                        )
                    })
                },
                false,
            )
            .into_iter()
            .collect::<HashMap<String, (i64, HashMap<String, i64>)>>();

        let mut last_actions = HashMap::new();

        for a in episode_actions {
            match a.action {
                Action::Play => {
                    log::debug!(
                        "EpisodeAction received - podcast: {} episode: {} position: {:?} total: {:?}",
                        a.podcast,
                        a.episode,
                        a.position,
                        a.total
                    );

                    if let Some(pod) = pod_data.get(&a.podcast)
                        && let Some(ep_id) = pod.1.get(a.episode.as_str())
                        && let Some(position) = a.position
                        && let Some(total) = a.total
                    {
                        last_actions.insert((pod.0, *ep_id), (position, total));
                    }
                }
                Action::Delete | Action::Download | Action::New => {}
            }
        }
        let mut updates = Vec::new();

        for ((pod_id, ep_id), (position, total)) in last_actions {
            updates.push((pod_id, ep_id, position, total));
        }
        let number_updates = updates.len();

        // mutable actions on self
        self.mark_played_db_batch(updates)?;
        for pod_id in removed_pods {
            self.remove_podcast(pod_id, true)?;
        }
        let _ = self.db.set_param("timestamp", &timestamp.to_string());
        self.update_unplayed(true);
        self.update_filters(self.filters, false);
        self.notif_to_ui(
            format!("Gpodder sync finished with {number_updates} updates"),
            false,
        );
        Ok(())
    }

    fn pos_sync_counter(&mut self) {
        // count up total new episodes and updated episodes when sync process is
        // finished
        let mut added = 0;
        let mut updated = 0;
        let mut new_eps = Vec::new();
        for res in &self.sync_tracker {
            added += res.added.len();
            updated += res.updated.len();
            new_eps.extend(res.added.clone());
        }
        if added + updated + new_eps.len() > 0 {
            self.update_filters(self.filters, false);
        }

        self.sync_tracker = Vec::new();
        self.notif_to_ui(
            format!("Sync complete: Added {added}, updated {updated} episodes."),
            false,
        );

        let _ = self.gpodder_sync_pre();
    }

    /// Handles the application logic for adding a new podcast, or synchronizing
    /// data from the RSS feed of an existing podcast. `pod_id` will be None if
    /// a new podcast is being added (i.e., the database has not given it an id
    /// yet).
    // TODO: improve error handling in this function
    pub fn add_or_sync_data(&mut self, pod: &PodcastNoId, pod_id: Option<i64>) -> Result<()> {
        let title = pod.title.clone();
        let db_result;
        let failure = if let Some(id) = pod_id {
            db_result = self.db.update_podcast(id, pod);
            format!("Error synchronizing {title}.")
        } else {
            let title = pod.title.clone();
            let url = pod.url.clone();
            db_result = self.db.insert_podcast(pod);
            if self.config.enable_sync {
                self.tx_to_gpodder.send(GpodderRequest::AddPodcast(url))?;
            }
            format!("Error adding podcast {title} to database.")
        };
        match db_result {
            Ok(result) => {
                if !result.added.is_empty() || !result.updated.is_empty() {
                    // TODO: this is quite inefficient, and currently only necessary
                    // to keep the order
                    {
                        self.podcasts.replace_all(
                            self.db
                                .get_podcasts()
                                .expect("Error retrieving info from database."),
                        );
                    }
                    self.update_unplayed(true);
                    self.update_queue();
                    self.update_filters(self.filters, true);
                }

                if pod_id.is_some() {
                    self.sync_tracker.push(result);
                    self.sync_counter -= 1;
                    self.update_tracker_notif();

                    if self.sync_counter == 0 {
                        self.pos_sync_counter();
                    }
                } else {
                    self.notif_to_ui(
                        format!("Successfully added {} episodes.", result.added.len()),
                        false,
                    );
                }
            }
            Err(_err) => self.notif_to_ui(failure, true),
        }
        Ok(())
    }

    /// Attempts to execute the play command on the given podcast episode.
    pub fn play_file(&self, pod_id: i64, ep_id: i64, external: bool) -> Result<()> {
        let (ep_path, ep_url) = {
            let pod = self
                .podcasts
                .get(pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?;
            let episodes = &pod.read().expect("RwLock read should not fail").episodes;
            let episode_map = episodes.borrow_map();
            let mut episode = episode_map
                .get(&ep_id)
                .ok_or_else(|| anyhow!("Failed to get ep_id: {ep_id}"))?
                .write()
                .expect("RwLock write should not fail");
            if Some(episode.position) == episode.duration {
                episode.position = 0;
            }
            (episode.path.clone(), episode.url.clone())
        };
        if external {
            if play_file::execute(&self.config.play_command, &ep_url).is_err() {
                self.notif_to_ui("Error: Could not stream URL.".to_string(), true);
            } else if self.config.mark_as_played_on_play {
                let _ = self.mark_played(pod_id, ep_id, true);
            }
        } else {
            match ep_path {
                Some(path) => match path.to_str() {
                    Some(_p) => {
                        self.tx_to_ui.send(MainMessage::PlayCurrent(ep_id))?;
                    }
                    None => self.notif_to_ui(
                        format!("Error: Filepath {} is not valid Unicode.", path.display()),
                        true,
                    ),
                },
                None => {
                    self.tx_to_ui.send(MainMessage::PlayCurrent(ep_id))?;
                }
            }
        }
        Ok(())
    }

    fn mark_played_db_batch(&mut self, updates: Vec<(i64, i64, i64, i64)>) -> Result<()> {
        let mut pod_map = HashMap::new();
        for (pod_id, ep_id, position, total) in updates {
            if let std::collections::hash_map::Entry::Vacant(e) = pod_map.entry(pod_id) {
                e.insert(vec![(ep_id, position, total)]);
            } else {
                pod_map
                    .get_mut(&pod_id)
                    .ok_or_else(|| anyhow!("pod_id: {pod_id} does not exist"))?
                    .push((ep_id, position, total));
            }
        }
        let mut changed = false;
        for pod_id in pod_map.keys() {
            let batch = {
                let podcast_map = self.podcasts.borrow_map();
                let episodes = &podcast_map
                    .get(pod_id)
                    .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?
                    .read()
                    .expect("RwLock read should not fail")
                    .episodes;
                let mut episode_map = episodes.borrow_map();
                let mut batch = Vec::new();
                for (ep_id, position, total) in pod_map
                    .get(pod_id)
                    .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?
                {
                    let mut episode = episode_map
                        .get_mut(ep_id)
                        .ok_or_else(|| anyhow!("Failed to get ep_id: {ep_id}"))?
                        .write()
                        .expect("RwLock write should not fail");
                    episode.position = *position;
                    if episode.duration.is_none() {
                        episode.duration = Some(*total);
                    }
                    let played = episode.duration.map_or_else(
                        || episode.played,
                        |duration| (duration - position).abs() <= 1,
                    );
                    if episode.played != played {
                        changed = true;
                        episode.played = played;
                    }
                    batch.push((episode.id, episode.position, episode.duration, played));
                }
                batch
            };
            if self.db.set_played_status_batch(batch).is_err() {
                self.notif_to_ui(
                    "Could not update played status in database.".to_string(),
                    true,
                );
            }
        }
        if changed {
            self.update_filters(self.filters, false);
        }
        Ok(())
    }

    pub fn update_position(&self, pod_id: i64, ep_id: i64, position: i64) -> Result<()> {
        let mut changed = false;
        let (duration, ep_url, pod_url) = {
            let podcast_map = self.podcasts.borrow_map();
            let podcast = podcast_map
                .get(&pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?;
            let podcast = podcast.read().expect("RwLock read should not fail");
            let mut episode_map = podcast.episodes.borrow_map();

            let w_episode = episode_map
                .get_mut(&ep_id)
                .ok_or_else(|| anyhow!("Failed to get ep_id: {ep_id}"))?;
            {
                let mut episode = w_episode.write().expect("RwLock write should not fail");
                if let Some(duration) = episode.duration
                    && !episode.played
                    && position == duration
                {
                    changed = true;
                    episode.played = true;
                }
                episode.position = position;
            }

            let episode = w_episode.read().expect("RwLock read should not fail");

            if episode.played && self.unplayed.contains_key(ep_id) {
                self.unplayed.remove(ep_id);
                changed = true;
            } else if !episode.played && !self.unplayed.contains_key(ep_id) {
                self.unplayed.push_arc(w_episode.clone());
                changed = true;
            }
            self.db
                .set_played_status(ep_id, episode.position, episode.duration, episode.played)?;
            (episode.duration, episode.url.clone(), podcast.url.clone())
        };

        if changed {
            self.update_unplayed(false);
            self.update_filters(self.filters, false);
        }

        if self.config.enable_sync {
            let duration = duration.unwrap_or_else( ||{
                log::warn!("Setting duration to infinity for episode {ep_url}, else cannot mark as played on gpodder");
                MAX_DURATION
            });
            self.tx_to_gpodder.send(GpodderRequest::MarkPlayed(
                pod_url, ep_url, position, duration,
            ))?;
        }
        Ok(())
    }

    /// Given a podcast and episode, it updates the given episode,
    /// sending this info to the database, updating in self.podcasts and syncing
    /// with gpodder.
    /// If played is true, do not modify the current position of episode
    /// if position is near duration, mark episode as played
    /// else just update position
    /// TODO: separate `mark_played` from set position
    pub fn mark_played(&self, pod_id: i64, ep_id: i64, played: bool) -> Result<()> {
        let mut changed = false;
        let (duration, ep_position, ep_url, pod_url) = {
            let podcast_map = self.podcasts.borrow_map();
            let podcast = podcast_map
                .get(&pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?
                .read()
                .expect("RwLock read should not fail");
            let mut episode_map = podcast.episodes.borrow_map();
            let w_episode = episode_map
                .get_mut(&ep_id)
                .ok_or_else(|| anyhow!("Failed to get ep_id: {ep_id}"))?;
            {
                let mut episode = w_episode.write().expect("RwLock write should not fail");
                if episode.played != played {
                    changed = true;
                    episode.played = played;
                    if !played {
                        episode.position = 0;
                    }
                }
            }
            let episode = w_episode.read().expect("RwLock read should not fail");
            if episode.played && self.unplayed.contains_key(ep_id) {
                self.unplayed.remove(ep_id);
                changed = true;
            } else if !episode.played && !self.unplayed.contains_key(ep_id) {
                self.unplayed.push_arc(w_episode.clone());
                changed = true;
            }

            self.db
                .set_played_status(ep_id, episode.position, episode.duration, played)?;
            (
                episode.duration,
                episode.position,
                episode.url.clone(),
                podcast.url.clone(),
            )
        };

        if changed {
            self.update_unplayed(false);
            self.update_filters(self.filters, false);
        }

        if self.config.enable_sync {
            let duration = duration.unwrap_or_else(||{
                log::warn!("Setting duration to infinity for episode {ep_url}, else cannot mark as played on gpodder");
                MAX_DURATION
            });
            let position = { if played { duration } else { ep_position } };
            self.tx_to_gpodder.send(GpodderRequest::MarkPlayed(
                pod_url, ep_url, position, duration,
            ))?;
        }
        Ok(())
    }

    /// Given a podcast, it marks all episodes for that podcast as
    /// played/unplayed, sending this info to the database and updating in
    /// self.podcasts
    pub fn mark_all_played(&mut self, pod_id: i64, played: bool) -> Result<()> {
        let mut changed = false;
        let (sync_list, db_list) = {
            let podcast_map = self.podcasts.borrow_map();
            let podcast = podcast_map
                .get(&pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?
                .read()
                .expect("RwLock read should not fail");
            let podcast_url = podcast.url.clone();

            let mut sync_list = Vec::new();
            let mut db_list = Vec::new();
            let mut episode_map = podcast.episodes.borrow_map();
            for (ep_id, episode) in episode_map.iter_mut() {
                let w_episode = episode;
                {
                    let mut episode = w_episode.write().expect("RwLock write should not fail");
                    if episode.played != played {
                        changed = true;
                        episode.played = played;
                    }
                }
                let episode = w_episode.read().expect("RwLock read should not fail");

                if episode.played && self.unplayed.contains_key(*ep_id) {
                    self.unplayed.remove(*ep_id);
                    changed = true;
                } else if !episode.played && !self.unplayed.contains_key(*ep_id) {
                    self.unplayed.push_arc(w_episode.clone());
                    changed = true;
                }
                if self.config.enable_sync {
                    let duration = episode.duration.unwrap_or_else(|| {
                        log::warn!(
                            "Setting duration to infinity, else cannot mark as played on gpodder"
                        );
                        MAX_DURATION
                    });
                    let position = if played { duration } else { episode.position };
                    sync_list.push((podcast_url.clone(), episode.url.clone(), position, duration));
                }
                db_list.push((*ep_id, episode.position, episode.duration, played));
            }
            (sync_list, db_list)
        };
        if changed {
            self.update_unplayed(false);
            self.update_filters(self.filters, false);
        }

        self.db.set_played_status_batch(db_list)?;

        if self.config.enable_sync {
            let episodes = sync_list
                .iter()
                .map(|(pod, ep, pos, dur)| (pod.clone(), ep.clone(), *pos, *dur))
                .collect();
            self.tx_to_gpodder
                .send(GpodderRequest::MarkPlayedBatch(episodes))?;
        }
        Ok(())
    }

    /// Given a podcast index (and not an episode index), this will send a
    /// vector of jobs to the threadpool to download all episodes in the
    /// podcast. If given an episode index as well, it will download just that
    /// episode.
    pub fn download(&mut self, pod_id: i64, ep_id: Option<i64>) -> Result<()> {
        let pod_title;
        let mut ep_data = Vec::new();
        {
            let borrowed_map = self.podcasts.borrow_map();
            let podcast = borrowed_map
                .get(&pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?;
            let podcast = podcast.read().expect("RwLock read should not fail");
            pod_title = podcast.title.clone();

            // if we are selecting one specific episode, just grab that one;
            // otherwise, loop through them all
            match ep_id {
                Some(ep_id) => {
                    // grab just the relevant data we need
                    let data = podcast
                        .episodes
                        .map_single(ep_id, |ep| {
                            (
                                EpData {
                                    id: ep.id,
                                    pod_id: ep.pod_id,
                                    title: ep.title.clone(),
                                    url: ep.url.clone(),
                                    pubdate: ep.pubdate,
                                    file_path: None,
                                    duration: None,
                                },
                                ep.path.is_none(),
                            )
                        })
                        .ok_or_else(|| anyhow!("ep_id: {ep_id} does not exist"))?;
                    if data.1 {
                        ep_data.push(data.0);
                    }
                }
                None => {
                    // grab just the relevant data we need
                    ep_data = podcast.episodes.filter_map(|ep| {
                        let ep = ep.read().expect("RwLock read should not fail");
                        if ep.path.is_none() {
                            Some(EpData {
                                id: ep.id,
                                pod_id: ep.pod_id,
                                title: ep.title.clone(),
                                url: ep.url.clone(),
                                pubdate: ep.pubdate,
                                file_path: None,
                                duration: ep.duration,
                            })
                        } else {
                            None
                        }
                    });
                }
            }
        }

        // Check against episodes currently being downloaded, so we don't
        // needlessly download them again
        ep_data.retain(|ep| !self.download_tracker.contains(&ep.id));

        if !ep_data.is_empty() {
            // add directory for podcast, create if it does not exist
            let dir_name = sanitize_with_options(
                &pod_title,
                Options {
                    truncate: true,
                    windows: true, // for simplicity, we'll just use Windows-friendly paths for everyone
                    replacement: "",
                },
            );
            match self.create_podcast_dir(dir_name) {
                Ok(path) => {
                    for ep in &ep_data {
                        self.download_tracker.insert(ep.id);
                    }
                    downloads::download_list(
                        ep_data,
                        &path,
                        self.config.max_retries,
                        &self.threadpool,
                        &self.tx_to_main,
                    );
                }
                Err(_) => self.notif_to_ui(format!("Could not create dir: {pod_title}"), true),
            }
            self.update_tracker_notif();
        }
        Ok(())
    }

    /// Handles logic for what to do when a download successfully completes.
    pub fn download_complete(&mut self, ep_data: EpData) -> Result<()> {
        let file_path = ep_data
            .file_path
            .ok_or_else(|| anyhow!("ep_data does not contain a file_path"))?;
        self.db.insert_file(ep_data.id, &file_path)?;
        {
            let borrowed_map = self.podcasts.borrow_map();
            let pod_id = ep_data.pod_id;
            let podcast = borrowed_map
                .get(&pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?;
            let podcast = podcast.read().expect("RwLock read should not fail");
            let mut episode_map = podcast.episodes.borrow_map();
            let ep_id = ep_data.id;
            let mut episode = episode_map
                .get_mut(&ep_id)
                .ok_or_else(|| anyhow!("Failed to get ep_data.id: {ep_id}"))?
                .write()
                .expect("RwLock write should not fail");
            episode.path = Some(file_path);
            if let Some(duration) = ep_data.duration {
                episode.duration = Some(duration);
            }
        }

        self.download_tracker.remove(&ep_data.id);
        self.update_tracker_notif();
        if self.download_tracker.is_empty() {
            self.notif_to_ui("Downloads complete.".to_string(), false);
        }

        self.update_filters(self.filters, false);
        Ok(())
    }

    /// Given a podcast title, creates a download directory for that podcast if
    /// it does not already exist.
    pub fn create_podcast_dir(&self, pod_title: String) -> Result<PathBuf, std::io::Error> {
        let mut download_path = self.config.download_path.clone();
        download_path.push(pod_title);
        match std::fs::create_dir_all(&download_path) {
            Ok(()) => Ok(download_path),
            Err(err) => Err(err),
        }
    }

    /// Deletes a downloaded file for an episode from the user's local system.
    pub fn delete_file(&self, pod_id: i64, ep_id: i64) -> Result<()> {
        let (file_path, title) = {
            let borrowed_map = self.podcasts.borrow_map();
            let podcast = borrowed_map
                .get(&pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?;
            let podcast = podcast.read().expect("RwLock read should not fail");
            let mut episode_map = podcast.episodes.borrow_map();
            let mut episode = episode_map
                .get_mut(&ep_id)
                .ok_or_else(|| anyhow!("Failed to get ep_id: {ep_id}"))?
                .write()
                .expect("RwLock write should not fail");
            let old_path = episode
                .path
                .clone()
                .ok_or_else(|| anyhow!("Episode has no path"))?;
            episode.path = None;
            (old_path, episode.title.clone())
        };

        match fs::remove_file(file_path) {
            Ok(()) => {
                self.db.remove_file(ep_id)?;
                self.update_filters(self.filters, false);
                self.notif_to_ui(format!("Deleted \"{title}\""), false);
            }
            Err(_) => self.notif_to_ui(format!("Error deleting \"{title}\""), true),
        }
        Ok(())
    }

    /// Deletes all downloaded files for a given podcast from the user's local
    /// system.
    pub fn delete_files(&self, pod_id: i64) -> Result<()> {
        let mut eps_id_to_remove = Vec::new();
        let mut eps_path_to_remove = Vec::new();

        {
            let borrowed_map = self.podcasts.borrow_map();
            let episodes = &borrowed_map
                .get(&pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?
                .read()
                .expect("RwLock read should not fail")
                .episodes;
            let mut borrowed_ep_map = episodes.borrow_map();

            for (_, ep) in borrowed_ep_map.iter_mut() {
                let mut ep = ep.write().expect("RwLock write should not fail");
                if ep.path.is_some() {
                    eps_path_to_remove.push(
                        ep.path
                            .clone()
                            .ok_or_else(|| anyhow!("Failed to get episode path"))?,
                    );
                    eps_id_to_remove.push(ep.id);
                    ep.path = None;
                }
            }
        }
        let mut success = true;
        for path in &eps_path_to_remove {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(_) => success = false,
            }
        }

        let res = self.db.remove_files(&eps_id_to_remove);
        if res.is_err() {
            success = false;
        }

        if success {
            if eps_id_to_remove.is_empty() {
                self.notif_to_ui("There are no downloads to delete".to_string(), false);
            } else {
                self.update_filters(self.filters, false);
                self.notif_to_ui("Files successfully deleted.".to_string(), false);
            }
        } else {
            self.notif_to_ui("Error while deleting files".to_string(), true);
        }
        Ok(())
    }

    /// Removes a podcast from the list, optionally deleting local files first
    pub fn remove_podcast(&self, pod_id: i64, delete_files: bool) -> Result<()> {
        if delete_files {
            let _ = self.delete_files(pod_id);
        }

        let pod = self
            .podcasts
            .get(pod_id)
            .ok_or_else(|| anyhow!("pod_id: {pod_id} not found"))?;
        let (pod_id, url) = {
            let pod = pod.read().expect("RwLock read should not fail");
            (pod.id, pod.url.clone())
        };
        self.db.remove_podcast(pod_id)?;
        if self.config.enable_sync {
            self.tx_to_gpodder
                .send(GpodderRequest::RemovePodcast(url))?;
        }
        {
            match self.db.get_podcasts() {
                Ok(podcasts) => {
                    self.podcasts.replace_all(podcasts);
                }
                Err(err) => {
                    log::warn!("Error retrieving info from database: {err}");
                }
            }
        }
        self.update_unplayed(true);
        self.update_queue();
        self.update_filters(self.filters, false);
        Ok(())
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
                let pod = pod.read().expect("RwLock read should not fail");
                let new_filter = pod.episodes.filter_map(|ep| {
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
                let mut filtered_order = pod.episodes.borrow_filtered_order();
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
