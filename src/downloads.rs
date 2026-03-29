use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sanitize_filename::{Options, sanitize_with_options};
use tokio::sync::Semaphore;

use crate::types::Message;
use crate::utils::audio_duration_file;

/// Enum used for communicating download results back to the main controller.
#[derive(Debug)]
pub enum DownloadMsg {
    Complete(EpData),
    Error(EpData, DownloadError),
}

#[derive(Debug)]
pub enum DownloadError {
    Response,
    FileCreate,
    FileWrite,
}

/// Enum used to communicate relevant data about an episode download.
#[derive(Debug, Clone)]
pub struct EpData {
    pub id: i64,
    pub pod_id: i64,
    pub title: String,
    pub url: String,
    pub pubdate: Option<DateTime<Utc>>,
    pub file_path: Option<PathBuf>,
    pub duration: Option<u64>,
}

/// This is the function the main controller uses to indicate new
/// files to download. It spawns async tasks for every episode to be
/// downloaded. New jobs can be requested by the user while there are
/// still ongoing jobs.
pub fn download_list(
    episodes: Vec<EpData>, dest: &Path, max_retries: usize, semaphore: &Arc<Semaphore>,
    tx_to_main: &Sender<Message>,
) {
    for ep in episodes {
        let tx = tx_to_main.clone();
        let dest2 = dest.to_path_buf();
        let sem = Arc::clone(semaphore);
        tokio::spawn(async move {
            let _permit = sem.acquire().await;
            let result = download_file(ep, dest2, max_retries).await;
            if tx.send(Message::Dl(result)).is_err() {
                log::error!("Failed to send download message: channel closed");
            }
        });
    }
}

/// Downloads a file to a local filepath, returning `DownloadMsg` variant
/// indicating success or failure.
async fn download_file(mut ep_data: EpData, dest: PathBuf, mut max_retries: usize) -> DownloadMsg {
    let Ok(client) = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()
    else {
        return DownloadMsg::Error(ep_data, DownloadError::Response);
    };

    let response = loop {
        if let Ok(resp) = client.get(&ep_data.url).send().await {
            break resp;
        }
        max_retries -= 1;
        if max_retries == 0 {
            return DownloadMsg::Error(ep_data, DownloadError::Response);
        }
    };
    let default_header: &str = "audio/mpeg";
    let header = response
        .headers()
        .get("content-type")
        .map(|h| h.to_str().unwrap_or(default_header));

    // figure out the file type
    // assume .mp3 unless we figure out otherwise
    let ext = get_file_ext(header, &ep_data.url).unwrap_or("mp3");

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

    let Ok(bytes) = response.bytes().await else {
        return DownloadMsg::Error(ep_data, DownloadError::FileWrite);
    };

    ep_data.file_path = Some(file_path.clone());

    if tokio::fs::write(&file_path, &bytes).await.is_ok() {
        let path = file_path.clone();
        ep_data.duration = tokio::task::spawn_blocking(move || audio_duration_file(path))
            .await
            .ok()
            .and_then(Result::ok);
        DownloadMsg::Complete(ep_data)
    } else {
        DownloadMsg::Error(ep_data, DownloadError::FileCreate)
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
