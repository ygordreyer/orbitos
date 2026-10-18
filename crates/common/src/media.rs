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
fn marker_budget(budget: usize) -> usize {
    (budget / 16).min(64 * 1024)
}
fn resident_budget(budget: usize) -> usize {
    budget.saturating_sub(marker_budget(budget))
}
type SharedMetadata = std::sync::Arc<Metadata>;
#[derive(Clone)]
struct Record {
    parent: PathBuf,
    data: SharedMetadata,
}
#[derive(Serialize, Deserialize)]
struct SourceEntry {
    name: String,
    start: u64,
    len: u64,
}
#[derive(Default)]
struct Cache {
    sources: BTreeMap<PathBuf, Option<(u64, SystemTime)>>,
    records: BTreeMap<PathBuf, Record>,
    order: std::collections::VecDeque<(PathBuf, usize)>,
    bytes: usize,
    budget: Option<usize>,
    names: BTreeMap<PathBuf, BTreeMap<PathBuf, SourceEntry>>,
    source_order: std::collections::VecDeque<(PathBuf, usize)>,
    skipped_sources: BTreeMap<PathBuf, Fingerprint>,
    skipped_bytes: usize,
    suppress_new_sources: bool,
    defer_cold: bool,
    local_containers: BTreeMap<PathBuf, Option<PathBuf>>,
    paths: BTreeMap<(PathBuf, Kind), Option<PathBuf>>,
    path_order: std::collections::VecDeque<(PathBuf, Kind)>,
    logos: BTreeMap<PathBuf, Option<std::sync::Arc<tiny_skia::Pixmap>>>,
}
const PATH_CACHE_LIMIT: usize = 512;
impl Cache {
    fn evict_record(&mut self) -> bool {
        let Some((old, bytes)) = self.order.pop_front() else {
            return false;
        };
        self.records.remove(&old);
        self.bytes -= bytes;
        true
    }

    fn forget_skipped_source(&mut self, path: &Path) {
        if self.skipped_sources.remove(path).is_some() {
            let bytes = source_bytes(path);
            self.skipped_bytes -= bytes;
            self.bytes -= bytes;
        }
    }

    fn remember_skipped_source(&mut self, path: PathBuf, fingerprint: Fingerprint) {
        if let Some(previous) = self.skipped_sources.get_mut(&path) {
            *previous = fingerprint;
            return;
        }
        let bytes = source_bytes(&path);
        let limit = marker_budget(self.budget.unwrap_or(METADATA_BUDGET));
        if self.skipped_bytes + bytes > limit {
            // Forgetting an eviction would make it cold again and restart admission churn.
            self.suppress_new_sources = true;
            return;
        }
        self.skipped_bytes += bytes;
        self.bytes += bytes;
        self.skipped_sources.insert(path, fingerprint);
    }

    fn evict_source(&mut self) -> Option<bool> {
        let (old, bytes) = self.source_order.pop_front()?;
        let fingerprint = self.sources.remove(&old).unwrap();
        let visible = self
            .names
            .remove(&old)
            .is_some_and(|names| !names.is_empty());
        self.bytes -= bytes;
        self.remember_skipped_source(old, fingerprint);
        Some(visible)
    }

    fn source_resolution_changes_names(&self, path: &Path, index: &NameIndex) -> bool {
        if !index.valid || self.sources.get(path) == Some(&index.fingerprint) {
            return false;
        }
        self.resolves_resident_names(path)
    }

    fn resolves_resident_names(&self, path: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        if path
            .file_name()
            .is_some_and(|name| name == "miyoogamelist.xml")
            && self
                .sources
                .get(&parent.join("gamelist.xml"))
                .or_else(|| self.skipped_sources.get(&parent.join("gamelist.xml")))
                .is_some_and(Option::is_some)
        {
            return false;
        }
        // Child resolution can expose an ancestor's exact identities or game containers.
        self.names.iter().any(|(source, names)| {
            source != path
                && source
                    .parent()
                    .is_some_and(|ancestor| parent.starts_with(ancestor))
                && names
                    .keys()
                    .any(|game| game.starts_with(parent) || parent.starts_with(game))
        })
    }

    fn insert_source(&mut self, path: PathBuf, index: NameIndex) -> bool {
        if index.valid
            && self
                .sources
                .get(&path)
                .is_some_and(|previous| *previous != index.fingerprint)
        {
            self.suppress_new_sources = false;
        }
        let resolution_changed = self.source_resolution_changes_names(&path, &index);
        if let Some(at) = self.source_order.iter().position(|(p, _)| p == &path) {
            let (_, bytes) = self.source_order.remove(at).unwrap();
            self.bytes -= bytes;
        }
        self.sources.remove(&path);
        self.forget_skipped_source(&path);
        let old_names = self.names.remove(&path).unwrap_or_default();
        let mut evicted_visible = false;
        let budget = self.budget.unwrap_or(METADATA_BUDGET);
        if !index.valid {
            return !old_names.is_empty();
        }
        // Reserve the bounded marker allowance before admitting a resident prefix.
        let resident_budget = resident_budget(budget);
        if index.bytes > resident_budget {
            self.remember_skipped_source(path, index.fingerprint);
            return !old_names.is_empty();
        }
        while self.bytes - self.skipped_bytes + index.bytes > resident_budget {
            if self.evict_record() {
                continue;
            }
            let Some(removed_names) = self.evict_source() else {
                self.remember_skipped_source(path, index.fingerprint);
                return evicted_visible || !old_names.is_empty();
            };
            evicted_visible |= removed_names;
        }
        let same_names = old_names
            .iter()
            .map(|(path, entry)| (path, &entry.name))
            .eq(index.names.iter().map(|(path, entry)| (path, &entry.name)));
        let visible = evicted_visible || resolution_changed || !same_names;
        self.bytes += index.bytes;
        self.source_order.push_back((path.clone(), index.bytes));
        self.sources.insert(path.clone(), index.fingerprint);
        self.names.insert(path, index.names);
        visible
    }

    fn discard_source(&mut self, path: &Path) {
        // Freed resident capacity must remain usable after a transient record-read failure.
        if self.sources.contains_key(path) {
            self.suppress_new_sources = false;
        }
        self.forget_skipped_source(path);
        if let Some(at) = self.source_order.iter().position(|(p, _)| p == path) {
            let (_, bytes) = self.source_order.remove(at).unwrap();
            self.bytes -= bytes;
        }
        self.sources.remove(path);
        self.names.remove(path);
        self.clear_records();
        changed();
        NAME_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
    }

    fn clear_records(&mut self) {
        while self.evict_record() {}
        self.local_containers.clear();
        self.paths.clear();
        paths_changed();
        self.path_order.clear();
        self.logos.clear();
    }

    fn insert_path(&mut self, key: (PathBuf, Kind), path: Option<PathBuf>) {
        if !self.paths.contains_key(&key) {
            if self.paths.len() >= PATH_CACHE_LIMIT {
                if let Some(old) = self.path_order.pop_front() {
                    self.paths.remove(&old);
                    paths_changed();
                }
            }
            self.path_order.push_back(key.clone());
        }
        self.paths.insert(key, path);
        paths_changed();
    }

    fn insert(&mut self, path: PathBuf, parent: PathBuf, data: Metadata) {
        if self.records.contains_key(&path) {
            return;
        }
        let bytes = 768
            + 2 * path.capacity()
            + parent.capacity()
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
        let budget = resident_budget(self.budget.unwrap_or(METADATA_BUDGET));
        if bytes > budget {
            return;
        }
        while self.bytes - self.skipped_bytes + bytes > budget {
            if !self.evict_record() {
                return;
            }
        }
        self.bytes += bytes;
        self.order.push_back((path.clone(), bytes));
        self.records.insert(
            path,
            Record {
                parent,
                data: std::sync::Arc::new(data),
            },
        );
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
    stream_indexed(path, |data, _, _| record(data))
}
fn stream_indexed(path: &Path, mut record: impl FnMut(Metadata, u64, u64)) -> anyhow::Result<()> {
    #[cfg(test)]
    PARSES.with(|counts| *counts.borrow_mut().entry(path.into()).or_default() += 1);
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
    let mut start = 0;
    loop {
        let position = reader.buffer_position();
        let event = reader.read_event_into(&mut buf)?;
        match &event {
            Event::Start(e) if depth == 0 && e.name().as_ref() == b"gameList" => root = true,
            Event::Start(e) if depth == 0 && e.name().as_ref() == b"game" => {
                game.clear();
                start = position;
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
                record(data, start, reader.buffer_position() - start);
                game.clear();
            }
        }
        buf.clear();
    }
    Ok(())
}
thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
    static CONTAINERS: RefCell<BTreeMap<PathBuf, PathBuf>> = const { RefCell::new(BTreeMap::new()) };
    #[cfg(test)]
    static PARSES: RefCell<BTreeMap<PathBuf, usize>> = const { RefCell::new(BTreeMap::new()) };
}
/// Descriptor relationships are explicit and survive metadata cache invalidation.
pub fn set_game_containers(containers: impl IntoIterator<Item = (PathBuf, PathBuf)>) {
    CONTAINERS.with(|c| {
        *c.borrow_mut() = containers
            .into_iter()
            .map(|(game, container)| (normalize(&game), normalize(&container)))
            .filter(|(game, container)| game != container && game.starts_with(container))
            .collect();
    });
}
pub fn game_container(path: &Path) -> Option<PathBuf> {
    CONTAINERS.with(|c| c.borrow().get(&normalize(path)).cloned())
}
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
thread_local! { static PATH_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
fn paths_changed() {
    PATH_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
}
/// Includes hydration, FIFO eviction and clears, independently of metadata revisions.
pub fn path_generation() -> u64 {
    PATH_GENERATION.with(std::cell::Cell::get)
}
pub fn generation() -> u64 {
    GENERATION.with(std::cell::Cell::get)
}
thread_local! { static KEEP_NAME_INDEXES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
/// Row reloads invalidate artwork, but must not discard the indexes that triggered the reload.
pub fn with_name_indexes<T>(reload: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            KEEP_NAME_INDEXES.with(|keep| keep.set(self.0));
        }
    }
    let old = KEEP_NAME_INDEXES.with(|keep| keep.replace(true));
    let _restore = Restore(old);
    reload()
}
pub fn invalidate() {
    if KEEP_NAME_INDEXES.with(std::cell::Cell::get) {
        CACHE.with(|c| c.borrow_mut().clear_records());
        changed();
        return;
    }
    paths_changed();
    CACHE.with(|c| {
        *c.borrow_mut() = Cache {
            defer_cold: UI_NAMES.with(std::cell::Cell::get),
            ..Cache::default()
        }
    });
    INDEX_EPOCH.with(|e| e.set(e.get().wrapping_add(1)));
    changed();
}
pub fn fingerprint(metadata: &fs::Metadata) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    metadata.len().hash(&mut hash);
    metadata.modified().ok().hash(&mut hash);
    hash.finish() as i64
}
fn source(path: &Path, game: &Path, cache: &mut Cache) -> Option<Record> {
    let parent = path.parent()?;
    let key = normalize(game);
    // The source index distinguishes absent identities from evicted full records.
    if !cache.names.get(path)?.contains_key(&key) {
        return None;
    }
    if let Some(record) = cache.records.get(&key) {
        return Some(record.clone());
    }
    let entry = cache.names.get(path)?.get(&key)?;
    let read = || -> anyhow::Result<Metadata> {
        use std::io::{Read, Seek, SeekFrom};
        io_op();
        let mut file = fs::File::open(path)?;
        anyhow::ensure!(
            Some(file_fingerprint(&file)?) == cache.sources[path],
            "Gamelist changed"
        );
        file.seek(SeekFrom::Start(entry.start))?;
        let mut text = String::new();
        (&mut file).take(entry.len).read_to_string(&mut text)?;
        let data: Metadata = quick_xml::de::from_str(&text)
            .or_else(|_| quick_xml::de::from_str(&text.replace('&', "&amp;")))?;
        anyhow::ensure!(
            normalize(&parent.join(&data.path)) == key,
            "Gamelist identity changed"
        );
        anyhow::ensure!(
            Some(file_fingerprint(&file)?) == cache.sources[path]
                && source_fingerprint(path) == cache.sources[path],
            "Gamelist changed"
        );
        Ok(data)
    };
    let data = match read() {
        Ok(data) => data,
        Err(error) => {
            log::error!("Media record {}: {error:#}", path.display());
            cache.discard_source(path);
            return None;
        }
    };
    cache.insert(key.clone(), parent.into(), data.clone());
    Some(cache.records.get(&key).cloned().unwrap_or_else(|| Record {
        parent: parent.into(),
        data: std::sync::Arc::new(data),
    }))
}
type Fingerprint = Option<(u64, SystemTime)>;
struct NameIndex {
    fingerprint: Fingerprint,
    names: BTreeMap<PathBuf, SourceEntry>,
    valid: bool,
    bytes: usize,
}
fn source_fingerprint(path: &Path) -> Fingerprint {
    stat(path).map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
}
fn file_fingerprint(file: &fs::File) -> std::io::Result<(u64, SystemTime)> {
    io_op();
    let metadata = file.metadata()?;
    Ok((
        metadata.len(),
        metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    ))
}
fn source_bytes(path: &Path) -> usize {
    640 + 4 * path.as_os_str().len()
}
fn entry_bytes(path: &PathBuf, entry: &SourceEntry) -> usize {
    192 + path.capacity() + entry.name.capacity()
}
fn read_name_index(path: &Path, budget: usize) -> NameIndex {
    read_name_index_records(path, budget, |_, _| {})
}
fn read_name_index_records(
    path: &Path,
    budget: usize,
    mut record: impl FnMut(PathBuf, Metadata),
) -> NameIndex {
    let budget = resident_budget(budget);
    let fingerprint = source_fingerprint(path);
    let mut index = NameIndex {
        fingerprint,
        names: BTreeMap::new(),
        valid: true,
        bytes: source_bytes(path),
    };
    let mut full = false;
    if fingerprint.is_some() {
        let parent = path.parent().unwrap();
        if let Err(error) = stream_indexed(path, |data, start, len| {
            if full {
                return;
            }
            let key = normalize(&parent.join(&data.path));
            if index.names.contains_key(&key) {
                return;
            }
            let entry = SourceEntry {
                name: data.name.clone(),
                start,
                len,
            };
            let bytes = entry_bytes(&key, &entry);
            // Keep a bounded prefix resident. Overflow identities remain unhydrated.
            if index.bytes + bytes > budget {
                full = true;
                return;
            }
            index.bytes += bytes;
            record(key.clone(), data);
            index.names.insert(key, entry);
        }) {
            log::error!("Media source {}: {error:#}", path.display());
        }
    }
    if fingerprint != source_fingerprint(path) {
        index.names.clear();
        index.bytes = source_bytes(path);
        index.valid = false;
    }
    index
}

fn load_source(path: &Path, cache: &mut Cache) {
    let was_skipped = cache.skipped_sources.contains_key(path);
    if let Some(previous) = cache.skipped_sources.get(path) {
        if *previous == source_fingerprint(path) {
            return;
        }
        cache.forget_skipped_source(path);
    }
    if cache.suppress_new_sources
        && !was_skipped
        && !cache.sources.contains_key(path)
        && !cache.resolves_resident_names(path)
    {
        return;
    }
    if cache.defer_cold {
        if KEEP_NAME_INDEXES.with(std::cell::Cell::get) && cache.sources.contains_key(path) {
            return;
        }
        schedule_name_index(
            path,
            cache.budget.unwrap_or(METADATA_BUDGET),
            cache.sources.get(path).copied(),
        );
        return;
    }
    let fingerprint = stat(path).map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
    let old = cache.sources.get(path);
    if old == Some(&fingerprint) {
        return;
    }
    if old.is_some() {
        changed();
        cache.clear_records();
    }
    let index = read_name_index_records(
        path,
        cache.budget.unwrap_or(METADATA_BUDGET),
        |key, data| {
            cache.insert(key, path.parent().unwrap().into(), data);
        },
    );
    if !index.valid {
        cache.clear_records();
    }
    cache.insert_source(path.into(), index);
}

// A single worker and a bounded queue keep both outstanding XML and completion payloads bounded.
const INDEX_WORKER_BUDGET: usize = 1024 * 1024;
#[cfg(test)]
static INDEX_PRIORITY_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn index_background_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        if libc::setpriority(libc::PRIO_PROCESS, 0, 10) != 0 {
            log::warn!("Cannot lower media index worker priority: {}", std::io::Error::last_os_error());
        }
    }
    #[cfg(test)]
    {
        assert_eq!(std::thread::current().name(), Some("media-names"));
        INDEX_PRIORITY_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}
type IndexJob = (PathBuf, usize, u64, Option<Fingerprint>);
type IndexDone = (PathBuf, Option<NameIndex>, u64);
struct IndexWorker {
    send: std::sync::mpsc::SyncSender<IndexJob>,
    recv: std::sync::mpsc::Receiver<IndexDone>,
    pending: std::collections::BTreeSet<PathBuf>,
    waiting: std::collections::VecDeque<IndexJob>,
}
impl IndexWorker {
    fn dispatch(&mut self) {
        if let Some(job) = self.waiting.pop_front() {
            if let Err(std::sync::mpsc::TrySendError::Full(job)) = self.send.try_send(job) {
                self.waiting.push_front(job);
            }
        }
    }

    fn new() -> Self {
        let (send, jobs) = std::sync::mpsc::sync_channel::<IndexJob>(1);
        let (done, recv) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("media-names".into())
            .spawn(move || {
                index_background_priority();
                while let Ok((path, budget, epoch, previous)) = jobs.recv() {
                    let current = stat(&path)
                        .map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
                    let index = (previous != Some(current))
                        .then(|| read_name_index(&path, budget.min(INDEX_WORKER_BUDGET)));
                    if done.send((path, index, epoch)).is_err() {
                        break;
                    }
                }
            })
            .expect("media index worker");
        Self {
            send,
            recv,
            pending: Default::default(),
            waiting: Default::default(),
        }
    }
}
thread_local! {
    static INDEX_WORKER: RefCell<Option<IndexWorker>> = const { RefCell::new(None) };
    static UI_NAMES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static INDEX_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static NAME_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
fn schedule_name_index(path: &Path, budget: usize, previous: Option<Fingerprint>) {
    INDEX_WORKER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let worker = slot.get_or_insert_with(IndexWorker::new);
        if worker.pending.len() >= PATH_CACHE_LIMIT || worker.pending.contains(path) {
            return;
        }
        let epoch = INDEX_EPOCH.with(std::cell::Cell::get);
        worker
            .waiting
            .push_back((path.into(), budget, epoch, previous));
        worker.pending.insert(path.into());
        worker.dispatch();
    });
}
/// Adopt completed indexes without waiting for the worker or opening files.
/// Call from the owning view's update hook, including when no redraw is pending.
pub fn poll_name_indexes() {
    INDEX_WORKER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(worker) = slot.as_mut() else { return };
        while let Ok((path, index, epoch)) = worker.recv.try_recv() {
            worker.pending.remove(&path);
            let current_epoch = INDEX_EPOCH.with(std::cell::Cell::get);
            if epoch != current_epoch {
                let budget = CACHE.with(|c| c.borrow().budget.unwrap_or(METADATA_BUDGET));
                worker
                    .waiting
                    .push_back((path.clone(), budget, current_epoch, None));
                worker.pending.insert(path);
                continue;
            }
            let Some(index) = index else { continue };
            let visible = CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                if index.fingerprint.is_some() || !index.valid {
                    cache.clear_records();
                }
                cache.insert_source(path, index)
            });
            if visible {
                NAME_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
            }
            changed();
        }
        worker.dispatch();
    });
}
pub fn name_generation() -> u64 {
    NAME_GENERATION.with(std::cell::Cell::get)
}
/// UI hydration is cache-only. All subsequent cold media lookups on this thread defer XML too.
pub fn defer_cold_sources() {
    UI_NAMES.with(|ui| ui.set(true));
    CACHE.with(|c| c.borrow_mut().defer_cold = true);
}
pub fn cached_name(path: &Path) -> Option<String> {
    defer_cold_sources();
    let record = cached_metadata(path);
    if !record.name.is_empty() {
        return Some(record.name.clone());
    }
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let (source, identity) = identity_source_policy(path, &mut cache, false)?;
        cache
            .names
            .get(&source)?
            .get(&identity)
            .map(|entry| entry.name.clone())
    })
}
/// Fingerprints are checked at directory load or explicit refresh, never at frame preparation.
pub fn check_directory(parent: &Path) {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        cache.local_containers.clear();
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
fn launch_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        ["cue", "m3u", "chd", "pbp", "iso"]
            .iter()
            .any(|ext| e.eq_ignore_ascii_case(ext))
    })
}
fn local_container(path: &Path, cache: &mut Cache) -> Option<PathBuf> {
    let key = normalize(path);
    if let Some(found) = cache.local_containers.get(&key) {
        return found.clone();
    }
    let found = (|| {
        let parent = path.parent()?;
        if !launch_file(path) || !is_file(path) {
            return None;
        }
        if path.file_stem() == parent.file_name() {
            return Some(parent.into());
        }
        io_op();
        let mut launches = 0;
        for entry in fs::read_dir(parent).ok()? {
            let entry = entry.ok()?;
            if launch_file(&entry.path()) && entry.file_type().ok()?.is_file() {
                launches += 1;
                if launches > 1 {
                    return None;
                }
            }
        }
        (launches == 1).then(|| parent.into())
    })();
    if cache.local_containers.len() >= PATH_CACHE_LIMIT {
        cache.local_containers.clear();
    }
    cache.local_containers.insert(key, found.clone());
    found
}
fn identity_source(path: &Path, cache: &mut Cache) -> Option<(PathBuf, PathBuf)> {
    identity_source_policy(path, cache, true)
}
fn identity_source_policy(
    path: &Path,
    cache: &mut Cache,
    discover_container: bool,
) -> Option<(PathBuf, PathBuf)> {
    let mut parents = Vec::new();
    for parent in path.ancestors().skip(1).take(8) {
        parents.push(parent);
        if parent.file_name().is_some_and(|n| n == "Roms") {
            break;
        }
    }
    let unresolved = std::cell::Cell::new(false);
    let find = |identity: &Path, cache: &mut Cache| {
        let key = normalize(identity);
        for parent in &parents {
            for name in ["gamelist.xml", "miyoogamelist.xml"] {
                let source = parent.join(name);
                if !cache.sources.contains_key(&source) {
                    load_source(&source, cache);
                    if !cache.sources.contains_key(&source)
                        && !cache.skipped_sources.contains_key(&source)
                    {
                        unresolved.set(true);
                        continue;
                    }
                }
                if cache
                    .sources
                    .get(&source)
                    .or_else(|| cache.skipped_sources.get(&source))
                    .is_some_and(Option::is_some)
                {
                    if cache
                        .names
                        .get(&source)
                        .and_then(|names| names.get(&key))
                        .is_some_and(|entry| entry.len > 0)
                    {
                        return (!unresolved.get()).then_some((source, key));
                    }
                    break;
                }
            }
        }
        None
    };
    // Exact identities win across all sources before an immediate game container is tried.
    find(path, cache).or_else(|| {
        if unresolved.get() {
            return None;
        }
        let container = game_container(path).or_else(|| {
            if discover_container {
                local_container(path, cache)
            } else {
                cache
                    .local_containers
                    .get(&normalize(path))
                    .cloned()
                    .flatten()
            }
        })?;
        find(&container, cache)
    })
}
/// Hydration uses the source name index, never reparsing evicted full metadata.
pub fn name(path: &Path) -> Option<String> {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let (source, identity) = identity_source(path, &mut cache)?;
        cache
            .names
            .get(&source)?
            .get(&identity)
            .map(|entry| entry.name.clone())
    })
}
fn metadata_with_parent(path: &Path, cache: &mut Cache) -> (PathBuf, SharedMetadata, PathBuf) {
    if cache.defer_cold {
        if let Some(record) = cache.records.get(&normalize(path)) {
            let identity = if record.data.path.as_os_str().is_empty() {
                path.into()
            } else {
                normalize(&record.parent.join(&record.data.path))
            };
            return (record.parent.clone(), record.data.clone(), identity);
        }
    }
    if let Some((source_path, identity)) = identity_source(path, cache) {
        if let Some(record) = source(&source_path, &identity, cache) {
            if identity != normalize(path) {
                cache.insert(
                    normalize(path),
                    record.parent.clone(),
                    (*record.data).clone(),
                );
            }
            return (record.parent, record.data, identity);
        }
    }
    let system = path
        .ancestors()
        .skip(1)
        .take(8)
        .find(|parent| {
            parent
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == "Roms")
        })
        .or_else(|| path.parent())
        .unwrap_or(Path::new("."));
    (system.into(), SharedMetadata::default(), path.into())
}
pub fn metadata(path: &Path) -> SharedMetadata {
    CACHE.with(|c| metadata_with_parent(path, &mut c.borrow_mut()).1)
}
/// No storage access, including when a record was evicted.
pub fn cached_metadata(path: &Path) -> SharedMetadata {
    try_cached_metadata(path).unwrap_or_default()
}
/// Distinguish a resident record from a cold or evicted identity without storage access.
pub fn try_cached_metadata(path: &Path) -> Option<SharedMetadata> {
    CACHE.with(|c| {
        c.borrow()
            .records
            .get(&normalize(path))
            .map(|record| record.data.clone())
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
        let (parent, data, identity) = metadata_with_parent(path, &mut cache);
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
        let inherited_stem = identity.file_stem().unwrap_or_default();
        let kinds: &[&str] = match kind {
            Kind::Cover | Kind::PreparedCover => &["covers", "box2dfront"],
            Kind::TallCover => &["covers-tall"],
            Kind::Logo => &["logos", "marquees", "wheel"],
            Kind::Background => &["backgrounds", "fanart"],
            Kind::Screenshot => &["screenshots"],
        };
        for art_stem in [
            Some(stem),
            (inherited_stem != stem).then_some(inherited_stem),
        ]
        .into_iter()
        .flatten()
        {
            for folder in kinds {
                for ext in ["png", "jpg"] {
                    let mut filename = art_stem.to_os_string();
                    filename.push(format!(".{ext}"));
                    let candidate = parent.join("media").join(folder).join(filename);
                    if found.is_none() && is_file(candidate.as_path()) {
                        found = Some(candidate);
                    }
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
    fn resolved_path_revision_tracks_insert_overwrite_eviction_and_every_clear() {
        invalidate();
        let mut cache = Cache::default();
        let game = PathBuf::from("path-revision.chd");
        let before = path_generation();
        cache.insert_path((game.clone(), Kind::Cover), None);
        assert_eq!(path_generation(), before.wrapping_add(1));
        cache.insert_path((game.clone(), Kind::Cover), Some("cover.png".into()));
        assert_eq!(path_generation(), before.wrapping_add(2));
        for i in 1..PATH_CACHE_LIMIT {
            cache.insert_path((PathBuf::from(format!("revision-{i}")), Kind::Cover), None);
        }
        let full = path_generation();
        cache.insert_path(("evicts-first".into(), Kind::Cover), None);
        assert_eq!(path_generation(), full.wrapping_add(2), "eviction and insert each bump");
        assert!(!cache.paths.contains_key(&(game, Kind::Cover)));
        let before = path_generation();
        cache.clear_records();
        assert_ne!(path_generation(), before);
        let before = path_generation();
        clear_prepared_paths();
        assert_ne!(path_generation(), before);
        let before = path_generation();
        invalidate();
        assert_ne!(path_generation(), before);
        let before = path_generation();
        with_name_indexes(invalidate);
        assert_ne!(path_generation(), before);
    }

    #[test]
    fn wave2_metadata_worker_lowers_priority_before_processing() {
        let before = INDEX_PRIORITY_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        let worker = IndexWorker::new();
        worker.send.send((PathBuf::from("/missing-wave2-name-index.xml"), 4096, 0, None)).unwrap();
        worker.recv.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(INDEX_PRIORITY_CALLS.load(std::sync::atomic::Ordering::Relaxed) > before);
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
                parent: "/fixture".into(),
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
            parent: "/fixture".into(),
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
    fn hydration_indexes_each_source_once_despite_misses_and_eviction() {
        invalidate();
        PARSES.with(|c| c.borrow_mut().clear());
        set_metadata_budget(512 * 1024);
        let root =
            std::env::temp_dir().join(format!("allium-hydrate-index-{}", std::process::id()));
        let mut rows = Vec::new();
        for system in ["PS", "SFC", "NES"] {
            let dir = root.join("Roms").join(system);
            fs::create_dir_all(&dir).unwrap();
            let mut xml = String::from("<gameList>");
            for n in 0..34 {
                xml.push_str(&format!(
                    "<game><path>./G{n}.chd</path><name>Title {n}</name><desc>{}</desc></game>",
                    "D".repeat(8192)
                ));
            }
            if system == "PS" {
                for n in 0..50 {
                    xml.push_str(&format!("<game><path>./Network{n}</path><name>Network title {n}</name><desc>{}</desc></game>", "N".repeat(8192)));
                }
            }
            xml.push_str("</gameList>");
            fs::write(dir.join("gamelist.xml"), xml).unwrap();
        }
        for n in 0..100 {
            let system = ["PS", "SFC", "NES"][n % 3];
            rows.push((
                root.join("Roms")
                    .join(system)
                    .join(format!("G{}.chd", n / 3)),
                format!("Title {}", n / 3),
            ));
        }
        let containers = (0..50)
            .map(|n| {
                let container = root.join(format!("Roms/PS/Network{n}"));
                let launch = container.join("Discs/Launch.cue");
                rows.push((launch.clone(), format!("Network title {n}")));
                (launch, container)
            })
            .collect::<Vec<_>>();
        set_game_containers(containers);
        for _ in 0..2 {
            for (path, title) in &rows {
                assert_eq!(name(path).as_deref(), Some(title.as_str()));
                assert_eq!(metadata(path).name, *title);
            }
            for system in ["PS", "SFC", "NES"] {
                check_directory(&root.join("Roms").join(system));
            }
            for n in 0..600 {
                assert!(name(&root.join(format!("Roms/PS/Absent{n}.chd"))).is_none());
                assert!(
                    metadata(&root.join(format!("Roms/PS/Absent{n}.chd")))
                        .name
                        .is_empty()
                );
            }
        }
        PARSES.with(|c| {
            let counts = c.borrow();
            assert_eq!(counts.len(), 3);
            for system in ["PS", "SFC", "NES"] {
                assert_eq!(
                    counts.get(&root.join("Roms").join(system).join("gamelist.xml")),
                    Some(&1)
                );
            }
        });
        assert!(CACHE.with(|c| c.borrow().bytes) <= 512 * 1024);
        assert!(!CACHE.with(|c| c.borrow().records.is_empty()));
        assert!(cached_metadata(&rows[0].0).name.is_empty());
        assert_eq!(name(&rows[0].0).as_deref(), Some(rows[0].1.as_str()));
        let ps = root.join("Roms/PS");
        fs::write(
            ps.join("gamelist.xml"),
            "<gameList><game><path>./G0.chd</path><name>Changed title</name></game></gameList>",
        )
        .unwrap();
        check_directory(&ps);
        assert_eq!(name(&ps.join("G0.chd")).as_deref(), Some("Changed title"));
        assert!(name(&ps.join("Network0/Discs/Launch.cue")).is_none());
        assert_eq!(
            name(&root.join("Roms/NES/G0.chd")).as_deref(),
            Some("Title 0")
        );
        PARSES.with(|c| {
            assert_eq!(c.borrow().get(&ps.join("gamelist.xml")), Some(&2));
            assert_eq!(
                c.borrow().get(&root.join("Roms/NES/gamelist.xml")),
                Some(&1)
            );
        });
        fs::remove_dir_all(root).unwrap();
        set_game_containers(std::iter::empty());
        invalidate();
    }
    #[test]
    fn name_indexes_and_absent_sources_share_the_retention_budget() {
        let root = std::env::temp_dir().join(format!("allium-index-budget-{}", std::process::id()));
        for budget in [METADATA_BUDGET, INDEX_WORKER_BUDGET] {
            invalidate();
            set_metadata_budget(budget);
            for n in 0..90 {
                let dir = root.join(format!("Roms/S{n}"));
                fs::create_dir_all(&dir).unwrap();
                let mut xml = String::from("<gameList>");
                for game in 0..180 {
                    xml.push_str(&format!(
                        "<game><path>./G{game}.chd</path><name>{n} Title {game}</name></game>"
                    ));
                }
                xml.push_str("</gameList>");
                fs::write(dir.join("gamelist.xml"), xml).unwrap();
                let admit_source = CACHE.with(|c| !c.borrow().suppress_new_sources);
                check_directory(&dir);
                CACHE.with(|c| {
                    let c = c.borrow();
                    assert!(c.bytes <= budget);
                    let charged = c.source_order.iter().map(|(_, bytes)| bytes).sum::<usize>()
                        + c.order.iter().map(|(_, bytes)| bytes).sum::<usize>();
                    assert_eq!(c.bytes, charged + c.skipped_bytes);
                    assert_eq!(c.names.len(), c.sources.len());
                    assert_eq!(c.source_order.len(), c.sources.len());
                    assert_eq!(
                        c.names.contains_key(&dir.join("gamelist.xml")),
                        admit_source
                    );
                });
            }
            CACHE.with(|c| {
                let c = c.borrow();
                assert!(!c.sources.contains_key(&root.join("Roms/S0/gamelist.xml")));
                assert!(!c.names.contains_key(&root.join("Roms/S0/gamelist.xml")));
                assert!(c.sources.values().any(Option::is_none));
            });
        }
        fs::remove_dir_all(root).unwrap();
        invalidate();
    }

    #[test]
    fn oversized_source_keeps_a_bounded_resident_prefix_without_scratch_files() {
        const ISOLATED: &str = "ALLIUM_TEST_ISOLATED_SCRATCH_FDS";
        // The fd assertion covers the whole process, so run it without other tests.
        if std::env::var_os(ISOLATED).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "media::tests::oversized_source_keeps_a_bounded_resident_prefix_without_scratch_files",
                    "--test-threads=1",
                ])
                .env(ISOLATED, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated scratch fd check failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use std::os::unix::fs::MetadataExt;
        let root =
            std::env::temp_dir().join(format!("allium-index-bounded-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("gamelist.xml");
        let mut xml = String::from("<gameList>");
        for n in 0..6000 {
            xml.push_str(&format!(
                "<game><path>G{n}.chd</path><name>Title {n} {}</name></game>",
                "N".repeat(900)
            ));
        }
        xml.push_str("</gameList>");
        fs::write(&source, xml).unwrap();
        let scratch_files = || -> std::collections::BTreeSet<_> {
            fs::read_dir("/dev/fd")
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| fs::File::open(entry.path()).ok()?.metadata().ok())
                .filter(|m| m.is_file() && m.nlink() == 0)
                .map(|m| (m.dev(), m.ino()))
                .collect()
        };
        for budget in [METADATA_BUDGET, INDEX_WORKER_BUDGET] {
            let before = scratch_files();
            let index = read_name_index(&source, budget);
            assert!(
                scratch_files().is_subset(&before),
                "index retained an unlinked scratch file"
            );
            assert!(index.bytes <= budget);
            assert!(!index.names.is_empty());
            assert!(index.names.len() < 6000);
            assert!(index.names.contains_key(&root.join("G0.chd")));
            assert!(!index.names.contains_key(&root.join("G5999.chd")));
            assert_eq!(
                index.bytes,
                source_bytes(&source)
                    + index
                        .names
                        .iter()
                        .map(|(p, e)| entry_bytes(p, e))
                        .sum::<usize>()
            );
            let mut cache = Cache {
                budget: Some(budget),
                ..Cache::default()
            };
            cache.insert_source(source.clone(), index);
            assert!(cache.names[&source].contains_key(&root.join("G0.chd")));
            assert!(self::source(&source, &root.join("G5999.chd"), &mut cache).is_none());
            assert!(cache.bytes <= budget);
            assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reordered_source_rejects_stale_offsets_before_deferred_validation() {
        let root = std::env::temp_dir().join(format!("allium-stale-offset-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let xml = root.join("gamelist.xml");
        let a = "<game><path>A.chd</path><name>Alpha</name></game>";
        let b = "<game><path>B.chd</path><name>Bravo</name></game>";
        fs::write(&xml, format!("<gameList>{a}{b}</gameList>")).unwrap();
        let mut cache = Cache {
            defer_cold: true,
            ..Cache::default()
        };
        cache.insert_source(xml.clone(), read_name_index(&xml, METADATA_BUDGET));
        fs::write(&xml, format!("<gameList>{b}{a} </gameList>")).unwrap();
        load_source(&xml, &mut cache);
        assert!(source(&xml, &root.join("A.chd"), &mut cache).is_none());
        assert!(!cache.sources.contains_key(&xml));
        assert!(cache.records.is_empty());
        cache.insert_source(xml.clone(), read_name_index(&xml, METADATA_BUDGET));
        assert_eq!(
            source(&xml, &root.join("A.chd"), &mut cache)
                .unwrap()
                .data
                .name,
            "Alpha"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn decoded_record_identity_is_checked_even_with_a_matching_fingerprint() {
        let root =
            std::env::temp_dir().join(format!("allium-offset-identity-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let xml = root.join("gamelist.xml");
        let a = "<game><path>A.chd</path><name>Alpha</name></game>";
        let b = "<game><path>B.chd</path><name>Bravo</name></game>";
        fs::write(&xml, format!("<gameList>{a}{b}</gameList>")).unwrap();
        let mut index = read_name_index(&xml, METADATA_BUDGET);
        fs::write(&xml, format!("<gameList>{b}{a}</gameList>")).unwrap();
        index.fingerprint =
            stat(&xml).map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
        let mut cache = Cache::default();
        cache.insert_source(xml.clone(), index);
        assert!(source(&xml, &root.join("A.chd"), &mut cache).is_none());
        assert!(cache.records.is_empty());
        assert!(!cache.sources.contains_key(&xml));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_changed_during_construction_discards_the_index() {
        let root = std::env::temp_dir().join(format!("allium-index-race-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let xml = root.join("gamelist.xml");
        fs::write(
            &xml,
            "<gameList><game><path>A.chd</path><name>Alpha</name></game></gameList>",
        )
        .unwrap();
        let index = read_name_index_records(&xml, METADATA_BUDGET, |_, _| {
            fs::write(&xml, "<gameList><game><path>B.chd</path><name>Replacement Bravo</name></game></gameList>").unwrap();
        });
        assert!(index.names.is_empty());
        let mut cache = Cache::default();
        cache.insert_source(xml.clone(), index);
        assert!(!cache.sources.contains_key(&xml));
        cache.insert_source(xml.clone(), read_name_index(&xml, METADATA_BUDGET));
        assert_eq!(
            source(&xml, &root.join("B.chd"), &mut cache)
                .unwrap()
                .data
                .name,
            "Replacement Bravo"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn metadata_evicted_source_hydration_jobs_and_generation_settle() {
        invalidate();
        defer_cold_sources();
        let root =
            std::env::temp_dir().join(format!("allium-evicted-names-{}", std::process::id()));
        let mut games = Vec::new();
        for n in 0..6 {
            let dir = root.join(format!("Roms/S{n}"));
            fs::create_dir_all(&dir).unwrap();
            let mut xml = String::from("<gameList>");
            for game in 0..1200 {
                xml.push_str(&format!(
                    "<game><path>G{game}.chd</path><name>{}</name></game>",
                    "N".repeat(900)
                ));
            }
            xml.push_str("</gameList>");
            fs::write(dir.join("gamelist.xml"), xml).unwrap();
            games.push(dir.join("G0.chd"));
        }
        let (send, jobs) = std::sync::mpsc::sync_channel::<IndexJob>(1);
        let (done, recv) = std::sync::mpsc::sync_channel(1);
        let old_worker = INDEX_WORKER.with(|slot| {
            slot.replace(Some(IndexWorker {
                send,
                recv,
                pending: Default::default(),
                waiting: Default::default(),
            }))
        });
        let mut counts = BTreeMap::<PathBuf, usize>::new();
        let mut settled = 0;
        for round in 0..40 {
            for game in &games {
                let _ = cached_name(game);
            }
            if let Ok((path, budget, epoch, previous)) = jobs.try_recv() {
                *counts.entry(path.clone()).or_default() += 1;
                let index = (previous != Some(source_fingerprint(&path)))
                    .then(|| read_name_index(&path, budget.min(INDEX_WORKER_BUDGET)));
                if let Some(index) = &index {
                    if index.fingerprint.is_some() {
                        assert!(index.bytes > METADATA_BUDGET / 5);
                    }
                }
                done.send((path, index, epoch)).unwrap();
            }
            poll_name_indexes();
            if round == 25 {
                settled = name_generation();
            }
        }
        let final_generation = name_generation();
        INDEX_WORKER.with(|slot| {
            slot.replace(old_worker);
        });
        fs::remove_dir_all(root).unwrap();
        invalidate();
        for game in &games {
            let source = game.parent().unwrap().join("gamelist.xml");
            assert_eq!(
                counts.get(&source),
                Some(&1),
                "reindexed {}",
                source.display()
            );
        }
        assert!(counts.values().all(|count| *count == 1), "jobs: {counts:?}");
        assert_eq!(final_generation, settled, "idle generation did not settle");
    }

    #[test]
    fn metadata_marker_overflow_hydration_jobs_and_generation_settle() {
        invalidate();
        defer_cold_sources();
        let budget = 32 * 1024;
        set_metadata_budget(budget);
        let root =
            std::env::temp_dir().join(format!("allium-marker-overflow-{}", std::process::id()));
        let mut games = Vec::new();
        for n in 0..12 {
            let dir = root.join(format!("Roms/S{n:02}"));
            fs::create_dir_all(&dir).unwrap();
            let mut xml = String::from("<gameList>");
            for game in 0..40 {
                xml.push_str(&format!(
                    "<game><path>G{game}.chd</path><name>{}</name></game>",
                    "N".repeat(900)
                ));
            }
            xml.push_str("</gameList>");
            fs::write(dir.join("gamelist.xml"), xml).unwrap();
            games.push(dir.join("G0.chd"));
        }
        let sources = games
            .iter()
            .map(|game| game.parent().unwrap().join("gamelist.xml"))
            .collect::<Vec<_>>();
        assert!(
            sources.iter().map(|path| source_bytes(path)).sum::<usize>() > marker_budget(budget)
        );
        assert!(
            sources
                .iter()
                .map(|path| read_name_index(path, budget).bytes)
                .sum::<usize>()
                > resident_budget(budget)
        );
        let (send, jobs) = std::sync::mpsc::sync_channel::<IndexJob>(1);
        let (done, recv) = std::sync::mpsc::sync_channel(1);
        let old_worker = INDEX_WORKER.with(|slot| {
            slot.replace(Some(IndexWorker {
                send,
                recv,
                pending: Default::default(),
                waiting: Default::default(),
            }))
        });
        let drive = || {
            let mut counts = BTreeMap::<PathBuf, usize>::new();
            let mut settled = 0;
            for round in 0..300 {
                for game in &games {
                    let _ = cached_name(game);
                }
                if let Ok((path, budget, epoch, previous)) = jobs.try_recv() {
                    *counts.entry(path.clone()).or_default() += 1;
                    let index = (previous != Some(source_fingerprint(&path)))
                        .then(|| read_name_index(&path, budget.min(INDEX_WORKER_BUDGET)));
                    done.send((path, index, epoch)).unwrap();
                }
                poll_name_indexes();
                CACHE.with(|c| {
                    let c = c.borrow();
                    assert!(c.bytes <= budget);
                    assert!(c.skipped_bytes <= marker_budget(budget));
                });
                if round == 100 {
                    settled = name_generation();
                }
            }
            (counts, settled, name_generation())
        };
        let first = drive();
        invalidate();
        set_metadata_budget(budget);
        let refreshed = drive();
        INDEX_WORKER.with(|slot| {
            slot.replace(old_worker);
        });
        fs::remove_dir_all(root).unwrap();
        invalidate();
        for (counts, settled, final_generation) in [first, refreshed] {
            for source in &sources {
                assert_eq!(
                    counts.get(source),
                    Some(&1),
                    "reindexed {}",
                    source.display()
                );
            }
            assert!(counts.values().all(|count| *count == 1), "jobs: {counts:?}");
            assert_eq!(final_generation, settled, "idle generation did not settle");
        }
    }

    #[test]
    fn metadata_suppression_resolves_children_of_resident_ancestor_names() {
        invalidate();
        defer_cold_sources();
        let budget = 32 * 1024;
        set_metadata_budget(budget);
        let root =
            std::env::temp_dir().join(format!("allium-suppressed-ancestor-{}", std::process::id()));
        let dir = root.join("Roms/PS");
        fs::create_dir_all(dir.join("Nested/Discs")).unwrap();
        let ancestor = dir.join("gamelist.xml");
        fs::write(&ancestor, "<gameList><game><path>Nested/Discs/Exact.cue</path><name>Ancestor title</name></game><game><path>Nested/Discs/Closer.cue</path><name>Shadowed title</name></game><game><path>Container</path><name>Container title</name></game></gameList>").unwrap();
        fs::write(dir.join("Nested/gamelist.xml"), "<gameList><game><path>Discs/Closer.cue</path><name>Child title</name></game></gameList>").unwrap();
        CACHE.with(|c| {
            let mut cache = c.borrow_mut();
            cache.insert_source(ancestor.clone(), read_name_index(&ancestor, budget));
            // Exact identities in outer sources must be ruled out before container fallback.
            for name in ["gamelist.xml", "miyoogamelist.xml"] {
                let outer = root.join("Roms").join(name);
                cache.insert_source(outer.clone(), read_name_index(&outer, budget));
            }
            for n in 0..8 {
                cache.remember_skipped_source(root.join(format!("Skipped{n}/gamelist.xml")), None);
            }
            assert!(cache.suppress_new_sources);
        });
        let (send, jobs) = std::sync::mpsc::sync_channel::<IndexJob>(1);
        let (done, recv) = std::sync::mpsc::sync_channel(1);
        let old_worker = INDEX_WORKER.with(|slot| {
            slot.replace(Some(IndexWorker {
                send,
                recv,
                pending: Default::default(),
                waiting: Default::default(),
            }))
        });
        let exact = dir.join("Nested/Discs/Exact.cue");
        let closer = dir.join("Nested/Discs/Closer.cue");
        let launch = dir.join("Container/Discs/Launch.cue");
        set_game_containers([(launch.clone(), dir.join("Container"))]);
        let mut counts = BTreeMap::<PathBuf, usize>::new();
        let mut settled = 0;
        let mut names = (None, None, None);
        for round in 0..300 {
            names = (
                cached_name(&exact),
                cached_name(&closer),
                cached_name(&launch),
            );
            if let Ok((path, budget, epoch, previous)) = jobs.try_recv() {
                *counts.entry(path.clone()).or_default() += 1;
                let index = (previous != Some(source_fingerprint(&path)))
                    .then(|| read_name_index(&path, budget.min(INDEX_WORKER_BUDGET)));
                done.send((path, index, epoch)).unwrap();
            }
            poll_name_indexes();
            CACHE.with(|c| {
                let cache = c.borrow();
                assert!(cache.suppress_new_sources);
                assert!(cache.sources.contains_key(&ancestor));
                assert!(cache.bytes <= budget);
                assert!(cache.skipped_bytes <= marker_budget(budget));
            });
            if round == 100 {
                settled = name_generation();
            }
        }
        let final_generation = name_generation();
        INDEX_WORKER.with(|slot| {
            slot.replace(old_worker);
        });
        fs::remove_dir_all(root).unwrap();
        set_game_containers(std::iter::empty());
        invalidate();
        assert_eq!(names.0.as_deref(), Some("Ancestor title"));
        assert_eq!(names.1.as_deref(), Some("Child title"));
        assert_eq!(names.2.as_deref(), Some("Container title"));
        assert!(counts.values().all(|count| *count == 1), "jobs: {counts:?}");
        assert_eq!(final_generation, settled, "idle generation did not settle");
    }

    #[test]
    fn metadata_suppression_retries_discarded_source_after_read_recovers() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "allium-suppressed-read-retry-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("gamelist.xml");
        let game = root.join("G.chd");
        fs::write(
            &path,
            "<gameList><game><path>G.chd</path><name>Recovered title</name></game></gameList>",
        )
        .unwrap();
        let budget = 32 * 1024;
        let mut cache = Cache {
            budget: Some(budget),
            ..Cache::default()
        };
        cache.insert_source(path.clone(), read_name_index(&path, budget));
        for n in 0..8 {
            cache.remember_skipped_source(root.join(format!("Skipped{n}/gamelist.xml")), None);
        }
        assert!(cache.suppress_new_sources);
        let markers = cache.skipped_sources.clone();
        let fingerprint = source_fingerprint(&path);
        let permissions = fs::metadata(&path).unwrap().permissions();
        fs::set_permissions(&path, fs::Permissions::from_mode(0)).unwrap();
        assert_eq!(source_fingerprint(&path), fingerprint);
        let failed = source(&path, &game, &mut cache).is_none();
        fs::set_permissions(&path, permissions).unwrap();
        assert!(failed, "unreadable record unexpectedly loaded");
        assert!(!cache.sources.contains_key(&path));
        assert_eq!(source_fingerprint(&path), fingerprint);
        load_source(&path, &mut cache);
        let recovered = source(&path, &game, &mut cache).map(|record| record.data.name.clone());
        fs::remove_dir_all(root).unwrap();
        assert_eq!(recovered.as_deref(), Some("Recovered title"));
        assert_eq!(cache.skipped_sources, markers);
        assert!(cache.bytes <= budget);
        assert!(cache.skipped_bytes <= marker_budget(budget));
    }

    #[test]
    fn metadata_suppression_budget_growth_admits_new_source() {
        invalidate();
        let root = std::env::temp_dir().join(format!(
            "allium-suppressed-budget-growth-{}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("Cold")).unwrap();
        let path = root.join("gamelist.xml");
        let cold = root.join("Cold/gamelist.xml");
        let xml = "<gameList><game><path>G.chd</path><name>New title</name></game></gameList>";
        fs::write(&path, xml).unwrap();
        fs::write(&cold, xml).unwrap();
        set_metadata_budget(128 * 1024);
        CACHE.with(|c| {
            let mut cache = c.borrow_mut();
            cache.insert_source(path.clone(), read_name_index(&path, 128 * 1024));
            for n in 0..4 {
                cache.remember_skipped_source(root.join(format!("Skipped{n}/gamelist.xml")), None);
            }
            assert!(!cache.suppress_new_sources);
        });
        let reduced_budget = source_bytes(&root.join("Skipped0/gamelist.xml")) * 16;
        set_metadata_budget(reduced_budget);
        CACHE.with(|c| {
            let mut cache = c.borrow_mut();
            assert!(cache.suppress_new_sources);
            assert!(cache.bytes <= reduced_budget);
            load_source(&cold, &mut cache);
            assert!(!cache.sources.contains_key(&cold));
        });
        let markers = CACHE.with(|c| c.borrow().skipped_sources.clone());
        assert!(!markers.is_empty());
        set_metadata_budget(128 * 1024);
        let indexed = CACHE.with(|c| {
            let mut cache = c.borrow_mut();
            assert_eq!(cache.skipped_sources, markers);
            for source in markers.keys() {
                load_source(source, &mut cache);
                assert!(!cache.sources.contains_key(source));
            }
            load_source(&cold, &mut cache);
            assert!(cache.bytes <= 128 * 1024);
            assert!(cache.skipped_bytes <= marker_budget(128 * 1024));
            cache.names.get(&cold).and_then(|names| {
                names
                    .get(&root.join("Cold/G.chd"))
                    .map(|entry| entry.name.clone())
            })
        });
        fs::remove_dir_all(root).unwrap();
        invalidate();
        assert_eq!(indexed.as_deref(), Some("New title"));
    }

    #[test]
    fn metadata_marker_overflow_preserves_markers_until_resident_change() {
        let root =
            std::env::temp_dir().join(format!("allium-marker-refresh-{}", std::process::id()));
        fs::create_dir_all(root.join("Cold")).unwrap();
        let path = root.join("gamelist.xml");
        let cold = root.join("Cold/gamelist.xml");
        let xml = "<gameList><game><path>G.chd</path><name>Original</name></game></gameList>";
        fs::write(&path, xml).unwrap();
        fs::write(&cold, xml).unwrap();
        let mut cache = Cache {
            budget: Some(32 * 1024),
            ..Cache::default()
        };
        load_source(&path, &mut cache);
        let marked = root.join("Skipped0/gamelist.xml");
        for n in 0..8 {
            cache.remember_skipped_source(root.join(format!("Skipped{n}/gamelist.xml")), None);
        }
        assert!(cache.suppress_new_sources);
        assert!(cache.skipped_sources.contains_key(&marked));
        let marker_bytes = cache.skipped_bytes;
        cache.remember_skipped_source(marked.clone(), None);
        assert_eq!(cache.skipped_bytes, marker_bytes);
        load_source(&cold, &mut cache);
        assert!(!cache.sources.contains_key(&cold));
        load_source(&path, &mut cache);
        assert!(!cache.insert_source(path.clone(), read_name_index(&path, 32 * 1024)));
        assert!(cache.suppress_new_sources);
        fs::create_dir_all(marked.parent().unwrap()).unwrap();
        fs::write(&marked, xml).unwrap();
        load_source(&marked, &mut cache);
        assert!(cache.sources.contains_key(&marked));
        assert!(cache.suppress_new_sources);
        fs::write(&path, xml.replace("Original", "Changed resident title")).unwrap();
        load_source(&path, &mut cache);
        assert!(!cache.suppress_new_sources);
        load_source(&cold, &mut cache);
        assert!(cache.sources.contains_key(&cold));
        let markers = cache.skipped_sources.clone();
        cache.remember_skipped_source(PathBuf::from("X".repeat(32 * 1024)), None);
        assert!(cache.suppress_new_sources);
        assert_eq!(cache.skipped_sources, markers);
        assert!(cache.skipped_bytes <= marker_budget(32 * 1024));
        assert!(cache.bytes <= 32 * 1024);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn metadata_budget_markers_retry_on_fingerprint_change_and_invalidation() {
        invalidate();
        let root = std::env::temp_dir().join(format!("allium-name-marker-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("gamelist.xml");
        fs::write(&path, "<gameList/>").unwrap();
        let mut cache = Cache {
            budget: Some(64 * 1024),
            ..Cache::default()
        };
        assert!(!cache.insert_source(
            path.clone(),
            NameIndex {
                fingerprint: source_fingerprint(&path),
                names: BTreeMap::new(),
                valid: true,
                bytes: 64 * 1024,
            }
        ));
        assert!(cache.skipped_sources.contains_key(&path));
        PARSES.with(|p| p.borrow_mut().clear());
        load_source(&path, &mut cache);
        PARSES.with(|p| assert!(p.borrow().is_empty()));
        fs::write(
            &path,
            "<gameList><game><path>G.chd</path><name>Changed</name></game></gameList>",
        )
        .unwrap();
        load_source(&path, &mut cache);
        assert!(!cache.skipped_sources.contains_key(&path));
        assert_eq!(cache.names[&path][&root.join("G.chd")].name, "Changed");
        assert_eq!(PARSES.with(|p| p.borrow()[&path]), 1);
        cache.evict_source().unwrap();
        assert!(cache.skipped_sources.contains_key(&path));
        CACHE.with(|c| {
            c.replace(cache);
        });
        invalidate();
        CACHE.with(|c| assert!(c.borrow().skipped_sources.is_empty()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn metadata_noop_index_completions_do_not_bump_name_generation() {
        invalidate();
        let path = PathBuf::from("/metadata-noop/gamelist.xml");
        let game = PathBuf::from("/metadata-noop/G.chd");
        let index = |start| NameIndex {
            fingerprint: Some((100, SystemTime::UNIX_EPOCH)),
            names: [(
                game.clone(),
                SourceEntry {
                    name: "Title".into(),
                    start,
                    len: 30,
                },
            )]
            .into(),
            valid: true,
            bytes: 2000,
        };
        CACHE.with(|c| {
            c.borrow_mut().insert_source(path.clone(), index(10));
        });
        let (send, _jobs) = std::sync::mpsc::sync_channel::<IndexJob>(1);
        let (done, recv) = std::sync::mpsc::sync_channel(1);
        let old_worker = INDEX_WORKER.with(|slot| {
            slot.replace(Some(IndexWorker {
                send,
                recv,
                pending: Default::default(),
                waiting: Default::default(),
            }))
        });
        let before = name_generation();
        let epoch = INDEX_EPOCH.with(std::cell::Cell::get);
        for completion in [
            None,
            Some(index(20)),
            Some(NameIndex {
                fingerprint: None,
                names: BTreeMap::new(),
                valid: true,
                bytes: 1000,
            }),
        ] {
            let source = if completion.as_ref().is_some_and(|i| i.fingerprint.is_none()) {
                PathBuf::from("/metadata-noop/miyoogamelist.xml")
            } else {
                path.clone()
            };
            done.send((source, completion, epoch)).unwrap();
            poll_name_indexes();
        }
        INDEX_WORKER.with(|slot| {
            slot.replace(old_worker);
        });
        assert_eq!(name_generation(), before);
        invalidate();
    }

    #[test]
    fn resolving_logo_at_path_cache_limit_keeps_visible_cover() {
        invalidate();
        let game = PathBuf::from("/fixture/selected.sfc");
        let cover = PathBuf::from("/fixture/cover.png");
        CACHE.with(|c| {
            let mut cache = c.borrow_mut();
            for i in 0..PATH_CACHE_LIMIT - 1 {
                cache.insert_path(
                    (PathBuf::from(format!("/fixture/{i}.sfc")), Kind::Cover),
                    None,
                );
            }
            cache.insert_path((game.clone(), Kind::Cover), Some(cover.clone()));
            cache.insert_path((game.clone(), Kind::Logo), Some("/fixture/logo.png".into()));
            assert_eq!(cache.paths.len(), PATH_CACHE_LIMIT);
        });
        assert_eq!(cached_resolve(&game, Kind::Cover), Some(cover));
        invalidate();
    }

    #[test]
    fn cached_name_never_probes_unregistered_launch_containers() {
        invalidate();
        let root = std::env::temp_dir().join(format!("allium-cached-name-{}", std::process::id()));
        let dir = root.join("Roms/PS/Game");
        fs::create_dir_all(&dir).unwrap();
        let launch = dir.join("Launch.cue");
        fs::write(&launch, b"fixture").unwrap();
        fs::write(
            root.join("Roms/PS/gamelist.xml"),
            "<gameList><game><path>Game</path><name>Container</name></game></gameList>",
        )
        .unwrap();
        check_directory(&dir);
        let io = crate::perf_trace::frontend_io_ops();
        assert_eq!(cached_name(&launch), None);
        assert_eq!(crate::perf_trace::frontend_io_ops() - io, 0);
        set_game_containers([(launch.clone(), dir)]);
        let io = crate::perf_trace::frontend_io_ops();
        assert_eq!(cached_name(&launch).as_deref(), Some("Container"));
        assert_eq!(crate::perf_trace::frontend_io_ops() - io, 0);
        set_game_containers(std::iter::empty());
        fs::remove_dir_all(root).unwrap();
        invalidate();
    }

    #[test]
    fn metadata_only_snapshot_does_not_prepare_art_paths() {
        invalidate();
        let path = fixture().join("Complete.sfc");
        let io = crate::perf_trace::frontend_io_ops();
        assert!(try_cached_metadata(&path).is_none());
        assert_eq!(crate::perf_trace::frontend_io_ops(), io);
        let prepared = prepare_game_kinds(&path, &[]);
        let io = crate::perf_trace::frontend_io_ops();
        assert_eq!(try_cached_metadata(&path).as_deref(), Some(prepared.metadata.as_ref()));
        assert_eq!(crate::perf_trace::frontend_io_ops(), io);
        assert!(!prepared.metadata.name.is_empty());
        assert!(prepared.paths.is_empty());
        assert!(CACHE.with(|c| c.borrow().paths.is_empty()));
        let prepared = prepare_game_kinds(&path, &[Kind::Logo]);
        assert_eq!(prepared.paths.len(), 1);
        assert_eq!(prepared.paths[0].0, Kind::Logo);
        assert!(prepared.paths[0].1.is_some());
        invalidate();
        assert!(try_cached_metadata(&path).is_none());
    }

    #[test]
    fn cold_ui_names_defer_xml_and_refresh_from_worker_indexes() {
        invalidate();
        PARSES.with(|p| p.borrow_mut().clear());
        let root = std::env::temp_dir().join(format!("allium-async-name-{}", std::process::id()));
        let dir = root.join("Roms/PS");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("gamelist.xml"), "<gameList><game><path>./Game</path><name>Worker title</name></game><game><path>./Game/Discs/Exact.cue</path><name>Exact title</name></game></gameList>").unwrap();
        let launch = dir.join("Game/Discs/Launch.cue");
        let exact = dir.join("Game/Discs/Exact.cue");
        set_game_containers([
            (launch.clone(), dir.join("Game")),
            (exact.clone(), dir.join("Game")),
        ]);
        let generation = name_generation();
        assert_eq!(cached_name(&launch), None);
        // Art lookup on this UI thread must defer cold XML as well.
        assert!(metadata(&launch).name.is_empty());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            poll_name_indexes();
            if cached_name(&launch).as_deref() == Some("Worker title") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "index worker did not finish"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_ne!(generation, name_generation());
        assert_eq!(cached_name(&exact).as_deref(), Some("Exact title"));
        PARSES.with(|p| assert!(p.borrow().is_empty(), "UI thread parsed XML"));
        assert!(CACHE.with(|c| c.borrow().bytes) <= METADATA_BUDGET);
        fs::remove_dir_all(root).unwrap();
        set_game_containers(std::iter::empty());
        invalidate();
    }

    #[test]
    fn inherited_folder_art_uses_container_stem_and_exact_file_wins() {
        invalidate();
        let root =
            std::env::temp_dir().join(format!("allium-container-art-{}", std::process::id()));
        let system = root.join("Roms/PS");
        fs::create_dir_all(system.join("Game/Discs")).unwrap();
        fs::write(system.join("gamelist.xml"), "<gameList><game><path>./Game</path><name>Container</name></game><game><path>./Game/Discs/Exact.cue</path><name>Exact</name></game></gameList>").unwrap();
        let launch = system.join("Game/Discs/Launch.cue");
        let exact = system.join("Game/Discs/Exact.cue");
        set_game_containers([
            (launch.clone(), system.join("Game")),
            (exact.clone(), system.join("Game")),
        ]);
        for (kind, folder) in [
            (Kind::Cover, "covers"),
            (Kind::Background, "backgrounds"),
            (Kind::Logo, "logos"),
        ] {
            let dir = system.join("media").join(folder);
            fs::create_dir_all(&dir).unwrap();
            for stem in ["Game", "Exact"] {
                fs::write(dir.join(format!("{stem}.png")), b"fixture").unwrap();
            }
            assert_eq!(resolve(&launch, kind), Some(dir.join("Game.png")));
            assert_eq!(resolve(&exact, kind), Some(dir.join("Exact.png")));
            fs::write(dir.join("Launch.png"), b"exact file art").unwrap();
            clear_prepared_paths();
            assert_eq!(resolve(&launch, kind), Some(dir.join("Launch.png")));
        }
        fs::remove_dir_all(root).unwrap();
        set_game_containers(std::iter::empty());
        invalidate();
    }

    #[test]
    fn directory_inheritance_requires_a_verified_immediate_container() {
        invalidate();
        let root =
            std::env::temp_dir().join(format!("allium-container-scope-{}", std::process::id()));
        for dir in ["Collection", "Grandchild/Sub", "Named", "Single"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in [
            "Collection/GameA.cue",
            "Collection/GameB.cue",
            "Grandchild/Sub/Launch.cue",
            "Named/Named.cue",
            "Named/Disc2.chd",
            "Single/Disc.iso",
        ] {
            fs::write(root.join(file), b"fixture").unwrap();
        }
        fs::write(
            root.join("gamelist.xml"),
            "<gameList>
            <game><path>./Collection</path><name>Collection title</name></game>
            <game><path>./Grandchild</path><name>Grandparent title</name></game>
            <game><path>./Named</path><name>Named title</name></game>
            <game><path>./Single</path><name>Single title</name></game>
            <game><path>./Network</path><name>Explicit network title</name></game>
            </gameList>",
        )
        .unwrap();
        for file in [
            "Collection/GameA.cue",
            "Collection/GameB.cue",
            "Grandchild/Sub/Launch.cue",
            "Network/Discs/Launch.cue",
        ] {
            assert!(name(&root.join(file)).is_none(), "{file}");
            assert!(metadata(&root.join(file)).name.is_empty(), "{file}");
        }
        for (file, title) in [
            ("Named/Named.cue", "Named title"),
            ("Single/Disc.iso", "Single title"),
        ] {
            assert_eq!(name(&root.join(file)).as_deref(), Some(title));
            assert_eq!(metadata(&root.join(file)).name, title);
        }
        let launch = root.join("Network/Discs/Launch.cue");
        set_game_containers([(launch.clone(), root.join("Network"))]);
        assert_eq!(name(&launch).as_deref(), Some("Explicit network title"));
        assert_eq!(metadata(&launch).name, "Explicit network title");
        assert_eq!(cached_metadata(&launch).name, "Explicit network title");
        fs::remove_dir_all(root).unwrap();
        set_game_containers(std::iter::empty());
        invalidate();
    }
    #[test]
    fn directory_rom_metadata_keeps_exact_priority_source_and_cached_identity() {
        invalidate();
        let root =
            std::env::temp_dir().join(format!("allium-directory-metadata-{}", std::process::id()));
        fs::create_dir_all(root.join("Dir/Sub")).unwrap();
        fs::create_dir_all(root.join("Spaces 100% literal")).unwrap();
        fs::create_dir_all(root.join("Sibling")).unwrap();
        fs::create_dir_all(root.join("art")).unwrap();
        for file in [
            "Dir/Dir.cue",
            "Dir/Dir.bin",
            "Dir/Sub/Exact.cue",
            "Spaces 100% literal/Disc.cue",
            "Sibling/Other.cue",
            "Single.nes",
            "Prefix.nes",
            "Spaces 100%.nes",
        ] {
            fs::write(root.join(file), b"fixture").unwrap();
        }
        for file in ["cover.png", "logo.png"] {
            fs::write(root.join("art").join(file), b"distinct media").unwrap();
        }
        fs::write(root.join("gamelist.xml"), "<gameList>
            <game><path>./Dir/</path><name>Directory title</name><releasedate>19981203T000000</releasedate><desc>Directory synopsis</desc><image>./art/cover.png</image><marquee>./art/logo.png</marquee></game>
            <game><path>./Spaces 100% literal</path><name>Literal directory</name></game>
            <game><path>./Dir/Sub/Exact.cue</path><name>Exact nested title</name><marquee>./art/logo.png</marquee></game>
            <game><path>Single.nes</path><name>Single title</name></game>
            <game><path>./Prefix.nes</path><name>Prefix title</name></game>
            <game><path>./Spaces 100%.nes</path><name>Literal file</name></game>
            </gameList>").unwrap();
        fs::write(
            root.join("Dir/gamelist.xml"),
            "<gameList><game><path>./Sub</path><name>Closer directory</name></game></gameList>",
        )
        .unwrap();
        for path in [
            root.join("Dir"),
            root.join("./Dir/"),
            root.join("Dir/Dir.cue"),
        ] {
            let data = metadata(&path);
            assert_eq!(data.name, "Directory title");
            assert_eq!(data.releasedate, "19981203T000000");
            assert_eq!(data.desc, "Directory synopsis");
            assert_eq!(cached_metadata(&path).name, "Directory title");
            assert_eq!(
                resolve(&path, Kind::Logo),
                Some(root.join("./art/logo.png"))
            );
            assert_eq!(
                resolve(&path, Kind::Cover),
                Some(root.join("./art/cover.png"))
            );
        }
        for (file, title) in [
            ("Single.nes", "Single title"),
            ("./Prefix.nes", "Prefix title"),
            ("Spaces 100%.nes", "Literal file"),
            ("Spaces 100% literal/Disc.cue", "Literal directory"),
            ("Dir/Sub/Exact.cue", "Exact nested title"),
        ] {
            assert_eq!(metadata(&root.join(file)).name, title);
        }
        assert_eq!(
            resolve(&root.join("Dir/Sub/Exact.cue"), Kind::Logo),
            Some(root.join("./art/logo.png"))
        );
        for file in ["Sibling/Other.cue", "Unknown.nes"] {
            assert!(metadata(&root.join(file)).name.is_empty());
            assert!(resolve(&root.join(file), Kind::Logo).is_none());
        }
        let cue = root.join("Dir/Dir.cue");
        let prepared = prepare_game(&cue);
        invalidate();
        adopt_game(prepared);
        assert_eq!(cached_metadata(&cue).name, "Directory title");
        clear_prepared_paths();
        assert_eq!(resolve(&cue, Kind::Logo), Some(root.join("./art/logo.png")));
        fs::remove_dir_all(root).unwrap();
        invalidate();
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
    parent: PathBuf,
    metadata: SharedMetadata,
    paths: Vec<(Kind, Option<PathBuf>)>,
}
pub fn prepare_game(path: &Path) -> PreparedGame {
    prepare_game_kinds(
        path,
        &[
            Kind::Cover,
            Kind::TallCover,
            Kind::Logo,
            Kind::Background,
            Kind::PreparedCover,
        ],
    )
}
pub fn prepare_game_kinds(path: &Path, kinds: &[Kind]) -> PreparedGame {
    let (parent, metadata, _) = CACHE.with(|c| metadata_with_parent(path, &mut c.borrow_mut()));
    let paths = kinds
        .iter()
        .map(|&kind| (kind, resolve(path, kind)))
        .collect();
    PreparedGame {
        path: path.into(),
        parent,
        metadata,
        paths,
    }
}
pub fn adopt_game(game: PreparedGame) {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let key = normalize(&game.path);
        if !cache.records.contains_key(&key) {
            cache.insert(key, game.parent, (*game.metadata).clone());
        }
        for (kind, path) in game.paths {
            cache.insert_path((game.path.clone(), kind), path);
        }
    });
}
/// Worker-local policy. Call after invalidation as well, which resets the cache.
pub fn set_metadata_budget(bytes: usize) {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let budget = bytes.min(METADATA_BUDGET);
        if budget > cache.budget.unwrap_or(METADATA_BUDGET) {
            cache.suppress_new_sources = false;
        }
        cache.budget = Some(budget);
        while cache.skipped_bytes > marker_budget(budget) {
            cache.suppress_new_sources = true;
            let path = cache.skipped_sources.keys().next().unwrap().clone();
            cache.forget_skipped_source(&path);
        }
        while cache.bytes - cache.skipped_bytes > resident_budget(budget) {
            if cache.evict_record() {
                continue;
            }
            if cache.evict_source().is_none() {
                break;
            }
        }
    });
}
pub fn clear_prepared_paths() {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.paths.clear();
        paths_changed();
        cache.path_order.clear();
        cache.local_containers.clear();
    });
}

pub fn adopt_fallback_paths(game: &Path, paths: impl IntoIterator<Item = (Kind, PathBuf)>) {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        for (kind, path) in paths {
            let key = (game.into(), kind);
            if cache.paths.get(&key).is_none_or(Option::is_none) {
                cache.insert_path(key, Some(path));
            }
        }
    });
}
