use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tauri::command;
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

#[derive(Serialize)]
struct DownloadResult {
    message: String,
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
                    "The saved download destination is empty. Choose an existing folder.".to_string(),
                );
            }

            path_to_string(&validate_download_directory(saved_path)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let default_path = default_download_directory()?;
            fs::create_dir_all(&default_path)
                .map_err(|error| format!("Could not create the default download destination: {error}"))?;
            path_to_string(&validate_download_directory(
                default_path.to_string_lossy().as_ref(),
            )?)
        }
        Err(error) => Err(format!("Could not read the saved download destination: {error}")),
    }
}

#[command]
fn download_audio(
    url: String,
    download_type: DownloadType,
    path: String,
) -> Result<DownloadResult, String> {
    let url = validate_youtube_url(&url)?;
    let download_path = validate_download_directory(&path)?;

    let mut command = Command::new("yt-dlp");
    command
        .arg("--extract-audio")
        .arg("--audio-format")
        .arg("mp3")
        .arg("--audio-quality")
        .arg("320K")
        .arg("--paths")
        .arg(download_path)
        .arg("--output");

    match download_type {
        DownloadType::Single => {
            command
                .arg("%(uploader)s/%(title)s.%(ext)s")
                .arg("--no-playlist");
        }
        DownloadType::Playlist => {
            command.arg("%(uploader)s/%(playlist_index)02d - %(title)s.%(ext)s");
        }
    }

    let output = command
        .arg("--")
        .arg(url.as_str())
        .output()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                "yt-dlp was not found. Install yt-dlp and restart the application.".to_string()
            }
            _ => format!("Could not start yt-dlp: {error}"),
        })?;

    if !output.status.success() {
        return Err(format!("yt-dlp failed: {}", command_output_message(&output)));
    }

    let message = match download_type {
        DownloadType::Single => "Finished downloading the video as an MP3.",
        DownloadType::Playlist => "Finished downloading playlist audio as MP3 files.",
    };

    Ok(DownloadResult {
        message: message.to_string(),
    })
}

fn settings_path() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join(SETTINGS_FILE))
        .ok_or_else(|| "Could not find the home directory for app settings.".to_string())
}

fn default_download_directory() -> Result<PathBuf, String> {
    dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Music")))
        .ok_or_else(|| "Could not determine a default download destination. Choose a folder.".to_string())
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

    Ok(canonical_path)
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
        .invoke_handler(tauri::generate_handler![
            check_installation,
            save_download_path,
            get_download_path,
            download_audio
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}

#[cfg(test)]
mod tests {
    use super::validate_youtube_url;

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
}
