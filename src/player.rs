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
    SpeedUp,
    SpeedDown,
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

const SPEED_STEPS: [f32; 9] = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];
const DEFAULT_SPEED_INDEX: usize = 2; // 1.0x

pub struct Player {
    stream_handle: MixerDeviceSink, // else the sink stops working
    sink: RodioPlayer,
    elapsed: Arc<RwLock<u64>>,
    duration: u64,
    playing: Arc<RwLock<PlaybackStatus>>,
    speed: Arc<RwLock<f32>>,
    speed_index: usize,
}

impl Player {
    fn new(
        elapsed: Arc<RwLock<u64>>, playing: Arc<RwLock<PlaybackStatus>>, speed: Arc<RwLock<f32>>,
    ) -> Self {
        let stream_handle = DeviceSinkBuilder::open_default_sink()
            .expect("rodio default stream should be available");
        let sink = RodioPlayer::connect_new(stream_handle.mixer());
        Self {
            stream_handle,
            sink,
            elapsed,
            duration: 0,
            playing,
            speed,
            speed_index: DEFAULT_SPEED_INDEX,
        }
    }

    fn reset(&mut self) {
        let stream_handle = DeviceSinkBuilder::open_default_sink()
            .expect("rodio default stream should be available");
        let sink = RodioPlayer::connect_new(stream_handle.mixer());
        self.stream_handle = stream_handle;
        self.sink = sink;
        self.speed_index = DEFAULT_SPEED_INDEX;
        *self.speed.write().expect("RwLock write should not fail") =
            SPEED_STEPS[DEFAULT_SPEED_INDEX];
        *self.playing.write().expect("RwLock write should not fail") = PlaybackStatus::Finished;
        *self.elapsed.write().expect("RwLock write should not fail") = 0;
    }

    pub async fn spawn_async(
        rx_from_ui: Receiver<PlayerMessage>, elapsed: Arc<RwLock<u64>>,
        playing: Arc<RwLock<PlaybackStatus>>, speed: Arc<RwLock<f32>>,
    ) {
        let mut player = Self::new(elapsed, playing, speed);
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
                    PlayerMessage::SpeedUp => player.change_speed(true),
                    PlayerMessage::SpeedDown => player.change_speed(false),
                    PlayerMessage::Quit => break,
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

    fn change_speed(&mut self, increase: bool) {
        if increase {
            if self.speed_index < SPEED_STEPS.len() - 1 {
                self.speed_index += 1;
            }
        } else if self.speed_index > 0 {
            self.speed_index -= 1;
        }
        let new_speed = SPEED_STEPS[self.speed_index];
        self.sink.set_speed(new_speed);
        *self.speed.write().expect("RwLock write should not fail") = new_speed;
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

    fn set_elapsed(&self) {
        let elapsed = self.sink.get_pos();
        if self.sink.empty() {
            *self.playing.write().expect("RwLock write should not fail") = PlaybackStatus::Finished;
            // Allow for tiny error in duration
            // TODO: this is a hack that should be done better
            if self.duration > 0 && self.duration <= elapsed.as_secs() + 1 {
                *self.elapsed.write().expect("RwLock write should not fail") = self.duration;
            }
            return;
        }
        *self.elapsed.write().expect("RwLock write should not fail") = elapsed.as_secs();
    }
}
