use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail, ensure};
use common::constants::ALLIUM_SD_ROOT;
use serde::Serialize;

const MAX_PLAYLIST_BYTES: u64 = 64 * 1024;
const MAX_DISCS: usize = 8;

#[derive(Debug, Clone)]
pub struct Disc {
    pub path: PathBuf,
    pub available: bool,
}

#[derive(Debug, Clone)]
pub struct Playlist {
    pub path: PathBuf,
    pub discs: Vec<Disc>,
    pub core_compatible: bool,
}

pub fn content_path(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() || !is_playlist(path) {
        return Ok(path.to_path_buf());
    }
    let matching = path.join(path.file_name().unwrap());
    if matching.is_file() {
        return Ok(matching);
    }
    let mut candidates = fs::read_dir(path)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|entry| entry.is_file() && is_playlist(entry));
    let Some(playlist) = candidates.next() else {
        bail!("No playlist was found in this game folder");
    };
    ensure!(
        candidates.next().is_none(),
        "This folder has more than one playlist"
    );
    ensure!(
        playlist.file_stem() == path.file_stem(),
        "Playlist and folder names differ; shared save identity is ambiguous"
    );
    Ok(playlist)
}

fn is_playlist(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("m3u"))
}

pub fn discover(path: &Path) -> Result<Option<Playlist>> {
    if !is_playlist(path) {
        return Ok(None);
    }
    let path = content_path(path)?;
    let bytes = bounded_read(&path)?;
    let text = std::str::from_utf8(&bytes)?;
    let mut core_compatible = !text.starts_with('\u{feff}');
    let text = text.trim_start_matches('\u{feff}');
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut discs = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if raw.len() >= 1023 || raw.contains('\0') || raw.contains('\r') {
            core_compatible = false;
        }
        if line.is_empty() || line.starts_with('#') {
            if !raw.is_empty() && !raw.starts_with('#') {
                core_compatible = false;
            }
            continue;
        }
        if raw != line || Path::new(line).is_absolute() || line.len() >= 1023 {
            core_compatible = false;
        }
        let disc = parent.join(line);
        discs.push(Disc {
            available: disc.is_file(),
            path: disc,
        });
        ensure!(
            discs.len() <= MAX_DISCS,
            "This core supports at most eight discs"
        );
    }
    ensure!(!discs.is_empty(), "The playlist has no discs");
    Ok(Some(Playlist {
        path,
        discs,
        core_compatible,
    }))
}

fn bounded_read(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_PLAYLIST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_PLAYLIST_BYTES,
        "Playlist or startup settings are too large"
    );
    Ok(bytes)
}

fn config_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .filter_map(|line| {
            let (name, value) = line.split_once('=')?;
            (name.trim() == key).then(|| value.trim().trim_matches('"'))
        })
        .last()
}

fn save_directory(playlist: &Playlist) -> Result<PathBuf> {
    let config_root = ALLIUM_SD_ROOT.join("RetroArch/.retroarch");
    let bytes = bounded_read(&config_root.join("retroarch.cfg"))?;
    let text = std::str::from_utf8(&bytes)?;
    ensure!(
        !text
            .lines()
            .any(|line| line.trim_start().starts_with("#include")),
        "Included startup settings require review"
    );
    let expected = ALLIUM_SD_ROOT.join("Saves/CurrentProfile/saves");
    ensure!(
        config_value(text, "savefile_directory") == expected.to_str(),
        "Disc selection needs the reviewed shared save directory"
    );
    for key in ["sort_savefiles_enable", "sort_savefiles_by_content_enable"] {
        ensure!(
            config_value(text, key) == Some("false"),
            "Custom save sorting requires review before disc selection"
        );
    }
    ensure!(
        config_value(text, "savefiles_in_content_dir").is_none_or(|value| value == "false"),
        "Content-local saves require review before disc selection"
    );
    ensure!(
        config_value(text, "rgui_config_directory").is_none_or(|value| value.is_empty()
            || value == "default"
            || Some(value) == config_root.join("config").to_str()),
        "Custom override directories require review before disc selection"
    );
    let stem = playlist.path.file_stem().unwrap();
    let content_dir = playlist
        .path
        .parent()
        .and_then(Path::file_name)
        .unwrap_or_default();
    for name in [std::ffi::OsStr::new("PCSX-ReARMed"), content_dir, stem] {
        let mut filename = name.to_os_string();
        filename.push(".cfg");
        let override_path = config_root.join("config/PCSX-ReARMed").join(filename);
        ensure!(
            !override_path.exists(),
            "Core, directory or game overrides require review before disc selection"
        );
    }
    Ok(expected)
}

#[derive(Serialize)]
struct DiskIndex<'a> {
    version: &'static str,
    image_index: usize,
    image_path: &'a Path,
}

pub struct StartRecord {
    path: PathBuf,
    previous: Option<Vec<u8>>,
}

impl StartRecord {
    pub fn apply(playlist: &Playlist, index: usize) -> Result<Self> {
        ensure!(
            playlist.core_compatible,
            "This playlist needs BOM/whitespace repair for the bundled core"
        );
        let disc = playlist
            .discs
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("Invalid disc selection"))?;
        ensure!(
            disc.available && disc.path.is_file(),
            "This disc file is missing"
        );
        let directory = save_directory(playlist)?;
        let mut filename = playlist.path.file_stem().unwrap().to_os_string();
        filename.push(".ldci");
        let path = directory.join(filename);
        ensure!(
            !path.is_symlink(),
            "Disc startup record cannot be a symbolic link"
        );
        let previous = if path.exists() {
            Some(bounded_read(&path)?)
        } else {
            None
        };
        let bytes = serde_json::to_vec_pretty(&DiskIndex {
            version: "1.0",
            image_index: index,
            image_path: &disc.path,
        })?;
        atomic_write(&path, &bytes)?;
        Ok(Self { path, previous })
    }

    pub fn rollback(self) -> Result<()> {
        if let Some(bytes) = self.previous {
            atomic_write(&self.path, &bytes)
        } else {
            fs::remove_file(self.path).map_err(Into::into)
        }
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".orbit-disc-{}.tmp", std::process::id()));
    let mut created = false;
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        created = true;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() && created && temporary.exists() {
        fs::remove_file(temporary)?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orbit_disc_parser_preserves_order_comments_bom_and_missing_indices() {
        let root = std::env::temp_dir().join(format!("allium-disc-parser-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("Example.m3u");
        fs::write(root.join("one.chd"), b"fixture").unwrap();
        fs::write(
            &path,
            "\u{feff}#EXTM3U\r\none.chd\r\n# comment\r\ntwo.chd\r\n",
        )
        .unwrap();
        let playlist = discover(&path).unwrap().unwrap();
        assert_eq!(playlist.discs.len(), 2);
        assert!(playlist.discs[0].available);
        assert!(!playlist.discs[1].available);
        assert!(!playlist.core_compatible);
        fs::write(&path, "#EXTM3U\none.chd\n# comment\ntwo.chd\n").unwrap();
        assert!(discover(&path).unwrap().unwrap().core_compatible);
        fs::remove_file(&path).unwrap();
        fs::remove_file(root.join("one.chd")).unwrap();
        fs::remove_dir(root).unwrap();
    }
    #[test]
    fn orbit_disc_folder_resolution_and_parser_limits() {
        let root = std::env::temp_dir().join(format!("allium-disc-folders-{}", std::process::id()));
        let folder = root.join("Example.m3u");
        fs::create_dir_all(&folder).unwrap();
        let inner = folder.join("Example.m3u");
        fs::write(&inner, "one.chd\n").unwrap();
        fs::write(folder.join("Other.m3u"), "other.chd\n").unwrap();
        assert_eq!(
            content_path(&folder).unwrap(),
            inner,
            "matching basename is preferred"
        );
        fs::remove_file(&inner).unwrap();
        assert!(
            content_path(&folder).is_err(),
            "different basename changes shared-save identity"
        );
        fs::write(folder.join("Another.m3u"), "other.chd\n").unwrap();
        assert!(content_path(&folder).is_err(), "ambiguous folder must fail");
        fs::write(&inner, " one.chd \n").unwrap();
        assert!(!discover(&folder).unwrap().unwrap().core_compatible);
        fs::write(&inner, "/absolute.chd\n").unwrap();
        assert!(!discover(&folder).unwrap().unwrap().core_compatible);
        fs::write(
            &inner,
            (0..9).map(|i| format!("{i}.chd\n")).collect::<String>(),
        )
        .unwrap();
        assert!(discover(&folder).is_err());
        for file in [inner, folder.join("Other.m3u"), folder.join("Another.m3u")] {
            fs::remove_file(file).unwrap();
        }
        let temporary = root.join(format!(".orbit-disc-{}.tmp", std::process::id()));
        fs::write(&temporary, b"preexisting unrelated fixture").unwrap();
        assert!(atomic_write(&root.join("target.ldci"), b"new").is_err());
        assert_eq!(
            fs::read(&temporary).unwrap(),
            b"preexisting unrelated fixture"
        );
        fs::remove_file(temporary).unwrap();
        fs::remove_dir(folder).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
