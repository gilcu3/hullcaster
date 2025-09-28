use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use serde::{de::Visitor, ser::SerializeStruct, Deserialize, Deserializer, Serialize, Serializer};
use std::{cell::Cell, fmt};

#[derive(Debug)]
pub enum GpodderRequest {
    GetSubscriptionChanges,
    AddPodcast(String),
    RemovePodcast(String),
    MarkPlayed(String, String, i64, i64),
    MarkPlayedBatch(Vec<(String, String, i64, i64)>),
    Quit,
}

#[derive(Debug)]
pub enum GpodderMsg {
    SubscriptionChanges((Vec<String>, Vec<String>), Vec<EpisodeAction>, i64),
}

#[derive(Clone, Debug)]
pub struct Config {
    pub max_retries: usize,
    pub server: String,
    pub device: String,
    pub username: String,
    pub credentials: String,
}

#[derive(Clone, Debug)]
pub struct State {
    pub actions_timestamp: Cell<i64>,
    pub subscriptions_timestamp: Cell<i64>,
    pub logged_in: Cell<bool>,
}

#[derive(Deserialize, Debug)]
pub struct Device {
    pub id: String,
    pub caption: String,
    #[serde(rename = "type")]
    pub _type: String,
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
    pub timestamp: i64,
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
    pub timestamp: i64,
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
    pub timestamp: i64,
    pub started: Option<i64>,
    pub position: Option<i64>,
    pub total: Option<i64>,
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
            let dt = DateTime::parse_from_rfc3339(value)
                .map_err(|err| E::custom(format!("failed to parse date: {err}")))?;
            Ok(dt.timestamp())
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

impl Config {
    pub(crate) fn new(
        max_retries: usize, server: String, device: String, username: String, password: String,
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
