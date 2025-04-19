# YouTube Audio Downloader

This is a Tauri-based desktop application for downloading audio from YouTube. It uses `yt-dlp` under the hood to handle the downloading process. The app is built with React, TypeScript, and Vite for the frontend, and Rust for the backend.

## Features

- Download individual YouTube videos or entire playlists as audio files.
- Save audio files in MP3 format with high quality (320K).
- Select and save a custom download path.
- Automatically installs `yt-dlp` if it is not already installed.

## Prerequisites

Before setting up the project, ensure you have the following installed:

- [Node.js](https://nodejs.org/) (version 16 or higher)
- [Rust](https://www.rust-lang.org/tools/install) (with `cargo` package manager)
- [Tauri prerequisites](https://tauri.app/v1/guides/getting-started/prerequisites) (varies by operating system)

## Setup

1. Clone the repository:
   ```sh
   git clone <repository-url>
   cd ytdownloader

2. Install the Node.js dependencies: npm install

3. Install Rust dependencies (if not already installed): rustup update

4. Ensure yt-dlp is installed. The app will attempt to install it automatically if it is missing.

Development
To run the app in development mode:

1. Start the development server: npm run dev
2. This will start the Vite development server and the Tauri backend. The app will be available at http://localhost:1420.

Build
To build the app for production:

1. Run the build command: npm run build
2. This will create a production-ready build of the app in the dist directory.
3. To package the app as a desktop application, run: npm run tauri build

The packaged app will be available in the src-tauri/target/release directory.

Running the App
After building the app, you can run it directly:

1. Navigate to the release directory: cd src-tauri/target/release

2. Run the executable: ./ytdownloader

Recommended IDE Setup
VS Code with the following extensions:
Tauri
Rust Analyzer
License
This project is licensed under the MIT License.







