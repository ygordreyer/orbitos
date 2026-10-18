//! Explicit activation boundaries and coalesced option previews. No legacy style-save calls.
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
    option_cache: std::collections::VecDeque<ThemeHandle>,
    pending_settings: Option<super::selection::Selection>,
    osd_token: Option<Option<super::selection::SelectionToken>>,
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
            osd_token: None,
            option_cache: Default::default(),
            pending_settings: None,
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
        self.osd_token = None;
        self.flush_settings()?;
        self.option_cache.clear();
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
        self.prepare_options(id, generation, &Default::default())
    }
    fn prepare_options(
        &mut self,
        id: &str,
        generation: u64,
        settings: &super::settings::Values,
    ) -> Result<ThemeHandle> {
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
            ThemeSnapshot::load_held(&held, settings)?
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

    /// Volume keys reuse validated assets until an explicit selection/settings change.
    /// General activation boundaries still revalidate external packs and invalidate this cache.
    pub fn activate_osd(&mut self) -> Activation {
        let token = self.store.cache_token().ok();
        let root_valid = self.themes.as_ref().is_none_or(|(binding, held)| binding.verify_held(held).is_ok());
        if self.pending_settings.is_none() && root_valid && token.is_some() {
            let current = self.osd_token == token || (self.status.diagnostic.is_none()
                && self.store.read().ok().is_some_and(|selection| match (&self.active, selection) {
                    (None, None) => true,
                    (Some(active), Some(selected)) => active.pack().id == selected.id
                        && active.pack().revision == selected.revision && active.digest() == selected.digest
                        && active.generation() == selected.generation && active.settings() == &selected.settings,
                    _ => false,
                }) && self.store.cache_token().ok() == token);
            if current {
                self.osd_token = token;
                return Activation { snapshot: self.active.clone(), status: self.status.clone(), changed: false, diagnostic_once: None };
            }
        }
        let start = std::time::Instant::now();
        let activation = self.activate(Boundary::OsdOpening);
        if activation.status.diagnostic.is_none() && self.store.cache_token().ok() == token {
            self.osd_token = token;
        }
        log::info!(target: "alliumd::osd", "osd theme activation changed={} elapsed_ms={}", activation.changed, start.elapsed().as_millis());
        activation
    }

    pub fn activate(&mut self, _boundary: Boundary) -> Activation {
        self.osd_token = None;
        if let Err(error) = self.flush_settings() {
            self.status.diagnostic = Some(format!("Theme settings could not be saved: {error}"));
            return Activation {
                snapshot: self.active.clone(),
                status: self.status.clone(),
                changed: false,
                diagnostic_once: self.status.diagnostic.clone(),
            };
        }
        self.option_cache.clear();
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
                    let (id, settings) =
                        super::settings::migrate(&requested.id, &requested.settings);
                    let result = self.prepare_options(&id, requested.generation, &settings);
                    match result {
                        Ok(h) => {
                            // Fully validated updates and canonical defaults publish a new identity
                            // only if the stored request still matches under the writer guard.
                            if h.pack().id != requested.id
                                || h.settings() != &requested.settings
                                || h.pack().revision != requested.revision
                                || h.digest() != requested.digest
                            {
                                match self.store.migrate(&h, &requested) {
                                    Ok(record) => {
                                        let h = h.generation_copy(record.generation);
                                        self.status.requested = Some(Identity::loaded(&h));
                                        self.active = Some(h);
                                    }
                                    Err(error) => {
                                        diagnostic = Some(format!(
                                            "Theme migration could not be saved: {error}"
                                        ));
                                        self.active = Some(h);
                                    }
                                }
                            } else {
                                self.active = Some(h);
                            }
                        }
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
        self.flush_settings()?;
        let (canonical, _) = super::settings::migrate(id, &Default::default());
        let settings = self
            .store
            .read()?
            .map(|v| v.saved_settings(&canonical))
            .unwrap_or_default();
        self.choose_with_settings(id, &settings)
    }
    pub fn choose_setting(&mut self, setting: &str, option: &str) -> Result<Activation> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No active theme"))?;
        ensure!(
            active
                .pack()
                .settings
                .iter()
                .any(|s| s.id == setting && s.options.iter().any(|o| o.id == option)),
            "Unknown theme setting or option"
        );
        let id = active.pack().id.clone();
        let mut settings = active.settings().clone();
        settings.insert(setting.into(), option.into());
        if &settings == active.settings() {
            return Ok(self.preview_activation(active.clone()));
        }
        let expected = match &self.pending_settings {
            Some(expected) => expected.clone(),
            None => self
                .store
                .read()?
                .ok_or_else(|| anyhow::anyhow!("No saved theme selection"))?,
        };
        let generation = expected
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Theme generation overflow"))?;
        if self.option_cache.is_empty() {
            self.cache_option(active.clone());
        }
        let candidate =
            if let Some(cached) = self.option_cache.iter().find(|h| h.settings() == &settings) {
                cached.generation_copy(generation)
            } else {
                let candidate = self.prepare_options(&id, generation, &settings)?;
                self.cache_option(candidate.clone());
                candidate
            };
        self.pending_settings = Some(expected);
        Ok(self.preview_activation(candidate))
    }
    fn cache_option(&mut self, handle: ThemeHandle) {
        // Bound retained decoded pixels on ARM. Cache lifetime ends at the next
        // explicit theme choice or activation boundary, which revalidates pack files.
        const CACHE_BYTES: u64 = 32 * 1024 * 1024;
        self.option_cache.push_back(handle);
        while self.option_cache.len() > 16
            || (self.option_cache.len() > 1
                && self
                    .option_cache
                    .iter()
                    .map(ThemeHandle::pixel_bytes)
                    .sum::<u64>()
                    > CACHE_BYTES)
        {
            self.option_cache.pop_front();
        }
    }
    fn preview_activation(&mut self, selected: ThemeHandle) -> Activation {
        self.menu_cache = None;
        let old = self.status.effective.clone();
        self.status.requested = Some(Identity::loaded(&selected));
        self.status.effective = self.status.requested.clone();
        self.status.diagnostic = None;
        self.last_diagnostic = None;
        self.active = Some(selected);
        Activation {
            snapshot: self.active.clone(),
            status: self.status.clone(),
            changed: old != self.status.effective,
            diagnostic_once: None,
        }
    }
    pub fn choose_with_settings(
        &mut self,
        id: &str,
        settings: &super::settings::Values,
    ) -> Result<Activation> {
        self.flush_settings()?;
        self.menu_cache = None;
        let (id, settings) = super::settings::migrate(id, settings);
        let candidate = self.prepare_options(&id, 0, &settings)?;
        let record = self.store.select(&candidate)?;
        let selected = candidate.generation_copy(record.generation);
        self.option_cache.clear();
        self.cache_option(selected.clone());
        Ok(self.preview_activation(selected))
    }
    /// Commit the final preview once, on release or leaving the settings view.
    /// A concurrent selection is preserved rather than overwritten by a stale preview.
    pub fn flush_settings(&mut self) -> Result<()> {
        if let Some(expected) = &self.pending_settings {
            let active = self.active.as_ref().unwrap();
            let record = self.store.migrate(active, expected)?;
            let selected = active.generation_copy(record.generation);
            self.pending_settings = None;
            self.preview_activation(selected);
        }
        Ok(())
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

#[cfg(test)]
mod review_fix_tests {
    use super::super::{runtime::REVIEW_COUNTS, settings::Values};
    use super::*;
    use serde_json::json;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("allium-review-{name}-{}", std::process::id()));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn runtime(&self, themes: &Path) -> ThemeRuntime {
            let fonts = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../static/.allium/fonts")
                .canonicalize()
                .unwrap();
            ThemeRuntime::bind(&self.0, themes, &fonts).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn themes() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../themes")
            .canonicalize()
            .unwrap()
    }
    #[test]
    fn osd_activation_reuses_external_snapshot_and_invalidates_on_selection_change() {
        let root = Fixture::new("osd-cache");
        let mut runtime = root.runtime(&themes());
        runtime.choose("compact-plus").unwrap();
        let first = runtime.activate_osd().snapshot.unwrap();
        REVIEW_COUNTS.with(|c| c.set([0; 4]));
        let next = runtime.activate_osd().snapshot.unwrap();
        assert!(first.downgrade().matches(&next));
        assert_eq!(REVIEW_COUNTS.with(|c| c.get()), [0; 4], "hot volume keys do no parsing/decoding/font preparation");
        let menu = runtime.activate(Boundary::MenuOpening).snapshot.unwrap();
        REVIEW_COUNTS.with(|c| c.set([0; 4]));
        let after_menu = runtime.activate_osd().snapshot.unwrap();
        assert!(menu.downgrade().matches(&after_menu), "menu activation already validated this snapshot");
        assert_eq!(REVIEW_COUNTS.with(|c| c.get()), [0; 4]);
        runtime.choose("big-picture").unwrap();
        let changed = runtime.activate_osd().snapshot.unwrap();
        assert_eq!(changed.pack().id, "big-picture");
        assert!(!first.downgrade().matches(&changed));
        runtime.choose_legacy().unwrap();
        assert!(runtime.activate_osd().snapshot.is_none());
    }

    #[test]
    fn review_per_theme_options_survive_switching_and_legacy_records() {
        for legacy in [false, true] {
            let root = Fixture::new(if legacy { "legacy-map" } else { "map" });
            let mut runtime = root.runtime(&themes());
            let wanted = Values::from([
                ("cover_shape".into(), "tall".into()),
                ("color_scheme".into(), "black".into()),
            ]);
            runtime
                .choose_with_settings("compact-plus", &wanted)
                .unwrap();
            if legacy {
                let path = root.0.join("ui-theme.json");
                let mut raw: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                raw.as_object_mut().unwrap().remove("theme_settings");
                std::fs::write(path, serde_json::to_vec(&raw).unwrap()).unwrap();
            }
            // A fresh session must recover even a legacy active-only record.
            let mut runtime = root.runtime(&themes());
            runtime.activate(Boundary::Launcher);
            runtime.choose("orbit-hero").unwrap();
            let mut runtime = root.runtime(&themes());
            runtime.activate(Boundary::Launcher);
            let restored = runtime.choose("compact-plus").unwrap().snapshot.unwrap();
            assert_eq!(restored.settings(), &wanted);
        }
    }
    #[test]
    fn theme_json_update_preserves_selected_pack_color_and_shape_without_revision_bump() {
        let root = Fixture::new("same-revision-update");
        let pack = root.0.join("compact-plus");
        std::fs::create_dir_all(pack.join("assets")).unwrap();
        let source = themes().join("compact-plus");
        let mut raw: serde_json::Value = serde_json::from_slice(
            &std::fs::read(source.join("theme.json")).unwrap()).unwrap();
        for asset in std::fs::read_dir(source.join("assets")).unwrap() {
            let asset = asset.unwrap();
            std::fs::copy(asset.path(), pack.join("assets").join(asset.file_name())).unwrap();
        }
        let write = |raw: &serde_json::Value| {
            std::fs::write(pack.join("theme.json"), serde_json::to_vec(raw).unwrap()).unwrap();
        };
        write(&raw);
        let wanted = Values::from([
            ("cover_shape".into(), "tall".into()),
            ("color_scheme".into(), "black".into()),
        ]);
        let mut runtime = root.runtime(&root.0);
        runtime.choose_with_settings("compact-plus", &wanted).unwrap();
        let previous = runtime.store.read().unwrap().unwrap();
        raw["tokens"]["typography"]["status"]["size"] = json!(19);
        write(&raw);
        for mut runtime in [root.runtime(&root.0), runtime] {
            let active = runtime.activate(Boundary::Launcher);
            assert!(active.status.diagnostic.is_none(), "{:?}", active.status);
            let handle = active.snapshot.unwrap();
            assert_eq!(handle.pack().id, "compact-plus");
            assert_eq!(handle.pack().tokens.typography[&super::super::pack::Role::Status].size, 19);
            assert_eq!(handle.pack().revision, previous.revision);
            assert_eq!(handle.settings(), &wanted);
            let saved = runtime.store.read().unwrap().unwrap();
            assert_eq!(saved.id, "compact-plus");
            assert_ne!(saved.digest, previous.digest);
            assert_eq!(saved.settings, wanted);
            assert_eq!(saved.theme_settings["compact-plus"], wanted);
            assert_eq!(saved.generation, previous.generation + 1);
        }
    }

    #[test]
    fn review_validated_pack_updates_migrate_identity_and_defaults() {
        for change in 0..4 {
            let root = Fixture::new(&format!("update-{change}"));
            let pack = root.0.join("updated");
            std::fs::create_dir_all(pack.join("assets")).unwrap();
            std::fs::write(
                pack.join("assets/accent-band.png"),
                include_bytes!("packs/accent-band.png"),
            )
            .unwrap();
            std::fs::write(
                pack.join("assets/next-disc-mask.png"),
                include_bytes!("packs/next-disc-mask.png"),
            )
            .unwrap();
            let mut raw: serde_json::Value =
                serde_json::from_slice(super::super::pack::Pack::builtin_bytes("compact").unwrap())
                    .unwrap();
            raw["id"] = json!("updated");
            raw["revision"] = json!("native-ink-1");
            raw["settings"] = json!([{ "id":"cover_shape", "label":"Cover shape", "default":"square", "options":[
                {"id":"square","label":"Square","patch":{}}, {"id":"tall","label":"Tall","patch":{"name":"Tall"}}]}]);
            let write = |raw: &serde_json::Value| {
                std::fs::write(pack.join("theme.json"), serde_json::to_vec(raw).unwrap()).unwrap()
            };
            write(&raw);
            let mut runtime = root.runtime(&root.0);
            runtime
                .choose_with_settings(
                    "updated",
                    &Values::from([("cover_shape".into(), "tall".into())]),
                )
                .unwrap();
            let previous = runtime.store.read().unwrap().unwrap();
            match change {
                0 => raw["revision"] = json!("native-ink-2"),
                1 => {
                    raw["revision"] = json!("native-ink-2");
                    raw["settings"][0]["options"].as_array_mut().unwrap().pop();
                }
                2 => {
                    raw["settings"][0]["options"].as_array_mut().unwrap().pop();
                }
                _ => {
                    raw.as_object_mut().unwrap().remove("settings");
                }
            }
            write(&raw);
            let mut fresh = root.runtime(&root.0);
            let activation = fresh.activate(Boundary::Launcher);
            assert!(
                activation.status.diagnostic.is_none(),
                "{:?}",
                activation.status
            );
            let handle = activation.snapshot.unwrap();
            assert_eq!(handle.pack().id, "updated");
            assert_eq!(
                handle.settings().get("cover_shape").map(String::as_str),
                match change {
                    0 => Some("tall"),
                    3 => None,
                    _ => Some("square"),
                }
            );
            let saved = fresh.store.read().unwrap().unwrap();
            assert_eq!(saved.generation, previous.generation + 1);
            assert_eq!(saved.digest, handle.digest());
            assert_eq!(saved.revision, handle.pack().revision);
            assert_eq!(saved.settings, *handle.settings());
            assert_eq!(saved.theme_settings["updated"], *handle.settings());
            assert!(
                fresh.store.migrate(&handle, &previous).is_err(),
                "stale migration must not overwrite a changed record"
            );
            assert_eq!(fresh.store.read().unwrap().unwrap(), saved);
            let reopened = root.runtime(&root.0).activate(Boundary::Launcher);
            assert_eq!(reopened.snapshot.unwrap().generation(), saved.generation);
        }
    }
    #[test]
    fn review_pending_options_preserve_concurrent_selection() {
        let root = Fixture::new("pending-conflict");
        let mut runtime = root.runtime(&themes());
        runtime.choose("compact-plus").unwrap();
        runtime.choose_setting("color_scheme", "black").unwrap();
        let mut other = root.runtime(&themes());
        other.choose("orbit-hero").unwrap();
        let saved = other.store.read().unwrap().unwrap();
        assert!(runtime.flush_settings().is_err());
        assert_eq!(runtime.store.read().unwrap().unwrap(), saved);
    }
    #[test]
    fn review_visited_options_skip_reads_decodes_and_coalesce_writes() {
        let root = Fixture::new("hot-options");
        let mut runtime = root.runtime(&themes());
        runtime.choose("compact-plus").unwrap();
        runtime.choose_setting("color_scheme", "black").unwrap();
        runtime.flush_settings().unwrap();
        let before = REVIEW_COUNTS.with(|c| c.get());
        for _ in 0..8 {
            runtime.choose_setting("color_scheme", "orange").unwrap();
            runtime.choose_setting("color_scheme", "black").unwrap();
        }
        assert_eq!(REVIEW_COUNTS.with(|c| c.get()), before);
        runtime.choose_setting("color_scheme", "orange").unwrap();
        runtime.flush_settings().unwrap();
        runtime.flush_settings().unwrap();
        let after = REVIEW_COUNTS.with(|c| c.get());
        assert_eq!(&after[..3], &before[..3]);
        assert_eq!(after[3], before[3] + 1);
        runtime.choose_setting("color_scheme", "white").unwrap();
        let new = REVIEW_COUNTS.with(|c| c.get());
        assert_eq!(new[0], after[0] + 1);
        assert!(new[1] > after[1]);
        assert_eq!(new[2], after[2] + 1);
        assert_eq!(new[3], after[3]);
        runtime.choose_setting("color_scheme", "white").unwrap();
        assert_eq!(REVIEW_COUNTS.with(|c| c.get()), new);
        runtime.flush_settings().unwrap();
        let restored = root
            .runtime(&themes())
            .activate(Boundary::Launcher)
            .snapshot
            .unwrap();
        assert_eq!(restored.settings()["color_scheme"], "white");
    }
}
