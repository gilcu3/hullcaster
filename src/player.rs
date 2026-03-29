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
    SpeedUp,
    SpeedDown,
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

const SPEED_STEPS: [f32; 9] = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];
const DEFAULT_SPEED_INDEX: usize = 2; // 1.0x

/// Conversion between the sink's timeline and the episode's.
///
/// rodio tracks the sink position after the speed stage, so at speed `s` one
/// sink second covers `s` seconds of the episode, and `try_seek` expects a
/// position in that same stretched timeline. Since the speed can change
/// mid-episode, the mapping is anchored at the last seek or speed change rather
/// than being a plain multiplication.
#[derive(Clone, Copy)]
struct Timeline {
    /// Sink position at the anchor.
    sink: Duration,
    /// Episode position at the anchor.
    episode: Duration,
    speed: f32,
}

impl Timeline {
    const fn new(speed: f32) -> Self {
        Self {
            sink: Duration::ZERO,
            episode: Duration::ZERO,
            speed,
        }
    }

    /// Episode position matching the sink position `sink_pos`.
    fn episode_pos(&self, sink_pos: Duration) -> Duration {
        self.episode + sink_pos.saturating_sub(self.sink).mul_f32(self.speed)
    }

    /// Sink position to seek to in order to land on `pos` in the episode. The
    /// two timelines line up there, so this also becomes the new anchor.
    fn seek(&mut self, pos: Duration) -> Duration {
        self.sink = pos.div_f32(self.speed);
        self.episode = pos;
        self.sink
    }

    /// Re-anchors at the current position before switching speed: whatever
    /// played so far did so at the old speed.
    fn set_speed(&mut self, sink_pos: Duration, speed: f32) {
        self.episode = self.episode_pos(sink_pos);
        self.sink = sink_pos;
        self.speed = speed;
    }

    /// Pins both timelines to the start of a freshly appended track.
    const fn reset(&mut self) {
        self.sink = Duration::ZERO;
        self.episode = Duration::ZERO;
    }
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
    speed: Arc<RwLock<f32>>,
    speed_index: usize,
    timeline: Timeline,
}

impl Player {
    fn new(
        elapsed: Arc<RwLock<u64>>, playing: Arc<RwLock<PlaybackStatus>>, speed: Arc<RwLock<f32>>,
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
            speed,
            speed_index: DEFAULT_SPEED_INDEX,
            timeline: Timeline::new(SPEED_STEPS[DEFAULT_SPEED_INDEX]),
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
                self.sink.set_speed(self.speed());
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
        playing: Arc<RwLock<PlaybackStatus>>, speed: Arc<RwLock<f32>>,
    ) {
        let (internal_tx, mut internal_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut player = match Self::new(elapsed, playing, speed, internal_tx) {
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
                        PlayerMessage::SpeedUp => player.change_speed(true),
                        PlayerMessage::SpeedDown => player.change_speed(false),
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

    async fn play_file(&mut self, path: &PathBuf) -> Result<()> {
        let file = std::fs::File::open(path)?;
        let source = rodio::Decoder::try_from(file)?;
        if !self.sink.empty() {
            self.sink.stop();
        }
        self.sink.set_volume(0.0);
        self.sink.append(source);
        self.timeline.reset();
        let position = *self.elapsed.read().expect("RwLock read should not fail");
        if position > 0
            && let Err(err) = self.try_seek(Duration::from_secs(position))
        {
            log::warn!("Failed to seek: {err}");
        }
        self.sink.play();
        tokio::time::sleep(std::time::Duration::from_millis(FADING_TIME)).await;
        self.sink.set_volume(1.0);
        Ok(())
    }

    async fn play_url(&mut self, url: &str) -> Result<()> {
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

        self.timeline.reset();
        let position = *self.elapsed.read().expect("RwLock read should not fail");
        if position > 0
            && let Err(err) = self.try_seek(Duration::from_secs(position))
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

    const fn speed(&self) -> f32 {
        SPEED_STEPS[self.speed_index]
    }

    /// Current position within the episode, not within the sink.
    fn episode_pos(&self) -> Duration {
        self.timeline.episode_pos(self.sink.get_pos())
    }

    /// Seeks to `pos` within the episode. rodio reports the requested position
    /// afterwards even if the seek failed, so the anchor moves either way.
    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        self.sink.try_seek(self.timeline.seek(pos))
    }

    fn change_speed(&mut self, increase: bool) {
        let index = if increase {
            (self.speed_index + 1).min(SPEED_STEPS.len() - 1)
        } else {
            self.speed_index.saturating_sub(1)
        };
        if index == self.speed_index {
            return;
        }
        self.speed_index = index;
        let new_speed = self.speed();
        self.timeline.set_speed(self.sink.get_pos(), new_speed);
        self.sink.set_speed(new_speed);
        *self.speed.write().expect("RwLock write should not fail") = new_speed;
    }

    async fn seek(&mut self, shift: Duration, direction: bool) {
        let pos = self.episode_pos();
        let target = if direction {
            let max_pos = Duration::from_secs(self.duration);
            // A duration of 0 means unknown, so there is nothing to clamp to.
            if self.duration > 0 && pos + shift >= max_pos {
                max_pos
            } else {
                pos + shift
            }
        } else {
            pos.saturating_sub(shift)
        };
        self.sink.pause();
        self.sink.set_volume(0.0);
        self.try_seek(target)
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
        let elapsed = self.episode_pos();
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

#[cfg(test)]
mod tests {
    use super::*;

    const fn secs(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    #[test]
    fn episode_position_follows_speed() {
        let timeline = Timeline::new(2.0);
        // 10 seconds of playback at 2x covers 20 seconds of the episode.
        assert_eq!(timeline.episode_pos(secs(10)), secs(20));

        let timeline = Timeline::new(0.5);
        assert_eq!(timeline.episode_pos(secs(10)), secs(5));
    }

    #[test]
    fn seek_target_is_scaled_back_to_the_sink() {
        let mut timeline = Timeline::new(2.0);
        // The sink stretches the seek by the speed, so ask for half.
        assert_eq!(timeline.seek(secs(60)), secs(30));
        assert_eq!(timeline.episode_pos(secs(30)), secs(60));
        assert_eq!(timeline.episode_pos(secs(40)), secs(80));
    }

    #[test]
    fn speed_change_keeps_the_position_continuous() {
        let mut timeline = Timeline::new(1.0);
        // 30 seconds played at 1x, then the speed doubles.
        timeline.set_speed(secs(30), 2.0);
        assert_eq!(timeline.episode_pos(secs(30)), secs(30));
        // 10 more seconds of playback, now worth 20 episode seconds.
        assert_eq!(timeline.episode_pos(secs(40)), secs(50));
    }

    #[test]
    fn reset_pins_a_new_track_to_zero() {
        let mut timeline = Timeline::new(1.5);
        timeline.seek(secs(90));
        timeline.reset();
        assert_eq!(timeline.episode_pos(Duration::ZERO), Duration::ZERO);
        // The speed survives the reset.
        assert_eq!(timeline.episode_pos(secs(10)), secs(15));
    }
}
