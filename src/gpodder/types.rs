use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Visitor, ser::SerializeStruct};
use std::{fmt, sync::RwLock};

#[derive(Debug)]
pub enum GpodderRequest {
    GetSubscriptionChanges,
    AddPodcast(String),
    RemovePodcast(String),
    MarkPlayed(String, String, u64, u64),
    MarkPlayedBatch(Vec<(String, String, u64, u64)>),
    Quit,
}

#[derive(Debug)]
pub enum GpodderMsg {
    SubscriptionChanges((Vec<String>, Vec<String>), Vec<EpisodeAction>, u64),
}

#[derive(Clone, Debug)]
pub struct Config {
    pub max_retries: usize,
    pub server: String,
    pub device: String,
    pub username: String,
    pub credentials: String,
}

#[derive(Debug)]
pub struct State {
    pub actions_timestamp: RwLock<u64>,
    pub subscriptions_timestamp: RwLock<u64>,
    pub logged_in: RwLock<bool>,
}

#[derive(Deserialize, Debug)]
pub struct Device {
    pub id: String,
    pub caption: String,
    #[serde(rename = "type")]
    pub dtype: String,
    pub subscriptions: u32,
    // These are not specified in gpodder API
    #[allow(dead_code)]
    pub user: Option<i32>,
    #[allow(dead_code)]
    pub deviceid: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct PodcastChanges {
    pub add: Vec<String>,
    pub remove: Vec<String>,
    pub timestamp: u64,
    #[allow(unused)]
    pub update_urls: Option<Vec<String>>,
}

#[derive(Deserialize, Debug)]
pub struct Podcast {
    pub feed: String,
    #[allow(dead_code)]
    pub title: Option<String>,
    #[allow(dead_code)]
    pub website: Option<String>,
    #[allow(dead_code)]
    pub description: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct UploadPodcastChanges {
    pub update_urls: Vec<Vec<String>>,
    pub timestamp: u64,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    New,
    Download,
    Play,
    Delete,
}

#[derive(Deserialize, Debug)]
pub struct EpisodeAction {
    pub podcast: String,
    pub episode: String,
    pub action: Action,
    #[serde(deserialize_with = "deserialize_date")]
    pub timestamp: u64,
    pub started: Option<u64>,
    pub position: Option<u64>,
    pub total: Option<u64>,
}

fn deserialize_date<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct GpodderDate;

    impl Visitor<'_> for GpodderDate {
        type Value = u64;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a date string in the format YYYY-MM-DD")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let dt = DateTime::parse_from_rfc3339(value)
                .map_err(|err| E::custom(format!("failed to parse date: {err}")))?;
            dt.timestamp()
                .try_into()
                .map_err(|err| E::custom(format!("failed to parse date: {err}")))
        }
    }

    deserializer.deserialize_str(GpodderDate)
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
            Action::New => "new",
            Action::Download => "download",
            Action::Play => "play",
            Action::Delete => "delete",
        };
        state.serialize_field("action", action)?;
        let datetime = Utc.timestamp_opt(
            self.timestamp
                .try_into()
                .map_err(|_| serde::ser::Error::custom("timestamp conversion error"))?,
            0,
        );
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

impl Config {
    pub(crate) fn new(
        max_retries: usize, server: String, device: String, username: String, password: &str,
    ) -> Self {
        let credentials =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        Self {
            max_retries,
            server,
            device,
            username,
            credentials,
        }
    }
}
