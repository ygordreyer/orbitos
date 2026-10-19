use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, io::AsRawFd},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tiny_skia::Pixmap;

const MAGIC: &[u8; 8] = b"ALORART\0";
const TRANSFORM_VERSION: u32 = 6;
const HEADER_BYTES: usize = 84;
const DISK_BYTES: u64 = 512 * 1024 * 1024;
// Art writes only need room to keep the card usable, not the network-library ROM reserve.
const ART_RESERVE_BYTES: u64 = 256 * 1024 * 1024;
const WRITE_MARGIN: u64 = 1024 * 1024;
static WRITER: Mutex<()> = Mutex::new(());
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) struct ArtKey {
    hash: [u8; 32],
    width: u32,
    height: u32,
}
impl ArtKey {
    pub fn new(
        path: &Path,
        metadata: &Metadata,
        source_bytes: &[u8],
        source: (u32, u32),
        target: (u32, u32),
        crop: bool,
    ) -> Option<Self> {
        Self::with_fit(path, metadata, source_bytes, source, target, crop, None)
    }
    pub fn with_fit(
        path: &Path,
        metadata: &Metadata,
        source_bytes: &[u8],
        source: (u32, u32),
        target: (u32, u32),
        crop: bool,
        fit: Option<common::theme::pack::Fit>,
    ) -> Option<Self> {
        let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        let mut hash = Sha256::new();
        let path = path.as_os_str().as_encoded_bytes();
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path);
        hash.update(Sha256::digest(source_bytes));
        for value in [
            metadata.len(),
            modified.as_secs(),
            u64::from(modified.subsec_nanos()),
            u64::from(source.0),
            u64::from(source.1),
            u64::from(target.0),
            u64::from(target.1),
            u64::from(crop),
            match fit { None => 0, Some(common::theme::pack::Fit::Contain) => 1, Some(common::theme::pack::Fit::Cover) => 2 },
            u64::from(TRANSFORM_VERSION),
        ] {
            hash.update(value.to_le_bytes());
        }
        Some(Self {
            hash: hash.finalize().into(),
            width: target.0,
            height: target.1,
        })
    }
    fn header(&self, pixels: &[u8]) -> [u8; HEADER_BYTES] {
        let mut header = [0; HEADER_BYTES];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&TRANSFORM_VERSION.to_le_bytes());
        header[12..16].copy_from_slice(&self.width.to_le_bytes());
        header[16..20].copy_from_slice(&self.height.to_le_bytes());
        header[20..52].copy_from_slice(&self.hash);
        header[52..].copy_from_slice(&Sha256::digest(pixels));
        header
    }
    fn path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.rgba", const_hex::encode(self.hash)))
    }
    pub fn load(&self, dir: &Path) -> Option<Pixmap> {
        let path = self.path(dir);
        let mut file = File::open(&path).ok()?;
        let result = (|| {
            let bytes = u64::from(self.width)
                .checked_mul(u64::from(self.height))?
                .checked_mul(4)?;
            if file.metadata().ok()?.len() != HEADER_BYTES as u64 + bytes {
                return None;
            }
            let mut header = [0; HEADER_BYTES];
            file.read_exact(&mut header).ok()?;
            if header[..52] != self.header(&[])[..52] {
                return None;
            }
            let mut image = Pixmap::new(self.width, self.height)?;
            file.read_exact(image.data_mut()).ok()?;
            if header != self.header(image.data()) {
                return None;
            }
            Some(image)
        })();
        if result.is_none() {
            // Do not delete a replacement published after our read.
            if let Some(_writer) = CacheWriter::acquire_existing(dir) {
                use std::os::unix::fs::MetadataExt;
                if file
                    .metadata()
                    .ok()
                    .zip(fs::metadata(&path).ok())
                    .is_some_and(|(opened, current)| opened.ino() == current.ino())
                {
                    let _ = fs::remove_file(path);
                }
            }
        }
        result
    }
    pub fn store(&self, dir: &Path, image: &Pixmap) {
        let _ = self.publish(dir, image);
    }
    fn publish(&self, dir: &Path, image: &Pixmap) -> std::io::Result<()> {
        self.publish_with_space(dir, image, filesystem_space)
    }
    fn publish_with_space(
        &self,
        dir: &Path,
        image: &Pixmap,
        mut space: impl FnMut(&Path) -> Option<(u64, u64)>,
    ) -> std::io::Result<()> {
        let incoming = HEADER_BYTES as u64 + image.data().len() as u64;
        if !admit_initialization(dir, &mut space) {
            return Ok(());
        }
        fs::create_dir_all(dir)?;
        let Some(_writer) = CacheWriter::acquire(dir) else {
            return Ok(());
        };
        if !prepare_write(dir, incoming, || space(dir))? {
            return Ok(());
        }
        let temp = dir.join(format!(
            ".{}-{}.tmp",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let result = (|| {
            let recheck = |space: &mut dyn FnMut(&Path) -> Option<(u64, u64)>,
                           bytes|
             -> std::io::Result<()> {
                if space(dir).is_some_and(|(free, block)| admitted(free, bytes, block)) {
                    Ok(())
                } else {
                    Err(std::io::Error::other("Storage floor would be crossed"))
                }
            };
            recheck(&mut space, incoming)?;
            file.write_all(&self.header(image.data()))?;
            recheck(&mut space, image.data().len() as u64)?;
            file.write_all(image.data())?;
            drop(file);
            recheck(&mut space, 0)?;
            fs::rename(&temp, self.path(dir))?;
            Ok(())
        })();
        result
    }
}
fn cache_file(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        && path.extension().is_some_and(|s| s == "rgba")
}
struct CacheWriter {
    _process: std::sync::MutexGuard<'static, ()>,
    lock: File,
}
impl CacheWriter {
    fn acquire(dir: &Path) -> Option<Self> {
        Self::open(
            dir,
            OpenOptions::new().write(true).create(true).truncate(false),
        )
    }
    fn acquire_existing(dir: &Path) -> Option<Self> {
        Self::open(dir, OpenOptions::new().write(true))
    }
    fn open(dir: &Path, options: &OpenOptions) -> Option<Self> {
        let process = WRITER.try_lock().ok()?;
        let lock = options.open(dir.join(".cache.lock")).ok()?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return None;
        }
        Some(Self {
            _process: process,
            lock,
        })
    }
}
impl Drop for CacheWriter {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
fn initialization_plan(dir: &Path) -> Option<(&Path, u64)> {
    let mut missing_directories = 0u64;
    for ancestor in dir.ancestors().chain(std::iter::once(Path::new("."))) {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match fs::metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() => return Some((ancestor, missing_directories)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing_directories = missing_directories.checked_add(1)?;
            }
            _ => return None,
        }
    }
    None
}
fn admit_initialization(dir: &Path, space: &mut impl FnMut(&Path) -> Option<(u64, u64)>) -> bool {
    let Some((ancestor, missing_directories)) = initialization_plan(dir) else {
        return false;
    };
    let Some((available, block)) = space(ancestor) else {
        return false;
    };
    let Ok(lock_exists) = dir.join(".cache.lock").try_exists() else {
        return false;
    };
    missing_directories
        .checked_add(u64::from(!lock_exists))
        .and_then(|blocks| blocks.checked_mul(block))
        .is_some_and(|allocation| admitted(available, allocation, block))
}
fn filesystem_space(dir: &Path) -> Option<(u64, u64)> {
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    let block = stat.f_frsize as u64;
    Some(((stat.f_bavail as u64).checked_mul(block)?, block))
}
fn temp_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with('.') && s.ends_with(".tmp"))
}
fn admitted(available: u64, incoming: u64, block: u64) -> bool {
    if block == 0 {
        return false;
    }
    incoming
        .checked_add(block - 1)
        .map(|n| n / block)
        .and_then(|n| n.checked_mul(block))
        .and_then(|n| n.checked_add(WRITE_MARGIN))
        .and_then(|n| available.checked_sub(n))
        .is_some_and(|remaining| remaining >= ART_RESERVE_BYTES)
}
#[cfg(test)]
thread_local! { static INVENTORY_PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
fn prepare_write(
    dir: &Path,
    incoming: u64,
    mut space: impl FnMut() -> Option<(u64, u64)>,
) -> std::io::Result<bool> {
    let now = SystemTime::now();
    let mut total = 0u64;
    let mut candidates = Vec::new();
    let mut evictable = 0u64;
    let mut evictable_bytes = 0u64;
    #[cfg(test)]
    INVENTORY_PASSES.with(|n| n.set(n.get() + 1));
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() || !(cache_file(&path) || temp_file(&path)) {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified = metadata.modified()?;
        let stale = temp_file(&path) && now.duration_since(modified).is_ok_and(|age| age > Duration::from_secs(60));
        total = total.saturating_add(metadata.len());
        if cache_file(&path) || stale {
            use std::os::unix::fs::MetadataExt;
            let allocated = metadata.blocks().saturating_mul(512);
            evictable = evictable.saturating_add(allocated);
            evictable_bytes = evictable_bytes.saturating_add(metadata.len());
            candidates.push((!stale, modified, path, metadata.len(), allocated));
        }
    }
    candidates.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));
    let mut candidates = candidates.into_iter();
    loop {
        let Some((available, block)) = space() else { return Ok(false); };
        if total.saturating_sub(evictable_bytes).saturating_add(incoming) > DISK_BYTES
            || !admitted(available.saturating_add(evictable), incoming, block) {
            return Ok(false);
        }
        if total.saturating_add(incoming) <= DISK_BYTES && admitted(available, incoming, block)
            && candidates.as_slice().first().is_none_or(|candidate| candidate.0) {
            return Ok(true);
        }
        let Some((_, _, path, bytes, allocated)) = candidates.next() else { return Ok(false); };
        fs::remove_file(path)?;
        total = total.saturating_sub(bytes);
        evictable = evictable.saturating_sub(allocated);
        evictable_bytes = evictable_bytes.saturating_sub(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // WRITER's try-lock also covers unrelated directories. Share the launcher
    // proof gate so concurrent tests cannot silently skip writes or cleanup.
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn exact_pixels_validation_identity_and_eviction() {
        let fixture = Fixture::new();
        let dir = &fixture.0;
        let source = dir.join("source.png");
        fs::write(&source, b"source").unwrap();
        let key = ArtKey::new(
            &source,
            &File::open(&source).unwrap().metadata().unwrap(),
            &fs::read(&source).unwrap(),
            (101, 101),
            (3, 2),
            false,
        )
        .unwrap();
        let mut pixels = Pixmap::new(3, 2).unwrap();
        pixels.data_mut().copy_from_slice(&[
            1, 2, 3, 4, 7, 8, 9, 10, 0, 0, 0, 0, 11, 12, 13, 255, 34, 56, 78, 255, 1, 1, 1, 1,
        ]);
        key.store(&dir, &pixels);
        assert_eq!(key.load(&dir).unwrap().data(), pixels.data());
        let path = key.path(&dir);
        let original = fs::read(&path).unwrap();
        for offset in [0, 8, 12, 16, 20] {
            let mut corrupt = original.clone();
            corrupt[offset] ^= 1;
            fs::write(&path, corrupt).unwrap();
            assert!(key.load(&dir).is_none());
        }
        fs::write(&path, &original[..original.len() - 1]).unwrap();
        assert!(key.load(&dir).is_none());
        let mut extra = original.clone();
        extra.push(0);
        fs::write(&path, extra).unwrap();
        assert!(key.load(&dir).is_none());
        fs::write(&path, &original).unwrap();
        fs::write(&source, b"changed source size").unwrap();
        let changed = ArtKey::new(
            &source,
            &fs::metadata(&source).unwrap(),
            &fs::read(&source).unwrap(),
            (101, 101),
            (3, 2),
            false,
        )
        .unwrap();
        assert_ne!(changed.hash, key.hash);
        assert!(changed.load(&dir).is_none());
        let different = ArtKey::new(
            &source,
            &fs::metadata(&source).unwrap(),
            &fs::read(&source).unwrap(),
            (101, 101),
            (2, 3),
            false,
        )
        .unwrap();
        assert_ne!(different.hash, changed.hash);
        assert!(!prepare_write(&dir, DISK_BYTES + 1, || Some((u64::MAX, 4096))).unwrap());
        assert!(path.exists());
        assert!(source.exists());
        assert!(key.load(&source).is_none());
        key.store(&source, &pixels);
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn fitted_round_trip_separates_geometry_fit_version_and_source() {
        use common::theme::{pack::Fit, runtime::fit_image};
        let fixture = Fixture::new();
        let path = fixture.0.join("source.png");
        let mut source = Pixmap::new(13, 21).unwrap();
        source.fill(tiny_skia::Color::from_rgba8(30, 60, 90, 173));
        source.save_png(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let key = |dims, fit| ArtKey::with_fit(&path, &metadata, &bytes, (0, 0), dims, false, fit).unwrap();
        let fitted = key((9, 7), Some(Fit::Cover));
        let version_hash = |version: u32| -> [u8; 32] {
            let mut hash = Sha256::new();
            let path = path.as_os_str().as_encoded_bytes();
            hash.update((path.len() as u64).to_le_bytes());
            hash.update(path);
            hash.update(Sha256::digest(&bytes));
            let modified = metadata.modified().unwrap().duration_since(UNIX_EPOCH).unwrap();
            for value in [metadata.len(), modified.as_secs(), u64::from(modified.subsec_nanos()),
                0, 0, 9, 7, 0, 2, u64::from(version)] {
                hash.update(value.to_le_bytes());
            }
            hash.finalize().into()
        };
        assert_eq!(fitted.hash, version_hash(TRANSFORM_VERSION));
        assert_ne!(fitted.hash, version_hash(TRANSFORM_VERSION - 1));
        let pixels = fit_image(&source, 9, 7, Fit::Cover).unwrap();
        fitted.store(&fixture.0, &pixels);
        let loaded = fitted.load(&fixture.0).unwrap();
        assert_eq!((loaded.width(), loaded.height()), (9, 7));
        assert_eq!(loaded.data(), pixels.data());
        for other in [key((7, 9), Some(Fit::Cover)), key((9, 7), Some(Fit::Contain)), key((9, 7), None)] {
            assert_ne!(fitted.hash, other.hash);
            assert!(other.load(&fixture.0).is_none());
        }
        let mut old = fs::read(fitted.path(&fixture.0)).unwrap();
        old[8..12].copy_from_slice(&(TRANSFORM_VERSION - 1).to_le_bytes());
        fs::write(fitted.path(&fixture.0), old).unwrap();
        assert!(fitted.load(&fixture.0).is_none());
        let old_path = fixture.0.join(format!("{}.rgba", const_hex::encode(version_hash(TRANSFORM_VERSION - 1))));
        let mut old_payload = fitted.header(pixels.data()).to_vec();
        old_payload[8..12].copy_from_slice(&(TRANSFORM_VERSION - 1).to_le_bytes());
        old_payload[20..52].copy_from_slice(&version_hash(TRANSFORM_VERSION - 1));
        old_payload.extend_from_slice(pixels.data());
        fs::write(&old_path, old_payload).unwrap();
        assert!(fitted.load(&fixture.0).is_none());
        assert!(old_path.exists(), "old-version filename must be ignored");
        // Same length and mtime still cannot alias replaced source content.
        let mut changed = bytes.clone();
        changed[0] ^= 1;
        let replaced = ArtKey::with_fit(&path, &metadata, &changed, (0, 0), (9, 7), false, Some(Fit::Cover)).unwrap();
        assert_ne!(replaced.hash, fitted.hash);
        assert!(replaced.load(&fixture.0).is_none());
    }
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);
            loop {
                let dir = std::env::temp_dir().join(format!(
                    "allium-art-cache-test-{}-{}",
                    std::process::id(),
                    FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&dir) {
                    Ok(()) => return Self(dir),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        panic!("cannot create art cache fixture {}: {error}", dir.display())
                    }
                }
            }
        }
        fn key(&self, bytes: &[u8]) -> ArtKey {
            let source = self.0.join("source.png");
            fs::write(&source, bytes).unwrap();
            ArtKey::new(
                &source,
                &fs::metadata(&source).unwrap(),
                bytes,
                (3, 2),
                (3, 2),
                false,
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn wave2_low_space_preserves_cache_entries() {
        let dir = Fixture::new();
        let entry = dir.0.join(format!("{:064x}.rgba", 0));
        fs::write(&entry, b"cached pixels").unwrap();
        let stale = dir.0.join(".stale.tmp");
        fs::write(&stale, b"stale pixels").unwrap();
        File::options().write(true).open(&stale).unwrap().set_times(
            fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(61)),
        ).unwrap();
        assert!(!prepare_write(&dir.0, 100, || Some((128 * 1024 * 1024, 4096))).unwrap());
        assert_eq!(fs::read(&entry).unwrap(), b"cached pixels");
        assert_eq!(fs::read(&stale).unwrap(), b"stale pixels");
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn wave2_space_between_art_and_network_reserves_admits_publication() {
        let dir = Fixture::new();
        let key = dir.key(b"source");
        let image = Pixmap::new(3, 2).unwrap();
        let available = 512 * 1024 * 1024;
        assert!(available < crate::network_library::RESERVE_BYTES);
        key.publish_with_space(&dir.0, &image, |_| Some((available, 4096))).unwrap();
        assert_eq!(key.load(&dir.0).unwrap().data(), image.data());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn reserve_admission_rounds_allocation_and_fails_closed() {
        let incoming = 4097;
        let exact = ART_RESERVE_BYTES + WRITE_MARGIN + 8192;
        assert!(admitted(exact, incoming, 4096));
        assert!(!admitted(exact - 1, incoming, 4096));
        assert!(!admitted(ART_RESERVE_BYTES, incoming, 4096));
        assert!(!admitted(u64::MAX, incoming, 0));
        assert!(!admitted(u64::MAX, u64::MAX, 4096));
        let dir = Fixture::new();
        assert!(!prepare_write(&dir.0, incoming, || None).unwrap());
        assert!(!prepare_write(&dir.0, incoming, || Some((exact - 1, 4096))).unwrap());
        assert!(fs::read_dir(&dir.0).unwrap().next().is_none());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn failed_reserve_or_statvfs_skips_publication_before_temp_creation() {
        let dir = Fixture::new();
        let key = dir.key(b"source");
        let image = Pixmap::new(3, 2).unwrap();
        key.publish_with_space(&dir.0, &image, |_| Some((ART_RESERVE_BYTES, 4096)))
            .unwrap();
        key.publish_with_space(&dir.0, &image, |_| None).unwrap();
        assert!(!key.path(&dir.0).exists());
        assert!(
            !fs::read_dir(&dir.0)
                .unwrap()
                .any(|e| temp_file(&e.unwrap().path()))
        );
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn initialization_denied_space_leaves_missing_directory_absent() {
        let fixture = Fixture::new();
        let key = fixture.key(b"source");
        let dir = fixture.0.join("missing/cache");
        key.publish_with_space(&dir, &Pixmap::new(3, 2).unwrap(), |_| {
            Some((ART_RESERVE_BYTES, 4096))
        })
        .unwrap();
        assert!(!dir.join(".cache.lock").exists());
        assert!(!fixture.0.join("missing").exists());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn initialization_failed_statvfs_leaves_missing_directory_absent() {
        let fixture = Fixture::new();
        let key = fixture.key(b"source");
        let dir = fixture.0.join("missing/cache");
        key.publish_with_space(&dir, &Pixmap::new(3, 2).unwrap(), |_| None)
            .unwrap();
        assert!(!dir.join(".cache.lock").exists());
        assert!(!fixture.0.join("missing").exists());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn initialization_budgets_every_missing_directory_and_lock_block() {
        let fixture = Fixture::new();
        let key = fixture.key(b"source");
        let dir = fixture.0.join("missing/cache");
        let exact = ART_RESERVE_BYTES + WRITE_MARGIN + 3 * 4096;
        key.publish_with_space(&dir, &Pixmap::new(3, 2).unwrap(), |_| {
            Some((exact - 1, 4096))
        })
        .unwrap();
        assert!(!dir.join(".cache.lock").exists());
        assert!(!fixture.0.join("missing").exists());
        let mut checks = 0;
        key.publish_with_space(&dir, &Pixmap::new(3, 2).unwrap(), |path| {
            checks += 1;
            assert_eq!(path, if checks == 1 { &fixture.0 } else { &dir });
            Some((exact, 4096))
        })
        .unwrap();
        assert_eq!(checks, 5);
        assert!(dir.join(".cache.lock").exists());
        assert!(key.load(&dir).is_some());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn initialization_existing_directory_budgets_only_missing_lock() {
        let fixture = Fixture::new();
        let key = fixture.key(b"source");
        let image = Pixmap::new(3, 2).unwrap();
        let exact = ART_RESERVE_BYTES + WRITE_MARGIN + 4096;
        key.publish_with_space(&fixture.0, &image, |_| Some((exact - 1, 4096)))
            .unwrap();
        assert!(!fixture.0.join(".cache.lock").exists());
        assert!(!key.path(&fixture.0).exists());
        key.publish_with_space(&fixture.0, &image, |_| Some((exact, 4096)))
            .unwrap();
        assert!(fixture.0.join(".cache.lock").exists());
        assert_eq!(key.load(&fixture.0).unwrap().data(), image.data());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn initialization_existing_lock_failed_statvfs_creates_nothing() {
        let fixture = Fixture::new();
        let key = fixture.key(b"source");
        File::create(fixture.0.join(".cache.lock")).unwrap();
        let mut checks = 0;
        key.publish_with_space(&fixture.0, &Pixmap::new(3, 2).unwrap(), |path| {
            checks += 1;
            assert_eq!(path, &fixture.0);
            None
        })
        .unwrap();
        assert_eq!(checks, 1);
        assert!(!key.path(&fixture.0).exists());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn initialization_cleanup_without_lock_creates_nothing() {
        let fixture = Fixture::new();
        let key = fixture.key(b"source");
        let path = key.path(&fixture.0);
        fs::write(&path, b"corrupt").unwrap();
        assert!(key.load(&fixture.0).is_none());
        assert!(!fixture.0.join(".cache.lock").exists());
        assert_eq!(fs::read(path).unwrap(), b"corrupt");
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn young_temps_block_admission_without_deletion() {
        let dir = Fixture::new();
        let young = dir.0.join(".young.tmp");
        File::create(&young).unwrap().set_len(DISK_BYTES).unwrap();
        assert!(!prepare_write(&dir.0, 1, || Some((u64::MAX, 4096))).unwrap());
        assert!(young.exists());
        File::options().write(true).open(&young).unwrap().set_len(DISK_BYTES - 1024 * 1024).unwrap();
        let valid = dir.key(b"valid").path(&dir.0);
        fs::write(&valid, vec![1; 1024 * 1024]).unwrap();
        let before: Vec<_> = fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().path()).collect();
        assert!(!prepare_write(&dir.0, 2 * 1024 * 1024, || Some((512 * 1024 * 1024, 4096))).unwrap());
        assert_eq!(fs::metadata(&young).unwrap().len(), DISK_BYTES - 1024 * 1024);
        assert_eq!(fs::read(&valid).unwrap(), vec![1; 1024 * 1024]);
        assert!(before.iter().all(|p| p.exists()));
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn failed_publication_keeps_young_temp_until_stale_reclamation() {
        let dir = Fixture::new();
        let key = dir.key(b"source");
        fs::create_dir(key.path(&dir.0)).unwrap();
        assert!(
            key.publish_with_space(&dir.0, &Pixmap::new(3, 2).unwrap(), |_| Some((
                u64::MAX,
                4096
            )))
            .is_err()
        );
        let temps: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| temp_file(p))
            .collect();
        assert_eq!(
            temps.len(),
            1,
            "failed writes must also preserve young temp files"
        );
        assert!(prepare_write(&dir.0, 1, || Some((u64::MAX, 4096))).unwrap());
        assert!(temps[0].exists());
        File::options()
            .write(true)
            .open(&temps[0])
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(61)),
            )
            .unwrap();
        assert!(prepare_write(&dir.0, 1, || Some((u64::MAX, 4096))).unwrap());
        assert!(!temps[0].exists());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn corrupt_pixel_payload_is_deleted_as_a_miss() {
        let dir = Fixture::new();
        let key = dir.key(b"source");
        let image = Pixmap::new(3, 2).unwrap();
        key.store(&dir.0, &image);
        let path = key.path(&dir.0);
        let mut bytes = fs::read(&path).unwrap();
        bytes[HEADER_BYTES] ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(key.load(&dir.0).is_none());
        assert!(!path.exists());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn same_metadata_and_length_content_changes_invalidate_key() {
        let dir = Fixture::new();
        let key = dir.key(b"source");
        let source = dir.0.join("source.png");
        let metadata = fs::metadata(&source).unwrap();
        let changed = ArtKey::new(&source, &metadata, b"SOURCE", (3, 2), (3, 2), false).unwrap();
        assert_ne!(key.hash, changed.hash);
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn stale_temps_are_reclaimed_young_temps_count_and_oldest_entries_evict_before_allocation() {
        let dir = Fixture::new();
        let stale = dir.0.join(".stale.tmp");
        let young = dir.0.join(".young.tmp");
        let oldest = dir.0.join(format!("{}.rgba", "0".repeat(64)));
        let newest = dir.0.join(format!("{}.rgba", "1".repeat(64)));
        for path in [&stale, &young, &oldest, &newest] {
            File::create(path).unwrap().set_len(DISK_BYTES / 4).unwrap();
        }
        let old = SystemTime::now() - Duration::from_secs(61);
        for path in [&stale, &oldest] {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        let incoming = DISK_BYTES / 2;
        assert!(prepare_write(&dir.0, incoming, || Some((u64::MAX, 4096))).unwrap());
        assert!(!stale.exists());
        assert!(young.exists());
        assert!(!oldest.exists());
        assert!(newest.exists());
        assert!(!prepare_write(&dir.0, DISK_BYTES, || Some((u64::MAX, 4096))).unwrap());
        assert!(young.exists());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn publication_evicts_before_payload_admission_with_existing_lock() {
        let dir = Fixture::new();
        let source = dir.0.join("source.png");
        fs::write(&source, b"source").unwrap();
        let key = ArtKey::new(
            &source,
            &fs::metadata(&source).unwrap(),
            b"source",
            (32, 32),
            (32, 32),
            false,
        )
        .unwrap();
        File::create(dir.0.join(".cache.lock")).unwrap();
        let entry = dir.0.join(format!("{}.rgba", "0".repeat(64)));
        fs::write(&entry, vec![0; 8192]).unwrap();
        let image = Pixmap::new(32, 32).unwrap();
        let incoming = HEADER_BYTES as u64 + image.data().len() as u64;
        let available = ART_RESERVE_BYTES + WRITE_MARGIN;
        assert!(!admitted(available, incoming, 4096));
        assert!(admitted(available + 8192, incoming, 4096));
        let mut checks = 0;
        key.publish_with_space(&dir.0, &image, |path| {
            assert_eq!(path, &dir.0);
            checks += 1;
            Some((available + if entry.exists() { 0 } else { 8192 }, 4096))
        })
        .unwrap();
        assert!(!entry.exists(), "publication must evict the old entry");
        assert_eq!(checks, 6);
        assert_eq!(key.load(&dir.0).unwrap().data(), image.data());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn reserve_pressure_evicts_then_rechecks_actual_free_space() {
        let dir = Fixture::new();
        let entry = dir.0.join(format!("{}.rgba", "0".repeat(64)));
        fs::write(&entry, vec![0; (WRITE_MARGIN + 4096) as usize]).unwrap();
        let mut checks = 0;
        assert!(
            prepare_write(&dir.0, 100, || {
                checks += 1;
                Some((
                    if entry.exists() {
                        ART_RESERVE_BYTES
                    } else {
                        ART_RESERVE_BYTES + WRITE_MARGIN + 4096
                    },
                    4096,
                ))
            })
            .unwrap()
        );
        assert_eq!(checks, 2);
        assert!(!entry.exists());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn advisory_lock_contender() {
        let Some(dir) = std::env::var_os("ALLIUM_CACHE_LOCK_TEST") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let source = dir.join("source.png");
        let key = ArtKey::new(
            &source,
            &fs::metadata(&source).unwrap(),
            b"source",
            (3, 2),
            (3, 2),
            false,
        )
        .unwrap();
        key.store(&dir, &Pixmap::new(3, 2).unwrap());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn writers_skip_process_and_cross_process_lock_contention() {
        let dir = Fixture::new();
        let key = dir.key(b"source");
        let guard = CacheWriter::acquire(&dir.0).unwrap();
        let other = Fixture::new();
        assert!(CacheWriter::acquire(&other.0).is_none());
        key.store(&dir.0, &Pixmap::new(3, 2).unwrap());
        assert!(!key.path(&dir.0).exists());
        let run = || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "art_cache::tests::advisory_lock_contender",
                    "--test-threads=1",
                ])
                .env("ALLIUM_CACHE_LOCK_TEST", &dir.0)
                .status()
                .unwrap()
        };
        assert!(run().success());
        assert!(!key.path(&dir.0).exists());
        drop(guard);
        assert!(run().success());
        assert!(key.path(&dir.0).exists());
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    #[ignore = "explicit source hashing versus PNG decode measurement"]
    fn source_hash_cost_measurement() {
        let root = PathBuf::from(std::env::var_os("ALLIUM_SOURCE_HASH_ART").unwrap());
        fn collect(root: &Path, files: &mut Vec<PathBuf>) {
            for e in fs::read_dir(root).unwrap() {
                let e = e.unwrap();
                if e.file_type().unwrap().is_dir() {
                    collect(&e.path(), files);
                } else if e.path().extension().is_some_and(|x| x == "png") {
                    files.push(e.path());
                }
            }
        }
        let mut files = vec![];
        collect(&root, &mut files);
        files.sort();
        let mut hashes = vec![];
        let mut decodes = vec![];
        let mut total_bytes = 0;
        for path in files {
            let bytes = fs::read(&path).unwrap();
            if bytes.len() > 524288 || Pixmap::decode_png(&bytes).is_err() {
                continue;
            }
            total_bytes += bytes.len();
            for _ in 0..10 {
                let start = std::time::Instant::now();
                let data = fs::read(&path).unwrap();
                std::hint::black_box(Sha256::digest(&data));
                hashes.push(start.elapsed().as_nanos());
                let start = std::time::Instant::now();
                let data = fs::read(&path).unwrap();
                std::hint::black_box(Pixmap::decode_png(&data).unwrap());
                decodes.push(start.elapsed().as_nanos());
            }
        }
        hashes.sort();
        decodes.sort();
        assert!(!hashes.is_empty());
        println!(
            "SOURCE_KEY_MEASUREMENT {}",
            serde_json::json!({"images":hashes.len()/10,"bytes":total_bytes,"samples":hashes.len(),"read_sha256_p50_ns":hashes[(hashes.len()-1)/2],"read_decode_p50_ns":decodes[(decodes.len()-1)/2]})
        );
    }
    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn dl8_art_rechecks_before_each_payload_write() {
        for denied_check in [3, 4] {
            let dir = Fixture::new();
            let key = dir.key(b"source");
            let mut checks = 0;
            assert!(
                key.publish_with_space(&dir.0, &Pixmap::new(3, 2).unwrap(), |_| {
                    checks += 1;
                    Some((
                        if checks == denied_check {
                            ART_RESERVE_BYTES
                        } else {
                            u64::MAX
                        },
                        4096,
                    ))
                })
                .is_err()
            );
            assert!(!key.path(&dir.0).exists());
            let temps = fs::read_dir(&dir.0)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|p| temp_file(p))
                .collect::<Vec<_>>();
            assert_eq!(temps.len(), 1);
            assert_eq!(
                fs::metadata(&temps[0]).unwrap().len(),
                if denied_check == 3 {
                    0
                } else {
                    HEADER_BYTES as u64
                }
            );
        }
    }
}

#[cfg(test)]
mod dl8_tests {
    use super::*;
    #[test]
    fn dl8_art_floor_boundary() {
        let floor = ART_RESERVE_BYTES;
        let cost = WRITE_MARGIN + 4096;
        assert!(!admitted(floor - 1, 1, 4096));
        assert!(!admitted(floor, 1, 4096));
        assert!(!admitted(floor + cost - 1, 1, 4096));
        assert!(admitted(floor + cost, 1, 4096));
    }
}

#[cfg(test)]
mod inventory_tests {
    use super::*;

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn multi_eviction_uses_one_inventory_and_preserves_unrelated_files() {
        let root = std::env::temp_dir().join(format!("allium-art-inventory-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let _writer = CacheWriter::acquire(&root).unwrap();
        for i in 0..5 {
            File::create(root.join(format!("{i:064x}.rgba"))).unwrap().set_len(DISK_BYTES / 4).unwrap();
        }
        fs::write(root.join("keep.txt"), b"not cache").unwrap();
        INVENTORY_PASSES.with(|n| n.set(0));
        assert!(prepare_write(&root, DISK_BYTES / 8, || Some((u64::MAX, 4096))).unwrap());
        INVENTORY_PASSES.with(|n| assert_eq!(n.get(), 1));
        let remaining: Vec<_> = fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).filter(|p| cache_file(p)).collect();
        assert_eq!(remaining.len(), 3);
        assert!(root.join("keep.txt").exists());
        assert_eq!(remaining.iter().map(|p| fs::metadata(p).unwrap().len()).sum::<u64>(), 3 * DISK_BYTES / 4);
        drop(_writer);
        fs::remove_dir_all(root).unwrap();
    }
}
