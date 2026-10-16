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
            .and_then(|path| image::open(path).ok())
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
