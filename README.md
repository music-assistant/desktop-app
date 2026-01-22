<p align="center">
  <p align="center">
   <img width="150" height="150" src="app-icon.png" alt="Logo">
  </p>
	<h1 align="center"><b>Music Assistant Desktop Companion App</b></h1>
	<p align="center">
		A native desktop companion app for Music Assistant
    <br />
    <a href="https://music-assistant.io/"><strong>Music Assistant »</strong></a>
    <br />
    <br />
    <b>Download for </b>
    macOS (<a href="https://github.com/music-assistant/desktop-app/releases/latest">Apple Silicon</a> |
    <a href="https://github.com/music-assistant/desktop-app/releases/latest">Intel</a>) ·
		<a href="https://github.com/music-assistant/desktop-app/releases/latest">Windows</a> ·
    Linux (<a href="https://github.com/music-assistant/desktop-app/releases/latest">Debian</a> | <a href="https://github.com/music-assistant/desktop-app/releases/latest">AppImage</a>)
  </p>
</p>

## Features

- **Native Audio Playback** - High-quality audio output via Sendspin protocol with device selection
- **System Tray Integration** - Control playback and see what's playing from the system tray
- **OS Media Controls** - Integrates with macOS Control Center, Windows Media Controls, and Linux MPRIS
- **Discord Rich Presence** - Show what you're listening to on Discord
- **Server Discovery** - Automatic discovery of Music Assistant servers via mDNS

## Architecture

The companion app wraps the Music Assistant frontend (hosted on your MA server) in a native webview, while providing some additional native features:

- Native Sendspin client for bit-perfect audio playback
- System-level media controls and Now Playing integration
- Background operation with tray icon
- Auto-start on system boot

## Installation

### Windows

Download the .msi installer from the [releases](https://github.com/music-assistant/desktop-app/releases/latest/).

### macOS

Download the .dmg from the [releases](https://github.com/music-assistant/desktop-app/releases/latest/).

Or install via Homebrew: `brew install music-assistant/tap/companion`

### Debian / Ubuntu

Download the .deb from the [releases](https://github.com/music-assistant/desktop-app/releases/latest/).

### Other Linux

Download the AppImage from the [releases](https://github.com/music-assistant/desktop-app/releases/latest/).

## Development

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (1.77.2 or later)
- [Node.js](https://nodejs.org/) (for Tauri CLI)
- Platform-specific dependencies (see [Tauri Prerequisites](https://tauri.app/v1/guides/getting-started/prerequisites))

### Setup

```bash
# Clone the repository
git clone https://github.com/music-assistant/desktop-app
cd desktop-app

# Install dependencies
yarn install

# Run in development mode
yarn dev

# Build for production
yarn build
```

### Project Structure

```
├── src-tauri/
│   ├── src/
│   │   ├── lib.rs              # Main Tauri application
│   │   ├── sendspin/           # Native Sendspin client
│   │   ├── media_controls.rs   # OS media controls integration
│   │   ├── now_playing.rs      # Now-playing state management
│   │   ├── discord_rpc.rs      # Discord Rich Presence
│   │   ├── mdns_discovery.rs   # Server discovery
│   │   └── settings.rs         # Settings management
│   ├── resources/              # HTML resources for settings/login
│   ├── icons/                  # App icons
│   └── Cargo.toml              # Rust dependencies
└── package.json                # Node.js dependencies (Tauri CLI)
```

## Contributing

Check the [CONTRIBUTING.md](CONTRIBUTING.md) file.

## License

[Apache-2.0](LICENSE)

---

[![A project from the Open Home Foundation](https://www.openhomefoundation.org/badges/ohf-project.png)](https://www.openhomefoundation.org/)
