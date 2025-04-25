use core::time;
use std::{
    io::BufReader,
    path::PathBuf,
    sync::{mpsc::Receiver, Arc, RwLock},
    thread,
    time::{Duration, Instant},
};

use rodio::{OutputStream, Sink};

use crate::config::{FADING_TIME, TICK_RATE};

pub enum PlayerMessage {
    PlayPause,
    PlayFile(PathBuf, u64, u64),
    Seek(Duration, bool),
    Quit,
}
#[derive(PartialEq)]
pub enum PlaybackStatus {
    Ready,
    Playing,
    Paused,
    Finished,
}

pub struct Player {
    _stream: OutputStream, // else the sink stops working
    sink: Sink,
    elapsed: Arc<RwLock<u64>>,
    duration: u64,
    playing: Arc<RwLock<PlaybackStatus>>,
}

impl Player {
    fn new(elapsed: Arc<RwLock<u64>>, playing: Arc<RwLock<PlaybackStatus>>) -> Self {
        let (_stream, stream_handle) = OutputStream::try_default().unwrap();
        let sink = Sink::try_new(&stream_handle).unwrap();
        Self {
            _stream,
            sink,
            elapsed,
            duration: 0,
            playing,
        }
    }
    pub fn spawn(
        rx_from_ui: Receiver<PlayerMessage>, elapsed: Arc<RwLock<u64>>,
        playing: Arc<RwLock<PlaybackStatus>>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
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
                        PlayerMessage::Seek(shift, direction) => {
                            if !player.sink.empty() {
                                player.seek(shift, direction)
                            }
                        }
                        PlayerMessage::Quit => break,
                    }
                }
                thread::sleep(time::Duration::from_millis(TICK_RATE));

                if *player.playing.read().unwrap() == PlaybackStatus::Playing {
                    let now = Instant::now();
                    if now.duration_since(last_time) >= Duration::from_secs(1) {
                        player.set_elapsed();
                        last_time = now;
                    }
                }
            }
        })
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
            if self.duration <= elapsed.as_secs() + 1 {
                *self.elapsed.write().unwrap() = self.duration;
            }
            return;
        }
        *self.elapsed.write().unwrap() = elapsed.as_secs();
    }
}
