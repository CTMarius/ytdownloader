import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from '@tauri-apps/plugin-dialog'; 
import "./App.css";

function App() {
  const [url, setUrl] = useState("");
  const [downloadPath, setDownloadPath] = useState("");
  const [isPlaylist, setIsPlaylist] = useState(false);
  const [status, setStatus] = useState("Checking yt-dlp...");

  useEffect(() => {
    invoke("check_installation").then((result) => setStatus(result as string));
    invoke("get_download_path").then((result) => setDownloadPath(result as string)).catch(() => setDownloadPath("~/Music"));
  }, []);

  const handleDownload = async () => {
    if (!url) {
      setStatus("Please enter a URL");
      return;
    }
    setStatus("Downloading...");
    const result = await invoke("download_audio", {
      url,
      downloadType: isPlaylist ? "playlist" : "single",
      path: downloadPath
    });
    setStatus(result as string);
  };

  const handleSelectPath = async () => {
    const selectedPath = await open({ directory: true });
    if (selectedPath) {
      setDownloadPath(selectedPath);
      invoke("save_download_path", { path: selectedPath });
    }
  };

  return (
    <div className="container">
      <h1>YouTube Audio Downloader</h1>
      <input 
        type="text" 
        value={url} 
        onChange={(e) => setUrl(e.target.value)} 
        placeholder="Enter YouTube URL" 
      />
      <div>
        <button onClick={handleSelectPath}>Select Download Path</button>
        <span>{downloadPath}</span>
      </div>
      <div>
        <label>
          <input 
            type="checkbox" 
            checked={isPlaylist} 
            onChange={() => setIsPlaylist(!isPlaylist)} 
          />
          Download entire playlist
        </label>
      </div>
      <button onClick={handleDownload}>Download</button>
      <p>{status}</p>
    </div>
  );
}

export default App;
