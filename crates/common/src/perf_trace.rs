use std::{
    cell::RefCell,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::io::AsRawFd,
    time::Instant,
};

const LOG_LIMIT: u64 = 256 * 1024;
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
    ComparedPixels,
    ScannedPixels,
    ChangedPixels,
    ImageHit,
    ImageMiss,
    InkBitmapPixels,
}
struct Trace {
    counts: [u64; 14],
    stages: [u64; 8],
    stage_begin: [u64; 8],
    stage_end: [u64; 8],
    file: File,
    path: String,
    cpu: [u64; 8],
    hits: [u64; 3],
    started: Option<Instant>,
    render_us: u64,
    attempt: u64,
    wake_us: u64,
    input_us: u64,
    damage_rects: u64,
    damage_pixels: u64,
    render_reason: &'static str,
    present_reason: &'static str,
}
thread_local! {
    static TRACE: RefCell<Option<Trace>> = const { RefCell::new(None) };
}

pub fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::fs::metadata("/tmp/allium-perf-trace").is_ok())
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
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let bytes = file.metadata()?.len();
    if bytes < LOG_LIMIT {
        TRACE.with(|trace| {
            *trace.borrow_mut() = Some(Trace {
                file,
                path: path.into(),
                counts: [0; 14],
                stages: [0; 8],
                stage_begin: [0; 8],
                stage_end: [0; 8],
                cpu: [0; 8],
                hits: [0; 3],
                started: None,
                render_us: 0,
                attempt: 0,
                wake_us: 0,
                input_us: 0,
                damage_rects: 0,
                damage_pixels: 0,
                render_reason: "none",
                present_reason: "none",
            })
        });
    }
    Ok(())
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
    let timing = TRACE.with(|trace| {
        let mut trace = trace.borrow_mut();
        let trace = trace.as_mut()?;
        if matches!(phase, Phase::Input) {
            trace.input_us = monotonic_us();
            trace.stages = [0; 8];
            trace.stage_begin = [0; 8];
            trace.stage_end = [0; 8];
        }
        Some(Timing {
            start: thread_cpu_ns(),
            children: trace.cpu.iter().sum(),
        })
    });
    PhaseGuard { phase, timing }
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
                trace.cpu[self.phase as usize] += duration.saturating_sub(children);
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
            let input = t.cpu[0];
            t.cpu = [0; 8];
            t.cpu[0] = input;
            t.hits = [0; 3];
            t.counts = [0; 14];
            t.started = Some(Instant::now());
            t.render_us = monotonic_us();
            t.attempt += 1;
            t.damage_rects = 0;
            t.damage_pixels = 0;
            t.render_reason = "none";
            t.present_reason = "none";
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
        let line = serde_json::json!({
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
            "input_cpu_us": trace.cpu[0]/1000, "art_load_cpu_us": trace.cpu[1]/1000,
            "component_prep_cpu_us": trace.cpu[2]/1000, "compose_paint_cpu_us": trace.cpu[3]/1000,
            "flush_cpu_us": trace.cpu[4]/1000,
            "render_cpu_us": trace.cpu[1..].iter().sum::<u64>()/1000,
            "text_cpu_us": trace.cpu[5]/1000, "artwork_cpu_us": trace.cpu[6]/1000, "blit_cpu_us": trace.cpu[7]/1000,
            "wake_monotonic_us": trace.wake_us, "input_monotonic_us": trace.input_us,
            "render_start_monotonic_us": trace.render_us,
            "render_end_monotonic_us": monotonic_us(), "render_attempt": trace.attempt,
            "ram_hit": trace.hits[0], "disk_cache_hit": trace.hits[1], "png_decode": trace.hits[2],
            "wall_us": trace.started.map_or(0, |start| start.elapsed().as_micros()),
        })
        .to_string()
            + "\n";
        if append_bounded(&mut trace.file, line.as_bytes()).is_err() {
            *state = None;
            return;
        }
        trace.cpu = [0; 8];
        trace.hits = [0; 3];
        trace.started = None;
    });
}

fn append_bounded(file: &mut File, line: &[u8]) -> std::io::Result<()> {
    append_line(file, line, false)
}
fn append_line(file: &mut File, line: &[u8], rolling: bool) -> std::io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
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
        assert!(TRACE.with(|trace| trace.borrow().is_none()));
        for line in std::fs::read_to_string(&path).unwrap().lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
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
        assert!(TRACE.with(|trace| trace.borrow().is_none()));
        for line in data.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["menu_phase"], "first_frame_prepare");
            assert!(value["wall_us"].is_number() && value["cpu_us"].is_number());
        }
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
        assert_eq!(std::fs::metadata(&path).unwrap().len(), LOG_LIMIT);
        assert!(TRACE.with(|t| t.borrow().is_none()));
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
            let line = serde_json::json!({
                "menu_phase": self.name, "wall_us": started.elapsed().as_micros(),
                "cpu_us": cpu / 1000,
            })
            .to_string()
                + "\n";
            if append_bounded(&mut trace.file, line.as_bytes()).is_err() {
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
pub fn present_reason(reason: &'static str) {
    TRACE.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.present_reason = reason;
        }
    });
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
pub fn probe_counts() -> serde_json::Value {
    TRACE.with(|t| {
        let t = t.borrow(); let t = t.as_ref().unwrap();
        serde_json::json!({"stages":stage_values(t),"compose_paint_cpu_us":t.cpu[3]/1000,"render_cpu_us":t.cpu[1..].iter().sum::<u64>()/1000,"composed_pixels":t.counts[6],"ink_bitmap_pixels":t.counts[13],"legacy_composed_pixels":t.counts[6].saturating_sub(t.counts[13]),"copied_pixels":t.counts[4],
            "converted_pixels":t.counts[7],"flushed_pixels":t.counts[5],
            "compared_pixels":t.counts[8],"scanned_pixels":t.counts[9],"changed_pixels":t.counts[10],
            "image_hits":t.counts[11],"image_misses":t.counts[12],
            "text_hits":t.counts[0],"text_misses":t.counts[1],"blit_map_builds":t.counts[3],
            "damage_rects":t.damage_rects,"damage_pixels":t.damage_pixels,
            "render_reason":t.render_reason,"full_present_reason":t.present_reason})
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
}
pub struct StageGuard {
    stage: Stage,
    start: Option<u64>,
}
pub fn stage(stage: Stage) -> StageGuard {
    let start = TRACE.with(|state| {
        let mut state = state.borrow_mut();
        let trace = state.as_mut()?;
        let now = monotonic_us();
        if trace.stage_begin[stage as usize] == 0 {
            trace.stage_begin[stage as usize] = now;
        }
        Some(now)
    });
    StageGuard { stage, start }
}
impl Drop for StageGuard {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            let end = monotonic_us();
            TRACE.with(|state| {
                if let Some(trace) = state.borrow_mut().as_mut() {
                    trace.stages[self.stage as usize] += end.saturating_sub(start);
                    trace.stage_end[self.stage as usize] = end;
                }
            });
        }
    }
}
fn stage_values(trace: &Trace) -> serde_json::Value {
    let mut stages = serde_json::Map::new();
    for (i, name) in [
        "selection",
        "selected_info",
        "artwork_lookup",
        "decode",
        "layout",
        "raster",
        "present",
        "disk_cache",
    ]
    .iter()
    .enumerate()
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
        }
        begin_render();
        for which in [
            Stage::SelectedInfo,
            Stage::ArtworkLookup,
            Stage::Layout,
            Stage::Raster,
            Stage::Present,
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
            "selection",
            "selected_info",
            "artwork_lookup",
            "layout",
            "raster",
            "present",
        ] {
            let start = phases[name]["begin_monotonic_us"].as_u64().unwrap();
            let end = phases[name]["end_monotonic_us"].as_u64().unwrap();
            assert!(start >= input && end >= start, "{name}: {row}");
        }
        assert_eq!(phases["decode"]["wall_us"], 0);
        TRACE.with(|trace| *trace.borrow_mut() = None);
        std::fs::remove_file(path).unwrap();
    }
}
