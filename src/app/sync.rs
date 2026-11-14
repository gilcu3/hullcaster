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
    feeds, normalize_url,
};
use crate::feeds::FetchedUrls;
use crate::gpodder::SubscriptionChanges;
use crate::utils::urls_equivalent;

/// Who made a subscription change, which decides whether it is pushed to the
/// gpodder server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChangeOrigin {
    /// The user, on this device: push it.
    User,
    /// The gpodder server: it already knows.
    Server,
}

/// What a batch of subscription changes from the gpodder server means
/// locally.
#[derive(Debug, Default, PartialEq, Eq)]
struct SubscriptionPlan {
    /// Local podcasts the server removed.
    remove: Vec<i64>,
    /// Podcasts the server added that we do not have yet.
    fetch: Vec<String>,
    /// Local podcasts to upload, because the server does not have them.
    upload: Vec<String>,
}

/// Plans how to apply subscription changes from the server to the local
/// podcasts, given as `(id, url)`, the way `AntennaPod` does:
/// - removals are applied first and matched exactly, as the servers do, so a
///   removed alias never takes an active podcast with it;
/// - adds are matched with [`urls_equivalent`], to skip feeds we already have;
/// - local podcasts are only uploaded on a first sync, when `add` is the
///   server's whole subscription list rather than its recent changes.
fn plan_subscription_changes(
    local: &[(i64, String)], changes: &SubscriptionChanges,
) -> SubscriptionPlan {
    let (removed, kept): (Vec<_>, Vec<_>) = local
        .iter()
        .partition(|(_, url)| changes.remove.contains(url));

    let mut fetch: Vec<String> = Vec::new();
    for url in &changes.add {
        let known = kept
            .iter()
            .map(|(_, local)| local)
            .chain(&fetch)
            .any(|known| urls_equivalent(known, url));
        if !known {
            fetch.push(url.clone());
        }
    }

    let upload = if changes.first_sync {
        kept.iter()
            .filter(|(_, url)| {
                !changes
                    .add
                    .iter()
                    .any(|server| urls_equivalent(server, url))
            })
            .map(|(_, url)| url.clone())
            .collect()
    } else {
        Vec::new()
    };

    SubscriptionPlan {
        remove: removed.iter().map(|(id, _)| *id).collect(),
        fetch,
        upload,
    }
}

impl App {
    /// Add a new podcast by fetching the RSS feed data.
    pub(super) fn add_podcast(&mut self, url: String, origin: ChangeOrigin) {
        if origin == ChangeOrigin::Server {
            self.gpodder_added_feeds.insert(url.clone());
        }
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

    /// Adds a newly fetched podcast to the database, unless it is already
    /// subscribed under any of the URLs it was fetched through.
    pub fn add_new_podcast(&mut self, pod: &PodcastNoId, urls: &FetchedUrls) -> Result<()> {
        let subscribed = self.podcasts.map(|p| p.url.clone(), false);
        if feeds::is_already_subscribed(pod, urls, subscribed.iter().map(String::as_str)) {
            log::info!("Not adding {}: already subscribed", urls.requested);
            self.notif_to_ui(format!("Already subscribed to {}.", pod.title), false);
        } else {
            match self.db.insert_podcast(pod) {
                Ok(result) => {
                    if !self.gpodder_added_feeds.contains(&urls.requested) {
                        self.push_subscription_changes(vec![pod.url.clone()], Vec::new());
                    }
                    // Re-fetch all to get the DB-assigned ID and sort order
                    self.podcasts.replace_all(self.db.get_podcasts()?);
                    self.update_unplayed(true);
                    self.update_queue();
                    self.update_filters(self.filters, true);
                    self.notif_to_ui(
                        format!("Successfully added {} episodes.", result.added.len()),
                        false,
                    );
                }
                Err(_err) => self.notif_to_ui(
                    format!("Error adding podcast {} to database.", pod.title),
                    true,
                ),
            }
        }
        // If a gpodder sync is waiting on this feed, mark it settled whatever
        // happened, so episode actions can run. The sync knows the feed by the
        // URL it requested, which differs from `pod.url` if the feed moved.
        self.gpodder_feed_settled(&urls.requested);
        Ok(())
    }

    /// Updates an existing podcast with data from its RSS feed.
    pub fn sync_podcast_data(&mut self, pod_id: i64, pod: &PodcastNoId) -> Result<()> {
        match self.db.update_podcast(pod_id, pod) {
            Ok(result) => {
                let moved = self
                    .podcasts
                    .map_single(pod_id, |p| p.url != pod.url)
                    .unwrap_or_default();
                let episodes_changed = !result.added.is_empty() || !result.updated.is_empty();
                if moved {
                    // Reload, so the list shows the URL the database kept
                    self.podcasts.replace_all(self.db.get_podcasts()?);
                } else if episodes_changed {
                    self.refresh_podcast_episodes(pod_id)?;
                }
                if moved || episodes_changed {
                    self.update_unplayed(true);
                    self.update_queue();
                    self.update_filters(self.filters, true);
                }

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
            }
            Err(_err) => self.notif_to_ui(format!("Error synchronizing {}.", pod.title), true),
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

    /// Pushes subscription changes made on this device to the gpodder server.
    /// Changes that came from the server must not come through here, or they
    /// would be echoed back.
    pub(super) fn push_subscription_changes(&self, add: Vec<String>, remove: Vec<String>) {
        if !self.config.enable_sync || (add.is_empty() && remove.is_empty()) {
            return;
        }
        self.tx_to_gpodder
            .send(GpodderRequest::UploadSubscriptionChanges { add, remove })
            .inspect_err(|err| log::error!("Failed to send subscription changes: {err}"))
            .ok();
    }

    /// Phase 1 of a gpodder sync: handle subscription changes. Removes the
    /// podcasts the server removed, adds the ones it added (spawning their feed
    /// fetches) and, on a first sync, uploads the local podcasts it lacks.
    /// Then either requests the episode actions immediately (when no new feeds
    /// are pending) or waits for those feeds to arrive first.
    pub(super) fn gpodder_subscription_changes(
        &mut self, changes: &SubscriptionChanges,
    ) -> Result<()> {
        let local = self.podcasts.map(|pod| (pod.id, pod.url.clone()), false);
        let plan = plan_subscription_changes(&local, changes);

        for pod_id in plan.remove {
            self.remove_podcast(pod_id, true, ChangeOrigin::Server)?;
        }
        let mut awaited_feeds = HashSet::new();
        for url in plan.fetch {
            awaited_feeds.insert(normalize_url(&url));
            self.add_podcast(url, ChangeOrigin::Server);
        }
        if !plan.upload.is_empty() {
            log::info!(
                "First gpodder sync: uploading {} local podcasts",
                plan.upload.len()
            );
            self.push_subscription_changes(plan.upload, Vec::new());
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
        self.gpodder_added_feeds.remove(url);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn local(urls: &[&str]) -> Vec<(i64, String)> {
        (1..).zip(urls.iter().map(ToString::to_string)).collect()
    }

    fn changes(add: &[&str], remove: &[&str], first_sync: bool) -> SubscriptionChanges {
        SubscriptionChanges {
            add: add.iter().map(ToString::to_string).collect(),
            remove: remove.iter().map(ToString::to_string).collect(),
            first_sync,
        }
    }

    #[test]
    fn incremental_sync_without_changes_uploads_nothing() {
        let local = local(&["https://a.com/feed", "https://b.com/feed"]);
        let plan = plan_subscription_changes(&local, &changes(&[], &[], false));
        assert_eq!(plan, SubscriptionPlan::default());
    }

    #[test]
    fn removed_alias_keeps_active_podcast() {
        // An old alias that redirects to the active feed is removed on the
        // server, while the active URL is (re)added.
        let local = local(&["https://podcast.darknetdiaries.com/"]);
        let plan = plan_subscription_changes(
            &local,
            &changes(
                &["https://podcast.darknetdiaries.com"],
                &["https://feeds.megaphone.fm/darknetdiaries"],
                false,
            ),
        );
        assert_eq!(plan, SubscriptionPlan::default());
    }

    #[test]
    fn removals_match_exactly() {
        let local = local(&["https://a.com/feed", "http://b.com/feed"]);
        let plan = plan_subscription_changes(
            &local,
            &changes(&[], &["https://a.com/feed", "https://b.com/feed"], false),
        );
        assert_eq!(plan.remove, vec![1]);
    }

    #[test]
    fn adds_skip_feeds_we_already_have() {
        let local = local(&["https://a.com/feed"]);
        let plan = plan_subscription_changes(
            &local,
            &changes(
                &[
                    "http://A.com/feed/",
                    "https://c.com/feed",
                    "https://C.com/feed",
                ],
                &[],
                false,
            ),
        );
        assert_eq!(plan.fetch, vec!["https://c.com/feed"]);
        assert_eq!(plan.upload, Vec::<String>::new());
    }

    #[test]
    fn removal_then_add_of_another_spelling_refetches() {
        let local = local(&["http://a.com/feed"]);
        let plan = plan_subscription_changes(
            &local,
            &changes(&["https://a.com/feed"], &["http://a.com/feed"], false),
        );
        assert_eq!(plan.remove, vec![1]);
        assert_eq!(plan.fetch, vec!["https://a.com/feed"]);
    }

    #[test]
    fn first_sync_uploads_only_what_the_server_lacks() {
        let local = local(&[
            "https://a.com/feed",
            "https://b.com/feed",
            "https://c.com/feed",
        ]);
        let plan = plan_subscription_changes(
            &local,
            &changes(
                &["https://A.com/feed/", "https://d.com/feed"],
                &["https://c.com/feed"],
                true,
            ),
        );
        assert_eq!(
            plan,
            SubscriptionPlan {
                remove: vec![3],
                fetch: vec!["https://d.com/feed".to_string()],
                upload: vec!["https://b.com/feed".to_string()],
            }
        );
    }
}
