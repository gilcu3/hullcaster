use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout},
    prelude::Rect,
    style::{Style, Stylize},
    text::Line,
    widgets::{Block, Clear, Gauge, HighlightSpacing, List, ListItem, Paragraph, Wrap},
};
use tui_input::Input;

use crate::{
    keymap::{Keybindings, UserAction},
    types::{Episode, Menuable, ShareableRwLock},
    utils::format_duration,
};

use super::colors::AppColors;
use super::notification::render_notification_line;
use super::{Details, MenuList, Panel, Popup, UiState};

impl UiState {
    #[allow(clippy::too_many_lines)]
    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();

        let vertical_layout = Layout::vertical([
            Constraint::Length(6),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]);
        let horizontal_layout =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]);
        let [play_area, center_area, notif_area, help_area] = vertical_layout.areas(area);
        let [select_area, queue_area] = horizontal_layout.areas(center_area);

        render_play_area(
            frame,
            play_area,
            &self.current_episode,
            self.current_podcast_title.as_ref(),
            *self.elapsed.read().expect("RwLock read should not fail"),
            &self.colors,
        );
        {
            let sp = self
                .sync_progress
                .read()
                .expect("RwLock read should not fail");
            let active = sp.is_active();
            let completed = sp.completed;
            let total = sp.total;
            drop(sp);
            self.podcasts.title = if active {
                format!("Podcasts (syncing {completed}/{total})")
            } else {
                "Podcasts".to_string()
            };
        }
        match self.left_panel {
            Panel::Podcasts => render_menuable_area(
                frame,
                select_area,
                &mut self.podcasts,
                &self.colors,
                self.active_panel == Panel::Podcasts,
            ),
            Panel::Episodes => render_menuable_area(
                frame,
                select_area,
                &mut self.episodes,
                &self.colors,
                self.active_panel == Panel::Episodes,
            ),
            Panel::Unplayed => render_menuable_area(
                frame,
                select_area,
                &mut self.unplayed,
                &self.colors,
                self.active_panel == Panel::Unplayed,
            ),
            Panel::Queue => {}
        }
        render_menuable_area(
            frame,
            queue_area,
            &mut self.queue,
            &self.colors,
            self.active_panel == Panel::Queue,
        );

        render_notification_line(frame, notif_area, &self.notification, &self.colors);
        render_help_line(frame, help_area, &self.keymap, &self.colors);

        if let Some(active_popup) = &self.active_popup {
            match active_popup {
                Popup::Welcome => {
                    render_welcome_popup(
                        frame,
                        compute_popup_area(area, 30, 30),
                        self.scroll_popup,
                        &self.keymap,
                        &self.colors,
                    );
                }
                Popup::Details => {
                    render_details_popup(
                        frame,
                        compute_popup_area(area, 70, 70),
                        self.current_details.as_ref(),
                        self.scroll_popup,
                        &self.colors,
                    );
                }
                Popup::Help => {
                    render_shortcut_help_popup(
                        frame,
                        compute_popup_area(area, 40, 70),
                        self.scroll_popup,
                        &self.keymap,
                        &self.colors,
                    );
                }
                Popup::AddPodcast => {
                    render_add_podcast_popup(
                        frame,
                        compute_popup_area(area, 30, 80),
                        &self.input,
                        &self.colors,
                    );
                }
                Popup::ConfirmRemovePodcast => {
                    render_confirmation_popup(
                        frame,
                        compute_popup_area(area, 30, 70),
                        "Do you want to remove the podcast?".to_string(),
                        &self.colors,
                    );
                }
                Popup::ConfirmQuit => {
                    render_confirmation_popup(
                        frame,
                        compute_popup_area(area, 30, 70),
                        "Do you want to quit the app?".to_string(),
                        &self.colors,
                    );
                }
            }
        }
    }
}

pub(super) fn render_confirmation_popup(
    frame: &mut Frame, area: Rect, msg: String, colors: &AppColors,
) {
    let [_, mid_area, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(area);
    let input = Paragraph::new(msg + " y/n")
        .style(colors.error)
        .block(Block::bordered());
    frame.render_widget(Clear, mid_area);
    frame.render_widget(input, mid_area);
}

#[allow(clippy::cast_possible_truncation)]
pub(super) fn render_add_podcast_popup(
    frame: &mut Frame, area: Rect, input: &Input, colors: &AppColors,
) {
    let [_, input_area, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(area);
    let width = area.width.max(3) - 3;
    let scroll = input.visual_scroll(width as usize);
    let input_text = Paragraph::new(input.value())
        .style(colors.normal)
        .scroll((0, scroll as u16))
        .block(Block::bordered().title("Podcast feed url:"));
    frame.render_widget(Clear, input_area);
    frame.render_widget(input_text, input_area);
    let x = input.visual_cursor().max(scroll) - scroll + 1;
    frame.set_cursor_position((area.x + x as u16, input_area.y + 1));
}

pub(super) fn render_shortcut_help_popup(
    frame: &mut Frame, area: Rect, scroll: u16, keymap: &Keybindings, colors: &AppColors,
) {
    let actions = vec![
        (Some(UserAction::Up), "Up:"),
        (Some(UserAction::Down), "Down:"),
        (Some(UserAction::PageUp), "Page up:"),
        (Some(UserAction::PageDown), "Page down:"),
        (Some(UserAction::GoTop), "Go to top:"),
        (Some(UserAction::GoBot), "Go to bottom:"),
        //(None, ""),
        (Some(UserAction::AddFeed), "Add feed:"),
        (Some(UserAction::Sync), "Refresh podcast:"),
        (Some(UserAction::SyncAll), "Refresh all podcasts:"),
        (Some(UserAction::SyncGpodder), "Sync with gpodder:"),
        //(None, ""),
        (Some(UserAction::Enter), "Open podcast/Play episode:"),
        (Some(UserAction::PlayPause), "Play/Pause:"),
        (Some(UserAction::Left), "Seek backward:"),
        (Some(UserAction::Right), "Seek forward:"),
        (Some(UserAction::MarkPlayed), "Mark as played:"),
        (Some(UserAction::MarkAllPlayed), "Mark all as played:"),
        //(None, ""),
        (Some(UserAction::Enqueue), "Enqueue:"),
        (Some(UserAction::Remove), "Remove from queue:"),
        (Some(UserAction::Download), "Download:"),
        (Some(UserAction::DownloadAll), "Download all:"),
        (Some(UserAction::Delete), "Delete file:"),
        (Some(UserAction::DeleteAll), "Delete all files:"),
        (Some(UserAction::UnplayedList), "Show/Hide Unplayed Panel"),
        (Some(UserAction::Help), "Help:"),
        (Some(UserAction::Back), "Back:"),
        (Some(UserAction::Quit), "Quit:"),
    ];
    let mut key_strs = Vec::new();
    let mut back_key = "<missing>".to_string();
    for (action, action_str) in actions {
        match action {
            Some(action) => {
                if let Some(keys) = keymap.keys_for_action(action) {
                    // longest prefix is 21 chars long
                    let key_str = match keys.len() {
                        0 => format!("{action_str:>28} <missing>"),
                        1 => format!("{:>28} \"{}\"", action_str, &keys[0]),
                        _ => format!("{:>28} \"{}\" or \"{}\"", action_str, &keys[0], &keys[1]),
                    };
                    if action == UserAction::Back && !keys.is_empty() {
                        back_key = format!("\"{}\"", keys[0]);
                    }
                    key_strs.push(key_str);
                }
            }
            None => key_strs.push(" ".to_string()),
        }
    }
    let line: Vec<Line> = key_strs.iter().map(|s| Line::from(s.as_str())).collect();
    let paragraph = Paragraph::new(line).scroll((scroll, 0));
    let last_line =
        Line::from(format!("Press {back_key} to close this window.")).alignment(Alignment::Right);

    let vertical = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]);

    let block = Block::bordered()
        .title("Available keybindings")
        .style(colors.normal);
    let inner = block.inner(area);
    let [keys, last] = vertical.areas(inner);

    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(paragraph, keys);
    frame.render_widget(last_line, last);
}

pub(super) fn render_welcome_popup(
    frame: &mut Frame, area: Rect, scroll: u16, keymap: &Keybindings, colors: &AppColors,
) {
    let actions = vec![UserAction::AddFeed, UserAction::Quit, UserAction::Help];
    let mut key_strs = Vec::new();
    for action in actions {
        if let Some(keys) = keymap.keys_for_action(action)
            && let Some(key) = keys.first()
        {
            key_strs.push(key);
        }
    }

    let line1 = format!(
        "Your podcast list is currently empty. Press \"{}\" to add a new podcast feed, \"{}\" to quit, or see all available commands by typing \"{}\" to get help.",
        key_strs[0], key_strs[1], key_strs[2]
    );
    let line2 = "More details of how to customize hullcaster can be found on the Github repo readme: https://github.com/gilcu3/hullcaster";
    let paragraph = Paragraph::new(vec![
        Line::from(""),
        Line::from(line1),
        Line::from(""),
        Line::from(line2),
    ])
    .scroll((scroll, 0))
    .wrap(Wrap { trim: true })
    .centered();

    let block = Block::bordered().title(" Welcome ").style(colors.normal);
    let inner = block.inner(area);

    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(paragraph, inner);
}

pub(super) fn render_details_popup(
    frame: &mut Frame, area: Rect, details: Option<&Details>, scroll: u16, colors: &AppColors,
) {
    if let Some(details) = details {
        let mut v = vec![];
        v.push(Line::from(""));

        if let Some(title) = &details.podcast_title {
            v.push(Line::from("Podcast: ".to_string() + title));
            v.push(Line::from(""));
        }

        if let Some(title) = &details.episode_title {
            v.push(Line::from("Episode: ".to_string() + title));
            v.push(Line::from(""));
        }

        if let Some(author) = &details.author {
            v.push(Line::from("Author: ".to_string() + author));
            v.push(Line::from(""));
        }

        if let Some(last_checked) = details.last_checked {
            v.push(Line::from(
                "Last checked: ".to_string() + format!("{last_checked}").as_str(),
            ));
            v.push(Line::from(""));
        }

        if let Some(date) = details.pubdate {
            v.push(Line::from(
                "Published: ".to_string() + format!("{date}").as_str(),
            ));
            v.push(Line::from(""));
        }

        if let Some(pos) = &details.position {
            v.push(Line::from("Elapsed: ".to_string() + pos));
            v.push(Line::from(""));
        }

        if let Some(dur) = &details.duration {
            v.push(Line::from("Duration: ".to_string() + dur));
            v.push(Line::from(""));
        }

        v.push(Line::from("URL: ".to_string() + &details.url));
        v.push(Line::from(""));

        if let Some(exp) = &details.explicit {
            v.push(Line::from(
                "Explicit: ".to_string() + { if *exp { "yes" } else { "no" } },
            ));
            v.push(Line::from(""));
        }

        match &details.description {
            Some(desc) => {
                v.push(Line::from("Description: "));
                for line in desc.lines() {
                    v.push(Line::from(line));
                }
            }
            None => {
                v.push(Line::from("No description."));
            }
        }
        let paragraph = Paragraph::new(v)
            .wrap(Wrap { trim: true })
            .scroll((scroll, 0));
        let block = Block::bordered().title(" Details ").style(colors.normal);
        let inner = block.inner(area);

        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        frame.render_widget(paragraph, inner);
    }
}

pub(super) fn render_help_line(
    frame: &mut Frame, area: Rect, keymap: &Keybindings, colors: &AppColors,
) {
    let actions = vec![
        (UserAction::Quit, "Quit"),
        (UserAction::Back, "Back"),
        (UserAction::Help, "Help"),
        (UserAction::Switch, "Switch"),
        (UserAction::Enter, "Open podcast/Play episode"),
        (UserAction::SyncAll, "Refresh podcasts"),
        (UserAction::SyncGpodder, "Sync"),
        (UserAction::MarkPlayed, "Mark as played"),
        (UserAction::AddFeed, "Add podcast"),
        (UserAction::Enqueue, "Enqueue"),
        (UserAction::Remove, "Remove"),
        (UserAction::UnplayedList, "Show/Hide Unplayed"),
    ];
    let mut cur_length = 0;
    let mut key_strs = Vec::new();
    for (action, action_str) in actions {
        if let Some(keys) = keymap.keys_for_action(action) {
            // longest prefix is 21 chars long
            let key_str = match keys.len() {
                0 => format!(":{action_str}"),
                _ => format!("{}:{}", &keys[0], action_str,),
            };
            if cur_length + key_str.len() > area.width as usize {
                break;
            }
            cur_length += key_str.len() + 3;
            key_strs.push(key_str);
        }
    }
    let line = Line::from(key_strs.join(" | "))
        .bg(colors.normal.1)
        .fg(colors.normal.0);
    frame.render_widget(line, area);
}

pub(super) fn compute_popup_area(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([Constraint::Percentage(percent_y)]).flex(Flex::Center);
    let horizontal = Layout::horizontal([Constraint::Percentage(percent_x)]).flex(Flex::Center);
    let [area] = vertical.areas(area);
    let [area] = horizontal.areas(area);
    area
}

pub(super) fn render_menuable_area<T: Menuable>(
    frame: &mut Frame, area: Rect, menu: &mut MenuList<T>, colors: &AppColors, active: bool,
) {
    let block = Block::bordered().title({
        let line = Line::from(format!(" {} ", menu.title));
        if active {
            line.style(colors.highlighted)
        } else {
            line.style(colors.normal)
        }
    });
    let text_width = block.inner(area).width as usize;
    let items: Vec<ListItem> = menu.items.map(
        |x| ListItem::from(x.get_title(text_width)).style(colors.normal),
        false,
    );

    let list = List::new(items)
        .block(block)
        .style(colors.normal)
        .highlight_style({
            if active {
                colors.highlighted
            } else {
                colors.normal
            }
        })
        .highlight_spacing(HighlightSpacing::Always);
    if !list.is_empty() && !menu.sync_state_with_selected() && menu.state.selected().is_none() {
        menu.state.select_first();
        menu.sync_selected_with_state();
    }

    frame.render_stateful_widget(list, area, &mut menu.state);
}

#[allow(clippy::cast_precision_loss)]
pub(super) fn compute_ratio(elapsed: u64, total: u64) -> f64 {
    (elapsed as f64 / total as f64).min(1.0)
}

pub(super) fn render_play_area(
    frame: &mut Frame, area: Rect, ep: &ShareableRwLock<Option<ShareableRwLock<Episode>>>,
    pod_title: Option<&String>, elapsed: u64, colors: &AppColors,
) {
    let block = Block::bordered()
        .title(Line::from(" Playing "))
        .style(colors.normal);
    let mut ratio = 0.0;
    let mut title = String::new();
    let mut podcast_title = String::new();
    let label = ep
        .read()
        .expect("RwLock read should not fail")
        .as_ref()
        .map_or_else(String::new, |ep| {
            let (ep_title, duration) = {
                let ep = ep.read().expect("RwLock read should not fail");
                (ep.title.clone(), ep.duration)
            };

            if let Some(total) = duration {
                ratio = compute_ratio(elapsed, total);
            }
            let total_label = format_duration(duration);
            title = ep_title;
            podcast_title = pod_title.map_or_else(String::new, std::clone::Clone::clone);
            format!("{}/{}", format_duration(Some(elapsed)), total_label)
        });
    let progress = Gauge::default()
        .gauge_style(Style::new().green().on_black())
        .label(label)
        .ratio(ratio);
    let inner_area = block.inner(area);
    let [episode_area, podcast_area, _, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner_area);
    frame.render_widget(block, area);
    frame.render_widget(Line::from(title), episode_area);
    frame.render_widget(Line::from(podcast_title), podcast_area);
    frame.render_widget(progress, bottom);
}
