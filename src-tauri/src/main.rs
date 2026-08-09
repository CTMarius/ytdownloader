use reqwest::{blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
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
struct DownloadResult {
    message: String,
    status: String,
}

#[derive(Deserialize)]
struct PodcastPlaylistMetadata {
    title: Option<String>,
    entries: Option<Vec<serde_json::Value>>,
    #[serde(rename = "_type")]
    item_type: Option<String>,
}

#[derive(Clone, Serialize)]
struct DownloadProgress {
    current: usize,
    total: usize,
    percent: String,
    kind: String,
}

/// Tracks the currently running `yt-dlp` process (if any) so it can be
/// stopped or paused from a separate command invocation, without blocking
/// the UI thread while a download is in progress.
#[derive(Default)]
struct DownloadManager {
    child: Arc<Mutex<Option<Arc<Mutex<Child>>>>>,
    stop_requested: Arc<AtomicBool>,
    pause_requested: Arc<AtomicBool>,
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
) -> Result<DownloadResult, String> {
    let url = validate_youtube_url(&url)?;
    let download_path = validate_download_directory(&path)?;
    let tools = resolve_runtime_tools(&app)?;
    let kind = match download_type {
        DownloadType::Single => "single",
        DownloadType::Playlist => "playlist",
    };
    let archive_path = archive_path_for(kind, &url)?;

    let mut command = yt_dlp_command(&tools);
    command
        .arg("--extract-audio")
        .arg("--audio-format")
        .arg("mp3")
        .arg("--audio-quality")
        .arg("320K")
        .arg("--newline")
        .arg("--download-archive")
        .arg(&archive_path)
        .arg("--paths")
        .arg(&download_path)
        .arg("--output");

    match download_type {
        DownloadType::Single => {
            command
                .arg("%(uploader)s/%(title)s.%(ext)s")
                .arg("--no-playlist")
                .arg("--progress-template")
                .arg("download-progress:1/1:%(progress._percent_str)s");
        }
        DownloadType::Playlist => {
            command
                .arg("%(uploader)s/%(playlist_index)02d - %(title)s.%(ext)s")
                .arg("--progress-template")
                .arg(
                    "download-progress:%(info.playlist_index)s/%(info.playlist_count)s:%(progress._percent_str)s",
                );
        }
    }

    command.arg("--").arg(url.as_str());

    let success_message = match download_type {
        DownloadType::Single => "Finished downloading the video as an MP3.".to_string(),
        DownloadType::Playlist => "Finished downloading playlist audio as MP3 files.".to_string(),
    };

    run_monitored_download(&state, &app, kind, archive_path, command, success_message).await
}

#[command]
async fn download_podcast(
    app: AppHandle,
    state: State<'_, DownloadManager>,
    url: String,
    path: String,
) -> Result<DownloadResult, String> {
    let url = validate_podcast_feed_url(&url)?;
    let download_path = validate_download_directory(&path)?;
    let tools = resolve_runtime_tools(&app)?;
    let metadata = read_podcast_metadata(&tools, &url)?;
    let folder_name = sanitize_podcast_folder_title(metadata.title.as_deref().unwrap_or_default());
    let podcast_path = create_podcast_directory(&download_path, &folder_name)?;
    let archive_path = archive_path_for("podcast", &url)?;

    let mut command = yt_dlp_command(&tools);
    command
        .arg("--extract-audio")
        .arg("--audio-format")
        .arg("mp3")
        .arg("--audio-quality")
        .arg("320K")
        .arg("--yes-playlist")
        .arg("--newline")
        .arg("--download-archive")
        .arg(&archive_path)
        .arg("--progress-template")
        .arg(
            "download-progress:%(info.playlist_index)s/%(info.playlist_count)s:%(progress._percent_str)s",
        )
        .arg("--paths")
        .arg(&podcast_path)
        .arg("--output")
        .arg("%(playlist_index)04d - %(title)s.%(ext)s")
        .arg("--")
        .arg(url.as_str());

    let success_message = format!(
        "Finished downloading all available podcast episodes to the “{folder_name}” folder."
    );

    run_monitored_download(
        &state,
        &app,
        "podcast",
        archive_path,
        command,
        success_message,
    )
    .await
}

/// Requests that the active download stop or pause. Both simply terminate the
/// running `yt-dlp` process (it is not truly suspendable across platforms);
/// the difference is that stopping also clears saved progress so a future
/// download starts fresh, while pausing keeps it so Resume can skip
/// already-completed items via `--download-archive`.
fn request_download_interruption(
    state: &DownloadManager,
    stop: bool,
) -> Result<DownloadResult, String> {
    let guard = state.child.lock().unwrap();
    let Some(child_arc) = guard.as_ref() else {
        return Err("No download is currently running.".to_string());
    };

    if stop {
        state.stop_requested.store(true, Ordering::SeqCst);
    } else {
        state.pause_requested.store(true, Ordering::SeqCst);
    }

    child_arc
        .lock()
        .unwrap()
        .kill()
        .map_err(|error| format!("Could not stop the download: {error}"))?;

    Ok(DownloadResult {
        message: if stop {
            "Stopping the download…".to_string()
        } else {
            "Pausing the download…".to_string()
        },
        status: "stopping".to_string(),
    })
}

#[command]
fn stop_download(state: State<'_, DownloadManager>) -> Result<DownloadResult, String> {
    request_download_interruption(&state, true)
}

#[command]
fn pause_download(state: State<'_, DownloadManager>) -> Result<DownloadResult, String> {
    request_download_interruption(&state, false)
}

fn yt_dlp_command(tools: &RuntimeTools) -> Command {
    let mut command = Command::new(&tools.yt_dlp);
    command.arg("--ffmpeg-location").arg(&tools.directory);
    command
}

/// Runs `yt-dlp` on a dedicated blocking thread (so the UI never freezes),
/// streams progress events to the frontend, and tracks the child process in
/// `DownloadManager` so `stop_download`/`pause_download` can interrupt it.
async fn run_monitored_download(
    state: &DownloadManager,
    app: &AppHandle,
    kind: &'static str,
    archive_path: PathBuf,
    mut command: Command,
    success_message: String,
) -> Result<DownloadResult, String> {
    let child_slot = state.child.clone();
    let stop_flag = state.stop_requested.clone();
    let pause_flag = state.pause_requested.clone();
    let progress_app = app.clone();

    tauri::async_runtime::spawn_blocking(move || {
        stop_flag.store(false, Ordering::SeqCst);
        pause_flag.store(false, Ordering::SeqCst);

        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => {
                    "The app's private yt-dlp tool is unavailable. Restart the app and complete setup."
                        .to_string()
                }
                _ => format!("Could not start the app's private yt-dlp tool: {error}"),
            })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Could not read yt-dlp download progress.".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "Could not read yt-dlp download errors.".to_string())?;

        let progress_kind = kind.to_string();
        let progress_reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some((current, total, percent)) = parse_download_progress(&line) {
                    let _ = progress_app.emit(
                        "download-progress",
                        DownloadProgress {
                            current,
                            total,
                            percent,
                            kind: progress_kind.clone(),
                        },
                    );
                }
            }
        });
        let error_reader = thread::spawn(move || {
            let mut output = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut output);
            output
        });

        let child_arc = Arc::new(Mutex::new(child));
        *child_slot.lock().unwrap() = Some(child_arc.clone());

        let status = loop {
            let wait_result = child_arc.lock().unwrap().try_wait();
            match wait_result {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(Duration::from_millis(150)),
                Err(error) => {
                    *child_slot.lock().unwrap() = None;
                    return Err(format!("Could not wait for yt-dlp to finish: {error}"));
                }
            }
        };

        *child_slot.lock().unwrap() = None;
        let _ = progress_reader.join();
        let error_output = error_reader.join().unwrap_or_default();

        let was_stopped = stop_flag.swap(false, Ordering::SeqCst);
        let was_paused = pause_flag.swap(false, Ordering::SeqCst);

        if was_stopped {
            let _ = fs::remove_file(&archive_path);
            return Ok(DownloadResult {
                message: "Download stopped. Progress for this download was cleared.".to_string(),
                status: "stopped".to_string(),
            });
        }

        if was_paused {
            return Ok(DownloadResult {
                message: "Download paused. Resume anytime to continue from where it left off."
                    .to_string(),
                status: "paused".to_string(),
            });
        }

        if !status.success() {
            let details = if error_output.trim().is_empty() {
                format!("process exited with status {status}")
            } else {
                error_output.chars().take(1_000).collect::<String>()
            };
            return Err(format!(
                "yt-dlp stopped before finishing. Check that the source is public, the destination is writable, and ffmpeg is installed for audio conversion: {details}"
            ));
        }

        let _ = fs::remove_file(&archive_path);
        Ok(DownloadResult {
            message: success_message,
            status: "completed".to_string(),
        })
    })
    .await
    .map_err(|error| format!("The download task ended unexpectedly: {error}"))?
}

/// Derives a stable, per-source path for `yt-dlp`'s `--download-archive`
/// file so pausing and resuming the same URL skips already-downloaded items.
fn archive_path_for(kind: &str, url: &Url) -> Result<PathBuf, String> {
    let mut hasher = DefaultHasher::new();
    url.as_str().hash(&mut hasher);
    let hash = hasher.finish();

    let base = dirs::cache_dir()
        .or_else(dirs::home_dir)
        .ok_or_else(|| "Could not determine a location to track download progress.".to_string())?
        .join("yt-dlp-tauri")
        .join("archives");
    fs::create_dir_all(&base)
        .map_err(|error| format!("Could not prepare download progress tracking: {error}"))?;

    Ok(base.join(format!("{kind}-{hash:x}.txt")))
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

fn read_podcast_metadata(
    tools: &RuntimeTools,
    url: &Url,
) -> Result<PodcastPlaylistMetadata, String> {
    let output = yt_dlp_command(tools)
        .arg("--simulate")
        .arg("--flat-playlist")
        .arg("--playlist-end")
        .arg("1")
        .arg("--dump-single-json")
        .arg("--no-warnings")
        .arg("--quiet")
        .arg("--")
        .arg(url.as_str())
        .output()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                "The app's private yt-dlp tool is unavailable. Restart the app and complete setup."
                    .to_string()
            }
            _ => format!(
                "Could not start the app's private yt-dlp tool to inspect the podcast feed: {error}"
            ),
        })?;

    if !output.status.success() {
        return Err(
            "yt-dlp could not read this podcast feed. Confirm that the URL is a public RSS feed supported by yt-dlp."
                .to_string(),
        );
    }

    let metadata: PodcastPlaylistMetadata = serde_json::from_slice(&output.stdout).map_err(|_| {
        "yt-dlp did not return valid podcast feed information. Confirm that the URL is an RSS feed."
            .to_string()
    })?;
    if metadata.item_type.as_deref() != Some("playlist")
        || metadata.entries.as_ref().is_none_or(Vec::is_empty)
    {
        return Err(
            "This RSS feed has no downloadable podcast episodes. Choose a feed with at least one episode."
                .to_string(),
        );
    }

    Ok(metadata)
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

fn parse_download_progress(line: &str) -> Option<(usize, usize, String)> {
    let progress = line.strip_prefix("download-progress:")?;
    let (item, percent) = progress.split_once(':')?;
    let (current, total) = item.split_once('/')?;
    let current = current.trim().parse().ok()?;
    let total = total.trim().parse().ok()?;

    Some((current, total, percent.trim().chars().take(12).collect()))
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
        parse_download_progress, runtime_assets_for, sanitize_podcast_folder_title,
        validate_podcast_feed_url, validate_youtube_url,
    };

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
