use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard};

use chrono::{DateTime, Utc};
use nohash_hasher::BuildNoHashHasher;

use crate::downloads::DownloadMsg;
use crate::feeds::FeedMsg;
use crate::ui::UiMsg;
use crate::utils::{format_duration, StringUtils};

/// Struct holding data about an individual podcast feed. This includes a
/// (possibly empty) vector of episodes.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Podcast {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub explicit: Option<bool>,
    pub last_checked: DateTime<Utc>,
    pub episodes: LockVec<Episode>,
}

impl Podcast {
    /// Counts and returns the number of unplayed episodes in the podcast.
    fn num_unplayed(&self) -> usize {
        self.episodes
            .map(|ep| !ep.is_played() as usize, false)
            .iter()
            .sum()
    }
}

impl PartialEq for Podcast {
    fn eq(&self, other: &Self) -> bool {
        self.title == other.title
    }
}
impl Eq for Podcast {}

impl PartialOrd for Podcast {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Podcast {
    fn cmp(&self, other: &Self) -> Ordering {
        self.title.cmp(&other.title)
    }
}

/// Struct holding data about an individual podcast episode. Most of this
/// is metadata, but if the episode has been downloaded to the local
/// machine, the filepath will be included here as well. `played`
/// indicates whether the podcast has been marked as played or unplayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    pub id: i64,
    pub pod_id: i64,
    pub title: String,
    pub url: String,
    pub guid: String,
    pub description: String,
    pub pubdate: Option<DateTime<Utc>>,
    pub duration: Option<i64>,
    pub position: i64,
    pub path: Option<PathBuf>,
    pub played: bool,
}

impl Ord for Episode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.pubdate.cmp(&other.pubdate)
    }
}

impl PartialOrd for Episode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Struct holding data about an individual podcast feed, before it has
/// been inserted into the database. This includes a
/// (possibly empty) vector of episodes.
#[derive(Debug, Clone)]
pub struct PodcastNoId {
    pub title: String,
    pub url: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub explicit: Option<bool>,
    pub last_checked: DateTime<Utc>,
    pub episodes: Vec<EpisodeNoId>,
}

/// Struct holding data about an individual podcast episode, before it
/// has been inserted into the database.
#[derive(Debug, Clone)]
pub struct EpisodeNoId {
    pub title: String,
    pub url: String,
    pub guid: String,
    pub description: String,
    pub pubdate: Option<DateTime<Utc>>,
    pub duration: Option<i64>,
}

/// Struct holding data about an individual podcast episode, specifically
/// for the popup window that asks users which new episodes they wish to
/// download.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct NewEpisode {
    pub id: i64,
    pub pod_id: i64,
    pub title: String,
    pub pod_title: String,
    pub selected: bool,
}

/// Defines interface used for both podcasts and episodes, to be
/// used and displayed in menus.
pub trait Menuable {
    fn get_id(&self) -> i64;
    fn get_title(&self, length: usize) -> String;
    fn is_played(&self) -> bool;
}

impl Menuable for Podcast {
    /// Returns the database ID for the podcast.
    fn get_id(&self) -> i64 {
        self.id
    }

    /// Returns the title for the podcast, up to length characters.
    fn get_title(&self, length: usize) -> String {
        let mut title_length = length;

        // if the size available is big enough, we add the unplayed data
        // to the end
        if length > crate::config::PODCAST_UNPLAYED_TOTALS_LENGTH {
            let meta_str = format!("({}/{})", self.num_unplayed(), self.episodes.len(false));
            title_length = length - meta_str.chars().count() - 3;

            let out = self.title.substr(0, title_length);

            format!(
                " {out} {meta_str:>width$} ",
                width = length - out.grapheme_len() - 3
            ) // this pads spaces between title and totals
        } else {
            format!(" {} ", self.title.substr(0, title_length - 2))
        }
    }

    fn is_played(&self) -> bool {
        self.num_unplayed() == 0
    }
}

impl Menuable for Episode {
    /// Returns the database ID for the episode.
    fn get_id(&self) -> i64 {
        self.id
    }

    /// Returns the title for the episode, up to length characters.
    fn get_title(&self, length: usize) -> String {
        let played = '✔';
        let downloaded = '↓';
        let title = self.title.substr(0, length - 3);
        let out = format!(
            "{}{} {}",
            if self.played { played } else { ' ' },
            if self.path.is_some() { downloaded } else { ' ' },
            title
        );

        if length > crate::config::EPISODE_DURATION_LENGTH {
            let dur = format_duration(self.duration.map(|x| x as u64));
            let meta_dur = format!("[{dur}]");
            let out_added = out.substr(0, length - meta_dur.chars().count() - 3);
            format!(
                " {out_added} {meta_dur:>width$} ",
                width = length - out_added.grapheme_len() - 3
            )
        } else {
            format!(" {} ", out.substr(0, length - 2))
        }
    }

    fn is_played(&self) -> bool {
        self.played
    }
}

impl Menuable for NewEpisode {
    /// Returns the database ID for the episode.
    fn get_id(&self) -> i64 {
        self.id
    }

    /// Returns the title for the episode, up to length characters.
    fn get_title(&self, length: usize) -> String {
        let selected = if self.selected { "✓" } else { " " };

        let title_len = self.title.grapheme_len();
        let pod_title_len = self.pod_title.grapheme_len();
        let empty_string = if length > title_len + pod_title_len + 9 {
            let empty = vec![" "; length - title_len - pod_title_len - 9];
            empty.join("")
        } else {
            "".to_string()
        };

        let full_string = format!(
            " [{}] {} ({}){} ",
            selected, self.title, self.pod_title, empty_string
        );
        full_string.substr(0, length)
    }

    fn is_played(&self) -> bool {
        true
    }
}

/// Struct used to hold a vector of data inside a reference-counted
/// mutex, to allow for multiple owners of mutable data.
/// Primarily, the LockVec is used to provide methods that abstract
/// away some of the logic necessary for borrowing and locking the
/// Arc<Mutex<_>>.
///
/// The data is structured in a way to allow for quick access both by
/// item ID (using a hash map), as well as by the order of an item in
/// the list (using a vector of the item IDs). The `order` vector
/// provides the full order of all the podcasts/episodes that are
/// present in the hash map; the `filtered_order` vector provides the
/// order only for the items that are currently filtered in, if the
/// user has set an active filter for played/unplayed or downloaded/
/// undownloaded.
type ShareableRwLock<T> = Arc<RwLock<T>>;
type ShareableMutex<T> = Arc<Mutex<T>>;
#[derive(Debug)]
pub struct LockVec<T>
where
    T: Clone + Menuable,
{
    data: ShareableMutex<HashMap<i64, ShareableRwLock<T>, BuildNoHashHasher<i64>>>,
    order: Arc<Mutex<Vec<i64>>>,
    filtered_order: Arc<Mutex<Vec<i64>>>,
}

impl<T: Clone + Menuable> LockVec<T> {
    /// Create a new LockVec.
    pub fn new(data: Vec<T>) -> LockVec<T> {
        let mut hm = HashMap::with_hasher(BuildNoHashHasher::default());
        let mut order = Vec::new();
        for i in data.into_iter() {
            let id = i.get_id();
            hm.insert(i.get_id(), Arc::new(RwLock::new(i)));
            order.push(id);
        }

        LockVec {
            data: Arc::new(Mutex::new(hm)),
            order: Arc::new(Mutex::new(order.clone())),
            filtered_order: Arc::new(Mutex::new(order)),
        }
    }

    pub fn new_arc(data: Vec<Arc<RwLock<T>>>) -> LockVec<T> {
        let mut hm = HashMap::with_hasher(BuildNoHashHasher::default());
        let mut order = Vec::new();
        for i in data.into_iter() {
            let id = i.read().unwrap().get_id();
            hm.insert(id, i);
            order.push(id);
        }

        LockVec {
            data: Arc::new(Mutex::new(hm)),
            order: Arc::new(Mutex::new(order.clone())),
            filtered_order: Arc::new(Mutex::new(order)),
        }
    }

    pub fn push(&self, item: T) {
        let id = item.get_id();
        let (mut map, mut order, mut filtered_order) = self.borrow();
        map.insert(id, Arc::new(RwLock::new(item)));
        order.push(id);
        filtered_order.push(id);
    }

    pub fn push_arc(&self, item: Arc<RwLock<T>>) {
        let id = item.read().unwrap().get_id();
        let (mut map, mut order, mut filtered_order) = self.borrow();
        map.insert(id, item);
        order.push(id);
        filtered_order.push(id);
    }

    pub fn remove(&self, id: i64) {
        let (mut map, mut order, mut filtered_order) = self.borrow();
        map.remove(&id);
        order.retain(|&x| x != id);
        filtered_order.retain(|&x| x != id);
    }

    pub fn get(&self, id: i64) -> Option<Arc<RwLock<T>>> {
        let borrowed = self.borrow_map();
        borrowed.get(&id).cloned()
    }

    pub fn contains_key(&self, id: i64) -> bool {
        let borrowed = self.borrow_map();
        borrowed.contains_key(&id)
    }

    /// Lock the LockVec hashmap for reading/writing.
    pub fn borrow_map(&self) -> MutexGuard<HashMap<i64, Arc<RwLock<T>>, BuildNoHashHasher<i64>>> {
        self.data.lock().expect("Mutex error")
    }

    /// Lock the LockVec order vector for reading/writing.
    pub fn borrow_order(&self) -> MutexGuard<Vec<i64>> {
        self.order.lock().expect("Mutex error")
    }

    /// Lock the LockVec filtered order vector for reading/writing.
    pub fn borrow_filtered_order(&self) -> MutexGuard<Vec<i64>> {
        self.filtered_order.lock().expect("Mutex error")
    }

    /// Lock the LockVec hashmap for reading/writing.
    #[allow(clippy::type_complexity)]
    pub fn borrow(
        &self,
    ) -> (
        MutexGuard<HashMap<i64, Arc<RwLock<T>>, BuildNoHashHasher<i64>>>,
        MutexGuard<Vec<i64>>,
        MutexGuard<Vec<i64>>,
    ) {
        (
            self.data.lock().expect("Mutex error"),
            self.order.lock().expect("Mutex error"),
            self.filtered_order.lock().expect("Mutex error"),
        )
    }

    /// Empty out and replace all the data in the LockVec.
    pub fn replace_all(&self, data: Vec<T>) {
        let (mut map, mut order, mut filtered_order) = self.borrow();
        map.clear();
        order.clear();
        filtered_order.clear();
        for i in data.into_iter() {
            let id = i.get_id();
            map.insert(i.get_id(), Arc::new(RwLock::new(i)));
            order.push(id);
            filtered_order.push(id);
        }
    }

    pub fn replace_all_arc(&self, data: Vec<Arc<RwLock<T>>>) {
        let (mut map, mut order, mut filtered_order) = self.borrow();
        map.clear();
        order.clear();
        filtered_order.clear();
        for i in data.into_iter() {
            let id = i.read().unwrap().get_id();
            map.insert(id, i);
            order.push(id);
            filtered_order.push(id);
        }
    }

    /// Maps a closure to every element in the LockVec, in the same way
    /// as an Iterator. However, to avoid issues with keeping the borrow
    /// alive, the function returns a Vec of the collected results,
    /// rather than an iterator.
    pub fn map<B, F>(&self, mut f: F, filtered: bool) -> Vec<B>
    where
        F: FnMut(&RwLockReadGuard<T>) -> B,
    {
        let (map, order, filtered_order) = self.borrow();
        if filtered {
            filtered_order
                .iter()
                .map(|id| f(&map.get(id).expect("Index error in LockVec").read().unwrap()))
                .collect()
        } else {
            order
                .iter()
                .map(|id| f(&map.get(id).expect("Index error in LockVec").read().unwrap()))
                .collect()
        }
    }

    /// Maps a closure to a single element in the LockVec, specified by
    /// `id`. If there is no element `id`, this returns None.
    pub fn map_single<B, F>(&self, id: i64, f: F) -> Option<B>
    where
        F: FnOnce(&T) -> B,
    {
        let borrowed = self.borrow_map();
        borrowed.get(&id).map(|x| f(&x.read().unwrap()))
    }

    /// Maps a closure to a single element in the LockVec, specified by
    /// `index` (position order). If there is no element at that index,
    /// this returns None.
    pub fn map_single_by_index<B, F>(&self, index: usize, f: F) -> Option<B>
    where
        F: FnOnce(&T) -> B,
    {
        let order = self.borrow_order();
        match order.get(index) {
            Some(id) => self.map_single(*id, f),
            None => None,
        }
    }

    /// Maps a closure to every element in the LockVec, in the same way
    /// as the `filter_map()` does on an Iterator, both mapping and
    /// filtering. However, to avoid issues with keeping the borrow
    /// alive, the function returns a Vec of the collected results,
    /// rather than an iterator.
    ///
    /// Note that the word "filter" in this sense represents the concept
    /// from functional programming, providing a function that evaluates
    /// items in the list and returns a boolean value. The word "filter"
    /// is used elsewhere in the code to represent user-selected
    /// filters to show only selected podcasts/episodes, but this is
    /// *not* the sense of the word here.
    pub fn filter_map<B, F>(&self, mut f: F) -> Vec<B>
    where
        F: FnMut(&Arc<RwLock<T>>) -> Option<B>,
    {
        let (map, order, _u) = self.borrow();
        order
            .iter()
            .filter_map(|id| f(map.get(id).expect("Index error in LockVec")))
            .collect()
    }

    /// Returns the number of items in the LockVec.
    pub fn len(&self, filtered: bool) -> usize {
        if filtered {
            return self.borrow_filtered_order().len();
        } else {
            return self.borrow_order().len();
        }
    }

    /// Returns whether or not there are any items in the LockVec.
    pub fn is_empty(&self) -> bool {
        return self.borrow_order().is_empty();
    }
}

impl<T: Clone + Menuable> Clone for LockVec<T> {
    fn clone(&self) -> Self {
        LockVec {
            data: Arc::clone(&self.data),
            order: Arc::clone(&self.order),
            filtered_order: Arc::clone(&self.filtered_order),
        }
    }
}

impl LockVec<Podcast> {
    pub fn get_episodes_map(&self) -> Option<HashMap<i64, Arc<RwLock<Episode>>>> {
        let mut all_ep_map = HashMap::new();
        let pod_map = self.borrow_map();
        for (_pod_id, pod) in pod_map.iter() {
            let rpod = pod.read().unwrap();
            let ep_map = rpod.episodes.borrow_map();
            for (ep_id, ep) in ep_map.iter() {
                all_ep_map.insert(*ep_id, ep.clone());
            }
        }
        Some(all_ep_map)
    }
}

impl LockVec<Episode> {
    pub fn sort(&self) {
        let dt = DateTime::from_timestamp(0, 0).unwrap();
        let mut epvec = self
            .borrow_map()
            .iter()
            .map(|(id, ep)| {
                if let Some(t) = ep.read().unwrap().pubdate {
                    (t, *id)
                } else {
                    (dt, *id)
                }
            })
            .collect::<Vec<(DateTime<Utc>, i64)>>();
        epvec.sort();

        let sforder = self
            .borrow_filtered_order()
            .clone()
            .into_iter()
            .collect::<HashSet<i64>>();
        let mut norder = Vec::new();
        let mut nforder = Vec::new();
        for (_, i) in epvec {
            if sforder.contains(&i) {
                norder.push(i);
                nforder.push(i);
            }
        }
        let mut order = self.borrow_order();
        *order = norder;
        let mut forder = self.borrow_filtered_order();
        *forder = nforder;
    }

    pub fn reverse(&self) {
        self.borrow_order().reverse();
        self.borrow_filtered_order().reverse();
    }
}

/// Simple enum to designate the status of a filter. "Positive" and
/// "Negative" cases represent, e.g., "played" vs. "unplayed".
#[derive(Debug, Clone, Copy)]
pub enum FilterStatus {
    PositiveCases,
    NegativeCases,
    All,
}

/// Enum to identify which filters has been changed
#[derive(Debug, Clone, Copy)]
pub enum FilterType {
    Played,
    Downloaded,
}

/// Struct holding information about all active filters.
#[derive(Debug, Clone, Copy)]
pub struct Filters {
    pub played: FilterStatus,
    pub downloaded: FilterStatus,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            played: FilterStatus::All,
            downloaded: FilterStatus::All,
        }
    }
}

/// Overarching Message enum that allows multiple threads to communicate
/// back to the main thread with a single enum type.
#[derive(Debug)]
pub enum Message {
    Ui(UiMsg),
    Feed(FeedMsg),
    Dl(DownloadMsg),
}
