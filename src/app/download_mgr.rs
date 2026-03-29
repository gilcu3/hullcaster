use super::{
    App, EpData, GpodderRequest, Options, PathBuf, Result, anyhow, downloads, fs,
    sanitize_with_options,
};

impl App {
    /// Given a podcast index (and not an episode index), this will send a
    /// vector of jobs to download all episodes in the
    /// podcast. If given an episode index as well, it will download just that
    /// episode.
    pub fn download(&mut self, pod_id: i64, ep_id: Option<i64>) -> Result<()> {
        let pod_title;
        let mut ep_data = Vec::new();
        {
            let podcast = self
                .podcasts
                .get(pod_id)
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
                        &self.semaphore,
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
            let pod_id = ep_data.pod_id;
            let podcast = self
                .podcasts
                .get(pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?;
            let episodes = &podcast
                .read()
                .expect("RwLock read should not fail")
                .episodes;
            let mut episode_map = episodes.borrow_map();
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
            let podcast = self
                .podcasts
                .get(pod_id)
                .ok_or_else(|| anyhow!("Failed to get pod_id: {pod_id}"))?;
            let episodes = &podcast
                .read()
                .expect("RwLock read should not fail")
                .episodes;
            let mut episode_map = episodes.borrow_map();
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
            self.delete_files(pod_id).ok();
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
}
