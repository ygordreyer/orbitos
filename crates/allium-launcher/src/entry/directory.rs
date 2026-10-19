use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
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

thread_local! {
    static SYSTEM_COUNTS: RefCell<HashMap<PathBuf, (Option<SystemTime>, u64, usize)>> = RefCell::new(HashMap::new());
}

pub(crate) fn invalidate_system_counts() {
    SYSTEM_COUNTS.with(|counts| counts.borrow_mut().clear());
    super::library::invalidate();
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Directory {
    pub name: String,
    pub full_name: String,
    pub path: PathBuf,
    /// image is loaded lazily.
    /// None means image hasn't been looked for, Some(None) means no image was found, Some(Some(path)) means an image was found.
    pub image: LazyImage,
    #[serde(skip)]
    pub game_count: Option<usize>,
    #[serde(skip)]
    pub(crate) game: Option<Box<Game>>,
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
            game_count: None,
            game: None,
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
            game_count: None,
            game: None,
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
            game_count: None,
            game: None,
        }
    }

    fn retain_populated_systems(entries: &mut Vec<Entry>, console_mapper: &ConsoleMapper) {
        entries.retain_mut(|entry| {
            if let Entry::Directory(directory) = entry {
                let virtual_system = crate::network_runtime::virtual_directory(&directory.path);
                let count = directory.cached_game_count(console_mapper);
                directory.game_count = Some(count);
                count > 0 || virtual_system
            } else { true }
        });
    }

    pub(super) fn cached_game_count(&self, console_mapper: &ConsoleMapper) -> usize {
        let modified = fs::metadata(&self.path).and_then(|metadata| metadata.modified()).ok();
        let catalog = crate::network_runtime::art_path_generation();
        SYSTEM_COUNTS.with(|counts| {
            if let Some((stamp, generation, count)) = counts.borrow().get(&self.path) {
                if modified == *stamp && catalog == *generation { return *count; }
            }
            let count = self.walk_games(console_mapper, &mut HashSet::new(), usize::MAX);
            counts.borrow_mut().insert(self.path.clone(), (modified, catalog, count));
            count
        })
    }

    fn walk_games(&self, console_mapper: &ConsoleMapper, ancestors: &mut HashSet<PathBuf>, limit: usize) -> usize {
        let canonical = self.path.canonicalize().unwrap_or_else(|_| self.path.clone());
        if !ancestors.insert(canonical.clone()) { return 0; }
        if limit != 1 {
            let count = super::library::count_entries(self, console_mapper).into_iter().map(|entry| match entry {
                Entry::Directory(directory) => directory.walk_games(console_mapper, ancestors, usize::MAX),
                Entry::Game(_) | Entry::App(_) => 1,
            }).sum();
            ancestors.remove(&canonical);
            return count;
        }
        #[cfg(test)]
        super::load_work::record(|work| work.directory_reads += 1);
        let virtual_system = crate::network_runtime::virtual_directory(&self.path);
        let mut entries = Vec::new();
        let mut count = 0;
        if let Ok(physical) = fs::read_dir(&self.path) {
            for entry in physical.filter_map(Result::ok) {
                #[cfg(test)]
                super::load_work::record(|work| work.file_types += 1);
                let path = entry.path();
                let is_dir = match entry.file_type() {
                    Ok(kind) if !kind.is_symlink() => kind.is_dir(),
                    _ => match path.metadata() {
                        Ok(metadata) => metadata.is_dir(),
                        Err(_) => continue,
                    },
                };
                if let Some(entry) = Entry::from_scanned_path(path, is_dir, console_mapper).ok().flatten() {
                    if virtual_system {
                        entries.push(entry);
                    } else {
                        count += match entry {
                            Entry::Directory(directory) => directory.walk_games(console_mapper, ancestors, limit - count),
                            Entry::Game(_) | Entry::App(_) => 1,
                        };
                        if count >= limit { break; }
                    }
                }
            }
        }
        if virtual_system { crate::network_runtime::merge_directory(&self.path, &mut entries); }
        let mut seen = HashSet::new();
        for entry in entries {
            if !seen.insert(entry.path().to_path_buf()) { continue; }
            count += match entry {
                Entry::Directory(directory) => directory.walk_games(console_mapper, ancestors, limit - count),
                Entry::Game(_) | Entry::App(_) => 1,
            };
            if count >= limit { break; }
        }
        ancestors.remove(&canonical);
        count
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
            if !super::listed_path(&path) { return None; }
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

            let folder = path.is_dir() && !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("m3u"));
            let game = Game {
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
            };
            if folder {
                let mut directory = Directory::with_name(game.path.clone(), game.name.clone());
                directory.image = game.image.clone();
                directory.game = Some(Box::new(game));
                Some(Entry::Directory(directory))
            } else {
                Some(Entry::Game(game))
            }
        });

        let folders = gamelist.folders.into_iter().filter_map(|folder| {
            let path = self.path.join(&folder.path);
            if !path.is_dir() || !super::listed_path(&path) {
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
        let physical: Vec<_> = scanned.iter().filter_map(|(path, is_dir, _)| {
            Entry::from_scanned_path(path.clone(), *is_dir, console_mapper).ok().flatten()
        }).collect();
        let inventory: HashSet<_> = scanned
            .iter()
            .filter(|(_, _, exists)| *exists)
            .map(|(path, _, _)| path.as_path())
            .collect();
        if self.path != *ALLIUM_GAMES_DIR {
            let present: Vec<_> = physical.iter().filter(|entry| inventory.contains(entry.path())).cloned().collect();
            super::library::remember(&self.path, &present);
        }
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
                                    Entry::Directory(directory) => {
                                        let mut game = directory.game.as_deref().cloned()
                                            .unwrap_or_else(|| Game::new(directory.path.clone()));
                                        game.name = directory.name.clone();
                                        game.image = directory.image.clone();
                                        Some(game.into())
                                    }
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
        let directories: HashSet<_> = scanned.iter().filter(|(_, is_dir, _)| *is_dir)
            .map(|(path, _, _)| path.as_path()).collect();
        let mut games = HashMap::with_capacity(rows.len());
        for row in rows {
            if !super::listed_path(&row.path) { continue; }
            games.insert(row.path.clone(), GameMetadata::from(&row));
            let exists = inventory_contains_or_exists(&inventory, &row.path);
            if exists || crate::network_runtime::registered(&row.path) {
                if directories.contains(row.path.as_path())
                    && !row.path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("m3u")) {
                    let mut directory = Directory::with_name(row.path.clone(), row.name.clone());
                    directory.image = LazyImage::from_path(&row.path, row.image);
                    entries.push(Entry::Directory(directory));
                } else {
                    entries.push(Entry::Game(Game::from_db_present(row)));
                }
            }
        }
        drop(inventory);
        drop(directories);
        entries.extend(physical.into_iter().sorted());

        crate::network_runtime::merge_directory(&self.path, &mut entries);
        let mut uniques = HashSet::with_capacity(entries.len());
        entries.retain(|entry| super::listed_path(entry.path()) && uniques.insert(entry.path().to_path_buf()));
        super::library::filter_games(&mut entries, console_mapper);
        if self.path == *ALLIUM_GAMES_DIR {
            Self::retain_populated_systems(&mut entries, console_mapper);
        } else {
            super::library::resolve_folders(&mut entries, console_mapper, database)?;
            let mut paths = HashSet::new();
            entries.retain(|entry| paths.insert(entry.path().to_path_buf()));
        }

        let nested: Vec<_> = entries.iter().filter_map(|entry| match entry {
            Entry::Game(game) if game.path.parent() != Some(self.path.as_path()) => Some(game.path.as_path()),
            _ => None,
        }).collect();
        if !nested.is_empty() {
            #[cfg(test)]
            super::load_work::record(|work| work.sql += 1);
            let mut rows: HashMap<_, _> = database.select_games(&nested)?.into_iter().flatten()
                .map(|row| (row.path.clone(), row)).collect();
            for entry in &mut entries {
                if let Some(row) = rows.remove(entry.path()) {
                    games.insert(row.path.clone(), GameMetadata::from(&row));
                    *entry = Entry::Game(Game::from_db_present(row));
                }
            }
        }

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
        invalidate_system_counts();
        let entries = self.entries(database, console_mapper, locale)?;

        for entry in super::library::inventory(&self.path, console_mapper).unwrap_or_default() {
            match entry {
                Entry::Directory(dir) => queue.push_back(dir),
                Entry::Game(game) if game.extension.eq_ignore_ascii_case("m3u") && game.path.is_dir() => {
                    queue.push_back(Directory::new(game.path));
                }
                _ => {}
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

    fn single_disc_catalog_folders(cached: bool) {
        use crate::network_library::{NetworkGame, Payload};
        use sha2::{Digest, Sha256};
        let fixture = Fixture::new();
        let root = &fixture.0.path;
        let ps = root.join("Roms/PS");
        let mut games = Vec::new();
        let titles = ["Mizzurna Falls: The Chilling Cut", "Mega Man Legends 2 PSP Improvements (v1.1)"];
        let mut xml = "<gameList>".to_owned();
        for (id, folder, title, stem, tracks) in [
            ("mizzurna", "Mizzurna Falls - The Chilling Cut (Owl)", "Mizzurna Falls: The Chilling Cut", "Mizzurna Falls - The Chilling Cut (Owl)", 1),
            ("megaman", "Mega Man Legends 2 PSP Improvements (v1.1)", "Mega Man Legends 2 PSP Improvements (v1.1)", "Mega Man Legends 2 (USA)", 2),
        ] {
            let path = ps.join(folder);
            fs::create_dir_all(&path).unwrap();
            let revision = "b".repeat(64);
            let cue = format!("{stem}.cue");
            let mut payloads = Vec::new();
            let mut cue_text = String::new();
            for track in 1..=tracks {
                let bin = if tracks == 1 { format!("{stem}.bin") } else { format!("{stem} (Track {track}).bin") };
                cue_text.push_str(&format!("FILE \"{bin}\" BINARY\r\n  TRACK {track:02} MODE2/2352\r\n    INDEX 01 00:00:00\r\n"));
                payloads.push((bin, "rom", b"fixture ROM".to_vec()));
            }
            payloads.push((cue.clone(), "cue", cue_text.into_bytes()));
            let files = payloads.into_iter().enumerate().map(|(index, (name, role, body))| {
                fs::write(path.join(&name), &body).unwrap();
                Payload {
                    id: format!("file{index}"), relative_path: name, bytes: body.len() as u64,
                    sha256: crate::network_transfer::digest_hex(Sha256::digest(&body).as_slice()), role: role.into(),
                    http_route: format!("/v1/files/{id}/{revision}/file{index}"),
                }
            }).collect();
            xml.push_str(&format!("<game><path>./{folder}</path><name>{title}</name></game>"));
            games.push(NetworkGame {
                id: id.into(), display_title: folder.into(), platform: "PS".into(), bundle_revision: revision,
                cache_path: format!("Roms/PS/{folder}"), game_path: format!("Roms/PS/{folder}/{cue}"),
                launch_path: format!("Roms/PS/{folder}/{cue}"), install_kind: "directory".into(), files, discs: vec![],
            });
            if !cached { fs::remove_dir_all(&path).unwrap(); }
        }
        xml.push_str("</gameList>");
        fs::write(ps.join("gamelist.xml"), xml).unwrap();
        let collection = ps.join("Collection");
        fs::create_dir_all(&collection).unwrap();
        for name in ["One.chd", "Two.chd"] { fs::write(collection.join(name), b"rom").unwrap(); }
        let _runtime = crate::network_runtime::tests::library_catalog_fixture(root, games.clone(), cached);
        for (game, title) in games.iter().zip(titles) {
            if cached {
                let path = root.join(&game.cache_path);
                fs::create_dir(path.join("Imgs")).unwrap();
                for name in ["cover.png", "alternate.png"] { fs::write(path.join("Imgs").join(name), b"art").unwrap(); }
                let cue = Path::new(&game.game_path).file_name().unwrap().to_str().unwrap();
                let child_xml = format!("<gameList><game><path>{cue}</path><name>{title}</name></game></gameList>");
                for name in ["gamelist.xml", "gamelist.xml.before-art2"] { fs::write(path.join(name), &child_xml).unwrap(); }
                fs::File::open(path).unwrap().set_modified(SystemTime::now() - std::time::Duration::from_secs(10)).unwrap();
            }
        }
        fs::File::open(&collection).unwrap().set_modified(SystemTime::now() - std::time::Duration::from_secs(10)).unwrap();
        let database = Database::in_memory().unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let directory = Directory::new(ps);
        let locale = Locale::new("en-US");
        common::media::with_name_indexes(|| common::media::check_directory(&directory.path));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        for (game, title) in games.iter().zip(titles) {
            while common::media::cached_name(&root.join(&game.game_path)).as_deref() != Some(title) {
                assert!(std::time::Instant::now() < deadline, "gamelist name index must finish");
                common::media::poll_name_indexes();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        invalidate_system_counts();
        for load in 0..2 {
            load_work::take();
            let entries = directory.entries(&database, &mapper, &locale).unwrap();
            assert_eq!(entries.len(), 3);
            for (descriptor, title) in games.iter().zip(titles) {
                let identity = root.join(if cached { &descriptor.cache_path } else { &descriptor.game_path });
                let entry = entries.iter().find(|entry| entry.path() == identity).unwrap();
                let Entry::Game(game) = entry else { panic!("single-disc catalog folder must collapse: {entry:?}"); };
                assert_eq!(game.name, title);
                assert!(crate::network_runtime::registered(&game.path));
                assert_eq!(crate::network_runtime::has_download(&game.path), !cached);
                assert_eq!(super::super::library::launch_path(&game.path, &mapper).unwrap(), root.join(&descriptor.launch_path));
                if !cached {
                    assert!(crate::network_runtime::guard_launch(&game.path).is_err());
                }
            }
            let Entry::Directory(folder) = entries.iter().find(|entry| entry.path() == collection).unwrap() else { panic!("two games must navigate"); };
            assert_eq!(folder.game_count, Some(2));
            assert_eq!(directory.cached_game_count(&mapper), 4, "catalog identities and physical copies count once");
            let work = load_work::take();
            if load == 1 {
                assert_eq!(work.directory_reads, 1, "warm navigation reuses inventories and counts");
                assert_eq!(work.descriptor_reads, 0, "warm navigation reuses cue references");
            }
        }
        drop(_runtime);
        if cached {
            invalidate_system_counts();
            let entries = directory.entries(&database, &mapper, &locale).unwrap();
            for (game, title) in games.iter().zip(titles) {
                assert!(entries.iter().any(|entry| matches!(entry, Entry::Game(local)
                    if local.path == root.join(&game.cache_path) && local.name == title)), "local-only folders also collapse");
            }
            assert_eq!(directory.cached_game_count(&mapper), 4);
        }
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn library_single_disc_local_and_catalog_folders_collapse() {
        single_disc_catalog_folders(true);
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn library_single_disc_uncached_catalog_folders_are_games() {
        single_disc_catalog_folders(false);
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn library_virtual_playstation_counts_local_catalog_and_multidisc_entries() {
        let (_fixture, game) = crate::network_runtime::tests::details_slot_fixture();
        let system = game.path.parent().unwrap();
        assert!(crate::network_runtime::virtual_directory(system));
        for name in ["Local.cue", "Local.chd", "Playlist.m3u"] { fs::write(system.join(name), b"fixture").unwrap(); }
        fs::create_dir(system.join("Multi.m3u")).unwrap();
        fs::write(system.join("Multi.m3u/Disc.chd"), b"fixture").unwrap();
        let mapper = ConsoleMapper::new();
        let mut entries = vec![Entry::Directory(Directory::new(system.to_path_buf()))];
        invalidate_system_counts();
        Directory::retain_populated_systems(&mut entries, &mapper);
        let Entry::Directory(directory) = &entries[0] else { panic!("system"); };
        assert_eq!(directory.game_count, Some(5), "catalog identity is counted once, including its physical copy");
        let before = super::super::load_work::take();
        assert_eq!(directory.cached_game_count(&mapper), 5);
        assert_eq!(super::super::load_work::take().directory_reads, 0, "cached root loads must not scan again: {before:?}");
        fs::remove_file(&game.path).unwrap();
        invalidate_system_counts();
        assert_eq!(directory.cached_game_count(&mapper), 5, "uncached catalog game remains listed");
    }

    #[test]
    fn library_hides_command_only_systems_and_retains_nested_game_counts() {
        let fixture = Fixture::new();
        for system in ["GG", "SFC"] {
            let directory = fixture.0.path.join(system);
            fs::create_dir_all(directory.join("Shortcuts")).unwrap();
            for name in ["~Filter.miyoocmd", "~Refresh roms.miyoocmd", "Shortcuts/~Import games.miyoocmd"] {
                fs::write(directory.join(name), b"command").unwrap();
            }
        }
        let sfc = fixture.0.path.join("SFC");
        fs::create_dir(sfc.join("ROMHACKS")).unwrap();
        fs::create_dir(sfc.join("media")).unwrap();
        fs::write(sfc.join("media/Art.png"), b"art").unwrap();
        fs::write(sfc.join("ROMHACKS/Real.sfc"), b"rom").unwrap();
        let database = Database::in_memory().unwrap();
        let mapper = ConsoleMapper::new();
        let mut entries = fixture.0.entries(&database, &mapper, &Locale::new("en-US")).unwrap();
        Directory::retain_populated_systems(&mut entries, &mapper);
        assert_eq!(entries.len(), 1);
        let Entry::Directory(system) = &entries[0] else { panic!("expected system"); };
        assert_eq!(system.path, sfc);
        assert_eq!(system.game_count, Some(1));
    }

    #[test]
    fn sidecars_and_onion_commands_are_filtered_from_scan_xml_and_database() {
        let fixture = Fixture::new();
        let database = Database::in_memory().unwrap();
        let mapper = ConsoleMapper::new();
        let names = ["~Filter.miyoocmd", "Refresh.MIYOOCMD", "~Import.sh", "metadata.xml", "systeminfo.txt", "metadata", "systeminfo", "icon.png", "Real.sfc", "~Hack.sfc"];
        let mut xml = "<gameList>".to_owned();
        for name in names {
            let path = fixture.0.path.join(name);
            fs::write(&path, b"fixture").unwrap();
            database.update_games(&[Game::new(path.clone()).into()]).unwrap();
            xml.push_str(&format!("<game><path>./{name}</path><name>{name}</name></game>"));
            assert_eq!(Game::from_db(database.select_game(&path).unwrap().unwrap()).is_some(), matches!(name, "Real.sfc" | "~Hack.sfc"));
        }
        use crate::entry::Sort;
        let recent = crate::view::recents::RecentsSort::Random.entries(&database, &mapper, &Locale::new("en-US")).unwrap();
        assert_eq!(recent.len(), 2);
        xml.push_str("</gameList>");
        fs::write(fixture.0.path.join("gamelist.xml"), xml).unwrap();
        let parsed = fixture.0.parse_game_list(&fixture.0.path.join("gamelist.xml")).unwrap();
        assert_eq!(parsed.len(), 2);
        let hidden = fixture.0.path.join("~Shortcuts");
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("Hidden.sfc"), b"rom").unwrap();
        assert!(Entry::new(hidden, &mapper).unwrap().is_none());
        for _ in 0..2 {
            let entries = fixture.0.entries(&database, &mapper, &Locale::new("en-US")).unwrap();
            assert_eq!(entries.len(), 2);
            for name in ["Real.sfc", "~Hack.sfc"] {
                assert!(entries.iter().any(|entry| entry.path() == fixture.0.path.join(name)));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn library_counts_linked_games_and_directories_without_cycles() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let targets = Fixture::new();
        let system = fixture.0.path.join("SFC");
        fs::create_dir(&system).unwrap();
        fs::create_dir(targets.0.path.join("Nested")).unwrap();
        fs::write(targets.0.path.join("Game.sfc"), b"rom").unwrap();
        fs::write(targets.0.path.join("Nested/Other.sfc"), b"rom").unwrap();
        symlink(targets.0.path.join("Game.sfc"), system.join("Linked.sfc")).unwrap();
        symlink(targets.0.path.join("Nested"), system.join("Folder")).unwrap();
        symlink(&system, targets.0.path.join("Nested/Back")).unwrap();
        symlink(targets.0.path.join("Missing.sfc"), system.join("Broken.sfc")).unwrap();
        let database = Database::in_memory().unwrap();
        let mapper = ConsoleMapper::new();
        let locale = Locale::new("en-US");
        let scanned = Directory::new(system.clone()).entries(&database, &mapper, &locale).unwrap();
        assert!(scanned.iter().any(|entry| matches!(entry, Entry::Game(game) if game.path == system.join("Linked.sfc"))));
        let mut root = fixture.0.entries(&database, &mapper, &locale).unwrap();
        Directory::retain_populated_systems(&mut root, &mapper);
        assert_eq!(root.len(), 1);
        let Entry::Directory(system) = &root[0] else { panic!("expected system"); };
        assert_eq!(system.game_count, Some(2));
    }

    #[test]
    fn library_counts_are_cached_across_root_loads_and_invalidated_by_mtime_and_rescan() {
        let fixture = Fixture::new();
        let system = Directory::new(fixture.0.path.join("SFC"));
        fs::create_dir(&system.path).unwrap();
        for i in 0..1700 {
            fs::write(system.path.join(format!("Game{i}.sfc")), b"rom").unwrap();
        }
        let database = Database::in_memory().unwrap();
        let mapper = ConsoleMapper::new();
        let locale = Locale::new("en-US");
        load_work::take();
        assert_eq!(system.walk_games(&mapper, &mut HashSet::new(), 1), 1);
        let probe = load_work::take();
        assert_eq!(probe.directory_reads, 1);
        assert_eq!(probe.file_types, 1, "emptiness must stop at the first playable entry");
        for load in 0..2 {
            load_work::take();
            let mut root = fixture.0.entries(&database, &mapper, &locale).unwrap();
            Directory::retain_populated_systems(&mut root, &mapper);
            let work = load_work::take();
            let Entry::Directory(directory) = &root[0] else { panic!("expected system"); };
            assert_eq!(directory.game_count, Some(1700));
            if load == 1 {
                assert_eq!(work.directory_reads, 1, "warm root load only reads the root directory");
                println!("SECOND_ROOT_LOAD read_dir={}", work.directory_reads);
            }
        }
        let stamp = fs::metadata(&system.path).unwrap().modified().unwrap();
        fs::create_dir(system.path.join("Nested")).unwrap();
        fs::write(system.path.join("Nested/Extra.sfc"), b"rom").unwrap();
        let changed = stamp + std::time::Duration::from_secs(2);
        File::open(&system.path).unwrap().set_times(fs::FileTimes::new().set_modified(changed)).unwrap();
        assert_eq!(system.cached_game_count(&mapper), 1701, "system mtime invalidates the cached count");
        fs::write(system.path.join("Nested/Another.sfc"), b"rom").unwrap();
        assert_eq!(system.cached_game_count(&mapper), 1701, "nested changes wait for an explicit refresh");
        system.populate_db(&mut VecDeque::new(), &database, &mapper, &locale).unwrap();
        assert_eq!(system.cached_game_count(&mapper), 1702, "existing database rescan invalidates counts");
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
                sql: 26,
                descriptor_reads: 0
            }
        );
        assert_eq!(
            new,
            load_work::Counts {
                directory_reads: 1,
                metadata: 0,
                file_types: 12,
                sql: 2,
                descriptor_reads: 0
            }
        );
    }
}
