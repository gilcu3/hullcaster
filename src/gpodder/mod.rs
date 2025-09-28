use anyhow::{anyhow, Result};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use ureq::Agent;

use self::net::{execute_request_get, execute_request_post};
use self::types::{Device, EpisodeAction, Podcast, PodcastChanges, State, UploadPodcastChanges};

pub use self::types::{Action, Config};
mod net;
mod types;

fn current_time() -> Result<i64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64)
}

#[derive(Clone, Debug)]
pub struct GpodderController {
    config: Config,
    agent: Agent,
    state: State,
}

impl GpodderController {
    pub fn new(config: Config, timestamp: Option<i64>) -> GpodderController {
        let agent_builder = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(30)));
        let agent = agent_builder.build().into();
        let timestamp = timestamp.unwrap_or(0);
        let state = State {
            actions_timestamp: timestamp.into(),
            subscriptions_timestamp: timestamp.into(),
            logged_in: false.into(),
        };
        GpodderController {
            config,
            agent,
            state,
        }
    }

    pub fn get_timestamp(&self) -> i64 {
        std::cmp::min(
            self.state.actions_timestamp.get(),
            self.state.subscriptions_timestamp.get(),
        )
    }

    fn init(&self) -> Result<()> {
        let res = self.get_devices();
        let mut exists = false;
        for dev in res.unwrap() {
            if dev.id == self.config.device {
                log::info!(
                    "Using device: id = {}, type = {}, subscriptions = {}, caption = {}",
                    dev.id,
                    dev._type,
                    dev.subscriptions,
                    dev.caption
                );
                exists = true;
                break;
            }
        }
        if !exists {
            self.register_device()
        } else {
            Ok(())
        }
    }

    pub fn mark_played(
        &self, podcast_url: &str, episode_url: &str, position: i64, duration: Option<i64>,
    ) -> Result<String> {
        duration.ok_or(anyhow!(
            "Impossible to mark played position without duration"
        ))?;
        self.require_login()?;
        let _url_mark_played = format!(
            "{}/api/2/episodes/{}/{}.json",
            self.config.server, self.config.username, self.config.device
        );
        let action = EpisodeAction {
            podcast: podcast_url.to_string(),
            episode: episode_url.to_string(),
            action: Action::Play,
            timestamp: current_time()?,
            started: Some(0),
            position: Some(position),
            total: duration,
        };
        let actions = [action];
        let msg = serde_json::to_string(&actions)?;

        let result = execute_request_post(
            &self.agent,
            _url_mark_played,
            msg,
            &self.config.credentials,
            self.config.max_retries,
        )?;
        log::info!("Marked position: {position} episode: {episode_url} podcast: {podcast_url}");
        Ok(result)
    }

    pub fn mark_played_batch(&self, eps: Vec<(&str, &str, i64, Option<i64>)>) -> Result<String> {
        self.require_login()?;
        let _url_mark_played = format!(
            "{}/api/2/episodes/{}/{}.json",
            self.config.server, self.config.username, self.config.device
        );
        let actions: Vec<EpisodeAction> = eps
            .iter()
            .filter_map(|(podcast_url, episode_url, position, duration)| {
                Some(EpisodeAction {
                    podcast: podcast_url.to_string(),
                    episode: episode_url.to_string(),
                    action: Action::Play,
                    timestamp: current_time().ok()?,
                    started: Some(0),
                    position: Some(*position),
                    total: *duration,
                })
            })
            .collect();
        let msg = serde_json::to_string(&actions)?;

        let result = execute_request_post(
            &self.agent,
            _url_mark_played,
            msg,
            &self.config.credentials,
            self.config.max_retries,
        )?;
        log::info!("Marked played: {} actions", actions.len());
        Ok(result)
    }

    pub fn get_episode_action_changes(&self) -> Result<Vec<EpisodeAction>> {
        self.require_login()?;
        let url_episode_action_changes = format!(
            "{}/api/2/episodes/{}.json",
            self.config.server, self.config.username
        );
        let since = self.state.actions_timestamp.get();
        let json_string = execute_request_get(
            &self.agent,
            url_episode_action_changes,
            vec![("since", since.to_string().as_str())],
            &self.config.credentials,
            self.config.max_retries,
        )?;
        let actions: serde_json::Value = serde_json::from_str(json_string.as_str())?;
        let timestamp = actions["timestamp"]
            .as_i64()
            .ok_or(anyhow::anyhow!("Parsing timestamp failed"))?;
        let episode_actions = actions["actions"].as_array().unwrap();
        let mut actions: Vec<EpisodeAction> = Vec::new();
        for action in episode_actions {
            let daction = serde_json::from_value::<EpisodeAction>(action.clone());
            actions.push(daction.unwrap());
        }
        self.state.actions_timestamp.set(timestamp + 1);
        Ok(actions)
    }

    fn require_login(&self) -> Result<()> {
        if !self.state.logged_in.get() {
            self.login()?;
            self.state.logged_in.set(true);
            self.init()?;
        }
        Ok(())
    }

    fn login(&self) -> Result<()> {
        let url_login = format!(
            "{}/api/2/auth/{}/login.json",
            self.config.server, self.config.username
        );
        execute_request_post(
            &self.agent,
            url_login,
            String::new(),
            &self.config.credentials,
            self.config.max_retries,
        )?;
        Ok(())
    }

    // This is probably not implemented in micro-gpodder
    #[allow(dead_code)]
    fn logout(&self) -> Result<()> {
        let url_login = format!(
            "{}/api/2/auth/{}/logout.json",
            self.config.server, self.config.username
        );
        execute_request_post(
            &self.agent,
            url_login,
            String::new(),
            &self.config.credentials,
            self.config.max_retries,
        )?;
        Ok(())
    }

    pub fn get_subscription_changes(&self) -> Result<(Vec<String>, Vec<String>)> {
        if self.state.subscriptions_timestamp.get() == 0 {
            let added = self.get_all_subscriptions()?;
            return Ok((added, Vec::new()));
        }
        let url_subscription_changes = format!(
            "{}/api/2/subscriptions/{}/{}.json",
            self.config.server, self.config.username, self.config.device
        );
        let pastime = self.state.subscriptions_timestamp.get().to_string();
        let params = vec![("since", pastime.as_str())];
        let json_string = execute_request_get(
            &self.agent,
            url_subscription_changes,
            params,
            &self.config.credentials,
            self.config.max_retries,
        )?;
        let parsed: serde_json::Result<PodcastChanges> = serde_json::from_str(json_string.as_str());
        if let Ok(changes) = parsed {
            for sub in &changes.add {
                log::info!("podcast added {sub}");
            }
            for sub in &changes.remove {
                log::info!("podcast removed {sub}");
            }
            self.state
                .subscriptions_timestamp
                .set(changes.timestamp + 1);
            Ok((changes.add, changes.remove))
        } else {
            Err(anyhow!("Error parsing subscription changes"))
        }
    }

    pub fn upload_subscription_changes(&self, changes: (Vec<&String>, Vec<&String>)) -> Result<()> {
        let url_upload_subscriptions = format!(
            "{}/api/2/subscriptions/{}/{}.json",
            self.config.server, self.config.username, self.config.device
        );
        let json_changes = serde_json::json!({
            "add": changes.0,
            "remove": changes.1
        })
        .to_string();
        let json_string = execute_request_post(
            &self.agent,
            url_upload_subscriptions,
            json_changes,
            &self.config.credentials,
            self.config.max_retries,
        )?;
        let parsed: serde_json::Result<UploadPodcastChanges> =
            serde_json::from_str(json_string.as_str());
        if let Ok(changes) = parsed {
            for sub in &changes.update_urls {
                log::info!("url changed {} {}", sub[0], sub[1]);
            }
            self.state
                .subscriptions_timestamp
                .set(changes.timestamp + 1);
            Ok(())
        } else {
            Err(anyhow!("Error parsing url subscription changes"))
        }
    }

    pub fn add_podcast(&self, url: &String) -> Result<()> {
        self.upload_subscription_changes((vec![url], vec![]))
    }

    pub fn remove_podcast(&self, url: &String) -> Result<()> {
        self.upload_subscription_changes((vec![], vec![url]))
    }

    fn get_all_subscriptions(&self) -> Result<Vec<String>> {
        let url_subscription_changes = format!(
            "{}/subscriptions/{}.json",
            self.config.server, self.config.username
        );
        let json_string = execute_request_get(
            &self.agent,
            url_subscription_changes,
            vec![],
            &self.config.credentials,
            self.config.max_retries,
        )?;
        let parsed: serde_json::Result<Vec<Podcast>> = serde_json::from_str(json_string.as_str());

        if let Ok(subscriptions) = parsed {
            Ok(subscriptions.iter().map(|f| f.feed.clone()).collect())
        } else {
            Err(anyhow!("Error parsing subscriptions"))
        }
    }

    fn get_devices(&self) -> Result<Vec<Device>> {
        let url_devices = format!(
            "{}/api/2/devices/{}.json",
            self.config.server, self.config.username
        );
        let json_string = execute_request_get(
            &self.agent,
            url_devices,
            vec![],
            &self.config.credentials,
            self.config.max_retries,
        )?;
        Ok(serde_json::from_str(json_string.as_str())?)
    }

    fn register_device(&self) -> Result<()> {
        let url_register = format!(
            "{}/api/2/devices/{}/{}.json",
            self.config.server, self.config.username, self.config.device
        );
        let device = serde_json::json!({
            "caption": "",
            "type": "laptop"
        })
        .to_string();
        execute_request_post(
            &self.agent,
            url_register,
            device,
            &self.config.credentials,
            self.config.max_retries,
        )?;
        log::info!("Registered device {}", self.config.device);
        Ok(())
    }

    #[allow(dead_code)]
    fn get_device_updates(&self) {
        let url_device_updates = format!(
            "{}/api/2/updates/{}/{}.json",
            self.config.server, self.config.username, self.config.device
        );
        let _json_string = execute_request_get(
            &self.agent,
            url_device_updates,
            vec![(
                "since",
                self.state.actions_timestamp.get().to_string().as_str(),
            )],
            &self.config.credentials,
            self.config.max_retries,
        );
    }
    #[allow(dead_code)]
    fn get_sync_status(&self) {
        let url_sync_status = format!(
            "{}/api/2/sync-devices/{}.json",
            self.config.server, self.config.username
        );
        let _json_string = execute_request_get(
            &self.agent,
            url_sync_status,
            vec![],
            &self.config.credentials,
            self.config.max_retries,
        );
    }

    // this is WIP
    #[allow(dead_code)]
    fn set_sync_status(&self) {
        let url_sync_status = format!(
            "{}/api/2/sync-devices/{}.json",
            self.config.server, self.config.username
        );
        let dev_sync = serde_json::json!({
            "synchronize": [["dev1", "dev2"]],
            "stop-synchronize": [] })
        .to_string();

        let _json_string = execute_request_post(
            &self.agent,
            url_sync_status,
            dev_sync,
            &self.config.credentials,
            self.config.max_retries,
        );
    }

    #[cfg(test)]
    pub fn test_gpodder_api(&self) -> Result<()> {
        let res = self.get_devices();
        let mut exists = false;
        for dev in res? {
            println!("{dev:?}");
            if dev.id == self.config.device {
                exists = true;
                break;
            }
        }
        if !exists {
            self.register_device()?;
        } else {
            println!("Device already exists");
        }

        // Not implemented in opodsync
        // get_sync_status();
        // set_sync_status();
        // get_device_updates();

        let actions = self.get_episode_action_changes();
        for a in actions? {
            match a.action {
                Action::Play => {
                    println!(
                        "Play: {} - {} -> {} {} {}",
                        a.podcast,
                        a.episode,
                        a.position.unwrap(),
                        a.total.unwrap(),
                        a.started.unwrap()
                    );
                }
                Action::Download => {
                    println!("Download: {} - {}", a.podcast, a.episode);
                }
                Action::Delete => {
                    println!("Delete: {} - {}", a.podcast, a.episode);
                }
                Action::New => {
                    println!("New: {} - {}", a.podcast, a.episode);
                }
            }
        }

        let (added, removed) = self.get_subscription_changes()?;

        for sub in added {
            println!("Added: {sub}");
        }

        for sub in removed {
            println!("Removed: {sub}");
        }

        let subs = self.get_all_subscriptions()?;
        for sub in subs {
            println!("Subscription: {sub}");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ignore = "test is server dependent"]
    #[test]
    fn gpodder() {
        let config = Config::new(
            3,
            "http://localhost".to_string(),
            "device".to_string(),
            "user".to_string(),
            "pass".to_string(),
        );
        // pull changes from last week
        let timestamp = current_time().unwrap() - 7 * 24 * 60 * 60;
        let sync_agent = GpodderController::new(config, Some(timestamp));
        assert!(sync_agent.test_gpodder_api().is_ok());
    }
}
