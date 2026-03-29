use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Visitor, ser::SerializeStruct};
use serde_json::Value;
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
    Error(String),
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
    #[serde(deserialize_with = "i64_to_u64_deserializer", default)]
    pub started: Option<u64>,
    #[serde(deserialize_with = "i64_to_u64_deserializer", default)]
    pub position: Option<u64>,
    #[serde(deserialize_with = "i64_to_u64_deserializer", default)]
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

fn i64_to_u64_deserializer<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Value::deserialize(deserializer)?;

    match v {
        Value::Number(n) => n.as_i64().map_or_else(
            || n.as_u64().map_or_else(|| Ok(None), |u| Ok(Some(u))),
            |i| {
                if i < 0 {
                    Ok(None)
                } else {
                    Ok(Some(i.cast_unsigned()))
                }
            },
        ),
        _ => Ok(None),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_play_action_full() {
        let json = r#"{
            "podcast": "https://example.com/feed.xml",
            "episode": "https://example.com/ep1.mp3",
            "action": "play",
            "timestamp": "2024-03-29T12:00:00Z",
            "started": 0,
            "position": 120,
            "total": 3600
        }"#;
        let action: EpisodeAction = serde_json::from_str(json).unwrap();
        assert_eq!(action.podcast, "https://example.com/feed.xml");
        assert_eq!(action.episode, "https://example.com/ep1.mp3");
        assert!(matches!(action.action, Action::Play));
        assert_eq!(action.started, Some(0));
        assert_eq!(action.position, Some(120));
        assert_eq!(action.total, Some(3600));
    }

    #[test]
    fn deserialize_action_types() {
        for (action_str, expected) in [
            ("new", "New"),
            ("download", "Download"),
            ("play", "Play"),
            ("delete", "Delete"),
        ] {
            let json = format!(
                r#"{{
                    "podcast": "https://example.com/feed.xml",
                    "episode": "https://example.com/ep.mp3",
                    "action": "{action_str}",
                    "timestamp": "2024-01-01T00:00:00Z"
                }}"#
            );
            let action: EpisodeAction = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{:?}", action.action), expected);
        }
    }

    #[test]
    fn deserialize_negative_position_becomes_none() {
        let json = r#"{
            "podcast": "https://example.com/feed.xml",
            "episode": "https://example.com/ep.mp3",
            "action": "play",
            "timestamp": "2024-01-01T00:00:00Z",
            "position": -1,
            "total": -1,
            "started": -100
        }"#;
        let action: EpisodeAction = serde_json::from_str(json).unwrap();
        assert_eq!(action.position, None);
        assert_eq!(action.total, None);
        assert_eq!(action.started, None);
    }

    #[test]
    fn deserialize_missing_optional_fields() {
        let json = r#"{
            "podcast": "https://example.com/feed.xml",
            "episode": "https://example.com/ep.mp3",
            "action": "download",
            "timestamp": "2024-06-15T08:30:00Z"
        }"#;
        let action: EpisodeAction = serde_json::from_str(json).unwrap();
        assert_eq!(action.position, None);
        assert_eq!(action.total, None);
        assert_eq!(action.started, None);
    }

    #[test]
    fn deserialize_timestamp_to_unix() {
        let json = r#"{
            "podcast": "p",
            "episode": "e",
            "action": "new",
            "timestamp": "2024-01-01T00:00:00Z"
        }"#;
        let action: EpisodeAction = serde_json::from_str(json).unwrap();
        // 2024-01-01T00:00:00Z = 1704067200 Unix seconds
        assert_eq!(action.timestamp, 1_704_067_200);
    }

    #[test]
    fn deserialize_invalid_timestamp_fails() {
        let json = r#"{
            "podcast": "p",
            "episode": "e",
            "action": "new",
            "timestamp": "not-a-date"
        }"#;
        assert!(serde_json::from_str::<EpisodeAction>(json).is_err());
    }

    #[test]
    fn deserialize_null_optional_field() {
        let json = r#"{
            "podcast": "p",
            "episode": "e",
            "action": "play",
            "timestamp": "2024-01-01T00:00:00Z",
            "position": null,
            "total": null
        }"#;
        let action: EpisodeAction = serde_json::from_str(json).unwrap();
        assert_eq!(action.position, None);
        assert_eq!(action.total, None);
    }

    #[test]
    fn serialize_roundtrip() {
        let json = r#"{
            "podcast": "https://example.com/feed.xml",
            "episode": "https://example.com/ep.mp3",
            "action": "play",
            "timestamp": "2024-01-01T00:00:00Z",
            "started": 0,
            "position": 120,
            "total": 3600
        }"#;
        let action: EpisodeAction = serde_json::from_str(json).unwrap();
        let serialized = serde_json::to_string(&action).unwrap();
        let deserialized: EpisodeAction = serde_json::from_str(&serialized).unwrap();

        assert_eq!(action.podcast, deserialized.podcast);
        assert_eq!(action.episode, deserialized.episode);
        assert_eq!(action.timestamp, deserialized.timestamp);
        assert_eq!(action.position, deserialized.position);
        assert_eq!(action.total, deserialized.total);
        assert_eq!(action.started, deserialized.started);
    }

    #[test]
    fn deserialize_podcast_changes() {
        let json = r#"{
            "add": ["https://example.com/feed1.xml", "https://example.com/feed2.xml"],
            "remove": ["https://example.com/old.xml"],
            "timestamp": 1704067200
        }"#;
        let changes: PodcastChanges = serde_json::from_str(json).unwrap();
        assert_eq!(changes.add.len(), 2);
        assert_eq!(changes.remove.len(), 1);
        assert_eq!(changes.timestamp, 1_704_067_200);
    }

    #[test]
    fn deserialize_device() {
        let json = r#"{
            "id": "my-device",
            "caption": "My Phone",
            "type": "mobile",
            "subscriptions": 42
        }"#;
        let device: Device = serde_json::from_str(json).unwrap();
        assert_eq!(device.id, "my-device");
        assert_eq!(device.caption, "My Phone");
        assert_eq!(device.dtype, "mobile");
        assert_eq!(device.subscriptions, 42);
    }

    #[test]
    fn config_encodes_credentials() {
        let config = Config::new(
            3,
            "https://gpodder.net".to_string(),
            "device".to_string(),
            "user".to_string(),
            "pass",
        );
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&config.credentials)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "user:pass");
    }

    #[test]
    fn deserialize_zero_position() {
        let json = r#"{
            "podcast": "p",
            "episode": "e",
            "action": "play",
            "timestamp": "2024-01-01T00:00:00Z",
            "position": 0
        }"#;
        let action: EpisodeAction = serde_json::from_str(json).unwrap();
        assert_eq!(action.position, Some(0));
    }

    #[test]
    fn deserialize_large_position() {
        let json = r#"{
            "podcast": "p",
            "episode": "e",
            "action": "play",
            "timestamp": "2024-01-01T00:00:00Z",
            "position": 999999999
        }"#;
        let action: EpisodeAction = serde_json::from_str(json).unwrap();
        assert_eq!(action.position, Some(999_999_999));
    }
}
