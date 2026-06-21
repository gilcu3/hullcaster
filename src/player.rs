use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::Result;
use rodio::{CpalError, CpalErrorKind, DeviceSinkBuilder, MixerDeviceSink, Player as RodioPlayer};
use stream_download::source::SourceStream;
use stream_download::{
    Settings, StreamDownload,
    http::{HttpStream, reqwest::Client},
    storage::temp::TempStorageProvider,
};
use tokio::sync::mpsc::{Receiver, UnboundedSender};

use crate::{config::FADING_TIME, utils::resolve_redirection_async};

pub enum PlayerMessage {
    PlayPause,
    PlayFile(PathBuf, u64, u64),
    PlayUrl(String, u64, u64),
    Seek(Duration, bool),
    Quit,
    /// Recreate the sink (e.g. after suspend) and resume where it left off.
    /// Sent automatically on device error, or manually via key binding.
    ResetSink,
}

#[derive(PartialEq, Eq, Copy, Clone)]
pub enum PlaybackStatus {
    Ready,
    Playing,
    Paused,
    Finished,
}

/// What is loaded in the sink, kept so playback survives a recreate.
#[derive(Clone)]
enum CurrentTrack {
    File(PathBuf),
    Url(String),
}

pub struct Player {
    stream_handle: MixerDeviceSink, // else the sink stops working
    sink: RodioPlayer,
    elapsed: Arc<RwLock<u64>>,
    duration: u64,
    playing: Arc<RwLock<PlaybackStatus>>,
    current: Option<CurrentTrack>,
    /// Lets the device error callback ask the loop to recreate the sink.
    internal_tx: UnboundedSender<PlayerMessage>,
}

impl Player {
    fn new(
        elapsed: Arc<RwLock<u64>>, playing: Arc<RwLock<PlaybackStatus>>,
        internal_tx: UnboundedSender<PlayerMessage>,
    ) -> Result<Self> {
        let (stream_handle, sink) = Self::open_sink(&internal_tx)?;
        Ok(Self {
            stream_handle,
            sink,
            elapsed,
            duration: 0,
            playing,
            current: None,
            internal_tx,
        })
    }

    /// Opens the default sink with an error callback that requests a recreate
    /// when the device becomes unusable (e.g. after suspend).
    fn open_sink(
        internal_tx: &UnboundedSender<PlayerMessage>,
    ) -> Result<(MixerDeviceSink, RodioPlayer)> {
        let tx = internal_tx.clone();
        let stream_handle = DeviceSinkBuilder::from_default_device()?
            .with_error_callback(move |err: CpalError| match err.kind() {
                CpalErrorKind::DeviceNotAvailable | CpalErrorKind::StreamInvalidated => {
                    log::warn!("Audio device error ({err}); requesting sink recreate");
                    tx.send(PlayerMessage::ResetSink).ok();
                }
                _ => log::debug!("Audio stream error: {err}"),
            })
            .open_sink_or_fallback()?;
        let sink = RodioPlayer::connect_new(stream_handle.mixer());
        Ok((stream_handle, sink))
    }

    /// Recreates the sink and restores the current track at its position and
    /// play/pause state. Finished/idle sessions are left alone.
    async fn recreate(&mut self) {
        let status = *self.playing.read().expect("RwLock read should not fail");
        match Self::open_sink(&self.internal_tx) {
            Ok((stream_handle, sink)) => {
                self.stream_handle = stream_handle;
                self.sink = sink;
            }
            Err(err) => {
                log::error!("Failed to recreate audio sink: {err}");
                return;
            }
        }

        if !matches!(status, PlaybackStatus::Playing | PlaybackStatus::Paused) {
            return;
        }
        let Some(track) = self.current.clone() else {
            return;
        };
        // play_file/play_url seek to the position held in `elapsed`.
        let result = match &track {
            CurrentTrack::File(path) => self.play_file(path).await,
            CurrentTrack::Url(url) => self.play_url(url).await,
        };
        if let Err(err) = result {
            log::error!("Failed to restore playback after sink recreate: {err}");
            return;
        }
        // play_file/play_url always start playing; re-pause if needed.
        if status == PlaybackStatus::Paused {
            self.sink.pause();
            *self.playing.write().expect("RwLock write should not fail") = PlaybackStatus::Paused;
        }
    }

    pub async fn spawn_async(
        mut rx_from_ui: Receiver<PlayerMessage>, elapsed: Arc<RwLock<u64>>,
        playing: Arc<RwLock<PlaybackStatus>>,
    ) {
        let (internal_tx, mut internal_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut player = match Self::new(elapsed, playing, internal_tx) {
            Ok(player) => player,
            Err(err) => {
                log::error!("No audio device available: {err}");
                Self::drain_messages(&mut rx_from_ui).await;
                return;
            }
        };
        let mut elapsed_interval = tokio::time::interval(Duration::from_secs(1));
        elapsed_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                msg = rx_from_ui.recv() => {
                    let Some(message) = msg else { break };
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
                            player.current = Some(CurrentTrack::File(path.clone()));
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
                            player.current = Some(CurrentTrack::Url(url.clone()));
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
                        PlayerMessage::ResetSink => player.recreate().await,
                    }
                }
                // Device error callback requested a recreate.
                Some(_) = internal_rx.recv() => {
                    player.recreate().await;
                    while internal_rx.try_recv().is_ok() {} // collapse bursts
                }
                _ = elapsed_interval.tick() => {
                    if *player.playing.read().expect("RwLock read should not fail")
                        == PlaybackStatus::Playing
                    {
                        player.set_elapsed();
                    }
                }
            }
        }
    }

    async fn play_file(&self, path: &PathBuf) -> Result<()> {
        let file = std::fs::File::open(path)?;
        let source = rodio::Decoder::try_from(file)?;
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
        let source = {
            match reader.content_length() {
                None => rodio::Decoder::builder().with_data(reader).build()?,
                Some(byte_len) => rodio::Decoder::builder()
                    .with_data(reader)
                    .with_byte_len(byte_len)
                    .with_seekable(true)
                    .build()?,
            }
        };
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

    async fn drain_messages(rx_from_ui: &mut Receiver<PlayerMessage>) {
        while let Some(msg) = rx_from_ui.recv().await {
            if matches!(msg, PlayerMessage::Quit) {
                break;
            }
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
