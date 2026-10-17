use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
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
    input_boost: Mutex<InputBoost>,
}

impl OndemandTuning {
    fn new(sysfs_root: &Path, lease_path: &Path) -> Self {
        Self {
            root: sysfs_root.join("devices/system/cpu/cpufreq/ondemand"),
            warned: std::array::from_fn(|_| AtomicBool::new(false)),
            ownership_epoch: Mutex::new(0),
            input_boost: Mutex::new(InputBoost::new(sysfs_root, lease_path)),
        }
    }

    fn set(&self, workload: Workload) -> u64 {
        let mut epoch = self
            .ownership_epoch
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *epoch = epoch.checked_add(1).expect("CPU tuning epoch exhausted");
        if matches!(workload, Workload::Game) {
            self.input_boost
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .restore();
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

fn try_lease_lock(path: &Path) -> io::Result<Option<fs::File>> {
    let file = open_lease_lock(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Inhibit new input boosts until the daemon resumes the launcher.
/// The OS releases the lock even if the daemon exits unexpectedly.
pub struct InputBoostPause {
    _lock: fs::File,
}

fn pause_input_boost_at(path: &Path) -> io::Result<InputBoostPause> {
    let lock = open_lease_lock(path)?;
    lock.lock()?;
    restore_locked_boost_lease_at(path)?;
    Ok(InputBoostPause { _lock: lock })
}

pub fn pause_input_boost() -> io::Result<InputBoostPause> {
    pause_input_boost_at(Path::new(BOOST_LEASE_PATH))
}

fn write_boost_lease(path: &Path, policy: &Path, minimum: &str) -> io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        let bytes = serde_json::to_vec(&BoostLease {
            policy: policy.to_owned(),
            minimum: minimum.to_owned(),
        })?;
        file.write_all(&bytes)?;
        file.flush()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
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
        self.tuning
            .input_boost
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .restore();
    }
}

/// Recover a previous process's lease at startup and restore this process on unwind.
pub fn input_boost_guard() -> InputBoostGuard<'static> {
    InputBoostGuard { tuning: system() }
}

const INPUT_BOOST_WINDOW: Duration = Duration::from_millis(1500);

struct InputBoost {
    lease_path: PathBuf,
    min_path: PathBuf,
    max_path: PathBuf,
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
        let policy = [
            "devices/system/cpu/cpufreq/policy0",
            "devices/system/cpu/cpu0/cpufreq",
        ]
        .into_iter()
        .map(|path| sysfs.join(path))
        .find(|path| path.join("scaling_min_freq").is_file())
        .unwrap_or_else(|| sysfs.join("devices/system/cpu/cpufreq/policy0"));
        Self {
            lease_path: lease_path.to_owned(),
            min_path: policy.join("scaling_min_freq"),
            max_path: policy.join("scaling_max_freq"),
            previous: None,
            deadline: None,
            unavailable: recovery.is_err(),
        }
    }

    fn write_min(&self, value: &str) -> std::io::Result<()> {
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.min_path)?
            .write_all(value.as_bytes())
    }

    fn input(&mut self, now: Instant) {
        if self.unavailable {
            return;
        }
        let _lock = match try_lease_lock(&self.lease_path) {
            Ok(Some(lock)) => lock,
            _ => return,
        };
        // The daemon may have restored the lease while this process was stopped.
        if self.previous.is_some() && !self.lease_path.exists() {
            self.previous = None;
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
            let values = fs::read_to_string(&self.min_path)
                .and_then(|min| fs::read_to_string(&self.max_path).map(|max| (min, max)));
            let Ok((min, max)) = values else {
                self.unavailable = true;
                return;
            };
            if let Err(error) = write_boost_lease(
                &self.lease_path,
                self.min_path.parent().expect("CPU policy directory"),
                &min,
            ) {
                log::warn!("Cannot save CPU boost lease: {error}");
                return;
            }
            if let Err(error) = self.write_min(max.trim()) {
                log::warn!("Cannot boost {}: {error}", self.min_path.display());
                self.unavailable = true;
                return;
            }
            self.previous = Some(min);
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
                self.previous = None;
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

/// Extend the input boost without repeating sysfs writes within its window.
pub fn boost_input() {
    system()
        .input_boost
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .input(Instant::now());
}

pub fn restore_input_boost() {
    system()
        .input_boost
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .restore();
}

pub fn expire_input_boost() {
    system()
        .input_boost
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expire(Instant::now());
}

pub fn input_boost_deadline() -> Option<Instant> {
    system()
        .input_boost
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .deadline
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
    fn published_boost_lease_round_trips_and_removes_temporary_file() {
        let f = Fixture::new("lease-round-trip");
        let path = f.0.join("lease");
        let policy = f.0.join("policy with spaces");
        write_boost_lease(&path, &policy, "400000\n").unwrap();
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
        let result = std::panic::catch_unwind(|| {
            let _guard = InputBoostGuard { tuning: &tuning };
            let mut boost = tuning.input_boost.lock().unwrap();
            boost.input(Instant::now());
            panic!("launcher panic during boost");
        });
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
    fn game_handoff_cancels_navigation_boost() {
        let f = Fixture::new("boost-game");
        f.seed(["20000", "60", "1"]);
        let min =
            f.0.join("devices/system/cpu/cpufreq/policy0/scaling_min_freq");
        fs::write(&min, "400000").unwrap();
        let tuning = OndemandTuning::new(&f.0, &f.0.join("lease"));
        tuning.input_boost.lock().unwrap().input(Instant::now());
        tuning.set(Workload::Game);
        assert_eq!(fs::read_to_string(&min).unwrap(), "400000");
        assert!(tuning.input_boost.lock().unwrap().deadline.is_none());
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
