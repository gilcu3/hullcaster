use anyhow::Result;
use base64::Engine;
use serde::de::Visitor;
use serde::ser::{Serialize, SerializeStruct, Serializer};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::cell::Cell;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use ureq::{builder, Agent};
//use crate::opml;

use chrono::{DateTime, TimeZone, Utc};
use std::fmt;

use crate::config::Config;
use crate::utils::{execute_request_get, execute_request_post};

// #[derive(Debug)]
// pub struct Podcast {
//     url: String,
//     title: String,
//     description: String,
//     subscribers: i32,
//     logo_url: String,
//     website: String,
//     mygpo_link: String,
//     author: String
// }

// fn read_podcast_from_json_string(json_string: String) -> Option<Podcast> {
//     let parsed: serde_json::Result<Value> = serde_json::from_str(json_string.as_str());
//     if let Ok(parsed_ok) = parsed {
//         //let p = parsed_ok.as_object().unwrap();

//         let url = parsed_ok["url"].as_str().unwrap();
//         let title = parsed_ok["title"].as_str().unwrap();
//         let description = parsed_ok["description"].as_str().unwrap();
//         let subscribers = parsed_ok["subscribers"].as_i64().unwrap() as i32;
//         let logo_url = parsed_ok["logo_url"].as_str().unwrap();
//         let website = parsed_ok["website"].as_str().unwrap();
//         let mygpo_link = parsed_ok["mygpo_link"].as_str().unwrap();
//         let author = parsed_ok["author"].as_str().unwrap();
//         Some(Podcast{
//             url: url.to_string(),
//             title: title.to_string(),
//             description: description.to_string(),
//             subscribers,
//             logo_url: logo_url.to_string(),
//             website: website.to_string(),
//             mygpo_link: mygpo_link.to_string(),
//             author: author.to_string()
//         })
//     }
//     else{
//         None
//     }

// }

#[allow(non_camel_case_types)]
#[allow(dead_code)]
#[derive(Deserialize, Debug)]
pub struct Device {
    deviceid: String,
    caption: String,
    #[serde(rename = "type")]
    typed: String,
    subscriptions: u32,
    user: i32,
    id: i32,
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
    config: Config,
    agent: Agent,
    timestamp: Cell<i64>,
    logged_in: Cell<bool>,
    encoded_credentials: String,
}

impl GpodderController {
    // pub fn testing(&self){

    //     let res = self.get_devices();
    //     let mut exists = false;
    //     for dev in res.unwrap(){
    //         println!("{:?}", dev);
    //         if dev.deviceid == self.config.sync_device_id{
    //             exists = true;
    //             break;
    //         }
    //     }
    //     if !exists{
    //         self.register_device();
    //     }
    //     else{
    //         println!("Device already exists");
    //     }
    //     // self.get_subscription_changes(0);

    //     // Not implemented in micro-gpodder
    //     // get_all_subscriptions(&agent);
    //     // get_sync_status(&agent);
    //     // set_sync_status(&agent);
    //     // get_device_updates(&agent, 0, DEVICE_ID);

    //     let actions = self.get_episode_action_changes();
    //     for a in actions.unwrap(){
    //         match a.action {
    //             Action::play => {
    //                 println!("Play: {} - {} -> {} {} {}", a.podcast, a.episode, a.position.unwrap(), a.total.unwrap(), a.started.unwrap());
    //             },
    //             Action::download => {
    //                 println!("Download: {} - {}", a.podcast, a.episode);
    //             },
    //             Action::delete => {
    //                 println!("Delete: {} - {}", a.podcast, a.episode);
    //             },
    //             Action::new => {
    //                 println!("New: {} - {}", a.podcast, a.episode);
    //             }
    //         }
    //     }
    // }

    pub fn new(config: Config, timestamp: Option<i64>) -> Option<GpodderController> {
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
            timestamp: timestamp.into(),
            logged_in: false.into(),
            encoded_credentials,
        })
    }

    pub fn get_timestamp(&self) -> i64 {
        self.timestamp.get()
    }

    pub fn init(&self) {
        self.require_login();
        let res = self.get_devices();
        let mut exists = false;
        for dev in res.unwrap() {
            // println!("{:?}", dev);
            if dev.deviceid == self.config.sync_device_id {
                exists = true;
                break;
            }
        }
        if !exists {
            self.register_device();
        } else {
            // println!("Device already exists");
        }
    }

    pub fn mark_played(
        &self, podcast_url: &str, episode_url: &str, duration: Option<i64>, played: bool,
    ) -> Option<String> {
        duration?;
        self.require_login();
        let _url_mark_played = format!(
            "{}/api/2/episodes/{}/{}.json",
            self.config.sync_server, self.config.sync_username, self.config.sync_device_id
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

    pub fn get_episode_action_changes(&self) -> Option<Vec<EpisodeAction>> {
        self.require_login();
        let url_episode_action_changes = format!(
            "{}/api/2/episodes/{}.json",
            self.config.sync_server, self.config.sync_username
        );
        let since = self.timestamp.get();
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
            self.timestamp.set(timestamp);
            Some(actions)
        } else {
            None
        }
    }

    fn require_login(&self) -> bool {
        if !self.logged_in.get() {
            if self.login() {
                self.logged_in.set(true);
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

    // fn logout(&self){
    //     let url_login = format!("{}/api/2/auth/{}/logout.json", self.config.sync_server, self.config.sync_username);
    //     execute_request_post(&self.agent, url_login, String::new(), &self.encoded_credentials);
    // }

    // fn get_subscription_changes(&self){
    //     let url_subscription_changes = format!("{}/api/2/subscriptions/{}/{}.json", self.config.sync_server, self.config.sync_username, self.config.sync_device_id);
    //     let pasttime = self.timestamp.get().to_string();
    //     let params = vec![("since", pasttime.as_str())];
    //     let res = execute_request_get(&self.agent, url_subscription_changes, params, &self.encoded_credentials);
    //     if res.is_none(){
    //         return;
    //     }
    //     let json_string = res.unwrap();
    //     let parsed: serde_json::Result<Value> = serde_json::from_str(json_string.as_str());
    //     if let Ok(parsed_ok) = parsed {
    //         let timestamp = parsed_ok["timestamp"].as_i64().unwrap();
    //         //println!("Timestamp: {}", timestamp);
    //         let subscriptions_add = parsed_ok["add"].as_array().unwrap();
    //         for sub in subscriptions_add{
    //             //println!("{}", sub);
    //         }
    //         //println!("Add: {}", subscriptions_add.len());
    //         let subscriptions_remove = parsed_ok["remove"].as_array().unwrap();
    //         for sub in subscriptions_remove{
    //             //println!("{}", sub);
    //         }
    //         //println!("Remove: {}", subscriptions_remove.len());
    //         let subscriptions_update_urls = parsed_ok["update_urls"].as_array().unwrap();
    //         for sub in subscriptions_update_urls{
    //             //println!("{}", sub);
    //         }
    //         // println!("Update URLs: {}", subscriptions_update_urls.len());

    //     }
    // }

    // fn get_all_subscriptions(&self){
    //     let url_subscription_changes = format!("{}/subscriptions/{}.json", self.config.sync_server, self.config.sync_username);
    //     let res = execute_request_get(&self.agent, url_subscription_changes, vec![], &self.encoded_credentials);
    //     if res.is_none(){
    //         return;
    //     }
    //     let opml_string = res.unwrap();

    //     if let Ok(subscriptions) = opml::import(opml_string) {
    //         // for sub in subscriptions{
    //         //     println!("{:?}", sub);
    //         // }
    //         //println!("Parsed {} subscriptions", subscriptions.len());
    //     }
    //     else{
    //         //println!("Error parsing subscriptions");
    //     }

    // }

    fn get_devices(&self) -> Option<Vec<Device>> {
        let url_devices = format!(
            "{}/api/2/devices/{}.json",
            self.config.sync_server, self.config.sync_username
        );
        let res = execute_request_get(&self.agent, url_devices, vec![], &self.encoded_credentials);
        res.as_ref()?;
        let json_string = res.unwrap();
        // println!("{}", json_string);
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
            self.config.sync_server, self.config.sync_username, self.config.sync_device_id
        );
        let device = serde_json::json!({
            "caption": self.config.sync_device_caption,
            "type": "laptop"
        })
        .to_string();
        let res =
            execute_request_post(&self.agent, url_register, device, &self.encoded_credentials);
        log::info!("Registered device {}", self.config.sync_device_id);
        res.is_some()
    }

    // fn get_device_updates(&self, since: u64, device_id: &str){
    //     let url_device_updates = format!("{}/api/2/updates/{}/{}.json", self.config.sync_server, self.config.sync_username, device_id);
    //     let json_string = execute_request_get(&self.agent, url_device_updates, vec![("since", since.to_string().as_str())], &self.encoded_credentials);
    //     //println!("{}", json_string);
    // }

    // fn get_sync_status(&self){
    //     let url_sync_status = format!("{}/api/2/sync-devices/{}.json", self.config.sync_server, self.config.sync_username);
    //     let json_string = execute_request_get(&self.agent, url_sync_status, vec![], &self.encoded_credentials);
    //     //println!("{}", json_string);
    // }

    // fn set_sync_status(&self){
    //     let url_sync_status = format!("{}/api/2/sync-devices/{}.json", self.config.sync_server, self.config.sync_username);
    //     let dev_sync = serde_json::json!({
    //         "synchronize": [["podcini", "rey-work-laptop"]],
    //         "stop-synchronize": []
    //     }).to_string();
    //     //println!("{}", dev_sync);
    //     let json_string = execute_request_post(&self.agent, url_sync_status, dev_sync, &self.encoded_credentials);
    //     //println!("{}", json_string);
    // }
}
