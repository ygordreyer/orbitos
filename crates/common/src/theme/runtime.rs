//! Read-only, bounded resource preparation. No legacy settings or domain writes.
use super::pack::{
    ASSET_DECODED_LIMIT, Asset, Fit, Kind, MANIFEST_LIMIT, Pack, ScreenId, valid_asset_path,
};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::File,
    io::Read,
    os::unix::{
        ffi::OsStrExt,
        io::{AsRawFd, FromRawFd},
    },
    path::{Component, Path},
    sync::Arc,
};
use tiny_skia::Pixmap;

#[cfg(test)]
thread_local! {
    pub(super) static REVIEW_COUNTS: std::cell::Cell<[usize; 4]> = const { std::cell::Cell::new([0; 4]) };
}
#[cfg(test)]
pub(super) fn review_count(index: usize) {
    REVIEW_COUNTS.with(|c| {
        let mut counts = c.get();
        counts[index] += 1;
        c.set(counts);
    });
}
#[derive(Clone)]
pub struct ThemeSnapshot(pub(super) Arc<ResolvedTheme>);
pub struct ResolvedTheme {
    pub pack: Pack,
    pub digest: String,
    pub settings: super::settings::Values,
    pub prepared_assets: BTreeMap<ScreenId, BTreeMap<String, Pixmap>>,
    pub decoded_bytes: u64,
    pub image_masks: BTreeMap<ScreenId, BTreeMap<String, Pixmap>>,
    pub(super) image_cache: std::sync::Mutex<super::image_cache::ImageCache>,
    pub(super) preview_cache: std::sync::Mutex<Option<PreviewCache>>,
}
pub(super) struct PreviewCache {
    pub generation: u64,
    pub screen: ScreenId,
    pub component: String,
    pub source: Arc<Pixmap>,
    pub pixels: Arc<Pixmap>,
}
/// Destination-only separable box blur, two bounded RGBA buffers at peak.
pub(super) fn blur(image: &mut Pixmap, radius: u32) -> Result<()> {
    ensure!(
        radius <= 4 && image.width() <= 640 && image.height() <= 480,
        "Blur bounds exceeded"
    );
    if radius == 0 {
        return Ok(());
    }
    let w = image.width() as usize;
    let h = image.height() as usize;
    let r = radius as usize;
    let stride = w * 4;
    let mut scratch = vec![0u8; image.data().len()];
    for (input, output) in image
        .data()
        .chunks_exact(stride)
        .zip(scratch.chunks_exact_mut(stride))
    {
        let mut sums = [0u16; 4];
        for pixel in input.chunks_exact(4).take(r + 1) {
            for (sum, value) in sums.iter_mut().zip(pixel) {
                *sum += u16::from(*value);
            }
        }
        for (x, pixel) in output.chunks_exact_mut(4).enumerate() {
            let count = ((x + r).min(w - 1) - x.saturating_sub(r) + 1) as u16;
            let reciprocal = super::runtime_neon::BLUR_RECIPROCAL[count as usize];
            for (value, sum) in pixel.iter_mut().zip(sums) {
                *value = ((u32::from(sum + count / 2) * reciprocal) >> 16) as u8;
            }
            if x >= r {
                for (sum, value) in sums.iter_mut().zip(&input[(x - r) * 4..][..4]) {
                    *sum -= u16::from(*value);
                }
            }
            if x + r + 1 < w {
                for (sum, value) in sums.iter_mut().zip(&input[(x + r + 1) * 4..][..4]) {
                    *sum += u16::from(*value);
                }
            }
        }
    }
    let mut sums = vec![0u16; stride];
    for row in scratch.chunks_exact(stride).take(r + 1) {
        for (sum, value) in sums.iter_mut().zip(row) {
            *sum += u16::from(*value);
        }
    }
    for (y, output) in image.data_mut().chunks_exact_mut(stride).enumerate() {
        let add = if y > 0 && y + r < h {
            &scratch[(y + r) * stride..][..stride]
        } else {
            &[]
        };
        let remove = if y > r {
            &scratch[(y - r - 1) * stride..][..stride]
        } else {
            &[]
        };
        let count = ((y + r).min(h - 1) - y.saturating_sub(r) + 1) as u16;
        super::runtime_neon::blur_row(&mut sums, add, remove, count, output);
    }
    Ok(())
}
pub(super) fn alpha_mask(image: &mut Pixmap, mask: &Pixmap) -> Result<()> {
    ensure!(
        image.width() == mask.width() && image.height() == mask.height(),
        "Mask dimensions mismatch"
    );
    for (pixel, alpha) in image
        .data_mut()
        .chunks_exact_mut(4)
        .zip(mask.data().chunks_exact(4))
    {
        for c in pixel {
            *c = ((u16::from(*c) * u16::from(alpha[3]) + 127) / 255) as u8;
        }
    }
    Ok(())
}
pub const PNG_DECODER_LIMIT: usize = 512 * 1024;
pub const TRANSIENT_LIMIT: usize = 2 * 1024 * 1024;
pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(super) fn opened(fd: i32) -> Result<File> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful openat returns a fresh descriptor transferred to File.
    Ok(unsafe { File::from_raw_fd(fd) })
}
pub(super) fn directory(parent: &File, name: &std::ffi::OsStr) -> Result<File> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: parent is held, name is a valid C string, returned FD is owned.
    opened(unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
}
pub(super) fn anchored_root(root: &Path) -> Result<File> {
    ensure!(root.is_absolute(), "Theme root must be absolute");
    let mut held = File::open("/")?;
    for component in root.components().skip(1) {
        ensure!(
            matches!(component, Component::Normal(_)),
            "Unsafe theme root"
        );
        held = directory(&held, component.as_os_str())?;
    }
    Ok(held)
}
/// Named ancestor identities are retained for the lifetime of the binding.
/// Held descriptors prevent redirection; a detected replacement latches failure.
pub(super) struct RootBinding {
    path: std::path::PathBuf,
    identities: Vec<(u64, u64)>,
    failed: std::sync::atomic::AtomicBool,
}
impl RootBinding {
    pub(super) fn bind(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.into(),
            identities: Self::observe(path)?,
            failed: std::sync::atomic::AtomicBool::new(false),
        })
    }
    fn observe(path: &Path) -> Result<Vec<(u64, u64)>> {
        use std::os::unix::fs::MetadataExt;
        ensure!(path.is_absolute(), "Theme root must be absolute");
        let mut held = File::open("/")?;
        let mut ids = Vec::new();
        for component in path.components().skip(1) {
            ensure!(
                matches!(component, Component::Normal(_)),
                "Unsafe theme root"
            );
            held = directory(&held, component.as_os_str())?;
            let m = held.metadata()?;
            ids.push((m.dev(), m.ino()));
        }
        Ok(ids)
    }
    pub(super) fn verify_held(&self, held: &File) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        self.check()?;
        let m = held.metadata()?;
        ensure!(
            self.identities.last().copied() == Some((m.dev(), m.ino())),
            "Theme binding changed while opening"
        );
        Ok(())
    }
    pub(super) fn check(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        ensure!(
            !self.failed.load(Ordering::Acquire),
            "Theme root binding failed; rebind explicitly"
        );
        if Self::observe(&self.path).is_ok_and(|ids| ids == self.identities) {
            return Ok(());
        }
        self.failed.store(true, Ordering::Release);
        anyhow::bail!("Theme named ancestor changed; rebind explicitly")
    }
}

pub(super) fn bounded_file(root: &File, path: &str, limit: usize) -> Result<File> {
    ensure!(valid_asset_path(path), "Unsafe theme resource path");
    let mut parent = root.try_clone()?;
    let parts: Vec<_> = path.split('/').collect();
    for p in &parts[..parts.len() - 1] {
        parent = directory(&parent, std::ffi::OsStr::new(p))?;
    }
    let name = CString::new(parts[parts.len() - 1])?;
    // Nonblocking prevents a malicious FIFO from blocking before regular-file validation.
    let file = opened(unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    })?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.len() <= limit as u64,
        "Invalid/oversized theme resource"
    );
    Ok(file)
}

pub(super) fn bounded_read(root: &File, path: &str, limit: usize) -> Result<Vec<u8>> {
    let mut file = bounded_file(root, path, limit)?;
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit
            && bytes.len() as u64 == before.len()
            && file.metadata()?.len() == before.len(),
        "Theme resource changed or exceeded limit"
    );
    Ok(bytes)
}
fn png(bytes: &[u8], asset: &Asset) -> Result<Pixmap> {
    #[cfg(test)]
    review_count(1);
    ensure!(
        bytes.len() as u64 == asset.bytes && digest(bytes) == asset.sha256,
        "Theme asset size/hash mismatch"
    );
    ensure!(
        bytes.len() >= 33 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" && &bytes[12..16] == b"IHDR",
        "Invalid PNG header"
    );
    let width = u32::from_be_bytes(bytes[16..20].try_into()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into()?);
    ensure!(
        width == asset.width
            && height == asset.height
            && width > 0
            && width <= 640
            && height > 0
            && height <= 480,
        "PNG dimensions mismatch/oversized"
    );
    validate_png_chunks(bytes)?;
    ensure!(
        bytes.len() + PNG_DECODER_LIMIT <= TRANSIENT_LIMIT,
        "PNG transient budget exceeded"
    );
    let mut decoder = png::Decoder::new_with_limits(
        std::io::Cursor::new(bytes),
        png::Limits {
            bytes: PNG_DECODER_LIMIT,
        },
    );
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_ignore_text_chunk(true);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let rgba_len = (width * height * 4) as usize;
    let output_len = reader
        .output_buffer_size()
        .ok_or_else(|| anyhow::anyhow!("Invalid PNG output"))?;
    ensure!(
        output_len <= rgba_len,
        "PNG output exceeds declared source pixels"
    );
    // Allocate one final RGBA buffer; expand other color types backwards in place.
    let mut data = vec![0; rgba_len];
    let info = reader.next_frame(&mut data)?;
    #[cfg(feature = "perf-bench")]
    crate::perf::hit(crate::perf::Op::Decode);
    reader.finish()?;
    ensure!(
        info.width == width && info.height == height && info.bit_depth == png::BitDepth::Eight,
        "PNG frame dimensions/depth mismatch"
    );
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        _ => anyhow::bail!("Unsupported PNG color type"),
    };
    ensure!(
        info.buffer_size() == (width * height) as usize * channels,
        "PNG output size mismatch"
    );
    for i in (0..(width * height) as usize).rev() {
        let at = i * channels;
        let r = data[at];
        let (g, b, a) = match channels {
            4 => (data[at + 1], data[at + 2], data[at + 3]),
            3 => (data[at + 1], data[at + 2], 255),
            2 => (r, r, data[at + 1]),
            _ => (r, r, 255),
        };
        let premultiply = |v: u8| ((u16::from(v) * u16::from(a) + 127) / 255) as u8;
        data[i * 4..i * 4 + 4].copy_from_slice(&[
            premultiply(r),
            premultiply(g),
            premultiply(b),
            a,
        ]);
    }
    Pixmap::from_vec(data, tiny_skia::IntSize::from_wh(width, height).unwrap())
        .ok_or_else(|| anyhow::anyhow!("Invalid PNG pixels"))
}
fn validate_png_chunks(bytes: &[u8]) -> Result<()> {
    let mut offset = 8;
    let mut ihdr = false;
    let mut idat = false;
    let mut ended = false;
    while offset < bytes.len() {
        ensure!(bytes.len() - offset >= 12, "Truncated PNG chunk");
        let len = u32::from_be_bytes(bytes[offset..offset + 4].try_into()?) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|v| v.checked_add(len))
            .ok_or_else(|| anyhow::anyhow!("PNG chunk overflow"))?;
        ensure!(end <= bytes.len(), "PNG chunk beyond file");
        let kind = &bytes[offset + 4..offset + 8];
        match kind {
            b"IHDR" => {
                ensure!(!ihdr && offset == 8 && len == 13, "Invalid PNG IHDR");
                ihdr = true;
            }
            b"PLTE" | b"tRNS" => ensure!(ihdr && !idat, "PNG palette/transparency ordering"),
            b"IDAT" => {
                ensure!(ihdr && !ended, "Invalid PNG IDAT");
                idat = true;
            }
            b"IEND" => {
                ensure!(
                    ihdr && idat && len == 0 && end == bytes.len(),
                    "Invalid PNG IEND"
                );
                ended = true;
            }
            _ => anyhow::bail!("Nonessential PNG chunks unsupported in theme v1"),
        }
        offset = end;
    }
    ensure!(ended, "Missing PNG IEND");
    Ok(())
}
fn sample_axis(length: u32, source: u32, offset: f64, ratio: f64) -> Vec<Option<(u32, u32, f64)>> {
    (0..length)
        .map(|index| {
            let position = (index as f64 + 0.5 - offset) / ratio;
            if position < 0.0 || position >= source as f64 {
                return None;
            }
            let position = (position - 0.5).clamp(0.0, source as f64 - 1.0);
            let low = position.floor() as u32;
            Some((low, (low + 1).min(source - 1), position - low as f64))
        })
        .collect()
}
#[derive(Clone, Copy)]
struct FixedSample {
    low: u32,
    high: u32,
    weight: u32,
    original: f64,
    error: u32,
}
fn fixed_axis(length: u32, source: u32, offset: f64, ratio: f64) -> Vec<Option<FixedSample>> {
    use super::runtime_neon::{WEIGHT_BITS, WEIGHT_ONE};
    sample_axis(length, source, offset, ratio)
        .into_iter()
        .map(|sample| {
            sample.map(|(low, high, original)| {
                let weight = (original * f64::from(WEIGHT_ONE)).round() as u32;
                // Bilinear interpolation is 255-Lipschitz in each weight. Include
                // a margin for the oracle's floating-point arithmetic at half ties.
                let error = ((original - f64::from(weight) / f64::from(WEIGHT_ONE)).abs()
                    * 255.0
                    * f64::from(1u32 << (2 * WEIGHT_BITS)))
                .ceil() as u32
                    + 4;
                FixedSample {
                    low,
                    high,
                    weight,
                    original,
                    error,
                }
            })
        })
        .collect()
}
fn horizontal_row(source: &[u8], columns: &[Option<FixedSample>], output: &mut [u32]) {
    use super::runtime_neon::WEIGHT_ONE;
    for (column, pixel) in columns.iter().zip(output.chunks_exact_mut(4)) {
        if let Some(column) = column {
            let a = &source[column.low as usize * 4..][..4];
            let b = &source[column.high as usize * 4..][..4];
            for ((out, a), b) in pixel.iter_mut().zip(a).zip(b) {
                *out = u32::from(*a) * (WEIGHT_ONE - column.weight) + u32::from(*b) * column.weight;
            }
        }
    }
}
fn exact_sample(source: &Pixmap, column: FixedSample, row: FixedSample, channel: usize) -> u8 {
    let sample = |x, y| source.data()[((y * source.width() + x) * 4) as usize + channel] as f64;
    let top = sample(column.low, row.low) * (1.0 - column.original)
        + sample(column.high, row.low) * column.original;
    let bottom = sample(column.low, row.high) * (1.0 - column.original)
        + sample(column.high, row.high) * column.original;
    (top * (1.0 - row.original) + bottom * row.original).round() as u8
}
pub(super) fn fit_image(source: &Pixmap, w: u32, h: u32, fit: Fit) -> Result<Pixmap> {
    use super::runtime_neon::WEIGHT_BITS;
    #[cfg(feature = "perf-bench")]
    crate::perf::hit(crate::perf::Op::Fit);
    if source.width() == w && source.height() == h {
        // Owned API compatibility. Arc callers bypass this copy in ImageCache::get.
        crate::perf_trace::count(crate::perf_trace::Count::Allocations, 1);
        return Ok(source.clone());
    }
    let ratio = if fit == Fit::Cover {
        (w as f64 / source.width() as f64).max(h as f64 / source.height() as f64)
    } else {
        (w as f64 / source.width() as f64).min(h as f64 / source.height() as f64)
    };
    let mut out =
        Pixmap::new(w, h).ok_or_else(|| anyhow::anyhow!("Invalid prepared asset size"))?;
    crate::perf_trace::count(crate::perf_trace::Count::Allocations, 1);
    crate::perf_trace::count(crate::perf_trace::Count::ResizePixels, w as u64 * h as u64);
    let ox = (w as f64 - source.width() as f64 * ratio) / 2.0;
    let oy = (h as f64 - source.height() as f64 * ratio) / 2.0;
    let columns = fixed_axis(w, source.width(), ox, ratio);
    let rows = fixed_axis(h, source.height(), oy, ratio);
    let mut top = vec![0u32; w as usize * 4];
    let mut bottom = top.clone();
    let mut sums = top.clone();
    let stride = source.width() as usize * 4;
    let mut previous = None;
    let half = 1 << (2 * WEIGHT_BITS - 1);
    let mask = (1 << (2 * WEIGHT_BITS)) - 1;
    for (row, output) in rows
        .into_iter()
        .zip(out.data_mut().chunks_exact_mut(w as usize * 4))
    {
        let Some(row) = row else {
            continue;
        };
        if previous != Some((row.low, row.high)) {
            horizontal_row(
                &source.data()[row.low as usize * stride..][..stride],
                &columns,
                &mut top,
            );
            horizontal_row(
                &source.data()[row.high as usize * stride..][..stride],
                &columns,
                &mut bottom,
            );
            previous = Some((row.low, row.high));
        }
        super::runtime_neon::bilinear_row(&top, &bottom, row.weight, &mut sums);
        for ((column, pixel), sums) in columns
            .iter()
            .zip(output.chunks_exact_mut(4))
            .zip(sums.chunks_exact(4))
        {
            let Some(column) = *column else {
                continue;
            };
            for (channel, (out, sum)) in pixel.iter_mut().zip(sums).enumerate() {
                // Away from half ties, the error bound proves integer and f64
                // rounding agree. Only ambiguous bytes use the original formula.
                *out = if (sum & mask).abs_diff(half) <= column.error + row.error {
                    exact_sample(source, column, row, channel)
                } else {
                    ((sum + half) >> (2 * WEIGHT_BITS)) as u8
                };
            }
        }
    }
    Ok(out)
}
#[cfg(test)]
fn dl16_fit_image_reference(source: &Pixmap, w: u32, h: u32, fit: Fit) -> Result<Pixmap> {
    #[cfg(feature = "perf-bench")]
    crate::perf::hit(crate::perf::Op::Fit);
    if source.width() == w && source.height() == h {
        return Ok(source.clone());
    }
    // Direct bounded bilinear sampling: no source copy or oversized aspect-ratio intermediate.
    // Source is premultiplied; sample channels together to preserve that invariant.
    let ratio = if fit == Fit::Cover {
        (w as f64 / source.width() as f64).max(h as f64 / source.height() as f64)
    } else {
        (w as f64 / source.width() as f64).min(h as f64 / source.height() as f64)
    };
    let mut out =
        Pixmap::new(w, h).ok_or_else(|| anyhow::anyhow!("Invalid prepared asset size"))?;
    let ox = (w as f64 - source.width() as f64 * ratio) / 2.0;
    let oy = (h as f64 - source.height() as f64 * ratio) / 2.0;
    for y in 0..h {
        for x in 0..w {
            let sx = (x as f64 + 0.5 - ox) / ratio;
            let sy = (y as f64 + 0.5 - oy) / ratio;
            if sx >= 0.0 && sx < source.width() as f64 && sy >= 0.0 && sy < source.height() as f64 {
                let sx = (sx - 0.5).clamp(0.0, source.width() as f64 - 1.0);
                let sy = (sy - 0.5).clamp(0.0, source.height() as f64 - 1.0);
                let x0 = sx.floor() as u32;
                let y0 = sy.floor() as u32;
                let x1 = (x0 + 1).min(source.width() - 1);
                let y1 = (y0 + 1).min(source.height() - 1);
                let wx = sx - x0 as f64;
                let wy = sy - y0 as f64;
                let at = ((y * w + x) * 4) as usize;
                for channel in 0..4 {
                    let sample = |xx, yy| {
                        source.data()[((yy * source.width() + xx) * 4) as usize + channel] as f64
                    };
                    let top = sample(x0, y0) * (1.0 - wx) + sample(x1, y0) * wx;
                    let bottom = sample(x0, y1) * (1.0 - wx) + sample(x1, y1) * wx;
                    out.data_mut()[at + channel] = (top * (1.0 - wy) + bottom * wy).round() as u8;
                }
            }
        }
    }
    Ok(out)
}
pub(super) fn rounded(image: &mut Pixmap, radius: u32) {
    let radius = radius.min(image.width() / 2).min(image.height() / 2);
    if radius == 0 {
        return;
    }
    let r = radius as f32;
    let w = image.width();
    let h = image.height();
    for y in (0..radius).chain(h - radius..h) {
        for x in (0..radius).chain(w - radius..w) {
            let cx = (x as f32 + 0.5).clamp(r, w as f32 - r);
            let cy = (y as f32 + 0.5).clamp(r, h as f32 - r);
            let distance = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let coverage = (r + 0.5 - distance).clamp(0.0, 1.0);
            let at = ((y * w + x) * 4) as usize;
            for channel in &mut image.data_mut()[at..at + 4] {
                *channel = (*channel as f32 * coverage).round() as u8;
            }
        }
    }
}
impl ThemeSnapshot {
    pub fn builtin(id: &str) -> Result<Self> {
        let bytes = Pack::builtin_bytes(id)?;
        Self::prepare(bytes, |path, _| match path {
            "assets/accent-band.png" => Ok(include_bytes!("packs/accent-band.png").to_vec()),
            "assets/next-disc-mask.png" => Ok(include_bytes!("packs/next-disc-mask.png").to_vec()),
            _ => anyhow::bail!("Unknown builtin asset"),
        })
    }
    pub fn load(root: &Path) -> Result<Self> {
        let held = anchored_root(root)?;
        Self::load_held(&held, &Default::default())
    }
    pub fn load_with_settings(root: &Path, settings: &super::settings::Values) -> Result<Self> {
        Self::load_held(&anchored_root(root)?, settings)
    }
    pub(super) fn load_held(held: &File, settings: &super::settings::Values) -> Result<Self> {
        #[cfg(test)]
        review_count(0);
        let bytes = bounded_read(held, "theme.json", MANIFEST_LIMIT)?;
        Self::prepare_options(&bytes, settings, |path, limit| {
            bounded_read(held, path, limit)
        })
    }
    fn prepare(bytes: &[u8], read: impl FnMut(&str, usize) -> Result<Vec<u8>>) -> Result<Self> {
        Self::prepare_options(bytes, &Default::default(), read)
    }
    fn prepare_options(
        bytes: &[u8],
        settings: &super::settings::Values,
        mut read: impl FnMut(&str, usize) -> Result<Vec<u8>>,
    ) -> Result<Self> {
        let (pack, settings, resolved_digest) = super::settings::resolve(bytes, settings)?;
        let mut originals = BTreeMap::new();
        let mut decoded = 0;
        for (id, asset) in &pack.assets {
            let loaded = read(&asset.path, asset.bytes as usize).and_then(|b| png(&b, asset));
            match loaded {
                Ok(image) => {
                    decoded += image.data().len() as u64;
                    originals.insert(id.clone(), image);
                }
                Err(error) if asset.required => return Err(error),
                Err(_) => {} // Optional decorative art is a transparent neutral placeholder.
            }
        }
        let mut prepared_assets: BTreeMap<ScreenId, BTreeMap<String, Pixmap>> = BTreeMap::new();
        let mut image_masks: BTreeMap<ScreenId, BTreeMap<String, Pixmap>> = BTreeMap::new();
        for (&screen, spec) in &pack.screens {
            for c in &spec.components {
                if let Some(mask) = &c.mask_asset_id {
                    let source = originals
                        .get(mask)
                        .ok_or_else(|| anyhow::anyhow!("Missing required image mask"))?;
                    decoded += source.data().len() as u64;
                    ensure!(
                        decoded <= ASSET_DECODED_LIMIT,
                        "Prepared mask budget exceeded"
                    );
                    image_masks
                        .entry(screen)
                        .or_default()
                        .insert(c.id.clone(), source.clone());
                }
                if c.kind == Kind::AssetImage {
                    let size = u64::from(c.rect.2) * u64::from(c.rect.3) * 4;
                    ensure!(
                        decoded + size <= ASSET_DECODED_LIMIT,
                        "Prepared theme asset budget exceeded"
                    );
                    let mut image = match originals.get(c.asset_id.as_ref().unwrap()) {
                        Some(source) => fit_image(source, c.rect.2, c.rect.3, c.fit.unwrap())?,
                        None => Pixmap::new(c.rect.2, c.rect.3)
                            .ok_or_else(|| anyhow::anyhow!("Invalid placeholder"))?,
                    };
                    rounded(&mut image, c.radius.unwrap());
                    decoded += size;
                    prepared_assets
                        .entry(screen)
                        .or_default()
                        .insert(c.id.clone(), image);
                }
            }
        }
        // Source pixmaps drop here. Accounting conservatively includes their preparation peak.
        let image_cache = super::image_cache::ImageCache::for_pack(pack.ink());
        Ok(Self(Arc::new(ResolvedTheme {
            pack,
            digest: resolved_digest,
            settings,
            prepared_assets,
            decoded_bytes: decoded,
            image_masks,
            image_cache: std::sync::Mutex::new(image_cache),
            preview_cache: std::sync::Mutex::new(None),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn theme_runtime_oversized_iccp_rejected_before_decoder_and_bounded_fit() {
        let bytes = include_bytes!("packs/icc-limit-fixture.png");
        let asset = Asset {
            path: "fixture.png".into(),
            sha256: digest(bytes),
            width: 32,
            height: 8,
            bytes: bytes.len() as u64,
            required: true,
        };
        let error = png(bytes, &asset).unwrap_err();
        assert!(error.to_string().contains("Nonessential PNG chunks"));
        assert!(1_048_576 + PNG_DECODER_LIMIT <= TRANSIENT_LIMIT);
        let mut source = Pixmap::new(1, 480).unwrap();
        source.fill(tiny_skia::Color::from_rgba8(8, 184, 216, 255));
        let output = fit_image(&source, 640, 1, Fit::Cover).unwrap();
        assert_eq!(output.data().len(), 2560);
        let mut p = Pack::builtin("orbit").unwrap();
        let c = p.screens[&ScreenId::Library]
            .components
            .last()
            .unwrap()
            .clone();
        for i in 0..4 {
            let mut extra = c.clone();
            extra.id = format!("large-{i}");
            extra.rect = super::super::pack::Rect(0, 0, 640, 480);
            p.screens
                .get_mut(&ScreenId::Library)
                .unwrap()
                .components
                .push(extra);
        }
        assert!(
            ThemeSnapshot::prepare(&serde_json::to_vec(&p).unwrap(), |_, _| Ok(include_bytes!(
                "packs/accent-band.png"
            )
            .to_vec()))
            .is_err()
        );
    }
    #[test]
    fn theme_runtime_color_expansion_one_rgba_buffer() {
        for (color, pixels, expected) in [
            (
                png::ColorType::Rgb,
                vec![8, 184, 216],
                vec![8, 184, 216, 255],
            ),
            (
                png::ColorType::Grayscale,
                vec![128],
                vec![128, 128, 128, 255],
            ),
            (
                png::ColorType::GrayscaleAlpha,
                vec![128, 64],
                vec![32, 32, 32, 64],
            ),
            (
                png::ColorType::Rgba,
                vec![128, 0, 0, 64],
                vec![32, 0, 0, 64],
            ),
        ] {
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
                encoder.set_color(color);
                encoder.set_depth(png::BitDepth::Eight);
                encoder
                    .write_header()
                    .unwrap()
                    .write_image_data(&pixels)
                    .unwrap();
            }
            let asset = Asset {
                path: "fixture.png".into(),
                sha256: digest(&bytes),
                width: 1,
                height: 1,
                bytes: bytes.len() as u64,
                required: true,
            };
            assert_eq!(png(&bytes, &asset).unwrap().data(), expected);
        }
    }
    #[test]
    fn theme_runtime_builtin_asset_prepared_and_hash() {
        for id in ["orbit", "compact"] {
            let s = ThemeSnapshot::builtin(id).unwrap();
            assert_eq!(s.0.digest, digest(Pack::builtin_bytes(id).unwrap()));
            if id == "orbit" {
                assert!(
                    s.0.pack
                        .component(ScreenId::Library, "accent-band")
                        .is_none()
                );
                assert!(!s.0.prepared_assets.contains_key(&ScreenId::Library));
            } else {
                let a = &s.0.prepared_assets[&ScreenId::Library]["accent-band"];
                assert_eq!((a.width(), a.height()), (308, 4));
            }
            assert!(s.0.decoded_bytes <= ASSET_DECODED_LIMIT);
        }
    }
    #[test]
    fn theme_runtime_bad_png_hash_dimensions_and_optional() {
        let a = Pack::builtin("orbit").unwrap().assets["accent-band"].clone();
        let mut bytes = include_bytes!("packs/accent-band.png").to_vec();
        bytes[20] ^= 1;
        assert!(png(&bytes, &a).is_err());
        let mut header = include_bytes!("packs/accent-band.png").to_vec();
        header[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        let mut asset = a.clone();
        asset.sha256 = digest(&header);
        assert!(png(&header, &asset).is_err());
        let bytes = Pack::builtin_bytes("orbit").unwrap();
        assert!(ThemeSnapshot::prepare(bytes, |_, _| anyhow::bail!("Missing required")).is_err());
        let mut p = Pack::builtin("orbit").unwrap();
        p.assets.get_mut("accent-band").unwrap().required = false;
        let s = ThemeSnapshot::prepare(&serde_json::to_vec(&p).unwrap(), |path, _| {
            if path == "assets/next-disc-mask.png" {
                return Ok(include_bytes!("packs/next-disc-mask.png").to_vec());
            }
            anyhow::bail!("Missing optional")
        })
        .unwrap();
        assert!(
            s.0.prepared_assets
                .values()
                .flat_map(|v| v.values())
                .all(|v| v.data().iter().all(|b| *b == 0))
        );
    }
    #[test]
    fn theme_runtime_external_load_symlink_escape_and_no_mutation() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("allium-theme-fixture-{}", std::process::id()));
        assert!(!root.exists());
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
        let _cleanup = Cleanup(root.clone());
        std::fs::create_dir(root.join("assets")).unwrap();
        std::fs::write(
            root.join("theme.json"),
            Pack::builtin_bytes("orbit").unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("assets/accent-band.png"),
            include_bytes!("packs/accent-band.png"),
        )
        .unwrap();
        std::fs::write(
            root.join("assets/next-disc-mask.png"),
            include_bytes!("packs/next-disc-mask.png"),
        )
        .unwrap();
        let before = std::fs::read(root.join("theme.json")).unwrap();
        assert!(ThemeSnapshot::load(&root).is_ok());
        assert_eq!(std::fs::read(root.join("theme.json")).unwrap(), before);
        std::fs::rename(root.join("assets"), root.join("owned-original")).unwrap();
        symlink(root.join("owned-original"), root.join("assets")).unwrap();
        assert!(ThemeSnapshot::load(&root).is_err());
        std::fs::remove_file(root.join("assets")).unwrap();
        std::fs::rename(root.join("owned-original"), root.join("assets")).unwrap();
        std::fs::rename(
            root.join("assets/accent-band.png"),
            root.join("original.png"),
        )
        .unwrap();
        symlink(
            root.join("original.png"),
            root.join("assets/accent-band.png"),
        )
        .unwrap();
        assert!(ThemeSnapshot::load(&root).is_err());
        assert_eq!(
            std::fs::read(root.join("original.png")).unwrap(),
            include_bytes!("packs/accent-band.png")
        );
    }
}

#[cfg(test)]
mod pixel_fast_path_tests {
    use super::*;
    fn reference_fit_image(source: &Pixmap, w: u32, h: u32, fit: Fit) -> Result<Pixmap> {
        // Direct bounded bilinear sampling: no source copy or oversized aspect-ratio intermediate.
        // Source is premultiplied; sample channels together to preserve that invariant.
        let ratio = if fit == Fit::Cover {
            (w as f64 / source.width() as f64).max(h as f64 / source.height() as f64)
        } else {
            (w as f64 / source.width() as f64).min(h as f64 / source.height() as f64)
        };
        let mut out =
            Pixmap::new(w, h).ok_or_else(|| anyhow::anyhow!("Invalid prepared asset size"))?;
        let ox = (w as f64 - source.width() as f64 * ratio) / 2.0;
        let oy = (h as f64 - source.height() as f64 * ratio) / 2.0;
        for y in 0..h {
            for x in 0..w {
                let sx = (x as f64 + 0.5 - ox) / ratio;
                let sy = (y as f64 + 0.5 - oy) / ratio;
                if sx >= 0.0
                    && sx < source.width() as f64
                    && sy >= 0.0
                    && sy < source.height() as f64
                {
                    let sx = (sx - 0.5).clamp(0.0, source.width() as f64 - 1.0);
                    let sy = (sy - 0.5).clamp(0.0, source.height() as f64 - 1.0);
                    let x0 = sx.floor() as u32;
                    let y0 = sy.floor() as u32;
                    let x1 = (x0 + 1).min(source.width() - 1);
                    let y1 = (y0 + 1).min(source.height() - 1);
                    let wx = sx - x0 as f64;
                    let wy = sy - y0 as f64;
                    let at = ((y * w + x) * 4) as usize;
                    for channel in 0..4 {
                        let sample = |xx, yy| {
                            source.data()[((yy * source.width() + xx) * 4) as usize + channel]
                                as f64
                        };
                        let top = sample(x0, y0) * (1.0 - wx) + sample(x1, y0) * wx;
                        let bottom = sample(x0, y1) * (1.0 - wx) + sample(x1, y1) * wx;
                        out.data_mut()[at + channel] =
                            (top * (1.0 - wy) + bottom * wy).round() as u8;
                    }
                }
            }
        }
        Ok(out)
    }
    fn reference_rounded(image: &mut Pixmap, radius: u32) {
        let radius = radius.min(image.width() / 2).min(image.height() / 2);
        if radius == 0 {
            return;
        }
        let r = radius as f32;
        let w = image.width();
        let h = image.height();
        for y in 0..h {
            for x in 0..w {
                let cx = (x as f32 + 0.5).clamp(r, w as f32 - r);
                let cy = (y as f32 + 0.5).clamp(r, h as f32 - r);
                let distance =
                    ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let coverage = (r + 0.5 - distance).clamp(0.0, 1.0);
                let at = ((y * w + x) * 4) as usize;
                for channel in &mut image.data_mut()[at..at + 4] {
                    *channel = (*channel as f32 * coverage).round() as u8;
                }
            }
        }
    }
    #[test]
    fn exact_size_and_corner_only_rounding_match_full_reference() {
        for (w, h) in [(1, 1), (2, 5), (9, 13), (140, 140), (390, 189)] {
            let mut pixels = Pixmap::new(w, h).unwrap();
            for (i, byte) in pixels.data_mut().iter_mut().enumerate() {
                *byte = (i * 73 + i / 7) as u8;
            }
            for fit in [Fit::Contain, Fit::Cover] {
                assert_eq!(
                    fit_image(&pixels, w, h, fit).unwrap().data(),
                    reference_fit_image(&pixels, w, h, fit).unwrap().data()
                );
            }
            for radius in [0, 1, 2, 8, 32, 640] {
                let mut actual = pixels.clone();
                let mut expected = pixels.clone();
                rounded(&mut actual, radius);
                reference_rounded(&mut expected, radius);
                assert_eq!(actual.data(), expected.data(), "{w}x{h} radius {radius}");
            }
        }
    }
}

#[cfg(test)]
mod dl17_sampling_tests {
    use super::*;
    #[test]
    fn axis_sampling_matches_dl16_bilinear_bytes() {
        let mut seed = 17u32;
        let mut cases = vec![
            (360, 210, 390, 164),
            (140, 140, 140, 140),
            (1, 480, 640, 1),
            (17, 1, 1, 480),
        ];
        for _ in 0..48 {
            let mut next = || {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                seed
            };
            cases.push((
                next() % 128 + 1,
                next() % 128 + 1,
                next() % 160 + 1,
                next() % 160 + 1,
            ));
        }
        for (sw, sh, w, h) in cases {
            let mut image = Pixmap::new(sw, sh).unwrap();
            for pixel in image.data_mut().chunks_exact_mut(4) {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let alpha = (seed >> 24) as u8;
                pixel.copy_from_slice(&[
                    (seed as u8).min(alpha),
                    ((seed >> 8) as u8).min(alpha),
                    ((seed >> 16) as u8).min(alpha),
                    alpha,
                ]);
            }
            for fit in [Fit::Cover, Fit::Contain] {
                let actual = fit_image(&image, w, h, fit).unwrap();
                let reference = dl16_fit_image_reference(&image, w, h, fit).unwrap();
                assert!(
                    actual.data() == reference.data(),
                    "{sw}x{sh} to {w}x{h} {fit:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod resize_tests {
    use super::*;
    fn legacy_fit_image(source: &Pixmap, w: u32, h: u32, fit: Fit) -> Result<Pixmap> {
        #[cfg(feature = "perf-bench")]
        crate::perf::hit(crate::perf::Op::Fit);
        if source.width() == w && source.height() == h {
            return Ok(source.clone());
        }
        // Direct bounded bilinear sampling: no source copy or oversized aspect-ratio intermediate.
        // Source is premultiplied; sample channels together to preserve that invariant.
        let ratio = if fit == Fit::Cover {
            (w as f64 / source.width() as f64).max(h as f64 / source.height() as f64)
        } else {
            (w as f64 / source.width() as f64).min(h as f64 / source.height() as f64)
        };
        let mut out =
            Pixmap::new(w, h).ok_or_else(|| anyhow::anyhow!("Invalid prepared asset size"))?;
        let ox = (w as f64 - source.width() as f64 * ratio) / 2.0;
        let oy = (h as f64 - source.height() as f64 * ratio) / 2.0;
        let columns = sample_axis(w, source.width(), ox, ratio);
        let rows = sample_axis(h, source.height(), oy, ratio);
        for (y, row) in rows.into_iter().enumerate() {
            let Some((y0, y1, wy)) = row else {
                continue;
            };
            for (x, column) in columns.iter().enumerate() {
                if let Some((x0, x1, wx)) = *column {
                    let at = (y * w as usize + x) * 4;
                    for channel in 0..4 {
                        let sample = |xx, yy| {
                            source.data()[((yy * source.width() + xx) * 4) as usize + channel]
                                as f64
                        };
                        let top = sample(x0, y0) * (1.0 - wx) + sample(x1, y0) * wx;
                        let bottom = sample(x0, y1) * (1.0 - wx) + sample(x1, y1) * wx;
                        out.data_mut()[at + channel] =
                            (top * (1.0 - wy) + bottom * wy).round() as u8;
                    }
                }
            }
        }
        Ok(out)
    }
    fn legacy_blur(image: &mut Pixmap, radius: u32) -> Result<()> {
        ensure!(
            radius <= 4 && image.width() <= 640 && image.height() <= 480,
            "Blur bounds exceeded"
        );
        if radius == 0 {
            return Ok(());
        }
        let w = image.width() as usize;
        let h = image.height() as usize;
        let r = radius as usize;
        let mut scratch = vec![0u8; image.data().len()];
        for y in 0..h {
            for x in 0..w {
                for channel in 0..4 {
                    let lo = x.saturating_sub(r);
                    let hi = (x + r).min(w - 1);
                    let sum: u32 = (lo..=hi)
                        .map(|xx| image.data()[(y * w + xx) * 4 + channel] as u32)
                        .sum();
                    scratch[(y * w + x) * 4 + channel] =
                        ((sum + (hi - lo + 1) as u32 / 2) / (hi - lo + 1) as u32) as u8;
                }
            }
        }
        for y in 0..h {
            for x in 0..w {
                for channel in 0..4 {
                    let lo = y.saturating_sub(r);
                    let hi = (y + r).min(h - 1);
                    let sum: u32 = (lo..=hi)
                        .map(|yy| scratch[(yy * w + x) * 4 + channel] as u32)
                        .sum();
                    image.data_mut()[(y * w + x) * 4 + channel] =
                        ((sum + (hi - lo + 1) as u32 / 2) / (hi - lo + 1) as u32) as u8;
                }
            }
        }
        Ok(())
    }
    fn source(w: u32, h: u32, mut seed: u32) -> Pixmap {
        let mut image = Pixmap::new(w, h).unwrap();
        for (i, pixel) in image.data_mut().chunks_exact_mut(4).enumerate() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let alpha = [0, 1, 127, 128, 254, 255, (seed >> 24) as u8][i % 7];
            pixel.copy_from_slice(&[
                (seed as u8).min(alpha),
                ((seed >> 8) as u8).min(alpha),
                ((seed >> 16) as u8).min(alpha),
                alpha,
            ]);
        }
        image
    }
    fn raw_fixed(source: &Pixmap, w: u32, h: u32, fit: Fit) -> Pixmap {
        use super::super::runtime_neon::WEIGHT_BITS;
        let ratio = if fit == Fit::Cover {
            (w as f64 / source.width() as f64).max(h as f64 / source.height() as f64)
        } else {
            (w as f64 / source.width() as f64).min(h as f64 / source.height() as f64)
        };
        let columns = fixed_axis(
            w,
            source.width(),
            (w as f64 - source.width() as f64 * ratio) / 2.0,
            ratio,
        );
        let rows = fixed_axis(
            h,
            source.height(),
            (h as f64 - source.height() as f64 * ratio) / 2.0,
            ratio,
        );
        let mut image = Pixmap::new(w, h).unwrap();
        let mut top = vec![0; w as usize * 4];
        let mut bottom = top.clone();
        let mut sums = top.clone();
        let stride = source.width() as usize * 4;
        for (row, output) in rows
            .into_iter()
            .zip(image.data_mut().chunks_exact_mut(w as usize * 4))
        {
            let Some(row) = row else {
                continue;
            };
            horizontal_row(
                &source.data()[row.low as usize * stride..][..stride],
                &columns,
                &mut top,
            );
            horizontal_row(
                &source.data()[row.high as usize * stride..][..stride],
                &columns,
                &mut bottom,
            );
            super::super::runtime_neon::bilinear_row(&top, &bottom, row.weight, &mut sums);
            for (out, sum) in output.iter_mut().zip(&sums) {
                *out = ((sum + (1 << (2 * WEIGHT_BITS - 1))) >> (2 * WEIGHT_BITS)) as u8;
            }
        }
        image
    }
    // Area averaging changes the established bilinear output. Keep this integer
    // candidate test-only until the renderer's pixel contract explicitly changes.
    fn area_average(source: &Pixmap, w: u32, h: u32) -> Pixmap {
        let sw = source.width();
        let sh = source.height();
        assert!(sw >= 2 * w && sh >= 2 * h);
        let footprints = |source: u32, target: u32| -> Vec<Vec<(u32, u32)>> {
            (0..target)
                .map(|i| {
                    let low = i * source;
                    let high = (i + 1) * source;
                    (low / target..high.div_ceil(target))
                        .map(|s| (s, high.min((s + 1) * target) - low.max(s * target)))
                        .collect()
                })
                .collect()
        };
        let columns = footprints(sw, w);
        let rows = footprints(sh, h);
        let divisor = u64::from(sw) * u64::from(sh);
        let mut image = Pixmap::new(w, h).unwrap();
        for (row, output) in rows
            .iter()
            .zip(image.data_mut().chunks_exact_mut(w as usize * 4))
        {
            for (column, pixel) in columns.iter().zip(output.chunks_exact_mut(4)) {
                let mut sums = [0u64; 4];
                for &(y, wy) in row {
                    for &(x, wx) in column {
                        let sample = &source.data()[((y * sw + x) * 4) as usize..][..4];
                        for (sum, value) in sums.iter_mut().zip(sample) {
                            *sum += u64::from(*value) * u64::from(wx) * u64::from(wy);
                        }
                    }
                }
                for (out, sum) in pixel.iter_mut().zip(sums) {
                    *out = ((sum + divisor / 2) / divisor) as u8;
                }
            }
        }
        image
    }
    fn max_difference(a: &Pixmap, b: &Pixmap) -> u8 {
        a.data()
            .iter()
            .zip(b.data())
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap()
    }
    fn slots() -> Vec<(u32, u32, u32, u32)> {
        vec![
            (480, 480, 176, 176),
            (640, 480, 592, 176),
            (320, 480, 104, 156),
            (320, 480, 176, 224),
            (320, 480, 168, 252),
            (320, 480, 176, 264),
            (640, 480, 640, 200),
            (640, 480, 640, 240),
            (640, 480, 192, 408),
            (1, 480, 640, 1),
            (480, 1, 1, 480),
            (1, 1, 17, 31),
        ]
    }
    #[test]
    fn resize_exact_bytes_and_candidate_differences() {
        let mut cases = slots();
        let mut seed = 19u32;
        for _ in 0..160 {
            let mut next = || {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                seed
            };
            cases.push((
                next() % 65 + 1,
                next() % 65 + 1,
                next() % 79 + 1,
                next() % 79 + 1,
            ));
        }
        let mut raw_max = 0;
        for (sw, sh, w, h) in cases {
            let source = source(sw, sh, 17);
            for fit in [Fit::Cover, Fit::Contain] {
                let expected = legacy_fit_image(&source, w, h, fit).unwrap();
                let actual = fit_image(&source, w, h, fit).unwrap();
                assert_eq!(
                    max_difference(&actual, &expected),
                    0,
                    "{sw}x{sh} to {w}x{h} {fit:?}"
                );
                raw_max = raw_max.max(max_difference(&raw_fixed(&source, w, h, fit), &expected));
                for pixel in actual.data().chunks_exact(4) {
                    assert!(pixel[..3].iter().all(|v| *v <= pixel[3]));
                }
            }
        }
        assert!(raw_max <= 1);
        let input = source(480, 480, 17);
        let area = area_average(&input, 176, 176);
        let expected = legacy_fit_image(&input, 176, 176, Fit::Cover).unwrap();
        let area_max = max_difference(&area, &expected);
        assert!(area_max > 1);
        let constant = {
            let mut p = Pixmap::new(480, 480).unwrap();
            p.fill(tiny_skia::Color::from_rgba8(51, 94, 137, 255));
            p
        };
        assert_eq!(
            max_difference(
                &area_average(&constant, 176, 176),
                &legacy_fit_image(&constant, 176, 176, Fit::Cover).unwrap()
            ),
            0
        );
        eprintln!(
            "Q12 max per-channel difference={raw_max}; corrected production=0; area-average max per-channel difference={area_max} (seed=17 premultiplied noise, 480x480 to 176x176)"
        );
    }
    #[test]
    fn sliding_blur_exact_bytes_at_edges_and_all_radii() {
        for (w, h) in [
            (1, 1),
            (1, 17),
            (17, 1),
            (7, 8),
            (8, 7),
            (15, 16),
            (16, 15),
            (17, 31),
            (192, 408),
        ] {
            for radius in 0..=4 {
                let mut actual = source(w, h, 29);
                let mut expected = actual.clone();
                blur(&mut actual, radius).unwrap();
                legacy_blur(&mut expected, radius).unwrap();
                assert_eq!(actual.data(), expected.data(), "{w}x{h} radius={radius}");
            }
        }
        assert!(blur(&mut source(17, 17, 1), 5).is_err());
        assert!(blur(&mut source(641, 1, 1), 1).is_err());
    }
    fn milliseconds(mut run: impl FnMut(), count: usize) -> f64 {
        for _ in 0..3 {
            run();
        }
        let start = std::time::Instant::now();
        for _ in 0..count {
            run();
        }
        start.elapsed().as_secs_f64() * 1000.0 / count as f64
    }
    #[test]
    #[ignore = "offline host microbenchmark; run with --release --ignored --nocapture"]
    fn resize_bench() {
        use std::hint::black_box;
        for (sw, sh, w, h) in slots().into_iter().take(6) {
            let source = source(sw, sh, 17);
            for fit in [Fit::Cover, Fit::Contain] {
                let before = milliseconds(
                    || {
                        black_box(legacy_fit_image(black_box(&source), w, h, fit).unwrap());
                    },
                    80,
                );
                let after = milliseconds(
                    || {
                        black_box(fit_image(black_box(&source), w, h, fit).unwrap());
                    },
                    80,
                );
                eprintln!(
                    "resize {sw}x{sh} to {w}x{h} {fit:?}: before={before:.6} ms after={after:.6} ms"
                );
            }
        }
        let input = source(480, 480, 17);
        let area = milliseconds(
            || {
                black_box(area_average(black_box(&input), 176, 176));
            },
            80,
        );
        eprintln!("test-only area-average 480x480 to 176x176: {area:.6} ms");
        let input = source(192, 408, 17);
        let before = milliseconds(
            || {
                let mut p = input.clone();
                legacy_blur(black_box(&mut p), 2).unwrap();
                black_box(p);
            },
            80,
        );
        let after = milliseconds(
            || {
                let mut p = input.clone();
                blur(black_box(&mut p), 2).unwrap();
                black_box(p);
            },
            80,
        );
        eprintln!(
            "blur 192x408 radius=2 (includes identical input clone): before={before:.6} ms after={after:.6} ms"
        );
    }
}

#[cfg(feature = "perf-bench")]
#[doc(hidden)]
pub fn benchmark_fit(source: &Pixmap, w: u32, h: u32) -> Result<Pixmap> {
    fit_image(source, w, h, Fit::Cover)
}
#[cfg(feature = "perf-bench")]
#[doc(hidden)]
pub fn benchmark_blur(image: &mut Pixmap, radius: u32) -> Result<()> {
    blur(image, radius)
}
