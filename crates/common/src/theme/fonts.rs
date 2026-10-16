//! Fixed trusted font registry sharing file-backed mappings with the stylesheet.
use super::{
    pack::{FontId, Role},
    runtime::{RootBinding, ThemeSnapshot, anchored_root, bounded_file},
};
use anyhow::{Result, ensure};
use rusttype::Font;
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
const FONT_LIMIT: usize = 32 * 1024 * 1024;
const FONT_BANK_LIMIT: usize = 64 * 1024 * 1024;
pub struct FontBank {
    path: PathBuf,
    root: File,
    binding: RootBinding,
    nested: Mutex<BTreeMap<String, (RootBinding, File)>>,
    cache: Mutex<BTreeMap<FontId, (Font<'static>, usize)>>,
}
impl FontBank {
    pub fn bind(path: &Path) -> Result<Arc<Self>> {
        let root = anchored_root(path)?;
        let binding = RootBinding::bind(path)?;
        binding.verify_held(&root)?;
        Ok(Arc::new(Self {
            path: path.into(),
            root,
            binding,
            nested: Mutex::new(BTreeMap::new()),
            cache: Mutex::new(BTreeMap::new()),
        }))
    }
    fn load(&self, id: FontId) -> Result<Font<'static>> {
        if id == FontId::BarlowCondensed {
            return Ok(Font::try_from_bytes(include_bytes!(
                "packs/fonts/BarlowCondensed-SemiBold.ttf"
            ) as &[u8])
            .unwrap());
        }
        self.binding.check()?;
        let resource = Path::new(id.relative_path());
        let read_root =
            if let Some(parent) = resource.parent().filter(|p| !p.as_os_str().is_empty()) {
                let mut nested = self
                    .nested
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Font directory unavailable"))?;
                let name = parent.to_string_lossy().into_owned();
                if !nested.contains_key(&name) {
                    let held = super::runtime::directory(&self.root, parent.as_os_str())?;
                    let binding = RootBinding::bind(&self.path.join(parent))?;
                    binding.verify_held(&held)?;
                    nested.insert(name.clone(), (binding, held));
                }
                nested[&name].0.check()?;
                nested[&name].1.try_clone()?
            } else {
                self.root.try_clone()?
            };
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Font registry unavailable"))?;
        if let Some((font, _)) = cache.get(&id) {
            return Ok(font.clone());
        }
        let file = bounded_file(
            &read_root,
            resource.file_name().unwrap().to_str().unwrap(),
            FONT_LIMIT,
        )?;
        let size = file.metadata()?.len() as usize;
        ensure!(
            cache.values().map(|(_, n)| n).sum::<usize>() + size <= FONT_BANK_LIMIT,
            "Trusted font residency cap exceeded"
        );
        let font = Font::try_from_bytes(crate::stylesheet::map_font_file(&file)?)
            .ok_or_else(|| anyhow::anyhow!("Required trusted font cannot be parsed"))?;
        cache.insert(id, (font.clone(), size));
        Ok(font)
    }
    pub fn heap_bytes(&self) -> usize {
        self.cache
            .lock()
            .expect("Font registry poisoned")
            .values()
            .map(|(font, _)| match font {
                Font::Owned(face) => face.as_slice().len(),
                Font::Ref(_) => 0,
            })
            .sum()
    }
    pub fn accounting(&self) -> (usize, usize) {
        let c = self.cache.lock().expect("Font registry poisoned");
        (c.len(), c.values().map(|(_, n)| n).sum())
    }
}
#[derive(Clone)]
pub struct ThemeHandle(Arc<PreparedTheme>);
pub(super) struct PreparedTheme {
    base: ThemeSnapshot,
    fonts: BTreeMap<Role, Font<'static>>,
    fallback: Font<'static>,
    ink_fonts: BTreeMap<FontId, Font<'static>>,
    generation: u64,
}
impl ThemeHandle {
    pub(super) fn weak(&self) -> std::sync::Weak<PreparedTheme> {
        Arc::downgrade(&self.0)
    }
    pub fn prepare(base: ThemeSnapshot, bank: &FontBank, generation: u64) -> Result<Self> {
        #[cfg(test)]
        super::runtime::review_count(2);
        let mut fonts = BTreeMap::new();
        for (role, t) in &base.0.pack.tokens.typography {
            fonts.insert(*role, bank.load(t.font)?);
        }
        let fallback = bank.load(FontId::NotoCjk)?;
        let mut ink_fonts = BTreeMap::new();
        if base.0.pack.ink() {
            for id in [FontId::Nunito, FontId::Roboto, FontId::BarlowCondensed] {
                ink_fonts.insert(id, bank.load(id)?);
            }
        }
        Ok(Self(Arc::new(PreparedTheme {
            base,
            fonts,
            fallback,
            ink_fonts,
            generation,
        })))
    }
    pub fn role_font(&self, role: Role) -> Font<'static> {
        self.0.fonts[&role].clone()
    }
    pub(super) fn ink_font(&self, id: FontId, weight: u32) -> Font<'static> {
        if id == FontId::BarlowCondensed && weight >= 700 {
            return Font::try_from_bytes(
                include_bytes!("packs/fonts/BarlowCondensed-Bold.ttf") as &[u8]
            )
            .unwrap();
        }
        self.0
            .ink_fonts
            .get(&id)
            .cloned()
            .unwrap_or_else(|| self.0.fallback.clone())
    }
    pub fn fallback_font(&self) -> Font<'static> {
        self.0.fallback.clone()
    }
    pub fn pack(&self) -> &super::pack::Pack {
        &self.0.base.0.pack
    }
    pub fn settings(&self) -> &super::settings::Values {
        &self.0.base.0.settings
    }
    pub fn digest(&self) -> &str {
        &self.0.base.0.digest
    }
    #[cfg(any(debug_assertions, feature = "perf-bench"))]
    #[doc(hidden)]
    pub fn debug_prepared_preview_identity(&self) -> Option<usize> {
        self.0
            .base
            .0
            .preview_cache
            .lock()
            .ok()?
            .as_ref()
            .map(|cache| Arc::as_ptr(&cache.pixels) as usize)
    }
    pub fn generation(&self) -> u64 {
        self.0.generation
    }
    pub fn pixel_bytes(&self) -> u64 {
        self.0.base.0.decoded_bytes
    }
    pub fn strong_count(&self) -> usize {
        Arc::strong_count(&self.0)
    }
    pub(super) fn base(&self) -> &ThemeSnapshot {
        &self.0.base
    }
    pub fn text_style(
        &self,
        role: Role,
        color: crate::display::color::Color,
    ) -> crate::display::font::FontTextStyle {
        crate::display::font::FontTextStyle::with_fallback(
            self.0.fonts[&role].clone(),
            color,
            self.pack().tokens.typography[&role].size,
            self.0.fallback.clone(),
        )
    }
    pub fn component_text_style(
        &self,
        c: &super::pack::Component,
        color: crate::display::color::Color,
    ) -> crate::display::font::FontTextStyle {
        let role = c.role.unwrap();
        if !self.pack().ink() {
            return self.text_style(role, color);
        }
        let t = &self.pack().tokens.typography[&role];
        let run = c.paint.as_ref().and_then(|p| p.run.as_ref());
        let font = run.and_then(|r| r.font).unwrap_or(t.font);
        let weight = run
            .and_then(|r| r.weight)
            .or_else(|| self.pack().ink_typography.get(&role).and_then(|r| r.weight))
            .unwrap_or(400);
        crate::display::font::FontTextStyle::with_fallback(
            self.ink_font(font, weight),
            color,
            run.and_then(|r| r.size).unwrap_or(t.size),
            self.fallback_font(),
        )
    }
    pub(super) fn generation_copy(&self, generation: u64) -> Self {
        Self(Arc::new(PreparedTheme {
            base: self.0.base.clone(),
            fonts: self.0.fonts.clone(),
            fallback: self.0.fallback.clone(),
            ink_fonts: self.0.ink_fonts.clone(),
            generation,
        }))
    }
}
