use std::collections::HashSet;

struct PodcastEpisodeMap {
    pod_id: i64,
    by_url: HashMap<String, i64>,
    by_guid: HashMap<String, i64>,
}

/// State held between the two phases of a gpodder sync. Phase 1 subscribes to
/// the new podcasts the server reports and records the feeds we are still
/// fetching; phase 2 (`GetEpisodeActions`) only runs once those feeds have all
/// settled, so the episodes exist locally before play/position actions are
/// applied.
pub(super) struct PendingGpodderSync {
    /// Normalized URLs of newly-subscribed podcasts whose feeds are still being
    /// fetched. Drained as `gpodder_feed_settled` fires for each one (whether it
    /// succeeded or failed).
    awaited_feeds: HashSet<String>,
}

use super::{
    Action, App, Arc, EpisodeAction, GpodderRequest, HashMap, PodcastFeed, PodcastNoId, Result,
    feeds, normalize_url, resolve_redirection,
};

impl App {
    /// Add a new podcast by fetching the RSS feed data.
    pub fn add_podcast(&self, url: String) {
        let feed = PodcastFeed::new(None, url, None);
        feeds::check_feed(
            feed,
            self.config.max_retries,
            Arc::clone(&self.semaphore),
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
        {
            let mut sp = self
                .sync_progress
                .write()
                .expect("RwLock write should not fail");
            sp.total += pod_data.len();
        }
        for feed in pod_data {
            self.sync_counter += 1;
            feeds::check_feed(
                feed,
                self.config.max_retries,
                Arc::clone(&self.semaphore),
                self.tx_to_main.clone(),
            );
        }
        self.update_tracker_notif();
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
                    if let Some(id) = pod_id {
                        // Existing podcast: refresh only its episodes
                        self.refresh_podcast_episodes(id)?;
                    } else {
                        // New podcast: re-fetch all to get DB-assigned ID and sort order
                        self.podcasts.replace_all(self.db.get_podcasts()?);
                    }
                    self.update_unplayed(true);
                    self.update_queue();
                    self.update_filters(self.filters, true);
                }

                if pod_id.is_some() {
                    self.sync_tracker.push(result);
                    self.sync_counter -= 1;
                    self.sync_progress
                        .write()
                        .expect("RwLock write should not fail")
                        .completed += 1;
                    self.update_tracker_notif();

                    if self.sync_counter == 0 {
                        self.pos_sync_counter();
                    }
                } else {
                    self.notif_to_ui(
                        format!("Successfully added {} episodes.", result.added.len()),
                        false,
                    );
                    // If a gpodder sync is waiting on this feed, mark it settled
                    // (now that its episodes exist) so episode actions can run.
                    self.gpodder_feed_settled(&pod.url);
                }
            }
            Err(_err) => self.notif_to_ui(failure, true),
        }
        Ok(())
    }

    /// Refreshes the episode list for a single podcast from the database.
    fn refresh_podcast_episodes(&self, pod_id: i64) -> Result<()> {
        let episodes = self.db.get_episodes(pod_id)?;
        if let Some(podcast) = self.podcasts.get(pod_id) {
            let pod = podcast.read().expect("RwLock read should not fail");
            pod.episodes.replace_all(episodes);
        }
        Ok(())
    }

    pub(super) fn gpodder_sync_pre(&self) -> Result<()> {
        if self.config.enable_sync {
            self.tx_to_gpodder
                .send(GpodderRequest::GetSubscriptionChanges)?;
        }
        Ok(())
    }

    /// Processes subscription changes: adds server-only podcasts locally,
    /// uploads local-only podcasts to server. Returns the IDs of podcasts to
    /// remove and the normalized URLs of the newly-added podcasts whose feeds
    /// are now being fetched (the set the gpodder sync waits on before applying
    /// episode actions).
    fn process_subscription_changes(
        &self, added: Vec<String>, deleted: Vec<String>,
    ) -> (Vec<i64>, HashSet<String>) {
        // Build map with normalized URLs for comparison
        let pod_map: HashMap<String, (i64, String)> = self
            .podcasts
            .borrow_map()
            .iter()
            .map(|(id, pod)| {
                let rpod = pod.read().expect("Failed to acquire read lock");
                (normalize_url(&rpod.url), (*id, rpod.url.clone()))
            })
            .collect();

        // Add server podcasts not in local
        let mut server_urls = HashSet::new();
        let mut awaited_feeds = HashSet::new();
        for url in added {
            let url_resolved = resolve_redirection(&url).unwrap_or(url);
            let normalized = normalize_url(&url_resolved);
            server_urls.insert(normalized.clone());
            if !pod_map.contains_key(&normalized) {
                self.add_podcast(url_resolved);
                awaited_feeds.insert(normalized);
            }
        }

        // Upload local podcasts not on server
        let local_only: Vec<String> = pod_map
            .iter()
            .filter(|(norm_url, _)| !server_urls.contains(norm_url.as_str()))
            .map(|(_, (_, raw_url))| raw_url.clone())
            .collect();
        if !local_only.is_empty() {
            log::info!("Uploading {} local podcasts to gpodder", local_only.len());
            for url in local_only {
                self.tx_to_gpodder
                    .send(GpodderRequest::AddPodcast(url))
                    .inspect_err(|err| {
                        log::error!("Failed to upload podcast to gpodder: {err}");
                    })
                    .ok();
            }
        }

        // Resolve deleted URLs and find matching local podcast IDs
        let removed_pods = deleted
            .into_iter()
            .filter_map(|url| {
                let url_resolved = resolve_redirection(&url).unwrap_or(url);
                let normalized = normalize_url(&url_resolved);
                pod_map.get(&normalized).map(|(id, _)| *id)
            })
            .collect();

        (removed_pods, awaited_feeds)
    }

    /// Phase 1 of a gpodder sync: handle subscription changes. Adds the new
    /// podcasts the server reports (spawning their feed fetches) and removes
    /// deleted ones, then either requests the episode actions immediately (when
    /// no new feeds are pending) or waits for those feeds to arrive first.
    pub(super) fn gpodder_subscription_changes(
        &mut self, subscription_changes: (Vec<String>, Vec<String>),
    ) -> Result<()> {
        let (added, deleted) = subscription_changes;
        let (removed_pods, awaited_feeds) = self.process_subscription_changes(added, deleted);

        for pod_id in removed_pods {
            self.remove_podcast(pod_id, true)?;
        }

        self.pending_gpodder = Some(PendingGpodderSync { awaited_feeds });
        self.maybe_request_episode_actions()
    }

    /// Requests episode actions (phase 2) once every newly-subscribed feed has
    /// been fetched, so the episodes exist locally before actions are applied.
    fn maybe_request_episode_actions(&self) -> Result<()> {
        if let Some(pending) = &self.pending_gpodder
            && pending.awaited_feeds.is_empty()
        {
            self.tx_to_gpodder.send(GpodderRequest::GetEpisodeActions)?;
        }
        Ok(())
    }

    /// Phase 2 of a gpodder sync: apply the episode actions. By now every
    /// newly-subscribed podcast's feed has settled, so for feeds that succeeded
    /// the episodes exist locally and their play/position status is no longer
    /// lost. The timestamp is a server-fetch watermark (`min` of the
    /// subscriptions and actions timestamps, each only advanced on a successful
    /// fetch), so it is persisted unconditionally: a feed that failed to fetch
    /// is retried via subscription changes, not by replaying episode actions.
    pub(super) fn gpodder_episode_actions(
        &mut self, episode_actions: Vec<EpisodeAction>, timestamp: u64,
    ) -> Result<()> {
        self.pending_gpodder = None;
        let number_updates = self.apply_gpodder_episode_actions(episode_actions)?;
        self.finalize_gpodder_sync(timestamp, number_updates);
        Ok(())
    }

    /// Matches gpodder episode actions to local episodes (by GUID, then URL) and
    /// applies the resulting play positions. Returns the number of episodes
    /// updated.
    fn apply_gpodder_episode_actions(
        &mut self, episode_actions: Vec<EpisodeAction>,
    ) -> Result<usize> {
        let pod_data: HashMap<String, PodcastEpisodeMap> = self
            .podcasts
            .map(
                |pod| {
                    let by_url: HashMap<String, i64> = pod
                        .episodes
                        .map(|ep| (ep.url.clone(), ep.id), false)
                        .into_iter()
                        .collect();
                    let by_guid: HashMap<String, i64> = pod
                        .episodes
                        .map(|ep| (ep.guid.clone(), ep.id), false)
                        .into_iter()
                        .filter(|(guid, _)| !guid.is_empty())
                        .collect();
                    (
                        normalize_url(&pod.url),
                        PodcastEpisodeMap {
                            pod_id: pod.id,
                            by_url,
                            by_guid,
                        },
                    )
                },
                false,
            )
            .into_iter()
            .collect();

        let mut last_actions = HashMap::new();

        for a in episode_actions {
            match a.action {
                Action::Play => {
                    log::debug!(
                        "EpisodeAction received - podcast: {} episode: {} guid: {:?} position: {:?} total: {:?}",
                        a.podcast,
                        a.episode,
                        a.guid,
                        a.position,
                        a.total
                    );

                    let normalized_podcast = normalize_url(&a.podcast);
                    if let Some(pod) = pod_data.get(&normalized_podcast)
                        && let Some(position) = a.position
                        && let Some(total) = a.total
                    {
                        // Match by GUID first (like AntennaPod), then fall back to URL
                        let ep_id = a
                            .guid
                            .as_deref()
                            .and_then(|g| pod.by_guid.get(g))
                            .or_else(|| pod.by_url.get(a.episode.as_str()));
                        if let Some(ep_id) = ep_id {
                            last_actions.insert((pod.pod_id, *ep_id), (position, total));
                        } else {
                            log::warn!(
                                "Gpodder episode action skipped: episode not found locally: {}",
                                a.episode
                            );
                        }
                    } else if !pod_data.contains_key(&normalized_podcast) {
                        log::warn!(
                            "Gpodder episode action skipped: podcast not found locally: {}",
                            a.podcast
                        );
                    }
                }
                Action::Delete | Action::Download | Action::New => {}
            }
        }

        let updates: Vec<(i64, i64, u64, u64)> = last_actions
            .into_iter()
            .map(|((pod_id, ep_id), (position, total))| (pod_id, ep_id, position, total))
            .collect();
        let number_updates = updates.len();

        self.mark_played_db_batch(updates)?;
        Ok(number_updates)
    }

    /// Completes a gpodder sync: persists the sync timestamp, refreshes the UI,
    /// and notifies how many episodes were updated.
    fn finalize_gpodder_sync(&self, timestamp: u64, number_updates: usize) {
        self.db
            .set_param("timestamp", &timestamp.to_string())
            .inspect_err(|err| log::error!("Failed to set timestamp in database: {err}"))
            .ok();
        self.update_unplayed(true);
        self.update_filters(self.filters, false);
        self.notif_to_ui(
            format!("Gpodder sync finished with {number_updates} updates"),
            false,
        );
    }

    /// Records that a newly-subscribed podcast's feed has settled, whether it
    /// was fetched successfully or failed. Once every feed a gpodder sync is
    /// waiting on has settled, the episode actions are requested. A no-op for
    /// feeds the sync is not waiting on (e.g. manual additions). A failed feed
    /// is simply dropped from the wait set so the sync isn't blocked on it.
    pub(super) fn gpodder_feed_settled(&mut self, url: &str) {
        let normalized = normalize_url(url);
        let cleared = self
            .pending_gpodder
            .as_mut()
            .is_some_and(|pending| pending.awaited_feeds.remove(&normalized));
        if cleared {
            self.maybe_request_episode_actions()
                .inspect_err(|err| log::error!("Failed to request episode actions: {err}"))
                .ok();
        }
    }

    pub(super) fn pos_sync_counter(&mut self) {
        // count up total new episodes and updated episodes when sync process is
        // finished
        let mut added = 0;
        let mut updated = 0;
        for res in &self.sync_tracker {
            added += res.added.len();
            updated += res.updated.len();
        }
        if added + updated > 0 {
            self.update_filters(self.filters, false);
        }

        self.sync_tracker = Vec::new();
        self.sync_progress
            .write()
            .expect("RwLock write should not fail")
            .reset();
        self.notif_to_ui(
            format!("Sync complete: Added {added}, updated {updated} episodes."),
            false,
        );

        self.gpodder_sync_pre()
            .inspect_err(|err| log::error!("gpodder_sync_pre failed: {err}"))
            .ok();
    }
}
