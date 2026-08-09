# YouTube & Podcast Audio Downloader

This is a Tauri-based desktop application for downloading audio from YouTube and public podcast RSS feeds. It uses `yt-dlp` under the hood to handle downloads. The app is built with React, TypeScript, and Vite for the frontend, and Rust for the backend.

## Features

- Download individual YouTube videos or entire playlists as audio files
- Download every available episode from a public podcast RSS feed
- Save audio files in MP3 format with high quality (320K)
- Select and save a custom download path
- Live progress bar with item counts for playlist and podcast downloads
- Pause and resume long downloads, or stop them outright, without freezing the UI
- Cross-platform support (Windows, macOS, Linux)

## Downloading a podcast RSS feed

1. Choose **Podcast RSS feed** in the Download section.
2. Paste a public `http` or `https` RSS feed URL.
3. Choose the parent download destination and select **Download podcast**.

The app asks `yt-dlp` to verify that the URL is an episode playlist, derives a safe folder name from the podcast title, and creates that folder inside the selected destination. All available episodes are saved there. A progress bar shows how many episodes are finished and how many remain, along with the current episode's percent complete.

Feeds that require authentication, are not recognized by `yt-dlp` as a playlist, or contain no downloadable episodes are rejected before the full download begins. Very large feeds can take a long time; the download now runs off the UI thread so the app stays responsive throughout.

## Progress, pause, and stop

Playlist and podcast downloads run in the background and report progress (current/total items and percent) as `yt-dlp` processes each item, so the app never locks up on large feeds or playlists.

- **Pause** stops the current `yt-dlp` process but remembers which items already finished (via a per-source download archive), so **Resume** picks up where it left off instead of re-downloading everything.
- **Stop** ends the download immediately and clears its saved progress, so a future download of the same source starts fresh.

## Prerequisites

Before setting up the project, ensure you have the following installed:

- [Node.js](https://nodejs.org/) (version 16 or higher)
- [Rust](https://www.rust-lang.org/tools/install) (with `cargo` package manager)
- [Tauri prerequisites](https://tauri.app/v1/guides/getting-started/prerequisites) (varies by operating system)
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp#installation), available on your system `PATH`

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




