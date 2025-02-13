use std::process::Command;
use dirs::home_dir;
use tauri::command;
extern crate tauri;
use std::fs;

#[command]
fn check_installation() -> String {
    let output = Command::new("yt-dlp").arg("--version").output();
    match output {
        Ok(_) => "yt-dlp is installed".to_string(),
        Err(_) => {
            let install_cmd = Command::new("sh")
                .arg("-c")
                .arg("pip install -U yt-dlp")
                .output();
            match install_cmd {
                Ok(_) => "yt-dlp installed successfully".to_string(),
                Err(e) => format!("Failed to install yt-dlp: {}", e),
            }
        }
    }
}

#[command]
fn save_download_path(path: String) -> Result<(), String> {
    let config_path = home_dir()
        .ok_or("Could not find home directory")?
        .join(".yt-dlp-tauri-settings");
    fs::write(config_path, path).map_err(|e| e.to_string())
}

#[command]
fn get_download_path() -> Result<String, String> {
    let config_path = home_dir()
        .ok_or("Could not find home directory")?
        .join(".yt-dlp-tauri-settings");
    fs::read_to_string(config_path).map_err(|_| "Default path: ~/Music".to_string())
}

#[command]
fn download_audio(url: String, download_type: String, path: String) -> String {
    let output_template = match download_type.as_str() {
        "single" => format!("{}/%(uploader)s/%(section_number)s - %(section_title)s.%(ext)s", path),
        "playlist" => format!("{}/%(uploader)s/%(playlist_index)s - %(title)s.%(ext)s", path),
        _ => return "Invalid download type".to_string(),
    };

    let mut cmd = Command::new("yt-dlp");
    cmd.arg("-x")
       .arg("--audio-format").arg("mp3")
       .arg("--audio-quality").arg("320K")
       .arg("-o").arg(output_template)
       .arg(url);

    if download_type == "single" {
        cmd.arg("--split-chapters");
    }

    match cmd.output() {
        Ok(output) => {
            if output.status.success() {
                String::from_utf8_lossy(&output.stdout).to_string()
            } else {
                String::from_utf8_lossy(&output.stderr).to_string()
            }
        }
        Err(e) => format!("Error: {}", e),
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            check_installation,
            save_download_path,
            get_download_path,
            download_audio
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}
