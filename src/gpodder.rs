use anyhow::{anyhow, Result};
use base64::Engine;
use serde::de::Visitor;
use serde::ser::{Serialize, SerializeStruct, Serializer};
use serde::{Deserialize, Deserializer};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use ureq::Agent;

use chrono::{DateTime, TimeZone, Utc};
use std::fmt;

use crate::config::Config;
use crate::utils::{execute_request_get, execute_request_post};

#[allow(non_camel_case_types)]
#[derive(Deserialize, Debug)]
struct Device {
    id: String,
    caption: String,
    #[serde(rename = "type")]
    typed: String,
    subscriptions: u32,
    // this are not specified in gpodder API
    #[allow(dead_code)]
    user: Option<i32>,
    #[allow(dead_code)]
    deviceid: Option<String>,
}

#[allow(non_camel_case_types)]
#[allow(dead_code)]
#[derive(Deserialize, Debug)]
struct PodcastChanges {
    add: Vec<String>,
    remove: Vec<String>,
    timestamp: i64,
    // I dont know where this came from
    update_urls: Option<Vec<String>>,
}

#[allow(non_camel_case_types)]
#[allow(dead_code)]
#[derive(Deserialize, Debug)]
struct Podcast {
    feed: String,
    title: Option<String>,
    website: Option<String>,
    description: Option<String>,
}

#[allow(non_camel_case_types)]
#[allow(dead_code)]
#[derive(Deserialize, Debug)]
struct UploadPodcastChanges {
    update_urls: Vec<Vec<String>>,
    timestamp: i64,
}

#[allow(non_camel_case_types)]
#[derive(Deserialize, Debug)]
pub enum Action {
    new,
    download,
    play,
    delete,
}

fn current_time() -> Result<i64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64)
}

fn deserialize_date<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    struct GpodderDate;

    impl Visitor<'_> for GpodderDate {
        type Value = i64;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a date string in the format YYYY-MM-DD")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let dt = DateTime::parse_from_rfc3339(value).unwrap();
            Ok(dt.timestamp())
        }
    }

    deserializer.deserialize_str(GpodderDate)
}

#[derive(Deserialize, Debug)]
pub struct EpisodeAction {
    pub podcast: String,
    pub episode: String,
    pub action: Action,
    #[serde(deserialize_with = "deserialize_date")]
    pub timestamp: i64,
    pub started: Option<i64>,
    pub position: Option<i64>,
    pub total: Option<i64>,
}

impl Serialize for EpisodeAction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("EpisodeAction", 6)?;
        state.serialize_field("podcast", &self.podcast)?;
        state.serialize_field("episode", &self.episode)?;
        let action = match self.action {
            Action::new => "new",
            Action::download => "download",
            Action::play => "play",
            Action::delete => "delete",
        };
        state.serialize_field("action", action)?;
        let datetime = Utc.timestamp_opt(self.timestamp, 0);
        let datetime_str = datetime
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        state.serialize_field("timestamp", datetime_str.as_str())?;
        state.serialize_field("started", &self.started)?;
        state.serialize_field("position", &self.position)?;
        state.serialize_field("total", &self.total)?;
        state.end()
    }
}

#[derive(Clone, Debug)]
pub struct GpodderController {
    config: Arc<Config>,
    agent: Agent,
    actions_timestamp: Cell<i64>,
    subscriptions_timestamp: Cell<i64>,
    device_id: String,
    logged_in: Cell<bool>,
    encoded_credentials: String,
}

impl GpodderController {
    pub fn new(
        config: Arc<Config>, timestamp: Option<i64>, device_id: String,
    ) -> GpodderController {
        let agent_builder = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(30)));
        let agent = agent_builder.build().into();
        let timestamp = timestamp.unwrap_or(0);
        let credentials = format!("{}:{}", config.sync_username, config.sync_password);
        let encoded_credentials = base64::engine::general_purpose::STANDARD.encode(credentials);

        GpodderController {
            config,
            agent,
            actions_timestamp: timestamp.into(),
            subscriptions_timestamp: timestamp.into(),
            device_id,
            logged_in: false.into(),
            encoded_credentials,
        }
    }

    pub fn get_timestamp(&self) -> i64 {
        std::cmp::min(
            self.actions_timestamp.get(),
            self.subscriptions_timestamp.get(),
        )
    }

    pub fn init(&self) -> Result<()> {
        let res = self.get_devices();
        let mut exists = false;
        for dev in res.unwrap() {
            if dev.id == self.device_id {
                log::info!(
                    "Using device: id = {}, type = {}, subscriptions = {}, caption = {}",
                    dev.id,
                    dev.typed,
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
            self.config.sync_server, self.config.sync_username, self.device_id
        );
        let action = EpisodeAction {
            podcast: podcast_url.to_string(),
            episode: episode_url.to_string(),
            action: Action::play,
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
            &self.encoded_credentials,
        )?;
        log::info!("Marked position: {position} episode: {episode_url} podcast: {podcast_url}");
        Ok(result)
    }

    pub fn mark_played_batch(&self, eps: Vec<(&str, &str, i64, Option<i64>)>) -> Result<String> {
        self.require_login()?;
        let _url_mark_played = format!(
            "{}/api/2/episodes/{}/{}.json",
            self.config.sync_server, self.config.sync_username, self.device_id
        );
        let actions: Vec<EpisodeAction> = eps
            .iter()
            .filter_map(|(podcast_url, episode_url, position, duration)| {
                Some(EpisodeAction {
                    podcast: podcast_url.to_string(),
                    episode: episode_url.to_string(),
                    action: Action::play,
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
            &self.encoded_credentials,
        )?;
        log::info!("Marked played: {} actions", actions.len());
        Ok(result)
    }

    pub fn get_episode_action_changes(&self) -> Result<Vec<EpisodeAction>> {
        self.require_login()?;
        let url_episode_action_changes = format!(
            "{}/api/2/episodes/{}.json",
            self.config.sync_server, self.config.sync_username
        );
        let since = self.actions_timestamp.get();
        let json_string = execute_request_get(
            &self.agent,
            url_episode_action_changes,
            vec![("since", since.to_string().as_str())],
            &self.encoded_credentials,
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
        self.actions_timestamp.set(timestamp + 1);
        Ok(actions)
    }

    fn require_login(&self) -> Result<()> {
        if !self.logged_in.get() {
            self.login()?;
            self.logged_in.set(true);
            self.init()?;
        }
        Ok(())
    }

    fn login(&self) -> Result<()> {
        let url_login = format!(
            "{}/api/2/auth/{}/login.json",
            self.config.sync_server, self.config.sync_username
        );
        execute_request_post(
            &self.agent,
            url_login,
            String::new(),
            &self.encoded_credentials,
        )?;
        Ok(())
    }

    // This is probably not implemented in micro-gpodder
    #[allow(dead_code)]
    fn logout(&self) -> Result<()> {
        let url_login = format!(
            "{}/api/2/auth/{}/logout.json",
            self.config.sync_server, self.config.sync_username
        );
        execute_request_post(
            &self.agent,
            url_login,
            String::new(),
            &self.encoded_credentials,
        )?;
        Ok(())
    }

    pub fn get_subscription_changes(&self) -> Result<(Vec<String>, Vec<String>)> {
        if self.subscriptions_timestamp.get() == 0 {
            let added = self.get_all_subscriptions()?;
            return Ok((added, Vec::new()));
        }
        let url_subscription_changes = format!(
            "{}/api/2/subscriptions/{}/{}.json",
            self.config.sync_server, self.config.sync_username, self.device_id
        );
        let pastime = self.subscriptions_timestamp.get().to_string();
        let params = vec![("since", pastime.as_str())];
        let json_string = execute_request_get(
            &self.agent,
            url_subscription_changes,
            params,
            &self.encoded_credentials,
        )?;
        let parsed: serde_json::Result<PodcastChanges> = serde_json::from_str(json_string.as_str());
        if let Ok(changes) = parsed {
            for sub in &changes.add {
                log::info!("podcast added {sub}");
            }
            for sub in &changes.remove {
                log::info!("podcast removed {sub}");
            }
            self.subscriptions_timestamp.set(changes.timestamp + 1);
            Ok((changes.add, changes.remove))
        } else {
            Err(anyhow!("Error parsing subscription changes"))
        }
    }

    pub fn upload_subscription_changes(&self, changes: (Vec<&String>, Vec<&String>)) -> Result<()> {
        let url_upload_subscriptions = format!(
            "{}/api/2/subscriptions/{}/{}.json",
            self.config.sync_server, self.config.sync_username, self.device_id
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
            &self.encoded_credentials,
        )?;
        let parsed: serde_json::Result<UploadPodcastChanges> =
            serde_json::from_str(json_string.as_str());
        if let Ok(changes) = parsed {
            for sub in &changes.update_urls {
                log::info!("url changed {} {}", sub[0], sub[1]);
            }
            self.subscriptions_timestamp.set(changes.timestamp + 1);
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
            self.config.sync_server, self.config.sync_username
        );
        let json_string = execute_request_get(
            &self.agent,
            url_subscription_changes,
            vec![],
            &self.encoded_credentials,
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
            self.config.sync_server, self.config.sync_username
        );
        let json_string =
            execute_request_get(&self.agent, url_devices, vec![], &self.encoded_credentials)?;
        Ok(serde_json::from_str(json_string.as_str())?)
    }

    fn register_device(&self) -> Result<()> {
        let url_register = format!(
            "{}/api/2/devices/{}/{}.json",
            self.config.sync_server, self.config.sync_username, self.device_id
        );
        let device = serde_json::json!({
            "caption": "",
            "type": "laptop"
        })
        .to_string();
        execute_request_post(&self.agent, url_register, device, &self.encoded_credentials)?;
        log::info!("Registered device {}", self.device_id);
        Ok(())
    }

    #[allow(dead_code)]
    fn get_device_updates(&self) {
        let url_device_updates = format!(
            "{}/api/2/updates/{}/{}.json",
            self.config.sync_server, self.config.sync_username, self.device_id
        );
        let _json_string = execute_request_get(
            &self.agent,
            url_device_updates,
            vec![("since", self.actions_timestamp.get().to_string().as_str())],
            &self.encoded_credentials,
        );
    }
    #[allow(dead_code)]
    fn get_sync_status(&self) {
        let url_sync_status = format!(
            "{}/api/2/sync-devices/{}.json",
            self.config.sync_server, self.config.sync_username
        );
        let _json_string = execute_request_get(
            &self.agent,
            url_sync_status,
            vec![],
            &self.encoded_credentials,
        );
    }

    // this is WIP
    #[allow(dead_code)]
    fn set_sync_status(&self) {
        let url_sync_status = format!(
            "{}/api/2/sync-devices/{}.json",
            self.config.sync_server, self.config.sync_username
        );
        let dev_sync = serde_json::json!({
            "synchronize": [["dev1", "dev2"]],
            "stop-synchronize": [] })
        .to_string();

        let _json_string = execute_request_post(
            &self.agent,
            url_sync_status,
            dev_sync,
            &self.encoded_credentials,
        );
    }

    #[cfg(test)]
    pub fn test_gpodder_api(&self) -> Result<()> {
        let res = self.get_devices();
        let mut exists = false;
        for dev in res? {
            println!("{dev:?}");
            if dev.id == self.device_id {
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
                Action::play => {
                    println!(
                        "Play: {} - {} -> {} {} {}",
                        a.podcast,
                        a.episode,
                        a.position.unwrap(),
                        a.total.unwrap(),
                        a.started.unwrap()
                    );
                }
                Action::download => {
                    println!("Download: {} - {}", a.podcast, a.episode);
                }
                Action::delete => {
                    println!("Delete: {} - {}", a.podcast, a.episode);
                }
                Action::new => {
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
    use crate::get_config_path;

    #[ignore = "test is server dependent"]
    #[test]
    fn gpodder() {
        let config_path = get_config_path(None).unwrap();
        let config = Arc::new(Config::new(&config_path).unwrap());
        let mut db_path = config_path;
        db_path.pop();
        // pull changes from last week
        let timestamp = current_time().unwrap() - 7 * 24 * 60 * 60;
        if config.enable_sync {
            let sync_agent = GpodderController::new(config.clone(), Some(timestamp), "msigil".to_string());
            assert!(sync_agent.test_gpodder_api().is_ok());
        }
    }
}
