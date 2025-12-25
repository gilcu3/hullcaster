use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use symphonia::core::codecs::CODEC_TYPE_NULL;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::probe::Hint;
use symphonia::default::get_probe;
use unicode_segmentation::UnicodeSegmentation;

use crate::types::*;

pub static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

static RE_BR_TAGS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"((\r\n)|\r|\n)*<br */?>((\r\n)|\r|\n)*").expect("Regex error"));

static RE_HTML_TAGS: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^<>]*>").expect("Regex error"));

static RE_MULT_LINE_BREAKS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"((\r\n)|\r|\n){3,}").expect("Regex error"));

/// Helper function converting an (optional) Unix timestamp to a
/// DateTime<Utc> object
pub fn convert_date(timestamp: i64) -> Result<DateTime<Utc>> {
    let ndt = DateTime::from_timestamp(timestamp, 0)
        .ok_or_else(|| anyhow!("Wrong timestamp: {timestamp}"))?;
    Ok(DateTime::from_naive_utc_and_offset(ndt.naive_utc(), Utc))
}

pub fn evaluate_in_shell(value: &str) -> Result<String> {
    let res = Command::new("sh").arg("-c").arg(value).output()?;
    Ok(String::from_utf8_lossy(&res.stdout).to_string())
}

pub fn audio_duration(audio_bytes: Vec<u8>) -> Result<i64> {
    let cursor = Cursor::new(audio_bytes);
    let mss = MediaSourceStream::new(Box::new(cursor), MediaSourceStreamOptions::default());
    let probed = get_probe().format(
        &Hint::new(),
        mss,
        &FormatOptions::default(),
        &Default::default(),
    )?;
    let mut duration = 0;
    for track in probed.format.tracks() {
        if track.codec_params.codec != CODEC_TYPE_NULL {
            let tt = track
                .codec_params
                .time_base
                .ok_or_else(|| anyhow!("time_base is None"))?
                .calc_time(
                    track
                        .codec_params
                        .n_frames
                        .ok_or_else(|| anyhow!("n_frames is None"))?,
                );
            duration += tt.seconds;
        }
    }
    Ok(duration as i64)
}

pub fn audio_duration_file(file_path: PathBuf) -> Result<i64> {
    let bytes = fs::read(file_path)?;
    audio_duration(bytes)
}

/// Some helper functions for dealing with Unicode strings.
pub trait StringUtils {
    fn substr(&self, start: usize, length: usize) -> String;
    fn grapheme_len(&self) -> usize;
}

impl StringUtils for String {
    /// Takes a slice of the String, properly separated at Unicode
    /// grapheme boundaries. Returns a new String.
    fn substr(&self, start: usize, length: usize) -> String {
        self.graphemes(true)
            .skip(start)
            .take(length)
            .collect::<Self>()
    }

    /// Counts the total number of Unicode graphemes in the String.
    fn grapheme_len(&self) -> usize {
        self.graphemes(true).count()
    }
}

pub fn current_time_ms() -> u128 {
    let start = SystemTime::now();
    let since_the_epoch = start
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards");
    since_the_epoch.as_millis()
}

pub fn clean_html(text: &str) -> String {
    // convert <br/> tags to a single line break
    let br_to_lb = RE_BR_TAGS.replace_all(text, "\n");

    // strip all HTML tags
    let stripped_tags = RE_HTML_TAGS.replace_all(&br_to_lb, "");

    // convert HTML entities (e.g., &amp;)
    let decoded =
        escaper::decode_html(&stripped_tags).unwrap_or_else(|_| stripped_tags.to_string());

    // remove anything more than two line breaks (i.e., one blank line)
    RE_MULT_LINE_BREAKS
        .replace_all(&decoded, "\n\n")
        .to_string()
}

pub async fn resolve_redirection_async(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(APP_USER_AGENT)
        .build()?;
    let response = client.get(url).send().await?;
    let final_url = response.url().to_string();
    Ok(final_url)
}

pub fn resolve_redirection(url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(APP_USER_AGENT)
        .build()?;
    let response = client.get(url).send()?;
    let final_url = response.url().to_string();
    Ok(final_url)
}

pub fn get_unplayed_episodes(podcasts: &LockVec<Podcast>) -> Vec<Arc<RwLock<Episode>>> {
    let podcast_map = podcasts.borrow_map();
    let mut ueps = Vec::new();
    for podcast in podcast_map.values() {
        let rpod = podcast.read().unwrap();
        let episode_map = rpod.episodes.borrow_map();
        for episode in episode_map.values() {
            let rep = episode.read().unwrap();
            if !rep.played {
                ueps.push(episode.clone());
            }
        }
    }
    ueps
}

/// Helper function that takes an (optionally specified) user directory
/// and an (OS-dependent) default directory, expands any environment
/// variables, ~ alias, etc. Returns a PathBuf. Panics if environment
/// variables cannot be found, if OS could not produce the appropriate
/// default directory, or if the specified directories in the path could
/// not be created.
pub fn parse_create_dir(user_dir: Option<&str>, default: Option<PathBuf>) -> Result<PathBuf> {
    let final_path = match user_dir {
        Some(path) => match shellexpand::full(path) {
            Ok(realpath) => PathBuf::from(realpath.as_ref()),
            Err(err) => {
                return Err(anyhow!(
                    "Could not parse environment variable {} in config.toml. Reason: {}",
                    err.var_name,
                    err.cause
                ));
            }
        },
        None => {
            if let Some(path) = default {
                path
            } else {
                return Err(anyhow!(
                    "Could not identify a default directory for your OS. Please specify paths manually in config.toml."
                ));
            }
        }
    };

    // create directories if they do not exist
    std::fs::create_dir_all(&final_path).with_context(|| {
        format!(
            "Could not create filepath: {}",
            final_path.to_string_lossy()
        )
    })?;

    Ok(final_path)
}

pub fn format_duration(duration: Option<u64>) -> String {
    duration.map_or_else(
        || "--:--:--".to_string(),
        |dur| {
            let mut seconds = dur;
            let hours = seconds / 3600;
            seconds -= hours * 3600;
            let minutes = seconds / 60;
            seconds -= minutes * 60;
            format!("{hours:02}:{minutes:02}:{seconds:02}")
        },
    )
}
