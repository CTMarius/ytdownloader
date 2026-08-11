import { FormEvent, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

type DownloadType = "single" | "playlist" | "podcast";
type NoticeTone = "neutral" | "success" | "error";

interface RuntimeSetupStatus {
  ready: boolean;
  message: string;
  ytDlpVersion: string | null;
}

interface RuntimeSetupProgress {
  current: number;
  total: number;
  component: "yt-dlp" | "ffmpeg" | "ffprobe";
  message: string;
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
  jobId: string;
  requestId: string;
  completed: number;
  total: number;
  active: number;
  activeItem: number | null;
  percent: string | null;
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

function createDownloadRequestId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }

  return `request-${Date.now()}-${Math.random().toString(36).slice(2)}`;
}

function SetupScreen({
  phase,
  progress,
  error,
  onRetry,
}: {
  phase: "checking" | "installing" | "error";
  progress: RuntimeSetupProgress | null;
  error: string | null;
  onRetry: () => void;
}) {
  const isInstalling = phase === "installing";
  const step = progress?.current ?? 0;
  const total = progress?.total ?? 3;
  const progressPercent = isInstalling ? Math.round((step / total) * 100) : 0;

  return (
    <main className="app-shell">
      <section className="setup-card" aria-labelledby="setup-title">
        <div className="brand-mark" aria-hidden="true">
          <svg viewBox="0 0 24 24" focusable="false">
            <path d="M9.5 8.5 16 12l-6.5 3.5v-7Z" />
            <path d="M3.5 12c0-3.6.4-5.8 1.5-6.9C6.1 4 8.4 3.5 12 3.5s5.9.5 7 1.6c1.1 1.1 1.5 3.3 1.5 6.9s-.4 5.8-1.5 6.9c-1.1 1.1-3.4 1.6-7 1.6s-5.9-.5-7-1.6C3.9 17.8 3.5 15.6 3.5 12Z" />
          </svg>
        </div>
        <p className="eyebrow">First-run setup</p>
        <h1 id="setup-title">Preparing audio tools</h1>
        <p className="setup-intro">
          YTDownloader uses private copies of yt-dlp, ffmpeg, and ffprobe. They are stored in
          this app’s data folder and do not change your system PATH.
        </p>

        {phase !== "error" && (
          <div className="setup-progress" role="status" aria-live="polite">
            <div className="progress-meta">
              <span>
                {isInstalling
                  ? progress?.message ?? "Preparing secure downloads…"
                  : "Checking installed audio tools…"}
              </span>
              {isInstalling && <strong>{step > 0 ? `${step} of ${total}` : ""}</strong>}
            </div>
            <div className="progress-bar-track">
              <div className="progress-bar-fill" style={{ width: `${progressPercent}%` }} />
            </div>
            <p className="help-text">
              {isInstalling
                ? "Each download is version-pinned and verified before it can be used."
                : "This only takes a moment when the tools are already installed."}
            </p>
          </div>
        )}

        {phase === "error" && (
          <div className="setup-error" role="alert">
            <strong>Setup could not finish.</strong>
            <span>{error}</span>
          </div>
        )}

        {phase === "error" && (
          <button type="button" className="download-button" onClick={onRetry}>
            Retry setup
          </button>
        )}
      </section>
    </main>
  );
}

function App() {
  const [url, setUrl] = useState("");
  const [downloadPath, setDownloadPath] = useState("");
  const [downloadType, setDownloadType] = useState<DownloadType>("single");
  const [setupPhase, setSetupPhase] = useState<"checking" | "installing" | "ready" | "error">(
    "checking",
  );
  const [setupAttempt, setSetupAttempt] = useState(0);
  const [setupProgress, setSetupProgress] = useState<RuntimeSetupProgress | null>(null);
  const [setupError, setSetupError] = useState<string | null>(null);
  const [isLoadingPath, setIsLoadingPath] = useState(true);
  const [isSelectingPath, setIsSelectingPath] = useState(false);
  const [isDownloading, setIsDownloading] = useState(false);
  const [isPaused, setIsPaused] = useState(false);
  const [isPausing, setIsPausing] = useState(false);
  const [isStopping, setIsStopping] = useState(false);
  const [progress, setProgress] = useState<DownloadProgress | null>(null);
  const [resumeParams, setResumeParams] = useState<StartedDownloadParams | null>(null);
  const activeJobIdRef = useRef<string | null>(null);
  const activeRequestIdRef = useRef<string | null>(null);
  const acceptsProgressRef = useRef(false);
  const [toolStatus, setToolStatus] = useState<Notice>({
    message: "Preparing private audio tools…",
    tone: "neutral",
  });
  const [notice, setNotice] = useState<Notice>({
    message: "Choose a YouTube link or podcast feed and a destination to get started.",
    tone: "neutral",
  });

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;

    void listen<RuntimeSetupProgress>("runtime-setup-progress", (event) => {
      setSetupProgress(event.payload);
    }).then((stopListening) => {
      if (disposed) {
        stopListening();
      } else {
        unlisten = stopListening;
      }
    }).catch(() => {
      // The setup command still reports completion or failure if progress events are unavailable.
    });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    let cancelled = false;

    const prepareRuntime = async () => {
      setSetupPhase("checking");
      setSetupError(null);
      setSetupProgress(null);

      try {
        let status = await invoke<RuntimeSetupStatus>("get_runtime_setup_status");
        if (!status.ready) {
          if (!cancelled) {
            setSetupPhase("installing");
          }
          status = await invoke<RuntimeSetupStatus>("setup_runtime_dependencies");
        }

        if (cancelled) {
          return;
        }

        if (!status.ready) {
          throw new Error(status.message);
        }

        setToolStatus({
          message: status.ytDlpVersion
            ? `yt-dlp ${status.ytDlpVersion} is ready with private ffmpeg and ffprobe.`
            : "Private yt-dlp, ffmpeg, and ffprobe tools are ready.",
          tone: "success",
        });
        setSetupPhase("ready");
      } catch (error) {
        if (!cancelled) {
          setSetupError(errorMessage(error));
          setSetupPhase("error");
        }
      }
    };

    void prepareRuntime();

    return () => {
      cancelled = true;
    };
  }, [setupAttempt]);

  useEffect(() => {
    let cancelled = false;

    const loadDownloadPath = async () => {
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
          setIsLoadingPath(false);
        }
      }
    };

    void loadDownloadPath();

    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;

    void listen<DownloadProgress>("download-progress", (event) => {
      const nextProgress = event.payload;
      if (
        !acceptsProgressRef.current ||
        nextProgress.total <= 0 ||
        nextProgress.requestId !== activeRequestIdRef.current
      ) {
        return;
      }

      if (activeJobIdRef.current && activeJobIdRef.current !== nextProgress.jobId) {
        return;
      }

      activeJobIdRef.current ??= nextProgress.jobId;
      setProgress((currentProgress) => {
        if (
          currentProgress?.jobId === nextProgress.jobId &&
          nextProgress.completed < currentProgress.completed
        ) {
          return currentProgress;
        }
        return nextProgress;
      });
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
      resetPauseStateIfNeeded();
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
    const requestId = createDownloadRequestId();

    setIsDownloading(true);
    setIsPaused(false);
    setProgress(null);
    setResumeParams(params);
    activeJobIdRef.current = null;
    activeRequestIdRef.current = requestId;
    acceptsProgressRef.current = true;
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
              requestId,
            })
          : await invoke<DownloadResult>("download_audio", {
              url: startUrl,
              downloadType: startType,
              path: startPath,
              requestId,
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
      if (activeRequestIdRef.current === requestId) {
        acceptsProgressRef.current = false;
        activeJobIdRef.current = null;
        activeRequestIdRef.current = null;
      }
      setIsDownloading(false);
    }
  };

  const resetPauseStateIfNeeded = () => {
    if (isPaused) {
      setIsPaused(false);
      setResumeParams(null);
      setProgress(null);
      acceptsProgressRef.current = false;
      activeJobIdRef.current = null;
      activeRequestIdRef.current = null;
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
    setNotice({
      message: "Pausing all active download workers…",
      tone: "neutral",
    });
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
    setNotice({
      message: "Stopping all active download workers…",
      tone: "neutral",
    });
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

  const downloadUnavailable = isLoadingPath || isSelectingPath || isDownloading;
  const isPodcast = downloadType === "podcast";

  if (setupPhase !== "ready") {
    return (
      <SetupScreen
        phase={setupPhase}
        progress={setupProgress}
        error={setupError}
        onRetry={() => {
          setSetupAttempt((attempt) => attempt + 1);
        }}
      />
    );
  }

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
                disabled={isLoadingPath || isDownloading || isSelectingPath}
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
                  ? `${progress.completed} of ${progress.total} ${
                      progress.kind === "podcast" ? "episodes" : "items"
                    } downloaded${progress.active > 0 ? ` · ${progress.active} worker${progress.active === 1 ? "" : "s"} active` : ""}`
                  : "Preparing download…"}
              </span>
              <strong>
                {progress?.percent ??
                  (progress?.activeItem ? `Item ${progress.activeItem}` : "")}
              </strong>
            </div>
            <div className="progress-bar-track">
              <div
                className="progress-bar-fill"
                style={{
                  width: progress
                    ? `${Math.min(100, Math.round((progress.completed / progress.total) * 100))}%`
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
