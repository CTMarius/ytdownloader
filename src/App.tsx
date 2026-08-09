import { FormEvent, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

type DownloadType = "single" | "playlist" | "podcast";
type NoticeTone = "neutral" | "success" | "error";

interface InstallationStatus {
  version: string;
}

interface DownloadResult {
  message: string;
  status: "completed" | "stopped" | "paused" | "stopping";
}

interface Notice {
  message: string;
  tone: NoticeTone;
}

interface DownloadProgress {
  current: number;
  total: number;
  percent: string;
  kind: "single" | "playlist" | "podcast";
}

interface StartedDownloadParams {
  url: string;
  downloadType: DownloadType;
  path: string;
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

function isHttpUrl(value: string): boolean {
  try {
    const parsedUrl = new URL(value.trim());
    return (
      (parsedUrl.protocol === "https:" || parsedUrl.protocol === "http:") &&
      Boolean(parsedUrl.hostname)
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
  const [isCheckingInstallation, setIsCheckingInstallation] = useState(true);
  const [isLoadingPath, setIsLoadingPath] = useState(true);
  const [isSelectingPath, setIsSelectingPath] = useState(false);
  const [isDownloading, setIsDownloading] = useState(false);
  const [isPaused, setIsPaused] = useState(false);
  const [isPausing, setIsPausing] = useState(false);
  const [isStopping, setIsStopping] = useState(false);
  const [progress, setProgress] = useState<DownloadProgress | null>(null);
  const [resumeParams, setResumeParams] = useState<StartedDownloadParams | null>(null);
  const [toolStatus, setToolStatus] = useState<Notice>({
    message: "Checking yt-dlp…",
    tone: "neutral",
  });
  const [notice, setNotice] = useState<Notice>({
    message: "Choose a YouTube link or podcast feed and a destination to get started.",
    tone: "neutral",
  });

  useEffect(() => {
    let cancelled = false;

    const initialise = async () => {
      const [installationResult, pathResult] = await Promise.allSettled([
        invoke<InstallationStatus>("check_installation"),
        invoke<string>("get_download_path"),
      ]);

      if (cancelled) {
        return;
      }

      if (installationResult.status === "fulfilled") {
        setToolStatus({
          message: `yt-dlp ${installationResult.value.version} is ready.`,
          tone: "success",
        });
      } else {
        setToolStatus({ message: errorMessage(installationResult.reason), tone: "error" });
      }
      setIsCheckingInstallation(false);

      if (pathResult.status === "fulfilled") {
        setDownloadPath(pathResult.value);
      } else {
        setNotice({
          message: `Could not load the saved destination: ${errorMessage(pathResult.reason)}`,
          tone: "error",
        });
      }
      setIsLoadingPath(false);
    };

    void initialise();

    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;

    void listen<DownloadProgress>("download-progress", (event) => {
      const { current, total } = event.payload;
      if (current > 0 && total > 0) {
        setProgress(event.payload);
      }
    })
      .then((stopListening) => {
        if (disposed) {
          stopListening();
        } else {
          unlisten = stopListening;
        }
      })
      .catch(() => {
        if (!disposed) {
          setNotice({
            message: "Download progress is unavailable, but downloads can still run.",
            tone: "neutral",
          });
        }
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const handleCheckInstallation = async () => {
    setIsCheckingInstallation(true);
    setToolStatus({ message: "Checking yt-dlp…", tone: "neutral" });

    try {
      const installation = await invoke<InstallationStatus>("check_installation");
      setToolStatus({
        message: `yt-dlp ${installation.version} is ready.`,
        tone: "success",
      });
    } catch (error) {
      setToolStatus({ message: errorMessage(error), tone: "error" });
    } finally {
      setIsCheckingInstallation(false);
    }
  };

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

  const startDownload = async (params: StartedDownloadParams) => {
    const { url: startUrl, downloadType: startType, path: startPath } = params;

    setIsDownloading(true);
    setIsPaused(false);
    setProgress(null);
    setResumeParams(params);
    setNotice({
      message:
        startType === "podcast"
          ? "Checking the podcast feed and preparing its folder…"
          : "Downloading audio. This can take a few minutes…",
      tone: "neutral",
    });

    try {
      const result =
        startType === "podcast"
          ? await invoke<DownloadResult>("download_podcast", {
              url: startUrl,
              path: startPath,
            })
          : await invoke<DownloadResult>("download_audio", {
              url: startUrl,
              downloadType: startType,
              path: startPath,
            });

      if (result.status === "paused") {
        setIsPaused(true);
        setNotice({ message: result.message, tone: "neutral" });
      } else {
        setIsPaused(false);
        setResumeParams(null);
        setProgress(null);
        setNotice({
          message: result.message,
          tone: result.status === "stopped" ? "neutral" : "success",
        });
      }
    } catch (error) {
      setIsPaused(false);
      setResumeParams(null);
      setProgress(null);
      setNotice({
        message: `Download failed: ${errorMessage(error)}`,
        tone: "error",
      });
    } finally {
      setIsDownloading(false);
    }
  };

  const resetPauseStateIfNeeded = () => {
    if (isPaused) {
      setIsPaused(false);
      setResumeParams(null);
      setProgress(null);
    }
  };

  const handleDownload = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();

    const trimmedUrl = url.trim();
    const trimmedPath = downloadPath.trim();

    if (!trimmedUrl) {
      setNotice({
        message:
          downloadType === "podcast"
            ? "Enter a podcast RSS feed URL."
            : "Enter a YouTube video or playlist URL.",
        tone: "error",
      });
      return;
    }

    if (downloadType !== "podcast" && !isYouTubeUrl(trimmedUrl)) {
      setNotice({
        message: "Enter a valid http(s) URL from YouTube or youtu.be.",
        tone: "error",
      });
      return;
    }

    if (downloadType === "podcast" && !isHttpUrl(trimmedUrl)) {
      setNotice({
        message: "Enter a valid http(s) podcast RSS feed URL.",
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

    await startDownload({ url: trimmedUrl, downloadType, path: trimmedPath });
  };

  const handleResume = async () => {
    if (!resumeParams) {
      return;
    }
    await startDownload(resumeParams);
  };

  const handlePause = async () => {
    setIsPausing(true);
    try {
      await invoke<DownloadResult>("pause_download");
    } catch (error) {
      setNotice({
        message: `Could not pause the download: ${errorMessage(error)}`,
        tone: "error",
      });
    } finally {
      setIsPausing(false);
    }
  };

  const handleStop = async () => {
    setIsStopping(true);
    try {
      await invoke<DownloadResult>("stop_download");
    } catch (error) {
      setNotice({
        message: `Could not stop the download: ${errorMessage(error)}`,
        tone: "error",
      });
    } finally {
      setIsStopping(false);
    }
  };

  const isInitializing = isCheckingInstallation || isLoadingPath;
  const downloadUnavailable =
    isInitializing || isSelectingPath || isDownloading || toolStatus.tone !== "success";
  const isPodcast = downloadType === "podcast";

  return (
    <main className="app-shell">
      <section className="download-card" aria-labelledby="app-title">
        <header className="card-header">
          <div className="brand-mark" aria-hidden="true">
            <svg viewBox="0 0 24 24" focusable="false">
              <path d="M9.5 8.5 16 12l-6.5 3.5v-7Z" />
              <path d="M3.5 12c0-3.6.4-5.8 1.5-6.9C6.1 4 8.4 3.5 12 3.5s5.9.5 7 1.6c1.1 1.1 1.5 3.3 1.5 6.9s-.4 5.8-1.5 6.9c-1.1 1.1-3.4 1.6-7 1.6s-5.9-.5-7-1.6C3.9 17.8 3.5 15.6 3.5 12Z" />
            </svg>
          </div>
          <div>
            <p className="eyebrow">Audio downloader</p>
            <h1 id="app-title">Audio to MP3</h1>
          </div>
          <p className="intro">Save YouTube videos, playlists, or podcast feeds as high-quality MP3 files.</p>
        </header>

        <div className={`tool-status ${toolStatus.tone}`} aria-live="polite">
          <span className="status-indicator" aria-hidden="true" />
          <span>{toolStatus.message}</span>
          {toolStatus.tone === "error" && (
            <button
              type="button"
              className="text-button"
              onClick={handleCheckInstallation}
              disabled={isCheckingInstallation || isDownloading}
            >
              Try again
            </button>
          )}
        </div>

        <form noValidate onSubmit={handleDownload}>
          <div className="field">
            <label htmlFor="source-url">{isPodcast ? "Podcast RSS feed URL" : "YouTube URL"}</label>
            <input
              id="source-url"
              name="source-url"
              type="url"
              inputMode="url"
              autoComplete="url"
              value={url}
              onChange={(event) => {
                setUrl(event.target.value);
                resetPauseStateIfNeeded();
              }}
              placeholder={
                isPodcast
                  ? "https://example.com/podcast.rss"
                  : "https://www.youtube.com/watch?v=…"
              }
              aria-describedby="url-help"
              disabled={isDownloading}
              required
            />
            <p id="url-help" className="help-text">
              {isPodcast
                ? "Paste a public RSS feed. Episodes are saved in a folder named for the podcast."
                : "Paste a YouTube video or playlist link."}
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
                onChange={() => {
                  setDownloadType("single");
                  resetPauseStateIfNeeded();
                }}
                disabled={isDownloading}
              />
              <span>
                <strong>Single video</strong>
                <small>One MP3 file</small>
              </span>
            </label>
            <label>
              <input
                type="radio"
                name="download-type"
                value="playlist"
                checked={downloadType === "playlist"}
                onChange={() => {
                  setDownloadType("playlist");
                  resetPauseStateIfNeeded();
                }}
                disabled={isDownloading}
              />
              <span>
                <strong>Playlist</strong>
                <small>Every available video</small>
              </span>
            </label>
            <label>
              <input
                type="radio"
                name="download-type"
                value="podcast"
                checked={downloadType === "podcast"}
                onChange={() => {
                  setDownloadType("podcast");
                  resetPauseStateIfNeeded();
                }}
                disabled={isDownloading}
              />
              <span>
                <strong>Podcast RSS feed</strong>
                <small>Every available episode</small>
              </span>
            </label>
          </fieldset>

          <div className="field">
            <span id="destination-label" className="field-label">
              Download destination
            </span>
            <div className="destination-row">
              <output
                className="destination-path"
                aria-labelledby="destination-label"
                title={downloadPath || undefined}
              >
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

          <button
            className="download-button"
            type="submit"
            disabled={downloadUnavailable && !isPaused}
            style={{ display: isPaused ? "none" : undefined }}
          >
            {isDownloading ? "Downloading…" : isPodcast ? "Download podcast" : "Download MP3"}
          </button>

          {isPaused && (
            <button
              className="download-button"
              type="button"
              onClick={handleResume}
              disabled={isPausing || isStopping}
            >
              Resume download
            </button>
          )}
        </form>

        {(isDownloading || progress) && (
          <div className="progress-panel" aria-live="polite">
            <div className="progress-meta">
              <span>
                {progress
                  ? `${progress.current} of ${progress.total} ${
                      progress.kind === "podcast" ? "episodes" : "items"
                    } downloaded`
                  : "Preparing download…"}
              </span>
              <strong>{progress?.percent ?? ""}</strong>
            </div>
            <div className="progress-bar-track">
              <div
                className="progress-bar-fill"
                style={{
                  width: progress
                    ? `${Math.min(100, Math.round((progress.current / progress.total) * 100))}%`
                    : "0%",
                }}
              />
            </div>
            {isDownloading && (
              <div className="download-controls">
                <button
                  type="button"
                  className="control-button"
                  onClick={handlePause}
                  disabled={isPausing || isStopping}
                >
                  {isPausing ? "Pausing…" : "Pause"}
                </button>
                <button
                  type="button"
                  className="control-button danger"
                  onClick={handleStop}
                  disabled={isPausing || isStopping}
                >
                  {isStopping ? "Stopping…" : "Stop"}
                </button>
              </div>
            )}
          </div>
        )}

        <p className={`notice ${notice.tone}`} role={notice.tone === "error" ? "alert" : "status"} aria-live="polite">
          {notice.message}
        </p>
      </section>
    </main>
  );
}

export default App;
