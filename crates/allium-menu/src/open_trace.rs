use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const LIMIT: u64 = 256 * 1024;
const QUEUE_CAPACITY: usize = 256;

enum TraceMessage {
    Record(String),
    Drain(SyncSender<()>),
}

struct TraceQueue {
    sender: SyncSender<TraceMessage>,
    dropped: Arc<AtomicU64>,
}

static LOG: OnceLock<Option<TraceQueue>> = OnceLock::new();
const DRAIN_LIMIT: Duration = Duration::from_millis(100);

impl TraceQueue {
    fn spawn(path: PathBuf) -> std::io::Result<(Self, JoinHandle<()>)> {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dropped = dropped.clone();
        let writer = thread::Builder::new()
            .name("menu-open-trace".into())
            .spawn(move || {
                let mut file = OpenOptions::new().create(true).append(true).open(path).ok();
                write_records(receiver, &worker_dropped, |line| {
                    let ok = file.as_mut().is_some_and(|file| append_event(file, line));
                    if !ok {
                        file = None;
                    }
                    ok
                });
            })?;
        Ok((Self { sender, dropped }, writer))
    }

    fn enqueue(&self, line: String) {
        if line.len() as u64 > LIMIT || self.sender.try_send(TraceMessage::Record(line)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn drain(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut message = TraceMessage::Drain(sender);
        loop {
            match self.sender.try_send(message) {
                Ok(()) => {
                    return receiver
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .is_ok();
                }
                Err(TrySendError::Disconnected(_)) => return false,
                Err(TrySendError::Full(returned)) => message = returned,
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

fn write_records(
    receiver: Receiver<TraceMessage>,
    dropped: &AtomicU64,
    mut append: impl FnMut(&[u8]) -> bool,
) {
    for message in receiver {
        match message {
            TraceMessage::Record(line) => {
                if !append(line.as_bytes()) {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            TraceMessage::Drain(done) => {
                let _ = done.try_send(());
            }
        }
    }
}

/// Only controlled process shutdown may wait for the writer. [code]
pub fn drain_on_shutdown() {
    if let Some(Some(queue)) = LOG.get() {
        let drained = queue.drain(DRAIN_LIMIT);
        let dropped = queue.dropped.load(Ordering::Relaxed);
        if !drained || dropped != 0 {
            log::warn!("Menu trace shutdown: drained={drained}, dropped_records={dropped}");
        }
    }
}

pub struct ShutdownDrain;
impl Drop for ShutdownDrain {
    fn drop(&mut self) {
        drain_on_shutdown();
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OpenTrace {
    id: u64,
    released: Instant,
}

impl OpenTrace {
    pub fn start() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let trace = Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            released: Instant::now(),
        };
        trace.event("release_handler", None);
        trace
    }

    pub fn event(self, event: &'static str, phase: Option<&'static str>) {
        if !common::perf_trace::enabled() {
            return;
        }
        let Some(log) = LOG.get_or_init(|| {
            TraceQueue::spawn("/tmp/allium-menu-open-trace.log".into())
                .ok()
                .map(|(queue, _)| queue)
        }) else {
            return;
        };
        log.enqueue(self.line(event, phase, log.dropped.load(Ordering::Relaxed)));
    }

    fn line(self, event: &'static str, phase: Option<&'static str>, dropped: u64) -> String {
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        let epoch = EPOCH.get_or_init(Instant::now);
        serde_json::json!({
            "event": event, "phase": phase, "session": self.id, "dropped_records": dropped,
            "pid": std::process::id(), "monotonic_us": epoch.elapsed().as_micros(),
            "clock": "process_instant", "release_elapsed_us": self.released.elapsed().as_micros(),
        })
        .to_string()
            + "\n"
    }

    pub fn scope(self) -> common::perf_trace::OpenScope {
        common::perf_trace::open_scope(self.id, self.released, |id, released, event, phase| {
            Self { id, released }.event(event, phase);
        })
    }

    pub fn phase(self, name: &'static str) -> OpenPhase {
        self.event("phase_begin", Some(name));
        OpenPhase {
            trace: self,
            name,
            _phase: common::perf_trace::menu_phase(name),
        }
    }

    pub fn first_present(self) {
        self.event("first_present", None);
        common::perf_trace::network_event("first_present", "ingame_menu", Some(true));
    }
}

pub struct OpenPhase {
    trace: OpenTrace,
    name: &'static str,
    _phase: common::perf_trace::MenuPhaseGuard,
}
impl Drop for OpenPhase {
    fn drop(&mut self) {
        self.trace.event("phase_end", Some(self.name));
    }
}

fn append_event(file: &mut File, line: &[u8]) -> bool {
    if line.len() as u64 > LIMIT {
        return false;
    }
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    let mut start = metadata.len();
    if start.saturating_add(line.len() as u64) > LIMIT {
        if file.set_len(0).is_err() {
            return false;
        }
        start = 0;
    }
    if file.write_all(line).is_err() {
        let _ = file.set_len(start);
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf9_failed_accepted_and_queued_records_are_counted() {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let queue = TraceQueue {
            sender,
            dropped: dropped.clone(),
        };
        for i in 0..100 {
            queue.enqueue(format!("{i}\n"));
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        let worker_dropped = dropped.clone();
        let writer = thread::spawn(move || write_records(receiver, &worker_dropped, |_| false));
        assert!(queue.drain(Duration::from_secs(1)));
        assert_eq!(dropped.load(Ordering::Relaxed), 100);
        drop(queue);
        writer.join().unwrap();
    }

    #[test]
    fn perf9_open_failure_counts_records_and_shutdown_drain_is_bounded() {
        let path = std::env::temp_dir().join(format!("perf9-absent-{}/trace", std::process::id()));
        let (queue, writer) = TraceQueue::spawn(path).unwrap();
        for _ in 0..100 {
            queue.enqueue("accepted or rejected\n".into());
        }
        assert!(queue.drain(Duration::from_secs(1)));
        assert_eq!(queue.dropped.load(Ordering::Relaxed), 100);
        drop(queue);
        writer.join().unwrap();

        let (sender, receiver) = mpsc::sync_channel(1);
        let queue = TraceQueue {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        queue.enqueue("full\n".into());
        let start = Instant::now();
        assert!(!queue.drain(Duration::from_millis(10)));
        assert!(start.elapsed() < Duration::from_millis(250));
        drop(receiver);
    }

    #[test]
    fn perf9_shutdown_barrier_preserves_terminal_event() {
        let path = std::env::temp_dir().join(format!("perf9-drain-{}.jsonl", std::process::id()));
        let (queue, writer) = TraceQueue::spawn(path.clone()).unwrap();
        queue.enqueue(OpenTrace::start().line("first_present", None, 0));
        assert!(queue.drain(Duration::from_secs(1)));
        let data = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(data.trim()).unwrap()["event"],
            "first_present"
        );
        drop(queue);
        writer.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn session_identity_and_first_present_are_monotonic() {
        let first = OpenTrace::start();
        let release: serde_json::Value =
            serde_json::from_str(&first.line("release_handler", None, 0)).unwrap();
        let present: serde_json::Value =
            serde_json::from_str(&first.line("first_present", None, 0)).unwrap();
        let second = OpenTrace::start();
        assert_ne!(first.id, second.id);
        assert_eq!(release["session"], present["session"]);
        assert_eq!(present["event"], "first_present");
        assert!(
            present["monotonic_us"].as_u64().unwrap() >= release["monotonic_us"].as_u64().unwrap()
        );
        assert!(
            present["release_elapsed_us"].as_u64().unwrap()
                >= release["release_elapsed_us"].as_u64().unwrap()
        );
    }

    #[test]
    fn full_or_disconnected_queue_drops_and_counts_without_waiting() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let queue = TraceQueue {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        queue.enqueue("first\n".into());
        queue.enqueue("second\n".into());
        queue.enqueue("overflow\n".into());
        assert_eq!(queue.dropped.load(Ordering::Relaxed), 1);
        assert!(
            matches!(receiver.recv().unwrap(), TraceMessage::Record(line) if line == "first\n")
        );
        assert!(
            matches!(receiver.recv().unwrap(), TraceMessage::Record(line) if line == "second\n")
        );
        drop(receiver);
        queue.enqueue("disconnected\n".into());
        assert_eq!(queue.dropped.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn writer_thread_drains_complete_records_through_rollover() {
        let path = std::env::temp_dir().join(format!("perf8-writer-{}.jsonl", std::process::id()));
        let seed = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        seed.set_len(LIMIT).unwrap();
        drop(seed);
        let (queue, writer) = TraceQueue::spawn(path.clone()).unwrap();
        let trace = OpenTrace::start();
        // Blocking test sends make rollover deterministic; the menu uses only try_send.
        for _ in 0..12000 {
            queue
                .sender
                .send(TraceMessage::Record(trace.line(
                    "phase_end",
                    Some("present_write"),
                    0,
                )))
                .unwrap();
        }
        queue
            .sender
            .send(TraceMessage::Record(trace.line("first_present", None, 0)))
            .unwrap();
        drop(queue);
        writer.join().unwrap();
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(data.len() as u64 <= LIMIT);
        assert!(data.ends_with('\n'));
        let records: Vec<serde_json::Value> = data
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.last().unwrap()["event"], "first_present");
        assert_eq!(records.last().unwrap()["session"], trace.id);
        assert!(records.len() < 12000);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bounded_trace_contains_only_complete_records() {
        let path =
            std::env::temp_dir().join(format!("perf6-open-trace-{}.jsonl", std::process::id()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .unwrap();
        let trace = OpenTrace::start();
        for _ in 0..10000 {
            if !append_event(&mut file, trace.line("first_present", None, 0).as_bytes()) {
                break;
            }
        }
        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(bytes.len() as u64 <= LIMIT);
        assert!(bytes.ends_with('\n'));
        for line in bytes.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn rollover_and_initially_full_log_keep_terminal_first_present() {
        for initially_full in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "perf7-rollover-{}-{initially_full}.jsonl",
                std::process::id()
            ));
            let mut file = OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(&path)
                .unwrap();
            if initially_full {
                file.set_len(LIMIT).unwrap();
            }
            let trace = OpenTrace::start();
            for _ in 0..12000 {
                assert!(append_event(
                    &mut file,
                    trace.line("phase_end", Some("present_write"), 0).as_bytes()
                ));
            }
            assert!(append_event(
                &mut file,
                trace.line("first_present", None, 0).as_bytes()
            ));
            let data = std::fs::read_to_string(&path).unwrap();
            assert!(data.len() as u64 <= LIMIT);
            assert!(data.ends_with('\n'));
            let records: Vec<serde_json::Value> = data
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(records.last().unwrap()["event"], "first_present");
            assert_eq!(records.last().unwrap()["session"], trace.id);
            assert!(records.len() < 12000);
            std::fs::remove_file(path).unwrap();
        }
    }
}
