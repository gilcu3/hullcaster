use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use semver::Version;

use crate::types::{Episode, EpisodeNoId, LockVec, NewEpisode, Podcast, PodcastNoId};
use crate::utils::convert_date;

pub struct SyncResult {
    pub added: Vec<NewEpisode>,
    pub updated: Vec<i64>,
}

/// Struct holding a sqlite database connection, with methods to interact
/// with this connection.
#[derive(Debug)]
pub struct Database {
    conn: Option<Connection>,
}

impl Database {
    fn conn(&self) -> Result<&Connection> {
        self.conn
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Database connection not available"))
    }

    fn conn_mut(&mut self) -> Result<&mut Connection> {
        self.conn
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Database connection not available"))
    }

    /// Creates a new connection to the database (and creates database if
    /// it does not already exist). Panics if database cannot be accessed.
    pub fn connect(path: &Path) -> Result<Self> {
        let mut db_path = path.to_path_buf();
        std::fs::create_dir_all(&db_path)
            .with_context(|| "Unable to create subdirectory for database.")?;
        db_path.push("data.db");
        let conn = Connection::open(&db_path)?;
        let db_conn = Self { conn: Some(conn) };
        db_conn.create()?;

        {
            let conn = db_conn.conn()?;

            // SQLite defaults to foreign key support off
            conn.execute("PRAGMA foreign_keys=ON;", params![])?;

            // get version number stored in database
            let vstr = db_conn.get_param("version");

            // compare to current app version
            let curr_ver = Version::parse(crate::VERSION)?;

            match vstr {
                Ok(vstr) => {
                    let db_version = Version::parse(&vstr)?;
                    if db_version < curr_ver {
                        // Any version checks for DB migrations should
                        // go here first, before we update the version
                        db_conn.set_param("version", &curr_ver.to_string())?;
                    }
                }
                Err(_) => db_conn.set_param("version", &curr_ver.to_string())?,
            }

            // get timestamp number stored in database
            let tstr = db_conn.get_param("timestamp");

            if tstr.is_err() {
                db_conn.set_param("timestamp", "0")?;
            }
        }

        Ok(db_conn)
    }

    pub fn get_param(&self, key: &str) -> Result<String> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT value FROM params WHERE key = ?;")?;
        let param_str: String = stmt.query_row(rusqlite::params![key], |row| row.get(0))?;
        Ok(param_str)
    }

    pub fn set_param(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(
            "INSERT OR REPLACE INTO params (key, value)
                VALUES (?, ?);",
        )?;
        stmt.execute(params![key, value])?;
        Ok(())
    }

    /// Creates the necessary database tables, if they do not already
    /// exist. Panics if database cannot be accessed, or if tables cannot
    /// be created.
    pub fn create(&self) -> Result<()> {
        let conn = self.conn()?;

        // create podcasts table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS podcasts (
                id INTEGER PRIMARY KEY NOT NULL,
                title TEXT NOT NULL,
                url TEXT NOT NULL UNIQUE,
                description TEXT,
                author TEXT,
                explicit INTEGER,
                last_checked INTEGER
            );",
            params![],
        )
        .with_context(|| "Could not create podcasts database table")?;

        // create episodes table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS episodes (
                id INTEGER PRIMARY KEY NOT NULL,
                podcast_id INTEGER NOT NULL,
                title TEXT NOT NULL,
                url TEXT NOT NULL,
                guid TEXT,
                description TEXT,
                pubdate INTEGER,
                duration INTEGER,
                position INTEGER,
                played INTEGER,
                FOREIGN KEY(podcast_id) REFERENCES podcasts(id) ON DELETE CASCADE
            );",
            params![],
        )
        .with_context(|| "Could not create episodes database table")?;

        // create files table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY NOT NULL,
                episode_id INTEGER NOT NULL,
                path TEXT NOT NULL UNIQUE,
                FOREIGN KEY (episode_id) REFERENCES episodes(id) ON DELETE CASCADE
            );",
            params![],
        )
        .with_context(|| "Could not create files database table")?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS queue (
                position INTEGER PRIMARY KEY NOT NULL,
                episode_id INTEGER NOT NULL,
                FOREIGN KEY(episode_id) REFERENCES episodes(id) ON DELETE CASCADE
            );",
            params![],
        )
        .with_context(|| "Could not create queue database table")?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS params (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );",
            params![],
        )
        .with_context(|| "Could not create params database table")?;
        Ok(())
    }

    /// Inserts a new podcast and list of podcast episodes into the
    /// database.
    pub fn insert_podcast(&mut self, podcast: &PodcastNoId) -> Result<SyncResult> {
        let conn = self.conn_mut()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO podcasts (title, url, description, author,
                explicit, last_checked)
                VALUES (?, ?, ?, ?, ?, ?);",
            )?;
            stmt.execute(params![
                podcast.title,
                podcast.url,
                podcast.description,
                podcast.author,
                podcast.explicit,
                podcast.last_checked.timestamp()
            ])?;
        }

        let pod_id;
        {
            let mut stmt = tx.prepare_cached("SELECT id FROM podcasts WHERE url = ?")?;
            pod_id = stmt.query_row::<i64, _, _>(params![podcast.url], |row| row.get(0))?;
        }
        let mut ep_ids = Vec::new();
        for ep in podcast.episodes.iter().rev() {
            let id = Self::insert_episode(&tx, pod_id, ep)?;
            let new_ep = NewEpisode {
                id,
                title: ep.title.clone(),
                pod_title: podcast.title.clone(),
                selected: false,
            };
            ep_ids.push(new_ep);
        }
        tx.commit()?;

        Ok(SyncResult {
            added: ep_ids,
            updated: Vec::new(),
        })
    }

    /// Inserts a podcast episode into the database.
    pub fn insert_episode(
        conn: &Connection, podcast_id: i64, episode: &EpisodeNoId,
    ) -> Result<i64> {
        let pubdate = episode.pubdate.map(|dt| dt.timestamp());

        let mut stmt = conn.prepare_cached(
            "INSERT INTO episodes (podcast_id, title, url, guid,
                description, pubdate, duration, played, position)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?);",
        )?;
        let duration: Option<i64> = episode
            .duration
            .map(std::convert::TryInto::try_into)
            .transpose()?;
        stmt.execute(params![
            podcast_id,
            episode.title,
            episode.url,
            episode.guid,
            episode.description,
            pubdate,
            duration,
            false,
            0,
        ])?;
        Ok(conn.last_insert_rowid())
    }

    /// Inserts a filepath to a downloaded episode.
    pub fn insert_file(&self, episode_id: i64, path: &Path) -> Result<()> {
        let conn = self.conn()?;

        let mut stmt = conn.prepare_cached(
            "INSERT INTO files (episode_id, path)
                VALUES (?, ?);",
        )?;
        stmt.execute(params![episode_id, path.to_str(),])?;
        Ok(())
    }

    /// Removes a file listing for an episode from the database when the
    /// user has chosen to delete the file.
    pub fn remove_file(&self, episode_id: i64) -> Result<()> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached("DELETE FROM files WHERE episode_id = ?;")?;
        stmt.execute(params![episode_id])?;
        Ok(())
    }

    /// Removes all file listings for the selected episode ids.
    pub fn remove_files(&self, episode_ids: &[i64]) -> Result<()> {
        let conn = self.conn()?;

        // convert list of episode ids into a comma-separated String
        let episode_list: Vec<String> = episode_ids
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let episodes = episode_list.join(", ");

        let mut stmt = conn.prepare_cached("DELETE FROM files WHERE episode_id = (?);")?;
        stmt.execute(params![episodes])?;
        Ok(())
    }

    /// Removes a podcast, all episodes, and files from the database.
    pub fn remove_podcast(&self, podcast_id: i64) -> Result<()> {
        let conn = self.conn()?;
        // Note: Because of the foreign key constraints on `episodes`
        // and `files` tables, all associated episodes for this podcast
        // will also be deleted, and all associated file entries for
        // those episodes as well.
        let mut stmt = conn.prepare_cached("DELETE FROM podcasts WHERE id = ?;")?;
        stmt.execute(params![podcast_id])?;
        Ok(())
    }

    /// Updates an existing podcast in the database, where metadata is
    /// changed if necessary, and episodes are updated (modified episodes
    /// are updated, new episodes are inserted).
    pub fn update_podcast(&mut self, pod_id: i64, podcast: &PodcastNoId) -> Result<SyncResult> {
        {
            let conn = self.conn()?;
            let mut stmt = conn.prepare_cached(
                "UPDATE podcasts SET title = ?, url = ?, description = ?,
            author = ?, explicit = ?, last_checked = ?
            WHERE id = ?;",
            )?;
            stmt.execute(params![
                podcast.title,
                podcast.url,
                podcast.description,
                podcast.author,
                podcast.explicit,
                podcast.last_checked.timestamp(),
                pod_id,
            ])?;
        }

        let result = self.update_episodes(pod_id, &podcast.title, &podcast.episodes)?;
        Ok(result)
    }

    /// Updates metadata about episodes that already exist in database,
    /// or inserts new episodes.
    ///
    /// Episodes are checked against the URL and published data in
    /// order to determine if they already exist. As such, an existing
    /// episode that has changed either of these fields will show up as
    /// a "new" episode. The old version will still remain in the
    /// database.
    fn update_episodes(
        &mut self, podcast_id: i64, podcast_title: &str, episodes: &[EpisodeNoId],
    ) -> Result<SyncResult> {
        let old_episodes = self.get_episodes(podcast_id)?;
        let mut old_ep_map = HashMap::new();
        for ep in &old_episodes {
            if !ep.guid.is_empty() {
                old_ep_map.insert(ep.guid.clone(), ep);
            }
        }

        let conn = self.conn_mut()?;
        let tx = conn.transaction()?;

        let mut insert_ep = Vec::new();
        let mut update_ep = Vec::new();
        for new_ep in episodes.iter().rev() {
            let new_pd = new_ep.pubdate.map(|dt| dt.timestamp());

            let mut existing_id = None;

            // primary matching mechanism: check guid to see if it
            // already exists in database
            let mut update = if !new_ep.guid.is_empty()
                && let Some(old_ep) = old_ep_map.get(&new_ep.guid)
            {
                existing_id = Some(old_ep.id);
                Self::check_for_updates(old_ep, new_ep)
            } else {
                false
            };

            // fallback matching: for each existing episode, check the
            // title, url, and pubdate -- if two of the three match, we
            // count it as an existing episode; otherwise, we add it as
            // a new episode
            if existing_id.is_none() {
                for old_ep in old_episodes.iter().rev() {
                    let mut matching = 0;
                    matching += i32::from(new_ep.title == old_ep.title);
                    matching += i32::from(new_ep.url == old_ep.url);

                    if let Some(pd) = new_pd
                        && let Some(old_pd) = old_ep.pubdate
                    {
                        matching += i32::from(pd == old_pd.timestamp());
                    }

                    if matching >= 2 {
                        existing_id = Some(old_ep.id);
                        update = Self::check_for_updates(old_ep, new_ep);
                        break;
                    }
                }
            }

            if let Some(id) = existing_id {
                if update {
                    let mut stmt = tx.prepare_cached(
                        "UPDATE episodes SET title = ?, url = ?,
                            guid = ?, description = ?, pubdate = ?,
                            duration = ? WHERE id = ?;",
                    )?;
                    let duration: Option<i64> = new_ep
                        .duration
                        .map(std::convert::TryInto::try_into)
                        .transpose()?;
                    stmt.execute(params![
                        new_ep.title,
                        new_ep.url,
                        new_ep.guid,
                        new_ep.description,
                        new_pd,
                        duration,
                        id,
                    ])?;
                    update_ep.push(id);
                }
            } else {
                let id = Self::insert_episode(&tx, podcast_id, new_ep)?;
                let new_ep = NewEpisode {
                    id,
                    title: new_ep.title.clone(),
                    pod_title: podcast_title.to_string(),
                    selected: false,
                };
                insert_ep.push(new_ep);
            }
        }
        tx.commit()?;
        Ok(SyncResult {
            added: insert_ep,
            updated: update_ep,
        })
    }

    /// Checks two matching episodes to see whether there are details
    /// that need to be updated (e.g., same episode, but the title has
    /// been changed).
    fn check_for_updates(old_ep: &Episode, new_ep: &EpisodeNoId) -> bool {
        let new_pd = new_ep.pubdate.map(|dt| dt.timestamp());
        let pd_match = if let Some(pd) = new_pd
            && let Some(old_pd) = old_ep.pubdate
        {
            pd == old_pd.timestamp()
        } else {
            false
        };
        if !(new_ep.title == old_ep.title
            && new_ep.url == old_ep.url
            && new_ep.guid == old_ep.guid
            && new_ep.description == old_ep.description
            // do not update duration, we can take it from the audio file
            // && new_ep.duration == old_ep.duration
            && pd_match)
        {
            return true;
        }
        false
    }

    /// Updates an episode to mark it as played or unplayed.
    pub fn set_played_status(
        &self, episode_id: i64, position: u64, duration: Option<u64>, played: bool,
    ) -> Result<()> {
        let conn = self.conn()?;

        let mut stmt = conn.prepare_cached(
            "UPDATE episodes SET played = ?, position = ?, duration = ? WHERE id = ?;",
        )?;
        let duration: Option<i64> = duration.map(std::convert::TryInto::try_into).transpose()?;
        let position: i64 = position.try_into()?;
        stmt.execute(params![played, position, duration, episode_id])?;
        Ok(())
    }

    /// Updates an episode to mark it as played or unplayed.
    pub fn set_played_status_batch(
        &mut self, eps: Vec<(i64, u64, Option<u64>, bool)>,
    ) -> Result<()> {
        let conn = self.conn_mut()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE episodes SET played = ?, position = ?, duration = ? WHERE id = ?;",
            )?;
            for (episode_id, position, duration, played) in eps {
                let position: i64 = position.try_into()?;
                let duration: Option<i64> =
                    duration.map(std::convert::TryInto::try_into).transpose()?;
                stmt.execute(params![played, position, duration, episode_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Generates list of all podcasts in database.
    /// TODO: This should probably use a JOIN statement instead.
    pub fn get_podcasts(&self) -> Result<Vec<Podcast>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached("SELECT * FROM podcasts;")?;
        let podcast_iter = stmt.query_map(params![], |row| {
            let pod_id = row.get("id")?;
            let episodes = self
                .get_episodes(pod_id)
                .map_err(|_| rusqlite::Error::InvalidQuery)?;

            let title: String = row.get("title")?;
            let last_checked = convert_date(row.get("last_checked")?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?;

            Ok(Podcast {
                id: pod_id,
                title,
                url: row.get("url")?,
                description: row.get("description")?,
                author: row.get("author")?,
                explicit: row.get("explicit")?,
                last_checked,
                episodes: LockVec::new(episodes),
            })
        })?;
        let mut podcasts = Vec::new();
        for pc in podcast_iter {
            podcasts.push(pc?);
        }
        podcasts.sort_unstable();

        Ok(podcasts)
    }

    /// Generates list of episodes for a given podcast.
    pub fn get_episodes(&self, pod_id: i64) -> Result<Vec<Episode>> {
        let conn = self.conn()?;

        let mut stmt = conn.prepare_cached(
            "SELECT * FROM episodes
                    LEFT JOIN files ON episodes.id = files.episode_id
                    WHERE episodes.podcast_id = ?
                    ORDER BY pubdate DESC;",
        )?;
        let duration_index = stmt.column_index("duration")?;
        let position_index = stmt.column_index("position")?;
        let episode_iter = stmt.query_map(params![pod_id], |row| {
            let path = row.get::<&str, String>("path").ok().map(PathBuf::from);
            let pubdate: Option<i64> = row.get("pubdate")?;
            let pubdate = pubdate.and_then(|ts| convert_date(ts).ok());
            let duration: Option<i64> = row.get("duration")?;
            let position: i64 = row.get("position")?;
            let duration = duration
                .map(|x| {
                    x.try_into()
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(duration_index, x))
                })
                .transpose()?;
            let position = position
                .try_into()
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(position_index, position))?;
            Ok(Episode {
                id: row.get("id")?,
                pod_id: row.get("podcast_id")?,
                title: row.get("title")?,
                url: row.get("url")?,
                guid: row
                    .get::<&str, Option<String>>("guid")?
                    .unwrap_or_else(String::new),
                description: row.get("description")?,
                pubdate,
                duration,
                position,
                path,
                played: row.get("played")?,
            })
        })?;
        let episodes = episode_iter.flatten().collect();
        Ok(episodes)
    }

    /// Generates list of episodes for a given podcast.
    pub fn get_queue(&self) -> Result<Vec<i64>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(
            "SELECT * FROM queue
            ORDER BY position ASC;",
        )?;
        let episode_iter = stmt.query_map(params![], |row| row.get("episode_id"))?;
        let episodes = episode_iter.flatten().collect();
        Ok(episodes)
    }

    /// Generates list of episodes for a given podcast.
    pub fn set_queue(&mut self, queue: Vec<i64>) -> Result<()> {
        let conn = self.conn_mut()?;
        conn.execute("DELETE FROM queue;", params![])?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO queue (episode_id)
            VALUES (?);",
            )?;
            for episode_id in queue {
                stmt.execute(params![episode_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Deletes all rows in all tables
    pub fn clear_db(&self) -> Result<()> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM files;", params![])?;
        conn.execute("DELETE FROM episodes;", params![])?;
        conn.execute("DELETE FROM podcasts;", params![])?;
        Ok(())
    }

    /// Creates an in-memory database for testing.
    #[cfg(test)]
    fn connect_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn: Some(conn) };
        db.create()?;
        {
            let conn = db.conn()?;
            conn.execute("PRAGMA foreign_keys=ON;", params![])?;
        }
        Ok(db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn sample_podcast() -> PodcastNoId {
        PodcastNoId {
            title: "Test Podcast".to_string(),
            url: "https://example.com/feed.xml".to_string(),
            description: Some("A test podcast".to_string()),
            author: Some("Test Author".to_string()),
            explicit: Some(false),
            last_checked: Utc::now(),
            episodes: vec![
                EpisodeNoId {
                    title: "Episode 1".to_string(),
                    url: "https://example.com/ep1.mp3".to_string(),
                    guid: "guid-ep1".to_string(),
                    description: "First episode".to_string(),
                    pubdate: Some(Utc::now()),
                    duration: Some(3600),
                },
                EpisodeNoId {
                    title: "Episode 2".to_string(),
                    url: "https://example.com/ep2.mp3".to_string(),
                    guid: "guid-ep2".to_string(),
                    description: "Second episode".to_string(),
                    pubdate: Some(Utc::now()),
                    duration: Some(1800),
                },
            ],
        }
    }

    #[test]
    fn create_tables() {
        let db = Database::connect_in_memory().unwrap();
        // Verify tables exist by querying them
        let conn = db.conn.as_ref().unwrap();
        conn.execute("SELECT 1 FROM podcasts LIMIT 1;", params![])
            .unwrap();
        conn.execute("SELECT 1 FROM episodes LIMIT 1;", params![])
            .unwrap();
        conn.execute("SELECT 1 FROM files LIMIT 1;", params![])
            .unwrap();
        conn.execute("SELECT 1 FROM queue LIMIT 1;", params![])
            .unwrap();
        conn.execute("SELECT 1 FROM params LIMIT 1;", params![])
            .unwrap();
    }

    #[test]
    fn params_set_and_get() {
        let db = Database::connect_in_memory().unwrap();
        db.set_param("test_key", "test_value").unwrap();
        assert_eq!(db.get_param("test_key").unwrap(), "test_value");
    }

    #[test]
    fn params_overwrite() {
        let db = Database::connect_in_memory().unwrap();
        db.set_param("key", "v1").unwrap();
        db.set_param("key", "v2").unwrap();
        assert_eq!(db.get_param("key").unwrap(), "v2");
    }

    #[test]
    fn params_missing_key_errors() {
        let db = Database::connect_in_memory().unwrap();
        assert!(db.get_param("nonexistent").is_err());
    }

    #[test]
    fn insert_and_get_podcast() {
        let mut db = Database::connect_in_memory().unwrap();
        let podcast = sample_podcast();

        let result = db.insert_podcast(&podcast).unwrap();
        assert_eq!(result.added.len(), 2);
        assert!(result.updated.is_empty());

        let podcasts = db.get_podcasts().unwrap();
        assert_eq!(podcasts.len(), 1);
        assert_eq!(podcasts[0].title, "Test Podcast");
        assert_eq!(podcasts[0].url, "https://example.com/feed.xml");
        assert_eq!(podcasts[0].author.as_deref(), Some("Test Author"));

        let eps = db.get_episodes(podcasts[0].id).unwrap();
        assert_eq!(eps.len(), 2);
    }

    #[test]
    fn insert_podcast_duplicate_url_fails() {
        let mut db = Database::connect_in_memory().unwrap();
        let podcast = sample_podcast();
        db.insert_podcast(&podcast).unwrap();
        assert!(db.insert_podcast(&podcast).is_err());
    }

    #[test]
    fn remove_podcast_cascades() {
        let mut db = Database::connect_in_memory().unwrap();
        let podcast = sample_podcast();
        db.insert_podcast(&podcast).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        let pod_id = podcasts[0].id;

        // Add a file to one episode
        let eps = db.get_episodes(pod_id).unwrap();
        db.insert_file(eps[0].id, Path::new("/tmp/test.mp3"))
            .unwrap();

        db.remove_podcast(pod_id).unwrap();
        assert!(db.get_podcasts().unwrap().is_empty());
        assert!(db.get_episodes(pod_id).unwrap().is_empty());
    }

    #[test]
    fn update_podcast_updates_metadata() {
        let mut db = Database::connect_in_memory().unwrap();
        let podcast = sample_podcast();
        db.insert_podcast(&podcast).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        let pod_id = podcasts[0].id;

        let mut updated = podcast.clone();
        updated.title = "Updated Title".to_string();
        updated.description = Some("Updated description".to_string());

        db.update_podcast(pod_id, &updated).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        assert_eq!(podcasts[0].title, "Updated Title");
        assert_eq!(
            podcasts[0].description.as_deref(),
            Some("Updated description")
        );
    }

    #[test]
    fn update_podcast_adds_new_episodes() {
        let mut db = Database::connect_in_memory().unwrap();
        let podcast = sample_podcast();
        db.insert_podcast(&podcast).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        let pod_id = podcasts[0].id;

        let mut updated = podcast.clone();
        updated.episodes.push(EpisodeNoId {
            title: "Episode 3".to_string(),
            url: "https://example.com/ep3.mp3".to_string(),
            guid: "guid-ep3".to_string(),
            description: "Third episode".to_string(),
            pubdate: Some(Utc::now()),
            duration: Some(900),
        });

        let result = db.update_podcast(pod_id, &updated).unwrap();
        assert_eq!(result.added.len(), 1);
        assert_eq!(result.added[0].title, "Episode 3");

        let eps = db.get_episodes(pod_id).unwrap();
        assert_eq!(eps.len(), 3);
    }

    #[test]
    fn update_podcast_detects_changes_by_guid() {
        let mut db = Database::connect_in_memory().unwrap();
        let podcast = sample_podcast();
        db.insert_podcast(&podcast).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        let pod_id = podcasts[0].id;

        let mut updated = podcast.clone();
        updated.episodes[0].title = "Episode 1 - Revised".to_string();

        let result = db.update_podcast(pod_id, &updated).unwrap();
        assert_eq!(result.updated.len(), 1);
        assert!(result.added.is_empty());

        let eps = db.get_episodes(pod_id).unwrap();
        let revised = eps.iter().find(|e| e.guid == "guid-ep1").unwrap();
        assert_eq!(revised.title, "Episode 1 - Revised");
    }

    #[test]
    fn set_played_status() {
        let mut db = Database::connect_in_memory().unwrap();
        let podcast = sample_podcast();
        db.insert_podcast(&podcast).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        let eps = db.get_episodes(podcasts[0].id).unwrap();
        let ep_id = eps[0].id;

        assert!(!eps[0].played);
        assert_eq!(eps[0].position, 0);

        db.set_played_status(ep_id, 120, Some(3600), true).unwrap();

        let eps = db.get_episodes(podcasts[0].id).unwrap();
        let ep = eps.iter().find(|e| e.id == ep_id).unwrap();
        assert!(ep.played);
        assert_eq!(ep.position, 120);
        assert_eq!(ep.duration, Some(3600));
    }

    #[test]
    fn set_played_status_batch() {
        let mut db = Database::connect_in_memory().unwrap();
        let podcast = sample_podcast();
        db.insert_podcast(&podcast).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        let eps = db.get_episodes(podcasts[0].id).unwrap();

        let batch = vec![
            (eps[0].id, 60, Some(3600), true),
            (eps[1].id, 30, Some(1800), false),
        ];
        db.set_played_status_batch(batch).unwrap();

        let eps = db.get_episodes(podcasts[0].id).unwrap();
        let ep0 = eps.iter().find(|e| e.position == 60).unwrap();
        let ep1 = eps.iter().find(|e| e.position == 30).unwrap();
        assert!(ep0.played);
        assert!(!ep1.played);
    }

    #[test]
    fn insert_and_remove_file() {
        let mut db = Database::connect_in_memory().unwrap();
        let podcast = sample_podcast();
        db.insert_podcast(&podcast).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        let eps = db.get_episodes(podcasts[0].id).unwrap();
        let ep_id = eps[0].id;

        db.insert_file(ep_id, Path::new("/tmp/episode1.mp3"))
            .unwrap();

        let eps = db.get_episodes(podcasts[0].id).unwrap();
        let ep = eps.iter().find(|e| e.id == ep_id).unwrap();
        assert_eq!(ep.path.as_deref(), Some(Path::new("/tmp/episode1.mp3")));

        db.remove_file(ep_id).unwrap();

        let eps = db.get_episodes(podcasts[0].id).unwrap();
        let ep = eps.iter().find(|e| e.id == ep_id).unwrap();
        assert!(ep.path.is_none());
    }

    #[test]
    fn queue_set_get_and_ordering() {
        let mut db = Database::connect_in_memory().unwrap();
        let podcast = sample_podcast();
        db.insert_podcast(&podcast).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        let eps = db.get_episodes(podcasts[0].id).unwrap();

        let queue = vec![eps[1].id, eps[0].id];
        db.set_queue(queue.clone()).unwrap();

        let retrieved = db.get_queue().unwrap();
        assert_eq!(retrieved, queue);
    }

    #[test]
    fn queue_replace() {
        let mut db = Database::connect_in_memory().unwrap();
        let podcast = sample_podcast();
        db.insert_podcast(&podcast).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        let eps = db.get_episodes(podcasts[0].id).unwrap();

        db.set_queue(vec![eps[0].id, eps[1].id]).unwrap();
        db.set_queue(vec![eps[1].id]).unwrap();

        let retrieved = db.get_queue().unwrap();
        assert_eq!(retrieved, vec![eps[1].id]);
    }

    #[test]
    fn queue_empty() {
        let mut db = Database::connect_in_memory().unwrap();
        db.set_queue(Vec::new()).unwrap();
        assert!(db.get_queue().unwrap().is_empty());
    }

    #[test]
    fn clear_db() {
        let mut db = Database::connect_in_memory().unwrap();
        let podcast = sample_podcast();
        db.insert_podcast(&podcast).unwrap();

        db.clear_db().unwrap();
        assert!(db.get_podcasts().unwrap().is_empty());
    }

    #[test]
    fn episode_without_duration() {
        let mut db = Database::connect_in_memory().unwrap();
        let mut podcast = sample_podcast();
        podcast.episodes[0].duration = None;
        db.insert_podcast(&podcast).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        let eps = db.get_episodes(podcasts[0].id).unwrap();
        let ep = eps.iter().find(|e| e.title == "Episode 1").unwrap();
        assert!(ep.duration.is_none());
    }

    #[test]
    fn episode_without_pubdate() {
        let mut db = Database::connect_in_memory().unwrap();
        let mut podcast = sample_podcast();
        podcast.episodes[0].pubdate = None;
        db.insert_podcast(&podcast).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        let eps = db.get_episodes(podcasts[0].id).unwrap();
        assert_eq!(eps.len(), 2);
        assert!(eps.iter().any(|e| e.pubdate.is_none()));
    }

    #[test]
    fn multiple_podcasts() {
        let mut db = Database::connect_in_memory().unwrap();
        let pod1 = sample_podcast();
        let mut pod2 = sample_podcast();
        pod2.url = "https://example.com/feed2.xml".to_string();
        pod2.title = "Second Podcast".to_string();

        db.insert_podcast(&pod1).unwrap();
        db.insert_podcast(&pod2).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        assert_eq!(podcasts.len(), 2);
    }

    #[test]
    fn fallback_matching_without_guid() {
        let mut db = Database::connect_in_memory().unwrap();
        let mut podcast = sample_podcast();
        // Clear guids so fallback matching is used
        for ep in &mut podcast.episodes {
            ep.guid = String::new();
        }
        db.insert_podcast(&podcast).unwrap();

        let podcasts = db.get_podcasts().unwrap();
        let pod_id = podcasts[0].id;

        // Update with same episodes (still no guid) but changed description
        let mut updated = podcast.clone();
        updated.episodes[0].description = "Updated description".to_string();

        let result = db.update_podcast(pod_id, &updated).unwrap();
        // Should detect as update via title+url match, not insert new
        assert_eq!(result.updated.len(), 1);
        assert!(result.added.is_empty());
    }
}
