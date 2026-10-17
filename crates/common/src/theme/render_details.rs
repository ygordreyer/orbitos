use super::*;
use crate::theme::image_cache::{ImageKey, SourceIdentity, source_identity, remember_source_identity};
use std::collections::VecDeque;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc};

type Pixels = (ImageKey, Arc<Pixmap>, SourceIdentity, Arc<Pixmap>);
type DetailJob = Box<dyn FnOnce() + Send>;
thread_local! {
    static WORKER: std::cell::RefCell<Option<mpsc::SyncSender<DetailJob>>> = const { std::cell::RefCell::new(None) };
}
fn submit(job: DetailJob) -> std::result::Result<(), DetailJob> {
    WORKER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let (send, recv) = mpsc::sync_channel::<DetailJob>(1);
            if std::thread::Builder::new().name("allium-details-images".into())
                .stack_size(256 * 1024).spawn(move || {
                    #[cfg(target_os = "linux")]
                    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10); }
                    while let Ok(job) = recv.recv() { job(); }
                }).is_err() { return Err(job); }
            *slot = Some(send);
        }
        slot.as_ref().unwrap().try_send(job).map_err(|error| match error { mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job) => job })
    })
}

/// One source/transform at a time. Dropping a stale plan never publishes its result.
pub struct DetailImageWarm {
    stamp: NavigationStamp,
    images: VecDeque<(ImageKey, Arc<Pixmap>)>,
    pending: Option<mpsc::Receiver<Result<Pixels>>>,
    cancelled: Arc<AtomicBool>,
}
impl Drop for DetailImageWarm {
    fn drop(&mut self) { self.cancelled.store(true, Ordering::Release); }
}
impl PreparedFrame {
    pub fn detail_images(&self, input: &FrameInputs) -> DetailImageWarm {
        let mut images = VecDeque::new();
        if self.can_prewarm_navigation() && self.theme.pack().ink() {
            if let Some(screen) = self.theme.pack().screens.get(&ScreenId::GameDetail) {
                // Dynamic dialog geometry requires its real metadata model.
                if !screen.components.iter().any(|c| c.paint.as_ref()
                    .is_some_and(|p| p.fit_content || p.collapse_empty)) {
                    for c in &screen.components {
                        if c.layout == Some(Arrangement::Hidden) || c.paint.as_ref()
                            .is_some_and(|p| p.when.is_some() || p.opacity == Some(0.0)) { continue; }
                        let logo = c.logo || (c.kind == Kind::Image && c.binding == Some(Binding::SelectedLogo));
                        let (component, rect, source) = if logo {
                            if super::super::logo_suppressed_by_cover(c, input) { continue; }
                            let Some(source) = &input.art.selected_logo else { continue; };
                            let mut slot = c.clone();
                            slot.fit = Some(super::super::pack::Fit::Contain);
                            slot.radius = Some(0);
                            (slot, Self::logo_rect(c, source), source)
                        } else if c.kind == Kind::Image {
                            let source = match c.binding {
                                Some(Binding::SelectedHero) => &input.art.selected_hero,
                                Some(Binding::NextCover) => &input.art.next_cover,
                                _ => &input.art.selected_cover,
                            };
                            let Some(source) = source else { continue; };
                            (self.ink_image_component(c), c.rect, source)
                        } else { continue; };
                        let key = self.image_key(ScreenId::GameDetail, rect, &component, component.radius.unwrap_or(0));
                        images.push_back((key, source.clone()));
                    }
                }
            }
        }
        DetailImageWarm { stamp: self.navigation_stamp(), images, pending: None, cancelled: Arc::new(AtomicBool::new(false)) }
    }

    /// Poll or submit one transform. Resize, blur and rounding run off the input thread.
    pub fn prewarm_detail_image_step(&self, work: &mut DetailImageWarm) -> Result<bool> {
        if !self.navigation_is_current(&work.stamp) {
            work.cancelled.store(true, Ordering::Release);
            work.images.clear();
            work.pending = None;
            return Ok(true);
        }
        if let Some(pending) = &work.pending {
            match pending.try_recv() {
                Ok(result) => {
                    let (key, source, identity, pixels) = result?;
                    remember_source_identity(&source, identity);
                    let mut cache = self.theme.base().0.image_cache.lock()
                        .map_err(|_| anyhow::anyhow!("Image cache unavailable"))?;
                    // Recheck admission after yielding. Speculation must never evict real paint.
                    if cache.get(&key, &source).is_none() && cache.prewarm_fits(pixels.data().len()) {
                        cache.insert(key, &source, pixels);
                    }
                    work.pending = None;
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(false),
                Err(mpsc::TryRecvError::Disconnected) => { work.pending = None; }
            }
            return Ok(work.images.is_empty());
        }
        let Some((key, source)) = work.images.pop_front() else { return Ok(true); };
        let mut cache = self.theme.base().0.image_cache.lock()
            .map_err(|_| anyhow::anyhow!("Image cache unavailable"))?;
        if cache.get_known(&key, &source).is_none()
            && cache.prewarm_fits(key.width as usize * key.height as usize * 4) {
            let (done, pending) = mpsc::sync_channel(1);
            let (theme, job_key, job_source, cancelled) = (self.theme.clone(), key.clone(), source.clone(), work.cancelled.clone());
            if submit(Box::new(move || {
                if cancelled.load(Ordering::Acquire) { return; }
                let identity = source_identity(&job_source);
                let result = PreparedFrame::transform_image(&theme, &job_key, &job_source)
                    .map(|pixels| (job_key, job_source, identity, pixels));
                if !cancelled.load(Ordering::Acquire) { let _ = done.try_send(result); }
            })).is_ok() {
                work.pending = Some(pending);
            } else {
                work.images.push_front((key, source));
            }
        }
        Ok(work.images.is_empty() && work.pending.is_none())
    }
}

type TextPixels = (String, Arc<Pixmap>);
pub struct DetailTextWarm {
    stamp: NavigationStamp,
    plan: Option<DetailJob>,
    planned: Option<mpsc::Receiver<Result<VecDeque<ink_renderer::TextRaster>>>>,
    texts: VecDeque<ink_renderer::TextRaster>,
    pending: Option<mpsc::Receiver<Result<TextPixels>>>,
    cancelled: Arc<AtomicBool>,
}
impl Drop for DetailTextWarm {
    fn drop(&mut self) { self.cancelled.store(true, Ordering::Release); }
}
impl PreparedFrame {
    pub fn detail_texts(&self, model: impl FnOnce() -> Result<FrameInputs> + Send + 'static) -> DetailTextWarm {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        let theme = self.theme.clone();
        let (done, planned) = mpsc::sync_channel(1);
        let plan = Box::new(move || {
            if cancel.load(Ordering::Acquire) { return; }
            let result = model().and_then(|input| ink_renderer::collect_detail_text(theme, &input));
            if !cancel.load(Ordering::Acquire) { let _ = done.try_send(result); }
        });
        DetailTextWarm { stamp: self.navigation_stamp(), plan: Some(plan), planned: Some(planned), texts: VecDeque::new(), pending: None, cancelled }
    }
    /// Layout and one raster at a time run on the niced image worker; admission never evicts.
    pub fn prewarm_detail_text_step(&self, work: &mut DetailTextWarm) -> Result<bool> {
        if !self.navigation_is_current(&work.stamp) {
            work.cancelled.store(true, Ordering::Release);
            work.plan = None;
            work.planned = None;
            work.pending = None;
            work.texts.clear();
            return Ok(true);
        }
        if let Some(job) = work.plan.take() {
            if let Err(job) = submit(job) { work.plan = Some(job); }
            return Ok(false);
        }
        if let Some(planned) = &work.planned {
            match planned.try_recv() {
                Ok(result) => { work.texts = result?; work.planned = None; }
                Err(mpsc::TryRecvError::Empty) => return Ok(false),
                Err(mpsc::TryRecvError::Disconnected) => { work.planned = None; }
            }
            return Ok(work.texts.is_empty());
        }
        if let Some(pending) = &work.pending {
            match pending.try_recv() {
                Ok(result) => {
                    let (key, pixels) = result?;
                    if !ink_renderer::admit_text(&self.theme, key, pixels) { work.texts.clear(); }
                    work.pending = None;
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(false),
                Err(mpsc::TryRecvError::Disconnected) => { work.pending = None; }
            }
            return Ok(work.texts.is_empty());
        }
        let Some(text) = work.texts.pop_front() else { return Ok(true); };
        if ink_renderer::text_cached(&self.theme, &text.key) { return Ok(work.texts.is_empty()); }
        if !ink_renderer::text_prewarm_fits(text.bytes()) { work.texts.clear(); return Ok(true); }
        let (done, pending) = mpsc::sync_channel(1);
        let cancelled = work.cancelled.clone();
        // Retain the request if the bounded worker queue is busy.
        let text = Arc::new(text);
        let job_text = text.clone();
        if submit(Box::new(move || {
            if cancelled.load(Ordering::Acquire) { return; }
            let result = job_text.pixels().map(|pixels| (job_text.key.clone(), pixels));
            if !cancelled.load(Ordering::Acquire) { let _ = done.try_send(result); }
        })).is_ok() {
            work.pending = Some(pending);
        } else {
            work.texts.push_front(Arc::try_unwrap(text).ok().unwrap());
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::render_navigation::tests::{arts, model, theme};
    use std::time::{Duration, Instant};

    fn finish(frame: &PreparedFrame, work: &mut DetailImageWarm) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !frame.prewarm_detail_image_step(work).unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn text_model(theme: &ThemeHandle, arts: &[Arc<Pixmap>], cover: bool, logo: bool) -> FrameInputs {
        use crate::theme::frame::{DetailLine, Hint, Row, Pagination};
        let mut input = model(theme, ScreenId::Recents, 0, arts);
        input.screen = ScreenId::GameDetail;
        input.rows.clear();
        input.selected.clear();
        input.art.entries.clear();
        if !cover { input.art.selected_cover = None; }
        if !logo { input.art.selected_logo = None; }
        input.detail_lines = vec![DetailLine { text: "SYNOPSIS".into(), heading: true, gap_before: 0 }];
        for index in 0..6 {
            input.detail_lines.push(DetailLine { text: format!("Synopsis line {index}: a golden hour of adventure."), heading: false, gap_before: 0 });
        }
        input.metadata = vec![("Platform".into(), "PlayStation".into()), ("Released".into(), "1997".into())];
        input.facts = vec![("Developer".into(), "Human Entertainment".into()), ("Genres".into(), "Adventure".into())];
        input.pagination = Some(Pagination { page: 1, total: 3 });
        input.hints = vec![Hint { button: "A".into(), label: "Play".into() }, Hint { button: "B".into(), label: "Back".into() }, Hint { button: "Dpad".into(), label: "More information".into() }];
        let actions = theme.pack().screens[&ScreenId::GameDetail].components.iter()
            .find(|c| c.binding == Some(Binding::Actions)).unwrap();
        input.focus.component = actions.id.clone();
        input.focus.item = Some("play".into());
        input.rows.insert(Binding::Actions, ["play", "back"].map(|id| Row { id: id.into(), label: if id == "play" { "Play" } else { "Back" }.into(), subtitle: String::new(), value: String::new(), enabled: true }).into());
        input
    }
    fn finish_text(frame: &PreparedFrame, work: &mut DetailTextWarm) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !frame.prewarm_detail_text_step(work).unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn await_raster(frame: &PreparedFrame, work: &mut DetailTextWarm) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while work.pending.is_none() {
            assert!(!frame.prewarm_detail_text_step(work).unwrap());
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn details_text_warm_matches_cold_pixels_with_zero_misses() {
        let trace = std::env::temp_dir().join(format!("details-text-{:?}.log", std::thread::current().id()));
        crate::perf_trace::start_probe(trace.to_str().unwrap());
        let arts = arts();
        for screen in [ScreenId::Recents, ScreenId::Library] {
        for shape in ["square", "tall"] {
            for (cover, logo) in [(true, true), (false, true), (false, false)] {
                let theme = theme("compact-plus", shape);
                let frame = PreparedFrame::prepare(theme.clone(), &model(&theme, screen, 0, &arts)).unwrap();
                let input = text_model(&theme, &arts, cover, logo);
                ink_renderer::clear_text_cache();
                crate::perf_trace::begin_render();
                let cold = ink_renderer::uncached(|| PreparedFrame::prepare(theme.clone(), &input)).unwrap();
                assert!(crate::perf_trace::probe_counts()["text_misses"].as_u64().unwrap() > 0);
                ink_renderer::clear_text_cache();
                let displayed = frame.pixels().data().to_vec();
                let targets = frame.targets.clone();
                let boxes = frame.text_boxes.clone();
                let mut warm = frame.detail_texts(move || Ok(input));
                finish_text(&frame, &mut warm);
                assert_eq!(frame.pixels().data(), displayed);
                assert_eq!(frame.targets, targets);
                assert_eq!(frame.text_boxes, boxes);
                let input = text_model(&theme, &arts, cover, logo);
                crate::perf_trace::begin_render();
                let hot = ink_renderer::uncached(|| PreparedFrame::prepare(theme.clone(), &input)).unwrap();
                assert_eq!(crate::perf_trace::probe_counts()["text_misses"], 0, "{shape} cover={cover} logo={logo}");
                assert_eq!(cold.pixels().data(), hot.pixels().data());
                assert_eq!(cold.targets, hot.targets);
                assert_eq!(cold.text_boxes, hot.text_boxes);
                let mut changed = input.clone();
                changed.status.clock = Some("12:35".into());
                crate::perf_trace::begin_render();
                let _ = ink_renderer::uncached(|| PreparedFrame::prepare(theme.clone(), &changed)).unwrap();
                assert_eq!(crate::perf_trace::probe_counts()["text_misses"], 1, "only the changed clock may miss");
            }
        }
        }
        std::fs::remove_file(trace).unwrap();
    }
    #[test]
    fn details_text_warm_drops_stale_selection_theme_and_inflight_results() {
        let arts = arts();
        for change_theme in [false, true] {
            for in_flight in [false, true] {
                let theme = theme("compact-plus", "square");
                let mut frame = PreparedFrame::prepare(theme.clone(), &model(&theme, ScreenId::Recents, 0, &arts)).unwrap();
                ink_renderer::clear_text_cache();
                let input = text_model(&theme, &arts, true, true);
                let mut warm = frame.detail_texts(move || Ok(input));
                let key = if in_flight {
                    await_raster(&frame, &mut warm);
                    // The first synopsis request is still unpublished, even if the worker is done.
                    ink_renderer::collect_detail_text(theme.clone(), &text_model(&theme, &arts, true, true)).unwrap().front().unwrap().key.clone()
                } else { String::new() };
                let other = if change_theme { crate::theme::render_navigation::tests::theme("compact-plus", "tall") } else { theme.clone() };
                frame = PreparedFrame::prepare_reusing(other.clone(), &model(&other, ScreenId::Recents, 1, &arts), Some(frame)).unwrap();
                assert!(frame.prewarm_detail_text_step(&mut warm).unwrap());
                assert!(warm.cancelled.load(Ordering::Acquire));
                assert!(warm.texts.is_empty() && warm.pending.is_none() && warm.planned.is_none());
                assert!(!ink_renderer::text_cached(&theme, &key));
            }
        }
    }
    #[test]
    fn details_text_warm_never_evicts_real_text_and_prioritizes_synopsis() {
        let arts = arts();
        let theme = theme("compact-plus", "square");
        let frame = PreparedFrame::prepare(theme.clone(), &model(&theme, ScreenId::Recents, 0, &arts)).unwrap();
        let input = text_model(&theme, &arts, true, true);
        let requests = ink_renderer::collect_detail_text(theme.clone(), &input).unwrap();
        let synopsis = requests.front().unwrap();
        assert!(synopsis.key.contains("Synopsis line"));
        for in_flight in [false, true] {
            ink_renderer::clear_text_cache();
            let model = input.clone();
            let mut work = frame.detail_texts(move || Ok(model));
            if in_flight { await_raster(&frame, &mut work); }
            assert!(ink_renderer::admit_text(&theme, "visible".into(), Arc::new(Pixmap::new(512, 511).unwrap())));
            finish_text(&frame, &mut work);
            assert!(ink_renderer::text_cached(&theme, "visible"));
            assert!(!ink_renderer::text_cached(&theme, &synopsis.key));
        }
        ink_renderer::clear_text_cache();
        let spare = synopsis.bytes();
        let held_pixels = (1024 * 1024 - spare - 16) / 4;
        assert!(ink_renderer::admit_text(&theme, "visible".into(), Arc::new(Pixmap::new(held_pixels as u32, 1).unwrap())));
        let mut work = frame.detail_texts(move || Ok(input));
        finish_text(&frame, &mut work);
        assert!(ink_renderer::text_cached(&theme, "visible"));
        assert!(ink_renderer::text_cached(&theme, &synopsis.key));
        assert!(!ink_renderer::text_cached(&theme, &requests.back().unwrap().key));
    }
    #[test]
    fn details_image_warm_matches_cold_pixels_and_has_zero_misses() {
        let trace = std::env::temp_dir().join(format!("details-images-{:?}.log", std::thread::current().id()));
        crate::perf_trace::start_probe(trace.to_str().unwrap());
        let arts = arts();
        for shape in ["square", "tall"] {
            for with_cover in [true, false] {
                let theme = theme("compact-plus", shape);
                let mut input = model(&theme, ScreenId::Recents, 0, &arts);
                let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
                input.screen = ScreenId::GameDetail;
                input.rows.clear();
                input.selected.clear();
                input.focus = Default::default();
                input.art.entries.clear();
                if !with_cover { input.art.selected_cover = None; }
                let cold = ink_renderer::uncached(|| PreparedFrame::prepare(theme.clone(), &input)).unwrap();
                assert!(crate::perf_trace::probe_counts()["image_misses"].as_u64().unwrap() > 0);
                *theme.base().0.image_cache.lock().unwrap() = super::super::super::image_cache::ImageCache::for_pack(true);
                let displayed = frame.pixels().data().to_vec();
                let mut warm = frame.detail_images(&input);
                assert!(!warm.images.is_empty());
                finish(&frame, &mut warm);
                assert_eq!(displayed, frame.pixels().data());
                crate::perf_trace::begin_render();
                let hot = ink_renderer::uncached(|| PreparedFrame::prepare(theme.clone(), &input)).unwrap();
                assert_eq!(crate::perf_trace::probe_counts()["image_misses"], 0, "{shape} cover={with_cover}");
                assert_eq!(cold.pixels().data(), hot.pixels().data(), "{shape} cover={with_cover}");
            }
        }
        std::fs::remove_file(trace).unwrap();
    }
    #[test]
    fn details_image_warm_drops_stale_selection_and_never_evicts_real_images() {
        let theme = theme("compact-plus", "square");
        let arts = arts();
        let input = model(&theme, ScreenId::Recents, 0, &arts);
        let mut frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        let mut warm = frame.detail_images(&input);
        let (key, source) = warm.images.front().unwrap().clone();
        // Queue a transform before the selection changes, including the in-flight case.
        frame.prewarm_detail_image_step(&mut warm).unwrap();
        frame = PreparedFrame::prepare_reusing(theme.clone(), &model(&theme, ScreenId::Recents, 1, &arts), Some(frame)).unwrap();
        assert!(frame.prewarm_detail_image_step(&mut warm).unwrap());
        assert!(warm.cancelled.load(Ordering::Acquire));
        assert!(theme.base().0.image_cache.lock().unwrap().get(&key, &source).is_none());

        for (bytes_limited, in_flight) in [(false, false), (true, false), (false, true), (true, true)] {
            *theme.base().0.image_cache.lock().unwrap() = super::super::super::image_cache::ImageCache::for_pack(true);
            let mut work = frame.detail_images(&input);
            if in_flight {
                frame.prewarm_detail_image_step(&mut work).unwrap();
                assert!(work.pending.is_some());
            }
            let mut cache = theme.base().0.image_cache.lock().unwrap();
            *cache = super::super::super::image_cache::ImageCache::for_pack(true);
            let mut held = Vec::new();
            for index in 0..if bytes_limited { 1 } else { 32 } {
                let mut real = key.clone();
                real.component = format!("visible-{index}");
                let pixels = Arc::new(Pixmap::new(if bytes_limited { 1024 } else { 1 }, if bytes_limited { 1024 } else { 1 }).unwrap());
                cache.insert(real.clone(), &source, pixels.clone());
                held.push((real, pixels));
            }
            drop(cache);
            finish(&frame, &mut work);
            let mut cache = theme.base().0.image_cache.lock().unwrap();
            assert!(cache.get(&key, &source).is_none());
            for (real, pixels) in held {
                assert!(Arc::ptr_eq(&cache.get(&real, &source).unwrap(), &pixels));
            }
        }
    }
}
