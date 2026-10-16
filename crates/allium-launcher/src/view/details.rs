//! Read-only selected-game information. Launch authority remains in EntryList.
use crate::entry::game::Game;
use serde::Deserialize;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(crate) struct Details {
    pub metadata: String,
    pub metadata_runs: Vec<(String, String)>,
    pub facts: Vec<(String, String)>,
    pub source: String,
    ink_body: String,
    pub blurb: String,
    pub pages: Vec<String>,
    pub page: usize,
    full_text: String,
}
impl Details {
    pub fn new(game: &Game, platform: &str) -> Self {
        let fallback = fallback(game);
        Self::from_metadata(game, platform, synopsis(&game.path), fallback.as_ref())
    }
    fn from_metadata(
        game: &Game,
        platform: &str,
        local_synopsis: Option<String>,
        fallback: Option<&MetadataEntry>,
    ) -> Self {
        let mut fields = vec![platform.to_owned()];
        if let Some(date) = game.release_date {
            fields.push(format!("Released {}", date.format("%Y")));
        } else if let Some(entry) = fallback {
            fields.push(format!(
                "Released {} ({})",
                entry.year, entry.release_region
            ));
        } else {
            fields.push("Year unavailable".into());
        }
        if let Some(value) = game
            .developer
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| fallback.map(|e| e.developer.as_str()))
        {
            fields.push(format!("Developer: {value}"));
        } else {
            fields.push("Developer: unavailable".into());
        }
        if let Some(value) = game
            .publisher
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| fallback.map(|e| e.publisher.as_str()))
        {
            fields.push(format!("Publisher: {value}"));
        } else {
            fields.push("Publisher: unavailable".into());
        }
        let genres = if game.genres.is_empty() {
            fallback.map(|e| e.genres.as_slice()).unwrap_or(&[])
        } else {
            &game.genres
        };
        if !genres.is_empty() {
            fields.push(genres.join(", "));
        } else {
            fields.push("Genre: unavailable".into());
        }
        if let Some(value) = game.rating {
            fields.push(format!("Metadata rating: {value}/10"));
        }
        let has_local_synopsis = local_synopsis.is_some();
        let synopsis = local_synopsis
            .or_else(|| {
                fallback
                    .map(|e| e.description.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "Synopsis unavailable.".into());
        let blurb = synopsis.chars().take(480).collect();
        let summary_count = 2;
        let metadata: String = fields[..summary_count]
            .join(" · ")
            .chars()
            .take(480)
            .collect();
        let facts = fields[summary_count..].join("\n");
        let mut full_text = if facts.is_empty() {
            synopsis
        } else {
            format!("{facts}\n\n{synopsis}")
        };
        if let Some(entry) = fallback {
            full_text.push_str(&format!(
                "\n\nFirst release: {}\nMetadata source: {}\n{}",
                entry.first_release_year,
                entry.source.title,
                display_url(&entry.source.url)
            ));
            if !has_local_synopsis {
                full_text.push_str(&format!(
                    "\n{}\n{}",
                    entry.source.license.name,
                    display_url(&entry.source.license.url)
                ));
            }
        } else {
            full_text.push_str("\n\nMetadata source: unavailable\nLicense: unavailable");
        }
        let mut fact_runs=vec![];
        if let Some(value)=game.developer.as_deref().filter(|v|!v.is_empty()).or_else(||fallback.map(|e|e.developer.as_str())) {fact_runs.push(("Developer".into(),value.into()));}
        if let Some(value)=game.publisher.as_deref().filter(|v|!v.is_empty()).or_else(||fallback.map(|e|e.publisher.as_str())) {fact_runs.push(("Publisher".into(),value.into()));}
        if !genres.is_empty(){fact_runs.push(("Genres".into(),genres.join(", ")));}
        if let Some(entry)=fallback {fact_runs.push(("First release".into(),entry.first_release_year.to_string()));}
        let source=fallback.map(|e|format!("Source: {}",e.source.title)).unwrap_or_default();
        let ink_body=if facts.is_empty(){full_text.clone()}else{full_text.strip_prefix(&(facts+"\n\n")).unwrap_or(&full_text).into()};
        Self {
            metadata,
            metadata_runs: vec![("Platform".into(),platform.into()),("Released".into(),fields[1].trim_start_matches("Released ").into())],
            facts: fact_runs, source, ink_body,
            blurb,
            pages: pages(&full_text),
            page: 0,
            full_text,
        }
    }
    pub fn add_history(&mut self, count: i64, minutes: u64) {
        self.full_text = format!(
            "{}\n\nPlayed {count} times · {minutes} minutes",
            self.full_text
        );
    }
    pub fn reflow(&mut self, handle: &common::theme::ThemeHandle) {
        use common::theme::pack::{Binding, ScreenId};
        let Some(slot) = handle.pack().screens[&ScreenId::GameDetail]
            .components
            .iter()
            .find(|c| c.binding == Some(Binding::SelectedBlurb))
        else {
            return;
        };
        let role = slot.role.unwrap();
        let style = handle.component_text_style(slot, common::display::color::Color::new(0, 0, 0));
        let line=if handle.pack().ink(){slot.paint.as_ref().and_then(|p|p.run.as_ref()).and_then(|r|r.line_height).unwrap_or(handle.pack().tokens.typography[&role].line_height)}else{handle.pack().tokens.typography[&role].line_height};
        let limit = (slot.rect.3 / line)
            .min(slot.max_lines.unwrap())
            .saturating_sub(1)
            .max(1) as usize;
        let body=if handle.pack().ink()&&handle.pack().screens[&ScreenId::GameDetail].components.iter().any(|c|c.binding==Some(Binding::SelectedFacts)){&self.ink_body}else{&self.full_text};
        let lines = common::theme::render::wrap(&style, body, slot.rect.2, 8192);
        self.pages = pages_from_lines(lines, limit, 480);
        if self.pages.is_empty() {
            self.pages.push("Synopsis unavailable.".into());
        }
        self.page = self.page.min(self.pages.len() - 1);
    }
    pub fn text(&self) -> String {
        format!(
            "{}\nPage {}/{}",
            self.pages[self.page],
            self.page + 1,
            self.pages.len()
        )
    }
    pub fn turn(&mut self, forward: bool) {
        self.page = if forward {
            (self.page + 1).min(self.pages.len() - 1)
        } else {
            self.page.saturating_sub(1)
        };
    }
}

// Preserve the canonical URL after whitespace removal while giving the text
// layout legal, readable break points between URL path segments.
fn display_url(url: &str) -> String {
    let Some((scheme, remainder)) = url.split_once("://") else {
        return url.to_owned();
    };
    format!("{scheme}://{}", remainder.replace('/', "/ "))
}

fn pages_from_lines(lines: Vec<String>, limit: usize, max_chars: usize) -> Vec<String> {
    let mut pages = Vec::<Vec<String>>::new();
    for line in lines {
        let pieces: Vec<String> = if line.is_empty() {
            vec![String::new()]
        } else {
            line.chars()
                .collect::<Vec<_>>()
                .chunks(480)
                .map(|chunk| chunk.iter().collect())
                .collect()
        };
        for piece in pieces {
            let next_len = pages
                .last()
                .map(|page| {
                    page.iter().map(|line| line.chars().count()).sum::<usize>()
                        + page.len()
                        + piece.chars().count()
                })
                .unwrap_or(0);
            if pages
                .last()
                .is_none_or(|page| page.len() == limit || next_len > max_chars)
            {
                pages.push(Vec::new());
            }
            pages.last_mut().unwrap().push(piece);
        }
    }
    if pages.len() > 1 {
        let terminal_is_orphan = pages
            .last()
            .is_some_and(|page| page.len() == 1 && page[0].trim().chars().count() < 12);
        if terminal_is_orphan {
            let prior = pages.len() - 2;
            if pages[prior].len() > 1 {
                let moved = pages[prior].pop().unwrap();
                let target_len = pages
                    .last()
                    .unwrap()
                    .iter()
                    .map(|line| line.chars().count())
                    .sum::<usize>()
                    + pages.last().unwrap().len()
                    + moved.chars().count();
                if target_len <= max_chars {
                    pages.last_mut().unwrap().insert(0, moved);
                } else {
                    pages[prior].push(moved);
                }
            }
        }
    }
    pages.into_iter().map(|page| page.join("\n")).collect()
}
fn pages(text: &str) -> Vec<String> {
    let chars: Vec<_> = text.chars().take(8192).collect();
    let pages: Vec<String> = chars.chunks(300).map(|c| c.iter().collect()).collect();
    if pages.is_empty() {
        vec!["Synopsis unavailable.".into()]
    } else {
        pages
    }
}
#[derive(Deserialize)]
struct List {
    #[serde(default, rename = "game")]
    games: Vec<Item>,
}
#[derive(Deserialize)]
struct Item {
    path: PathBuf,
    #[serde(default)]
    desc: String,
}
fn synopsis(game: &Path) -> Option<String> {
    let parent = game.parent()?;
    for name in ["gamelist.xml", "miyoogamelist.xml"] {
        let source = parent.join(name);
        let outcome = METADATA_CACHE.with(|cache| {
            cache.borrow_mut().with_source(
                &source,
                || load_synopses(parent, &source),
                |value| match value {
                    CachedValue::Synopses(map) => Ok(map.get(game).cloned()),
                    CachedValue::Skipped => Ok(None),
                    _ => Err(()),
                },
            )
        });
        match outcome {
            Ok(Some(description)) => return Some(description),
            Ok(None) => {}
            Err(()) => return None,
        }
    }
    None
}
fn load_synopses(parent: &Path, source: &Path) -> CachedValue {
    common::perf_trace::frontend_io_op();
    let Ok(meta) = fs::symlink_metadata(source) else {
        return CachedValue::Skipped;
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 1024 * 1024 {
        return CachedValue::Skipped;
    }
    let read = || -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        common::perf_trace::frontend_io_op();
        fs::File::open(source)
            .ok()?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        Some(bytes)
    };
    let Some(bytes) = read() else {
        return CachedValue::Failed;
    };
    if bytes.len() > 1024 * 1024 {
        return CachedValue::Skipped;
    }
    let parsed = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| quick_xml::de::from_str::<List>(text).ok());
    let Some(list) = parsed else {
        return CachedValue::Failed;
    };
    let mut descriptions = std::collections::BTreeMap::new();
    for item in list.games {
        if item.desc.trim().is_empty() {
            continue;
        }
        let path = if item.path.is_absolute() {
            item.path
        } else {
            parent.join(item.path)
        };
        descriptions
            .entry(path)
            .or_insert_with(|| item.desc.chars().take(8192).collect());
    }
    CachedValue::Synopses(descriptions)
}
const METADATA_BYTES: usize = 256 * 1024;
const METADATA_SOURCES: usize = 16;
enum CachedValue {
    Synopses(std::collections::BTreeMap<PathBuf, String>),
    Catalog(MetadataCatalog),
    Skipped,
    Failed,
}
impl CachedValue {
    fn bytes(&self) -> usize {
        match self {
            // Charge a whole B-tree node per entry, including unused slots.
            Self::Synopses(map) => map
                .iter()
                .map(|(path, text)| 1024 + path.capacity() + text.capacity())
                .sum(),
            Self::Catalog(catalog) => catalog.bytes(),
            _ => 0,
        }
    }
}
type SourceIdentity = (u64, u64, u64, i64, i64, u32);
fn source_identity(path: &Path) -> Option<SourceIdentity> {
    use std::os::unix::fs::MetadataExt;
    common::perf_trace::frontend_io_op();
    let metadata = fs::symlink_metadata(path).ok()?;
    Some((
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.mode(),
    ))
}
struct CachedSource {
    path: PathBuf,
    identity: Option<SourceIdentity>,
    value: CachedValue,
    bytes: usize,
}
#[derive(Default)]
struct MetadataCache {
    sources: std::collections::VecDeque<CachedSource>,
    bytes: usize,
}
impl MetadataCache {
    fn with_source<T>(
        &mut self,
        path: &Path,
        load: impl FnOnce() -> CachedValue,
        read: impl FnOnce(&CachedValue) -> T,
    ) -> T {
        let identity = source_identity(path);
        if let Some(index) = self.sources.iter().position(|entry| entry.path == path) {
            let source = self.sources.remove(index).unwrap();
            if source.identity == identity {
                let result = read(&source.value);
                self.sources.push_front(source);
                return result;
            }
            self.bytes -= source.bytes;
        }
        let value = load();
        let result = read(&value);
        let bytes = value.bytes() + path.as_os_str().len() + std::mem::size_of::<CachedSource>();
        if bytes <= METADATA_BYTES {
            while self.bytes + bytes > METADATA_BYTES || self.sources.len() == METADATA_SOURCES {
                self.bytes -= self.sources.pop_back().unwrap().bytes;
            }
            self.bytes += bytes;
            self.sources.push_front(CachedSource {
                path: path.into(),
                identity,
                value,
                bytes,
            });
        }
        result
    }
}
thread_local! {
    static METADATA_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static METADATA_CACHE: std::cell::RefCell<MetadataCache> = std::cell::RefCell::new(MetadataCache::default());
}
pub(crate) fn metadata_generation() -> u64 {
    METADATA_GENERATION.with(std::cell::Cell::get)
}
pub(crate) fn invalidate_metadata() {
    METADATA_CACHE.with(|cache| *cache.borrow_mut() = MetadataCache::default());
    METADATA_GENERATION.with(|generation| generation.set(generation.get().wrapping_add(1)));
    crate::view::orbit::invalidate_art();
}
const SIDECAR_LIMIT: usize = 128 * 1024;
#[derive(Deserialize)]
struct MetadataCatalog {
    schema_version: u32,
    platform: String,
    entries: Vec<MetadataEntry>,
}
#[derive(Clone, Deserialize)]
struct MetadataEntry {
    id: String,
    title: String,
    aliases: Vec<String>,
    year: u16,
    first_release_year: u16,
    release_region: String,
    developer: String,
    publisher: String,
    genres: Vec<String>,
    description: String,
    source: MetadataSource,
}
#[derive(Clone, Deserialize)]
struct MetadataSource {
    title: String,
    url: String,
    license: MetadataLicense,
}
#[derive(Clone, Deserialize)]
struct MetadataLicense {
    name: String,
    url: String,
}
impl MetadataCatalog {
    fn bytes(&self) -> usize {
        let strings = |items: &Vec<String>| {
            std::mem::size_of::<String>() * items.capacity()
                + items.iter().map(String::capacity).sum::<usize>()
        };
        std::mem::size_of::<Self>()
            + self.platform.capacity()
            + self.entries.capacity() * std::mem::size_of::<MetadataEntry>()
            + self
                .entries
                .iter()
                .map(|e| {
                    [
                        &e.id,
                        &e.title,
                        &e.release_region,
                        &e.developer,
                        &e.publisher,
                        &e.description,
                        &e.source.title,
                        &e.source.url,
                        &e.source.license.name,
                        &e.source.license.url,
                    ]
                    .iter()
                    .map(|s| s.capacity())
                    .sum::<usize>()
                        + strings(&e.aliases)
                        + strings(&e.genres)
                })
                .sum::<usize>()
    }
}
fn identity(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
fn parse_catalog(bytes: &[u8]) -> Option<MetadataCatalog> {
    if bytes.len() > SIDECAR_LIMIT {
        return None;
    }
    let catalog: MetadataCatalog = serde_json::from_slice(bytes).ok()?;
    if catalog.schema_version != 1 || catalog.platform != "PS" || catalog.entries.len() > 64 {
        return None;
    }
    let mut ids = std::collections::BTreeSet::new();
    let bounded = |s: &str, limit: usize| s.chars().count() <= limit && !s.contains('\0');
    for entry in &catalog.entries {
        if entry.id.is_empty()
            || !ids.insert(&entry.id)
            || !bounded(&entry.id, 64)
            || !bounded(&entry.title, 256)
            || entry.aliases.is_empty()
            || entry.aliases.len() > 16
            || entry
                .aliases
                .iter()
                .any(|s| s.is_empty() || !bounded(s, 256))
            || !(1950..=2100).contains(&entry.year)
            || !(1950..=entry.year).contains(&entry.first_release_year)
            || !matches!(entry.release_region.as_str(), "USA" | "Japan" | "Europe")
            || !bounded(&entry.developer, 256)
            || !bounded(&entry.publisher, 256)
            || entry.genres.len() > 16
            || entry.genres.iter().any(|s| !bounded(s, 128))
            || !bounded(&entry.description, 8192)
            || !bounded(&entry.source.title, 256)
            || !bounded(&entry.source.url, 512)
            || !entry.source.url.starts_with("https://")
            || !bounded(&entry.source.license.name, 256)
            || !bounded(&entry.source.license.url, 512)
            || !entry.source.license.url.starts_with("https://")
        {
            return None;
        }
    }
    Some(catalog)
}
#[cfg(test)]
fn parse_sidecar(bytes: &[u8], game: &Path) -> Option<MetadataEntry> {
    matching_entry(&parse_catalog(bytes)?, game)
}
fn matching_entry(catalog: &MetadataCatalog, game: &Path) -> Option<MetadataEntry> {
    let stem = game.file_stem()?.to_str()?;
    let key = identity(stem);
    let mut matched = catalog
        .entries
        .iter()
        .filter(|e| e.aliases.iter().any(|a| identity(a) == key));
    let entry = matched.next()?;
    if matched.next().is_some() {
        return None;
    }
    // Region tags and sequel numerals remain part of exact alias identity.
    for region in ["USA", "Japan", "Europe"] {
        if key.contains(&format!("({})", region.to_lowercase())) && entry.release_region != region {
            return None;
        }
    }
    Some(entry.clone())
}
fn fallback(game: &Game) -> Option<MetadataEntry> {
    if !game
        .path
        .starts_with(common::constants::ALLIUM_GAMES_DIR.join("PS"))
    {
        return None;
    }
    let path = common::constants::ALLIUM_BASE_DIR.join("metadata/game-details.json");
    METADATA_CACHE.with(|cache| {
        cache.borrow_mut().with_source(
            &path,
            || {
                read_sidecar(&path)
                    .and_then(|bytes| parse_catalog(&bytes))
                    .map(CachedValue::Catalog)
                    .unwrap_or(CachedValue::Failed)
            },
            |value| {
                if let CachedValue::Catalog(catalog) = value {
                    matching_entry(catalog, &game.path)
                } else {
                    None
                }
            },
        )
    })
}
fn read_sidecar(path: &Path) -> Option<Vec<u8>> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    };
    if !path.is_absolute() {
        return None;
    }
    let components: Vec<_> = path.components().skip(1).collect();
    if components.is_empty()
        || components
            .iter()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    let open = |parent: &fs::File, name: &std::ffi::OsStr, directory: bool| -> Option<fs::File> {
        let name = std::ffi::CString::new(name.as_bytes()).ok()?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if directory {
                libc::O_DIRECTORY
            } else {
                libc::O_NONBLOCK
            };
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        (fd >= 0).then(|| unsafe { fs::File::from_raw_fd(fd) })
    };
    let mut parents = vec![fs::File::open("/").ok()?];
    let mut identities = Vec::new();
    for component in components.iter().take(components.len() - 1) {
        let held = open(parents.last()?, component.as_os_str(), true)?;
        let m = held.metadata().ok()?;
        identities.push((m.dev(), m.ino()));
        parents.push(held);
    }
    let file = open(parents.last()?, components.last()?.as_os_str(), false)?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > SIDECAR_LIMIT as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(SIDECAR_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > SIDECAR_LIMIT {
        return None;
    }
    let mut observed = fs::File::open("/").ok()?;
    for (component, expected) in components.iter().take(components.len() - 1).zip(identities) {
        observed = open(&observed, component.as_os_str(), true)?;
        let m = observed.metadata().ok()?;
        if (m.dev(), m.ino()) != expected {
            return None;
        }
    }
    Some(bytes)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn verified_proposal() -> Vec<u8> {
        fs::read(std::env::var("ALLIUM_DETAILS_METADATA_FIXTURE").unwrap()).unwrap()
    }
    #[test]
    fn sidecar_verified_aliases_region_sequel_and_primary_truth() {
        let bytes = verified_proposal();
        let r4 = Path::new("R4 - Ridge Racer Type 4 (USA).chd");
        let entry = parse_sidecar(&bytes, r4).unwrap();
        assert_eq!((entry.year, entry.first_release_year), (1999, 1998));
        assert_eq!(entry.developer, "Namco");
        assert!(parse_sidecar(&bytes, Path::new("Suikoden.chd")).is_none());
        assert!(parse_sidecar(&bytes, Path::new("Suikoden II (USA).chd")).is_some());
        assert!(parse_sidecar(&bytes, Path::new("Wipeout 3 Special Edition.chd")).is_none());
        let mut game = Game::new(r4.into());
        game.release_date = chrono::NaiveDate::from_ymd_opt(1998, 1, 1);
        game.developer = Some("Primary developer".into());
        game.publisher = Some("Primary publisher".into());
        game.genres = vec!["Primary genre".into()];
        let detail = Details::from_metadata(
            &game,
            "PlayStation",
            Some("Primary synopsis".into()),
            Some(&entry),
        );
        assert!(detail.metadata.contains("1998"));
        assert!(!detail.metadata.contains("1999"));
        for value in [
            "Primary developer",
            "Primary publisher",
            "Primary genre",
            "Primary synopsis",
        ] {
            assert!(detail.full_text.contains(value));
        }
        assert!(detail.full_text.contains(&display_url(&entry.source.url)));
        assert!(
            detail
                .full_text
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
                .contains(&entry.source.url)
        );
        assert!(!detail.full_text.contains(&entry.description));
        let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        changed["entries"][3]["release_region"] = "Japan".into();
        assert!(parse_sidecar(&serde_json::to_vec(&changed).unwrap(), r4).is_none());
    }
    #[test]
    fn verified_full_info_paginates_inside_actual_native_text_boxes() {
        use common::theme::{
            ThemeRuntime,
            frame::FrameInputs,
            pack::{Binding, ScreenId},
            render::PreparedFrame,
        };
        let root = common::constants::ALLIUM_SD_ROOT.as_path();
        let mut runtime = ThemeRuntime::bind(
            &root.join(".allium/state"),
            &root.join("Themes"),
            &root.join(".allium/fonts"),
        )
        .unwrap();
        let handle = runtime.choose("orbit").unwrap().snapshot.unwrap();
        let game = Game::new(root.join("Roms/PS/R4 - Ridge Racer Type 4 (USA).chd"));
        let mut details = Details::new(&game, "PlayStation");
        details.reflow(&handle);
        assert!(details.metadata.contains("1999"));
        assert!(details.full_text.contains("Namco"));
        let body = handle.pack().screens[&ScreenId::GameDetail]
            .components
            .iter()
            .find(|c| c.binding == Some(Binding::SelectedBlurb))
            .unwrap();
        let evidence = PathBuf::from(std::env::var("ALLIUM_DETAILS_PROOF_DIR").unwrap());
        fs::create_dir_all(&evidence).unwrap();
        let cover = tiny_skia::Pixmap::load_png(
            root.join("Roms/PS/Imgs/R4 - Ridge Racer Type 4 (USA).orbit.png"),
        )
        .unwrap();
        let mut shown = String::new();
        for index in 0..details.pages.len() {
            assert_eq!(details.page, index);
            assert!(details.text().chars().count() <= 512);
            shown.push_str(&details.pages[index]);
            shown.push(' ');
            let mut frame = FrameInputs::new(ScreenId::GameDetail);
            frame.text.insert(Binding::SelectedTitle, game.name.clone());
            frame
                .text
                .insert(Binding::SelectedMetadata, details.metadata.clone());
            frame.text.insert(Binding::SelectedBlurb, details.text());
            frame.rows.insert(Binding::Entries, vec![]);
            frame.rows.insert(Binding::Actions, vec![]);
            frame.art.selected_cover = Some(std::sync::Arc::new(cover.clone()));
            let prepared = PreparedFrame::prepare(handle.clone(), &frame).unwrap();
            assert!(
                prepared
                    .text_boxes
                    .iter()
                    .any(|(id, rect)| id == &body.id && *rect == body.rect)
            );
            assert!(
                details.text().lines().count() as u32
                    <= (body.rect.3
                        / handle.pack().tokens.typography[&body.role.unwrap()].line_height)
                        .min(body.max_lines.unwrap())
            );
            prepared
                .pixels()
                .save_png(evidence.join(format!("r4-full-info-page-{}.png", index + 1)))
                .unwrap();
            details.turn(true);
        }
        let shown = identity(&shown);
        let entry = parse_sidecar(&verified_proposal(), &game.path).unwrap();
        for expected in [
            &entry.developer,
            &entry.publisher,
            &entry.description,
            &entry.source.url,
            &entry.source.license.url,
        ] {
            let contains = if expected.starts_with("https://") {
                shown
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>()
                    .contains(&identity(expected))
            } else {
                shown.contains(&identity(expected))
            };
            assert!(contains, "Missing paged value: {expected}");
        }
        while details.page > 0 {
            details.turn(false);
        }
        assert_eq!(details.page, 0);
    }
    #[test]
    fn sidecar_caps_unknown_and_duplicate_aliases_fail_closed() {
        let bytes = verified_proposal();
        let game = Path::new("R4 - Ridge Racer Type 4 (USA).chd");
        assert!(parse_sidecar(&vec![b' '; SIDECAR_LIMIT + 1], game).is_none());
        assert!(parse_sidecar(b"not JSON", game).is_none());
        for key in ["developer", "description"] {
            let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            value["entries"][3][key] = "x"
                .repeat(if key == "description" { 8193 } else { 257 })
                .into();
            assert!(parse_sidecar(&serde_json::to_vec(&value).unwrap(), game).is_none());
        }
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let entry = value["entries"][3].clone();
        value["entries"] = vec![entry; 65].into();
        assert!(parse_sidecar(&serde_json::to_vec(&value).unwrap(), game).is_none());
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["entries"][0]["aliases"] = vec!["R4 - Ridge Racer Type 4 (USA)"].into();
        assert!(parse_sidecar(&serde_json::to_vec(&value).unwrap(), game).is_none());
        assert!(parse_sidecar(&bytes, Path::new("Unknown.chd")).is_none());
    }
    #[test]
    fn sidecar_read_rejects_symlink_ancestors_and_leaves() {
        use std::os::unix::fs::symlink;
        let root = PathBuf::from(format!(
            "/private/tmp/details-sidecar-{}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("real")).unwrap();
        let file = root.join("real/details.json");
        fs::write(&file, verified_proposal()).unwrap();
        let before = fs::read(&file).unwrap();
        assert_eq!(read_sidecar(&file).unwrap(), before);
        symlink(root.join("real"), root.join("alias")).unwrap();
        assert!(read_sidecar(&root.join("alias/details.json")).is_none());
        symlink(&file, root.join("leaf.json")).unwrap();
        assert!(read_sidecar(&root.join("leaf.json")).is_none());
        assert_eq!(fs::read(&file).unwrap(), before);
        let fifo = root.join("fifo.json");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(read_sidecar(&fifo).is_none());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn metadata_uses_existing_game_fields_and_missing_synopsis_is_explicit() {
        let mut game = Game::new("/nonexistent/Fixture.chd".into());
        game.release_date = chrono::NaiveDate::from_ymd_opt(1998, 1, 1);
        game.developer = Some("Developer fixture".into());
        game.publisher = Some("Publisher fixture".into());
        game.genres = vec!["Racing".into()];
        game.rating = Some(8);
        let details = Details::new(&game, "PlayStation");
        for expected in [
            "1998",
            "Developer fixture",
            "Publisher fixture",
            "Racing",
            "8/10",
        ] {
            assert!(format!("{} {}", details.metadata, details.full_text).contains(expected));
        }
        assert!(details.full_text.contains("Synopsis unavailable."));
    }
    #[test]
    fn synopsis_reads_matching_xml_without_mutation_and_rejects_oversize() {
        let root = std::env::temp_dir().join(format!("allium-details-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("miyoogamelist.xml");
        let data = b"<gameList><game><path>./Known.chd</path><desc>Real &amp; bounded synopsis</desc></game></gameList>";
        std::fs::write(&source, data).unwrap();
        assert_eq!(
            synopsis(&root.join("Known.chd")).as_deref(),
            Some("Real & bounded synopsis")
        );
        assert!(synopsis(&root.join("Unknown.chd")).is_none());
        assert_eq!(std::fs::read(&source).unwrap(), data);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len(1024 * 1024 + 1)
            .unwrap();
        assert!(synopsis(&root.join("Known.chd")).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn paging_is_bounded_utf8_and_clamped() {
        let mut detail = Details {
            metadata: String::new(),
            metadata_runs: vec![], facts: vec![], source: String::new(), ink_body: String::new(),
            blurb: String::new(),
            pages: pages(&"é".repeat(10000)),
            page: 0,
            full_text: String::new(),
        };
        assert_eq!(
            detail
                .pages
                .iter()
                .map(|s| s.chars().count())
                .sum::<usize>(),
            8192
        );
        for _ in 0..100 {
            detail.turn(true);
        }
        assert_eq!(detail.page, detail.pages.len() - 1);
        for _ in 0..100 {
            detail.turn(false);
        }
        assert_eq!(detail.page, 0);
    }

    #[test]
    fn citation_urls_are_breakable_and_terminal_orphans_are_rebalanced() {
        let url = "https://creativecommons.org/licenses/by-sa/4.0/";
        let displayed = display_url(url);
        assert_eq!(
            displayed
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>(),
            url
        );
        assert!(displayed.contains("creativecommons.org/ licenses/ by-sa/ 4.0/"));

        let pages = pages_from_lines(
            vec![
                "facts".into(),
                "source".into(),
                "license".into(),
                "0/".into(),
            ],
            3,
            480,
        );
        assert_eq!(pages, vec!["facts\nsource", "license\n0/"]);
        assert!(pages.last().unwrap().contains('\n'));
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    #[test]
    fn synopsis_precedence_duplicates_refresh_and_parse_failures_are_preserved() {
        let root =
            std::env::temp_dir().join(format!("allium-details-cache-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let game = root.join("Game.chd");
        let primary = root.join("gamelist.xml");
        fs::write(&primary, "<gameList><game><path>./Game.chd</path><desc>First</desc></game><game><path>./Game.chd</path><desc>Duplicate</desc></game></gameList>").unwrap();
        fs::write(
            root.join("miyoogamelist.xml"),
            "<gameList><game><path>./Game.chd</path><desc>Second source</desc></game></gameList>",
        )
        .unwrap();
        invalidate_metadata();
        assert_eq!(synopsis(&game).as_deref(), Some("First"));
        fs::write(&primary, "<gameList/>").unwrap();
        assert_eq!(synopsis(&game).as_deref(), Some("Second source"));
        invalidate_metadata();
        assert_eq!(synopsis(&game).as_deref(), Some("Second source"));
        fs::write(&primary, "invalid XML").unwrap();
        invalidate_metadata();
        assert_eq!(synopsis(&game), None);
        invalidate_metadata();
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn metadata_residency_is_bounded_and_oversized_results_still_return() {
        let mut cache = MetadataCache::default();
        for index in 0..100 {
            let source = PathBuf::from(format!("/directory/{index}/gamelist.xml"));
            cache.with_source(
                &source,
                || {
                    let mut map = std::collections::BTreeMap::new();
                    map.insert(PathBuf::from("Game.chd"), "description".repeat(200));
                    CachedValue::Synopses(map)
                },
                |_| (),
            );
            assert!(cache.bytes <= METADATA_BYTES);
            assert!(cache.sources.len() <= METADATA_SOURCES);
        }
        let result = cache.with_source(
            Path::new("/large/gamelist.xml"),
            || {
                let mut map = std::collections::BTreeMap::new();
                map.insert(PathBuf::from("Game.chd"), "x".repeat(METADATA_BYTES));
                CachedValue::Synopses(map)
            },
            |value| {
                if let CachedValue::Synopses(map) = value {
                    map[Path::new("Game.chd")].len()
                } else {
                    0
                }
            },
        );
        assert_eq!(result, METADATA_BYTES);
        assert!(
            !cache
                .sources
                .iter()
                .any(|source| source.path == Path::new("/large/gamelist.xml"))
        );
    }
}
