use anyhow::{Result, anyhow};
use reqwest::{StatusCode, Url};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rss::{Channel, Item};
use tokio::sync::Semaphore;

use crate::types::{EpisodeNoId, Message, PodcastNoId};
use crate::utils::{APP_USER_AGENT, urls_equivalent};

/// Same limit as reqwest's default redirect policy.
const MAX_REDIRECTS: usize = 10;

/// Enum for communicating back to the main thread after feed data has
/// been retrieved.
#[derive(Debug)]
pub enum FeedMsg {
    NewData(PodcastNoId, FetchedUrls),
    SyncData((i64, PodcastNoId)),
    Error(PodcastFeed),
}

/// The URLs a new feed was fetched through. `PodcastNoId::url` holds the URL
/// to store, which differs from these when the feed has moved.
#[derive(Debug, Clone)]
pub struct FetchedUrls {
    /// The URL that was requested.
    pub requested: String,
    /// The URL the feed was finally served from, after all redirects.
    pub final_url: String,
}

/// Whether a newly fetched feed is already subscribed under any of its URLs:
/// the one requested, the one it was finally served from, or the one it moved
/// to.
pub fn is_already_subscribed<'a>(
    pod: &PodcastNoId, urls: &FetchedUrls, subscribed: impl IntoIterator<Item = &'a str>,
) -> bool {
    let candidates = [pod.url.as_str(), &urls.requested, &urls.final_url];
    subscribed.into_iter().any(|known| {
        candidates
            .iter()
            .any(|candidate| urls_equivalent(known, candidate))
    })
}

/// Struct holding data about a podcast feed (subset of info about a
/// podcast) for the purpose of passing back and forth between threads.
#[derive(Debug, Clone)]
pub struct PodcastFeed {
    pub id: Option<i64>,
    pub url: String,
    pub title: Option<String>,
}

impl PodcastFeed {
    pub const fn new(id: Option<i64>, url: String, title: Option<String>) -> Self {
        Self { id, url, title }
    }
}

/// Spawns a new async task to check a feed and retrieve podcast data.
pub fn check_feed(
    feed: PodcastFeed, max_retries: usize, semaphore: Arc<Semaphore>,
    tx_to_main: mpsc::Sender<Message>,
) {
    tokio::spawn(async move {
        let _permit = semaphore.acquire().await;
        let msg = match get_feed_data(&feed.url, max_retries).await {
            Ok((pod, final_url)) => match feed.id {
                Some(id) => Message::Feed(FeedMsg::SyncData((id, pod))),
                None => Message::Feed(FeedMsg::NewData(
                    pod,
                    FetchedUrls {
                        requested: feed.url,
                        final_url,
                    },
                )),
            },
            Err(_err) => Message::Feed(FeedMsg::Error(feed)),
        };
        if tx_to_main.send(msg).is_err() {
            log::error!("Failed to send feed message: channel closed");
        }
    });
}

/// Given a URL, this attempts to pull the data about a podcast and its
/// episodes from an RSS feed. Returns the podcast, whose URL follows the feed
/// if it moved (see [`moved_feed_url`]), and the URL it was finally served
/// from.
async fn get_feed_data(url: &str, mut max_retries: usize) -> Result<(PodcastNoId, String)> {
    let redirected_to: Arc<Mutex<Option<String>>> = Arc::default();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .user_agent(APP_USER_AGENT)
        .redirect(record_permanent_redirect(Arc::clone(&redirected_to)))
        .build()?;

    let response = loop {
        // Only the redirects of the response we end up using count
        *redirected_to.lock().expect("Mutex lock should not fail") = None;
        if let Ok(resp) = client.get(url).send().await {
            if resp.status().is_success() {
                break Ok(resp);
            }
            max_retries -= 1;
            if max_retries == 0 {
                break Err(anyhow!("Failed to fetch feed: Status {}", resp.status()));
            }
        } else {
            max_retries -= 1;
            if max_retries == 0 {
                break Err(anyhow!("No response from feed"));
            }
        }
    }?;

    let final_url = response.url().to_string();
    let resp_data = response.bytes().await?;

    let channel = Channel::read_from(&resp_data[..])?;
    let redirected_to = redirected_to
        .lock()
        .expect("Mutex lock should not fail")
        .take();
    let url = moved_feed_url(url, redirected_to, &channel).unwrap_or_else(|| url.to_string());
    Ok((parse_feed_data(channel, &url), final_url))
}

/// A redirect policy that follows redirects like reqwest's default one, and
/// records where the first hop leads when it is a permanent move.
fn record_permanent_redirect(
    redirected_to: Arc<Mutex<Option<String>>>,
) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        if let [first] = attempt.previous()
            && is_permanent_redirect(attempt.status(), first, attempt.url())
        {
            *redirected_to.lock().expect("Mutex lock should not fail") =
                Some(attempt.url().to_string());
        }
        attempt.follow()
    })
}

/// Whether a redirect means the resource moved for good: a 301 or 308, or a
/// redirect that only upgrades http to https. `AntennaPod` treats both alike.
fn is_permanent_redirect(status: StatusCode, from: &Url, to: &Url) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY | StatusCode::PERMANENT_REDIRECT
    ) || (from.scheme() == "http" && to.as_str() == from.as_str().replacen("http", "https", 1))
}

/// The URL a feed now lives at, if it differs from the requested one: where a
/// permanent redirect on the first hop led, or else the feed's own
/// `<itunes:new-feed-url>`. This follows `AntennaPod`, so both clients agree on
/// when a feed has moved; a temporary redirect keeps the requested URL.
///
/// Unlike `AntennaPod`, a `new-feed-url` that only differs cosmetically from the
/// requested URL (see [`urls_equivalent`]) is ignored. Otherwise a feed that
/// names its http URL while that URL redirects to https would switch URLs on
/// every refresh.
fn moved_feed_url(
    requested: &str, redirected_to: Option<String>, channel: &Channel,
) -> Option<String> {
    let new_feed_url = channel
        .itunes_ext()
        .and_then(|itunes| itunes.new_feed_url())
        .map(str::trim)
        .filter(|url| Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https")))
        .filter(|url| !urls_equivalent(url, requested))
        .map(str::to_string);
    redirected_to
        .or(new_feed_url)
        .filter(|url| url != requested)
}

/// Given a Channel with the RSS feed data, this parses the data about a
/// podcast and its episodes and returns a Podcast. There are existing
/// specifications for podcast RSS feeds that a feed should adhere to, but
/// this does try to make some attempt to account for the possibility that
/// a feed might not be valid according to the spec.
fn parse_feed_data(channel: Channel, url: &str) -> PodcastNoId {
    let title = channel.title().to_string();
    let url = url.to_string();
    let description = Some(channel.description().to_string());
    let last_checked = Utc::now();

    let mut author = None;
    let explicit = channel.itunes_ext().and_then(|itunes| {
        author = itunes.author().map(std::string::ToString::to_string);
        itunes.explicit().and_then(|s| {
            let ss = s.to_lowercase();
            match &ss[..] {
                "yes" | "explicit" | "true" => Some(true),
                "no" | "clean" | "false" => Some(false),
                _ => None,
            }
        })
    });

    let mut episodes = Vec::new();
    let items = channel.into_items();
    if !items.is_empty() {
        for item in &items {
            episodes.push(parse_episode_data(item));
        }
    }

    PodcastNoId {
        title,
        url,
        description,
        author,
        explicit,
        last_checked,
        episodes,
    }
}

/// For an item (episode) in an RSS feed, this pulls data about the item
/// and converts it to an Episode. There are existing specifications for
/// podcast RSS feeds that a feed should adhere to, but this does try to
/// make some attempt to account for the possibility that a feed might
/// not be valid according to the spec.
fn parse_episode_data(item: &Item) -> EpisodeNoId {
    let title = item
        .title()
        .map_or_else(String::new, std::string::ToString::to_string);
    let url = item
        .enclosure()
        .map_or_else(String::new, |enc| enc.url().to_string());
    let guid = item
        .guid()
        .map_or_else(String::new, |guid| guid.value().to_string());
    let description = item
        .description()
        .map_or_else(String::new, std::string::ToString::to_string);
    let pubdate = item
        .pub_date()
        .and_then(|pd| DateTime::parse_from_rfc2822(pd).map(|dt| dt.to_utc()).ok());

    let duration = if let Some(itunes) = item.itunes_ext()
        && let Some(itures_duration) = itunes.duration()
    {
        parse_duration(itures_duration).ok()
    } else {
        None
    };

    EpisodeNoId {
        title,
        url,
        guid,
        description,
        pubdate,
        duration,
    }
}

fn parse_duration(s: &str) -> Result<u64> {
    let parts: Vec<&str> = s.split(':').collect();

    // Depending on the number of parts, assign hour, minute, second
    match parts.len() {
        1 => {
            // SS
            Ok(parts[0].parse::<u64>()?)
        }
        2 => {
            // MM:SS
            let minutes = parts[0].parse::<u64>()?;
            let seconds = parts[1].parse::<u64>()?;
            Ok(minutes * 60 + seconds)
        }
        3 => {
            // HH:MM:SS
            let hours = parts[0].parse::<u64>()?;
            let minutes = parts[1].parse::<u64>()?;
            let seconds = parts[2].parse::<u64>()?;
            Ok(hours * 3600 + minutes * 60 + seconds)
        }
        _ => Err(anyhow!("Wrong number of parts")),
    }
}

// TESTS -----------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::BufReader;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn open_file(path: &str) -> BufReader<File> {
        BufReader::new(File::open(path).unwrap())
    }

    fn feed_xml(new_feed_url: Option<&str>) -> String {
        let new_feed_url = new_feed_url
            .map(|url| format!("<itunes:new-feed-url>{url}</itunes:new-feed-url>"))
            .unwrap_or_default();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
<channel>
<title>Test</title><link>https://example.com</link><description>Test feed</description>
{new_feed_url}
<item><title>Episode</title><guid>guid-1</guid>
<enclosure url="https://example.com/ep1.mp3" length="1" type="audio/mpeg"/></item>
</channel>
</rss>"#
        )
    }

    async fn serve_feed(server: &MockServer, route: &str, new_feed_url: Option<&str>) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_string(feed_xml(new_feed_url)))
            .mount(server)
            .await;
    }

    async fn serve_redirect(server: &MockServer, from: &str, status: u16, to: &str) {
        Mock::given(method("GET"))
            .and(path(from))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("Location", format!("{}{to}", server.uri())),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn permanent_redirect_moves_feed() {
        for status in [301, 308] {
            let server = MockServer::start().await;
            serve_redirect(&server, "/old", status, "/new").await;
            serve_feed(&server, "/new", None).await;

            let (pod, final_url) = get_feed_data(&format!("{}/old", server.uri()), 1)
                .await
                .unwrap();
            assert_eq!(pod.url, format!("{}/new", server.uri()), "{status}");
            assert_eq!(final_url, format!("{}/new", server.uri()), "{status}");
        }
    }

    #[tokio::test]
    async fn temporary_redirect_keeps_requested_url() {
        for status in [302, 303, 307] {
            let server = MockServer::start().await;
            serve_redirect(&server, "/old", status, "/new").await;
            serve_feed(&server, "/new", None).await;

            let (pod, final_url) = get_feed_data(&format!("{}/old", server.uri()), 1)
                .await
                .unwrap();
            assert_eq!(pod.url, format!("{}/old", server.uri()), "{status}");
            assert_eq!(final_url, format!("{}/new", server.uri()), "{status}");
        }
    }

    #[tokio::test]
    async fn only_the_first_redirect_decides_a_move() {
        let server = MockServer::start().await;
        serve_redirect(&server, "/a", 302, "/b").await;
        serve_redirect(&server, "/b", 301, "/c").await;
        serve_redirect(&server, "/d", 301, "/e").await;
        serve_redirect(&server, "/e", 302, "/c").await;
        serve_feed(&server, "/c", None).await;

        let (pod, _) = get_feed_data(&format!("{}/a", server.uri()), 1)
            .await
            .unwrap();
        assert_eq!(pod.url, format!("{}/a", server.uri()));

        let (pod, _) = get_feed_data(&format!("{}/d", server.uri()), 1)
            .await
            .unwrap();
        assert_eq!(pod.url, format!("{}/e", server.uri()));
    }

    #[tokio::test]
    async fn new_feed_url_moves_feed() {
        let server = MockServer::start().await;
        serve_feed(&server, "/moved", Some("https://example.org/feed.xml")).await;
        serve_feed(&server, "/invalid", Some("not a url")).await;

        let (pod, final_url) = get_feed_data(&format!("{}/moved", server.uri()), 1)
            .await
            .unwrap();
        assert_eq!(pod.url, "https://example.org/feed.xml");
        assert_eq!(final_url, format!("{}/moved", server.uri()));

        let (pod, _) = get_feed_data(&format!("{}/invalid", server.uri()), 1)
            .await
            .unwrap();
        assert_eq!(pod.url, format!("{}/invalid", server.uri()));
    }

    #[test]
    fn cosmetic_new_feed_url_is_ignored() {
        // The Joy of Why names its http URL, which redirects back to https:
        // following it would switch URLs on every refresh.
        let channel =
            Channel::read_from(feed_xml(Some("http://joy.quantamagazine.org")).as_bytes()).unwrap();
        let requested = "https://joy.quantamagazine.org/";
        assert_eq!(moved_feed_url(requested, None, &channel), None);

        // A real redirect still counts
        assert_eq!(
            moved_feed_url(
                "http://joy.quantamagazine.org",
                Some(requested.to_string()),
                &channel
            ),
            Some(requested.to_string())
        );

        // So does a new-feed-url that really points elsewhere
        let channel =
            Channel::read_from(feed_xml(Some("https://example.org/joy.xml")).as_bytes()).unwrap();
        assert_eq!(
            moved_feed_url(requested, None, &channel),
            Some("https://example.org/joy.xml".to_string())
        );
    }

    #[tokio::test]
    async fn permanent_redirect_wins_over_new_feed_url() {
        let server = MockServer::start().await;
        serve_redirect(&server, "/old", 301, "/new").await;
        serve_feed(&server, "/new", Some("https://example.org/feed.xml")).await;

        let (pod, _) = get_feed_data(&format!("{}/old", server.uri()), 1)
            .await
            .unwrap();
        assert_eq!(pod.url, format!("{}/new", server.uri()));
    }

    #[test]
    fn upgrade_to_https_counts_as_permanent() {
        let url = |s: &str| Url::parse(s).unwrap();
        let from = url("http://example.com/feed.xml");
        assert!(is_permanent_redirect(
            StatusCode::FOUND,
            &from,
            &url("https://example.com/feed.xml")
        ));
        assert!(!is_permanent_redirect(
            StatusCode::FOUND,
            &from,
            &url("https://example.com/other.xml")
        ));
        assert!(!is_permanent_redirect(
            StatusCode::FOUND,
            &url("https://example.com/feed.xml"),
            &url("https://cdn.example.com/feed.xml")
        ));
    }

    #[test]
    fn already_subscribed_under_any_fetched_url() {
        let channel = Channel::read_from(feed_xml(None).as_bytes()).unwrap();
        let pod = parse_feed_data(channel, "https://moved.example.com/feed");
        let urls = FetchedUrls {
            requested: "https://old.example.com/feed".to_string(),
            final_url: "https://cdn.example.com/feed".to_string(),
        };

        for known in [
            "https://moved.example.com/feed",
            "http://OLD.example.com/feed/",
            "https://cdn.example.com/feed",
        ] {
            assert!(is_already_subscribed(&pod, &urls, [known]), "{known}");
        }
        assert!(!is_already_subscribed(
            &pod,
            &urls,
            ["https://other.example.com/feed"]
        ));
    }

    #[test]
    fn no_description() {
        let path = "./tests/test_no_description.xml";
        let channel = Channel::read_from(open_file(path)).unwrap();
        let data = parse_feed_data(channel, "dummy_url");
        assert_eq!(data.description, Some(String::new()));
    }

    #[test]
    fn invalid_explicit() {
        let path = "./tests/test_inval_explicit.xml";
        let channel = Channel::read_from(open_file(path)).unwrap();
        let data = parse_feed_data(channel, "dummy_url");
        assert_eq!(data.explicit, None);
    }

    #[test]
    fn no_episodes() {
        let path = "./tests/test_no_episodes.xml";
        let channel = Channel::read_from(open_file(path)).unwrap();
        let data = parse_feed_data(channel, "dummy_url");
        assert_eq!(data.episodes.len(), 0);
    }

    #[test]
    fn nan_duration() {
        let duration = String::from("nan");
        assert!(parse_duration(&duration).is_err());
    }

    #[test]
    fn nonnumeric_duration() {
        let duration = String::from("some string");
        assert!(parse_duration(&duration).is_err());
    }

    #[test]
    fn duration_hhhmmss() {
        let duration = String::from("31:38:42");
        assert_eq!(parse_duration(&duration).ok(), Some(113_922));
    }

    #[test]
    fn duration_hhmmss() {
        let duration = String::from("01:38:42");
        assert_eq!(parse_duration(&duration).ok(), Some(5922));
    }

    #[test]
    fn duration_hmmss() {
        let duration = String::from("1:38:42");
        assert_eq!(parse_duration(&duration).ok(), Some(5922));
    }

    #[test]
    fn duration_mmmss() {
        let duration = String::from("68:42");
        assert_eq!(parse_duration(&duration).ok(), Some(4122));
    }

    #[test]
    fn duration_mmss() {
        let duration = String::from("08:42");
        assert_eq!(parse_duration(&duration).ok(), Some(522));
    }

    #[test]
    fn duration_mss() {
        let duration = String::from("8:42");
        assert_eq!(parse_duration(&duration).ok(), Some(522));
    }

    #[test]
    fn duration_sss() {
        let duration = String::from("142");
        assert_eq!(parse_duration(&duration).ok(), Some(142));
    }

    #[test]
    fn duration_ss() {
        let duration = String::from("08");
        assert_eq!(parse_duration(&duration).ok(), Some(8));
    }

    #[test]
    fn duration_s() {
        let duration = String::from("8");
        assert_eq!(parse_duration(&duration).ok(), Some(8));
    }

    #[test]
    fn duration_too_many_parts() {
        let duration = String::from("1:2:3:4");
        assert!(parse_duration(&duration).is_err());
    }

    #[test]
    fn duration_empty() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn full_feed_parsing() {
        let path = "./tests/test.xml";
        let channel = Channel::read_from(open_file(path)).unwrap();
        let data = parse_feed_data(channel, "https://example.com/feed.xml");

        assert_eq!(data.title, "Reply All");
        assert_eq!(data.url, "https://example.com/feed.xml");
        assert_eq!(data.author.as_deref(), Some("Gimlet"));
        assert_eq!(data.explicit, Some(true));
        assert!(data.description.is_some());
        assert!(!data.episodes.is_empty());
    }

    #[test]
    fn episode_fields_parsed() {
        let path = "./tests/test.xml";
        let channel = Channel::read_from(open_file(path)).unwrap();
        let data = parse_feed_data(channel, "dummy");

        let ep = &data.episodes[0];
        assert_eq!(ep.title, "#20 I Want To Break Free");
        assert_ne!(ep.url, "");
        assert_ne!(ep.guid, "");
        assert_ne!(ep.description, "");
        assert!(ep.pubdate.is_some());
        assert_eq!(ep.duration, Some(1824));
    }

    #[test]
    fn parse_episode_missing_all_optional() {
        let item = Item::default();
        let ep = parse_episode_data(&item);
        assert_eq!(ep.title, "");
        assert_eq!(ep.url, "");
        assert_eq!(ep.guid, "");
        assert_eq!(ep.description, "");
        assert!(ep.pubdate.is_none());
        assert!(ep.duration.is_none());
    }
}
