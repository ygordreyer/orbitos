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
const MAX_PNG_BYTES: u64 = 512 * 1024;

pub fn enabled() -> bool {
    Stylesheet::orbit_enabled()
}

thread_local! {
    static ART_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static ART_COMPLETIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
pub(crate) fn completion_generation() -> u64 { ART_COMPLETIONS.with(std::cell::Cell::get) }
pub(crate) fn invalidate_art() {
    common::media::invalidate();
    ART_GENERATION.with(|generation| generation.set(generation.get().wrapping_add(1)));
}
fn art_generation() -> u64 {
    ART_GENERATION.with(std::cell::Cell::get)
}

#[derive(Debug)]
struct CachedArt {
    path: PathBuf,
    image: Option<std::sync::Arc<Pixmap>>,
}

#[derive(Clone, Debug)]
pub(crate) enum WarmArt {
    Metadata(Entry),
    Cover(Entry),
    CoverFallback(Entry),
    Logo(Entry),
    OtherCover(Entry),
    Hero(Entry),
}

#[derive(Debug)]
pub struct Orbit {
    cache: VecDeque<CachedArt>,
    source_account: std::rc::Rc<std::cell::Cell<usize>>,
    generation: u64,
    media_generation: u64,
    legacy_ready: bool,
    ink_art: bool,
    tall_art: bool,
    retired: Vec<std::sync::Weak<Pixmap>>,
    dirty: bool,
    worker_id: u64,
    worker_epoch: u64,
    pending_task: Option<WarmArt>,
    #[cfg(feature = "perf-bench")]
    worker_samples: Vec<(common::perf::Counts, serde_json::Value)>,
}

impl Orbit {
    pub fn new() -> Self {
        Self {
            cache: VecDeque::new(),
            source_account: source_account(),
            generation: art_generation(),
            media_generation: common::media::generation(),
            legacy_ready: false,
            ink_art: false,
            tall_art: false,
            retired: Vec::new(),
            dirty: true,
            worker_id: { static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1); NEXT.fetch_add(1,std::sync::atomic::Ordering::Relaxed) },
            worker_epoch: 0,
            pending_task: None,
            #[cfg(feature = "perf-bench")]
            worker_samples: Vec::new(),
        }
    }

    pub(crate) fn defer_legacy(&mut self) {
        self.legacy_ready = false;
        self.dirty = true;
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
    fn cover_path(&self, entry: &Entry) -> PathBuf {
        if self.tall_art {
            if let Some(path) =
                common::media::cached_resolve(entry.path(), common::media::Kind::TallCover)
            {
                return path;
            }
        }
        art_path(entry, "stock")
    }
    fn cache_logo(&mut self, entry: &Entry) {
        let Some(path) = common::media::cached_resolve(entry.path(), common::media::Kind::Logo)
        else {
            return;
        };
        if !self.cache.iter().any(|item| item.path == path) {
            let image = if self.ink_art { load_device_art(&path, 480, 200) } else { load_prepared_art(&path, (0, 0), 480, 200, false) }.map(std::sync::Arc::new);
            self.insert_cached(CachedArt { path, image });
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
    fn prepare_loaded_window(
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
    pub(crate) fn navigation_art_work(
        entries: &[Entry],
        selected: usize,
        first: usize,
        count: usize,
    ) -> VecDeque<WarmArt> {
        let mut work: VecDeque<_> = entries
            .iter()
            .skip(first)
            .take(count.min(CACHE_ENTRIES - 1))
            .cloned()
            .map(WarmArt::Cover)
            .collect();
        if let Some(entry) = entries.get(selected) {
            work.push_back(WarmArt::Hero(entry.clone()));
            work.push_back(WarmArt::Cover(entry.clone()));
            work.push_back(WarmArt::Logo(entry.clone()));
        }
        if let Some(next) = peek_game(entries, first, count) {
            work.push_back(WarmArt::Cover(entries[next].clone()));
        }
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
        let entry = match &task {
            WarmArt::Metadata(e)
            | WarmArt::Cover(e)
            | WarmArt::CoverFallback(e)
            | WarmArt::Hero(e)
            | WarmArt::Logo(e)
            | WarmArt::OtherCover(e) => e,
        };
        for kind in [
            common::media::Kind::Cover,
            common::media::Kind::TallCover,
            common::media::Kind::PreparedCover,
            common::media::Kind::Background,
            common::media::Kind::Logo,
        ] {
            common::media::resolve(entry.path(), kind);
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
                    if !self.cache.iter().any(|item| item.path == path) {
                        let image = if self.ink_art {
                            load_ink_cover(&path)
                        } else {
                            load_prepared_art(
                                &path,
                                (0, 0),
                                THEME_COVER_SIZE,
                                THEME_COVER_SIZE,
                                false,
                            )
                        };
                        self.insert_cached(CachedArt {
                            path,
                            image: image.map(std::sync::Arc::new),
                        });
                    }
                }
            }
            WarmArt::Cover(entry) | WarmArt::CoverFallback(entry) => {
                let path = self.cover_path(&entry);
                if self.cache.iter().any(|item| item.path == path) {
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
                    load_ink_cover(&path)
                } else {
                    load_prepared_art(&path, (0, 0), THEME_COVER_SIZE, THEME_COVER_SIZE, false)
                };
                if image.is_none() && !fallback {
                    work.push_front(WarmArt::CoverFallback(entry));
                } else {
                    self.insert_cached(CachedArt {
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
        let _stage = common::perf_trace::stage(common::perf_trace::Stage::ArtworkLookup);
        let mut art = common::theme::frame::Art::default();
        if let Some(entry) = entries.get(selected) {
            art.selected_logo = self.cached_image(&art_path(entry, "logos")).cloned();
            art.selected_hero = self.image(entry, "heroes").cloned();
            art.selected_cover = self.theme_cover_image(entry).cloned();
        }
        if let Some(next) = peek_game(entries, first, count) {
            art.next_cover = self.theme_cover_image(&entries[next]).cloned();
        }
        for entry in entries.iter().skip(first).take(count) {
            if let Some(image) = self.theme_cover_image(entry) {
                art.entries
                    .insert(crate::theme_adapter::entry_id(entry.path()), image.clone());
            }
        }
        art
    }

    fn theme_cover_image(&self, entry: &Entry) -> Option<&std::sync::Arc<Pixmap>> {
        self.cached_image(&self.cover_path(entry))
    }

    fn cached_image(&self, path: &Path) -> Option<&std::sync::Arc<Pixmap>> {
        self.cache
            .iter()
            .find(|item| item.path == path)?
            .image
            .as_ref()
    }

    fn cache_art(&mut self, path: PathBuf, dimensions: (u32, u32)) {
        if let Some(index) = self.cache.iter().position(|item| item.path == path) {
            let item = self.cache.remove(index).unwrap();
            self.cache.push_front(item);
            common::perf_trace::hit(common::perf_trace::ArtHit::Ram);
            #[cfg(feature = "perf-bench")]
            common::perf::hit(common::perf::Op::RamHit);
            return;
        }
        if self.ink_art && dimensions == (360, 210) {
            self.insert_cached(CachedArt { image:load_device_art(&path,640,480).map(std::sync::Arc::new), path });
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
        self.insert_cached(CachedArt {
            image: load_prepared_art(&path, dimensions, width, height, crop)
                .map(std::sync::Arc::new),
            path,
        });
    }

    fn source_bytes(&mut self) -> usize {
        self.retired.retain(|image| image.strong_count() != 0);
        let bytes = self.cache
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
        self.source_account.set(bytes.saturating_sub(BASE_CACHE_BYTES));
        bytes
    }

    fn retire(&mut self, item: CachedArt) {
        if let Some(image) = item
            .image
            .filter(|image| std::sync::Arc::strong_count(image) > 1)
        {
            self.retired.push(std::sync::Arc::downgrade(&image));
        }
    }

    fn insert_cached(&mut self, item: CachedArt) {
        let bytes = item.image.as_ref().map_or(0, |image| image.data().len());
        while self.cache.len() >= CACHE_ENTRIES || self.source_bytes() + bytes > self.source_budget() {
            let Some(old) = self.cache.pop_back() else {
                return;
            };
            self.retire(old);
        }
        self.cache.push_front(item);
        self.source_bytes();
    }

    fn source_budget(&self) -> usize {
        let other_extra = EXTRA_SOURCE_BYTES.with(|accounts| accounts.borrow().iter().filter_map(std::rc::Weak::upgrade).filter(|a| !std::rc::Rc::ptr_eq(a,&self.source_account)).map(|a|a.get()).sum::<usize>());
        BASE_CACHE_BYTES + (CACHE_PIXEL_BYTES - BASE_CACHE_BYTES).saturating_sub(other_extra)
    }

    fn image(&self, entry: &Entry, kind: &str) -> Option<&std::sync::Arc<Pixmap>> {
        let path = art_path(entry, kind);
        self.cached_image(&path)
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

    pub fn release_art(&mut self) {
        self.worker_epoch = self.worker_epoch.wrapping_add(1);
        self.pending_task = None;
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
            let lines = wrapped(&title, entry.name(), 340, 3);
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
            let label = wrapped(&caption, entry.name(), 158, 1);
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

fn decode_art_bytes(bytes: &[u8], dimensions: (u32, u32)) -> Option<Pixmap> {
    let _stage = common::perf_trace::stage(common::perf_trace::Stage::Decode);
    if bytes.starts_with(b"\x89PNG") {
        return decode_png_bytes(bytes, dimensions);
    }
    #[cfg(feature = "perf-bench")]
    common::perf::hit(common::perf::Op::Decode);
    common::perf_trace::hit(common::perf_trace::ArtHit::Decode);
    let image = image::load_from_memory(bytes).ok()?;
    if image.width() > 2048 || image.height() > 2048 {
        return None;
    }
    let rgba = image.to_rgba8();
    let mut pixels = Pixmap::new(rgba.width(), rgba.height())?;
    for (src, dst) in rgba.pixels().zip(pixels.pixels_mut()) {
        *dst = tiny_skia::ColorU8::from_rgba(src[0], src[1], src[2], src[3]).premultiply();
    }
    Some(pixels)
}

fn load_device_art(path: &Path, width: u32, height: u32) -> Option<Pixmap> {
    let _phase = common::perf_trace::phase(common::perf_trace::Phase::ArtLoad);
    common::perf_trace::frontend_io_op();
    let file = File::open(path).ok()?;
    common::perf_trace::frontend_io_op();
    if file.metadata().ok()?.len() > MAX_PNG_BYTES { return None; }
    common::perf_trace::frontend_io_op();
    let bytes = read_png_bytes(file)?;
    let (w,h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format().ok()?.into_dimensions().ok()?;
    if w == 0 || h == 0 || w > width || h > height { return None; }
    decode_art_bytes(&bytes, (0,0))
}
fn load_ink_cover(path: &Path) -> Option<Pixmap> {
    // MEDIA.md sources already fit the device. Geometry is fitted once by the renderer.
    load_device_art(path, 480, 480)
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
    let (width, height) = if crop {
        (width, height)
    } else {
        let (sw, sh) = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok()?;
        if sw == 0 || sh == 0 || sw > 2048 || sh > 2048 {
            return None;
        }
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
    let image = resize_art(decode_art_bytes(&bytes, dimensions)?, width, height, crop)?;
    if let Some(key) = key {
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
    Some(bytes)
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
    common::perf_trace::hit(common::perf_trace::ArtHit::Decode);
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

fn peek_game(entries: &[Entry], first: usize, count: usize) -> Option<usize> {
    let next = first.checked_add(count)?;
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
        assert!(input.art.next_cover.is_none());
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
    fn next_cover_follows_visible_window_without_wrap() {
        let entries = vec![
            Entry::Game(crate::entry::game::Game::new("First.chd".into())),
            Entry::Game(crate::entry::game::Game::new("Second.chd".into())),
        ];
        assert_eq!(peek_game(&entries, 0, 1), Some(1));
        assert_eq!(peek_game(&entries, 0, 2), None);
        assert_eq!(peek_game(&entries, usize::MAX, 1), None);
        assert_eq!(peek_game(&entries, 1, 1), None);
        assert_eq!(peek_game(&entries[..1], 0, 1), None);
        assert_eq!(peek_game(&[], 0, 1), None);
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use crate::entry::game::Game;

    fn write_art(path: &Path, width: u32, height: u32, red: u8) {
        let mut pixels = Pixmap::new(width, height).unwrap();
        pixels.fill(tiny_skia::Color::from_rgba8(red, 0, 0, 255));
        pixels.save_png(path).unwrap();
    }
    #[test]
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
        orbit.insert_cached(CachedArt {
            path: PathBuf::from("held"),
            image: Some(held.clone()),
        });
        orbit.release_art();
        assert_eq!(orbit.source_bytes(), 294840);
        for i in 0..32 {
            orbit.insert_cached(CachedArt {
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
            orbit.insert_cached(CachedArt {
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
            orbit.insert_cached(CachedArt {
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
        assert_eq!(fallback.width(), fallback.height());
        assert_eq!(fallback.width(), 64);
    }
    #[test]
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

#[derive(Debug)]
struct ArtJob {
    owner: u64,
    epoch: u64,
    generation: u64,
    #[cfg(test)]
    gate: Option<std::sync::Arc<std::sync::Barrier>>,
    ink: bool,
    tall: bool,
    task: WarmArt,
    network_paths: Vec<(common::media::Kind, PathBuf)>,
    #[cfg(feature = "perf-bench")]
    trace: Option<String>,
}
struct ArtResult {
    owner: u64,
    epoch: u64,
    media: common::media::PreparedGame,
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
    busy: Option<u64>,
}
thread_local! {
    // One worker and one result slot for the whole launcher, including inactive tabs.
    static ART_WORKER: std::cell::RefCell<Option<ArtWorker>> = const { std::cell::RefCell::new(None) };
}
fn worker() -> ArtWorker {
    let (send, jobs) = std::sync::mpsc::sync_channel::<ArtJob>(1);
    let (done, recv) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("allium-art".into())
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || {
            #[cfg(target_os = "linux")]
            unsafe {
                libc::setpriority(libc::PRIO_PROCESS, 0, 10);
            }
            common::perf_trace::initialize();
            common::media::set_metadata_budget(WORKER_METADATA_BYTES);
            let mut generation = None;
            while let Ok(job) = jobs.recv() {
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
                let entry = match &job.task {
                    WarmArt::Metadata(e)
                    | WarmArt::Cover(e)
                    | WarmArt::CoverFallback(e)
                    | WarmArt::OtherCover(e)
                    | WarmArt::Logo(e)
                    | WarmArt::Hero(e) => e,
                };
                let path = entry.path().to_owned();
                let metadata = matches!(job.task, WarmArt::Metadata(_));
                if !job.network_paths.is_empty() {
                    let _ = common::media::prepare_game(&path);
                    common::media::adopt_paths(&path, job.network_paths);
                }
                let mut orbit = Orbit::new();
                orbit.ink_art = job.ink;
                orbit.tall_art = job.tall;
                let mut work = VecDeque::from([job.task]);
                #[cfg(feature = "perf-bench")]
                if let Some(path) = &job.trace {
                    common::perf_trace::start_probe(path);
                }
                #[cfg(feature = "perf-bench")]
                common::perf::take();
                common::perf_trace::begin_render();
                while !work.is_empty() {
                    orbit.warm_art_step_sync(&mut work);
                }
                let media = common::media::prepare_game(&path);
                let result = ArtResult {
                    owner: job.owner,
                    epoch: job.epoch,
                    media,
                    sidecars: metadata.then(super::details::warm_cache),
                    art: orbit.cache,
                    #[cfg(feature = "perf-bench")]
                    counts: common::perf::take(),
                    #[cfg(feature = "perf-bench")]
                    stages: common::perf_trace::probe_counts_if_enabled(),
                };
                common::media::clear_prepared_paths();
                common::perf_trace::drawn_frame();
                if done.send(result).is_err() {
                    break;
                }
            }
        })
        .expect("art worker unavailable");
    ArtWorker {
        send,
        recv,
        busy: None,
    }
}

impl Orbit {
    pub(crate) fn art_pending(&self) -> bool {
        self.pending_task.is_some()
            || ART_WORKER.with(|state| {
                state
                    .borrow()
                    .as_ref()
                    .is_some_and(|w| w.busy == Some(self.worker_id))
            })
    }
    pub(crate) fn warm_art_step(&mut self, work: &mut VecDeque<WarmArt>) -> bool {
        self.refresh_art();
        ART_WORKER.with(|state| {
            let mut state = state.borrow_mut();
            let worker = state.get_or_insert_with(worker);
            let mut changed = false;
            if let Ok(result) = worker.recv.try_recv() {
                worker.busy = None;
                if result.owner == self.worker_id && result.epoch == self.worker_epoch {
                    self.pending_task = None;
                    common::media::adopt_game(result.media);
                    if let Some(cache) = result.sidecars {
                        super::details::adopt_warm_cache(cache);
                    }
                    for item in result.art {
                        self.insert_cached(item);
                    }
                    #[cfg(feature = "perf-bench")]
                    {
                        if self.worker_samples.len() == 64 {
                            self.worker_samples.remove(0);
                        }
                        self.worker_samples.push((result.counts, result.stages));
                    }
                    ART_COMPLETIONS.with(|generation| generation.set(generation.get().wrapping_add(1)));
                    changed = true;
                }
            }
            if worker.busy != Some(self.worker_id) {
                if let Some(task) = self.pending_task.take() {
                    work.push_front(task);
                }
            }
            if worker.busy.is_some() {
                return changed;
            }
            while let Some(task) = work.pop_front() {
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
                    .and_then(|path| self.cache.iter().position(|item| &item.path == path))
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
                .filter_map(|(name, kind)| {
                    crate::network_runtime::art_path(entry.path(), name).map(|p| (kind, p))
                })
                .collect();
                self.pending_task = Some(task.clone());
                let job = ArtJob {
                    owner: self.worker_id,
                    epoch: self.worker_epoch,
                    generation: common::media::generation(),
                    #[cfg(test)]
                    gate: TEST_ART_GATE.with(|slot| slot.borrow().clone()),
                    ink: self.ink_art,
                    tall: self.tall_art,
                    task,
                    network_paths,
                    #[cfg(feature = "perf-bench")]
                    trace: common::perf_trace::probe_path(),
                };
                if worker.send.try_send(job).is_ok() {
                    worker.busy = Some(self.worker_id);
                }
                break;
            }
            changed
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
thread_local! { static TEST_ART_GATE: std::cell::RefCell<Option<std::sync::Arc<std::sync::Barrier>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
pub(crate) fn proof_art_gate(gate: Option<std::sync::Arc<std::sync::Barrier>>) {
    TEST_ART_GATE.with(|slot| *slot.borrow_mut() = gate);
}

#[cfg(test)]
mod latency_tests {
    use super::*;
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
    fn source_cache_extra_allowance_is_shared_between_lists() {
        let added_budget = (CACHE_PIXEL_BYTES - BASE_CACHE_BYTES)
            + WORKER_METADATA_BYTES
            + 2 * 256 * 1024
            + 640 * 480 * 4
            + 512 * 1024
            + 64 * 1024
            + WORKER_STACK_BYTES
            + 64 * 1024;
        assert!(added_budget <= 8 * 1024 * 1024);
        let mut first = Orbit::new();
        let mut second = Orbit::new();
        for i in 0..12 {
            first.insert_cached(CachedArt {
                path: format!("first-{i}").into(),
                image: Some(std::sync::Arc::new(Pixmap::new(480, 480).unwrap())),
            });
            second.insert_cached(CachedArt {
                path: format!("second-{i}").into(),
                image: Some(std::sync::Arc::new(Pixmap::new(480, 480).unwrap())),
            });
            let added = first.source_bytes().saturating_sub(BASE_CACHE_BYTES)
                + second.source_bytes().saturating_sub(BASE_CACHE_BYTES);
            assert!(added <= 4 * 1024 * 1024);
        }
    }
}
