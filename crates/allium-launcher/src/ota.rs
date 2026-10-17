use anyhow::{Context, Result};
use common::constants::{ALLIUM_SD_ROOT, ALLIUM_UPDATE_SETTINGS};
use const_hex::ToHexExt;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Instant;
use tokio::sync::mpsc;

const GITHUB_REPOSITORY: &str = "goweiwen/Allium";
const RELEASE_FILE: &str = "allium-armv7-unknown-linux-gnueabihf.zip";
const USER_AGENT: &str = "Allium-OTA-Updater";

static UPDATE_FILE_PATH: LazyLock<PathBuf> =
    LazyLock::new(|| ALLIUM_SD_ROOT.join("allium-ota.zip"));

/// The device has no system certificate store, so the Mozilla root set is compiled in instead of
/// going through reqwest's platform verifier.
static TLS_CONFIG: LazyLock<rustls::ClientConfig> = LazyLock::new(|| {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();

    // A preconfigured config bypasses reqwest's own ALPN setup, and http2 is not compiled in.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
});

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .tls_backend_preconfigured(TLS_CONFIG.clone())
        .build()
        .context("Failed to build HTTP client")
}

/// Update channel selection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum UpdateChannel {
    #[default]
    Stable,
    Nightly,
}

/// Update settings that are persisted to disk
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateSettings {
    pub channel: UpdateChannel,
}

impl UpdateSettings {
    pub fn load() -> Result<Self> {
        if ALLIUM_UPDATE_SETTINGS.exists() {
            debug!("found update settings, loading from file");
            let file = File::open(ALLIUM_UPDATE_SETTINGS.as_path())?;
            if let Ok(json) = serde_json::from_reader(file) {
                return Ok(json);
            }
            warn!("failed to read update settings file, removing");
            fs::remove_file(ALLIUM_UPDATE_SETTINGS.as_path())?;
        }
        Ok(Self::default())
    }

    pub fn save(&self) -> Result<()> {
        let file = File::create(ALLIUM_UPDATE_SETTINGS.as_path())?;
        serde_json::to_writer(file, &self)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub assets: Vec<GitHubAsset>,
    #[serde(default)]
    pub prerelease: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub digest: Option<String>,
}

/// GitHub API response for git reference (tag)
#[derive(Debug, Clone, Deserialize)]
struct GitRef {
    object: GitObject,
}

#[derive(Debug, Clone, Deserialize)]
struct GitObject {
    sha: String,
    #[serde(rename = "type")]
    object_type: String,
}

/// GitHub API response for tag object (annotated tags)
#[derive(Debug, Clone, Deserialize)]
struct GitTag {
    object: GitTagObject,
}

#[derive(Debug, Clone, Deserialize)]
struct GitTagObject {
    sha: String,
}

/// Check if an update is available for the given channel
/// Returns the GitHubRelease if an update is available
pub async fn check_for_update(channel: UpdateChannel) -> Result<Option<GitHubRelease>> {
    let current_version = tokio::task::spawn_blocking(|| common::constants::ALLIUM_VERSION.clone())
        .await
        .context("Version storage worker failed")?;
    info!("Current version: {}", current_version);

    let release = match channel {
        UpdateChannel::Stable => get_latest_stable_release().await?,
        UpdateChannel::Nightly => get_latest_nightly_release().await?,
    };
    let latest_version = get_release_version(&release).await;
    info!("Latest version: {}", latest_version);

    if current_version != latest_version {
        Ok(Some(release))
    } else {
        Ok(None)
    }
}

/// Get the version string for a release (fetches commit hash for nightly/prerelease)
pub async fn get_release_version(release: &GitHubRelease) -> String {
    if release.prerelease {
        // For nightly, use "nightly-<commit-hash>"
        if let Ok(commit_sha) = get_tag_commit_sha(&release.tag_name).await {
            let short_hash = if commit_sha.len() >= 7 {
                &commit_sha[..7]
            } else {
                &commit_sha
            };
            return format!("nightly-{}", short_hash);
        }
        "nightly".to_string()
    } else {
        release.tag_name.clone()
    }
}

/// Get the latest stable release from GitHub
async fn get_latest_stable_release() -> Result<GitHubRelease> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        GITHUB_REPOSITORY
    );

    let client = client()?;

    let release: GitHubRelease = client
        .get(&url)
        .send()
        .await
        .context("Failed to fetch latest release")?
        .json()
        .await
        .context("Failed to parse release JSON")?;

    Ok(release)
}

/// Get the latest nightly (prerelease) from GitHub
async fn get_latest_nightly_release() -> Result<GitHubRelease> {
    let url = format!(
        "https://api.github.com/repos/{}/releases",
        GITHUB_REPOSITORY
    );

    let client = client()?;

    let releases: Vec<GitHubRelease> = client
        .get(&url)
        .query(&[("per_page", "20")])
        .send()
        .await
        .context("Failed to fetch releases")?
        .json()
        .await
        .context("Failed to parse releases JSON")?;

    // Find the first prerelease (they're returned in date order, newest first)
    releases
        .into_iter()
        .find(|r| r.prerelease)
        .context("No nightly release found")
}

/// Fetch the commit SHA for a given tag
async fn get_tag_commit_sha(tag_name: &str) -> Result<String> {
    let client = client()?;

    let url = format!(
        "https://api.github.com/repos/{}/git/ref/tags/{}",
        GITHUB_REPOSITORY, tag_name
    );

    let git_ref: GitRef = client
        .get(&url)
        .send()
        .await
        .context("Failed to fetch tag reference")?
        .json()
        .await
        .context("Failed to parse tag reference JSON")?;

    if git_ref.object.object_type == "tag" {
        // Annotated tag: we need to resolve to get the commit SHA
        let tag_url = format!(
            "https://api.github.com/repos/{}/git/tags/{}",
            GITHUB_REPOSITORY, git_ref.object.sha
        );

        let git_tag: GitTag = client
            .get(&tag_url)
            .send()
            .await
            .context("Failed to fetch tag object")?
            .json()
            .await
            .context("Failed to parse tag object JSON")?;

        Ok(git_tag.object.sha)
    } else {
        // Lightweight tag: points directly to commit
        Ok(git_ref.object.sha)
    }
}

/// Download progress information
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: u64,
}

impl DownloadProgress {
    pub fn percentage(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.downloaded as f32 / self.total as f32 * 100.0
        }
    }
}

/// Download event - either progress or completion/error
#[derive(Debug, Clone)]
pub enum DownloadEvent {
    Progress(DownloadProgress),
    Completed,
    Error(String),
}

enum StorageMessage {
    Chunk(Vec<u8>),
    Finish,
}

// A bounded queue limits SD-card backpressure to two chunks. The worker also owns
// the writer's drop, so cancellation cannot flush a BufWriter on the UI thread.
fn spawn_storage_writer<W, F>(
    create: F,
) -> (
    mpsc::Sender<StorageMessage>,
    tokio::task::JoinHandle<Result<String>>,
)
where
    W: Write + Send + 'static,
    F: FnOnce() -> std::io::Result<W> + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel(2);
    let task = tokio::task::spawn_blocking(move || {
        let mut writer = create().context("Failed to create update file")?;
        let mut hasher = Sha256::new();
        while let Some(message) = rx.blocking_recv() {
            match message {
                StorageMessage::Chunk(chunk) => {
                    writer
                        .write_all(&chunk)
                        .context("Failed to write to file")?;
                    hasher.update(&chunk);
                }
                StorageMessage::Finish => {
                    writer.flush().context("Failed to flush file")?;
                    return Ok(hasher.finalize().encode_hex());
                }
            }
        }
        anyhow::bail!("Update download cancelled before completion")
    });
    (tx, task)
}

/// Download a release to /mnt/SDCARD/allium-ota.zip, reporting the progress to event_tx
pub async fn download_update_with_progress(
    release: &GitHubRelease,
    event_tx: Option<mpsc::UnboundedSender<DownloadEvent>>,
) -> Result<()> {
    // Check if there's enough space (need 300MB)
    #[cfg(feature = "miyoo")]
    {
        let output = tokio::task::spawn_blocking(|| {
            std::process::Command::new("df")
                .args(["-m", "/mnt/SDCARD"])
                .output()
        })
        .await
        .context("Disk space worker failed")?
        .context("Failed to check disk space")?;

        let output_str = String::from_utf8_lossy(&output.stdout);
        let available_space: i32 = output_str
            .lines()
            .nth(1)
            .and_then(|line| line.split_whitespace().nth(3))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        if available_space < 300 {
            anyhow::bail!(
                "Insufficient disk space. Need 300MB, have {}MB",
                available_space
            );
        }
    }

    info!("Downloading update version {}", release.tag_name);

    // Find the asset with the expected filename
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == RELEASE_FILE)
        .context(format!("Asset '{}' not found in release", RELEASE_FILE))?;

    let expected_hash = asset
        .digest
        .as_ref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .context("SHA256 digest not found for release asset")?;

    info!("Downloading from: {}", asset.browser_download_url);
    let mut response = client()?
        .get(&asset.browser_download_url)
        .send()
        .await
        .context("Failed to download update")?;

    if !response.status().is_success() {
        anyhow::bail!("Failed to download update: HTTP {}", response.status());
    }

    // Get content length for progress reporting
    let total_size = response.content_length().unwrap_or(0);
    let mut downloaded: u64 = 0;

    // Create file and hasher
    info!("Writing update to {}", UPDATE_FILE_PATH.display());
    let (storage_tx, storage_task) =
        spawn_storage_writer(|| File::create(&*UPDATE_FILE_PATH).map(BufWriter::new));

    // Stream download to file while calculating hash
    let mut last_progress_update = Instant::now();
    while let Some(chunk) = response.chunk().await.context("Failed to read chunk")? {
        if storage_tx
            .send(StorageMessage::Chunk(chunk.to_vec()))
            .await
            .is_err()
        {
            storage_task
                .await
                .context("Update storage worker failed")??;
            anyhow::bail!("Update storage worker stopped early");
        }

        downloaded += chunk.len() as u64;

        // Send progress update at most once per second
        if let Some(ref tx) = event_tx {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_secs() >= 1 {
                last_progress_update = now;
                info!(
                    "Downloaded {}% ({}/{} bytes)",
                    downloaded
                        .saturating_mul(100)
                        .checked_div(total_size)
                        .unwrap_or(0),
                    downloaded,
                    total_size
                );
                let _ = tx.send(DownloadEvent::Progress(DownloadProgress {
                    downloaded,
                    total: total_size,
                }));
            }
        }
    }

    // Send final progress update
    if let Some(ref tx) = event_tx {
        let _ = tx.send(DownloadEvent::Progress(DownloadProgress {
            downloaded,
            total: total_size,
        }));
    }

    // Always join the worker to preserve its filesystem error if its receiver closed.
    let _ = storage_tx.send(StorageMessage::Finish).await;
    let calculated_hash = storage_task
        .await
        .context("Update storage worker failed")??;

    info!("Verifying SHA256 checksum...");

    if calculated_hash != expected_hash {
        // Delete the file if verification fails
        let _ = tokio::task::spawn_blocking(|| fs::remove_file(&*UPDATE_FILE_PATH)).await;
        let error_msg = format!(
            "SHA256 checksum mismatch!\nExpected: {}\nCalculated: {}",
            expected_hash, calculated_hash
        );
        if let Some(ref tx) = event_tx {
            let _ = tx.send(DownloadEvent::Error(error_msg.clone()));
        }
        anyhow::bail!(error_msg);
    }

    info!("Update downloaded successfully");
    if let Some(ref tx) = event_tx {
        let _ = tx.send(DownloadEvent::Completed);
    }
    Ok(())
}

pub fn update_file_exists() -> bool {
    UPDATE_FILE_PATH.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn slow_download_storage_keeps_runtime_timers_responsive() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use std::time::Duration;

        struct SlowWriter {
            entered: mpsc::UnboundedSender<usize>,
            release: std::sync::mpsc::Receiver<()>,
            completed: Arc<AtomicUsize>,
            bytes: Arc<std::sync::Mutex<Vec<u8>>>,
        }
        impl Write for SlowWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.entered.send(1).unwrap();
                self.release.recv_timeout(Duration::from_secs(2)).unwrap();
                self.bytes.lock().unwrap().extend_from_slice(bytes);
                self.completed.store(2, Ordering::SeqCst);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.entered.send(2).unwrap();
                self.release.recv_timeout(Duration::from_secs(2)).unwrap();
                self.completed.store(3, Ordering::SeqCst);
                Ok(())
            }
        }

        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let completed = Arc::new(AtomicUsize::new(0));
        let worker_completed = Arc::clone(&completed);
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let worker_bytes = Arc::clone(&bytes);
        let (storage, task) = spawn_storage_writer(move || {
            entered_tx.send(0).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            worker_completed.store(1, Ordering::SeqCst);
            Ok(SlowWriter {
                entered: entered_tx,
                release: release_rx,
                completed: worker_completed,
                bytes: worker_bytes,
            })
        });
        storage
            .send(StorageMessage::Chunk(b"update bytes".to_vec()))
            .await
            .unwrap();
        storage.send(StorageMessage::Finish).await.unwrap();
        for stage in 0..3 {
            assert_eq!(entered_rx.recv().await, Some(stage));
            tokio::time::sleep(Duration::from_millis(20)).await;
            // The timer ran while create, write or flush was still blocked.
            assert_eq!(completed.load(Ordering::SeqCst), stage);
            release_tx.send(()).unwrap();
        }
        let hash = task.await.unwrap().unwrap();
        assert_eq!(*bytes.lock().unwrap(), b"update bytes");
        assert_eq!(hash, Sha256::digest(b"update bytes").encode_hex());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn download_storage_propagates_create_write_and_flush_errors() {
        struct FailingWriter(bool);
        impl Write for FailingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0 {
                    Err(std::io::Error::other("write error"))
                } else {
                    Ok(bytes.len())
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("flush error"))
            }
        }
        for failure in ["create", "write", "flush"] {
            let (storage, task) = spawn_storage_writer(move || {
                if failure == "create" {
                    Err(std::io::Error::other("create error"))
                } else {
                    Ok(FailingWriter(failure == "write"))
                }
            });
            let _ = storage.send(StorageMessage::Chunk(vec![1])).await;
            let _ = storage.send(StorageMessage::Finish).await;
            let error = task.await.unwrap().unwrap_err();
            assert!(
                format!("{error:#}").contains(&format!("{failure} error")),
                "{error:#}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_download_storage_drops_writer_on_worker() {
        struct DropWriter {
            dropped: Option<tokio::sync::oneshot::Sender<bool>>,
            ui_thread: std::thread::ThreadId,
        }
        impl Write for DropWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl Drop for DropWriter {
            fn drop(&mut self) {
                let _ = self
                    .dropped
                    .take()
                    .unwrap()
                    .send(std::thread::current().id() != self.ui_thread);
            }
        }
        let ui_thread = std::thread::current().id();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let (storage, task) = spawn_storage_writer(move || {
            Ok(DropWriter {
                dropped: Some(dropped_tx),
                ui_thread,
            })
        });
        drop(storage);
        assert!(task.await.unwrap().is_err());
        assert!(dropped_rx.await.unwrap());
    }

    fn make_stable_release(tag: &str) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.to_string(),
            assets: vec![],
            prerelease: false,
        }
    }

    #[tokio::test]
    async fn test_get_release_version_stable() {
        let release = make_stable_release("v0.29.0");
        let version = get_release_version(&release).await;
        assert_eq!(version, "v0.29.0");
    }

    #[tokio::test]
    async fn test_stable_version_format_matches_semver() {
        let release = make_stable_release("v1.2.3");
        let version = get_release_version(&release).await;
        // Should match semver format: vX.Y.Z
        assert!(
            version.starts_with('v'),
            "Version should start with 'v': {}",
            version
        );
        let version = version.trim_start_matches('v').to_string();
        let parts: Vec<&str> = version.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].parse::<u32>().is_ok());
        assert!(parts[1].parse::<u32>().is_ok());
        assert!(parts[2].parse::<u32>().is_ok());
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_check_for_update_stable() {
        let result = check_for_update(UpdateChannel::Stable).await;
        assert!(result.is_ok(), "Failed to check for update: {:?}", result);
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_check_for_update_nightly() {
        let result = check_for_update(UpdateChannel::Nightly).await;
        assert!(result.is_ok(), "Failed to check for update: {:?}", result);
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_stable_release_version_format() {
        let release = get_latest_stable_release()
            .await
            .expect("Failed to get stable release");

        let version = get_release_version(&release).await;
        println!("Stable version: {}", version);

        // Stable should not have a dash (no commit hash)
        assert!(
            version.starts_with('v'),
            "Stable version should start with 'v': {}",
            version
        );

        // Should be semver format
        let parts: Vec<&str> = version.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "Version should have 3 parts (semver): {}",
            version
        );
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_nightly_release_version_format() {
        let release = get_latest_nightly_release()
            .await
            .expect("Failed to get nightly release");

        let version = get_release_version(&release).await;
        println!("Nightly version: {}", version);

        // Nightly should have format: nightly-HHHHHHH
        assert!(
            version.starts_with("nightly"),
            "Nightly version should start with 'nightly-': {}",
            version
        );

        let parts: Vec<&str> = version.split('-').collect();
        assert_eq!(
            parts.len(),
            2,
            "Nightly version should have base and hash: {}",
            version
        );

        // Commit hash should be 7 characters
        assert_eq!(
            parts[1].len(),
            7,
            "Commit hash should be 7 characters: {}",
            parts[1]
        );
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_get_latest_stable_release() {
        let result = get_latest_stable_release().await;
        assert!(result.is_ok(), "Failed to get latest release: {:?}", result);

        let release = result.unwrap();
        assert!(!release.tag_name.is_empty(), "Tag name should not be empty");
        assert!(!release.assets.is_empty(), "Assets should not be empty");

        // Check if the expected asset exists
        let asset = release.assets.iter().find(|a| a.name == RELEASE_FILE);
        assert!(asset.is_some());

        // Check if SHA256 digest exists
        let asset = asset.unwrap();
        assert!(asset.digest.as_ref().unwrap().starts_with("sha256:"));
    }
}
