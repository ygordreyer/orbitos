use super::*;
use std::{
    collections::VecDeque,
    sync::{Arc, Weak},
};

const BYTE_LIMIT: usize = 1024 * 1024;
const ENTRY_LIMIT: usize = 9;
pub(super) struct Key<'a> {
    pub label: &'a str,
    pub source: Option<&'a Arc<Pixmap>>,
    pub focused: bool,
    pub preview: bool,
}
struct Tile {
    label: String,
    source: Option<Weak<Pixmap>>,
    focused: bool,
    preview: bool,
    pixels: Pixmap,
}
impl Tile {
    fn matches(&self, key: &Key<'_>, rect: Rect) -> bool {
        self.label == key.label
            && self.focused == key.focused
            && self.preview == key.preview
            && self.pixels.width() == rect.2
            && self.pixels.height() == rect.3
            && match (&self.source, key.source) {
                (None, None) => true,
                (Some(a), Some(b)) => a.upgrade().is_some_and(|a| Arc::ptr_eq(&a, b)),
                _ => false,
            }
    }
    fn bytes(&self) -> usize {
        self.pixels.data().len() + self.label.capacity() + std::mem::size_of::<Self>() + 32
    }
}
#[derive(Default)]
pub(super) struct Cache {
    entries: VecDeque<Tile>,
    bytes: usize,
}
impl Cache {
    pub fn paint(&mut self, key: &Key<'_>, dest: &mut Pixmap, rect: Rect) -> bool {
        let Some(index) = self.entries.iter().position(|e| e.matches(key, rect)) else {
            return false;
        };
        let tile = self.entries.remove(index).unwrap();
        crate::perf_trace::count(crate::perf_trace::Count::RestoreBytes, rect.2 as u64 * rect.3 as u64 * 4);
        for (y, source) in tile
            .pixels
            .data()
            .chunks_exact(rect.2 as usize * 4)
            .enumerate()
        {
            let at = ((rect.1 as usize + y) * dest.width() as usize + rect.0 as usize) * 4;
            dest.data_mut()[at..at + source.len()].copy_from_slice(source);
        }
        crate::perf_trace::count(
            crate::perf_trace::Count::ComposedPixels,
            rect.2 as u64 * rect.3 as u64,
        );
        self.entries.push_front(tile);
        true
    }
    pub fn capture(&mut self, key: Key<'_>, source: &Pixmap, rect: Rect) {
        let bytes = rect.2 as usize * rect.3 as usize * 4
            + key.label.len()
            + std::mem::size_of::<Tile>()
            + 32;
        if bytes > BYTE_LIMIT {
            return;
        }
        while self.bytes + bytes > BYTE_LIMIT || self.entries.len() >= ENTRY_LIMIT {
            self.bytes -= self.entries.pop_back().unwrap().bytes();
        }
        crate::perf_trace::count(crate::perf_trace::Count::Allocations, 1);
        crate::perf_trace::count(crate::perf_trace::Count::CaptureBytes, rect.2 as u64 * rect.3 as u64 * 4);
        let mut pixels = Pixmap::new(rect.2, rect.3).unwrap();
        for (y, dest) in pixels
            .data_mut()
            .chunks_exact_mut(rect.2 as usize * 4)
            .enumerate()
        {
            let at = ((rect.1 as usize + y) * source.width() as usize + rect.0 as usize) * 4;
            dest.copy_from_slice(&source.data()[at..at + dest.len()]);
        }
        self.entries.push_front(Tile {
            label: key.label.into(),
            source: key.source.map(Arc::downgrade),
            focused: key.focused,
            preview: key.preview,
            pixels,
        });
        self.bytes += self.entries.front().unwrap().bytes();
        debug_assert!(self.bytes <= BYTE_LIMIT);
    }
    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}
