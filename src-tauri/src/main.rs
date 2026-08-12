use reqwest::{blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{command, AppHandle, Emitter, Manager, State};
use url::Url;

const SETTINGS_FILE: &str = ".yt-dlp-tauri-settings";
const RUNTIME_DIRECTORY: &str = "runtime-tools-v1";
const RUNTIME_MANIFEST_FILE: &str = "runtime-manifest.json";
const RUNTIME_SCHEMA_VERSION: u8 = 1;
const YT_DLP_VERSION: &str = "2026.07.04";
const FFMPEG_RELEASE: &str = "b6.1.1";
const MAX_RUNTIME_ASSET_BYTES: u64 = 500 * 1024 * 1024;
const DOWNLOAD_WORKER_LIMIT: usize = 4;
const CHECKPOINT_SCHEMA_VERSION: u8 = 1;
const PLAYLIST_LAYOUT_VERSION: &str = "playlist-static-index-v1";
const PODCAST_LAYOUT_VERSION: &str = "podcast-static-index-v1";
const SINGLE_LAYOUT_VERSION: &str = "single-v1";

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum DownloadType {
    Single,
    Playlist,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeSetupStatus {
    ready: bool,
    message: String,
    yt_dlp_version: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeSetupProgress {
    current: usize,
    total: usize,
    component: String,
    message: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeManifest {
    schema_version: u8,
    platform: String,
    yt_dlp_version: String,
    ffmpeg_release: String,
}

struct RuntimeAsset {
    component: &'static str,
    file_name: &'static str,
    url: &'static str,
    sha256: &'static str,
}

#[derive(Clone)]
struct RuntimeTools {
    directory: PathBuf,
    yt_dlp: PathBuf,
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

/// The outcome of a download command. `status` lets the UI distinguish a
/// completed download from one that the user paused or stopped, all of
/// which resolve the invoke promise successfully rather than as an error.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadResult {
    message: String,
    status: String,
}

#[derive(Deserialize)]
struct PlaylistMetadata {
    title: Option<String>,
    entries: Option<Vec<PlaylistEntry>>,
    #[serde(rename = "_type")]
    item_type: Option<String>,
}

#[derive(Deserialize)]
struct PlaylistEntry {
    id: Option<String>,
    url: Option<String>,
    webpage_url: Option<String>,
    original_url: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadProgress {
    job_id: String,
    request_id: String,
    completed: usize,
    total: usize,
    active: usize,
    active_item: Option<usize>,
    percent: Option<String>,
    kind: String,
}

/// Tracks one logical job and every child process it starts. A single
/// application-level job can have several yt-dlp workers, but a second job
/// is always rejected by the native layer.
#[derive(Clone, Default)]
struct DownloadManager {
    active: Arc<(Mutex<Option<Arc<ActiveDownload>>>, Condvar)>,
}

struct ActiveDownload {
    children: Arc<Mutex<Vec<Arc<Mutex<Child>>>>>,
    stop_requested: Arc<AtomicBool>,
    pause_requested: Arc<AtomicBool>,
}

#[derive(Serialize, Deserialize)]
struct DownloadCheckpoint {
    schema_version: u8,
    job_id: String,
    completed: BTreeSet<String>,
}

#[derive(Clone)]
struct DownloadItem {
    checkpoint_key: String,
    locator: String,
    source_index: usize,
}

enum WorkerEvent {
    Started(DownloadItem),
    Progress {
        checkpoint_key: String,
        percent: String,
    },
    Finished {
        item: DownloadItem,
        result: WorkerResult,
    },
}

enum WorkerResult {
    Completed,
    Interrupted,
    Failed(String),
}

/// Runs one item from the coordinator queue. Keeping this boundary private lets
/// the coordinator be exercised with controlled workers without changing the
/// command or runtime-tool trust boundary.
trait DownloadWorker: Send + Sync {
    fn run(
        &self,
        active_job: &Arc<ActiveDownload>,
        tools: &RuntimeTools,
        download_path: &Path,
        output_layout: &OutputLayout,
        item: &DownloadItem,
        sender: &mpsc::Sender<WorkerEvent>,
    ) -> WorkerResult;
}

struct YtDlpDownloadWorker;

impl DownloadWorker for YtDlpDownloadWorker {
    fn run(
        &self,
        active_job: &Arc<ActiveDownload>,
        tools: &RuntimeTools,
        download_path: &Path,
        output_layout: &OutputLayout,
        item: &DownloadItem,
        sender: &mpsc::Sender<WorkerEvent>,
    ) -> WorkerResult {
        run_download_worker(
            active_job,
            tools,
            download_path,
            output_layout,
            item,
            sender,
        )
    }
}

struct AggregateProgress {
    completed: usize,
    total: usize,
    active: BTreeMap<String, (usize, Option<String>)>,
}

#[derive(Clone, Default)]
struct RuntimeSetupManager {
    install_lock: Arc<Mutex<()>>,
}

#[command]
async fn get_runtime_setup_status(app: AppHandle) -> Result<RuntimeSetupStatus, String> {
    tauri::async_runtime::spawn_blocking(move || runtime_setup_status(&app))
        .await
        .map_err(|error| format!("Could not check the app runtime: {error}"))?
}

#[command]
async fn setup_runtime_dependencies(
    app: AppHandle,
    state: State<'_, RuntimeSetupManager>,
) -> Result<RuntimeSetupStatus, String> {
    let setup_manager = state.inner().clone();

    tauri::async_runtime::spawn_blocking(move || {
        let _install_guard = setup_manager
            .install_lock
            .lock()
            .map_err(|_| "Runtime setup could not acquire its installation lock.".to_string())?;
        install_runtime_dependencies(&app)?;

        let status = runtime_setup_status(&app)?;
        if status.ready {
            Ok(status)
        } else {
            Err("Runtime setup finished, but the installed tools could not be verified. Retry setup."
                .to_string())
        }
    })
    .await
    .map_err(|error| format!("Runtime setup ended unexpectedly: {error}"))?
}

fn runtime_setup_status(app: &AppHandle) -> Result<RuntimeSetupStatus, String> {
    let assets = runtime_assets()?;
    let directory = runtime_tools_directory(app)?;

    if !runtime_manifest_is_current(&directory) {
        return Ok(runtime_needs_setup_status());
    }

    let tools = runtime_tools_in_directory(directory, &assets);
    if !runtime_tools_are_regular_files(&tools) {
        return Ok(runtime_needs_setup_status());
    }

    match verify_runtime_tools(&tools) {
        Ok(yt_dlp_version) => Ok(RuntimeSetupStatus {
            ready: true,
            message: "Private yt-dlp, ffmpeg, and ffprobe tools are ready.".to_string(),
            yt_dlp_version: Some(yt_dlp_version),
        }),
        Err(_) => Ok(runtime_needs_setup_status()),
    }
}

fn runtime_needs_setup_status() -> RuntimeSetupStatus {
    RuntimeSetupStatus {
        ready: false,
        message: "The app needs to download its private yt-dlp, ffmpeg, and ffprobe tools."
            .to_string(),
        yt_dlp_version: None,
    }
}

fn runtime_assets() -> Result<Vec<RuntimeAsset>, String> {
    runtime_assets_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn runtime_assets_for(os: &str, arch: &str) -> Result<Vec<RuntimeAsset>, String> {
    let yt_dlp = |file_name, url, sha256| RuntimeAsset {
        component: "yt-dlp",
        file_name,
        url,
        sha256,
    };
    let ffmpeg = |file_name, url, sha256| RuntimeAsset {
        component: "ffmpeg",
        file_name,
        url,
        sha256,
    };
    let ffprobe = |file_name, url, sha256| RuntimeAsset {
        component: "ffprobe",
        file_name,
        url,
        sha256,
    };

    match (os, arch) {
        ("windows", "x86_64") => Ok(vec![
            yt_dlp(
                "yt-dlp.exe",
                "https://github.com/yt-dlp/yt-dlp/releases/download/2026.07.04/yt-dlp.exe",
                "52fe3c26dcf71fbdc85b528589020bb0b8e383155cfa81b64dd447bbe35e24b8",
            ),
            ffmpeg(
                "ffmpeg.exe",
                "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-win32-x64",
                "04e1307997530f9cf2fe35cba2ca7e8875ca91da02f89d6c7243df819c94ad00",
            ),
            ffprobe(
                "ffprobe.exe",
                "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffprobe-win32-x64",
                "3a7e2dc003dc2cd1472827e4c7c4f056ae1ae0ae7c5bbc580c99b49827351ba4",
            ),
        ]),
        ("linux", "x86_64") => Ok(vec![
            yt_dlp(
                "yt-dlp",
                "https://github.com/yt-dlp/yt-dlp/releases/download/2026.07.04/yt-dlp_linux",
                "6bbb3d314cde4febe36e5fa1d55462e29c974f63444e707871834f6d8cc210ae",
            ),
            ffmpeg(
                "ffmpeg",
                "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-linux-x64",
                "e7e7fb30477f717e6f55f9180a70386c62677ef8a4d4d1a5d948f4098aa3eb99",
            ),
            ffprobe(
                "ffprobe",
                "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffprobe-linux-x64",
                "4f231a1960d83e403d08f7971e271707bec278a9ae18e21b8b5b03186668450d",
            ),
        ]),
        ("linux", "aarch64") => Ok(vec![
            yt_dlp(
                "yt-dlp",
                "https://github.com/yt-dlp/yt-dlp/releases/download/2026.07.04/yt-dlp_linux_aarch64",
                "b6ce97646773070d7a7ffd6bbbdcaecb47c48483909c54c915bf08a7a9b5e0b1",
            ),
            ffmpeg(
                "ffmpeg",
                "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-linux-arm64",
                "6bb182d0d75d23028db82e9e4f723ca69b853d055698486e6984ddb2c06fb8ce",
            ),
            ffprobe(
                "ffprobe",
                "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffprobe-linux-arm64",
                "d17ae9b4c297d48e2521ba14e417bb0537c6ff77c584cdbcd6bb0d8d0307a2e8",
            ),
        ]),
        ("macos", "x86_64") => Ok(vec![
            yt_dlp(
                "yt-dlp",
                "https://github.com/yt-dlp/yt-dlp/releases/download/2026.07.04/yt-dlp_macos",
                "498bd0dae17855c599d371d68ec5bafc439a9d8640e838be25c765a9792f261b",
            ),
            ffmpeg(
                "ffmpeg",
                "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-darwin-x64",
                "ebdddc936f61e14049a2d4b549a412b8a40deeff6540e58a9f2a2da9e6b18894",
            ),
            ffprobe(
                "ffprobe",
                "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffprobe-darwin-x64",
                "fa3add0ce901f7241abe0dfc0155d958fc834aca3f8ce61f87cc712ae669c1e0",
            ),
        ]),
        ("macos", "aarch64") => Ok(vec![
            yt_dlp(
                "yt-dlp",
                "https://github.com/yt-dlp/yt-dlp/releases/download/2026.07.04/yt-dlp_macos",
                "498bd0dae17855c599d371d68ec5bafc439a9d8640e838be25c765a9792f261b",
            ),
            ffmpeg(
                "ffmpeg",
                "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-darwin-arm64",
                "a90e3db6a3fd35f6074b013f948b1aa45b31c6375489d39e572bea3f18336584",
            ),
            ffprobe(
                "ffprobe",
                "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffprobe-darwin-arm64",
                "bb2db6f5d8cef919da12fbf592119a987202a8c060a886f3cab091f9cab90b64",
            ),
        ]),
        _ => Err(format!(
            "Runtime setup is not supported on {os} ({arch}). Supported platforms are Windows x64, Linux x64 or ARM64, and macOS Intel or Apple silicon."
        )),
    }
}

fn runtime_tools_directory(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|directory| directory.join(RUNTIME_DIRECTORY))
        .map_err(|error| {
            format!("Could not determine the app data directory for runtime setup: {error}")
        })
}

fn runtime_tools_in_directory(directory: PathBuf, assets: &[RuntimeAsset]) -> RuntimeTools {
    RuntimeTools {
        directory: directory.clone(),
        yt_dlp: directory.join(assets[0].file_name),
        ffmpeg: directory.join(assets[1].file_name),
        ffprobe: directory.join(assets[2].file_name),
    }
}

fn resolve_runtime_tools(app: &AppHandle) -> Result<RuntimeTools, String> {
    let assets = runtime_assets()?;
    let directory = runtime_tools_directory(app)?;
    if !runtime_manifest_is_current(&directory) {
        return Err(
            "The app's private runtime tools are unavailable. Restart the app and complete setup."
                .to_string(),
        );
    }

    let tools = runtime_tools_in_directory(directory, &assets);
    if !runtime_tools_are_regular_files(&tools) {
        return Err(
            "The app's private runtime tools are unavailable. Restart the app and complete setup."
                .to_string(),
        );
    }

    Ok(tools)
}

fn runtime_tools_are_regular_files(tools: &RuntimeTools) -> bool {
    [&tools.yt_dlp, &tools.ffmpeg, &tools.ffprobe]
        .iter()
        .all(|path| {
            fs::symlink_metadata(path)
                .map(|metadata| {
                    metadata.file_type().is_file() && !metadata.file_type().is_symlink()
                })
                .unwrap_or(false)
        })
}

fn runtime_manifest_is_current(directory: &Path) -> bool {
    let manifest_path = directory.join(RUNTIME_MANIFEST_FILE);
    let Ok(metadata) = fs::symlink_metadata(&manifest_path) else {
        return false;
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return false;
    }

    let Some(manifest) = fs::read(&manifest_path)
        .ok()
        .and_then(|contents| serde_json::from_slice::<RuntimeManifest>(&contents).ok())
    else {
        return false;
    };

    manifest.schema_version == RUNTIME_SCHEMA_VERSION
        && manifest.platform == current_platform()
        && manifest.yt_dlp_version == YT_DLP_VERSION
        && manifest.ffmpeg_release == FFMPEG_RELEASE
}

fn current_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn verify_runtime_tools(tools: &RuntimeTools) -> Result<String, String> {
    let yt_dlp_version = command_version(&tools.yt_dlp, "--version", "yt-dlp")?;
    if yt_dlp_version != YT_DLP_VERSION {
        return Err(
            "The installed yt-dlp version does not match the expected runtime version.".to_string(),
        );
    }

    command_version(&tools.ffmpeg, "-version", "ffmpeg")?;
    command_version(&tools.ffprobe, "-version", "ffprobe")?;

    Ok(yt_dlp_version)
}

fn command_version(path: &Path, argument: &str, component: &str) -> Result<String, String> {
    let output = Command::new(path)
        .arg(argument)
        .output()
        .map_err(|_| format!("Could not run the private {component} tool."))?;
    if !output.status.success() {
        return Err(format!(
            "The private {component} tool did not start successfully."
        ));
    }

    let version = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if version.is_empty() {
        return Err(format!(
            "The private {component} tool did not return version information."
        ));
    }

    Ok(version)
}

fn install_runtime_dependencies(app: &AppHandle) -> Result<(), String> {
    let assets = runtime_assets()?;
    let tools_directory = runtime_tools_directory(app)?;
    let app_data_directory = tools_directory
        .parent()
        .ok_or_else(|| "Could not prepare the app data directory for runtime setup.".to_string())?;
    ensure_private_runtime_parent(app_data_directory)?;

    let staging_directory = create_staging_directory(app_data_directory)?;
    let setup_result = (|| {
        let client = Client::builder()
            .https_only(true)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(15 * 60))
            .redirect(Policy::custom(|attempt| {
                if attempt.previous().len() < 5 && attempt.url().scheme() == "https" {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .user_agent("YTDownloader runtime setup")
            .build()
            .map_err(|error| format!("Could not prepare a secure runtime download: {error}"))?;

        for (index, asset) in assets.iter().enumerate() {
            app.emit(
                "runtime-setup-progress",
                RuntimeSetupProgress {
                    current: index + 1,
                    total: assets.len(),
                    component: asset.component.to_string(),
                    message: format!("Downloading {}…", asset.component),
                },
            )
            .map_err(|error| format!("Could not report runtime setup progress: {error}"))?;

            let destination = staging_directory.join(asset.file_name);
            download_runtime_asset(&client, asset, &destination)?;
            set_runtime_executable_permissions(&destination)?;
        }

        write_runtime_manifest(&staging_directory)?;
        let staged_tools = runtime_tools_in_directory(staging_directory.clone(), &assets);
        verify_runtime_tools(&staged_tools)?;
        replace_runtime_installation(&staging_directory, &tools_directory)
    })();

    if setup_result.is_err() && staging_directory.exists() {
        let _ = fs::remove_dir_all(&staging_directory);
    }

    setup_result
}

fn ensure_private_runtime_parent(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| {
        format!("Could not create the app data directory for runtime setup: {error}")
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!("Could not access the app data directory for runtime setup: {error}")
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err("The app data location for runtime setup is not a directory.".to_string());
    }
    set_private_directory_permissions(path)
}

fn create_staging_directory(parent: &Path) -> Result<PathBuf, String> {
    for attempt in 0..10 {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let path = parent.join(format!(
            ".{RUNTIME_DIRECTORY}-staging-{}-{timestamp}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                set_private_directory_permissions(&path)?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Could not create a temporary directory for runtime setup: {error}"
                ));
            }
        }
    }

    Err("Could not create a unique temporary directory for runtime setup.".to_string())
}

fn download_runtime_asset(
    client: &Client,
    asset: &RuntimeAsset,
    destination: &Path,
) -> Result<(), String> {
    let download_result = (|| {
        let mut response = client
            .get(asset.url)
            .send()
            .map_err(|error| format!("Could not download {}: {error}", asset.component))?
            .error_for_status()
            .map_err(|error| format!("Could not download {} securely: {error}", asset.component))?;

        if response
            .content_length()
            .is_some_and(|length| length > MAX_RUNTIME_ASSET_BYTES)
        {
            return Err(format!(
                "The {} download is unexpectedly large and was rejected.",
                asset.component
            ));
        }

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .map_err(|error| format!("Could not save {} for setup: {error}", asset.component))?;
        let mut hasher = Sha256::new();
        let mut total_bytes = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];

        loop {
            let bytes_read = response
                .read(&mut buffer)
                .map_err(|error| format!("Could not download {}: {error}", asset.component))?;
            if bytes_read == 0 {
                break;
            }

            total_bytes = total_bytes.saturating_add(bytes_read as u64);
            if total_bytes > MAX_RUNTIME_ASSET_BYTES {
                return Err(format!(
                    "The {} download is unexpectedly large and was rejected.",
                    asset.component
                ));
            }

            file.write_all(&buffer[..bytes_read]).map_err(|error| {
                format!("Could not save {} for setup: {error}", asset.component)
            })?;
            hasher.update(&buffer[..bytes_read]);
        }

        file.sync_all().map_err(|error| {
            format!(
                "Could not finish saving {} for setup: {error}",
                asset.component
            )
        })?;

        let actual_sha256 = format!("{:x}", hasher.finalize());
        if !actual_sha256.eq_ignore_ascii_case(asset.sha256) {
            return Err(format!(
                "The {} download failed its security check. Retry setup.",
                asset.component
            ));
        }

        Ok(())
    })();

    if download_result.is_err() {
        let _ = fs::remove_file(destination);
    }
    download_result
}

fn write_runtime_manifest(directory: &Path) -> Result<(), String> {
    let manifest = RuntimeManifest {
        schema_version: RUNTIME_SCHEMA_VERSION,
        platform: current_platform(),
        yt_dlp_version: YT_DLP_VERSION.to_string(),
        ffmpeg_release: FFMPEG_RELEASE.to_string(),
    };
    let contents = serde_json::to_vec(&manifest)
        .map_err(|error| format!("Could not prepare runtime setup metadata: {error}"))?;
    let path = directory.join(RUNTIME_MANIFEST_FILE);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("Could not save runtime setup metadata: {error}"))?;
    file.write_all(&contents)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("Could not save runtime setup metadata: {error}"))
}

fn replace_runtime_installation(
    staging_directory: &Path,
    tools_directory: &Path,
) -> Result<(), String> {
    let backup_directory = tools_directory.with_file_name(format!(
        ".{RUNTIME_DIRECTORY}-backup-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));

    let existing_tools = match fs::symlink_metadata(tools_directory) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                return Err("The existing private runtime location is not a directory.".to_string());
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "Could not inspect the existing private runtime location: {error}"
            ));
        }
    };

    if existing_tools {
        fs::rename(tools_directory, &backup_directory)
            .map_err(|error| format!("Could not update the private runtime tools: {error}"))?;
    }

    if let Err(error) = fs::rename(staging_directory, tools_directory) {
        if existing_tools {
            let _ = fs::rename(&backup_directory, tools_directory);
        }
        return Err(format!(
            "Could not install the private runtime tools: {error}"
        ));
    }

    if existing_tools {
        // The new, verified runtime is already active, so a leftover backup must not block the app.
        let _ = fs::remove_dir_all(&backup_directory);
    }

    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("Could not secure the app runtime directory: {error}"))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn set_runtime_executable_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("Could not mark a private runtime tool as executable: {error}"))
}

#[cfg(not(unix))]
fn set_runtime_executable_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[command]
fn save_download_path(path: String) -> Result<String, String> {
    let download_path = validate_download_directory(&path)?;
    fs::write(settings_path()?, path_to_string(&download_path)?)
        .map_err(|error| format!("Could not save the download destination: {error}"))?;

    path_to_string(&download_path)
}

#[command]
fn get_download_path() -> Result<String, String> {
    let path = settings_path()?;

    match fs::read_to_string(path) {
        Ok(saved_path) => {
            let saved_path = saved_path.trim();
            if saved_path.is_empty() {
                return Err(
                    "The saved download destination is empty. Choose an existing folder."
                        .to_string(),
                );
            }

            path_to_string(&validate_download_directory(saved_path)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let default_path = default_download_directory()?;
            fs::create_dir_all(&default_path).map_err(|error| {
                format!("Could not create the default download destination: {error}")
            })?;
            path_to_string(&validate_download_directory(
                default_path.to_string_lossy().as_ref(),
            )?)
        }
        Err(error) => Err(format!(
            "Could not read the saved download destination: {error}"
        )),
    }
}

#[command]
async fn download_audio(
    app: AppHandle,
    state: State<'_, DownloadManager>,
    url: String,
    download_type: DownloadType,
    path: String,
    request_id: String,
) -> Result<DownloadResult, String> {
    let manager = state.inner().clone();
    let active_job = manager.claim_job()?;
    let task_job = active_job.clone();
    let task_app = app.clone();

    let result = tauri::async_runtime::spawn_blocking(move || {
        let request_id = validate_request_id(request_id)?;
        let url = validate_youtube_url(&url)?;
        let download_path = validate_download_directory(&path)?;
        let tools = resolve_runtime_tools(&task_app)?;

        match download_type {
            DownloadType::Single => {
                let (job_id, checkpoint_path) =
                    checkpoint_path_for("single", &url, &download_path, SINGLE_LAYOUT_VERSION)?;
                if task_job.is_interrupted() {
                    return finish_interruption(&task_job, &checkpoint_path);
                }

                run_coordinated_download(
                    &task_job,
                    &task_app,
                    "single",
                    job_id,
                    &request_id,
                    checkpoint_path,
                    &tools,
                    &download_path,
                    vec![DownloadItem {
                        checkpoint_key: "source".to_string(),
                        locator: url.to_string(),
                        source_index: 1,
                    }],
                    OutputLayout::Single,
                    "Finished downloading the video as an MP3.".to_string(),
                )
            }
            DownloadType::Playlist => {
                let (job_id, checkpoint_path) =
                    checkpoint_path_for("playlist", &url, &download_path, PLAYLIST_LAYOUT_VERSION)?;
                let metadata = match read_playlist_metadata(&tools, &url, &task_job) {
                    Ok(metadata) => metadata,
                    Err(_) if task_job.is_interrupted() => {
                        return finish_interruption(&task_job, &checkpoint_path);
                    }
                    Err(error) => return Err(error),
                };
                if task_job.is_interrupted() {
                    return finish_interruption(&task_job, &checkpoint_path);
                }
                let items = playlist_items(&metadata, true)?;
                let width = index_width(&items, 2);

                run_coordinated_download(
                    &task_job,
                    &task_app,
                    "playlist",
                    job_id,
                    &request_id,
                    checkpoint_path,
                    &tools,
                    &download_path,
                    items,
                    OutputLayout::Indexed {
                        width,
                        include_uploader: true,
                    },
                    "Finished downloading playlist audio as MP3 files.".to_string(),
                )
            }
        }
    })
    .await;

    manager.release_job(&active_job);
    result.map_err(|error| format!("The download task ended unexpectedly: {error}"))?
}

#[command]
async fn download_podcast(
    app: AppHandle,
    state: State<'_, DownloadManager>,
    url: String,
    path: String,
    request_id: String,
) -> Result<DownloadResult, String> {
    let manager = state.inner().clone();
    let active_job = manager.claim_job()?;
    let task_job = active_job.clone();
    let task_app = app.clone();

    let result = tauri::async_runtime::spawn_blocking(move || {
        let request_id = validate_request_id(request_id)?;
        let url = validate_podcast_feed_url(&url)?;
        let download_path = validate_download_directory(&path)?;
        let tools = resolve_runtime_tools(&task_app)?;
        let (job_id, checkpoint_path) =
            checkpoint_path_for("podcast", &url, &download_path, PODCAST_LAYOUT_VERSION)?;
        let metadata = match read_playlist_metadata(&tools, &url, &task_job) {
            Ok(metadata) => metadata,
            Err(_) if task_job.is_interrupted() => {
                return finish_interruption(&task_job, &checkpoint_path);
            }
            Err(error) => return Err(error),
        };
        if task_job.is_interrupted() {
            return finish_interruption(&task_job, &checkpoint_path);
        }

        let folder_name =
            sanitize_podcast_folder_title(metadata.title.as_deref().unwrap_or_default());
        let podcast_path = create_podcast_directory(&download_path, &folder_name)?;
        let items = playlist_items(&metadata, false)?;
        let width = index_width(&items, 4);
        let success_message = format!(
            "Finished downloading all available podcast episodes to the “{folder_name}” folder."
        );

        run_coordinated_download(
            &task_job,
            &task_app,
            "podcast",
            job_id,
            &request_id,
            checkpoint_path,
            &tools,
            &podcast_path,
            items,
            OutputLayout::Indexed {
                width,
                include_uploader: false,
            },
            success_message,
        )
    })
    .await;

    manager.release_job(&active_job);
    result.map_err(|error| format!("The download task ended unexpectedly: {error}"))?
}

#[command]
async fn stop_download(state: State<'_, DownloadManager>) -> Result<DownloadResult, String> {
    wait_for_interruption(state.inner().clone(), true).await
}

#[command]
async fn pause_download(state: State<'_, DownloadManager>) -> Result<DownloadResult, String> {
    wait_for_interruption(state.inner().clone(), false).await
}

fn yt_dlp_command(tools: &RuntimeTools) -> Command {
    let mut command = Command::new(&tools.yt_dlp);
    command.arg("--ffmpeg-location").arg(&tools.directory);
    command
}

impl DownloadManager {
    fn claim_job(&self) -> Result<Arc<ActiveDownload>, String> {
        let (active, _) = &*self.active;
        let mut guard = active.lock().map_err(|_| {
            "The download manager is unavailable. Restart the app and try again.".to_string()
        })?;
        if guard.is_some() {
            return Err(
                "Another download is already running. Pause or stop it before starting a new download."
                    .to_string(),
            );
        }

        let job = Arc::new(ActiveDownload {
            children: Arc::new(Mutex::new(Vec::new())),
            stop_requested: Arc::new(AtomicBool::new(false)),
            pause_requested: Arc::new(AtomicBool::new(false)),
        });
        *guard = Some(job.clone());
        Ok(job)
    }

    fn release_job(&self, job: &Arc<ActiveDownload>) {
        let (active, finished) = &*self.active;
        if let Ok(mut guard) = active.lock() {
            if guard
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, job))
            {
                *guard = None;
                finished.notify_all();
            }
        }
    }

    fn request_interruption(&self, stop: bool) -> Result<Arc<ActiveDownload>, String> {
        let (active, _) = &*self.active;
        let job = active
            .lock()
            .map_err(|_| {
                "The download manager is unavailable. Restart the app and try again.".to_string()
            })?
            .clone()
            .ok_or_else(|| "No download is currently running.".to_string())?;

        if stop {
            job.stop_requested.store(true, Ordering::SeqCst);
        } else if !job.stop_requested.load(Ordering::SeqCst) {
            job.pause_requested.store(true, Ordering::SeqCst);
        }
        // A worker can have just exited between being listed and signalled.
        // The coordinator still waits for every worker before resolving pause
        // or stop, even if a platform reports a transient signalling error.
        let _ = job.kill_children();
        Ok(job)
    }

    fn wait_for_job(&self, job: &Arc<ActiveDownload>) -> Result<(), String> {
        let (active, finished) = &*self.active;
        let mut guard = active.lock().map_err(|_| {
            "The download manager is unavailable. Restart the app and try again.".to_string()
        })?;
        while guard
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, job))
        {
            guard = finished.wait(guard).map_err(|_| {
                "The download manager is unavailable. Restart the app and try again.".to_string()
            })?;
        }
        Ok(())
    }
}

impl ActiveDownload {
    fn is_interrupted(&self) -> bool {
        self.stop_requested.load(Ordering::SeqCst) || self.pause_requested.load(Ordering::SeqCst)
    }

    fn add_child(&self, child: Arc<Mutex<Child>>) {
        if let Ok(mut children) = self.children.lock() {
            children.push(child.clone());
        }
        if self.is_interrupted() {
            let _ = child
                .lock()
                .ok()
                .and_then(|mut process| process.kill().ok());
        }
    }

    fn remove_child(&self, child: &Arc<Mutex<Child>>) {
        if let Ok(mut children) = self.children.lock() {
            children.retain(|current| !Arc::ptr_eq(current, child));
        }
    }

    fn kill_children(&self) -> Result<(), String> {
        let children = self
            .children
            .lock()
            .map_err(|_| "Could not signal the active download workers.".to_string())?
            .clone();
        let mut errors = Vec::new();
        for child in children {
            match child
                .lock()
                .map_err(|_| "Could not signal an active download worker.".to_string())?
                .kill()
            {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
                Err(_) => errors.push(()),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err("Could not signal every active download worker. The download is still waiting for them to exit.".to_string())
        }
    }
}

async fn wait_for_interruption(
    manager: DownloadManager,
    stop: bool,
) -> Result<DownloadResult, String> {
    let job = manager.request_interruption(stop)?;
    tauri::async_runtime::spawn_blocking(move || {
        manager.wait_for_job(&job)?;
        let stopped = job.stop_requested.load(Ordering::SeqCst);
        Ok(DownloadResult {
            message: if stopped {
                "Download stopped. Progress for this download was cleared.".to_string()
            } else {
                "Download paused. Resume anytime to continue from where it left off.".to_string()
            },
            status: if stopped { "stopped" } else { "paused" }.to_string(),
        })
    })
    .await
    .map_err(|error| format!("The interruption task ended unexpectedly: {error}"))?
}

#[derive(Clone)]
enum OutputLayout {
    Single,
    Indexed {
        width: usize,
        include_uploader: bool,
    },
}

fn run_coordinated_download(
    active_job: &Arc<ActiveDownload>,
    app: &AppHandle,
    kind: &'static str,
    job_id: String,
    request_id: &str,
    checkpoint_path: PathBuf,
    tools: &RuntimeTools,
    download_path: &Path,
    items: Vec<DownloadItem>,
    output_layout: OutputLayout,
    success_message: String,
) -> Result<DownloadResult, String> {
    run_coordinated_download_with_worker(
        active_job,
        &job_id,
        &checkpoint_path,
        tools,
        download_path,
        items,
        output_layout,
        &success_message,
        Arc::new(YtDlpDownloadWorker),
        |aggregate| aggregate.emit(app, &job_id, request_id, kind),
    )
}

fn run_coordinated_download_with_worker<W, F>(
    active_job: &Arc<ActiveDownload>,
    job_id: &str,
    checkpoint_path: &Path,
    tools: &RuntimeTools,
    download_path: &Path,
    items: Vec<DownloadItem>,
    output_layout: OutputLayout,
    success_message: &str,
    worker: Arc<W>,
    emit_progress: F,
) -> Result<DownloadResult, String>
where
    W: DownloadWorker + 'static,
    F: Fn(&AggregateProgress),
{
    let mut checkpoint = read_checkpoint(checkpoint_path, job_id)?;
    let pending = pending_items(&items, &checkpoint.completed);
    let completed = completed_item_count(&items, &checkpoint.completed);
    let mut aggregate = AggregateProgress::new(completed, items.len());
    emit_progress(&aggregate);

    if active_job.is_interrupted() {
        return finish_interruption(active_job, checkpoint_path);
    }

    let queue = Arc::new(Mutex::new(VecDeque::from(pending)));
    let (sender, receiver) = mpsc::channel();
    let worker_count = DOWNLOAD_WORKER_LIMIT.min(
        queue
            .lock()
            .map_err(|_| "Could not prepare the download queue.".to_string())?
            .len(),
    );
    let mut workers = Vec::with_capacity(worker_count);

    for _ in 0..worker_count {
        let worker_queue = queue.clone();
        let worker_sender = sender.clone();
        let worker_job = active_job.clone();
        let worker_tools = tools.clone();
        let worker_path = download_path.to_path_buf();
        let worker_layout = output_layout.clone();
        let worker_runner = worker.clone();
        workers.push(thread::spawn(move || loop {
            if worker_job.is_interrupted() {
                break;
            }
            let item = match worker_queue.lock() {
                Ok(mut queue) => queue.pop_front(),
                Err(_) => None,
            };
            let Some(item) = item else {
                break;
            };

            if worker_sender
                .send(WorkerEvent::Started(item.clone()))
                .is_err()
            {
                break;
            }
            let result = worker_runner.run(
                &worker_job,
                &worker_tools,
                &worker_path,
                &worker_layout,
                &item,
                &worker_sender,
            );
            if worker_sender
                .send(WorkerEvent::Finished { item, result })
                .is_err()
            {
                break;
            }
        }));
    }
    drop(sender);

    let mut failures = Vec::new();
    while let Ok(event) = receiver.recv() {
        match event {
            WorkerEvent::Started(item) => {
                aggregate.start(&item);
                emit_progress(&aggregate);
            }
            WorkerEvent::Progress {
                checkpoint_key,
                percent,
            } => {
                aggregate.update(&checkpoint_key, percent);
                emit_progress(&aggregate);
            }
            WorkerEvent::Finished { item, result } => {
                match result {
                    WorkerResult::Completed => {
                        if checkpoint.completed.insert(item.checkpoint_key.clone()) {
                            if let Err(error) = write_checkpoint(checkpoint_path, &checkpoint) {
                                checkpoint.completed.remove(&item.checkpoint_key);
                                failures.push(format!(
                                    "item {} could not save its resume checkpoint ({error})",
                                    item.source_index
                                ));
                            }
                        }
                    }
                    WorkerResult::Interrupted => {}
                    WorkerResult::Failed(error) => {
                        failures.push(format!("item {}: {error}", item.source_index));
                    }
                }
                let completed = completed_item_count(&items, &checkpoint.completed);
                aggregate.finish(&item.checkpoint_key, completed);
                emit_progress(&aggregate);
            }
        }
    }

    for worker in workers {
        if worker.join().is_err() {
            failures.push("a download worker ended unexpectedly".to_string());
        }
    }

    if active_job.is_interrupted() {
        return finish_interruption(active_job, checkpoint_path);
    }

    if !failures.is_empty() {
        let details = failures
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "{} item(s) could not be downloaded after the other workers finished. {} of {} item(s) were checkpointed and will be skipped if you choose Download again. Check that the source is public and the destination is writable. Details: {}",
            failures.len(),
            completed_item_count(&items, &checkpoint.completed),
            items.len(),
            details
        ));
    }

    clear_checkpoint(checkpoint_path)?;
    Ok(DownloadResult {
        message: success_message.to_string(),
        status: "completed".to_string(),
    })
}

fn run_download_worker(
    active_job: &Arc<ActiveDownload>,
    tools: &RuntimeTools,
    download_path: &Path,
    output_layout: &OutputLayout,
    item: &DownloadItem,
    sender: &mpsc::Sender<WorkerEvent>,
) -> WorkerResult {
    if active_job.is_interrupted() {
        return WorkerResult::Interrupted;
    }

    let mut command = yt_dlp_command(tools);
    command
        .arg("--extract-audio")
        .arg("--audio-format")
        .arg("mp3")
        .arg("--audio-quality")
        .arg("320K")
        .arg("--newline")
        .arg("--no-playlist")
        .arg("--paths")
        .arg(download_path)
        .arg("--output")
        .arg(output_template(output_layout, item))
        .arg("--progress-template")
        .arg("worker-progress:%(progress._percent_str)s")
        .arg("--")
        .arg(&item.locator);

    let mut child = match command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return WorkerResult::Failed(match error.kind() {
                std::io::ErrorKind::NotFound => {
                    "the app's private yt-dlp tool is unavailable".to_string()
                }
                _ => "the app could not start its private yt-dlp tool".to_string(),
            });
        }
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return WorkerResult::Failed("could not read yt-dlp progress output".to_string());
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return WorkerResult::Failed("could not read yt-dlp error output".to_string());
    };

    let child = Arc::new(Mutex::new(child));
    active_job.add_child(child.clone());
    let progress_sender = sender.clone();
    let checkpoint_key = item.checkpoint_key.clone();
    let progress_reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(percent) = parse_worker_progress(&line) {
                let _ = progress_sender.send(WorkerEvent::Progress {
                    checkpoint_key: checkpoint_key.clone(),
                    percent,
                });
            }
        }
    });
    let error_reader = thread::spawn(move || {
        let mut output = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut output);
    });

    let status = wait_for_child(&child);
    active_job.remove_child(&child);
    let _ = progress_reader.join();
    let _ = error_reader.join();

    if active_job.is_interrupted() {
        return WorkerResult::Interrupted;
    }
    match status {
        Ok(status) if status.success() => WorkerResult::Completed,
        Ok(status) => WorkerResult::Failed(format!("yt-dlp exited with status {status}")),
        Err(error) => WorkerResult::Failed(error),
    }
}

fn wait_for_child(child: &Arc<Mutex<Child>>) -> Result<std::process::ExitStatus, String> {
    loop {
        let status = child
            .lock()
            .map_err(|_| "Could not wait for a yt-dlp worker.".to_string())?
            .try_wait()
            .map_err(|_| "Could not wait for a yt-dlp worker.".to_string())?;
        if let Some(status) = status {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn output_template(layout: &OutputLayout, item: &DownloadItem) -> String {
    match layout {
        OutputLayout::Single => "%(uploader)s/%(title)s.%(ext)s".to_string(),
        OutputLayout::Indexed {
            width,
            include_uploader,
        } => {
            let prefix = format!("{:0width$}", item.source_index, width = *width);
            if *include_uploader {
                format!("%(uploader)s/{prefix} - %(title)s.%(ext)s")
            } else {
                format!("{prefix} - %(title)s.%(ext)s")
            }
        }
    }
}

fn finish_interruption(
    active_job: &ActiveDownload,
    checkpoint_path: &Path,
) -> Result<DownloadResult, String> {
    if active_job.stop_requested.load(Ordering::SeqCst) {
        clear_checkpoint(checkpoint_path)?;
        Ok(DownloadResult {
            message: "Download stopped. Progress for this download was cleared.".to_string(),
            status: "stopped".to_string(),
        })
    } else {
        Ok(DownloadResult {
            message: "Download paused. Resume anytime to continue from where it left off."
                .to_string(),
            status: "paused".to_string(),
        })
    }
}

fn pending_items(items: &[DownloadItem], completed: &BTreeSet<String>) -> Vec<DownloadItem> {
    items
        .iter()
        .filter(|item| !completed.contains(&item.checkpoint_key))
        .cloned()
        .collect()
}

fn completed_item_count(items: &[DownloadItem], completed: &BTreeSet<String>) -> usize {
    items
        .iter()
        .filter(|item| completed.contains(&item.checkpoint_key))
        .count()
}

impl AggregateProgress {
    fn new(completed: usize, total: usize) -> Self {
        Self {
            completed: completed.min(total),
            total,
            active: BTreeMap::new(),
        }
    }

    fn start(&mut self, item: &DownloadItem) {
        self.active
            .insert(item.checkpoint_key.clone(), (item.source_index, None));
    }

    fn update(&mut self, checkpoint_key: &str, percent: String) {
        if let Some((_, active_percent)) = self.active.get_mut(checkpoint_key) {
            *active_percent = Some(percent);
        }
    }

    fn finish(&mut self, checkpoint_key: &str, completed: usize) {
        self.active.remove(checkpoint_key);
        self.completed = self.completed.max(completed.min(self.total));
    }

    fn emit(&self, app: &AppHandle, job_id: &str, request_id: &str, kind: &str) {
        let active = self.active.values().next();
        let _ = app.emit(
            "download-progress",
            DownloadProgress {
                job_id: job_id.to_string(),
                request_id: request_id.to_string(),
                completed: self.completed,
                total: self.total,
                active: self.active.len(),
                active_item: active.map(|(item, _)| *item),
                percent: active.and_then(|(_, percent)| percent.clone()),
                kind: kind.to_string(),
            },
        );
    }
}

fn checkpoint_path_for(
    kind: &str,
    url: &Url,
    destination: &Path,
    layout_version: &str,
) -> Result<(String, PathBuf), String> {
    let job_id = checkpoint_identity(kind, url, destination, layout_version)?;
    let base = dirs::cache_dir()
        .or_else(dirs::home_dir)
        .ok_or_else(|| "Could not determine a location to track download progress.".to_string())?
        .join("yt-dlp-tauri")
        .join("checkpoints");
    fs::create_dir_all(&base)
        .map_err(|error| format!("Could not prepare download progress tracking: {error}"))?;

    Ok((job_id.clone(), base.join(format!("{kind}-{job_id}.json"))))
}

fn checkpoint_identity(
    kind: &str,
    url: &Url,
    destination: &Path,
    layout_version: &str,
) -> Result<String, String> {
    let destination = path_to_string(destination)?;
    let normalized_url = normalized_source_url(url);
    let mut hasher = Sha256::new();
    for value in [
        "yt-dlp-tauri-checkpoint",
        kind,
        layout_version,
        normalized_url.as_str(),
        destination.as_str(),
    ] {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn normalized_source_url(url: &Url) -> String {
    let mut normalized = url.clone();
    normalized.set_fragment(None);
    let remove_port = matches!(
        (normalized.scheme(), normalized.port()),
        ("http", Some(80)) | ("https", Some(443))
    );
    if remove_port {
        let _ = normalized.set_port(None);
    }
    normalized.to_string()
}

fn read_checkpoint(path: &Path, job_id: &str) -> Result<DownloadCheckpoint, String> {
    match fs::read(path) {
        Ok(contents) => {
            let checkpoint: DownloadCheckpoint =
                serde_json::from_slice(&contents).map_err(|_| {
                    "Saved download progress is invalid. Stop this download and try again."
                        .to_string()
                })?;
            if checkpoint.schema_version != CHECKPOINT_SCHEMA_VERSION || checkpoint.job_id != job_id
            {
                return Err(
                    "Saved download progress is incompatible with this download. Stop this download and try again."
                        .to_string(),
                );
            }
            Ok(checkpoint)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DownloadCheckpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            job_id: job_id.to_string(),
            completed: BTreeSet::new(),
        }),
        Err(error) => Err(format!("Could not read saved download progress: {error}")),
    }
}

fn write_checkpoint(path: &Path, checkpoint: &DownloadCheckpoint) -> Result<(), String> {
    let contents = serde_json::to_vec(checkpoint)
        .map_err(|error| format!("Could not prepare saved download progress: {error}"))?;
    let parent = path
        .parent()
        .ok_or_else(|| "Could not prepare saved download progress.".to_string())?;
    let mut temporary_path = None;
    for attempt in 0..10 {
        let candidate = parent.join(format!(
            ".checkpoint-{}-{}-{attempt}.tmp",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                let write_result = file.write_all(&contents).and_then(|()| file.sync_all());
                drop(file);
                if let Err(error) = write_result {
                    let _ = fs::remove_file(&candidate);
                    return Err(format!("Could not save download progress: {error}"));
                }
                temporary_path = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Could not save download progress: {error}")),
        }
    }
    let temporary_path =
        temporary_path.ok_or_else(|| "Could not create saved download progress.".to_string())?;
    if let Err(error) = fs::rename(&temporary_path, path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(format!("Could not save download progress: {error}"));
    }
    sync_checkpoint_directory(parent)
}

#[cfg(unix)]
fn sync_checkpoint_directory(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Could not finish saving download progress: {error}"))
}

#[cfg(not(unix))]
fn sync_checkpoint_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn clear_checkpoint(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not clear saved download progress: {error}")),
    }
}

fn settings_path() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join(SETTINGS_FILE))
        .ok_or_else(|| "Could not find the home directory for app settings.".to_string())
}

fn default_download_directory() -> Result<PathBuf, String> {
    dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Music")))
        .ok_or_else(|| {
            "Could not determine a default download destination. Choose a folder.".to_string()
        })
}

fn validate_download_directory(value: &str) -> Result<PathBuf, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Choose an existing folder for downloaded audio.".to_string());
    }

    let path = Path::new(value);
    let canonical_path = fs::canonicalize(path)
        .map_err(|error| format!("The download folder cannot be accessed: {error}"))?;

    if !canonical_path.is_dir() {
        return Err("The download destination must be a folder, not a file.".to_string());
    }

    verify_directory_is_writable(&canonical_path)?;

    Ok(canonical_path)
}

fn verify_directory_is_writable(path: &Path) -> Result<(), String> {
    for attempt in 0..10 {
        let probe_path = path.join(format!(
            ".yt-dlp-tauri-write-test-{}-{attempt}",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe_path)
        {
            Ok(file) => {
                drop(file);
                fs::remove_file(&probe_path).map_err(|error| {
                    format!("The download folder is not writable because a test file could not be removed: {error}")
                })?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("The download folder is not writable: {error}"));
            }
        }
    }

    Err(
        "The download folder is not writable because a temporary filename is unavailable."
            .to_string(),
    )
}

fn validate_youtube_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value.trim())
        .map_err(|_| "Enter a valid YouTube video or playlist URL.".to_string())?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err("The YouTube URL must use http or https.".to_string());
    }

    let host = url
        .host_str()
        .ok_or_else(|| "Enter a valid YouTube video or playlist URL.".to_string())?
        .to_ascii_lowercase();
    let is_youtube_host = ["youtube.com", "youtu.be", "youtube-nocookie.com"]
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")));

    if !is_youtube_host {
        return Err("Only YouTube and youtu.be URLs are supported.".to_string());
    }

    Ok(url)
}

fn validate_podcast_feed_url(value: &str) -> Result<Url, String> {
    let url =
        Url::parse(value.trim()).map_err(|_| "Enter a valid podcast RSS feed URL.".to_string())?;

    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("The podcast RSS feed URL must use http or https.".to_string());
    }

    Ok(url)
}

fn validate_request_id(value: String) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("Could not start the download. Please try again.".to_string());
    }
    Ok(value.to_string())
}

/// Enumerates every source item before workers begin. The resulting locators
/// are passed one at a time to `yt-dlp --no-playlist`, which avoids concurrent
/// playlist processing and makes the coordinator the sole checkpoint writer.
fn read_playlist_metadata(
    tools: &RuntimeTools,
    url: &Url,
    active_job: &Arc<ActiveDownload>,
) -> Result<PlaylistMetadata, String> {
    let mut command = yt_dlp_command(tools);
    command
        .arg("--simulate")
        .arg("--flat-playlist")
        .arg("--dump-single-json")
        .arg("--no-warnings")
        .arg("--quiet")
        .arg("--")
        .arg(url.as_str());
    let output = run_tracked_capture(command, active_job)?;

    if !output.status.success() {
        return Err(
            "yt-dlp could not read this source. Confirm that it is public and supported by yt-dlp."
                .to_string(),
        );
    }

    let metadata: PlaylistMetadata = serde_json::from_slice(&output.stdout).map_err(|_| {
        "yt-dlp did not return valid playlist information. Confirm that the selected URL is a playlist or public RSS feed."
            .to_string()
    })?;
    if metadata.item_type.as_deref() != Some("playlist")
        || metadata.entries.as_ref().is_none_or(Vec::is_empty)
    {
        return Err(
            "This source has no downloadable items. Choose a playlist or podcast feed with at least one item."
                .to_string(),
        );
    }

    Ok(metadata)
}

struct CapturedProcess {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
}

fn run_tracked_capture(
    mut command: Command,
    active_job: &Arc<ActiveDownload>,
) -> Result<CapturedProcess, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                "The app's private yt-dlp tool is unavailable. Restart the app and complete setup."
                    .to_string()
            }
            _ => {
                "Could not start the app's private yt-dlp tool to inspect this source.".to_string()
            }
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Could not read yt-dlp source information.".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Could not read yt-dlp source information.".to_string())?;
    let child = Arc::new(Mutex::new(child));
    active_job.add_child(child.clone());
    let stdout_reader = thread::spawn(move || {
        let mut output = Vec::new();
        let _ = BufReader::new(stdout).read_to_end(&mut output);
        output
    });
    let stderr_reader = thread::spawn(move || {
        let mut output = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut output);
    });
    let status = wait_for_child(&child);
    active_job.remove_child(&child);
    let stdout = stdout_reader.join().unwrap_or_default();
    let _ = stderr_reader.join();

    if active_job.is_interrupted() {
        return Err("The download was interrupted.".to_string());
    }
    Ok(CapturedProcess {
        status: status?,
        stdout,
    })
}

fn playlist_items(
    metadata: &PlaylistMetadata,
    is_youtube_playlist: bool,
) -> Result<Vec<DownloadItem>, String> {
    let entries = metadata.entries.as_ref().ok_or_else(|| {
        "This source has no downloadable items. Choose a playlist or podcast feed with at least one item."
            .to_string()
    })?;
    let mut used_keys = BTreeSet::new();
    let mut items = Vec::with_capacity(entries.len());

    for (position, entry) in entries.iter().enumerate() {
        let source_index = position + 1;
        let locator = item_locator(entry, is_youtube_playlist).ok_or_else(|| {
            format!(
                "yt-dlp could not find a safe, downloadable locator for item {source_index}. Try the source again or choose another feed."
            )
        })?;
        let identity = entry
            .id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or(locator.as_str());
        let mut checkpoint_key = format!("item:{identity}");
        if !used_keys.insert(checkpoint_key.clone()) {
            checkpoint_key = format!("{checkpoint_key}#{source_index}");
            used_keys.insert(checkpoint_key.clone());
        }
        items.push(DownloadItem {
            checkpoint_key,
            locator,
            source_index,
        });
    }

    if items.is_empty() {
        return Err(
            "This source has no downloadable items. Choose a playlist or podcast feed with at least one item."
                .to_string(),
        );
    }
    Ok(items)
}

fn item_locator(entry: &PlaylistEntry, is_youtube_playlist: bool) -> Option<String> {
    let candidates = if is_youtube_playlist {
        [&entry.webpage_url, &entry.original_url, &entry.url]
    } else {
        [&entry.url, &entry.webpage_url, &entry.original_url]
    };
    for candidate in candidates.into_iter().flatten() {
        if is_http_url(candidate) {
            return Some(candidate.to_string());
        }
    }

    if is_youtube_playlist {
        let candidate = entry.id.as_deref().or(entry.url.as_deref())?;
        let mut locator = Url::parse("https://www.youtube.com/watch").ok()?;
        locator.query_pairs_mut().append_pair("v", candidate);
        return Some(locator.to_string());
    }
    None
}

fn is_http_url(value: &str) -> bool {
    Url::parse(value)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

fn index_width(items: &[DownloadItem], minimum: usize) -> usize {
    items.len().max(1).to_string().len().max(minimum)
}

fn create_podcast_directory(download_path: &Path, folder_name: &str) -> Result<PathBuf, String> {
    let podcast_path = download_path.join(folder_name);
    fs::create_dir_all(&podcast_path)
        .map_err(|error| format!("Could not create the podcast folder: {error}"))?;

    let canonical_podcast_path = fs::canonicalize(&podcast_path).map_err(|error| {
        format!("Could not access the podcast folder after creating it: {error}")
    })?;
    if !canonical_podcast_path.is_dir() {
        return Err("The podcast download location is not a folder.".to_string());
    }
    if !canonical_podcast_path.starts_with(download_path) {
        return Err(
            "The podcast folder resolves outside the selected download destination. Choose another destination."
                .to_string(),
        );
    }

    verify_directory_is_writable(&canonical_podcast_path)?;
    Ok(canonical_podcast_path)
}

fn sanitize_podcast_folder_title(title: &str) -> String {
    let normalized: String = title
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
                )
            {
                ' '
            } else {
                character
            }
        })
        .collect();
    let mut folder_name = normalized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches('.')
        .trim()
        .chars()
        .take(100)
        .collect::<String>();

    if folder_name.is_empty()
        || [
            ".", "..", "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6",
            "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
            "LPT9",
        ]
        .iter()
        .any(|reserved| folder_name.eq_ignore_ascii_case(reserved))
    {
        folder_name = "Untitled Podcast".to_string();
    }

    folder_name
}

#[cfg(test)]
fn parse_download_progress(line: &str) -> Option<(usize, usize, String)> {
    let progress = line.strip_prefix("download-progress:")?;
    let (item, percent) = progress.split_once(':')?;
    let (current, total) = item.split_once('/')?;
    let current = current.trim().parse().ok()?;
    let total = total.trim().parse().ok()?;

    Some((current, total, percent.trim().chars().take(12).collect()))
}

fn parse_worker_progress(line: &str) -> Option<String> {
    line.strip_prefix("worker-progress:")
        .map(|percent| percent.trim().chars().take(12).collect())
        .filter(|percent: &String| !percent.is_empty())
}

fn path_to_string(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "The selected folder contains unsupported characters.".to_string())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(DownloadManager::default())
        .manage(RuntimeSetupManager::default())
        .invoke_handler(tauri::generate_handler![
            get_runtime_setup_status,
            setup_runtime_dependencies,
            save_download_path,
            get_download_path,
            download_audio,
            download_podcast,
            stop_download,
            pause_download
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}

#[cfg(test)]
mod tests {
    use super::{
        checkpoint_identity, index_width, output_template, parse_download_progress, pending_items,
        playlist_items, read_checkpoint, run_coordinated_download_with_worker, runtime_assets_for,
        sanitize_podcast_folder_title, validate_podcast_feed_url, validate_youtube_url,
        write_checkpoint, ActiveDownload, AggregateProgress, DownloadCheckpoint, DownloadItem,
        DownloadManager, DownloadWorker, OutputLayout, PlaylistEntry, PlaylistMetadata,
        RuntimeTools, WorkerEvent, WorkerResult, CHECKPOINT_SCHEMA_VERSION, DOWNLOAD_WORKER_LIMIT,
    };
    use std::{
        collections::BTreeSet,
        fs,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc, Arc, Condvar, Mutex,
        },
        thread,
        time::Duration,
    };
    use url::Url;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    static TEST_DIRECTORY_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ytdownloader-coordinator-test-{}-{}",
                std::process::id(),
                TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir(&path).expect("create isolated test directory");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn test_items(count: usize) -> Vec<DownloadItem> {
        (1..=count)
            .map(|source_index| DownloadItem {
                checkpoint_key: format!("item:{source_index}"),
                locator: format!("https://example.test/{source_index}"),
                source_index,
            })
            .collect()
    }

    fn test_runtime_tools() -> RuntimeTools {
        RuntimeTools {
            directory: PathBuf::from("test-runtime"),
            yt_dlp: PathBuf::from("test-runtime/yt-dlp"),
            ffmpeg: PathBuf::from("test-runtime/ffmpeg"),
            ffprobe: PathBuf::from("test-runtime/ffprobe"),
        }
    }

    #[derive(Default)]
    struct ConcurrentWorkerSnapshot {
        active: usize,
        max_active: usize,
        started: Vec<usize>,
        release: bool,
    }

    #[derive(Default)]
    struct ConcurrentWorkerState {
        snapshot: Mutex<ConcurrentWorkerSnapshot>,
        changed: Condvar,
    }

    impl ConcurrentWorkerState {
        fn wait_for_started(&self, expected: usize) {
            let snapshot = self.snapshot.lock().expect("lock concurrent worker state");
            let (snapshot, timeout) = self
                .changed
                .wait_timeout_while(snapshot, TEST_TIMEOUT, |snapshot| {
                    snapshot.started.len() < expected
                })
                .expect("wait for concurrent workers");
            assert!(
                !timeout.timed_out(),
                "only {} of {expected} workers started",
                snapshot.started.len()
            );
        }

        fn release(&self) {
            let mut snapshot = self.snapshot.lock().expect("lock concurrent worker state");
            snapshot.release = true;
            self.changed.notify_all();
        }

        fn snapshot(&self) -> (usize, Vec<usize>) {
            let snapshot = self.snapshot.lock().expect("lock concurrent worker state");
            (snapshot.max_active, snapshot.started.clone())
        }
    }

    struct ConcurrentWorker {
        state: Arc<ConcurrentWorkerState>,
    }

    impl DownloadWorker for ConcurrentWorker {
        fn run(
            &self,
            _active_job: &Arc<ActiveDownload>,
            _tools: &RuntimeTools,
            _download_path: &Path,
            _output_layout: &OutputLayout,
            item: &DownloadItem,
            sender: &mpsc::Sender<WorkerEvent>,
        ) -> WorkerResult {
            {
                let mut snapshot = self
                    .state
                    .snapshot
                    .lock()
                    .expect("lock concurrent worker state");
                snapshot.active += 1;
                snapshot.max_active = snapshot.max_active.max(snapshot.active);
                snapshot.started.push(item.source_index);
                self.state.changed.notify_all();
            }
            let _ = sender.send(WorkerEvent::Progress {
                checkpoint_key: item.checkpoint_key.clone(),
                percent: "50.0%".to_string(),
            });

            let mut snapshot = self
                .state
                .snapshot
                .lock()
                .expect("lock concurrent worker state");
            while !snapshot.release {
                snapshot = self
                    .state
                    .changed
                    .wait(snapshot)
                    .expect("wait for concurrent worker release");
            }
            snapshot.active -= 1;
            self.state.changed.notify_all();
            WorkerResult::Completed
        }
    }

    struct ScriptedWorker {
        failed_key: String,
        calls: Mutex<Vec<String>>,
    }

    impl DownloadWorker for ScriptedWorker {
        fn run(
            &self,
            _active_job: &Arc<ActiveDownload>,
            _tools: &RuntimeTools,
            _download_path: &Path,
            _output_layout: &OutputLayout,
            item: &DownloadItem,
            sender: &mpsc::Sender<WorkerEvent>,
        ) -> WorkerResult {
            self.calls
                .lock()
                .expect("record scripted worker call")
                .push(item.checkpoint_key.clone());
            let _ = sender.send(WorkerEvent::Progress {
                checkpoint_key: item.checkpoint_key.clone(),
                percent: "25.0%".to_string(),
            });

            if item.checkpoint_key == self.failed_key {
                WorkerResult::Failed("simulated yt-dlp failure".to_string())
            } else {
                WorkerResult::Completed
            }
        }
    }

    #[derive(Default)]
    struct InterruptionWorkerSnapshot {
        started: usize,
        observed_interruption: usize,
        permit_interruption_check: bool,
        release: bool,
    }

    #[derive(Default)]
    struct InterruptionWorkerState {
        snapshot: Mutex<InterruptionWorkerSnapshot>,
        changed: Condvar,
    }

    impl InterruptionWorkerState {
        fn wait_for_started(&self, expected: usize) {
            let snapshot = self
                .snapshot
                .lock()
                .expect("lock interruption worker state");
            let (snapshot, timeout) = self
                .changed
                .wait_timeout_while(snapshot, TEST_TIMEOUT, |snapshot| {
                    snapshot.started < expected
                })
                .expect("wait for interruption workers");
            assert!(
                !timeout.timed_out(),
                "only {} of {expected} workers started",
                snapshot.started
            );
        }

        fn permit_interruption_check(&self) {
            let mut snapshot = self
                .snapshot
                .lock()
                .expect("lock interruption worker state");
            snapshot.permit_interruption_check = true;
            self.changed.notify_all();
        }

        fn wait_for_observed_interruption(&self, expected: usize) {
            let snapshot = self
                .snapshot
                .lock()
                .expect("lock interruption worker state");
            let (snapshot, timeout) = self
                .changed
                .wait_timeout_while(snapshot, TEST_TIMEOUT, |snapshot| {
                    snapshot.observed_interruption < expected
                })
                .expect("wait for worker interruption observations");
            assert!(
                !timeout.timed_out(),
                "only {} of {expected} workers observed the interruption",
                snapshot.observed_interruption
            );
        }

        fn release(&self) {
            let mut snapshot = self
                .snapshot
                .lock()
                .expect("lock interruption worker state");
            snapshot.release = true;
            self.changed.notify_all();
        }
    }

    struct InterruptibleWorker {
        state: Arc<InterruptionWorkerState>,
    }

    impl DownloadWorker for InterruptibleWorker {
        fn run(
            &self,
            active_job: &Arc<ActiveDownload>,
            _tools: &RuntimeTools,
            _download_path: &Path,
            _output_layout: &OutputLayout,
            _item: &DownloadItem,
            _sender: &mpsc::Sender<WorkerEvent>,
        ) -> WorkerResult {
            let mut snapshot = self
                .state
                .snapshot
                .lock()
                .expect("lock interruption worker state");
            snapshot.started += 1;
            self.state.changed.notify_all();
            while !snapshot.permit_interruption_check {
                snapshot = self
                    .state
                    .changed
                    .wait(snapshot)
                    .expect("wait to check interruption");
            }

            let interrupted = active_job.is_interrupted();
            if interrupted {
                snapshot.observed_interruption += 1;
                self.state.changed.notify_all();
            }
            while !snapshot.release {
                snapshot = self
                    .state
                    .changed
                    .wait(snapshot)
                    .expect("wait for interruption worker release");
            }

            if interrupted {
                WorkerResult::Interrupted
            } else {
                WorkerResult::Failed("worker did not receive the interruption".to_string())
            }
        }
    }

    #[test]
    fn accepts_supported_youtube_hosts() {
        assert!(validate_youtube_url("https://www.youtube.com/watch?v=test").is_ok());
        assert!(validate_youtube_url("https://youtu.be/test").is_ok());
    }

    #[test]
    fn rejects_non_youtube_or_unsafe_urls() {
        assert!(validate_youtube_url("https://example.com/watch?v=test").is_err());
        assert!(validate_youtube_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn accepts_http_podcast_feed_urls_only() {
        assert!(validate_podcast_feed_url("https://example.com/podcast.rss").is_ok());
        assert!(validate_podcast_feed_url("file:///tmp/podcast.rss").is_err());
        assert!(validate_podcast_feed_url("not a URL").is_err());
    }

    #[test]
    fn sanitizes_podcast_folder_titles() {
        assert_eq!(
            sanitize_podcast_folder_title(" ../ Stuff / You: Should * Know? "),
            "Stuff You Should Know"
        );
        assert_eq!(sanitize_podcast_folder_title("CON"), "Untitled Podcast");
        assert_eq!(sanitize_podcast_folder_title("..."), "Untitled Podcast");
    }

    #[test]
    fn parses_yt_dlp_download_progress_lines() {
        let (current, total, percent) =
            parse_download_progress("download-progress:17/2857: 42.5%").unwrap();
        assert_eq!(current, 17);
        assert_eq!(total, 2857);
        assert_eq!(percent, "42.5%");
        assert!(parse_download_progress("unrelated output").is_none());
    }

    #[test]
    fn checkpoint_identity_uses_source_destination_kind_and_layout() {
        let source = Url::parse("https://www.youtube.com/playlist?list=example#ignored").unwrap();
        let same_source_without_fragment =
            Url::parse("https://www.youtube.com/playlist?list=example").unwrap();
        let first = checkpoint_identity(
            "playlist",
            &source,
            Path::new("/downloads/one"),
            "playlist-static-index-v1",
        )
        .unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|character| character.is_ascii_hexdigit()));
        assert_eq!(
            first,
            checkpoint_identity(
                "playlist",
                &same_source_without_fragment,
                Path::new("/downloads/one"),
                "playlist-static-index-v1",
            )
            .unwrap()
        );
        assert_ne!(
            first,
            checkpoint_identity(
                "playlist",
                &same_source_without_fragment,
                Path::new("/downloads/two"),
                "playlist-static-index-v1",
            )
            .unwrap()
        );
        assert_ne!(
            first,
            checkpoint_identity(
                "podcast",
                &same_source_without_fragment,
                Path::new("/downloads/one"),
                "podcast-static-index-v1",
            )
            .unwrap()
        );
    }

    #[test]
    fn playlist_queue_keeps_source_order_and_skips_checkpointed_items() {
        let metadata = PlaylistMetadata {
            title: None,
            item_type: Some("playlist".to_string()),
            entries: Some(vec![
                PlaylistEntry {
                    id: Some("first-item".to_string()),
                    url: Some("first-item".to_string()),
                    webpage_url: None,
                    original_url: None,
                },
                PlaylistEntry {
                    id: Some("second-item".to_string()),
                    url: Some("second-item".to_string()),
                    webpage_url: None,
                    original_url: None,
                },
            ]),
        };
        let items = playlist_items(&metadata, true).unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.source_index)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            items[0].locator,
            "https://www.youtube.com/watch?v=first-item"
        );
        assert_eq!(index_width(&items, 2), 2);
        assert_eq!(
            output_template(
                &OutputLayout::Indexed {
                    width: 2,
                    include_uploader: true,
                },
                &items[1],
            ),
            "%(uploader)s/02 - %(title)s.%(ext)s"
        );

        let completed = BTreeSet::from([items[0].checkpoint_key.clone()]);
        let pending = pending_items(&items, &completed);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].source_index, 2);
    }

    #[test]
    fn aggregate_progress_never_decreases_completed_items() {
        let first = DownloadItem {
            checkpoint_key: "first".to_string(),
            locator: "https://example.com/first".to_string(),
            source_index: 1,
        };
        let second = DownloadItem {
            checkpoint_key: "second".to_string(),
            locator: "https://example.com/second".to_string(),
            source_index: 2,
        };
        let mut progress = AggregateProgress::new(0, 2);
        progress.start(&first);
        progress.start(&second);
        progress.update("first", "42.5%".to_string());
        progress.finish("first", 1);
        progress.finish("second", 0);

        assert_eq!(progress.completed, 1);
        assert_eq!(progress.active.len(), 0);
    }

    #[test]
    fn download_manager_rejects_a_second_logical_job() {
        let manager = DownloadManager::default();
        let first = manager.claim_job().unwrap();
        assert!(manager.claim_job().is_err());
        manager.release_job(&first);
        assert!(manager.claim_job().is_ok());
    }

    #[test]
    fn coordinator_bounds_workers_runs_each_item_once_and_reports_final_progress() {
        let directory = TestDirectory::new();
        let checkpoint_path = directory.path().join("progress.json");
        let download_path = directory.path().to_path_buf();
        let items = test_items(DOWNLOAD_WORKER_LIMIT + 3);
        let item_count = items.len();
        let manager = DownloadManager::default();
        let active_job = manager.claim_job().expect("claim coordinator job");
        let worker_state = Arc::new(ConcurrentWorkerState::default());
        let worker = Arc::new(ConcurrentWorker {
            state: worker_state.clone(),
        });
        let progress = Arc::new(Mutex::new(Vec::new()));
        let progress_for_coordinator = progress.clone();
        let tools = test_runtime_tools();
        let coordinator_job = active_job.clone();
        let coordinator_checkpoint_path = checkpoint_path.clone();

        let coordinator = thread::spawn(move || {
            run_coordinated_download_with_worker(
                &coordinator_job,
                "concurrency-job",
                &coordinator_checkpoint_path,
                &tools,
                &download_path,
                items,
                OutputLayout::Indexed {
                    width: 2,
                    include_uploader: true,
                },
                "done",
                worker,
                move |aggregate| {
                    progress_for_coordinator
                        .lock()
                        .expect("record aggregate progress")
                        .push((aggregate.completed, aggregate.total, aggregate.active.len()));
                },
            )
        });

        worker_state.wait_for_started(DOWNLOAD_WORKER_LIMIT);
        assert!(manager.claim_job().is_err());
        worker_state.release();

        let result = coordinator.join().expect("join coordinator");
        let result = match result {
            Ok(result) => result,
            Err(error) => panic!("coordinator unexpectedly failed: {error}"),
        };
        manager.release_job(&active_job);

        assert_eq!(result.status, "completed");
        assert!(!checkpoint_path.exists());

        let (max_active, mut started) = worker_state.snapshot();
        started.sort_unstable();
        assert!(
            max_active <= DOWNLOAD_WORKER_LIMIT,
            "started {max_active} workers with a cap of {DOWNLOAD_WORKER_LIMIT}"
        );
        assert_eq!(max_active, DOWNLOAD_WORKER_LIMIT);
        assert_eq!(started, (1..=item_count).collect::<Vec<_>>());

        let progress = progress.lock().expect("read aggregate progress");
        assert!(!progress.is_empty());
        assert!(progress
            .windows(2)
            .all(|pair| pair[0].0 <= pair[1].0 && pair[0].1 == pair[1].1));
        assert_eq!(progress.last(), Some(&(item_count, item_count, 0)));
    }

    #[test]
    fn coordinator_skips_checkpointed_items_and_preserves_successes_after_a_failure() {
        let directory = TestDirectory::new();
        let checkpoint_path = directory.path().join("resume.json");
        let items = test_items(3);
        let first_key = items[0].checkpoint_key.clone();
        let successful_key = items[1].checkpoint_key.clone();
        let failed_key = items[2].checkpoint_key.clone();
        write_checkpoint(
            &checkpoint_path,
            &DownloadCheckpoint {
                schema_version: CHECKPOINT_SCHEMA_VERSION,
                job_id: "resume-job".to_string(),
                completed: BTreeSet::from([first_key.clone()]),
            },
        )
        .expect("seed checkpoint");

        let manager = DownloadManager::default();
        let active_job = manager.claim_job().expect("claim coordinator job");
        let worker = Arc::new(ScriptedWorker {
            failed_key: failed_key.clone(),
            calls: Mutex::new(Vec::new()),
        });
        let result = run_coordinated_download_with_worker(
            &active_job,
            "resume-job",
            &checkpoint_path,
            &test_runtime_tools(),
            directory.path(),
            items,
            OutputLayout::Indexed {
                width: 2,
                include_uploader: false,
            },
            "done",
            worker.clone(),
            |_| {},
        );
        manager.release_job(&active_job);

        let error = match result {
            Ok(result) => panic!("coordinator unexpectedly completed: {}", result.status),
            Err(error) => error,
        };
        assert!(error.contains("1 item(s) could not be downloaded"));
        assert!(error.contains("2 of 3 item(s) were checkpointed"));
        assert!(error.contains("item 3: simulated yt-dlp failure"));

        let mut calls = worker
            .calls
            .lock()
            .expect("read scripted worker calls")
            .clone();
        calls.sort();
        assert_eq!(calls, vec![successful_key.clone(), failed_key.clone()]);

        let checkpoint = read_checkpoint(&checkpoint_path, "resume-job").expect("read checkpoint");
        assert_eq!(
            checkpoint.completed,
            BTreeSet::from([first_key, successful_key])
        );
        assert!(!checkpoint.completed.contains(&failed_key));
    }

    fn assert_interrupted_coordinator_cleans_or_retains_checkpoints(stop: bool) {
        let directory = TestDirectory::new();
        let checkpoint_path = directory.path().join("interrupted.json");
        let items = test_items(DOWNLOAD_WORKER_LIMIT + 2);
        let retained_key = items[0].checkpoint_key.clone();
        write_checkpoint(
            &checkpoint_path,
            &DownloadCheckpoint {
                schema_version: CHECKPOINT_SCHEMA_VERSION,
                job_id: "interruption-job".to_string(),
                completed: BTreeSet::from([retained_key.clone()]),
            },
        )
        .expect("seed checkpoint");

        let manager = DownloadManager::default();
        let active_job = manager.claim_job().expect("claim coordinator job");
        let worker_state = Arc::new(InterruptionWorkerState::default());
        let worker = Arc::new(InterruptibleWorker {
            state: worker_state.clone(),
        });
        let coordinator_manager = manager.clone();
        let coordinator_job = active_job.clone();
        let coordinator_checkpoint_path = checkpoint_path.clone();
        let tools = test_runtime_tools();
        let download_path = directory.path().to_path_buf();
        let coordinator = thread::spawn(move || {
            let result = run_coordinated_download_with_worker(
                &coordinator_job,
                "interruption-job",
                &coordinator_checkpoint_path,
                &tools,
                &download_path,
                items,
                OutputLayout::Indexed {
                    width: 2,
                    include_uploader: false,
                },
                "done",
                worker,
                |_| {},
            );
            coordinator_manager.release_job(&coordinator_job);
            result
        });

        worker_state.wait_for_started(DOWNLOAD_WORKER_LIMIT);
        let requested_job = manager
            .request_interruption(stop)
            .expect("request interruption");
        assert!(Arc::ptr_eq(&active_job, &requested_job));

        let waiting_manager = manager.clone();
        let waiting_job = requested_job.clone();
        let (waiter_sender, waiter_receiver) = mpsc::channel();
        let waiter = thread::spawn(move || {
            let _ = waiter_sender.send(waiting_manager.wait_for_job(&waiting_job));
        });

        worker_state.permit_interruption_check();
        worker_state.wait_for_observed_interruption(DOWNLOAD_WORKER_LIMIT);
        assert!(matches!(
            waiter_receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        worker_state.release();
        assert!(waiter_receiver
            .recv_timeout(TEST_TIMEOUT)
            .expect("waiter did not observe coordinator completion")
            .is_ok());
        waiter.join().expect("join interruption waiter");

        let result = coordinator.join().expect("join coordinator");
        let result = match result {
            Ok(result) => result,
            Err(error) => panic!("coordinator unexpectedly failed: {error}"),
        };
        assert_eq!(result.status, if stop { "stopped" } else { "paused" });

        if stop {
            assert!(!checkpoint_path.exists());
        } else {
            let checkpoint =
                read_checkpoint(&checkpoint_path, "interruption-job").expect("read checkpoint");
            assert_eq!(checkpoint.completed, BTreeSet::from([retained_key]));
        }
    }

    #[test]
    fn coordinator_pause_interrupts_all_active_workers_and_retains_checkpoints() {
        assert_interrupted_coordinator_cleans_or_retains_checkpoints(false);
    }

    #[test]
    fn coordinator_stop_interrupts_all_active_workers_waits_and_clears_checkpoints() {
        assert_interrupted_coordinator_cleans_or_retains_checkpoints(true);
    }

    #[test]
    fn pins_and_checksums_runtime_assets_for_supported_platforms() {
        for (os, arch) in [
            ("windows", "x86_64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
        ] {
            let assets = runtime_assets_for(os, arch).unwrap();
            assert_eq!(assets.len(), 3);
            assert_eq!(assets[0].component, "yt-dlp");
            assert_eq!(assets[1].component, "ffmpeg");
            assert_eq!(assets[2].component, "ffprobe");
            assert!(assets.iter().all(|asset| asset.url.starts_with("https://")));
            assert!(assets.iter().all(|asset| {
                asset.sha256.len() == 64
                    && asset
                        .sha256
                        .chars()
                        .all(|character| character.is_ascii_hexdigit())
            }));
        }
    }

    #[test]
    fn rejects_unsupported_runtime_platforms() {
        assert!(runtime_assets_for("windows", "aarch64").is_err());
        assert!(runtime_assets_for("linux", "arm").is_err());
        assert!(runtime_assets_for("freebsd", "x86_64").is_err());
    }
}
