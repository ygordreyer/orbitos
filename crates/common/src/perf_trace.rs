use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    fs::{File, OpenOptions},
    io::Write,
    os::unix::io::AsRawFd,
    time::Instant,
    sync::{Arc, Mutex, OnceLock, Weak},
};

const COUNT_LEN: usize = 44;
const STAGE_NAMES: [&str; 34] = [
    "selection", "selected_info", "artwork_lookup", "decode", "layout", "raster", "present",
    "disk_cache", "details_metadata", "details_database", "details_reflow", "shadow_gaussian", "snapshot_capture", "snapshot_restore", "semantic_model",
    "frame_model", "frame_normalize", "neighbor_generation", "neighbor_filter", "neighbor_search", "retained_compare", "neighbor_misses", "neighbor_clear", "frame_retire", "frame_readiness", "page_history", "drawing_style", "compose_draw", "network_overlay",
    "frame_model_key", "frame_model_lookup", "frame_model_artwork",
    "frame_art_config", "frame_art_protection",
];
const LOG_LIMIT: u64 = 256 * 1024;
// 64 rows cover a press burst without letting incomplete content grow the queue
// indefinitely. Drain at 16 rows after presentation, even before final content.
const PENDING_LIMIT: usize = 64;
const PENDING_FLUSH_THRESHOLD: usize = 16;
static DROPPED_TRACE_LINES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[derive(Clone, Copy)]
pub enum Phase {
    Input,
    ArtLoad,
    ComponentPrep,
    Compose,
    Flush,
    Text,
    Artwork,
    Blit,
}
#[derive(Clone, Copy)]
pub enum ArtHit {
    Ram,
    Disk,
    Decode,
}
#[derive(Clone, Copy)]
pub enum Count {
    TextHit,
    TextMiss,
    TextEviction,
    MapBuild,
    CopiedPixels,
    FlushedPixels,
    ComposedPixels,
    ConvertedPixels,
    /// Actual pixel-pair inspections, including the bounded dense-damage sample.
    ComparedPixels,
    ScannedPixels,
    /// Differing inspected pairs, not an estimate of uninspected dense damage.
    ChangedPixels,
    ImageHit,
    ImageMiss,
    InkBitmapPixels,
    /// Explicit raster/snapshot buffer allocations, not a process allocator census.
    Allocations,
    PlanBuilds,
    ResizePixels,
    ShadowMisses,
    GlowMisses,
    CaptureBytes,
    RestoreBytes,
    OpacityPixels,
    PreviewMaskBytes,
    ArtAdmissionRejected,
    ArtAdmissionRejectedBytes,
    ArtRequestHero,
    ArtRequestLogo,
    ArtRequestCover,
    ArtRequestNext,
    ArtRequestEntries,
    ArtFittedHit,
    ArtFittedMiss,
    HeroHit,
    HeroMiss,
    HeroPrewarmPublish,
    PanelHit,
    PanelMiss,
    NeighborHit,
    NeighborMiss,
    NeighborBuilt,
    NeighborPointerAdopt,
    NeighborDamageCopy,
    FullFrameDetach,
    PackedSpanHit,
}
#[derive(Clone, Copy)]
struct InputTotals {
    stages: [u64; STAGE_NAMES.len()],
    cpu: [u64; 8],
    counts: [u64; COUNT_LEN],
}
struct Trace {
    counts: [u64; COUNT_LEN],
    stages: [u64; STAGE_NAMES.len()],
    frame_stages: [u64; STAGE_NAMES.len()],
    input_counts: [u64; COUNT_LEN],
    input_cpu: [u64; 8],
    generation: u64,
    input_epoch: u64,
    input_key: Option<crate::platform::Key>,
    input_edge: Option<&'static str>,
    viewport_shift: bool,
    first_feedback_us: u64,
    final_content_us: u64,
    completed_input: Option<InputTotals>,
    stage_begin: [u64; STAGE_NAMES.len()],
    stage_end: [u64; STAGE_NAMES.len()],
    frame_stage_begin: [u64; STAGE_NAMES.len()],
    frame_stage_end: [u64; STAGE_NAMES.len()],
    presented_us: u64,
    output: Arc<TraceOutput>,
    path: String,
    cpu: [u64; 8],
    hits: [u64; 3],
    started: Option<Instant>,
    render_us: u64,
    attempt: u64,
    wake_us: u64,
    input_us: u64,
    boost_us: u64,
    damage_rects: u64,
    damage_pixels: u64,
    render_reason: &'static str,
    present_reason: &'static str,
    packed_reason: &'static str,
    packed_needed_bytes: usize,
    packed_available_bytes: usize,
    neighbor_build: &'static str,
    neighbor_adopt: &'static str,
    neighbor_warm: &'static str,
    content_pending_reason: &'static str,
    content_pending_reasons: Vec<(&'static str, usize)>,
    next_cover_expected: bool,
    missing_content_rows: usize,
}
thread_local! {
    static TRACE: RefCell<Option<Trace>> = const { RefCell::new(None) };
}

pub fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::fs::metadata("/tmp/allium-perf-trace").is_ok())
}
pub fn recording() -> bool {
    TRACE.with(|trace| trace.borrow().is_some())
}
pub fn initialize() {
    thread_local! { static INITIALIZED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    if enabled() {
        INITIALIZED.with(|done| {
            if !done.replace(true) {
                let _ = install("/tmp/allium-perf-trace.log");
            }
        });
    }
}
fn install(path: &str) -> std::io::Result<()> {
    let output = trace_output(path)?;
    TRACE.with(|trace| {
        *trace.borrow_mut() = Some(Trace {
            output,
            path: path.into(),
            counts: [0; COUNT_LEN],
            stages: [0; STAGE_NAMES.len()],
            frame_stages: [0; STAGE_NAMES.len()],
            input_counts: [0; COUNT_LEN],
            input_cpu: [0; 8],
            generation: 0,
            input_epoch: 0,
            input_key: None,
            input_edge: None,
            viewport_shift: false,
            first_feedback_us: 0,
            final_content_us: 0,
            completed_input: None,
            stage_begin: [0; STAGE_NAMES.len()],
            stage_end: [0; STAGE_NAMES.len()],
            frame_stage_begin: [0; STAGE_NAMES.len()],
            frame_stage_end: [0; STAGE_NAMES.len()],
            presented_us: 0,
            cpu: [0; 8],
            hits: [0; 3],
            started: None,
            render_us: 0,
            attempt: 0,
            wake_us: 0,
            input_us: 0,
            boost_us: 0,
            damage_rects: 0,
            damage_pixels: 0,
            render_reason: "none",
            present_reason: "none",
            packed_reason: "none",
            packed_needed_bytes: 0,
            packed_available_bytes: 0,
            neighbor_build: "not_attempted",
            neighbor_adopt: "not_attempted",
            neighbor_warm: "not_evaluated",
            content_pending_reason: "not_evaluated",
            content_pending_reasons: Vec::new(),
            next_cover_expected: false,
            missing_content_rows: 0,
        })
    });
    Ok(())
}
fn thread_id() -> u64 {
    #[cfg(target_os = "linux")]
    {
        return unsafe { libc::syscall(libc::SYS_gettid) } as u64;
    }
    #[cfg(target_os = "macos")]
    {
        let mut tid = 0;
        // A null thread selects the calling thread on Darwin.
        unsafe {
            libc::pthread_threadid_np(0, &mut tid);
        }
        return tid;
    }
    #[allow(unreachable_code)]
    0
}
fn thread_cpu_ns() -> u64 {
    #[cfg(unix)]
    {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // The pointer refers to a writable timespec for the duration of the syscall.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } == 0 {
            return time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64;
        }
    }
    0
}
struct Timing {
    start: u64,
    children: u64,
}
pub struct PhaseGuard {
    phase: Phase,
    timing: Option<Timing>,
}
pub fn phase(phase: Phase) -> PhaseGuard {
    phase_at(phase, None)
}
fn phase_at(phase: Phase, input_start: Option<(u64, u64)>) -> PhaseGuard {
    let timing = TRACE.with(|trace| {
        let mut trace = trace.borrow_mut();
        let trace = trace.as_mut()?;
        if matches!(phase, Phase::Input) {
            trace.input_us = input_start.map_or_else(monotonic_us, |start| start.0);
            trace.boost_us = 0;
            trace.generation = trace.generation.wrapping_add(1);
            trace.input_epoch = trace.input_epoch.wrapping_add(1);
            trace.input_key = None;
            trace.input_edge = None;
            trace.content_pending_reason = "not_evaluated";
            trace.neighbor_adopt = "not_attempted";
            trace.content_pending_reasons.clear();
            trace.viewport_shift = false;
            trace.first_feedback_us = 0;
            trace.final_content_us = 0;
            trace.completed_input = None;
            trace.input_counts = [0; COUNT_LEN];
            trace.input_cpu = [0; 8];
            trace.cpu = [0; 8];
            trace.stages = [0; STAGE_NAMES.len()];
            trace.stage_begin = [0; STAGE_NAMES.len()];
            trace.stage_end = [0; STAGE_NAMES.len()];
        }
        Some(Timing {
            start: input_start.map_or_else(thread_cpu_ns, |start| start.1),
            children: trace.cpu.iter().sum(),
        })
    });
    PhaseGuard { phase, timing }
}
pub fn input(event: crate::platform::KeyEvent) -> PhaseGuard {
    input_at(event, None, 0)
}
pub fn input_with_boost(event: crate::platform::KeyEvent, boost: impl FnOnce()) -> PhaseGuard {
    let start = recording().then(|| {
        let cpu = thread_cpu_ns();
        (monotonic_us(), cpu)
    });
    boost();
    let boost_us = start.map_or(0, |start| monotonic_us().saturating_sub(start.0));
    input_at(event, start, boost_us)
}
#[derive(Clone, Copy)]
struct BoostWorkTiming {
    start: u64,
    lease: [u64; 2],
    sysfs: [u64; 2],
}

pub struct BoostWork {
    timing: Option<BoostWorkTiming>,
}

/// Worker-local timings never mutate the input thread's trace or extend boost_us.
pub fn boost_work() -> BoostWork {
    BoostWork {
        timing: recording().then(|| BoostWorkTiming {
            start: monotonic_us(),
            lease: [0; 2],
            sysfs: [0; 2],
        }),
    }
}

impl BoostWork {
    pub fn lease<T>(&mut self, work: impl FnOnce() -> T) -> T {
        let Some(timing) = self.timing.as_mut() else { return work(); };
        timing.lease[0] = monotonic_us();
        let result = work();
        timing.lease[1] = monotonic_us();
        result
    }

    pub fn sysfs<T>(&mut self, work: impl FnOnce() -> T) -> T {
        let Some(timing) = self.timing.as_mut() else { return work(); };
        timing.sysfs[0] = monotonic_us();
        let result = work();
        timing.sysfs[1] = monotonic_us();
        result
    }
}

impl Drop for BoostWork {
    fn drop(&mut self) {
        let Some(timing) = &self.timing else { return; };
        TRACE.with(|state| {
            if let Some(t) = state.borrow_mut().as_mut() {
                defer_event(t, "boost_completed", 0, Some(*timing));
            }
        });
    }
}

fn input_at(event: crate::platform::KeyEvent, start: Option<(u64, u64)>, boost_us: u64) -> PhaseGuard {
    use crate::platform::KeyEvent;
    let guard = phase_at(Phase::Input, start);
    if guard.timing.is_none() { return guard; }
    TRACE.with(|state| {
        if let Some(t) = state.borrow_mut().as_mut() {
            let (key, edge) = match event {
                KeyEvent::Pressed(key) => (key, "press"),
                KeyEvent::Released(key) => (key, "release"),
                KeyEvent::Autorepeat(key) => (key, "autorepeat"),
            };
            t.input_key = Some(key);
            t.input_edge = Some(edge);
            t.boost_us = boost_us;
            defer_event(t, "input_received", t.generation, None);
        }
    });
    guard
}
impl Drop for PhaseGuard {
    fn drop(&mut self) {
        let Some(timing) = &self.timing else {
            return;
        };
        let duration = thread_cpu_ns().saturating_sub(timing.start);
        TRACE.with(|trace| {
            if let Some(trace) = trace.borrow_mut().as_mut() {
                let children = trace
                    .cpu
                    .iter()
                    .sum::<u64>()
                    .saturating_sub(timing.children);
                let elapsed = duration.saturating_sub(children);
                trace.cpu[self.phase as usize] += elapsed;
                trace.input_cpu[self.phase as usize] += elapsed;
            }
        });
    }
}
pub fn hit(hit: ArtHit) {
    TRACE.with(|trace| {
        if let Some(trace) = trace.borrow_mut().as_mut() {
            trace.hits[hit as usize] += 1;
        }
    });
}
pub fn count(kind: Count, amount: u64) {
    TRACE.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.counts[kind as usize] += amount;
            t.input_counts[kind as usize] += amount;
        }
    });
}
fn monotonic_us() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time);
    }
    time.tv_sec as u64 * 1_000_000 + time.tv_nsec as u64 / 1000
}
pub fn wake() {
    TRACE.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.wake_us = monotonic_us();
        }
    });
}
pub fn begin_render() {
    TRACE.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.cpu = [0; 8];
            t.frame_stages = [0; STAGE_NAMES.len()];
            t.frame_stage_begin = [0; STAGE_NAMES.len()];
            t.frame_stage_end = [0; STAGE_NAMES.len()];
            t.presented_us = 0;
            t.hits = [0; 3];
            t.counts = [0; COUNT_LEN];
            t.started = Some(Instant::now());
            t.render_us = monotonic_us();
            t.attempt += 1;
            t.damage_rects = 0;
            t.damage_pixels = 0;
            t.render_reason = "none";
            t.present_reason = "none";
            t.packed_reason = "none";
            t.packed_needed_bytes = 0;
            t.packed_available_bytes = 0;
        }
    });
}
pub fn end_render() {
    TRACE.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.started = None;
        }
    });
}
pub fn drawn_frame() {
    TRACE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(trace) = state.as_mut() else {
            return;
        };
        let mut row = serde_json::json!({
            "thread": std::thread::current().name(),
            "stages": stage_values(trace),
            "frontend_io_ops": FRONTEND_IO_OPS.with(std::cell::Cell::get),
            "text_hits":trace.counts[0], "text_misses":trace.counts[1], "text_evictions":trace.counts[2],
            "blit_map_builds":trace.counts[3], "copied_pixels":trace.counts[4], "flushed_pixels":trace.counts[5],
            "composed_pixels": trace.counts[6], "ink_bitmap_pixels": trace.counts[13], "legacy_composed_pixels": trace.counts[6].saturating_sub(trace.counts[13]), "converted_pixels": trace.counts[7],
            "compared_pixels": trace.counts[8], "scanned_pixels": trace.counts[9],
            "changed_pixels":trace.counts[10],
            "image_hits":trace.counts[11], "image_misses":trace.counts[12],
            "damage_rects":trace.damage_rects,"damage_pixels":trace.damage_pixels,
            "render_reason":trace.render_reason,"full_present_reason":trace.present_reason,
            "input_cpu_us": trace.input_cpu[0]/1000, "art_load_cpu_us": trace.cpu[1]/1000,
            "component_prep_cpu_us": trace.cpu[2]/1000, "compose_paint_cpu_us": trace.cpu[3]/1000,
            "flush_cpu_us": trace.cpu[4]/1000,
            "render_cpu_us": trace.cpu[1..].iter().sum::<u64>()/1000,
            "text_cpu_us": trace.cpu[5]/1000, "artwork_cpu_us": trace.cpu[6]/1000, "blit_cpu_us": trace.cpu[7]/1000,
            "wake_monotonic_us": trace.wake_us, "input_monotonic_us": trace.input_us,
            "render_start_monotonic_us": trace.render_us,
            "render_end_monotonic_us": monotonic_us(), "render_attempt": trace.attempt,
            "ram_hit": trace.hits[0], "disk_cache_hit": trace.hits[1], "png_decode": trace.hits[2],
            "wall_us": trace.started.map_or(0, |start| start.elapsed().as_micros()),
        });
        row["packed_span_reason"] = trace.packed_reason.into();
        row["packed_span_needed_bytes"] = trace.packed_needed_bytes.into();
        row["packed_span_available_bytes"] = trace.packed_available_bytes.into();
        row["packed_span_hit"] = trace.counts[43].into();
        row["art_fitted_hits"] = trace.counts[30].into();
        row["art_fitted_misses"] = trace.counts[31].into();
        row["event"] = "frame".into();
        row["pid"] = std::process::id().into();
        row["measurement"] = measurement_values(trace);
        if append_row(trace, row, true).is_err() {
            *state = None;
            return;
        }
        trace.cpu = [0; 8];
        trace.hits = [0; 3];
        trace.started = None;
    });
}

// Writers for one log share a FIFO, including the boost thread. A deferred
// record prevents later rows from overtaking it until presentation or idle.
struct TraceOutput {
    // Never hold this lock during JSON construction, serialization or file I/O.
    pending: Mutex<VecDeque<PendingRow>>,
    writer: Mutex<TraceWriter>,
}
struct TraceWriter {
    file: File,
    last_write: [u64; 2],
}
enum PendingRow {
    Snapshot(EventSnapshot, Vec<(&'static str, usize)>),
    Ready(serde_json::Value, bool),
}
struct EventSnapshot {
    event: &'static str,
    monotonic_us: u64,
    thread_cpu_ns: u64,
    pid: u32,
    tid: u64,
    generation: u64,
    measurement: MeasurementSnapshot,
    boost: Option<BoostWorkTiming>,
}
impl EventSnapshot {
    fn values(self, reasons: &[(&'static str, usize)]) -> serde_json::Value {
        let mut row = serde_json::json!({
            "event": self.event, "monotonic_us": self.monotonic_us,
            "thread_cpu_ns": self.thread_cpu_ns, "pid": self.pid, "tid": self.tid,
            "selection_generation": self.generation,
            "viewport_shift": self.measurement.viewport_shift, "task": null,
            "measurement": self.measurement.values(reasons),
        });
        if let Some(timing) = self.boost {
            let m = &mut row["measurement"];
            m["boost_started_monotonic_us"] = timing.start.into();
            m["boost_lease_us"] = timing.lease[1].saturating_sub(timing.lease[0]).into();
            m["boost_sysfs_us"] = timing.sysfs[1].saturating_sub(timing.sysfs[0]).into();
            m["boost_lease_begin_monotonic_us"] = timing.lease[0].into();
            m["boost_lease_end_monotonic_us"] = timing.lease[1].into();
            m["boost_sysfs_begin_monotonic_us"] = timing.sysfs[0].into();
            m["boost_sysfs_end_monotonic_us"] = timing.sysfs[1].into();
        }
        row
    }
}
fn trace_output(path: &str) -> std::io::Result<Arc<TraceOutput>> {
    static OUTPUTS: OnceLock<Mutex<HashMap<String, Weak<TraceOutput>>>> = OnceLock::new();
    let mut outputs = OUTPUTS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(output) = outputs.get(path).and_then(Weak::upgrade) {
        return Ok(output);
    }
    let output = Arc::new(TraceOutput {
        pending: Mutex::new(VecDeque::with_capacity(PENDING_LIMIT)),
        writer: Mutex::new(TraceWriter {
            file: OpenOptions::new().create(true).append(true).open(path)?,
            last_write: [0; 2],
        }),
    });
    outputs.retain(|_, output| output.strong_count() != 0);
    outputs.insert(path.into(), Arc::downgrade(&output));
    Ok(output)
}
fn defer_event(t: &Trace, event: &'static str, generation: u64, boost: Option<BoostWorkTiming>) {
    #[cfg(test)]
    SNAPSHOTS.with(|count| count.set(count.get() + 1));
    let snapshot = EventSnapshot {
        event, monotonic_us: monotonic_us(), thread_cpu_ns: thread_cpu_ns(),
        pid: std::process::id(), tid: thread_id(), generation,
        measurement: MeasurementSnapshot::capture(t), boost,
    };
    // Input has just cleared these reasons; boost workers also normally have none.
    // Preserve them for callers that use boost_work on a foreground trace.
    let reasons = t.content_pending_reasons.clone();
    TraceOutput::enqueue(&mut t.output.pending.lock().unwrap_or_else(|e| e.into_inner()),
        PendingRow::Snapshot(snapshot, reasons));
}
fn append_row(t: &Trace, row: serde_json::Value, fatal: bool) -> std::io::Result<()> {
    // Take the writer first to preserve direct-write order, but never wait for it.
    let mut writer = match t.output.writer.try_lock() {
        Ok(writer) => writer,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            TraceOutput::enqueue(&mut t.output.pending.lock().unwrap_or_else(|e| e.into_inner()),
                PendingRow::Ready(row, fatal));
            return Ok(());
        }
    };
    let mut pending = t.output.pending.lock().unwrap_or_else(|e| e.into_inner());
    if pending.is_empty() {
        drop(pending);
        writer.write(row)
    } else {
        TraceOutput::enqueue(&mut pending, PendingRow::Ready(row, fatal));
        Ok(())
    }
}
impl TraceOutput {
    fn enqueue(pending: &mut VecDeque<PendingRow>, row: PendingRow) {
        if pending.len() == PENDING_LIMIT {
            DROPPED_TRACE_LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            pending.push_back(row);
        }
    }
    fn flush_if_full(&self) {
        let full = self.pending.lock().unwrap_or_else(|e| e.into_inner()).len() >= PENDING_FLUSH_THRESHOLD;
        if full { let _ = self.flush(); }
    }
    fn flush(&self) -> std::io::Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        // Bound each drain even if another producer keeps adding records.
        let count = self.pending.lock().unwrap_or_else(|e| e.into_inner()).len();
        let mut result = Ok(());
        for _ in 0..count {
            let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
            let Some(pending) = pending else { break; };
            let begin = monotonic_us();
            let (row, fatal) = match pending {
                PendingRow::Snapshot(snapshot, reasons) => (snapshot.values(&reasons), false),
                PendingRow::Ready(row, fatal) => (row, fatal),
            };
            if let Err(error) = writer.write_since(row, begin) {
                if fatal { result = Err(error); }
            }
        }
        result
    }
}
impl TraceWriter {
    fn write(&mut self, row: serde_json::Value) -> std::io::Result<()> {
        self.write_since(row, monotonic_us())
    }
    fn write_since(&mut self, mut row: serde_json::Value, begin: u64) -> std::io::Result<()> {
        // The following row exposes the preceding serialization and append cost.
        row["trace_write_begin_us"] = self.last_write[0].into();
        row["trace_write_end_us"] = self.last_write[1].into();
        if let Some(m) = row.get_mut("measurement") {
            m["dropped_trace_lines"] = DROPPED_TRACE_LINES.load(std::sync::atomic::Ordering::Relaxed).into();
        }
        #[cfg(test)]
        SERIALIZATIONS.with(|count| count.set(count.get() + 1));
        let line = row.to_string() + "\n";
        let result = append_bounded(&mut self.file, line.as_bytes());
        self.last_write = [begin, monotonic_us()];
        result
    }
}

impl Drop for TraceOutput {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}
#[cfg(test)]
thread_local! {
    static SERIALIZATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static SNAPSHOTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
/// Drain at the next idle boundary, including inputs that never produced a frame.
pub fn flush_pending() {
    TRACE.with(|state| {
        let mut state = state.borrow_mut();
        let failed = state.as_ref().is_some_and(|t| {
            t.output.flush().is_err()
        });
        if failed { *state = None; }
    });
}
pub struct FlushPendingOnDrop;
impl Drop for FlushPendingOnDrop {
    fn drop(&mut self) { flush_pending(); }
}

fn append_bounded(file: &mut File, line: &[u8]) -> std::io::Result<()> {
    append_line(file, line, true)
}
fn append_line(file: &mut File, line: &[u8], rolling: bool) -> std::io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            DROPPED_TRACE_LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(());
        }
        return Err(error);
    }
    let result = (|| {
        if file.metadata()?.len().saturating_add(line.len() as u64) > LOG_LIMIT {
            if !rolling || line.len() as u64 > LOG_LIMIT {
                return Err(std::io::Error::other("trace full"));
            }
            file.set_len(0)?;
        }
        file.write_all(line)
    })();
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
    result
}

pub fn network_event(event: &'static str, cause: &'static str, active: Option<bool>) {
    if !enabled() {
        return;
    }
    thread_local! {
        static NETWORK: RefCell<Option<File>> = RefCell::new(
            OpenOptions::new().create(true).append(true).open("/tmp/allium-network-trace.log").ok()
        );
    }
    NETWORK.with(|state| {
        let mut state = state.borrow_mut();
        let Some(file) = state.as_mut() else { return };
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // A process-wide monotonic clock joins worker and UI events.
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time);
        }
        let line = serde_json::json!({
            "event": event, "cause": cause, "active": active,
            "monotonic_us": time.tv_sec as u64 * 1_000_000 + time.tv_nsec as u64 / 1000,
            "pid": std::process::id(), "thread": format!("{:?}", std::thread::current().id()),
        })
        .to_string()
            + "\n";
        // Keep the completion window even when verification outlasts the log cap.
        if append_line(file, line.as_bytes(), true).is_err() {
            *state = None;
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn foreground_tasks_queue_without_waiting_for_writer_and_flush_in_order() {
        let path = std::env::temp_dir().join(format!("allium-task-writer-lock-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        drop(task("before_lock", 7));
        let output = TRACE.with(|t| t.borrow().as_ref().unwrap().output.clone());
        let (locked, wait_locked) = std::sync::mpsc::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _writer = output.writer.lock().unwrap();
            locked.send(()).unwrap();
            // Bound failure time if a regression blocks the foreground caller.
            let _ = wait_release.recv_timeout(std::time::Duration::from_secs(2));
        });
        wait_locked.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        drop(task("media_adopt", 7));
        drop(task("while_locked", 7));
        let elapsed = start.elapsed();
        let while_locked = std::fs::read_to_string(&path).unwrap();
        let _ = release.send(());
        holder.join().unwrap();
        drop(task("after_lock", 7));
        let before_flush = std::fs::read_to_string(&path).unwrap();
        flush_pending();
        let data = std::fs::read_to_string(&path).unwrap();
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
        assert!(elapsed < std::time::Duration::from_millis(100), "foreground tasks waited {elapsed:?}");
        assert_eq!(while_locked.lines().count(), 1);
        assert_eq!(before_flush, while_locked, "later tasks must not overtake queued rows");
        let rows: Vec<serde_json::Value> = data.lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(rows.iter().map(|row| row["task"]["name"].as_str().unwrap()).collect::<Vec<_>>(),
            ["before_lock", "media_adopt", "while_locked", "after_lock"]);
        assert!(rows.iter().all(|row| row["event"] == "task_completed" && row["selection_generation"] == 7));
    }
    #[test]
    fn wave2_contended_append_drops_whole_record_and_later_frame_reports_it() {
        let path = std::env::temp_dir().join(format!("allium-wave2-lock-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        let before = probe_counts()["measurement"]["dropped_trace_lines"].as_u64().unwrap_or(0);
        let holder = OpenOptions::new().append(true).open(&path).unwrap();
        assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) }, 0);
        let (done, wait) = std::sync::mpsc::channel();
        let target = path.clone();
        let worker = std::thread::spawn(move || {
            let mut file = OpenOptions::new().append(true).open(target).unwrap();
            let start = Instant::now();
            append_line(&mut file, b"{\"event\":\"must_drop\"}\n", true).unwrap();
            done.send(start.elapsed()).unwrap();
        });
        let elapsed = match wait.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(elapsed) => elapsed,
            Err(error) => {
                assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_UN) }, 0);
                worker.join().unwrap();
                panic!("contended append must complete while the lock is held: {error}");
            }
        };
        assert!(elapsed < std::time::Duration::from_millis(100), "contended append waited {elapsed:?}");
        let while_locked = std::fs::read_to_string(&path).unwrap();
        assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_UN) }, 0);
        worker.join().unwrap();
        let after_append = std::fs::read_to_string(&path).unwrap();
        drawn_frame();
        let data = std::fs::read_to_string(&path).unwrap();
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
        assert!(while_locked.is_empty());
        assert!(after_append.is_empty(), "a dropped record must not publish any bytes");
        let row: serde_json::Value = serde_json::from_str(data.trim()).unwrap();
        assert!(row["measurement"]["dropped_trace_lines"].as_u64().unwrap() > before);
    }
    #[test]
    fn dl12_render_wall_excludes_input_wait_and_each_attempt_resets() {
        let path =
            std::env::temp_dir().join(format!("allium-dl12-wall-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        wake();
        {
            let _input = phase(Phase::Input);
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
        begin_render();
        {
            let _compose = phase(Phase::Compose);
            let _text = phase(Phase::Text);
            let _blit = phase(Phase::Blit);
        }
        drawn_frame();
        end_render();
        std::thread::sleep(std::time::Duration::from_millis(30));
        begin_render();
        drawn_frame();
        end_render();
        let data = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = data
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        for (at, line) in lines.iter().enumerate() {
            assert!(line["wall_us"].as_u64().unwrap() < 20_000);
            assert_eq!(line["render_attempt"], at + 1);
            assert!(
                line["render_start_monotonic_us"].as_u64().unwrap()
                    > line["input_monotonic_us"].as_u64().unwrap()
            );
            assert!(
                line["render_end_monotonic_us"].as_u64().unwrap()
                    >= line["render_start_monotonic_us"].as_u64().unwrap()
            );
        }
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn network_rollover_retains_terminal_event_and_complete_lines() {
        let path = std::env::temp_dir().join(format!(
            "allium-network-rollover-{}.log",
            std::process::id()
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .unwrap();
        for _ in 0..20000 {
            append_line(
                &mut file,
                b"{\"event\":\"wake\",\"cause\":\"timer\"}\n",
                true,
            )
            .unwrap();
        }
        append_line(&mut file, b"{\"event\":\"frame_presented\"}\n", true).unwrap();
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(data.len() as u64 <= LOG_LIMIT);
        assert!(data.lines().count() < 20000);
        for line in data.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
        assert!(data.ends_with("{\"event\":\"frame_presented\"}\n"));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn disabled_trace_and_bounded_complete_frame_lines() {
        let path =
            std::env::temp_dir().join(format!("allium-trace-test-{}.log", std::process::id()));
        {
            let _disabled = phase(Phase::Input);
            hit(ArtHit::Ram);
        }
        let disabled = input(crate::platform::KeyEvent::Pressed(crate::platform::Key::B));
        assert!(disabled.timing.is_none());
        drop(disabled);
        content_readiness("rows", false, 1);
        artwork_readiness(&crate::theme::frame::Art::default(), &crate::theme::frame::Art::default());
        assert!(!path.exists());
        install(path.to_str().unwrap()).unwrap();
        {
            let _input = phase(Phase::Input);
            let _art = phase(Phase::ArtLoad);
            hit(ArtHit::Disk);
        }
        drawn_frame();
        let text = std::fs::read_to_string(&path).unwrap();
        let line: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(line["disk_cache_hit"], 1);
        for _ in 0..2000 {
            drawn_frame();
        }
        assert!(std::fs::metadata(&path).unwrap().len() <= LOG_LIMIT);
        assert!(TRACE.with(|trace| trace.borrow().is_some()));
        for line in std::fs::read_to_string(&path).unwrap().lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn ux4_menu_phase_lines_share_the_existing_log_cap() {
        initialize();
        let path =
            std::env::temp_dir().join(format!("allium-menu-trace-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        for _ in 0..10000 {
            let _phase = menu_phase("first_frame_prepare");
        }
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(data.len() as u64 <= LOG_LIMIT);
        assert!(TRACE.with(|trace| trace.borrow().is_some()));
        for line in data.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["menu_phase"], "first_frame_prepare");
            assert!(value["wall_us"].is_number() && value["cpu_us"].is_number());
        }
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn full_existing_frame_log_installs_and_rolls_repeatedly() {
        let path =
            std::env::temp_dir().join(format!("allium-trace-full-{}.log", std::process::id()));
        let file = File::create(&path).unwrap();
        file.set_len(LOG_LIMIT + 1).unwrap();
        drop(file);
        install(path.to_str().unwrap()).unwrap();
        let mut previous = LOG_LIMIT + 1;
        let mut rollovers = 0;
        for _ in 0..2000 {
            begin_render();
            drawn_frame();
            let length = std::fs::metadata(&path).unwrap().len();
            assert!(length <= LOG_LIMIT);
            if length < previous {
                rollovers += 1;
            }
            previous = length;
        }
        assert!(rollovers >= 3);
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(data.ends_with('\n'));
        let lines: Vec<serde_json::Value> = data
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.last().unwrap()["render_attempt"], 2000);
        assert!(
            lines
                .windows(2)
                .all(|pair| pair[1]["render_attempt"].as_u64().unwrap()
                    == pair[0]["render_attempt"].as_u64().unwrap() + 1)
        );
        assert!(TRACE.with(|t| t.borrow().is_some()));
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn trace_process_writer() {
        let Some(path) = std::env::var_os("ALLIUM_TRACE_PROCESS_TEST") else {
            return;
        };
        install(path.to_str().unwrap()).unwrap();
        for _ in 0..2000 {
            drawn_frame();
        }
    }
    #[test]
    fn log_cap_holds_for_independent_processes_and_stale_open_handles() {
        let path = std::env::temp_dir().join(format!(
            "allium-trace-multiprocess-{}.log",
            std::process::id()
        ));
        let mut processes = vec![];
        for _ in 0..4 {
            processes.push(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "perf_trace::tests::trace_process_writer",
                        "--test-threads=1",
                    ])
                    .env("ALLIUM_TRACE_PROCESS_TEST", &path)
                    .spawn()
                    .unwrap(),
            );
        }
        for mut child in processes {
            assert!(child.wait().unwrap().success());
        }
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(!data.is_empty());
        assert!(data.len() as u64 <= LOG_LIMIT);
        for line in data.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
        std::fs::remove_file(path).unwrap();
        let path =
            std::env::temp_dir().join(format!("allium-trace-stale-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(LOG_LIMIT)
            .unwrap();
        drawn_frame();
        assert!(std::fs::metadata(&path).unwrap().len() < LOG_LIMIT);
        assert!(TRACE.with(|t| t.borrow().is_some()));
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(data.ends_with('\n'));
        serde_json::from_str::<serde_json::Value>(data.trim()).unwrap();
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }
}

pub struct MenuPhaseGuard {
    name: &'static str,
    started: Option<Instant>,
    cpu: u64,
}
pub fn menu_phase(name: &'static str) -> MenuPhaseGuard {
    initialize();
    let enabled = TRACE.with(|trace| trace.borrow().is_some());
    MenuPhaseGuard {
        name,
        started: enabled.then(Instant::now),
        cpu: if enabled { thread_cpu_ns() } else { 0 },
    }
}
impl Drop for MenuPhaseGuard {
    fn drop(&mut self) {
        let Some(started) = self.started else {
            return;
        };
        let cpu = thread_cpu_ns().saturating_sub(self.cpu);
        TRACE.with(|state| {
            let mut state = state.borrow_mut();
            let Some(trace) = state.as_mut() else {
                return;
            };
            let row = serde_json::json!({
                "menu_phase": self.name, "wall_us": started.elapsed().as_micros(),
                "cpu_us": cpu / 1000,
            });
            if append_row(trace, row, true).is_err() {
                *state = None;
            }
        });
    }
}

#[derive(Clone, Copy)]
struct OpenContext {
    session: u64,
    released: Instant,
    record: fn(u64, Instant, &'static str, Option<&'static str>),
}
thread_local! {
    static OPEN_CONTEXT: std::cell::Cell<Option<OpenContext>> = const { std::cell::Cell::new(None) };
}

pub struct OpenScope {
    previous: Option<OpenContext>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
pub fn open_scope(
    session: u64,
    released: Instant,
    record: fn(u64, Instant, &'static str, Option<&'static str>),
) -> OpenScope {
    let previous = OPEN_CONTEXT.with(|context| {
        context.replace(Some(OpenContext {
            session,
            released,
            record,
        }))
    });
    OpenScope {
        previous,
        _thread: std::marker::PhantomData,
    }
}
impl Drop for OpenScope {
    fn drop(&mut self) {
        OPEN_CONTEXT.with(|context| context.set(self.previous));
    }
}
pub struct OpenPhaseGuard {
    context: Option<OpenContext>,
    name: &'static str,
}
pub fn open_phase(name: &'static str) -> OpenPhaseGuard {
    let context = OPEN_CONTEXT.with(|context| context.get());
    if let Some(c) = context {
        (c.record)(c.session, c.released, "phase_begin", Some(name));
    }
    OpenPhaseGuard { context, name }
}
impl Drop for OpenPhaseGuard {
    fn drop(&mut self) {
        if let Some(c) = self.context {
            (c.record)(c.session, c.released, "phase_end", Some(self.name));
        }
    }
}

#[cfg(test)]
mod open_scope_tests {
    use super::*;
    #[test]
    fn framebuffer_subphases_keep_session_release_and_restore_nested_scope() {
        static EVENTS: std::sync::Mutex<Vec<(u64, Instant, &'static str, Option<&'static str>)>> =
            std::sync::Mutex::new(Vec::new());
        fn record(id: u64, released: Instant, event: &'static str, name: Option<&'static str>) {
            EVENTS.lock().unwrap().push((id, released, event, name));
        }
        let released = Instant::now();
        {
            let _outside = open_phase("outside");
        }
        {
            let _scope = open_scope(91, released, record);
            for name in [
                "capture_bulk_read",
                "capture_convert",
                "page_normalize",
                "compose",
                "present_convert",
                "present_write",
            ] {
                let _phase = open_phase(name);
            }
            {
                let _inner = open_scope(92, released, record);
                let _phase = open_phase("nested");
            }
            {
                let _phase = open_phase("restored");
            }
        }
        {
            let _outside = open_phase("outside");
        }
        let events = EVENTS.lock().unwrap();
        assert_eq!(events.len(), 16);
        for pair in events.chunks_exact(2) {
            assert_eq!(pair[0].2, "phase_begin");
            assert_eq!(pair[1].2, "phase_end");
            assert_eq!(pair[0].0, pair[1].0);
            assert_eq!(pair[0].1, released);
        }
        assert_eq!(events[12].0, 92);
        assert_eq!(events[14].0, 91);
    }
}

pub fn render_reason(reason: &'static str) {
    TRACE.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.render_reason = reason;
        }
    });
}
pub fn packed_span_reason(reason: &'static str) {
    TRACE.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() { t.packed_reason = reason; }
    });
}

pub fn packed_span_budget(needed: usize, available: usize) {
    TRACE.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.packed_needed_bytes = needed;
            t.packed_available_bytes = available;
        }
    });
}

pub fn present_reason(reason: &'static str) {
    TRACE.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.present_reason = reason;
        }
    });
}
// Build/warm outcomes survive idle frames and input reset so the next press explains misses.
pub fn neighbor_build(reason: &'static str) {
    TRACE.with(|t| { if let Some(t) = t.borrow_mut().as_mut() { t.neighbor_build = reason; } });
}
pub fn neighbor_adopt(reason: &'static str) {
    TRACE.with(|t| { if let Some(t) = t.borrow_mut().as_mut() { t.neighbor_adopt = reason; } });
}
pub fn neighbor_warm(reason: &'static str) {
    TRACE.with(|t| { if let Some(t) = t.borrow_mut().as_mut() { t.neighbor_warm = reason; } });
}
pub fn damage(areas: &[crate::geom::Rect]) {
    TRACE.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.damage_rects = areas.len() as u64;
            t.damage_pixels = areas.iter().map(|r| r.w as u64 * r.h as u64).sum();
        }
    });
}
#[cfg(any(test, feature = "perf-bench"))]
pub fn start_probe(path: &str) {
    install(path).unwrap();
}
#[cfg(any(test, feature = "perf-bench"))]
pub fn stop_probe() {
    TRACE.with(|trace| *trace.borrow_mut() = None);
}
#[cfg(any(test, feature = "perf-bench"))]
pub fn probe_counts() -> serde_json::Value {
    TRACE.with(|t| {
        let t = t.borrow(); let t = t.as_ref().unwrap();
        serde_json::json!({"measurement":measurement_values(t),"stages":stage_values(t),"compose_paint_cpu_us":t.cpu[3]/1000,"render_cpu_us":t.cpu[1..].iter().sum::<u64>()/1000,"composed_pixels":t.counts[6],"ink_bitmap_pixels":t.counts[13],"legacy_composed_pixels":t.counts[6].saturating_sub(t.counts[13]),"copied_pixels":t.counts[4],
            "converted_pixels":t.counts[7],"flushed_pixels":t.counts[5],
            "compared_pixels":t.counts[8],"scanned_pixels":t.counts[9],"changed_pixels":t.counts[10],
            "art_fitted_hits":t.counts[30],"art_fitted_misses":t.counts[31],
            "hero_hits":t.counts[32],"hero_misses":t.counts[33],"hero_prewarm_publishes":t.counts[34],
            "panel_hits":t.counts[35],"panel_misses":t.counts[36],
            "neighbor_hits":t.counts[37],"neighbor_misses":t.counts[38],"neighbor_built":t.counts[39],
            "neighbor_pointer_adopts":t.counts[40],"neighbor_damage_copies":t.counts[41],"full_frame_detaches":t.counts[42],"packed_span_hit":t.counts[43],
            "image_hits":t.counts[11],"image_misses":t.counts[12],
            "text_hits":t.counts[0],"text_misses":t.counts[1],"blit_map_builds":t.counts[3],
            "damage_rects":t.damage_rects,"damage_pixels":t.damage_pixels,
            "render_reason":t.render_reason,"full_present_reason":t.present_reason,"packed_span_reason":t.packed_reason,
            "packed_span_needed_bytes":t.packed_needed_bytes,"packed_span_available_bytes":t.packed_available_bytes})
    })
}

thread_local! {
    static FRONTEND_IO_OPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
pub fn frontend_io_op() {
    FRONTEND_IO_OPS.with(|count| count.set(count.get().wrapping_add(1)));
}
#[cfg(any(test, feature = "perf-bench"))]
pub fn frontend_io_ops() -> u64 {
    FRONTEND_IO_OPS.with(std::cell::Cell::get)
}

/// Inclusive wall times and process-wide monotonic endpoints for one input/frame.
#[derive(Clone, Copy)]
pub enum Stage {
    Selection,
    SelectedInfo,
    ArtworkLookup,
    Decode,
    Layout,
    Raster,
    Present,
    DiskCache,
    DetailsMetadata,
    DetailsDatabase,
    DetailsReflow,
    ShadowGaussian,
    SnapshotCapture,
    SnapshotRestore,
    SemanticModel,
    FrameModel,
    FrameNormalize,
    NeighborGeneration,
    NeighborFilter,
    NeighborSearch,
    RetainedCompare,
    NeighborMisses,
    NeighborClear,
    FrameRetire,
    FrameReadiness,
    PageHistory,
    DrawingStyle,
    ComposeDraw,
    NetworkOverlay,
    FrameModelKey,
    FrameModelLookup,
    FrameModelArtwork,
    FrameArtConfig,
    FrameArtProtection,
}
pub struct StageGuard {
    stage: Stage,
    start: Option<u64>,
    frame_attempt: u64,
    input_epoch: u64,
}
pub fn stage(stage: Stage) -> StageGuard {
    let (start, frame_attempt, input_epoch) = TRACE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(trace) = state.as_mut() else {
            return (None, 0, 0);
        };
        let now = monotonic_us();
        if trace.stage_begin[stage as usize] == 0 {
            trace.stage_begin[stage as usize] = now;
        }
        if trace.started.is_some() && trace.frame_stage_begin[stage as usize] == 0 {
            trace.frame_stage_begin[stage as usize] = now;
        }
        (
            Some(now),
            if trace.started.is_some() {
                trace.attempt
            } else {
                0
            },
            trace.input_epoch,
        )
    });
    StageGuard {
        stage,
        start,
        frame_attempt,
        input_epoch,
    }
}
impl Drop for StageGuard {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            let end = monotonic_us();
            TRACE.with(|state| {
                if let Some(trace) = state.borrow_mut().as_mut() {
                    if trace.input_epoch == self.input_epoch {
                        trace.stages[self.stage as usize] += end.saturating_sub(start);
                        trace.stage_end[self.stage as usize] = end;
                        if self.frame_attempt != 0 && self.frame_attempt == trace.attempt {
                            trace.frame_stages[self.stage as usize] += end.saturating_sub(start);
                            trace.frame_stage_end[self.stage as usize] = end;
                        }
                    }
                }
            });
        }
    }
}
fn stage_values(trace: &Trace) -> serde_json::Value {
    let mut stages = serde_json::Map::new();
    for (i, name) in STAGE_NAMES.iter().enumerate()
    {
        stages.insert((*name).into(), serde_json::json!({"wall_us":trace.stages[i],"begin_monotonic_us":trace.stage_begin[i],"end_monotonic_us":trace.stage_end[i]}));
    }
    serde_json::json!({"input_received_monotonic_us":trace.input_us,"phases":stages})
}

#[cfg(any(test, feature = "perf-bench"))]
pub fn probe_counts_if_enabled() -> serde_json::Value {
    if TRACE.with(|t| t.borrow().is_some()) {
        probe_counts()
    } else {
        serde_json::Value::Null
    }
}

#[cfg(any(test, feature = "perf-bench"))]
pub fn probe_path() -> Option<String> {
    TRACE.with(|state| state.borrow().as_ref().map(|trace| trace.path.clone()))
}

#[cfg(test)]
mod latency_stage_tests {
    use super::*;
    #[test]
    fn right_trace_preserves_input_stages_across_begin_render() {
        let path =
            std::env::temp_dir().join(format!("allium-right-stages-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        {
            let _input = phase(Phase::Input);
            let _selection = stage(Stage::Selection);
            for which in [Stage::DetailsMetadata, Stage::DetailsDatabase, Stage::DetailsReflow] {
                let _stage = stage(which);
            }
        }
        begin_render();
        for which in [
            Stage::SelectedInfo,
            Stage::ArtworkLookup,
            Stage::Layout,
            Stage::SemanticModel,
            Stage::FrameModel,
            Stage::FrameNormalize,
            Stage::NeighborGeneration,
            Stage::NeighborFilter,
            Stage::NeighborSearch,
            Stage::RetainedCompare,
            Stage::NeighborMisses,
            Stage::NeighborClear,
            Stage::FrameRetire,
            Stage::FrameReadiness,
            Stage::PageHistory,
            Stage::DrawingStyle,
            Stage::ComposeDraw,
            Stage::NetworkOverlay,
            Stage::FrameModelKey,
            Stage::FrameModelLookup,
            Stage::FrameModelArtwork,
            Stage::FrameArtConfig,
            Stage::FrameArtProtection,
            Stage::Raster,
            Stage::Present,
            Stage::ShadowGaussian,
            Stage::SnapshotCapture,
            Stage::SnapshotRestore,
        ] {
            let _stage = stage(which);
        }
        drawn_frame();
        let row: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        let input = row["stages"]["input_received_monotonic_us"]
            .as_u64()
            .unwrap();
        let phases = &row["stages"]["phases"];
        assert!(input > 0);
        for name in [
            "shadow_gaussian",
            "snapshot_capture",
            "snapshot_restore",
            "selection",
            "details_metadata",
            "details_database",
            "details_reflow",
            "page_history",
            "drawing_style",
            "compose_draw",
            "network_overlay",
            "frame_model_key",
            "frame_model_lookup",
            "frame_model_artwork",
            "frame_art_config",
            "frame_art_protection",
            "selected_info",
            "artwork_lookup",
            "layout",
            "semantic_model",
            "frame_model",
            "frame_normalize",
            "neighbor_generation",
            "neighbor_filter",
            "neighbor_search",
            "retained_compare",
            "neighbor_misses",
            "neighbor_clear",
            "frame_retire",
            "frame_readiness",
            "raster",
            "present",
        ] {
            let start = phases[name]["begin_monotonic_us"].as_u64().unwrap();
            let end = phases[name]["end_monotonic_us"].as_u64().unwrap();
            assert!(start >= input && end >= start, "{name}: {row}");
        }
        let names = row["measurement"]["stage_names"].as_array().unwrap();
        assert_eq!(names.len(), 34);
        for (index, name) in [(25, "page_history"), (26, "drawing_style"),
            (27, "compose_draw"), (28, "network_overlay"), (29, "frame_model_key"),
            (30, "frame_model_lookup"), (31, "frame_model_artwork"),
            (32, "frame_art_config"), (33, "frame_art_protection")] {
            assert_eq!(names[index], name);
            assert!(row["measurement"]["frame"]["stage_begin_monotonic_us"][index].as_u64().unwrap() > 0);
        }
        assert_eq!(phases["decode"]["wall_us"], 0);
        TRACE.with(|trace| *trace.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }
}

/// Existing `stages` are inclusive input totals; frame totals reset at begin_render.
#[derive(Clone, Copy)]
struct MeasurementSnapshot {
    generation: u64,
    viewport_shift: bool,
    input_epoch: u64,
    input_key: Option<crate::platform::Key>,
    input_edge: Option<&'static str>,
    input_us: u64,
    boost_us: u64,
    neighbor_build: &'static str,
    neighbor_adopt: &'static str,
    neighbor_warm: &'static str,
    content_pending_reason: &'static str,
    next_cover_expected: bool,
    missing_content_rows: usize,
    first_feedback_us: u64,
    final_content_us: u64,
    frame_stages: [u64; STAGE_NAMES.len()],
    cpu: [u64; 8],
    counts: [u64; COUNT_LEN],
    attempt: u64,
    render_us: u64,
    presented_us: u64,
    frame_stage_begin: [u64; STAGE_NAMES.len()],
    frame_stage_end: [u64; STAGE_NAMES.len()],
    input: InputTotals,
}
impl MeasurementSnapshot {
    fn capture(t: &Trace) -> Self {
        Self {
            generation: t.generation,
            viewport_shift: t.viewport_shift,
            input_epoch: t.input_epoch,
            input_key: t.input_key,
            input_edge: t.input_edge,
            input_us: t.input_us,
            boost_us: t.boost_us,
            neighbor_build: t.neighbor_build,
            neighbor_adopt: t.neighbor_adopt,
            neighbor_warm: t.neighbor_warm,
            content_pending_reason: t.content_pending_reason,
            next_cover_expected: t.next_cover_expected,
            missing_content_rows: t.missing_content_rows,
            first_feedback_us: t.first_feedback_us,
            final_content_us: t.final_content_us,
            frame_stages: t.frame_stages,
            cpu: t.cpu,
            counts: t.counts,
            attempt: t.attempt,
            render_us: t.render_us,
            presented_us: t.presented_us,
            frame_stage_begin: t.frame_stage_begin,
            frame_stage_end: t.frame_stage_end,
            input: t.completed_input.unwrap_or_else(|| input_totals(t)),
        }
    }
    fn values(&self, reasons: &[(&'static str, usize)]) -> serde_json::Value {
        serde_json::json!({
            "selection_generation": self.generation, "viewport_shift": self.viewport_shift,
            "input_generation": self.input_epoch, "input_key": self.input_key, "input_edge": self.input_edge,
            "input_received_monotonic_us": self.input_us,
            "boost_us": self.boost_us,
            "dropped_trace_lines": DROPPED_TRACE_LINES.load(std::sync::atomic::Ordering::Relaxed),
            "neighbor_build": self.neighbor_build, "neighbor_adopt": self.neighbor_adopt,
            "neighbor_warm": self.neighbor_warm,
            "content_pending_reason": self.content_pending_reason,
            "content_pending_reasons": reasons.iter().map(|(reason, missing_rows)|
                serde_json::json!({"reason":reason,"missing_rows":missing_rows})).collect::<Vec<_>>(),
            "next_cover_expected": self.next_cover_expected,
            "missing_content_rows": self.missing_content_rows,
            "first_feedback_monotonic_us": self.first_feedback_us,
            "final_content_monotonic_us": self.final_content_us,
            "first_feedback_us": (self.first_feedback_us != 0).then(|| self.first_feedback_us.saturating_sub(self.input_us)),
            "final_content_us": (self.final_content_us != 0).then(|| self.final_content_us.saturating_sub(self.input_us)),
            "frame": {"stage_wall_us": self.frame_stages.as_slice(), "cpu_ns": self.cpu, "counters": self.counts.as_slice(),
                "render_attempt": self.attempt, "render_start_monotonic_us": self.render_us,
                "presented_monotonic_us": self.presented_us,
                "stage_begin_monotonic_us": self.frame_stage_begin.as_slice(), "stage_end_monotonic_us": self.frame_stage_end.as_slice()},
            "input": {"stage_wall_us": self.input.stages.as_slice(), "cpu_ns": self.input.cpu, "counters": self.input.counts.as_slice()},
            "counter_names": ["text_hit", "text_miss", "text_eviction", "map_build", "copied_pixels", "flushed_pixels", "composed_pixels", "converted_pixels", "compared_pixels", "scanned_pixels", "changed_pixels", "image_hit", "image_miss", "ink_bitmap_pixels", "allocations", "plan_builds", "resize_pixels", "shadow_misses", "glow_misses", "capture_bytes", "restore_bytes", "opacity_pixels", "preview_mask_bytes", "art_admission_rejected", "art_admission_rejected_bytes", "art_request_hero", "art_request_logo", "art_request_cover", "art_request_next", "art_request_entries", "art_fitted_hit", "art_fitted_miss", "hero_hit", "hero_miss", "hero_prewarm_publish", "panel_hit", "panel_miss", "neighbor_hit", "neighbor_miss", "neighbor_built", "neighbor_pointer_adopt", "neighbor_damage_copy", "full_frame_detach", "packed_span_hit"],
            "stage_names": STAGE_NAMES.as_slice(),
            "phase_names": ["input", "art_load", "component_prep", "compose", "flush", "text", "artwork", "blit"]
        })
    }
}
fn measurement_values(t: &Trace) -> serde_json::Value {
    MeasurementSnapshot::capture(t).values(&t.content_pending_reasons)
}

fn input_totals(t: &Trace) -> InputTotals {
    InputTotals { stages: t.stages, cpu: t.input_cpu, counts: t.input_counts }
}

pub fn content_readiness(reason: &'static str, next_cover_expected: bool, missing_rows: usize) {
    if !recording() { return; }
    content_readiness_reasons(if reason == "ready" { vec![] } else { vec![(reason, missing_rows)] }, next_cover_expected);
}

pub fn content_readiness_reasons(reasons: Vec<(&'static str, usize)>, next_cover_expected: bool) {
    TRACE.with(|state| {
        if let Some(t) = state.borrow_mut().as_mut() {
            t.content_pending_reason = reasons.first().map_or("ready", |p| p.0);
            t.next_cover_expected = next_cover_expected;
            t.missing_content_rows = reasons.first().map_or(0, |p| p.1);
            t.content_pending_reasons = reasons;
        }
    });
}

/// The current input sequence is shared by controller selection and foreground tasks.
pub fn generation() -> u64 {
    TRACE.with(|state| state.borrow().as_ref().map_or(0, |t| t.generation))
}

/// Call after input receipt, using the controller's generation for stale-work correlation.
pub fn selection(generation: u64, viewport_shift: bool) {
    TRACE.with(|state| {
        if let Some(t) = state.borrow_mut().as_mut() {
            t.generation = generation;
            t.viewport_shift = viewport_shift;
        }
    });
}

/// Names shared with the read-only device sampler. Other lifecycle stages may use lifecycle().
pub const LAUNCHER_ENTRY: &str = "launcher_entry";
pub const FIRST_COMPLETED_FLUSH: &str = "first_completed_flush";
pub const LAUNCH_PREP: &str = "launch_prep";
pub const IMMINENT_EXEC: &str = "imminent_exec";
pub const WORKER_SHUTDOWN: &str = "worker_shutdown";
pub const CHILD_EXIT: &str = "child_exit";
pub const SESSION_RECORD: &str = "session_record";
pub const LAUNCHER_SPAWN: &str = "launcher_spawn";

pub fn lifecycle(event: &'static str) {
    initialize();
    TRACE.with(|state| {
        if let Some(t) = state.borrow_mut().as_mut() {
            write_event(t, event, t.generation, None);
            if event != FIRST_COMPLETED_FLUSH {
                let _ = t.output.flush();
            }
        }
    });
}

fn write_event(
    t: &mut Trace,
    event: &'static str,
    generation: u64,
    task: Option<(&'static str, u64, u64)>,
) {
    let _ = append_row(t, event_values(t, event, generation, task), false);
}

fn event_values(t: &Trace, event: &'static str, generation: u64, task: Option<(&'static str, u64, u64)>) -> serde_json::Value {
    serde_json::json!({
        "event": event, "monotonic_us": monotonic_us(), "thread_cpu_ns": thread_cpu_ns(),
        "pid": std::process::id(), "tid": thread_id(),
        "selection_generation": generation, "viewport_shift": t.viewport_shift,
        "task": task.map(|(name, wall_us, cpu_ns)| serde_json::json!({"name":name,"wall_us":wall_us,"cpu_ns":cpu_ns})),
        "measurement": measurement_values(t),
    })
}

pub fn artwork_readiness(before: &crate::theme::frame::Art, after: &crate::theme::frame::Art) {
    TRACE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(t) = state.as_mut() else { return };
        let mut changes = Vec::new();
        for (binding, old, new, missed_before, missed_after) in [
            ("selected_cover", &before.selected_cover, &after.selected_cover, before.completed_misses.selected_cover, after.completed_misses.selected_cover),
            ("selected_hero", &before.selected_hero, &after.selected_hero, before.completed_misses.selected_hero, after.completed_misses.selected_hero),
            ("selected_logo", &before.selected_logo, &after.selected_logo, before.completed_misses.selected_logo, after.completed_misses.selected_logo),
            ("next_cover", &before.next_cover, &after.next_cover, before.completed_misses.next_cover, after.completed_misses.next_cover),
        ] {
            let adopted = new.as_ref().is_some_and(|new| old.as_ref().is_none_or(|old| !std::sync::Arc::ptr_eq(old, new)));
            if adopted || (!missed_before && missed_after) {
                changes.push(serde_json::json!({"binding":binding,"outcome":if adopted { "adopted" } else { "completed_miss" }}));
            }
        }
        for (id, new) in &after.entries {
            if before.entries.get(id).is_none_or(|old| !std::sync::Arc::ptr_eq(old, new)) {
                changes.push(serde_json::json!({"binding":"rows","outcome":"adopted","row_id":id}));
            }
        }
        for id in after.completed_misses.entries.difference(&before.completed_misses.entries) {
            changes.push(serde_json::json!({"binding":"rows","outcome":"completed_miss","row_id":id}));
        }
        if !changes.is_empty() {
            let mut row = event_values(t, "artwork_readiness", t.generation, None);
            row["artwork_changes"] = changes.into();
            let _ = append_row(t, row, false);
        }
    });
}

fn content_marker(generation: u64, final_content: bool) {
    TRACE.with(|state| {
        if let Some(t) = state.borrow_mut().as_mut() {
            // A delayed completion must never mark a newer selection as finished.
            if t.generation != generation {
                return;
            }
            let marker = if final_content {
                &mut t.final_content_us
            } else {
                &mut t.first_feedback_us
            };
            if *marker != 0 {
                return;
            }
            *marker = monotonic_us();
            if final_content {
                t.completed_input = Some(input_totals(t));
            }
            write_event(
                t,
                if final_content {
                    "final_content"
                } else {
                    "first_feedback"
                },
                generation,
                None,
            );
            if final_content {
                let _ = t.output.flush();
            }
        }
    });
}
/// Mark a completed flush. A complete first frame shares one timestamp for both
/// milestones, so writing the feedback trace cannot inflate content latency.
pub fn presented(generation: u64, content_complete: bool) {
    TRACE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(t) = state.as_mut() else {
            return;
        };
        if t.generation != generation {
            return;
        }
        let now = monotonic_us();
        t.presented_us = now;
        let first = t.first_feedback_us == 0;
        let final_content = content_complete && t.final_content_us == 0;
        if !first && !final_content {
            t.output.flush_if_full();
            return;
        }
        if first {
            t.first_feedback_us = now;
        }
        if final_content {
            t.final_content_us = now;
            t.completed_input = Some(input_totals(t));
        }
        if first {
            write_event(t, "first_feedback", generation, None);
        }
        if final_content {
            write_event(t, "final_content", generation, None);
            let _ = t.output.flush();
        } else {
            t.output.flush_if_full();
        }
    });
}

/// Settle unchanged content only after this selection has actually been presented.
pub fn settled(generation: u64) {
    if TRACE.with(|state| {
        state.borrow().as_ref().is_some_and(|t| {
            t.generation == generation && t.first_feedback_us != 0 && t.final_content_us == 0
        })
    }) {
        final_content(generation);
    }
}

pub fn first_feedback(generation: u64) {
    content_marker(generation, false);
}
pub fn final_content(generation: u64) {
    content_marker(generation, true);
}

pub struct TaskGuard {
    name: &'static str,
    generation: u64,
    started: Option<(u64, u64)>,
    completed: Option<(u64, u64)>,
}
/// Worker threads initialize their own trace; pass the scheduled generation explicitly.
pub fn task(name: &'static str, generation: u64) -> TaskGuard {
    initialize();
    // Idle prewarm can run hundreds of times after final content, evicting that input.
    let started = (name != "navigation_prewarm").then(|| TRACE.with(|state| {
        state
            .borrow()
            .as_ref()
            .map(|_| (monotonic_us(), thread_cpu_ns()))
    })).flatten();
    TaskGuard {
        name,
        generation,
        started,
        completed: None,
    }
}
impl TaskGuard {
    /// Stop timing before publishing a result; defer trace I/O until after publication.
    pub fn finish(&mut self) {
        if let Some((wall, cpu)) = self.started.take() {
            self.completed = Some((
                monotonic_us().saturating_sub(wall),
                thread_cpu_ns().saturating_sub(cpu),
            ));
        }
    }
}
impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.finish();
        let Some(elapsed) = self.completed else {
            return;
        };
        TRACE.with(|state| {
            if let Some(t) = state.borrow_mut().as_mut() {
                write_event(
                    t,
                    "task_completed",
                    self.generation,
                    Some((self.name, elapsed.0, elapsed.1)),
                );
            }
        });
    }
}

#[cfg(test)]
mod measurement_tests {
    use super::*;
    #[test]
    fn boost_worker_timings_are_one_row_and_disabled_work_still_runs() {
        let path = std::env::temp_dir().join(format!("allium-boost-work-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        {
            let mut timing = boost_work();
            assert_eq!(timing.lease(|| 17), 17);
            assert_eq!(timing.sysfs(|| 23), 23);
        }
        flush_pending();
        let rows = std::fs::read_to_string(&path).unwrap();
        assert_eq!(rows.lines().count(), 1);
        let row: serde_json::Value = serde_json::from_str(rows.trim()).unwrap();
        assert_eq!(row["event"], "boost_completed");
        let m = &row["measurement"];
        let start = m["boost_started_monotonic_us"].as_u64().unwrap();
        let lease_begin = m["boost_lease_begin_monotonic_us"].as_u64().unwrap();
        let lease_end = m["boost_lease_end_monotonic_us"].as_u64().unwrap();
        let sysfs_begin = m["boost_sysfs_begin_monotonic_us"].as_u64().unwrap();
        let sysfs_end = m["boost_sysfs_end_monotonic_us"].as_u64().unwrap();
        assert!(start > 0 && start <= lease_begin && lease_begin <= lease_end);
        assert!(lease_end <= sysfs_begin && sysfs_begin <= sysfs_end);
        assert_eq!(m["boost_lease_us"], lease_end - lease_begin);
        assert_eq!(m["boost_sysfs_us"], sysfs_end - sysfs_begin);
        TRACE.with(|trace| *trace.borrow_mut() = None);
        let mut timing = boost_work();
        assert!(timing.timing.is_none());
        assert_eq!(timing.lease(|| 31), 31);
        assert_eq!(timing.sysfs(|| 47), 47);
        drop(timing);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), rows);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn boost_precedes_trace_serialization_but_follows_input_receipt() {
        use crate::platform::{Key, KeyEvent};
        let path = std::env::temp_dir().join(format!("allium-input-boost-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        let mut boost_begin = 0;
        let mut boost_end = 0;
        {
            let _input = input_with_boost(KeyEvent::Pressed(Key::B), || {
                boost_begin = monotonic_us();
                assert_eq!(generation(), 0);
                assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
                std::thread::sleep(std::time::Duration::from_millis(1));
                boost_end = monotonic_us();
            });
        }
        begin_render();
        presented(generation(), true);
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(rows[0]["event"], "input_received");
        for row in &rows {
            let measurement = &row["measurement"];
            let receipt = measurement["input_received_monotonic_us"].as_u64().unwrap();
            let boost_us = measurement["boost_us"].as_u64().unwrap();
            assert!(receipt > 0 && receipt <= boost_begin);
            assert!(boost_us >= boost_end - boost_begin);
            assert!(receipt + boost_us <= row["monotonic_us"].as_u64().unwrap());
            if let Some(final_us) = measurement["final_content_us"].as_u64() {
                assert!(final_us >= boost_us);
            }
        }
        { let _input = input(KeyEvent::Released(Key::B)); }
        assert_eq!(probe_counts()["measurement"]["boost_us"], 0);
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn boost_runs_without_trace_recording() {
        TRACE.with(|t| *t.borrow_mut() = None);
        let mut calls = 0;
        let guard = input_with_boost(crate::platform::KeyEvent::Pressed(crate::platform::Key::B), || calls += 1);
        assert_eq!(calls, 1);
        assert!(guard.timing.is_none());
        assert!(!recording());
    }

    #[test]
    fn prewarm_flood_preserves_open_and_completed_input_rows_across_threads() {
        let path = std::env::temp_dir().join(format!("allium-prewarm-flood-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        { let _input = input(crate::platform::KeyEvent::Pressed(crate::platform::Key::B)); }
        begin_render();
        content_readiness("selected_logo", false, 0);
        presented(generation(), false);
        drawn_frame();
        let flood = || {
            let worker_path = path.clone();
            std::thread::spawn(move || {
                install(worker_path.to_str().unwrap()).unwrap();
                for _ in 0..1000 { let _task = task("navigation_prewarm", 1); }
            }).join().unwrap();
            for _ in 0..1000 { let _task = task("navigation_prewarm", 1); }
        };
        flush_pending();
        let open = std::fs::read(&path).unwrap();
        flood();
        assert_eq!(std::fs::read(&path).unwrap(), open);
        content_readiness("ready", false, 0);
        presented(generation(), true);
        let completed = std::fs::read(&path).unwrap();
        flood();
        assert_eq!(std::fs::read(&path).unwrap(), completed);
        let rows: Vec<serde_json::Value> = std::str::from_utf8(&completed).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(rows.iter().map(|row| row["event"].as_str().unwrap()).collect::<Vec<_>>(),
            ["input_received", "first_feedback", "frame", "final_content"]);
        assert!(completed.len() as u64 <= LOG_LIMIT);
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn input_records_identify_key_edge_and_independent_measurement_generation() {
        use crate::platform::{Key, KeyEvent};
        let path = std::env::temp_dir().join(format!("allium-input-edge-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        for event in [KeyEvent::Pressed(Key::B), KeyEvent::Released(Key::B), KeyEvent::Autorepeat(Key::Right)] {
            let _input = input(event);
            selection(91, false);
            first_feedback(91);
        }
        flush_pending();
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        for (i, (key, edge)) in [("B", "press"), ("B", "release"), ("Right", "autorepeat")].iter().enumerate() {
            for row in &rows[i * 2..i * 2 + 2] {
                assert_eq!(row["measurement"]["input_key"], *key);
                assert_eq!(row["measurement"]["input_edge"], *edge);
                assert_eq!(row["measurement"]["input_generation"], i + 1);
                assert!(row["measurement"]["input_received_monotonic_us"].as_u64().unwrap() > 0);
                assert_eq!(row["measurement"]["boost_us"], 0);
            }
            assert_eq!(rows[i * 2]["event"], "input_received");
            assert_eq!(rows[i * 2 + 1]["selection_generation"], 91);
        }
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn presented_frames_keep_separate_stage_endpoints_and_all_pending_reasons() {
        let path = std::env::temp_dir().join(format!("allium-frame-readiness-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        { let _input = input(crate::platform::KeyEvent::Pressed(crate::platform::Key::B)); }
        for reasons in [vec![("selected_logo", 0), ("rows", 2)], vec![("rows", 1)], vec![]] {
            begin_render();
            content_readiness_reasons(reasons, true);
            for which in [Stage::Layout, Stage::Raster, Stage::Present] {
                let _stage = stage(which);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let complete = probe_counts()["measurement"]["content_pending_reasons"].as_array().unwrap().is_empty();
            presented(generation(), complete);
            drawn_frame();
        }
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        let frames: Vec<_> = rows.iter().filter(|row| row["event"] == "frame").collect();
        for (i, row) in frames.iter().enumerate() {
            let frame = &row["measurement"]["frame"];
            assert_eq!(frame["render_attempt"], i + 1);
            let start = frame["render_start_monotonic_us"].as_u64().unwrap();
            let present = frame["presented_monotonic_us"].as_u64().unwrap();
            for index in [4, 5, 6] {
                let begin = frame["stage_begin_monotonic_us"][index].as_u64().unwrap();
                let end = frame["stage_end_monotonic_us"][index].as_u64().unwrap();
                assert!(start <= begin && begin <= end && end <= present);
                assert_eq!(frame["stage_wall_us"][index], end - begin);
            }
            assert_eq!(row["measurement"]["content_pending_reasons"].as_array().unwrap().len(), 2 - i);
        }
        let final_row = rows.iter().find(|row| row["event"] == "final_content").unwrap();
        assert_eq!(final_row["measurement"]["frame"]["render_attempt"], 3);
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn artwork_readiness_names_adoptions_and_completed_misses_without_duplicates() {
        use crate::theme::frame::Art;
        let path = std::env::temp_dir().join(format!("allium-art-readiness-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        let before = Art::default();
        let mut after = before.clone();
        after.selected_logo = Some(std::sync::Arc::new(tiny_skia::Pixmap::new(1, 1).unwrap()));
        after.completed_misses.next_cover = true;
        after.completed_misses.entries.insert("missing-row".into());
        artwork_readiness(&before, &after);
        artwork_readiness(&after, &after);
        let row: serde_json::Value = serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(row["event"], "artwork_readiness");
        assert_eq!(row["artwork_changes"], serde_json::json!([
            {"binding":"selected_logo","outcome":"adopted"},
            {"binding":"next_cover","outcome":"completed_miss"},
            {"binding":"rows","outcome":"completed_miss","row_id":"missing-row"}
        ]));
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn old_frame_projection_and_counter_indices_remain_compatible() {
        #[derive(serde::Deserialize)]
        struct LegacyFrame { event: String, measurement: LegacyMeasurement }
        #[derive(serde::Deserialize)]
        struct LegacyMeasurement { frame: LegacyCounters, counter_names: Vec<String> }
        #[derive(serde::Deserialize)]
        struct LegacyCounters { counters: Vec<u64> }
        let old = r#"{"event":"frame","measurement":{"frame":{"counters":[0,1]},"counter_names":["text_hit","text_miss"]}}"#;
        let legacy: LegacyFrame = serde_json::from_str(old).unwrap();
        assert_eq!(legacy.measurement.frame.counters, [0, 1]);
        let path = std::env::temp_dir().join(format!("allium-legacy-frame-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        count(Count::HeroHit, 32);
        count(Count::HeroMiss, 33);
        count(Count::HeroPrewarmPublish, 34);
        count(Count::PanelHit, 35);
        count(Count::PanelMiss, 36);
        count(Count::NeighborPointerAdopt, 40);
        count(Count::NeighborDamageCopy, 41);
        count(Count::FullFrameDetach, 42);
        drawn_frame();
        let legacy: LegacyFrame = serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(legacy.event, "frame");
        assert_eq!(&legacy.measurement.counter_names[32..], ["hero_hit", "hero_miss", "hero_prewarm_publish", "panel_hit", "panel_miss", "neighbor_hit", "neighbor_miss", "neighbor_built", "neighbor_pointer_adopt", "neighbor_damage_copy", "full_frame_detach", "packed_span_hit"]);
        assert_eq!(&legacy.measurement.frame.counters[32..], [32, 33, 34, 35, 36, 0, 0, 0, 40, 41, 42, 0]);
        TRACE.with(|t| *t.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn worker_decode_trace_cannot_complete_foreground_selection() {
        let path = std::env::temp_dir().join(format!("allium-worker-isolation-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        { let _input = phase(Phase::Input); }
        selection(26, false);
        content_readiness("selected_logo", true, 0);
        presented(26, false);
        let worker_path = path.clone();
        std::thread::Builder::new().name("allium-art".into()).spawn(move || {
            install(worker_path.to_str().unwrap()).unwrap();
            {
                let _task = task("artwork", 26);
                begin_render();
                hit(ArtHit::Decode);
                drawn_frame();
            }
            let values = probe_counts();
            assert_eq!(values["measurement"]["selection_generation"], 0);
            assert_eq!(values["measurement"]["content_pending_reason"], "not_evaluated");
            assert_eq!(values["measurement"]["final_content_monotonic_us"], 0);
        }).unwrap().join().unwrap();
        assert_eq!(probe_counts()["measurement"]["content_pending_reason"], "selected_logo");
        assert_eq!(probe_counts()["measurement"]["final_content_monotonic_us"], 0);
        content_readiness("ready", true, 0);
        presented(26, true);
        presented(26, true);
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        let worker_frame = rows.iter().find(|r| r["event"] == "frame").unwrap();
        assert_eq!(worker_frame["thread"], "allium-art");
        assert_eq!(worker_frame["input_monotonic_us"], 0);
        assert_eq!(worker_frame["flushed_pixels"], 0);
        let task = rows.iter().find(|r| r["event"] == "task_completed").unwrap();
        assert_eq!(task["selection_generation"], 26);
        assert_eq!(task["measurement"]["selection_generation"], 0);
        assert_eq!(rows.iter().filter(|r| r["event"] == "final_content").count(), 1);
        TRACE.with(|state| *state.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn settlement_requires_presented_current_selection_and_is_deduplicated() {
        let path = std::env::temp_dir().join(format!("allium-settlement-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        selection(41, false);
        settled(41);
        assert_eq!(probe_counts()["measurement"]["final_content_monotonic_us"], 0);
        presented(41, false);
        settled(40);
        assert_eq!(probe_counts()["measurement"]["final_content_monotonic_us"], 0);
        settled(41);
        settled(41);
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(rows.iter().filter(|row| row["event"] == "final_content").count(), 1);
        { let _input = phase(Phase::Input); }
        settled(generation());
        assert_eq!(probe_counts()["measurement"]["final_content_monotonic_us"], 0);
        TRACE.with(|state| *state.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn completed_input_totals_exclude_idle_work_and_reset_on_next_input() {
        let path = std::env::temp_dir().join(format!("allium-final-totals-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        { let _input = phase(Phase::Input); }
        selection(91, true);
        begin_render();
        count(Count::CaptureBytes, 128);
        presented(91, false);
        count(Count::RestoreBytes, 64);
        presented(91, true);
        let final_totals = probe_counts()["measurement"]["input"].clone();
        count(Count::PlanBuilds, 17);
        { let _phase = phase(Phase::Compose); }
        { let _stage = stage(Stage::Raster); }
        begin_render();
        presented(90, true);
        assert_eq!(probe_counts()["measurement"]["input"], final_totals);
        assert_eq!(final_totals["counters"][19], 128);
        assert_eq!(final_totals["counters"][20], 64);
        { let _input = phase(Phase::Input); }
        count(Count::PlanBuilds, 2);
        assert_eq!(probe_counts()["measurement"]["input"]["counters"][15], 2);
        TRACE.with(|state| *state.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn input_totals_span_frames_and_stale_completion_is_ignored() {
        let path =
            std::env::temp_dir().join(format!("allium-measurement-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        {
            let _input = phase(Phase::Input);
        }
        {
            let _selection = stage(Stage::Selection);
            selection(42, true);
        }
        assert!(
            probe_counts()["stages"]["phases"]["selection"]["end_monotonic_us"]
                .as_u64()
                .unwrap()
                > 0
        );
        lifecycle(LAUNCHER_ENTRY);
        begin_render();
        count(Count::PlanBuilds, 2);
        {
            let _stage = stage(Stage::Raster);
        }
        first_feedback(42);
        first_feedback(42);
        final_content(41);
        assert_eq!(
            probe_counts()["measurement"]["final_content_monotonic_us"],
            0
        );
        drawn_frame();
        begin_render();
        count(Count::PlanBuilds, 3);
        {
            let _task = task("worker_apply", 42);
        }
        final_content(42);
        let row = probe_counts();
        assert_eq!(row["measurement"]["frame"]["counters"][15], 3);
        assert_eq!(row["measurement"]["input"]["counters"][15], 5);
        assert!(row["measurement"]["first_feedback_us"].is_number());
        drawn_frame();
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(
            rows.iter()
                .filter(|r| r["event"] == "first_feedback")
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r["event"] == "final_content")
                .count(),
            1
        );
        assert!(
            rows.iter()
                .filter(|r| r["event"] != "frame")
                .all(|r| r["monotonic_us"].is_number() && r["thread_cpu_ns"].is_number())
        );
        {
            let _input = phase(Phase::Input);
        }
        selection(43, false);
        begin_render();
        presented(43, true);
        let both = probe_counts();
        assert_eq!(
            both["measurement"]["first_feedback_monotonic_us"],
            both["measurement"]["final_content_monotonic_us"]
        );
        TRACE.with(|state| *state.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod task_publication_tests {
    use super::*;
    #[test]
    fn task_finish_excludes_result_publication_and_preserves_request_generation() {
        let path = std::env::temp_dir().join(format!("allium-task-publication-{}.log", std::process::id()));
        install(path.to_str().unwrap()).unwrap();
        selection(12, false);
        let mut task = task("artwork", 91);
        task.finish();
        let elapsed = task.completed.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        task.finish();
        assert_eq!(task.completed, Some(elapsed));
        drop(task);
        let row: serde_json::Value = serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(row["selection_generation"], 91);
        assert_eq!(row["task"]["wall_us"], elapsed.0);
        assert_eq!(row["task"]["cpu_ns"], elapsed.1);
        TRACE.with(|state| *state.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
#[test]
fn fpdiag_idle_build_reason_survives_into_press_final_content() {
    let path = std::env::temp_dir().join(format!("allium-fpdiag-trace-{}.log", std::process::id()));
    start_probe(path.to_str().unwrap());
    neighbor_build("admission_rejected");
    neighbor_warm("allowed");
    begin_render();
    { let _input = input(crate::platform::KeyEvent::Pressed(crate::platform::Key::Down)); }
    begin_render();
    assert_eq!(probe_counts()["measurement"]["neighbor_build"], "admission_rejected");
    assert_eq!(probe_counts()["measurement"]["neighbor_adopt"], "not_attempted");
    neighbor_adopt("no_candidate");
    presented(generation(), true);
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    let final_row = rows.iter().find(|row| row["event"] == "final_content").unwrap();
    assert_eq!(final_row["measurement"]["neighbor_build"], "admission_rejected");
    assert_eq!(final_row["measurement"]["neighbor_adopt"], "no_candidate");
    assert_eq!(final_row["measurement"]["neighbor_warm"], "allowed");
    stop_probe();
    std::fs::remove_file(path).unwrap();
}

#[cfg(test)]
mod deferred_trace_tests {
    use super::*;
    use crate::platform::{Key, KeyEvent};

    fn path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("allium-tracedefer-{name}-{}.log", std::process::id()))
    }
    fn rows(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect()
    }

    #[test]
    fn input_then_frame_keeps_event_time_fields_and_line_order() {
        let path = path("frame");
        install(path.to_str().unwrap()).unwrap();
        TRACE.with(|state| {
            let mut state = state.borrow_mut();
            let t = state.as_mut().unwrap();
            t.counts[0] = 17;
            t.frame_stages[2] = 23;
            t.frame_stage_begin[2] = 29;
            t.frame_stage_end[2] = 31;
            t.neighbor_build = "ready:idle";
        });
        let serializations = SERIALIZATIONS.with(std::cell::Cell::get);
        let before = monotonic_us();
        let guard = input(KeyEvent::Pressed(Key::Down));
        let after = monotonic_us();
        let mut expected = TRACE.with(|state| {
            let state = state.borrow();
            let t = state.as_ref().unwrap();
            let pending = t.output.pending.lock().unwrap();
            let Some(PendingRow::Snapshot(snapshot, _)) = pending.front() else { panic!("missing snapshot") };
            assert!(before <= snapshot.monotonic_us && snapshot.monotonic_us <= after);
            // Compare the complete pre-existing event projection at receipt,
            // including the previous frame's counters and endpoints.
            let mut row = event_values(t, "input_received", t.generation, None);
            row["monotonic_us"] = snapshot.monotonic_us.into();
            row["thread_cpu_ns"] = snapshot.thread_cpu_ns.into();
            row
        });
        assert_eq!(SERIALIZATIONS.with(std::cell::Cell::get), serializations);
        assert!(rows(&path).is_empty());
        drop(guard);
        selection(91, true);
        begin_render();
        count(Count::TextMiss, 43);
        content_readiness("ready", false, 0);
        presented(91, true);
        drawn_frame();
        let output = rows(&path);
        assert_eq!(output.iter().map(|row| row["event"].as_str().unwrap()).collect::<Vec<_>>(),
            ["input_received", "first_feedback", "final_content", "frame"]);
        let mut actual = output[0].clone();
        actual.as_object_mut().unwrap().remove("trace_write_begin_us");
        actual.as_object_mut().unwrap().remove("trace_write_end_us");
        // Other tests may drop a line on the process-wide counter concurrently.
        expected["measurement"]["dropped_trace_lines"] = actual["measurement"]["dropped_trace_lines"].clone();
        assert_eq!(actual, expected);
        assert_eq!(actual["measurement"]["frame"]["counters"][0], 17);
        assert_eq!(actual["measurement"]["input"]["cpu_ns"][0], 0);
        assert_eq!(output[2]["measurement"]["input_key"], "Down");
        assert_eq!(output[2]["measurement"]["selection_generation"], 91);
        let presented = output[2]["measurement"]["final_content_monotonic_us"].as_u64().unwrap();
        let write_begin = output[1]["trace_write_begin_us"].as_u64().unwrap();
        let write_end = output[1]["trace_write_end_us"].as_u64().unwrap();
        assert!(presented <= write_begin && write_begin <= write_end);
        stop_probe();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn no_frame_inputs_flush_at_idle_lifecycle_and_scope_exit() {
        let path = path("no-frame");
        install(path.to_str().unwrap()).unwrap();
        { let _input = input(KeyEvent::Released(Key::B)); }
        assert!(rows(&path).is_empty());
        flush_pending();
        assert_eq!(rows(&path)[0]["measurement"]["input_edge"], "release");
        { let _input = input(KeyEvent::Pressed(Key::B)); }
        lifecycle(LAUNCH_PREP);
        assert_eq!(rows(&path).iter().map(|row| row["event"].as_str().unwrap()).collect::<Vec<_>>(),
            ["input_received", "input_received", LAUNCH_PREP]);
        {
            let _flush = FlushPendingOnDrop;
            let _input = input(KeyEvent::Autorepeat(Key::B));
        }
        assert_eq!(rows(&path).len(), 4);
        { let _input = input(KeyEvent::Pressed(Key::Menu)); }
        stop_probe();
        assert_eq!(rows(&path).len(), 5);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn boost_worker_defers_serialization_and_keeps_shared_writer_order() {
        let path = path("worker");
        install(path.to_str().unwrap()).unwrap();
        { let _input = input(KeyEvent::Pressed(Key::Down)); }
        let worker_path = path.clone();
        std::thread::spawn(move || {
            install(worker_path.to_str().unwrap()).unwrap();
            let before = SERIALIZATIONS.with(std::cell::Cell::get);
            {
                let mut work = boost_work();
                work.lease(|| ());
                work.sysfs(|| ());
            }
            assert_eq!(SERIALIZATIONS.with(std::cell::Cell::get), before);
            { let _task = task("test_worker", 1); }
            assert!(rows(&worker_path).is_empty());
            stop_probe();
        }).join().unwrap();
        assert!(rows(&path).is_empty());
        begin_render();
        presented(generation(), true);
        drawn_frame();
        let output = rows(&path);
        assert_eq!(output.iter().map(|row| row["event"].as_str().unwrap()).collect::<Vec<_>>(),
            ["input_received", "boost_completed", "task_completed", "first_feedback", "final_content", "frame"]);
        assert_ne!(output[0]["tid"], output[1]["tid"]);
        let m = &output[1]["measurement"];
        assert!(m["boost_started_monotonic_us"].as_u64().unwrap()
            <= m["boost_sysfs_end_monotonic_us"].as_u64().unwrap());
        assert!(output[1]["monotonic_us"].as_u64().unwrap()
            <= output[3]["monotonic_us"].as_u64().unwrap());
        stop_probe();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn disabled_input_and_boost_do_not_serialize_or_capture() {
        stop_probe();
        let before = SERIALIZATIONS.with(std::cell::Cell::get);
        let snapshots = SNAPSHOTS.with(std::cell::Cell::get);
        for event in [KeyEvent::Pressed(Key::B), KeyEvent::Released(Key::B), KeyEvent::Autorepeat(Key::Down)] {
            let guard = input_with_boost(event, || {
                let mut work = boost_work();
                assert!(work.timing.is_none());
                work.lease(|| ());
                work.sysfs(|| ());
            });
            assert!(guard.timing.is_none());
            drop(guard);
            flush_pending();
        }
        assert_eq!(SERIALIZATIONS.with(std::cell::Cell::get), before);
        assert_eq!(SNAPSHOTS.with(std::cell::Cell::get), snapshots);
        assert!(!recording());
    }

    #[test]
    fn queued_no_frame_inputs_are_not_overwritten_and_keep_the_log_cap() {
        let path = path("queue");
        install(path.to_str().unwrap()).unwrap();
        for _ in 0..16 { let _input = input(KeyEvent::Pressed(Key::B)); }
        assert!(rows(&path).is_empty());
        flush_pending();
        let output = rows(&path);
        assert_eq!(output.len(), 16);
        for (i, row) in output.iter().enumerate() {
            assert_eq!(row["measurement"]["input_generation"], i + 1);
        }
        for _ in 0..16 {
            for _ in 0..16 { let _input = input(KeyEvent::Pressed(Key::B)); }
            flush_pending();
        }
        let output = rows(&path);
        assert_eq!(output.last().unwrap()["measurement"]["input_generation"], 272);
        assert!(std::fs::metadata(&path).unwrap().len() <= LOG_LIMIT);
        stop_probe();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn deferred_contended_append_reports_dropped_lines_on_the_following_record() {
        let path = path("drop");
        install(path.to_str().unwrap()).unwrap();
        let before = DROPPED_TRACE_LINES.load(std::sync::atomic::Ordering::Relaxed);
        let holder = OpenOptions::new().append(true).open(&path).unwrap();
        assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) }, 0);
        { let _input = input(KeyEvent::Pressed(Key::B)); }
        flush_pending();
        assert!(rows(&path).is_empty());
        assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_UN) }, 0);
        begin_render();
        presented(generation(), true);
        drawn_frame();
        let output = rows(&path);
        assert_eq!(output[0]["event"], "first_feedback");
        for row in output {
            assert!(row["measurement"]["dropped_trace_lines"].as_u64().unwrap() > before);
        }
        stop_probe();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn input_and_boost_snapshots_do_not_wait_for_worker_writer() {
        let path = path("writer-lock");
        install(path.to_str().unwrap()).unwrap();
        let output = TRACE.with(|state| state.borrow().as_ref().unwrap().output.clone());
        let (locked, wait_locked) = std::sync::mpsc::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut writer = output.writer.lock().unwrap();
            locked.send(()).unwrap();
            let released = wait_release.recv_timeout(std::time::Duration::from_secs(2)).is_ok();
            writer.write(serde_json::json!({"event":"worker_write"})).unwrap();
            released
        });
        wait_locked.recv().unwrap();
        let before = SERIALIZATIONS.with(std::cell::Cell::get);
        let started = Instant::now();
        { let _input = input(KeyEvent::Pressed(Key::Down)); }
        { let _boost = boost_work(); }
        let elapsed = started.elapsed();
        let _ = release.send(());
        let released = worker.join().unwrap();
        assert_eq!(SERIALIZATIONS.with(std::cell::Cell::get), before);
        flush_pending();
        let events = rows(&path);
        stop_probe();
        std::fs::remove_file(path).unwrap();
        assert!(released && elapsed < std::time::Duration::from_millis(100), "snapshot waited {elapsed:?}");
        assert_eq!(events.iter().map(|row| row["event"].as_str().unwrap()).collect::<Vec<_>>(),
            ["worker_write", "input_received", "boost_completed"]);
    }

    #[test]
    fn pending_queue_bounds_both_enqueue_paths_and_reports_overflow() {
        let path = path("overflow");
        install(path.to_str().unwrap()).unwrap();
        let before = DROPPED_TRACE_LINES.load(std::sync::atomic::Ordering::Relaxed);
        for _ in 0..PENDING_LIMIT { let _input = input(KeyEvent::Pressed(Key::B)); }
        for _ in 0..5 { let _input = input(KeyEvent::Pressed(Key::B)); }
        for _ in 0..4 { drawn_frame(); }
        TRACE.with(|state| {
            assert_eq!(state.borrow().as_ref().unwrap().output.pending.lock().unwrap().len(), PENDING_LIMIT);
        });
        assert!(DROPPED_TRACE_LINES.load(std::sync::atomic::Ordering::Relaxed) >= before + 9);
        assert!(rows(&path).is_empty());
        flush_pending();
        let output = rows(&path);
        assert!(!output.is_empty());
        for row in &output {
            assert!(row["measurement"]["dropped_trace_lines"].as_u64().unwrap() >= before + 9);
        }
        for pair in output.windows(2) {
            assert_eq!(pair[1]["measurement"]["input_generation"].as_u64().unwrap(),
                pair[0]["measurement"]["input_generation"].as_u64().unwrap() + 1);
        }
        assert_eq!(output.last().unwrap()["measurement"]["input_generation"], PENDING_LIMIT);
        stop_probe();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn incomplete_presentations_drain_at_threshold_without_an_input_gap() {
        let path = path("threshold");
        install(path.to_str().unwrap()).unwrap();
        for _ in 0..PENDING_FLUSH_THRESHOLD { let _input = input(KeyEvent::Pressed(Key::Down)); }
        assert!(rows(&path).is_empty());
        presented(generation(), false);
        assert_eq!(rows(&path).len(), PENDING_FLUSH_THRESHOLD + 1);
        // Repeated incomplete frames must also drain after first feedback.
        for _ in 0..PENDING_FLUSH_THRESHOLD { let _boost = boost_work(); }
        presented(generation(), false);
        let output = rows(&path);
        assert_eq!(output.len(), 2 * PENDING_FLUSH_THRESHOLD + 1);
        assert!(output.iter().all(|row| row["event"] != "final_content"));
        stop_probe();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn final_guard_flushes_boost_completed_after_loop_exit_on_all_paths() {
        // Model main's reverse drop order with a worker join as boost quiescence.
        struct Quiesce(Option<(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>)>);
        impl Drop for Quiesce {
            fn drop(&mut self) {
                let (release, worker) = self.0.take().unwrap();
                release.send(()).unwrap();
                worker.join().unwrap();
            }
        }
        for mode in ["normal", "error", "unwind"] {
            let path = path(mode);
            install(path.to_str().unwrap()).unwrap();
            let worker_path = path.clone();
            let (ready, wait_ready) = std::sync::mpsc::channel();
            let (release, wait_release) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                install(worker_path.to_str().unwrap()).unwrap();
                let work = boost_work();
                ready.send(()).unwrap();
                wait_release.recv().unwrap();
                drop(work);
                stop_probe();
            });
            wait_ready.recv().unwrap();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), ()> {
                let _final_flush = FlushPendingOnDrop;
                let _boost_guard = Quiesce(Some((release, worker)));
                {
                    let _loop_flush = FlushPendingOnDrop;
                    let _input = input(KeyEvent::Released(Key::B));
                }
                assert_eq!(rows(&path).len(), 1);
                match mode {
                    "normal" => Ok(()),
                    "error" => Err(()),
                    _ => panic!("simulate event loop unwind"),
                }
            }));
            assert_eq!(result.is_err(), mode == "unwind");
            if mode == "error" { assert_eq!(result.unwrap(), Err(())); }
            assert_eq!(rows(&path).iter().map(|row| row["event"].as_str().unwrap()).collect::<Vec<_>>(),
                ["input_received", "boost_completed"]);
            stop_probe();
            std::fs::remove_file(path).unwrap();
        }
    }

}
