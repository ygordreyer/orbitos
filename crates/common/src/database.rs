use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate};
use log::{info, trace};
use rusqlite::{Connection, OptionalExtension, Row, params};
use rusqlite_migration::{M, Migrations};

use crate::constants::{ALLIUM_BASE_DIR, ALLIUM_DATABASE};

#[derive(Debug, Clone, Default)]
pub struct Database {
    conn: Option<Rc<Connection>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Game {
    pub name: String,
    pub path: PathBuf,
    pub image: Option<PathBuf>,
    pub play_count: i64,
    pub play_time: Duration,
    pub last_played: i64,
    pub core: Option<String>,
    pub rating: Option<u8>,
    pub release_date: Option<NaiveDate>,
    pub developer: Option<String>,
    pub publisher: Option<String>,
    pub genres: Vec<String>,
    pub favorite: bool,
    pub screenshot_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewGame {
    pub name: String,
    pub path: PathBuf,
    pub image: Option<PathBuf>,
    pub core: Option<String>,
    pub rating: Option<u8>,
    pub release_date: Option<NaiveDate>,
    pub developer: Option<String>,
    pub publisher: Option<String>,
    pub genres: Vec<String>,
    pub favorite: bool,
}

/// Fields needed by game lists without loading images, genres or other imported metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct GameListMetadata {
    pub core: Option<String>,
    pub play_count: i64,
    pub play_time: Duration,
    pub last_played: i64,
    pub rating: Option<u8>,
    pub release_date: Option<NaiveDate>,
    pub favorite: bool,
}

const DIRECTORY_QUERY: &str = "SELECT name, path, image, play_count, play_time, last_played, core, rating, release_date, developer, publisher, genres, favorite, screenshot_path FROM games WHERE path >= ?1 AND path < ?2 AND instr(substr(path, length(?1) + 1), '/') = 0";
const RECENTS_QUERY: &str = "SELECT name, path, image, play_count, play_time, last_played, core, rating, release_date, developer, publisher, genres, favorite, screenshot_path FROM games WHERE last_played > 0 ORDER BY last_played DESC LIMIT ?";

#[derive(Debug, Clone, PartialEq)]
pub struct GameSession {
    pub id: i64,
    pub game_name: String,
    pub game_path: PathBuf,
    pub start_time: i64,
    pub end_time: i64,
    pub duration: i64,
}

impl Database {
    pub fn new() -> Result<Self> {
        if !ALLIUM_DATABASE.exists() {
            let path = ALLIUM_BASE_DIR.join("state/allium.db");
            if path.exists() {
                info!("migrating database to new location");
                std::fs::copy(path, ALLIUM_DATABASE.as_path())?;
            }
        }

        let mut conn = Connection::open(ALLIUM_DATABASE.as_path())
            .with_context(|| format!("{}", ALLIUM_DATABASE.display()))?;
        Self::migrations().to_latest(&mut conn)?;
        Ok(Self {
            conn: Some(Rc::new(conn)),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        Self::migrations().to_latest(&mut conn)?;
        Ok(Self {
            conn: Some(Rc::new(conn)),
        })
    }

    /// A query-free revision shared by clones of this database connection.
    pub fn revision(&self) -> u64 {
        self.conn.as_ref().map_or(0, |conn| conn.total_changes())
    }

    pub fn migrations<'a>() -> Migrations<'a> {
        Migrations::new(vec![
        M::up("
CREATE TABLE IF NOT EXISTS games (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    image TEXT,
    play_count INTEGER NOT NULL,
    play_time INTEGER NOT NULL,
    last_played INTEGER NOT NULL
);"),
        M::up("
CREATE VIRTUAL TABLE games_fts USING fts5(name, path, content='games', content_rowid='id');

CREATE TRIGGER games_fts_ai AFTER INSERT ON games BEGIN
    INSERT INTO games_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;
CREATE TRIGGER games_fts_ad AFTER DELETE ON games BEGIN
    INSERT INTO games_fts(games_fts, rowid, name, path) VALUES ('delete', old.id, old.name, old.path);
END;
CREATE TRIGGER games_fts_au AFTER UPDATE ON games BEGIN
    INSERT INTO games_fts(games_fts, rowid, name, path) VALUES ('delete', old.id, old.name, old.path);
    INSERT INTO games_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;"),
        M::up("
CREATE TABLE IF NOT EXISTS guides (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    cursor INTEGER NOT NULL
);"),
        M::up("
CREATE TABLE IF NOT EXISTS key_value (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);"),
        M::up("
ALTER TABLE games ADD COLUMN core TEXT;
"),
        M::up("
CREATE TABLE IF NOT EXISTS directories (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    gamelist_fingerprint INTEGER
);"),
        M::up("
ALTER TABLE games ADD COLUMN rating INTEGER;
ALTER TABLE games ADD COLUMN release_date STRING;
"),
        M::up("
ALTER TABLE games ADD COLUMN developer STRING;
ALTER TABLE games ADD COLUMN publisher STRING;
ALTER TABLE games ADD COLUMN genres STRING NOT NULL DEFAULT '[]';
"),
        M::up("
CREATE TEMP TABLE games_fts_backup AS SELECT * FROM games_fts;
DROP TABLE IF EXISTS games_fts;
CREATE VIRTUAL TABLE games_fts USING fts5(name, path, developer, publisher, content='games', content_rowid='id');
INSERT INTO games_fts (rowid, name, path) SELECT rowid, name, path FROM games_fts_backup;

DROP TRIGGER IF EXISTS games_fts_ai;
CREATE TRIGGER games_fts_ai AFTER INSERT ON games BEGIN
    INSERT INTO games_fts(rowid, name, path, developer, publisher) VALUES (new.id, new.name, new.path, new.developer, new.publisher);
END;

DROP TRIGGER IF EXISTS games_fts_ad;
CREATE TRIGGER games_fts_ad AFTER DELETE ON games BEGIN
    INSERT INTO games_fts(games_fts, rowid, name, path, developer, publisher) VALUES ('delete', old.id, old.name, old.path, old.developer, old.publisher);
END;

DROP TRIGGER IF EXISTS games_fts_au;
CREATE TRIGGER games_fts_au AFTER UPDATE ON games BEGIN
    INSERT INTO games_fts(games_fts, rowid, name, path, developer, publisher) VALUES ('delete', old.id, old.name, old.path, old.developer, old.publisher);
    INSERT INTO games_fts(rowid, name, path, developer, publisher) VALUES (new.id, new.name, new.path, new.developer, new.publisher);
END;"),
        M::up("
ALTER TABLE games ADD COLUMN favorite INTEGER NOT NULL DEFAULT 0;
"),
        M::up("
ALTER TABLE games ADD COLUMN screenshot_path TEXT;
"),
        M::up("
CREATE TABLE IF NOT EXISTS game_sessions (
    id INTEGER PRIMARY KEY,
    game_path TEXT NOT NULL,
    start_time INTEGER NOT NULL,
    end_time INTEGER NOT NULL,
    duration INTEGER NOT NULL,
    FOREIGN KEY (game_path) REFERENCES games(path)
);
CREATE INDEX IF NOT EXISTS idx_game_sessions_start_time ON game_sessions(start_time DESC);
"),
        M::up("
DROP TRIGGER games_fts_au;
CREATE TRIGGER games_fts_au AFTER UPDATE OF name, path, developer, publisher ON games
WHEN old.name IS NOT new.name OR old.path IS NOT new.path
  OR old.developer IS NOT new.developer OR old.publisher IS NOT new.publisher
BEGIN
    INSERT INTO games_fts(games_fts, rowid, name, path, developer, publisher) VALUES ('delete', old.id, old.name, old.path, old.developer, old.publisher);
    INSERT INTO games_fts(rowid, name, path, developer, publisher) VALUES (new.id, new.name, new.path, new.developer, new.publisher);
END;
CREATE INDEX idx_games_last_played ON games(last_played);
"),
                ])
    }

    pub fn reset_game(&self, path: &Path) -> Result<()> {
        self.conn
            .as_ref()
            .unwrap()
            .prepare_cached(
                "UPDATE games SET play_count = 0, play_time = 0, last_played = 0 WHERE path = ?",
            )?
            .execute(params![path.display().to_string()])?;
        Ok(())
    }

    pub fn update_screenshot_path(
        &self,
        game_path: &Path,
        screenshot_path: Option<&Path>,
    ) -> Result<()> {
        self.conn
            .as_ref()
            .unwrap()
            .prepare_cached("UPDATE games SET screenshot_path = ? WHERE path = ?")?
            .execute(params![
                screenshot_path.map(|p| p.display().to_string()),
                game_path.display().to_string()
            ],
        )?;
        Ok(())
    }

    pub fn update_game_path(&self, old: &Path, new: &Path) -> Result<()> {
        let conn = self.conn.as_ref().unwrap();
        let tx = conn.unchecked_transaction()?;
        // The parent path and its sessions must move together before checking foreign keys.
        tx.pragma_update(None, "defer_foreign_keys", true)?;

        let result = tx
            .prepare_cached("UPDATE games SET path = ? WHERE path = ?")?
            .execute(params![
                new.display().to_string(),
                old.display().to_string()
            ]);

        // Handle UNIQUE constraint violation by merging rows
        if let Err(rusqlite::Error::SqliteFailure(err, _)) = result
            && err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
        {
            let (conflicting_play_count, conflicting_play_time, conflicting_favorite): (
                i64,
                i64,
                i64,
            ) = tx
                .prepare_cached("SELECT play_count, play_time, favorite FROM games WHERE path = ?")?
                .query_row([new.display().to_string()], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;

            tx.prepare_cached("UPDATE games SET play_count = play_count + ?, play_time = play_time + ?, favorite = MAX(favorite, ?) WHERE path = ?")?.execute(
                    params![
                        conflicting_play_count,
                        conflicting_play_time,
                        conflicting_favorite,
                        old.display().to_string()
                    ],
                )?;

            // Update game_sessions to point to old path before deleting conflicting row
            tx.prepare_cached("UPDATE game_sessions SET game_path = ? WHERE game_path = ?")?
                .execute(params![
                    old.display().to_string(),
                    new.display().to_string()
                ])?;

            tx.prepare_cached("DELETE FROM games WHERE path = ?")?
                .execute([new.display().to_string()])?;

            tx.prepare_cached("UPDATE games SET path = ? WHERE path = ?")?
                .execute(params![
                    new.display().to_string(),
                    old.display().to_string()
                ])?;
        } else {
            result?;
        }

        tx.prepare_cached("UPDATE game_sessions SET game_path = ? WHERE game_path = ?")?
            .execute(params![
                new.display().to_string(),
                old.display().to_string()
            ])?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_games(&self, games: &[NewGame]) -> Result<()> {
        let tx = self.conn.as_ref().unwrap().unchecked_transaction()?; // safe because single-threaded

        let mut stmt = tx.prepare_cached(
            "
INSERT INTO games (name, path, image, play_count, play_time, last_played, core, rating, release_date, developer, publisher, genres)
VALUES (?, ?, ?, 0, 0, 0, ?, ?, ?, ?, ?, ?)
ON CONFLICT(path) DO UPDATE SET name = ?, image = ?, core = ?, rating = ?, release_date = ?, developer = ?, publisher = ?, genres = ?",
        )?;

        for game in games {
            let path = game.path.display().to_string();
            let image = game.image.as_ref().map(|p| p.display().to_string());
            let genres = serde_json::to_string(&game.genres)?;
            stmt.execute(params![
                game.name,
                path,
                image,
                game.core,
                game.rating,
                game.release_date,
                game.developer,
                game.publisher,
                genres,
                game.name,
                image,
                game.core,
                game.rating,
                game.release_date,
                game.developer,
                game.publisher,
                genres,
            ])?;
        }

        drop(stmt);

        tx.commit()?;

        Ok(())
    }

    /// Refresh imported metadata without changing core, favorites, history or screenshots.
    pub fn update_game_metadata(&self, games: &[NewGame]) -> Result<()> {
        let tx = self.conn.as_ref().unwrap().unchecked_transaction()?;
        {
            let mut stmt = tx.prepare_cached("
INSERT INTO games (name, path, image, play_count, play_time, last_played, rating, release_date, developer, publisher, genres)
VALUES (?, ?, ?, 0, 0, 0, ?, ?, ?, ?, ?)
ON CONFLICT(path) DO UPDATE SET name = excluded.name, image = excluded.image,
rating = excluded.rating, release_date = excluded.release_date, developer = excluded.developer,
publisher = excluded.publisher, genres = excluded.genres")?;
            for game in games {
                stmt.execute(params![game.name, game.path.display().to_string(),
                    game.image.as_ref().map(|p| p.display().to_string()), game.rating,
                    game.release_date, game.developer, game.publisher,
                    serde_json::to_string(&game.genres)?])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Selects played games sorted by most play time first.
    pub fn select_most_played(&self, limit: i64) -> Result<Vec<Game>> {
        let mut stmt = self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("SELECT name, path, image, play_count, play_time, last_played, core, rating, release_date, developer, publisher, genres, favorite, screenshot_path FROM games WHERE last_played > 0 ORDER BY play_time DESC LIMIT ?")?;

        let results = stmt
            .query_map([limit], map_game)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Selects played games sorted by last played first.
    pub fn select_last_played(&self, limit: i64) -> Result<Vec<Game>> {
        let mut stmt = self.conn.as_ref().unwrap().prepare_cached(RECENTS_QUERY)?;

        let results = stmt
            .query_map([limit], map_game)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Selects played games sorted by highest rating first.
    pub fn select_by_rating(&self, limit: i64) -> Result<Vec<Game>> {
        let mut stmt = self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("SELECT name, path, image, play_count, play_time, last_played, core, rating, release_date, developer, publisher, genres, favorite, screenshot_path FROM games ORDER BY rating DESC LIMIT ?")?;

        let results = stmt
            .query_map([limit], map_game)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Selects played games sorted by release date (earliest first).
    pub fn select_by_release_date(&self, limit: i64) -> Result<Vec<Game>> {
        let mut stmt = self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("SELECT name, path, image, play_count, play_time, last_played, core, rating, release_date, developer, publisher, genres, favorite, screenshot_path FROM games ORDER BY release_date DESC LIMIT ?")?;

        let results = stmt
            .query_map([limit], map_game)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Selects random games.
    pub fn select_random(&self, limit: i64) -> Result<Vec<Game>> {
        let mut stmt = self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("SELECT name, path, image, play_count, play_time, last_played, core, rating, release_date, developer, publisher, genres, favorite, screenshot_path FROM games WHERE id IN (SELECT id FROM games ORDER BY RANDOM() LIMIT ?)")?;

        let results = stmt
            .query_map([limit], map_game)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Selects favorite games.
    pub fn select_favorites(&self, limit: i64) -> Result<Vec<Game>> {
        let mut stmt = self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("SELECT name, path, image, play_count, play_time, last_played, core, rating, release_date, developer, publisher, genres, favorite, screenshot_path FROM games WHERE favorite = 1 ORDER BY last_played DESC LIMIT ?")?;

        let results = stmt
            .query_map([limit], map_game)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Search for games by name. The query is a prefix search on words, so "Fi" will match both "Fire Emblem" and "Pokemon Fire Red".
    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Game>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn.as_ref().unwrap();

        let mut stmt = conn.prepare_cached("SELECT games.name, games.path, image, play_count, play_time, last_played, core, rating, release_date, games.developer, games.publisher, genres, favorite, screenshot_path FROM games JOIN games_fts ON games.id = games_fts.rowid WHERE games_fts MATCH ?1 ORDER BY games_fts.rowid NOT IN (SELECT rowid FROM games_fts WHERE games_fts MATCH ?2), games_fts.rowid LIMIT ?3")?;

        // Title matches come first so developer or publisher matches never push them past the limit.
        let name_query = format!("name:\"{query}\" *");
        let query = format!("{name_query} OR developer:\"{query}\" * OR publisher:\"{query}\" *");
        let results = stmt
            .query_map(params![query, name_query, limit], map_game)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    pub fn select_games_in_directory(&self, path: &Path) -> Result<Vec<Game>> {
        trace!("select_games_in_directory({:?})", path);
        let conn = self.conn.as_ref().unwrap();

        // Binary bounds keep wildcard characters literal and use the UNIQUE path index.
        let (lower, upper) = directory_bounds(path);
        let mut stmt = conn.prepare_cached(DIRECTORY_QUERY)?;
        let results = stmt
            .query_map(params![lower, upper], map_game)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(results)
    }

    pub fn select_game(&self, path: &Path) -> Result<Option<Game>> {
        let game = self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("SELECT name, path, image, play_count, play_time, last_played, core, rating, release_date, developer, publisher, genres, favorite, screenshot_path FROM games WHERE path = ? LIMIT 1")?.query_row( [path.display().to_string()], map_game)
            .optional()?;

        Ok(game)
    }

    /// Results follow input order, including duplicates and missing paths.
    pub fn select_games(&self, paths: &[&Path]) -> Result<Vec<Option<Game>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.as_ref().unwrap().prepare_cached(
            "SELECT games.name, games.path, image, play_count, play_time, last_played, core, rating, release_date, developer, publisher, genres, favorite, screenshot_path, CAST(requested.key AS INTEGER) FROM json_each(?) AS requested JOIN games ON games.path = requested.value",
        )?;
        let mut results = vec![None; paths.len()];
        let rows = stmt.query_map([paths_json(paths)?], |row| {
            Ok((row.get::<_, i64>(14)? as usize, map_game(row)?))
        })?;
        for row in rows {
            let (index, game) = row?;
            results[index] = Some(game);
        }
        Ok(results)
    }

    /// Fetch cores and sort keys in one indexed join, without SQLite's parameter limit.
    /// Results follow input order, including duplicates and missing paths.
    pub fn select_game_list_metadata(
        &self,
        paths: &[&Path],
    ) -> Result<Vec<Option<GameListMetadata>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.as_ref().unwrap().prepare_cached(
            "SELECT CAST(requested.key AS INTEGER), core, play_count, play_time, last_played, rating, release_date, favorite FROM json_each(?) AS requested JOIN games ON games.path = requested.value",
        )?;
        let mut results = vec![None; paths.len()];
        let rows = stmt.query_map([paths_json(paths)?], |row| {
            Ok((
                row.get::<_, i64>(0)? as usize,
                GameListMetadata {
                    core: row.get(1)?,
                    play_count: row.get(2)?,
                    play_time: Duration::seconds(row.get(3)?),
                    last_played: row.get(4)?,
                    rating: row.get(5)?,
                    release_date: row.get(6)?,
                    favorite: row.get::<_, i64>(7)? != 0,
                },
            ))
        })?;
        for row in rows {
            let (index, metadata) = row?;
            results[index] = Some(metadata);
        }

        Ok(results)
    }

    pub fn select_all_games(&self) -> Result<Vec<Game>> {
        let mut stmt = self.conn.as_ref().unwrap().prepare_cached(
            "SELECT name, path, image, play_count, play_time, last_played, core, rating, release_date, developer, publisher, genres, favorite, screenshot_path FROM games",
        )?;

        let results = stmt
            .query_map([], map_game)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Increment the play count of a game, inserting a new row if it doesn't exist.
    pub fn increment_play_count(&self, game: &NewGame) -> Result<()> {
        self.conn.as_ref().unwrap().prepare_cached("
INSERT INTO games (name, path, image, play_count, play_time, last_played, core, rating, release_date)
VALUES (?, ?, ?, 1, 0, (SELECT COALESCE(MAX(last_played), 0) + 1 FROM games), ?, ?, ?)
ON CONFLICT(path) DO UPDATE SET play_count = games.play_count + 1, last_played = excluded.last_played;")?.execute(
            params![
                game.name,
                game.path.display().to_string(),
                game.image.as_ref().map(|p| p.display().to_string()),
                game.core,
                game.rating,
                game.release_date
            ],
        )?;

        Ok(())
    }

    /// Increases the play time of a game. Does nothing if the game doesn't exist.
    pub fn add_play_time(&self, path: &Path, play_time: Duration) -> Result<()> {
        add_play_time_on(self.conn.as_ref().unwrap(), path, play_time.num_seconds())?;

        Ok(())
    }

    /// Sets whether a game is a favorite, inserting a new row if it doesn't exist.
    pub fn set_favorite(&self, game: &NewGame) -> Result<()> {
        // If the game isn't in db it has never been played
        self.conn.as_ref().unwrap().prepare_cached("INSERT INTO games (name, path, image, play_count, play_time, last_played, core, rating, release_date, favorite)
             VALUES (?, ?, ?, 0, 0, 0, ?, ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET favorite = excluded.favorite")?.execute(
            params![
                game.name,
                game.path.display().to_string(),
                game.image.as_ref().map(|p| p.display().to_string()),
                game.core,
                game.rating,
                game.release_date,
                if game.favorite { 1 } else { 0 }
            ],
        )?;
        Ok(())
    }

    pub fn get_guide_cursor(&self, path: &Path) -> Result<i64> {
        let cursor = self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("SELECT cursor FROM guides WHERE path = ?")?
            .query_row([path.display().to_string()], |row| row.get(0))
            .optional()?;

        Ok(cursor.unwrap_or(0))
    }

    pub fn update_guide_cursor(&self, path: &Path, cursor: i64) -> Result<()> {
        self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("INSERT INTO guides (path, cursor) VALUES (?, ?) ON CONFLICT(path) DO UPDATE SET cursor = ?")?.execute( params![path.display().to_string(), cursor, cursor])?;

        Ok(())
    }

    /// Deletes a game and its sessions from the database.
    pub fn delete_game(&self, path: &Path) -> Result<()> {
        let conn = self.conn.as_ref().unwrap();
        let path = path.display().to_string();
        let tx = conn.unchecked_transaction()?;
        tx.prepare_cached("DELETE FROM game_sessions WHERE game_path = ?")?
            .execute([&path])?;
        tx.prepare_cached("DELETE FROM games WHERE path = ?")?
            .execute([&path])?;
        tx.commit()?;

        Ok(())
    }

    /// Deletes unplayed games without session history.
    pub fn delete_all_unplayed_games(&self) -> Result<()> {
        self.conn
            .as_ref()
            .unwrap()
            .prepare_cached("DELETE FROM games WHERE last_played = 0 AND play_time = 0 AND NOT EXISTS (SELECT 1 FROM game_sessions WHERE game_path = games.path)")?
            .execute([])?;

        Ok(())
    }

    /// Deletes all directories
    pub fn delete_all_directories(&self) -> Result<()> {
        self.conn
            .as_ref()
            .unwrap()
            .prepare_cached("DELETE FROM directories")?
            .execute([])?;

        Ok(())
    }

    pub fn set_has_indexed(&self, has_indexed: bool) -> Result<()> {
        self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("INSERT INTO key_value (key, value) VALUES ('has_indexed', 1) ON CONFLICT(key) DO UPDATE SET value = ?")?.execute( [if has_indexed { "1" } else {"0"}])?;

        Ok(())
    }

    pub fn has_indexed(&self) -> Result<bool> {
        let value = self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("SELECT value FROM key_value WHERE key = 'has_indexed'")?
            .query_row([], |row| row.get::<_, String>(0))
            .optional()?;

        Ok(matches!(value.as_deref(), Some("1")))
    }

    pub fn invalidate_gamelists(&self) -> Result<()> {
        self.conn
            .as_ref()
            .unwrap()
            .prepare_cached("UPDATE directories SET gamelist_fingerprint = NULL")?
            .execute([])?;
        Ok(())
    }

    pub fn set_gamelist_fingerprint(&self, path: &Path, fingerprint: i64) -> Result<()> {
        trace!("set_gamelist_fingerprint({:?}, {})", path, fingerprint);
        self.conn.as_ref().unwrap().prepare_cached("INSERT INTO directories (path, gamelist_fingerprint) VALUES (?, ?) ON CONFLICT(path) DO UPDATE SET gamelist_fingerprint = ?")?.execute(
            params![path.display().to_string(), fingerprint, fingerprint],
        )?;

        Ok(())
    }

    pub fn get_gamelist_fingerprint(&self, path: &Path) -> Result<Option<i64>> {
        trace!("get_gamelist_fingerprint({:?})", path);
        let fingerprint = self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("SELECT gamelist_fingerprint FROM directories WHERE path = ?")?
            .query_row([path.display().to_string()], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .optional()?
            .flatten();

        Ok(fingerprint)
    }

    pub fn get_core(&self, path: &Path) -> Result<Option<String>> {
        let core = self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("SELECT core FROM games WHERE path = ?")?
            .query_row([path.display().to_string()], |row| {
                row.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten();

        Ok(core)
    }

    pub fn set_core(&self, path: &Path, core: &str) -> Result<()> {
        self.conn
            .as_ref()
            .unwrap()
            .prepare_cached("UPDATE games SET core = ? WHERE path = ?")?
            .execute(params![core, path.display().to_string()])?;

        Ok(())
    }

    /// Inserts a new game session record.
    pub fn insert_game_session(
        &self,
        path: &Path,
        start_time: i64,
        end_time: i64,
        duration: i64,
    ) -> Result<()> {
        insert_game_session_on(
            self.conn.as_ref().unwrap(),
            path,
            start_time,
            end_time,
            duration,
        )?;
        Ok(())
    }

    /// Add play time and its session in one transaction. A missing game is an error.
    /// Use instead of separate add_play_time/insert_game_session calls on emulator return.
    pub fn record_game_session(
        &self,
        path: &Path,
        start_time: i64,
        end_time: i64,
        duration: i64,
    ) -> Result<()> {
        let tx = self.conn.as_ref().unwrap().unchecked_transaction()?;
        let changed = add_play_time_on(&tx, path, duration)?;
        anyhow::ensure!(
            changed == 1,
            "cannot record session for missing game: {}",
            path.display()
        );
        let inserted = insert_game_session_on(&tx, path, start_time, end_time, duration)?;
        anyhow::ensure!(inserted == 1, "session insert was ignored");
        tx.commit()?;
        Ok(())
    }

    /// Selects game sessions ordered by start time (most recent first), with limit and offset for pagination.
    pub fn select_sessions_by_day(&self, limit: i64, offset: i64) -> Result<Vec<GameSession>> {
        let mut stmt = self
            .conn
            .as_ref()
            .unwrap()
            .prepare_cached("SELECT game_sessions.id, games.name, game_sessions.game_path, game_sessions.start_time, game_sessions.end_time, game_sessions.duration FROM game_sessions JOIN games ON game_sessions.game_path = games.path ORDER BY game_sessions.start_time DESC LIMIT ? OFFSET ?")?;

        let results = stmt
            .query_map(params![limit, offset], |row| {
                Ok(GameSession {
                    id: row.get(0)?,
                    game_name: row.get(1)?,
                    game_path: PathBuf::from(row.get::<_, String>(2)?),
                    start_time: row.get(3)?,
                    end_time: row.get(4)?,
                    duration: row.get(5)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }
}

fn directory_bounds(path: &Path) -> (String, String) {
    let directory = path.display().to_string();
    let directory = directory.trim_end_matches('/');
    // '/' and '0' are adjacent in BINARY collation, bounding every descendant.
    (format!("{directory}/"), format!("{directory}0"))
}

fn paths_json(paths: &[&Path]) -> Result<String> {
    let mut json = Vec::new();
    serde::Serializer::collect_seq(
        &mut serde_json::Serializer::new(&mut json),
        paths.iter().map(|p| p.to_string_lossy()),
    )?;
    Ok(String::from_utf8(json)?)
}

fn add_play_time_on(conn: &Connection, path: &Path, seconds: i64) -> rusqlite::Result<usize> {
    conn.prepare_cached("UPDATE games SET play_time = play_time + ? WHERE path = ?")?
        .execute(params![seconds, path.display().to_string()])
}

fn insert_game_session_on(
    conn: &Connection,
    path: &Path,
    start_time: i64,
    end_time: i64,
    duration: i64,
) -> rusqlite::Result<usize> {
    conn.prepare_cached(
        "INSERT INTO game_sessions (game_path, start_time, end_time, duration) VALUES (?, ?, ?, ?)",
    )?
    .execute(params![
        path.display().to_string(),
        start_time,
        end_time,
        duration
    ])
}

fn map_game(row: &Row<'_>) -> rusqlite::Result<Game> {
    Ok(Game {
        name: row.get(0)?,
        path: PathBuf::from(row.get::<_, String>(1)?),
        image: row.get::<_, Option<String>>(2)?.map(PathBuf::from),
        play_count: row.get(3)?,
        play_time: Duration::seconds(row.get(4)?),
        last_played: row.get(5)?,
        core: row.get(6)?,
        rating: row.get(7)?,
        release_date: row.get(8)?,
        developer: row.get(9)?,
        publisher: row.get(10)?,
        genres: serde_json::from_str(&row.get::<_, String>(11)?).unwrap(),
        favorite: row.get::<_, i64>(12)? != 0,
        screenshot_path: row.get::<_, Option<String>>(13)?.map(PathBuf::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_game(path: &str) -> NewGame {
        NewGame {
            name: "Original".to_owned(),
            path: PathBuf::from(path),
            image: None,
            core: None,
            rating: None,
            release_date: None,
            developer: None,
            publisher: None,
            genres: Vec::new(),
            favorite: false,
        }
    }

    #[test]
    fn rebuild_preserves_reset_game_history() -> Result<()> {
        let db = Database::in_memory()?;
        db.conn
            .as_ref()
            .unwrap()
            .execute_batch("PRAGMA foreign_keys = ON;")?;
        let game = new_game("roms/reset.rom");
        let unplayed = new_game("roms/unplayed.rom");
        db.increment_play_count(&game)?;
        db.record_game_session(&game.path, 10, 70, 60)?;
        db.update_games(&[unplayed.clone()])?;
        let sessions = db.select_sessions_by_day(10, 0)?;
        db.reset_game(&game.path)?;
        let reset = db.select_game(&game.path)?.unwrap();
        assert_eq!(
            (reset.play_count, reset.play_time, reset.last_played),
            (0, Duration::zero(), 0)
        );

        for _ in 0..2 {
            db.delete_all_directories()?;
            db.delete_all_unplayed_games()?;
            assert!(db.select_game(&unplayed.path)?.is_none());
            assert_eq!(db.select_game(&game.path)?, Some(reset.clone()));
            db.update_games(&[game.clone()])?;
            assert_eq!(db.select_sessions_by_day(10, 0)?, sessions);
        }
        Ok(())
    }

    #[test]
    fn rebuild_moves_sessions_to_free_game_path() -> Result<()> {
        let db = Database::in_memory()?;
        db.conn
            .as_ref()
            .unwrap()
            .execute_batch("PRAGMA foreign_keys = ON;")?;
        let game = new_game("roms/old.rom");
        let new = Path::new("roms/new.rom");
        db.increment_play_count(&game)?;
        db.record_game_session(&game.path, 10, 70, 60)?;
        let mut expected_game = db.select_game(&game.path)?.unwrap();
        let mut expected_sessions = db.select_sessions_by_day(10, 0)?;
        expected_game.path = new.to_path_buf();
        expected_sessions[0].game_path = new.to_path_buf();

        db.update_game_path(&game.path, new)?;
        assert!(db.select_game(&game.path)?.is_none());
        assert_eq!(db.select_game(new)?, Some(expected_game));
        assert_eq!(db.select_sessions_by_day(10, 0)?, expected_sessions);
        Ok(())
    }

    #[test]
    fn rebuild_merges_game_paths_with_sessions_on_both() -> Result<()> {
        let db = Database::in_memory()?;
        db.conn
            .as_ref()
            .unwrap()
            .execute_batch("PRAGMA foreign_keys = ON;")?;
        let old = new_game("roms/old.rom");
        let mut new = new_game("roms/new.rom");
        new.favorite = true;
        db.increment_play_count(&old)?;
        db.increment_play_count(&new)?;
        db.set_favorite(&new)?;
        db.record_game_session(&old.path, 10, 70, 60)?;
        db.record_game_session(&new.path, 80, 110, 30)?;
        let mut expected_sessions = db.select_sessions_by_day(10, 0)?;
        for session in &mut expected_sessions {
            session.game_path = new.path.clone();
        }

        db.update_game_path(&old.path, &new.path)?;
        assert!(db.select_game(&old.path)?.is_none());
        let merged = db.select_game(&new.path)?.unwrap();
        assert_eq!(
            (merged.play_count, merged.play_time),
            (2, Duration::seconds(90))
        );
        assert!(merged.favorite);
        assert_eq!(db.select_all_games()?.len(), 1);
        assert_eq!(db.select_sessions_by_day(10, 0)?, expected_sessions);
        Ok(())
    }

    #[test]
    fn rebuild_path_update_rolls_back_when_session_move_fails() -> Result<()> {
        for conflict in [false, true] {
            let db = Database::in_memory()?;
            let conn = db.conn.as_ref().unwrap();
            conn.execute_batch("PRAGMA foreign_keys = ON;")?;
            let old = new_game("roms/old.rom");
            let new = new_game("roms/new.rom");
            db.increment_play_count(&old)?;
            db.record_game_session(&old.path, 10, 70, 60)?;
            if conflict {
                db.increment_play_count(&new)?;
                db.record_game_session(&new.path, 80, 110, 30)?;
            }
            let games = db.select_all_games()?;
            let sessions = db.select_sessions_by_day(10, 0)?;
            conn.execute_batch("CREATE TRIGGER reject_session_move BEFORE UPDATE OF game_path ON game_sessions BEGIN SELECT RAISE(ABORT, 'session move rejected'); END;")?;

            let error = db.update_game_path(&old.path, &new.path).unwrap_err();
            assert!(error.to_string().contains("session move rejected"), "{error}");
            assert_eq!(db.select_all_games()?, games);
            assert_eq!(db.select_sessions_by_day(10, 0)?, sessions);
            assert!(conn.is_autocommit());
            assert_eq!(
                conn.query_row("PRAGMA defer_foreign_keys", [], |r| r.get::<_, i64>(0))?,
                0
            );
        }
        Ok(())
    }

    #[test]
    fn populated_previous_schema_migrates_without_losing_data() -> Result<()> {
        let mut conn = Connection::open_in_memory()?;
        Database::migrations().to_version(&mut conn, 12)?;
        conn.execute_batch("
INSERT INTO games (name, path, play_count, play_time, last_played, developer, publisher, favorite, core, screenshot_path)
VALUES ('Original', 'roms/original.rom', 3, 120, 7, 'Developer', 'Publisher', 1, 'core', 'shot.png');
INSERT INTO game_sessions (game_path, start_time, end_time, duration) VALUES ('roms/original.rom', 1, 121, 120);
INSERT INTO guides (path, cursor) VALUES ('guide.txt', 42);
INSERT INTO directories (path, gamelist_fingerprint) VALUES ('roms', 123);
INSERT INTO key_value (key, value) VALUES ('has_indexed', '1');")?;
        let before = conn.total_changes();
        conn.execute("UPDATE games SET play_count = play_count + 1", [])?;
        let old_history_changes = conn.total_changes() - before;
        assert!(old_history_changes > 1);
        Database::migrations().to_latest(&mut conn)?;
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
            13
        );
        let db = Database {
            conn: Some(Rc::new(conn)),
        };
        let path = Path::new("roms/original.rom");
        let game = db.select_game(path)?.unwrap();
        assert_eq!(
            (game.play_count, game.play_time, game.last_played),
            (4, Duration::seconds(120), 7)
        );
        assert!(game.favorite);
        assert_eq!(game.core.as_deref(), Some("core"));
        assert_eq!(game.screenshot_path, Some(PathBuf::from("shot.png")));
        assert_eq!(db.select_sessions_by_day(10, 0)?.len(), 1);
        assert_eq!(db.get_guide_cursor(Path::new("guide.txt"))?, 42);
        assert_eq!(db.get_gamelist_fingerprint(Path::new("roms"))?, Some(123));
        assert!(db.has_indexed()?);
        for term in ["Original", "Developer", "Publisher"] {
            assert_eq!(db.search(term, 10)?.len(), 1);
        }
        let conn = db.conn.as_ref().unwrap();
        let before = conn.total_changes();
        db.increment_play_count(&new_game("roms/original.rom"))?;
        assert_eq!(conn.total_changes() - before, 1);
        println!("history total_changes previous={old_history_changes} migrated=1");
        Ok(())
    }

    #[test]
    fn fts_tracks_only_actual_indexed_value_changes() -> Result<()> {
        let db = Database::in_memory()?;
        let conn = db.conn.as_ref().unwrap();
        let mut game = new_game("roms/original.rom");
        db.update_games(&[game.clone()])?;
        let before = conn.total_changes();
        db.update_games(&[game.clone()])?;
        db.update_game_metadata(&[game.clone()])?;
        db.increment_play_count(&game)?;
        db.add_play_time(&game.path, Duration::seconds(30))?;
        db.set_core(&game.path, "core")?;
        db.set_favorite(&game)?;
        db.update_screenshot_path(&game.path, Some(Path::new("shot.png")))?;
        db.reset_game(&game.path)?;
        assert_eq!(conn.total_changes() - before, 8);
        assert_eq!(db.search("Original", 10)?.len(), 1);

        game.name = "Renamed".to_owned();
        game.developer = Some("Newdeveloper".to_owned());
        game.publisher = Some("Newpublisher".to_owned());
        db.update_game_metadata(&[game.clone()])?;
        assert!(db.search("Original", 10)?.is_empty());
        for term in ["Renamed", "Newdeveloper", "Newpublisher"] {
            assert_eq!(db.search(term, 10)?.len(), 1);
        }
        let renamed_path = Path::new("roms/moved.rom");
        db.update_game_path(&game.path, renamed_path)?;
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM games_fts WHERE games_fts MATCH 'path:moved'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM games_fts WHERE games_fts MATCH 'path:original'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        game.path = renamed_path.to_path_buf();
        game.developer = None;
        game.publisher = None;
        db.update_game_metadata(&[game])?;
        assert!(db.search("Newdeveloper", 10)?.is_empty());
        assert!(db.search("Newpublisher", 10)?.is_empty());
        assert_eq!(db.search("Renamed", 10)?.len(), 1);
        Ok(())
    }

    #[test]
    fn directory_bounds_treat_wildcards_literally_and_exclude_descendants() -> Result<()> {
        let db = Database::in_memory()?;
        let paths = [
            "roms/100%_fun/child.rom",
            "roms/100%_fun/nested/child.rom",
            "roms/100ZZfun/other.rom",
            "roms/100%Xfun/other.rom",
            "roms/100%_fun-extra/other.rom",
            "roms/100%_fun0/other.rom",
            "roms/100%_fun/100%_name.rom",
            "roms/é_%/日本.rom",
            "roms/é_%/sub/nested.rom",
            "/root.rom",
            "/nested/child.rom",
            "roms/back\\slash_%/child.rom",
            "roms/Case/one.rom",
            "roms/case/two.rom",
        ];
        db.update_games(&paths.iter().map(|p| new_game(p)).collect::<Vec<_>>())?;
        for directory in ["roms/100%_fun", "roms/100%_fun/"] {
            let rows = db.select_games_in_directory(Path::new(directory))?;
            let paths = rows.iter().map(|g| g.path.as_path()).collect::<Vec<_>>();
            assert_eq!(
                paths,
                [
                    Path::new("roms/100%_fun/100%_name.rom"),
                    Path::new("roms/100%_fun/child.rom")
                ]
            );
        }
        assert_eq!(
            db.select_games_in_directory(Path::new("roms/é_%"))?[0].path,
            Path::new("roms/é_%/日本.rom")
        );
        assert_eq!(
            db.select_games_in_directory(Path::new("roms/back\\slash_%"))?
                .len(),
            1
        );
        assert_eq!(
            db.select_games_in_directory(Path::new("/"))?
                .iter()
                .map(|g| g.path.clone())
                .collect::<Vec<_>>(),
            [PathBuf::from("/root.rom")]
        );
        assert_eq!(
            db.select_games_in_directory(Path::new("roms/Case"))?.len(),
            1
        );
        Ok(())
    }

    #[test]
    fn launch_upsert_and_session_recording_are_atomic() -> Result<()> {
        let db = Database::in_memory()?;
        let conn = db.conn.as_ref().unwrap();
        let game = new_game("roms/game.rom");
        db.increment_play_count(&game)?;
        let original = db.select_game(&game.path)?.unwrap();
        assert_eq!((original.play_count, original.last_played), (1, 1));
        conn.execute_batch("CREATE TRIGGER reject_launch AFTER UPDATE OF last_played ON games BEGIN SELECT RAISE(ABORT, 'launch rejected'); END;")?;
        assert!(db.increment_play_count(&game).is_err());
        assert_eq!(db.select_game(&game.path)?.unwrap(), original);
        assert!(conn.is_autocommit());
        conn.execute_batch("DROP TRIGGER reject_launch;
CREATE TRIGGER reject_new_launch AFTER INSERT ON games BEGIN SELECT RAISE(ABORT, 'insert rejected'); END;")?;
        assert!(
            db.increment_play_count(&new_game("roms/missing.rom"))
                .is_err()
        );
        assert!(db.select_game(Path::new("roms/missing.rom"))?.is_none());
        conn.execute_batch("DROP TRIGGER reject_new_launch;
CREATE TRIGGER reject_session AFTER INSERT ON game_sessions BEGIN SELECT RAISE(FAIL, 'session rejected'); END;")?;
        assert!(db.record_game_session(&game.path, 10, 70, 60).is_err());
        assert_eq!(db.select_game(&game.path)?.unwrap(), original);
        assert_eq!(
            conn.query_row("SELECT count(*) FROM game_sessions", [], |r| r
                .get::<_, i64>(0))?,
            0
        );
        assert!(conn.is_autocommit());
        conn.execute_batch("DROP TRIGGER reject_session;
CREATE TRIGGER reject_play_time BEFORE UPDATE OF play_time ON games BEGIN SELECT RAISE(ABORT, 'time rejected'); END;")?;
        assert!(db.record_game_session(&game.path, 10, 70, 60).is_err());
        assert!(db.select_sessions_by_day(10, 0)?.is_empty());
        conn.execute_batch(
            "DROP TRIGGER reject_play_time;
CREATE TRIGGER ignore_session BEFORE INSERT ON game_sessions BEGIN SELECT RAISE(IGNORE); END;",
        )?;
        assert!(db.record_game_session(&game.path, 10, 70, 60).is_err());
        assert_eq!(db.select_game(&game.path)?.unwrap(), original);
        assert!(db.select_sessions_by_day(10, 0)?.is_empty());
        conn.execute_batch("DROP TRIGGER ignore_session;")?;
        assert!(
            db.record_game_session(Path::new("roms/missing.rom"), 10, 70, 60)
                .is_err()
        );
        assert!(conn.is_autocommit());
        let before = conn.total_changes();
        db.record_game_session(&game.path, 10, 70, 60)?;
        assert_eq!(conn.total_changes() - before, 2);
        assert_eq!(
            db.select_game(&game.path)?.unwrap().play_time,
            Duration::seconds(60)
        );
        let sessions = db.select_sessions_by_day(10, 0)?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            (
                sessions[0].start_time,
                sessions[0].end_time,
                sessions[0].duration
            ),
            (10, 70, 60)
        );
        // Compatibility wrappers still perform only their original operation.
        db.add_play_time(&game.path, Duration::seconds(5))?;
        db.insert_game_session(&game.path, 70, 75, 5)?;
        assert_eq!(
            db.select_game(&game.path)?.unwrap().play_time,
            Duration::seconds(65)
        );
        assert_eq!(db.select_sessions_by_day(10, 0)?.len(), 2);
        Ok(())
    }

    #[test]
    fn batch_lookup_preserves_order_duplicates_and_missing_paths() -> Result<()> {
        let db = Database::in_memory()?;
        let mut game = new_game("roms/quote\"_%\\.rom");
        game.core = Some("core".to_owned());
        game.rating = Some(85);
        game.release_date = NaiveDate::from_ymd_opt(1999, 3, 4);
        game.favorite = true;
        db.increment_play_count(&game)?;
        db.set_favorite(&game)?;
        db.add_play_time(&game.path, Duration::seconds(45))?;
        let full = db.select_game(&game.path)?.unwrap();
        let mut paths = vec![game.path.as_path(); 1100];
        paths[1] = Path::new("missing");
        let rows = db.select_game_list_metadata(&paths)?;
        assert_eq!(rows.len(), paths.len());
        assert!(rows[1].is_none());
        let expected = GameListMetadata {
            core: full.core.clone(),
            play_count: full.play_count,
            play_time: full.play_time,
            last_played: full.last_played,
            rating: full.rating,
            release_date: full.release_date,
            favorite: full.favorite,
        };
        for (i, row) in rows.iter().enumerate() {
            if i != 1 {
                assert_eq!(row.as_ref(), Some(&expected));
            }
        }
        let other = new_game("roms/other.rom");
        db.increment_play_count(&other)?;
        paths[2] = other.path.as_path();
        let rows = db.select_games(&paths)?;
        assert_eq!(rows.len(), paths.len());
        assert_eq!(rows[2].as_ref().unwrap().path, other.path);
        for (i, row) in rows.iter().enumerate() {
            if i != 2 {
                assert_eq!(row.as_ref(), if i == 1 { None } else { Some(&full) });
            }
        }
        assert!(db.select_game_list_metadata(&[])?.is_empty());
        assert!(db.select_games(&[])?.is_empty());
        Ok(())
    }

    #[test]
    #[ignore = "host full-row versus list-metadata batch benchmark"]
    fn benchmark_game_list_metadata_batch() {
        let db = Database::in_memory().unwrap();
        let games: Vec<_> = (0..2_000)
            .map(|i| {
                let mut game = new_game(&format!("roms/Game {i}.rom"));
                game.genres = vec!["Action".repeat(64); 4];
                game
            })
            .collect();
        db.update_games(&games).unwrap();
        let paths: Vec<_> = games.iter().map(|game| game.path.as_path()).collect();
        let mut full = Vec::new();
        let mut narrow = Vec::new();
        for _ in 0..11 {
            let start = std::time::Instant::now();
            std::hint::black_box(db.select_games(&paths).unwrap());
            full.push(start.elapsed().as_nanos());
            let start = std::time::Instant::now();
            std::hint::black_box(db.select_game_list_metadata(&paths).unwrap());
            narrow.push(start.elapsed().as_nanos());
        }
        full.sort_unstable();
        narrow.sort_unstable();
        println!(
            "LIST_METADATA_BENCH n=2000 full_median_ns={} narrow_median_ns={}",
            full[5], narrow[5]
        );
    }

    fn query_plan(
        conn: &Connection,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<String>> {
        Ok(conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?
            .query_map(params, |row| row.get(3))?
            .collect::<rusqlite::Result<_>>()?)
    }

    #[test]
    fn directory_and_recents_queries_use_indexes() -> Result<()> {
        let db = Database::in_memory()?;
        let conn = db.conn.as_ref().unwrap();
        let directory = query_plan(conn, DIRECTORY_QUERY, params!["roms/", "roms0"])?;
        assert!(
            directory.iter().any(|p| p
                .contains("SEARCH games USING INDEX sqlite_autoindex_games_1 (path>? AND path<?)")),
            "{directory:?}"
        );
        assert!(
            !directory
                .iter()
                .any(|p| p.contains("games_fts") || p.contains("SCAN games")),
            "{directory:?}"
        );
        let recents = query_plan(conn, RECENTS_QUERY, [100])?;
        assert!(
            recents
                .iter()
                .any(|p| p.contains("idx_games_last_played (last_played>?)")),
            "{recents:?}"
        );
        assert!(
            !recents
                .iter()
                .any(|p| p.contains("TEMP B-TREE") || p.contains("SCAN games")),
            "{recents:?}"
        );
        let max = query_plan(conn, "SELECT MAX(last_played) FROM games", [])?;
        assert!(
            max.iter()
                .any(|p| p.contains("COVERING INDEX idx_games_last_played")),
            "{max:?}"
        );
        println!("directory plan: {directory:?}\nrecents plan: {recents:?}\nmax plan: {max:?}");
        Ok(())
    }

    #[test]
    #[ignore = "10,000-row temporary SQLite database timing comparison"]
    fn database_query_timing_10000() -> Result<()> {
        use std::time::{Duration as WallDuration, Instant};
        fn median(
            mut query: impl FnMut() -> Result<Vec<Game>>,
        ) -> Result<(WallDuration, Vec<Game>)> {
            let mut times = Vec::new();
            let mut rows = Vec::new();
            for _ in 0..21 {
                let start = Instant::now();
                rows = query()?;
                times.push(start.elapsed());
            }
            times.sort_unstable();
            Ok((times[times.len() / 2], rows))
        }
        // An empty filename opens an automatically deleted on-disk temporary database.
        let mut conn = Connection::open("")?;
        Database::migrations().to_version(&mut conn, 12)?;
        {
            let tx = conn.unchecked_transaction()?;
            let mut stmt = tx.prepare("INSERT INTO games (name, path, play_count, play_time, last_played) VALUES (?, ?, 1, ?, ?)")?;
            for i in 0..10_000 {
                stmt.execute(params![
                    format!("Game {i}"),
                    format!("roms/console{:02}/game{i:05}.rom", i % 20),
                    i * 10,
                    i + 1
                ])?;
            }
            drop(stmt);
            tx.commit()?;
        }
        const OLD_DIRECTORY: &str = "SELECT games.name, games.path, image, play_count, play_time, last_played, core, rating, release_date, games.developer, games.publisher, genres, favorite, screenshot_path FROM games JOIN games_fts ON games.id = games_fts.rowid WHERE games_fts.path LIKE ? AND games_fts.path NOT LIKE ?";
        let old_directory_plan = query_plan(
            &conn,
            OLD_DIRECTORY,
            params!["roms/console03/%", "roms/console03/%/%"],
        )?;
        let old_recents_plan = query_plan(&conn, RECENTS_QUERY, [100])?;
        let (old_directory_time, mut old_directory) = median(|| {
            Ok(conn
                .prepare(OLD_DIRECTORY)?
                .query_map(params!["roms/console03/%", "roms/console03/%/%"], map_game)?
                .collect::<rusqlite::Result<_>>()?)
        })?;
        let (old_recents_time, old_recents) = median(|| {
            Ok(conn
                .prepare(RECENTS_QUERY)?
                .query_map([100], map_game)?
                .collect::<rusqlite::Result<_>>()?)
        })?;
        Database::migrations().to_latest(&mut conn)?;
        let db = Database {
            conn: Some(Rc::new(conn)),
        };
        let (directory_time, mut directory) =
            median(|| db.select_games_in_directory(Path::new("roms/console03")))?;
        let (recents_time, recents) = median(|| db.select_last_played(100))?;
        old_directory.sort_by(|a, b| a.path.cmp(&b.path));
        directory.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(old_directory, directory);
        assert_eq!(old_recents, recents);
        println!("10,000 rows, temporary DB, 21 samples, full Game row mapping");
        println!(
            "directory rows={} median before={old_directory_time:?} after={directory_time:?}",
            directory.len()
        );
        println!(
            "recents rows={} median before={old_recents_time:?} after={recents_time:?}",
            recents.len()
        );
        println!("directory before plan: {old_directory_plan:?}");
        println!("recents before plan: {old_recents_plan:?}");
        println!(
            "directory after plan: {:?}",
            query_plan(
                db.conn.as_ref().unwrap(),
                DIRECTORY_QUERY,
                params!["roms/console03/", "roms/console030"]
            )?
        );
        println!(
            "recents after plan: {:?}",
            query_plan(db.conn.as_ref().unwrap(), RECENTS_QUERY, [100])?
        );
        Ok(())
    }

    #[test]
    fn test_migrations() {
        Database::migrations().validate().unwrap();
    }

    #[test]
    fn test_most_played() {
        let database = Database::in_memory().unwrap();

        let games = vec![
            NewGame {
                name: "Game One".to_owned(),
                path: PathBuf::from("test_directory/Game One.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game One.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
            NewGame {
                name: "Game Two".to_owned(),
                path: PathBuf::from("test_directory/Game Two.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game Two.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
        ];

        database.update_games(&games).unwrap();

        database.increment_play_count(&games[1]).unwrap();
        database
            .add_play_time(games[1].path.as_path(), Duration::seconds(1))
            .unwrap();
        let most_played = database.select_most_played(2).unwrap();
        assert_eq!(most_played.len(), 1);
        assert_eq!(most_played[0].path, games[1].path);

        database.increment_play_count(&games[0]).unwrap();
        database
            .add_play_time(games[0].path.as_path(), Duration::seconds(2))
            .unwrap();
        let most_played = database.select_most_played(2).unwrap();
        assert_eq!(most_played.len(), 2);
        assert_eq!(most_played[0].path, games[0].path);
        assert_eq!(most_played[1].path, games[1].path);
    }

    #[test]
    fn test_last_played() {
        let database = Database::in_memory().unwrap();

        let games = vec![
            NewGame {
                name: "Game One".to_owned(),
                path: PathBuf::from("test_directory/Game One.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game One.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
            NewGame {
                name: "Game Two".to_owned(),
                path: PathBuf::from("test_directory/Game Two.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game Two.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
        ];

        database.update_games(&games).unwrap();

        for _ in 0..2 {
            database.increment_play_count(&games[1]).unwrap();
        }
        let last_played = database.select_last_played(2).unwrap();
        assert_eq!(last_played.len(), 1);
        assert_eq!(last_played[0].path, games[1].path);

        database.increment_play_count(&games[0]).unwrap();
        let last_played = database.select_last_played(2).unwrap();
        assert_eq!(last_played.len(), 2);
        assert_eq!(last_played[0].path, games[0].path);
        assert_eq!(last_played[1].path, games[1].path);
    }

    #[test]
    fn test_by_rating() {
        let database = Database::in_memory().unwrap();

        let games = vec![
            NewGame {
                name: "Game One".to_owned(),
                path: PathBuf::from("test_directory/Game One.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game One.png")),
                core: None,
                rating: Some(5),
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
            NewGame {
                name: "Game Two".to_owned(),
                path: PathBuf::from("test_directory/Game Two.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game Two.png")),
                core: None,
                rating: Some(9),
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
        ];

        database.update_games(&games).unwrap();

        let by_rating = database.select_by_rating(2).unwrap();
        assert_eq!(by_rating.len(), 2);
        assert_eq!(by_rating[0].path, games[1].path);
        assert_eq!(by_rating[1].path, games[0].path);

        database
            .update_games(&[NewGame {
                name: "Game Two".to_owned(),
                path: PathBuf::from("test_directory/Game Two.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game Two.png")),
                core: None,
                rating: Some(1),
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            }])
            .unwrap();
        let by_rating = database.select_by_rating(2).unwrap();
        assert_eq!(by_rating.len(), 2);
        assert_eq!(by_rating[0].path, games[0].path);
        assert_eq!(by_rating[1].path, games[1].path);
    }

    #[test]
    fn test_by_release_date() {
        let database = Database::in_memory().unwrap();

        let games = vec![
            NewGame {
                name: "Game One".to_owned(),
                path: PathBuf::from("test_directory/Game One.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game One.png")),
                core: None,
                rating: None,
                release_date: Some(NaiveDate::from_ymd_opt(2023, 1, 1).unwrap()),
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
            NewGame {
                name: "Game Two".to_owned(),
                path: PathBuf::from("test_directory/Game Two.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game Two.png")),
                core: None,
                rating: None,
                release_date: Some(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap()),
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
        ];

        database.update_games(&games).unwrap();

        let by_release_date = database.select_by_release_date(2).unwrap();
        assert_eq!(by_release_date.len(), 2);
        assert_eq!(by_release_date[0].path, games[1].path);
        assert_eq!(by_release_date[1].path, games[0].path);

        database
            .update_games(&[NewGame {
                name: "Game Two".to_owned(),
                path: PathBuf::from("test_directory/Game Two.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game Two.png")),
                core: None,
                rating: None,
                release_date: Some(NaiveDate::from_ymd_opt(2022, 1, 1).unwrap()),
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            }])
            .unwrap();
        let by_release_date = database.select_by_release_date(2).unwrap();
        assert_eq!(by_release_date.len(), 2);
        assert_eq!(by_release_date[0].path, games[0].path);
        assert_eq!(by_release_date[1].path, games[1].path);
    }

    #[test]
    fn test_search() {
        let database = Database::in_memory().unwrap();

        let games = vec![
            NewGame {
                name: "Game One".to_owned(),
                path: PathBuf::from("test_directory/Game One.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game One.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
            NewGame {
                name: "Game Two".to_owned(),
                path: PathBuf::from("test_directory/Game Two.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game Two.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: Some("Square Enix".to_owned()),
                publisher: Some("Nintendo".to_owned()),
                genres: Vec::new(),
                favorite: false,
            },
        ];

        database.update_games(&games).unwrap();

        let results = database.search("Game", 100).unwrap();
        assert_eq!(results.len(), 2);

        let results = database.search("One", 100).unwrap();
        assert_eq!(results[0].path, games[0].path);

        let results = database.search("Game One", 100).unwrap();
        assert_eq!(results[0].path, games[0].path);

        let results = database.search("Ga", 100).unwrap();
        assert_eq!(results[0].path, games[0].path);

        let results = database.search("square enix", 100).unwrap();
        assert_eq!(results[0].path, games[1].path);

        let results = database.search("nintendo", 100).unwrap();
        assert_eq!(results[0].path, games[1].path);

        // A title match outranks developer matches even when the limit cuts the list.
        let mut extra: Vec<NewGame> = (0..3).map(|i| NewGame {
            name: format!("Racer {i}"),
            path: PathBuf::from(format!("test_directory/Racer {i}.rom")),
            developer: Some("Bit Managers".to_owned()),
            ..games[0].clone()
        }).collect();
        extra.push(NewGame {
            name: "Secret of Mana".to_owned(),
            path: PathBuf::from("test_directory/Secret of Mana.rom"),
            developer: Some("Square".to_owned()),
            ..games[0].clone()
        });
        database.update_games(&extra).unwrap();
        let results = database.search("mana", 1).unwrap();
        assert_eq!(results[0].name, "Secret of Mana");
    }

    #[test]
    fn test_select_games() {
        let database = Database::in_memory().unwrap();

        let games = vec![
            NewGame {
                name: "Game One".to_owned(),
                path: PathBuf::from("test_directory/Game One.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game One.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
            NewGame {
                name: "Game Two".to_owned(),
                path: PathBuf::from("test_directory/Game Two.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game Two.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
        ];

        database.update_games(&games).unwrap();

        let results = database
            .select_games(&[&games[0].path, &games[1].path])
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().map(|g| &g.path), Some(&games[0].path));
        assert_eq!(results[1].as_ref().map(|g| &g.path), Some(&games[1].path));

        let results = database.select_games(&[&games[1].path]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().map(|g| &g.path), Some(&games[1].path));

        let fake_path = PathBuf::from("test_directory/Game Three.rom");
        let results = database
            .select_games(&[&games[0].path, &fake_path])
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().map(|g| &g.path), Some(&games[0].path));
        assert_eq!(results[1].as_ref().map(|g| &g.path), None);
    }

    #[test]
    fn test_select_games_in_directory() {
        let database = Database::in_memory().unwrap();

        let games = vec![
            NewGame {
                name: "Game One".to_owned(),
                path: PathBuf::from("test_directory/Game One.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game One.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
            NewGame {
                name: "Game Two".to_owned(),
                path: PathBuf::from("test_directory/Game Two.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game Two.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
            NewGame {
                name: "Game Three".to_owned(),
                path: PathBuf::from("different_directory/Game Three.rom"),
                image: Some(PathBuf::from("different_directory/Imgs/Game Three.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
        ];

        database.update_games(&games).unwrap();

        let results = database
            .select_games_in_directory(Path::new("test_directory"))
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].path, games[0].path);
        assert_eq!(results[1].path, games[1].path);

        let results = database
            .select_games_in_directory(Path::new("test_directory/Imgs"))
            .unwrap();
        assert_eq!(results.len(), 0);

        let results = database
            .select_games_in_directory(Path::new("different_directory"))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, games[2].path);

        let results = database
            .select_games_in_directory(Path::new("directory"))
            .unwrap();
        assert_eq!(results.len(), 0);

        let results = database
            .select_games_in_directory(Path::new("test"))
            .unwrap();
        assert_eq!(results.len(), 0);

        let results = database.select_games_in_directory(Path::new("")).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_set_core() -> Result<()> {
        let db = Database::in_memory().unwrap();

        let games = vec![
            NewGame {
                name: "Game One".to_owned(),
                path: PathBuf::from("test_directory/Game One.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game One.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
            NewGame {
                name: "Game Two".to_owned(),
                path: PathBuf::from("test_directory/Game Two.rom"),
                image: Some(PathBuf::from("test_directory/Imgs/Game Two.png")),
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: Vec::new(),
                favorite: false,
            },
        ];

        db.update_games(&games).unwrap();

        let core = db.get_core(&games[0].path)?;
        assert_eq!(core, None);

        db.set_core(&games[0].path, "CORE")?;

        let core = db.get_core(&games[0].path)?;
        assert_eq!(core, Some("CORE".to_owned()));

        Ok(())
    }

    #[test]
    fn test_set_genres() -> Result<()> {
        let db = Database::in_memory().unwrap();

        let mut games = vec![NewGame {
            name: "Game One".to_owned(),
            path: PathBuf::from("test_directory/Game One.rom"),
            image: Some(PathBuf::from("test_directory/Imgs/Game One.png")),
            core: None,
            rating: None,
            release_date: None,
            developer: None,
            publisher: None,
            genres: vec!["Action".to_owned(), "Adventure".to_owned()],
            favorite: false,
        }];

        db.update_games(&games).unwrap();
        let game = db.select_game(&games[0].path)?.unwrap();
        assert_eq!(
            game.genres,
            vec!["Action".to_owned(), "Adventure".to_owned()]
        );

        games[0].genres = Vec::new();
        db.update_games(&games).unwrap();
        let game = db.select_game(&games[0].path)?.unwrap();
        assert!(game.genres.is_empty());

        games[0].genres = vec!["Puzzle".to_owned()];
        db.update_games(&games).unwrap();
        let game = db.select_game(&games[0].path)?.unwrap();
        assert_eq!(game.genres, vec!["Puzzle".to_owned()]);

        Ok(())
    }
}
