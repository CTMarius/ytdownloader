import { FormEvent, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import "./App.css";

type DownloadType = "single" | "playlist";
type NoticeTone = "neutral" | "success" | "error";

interface InstallationStatus {
  version: string;
}

interface DownloadResult {
  message: string;
}

interface Notice {
  message: string;
  tone: NoticeTone;
}

const YOUTUBE_HOSTS = ["youtube.com", "youtu.be", "youtube-nocookie.com"];

function isYouTubeUrl(value: string): boolean {
  try {
    const parsedUrl = new URL(value.trim());
    const host = parsedUrl.hostname.toLowerCase();

    return (
      (parsedUrl.protocol === "https:" || parsedUrl.protocol === "http:") &&
      YOUTUBE_HOSTS.some(
        (allowedHost) => host === allowedHost || host.endsWith(`.${allowedHost}`),
      )
    );
  } catch {
    return false;
  }
}

function errorMessage(error: unknown): string {
  if (typeof error === "string") {
    return error;
  }

  if (error instanceof Error) {
    return error.message;
  }

  return "An unexpected error occurred. Please try again.";
}

function App() {
  const [url, setUrl] = useState("");
  const [downloadPath, setDownloadPath] = useState("");
  const [downloadType, setDownloadType] = useState<DownloadType>("single");
  const [isInitializing, setIsInitializing] = useState(true);
  const [isSelectingPath, setIsSelectingPath] = useState(false);
  const [isDownloading, setIsDownloading] = useState(false);
  const [toolStatus, setToolStatus] = useState<Notice>({
    message: "Checking yt-dlp…",
    tone: "neutral",
  });
  const [notice, setNotice] = useState<Notice>({
    message: "Choose a YouTube link and a destination to get started.",
    tone: "neutral",
  });

  useEffect(() => {
    let cancelled = false;

    const initialise = async () => {
      try {
        const installation = await invoke<InstallationStatus>("check_installation");
        if (!cancelled) {
          setToolStatus({
            message: `yt-dlp ${installation.version} is ready.`,
            tone: "success",
          });
        }
      } catch (error) {
        if (!cancelled) {
          setToolStatus({ message: errorMessage(error), tone: "error" });
        }
      }

      try {
        const savedPath = await invoke<string>("get_download_path");
        if (!cancelled) {
          setDownloadPath(savedPath);
        }
      } catch (error) {
        if (!cancelled) {
          setNotice({
            message: `Could not load the saved destination: ${errorMessage(error)}`,
            tone: "error",
          });
        }
      } finally {
        if (!cancelled) {
          setIsInitializing(false);
        }
      }
    };

    void initialise();

    return () => {
      cancelled = true;
    };
  }, []);

  const handleSelectPath = async () => {
    setIsSelectingPath(true);

    try {
      const selectedPath = await open({
        directory: true,
        multiple: false,
        title: "Choose download destination",
      });
      const path = Array.isArray(selectedPath) ? selectedPath[0] : selectedPath;

      if (!path) {
        return;
      }

      const savedPath = await invoke<string>("save_download_path", { path });
      setDownloadPath(savedPath);
      setNotice({
        message: "Download destination saved.",
        tone: "success",
      });
    } catch (error) {
      setNotice({
        message: `Could not save the download destination: ${errorMessage(error)}`,
        tone: "error",
      });
    } finally {
      setIsSelectingPath(false);
    }
  };

  const handleDownload = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();

    const trimmedUrl = url.trim();
    const trimmedPath = downloadPath.trim();

    if (!trimmedUrl) {
      setNotice({ message: "Enter a YouTube video or playlist URL.", tone: "error" });
      return;
    }

    if (!isYouTubeUrl(trimmedUrl)) {
      setNotice({
        message: "Enter a valid http(s) URL from YouTube or youtu.be.",
        tone: "error",
      });
      return;
    }

    if (!trimmedPath) {
      setNotice({
        message: "Choose an existing folder for downloaded audio.",
        tone: "error",
      });
      return;
    }

    if (toolStatus.tone === "error") {
      setNotice({
        message: "yt-dlp must be available before a download can start.",
        tone: "error",
      });
      return;
    }

    setIsDownloading(true);
    setNotice({ message: "Downloading audio. This can take a few minutes…", tone: "neutral" });

    try {
      const result = await invoke<DownloadResult>("download_audio", {
        url: trimmedUrl,
        downloadType,
        path: trimmedPath,
      });
      setNotice({ message: result.message, tone: "success" });
    } catch (error) {
      setNotice({
        message: `Download failed: ${errorMessage(error)}`,
        tone: "error",
      });
    } finally {
      setIsDownloading(false);
    }
  };

  const downloadUnavailable =
    isInitializing || isSelectingPath || isDownloading || toolStatus.tone === "error";

  return (
    <main className="app-shell">
      <section className="download-card" aria-labelledby="app-title">
        <header className="card-header">
          <p className="eyebrow">Audio downloader</p>
          <h1 id="app-title">YouTube to MP3</h1>
          <p className="intro">Save a video or playlist as high-quality MP3 files.</p>
        </header>

        <p className={`tool-status ${toolStatus.tone}`} role="status" aria-live="polite">
          {toolStatus.message}
        </p>

        <form onSubmit={handleDownload}>
          <div className="field">
            <label htmlFor="youtube-url">YouTube URL</label>
            <input
              id="youtube-url"
              name="youtube-url"
              type="url"
              inputMode="url"
              autoComplete="url"
              value={url}
              onChange={(event) => setUrl(event.target.value)}
              placeholder="https://www.youtube.com/watch?v=…"
              aria-describedby="url-help"
              disabled={isDownloading}
              required
            />
            <p id="url-help" className="help-text">
              Paste a YouTube video or playlist link.
            </p>
          </div>

          <fieldset className="download-type">
            <legend>Download</legend>
            <label>
              <input
                type="radio"
                name="download-type"
                value="single"
                checked={downloadType === "single"}
                onChange={() => setDownloadType("single")}
                disabled={isDownloading}
              />
              Single video
            </label>
            <label>
              <input
                type="radio"
                name="download-type"
                value="playlist"
                checked={downloadType === "playlist"}
                onChange={() => setDownloadType("playlist")}
                disabled={isDownloading}
              />
              Entire playlist
            </label>
          </fieldset>

          <div className="field">
            <span id="destination-label" className="field-label">
              Download destination
            </span>
            <div className="destination-row">
              <output className="destination-path" aria-labelledby="destination-label">
                {downloadPath || "No folder selected"}
              </output>
              <button
                type="button"
                className="secondary-button"
                onClick={handleSelectPath}
                disabled={isInitializing || isDownloading || isSelectingPath}
              >
                {isSelectingPath ? "Opening…" : "Choose folder"}
              </button>
            </div>
          </div>

          <button className="download-button" type="submit" disabled={downloadUnavailable}>
            {isDownloading ? "Downloading…" : "Download MP3"}
          </button>
        </form>

        <p className={`notice ${notice.tone}`} role={notice.tone === "error" ? "alert" : "status"} aria-live="polite">
          {notice.message}
        </p>
      </section>
    </main>
  );
}

export default App;
