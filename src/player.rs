use std::{
    error::Error,
    io::BufReader,
    path::PathBuf,
    sync::{mpsc::Receiver, Arc, RwLock},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use rodio::{OutputStream, Sink};
use stream_download::source::SourceStream;
use stream_download::{
    http::{reqwest::Client, HttpStream},
    storage::temp::TempStorageProvider,
    Settings, StreamDownload,
};

use crate::{
    config::{FADING_TIME, TICK_RATE},
    utils::resolve_redirection,
};

pub enum PlayerMessage {
    PlayPause,
    PlayFile(PathBuf, u64, u64),
    PlayUrl(String, u64, u64),
    Seek(Duration, bool),
    Quit,
}

#[derive(PartialEq, Copy, Clone)]
pub enum PlaybackStatus {
    Ready,
    Playing,
    Paused,
    Finished,
}

pub struct Player {
    _stream_handle: OutputStream, // else the sink stops working
    sink: Sink,
    elapsed: Arc<RwLock<u64>>,
    duration: u64,
    playing: Arc<RwLock<PlaybackStatus>>,
}

impl Player {
    fn new(elapsed: Arc<RwLock<u64>>, playing: Arc<RwLock<PlaybackStatus>>) -> Self {
        let stream_handle = rodio::OutputStreamBuilder::open_default_stream().unwrap();
        let sink = rodio::Sink::connect_new(stream_handle.mixer());
        Self {
            _stream_handle: stream_handle,
            sink,
            elapsed,
            duration: 0,
            playing,
        }
    }
    #[tokio::main]
    pub async fn spawn_async(
        rx_from_ui: Receiver<PlayerMessage>, elapsed: Arc<RwLock<u64>>,
        playing: Arc<RwLock<PlaybackStatus>>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut message_iter = rx_from_ui.try_iter();
        let mut player = Player::new(elapsed, playing);
        let mut last_time = Instant::now();
        loop {
            if let Some(message) = message_iter.next() {
                match message {
                    PlayerMessage::PlayPause => {
                        if !player.sink.empty() {
                            player.play_pause()
                        }
                    }
                    PlayerMessage::PlayFile(path, position, duration) => {
                        player.duration = duration;
                        *player.elapsed.write().unwrap() = position;
                        *player.playing.write().unwrap() = PlaybackStatus::Playing;
                        player.play_file(&path);
                    }
                    PlayerMessage::PlayUrl(url, position, duration) => {
                        player.duration = duration;
                        *player.elapsed.write().unwrap() = position;
                        *player.playing.write().unwrap() = PlaybackStatus::Playing;
                        let _ = player.play_url(&url).await;
                    }
                    PlayerMessage::Seek(shift, direction) => {
                        if !player.sink.empty() {
                            player.seek(shift, direction)
                        }
                    }
                    PlayerMessage::Quit => break,
                }
            }
            tokio::time::sleep(Duration::from_millis(TICK_RATE)).await;

            if *player.playing.read().unwrap() == PlaybackStatus::Playing {
                let now = Instant::now();
                if now.duration_since(last_time) >= Duration::from_secs(1) {
                    player.set_elapsed();
                    last_time = now;
                }
            }
        }
        Ok(())
    }

    fn play_file(&mut self, path: &PathBuf) {
        let file = std::fs::File::open(path).unwrap();
        let source = rodio::Decoder::new(BufReader::new(file)).unwrap();
        if !self.sink.empty() {
            self.sink.stop();
        }
        self.sink.set_volume(0.0);
        self.sink.append(source);
        let position = *self.elapsed.read().unwrap();
        let _ = self.sink.try_seek(Duration::from_secs(position));
        self.sink.play();
        std::thread::sleep(std::time::Duration::from_millis(FADING_TIME));
        self.sink.set_volume(1.0);
    }

    async fn play_url(&mut self, url: &str) -> Result<()> {
        let url = resolve_redirection(url).unwrap_or(url.to_string());
        let stream = HttpStream::<Client>::create(url.parse()?).await?;
        let reader =
            StreamDownload::from_stream(stream, TempStorageProvider::new(), Settings::default())
                .await?;
        let source = rodio::Decoder::new(reader)?;
        if !self.sink.empty() {
            self.sink.stop();
        }

        self.sink.set_volume(0.0);
        self.sink.append(source);

        let position = *self.elapsed.read().unwrap();
        let _ = self.sink.try_seek(Duration::from_secs(position));
        self.sink.play();
        std::thread::sleep(std::time::Duration::from_millis(FADING_TIME));
        self.sink.set_volume(1.0);
        Ok(())
    }
    fn play_pause(&self) {
        if self.sink.is_paused() {
            self.sink.play();
            *self.playing.write().unwrap() = PlaybackStatus::Playing;
        } else {
            self.sink.pause();
            *self.playing.write().unwrap() = PlaybackStatus::Paused;
        }
    }

    fn seek(&mut self, shift: Duration, direction: bool) {
        let pos = self.sink.get_pos();
        self.sink.pause();
        self.sink.set_volume(0.0);
        let _ = self.sink.try_seek({
            if direction {
                let max_pos = Duration::from_secs(self.duration);
                if pos + shift >= max_pos {
                    max_pos
                } else {
                    pos + shift
                }
            } else if pos >= shift {
                pos - shift
            } else {
                Duration::ZERO
            }
        });
        self.sink.play();
        std::thread::sleep(std::time::Duration::from_millis(FADING_TIME));
        self.sink.set_volume(1.0);
        self.set_elapsed();
    }

    fn set_elapsed(&mut self) {
        let elapsed = self.sink.get_pos();
        if self.sink.empty() {
            *self.playing.write().unwrap() = PlaybackStatus::Finished;
            // Allow for tiny error in duration
            // TODO: this is a hack that should be done better
            if self.duration > 0 && self.duration <= elapsed.as_secs() + 1 {
                *self.elapsed.write().unwrap() = self.duration;
            }
            return;
        }
        *self.elapsed.write().unwrap() = elapsed.as_secs();
    }
}

pub fn init_player(
    rx_from_ui: Receiver<PlayerMessage>, elapsed: Arc<RwLock<u64>>,
    playing: Arc<RwLock<PlaybackStatus>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let _ = Player::spawn_async(rx_from_ui, elapsed, playing);
    })
}
