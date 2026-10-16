use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
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
}

impl OndemandTuning {
    fn new(sysfs_root: &Path) -> Self {
        Self {
            root: sysfs_root.join("devices/system/cpu/cpufreq/ondemand"),
            warned: std::array::from_fn(|_| AtomicBool::new(false)),
            ownership_epoch: Mutex::new(0),
        }
    }

    fn set(&self, workload: Workload) -> u64 {
        let mut epoch = self
            .ownership_epoch
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *epoch = epoch.checked_add(1).expect("CPU tuning epoch exhausted");
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

fn system() -> &'static OndemandTuning {
    static TUNING: OnceLock<OndemandTuning> = OnceLock::new();
    TUNING.get_or_init(|| OndemandTuning::new(Path::new("/sys")))
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
    fn dl14_frontend_selects_chosen_ondemand_values() {
        let f = Fixture::new("frontend");
        f.seed(["100000", "95", "1"]);
        let tuning = OndemandTuning::new(&f.0);
        tuning.set(Workload::Frontend);
        f.assert_values(["20000", "60", "1"]);
    }

    #[test]
    fn dl14_game_handoff_restores_stock_then_return_reapplies_frontend() {
        let f = Fixture::new("handoff");
        f.seed(["20000", "60", "1"]);
        let tuning = OndemandTuning::new(&f.0);
        tuning.set(Workload::Game);
        f.assert_values(["100000", "95", "1"]);
        tuning.set(Workload::Frontend);
        f.assert_values(["20000", "60", "1"]);
    }

    #[test]
    fn dl14_menu_scope_restores_game_values_on_error() {
        let f = Fixture::new("menu");
        f.seed(["100000", "95", "1"]);
        let tuning = OndemandTuning::new(&f.0);
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
        let tuning = OndemandTuning::new(&f.0);
        let foreground = tuning.foreground();
        tuning.set(Workload::Frontend);
        drop(foreground);
        f.assert_values(["20000", "60", "1"]);
    }

    #[test]
    fn dl14b_current_menu_owner_restores_game() {
        let f = Fixture::new("current-menu-owner");
        f.seed(["100000", "95", "1"]);
        let tuning = OndemandTuning::new(&f.0);
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
        let tuning = OndemandTuning::new(&f.0);
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
        let tuning = OndemandTuning::new(&f.0);
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
