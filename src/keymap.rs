use ahash::AHashMap;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::KeybindingsFromToml;

/// Enum delineating all actions that may be performed by the user, and
/// thus have keybindings associated with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserAction {
    Left,
    Right,
    Up,
    Down,
    MoveUp,
    MoveDown,
    Enter,

    PageUp,
    PageDown,
    GoTop,
    GoBot,

    AddFeed,
    Sync,
    SyncAll,
    SyncGpodder,

    PlayPause,
    MarkPlayed,
    MarkAllPlayed,

    Download,
    DownloadAll,
    Delete,
    DeleteAll,
    Remove,

    FilterPlayed,
    FilterDownloaded,

    Enqueue,

    Help,
    Quit,

    UnplayedList,
    Information,
    Back,
    Switch,
    PlayExternal,
}

/// Wrapper around a hash map that keeps track of all keybindings. Multiple
/// keys may perform the same action, but each key may only perform one
/// action.
#[derive(Debug, Clone)]
pub struct Keybindings(
    AHashMap<String, UserAction>,
    AHashMap<UserAction, Vec<String>>,
);

impl Keybindings {
    /// Returns a new Keybindings struct.
    pub fn new() -> Self {
        Self(AHashMap::new(), AHashMap::new())
    }

    /// Returns a Keybindings struct with all default values set.
    pub fn default() -> Self {
        let defaults = Self::_defaults();
        let mut keymap = Self::new();
        for (action, defaults) in defaults {
            keymap.insert_from_vec(defaults.clone(), action);
            keymap.1.insert(action, defaults);
        }
        keymap
    }

    /// Given a struct deserialized from config.toml (for which any or
    /// all fields may be missing), create a Keybindings struct using
    /// user-defined keys where specified, and default values otherwise.
    pub fn from_config(config: KeybindingsFromToml) -> Self {
        let config_actions: Vec<(Option<Vec<String>>, UserAction)> = vec![
            (config.left, UserAction::Left),
            (config.right, UserAction::Right),
            (config.up, UserAction::Up),
            (config.down, UserAction::Down),
            (config.page_up, UserAction::PageUp),
            (config.page_down, UserAction::PageDown),
            (config.go_top, UserAction::GoTop),
            (config.go_bot, UserAction::GoBot),
            (config.move_up, UserAction::MoveUp),
            (config.move_down, UserAction::MoveDown),
            (config.add_feed, UserAction::AddFeed),
            (config.sync, UserAction::Sync),
            (config.sync_all, UserAction::SyncAll),
            (config.sync_gpodder, UserAction::SyncGpodder),
            (config.play_pause, UserAction::PlayPause),
            (config.enter, UserAction::Enter),
            (config.mark_played, UserAction::MarkPlayed),
            (config.mark_all_played, UserAction::MarkAllPlayed),
            (config.download, UserAction::Download),
            (config.download_all, UserAction::DownloadAll),
            (config.delete, UserAction::Delete),
            (config.delete_all, UserAction::DeleteAll),
            (config.remove, UserAction::Remove),
            (config.filter_played, UserAction::FilterPlayed),
            (config.filter_downloaded, UserAction::FilterDownloaded),
            (config.enqueue, UserAction::Enqueue),
            (config.help, UserAction::Help),
            (config.quit, UserAction::Quit),
            (config.unplayed_list, UserAction::UnplayedList),
            (config.back, UserAction::Back),
            (config.switch, UserAction::Switch),
            (config.play_external, UserAction::PlayExternal),
        ];

        let mut keymap = Self::default();
        for (config, action) in config_actions {
            if let Some(config) = config {
                keymap.insert_from_vec(config.clone(), action);
                keymap.1.insert(action, config);
            }
        }
        keymap
    }

    /// Takes an Input object from crossterm and returns the associated
    /// user action, if one exists.
    pub fn get_from_input(&self, input: KeyEvent) -> Option<&UserAction> {
        self.0.get(&input_to_str(input)?)
    }

    /// Inserts a set of new key-bindings into the hash map, each one
    /// corresponding to the same `UserAction`. Will overwrite the value
    /// of keys that already exist.
    pub fn insert_from_vec(&mut self, vec: Vec<String>, action: UserAction) {
        for key in vec {
            self.0.insert(key, action);
        }
    }

    /// Returns a Vec with all of the keys mapped to a particular user
    /// action.
    pub fn keys_for_action(&self, action: UserAction) -> Option<&Vec<String>> {
        self.1.get(&action)
    }

    fn _defaults() -> Vec<(UserAction, Vec<String>)> {
        vec![
            (UserAction::Left, vec!["Left".to_string(), "h".to_string()]),
            (
                UserAction::Right,
                vec!["Right".to_string(), "l".to_string()],
            ),
            (UserAction::Up, vec!["Up".to_string(), "k".to_string()]),
            (UserAction::Down, vec!["Down".to_string(), "j".to_string()]),
            (UserAction::PageUp, vec!["PgUp".to_string()]),
            (UserAction::PageDown, vec!["PgDn".to_string()]),
            (UserAction::GoTop, vec!["g".to_string()]),
            (UserAction::GoBot, vec!["G".to_string()]),
            (UserAction::MoveUp, vec!["Ctrl+Up".to_string()]),
            (UserAction::MoveDown, vec!["Ctrl+Down".to_string()]),
            (UserAction::AddFeed, vec!["a".to_string()]),
            (UserAction::Sync, vec!["s".to_string()]),
            (UserAction::SyncAll, vec!["S".to_string()]),
            (UserAction::SyncGpodder, vec!["A".to_string()]),
            (UserAction::PlayPause, vec!["Space".to_string()]),
            (UserAction::Enter, vec!["Enter".to_string()]),
            (UserAction::MarkPlayed, vec!["m".to_string()]),
            (UserAction::MarkAllPlayed, vec!["M".to_string()]),
            (UserAction::Download, vec!["d".to_string()]),
            (UserAction::DownloadAll, vec!["D".to_string()]),
            (UserAction::Delete, vec!["x".to_string()]),
            (UserAction::DeleteAll, vec!["X".to_string()]),
            (UserAction::Remove, vec!["r".to_string()]),
            (UserAction::FilterPlayed, vec!["1".to_string()]),
            (UserAction::FilterDownloaded, vec!["2".to_string()]),
            (UserAction::Enqueue, vec!["e".to_string()]),
            (UserAction::Help, vec!["?".to_string()]),
            (UserAction::Quit, vec!["q".to_string()]),
            (UserAction::UnplayedList, vec!["u".to_string()]),
            (UserAction::Information, vec!["i".to_string()]),
            (UserAction::Back, vec!["Esc".to_string()]),
            (UserAction::Switch, vec!["Tab".to_string()]),
            (UserAction::PlayExternal, vec!["P".to_string()]),
        ]
    }
}

/// Helper function converting a crossterm `KeyEvent` object to a unique
/// string representing that input.
pub fn input_to_str(input: KeyEvent) -> Option<String> {
    let ctrl = if input.modifiers.intersects(KeyModifiers::CONTROL) {
        "Ctrl+"
    } else {
        ""
    };
    let alt = if input.modifiers.intersects(KeyModifiers::ALT) {
        "Alt+"
    } else {
        ""
    };
    let shift = if input.modifiers.intersects(KeyModifiers::SHIFT) {
        "Shift+"
    } else {
        ""
    };
    let mut tmp = [0; 4];
    match input.code {
        KeyCode::Backspace => Some(format!("{ctrl}{alt}{shift}Backspace")),
        KeyCode::Enter => Some(format!("{ctrl}{alt}{shift}Enter")),
        KeyCode::Left => Some(format!("{ctrl}{alt}{shift}Left")),
        KeyCode::Right => Some(format!("{ctrl}{alt}{shift}Right")),
        KeyCode::Up => Some(format!("{ctrl}{alt}{shift}Up")),
        KeyCode::Down => Some(format!("{ctrl}{alt}{shift}Down")),
        KeyCode::Home => Some(format!("{ctrl}{alt}{shift}Home")),
        KeyCode::End => Some(format!("{ctrl}{alt}{shift}End")),
        KeyCode::PageUp => Some(format!("{ctrl}{alt}{shift}PgUp")),
        KeyCode::PageDown => Some(format!("{ctrl}{alt}{shift}PgDn")),
        KeyCode::Tab | KeyCode::BackTab => Some(format!("{ctrl}{alt}{shift}Tab")),
        KeyCode::Delete => Some(format!("{ctrl}{alt}{shift}Del")),
        KeyCode::Insert => Some(format!("{ctrl}{alt}{shift}Ins")),
        KeyCode::Esc => Some(format!("{ctrl}{alt}{shift}Esc")),
        KeyCode::F(num) => Some(format!("{ctrl}{alt}{shift}F{num}")), // Function keys
        KeyCode::Char(c) => {
            if c == '\u{7f}' {
                Some(format!("{ctrl}{alt}{shift}Backspace"))
            } else if c == '\u{1b}' {
                Some(format!("{ctrl}{alt}{shift}Esc"))
            } else if c == '\n' {
                Some(format!("{ctrl}{alt}{shift}Enter"))
            } else if c == '\t' {
                Some(format!("{ctrl}{alt}{shift}Tab"))
            } else if c == ' ' {
                Some(format!("{ctrl}{alt}{shift}Space"))
            } else {
                // here we don't include "shift" because that will
                // already be encoded in the character itself
                Some(format!("{}{}{}", ctrl, alt, c.encode_utf8(&mut tmp)))
            }
        }
        _ => None,
    }
}
