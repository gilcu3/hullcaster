use std::{
    path::PathBuf,
    sync::{Arc, RwLock, mpsc::Receiver},
    time::{Duration, Instant},
};

use anyhow::Result;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player as RodioPlayer};
use stream_download::source::SourceStream;
use stream_download::{
    Settings, StreamDownload,
    http::{HttpStream, reqwest::Client},
    storage::temp::TempStorageProvider,
};

use crate::{
    config::{FADING_TIME, TICK_RATE},
    utils::resolve_redirection_async,
};

pub enum PlayerMessage {
    PlayPause,
    PlayFile(PathBuf, u64, u64),
    PlayUrl(String, u64, u64),
    Seek(Duration, bool),
    Quit,
    /// Workaround for sound not working after resume
    ResetSink,
}

#[derive(PartialEq, Eq, Copy, Clone)]
pub enum PlaybackStatus {
    Ready,
    Playing,
    Paused,
    Finished,
}

pub struct Player {
    stream_handle: MixerDeviceSink, // else the sink stops working
    sink: RodioPlayer,
    elapsed: Arc<RwLock<u64>>,
    duration: u64,
    playing: Arc<RwLock<PlaybackStatus>>,
}

impl Player {
    fn new(elapsed: Arc<RwLock<u64>>, playing: Arc<RwLock<PlaybackStatus>>) -> Result<Self> {
        let stream_handle = DeviceSinkBuilder::open_default_sink()?;
        let sink = RodioPlayer::connect_new(stream_handle.mixer());
        Ok(Self {
            stream_handle,
            sink,
            elapsed,
            duration: 0,
            playing,
        })
    }

    fn reset(&mut self) {
        match DeviceSinkBuilder::open_default_sink() {
            Ok(stream_handle) => {
                let sink = RodioPlayer::connect_new(stream_handle.mixer());
                self.stream_handle = stream_handle;
                self.sink = sink;
            }
            Err(err) => log::error!("Failed to reset audio sink: {err}"),
        }
        *self.playing.write().expect("RwLock write should not fail") = PlaybackStatus::Finished;
        *self.elapsed.write().expect("RwLock write should not fail") = 0;
    }

    pub async fn spawn_async(
        rx_from_ui: Receiver<PlayerMessage>, elapsed: Arc<RwLock<u64>>,
        playing: Arc<RwLock<PlaybackStatus>>,
    ) {
        let mut player = match Self::new(elapsed, playing) {
            Ok(player) => player,
            Err(err) => {
                log::error!("No audio device available: {err}");
                Self::drain_messages(rx_from_ui).await;
                return;
            }
        };
        let mut last_time = Instant::now();
        loop {
            if let Ok(message) = rx_from_ui.try_recv() {
                match message {
                    PlayerMessage::PlayPause => {
                        if !player.sink.empty() {
                            player.play_pause();
                        }
                    }
                    PlayerMessage::PlayFile(path, position, duration) => {
                        player.duration = duration;
                        *player
                            .elapsed
                            .write()
                            .expect("RwLock write should not fail") = position;
                        *player
                            .playing
                            .write()
                            .expect("RwLock write should not fail") = PlaybackStatus::Playing;
                        player
                            .play_file(&path)
                            .await
                            .unwrap_or_else(|err| log::error!("Error playing file: {err}"));
                    }
                    PlayerMessage::PlayUrl(url, position, duration) => {
                        player.duration = duration;
                        *player
                            .elapsed
                            .write()
                            .expect("RwLock write should not fail") = position;
                        *player
                            .playing
                            .write()
                            .expect("RwLock write should not fail") = PlaybackStatus::Playing;
                        player
                            .play_url(&url)
                            .await
                            .unwrap_or_else(|err| log::error!("Error playing url: {err}"));
                    }
                    PlayerMessage::Seek(shift, direction) => {
                        if !player.sink.empty() {
                            player.seek(shift, direction).await;
                        }
                    }
                    PlayerMessage::Quit => {
                        player.sink.stop();
                        break;
                    }
                    PlayerMessage::ResetSink => player.reset(),
                }
            }
            tokio::time::sleep(Duration::from_millis(TICK_RATE)).await;

            if *player.playing.read().expect("RwLock read should not fail")
                == PlaybackStatus::Playing
            {
                let now = Instant::now();
                if now.duration_since(last_time) >= Duration::from_secs(1) {
                    player.set_elapsed();
                    last_time = now;
                }
            }
        }
    }

    async fn play_file(&self, path: &PathBuf) -> Result<()> {
        let file = std::fs::File::open(path)?;
        let source = rodio::Decoder::builder()
            .with_seekable(true)
            .with_data(file)
            .build()?;
        if !self.sink.empty() {
            self.sink.stop();
        }
        self.sink.set_volume(0.0);
        self.sink.append(source);
        let position = *self.elapsed.read().expect("RwLock read should not fail");
        if position > 0
            && let Err(err) = self.sink.try_seek(Duration::from_secs(position))
        {
            log::warn!("Failed to seek: {err}");
        }
        self.sink.play();
        tokio::time::sleep(std::time::Duration::from_millis(FADING_TIME)).await;
        self.sink.set_volume(1.0);
        Ok(())
    }

    async fn play_url(&self, url: &str) -> Result<()> {
        let url = resolve_redirection_async(url)
            .await
            .unwrap_or_else(|_| url.to_string());
        let stream = HttpStream::<Client>::create(url.parse()?).await?;
        let reader =
            StreamDownload::from_stream(stream, TempStorageProvider::new(), Settings::default())
                .await?;
        let source = rodio::Decoder::builder()
            .with_seekable(true)
            .with_data(reader)
            .build()?;
        if !self.sink.empty() {
            self.sink.stop();
        }

        self.sink.set_volume(0.0);
        self.sink.append(source);

        let position = *self.elapsed.read().expect("RwLock read should not fail");
        if position > 0
            && let Err(err) = self.sink.try_seek(Duration::from_secs(position))
        {
            log::warn!("Failed to seek: {err}");
        }
        self.sink.play();
        tokio::time::sleep(std::time::Duration::from_millis(FADING_TIME)).await;
        self.sink.set_volume(1.0);
        Ok(())
    }
    fn play_pause(&self) {
        if self.sink.is_paused() {
            self.sink.play();
            *self.playing.write().expect("RwLock write should not fail") = PlaybackStatus::Playing;
        } else {
            self.sink.pause();
            *self.playing.write().expect("RwLock write should not fail") = PlaybackStatus::Paused;
        }
    }

    async fn seek(&self, shift: Duration, direction: bool) {
        let pos = self.sink.get_pos();
        self.sink.pause();
        self.sink.set_volume(0.0);
        self.sink
            .try_seek({
                if direction {
                    let max_pos = Duration::from_secs(self.duration);
                    if pos + shift >= max_pos {
                        max_pos
                    } else {
                        pos + shift
                    }
                } else {
                    pos.checked_sub(shift).unwrap_or(Duration::ZERO)
                }
            })
            .inspect_err(|err| log::warn!("Failed to seek: {err}"))
            .unwrap_or_default();
        self.sink.play();
        tokio::time::sleep(std::time::Duration::from_millis(FADING_TIME)).await;
        self.sink.set_volume(1.0);
        self.set_elapsed();
    }

    async fn drain_messages(rx_from_ui: Receiver<PlayerMessage>) {
        loop {
            if matches!(rx_from_ui.try_recv(), Ok(PlayerMessage::Quit)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(TICK_RATE)).await;
        }
    }

    fn set_elapsed(&self) {
        let elapsed = self.sink.get_pos();
        if self.sink.empty() {
            *self.playing.write().expect("RwLock write should not fail") = PlaybackStatus::Finished;
            // Snap elapsed to duration on natural finish (1s tolerance for
            // rounding between sink position and RSS/symphonia duration)
            if self.duration > 0 && self.duration <= elapsed.as_secs() + 1 {
                *self.elapsed.write().expect("RwLock write should not fail") = self.duration;
            }
            return;
        }
        *self.elapsed.write().expect("RwLock write should not fail") = elapsed.as_secs();
    }
}
