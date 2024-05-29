use anyhow::Result;
use base64::Engine;
use serde::de::Visitor;
use serde::ser::{Serialize, SerializeStruct, Serializer};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use ureq::{builder, Agent};

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
#[derive(Deserialize, Debug)]
pub enum Action {
    new,
    download,
    play,
    delete,
}

fn current_time() -> i64 {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    timestamp as i64
}

fn deserialize_date<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    struct GpodderDate;

    impl<'de> Visitor<'de> for GpodderDate {
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
    ) -> Option<GpodderController> {
        let agent_builder = builder()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30));
        let agent = agent_builder.build();
        let timestamp = timestamp.unwrap_or(0);
        let credentials = format!("{}:{}", config.sync_username, config.sync_password);
        let encoded_credentials = base64::engine::general_purpose::STANDARD.encode(credentials);

        Some(GpodderController {
            config,
            agent,
            actions_timestamp: timestamp.into(),
            subscriptions_timestamp: timestamp.into(),
            device_id,
            logged_in: false.into(),
            encoded_credentials,
        })
    }

    pub fn get_timestamp(&self) -> i64 {
        std::cmp::min(
            self.actions_timestamp.get(),
            self.subscriptions_timestamp.get(),
        )
    }

    pub fn init(&self) {
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
            self.register_device();
        }
    }

    pub fn mark_played(
        &self, podcast_url: &str, episode_url: &str, duration: Option<i64>, played: bool,
    ) -> Option<String> {
        duration?;
        self.require_login();
        let _url_mark_played = format!(
            "{}/api/2/episodes/{}/{}.json",
            self.config.sync_server, self.config.sync_username, self.device_id
        );
        let action = EpisodeAction {
            podcast: podcast_url.to_string(),
            episode: episode_url.to_string(),
            action: Action::play,
            timestamp: current_time(),
            started: Some(0),
            position: if played { duration } else { Some(0) },
            total: duration,
        };
        let actions = [action];
        let msg = serde_json::to_string(&actions).unwrap();

        let res = execute_request_post(
            &self.agent,
            _url_mark_played,
            msg,
            &self.encoded_credentials,
        );
        if res.is_some() {
            log::info!(
                "Marked played: {} episode: {} podcast: {}",
                played,
                episode_url,
                podcast_url
            );
        }
        res
    }

    pub fn mark_played_batch(&self, eps: Vec<(&str, &str, Option<i64>, bool)>) -> Option<String> {
        self.require_login();
        let _url_mark_played = format!(
            "{}/api/2/episodes/{}/{}.json",
            self.config.sync_server, self.config.sync_username, self.device_id
        );
        let actions: Vec<EpisodeAction> = eps
            .iter()
            .filter(|(_, _, duration, _)| duration.is_some())
            .map(
                |(podcast_url, episode_url, duration, played)| EpisodeAction {
                    podcast: podcast_url.to_string(),
                    episode: episode_url.to_string(),
                    action: Action::play,
                    timestamp: current_time(),
                    started: Some(0),
                    position: if *played { *duration } else { Some(0) },
                    total: *duration,
                },
            )
            .collect();
        let msg = serde_json::to_string(&actions).unwrap();

        let res = execute_request_post(
            &self.agent,
            _url_mark_played,
            msg,
            &self.encoded_credentials,
        );
        if res.is_some() {
            log::info!("Marked played: {} actions", actions.len());
        }
        res
    }

    pub fn get_episode_action_changes(&self) -> Option<Vec<EpisodeAction>> {
        self.require_login();
        let url_episode_action_changes = format!(
            "{}/api/2/episodes/{}.json",
            self.config.sync_server, self.config.sync_username
        );
        let since = self.actions_timestamp.get();
        let res = execute_request_get(
            &self.agent,
            url_episode_action_changes,
            vec![("since", since.to_string().as_str())],
            &self.encoded_credentials,
        );
        res.as_ref()?;
        let json_string = res.unwrap();
        let actions_r: serde_json::Result<Value> = serde_json::from_str(json_string.as_str());
        if let Ok(actions) = actions_r {
            let timestamp = actions["timestamp"].as_i64().unwrap();
            let episode_actions = actions["actions"].as_array().unwrap();
            let mut actions: Vec<EpisodeAction> = Vec::new();
            for action in episode_actions {
                let daction = serde_json::from_value::<EpisodeAction>(action.clone());
                actions.push(daction.unwrap());
            }
            self.actions_timestamp.set(timestamp);
            Some(actions)
        } else {
            None
        }
    }

    fn require_login(&self) -> bool {
        if !self.logged_in.get() {
            if self.login() {
                self.logged_in.set(true);
                self.init();
                true
            } else {
                false
            }
        } else {
            true
        }
    }

    fn login(&self) -> bool {
        let url_login = format!(
            "{}/api/2/auth/{}/login.json",
            self.config.sync_server, self.config.sync_username
        );
        let res = execute_request_post(
            &self.agent,
            url_login,
            String::new(),
            &self.encoded_credentials,
        );
        res.is_some()
    }

    // This is probably not implemented in micro-gpodder
    #[allow(dead_code)]
    fn logout(&self) {
        let url_login = format!(
            "{}/api/2/auth/{}/logout.json",
            self.config.sync_server, self.config.sync_username
        );
        execute_request_post(
            &self.agent,
            url_login,
            String::new(),
            &self.encoded_credentials,
        );
    }

    pub fn get_subscription_changes(&self) -> Option<(Vec<String>, Vec<String>)> {
        if self.subscriptions_timestamp.get() == 0 {
            let added = self.get_all_subscriptions()?;
            return Some((added, Vec::new()));
        }
        let url_subscription_changes = format!(
            "{}/api/2/subscriptions/{}/{}.json",
            self.config.sync_server, self.config.sync_username, self.device_id
        );
        let pasttime = self.subscriptions_timestamp.get().to_string();
        let params = vec![("since", pasttime.as_str())];
        let json_string = execute_request_get(
            &self.agent,
            url_subscription_changes,
            params,
            &self.encoded_credentials,
        )?;
        let parsed: serde_json::Result<PodcastChanges> = serde_json::from_str(json_string.as_str());
        if let Ok(changes) = parsed {
            for sub in &changes.add {
                log::info!("podcast added {}", sub);
            }
            for sub in &changes.remove {
                log::info!("podcast removed {}", sub);
            }
            Some((changes.add, changes.remove))
        } else {
            log::info!("Error parsing subscription changes");
            None
        }
    }

    fn get_all_subscriptions(&self) -> Option<Vec<String>> {
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
        let parsed: serde_json::Result<Vec<String>> = serde_json::from_str(json_string.as_str());

        if let Ok(subscriptions) = parsed {
            Some(subscriptions)
        } else {
            log::info!("Error parsing subscriptions");
            None
        }
    }

    fn get_devices(&self) -> Option<Vec<Device>> {
        let url_devices = format!(
            "{}/api/2/devices/{}.json",
            self.config.sync_server, self.config.sync_username
        );
        let res = execute_request_get(&self.agent, url_devices, vec![], &self.encoded_credentials);
        res.as_ref()?;
        let json_string = res.unwrap();
        let parsed: serde_json::Result<Vec<Device>> = serde_json::from_str(json_string.as_str());
        if let Ok(parsed_ok) = parsed {
            Some(parsed_ok)
        } else {
            None
        }
    }

    fn register_device(&self) -> bool {
        let url_register = format!(
            "{}/api/2/devices/{}/{}.json",
            self.config.sync_server, self.config.sync_username, self.device_id
        );
        let device = serde_json::json!({
            "caption": "",
            "type": "laptop"
        })
        .to_string();
        let res =
            execute_request_post(&self.agent, url_register, device, &self.encoded_credentials);
        log::info!("Registered device {}", self.device_id);
        res.is_some()
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
    pub fn testing(&self) -> Option<()> {
        let res = self.get_devices();
        let mut exists = false;
        for dev in res.unwrap() {
            println!("{:?}", dev);
            if dev.id == self.device_id {
                exists = true;
                break;
            }
        }
        if !exists {
            self.register_device();
        } else {
            println!("Device already exists");
        }

        // Not implemented in opodsync
        // get_sync_status();
        // set_sync_status();
        // get_device_updates();

        let actions = self.get_episode_action_changes();
        for a in actions.unwrap() {
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
            println!("Added: {}", sub);
        }

        for sub in removed {
            println!("Removed: {}", sub);
        }

        let subs = self.get_all_subscriptions()?;
        for sub in subs {
            println!("Subscription: {}", sub);
        }

        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::get_config_path;

    #[test]
    fn gpodder() {
        let config_path = get_config_path(None).unwrap();
        let config = Arc::new(Config::new(&config_path).unwrap());
        let mut db_path = config_path;
        db_path.pop();
        let sync_agent = if config.enable_sync {
            GpodderController::new(config.clone(), Some(0), "msigil".to_string())
        } else {
            None
        };
        assert!(sync_agent.unwrap().testing().is_some());
    }
}
