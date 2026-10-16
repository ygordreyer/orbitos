pub mod app;
pub mod directory;
pub mod game;
mod gamelist;
pub mod lazy_image;

use std::ffi::OsStr;
use std::fmt::Debug;
use std::path::{Path, PathBuf};

use anyhow::Result;
use common::database::Database;
use common::locale::Locale;
use lazy_static::lazy_static;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::consoles::ConsoleMapper;
use crate::entry::app::App;
use crate::entry::directory::Directory;
use crate::entry::game::Game;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Entry {
    Directory(Directory),
    App(App),
    Game(Game),
}

impl Entry {
    pub fn new(path: PathBuf, console_mapper: &ConsoleMapper) -> Result<Option<Entry>> {
        // Don't add hidden files starting with . or _
        let file_name = match path.file_name().and_then(OsStr::to_str) {
            Some(file_name) => file_name,
            None => return Ok(None),
        };
        if file_name.starts_with('.') || file_name.starts_with('_') {
            return Ok(None);
        }

        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_owned();

        // Exclude Imgs and Guide directories
        if file_name == "media"
            || file_name == "Imgs"
            || file_name == "Guides"
            || file_name == "gamelist.xml"
            || file_name == "miyoogamelist.xml"
            || file_name == "neogeo.zip"
        {
            return Ok(None);
        }

        // Exclude DB
        const EXCLUDE_EXTENSIONS: [&str; 5] = [
            "db", "sbi", // PSX
            "nfo", // PICO-8
            "msu", // MSU-1
            "pcm", // MSU-1
        ];
        if EXCLUDE_EXTENSIONS.contains(&extension.as_str()) {
            return Ok(None);
        }

        if path.is_dir() {
            // Playlist folders retain game/save identity; app packages keep their rules.
            // Other directories may contain periods, such as a version suffix.
            if !extension.eq_ignore_ascii_case("m3u") && !extension.eq_ignore_ascii_case("pak") {
                return Ok(Some(Entry::Directory(
                    if let Some(console) = console_mapper.get_console_by_dir(&path) {
                        Directory::with_name(path, console.name.clone())
                    } else {
                        Directory::new(path)
                    },
                )));
            }

            // Apps are directories with .pak extension and have a config.json file inside
            if extension == "pak"
                && (path.join("config.json").exists() || path.join("launch.sh").exists())
            {
                return Ok(Some(Entry::App(App::new(path)?)));
            }
        }

        Ok(Some(Entry::Game(Game::new(path))))
    }

    pub fn path(&self) -> &Path {
        match self {
            Entry::Game(game) => &game.path,
            Entry::Directory(directory) => &directory.path,
            Entry::App(app) => &app.directory,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Entry::Game(game) => &game.name,
            Entry::Directory(directory) => &directory.name,
            Entry::App(app) => &app.name,
        }
    }

    pub fn image(&mut self) -> Option<&Path> {
        match self {
            Entry::Game(game) => game.image(),
            Entry::Directory(dir) => dir.image(),
            Entry::App(app) => app.image.as_deref(),
        }
    }
}

fn short_name(mut name: &str) -> String {
    // Remove the .p8 extension for .p8.png files
    if name.ends_with(".p8") {
        name = &name[..name.len() - 3]
    }

    // Remove numbers
    lazy_static! {
        static ref NUMBERS_RE: Regex = Regex::new(r"^\d+[.\)]").unwrap();
    }
    let mut name = NUMBERS_RE.replace(name, "").to_string();

    // Remove trailing parenthesis
    lazy_static! {
        static ref PARENTHESIS_RE: Regex = Regex::new(r"[\(\[].+[\)\]]$").unwrap();
    }
    name = PARENTHESIS_RE.replace(&name, "").to_string();

    // Trim whitespaces
    name = name.trim().to_owned();

    name
}

pub trait Sort: Debug + Clone {
    const HAS_BUTTON_HINTS: bool = true;
    fn button_hint(&self, locale: &Locale) -> String;
    fn next(&self) -> Self;
    fn with_directory(&self, directory: Directory) -> Self;
    fn entries(
        &self,
        database: &Database,
        console_mapper: &ConsoleMapper,
        locale: &Locale,
    ) -> Result<Vec<Entry>>;
    fn preserve_selection(&self) -> bool;
}

#[cfg(test)]
mod dotted_directory_tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "allium-directory-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn dotted_folder_navigates_and_preserves_full_name() {
        let fixture = Fixture::new();
        let path = fixture.0.join("Mega Man Legends 2 PSP Improvements (v1.1)");
        fs::create_dir(&path).unwrap();
        let cue = path.join("Mega Man Legends 2 (USA).cue");
        fs::write(&cue, b"FILE \"game.bin\" BINARY\n").unwrap();
        fs::write(path.join("gamelist.xml"), b"<gameList/>").unwrap();
        let entry = Entry::new(path.clone(), &ConsoleMapper::new())
            .unwrap()
            .unwrap();
        let Entry::Directory(directory) = entry else {
            panic!("folder must navigate, not launch")
        };
        assert_eq!(directory.path, path);
        assert_eq!(
            directory.full_name,
            "Mega Man Legends 2 PSP Improvements (v1.1)"
        );
        assert_eq!(directory.name, "Mega Man Legends 2 PSP Improvements");
        assert_eq!(
            Directory::with_name(path, "Custom label".into()).full_name,
            "Mega Man Legends 2 PSP Improvements (v1.1)"
        );
        let child = Entry::new(cue.clone(), &ConsoleMapper::new())
            .unwrap()
            .unwrap();
        assert!(matches!(child, Entry::Game(game) if game.path == cue));
    }

    #[test]
    fn playlist_folders_keep_game_path_and_save_identity() {
        let fixture = Fixture::new();
        for name in ["Legend of Dragoon, The (USA).m3u", "Case.M3U"] {
            let path = fixture.0.join(name);
            fs::create_dir(&path).unwrap();
            let entry = Entry::new(path.clone(), &ConsoleMapper::new())
                .unwrap()
                .unwrap();
            let Entry::Game(game) = entry else {
                panic!("playlist folder must stay a game")
            };
            assert_eq!(game.path, path);
            assert_eq!(game.full_name, path.file_stem().unwrap().to_str().unwrap());
        }
    }

    #[test]
    fn app_packages_and_excluded_entries_keep_behavior() {
        let fixture = Fixture::new();
        let valid = fixture.0.join("Tool.pak");
        fs::create_dir(&valid).unwrap();
        fs::write(valid.join("launch.sh"), b"#!/bin/sh\n").unwrap();
        assert!(matches!(
            Entry::new(valid, &ConsoleMapper::new()).unwrap(),
            Some(Entry::App(_))
        ));
        let invalid = fixture.0.join("Incomplete.pak");
        fs::create_dir(&invalid).unwrap();
        assert!(matches!(
            Entry::new(invalid, &ConsoleMapper::new()).unwrap(),
            Some(Entry::Game(_))
        ));
        for name in [".hidden.v1", "_hidden.v1", "Imgs", "Guides", "gamelist.xml"] {
            let path = fixture.0.join(name);
            fs::create_dir(&path).unwrap();
            assert!(Entry::new(path, &ConsoleMapper::new()).unwrap().is_none());
        }
    }
}
