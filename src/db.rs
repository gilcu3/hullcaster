use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use ahash::AHashMap;
use rusqlite::{Connection, params};
use semver::Version;

use crate::types::*;
use crate::utils::convert_date;

pub struct SyncResult {
    pub added: Vec<NewEpisode>,
    pub updated: Vec<i64>,
}

/// Struct holding a sqlite database connection, with methods to interact
/// with this connection.
#[derive(Debug)]
pub struct Database {
    path: PathBuf,
    conn: Option<Connection>,
}

impl Database {
    /// Creates a new connection to the database (and creates database if
    /// it does not already exist). Panics if database cannot be accessed.
    pub fn connect(path: &Path) -> Result<Database> {
        let mut db_path = path.to_path_buf();
        std::fs::create_dir_all(&db_path)
            .with_context(|| "Unable to create subdirectory for database.")?;
        db_path.push("data.db");
        let conn = Connection::open(&db_path)?;
        let db_conn = Database {
            path: db_path,
            conn: Some(conn),
        };
        db_conn.create()?;

        {
            let conn = db_conn
                .conn
                .as_ref()
                .expect("Error connecting to database.");

            // SQLite defaults to foreign key support off
            conn.execute("PRAGMA foreign_keys=ON;", params![])
                .expect("Could not set database parameters.");

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
        let conn = self.conn.as_ref().expect("Error connecting to database.");
        let mut stmt = conn.prepare("SELECT value FROM params WHERE key = ?;")?;
        let param_str: String = stmt.query_row(rusqlite::params![key], |row| row.get(0))?;
        Ok(param_str)
    }

    pub fn set_param(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.as_ref().expect("Error connecting to database.");
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
        let conn = self.conn.as_ref().expect("Error connecting to database.");

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
    pub fn insert_podcast(&self, podcast: PodcastNoId) -> Result<SyncResult> {
        let mut conn = Connection::open(&self.path).expect("Error connecting to database.");
        let tx = conn.transaction()?;
        // let conn = self.conn.as_ref().expect("Error connecting to database.");
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
            let id = self.insert_episode(&tx, pod_id, ep)?;
            let new_ep = NewEpisode {
                id,
                pod_id,
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
        &self, conn: &Connection, podcast_id: i64, episode: &EpisodeNoId,
    ) -> Result<i64> {
        let pubdate = episode.pubdate.map(|dt| dt.timestamp());

        let mut stmt = conn.prepare_cached(
            "INSERT INTO episodes (podcast_id, title, url, guid,
                description, pubdate, duration, played, position)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?);",
        )?;
        stmt.execute(params![
            podcast_id,
            episode.title,
            episode.url,
            episode.guid,
            episode.description,
            pubdate,
            episode.duration,
            false,
            0,
        ])?;
        Ok(conn.last_insert_rowid())
    }

    /// Inserts a filepath to a downloaded episode.
    pub fn insert_file(&self, episode_id: i64, path: &Path) -> Result<()> {
        let conn = self.conn.as_ref().expect("Error connecting to database.");

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
        let conn = self.conn.as_ref().expect("Error connecting to database.");
        let mut stmt = conn.prepare_cached("DELETE FROM files WHERE episode_id = ?;")?;
        stmt.execute(params![episode_id])?;
        Ok(())
    }

    /// Removes all file listings for the selected episode ids.
    pub fn remove_files(&self, episode_ids: &[i64]) -> Result<()> {
        let conn = self.conn.as_ref().expect("Error connecting to database.");

        // convert list of episode ids into a comma-separated String
        let episode_list: Vec<String> = episode_ids.iter().map(|x| x.to_string()).collect();
        let episodes = episode_list.join(", ");

        let mut stmt = conn.prepare_cached("DELETE FROM files WHERE episode_id = (?);")?;
        stmt.execute(params![episodes])?;
        Ok(())
    }

    /// Removes a podcast, all episodes, and files from the database.
    pub fn remove_podcast(&self, podcast_id: i64) -> Result<()> {
        let conn = self.conn.as_ref().expect("Error connecting to database.");
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
    pub fn update_podcast(&self, pod_id: i64, podcast: PodcastNoId) -> Result<SyncResult> {
        {
            let conn = self.conn.as_ref().expect("Error connecting to database.");
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

        let result = self.update_episodes(pod_id, podcast.title, podcast.episodes)?;
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
        &self, podcast_id: i64, podcast_title: String, episodes: Vec<EpisodeNoId>,
    ) -> Result<SyncResult> {
        let old_episodes = self.get_episodes(podcast_id)?;
        let mut old_ep_map = AHashMap::new();
        for ep in old_episodes.iter() {
            if !ep.guid.is_empty() {
                old_ep_map.insert(ep.guid.clone(), ep);
            }
        }

        let mut conn = Connection::open(&self.path).expect("Error connecting to database.");
        let tx = conn.transaction()?;

        let mut insert_ep = Vec::new();
        let mut update_ep = Vec::new();
        for new_ep in episodes.iter().rev() {
            let new_pd = new_ep.pubdate.map(|dt| dt.timestamp());

            let mut existing_id = None;
            let mut update = false;

            // primary matching mechanism: check guid to see if it
            // already exists in database
            if !new_ep.guid.is_empty() {
                if let Some(old_ep) = old_ep_map.get(&new_ep.guid) {
                    existing_id = Some(old_ep.id);
                    update = self.check_for_updates(old_ep, new_ep);
                }
            }

            // fallback matching: for each existing episode, check the
            // title, url, and pubdate -- if two of the three match, we
            // count it as an existing episode; otherwise, we add it as
            // a new episode
            if existing_id.is_none() {
                for old_ep in old_episodes.iter().rev() {
                    let mut matching = 0;
                    matching += (new_ep.title == old_ep.title) as i32;
                    matching += (new_ep.url == old_ep.url) as i32;

                    if let Some(pd) = new_pd {
                        if let Some(old_pd) = old_ep.pubdate {
                            matching += (pd == old_pd.timestamp()) as i32;
                        }
                    }

                    if matching >= 2 {
                        existing_id = Some(old_ep.id);
                        update = self.check_for_updates(old_ep, new_ep);
                        break;
                    }
                }
            }

            match existing_id {
                Some(id) => {
                    if update {
                        let mut stmt = tx.prepare_cached(
                            "UPDATE episodes SET title = ?, url = ?,
                                guid = ?, description = ?, pubdate = ?,
                                duration = ? WHERE id = ?;",
                        )?;
                        stmt.execute(params![
                            new_ep.title,
                            new_ep.url,
                            new_ep.guid,
                            new_ep.description,
                            new_pd,
                            new_ep.duration,
                            id,
                        ])?;
                        update_ep.push(id);
                    }
                }
                None => {
                    let id = self.insert_episode(&tx, podcast_id, new_ep)?;
                    let new_ep = NewEpisode {
                        id,
                        pod_id: podcast_id,
                        title: new_ep.title.clone(),
                        pod_title: podcast_title.clone(),
                        selected: false,
                    };
                    insert_ep.push(new_ep);
                }
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
    fn check_for_updates(&self, old_ep: &Episode, new_ep: &EpisodeNoId) -> bool {
        let new_pd = new_ep.pubdate.map(|dt| dt.timestamp());
        let mut pd_match = false;
        if let Some(pd) = new_pd {
            if let Some(old_pd) = old_ep.pubdate {
                pd_match = pd == old_pd.timestamp();
            }
        }
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
        &self, episode_id: i64, position: i64, duration: Option<i64>, played: bool,
    ) -> Result<()> {
        let conn = self.conn.as_ref().expect("Error connecting to database.");

        let mut stmt = conn.prepare_cached(
            "UPDATE episodes SET played = ?, position = ?, duration = ? WHERE id = ?;",
        )?;
        stmt.execute(params![played, position, duration, episode_id])?;
        Ok(())
    }

    /// Updates an episode to mark it as played or unplayed.
    pub fn set_played_status_batch(
        &mut self, eps: Vec<(i64, i64, Option<i64>, bool)>,
    ) -> Result<()> {
        let conn = self.conn.as_mut().expect("Error connecting to database.");
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE episodes SET played = ?, position = ?, duration = ? WHERE id = ?;",
            )?;
            for (episode_id, position, duration, played) in eps {
                stmt.execute(params![played, position, duration, episode_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Generates list of all podcasts in database.
    /// TODO: This should probably use a JOIN statement instead.
    pub fn get_podcasts(&self) -> Result<Vec<Podcast>> {
        let conn = self.conn.as_ref().expect("Error connecting to database.");
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
        let conn = self.conn.as_ref().expect("Error connecting to database.");

        let mut stmt = conn.prepare_cached(
            "SELECT * FROM episodes
                    LEFT JOIN files ON episodes.id = files.episode_id
                    WHERE episodes.podcast_id = ?
                    ORDER BY pubdate DESC;",
        )?;
        let episode_iter = stmt.query_map(params![pod_id], |row| {
            let path = row.get::<&str, String>("path").ok().map(PathBuf::from);
            let pubdate = convert_date(row.get("pubdate")?).ok();
            Ok(Episode {
                id: row.get("id")?,
                pod_id: row.get("podcast_id")?,
                title: row.get("title")?,
                url: row.get("url")?,
                guid: row
                    .get::<&str, Option<String>>("guid")?
                    .unwrap_or_else(|| "".to_string()),
                description: row.get("description")?,
                pubdate,
                duration: row.get("duration")?,
                position: row.get("position")?,
                path,
                played: row.get("played")?,
            })
        })?;
        let episodes = episode_iter.flatten().collect();
        Ok(episodes)
    }

    /// Generates list of episodes for a given podcast.
    pub fn get_queue(&self) -> Result<Vec<i64>> {
        let conn = self.conn.as_ref().expect("Error connecting to database.");
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
        let conn = self.conn.as_mut().expect("Error connecting to database.");
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
        let conn = self.conn.as_ref().expect("Error connecting to database.");
        conn.execute("DELETE FROM files;", params![])?;
        conn.execute("DELETE FROM episodes;", params![])?;
        conn.execute("DELETE FROM podcasts;", params![])?;
        Ok(())
    }
}
