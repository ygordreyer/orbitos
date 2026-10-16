use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// Includes bounded option patches with whole-array replacements.
pub const MANIFEST_LIMIT: usize = 131_072;
pub const ASSET_DECODED_LIMIT: u64 = 4 * 1024 * 1024;

macro_rules! vocabulary {
    ($name:ident { $($v:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($v),+ }
    }
}
vocabulary!(ScreenId {
    Library,
    Recents,
    SearchResults,
    Apps,
    SettingsRoot,
    SettingsPage,
    Search,
    Keyboard,
    DiscPicker,
    ContextMenu,
    Toast,
    NetworkDialog,
    IngameMenu,
    GuideSelector,
    TextReader,
    OsdVolume,
    OsdBrightness,
    GameDetail
});
impl ScreenId {
    pub const ALL: [Self; 18] = [
        Self::Library,
        Self::Recents,
        Self::SearchResults,
        Self::Apps,
        Self::SettingsRoot,
        Self::SettingsPage,
        Self::Search,
        Self::Keyboard,
        Self::DiscPicker,
        Self::ContextMenu,
        Self::Toast,
        Self::NetworkDialog,
        Self::IngameMenu,
        Self::GuideSelector,
        Self::TextReader,
        Self::OsdVolume,
        Self::OsdBrightness,
        Self::GameDetail,
    ];
    pub fn is_osd(self) -> bool {
        matches!(self, Self::OsdVolume | Self::OsdBrightness)
    }
    pub fn is_dialog(self) -> bool {
        matches!(
            self,
            Self::DiscPicker
                | Self::ContextMenu
                | Self::Toast
                | Self::NetworkDialog
                | Self::IngameMenu
        )
    }
}
vocabulary!(Kind {
    Background,
    Overlay,
    Panel,
    Text,
    Image,
    AssetImage,
    Entries,
    SettingsRows,
    Keyboard,
    Status,
    Hints,
    DialogActions,
    Progress,
    Navigation
});
vocabulary!(Role {
    Title,
    Body,
    Row,
    Hint,
    Label,
    Caption,
    Status
});
vocabulary!(ColorRole {
    Background,
    Surface,
    Text,
    Muted,
    Accent,
    Focus,
    FocusText,
    Border,
    Scrim
});
vocabulary!(Binding {
    Title,
    Subtitle,
    Breadcrumb,
    SelectedTitle,
    SelectedMetadata,
    SelectedBlurb,
    ThemeName,
    Message,
    KeyboardValue,
    GuideText,
    SelectedLogo,
    SelectedFacts,
    SelectedSource,
    SelectedEyebrow,
    Pagination,
    EmptyTitle,
    EmptyHint,
    OsdValue,
    SelectedHero,
    SelectedCover,
    NextCover,
    Entries,
    SettingsRows,
    Discs,
    StorageEntries,
    Guides,
    Keyboard,
    Status,
    Hints,
    Actions,
    Progress,
    Navigation
});
vocabulary!(Arrangement {
    Inline,
    List,
    Shelf,
    Horizontal,
    Vertical,
    Tabs,
    Current,
    Hidden,
    Rail,
    Stacked
});
vocabulary!(Fit { Contain, Cover });
vocabulary!(Overflow { Ellipsis, Wrap });
vocabulary!(Align {
    Left,
    Center,
    Right
});
vocabulary!(NavGroup {
    Primary,
    LibraryActions
});
vocabulary!(FocusStyle {
    Outline,
    Glow,
    Fill,
    Underline
});
vocabulary!(FadeEdge { None, Left });
vocabulary!(ClipShape { LeftSemicircle });
vocabulary!(HintPresentation { Plain, Keycaps });
vocabulary!(BatteryStyle {
    Icon,
    IconPercentage,
    Percentage
});
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FontId {
    #[serde(rename = "barlow-condensed")]
    BarlowCondensed,
    #[serde(rename = "nunito")]
    Nunito,
    #[serde(rename = "roboto")]
    Roboto,
    #[serde(rename = "mplus-rounded")]
    MplusRounded,
    #[serde(rename = "noto-cjk")]
    NotoCjk,
}
impl FontId {
    pub fn relative_path(self) -> &'static str {
        match self {
            Self::BarlowCondensed => "theme/BarlowCondensed-SemiBold.ttf",
            Self::Nunito => "Nunito.ttf",
            Self::Roboto => "fonts/Roboto.ttf",
            Self::MplusRounded => "MPLUSRounded1c.ttf",
            Self::NotoCjk => "NotoSansCJK.otf",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect(pub u32, pub u32, pub u32, pub u32);
impl Rect {
    pub fn validate(self) -> Result<()> {
        ensure!(
            self.2 > 0
                && self.3 > 0
                && self.0.checked_add(self.2).is_some_and(|v| v <= 640)
                && self.1.checked_add(self.3).is_some_and(|v| v <= 480),
            "Rectangle outside canvas"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub logo: bool,
    pub id: String,
    pub kind: Kind,
    pub rect: Rect,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<Binding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<ColorRole>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub radius: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overflow: Option<Overflow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fit: Option<Fit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<Arrangement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_extent: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub art_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption_height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption_lines: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_color: Option<ColorRole>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus_color: Option<ColorRole>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus_style: Option<FocusStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glow_radius: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<NavGroup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_items: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fade_edge: Option<FadeEdge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip_shape: Option<ClipShape>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fade_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_style: Option<BatteryStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blur_radius: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask_asset_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation: Option<HintPresentation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paint: Option<Box<super::ink::Paint>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paint_ref: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Typography {
    pub font: FontId,
    pub size: u32,
    pub line_height: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tokens {
    pub colors: BTreeMap<ColorRole, String>,
    pub typography: BTreeMap<Role, Typography>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screen {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omit: Vec<Binding>,
    pub components: Vec<Component>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub path: String,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub required: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pack {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<super::settings::Setting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alignment: Option<super::ink::Alignment>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ink_typography: BTreeMap<Role, super::ink::Run>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub paint_presets: BTreeMap<String, Box<super::ink::Paint>>,
    pub schema_version: u32,
    pub id: String,
    pub revision: String,
    pub name: String,
    pub canvas: Canvas,
    pub tokens: Tokens,
    pub screens: BTreeMap<ScreenId, Screen>,
    pub assets: BTreeMap<String, Asset>,
    pub customization: Vec<String>,
    pub motion: String,
}
pub fn valid_id(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 48
        && v.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && v.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
pub fn valid_asset_path(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 240
        && !v.contains(['\\', ':', '\0'])
        && v.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}
pub fn valid_digest(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Pack {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= MANIFEST_LIMIT, "Theme manifest exceeds limit");
        let mut parser = serde_json::Deserializer::from_slice(bytes);
        UniqueJson::deserialize(&mut parser)?;
        parser.end()?;
        super::settings::validate(&serde_json::from_slice(bytes)?)?;
        let base = Self::parse_descriptor(bytes)?;
        Ok(base)
    }
    pub(super) fn parse_descriptor(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MANIFEST_LIMIT,
            "Theme manifest exceeds limit"
        );
        let mut parser = serde_json::Deserializer::from_slice(bytes);
        UniqueJson::deserialize(&mut parser)?;
        parser.end()?;
        let mut value: Self = serde_json::from_slice(bytes)?;
        for screen in value.screens.values_mut() {
            for c in &mut screen.components {
                if let Some(id) = c.paint_ref.take() {
                    ensure!(c.paint.is_none(), "Ambiguous paint preset");
                    c.paint = Some(
                        value
                            .paint_presets
                            .get(&id)
                            .ok_or_else(|| anyhow::anyhow!("Unknown paint preset"))?
                            .clone(),
                    );
                }
            }
        }
        value.validate()?;
        Ok(value)
    }
    pub fn builtin(id: &str) -> Result<Self> {
        Self::parse(Self::builtin_bytes(id)?)
    }
    pub fn builtin_bytes(id: &str) -> Result<&'static [u8]> {
        match id {
            "orbit" => Ok(include_bytes!("packs/orbit.json")),
            "compact" => Ok(include_bytes!("packs/compact.json")),
            _ => anyhow::bail!("Unknown builtin theme"),
        }
    }
    pub fn component(&self, screen: ScreenId, id: &str) -> Option<&Component> {
        self.screens
            .get(&screen)?
            .components
            .iter()
            .find(|v| v.id == id)
    }
    pub fn ink(&self) -> bool {
        self.alignment.as_ref().is_some_and(|a| a.mode == "ink")
    }
    pub fn validate(&self) -> Result<()> {
        if let Some(a) = &self.alignment {
            ensure!(
                a.mode == "ink"
                    && matches!(a.cover_shape.as_str(), "square" | "tall")
                    && a.row_inset <= 32
                    && a.status_gap <= 32
                    && a.panel_padding <= 32
                    && self.schema_version == 2,
                "Invalid ink alignment"
            );
        }
        ensure!(
            (1..=2).contains(&self.schema_version)
                && self.canvas.width == 640
                && self.canvas.height == 480,
            "Unsupported theme version/canvas"
        );
        ensure!(
            valid_id(&self.id)
                && !self.revision.is_empty()
                && self.revision.len() <= 48
                && self
                    .revision
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_alphanumeric())
                && self
                    .revision
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "Invalid theme identity"
        );
        ensure!(
            !self.name.is_empty() && self.name.chars().count() <= 64,
            "Invalid theme name"
        );
        ensure!(
            self.motion == "none" && self.customization.is_empty(),
            "Unsupported theme customization/motion"
        );
        let roles = [
            Role::Title,
            Role::Body,
            Role::Row,
            Role::Hint,
            Role::Label,
            Role::Caption,
            Role::Status,
        ];
        ensure!(
            self.tokens.typography.len() == roles.len(),
            "Missing typography role"
        );
        for role in roles {
            let t = self
                .tokens
                .typography
                .get(&role)
                .ok_or_else(|| anyhow::anyhow!("Missing role"))?;
            ensure!(
                t.size
                    >= if role == Role::Title {
                        24
                    } else if role == Role::Hint && self.schema_version == 2 {
                        14
                    } else {
                        18
                    }
                    && t.size <= 48
                    && (t.line_height == t.size
                        || (self.schema_version == 2
                            && t.line_height >= t.size
                            && t.line_height <= t.size + 8)),
                "Invalid font size"
            );
        }
        ensure!(self.tokens.colors.len() == 9, "Missing color role");
        for v in self.tokens.colors.values() {
            ensure!(
                v.len() == 9 && v.starts_with('#') && v[1..].bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid color"
            );
        }
        ensure!(
            self.screens.len() == 18
                && ScreenId::ALL.iter().all(|id| self.screens.contains_key(id)),
            "Incomplete screen map"
        );
        ensure!(
            self.ink() || (self.ink_typography.is_empty() && self.paint_presets.is_empty()),
            "Ink typography/presets require alignment mode"
        );
        for run in self.ink_typography.values() {
            run.validate()?;
        }
        ensure!(self.paint_presets.len() <= 256, "Too many paint presets");
        for paint in self.paint_presets.values() {
            paint.validate()?;
        }
        let mut total = 0;
        for (&screen, spec) in &self.screens {
            ensure!(
                spec.omit.is_empty()
                    || self.ink()
                        && spec.omit.len() == 1
                        && ((screen == ScreenId::GameDetail && spec.omit == [Binding::Entries])
                            || (screen == ScreenId::NetworkDialog
                                && spec.omit == [Binding::StorageEntries])),
                "Invalid omitted screen binding"
            );
            ensure!(
                !spec
                    .components
                    .iter()
                    .any(|c| c.binding.is_some_and(|b| spec.omit.contains(&b))),
                "Omitted binding has a component"
            );
            ensure!(
                !spec.components.is_empty() && spec.components.len() <= 64,
                "Invalid component count"
            );
            total += spec.components.len();
            let mut ids = BTreeSet::new();
            for c in &spec.components {
                ensure!(
                    valid_id(&c.id) && ids.insert(&c.id),
                    "Invalid/duplicate component ID"
                );
                c.rect.validate()?;
                if self.ink() {
                    self.validate_ink_component(screen, c)?;
                } else {
                    ensure!(
                        c.paint.is_none() && spec.omit.is_empty(),
                        "Ink fields require alignment mode"
                    );
                    self.validate_component(screen, c)?;
                }
            }
            let mut semantics = BTreeSet::new();
            for c in &spec.components {
                if let Some(b) = c.binding {
                    let conditional =
                        self.ink() && c.paint.as_ref().is_some_and(|p| p.when.is_some());
                    ensure!(
                        semantics.insert((b, c.group)) || conditional,
                        "Duplicate semantic slot"
                    );
                }
            }
            for required in required_bindings(screen) {
                if self.ink()
                    && (spec.omit.contains(required)
                        || (*required == Binding::SelectedTitle
                            && semantics.contains(&(Binding::SelectedLogo, None))))
                {
                    continue;
                }
                if self.schema_version == 2
                    && screen == ScreenId::Library
                    && *required == Binding::SelectedMetadata
                {
                    continue;
                }
                ensure!(
                    semantics.contains(&(
                        *required,
                        if *required == Binding::Navigation {
                            Some(NavGroup::Primary)
                        } else {
                            None
                        }
                    )),
                    "Missing required screen binding {required:?}"
                );
            }
            if screen == ScreenId::Library {
                ensure!(
                    semantics.contains(&(Binding::SelectedHero, None))
                        || semantics.contains(&(Binding::SelectedCover, None)),
                    "Missing selected artwork"
                );
            }
            if screen == ScreenId::Library {
                ensure!(
                    semantics.contains(&(Binding::Navigation, Some(NavGroup::LibraryActions)))
                        && (self.schema_version == 2
                            || semantics.contains(&(Binding::SelectedBlurb, None))),
                    "Missing library secondary group/blurb"
                );
            }
            if !screen.is_osd() && !screen.is_dialog() {
                ensure!(
                    semantics.contains(&(Binding::ThemeName, None)),
                    "Missing effective theme name"
                );
            }
            let has = |kind| spec.components.iter().any(|c| c.kind == kind);
            if !screen.is_osd() {
                ensure!(has(Kind::Hints), "Missing back/action hints");
            }
            if screen.is_dialog() {
                ensure!(has(Kind::DialogActions), "Missing dialog actions");
            }
            if screen.is_osd() {
                ensure!(
                    has(Kind::Progress) && has(Kind::Text),
                    "Missing OSD semantics"
                );
            }
            if !screen.is_osd() && !screen.is_dialog() {
                ensure!(
                    has(Kind::Navigation) && has(Kind::Status),
                    "Missing navigation/status"
                );
            }
            match screen {
                ScreenId::Library => ensure!(
                    has(Kind::Entries) && has(Kind::Image),
                    "Missing library entries/art"
                ),
                ScreenId::SettingsRoot | ScreenId::SettingsPage => {
                    ensure!(has(Kind::SettingsRows), "Missing settings")
                }
                ScreenId::Keyboard | ScreenId::Search => {
                    ensure!(has(Kind::Keyboard), "Missing fixed keyboard")
                }
                ScreenId::TextReader => ensure!(
                    spec.components
                        .iter()
                        .any(|c| c.binding == Some(Binding::GuideText)),
                    "Missing guide text"
                ),
                ScreenId::DiscPicker => ensure!(
                    spec.components
                        .iter()
                        .any(|c| c.binding == Some(Binding::Discs)),
                    "Missing discs"
                ),
                _ => {}
            }
        }
        ensure!(
            total <= 512 && self.assets.len() <= 64,
            "Theme resource count exceeded"
        );
        let mut decoded = 0u64;
        for (id, a) in &self.assets {
            ensure!(
                valid_id(id) && valid_asset_path(&a.path) && valid_digest(&a.sha256),
                "Invalid asset identity/path"
            );
            ensure!(
                a.path.ends_with(".png")
                    && a.width > 0
                    && a.width <= 640
                    && a.height > 0
                    && a.height <= 480
                    && a.bytes > 0
                    && a.bytes <= 1_048_576,
                "Invalid PNG limits"
            );
            decoded += u64::from(a.width) * u64::from(a.height) * 4;
        }
        ensure!(
            decoded <= ASSET_DECODED_LIMIT,
            "Theme decoded asset budget exceeded"
        );
        Ok(())
    }
    pub(crate) fn ink_run(&self, c: &Component, focused: bool) -> super::ink::Run {
        use super::ink::{Run, merge_run};
        let role = c.role.unwrap_or(Role::Body);
        let t = &self.tokens.typography[&role];
        let mut run = Run {
            font: Some(t.font),
            size: Some(t.size),
            line_height: Some(t.line_height),
            color: c.color,
            ..Default::default()
        };
        if let Some(base) = self.ink_typography.get(&role) {
            merge_run(&mut run, base);
        }
        let p = c.paint.as_ref().unwrap();
        if let Some(r) = &p.run {
            merge_run(&mut run, r);
        }
        if focused {
            if let Some(r) = &p.focus_run {
                merge_run(&mut run, r);
            }
            if let Some(color) = p.focus_text {
                run.color = Some(color);
            } else if c.focus_style == Some(FocusStyle::Fill) {
                run.color = Some(ColorRole::FocusText);
            }
        }
        run
    }
    fn validate_component_screen(&self, screen: ScreenId, c: &Component) -> Result<()> {
        ensure!(!c.logo || c.kind == Kind::Text && matches!(c.binding, Some(Binding::Title | Binding::SelectedTitle)), "Logo opt-in requires title text");
        let allowed = match c.kind {
            Kind::Background | Kind::Panel | Kind::Text | Kind::AssetImage => true,
            Kind::Overlay => self.ink(),
            Kind::Image => {
                matches!(
                    screen,
                    ScreenId::Library
                        | ScreenId::GameDetail
                        | ScreenId::Recents
                        | ScreenId::Apps
                        | ScreenId::SearchResults
                ) || self.ink()
                    && matches!(
                        screen,
                        ScreenId::DiscPicker
                            | ScreenId::ContextMenu
                            | ScreenId::IngameMenu
                            | ScreenId::GuideSelector
                    )
            }
            Kind::Entries => matches!(
                screen,
                ScreenId::Library
                    | ScreenId::Recents
                    | ScreenId::SearchResults
                    | ScreenId::Apps
                    | ScreenId::GuideSelector
                    | ScreenId::DiscPicker
                    | ScreenId::ContextMenu
                    | ScreenId::IngameMenu
                    | ScreenId::NetworkDialog
                    | ScreenId::GameDetail
            ),
            Kind::SettingsRows => matches!(screen, ScreenId::SettingsRoot | ScreenId::SettingsPage),
            Kind::Keyboard => matches!(screen, ScreenId::Search | ScreenId::Keyboard),
            Kind::Status | Kind::Navigation => !screen.is_dialog() && !screen.is_osd(),
            Kind::Hints => self.ink() || !screen.is_osd(),
            Kind::DialogActions => screen.is_dialog() || screen == ScreenId::GameDetail,
            Kind::Progress => screen == ScreenId::NetworkDialog || screen.is_osd(),
        };
        ensure!(allowed, "Component incompatible with screen");
        if let Some(b) = c.binding {
            ensure!(
                match b {
                    Binding::Discs => screen == ScreenId::DiscPicker,
                    Binding::StorageEntries | Binding::Progress =>
                        screen == ScreenId::NetworkDialog || screen.is_osd(),
                    Binding::Guides => screen == ScreenId::GuideSelector,
                    Binding::GuideText => screen == ScreenId::TextReader && c.kind == Kind::Text,
                    Binding::Keyboard | Binding::KeyboardValue =>
                        matches!(screen, ScreenId::Keyboard | ScreenId::Search),
                    Binding::SettingsRows =>
                        matches!(screen, ScreenId::SettingsRoot | ScreenId::SettingsPage),
                    _ => true,
                },
                "Binding incompatible with screen/component"
            );
        }
        Ok(())
    }
    fn validate_ink_component(&self, screen: ScreenId, c: &Component) -> Result<()> {
        self.validate_component_screen(screen, c)?;
        let p = c
            .paint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Ink component missing paint"))?;
        p.validate()?;
        let required: &[&str] = match c.kind {
            Kind::Background | Kind::Panel | Kind::Overlay => &["color", "radius"],
            Kind::Text => &["binding", "role", "color", "max_lines", "overflow", "align"],
            Kind::Image => &["binding", "fit", "radius", "fade_edge", "fade_width"],
            Kind::AssetImage => &["asset_id", "fit", "radius"],
            Kind::Entries | Kind::SettingsRows => &[
                "binding",
                "layout",
                "role",
                "visible_count",
                "item_extent",
                "gap",
                "padding",
                "art_size",
                "caption_height",
                "caption_lines",
                "frame_color",
                "focus_color",
                "focus_style",
                "focus_width",
                "glow_radius",
                "radius",
            ],
            Kind::Keyboard => &["binding", "role", "gap", "padding", "radius"],
            Kind::Hints => &["binding", "role", "color"],
            Kind::Status => &[
                "binding",
                "role",
                "color",
                "layout",
                "max_lines",
                "battery_style",
            ],
            Kind::DialogActions => &["binding", "role", "layout", "radius"],
            Kind::Navigation => &[
                "binding",
                "role",
                "layout",
                "radius",
                "group",
                "max_items",
                "item_extent",
                "gap",
                "padding",
                "focus_color",
                "focus_width",
            ],
            Kind::Progress => &["binding", "color", "radius"],
        };
        let object = serde_json::to_value(c)?;
        ensure!(
            required.iter().all(|k| object.get(k).is_some()),
            "Missing ink component fields"
        );
        let mut fields = object.as_object().unwrap().clone();
        for field in ["id", "kind", "rect", "paint", "paint_ref"]
            .into_iter()
            .chain(required.iter().copied())
        {
            fields.remove(field);
        }
        let extras: &[&str] = match c.kind {
            Kind::Text => &["layout", "gap", "radius", "logo"],
            Kind::Image => &["role", "color", "blur_radius"],
            Kind::Entries | Kind::SettingsRows | Kind::Navigation | Kind::Keyboard => &["color"],
            Kind::Status | Kind::DialogActions => &["align", "radius", "color"],
            Kind::Hints => &["max_lines", "presentation", "radius"],
            _ => &[],
        };
        ensure!(
            fields.keys().all(|field| extras.contains(&field.as_str())),
            "Incompatible ink component fields: {:?} {:?}",
            c.kind,
            fields.keys().collect::<Vec<_>>()
        );
        ensure!(
            c.max_lines.is_none_or(|n| (1..=16).contains(&n)),
            "Invalid ink line count"
        );
        ensure!(
            match c.kind {
                Kind::Background
                | Kind::Panel
                | Kind::Overlay
                | Kind::AssetImage
                | Kind::Text
                | Kind::Image => true,
                Kind::Entries => matches!(
                    c.binding,
                    Some(
                        Binding::Entries
                            | Binding::Discs
                            | Binding::StorageEntries
                            | Binding::Guides
                    )
                ),
                Kind::SettingsRows => c.binding == Some(Binding::SettingsRows),
                Kind::Keyboard => c.binding == Some(Binding::Keyboard),
                Kind::Status => c.binding == Some(Binding::Status),
                Kind::Hints => c.binding == Some(Binding::Hints),
                Kind::DialogActions => c.binding == Some(Binding::Actions),
                Kind::Navigation => c.binding == Some(Binding::Navigation),
                Kind::Progress => c.binding == Some(Binding::Progress),
            },
            "Binding incompatible with ink component"
        );
        ensure!(
            !matches!(c.kind, Kind::Text | Kind::Image)
                || c.binding.is_some_and(|b| match c.kind {
                    Kind::Image => matches!(
                        b,
                        Binding::SelectedHero
                            | Binding::SelectedCover
                            | Binding::NextCover
                            | Binding::SelectedLogo
                    ),
                    _ => matches!(
                        b,
                        Binding::Title
                            | Binding::Subtitle
                            | Binding::Breadcrumb
                            | Binding::SelectedTitle
                            | Binding::SelectedMetadata
                            | Binding::SelectedBlurb
                            | Binding::ThemeName
                            | Binding::Message
                            | Binding::KeyboardValue
                            | Binding::GuideText
                            | Binding::SelectedFacts
                            | Binding::SelectedSource
                            | Binding::SelectedEyebrow
                            | Binding::Pagination
                            | Binding::EmptyTitle
                            | Binding::EmptyHint
                            | Binding::OsdValue
                    ),
                }),
            "Invalid ink binding"
        );
        ensure!(
            c.radius.unwrap_or(0) <= 96
                && c.padding.unwrap_or(0) <= 32
                && c.gap.unwrap_or(0) <= 64
                && c.caption_height.unwrap_or(0) <= 96
                && c.caption_lines.unwrap_or(0) <= 4
                && c.art_size.unwrap_or(0) <= 256,
            "Invalid ink spacing"
        );
        ensure!(
            c.blur_radius.unwrap_or(0) <= 4 && c.clip_shape.is_none(),
            "Unsupported ink image effect"
        );
        if c.role.is_some() {
            for focused in [false, true] {
                let run = self.ink_run(c, focused);
                run.validate()?;
                ensure!(c.rect.3 >= run.size.unwrap(), "Ink text box too small");
                if c.kind == Kind::Keyboard {
                    let cell = run
                        .line_height
                        .unwrap()
                        .saturating_add(c.padding.unwrap_or(0).saturating_mul(2));
                    ensure!(
                        cell.saturating_mul(11)
                            .saturating_add(c.gap.unwrap_or(0).saturating_mul(10))
                            <= c.rect.2
                            && cell
                                .saturating_mul(5)
                                .saturating_add(c.gap.unwrap_or(0).saturating_mul(4))
                                <= c.rect.3,
                        "Ink keyboard exceeds bounds"
                    );
                }
            }
            for extra in [&p.subtitle, &p.label, &p.value, &p.section_style]
                .into_iter()
                .flatten()
            {
                let mut sub = c.clone();
                if let Some(role) = extra.role {
                    sub.role = Some(role);
                    sub.paint.as_mut().unwrap().run = None;
                }
                let mut run = sub.paint.as_mut().unwrap().run.clone().unwrap_or_default();
                super::ink::merge_run(&mut run, extra);
                sub.paint.as_mut().unwrap().run = Some(run);
                let run = self.ink_run(&sub, false);
                run.validate()?;
                ensure!(c.rect.3 >= run.size.unwrap(), "Ink text box too small");
            }
        }
        match c.kind {
            Kind::Image => ensure!(
                c.fit.is_some()
                    && c.binding.is_some()
                    && c.fade_width.unwrap_or(0) <= c.rect.2.min(192),
                "Invalid ink image"
            ),
            Kind::AssetImage => ensure!(
                c.asset_id
                    .as_ref()
                    .is_some_and(|id| self.assets.contains_key(id))
                    && c.fit.is_some(),
                "Invalid ink asset"
            ),
            Kind::Text => ensure!(
                c.binding.is_some()
                    && c.role.is_some()
                    && (1..=16).contains(&c.max_lines.unwrap_or(0)),
                "Invalid ink text"
            ),
            Kind::Overlay => ensure!(p.gradient.is_some(), "Overlay requires gradient"),
            Kind::Entries | Kind::SettingsRows => {
                let n = c.visible_count.unwrap_or(0);
                let extent = c.item_extent.unwrap_or(0);
                let gap = c.gap.unwrap_or(0);
                let pad = c.padding.unwrap_or(0);
                ensure!(
                    (1..=16).contains(&n)
                        && extent > 0
                        && extent <= 640
                        && c.binding.is_some()
                        && c.role.is_some(),
                    "Invalid ink entries"
                );
                let shelf = c.layout == Some(Arrangement::Shelf);
                ensure!(
                    matches!(c.layout, Some(Arrangement::List | Arrangement::Shelf))
                        && n * extent + (n - 1) * gap <= if shelf { c.rect.2 } else { c.rect.3 },
                    "Ink entries exceed component"
                );
                if shelf {
                    let (w, h) = p
                        .art
                        .as_ref()
                        .map(|a| (a.width, a.height))
                        .unwrap_or((c.art_size.unwrap_or(0), c.art_size.unwrap_or(0)));
                    ensure!(
                        w > 0
                            && w + 2 * pad <= extent
                            && h + 2 * pad + c.caption_height.unwrap_or(0) <= c.rect.3,
                        "Ink shelf exceeds bounds"
                    );
                    let scale = p.focus_scale.unwrap_or(1.0);
                    ensure!(
                        (extent as f32 * scale).round() <= 640.0
                            && ((h + 2 * pad) as f32 * scale).round() <= 480.0,
                        "Focused ink shelf exceeds canvas"
                    );
                }
                ensure!(
                    c.focus_width.is_some_and(|v| (1..=4).contains(&v))
                        && c.glow_radius.unwrap_or(0) <= pad.min(8),
                    "Invalid ink focus"
                );
            }
            Kind::Navigation => {
                let n = c.max_items.unwrap();
                let extent = c.item_extent.unwrap();
                let gap = c.gap.unwrap();
                let axis = if c.layout == Some(Arrangement::Rail) {
                    c.rect.3
                } else {
                    c.rect.2
                };
                ensure!(
                    (1..=4).contains(&n)
                        && extent > 0
                        && extent <= 640
                        && (if matches!(c.layout, Some(Arrangement::Current | Arrangement::Hidden))
                        {
                            extent
                        } else {
                            n * extent + (n - 1) * gap
                        }) <= axis
                        && matches!(
                            c.layout,
                            Some(
                                Arrangement::Rail
                                    | Arrangement::Tabs
                                    | Arrangement::Horizontal
                                    | Arrangement::Current
                                    | Arrangement::Hidden
                            )
                        ),
                    "Invalid ink navigation"
                );
            }
            Kind::Status => ensure!(
                c.role.is_some() && (1..=3).contains(&c.max_lines.unwrap_or(0)),
                "Invalid ink status"
            ),
            Kind::DialogActions => ensure!(
                c.role.is_some()
                    && matches!(
                        c.layout,
                        Some(Arrangement::Horizontal | Arrangement::Vertical)
                    ),
                "Invalid ink actions"
            ),
            _ => {}
        }
        ensure!(
            !p.fit_content || screen.is_dialog() && c.kind == Kind::Panel,
            "Content fit requires dialog"
        );
        ensure!(
            !p.collapse_empty
                || screen == ScreenId::NetworkDialog
                    && c.kind == Kind::Entries
                    && c.binding == Some(Binding::StorageEntries),
            "Collapse empty requires storage entries"
        );
        Ok(())
    }
    fn validate_component(&self, screen: ScreenId, c: &Component) -> Result<()> {
        self.validate_component_screen(screen, c)?;
        let v = serde_json::to_value(c)?;
        let expected: &[&str] = match c.kind {
            Kind::Background | Kind::Panel | Kind::Overlay => &["color", "radius"],
            Kind::Text => &["binding", "role", "color", "max_lines", "overflow", "align"],
            Kind::Image => &["binding", "fit", "radius", "fade_edge", "fade_width"],
            Kind::AssetImage => &["asset_id", "fit", "radius"],
            Kind::Entries | Kind::SettingsRows => &[
                "binding",
                "layout",
                "role",
                "visible_count",
                "item_extent",
                "gap",
                "padding",
                "art_size",
                "caption_height",
                "caption_lines",
                "frame_color",
                "focus_color",
                "focus_style",
                "focus_width",
                "glow_radius",
                "radius",
            ],
            Kind::Keyboard => &["binding", "role", "gap", "padding", "radius"],
            Kind::Hints => &["binding", "role", "color"],
            Kind::Status => &[
                "binding",
                "role",
                "color",
                "layout",
                "max_lines",
                "battery_style",
            ],
            Kind::DialogActions => &["binding", "role", "layout", "radius"],
            Kind::Navigation => &[
                "binding",
                "role",
                "layout",
                "radius",
                "group",
                "max_items",
                "item_extent",
                "gap",
                "padding",
                "focus_color",
                "focus_width",
            ],
            Kind::Progress => &["binding", "color", "radius"],
        };
        let mut object = v.as_object().unwrap().clone();
        object.remove("logo");
        for extension in ["blur_radius", "mask_asset_id", "clip_shape"] {
            if object.remove(extension).is_some() {
                ensure!(
                    self.schema_version == 2 && c.kind == Kind::Image,
                    "Image extension requires v2 image"
                );
            }
        }
        if object.remove("presentation").is_some() {
            ensure!(
                self.schema_version == 2 && c.kind == Kind::Hints,
                "Hint presentation requires v2 hints"
            );
        }
        if c.kind == Kind::Hints && object.remove("max_lines").is_some() {
            ensure!(
                self.schema_version == 2 && c.max_lines == Some(1),
                "Single-row hints require v2"
            );
        }
        let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
        let needed: BTreeSet<&str> = ["id", "kind", "rect"]
            .into_iter()
            .chain(expected.iter().copied())
            .collect();
        ensure!(keys == needed, "Wrong/missing component fields");
        ensure!(
            c.radius.unwrap_or(0) <= 32 && c.gap.unwrap_or(0) <= 64 && c.padding.unwrap_or(0) <= 24,
            "Invalid spacing/radius"
        );
        let binding_ok = match c.kind {
            Kind::Background | Kind::Panel | Kind::Overlay | Kind::AssetImage => true,
            Kind::Text => matches!(
                c.binding,
                Some(
                    Binding::Title
                        | Binding::Subtitle
                        | Binding::Breadcrumb
                        | Binding::SelectedTitle
                        | Binding::SelectedMetadata
                        | Binding::SelectedBlurb
                        | Binding::ThemeName
                        | Binding::Message
                        | Binding::KeyboardValue
                        | Binding::GuideText
                )
            ),
            Kind::Image => matches!(
                c.binding,
                Some(Binding::SelectedHero | Binding::SelectedCover | Binding::NextCover)
            ),
            Kind::Entries => matches!(
                c.binding,
                Some(Binding::Entries | Binding::Discs | Binding::StorageEntries | Binding::Guides)
            ),
            Kind::SettingsRows => c.binding == Some(Binding::SettingsRows),
            Kind::Keyboard => c.binding == Some(Binding::Keyboard),
            Kind::Status => c.binding == Some(Binding::Status),
            Kind::Hints => c.binding == Some(Binding::Hints),
            Kind::DialogActions => c.binding == Some(Binding::Actions),
            Kind::Progress => c.binding == Some(Binding::Progress),
            Kind::Navigation => c.binding == Some(Binding::Navigation),
        };
        ensure!(binding_ok, "Binding incompatible with component");
        if c.kind == Kind::AssetImage {
            ensure!(
                c.asset_id
                    .as_ref()
                    .is_some_and(|id| self.assets.contains_key(id)),
                "Unknown pack asset reference"
            );
        }
        if let Some(b) = c.binding {
            ensure!(
                match b {
                    Binding::Discs => screen == ScreenId::DiscPicker,
                    Binding::StorageEntries | Binding::Progress =>
                        screen == ScreenId::NetworkDialog || screen.is_osd(),
                    Binding::Guides => screen == ScreenId::GuideSelector,
                    Binding::GuideText => screen == ScreenId::TextReader,
                    Binding::Keyboard | Binding::KeyboardValue =>
                        matches!(screen, ScreenId::Keyboard | ScreenId::Search),
                    Binding::SettingsRows =>
                        matches!(screen, ScreenId::SettingsRoot | ScreenId::SettingsPage),
                    _ => true,
                },
                "Binding incompatible with screen"
            );
        }
        if c.kind == Kind::Image {
            ensure!(
                c.binding != Some(Binding::NextCover) || self.schema_version == 2,
                "Next cover requires v2"
            );
            ensure!(c.blur_radius.unwrap_or(0) <= 4, "Image blur exceeds bound");
            if c.clip_shape == Some(ClipShape::LeftSemicircle) {
                ensure!(
                    c.binding == Some(Binding::NextCover)
                        && c.rect.2 * 2 == c.rect.3
                        && c.mask_asset_id.is_none(),
                    "Left semicircle requires unmasked half-disc preview"
                );
            }
            if let Some(id) = &c.mask_asset_id {
                let asset = self
                    .assets
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("Unknown image mask"))?;
                ensure!(
                    asset.required && asset.width == c.rect.2 && asset.height == c.rect.3,
                    "Image mask must be required and destination sized"
                );
            }
            ensure!(
                c.fade_width.unwrap() <= 192
                    && c.fade_width.unwrap() <= c.rect.2
                    && (c.fade_edge == Some(FadeEdge::None)) == (c.fade_width == Some(0)),
                "Invalid cached image fade"
            );
        }
        if c.kind == Kind::Status {
            ensure!(
                c.battery_style != Some(BatteryStyle::Icon)
                    || (self.schema_version == 2 && c.layout == Some(Arrangement::Horizontal)),
                "Icon-only battery requires v2 horizontal status"
            );
            ensure!(
                ((c.layout == Some(Arrangement::Stacked)
                    && (2..=3).contains(&c.max_lines.unwrap()))
                    || (self.schema_version == 2
                        && c.layout == Some(Arrangement::Horizontal)
                        && c.max_lines == Some(1)
                        && c.rect.2 >= 176))
                    && c.max_lines.unwrap() * self.tokens.typography[&c.role.unwrap()].line_height
                        <= c.rect.3,
                "Status lines exceed bounds"
            );
        }
        if let Some(width) = c.focus_width {
            ensure!((1..=4).contains(&width), "Invalid focus outline");
        }
        if c.kind == Kind::Text {
            let lines = c.max_lines.unwrap();
            ensure!(
                (1..=16).contains(&lines)
                    && lines * self.tokens.typography[&c.role.unwrap()].size <= c.rect.3,
                "Text exceeds line bounds"
            );
        }
        if let Some(role) = c.role {
            let font = self.tokens.typography[&role].size;
            ensure!(
                c.rect.2 >= font * 2 && c.rect.3 >= font,
                "Component too small for text role"
            );
        }
        if matches!(c.kind, Kind::Entries | Kind::SettingsRows) {
            let n = c.visible_count.unwrap();
            let extent = c.item_extent.unwrap();
            let gap = c.gap.unwrap();
            ensure!(
                (1..=16).contains(&n)
                    && extent > 0
                    && extent <= 640
                    && matches!(c.layout, Some(Arrangement::List | Arrangement::Shelf)),
                "Invalid entries layout"
            );
            let shelf = c.layout == Some(Arrangement::Shelf);
            ensure!(
                n * extent + (n - 1) * gap <= if shelf { c.rect.2 } else { c.rect.3 },
                "Entries exceed bounds"
            );
            ensure!(
                extent >= self.tokens.typography[&c.role.unwrap()].size + 2 * c.padding.unwrap(),
                "Row text exceeds bounds"
            );
            ensure!(
                c.art_size.unwrap() <= 192
                    && c.caption_height.unwrap() <= 64
                    && (!shelf || c.art_size.unwrap() + c.caption_height.unwrap() <= c.rect.3),
                "Art exceeds card bounds"
            );
            let padding = c.padding.unwrap();
            let art = c.art_size.unwrap();
            let caption = c.caption_height.unwrap();
            let font = self.tokens.typography[&c.role.unwrap()].size;
            ensure!(
                c.glow_radius.unwrap() <= 8
                    && c.glow_radius.unwrap() <= padding
                    && (c.focus_style == Some(FocusStyle::Outline)) == (c.glow_radius == Some(0)),
                "Invalid bounded focus glow"
            );
            if shelf {
                ensure!(
                    art > 0
                        && art + 2 * padding <= extent
                        && (1..=3).contains(&c.caption_lines.unwrap())
                        && caption >= font * c.caption_lines.unwrap()
                        && art + caption + 2 * padding <= c.rect.3,
                    "Shelf artwork/caption exceeds item bounds"
                );
            } else {
                ensure!(
                    c.caption_lines == Some(0) && caption == 0,
                    "List cannot add shelf captions"
                );
                ensure!(
                    art + 2 * padding <= extent
                        && c.rect.2
                            >= 2 * padding + font * 2 + if art > 0 { art + padding } else { 0 },
                    "List cross-axis/text/art exceeds row bounds"
                );
            }
        }
        if c.kind == Kind::Keyboard {
            let font = self.tokens.typography[&c.role.unwrap()].size;
            let padding = c.padding.unwrap();
            let gap = c.gap.unwrap();
            ensure!(
                padding <= 16
                    && gap <= 16
                    && 11 * (font + 2 * padding) + 10 * gap <= c.rect.2
                    && 5 * (font + 2 * padding) + 4 * gap <= c.rect.3,
                "Fixed keyboard children exceed bounds"
            );
        }
        if c.kind == Kind::Navigation {
            ensure!(
                c.layout != Some(Arrangement::Hidden) || self.schema_version == 2,
                "Hidden navigation requires v2"
            );
            ensure!(
                matches!(
                    c.layout,
                    Some(
                        Arrangement::Tabs
                            | Arrangement::Current
                            | Arrangement::Rail
                            | Arrangement::Hidden
                    )
                ),
                "Invalid navigation arrangement"
            );
            let font = self.tokens.typography[&c.role.unwrap()].line_height;
            let n = c.max_items.unwrap();
            let extent = c.item_extent.unwrap();
            let gap = c.gap.unwrap();
            let padding = c.padding.unwrap();
            ensure!(
                n == if c.group == Some(NavGroup::Primary) {
                    4
                } else {
                    3
                },
                "Invalid navigation group capacity"
            );
            ensure!(
                c.group != Some(NavGroup::LibraryActions) || screen == ScreenId::Library,
                "Secondary navigation only belongs to library"
            );
            ensure!(
                gap <= 32
                    && padding <= 12
                    && extent >= font + 2 * padding
                    && (if matches!(c.layout, Some(Arrangement::Current | Arrangement::Hidden)) {
                        extent
                    } else {
                        n * extent + (n - 1) * gap
                    }) <= if c.layout == Some(Arrangement::Rail) {
                        c.rect.3
                    } else {
                        c.rect.2
                    },
                "Navigation children exceed bounds"
            );
            ensure!(
                if c.layout == Some(Arrangement::Rail) {
                    c.rect.2
                } else {
                    c.rect.3
                } >= font + 2 * padding,
                "Navigation cross axis too small"
            );
        }

        if c.kind == Kind::DialogActions {
            ensure!(
                matches!(
                    c.layout,
                    Some(Arrangement::Horizontal | Arrangement::Vertical)
                ),
                "Invalid action arrangement"
            );
            let font = self.tokens.typography[&c.role.unwrap()].size;
            ensure!(
                if c.layout == Some(Arrangement::Vertical) {
                    c.rect.3 >= 2 * (font + 8)
                } else {
                    c.rect.2 >= 2 * (font * 3 + 12)
                },
                "Confirm/back action cells exceed bounds"
            );
        }
        Ok(())
    }
}

/// Essential controller-supplied slots must remain present even when their collection is empty.
pub fn required_bindings(screen: ScreenId) -> &'static [Binding] {
    use Binding::*;
    match screen {
        ScreenId::Library => &[
            Entries,
            SelectedTitle,
            SelectedMetadata,
            Navigation,
            Status,
            Hints,
        ],
        ScreenId::Recents | ScreenId::SearchResults | ScreenId::Apps => {
            &[Title, Entries, Navigation, Status, Hints]
        }
        ScreenId::SettingsRoot | ScreenId::SettingsPage => {
            &[Title, SettingsRows, Navigation, Status, Hints]
        }
        ScreenId::Search | ScreenId::Keyboard => {
            &[Title, KeyboardValue, Keyboard, Navigation, Status, Hints]
        }
        ScreenId::DiscPicker => &[Title, Subtitle, Discs, Actions, Hints],
        ScreenId::ContextMenu | ScreenId::IngameMenu => &[Title, Subtitle, Entries, Actions, Hints],
        ScreenId::Toast => &[Title, Message, Actions, Hints],
        ScreenId::NetworkDialog => &[
            Title,
            Message,
            StorageEntries,
            Progress,
            Subtitle,
            Actions,
            Hints,
        ],
        ScreenId::GuideSelector => &[Title, Guides, Navigation, Status, Hints],
        ScreenId::TextReader => &[Title, GuideText, Navigation, Status, Hints],
        ScreenId::OsdVolume | ScreenId::OsdBrightness => &[Title, Progress],
        ScreenId::GameDetail => &[
            SelectedTitle,
            SelectedMetadata,
            SelectedCover,
            Entries,
            Actions,
            Navigation,
            Status,
            Hints,
        ],
    }
}

// serde map collections otherwise silently accept duplicate JSON keys. Check all nesting first.
struct UniqueJson;
impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueJson;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON with unique object keys")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                _: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson)
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson)
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson)
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson)
            }
            fn visit_str<E: serde::de::Error>(
                self,
                _: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson)
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                while a.next_element::<UniqueJson>()?.is_some() {}
                Ok(UniqueJson)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut keys = BTreeSet::new();
                while let Some(key) = a.next_key::<String>()? {
                    if !keys.insert(key) {
                        return Err(serde::de::Error::custom("Duplicate JSON object key"));
                    }
                    a.next_value::<UniqueJson>()?;
                }
                Ok(UniqueJson)
            }
        }
        d.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn theme_pack_every_required_binding_and_duplicate_rejected() {
        let mut checked = 0;
        for id in ["orbit", "compact"] {
            let valid = Pack::builtin(id).unwrap();
            for screen in ScreenId::ALL {
                for binding in required_bindings(screen) {
                    if valid.schema_version == 2
                        && screen == ScreenId::Library
                        && *binding == Binding::SelectedMetadata
                    {
                        continue;
                    }
                    let mut p = valid.clone();
                    p.screens
                        .get_mut(&screen)
                        .unwrap()
                        .components
                        .retain(|c| c.binding != Some(*binding));
                    assert!(p.validate().is_err(), "{id}/{screen:?}/{binding:?}");
                    checked += 1;
                }
                let mut p = valid.clone();
                let c = p.screens[&screen]
                    .components
                    .iter()
                    .find(|c| c.binding.is_some())
                    .unwrap()
                    .clone();
                let mut duplicate = c;
                duplicate.id = "duplicate-slot".into();
                p.screens
                    .get_mut(&screen)
                    .unwrap()
                    .components
                    .push(duplicate);
                assert!(p.validate().is_err());
                checked += 1;
            }
            let mut p = valid.clone();
            p.screens
                .get_mut(&ScreenId::Library)
                .unwrap()
                .components
                .retain(|c| c.kind != Kind::Image);
            assert!(p.validate().is_err());
            checked += 1;
        }
        println!("required-slot/duplicate/art negative fixtures: {checked}");
    }
    #[test]
    fn theme_pack_child_bounds_negative_fixtures() {
        let valid = Pack::builtin("orbit").unwrap();
        for (screen, kind) in [
            (ScreenId::Keyboard, Kind::Keyboard),
            (ScreenId::Library, Kind::Hints),
            (ScreenId::DiscPicker, Kind::DialogActions),
            (ScreenId::Library, Kind::Status),
            (ScreenId::Library, Kind::Navigation),
        ] {
            let mut p = valid.clone();
            let c = p
                .screens
                .get_mut(&screen)
                .unwrap()
                .components
                .iter_mut()
                .find(|c| c.kind == kind)
                .unwrap();
            c.rect.2 = 1;
            c.rect.3 = 1;
            assert!(p.validate().is_err());
        }
        for case in 0..5 {
            let mut p = valid.clone();
            let c = p
                .screens
                .get_mut(&ScreenId::Library)
                .unwrap()
                .components
                .iter_mut()
                .find(|c| c.kind == Kind::Entries)
                .unwrap();
            match case {
                0 => {
                    c.art_size = Some(150);
                    c.caption_height = Some(18);
                    c.rect.1 = 200;
                    c.rect.3 = 220;
                }
                1 => c.caption_height = Some(0),
                2 => c.caption_height = Some(17),
                3 => c.padding = Some(24),
                _ => c.rect.3 = 140,
            };
            assert!(p.validate().is_err(), "Shelf case {case}");
        }
        for case in 0..2 {
            let mut p = valid.clone();
            let c = p
                .screens
                .get_mut(&ScreenId::Recents)
                .unwrap()
                .components
                .iter_mut()
                .find(|c| c.kind == Kind::Entries)
                .unwrap();
            if case == 0 {
                c.rect.2 = 28;
            } else {
                c.padding = Some(24);
            }
            assert!(p.validate().is_err());
        }
    }
    #[test]
    fn theme_pack_builtin_complete_distinct_and_geometry() {
        let a = Pack::builtin("orbit").unwrap();
        let b = Pack::builtin("compact").unwrap();
        for id in ScreenId::ALL {
            assert_ne!(
                serde_json::to_value(&a.screens[&id]).unwrap(),
                serde_json::to_value(&b.screens[&id]).unwrap()
            );
        }
        assert_eq!(
            a.component(ScreenId::Library, "entries").unwrap().art_size,
            Some(140)
        );
        assert_eq!(
            b.component(ScreenId::Library, "entries")
                .unwrap()
                .visible_count,
            Some(8)
        );
    }
    #[test]
    fn theme_pack_rejects_unknown_invalid_and_oversized() {
        let valid =
            serde_json::from_slice::<serde_json::Value>(Pack::builtin_bytes("orbit").unwrap())
                .unwrap();
        for change in 0..8 {
            let mut v = valid.clone();
            match change {
                0 => v["script"] = "exec".into(),
                1 => v["schema_version"] = 3.into(),
                2 => {
                    v["screens"].as_object_mut().unwrap().remove("game_detail");
                }
                3 => {
                    v["screens"]["library"]["components"][0]["rect"] =
                        serde_json::json!([630, 0, 20, 480])
                }
                4 => v["tokens"]["typography"]["body"]["size"] = 17.into(),
                5 => v["tokens"]["typography"]["body"]["font"] = "../font.ttf".into(),
                6 => v["screens"]["library"]["components"][0]["binding"] = "actions".into(),
                _ => v["screens"]["library"]["components"][0]["kind"] = "executable".into(),
            }
            assert!(
                Pack::parse(&serde_json::to_vec(&v).unwrap()).is_err(),
                "case {change}"
            );
        }
        assert!(Pack::parse(&vec![b' '; MANIFEST_LIMIT + 1]).is_err());
    }
    #[test]
    fn theme_pack_paths_and_png_budget() {
        for p in [
            "/a.png",
            "../a.png",
            "a/../b.png",
            "a//b.png",
            "a\\b.png",
            "http:a.png",
            "./a.png",
            "",
        ] {
            assert!(!valid_asset_path(p));
        }
        let mut p = Pack::builtin("orbit").unwrap();
        let asset = Asset {
            path: "safe.png".into(),
            sha256: "0".repeat(64),
            width: 640,
            height: 480,
            bytes: 32,
            required: true,
        };
        for i in 0..4 {
            p.assets.insert(format!("image-{i}"), asset.clone());
        }
        assert!(p.validate().is_err());
    }
    #[test]
    fn theme_pack_duplicate_keys_asset_reference_and_wrong_screen() {
        let bytes = std::str::from_utf8(Pack::builtin_bytes("orbit").unwrap()).unwrap();
        let duplicate = bytes.replacen(
            "\"schema_version\": 2",
            "\"schema_version\": 2, \"schema_version\": 2",
            1,
        );
        assert!(Pack::parse(duplicate.as_bytes()).is_err());
        let mut p = Pack::builtin("orbit").unwrap();
        p.screens
            .get_mut(&ScreenId::Library)
            .unwrap()
            .components
            .last_mut()
            .unwrap()
            .asset_id = Some("missing".into());
        assert!(p.validate().is_err());
        let mut p = Pack::builtin("orbit").unwrap();
        let rows = p.screens[&ScreenId::SettingsPage]
            .components
            .last()
            .unwrap()
            .clone();
        p.screens
            .get_mut(&ScreenId::OsdVolume)
            .unwrap()
            .components
            .push(rows);
        assert!(p.validate().is_err());
    }
    #[test]
    fn theme_pack_shared_json_fixture_parity_export() {
        let mut themes = serde_json::Map::new();
        for id in ["orbit", "compact"] {
            let p = Pack::builtin(id).unwrap();
            themes.insert(
                id.into(),
                serde_json::json!({"tokens":p.tokens,"screens":p.screens}),
            );
        }
        let export = serde_json::json!({"schema_version":1,"canvas":{"width":640,"height":480},
            "fixture_id":"same-controller-state-v1","origin":"Rust typed descriptor resolver export",
            "themes":themes});
        if let Ok(root) = std::env::var("ALLIUM_THEME_CONTRACT_ROOT") {
            let path = std::path::Path::new(&root);
            let expected: serde_json::Value = serde_json::from_slice(
                &std::fs::read(path.join("resolved-layout-fixture.json")).unwrap(),
            )
            .unwrap();
            for id in ["orbit", "compact"] {
                for field in ["tokens", "screens"] {
                    assert!(
                        expected["themes"][id][field] == export["themes"][id][field],
                        "Resolved {id}/{field} differs"
                    );
                }
            }
            for id in ["orbit", "compact"] {
                assert_eq!(
                    std::fs::read(path.join("packs").join(id).join("theme.json")).unwrap(),
                    Pack::builtin_bytes(id).unwrap()
                );
            }
        }
        if let Ok(root) = std::env::var("ALLIUM_THEME_PROOF_DIR") {
            std::fs::write(
                std::path::Path::new(&root).join("rust-resolved-layout-fixture.json"),
                serde_json::to_vec_pretty(&export).unwrap(),
            )
            .unwrap();
        }
    }
    #[test]
    fn theme_port_merged_keyboard_geometry_rejects_oversize_cells() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../themes/orbit-hero/theme.json");
        let mut pack = Pack::parse(&std::fs::read(root).unwrap()).unwrap();
        let mut keyboard = pack.screens[&ScreenId::Search]
            .components
            .iter()
            .find(|c| c.kind == Kind::Keyboard)
            .unwrap()
            .clone();
        keyboard.paint.as_mut().unwrap().run = None;
        pack.validate_ink_component(ScreenId::Search, &keyboard)
            .unwrap();
        pack.ink_typography
            .get_mut(&Role::Hint)
            .unwrap()
            .line_height = Some(28);
        assert_eq!(
            pack.validate_ink_component(ScreenId::Search, &keyboard)
                .unwrap_err()
                .to_string(),
            "Ink keyboard exceeds bounds"
        );
    }
}
