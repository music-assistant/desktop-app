# Music Assistant Desktop App Contributing Guide

Thank you for your interest in contributing to the Music Assistant Desktop App! This guide will provide you with the necessary information to get started.

## Background Information

The Music Assistant Desktop App is built with [Tauri](https://v2.tauri.app/). It wraps the Music Assistant frontend (hosted on your MA server) in a native webview, while providing additional native features like audio playback via the Sendspin protocol, system media controls, and Discord Rich Presence.

### Architecture

Below are some of the important files and folders used by the Desktop App:

```
desktop-app
├── app-icon.png - Application icon
├── music-assistant.desktop - Desktop file for Linux
├── package.json - Node.js dependencies and scripts
├── README.md
└── src-tauri - Tauri folder
    ├── build.rs - Build script
    ├── capabilities - Tauri capability definitions
    ├── Cargo.lock
    ├── Cargo.toml - Rust dependencies
    ├── Entitlements.plist - Entitlements file for macOS builds
    ├── icons - Application icons for various platforms
    ├── resources - HTML resources for settings/login pages
    ├── src - Tauri backend source code
    │   ├── lib.rs - Main Tauri application
    │   ├── main.rs - Application entry point
    │   ├── sendspin/ - Native Sendspin client for audio playback
    │   │   ├── mod.rs - Main Sendspin client implementation
    │   │   ├── devices.rs - Audio device enumeration
    │   │   └── protocol.rs - Sendspin protocol handling
    │   ├── media_controls.rs - OS media controls integration
    │   ├── now_playing.rs - Now-playing state management
    │   ├── discord_rpc.rs - Discord Rich Presence integration
    │   ├── mdns_discovery.rs - Server discovery via mDNS
    │   └── settings.rs - Settings management
    ├── target - Build output folder
    └── tauri.conf.json - Tauri configuration
```

### Key Features

The desktop app provides these native features on top of the web frontend:

- **Sendspin Audio Client**: Native bit-perfect audio playback via the Sendspin protocol with audio device selection
- **System Media Controls**: Integrates with macOS Control Center, Windows Media Controls, and Linux MPRIS
- **Discord Rich Presence**: Shows currently playing track on Discord
- **Server Discovery**: Automatic discovery of Music Assistant servers via mDNS
- **System Tray**: Background operation with tray icon and playback controls

### How It Works

1. On first launch, the app discovers or prompts the user to enter the URL of their Music Assistant server
2. The server URL and settings are stored locally for subsequent launches
3. The app opens a WebView pointing to the Music Assistant frontend served by the user's server
4. Native Sendspin client connects to the server for audio playback
5. Now-playing information is shared with media controls and Discord

## Prerequisites

Before you begin, please ensure that you have the following installed:

- [Rust](https://www.rust-lang.org/tools/install) (1.77.2 or later)
- Node.js
- Yarn or npm
- [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/)

## Getting Started

To contribute to the Music Assistant Desktop App, follow these steps:

1. Fork the repository on GitHub.
2. Clone your forked repository to your local machine: `git clone [github link]`
3. Install the project dependencies by running `yarn install` or `npm install`.
4. Start the development server by running `yarn dev` or `npm run dev`.

Note: You will need a running Music Assistant server to test the app.

## Code Quality

This project uses automated linting and formatting tools to maintain code quality. Pre-commit hooks are automatically installed when you run `yarn install`.

### Pre-commit Hooks

After running `yarn install`, git hooks are automatically set up via Husky. On every commit, the following checks run:

- **Prettier**: Auto-formats staged JS/HTML/JSON/MD files
- **Rustfmt**: Checks Rust code formatting
- **Clippy**: Lints Rust code (pedantic mode)

If any check fails, the commit will be blocked until the issues are fixed.

### Manual Linting and Formatting

You can also run linting and formatting manually:

```bash
# Run all linters
yarn lint

# Format all code
yarn format

# Rust only
yarn lint:rust      # Check Rust code with Clippy
yarn format:rust    # Format Rust code with rustfmt

# Frontend only (HTML/CSS/JS/JSON)
yarn lint:format    # Check formatting with Prettier
yarn format:prettier # Format with Prettier
```

### What Gets Checked

- **Rust code**: Formatted with `rustfmt`, linted with `clippy` (pedantic mode)
- **HTML/CSS/JS/JSON/MD**: Formatted with Prettier

## Making Changes

When making changes, please follow these guidelines:

- Create a new branch for your changes.
- Make your changes and ensure that the code compiles without errors.
- Run `yarn lint` to check for linting issues before committing.
- Commit your changes with a descriptive commit message.
- Push your changes to your forked repository.
- Submit a pull request to the main repository.

## Reporting Issues

If you encounter any issues or have suggestions for improvement, please open an issue on the GitHub repository. Provide a clear and detailed description of the problem or suggestion.
