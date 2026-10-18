#[path = "orbit_portrait.rs"]
mod portrait;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use common::display::color::Color;
use common::display::font::{FontTextStyle, FontTextStyleBuilder};
use common::display::{Display, fill_rect, fill_rounded_rect};
use common::geom::{Point, Rect};
use common::platform::{DefaultPlatform, Platform};
use common::stylesheet::Stylesheet;
use common::theme::art::{Target as ArtTarget, Targets as ArtTargets};
use tiny_skia::{Pixmap, PixmapPaint, Transform};

use crate::entry::Entry;

pub fn background() -> Color {
    Color::new(243, 244, 238)
}
fn hero_background() -> Color {
    Color::new(231, 235, 224)
}
fn text_color() -> Color {
    Color::new(35, 48, 51)
}
fn muted_color() -> Color {
    Color::new(96, 118, 105)
}
pub const HEADER_HEIGHT: i32 = 54;
pub const FOOTER_Y: i32 = 430;
pub const CONTENT_HEIGHT: u32 = 376;
const COVER_SIZE: u32 = 132;
const THEME_COVER_SIZE: u32 = 140;
const CARD_STRIDE: i32 = 176;
const SHELF_CARDS: usize = 3;
const CACHE_ENTRIES: usize = 16;
const RESOLVED_PATH_LIMIT: usize = 64;
const CACHE_PIXEL_BYTES: usize = common::theme::frame::ART_INPUT_BYTES;
const BASE_CACHE_BYTES: usize = 3 * 1024 * 1024;
const WORKER_METADATA_BYTES: usize = 1024 * 1024;
const WORKER_STACK_BYTES: usize = 256 * 1024;
thread_local! {
    static EXTRA_SOURCE_BYTES: std::cell::RefCell<Vec<std::rc::Weak<std::cell::Cell<usize>>>> = const { std::cell::RefCell::new(Vec::new()) };
}
fn source_account() -> std::rc::Rc<std::cell::Cell<usize>> {
    let account = std::rc::Rc::new(std::cell::Cell::new(0));
    EXTRA_SOURCE_BYTES.with(|accounts| {
        let mut accounts = accounts.borrow_mut();
        accounts.retain(|a| a.strong_count() > 0);
        accounts.push(std::rc::Rc::downgrade(&account));
    });
    account
}
// A 480x480 RGBA exporter PNG fits with filter/zlib overhead, within the worker memory budget.
const MAX_PNG_BYTES: u64 = 960 * 1024;

pub fn enabled() -> bool {
    Stylesheet::orbit_enabled()
}

thread_local! {
    #[cfg(test)]
    static RESOLVED_PATH_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    #[cfg(test)]
    static PROTECTION_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    #[cfg(test)]
    pub(crate) static ART_SNAPSHOT_HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ART_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static ART_CACHE_REVISION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static ART_COMPLETIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static METADATA_COMPLETIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
pub(super) fn metadata_completion_generation() -> u64 {
    METADATA_COMPLETIONS.with(std::cell::Cell::get)
}
pub(crate) fn art_cache_revision() -> u64 { ART_CACHE_REVISION.with(std::cell::Cell::get) }
fn art_cache_changed() {
    ART_CACHE_REVISION.with(|revision| revision.set(revision.get().wrapping_add(1)));
}
pub(crate) fn completion_generation() -> u64 {
    ART_COMPLETIONS.with(std::cell::Cell::get)
}
pub(crate) fn visible_completion() {
    ART_COMPLETIONS.with(|generation| generation.set(generation.get().wrapping_add(1)));
}
pub(crate) fn invalidate_art() {
    common::media::invalidate();
    ART_GENERATION.with(|generation| generation.set(generation.get().wrapping_add(1)));
}
fn art_generation() -> u64 {
    ART_GENERATION.with(std::cell::Cell::get)
}

#[derive(Debug)]
struct CachedArt {
    target: Option<ArtTarget>,
    path: PathBuf,
    image: Option<std::sync::Arc<Pixmap>>,
}

#[derive(Clone, Debug)]
pub(crate) enum WarmArt {
    Metadata(Entry),
    Cover(Entry),
    CoverFallback(Entry),
    Logo(Entry),
    #[allow(dead_code)]
    OtherCover(Entry),
    Hero(Entry),
}

#[derive(Debug)]
struct CompletedTask {
    targets: ArtTargets,
    key: super::art_queue::AssetKey,
    admission_rejected: bool,
    missing: bool,
    // Dedupe must not retain metadata after the common cache evicts its record.
    metadata: Option<std::sync::Weak<common::media::Metadata>>,
}
impl CompletedTask {
    fn is_resident(&self, task: &WarmArt) -> bool {
        self.metadata.as_ref().is_none_or(|metadata| {
            metadata.ptr_eq(&std::sync::Arc::downgrade(&common::media::cached_metadata(
                task.entry().path(),
            )))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PathRevision {
    media: u64,
    paths: u64,
    network: u64,
}
impl PathRevision {
    fn current() -> Self {
        Self { media: common::media::generation(), paths: common::media::path_generation(),
            network: crate::network_runtime::art_path_generation() }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ArtStamp {
    artwork: u64,
    completion: u64,
    path: PathRevision,
    invalidation: (u64, u64, u64),
    media_generation: u64,
    owner: u64,
    tall: bool,
    ink: bool,
    targets: ArtTargets,
    recording: bool,
}

pub(super) struct ArtSnapshot {
    theme: common::theme::fonts::WeakThemeHandle,
    theme_generation: u64,
    theme_digest: String,
    stamp: ArtStamp,
    selected_logo: Option<std::sync::Weak<Pixmap>>,
    selected_cover: Option<std::sync::Weak<Pixmap>>,
    selected_hero: Option<std::sync::Weak<Pixmap>>,
    next_cover: Option<std::sync::Weak<Pixmap>>,
    next_cover_expected: bool,
    entries: Vec<(String, std::sync::Weak<Pixmap>)>,
    completed_misses: common::theme::frame::ArtMisses,
}
impl ArtSnapshot {
    pub(super) fn restore(&self, orbit: &Orbit, handle: &common::theme::ThemeHandle,
        targets: ArtTargets,
    ) -> Option<common::theme::frame::Art> {
        if !self.theme.matches(handle) || self.theme_generation != handle.generation()
            || self.theme_digest != handle.digest() || self.stamp != orbit.art_stamp()
            || self.stamp.targets != targets { return None; }
        fn upgrade(source: &Option<std::sync::Weak<Pixmap>>) -> Option<Option<std::sync::Arc<Pixmap>>> {
            match source { Some(source) => source.upgrade().map(Some), None => Some(None) }
        }
        let art = common::theme::frame::Art {
            selected_logo: upgrade(&self.selected_logo)?,
            selected_cover: upgrade(&self.selected_cover)?,
            selected_hero: upgrade(&self.selected_hero)?,
            next_cover: upgrade(&self.next_cover)?,
            next_cover_expected: self.next_cover_expected,
            entries: self.entries.iter().map(|(id, source)| Some((id.clone(), source.upgrade()?)))
                .collect::<Option<_>>()?,
            completed_misses: self.completed_misses.clone(),
        };
        #[cfg(test)]
        ART_SNAPSHOT_HITS.with(|n| n.set(n.get() + 1));
        Some(art)
    }
}

#[derive(Debug, Default)]
struct ResolvedPaths {
    revision: Option<PathRevision>,
    entries: VecDeque<(PathBuf, bool, &'static str, PathBuf, Option<String>)>,
}

#[derive(Debug)]
pub struct Orbit {
    display_names: crate::entry::display_names::DisplayNames,
    cache: VecDeque<CachedArt>,
    source_account: std::rc::Rc<std::cell::Cell<usize>>,
    generation: u64,
    art_epoch: u64,
    media_generation: u64,
    legacy_ready: bool,
    ink_art: bool,
    tall_art: bool,
    targets: ArtTargets,
    theme_identity: Option<(u64, String)>,
    covered_art: Vec<(PathBuf, Option<ArtTarget>)>,
    retired: Vec<std::sync::Weak<Pixmap>>,
    dirty: bool,
    worker_id: u64,
    worker_epoch: u64,
    pending_tasks: VecDeque<WarmArt>,
    cancellation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    completed_tasks: VecDeque<CompletedTask>,
    selected_path: Option<PathBuf>,
    protected_paths: Vec<PathBuf>,
    selected_protection: Option<(PathRevision, bool, bool)>,
    resolved_paths: std::cell::RefCell<ResolvedPaths>,
    visible_cover_entries: Vec<PathBuf>,
    #[cfg(feature = "perf-bench")]
    worker_samples: Vec<(common::perf::Counts, serde_json::Value)>,
}

impl Orbit {
    pub fn new() -> Self {
        Self {
            display_names: crate::entry::display_names::DisplayNames::default(),
            cache: VecDeque::new(),
            source_account: source_account(),
            generation: art_generation(),
            art_epoch: 0,
            media_generation: common::media::generation(),
            legacy_ready: false,
            ink_art: false,
            tall_art: false,
            targets: ArtTargets::default(),
            theme_identity: None,
            covered_art: Vec::new(),
            retired: Vec::new(),
            dirty: true,
            worker_id: {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            },
            worker_epoch: 0,
            pending_tasks: VecDeque::new(),
            cancellation: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            completed_tasks: VecDeque::new(),
            selected_path: None,
            protected_paths: Vec::new(),
            selected_protection: None,
            resolved_paths: std::cell::RefCell::new(ResolvedPaths::default()),
            visible_cover_entries: Vec::new(),
            #[cfg(feature = "perf-bench")]
            worker_samples: Vec::new(),
        }
    }

    pub(crate) fn set_display_names(&mut self, names: crate::entry::display_names::DisplayNames) {
        self.display_names = names;
        self.dirty = true;
    }

    pub(crate) fn defer_legacy(&mut self) {
        self.legacy_ready = false;
        self.dirty = true;
    }

    pub(crate) fn refresh_names(&mut self) {
        let media_generation = common::media::generation();
        if self.media_generation != media_generation {
            // Cold indexes can supply previously missing art. Successful paths remain reusable.
            self.cache.retain(|item| item.image.is_some());
            if !self.completed_tasks.is_empty() { art_cache_changed(); }
            self.completed_tasks.clear();
            self.media_generation = media_generation;
        }
        self.defer_legacy();
    }

    pub fn prepare(&mut self, entries: &[Entry], selected: usize) {
        self.refresh_art();
        self.dirty = true;
        if let Some(entry) = entries.get(selected) {
            self.cache_art(art_path(entry, "heroes"), (360, 210));
        }
        self.legacy_ready = true;
        let first = shelf_start(selected, entries.len());
        for entry in entries.iter().skip(first).take(SHELF_CARDS + 1) {
            self.cache_art(art_path(entry, "covers"), (101, 101));
        }
    }

    pub(crate) fn set_cover_shape(&mut self, tall: bool) {
        if self.tall_art != tall {
            self.release_art();
            self.tall_art = tall;
        }
    }
    fn with_art_path<T>(&self, entry: &Entry, kind: &'static str, use_path: impl FnOnce(&Path) -> T) -> T {
        self.with_art_identity(entry, kind, |path, _| use_path(path))
    }
    fn with_art_identity<T>(&self, entry: &Entry, kind: &'static str, use_path: impl FnOnce(&Path, Option<&str>) -> T) -> T {
        let revision = PathRevision::current();
        let directory = matches!(entry, Entry::Directory(_));
        let mut paths = self.resolved_paths.borrow_mut();
        if paths.revision != Some(revision) {
            paths.entries.clear();
            paths.revision = Some(revision);
        }
        if let Some((_, _, _, path, id)) = paths.entries.iter().find(|(game, dir, k, _, _)| {
            game == entry.path() && *dir == directory && *k == kind
        }) {
            return use_path(path, id.as_deref());
        }
        #[cfg(test)]
        RESOLVED_PATH_BUILDS.with(|n| n.set(n.get() + 1));
        let path = if kind == "tall" {
            common::media::cached_resolve(entry.path(), common::media::Kind::TallCover)
                .unwrap_or_else(|| art_path(entry, "stock"))
        } else { art_path(entry, kind) };
        if paths.entries.len() >= RESOLVED_PATH_LIMIT { paths.entries.pop_front(); }
        let id = matches!(kind, "tall" | "stock").then(|| crate::theme_adapter::entry_id(entry.path()));
        paths.entries.push_back((entry.path().into(), directory, kind, path, id));
        let item = paths.entries.back().unwrap();
        use_path(&item.3, item.4.as_deref())
    }
    fn with_cover_path<T>(&self, entry: &Entry, use_path: impl FnOnce(&Path) -> T) -> T {
        self.with_art_path(entry, if self.tall_art { "tall" } else { "stock" }, use_path)
    }
    fn cover_path(&self, entry: &Entry) -> PathBuf {
        self.with_cover_path(entry, Path::to_path_buf)
    }
    fn cache_logo(&mut self, entry: &Entry) {
        let Some(path) = common::media::cached_resolve(entry.path(), common::media::Kind::Logo)
        else {
            return;
        };
        if !self.cache.iter().any(|item| item.path == path && item.target == self.targets.logo) {
            let image = if self.ink_art {
                load_device_art_fitted(&path, 480, 200, self.targets.logo, true, false)
            } else {
                load_prepared_art(&path, (0, 0), 480, 200, false)
            }
            .map(std::sync::Arc::new);
            self.insert_cached(CachedArt { target: self.targets.logo, path, image });
        }
    }
    pub(crate) fn set_art_targets(&mut self, targets: ArtTargets) {
        if self.targets != targets {
            self.cancel_selection();
            self.targets = targets;
        }
    }
    pub(crate) fn set_art_theme(&mut self, handle: &common::theme::ThemeHandle) -> bool {
        let changed = self.theme_identity.as_ref().is_none_or(|(generation, digest)| {
            *generation != handle.generation() || digest != handle.digest()
        });
        if changed {
            self.release_art();
            self.theme_identity = Some((handle.generation(), handle.digest().to_owned()));
        }
        self.refresh_art();
        changed
    }
    pub(crate) fn protect_covered(&mut self, entry: Option<&Entry>, targets: ArtTargets) {
        self.covered_art.clear();
        if let Some(entry) = entry {
            self.covered_art.extend([
                (art_path(entry, "heroes"), targets.hero),
                (art_path(entry, "logos"), targets.logo),
                (self.cover_path(entry), targets.cover),
            ]);
        }
    }
    fn is_covered(&self, item: &CachedArt) -> bool {
        self.covered_art.iter().any(|(path, target)| *path == item.path && *target == item.target)
    }
    fn task_target(task: &WarmArt, targets: ArtTargets) -> Option<ArtTarget> {
        match task {
            WarmArt::Hero(_) => targets.hero,
            WarmArt::Logo(_) => targets.logo,
            WarmArt::Cover(_) | WarmArt::CoverFallback(_) | WarmArt::OtherCover(_) => targets.cover,
            WarmArt::Metadata(_) => None,
        }
    }
    pub(crate) fn set_ink_art(&mut self, enabled: bool) {
        if self.ink_art != enabled {
            self.release_art();
            self.ink_art = enabled;
        }
    }

    pub(crate) fn prepare_window(
        &mut self,
        entries: &[Entry],
        selected: usize,
        first: usize,
        count: usize,
    ) {
        let _ = (entries, selected, first, count);
        self.refresh_art();
    }
    #[cfg(test)]
    pub(super) fn prepare_loaded_window(
        &mut self,
        entries: &[Entry],
        selected: usize,
        first: usize,
        count: usize,
    ) {
        let mut work = Self::navigation_art_work(entries, selected, first, count);
        while !work.is_empty() {
            self.warm_art_step_sync(&mut work);
        }
        self.prepare_window(entries, selected, first, count);
    }
    #[cfg(test)]
    pub(crate) fn navigation_art_work(
        entries: &[Entry],
        selected: usize,
        first: usize,
        count: usize,
    ) -> VecDeque<WarmArt> {
        let mut work = VecDeque::new();
        if let Some(entry) = entries.get(selected) {
            work.extend([
                WarmArt::Hero(entry.clone()),
                WarmArt::Cover(entry.clone()),
                WarmArt::Logo(entry.clone()),
            ]);
        }
        work.extend(
            entries
                .iter()
                .skip(first)
                .take(count.min(CACHE_ENTRIES - 1))
                .cloned()
                .map(WarmArt::Cover),
        );
        if let Some(next) = peek_game(entries, first, count) {
            work.push_back(WarmArt::Cover(entries[next].clone()));
        }
        let mut seen = std::collections::HashSet::new();
        work.retain(|task| seen.insert(task.key()));
        work
    }
    fn warm_art_step_sync(&mut self, work: &mut VecDeque<WarmArt>) {
        self.refresh_art();
        let Some(task) = work.pop_front() else {
            return;
        };
        if let WarmArt::Metadata(entry) = task {
            if let Entry::Game(game) = entry {
                let _ = super::details::Details::new(&game, "");
            }
            return;
        }
        for kind in task.media_kinds(self.tall_art) {
            common::media::resolve(task.entry().path(), kind);
        }
        if matches!(task, WarmArt::Cover(_))
            && self.tall_art
            && common::media::cached_resolve(task.entry().path(), common::media::Kind::TallCover)
                .is_none()
        {
            common::media::resolve(task.entry().path(), common::media::Kind::Cover);
        }
        if !art_job_current() {
            return;
        }
        let fallback = matches!(&task, WarmArt::CoverFallback(_));
        match task {
            WarmArt::Metadata(_) => unreachable!(),
            WarmArt::Hero(entry) => self.cache_art(art_path(&entry, "heroes"), (360, 210)),
            WarmArt::Logo(entry) => self.cache_logo(&entry),
            WarmArt::OtherCover(entry) => {
                let path = if self.tall_art {
                    common::media::cached_resolve(entry.path(), common::media::Kind::Cover)
                } else {
                    common::media::cached_resolve(entry.path(), common::media::Kind::TallCover)
                };
                if let Some(path) = path {
                    if path == self.cover_path(&entry) {
                        return;
                    }
                    if !self.cache.iter().any(|item| item.path == path && item.target == self.targets.cover) {
                        let image = if self.ink_art {
                            load_device_art_fitted(&path, 480, 480, self.targets.cover, false, false)
                        } else {
                            load_prepared_art(
                                &path,
                                (0, 0),
                                THEME_COVER_SIZE,
                                THEME_COVER_SIZE,
                                false,
                            )
                        };
                        self.insert_cached(CachedArt { target: self.targets.cover,
                            path,
                            image: image.map(std::sync::Arc::new),
                        });
                    }
                }
            }
            WarmArt::Cover(entry) | WarmArt::CoverFallback(entry) => {
                let path = self.cover_path(&entry);
                if self.cache.iter().any(|item| item.path == path && item.target == self.targets.cover) {
                    return;
                }
                let image = if fallback {
                    load_prepared_art(
                        &art_path(&entry, "covers"),
                        (101, 101),
                        COVER_SIZE,
                        COVER_SIZE,
                        false,
                    )
                } else if self.ink_art {
                    load_device_art_fitted(&path, 480, 480, self.targets.cover, false,
                        self.tall_art && common::media::cached_resolve(entry.path(), common::media::Kind::TallCover).is_none())
                } else {
                    load_prepared_art(&path, (0, 0), THEME_COVER_SIZE, THEME_COVER_SIZE, false)
                };
                let image = if fallback && self.ink_art
                    && self.tall_art
                    && common::media::cached_resolve(entry.path(), common::media::Kind::TallCover)
                        .is_none()
                {
                    image.and_then(portrait::compose)
                } else {
                    image
                };
                let image = if fallback && self.ink_art {
                    image.and_then(|image| fit_art(image, self.targets.cover))
                } else { image };
                if image.is_none() && !fallback {
                    work.push_front(WarmArt::CoverFallback(entry));
                } else {
                    self.insert_cached(CachedArt { target: self.targets.cover,
                        path,
                        image: image.map(std::sync::Arc::new),
                    });
                }
            }
        }
    }
    pub(crate) fn theme_art(
        &self,
        entries: &[Entry],
        selected: usize,
        first: usize,
        count: usize,
    ) -> common::theme::frame::Art {
        self.theme_art_for_targets(entries, selected, first, count, self.targets)
    }
    fn art_stamp(&self) -> ArtStamp {
        ArtStamp {
            artwork: art_cache_revision(), completion: completion_generation(), path: PathRevision::current(),
            invalidation: (art_generation(), self.generation, self.art_epoch),
            media_generation: self.media_generation, owner: self.worker_id,
            tall: self.tall_art, ink: self.ink_art, targets: self.targets,
            recording: common::perf_trace::recording(),
        }
    }
    pub(super) fn art_snapshot(&self, handle: &common::theme::ThemeHandle,
        art: &common::theme::frame::Art,
    ) -> ArtSnapshot {
        ArtSnapshot {
            theme: handle.downgrade(), theme_generation: handle.generation(), theme_digest: handle.digest().into(),
            stamp: self.art_stamp(),
            selected_logo: art.selected_logo.as_ref().map(std::sync::Arc::downgrade),
            selected_cover: art.selected_cover.as_ref().map(std::sync::Arc::downgrade),
            selected_hero: art.selected_hero.as_ref().map(std::sync::Arc::downgrade),
            next_cover: art.next_cover.as_ref().map(std::sync::Arc::downgrade),
            next_cover_expected: art.next_cover_expected,
            entries: art.entries.iter().map(|(id, source)| (id.clone(), std::sync::Arc::downgrade(source))).collect(),
            completed_misses: art.completed_misses.clone(),
        }
    }
    pub(crate) fn theme_art_for_targets(&self, entries: &[Entry], selected: usize, first: usize, count: usize, targets: ArtTargets) -> common::theme::frame::Art {
        let _stage = common::perf_trace::stage(common::perf_trace::Stage::ArtworkLookup);
        let mut art = common::theme::frame::Art::default();
        if let Some(entry) = entries.get(selected) {
            art.selected_logo = self.with_art_path(entry, "logos", |path| self.cached_variant(path, targets.logo).cloned());
            art.selected_hero = self.with_art_path(entry, "heroes", |path| self.cached_variant(path, if self.ink_art { targets.hero } else { None }).cloned());
            art.selected_cover = self.with_cover_path(entry, |path| self.cached_variant(path, targets.cover).cloned());
        }
        if let Some(next) = peek_game(entries, first, count) {
            art.next_cover_expected = true;
            art.next_cover = self.with_cover_path(&entries[next], |path| self.cached_variant(path, targets.cover).cloned());
        }
        for entry in entries.iter().skip(first).take(count) {
            self.with_art_identity(entry, if self.tall_art { "tall" } else { "stock" }, |path, id| {
                if let Some(image) = self.cached_variant(path, targets.cover) {
                    art.entries.insert(id.unwrap().to_owned(), image.clone());
                }
            });
        }
        if common::perf_trace::recording() {
            let missed = |task: WarmArt| {
                let key = task.key();
                self.completed_tasks.iter().any(|completed| completed.key == key && Self::task_target(&task, completed.targets) == Self::task_target(&task, targets) && completed.missing)
            };
            if let Some(entry) = entries.get(selected) {
                art.completed_misses.selected_cover = art.selected_cover.is_none()
                    && missed(WarmArt::Cover(entry.clone()));
                art.completed_misses.selected_hero = art.selected_hero.is_none()
                    && missed(WarmArt::Hero(entry.clone()));
                art.completed_misses.selected_logo = art.selected_logo.is_none()
                    && missed(WarmArt::Logo(entry.clone()));
            }
            if let Some(next) = peek_game(entries, first, count) {
                art.completed_misses.next_cover = art.next_cover.is_none()
                    && missed(WarmArt::Cover(entries[next].clone()));
            }
            for entry in entries.iter().skip(first).take(count) {
                self.with_art_identity(entry, if self.tall_art { "tall" } else { "stock" }, |path, id| {
                    if self.cached_variant(path, targets.cover).is_none() && missed(WarmArt::Cover(entry.clone())) {
                        art.completed_misses.entries.insert(id.unwrap().to_owned());
                    }
                });
            }
        }
        art
    }

    fn cached_image(&self, path: &Path) -> Option<&std::sync::Arc<Pixmap>> {
        self.cached_variant(path, None)
    }
    fn cached_variant(&self, path: &Path, target: Option<ArtTarget>) -> Option<&std::sync::Arc<Pixmap>> {
        self.cache
            .iter()
            .find(|item| item.path == path && item.target == target)?
            .image
            .as_ref()
    }

    fn cache_art(&mut self, path: PathBuf, dimensions: (u32, u32)) {
        let target = if self.ink_art && dimensions == (360, 210) { self.targets.hero } else { None };
        if let Some(index) = self.cache.iter().position(|item| item.path == path && item.target == target) {
            let item = self.cache.remove(index).unwrap();
            self.cache.push_front(item);
            common::perf_trace::hit(common::perf_trace::ArtHit::Ram);
            #[cfg(feature = "perf-bench")]
            common::perf::hit(common::perf::Op::RamHit);
            return;
        }
        if self.ink_art && dimensions == (360, 210) {
            self.insert_cached(CachedArt { target,
                image: load_device_art_fitted(&path, 640, 480, target, false, false).map(std::sync::Arc::new),
                path,
            });
            return;
        }
        let (width, height, crop) = if dimensions == (101, 101) {
            (COVER_SIZE, COVER_SIZE, false)
        } else {
            (390, 189, true)
        };
        let dimensions = if path.file_name().is_some_and(|n| {
            n.to_string_lossy().ends_with(".hero.png")
                || n.to_string_lossy().ends_with(".orbit.png")
        }) {
            dimensions
        } else {
            (0, 0)
        };
        self.insert_cached(CachedArt { target: None,
            image: load_prepared_art(&path, dimensions, width, height, crop)
                .map(std::sync::Arc::new),
            path,
        });
    }

    fn source_bytes(&mut self) -> usize {
        self.retired.retain(|image| image.strong_count() != 0);
        let bytes = self
            .cache
            .iter()
            .filter_map(|item| item.image.as_ref())
            .map(|image| image.data().len())
            .sum::<usize>()
            + self
                .retired
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .map(|image| image.data().len())
                .sum::<usize>();
        self.source_account
            .set(bytes.saturating_sub(BASE_CACHE_BYTES));
        bytes
    }

    fn retire(&mut self, item: CachedArt) {
        art_cache_changed();
        if let Some(image) = item
            .image
            .filter(|image| std::sync::Arc::strong_count(image) > 1)
        {
            self.retired.push(std::sync::Arc::downgrade(&image));
        }
    }

    fn is_protected(&self, item: &CachedArt) -> bool {
        self.protected_paths.contains(&item.path)
            && [self.targets.hero, self.targets.cover, self.targets.logo].contains(&item.target)
    }
    fn insert_cached(&mut self, item: CachedArt) -> bool {
        if let Some(index) = self.cache.iter().position(|old| old.path == item.path && old.target == item.target) {
            let old = self.cache.remove(index).unwrap();
            self.retire(old);
        }
        let bytes = item.image.as_ref().map_or(0, |image| image.data().len());
        while self.cache.len() >= CACHE_ENTRIES
            || self.source_bytes() + bytes > self.source_budget()
        {
            let Some(index) = self
                .cache
                .iter()
                .rposition(|item| !self.is_protected(item) && !self.is_covered(item))
                .or_else(|| self.cache.iter().rposition(|item| !self.is_protected(item)))
            else {
                // The soft allowance is shared with inactive lists and retained frame sources.
                // Visible sources must survive until the next frame releases those references.
                let resident_bytes: usize = self.cache.iter().filter_map(|item| item.image.as_ref())
                    .map(|image| image.data().len()).sum();
                if self.is_protected(&item) && resident_bytes + bytes <= CACHE_PIXEL_BYTES {
                    break;
                }
                common::perf_trace::count(common::perf_trace::Count::ArtAdmissionRejected, 1);
                common::perf_trace::count(common::perf_trace::Count::ArtAdmissionRejectedBytes, bytes as u64);
                return false;
            };
            let old = self.cache.remove(index).unwrap();
            self.retire(old);
        }
        self.cache.push_front(item);
        art_cache_changed();
        self.source_bytes();
        true
    }

    fn source_budget(&self) -> usize {
        let other_extra = EXTRA_SOURCE_BYTES.with(|accounts| {
            accounts
                .borrow()
                .iter()
                .filter_map(std::rc::Weak::upgrade)
                .filter(|a| !std::rc::Rc::ptr_eq(a, &self.source_account))
                .map(|a| a.get())
                .sum::<usize>()
        });
        BASE_CACHE_BYTES + (CACHE_PIXEL_BYTES - BASE_CACHE_BYTES).saturating_sub(other_extra)
    }

    fn image(&self, entry: &Entry, kind: &str) -> Option<&std::sync::Arc<Pixmap>> {
        let path = art_path(entry, kind);
        self.cached_variant(&path, if self.ink_art && kind == "heroes" { self.targets.hero } else { None })
    }

    #[cfg(test)]
    pub(crate) fn proof_long_title(
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
        path: PathBuf,
    ) {
        let mut game = crate::entry::game::Game::new(path);
        game.name =
            "Ridge Racer Type 4 Anniversary Championship with a very long title".to_string();
        let title = font(styles, 32, text_color());
        let lines = wrapped(&title, &game.name, 340, 3);
        assert!((2..=3).contains(&lines.len()));
        assert!(lines.iter().all(|line| title.measure(line).w <= 340));
        let entries = [Entry::Game(game)];
        let mut orbit = Self::new();
        orbit.prepare(&entries, 0);
        orbit.draw(&entries, 0, display, styles);
    }

    fn refresh_art(&mut self) {
        let media_generation = common::media::generation();
        if self.media_generation != media_generation {
            self.release_art();
            self.media_generation = media_generation;
        }
        let generation = art_generation();
        if self.generation != generation {
            self.release_art();
            self.generation = generation;
        }
    }

    pub(crate) fn protect_selected(&mut self, entry: &Entry) {
        let proof = (PathRevision::current(), self.tall_art, matches!(entry, Entry::Directory(_)));
        if self.selected_path.as_deref() == Some(entry.path()) && self.selected_protection == Some(proof) { return; }
        #[cfg(test)]
        PROTECTION_BUILDS.with(|n| n.set(n.get() + 1));
        if self.selected_path.as_deref() != Some(entry.path()) {
            self.cancel_selection();
            self.selected_path = Some(entry.path().into());
        }
        self.protected_paths.clear();
        self.protected_paths.extend([
            self.cover_path(entry),
            self.with_art_path(entry, "heroes", Path::to_path_buf),
            self.with_art_path(entry, "logos", Path::to_path_buf),
        ]);
        self.selected_protection = Some(proof);
    }

    pub(crate) fn protect_window(
        &mut self,
        entries: &[Entry],
        selected: usize,
        first: usize,
        count: usize,
        rows: bool,
        next: bool,
    ) {
        if let Some(entry) = entries.get(selected) {
            self.protect_selected(entry);
        }
        self.selected_protection = None;
        self.visible_cover_entries.clear();
        if rows {
            for entry in entries.iter().skip(first).take(count) {
                self.protected_paths.push(self.cover_path(entry));
                self.visible_cover_entries.push(entry.path().into());
            }
        }
        if next {
            if let Some(index) = peek_game(entries, first, count) {
                self.protected_paths.push(self.cover_path(&entries[index]));
                self.visible_cover_entries.push(entries[index].path().into());
            }
        }
    }

    pub(crate) fn cancel_selection(&mut self) {
        if self.completed_tasks.iter().any(|task| task.admission_rejected) { art_cache_changed(); }
        self.completed_tasks.retain(|task| !task.admission_rejected);
        self.worker_epoch = self.worker_epoch.wrapping_add(1);
        self.cancellation
            .store(self.worker_epoch, std::sync::atomic::Ordering::Release);
        self.pending_tasks.clear();
    }

    pub fn release_art(&mut self) {
        // External frame owners can keep pixels alive after this Orbit releases them.
        self.art_epoch = self.art_epoch.wrapping_add(1);
        if !self.completed_tasks.is_empty() { art_cache_changed(); }
        self.cancel_selection();
        self.completed_tasks.clear();
        self.protected_paths.clear();
        self.selected_protection = None;
        self.resolved_paths.get_mut().entries.clear();
        self.covered_art.clear();
        while let Some(item) = self.cache.pop_back() {
            self.retire(item);
        }
        self.source_bytes();
        self.legacy_ready = false;
        self.dirty = true;
    }

    #[cfg(test)]
    pub(crate) fn cache_entries(&self) -> usize {
        self.cache.len()
    }

    #[cfg(all(test, feature = "perf-bench"))]
    pub(crate) fn proof_source_cache(images: &[std::sync::Arc<Pixmap>]) -> Self {
        let mut orbit = Self::new();
        for (i, image) in images.iter().enumerate() {
            let path = PathBuf::from(format!("inactive-{i}"));
            orbit.protected_paths.push(path.clone());
            orbit.insert_cached(CachedArt { target: None,
                path,
                image: Some(std::sync::Arc::new((**image).clone())),
            });
        }
        orbit
    }

    pub fn should_draw(&self) -> bool {
        self.dirty
    }
    pub fn redraw(&mut self) {
        self.dirty = true;
    }

    pub fn draw(
        &mut self,
        entries: &[Entry],
        selected: usize,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) {
        if !self.legacy_ready || self.generation != art_generation() {
            self.prepare(entries, selected);
        }
        fill_rect(
            &mut display.pixmap_mut(),
            Rect::new(0, HEADER_HEIGHT, 640, CONTENT_HEIGHT),
            background(),
        );
        fill_rect(
            &mut display.pixmap_mut(),
            Rect::new(0, HEADER_HEIGHT, 640, 189),
            hero_background(),
        );
        let small = font(styles, 16, muted_color());
        let title = font(styles, 32, text_color());
        let caption = font(styles, 18, muted_color());
        if let Some(entry) = entries.get(selected) {
            if let Some(image) = self.image(entry, "heroes") {
                blit(display, image, Point::new(250, HEADER_HEIGHT));
            }
            draw_readability_gradient(display);
            if let Some(status) = crate::network_runtime::badge(entry.path()) {
                small.draw(&mut display.pixmap_mut(), &status, Point::new(124, 69));
            }
            small.draw(&mut display.pixmap_mut(), "LIBRARY", Point::new(24, 69));
            let lines = wrapped(&title, self.display_names.name(entry), 340, 3);
            for (index, line) in lines.iter().enumerate() {
                title.draw(
                    &mut display.pixmap_mut(),
                    line,
                    Point::new(24, 93 + index as i32 * 32),
                );
            }
            fill_rounded_rect(
                &mut display.pixmap_mut(),
                Rect::new(24, 197, 178, 37),
                14,
                Color::new(255, 255, 255),
            );
            font(styles, 20, text_color()).draw(
                &mut display.pixmap_mut(),
                "A  Select / Play",
                Point::new(36, 205),
            );
            small.draw(
                &mut display.pixmap_mut(),
                &format!("{} / {}", selected + 1, entries.len()),
                Point::new(549, 215),
            );
        } else {
            title.draw(
                &mut display.pixmap_mut(),
                "No games here",
                Point::new(29, 104),
            );
            small.draw(
                &mut display.pixmap_mut(),
                "B returns to the parent directory",
                Point::new(29, 158),
            );
        }
        small.draw(&mut display.pixmap_mut(), "Library", Point::new(22, 247));
        let first = shelf_start(selected, entries.len());
        for (index, entry) in entries.iter().enumerate().skip(first).take(SHELF_CARDS) {
            let x = 22 + (index - first) as i32 * CARD_STRIDE;
            let y = if index == selected { 270 } else { 273 };
            if index == selected {
                fill_rounded_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(x - 4, y - 4, COVER_SIZE + 8, COVER_SIZE + 8),
                    12,
                    Color::new(143, 236, 238),
                );
                fill_rounded_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(x - 2, y - 2, COVER_SIZE + 4, COVER_SIZE + 4),
                    10,
                    Color::new(45, 190, 200),
                );
            }
            fill_rounded_rect(
                &mut display.pixmap_mut(),
                Rect::new(x, y, COVER_SIZE, COVER_SIZE),
                9,
                Color::new(255, 255, 255),
            );
            if let Some(image) = self.image(entry, "covers") {
                blit(display, image, Point::new(x, y));
            } else {
                fill_rounded_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(x + 4, y + 4, COVER_SIZE - 8, COVER_SIZE - 8),
                    6,
                    Color::new(215, 225, 209),
                );
                font(styles, 22, muted_color()).draw(
                    &mut display.pixmap_mut(),
                    "...",
                    Point::new(x + 48, y + 51),
                );
            }
            if crate::network_runtime::badge(entry.path()).is_some_and(|status| status != "Cached")
            {
                // Small software-rendered cloud; artwork remains the persistent card identity.
                fill_rounded_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(x + 98, y + 101, 27, 22),
                    8,
                    Color::new(45, 190, 200),
                );
                fill_rounded_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(x + 102, y + 111, 19, 8),
                    3,
                    Color::new(255, 255, 255),
                );
                fill_rounded_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(x + 106, y + 105, 10, 13),
                    4,
                    Color::new(255, 255, 255),
                );
            }
            let label = wrapped(&caption, self.display_names.name(entry), 158, 1);
            if let Some(line) = label.first() {
                caption.draw(&mut display.pixmap_mut(), line, Point::new(x, y + 136));
            }
        }
        self.dirty = false;
    }
}

fn shelf_start(selected: usize, count: usize) -> usize {
    selected.min(count.saturating_sub(SHELF_CARDS))
}

fn art_path(entry: &Entry, kind: &str) -> PathBuf {
    let media_kind = match kind {
        "heroes" => common::media::Kind::Background,
        "logos" => common::media::Kind::Logo,
        "covers" => common::media::Kind::PreparedCover,
        _ => common::media::Kind::Cover,
    };
    if let Some(path) = common::media::cached_resolve(entry.path(), media_kind) {
        return path;
    }

    if let Some(path) = crate::network_runtime::art_path(entry.path(), kind) {
        return path;
    }
    let name = match entry {
        Entry::Directory(_) => entry.path().file_name(),
        _ => entry.path().file_stem(),
    }
    .unwrap_or_default();
    let mut filename = name.to_os_string();
    filename.push(match kind {
        "heroes" => ".hero.png",
        "stock" => ".png",
        "logos" => ".logo.png",
        _ => ".orbit.png",
    });
    entry
        .path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("Imgs")
        .join(filename)
}

const MAX_MEDIA_DIMENSION: u32 = 2048;
const MAX_MEDIA_PIXELS: u64 = 2048 * 2048;

#[cfg(test)]
thread_local! {
    static MEDIA_DECODE_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn media_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() as u64 > MAX_PNG_BYTES {
        return None;
    }
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    (width > 0
        && height > 0
        && width <= MAX_MEDIA_DIMENSION
        && height <= MAX_MEDIA_DIMENSION
        && u64::from(width) * u64::from(height) <= MAX_MEDIA_PIXELS)
        .then_some((width, height))
}

fn decode_art_bytes(bytes: &[u8], dimensions: (u32, u32)) -> Option<Pixmap> {
    let _stage = common::perf_trace::stage(common::perf_trace::Stage::Decode);
    if bytes.starts_with(b"\x89PNG") {
        return decode_png_bytes(bytes, dimensions);
    }
    media_dimensions(bytes)?;
    #[cfg(feature = "perf-bench")]
    common::perf::hit(common::perf::Op::Decode);
    common::perf_trace::hit(common::perf_trace::ArtHit::Decode);
    #[cfg(test)]
    MEDIA_DECODE_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
    let image = image::load_from_memory(bytes).ok()?;
    let rgba = image.to_rgba8();
    let mut pixels = Pixmap::new(rgba.width(), rgba.height())?;
    for (src, dst) in rgba.pixels().zip(pixels.pixels_mut()) {
        *dst = tiny_skia::ColorU8::from_rgba(src[0], src[1], src[2], src[3]).premultiply();
    }
    Some(pixels)
}

#[cfg(test)]
fn load_ink_cover(path: &Path) -> Option<Pixmap> {
    load_device_art(path, 480, 480)
}
#[cfg(test)]
fn load_device_art(path: &Path, width: u32, height: u32) -> Option<Pixmap> {
    load_device_art_fitted(path, width, height, None, false, false)
}
fn fit_art(image: Pixmap, target: Option<ArtTarget>) -> Option<Pixmap> {
    if let Some(target) = target {
        common::theme::runtime::fit_image(&image, target.width, target.height, target.fit).ok()
    } else { Some(image) }
}
fn load_device_art_fitted(path: &Path, width: u32, height: u32, target: Option<ArtTarget>, logo: bool, portrait: bool) -> Option<Pixmap> {
    let cover_logo = !logo && path.file_name().is_some_and(|name| name == "logo.png");
    let portrait = portrait && !cover_logo;
    let target = target.map(|mut target| {
        if cover_logo { target.fit = common::theme::pack::Fit::Contain; }
        target
    });
    if portrait && target.is_none() {
        return load_device_art_fitted(path, width, height, None, logo, false).and_then(portrait::compose);
    }
    let _phase = common::perf_trace::phase(common::perf_trace::Phase::ArtLoad);
    common::perf_trace::frontend_io_op();
    let file = File::open(path).ok()?;
    common::perf_trace::frontend_io_op();
    let metadata = file.metadata().ok()?;
    if metadata.len() > MAX_PNG_BYTES {
        return None;
    }
    common::perf_trace::frontend_io_op();
    let bytes = read_png_bytes(file)?;
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if w == 0 || h == 0 || w > width || h > height {
        return None;
    }
    let target = target.map(|target| if logo { target.logo(w, h) } else { target })
        .or_else(|| cover_logo.then_some(ArtTarget {
            width: w.max(h), height: w.max(h), fit: common::theme::pack::Fit::Contain, logo_box: false,
        }));
    let dimensions = target.map_or((w, h), |target| (target.width, target.height));
    let key = crate::art_cache::ArtKey::with_fit(path, &metadata, &bytes,
        if portrait { (1, 1) } else { (0, 0) }, dimensions, false, target.map(|target| target.fit));
    let cache_dir = common::constants::ALLIUM_ORBIT_ART_CACHE_DIR.as_path();
    if let Some(image) = key.as_ref().and_then(|key| key.load(cache_dir)) {
        if target.is_some() { common::perf_trace::count(common::perf_trace::Count::ArtFittedHit, 1); }
        common::perf_trace::hit(common::perf_trace::ArtHit::Disk);
        #[cfg(feature = "perf-bench")]
        common::perf::hit(common::perf::Op::DiskHit);
        return Some(image);
    }
    if !art_job_current() {
        return None;
    }
    if target.is_some() { common::perf_trace::count(common::perf_trace::Count::ArtFittedMiss, 1); }
    let image = decode_art_bytes(&bytes, (0, 0))?;
    let image = if portrait { portrait::compose(image)? } else { image };
    let image = fit_art(image, target)?;
    if art_job_current() {
        if let Some(key) = key {
            key.store(cache_dir, &image);
        }
    }
    Some(image)
}

fn load_prepared_art(
    path: &Path,
    dimensions: (u32, u32),
    width: u32,
    height: u32,
    crop: bool,
) -> Option<Pixmap> {
    #[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
    crate::allium_launcher::navigation_warm_tests::art_load();
    let _phase = common::perf_trace::phase(common::perf_trace::Phase::ArtLoad);
    common::perf_trace::frontend_io_op();
    let file = File::open(path).ok()?;
    common::perf_trace::frontend_io_op();
    let metadata = file.metadata().ok();
    if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.len() > MAX_PNG_BYTES)
    {
        return None;
    }
    common::perf_trace::frontend_io_op();
    let bytes = read_png_bytes(file)?;
    let (sw, sh) = media_dimensions(&bytes)?;
    let (width, height) = if crop {
        (width, height)
    } else {
        let scale = (width as f32 / sw as f32).min(height as f32 / sh as f32);
        (
            (sw as f32 * scale).round().max(1.0) as u32,
            (sh as f32 * scale).round().max(1.0) as u32,
        )
    };
    let key = {
        let _disk_stage = common::perf_trace::stage(common::perf_trace::Stage::DiskCache);
        metadata.as_ref().and_then(|metadata| {
            crate::art_cache::ArtKey::new(path, metadata, &bytes, dimensions, (width, height), crop)
        })
    };
    let cache_dir = common::constants::ALLIUM_ORBIT_ART_CACHE_DIR.as_path();
    let disk_image = {
        let _disk_stage = common::perf_trace::stage(common::perf_trace::Stage::DiskCache);
        key.as_ref().and_then(|key| key.load(cache_dir))
    };
    if let Some(image) = disk_image {
        common::perf_trace::hit(common::perf_trace::ArtHit::Disk);
        #[cfg(feature = "perf-bench")]
        common::perf::hit(common::perf::Op::DiskHit);
        return Some(image);
    }
    if !art_job_current() {
        return None;
    }
    let image = resize_art(decode_art_bytes(&bytes, dimensions)?, width, height, crop)?;
    if let Some(key) = key.filter(|_| art_job_current()) {
        let _disk_stage = common::perf_trace::stage(common::perf_trace::Stage::DiskCache);
        key.store(cache_dir, &image);
    }
    Some(image)
}

#[cfg(test)]
fn load_final_png(path: &Path, dimensions: (u32, u32)) -> Option<Pixmap> {
    common::perf_trace::frontend_io_op();
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_PNG_BYTES {
        return None;
    }
    decode_final_png(file, dimensions)
}

#[cfg(test)]
fn decode_final_png(file: File, dimensions: (u32, u32)) -> Option<Pixmap> {
    decode_png_bytes(&read_png_bytes(file)?, dimensions)
}

fn read_png_bytes(file: File) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    #[cfg(feature = "perf-bench")]
    common::perf::hit(common::perf::Op::Read);
    file.take(MAX_PNG_BYTES + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= MAX_PNG_BYTES).then_some(bytes)
}

fn decode_png_bytes(bytes: &[u8], dimensions: (u32, u32)) -> Option<Pixmap> {
    if bytes.len() < 24 || bytes.len() as u64 > MAX_PNG_BYTES || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
    {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    if dimensions == (0, 0) {
        if width < 1
            || height < 1
            || width > 1024
            || height > 1024
            || width.checked_mul(height)? > 1024 * 1024
        {
            return None;
        }
    } else if (width, height) != dimensions {
        return None;
    }
    if width > MAX_MEDIA_DIMENSION
        || height > MAX_MEDIA_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_MEDIA_PIXELS
    {
        return None;
    }
    common::perf_trace::hit(common::perf_trace::ArtHit::Decode);
    #[cfg(test)]
    MEDIA_DECODE_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
    let image = Pixmap::decode_png(&bytes).ok()?;
    #[cfg(feature = "perf-bench")]
    common::perf::hit(common::perf::Op::Decode);
    ((image.width(), image.height()) == (width, height)).then_some(image)
}
fn resize_art(image: Pixmap, width: u32, height: u32, crop: bool) -> Option<Pixmap> {
    #[cfg(feature = "perf-bench")]
    common::perf::hit(common::perf::Op::ArtResize);
    let pixels = image::RgbaImage::from_raw(image.width(), image.height(), image.data().to_vec())?;
    let scale = if crop {
        (width as f32 / image.width() as f32).max(height as f32 / image.height() as f32)
    } else {
        (width as f32 / image.width() as f32).min(height as f32 / image.height() as f32)
    };
    let resized_width = (image.width() as f32 * scale).round().max(1.0) as u32;
    let resized_height = (image.height() as f32 * scale).round().max(1.0) as u32;
    if u64::from(resized_width) * u64::from(resized_height) > MAX_MEDIA_PIXELS {
        return None;
    }
    let resized = image::imageops::resize(
        &pixels,
        resized_width,
        resized_height,
        image::imageops::FilterType::Triangle,
    );
    let final_image = if crop {
        image::imageops::crop_imm(
            &resized,
            (resized_width - width) / 2,
            (resized_height - height) / 2,
            width,
            height,
        )
        .to_image()
    } else {
        resized
    };
    let size = tiny_skia::IntSize::from_wh(final_image.width(), final_image.height())?;
    Pixmap::from_vec(final_image.into_raw(), size)
}

pub fn font(styles: &Stylesheet, size: u32, color: Color) -> FontTextStyle {
    FontTextStyleBuilder::new(styles.ui.ui_font.font())
        .font_fallback(styles.cjk_font.font())
        .font_size(size)
        .text_color(color)
        .build()
}

pub(crate) fn wrapped(
    style: &FontTextStyle,
    text: &str,
    width: u32,
    max_lines: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let next = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if style.measure(&next).w > width && !line.is_empty() {
            lines.push(line);
            line = word.to_string();
            if lines.len() == max_lines {
                break;
            }
        } else {
            line = next;
        }
    }
    if lines.len() < max_lines && !line.is_empty() {
        lines.push(line);
    }
    for line in &mut lines {
        while style.measure(line).w > width {
            line.pop();
            if line.is_empty() {
                break;
            }
        }
    }
    lines
}

fn blit(display: &mut <DefaultPlatform as Platform>::Display, image: &Pixmap, point: Point) {
    display.pixmap_mut().draw_pixmap(
        point.x,
        point.y,
        image.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
}

fn draw_readability_gradient(display: &mut <DefaultPlatform as Platform>::Display) {
    for x in 250..480 {
        let alpha = if x < 307 {
            245
        } else {
            ((480 - x) * 245 / 173) as u8
        };
        fill_rect(
            &mut display.pixmap_mut(),
            Rect::new(x, HEADER_HEIGHT, 1, 189),
            hero_background().with_a(alpha),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_directory_art_uses_full_basename_but_games_keep_stems() {
        use crate::entry::{directory::Directory, game::Game};
        let directory = Entry::Directory(Directory::new(PathBuf::from(
            "Roms/PS/Mega Man Legends 2 PSP Improvements (v1.1)",
        )));
        assert_eq!(
            art_path(&directory, "heroes"),
            PathBuf::from("Roms/PS/Imgs/Mega Man Legends 2 PSP Improvements (v1.1).hero.png")
        );
        assert_eq!(
            art_path(&directory, "covers"),
            PathBuf::from("Roms/PS/Imgs/Mega Man Legends 2 PSP Improvements (v1.1).orbit.png")
        );
        let playlist = Entry::Game(Game::new(PathBuf::from(
            "Roms/PS/Legend of Dragoon, The (USA).m3u",
        )));
        assert_eq!(
            art_path(&playlist, "heroes"),
            PathBuf::from("Roms/PS/Imgs/Legend of Dragoon, The (USA).hero.png")
        );
        let cue = Entry::Game(Game::new(PathBuf::from(
            "Roms/PS/Mega Man Legends 2 PSP Improvements (v1.1)/Mega Man Legends 2 (USA).cue",
        )));
        assert_eq!(
            art_path(&cue, "covers"),
            PathBuf::from(
                "Roms/PS/Mega Man Legends 2 PSP Improvements (v1.1)/Imgs/Mega Man Legends 2 (USA).orbit.png"
            )
        );
    }

    #[test]
    fn shelf_keeps_selection_inside_three_cards() {
        for count in 0..100 {
            for selected in 0..count {
                let first = shelf_start(selected, count);
                assert!(first <= selected && selected < first + SHELF_CARDS);
            }
        }
        assert_eq!(shelf_start(0, 0), 0);
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn native_orbit_readability_art_and_four_disc_frames() {
        use crate::entry::{directory::Directory, game::Game};
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        let mut display = DefaultPlatform::new().unwrap().display().unwrap();
        let game_path =
            common::constants::ALLIUM_GAMES_DIR.join("PS/R4 - Ridge Racer Type 4 (USA).chd");
        let evidence = PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap());
        std::fs::create_dir_all(&evidence).unwrap();
        let entries = [
            Entry::Game(Game::new(game_path)),
            Entry::Directory(Directory::new(
                common::constants::ALLIUM_GAMES_DIR
                    .join("PS/Mega Man Legends 2 PSP Improvements (v1.1)"),
            )),
        ];
        let mut orbit = Orbit::new();
        orbit.prepare(&entries, 0);
        let cover = orbit
            .image(&entries[0], "covers")
            .expect("existing owned real-art fixture");
        assert_eq!((cover.width(), cover.height()), (132, 132));
        let hero = orbit.image(&entries[0], "heroes").unwrap();
        assert_eq!((hero.width(), hero.height()), (390, 189));
        assert!(273 + COVER_SIZE as i32 + 4 + 18 <= FOOTER_Y);
        let caption = font(&styles, 18, text_color());
        assert!(
            wrapped(&caption, entries[1].name(), 158, 1)
                .iter()
                .all(|line| caption.measure(line).w <= 158)
        );
        orbit.draw(&entries, 1, &mut display, &styles);
        display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-readable-dotted-title.png"))
            .unwrap();
        let mut picker = super::super::disc_picker::DiscPicker::new(crate::discs::Playlist {
            path: PathBuf::from("Legend of Dragoon, The (USA).m3u"),
            discs: (1..=4)
                .map(|i| crate::discs::Disc {
                    path: PathBuf::from(format!("Legend of Dragoon, The (USA) (Disc {i}).chd")),
                    available: true,
                })
                .collect(),
            core_compatible: true,
        });
        picker.select(true);
        picker.draw(&mut display, &styles);
        display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-readable-four-discs.png"))
            .unwrap();
        assert!(!picker.should_draw());
        for _ in 0..4 {
            picker.playlist.discs.push(crate::discs::Disc {
                path: PathBuf::from("Extra disc.chd"),
                available: true,
            });
        }
        picker.selected = 7;
        picker.draw(&mut display, &styles);
        display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-readable-eight-disc-window.png"))
            .unwrap();
    }

    #[test]
    fn incorrect_dimensions_and_invalid_png_are_rejected_before_decode() {
        let path = std::env::temp_dir().join(format!("allium-orbit-{}.png", std::process::id()));
        let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
        data.resize(24, 0);
        data[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        std::fs::write(&path, data).unwrap();
        assert!(load_final_png(&path, (101, 101)).is_none());
        std::fs::remove_file(path).unwrap();
    }
}

pub(super) fn peek_game(entries: &[Entry], first: usize, count: usize) -> Option<usize> {
    if first >= entries.len() || count == 0 {
        return None;
    }
    let next = first.checked_add(count)?;
    let next = if next >= entries.len() { 0 } else { next };
    matches!(entries.get(next), Some(Entry::Game(_))).then_some(next)
}

#[cfg(test)]
mod next_cover_tests {
    use super::*;
    #[test]
    #[cfg(feature = "perf-bench")]
    fn review_frame_preparation_never_decodes_cold_selection() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../common/tests/fixtures/media-sd/Roms/TEST");
        let entries = [Entry::Game(crate::entry::game::Game::new(
            root.join("Complete.sfc"),
        ))];
        let mut orbit = Orbit::new();
        common::perf::take();
        orbit.prepare_window(&entries, 0, 0, 1);
        let counts = common::perf::take();
        assert_eq!(
            counts.read_attempts + counts.decodes + counts.media_io,
            0,
            "frame performed IO/decode: {counts:?}"
        );
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn captured_case_art_prefers_clean_original_and_keeps_prepared_fallback() {
        use crate::entry::game::Game;
        use common::theme::{
            ThemeRuntime,
            frame::{FrameInputs, Row},
            pack::{Binding, ScreenId},
            render::PreparedFrame,
        };
        let captured = PathBuf::from(std::env::var("ALLIUM_ACTUAL_CASE_ART_DIR").unwrap());
        let proof = PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap());
        std::fs::create_dir_all(&proof).unwrap();
        let root = PathBuf::from("/private/tmp")
            .join(format!("allium-actual-case-art-{}", std::process::id()));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        std::fs::create_dir_all(root.join(".allium/state")).unwrap();
        std::fs::create_dir_all(root.join(".allium/fonts")).unwrap();
        std::fs::create_dir_all(root.join("Themes")).unwrap();
        std::fs::create_dir_all(root.join("Roms/PS/Imgs")).unwrap();
        let _cleanup = Cleanup(root.clone());
        for font in ["Nunito.ttf", "NotoSansCJK.otf"] {
            std::fs::copy(
                common::constants::ALLIUM_FONTS_DIR.join(font),
                root.join(".allium/fonts").join(font),
            )
            .unwrap();
        }
        let game_path = root.join("Roms/PS/Mega Man Legends 2 PSP Improvements (v1.1).chd");
        let entry = Entry::Game(Game::new(game_path));
        let original = art_path(&entry, "stock");
        let prepared = art_path(&entry, "covers");
        std::fs::copy(captured.join("megaman-original.png"), &original).unwrap();
        std::fs::copy(captured.join("megaman-orbit.png"), &prepared).unwrap();
        let mut runtime = ThemeRuntime::bind(
            &root.join(".allium/state"),
            &root.join("Themes"),
            &root.join(".allium/fonts"),
        )
        .unwrap();
        let handle = runtime.choose("orbit").unwrap().snapshot.unwrap();
        let entries = [entry];
        let mut orbit = Orbit::new();
        orbit.prepare_loaded_window(&entries, 0, 0, 1);
        let art = orbit.theme_art(&entries, 0, 0, 1);
        let clean = art.selected_cover.as_ref().unwrap().clone();
        let expected = resize_art(
            load_final_png(&original, (0, 0)).unwrap(),
            THEME_COVER_SIZE,
            THEME_COVER_SIZE,
            false,
        )
        .unwrap();
        assert_eq!((clean.width(), clean.height()), (140, 140));
        assert_eq!(
            clean.data(),
            expected.data(),
            "themed case uses actual clean original"
        );
        let again = orbit.theme_art(&entries, 0, 0, 1);
        assert!(std::sync::Arc::ptr_eq(
            &clean,
            again.selected_cover.as_ref().unwrap()
        ));
        assert!(orbit.cache_entries() <= CACHE_ENTRIES);
        let mut input = FrameInputs::new(ScreenId::Library);
        input.rows.insert(
            Binding::Entries,
            vec![Row {
                subtitle: String::new(),
                id: crate::theme_adapter::entry_id(entries[0].path()),
                label: entries[0].name().into(),
                value: String::new(),
                enabled: true,
            }],
        );
        input.art = art;
        PreparedFrame::prepare(handle.clone(), &input)
            .unwrap()
            .pixels()
            .save_png(proof.join("actual-megaman-clean-rounded-case.png"))
            .unwrap();

        std::fs::remove_file(&original).unwrap();
        orbit.release_art();
        orbit.prepare_loaded_window(&entries, 0, 0, 1);
        let fallback = orbit.theme_art(&entries, 0, 0, 1);
        assert_eq!(
            (
                fallback.selected_cover.as_ref().unwrap().width(),
                fallback.selected_cover.as_ref().unwrap().height()
            ),
            (COVER_SIZE, COVER_SIZE)
        );
        input.art = fallback;
        PreparedFrame::prepare(handle, &input)
            .unwrap()
            .pixels()
            .save_png(proof.join("actual-megaman-prepared-fallback-case.png"))
            .unwrap();

        let window = (0..9)
            .map(|i| Entry::Game(Game::new(root.join(format!("Roms/PS/Window {i}.chd")))))
            .collect::<Vec<_>>();
        for entry in &window {
            std::fs::copy(&prepared, art_path(entry, "covers")).unwrap();
        }
        orbit.release_art();
        orbit.prepare_loaded_window(&window, 7, 0, 8);
        let full = orbit.theme_art(&window, 7, 0, 8);
        assert_eq!(
            full.entries.len(),
            8,
            "all eight visible prepared fallbacks survive"
        );
        assert!(full.next_cover.is_some(), "ninth game's preview survives");
        assert_eq!(
            orbit.cache_entries(),
            10,
            "nine logical covers and one hero miss"
        );
        for entry in window.iter().take(8) {
            assert!(
                full.entries
                    .contains_key(&crate::theme_adapter::entry_id(entry.path()))
            );
        }
        let repeated = orbit.theme_art(&window, 7, 0, 8);
        assert!(std::sync::Arc::ptr_eq(
            full.next_cover.as_ref().unwrap(),
            repeated.next_cover.as_ref().unwrap()
        ));
    }
    #[test]
    #[cfg(debug_assertions)]
    #[serial_test::serial(orbit_fixture)]
    fn actual_theme_art_identity_reuses_prepared_preview_and_invalidates() {
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
        let entries = [
            Entry::Game(crate::entry::game::Game::new(
                root.join("Roms/PS/R4 - Ridge Racer Type 4 (USA).chd"),
            )),
            Entry::Game(crate::entry::game::Game::new(
                root.join("Roms/PS/Silent Hill (USA).chd"),
            )),
            Entry::Game(crate::entry::game::Game::new(
                root.join("Roms/PS/Preview Fixture.chd"),
            )),
        ];
        let extra = root.join("Roms/PS/Imgs/Preview Fixture.orbit.png");
        std::fs::copy(
            root.join("Roms/PS/Imgs/R4 - Ridge Racer Type 4 (USA).orbit.png"),
            &extra,
        )
        .unwrap();
        let mut orbit = Orbit::new();
        orbit.prepare_loaded_window(&entries, 0, 0, 1);
        let first = orbit.theme_art(&entries, 0, 0, 1);
        let original_cover = first.selected_cover.as_ref().unwrap().clone();
        let second = orbit.theme_art(&entries, 0, 0, 1);
        assert!(std::sync::Arc::ptr_eq(
            first.next_cover.as_ref().unwrap(),
            second.next_cover.as_ref().unwrap()
        ));
        assert!(std::sync::Arc::ptr_eq(
            first.selected_cover.as_ref().unwrap(),
            second.selected_cover.as_ref().unwrap()
        ));
        let mut input = FrameInputs::new(ScreenId::Library);
        input.rows.insert(Binding::Entries, vec![]);
        input.art = first;
        PreparedFrame::prepare(handle.clone(), &input).unwrap();
        let cached = handle.debug_prepared_preview_identity().unwrap();
        input.art = second;
        PreparedFrame::prepare(handle.clone(), &input).unwrap();
        assert_eq!(handle.debug_prepared_preview_identity(), Some(cached));
        orbit.prepare_loaded_window(&entries, 1, 1, 1);
        let changed = orbit.theme_art(&entries, 1, 1, 1);
        assert!(!std::sync::Arc::ptr_eq(
            input.art.next_cover.as_ref().unwrap(),
            changed.next_cover.as_ref().unwrap()
        ));
        input.art = changed;
        PreparedFrame::prepare(handle.clone(), &input).unwrap();
        assert_ne!(handle.debug_prepared_preview_identity(), Some(cached));
        input.art = orbit.theme_art(&entries, 2, 2, 1);
        assert!(input.art.next_cover.is_some());
        PreparedFrame::prepare(handle.clone(), &input).unwrap();
        assert!(handle.debug_prepared_preview_identity().is_some());
        input.art.next_cover = None;
        input.art.next_cover_expected = false;
        PreparedFrame::prepare(handle.clone(), &input).unwrap();
        assert!(handle.debug_prepared_preview_identity().is_none());
        orbit.release_art();
        orbit.prepare_loaded_window(&entries, 0, 0, 1);
        let reloaded = orbit.theme_art(&entries, 0, 0, 1);
        assert!(!std::sync::Arc::ptr_eq(
            &original_cover,
            reloaded.selected_cover.as_ref().unwrap()
        ));
        std::fs::remove_file(extra).unwrap();
    }
    #[test]
    fn next_cover_wraps_and_is_requested_and_protected_at_the_last_window() {
        let entries = vec![
            Entry::Game(crate::entry::game::Game::new("First.chd".into())),
            Entry::Game(crate::entry::game::Game::new("Second.chd".into())),
        ];
        assert_eq!(peek_game(&entries, 0, 1), Some(1));
        assert_eq!(peek_game(&entries, 0, 2), Some(0));
        assert_eq!(peek_game(&entries, usize::MAX, 1), None);
        assert_eq!(peek_game(&entries, 1, 1), Some(0));
        assert_eq!(peek_game(&entries, 1, 8), Some(0));
        assert_eq!(peek_game(&entries, 0, 0), None);
        assert_eq!(peek_game(&entries, 1, usize::MAX), None);
        assert_eq!(peek_game(&entries[..1], 0, 1), Some(0));
        assert_eq!(peek_game(&[], 0, 1), None);
        let work = super::super::art_queue::plan(&entries, 1, 1, 1, 1,
            super::super::art_queue::Demand { next: true, ..Default::default() }, false);
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].entry().path(), entries[0].path());
        let mut orbit = Orbit::new();
        let path = orbit.cover_path(&entries[0]);
        let image = std::sync::Arc::new(Pixmap::new(10, 10).unwrap());
        orbit.insert_cached(CachedArt { path: path.clone(), target: None, image: Some(image.clone()) });
        let art = orbit.theme_art(&entries, 1, 1, 1);
        assert!(art.next_cover_expected);
        assert!(std::sync::Arc::ptr_eq(art.next_cover.as_ref().unwrap(), &image));
        orbit.protect_window(&entries, 1, 1, 1, false, true);
        assert!(orbit.protected_paths.contains(&path));
        assert!(orbit.visible_cover_entries.contains(&entries[0].path().to_owned()));
        let mixed = [Entry::Directory(crate::entry::directory::Directory::new("Folder".into())), entries[1].clone()];
        assert_eq!(peek_game(&mixed, 1, 1), None);
    }
}

#[cfg(test)]
pub(super) mod refresh_tests {
    use super::*;
    use crate::entry::game::Game;

    pub(in crate::view) fn expect_selection_protection(orbit: &mut Orbit, previous: &Entry,
        selected: &Entry,
    ) -> impl FnOnce(&Orbit) + use<> {
        assert_ne!(previous.path(), selected.path());
        orbit.protect_selected(previous);
        orbit.pending_tasks.push_back(WarmArt::Cover(previous.clone()));
        let epoch = orbit.worker_epoch;
        assert_eq!(orbit.cancellation.load(std::sync::atomic::Ordering::Acquire), epoch);
        let protected = vec![orbit.cover_path(selected), art_path(selected, "heroes"), art_path(selected, "logos")];
        let selected = selected.path().to_owned();
        move |orbit| {
            assert_eq!(orbit.selected_path.as_deref(), Some(selected.as_path()));
            assert_eq!(orbit.protected_paths, protected);
            assert_eq!(orbit.worker_epoch, epoch.wrapping_add(1), "selection must cancel old work");
            assert_eq!(orbit.cancellation.load(std::sync::atomic::Ordering::Acquire), orbit.worker_epoch);
            assert!(orbit.pending_tasks.is_empty(), "selection must discard queued old work");
        }
    }

    fn write_art(path: &Path, width: u32, height: u32, red: u8) {
        let mut pixels = Pixmap::new(width, height).unwrap();
        pixels.fill(tiny_skia::Color::from_rgba8(red, 0, 0, 255));
        pixels.save_png(path).unwrap();
    }
    fn uncached_art(orbit: &Orbit, entries: &[Entry], selected: usize, first: usize, count: usize) -> common::theme::frame::Art {
        let cover = |entry: &Entry| {
            if orbit.tall_art {
                if let Some(path) = common::media::cached_resolve(entry.path(), common::media::Kind::TallCover) { return path; }
            }
            art_path(entry, "stock")
        };
        let mut art = common::theme::frame::Art::default();
        if let Some(entry) = entries.get(selected) {
            art.selected_logo = orbit.cached_variant(&art_path(entry, "logos"), orbit.targets.logo).cloned();
            art.selected_hero = orbit.cached_variant(&art_path(entry, "heroes"), if orbit.ink_art { orbit.targets.hero } else { None }).cloned();
            art.selected_cover = orbit.cached_variant(&cover(entry), orbit.targets.cover).cloned();
        }
        if let Some(next) = peek_game(entries, first, count) {
            art.next_cover_expected = true;
            art.next_cover = orbit.cached_variant(&cover(&entries[next]), orbit.targets.cover).cloned();
        }
        for entry in entries.iter().skip(first).take(count) {
            if let Some(image) = orbit.cached_variant(&cover(entry), orbit.targets.cover) {
                art.entries.insert(crate::theme_adapter::entry_id(entry.path()), image.clone());
            }
        }
        art
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn warmed_artwork_matches_original_sources_and_ids() {
        common::media::invalidate();
        let entries: Vec<_> = (0..4).map(|i| Entry::Game(Game::new(format!("/artrefresh/Compare{i}.chd").into()))).collect();
        let mut orbit = Orbit::new();
        for entry in &entries {
            for kind in ["stock", "logos", "heroes"] {
                assert!(orbit.insert_cached(CachedArt { target: None, path: art_path(entry, kind),
                    image: Some(std::sync::Arc::new(Pixmap::new(1, 1).unwrap())) }));
            }
        }
        for selected in [0, 1, 2, 3, 0] {
            for first in 0..4 {
                let expected = uncached_art(&orbit, &entries, selected, first, 2);
                let actual = orbit.theme_art(&entries, selected, first, 2);
                assert!(actual == expected, "selection {selected}, first {first}");
                assert_eq!(actual.entries.len(), (4 - first).min(2));
                for (id, pixels) in &actual.entries {
                    assert_eq!(pixels.data(), expected.entries[id].data());
                }
            }
        }
        let game = Entry::Game(Game::new("/artrefresh/Container.ext".into()));
        let directory = Entry::Directory(crate::entry::directory::Directory {
            path: game.path().into(), ..Default::default()
        });
        orbit.protect_selected(&game);
        orbit.protect_selected(&directory);
        assert_eq!(orbit.protected_paths[0], art_path(&directory, "stock"));
        assert_ne!(art_path(&game, "stock"), orbit.protected_paths[0]);
        common::media::invalidate();
    }

    #[test]
    #[ignore = "explicit artwork refresh host microbenchmark"]
    #[serial_test::serial(orbit_fixture)]
    fn artrefresh_warmed_artwork_bench() {
        common::media::invalidate();
        let entries: Vec<_> = (0..12).map(|i| Entry::Game(Game::new(format!("/artrefresh/Bench{i}.chd").into()))).collect();
        let mut orbit = Orbit::new();
        for entry in &entries {
            assert!(orbit.insert_cached(CachedArt { target: None, path: art_path(entry, "stock"),
                image: Some(std::sync::Arc::new(Pixmap::new(1, 1).unwrap())) }));
        }
        orbit.theme_art(&entries, 0, 0, 6);
        orbit.theme_art(&entries, 1, 0, 6);
        let mut times = [Vec::new(), Vec::new()];
        for i in 0..1001 {
            let start = std::time::Instant::now();
            std::hint::black_box(uncached_art(&orbit, &entries, i % 2, 0, 6));
            times[0].push(start.elapsed().as_nanos());
            let start = std::time::Instant::now();
            std::hint::black_box(orbit.theme_art(&entries, i % 2, 0, 6));
            times[1].push(start.elapsed().as_nanos());
        }
        for values in &mut times { values.sort_unstable(); }
        println!("ARTREFRESH host_debug_samples=1001 uncached_median_ns={} cached_median_ns={}", times[0][500], times[1][500]);
        common::media::invalidate();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn warmed_paths_and_protection_skip_builds_and_follow_all_revisions() {
        let entry = Entry::Game(Game::new("/artrefresh/Game.chd".into()));
        let entries = [entry.clone()];
        common::media::invalidate();
        let mut orbit = Orbit::new();
        orbit.set_cover_shape(true);
        common::media::adopt_fallback_paths(entry.path(), [(common::media::Kind::TallCover, "tall.png".into())]);
        orbit.protect_selected(&entry);
        orbit.theme_art(&entries, 0, 0, 1);
        let counts = || (RESOLVED_PATH_BUILDS.with(std::cell::Cell::get), PROTECTION_BUILDS.with(std::cell::Cell::get));
        let warmed = counts();
        for _ in 0..10 {
            orbit.protect_selected(&entry);
            orbit.theme_art(&entries, 0, 0, 1);
        }
        assert_eq!(counts(), warmed, "hit path must build neither paths nor protection");
        assert_eq!(orbit.cover_path(&entry), Path::new("tall.png"));
        // No metadata/model generation changes during FIFO eviction.
        let media = common::media::generation();
        for i in 0..512 {
            common::media::adopt_fallback_paths(Path::new(&format!("/artrefresh/Other{i}.chd")),
                [(common::media::Kind::Cover, "other.png".into())]);
        }
        assert_eq!(common::media::generation(), media);
        let fallback = art_path(&entry, "stock");
        assert_eq!(orbit.cover_path(&entry), fallback);
        orbit.protect_selected(&entry);
        assert_eq!(orbit.protected_paths[0], fallback);
        common::media::adopt_fallback_paths(entry.path(), [(common::media::Kind::TallCover, "new-tall.png".into())]);
        assert_eq!(orbit.cover_path(&entry), Path::new("new-tall.png"));
        common::media::clear_prepared_paths();
        assert_eq!(orbit.cover_path(&entry), fallback);
        let before = counts();
        common::media::invalidate();
        orbit.protect_selected(&entry);
        assert!(counts().0 > before.0 && counts().1 > before.1);
        orbit.protect_window(&entries, 0, 0, 1, true, false);
        assert_eq!(orbit.protected_paths.len(), 4);
        let before = counts().1;
        orbit.protect_selected(&entry);
        assert_eq!(counts().1, before + 1, "window protection must be replaced");
        assert_eq!(orbit.protected_paths.len(), 3);
        orbit.set_cover_shape(false);
        orbit.protect_selected(&entry);
        assert_eq!(orbit.selected_protection.unwrap().1, false);
        let before = counts();
        orbit.release_art();
        orbit.protect_selected(&entry);
        assert!(counts().0 > before.0 && counts().1 > before.1);
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let first = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(), &bank, 901).unwrap();
        let changed = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/big-picture")).unwrap(), &bank, 901).unwrap();
        assert!(orbit.set_art_theme(&first));
        orbit.protect_selected(&entry);
        let before = counts();
        assert!(!orbit.set_art_theme(&first));
        orbit.protect_selected(&entry);
        assert_eq!(counts(), before);
        assert!(orbit.set_art_theme(&changed), "digest change at same generation must refresh");
        orbit.protect_selected(&entry);
        assert!(counts().0 > before.0 && counts().1 > before.1);
        let new_generation = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/big-picture")).unwrap(), &bank, 902).unwrap();
        let before = counts();
        assert!(orbit.set_art_theme(&new_generation));
        orbit.protect_selected(&entry);
        assert!(counts().0 > before.0 && counts().1 > before.1);
        let before = counts();
        crate::network_runtime::initialize(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        orbit.protect_selected(&entry);
        assert!(counts().0 > before.0 && counts().1 > before.1, "runtime replacement must invalidate fallbacks");
        for i in 0..RESOLVED_PATH_LIMIT * 2 {
            orbit.cover_path(&Entry::Game(Game::new(format!("/artrefresh/Bound{i}.chd").into())));
        }
        assert_eq!(orbit.resolved_paths.borrow().entries.len(), RESOLVED_PATH_LIMIT);
        common::media::invalidate();
    }

    #[test]
    #[cfg(feature = "perf-bench")]
    #[serial_test::serial(orbit_fixture)]
    fn art_snapshot_matches_rebuild_pixels_and_completed_misses() {
        use common::theme::{fonts::FontBank, frame::FrameInputs, pack::ScreenId,
            render::PreparedFrame, runtime::ThemeSnapshot, ThemeHandle};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(), &bank, 903).unwrap();
        let trace = std::env::temp_dir().join(format!("allium-art-snapshot-{}.jsonl", std::process::id()));
        common::perf_trace::start_probe(trace.to_str().unwrap());
        common::media::invalidate();
        let entries: Vec<_> = (0..4).map(|i| Entry::Game(Game::new(format!("/snapshot/Game{i}.chd").into()))).collect();
        let mut orbit = Orbit::new();
        orbit.set_art_theme(&theme);
        for index in [0, 3] {
            for kind in ["stock", "heroes"] {
                let mut pixels = Pixmap::new(2, 2).unwrap();
                pixels.fill(tiny_skia::Color::from_rgba8(40 + index as u8, 80, 120, 255));
                assert!(orbit.insert_cached(CachedArt { target: None, path: art_path(&entries[index], kind),
                    image: Some(std::sync::Arc::new(pixels)) }));
            }
        }
        for entry in &entries {
            for task in [WarmArt::Cover(entry.clone()), WarmArt::Hero(entry.clone()), WarmArt::Logo(entry.clone())] {
                orbit.completed_tasks.push_back(CompletedTask { targets: orbit.targets, key: task.key(),
                    admission_rejected: false, missing: true, metadata: None });
            }
        }
        let baseline = orbit.theme_art(&entries, 0, 0, 2);
        assert!(baseline.completed_misses.selected_logo && baseline.completed_misses.next_cover);
        assert!(!baseline.completed_misses.entries.is_empty());
        for (selected, first, count) in [(0, 0, 2), (3, 1, 2), (1, 0, 4)] {
            let expected = orbit.theme_art(&entries, selected, first, count);
            let snapshot = orbit.art_snapshot(&theme, &expected);
            let actual = snapshot.restore(&orbit, &theme, orbit.targets).unwrap();
            assert!(actual == expected);
            assert!(actual.completed_misses == expected.completed_misses);
            for (id, pixels) in &actual.entries { assert_eq!(pixels.data(), expected.entries[id].data()); }
            let mut input = FrameInputs::new(ScreenId::Recents);
            input.art = actual;
            let pixels = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            input.art = expected;
            assert_eq!(pixels.pixels().data(), PreparedFrame::prepare(theme.clone(), &input).unwrap().pixels().data());
        }
        let snapshot = orbit.art_snapshot(&theme, &baseline);
        common::perf_trace::stop_probe();
        assert!(snapshot.restore(&orbit, &theme, orbit.targets).is_none(), "trace mode changes completed misses");
        std::fs::remove_file(trace).unwrap();
        orbit.release_art();
        common::media::invalidate();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn art_snapshot_weak_lifetime_and_stamp_fallbacks() {
        use common::theme::{fonts::FontBank, frame::Art, runtime::ThemeSnapshot, ThemeHandle};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let load = || ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap();
        let theme = ThemeHandle::prepare(load(), &bank, 904).unwrap();
        let mut orbit = Orbit::new();
        orbit.set_art_theme(&theme);
        for field in 0..5 {
            let source = std::sync::Arc::new(Pixmap::new(1, 1).unwrap());
            let mut art = Art::default();
            match field {
                0 => art.selected_logo = Some(source.clone()),
                1 => art.selected_cover = Some(source.clone()),
                2 => art.selected_hero = Some(source.clone()),
                3 => art.next_cover = Some(source.clone()),
                _ => { art.entries.insert("row".into(), source.clone()); },
            }
            let source_count = std::sync::Arc::strong_count(&source);
            let theme_count = theme.strong_count();
            let snapshot = orbit.art_snapshot(&theme, &art);
            assert_eq!(theme.strong_count(), theme_count);
            assert_eq!(std::sync::Arc::strong_count(&source), source_count);
            assert_eq!(std::sync::Arc::weak_count(&source), 1);
            assert!(snapshot.restore(&orbit, &theme, orbit.targets).is_some());
            drop(art);
            drop(source);
            assert!(snapshot.restore(&orbit, &theme, orbit.targets).is_none(), "expired field {field}");
        }
        let snapshot = || orbit.art_snapshot(&theme, &Art::default());
        let other = ThemeHandle::prepare(load(), &bank, theme.generation()).unwrap();
        assert_eq!(theme.digest(), other.digest());
        assert!(snapshot().restore(&orbit, &other, orbit.targets).is_none(), "distinct theme handle");
        let saved = snapshot();
        art_cache_changed();
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_none(), "artwork cache revision");
        let saved = snapshot();
        visible_completion();
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_none(), "completion generation");
        let saved = snapshot();
        crate::network_runtime::initialize(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_none(), "network paths");
        let saved = snapshot();
        invalidate_art();
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_none(), "art invalidation before refresh");
        orbit.refresh_art();
        let saved = orbit.art_snapshot(&theme, &Art::default());
        orbit.release_art();
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_none(), "empty release still invalidates");
        for field in 0..3 {
            let saved = orbit.art_snapshot(&theme, &Art::default());
            match field {
                0 => orbit.set_cover_shape(!orbit.tall_art),
                1 => orbit.set_ink_art(!orbit.ink_art),
                _ => orbit.set_art_targets(ArtTargets { cover: Some(ArtTarget { width: 12, height: 16,
                    fit: common::theme::pack::Fit::Cover, logo_box: false }), ..ArtTargets::default() }),
            }
            assert!(saved.restore(&orbit, &theme, orbit.targets).is_none(), "configuration field {field}");
        }
        common::media::invalidate();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn art_snapshot_rejects_each_isolated_stamp_input() {
        use common::theme::{fonts::FontBank, frame::Art, runtime::ThemeSnapshot, ThemeHandle};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(), &bank, 907).unwrap();
        let mut orbit = Orbit::new();
        let pixels = std::sync::Arc::new(Pixmap::new(1, 1).unwrap());
        let art = Art { selected_cover: Some(pixels.clone()), ..Art::default() };
        macro_rules! rejects {
            ($name:literal, $($field:tt).+, $change:expr) => {{
                let saved = orbit.art_snapshot(&theme, &art);
                assert!(saved.restore(&orbit, &theme, orbit.targets).is_some(), "{} baseline", $name);
                $change;
                let current = orbit.art_stamp();
                let mut expected = saved.stamp;
                assert!(expected.$($field).+ != current.$($field).+, "{} did not change", $name);
                expected.$($field).+ = current.$($field).+;
                assert!(current == expected, "{} changed another stamp field", $name);
                assert!(saved.restore(&orbit, &theme, orbit.targets).is_none(), "{} reused", $name);
            }};
        }
        // Media invalidation also changes prepared paths; isolate its stored proof instead.
        let mut saved = orbit.art_snapshot(&theme, &art);
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_some());
        saved.stamp.path.media = saved.stamp.path.media.wrapping_add(1);
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_none(), "media generation reused");
        rejects!("prepared paths", path.paths, common::media::clear_prepared_paths());
        rejects!("network paths", path.network,
            crate::network_runtime::initialize(std::env::temp_dir().canonicalize().unwrap()).unwrap());
        rejects!("artwork cache revision", artwork, art_cache_changed());
        rejects!("completion generation", completion, visible_completion());
        rejects!("artwork invalidation generation", invalidation.0,
            ART_GENERATION.with(|g| g.set(g.get().wrapping_add(1))));
        rejects!("observed artwork generation", invalidation.1, orbit.generation = orbit.generation.wrapping_add(1));
        rejects!("release epoch", invalidation.2, orbit.release_art());
        rejects!("observed media generation", media_generation, orbit.media_generation = orbit.media_generation.wrapping_add(1));
        rejects!("cover shape", tall, orbit.tall_art = !orbit.tall_art);
        rejects!("ink mode", ink, orbit.ink_art = !orbit.ink_art);
        rejects!("configured targets", targets, orbit.targets.cover = Some(ArtTarget { width: 12, height: 16,
            fit: common::theme::pack::Fit::Cover, logo_box: false }));
        let saved = orbit.art_snapshot(&theme, &art);
        let mut requested = orbit.targets;
        requested.cover.as_mut().unwrap().width += 1;
        assert!(saved.stamp == orbit.art_stamp());
        assert!(saved.restore(&orbit, &theme, requested).is_none(), "requested targets reused");
        assert_eq!(pixels.data(), art.selected_cover.as_ref().unwrap().data());
        common::media::invalidate();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn art_snapshot_rejects_isolated_theme_proof_and_owner_inputs() {
        use common::theme::{fonts::FontBank, frame::Art, runtime::ThemeSnapshot, ThemeHandle};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let load = || ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap();
        let theme = ThemeHandle::prepare(load(), &bank, 908).unwrap();
        let other_theme = ThemeHandle::prepare(load(), &bank, theme.generation()).unwrap();
        assert_eq!(theme.generation(), other_theme.generation());
        assert_eq!(theme.digest(), other_theme.digest());
        let orbit = Orbit::new();
        let other_orbit = Orbit::new();
        let pixels = std::sync::Arc::new(Pixmap::new(1, 1).unwrap());
        let art = Art { selected_cover: Some(pixels.clone()), ..Art::default() };
        let snapshot = || orbit.art_snapshot(&theme, &art);
        let saved = snapshot();
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_some());
        assert!(saved.restore(&orbit, &other_theme, orbit.targets).is_none(), "theme identity reused");
        // Prepared themes are immutable; vary only the stored proof for these guards.
        let mut saved = snapshot();
        saved.theme_generation = saved.theme_generation.wrapping_add(1);
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_none(), "theme generation reused");
        let mut saved = snapshot();
        saved.theme_digest.push_str("-changed");
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_none(), "theme digest reused");
        let saved = snapshot();
        let mut expected = saved.stamp;
        assert_ne!(expected.owner, other_orbit.worker_id);
        expected.owner = other_orbit.worker_id;
        assert!(expected == other_orbit.art_stamp(), "distinct Orbits must have equal counters and configuration");
        assert!(saved.restore(&other_orbit, &theme, other_orbit.targets).is_none(), "distinct Orbit reused live pixels");
        assert!(std::sync::Arc::ptr_eq(&pixels,
            saved.restore(&orbit, &theme, orbit.targets).unwrap().selected_cover.as_ref().unwrap()));
    }

    #[test]
    #[cfg(feature = "perf-bench")]
    #[serial_test::serial(orbit_fixture)]
    fn art_snapshot_rejects_isolated_trace_recording_change() {
        use common::theme::{fonts::FontBank, frame::Art, runtime::ThemeSnapshot, ThemeHandle};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(), &bank, 909).unwrap();
        let orbit = Orbit::new();
        let saved = orbit.art_snapshot(&theme, &Art::default());
        assert!(!saved.stamp.recording);
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_some());
        let trace = std::env::temp_dir().join(format!("allium-isolated-snapshot-{}.jsonl", std::process::id()));
        common::perf_trace::start_probe(trace.to_str().unwrap());
        let mut expected = saved.stamp;
        expected.recording = true;
        assert!(orbit.art_stamp() == expected, "recording changed another stamp field");
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_none(), "trace recording reused");
        common::perf_trace::stop_probe();
        assert!(saved.restore(&orbit, &theme, orbit.targets).is_some());
        std::fs::remove_file(trace).unwrap();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn resolved_art_path_can_change_without_model_key_revisions() {
        let root = std::env::temp_dir().canonicalize().unwrap()
            .join(format!("allium-art-keycost-{}", std::process::id()));
        assert!(!root.exists());
        let tall = root.join("media/covers-tall/Game.png");
        std::fs::create_dir_all(tall.parent().unwrap()).unwrap();
        write_art(&tall, 1, 1, 90);
        common::media::invalidate();
        let entries = [Entry::Game(Game::new(root.join("Game.chd")))];
        common::media::metadata(entries[0].path());
        let mut orbit = Orbit::new();
        orbit.set_cover_shape(true);
        orbit.prepare_window(&entries, 0, 0, 1);
        let mut fallback_pixels = Pixmap::new(1, 1).unwrap();
        fallback_pixels.fill(tiny_skia::Color::from_rgba8(10, 0, 0, 255));
        let fallback = std::sync::Arc::new(fallback_pixels);
        let mut resolved_pixels = Pixmap::new(1, 1).unwrap();
        resolved_pixels.fill(tiny_skia::Color::from_rgba8(90, 0, 0, 255));
        let resolved = std::sync::Arc::new(resolved_pixels);
        assert!(orbit.insert_cached(CachedArt { target: None,
            path: art_path(&entries[0], "stock"), image: Some(fallback.clone()) }));
        assert!(orbit.insert_cached(CachedArt { target: None,
            path: tall.clone(), image: Some(resolved.clone()) }));
        let revisions = || (art_cache_revision(), completion_generation(),
            common::media::generation(), common::media::name_generation());
        let before = revisions();
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(), &bank, 905).unwrap();
        let old = orbit.theme_art(&entries, 0, 0, 1);
        let snapshot = orbit.art_snapshot(&theme, &old);
        assert!(snapshot.restore(&orbit, &theme, orbit.targets).is_some());
        assert!(std::sync::Arc::ptr_eq(old.selected_cover.as_ref().unwrap(), &fallback));
        assert_eq!(common::media::resolve(entries[0].path(), common::media::Kind::TallCover), Some(tall));
        assert_eq!(revisions(), before, "path-cache hydration has no model-key revision");
        assert!(snapshot.restore(&orbit, &theme, orbit.targets).is_none(), "path hydration");
        let refreshed = orbit.theme_art(&entries, 0, 0, 1);
        assert!(std::sync::Arc::ptr_eq(refreshed.selected_cover.as_ref().unwrap(), &resolved));
        assert_ne!(old.selected_cover.as_ref().unwrap().data(), refreshed.selected_cover.as_ref().unwrap().data());
        let actual = snapshot.restore(&orbit, &theme, orbit.targets)
            .unwrap_or_else(|| orbit.theme_art(&entries, 0, 0, 1));
        assert!(actual == refreshed && actual.completed_misses == refreshed.completed_misses);
        let snapshot = orbit.art_snapshot(&theme, &refreshed);
        for i in 0..512 {
            common::media::adopt_fallback_paths(Path::new(&format!("/snapshot/Evict{i}.chd")),
                [(common::media::Kind::Cover, "other.png".into())]);
        }
        assert_eq!(revisions(), before, "path eviction has no model-key revision");
        assert!(snapshot.restore(&orbit, &theme, orbit.targets).is_none(), "path eviction");
        let rebuilt = orbit.theme_art(&entries, 0, 0, 1);
        let actual = snapshot.restore(&orbit, &theme, orbit.targets)
            .unwrap_or_else(|| orbit.theme_art(&entries, 0, 0, 1));
        assert!(actual == rebuilt && actual.completed_misses == rebuilt.completed_misses);
        assert_eq!(actual.selected_cover.as_ref().unwrap().data(), fallback.data());
        orbit.release_art();
        common::media::invalidate();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn art_invalidation_reloads_replaced_files_and_previous_misses() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("allium-art-refresh-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        let entries = [Entry::Game(Game::new(root.join("Game.chd")))];
        let cover = art_path(&entries[0], "stock");
        let hero = art_path(&entries[0], "heroes");
        let prepared = art_path(&entries[0], "covers");
        write_art(&prepared, 101, 101, 20);
        let mut orbit = Orbit::new();
        orbit.prepare(&entries, 0);
        orbit.prepare_loaded_window(&entries, 0, 0, 1);
        let old = orbit.theme_art(&entries, 0, 0, 1);
        let old_legacy = orbit.image(&entries[0], "covers").unwrap().clone();
        assert!(old.selected_hero.is_none());
        assert_eq!(old.selected_cover.as_ref().unwrap().width(), COVER_SIZE);
        write_art(&cover, 240, 240, 90);
        write_art(&prepared, 101, 101, 100);
        write_art(&hero, 360, 210, 110);
        orbit.prepare(&entries, 0);
        orbit.prepare_loaded_window(&entries, 0, 0, 1);
        assert!(orbit.theme_art(&entries, 0, 0, 1).selected_hero.is_none());
        crate::view::details::invalidate_metadata();
        let mut platform = DefaultPlatform::new().unwrap();
        let mut display = platform.display().unwrap();
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        orbit.draw(&entries, 0, &mut display, &styles);
        orbit.prepare_loaded_window(&entries, 0, 0, 1);
        let new = orbit.theme_art(&entries, 0, 0, 1);
        assert_eq!(
            new.selected_cover.as_ref().unwrap().width(),
            THEME_COVER_SIZE
        );
        assert_eq!(new.selected_cover.as_ref().unwrap().data()[0], 90);
        assert_eq!(new.selected_hero.as_ref().unwrap().data()[0], 110);
        assert!(!std::sync::Arc::ptr_eq(
            old.selected_cover.as_ref().unwrap(),
            new.selected_cover.as_ref().unwrap()
        ));
        assert_eq!(orbit.image(&entries[0], "covers").unwrap().data()[0], 100);
        assert!(!std::sync::Arc::ptr_eq(
            &old_legacy,
            orbit.image(&entries[0], "covers").unwrap()
        ));
        use common::theme::{
            ThemeRuntime,
            frame::FrameInputs,
            pack::{Binding, ScreenId},
            render::PreparedFrame,
        };
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::create_dir_all(root.join("Themes")).unwrap();
        let mut runtime = ThemeRuntime::bind(
            &root.join("state"),
            &root.join("Themes"),
            &common::constants::ALLIUM_FONTS_DIR.canonicalize().unwrap(),
        )
        .unwrap();
        let handle = runtime.choose("orbit").unwrap().snapshot.unwrap();
        let mut input = FrameInputs::new(ScreenId::Library);
        input.rows.insert(Binding::Entries, vec![]);
        input.art = new.clone();
        let first_frame = PreparedFrame::prepare(handle.clone(), &input)
            .unwrap()
            .pixels()
            .data()
            .to_vec();
        let held = new.selected_cover.unwrap();
        write_art(&cover, 240, 240, 180);
        write_art(&hero, 360, 210, 190);
        invalidate_art();
        orbit.prepare_loaded_window(&entries, 0, 0, 1);
        let replaced = orbit.theme_art(&entries, 0, 0, 1);
        assert_eq!(replaced.selected_cover.as_ref().unwrap().data()[0], 180);
        assert_eq!(replaced.selected_hero.as_ref().unwrap().data()[0], 190);
        assert!(!std::sync::Arc::ptr_eq(
            &held,
            replaced.selected_cover.as_ref().unwrap()
        ));
        input.art = replaced;
        let second_frame = PreparedFrame::prepare(handle, &input).unwrap();
        assert_ne!(
            first_frame,
            second_frame.pixels().data(),
            "final-pixel cache follows the new source identity"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod performance_round_tests {
    use super::*;
    #[test]
    fn deferred_legacy_is_lazy_and_source_budget_counts_retained_arcs() {
        let mut orbit = Orbit::new();
        orbit.prepare(&[], 0);
        assert!(orbit.legacy_ready);
        orbit.defer_legacy();
        assert!(!orbit.legacy_ready);
        assert!(orbit.should_draw());
        assert_eq!(orbit.cache_entries(), 0);
        let held = std::sync::Arc::new(Pixmap::new(390, 189).unwrap());
        orbit.insert_cached(CachedArt { target: None,
            path: PathBuf::from("held"),
            image: Some(held.clone()),
        });
        orbit.release_art();
        assert_eq!(orbit.source_bytes(), 294840);
        for i in 0..32 {
            orbit.insert_cached(CachedArt { target: None,
                path: PathBuf::from(format!("hero{i}")),
                image: Some(std::sync::Arc::new(Pixmap::new(390, 189).unwrap())),
            });
            assert!(orbit.source_bytes() <= CACHE_PIXEL_BYTES);
        }
        drop(held);
        orbit.release_art();
        assert_eq!(orbit.source_bytes(), 0);
        assert!(orbit.retired.is_empty());
    }
    #[test]
    fn replaced_full_window_survives_while_previous_model_holds_art() {
        let mut orbit = Orbit::new();
        let mut previous = Vec::new();
        for i in 0..16 {
            let image = std::sync::Arc::new(if i == 15 {
                Pixmap::new(390, 189).unwrap()
            } else {
                Pixmap::new(140, 140).unwrap()
            });
            previous.push(image.clone());
            orbit.insert_cached(CachedArt { target: None,
                path: PathBuf::from(format!("old{i}")),
                image: Some(image),
            });
        }
        for i in 0..16 {
            let image = std::sync::Arc::new(if i == 15 {
                Pixmap::new(390, 189).unwrap()
            } else {
                Pixmap::new(140, 140).unwrap()
            });
            orbit.insert_cached(CachedArt { target: None,
                path: PathBuf::from(format!("new{i}")),
                image: Some(image),
            });
            assert!(orbit.source_bytes() <= CACHE_PIXEL_BYTES);
        }
        for i in 0..16 {
            assert!(
                orbit
                    .cached_image(&PathBuf::from(format!("new{i}")))
                    .is_some()
            );
        }
        assert_eq!(orbit.source_bytes(), 2 * (294840 + 15 * 78400));
        drop(previous);
        assert_eq!(orbit.source_bytes(), 294840 + 15 * 78400);
    }
    #[test]
    fn directory_logo_cover_fallback_is_contained_and_centered() {
        let root = std::env::temp_dir().join(format!("allium-polish-logo-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("logo.png");
        let mut logo = Pixmap::new(160, 40).unwrap(); logo.fill(tiny_skia::Color::from_rgba8(220, 80, 20, 255));
        logo.save_png(&path).unwrap();
        for target in [None, Some(ArtTarget { width: 128, height: 128, fit: common::theme::pack::Fit::Cover, logo_box: false })] {
            let image = load_device_art_fitted(&path, 480, 480, target, false, true).unwrap();
            assert_eq!(image.width(), image.height());
            assert_eq!(image.pixel(0, 0).unwrap().alpha(), 0);
            assert_eq!(image.pixel(image.width()/2, image.height()/2).unwrap().alpha(), 255);
            let rows: Vec<_> = (0..image.height()).filter(|y| image.pixel(image.width()/2, *y).unwrap().alpha() > 0).collect();
            assert!(rows.len() as u32 <= image.height()/4 + 2, "logo aspect ratio survives cover fitting");
            assert!((rows[0] + rows[rows.len()-1]).abs_diff(image.height()-1) <= 1);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_opened_source_is_rejected_before_payload_read() {
        let path = std::env::temp_dir().join(format!("allium-oversize-{}.png", std::process::id()));
        let file = File::create(&path).unwrap();
        file.set_len(MAX_PNG_BYTES + 1).unwrap();
        #[cfg(feature = "perf-bench")]
        common::perf::take();
        assert!(load_prepared_art(&path, (0, 0), 140, 140, false).is_none());
        #[cfg(feature = "perf-bench")]
        {
            let counts = common::perf::take();
            assert_eq!(
                (counts.read_attempts, counts.decodes, counts.disk_hits),
                (0, 0, 0)
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod media_contract_tests {
    use super::*;
    use crate::entry::game::Game;
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn resolved_shape_selects_separate_tall_cover_and_fits_square_fallback() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../common/tests/fixtures/media-sd/Roms/TEST")
            .canonicalize()
            .unwrap();
        let entries = vec![
            Entry::Game(Game::new(root.join("Complete.sfc"))),
            Entry::Game(Game::new(root.join("Legacy.sfc"))),
        ];
        let mut orbit = Orbit::new();
        orbit.set_ink_art(true);
        orbit.prepare_loaded_window(&entries, 0, 0, 2);
        let square = orbit.theme_art(&entries, 0, 0, 2);
        let image = square.selected_cover.unwrap();
        assert_eq!(image.width(), image.height());
        let logo = square.selected_logo.unwrap();
        assert_eq!(logo.width() * 24, logo.height() * 96);
        orbit.set_cover_shape(true);
        orbit.prepare_loaded_window(&entries, 0, 0, 2);
        let tall = orbit.theme_art(&entries, 0, 0, 2).selected_cover.unwrap();
        assert_eq!(tall.width() * 3, tall.height() * 2);
        let fallback = orbit.theme_art(&entries, 1, 0, 2).selected_cover.unwrap();
        assert_eq!(fallback.width() * 3, fallback.height() * 2);
        assert_eq!(fallback.width(), 64);
        assert!(std::sync::Arc::ptr_eq(
            &fallback,
            &orbit.theme_art(&entries, 1, 0, 2).selected_cover.unwrap()
        ));
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn fitted_prepared_art_preserves_aspect_and_disk_cache_dimensions() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../common/tests/fixtures/media-sd/Roms/TEST/media/logos/Complete.png");
        let image = load_prepared_art(&path, (0, 0), 480, 200, false).unwrap();
        assert_eq!((image.width(), image.height()), (480, 120));
        let again = load_prepared_art(&path, (0, 0), 480, 200, false).unwrap();
        assert_eq!(image.data(), again.data());
        let root = std::env::temp_dir().join(format!("allium-media-jpg-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let jpg = root.join("cover.jpg");
        image::RgbImage::new(80, 120).save(&jpg).unwrap();
        let jpeg = load_ink_cover(&jpg).unwrap();
        assert_eq!((jpeg.width(), jpeg.height()), (80, 120));
        std::fs::remove_dir_all(root).unwrap();
    }
}

thread_local! {
    static ART_CANCELLATION: std::cell::RefCell<Option<(std::sync::Arc<std::sync::atomic::AtomicU64>, u64)>> = const { std::cell::RefCell::new(None) };
}
fn art_job_current() -> bool {
    ART_CANCELLATION.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_none_or(|(token, epoch)| token.load(std::sync::atomic::Ordering::Acquire) == *epoch)
    })
}

#[derive(Debug)]
struct ArtJob {
    selection_generation: u64,
    owner: u64,
    epoch: u64,
    generation: u64,
    cancellation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    #[cfg(test)]
    gate: Option<std::sync::Arc<std::sync::Barrier>>,
    #[cfg(test)]
    started: Option<std::sync::mpsc::Sender<super::art_queue::AssetKey>>,
    ink: bool,
    tall: bool,
    targets: ArtTargets,
    task: WarmArt,
    network_paths: Vec<(common::media::Kind, PathBuf)>,
    container: Option<PathBuf>,
    #[cfg(feature = "perf-bench")]
    trace: Option<String>,
}
const RESULT_PIXEL_BYTES: usize = 640 * 480 * 4;
const IN_FLIGHT_JOBS: usize = 2;
struct QueuedPixels {
    bytes: usize,
    account: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl QueuedPixels {
    fn new(bytes: usize, account: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        account.fetch_add(bytes, std::sync::atomic::Ordering::AcqRel);
        Self { bytes, account: account.clone() }
    }
}
impl Drop for QueuedPixels {
    fn drop(&mut self) {
        self.account.fetch_sub(self.bytes, std::sync::atomic::Ordering::AcqRel);
    }
}
fn bound_result_art(art: &mut VecDeque<CachedArt>) -> bool {
    let bytes: usize = art.iter().filter_map(|art| art.image.as_ref()).map(|image| image.data().len()).sum();
    if bytes <= RESULT_PIXEL_BYTES { return false; }
    art.clear();
    common::perf_trace::count(common::perf_trace::Count::ArtAdmissionRejected, 1);
    common::perf_trace::count(common::perf_trace::Count::ArtAdmissionRejectedBytes, bytes as u64);
    true
}
pub(crate) async fn art_ready() {
    let ready = ART_WORKER.with(|state| state.borrow().as_ref().map(|worker| worker.ready.clone()));
    if let Some(ready) = ready { ready.notified().await; }
    else { std::future::pending::<()>().await; }
}
pub(crate) fn art_result_queued() -> bool {
    ART_WORKER.with(|state| state.borrow().as_ref().is_some_and(|worker|
        worker.queued_results.load(std::sync::atomic::Ordering::Acquire) != 0))
}

struct ArtResult {
    targets: ArtTargets,
    owner: u64,
    epoch: u64,
    task: WarmArt,
    rejected: bool,
    queued_pixels: Option<QueuedPixels>,
    media: Option<common::media::PreparedGame>,
    paths: Vec<(common::media::Kind, PathBuf)>,
    path: PathBuf,
    #[cfg(test)]
    decode_attempts: usize,
    sidecars: Option<super::details::WarmCache>,
    art: VecDeque<CachedArt>,
    #[cfg(feature = "perf-bench")]
    counts: common::perf::Counts,
    #[cfg(feature = "perf-bench")]
    stages: serde_json::Value,
}
struct ArtWorker {
    send: std::sync::mpsc::SyncSender<ArtJob>,
    recv: std::sync::mpsc::Receiver<ArtResult>,
    queued_results: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    busy: Option<(u64, u64)>,
    in_flight: usize,
    ready: std::sync::Arc<tokio::sync::Notify>,
    #[cfg(test)]
    queued_bytes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
thread_local! {
    // One decoder, one buffered result, and at most two jobs across all tabs.
    static ART_WORKER: std::cell::RefCell<Option<ArtWorker>> = const { std::cell::RefCell::new(None) };
}
fn worker() -> ArtWorker {
    let (send, jobs) = std::sync::mpsc::sync_channel::<ArtJob>(IN_FLIGHT_JOBS);
    let (done, recv) = std::sync::mpsc::sync_channel(1);
    let queued_bytes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let result_account = queued_bytes.clone();
    let queued_results = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let result_count = queued_results.clone();
    let ready = std::sync::Arc::new(tokio::sync::Notify::new());
    let completion = ready.clone();
    std::thread::Builder::new()
        .name("allium-art".into())
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || {
            #[cfg(target_os = "linux")]
            unsafe {
                if libc::setpriority(libc::PRIO_PROCESS, 0, 10) != 0 {
                    log::warn!(
                        "Cannot lower artwork worker priority: {}",
                        std::io::Error::last_os_error()
                    );
                }
            }
            common::perf_trace::initialize();
            common::media::set_metadata_budget(WORKER_METADATA_BYTES);
            let mut generation = None;
            while let Ok(job) = jobs.recv() {
                #[cfg(test)]
                if let Some(started) = &job.started { let _ = started.send(job.task.key()); }
                #[cfg(feature = "perf-bench")]
                if let Some(path) = &job.trace {
                    common::perf_trace::start_probe(path);
                }
                let mut task = common::perf_trace::task("artwork", job.selection_generation);
                #[cfg(test)]
                if let Some(gate) = &job.gate {
                    gate.wait();
                }
                if generation != Some(job.generation) {
                    common::media::invalidate();
                    super::details::invalidate_metadata();
                    common::media::set_metadata_budget(WORKER_METADATA_BYTES);
                    generation = Some(job.generation);
                }
                ART_CANCELLATION
                    .with(|slot| slot.replace(Some((job.cancellation.clone(), job.epoch))));
                let path = job.task.entry().path().to_owned();
                common::media::set_game_containers(
                    job.container.map(|container| (path.clone(), container)),
                );
                let metadata = matches!(job.task, WarmArt::Metadata(_));
                let mut kinds = job.task.media_kinds(job.tall);
                if art_job_current() {
                    for &kind in &kinds {
                        common::media::resolve(&path, kind);
                    }
                    if matches!(job.task, WarmArt::Cover(_))
                        && job.tall
                        && common::media::cached_resolve(&path, common::media::Kind::TallCover)
                            .is_none()
                    {
                        kinds.push(common::media::Kind::Cover);
                        common::media::resolve(&path, common::media::Kind::Cover);
                    }
                    common::media::adopt_fallback_paths(&path, job.network_paths);
                }
                let mut orbit = Orbit::new();
                orbit.ink_art = job.ink;
                orbit.tall_art = job.tall;
                orbit.targets = job.targets;
                let mut work = VecDeque::from([job.task.clone()]);
                #[cfg(feature = "perf-bench")]
                common::perf::take();
                common::perf_trace::begin_render();
                #[cfg(test)]
                MEDIA_DECODE_ATTEMPTS.with(|n| n.set(0));
                while !work.is_empty() && art_job_current() {
                    orbit.warm_art_step_sync(&mut work);
                }
                let media =
                    (metadata && art_job_current()).then(|| common::media::prepare_game_kinds(&path, &[]));
                if kinds.contains(&common::media::Kind::Cover) {
                    kinds.push(common::media::Kind::PreparedCover);
                }
                let paths = kinds
                    .into_iter()
                    .filter_map(|kind| {
                        common::media::cached_resolve(&path, kind).map(|path| (kind, path))
                    })
                    .collect();
                let rejected = bound_result_art(&mut orbit.cache);
                let bytes = orbit.cache.iter().filter_map(|art| art.image.as_ref()).map(|image| image.data().len()).sum();
                let result = ArtResult {
                    targets: job.targets,
                    owner: job.owner,
                    epoch: job.epoch,
                    task: job.task,
                    rejected,
                    queued_pixels: Some(QueuedPixels::new(bytes, &result_account)),
                    media,
                    paths,
                    path,
                    #[cfg(test)]
                    decode_attempts: MEDIA_DECODE_ATTEMPTS.with(|n| n.get()),
                    sidecars: metadata.then(super::details::warm_cache),
                    art: orbit.cache,
                    #[cfg(feature = "perf-bench")]
                    counts: common::perf::take(),
                    #[cfg(feature = "perf-bench")]
                    stages: common::perf_trace::probe_counts_if_enabled(),
                };
                ART_CANCELLATION.with(|slot| slot.replace(None));
                task.finish();
                // Publish before send: a completed result blocked on the bounded channel
                // must also prevent the pre-render no-result shortcut. Only receive decrements.
                result_count.fetch_add(1, std::sync::atomic::Ordering::Release);
                if done.send(result).is_err() {
                    result_count.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                    break;
                }
                completion.notify_one();
                common::perf_trace::drawn_frame();
            }
        })
        .expect("art worker unavailable");
    ArtWorker {
        send,
        recv,
        queued_results,
        busy: None,
        in_flight: 0,
        ready,
        #[cfg(test)]
        queued_bytes,
    }
}

impl Orbit {
    fn task_visible(&self, task: &WarmArt) -> bool {
        !matches!(task, WarmArt::Metadata(_)) && (self.selected_path.as_deref() == Some(task.entry().path())
            || (matches!(task, WarmArt::Cover(_) | WarmArt::CoverFallback(_))
                && self.visible_cover_entries.iter().any(|path| path == task.entry().path())))
    }
    pub(crate) fn art_pending(&self) -> bool {
        !self.pending_tasks.is_empty()
            || ART_WORKER.with(|state| {
                state
                    .borrow()
                    .as_ref()
                    .is_some_and(|w| w.busy.is_some_and(|(owner, _)| owner == self.worker_id))
            })
    }
    pub(crate) fn warm_art_step(&mut self, work: &mut VecDeque<WarmArt>) -> bool {
        self.warm_art_step_for_targets(work, self.targets)
    }
    pub(crate) fn warm_art_step_for_targets(&mut self, work: &mut VecDeque<WarmArt>, targets: ArtTargets) -> bool {
        let completed = self.adopt_art_result();
        self.submit_art_step_for_targets(work, targets);
        completed
    }
    pub(crate) fn adopt_art_result(&mut self) -> bool {
        self.refresh_art();
        ART_WORKER.with(|state| {
            let mut state = state.borrow_mut();
            let Some(worker) = state.as_mut() else { return false; };
            let mut completed = false;
            if let Ok(mut result) = worker.recv.try_recv() {
                worker.queued_results.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                result.queued_pixels.take();
                worker.in_flight -= 1;
                #[cfg(test)]
                TEST_WORKER_DECODES.with(|n| n.set(result.decode_attempts));
                if worker.in_flight == 0 { worker.busy = None; }
                if result.owner == self.worker_id && result.epoch == self.worker_epoch {
                    #[cfg(all(test, feature = "perf-bench"))]
                    TEST_ART_COMPLETION.with(|slot| *slot.borrow_mut() = Some(result.task.clone()));
                    let key = result.task.key();
                    self.pending_tasks.retain(|task| task.key() != key);
                    let task = Some(result.task);
                    let visible_cover = task.as_ref().is_some_and(|task| {
                        matches!(task, WarmArt::Cover(_) | WarmArt::CoverFallback(_))
                    }) && self.visible_cover_entries.contains(&result.path);
                    let adopted_metadata = result.media.is_some() || result.sidecars.is_some();
                    if let Some(media) = result.media {
                        common::media::adopt_game(media);
                    }
                    common::media::adopt_fallback_paths(&result.path, result.paths);
                    if let Some(cache) = result.sidecars {
                        super::details::adopt_warm_cache(cache);
                    }
                    if adopted_metadata {
                        METADATA_COMPLETIONS
                            .with(|generation| generation.set(generation.get().wrapping_add(1)));
                    }
                    let missed = result.art.iter().all(|art| art.image.is_none());
                    let visible = self.selected_path.as_ref() == Some(&result.path) || visible_cover;
                    if visible {
                        self.selected_protection = None;
                        self.protected_paths
                            .extend(result.art.iter().map(|item| item.path.clone()));
                    }
                    let mut rejected = result.rejected;
                    for item in result.art {
                        rejected |= !self.insert_cached(item);
                    }
                    if let Some(task) = task.filter(|task| {
                        matches!(task, WarmArt::Metadata(_)) || missed || rejected
                    }) {
                        let key = task.key();
                        let metadata = matches!(task, WarmArt::Metadata(_)).then(|| {
                            std::sync::Arc::downgrade(&common::media::cached_metadata(
                                task.entry().path(),
                            ))
                        });
                        self.completed_tasks
                            .retain(|completed| completed.key != key
                                || Self::task_target(&task, completed.targets) != Self::task_target(&task, result.targets));
                        if self.completed_tasks.len() >= 128 {
                            self.completed_tasks.pop_back();
                        }
                        art_cache_changed();
                        self.completed_tasks
                            .push_front(CompletedTask {
                                targets: result.targets, key, metadata, admission_rejected: rejected,
                                missing: missed || (rejected && visible),
                            });
                    }
                    #[cfg(feature = "perf-bench")]
                    {
                        if self.worker_samples.len() == 64 {
                            self.worker_samples.remove(0);
                        }
                        self.worker_samples.push((result.counts, result.stages));
                    }
                    completed = true;
                }
            }
            completed
        })
    }
    pub(crate) fn submit_art_step_for_targets(&mut self, work: &mut VecDeque<WarmArt>, targets: ArtTargets) {
        self.refresh_art();
        ART_WORKER.with(|state| {
            let mut state = state.borrow_mut();
            let worker = state.get_or_insert_with(worker);
            let owner = (self.worker_id, self.worker_epoch);
            if worker.busy != Some(owner) {
                while let Some(task) = self.pending_tasks.pop_back() { work.push_front(task); }
            }
            if worker.busy.is_some_and(|busy| busy != owner) || worker.in_flight >= IN_FLIGHT_JOBS {
                return;
            }
            while let Some(task) = work.pop_front() {
                if worker.in_flight > 0 && !(self.task_visible(&task)
                    && self.pending_tasks.back().is_some_and(|pending| self.task_visible(pending))) {
                    work.push_front(task);
                    break;
                }
                let key = task.key();
                if self.pending_tasks.iter().any(|pending| pending.key() == key) { continue; }
                if self
                    .completed_tasks
                    .iter()
                    .any(|completed| completed.key == key && Self::task_target(&task, completed.targets) == Self::task_target(&task, targets) && completed.is_resident(&task))
                {
                    continue;
                }
                work.retain(|queued| queued.key() != task.key());
                let path = match &task {
                    WarmArt::Metadata(_) => None,
                    WarmArt::Cover(e) | WarmArt::CoverFallback(e) => Some(self.cover_path(e)),
                    WarmArt::Hero(e) => Some(art_path(e, "heroes")),
                    WarmArt::Logo(e) => {
                        common::media::cached_resolve(e.path(), common::media::Kind::Logo)
                    }
                    WarmArt::OtherCover(e) => common::media::cached_resolve(
                        e.path(),
                        if self.tall_art {
                            common::media::Kind::Cover
                        } else {
                            common::media::Kind::TallCover
                        },
                    ),
                };
                // A missing path has to be resolved by the worker at least once.
                if let Some(index) = path
                    .as_ref()
                    .and_then(|path| self.cache.iter().position(|item| &item.path == path && item.target == Self::task_target(&task, targets)))
                {
                    let item = self.cache.remove(index).unwrap();
                    self.cache.push_front(item);
                    continue;
                }
                #[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
                crate::allium_launcher::navigation_warm_tests::art_load();
                let entry = match &task {
                    WarmArt::Metadata(e)
                    | WarmArt::Cover(e)
                    | WarmArt::CoverFallback(e)
                    | WarmArt::OtherCover(e)
                    | WarmArt::Logo(e)
                    | WarmArt::Hero(e) => e,
                };
                let network_paths = [
                    ("stock", common::media::Kind::Cover),
                    ("covers", common::media::Kind::PreparedCover),
                    ("heroes", common::media::Kind::Background),
                    ("logos", common::media::Kind::Logo),
                ]
                .into_iter()
                .filter(|(_, kind)| {
                    task.media_kinds(self.tall_art).contains(kind)
                        || (matches!(task, WarmArt::Cover(_))
                            && matches!(
                                kind,
                                common::media::Kind::Cover | common::media::Kind::PreparedCover
                            ))
                })
                .filter_map(|(name, kind)| {
                    crate::network_runtime::art_path(entry.path(), name).map(|p| (kind, p))
                })
                .collect();
                let container = common::media::game_container(entry.path());
                let job = ArtJob {
                    selection_generation: common::perf_trace::generation(),
                    owner: self.worker_id,
                    epoch: self.worker_epoch,
                    generation: common::media::generation(),
                    cancellation: self.cancellation.clone(),
                    #[cfg(test)]
                    gate: TEST_ART_GATE.with(|slot| slot.borrow_mut().take()),
                    #[cfg(test)]
                    started: TEST_ART_STARTED.with(|slot| slot.borrow().clone()),
                    ink: self.ink_art,
                    tall: self.tall_art,
                    targets,
                    task: task.clone(),
                    network_paths,
                    container,
                    #[cfg(feature = "perf-bench")]
                    trace: common::perf_trace::probe_path(),
                };
                if worker.send.try_send(job).is_ok() {
                    #[cfg(test)]
                    TEST_ART_REQUESTS.with(|requests| requests.set(requests.get() + 1));
                    self.pending_tasks.push_back(task);
                    worker.in_flight += 1;
                    worker.busy = Some(owner);
                } else {
                    work.push_front(task);
                    break;
                }
                // Return to the input loop between submissions, even when a
                // visible successor can run before the previous result is adopted.
                break;
            }
        })
    }
}

#[cfg(feature = "perf-bench")]
impl Orbit {
    pub(crate) fn take_worker_samples(&mut self) -> Vec<(common::perf::Counts, serde_json::Value)> {
        std::mem::take(&mut self.worker_samples)
    }
}

#[cfg(test)]
thread_local! { static TEST_WORKER_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
thread_local! { static TEST_ART_REQUESTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(crate) fn test_art_requests() -> usize {
    TEST_ART_REQUESTS.with(std::cell::Cell::get)
}
#[cfg(test)]
thread_local! { static TEST_ART_STARTED: std::cell::RefCell<Option<std::sync::mpsc::Sender<super::art_queue::AssetKey>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
thread_local! { static TEST_ART_GATE: std::cell::RefCell<Option<std::sync::Arc<std::sync::Barrier>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(all(test, feature = "perf-bench"))]
thread_local! { static TEST_ART_COMPLETION: std::cell::RefCell<Option<WarmArt>> = const { std::cell::RefCell::new(None) }; }
#[cfg(all(test, feature = "perf-bench"))]
pub(crate) fn proof_art_completion() -> Option<WarmArt> {
    TEST_ART_COMPLETION.with(|slot| slot.borrow_mut().take())
}
#[cfg(test)]
pub(crate) fn proof_art_gate(gate: Option<std::sync::Arc<std::sync::Barrier>>) {
    TEST_ART_GATE.with(|slot| *slot.borrow_mut() = gate);
}

#[cfg(test)]
mod network_media_tests {
    use super::*;
    use crate::{
        consoles::ConsoleMapper,
        entry::{Sort, directory::Directory, game::Game},
        network_library::{Catalog, NetworkGame, Payload},
        view::{details::Details, recents::RecentsSort},
    };
    use common::{database::Database, locale::Locale, media::Kind};

    #[test]
    fn network_directory_metadata_names_and_worker_art_preserve_gamelist() {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("allium-network-media-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ =
                    std::fs::remove_file(self.0.join(".allium/state/network-library/config.json"));
                crate::network_runtime::initialize(self.0.clone()).unwrap();
                std::fs::remove_dir_all(&self.0).unwrap();
                common::media::invalidate();
            }
        }
        let _cleanup = Cleanup(root.clone());
        let system = root.join("Roms/PS");
        let state = root.join(".allium/state/network-library");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(system.join("art")).unwrap();
        std::fs::create_dir_all(system.join("Imgs")).unwrap();
        for (file, width, height, color) in [
            (
                "art/cover.png",
                140,
                140,
                tiny_skia::Color::from_rgba8(255, 0, 0, 255),
            ),
            (
                "art/logo.png",
                400,
                175,
                tiny_skia::Color::from_rgba8(0, 255, 0, 255),
            ),
            (
                "art/hero.png",
                360,
                210,
                tiny_skia::Color::from_rgba8(255, 255, 0, 255),
            ),
            (
                "Imgs/Dir.png",
                140,
                140,
                tiny_skia::Color::from_rgba8(0, 0, 255, 255),
            ),
            (
                "Imgs/Dir.orbit.png",
                101,
                101,
                tiny_skia::Color::from_rgba8(0, 0, 255, 255),
            ),
            (
                "Imgs/Dir.hero.png",
                360,
                210,
                tiny_skia::Color::from_rgba8(0, 0, 255, 255),
            ),
        ] {
            let mut image = Pixmap::new(width, height).unwrap();
            image.fill(color);
            image.save_png(system.join(file)).unwrap();
        }
        let revision = "b".repeat(64);
        let descriptor = NetworkGame {
            id: "directory".into(),
            display_title: "Catalog title".into(),
            platform: "PS".into(),
            bundle_revision: revision.clone(),
            game_path: "Roms/PS/Dir/Dir.cue".into(),
            cache_path: "Roms/PS/Dir".into(),
            launch_path: "Roms/PS/Dir/Dir.cue".into(),
            install_kind: "directory".into(),
            files: [("cue", "Dir.cue", "cue"), ("bin", "Dir.bin", "rom")]
                .into_iter()
                .map(|(id, file, role)| Payload {
                    id: id.into(),
                    relative_path: file.into(),
                    bytes: 1,
                    sha256: "c".repeat(64),
                    role: role.into(),
                    http_route: format!("/v1/files/directory/{revision}/{id}"),
                })
                .collect(),
            discs: vec![],
        };
        let catalog = Catalog {
            version: 1,
            library_id: "fixture".into(),
            revision: "a".repeat(64),
            games: vec![descriptor.clone()],
        };
        catalog.validate().unwrap();
        std::fs::write(
            state.join("catalog.json"),
            serde_json::to_vec(&catalog).unwrap(),
        )
        .unwrap();
        let locale = Locale::new("en-US");
        let mapper = ConsoleMapper::new();
        for marquee in [true, false] {
            common::media::invalidate();
            std::fs::create_dir_all(system.join("Dir")).unwrap();
            std::fs::write(system.join("Dir/Dir.cue"), b"FILE \"Dir.bin\" BINARY\n").unwrap();
            std::fs::write(system.join("Dir/Dir.bin"), b"fixture").unwrap();
            std::fs::write(system.join("gamelist.xml"), format!("<gameList><game><path>./Dir/</path><name>Gamelist directory name</name><releasedate>19981203T000000</releasedate><desc>Directory synopsis</desc><image>./art/cover.png</image><fanart>./art/hero.png</fanart>{}</game></gameList>", if marquee { "<marquee>./art/logo.png</marquee>" } else { "" })).unwrap();
            let db = Database::in_memory().unwrap();
            let raw = Game::new(root.join(&descriptor.game_path));
            common::media::set_game_containers([(raw.path.clone(), system.join("Dir"))]);
            let row: common::database::NewGame = raw.clone().into();
            db.update_games(&[row.clone()]).unwrap();
            db.increment_play_count(&row).unwrap();
            common::media::check_directory(&system.join("Dir"));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let recents = loop {
                let recents = RecentsSort::LastPlayed
                    .entries(&db, &mapper, &locale)
                    .unwrap();
                if recents[0].name() == "Gamelist directory name" {
                    break recents;
                }
                common::media::poll_name_indexes();
                assert!(
                    std::time::Instant::now() < deadline,
                    "cold recent index did not finish"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            };
            assert_eq!(recents[0].name(), "Gamelist directory name");
            assert_eq!(db.select_game(&raw.path).unwrap().unwrap().name, row.name);
            let local = Directory::new(system.clone())
                .entries(&db, &mapper, &locale)
                .unwrap();
            assert!(
                local.iter().any(
                    |e| e.path() == system.join("Dir") && e.name() == "Gamelist directory name"
                )
            );
            std::fs::remove_dir_all(system.join("Dir")).unwrap();
            std::fs::write(
                state.join("config.json"),
                r#"{"version":1,"library_id":"fixture","address":"127.0.0.1:9"}"#,
            )
            .unwrap();
            crate::network_runtime::initialize(root.clone()).unwrap();
            assert!(crate::network_runtime::registered(&raw.path));
            common::media::invalidate();
            let mut search = Vec::new();
            crate::network_runtime::search("gAmElIsT dIrEcToRy", &mut search);
            assert!(search.is_empty());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while search.is_empty() {
                crate::network_runtime::poll();
                crate::network_runtime::search("gAmElIsT dIrEcToRy", &mut search);
                assert!(
                    std::time::Instant::now() < deadline,
                    "cold search index did not finish"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert_eq!(search.len(), 1);
            assert_eq!(search[0].path(), raw.path);
            assert_eq!(search[0].name(), "Gamelist directory name");
            assert_eq!(db.select_game(&raw.path).unwrap().unwrap().name, row.name);
            let mut stored = raw.clone();
            stored.favorite = true;
            stored.rating = Some(8);
            stored.developer = Some("Database studio".into());
            let mut existing = vec![Entry::Game(stored.clone())];
            crate::network_runtime::search(&raw.name, &mut existing);
            assert_eq!(existing.len(), 1);
            let Entry::Game(hydrated) = &existing[0] else {
                panic!("game")
            };
            let mut expected = stored;
            expected.name = "Gamelist directory name".into();
            expected.full_name = expected.name.clone();
            assert_eq!(hydrated, &expected);
            crate::network_runtime::search("Catalog title", &mut search);
            assert_eq!(search.len(), 1);
            for (kind, suffix) in [
                ("stock", ".png"),
                ("covers", ".orbit.png"),
                ("heroes", ".hero.png"),
            ] {
                assert_eq!(
                    crate::network_runtime::art_path(&raw.path, kind),
                    Some(system.join(format!("Imgs/Dir{suffix}")))
                );
            }
            for kind in ["logos", "screenshots", "covers-tall", "unknown"] {
                assert!(crate::network_runtime::art_path(&raw.path, kind).is_none());
            }
            let library = Directory::new(system.clone())
                .entries(&db, &mapper, &locale)
                .unwrap();
            let entry = library
                .iter()
                .find(|e| e.path() == raw.path)
                .unwrap()
                .clone();
            assert_eq!(entry.name(), "Gamelist directory name");
            assert!(!library.iter().any(|e| e.path() == system.join("Dir")));
            let recents = RecentsSort::LastPlayed
                .entries(&db, &mapper, &locale)
                .unwrap();
            assert_eq!(recents[0].name(), "Gamelist directory name");
            common::media::invalidate();
            let mut orbit = Orbit::new();
            orbit.set_ink_art(true);
            let mut work = VecDeque::from([
                WarmArt::Metadata(entry.clone()),
                WarmArt::Cover(entry.clone()),
                WarmArt::Logo(entry.clone()),
                WarmArt::Hero(entry.clone()),
            ]);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !work.is_empty() || orbit.art_pending() {
                orbit.warm_art_step(&mut work);
                assert!(std::time::Instant::now() < deadline, "art worker timed out");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let art = orbit.theme_art(std::slice::from_ref(&entry), 0, 0, 1);
            if marquee {
                let logo = art.selected_logo.unwrap();
                assert_eq!((logo.width(), logo.height()), (400, 175));
                assert_eq!(logo.pixel(0, 0).unwrap().green(), 255);
            } else {
                assert!(art.selected_logo.is_none());
            }
            assert_eq!(art.selected_cover.unwrap().pixel(0, 0).unwrap().red(), 255);
            assert_eq!(art.selected_hero.unwrap().pixel(0, 0).unwrap().red(), 255);
            assert_eq!(
                common::media::cached_resolve(&raw.path, Kind::Cover),
                Some(system.join("./art/cover.png"))
            );
            assert_eq!(
                common::media::cached_metadata(&raw.path).name,
                "Gamelist directory name"
            );
            for details in [
                Details::cached(&raw, "PlayStation"),
                Details::new(&raw, "PlayStation"),
            ] {
                assert!(details.metadata.contains("1998"));
                assert_eq!(details.blurb, "Directory synopsis");
            }
            if !marquee {
                std::fs::remove_file(system.join("art/cover.png")).unwrap();
                std::fs::remove_file(system.join("art/hero.png")).unwrap();
                common::media::invalidate();
                let mut work = VecDeque::from([
                    WarmArt::Cover(entry.clone()),
                    WarmArt::Hero(entry.clone()),
                    WarmArt::Logo(entry.clone()),
                ]);
                while !work.is_empty() || orbit.art_pending() {
                    orbit.warm_art_step(&mut work);
                    assert!(
                        std::time::Instant::now() < deadline,
                        "art fallback timed out"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                let art = orbit.theme_art(&[entry], 0, 0, 1);
                assert_eq!(art.selected_cover.unwrap().pixel(0, 0).unwrap().blue(), 255);
                assert_eq!(art.selected_hero.unwrap().pixel(0, 0).unwrap().blue(), 255);
                assert!(art.selected_logo.is_none());
            }
            std::fs::remove_file(state.join("config.json")).unwrap();
            crate::network_runtime::initialize(root.clone()).unwrap();
        }
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;
    #[test]
    fn hero_visible_sources_survive_shared_budget_and_retained_frame() {
        let theme: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../themes/orbit-hero/theme.json"
        )).unwrap();
        assert_eq!(theme["alignment"]["cover_shape"], "square");
        let components = theme["screens"]["library"]["components"].as_array().unwrap();
        let shelf = components.iter().find(|c| c["kind"] == "entries").unwrap();
        let count = shelf["visible_count"].as_u64().unwrap() as usize;
        assert_eq!(count, 3);
        assert_eq!(shelf["paint"]["art"]["width"], 160);
        assert!(shelf["paint"]["peek"].is_object());
        let hero = components.iter().find(|c| c["binding"] == "selected_hero").unwrap();
        let logo = components.iter().find(|c| c["binding"] == "selected_logo").unwrap();
        assert_eq!(hero["rect"], serde_json::json!([24, 56, 592, 176]));
        assert_eq!(logo["rect"], serde_json::json!([104, 96, 432, 80]));
        // Ink keeps MEDIA.md sources, not component-sized rasters. Selected cover aliases a row.
        let sizes: Vec<_> = [(640, 480), (480, 200)].into_iter()
            .chain(std::iter::repeat_n((480, 480), count + 1)).collect();
        let visible_bytes: usize = sizes.iter().map(|(w, h)| w * h * 4).sum();
        assert_eq!(visible_bytes, 5_299_200);
        let mut other = Orbit::new();
        for (i, &(w, h)) in sizes.iter().enumerate() {
            other.insert_cached(CachedArt { target: None,
                path: format!("other-{i}").into(),
                image: Some(std::sync::Arc::new(Pixmap::new(w as u32, h as u32).unwrap())),
            });
        }
        let mut orbit = Orbit::new();
        assert_eq!(orbit.source_budget(), 5_186_560);
        assert_eq!(visible_bytes - orbit.source_budget(), 112_640);
        let previous = std::sync::Arc::new(Pixmap::new(640, 480).unwrap());
        orbit.insert_cached(CachedArt { target: None, path: "previous-hero".into(), image: Some(previous.clone()) });
        for (i, &(w, h)) in sizes.iter().enumerate() {
            let path = PathBuf::from(format!("visible-{i}"));
            orbit.protected_paths.push(path.clone());
            orbit.insert_cached(CachedArt { target: None,
                path,
                image: Some(std::sync::Arc::new(Pixmap::new(w as u32, h as u32).unwrap())),
            });
        }
        println!("HERO_BYTES visible={visible_bytes} budget={} retired={} total={}",
            orbit.source_budget(), previous.data().len(), orbit.source_bytes());
        assert_eq!(orbit.cache.len(), sizes.len(), "visible source silently dropped");
        assert_eq!(orbit.source_bytes(), visible_bytes + previous.data().len());
        drop(previous);
        assert_eq!(orbit.source_bytes(), visible_bytes);
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn artkeep_covered_variants_are_bounded_and_selection_media_theme_release_them() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle, pack::ScreenId};
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let snapshot = || ThemeSnapshot::load(&root.join("themes/compact-plus")).unwrap();
        let theme = ThemeHandle::prepare(snapshot(), &bank, 501).unwrap();
        let lower = ArtTargets::for_screens(theme.pack(), &[ScreenId::Recents]);
        let upper = ArtTargets::for_screens(theme.pack(), &[ScreenId::GameDetail]);
        let entry = Entry::Game(crate::entry::game::Game::new("/artkeep/first.chd".into()));
        let next = Entry::Game(crate::entry::game::Game::new("/artkeep/second.chd".into()));
        let mut orbit = Orbit::new();
        orbit.set_art_theme(&theme);
        orbit.set_ink_art(true);
        orbit.set_art_targets(lower);
        orbit.protect_selected(&entry);
        orbit.protect_covered(Some(&entry), lower);
        let variants = orbit.covered_art.clone();
        let mut retained_bytes = 0;
        for (path, target) in &variants {
            let target = target.unwrap();
            let image = Pixmap::new(target.width, target.height).unwrap();
            retained_bytes += image.data().len();
            assert!(orbit.insert_cached(CachedArt { path: path.clone(), target: Some(target),
                image: Some(std::sync::Arc::new(image)) }));
        }
        assert_eq!(retained_bytes, 446464);
        orbit.set_art_targets(upper);
        for i in 0..20 {
            assert!(orbit.insert_cached(CachedArt { path: format!("pressure-{i}").into(), target: None,
                image: Some(std::sync::Arc::new(Pixmap::new(480, 480).unwrap())) }));
            assert!(orbit.source_bytes() <= orbit.source_budget());
            assert!(orbit.source_bytes() <= CACHE_PIXEL_BYTES);
            assert!(variants.iter().all(|(path, target)| orbit.cached_variant(path, *target).is_some()));
        }
        // Covered variants cannot force admission beyond the shared source allowance.
        assert!(!orbit.insert_cached(CachedArt { path: "oversized".into(), target: None,
            image: Some(std::sync::Arc::new(Pixmap::new(2048, 1024).unwrap())) }));
        assert!(orbit.source_bytes() <= orbit.source_budget());
        let epoch = orbit.worker_epoch;
        orbit.protect_selected(&next);
        orbit.protect_covered(Some(&next), lower);
        assert_ne!(orbit.worker_epoch, epoch);
        assert!(orbit.covered_art.iter().all(|(path, _)| !variants.iter().any(|(old, _)| path == old)));
        common::media::invalidate();
        orbit.refresh_art();
        assert!(orbit.cache.is_empty() && orbit.covered_art.is_empty());
        orbit.protect_covered(Some(&entry), lower);
        orbit.set_art_theme(&ThemeHandle::prepare(snapshot(), &bank, 502).unwrap());
        assert!(orbit.cache.is_empty() && orbit.covered_art.is_empty());
        orbit.protect_covered(Some(&entry), lower);
        orbit.release_art();
        assert!(orbit.covered_art.is_empty());
        assert_eq!(orbit.source_bytes(), 0);
    }

    #[test]
    fn artkeep_target_switch_preserves_completed_misses_per_variant() {
        use common::theme::pack::Fit;
        let mut orbit = Orbit::new();
        let lower = ArtTargets { hero: Some(ArtTarget { width: 192, height: 408,
            fit: Fit::Cover, logo_box: false }), ..ArtTargets::default() };
        let upper = ArtTargets { hero: Some(ArtTarget { width: 256, height: 328,
            fit: Fit::Cover, logo_box: false }), ..ArtTargets::default() };
        let task = WarmArt::Hero(Entry::Game(crate::entry::game::Game::new("/artkeep/missing.chd".into())));
        for targets in [lower, upper] {
            orbit.completed_tasks.push_back(CompletedTask { targets, key: task.key(),
                admission_rejected: false, missing: true, metadata: None });
        }
        let requests = test_art_requests();
        for targets in [lower, upper, lower] {
            orbit.set_art_targets(targets);
            let mut work = VecDeque::from([task.clone()]);
            orbit.warm_art_step(&mut work);
            assert!(work.is_empty() && !orbit.art_pending());
            assert_eq!(orbit.completed_tasks.len(), 2);
            assert_eq!(test_art_requests(), requests);
        }
        orbit.release_art();
        assert!(orbit.completed_tasks.is_empty());
    }

    #[test]
    fn ram_variants_do_not_alias_sizes_or_fit_modes() {
        use common::theme::pack::Fit;
        let mut orbit = Orbit::new();
        let path = PathBuf::from("same-source");
        let a = ArtTarget { width: 160, height: 160, fit: Fit::Cover, logo_box: false };
        let b = ArtTarget { width: 104, height: 156, fit: Fit::Contain, logo_box: false };
        for target in [a, b, ArtTarget { fit: Fit::Contain, ..a }] {
            assert!(orbit.insert_cached(CachedArt { path: path.clone(), target: Some(target),
                image: Some(std::sync::Arc::new(Pixmap::new(target.width, target.height).unwrap())) }));
        }
        assert_eq!(orbit.cache.len(), 3);
        for target in [a, b] {
            let image = orbit.cached_variant(&path, Some(target)).unwrap();
            assert_eq!((image.width(), image.height()), (target.width, target.height));
        }
        assert!(orbit.cached_variant(&path, None).is_none());
        assert_eq!(orbit.source_bytes(), (160 * 160 * 2 + 104 * 156) * 4);
    }
    #[test]
    fn protected_source_admission_still_has_a_hard_pixel_limit() {
        let mut orbit = Orbit::new();
        for i in 0..10 {
            let path = PathBuf::from(format!("visible-{i}"));
            orbit.protected_paths.push(path.clone());
            assert_eq!(orbit.insert_cached(CachedArt { target: None,
                path,
                image: Some(std::sync::Arc::new(Pixmap::new(480, 480).unwrap())),
            }), i < 7);
            assert!(orbit.source_bytes() <= CACHE_PIXEL_BYTES);
        }
        assert_eq!(orbit.cache.len(), 7);
    }
    #[test]
    #[cfg(feature = "perf-bench")]
    fn pack_sized_media_decode_once_without_resize_or_disk_cache() {
        let root = std::env::temp_dir().join(format!("allium-packed-art-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for (kind, w, h) in [
            ("cover", 480, 480),
            ("tall", 320, 480),
            ("logo", 480, 200),
            ("hero", 640, 480),
        ] {
            let path = root.join(format!("{kind}.png"));
            Pixmap::new(w, h).unwrap().save_png(&path).unwrap();
            common::perf::take();
            let image = load_device_art(&path, w, h).unwrap();
            let counts = common::perf::take();
            assert_eq!((image.width(), image.height()), (w, h));
            assert_eq!(
                (counts.decodes, counts.art_resizes, counts.disk_hits),
                (1, 0, 0)
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn fitted_device_disk_and_ram_paths_match_complete_source_frame() {
        use common::theme::{FontBank, ThemeHandle, frame::{FrameInputs, Row}, pack::{Binding, ScreenId}, render::PreparedFrame, runtime::ThemeSnapshot};
        let root = std::env::temp_dir().join(format!("allium-fitted-proof-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let mut orbit = Orbit::new();
        for id in ["orbit-hero", "big-picture"] {
            for shape in ["square", "tall"] {
                let snapshot = ThemeSnapshot::load_with_settings(&repo.join("themes").join(id),
                    &std::collections::BTreeMap::from([("cover_shape".into(), shape.into())])).unwrap();
                let theme = ThemeHandle::prepare(snapshot, &bank, 37).unwrap();
                let targets = ArtTargets::for_screens(theme.pack(), &[ScreenId::Library]);
                orbit.set_art_targets(targets);
                for (index, (w, h)) in [(101, 101), (79, 213), (137, 83)].into_iter().enumerate() {
                    let path = root.join(format!("{id}-{shape}-{index}.png"));
                    for replacement in [false, true] {
                        let mut source = Pixmap::new(w, h).unwrap();
                        for y in 1..h - 1 {
                            for x in 1..w - 1 {
                                let at = ((y * w + x) * 4) as usize;
                                let alpha = ((x * 13 + y * 7) % 256) as u8;
                                source.data_mut()[at..at + 4].copy_from_slice(&[if replacement { alpha } else { alpha / 3 }, alpha / 2, alpha, alpha]);
                            }
                        }
                        source.save_png(&path).unwrap();
                        if replacement { orbit.release_art(); }
                        let mut input = FrameInputs::new(ScreenId::Library);
                        let entries = theme.pack().screens[&ScreenId::Library].components.iter()
                            .find(|c| c.kind == common::theme::pack::Kind::Entries).unwrap();
                        input.focus.component = entries.id.clone();
                        input.focus.item = Some("0".into());
                        input.selected.insert(Binding::Entries, "0".into());
                        input.rows.insert(Binding::Entries, (0..entries.visible_count.unwrap()).map(|i| Row {
                            id: i.to_string(), label: format!("Game {i}"), subtitle: String::new(), value: String::new(), enabled: true,
                        }).collect());
                        let source = std::sync::Arc::new(source);
                        input.art.selected_hero = Some(source.clone());
                        input.art.selected_logo = Some(source.clone());
                        input.art.selected_cover = Some(source.clone());
                        input.art.next_cover = Some(source.clone());
                        for row in &input.rows[&Binding::Entries] { input.art.entries.insert(row.id.clone(), source.clone()); }
                        let reference = PreparedFrame::prepare(theme.clone(), &input).unwrap();
                        for pass in 0..3 {
                            let mut fitted = input.clone();
                            let mut load = |target, logo| {
                                if pass == 2 {
                                    return orbit.cached_variant(&path, target).unwrap().clone();
                                }
                                let image = std::sync::Arc::new(load_device_art_fitted(&path, 640, 480, target, logo, false).unwrap());
                                if pass == 1 {
                                    orbit.insert_cached(CachedArt { path: path.clone(), target, image: Some(image.clone()) });
                                }
                                image
                            };
                            fitted.art.selected_hero = Some(load(targets.hero, false));
                            fitted.art.selected_logo = Some(load(targets.logo, true));
                            let cover = load(targets.cover, false);
                            fitted.art.selected_cover = Some(cover.clone());
                            fitted.art.next_cover = Some(cover.clone());
                            for image in fitted.art.entries.values_mut() { *image = cover.clone(); }
                            let rendered = PreparedFrame::prepare(theme.clone(), &fitted).unwrap();
                            assert!(rendered.pixels().data() == reference.pixels().data(), "{id} {shape} {index} replacement={replacement} pass={pass}");
                        }
                        orbit.release_art();
                    }
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_cache_extra_allowance_is_shared_between_lists() {
        let added_budget = (CACHE_PIXEL_BYTES - BASE_CACHE_BYTES)
            + WORKER_METADATA_BYTES
            + 2 * 256 * 1024
            + 640 * 480 * 4
            + MAX_PNG_BYTES as usize
            + 64 * 1024
            + WORKER_STACK_BYTES
            + 64 * 1024;
        assert!(added_budget <= 8 * 1024 * 1024);
        let mut first = Orbit::new();
        let mut second = Orbit::new();
        for i in 0..12 {
            first.insert_cached(CachedArt { target: None,
                path: format!("first-{i}").into(),
                image: Some(std::sync::Arc::new(Pixmap::new(480, 480).unwrap())),
            });
            second.insert_cached(CachedArt { target: None,
                path: format!("second-{i}").into(),
                image: Some(std::sync::Arc::new(Pixmap::new(480, 480).unwrap())),
            });
            let added = first.source_bytes().saturating_sub(BASE_CACHE_BYTES)
                + second.source_bytes().saturating_sub(BASE_CACHE_BYTES);
            assert!(added <= 4 * 1024 * 1024);
        }
    }
}

#[cfg(test)]
mod media_dimension_review_tests {
    use super::*;

    #[test]
    fn tiny_huge_jpeg_is_rejected_before_every_media_decode() {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode(&[0, 0, 0], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        let sof = bytes.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
        bytes[sof + 5..sof + 7].copy_from_slice(&8192u16.to_be_bytes());
        bytes[sof + 7..sof + 9].copy_from_slice(&8192u16.to_be_bytes());
        assert!(bytes.len() < 1024);
        assert_eq!(
            image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .unwrap()
                .into_dimensions()
                .unwrap(),
            (8192, 8192)
        );
        let path =
            std::env::temp_dir().join(format!("allium-huge-header-{}.jpg", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        MEDIA_DECODE_ATTEMPTS.with(|n| n.set(0));
        assert!(decode_art_bytes(&bytes, (0, 0)).is_none());
        for (w, h, crop) in [
            (140, 140, false),
            (176, 264, false),
            (390, 189, true),
            (480, 200, false),
        ] {
            assert!(load_prepared_art(&path, (0, 0), w, h, crop).is_none());
        }
        assert!(load_ink_cover(&path).is_none());
        MEDIA_DECODE_ATTEMPTS.with(|n| assert_eq!(n.get(), 0));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn hero_crop_bounds_intermediate_pixels_for_extreme_aspect_ratio() {
        assert!(resize_art(Pixmap::new(2048, 1).unwrap(), 390, 189, true).is_none());
    }

    #[test]
    fn huge_png_cannot_bypass_limit_with_matching_requested_dimensions() {
        let mut bytes = Pixmap::new(1, 1).unwrap().encode_png().unwrap();
        bytes[16..20].copy_from_slice(&8192u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&8192u32.to_be_bytes());
        let mut crc = !0u32;
        for byte in &bytes[12..29] {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320u32 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
        assert_eq!(
            image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .unwrap()
                .into_dimensions()
                .unwrap(),
            (8192, 8192)
        );
        MEDIA_DECODE_ATTEMPTS.with(|n| n.set(0));
        assert!(decode_art_bytes(&bytes, (8192, 8192)).is_none());
        assert!(decode_png_bytes(&bytes, (8192, 8192)).is_none());
        MEDIA_DECODE_ATTEMPTS.with(|n| assert_eq!(n.get(), 0));
    }
}

#[cfg(test)]
mod demand_queue_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    fn drain(orbit: &mut Orbit, work: &mut VecDeque<WarmArt>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !work.is_empty() || orbit.art_pending() {
            orbit.warm_art_step(work);
            assert!(std::time::Instant::now() < deadline, "worker timeout");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    #[test]
    #[cfg(feature = "perf-bench")]
    #[serial_test::serial(orbit_fixture)]
    fn admission_rejection_is_deduplicated_until_selection_changes() {
        let root = std::env::temp_dir().join(format!("allium-admission-retry-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        Pixmap::new(640, 480).unwrap().save_png(root.join("Imgs/Next.hero.png")).unwrap();
        let entry = Entry::Game(crate::entry::game::Game::new(root.join("Next.sfc")));
        common::media::invalidate();
        let mut orbit = Orbit::new();
        orbit.set_ink_art(true);
        for i in 0..7 {
            let path = PathBuf::from(format!("protected-{i}"));
            orbit.protected_paths.push(path.clone());
            assert!(orbit.insert_cached(CachedArt { target: None,
                path,
                image: Some(Arc::new(Pixmap::new(480, 480).unwrap())),
            }));
        }
        common::perf_trace::start_probe(root.join("trace.log").to_str().unwrap());
        { let _input = common::perf_trace::phase(common::perf_trace::Phase::Input); }
        let tasks = VecDeque::from([WarmArt::Hero(entry.clone())]);
        drain(&mut orbit, &mut tasks.clone());
        assert!(orbit.image(&entry, "heroes").is_none());
        assert!(orbit.completed_tasks[0].admission_rejected);
        assert!(!orbit.theme_art(&[entry.clone()], 0, 0, 1).completed_misses.selected_hero,
            "a rejected prediction must not mark the future selection ready");
        for _ in 0..8 {
            let mut repeated = tasks.clone();
            assert!(!orbit.warm_art_step(&mut repeated));
            assert!(repeated.is_empty());
            assert!(!orbit.art_pending());
        }
        let measurement = common::perf_trace::probe_counts()["measurement"].clone();
        let names = measurement["counter_names"].as_array().unwrap();
        for (name, expected) in [("art_admission_rejected", 1), ("art_admission_rejected_bytes", 1_228_800)] {
            let index = names.iter().position(|n| n == name).unwrap();
            assert_eq!(measurement["input"]["counters"][index], expected);
        }
        orbit.protect_selected(&entry);
        drain(&mut orbit, &mut tasks.clone());
        assert!(orbit.image(&entry, "heroes").is_some());
        assert!(orbit.completed_tasks.is_empty());
        assert!(orbit.source_bytes() <= CACHE_PIXEL_BYTES);
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn completed_missing_assets_and_metadata_are_not_requeued_until_invalidation() {
        common::media::invalidate();
        let entry = Entry::Game(crate::entry::game::Game::new(
            "/missing-art-queue/Game.sfc".into(),
        ));
        let mut orbit = Orbit::new();
        let tasks = VecDeque::from([WarmArt::Logo(entry.clone()), WarmArt::Metadata(entry)]);
        drain(&mut orbit, &mut tasks.clone());
        assert_eq!(orbit.completed_tasks.len(), 2);
        orbit.cancel_selection();
        let mut repeated = tasks.clone();
        assert!(!orbit.warm_art_step(&mut repeated));
        assert!(repeated.is_empty());
        assert!(!orbit.art_pending());
        common::media::invalidate();
        orbit.refresh_art();
        assert!(orbit.completed_tasks.is_empty());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn name_refresh_retries_worker_cover_miss_after_index_adoption() {
        common::media::invalidate();
        let root =
            std::env::temp_dir().join(format!("allium-worker-name-refresh-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        let cover = root.join("new-cover.png");
        let pixels = Pixmap::new(32, 32).unwrap();
        pixels.save_png(root.join("Imgs/Retained.png")).unwrap();
        let entry = Entry::Game(crate::entry::game::Game::new(root.join("Game.sfc")));
        let retained_entry =
            Entry::Game(crate::entry::game::Game::new(root.join("Retained.sfc")));
        let mut orbit = Orbit::new();
        drain(
            &mut orbit,
            &mut VecDeque::from([
                WarmArt::Cover(entry.clone()),
                WarmArt::Metadata(entry.clone()),
                WarmArt::Cover(retained_entry.clone()),
            ]),
        );
        let failed_key = WarmArt::Cover(entry.clone()).key();
        assert!(orbit.completed_tasks.iter().any(|task| {
            task.key == failed_key && task.metadata.is_none()
        }));
        assert_eq!(orbit.completed_tasks.len(), 2);
        let retained_path = orbit.cover_path(&retained_entry);
        let retained = orbit.image(&retained_entry, "stock").unwrap().clone();
        let mut repeated = VecDeque::from([WarmArt::Cover(entry.clone())]);
        assert!(!orbit.warm_art_step(&mut repeated));
        assert!(repeated.is_empty());
        assert!(!orbit.art_pending());

        pixels.save_png(&cover).unwrap();
        std::fs::write(root.join("gamelist.xml"),
            "<gameList><game><path>Game.sfc</path><name>Indexed game</name><image>new-cover.png</image></game></gameList>").unwrap();
        common::media::defer_cold_sources();
        common::media::check_directory(&root);
        let generation = orbit.media_generation;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while common::media::cached_name(entry.path()).as_deref() != Some("Indexed game") {
            common::media::poll_name_indexes();
            assert!(std::time::Instant::now() < deadline, "name index timeout");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_ne!(generation, common::media::generation());
        orbit.refresh_names();
        let mut retry = VecDeque::from([WarmArt::Cover(entry.clone())]);
        assert!(!orbit.warm_art_step(&mut retry));
        assert!(orbit.art_pending(), "failed cover was not dispatched again");
        drain(&mut orbit, &mut retry);
        assert!(orbit.image(&entry, "stock").is_some());
        assert!(orbit.completed_tasks.is_empty());
        assert!(std::sync::Arc::ptr_eq(
            &retained,
            orbit.image(&retained_entry, "stock").unwrap()
        ));
        let mut cached = VecDeque::from([WarmArt::Cover(retained_entry)]);
        assert!(!orbit.warm_art_step(&mut cached));
        assert!(cached.is_empty());
        assert!(!orbit.art_pending());
        assert!(orbit.cache.iter().any(|item| item.path == retained_path));
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn metadata_eviction_and_revisit_reloads_completed_record() {
        common::media::invalidate();
        let root =
            std::env::temp_dir().join(format!("allium-metadata-revisit-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        let pixels = Pixmap::new(32, 32).unwrap().encode_png().unwrap();
        let description = "D".repeat(55 * 1024);
        let mut xml = String::from("<gameList>");
        let entries: Vec<_> = (0..80).map(|i| {
            xml.push_str(&format!("<game><path>./G{i}.sfc</path><desc>{description}</desc><developer>Studio {i}</developer></game>"));
            std::fs::write(root.join(format!("Imgs/G{i}.png")), &pixels).unwrap();
            Entry::Game(crate::entry::game::Game::new(root.join(format!("G{i}.sfc"))))
        }).collect();
        xml.push_str("</gameList>");
        assert!(xml.len() > common::media::METADATA_BUDGET);
        std::fs::write(root.join("gamelist.xml"), xml).unwrap();
        let mut orbit = Orbit::new();
        let mut retained = None;
        for (index, entry) in entries.iter().enumerate() {
            drain(
                &mut orbit,
                &mut VecDeque::from([
                    WarmArt::Cover(entry.clone()),
                    WarmArt::Metadata(entry.clone()),
                ]),
            );
            assert_eq!(
                common::media::cached_metadata(entry.path()).desc.len(),
                description.len()
            );
            if index == 0 {
                retained = Some(common::media::cached_metadata(entry.path()));
            }
        }
        let first = &entries[0];
        assert!(
            common::media::cached_metadata(first.path()).desc.is_empty(),
            "record was not evicted"
        );
        // All 80 successful metadata markers still fit in the 128-key dedupe window.
        assert_eq!(orbit.completed_tasks.len(), 80);
        orbit.cancel_selection();
        drain(
            &mut orbit,
            &mut VecDeque::from([WarmArt::Metadata(first.clone())]),
        );
        let metadata = common::media::cached_metadata(first.path());
        assert!(!std::sync::Arc::ptr_eq(
            retained.as_ref().unwrap(),
            &metadata
        ));
        let generation = metadata_completion_generation();
        let mut repeated = VecDeque::from([WarmArt::Metadata(first.clone())]);
        assert!(!orbit.warm_art_step(&mut repeated));
        assert!(repeated.is_empty());
        assert!(!orbit.art_pending());
        assert_eq!(metadata_completion_generation(), generation);
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
        assert!(
            metadata.desc == description,
            "evicted synopsis did not reload"
        );
        assert_eq!(
            orbit.completed_tasks.len(),
            80,
            "revisit duplicated a marker"
        );
        assert_eq!(metadata.developer, "Studio 0");
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn stale_job_is_skipped_before_decode_and_never_adopted() {
        common::media::invalidate();
        let fixture =
            std::env::temp_dir().join(format!("allium-stale-queue-{}", std::process::id()));
        std::fs::create_dir_all(fixture.join("Imgs")).unwrap();
        Pixmap::new(360, 210)
            .unwrap()
            .save_png(fixture.join("Imgs/Game.hero.png"))
            .unwrap();
        let entry = Entry::Game(crate::entry::game::Game::new(fixture.join("Game.sfc")));
        let gate = Arc::new(std::sync::Barrier::new(2));
        TEST_ART_GATE.with(|slot| slot.replace(Some(gate.clone())));
        let mut orbit = Orbit::new();
        let mut work = VecDeque::from([WarmArt::Hero(entry)]);
        orbit.warm_art_step(&mut work);
        assert!(orbit.art_pending());
        orbit.cancel_selection();
        TEST_ART_GATE.with(|slot| slot.replace(None));
        gate.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while orbit.art_pending() {
            assert!(!orbit.warm_art_step(&mut work));
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(orbit.cache.is_empty());
        assert!(orbit.completed_tasks.is_empty());
        TEST_WORKER_DECODES.with(|n| assert_eq!(n.get(), 0));
        std::fs::remove_dir_all(fixture).unwrap();
        let epoch = Arc::new(AtomicU64::new(1));
        ART_CANCELLATION.with(|slot| slot.replace(Some((epoch.clone(), 0))));
        let bytes = Pixmap::new(2, 2).unwrap().encode_png().unwrap();
        let root =
            std::env::temp_dir().join(format!("allium-stale-codec-{}.png", std::process::id()));
        std::fs::write(&root, &bytes).unwrap();
        MEDIA_DECODE_ATTEMPTS.with(|n| n.set(0));
        assert!(load_device_art(&root, 2, 2).is_none());
        MEDIA_DECODE_ATTEMPTS.with(|n| assert_eq!(n.get(), 0));
        epoch.store(0, Ordering::Release);
        assert!(art_job_current());
        ART_CANCELLATION.with(|slot| slot.replace(None));
        std::fs::remove_file(root).unwrap();
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn right_press_request_and_decode_counts() {
        use common::theme::pack::{Pack, ScreenId};
        use std::collections::HashSet;
        let root =
            std::env::temp_dir().join(format!("allium-right-art-counts-{}", std::process::id()));
        for folder in ["Imgs", "media/logos", "media/covers-tall"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        let pixels = Pixmap::new(32, 32).unwrap().encode_png().unwrap();
        let entries: Vec<_> = (0..12)
            .map(|i| {
                for suffix in [".png", ".hero.png", ".orbit.png"] {
                    std::fs::write(root.join(format!("Imgs/G{i}{suffix}")), &pixels).unwrap();
                }
                for folder in ["logos", "covers-tall"] {
                    std::fs::write(root.join(format!("media/{folder}/G{i}.png")), &pixels).unwrap();
                }
                Entry::Game(crate::entry::game::Game::new(
                    root.join(format!("G{i}.sfc")),
                ))
            })
            .collect();
        for (id, screen, count) in [
            ("orbit-hero", ScreenId::Recents, 3),
            ("compact", ScreenId::Recents, 8),
            ("compact-plus", ScreenId::Library, 9),
        ] {
            common::media::invalidate();
            let pack = if id == "compact" {
                Pack::builtin(id).unwrap()
            } else {
                Pack::parse(
                    &std::fs::read(
                        Path::new(env!("CARGO_MANIFEST_DIR"))
                            .join(format!("../../themes/{id}/theme.json")),
                    )
                    .unwrap(),
                )
                .unwrap()
            };
            let selected = 1;
            let first = 1;
            // Reference trace from the pre-change idle queue and raw Ink loader.
            let mut legacy: VecDeque<_> = entries
                .iter()
                .skip(first)
                .take(count)
                .cloned()
                .map(WarmArt::Cover)
                .collect();
            legacy.extend([
                WarmArt::Hero(entries[selected].clone()),
                WarmArt::Cover(entries[selected].clone()),
                WarmArt::Logo(entries[selected].clone()),
            ]);
            legacy.push_back(WarmArt::Cover(entries[first + count].clone()));
            for i in [0, 2, 1] {
                legacy.extend([
                    WarmArt::Metadata(entries[i].clone()),
                    WarmArt::Cover(entries[i].clone()),
                ]);
                if i == 1 || i == 2 {
                    legacy.extend([
                        WarmArt::Hero(entries[i].clone()),
                        WarmArt::Logo(entries[i].clone()),
                    ]);
                }
                if i == 1 {
                    legacy.push_back(WarmArt::OtherCover(entries[i].clone()));
                }
            }
            let before_tasks = legacy.len();
            let mut resident = HashSet::new();
            MEDIA_DECODE_ATTEMPTS.with(|n| n.set(0));
            for task in legacy {
                for kind in task.media_kinds(false) {
                    common::media::resolve(task.entry().path(), kind);
                }
                let path = match &task {
                    WarmArt::Metadata(_) => continue,
                    WarmArt::Cover(e) => art_path(e, "stock"),
                    WarmArt::Hero(e) => art_path(e, "heroes"),
                    WarmArt::Logo(e) => {
                        common::media::cached_resolve(e.path(), common::media::Kind::Logo).unwrap()
                    }
                    WarmArt::OtherCover(e) => {
                        common::media::cached_resolve(e.path(), common::media::Kind::TallCover)
                            .unwrap()
                    }
                    _ => unreachable!(),
                };
                if resident.insert(path.clone()) {
                    let bytes = read_png_bytes(File::open(path).unwrap()).unwrap();
                    assert!(decode_art_bytes(&bytes, (0, 0)).is_some());
                }
            }
            let before_decodes = MEDIA_DECODE_ATTEMPTS.with(|n| n.get());
            common::media::invalidate();
            let demand = super::super::art_queue::Demand::from_components(
                &pack.screens[&screen].components,
                pack.ink(),
            );
            let mut work =
                super::super::art_queue::plan(&entries, selected, first, count, 1, demand, true);
            let after_tasks = work.len();
            MEDIA_DECODE_ATTEMPTS.with(|n| n.set(0));
            let mut orbit = Orbit::new();
            orbit.set_ink_art(pack.ink());
            while !work.is_empty() {
                orbit.warm_art_step_sync(&mut work);
            }
            let after_decodes = MEDIA_DECODE_ATTEMPTS.with(|n| n.get());
            println!(
                "Right trace {id} {screen:?}: tasks {before_tasks} -> {after_tasks}; decodes {before_decodes} -> {after_decodes}"
            );
            assert!(after_tasks < before_tasks);
            assert!(after_decodes < before_decodes);
        }
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn largest_exporter_cover_and_second_visit_use_raw_cache_without_decode() {
        common::media::invalidate();
        let root =
            std::env::temp_dir().join(format!("allium-max-exporter-art-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        let cover = root.join("Imgs/Game.png");
        let entry = Entry::Game(crate::entry::game::Game::new(root.join("Game.sfc")));
        let mut pixels = Pixmap::new(480, 480).unwrap();
        let mut seed = 42u32;
        for pixel in pixels.pixels_mut() {
            let mut channel = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            };
            *pixel = tiny_skia::ColorU8::from_rgba(channel(), channel(), channel(), channel())
                .premultiply();
        }
        let bytes = pixels.encode_png().unwrap();
        assert!(bytes.len() > 512 * 1024 && bytes.len() as u64 <= MAX_PNG_BYTES);
        std::fs::write(&cover, &bytes).unwrap();
        let mut orbit = Orbit::new();
        orbit.set_ink_art(true);
        MEDIA_DECODE_ATTEMPTS.with(|n| n.set(0));
        orbit.warm_art_step_sync(&mut VecDeque::from([WarmArt::Cover(entry.clone())]));
        let first = orbit
            .theme_art(std::slice::from_ref(&entry), 0, 0, 1)
            .selected_cover
            .unwrap();
        MEDIA_DECODE_ATTEMPTS.with(|n| assert_eq!(n.get(), 1));
        orbit.release_art();
        orbit.warm_art_step_sync(&mut VecDeque::from([WarmArt::Cover(entry.clone())]));
        let second = orbit
            .theme_art(std::slice::from_ref(&entry), 0, 0, 1)
            .selected_cover
            .unwrap();
        MEDIA_DECODE_ATTEMPTS.with(|n| assert_eq!(n.get(), 1, "second visit decoded"));
        assert_eq!(first.data(), second.data());
        assert_eq!(first.data(), pixels.data());
        println!(
            "exporter_cover bytes={} first_visit_decodes=1 second_visit_decodes=0",
            bytes.len()
        );
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(orbit_fixture)]
    async fn medianoop_queue_probe_is_nonconsuming_and_submission_does_not_adopt() {
        common::media::invalidate();
        let entry = Entry::Game(crate::entry::game::Game::new("/missing-medianoop/Game.sfc".into()));
        let mut orbit = Orbit::new();
        orbit.protect_selected(&entry);
        assert!(!art_result_queued());
        let generation = completion_generation();
        let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
        proof_art_gate(Some(gate.clone()));
        let mut work = VecDeque::from([WarmArt::Hero(entry.clone())]);
        orbit.submit_art_step_for_targets(&mut work, orbit.targets);
        assert!(!art_result_queued(), "in-flight decode is not a queued result");
        gate.wait();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !art_result_queued() {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        for _ in 0..64 {
            assert!(art_result_queued());
            orbit.submit_art_step_for_targets(&mut work, orbit.targets);
            assert!(orbit.completed_tasks.is_empty(), "selection planning consumed a result");
        }
        assert_eq!(completion_generation(), generation);
        assert!(orbit.adopt_art_result());
        assert!(!art_result_queued());
        assert!(orbit.completed_tasks.iter().any(|task| task.key == WarmArt::Hero(entry.clone()).key() && task.missing));
        assert!(!orbit.adopt_art_result());
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(orbit_fixture)]
    async fn visible_jobs_start_before_adoption_wake_and_drop_stale_results() {
        let root = std::env::temp_dir().join(format!("allium-pipeline-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        std::fs::create_dir_all(root.join("media/logos")).unwrap();
        let entry = Entry::Game(crate::entry::game::Game::new(root.join("Game.sfc")));
        std::fs::write(entry.path(), []).unwrap();
        for (suffix, w, h) in [("hero", 640, 480), ("logo", 480, 200), ("cover", 480, 480)] {
            let mut image = Pixmap::new(w, h).unwrap();
            image.fill(tiny_skia::Color::from_rgba8(80, 90, 100, 173));
            let name = if suffix == "cover" { "Game.png".into() } else { format!("Game.{suffix}.png") };
            image.save_png(if suffix == "logo" { root.join("media/logos/Game.png") } else { root.join("Imgs").join(name) }).unwrap();
        }
        for cancel_before_adoption in [true, false] {
            loop {
                let mut ready = std::pin::pin!(art_ready());
                if std::future::Future::poll(ready.as_mut(), &mut std::task::Context::from_waker(std::task::Waker::noop())).is_pending() { break; }
            }
            common::media::invalidate();
            let mut orbit = Orbit::new();
            orbit.set_ink_art(true);
            orbit.protect_selected(&entry);
            let hero = WarmArt::Hero(entry.clone());
            let logo = WarmArt::Logo(entry.clone());
            let cover = WarmArt::Cover(entry.clone());
            let mut work = VecDeque::from([hero.clone(), logo.clone(), cover.clone()]);
            let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
            let (started, starts) = std::sync::mpsc::channel();
            TEST_ART_STARTED.with(|slot| *slot.borrow_mut() = Some(started));
            proof_art_gate(Some(gate.clone()));
            assert!(!orbit.warm_art_step(&mut work));
            assert_eq!(starts.recv_timeout(Duration::from_secs(2)).unwrap(), hero.key());
            assert!(tokio::time::timeout(Duration::from_millis(5), art_ready()).await.is_err());
            assert!(!orbit.warm_art_step(&mut work));
            gate.wait();
            assert_eq!(starts.recv_timeout(Duration::from_secs(2)).unwrap(), logo.key());
            tokio::time::timeout(Duration::from_secs(2), art_ready()).await.unwrap();
            assert!(orbit.cache.is_empty(), "second visible job required adoption");
            assert_eq!(work.len(), 1);
            assert_eq!(orbit.pending_tasks.len(), 2);
            let expected = (640 * 480 + 480 * 200) * 4;
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let bytes = ART_WORKER.with(|slot| slot.borrow().as_ref().unwrap().queued_bytes.load(std::sync::atomic::Ordering::Acquire));
                assert!(bytes <= IN_FLIGHT_JOBS * RESULT_PIXEL_BYTES);
                if bytes == expected { break; }
                assert!(Instant::now() < deadline, "second result never reached bounded queue: {bytes}");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert!(starts.try_recv().is_err(), "third job escaped in-flight bound");
            let adopted = if cancel_before_adoption { 0 } else {
                assert!(orbit.warm_art_step(&mut work));
                assert_eq!(starts.recv_timeout(Duration::from_secs(2)).unwrap(), cover.key());
                assert_eq!(orbit.cache.len(), 1);
                1
            };
            orbit.cancel_selection();
            work.clear();
            while orbit.art_pending() {
                assert!(!orbit.warm_art_step(&mut work), "stale result adopted");
                assert!(Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert_eq!(orbit.cache.len(), adopted);
            assert!(orbit.completed_tasks.is_empty());
            assert!(orbit.theme_art(std::slice::from_ref(&entry), 0, 0, 1).selected_logo.is_none());
            ART_WORKER.with(|slot| assert_eq!(slot.borrow().as_ref().unwrap().queued_bytes.load(std::sync::atomic::Ordering::Acquire), 0));
            TEST_ART_STARTED.with(|slot| *slot.borrow_mut() = None);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_results_are_rejected_before_publication_and_accounting_releases_on_drop() {
        let mut art = VecDeque::from([CachedArt { path: "oversized".into(), target: None,
            image: Some(std::sync::Arc::new(Pixmap::new(641, 480).unwrap())) }]);
        assert!(bound_result_art(&mut art));
        assert!(art.is_empty());
        let mut orbit = Orbit::new();
        let task = WarmArt::Cover(Entry::Game(crate::entry::game::Game::new("rejected".into())));
        orbit.completed_tasks.push_back(CompletedTask { targets: orbit.targets, key: task.key(), admission_rejected: true, missing: true, metadata: None });
        orbit.cancel_selection();
        assert!(orbit.completed_tasks.is_empty());
        let account = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first = QueuedPixels::new(RESULT_PIXEL_BYTES, &account);
        let second = QueuedPixels::new(RESULT_PIXEL_BYTES, &account);
        assert_eq!(account.load(std::sync::atomic::Ordering::Acquire), IN_FLIGHT_JOBS * RESULT_PIXEL_BYTES);
        drop(first);
        assert_eq!(account.load(std::sync::atomic::Ordering::Acquire), RESULT_PIXEL_BYTES);
        drop(second);
        assert_eq!(account.load(std::sync::atomic::Ordering::Acquire), 0);
    }
}
