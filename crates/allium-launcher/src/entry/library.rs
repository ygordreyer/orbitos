use std::{cell::RefCell, collections::{HashMap, HashSet}, fs, io::Read, path::{Path, PathBuf}, time::{Duration, SystemTime}};

use super::{Entry, directory::Directory};
use crate::consoles::ConsoleMapper;

struct Inventory {
    modified: Option<SystemTime>,
    generation: u64,
    stable: bool,
    entries: Vec<Entry>,
}

thread_local! {
    static REFERENCES: RefCell<HashMap<PathBuf, (Option<SystemTime>, u64, HashSet<std::ffi::OsString>)>> = RefCell::new(HashMap::new());
    static INVENTORIES: RefCell<HashMap<PathBuf, Inventory>> = RefCell::new(HashMap::new());
}

pub(super) fn invalidate() {
    INVENTORIES.with(|cache| cache.borrow_mut().clear());
    REFERENCES.with(|cache| cache.borrow_mut().clear());
}

fn stable_timestamp(modified: Option<SystemTime>) -> bool {
    modified.and_then(|stamp| SystemTime::now().duration_since(stamp).ok())
        .is_some_and(|age| age >= Duration::from_secs(3))
}

pub(super) fn remember(path: &Path, entries: &[Entry]) {
    let modified = fs::metadata(path).and_then(|metadata| metadata.modified()).ok();
    let generation = crate::network_runtime::art_path_generation();
    INVENTORIES.with(|cache| {
        let mut cache = cache.borrow_mut();
        // Reserve inventory space for the bounded descriptor cache on small devices.
        if cache.len() >= 256 || cache.values().map(|inventory| inventory.entries.len()).sum::<usize>() + entries.len() > 2048 {
            cache.clear();
        }
        if entries.len() <= 2048 {
            cache.insert(path.to_path_buf(), Inventory { modified, generation, stable: stable_timestamp(modified), entries: entries.to_vec() });
        }
    });
}

pub(super) fn inventory(path: &Path, mapper: &ConsoleMapper) -> Option<Vec<Entry>> {
    let modified = fs::metadata(path).and_then(|metadata| metadata.modified()).ok();
    let generation = crate::network_runtime::art_path_generation();
    if let Some(entries) = INVENTORIES.with(|cache| {
        cache.borrow().get(path).filter(|cached| cached.stable && cached.modified == modified && cached.generation == generation)
            .map(|cached| cached.entries.clone())
    }) {
        return Some(entries);
    }
    #[cfg(test)]
    super::load_work::record(|work| work.directory_reads += 1);
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).ok()?.filter_map(Result::ok) {
        let path = entry.path();
        if !super::listed_path(&path) { continue; }
        #[cfg(test)]
        super::load_work::record(|work| work.file_types += 1);
        let kind = match entry.file_type() { Ok(kind) => kind, Err(_) => continue };
        let is_dir = if kind.is_symlink() {
            match path.metadata() { Ok(metadata) => metadata.is_dir(), Err(_) => continue }
        } else { kind.is_dir() };
        if let Some(entry) = Entry::from_scanned_path(path, is_dir, mapper).ok().flatten() {
            entries.push(entry);
        }
    }
    remember(path, &entries);
    Some(entries)
}

pub(super) fn filter_games(entries: &mut Vec<Entry>, mapper: &ConsoleMapper) {
    entries.retain(|entry| super::listed_path(entry.path()) && match entry {
        Entry::Game(game) => mapper.accepts_game(&game.path),
        _ => true,
    });
    let mut referenced = HashSet::new();
    for entry in entries.iter() {
        let Entry::Game(game) = entry else { continue; };
        let cue = game.extension.eq_ignore_ascii_case("cue");
        if !cue && !game.extension.eq_ignore_ascii_case("m3u") { continue; }
        #[cfg(test)]
        super::load_work::record(|work| work.metadata += 1);
        let metadata = fs::metadata(&game.path).ok();
        if metadata.as_ref().is_some_and(|metadata| metadata.is_dir()) { continue; }
        let modified = metadata.as_ref().and_then(|metadata| metadata.modified().ok());
        let length = metadata.as_ref().map_or(0, fs::Metadata::len);
        if let Some(cached) = REFERENCES.with(|cache| cache.borrow().get(&game.path)
            .filter(|(stamp, size, _)| *stamp == modified && *size == length).map(|(_, _, refs)| refs.clone())) {
            referenced.extend(cached);
            continue;
        }
        let mut targets = HashSet::new();
        if let Ok(file) = fs::File::open(&game.path) {
            #[cfg(test)]
            super::load_work::record(|work| work.descriptor_reads += 1);
            let mut text = String::new();
            if file.take(64 * 1024 + 1).read_to_string(&mut text).is_ok() && text.len() <= 64 * 1024 {
                for line in text.trim_start_matches('\u{feff}').lines().map(str::trim) {
                    let target = if cue {
                        let Some(rest) = line.get(..4).filter(|prefix| prefix.eq_ignore_ascii_case("FILE"))
                            .and_then(|_| line.get(4..)).filter(|rest| rest.starts_with(char::is_whitespace)) else { continue; };
                        let rest = rest.trim_start();
                        if let Some(rest) = rest.strip_prefix('"') { rest.split('"').next().unwrap_or_default() }
                        else { rest.split_whitespace().next().unwrap_or_default() }
                    } else {
                        if line.is_empty() || line.starts_with('#') { continue; }
                        line
                    };
                    let parent = game.path.parent().unwrap_or_else(|| Path::new(""));
                    // Playlists commonly use Windows separators or a leading ./ on FAT32.
                    let target = target.replace('\\', "/");
                    let target: PathBuf = parent.join(target).components()
                        .filter(|component| !matches!(component, std::path::Component::CurDir)).collect();
                    targets.insert(target.as_os_str().to_ascii_lowercase());
                }
            }
        }
        REFERENCES.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() >= 256 || cache.values().map(|(_, _, refs)| refs.iter().map(|path| path.len()).sum::<usize>()).sum::<usize>()
                + targets.iter().map(|path| path.len()).sum::<usize>() > 64 * 1024 { cache.clear(); }
            if targets.iter().map(|path| path.len()).sum::<usize>() <= 64 * 1024 {
                cache.insert(game.path.clone(), (modified, length, targets.clone()));
            }
        });
        referenced.extend(targets);
    }

    entries.retain(|entry| !matches!(entry, Entry::Game(_))
        || !referenced.contains(&entry.path().as_os_str().to_ascii_lowercase()));
}

pub(crate) fn launch_path(path: &Path, mapper: &ConsoleMapper) -> anyhow::Result<PathBuf> {
    if !path.is_dir() || path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("m3u")) {
        return crate::discs::content_path(path);
    }
    let mut entries = inventory(path, mapper).ok_or_else(|| anyhow::anyhow!("Game folder is missing"))?;
    filter_games(&mut entries, mapper);
    anyhow::ensure!(entries.len() == 1, "Game folder no longer contains a single game");
    let Entry::Game(game) = &entries[0] else { anyhow::bail!("Game folder contains no launchable file"); };
    crate::discs::content_path(&game.path)
}

pub(crate) fn discover_discs(path: &Path, mapper: &ConsoleMapper) -> anyhow::Result<Option<crate::discs::Playlist>> {
    crate::discs::discover(&launch_path(path, mapper)?)
}

pub(super) fn resolve_folders(entries: &mut [Entry], mapper: &ConsoleMapper, database: &common::database::Database) -> anyhow::Result<()> {
    let mut collapsed = Vec::new();
    for (index, entry) in entries.iter_mut().enumerate() {
        let Entry::Directory(directory) = entry else { continue; };
        if mapper.library_console(&directory.path).is_none()
            || mapper.get_console_by_dir(&directory.path).is_some() { continue; }
        let Some(mut children) = inventory(&directory.path, mapper) else { continue; };
        filter_games(&mut children, mapper);
        if children.len() == 1 && let Entry::Game(game) = &children[0] {
            collapsed.push((index, game.clone(), directory.clone()));
        } else {
            directory.game_count = Some(directory.cached_game_count(mapper));
        }
    }
    if !collapsed.is_empty() {
        #[cfg(test)]
        super::load_work::record(|work| work.sql += 1);
        let paths: Vec<_> = collapsed.iter().flat_map(|(_, game, directory)| [directory.path.as_path(), game.path.as_path()]).collect();
        let mut rows = database.select_games(&paths)?.into_iter();
        for (index, child, directory) in collapsed {
            let folder = rows.next().flatten().map(super::game::Game::from_db_present)
                .or_else(|| directory.game.map(|game| *game));
            let mut game = rows.next().flatten().map(super::game::Game::from_db_present).unwrap_or(child);
            if let Some(folder) = folder {
                game.core = folder.core.filter(|value| !value.is_empty()).or(game.core);
                game.rating = folder.rating.or(game.rating);
                game.release_date = folder.release_date.or(game.release_date);
                game.developer = folder.developer.filter(|value| !value.is_empty()).or(game.developer);
                game.publisher = folder.publisher.filter(|value| !value.is_empty()).or(game.publisher);
                if !folder.genres.is_empty() { game.genres = folder.genres; }
                game.favorite = folder.favorite;
                game.screenshot_path = folder.screenshot_path.or(game.screenshot_path);
            }
            game.path = directory.path;
            game.name = directory.name;
            game.full_name = game.name.clone();
            game.image = directory.image;
            entries[index] = Entry::Game(game);
        }
    }
    Ok(())
}

pub(super) fn count_entries(directory: &Directory, mapper: &ConsoleMapper) -> Vec<Entry> {
    let mut entries = inventory(&directory.path, mapper).unwrap_or_default();
    crate::network_runtime::merge_directory(&directory.path, &mut entries);
    filter_games(&mut entries, mapper);
    let mut seen = HashSet::new();
    entries.retain(|entry| seen.insert(entry.path().to_path_buf()));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Sort, directory::list_tests::Fixture, game::Game, load_work};
    use common::{database::Database, locale::Locale};

    #[test]
    fn syncthing_conflicts_and_control_directories_never_enter_library_sources() {
        let fixture = Fixture::new();
        let database = Database::in_memory().unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let locale = Locale::new("en-US");
        let gb = fixture.0.path.join("GB");
        fs::create_dir(&gb).unwrap();
        let names = [
            "Gargoyle's Quest II - The Demon Darkness (Japan) [T-En by Bailli v1.0] [n].sync-conflict-20250415-202527-JENTCHY.zip",
            "systeminfo.sync-conflict-19700102-180230-L4ZYQW7.txt",
            ".stfolder/Hidden.gb", ".stversions/Old.gb", "Real.gb",
        ];
        let mut xml = "<gameList>".to_owned();
        for name in names {
            let path = gb.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"rom").unwrap();
            database.increment_play_count(&Game::new(path.clone()).into()).unwrap();
            xml.push_str(&format!("<game><path>{name}</path><name>Fixture</name></game>"));
            assert_eq!(Game::from_db(database.select_game(&path).unwrap().unwrap()).is_some(), name == "Real.gb");
        }
        xml.push_str("</gameList>");
        fs::write(gb.join("gamelist.xml"), xml).unwrap();
        let directory = Directory::new(gb.clone());
        assert_eq!(directory.parse_game_list(&gb.join("gamelist.xml")).unwrap().len(), 1);
        for _ in 0..2 {
            let entries = directory.entries(&database, &mapper, &locale).unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].path(), gb.join("Real.gb"));
        }
        assert_eq!(directory.cached_game_count(&mapper), 1);
        for sort in [crate::view::recents::RecentsSort::Random, crate::view::recents::RecentsSort::LastPlayed] {
            assert_eq!(sort.entries(&database, &mapper, &locale).unwrap().len(), 1);
        }
        for path in [gb.join(".stfolder"), gb.join(".stversions"), gb.join("Missing.sync-conflict-123.zip")] {
            load_work::take();
            assert!(Entry::new(path, &mapper).unwrap().is_none());
            assert_eq!(load_work::take().metadata, 0);
        }
    }

    #[test]
    fn single_game_folders_resolve_cue_multitrack_and_m3u_directory_layouts() {
        let fixture = Fixture::new();
        let ps = fixture.0.path.join("PS");
        let echo = "Echo Night (USA)";
        let alundra = "Alundra (USA) (Rev 1)";
        let chaos = "Chaos Break (Europe) (En,Fr,De,It)";
        for name in [echo.to_owned(), format!("{alundra}.m3u"), chaos.to_owned(), "Collection".into(), "Imgs".into(), "media".into()] {
            fs::create_dir_all(ps.join(name)).unwrap();
        }
        fs::write(ps.join(format!("{echo}/{echo}.bin")), b"rom").unwrap();
        fs::write(ps.join(format!("{echo}/{echo}.cue")), format!("FILE \"{echo}.bin\" BINARY\n")).unwrap();
        fs::write(ps.join(format!("{alundra}.m3u/{alundra}.chd")), b"rom").unwrap();
        fs::write(ps.join(format!("{alundra}.m3u/{alundra}.m3u")), format!("{alundra}.chd\n")).unwrap();
        for track in 1..=2 {
            fs::write(ps.join(format!("{chaos}/{chaos} (Track {track}).bin")), b"rom").unwrap();
        }
        fs::write(ps.join(format!("{chaos}/{chaos}.cue")), format!("FILE \"{chaos} (Track 1).bin\" BINARY\n  TRACK 01 MODE2/2352\nFILE \"{chaos} (Track 2).bin\" BINARY\n  TRACK 02 AUDIO\n")).unwrap();
        fs::write(ps.join(format!("{echo}/Readme.pdf")), b"sidecar").unwrap();
        fs::write(ps.join(format!("{echo}/Other.sync-conflict-123.chd")), b"conflict").unwrap();
        fs::write(ps.join("Collection/One.chd"), b"rom").unwrap();
        fs::write(ps.join("Collection/Two.chd"), b"rom").unwrap();
        for name in [echo, &format!("{alundra}.m3u"), chaos, "Collection"] {
            fs::File::open(ps.join(name)).unwrap().set_modified(SystemTime::now() - Duration::from_secs(10)).unwrap();
        }
        let database = Database::in_memory().unwrap();
        let cue = ps.join(format!("{echo}/{echo}.cue"));
        let mut saved: common::database::NewGame = Game::new(cue.clone()).into();
        saved.name = "Scraped Echo Night".into();
        saved.image = Some(ps.join("Imgs/custom-cover.png"));
        saved.core = Some("custom-core".into());
        saved.rating = Some(9);
        database.update_games(&[saved]).unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let locale = Locale::new("en-US");
        let directory = Directory::new(ps.clone());
        for load in 0..2 {
            load_work::take();
            let entries = directory.entries(&database, &mapper, &locale).unwrap();
            let work = load_work::take();
            assert_eq!(entries.len(), 4);
            for expected in [ps.join(echo), ps.join(format!("{alundra}.m3u")), ps.join(chaos)] {
                assert!(entries.iter().any(|entry| matches!(entry, Entry::Game(game) if game.path == expected)));
            }
            let Entry::Game(saved) = entries.iter().find(|entry| entry.path() == ps.join(echo)).unwrap() else { panic!("single game must retain metadata"); };
            assert_eq!(saved.name, "Echo Night");
            assert_eq!(saved.image, super::super::lazy_image::LazyImage::Unknown(ps.join(echo)));
            assert_eq!(saved.core.as_deref(), Some("custom-core"));
            assert_eq!(saved.rating, Some(9));
            let Entry::Directory(collection) = entries.iter().find(|entry| entry.path() == ps.join("Collection")).unwrap() else { panic!("multi-game folders must navigate"); };
            assert_eq!(collection.game_count, Some(2));
            if load == 1 { assert_eq!(work.directory_reads, 1, "warm loads reuse child inventories and counts"); }
        }
        assert_eq!(directory.cached_game_count(&mapper), 5);
        let playlist = ps.join(format!("{alundra}.m3u"));
        assert_eq!(crate::discs::content_path(&playlist).unwrap(), playlist.join(format!("{alundra}.m3u")));
    }

    #[test]
    fn playlist_references_win_without_hiding_an_unrelated_game_and_counts_refresh() {
        let fixture = Fixture::new();
        let ps = fixture.0.path.join("PS");
        let folder = ps.join("Collection");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("Disc.bin"), b"rom").unwrap();
        fs::write(folder.join("Disc.cue"), "FILE \"Disc.bin\" BINARY\n").unwrap();
        fs::write(folder.join("Set.m3u"), "#EXTM3U\n./Disc.cue\n").unwrap();
        fs::write(folder.join("Other.chd"), b"rom").unwrap();
        let database = Database::in_memory().unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let locale = Locale::new("en-US");
        let directory = Directory::new(ps);
        let entries = directory.entries(&database, &mapper, &locale).unwrap();
        let Entry::Directory(collection) = &entries[0] else { panic!("unrelated game prevents collapse"); };
        assert_eq!(collection.game_count, Some(2));
        assert_eq!(collection.entries(&database, &mapper, &locale).unwrap().len(), 2);
        fs::remove_file(folder.join("Other.chd")).unwrap();
        super::super::directory::invalidate_system_counts();
        let entries = directory.entries(&database, &mapper, &locale).unwrap();
        assert!(matches!(&entries[0], Entry::Game(game) if game.path == folder));
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use crate::entry::{Sort, directory::list_tests::Fixture, game::Game, load_work};
    use common::{database::Database, locale::Locale, media::{self, Kind}};

    fn age(path: &Path) {
        fs::File::open(path).unwrap().set_modified(SystemTime::now() - Duration::from_secs(10)).unwrap();
    }

    #[test]
    fn collapsed_folder_uses_system_gamelist_name_and_all_art() {
        let fixture = Fixture::new();
        let ps = fixture.0.path.join("Roms/PS");
        let folder = ps.join("B. L. U. E. - Legend of Water (Japan)");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("disc.bin"), b"rom").unwrap();
        fs::write(folder.join("disc.cue"), "FILE \"disc.bin\" BINARY\n").unwrap();
        for (kind, ext) in [("covers", "jpg"), ("logos", "png"), ("backgrounds", "jpg")] {
            fs::create_dir_all(ps.join("media").join(kind)).unwrap();
            fs::write(ps.join(format!("media/{kind}/custom.{ext}")), b"art").unwrap();
        }
        fs::write(ps.join("gamelist.xml"), "<gameList><game><path>./B. L. U. E. - Legend of Water (Japan)</path><name>B.L.U.E.: Legend of Water</name><desc>Explore the ocean.</desc><rating>0.7</rating><releasedate>19980709T000000</releasedate><developer>Hudson</developer><publisher>Hudson Soft</publisher><genre>Adventure, Puzzle</genre><players>1</players><image>./media/covers/custom.jpg</image><marquee>./media/logos/custom.png</marquee><fanart>./media/backgrounds/custom.jpg</fanart></game></gameList>").unwrap();
        let database = Database::in_memory().unwrap();
        let mut child = Game::new(folder.join("disc.cue"));
        child.core = Some("child-core".into());
        child.rating = Some(2);
        child.release_date = chrono::NaiveDate::from_ymd_opt(2000, 1, 1);
        child.developer = Some("Child developer".into());
        child.publisher = Some("Child publisher".into());
        child.genres = vec!["Child genre".into()];
        database.update_games(&[child.into()]).unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let locale = Locale::new("en-US");
        for _ in 0..2 {
            let mut entries = Directory::new(ps.clone()).entries(&database, &mapper, &locale).unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].path(), folder);
            assert_eq!(entries[0].name(), "B.L.U.E.: Legend of Water");
            assert_eq!(entries[0].image(), Some(ps.join("media/covers/custom.jpg").as_path()));
            let Entry::Game(game) = &entries[0] else { panic!("folder must collapse"); };
            let row = database.select_game(&folder).unwrap().unwrap();
            for game in [game, &Game::from_db_present(row)] {
                assert_eq!(game.rating, Some(7));
                assert_eq!(game.release_date, chrono::NaiveDate::from_ymd_opt(1998, 7, 9));
                assert_eq!(game.developer.as_deref(), Some("Hudson"));
                assert_eq!(game.publisher.as_deref(), Some("Hudson Soft"));
                assert_eq!(game.genres, ["Adventure", "Puzzle"]);
            }
            assert_eq!(game.core.as_deref(), Some("child-core"));
            let metadata = media::metadata(&folder);
            assert_eq!(metadata.desc, "Explore the ocean.");
            assert_eq!(metadata.rating, "0.7");
            assert_eq!(metadata.releasedate, "19980709T000000");
            assert_eq!(metadata.developer, "Hudson");
            assert_eq!(metadata.publisher, "Hudson Soft");
            assert_eq!(metadata.genre, "Adventure, Puzzle");
            assert_eq!(metadata.players, "1");
            let details = crate::view::details::Details::new(game, "PlayStation");
            assert_eq!(details.blurb, "Explore the ocean.");
            let snapshot = crate::view::details::DetailsSnapshot::cached(game).unwrap();
            let cached = crate::view::details::Details::from_snapshot(game, "PlayStation", snapshot);
            assert_eq!(cached.blurb, details.blurb);
            assert_eq!(cached.facts, details.facts);
            for (key, value) in [("Developer", "Hudson"), ("Publisher", "Hudson Soft"), ("Genres", "Adventure, Puzzle"), ("Players", "1")] {
                assert!(details.facts.contains(&(key.into(), value.into())), "{key}");
            }
            for (kind, file) in [(Kind::Logo, "logos/custom.png"), (Kind::Background, "backgrounds/custom.jpg")] {
                assert_eq!(media::resolve(entries[0].path(), kind), Some(ps.join("media").join(file)));
            }
            assert_eq!(launch_path(entries[0].path(), &mapper).unwrap(), folder.join("disc.cue"));
        }
        let search = database.search("B.L.U.E.", 10).unwrap();
        assert_eq!(search.len(), 1);
        assert_eq!(search[0].name, "B.L.U.E.: Legend of Water");
        assert_eq!(search[0].path, folder);
    }

    #[test]
    fn collapsed_folder_prefers_saved_folder_fields_and_falls_back_to_child() {
        let fixture = Fixture::new();
        let ps = fixture.0.path.join("Roms/PS");
        let folder = ps.join("Folder Title");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("disc.chd"), b"rom").unwrap();
        let database = Database::in_memory().unwrap();
        let mut child = Game::new(folder.join("disc.chd"));
        child.core = Some("child-core".into());
        child.rating = Some(9);
        child.release_date = chrono::NaiveDate::from_ymd_opt(1999, 1, 2);
        child.developer = Some("Child developer".into());
        child.publisher = Some("Child publisher".into());
        child.genres = vec!["Child genre".into()];
        let mut saved = Game::new(folder.clone());
        saved.name = "Saved folder".into();
        saved.core = Some("folder-core".into());
        saved.publisher = Some(String::new());
        saved.favorite = true;
        database.update_games(&[child.clone().into(), saved.clone().into()]).unwrap();
        database.set_favorite(&saved.into()).unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let locale = Locale::new("en-US");
        for _ in 0..2 {
            let entries = Directory::new(ps.clone()).entries(&database, &mapper, &locale).unwrap();
            let [Entry::Game(game)] = entries.as_slice() else { panic!("folder must collapse"); };
            assert_eq!(game.path, folder);
            assert_eq!(game.name, "Saved folder");
            assert_eq!(game.core.as_deref(), Some("folder-core"));
            assert!(game.favorite);
            assert_eq!(game.rating, child.rating);
            assert_eq!(game.release_date, child.release_date);
            assert_eq!(game.developer, child.developer);
            assert_eq!(game.publisher, child.publisher);
            assert_eq!(game.genres, child.genres);
            assert_eq!(game.image, super::super::lazy_image::LazyImage::Unknown(folder.clone()));
        }
        fs::write(folder.join("other.chd"), b"rom").unwrap();
        super::super::directory::invalidate_system_counts();
        let entries = Directory::new(ps).entries(&database, &mapper, &locale).unwrap();
        let [Entry::Directory(directory)] = entries.as_slice() else { panic!("multi-game folder must navigate"); };
        assert_eq!(directory.entries(&database, &mapper, &locale).unwrap().len(), 2);
    }

    #[test]
    fn collapsed_folder_falls_back_to_folder_stem_and_legacy_or_media_art() {
        let fixture = Fixture::new();
        let ps = fixture.0.path.join("Roms/PS");
        for (name, art) in [("Folder Title", "Imgs/Folder Title.png"), ("Playlist Title", "media/covers/Playlist Title.jpg")] {
            let folder = ps.join(name);
            fs::create_dir_all(&folder).unwrap();
            let file = if name == "Folder Title" { "disc.cue" } else { "disc.m3u" };
            fs::write(folder.join(file), b"disc.bin\n").unwrap();
            let art = ps.join(art);
            fs::create_dir_all(art.parent().unwrap()).unwrap();
            fs::write(art, b"art").unwrap();
        }
        let database = Database::in_memory().unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let mut entries = Directory::new(ps.clone()).entries(&database, &mapper, &Locale::new("en-US")).unwrap();
        assert_eq!(entries.len(), 2);
        entries.sort();
        assert_eq!(entries[0].name(), "Folder Title");
        assert_eq!(entries[0].image(), Some(ps.join("Imgs/Folder Title.png").as_path()));
        assert_eq!(launch_path(entries[0].path(), &mapper).unwrap(), ps.join("Folder Title/disc.cue"));
        assert_eq!(entries[1].name(), "Playlist Title");
        assert_eq!(entries[1].image(), Some(ps.join("media/covers/Playlist Title.jpg").as_path()));
        assert_eq!(launch_path(entries[1].path(), &mapper).unwrap(), ps.join("Playlist Title/disc.m3u"));
    }

    #[test]
    fn rescan_traverses_collapsed_and_playlist_folders_and_preserves_folder_history() {
        let fixture = Fixture::new();
        let ps = fixture.0.path.join("PS");
        let folder = ps.join("Folder Title");
        let playlist = ps.join("Alundra.m3u");
        for (path, child) in [(&folder, "disc.cue"), (&playlist, "Alundra.m3u")] {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join(child), b"disc.bin\n").unwrap();
            fs::write(path.join("gamelist.xml"), format!("<gameList><game><path>./{child}</path><name>Local Metadata</name><developer>Local Studio</developer></game></gameList>")).unwrap();
        }
        let database = Database::in_memory().unwrap();
        database.increment_play_count(&Game::new(folder.clone()).into()).unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let locale = Locale::new("en-US");
        for _ in 0..2 {
            database.delete_all_directories().unwrap();
            database.delete_all_unplayed_games().unwrap();
            let mut queue = std::collections::VecDeque::from([Directory::new(ps.clone())]);
            while let Some(directory) = queue.pop_front() {
                directory.populate_db(&mut queue, &database, &mapper, &locale).unwrap();
            }
            for (path, child) in [(&folder, "disc.cue"), (&playlist, "Alundra.m3u")] {
                let row = database.select_game(&path.join(child)).unwrap().unwrap();
                assert_eq!(row.name, "Local Metadata");
                assert_eq!(row.developer.as_deref(), Some("Local Studio"));
                assert_eq!(launch_path(path, &mapper).unwrap(), path.join(child));
            }
            let row = database.select_game(&folder).unwrap().unwrap();
            assert_eq!(row.play_count, 1);
            assert_eq!(row.name, "Folder Title");
            let recent = crate::view::recents::RecentsSort::LastPlayed.entries(&database, &mapper, &locale).unwrap();
            assert_eq!(recent.len(), 1);
            assert_eq!(recent[0].path(), folder);
            assert_eq!(recent[0].name(), "Folder Title");
        }
    }

    #[test]
    fn collapsed_playlist_keeps_disc_picker_and_inner_content_path() {
        let fixture = Fixture::new();
        let folder = fixture.0.path.join("PS/Playlist Title");
        fs::create_dir_all(&folder).unwrap();
        for disc in ["one.chd", "two.chd"] { fs::write(folder.join(disc), b"rom").unwrap(); }
        fs::write(folder.join("disc.m3u"), "one.chd\ntwo.chd\n").unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let playlist = discover_discs(&folder, &mapper).unwrap().unwrap();
        assert_eq!(playlist.path, folder.join("disc.m3u"));
        assert_eq!(playlist.discs.len(), 2);
        assert_eq!(playlist.discs[0].path, folder.join("one.chd"));
        assert_eq!(launch_path(&folder, &mapper).unwrap(), playlist.path);
    }

    #[test]
    fn racy_inventory_mtime_is_reread_and_explicit_refresh_clears_stable_inventory() {
        let fixture = Fixture::new();
        let folder = fixture.0.path.join("PS/Folder");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("one.chd"), b"rom").unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let stamp = SystemTime::now() - Duration::from_secs(1);
        fs::File::open(&folder).unwrap().set_modified(stamp).unwrap();
        assert_eq!(inventory(&folder, &mapper).unwrap().len(), 1);
        fs::write(folder.join("two.chd"), b"rom").unwrap();
        fs::File::open(&folder).unwrap().set_modified(stamp).unwrap();
        assert_eq!(inventory(&folder, &mapper).unwrap().len(), 2);
        age(&folder);
        assert_eq!(inventory(&folder, &mapper).unwrap().len(), 2);
        load_work::take();
        assert_eq!(inventory(&folder, &mapper).unwrap().len(), 2);
        assert_eq!(load_work::take().directory_reads, 0);
        let stamp = fs::metadata(&folder).unwrap().modified().unwrap();
        fs::write(folder.join("three.chd"), b"rom").unwrap();
        fs::File::open(&folder).unwrap().set_modified(stamp).unwrap();
        super::super::directory::invalidate_system_counts();
        assert_eq!(inventory(&folder, &mapper).unwrap().len(), 3);
    }

    #[test]
    fn descriptor_reads_are_cached_for_merged_rows_counts_and_warm_reload() {
        let fixture = Fixture::new();
        let ps = fixture.0.path.join("PS");
        fs::create_dir(&ps).unwrap();
        let bin = ps.join("disc.bin");
        fs::write(&bin, b"rom").unwrap();
        fs::write(ps.join("disc.cue"), "FILE \"disc.bin\" BINARY\n").unwrap();
        fs::write(ps.join("set.m3u"), "disc.cue\n").unwrap();
        let database = Database::in_memory().unwrap();
        database.update_games(&[Game::new(bin).into()]).unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let directory = Directory::new(ps.clone());
        let locale = Locale::new("en-US");
        load_work::take();
        let entries = directory.entries(&database, &mapper, &locale).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path(), ps.join("set.m3u"));
        assert_eq!(load_work::take().descriptor_reads, 2);
        assert_eq!(count_entries(&directory, &mapper).len(), 1);
        assert_eq!(load_work::take().descriptor_reads, 0);
        assert_eq!(directory.entries(&database, &mapper, &locale).unwrap(), entries);
        assert_eq!(load_work::take().descriptor_reads, 0);
        let cue = ps.join("disc.cue");
        fs::File::open(&cue).unwrap().set_modified(SystemTime::now() + Duration::from_secs(1)).unwrap();
        assert_eq!(directory.entries(&database, &mapper, &locale).unwrap(), entries);
        assert_eq!(load_work::take().descriptor_reads, 1);
    }
}
