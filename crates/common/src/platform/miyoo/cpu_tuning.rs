use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{FileExt, MetadataExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub enum Workload {
    Frontend,
    Game,
}

impl Workload {
    fn values(self) -> [&'static str; 3] {
        match self {
            Self::Frontend => ["20000", "60", "1"],
            Self::Game => ["100000", "95", "1"],
        }
    }
}

const PARAMETERS: [&str; 3] = ["sampling_rate", "up_threshold", "sampling_down_factor"];

struct OndemandTuning {
    root: PathBuf,
    warned: [AtomicBool; 3],
    ownership_epoch: Mutex<u64>,
    input_boost: InputBoostWorker,
}

impl OndemandTuning {
    fn new(sysfs_root: &Path, lease_path: &Path) -> Self {
        Self {
            root: sysfs_root.join("devices/system/cpu/cpufreq/ondemand"),
            warned: std::array::from_fn(|_| AtomicBool::new(false)),
            ownership_epoch: Mutex::new(0),
            input_boost: InputBoostWorker::new(InputBoost::new(sysfs_root, lease_path)),
        }
    }

    fn set(&self, workload: Workload) -> u64 {
        let mut epoch = self
            .ownership_epoch
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *epoch = epoch.checked_add(1).expect("CPU tuning epoch exhausted");
        if matches!(workload, Workload::Game) {
            self.input_boost.restore();
        } else {
            self.input_boost.enable();
        }
        self.write_values(workload);
        *epoch
    }

    fn write_values(&self, workload: Workload) {
        for ((name, value), warned) in PARAMETERS.iter().zip(workload.values()).zip(&self.warned) {
            let path = self.root.join(name);
            let result = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .and_then(|mut file| file.write_all(value.as_bytes()));
            if let Err(error) = result {
                if !warned.swap(true, Ordering::Relaxed) {
                    log::warn!("Cannot tune {}: {error}", path.display());
                }
            }
        }
    }

    fn foreground(&self) -> ForegroundTuning<'_> {
        ForegroundTuning {
            tuning: self,
            epoch: self.set(Workload::Frontend),
        }
    }
}

pub struct ForegroundTuning<'a> {
    tuning: &'a OndemandTuning,
    epoch: u64,
}

impl Drop for ForegroundTuning<'_> {
    fn drop(&mut self) {
        let epoch = self
            .tuning
            .ownership_epoch
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *epoch == self.epoch {
            self.tuning.input_boost.restore();
            self.tuning.write_values(Workload::Game);
        }
    }
}

const BOOST_LEASE_PATH: &str = "/tmp/allium-cpu-boost";

#[derive(serde::Serialize, serde::Deserialize)]
struct BoostLease {
    policy: PathBuf,
    minimum: String,
}

fn open_lease_lock(path: &Path) -> io::Result<fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
}

struct LeaseLock(Arc<fs::File>);

impl Drop for LeaseLock {
    fn drop(&mut self) {
        // A concurrent spawn can inherit the open description until exec. Closing
        // only this descriptor would leave its flock held in that child meanwhile.
        let _ = self.0.unlock();
    }
}

fn try_lease_lock(path: &Path) -> io::Result<Option<LeaseLock>> {
    try_lease_lock_file(Arc::new(open_lease_lock(path)?))
}

fn try_lease_lock_file(file: Arc<fs::File>) -> io::Result<Option<LeaseLock>> {
    match file.try_lock() {
        Ok(()) => Ok(Some(LeaseLock(file))),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Inhibit new input boosts until the daemon resumes the launcher.
/// The OS releases the lock even if the daemon exits unexpectedly.
pub struct InputBoostPause {
    _lock: LeaseLock,
}

fn pause_input_boost_at(path: &Path) -> io::Result<InputBoostPause> {
    let lock = open_lease_lock(path)?;
    lock.lock()?;
    let lock = LeaseLock(Arc::new(lock));
    restore_locked_boost_lease_at(path)?;
    Ok(InputBoostPause { _lock: lock })
}

pub fn pause_input_boost() -> io::Result<InputBoostPause> {
    pause_input_boost_at(Path::new(BOOST_LEASE_PATH))
}

fn write_boost_lease(
    path: &Path, temporary: &Path, policy: &Path, minimum: &str, bytes: &mut Vec<u8>,
) -> io::Result<()> {
    #[derive(serde::Serialize)]
    struct Lease<'a> {
        policy: &'a Path,
        minimum: &'a str,
    }
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(temporary)?;
        bytes.clear();
        serde_json::to_writer(&mut *bytes, &Lease { policy, minimum })?;
        file.write_all(bytes)?;
        file.flush()?;
        fs::rename(temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn restore_boost_lease_at(path: &Path) -> io::Result<bool> {
    if !path.try_exists()? {
        return Ok(false);
    }
    let Some(_lock) = try_lease_lock(path)? else {
        return Ok(false);
    };
    restore_locked_boost_lease_at(path)
}

fn restore_locked_boost_lease_at(path: &Path) -> io::Result<bool> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let lease: BoostLease = serde_json::from_reader(file)?;
    let mut minimum = match OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(lease.policy.join("scaling_min_freq"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    minimum.write_all(lease.minimum.trim().as_bytes())?;
    fs::remove_file(path)?;
    Ok(true)
}

/// Recover a launcher's boost without relying on any process-local tuning state.
/// Keep the lease on failed restoration so a later owner can retry it.
pub fn restore_boost_lease() -> io::Result<bool> {
    restore_boost_lease_at(Path::new(BOOST_LEASE_PATH))
}

pub struct InputBoostGuard<'a> {
    tuning: &'a OndemandTuning,
}

impl Drop for InputBoostGuard<'_> {
    fn drop(&mut self) {
        self.tuning.input_boost.restore();
    }
}

/// Recover a previous process's lease at startup and restore this process on unwind.
pub fn input_boost_guard() -> InputBoostGuard<'static> {
    InputBoostGuard { tuning: system() }
}

const INPUT_BOOST_WINDOW: Duration = Duration::from_millis(1500);

struct BoostRequests {
    enabled: bool,
    pending: bool,
    running: bool,
    shutdown: bool,
}

struct BoostWorkerShared {
    requests: Mutex<BoostRequests>,
    wake: Condvar,
    boost: Mutex<InputBoost>,
}

struct InputBoostWorker {
    shared: Arc<BoostWorkerShared>,
    thread: Option<JoinHandle<()>>,
}

impl InputBoostWorker {
    fn new(boost: InputBoost) -> Self {
        let shared = Arc::new(BoostWorkerShared {
            requests: Mutex::new(BoostRequests {
                enabled: true,
                pending: false,
                running: false,
                shutdown: false,
            }),
            wake: Condvar::new(),
            boost: Mutex::new(boost),
        });
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new().name("input-boost".into()).spawn(move || {
            crate::perf_trace::initialize();
            let mut deadline = None;
            let mut requests = worker.requests.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                while !requests.shutdown && !requests.pending {
                    if let Some(end) = deadline {
                        let now = Instant::now();
                        if now >= end { break; }
                        requests = worker.wake.wait_timeout(requests, end - now)
                            .unwrap_or_else(|e| e.into_inner()).0;
                    } else {
                        requests = worker.wake.wait(requests).unwrap_or_else(|e| e.into_inner());
                    }
                }
                if requests.shutdown { break; }
                let input = requests.pending;
                requests.pending = false;
                requests.running = true;
                #[cfg(test)]
                worker.wake.notify_all();
                drop(requests);
                {
                    let mut boost = worker.boost.lock().unwrap_or_else(|e| e.into_inner());
                    if input {
                        boost.input(Instant::now());
                    } else {
                        boost.expire(Instant::now());
                    }
                    deadline = boost.deadline;
                }
                requests = worker.requests.lock().unwrap_or_else(|e| e.into_inner());
                requests.running = false;
                worker.wake.notify_all();
                // A synchronous restore owns expiry while requests are disabled.
                if !requests.enabled { deadline = None; }
            }
        });
        let thread = match thread {
            Ok(thread) => Some(thread),
            Err(error) => {
                log::warn!("Cannot start input boost worker: {error}");
                shared.requests.lock().unwrap_or_else(|e| e.into_inner()).enabled = false;
                None
            }
        };
        Self { shared, thread }
    }

    fn input(&self) {
        // The worker never holds this lock across lease, sysfs, or trace I/O.
        let mut requests = self.shared.requests.lock().unwrap_or_else(|e| e.into_inner());
        if requests.enabled {
            requests.pending = true;
            drop(requests);
            self.shared.wake.notify_one();
        }
    }

    fn enable(&self) {
        if self.thread.is_some() {
            self.shared.requests.lock().unwrap_or_else(|e| e.into_inner()).enabled = true;
        }
    }

    fn restore(&self) {
        let mut requests = self.shared.requests.lock().unwrap_or_else(|e| e.into_inner());
        requests.enabled = false;
        requests.pending = false;
        #[cfg(test)]
        self.shared.wake.notify_all();
        while requests.running {
            requests = self.shared.wake.wait(requests).unwrap_or_else(|e| e.into_inner());
        }
        // Hold the request gate until restoration finishes, including after a poisoned lock.
        self.shared.boost.lock().unwrap_or_else(|e| e.into_inner()).restore();
    }
}

impl Drop for InputBoostWorker {
    fn drop(&mut self) {
        self.restore();
        self.shared.requests.lock().unwrap_or_else(|e| e.into_inner()).shutdown = true;
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct PolicyFiles {
    minimum: fs::File,
    maximum: fs::File,
    identities: [(u64, u64); 2],
}

impl PolicyFiles {
    fn open(minimum: &Path, maximum: &Path) -> io::Result<Self> {
        let minimum = OpenOptions::new().read(true).write(true).open(minimum)?;
        let maximum = fs::File::open(maximum)?;
        let identities = [minimum.metadata()?, maximum.metadata()?]
            .map(|metadata| (metadata.dev(), metadata.ino()));
        Ok(Self { minimum, maximum, identities })
    }

    fn read(&self, paths: [&Path; 2], values: [&mut String; 2]) -> io::Result<()> {
        for (((file, identity), path), value) in [&self.minimum, &self.maximum]
            .into_iter().zip(self.identities).zip(paths).zip(values)
        {
            let metadata = fs::metadata(path)?;
            if (metadata.dev(), metadata.ino()) != identity {
                return Err(io::Error::other("CPU policy changed"));
            }
            let mut bytes = [0; 64];
            let len = file.read_at(&mut bytes, 0)?;
            if len == 0 || len == bytes.len() {
                return Err(io::Error::other("Invalid CPU frequency length"));
            }
            let text = std::str::from_utf8(&bytes[..len])
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            value.clear();
            value.push_str(text);
        }
        Ok(())
    }

    fn write_min(&mut self, path: &Path, value: &str) -> io::Result<()> {
        let write = |file: &fs::File| -> io::Result<()> {
            if file.write_at(value.as_bytes(), 0)? != value.len() {
                return Err(io::ErrorKind::WriteZero.into());
            }
            Ok(())
        };
        if write(&self.minimum).is_err() {
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            let metadata = file.metadata()?;
            // The published lease belongs to the policy read before this write.
            if (metadata.dev(), metadata.ino()) != self.identities[0] {
                return Err(io::Error::other("CPU policy changed after lease publication"));
            }
            self.minimum = file;
            write(&self.minimum)?;
        }
        Ok(())
    }
}

struct InputBoost {
    sysfs: PathBuf,
    lease_path: PathBuf,
    lease_temporary: PathBuf,
    lease_lock: Option<Arc<fs::File>>,
    lease_bytes: Vec<u8>,
    min_path: PathBuf,
    max_path: PathBuf,
    files: Option<PolicyFiles>,
    minimum: String,
    maximum: String,
    previous: Option<String>,
    deadline: Option<Instant>,
    unavailable: bool,
}

impl InputBoost {
    fn new(sysfs: &Path, lease_path: &Path) -> Self {
        let recovery = restore_boost_lease_at(lease_path);
        if let Err(error) = &recovery {
            log::warn!("Cannot recover CPU boost lease: {error}");
        }
        let policy = Self::policy(sysfs);
        Self {
            sysfs: sysfs.to_owned(),
            lease_path: lease_path.to_owned(),
            lease_temporary: lease_path.with_extension(format!("{}.tmp", std::process::id())),
            lease_lock: None,
            lease_bytes: Vec::new(),
            min_path: policy.join("scaling_min_freq"),
            max_path: policy.join("scaling_max_freq"),
            files: None,
            minimum: String::new(),
            maximum: String::new(),
            previous: None,
            deadline: None,
            unavailable: recovery.is_err(),
        }
    }

    fn policy(sysfs: &Path) -> PathBuf {
        [
            "devices/system/cpu/cpufreq/policy0",
            "devices/system/cpu/cpu0/cpufreq",
        ]
        .into_iter()
        .map(|path| sysfs.join(path))
        .find(|path| path.join("scaling_min_freq").is_file())
        .unwrap_or_else(|| sysfs.join("devices/system/cpu/cpufreq/policy0"))
    }

    fn try_lock(&mut self) -> io::Result<Option<LeaseLock>> {
        for attempt in 0..2 {
            if self.lease_lock.is_none() {
                self.lease_lock = Some(Arc::new(open_lease_lock(&self.lease_path)?));
            }
            let result = try_lease_lock_file(Arc::clone(self.lease_lock.as_ref().unwrap()));
            if result.is_ok() || attempt == 1 {
                return result;
            }
            self.lease_lock = None;
        }
        unreachable!()
    }

    fn read_values(&mut self) -> io::Result<()> {
        for attempt in 0..2 {
            if attempt == 1 {
                self.files = None;
                let policy = Self::policy(&self.sysfs);
                self.min_path = policy.join("scaling_min_freq");
                self.max_path = policy.join("scaling_max_freq");
            }
            let result = (|| {
                if self.files.is_none() {
                    self.files = Some(PolicyFiles::open(&self.min_path, &self.max_path)?);
                }
                self.files.as_ref().unwrap().read(
                    [&self.min_path, &self.max_path], [&mut self.minimum, &mut self.maximum],
                )
            })();
            if result.is_ok() || attempt == 1 {
                return result;
            }
        }
        unreachable!()
    }

    fn input(&mut self, now: Instant) {
        if self.unavailable {
            return;
        }
        let _lock = match self.try_lock() {
            Ok(Some(lock)) => lock,
            _ => return,
        };
        // The daemon may have restored the lease while this process was stopped.
        if self.previous.is_some() && !self.lease_path.exists() {
            self.minimum = self.previous.take().unwrap();
            self.deadline = None;
        }
        if self.previous.is_none() {
            // Never replace an unrecovered baseline with the currently boosted minimum.
            if self.lease_path.exists() {
                match restore_locked_boost_lease_at(&self.lease_path) {
                    Ok(true) => {}
                    _ => return,
                }
            }
            if self.read_values().is_err() {
                self.unavailable = true;
                return;
            }
            let mut timing = crate::perf_trace::boost_work();
            if let Err(error) = timing.lease(|| write_boost_lease(
                &self.lease_path,
                &self.lease_temporary,
                self.min_path.parent().expect("CPU policy directory"),
                &self.minimum,
                &mut self.lease_bytes,
            )) {
                log::warn!("Cannot save CPU boost lease: {error}");
                return;
            }
            if let Err(error) = timing.sysfs(|| self.files.as_mut().unwrap()
                .write_min(&self.min_path, self.maximum.trim()))
            {
                log::warn!("Cannot boost {}: {error}", self.min_path.display());
                self.unavailable = true;
                return;
            }
            self.previous = Some(std::mem::take(&mut self.minimum));
        }
        self.deadline = Some(now + INPUT_BOOST_WINDOW);
    }

    fn expire(&mut self, now: Instant) {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.restore();
        }
    }

    fn restore(&mut self) {
        match restore_boost_lease_at(&self.lease_path) {
            Ok(_) => {
                if let Some(previous) = self.previous.take() {
                    self.minimum = previous;
                }
                self.deadline = None;
            }
            Err(error) => {
                log::warn!("Cannot restore {}: {error}", self.min_path.display());
                self.deadline = None;
                self.unavailable = true;
            }
        }
    }
}

/// Coalesce input requests without waiting for the lease or frequency switch.
pub fn boost_input() {
    system().input_boost.input();
}

/// Disable and drain requests before restoring for exit or game handoff.
pub fn restore_input_boost() {
    system().input_boost.restore();
}

fn system() -> &'static OndemandTuning {
    static TUNING: OnceLock<OndemandTuning> = OnceLock::new();
    TUNING.get_or_init(|| OndemandTuning::new(Path::new("/sys"), Path::new(BOOST_LEASE_PATH)))
}

pub fn set(workload: Workload) {
    system().set(workload);
}

pub fn foreground() -> ForegroundTuning<'static> {
    system().foreground()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(label: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("dl14-cpu-{label}-{}", std::process::id()));
            assert!(!root.exists());
            fs::create_dir_all(root.join("devices/system/cpu/cpufreq/ondemand")).unwrap();
            Self(root)
        }

        fn seed(&self, values: [&str; 3]) {
            let root = self.0.join("devices/system/cpu/cpufreq");
            for (name, value) in PARAMETERS.iter().zip(values) {
                fs::write(root.join("ondemand").join(name), value).unwrap();
            }
            fs::create_dir_all(root.join("policy0")).unwrap();
            fs::write(root.join("policy0/scaling_governor"), "ondemand").unwrap();
            fs::write(root.join("policy0/scaling_max_freq"), "1200000").unwrap();
        }

        fn assert_values(&self, values: [&str; 3]) {
            let root = self.0.join("devices/system/cpu/cpufreq");
            for (name, expected) in PARAMETERS.iter().zip(values) {
                assert_eq!(
                    fs::read_to_string(root.join("ondemand").join(name)).unwrap(),
                    expected,
                    "{name}"
                );
            }
            assert_eq!(
                fs::read_to_string(root.join("policy0/scaling_governor")).unwrap(),
                "ondemand"
            );
            assert_eq!(
                fs::read_to_string(root.join("policy0/scaling_max_freq")).unwrap(),
                "1200000"
            );
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn lease_unlock_is_not_delayed_by_an_inherited_open_description() {
        let f = Fixture::new("lease-unlock");
        let path = f.0.join("lease");
        let lock = try_lease_lock(&path).unwrap().unwrap();
        // A cloned descriptor has the same flock lifetime as one inherited at fork.
        let inherited = lock.0.try_clone().unwrap();
        assert!(try_lease_lock(&path).unwrap().is_none());
        drop(lock);
        let next = try_lease_lock(&path).unwrap();
        assert!(next.is_some(), "a completed transaction must release its lock before exec");
        drop(next);
        drop(inherited);
    }

    #[test]
    fn published_boost_lease_round_trips_and_removes_temporary_file() {
        let f = Fixture::new("lease-round-trip");
        let path = f.0.join("lease");
        let policy = f.0.join("policy with spaces");
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        write_boost_lease(&path, &temporary, &policy, "400000\n", &mut Vec::new()).unwrap();
        let lease: BoostLease = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(lease.policy, policy);
        assert_eq!(lease.minimum, "400000\n");
        assert!(!path.with_extension(format!("{}.tmp", std::process::id())).exists());
    }

    #[test]
    fn boost_lease_process_fixture() {
        let Some(root) = std::env::var_os("ALLIUM_BOOST_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let mut boost = InputBoost::new(&root, &root.join("lease"));
        let minimum = root.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        if std::env::var_os("ALLIUM_BOOST_TEST_CRASH").is_some() {
            boost.input(Instant::now());
            assert_eq!(fs::read_to_string(&minimum).unwrap(), "1200000");
            // An abrupt exit skips all guards and leaves only the on-disk lease.
            std::process::exit(0);
        }
        assert_eq!(fs::read_to_string(&minimum).unwrap(), "400000");
        assert!(!root.join("lease").exists());
        boost.input(Instant::now());
        boost.restore();
        assert_eq!(fs::read_to_string(&minimum).unwrap(), "400000");
    }

    #[test]
    fn crashed_launcher_lease_recovers_across_process_instances() {
        let f = Fixture::new("cross-process-boost");
        f.seed(["20000", "60", "1"]);
        let minimum =
            f.0.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        fs::write(&minimum, "400000").unwrap();
        for crash in [true, false] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "platform::cpu_tuning::tests::boost_lease_process_fixture",
                ])
                .env("ALLIUM_BOOST_TEST_ROOT", &f.0)
                .env_remove("ALLIUM_BOOST_TEST_CRASH");
            if crash {
                child.env("ALLIUM_BOOST_TEST_CRASH", "1");
            }
            let output = child.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            if crash {
                assert!(f.0.join("lease").exists());
                assert_eq!(fs::read_to_string(&minimum).unwrap(), "1200000");
            }
        }
        assert_eq!(fs::read_to_string(&minimum).unwrap(), "400000");
    }

    #[test]
    fn daemon_restore_lease_clears_boost_and_resumed_launcher_uses_new_baseline() {
        let f = Fixture::new("daemon-boost");
        f.seed(["20000", "60", "1"]);
        let minimum =
            f.0.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        let lease = f.0.join("lease");
        fs::write(&minimum, "400000").unwrap();
        let mut launcher = InputBoost::new(&f.0, &lease);
        launcher.input(Instant::now());
        // This is the daemon API's implementation, with an isolated lease path.
        assert!(restore_boost_lease_at(&lease).unwrap());
        assert_eq!(fs::read_to_string(&minimum).unwrap(), "400000");
        assert!(!restore_boost_lease_at(&lease).unwrap());
        fs::write(&minimum, "600000").unwrap();
        launcher.input(Instant::now());
        launcher.restore();
        assert_eq!(fs::read_to_string(&minimum).unwrap(), "600000");
    }

    #[test]
    fn daemon_suspend_inhibits_navigation_boost_until_resume() {
        let f = Fixture::new("suspended-boost");
        f.seed(["20000", "60", "1"]);
        let minimum =
            f.0.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        fs::write(&minimum, "400000").unwrap();
        let lease = f.0.join("lease");
        let mut launcher = InputBoost::new(&f.0, &lease);
        launcher.input(Instant::now());
        let pause = pause_input_boost_at(&lease).unwrap();
        assert_eq!(fs::read_to_string(&minimum).unwrap(), "400000");
        launcher.input(Instant::now());
        assert!(!lease.exists());
        assert_eq!(fs::read_to_string(&minimum).unwrap(), "400000");
        drop(pause);
        launcher.input(Instant::now());
        assert_eq!(fs::read_to_string(&minimum).unwrap(), "1200000");
        launcher.restore();
        assert_eq!(fs::read_to_string(&minimum).unwrap(), "400000");
    }

    #[test]
    fn panic_guard_restores_boost_even_with_poisoned_mutex() {
        let f = Fixture::new("panic-boost");
        f.seed(["20000", "60", "1"]);
        let minimum =
            f.0.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        fs::write(&minimum, "400000").unwrap();
        let tuning = OndemandTuning::new(&f.0, &f.0.join("lease"));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = InputBoostGuard { tuning: &tuning };
            let mut boost = tuning.input_boost.shared.boost.lock().unwrap();
            boost.input(Instant::now());
            panic!("launcher panic during boost");
        }));
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&minimum).unwrap(), "400000");
        assert!(!f.0.join("lease").exists());
    }

    #[test]
    fn failed_lease_publication_never_boosts_and_failed_restore_keeps_baseline() {
        let f = Fixture::new("failed-boost-lease");
        f.seed(["20000", "60", "1"]);
        let minimum =
            f.0.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        fs::write(&minimum, "400000").unwrap();
        let mut boost = InputBoost::new(&f.0, &f.0.join("missing/lease"));
        boost.input(Instant::now());
        assert_eq!(fs::read_to_string(&minimum).unwrap(), "400000");
        assert!(boost.previous.is_none());
        let lease = f.0.join("lease");
        let mut boost = InputBoost::new(&f.0, &lease);
        boost.input(Instant::now());
        fs::remove_file(&minimum).unwrap();
        assert!(!restore_boost_lease_at(&lease).unwrap());
        assert!(lease.exists());
        fs::create_dir(&minimum).unwrap();
        assert!(restore_boost_lease_at(&lease).is_err());
        assert!(lease.exists());
        fs::remove_dir(&minimum).unwrap();
        fs::write(&minimum, "1200000").unwrap();
        assert!(restore_boost_lease_at(&lease).unwrap());
        assert_eq!(fs::read_to_string(&minimum).unwrap(), "400000");
    }

    #[test]
    fn navigation_boost_extends_without_rewriting_and_restores_previous_min() {
        let f = Fixture::new("input-boost");
        f.seed(["20000", "60", "1"]);
        let min =
            f.0.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        fs::write(&min, "400000").unwrap();
        let mut boost = InputBoost::new(&f.0, &f.0.join("lease"));
        let now = Instant::now();
        boost.input(now);
        assert_eq!(fs::read_to_string(&min).unwrap(), "1200000");
        // An external sentinel proves extending the window does not write sysfs again.
        fs::write(&min, "1100000").unwrap();
        boost.input(now + Duration::from_secs(1));
        assert_eq!(fs::read_to_string(&min).unwrap(), "1100000");
        boost.expire(now + INPUT_BOOST_WINDOW);
        assert_eq!(fs::read_to_string(&min).unwrap(), "1100000");
        boost.expire(now + Duration::from_secs(1) + INPUT_BOOST_WINDOW);
        assert_eq!(fs::read_to_string(&min).unwrap(), "400000");
        assert!(boost.deadline.is_none());
        boost.input(now + Duration::from_secs(3));
        assert_eq!(fs::read_to_string(&min).unwrap(), "1200000");
        boost.restore();
        assert_eq!(fs::read_to_string(&min).unwrap(), "400000");
    }

    #[test]
    fn navigation_boost_missing_sysfs_is_a_noop() {
        let f = Fixture::new("no-input-boost");
        let mut boost = InputBoost::new(&f.0, &f.0.join("lease"));
        boost.input(Instant::now());
        boost.input(Instant::now());
        assert!(boost.unavailable);
        assert!(boost.previous.is_none());
        assert!(boost.deadline.is_none());
        assert!(!boost.min_path.exists());
    }

    #[test]
    fn cold_boost_reuses_descriptors_but_reads_fresh_minimum_and_maximum() {
        use std::os::fd::AsRawFd;

        let f = Fixture::new("reuse-boost");
        f.seed(["20000", "60", "1"]);
        let lease = f.0.join("lease");
        let mut boost = InputBoost::new(&f.0, &lease);
        let min = boost.min_path.clone();
        fs::write(&min, "400000\n").unwrap();
        let now = Instant::now();
        boost.input(now);
        let lock = Arc::clone(boost.lease_lock.as_ref().unwrap());
        let files = boost.files.as_ref().unwrap();
        let descriptors = (files.minimum.as_raw_fd(), files.maximum.as_raw_fd());
        let bytes_capacity = boost.lease_bytes.capacity();
        boost.expire(now + INPUT_BOOST_WINDOW);
        fs::write(&min, "600000\n").unwrap();
        fs::write(&boost.max_path, "1100000\n").unwrap();
        boost.input(now + Duration::from_secs(2));
        let saved: BoostLease = serde_json::from_slice(&fs::read(&lease).unwrap()).unwrap();
        assert_eq!(saved.minimum, "600000\n");
        assert_eq!(saved.policy, min.parent().unwrap());
        assert_eq!(fs::read_to_string(&min).unwrap(), "1100000");
        assert!(Arc::ptr_eq(&lock, boost.lease_lock.as_ref().unwrap()));
        let files = boost.files.as_ref().unwrap();
        assert_eq!(descriptors, (files.minimum.as_raw_fd(), files.maximum.as_raw_fd()));
        assert_eq!(bytes_capacity, boost.lease_bytes.capacity());
        assert!(try_lease_lock(&lease).unwrap().is_some());
        boost.restore();
        assert_eq!(fs::read_to_string(&min).unwrap(), "600000");
    }

    #[test]
    fn cold_boost_reopens_failed_read_and_write_descriptors() {
        let f = Fixture::new("reopen-boost");
        f.seed(["20000", "60", "1"]);
        let lease = f.0.join("lease");
        let mut boost = InputBoost::new(&f.0, &lease);
        fs::write(&boost.min_path, "400000").unwrap();
        boost.input(Instant::now());
        boost.restore();
        // Reading a write-only descriptor fails with EBADF without invalid ownership.
        boost.files.as_mut().unwrap().minimum = OpenOptions::new()
            .write(true).open(&boost.min_path).unwrap();
        fs::write(&boost.min_path, "600000").unwrap();
        boost.input(Instant::now());
        assert_eq!(fs::read_to_string(&boost.min_path).unwrap(), "1200000");
        boost.restore();
        assert_eq!(fs::read_to_string(&boost.min_path).unwrap(), "600000");
        boost.files.as_mut().unwrap().minimum = fs::File::open(&boost.min_path).unwrap();
        boost.input(Instant::now());
        assert_eq!(fs::read_to_string(&boost.min_path).unwrap(), "1200000");
        boost.restore();
        assert_eq!(fs::read_to_string(&boost.min_path).unwrap(), "600000");
        assert!(!boost.unavailable);
    }

    #[test]
    fn cold_boost_reopens_replaced_policy_and_falls_back_to_cpu0() {
        let f = Fixture::new("replace-policy-boost");
        f.seed(["20000", "60", "1"]);
        let lease = f.0.join("lease");
        let mut boost = InputBoost::new(&f.0, &lease);
        fs::write(&boost.min_path, "400000").unwrap();
        boost.input(Instant::now());
        boost.restore();
        let replacement = f.0.join("replacement");
        fs::write(&replacement, "600000").unwrap();
        fs::rename(&replacement, &boost.min_path).unwrap();
        boost.input(Instant::now());
        boost.restore();
        assert_eq!(fs::read_to_string(&boost.min_path).unwrap(), "600000");
        let policy = boost.min_path.parent().unwrap().to_owned();
        let fallback = f.0.join("devices/system/cpu/cpu0/cpufreq");
        fs::create_dir_all(fallback.parent().unwrap()).unwrap();
        fs::rename(&policy, &fallback).unwrap();
        fs::write(fallback.join("scaling_min_freq"), "500000").unwrap();
        boost.input(Instant::now());
        let saved: BoostLease = serde_json::from_slice(&fs::read(&lease).unwrap()).unwrap();
        assert_eq!(saved.policy, fallback);
        assert_eq!(saved.minimum, "500000");
        assert_eq!(fs::read_to_string(&boost.min_path).unwrap(), "1200000");
        boost.restore();
        assert_eq!(fs::read_to_string(&boost.min_path).unwrap(), "500000");
        assert!(!boost.unavailable);
    }

    #[test]
    fn game_handoff_cancels_navigation_boost() {
        let f = Fixture::new("boost-game");
        f.seed(["20000", "60", "1"]);
        let min =
            f.0.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        fs::write(&min, "400000").unwrap();
        let tuning = OndemandTuning::new(&f.0, &f.0.join("lease"));
        tuning.input_boost.shared.boost.lock().unwrap().input(Instant::now());
        tuning.set(Workload::Game);
        assert_eq!(fs::read_to_string(&min).unwrap(), "400000");
        assert!(tuning.input_boost.shared.boost.lock().unwrap().deadline.is_none());
    }

    fn wait_requests(worker: &InputBoostWorker, ready: impl Fn(&BoostRequests) -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        let mut requests = worker.shared.requests.lock().unwrap();
        while !ready(&requests) {
            let left = end.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "boost worker did not reach expected state");
            requests = worker.shared.wake.wait_timeout(requests, left).unwrap().0;
        }
    }

    #[test]
    fn boost_worker_coalesces_bursts_without_waiting_for_sysfs() {
        let f = Fixture::new("worker-coalesce");
        f.seed(["20000", "60", "1"]);
        let min = f.0.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        fs::write(&min, "400000").unwrap();
        let worker = InputBoostWorker::new(InputBoost::new(&f.0, &f.0.join("lease")));
        let boost = worker.shared.boost.lock().unwrap();
        worker.input();
        wait_requests(&worker, |r| r.running);
        // The worker is blocked at the transaction lock, but signaling still completes.
        for _ in 0..1000 { worker.input(); }
        {
            let requests = worker.shared.requests.lock().unwrap();
            assert!(requests.running && requests.pending);
        }
        drop(boost);
        wait_requests(&worker, |r| !r.running && !r.pending);
        assert_eq!(fs::read_to_string(&min).unwrap(), "1200000");
        // A coalesced extension must retain the original lease baseline.
        worker.restore();
        assert_eq!(fs::read_to_string(&min).unwrap(), "400000");
    }

    #[test]
    fn game_handoff_drains_inflight_and_discards_pending_worker_requests() {
        let f = Fixture::new("worker-handoff");
        f.seed(["20000", "60", "1"]);
        let min = f.0.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        fs::write(&min, "400000").unwrap();
        let tuning = OndemandTuning::new(&f.0, &f.0.join("lease"));
        let worker = &tuning.input_boost;
        thread::scope(|scope| {
            let boost = worker.shared.boost.lock().unwrap();
            worker.input();
            wait_requests(worker, |r| r.running);
            worker.input();
            assert!(worker.shared.requests.lock().unwrap().pending);
            let handoff = scope.spawn(|| tuning.set(Workload::Game));
            wait_requests(worker, |r| !r.enabled && !r.pending);
            assert!(!handoff.is_finished(), "handoff must wait for in-flight boost");
            worker.input();
            assert!(!worker.shared.requests.lock().unwrap().pending);
            drop(boost);
            handoff.join().unwrap();
        });
        assert_eq!(fs::read_to_string(&min).unwrap(), "400000");
        assert!(!f.0.join("lease").exists());
        worker.input();
        let requests = worker.shared.requests.lock().unwrap();
        assert!(!requests.enabled && !requests.pending && !requests.running);
        drop(requests);
        // Failed exec returns to the frontend and explicitly permits fresh requests.
        tuning.set(Workload::Frontend);
        worker.input();
        wait_requests(worker, |r| !r.running && !r.pending);
        assert_eq!(fs::read_to_string(&min).unwrap(), "1200000");
        worker.restore();
        assert_eq!(fs::read_to_string(&min).unwrap(), "400000");
    }

    #[test]
    fn boost_worker_expires_without_an_input_thread_wakeup() {
        let f = Fixture::new("worker-expiry");
        f.seed(["20000", "60", "1"]);
        let min = f.0.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        fs::write(&min, "400000").unwrap();
        let worker = InputBoostWorker::new(InputBoost::new(&f.0, &f.0.join("lease")));
        worker.input();
        wait_requests(&worker, |r| !r.running && !r.pending);
        assert_eq!(fs::read_to_string(&min).unwrap(), "1200000");
        let end = Instant::now() + Duration::from_secs(5);
        let mut requests = worker.shared.requests.lock().unwrap();
        while worker.shared.boost.lock().unwrap().deadline.is_some() {
            let left = end.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "worker did not expire boost");
            requests = worker.shared.wake.wait_timeout(requests, left).unwrap().0;
        }
        drop(requests);
        assert_eq!(fs::read_to_string(&min).unwrap(), "400000");
        assert!(!f.0.join("lease").exists());
    }

    #[test]
    fn daemon_pause_blocks_pending_worker_and_resumed_boost_reads_new_minimum() {
        let f = Fixture::new("worker-pause");
        f.seed(["20000", "60", "1"]);
        let min = f.0.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        let lease = f.0.join("lease");
        fs::write(&min, "400000").unwrap();
        let worker = InputBoostWorker::new(InputBoost::new(&f.0, &lease));
        let pause = pause_input_boost_at(&lease).unwrap();
        worker.input();
        wait_requests(&worker, |r| !r.running && !r.pending);
        assert_eq!(fs::read_to_string(&min).unwrap(), "400000");
        assert!(!lease.exists());
        fs::write(&min, "600000").unwrap();
        drop(pause);
        worker.input();
        wait_requests(&worker, |r| !r.running && !r.pending);
        assert_eq!(fs::read_to_string(&min).unwrap(), "1200000");
        worker.restore();
        assert_eq!(fs::read_to_string(&min).unwrap(), "600000");
    }

    #[test]
    fn dl14_frontend_selects_chosen_ondemand_values() {
        let f = Fixture::new("frontend");
        f.seed(["100000", "95", "1"]);
        let tuning = OndemandTuning::new(&f.0, &f.0.join("lease"));
        tuning.set(Workload::Frontend);
        f.assert_values(["20000", "60", "1"]);
    }

    #[test]
    fn dl14_game_handoff_restores_stock_then_return_reapplies_frontend() {
        let f = Fixture::new("handoff");
        f.seed(["20000", "60", "1"]);
        let tuning = OndemandTuning::new(&f.0, &f.0.join("lease"));
        tuning.set(Workload::Game);
        f.assert_values(["100000", "95", "1"]);
        tuning.set(Workload::Frontend);
        f.assert_values(["20000", "60", "1"]);
    }

    #[test]
    fn dl14_menu_scope_restores_game_values_on_error() {
        let f = Fixture::new("menu");
        f.seed(["100000", "95", "1"]);
        let tuning = OndemandTuning::new(&f.0, &f.0.join("lease"));
        let result: Result<(), ()> = (|| {
            let _foreground = tuning.foreground();
            f.assert_values(["20000", "60", "1"]);
            Err(())
        })();
        assert!(result.is_err());
        f.assert_values(["100000", "95", "1"]);
    }

    #[test]
    fn dl14b_launcher_respawn_supersedes_menu_restore() {
        let f = Fixture::new("launcher-respawn");
        f.seed(["100000", "95", "1"]);
        let tuning = OndemandTuning::new(&f.0, &f.0.join("lease"));
        let foreground = tuning.foreground();
        tuning.set(Workload::Frontend);
        drop(foreground);
        f.assert_values(["20000", "60", "1"]);
    }

    #[test]
    fn dl14b_current_menu_owner_restores_game() {
        let f = Fixture::new("current-menu-owner");
        f.seed(["100000", "95", "1"]);
        let tuning = OndemandTuning::new(&f.0, &f.0.join("lease"));
        let foreground = tuning.foreground();
        f.assert_values(["20000", "60", "1"]);
        drop(foreground);
        f.assert_values(["100000", "95", "1"]);
    }

    #[test]
    fn dl14b_concurrent_menu_drop_and_launcher_respawn_preserve_last_owner() {
        use std::{sync::Barrier, thread};

        let f = Fixture::new("concurrent-menu-owner");
        f.seed(["100000", "95", "1"]);
        let tuning = OndemandTuning::new(&f.0, &f.0.join("lease"));
        for round in 0..256 {
            let foreground = tuning.foreground();
            let start = Barrier::new(2);
            thread::scope(|scope| {
                scope.spawn(|| {
                    start.wait();
                    if round % 2 == 0 {
                        thread::yield_now();
                    }
                    drop(foreground);
                });
                scope.spawn(|| {
                    start.wait();
                    if round % 2 != 0 {
                        thread::yield_now();
                    }
                    tuning.set(Workload::Frontend);
                });
            });
            f.assert_values(["20000", "60", "1"]);
        }
    }

    #[test]
    fn dl14_missing_sysfs_is_best_effort_and_warns_once_per_parameter() {
        let f = Fixture::new("missing");
        let tuning = OndemandTuning::new(&f.0, &f.0.join("lease"));
        fs::write(tuning.root.join("sampling_rate"), "100000").unwrap();
        fs::create_dir(tuning.root.join("sampling_down_factor")).unwrap();
        tuning.set(Workload::Frontend);
        tuning.set(Workload::Frontend);
        assert_eq!(
            fs::read_to_string(tuning.root.join("sampling_rate")).unwrap(),
            "20000"
        );
        assert!(
            !tuning.root.join("up_threshold").exists(),
            "must not create missing sysfs files"
        );
        assert_eq!(
            tuning.warned.each_ref().map(|v| v.load(Ordering::Relaxed)),
            [false, true, true]
        );
    }
}
