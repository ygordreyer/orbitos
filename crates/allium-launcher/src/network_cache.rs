//! Private cache jobs, identity-bound checkpoints and no-replace installs.
#![allow(dead_code)]
use crate::network_fs::{AnchoredFile, AnchoredFs, Identity};
use crate::network_library::{
    CacheState, Catalog, METADATA_MARGIN, NetworkGame, PartialAllocation, RESERVE_BYTES, plan_space,
};
#[cfg(test)]
use crate::network_transfer::hash_reader;
use crate::network_transfer::{
    Cancellation, Endpoint, WINDOW, digest_hex, download, hash_reader_progress,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{fs::MetadataExt, io::AsRawFd},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
};

static WORKER_ACTIVE: AtomicBool = AtomicBool::new(false);
#[derive(Clone, Copy, Debug)]
pub struct Space {
    pub free_bytes: u64,
    pub allocation_unit: u64,
}
pub trait SpaceProbe: Send + Sync {
    fn observe(&self, fs: &AnchoredFs) -> Result<Space>;
}
pub struct FilesystemSpace;
impl SpaceProbe for FilesystemSpace {
    fn observe(&self, fs: &AnchoredFs) -> Result<Space> {
        fs.check_root()?;
        let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        ensure!(
            unsafe { libc::fstatvfs(fs.root_fd(), info.as_mut_ptr()) } == 0,
            "statvfs failed: {}",
            std::io::Error::last_os_error()
        );
        let info = unsafe { info.assume_init() };
        let unit = info.f_frsize as u64;
        ensure!(unit > 0, "Zero allocation unit");
        Ok(Space {
            free_bytes: (info.f_bavail as u64)
                .checked_mul(unit)
                .ok_or_else(|| anyhow::anyhow!("statvfs overflow"))?,
            allocation_unit: unit,
        })
    }
}
fn directories(root: &AnchoredFs, path: &Path) -> Result<()> {
    root.mkdirs(path)
}
fn regular(root: &AnchoredFs, path: &Path, writable: bool) -> Result<AnchoredFile> {
    root.open(path, writable)
}
fn sync_directory(root: &AnchoredFs, path: &Path) -> Result<()> {
    root.sync_directory(path)
}
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Partial {
    id: String,
    bytes: u64,
    prefix_sha256: String,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct RetryIntent {
    file_id: String,
    failed_bytes: u64,
    failed_sha256: String,
    device: u64,
    inode: u64,
}
#[derive(Serialize, Deserialize, Clone)]
struct Journal {
    version: u32,
    game: NetworkGame,
    phase: String,
    partials: Vec<Partial>,
    #[serde(default)]
    retry_file: Option<RetryIntent>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyRecord {
    version: u32,
    game: NetworkGame,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<Vec<PayloadMetadata>>,
}
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
struct PayloadMetadata {
    id: String,
    bytes: u64,
    mtime: i64,
    mtime_nsec: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sampled_sha256: Option<String>,
}
impl PayloadMetadata {
    fn stat(id: &str, file: &AnchoredFile) -> Result<Self> {
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "Expected regular payload");
        file.validate()?;
        Ok(Self {
            id: id.into(),
            bytes: metadata.len(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            sampled_sha256: None,
        })
    }
    fn read(id: &str, file: &AnchoredFile, cancel: &Cancellation) -> Result<Self> {
        Self::read_reported(id, file, cancel, None)
    }
    fn read_reported(
        id: &str,
        file: &AnchoredFile,
        cancel: &Cancellation,
        progress: Option<&mut crate::network_transfer::VerificationProgress<'_>>,
    ) -> Result<Self> {
        let mut before = Self::stat(id, file)?;
        let fingerprint = sampled_fingerprint_reported(file, before.bytes, cancel, progress)?;
        ensure!(
            Self::stat(id, file)? == before,
            "Payload changed during sampling"
        );
        before.sampled_sha256 = Some(fingerprint);
        Ok(before)
    }
    fn matches_stat(&self, current: &Self) -> bool {
        self.id == current.id
            && self.bytes == current.bytes
            && self.mtime == current.mtime
            && self.mtime_nsec == current.mtime_nsec
    }
}
const SAMPLE_BLOCK: usize = 4096;
const SAMPLE_EDGE_BYTES: u64 = 64 * 1024;
const SAMPLE_INTERIOR_BLOCKS: u64 = 16;
const SAMPLE_TOTAL: u64 = 2 * SAMPLE_EDGE_BYTES + SAMPLE_INTERIOR_BLOCKS * SAMPLE_BLOCK as u64;

fn hash_sample_region(
    file: &AnchoredFile,
    mut offset: u64,
    mut bytes: u64,
    hash: &mut Sha256,
    cancel: &Cancellation,
    progress: &mut Option<&mut crate::network_transfer::VerificationProgress<'_>>,
) -> Result<()> {
    let mut buffer = [0; SAMPLE_BLOCK];
    while bytes > 0 {
        cancel.check()?;
        file.validate()?;
        let limit = bytes.min(buffer.len() as u64) as usize;
        #[cfg(not(target_os = "linux"))]
        let read = unsafe {
            libc::pread(
                file.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                limit,
                offset as libc::off_t,
            )
        };
        #[cfg(target_os = "linux")]
        let read = unsafe {
            libc::pread64(
                file.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                limit,
                offset as libc::off64_t,
            )
        };
        if read < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        ensure!(read > 0, "Payload shortened during sampling");
        hash.update(&buffer[..read as usize]);
        offset += read as u64;
        bytes -= read as u64;
        if let Some(progress) = progress.as_deref_mut() {
            progress.advance(read as u64);
        }
    }
    file.validate()
}
fn sampled_fingerprint(file: &AnchoredFile, bytes: u64, cancel: &Cancellation) -> Result<String> {
    sampled_fingerprint_reported(file, bytes, cancel, None)
}
fn sampled_fingerprint_reported(
    file: &AnchoredFile,
    bytes: u64,
    cancel: &Cancellation,
    mut progress: Option<&mut crate::network_transfer::VerificationProgress<'_>>,
) -> Result<String> {
    ensure!(
        bytes <= i64::MAX as u64,
        "Payload exceeds sampling offset range"
    );
    let mut hash = Sha256::new();
    for (offset, size) in sample_regions(bytes) {
        hash_sample_region(file, offset, size, &mut hash, cancel, &mut progress)?;
    }
    Ok(digest_hex(hash.finalize().as_slice()))
}
fn sample_regions(bytes: u64) -> Vec<(u64, u64)> {
    if bytes <= SAMPLE_TOTAL {
        return vec![(0, bytes)];
    }
    let mut regions = vec![
        (0, SAMPLE_EDGE_BYTES),
        (bytes - SAMPLE_EDGE_BYTES, SAMPLE_EDGE_BYTES),
    ];
    let interior = bytes - 2 * SAMPLE_EDGE_BYTES - SAMPLE_BLOCK as u64;
    for i in 1..=SAMPLE_INTERIOR_BLOCKS {
        regions.push((
            SAMPLE_EDGE_BYTES
                + (interior as u128 * i as u128 / (SAMPLE_INTERIOR_BLOCKS + 1) as u128) as u64,
            SAMPLE_BLOCK as u64,
        ));
    }
    regions
}
struct StreamSamples(Vec<(u64, Vec<u8>)>);
impl StreamSamples {
    fn new(bytes: u64) -> Self {
        Self(
            sample_regions(bytes)
                .into_iter()
                .map(|(offset, size)| (offset, vec![0; size as usize]))
                .collect(),
        )
    }
    fn capture(&mut self, offset: u64, bytes: &[u8]) {
        for (start, sample) in &mut self.0 {
            let from = offset.max(*start);
            let to = (offset + bytes.len() as u64).min(*start + sample.len() as u64);
            if from < to {
                sample[(from - *start) as usize..(to - *start) as usize]
                    .copy_from_slice(&bytes[(from - offset) as usize..(to - offset) as usize]);
            }
        }
    }
    fn fingerprint(&self) -> String {
        let mut hash = Sha256::new();
        for (_, sample) in &self.0 {
            hash.update(sample);
        }
        digest_hex(hash.finalize().as_slice())
    }
}
struct VerifiedFile {
    file: AnchoredFile,
    metadata: PayloadMetadata,
}
impl std::ops::Deref for VerifiedFile {
    type Target = AnchoredFile;
    fn deref(&self) -> &AnchoredFile {
        &self.file
    }
}
#[derive(Debug, PartialEq)]
pub enum InstallState {
    Complete,
    Missing,
    Conflicting,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnloadLeaf {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl UnloadLeaf {
    fn identity(&self) -> Identity {
        Identity {
            device: self.device,
            inode: self.inode,
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnloadRecord {
    version: u32,
    game: NetworkGame,
    unload_files: Vec<UnloadLeaf>,
    unload_dirs: Vec<UnloadLeaf>,
}
fn validate_unload(record: &UnloadRecord, game: &NetworkGame) -> Result<()> {
    ensure!(
        record.version == 1 && same_game(&record.game, game),
        "Unknown unload identity"
    );
    let expected = game.targets()?;
    ensure!(
        record
            .unload_files
            .iter()
            .map(|f| &f.path)
            .eq(expected.iter()),
        "Unknown unload files"
    );
    ensure!(
        record
            .unload_dirs
            .iter()
            .map(|d| d.path.clone())
            .collect::<Vec<_>>()
            == unload_dirs(game)?,
        "Unknown unload directories"
    );
    Ok(())
}
fn unload_dirs(game: &NetworkGame) -> Result<Vec<PathBuf>> {
    if game.install_kind != "directory" {
        return Ok(Vec::new());
    }
    let mut dirs = game
        .targets()?
        .iter()
        .flat_map(|p| {
            p.ancestors()
                .skip(1)
                .take_while(|p| p.starts_with(&game.cache_path))
                .map(Path::to_path_buf)
        })
        .collect::<Vec<_>>();
    dirs.sort();
    dirs.dedup();
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    Ok(dirs)
}
fn same_game(a: &NetworkGame, b: &NetworkGame) -> bool {
    serde_json::to_vec(a).ok() == serde_json::to_vec(b).ok()
}
fn load_json<T: for<'a> Deserialize<'a>>(root: &AnchoredFs, path: &Path) -> Result<T> {
    let mut file = regular(root, path, false)?;
    read_json(&mut file)
}
fn read_json<T: for<'a> Deserialize<'a>>(file: &mut AnchoredFile) -> Result<T> {
    ensure!(file.metadata()?.len() <= 1024 * 1024, "Metadata too large");
    let mut reader = std::io::BufReader::with_capacity(WINDOW, file);
    let value = serde_json::from_reader(&mut reader);
    reader.get_ref().validate()?;
    Ok(value?)
}
fn validate_metadata_value(value: &serde_json::Value) -> Result<()> {
    let game: NetworkGame = serde_json::from_value(value["game"].clone())?;
    let canonical = if value.get("unload_files").is_some() {
        let record: UnloadRecord = serde_json::from_value(value.clone())?;
        validate_unload(&record, &game)?;
        serde_json::to_value(record)?
    } else if value.get("phase").is_some() {
        let journal: Journal = serde_json::from_value(value.clone())?;
        validate_journal(&journal, &game)?;
        let mut result = serde_json::to_value(journal)?;
        if value.get("retry_file").is_none() {
            result.as_object_mut().unwrap().remove("retry_file");
        }
        result
    } else {
        let record: ReadyRecord = serde_json::from_value(value.clone())?;
        ensure!(record.version == 1, "Unknown Ready version");
        serde_json::to_value(record)?
    };
    ensure!(canonical == *value, "Unknown generated metadata fields");
    Ok(())
}
fn owned_metadata_identity(
    root: &AnchoredFs,
    path: &Path,
    next: &serde_json::Value,
) -> Result<Option<Identity>> {
    validate_metadata_value(next)?;
    if !root.exists(path)? {
        return Ok(None);
    }
    let mut file = regular(root, path, false)?;
    ensure!(file.metadata()?.len() <= 1024 * 1024, "Metadata too large");
    let previous: serde_json::Value =
        serde_json::from_reader(std::io::BufReader::with_capacity(WINDOW, &mut file))?;
    ensure!(
        previous.get("version") == next.get("version") && previous.get("game") == next.get("game"),
        "Unknown metadata identity"
    );
    validate_metadata_value(&previous)?;
    file.validate()?;
    Ok(Some(file.identity()))
}
fn save_json(root: &AnchoredFs, path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("new");
    let value = serde_json::to_value(value)?;
    let expected = owned_metadata_identity(root, path, &value)?;
    let bytes = serde_json::to_vec(&value)?;
    ensure!(bytes.len() <= 1024 * 1024, "Metadata too large");
    let mut file = root.create(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    root.replace_metadata(&temporary, path, expected)
}
fn promote_metadata(root: &AnchoredFs, source: &Path, target: &Path) -> Result<()> {
    let mut file = regular(root, source, false)?;
    let next: serde_json::Value = read_json(&mut file)?;
    validate_metadata_value(&next)?;
    let expected = owned_metadata_identity(root, target, &next)?;
    file.sync_all()?;
    root.replace_metadata_file(&file, target, expected)
}
struct CacheLock {
    _file: AnchoredFile,
}
impl CacheLock {
    fn acquire(root: &AnchoredFs, path: &Path) -> Result<Self> {
        let file = match root.create(path) {
            Ok(mut file) => {
                file.write_all(b"allium-network-cache\n")?;
                file.sync_all()?;
                file
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                let mut file = regular(root, path, true)?;
                let mut contents = String::new();
                Read::by_ref(&mut file)
                    .take(64)
                    .read_to_string(&mut contents)?;
                ensure!(
                    contents == "allium-network-cache\n",
                    "Unregistered cache lock"
                );
                file
            }
            Err(error) => return Err(error.into()),
        };
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "Cache job already active"
        );
        Ok(Self { _file: file })
    }
}
struct ExclusiveFile {
    file: AnchoredFile,
}
impl ExclusiveFile {
    fn acquire(path: &Path, optional_existing: bool, reserve_device: u64) -> Result<Option<Self>> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Lease parent missing"))?;
        let fs = AnchoredFs::new_lease(parent, reserve_device)?;
        match fs.create(path) {
            Ok(mut file) => {
                file.write_all(b"allium-network-cache\n")?;
                file.sync_all()?;
                Ok(Some(Self { file }))
            }
            Err(error)
                if optional_existing
                    && error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}
impl Drop for ExclusiveFile {
    fn drop(&mut self) {
        let _ = self.file.remove_owned();
    }
}
#[derive(Clone)]
pub struct CacheConfig {
    pub root: PathBuf,
    pub stay_awake: Option<PathBuf>,
    pub expected_root: Identity,
    binding: AnchoredFs,
}
impl CacheConfig {
    pub fn bind(root: PathBuf, stay_awake: Option<PathBuf>) -> Result<Self> {
        let fs = AnchoredFs::new(&root)?;
        fs.pin_existing(&root.join("Roms/PS"))?;
        fs.pin_existing(&root.join(".allium/state/network-library"))?;
        Ok(Self {
            root,
            stay_awake,
            expected_root: fs.root_identity()?,
            binding: fs,
        })
    }
}
pub struct Cache {
    config: CacheConfig,
    fs: AnchoredFs,
    #[cfg(test)]
    commit_hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    verification_hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
#[derive(Debug, Clone)]
pub struct Progress {
    pub game_id: String,
    pub phase: &'static str,
    pub bytes_done: u64,
    pub bytes_total: u64,
}
impl Cache {
    pub fn new(config: CacheConfig) -> Result<Self> {
        let fs = config.binding.clone();
        ensure!(
            config.root == fs.root_path() && fs.root_identity()? == config.expected_root,
            "Configured root binding changed"
        );
        fs.check_bound_directories()?;
        Ok(Self {
            config,
            fs,
            #[cfg(test)]
            commit_hook: std::sync::Mutex::new(None),
            #[cfg(test)]
            verification_hook: std::sync::Mutex::new(None),
        })
    }
    #[cfg(test)]
    pub fn test_before_next_fs_operation(&self, hook: impl FnOnce() + Send + 'static) {
        self.fs.test_before_next_operation(hook);
    }
    #[cfg(test)]
    pub fn test_after_stage_verification(&self, hook: impl FnOnce() + Send + 'static) {
        *self.commit_hook.lock().unwrap() = Some(Box::new(hook));
    }
    fn state(&self) -> PathBuf {
        self.config.root.join(".allium/state/network-library")
    }
    fn job(&self, game: &NetworkGame) -> PathBuf {
        self.state()
            .join("jobs")
            .join(&game.id)
            .join(&game.bundle_revision)
    }
    fn index(&self, game: &NetworkGame) -> PathBuf {
        self.state()
            .join("installed")
            .join(format!("{}.json", game.id))
    }
    fn prepare(&self, game: &NetworkGame) -> Result<()> {
        let catalog = Catalog {
            version: 1,
            library_id: "cache".into(),
            revision: "0".repeat(64),
            games: vec![game.clone()],
        };
        catalog.validate()?;
        directories(&self.fs, &self.state().join("installed"))?;
        directories(&self.fs, &self.job(game))
    }
    fn prepare_adoption(&self, game: &NetworkGame) -> Result<()> {
        let catalog = Catalog {
            version: 1,
            library_id: "cache".into(),
            revision: "0".repeat(64),
            games: vec![game.clone()],
        };
        catalog.validate()?;
        directories(&self.fs, &self.state().join("installed"))
    }
    fn ensure_no_managed_partial(&self, game: &NetworkGame) -> Result<()> {
        let jobs = self.state().join("jobs").join(&game.id);
        if !self.fs.exists(&jobs)? {
            return Ok(());
        }
        for (revision, directory) in self.fs.entries(&jobs)? {
            ensure!(
                directory
                    && revision
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name == game.bundle_revision),
                "Unknown managed job revision blocks adoption"
            );
            ensure!(
                self.fs.entries(&revision)?.is_empty(),
                "Managed partial blocks adoption"
            );
        }
        Ok(())
    }
    fn payload(&self, game: &NetworkGame, index: usize) -> PathBuf {
        let name = if game.install_kind == "directory" {
            PathBuf::from(&game.files[index].relative_path)
        } else {
            PathBuf::from("content.bin")
        };
        self.job(game).join("payloads").join(name)
    }
    fn journal(&self, game: &NetworkGame) -> Result<Journal> {
        let path = self.job(game).join("job.json");
        if self.fs.exists(&path)? {
            let journal: Journal = load_json(&self.fs, &path)?;
            ensure!(
                journal.version == 1
                    && same_game(&journal.game, game)
                    && ["downloading", "installing", "ready"].contains(&journal.phase.as_str()),
                "Stale job identity"
            );
            Ok(journal)
        } else {
            let journal = Journal {
                version: 1,
                game: game.clone(),
                phase: "downloading".into(),
                partials: Vec::new(),
                retry_file: None,
            };
            save_json(&self.fs, &path, &journal)?;
            Ok(journal)
        }
    }
    fn save_journal(&self, journal: &Journal) -> Result<()> {
        save_json(&self.fs, &self.job(&journal.game).join("job.json"), journal)
    }
    fn recover_partial(
        &self,
        game: &NetworkGame,
        index: usize,
        journal: &Journal,
        cancel: &Cancellation,
    ) -> Result<(AnchoredFile, Sha256, u64, StreamSamples)> {
        let path = self.payload(game, index);
        directories(&self.fs, path.parent().unwrap())?;
        let checkpoint = journal
            .partials
            .iter()
            .find(|p| p.id == game.files[index].id);
        let retry = journal
            .retry_file
            .as_ref()
            .filter(|r| r.file_id == game.files[index].id);
        let mut file = if self.fs.exists(&path)? {
            ensure!(checkpoint.is_some(), "Unregistered staging file");
            regular(&self.fs, &path, true)?
        } else {
            ensure!(retry.is_none(), "Missing retry payload must be preserved");
            ensure!(
                checkpoint.is_none_or(|p| p.bytes == 0),
                "Missing checkpoint payload"
            );
            self.fs.create(&path)?
        };
        if let Some(retry) = retry {
            verify_retry_payload(&mut file, retry, cancel)?;
        }
        let bytes = checkpoint.map_or(0, |p| p.bytes);
        ensure!(
            bytes <= game.files[index].bytes
                && file.metadata()?.len() >= bytes
                && file.metadata()?.len() <= game.files[index].bytes,
            "Checkpoint length mismatch"
        );
        let mut hash = Sha256::new();
        let mut buffer = [0; WINDOW];
        let mut samples = StreamSamples::new(game.files[index].bytes);
        let mut progress = (bytes > 0).then(|| cancel.verification(bytes));
        let mut left = bytes;
        while left > 0 {
            cancel.check_verification_read()?;
            let limit = left.min(WINDOW as u64) as usize;
            file.read_exact(&mut buffer[..limit])?;
            hash.update(&buffer[..limit]);
            samples.capture(bytes - left, &buffer[..limit]);
            if let Some(progress) = &mut progress {
                progress.advance(limit as u64);
            }
            left -= limit as u64;
        }
        if let Some(checkpoint) = checkpoint {
            ensure!(
                digest_hex(hash.clone().finalize().as_slice()) == checkpoint.prefix_sha256,
                "Checkpoint SHA mismatch"
            );
        }
        cancel.check()?;
        file.set_len(bytes)?;
        file.seek(SeekFrom::Start(bytes))?;
        if let Some(progress) = &mut progress {
            progress.complete();
        }
        Ok((file, hash, bytes, samples))
    }
    fn verify_files(
        &self,
        game: &NetworkGame,
        installed: bool,
        cancel: &Cancellation,
    ) -> Result<Vec<VerifiedFile>> {
        #[cfg(test)]
        if let Some(hook) = self.verification_hook.lock().unwrap().take() {
            hook();
        }
        let mut handles = Vec::new();
        let targets = game.targets()?;
        let expected = targets
            .iter()
            .map(|p| self.config.root.join(p))
            .collect::<HashSet<_>>();
        let base = if installed {
            self.config.root.join(&game.cache_path)
        } else {
            self.job(game).join("payloads")
        };
        for (index, payload) in game.files.iter().enumerate() {
            let path = if installed {
                self.config.root.join(&targets[index])
            } else {
                self.payload(game, index)
            };
            let mut file = regular(&self.fs, &path, false)?;
            let before = PayloadMetadata::read(&payload.id, &file, cancel)?;
            ensure!(before.bytes == payload.bytes, "Unexpected payload length");
            let (size, hash, mut verification) = hash_reader_progress(
                &mut Read::by_ref(&mut file).take(payload.bytes + 1),
                cancel,
                payload.bytes,
            )?;
            file.validate()?;
            ensure!(
                size == payload.bytes && hash == payload.sha256,
                "Full payload SHA/length mismatch"
            );
            ensure!(
                PayloadMetadata::read(&payload.id, &file, cancel)? == before,
                "Payload changed during verification"
            );
            verification.complete();
            handles.push(VerifiedFile {
                file,
                metadata: before,
            });
        }
        if game.install_kind == "directory" {
            let paths = if installed {
                expected
            } else {
                (0..game.files.len())
                    .map(|i| self.payload(game, i))
                    .collect()
            };
            verify_tree(&self.fs, &base, &paths, cancel)?;
        }
        Ok(handles)
    }
    fn ready_state(&self, game: &NetworkGame, cancel: &Cancellation) -> Result<CacheState> {
        self.fs.check_bound_directories()?;
        cancel.check()?;
        Ok(CacheState::Ready {
            game_id: game.id.clone(),
            bundle_revision: game.bundle_revision.clone(),
            verified_files: game.files.len(),
        })
    }
    fn cached_ready(
        &self,
        game: &NetworkGame,
        cancel: &Cancellation,
    ) -> Result<Option<CacheState>> {
        cancel.check()?;
        ensure!(
            !self.unloading(game)?,
            "Interrupted unload; finish through Manage storage"
        );
        let record: ReadyRecord = load_json(&self.fs, &self.index(game))?;
        ensure!(
            record.version == 1 && same_game(&record.game, game),
            "Stale Ready identity"
        );
        if self.fs.exists(&self.index(game).with_extension("new"))?
            || self.fs.exists(&self.job(game).join("job.new"))?
        {
            return Ok(None);
        }
        let Some(metadata) = record.metadata else {
            return Ok(None);
        };
        if metadata.len() != game.files.len() {
            return Ok(None);
        }
        for ((path, payload), expected) in game.targets()?.iter().zip(&game.files).zip(&metadata) {
            cancel.check()?;
            let file = regular(&self.fs, &self.config.root.join(path), false)?;
            let current = PayloadMetadata::stat(&payload.id, &file)?;
            if current.bytes != payload.bytes
                || !expected.matches_stat(&current)
                || expected.sampled_sha256.is_none()
            {
                return Ok(None);
            }
            if PayloadMetadata::read(&payload.id, &file, cancel)? != *expected {
                return Ok(None);
            }
        }
        self.ready_state(game, cancel).map(Some)
    }
    fn verify_ready(
        &self,
        game: &NetworkGame,
        cancel: &Cancellation,
        lock: &CacheLock,
    ) -> Result<CacheState> {
        ensure!(
            !self.unloading(game)?,
            "Interrupted unload; finish through Manage storage"
        );
        ensure!(self.has_index(game)?, "Missing Ready identity");
        let handles = self
            .verify_files(game, true, cancel)
            .context("Conflicting or changed installed member; existing files preserved")?;
        cancel.check()?;
        self.publish_ready(game, &handles, lock)?;
        self.ready_state(game, cancel)
    }
    pub fn ready(&self, game: &NetworkGame, cancel: &Cancellation) -> Result<CacheState> {
        if let Some(state) = self.cached_ready(game, cancel)? {
            return Ok(state);
        }
        let lock = CacheLock::acquire(&self.fs, &self.state().join("active.lock"))?;
        let _awake = self.awake_lease()?;
        self.recover_metadata(game, cancel, &lock)?;
        self.verify_ready(game, cancel, &lock)
    }
    fn install(
        &self,
        journal: &mut Journal,
        cancel: &Cancellation,
        space: &dyn SpaceProbe,
        lock: &CacheLock,
    ) -> Result<()> {
        let game = journal.game.clone();
        let target = self.config.root.join(&game.cache_path);
        if ["installing", "ready"].contains(&journal.phase.as_str()) && self.fs.exists(&target)? {
            ensure!(
                self.committed_target(&game)?,
                "Ambiguous interrupted install: staging and target both exist"
            );
            self.verify_files(&game, true, cancel)?;
        } else {
            ensure!(
                !self.fs.exists(&target)?,
                "Existing ROM target requires explicit adoption"
            );
            let source = if game.install_kind == "directory" {
                self.job(&game).join("payloads")
            } else {
                self.payload(&game, 0)
            };
            let verified_source = self.fs.pin_source(&source)?;
            let verified_files = self.verify_files(&game, false, cancel)?;
            #[cfg(test)]
            {
                let hook = self.commit_hook.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook();
                }
            }

            cancel.check()?;
            self.check_reserve(space)?;
            journal.phase = "installing".into();
            self.save_journal(journal)?;
            directories(&self.fs, target.parent().unwrap())?;
            self.fs.check(&target)?;
            sync_tree_directories(&self.fs, &source, cancel)?;
            self.check_reserve(space)?;
            for file in &verified_files {
                file.validate()?;
            }
            self.fs.rename_verified(&verified_source, &target)?;
            sync_directory(&self.fs, source.parent().unwrap())?;
            sync_directory(&self.fs, target.parent().unwrap())?;
        }
        let handles = self.verify_files(&game, true, cancel)?;
        self.check_reserve(space)?;
        self.publish_ready(&game, &handles, lock)?;
        journal.phase = "ready".into();
        self.save_journal(journal)
    }
    fn installed_streamed(
        &self,
        game: &NetworkGame,
        handles: Vec<VerifiedFile>,
        cancel: &Cancellation,
    ) -> Result<Vec<VerifiedFile>> {
        game.targets()?
            .iter()
            .zip(handles)
            .map(|(path, verified)| {
                let file = regular(&self.fs, &self.config.root.join(path), false)?;
                ensure!(
                    file.identity() == verified.identity()
                        && PayloadMetadata::read(&verified.metadata.id, &file, cancel)?
                            == verified.metadata,
                    "Streamed payload changed during install"
                );
                Ok(VerifiedFile {
                    file,
                    metadata: verified.metadata,
                })
            })
            .collect()
    }
    fn install_streamed(
        &self,
        journal: &mut Journal,
        handles: Vec<VerifiedFile>,
        cancel: &Cancellation,
        space: &dyn SpaceProbe,
        lock: &CacheLock,
    ) -> Result<()> {
        let game = &journal.game;
        let target = self.config.root.join(&game.cache_path);
        ensure!(
            !self.fs.exists(&target)?,
            "Existing ROM target requires explicit adoption"
        );
        let source = if game.install_kind == "directory" {
            self.job(game).join("payloads")
        } else {
            self.payload(game, 0)
        };
        if game.install_kind == "directory" {
            verify_tree(
                &self.fs,
                &source,
                &(0..game.files.len())
                    .map(|i| self.payload(game, i))
                    .collect(),
                cancel,
            )?;
        }
        let pinned = self.fs.pin_source(&source)?;
        #[cfg(test)]
        if let Some(hook) = self.commit_hook.lock().unwrap().take() {
            hook();
        }
        cancel.check()?;
        self.check_reserve(space)?;
        for handle in &handles {
            ensure!(
                PayloadMetadata::read(&handle.metadata.id, handle, cancel)? == handle.metadata,
                "Streamed payload changed before install"
            );
        }
        journal.phase = "installing".into();
        self.save_journal(journal)?;
        directories(&self.fs, target.parent().unwrap())?;
        sync_tree_directories(&self.fs, &source, cancel)?;
        self.check_reserve(space)?;
        self.fs.rename_verified(&pinned, &target)?;
        sync_directory(&self.fs, source.parent().unwrap())?;
        sync_directory(&self.fs, target.parent().unwrap())?;
        let handles = self.installed_streamed(&journal.game, handles, cancel)?;
        self.check_reserve(space)?;
        self.publish_ready(&journal.game, &handles, lock)?;
        journal.phase = "ready".into();
        self.save_journal(journal)
    }
    fn check_capacity(
        &self,
        game: &NetworkGame,
        journal: &Journal,
        space: &dyn SpaceProbe,
        cancel: &Cancellation,
    ) -> Result<()> {
        let observation = space.observe(&self.fs)?;
        let mut allocations = Vec::new();
        for index in 0..game.files.len() {
            let target = self.config.root.join(&game.targets()?[index]);
            if self.fs.exists(&target)? {
                let _verified = self.verify_member(game, index, &target, cancel)?;
                let unit = observation.allocation_unit;
                let allocated = (game.files[index].bytes + unit - 1) / unit * unit;
                allocations.push(PartialAllocation {
                    game_id: game.id.clone(),
                    bundle_revision: game.bundle_revision.clone(),
                    file_id: game.files[index].id.clone(),
                    sha256: game.files[index].sha256.clone(),
                    allocated_bytes: allocated,
                });
                continue;
            }
            if journal
                .partials
                .iter()
                .any(|p| p.id == game.files[index].id)
            {
                let file = regular(&self.fs, &self.payload(game, index), false)?;
                let checkpoint = journal
                    .partials
                    .iter()
                    .find(|p| p.id == game.files[index].id)
                    .unwrap();
                ensure!(
                    checkpoint.bytes <= game.files[index].bytes
                        && file.metadata()?.len() >= checkpoint.bytes
                        && file.metadata()?.len() <= game.files[index].bytes,
                    "Checkpoint length mismatch"
                );
                let allocated = file
                    .metadata()?
                    .blocks()
                    .checked_mul(512)
                    .ok_or_else(|| anyhow::anyhow!("Allocation overflow"))?;
                allocations.push(PartialAllocation {
                    game_id: game.id.clone(),
                    bundle_revision: game.bundle_revision.clone(),
                    file_id: game.files[index].id.clone(),
                    sha256: game.files[index].sha256.clone(),
                    allocated_bytes: allocated / observation.allocation_unit
                        * observation.allocation_unit,
                });
            }
        }
        ensure!(
            plan_space(
                game,
                observation.allocation_unit,
                &allocations,
                observation.free_bytes
            )?
            .shortage_bytes
                == 0,
            "Insufficient space"
        );
        Ok(())
    }
    fn check_reserve(&self, space: &dyn SpaceProbe) -> Result<()> {
        ensure!(
            space.observe(&self.fs)?.free_bytes >= RESERVE_BYTES + METADATA_MARGIN,
            "Reserve lost before install"
        );
        Ok(())
    }
    fn transfer_file(
        &self,
        game: &NetworkGame,
        index: usize,
        journal: &mut Journal,
        endpoint: &Endpoint,
        cancel: &Cancellation,
        space: &dyn SpaceProbe,
        progress: &SyncSender<Progress>,
    ) -> Result<VerifiedFile> {
        cancel.check()?;
        let (file, hash, bytes, samples) = self.recover_partial(game, index, journal, cancel)?;
        let mut writer = CheckpointWriter {
            cache: self,
            journal,
            index,
            file,
            hash,
            bytes,
            last_checkpoint: bytes,
            samples,
            progress,
            cancel,
            verification: None,
        };
        writer.checkpoint()?;
        if bytes < game.files[index].bytes {
            let result = download(
                endpoint,
                &game.files[index],
                bytes,
                &mut writer,
                cancel,
                |incoming| {
                    let observed = space.observe(&self.fs)?;
                    admit_transfer(observed, incoming)
                },
            );
            writer.checkpoint()?;
            result?;
        }
        writer.finish(cancel)
    }
    fn validate_checkpoint_prefixes(&self, journal: &Journal, cancel: &Cancellation) -> Result<()> {
        for checkpoint in &journal.partials {
            cancel.check()?;
            let index = journal
                .game
                .files
                .iter()
                .position(|p| p.id == checkpoint.id)
                .unwrap();
            let path = self.payload(&journal.game, index);
            if !self.fs.exists(&path)? {
                let target = self.config.root.join(&journal.game.targets()?[index]);
                if self.fs.exists(&target)? && journal.phase == "installing" {
                    self.verify_member(&journal.game, index, &target, cancel)?;
                    continue;
                }
                if checkpoint.bytes == 0 {
                    continue;
                }
            }
            let mut file = regular(&self.fs, &path, false)?;
            if let Some(retry) = journal
                .retry_file
                .as_ref()
                .filter(|r| r.file_id == checkpoint.id)
            {
                verify_retry_payload(&mut file, retry, cancel)?;
            }
            ensure!(
                file.metadata()?.len() >= checkpoint.bytes
                    && file.metadata()?.len() <= journal.game.files[index].bytes,
                "Scratch checkpoint payload too short"
            );
            let (bytes, hash, mut verification) = hash_reader_progress(
                &mut Read::by_ref(&mut file).take(checkpoint.bytes),
                cancel,
                checkpoint.bytes,
            )?;
            file.validate()?;
            ensure!(
                bytes == checkpoint.bytes && hash == checkpoint.prefix_sha256,
                "Scratch checkpoint prefix mismatch"
            );
            verification.complete();
        }
        Ok(())
    }
    fn committed_target(&self, game: &NetworkGame) -> Result<bool> {
        if !self.fs.exists(&self.job(game).join("payloads"))? {
            return Ok(true);
        }
        for index in 0..game.files.len() {
            if self.fs.exists(&self.payload(game, index))? {
                return Ok(false);
            }
        }
        if game.install_kind == "directory" {
            let expected = (0..game.files.len())
                .map(|i| self.payload(game, i))
                .collect();
            verify_tree(
                &self.fs,
                &self.job(game).join("payloads"),
                &expected,
                &Cancellation::default(),
            )?;
        }
        Ok(true)
    }
    fn recover_job_scratch(&self, game: &NetworkGame, cancel: &Cancellation) -> Result<()> {
        let path = self.job(game).join("job.json");
        let scratch = path.with_extension("new");
        if !self.fs.exists(&scratch)? {
            return Ok(());
        }
        cancel.check()?;
        let candidate: Journal = load_json(&self.fs, &scratch)?;
        validate_journal(&candidate, game)?;
        if self.fs.exists(&path)? {
            let committed: Journal = load_json(&self.fs, &path)?;
            validate_journal(&committed, game)?;
            validate_progression(&committed, &candidate)?;
            if candidate.retry_file.is_some() {
                self.validate_checkpoint_prefixes(&committed, cancel)?;
            }
        } else {
            ensure!(candidate.retry_file.is_none(), "Unproven retry intent");
        }
        if candidate.phase == "downloading" {
            ensure!(
                self.install_state(game)? != InstallState::Conflicting,
                "Conflicting recovery target"
            );
            self.validate_checkpoint_prefixes(&candidate, cancel)?;
        } else if self.fs.exists(&self.config.root.join(&game.cache_path))? {
            if self.committed_target(game)? {
                self.verify_files(game, true, cancel)?;
            } else {
                ensure!(candidate.phase == "installing", "Ambiguous scratch install");
                let targets = game.targets()?;
                for (index, target) in targets.iter().enumerate() {
                    let target = self.config.root.join(target);
                    let staged = self.payload(game, index);
                    let installed = self.fs.exists(&target)?;
                    ensure!(
                        !(installed && self.fs.exists(&staged)?),
                        "Ambiguous staged and installed member"
                    );
                    self.verify_member(
                        game,
                        index,
                        if installed { &target } else { &staged },
                        cancel,
                    )?;
                }
            }
        } else {
            ensure!(candidate.phase == "installing", "Missing Ready target");
            self.verify_files(game, false, cancel)?;
        }
        cancel.check()?;
        promote_metadata(&self.fs, &scratch, &path)
    }
    fn recover_index_scratch(
        &self,
        game: &NetworkGame,
        cancel: &Cancellation,
        lock: &CacheLock,
    ) -> Result<()> {
        let path = self.index(game);
        let scratch = path.with_extension("new");
        if !self.fs.exists(&scratch)? {
            return Ok(());
        }
        cancel.check()?;
        let mut scratch_file = regular(&self.fs, &scratch, false)?;
        let candidate: ReadyRecord = match read_json(&mut scratch_file) {
            Ok(candidate) => candidate,
            Err(error)
                if error
                    .downcast_ref::<serde_json::Error>()
                    .is_some_and(|e| e.is_eof() || e.is_syntax()) =>
            {
                ensure!(
                    self.has_index(game)?,
                    "Uncommitted interrupted Ready scratch"
                );
                let handles = self.verify_files(game, true, cancel)?;
                cancel.check()?;
                self.fs.unlink_registered(&scratch_file)?;
                return self.publish_ready(game, &handles, lock);
            }
            Err(error) => return Err(error),
        };
        ensure!(
            candidate.version == 1 && same_game(&candidate.game, game),
            "Unknown Ready scratch identity"
        );
        if self.fs.exists(&self.job(game).join("job.json"))? {
            let journal: Journal = load_json(&self.fs, &self.job(game).join("job.json"))?;
            validate_journal(&journal, game)?;
            ensure!(
                ["installing", "ready"].contains(&journal.phase.as_str())
                    && self.committed_target(game)?,
                "Uncommitted Ready scratch"
            );
        } else {
            ensure!(self.has_index(game)?, "Uncommitted Ready scratch");
        }
        let handles = self.verify_files(game, true, cancel)?;
        if self.fs.exists(&path)? {
            let current: ReadyRecord = load_json(&self.fs, &path)?;
            ensure!(
                current.version == 1 && same_game(&current.game, game),
                "Unknown existing Ready identity"
            );
        }
        if let Some(metadata) = &candidate.metadata {
            ensure!(
                metadata.len() == handles.len()
                    && metadata.iter().zip(&handles).all(|(expected, file)| {
                        expected.matches_stat(&file.metadata)
                            && expected.sampled_sha256.as_ref().is_none_or(|hash| {
                                Some(hash) == file.metadata.sampled_sha256.as_ref()
                            })
                    }),
                "Ready scratch payload metadata mismatch"
            );
        }
        cancel.check()?;
        self.fs.unlink_registered(&scratch_file)?;
        self.publish_ready(game, &handles, lock)
    }
    fn recover_metadata(
        &self,
        game: &NetworkGame,
        cancel: &Cancellation,
        lock: &CacheLock,
    ) -> Result<()> {
        self.recover_job_scratch(game, cancel)?;
        self.recover_index_scratch(game, cancel, lock)
    }
    fn verified_failed_payload(
        &self,
        game: &NetworkGame,
        journal: &Journal,
        file_id: &str,
        cancel: &Cancellation,
    ) -> Result<AnchoredFile> {
        let index = game
            .files
            .iter()
            .position(|p| p.id == file_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown retry payload"))?;
        let checkpoint = journal
            .partials
            .iter()
            .find(|p| p.id == file_id)
            .ok_or_else(|| anyhow::anyhow!("Missing failed checkpoint"))?;
        ensure!(
            checkpoint.bytes == game.files[index].bytes
                && checkpoint.prefix_sha256 != game.files[index].sha256,
            "Retry requires a complete known wrong-SHA payload"
        );
        let mut file = regular(&self.fs, &self.payload(game, index), true)?;
        let (bytes, hash, mut verification) =
            hash_reader_progress(&mut file, cancel, checkpoint.bytes)?;
        ensure!(
            bytes == checkpoint.bytes && hash == checkpoint.prefix_sha256,
            "Changed failed payload must be preserved"
        );
        file.validate()?;
        verification.complete();
        Ok(file)
    }
    pub fn reset_failed_payload(
        &self,
        game: &NetworkGame,
        file_id: &str,
        cancel: &Cancellation,
    ) -> Result<()> {
        self.prepare(game)?;
        let _lock = CacheLock::acquire(&self.fs, &self.state().join("active.lock"))?;
        let _awake = if let Some(path) = &self.config.stay_awake {
            ExclusiveFile::acquire(path, true, self.config.expected_root.device)?
        } else {
            None
        };
        cancel.check()?;
        self.recover_metadata(game, cancel, &_lock)?;
        ensure!(
            !self.unloading(game)?,
            "Interrupted unload; finish through Manage storage"
        );
        ensure!(
            !self.fs.exists(&self.index(game))?
                && !self.fs.exists(&self.config.root.join(&game.cache_path))?,
            "Installed games cannot be reset"
        );
        let mut journal = self.journal(game)?;
        validate_journal(&journal, game)?;
        ensure!(journal.phase == "downloading", "Install already committed");
        let file = self.verified_failed_payload(game, &journal, file_id, cancel)?;
        cancel.check()?;
        let metadata = file.metadata()?;
        let previous = journal.partials.iter().find(|p| p.id == file_id).unwrap();
        journal.retry_file = Some(RetryIntent {
            file_id: file_id.into(),
            failed_bytes: previous.bytes,
            failed_sha256: previous.prefix_sha256.clone(),
            device: metadata.dev(),
            inode: metadata.ino(),
        });
        let checkpoint = journal
            .partials
            .iter_mut()
            .find(|p| p.id == file_id)
            .unwrap();
        checkpoint.bytes = 0;
        checkpoint.prefix_sha256 = digest_hex(Sha256::digest([]).as_slice());
        self.save_journal(&journal)?;
        file.set_len(0)?;
        file.sync_all()?;
        Ok(())
    }
    fn unload_index(&self, game: &NetworkGame) -> PathBuf {
        self.index(game)
            .with_file_name(format!("{}.unloading.json", game.id))
    }
    pub fn unloading(&self, game: &NetworkGame) -> Result<bool> {
        let mut path = self.unload_index(game);
        if !self.fs.exists(&path)? {
            path = path.with_extension("new");
            if !self.fs.exists(&path)? {
                return Ok(false);
            }
        }
        let record: UnloadRecord = load_json(&self.fs, &path)?;
        validate_unload(&record, game)?;
        Ok(true)
    }
    pub fn has_index(&self, game: &NetworkGame) -> Result<bool> {
        if self.unloading(game)? {
            return Ok(false);
        }
        if !self.fs.exists(&self.index(game))? {
            return Ok(false);
        }
        let record: ReadyRecord = load_json(&self.fs, &self.index(game))?;
        ensure!(
            record.version == 1 && same_game(&record.game, game),
            "Unregistered cache index"
        );
        Ok(true)
    }
    fn installed_metadata(
        &self,
        game: &NetworkGame,
        cancel: &Cancellation,
    ) -> Result<Vec<PayloadMetadata>> {
        game.targets()?
            .iter()
            .zip(&game.files)
            .map(|(path, payload)| {
                let file = regular(&self.fs, &self.config.root.join(path), false)?;
                let metadata = PayloadMetadata::read(&payload.id, &file, cancel)?;
                ensure!(metadata.bytes == payload.bytes, "Unexpected payload length");
                Ok(metadata)
            })
            .collect()
    }
    fn publish_ready(
        &self,
        game: &NetworkGame,
        handles: &[VerifiedFile],
        _lock: &CacheLock,
    ) -> Result<()> {
        let metadata = handles
            .iter()
            .zip(&game.files)
            .map(|(file, payload)| {
                ensure!(
                    file.metadata
                        == PayloadMetadata::read(&payload.id, file, &Cancellation::default())?,
                    "Verified payload changed before metadata publication"
                );
                Ok(file.metadata.clone())
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            metadata.len() == game.files.len(),
            "Incomplete verified install"
        );
        save_json(
            &self.fs,
            &self.index(game),
            &ReadyRecord {
                version: 1,
                game: game.clone(),
                metadata: Some(metadata),
            },
        )
    }
    pub fn install_state(&self, game: &NetworkGame) -> Result<InstallState> {
        let mut missing = false;
        for (path, payload) in game.targets()?.iter().zip(&game.files) {
            let path = self.config.root.join(path);
            if !self.fs.exists(&path)? {
                missing = true;
                continue;
            }
            let Ok(file) = regular(&self.fs, &path, false) else {
                return Ok(InstallState::Conflicting);
            };
            if file.metadata()?.len() != payload.bytes {
                return Ok(InstallState::Conflicting);
            }
            file.validate()?;
        }
        Ok(if missing {
            InstallState::Missing
        } else {
            InstallState::Complete
        })
    }
    pub fn target_exists(&self, game: &NetworkGame) -> Result<bool> {
        self.fs.exists(&self.config.root.join(&game.cache_path))
    }
    pub fn resume_path(&self) -> Result<Option<PathBuf>> {
        let path = self.config.root.join(".allium/state/current_game");
        if !self.fs.exists(&path)? {
            return Ok(None);
        }
        let value: serde_json::Value = load_json(&self.fs, &path)?;
        let path = value
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Unknown resume metadata; storage removal blocked"))?;
        Ok(Some(PathBuf::from(path)))
    }
    pub fn resume_protected(&self, game: &NetworkGame) -> Result<bool> {
        let Some(resume) = self.resume_path()? else {
            return Ok(false);
        };
        let resume = resume.to_string_lossy().to_lowercase();
        let mut paths = game.targets()?;
        paths.push(PathBuf::from(&game.game_path));
        Ok(paths
            .iter()
            .any(|path| self.config.root.join(path).to_string_lossy().to_lowercase() == resume))
    }
    pub fn adopt_existing(&self, game: &NetworkGame, cancel: &Cancellation) -> Result<CacheState> {
        self.prepare_adoption(game)?;
        let lock = CacheLock::acquire(&self.fs, &self.state().join("active.lock"))?;
        let _awake = self.awake_lease()?;
        cancel.check()?;
        ensure!(
            !self.unloading(game)?,
            "Interrupted unload; finish through Manage storage"
        );
        ensure!(!self.has_index(game)?, "Already registered");
        self.ensure_no_managed_partial(game)?;
        let handles = self.verify_files(game, true, cancel)?;
        for file in &handles {
            file.validate()?;
        }
        cancel.check()?;
        self.ensure_no_managed_partial(game)?;
        self.publish_ready(game, &handles, &lock)?;
        self.ready_state(game, cancel)
    }
    fn awake_lease(&self) -> Result<Option<ExclusiveFile>> {
        self.config.stay_awake.as_ref().map_or(Ok(None), |path| {
            ExclusiveFile::acquire(path, true, self.config.expected_root.device)
        })
    }
    pub fn unload_registered(&self, game: &NetworkGame, cancel: &Cancellation) -> Result<()> {
        self.prepare(game)?;
        let _lock = CacheLock::acquire(&self.fs, &self.state().join("active.lock"))?;
        let _awake = self.awake_lease()?;
        let intent_path = self.unload_index(game);
        let scratch = intent_path.with_extension("new");
        if self.fs.exists(&scratch)? {
            let record: UnloadRecord = load_json(&self.fs, &scratch)?;
            validate_unload(&record, game)?;
            promote_metadata(&self.fs, &scratch, &intent_path)?;
        }
        ensure!(
            !self.resume_protected(game)?,
            "Current resume game cannot be unloaded"
        );
        self.ensure_unload_intent(game, &intent_path, cancel)?;
        let record: UnloadRecord = load_json(&self.fs, &intent_path)?;
        validate_unload(&record, game)?;
        let handles = self.verify_unload_remaining(game, &record, cancel)?;
        cancel.check()?;
        self.finish_unload(game, &record, &handles, &intent_path)
    }
    fn ensure_unload_intent(
        &self,
        game: &NetworkGame,
        intent_path: &Path,
        cancel: &Cancellation,
    ) -> Result<()> {
        if !self.unloading(game)? {
            ensure!(
                self.has_index(game)?,
                "Unregistered game cannot be unloaded"
            );
            let files = self.verify_files(game, true, cancel)?;
            let targets = game.targets()?;
            let record = UnloadRecord {
                version: 1,
                game: game.clone(),
                unload_files: files
                    .iter()
                    .zip(targets)
                    .map(|(f, path)| {
                        let id = f.identity();
                        UnloadLeaf {
                            path,
                            device: id.device,
                            inode: id.inode,
                        }
                    })
                    .collect(),
                unload_dirs: unload_dirs(game)?
                    .into_iter()
                    .map(|path| {
                        let held = self.fs.pin_source(&self.config.root.join(&path))?;
                        held.validate()?;
                        let id = held.identity();
                        Ok(UnloadLeaf {
                            path,
                            device: id.device,
                            inode: id.inode,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            };
            for file in &files {
                file.validate()?;
            }
            cancel.check()?;
            save_json(&self.fs, &intent_path, &record)?;
        }
        Ok(())
    }
    fn verify_unload_remaining(
        &self,
        game: &NetworkGame,
        record: &UnloadRecord,
        cancel: &Cancellation,
    ) -> Result<Vec<AnchoredFile>> {
        let mut handles = Vec::new();
        for (leaf, payload) in record.unload_files.iter().zip(&game.files) {
            cancel.check()?;
            let path = self.config.root.join(&leaf.path);
            if !self.fs.exists(&path)? {
                continue;
            }
            let mut file = regular(&self.fs, &path, false)?;
            ensure!(
                file.identity() == leaf.identity(),
                "Replaced unload payload preserved"
            );
            let (bytes, hash, mut verification) = hash_reader_progress(
                &mut Read::by_ref(&mut file).take(payload.bytes + 1),
                cancel,
                payload.bytes,
            )?;
            ensure!(
                bytes == payload.bytes && hash == payload.sha256,
                "Changed unload payload preserved"
            );
            file.validate()?;
            verification.complete();
            handles.push(file);
        }
        if game.install_kind == "directory" && self.target_exists(game)? {
            let expected = game
                .targets()?
                .iter()
                .map(|p| self.config.root.join(p))
                .collect();
            verify_tree(
                &self.fs,
                &self.config.root.join(&game.cache_path),
                &expected,
                cancel,
            )?;
        }
        for directory in &record.unload_dirs {
            let path = self.config.root.join(&directory.path);
            if self.fs.exists(&path)? {
                let pinned = self.fs.pin_source(&path)?;
                ensure!(
                    pinned.identity() == directory.identity(),
                    "Replaced unload directory preserved"
                );
                pinned.validate()?;
            }
        }
        Ok(handles)
    }
    fn finish_unload(
        &self,
        game: &NetworkGame,
        record: &UnloadRecord,
        handles: &[AnchoredFile],
        intent_path: &Path,
    ) -> Result<()> {
        // Durable intent makes partial deletion non-Ready; a later explicit Unload rechecks every remaining inode/hash.
        let index_path = self.index(game);
        if self.fs.exists(&index_path)? {
            let index: ReadyRecord = load_json(&self.fs, &index_path)?;
            ensure!(
                index.version == 1 && same_game(&index.game, game),
                "Unknown Ready index preserved"
            );
            self.fs
                .unlink_registered(&regular(&self.fs, &index_path, false)?)?;
        }
        for file in handles {
            self.fs.unlink_registered(file)?;
        }
        for directory in &record.unload_dirs {
            let path = self.config.root.join(&directory.path);
            if self.fs.exists(&path)? {
                self.fs
                    .remove_empty_directory(&path, directory.identity())?;
            }
        }
        let journal = self.job(game).join("job.json");
        if self.fs.exists(&journal)? {
            let record: Journal = load_json(&self.fs, &journal)?;
            validate_journal(&record, game)?;
            self.fs
                .unlink_registered(&regular(&self.fs, &journal, false)?)?;
        }
        self.fs
            .unlink_registered(&regular(&self.fs, &intent_path, false)?)?;
        Ok(())
    }
    fn reset_missing_ready_journal(&self, game: &NetworkGame, target_exists: bool) -> Result<()> {
        let path = self.job(game).join("job.json");
        if target_exists || !self.fs.exists(&path)? {
            return Ok(());
        }
        let journal: Journal = load_json(&self.fs, &path)?;
        validate_journal(&journal, game)?;
        if journal.phase == "ready" && self.committed_target(game)? {
            self.fs
                .unlink_registered(&regular(&self.fs, &path, false)?)?;
            sync_directory(&self.fs, path.parent().unwrap())?;
        }
        Ok(())
    }
    fn verify_member(
        &self,
        game: &NetworkGame,
        index: usize,
        path: &Path,
        cancel: &Cancellation,
    ) -> Result<VerifiedFile> {
        let payload = &game.files[index];
        let mut file = regular(&self.fs, path, false)?;
        let before = PayloadMetadata::read(&payload.id, &file, cancel)?;
        ensure!(
            before.bytes == payload.bytes,
            "Conflicting installed member length; existing files preserved"
        );
        let (bytes, hash, mut verification) = hash_reader_progress(
            &mut Read::by_ref(&mut file).take(payload.bytes + 1),
            cancel,
            payload.bytes,
        )?;
        ensure!(
            bytes == payload.bytes && hash == payload.sha256,
            "Full payload SHA/length mismatch; existing files preserved"
        );
        ensure!(
            PayloadMetadata::read(&payload.id, &file, cancel)? == before,
            "Payload changed during verification"
        );
        verification.complete();
        Ok(VerifiedFile {
            file,
            metadata: before,
        })
    }
    fn recover_members(
        &self,
        game: &NetworkGame,
        endpoint: &Endpoint,
        cancel: &Cancellation,
        space: &dyn SpaceProbe,
        progress: &SyncSender<Progress>,
        lock: &CacheLock,
    ) -> Result<CacheState> {
        ensure!(
            game.install_kind == "directory",
            "Existing ROM target requires explicit adoption"
        );
        let targets = game
            .targets()?
            .iter()
            .map(|p| self.config.root.join(p))
            .collect::<Vec<_>>();
        let expected = targets.iter().cloned().collect::<HashSet<_>>();
        verify_tree(
            &self.fs,
            &self.config.root.join(&game.cache_path),
            &expected,
            cancel,
        )?;
        let mut journal = self.journal(game)?;
        validate_journal(&journal, game)?;
        ensure!(
            journal.phase != "ready",
            "Unconsumed Ready job must be preserved"
        );
        let staged_root = self.job(game).join("payloads");
        if self.fs.exists(&staged_root)? {
            let expected = (0..game.files.len())
                .map(|i| self.payload(game, i))
                .collect();
            verify_tree(&self.fs, &staged_root, &expected, cancel)?;
        }
        let mut handles = Vec::new();
        let mut missing = Vec::new();
        for (index, target) in targets.iter().enumerate() {
            if self.fs.exists(target)? {
                ensure!(
                    !self.fs.exists(&self.payload(game, index))?,
                    "Ambiguous staged and installed member; partial job preserved"
                );
                handles.push((index, self.verify_member(game, index, target, cancel)?));
            } else {
                missing.push(index);
            }
        }
        self.check_capacity(game, &journal, space, cancel)?;
        let mut sources = Vec::new();
        for index in &missing {
            let file = self.transfer_file(
                game,
                *index,
                &mut journal,
                endpoint,
                cancel,
                space,
                progress,
            )?;
            let source = self.fs.pin_source(&self.payload(game, *index))?;
            sources.push((*index, source, file));
        }
        self.verifying(game, progress, cancel)?;
        #[cfg(test)]
        if let Some(hook) = self.commit_hook.lock().unwrap().take() {
            hook();
        }
        cancel.check()?;
        self.check_reserve(space)?;
        journal.phase = "installing".into();
        self.save_journal(&journal)?;
        for (index, source, file) in sources {
            for (_, existing) in &handles {
                existing.validate()?;
            }
            file.validate()?;
            directories(&self.fs, targets[index].parent().unwrap())?;
            self.check_reserve(space)?;
            self.fs.rename_verified(&source, &targets[index])?;
            sync_directory(&self.fs, self.payload(game, index).parent().unwrap())?;
            sync_directory(&self.fs, targets[index].parent().unwrap())?;
            let moved = regular(&self.fs, &targets[index], false)?;
            ensure!(
                moved.identity() == file.identity(),
                "Installed member identity changed"
            );
            handles.push((
                index,
                VerifiedFile {
                    file: moved,
                    metadata: file.metadata,
                },
            ));
        }
        handles.sort_by_key(|(index, _)| *index);
        let handles = self.installed_streamed(
            game,
            handles.into_iter().map(|(_, file)| file).collect(),
            cancel,
        )?;
        self.check_reserve(space)?;
        self.publish_ready(game, &handles, lock)?;
        journal.phase = "ready".into();
        self.save_journal(&journal)?;
        self.ready_state(game, cancel)
    }
    fn verifying(
        &self,
        game: &NetworkGame,
        progress: &SyncSender<Progress>,
        cancel: &Cancellation,
    ) -> Result<()> {
        send_verification_phase(
            progress,
            Progress {
                game_id: game.id.clone(),
                phase: "verifying",
                bytes_done: 0,
                bytes_total: 0,
            },
            cancel,
        )
    }
    pub fn run(
        &self,
        game: &NetworkGame,
        endpoint: &Endpoint,
        cancel: &Cancellation,
        space: &dyn SpaceProbe,
        progress: &SyncSender<Progress>,
    ) -> Result<CacheState> {
        let cancel = &verification_reporting(cancel, game, progress);
        cancel.check()?;
        if self.has_index(game)?
            && self.install_state(game)? == InstallState::Complete
            && !self.fs.exists(&self.index(game).with_extension("new"))?
            && !self.fs.exists(&self.job(game).join("job.new"))?
        {
            if let Some(state) = self.cached_ready(game, cancel)? {
                return Ok(state);
            }
        }
        self.check_reserve(space)?;
        self.prepare(game)?;
        let lock = CacheLock::acquire(&self.fs, &self.state().join("active.lock"))?;
        let _awake = self.awake_lease()?;
        cancel.check()?;
        let recovering = self.fs.exists(&self.index(game).with_extension("new"))?
            || self.fs.exists(&self.job(game).join("job.new"))?;
        if recovering {
            self.verifying(game, progress, cancel)?;
        }
        self.recover_metadata(game, cancel, &lock)?;
        ensure!(
            !self.unloading(game)?,
            "Interrupted unload; finish through Manage storage"
        );
        let target_exists = self.target_exists(game)?;
        let state = self.install_state(game)?;
        ensure!(
            state != InstallState::Conflicting,
            "Conflicting installed member: wrong length or type; existing files preserved"
        );
        if self.has_index(game)? && state == InstallState::Complete {
            if let Some(state) = self.cached_ready(game, cancel)? {
                return Ok(state);
            }
            if !recovering {
                self.verifying(game, progress, cancel)?;
            }
            return self.verify_ready(game, cancel, &lock);
        }
        self.reset_missing_ready_journal(game, state == InstallState::Complete)?;
        if target_exists && state == InstallState::Missing {
            return self.recover_members(game, endpoint, cancel, space, progress, &lock);
        }
        let mut journal = self.journal(game)?;
        validate_journal(&journal, game)?;
        if ["installing", "ready"].contains(&journal.phase.as_str()) {
            self.verifying(game, progress, cancel)?;
            self.check_reserve(space)?;
            self.install(&mut journal, cancel, space, &lock)?;
            return self.ready_state(game, cancel);
        }
        ensure!(
            !self.fs.exists(&self.config.root.join(&game.cache_path))?,
            "Unregistered existing game target"
        );
        self.check_capacity(game, &journal, space, cancel)?;
        let mut handles = Vec::new();
        for index in 0..game.files.len() {
            handles.push(self.transfer_file(
                game,
                index,
                &mut journal,
                endpoint,
                cancel,
                space,
                progress,
            )?);
        }
        self.verifying(game, progress, cancel)?;
        self.install_streamed(&mut journal, handles, cancel, space, &lock)?;
        self.ready_state(game, cancel)
    }
}
fn send_verification_phase(
    progress: &SyncSender<Progress>,
    mut update: Progress,
    cancel: &Cancellation,
) -> Result<()> {
    loop {
        cancel.check()?;
        match progress.try_send(update) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(pending)) => update = pending,
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
pub fn verification_reporting(
    cancel: &Cancellation,
    game: &NetworkGame,
    progress: &SyncSender<Progress>,
) -> Cancellation {
    let progress = progress.clone();
    let game_id = game.id.clone();
    let phase_cancel = cancel.clone();
    cancel.reporting(move |bytes_done, bytes_total, report| {
        let update = Progress {
            game_id: game_id.clone(),
            phase: "verifying",
            bytes_done,
            bytes_total,
        };
        match report {
            crate::network_transfer::VerificationReport::Started => {
                let _ = send_verification_phase(&progress, update, &phase_cancel);
            }
            crate::network_transfer::VerificationReport::Progress => {
                let _ = progress.try_send(update);
            }
        }
    })
}
fn admit_transfer(observed: Space, incoming: u64) -> Result<()> {
    ensure!(observed.allocation_unit > 0, "Zero allocation unit");
    let required = RESERVE_BYTES
        .checked_add(METADATA_MARGIN)
        .and_then(|n| n.checked_add(incoming))
        .and_then(|n| n.checked_add(observed.allocation_unit))
        .ok_or_else(|| anyhow::anyhow!("Transfer space overflow"))?;
    ensure!(
        observed.free_bytes >= required,
        "Space lost during transfer"
    );
    Ok(())
}
fn verify_retry_payload(
    file: &mut AnchoredFile,
    intent: &RetryIntent,
    cancel: &Cancellation,
) -> Result<()> {
    cancel.check()?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.dev() == intent.device && metadata.ino() == intent.inode,
        "Replaced retry payload must be preserved"
    );
    if metadata.len() == 0 {
        return Ok(());
    }
    ensure!(
        metadata.len() == intent.failed_bytes,
        "Changed retry length must be preserved"
    );
    file.seek(SeekFrom::Start(0))?;
    let (bytes, hash, mut verification) = hash_reader_progress(file, cancel, intent.failed_bytes)?;
    ensure!(
        bytes == intent.failed_bytes && hash == intent.failed_sha256,
        "Changed retry bytes must be preserved"
    );
    cancel.check()?;
    file.seek(SeekFrom::Start(0))?;
    verification.complete();
    Ok(())
}
fn validate_journal(journal: &Journal, game: &NetworkGame) -> Result<()> {
    ensure!(
        journal.version == 1
            && same_game(&journal.game, game)
            && ["downloading", "installing", "ready"].contains(&journal.phase.as_str()),
        "Unknown journal identity/phase"
    );
    let mut ids = HashSet::new();
    for partial in &journal.partials {
        let payload = game
            .files
            .iter()
            .find(|p| p.id == partial.id)
            .ok_or_else(|| anyhow::anyhow!("Unknown checkpoint"))?;
        ensure!(
            ids.insert(&partial.id)
                && partial.bytes <= payload.bytes
                && partial.prefix_sha256.len() == 64
                && partial
                    .prefix_sha256
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "Invalid checkpoint"
        );
    }
    if let Some(intent) = &journal.retry_file {
        let payload = game
            .files
            .iter()
            .find(|p| p.id == intent.file_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown retry file"))?;
        ensure!(
            journal.phase == "downloading"
                && intent.failed_bytes == payload.bytes
                && intent.failed_sha256 != payload.sha256
                && intent.failed_sha256.len() == 64
                && intent
                    .failed_sha256
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                && journal.partials.iter().any(|p| p.id == intent.file_id
                    && p.bytes == 0
                    && p.prefix_sha256 == digest_hex(Sha256::digest([]).as_slice())),
            "Invalid retry intent"
        );
    }
    Ok(())
}
fn validate_progression(old: &Journal, new: &Journal) -> Result<()> {
    let rank = |phase: &str| match phase {
        "downloading" => 0,
        "installing" => 1,
        _ => 2,
    };
    ensure!(
        rank(new.phase.as_str()) >= rank(old.phase.as_str()),
        "Metadata phase regression"
    );
    if old.retry_file.is_some() && new.retry_file.is_some() {
        ensure!(
            old.retry_file == new.retry_file,
            "Retry intent identity changed"
        );
    }
    for previous in &old.partials {
        let next = new
            .partials
            .iter()
            .find(|p| p.id == previous.id)
            .ok_or_else(|| anyhow::anyhow!("Missing committed checkpoint"))?;
        if next.bytes < previous.bytes {
            let file = old.game.files.iter().find(|p| p.id == previous.id).unwrap();
            ensure!(
                new.retry_file
                    .as_ref()
                    .is_some_and(|r| r.file_id == previous.id
                        && r.failed_bytes == previous.bytes
                        && r.failed_sha256 == previous.prefix_sha256)
                    && next.bytes == 0
                    && previous.bytes == file.bytes
                    && previous.prefix_sha256 != file.sha256,
                "Unproven checkpoint regression"
            );
        } else if next.bytes == previous.bytes {
            ensure!(
                next.prefix_sha256 == previous.prefix_sha256,
                "Changed committed checkpoint"
            );
        }
    }
    Ok(())
}
fn sync_tree_directories(fs: &AnchoredFs, path: &Path, cancel: &Cancellation) -> Result<()> {
    cancel.check()?;
    if fs.open(path, false).is_ok() {
        return fs.sync_directory(path.parent().unwrap());
    }
    for (child, directory) in fs.entries(path)? {
        if directory {
            sync_tree_directories(fs, &child, cancel)?;
        } else {
            cancel.check()?;
        }
    }
    fs.sync_directory(path)
}
fn verify_tree(
    root: &AnchoredFs,
    directory: &Path,
    expected: &HashSet<PathBuf>,
    cancel: &Cancellation,
) -> Result<()> {
    cancel.check()?;
    for (path, is_directory) in root.entries(directory)? {
        cancel.check()?;
        if is_directory {
            ensure!(
                expected.iter().any(|p| p.starts_with(&path)),
                "Unexpected directory"
            );
            verify_tree(root, &path, expected, cancel)?;
        } else {
            ensure!(expected.contains(&path), "Unexpected file");
        }
    }
    Ok(())
}
struct CheckpointWriter<'a> {
    cache: &'a Cache,
    journal: &'a mut Journal,
    index: usize,
    file: AnchoredFile,
    hash: Sha256,
    bytes: u64,
    last_checkpoint: u64,
    samples: StreamSamples,
    progress: &'a SyncSender<Progress>,
    cancel: &'a Cancellation,
    verification: Option<crate::network_transfer::VerificationProgress<'a>>,
}
impl<'a> CheckpointWriter<'a> {
    fn begin_verification(&mut self) {
        if self.verification.is_none() {
            let total = sample_regions(self.journal.game.files[self.index].bytes)
                .iter()
                .map(|(_, size)| *size)
                .sum();
            self.verification = Some(self.cancel.verification(total));
        }
    }
    fn finish(mut self, cancel: &Cancellation) -> Result<VerifiedFile> {
        cancel.check()?;
        self.begin_verification();
        cancel.check()?;
        let payload = &self.journal.game.files[self.index];
        let total = sample_regions(payload.bytes)
            .iter()
            .map(|(_, size)| *size)
            .sum();
        let mut verification = self.verification.take().unwrap();
        cancel.sync_with_heartbeat(total, || self.file.sync_all().map_err(Into::into))?;
        cancel.check()?;
        ensure!(
            self.bytes == payload.bytes
                && digest_hex(self.hash.finalize().as_slice()) == payload.sha256,
            "Full payload SHA/length mismatch"
        );
        cancel.verification_event("verify_readback_begin", "streamed");
        let metadata = PayloadMetadata::read_reported(
            &payload.id,
            &self.file,
            cancel,
            Some(&mut verification),
        );
        cancel.verification_event("verify_readback_end", "streamed");
        let metadata = metadata?;
        ensure!(
            metadata.bytes == payload.bytes
                && metadata.sampled_sha256.as_deref() == Some(self.samples.fingerprint().as_str()),
            "Streamed payload read-back mismatch"
        );
        verification.complete();
        Ok(VerifiedFile {
            file: self.file,
            metadata,
        })
    }

    fn persist_checkpoint(&mut self) -> Result<()> {
        self.file.sync_all()?;
        let id = self.journal.game.files[self.index].id.clone();
        if self.bytes > 0
            && self
                .journal
                .retry_file
                .as_ref()
                .is_some_and(|r| r.file_id == id)
        {
            self.journal.retry_file = None;
        }
        self.journal.partials.retain(|p| p.id != id);
        self.journal.partials.push(Partial {
            id,
            bytes: self.bytes,
            prefix_sha256: digest_hex(self.hash.clone().finalize().as_slice()),
        });
        self.cache.save_journal(self.journal)?;
        self.last_checkpoint = self.bytes;
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<()> {
        let final_bytes = self.bytes == self.journal.game.files[self.index].bytes;
        if final_bytes {
            self.begin_verification();
            let cancel = self.cancel;
            cancel.check()?;
            let total = sample_regions(self.bytes)
                .iter()
                .map(|(_, size)| *size)
                .sum();
            cancel.sync_with_heartbeat(total, || self.persist_checkpoint())?;
            return Ok(());
        }
        self.persist_checkpoint()?;
        let _ = self.progress.try_send(Progress {
            game_id: self.journal.game.id.clone(),
            phase: "downloading",
            bytes_done: self.journal.partials.iter().map(|p| p.bytes).sum(),
            bytes_total: self.journal.game.files.iter().map(|f| f.bytes).sum(),
        });
        Ok(())
    }
}
impl Write for CheckpointWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let size = self.file.write(bytes)?;
        self.hash.update(&bytes[..size]);
        self.samples.capture(self.bytes, &bytes[..size]);
        let previous = self.bytes;
        self.bytes += size as u64;
        if previous / WINDOW as u64 != self.bytes / WINDOW as u64 {
            // Volatile UI progress is separate from the durable 1 MiB checkpoint cadence.
            let id = &self.journal.game.files[self.index].id;
            let _ = self.progress.try_send(Progress {
                game_id: self.journal.game.id.clone(),
                phase: "downloading",
                bytes_done: self.bytes
                    + self
                        .journal
                        .partials
                        .iter()
                        .filter(|p| &p.id != id)
                        .map(|p| p.bytes)
                        .sum::<u64>(),
                bytes_total: self.journal.game.files.iter().map(|f| f.bytes).sum(),
            });
        }
        if self.bytes - self.last_checkpoint >= 1024 * 1024 {
            self.checkpoint().map_err(std::io::Error::other)?;
        }
        Ok(size)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
pub struct Worker {
    pub cancel: Cancellation,
    pub progress: Receiver<Progress>,
    pub result: Receiver<Result<CacheState>>,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn start(config: CacheConfig, game: NetworkGame, endpoint: Endpoint) -> Result<Self> {
        ensure!(
            WORKER_ACTIVE
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "Transfer worker already active"
        );
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let (progress_tx, progress) = mpsc::sync_channel(16);
        let (result_tx, result) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("network-cache".into())
            .spawn(move || {
                let result = Cache::new(config).and_then(|cache| {
                    cache.run(
                        &game,
                        &endpoint,
                        &worker_cancel,
                        &FilesystemSpace,
                        &progress_tx,
                    )
                });
                WORKER_ACTIVE.store(false, Ordering::Release);
                let _ = result_tx.send(result);
            });
        match thread {
            Ok(thread) => Ok(Self {
                cancel,
                progress,
                result,
                thread: Some(thread),
            }),
            Err(error) => {
                WORKER_ACTIVE.store(false, Ordering::Release);
                Err(error.into())
            }
        }
    }
    pub fn join(mut self) -> Result<()> {
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("Worker panicked"))?;
        }
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod dl8_tests {
    include!("network_cache_dl11_tests.rs");
    use super::*;
    use crate::network_library::Payload;
    use std::fs;
    struct Fixture {
        root: PathBuf,
        game: NetworkGame,
    }
    impl Fixture {
        fn new(label: &str, playlist: bool) -> Self {
            let root = std::env::temp_dir().join(format!("dl8-{label}-{}", std::process::id()));
            assert!(!root.exists());
            fs::create_dir_all(root.join("Roms/PS")).unwrap();
            fs::create_dir_all(root.join(".allium/state/network-library/installed")).unwrap();
            let root = fs::canonicalize(root).unwrap();
            let revision = "b".repeat(64);
            let cache = if playlist {
                "Roms/PS/Fixture.m3u"
            } else {
                "Roms/PS/Fixture"
            };
            let launch = if playlist { "Fixture.m3u" } else { "disc.cue" };
            let names = [launch, "disc.bin", "disc2.bin"];
            let files = names
                .iter()
                .enumerate()
                .map(|(i, name)| Payload {
                    id: format!("file{i}"),
                    relative_path: name.to_string(),
                    bytes: 4,
                    sha256: digest_hex(Sha256::digest(b"data").as_slice()),
                    role: if i == 0 {
                        if playlist { "playlist" } else { "cue" }
                    } else {
                        "rom"
                    }
                    .into(),
                    http_route: format!("/v1/files/fixture/{revision}/file{i}"),
                })
                .collect();
            let game = NetworkGame {
                id: "fixture".into(),
                display_title: "Fixture".into(),
                platform: "PS".into(),
                bundle_revision: revision,
                game_path: if playlist {
                    cache.into()
                } else {
                    format!("{cache}/{launch}")
                },
                cache_path: cache.into(),
                launch_path: format!("{cache}/{launch}"),
                install_kind: "directory".into(),
                files,
                discs: vec!["file1".into(), "file2".into()],
            };
            fs::create_dir_all(root.join(cache)).unwrap();
            for path in game.targets().unwrap() {
                fs::write(root.join(path), b"data").unwrap();
            }
            Self { root, game }
        }
        fn cache(&self) -> Cache {
            Cache::new(CacheConfig::bind(self.root.clone(), None).unwrap()).unwrap()
        }
        fn stage_missing(&self, cache: &Cache) {
            let job = cache.job(&self.game);
            fs::create_dir_all(job.join("payloads")).unwrap();
            fs::write(cache.payload(&self.game, 2), b"data").unwrap();
            fs::write(job.join("job.json"),serde_json::to_vec(&serde_json::json!({"version":1,"game":self.game,"phase":"downloading","partials":[{"id":"file2","bytes":4,"prefix_sha256":self.game.files[2].sha256}]})).unwrap()).unwrap();
        }
        fn run(&self, cache: &Cache) -> Result<CacheState> {
            let (tx, _rx) = mpsc::sync_channel(32);
            cache.run(
                &self.game,
                &Endpoint::lan("127.0.0.1:9".parse().unwrap()).unwrap(),
                &Cancellation::default(),
                &FilesystemSpace,
                &tx,
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
    fn run_with_cancel(f: &Fixture, cache: &Cache, cancel: &Cancellation) -> Result<CacheState> {
        let (tx, _rx) = mpsc::sync_channel(32);
        cache.run(
            &f.game,
            &Endpoint::lan("127.0.0.1:9".parse().unwrap()).unwrap(),
            cancel,
            &FilesystemSpace,
            &tx,
        )
    }
    fn change_mtime(path: &Path) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1234)),
            )
            .unwrap();
    }
    fn sampled_payload_fixture(label: &str) -> Fixture {
        let mut f = Fixture::new(label, false);
        let bytes = vec![b'a'; 1024 * 1024];
        let path = f.root.join(&f.game.targets().unwrap()[1]);
        fs::write(path, &bytes).unwrap();
        f.game.files[1].bytes = bytes.len() as u64;
        f.game.files[1].sha256 = digest_hex(Sha256::digest(&bytes).as_slice());
        f
    }
    fn mutate_preserving_mtime(f: &Fixture, replacement: bool) {
        let path = f.root.join(&f.game.targets().unwrap()[1]);
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        if replacement {
            let next = path.with_extension("replacement");
            let mut data = fs::read(&path).unwrap();
            let last = data.len() - 1;
            data[last] = b'z';
            fs::write(&next, data).unwrap();
            fs::rename(next, &path).unwrap();
        } else {
            let mut file = fs::File::options().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(
                64 * 1024 + (1024 * 1024 - 2 * 64 * 1024 - 4096) / 17,
            ))
            .unwrap();
            file.write_all(b"z").unwrap();
        }
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }
    #[test]
    fn dl10_recovered_metadata_syncs_before_replacing_committed_record() {
        let f = Fixture::new("metadata-sync-order", false);
        let root = AnchoredFs::new(&f.root).unwrap();
        let target = f.root.join(".allium/state/network-library/job.json");
        let source = target.with_extension("new");
        let previous = serde_json::json!({"version": 1, "game": f.game,
            "phase": "downloading", "partials": []});
        let mut next = previous.clone();
        next["phase"] = "ready".into();
        fs::write(&target, serde_json::to_vec(&previous).unwrap()).unwrap();
        fs::write(&source, serde_json::to_vec(&next).unwrap()).unwrap();
        promote_metadata(&root, &source, &target).unwrap();
        assert_eq!(
            root.test_metadata_operations(),
            vec![("sync", source.clone()), ("rename", source.clone())]
        );
        assert!(!source.exists());
        assert_eq!(
            load_json::<serde_json::Value>(&root, &target).unwrap(),
            next
        );
    }
    #[test]
    fn dl10_recovered_metadata_rejects_scratch_swap_before_rename() {
        for committed in [true, false] {
            let f = Fixture::new(
                if committed {
                    "metadata-swap-committed"
                } else {
                    "metadata-swap-new"
                },
                false,
            );
            let root = AnchoredFs::new(&f.root).unwrap();
            let target = f.root.join(".allium/state/network-library/job.json");
            let source = target.with_extension("new");
            let value = serde_json::json!({"version": 1, "game": f.game,
                "phase": "downloading", "partials": []});
            let bytes = serde_json::to_vec(&value).unwrap();
            if committed {
                fs::write(&target, &bytes).unwrap();
            }
            fs::write(&source, &bytes).unwrap();
            let raced = source.clone();
            let bound = root.clone();
            root.test_after_file_sync(move || {
                bound.test_before_next_operation(move || {
                    let replacement = raced.with_extension("replacement");
                    fs::write(&replacement, b"unrelated replacement").unwrap();
                    fs::rename(replacement, raced).unwrap();
                });
            });
            assert!(promote_metadata(&root, &source, &target).is_err());
            assert_eq!(fs::read(&source).unwrap(), b"unrelated replacement");
            if committed {
                assert_eq!(fs::read(&target).unwrap(), bytes);
            } else {
                assert!(!target.exists());
            }
        }
    }
    #[test]
    fn dl9_sample_catches_same_size_same_mtime_in_place_change() {
        let f = sampled_payload_fixture("sample-in-place");
        let cache = f.cache();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        mutate_preserving_mtime(&f, false);
        let cancel = Cancellation::default();
        assert!(run_with_cancel(&f, &cache, &cancel).is_err());
        assert!(cancel.verification_reads() > 0);
    }
    #[test]
    fn dl9_sample_catches_replacement_preserving_mtime() {
        let f = sampled_payload_fixture("sample-replacement");
        let cache = f.cache();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        mutate_preserving_mtime(&f, true);
        let cancel = Cancellation::default();
        assert!(run_with_cancel(&f, &cache, &cancel).is_err());
        assert!(cancel.verification_reads() > 0);
    }
    #[test]
    fn dl9_real_fallback_holds_lock_awake_and_emits_verifying() {
        let f = Fixture::new("protected-fallback", false);
        let awake = f.root.join("stay_awake");
        let cache =
            Cache::new(CacheConfig::bind(f.root.clone(), Some(awake.clone())).unwrap()).unwrap();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        change_mtime(&f.root.join(&f.game.targets().unwrap()[1]));
        let (tx, rx) = mpsc::sync_channel::<Progress>(32);
        let lock_path = cache.state().join("active.lock");
        let checked = std::sync::Arc::new(AtomicBool::new(false));
        let observed = checked.clone();
        *cache.verification_hook.lock().unwrap() = Some(Box::new(move || {
            let file = fs::File::open(lock_path).unwrap();
            assert_ne!(
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0,
                "fallback must hold cache lock before payload reads"
            );
            assert!(
                awake.exists(),
                "fallback must hold awake lease before payload reads"
            );
            assert_eq!(rx.try_recv().unwrap().phase, "verifying");
            observed.store(true, Ordering::Release);
        }));
        cache
            .run(
                &f.game,
                &Endpoint::lan("127.0.0.1:9".parse().unwrap()).unwrap(),
                &Cancellation::default(),
                &FilesystemSpace,
                &tx,
            )
            .unwrap();
        assert!(checked.load(Ordering::Acquire));
        assert!(!f.root.join("stay_awake").exists());
    }
    #[test]
    fn dl9_mtime_only_change_verifies_once() {
        let f = Fixture::new("mtime-refresh", false);
        let cache = f.cache();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        change_mtime(&f.root.join(&f.game.targets().unwrap()[1]));
        let cancel = Cancellation::default();
        run_with_cancel(&f, &cache, &cancel).unwrap();
        assert!(cancel.verification_reads() > 0);
        let index = fs::read(cache.index(&f.game)).unwrap();
        let cancel = Cancellation::default();
        run_with_cancel(&f, &cache, &cancel).unwrap();
        assert_eq!(cancel.verification_reads(), 0);
        assert_eq!(fs::read(cache.index(&f.game)).unwrap(), index);
    }
    fn interrupted_index_recovers(label: &str, contents: &[u8]) {
        let f = Fixture::new(label, false);
        let cache = f.cache();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        let scratch = cache.index(&f.game).with_extension("new");
        fs::write(&scratch, contents).unwrap();
        let cancel = Cancellation::default();
        run_with_cancel(&f, &cache, &cancel).unwrap();
        assert!(cancel.verification_reads() > 0);
        assert!(!scratch.exists());
        let cancel = Cancellation::default();
        run_with_cancel(&f, &cache, &cancel).unwrap();
        assert_eq!(cancel.verification_reads(), 0);
    }
    #[test]
    fn dl9_empty_index_scratch_recovers() {
        interrupted_index_recovers("empty-index", b"");
    }
    #[test]
    fn dl9_truncated_index_scratch_recovers() {
        interrupted_index_recovers("truncated-index", br#"{"version":1,"game":{"id":"fixture""#);
    }
    #[test]
    fn dl9_small_payload_fingerprint_hashes_every_byte() {
        let f = Fixture::new("small-sample", false);
        let cache = f.cache();
        let path = f.root.join(&f.game.targets().unwrap()[1]);
        let contents = (0..196608).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        fs::write(&path, &contents).unwrap();
        let file = regular(&cache.fs, &path, false).unwrap();
        assert_eq!(
            sampled_fingerprint(&file, contents.len() as u64, &Cancellation::default()).unwrap(),
            digest_hex(Sha256::digest(&contents).as_slice())
        );
    }
    #[test]
    fn dl9_large_fingerprint_matches_all_sample_regions() {
        let f = sampled_payload_fixture("sample-layout");
        let cache = f.cache();
        let path = f.root.join(&f.game.targets().unwrap()[1]);
        let contents = (0..1024 * 1024)
            .map(|i| ((i / 4096 + i % 251) % 256) as u8)
            .collect::<Vec<_>>();
        fs::write(&path, &contents).unwrap();
        let file = regular(&cache.fs, &path, false).unwrap();
        let mut expected = Sha256::new();
        expected.update(&contents[..65536]);
        expected.update(&contents[contents.len() - 65536..]);
        for i in 1..=16 {
            let offset = 65536 + (contents.len() - 2 * 65536 - 4096) * i / 17;
            expected.update(&contents[offset..offset + 4096]);
        }
        assert_eq!(
            sampled_fingerprint(&file, contents.len() as u64, &Cancellation::default()).unwrap(),
            digest_hex(expected.finalize().as_slice())
        );
    }
    #[test]
    fn dl9_metadata_without_fingerprint_upgrades_once() {
        let f = Fixture::new("legacy-sample", false);
        let cache = f.cache();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        let index = cache.index(&f.game);
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
        for metadata in record["metadata"].as_array_mut().unwrap() {
            metadata.as_object_mut().unwrap().remove("sampled_sha256");
        }
        fs::write(&index, serde_json::to_vec(&record).unwrap()).unwrap();
        let cancel = Cancellation::default();
        run_with_cancel(&f, &cache, &cancel).unwrap();
        assert!(cancel.verification_reads() > 0);
        let cancel = Cancellation::default();
        run_with_cancel(&f, &cache, &cancel).unwrap();
        assert_eq!(cancel.verification_reads(), 0);
    }
    #[test]
    fn dl9_fast_launch_needs_no_lock_lease_or_writes() {
        let f = Fixture::new("read-only-fast", false);
        let awake = f.root.join("stay_awake");
        let cache =
            Cache::new(CacheConfig::bind(f.root.clone(), Some(awake.clone())).unwrap()).unwrap();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        let index = fs::read(cache.index(&f.game)).unwrap();
        let _lock = CacheLock::acquire(&cache.fs, &cache.state().join("active.lock")).unwrap();
        cache.fs.test_space(0, 4096);
        run_with_cancel(&f, &cache, &Cancellation::default()).unwrap();
        assert!(!awake.exists());
        assert_eq!(fs::read(cache.index(&f.game)).unwrap(), index);
    }
    #[test]
    fn dl9_unknown_scratch_identity_and_unrelated_file_are_preserved() {
        let f = Fixture::new("unknown-scratch", false);
        let cache = f.cache();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        let index = cache.index(&f.game);
        let before = fs::read(&index).unwrap();
        let mut unknown: serde_json::Value = serde_json::from_slice(&before).unwrap();
        unknown["game"]["id"] = "unrelated".into();
        let unknown = serde_json::to_vec(&unknown).unwrap();
        let scratch = index.with_extension("new");
        fs::write(&scratch, &unknown).unwrap();
        let unrelated = index.parent().unwrap().join("unrelated.new");
        fs::write(&unrelated, b"private").unwrap();
        assert!(run_with_cancel(&f, &cache, &Cancellation::default()).is_err());
        assert_eq!(fs::read(&scratch).unwrap(), unknown);
        assert_eq!(fs::read(&index).unwrap(), before);
        fs::write(&scratch, b"").unwrap();
        run_with_cancel(&f, &cache, &Cancellation::default()).unwrap();
        assert_eq!(fs::read(unrelated).unwrap(), b"private");
    }
    #[test]
    fn dl9_invalid_scratch_without_committed_index_is_preserved() {
        let f = Fixture::new("uncommitted-scratch", false);
        let cache = f.cache();
        let scratch = cache.index(&f.game).with_extension("new");
        fs::write(&scratch, b"{").unwrap();
        assert!(run_with_cancel(&f, &cache, &Cancellation::default()).is_err());
        assert_eq!(fs::read(scratch).unwrap(), b"{");
        assert!(!cache.index(&f.game).exists());
    }
    #[test]
    fn dl9_failed_scratch_verification_preserves_committed_record_and_scratch() {
        let f = Fixture::new("bad-scratch-payload", false);
        let cache = f.cache();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        let index = cache.index(&f.game);
        let before = fs::read(&index).unwrap();
        let scratch = index.with_extension("new");
        fs::write(&scratch, b"{").unwrap();
        fs::write(f.root.join(&f.game.targets().unwrap()[1]), b"evil").unwrap();
        assert!(run_with_cancel(&f, &cache, &Cancellation::default()).is_err());
        assert_eq!(fs::read(scratch).unwrap(), b"{");
        assert_eq!(fs::read(index).unwrap(), before);
    }
    #[test]
    fn dl9_cancelled_scratch_verification_preserves_scratch() {
        let f = Fixture::new("cancel-scratch", false);
        let cache = f.cache();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        let scratch = cache.index(&f.game).with_extension("new");
        fs::write(&scratch, b"{").unwrap();
        let cancel = Cancellation::default();
        cancel.cancel_after_verification_reads(0);
        assert!(run_with_cancel(&f, &cache, &cancel).is_err());
        assert_eq!(fs::read(scratch).unwrap(), b"{");
    }
    #[test]
    fn dl9_replaced_interrupted_scratch_is_preserved() {
        let f = Fixture::new("raced-scratch", false);
        let cache = f.cache();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        let scratch = cache.index(&f.game).with_extension("new");
        fs::write(&scratch, b"{").unwrap();
        let raced = scratch.clone();
        *cache.verification_hook.lock().unwrap() = Some(Box::new(move || {
            let replacement = raced.with_extension("replacement");
            fs::write(&replacement, b"user").unwrap();
            fs::rename(replacement, raced).unwrap();
        }));
        assert!(run_with_cancel(&f, &cache, &Cancellation::default()).is_err());
        assert_eq!(fs::read(scratch).unwrap(), b"user");
    }
    #[test]
    fn dl8_ready_does_not_full_hash_payload() {
        let f = Fixture::new("fast", false);
        let cache = f.cache();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        let cancel = Cancellation::default();
        let (tx, _rx) = mpsc::sync_channel(32);
        cache
            .run(
                &f.game,
                &Endpoint::lan("127.0.0.1:9".parse().unwrap()).unwrap(),
                &cancel,
                &FilesystemSpace,
                &tx,
            )
            .unwrap();
        cache.ready(&f.game, &cancel).unwrap();
        assert_eq!(cancel.verification_reads(), 0);
    }
    #[test]
    fn dl8_missing_members_recover_without_touching_existing() {
        for playlist in [false, true] {
            let f = Fixture::new(
                if playlist {
                    "recover-m3u"
                } else {
                    "recover-cue"
                },
                playlist,
            );
            let cache = f.cache();
            cache
                .adopt_existing(&f.game, &Cancellation::default())
                .unwrap();
            let existing = f.root.join(&f.game.targets().unwrap()[0]);
            let before = fs::metadata(&existing).unwrap();
            fs::remove_file(f.root.join(&f.game.targets().unwrap()[2])).unwrap();
            f.stage_missing(&cache);
            f.run(&cache).unwrap();
            let after = fs::metadata(&existing).unwrap();
            assert_eq!(
                (before.ino(), before.mtime(), before.mtime_nsec()),
                (after.ino(), after.mtime(), after.mtime_nsec())
            );
            assert_eq!(fs::read(existing).unwrap(), b"data");
            cache
                .verify_files(&f.game, true, &Cancellation::default())
                .unwrap();
        }
    }

    #[test]
    fn dl8_transfer_and_admission_boundaries() {
        let f = Fixture::new("reserve", false);
        let cache = f.cache();
        struct Fixed(u64);
        impl SpaceProbe for Fixed {
            fn observe(&self, _: &AnchoredFs) -> Result<Space> {
                Ok(Space {
                    free_bytes: self.0,
                    allocation_unit: 4096,
                })
            }
        }
        let floor = 2_162_688 * 1024;
        for free in [floor - 1, floor, floor + METADATA_MARGIN - 1] {
            assert!(cache.check_reserve(&Fixed(free)).is_err());
        }
        assert!(cache.check_reserve(&Fixed(floor + METADATA_MARGIN)).is_ok());
        let cost = METADATA_MARGIN + 4096 + WINDOW as u64;
        for free in [floor - 1, floor, floor + cost - 1] {
            assert!(
                admit_transfer(
                    Space {
                        free_bytes: free,
                        allocation_unit: 4096
                    },
                    WINDOW as u64
                )
                .is_err()
            );
        }
        assert!(
            admit_transfer(
                Space {
                    free_bytes: floor + cost,
                    allocation_unit: 4096
                },
                WINDOW as u64
            )
            .is_ok()
        );
        let plan = plan_space(&f.game, 4096, &[], u64::MAX).unwrap();
        assert_eq!(plan.required_free_bytes, floor + METADATA_MARGIN + 3 * 4096);
        assert_eq!(
            plan_space(&f.game, 4096, &[], plan.required_free_bytes - 1)
                .unwrap()
                .shortage_bytes,
            1
        );
        assert_eq!(
            plan_space(&f.game, 4096, &[], plan.required_free_bytes)
                .unwrap()
                .shortage_bytes,
            0
        );
    }
    #[test]
    fn dl8_metadata_and_anchored_writer_boundaries() {
        let f = Fixture::new("writers", false);
        let cache = f.cache();
        let fs = &cache.fs;
        let floor = 2_162_688 * 1024;
        let cost = METADATA_MARGIN + 4096;
        for free in [floor - 1, floor, floor + cost - 1] {
            fs.test_space(free, 4096);
            assert!(fs.create(&f.root.join("new.bin")).is_err());
            assert!(fs.mkdirs(&f.root.join("newdir")).is_err());
        }
        fs.test_space(floor + cost, 4096);
        fs.mkdirs(&f.root.join("newdir")).unwrap();
        let mut file = fs.create(&f.root.join("new.bin")).unwrap();
        for free in [floor - 1, floor, floor + cost + 4096 - 1] {
            fs.test_space(free, 4096);
            assert!(file.write_all(b"x").is_err());
            assert!(file.set_len(1).is_err());
        }
        fs.test_space(floor + cost + 4096, 4096);
        file.write_all(b"x").unwrap();
        file.set_len(2).unwrap();
        fs.test_space(floor + cost - 1, 4096);
        assert!(
            fs.rename_no_replace(&f.root.join("new.bin"), &f.root.join("renamed.bin"))
                .is_err()
        );
        fs.test_space(floor + cost, 4096);
        fs.rename_no_replace(&f.root.join("new.bin"), &f.root.join("renamed.bin"))
            .unwrap();
        let record = ReadyRecord {
            version: 1,
            game: f.game.clone(),
            metadata: None,
        };
        let target = f
            .root
            .join(".allium/state/network-library/installed/writer.json");
        fs.test_space(floor + cost - 1, 4096);
        assert!(save_json(fs, &target, &record).is_err());
        assert!(!target.with_extension("new").exists());
        fs.test_space(floor + cost + 4096, 4096);
        save_json(fs, &target, &record).unwrap();
    }
    #[test]
    fn dl8_metadata_mismatch_and_legacy_upgrade() {
        let f = Fixture::new("legacy", false);
        let cache = f.cache();
        let cancel = Cancellation::default();
        cache.adopt_existing(&f.game, &cancel).unwrap();
        let path = cache.index(&f.game);
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        record.as_object_mut().unwrap().remove("metadata");
        fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        let cancel = Cancellation::default();
        cache.ready(&f.game, &cancel).unwrap();
        assert!(cancel.verification_reads() > 0);
        assert!(
            load_json::<ReadyRecord>(&cache.fs, &path)
                .unwrap()
                .metadata
                .is_some()
        );
        let before = fs::read(&path).unwrap();
        let cancel = Cancellation::default();
        f.run(&cache).unwrap();
        cache.ready(&f.game, &cancel).unwrap();
        assert_eq!(cancel.verification_reads(), 0);
        assert_eq!(fs::read(&path).unwrap(), before);
        let payload = f.root.join(&f.game.targets().unwrap()[1]);
        let file = std::fs::File::options().write(true).open(&payload).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1234)),
        )
        .unwrap();
        let cancel = Cancellation::default();
        cache.ready(&f.game, &cancel).unwrap();
        assert!(cancel.verification_reads() > 0);
        assert_ne!(fs::read(&path).unwrap(), before);
        fs::write(&payload, b"changed").unwrap();
        let cancel = Cancellation::default();
        assert!(cache.ready(&f.game, &cancel).is_err());
        fs::write(&payload, b"evil").unwrap();
        let cancel = Cancellation::default();
        assert!(cache.ready(&f.game, &cancel).is_err());
        assert!(cancel.verification_reads() > 0);
    }
    #[test]
    fn dl8_changed_verified_payload_cannot_record_metadata() {
        let f = Fixture::new("changed-publication", false);
        let cache = f.cache();
        let handles = cache
            .verify_files(&f.game, true, &Cancellation::default())
            .unwrap();
        let path = f.root.join(&f.game.targets().unwrap()[1]);
        fs::write(&path, b"evil").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1234)),
            )
            .unwrap();
        let lock = CacheLock::acquire(&cache.fs, &cache.state().join("active.lock")).unwrap();
        assert!(cache.publish_ready(&f.game, &handles, &lock).is_err());
        assert!(!cache.index(&f.game).exists());
    }
    #[test]
    fn dl8_conflicting_member_preserved() {
        for directory in [false, true] {
            let f = Fixture::new(if directory { "wrongtype" } else { "wrongsize" }, false);
            let cache = f.cache();
            cache
                .adopt_existing(&f.game, &Cancellation::default())
                .unwrap();
            let path = f.root.join(&f.game.targets().unwrap()[1]);
            fs::remove_file(&path).unwrap();
            if directory {
                fs::create_dir(&path).unwrap();
            } else {
                fs::write(&path, b"user").unwrap();
                fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(b"data")
                    .unwrap();
            }
            fs::remove_file(f.root.join(&f.game.targets().unwrap()[2])).unwrap();
            let error = f.run(&cache).unwrap_err();
            assert!(error.to_string().contains("Conflicting"), "{error}");
            if directory {
                assert!(path.is_dir());
            } else {
                assert_eq!(fs::read(path).unwrap(), b"userdata");
            }
        }
    }
    #[test]
    fn dl8_recovery_repeated_missing_and_no_replace() {
        let f = Fixture::new("repeat", true);
        let cache = f.cache();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        let target = f.root.join(&f.game.targets().unwrap()[2]);
        for _ in 0..2 {
            fs::remove_file(&target).unwrap();
            f.stage_missing(&cache);
            f.run(&cache).unwrap();
        }
        fs::remove_file(&target).unwrap();
        f.stage_missing(&cache);
        let raced = target.clone();
        cache.test_after_stage_verification(move || {
            fs::write(&raced, b"user").unwrap();
        });
        assert!(f.run(&cache).is_err());
        assert_eq!(fs::read(target).unwrap(), b"user");
        assert_eq!(fs::read(cache.payload(&f.game, 2)).unwrap(), b"data");
    }

    #[test]
    fn dl8_interrupted_member_publication_recovers() {
        let f = Fixture::new("interrupted", false);
        let cache = f.cache();
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        let targets = f.game.targets().unwrap();
        for index in [1, 2] {
            fs::remove_file(f.root.join(&targets[index])).unwrap();
        }
        f.stage_missing(&cache);
        fs::write(cache.payload(&f.game, 1), b"data").unwrap();
        let job = cache.job(&f.game).join("job.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&job).unwrap()).unwrap();
        value["partials"].as_array_mut().unwrap().push(
            serde_json::json!({"id":"file1","bytes":4,"prefix_sha256":f.game.files[1].sha256}),
        );
        fs::write(&job, serde_json::to_vec(&value).unwrap()).unwrap();
        let raced = f.root.join(&targets[2]);
        cache.test_after_stage_verification(move || {
            fs::write(raced, b"userdata").unwrap();
        });
        assert!(f.run(&cache).is_err());
        assert_eq!(fs::read(f.root.join(&targets[1])).unwrap(), b"data");
        let preserved = fs::metadata(f.root.join(&targets[1])).unwrap().ino();
        assert_eq!(fs::read(f.root.join(&targets[2])).unwrap(), b"userdata");
        assert_eq!(
            load_json::<Journal>(&cache.fs, &job).unwrap().phase,
            "installing"
        );
        fs::remove_file(f.root.join(&targets[2])).unwrap();
        f.run(&cache).unwrap();
        assert_eq!(
            fs::metadata(f.root.join(&targets[1])).unwrap().ino(),
            preserved
        );
        cache
            .verify_files(&f.game, true, &Cancellation::default())
            .unwrap();
    }
    #[test]
    #[ignore]
    fn dl9_host_cached_launch_683_mib() {
        let mut f = Fixture::new("timing", false);
        let cache = f.cache();
        let path = f.root.join("Roms/PS/Timing.chd");
        fs::File::create(&path)
            .unwrap()
            .set_len(683 * 1024 * 1024)
            .unwrap();
        f.game.install_kind = "file".into();
        f.game.game_path = "Roms/PS/Timing.chd".into();
        f.game.launch_path = f.game.game_path.clone();
        f.game.cache_path = f.game.game_path.clone();
        f.game.files.truncate(1);
        f.game.files[0].relative_path.clear();
        f.game.files[0].role = "rom".into();
        f.game.discs.clear();
        f.game.files[0].bytes = 683 * 1024 * 1024;
        f.game.files[0].sha256 = hash_reader(
            &mut fs::File::open(&path).unwrap(),
            &Cancellation::default(),
        )
        .unwrap()
        .1;
        cache
            .adopt_existing(&f.game, &Cancellation::default())
            .unwrap();
        let cancel = Cancellation::default();
        let start = std::time::Instant::now();
        cache.verify_files(&f.game, true, &cancel).unwrap();
        let before = start.elapsed();
        let cancel = Cancellation::default();
        let (tx, _rx) = mpsc::sync_channel(32);
        let start = std::time::Instant::now();
        cache
            .run(
                &f.game,
                &Endpoint::lan("127.0.0.1:9".parse().unwrap()).unwrap(),
                &cancel,
                &FilesystemSpace,
                &tx,
            )
            .unwrap();
        let after = start.elapsed();
        assert_eq!(cancel.verification_reads(), 0);
        println!(
            "DL9_HOST_TIMING {{\"bytes\":{},\"full_verify_ms\":{},\"cached_run_ms\":{},\"sample_bytes\":{}}}",
            f.game.files[0].bytes,
            before.as_secs_f64() * 1000.,
            after.as_secs_f64() * 1000.,
            SAMPLE_TOTAL
        );
    }
    #[test]
    fn dl8_floor_is_real() {
        assert_eq!(RESERVE_BYTES, 2_162_688 * 1024);
    }
}
