use anyhow::Context;
use anyhow::{Result, anyhow};
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::TICK_RATE;
use crate::types::Message;

use self::net::{execute_request_get, execute_request_post};
use self::types::{Device, Podcast, PodcastChanges, State, UploadPodcastChanges};

pub use self::types::{Action, Config, EpisodeAction, GpodderMsg, GpodderRequest};
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
            .timeout(Duration::from_secs(120))
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
                        let mut had_error = false;
                        let subscription_changes = sync_client
                            .get_subscription_changes()
                            .await
                            .unwrap_or_else(|err| {
                                log::error!("Failed to get subscription changes: {err}");
                                send_error(&tx_to_app, format!("Gpodder sync failed: {err}"));
                                had_error = true;
                                (Vec::new(), Vec::new())
                            });
                        let episode_actions = sync_client
                            .get_episode_action_changes()
                            .await
                            .unwrap_or_else(|err| {
                                log::error!("Failed to get episode action changes: {err:?}");
                                if !had_error {
                                    send_error(
                                        &tx_to_app,
                                        format!("Gpodder episode sync failed: {err}"),
                                    );
                                }
                                Vec::new()
                            });
                        let timestamp = sync_client.get_timestamp();
                        if tx_to_app
                            .send(Message::Gpodder(GpodderMsg::SubscriptionChanges(
                                subscription_changes,
                                episode_actions,
                                timestamp,
                            )))
                            .is_err()
                        {
                            log::error!("Failed to send gpodder message: channel closed");
                            break;
                        }
                    }
                    GpodderRequest::AddPodcast(url) => {
                        if let Err(err) = sync_client.add_podcast(&url).await {
                            log::error!("Failed to add podcast with url {url}: {err}");
                            send_error(&tx_to_app, format!("Gpodder: failed to add {url}"));
                        }
                    }
                    GpodderRequest::RemovePodcast(url) => {
                        if let Err(err) = sync_client.remove_podcast(&url).await {
                            log::error!("Failed to remove podcast with url {url}: {err}");
                            send_error(&tx_to_app, format!("Gpodder: failed to remove {url}"));
                        }
                    }
                    GpodderRequest::MarkPlayed(pod_url, ep_url, position, duration) => {
                        if let Err(err) = sync_client
                            .mark_played(&pod_url, &ep_url, position, duration)
                            .await
                        {
                            log::error!(
                                "Failed to mark played {ep_url} {position} {duration}: {err}"
                            );
                            send_error(
                                &tx_to_app,
                                format!("Gpodder: failed to sync position for {ep_url}"),
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

    pub async fn mark_played(
        &self, podcast_url: &str, episode_url: &str, position: u64, duration: u64,
    ) -> Result<String> {
        self.require_login().await?;
        let url_mark_played = format!(
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
            total: Some(duration),
        };
        let actions = [action];
        let msg = serde_json::to_string(&actions)?;

        let result = execute_request_post(
            &self.client,
            url_mark_played,
            msg,
            &self.config.credentials,
            self.config.max_retries,
        )
        .await?;
        log::debug!("Marked position: {position} episode: {episode_url} podcast: {podcast_url}");
        Ok(result)
    }

    pub async fn mark_played_batch(&self, eps: Vec<(String, String, u64, u64)>) -> Result<String> {
        self.require_login().await?;
        let url_mark_played = format!(
            "{}/api/2/episodes/{}/{}.json",
            self.config.server, self.config.username, self.config.device
        );
        let actions: Vec<EpisodeAction> = eps
            .iter()
            .filter_map(|(podcast_url, episode_url, position, duration)| {
                Some(EpisodeAction {
                    podcast: podcast_url.into(),
                    episode: episode_url.into(),
                    action: Action::Play,
                    timestamp: current_time().ok()?,
                    started: Some(0),
                    position: Some(*position),
                    total: Some(*duration),
                })
            })
            .collect();
        let msg = serde_json::to_string(&actions)?;

        let result = execute_request_post(
            &self.client,
            url_mark_played,
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
        let url_episode_action_changes = format!(
            "{}/api/2/episodes/{}.json",
            self.config.server, self.config.username
        );
        let since = *self
            .state
            .actions_timestamp
            .read()
            .expect("RwLock read should not fail");
        let json_string = execute_request_get(
            &self.client,
            url_episode_action_changes,
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
            let action = serde_json::from_value::<EpisodeAction>(action.clone())
                .with_context(|| format!("{action:?}"))?;
            actions.push(action);
        }
        *self
            .state
            .actions_timestamp
            .write()
            .expect("RwLock write should not fail") = timestamp + 1;
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

    pub async fn get_subscription_changes(&self) -> Result<(Vec<String>, Vec<String>)> {
        self.require_login().await?;
        if *self
            .state
            .subscriptions_timestamp
            .read()
            .expect("RwLock read should not fail")
            == 0
        {
            let added = self.get_all_subscriptions().await?;
            return Ok((added, Vec::new()));
        }
        let url_subscription_changes = format!(
            "{}/api/2/subscriptions/{}/{}.json",
            self.config.server, self.config.username, self.config.device
        );
        let pastime = (*self
            .state
            .subscriptions_timestamp
            .read()
            .expect("RwLock read should not fail"))
        .to_string();
        let params = vec![("since", pastime.as_str())];
        let json_string = execute_request_get(
            &self.client,
            url_subscription_changes,
            params,
            &self.config.credentials,
            self.config.max_retries,
        )
        .await?;
        let parsed: serde_json::Result<PodcastChanges> = serde_json::from_str(json_string.as_str());
        if let Ok(changes) = parsed {
            for sub in &changes.add {
                log::info!("podcast added {sub}");
            }
            for sub in &changes.remove {
                log::info!("podcast removed {sub}");
            }
            *self
                .state
                .subscriptions_timestamp
                .write()
                .expect("RwLock write should not fail") = changes.timestamp + 1;
            Ok((changes.add, changes.remove))
        } else {
            Err(anyhow!("Error parsing subscription changes"))
        }
    }

    pub async fn upload_subscription_changes(
        &self, changes: (Vec<&String>, Vec<&String>),
    ) -> Result<()> {
        self.require_login().await?;
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
            &self.client,
            url_upload_subscriptions,
            json_changes,
            &self.config.credentials,
            self.config.max_retries,
        )
        .await?;
        let parsed: serde_json::Result<UploadPodcastChanges> =
            serde_json::from_str(json_string.as_str());
        if let Ok(changes) = parsed {
            for sub in &changes.update_urls {
                log::info!("url changed {} {}", sub[0], sub[1]);
            }
            *self
                .state
                .subscriptions_timestamp
                .write()
                .expect("RwLock write should not fail") = changes.timestamp + 1;
            Ok(())
        } else {
            Err(anyhow!("Error parsing url subscription changes"))
        }
    }

    pub async fn add_podcast(&self, url: &String) -> Result<()> {
        self.upload_subscription_changes((vec![url], vec![])).await
    }

    pub async fn remove_podcast(&self, url: &String) -> Result<()> {
        self.upload_subscription_changes((vec![], vec![url])).await
    }

    async fn get_all_subscriptions(&self) -> Result<Vec<String>> {
        let url_subscription_changes = format!(
            "{}/subscriptions/{}.json",
            self.config.server, self.config.username
        );
        let json_string = execute_request_get(
            &self.client,
            url_subscription_changes,
            vec![],
            &self.config.credentials,
            self.config.max_retries,
        )
        .await?;
        let parsed: serde_json::Result<Vec<Podcast>> = serde_json::from_str(json_string.as_str());

        parsed.map_or_else(
            |err| Err(anyhow!("Error parsing subscriptions: {err}")),
            |subscriptions| Ok(subscriptions.iter().map(|f| f.feed.clone()).collect()),
        )
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
    use wiremock::matchers::{method, path};
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

        let (added, removed) = controller.get_subscription_changes().await.unwrap();
        assert_eq!(added, vec!["https://example.com/feed1.xml"]);
        assert_eq!(removed, vec!["https://example.com/old.xml"]);

        // Timestamp should be updated to response timestamp + 1
        assert_eq!(
            *controller
                .state
                .subscriptions_timestamp
                .read()
                .expect("RwLock read should not fail"),
            2001
        );
    }

    #[tokio::test]
    async fn get_subscription_changes_initial_sync() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        // When timestamp is 0, get_all_subscriptions is called instead
        Mock::given(method("GET"))
            .and(path("/subscriptions/testuser.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"feed": "https://example.com/feed1.xml", "title": "Feed 1"},
                {"feed": "https://example.com/feed2.xml", "title": "Feed 2"}
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        // timestamp 0 triggers initial sync path
        let controller = GpodderController::new(config, Some(0));

        let (added, removed) = controller.get_subscription_changes().await.unwrap();
        assert_eq!(added.len(), 2);
        assert!(removed.is_empty());
    }

    #[tokio::test]
    async fn get_episode_action_changes() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        Mock::given(method("GET"))
            .and(path("/api/2/episodes/testuser.json"))
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

        // Timestamp should be updated to response timestamp + 1
        assert_eq!(
            *controller
                .state
                .actions_timestamp
                .read()
                .expect("RwLock read should not fail"),
            5001
        );
    }

    #[tokio::test]
    async fn mark_played_single() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        Mock::given(method("POST"))
            .and(path("/api/2/episodes/testuser/testdevice.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let controller = GpodderController::new(config, Some(1000));

        let result = controller
            .mark_played(
                "https://example.com/feed.xml",
                "https://example.com/ep1.mp3",
                120,
                3600,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn mark_played_batch() {
        let server = MockServer::start().await;
        mock_login_and_init(&server).await;

        Mock::given(method("POST"))
            .and(path("/api/2/episodes/testuser/testdevice.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let controller = GpodderController::new(config, Some(1000));

        let episodes = vec![
            (
                "https://example.com/feed.xml".into(),
                "https://example.com/ep1.mp3".into(),
                60,
                1800,
            ),
            (
                "https://example.com/feed.xml".into(),
                "https://example.com/ep2.mp3".into(),
                120,
                3600,
            ),
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
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "update_urls": [],
                "timestamp": 3000
            })))
            .expect(1)
            .mount(&server)
            .await;

        let config = test_config(&server.uri());
        let controller = GpodderController::new(config, Some(1000));

        let url = "https://example.com/new_feed.xml".to_string();
        let result = controller.add_podcast(&url).await;
        assert!(result.is_ok());

        assert_eq!(
            *controller
                .state
                .subscriptions_timestamp
                .read()
                .expect("RwLock read should not fail"),
            3001
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
            GpodderRequest::AddPodcast("https://example.com/feed.xml".to_string()),
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
}
