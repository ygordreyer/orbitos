use std::collections::VecDeque;
use std::fs;
use std::mem::drop;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;
use common::constants::ALLIUM_VERSION;
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::view::{Button, ButtonHint, ButtonHints, Label, Select, SettingsList, View};
use log::{error, info};
use tokio::sync::mpsc::{self, Sender};

use crate::ota::{self, GitHubRelease, UpdateChannel, UpdateSettings};
use crate::view::settings::{ChildState, SettingsChild};

#[derive(Debug, Clone)]
enum UpdateStatus {
    Idle,
    Checking,
    Available(GitHubRelease), // Store the available release
    Downloading(f32),         // Store download progress percentage
    Ready,
    UpToDate,
    Error(String),
}

impl UpdateStatus {
    fn text(&self, locale: &Locale) -> String {
        match self {
            UpdateStatus::Idle => locale.t("settings-system-update-check"),
            UpdateStatus::Checking => locale.t("settings-system-update-checking"),
            UpdateStatus::Available(_) => locale.t("settings-system-update-available"),
            UpdateStatus::Downloading(progress) => format!(
                "{} ({:.1}%)",
                locale.t("settings-system-update-downloading"),
                progress
            ),
            UpdateStatus::Ready => locale.t("settings-system-update-restart-to-update"),
            UpdateStatus::UpToDate => locale.t("settings-system-update-up-to-date"),
            UpdateStatus::Error(msg) => msg.to_string(),
        }
    }
}

impl From<usize> for UpdateChannel {
    fn from(value: usize) -> Self {
        match value {
            0 => UpdateChannel::Stable,
            1 => UpdateChannel::Nightly,
            _ => UpdateChannel::Stable,
        }
    }
}

/// Event for version check results
enum VersionCheckEvent {
    Found {
        release: GitHubRelease,
        version: String, // Pre-resolved version string (includes commit hash for nightly)
    },
    UpToDate,
    Error(String),
}

// List indices (current version is index 0, but read-only)
#[allow(dead_code)]
const INDEX_CURRENT_VERSION: usize = 0;
const INDEX_LATEST_VERSION: usize = 1;
const INDEX_UPDATE_CHANNEL: usize = 2;
const INDEX_SYSTEM_UPDATE: usize = 3;

pub struct SystemUpdate {
    rect: Rect,
    res: Resources,
    list: SettingsList,
    button_hints: ButtonHints<String>,
    has_wifi: bool,
    update_status: UpdateStatus,
    download_rx: Option<mpsc::UnboundedReceiver<ota::DownloadEvent>>,
    version_check_rx: Option<mpsc::UnboundedReceiver<VersionCheckEvent>>,
    latest_version: Option<String>,
    update_channel: UpdateChannel,
    toast_shown: bool,
}

impl SystemUpdate {
    pub fn new(rect: Rect, res: Resources, state: Option<ChildState>) -> Self {
        let Rect { x, y, w, .. } = rect;

        let has_wifi = DefaultPlatform::has_wifi();

        let locale = res.get::<Locale>();
        let styles = res.get::<Stylesheet>();

        // Determine initial update status
        let update_status = if ota::update_file_exists() {
            UpdateStatus::Ready
        } else {
            UpdateStatus::Idle
        };

        let update_channel = UpdateSettings::load()
            .map(|s| s.channel)
            .unwrap_or_default();

        let mut labels = vec![];
        let mut values: Vec<Box<dyn View>> = vec![];

        // Current Version
        labels.push(locale.t("settings-about-version"));
        values.push(Box::new(Label::new(
            Point::zero(),
            common::product::VERSION.to_owned(),
            Alignment::Right,
            None,
        )));

        // Latest Version (will be updated after fetch)
        labels.push(locale.t("settings-system-latest-version"));
        values.push(Box::new(Label::new(
            Point::zero(),
            locale.t("settings-system-update-checking"),
            Alignment::Right,
            None,
        )));

        // Update Channel (using Select)
        labels.push(locale.t("settings-system-update-channel"));
        values.push(Box::new(Select::new(
            Point::zero(),
            update_channel as usize,
            vec![
                locale.t("settings-system-update-channel-stable"),
                locale.t("settings-system-update-channel-nightly"),
            ],
            Alignment::Right,
        )));

        // System Update (using Button)
        labels.push(locale.t("settings-system-update"));
        values.push(Box::new(Button::new(Label::new(
            Point::zero(),
            update_status.text(&locale),
            Alignment::Right,
            None,
        ))));

        let mut button_hints = ButtonHints::new(
            res.clone(),
            vec![],
            vec![ButtonHint::new(
                res.clone(),
                Point::zero(),
                Key::B,
                locale.t("button-back"),
                Alignment::Right,
            )],
        );

        let button_hints_rect = button_hints.bounding_box(&styles);
        let list_height = (button_hints_rect.y - y) as u32;

        drop(locale);
        drop(styles);

        let styles = res.get::<Stylesheet>();
        let mut list = SettingsList::new(
            res.clone(),
            Rect::new(
                x + styles.ui.margin_x,
                y,
                w - styles.ui.margin_x as u32 * 2,
                list_height,
            ),
            labels,
            values,
            styles.ui.ui_font.size + styles.ui.padding_y as u32,
        );
        drop(styles);
        if let Some(state) = state {
            list.select(state.selected);
        }

        let mut system = Self {
            rect,
            res,
            list,
            button_hints,
            has_wifi,
            update_status,
            download_rx: None,
            version_check_rx: None,
            latest_version: None,
            update_channel,
            toast_shown: false,
        };

        // Start fetching latest version in background
        if has_wifi && matches!(system.update_status, UpdateStatus::Idle) {
            system.start_version_check();
        }

        system
    }

    /// Starts a background task to check for the latest version
    fn start_version_check(&mut self) {
        // Show "Checking..." in latest version label
        let locale = self.res.get::<Locale>();
        self.list.set_right(
            INDEX_LATEST_VERSION,
            Box::new(Label::new(
                Point::zero(),
                locale.t("settings-system-update-checking"),
                Alignment::Right,
                None,
            )),
        );
        drop(locale);

        // Create channel and spawn background task
        let (tx, rx) = mpsc::unbounded_channel();
        self.version_check_rx = Some(rx);

        let channel = self.update_channel;
        tokio::spawn(async move {
            match ota::check_for_update(channel).await {
                Ok(Some(release)) => {
                    // Resolve version string (fetches commit hash for nightly)
                    let version = ota::get_release_version(&release).await;
                    let _ = tx.send(VersionCheckEvent::Found { release, version });
                }
                Ok(None) => {
                    let _ = tx.send(VersionCheckEvent::UpToDate);
                }
                Err(e) => {
                    error!("Failed to check for updates: {:?}", e);
                    let _ = tx.send(VersionCheckEvent::Error(e.to_string()));
                }
            }
        });
    }

    fn update_status_label(&mut self) {
        let locale = self.res.get::<Locale>();
        let update_text = self.update_status.text(&locale);

        self.list.set_right(
            INDEX_SYSTEM_UPDATE,
            Box::new(Button::new(Label::new(
                Point::zero(),
                update_text,
                Alignment::Right,
                None,
            ))),
        );
    }

    fn update_latest_version_label(&mut self) {
        if self.has_wifi {
            let text = self
                .latest_version
                .clone()
                .unwrap_or_else(|| "-".to_string());

            self.list.set_right(
                INDEX_LATEST_VERSION,
                Box::new(Label::new(Point::zero(), text, Alignment::Right, None)),
            );
        }
    }

    fn check_background_tasks(&mut self) {
        // Check version check results
        let mut version_events = Vec::new();
        if let Some(ref mut rx) = self.version_check_rx {
            while let Ok(event) = rx.try_recv() {
                version_events.push(event);
            }
        }

        for event in version_events {
            match event {
                VersionCheckEvent::Found { release, version } => {
                    info!("Latest version: {}", version);
                    self.latest_version = Some(version);
                    self.update_status = UpdateStatus::Available(release);
                    self.version_check_rx = None;
                }
                VersionCheckEvent::UpToDate => {
                    info!("Already up to date");
                    self.latest_version = Some(ALLIUM_VERSION.clone());
                    self.update_status = UpdateStatus::UpToDate;
                    self.version_check_rx = None;
                }
                VersionCheckEvent::Error(msg) => {
                    error!("Failed to check for updates: {}", msg);
                    self.latest_version = None;
                    self.update_status = UpdateStatus::Error(msg);
                    self.version_check_rx = None;
                }
            }
            self.update_latest_version_label();
            self.update_status_label();
        }

        // Check download progress
        let mut download_events = Vec::new();
        if let Some(ref mut rx) = self.download_rx {
            while let Ok(event) = rx.try_recv() {
                download_events.push(event);
            }
        }

        for event in download_events {
            match event {
                ota::DownloadEvent::Progress(progress) => {
                    let percentage = progress.percentage();
                    if let UpdateStatus::Downloading(_) = self.update_status {
                        self.update_status = UpdateStatus::Downloading(percentage);
                        self.update_status_label();
                    }
                }
                ota::DownloadEvent::Completed => {
                    // Download completed successfully
                    info!("Download verification complete, rebooting to apply update");
                    let _ = fs::remove_file("/tmp/stay_awake");
                    self.update_status = UpdateStatus::Ready;
                    self.update_status_label();
                    self.download_rx = None;
                    // Reboot to apply the update
                    let _ = std::process::Command::new("shutdown").arg("-r").spawn();
                }
                ota::DownloadEvent::Error(msg) => {
                    // Download failed
                    error!("Download failed: {}", msg);
                    let _ = fs::remove_file("/tmp/stay_awake");
                    self.update_status = UpdateStatus::Error(msg);
                    self.update_status_label();
                    self.download_rx = None;
                }
            }
        }
    }

    async fn handle_update_action(&mut self) -> Result<()> {
        if !self.has_wifi {
            return Ok(());
        }

        let selected = self.list.selected();

        if selected != INDEX_SYSTEM_UPDATE {
            return Ok(());
        }

        match &self.update_status {
            UpdateStatus::Idle => {
                // Check for updates
                info!("Checking for updates...");
                self.update_status = UpdateStatus::Checking;
                self.update_status_label();

                match ota::check_for_update(self.update_channel).await {
                    Ok(Some(release)) => {
                        let version = ota::get_release_version(&release).await;
                        info!("Update available: v{}", version);
                        self.latest_version = Some(version);
                        self.update_latest_version_label();
                        self.update_status = UpdateStatus::Available(release);
                    }
                    Ok(None) => {
                        info!("Already up to date");
                        // Set latest version to current version when up to date
                        self.latest_version = Some(ALLIUM_VERSION.clone());
                        self.update_latest_version_label();
                        self.update_status = UpdateStatus::UpToDate;
                    }
                    Err(e) => {
                        error!("Failed to check for updates: {}", e);
                        self.update_status = UpdateStatus::Error(e.to_string());
                    }
                }
                self.update_status_label();
            }
            UpdateStatus::Available(release) => {
                // Download update in background task
                let version = release.tag_name.trim_start_matches('v');
                info!("Downloading update v{}...", version);
                let release = release.clone();

                // Create stay_awake file to prevent sleep during download
                if let Err(e) = fs::write("/tmp/stay_awake", "") {
                    error!("Failed to create stay_awake file: {}", e);
                }

                // Create event channel
                let (event_tx, event_rx) = mpsc::unbounded_channel();
                self.download_rx = Some(event_rx);

                // Start with 0% progress
                self.update_status = UpdateStatus::Downloading(0.0);
                self.update_status_label();

                // Spawn download task
                tokio::spawn(async move {
                    let result =
                        ota::download_update_with_progress(&release, Some(event_tx.clone())).await;
                    if let Err(e) = result {
                        // Send error event if the task failed without sending one
                        let _ = event_tx.send(ota::DownloadEvent::Error(e.to_string()));
                    }
                });
            }
            UpdateStatus::UpToDate => {
                // Already up to date, do nothing
            }
            UpdateStatus::Ready => {
                // Should reboot
            }
            UpdateStatus::Error(_) => {
                // Reset to idle on error
                self.update_status = UpdateStatus::Idle;
                self.update_status_label();
            }
            _ => {}
        }

        Ok(())
    }

    fn handle_channel_change(&mut self, channel: UpdateChannel) {
        if self.update_channel != channel {
            self.update_channel = channel;
            // Reset update status when channel changes
            self.update_status = UpdateStatus::Idle;
            self.latest_version = None;
            self.update_status_label();

            // Save channel setting
            let settings = UpdateSettings { channel };
            if let Err(e) = settings.save() {
                error!("Failed to save update settings: {}", e);
            }

            // Re-fetch latest version for new channel
            self.start_version_check();
        }
    }
}

#[async_trait(?Send)]
impl View for SystemUpdate {
    fn update(&mut self, _dt: Duration) {
        self.check_background_tasks();
    }

    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        // Check for background task updates
        self.check_background_tasks();

        let mut drawn = false;

        drawn |= self.list.should_draw() && self.list.draw(display, styles)?;
        drawn |= self.button_hints.should_draw() && self.button_hints.draw(display, styles)?;

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        self.list.should_draw() || self.button_hints.should_draw()
    }

    fn set_should_draw(&mut self) {
        self.list.set_should_draw();
        self.button_hints.set_should_draw();
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        // First, let the list handle the event (for Select/Button interactions)
        if self
            .list
            .handle_key_event(event, commands.clone(), bubble)
            .await?
        {
            // Check for value changes from Select or Button
            while let Some(command) = bubble.pop_front() {
                if let Command::ValueChanged(i, val) = command {
                    match i {
                        INDEX_UPDATE_CHANNEL => {
                            let channel: UpdateChannel = (val.as_int().unwrap() as usize).into();
                            self.handle_channel_change(channel);
                        }
                        INDEX_SYSTEM_UPDATE => {
                            // Button was pressed - handle update action
                            self.handle_update_action().await?;
                        }
                        _ => {}
                    }
                }
            }
            return Ok(true);
        }

        match event {
            KeyEvent::Pressed(Key::B) => {
                // Don't allow exiting while downloading
                if matches!(self.update_status, UpdateStatus::Downloading(_)) {
                    return Ok(true);
                }
                // Dismiss toast if it was shown
                if self.toast_shown {
                    commands.send(Command::DismissToast).await?;
                    self.toast_shown = false;
                }
                bubble.push_back(Command::CloseView);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn children(&self) -> Vec<&dyn View> {
        vec![&self.list, &self.button_hints]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        vec![&mut self.list, &mut self.button_hints]
    }

    fn bounding_box(&mut self, _styles: &Stylesheet) -> Rect {
        self.rect
    }

    fn set_position(&mut self, _point: Point) {
        unimplemented!()
    }
}

impl SettingsChild for SystemUpdate {
    fn save(&self) -> ChildState {
        ChildState {
            selected: self.list.selected(),
        }
    }
}
