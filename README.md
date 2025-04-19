# YouTube Audio Downloader

This is a Tauri-based desktop application for downloading audio from YouTube. It uses `yt-dlp` under the hood to handle the downloading process. The app is built with React, TypeScript, and Vite for the frontend, and Rust for the backend.

## Features

- Download individual YouTube videos or entire playlists as audio files
- Save audio files in MP3 format with high quality (320K)
- Select and save a custom download path
- Automatically installs `yt-dlp` if it is not already installed
- Cross-platform support (Windows, macOS, Linux)

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
npm run dev
```

This will start both the Vite development server and the Tauri backend. The app will be available at `http://localhost:1420`.

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







