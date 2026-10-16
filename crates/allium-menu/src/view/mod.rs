mod guide_selector;
pub mod ingame_menu;
mod text_reader;

fn guide_shell(input: &mut common::theme::frame::FrameInputs, locale: &common::locale::Locale) {
    use common::theme::{
        frame::Row,
        pack::{Binding, NavGroup},
    };
    input.navigation.insert(
        NavGroup::Primary,
        [
            locale.t("tab-recents"),
            "Library".into(),
            locale.t("tab-apps"),
            locale.t("tab-settings"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, label)| Row {
            id: format!("tab-{index}"),
            label,
            subtitle: String::new(),
            value: String::new(),
            enabled: true,
        })
        .collect(),
    );
    input.selected.insert(Binding::Navigation, "tab-2".into());
}

fn game_context(
    input: &mut common::theme::frame::FrameInputs,
    res: &common::resources::Resources,
    theme: &common::theme::ThemeHandle,
) {
    use common::theme::pack::Binding;
    let resources = res.0.borrow();
    let Some(game) = resources.get::<common::game_info::GameInfo>() else {
        return;
    };
    let tall = theme
        .pack()
        .alignment
        .as_ref()
        .is_some_and(|alignment| alignment.cover_shape == "tall");
    let kind = if tall {
        common::media::Kind::TallCover
    } else {
        common::media::Kind::Cover
    };
    input.art.selected_cover = COVER.with_borrow_mut(|cached| {
        let generation = common::media::generation();
        if let Some((path, fallback, shape, version, cover)) = cached.as_ref()
            && path == &game.path
            && fallback == &game.image
            && *shape == tall
            && *version == generation
        {
            return cover.clone();
        }
        let cover = common::media::resolve(&game.path, kind)
            .or_else(|| common::media::resolve(&game.path, common::media::Kind::Cover))
            .or_else(|| game.image.clone())
            .and_then(|path| cover_image(&path))
            .and_then(|image| {
                let rgba = image.thumbnail(320, 320).to_rgba8();
                let mut pixels = tiny_skia::Pixmap::new(rgba.width(), rgba.height())?;
                for (pixel, source) in pixels.pixels_mut().iter_mut().zip(rgba.pixels()) {
                    *pixel =
                        tiny_skia::ColorU8::from_rgba(source[0], source[1], source[2], source[3])
                            .premultiply();
                }
                Some(std::sync::Arc::new(pixels))
            });
        *cached = Some((
            game.path.clone(),
            game.image.clone(),
            tall,
            generation,
            cover.clone(),
        ));
        cover
    });
    input.art.selected_logo = common::media::load_logo(&game.path);
    let metadata = common::media::metadata(&game.path);
    input.text.insert(
        Binding::SelectedTitle,
        if metadata.name.is_empty() {
            game.name.clone()
        } else {
            metadata.name.clone()
        },
    );
    let platform = game
        .path
        .parent()
        .and_then(|path| path.file_name())
        .unwrap_or_default()
        .to_string_lossy();
    let year = metadata
        .releasedate
        .get(..4)
        .filter(|year| year.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or("");
    input
        .metadata
        .push(("Platform".into(), platform.to_string()));
    if !year.is_empty() {
        input.metadata.push(("Released".into(), year.into()));
    }
    input.text.insert(
        Binding::SelectedMetadata,
        if year.is_empty() {
            platform.to_string()
        } else {
            format!("{platform} · Released {year}")
        },
    );
}

thread_local! {
    static COVER: std::cell::RefCell<Option<(std::path::PathBuf, Option<std::path::PathBuf>, bool, u64, Option<std::sync::Arc<tiny_skia::Pixmap>>)>> = const { std::cell::RefCell::new(None) };
}

const MAX_COVER_DIMENSION: u32 = 2048;
const MAX_COVER_PIXELS: u64 = 2048 * 2048;

fn cover_image(path: &std::path::Path) -> Option<image::DynamicImage> {
    let (width, height) = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if width == 0
        || height == 0
        || width > MAX_COVER_DIMENSION
        || height > MAX_COVER_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_COVER_PIXELS
    {
        return None;
    }
    let mut reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_COVER_DIMENSION);
    limits.max_image_height = Some(MAX_COVER_DIMENSION);
    reader.limits(limits);
    #[cfg(test)]
    COVER_DECODE_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
    reader.decode().ok()
}

#[cfg(test)]
thread_local! {
    static COVER_DECODE_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod cover_tests {
    use super::*;

    #[test]
    fn menu_cover_rejects_oversized_header_before_decode() {
        let path =
            std::env::temp_dir().join(format!("allium-menu-cover-{}.png", std::process::id()));
        image::RgbaImage::new(1, 1).save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        COVER_DECODE_ATTEMPTS.with(|attempts| attempts.set(0));
        assert!(cover_image(&path).is_some());
        COVER_DECODE_ATTEMPTS.with(|attempts| assert_eq!(attempts.get(), 1));
        for (width, height) in [(8192u32, 8192u32), (2049, 1), (1, 2049)] {
            let mut bytes = original.clone();
            bytes[16..20].copy_from_slice(&width.to_be_bytes());
            bytes[20..24].copy_from_slice(&height.to_be_bytes());
            let mut crc = !0u32;
            for byte in &bytes[12..29] {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
                }
            }
            bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(image::image_dimensions(&path).unwrap(), (width, height));
            COVER_DECODE_ATTEMPTS.with(|attempts| attempts.set(0));
            assert!(cover_image(&path).is_none());
            COVER_DECODE_ATTEMPTS.with(|attempts| assert_eq!(attempts.get(), 0));
        }
        std::fs::remove_file(path).unwrap();
    }
}
