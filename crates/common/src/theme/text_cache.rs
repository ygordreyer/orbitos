use super::{
    fonts::{PreparedTheme, ThemeHandle},
    pack::{Align, Overflow, Rect, Role},
};
use crate::display::color::Color;
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::VecDeque,
    sync::{Arc, Weak},
};
use tiny_skia::Pixmap;

const BYTE_LIMIT: usize = 512 * 1024;
const ENTRY_LIMIT: usize = 64;
#[derive(PartialEq, Eq, Clone)]
pub(super) struct Key<'a> {
    generation: u64,
    role: Role,
    color: Color,
    text: Cow<'a, str>,
    width: u32,
    height: u32,
    lines: u32,
    align: Align,
    overflow: Overflow,
}
impl<'a> Key<'a> {
    pub fn text_value(&self) -> &str {
        &self.text
    }
    pub fn new(
        theme: &ThemeHandle,
        role: Role,
        color: Color,
        text: &'a str,
        rect: Rect,
        lines: u32,
        align: Align,
        overflow: Overflow,
    ) -> Self {
        Self {
            generation: theme.generation(),
            role,
            color,
            text: text.into(),
            width: rect.2,
            height: rect.3,
            lines,
            align,
            overflow,
        }
    }
}
struct Entry {
    theme: Weak<PreparedTheme>,
    key: Key<'static>,
    pixels: Arc<Pixmap>,
    map: Arc<super::blit::BlitMap>,
}
impl Entry {
    fn bytes(&self) -> usize {
        self.pixels.data().len()
            + self.key.text.len()
            + self.map.bytes()
            + std::mem::size_of::<Self>()
            + 64
    }
}
#[derive(Default)]
struct Cache {
    entries: VecDeque<Entry>,
    bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}
impl Cache {
    fn get(
        &mut self,
        theme: &Weak<PreparedTheme>,
        key: &Key<'_>,
    ) -> Option<(Arc<Pixmap>, Arc<super::blit::BlitMap>)> {
        let at = self.entries.iter().position(|entry| {
            entry.theme.ptr_eq(theme) && entry.theme.strong_count() > 0 && entry.key == *key
        });
        let Some(at) = at else {
            self.misses += 1;
            crate::perf_trace::count(crate::perf_trace::Count::TextMiss, 1);
            return None;
        };
        self.hits += 1;
        crate::perf_trace::count(crate::perf_trace::Count::TextHit, 1);
        let entry = self.entries.remove(at).unwrap();
        let pixels = (entry.pixels.clone(), entry.map.clone());
        self.entries.push_front(entry);
        Some(pixels)
    }
    fn reserve(&mut self, bytes: usize) {
        while !self.entries.is_empty()
            && (self.bytes + bytes > BYTE_LIMIT || self.entries.len() >= ENTRY_LIMIT)
        {
            self.bytes -= self.entries.pop_back().unwrap().bytes();
            self.evictions += 1;
            crate::perf_trace::count(crate::perf_trace::Count::TextEviction, 1);
        }
    }
    fn insert(&mut self, entry: Entry) {
        let bytes = entry.bytes();
        if bytes > BYTE_LIMIT {
            return;
        }
        self.reserve(bytes);
        self.bytes += bytes;
        self.entries.push_front(entry);
    }
}
pub(super) fn clear() { CACHE.with(|c| *c.borrow_mut() = Cache::default()); }
thread_local! { static CACHE: RefCell<Cache> = RefCell::new(Cache::default()); }
pub(super) fn get(
    theme: &ThemeHandle,
    key: &Key<'_>,
) -> Option<(Arc<Pixmap>, Arc<super::blit::BlitMap>)> {
    CACHE.with(|c| c.borrow_mut().get(&theme.weak(), key))
}
pub(super) fn reserve(width: u32, height: u32, text: &str) {
    CACHE.with(|c| {
        c.borrow_mut().reserve(
            width as usize * height as usize * 4
                + super::blit::MAP_LIMIT
                + text.len()
                + std::mem::size_of::<Entry>()
                + 64,
        )
    });
}
pub(super) fn insert(
    theme: &ThemeHandle,
    key: Key<'_>,
    pixels: Arc<Pixmap>,
) -> Arc<super::blit::BlitMap> {
    let map = Arc::new(super::blit::BlitMap::new(pixels.as_ref().as_ref()));
    let key = Key {
        generation: key.generation,
        role: key.role,
        color: key.color,
        text: Cow::Owned(key.text.into_owned()),
        width: key.width,
        height: key.height,
        lines: key.lines,
        align: key.align,
        overflow: key.overflow,
    };
    CACHE.with(|c| {
        c.borrow_mut().insert(Entry {
            theme: theme.weak(),
            key,
            pixels,
            map: map.clone(),
        })
    });
    map
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dl15_two_title_working_set_fits() {
        assert!(BYTE_LIMIT >= 298_912 + 32 * 1024);
    }
    #[test]
    fn dl12_text_cache_bounds_and_key_fields() {
        let mut cache = Cache::default();
        let key = Key {
            generation: 1,
            role: Role::Title,
            color: Color::new(3, 4, 5),
            text: "hello".into(),
            width: 200,
            height: 40,
            lines: 1,
            align: Align::Left,
            overflow: Overflow::Wrap,
        };
        for _ in 0..100 {
            cache.insert(Entry {
                theme: Weak::new(),
                key: key.clone(),
                pixels: Arc::new(Pixmap::new(200, 40).unwrap()),
                map: Arc::new(Default::default()),
            });
            assert!(cache.bytes <= BYTE_LIMIT);
            assert!(cache.entries.len() <= ENTRY_LIMIT);
        }
        assert!(cache.get(&Weak::new(), &key).is_none());
        let mut changed = key.clone();
        changed.width += 1;
        assert!(changed != key);
        changed = key.clone();
        changed.generation += 1;
        assert!(changed != key);
        changed = key.clone();
        changed.text.to_mut().push('!');
        assert!(changed != key);
    }
}

#[cfg(test)]
pub(super) fn stats() -> (u64, u64, u64, usize) {
    CACHE.with(|c| {
        let c = c.borrow();
        (c.hits, c.misses, c.evictions, c.bytes)
    })
}
