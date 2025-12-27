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

fn update_control_metadata(
    title: &str, controls: &mut MediaControls,
) -> Result<(), souvlaki::Error> {
    controls.set_metadata(MediaMetadata {
        title: Some(title),
        ..Default::default()
    })
}

pub fn init_controls(
    tx_to_ui: Sender<ControlMessage>,
    current_episode: ShareableRwLock<Option<ShareableRwLock<Episode>>>,
    playing: ShareableRwLock<PlaybackStatus>, mut rx_from_main: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let task = tokio::task::spawn({
        let config = PlatformConfig {
            dbus_name: "hullcaster",
            display_name: "Hullcaster",
            hwnd: None,
        };
        let mut controls = MediaControls::new(config)?;
        let mut last_episode_id = -1_i64;
        let mut last_status = PlaybackStatus::Ready;

        controls.attach(move |event: MediaControlEvent| {
            if event == MediaControlEvent::Toggle {
                tx_to_ui
                    .send(ControlMessage::PlayPause)
                    .inspect_err(|err| {
                        log::error!("Could not send ControlMessage::PlayPause to ui: {err}");
                    })
                    .ok();
            }
        })?;

        async move {
            loop {
                if last_status != *playing.read().expect("RwLock read should not fail") {
                    last_status = *playing.read().expect("RwLock read should not fail");
                    match last_status {
                        PlaybackStatus::Playing => {
                            controls
                                .set_playback(MediaPlayback::Playing { progress: None })
                                .inspect_err(|err| {
                                    log::error!(
                                        "Could not set playback to MediaPlayback::Playing: {err}"
                                    );
                                })
                                .ok();
                        }
                        PlaybackStatus::Paused => {
                            controls
                                .set_playback(MediaPlayback::Paused { progress: None })
                                .inspect_err(|err| {
                                    log::error!(
                                        "Could not set playback to MediaPlayback::Paused: {err}"
                                    );
                                })
                                .ok();
                        }
                        PlaybackStatus::Finished | PlaybackStatus::Ready => {
                            controls
                                .set_playback(MediaPlayback::Stopped)
                                .inspect_err(|err| {
                                    log::error!(
                                        "Could not set playback to MediaPlayback::Stopped: {err}"
                                    );
                                })
                                .ok();
                        }
                    }
                }

                if let Some(ep) = current_episode
                    .read()
                    .expect("RwLock read should not fail")
                    .as_ref()
                {
                    let ep = ep.read().expect("RwLock read should not fail");
                    if ep.id != last_episode_id {
                        update_control_metadata(&ep.title, &mut controls)
                            .inspect_err(|err| {
                                log::error!("update_control_metadata failed: {err}");
                            })
                            .unwrap_or_default();
                        last_episode_id = ep.id;
                    }
                }
                if rx_from_main.try_recv().is_ok() {
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(TICK_RATE)).await;
            }
        }
    });
    Ok(task)
}
