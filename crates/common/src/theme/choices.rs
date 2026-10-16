//! Descriptor-only discovery. Assets and fonts are prepared only when chosen.
use super::{
    pack::{MANIFEST_LIMIT, Pack, valid_id},
    runtime::{RootBinding, bounded_read, digest, directory},
};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    ffi::{CStr, CString},
    fs::File,
    os::unix::io::AsRawFd,
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum ChoiceKind {
    Legacy,
    Builtin,
    External,
}
#[derive(Debug, Clone, Serialize)]
pub struct ThemeChoice {
    pub id: Option<String>,
    pub name: String,
    pub revision: Option<String>,
    pub digest: Option<String>,
    pub kind: ChoiceKind,
}
#[derive(Debug, Clone, Serialize)]
pub struct ThemeChoices {
    pub choices: Vec<ThemeChoice>,
    pub rejected: Vec<String>,
}
impl ThemeChoice {
    fn pack(pack: Pack, hash: String, kind: ChoiceKind) -> Self {
        Self {
            id: Some(pack.id),
            name: pack.name,
            revision: Some(pack.revision),
            digest: Some(hash),
            kind,
        }
    }
}
pub(super) fn discover(root: &File, binding: &RootBinding) -> Result<ThemeChoices> {
    binding.check()?;
    let mut out = builtins()?;
    let dot = CString::new(".")?;
    let fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    ensure!(fd >= 0, "Theme directory enumeration open failed");
    let raw = unsafe { libc::fdopendir(fd) };
    if raw.is_null() {
        unsafe { libc::close(fd) };
        return Err(std::io::Error::last_os_error().into());
    }
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    let held = Directory(raw);
    unsafe { libc::rewinddir(held.0) };
    let mut names = 0;
    let mut directories = 0;
    loop {
        clear_errno();
        let entry = unsafe { libc::readdir(held.0) };
        if entry.is_null() {
            ensure!(
                std::io::Error::last_os_error().raw_os_error() == Some(0),
                "Theme directory enumeration failed"
            );
            break;
        }
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names += 1;
        ensure!(names <= 128, "Theme directory entry limit exceeded");
        let name = CString::new(bytes)?;
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
            "Theme directory changed during discovery"
        );
        if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFDIR {
            continue;
        }
        directories += 1;
        ensure!(directories <= 32, "External theme directory limit exceeded");
        let Ok(id) = std::str::from_utf8(bytes) else {
            out.rejected.push("Invalid theme directory ID".into());
            continue;
        };
        if !valid_id(id) || matches!(id, "orbit" | "compact") {
            out.rejected
                .push("Reserved or invalid theme directory ID".into());
            continue;
        }
        let candidate = (|| -> Result<ThemeChoice> {
            let held = directory(root, std::ffi::OsStr::new(id))?;
            let bytes = bounded_read(&held, "theme.json", MANIFEST_LIMIT)?;
            let pack = Pack::parse(&bytes)?;
            ensure!(pack.id == id, "Theme directory identity mismatch");
            Ok(ThemeChoice::pack(
                pack,
                digest(&bytes),
                ChoiceKind::External,
            ))
        })();
        match candidate {
            Ok(choice) => out.choices.push(choice),
            Err(_) => out.rejected.push(format!("Invalid theme descriptor: {id}")),
        }
    }
    binding.check()?;
    out.choices[3..].sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}
pub(super) fn builtins() -> Result<ThemeChoices> {
    let mut choices = vec![ThemeChoice {
        id: None,
        name: "Legacy".into(),
        revision: None,
        digest: None,
        kind: ChoiceKind::Legacy,
    }];
    for id in ["orbit", "compact"] {
        choices.push(ThemeChoice::pack(
            Pack::builtin(id)?,
            digest(Pack::builtin_bytes(id)?),
            ChoiceKind::Builtin,
        ));
    }
    Ok(ThemeChoices {
        choices,
        rejected: vec![],
    })
}
#[cfg(target_os = "macos")]
fn clear_errno() {
    unsafe {
        *libc::__error() = 0;
    }
}
#[cfg(not(target_os = "macos"))]
fn clear_errno() {
    unsafe {
        *libc::__errno_location() = 0;
    }
}
