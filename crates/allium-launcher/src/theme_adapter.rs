use anyhow::Result;
use common::{
    constants::{ALLIUM_BASE_DIR, ALLIUM_FONTS_DIR, ALLIUM_THEMES_DIR},
    resources::Resources,
    stylesheet::Stylesheet,
    theme::frame::{FrameInputs, Hint, Row},
    theme::pack::{Binding, ColorRole, NavGroup, Role, ScreenId},
    theme::{Boundary, ThemeChoices, ThemeHandle, ThemeRuntime, ThemeStatus},
};
use std::{cell::RefCell, rc::Rc};

pub struct ThemeService {
    pub runtime: Option<ThemeRuntime>,
    pub status: ThemeStatus,
}
pub type SharedThemeService = Rc<RefCell<ThemeService>>;
pub fn initialize(res: &Resources) {
    crate::view::details::invalidate_metadata();
    let runtime = ThemeRuntime::bind(
        &ALLIUM_BASE_DIR.join("state"),
        &ALLIUM_THEMES_DIR,
        &ALLIUM_FONTS_DIR,
    );
    let (runtime, status, snapshot) = match runtime {
        Ok(mut runtime) => {
            let active = runtime.activate(Boundary::Launcher);
            (Some(runtime), active.status, active.snapshot)
        }
        Err(error) => {
            let status = ThemeStatus {
                diagnostic: Some(format!("Theme unavailable: {error}")),
                ..Default::default()
            };
            (None, status, None)
        }
    };
    if let Some(message) = &status.diagnostic {
        log::warn!("{message}");
    }
    res.insert(snapshot);
    res.insert(Rc::new(RefCell::new(ThemeService { runtime, status })));
}
pub fn snapshot(res: &Resources) -> Option<ThemeHandle> {
    res.0
        .borrow()
        .get::<Option<ThemeHandle>>()
        .cloned()
        .flatten()
}
pub fn active(res: &Resources) -> bool {
    snapshot(res).is_some()
}
pub fn choices(res: &Resources) -> Result<ThemeChoices> {
    let service = res.get::<SharedThemeService>();
    let service = service.borrow();
    service
        .runtime
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Theme library unavailable"))?
        .choices()
        .map(ThemeChoices::launcher_picker)
}
pub fn choose(res: &Resources, id: Option<&str>) -> Result<()> {
    let (snapshot, status) = {
        let shared = res.get::<SharedThemeService>();
        let mut service = shared.borrow_mut();
        let runtime = service
            .runtime
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Theme library unavailable"))?;
        let active = if let Some(id) = id {
            runtime.choose(id)?
        } else {
            runtime.choose_legacy()?
        };
        service.status = active.status.clone();
        (active.snapshot, active.status)
    };
    res.insert(snapshot);
    let _ = status;
    Ok(())
}
pub fn choose_setting(res: &Resources, setting: &str, option: &str) -> Result<()> {
    let active = {
        let shared = res.get::<SharedThemeService>();
        let mut service = shared.borrow_mut();
        let active = service
            .runtime
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Theme library unavailable"))?
            .choose_setting(setting, option)?;
        service.status = active.status.clone();
        active
    };
    res.insert(active.snapshot);
    Ok(())
}
pub fn flush_settings(res: &Resources) -> Result<()> {
    let shared = res.get::<SharedThemeService>();
    let mut service = shared.borrow_mut();
    let runtime = service
        .runtime
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("Theme library unavailable"))?;
    runtime.flush_settings()?;
    service.status = runtime.status().clone();
    Ok(())
}
pub fn capacity(handle: &ThemeHandle, screen: ScreenId, binding: Binding) -> usize {
    handle.pack().screens[&screen]
        .components
        .iter()
        .find(|c| c.binding == Some(binding))
        .and_then(|c| c.visible_count)
        .unwrap_or(4) as usize
}
pub fn shell(input: &mut FrameInputs, res: &Resources, selected: usize) {
    let locale = res.get::<common::locale::Locale>();
    if input.screen == ScreenId::GameDetail
        && !res.0.borrow().get::<Option<ThemeHandle>>().and_then(|h|h.as_ref()).is_some_and(|h| h.pack().ink()) {
        input.navigation.insert(
            NavGroup::Primary,
            vec![Row {
                subtitle: String::new(),
                id: "details".into(),
                label: "Details".into(),
                value: String::new(),
                enabled: true,
            }],
        );
        input.selected.insert(Binding::Navigation, "details".into());
    } else {
        let labels = [
            locale.t("tab-recents"),
            "Library".into(),
            locale.t("tab-apps"),
            locale.t("tab-settings"),
        ];
        input.navigation.insert(
            NavGroup::Primary,
            labels
                .iter()
                .enumerate()
                .map(|(i, label)| Row {
                    subtitle: String::new(),
                    id: format!("tab-{i}"),
                    label: label.clone(),
                    value: String::new(),
                    enabled: true,
                })
                .collect(),
        );
        input
            .selected
            .insert(Binding::Navigation, format!("tab-{}", if input.screen == ScreenId::GameDetail { 1 } else { selected }));
    }
    input.navigation.insert(NavGroup::LibraryActions, vec![]);
    if input.hints.is_empty() {
        input.hints = vec![
            Hint {
                button: "A".into(),
                label: locale.t("button-select"),
            },
            Hint {
                button: "B".into(),
                label: locale.t("button-back"),
            },
            Hint {
                button: "L/R".into(),
                label: "Mode".into(),
            },
        ];
    }
    if input.screen != ScreenId::GameDetail {
        let labels = [
            locale.t("tab-recents"),
            "Library".into(),
            locale.t("tab-apps"),
            locale.t("tab-settings"),
        ];
        let previous = &labels[(selected + labels.len() - 1) % labels.len()];
        let next = &labels[(selected + 1) % labels.len()];
        if let Some(hint) = input.hints.iter_mut().find(|hint| hint.button == "L/R") {
            hint.label = format!("{previous} / {next}");
        }
    }
}
fn color(handle: &ThemeHandle, role: ColorRole) -> common::display::color::Color {
    let s = &handle.pack().tokens.colors[&role];
    common::display::color::Color::rgba(
        u8::from_str_radix(&s[1..3], 16).unwrap(),
        u8::from_str_radix(&s[3..5], 16).unwrap(),
        u8::from_str_radix(&s[5..7], 16).unwrap(),
        u8::from_str_radix(&s[7..9], 16).unwrap(),
    )
}
#[cfg(test)]
thread_local! { pub(crate) static DRAWING_STYLE_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

pub fn drawing_style(raw: &Stylesheet, handle: Option<&ThemeHandle>) -> Stylesheet {
    let _stage = common::perf_trace::stage(common::perf_trace::Stage::DrawingStyle);
    #[cfg(test)]
    DRAWING_STYLE_BUILDS.with(|count| count.set(count.get() + 1));
    let Some(handle) = handle else {
        return raw.clone();
    };
    let mut style = raw.clone();
    style.wallpaper = None;
    style.ui.ui_font.font = Some(handle.role_font(Role::Row));
    style.ui.ui_font.size = handle.pack().tokens.typography[&Role::Row].size;
    style.cjk_font.font = Some(handle.fallback_font());
    style.ui.background_color = color(handle, ColorRole::Background);
    style.ui.text_color = color(handle, ColorRole::Text);
    style.ui.text_stroke_color = common::display::color::Color::rgba(0, 0, 0, 0);
    style.ui.highlight_color = color(handle, ColorRole::Focus);
    style.ui.highlight_text_color = color(handle, ColorRole::FocusText);
    style.ui.highlight_text_stroke_color = style.ui.text_stroke_color;
    style.ui.disabled_color = color(handle, ColorRole::Muted);
    style.ui.stroke_width = 0;
    style.ui.tab_color = color(handle, ColorRole::Muted);
    style.ui.tab_selected_color = color(handle, ColorRole::Accent);
    style.ui.tab_stroke_color = style.ui.text_stroke_color;
    style.ui.tab_selected_stroke_color = style.ui.text_stroke_color;
    style.ui.tab_font_size =
        handle.pack().tokens.typography[&Role::Label].size as f32 / style.ui.ui_font.size as f32;
    style.ui.tabs_backdrop_color = style.ui.background_color;
    style.status_bar.text_color = color(handle, ColorRole::Muted);
    style.status_bar.text_stroke_color = style.ui.text_stroke_color;
    style.status_bar.status_backdrop_color = style.ui.background_color;
    style.status_bar.font_size =
        handle.pack().tokens.typography[&Role::Status].size as f32 / style.ui.ui_font.size as f32;
    style.button_hints.text_color = color(handle, ColorRole::Muted);
    style.button_hints.button_text_color = color(handle, ColorRole::Text);
    style.button_hints.button_bg_color = color(handle, ColorRole::Surface);
    style.button_hints.button_hint_font_size =
        handle.pack().tokens.typography[&Role::Hint].size as f32 / style.ui.ui_font.size as f32;
    style.menu.background_color = color(handle, ColorRole::Surface);
    style
}

pub fn entry_id(path: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 15) as usize] as char);
    }
    output
}
pub struct ThemeScene {
    pub frames: Vec<common::theme::render::PreparedFrame>,
}
impl std::fmt::Debug for ThemeScene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThemeScene")
            .field("frames", &self.frames.len())
            .finish()
    }
}

pub fn component_id(handle: &ThemeHandle, screen: ScreenId, binding: Binding) -> String {
    handle.pack().screens[&screen]
        .components
        .iter()
        .find(|c| c.binding == Some(binding))
        .unwrap()
        .id
        .clone()
}

pub fn legacy_orbit(res: &Resources) -> bool {
    if res.0.borrow().get::<Option<ThemeHandle>>().is_some() {
        false
    } else {
        crate::view::orbit::enabled()
    }
}
pub fn keyboard_model(capture: common::view::KeyboardThemeInputs) -> FrameInputs {
    let mut input = FrameInputs::new(ScreenId::Search);
    input.text.insert(Binding::Title, "Search".into());
    input
        .text
        .insert(Binding::KeyboardValue, capture.display_value);
    input.keyboard = capture.labels;
    input.keyboard_cursor = capture.cursor;
    input.hints = vec![
        Hint {
            button: "START".into(),
            label: "Search".into(),
        },
        Hint {
            button: "B".into(),
            label: "Back".into(),
        },
        Hint {
            button: "SELECT".into(),
            label: "Case".into(),
        },
    ];
    input
}

#[cfg(test)]
mod id_tests {
    #[test]
    fn entry_id_preserves_sha256_hex_for_path_bytes() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::Path};
        use sha2::{Digest, Sha256};
        for bytes in [b"".as_slice(), b"PS/Game.chd", "日本語/Game.chd".as_bytes(), b"PS/\xff.chd"] {
            let path = Path::new(OsStr::from_bytes(bytes));
            let expected = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect::<String>();
            assert_eq!(super::entry_id(path), expected);
            assert_eq!(super::entry_id(path).len(), 64);
        }
    }
}
