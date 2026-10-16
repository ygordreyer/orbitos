use super::pack::{FadeEdge, Fit, ScreenId};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::{
    collections::VecDeque,
    sync::{Arc, Weak},
};
use tiny_skia::Pixmap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SourceIdentity {
    width: u32,
    height: u32,
    digest: [u8; 32],
}

thread_local! {
    static IDENTITIES: RefCell<VecDeque<(Weak<Pixmap>, SourceIdentity)>> = const { RefCell::new(VecDeque::new()) };
}

pub(super) fn source_identity(source: &Arc<Pixmap>) -> SourceIdentity {
    IDENTITIES.with(|identities| {
        let mut identities = identities.borrow_mut();
        if let Some(index) = identities
            .iter()
            .position(|(held, _)| held.ptr_eq(&Arc::downgrade(source)))
        {
            let entry = identities.remove(index).unwrap();
            let identity = entry.1.clone();
            identities.push_front(entry);
            return identity;
        }
        let identity = SourceIdentity {
            width: source.width(),
            height: source.height(),
            digest: Sha256::digest(source.data()).into(),
        };
        identities.retain(|(source, _)| source.strong_count() > 0);
        if identities.len() >= 128 {
            identities.pop_back();
        }
        identities.push_front((Arc::downgrade(source), identity.clone()));
        identity
    })
}

const PIXEL_LIMIT: usize = 1536 * 1024;
const ENTRY_LIMIT: usize = 16;

#[derive(PartialEq, Eq)]
pub(super) struct ImageKey {
    pub generation: u64,
    pub screen: ScreenId,
    pub component: String,
    pub width: u32,
    pub height: u32,
    pub fit: Fit,
    pub radius: u32,
    pub blur: u32,
    pub fade_edge: Option<FadeEdge>,
    pub fade_width: Option<u32>,
    pub mask: usize,
    pub background: Option<[u8; 4]>,
}
struct Entry {
    key: ImageKey,
    source: SourceIdentity,
    pixels: Arc<Pixmap>,
}
#[derive(Default)]
pub(super) struct ImageCache {
    entries: VecDeque<Entry>,
    bytes: usize,
    ink: bool,
}
impl ImageCache {
    pub fn for_pack(ink: bool) -> Self {
        #[cfg(test)]
        if std::env::var_os("ALLIUM_INK_REFERENCE").is_some() {
            return Self::default();
        }
        Self {
            ink,
            ..Self::default()
        }
    }
    fn pixel_limit(&self) -> usize {
        if self.ink {
            4 * 1024 * 1024
        } else {
            PIXEL_LIMIT
        }
    }
    fn entry_limit(&self) -> usize {
        if self.ink { 32 } else { ENTRY_LIMIT }
    }

    pub fn get(&mut self, key: &ImageKey, source: &Arc<Pixmap>) -> Option<Arc<Pixmap>> {
        let identity = source_identity(source);
        let index = self
            .entries
            .iter()
            .position(|entry| entry.key == *key && entry.source == identity)?;
        let entry = self.entries.remove(index).unwrap();
        let pixels = entry.pixels.clone();
        self.entries.push_front(entry);
        Some(pixels)
    }
    pub fn reserve(&mut self, bytes: usize) {
        while !self.entries.is_empty()
            && (self.bytes + bytes > self.pixel_limit() || self.entries.len() >= self.entry_limit())
        {
            self.bytes -= self.entries.pop_back().unwrap().pixels.data().len();
        }
    }
    pub fn insert(&mut self, key: ImageKey, source: &Arc<Pixmap>, pixels: Arc<Pixmap>) {
        let bytes = pixels.data().len();
        if bytes > self.pixel_limit() {
            return;
        }
        while self.bytes + bytes > self.pixel_limit() || self.entries.len() == self.entry_limit() {
            self.bytes -= self.entries.pop_back().unwrap().pixels.data().len();
        }
        self.bytes += bytes;
        self.entries.push_front(Entry {
            key,
            source: source_identity(source),
            pixels,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(id: usize) -> ImageKey {
        ImageKey {
            generation: id as u64,
            screen: ScreenId::Library,
            component: "hero".into(),
            width: 640,
            height: 400,
            fit: Fit::Cover,
            radius: 8,
            blur: 0,
            fade_edge: None,
            fade_width: None,
            mask: 0,
            background: None,
        }
    }
    #[test]
    fn bounded_pixels_reuse_identical_content_without_retaining_sources() {
        let mut cache = ImageCache::default();
        let source = Arc::new(Pixmap::new(2, 2).unwrap());
        let other = Arc::new((*source).clone());
        for id in 0..20 {
            cache.insert(key(id), &source, Arc::new(Pixmap::new(640, 400).unwrap()));
            assert!(cache.bytes <= PIXEL_LIMIT);
            assert!(cache.entries.len() <= ENTRY_LIMIT);
        }
        assert!(cache.get(&key(19), &source).is_some());
        assert!(cache.get(&key(19), &other).is_some());
        let mut changed = (*other).clone();
        changed.fill(tiny_skia::Color::WHITE);
        assert!(cache.get(&key(19), &Arc::new(changed)).is_none());
        assert_eq!(Arc::strong_count(&source), 1);
        drop(source);
        assert!(cache.get(&key(19), &other).is_some());
    }
}
