use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sanitize_filename::{Options, sanitize_with_options};

use crate::threadpool::Threadpool;
use crate::types::Message;
use crate::utils::audio_duration_file;

/// Enum used for communicating back to the main controller upon
/// successful or unsuccessful downloading of a file. i32 value
/// represents the episode ID, and `PathBuf` the location of the new file.
// TODO: this type needs to be more idiomatic
#[derive(Debug)]
pub enum DownloadMsg {
    Complete(EpData),
    ResponseError(EpData),
    FileCreateError(EpData),
    FileWriteError(EpData),
}

/// Enum used to communicate relevant data to the threadpool.
#[derive(Debug, Clone)]
pub struct EpData {
    pub id: i64,
    pub pod_id: i64,
    pub title: String,
    pub url: String,
    pub pubdate: Option<DateTime<Utc>>,
    pub file_path: Option<PathBuf>,
    pub duration: Option<i64>,
}

/// This is the function the main controller uses to indicate new
/// files to download. It uses the threadpool to start jobs
/// for every episode to be downloaded. New jobs can be requested
/// by the user while there are still ongoing jobs.
pub fn download_list(
    episodes: Vec<EpData>, dest: &Path, max_retries: usize, threadpool: &Threadpool,
    tx_to_main: &Sender<Message>,
) {
    // parse episode details and push to queue
    for ep in episodes {
        let tx = tx_to_main.clone();
        let dest2 = dest.to_path_buf();
        threadpool.execute(move || {
            let result = download_file(ep, dest2, max_retries);
            tx.send(Message::Dl(result))
                .expect("Thread messaging error");
        });
    }
}

/// Downloads a file to a local filepath, returning `DownloadMsg` variant
/// indicating success or failure.
fn download_file(mut ep_data: EpData, dest: PathBuf, mut max_retries: usize) -> DownloadMsg {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()
        .expect("Could not build reqwest::Client");

    let mut response = loop {
        if let Ok(resp) = client.get(&ep_data.url).send() {
            break resp;
        }
        max_retries -= 1;
        if max_retries == 0 {
            return DownloadMsg::ResponseError(ep_data);
        }
    };

    // figure out the file type
    // assume .mp3 unless we figure out otherwise
    let ext = get_file_ext(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .ok(),
        &ep_data.url,
    )
    .unwrap_or("mp3");

    let mut file_name = sanitize_with_options(
        &ep_data.title,
        Options {
            truncate: true,
            windows: true, // for simplicity, we'll just use Windows-friendly paths for everyone
            replacement: "",
        },
    );

    if let Some(pubdate) = ep_data.pubdate {
        file_name = format!("{}_{}", file_name, pubdate.format("%Y%m%d_%H%M%S"));
    }

    let mut file_path = dest;
    file_path.push(format!("{file_name}.{ext}"));

    let dest = File::create(&file_path);
    if dest.is_err() {
        return DownloadMsg::FileCreateError(ep_data);
    }

    ep_data.file_path = Some(file_path.clone());

    if response.copy_to(&mut dest.unwrap()).is_ok() {
        ep_data.duration = audio_duration_file(file_path).ok();
        DownloadMsg::Complete(ep_data)
    } else {
        DownloadMsg::FileWriteError(ep_data)
    }
}

/// Returns what the extension of a downloaded file should be, based first on
/// its mime type, and then on its URL if the mime type is missing or unknown
/// Reference: <https://www.iana.org/assignments/media-types/media-types.xhtml>
fn get_file_ext<'a>(mime_type: Option<&str>, url: &'a str) -> Option<&'a str> {
    match mime_type {
        // Audio
        Some("audio/3gpp" | "video/3gpp") => Some("3gp"),
        Some("audio/aac") => Some("aac"),
        Some("audio/flac") => Some("flac"),
        Some("audio/x-m4a") => Some("m4a"),
        Some("audio/matroska") => Some("mka"),
        Some("audio/midi" | "audio/x-midi") => Some("mid"),
        Some("audio/midi-clip") => Some("midi2"),
        Some("audio/mp4" | "video/mp4") => Some("mp4"),
        Some("audio/mpeg") => Some("mp3"),
        Some("audio/ogg" | "audio/vorbis") => Some("oga"),
        Some("audio/opus") => Some("opus"),
        Some("audio/wav") => Some("wav"),
        Some("audio/webm") => Some("weba"),
        Some("video/3gpp2") => Some("3g2"),
        Some("video/matroska") => Some("mkv"),
        Some("video/matroska-3d") => Some("mk3d"),
        Some("video/quicktime") => Some("mov"),
        Some("video/x-m4v") => Some("m4v"),
        // Otherwise, use the extension in the URL as a fallback
        _ => {
            // Look for what's after the last slash (/)
            url.rsplit('/')
                .next()
                .and_then(|file_name| file_name.rsplit('.').next())
        }
    }
}
