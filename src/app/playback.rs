use std::sync::{Arc, RwLock};

use super::{App, GpodderRequest, HashMap, MAX_DURATION, MainMessage, Result, anyhow, play_file};
use crate::gpodder::EpisodePlayedData;
use crate::types::Episode;

impl App {
    /// Syncs the unplayed list for a single episode: adds or removes it based
    /// on its played state. Returns true if a change was made.
    fn sync_unplayed_episode(&self, ep_id: i64, episode: &Arc<RwLock<Episode>>) -> bool {
        let played = episode.read().expect("RwLock read should not fail").played;
        if played && self.unplayed.contains_key(ep_id) {
            self.unplayed.remove(ep_id);
            true
        } else if !played && !self.unplayed.contains_key(ep_id) {
            self.unplayed.push_arc(episode.clone());
            true
        } else {
            false
        }
    }

    /// Sends a gpodder `MarkPlayed` request for a single episode.
    fn gpodder_mark_played(
        &self, pod_url: String, ep_url: String, guid: String, position: u64, duration: Option<u64>,
    ) -> Result<()> {
        if self.config.enable_sync {
            let duration = duration.unwrap_or_else(|| {
                log::warn!("Setting duration to infinity for episode {ep_url}, else cannot mark as played on gpodder");
                MAX_DURATION
            });
            self.tx_to_gpodder
                .send(GpodderRequest::MarkPlayed(EpisodePlayedData {
                    podcast_url: pod_url,
                    episode_url: ep_url,
                    guid,
                    position,
                    duration,
                }))?;
        }
        Ok(())
    }

    /// Updates unplayed list and filters if changes were made.
    fn apply_played_changes(&self, changed: bool) {
        if changed {
            self.update_unplayed(false);
            self.update_filters(self.filters, false);
        }
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
            if let Err(err) = play_file::execute(&self.config.play_command, &ep_url) {
                self.notif_to_ui(format!("Could not stream URL: {err}"), true);
            } else if self.config.mark_as_played_on_play
                && let Err(err) = self.mark_played(pod_id, ep_id, true)
            {
                self.notif_to_ui(format!("Could not mark episode played: {err}"), true);
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

    /// Given a podcast and episode, it updates the given episode,
    /// sending this info to the database, updating in self.podcasts and syncing
    /// with gpodder.
    /// If played is true, do not modify the current position of episode
    /// if position is near duration, mark episode as played
    /// else just update position
    /// TODO: separate `mark_played` from set position
    pub fn mark_played(&self, pod_id: i64, ep_id: i64, played: bool) -> Result<()> {
        let mut changed = false;
        let (duration, ep_position, ep_url, ep_guid, pod_url) = {
            let podcast_map = self.podcasts.borrow_map();
            let podcast = podcast_map
                .get(&pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?
                .read()
                .expect("RwLock read should not fail");
            let w_episode = podcast
                .episodes
                .get(ep_id)
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
            changed |= self.sync_unplayed_episode(ep_id, &w_episode);

            let episode = w_episode.read().expect("RwLock read should not fail");
            self.db
                .set_played_status(ep_id, episode.position, episode.duration, played)?;
            (
                episode.duration,
                episode.position,
                episode.url.clone(),
                episode.guid.clone(),
                podcast.url.clone(),
            )
        };

        self.apply_played_changes(changed);
        if self.config.enable_sync {
            let dur = duration.unwrap_or_else(|| {
                log::warn!("Setting duration to infinity for episode {ep_url}, else cannot mark as played on gpodder");
                MAX_DURATION
            });
            let position = if played { dur } else { ep_position };
            self.tx_to_gpodder
                .send(GpodderRequest::MarkPlayed(EpisodePlayedData {
                    podcast_url: pod_url,
                    episode_url: ep_url,
                    guid: ep_guid,
                    position,
                    duration: dur,
                }))?;
        }
        Ok(())
    }

    pub(super) fn mark_played_db_batch(
        &mut self, updates: Vec<(i64, i64, u64, u64)>,
    ) -> Result<()> {
        let mut pod_map = HashMap::with_capacity(updates.len());
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
                    let played = episode
                        .duration
                        .map_or_else(|| episode.played, |duration| duration <= 1 + position);
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

    /// Given a podcast, it marks all episodes for that podcast as
    /// played/unplayed, sending this info to the database and updating in
    /// self.podcasts
    pub fn mark_all_played(&mut self, pod_id: i64, played: bool) -> Result<()> {
        let mut changed = false;
        let (sync_list, db_list) = {
            let podcast = self
                .podcasts
                .get(pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?;
            let podcast_url = podcast
                .read()
                .expect("RwLock read should not fail")
                .url
                .clone();

            let mut sync_list = Vec::new();
            let mut db_list = Vec::new();
            let episodes = &podcast
                .read()
                .expect("RwLock read should not fail")
                .episodes;
            for (ep_id, w_episode) in episodes.borrow_map().iter_mut() {
                {
                    let mut episode = w_episode.write().expect("RwLock write should not fail");
                    if episode.played != played {
                        changed = true;
                        episode.played = played;
                    }
                }
                changed |= self.sync_unplayed_episode(*ep_id, w_episode);

                let episode = w_episode.read().expect("RwLock read should not fail");
                if self.config.enable_sync {
                    let duration = episode.duration.unwrap_or_else(|| {
                        log::warn!(
                            "Setting duration to infinity, else cannot mark as played on gpodder"
                        );
                        MAX_DURATION
                    });
                    let position = if played { duration } else { episode.position };
                    sync_list.push(EpisodePlayedData {
                        podcast_url: podcast_url.clone(),
                        episode_url: episode.url.clone(),
                        guid: episode.guid.clone(),
                        position,
                        duration,
                    });
                }
                db_list.push((*ep_id, episode.position, episode.duration, played));
            }
            (sync_list, db_list)
        };

        self.apply_played_changes(changed);
        self.db.set_played_status_batch(db_list)?;

        if self.config.enable_sync {
            self.tx_to_gpodder
                .send(GpodderRequest::MarkPlayedBatch(sync_list))?;
        }
        Ok(())
    }

    pub fn update_position(&self, pod_id: i64, ep_id: i64, position: u64) -> Result<()> {
        let mut changed = false;
        let (duration, ep_url, ep_guid, pod_url) = {
            let podcast = self
                .podcasts
                .get(pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?;
            let podcast = podcast.read().expect("RwLock read should not fail");

            let w_episode = podcast
                .episodes
                .get(ep_id)
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

            changed |= self.sync_unplayed_episode(ep_id, &w_episode);
            let episode = w_episode.read().expect("RwLock read should not fail");
            self.db
                .set_played_status(ep_id, episode.position, episode.duration, episode.played)?;
            (
                episode.duration,
                episode.url.clone(),
                episode.guid.clone(),
                podcast.url.clone(),
            )
        };

        self.apply_played_changes(changed);
        self.gpodder_mark_played(pod_url, ep_url, ep_guid, position, duration)?;
        Ok(())
    }
}
