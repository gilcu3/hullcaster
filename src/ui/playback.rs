use anyhow::{Result, anyhow};

use crate::{
    player::{PlaybackStatus, PlayerMessage},
    types::{Episode, ShareableRwLock},
};

use super::{UiMsg, UiState};

impl UiState {
    pub(super) fn play_current(&mut self, ep_id: i64) -> Result<()> {
        self.construct_current_episode(ep_id);
        let ep = self
            .current_episode
            .read()
            .expect("RwLock read should not fail");
        let (path, position, duration, url) = {
            let ep = ep
                .as_ref()
                .ok_or_else(|| anyhow!("Failed to get current episode"))?
                .read()
                .expect("RwLock read should not fail");
            (
                ep.path.clone(),
                ep.position,
                ep.duration.unwrap_or(0),
                ep.url.clone(),
            )
        };

        *self.elapsed.write().expect("RwLock write should not fail") = position;
        if let Some(path) = path {
            self.tx_to_player
                .send(PlayerMessage::PlayFile(path, position, duration))?;
        } else {
            self.tx_to_player
                .send(PlayerMessage::PlayUrl(url, position, duration))?;
        }
        Ok(())
    }

    pub(super) fn playback_finished(&self) -> bool {
        self.current_episode
            .read()
            .expect("RwLock read should not fail")
            .is_some()
            && *self.playing.read().expect("RwLock read should not fail")
                == PlaybackStatus::Finished
    }

    pub(super) fn update_position(&self) -> Option<UiMsg> {
        let cur_ep = self
            .current_episode
            .read()
            .expect("RwLock read should not fail");
        let position = *self.elapsed.read().expect("RwLock read should not fail");
        let cur_ep = cur_ep
            .as_ref()?
            .read()
            .expect("RwLock read should not fail");
        Some(UiMsg::UpdatePosition(cur_ep.pod_id, cur_ep.id, position))
    }

    pub(super) fn play_pause(&self) -> Option<UiMsg> {
        let playing = self.playing.read().expect("RwLock read should not fail");
        self.tx_to_player.send(PlayerMessage::PlayPause).ok()?;
        // only updates position after Pause
        match *playing {
            PlaybackStatus::Playing => self.update_position(),
            _ => None,
        }
    }

    pub(super) fn play_selected_episode(&self) -> Vec<UiMsg> {
        if let Some(pod_id) = self.get_podcast_id()
            && let Some(ep_id) = self.get_episode_id()
        {
            return self.play_episode(pod_id, ep_id);
        }
        vec![]
    }

    pub(super) fn play_episode(&self, pod_id: i64, ep_id: i64) -> Vec<UiMsg> {
        let (same, playing, cur_ep_id, cur_pod_id) = self
            .current_episode
            .read()
            .expect("RwLock read should not fail")
            .as_ref()
            .map_or((false, false, 0, 0), |cur_ep| {
                let cur_ep = cur_ep.read().expect("RwLock read should not fail");
                (
                    cur_ep.id == ep_id && cur_ep.pod_id == pod_id,
                    *self.playing.read().expect("RwLock read should not fail")
                        == PlaybackStatus::Playing,
                    cur_ep.id,
                    cur_ep.pod_id,
                )
            });
        if !same {
            if playing {
                let position = *self.elapsed.read().expect("RwLock read should not fail");
                return vec![
                    UiMsg::UpdatePosition(cur_pod_id, cur_ep_id, position),
                    UiMsg::Play(pod_id, ep_id, false),
                ];
            }
            return vec![UiMsg::Play(pod_id, ep_id, false)];
        } else if *self.playing.read().expect("RwLock read should not fail")
            == PlaybackStatus::Paused
        {
            self.tx_to_player
                .send(PlayerMessage::PlayPause)
                .inspect_err(|err| {
                    log::error!("Failed to send PlayerMessage::PlayPause to player: {err}");
                })
                .ok();
        }
        vec![]
    }

    pub(super) fn next_from_queue(&self, queue_index: usize) -> Option<ShareableRwLock<Episode>> {
        if queue_index + 1 < self.queue.items.len(false) {
            let ep_id = self.queue.items.get_id_by_index(queue_index + 1)?;
            self.queue.items.get(ep_id)
        } else {
            None
        }
    }
}
