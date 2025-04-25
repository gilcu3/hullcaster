use crate::types::FilterType;

#[derive(Debug)]
pub enum UiMsg {
    AddFeed(String),
    Play(i64, i64, bool),
    MarkPlayed(i64, i64, bool),
    MarkAllPlayed(i64, bool),
    UpdatePosition(i64, i64, i64),
    Sync(i64),
    SyncAll,
    SyncGpodder,
    Download(i64, i64),
    DownloadAll(i64),
    Delete(i64, i64),
    DeleteAll(i64),
    RemovePodcast(i64, bool),
    FilterChange(FilterType),
    QueueModified,
    Quit,
    Noop,
}
