//! Explicit activation boundaries only. No timer, thread or legacy style-save calls.
use super::{
    fonts::{FontBank, ThemeHandle},
    pack::valid_id,
    runtime::{RootBinding, ThemeSnapshot},
    selection::SelectionStore,
};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Identity {
    pub id: String,
    pub revision: String,
    pub digest: String,
    pub generation: u64,
}
impl Identity {
    pub fn loaded(h: &ThemeHandle) -> Self {
        Self {
            id: h.pack().id.clone(),
            revision: h.pack().revision.clone(),
            digest: h.digest().into(),
            generation: h.generation(),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct ThemeStatus {
    pub requested: Option<Identity>,
    pub effective: Option<Identity>,
    pub diagnostic: Option<String>,
}
#[derive(Clone, Copy, Debug)]
pub enum Boundary {
    Launcher,
    MenuOpening,
    OsdOpening,
    UserSelection,
}
pub struct Activation {
    pub snapshot: Option<ThemeHandle>,
    pub status: ThemeStatus,
    pub changed: bool,
    pub diagnostic_once: Option<String>,
}
pub struct ThemeRuntime {
    store: SelectionStore,
    themes: Option<(RootBinding, std::fs::File)>,
    font_root: PathBuf,
    bank: Option<Arc<FontBank>>,
    active: Option<ThemeHandle>,
    status: ThemeStatus,
    last_diagnostic: Option<String>,
    menu_cache: Option<(Option<super::selection::SelectionToken>, String)>,
}
impl ThemeRuntime {
    pub fn bind(state: &Path, themes: &Path, fonts: &Path) -> Result<Self> {
        let themes = match super::runtime::anchored_root(themes) {
            Ok(held) => {
                let binding = RootBinding::bind(themes)?;
                binding.verify_held(&held)?;
                Some((binding, held))
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            store: SelectionStore::bind(state)?,
            themes,
            font_root: fonts.into(),
            bank: None,
            active: None,
            status: ThemeStatus::default(),
            last_diagnostic: None,
            menu_cache: None,
        })
    }
    pub fn choices(&self) -> Result<super::choices::ThemeChoices> {
        if let Some((binding, held)) = &self.themes {
            binding.verify_held(held)?;
            super::choices::discover(held, binding)
        } else {
            let mut out = super::choices::builtins()?;
            out.rejected
                .push("External theme directory unavailable".into());
            Ok(out)
        }
    }
    pub fn choose_legacy(&mut self) -> Result<Activation> {
        self.menu_cache = None;
        self.store.choose_legacy()?;
        let changed = self.active.is_some();
        self.active = None;
        self.status = ThemeStatus::default();
        self.last_diagnostic = None;
        Ok(Activation {
            snapshot: None,
            status: self.status.clone(),
            changed,
            diagnostic_once: None,
        })
    }
    fn prepare(&mut self, id: &str, generation: u64) -> Result<ThemeHandle> {
        ensure!(valid_id(id), "Invalid theme ID");
        let base = if matches!(id, "orbit" | "compact") {
            ThemeSnapshot::builtin(id)?
        } else {
            let (binding, root) = self
                .themes
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("External theme directory unavailable"))?;
            binding.verify_held(root)?;
            let held = super::runtime::directory(root, std::ffi::OsStr::new(id))?;
            ThemeSnapshot::load_held(&held)?
        };
        ensure!(base.0.pack.id == id, "Theme directory identity mismatch");
        if self.bank.is_none() {
            self.bank = Some(FontBank::bind(&self.font_root)?);
        }
        ThemeHandle::prepare(base, self.bank.as_ref().unwrap(), generation)
    }
    pub fn activate_menu(&mut self, locale: &str) -> Activation {
        let token = self.store.cache_token().ok();
        if let (Some(token), Some((previous, lang))) = (&token, &self.menu_cache) {
            if token == previous && lang == locale {
                return Activation {
                    snapshot: self.active.clone(),
                    status: self.status.clone(),
                    changed: false,
                    diagnostic_once: None,
                };
            }
        }
        let activation = self.activate(Boundary::MenuOpening);
        // External packs retain boundary-time asset and root validation.
        let builtin = activation
            .status
            .effective
            .as_ref()
            .is_some_and(|v| matches!(v.id.as_str(), "orbit" | "compact"));
        if builtin
            && activation.status.diagnostic.is_none()
            && self.store.cache_token().ok() == token
        {
            self.menu_cache = token.map(|token| (token, locale.into()));
        }
        activation
    }

    pub fn activate(&mut self, _boundary: Boundary) -> Activation {
        self.menu_cache = None;
        let old = self.status.effective.clone();
        let read = self.store.read();
        let mut diagnostic = None;
        match read {
            Ok(None) => {
                self.active = None;
                self.status = ThemeStatus::default();
            }
            Ok(Some(requested)) => {
                self.status.requested = Some(Identity {
                    id: requested.id.clone(),
                    revision: requested.revision.clone(),
                    digest: requested.digest.clone(),
                    generation: requested.generation,
                });
                let unchanged = self
                    .status
                    .effective
                    .as_ref()
                    .is_some_and(|v| v == self.status.requested.as_ref().unwrap());
                if !unchanged || !matches!(requested.id.as_str(), "orbit" | "compact") {
                    let result = self
                        .prepare(&requested.id, requested.generation)
                        .and_then(|h| {
                            ensure!(
                                h.pack().revision == requested.revision
                                    && h.digest() == requested.digest,
                                "Requested theme bytes differ"
                            );
                            Ok(h)
                        });
                    match result {
                        Ok(h) => self.active = Some(h),
                        Err(error) => {
                            diagnostic = Some(format!("Theme unavailable: {error}"));
                            if self.active.is_none() {
                                if let Ok(safe) = self.prepare("orbit", requested.generation) {
                                    self.active = Some(safe);
                                }
                            }
                        }
                    }
                }
            }
            Err(error) => {
                diagnostic = Some(format!("Theme selection unavailable: {error}"));
                self.status.requested = None;
                if let Ok(safe) = self.prepare("orbit", 0) {
                    self.active = Some(safe);
                }
            }
        }
        self.status.effective = self.active.as_ref().map(Identity::loaded);
        self.status.diagnostic = diagnostic.clone();
        let once = if diagnostic != self.last_diagnostic {
            diagnostic.clone()
        } else {
            None
        };
        self.last_diagnostic = diagnostic;
        Activation {
            snapshot: self.active.clone(),
            status: self.status.clone(),
            changed: old != self.status.effective,
            diagnostic_once: once,
        }
    }
    /// Candidate assets/fonts validate fully BEFORE the sole owned selection write.
    pub fn choose(&mut self, id: &str) -> Result<Activation> {
        self.menu_cache = None;
        let candidate = self.prepare(id, 0)?;
        let record = self.store.select(&candidate)?;
        let selected = candidate.generation_copy(record.generation);
        let old = self.status.effective.clone();
        self.status.requested = Some(Identity::loaded(&selected));
        self.status.effective = self.status.requested.clone();
        self.status.diagnostic = None;
        self.last_diagnostic = None;
        self.active = Some(selected);
        Ok(Activation {
            snapshot: self.active.clone(),
            status: self.status.clone(),
            changed: old != self.status.effective,
            diagnostic_once: None,
        })
    }
    pub fn status(&self) -> &ThemeStatus {
        &self.status
    }
    pub fn resource_accounting(&self) -> (usize, usize, u64) {
        let (fonts, bytes) = self.bank.as_ref().map_or((0, 0), |b| b.accounting());
        (
            fonts,
            bytes,
            self.active.as_ref().map_or(0, ThemeHandle::pixel_bytes),
        )
    }
}

#[cfg(test)]
mod menu_cache_tests {
    use super::*;
    #[test]
    fn builtin_menu_activation_tracks_selection_generation_locale_and_errors() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("perf7-theme-cache-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let fonts = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../static/.allium/fonts")
            .canonicalize()
            .unwrap();
        let mut runtime = ThemeRuntime::bind(&root, &root, &fonts).unwrap();
        runtime.choose("orbit").unwrap();
        let first = runtime.activate_menu("en");
        assert!(runtime.menu_cache.is_some());
        let cached = runtime.menu_cache.clone();
        assert!(!runtime.activate_menu("en").changed);
        assert_eq!(runtime.menu_cache, cached);
        let next = runtime.activate_menu("pt");
        assert_eq!(
            next.snapshot.unwrap().generation(),
            first.snapshot.unwrap().generation()
        );
        assert_eq!(runtime.menu_cache.as_ref().unwrap().1, "pt");
        let other = runtime.choose("compact").unwrap().snapshot.unwrap();
        assert!(runtime.menu_cache.is_none());
        assert_eq!(
            runtime.activate_menu("pt").snapshot.unwrap().digest(),
            other.digest()
        );
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("ui-theme.json")).unwrap()).unwrap();
        record["generation"] = serde_json::json!(other.generation() + 1);
        std::fs::write(
            root.join("ui-theme.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        assert_eq!(
            runtime.activate_menu("pt").snapshot.unwrap().generation(),
            other.generation() + 1
        );
        std::fs::write(root.join("ui-theme.json"), b"invalid").unwrap();
        assert!(runtime.activate_menu("pt").status.diagnostic.is_some());
        assert!(runtime.menu_cache.is_none());
        std::fs::remove_file(root.join("ui-theme.json")).unwrap();
        assert!(runtime.activate_menu("pt").snapshot.is_none());
        runtime.choose("orbit").unwrap();
        runtime.activate_menu("pt");
        runtime.choose_legacy().unwrap();
        assert!(runtime.activate_menu("pt").snapshot.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
