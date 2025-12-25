use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};
use std::sync::mpsc::Sender;

use crate::{
    config::TICK_RATE,
    player::PlaybackStatus,
    types::{Episode, ShareableRwLock},
};

pub enum ControlMessage {
    PlayPause,
}

fn update_control_metadata(title: &str, controls: &mut MediaControls) {
    controls
        .set_metadata(MediaMetadata {
            title: Some(title),
            ..Default::default()
        })
        .unwrap();
}

pub fn init_controls(
    tx_to_ui: Sender<ControlMessage>,
    current_episode: ShareableRwLock<Option<ShareableRwLock<Episode>>>,
    playing: ShareableRwLock<PlaybackStatus>, mut rx_from_main: tokio::sync::oneshot::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn({
        let config = PlatformConfig {
            dbus_name: "hullcaster",
            display_name: "Hullcaster",
            hwnd: None,
        };
        let mut controls = MediaControls::new(config).unwrap();
        let mut last_episode_id = -1_i64;
        let mut last_status = PlaybackStatus::Ready;

        controls
            .attach(move |event: MediaControlEvent| {
                if event == MediaControlEvent::Toggle {
                    let _ = tx_to_ui.send(ControlMessage::PlayPause);
                }
            })
            .unwrap();

        async move {
            loop {
                if last_status != *playing.read().unwrap() {
                    last_status = *playing.read().unwrap();
                    match last_status {
                        PlaybackStatus::Playing => {
                            let _ =
                                controls.set_playback(MediaPlayback::Playing { progress: None });
                        }
                        PlaybackStatus::Paused => {
                            let _ = controls.set_playback(MediaPlayback::Paused { progress: None });
                        }
                        PlaybackStatus::Finished | PlaybackStatus::Ready => {
                            let _ = controls.set_playback(MediaPlayback::Stopped);
                        }
                    }
                }

                if let Some(ep) = current_episode.read().unwrap().as_ref() {
                    let ep = ep.read().unwrap();
                    if ep.id != last_episode_id {
                        update_control_metadata(&ep.title, &mut controls);
                        last_episode_id = ep.id;
                    }
                }
                if rx_from_main.try_recv().is_ok() {
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(TICK_RATE)).await;
            }
        }
    })
}
