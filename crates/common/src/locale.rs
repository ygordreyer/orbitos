use std::{
    borrow::Cow,
    collections::HashMap,
    fmt,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};

use anyhow::Result;
use fluent_templates::{
    ArcLoader, LanguageIdentifier, Loader, fluent_bundle::FluentValue, loader::langid,
};
use log::{debug, warn};
use serde::{Deserialize, Serialize};

use crate::constants::{ALLIUM_LOCALE_SETTINGS, ALLIUM_LOCALES_DIR, ALLIUM_THEMES_DIR};
use crate::stylesheet::Theme;

pub use fluent_templates::fluent_bundle::FluentValue as LocaleFluentValue;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocaleSettings {
    pub lang: String,
}

impl Default for LocaleSettings {
    fn default() -> Self {
        Self {
            lang: "en-US".into(),
        }
    }
}

impl LocaleSettings {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn load() -> Result<Self> {
        invalidate_theme_loader();
        if ALLIUM_LOCALE_SETTINGS.exists() {
            debug!("found state, loading from file");
            let file = File::open(ALLIUM_LOCALE_SETTINGS.as_path())?;
            if let Ok(json) = serde_json::from_reader(file) {
                return Ok(json);
            }
            warn!("failed to read locale file, removing");
            fs::remove_file(ALLIUM_LOCALE_SETTINGS.as_path())?;
        }
        Ok(Self::new())
    }

    pub fn save(&self) -> Result<()> {
        let file = File::create(ALLIUM_LOCALE_SETTINGS.as_path())?;
        serde_json::to_writer(file, &self)?;
        invalidate_theme_loader();
        Ok(())
    }
}

pub struct Locale {
    pub default_loader: Arc<ArcLoader>,
    pub theme_loader: Option<Arc<ArcLoader>>,
    pub lang: LanguageIdentifier,
}

fn build_loader(path: &Path) -> Arc<ArcLoader> {
    try_build_loader(path).unwrap()
}

fn try_build_loader(path: &Path) -> Result<Arc<ArcLoader>> {
    Ok(Arc::new(
        ArcLoader::builder(path, langid!("en-US"))
            .customize(|b| b.set_use_isolating(false))
            .build()?,
    ))
}

#[derive(PartialEq)]
struct SourceIdentity(Vec<(PathBuf, u64, Option<SystemTime>)>);
struct CachedLoader {
    identity: SourceIdentity,
    loader: Arc<ArcLoader>,
}

static THEME: OnceLock<Mutex<Option<CachedLoader>>> = OnceLock::new();

// Explicit settings reloads and theme changes must see edits even within
// FAT32's timestamp resolution, where length and mtime can remain unchanged.
pub(crate) fn invalidate_theme_loader() {
    if let Some(cache) = THEME.get() {
        *cache.lock().unwrap() = None;
    }
}

// Include directory entries and file metadata so switching themes, adding a
// language, replacing a resource, and editing an existing override invalidate it.
fn source_identity(path: &Path) -> std::io::Result<SourceIdentity> {
    fn visit(
        path: &Path,
        entries: &mut Vec<(PathBuf, u64, Option<SystemTime>)>,
    ) -> std::io::Result<()> {
        let metadata = fs::metadata(path)?;
        entries.push((path.to_owned(), metadata.len(), Some(metadata.modified()?)));
        if metadata.is_dir() {
            for child in fs::read_dir(path)?.filter_map(|entry| entry.ok()) {
                let kind = child.file_type()?;
                if kind.is_dir()
                    || (kind.is_file() && child.path().extension().is_some_and(|e| e == "ftl"))
                {
                    visit(&child.path(), entries)?;
                }
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    visit(path, &mut entries)?;
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(SourceIdentity(entries))
}

fn theme_loader(cache: &Mutex<Option<CachedLoader>>, path: &Path) -> Option<Arc<ArcLoader>> {
    let mut cached = cache.lock().unwrap();
    if !path.exists()
        || fs::read_dir(path)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true)
    {
        *cached = None;
        return None;
    }
    let identity = source_identity(path).ok();
    if let Some(entry) = cached
        .as_ref()
        .filter(|entry| identity.as_ref() == Some(&entry.identity))
    {
        return Some(entry.loader.clone());
    }
    debug!("loading locale overrides from theme: {}", path.display());
    *cached = None;
    let loader = match try_build_loader(path) {
        Ok(loader) => loader,
        Err(error) => {
            warn!(
                "failed to load theme locales at {}: {error}",
                path.display()
            );
            return None;
        }
    };
    // Retain only the active override source set, and only with a complete identity.
    *cached = identity.map(|identity| CachedLoader {
        identity,
        loader: loader.clone(),
    });
    Some(loader)
}

impl Locale {
    pub fn new(lang: &str) -> Self {
        let theme = Theme::load();
        let theme_locales_dir = ALLIUM_THEMES_DIR.join(&theme.0).join("locales");

        // The installed source set is immutable for the lifetime of this process.
        static DEFAULT: OnceLock<Arc<ArcLoader>> = OnceLock::new();
        let default_loader = DEFAULT
            .get_or_init(|| build_loader(ALLIUM_LOCALES_DIR.as_path()))
            .clone();
        let theme_loader = theme_loader(THEME.get_or_init(|| Mutex::new(None)), &theme_locales_dir);

        let lang = lang.parse().unwrap();
        Self {
            default_loader,
            theme_loader,
            lang,
        }
    }

    pub fn t(&self, key: &str) -> String {
        self.theme_loader
            .as_ref()
            .and_then(|loader| {
                loader.lookup_no_default_fallback(
                    &self.lang,
                    key,
                    None as Option<&HashMap<&str, _>>,
                )
            })
            .unwrap_or_else(|| self.default_loader.lookup(&self.lang, key))
    }

    pub fn ta(&self, key: &str, args: &HashMap<Cow<'static, str>, FluentValue<'_>>) -> String {
        self.theme_loader
            .as_ref()
            .and_then(|loader| loader.lookup_no_default_fallback(&self.lang, key, Some(args)))
            .unwrap_or_else(|| self.default_loader.lookup_with_args(&self.lang, key, args))
    }

    pub fn language(&self) -> String {
        self.lang.to_string()
    }

    pub fn languages(&self) -> Vec<String> {
        let mut vec: Vec<_> = self
            .default_loader
            .locales()
            .map(|i| i.to_string())
            .collect();
        vec.sort_unstable();
        vec
    }
}

impl fmt::Debug for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Locale").field("lang", &self.lang).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_construction_shares_parsed_bundles_and_switches_language() {
        let english = Locale::new("en-US");
        let french = Locale::new("fr-FR");
        assert!(Arc::ptr_eq(&english.default_loader, &french.default_loader));
        assert_eq!(english.t("tab-settings"), "Settings");
        assert_ne!(french.t("tab-settings"), english.t("tab-settings"));
        assert_eq!(english.languages(), french.languages());
        let fallback = Locale::new("zz-ZZ");
        assert_eq!(fallback.t("tab-settings"), "Settings");
    }

    #[test]
    fn theme_source_identity_reuses_and_invalidates_parsed_overrides() {
        let root = std::env::temp_dir().join(format!("allium-locale-cache-{}", std::process::id()));
        fs::create_dir_all(root.join("en-US")).unwrap();
        let source = root.join("en-US/main.ftl");
        fs::write(&source, "tab-settings = Theme settings\n").unwrap();
        let cache = Mutex::new(None);
        let first = theme_loader(&cache, &root).unwrap();
        assert!(Arc::ptr_eq(&first, &theme_loader(&cache, &root).unwrap()));
        let mut locale = Locale {
            default_loader: Locale::new("en-US").default_loader,
            theme_loader: Some(first.clone()),
            lang: langid!("en-US"),
        };
        assert_eq!(locale.t("tab-settings"), "Theme settings");
        locale.lang = langid!("fr-FR");
        assert_ne!(locale.t("tab-settings"), "Theme settings");
        fs::write(&source, "tab-settings = Changed theme settings\n").unwrap();
        let changed = theme_loader(&cache, &root).unwrap();
        assert!(!Arc::ptr_eq(&first, &changed));
        assert_eq!(
            changed.lookup(&langid!("en-US"), "tab-settings"),
            "Changed theme settings"
        );
        fs::remove_file(&source).unwrap();
        fs::remove_dir(root.join("en-US")).unwrap();
        assert!(theme_loader(&cache, &root).is_none());
        fs::remove_dir(&root).unwrap();
    }

    struct ThemeFixture(PathBuf);

    impl ThemeFixture {
        fn new(label: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("allium-locale-{label}-{}", std::process::id()));
            fs::create_dir_all(root.join("en-US")).unwrap();
            fs::write(
                root.join("en-US/main.ftl"),
                "tab-settings = First override\n",
            )
            .unwrap();
            Self(root)
        }
    }

    impl Drop for ThemeFixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn theme_identity_ignores_dangling_ftl_symlinks() {
        let fixture = ThemeFixture::new("dangling");
        std::os::unix::fs::symlink("missing.ftl", fixture.0.join("en-US/dangling.ftl")).unwrap();
        let cache = Mutex::new(None);
        let first = theme_loader(&cache, &fixture.0).unwrap();
        assert_eq!(
            first.lookup(&langid!("en-US"), "tab-settings"),
            "First override"
        );
        assert!(Arc::ptr_eq(
            &first,
            &theme_loader(&cache, &fixture.0).unwrap()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn theme_unreadable_nested_directory_loads_fresh_without_panicking() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let fixture = ThemeFixture::new("unreadable");
        let nested = fixture.0.join("en-US/nested");
        fs::create_dir(&nested).unwrap();
        let cache = Mutex::new(None);
        let first = theme_loader(&cache, &fixture.0).unwrap();
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o000)).unwrap();
        let identity_failed = source_identity(&fixture.0).is_err();
        let fresh = theme_loader(&cache, &fixture.0);
        let cache_bypassed = cache.lock().unwrap().is_none();
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(identity_failed);
        assert!(cache_bypassed);
        let fresh = fresh.unwrap();
        assert!(!Arc::ptr_eq(&first, &fresh));
        assert_eq!(
            fresh.lookup(&langid!("en-US"), "tab-settings"),
            "First override"
        );
    }

    #[test]
    fn explicit_reload_invalidates_equal_length_rewrite_with_restored_mtime() {
        // Isolate process-global paths and caches from concurrent locale tests.
        if std::env::var_os("BOOT_LOCALE_RELOAD_CHILD").is_none() {
            let fixture = ThemeFixture::new("explicit-reload");
            let theme = fixture.0.join("Themes/Allium");
            fs::create_dir_all(theme.join("locales")).unwrap();
            fs::rename(fixture.0.join("en-US"), theme.join("locales/en-US")).unwrap();
            fs::write(theme.join("stylesheet.json"), "{}\n").unwrap();
            let base = fixture.0.join(".allium");
            fs::create_dir_all(base.join("state")).unwrap();
            fs::create_dir_all(base.join("locales/en-US")).unwrap();
            fs::write(
                base.join("locales/en-US/main.ftl"),
                "tab-settings = Settings\n",
            )
            .unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "locale::tests::explicit_reload_invalidates_equal_length_rewrite_with_restored_mtime",
                    "--nocapture",
                ])
                .env("BOOT_LOCALE_RELOAD_CHILD", "1")
                .env("ALLIUM_SD_ROOT", &fixture.0)
                .env("ALLIUM_BASE_DIR", &base)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let root = ALLIUM_THEMES_DIR.join("Allium/locales");
        let source = root.join("en-US/main.ftl");
        let reloads: [fn(); 3] = [
            || {
                LocaleSettings::load().unwrap();
            },
            || {
                LocaleSettings::new().save().unwrap();
            },
            || {
                Theme("Allium".into()).save().unwrap();
            },
        ];
        for (reload, replacement) in
            reloads
                .into_iter()
                .zip(["Other override", "First override", "Other override"])
        {
            let first = Locale::new("en-US");
            let modified = fs::metadata(&source).unwrap().modified().unwrap();
            let before = source_identity(&root).unwrap();
            fs::write(&source, format!("tab-settings = {replacement}\n")).unwrap();
            File::options()
                .write(true)
                .open(&source)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(modified))
                .unwrap();
            assert!(before == source_identity(&root).unwrap());
            let repeated = Locale::new("en-US");
            assert!(Arc::ptr_eq(
                first.theme_loader.as_ref().unwrap(),
                repeated.theme_loader.as_ref().unwrap()
            ));
            reload();
            let reloaded = Locale::new("en-US");
            assert!(!Arc::ptr_eq(
                first.theme_loader.as_ref().unwrap(),
                reloaded.theme_loader.as_ref().unwrap()
            ));
            assert_eq!(reloaded.t("tab-settings"), replacement);
        }
    }

    #[test]
    fn locale_host_construction_bench() {
        if std::env::var_os("BOOT_LOCALE_BENCH").is_none() {
            return;
        }
        let mut samples = Vec::new();
        for iteration in 0..21 {
            let start = std::time::Instant::now();
            std::hint::black_box(Locale::new("en-US"));
            let first = start.elapsed();
            std::hint::black_box(Locale::new("en-US"));
            let pair = start.elapsed();
            if iteration == 0 {
                println!(
                    "BOOT_LOCALE first_us={} second_us={} pair_us={}",
                    first.as_micros(),
                    (pair - first).as_micros(),
                    pair.as_micros()
                );
            }
            samples.push(pair.as_nanos());
        }
        samples.sort_unstable();
        println!(
            "BOOT_LOCALE pair_median_ns={} samples=21 debug={}",
            samples[10],
            cfg!(debug_assertions)
        );
    }
}
