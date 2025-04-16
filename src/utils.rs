use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use std::io::{Cursor, Read};
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
use ureq::{Agent, Error, ResponseExt};

use crate::types::*;

static RE_BR_TAGS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"((\r\n)|\r|\n)*<br */?>((\r\n)|\r|\n)*").expect("Regex error"));

static RE_HTML_TAGS: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^<>]*>").expect("Regex error"));

static RE_MULT_LINE_BREAKS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"((\r\n)|\r|\n){3,}").expect("Regex error"));

/// Helper function converting an (optional) Unix timestamp to a
/// DateTime<Utc> object
pub fn convert_date(result: Result<i64, rusqlite::Error>) -> Option<DateTime<Utc>> {
    match result {
        Ok(timestamp) => DateTime::from_timestamp(timestamp, 0)
            .map(|ndt| DateTime::from_naive_utc_and_offset(ndt.naive_utc(), Utc)),
        Err(_) => None,
    }
}

pub fn evaluate_in_shell(value: &str) -> Option<String> {
    let res = Command::new("sh").arg("-c").arg(value).output();
    if let Ok(res) = res {
        Some(String::from_utf8_lossy(&res.stdout).to_string())
    } else {
        None
    }
}

pub fn execute_request_post(
    agent: &Agent, url: String, body: String, encoded_credentials: &String,
) -> Option<String> {
    let mut max_retries = 3;

    let request = loop {
        let response = agent
            .post(&url)
            .header("Authorization", &format!("Basic {}", encoded_credentials))
            .send(&body);

        match response {
            Ok(resp) => {
                //println!("Ok code: {:?}", resp);
                break Ok(resp);
            }
            Err(Error::StatusCode(code)) => {
                // Handle HTTP error statuses (e.g., 404, 500)
                println!("Error code: {}", code);
                max_retries -= 1;
                if max_retries == 0 {
                    break Err(());
                }
            }
            Err(_) => {
                max_retries -= 1;
                if max_retries == 0 {
                    break Err(());
                }
            }
        }
    };
    if let Ok(req) = request {
        req.into_body().read_to_string().ok()
    } else {
        None
    }
}

pub fn execute_request_get(
    agent: &Agent, url: String, params: Vec<(&str, &str)>, encoded_credentials: &String,
) -> Option<String> {
    let mut max_retries = 3;

    let request = loop {
        let response = agent
            .get(&url)
            .header("Authorization", &format!("Basic {}", encoded_credentials))
            .query_pairs(params.clone())
            .call();

        match response {
            Ok(resp) => {
                // println!("Ok code: {:?}", resp);
                break Ok(resp);
            }
            Err(Error::StatusCode(code)) => {
                println!("Error code: {}", code);
                max_retries -= 1;
                if max_retries == 0 {
                    break Err(());
                }
            }
            Err(_) => {
                max_retries -= 1;
                if max_retries == 0 {
                    break Err(());
                }
            }
        }
    };
    if let Ok(req) = request {
        req.into_body().read_to_string().ok()
    } else {
        None
    }
}

pub fn audio_duration(url: &str) -> Option<i64> {
    log::info!("Getting audio duration for {}", url);
    let mut response = ureq::get(url).call().ok()?;
    let bytes = response
        .body_mut()
        .as_reader()
        .bytes()
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    log::info!("Bytes: {:?}", bytes.len());
    let cursor = Cursor::new(bytes);
    let mss = MediaSourceStream::new(Box::new(cursor), MediaSourceStreamOptions::default());
    let probed = get_probe()
        .format(
            &Hint::new(),
            mss,
            &FormatOptions::default(),
            &Default::default(),
        )
        .ok()?;
    let mut duration = 0;
    for track in probed.format.tracks() {
        if track.codec_params.codec != CODEC_TYPE_NULL {
            let tt = track
                .codec_params
                .time_base?
                .calc_time(track.codec_params.n_frames?);
            duration += tt.seconds;
        }
    }
    Some(duration as i64)
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
            .collect::<String>()
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
    let decoded = match escaper::decode_html(&stripped_tags) {
        Err(_) => stripped_tags.to_string(),
        Ok(s) => s,
    };

    // remove anything more than two line breaks (i.e., one blank line)
    let no_line_breaks = RE_MULT_LINE_BREAKS.replace_all(&decoded, "\n\n");

    no_line_breaks.to_string()
}

// Probably should be done better, without downloading the page
pub fn resolve_redirection(url: &str) -> Option<String> {
    let agent = ureq::agent();

    let response = agent.get(url).call().ok()?;

    let final_url = response.get_uri().to_string();
    Some(final_url)
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
                ))
            }
        },
        None => {
            if let Some(path) = default {
                path
            } else {
                return Err(anyhow!("Could not identify a default directory for your OS. Please specify paths manually in config.toml."));
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
    match duration {
        Some(dur) => {
            let mut seconds = dur;
            let hours = seconds / 3600;
            seconds -= hours * 3600;
            let minutes = seconds / 60;
            seconds -= minutes * 60;
            format!("{hours:02}:{minutes:02}:{seconds:02}")
        }
        None => "--:--:--".to_string(),
    }
}
