//! Shared read-only MEDIA.md lookup. Cache misses as well as successful paths.
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Cover,
    TallCover,
    Logo,
    Background,
    Screenshot,
    PreparedCover,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Metadata {
    pub path: PathBuf,
    pub name: String,
    pub desc: String,
    pub releasedate: String,
    pub developer: String,
    pub publisher: String,
    pub genre: String,
    pub players: String,
    pub rating: String,
    pub scraper: String,
    pub image: String,
    pub thumbnail: String,
    pub marquee: String,
    pub fanart: String,
}
// Retained records are shared only by readers. Game rows carry no owned scraper payload.
pub const METADATA_BUDGET: usize = 4 * 1024 * 1024;
pub const MAX_GAMELIST_BYTES: u64 = 32 * 1024 * 1024;
const RECORD_LIMIT: usize = 64 * 1024;
type SharedMetadata = std::sync::Arc<Metadata>;
#[derive(Default)]
struct Cache {
    sources: BTreeMap<PathBuf, Option<(u64, SystemTime)>>,
    records: BTreeMap<PathBuf, SharedMetadata>,
    order: std::collections::VecDeque<(PathBuf, usize)>,
    bytes: usize,
    budget: Option<usize>,
    missing: std::collections::BTreeSet<(PathBuf, PathBuf)>,
    paths: BTreeMap<(PathBuf, Kind), Option<PathBuf>>,
    logos: BTreeMap<PathBuf, Option<std::sync::Arc<tiny_skia::Pixmap>>>,
}
const PATH_CACHE_LIMIT: usize = 512;
impl Cache {
    fn insert_path(&mut self, key: (PathBuf, Kind), path: Option<PathBuf>) {
        if self.paths.len() >= PATH_CACHE_LIMIT {
            self.paths.clear();
        }
        self.paths.insert(key, path);
    }

    fn insert(&mut self, path: PathBuf, data: Metadata) {
        if self.records.contains_key(&path) {
            return;
        }
        let bytes = 768
            + 2 * path.capacity()
            + data.path.capacity()
            + [
                &data.name,
                &data.desc,
                &data.releasedate,
                &data.developer,
                &data.publisher,
                &data.genre,
                &data.players,
                &data.rating,
                &data.scraper,
                &data.image,
                &data.thumbnail,
                &data.marquee,
                &data.fanart,
            ]
            .iter()
            .map(|s| s.capacity())
            .sum::<usize>();
        let budget = self.budget.unwrap_or(METADATA_BUDGET);
        if bytes > budget {
            return;
        }
        while self.bytes + bytes > budget {
            let (old, bytes) = self.order.pop_front().unwrap();
            self.records.remove(&old);
            self.bytes -= bytes;
        }
        self.bytes += bytes;
        self.order.push_back((path.clone(), bytes));
        self.records.insert(path, std::sync::Arc::new(data));
    }
}
fn io_op() {
    crate::perf_trace::frontend_io_op();
    #[cfg(feature = "perf-bench")]
    crate::perf::hit(crate::perf::Op::MediaIo);
}
fn is_file(path: &Path) -> bool {
    io_op();
    path.is_file()
}
fn stat(path: &Path) -> Option<fs::Metadata> {
    io_op();
    fs::metadata(path).ok()
}

/// Streaming parser: at most one 64 KiB record and an 8 KiB reader buffer are live.
/// Large or malformed input is an explicit error, never an empty replacement map.
pub fn stream_gamelist(path: &Path, mut record: impl FnMut(Metadata)) -> anyhow::Result<()> {
    use quick_xml::events::Event;
    let size = stat(path)
        .ok_or_else(|| anyhow::anyhow!("Cannot stat {}", path.display()))?
        .len();
    anyhow::ensure!(
        size <= MAX_GAMELIST_BYTES,
        "Gamelist exceeds 32 MiB: {}",
        path.display()
    );
    io_op();
    let file = fs::File::open(path)?;
    let mut reader = quick_xml::Reader::from_reader(std::io::BufReader::with_capacity(8192, file));
    let mut buf = Vec::new();
    let mut game = Vec::new();
    let mut depth = 0usize;
    let mut root = false;
    let mut closed = false;
    loop {
        let event = reader.read_event_into(&mut buf)?;
        match &event {
            Event::Start(e) if depth == 0 && e.name().as_ref() == b"gameList" => root = true,
            Event::Start(e) if depth == 0 && e.name().as_ref() == b"game" => {
                game.clear();
                depth = 1;
            }
            Event::Start(_) if depth > 0 => depth += 1,
            Event::End(_) if depth > 0 => depth -= 1,
            Event::End(e) if depth == 0 && e.name().as_ref() == b"gameList" => closed = true,
            Event::Eof => {
                anyhow::ensure!(root && closed && depth == 0, "Incomplete gamelist");
                break;
            }
            _ => {}
        }
        if depth > 0 || !game.is_empty() {
            let mut writer = quick_xml::Writer::new(&mut game);
            writer.write_event(event)?;
            anyhow::ensure!(game.len() <= RECORD_LIMIT, "Gamelist record exceeds 64 KiB");
            if depth == 0 {
                let text = std::str::from_utf8(&game)?;
                let data = quick_xml::de::from_str::<Metadata>(text).or_else(|_| {
                    quick_xml::de::from_str::<Metadata>(&text.replace('&', "&amp;"))
                })?;
                record(data);
                game.clear();
            }
        }
        buf.clear();
    }
    Ok(())
}
thread_local! { static CACHE: RefCell<Cache> = RefCell::new(Cache::default()); }
fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(c.as_os_str()),
        }
    }
    result
}
thread_local! { static GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
fn changed() {
    GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
}
pub fn generation() -> u64 {
    GENERATION.with(std::cell::Cell::get)
}
pub fn invalidate() {
    CACHE.with(|c| *c.borrow_mut() = Cache::default());
    changed();
}
pub fn fingerprint(metadata: &fs::Metadata) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    metadata.len().hash(&mut hash);
    metadata.modified().ok().hash(&mut hash);
    hash.finish() as i64
}
fn source(parent: &Path, game: &Path, cache: &mut Cache) -> Option<SharedMetadata> {
    for name in ["gamelist.xml", "miyoogamelist.xml"] {
        let path = parent.join(name);
        if !cache.sources.contains_key(&path) {
            load_source(&path, cache);
        }
        if cache.sources.get(&path).is_some_and(Option::is_some) {
            let key = normalize(game);
            if cache.missing.contains(&(path.clone(), key.clone())) {
                return None;
            }
            if let Some(record) = cache.records.get(&key) {
                return Some(record.clone());
            }
            // An evicted record is read only by explicit load/idle callers.
            let mut found = None;
            if let Err(error) = stream_gamelist(&path, |data| {
                if normalize(&parent.join(&data.path)) == key {
                    found = Some(data);
                }
            }) {
                log::error!("Media source {}: {error:#}", path.display());
            }
            let Some(data) = found else {
                if cache.missing.len() >= 512 {
                    cache.missing.clear();
                }
                cache.missing.insert((path.clone(), key));
                return None;
            };
            cache.insert(key.clone(), data);
            return cache.records.get(&key).cloned();
        }
    }
    None
}
fn load_source(path: &Path, cache: &mut Cache) {
    let fingerprint = stat(path).map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
    let old = cache.sources.get(path);
    if old == Some(&fingerprint) {
        return;
    }
    if old.is_some() {
        changed();
        cache.missing.clear();
        cache.paths.clear();
        cache.logos.clear();
        cache.records.clear();
        cache.order.clear();
        cache.bytes = 0;
    }
    if fingerprint.is_some() {
        let parent = path.parent().unwrap();
        if let Err(error) = stream_gamelist(path, |data| {
            cache.insert(normalize(&parent.join(&data.path)), data)
        }) {
            log::error!("Media source {}: {error:#}", path.display());
        }
    }
    if cache.sources.len() >= 64 {
        cache.sources.clear();
    }
    cache.sources.insert(path.into(), fingerprint);
}
/// Fingerprints are checked at directory load or explicit refresh, never at frame preparation.
pub fn check_directory(parent: &Path) {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        for ancestor in parent.ancestors().take(8) {
            for name in ["gamelist.xml", "miyoogamelist.xml"] {
                load_source(&ancestor.join(name), &mut cache);
            }
            if ancestor.file_name().is_some_and(|n| n == "Roms") {
                break;
            }
        }
    });
}
fn metadata_with_parent(path: &Path, cache: &mut Cache) -> (PathBuf, SharedMetadata) {
    let mut system = path.parent().unwrap_or(Path::new(".")).to_owned();
    for parent in path.ancestors().skip(1).take(8) {
        if parent
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "Roms")
        {
            system = parent.into();
        }
        if let Some(game) = source(parent, path, cache) {
            return (parent.into(), game);
        }
        if parent.file_name().is_some_and(|n| n == "Roms") {
            break;
        }
    }
    (system, SharedMetadata::default())
}
pub fn metadata(path: &Path) -> SharedMetadata {
    CACHE.with(|c| metadata_with_parent(path, &mut c.borrow_mut()).1)
}
/// No storage access, including when a record was evicted.
pub fn cached_metadata(path: &Path) -> SharedMetadata {
    CACHE.with(|c| {
        c.borrow()
            .records
            .get(&normalize(path))
            .cloned()
            .unwrap_or_default()
    })
}
pub fn cached_resolve(path: &Path, kind: Kind) -> Option<PathBuf> {
    CACHE.with(|c| {
        c.borrow()
            .paths
            .get(&(path.into(), kind))
            .cloned()
            .flatten()
    })
}

pub fn resolve(path: &Path, kind: Kind) -> Option<PathBuf> {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let key = (path.to_owned(), kind);
        if let Some(found) = cache.paths.get(&key) {
            return found.clone();
        }
        let (parent, data) = metadata_with_parent(path, &mut cache);
        let tags: &[&str] = match kind {
            Kind::Cover | Kind::PreparedCover => &[&data.image, &data.thumbnail],
            Kind::Logo => &[&data.marquee],
            Kind::Background => &[&data.fanart],
            _ => &[],
        };
        let mut found = tags
            .iter()
            .filter(|tag| !tag.is_empty() && Path::new(tag).is_relative())
            .map(|tag| parent.join(tag))
            .find(|candidate| is_file(candidate.as_path()));
        let stem = path.file_stem().unwrap_or_default();
        let kinds: &[&str] = match kind {
            Kind::Cover | Kind::PreparedCover => &["covers", "box2dfront"],
            Kind::TallCover => &["covers-tall"],
            Kind::Logo => &["logos", "marquees", "wheel"],
            Kind::Background => &["backgrounds", "fanart"],
            Kind::Screenshot => &["screenshots"],
        };
        for folder in kinds {
            for ext in ["png", "jpg"] {
                let mut filename = stem.to_os_string();
                filename.push(format!(".{ext}"));
                let candidate = parent.join("media").join(folder).join(filename);
                if found.is_none() && is_file(candidate.as_path()) {
                    found = Some(candidate);
                }
            }
        }
        if found.is_none() && matches!(kind, Kind::Cover | Kind::PreparedCover | Kind::Background) {
            let suffixes: &[&str] = match kind {
                Kind::Cover => &[".png", ".jpg", ".jpeg", ".gif"],
                Kind::PreparedCover => &[".orbit.png"],
                _ => &[".hero.png"],
            };
            'legacy: for ancestor in path.ancestors().skip(1).take(8) {
                for suffix in suffixes {
                    let mut filename = stem.to_os_string();
                    filename.push(suffix);
                    let candidate = ancestor.join("Imgs").join(filename);
                    if is_file(candidate.as_path()) {
                        found = Some(candidate);
                        break 'legacy;
                    }
                    if let Ok(relative) = path.strip_prefix(ancestor) {
                        let mut candidate = ancestor.join("Imgs").join(relative);
                        candidate.set_extension(suffix.trim_start_matches('.'));
                        if is_file(candidate.as_path()) {
                            found = Some(candidate);
                            break 'legacy;
                        }
                    }
                    if ancestor.file_name().is_some_and(|name| name == "Roms") {
                        if let (Some(root), Ok(relative)) =
                            (ancestor.parent(), path.strip_prefix(ancestor))
                        {
                            let mut candidate = root.join("Imgs").join(relative);
                            candidate.set_extension(suffix.trim_start_matches('.'));
                            if is_file(candidate.as_path()) {
                                found = Some(candidate);
                                break 'legacy;
                            }
                        }
                    }
                }
            }
        }
        if found.is_none()
            && kind == Kind::Cover
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ["png", "jpg", "jpeg", "gif"].contains(&e))
        {
            found = Some(path.into());
        }
        cache.insert_path(key, found.clone());
        found
    })
}

/// Small bounded decoder for menu logos, which do not use the launcher's art cache.
pub fn load_logo(path: &Path) -> Option<std::sync::Arc<tiny_skia::Pixmap>> {
    let path = resolve(path, Kind::Logo)?;
    if let Some(cached) = CACHE.with(|c| c.borrow().logos.get(&path).cloned()) {
        return cached;
    }
    let image = decode_logo(&path);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.logos.len() >= 16 {
            c.logos.clear();
        }
        c.logos.insert(path, image.clone());
    });
    image
}
fn decode_logo(path: &Path) -> Option<std::sync::Arc<tiny_skia::Pixmap>> {
    if stat(path)?.len() > 512 * 1024 {
        return None;
    }
    io_op();
    let dimensions = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if dimensions.0 > 2048 || dimensions.1 > 2048 {
        return None;
    }
    io_op();
    #[cfg(feature = "perf-bench")]
    crate::perf::hit(crate::perf::Op::Decode);
    let image = image::open(path).ok()?;
    let image = image.thumbnail(480, 200).to_rgba8();
    let mut pixels = tiny_skia::Pixmap::new(image.width(), image.height())?;
    for (src, dst) in image.pixels().zip(pixels.pixels_mut()) {
        *dst = tiny_skia::ColorU8::from_rgba(src[0], src[1], src[2], src[3]).premultiply();
    }
    Some(std::sync::Arc::new(pixels))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/media-sd/Roms/TEST")
    }
    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for e in fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                copy_tree(&e.path(), &to.join(e.file_name()));
            } else {
                fs::copy(e.path(), to.join(e.file_name())).unwrap();
            }
        }
    }
    #[test]
    fn adopted_game_paths_obey_resolve_cache_limit() {
        invalidate();
        let kinds = [
            Kind::Cover,
            Kind::TallCover,
            Kind::Logo,
            Kind::Background,
            Kind::PreparedCover,
        ];
        for n in 0..600 {
            let game = PathBuf::from(format!("/fixture/G{n}.sfc"));
            adopt_game(PreparedGame {
                path: game.clone(),
                metadata: std::sync::Arc::new(Metadata::default()),
                paths: kinds
                    .into_iter()
                    .map(|kind| (kind, Some(game.with_extension("png"))))
                    .collect(),
            });
            CACHE.with(|cache| {
                let cache = cache.borrow();
                assert!(cache.paths.len() <= PATH_CACHE_LIMIT);
                assert_eq!(
                    cache.paths.get(&(game.clone(), Kind::PreparedCover)),
                    Some(&Some(game.with_extension("png")))
                );
            });
        }
        // A worker cache rollover still admits successful paths and cached misses.
        let game = PathBuf::from("/fixture/last.sfc");
        adopt_game(PreparedGame {
            path: game.clone(),
            metadata: std::sync::Arc::new(Metadata::default()),
            paths: vec![
                (Kind::Cover, Some(game.with_extension("png"))),
                (Kind::Logo, None),
            ],
        });
        assert_eq!(
            resolve(&game, Kind::Cover),
            Some(game.with_extension("png"))
        );
        assert_eq!(resolve(&game, Kind::Logo), None);
        CACHE.with(|cache| assert_eq!(cache.borrow().paths.get(&(game, Kind::Logo)), Some(&None)));
        invalidate();
    }

    #[test]
    fn media_fixture_present_absent_metadata_unknown_and_missing_fields() {
        invalidate();
        let root = fixture();
        for (kind, folder) in [
            (Kind::Cover, "covers"),
            (Kind::TallCover, "covers-tall"),
            (Kind::Logo, "logos"),
            (Kind::Background, "backgrounds"),
            (Kind::Screenshot, "screenshots"),
        ] {
            assert_eq!(
                resolve(&root.join("Complete.sfc"), kind),
                Some(root.join(format!("media/{folder}/Complete.png")))
            );
            assert_eq!(resolve(&root.join("Missing.sfc"), kind), None);
        }
        let data = metadata(&root.join("Complete.sfc"));
        assert_eq!(data.desc, "A & B race.");
        assert_eq!(data.releasedate, "19981203T000000");
        assert_eq!(data.developer, "Fixture studio");
        assert_eq!(data.publisher, "Fixture publisher");
        assert_eq!(data.genre, "Racing, Arcade");
        assert_eq!(data.players, "1-2");
        assert_eq!(data.rating, "0.8");
        assert_eq!(data.scraper, "Fixture scraper");
        let missing = metadata(&root.join("Missing.sfc"));
        assert!(
            missing.desc.is_empty() && missing.players.is_empty() && missing.releasedate.is_empty()
        );
        assert_eq!(
            resolve(&root.join("Legacy.sfc"), Kind::Cover),
            Some(root.join("Imgs/Legacy.png"))
        );
        assert_eq!(
            resolve(&root.join("Legacy.sfc"), Kind::Background),
            Some(root.join("Imgs/Legacy.hero.png"))
        );
        for (kind, file) in [
            (Kind::Cover, "image"),
            (Kind::Logo, "marquee"),
            (Kind::Background, "fanart"),
        ] {
            assert_eq!(
                normalize(&resolve(&root.join("Override.sfc"), kind).unwrap()),
                root.join(format!("overrides/{file}.png"))
            );
        }
    }
    #[test]
    fn media_lookup_order_png_jpg_aliases_and_cached_misses() {
        let root = std::env::temp_dir().join(format!("allium-media-order-{}", std::process::id()));
        copy_tree(&fixture(), &root);
        invalidate();
        let game = root.join("Complete.sfc");
        for (kind, folders) in [
            (Kind::Cover, vec!["covers", "box2dfront"]),
            (Kind::Logo, vec!["logos", "marquees", "wheel"]),
            (Kind::Background, vec!["backgrounds", "fanart"]),
        ] {
            let original = root.join(format!("media/{}/Complete.png", folders[0]));
            let bytes = fs::read(&original).unwrap();
            for folder in &folders {
                let dir = root.join("media").join(folder);
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join("Complete.png"), &bytes).unwrap();
                image::RgbImage::new(64, 64)
                    .save(dir.join("Complete.jpg"))
                    .unwrap();
            }
            for folder in folders {
                for ext in ["png", "jpg"] {
                    let path = root.join(format!("media/{folder}/Complete.{ext}"));
                    invalidate();
                    assert_eq!(resolve(&game, kind), Some(path.clone()));
                    fs::remove_file(path).unwrap();
                }
            }
            invalidate();
            assert!(resolve(&game, kind).is_none());
        }
        let missing = root.join("Missing.sfc");
        assert!(resolve(&missing, Kind::Cover).is_none());
        let cover = root.join("media/covers/Missing.png");
        fs::copy(root.join("Imgs/Legacy.png"), &cover).unwrap();
        assert!(resolve(&missing, Kind::Cover).is_none());
        invalidate();
        assert_eq!(resolve(&missing, Kind::Cover), Some(cover));
        fs::remove_dir_all(root).unwrap();
        invalidate();
    }
    #[test]
    fn media_gamelist_same_size_change_invalidates_cached_paths() {
        let root =
            std::env::temp_dir().join(format!("allium-media-fingerprint-{}", std::process::id()));
        copy_tree(&fixture(), &root);
        invalidate();
        let game = root.join("Override.sfc");
        let before = resolve(&game, Kind::Cover).unwrap();
        let source = root.join("gamelist.xml");
        let xml = fs::read_to_string(&source).unwrap();
        fs::write(&source, xml.replace("image.png", "other.png")).unwrap();
        fs::copy(
            root.join("overrides/image.png"),
            root.join("overrides/other.png"),
        )
        .unwrap();
        check_directory(&root);
        let after = resolve(&game, Kind::Cover).unwrap();
        assert_ne!(before, after);
        fs::remove_dir_all(root).unwrap();
        invalidate();
    }
    #[test]
    fn review_large_scrape_preserves_metadata_and_overrides() {
        let root = std::env::temp_dir().join(format!("allium-review-large-{}", std::process::id()));
        fs::create_dir_all(root.join("overrides")).unwrap();
        fs::write(root.join("overrides/logo.png"), b"fixture").unwrap();
        let mut xml = String::from("<gameList>");
        for n in 0..1000 {
            xml.push_str(&format!("<game><path>./Game{n}.sfc</path><desc>{}</desc><players>1-2</players><marquee>./overrides/logo.png</marquee></game>","D".repeat(1024)));
        }
        xml.push_str("</gameList>");
        assert!(xml.len() > 1024 * 1024);
        fs::write(root.join("gamelist.xml"), xml).unwrap();
        invalidate();
        let data = metadata(&root.join("Game999.sfc"));
        let result = (
            data.desc.len(),
            data.players.clone(),
            resolve(&root.join("Game999.sfc"), Kind::Logo),
        );
        fs::remove_dir_all(&root).unwrap();
        invalidate();
        assert_eq!(
            result,
            (1024, "1-2".into(), Some(root.join("./overrides/logo.png")))
        );
    }
    #[test]
    fn review_multimegabyte_scrape_can_reload_an_evicted_record() {
        let root =
            std::env::temp_dir().join(format!("allium-review-multimeg-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let mut xml = String::from("<gameList>");
        for n in 0..3500 {
            xml.push_str(&format!(
                "<game><path>./G{n}.sfc</path><desc>{}</desc><players>1-2</players></game>",
                "D".repeat(1024)
            ));
        }
        xml.push_str("</gameList>");
        assert!(xml.len() > 3 * 1024 * 1024);
        fs::write(root.join("gamelist.xml"), xml).unwrap();
        invalidate();
        let first = metadata(&root.join("G0.sfc"));
        let last = metadata(&root.join("G3499.sfc"));
        assert_eq!((first.desc.len(), last.desc.len()), (1024, 1024));
        assert_eq!(first.players, "1-2");
        assert!(CACHE.with(|c| c.borrow().bytes) <= METADATA_BUDGET);
        fs::remove_dir_all(root).unwrap();
        invalidate();
    }
    #[test]
    fn review_metadata_records_are_shared_and_survive_eviction() {
        invalidate();
        let game = fixture().join("Complete.sfc");
        let first = metadata(&game);
        let second = metadata(&game);
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        invalidate();
        assert_eq!(first.desc, "A & B race.");
    }
    #[test]
    fn review_metadata_retention_has_a_byte_budget() {
        let root =
            std::env::temp_dir().join(format!("allium-review-budget-{}", std::process::id()));
        invalidate();
        for n in 0..12 {
            let dir = root.join(n.to_string());
            fs::create_dir_all(&dir).unwrap();
            let mut xml = String::from("<gameList>");
            for j in 0..2000 {
                xml.push_str(&format!(
                    "<game><path>./G{j}.sfc</path><desc>{}</desc></game>",
                    "D".repeat(64)
                ));
            }
            xml.push_str("</gameList>");
            fs::write(dir.join("gamelist.xml"), xml).unwrap();
            let _ = metadata(&dir.join("G0.sfc"));
        }
        let retained = CACHE.with(|c| c.borrow().bytes);
        fs::remove_dir_all(root).unwrap();
        invalidate();
        assert!(
            retained <= 4 * 1024 * 1024,
            "retained {retained} bytes exceeds 4 MiB"
        );
    }
    #[test]
    fn media_logo_keeps_native_aspect() {
        invalidate();
        let logo = load_logo(&fixture().join("Complete.sfc")).unwrap();
        assert_eq!(logo.width() * 24, logo.height() * 96);
        assert!(load_logo(&fixture().join("Missing.sfc")).is_none());
    }
}

/// A worker's resolved paths and shared record, adopted without touching storage.
pub struct PreparedGame {
    path: PathBuf,
    metadata: SharedMetadata,
    paths: Vec<(Kind, Option<PathBuf>)>,
}
pub fn prepare_game(path: &Path) -> PreparedGame {
    let metadata = metadata(path);
    let paths = [
        Kind::Cover,
        Kind::TallCover,
        Kind::Logo,
        Kind::Background,
        Kind::PreparedCover,
    ]
    .into_iter()
    .map(|kind| (kind, resolve(path, kind)))
    .collect();
    PreparedGame {
        path: path.into(),
        metadata,
        paths,
    }
}
pub fn adopt_game(game: PreparedGame) {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let key = normalize(&game.path);
        if !cache.records.contains_key(&key) {
            cache.insert(key, (*game.metadata).clone());
        }
        for (kind, path) in game.paths {
            cache.insert_path((game.path.clone(), kind), path);
        }
    });
}
/// Worker-local policy. Call after invalidation as well, which resets the cache.
pub fn set_metadata_budget(bytes: usize) {
    CACHE.with(|cache| cache.borrow_mut().budget = Some(bytes.min(METADATA_BUDGET)));
}
pub fn clear_prepared_paths() {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.paths.clear();
        cache.missing.clear();
    });
}

pub fn adopt_paths(game: &Path, paths: impl IntoIterator<Item = (Kind, PathBuf)>) {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        for (kind, path) in paths {
            cache.insert_path((game.into(), kind), Some(path));
        }
    });
}
