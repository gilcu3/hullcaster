use chrono::{DateTime, Utc};
use std::io::{Cursor, Read};
use std::process::Command;
use symphonia::core::codecs::CODEC_TYPE_NULL;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::probe::Hint;
use symphonia::default::get_probe;
use unicode_segmentation::UnicodeSegmentation;
use ureq::{Agent, Error, Response};

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

    let request: Result<Response, ()> = loop {
        let response = agent
            .post(&url)
            .set("Authorization", &format!("Basic {}", encoded_credentials))
            .send_string(&body);

        match response {
            Ok(resp) => {
                //println!("Ok code: {:?}", resp);
                break Ok(resp);
            }
            Err(Error::Status(code, _error_response)) => {
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
        req.into_string().ok()
    } else {
        None
    }
}

pub fn execute_request_get(
    agent: &Agent, url: String, params: Vec<(&str, &str)>, encoded_credentials: &String,
) -> Option<String> {
    let mut max_retries = 3;

    let request: Result<Response, ()> = loop {
        let response = agent
            .get(&url)
            .set("Authorization", &format!("Basic {}", encoded_credentials))
            .query_pairs(params.clone())
            .call();

        match response {
            Ok(resp) => {
                // println!("Ok code: {:?}", resp);
                break Ok(resp);
            }
            Err(Error::Status(code, _error_response)) => {
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
        req.into_string().ok()
    } else {
        None
    }
}

pub fn audio_duration(url: &str) -> Option<i64> {
    log::info!("Getting audio duration for {}", url);
    let response = ureq::get(url).call().ok()?;
    let bytes = response
        .into_reader()
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
        return self
            .graphemes(true)
            .skip(start)
            .take(length)
            .collect::<String>();
    }

    /// Counts the total number of Unicode graphemes in the String.
    fn grapheme_len(&self) -> usize {
        return self.graphemes(true).count();
    }
}
