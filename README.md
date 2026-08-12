# YouTube & Podcast Audio Downloader

This is a Tauri-based desktop application for downloading audio from YouTube and public podcast RSS feeds. It uses `yt-dlp` under the hood to handle downloads. The app is built with React, TypeScript, and Vite for the frontend, and Rust for the backend.

## Features

- Download individual YouTube videos or entire playlists as audio files
- Download every available episode from a public podcast RSS feed
- Save audio files in MP3 format with high quality (320K)
- Select and save a custom download path and concurrent-worker preference
- Live progress bar with item counts for playlist and podcast downloads
- Pause and resume long downloads, or stop them outright, without freezing the UI
- Cross-platform support (Windows, macOS, Linux)
- First-run runtime setup for private, verified copies of `yt-dlp`, `ffmpeg`, and `ffprobe`

## First-run runtime setup

On first launch, the app blocks downloads while it downloads the audio tools it needs. The tools
are stored only in this application's per-user app-data directory; YTDownloader never modifies
the system or user `PATH`, and every tool invocation uses those private copies explicitly.

Setup requires an internet connection and shows the current component as it downloads. Downloads
are HTTPS-only, version-pinned, and SHA-256 checked before installation. If a download, checksum,
or local validation fails, setup remains on the error screen and can be retried safely.

The current runtime supports:

- Windows x64
- Linux x64 and ARM64
- macOS Intel and Apple silicon

Other platform/architecture combinations show an unsupported-platform error instead of falling
back to a tool found on `PATH`.

The pinned artifacts are `yt-dlp` `2026.07.04` from the
[yt-dlp releases](https://github.com/yt-dlp/yt-dlp/releases) and `ffmpeg`/`ffprobe` `b6.1.1`
from [ffmpeg-static releases](https://github.com/eugeneware/ffmpeg-static/releases). Their
expected SHA-256 digests are part of the native application and are verified during setup.

## Downloading a podcast RSS feed

1. Choose **Podcast RSS feed** in the Download section.
2. Paste a public `http` or `https` RSS feed URL.
3. Choose the parent download destination and select **Download podcast**.

The app asks `yt-dlp` to verify that the URL is an episode playlist, derives a safe folder name from the podcast title, and creates that folder inside the selected destination. All available episodes are saved there. A progress bar shows how many episodes are finished and how many remain, along with the current episode's percent complete.

Feeds that require authentication, are not recognized by `yt-dlp` as a playlist, or contain no downloadable episodes are rejected before the full download begins. Very large feeds can take a long time; the download now runs off the UI thread so the app stays responsive throughout.

## Progress, pause, and stop

Choose **1–8 concurrent workers** in the app (the default is **4**) to control how many playlist
videos or podcast episodes download at once. More workers can finish large collections sooner, but
use more network bandwidth and CPU. Single-video downloads always use one worker. The preference is
saved with the download destination and applies to new playlist and podcast jobs; a paused job keeps
the worker count it started with when resumed.

The app still runs only one playlist, podcast, or single-video job at a time, and reports aggregate
completed/total progress plus active workers so the app never locks up on large feeds or playlists.

- **Pause** stops every active worker and remembers checkpointed items, so **Resume** picks up where it left off instead of re-downloading completed items.
- **Stop** waits for every active worker to exit, then clears saved progress so a future download of the same source starts fresh.

## Prerequisites

Before setting up the project, ensure you have the following installed:

- [Node.js](https://nodejs.org/) (version 16 or higher)
- [Rust](https://www.rust-lang.org/tools/install) (with `cargo` package manager)
- [Tauri prerequisites](https://tauri.app/v1/guides/getting-started/prerequisites) (varies by operating system)

## Setup

1. Clone the repository:
   ```sh
   git clone https://github.com/yourusername/ytdownloader.git
   cd ytdownloader
   ```

2. Install the Node.js dependencies:
   ```sh
   npm install
   ```

3. Install Rust dependencies:
   ```sh
   rustup update
   ```

## Development

Run the app in development mode:
```sh
npm run tauri dev
```

This starts the Vite development server and opens the Tauri desktop application. Use
`npm run dev` only when you need the frontend development server at
`http://localhost:1420`.

## Building for Production

1. Create a production build:
   ```sh
   npm run build
   ```

2. Package the app:
   ```sh
   npm run tauri build
   ```

The packaged application will be available in:
- Linux: `src-tauri/target/release/ytdownloader`
- Windows: `src-tauri/target/release/ytdownloader.exe`
- macOS: `src-tauri/target/release/ytdownloader.app`

## Development Environment

### Recommended IDE Setup
- [Visual Studio Code](https://code.visualstudio.com/) with extensions:
  - [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode)
  - [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
  - [TypeScript and JavaScript](https://marketplace.visualstudio.com/items?itemName=ms-vscode.vscode-typescript-next)

### Project Structure
```
ytdownloader/
├── src/                 # React frontend source
├── src-tauri/          # Rust backend source
├── public/             # Static assets
└── package.json        # Project configuration
```

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Contributing

1. Fork the repository
2. Create a new branch
3. Make your changes
4. Submit a pull request

## Support

If you encounter any issues or have questions, please file an issue on the GitHub repository.

