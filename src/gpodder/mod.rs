use anyhow::{Result, anyhow};
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::TICK_RATE;
use crate::types::Message;

use self::net::{execute_request_get, execute_request_post};
use self::types::{Device, PodcastChanges, State};

pub use self::types::{
    Action, Config, EpisodeAction, EpisodePlayedData, GpodderMsg, GpodderRequest,
    SubscriptionChanges,
};
mod net;
mod types;

fn current_time() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn send_error(tx: &Sender<Message>, msg: String) {
    if tx.send(Message::Gpodder(GpodderMsg::Error(msg))).is_err() {
        log::error!("Failed to send gpodder error: channel closed");
    }
}

#[derive(Debug)]
pub struct GpodderController {
    config: Config,
    client: reqwest::Client,
    state: State,
}

impl GpodderController {
    fn new(config: Config, timestamp: Option<u64>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_mins(2))
            .build()
            .expect("Could not build reqwest::Client");

        let timestamp = timestamp.unwrap_or(0);
        let state = State {
            actions_timestamp: timestamp.into(),
            subscriptions_timestamp: timestamp.into(),
            logged_in: false.into(),
        };
        Self {
            config,
            client,
            state,
        }
    }

    pub async fn spawn_async(
        rx_from_app: Receiver<GpodderRequest>, tx_to_app: Sender<Message>, config: Config,
        timestamp: Option<u64>,
    ) {
        let sync_client = Self::new(config, timestamp);
        loop {
            if let Ok(message) = rx_from_app.try_recv() {
                match message {
                    GpodderRequest::GetSubscriptionChanges => {
                        let subscription_changes = sync_client
                            .get_subscription_changes()
                            .await
                            .unwrap_or_else(|err| {
                                log::error!("Failed to get subscription changes: {err}");
                                send_error(&tx_to_app, format!("Gpodder sync failed: {err}"));
                                // Not a first sync: an empty list from a failed
                                // fetch must not look like an empty server.
                                SubscriptionChanges::default()
                            });
                        // Send subscription changes only; episode actions are
                        // fetched in a second phase (GetEpisodeActions) once the
                        // newly-added podcasts' feeds have been ingested, so their
                        // episodes exist before actions are applied.
                        if tx_to_app
                            .send(Message::Gpodder(GpodderMsg::SubscriptionChanges(
                                subscription_changes,
                            )))
                            .is_err()
                        {
                            log::error!("Failed to send gpodder message: channel closed");
                            break;
                        }
                    }
                    GpodderRequest::GetEpisodeActions => {
                        let episode_actions = sync_client
                            .get_episode_action_changes()
                            .await
                            .unwrap_or_else(|err| {
                                log::error!("Failed to get episode action changes: {err:?}");
                                send_error(
                                    &tx_to_app,
                                    format!("Gpodder episode sync failed: {err}"),
                                );
                                Vec::new()
                            });
                        let timestamp = sync_client.get_timestamp();
                        if tx_to_app
                            .send(Message::Gpodder(GpodderMsg::EpisodeActions(
                                episode_actions,
                                timestamp,
                            )))
                            .is_err()
                        {
                            log::error!("Failed to send gpodder message: channel closed");
                            break;
                        }
                    }
                    GpodderRequest::UploadSubscriptionChanges { add, remove } => {
                        if let Err(err) =
                            sync_client.upload_subscription_changes(&add, &remove).await
                        {
                            log::error!(
                                "Failed to upload subscription changes (add: {add:?}, remove: {remove:?}): {err}"
                            );
                            send_error(
                                &tx_to_app,
                                "Gpodder: failed to upload subscription changes".to_string(),
                            );
                        }
                    }
                    GpodderRequest::MarkPlayed(data) => {
                        let episode_url = data.episode_url.clone();
                        if let Err(err) = sync_client.mark_played_batch(vec![data]).await {
                            log::error!("Failed to mark played {episode_url}: {err}");
                            send_error(
                                &tx_to_app,
                                format!("Gpodder: failed to sync position for {episode_url}"),
                            );
                        }
                    }
                    GpodderRequest::MarkPlayedBatch(episodes) => {
                        if let Err(err) = sync_client.mark_played_batch(episodes).await {
                            log::error!("Failed to mark episodes as played: {err}");
                            send_error(
                                &tx_to_app,
                                "Gpodder: failed to sync episode positions".to_string(),
                            );
                        }
                    }
                    GpodderRequest::Quit => break,
                }
            }
            tokio::time::sleep(Duration::from_millis(TICK_RATE)).await;
        }
    }

    pub fn get_timestamp(&self) -> u64 {
        std::cmp::min(
            *self
                .state
                .actions_timestamp
                .read()
                .expect("RwLock read should not fail"),
            *self
                .state
                .subscriptions_timestamp
                .read()
                .expect("RwLock read should not fail"),
        )
    }

    async fn init(&self) -> Result<()> {
        let res = self.get_devices().await?;
        let mut exists = false;
        for dev in res {
            if dev.id == self.config.device {
                log::debug!(
                    "Using device: id = {}, type = {}, subscriptions = {}, caption = {}",
                    dev.id,
                    dev.dtype,
                    dev.subscriptions,
                    dev.caption
                );
                exists = true;
                break;
            }
        }
        if exists {
            Ok(())
        } else {
            self.register_device().await
        }
    }

    /// The episode actions endpoint. The gpodder API has no per-device variant
    /// for uploads; the device goes into each action instead.
    fn episodes_url(&self) -> String {
        format!(
            "{}/api/2/episodes/{}.json",
            self.config.server, self.config.username
        )
    }

    pub async fn mark_played_batch(&self, eps: Vec<EpisodePlayedData>) -> Result<String> {
        self.require_login().await?;
        let timestamp = current_time()?;
        let actions: Vec<EpisodeAction> = eps
            .into_iter()
            .map(|ep| EpisodeAction {
                podcast: ep.podcast_url,
                episode: ep.episode_url,
                device: Some(self.config.device.clone()),
                guid: (!ep.guid.is_empty()).then_some(ep.guid),
                action: Action::Play,
                timestamp: Some(timestamp),
                started: Some(0),
                position: Some(ep.position),
                total: Some(ep.duration),
            })
            .collect();
        let msg = serde_json::to_string(&actions)?;

        let result = execute_request_post(
            &self.client,
            self.episodes_url(),
            msg,
            &self.config.credentials,
            self.config.max_retries,
        )
        .await?;
        log::debug!("Marked played: {} actions", actions.len());
        Ok(result)
    }

    pub async fn get_episode_action_changes(&self) -> Result<Vec<EpisodeAction>> {
        self.require_login().await?;
        let since = *self
            .state
            .actions_timestamp
            .read()
            .expect("RwLock read should not fail");
        let json_string = execute_request_get(
            &self.client,
            self.episodes_url(),
            vec![("since", since.to_string().as_str())],
            &self.config.credentials,
            self.config.max_retries,
        )
        .await?;
        let actions: serde_json::Value = serde_json::from_str(json_string.as_str())?;
        let timestamp = actions["timestamp"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Parsing timestamp failed"))?;
        let episode_actions = actions["actions"]
            .as_array()
            .ok_or_else(|| anyhow!("Failed to get actions"))?;
        let mut actions: Vec<EpisodeAction> = Vec::new();
        for action in episode_actions {
            match serde_json::from_value::<EpisodeAction>(action.clone()) {
                Ok(action) => actions.push(action),
                Err(err) => log::warn!("Skipping unparsable episode action ({err}): {action}"),
            }
        }
        *self
            .state
            .actions_timestamp
            .write()
            .expect("RwLock write should not fail") = timestamp;
        Ok(actions)
    }

    async fn require_login(&self) -> Result<()> {
        if !*self
            .state
            .logged_in
            .read()
            .expect("RwLock read should not fail")
        {
            self.login().await?;
            *self
                .state
                .logged_in
                .write()
                .expect("RwLock write should not fail") = true;
            self.init().await?;
        }
        Ok(())
    }

    async fn login(&self) -> Result<()> {
        let url_login = format!(
            "{}/api/2/auth/{}/login.json",
            self.config.server, self.config.username
        );
        execute_request_post(
            &self.client,
            url_login,
            String::new(),
            &self.config.credentials,
            self.config.max_retries,
        )
        .await?;
        Ok(())
    }

    // This is probably not implemented in micro-gpodder
    #[allow(dead_code)]
    async fn logout(&self) -> Result<()> {
        let url_login = format!(
            "{}/api/2/auth/{}/logout.json",
            self.config.server, self.config.username
        );
        execute_request_post(
            &self.client,
            url_login,
            String::new(),
            &self.config.credentials,
            self.config.max_retries,
        )
        .await?;
        Ok(())
    }

    /// The subscriptions endpoint of this device, for both fetching and
    /// uploading changes.
    fn subscriptions_url(&self) -> String {
        format!(
            "{}/api/2/subscriptions/{}/{}.json",
            self.config.server, self.config.username, self.config.device
        )
    }

    /// Fetches the subscription changes since the last sync. The first sync
    /// uses `since=0`, which every server answers with all subscriptions.
    pub async fn get_subscription_changes(&self) -> Result<SubscriptionChanges> {
        self.require_login().await?;
        let since = *self
            .state
            .subscriptions_timestamp
            .read()
            .expect("RwLock read should not fail");
        let json_string = execute_request_get(
            &self.client,
            self.subscriptions_url(),
            vec![("since", since.to_string().as_str())],
            &self.config.credentials,
            self.config.max_retries,
        )
        .await?;
        let changes: PodcastChanges = serde_json::from_str(json_string.as_str())
            .map_err(|err| anyhow!("Error parsing subscription changes: {err}"))?;
        for sub in &changes.add {
            log::info!("podcast added {sub}");
        }
        for sub in &changes.remove {
            log::info!("podcast removed {sub}");
        }
        // The API says to reuse the returned timestamp as is
        *self
            .state
            .subscriptions_timestamp
            .write()
            .expect("RwLock write should not fail") = changes.timestamp;
        Ok(SubscriptionChanges {
            add: changes.add,
            remove: changes.remove,
            first_sync: since == 0,
        })
    }

    /// Uploads local subscription changes. The timestamp of the response is
    /// not used as the next `since`: changes other devices made since our
    /// last fetch would be skipped, and fetching our own changes back is
    /// harmless.
    pub async fn upload_subscription_changes(
        &self, add: &[String], remove: &[String],
    ) -> Result<()> {
        self.require_login().await?;
        let json_changes = serde_json::json!({
            "add": add,
            "remove": remove
        })
        .to_string();
        execute_request_post(
            &self.client,
            self.subscriptions_url(),
            json_changes,
            &self.config.credentials,
            self.config.max_retries,
        )
        .await?;
        Ok(())
    }

    async fn get_devices(&self) -> Result<Vec<Device>> {
        let url_devices = format!(
            "{}/api/2/devices/{}.json",
            self.config.server, self.config.username
        );
        let json_string = execute_request_get(
            &self.client,
            url_devices,
            vec![],
            &self.config.credentials,
            self.config.max_retries,
        )
        .await?;
        Ok(serde_json::from_str(json_string.as_str())?)
    }

    async fn register_device(&self) -> Result<()> {
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
            &self.client,
            url_register,
            device,
            &self.config.credentials,
            self.config.max_retries,
        )
        .await?;
        log::info!("Registered device {}", self.config.device);
        Ok(())
    }

    #[allow(dead_code)]
    async fn get_device_updates(&self) {
        let url_device_updates = format!(
            "{}/api/2/updates/{}/{}.json",
            self.config.server, self.config.username, self.config.device
        );
        let actions_timestamp = *self
            .state
            .actions_timestamp
            .read()
            .expect("RwLock read should not fail");
        let _json_string = execute_request_get(
            &self.client,
            url_device_updates,
            vec![("since", actions_timestamp.to_string().as_str())],
            &self.config.credentials,
            self.config.max_retries,
        )
        .await;
    }
    #[allow(dead_code)]
    fn get_sync_status(&self) {
        let url_sync_status = format!(
            "{}/api/2/sync-devices/{}.json",
            self.config.server, self.config.username
        );
        let _json_string = execute_request_get(
            &self.client,
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
            &self.client,
            url_sync_status,
            dev_sync,
            &self.config.credentials,
            self.config.max_retries,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, body_partial_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_config(server_url: &str) -> Config {
        Config::new(
            1,
            server_url.to_string(),
            "testdevice".to_string(),
            "testuser".to_string(),
            "testpass",
        )
    }

    /// Sets up login + device registration mocks (required by `require_login`/`init`).
    async fn mock_login_and_init(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api/2/auth/testuser/login.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .expect(1)
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/2/devices/testuser.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "id": "testdevice",
                    "caption": "Test",
                    "type": "laptop",
                    "subscriptions": 0
                }
            ])))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn login_and_init_existing_device() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        let config = test_config(&server.uri());
        let controller = GpodderController::new(config, Some(100));

        controller.require_login().await.unwrap();
        assert!(
            *controller
                .state
                .logged_in
                .read()
                .expect("RwLock read should not fail")
        );
    }

    #[tokio::test]
    async fn init_registers_missing_device() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/2/auth/testuser/login.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .expect(1)
            .mount(&server)
            .await;

        // Return empty device list so registration is triggered
        Mock::given(method("GET"))
            .and(path("/api/2/devices/testuser.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/2/devices/testuser/testdevice.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let controller = GpodderController::new(config, Some(100));
        controller.require_login().await.unwrap();
    }

    #[tokio::test]
    async fn get_subscription_changes_incremental() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        Mock::given(method("GET"))
            .and(path("/api/2/subscriptions/testuser/testdevice.json"))
            .and(query_param("since", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "add": ["https://example.com/feed1.xml"],
                "remove": ["https://example.com/old.xml"],
                "timestamp": 2000
            })))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let controller = GpodderController::new(config, Some(1000));

        let changes = controller.get_subscription_changes().await.unwrap();
        assert_eq!(
            changes,
            SubscriptionChanges {
                add: vec!["https://example.com/feed1.xml".to_string()],
                remove: vec!["https://example.com/old.xml".to_string()],
                first_sync: false,
            }
        );

        // The response timestamp is stored as is, for use as the next `since`
        assert_eq!(
            *controller
                .state
                .subscriptions_timestamp
                .read()
                .expect("RwLock read should not fail"),
            2000
        );
    }

    #[tokio::test]
    async fn get_subscription_changes_initial_sync() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        // The first sync asks the device endpoint for all changes since 0
        Mock::given(method("GET"))
            .and(path("/api/2/subscriptions/testuser/testdevice.json"))
            .and(query_param("since", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "add": ["https://example.com/feed1.xml", "https://example.com/feed2.xml"],
                "remove": ["https://example.com/old.xml"],
                "timestamp": 2000
            })))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let controller = GpodderController::new(config, Some(0));

        let changes = controller.get_subscription_changes().await.unwrap();
        assert!(changes.first_sync);
        assert_eq!(changes.add.len(), 2);
        assert_eq!(changes.remove, vec!["https://example.com/old.xml"]);
    }

    #[tokio::test]
    async fn get_episode_action_changes() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        Mock::given(method("GET"))
            .and(path("/api/2/episodes/testuser.json"))
            .and(query_param("since", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "timestamp": 5000,
                "actions": [
                    {
                        "podcast": "https://example.com/feed.xml",
                        "episode": "https://example.com/ep1.mp3",
                        "action": "play",
                        "timestamp": "2024-01-15T10:30:00Z",
                        "started": 0,
                        "position": 300,
                        "total": 3600
                    },
                    {
                        "podcast": "https://example.com/feed.xml",
                        "episode": "https://example.com/ep2.mp3",
                        "action": "download",
                        "timestamp": "2024-01-15T11:00:00Z"
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let controller = GpodderController::new(config, Some(1000));

        let actions = controller.get_episode_action_changes().await.unwrap();
        assert_eq!(actions.len(), 2);
        assert!(matches!(actions[0].action, Action::Play));
        assert_eq!(actions[0].position, Some(300));
        assert!(matches!(actions[1].action, Action::Download));

        // The response timestamp is stored as is, for use as the next `since`
        assert_eq!(
            *controller
                .state
                .actions_timestamp
                .read()
                .expect("RwLock read should not fail"),
            5000
        );
    }

    #[tokio::test]
    async fn get_episode_action_changes_skips_unparsable_actions() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        // goPodder style: zone-less timestamps and omitted optional fields,
        // mixed with actions this client cannot parse.
        Mock::given(method("GET"))
            .and(path("/api/2/episodes/testuser.json"))
            .and(query_param("since", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "timestamp": 5000,
                "actions": [
                    {
                        "podcast": "https://example.com/feed.xml",
                        "episode": "https://example.com/ep1.mp3",
                        "device": "antennapod",
                        "action": "play",
                        "timestamp": "2024-05-10T07:54:45",
                        "started": 0,
                        "position": 300,
                        "total": 3600
                    },
                    {
                        "podcast": "https://example.com/feed.xml",
                        "episode": "https://example.com/ep2.mp3",
                        "action": "flattr"
                    },
                    {
                        "episode": "https://example.com/ep3.mp3",
                        "action": "play"
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/2/episodes/testuser.json"))
            .and(query_param("since", "5000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "timestamp": 6000,
                "actions": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let controller = GpodderController::new(config, Some(1000));

        let actions = controller.get_episode_action_changes().await.unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].episode, "https://example.com/ep1.mp3");
        assert_eq!(actions[0].timestamp, Some(1_715_327_685));
        assert_eq!(actions[0].position, Some(300));

        // The skipped actions must not stall the sync: the next request asks
        // for changes since the returned timestamp.
        let actions = controller.get_episode_action_changes().await.unwrap();
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn spawn_mark_played_uploads_single_action() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        Mock::given(method("POST"))
            .and(path("/api/2/episodes/testuser.json"))
            .and(body_partial_json(serde_json::json!([{
                "podcast": "https://example.com/feed.xml",
                "episode": "https://example.com/ep1.mp3",
                "guid": "guid-ep1",
                "device": "testdevice",
                "action": "play",
                "position": 120,
                "total": 3600
            }])))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let messages = run_spawn_and_collect(
            config,
            Some(1000),
            GpodderRequest::MarkPlayed(EpisodePlayedData {
                podcast_url: "https://example.com/feed.xml".into(),
                episode_url: "https://example.com/ep1.mp3".into(),
                guid: "guid-ep1".into(),
                position: 120,
                duration: 3600,
            }),
        )
        .await;

        let has_error = messages
            .iter()
            .any(|m| matches!(m, Message::Gpodder(GpodderMsg::Error(_))));
        assert!(!has_error, "Unexpected GpodderMsg::Error: {messages:?}");
    }

    #[tokio::test]
    async fn mark_played_batch() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        Mock::given(method("POST"))
            .and(path("/api/2/episodes/testuser.json"))
            .and(body_partial_json(serde_json::json!([
                {"episode": "https://example.com/ep1.mp3", "device": "testdevice"},
                {"episode": "https://example.com/ep2.mp3", "device": "testdevice"}
            ])))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let controller = GpodderController::new(config, Some(1000));

        let episodes = vec![
            EpisodePlayedData {
                podcast_url: "https://example.com/feed.xml".into(),
                episode_url: "https://example.com/ep1.mp3".into(),
                guid: "guid-ep1".into(),
                position: 60,
                duration: 1800,
            },
            EpisodePlayedData {
                podcast_url: "https://example.com/feed.xml".into(),
                episode_url: "https://example.com/ep2.mp3".into(),
                guid: "guid-ep2".into(),
                position: 120,
                duration: 3600,
            },
        ];
        let result = controller.mark_played_batch(episodes).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn upload_subscription_changes() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        Mock::given(method("POST"))
            .and(path("/api/2/subscriptions/testuser/testdevice.json"))
            .and(body_json(serde_json::json!({
                "add": ["https://example.com/new1.xml", "https://example.com/new2.xml"],
                "remove": ["https://example.com/old.xml"]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "update_urls": [],
                "timestamp": 3000
            })))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let controller = GpodderController::new(config, Some(1000));

        controller
            .upload_subscription_changes(
                &[
                    "https://example.com/new1.xml".to_string(),
                    "https://example.com/new2.xml".to_string(),
                ],
                &["https://example.com/old.xml".to_string()],
            )
            .await
            .unwrap();

        // Uploading does not move `since`: changes other devices made since
        // our last fetch must still be fetched.
        assert_eq!(
            *controller
                .state
                .subscriptions_timestamp
                .read()
                .expect("RwLock read should not fail"),
            1000
        );
    }

    #[tokio::test]
    async fn login_failure_propagates() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/2/auth/testuser/login.json"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let controller = GpodderController::new(config, Some(100));

        assert!(controller.require_login().await.is_err());
    }

    #[tokio::test]
    async fn get_timestamp_returns_min() {
        let config = test_config("http://unused");
        let controller = GpodderController::new(config, Some(500));

        // Both timestamps start at 500
        assert_eq!(controller.get_timestamp(), 500);

        // Modify one to be lower
        *controller
            .state
            .actions_timestamp
            .write()
            .expect("RwLock write should not fail") = 100;
        assert_eq!(controller.get_timestamp(), 100);
    }

    /// Helper to run `spawn_async`, send a request, collect messages, and quit.
    async fn run_spawn_and_collect(
        config: Config, timestamp: Option<u64>, request: GpodderRequest,
    ) -> Vec<Message> {
        let (tx_req, rx_req) = std::sync::mpsc::channel();
        let (tx_msg, rx_msg) = std::sync::mpsc::channel();

        let handle = tokio::spawn(GpodderController::spawn_async(
            rx_req, tx_msg, config, timestamp,
        ));

        tx_req.send(request).unwrap();
        // Give spawn_async time to process (it polls every TICK_RATE ms)
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        tx_req.send(GpodderRequest::Quit).unwrap();
        handle.await.unwrap();

        let mut messages = Vec::new();
        while let Ok(msg) = rx_msg.try_recv() {
            messages.push(msg);
        }
        messages
    }

    #[tokio::test]
    async fn spawn_sync_error_sends_error_message() {
        let server = MockServer::start().await;

        // Login succeeds but subscription fetch returns 500
        Mock::given(method("POST"))
            .and(path("/api/2/auth/testuser/login.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/2/devices/testuser.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "id": "testdevice",
                    "caption": "Test",
                    "type": "laptop",
                    "subscriptions": 0
                }])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/2/subscriptions/testuser/testdevice.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let messages =
            run_spawn_and_collect(config, Some(1000), GpodderRequest::GetSubscriptionChanges).await;

        let has_error = messages
            .iter()
            .any(|m| matches!(m, Message::Gpodder(GpodderMsg::Error(_))));
        assert!(has_error, "Expected GpodderMsg::Error, got: {messages:?}");
    }

    #[tokio::test]
    async fn spawn_add_podcast_error_sends_error_message() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/2/auth/testuser/login.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/2/devices/testuser.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "id": "testdevice",
                    "caption": "Test",
                    "type": "laptop",
                    "subscriptions": 0
                }])),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/2/subscriptions/testuser/testdevice.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let messages = run_spawn_and_collect(
            config,
            Some(1000),
            GpodderRequest::UploadSubscriptionChanges {
                add: vec!["https://example.com/feed.xml".to_string()],
                remove: Vec::new(),
            },
        )
        .await;

        let has_error = messages
            .iter()
            .any(|m| matches!(m, Message::Gpodder(GpodderMsg::Error(_))));
        assert!(has_error, "Expected GpodderMsg::Error, got: {messages:?}");
    }

    #[tokio::test]
    async fn spawn_success_no_error_message() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        Mock::given(method("GET"))
            .and(path("/api/2/subscriptions/testuser/testdevice.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "add": [],
                "remove": [],
                "timestamp": 2000
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/2/episodes/testuser.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "timestamp": 2000,
                "actions": []
            })))
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let messages =
            run_spawn_and_collect(config, Some(1000), GpodderRequest::GetSubscriptionChanges).await;

        let has_error = messages
            .iter()
            .any(|m| matches!(m, Message::Gpodder(GpodderMsg::Error(_))));
        assert!(!has_error, "Expected no errors, got: {messages:?}");

        let has_changes = messages
            .iter()
            .any(|m| matches!(m, Message::Gpodder(GpodderMsg::SubscriptionChanges(..))));
        assert!(
            has_changes,
            "Expected SubscriptionChanges, got: {messages:?}"
        );
    }

    #[tokio::test]
    async fn spawn_initial_sync_returns_all_subscriptions() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        Mock::given(method("GET"))
            .and(path("/api/2/subscriptions/testuser/testdevice.json"))
            .and(query_param("since", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "add": [
                    "https://example.com/feed1.xml",
                    "https://example.com/feed2.xml",
                    "https://example.com/feed3.xml"
                ],
                "remove": [],
                "timestamp": 100
            })))
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let messages =
            run_spawn_and_collect(config, Some(0), GpodderRequest::GetSubscriptionChanges).await;

        let changes = messages
            .iter()
            .find_map(|m| {
                if let Message::Gpodder(GpodderMsg::SubscriptionChanges(changes)) = m {
                    Some(changes)
                } else {
                    None
                }
            })
            .expect("Expected SubscriptionChanges");
        assert!(changes.first_sync);
        assert_eq!(changes.add.len(), 3);
        assert_eq!(changes.remove, Vec::<String>::new());
        assert!(
            changes
                .add
                .contains(&"https://example.com/feed1.xml".to_string())
        );
    }

    #[tokio::test]
    async fn spawn_get_episode_actions_returns_actions() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        Mock::given(method("GET"))
            .and(path("/api/2/episodes/testuser.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "timestamp": 2000,
                "actions": [{
                    "podcast": "https://example.com/feed.xml",
                    "episode": "https://example.com/ep1.mp3",
                    "action": "play",
                    "timestamp": "2024-01-01T00:00:00Z",
                    "position": 30,
                    "total": 120
                }]
            })))
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let messages =
            run_spawn_and_collect(config, Some(1000), GpodderRequest::GetEpisodeActions).await;

        let actions = messages.iter().find_map(|m| {
            if let Message::Gpodder(GpodderMsg::EpisodeActions(actions, _)) = m {
                Some(actions)
            } else {
                None
            }
        });
        assert!(
            actions.is_some(),
            "Expected EpisodeActions, got: {messages:?}"
        );
        assert_eq!(actions.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn spawn_login_error_on_sync_sends_error() {
        let server = MockServer::start().await;

        // Login fails with 401
        Mock::given(method("POST"))
            .and(path("/api/2/auth/testuser/login.json"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let messages =
            run_spawn_and_collect(config, Some(0), GpodderRequest::GetSubscriptionChanges).await;

        let has_error = messages
            .iter()
            .any(|m| matches!(m, Message::Gpodder(GpodderMsg::Error(_))));
        assert!(has_error, "Expected error on login failure: {messages:?}");
    }
}
