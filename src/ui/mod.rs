use std::io::{self, Write};
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use crossterm::{
    self, cursor,
    event::{self, Event},
    execute, terminal,
};

#[cfg_attr(not(test), path = "panel.rs")]
#[cfg_attr(test, path = "mock_panel.rs")]
mod panel;

pub mod colors;
mod details_panel;
mod keybindings;
mod menu;
mod notification;
mod popup;

use self::colors::AppColors;
use self::details_panel::{Details, DetailsPanel};
use self::keybindings::KeybindingsWin;
use self::menu::Menu;
use self::notification::NotifWin;
use self::panel::Panel;
use self::popup::PopupWin;

use super::MainMessage;
use crate::config::Config;
use crate::keymap::{Keybindings, UserAction};
use crate::types::*;
use crate::utils::clean_html;

/// Amount of time between ticks in the event loop
const TICK_RATE: u64 = 10;

/// Enum used for communicating back to the main controller after user
/// input has been captured by the UI. usize values always represent the
/// selected podcast, and (if applicable), the selected episode, in that
/// order.
#[derive(Debug)]
pub enum UiMsg {
    AddFeed(String),
    Play(i64, i64),
    MarkPlayed(i64, i64, bool),
    MarkAllPlayed(i64, bool),
    Sync(i64),
    SyncAll,
    SyncGpodder,
    Download(i64, i64),
    DownloadMulti(Vec<(i64, i64)>),
    DownloadAll(i64),
    Delete(i64, i64),
    DeleteAll(i64),
    RemovePodcast(i64, bool),
    FilterChange(FilterType),
    Quit,
    Noop,
}

/// Holds a value for how much to scroll the menu up or down, without
/// having to deal with positive/negative values.
pub enum Scroll {
    Up(u16),
    Down(u16),
}

pub enum Move {
    Up,
    Down,
}

/// Simple enum to identify which menu is currently active.
#[derive(Debug)]
enum ActivePanel {
    PodcastMenu,
    EpisodeMenu,
    QueueMenu,
    DetailsPanel,
}

/// Struct containing all interface elements of the TUI. Functionally,
/// it encapsulates the terminal menus and panels, and holds data about
/// the size of the screen.
#[derive(Debug)]
pub struct Ui {
    n_row: u16,
    n_col: u16,
    keymap: Keybindings,
    colors: Rc<AppColors>,
    podcast_menu: Menu<Podcast>,
    episode_menu: Menu<Episode>,
    queue_menu: Menu<Episode>,
    details_panel: Option<DetailsPanel>,
    active_panel: ActivePanel,
    notif_win: NotifWin,
    popup_win: PopupWin,
    keybindings_win: KeybindingsWin,
}

impl Ui {
    /// Spawns a UI object in a new thread, with message channels to send
    /// and receive messages
    pub fn spawn(
        config: Arc<Config>, items: LockVec<Podcast>, queue_items: LockVec<Episode>,
        rx_from_main: mpsc::Receiver<MainMessage>, tx_to_main: mpsc::Sender<Message>,
    ) -> thread::JoinHandle<()> {
        return thread::spawn(move || {
            let mut ui = Ui::new(config, items, queue_items);
            ui.init();
            let mut message_iter = rx_from_main.try_iter();
            // this is the main event loop: on each loop, we update
            // any messages at the bottom, check for user input, and
            // then process any messages from the main thread
            loop {
                ui.notif_win.check_notifs();

                match ui.getch() {
                    UiMsg::Noop => (),
                    input => tx_to_main
                        .send(Message::Ui(input))
                        .expect("Thread messaging error"),
                }

                if let Some(message) = message_iter.next() {
                    match message {
                        MainMessage::UiUpdateMenus => ui.update_menus(),
                        MainMessage::UiSpawnNotif(msg, duration, error) => {
                            ui.timed_notif(msg, error, duration)
                        }
                        MainMessage::UiSpawnPersistentNotif(msg, error) => {
                            ui.persistent_notif(msg, error)
                        }
                        MainMessage::UiClearPersistentNotif => ui.clear_persistent_notif(),
                        MainMessage::UiTearDown => {
                            ui.tear_down();
                            break;
                        }
                        MainMessage::UiSpawnDownloadPopup(episodes, selected) => {
                            ui.popup_win.spawn_download_win(episodes, selected);
                        }
                    }
                }

                io::stdout().flush().unwrap();
            }
        });
    }

    /// Initializes the UI with a list of podcasts and podcast episodes,
    /// creates the menus and panels, and returns a UI object for future
    /// manipulation.
    pub fn new(config: Arc<Config>, items: LockVec<Podcast>, queue_items: LockVec<Episode>) -> Ui {
        terminal::enable_raw_mode().expect("Terminal can't run in raw mode.");
        execute!(
            io::stdout(),
            terminal::EnterAlternateScreen,
            terminal::Clear(terminal::ClearType::All),
            cursor::Hide
        )
        .expect("Can't draw to screen.");

        let colors = Rc::new(config.colors.clone());

        let (n_col, n_row) = terminal::size().expect("Can't get terminal size");
        let (pod_col, det_col, queue_col) = Self::calculate_sizes(n_col);

        let first_pod = match items.borrow_filtered_order().first() {
            Some(first_id) => match items.borrow_map().get(first_id) {
                Some(pod) => pod.episodes.clone(),
                None => LockVec::new(Vec::new()),
            },
            None => LockVec::new(Vec::new()),
        };

        let podcast_panel = Panel::new(
            "Podcasts".to_string(),
            0,
            colors.clone(),
            n_row - 2,
            pod_col,
            0,
            (0, 0, 0, 0),
        );
        let podcast_menu = Menu::new(podcast_panel, None, items);

        let episode_panel = Panel::new(
            "Episodes".to_string(),
            0,
            colors.clone(),
            n_row - 2,
            pod_col,
            0,
            (0, 0, 0, 0),
        );

        let episode_menu = Menu::new(episode_panel, None, first_pod);

        let details_panel = if det_col > 1 {
            Some(DetailsPanel::new(
                "Details".to_string(),
                1,
                colors.clone(),
                n_row - 2,
                det_col,
                pod_col - 1,
                (0, 1, 0, 1),
            ))
        } else {
            None
        };

        let queue_panel = Panel::new(
            "Queue".to_string(),
            2,
            colors.clone(),
            n_row - 2,
            queue_col,
            pod_col + det_col - 2,
            (0, 0, 0, 0),
        );
        // This should load from the database
        //let queue_items: LockVec<Episode> = LockVec::new(Vec::new());
        let queue_menu = Menu::new(queue_panel, None, queue_items);

        let notif_win = NotifWin::new(colors.clone(), n_row - 2, n_col);
        let popup_win = PopupWin::new(&config.keybindings, colors.clone(), n_row - 1, n_col);

        let keybindings_win =
            KeybindingsWin::new(&config.keybindings, colors.clone(), n_row - 1, n_col);

        Ui {
            n_row,
            n_col,
            keymap: config.keybindings.clone(),
            colors,
            podcast_menu,
            episode_menu,
            queue_menu,
            details_panel,
            active_panel: ActivePanel::PodcastMenu,
            notif_win,
            popup_win,
            keybindings_win,
        }
    }

    /// This should be called immediately after creating the UI, in order
    /// to draw everything to the screen.
    pub fn init(&mut self) {
        self.podcast_menu.visible = true;
        self.queue_menu.visible = true;
        self.episode_menu.visible = false;
        self.podcast_menu.activate();
        self.podcast_menu.redraw();
        //self.episode_menu.redraw();
        self.queue_menu.redraw();
        self.highlight_items();

        self.active_panel = ActivePanel::PodcastMenu;

        self.update_details_panel(false);

        self.notif_win.redraw();
        self.keybindings_win.redraw();

        // welcome screen if user does not have any podcasts yet
        if self.podcast_menu.items.is_empty() {
            self.popup_win.spawn_welcome_win();
        }
        io::stdout().flush().unwrap();
    }

    /// Waits for user input and, where necessary, provides UiMsgs
    /// back to the main controller.
    ///
    /// Anything UI-related (e.g., scrolling up and down menus) is
    /// handled internally, producing an empty UiMsg. This allows for
    /// some greater degree of abstraction; for example, input to add a
    /// new podcast feed spawns a UI window to capture the feed URL, and
    /// only then passes this data back to the main controller.
    pub fn getch(&mut self) -> UiMsg {
        if event::poll(Duration::from_millis(TICK_RATE)).expect("Can't poll for inputs") {
            match event::read().expect("Can't read inputs") {
                Event::Resize(n_col, n_row) => self.resize(n_col, n_row),
                Event::Key(input) => {
                    let (curr_pod_id, curr_sel_id) = self.get_current_ids();

                    // get rid of the "welcome" window once the podcast
                    // list is no longer empty
                    if self.popup_win.welcome_win && !self.podcast_menu.items.is_empty() {
                        self.popup_win.turn_off_welcome_win();
                    }

                    // if there is a popup window active (apart from the
                    // welcome window which takes no input), then
                    // redirect user input there
                    if self.popup_win.is_non_welcome_popup_active() {
                        let popup_msg = self.popup_win.handle_input(input);

                        // need to check if popup window is still active,
                        // as handling character input above may involve
                        // closing the popup window
                        if !self.popup_win.is_popup_active() {
                            self.update_menus();
                            io::stdout().flush().unwrap();
                        }
                        return popup_msg;
                    } else {
                        let action = self.keymap.get_from_input(input).cloned();
                        match action {
                            Some(a @ UserAction::Down)
                            | Some(a @ UserAction::Up)
                            | Some(a @ UserAction::Left)
                            | Some(a @ UserAction::Right)
                            | Some(a @ UserAction::PageUp)
                            | Some(a @ UserAction::PageDown)
                            | Some(a @ UserAction::BigUp)
                            | Some(a @ UserAction::BigDown)
                            | Some(a @ UserAction::GoTop)
                            | Some(a @ UserAction::GoBot) => self.move_cursor(&a, curr_sel_id),

                            Some(a @ UserAction::MoveUp) | Some(a @ UserAction::MoveDown) => {
                                if let ActivePanel::QueueMenu = self.active_panel {
                                    self.move_eps(&a, curr_sel_id);
                                }
                            }

                            Some(UserAction::AddFeed) => {
                                let url = &self.spawn_input_notif("Feed URL: ");
                                if !url.is_empty() {
                                    return UiMsg::AddFeed(url.to_string());
                                }
                            }

                            Some(UserAction::Sync) => {
                                if let ActivePanel::PodcastMenu = self.active_panel {
                                    if let Some(pod_id) = curr_sel_id {
                                        return UiMsg::Sync(pod_id);
                                    }
                                }
                            }
                            Some(UserAction::SyncAll) => {
                                return UiMsg::SyncAll;
                            }

                            Some(UserAction::SyncGpodder) => {
                                return UiMsg::SyncGpodder;
                            }

                            Some(UserAction::Enter) => match self.active_panel {
                                ActivePanel::PodcastMenu => {
                                    self.active_panel = ActivePanel::EpisodeMenu;
                                    self.episode_menu.visible = true;
                                    self.podcast_menu.visible = false;
                                    self.podcast_menu.deactivate(false);
                                    self.episode_menu.activate();
                                    self.episode_menu.redraw();
                                    self.highlight_items();
                                    self.update_details_panel(false);
                                }
                                ActivePanel::QueueMenu => {
                                    if let Some(pod_id) = curr_pod_id {
                                        if let Some(ep_id) = curr_sel_id {
                                            return UiMsg::Play(pod_id, ep_id);
                                        }
                                    }
                                }
                                ActivePanel::EpisodeMenu => {
                                    if let Some(pod_id) = curr_pod_id {
                                        if let Some(ep_id) = curr_sel_id {
                                            return UiMsg::Play(pod_id, ep_id);
                                        }
                                    }
                                }
                                ActivePanel::DetailsPanel => {}
                            },

                            Some(UserAction::Enqueue) => {
                                if let ActivePanel::EpisodeMenu = self.active_panel {
                                    if let Some(ep_id) = curr_sel_id {
                                        let ep = self.episode_menu.items.get(ep_id).unwrap();
                                        self.queue_menu.items.push(ep);
                                        self.queue_menu.redraw();
                                    }
                                }
                            }

                            Some(UserAction::Play) => {
                                if let Some(pod_id) = curr_pod_id {
                                    if let Some(ep_id) = curr_sel_id {
                                        return UiMsg::Play(pod_id, ep_id);
                                    }
                                }
                            }
                            Some(UserAction::MarkPlayed) => match self.active_panel {
                                ActivePanel::EpisodeMenu | ActivePanel::QueueMenu => {
                                    if let Some(ui_msg) = self.mark_played(curr_pod_id, curr_sel_id)
                                    {
                                        return ui_msg;
                                    }
                                }
                                _ => {}
                            },
                            Some(UserAction::MarkAllPlayed) => {
                                if let ActivePanel::EpisodeMenu = self.active_panel {
                                    if let Some(ui_msg) = self.mark_all_played(curr_pod_id) {
                                        return ui_msg;
                                    }
                                }
                            }

                            Some(UserAction::Download) => match self.active_panel {
                                ActivePanel::EpisodeMenu | ActivePanel::QueueMenu => {
                                    if let Some(pod_id) = curr_pod_id {
                                        if let Some(ep_id) = curr_sel_id {
                                            return UiMsg::Download(pod_id, ep_id);
                                        }
                                    }
                                }
                                _ => {}
                            },
                            Some(UserAction::DownloadAll) => {
                                if let ActivePanel::PodcastMenu = self.active_panel {
                                    if let Some(pod_id) = curr_pod_id {
                                        return UiMsg::DownloadAll(pod_id);
                                    }
                                }
                            }

                            Some(UserAction::Delete) => {
                                if let ActivePanel::EpisodeMenu = self.active_panel {
                                    if let Some(pod_id) = curr_pod_id {
                                        if let Some(ep_id) = curr_sel_id {
                                            return UiMsg::Delete(pod_id, ep_id);
                                        }
                                    }
                                }
                            }
                            Some(UserAction::DeleteAll) => {
                                if let ActivePanel::PodcastMenu = self.active_panel {
                                    if let Some(pod_id) = curr_pod_id {
                                        return UiMsg::DeleteAll(pod_id);
                                    }
                                }
                            }

                            Some(UserAction::Remove) => match self.active_panel {
                                ActivePanel::PodcastMenu => {
                                    if let Some(ui_msg) = self.remove_podcast(curr_pod_id) {
                                        self.highlight_items();
                                        return ui_msg;
                                    }
                                }
                                ActivePanel::QueueMenu => {
                                    if let Some(ep_id) = curr_sel_id {
                                        self.queue_menu.items.remove(ep_id);
                                        self.queue_menu.redraw();
                                        self.highlight_items();
                                        self.update_details_panel(false);
                                    }
                                }
                                ActivePanel::EpisodeMenu => {}
                                ActivePanel::DetailsPanel => {}
                            },

                            Some(UserAction::FilterPlayed) => {
                                return UiMsg::FilterChange(FilterType::Played);
                            }
                            Some(UserAction::FilterDownloaded) => {
                                return UiMsg::FilterChange(FilterType::Downloaded);
                            }

                            Some(UserAction::Help) => self.popup_win.spawn_help_win(),

                            Some(UserAction::Quit) => {
                                return UiMsg::Quit;
                            }
                            None => (),
                        } // end of input match
                    }
                }
                _ => (),
            }
        } // end of poll()
        std::thread::sleep(Duration::from_millis(TICK_RATE));
        UiMsg::Noop
    }

    /// Resize all the windows on the screen and redraw them.
    pub fn resize(&mut self, n_col: u16, n_row: u16) {
        self.n_row = n_row;
        self.n_col = n_col;

        let (pod_col, det_col, queue_col) = Self::calculate_sizes(n_col);

        self.podcast_menu.resize(n_row - 2, pod_col, 0);
        self.episode_menu.resize(n_row - 2, pod_col, 0);
        self.queue_menu
            .resize(n_row - 2, queue_col, pod_col + det_col - 2);
        self.highlight_items();

        if self.details_panel.is_some() {
            if det_col > 1 {
                let det = self.details_panel.as_mut().unwrap();
                det.resize(n_row - 2, det_col, pod_col - 1);
                // resizing the menus may change which item is selected
                self.update_details_panel(false);
            } else {
                self.details_panel = None;
                // if the details panel is currently active, but the
                // terminal is resized so the panel disappears, switch
                // the active focus to the episode menu automatically
                if let ActivePanel::DetailsPanel = self.active_panel {
                    self.active_panel = ActivePanel::PodcastMenu;
                    self.podcast_menu.activate();
                    self.highlight_items();
                }
            }
        } else if det_col > 1 {
            self.details_panel = Some(DetailsPanel::new(
                "Details".to_string(),
                1,
                self.colors.clone(),
                n_row - 2,
                det_col,
                pod_col - 1,
                (0, 1, 0, 1),
            ));
            self.update_details_panel(false);
        }

        self.popup_win.resize(n_row - 1, n_col);
        self.notif_win.resize(n_row - 2, n_col);
        self.keybindings_win.resize(n_row - 1, n_col);
    }

    /// Move the menu cursor around and redraw menus when necessary.
    pub fn move_cursor(&mut self, action: &UserAction, curr_sel_id: Option<i64>) {
        match action {
            UserAction::Down => {
                if curr_sel_id.is_some() {
                    self.scroll_current_window(Scroll::Down(1));
                }
            }

            UserAction::Up => {
                if curr_sel_id.is_some() {
                    self.scroll_current_window(Scroll::Up(1));
                }
            }

            UserAction::Left => match self.active_panel {
                ActivePanel::PodcastMenu => {}
                ActivePanel::EpisodeMenu => {
                    self.active_panel = ActivePanel::PodcastMenu;
                    self.episode_menu.visible = false;
                    self.podcast_menu.visible = true;
                    self.episode_menu.deactivate(false);
                    self.podcast_menu.activate();
                    self.podcast_menu.redraw();
                    self.highlight_items();
                    self.update_details_panel(false);
                }
                ActivePanel::QueueMenu => {
                    if self.details_panel.is_some() {
                        self.update_details_panel(true);
                        self.active_panel = ActivePanel::DetailsPanel;
                    } else if self.podcast_menu.visible {
                        self.active_panel = ActivePanel::PodcastMenu;
                        self.podcast_menu.activate();
                        self.podcast_menu.redraw();
                    } else if self.episode_menu.visible {
                        self.active_panel = ActivePanel::EpisodeMenu;
                        self.episode_menu.activate();
                        self.episode_menu.redraw();
                    } else {
                        log::error!("No menu is visible on the left");
                    }

                    self.queue_menu.deactivate(false);
                    self.queue_menu.redraw();
                    self.highlight_items();
                }
                ActivePanel::DetailsPanel => {
                    if self.podcast_menu.visible {
                        self.active_panel = ActivePanel::PodcastMenu;
                        self.podcast_menu.activate();
                        self.podcast_menu.redraw();
                    } else if self.episode_menu.visible {
                        self.active_panel = ActivePanel::EpisodeMenu;
                        self.episode_menu.activate();
                        self.episode_menu.redraw();
                    } else {
                        log::error!("No menu is visible on the left");
                    }
                    self.highlight_items();
                    self.update_details_panel(false);
                }
            },

            UserAction::Right => match self.active_panel {
                ActivePanel::PodcastMenu => {
                    if self.details_panel.is_some() {
                        self.update_details_panel(true);
                        self.active_panel = ActivePanel::DetailsPanel;
                    } else {
                        self.active_panel = ActivePanel::QueueMenu;
                        self.queue_menu.activate();
                        self.queue_menu.redraw();
                    }

                    self.podcast_menu.deactivate(false);
                    self.podcast_menu.redraw();
                    self.highlight_items();
                }
                ActivePanel::EpisodeMenu => {
                    self.update_details_panel(true);
                    self.active_panel = ActivePanel::DetailsPanel;
                    self.episode_menu.deactivate(false);
                    self.episode_menu.redraw();
                    self.highlight_items();
                }
                ActivePanel::QueueMenu => {}
                ActivePanel::DetailsPanel => {
                    self.active_panel = ActivePanel::QueueMenu;
                    self.queue_menu.activate();
                    self.queue_menu.redraw();
                    self.highlight_items();
                    self.update_details_panel(false);
                }
            },

            UserAction::PageUp => {
                if curr_sel_id.is_some() {
                    self.scroll_current_window(Scroll::Up(self.n_row - 3));
                }
            }

            UserAction::PageDown => {
                if curr_sel_id.is_some() {
                    self.scroll_current_window(Scroll::Down(self.n_row - 3));
                }
            }

            UserAction::BigUp => {
                if curr_sel_id.is_some() {
                    self.scroll_current_window(Scroll::Up(
                        self.n_row / crate::config::BIG_SCROLL_AMOUNT,
                    ));
                }
            }

            UserAction::BigDown => {
                if curr_sel_id.is_some() {
                    self.scroll_current_window(Scroll::Down(
                        self.n_row / crate::config::BIG_SCROLL_AMOUNT,
                    ));
                }
            }

            UserAction::GoTop => {
                if curr_sel_id.is_some() {
                    self.scroll_current_window(Scroll::Up(u16::MAX));
                }
            }

            UserAction::GoBot => {
                if curr_sel_id.is_some() {
                    self.scroll_current_window(Scroll::Down(u16::MAX));
                }
            }

            // this shouldn't occur because we only trigger this
            // function when the UserAction is Up, Down, Left, Right,
            // BigUp, BigDown, PageUp, PageDown, GoBot and GoTop
            _ => (),
        }
    }

    pub fn move_eps(&mut self, action: &UserAction, curr_sel_id: Option<i64>) {
        match action {
            UserAction::MoveDown => {
                if let Some(_curr_sel_id) = curr_sel_id {
                    self.queue_menu.move_item(Move::Down);
                }
            }

            UserAction::MoveUp => {
                if let Some(_curr_sel_id) = curr_sel_id {
                    self.queue_menu.move_item(Move::Up);
                }
            }
            _ => (),
        }
    }

    /// Scrolls the current active menu by the specified amount and
    /// refreshes the window.
    pub fn scroll_current_window(&mut self, scroll: Scroll) {
        match self.active_panel {
            ActivePanel::PodcastMenu => {
                if !self.podcast_menu.scroll(scroll) {
                    return;
                }

                self.episode_menu.top_row = 0;
                self.episode_menu.selected = 0;

                // update episodes menu with new list
                self.episode_menu.items = self.podcast_menu.get_episodes();
                self.update_details_panel(false);
            }
            ActivePanel::EpisodeMenu => {
                if !self.episode_menu.scroll(scroll) {
                    return;
                }
                self.update_details_panel(false);
            }
            ActivePanel::QueueMenu => {
                if !self.queue_menu.scroll(scroll) {
                    return;
                }
                self.update_details_panel(false);
            }
            ActivePanel::DetailsPanel => {
                if let Some(ref mut det) = self.details_panel {
                    log::info!("Scrolling details panel");
                    det.scroll(scroll);
                }
            }
        }
    }

    /// Mark an episode as played or unplayed (opposite of its current
    /// status).
    pub fn mark_played(
        &mut self, curr_pod_id: Option<i64>, curr_ep_id: Option<i64>,
    ) -> Option<UiMsg> {
        if let Some(pod_id) = curr_pod_id {
            if let Some(ep_id) = curr_ep_id {
                match self.active_panel {
                    ActivePanel::EpisodeMenu => {
                        if let Some(played) = self
                            .episode_menu
                            .items
                            .map_single(ep_id, |ep| ep.is_played())
                        {
                            return Some(UiMsg::MarkPlayed(pod_id, ep_id, !played));
                        }
                    }
                    ActivePanel::QueueMenu => {
                        if let Some(played) =
                            self.queue_menu.items.map_single(ep_id, |ep| ep.is_played())
                        {
                            return Some(UiMsg::MarkPlayed(pod_id, ep_id, !played));
                        }
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// Mark all episodes for a given podcast as played or unplayed. If
    /// there are any unplayed episodes, this will convert all episodes
    /// to played; if all are played already, only then will it convert
    /// all to unplayed.
    pub fn mark_all_played(&mut self, curr_pod_id: Option<i64>) -> Option<UiMsg> {
        if let Some(pod_id) = curr_pod_id {
            if let Some(played) = self
                .podcast_menu
                .items
                .map_single(pod_id, |pod| pod.is_played())
            {
                return Some(UiMsg::MarkAllPlayed(pod_id, !played));
            }
        }
        None
    }

    /// Remove a podcast from the list.
    pub fn remove_podcast(&mut self, curr_pod_id: Option<i64>) -> Option<UiMsg> {
        let confirm = self.ask_for_confirmation("Are you sure you want to remove the podcast?");
        // If we don't get a confirmation to delete, then don't remove
        if !confirm {
            return None;
        }
        let mut delete = false;

        if let Some(pod_id) = curr_pod_id {
            // check if we have local files first and if so, ask whether
            // to delete those too
            if self.check_for_local_files(pod_id) {
                let ask_delete = self.spawn_yes_no_notif("Delete local files too?");
                delete = ask_delete.unwrap_or(false); // default not to delete
            }

            return Some(UiMsg::RemovePodcast(pod_id, delete));
        }
        None
    }

    /// Based on the current selected value of the podcast and episode
    /// menus, returns the IDs of the current podcast and episode (if
    /// they exist).
    pub fn get_current_ids(&self) -> (Option<i64>, Option<i64>) {
        let current_pod_index = (self.podcast_menu.selected + self.podcast_menu.top_row) as usize;
        let pod_id = self
            .podcast_menu
            .items
            .borrow_filtered_order()
            .get(current_pod_index)
            .copied();
        match self.active_panel {
            ActivePanel::PodcastMenu => (pod_id, pod_id),
            ActivePanel::EpisodeMenu => {
                let current_ep_index =
                    (self.episode_menu.selected + self.episode_menu.top_row) as usize;
                let ep_id = self
                    .episode_menu
                    .items
                    .borrow_filtered_order()
                    .get(current_ep_index)
                    .copied();
                if ep_id.is_none() {
                    (None, None)
                } else {
                    let ep = self.episode_menu.items.get(ep_id.unwrap());
                    (ep.as_ref().map(|e| e.pod_id), ep.as_ref().map(|e| e.id))
                }
            }
            ActivePanel::QueueMenu => {
                let current_ep_index =
                    (self.queue_menu.selected + self.queue_menu.top_row) as usize;
                let ep_id = self
                    .queue_menu
                    .items
                    .borrow_filtered_order()
                    .get(current_ep_index)
                    .copied();
                if ep_id.is_none() {
                    (None, None)
                } else {
                    let ep = self.queue_menu.items.get(ep_id.unwrap());
                    (ep.as_ref().map(|e| e.pod_id), ep.as_ref().map(|e| e.id))
                }
            }
            ActivePanel::DetailsPanel => (pod_id, pod_id),
        }
    }

    /// Calculates the number of columns to allocate for each of the main
    /// panels: podcast menu, queue, and details panel; if the screen is too
    /// small to display the details panel, this size will be 0
    pub fn calculate_sizes(n_col: u16) -> (u16, u16, u16) {
        let pod_col;
        let queue_col;
        let det_col;
        if n_col > crate::config::DETAILS_PANEL_LENGTH {
            pod_col = (n_col + 2) / 3;
            queue_col = (n_col + 2) / 3;
            det_col = n_col + 2 - pod_col - queue_col;
        } else {
            pod_col = (n_col + 1) / 2;
            queue_col = n_col + 1 - pod_col;
            det_col = 1;
        }
        (pod_col, det_col, queue_col)
    }

    /// Checks whether the user has downloaded any episodes for the
    /// given podcast to their local system.
    pub fn check_for_local_files(&self, pod_id: i64) -> bool {
        let mut any_downloaded = false;
        let borrowed_map = self.podcast_menu.items.borrow_map();
        let borrowed_pod = borrowed_map
            .get(&pod_id)
            .expect("Could not retrieve podcast info.");

        let borrowed_ep_list = borrowed_pod.episodes.borrow_map();

        for (_ep_id, ep) in borrowed_ep_list.iter() {
            if ep.path.is_some() {
                any_downloaded = true;
                break;
            }
        }
        any_downloaded
    }

    /// Spawns a "(y/n)" notification with the specified input
    /// `message` using `spawn_input_notif`. If the the user types
    /// 'y', then the function returns `true`, and 'n' returns
    /// `false`. Cancelling the action returns `false` as well.
    pub fn ask_for_confirmation(&self, message: &str) -> bool {
        self.spawn_yes_no_notif(message).unwrap_or(false)
    }

    /// Adds a notification to the bottom of the screen that solicits
    /// user text input. A prefix can be specified as a prompt for the
    /// user at the beginning of the input line. This returns the user's
    /// input; if the user cancels their input, the String will be empty.
    pub fn spawn_input_notif(&self, prefix: &str) -> String {
        self.notif_win.input_notif(prefix)
    }

    /// Adds a notification to the bottom of the screen that solicits
    /// user for a yes/no input. A prefix can be specified as a prompt
    /// for the user at the beginning of the input line. "(y/n)" will
    /// automatically be appended to the end of the prefix. If the user
    /// types 'y' or 'n', the boolean will represent this value. If the
    /// user cancels the input or types anything else, the function will
    /// return None.
    pub fn spawn_yes_no_notif(&self, prefix: &str) -> Option<bool> {
        let mut out_val = None;
        let input = self.notif_win.input_notif(&format!("{prefix} (y/n) "));
        if let Some(c) = input.trim().chars().next() {
            if c == 'Y' || c == 'y' {
                out_val = Some(true);
            } else if c == 'N' || c == 'n' {
                out_val = Some(false);
            }
        }
        out_val
    }

    /// Adds a notification to the bottom of the screen for `duration`
    /// time (in milliseconds). Useful for presenting error messages,
    /// among other things.
    pub fn timed_notif(&mut self, message: String, duration: u64, error: bool) {
        self.notif_win.timed_notif(message, duration, error);
    }

    /// Adds a notification to the bottom of the screen that will stay on
    /// screen indefinitely. Must use `clear_persistent_msg()` to erase.
    pub fn persistent_notif(&mut self, message: String, error: bool) {
        self.notif_win.persistent_notif(message, error);
    }

    /// Clears any persistent notification that is being displayed at the
    /// bottom of the screen. Does not affect timed notifications, user
    /// input notifications, etc.
    pub fn clear_persistent_notif(&mut self) {
        self.notif_win.clear_persistent_notif();
    }

    /// Forces the menus to check the list of podcasts/episodes again and
    /// update.
    pub fn update_menus(&mut self) {
        if self.podcast_menu.visible {
            self.podcast_menu.redraw();
        }
        if self.episode_menu.visible {
            self.episode_menu.redraw();
        }
        self.queue_menu.redraw();
        if let Some(details_panel) = self.details_panel.as_ref() {
            let active = details_panel.panel.active;
            self.update_details_panel(active);
        }

        self.highlight_items();
    }

    /// Forces the menus to redraw the highlighted item.
    pub fn highlight_items(&mut self) {
        match self.active_panel {
            ActivePanel::PodcastMenu => {
                self.podcast_menu.highlight_selected();
            }
            ActivePanel::EpisodeMenu => {
                self.episode_menu.highlight_selected();
            }
            ActivePanel::QueueMenu => {
                self.queue_menu.highlight_selected();
            }
            _ => (),
        }
    }

    /// When the program is ending, this performs tear-down functions so
    /// that the terminal is properly restored to its prior settings.
    pub fn tear_down(&self) {
        terminal::disable_raw_mode().unwrap();
        execute!(
            io::stdout(),
            terminal::Clear(terminal::ClearType::All),
            terminal::LeaveAlternateScreen,
            cursor::Show
        )
        .unwrap();
    }

    /// Updates the details panel with information about the current
    /// podcast and episode, and redraws to the screen.
    pub fn update_details_panel(&mut self, active: bool) -> Option<()> {
        if self.details_panel.is_some() {
            let (curr_pod_id, curr_ep_id) = self.get_current_ids();
            let det = self.details_panel.as_mut().unwrap();
            det.panel.active = active;

            match self.active_panel {
                ActivePanel::PodcastMenu => {
                    if let Some(pod_id) = curr_pod_id {
                        let (description, author, last_checked, title) = {
                            let podcast_map = self.podcast_menu.items.borrow_map();

                            let podcast = podcast_map.get(&pod_id)?;
                            (
                                if podcast.description.is_none() {
                                    None
                                } else {
                                    Some(clean_html(podcast.description.as_ref().unwrap()))
                                },
                                podcast.author.clone(),
                                podcast.last_checked,
                                podcast.title.clone(),
                            )
                        };

                        let details = Details {
                            pubdate: None,
                            duration: None,
                            explicit: None,
                            description,
                            author,
                            last_checked: Some(last_checked),
                            title: Some(title),
                        };
                        det.change_details(details);
                    } else {
                        det.clear_details();
                    }
                }
                ActivePanel::EpisodeMenu => {
                    if let Some(ep_id) = curr_ep_id {
                        // the rest of the details come from the current episode
                        if let Some(ep) = self.episode_menu.items.get(ep_id) {
                            let desc = clean_html(&ep.description);

                            let details = Details {
                                pubdate: ep.pubdate,
                                duration: Some(ep.format_duration()),
                                explicit: None,
                                description: Some(desc),
                                author: None,
                                last_checked: None,
                                title: Some(ep.title.clone()),
                            };
                            det.change_details(details);
                        };
                    } else {
                        det.clear_details();
                    }
                }
                ActivePanel::QueueMenu => {
                    if let Some(ep_id) = curr_ep_id {
                        // the rest of the details come from the current episode
                        if let Some(ep) = self.queue_menu.items.get(ep_id) {
                            let desc = clean_html(&ep.description);

                            let details = Details {
                                pubdate: ep.pubdate,
                                duration: Some(ep.format_duration()),
                                explicit: None,
                                description: Some(desc),
                                author: None,
                                last_checked: None,
                                title: Some(ep.title.clone()),
                            };
                            det.change_details(details);
                        };
                    } else {
                        det.clear_details();
                    }
                }
                ActivePanel::DetailsPanel => {
                    det.panel.active = true;
                    det.redraw();
                }
            }
        }
        Some(())
    }
}
