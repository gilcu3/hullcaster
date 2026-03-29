use std::collections::HashSet;

use super::{
    Action, App, Arc, EpisodeAction, GpodderRequest, HashMap, PodcastFeed, PodcastNoId, Result,
    feeds, resolve_redirection,
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
    /// uploads local-only podcasts to server, returns IDs of podcasts to remove.
    fn process_subscription_changes(&self, added: Vec<String>, deleted: Vec<String>) -> Vec<i64> {
        let pod_map: HashMap<String, i64> = self
            .podcasts
            .borrow_map()
            .iter()
            .map(|(id, pod)| {
                let rpod = pod.read().expect("Failed to acquire read lock");
                (rpod.url.clone(), *id)
            })
            .collect();

        // Add server podcasts not in local
        let mut server_urls = HashSet::new();
        for url in added {
            let url_resolved = resolve_redirection(&url).unwrap_or(url);
            server_urls.insert(url_resolved.clone());
            if !pod_map.contains_key(&url_resolved) {
                self.add_podcast(url_resolved);
            }
        }

        // Upload local podcasts not on server
        let local_only: Vec<String> = pod_map
            .keys()
            .filter(|url| !server_urls.contains(*url))
            .cloned()
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
        deleted
            .into_iter()
            .filter_map(|url| {
                let url_resolved = resolve_redirection(&url).unwrap_or(url);
                pod_map.get(&url_resolved).copied()
            })
            .collect()
    }

    pub(super) fn gpodder_sync_pos(
        &mut self, subscription_changes: (Vec<String>, Vec<String>),
        episode_actions: Vec<EpisodeAction>, timestamp: u64,
    ) -> Result<()> {
        let (added, deleted) = subscription_changes;
        let removed_pods = self.process_subscription_changes(added, deleted);

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
        Ok(())
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
