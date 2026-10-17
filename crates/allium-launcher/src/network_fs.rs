//! Cache filesystem operations rooted in held, identity-bound directory descriptors.
#![allow(dead_code)]
use crate::network_library::{METADATA_MARGIN, RESERVE_BYTES};
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    ffi::{CString, OsStr, OsString},
    fs::{File, Metadata},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
        io::{AsRawFd, FromRawFd},
    },
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
// Count the calling thread's probes across cache bindings without including
// asynchronous inventory work performed on other threads.
#[cfg(test)]
thread_local! {
    static EXISTS_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
pub(crate) fn test_exists_probes() -> usize {
    EXISTS_PROBES.with(std::cell::Cell::get)
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Identity {
    pub device: u64,
    pub inode: u64,
}
impl Identity {
    fn metadata(m: &Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
        }
    }
}
fn name(value: &OsStr) -> Result<CString> {
    Ok(CString::new(value.as_bytes())?)
}
fn file_from_result(fd: i32) -> Result<File> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn directory_at(parent: &File, child: &OsStr) -> Result<File> {
    file_from_result(unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name(child)?.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
}
fn stat_at(parent: &File, child: &OsStr) -> Result<Option<libc::stat>> {
    let mut result = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name(child)?.as_ptr(),
            result.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok(Some(unsafe { result.assume_init() }))
}
fn stat_identity(m: &libc::stat) -> Identity {
    Identity {
        device: m.st_dev as u64,
        inode: m.st_ino as u64,
    }
}
fn named_identity(parent: &File, child: &OsStr, expected: Identity) -> Result<()> {
    let metadata = stat_at(parent, child)?
        .ok_or_else(|| anyhow::anyhow!("Named filesystem identity missing"))?;
    ensure!(
        stat_identity(&metadata) == expected && metadata.st_mode & libc::S_IFMT != libc::S_IFLNK,
        "Named filesystem identity changed"
    );
    Ok(())
}
struct RootEdge {
    parent: Arc<File>,
    name: OsString,
    identity: Identity,
}
struct Inner {
    invalid: AtomicBool,
    root_path: PathBuf,
    reserve_device: Option<u64>,
    root: Arc<File>,
    absolute_edges: Vec<RootEdge>,
    directories: Mutex<HashMap<PathBuf, Arc<File>>>,
    #[cfg(test)]
    space: Mutex<Option<(u64, u64)>>,
    #[cfg(test)]
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    metadata_operations: Mutex<Vec<(&'static str, PathBuf)>>,
    #[cfg(test)]
    payload_read_bytes: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    after_file_sync: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
#[derive(Clone)]
pub struct AnchoredFs(Arc<Inner>);
impl AnchoredFs {
    pub fn new(root: &Path) -> Result<Self> {
        Self::new_bound(root, None)
    }
    pub(crate) fn new_lease(root: &Path, reserve_device: u64) -> Result<Self> {
        Self::new_bound(root, Some(reserve_device))
    }
    fn new_bound(root: &Path, reserve_device: Option<u64>) -> Result<Self> {
        ensure!(root.is_absolute(), "Root must be absolute");
        let mut parent = Arc::new(File::open("/")?);
        let mut edges = Vec::new();
        for component in root.components().skip(1) {
            ensure!(
                matches!(component, Component::Normal(_)),
                "Unsafe root component"
            );
            let child = component.as_os_str();
            let opened = Arc::new(directory_at(&parent, child)?);
            let identity = Identity::metadata(&opened.metadata()?);
            named_identity(&parent, child, identity)?;
            edges.push(RootEdge {
                parent: parent.clone(),
                name: child.to_owned(),
                identity,
            });
            parent = opened;
        }
        let root_descriptor = parent;
        let mut directories = HashMap::new();
        directories.insert(PathBuf::new(), root_descriptor.clone());
        Ok(Self(Arc::new(Inner {
            invalid: AtomicBool::new(false),
            root_path: root.to_owned(),
            reserve_device,
            root: root_descriptor,
            absolute_edges: edges,
            directories: Mutex::new(directories),
            #[cfg(test)]
            space: Mutex::new(None),
            #[cfg(test)]
            hook: Mutex::new(None),
            #[cfg(test)]
            metadata_operations: Mutex::new(Vec::new()),
            #[cfg(test)]
            payload_read_bytes: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            after_file_sync: Mutex::new(None),
        })))
    }
    #[cfg(test)]
    pub fn test_payload_read_bytes(&self) -> u64 {
        self.0.payload_read_bytes.load(Ordering::Relaxed)
    }
    pub fn root_path(&self) -> &Path {
        &self.0.root_path
    }
    pub fn root_identity(&self) -> Result<Identity> {
        Ok(Identity::metadata(&self.0.root.metadata()?))
    }
    pub fn admit_write(&self, incoming: u64) -> Result<()> {
        self.check_root()?;
        // A stay-awake lease on tmpfs does not consume SD capacity.
        if self
            .0
            .reserve_device
            .is_some_and(|device| self.root_identity().is_ok_and(|id| id.device != device))
        {
            return Ok(());
        }
        #[cfg(test)]
        if let Some((free, unit)) = *self.0.space.lock().unwrap() {
            return admit_allocation(free, unit, incoming);
        }
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        ensure!(
            unsafe { libc::fstatvfs(self.root_fd(), stat.as_mut_ptr()) } == 0,
            "Free-space check failed: {}",
            std::io::Error::last_os_error()
        );
        let stat = unsafe { stat.assume_init() };
        let unit = stat.f_frsize as u64;
        let free = (stat.f_bavail as u64)
            .checked_mul(unit)
            .ok_or_else(|| anyhow::anyhow!("Free-space overflow"))?;
        admit_allocation(free, unit, incoming)
    }
    #[cfg(test)]
    pub(crate) fn test_space(&self, free: u64, unit: u64) {
        *self.0.space.lock().unwrap() = Some((free, unit));
    }
    pub fn root_fd(&self) -> i32 {
        self.0.root.as_raw_fd()
    }
    fn latch_failure<T>(&self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.0.invalid.store(true, Ordering::Release);
        }
        result
    }
    pub fn check_root(&self) -> Result<()> {
        ensure!(
            !self.0.invalid.load(Ordering::Acquire),
            "Filesystem identity invalidated; explicit root recovery required"
        );
        for edge in &self.0.absolute_edges {
            self.latch_failure(named_identity(&edge.parent, &edge.name, edge.identity))?;
        }
        Ok(())
    }
    pub fn check_bound_directories(&self) -> Result<()> {
        let paths = self
            .0
            .directories
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for relative in paths {
            self.walk(&self.0.root_path.join(relative), false)?;
        }
        self.check_root()
    }
    fn relative(&self, path: &Path) -> Result<PathBuf> {
        let relative = path.strip_prefix(&self.0.root_path)?;
        ensure!(
            relative
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
            "Unsafe relative component"
        );
        Ok(relative.to_owned())
    }
    fn before_operation(&self) {
        #[cfg(test)]
        {
            let hook = self.0.hook.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
    }
    #[cfg(test)]
    pub fn test_before_next_operation(&self, hook: impl FnOnce() + Send + 'static) {
        *self.0.hook.lock().unwrap() = Some(Box::new(hook));
    }
    #[cfg(test)]
    pub(crate) fn test_metadata_operations(&self) -> Vec<(&'static str, PathBuf)> {
        self.0.metadata_operations.lock().unwrap().clone()
    }
    #[cfg(test)]
    pub(crate) fn test_after_file_sync(&self, hook: impl FnOnce() + Send + 'static) {
        *self.0.after_file_sync.lock().unwrap() = Some(Box::new(hook));
    }
    fn walk(&self, path: &Path, create: bool) -> Result<Arc<File>> {
        self.check_root()?;
        let relative = self.relative(path)?;
        let mut prefix = PathBuf::new();
        let mut parent = self.0.root.clone();
        for component in relative.components() {
            let child = component.as_os_str();
            prefix.push(child);
            let cached = self.0.directories.lock().unwrap().get(&prefix).cloned();
            if let Some(opened) = cached {
                self.latch_failure(named_identity(
                    &parent,
                    child,
                    Identity::metadata(&opened.metadata()?),
                ))?;
                parent = opened;
                continue;
            }
            ensure!(
                self.0.directories.lock().unwrap().len() < 256,
                "Pinned directory bound exceeded"
            );
            if create && stat_at(&parent, child)?.is_none() {
                self.before_operation();
                self.admit_write(0)?;
                let result =
                    unsafe { libc::mkdirat(parent.as_raw_fd(), name(child)?.as_ptr(), 0o700) };
                if result < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                }
                parent.sync_all()?;
            }
            let opened = Arc::new(directory_at(&parent, child)?);
            let identity = Identity::metadata(&opened.metadata()?);
            named_identity(&parent, child, identity)?;
            self.0
                .directories
                .lock()
                .unwrap()
                .insert(prefix.clone(), opened.clone());
            parent = opened;
        }
        self.check_root()?;
        Ok(parent)
    }
    fn parent(&self, path: &Path) -> Result<(Arc<File>, OsString)> {
        self.relative(path)?;
        let child = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("Missing leaf"))?;
        Ok((self.walk(path.parent().unwrap(), false)?, child.to_owned()))
    }
    pub fn pin_existing(&self, path: &Path) -> Result<()> {
        match self.walk(path, false) {
            Ok(_) => Ok(()),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
    pub fn mkdirs(&self, path: &Path) -> Result<()> {
        self.walk(path, true)?;
        self.check(path)
    }
    pub fn check(&self, path: &Path) -> Result<()> {
        self.check_root()?;
        if path == self.root_path() {
            return Ok(());
        }
        let (parent, child) = self.parent(path)?;
        if let Some(metadata) = stat_at(&parent, &child)? {
            ensure!(
                metadata.st_mode & libc::S_IFMT != libc::S_IFLNK,
                "Symlink leaf"
            );
        }
        Ok(())
    }
    pub fn exists(&self, path: &Path) -> Result<bool> {
        #[cfg(test)]
        EXISTS_PROBES.with(|count| count.set(count.get() + 1));
        match self.parent(path) {
            Ok((parent, child)) => {
                let m = stat_at(&parent, &child)?;
                if let Some(m) = &m {
                    ensure!(m.st_mode & libc::S_IFMT != libc::S_IFLNK, "Symlink leaf");
                }
                Ok(m.is_some())
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
    fn open_flags(&self, path: &Path, flags: i32) -> Result<AnchoredFile> {
        let (parent, child) = self.parent(path)?;
        self.before_operation();
        if flags & libc::O_CREAT != 0 {
            self.admit_write(0)?;
        }
        let file = file_from_result(unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name(&child)?.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                0o600,
            )
        })?;
        ensure!(file.metadata()?.is_file(), "Expected regular file");
        let identity = Identity::metadata(&file.metadata()?);
        let result = AnchoredFile {
            fs: self.clone(),
            path: path.to_owned(),
            parent,
            child,
            identity,
            file,
            created: flags & libc::O_EXCL != 0,
        };
        if let Err(error) = result.validate() {
            if result.created {
                let _ = result.remove_owned();
            }
            return Err(error);
        }
        Ok(result)
    }
    pub fn open(&self, path: &Path, writable: bool) -> Result<AnchoredFile> {
        self.open_flags(
            path,
            if writable {
                libc::O_RDWR
            } else {
                libc::O_RDONLY
            },
        )
    }
    pub fn create(&self, path: &Path) -> Result<AnchoredFile> {
        self.open_flags(path, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)
    }
    pub fn sync_directory(&self, path: &Path) -> Result<()> {
        let file = self.walk(path, false)?;
        file.sync_all()?;
        self.check_root()
    }
    pub fn entries(&self, path: &Path) -> Result<Vec<(PathBuf, bool)>> {
        let directory = self.walk(path, false)?;
        let duplicate = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        ensure!(duplicate >= 0, "dup directory failed");
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            unsafe { libc::close(duplicate) };
            return Err(std::io::Error::last_os_error().into());
        }
        struct DirectoryStream(*mut libc::DIR);
        impl Drop for DirectoryStream {
            fn drop(&mut self) {
                unsafe { libc::closedir(self.0) };
            }
        }
        let _stream = DirectoryStream(stream);
        let mut result = Vec::new();
        loop {
            #[cfg(target_os = "macos")]
            let errno = unsafe { libc::__error() };
            #[cfg(target_os = "linux")]
            let errno = unsafe { libc::__errno_location() };
            unsafe {
                *errno = 0;
            }
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                if unsafe { *errno } != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                break;
            }
            let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            ensure!(result.len() < 1024, "Directory entry bound exceeded");
            let child = OsString::from_vec(bytes.to_vec());
            let metadata =
                stat_at(&directory, &child)?.ok_or_else(|| anyhow::anyhow!("Entry disappeared"))?;
            let kind = metadata.st_mode & libc::S_IFMT;
            ensure!(
                kind == libc::S_IFDIR || kind == libc::S_IFREG,
                "Unexpected non-regular entry"
            );
            result.push((path.join(child), kind == libc::S_IFDIR));
        }
        self.check(path)?;
        Ok(result)
    }
    fn rebind_move(&self, source: &Path, target: &Path) -> Result<()> {
        let from = self.relative(source)?;
        let to = self.relative(target)?;
        let mut cache = self.0.directories.lock().unwrap();
        let moves = cache
            .iter()
            .filter(|(p, _)| p.starts_with(&from))
            .map(|(p, f)| {
                (
                    p.clone(),
                    to.join(p.strip_prefix(&from).unwrap()),
                    f.clone(),
                )
            })
            .collect::<Vec<_>>();
        for (old, new, file) in moves {
            cache.remove(&old);
            cache.insert(new, file);
        }
        Ok(())
    }
    pub fn pin_source(&self, source: &Path) -> Result<PinnedSource> {
        let (parent, child) = self.parent(source)?;
        let metadata =
            stat_at(&parent, &child)?.ok_or_else(|| anyhow::anyhow!("Source missing"))?;
        let kind = metadata.st_mode & libc::S_IFMT;
        ensure!(
            kind == libc::S_IFDIR || kind == libc::S_IFREG,
            "Unsafe source"
        );
        let held = if kind == libc::S_IFDIR {
            self.walk(source, false)?
        } else {
            Arc::new(self.open(source, false)?.file.try_clone()?)
        };
        let identity = Identity::metadata(&held.metadata()?);
        named_identity(&parent, &child, identity)?;
        Ok(PinnedSource {
            fs: self.clone(),
            path: source.to_owned(),
            parent,
            name: child,
            identity,
            _held: held,
        })
    }
    pub fn unlink_registered(&self, file: &AnchoredFile) -> Result<()> {
        ensure!(Arc::ptr_eq(&self.0, &file.fs.0), "Foreign file binding");
        file.validate()?;
        if unsafe { libc::unlinkat(file.parent.as_raw_fd(), name(&file.child)?.as_ptr(), 0) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        file.parent.sync_all()?;
        Ok(())
    }
    pub fn remove_empty_directory(&self, path: &Path, expected: Identity) -> Result<()> {
        let pinned = self.pin_source(path)?;
        ensure!(pinned.identity == expected, "Replaced unload directory");
        ensure!(pinned._held.metadata()?.is_dir(), "Expected directory");
        pinned.validate()?;
        if unsafe {
            libc::unlinkat(
                pinned.parent.as_raw_fd(),
                name(&pinned.name)?.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        pinned.parent.sync_all()?;
        let relative = self.relative(path)?;
        self.0
            .directories
            .lock()
            .unwrap()
            .retain(|p, _| !p.starts_with(&relative));
        Ok(())
    }
    pub fn rename_no_replace(&self, source: &Path, target: &Path) -> Result<()> {
        let pinned = self.pin_source(source)?;
        self.rename_verified(&pinned, target)
    }
    pub fn rename_verified(&self, pinned: &PinnedSource, target: &Path) -> Result<()> {
        ensure!(Arc::ptr_eq(&self.0, &pinned.fs.0), "Foreign source binding");
        pinned.validate()?;
        let source = &pinned.path;
        let from = pinned.parent.clone();
        let name_from = &pinned.name;
        let (to, name_to) = self.parent(target)?;
        let a = name(name_from)?;
        let b = name(&name_to)?;
        self.before_operation();
        pinned.validate()?;
        self.admit_write(0)?;
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                from.as_raw_fd(),
                a.as_ptr(),
                to.as_raw_fd(),
                b.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                from.as_raw_fd(),
                a.as_ptr(),
                to.as_raw_fd(),
                b.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        from.sync_all()?;
        to.sync_all()?;
        self.rebind_move(source, target)?;
        self.check(source.parent().unwrap())?;
        self.check(target)
    }
    /// Only generated private metadata may replace an identity-checked committed record.
    pub fn replace_metadata(
        &self,
        source: &Path,
        target: &Path,
        expected: Option<Identity>,
    ) -> Result<()> {
        self.replace_metadata_bound(source, target, expected, None)
    }
    pub fn replace_metadata_file(
        &self,
        source: &AnchoredFile,
        target: &Path,
        expected: Option<Identity>,
    ) -> Result<()> {
        ensure!(Arc::ptr_eq(&self.0, &source.fs.0), "Foreign file binding");
        source.validate()?;
        self.replace_metadata_bound(&source.path, target, expected, Some(source))
    }
    fn replace_metadata_bound(
        &self,
        source: &Path,
        target: &Path,
        expected: Option<Identity>,
        held: Option<&AnchoredFile>,
    ) -> Result<()> {
        ensure!(
            self.relative(target)?
                .starts_with(".allium/state/network-library")
                && target.extension().is_some_and(|e| e == "json")
                && source == target.with_extension("new"),
            "Metadata-only replacement policy"
        );
        let (from, a) = self.parent(source)?;
        let (to, b) = self.parent(target)?;
        if let Some(expected) = expected {
            named_identity(&to, &b, expected)?;
        } else {
            if let Some(held) = held {
                let pinned = PinnedSource {
                    fs: self.clone(),
                    path: held.path.clone(),
                    parent: held.parent.clone(),
                    name: held.child.clone(),
                    identity: held.identity,
                    _held: Arc::new(held.file.try_clone()?),
                };
                return self.rename_verified(&pinned, target);
            }
            return self.rename_no_replace(source, target);
        }
        self.before_operation();
        self.admit_write(0)?;
        if let Some(held) = held {
            held.validate()?;
            named_identity(&to, &b, expected.unwrap())?;
        }
        #[cfg(test)]
        self.0
            .metadata_operations
            .lock()
            .unwrap()
            .push(("rename", source.to_owned()));
        if unsafe {
            libc::renameat(
                from.as_raw_fd(),
                name(&a)?.as_ptr(),
                to.as_raw_fd(),
                name(&b)?.as_ptr(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        from.sync_all()?;
        to.sync_all()?;
        self.check(target)
    }
}
pub struct PinnedSource {
    fs: AnchoredFs,
    path: PathBuf,
    parent: Arc<File>,
    name: OsString,
    identity: Identity,
    _held: Arc<File>,
}
impl PinnedSource {
    pub fn identity(&self) -> Identity {
        self.identity
    }
    pub fn validate(&self) -> Result<()> {
        self.fs.parent(&self.path)?;
        named_identity(&self.parent, &self.name, self.identity)
    }
}
pub struct AnchoredFile {
    created: bool,
    fs: AnchoredFs,
    path: PathBuf,
    parent: Arc<File>,
    child: OsString,
    identity: Identity,
    file: File,
}
impl AnchoredFile {
    pub fn validate(&self) -> Result<()> {
        self.fs.parent(&self.path)?;
        named_identity(&self.parent, &self.child, self.identity)
    }
    pub fn identity(&self) -> Identity {
        self.identity
    }
    pub fn metadata(&self) -> Result<Metadata> {
        Ok(self.file.metadata()?)
    }
    pub fn set_len(&self, len: u64) -> Result<()> {
        self.validate()?;
        self.fs.before_operation();
        if len > self.file.metadata()?.len() {
            self.fs.admit_write(len - self.file.metadata()?.len())?;
        }
        self.file.set_len(len)?;
        self.validate()
    }
    pub fn sync_all(&self) -> Result<()> {
        self.validate()?;
        self.file.sync_all()?;
        #[cfg(test)]
        {
            self.fs
                .0
                .metadata_operations
                .lock()
                .unwrap()
                .push(("sync", self.path.clone()));
            let hook = self.fs.0.after_file_sync.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        Ok(())
    }
    pub fn remove_owned(&self) -> Result<()> {
        ensure!(
            self.created,
            "Cleanup requires exclusive creation ownership"
        );
        named_identity(&self.parent, &self.child, self.identity)?;
        if unsafe { libc::unlinkat(self.parent.as_raw_fd(), name(&self.child)?.as_ptr(), 0) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        self.parent.sync_all()?;
        Ok(())
    }
}
impl Read for AnchoredFile {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.validate().map_err(std::io::Error::other)?;
        self.fs.before_operation();
        let read = self.file.read(buffer)?;
        #[cfg(test)]
        if self
            .path
            .extension()
            .is_some_and(|e| e == "bin" || e == "cue" || e == "m3u")
        {
            self.fs
                .0
                .payload_read_bytes
                .fetch_add(read as u64, Ordering::Relaxed);
        }
        Ok(read)
    }
}
impl Write for AnchoredFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.validate().map_err(std::io::Error::other)?;
        self.fs.before_operation();
        self.fs
            .admit_write(bytes.len() as u64)
            .map_err(std::io::Error::other)?;
        self.file.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
impl Seek for AnchoredFile {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.validate().map_err(std::io::Error::other)?;
        self.file.seek(position)
    }
}
impl AsRawFd for AnchoredFile {
    fn as_raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
}

fn admit_allocation(free: u64, unit: u64, incoming: u64) -> Result<()> {
    ensure!(unit > 0, "Zero allocation unit");
    let needed = incoming
        .checked_add(unit - 1)
        .map(|n| n / unit)
        .and_then(|n| n.checked_mul(unit))
        .and_then(|n| n.checked_add(unit))
        .and_then(|n| n.checked_add(METADATA_MARGIN))
        .and_then(|n| n.checked_add(RESERVE_BYTES))
        .ok_or_else(|| anyhow::anyhow!("Space admission overflow"))?;
    ensure!(free >= needed, "Storage floor would be crossed");
    Ok(())
}
