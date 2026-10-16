//! Opt-in UI-thread catalog/controller; payload I/O runs on one cancellable worker.
use crate::{
    entry::{Entry, directory::Directory, game::Game},
    network_cache::{Cache, CacheConfig, FilesystemSpace, Progress},
    network_library::{Catalog, NetworkGame},
    network_transfer::{Cancellation, Endpoint},
    view::{
        disc_picker::DiscPicker,
        orbit::{font, wrapped},
    },
};
use anyhow::{Result, ensure};
use common::{
    display::{Display, color::Color, fill_rect, fill_rounded_rect},
    geom::{Point, Rect},
    platform::{DefaultPlatform, Key, KeyEvent, Platform},
    stylesheet::Stylesheet,
};
use serde::Deserialize;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
};
thread_local! {
    static COMPLETION_PENDING: Cell<bool> = const { Cell::new(false) };
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
    static STARTUP: RefCell<Startup> = const { RefCell::new(Startup::Legacy) };
}
enum Startup {
    Legacy,
    Ready,
    Blocked {
        root: PathBuf,
        modal: bool,
        dirty: bool,
    },
}
fn blocked_scope(path: &Path) -> bool {
    STARTUP.with(|cell| match &*cell.borrow() {
        Startup::Blocked { root, .. } => {
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            let scope = root.join("Roms/PS").to_string_lossy().to_lowercase();
            let path = path.to_string_lossy().to_lowercase();
            path == scope || path.starts_with(&(scope + "/"))
        }
        _ => false,
    })
}
fn show_startup_block() {
    STARTUP.with(|cell| {
        if let Startup::Blocked { modal, dirty, .. } = &mut *cell.borrow_mut() {
            *modal = true;
            *dirty = true;
        }
    });
}
fn startup_event(event: KeyEvent) -> bool {
    STARTUP.with(|cell| {
        let mut state = cell.borrow_mut();
        let Startup::Blocked { modal, dirty, .. } = &mut *state else {
            return false;
        };
        if !*modal {
            return false;
        }
        if matches!(event, KeyEvent::Pressed(Key::B)) {
            *modal = false;
            *dirty = true;
        }
        true
    })
}
fn startup_dirty() -> bool {
    STARTUP.with(|cell| {
        if let Startup::Blocked { dirty, .. } = &mut *cell.borrow_mut() {
            std::mem::take(dirty)
        } else {
            false
        }
    })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceConfig {
    version: u32,
    library_id: String,
    address: SocketAddr,
}
#[derive(Clone)]
struct Pending {
    epoch: u64,
    game: Game,
    reset: bool,
    disc: Option<(crate::discs::Playlist, usize)>,
}
pub enum Action {
    Launch(Game, bool),
    Disc(Game, crate::discs::Playlist, usize),
}
enum Task {
    Play(NetworkGame),
    Prefill(NetworkGame),
    Adopt(NetworkGame),
    Unload(NetworkGame),
    Storage,
}
enum Output {
    Ready,
    Prefilled(String),
    Unloaded(String),
    Storage(Vec<String>),
}
struct Job {
    epoch: u64,
    prefill_id: Option<String>,
    cancel: Cancellation,
    progress: Receiver<Progress>,
    result: Receiver<Result<Output>>,
    thread: Option<thread::JoinHandle<()>>,
}
enum Modal {
    Working(String),
    Completed,
    Error(String),
    Storage { ids: Vec<String>, selected: usize },
    ConfirmUnload(String),
    ConfirmImport(String),
    Discs { picker: DiscPicker, game: Game },
}
struct Runtime {
    epoch: u64,
    catalog: Catalog,
    path_index: Option<HashMap<PathBuf, usize>>,
    config: CacheConfig,
    endpoint: Endpoint,
    status: HashMap<String, String>,
    job: Option<Job>,
    modal: Option<Modal>,
    pending: Option<Pending>,
    permit: Option<PathBuf>,
    actions: VecDeque<Action>,
    dirty: bool,
    last_played: HashMap<String, i64>,
    progress_fraction: Option<f64>,
    storage_recent: bool,
}
fn space_error(message: &str) -> bool {
    let message = message.to_lowercase();
    message.contains("space") || message.contains("reserve")
}
fn with<T>(default: T, f: impl FnOnce(&mut Runtime) -> T) -> T {
    RUNTIME.with(|cell| cell.borrow_mut().as_mut().map_or(default, f))
}
pub fn initialize(root: PathBuf) -> Result<()> {
    COMPLETION_PENDING.with(|pending| pending.set(false));
    RUNTIME.with(|cell| *cell.borrow_mut() = None);
    // Only a confirmed missing profile can select legacy launch behavior.
    STARTUP.with(|cell| {
        *cell.borrow_mut() = Startup::Blocked {
            root: root.clone(),
            modal: true,
            dirty: true,
        }
    });
    let result = initialize_profile(root);
    if result.is_ok() {
        let loaded = RUNTIME.with(|cell| cell.borrow().is_some());
        STARTUP.with(|cell| {
            *cell.borrow_mut() = if loaded {
                Startup::Ready
            } else {
                Startup::Legacy
            }
        });
    }
    result
}
fn initialize_profile(root: PathBuf) -> Result<()> {
    let fs = crate::network_fs::AnchoredFs::new(&root)?;
    let state = root.join(".allium/state/network-library");
    let profile = state.join("config.json");
    if !fs.exists(&profile)? {
        return Ok(());
    }
    let read = |path: &Path| -> Result<Vec<u8>> {
        use std::io::Read;
        let file = fs.open(path, false)?;
        ensure!(
            file.metadata()?.len() <= 1024 * 1024,
            "Network configuration too large"
        );
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "Network configuration too large"
        );
        Ok(bytes)
    };
    let source: SourceConfig = serde_json::from_slice(&read(&profile)?)?;
    ensure!(
        source.version == 1,
        "Unsupported network source configuration"
    );
    let catalog: Catalog = serde_json::from_slice(&read(&state.join("catalog.json"))?)?;
    catalog.validate()?;
    ensure!(
        source.library_id == catalog.library_id,
        "Catalog/source identity mismatch"
    );
    let endpoint = Endpoint::lan(source.address)?;
    #[cfg(feature = "miyoo")]
    let lease = Some(PathBuf::from("/tmp/stay_awake"));
    #[cfg(not(feature = "miyoo"))]
    let lease = None;
    let config = CacheConfig::bind(root, lease)?;
    let cache = Cache::new(config.clone())?;
    let mut status = HashMap::new();
    for game in &catalog.games {
        status.insert(
            game.id.clone(),
            if cache.has_index(game)? {
                "Cached"
            } else if cache.unloading(game)? {
                "Unload interrupted"
            } else {
                "NAS"
            }
            .into(),
        );
    }
    RUNTIME.with(|cell| {
        *cell.borrow_mut() = Some(Runtime {
            epoch: 0,
            path_index: catalog_path_index(&catalog, &config.root),
            catalog,
            config,
            endpoint,
            status,
            job: None,
            modal: None,
            pending: None,
            permit: None,
            actions: VecDeque::new(),
            dirty: true,
            last_played: HashMap::new(),
            progress_fraction: None,
            storage_recent: false,
        })
    });
    Ok(())
}
const PATH_INDEX_LIMIT: usize = 384 * 1024;
fn catalog_path_index(catalog: &Catalog, root: &Path) -> Option<HashMap<PathBuf, usize>> {
    let mut index = HashMap::new();
    let mut bytes = 0;
    for (at, game) in catalog.games.iter().enumerate() {
        for relative in std::iter::once(PathBuf::from(&game.game_path))
            .chain(game.targets().unwrap_or_default())
        {
            let path = root.join(relative);
            if !index.contains_key(&path) {
                bytes += path.as_os_str().len() + 192;
                if bytes > PATH_INDEX_LIMIT {
                    return None;
                }
                index.insert(path, at);
            }
        }
    }
    Some(index)
}
fn descriptor_scan(catalog: &Catalog, root: &Path, path: &Path) -> Option<NetworkGame> {
    catalog
        .games
        .iter()
        .find(|game| {
            root.join(&game.game_path) == path
                || game
                    .targets()
                    .is_ok_and(|targets| targets.iter().any(|target| root.join(target) == path))
        })
        .cloned()
}
impl Runtime {
    fn descriptor(&self, path: &Path) -> Option<NetworkGame> {
        if let Some(index) = &self.path_index {
            return index.get(path).map(|i| self.catalog.games[*i].clone());
        }
        descriptor_scan(&self.catalog, &self.config.root, path)
    }
    fn game(&self, id: &str) -> Option<NetworkGame> {
        self.catalog.games.iter().find(|g| g.id == id).cloned()
    }
    fn virtual_game(&self, descriptor: &NetworkGame) -> Game {
        let mut game = Game::new(self.config.root.join(&descriptor.game_path));
        game.name = descriptor.display_title.clone();
        game.full_name = descriptor.display_title.clone();
        let stock = self.catalog_art(descriptor, "stock");
        if stock.exists() {
            game.image = crate::entry::lazy_image::LazyImage::Found(stock);
        }
        game
    }
    fn catalog_art(&self, game: &NetworkGame, kind: &str) -> PathBuf {
        let cache = Path::new(&game.cache_path);
        let basename = if game.install_kind == "directory"
            && !cache
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("m3u"))
        {
            cache.file_name()
        } else {
            cache.file_stem()
        }
        .unwrap_or_default();
        let mut name = basename.to_os_string();
        name.push(match kind {
            "heroes" => ".hero.png",
            "covers" => ".orbit.png",
            _ => ".png",
        });
        self.config.root.join("Roms/PS/Imgs").join(name)
    }
    fn start(&mut self, task: Task) -> Result<()> {
        ensure!(self.job.is_none(), "Network operation already active");
        let prefill_id = match &task {
            Task::Prefill(game) => Some(game.id.clone()),
            _ => None,
        };
        let config = self.config.clone();
        let endpoint = self.endpoint.clone();
        let catalog = self.catalog.clone();
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let (tx, progress) = mpsc::sync_channel(16);
        let (result_tx, result) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("network-library-ui".into())
            .spawn(move || {
                let result = (|| {
                    let root = config.root.clone();
                    let cache = Cache::new(config)?;
                    match task {
                        Task::Play(game) => {
                            cache.run(&game, &endpoint, &worker_cancel, &FilesystemSpace, &tx)?;
                            Ok(Output::Ready)
                        }
                        Task::Prefill(game) => {
                            cache.run(&game, &endpoint, &worker_cancel, &FilesystemSpace, &tx)?;
                            Ok(Output::Prefilled(game.id))
                        }
                        Task::Adopt(game) => {
                            let cancel = crate::network_cache::verification_reporting(
                                &worker_cancel,
                                &game,
                                &tx,
                            );
                            cache.adopt_existing(&game, &cancel)?;
                            Ok(Output::Ready)
                        }
                        Task::Unload(game) => {
                            let cancel = crate::network_cache::verification_reporting(
                                &worker_cancel,
                                &game,
                                &tx,
                            );
                            cache.unload_registered(&game, &cancel)?;
                            Ok(Output::Unloaded(game.id))
                        }
                        Task::Storage => {
                            let resume = cache.resume_path()?;
                            let mut ids = Vec::new();
                            for game in catalog.games {
                                worker_cancel.check()?;
                                if (cache.has_index(&game)? || cache.unloading(&game)?)
                                    && !resume.as_ref().is_some_and(|path| {
                                        let path = path.to_string_lossy().to_lowercase();
                                        root.join(&game.game_path).to_string_lossy().to_lowercase()
                                            == path
                                            || game.targets().is_ok_and(|targets| {
                                                targets.iter().any(|target| {
                                                    root.join(target)
                                                        .to_string_lossy()
                                                        .to_lowercase()
                                                        == path
                                                })
                                            })
                                    })
                                {
                                    ids.push(game.id);
                                }
                            }
                            Ok(Output::Storage(ids))
                        }
                    }
                })();
                let outcome = if result.is_ok() { "ok" } else { "error" };
                if result_tx.send(result).is_ok() {
                    common::perf_trace::network_event("worker_result_sent", outcome, None);
                }
            })?;
        self.job = Some(Job {
            epoch: self.epoch,
            prefill_id,
            cancel,
            progress,
            result,
            thread: Some(thread),
        });
        self.progress_fraction = None;
        self.modal = Some(Modal::Working("Checking game and space…".into()));
        self.dirty = true;
        Ok(())
    }
    fn begin(&mut self, mut game: Game, reset: bool) -> Result<bool> {
        let Some(descriptor) = self.descriptor(&game.path) else {
            return Ok(false);
        };
        if self.job.is_some() {
            return Ok(true);
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.permit = None;
        if game.path != self.config.root.join(&descriptor.game_path) {
            game = self.virtual_game(&descriptor);
        }
        self.pending = Some(Pending {
            epoch: self.epoch,
            game,
            reset,
            disc: None,
        });
        let cache = Cache::new(self.config.clone())?;
        if cache.unloading(&descriptor)? {
            self.modal = Some(Modal::ConfirmUnload(descriptor.id));
            self.dirty = true;
        } else if !cache.has_index(&descriptor)? && cache.target_exists(&descriptor)? {
            self.modal = Some(Modal::ConfirmImport(descriptor.id));
            self.dirty = true;
        } else {
            self.start(Task::Play(descriptor))?;
        }
        Ok(true)
    }
    fn prefill(&mut self, id: &str) -> Result<()> {
        ensure!(self.job.is_none(), "Network operation already active");
        let game = self
            .game(id)
            .ok_or_else(|| anyhow::anyhow!("Unknown network catalog ID"))?;
        self.epoch = self.epoch.wrapping_add(1);
        self.pending = None;
        self.permit = None;
        self.actions.clear();
        self.modal = None;
        self.start(Task::Prefill(game))
    }
    fn poll(&mut self) {
        let Some(job) = self.job.as_mut() else {
            return;
        };
        while let Ok(progress) = job.progress.try_recv() {
            if job.cancel.check().is_err() {
                continue;
            }
            if progress.phase == "verifying" {
                self.progress_fraction = (progress.bytes_total > 0)
                    .then_some(progress.bytes_done as f64 / progress.bytes_total.max(1) as f64);
                self.modal = Some(Modal::Working("Verifying".into()));
                self.status.insert(progress.game_id, "Verifying".into());
                self.dirty = true;
                continue;
            }
            self.progress_fraction =
                Some(progress.bytes_done as f64 / progress.bytes_total.max(1) as f64);
            let (unit, divisor) = if progress.bytes_total < 1048576 {
                ("KiB", 1024)
            } else {
                ("MiB", 1048576)
            };
            self.modal = Some(Modal::Working(format!(
                "Downloading {} / {} {unit}",
                progress.bytes_done / divisor,
                progress.bytes_total / divisor
            )));
            self.status.insert(progress.game_id, "Downloading".into());
            self.dirty = true;
        }
        let result = match job.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!(
                "Network worker stopped; files remain protected"
            )),
        };
        common::perf_trace::network_event(
            "poll_result_received",
            if result.is_ok() { "ok" } else { "error" },
            Some(true),
        );
        COMPLETION_PENDING.with(|pending| pending.set(true));
        let mut job = self.job.take().unwrap();
        if let Some(thread) = job.thread.take() {
            let _ = thread.join();
        }
        self.dirty = true;
        if job.cancel.check().is_err() || job.epoch != self.epoch {
            if let Some(id) = job.prefill_id {
                self.status.insert(id, "Partial".into());
            }
            self.pending = None;
            self.permit = None;
            self.modal = None;
            return;
        }
        match result {
            Ok(Output::Ready) => {
                if let Some(pending) = self
                    .pending
                    .take()
                    .filter(|pending| pending.epoch == job.epoch)
                {
                    if let Some(game) = self.descriptor(&pending.game.path) {
                        self.status.insert(game.id, "Cached".into());
                    }
                    self.permit = Some(pending.game.path.clone());
                    self.modal = None;
                    self.actions
                        .push_back(if let Some((playlist, index)) = pending.disc {
                            Action::Disc(pending.game, playlist, index)
                        } else {
                            Action::Launch(pending.game, pending.reset)
                        });
                } else {
                    self.modal = None;
                }
            }
            Ok(Output::Prefilled(id)) => {
                self.status.insert(id, "Cached".into());
                self.pending = None;
                self.permit = None;
                self.modal = Some(Modal::Completed);
                common::perf_trace::network_event("modal_changed", "Completed", Some(false));
            }
            Ok(Output::Unloaded(id)) => {
                self.status.insert(id, "NAS".into());
                self.modal = None;
            }
            Ok(Output::Storage(mut ids)) => {
                self.sort_storage(&mut ids);
                self.modal = Some(Modal::Storage { ids, selected: 0 });
            }
            Err(error) => {
                let message = error.to_string();
                if let Some(id) = job.prefill_id {
                    self.status.insert(id, "Unavailable".into());
                }
                if let Some(game) = self
                    .pending
                    .as_ref()
                    .and_then(|pending| self.descriptor(&pending.game.path))
                {
                    self.status.insert(
                        game.id,
                        if message.contains("Cancelled") {
                            "Partial"
                        } else {
                            "Unavailable"
                        }
                        .into(),
                    );
                }
                if message.contains("Cancelled") {
                    self.pending = None;
                    self.modal = None;
                } else {
                    self.modal = Some(Modal::Error(message));
                    common::perf_trace::network_event("modal_changed", "Failed", Some(false));
                }
                self.permit = None;
            }
        }
    }
    fn sort_storage(&self, ids: &mut [String]) {
        ids.sort_by(|a, b| {
            let bytes = |id: &str| {
                self.game(id)
                    .map_or(0, |g| g.files.iter().map(|p| p.bytes).sum::<u64>())
            };
            let played = |id: &str| self.last_played.get(id).copied().unwrap_or(0);
            if self.storage_recent {
                played(a)
                    .cmp(&played(b))
                    .then(bytes(b).cmp(&bytes(a)))
                    .then(a.cmp(b))
            } else {
                bytes(b)
                    .cmp(&bytes(a))
                    .then(played(a).cmp(&played(b)))
                    .then(a.cmp(b))
            }
        });
    }

    fn event(&mut self, event: KeyEvent) -> Result<bool> {
        if self.modal.is_none() {
            return Ok(false);
        }
        let KeyEvent::Pressed(key) = event else {
            return Ok(true);
        };
        let modal = self.modal.take().unwrap();
        match modal {
            Modal::Working(message) => self.handle_working(key, message)?,
            Modal::Completed if key != Key::B => self.modal = Some(Modal::Completed),
            Modal::Completed => {}
            Modal::Error(message) => self.handle_error(key, message)?,
            Modal::Storage { ids, selected } => self.handle_storage(key, ids, selected)?,
            Modal::ConfirmUnload(id) => self.handle_unload_confirmation(key, id)?,
            Modal::ConfirmImport(id) => self.handle_import_confirmation(key, id)?,
            Modal::Discs { picker, game } => self.handle_discs(key, picker, game)?,
        }
        self.dirty = true;
        Ok(true)
    }
    fn handle_working(&mut self, key: Key, message: String) -> Result<()> {
        if key == Key::B {
            self.epoch = self.epoch.wrapping_add(1);
            if let Some(job) = &self.job {
                job.cancel.cancel();
            }
            if let Some(game) = self
                .pending
                .as_ref()
                .and_then(|pending| self.descriptor(&pending.game.path))
            {
                self.status.insert(game.id, "Partial".into());
            }
            self.pending = None;
            self.permit = None;
            self.modal = Some(Modal::Working("Cancelling…".into()));
        } else {
            self.modal = Some(Modal::Working(message));
        }

        Ok(())
    }
    fn handle_error(&mut self, key: Key, message: String) -> Result<()> {
        if key == Key::A && (space_error(&message)) {
            self.start(Task::Storage)?;
        } else if key == Key::B {
            self.pending = None;
        } else {
            self.modal = Some(Modal::Error(message));
        }

        Ok(())
    }
    fn handle_storage(
        &mut self,
        key: Key,
        mut ids: Vec<String>,
        mut selected: usize,
    ) -> Result<()> {
        match key {
            Key::X => {
                self.storage_recent = !self.storage_recent;
                self.sort_storage(&mut ids);
                selected = 0;
            }
            Key::Up => selected = selected.saturating_sub(1),
            Key::Down => selected = (selected + 1).min(ids.len().saturating_sub(1)),
            Key::A if !ids.is_empty() => {
                self.modal = Some(Modal::ConfirmUnload(ids[selected].clone()));
            }
            Key::B => {
                self.pending = None;
            }
            _ => {}
        }
        if self.modal.is_none() && key != Key::B {
            self.modal = Some(Modal::Storage { ids, selected });
        }

        Ok(())
    }
    fn handle_unload_confirmation(&mut self, key: Key, id: String) -> Result<()> {
        if key == Key::A {
            self.pending = None;
            self.start(Task::Unload(self.game(&id).unwrap()))?;
        } else if key != Key::B {
            self.modal = Some(Modal::ConfirmUnload(id));
        }

        Ok(())
    }
    fn handle_import_confirmation(&mut self, key: Key, id: String) -> Result<()> {
        if key == Key::A {
            self.start(Task::Adopt(self.game(&id).unwrap()))?;
        } else if key == Key::B {
            self.pending = None;
        } else {
            self.modal = Some(Modal::ConfirmImport(id));
        }

        Ok(())
    }
    fn handle_discs(&mut self, key: Key, mut picker: DiscPicker, game: Game) -> Result<()> {
        match key {
            Key::Up => picker.select(false),
            Key::Down => picker.select(true),
            Key::A => {
                self.epoch = self.epoch.wrapping_add(1);
                self.pending = Some(Pending {
                    epoch: self.epoch,
                    game: game.clone(),
                    reset: false,
                    disc: Some((picker.playlist.clone(), picker.selected)),
                });
                self.start(Task::Play(self.descriptor(&game.path).unwrap()))?;
            }
            Key::B => {
                self.pending = None;
                self.permit = None;
            }
            _ => {}
        }
        if self.modal.is_none() && key != Key::A && key != Key::B {
            self.modal = Some(Modal::Discs { picker, game });
        }

        Ok(())
    }
}
pub fn begin(game: &Game, reset: bool) -> Result<bool> {
    if blocked_scope(&game.path) {
        show_startup_block();
        return Ok(true);
    }
    with(Ok(false), |runtime| {
        match runtime.begin(game.clone(), reset) {
            Ok(value) => Ok(value),
            Err(error) => {
                runtime.permit = None;
                runtime.modal = Some(Modal::Error(error.to_string()));
                runtime.dirty = true;
                Ok(true)
            }
        }
    })
}
pub fn download_id(path: &Path) -> Option<String> {
    with(None, |runtime| {
        runtime.descriptor(path).and_then(|game| {
            (runtime.status.get(&game.id).map(String::as_str) != Some("Cached")).then_some(game.id)
        })
    })
}
pub fn download_offer_id(path: &Path) -> Option<String> {
    with(None, |runtime| {
        let game = runtime.descriptor(path)?;
        let config = CacheConfig::bind(runtime.config.root.clone(), None).ok()?;
        if config.expected_root != runtime.config.expected_root {
            return None;
        }
        let cache = Cache::new(config).ok()?;
        (cache.install_state(&game).ok()? == crate::network_cache::InstallState::Missing)
            .then_some(game.id)
    })
}
pub fn prefill(id: &str) -> Result<()> {
    with(
        Err(anyhow::anyhow!("Network catalog is unavailable")),
        |runtime| runtime.prefill(id),
    )
}
pub fn guard_launch(path: &Path) -> Result<()> {
    ensure!(
        !blocked_scope(path),
        "Network library startup failed; PlayStation launch is blocked until configuration is repaired"
    );
    with(Ok(()), |runtime| {
        if runtime.descriptor(path).is_some() {
            ensure!(
                runtime.permit.as_deref() == Some(path),
                "Network game must be fully verified through Play before launch"
            );
        }
        Ok(())
    })
}
pub fn badge(path: &Path) -> Option<String> {
    with(None, |runtime| {
        runtime
            .descriptor(path)
            .and_then(|g| runtime.status.get(&g.id).cloned())
    })
}
pub fn art_path(path: &Path, kind: &str) -> Option<PathBuf> {
    with(None, |r| {
        r.descriptor(path).map(|game| r.catalog_art(&game, kind))
    })
}
pub fn virtual_directory(path: &Path) -> bool {
    with(false, |r| path == r.config.root.join("Roms/PS"))
}
pub fn registered(path: &Path) -> bool {
    with(false, |r| r.descriptor(path).is_some())
}
pub fn refresh_history(database: &common::database::Database) -> Result<()> {
    let rows = database.select_all_games()?;
    with((), |r| {
        for row in rows {
            if let Some(game) = r.descriptor(&row.path) {
                r.last_played.insert(game.id, row.last_played);
            }
        }
    });
    Ok(())
}
pub fn finish_action() {
    with((), |r| r.permit = None);
}
pub fn failure(message: String) {
    with((), |r| {
        r.permit = None;
        r.pending = None;
        r.modal = Some(Modal::Error(message));
        r.dirty = true;
    });
}

pub(crate) fn blocks_navigation_warm() -> bool {
    STARTUP.with(|cell| matches!(&*cell.borrow(), Startup::Blocked { modal: true, .. }))
        || COMPLETION_PENDING.with(Cell::get)
        || with(false, |r| {
            r.job.is_some()
                || r.modal.is_some()
                || r.pending.is_some()
                || r.permit.is_some()
                || !r.actions.is_empty()
        })
}

pub fn active() -> bool {
    with(false, |r| r.job.is_some())
}
pub fn trace_wake(cause: &'static str) {
    common::perf_trace::wake();
    if common::perf_trace::enabled() {
        common::perf_trace::network_event("wake", cause, Some(active()));
    }
}
pub fn trace_loop(event: &'static str, cause: &'static str) {
    if !common::perf_trace::enabled() {
        return;
    }
    with((), |r| {
        if r.job.is_some() || r.modal.is_some() {
            common::perf_trace::network_event(event, cause, Some(r.job.is_some()));
        }
    });
}
pub fn frame_presented() {
    #[cfg(test)]
    test_loop_event("flush");
    trace_loop("frame_presented", "flush_returned");
    COMPLETION_PENDING.with(|pending| {
        if pending.replace(false) {
            common::perf_trace::network_event(
                "completion_presented",
                "flush_returned",
                Some(false),
            );
        }
    });
}
pub fn poll() -> bool {
    let blocked_dirty = startup_dirty();
    with(false, |r| {
        r.poll();
        let dirty = r.dirty;
        r.dirty = false;
        if dirty {
            common::perf_trace::network_event("dirty_returned", "poll", Some(r.job.is_some()));
        }
        dirty
    }) || blocked_dirty
}
pub fn event(event: KeyEvent) -> Result<bool> {
    if startup_event(event) {
        return Ok(true);
    }
    with(Ok(false), |r| r.event(event))
}
pub fn action() -> Option<Action> {
    with(None, |r| r.actions.pop_front())
}
pub fn open_discs(game: Game, playlist: crate::discs::Playlist) {
    with((), |r| {
        r.permit = None;
        r.modal = Some(Modal::Discs {
            picker: DiscPicker::new(playlist),
            game,
        });
        r.dirty = true;
    });
}
pub fn storage() -> Result<()> {
    with(Ok(()), |r| {
        r.pending = None;
        r.start(Task::Storage)
    })
}
pub fn merge_directory(directory: &Path, entries: &mut Vec<Entry>) {
    with((), |r| {
        if directory == r.config.root.join("Roms")
            && !entries
                .iter()
                .any(|e| e.path() == r.config.root.join("Roms/PS"))
        {
            entries.push(Entry::Directory(Directory::with_name(
                r.config.root.join("Roms/PS"),
                "PlayStation".into(),
            )));
        }
        for descriptor in &r.catalog.games {
            let identity = r.config.root.join(&descriptor.game_path);
            let cache_path = r.config.root.join(&descriptor.cache_path);
            entries.retain(|entry| entry.path() != cache_path || entry.path() == identity);
            if let Some(Entry::Game(game)) =
                entries.iter_mut().find(|entry| entry.path() == identity)
            {
                game.name = descriptor.display_title.clone();
                game.full_name = descriptor.display_title.clone();
            }
            let container = if descriptor.install_kind == "directory" {
                Path::new(&descriptor.cache_path).parent()
            } else {
                Path::new(&descriptor.game_path).parent()
            };
            if container.is_some_and(|p| r.config.root.join(p) == directory)
                && !entries
                    .iter()
                    .any(|e| e.path() == r.config.root.join(&descriptor.game_path))
            {
                entries.push(Entry::Game(r.virtual_game(descriptor)));
            }
        }
    });
}
pub fn search(query: &str, entries: &mut Vec<Entry>) {
    with((), |r| {
        for game in &r.catalog.games {
            if game
                .display_title
                .to_lowercase()
                .contains(&query.to_lowercase())
                && !entries
                    .iter()
                    .any(|e| e.path() == r.config.root.join(&game.game_path))
            {
                entries.push(Entry::Game(r.virtual_game(game)));
            }
        }
    });
}
pub fn draw(display: &mut <DefaultPlatform as Platform>::Display, styles: &Stylesheet) -> bool {
    if draw_startup_block(display, styles) {
        return true;
    }
    with(false, |r| {
        let titles = r
            .catalog
            .games
            .iter()
            .map(|g| {
                (
                    g.id.clone(),
                    format!(
                        "{}  ({} MiB)",
                        g.display_title,
                        g.files.iter().map(|f| f.bytes).sum::<u64>() / 1048576
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        let Some(modal) = r.modal.as_mut() else {
            return false;
        };
        if let Modal::Discs { picker, .. } = modal {
            picker.draw(display, styles);
            footer(display, styles, "A Start disc    B Cancel");
            return true;
        }
        fill_rect(
            &mut display.pixmap_mut(),
            Rect::new(
                0,
                crate::view::orbit::HEADER_HEIGHT,
                640,
                crate::view::orbit::CONTENT_HEIGHT,
            ),
            Color::rgba(22, 37, 30, 145),
        );
        fill_rounded_rect(
            &mut display.pixmap_mut(),
            Rect::new(90, 74, 460, 340),
            14,
            Color::new(243, 244, 238),
        );
        let title = font(styles, 28, Color::new(35, 48, 51));
        let body = font(styles, 17, Color::new(35, 48, 51));
        let (header, message, hints) = match modal {
            Modal::Working(m) => ("Network library", m.clone(), "B Cancel"),
            Modal::Completed => (
                "Download complete",
                "Ready in the local cache".into(),
                "B Back",
            ),
            Modal::Error(m) => (
                "Unable to continue",
                m.clone(),
                if space_error(m) {
                    "A Manage storage    B Back"
                } else {
                    "B Back"
                },
            ),
            Modal::ConfirmImport(id) => (
                "Verify existing game",
                format!(
                    "Verify all files before registering {} for cache management?",
                    titles.get(id).unwrap_or(id)
                ),
                "A Verify and register    B Cancel",
            ),
            Modal::ConfirmUnload(id) => (
                "Unload game",
                format!(
                    "Remove only registered ROM files for {}? Art, saves and history remain.",
                    titles.get(id).unwrap_or(id)
                ),
                "A Unload    B Cancel",
            ),
            Modal::Storage { ids, selected } => {
                let m = if ids.is_empty() {
                    "No eligible registered games. The current resume game is protected.".into()
                } else {
                    ids.iter()
                        .enumerate()
                        .skip(selected.saturating_sub(2))
                        .take(7)
                        .map(|(i, id)| {
                            format!(
                                "{} {}",
                                if i == *selected { ">" } else { " " },
                                titles.get(id).unwrap_or(id)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                (
                    if r.storage_recent {
                        "Least recently played"
                    } else {
                        "Largest games"
                    },
                    m,
                    "Up/Down Select  A Unload  X Sort  B Back",
                )
            }
            Modal::Discs { .. } => unreachable!(),
        };
        title.draw(&mut display.pixmap_mut(), header, Point::new(108, 93));
        let mut y = 142;
        for line in message.lines() {
            for line in wrapped(&body, line, 424, 10) {
                if y > 365 {
                    break;
                }
                body.draw(&mut display.pixmap_mut(), &line, Point::new(108, y));
                y += 22;
            }
        }
        if matches!(modal, Modal::Working(_)) {
            fill_rounded_rect(
                &mut display.pixmap_mut(),
                Rect::new(108, 382, 424, 12),
                5,
                Color::new(216, 223, 216),
            );
            if let Some(fraction) = r.progress_fraction {
                fill_rounded_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(108, 382, (424.0 * fraction.clamp(0.0, 1.0)) as u32, 12),
                    5,
                    Color::new(27, 189, 200),
                );
            }
        }
        footer(display, styles, hints);
        true
    })
}
fn draw_startup_block(
    display: &mut <DefaultPlatform as Platform>::Display,
    styles: &Stylesheet,
) -> bool {
    let visible =
        STARTUP.with(|cell| matches!(&*cell.borrow(), Startup::Blocked { modal: true, .. }));
    if !visible {
        return false;
    }
    fill_rect(
        &mut display.pixmap_mut(),
        Rect::new(
            0,
            crate::view::orbit::HEADER_HEIGHT,
            640,
            crate::view::orbit::CONTENT_HEIGHT,
        ),
        Color::rgba(22, 37, 30, 145),
    );
    fill_rounded_rect(
        &mut display.pixmap_mut(),
        Rect::new(90, 74, 460, 340),
        14,
        Color::new(243, 244, 238),
    );
    font(styles, 28, Color::new(35, 48, 51)).draw(
        &mut display.pixmap_mut(),
        "Library unavailable",
        Point::new(108, 93),
    );
    let body = font(styles, 17, Color::new(35, 48, 51));
    let message = "PlayStation launch is paused until the network library configuration is repaired. Your files remain untouched. Press B to return to the library, Apps or Settings.";
    for (index, line) in wrapped(&body, message, 424, 8).iter().enumerate() {
        body.draw(
            &mut display.pixmap_mut(),
            line,
            Point::new(108, 142 + index as i32 * 22),
        );
    }
    footer(display, styles, "B Back");
    true
}
pub fn footer(
    display: &mut <DefaultPlatform as Platform>::Display,
    styles: &Stylesheet,
    text: &str,
) {
    fill_rect(
        &mut display.pixmap_mut(),
        Rect::new(0, crate::view::orbit::FOOTER_Y, 640, 50),
        Color::new(243, 244, 238),
    );
    font(styles, 16, Color::new(35, 48, 51)).draw(
        &mut display.pixmap_mut(),
        text,
        Point::new(18, 447),
    );
}

#[cfg(test)]
mod tests {
    include!("network_runtime_dl11_tests.rs");
    include!("network_runtime_dl12_bench.rs");
    include!("network_runtime_dl17b_tests.rs");
    use super::*;
    use crate::network_library::Payload;
    use sha2::{Digest, Sha256};
    fn settle() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while active() {
            assert!(std::time::Instant::now() < deadline);
            poll();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn network_orbit_multidisc_rechecks_full_group_before_disc_continuation() {
        run_multidisc_fixture(false);
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn ux4_download_offer_excludes_unregistered_local_targets() {
        run_multidisc_fixture(true);
    }
    fn run_multidisc_fixture(check_download_offer: bool) {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("allium-network-ui-multi-{}", std::process::id()));
        assert!(!root.exists());
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                RUNTIME.with(|cell| *cell.borrow_mut() = None);
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
        std::fs::create_dir_all(root.join("Roms/PS/Network Multi.m3u")).unwrap();
        std::fs::create_dir_all(root.join(".allium/state/network-library")).unwrap();
        let _cleanup = Cleanup(root.clone());
        let bodies: [&[u8]; 3] = [
            b"fake-disc-one",
            b"fake-disc-two",
            b"disc1.chd\ndisc2.chd\n",
        ];
        let names = ["disc1.chd", "disc2.chd", "Network Multi.m3u"];
        let revision = "b".repeat(64);
        let descriptor = NetworkGame {
            id: "network-multi".into(),
            display_title: "Network Multi".into(),
            platform: "PS".into(),
            bundle_revision: revision.clone(),
            game_path: "Roms/PS/Network Multi.m3u".into(),
            cache_path: "Roms/PS/Network Multi.m3u".into(),
            launch_path: "Roms/PS/Network Multi.m3u/Network Multi.m3u".into(),
            install_kind: "directory".into(),
            files: bodies
                .iter()
                .enumerate()
                .map(|(i, body)| Payload {
                    id: format!("file{i}"),
                    relative_path: names[i].into(),
                    bytes: body.len() as u64,
                    sha256: crate::network_transfer::digest_hex(Sha256::digest(body).as_slice()),
                    role: if i == 2 { "playlist" } else { "rom" }.into(),
                    http_route: format!("/v1/files/network-multi/{revision}/file{i}"),
                })
                .collect(),
            discs: vec!["file0".into(), "file1".into()],
        };
        for (name, body) in names.iter().zip(bodies) {
            std::fs::write(root.join(&descriptor.cache_path).join(name), body).unwrap();
        }
        let catalog = Catalog {
            version: 1,
            library_id: "proof".into(),
            revision: "a".repeat(64),
            games: vec![descriptor.clone()],
        };
        catalog.validate().unwrap();
        let state = root.join(".allium/state/network-library");
        std::fs::write(
            state.join("config.json"),
            r#"{"version":1,"library_id":"proof","address":"127.0.0.1:9"}"#,
        )
        .unwrap();
        std::fs::write(
            state.join("catalog.json"),
            serde_json::to_vec(&catalog).unwrap(),
        )
        .unwrap();
        initialize(root.clone()).unwrap();
        let game = Game::new(root.join(&descriptor.game_path));
        if check_download_offer {
            assert!(
                !crate::view::ux4_offers_download(game.clone()),
                "local unregistered target offered Download"
            );
            std::fs::remove_dir_all(root.join(&descriptor.cache_path)).unwrap();
            assert!(crate::view::ux4_offers_download(game.clone()));
            std::fs::write(
                root.join(&descriptor.cache_path),
                b"unregistered local file",
            )
            .unwrap();
            assert!(!crate::view::ux4_offers_download(game.clone()));
            return;
        }
        assert!(begin(&game, false).unwrap());
        // Explicit registration is required, even for an exact preexisting bundle.
        assert!(!active());
        assert!(action().is_none());
        event(KeyEvent::Pressed(Key::B)).unwrap();
        assert!(
            !Cache::new(CacheConfig::bind(root.clone(), None).unwrap())
                .unwrap()
                .has_index(&descriptor)
                .unwrap()
        );
        begin(&game, false).unwrap();
        event(KeyEvent::Pressed(Key::A)).unwrap();
        settle();
        let Some(Action::Launch(ready, false)) = action() else {
            panic!("complete adopted group required")
        };
        let playlist = crate::discs::discover(&ready.path).unwrap().unwrap();
        assert_eq!(playlist.discs.len(), 2);
        open_discs(ready, playlist.clone());
        for key in [Key::X, Key::L, Key::R, Key::Select, Key::Start] {
            assert!(event(KeyEvent::Pressed(key)).unwrap());
            assert!(action().is_none());
        }
        event(KeyEvent::Pressed(Key::B)).unwrap();
        assert!(action().is_none());
        assert!(guard_launch(&game.path).is_err());
        begin(&game, false).unwrap();
        settle();
        let Some(Action::Launch(ready, false)) = action() else {
            panic!()
        };
        open_discs(ready, playlist.clone());
        std::fs::remove_file(root.join(&descriptor.cache_path).join("disc2.chd")).unwrap();
        event(KeyEvent::Pressed(Key::A)).unwrap();
        settle();
        assert!(action().is_none());
        assert!(guard_launch(&game.path).is_err());
        event(KeyEvent::Pressed(Key::B)).unwrap();
        std::fs::write(
            root.join(&descriptor.cache_path).join("disc2.chd"),
            bodies[1],
        )
        .unwrap();
        begin(&game, false).unwrap();
        settle();
        let Some(Action::Launch(ready, false)) = action() else {
            panic!()
        };
        open_discs(ready, playlist);
        event(KeyEvent::Pressed(Key::Down)).unwrap();
        event(KeyEvent::Pressed(Key::A)).unwrap();
        settle();
        let Some(Action::Disc(ready, playlist, index)) = action() else {
            panic!("selected disc continuation only after every file reverified")
        };
        assert_eq!(index, 1);
        assert_eq!(ready.path, game.path);
        assert_eq!(playlist.discs.len(), 2);
        assert!(guard_launch(&root.join(&descriptor.cache_path).join("disc1.chd")).is_err());
        assert!(guard_launch(&game.path).is_ok());
        finish_action();
        assert!(guard_launch(&game.path).is_err());
        begin(&game, false).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while with(false, |r| {
            !r.job
                .as_ref()
                .unwrap()
                .thread
                .as_ref()
                .unwrap()
                .is_finished()
        }) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        event(KeyEvent::Pressed(Key::B)).unwrap();
        settle();
        assert!(action().is_none());
        assert!(guard_launch(&game.path).is_err());
    }
    struct StartupFixture {
        root: PathBuf,
        game: NetworkGame,
    }
    impl StartupFixture {
        fn new(label: &str) -> Self {
            let root = std::fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!(
                    "allium-network-startup-{label}-{}",
                    std::process::id()
                ));
            assert!(!root.exists());
            std::fs::create_dir_all(root.join("Roms/PS")).unwrap();
            let body = b"startup fake ROM bytes";
            let revision = "b".repeat(64);
            let game = NetworkGame {
                id: "startup-fixture".into(),
                display_title: "Startup Fixture".into(),
                platform: "PS".into(),
                bundle_revision: revision.clone(),
                game_path: "Roms/PS/Startup Fixture.chd".into(),
                cache_path: "Roms/PS/Startup Fixture.chd".into(),
                launch_path: "Roms/PS/Startup Fixture.chd".into(),
                install_kind: "file".into(),
                files: vec![Payload {
                    id: "disc".into(),
                    relative_path: String::new(),
                    bytes: body.len() as u64,
                    sha256: crate::network_transfer::digest_hex(Sha256::digest(body).as_slice()),
                    role: "rom".into(),
                    http_route: format!("/v1/files/startup-fixture/{revision}/disc"),
                }],
                discs: vec![],
            };
            std::fs::write(root.join(&game.game_path), body).unwrap();
            Self { root, game }
        }
        fn configured(&self) {
            let state = self.root.join(".allium/state/network-library");
            std::fs::create_dir_all(&state).unwrap();
            std::fs::write(
                state.join("config.json"),
                r#"{"version":1,"library_id":"proof","address":"127.0.0.1:9"}"#,
            )
            .unwrap();
            let catalog = Catalog {
                version: 1,
                library_id: "proof".into(),
                revision: "a".repeat(64),
                games: vec![self.game.clone()],
            };
            std::fs::write(
                state.join("catalog.json"),
                serde_json::to_vec(&catalog).unwrap(),
            )
            .unwrap();
            Cache::new(CacheConfig::bind(self.root.clone(), None).unwrap())
                .unwrap()
                .adopt_existing(&self.game, &Cancellation::default())
                .unwrap();
        }
    }
    impl Drop for StartupFixture {
        fn drop(&mut self) {
            RUNTIME.with(|cell| *cell.borrow_mut() = None);
            STARTUP.with(|cell| *cell.borrow_mut() = Startup::Legacy);
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn dl7_stale_ready_index_offers_download() {
        let f = StartupFixture::new("stale-offer");
        f.configured();
        std::fs::remove_file(f.root.join(&f.game.cache_path)).unwrap();
        initialize(f.root.clone()).unwrap();
        assert_eq!(
            badge(&f.root.join(&f.game.game_path)).as_deref(),
            Some("Cached")
        );
        assert_eq!(
            download_offer_id(&f.root.join(&f.game.game_path)),
            Some(f.game.id.clone())
        );
        assert!(crate::view::ux4_offers_download(Game::new(
            f.root.join(&f.game.game_path)
        )));
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn dl7_stale_ready_index_job_publishes_verified_payload() {
        let f = StartupFixture::new("stale-job");
        f.configured();
        let body = std::fs::read(f.root.join(&f.game.cache_path)).unwrap();
        std::fs::remove_file(f.root.join(&f.game.cache_path)).unwrap();
        let state = f.root.join(".allium/state/network-library");
        let job = state
            .join("jobs")
            .join(&f.game.id)
            .join(&f.game.bundle_revision);
        std::fs::create_dir_all(job.join("payloads")).unwrap();
        std::fs::write(job.join("payloads/content.bin"), &body).unwrap();
        std::fs::write(job.join("job.json"), serde_json::to_vec(&serde_json::json!({
            "version": 1, "game": f.game, "phase": "downloading",
            "partials": [{"id": f.game.files[0].id, "bytes": body.len(), "prefix_sha256": f.game.files[0].sha256}]
        })).unwrap()).unwrap();
        let cache = Cache::new(CacheConfig::bind(f.root.clone(), None).unwrap()).unwrap();
        assert!(cache.has_index(&f.game).unwrap());
        let (tx, rx) = mpsc::sync_channel(16);
        cache
            .run(
                &f.game,
                &Endpoint::lan("127.0.0.1:9".parse().unwrap()).unwrap(),
                &Cancellation::default(),
                &FilesystemSpace,
                &tx,
            )
            .unwrap();
        assert_eq!(
            std::fs::read(f.root.join(&f.game.cache_path)).unwrap(),
            body
        );
        assert!(cache.ready(&f.game, &Cancellation::default()).is_ok());
        assert!(rx.try_iter().any(|p| p.phase == "verifying"));
        std::fs::remove_file(f.root.join(&f.game.cache_path)).unwrap();
        struct NoSpace;
        impl crate::network_cache::SpaceProbe for NoSpace {
            fn observe(
                &self,
                _: &crate::network_fs::AnchoredFs,
            ) -> Result<crate::network_cache::Space> {
                Ok(crate::network_cache::Space {
                    free_bytes: 0,
                    allocation_unit: 4096,
                })
            }
        }
        let error = cache
            .run(
                &f.game,
                &Endpoint::lan("127.0.0.1:9".parse().unwrap()).unwrap(),
                &Cancellation::default(),
                &NoSpace,
                &tx,
            )
            .unwrap_err();
        assert!(
            space_error(&error.to_string()),
            "fresh download must reach capacity check: {error}"
        );
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn dl8_missing_directory_member_offers_download() {
        for playlist in [false, true] {
            let mut f = StartupFixture::new(if playlist {
                "m3u-offer"
            } else {
                "member-offer"
            });
            std::fs::remove_file(f.root.join(&f.game.cache_path)).unwrap();
            f.game.install_kind = "directory".into();
            f.game.cache_path = if playlist {
                "Roms/PS/Bundle.m3u"
            } else {
                "Roms/PS/Bundle"
            }
            .into();
            let member = if playlist { "Bundle.m3u" } else { "disc.chd" };
            f.game.launch_path = format!("{}/{member}", f.game.cache_path);
            f.game.game_path = if playlist {
                f.game.cache_path.clone()
            } else {
                f.game.launch_path.clone()
            };
            f.game.files[0].relative_path = member.into();
            f.game.files[0].role = if playlist { "playlist" } else { "rom" }.into();
            std::fs::create_dir_all(f.root.join(&f.game.cache_path)).unwrap();
            std::fs::write(f.root.join(&f.game.launch_path), b"startup fake ROM bytes").unwrap();
            f.configured();
            initialize(f.root.clone()).unwrap();
            assert_eq!(download_offer_id(&f.root.join(&f.game.game_path)), None);
            std::fs::write(f.root.join(&f.game.launch_path), b"wrong length").unwrap();
            assert_eq!(download_offer_id(&f.root.join(&f.game.game_path)), None);
            std::fs::remove_file(f.root.join(&f.game.launch_path)).unwrap();
            assert_eq!(
                download_offer_id(&f.root.join(&f.game.game_path)),
                Some(f.game.id.clone())
            );
        }
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn dl7_verifying_reaches_modal_before_completed() {
        let f = StartupFixture::new("verifying");
        f.configured();
        initialize(f.root.clone()).unwrap();
        let (tx, progress) = mpsc::sync_channel(16);
        let (result_tx, result) = mpsc::sync_channel(1);
        with((), |r| {
            r.job = Some(Job {
                epoch: r.epoch,
                prefill_id: Some(f.game.id.clone()),
                cancel: Cancellation::default(),
                progress,
                result,
                thread: None,
            })
        });
        tx.send(Progress {
            game_id: f.game.id.clone(),
            phase: "downloading",
            bytes_done: 10,
            bytes_total: 10,
        })
        .unwrap();
        assert!(poll());
        tx.send(Progress {
            game_id: f.game.id.clone(),
            phase: "verifying",
            bytes_done: 10,
            bytes_total: 10,
        })
        .unwrap();
        assert!(poll());
        with((), |r| {
            assert!(matches!(&r.modal, Some(Modal::Working(text)) if text == "Verifying"));
            assert_eq!(r.progress_fraction, Some(1.0));
        });
        assert!(active());
        result_tx
            .send(Ok(Output::Prefilled(f.game.id.clone())))
            .unwrap();
        assert!(poll());
        with((), |r| assert!(matches!(r.modal, Some(Modal::Completed))));
    }
    #[cfg(not(any(feature = "miyoo", feature = "simulator")))]
    async fn finish_without_input(outcome: Result<Output>, cancel: bool) {
        use crate::allium_launcher::AlliumLauncher;
        use std::time::Duration;

        let completed = outcome.is_ok();
        let f = StartupFixture::new("no-input-result");
        f.configured();
        let snapshot = common::theme::runtime::ThemeSnapshot::builtin("orbit").unwrap();
        let selection = common::constants::ALLIUM_BASE_DIR.join("state/ui-theme.json");
        let previous_selection = std::fs::read(&selection).ok();
        std::fs::write(
            &selection,
            serde_json::to_vec(&common::theme::selection::Selection::from_snapshot(
                &snapshot, 1,
            ))
            .unwrap(),
        )
        .unwrap();
        let app = AlliumLauncher::new(DefaultPlatform::new().unwrap());
        if let Some(bytes) = previous_selection {
            std::fs::write(&selection, bytes).unwrap();
        } else {
            std::fs::remove_file(&selection).unwrap();
        }
        let mut app = app.unwrap();
        initialize(f.root.clone()).unwrap();
        let styles = Stylesheet::load().unwrap();
        let (progress_tx, progress) = mpsc::sync_channel(1);
        let (result_tx, result) = mpsc::sync_channel(1);
        progress_tx
            .send(Progress {
                game_id: f.game.id.clone(),
                phase: "download",
                bytes_done: 683 * 1048576,
                bytes_total: 683 * 1048576,
            })
            .unwrap();
        with((), |r| {
            r.job = Some(Job {
                epoch: r.epoch,
                prefill_id: Some(f.game.id.clone()),
                cancel: Cancellation::default(),
                progress,
                result,
                thread: None,
            });
        });
        assert!(poll());
        let mut working = DefaultPlatform::new().unwrap().display().unwrap();
        assert!(draw(&mut working, &styles));
        assert!(!poll());

        let finish = async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            if cancel {
                with((), |r| r.job.as_ref().unwrap().cancel.cancel());
            }
            let worker = thread::spawn(move || result_tx.send(outcome).unwrap());
            worker.join().unwrap();
            assert!(
                active(),
                "finished result must keep the polling timer active"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        tokio::select! {
            result = app.run_event_loop() => panic!("event loop stopped: {result:?}"),
            _ = finish => {}
        }
        assert!(
            !active(),
            "result was not consumed within the polling interval"
        );
        with((), |r| {
            assert!(if cancel {
                r.modal.is_none()
            } else if completed {
                matches!(r.modal, Some(Modal::Completed))
            } else {
                matches!(r.modal, Some(Modal::Error(_)))
            });
        });
        assert!(!poll(), "event loop must consume the redraw without input");
        let actual = app.proof_frame();
        let mut expected = DefaultPlatform::new().unwrap().display().unwrap();
        let terminal_modal_drawn = draw(&mut expected, &styles);
        assert_eq!(terminal_modal_drawn, !cancel);
        for y in 94..374 {
            let row = y * 640 * 4 + 108 * 4..y * 640 * 4 + 532 * 4;
            if cancel {
                if actual.data()[row.clone()] != working.pixmap().data()[row] {
                    return;
                }
            } else {
                assert_eq!(actual.data()[row.clone()], expected.pixmap().data()[row]);
            }
        }
        assert!(!cancel, "cancelled Working modal remained on screen");
    }
    #[cfg(not(any(feature = "miyoo", feature = "simulator")))]
    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn dl6_completion_draws_without_input() {
        finish_without_input(Ok(Output::Prefilled("startup-fixture".into())), false).await;
    }
    #[cfg(not(any(feature = "miyoo", feature = "simulator")))]
    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn dl6_failure_draws_without_input() {
        finish_without_input(Err(anyhow::anyhow!("Verification failed")), false).await;
    }
    #[cfg(not(any(feature = "miyoo", feature = "simulator")))]
    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn dl6_cancellation_redraws_without_input() {
        finish_without_input(Err(anyhow::anyhow!("Cancelled")), true).await;
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn prefill_ready_clears_pending_permit_and_queued_launch() {
        let f = StartupFixture::new("prefill-ready");
        f.configured();
        let disc_state = f.root.join("Saves/CurrentProfile/saves/startup.ldci");
        std::fs::create_dir_all(disc_state.parent().unwrap()).unwrap();
        std::fs::write(&disc_state, b"disc state sentinel").unwrap();
        initialize(f.root.clone()).unwrap();
        let game = Game::new(f.root.join(&f.game.game_path));
        let database = common::database::Database::new().unwrap();
        let rows_before = database.select_games(&[&game.path]).unwrap();
        let info_before = std::fs::read(common::constants::ALLIUM_GAME_INFO.as_path()).ok();
        let disc_before = std::fs::read(&disc_state).unwrap();
        RUNTIME.with(|cell| {
            let mut runtime = cell.borrow_mut();
            let runtime = runtime.as_mut().unwrap();
            runtime.pending = Some(Pending {
                epoch: runtime.epoch,
                game: game.clone(),
                reset: false,
                disc: None,
            });
            runtime.permit = Some(game.path.clone());
            runtime
                .actions
                .push_back(Action::Launch(game.clone(), false));
        });
        prefill(&f.game.id).unwrap();
        RUNTIME.with(|cell| {
            let runtime = cell.borrow();
            let runtime = runtime.as_ref().unwrap();
            assert!(runtime.pending.is_none());
            assert!(runtime.permit.is_none());
            assert!(runtime.actions.is_empty());
        });
        settle();
        assert!(action().is_none());
        assert!(guard_launch(&game.path).is_err());
        assert_eq!(badge(&game.path).as_deref(), Some("Cached"));
        assert_eq!(database.select_games(&[&game.path]).unwrap(), rows_before);
        assert_eq!(
            std::fs::read(common::constants::ALLIUM_GAME_INFO.as_path()).ok(),
            info_before
        );
        assert_eq!(std::fs::read(&disc_state).unwrap(), disc_before);
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn prefill_late_ready_result_cancelled_before_poll_cannot_launch() {
        let f = StartupFixture::new("prefill-late");
        f.configured();
        let disc_state = f.root.join("Saves/CurrentProfile/saves/startup.ldci");
        std::fs::create_dir_all(disc_state.parent().unwrap()).unwrap();
        std::fs::write(&disc_state, b"disc state sentinel").unwrap();
        initialize(f.root.clone()).unwrap();
        let game = Game::new(f.root.join(&f.game.game_path));
        let database = common::database::Database::new().unwrap();
        let rows_before = database.select_games(&[&game.path]).unwrap();
        let info_before = std::fs::read(common::constants::ALLIUM_GAME_INFO.as_path()).ok();
        let disc_before = std::fs::read(&disc_state).unwrap();
        prefill(&f.game.id).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !RUNTIME.with(|cell| {
            cell.borrow()
                .as_ref()
                .unwrap()
                .job
                .as_ref()
                .unwrap()
                .thread
                .as_ref()
                .unwrap()
                .is_finished()
        }) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(active());
        assert!(event(KeyEvent::Pressed(Key::B)).unwrap());
        settle();
        assert!(action().is_none());
        assert!(guard_launch(&game.path).is_err());
        RUNTIME.with(|cell| {
            let runtime = cell.borrow();
            let runtime = runtime.as_ref().unwrap();
            assert!(runtime.pending.is_none());
            assert!(runtime.permit.is_none());
        });
        assert_eq!(
            std::fs::read(common::constants::ALLIUM_GAME_INFO.as_path()).ok(),
            info_before
        );
        assert_eq!(database.select_games(&[&game.path]).unwrap(), rows_before);
        assert_eq!(std::fs::read(&disc_state).unwrap(), disc_before);
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn network_orbit_confirmed_absent_profile_retains_legacy_launch() {
        let f = StartupFixture::new("absent");
        initialize(f.root.clone()).unwrap();
        let game = Game::new(f.root.join(&f.game.game_path));
        assert!(!begin(&game, false).unwrap());
        assert!(!begin(&game, true).unwrap());
        assert!(guard_launch(&game.path).is_ok());
        assert!(!event(KeyEvent::Pressed(Key::X)).unwrap());
        assert!(!poll());
        assert!(
            !f.root.join(".allium").exists(),
            "absent configuration remains read-only"
        );
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn network_orbit_failed_startup_blocks_metadata_unknown_before_all_launch_side_effects() {
        let database = common::database::Database::new().unwrap();
        let mut mapper = crate::consoles::ConsoleMapper::new();
        mapper.load_config().unwrap();
        for label in ["ready-index", "unload-intent", "catalog", "config"] {
            let f = StartupFixture::new(label);
            f.configured();
            let state = f.root.join(".allium/state/network-library");
            let unknown = state.join(match label {
                "ready-index" => "installed/startup-fixture.json",
                "unload-intent" => "installed/startup-fixture.unloading.json",
                "catalog" => "catalog.json",
                _ => "config.json",
            });
            std::fs::write(&unknown, b"{torn unknown metadata").unwrap();
            let unknown_before = std::fs::read(&unknown).unwrap();
            let mut game = Game::new(f.root.join(&f.game.game_path));
            let count_before = database.select_games(&[&game.path]).unwrap()[0]
                .as_ref()
                .map_or(0, |g| g.play_count);
            let info = common::constants::ALLIUM_GAME_INFO.as_path();
            let info_before = std::fs::read(info).ok();
            assert!(initialize(f.root.clone()).is_err());
            assert!(download_id(&game.path).is_none());
            assert!(prefill(&f.game.id).is_err());
            if label == "ready-index" {
                let mut platform = DefaultPlatform::new().unwrap();
                let mut display = platform.display().unwrap();
                fill_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(0, 0, 640, 480),
                    Color::new(243, 244, 238),
                );
                assert!(draw(&mut display, &Stylesheet::load().unwrap()));
                display
                    .pixmap()
                    .to_owned()
                    .save_png(
                        PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap())
                            .join("native-network-startup-blocked.png"),
                    )
                    .unwrap();
            }

            assert!(guard_launch(&game.path).is_err());
            assert!(poll());
            assert!(event(KeyEvent::Pressed(Key::B)).unwrap());
            poll();
            assert!(!poll());
            assert!(
                !event(KeyEvent::Pressed(Key::L)).unwrap(),
                "Apps/Settings navigation remains available after Back"
            );
            for reset in [false, true] {
                assert!(begin(&game, reset).unwrap());
                assert!(mapper.launch_game(&database, &mut game, reset).is_err());
                assert!(action().is_none());
                assert!(!active());
                event(KeyEvent::Pressed(Key::B)).unwrap();
            }
            let playlist = crate::discs::Playlist {
                path: f.root.join("Roms/PS/Unknown.m3u"),
                discs: vec![crate::discs::Disc {
                    path: game.path.clone(),
                    available: true,
                }],
                core_compatible: true,
            };
            assert!(
                mapper
                    .launch_game_with_disc(&database, &mut game, &playlist, 0)
                    .is_err()
            );
            // Unknown catalog means no managed-path inference: the entire configured PS namespace is blocked.
            assert!(guard_launch(&f.root.join("Roms/PS/Other.chd")).is_err());
            assert!(guard_launch(Path::new("Roms/PS/Other.chd")).is_err());
            assert!(guard_launch(&f.root.join("Roms/SFC/Star Fox.sfc")).is_ok());
            assert_eq!(
                database.select_games(&[&game.path]).unwrap()[0]
                    .as_ref()
                    .map_or(0, |g| g.play_count),
                count_before
            );
            assert_eq!(std::fs::read(info).ok(), info_before);
            assert_eq!(std::fs::read(&unknown).unwrap(), unknown_before);
            assert_eq!(
                std::fs::read(&game.path).unwrap(),
                b"startup fake ROM bytes"
            );
        }
    }
}

#[cfg(test)]
thread_local! { static LOOP_PROBE: RefCell<Option<Box<dyn FnMut(&'static str) -> bool>>> = RefCell::new(None); }
#[cfg(test)]
pub(crate) fn test_loop_event(event: &'static str) -> bool {
    LOOP_PROBE.with(|p| p.borrow_mut().as_mut().is_some_and(|probe| probe(event)))
}
