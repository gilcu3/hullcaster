use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::keymap::Keybindings;
use crate::ui::colors::AppColors;
use crate::utils::{evaluate_in_shell, parse_create_dir};

// Specifies how long, in milliseconds, to display messages at the
// bottom of the screen in the UI.
pub const MESSAGE_TIME: u64 = 5000;

// How many columns we need, minimum, before we display the
// (unplayed/total) after the podcast title
pub const PODCAST_UNPLAYED_TOTALS_LENGTH: usize = 25;

// How many columns we need, minimum, before we display the duration of
// the episode
pub const EPISODE_DURATION_LENGTH: usize = 45;

// How many columns we need, minimum, before we display the pubdate
// of the episode
pub const EPISODE_PUBDATE_LENGTH: usize = 60;

// How many columns we need (total terminal window width) before we
// display the details panel
pub const DETAILS_PANEL_LENGTH: u16 = 135;

// How many lines will be scrolled by the big scroll,
// in relation to the rows eg: 4 = 1/4 of the screen
pub const BIG_SCROLL_AMOUNT: u16 = 4;

/// Identifies the user's selection for what to do with new episodes
/// when syncing.
#[derive(Debug, Clone)]
pub enum DownloadNewEpisodes {
    Always,
    AskSelected,
    AskUnselected,
    Never,
}

/// Holds information about user configuration of program.
#[derive(Debug, Clone)]
pub struct Config {
    pub download_path: PathBuf,
    pub play_command: String,
    pub download_new_episodes: DownloadNewEpisodes,
    pub simultaneous_downloads: usize,
    pub max_retries: usize,
    pub mark_as_played_on_play: bool,
    pub enable_sync: bool,
    pub sync_server: String,
    pub sync_username: String,
    pub sync_password: String,
    pub sync_on_start: bool,
    pub keybindings: Keybindings,
    pub colors: AppColors,
}

/// A temporary struct used to deserialize data from the TOML configuration
/// file. Will be converted into Config struct.
#[derive(Debug, Deserialize)]
struct ConfigFromToml {
    download_path: Option<String>,
    play_command: Option<String>,
    download_new_episodes: Option<String>,
    simultaneous_downloads: Option<usize>,
    max_retries: Option<usize>,
    mark_as_played_on_play: Option<bool>,
    enable_sync: Option<bool>,
    sync_server: Option<String>,
    sync_username: Option<String>,
    sync_password: Option<String>,
    sync_password_eval: Option<String>,
    sync_on_start: Option<bool>,
    keybindings: Option<KeybindingsFromToml>,
    colors: Option<AppColorsFromToml>,
}

/// A temporary struct used to deserialize keybinding data from the TOML
/// configuration file.
#[derive(Debug, Deserialize)]
pub struct KeybindingsFromToml {
    pub left: Option<Vec<String>>,
    pub right: Option<Vec<String>>,
    pub up: Option<Vec<String>>,
    pub down: Option<Vec<String>>,
    pub big_up: Option<Vec<String>>,
    pub big_down: Option<Vec<String>>,
    pub go_top: Option<Vec<String>>,
    pub go_bot: Option<Vec<String>>,
    pub page_up: Option<Vec<String>>,
    pub page_down: Option<Vec<String>>,
    pub move_up: Option<Vec<String>>,
    pub move_down: Option<Vec<String>>,
    pub add_feed: Option<Vec<String>>,
    pub sync: Option<Vec<String>>,
    pub sync_all: Option<Vec<String>>,
    pub sync_gpodder: Option<Vec<String>>,
    pub play: Option<Vec<String>>,
    pub enter: Option<Vec<String>>,
    pub mark_played: Option<Vec<String>>,
    pub mark_all_played: Option<Vec<String>>,
    pub download: Option<Vec<String>>,
    pub download_all: Option<Vec<String>>,
    pub delete: Option<Vec<String>>,
    pub delete_all: Option<Vec<String>>,
    pub remove: Option<Vec<String>>,
    pub filter_played: Option<Vec<String>>,
    pub filter_downloaded: Option<Vec<String>>,
    pub enqueue: Option<Vec<String>>,
    pub help: Option<Vec<String>>,
    pub quit: Option<Vec<String>>,
    pub unplayed_list: Option<Vec<String>>,
}

/// A temporary struct used to deserialize colors data from the TOML
/// configuration file. See crate::ui::colors module for the AppColors
/// struct which handles the final color scheme.
#[derive(Debug, Deserialize)]
pub struct AppColorsFromToml {
    pub normal_foreground: Option<String>,
    pub normal_background: Option<String>,
    pub bold_foreground: Option<String>,
    pub bold_background: Option<String>,
    pub highlighted_active_foreground: Option<String>,
    pub highlighted_active_background: Option<String>,
    pub highlighted_foreground: Option<String>,
    pub highlighted_background: Option<String>,
    pub error_foreground: Option<String>,
    pub error_background: Option<String>,
}

impl Config {
    /// Given a file path, this reads a TOML config file and returns a
    /// Config struct with keybindings, etc. Inserts defaults if config
    /// file does not exist, or if specific values are not set.
    pub fn new(path: &Path) -> Result<Config> {
        let mut config_string = String::new();

        let config_toml = match File::open(path) {
            Ok(mut file) => {
                file.read_to_string(&mut config_string).with_context(|| {
                    "Could not read config.toml. Please ensure file is readable."
                })?;
                toml::from_str(&config_string)
                    .with_context(|| "Could not parse config.toml. Please check file syntax.")?
            }
            Err(_) => {
                // if we can't find the file, set everything to empty
                // so we it will use the defaults for everything
                let keybindings = KeybindingsFromToml {
                    left: None,
                    right: None,
                    up: None,
                    down: None,
                    big_up: None,
                    big_down: None,
                    go_top: None,
                    go_bot: None,
                    page_up: None,
                    page_down: None,
                    move_up: None,
                    move_down: None,
                    add_feed: None,
                    sync: None,
                    sync_all: None,
                    sync_gpodder: None,
                    play: None,
                    enter: None,
                    mark_played: None,
                    mark_all_played: None,
                    download: None,
                    download_all: None,
                    delete: None,
                    delete_all: None,
                    remove: None,
                    filter_played: None,
                    filter_downloaded: None,
                    enqueue: None,
                    help: None,
                    quit: None,
                    unplayed_list: None,
                };

                let colors = AppColorsFromToml {
                    normal_foreground: None,
                    normal_background: None,
                    bold_foreground: None,
                    bold_background: None,
                    highlighted_active_foreground: None,
                    highlighted_active_background: None,
                    highlighted_foreground: None,
                    highlighted_background: None,
                    error_foreground: None,
                    error_background: None,
                };
                ConfigFromToml {
                    download_path: None,
                    play_command: None,
                    download_new_episodes: None,
                    simultaneous_downloads: None,
                    max_retries: None,
                    enable_sync: Some(false),
                    sync_server: None,
                    sync_username: None,
                    sync_password: None,
                    sync_password_eval: None,
                    mark_as_played_on_play: None,
                    sync_on_start: Some(true),
                    keybindings: Some(keybindings),
                    colors: Some(colors),
                }
            }
        };

        config_with_defaults(config_toml)
    }
}

/// Takes the deserialized TOML configuration, and creates a Config struct
/// that specifies user settings where indicated, and defaults for any
/// settings that were not specified by the user.
fn config_with_defaults(config_toml: ConfigFromToml) -> Result<Config> {
    // specify keybindings
    let keymap = match config_toml.keybindings {
        Some(kb) => Keybindings::from_config(kb),
        None => Keybindings::default(),
    };

    // specify app colors
    let colors = match config_toml.colors {
        Some(clrs) => {
            let mut colors = AppColors::default();
            colors.add_from_config(clrs);
            colors
        }
        None => AppColors::default(),
    };

    // paths are set by user, or they resolve to OS-specific path as
    // provided by dirs crate
    let download_path =
        parse_create_dir(config_toml.download_path.as_deref(), dirs::data_local_dir())?;

    let play_command = match config_toml.play_command.as_deref() {
        Some(cmd) => cmd.to_string(),
        None => "vlc %s".to_string(),
    };

    let download_new_episodes = match config_toml.download_new_episodes.as_deref() {
        Some("always") => DownloadNewEpisodes::Always,
        Some("ask-selected") => DownloadNewEpisodes::AskSelected,
        Some("ask-unselected") => DownloadNewEpisodes::AskUnselected,
        Some("never") => DownloadNewEpisodes::Never,
        Some(_) | None => DownloadNewEpisodes::AskUnselected,
    };

    let simultaneous_downloads = match config_toml.simultaneous_downloads {
        Some(num) if num > 0 => num,
        Some(_) => 3,
        None => 3,
    };

    let max_retries = match config_toml.max_retries {
        Some(num) if num > 0 => num,
        Some(_) => 3,
        None => 3,
    };

    let mark_as_played_on_play = config_toml.mark_as_played_on_play.unwrap_or(true);

    let enable_sync = config_toml.enable_sync.unwrap_or(false);

    let sync_server = config_toml.sync_server.unwrap_or_default();
    let sync_username = config_toml.sync_username.unwrap_or_default();

    let sync_password = if config_toml.sync_password.is_some() {
        config_toml.sync_password.unwrap_or_default()
    } else if config_toml.sync_password_eval.is_some() {
        let sync_password_eval = config_toml.sync_password_eval.unwrap_or_default();
        let tmp_sync_password = evaluate_in_shell(&sync_password_eval);
        if let Some(password) = tmp_sync_password {
            password.trim().to_string()
        } else {
            "".to_string()
        }
    } else {
        "".to_string()
    };

    let sync_on_start = config_toml.sync_on_start.unwrap_or(true);

    Ok(Config {
        download_path,
        play_command,
        download_new_episodes,
        simultaneous_downloads,
        max_retries,
        mark_as_played_on_play,
        enable_sync,
        sync_server,
        sync_username,
        sync_password,
        sync_on_start,
        keybindings: keymap,
        colors,
    })
}
