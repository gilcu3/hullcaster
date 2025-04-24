use anyhow::Result;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};

use sanitize_filename::{sanitize_with_options, Options};

use crate::config::Config;
use crate::db::{Database, SyncResult};
use crate::downloads::{self, DownloadMsg, EpData};
use crate::feeds::{self, FeedMsg, PodcastFeed};
use crate::gpodder::{Action, GpodderController};
use crate::play_file;
use crate::threadpool::Threadpool;
use crate::types::*;
use crate::ui::{UiMsg, UiState};
use crate::utils::{
    audio_duration, current_time_ms, evaluate_in_shell, get_unplayed_episodes, resolve_redirection,
};

/// Enum used for communicating with other threads.
#[derive(Debug)]
pub enum MainMessage {
    SpawnNotif(String, u64, bool),
    SpawnPersistentNotif(String, bool),
    ClearPersistentNotif,
    PlayCurrent,
    TearDown,
}

/// Main application controller, holding all of the main application state and
/// mechanisms for communicating with the rest of the app.
pub struct App {
    config: Arc<Config>,
    db: Database,
    threadpool: Threadpool,
    sync_agent: Option<GpodderController>,
    podcasts: LockVec<Podcast>,
    queue: LockVec<Episode>,
    unplayed: LockVec<Episode>,
    filters: Filters,
    sync_counter: usize,
    sync_tracker: Vec<SyncResult>,
    download_tracker: HashSet<i64>,
    last_filter_time_ms: Cell<u128>,
    pub ui_thread: Option<std::thread::JoinHandle<()>>,
    pub tx_to_ui: mpsc::Sender<MainMessage>,
    pub tx_to_main: mpsc::Sender<Message>,
    pub rx_to_main: mpsc::Receiver<Message>,
}

impl App {
    /// Instantiates the main controller (used during app startup), which sets
    /// up the connection to the database, download manager, and UI thread, and
    /// reads the list of podcasts from the database.
    pub fn new(config: Arc<Config>, db_path: &Path) -> Result<App> {
        // create transmitters and receivers for passing messages between
        // threads
        let (tx_to_ui, rx_from_main) = mpsc::channel();
        let (tx_to_main, rx_to_main) = mpsc::channel::<Message>();

        // get connection to the database
        let db_inst = Database::connect(db_path)?;

        // set up threadpool
        let threadpool = Threadpool::new(config.simultaneous_downloads);

        // create vector of podcasts, where references are checked at runtime;
        // this is necessary because we want main.rs to hold the "ground truth"
        // list of podcasts, and it must be mutable, but UI needs to check this
        // list and update the screen when necessary
        let podcast_list = LockVec::new(db_inst.get_podcasts()?);

        let sync_agent = if config.enable_sync {
            let timestamp = db_inst
                .get_param("timestamp")
                .and_then(|s| s.parse::<i64>().ok());
            let device_id = db_inst.get_param("device_id").unwrap_or({
                let res = evaluate_in_shell("hostname")
                    .expect("Failed to get hostname")
                    .trim()
                    .to_string();
                db_inst.set_param("device_id", &res)?;
                res
            });
            GpodderController::new(config.clone(), timestamp, device_id)
        } else {
            None
        };

        let stored_queue = db_inst.get_queue()?;
        let queue_items = LockVec::new_arc({
            let all_eps_map = podcast_list.get_episodes_map().unwrap();
            let res = stored_queue
                .iter()
                .map(|v| all_eps_map.get(v).unwrap().clone())
                .collect();
            res
        });

        let unplayed_items = LockVec::new_arc(get_unplayed_episodes(&podcast_list));
        unplayed_items.sort();
        unplayed_items.reverse();

        // set up UI in new thread
        let tx_ui_to_main = mpsc::Sender::clone(&tx_to_main);

        let ui_thread = UiState::spawn(
            config.clone(),
            podcast_list.clone(),
            queue_items.clone(),
            unplayed_items.clone(),
            rx_from_main,
            tx_ui_to_main,
        );

        Ok(App {
            config,
            db: db_inst,
            threadpool,
            sync_agent,
            podcasts: podcast_list,
            queue: queue_items,
            unplayed: unplayed_items,
            filters: Filters::default(),
            ui_thread: Some(ui_thread),
            sync_counter: 0,
            sync_tracker: Vec::new(),
            download_tracker: HashSet::new(),
            last_filter_time_ms: 0.into(),
            tx_to_ui,
            tx_to_main,
            rx_to_main,
        })
    }

    /// Initiates the main loop where the controller waits for messages coming
    /// in from the UI and other threads, and processes them.
    pub fn run(&mut self) -> Result<()> {
        if self.config.sync_on_start {
            self.sync(None);
        }

        while let Some(message) = self.rx_to_main.iter().next() {
            match message {
                Message::Ui(UiMsg::Quit) => break,

                Message::Ui(UiMsg::AddFeed(url)) => self.add_podcast(url),

                Message::Feed(FeedMsg::NewData(pod)) => self.add_or_sync_data(pod, None),

                Message::Feed(FeedMsg::Error(feed)) => match feed.title {
                    Some(t) => {
                        self.sync_counter -= 1;
                        self.update_tracker_notif();
                        if self.sync_counter == 0 {
                            self.pos_sync_counter();
                        }

                        self.notif_to_ui(format!("Error retrieving RSS feed for {t}"), true);
                    }
                    None => self
                        .notif_to_ui("Error retrieving RSS feed for (no_title)".to_string(), true),
                },

                Message::Ui(UiMsg::Sync(pod_id)) => self.sync(Some(pod_id)),

                Message::Feed(FeedMsg::SyncData((id, pod))) => self.add_or_sync_data(pod, Some(id)),

                Message::Ui(UiMsg::SyncAll) => {
                    self.sync(None);
                }

                Message::Ui(UiMsg::SyncGpodder) => {
                    if self.config.enable_sync {
                        self.gpodder_sync();
                    }
                }

                Message::Ui(UiMsg::Play(pod_id, ep_id)) => self.play_file(pod_id, ep_id),

                Message::Ui(UiMsg::MarkPlayed(pod_id, ep_id, played)) => {
                    self.mark_played(pod_id, ep_id, played);
                }

                Message::Ui(UiMsg::MarkAllPlayed(pod_id, played)) => {
                    self.mark_all_played(pod_id, played);
                }

                Message::Ui(UiMsg::Download(pod_id, ep_id)) => self.download(pod_id, Some(ep_id)),

                Message::Ui(UiMsg::DownloadAll(pod_id)) => self.download(pod_id, None),

                // downloading can produce any one of these responses
                Message::Dl(DownloadMsg::Complete(ep_data)) => {
                    self.download_complete(ep_data);
                }
                Message::Dl(DownloadMsg::ResponseError(ep)) => self.notif_to_ui(
                    "Error sending download request. ".to_string() + &ep.url,
                    true,
                ),
                Message::Dl(DownloadMsg::FileCreateError(ep)) => {
                    self.notif_to_ui("Error creating file. ".to_string() + &ep.title, true)
                }
                Message::Dl(DownloadMsg::FileWriteError(ep)) => {
                    self.notif_to_ui("Error downloading episode. ".to_string() + &ep.title, true)
                }

                Message::Ui(UiMsg::Delete(pod_id, ep_id)) => {
                    self.delete_file(pod_id, ep_id);
                }
                Message::Ui(UiMsg::DeleteAll(pod_id)) => self.delete_files(pod_id),

                Message::Ui(UiMsg::RemovePodcast(pod_id, delete_files)) => {
                    self.remove_podcast(pod_id, delete_files)
                }

                Message::Ui(UiMsg::FilterChange(filter_type)) => {
                    let new_filter;
                    let message;
                    match filter_type {
                        // we need to handle these separately because the order
                        // that makes the most sense to me is different: played
                        // goes from all -> neg -> pos; downloaded goes from all
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
                }
                Message::Ui(UiMsg::QueueModified) => {
                    self.write_queue();
                }
                Message::Ui(UiMsg::Noop) => (),
            }
        }
        Ok(())
    }

    // sync queue back to database
    pub fn write_queue(&mut self) -> Option<()> {
        let queue = self.queue.borrow_order().clone();
        self.db.set_queue(queue).ok()
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
                "Syncing {sync_len} podcast{sync_plural}, downloading {dl_len} episode{dl_plural}...");
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
            Some(id) => pod_data.push(
                self.podcasts
                    .map_single(id, |pod| {
                        PodcastFeed::new(Some(pod.id), pod.url.clone(), Some(pod.title.clone()))
                    })
                    .unwrap(),
            ),
            // get all of 'em!
            None => {
                pod_data = self.podcasts.map(
                    |pod| PodcastFeed::new(Some(pod.id), pod.url.clone(), Some(pod.title.clone())),
                    false,
                )
            }
        }
        for feed in pod_data.into_iter() {
            self.sync_counter += 1;
            feeds::check_feed(
                feed,
                self.config.max_retries,
                &self.threadpool,
                self.tx_to_main.clone(),
            )
        }
        self.update_tracker_notif();
    }

    fn gpodder_sync(&mut self) {
        let sync_agent = self.sync_agent.as_ref().unwrap();
        let subcription_changes = sync_agent.get_subscription_changes();

        let removed_pods = if let Some((added, deleted)) = subcription_changes {
            let pod_map = self
                .podcasts
                .borrow_map()
                .iter()
                .map(|(id, pod)| {
                    let rpod = pod.read().unwrap();
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
        } else {
            Vec::new()
        };

        let actions = sync_agent.get_episode_action_changes();

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

        for a in actions.unwrap() {
            match a.action {
                Action::play => {
                    log::info!(
                        "EpisodeAction received - podcast: {} episode: {} position: {} total: {}",
                        a.podcast,
                        a.episode,
                        a.position.unwrap(),
                        a.total.unwrap()
                    );

                    let pod_id_opt = pod_data.get(&a.podcast);
                    if pod_id_opt.is_none() {
                        continue;
                    }
                    let pod_id = pod_id_opt.unwrap().0;
                    let ep_id_opt = pod_id_opt.unwrap().1.get(a.episode.as_str());
                    if ep_id_opt.is_none() {
                        continue;
                    }
                    let ep_id = *ep_id_opt.unwrap();
                    last_actions.insert((pod_id, ep_id), (a.position.unwrap(), a.total.unwrap()));
                }
                Action::download => {}
                Action::delete => {}
                Action::new => {}
            }
        }
        let mut updates = Vec::new();

        for ((pod_id, ep_id), (position, total)) in last_actions {
            let played = (total - position).abs() <= 10;
            updates.push((pod_id, ep_id, played));
        }
        let number_updates = updates.len();
        let timestamp = &sync_agent.get_timestamp().to_string();

        // mutable actions on self
        self.mark_played_db_batch(updates);
        for pod_id in removed_pods {
            self.remove_podcast(pod_id, true);
        }
        let _ = self.db.set_param("timestamp", timestamp);
        self.update_unplayed(true);
        self.update_filters(self.filters, false);
        self.notif_to_ui(
            format!("Gpodder sync finished with {} updates", number_updates).to_string(),
            false,
        );
    }

    fn pos_sync_counter(&mut self) {
        // count up total new episodes and updated episodes when sync process is
        // finished
        let mut added = 0;
        let mut updated = 0;
        let mut new_eps = Vec::new();
        for res in self.sync_tracker.iter() {
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

        if self.config.enable_sync {
            self.gpodder_sync();
        }
    }

    /// Handles the application logic for adding a new podcast, or synchronizing
    /// data from the RSS feed of an existing podcast. `pod_id` will be None if
    /// a new podcast is being added (i.e., the database has not given it an id
    /// yet).
    pub fn add_or_sync_data(&mut self, pod: PodcastNoId, pod_id: Option<i64>) {
        let title = pod.title.clone();
        let db_result;
        let failure;

        if let Some(id) = pod_id {
            db_result = self.db.update_podcast(id, pod);
            failure = format!("Error synchronizing {title}.");
        } else {
            let title = pod.title.clone();
            let url = pod.url.clone();
            db_result = self.db.insert_podcast(pod);
            if self.config.enable_sync && self.sync_agent.is_some() {
                self.sync_agent.as_ref().unwrap().add_podcast(url);
            }
            failure = format!("Error adding podcast {title} to database.");
        }
        match db_result {
            Ok(result) => {
                if !result.added.is_empty() || !result.updated.is_empty() {
                    {
                        self.podcasts.replace_all(
                            self.db
                                .get_podcasts()
                                .expect("Error retrieving info from database."),
                        );
                    }
                    self.update_unplayed(true);
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
    }

    /// Attempts to execute the play command on the given podcast episode.
    pub fn play_file(&self, pod_id: i64, ep_id: i64) {
        if self.config.mark_as_played_on_play {
            self.mark_played(pod_id, ep_id, true);
        }
        let (ep_path, ep_url) = {
            let pod = self.podcasts.get(pod_id).unwrap();
            let pod = pod.read().unwrap();
            let episode_map = pod.episodes.borrow_map();
            let episode = episode_map.get(&ep_id).unwrap().read().unwrap();
            (episode.path.clone(), episode.url.clone())
        };

        match ep_path {
            // if there is a local file, try to play that
            Some(path) => match path.to_str() {
                Some(_p) => {
                    // if play_file::execute(&self.config.play_command, p).is_err() {
                    //     self.notif_to_ui(
                    //         "Error: Could not play file. Check configuration.".to_string(),
                    //         true,
                    //     );
                    // }
                    self.tx_to_ui.send(MainMessage::PlayCurrent).unwrap();
                }
                None => self.notif_to_ui("Error: Filepath is not valid Unicode.".to_string(), true),
            },
            // otherwise, try to stream the URL
            None => {
                if play_file::execute(&self.config.play_command, &ep_url).is_err() {
                    self.notif_to_ui("Error: Could not stream URL.".to_string(), true);
                }
            }
        }
    }

    fn mark_played_db_batch(&mut self, updates: Vec<(i64, i64, bool)>) -> Option<()> {
        let mut pod_map = HashMap::new();
        for (pod_id, ep_id, played) in updates {
            if let std::collections::hash_map::Entry::Vacant(e) = pod_map.entry(pod_id) {
                e.insert(vec![(ep_id, played)]);
            } else {
                pod_map.get_mut(&pod_id).unwrap().push((ep_id, played));
            }
        }
        let mut changed = false;
        for pod_id in pod_map.keys() {
            let batch = {
                let podcast_map = self.podcasts.borrow_map();
                let podcast = podcast_map.get(pod_id)?.read().unwrap();
                let mut episode_map = podcast.episodes.borrow_map();
                let mut batch = Vec::new();
                for (ep_id, played) in pod_map.get(pod_id).unwrap() {
                    let mut episode = episode_map.get_mut(ep_id).unwrap().write().unwrap();
                    if episode.played != *played {
                        changed = true;
                        episode.played = *played;
                    }
                    batch.push((episode.id, *played));
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
        Some(())
    }

    /// Given a podcast and episode, it marks the given episode as
    /// played/unplayed, sending this info to the database and updating in
    /// self.podcasts
    pub fn mark_played(&self, pod_id: i64, ep_id: i64, played: bool) -> Option<()> {
        let mut changed = false;
        let (mut duration, ep_url, pod_url) = {
            let podcast_map = self.podcasts.borrow_map();
            let podcast = podcast_map.get(&pod_id)?.read().unwrap();
            let mut episode_map = podcast.episodes.borrow_map();
            let mut episode = episode_map.get_mut(&ep_id)?.write().unwrap();
            if episode.played != played {
                changed = true;
                episode.played = played;
            }
            if episode.played && self.unplayed.contains_key(ep_id) {
                self.unplayed.remove(ep_id);
                changed = true;
            } else if !episode.played && !self.unplayed.contains_key(ep_id) {
                self.unplayed.push(episode.clone());
                changed = true;
            }
            self.db.set_played_status(ep_id, played).ok()?;
            (episode.duration, episode.url.clone(), podcast.url.clone())
        };

        if changed {
            self.update_unplayed(false);
            self.update_filters(self.filters, false);
        }

        if self.config.enable_sync {
            if duration.is_none() {
                duration = audio_duration(&ep_url);
                if duration.is_none() {
                    self.notif_to_ui(
                        "Could not mark episode as played in gpodder: missing duration."
                            .to_string(),
                        true,
                    );
                    return None;
                }
            }
            self.sync_agent
                .as_ref()
                .unwrap()
                .mark_played(&pod_url, &ep_url, duration, played);
        }
        Some(())
    }

    /// Given a podcast, it marks all episodes for that podcast as
    /// played/unplayed, sending this info to the database and updating in
    /// self.podcasts
    pub fn mark_all_played(&mut self, pod_id: i64, played: bool) -> Option<()> {
        let mut changed = false;
        let (sync_list, db_list) = {
            let podcast_map = self.podcasts.borrow_map();
            let podcast = podcast_map.get(&pod_id)?.read().unwrap();
            let podcast_url = podcast.url.clone();

            let mut sync_list = Vec::new();
            let mut db_list = Vec::new();
            let mut episode_map = podcast.episodes.borrow_map();
            for (ep_id, episode) in episode_map.iter_mut() {
                db_list.push((*ep_id, played));
                let mut episode = episode.write().unwrap();
                if episode.played != played {
                    changed = true;
                    episode.played = played;
                }
                if episode.played && self.unplayed.contains_key(*ep_id) {
                    self.unplayed.remove(*ep_id);
                    changed = true;
                } else if !episode.played && !self.unplayed.contains_key(*ep_id) {
                    self.unplayed.push(episode.clone());
                    changed = true;
                }
                if self.config.enable_sync {
                    sync_list.push((
                        podcast_url.to_owned(),
                        episode.url.to_owned(),
                        episode.duration,
                        played,
                    ));
                }
            }
            (sync_list, db_list)
        };
        if changed {
            self.update_unplayed(false);
            self.update_filters(self.filters, false);
        }

        self.db.set_played_status_batch(db_list).ok()?;

        if self.config.enable_sync {
            self.sync_agent.as_ref().unwrap().mark_played_batch(
                sync_list
                    .iter()
                    .map(|(pod, ep, dur, p)| (pod.as_str(), ep.as_str(), *dur, *p))
                    .collect(),
            );
        }
        Some(())
    }

    /// Given a podcast index (and not an episode index), this will send a
    /// vector of jobs to the threadpool to download all episodes in the
    /// podcast. If given an episode index as well, it will download just that
    /// episode.
    pub fn download(&mut self, pod_id: i64, ep_id: Option<i64>) {
        let pod_title;
        let mut ep_data = Vec::new();
        {
            let borrowed_map = self.podcasts.borrow_map();
            let podcast = borrowed_map.get(&pod_id).unwrap();
            let podcast = podcast.read().unwrap();
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
                                },
                                ep.path.is_none(),
                            )
                        })
                        .unwrap();
                    if data.1 {
                        ep_data.push(data.0);
                    }
                }
                None => {
                    // grab just the relevant data we need
                    ep_data = podcast.episodes.filter_map(|ep| {
                        let ep = ep.read().unwrap();
                        if ep.path.is_none() {
                            Some(EpData {
                                id: ep.id,
                                pod_id: ep.pod_id,
                                title: ep.title.clone(),
                                url: ep.url.clone(),
                                pubdate: ep.pubdate,
                                file_path: None,
                            })
                        } else {
                            None
                        }
                    });
                }
            }
        }

        // check against episodes currently being downloaded -- so we don't
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
                    for ep in ep_data.iter() {
                        self.download_tracker.insert(ep.id);
                    }
                    downloads::download_list(
                        ep_data,
                        &path,
                        self.config.max_retries,
                        &self.threadpool,
                        self.tx_to_main.clone(),
                    );
                }
                Err(_) => self.notif_to_ui(format!("Could not create dir: {pod_title}"), true),
            }
            self.update_tracker_notif();
        }
    }

    /// Handles logic for what to do when a download successfully completes.
    pub fn download_complete(&mut self, ep_data: EpData) -> Option<()> {
        let file_path = ep_data.file_path.unwrap();
        let res = self.db.insert_file(ep_data.id, &file_path);
        if res.is_err() {
            self.notif_to_ui(
                format!(
                    "Could not add episode file to database: {}",
                    file_path.to_string_lossy()
                ),
                true,
            );
            return None;
        }
        {
            let borrowed_map = self.podcasts.borrow_map();
            let podcast = borrowed_map.get(&ep_data.pod_id).unwrap();
            let podcast = podcast.read().unwrap();
            let mut episode_map = podcast.episodes.borrow_map();
            let mut episode = episode_map.get_mut(&ep_data.id)?.write().unwrap();
            episode.path = Some(file_path.clone());
        }

        self.download_tracker.remove(&ep_data.id);
        self.update_tracker_notif();
        if self.download_tracker.is_empty() {
            self.notif_to_ui("Downloads complete.".to_string(), false);
        }

        self.update_filters(self.filters, false);
        Some(())
    }

    /// Given a podcast title, creates a download directory for that podcast if
    /// it does not already exist.
    pub fn create_podcast_dir(&self, pod_title: String) -> Result<PathBuf, std::io::Error> {
        let mut download_path = self.config.download_path.clone();
        download_path.push(pod_title);
        match std::fs::create_dir_all(&download_path) {
            Ok(_) => Ok(download_path),
            Err(err) => Err(err),
        }
    }

    /// Deletes a downloaded file for an episode from the user's local system.
    pub fn delete_file(&self, pod_id: i64, ep_id: i64) -> Option<()> {
        let (file_path, title) = {
            let borrowed_map = self.podcasts.borrow_map();
            let podcast = borrowed_map.get(&pod_id).unwrap();
            let podcast = podcast.read().unwrap();
            let mut episode_map = podcast.episodes.borrow_map();
            let mut episode = episode_map.get_mut(&ep_id)?.write().unwrap();
            let _path = episode.path.clone()?;
            episode.path = None;
            (_path, episode.title.clone())
        };

        match fs::remove_file(file_path) {
            Ok(_) => {
                let res = self.db.remove_file(ep_id);
                if res.is_err() {
                    self.notif_to_ui(
                        format!("Could not remove file from database: {title}"),
                        true,
                    );
                    return None;
                }
                self.update_filters(self.filters, false);
                self.notif_to_ui(format!("Deleted \"{title}\""), false);
            }
            Err(_) => self.notif_to_ui(format!("Error deleting \"{title}\""), true),
        }
        Some(())
    }

    /// Deletes all downloaded files for a given podcast from the user's local
    /// system.
    pub fn delete_files(&self, pod_id: i64) {
        let mut eps_id_to_remove = Vec::new();
        let mut eps_path_to_remove = Vec::new();

        {
            let borrowed_map = self.podcasts.borrow_map();
            let podcast = borrowed_map.get(&pod_id).unwrap();
            let podcast = podcast.read().unwrap();
            let mut borrowed_ep_map = podcast.episodes.borrow_map();

            for (_, ep) in borrowed_ep_map.iter_mut() {
                let mut ep = ep.write().unwrap();
                if ep.path.is_some() {
                    eps_path_to_remove.push(ep.path.clone().unwrap());
                    eps_id_to_remove.push(ep.id);
                    ep.path = None;
                }
            }
        }
        let mut success = true;
        for path in eps_path_to_remove.iter() {
            match fs::remove_file(path) {
                Ok(_) => {}
                Err(_) => success = false,
            }
        }

        let res = self.db.remove_files(&eps_id_to_remove);
        if res.is_err() {
            success = false;
        }

        if success {
            if !eps_id_to_remove.is_empty() {
                self.update_filters(self.filters, false);
                self.notif_to_ui("Files successfully deleted.".to_string(), false);
            } else {
                self.notif_to_ui("There are no downloads to delete".to_string(), false);
            }
        } else {
            self.notif_to_ui("Error while deleting files".to_string(), true);
        }
    }

    /// Removes a podcast from the list, optionally deleting local files first
    pub fn remove_podcast(&mut self, pod_id: i64, delete_files: bool) {
        if delete_files {
            self.delete_files(pod_id);
        }

        let pod = self.podcasts.get(pod_id);
        let (pod_id, url) = pod
            .map(|pod| {
                let pod = pod.read().unwrap();
                (pod.id, pod.url.clone())
            })
            .unwrap();
        let res = self.db.remove_podcast(pod_id);
        if self.config.enable_sync && self.sync_agent.is_some() {
            self.sync_agent.as_ref().unwrap().remove_podcast(url);
        }
        if res.is_err() {
            self.notif_to_ui("Could not remove podcast from database".to_string(), true);
            return;
        }
        {
            self.podcasts.replace_all(
                self.db
                    .get_podcasts()
                    .expect("Error retrieving info from database."),
            );
        }
        self.update_unplayed(true);
        self.update_filters(self.filters, false);
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

            let (pod_map, pod_order, _unused) = self.podcasts.borrow();
            for pod_id in pod_order.iter() {
                let pod = pod_map.get(pod_id).unwrap().read().unwrap();
                let new_filter = pod.episodes.filter_map(|ep| {
                    let ep = ep.read().unwrap();
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
                    if !(play_filter | download_filter) {
                        Some(ep.id)
                    } else {
                        None
                    }
                });
                let mut filtered_order = pod.episodes.borrow_filtered_order();
                *filtered_order = new_filter;
            }
        }
    }

    pub fn finalize(&mut self) {
        self.tx_to_ui.send(MainMessage::TearDown).unwrap();
        if let Some(thread) = self.ui_thread.take() {
            thread.join().unwrap(); // wait for UI thread to finish teardown
        }
    }
}
