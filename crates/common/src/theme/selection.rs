//! Owned selection record; explicit selection never invokes legacy Stylesheet::save.
use super::fonts::ThemeHandle;
#[cfg(test)]
use super::fonts::FontBank;
use super::pack::{valid_digest, valid_id};
use super::runtime::{RootBinding, ThemeSnapshot, anchored_root, bounded_read, opened};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs::File,
    io::Write,
    os::unix::{fs::MetadataExt, io::AsRawFd},
    path::Path,
    sync::Mutex,
};

const NAME: &str = "ui-theme.json";
const RECORD_LIMIT: usize = 2048;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub schema_version: u32,
    pub id: String,
    pub revision: String,
    pub digest: String,
    pub generation: u64,
}
impl Selection {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && valid_id(&self.id)
                && valid_digest(&self.digest)
                && !self.revision.is_empty()
                && self.revision.len() <= 48
                && self
                    .revision
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_alphanumeric())
                && self
                    .revision
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                && self.generation > 0,
            "Invalid theme selection"
        );
        Ok(())
    }
    pub fn from_snapshot(s: &ThemeSnapshot, generation: u64) -> Self {
        Self {
            schema_version: 1,
            id: s.0.pack.id.clone(),
            revision: s.0.pack.revision.clone(),
            digest: s.0.digest.clone(),
            generation,
        }
    }
}
/// Bind one already-existing state directory. No directory creation or migration.
pub(super) type SelectionToken = (u64, u64, u64, i64, i64, i64, i64);

pub struct SelectionStore {
    path: std::path::PathBuf,
    root: File,
    binding: RootBinding,
    writer: Mutex<()>,
}
impl SelectionStore {
    pub fn bind(path: &Path) -> Result<Self> {
        let root = anchored_root(path)?;
        let binding = RootBinding::bind(path)?;
        binding.verify_held(&root)?;
        Ok(Self {
            binding,
            path: path.into(),
            root,
            writer: Mutex::new(()),
        })
    }
    fn check(&self) -> Result<()> {
        self.binding.check()?;
        Ok(())
    }
    pub(super) fn cache_token(&self) -> Result<Option<SelectionToken>> {
        self.binding.check()?;
        let named = std::fs::symlink_metadata(&self.path)?;
        let held = self.root.metadata()?;
        ensure!(
            named.is_dir() && (named.dev(), named.ino()) == (held.dev(), held.ino()),
            "Theme state binding changed"
        );
        match std::fs::symlink_metadata(self.path.join(NAME)) {
            Ok(m) => {
                ensure!(m.is_file(), "Invalid theme selection file");
                Ok(Some((
                    m.dev(),
                    m.ino(),
                    m.len(),
                    m.mtime(),
                    m.mtime_nsec(),
                    m.ctime(),
                    m.ctime_nsec(),
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn read(&self) -> Result<Option<Selection>> {
        self.check()?;
        match bounded_read(&self.root, NAME, RECORD_LIMIT) {
            Ok(bytes) => {
                let v: Selection = serde_json::from_slice(&bytes)?;
                v.validate()?;
                Ok(Some(v))
            }
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
    /// Explicit single-writer operation; verifies old content under advisory state-directory lock.
    /// External hostile replacement of this private generated metadata during commit is unsupported.
    /// Commit/fync error after rename is reported as unknown durability, never claimed old state kept.
    fn with_write_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let _writer = self
            .writer
            .try_lock()
            .map_err(|_| anyhow::anyhow!("Theme selection writer busy"))?;
        self.check()?;
        // Lock the held state directory: no extra lock-file or unrelated state writes.
        ensure!(
            unsafe { libc::flock(self.root.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "Theme selection writer busy"
        );
        struct Unlock<'a>(&'a File);
        impl Drop for Unlock<'_> {
            fn drop(&mut self) {
                unsafe {
                    libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
                }
            }
        }
        let _lock = Unlock(&self.root);
        operation()
    }
    pub fn select(&self, snapshot: &ThemeHandle) -> Result<Selection> {
        self.with_write_lock(|| self.select_locked(snapshot))
    }
    fn select_locked(&self, snapshot: &ThemeHandle) -> Result<Selection> {
        let old = self.read()?; // malformed/symlink/unknown bytes are preserved, not overwritten.
        let generation = old.as_ref().map_or(Ok(1), |v| {
            v.generation
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("Theme generation overflow"))
        })?;
        let next = Selection {
            schema_version: 1,
            id: snapshot.pack().id.clone(),
            revision: snapshot.pack().revision.clone(),
            digest: snapshot.digest().into(),
            generation,
        };
        next.validate()?;
        let bytes = serde_json::to_vec(&next)?;
        ensure!(bytes.len() <= RECORD_LIMIT, "Theme selection exceeds limit");
        let temp = CString::new(format!(".ui-theme.json.new-{}", std::process::id()))?;
        let fd = unsafe {
            libc::openat(
                self.root.as_raw_fd(),
                temp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        let mut file = opened(fd)?;
        let identity = file.metadata()?;
        let result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            self.check()?;
            ensure!(
                self.read()? == old,
                "Theme selection changed during preparation"
            );
            ensure!(
                bounded_identity(&self.root, &temp)? == (identity.dev(), identity.ino()),
                "Theme selection temporary identity changed"
            );
            let name = CString::new(NAME)?;
            // Renaming owned metadata is atomic; this is not conditional-inode rename.
            ensure!(
                unsafe {
                    libc::renameat(
                        self.root.as_raw_fd(),
                        temp.as_ptr(),
                        self.root.as_raw_fd(),
                        name.as_ptr(),
                    )
                } == 0,
                "Theme selection commit failed: {}",
                std::io::Error::last_os_error()
            );
            self.root.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            // Only clean our exact temp identity. A replacement or already-renamed leaf stays untouched.
            if let Ok(observed) = bounded_identity(&self.root, &temp) {
                if observed == (identity.dev(), identity.ino()) {
                    unsafe {
                        libc::unlinkat(self.root.as_raw_fd(), temp.as_ptr(), 0);
                    }
                }
            }
        }
        result?;
        Ok(next)
    }
    /// Remove only a validated, unchanged owned record under the same writer guard.
    /// The private metadata cooperative-leaf contract also applies to unlink.
    pub fn choose_legacy(&self) -> Result<bool> {
        self.with_write_lock(|| {
            if self.read()?.is_none() {
                return Ok(false);
            }
            let temp = CString::new(format!(".ui-theme.json.new-{}", std::process::id()))?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            let result = unsafe {
                libc::fstatat(
                    self.root.as_raw_fd(),
                    temp.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            ensure!(
                result < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound,
                "Unexpected theme selection temporary file"
            );
            let name = CString::new(NAME)?;
            let identity = bounded_identity(&self.root, &name)?;
            let bytes = bounded_read(&self.root, NAME, RECORD_LIMIT)?;
            let parsed: Selection = serde_json::from_slice(&bytes)?;
            parsed.validate()?;
            self.check()?;
            ensure!(
                bounded_identity(&self.root, &name)? == identity
                    && bounded_read(&self.root, NAME, RECORD_LIMIT)? == bytes,
                "Theme selection changed before Legacy reset"
            );
            ensure!(
                unsafe { libc::unlinkat(self.root.as_raw_fd(), name.as_ptr(), 0) } == 0,
                "Legacy reset failed: {}",
                std::io::Error::last_os_error()
            );
            self.root.sync_all()?;
            Ok(true)
        })
    }
}
fn bounded_identity(root: &File, name: &CString) -> Result<(u64, u64)> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    ensure!(
        unsafe {
            libc::fstatat(
                root.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0,
        "Temp absent"
    );
    let stat = unsafe { stat.assume_init() };
    ensure!(
        stat.st_mode & libc::S_IFMT == libc::S_IFREG,
        "Unexpected temp type"
    );
    Ok((stat.st_dev as u64, stat.st_ino as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[test]
    fn theme_selection_missing_roundtrip_only_owned_record_and_bad_preserved() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("allium-theme-state-{}", std::process::id()));
        assert!(!root.exists());
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
        let _cleanup = Cleanup(root.clone());
        std::fs::write(root.join("protected-settings"), b"unchanged").unwrap();
        let store = SelectionStore::bind(&root).unwrap();
        let bank = FontBank::bind(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../static/.allium/fonts")
                .canonicalize()
                .unwrap(),
        )
        .unwrap();
        let ready =
            |id| ThemeHandle::prepare(ThemeSnapshot::builtin(id).unwrap(), &bank, 0).unwrap();
        assert!(store.read().unwrap().is_none());
        assert!(!root.join(NAME).exists());
        let first = store.select(&ready("compact")).unwrap();
        assert_eq!(first.generation, 1);
        assert_eq!(store.read().unwrap().unwrap().id, "compact");
        let second = store.select(&ready("orbit")).unwrap();
        assert_eq!(second.generation, 2);
        assert_eq!(
            std::fs::read(root.join("protected-settings")).unwrap(),
            b"unchanged"
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::write(root.join(NAME), b"torn bytes").unwrap();
        assert!(store.read().is_err());
        assert!(store.select(&ready("compact")).is_err());
        assert_eq!(std::fs::read(root.join(NAME)).unwrap(), b"torn bytes");
    }
}
