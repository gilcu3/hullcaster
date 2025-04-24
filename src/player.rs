use core::time;
use std::{
    io::BufReader,
    path::PathBuf,
    sync::{mpsc::Receiver, Arc, RwLock},
    thread,
    time::{Duration, Instant},
};

use rodio::{OutputStream, Sink};

use crate::config::TICK_RATE;

pub enum PlayerMessage {
    PlayPause,
    PlayFile(PathBuf),
    Quit,
}

pub struct Player {
    _stream: OutputStream, // else the sink stops working
    sink: Sink,
    elapsed: Arc<RwLock<u64>>,
}

impl Player {
    fn new(elapsed: Arc<RwLock<u64>>) -> Self {
        let (_stream, stream_handle) = OutputStream::try_default().unwrap();
        let sink = Sink::try_new(&stream_handle).unwrap();
        Self {
            _stream,
            sink,
            elapsed,
        }
    }
    pub fn spawn(
        rx_from_ui: Receiver<PlayerMessage>, elapsed: Arc<RwLock<u64>>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut message_iter = rx_from_ui.try_iter();
            let mut player = Player::new(elapsed);
            let mut last_time = Instant::now();
            loop {
                if let Some(message) = message_iter.next() {
                    match message {
                        PlayerMessage::PlayPause => player.play_pause(),
                        PlayerMessage::PlayFile(path) => player.play_file(&path),
                        PlayerMessage::Quit => break,
                    }
                }
                thread::sleep(time::Duration::from_millis(TICK_RATE));
                let now = Instant::now();
                if now.duration_since(last_time) >= Duration::from_secs(1) {
                    player.set_elapsed();
                    last_time = now;
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
        self.sink.append(source);
        self.sink.play();
    }
    fn play_pause(&self) {
        if self.sink.is_paused() {
            self.sink.play();
        } else {
            self.sink.pause();
        }
    }

    fn set_elapsed(&mut self) {
        let elapsed = self.sink.get_pos();
        *self.elapsed.write().unwrap() = elapsed.as_secs();
    }
}
