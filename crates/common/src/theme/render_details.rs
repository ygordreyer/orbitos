use super::*;
use crate::theme::image_cache::{ImageKey, SourceIdentity, source_identity, remember_source_identity};
use std::collections::VecDeque;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc};

type Pixels = (ImageKey, Arc<Pixmap>, SourceIdentity, Arc<Pixmap>);
struct TransformJob {
    theme: ThemeHandle,
    key: ImageKey,
    source: Arc<Pixmap>,
    cancelled: Arc<AtomicBool>,
    done: mpsc::SyncSender<Result<Pixels>>,
}
thread_local! {
    static WORKER: std::cell::RefCell<Option<mpsc::SyncSender<TransformJob>>> = const { std::cell::RefCell::new(None) };
}
fn submit(job: TransformJob) -> bool {
    WORKER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let (send, recv) = mpsc::sync_channel::<TransformJob>(1);
            if std::thread::Builder::new().name("allium-details-images".into())
                .stack_size(256 * 1024).spawn(move || {
                    #[cfg(target_os = "linux")]
                    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10); }
                    while let Ok(job) = recv.recv() {
                        if job.cancelled.load(Ordering::Acquire) { continue; }
                        let identity = source_identity(&job.source);
                        let result = PreparedFrame::transform_image(&job.theme, &job.key, &job.source)
                            .map(|pixels| (job.key, job.source, identity, pixels));
                        if !job.cancelled.load(Ordering::Acquire) { let _ = job.done.try_send(result); }
                    }
                }).is_err() { return false; }
            *slot = Some(send);
        }
        slot.as_ref().unwrap().try_send(job).is_ok()
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
            if submit(TransformJob { theme: self.theme.clone(), key: key.clone(), source: source.clone(), cancelled: work.cancelled.clone(), done }) {
                work.pending = Some(pending);
            } else {
                work.images.push_front((key, source));
            }
        }
        Ok(work.images.is_empty() && work.pending.is_none())
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
