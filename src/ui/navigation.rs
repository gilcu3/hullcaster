use crate::{
    config::SCROLL_AMOUNT,
    keymap::UserAction,
    types::Menuable,
    utils::{clean_html, format_duration},
};

use super::{Details, Panel, UiMsg, UiState};

impl UiState {
    pub(super) fn move_cursor(&mut self, action: UserAction) {
        if self.active_popup.is_some() {
            match action {
                UserAction::Down => {
                    self.scroll_popup = self.scroll_popup.saturating_add(1);
                }

                UserAction::Up => {
                    self.scroll_popup = self.scroll_popup.saturating_sub(1);
                }

                UserAction::PageUp => {
                    self.scroll_popup = self.scroll_popup.saturating_sub(SCROLL_AMOUNT);
                }

                UserAction::PageDown => {
                    self.scroll_popup = self.scroll_popup.saturating_add(SCROLL_AMOUNT);
                }

                UserAction::GoTop => {
                    self.scroll_popup = 0;
                }

                _ => (),
            }
        } else {
            let current_state = {
                match self.active_panel {
                    Panel::Podcasts => &mut self.podcasts.state,
                    Panel::Unplayed => &mut self.unplayed.state,
                    Panel::Episodes => &mut self.episodes.state,
                    Panel::Queue => &mut self.queue.state,
                }
            };
            match action {
                UserAction::Down => current_state.select_next(),

                UserAction::Up => current_state.select_previous(),

                UserAction::Left => match self.active_panel {
                    Panel::Podcasts | Panel::Unplayed => {}
                    Panel::Episodes => {
                        self.select_panel(&Panel::Podcasts);
                    }
                    Panel::Queue => {
                        self.queue.state.select(None);
                        self.select_panel(&self.left_panel.clone());
                    }
                },

                UserAction::Right => match self.active_panel {
                    Panel::Podcasts | Panel::Unplayed | Panel::Episodes => {
                        self.active_panel = Panel::Queue;
                        self.queue.state.select_first();
                    }
                    Panel::Queue => {}
                },

                UserAction::PageUp => current_state.scroll_up_by(SCROLL_AMOUNT),

                UserAction::PageDown => current_state.scroll_down_by(SCROLL_AMOUNT),

                UserAction::GoTop => current_state.select_first(),

                UserAction::GoBot => current_state.select_last(),

                _ => (),
            }

            match self.active_panel {
                Panel::Podcasts => self.podcasts.sync_selected_with_state(),
                Panel::Unplayed => self.unplayed.sync_selected_with_state(),
                Panel::Episodes => self.episodes.sync_selected_with_state(),
                Panel::Queue => self.queue.sync_selected_with_state(),
            }
        }
    }

    pub(super) const fn select_panel(&mut self, panel: &Panel) {
        match panel {
            Panel::Podcasts => {
                self.active_panel = Panel::Podcasts;
                self.left_panel = Panel::Podcasts;
            }
            Panel::Episodes => {
                self.active_panel = Panel::Episodes;
                self.left_panel = Panel::Episodes;
            }
            Panel::Unplayed => {
                self.active_panel = Panel::Unplayed;
                self.left_panel = Panel::Unplayed;
            }
            Panel::Queue => {
                self.active_panel = Panel::Queue;
            }
        }
    }

    pub(super) fn get_episode_id(&self) -> Option<i64> {
        match self.active_panel {
            Panel::Podcasts => None,
            Panel::Episodes => {
                let id = self.episodes.state.selected()?;
                self.episodes.items.map_single_by_index(id, |x| x.id)
            }
            Panel::Unplayed => {
                let id = self.unplayed.state.selected()?;
                self.unplayed.items.map_single_by_index(id, |x| x.id)
            }
            Panel::Queue => {
                let id = self.queue.state.selected()?;
                self.queue.items.map_single_by_index(id, |x| x.id)
            }
        }
    }

    pub(super) fn get_podcast_id(&self) -> Option<i64> {
        match self.active_panel {
            Panel::Podcasts => {
                let id = self.podcasts.state.selected()?;
                self.podcasts.items.map_single_by_index(id, |x| x.id)
            }
            Panel::Episodes => {
                let id = self.episodes.state.selected()?;
                self.episodes.items.map_single_by_index(id, |x| x.pod_id)
            }
            Panel::Unplayed => {
                let id = self.unplayed.state.selected()?;
                self.unplayed.items.map_single_by_index(id, |x| x.pod_id)
            }
            Panel::Queue => {
                let id = self.queue.state.selected()?;
                self.queue.items.map_single_by_index(id, |x| x.pod_id)
            }
        }
    }

    pub(super) fn move_eps(&mut self, action: UserAction) -> Option<UiMsg> {
        let selected = self.queue.state.selected()?;

        match action {
            UserAction::MoveDown => {
                if selected + 1 < self.queue.items.len(false) {
                    {
                        let mut order_vec = self.queue.items.borrow_order();
                        order_vec.swap(selected, selected + 1);
                    }
                    self.queue.state.select(Some(selected + 1));
                    self.queue.sync_selected_with_state();
                    return Some(UiMsg::QueueModified);
                }
            }
            UserAction::MoveUp => {
                if selected >= 1 {
                    {
                        let mut order_vec = self.queue.items.borrow_order();
                        order_vec.swap(selected, selected - 1);
                    }
                    self.queue.state.select(Some(selected - 1));
                    self.queue.sync_selected_with_state();
                    return Some(UiMsg::QueueModified);
                }
            }
            _ => (),
        }

        None
    }

    pub(super) fn mark_played(&self) -> Option<UiMsg> {
        let pod_id = self.get_podcast_id()?;
        let ep_id = self.get_episode_id()?;
        match self.active_panel {
            Panel::Episodes => {
                let played = self.episodes.items.map_single(ep_id, Menuable::is_played)?;
                Some(UiMsg::MarkPlayed(pod_id, ep_id, !played))
            }
            Panel::Unplayed => {
                let played = self.unplayed.items.map_single(ep_id, Menuable::is_played)?;
                Some(UiMsg::MarkPlayed(pod_id, ep_id, !played))
            }
            Panel::Queue => {
                let played = self.queue.items.map_single(ep_id, Menuable::is_played)?;
                Some(UiMsg::MarkPlayed(pod_id, ep_id, !played))
            }
            Panel::Podcasts => None,
        }
    }

    pub fn mark_all_played(&self) -> Option<UiMsg> {
        let pod_id = self.get_podcast_id()?;
        let played = self
            .podcasts
            .items
            .map_single(pod_id, Menuable::is_played)?;
        Some(UiMsg::MarkAllPlayed(pod_id, !played))
    }

    pub(super) fn remove_podcast(&self) -> Option<UiMsg> {
        let pod_id = self.get_podcast_id()?;
        Some(UiMsg::RemovePodcast(pod_id, true))
    }

    pub(super) fn construct_details_episode(&mut self) {
        if let Some(ep_id) = self.get_episode_id() {
            let ep = match self.active_panel {
                Panel::Episodes => self.episodes.items.get(ep_id),
                Panel::Queue => self.queue.items.get(ep_id),
                Panel::Unplayed => self.unplayed.items.get(ep_id),
                Panel::Podcasts => None,
            };
            if let Some(ep) = ep {
                let ep = ep.read().expect("RwLock read should not fail");
                let desc = clean_html(&ep.description);
                let podcast_title = {
                    let pod = self.podcasts.items.get(ep.pod_id);
                    pod.map(|pod| {
                        pod.read()
                            .expect("RwLock read should not fail")
                            .title
                            .clone()
                    })
                };
                self.current_details = Some(Details {
                    pubdate: ep.pubdate,
                    position: Some(format_duration(Some(ep.position))),
                    duration: Some(format_duration(ep.duration)),
                    explicit: None,
                    description: Some(desc),
                    author: None,
                    last_checked: None,
                    episode_title: Some(ep.title.clone()),
                    podcast_title,
                    url: ep.url.clone(),
                });
            }
        }
    }

    pub(super) fn construct_details_podcast(&mut self) {
        if let Some(pod_id) = self.get_podcast_id()
            && let Some(pod) = self.podcasts.items.get(pod_id)
        {
            let pod = pod.read().expect("RwLock read should not fail");
            let desc = pod.description.clone().map(|desc| clean_html(&desc));
            self.current_details = Some(Details {
                pubdate: None,
                position: None,
                duration: None,
                explicit: pod.explicit,
                description: desc,
                author: pod.author.clone(),
                last_checked: Some(pod.last_checked),
                episode_title: None,
                podcast_title: Some(pod.title.clone()),
                url: pod.url.clone(),
            });
        }
    }

    pub(super) fn construct_current_episode(&mut self, ep_id: i64) {
        let ep = match self.active_panel {
            Panel::Episodes => self.episodes.items.get(ep_id),
            Panel::Queue => self.queue.items.get(ep_id),
            Panel::Unplayed => self.unplayed.items.get(ep_id),
            Panel::Podcasts => None,
        };
        if let Some(ep_arc) = ep {
            let ep_pod_id = ep_arc.read().expect("RwLock read should not fail").pod_id;
            let podcast_title = {
                let pod = self.podcasts.items.get(ep_pod_id);
                pod.map_or_else(
                    || "No title".to_string(),
                    |pod| {
                        pod.read()
                            .expect("RwLock read should not fail")
                            .title
                            .clone()
                    },
                )
            };
            self.current_podcast_title = Some(podcast_title);
            *self
                .current_episode
                .write()
                .expect("RwLock write should not fail") = Some(ep_arc);
        }
    }
}
