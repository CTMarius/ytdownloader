use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use tauri::{command, AppHandle, Emitter, State};
use url::Url;

const SETTINGS_FILE: &str = ".yt-dlp-tauri-settings";

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum DownloadType {
    Single,
    Playlist,
}

#[derive(Serialize)]
struct InstallationStatus {
    version: String,
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

#[command]
fn check_installation() -> Result<InstallationStatus, String> {
    let output = Command::new("yt-dlp")
        .arg("--version")
        .output()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                "yt-dlp was not found. Install yt-dlp and ensure it is available on your PATH."
                    .to_string()
            }
            _ => format!("Could not run yt-dlp: {error}"),
        })?;

    if !output.status.success() {
        return Err(format!(
            "yt-dlp could not start successfully: {}",
            command_output_message(&output)
        ));
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        return Err("yt-dlp did not return a version number.".to_string());
    }

    Ok(InstallationStatus { version })
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
    let kind = match download_type {
        DownloadType::Single => "single",
        DownloadType::Playlist => "playlist",
    };
    let archive_path = archive_path_for(kind, &url)?;

    let mut command = Command::new("yt-dlp");
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
    let metadata = read_podcast_metadata(&url)?;
    let folder_name = sanitize_podcast_folder_title(metadata.title.as_deref().unwrap_or_default());
    let podcast_path = create_podcast_directory(&download_path, &folder_name)?;
    let archive_path = archive_path_for("podcast", &url)?;

    let mut command = Command::new("yt-dlp");
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

    run_monitored_download(&state, &app, "podcast", archive_path, command, success_message).await
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
                    "yt-dlp was not found. Install yt-dlp and restart the application.".to_string()
                }
                _ => format!("Could not start yt-dlp: {error}"),
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

fn read_podcast_metadata(url: &Url) -> Result<PodcastPlaylistMetadata, String> {
    let output = Command::new("yt-dlp")
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
                "yt-dlp was not found. Install yt-dlp and restart the application.".to_string()
            }
            _ => format!("Could not start yt-dlp to inspect the podcast feed: {error}"),
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

fn command_output_message(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let message = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };

    if message.is_empty() {
        format!("process exited with status {}", output.status)
    } else {
        message.chars().take(1_000).collect()
    }
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
        .invoke_handler(tauri::generate_handler![
            check_installation,
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
        parse_download_progress, sanitize_podcast_folder_title, validate_podcast_feed_url,
        validate_youtube_url,
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
}
