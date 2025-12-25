use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

// How many lines will be scrolled by the PageUp/PageDown
pub const SCROLL_AMOUNT: u16 = 6;

/// Amount of time between ticks in the event loop
pub const TICK_RATE: u64 = 50;

/// Amount of time between ticks in the event loop
pub const SEEK_LENGTH: Duration = Duration::from_secs(30);

/// Maximum duration of episode when unknown
pub const MAX_DURATION: i64 = 10000;

/// Number of milliseconds on mute to avoid audio artifacts
pub const FADING_TIME: u64 = 100;

/// Holds information about user configuration of program.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    pub download_path: PathBuf,
    pub play_command: String,
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
    pub confirm_quit: bool,
}

/// A temporary struct used to deserialize data from the TOML configuration
/// file. Will be converted into Config struct.
#[derive(Debug, Deserialize)]
struct ConfigFromToml {
    download_path: Option<String>,
    play_command: Option<String>,
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
    confirm_quit: Option<bool>,
}

/// A temporary struct used to deserialize keybinding data from the TOML
/// configuration file.
#[derive(Debug, Deserialize)]
pub struct KeybindingsFromToml {
    pub left: Option<Vec<String>>,
    pub right: Option<Vec<String>>,
    pub up: Option<Vec<String>>,
    pub down: Option<Vec<String>>,
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
    pub play_pause: Option<Vec<String>>,
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
    pub back: Option<Vec<String>>,
    pub switch: Option<Vec<String>>,
    pub play_external: Option<Vec<String>>,
}

/// A temporary struct used to deserialize colors data from the TOML
/// configuration file. See `crate::ui::colors` module for the `AppColors`
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
    pub fn new(path: &Path) -> Result<Self> {
        let mut config_string = String::new();

        let config_toml = if let Ok(mut file) = File::open(path) {
            file.read_to_string(&mut config_string)
                .with_context(|| "Could not read config.toml. Please ensure file is readable.")?;
            toml::from_str(&config_string)
                .with_context(|| "Could not parse config.toml. Please check file syntax.")?
        } else {
            // if we can't find the file, set everything to empty
            // so we it will use the defaults for everything
            let keybindings = KeybindingsFromToml {
                left: None,
                right: None,
                up: None,
                down: None,
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
                play_pause: None,
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
                back: None,
                switch: None,
                play_external: None,
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
                confirm_quit: Some(true),
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
    let keymap = config_toml
        .keybindings
        .map_or_else(Keybindings::default, Keybindings::from_config);

    // specify app colors
    let colors = config_toml.colors.map_or_else(AppColors::default, |clrs| {
        let mut colors = AppColors::default();
        colors.add_from_config(clrs);
        colors
    });

    // paths are set by user, or they resolve to OS-specific path as
    // provided by dirs crate
    let default_path = dirs::data_local_dir().map(|mut p| {
        p.push("hullcaster");
        p
    });
    let download_path = parse_create_dir(config_toml.download_path.as_deref(), default_path)?;

    let play_command = config_toml
        .play_command
        .as_deref()
        .map_or_else(|| "vlc %s".to_string(), std::string::ToString::to_string);

    let simultaneous_downloads = match config_toml.simultaneous_downloads {
        Some(num) if num > 0 => num,
        Some(_) | None => 3,
    };

    let max_retries = match config_toml.max_retries {
        Some(num) if num > 0 => num,
        Some(_) | None => 3,
    };

    let mark_as_played_on_play = config_toml.mark_as_played_on_play.unwrap_or(true);

    let enable_sync = config_toml.enable_sync.unwrap_or(false);

    let sync_server = config_toml.sync_server.unwrap_or_default();
    let sync_username = config_toml.sync_username.unwrap_or_default();

    let sync_password = if config_toml.sync_password.is_some() {
        config_toml.sync_password.unwrap_or_default()
    } else if config_toml.sync_password_eval.is_some() {
        let sync_password_eval = config_toml.sync_password_eval.unwrap_or_default();
        let password = evaluate_in_shell(&sync_password_eval)?;
        password.trim().to_string()
    } else {
        log::warn!("sync_password is not set, assuming empty");
        String::new()
    };

    let sync_on_start = config_toml.sync_on_start.unwrap_or(true);

    let confirm_quit = config_toml.confirm_quit.unwrap_or(true);

    Ok(Config {
        download_path,
        play_command,
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
        confirm_quit,
    })
}
