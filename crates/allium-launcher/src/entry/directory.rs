use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use common::{
    constants::ALLIUM_GAMES_DIR,
    database::{Database, Game as DbGame, NewGame},
    locale::Locale,
};
use itertools::Itertools;
use log::{debug, error, trace};
use serde::{Deserialize, Serialize};

use crate::{
    consoles::ConsoleMapper,
    entry::{Entry, game::Game, gamelist::GameList, lazy_image::LazyImage, short_name},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Directory {
    pub name: String,
    pub full_name: String,
    pub path: PathBuf,
    /// image is loaded lazily.
    /// None means image hasn't been looked for, Some(None) means no image was found, Some(Some(path)) means an image was found.
    pub image: LazyImage,
}

impl Ord for Directory {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl PartialOrd for Directory {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Default for Directory {
    fn default() -> Self {
        Directory {
            name: "Games".into(),
            full_name: "Games".into(),
            path: ALLIUM_GAMES_DIR.to_owned(),
            image: LazyImage::Unknown(ALLIUM_GAMES_DIR.to_owned()),
        }
    }
}

impl Directory {
    pub fn new(path: PathBuf) -> Directory {
        let full_name = path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("")
            .to_string();
        let name = short_name(&full_name);
        let image = LazyImage::Unknown(path.clone());
        Directory {
            name,
            full_name,
            path,
            image,
        }
    }

    pub fn with_name(path: PathBuf, name: String) -> Directory {
        let full_name = path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("")
            .to_string();
        let image = LazyImage::Unknown(path.clone());
        Directory {
            name,
            full_name,
            path,
            image,
        }
    }

    pub fn image(&mut self) -> Option<&Path> {
        self.image.image()
    }

    pub(crate) fn parse_game_list(&self, game_list: &Path) -> Result<Vec<Entry>> {
        let mut file = File::open(game_list)?;
        let mut s = String::with_capacity(1024);
        file.read_to_string(&mut s)?;
        let gamelist: GameList = match quick_xml::de::from_str(&s) {
            Ok(gamelist) => gamelist,
            Err(_) => {
                // Some scrapers produce ill-formed XML where ampersands (&) are not escaped,
                // so we try to failover to replacing them to &amp;
                // (https://github.com/RReverser/serde-xml-rs/issues/106)
                s = s.replace('&', "&amp;");
                quick_xml::de::from_str(&s)?
            }
        };

        let games = gamelist.games.into_iter().filter_map(|game| {
            let path = self.path.join(&game.path).canonicalize().ok()?;
            let extension = game
                .path
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_owned();

            let full_name = game.name.clone();

            let image = game.image.or(game.thumbnail);
            let image = match image {
                Some(image) => {
                    if let Ok(path) = self.path.join(image).canonicalize() {
                        LazyImage::Found(path)
                    } else {
                        LazyImage::Unknown(path.clone())
                    }
                }
                None => LazyImage::Unknown(path.clone()),
            };

            Some(Entry::Game(Game {
                path,
                name: game.name,
                full_name,
                image,
                extension,
                core: None,
                rating: game.rating,
                release_date: game.release_date.map(|d| d.date()),
                developer: game.developer,
                publisher: game.publisher,
                genres: game.genres,
                favorite: false,
                screenshot_path: None,
            }))
        });

        let folders = gamelist.folders.into_iter().filter_map(|folder| {
            let path = self.path.join(&folder.path);
            if !path.exists() {
                return None;
            }

            let name = folder.name;

            Some(Entry::Directory(Directory::with_name(path, name)))
        });

        Ok(folders.chain(games).collect())
    }

    pub fn entries(
        &self,
        database: &Database,
        console_mapper: &ConsoleMapper,
        locale: &Locale,
    ) -> Result<Vec<Entry>> {
        Ok(self
            .entries_with_metadata(database, console_mapper, locale)?
            .entries)
    }

    pub(crate) fn entries_with_metadata(
        &self,
        database: &Database,
        console_mapper: &ConsoleMapper,
        #[allow(unused)] locale: &Locale,
    ) -> Result<DirectoryEntries> {
        common::media::check_directory(&self.path);
        debug!("Populating entries for directory: {:?}", self.path);
        #[cfg(test)]
        super::load_work::record(|work| work.directory_reads += 1);
        let physical = match fs::read_dir(&self.path) {
            Ok(entries) => Some(entries),
            Err(_) if crate::network_runtime::virtual_directory(&self.path) => None,
            Err(error) => {
                return Err(anyhow!(
                    "Failed to open directory: {:?}, {}",
                    self.path,
                    error
                ));
            }
        };
        let scanned: Vec<_> = physical
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| {
                let path = entry.path();
                #[cfg(test)]
                super::load_work::record(|work| work.file_types += 1);
                let (is_dir, exists) = match entry.file_type() {
                    Ok(kind) if !kind.is_symlink() => (kind.is_dir(), true),
                    _ => {
                        // DirEntry::file_type does not follow symlinks; preserve the old behavior.
                        #[cfg(test)]
                        super::load_work::record(|work| work.metadata += 1);
                        match path.metadata() {
                            Ok(metadata) => (metadata.is_dir(), true),
                            Err(_) => (false, false),
                        }
                    }
                };
                (path, is_dir, exists)
            })
            .collect();
        let inventory: HashSet<_> = scanned
            .iter()
            .filter(|(_, _, exists)| *exists)
            .map(|(path, _, _)| path.as_path())
            .collect();
        let mut entries = Vec::with_capacity(scanned.len());

        #[cfg(test)]
        super::load_work::record(|work| work.sql += 1);
        let fingerprint = database.get_gamelist_fingerprint(&self.path)?;
        // Prefer gamelist.xml even when unchanged or malformed, as before.
        let gamelist = ["gamelist.xml", "miyoogamelist.xml"]
            .into_iter()
            .map(|name| self.path.join(name))
            .find(|path| inventory.contains(path.as_path()));
        if let Some(gamelist) = gamelist {
            #[cfg(test)]
            super::load_work::record(|work| work.metadata += 1);
            if let Ok(metadata) = fs::metadata(&gamelist) {
                let current_fingerprint = common::media::fingerprint(&metadata);
                if fingerprint != Some(current_fingerprint) {
                    common::media::with_name_indexes(crate::view::details::invalidate_metadata);
                    #[cfg(feature = "miyoo")]
                    {
                        std::process::Command::new("show")
                            .arg("--darken")
                            .spawn()?
                            .wait()?;
                        let mut map = HashMap::new();
                        map.insert(
                            "directory".into(),
                            self.path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into(),
                        );
                        std::process::Command::new("say")
                            .arg(locale.ta("populating-games", &map))
                            .spawn()?
                            .wait()?;
                    }
                    match self.parse_game_list(&gamelist) {
                        Ok(imported) => {
                            let games: Vec<_> = imported
                                .iter()
                                .filter_map(|entry| match entry {
                                    Entry::Game(game) => Some(NewGame {
                                        name: game.name.clone(),
                                        path: game.path.clone(),
                                        image: game.image.try_image().map(Path::to_path_buf),
                                        core: game.core.clone(),
                                        rating: game.rating,
                                        release_date: game.release_date,
                                        developer: game.developer.clone(),
                                        publisher: game.publisher.clone(),
                                        genres: game.genres.clone(),
                                        favorite: game.favorite,
                                    }),
                                    _ => None,
                                })
                                .collect();
                            database.update_game_metadata(&games)?;
                            // A failed parse/import must remain retryable on the next load.
                            database.set_gamelist_fingerprint(&self.path, current_fingerprint)?;
                            entries.extend(imported);
                        }
                        Err(error) => error!("Failed to parse {}: {error:#}", gamelist.display()),
                    }
                }
            }
        }

        #[cfg(test)]
        super::load_work::record(|work| work.sql += 1);
        // Components remove trailing/repeated separators without filesystem access.
        let query_path: PathBuf = self.path.components().collect();
        let mut rows = database.select_games_in_directory(&query_path)?;
        if query_path.to_string_lossy().contains(['%', '_']) {
            // The database API has no LIKE escaping. Discard wildcard overmatches
            // with literal parent comparisons without loading the entire database.
            rows.retain(|row| row.path.parent() == Some(query_path.as_path()));
        }
        let mut games = HashMap::with_capacity(rows.len());
        for row in rows {
            games.insert(row.path.clone(), GameMetadata::from(&row));
            let exists = inventory_contains_or_exists(&inventory, &row.path);
            if exists || crate::network_runtime::registered(&row.path) {
                entries.push(Entry::Game(Game::from_db_present(row)));
            }
        }
        drop(inventory);
        entries.extend(
            scanned
                .into_iter()
                .filter_map(|(path, is_dir, _)| {
                    Entry::from_scanned_path(path, is_dir, console_mapper)
                        .ok()
                        .flatten()
                })
                .sorted()
                .dedup_by(|a, b| a.name() == b.name()),
        );

        crate::network_runtime::merge_directory(&self.path, &mut entries);
        let mut uniques = HashSet::with_capacity(entries.len());
        entries.retain(|entry| uniques.insert(entry.path().to_path_buf()));

        // Imports and physical entries can be absent from the bulk result, including
        // immediate children. Recover their metadata with exact path lookups.
        let missing: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Game(game) if !games.contains_key(&game.path) => Some(game.path.as_path()),
                _ => None,
            })
            .collect();
        if !missing.is_empty() {
            #[cfg(test)]
            super::load_work::record(|work| work.sql += 1);
            let rows = database.select_game_list_metadata(&missing)?;
            for (path, row) in missing.into_iter().zip(rows) {
                if let Some(row) = row {
                    games.insert(path.to_path_buf(), GameMetadata::from(&row));
                }
            }
        }
        for entry in &mut entries {
            if let Entry::Game(game) = entry {
                game.hydrate_name();
                if let Some(row) = games.get(&game.path) {
                    game.favorite = row.favorite;
                    if let Some(core) = &row.core {
                        game.core = Some(core.clone());
                    }
                }
            }
        }
        trace!(
            "Final entries for directory {:?}: {:?}",
            self.path,
            entries.iter().map(|entry| entry.path()).collect::<Vec<_>>()
        );
        Ok(DirectoryEntries { entries, games })
    }

    /// Populate the database with the games in this directory, pushing any subdirectories onto the
    /// queue.
    pub fn populate_db(
        &self,
        queue: &mut VecDeque<Directory>,
        database: &Database,
        console_mapper: &ConsoleMapper,
        locale: &Locale,
    ) -> Result<()> {
        let entries = self.entries(database, console_mapper, locale)?;

        for entry in &entries {
            match entry {
                Entry::Directory(dir) => queue.push_back(dir.clone()),
                Entry::Game(_) | Entry::App(_) => {}
            }
        }

        let games: Vec<_> = entries
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Game(game) => Some(NewGame {
                    name: game.name,
                    path: game.path,
                    image: game.image.try_image().map(Path::to_path_buf),
                    core: game.core,
                    rating: game.rating,
                    release_date: game.release_date,
                    developer: game.developer,
                    publisher: game.publisher,
                    genres: game.genres,
                    favorite: game.favorite,
                }),
                _ => None,
            })
            .collect();
        database.update_games(&games)?;

        Ok(())
    }
}

impl From<&Path> for Directory {
    fn from(path: &Path) -> Self {
        Directory::new(path.into())
    }
}

fn inventory_contains_or_exists(inventory: &HashSet<&Path>, path: &Path) -> bool {
    if inventory.contains(path) {
        return true;
    }
    // FAT32 can resolve saved paths whose spelling differs from the directory entry.
    // Keep filesystem probes on inventory misses so ordinary rows need no stat calls.
    #[cfg(test)]
    super::load_work::record(|work| work.metadata += 1);
    path.exists()
}

pub(crate) struct DirectoryEntries {
    pub entries: Vec<Entry>,
    pub games: HashMap<PathBuf, GameMetadata>,
}

#[derive(Debug, Clone)]
pub(crate) struct GameMetadata {
    pub core: Option<String>,
    pub favorite: bool,
    pub last_played: i64,
    pub play_time: chrono::Duration,
    pub rating: Option<u8>,
    pub release_date: Option<chrono::NaiveDate>,
}

impl From<&DbGame> for GameMetadata {
    fn from(game: &DbGame) -> Self {
        Self {
            core: game.core.clone(),
            favorite: game.favorite,
            last_played: game.last_played,
            play_time: game.play_time,
            rating: game.rating,
            release_date: game.release_date,
        }
    }
}

impl From<&common::database::GameListMetadata> for GameMetadata {
    fn from(game: &common::database::GameListMetadata) -> Self {
        Self {
            core: game.core.clone(),
            favorite: game.favorite,
            last_played: game.last_played,
            play_time: game.play_time,
            rating: game.rating,
            release_date: game.release_date,
        }
    }
}

#[cfg(test)]
pub(crate) mod list_tests {
    use super::*;
    use crate::entry::load_work;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    pub(crate) struct Fixture(pub Directory);
    impl Fixture {
        pub(crate) fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "allium-lists-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            Self(Directory::new(root.canonicalize().unwrap()))
        }
        pub(crate) fn games(&self, count: usize, database: &Database) {
            let rows: Vec<_> = (0..count)
                .map(|i| {
                    let path = self.0.path.join(format!("Game {i:05}.chd"));
                    fs::write(&path, b"fixture").unwrap();
                    let mut game = Game::new(path);
                    game.image = LazyImage::NotFound;
                    game.into()
                })
                .collect();
            database.update_games(&rows).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0.path).unwrap();
        }
    }

    // Reference for the original warm load, with no gamelist import or network catalog.
    pub(crate) fn legacy_warm_load(directory: &Directory, database: &Database) -> DirectoryEntries {
        common::media::check_directory(&directory.path);
        load_work::record(|work| work.sql += 1);
        let _ = database.get_gamelist_fingerprint(&directory.path).unwrap();
        for name in ["gamelist.xml", "gamelist.xml", "miyoogamelist.xml"] {
            load_work::record(|work| work.metadata += 1);
            assert!(!directory.path.join(name).exists());
        }
        load_work::record(|work| work.sql += 1);
        let mut entries: Vec<_> = database
            .select_games_in_directory(&directory.path)
            .unwrap()
            .into_iter()
            .filter_map(Game::from_db)
            .map(Entry::Game)
            .collect();
        load_work::record(|work| work.directory_reads += 1);
        entries.extend(
            fs::read_dir(&directory.path)
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    Entry::new(entry.path(), &ConsoleMapper::new())
                        .ok()
                        .flatten()
                })
                .sorted()
                .dedup_by(|a, b| a.name() == b.name()),
        );
        let mut unique = HashSet::new();
        entries.retain(|entry| unique.insert(entry.path().to_path_buf()));
        for entry in &mut entries {
            if let Entry::Game(game) = entry {
                game.hydrate_name();
                load_work::record(|work| work.sql += 1);
                if let Some(core) = database.get_core(&game.path).unwrap() {
                    game.core = Some(core);
                }
            }
        }
        let paths: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Game(game) => Some(game.path.as_path()),
                _ => None,
            })
            .collect();
        load_work::record(|work| work.sql += paths.len());
        let games = database
            .select_games(&paths)
            .unwrap()
            .into_iter()
            .flatten()
            .map(|row| (row.path.clone(), GameMetadata::from(&row)))
            .collect();
        DirectoryEntries { entries, games }
    }

    #[test]
    fn directory_inventory_matches_legacy_and_removes_stale_rows() {
        let fixture = Fixture::new();
        let database = Database::in_memory().unwrap();
        fixture.games(4, &database);
        let stale = fixture.0.path.join("Game 00000.chd");
        fs::remove_file(&stale).unwrap();
        fs::create_dir(fixture.0.path.join("Folder.v1")).unwrap();
        fs::create_dir(fixture.0.path.join("Playlist.m3u")).unwrap();
        let pak = fixture.0.path.join("Tool.pak");
        fs::create_dir(&pak).unwrap();
        fs::write(pak.join("launch.sh"), b"#!/bin/sh\n").unwrap();
        for name in [".hidden", "Imgs", "skip.sbi"] {
            fs::write(fixture.0.path.join(name), b"fixture").unwrap();
        }
        database
            .set_core(&fixture.0.path.join("Game 00001.chd"), "custom-core")
            .unwrap();
        let before = legacy_warm_load(&fixture.0, &database);
        let after = fixture
            .0
            .entries_with_metadata(&database, &ConsoleMapper::new(), &Locale::new("en-US"))
            .unwrap();
        assert_eq!(after.entries, before.entries);
        assert!(!after.entries.iter().any(|entry| entry.path() == stale));
        assert!(after.entries.iter().any(|entry| matches!(entry, Entry::Game(game) if game.core.as_deref() == Some("custom-core"))));
    }

    #[test]
    fn case_only_rename_keeps_saved_identity_and_metadata() {
        let fixture = Fixture::new();
        let database = Database::in_memory().unwrap();
        let saved = fixture.0.path.join("Game.chd");
        let renamed = fixture.0.path.join("GAME.chd");
        fs::write(&saved, b"fixture").unwrap();
        let mut row: NewGame = Game::new(saved.clone()).into();
        row.core = Some("custom-core".into());
        row.favorite = true;
        database.update_games(std::slice::from_ref(&row)).unwrap();
        database.set_favorite(&row).unwrap();
        assert!(database.select_game(&saved).unwrap().unwrap().favorite);
        database
            .add_play_time(&saved, chrono::Duration::seconds(123))
            .unwrap();
        fs::rename(&saved, &renamed).unwrap();
        if !saved.exists() {
            // On case-sensitive hosts a hard link simulates FAT32 resolving the saved
            // spelling. The synthetic inventory still contains only the renamed entry.
            fs::hard_link(&renamed, &saved).unwrap();
        }
        let inventory = HashSet::from([renamed.as_path()]);
        assert!(!inventory.contains(saved.as_path()));
        assert!(inventory_contains_or_exists(&inventory, &saved));
        let loaded = fixture
            .0
            .entries_with_metadata(&database, &ConsoleMapper::new(), &Locale::new("en-US"))
            .unwrap();
        let game = loaded
            .entries
            .iter()
            .find_map(|entry| match entry {
                Entry::Game(game) if game.path == saved => Some(game),
                _ => None,
            })
            .expect("saved DB identity must survive the case-only rename");
        assert_eq!(game.core.as_deref(), Some("custom-core"));
        assert!(game.favorite);
        assert_eq!(
            loaded.games[&saved].play_time,
            chrono::Duration::seconds(123)
        );
        assert!(database.select_game(&renamed).unwrap().is_none());
    }

    #[test]
    fn directory_wildcards_do_not_admit_other_directories() {
        let fixture = Fixture::new();
        let database = Database::in_memory().unwrap();
        let directory = Directory::new(fixture.0.path.join("PS%_"));
        let other = fixture.0.path.join("PS-other");
        fs::create_dir(&directory.path).unwrap();
        fs::create_dir(&other).unwrap();
        let saved = directory.path.join("Saved.chd");
        let foreign = other.join("Foreign.chd");
        for path in [&saved, &foreign] {
            fs::write(path, b"fixture").unwrap();
            database
                .update_games(&[Game::new(path.clone()).into()])
                .unwrap();
        }
        database.set_core(&saved, "custom-core").unwrap();
        let loaded = directory
            .entries_with_metadata(&database, &ConsoleMapper::new(), &Locale::new("en-US"))
            .unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].path(), saved);
        assert_eq!(loaded.games.len(), 1);
        assert_eq!(loaded.games[&saved].core.as_deref(), Some("custom-core"));
    }

    #[cfg(unix)]
    #[test]
    fn scan_preserves_symlink_directory_app_and_dangling_behavior() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let target = fixture.0.path.join("Folder");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("launch.sh"), b"#!/bin/sh\n").unwrap();
        symlink(&target, fixture.0.path.join("Link.v1")).unwrap();
        symlink(&target, fixture.0.path.join("Link.pak")).unwrap();
        symlink(&target, fixture.0.path.join("Link.m3u")).unwrap();
        symlink(
            fixture.0.path.join("missing"),
            fixture.0.path.join("Dangling.chd"),
        )
        .unwrap();
        let database = Database::in_memory().unwrap();
        let before = legacy_warm_load(&fixture.0, &database);
        let after = fixture
            .0
            .entries(&database, &ConsoleMapper::new(), &Locale::new("en-US"))
            .unwrap();
        assert_eq!(after, before.entries);
    }

    #[test]
    fn failed_gamelist_import_does_not_publish_fingerprint() {
        let fixture = Fixture::new();
        let database = Database::in_memory().unwrap();
        fixture.games(1, &database);
        let gamelist = fixture.0.path.join("gamelist.xml");
        fs::write(&gamelist, "<gameList><game>").unwrap();
        let load = || {
            fixture
                .0
                .entries(&database, &ConsoleMapper::new(), &Locale::new("en-US"))
                .unwrap()
        };
        load();
        assert_eq!(
            database.get_gamelist_fingerprint(&fixture.0.path).unwrap(),
            None
        );
        // Retrying the identical malformed file must not mark it as imported either.
        load();
        assert_eq!(
            database.get_gamelist_fingerprint(&fixture.0.path).unwrap(),
            None
        );
        fs::write(
            &gamelist,
            "<gameList><game><path>./Game 00000.chd</path><name>Imported</name></game></gameList>",
        )
        .unwrap();
        load();
        let published = database
            .get_gamelist_fingerprint(&fixture.0.path)
            .unwrap()
            .unwrap();
        assert_eq!(
            published,
            common::media::fingerprint(&fs::metadata(&gamelist).unwrap())
        );
        assert_eq!(
            database
                .select_game(&fixture.0.path.join("Game 00000.chd"))
                .unwrap()
                .unwrap()
                .name,
            "Imported"
        );
        fs::write(&gamelist, "<gameList><game>").unwrap();
        load();
        assert_eq!(
            database.get_gamelist_fingerprint(&fixture.0.path).unwrap(),
            Some(published)
        );
    }

    #[test]
    fn gamelist_import_keeps_favorite_sort_identity_and_custom_core() {
        let fixture = Fixture::new();
        let database = Database::in_memory().unwrap();
        fixture.games(1, &database);
        let path = fixture.0.path.join("Game 00000.chd");
        database.set_core(&path, "custom-core").unwrap();
        let mut favorite: NewGame = Game::new(path.clone()).into();
        favorite.favorite = true;
        database.set_favorite(&favorite).unwrap();
        fs::write(fixture.0.path.join("gamelist.xml"), "<gameList><game><path>./Game 00000.chd</path><name>Imported favorite</name></game></gameList>").unwrap();
        let loaded = fixture
            .0
            .entries_with_metadata(&database, &ConsoleMapper::new(), &Locale::new("en-US"))
            .unwrap();
        let Entry::Game(game) = &loaded.entries[0] else {
            panic!("expected imported game")
        };
        assert!(game.favorite);
        assert!(loaded.games[&game.path].favorite);
        assert_eq!(game.core.as_deref(), Some("custom-core"));
        assert_eq!(
            database.get_core(&path).unwrap().as_deref(),
            Some("custom-core")
        );
    }

    #[test]
    fn inventory_reuses_rows_without_per_game_metadata_or_sql() {
        let fixture = Fixture::new();
        let database = Database::in_memory().unwrap();
        fixture.games(12, &database);
        load_work::take();
        let before = legacy_warm_load(&fixture.0, &database);
        let old = load_work::take();
        let after = fixture
            .0
            .entries_with_metadata(&database, &ConsoleMapper::new(), &Locale::new("en-US"))
            .unwrap();
        let new = load_work::take();
        assert_eq!(before.entries, after.entries);
        assert_eq!(
            old,
            load_work::Counts {
                directory_reads: 1,
                metadata: 27,
                file_types: 0,
                sql: 26
            }
        );
        assert_eq!(
            new,
            load_work::Counts {
                directory_reads: 1,
                metadata: 0,
                file_types: 12,
                sql: 2
            }
        );
    }
}
