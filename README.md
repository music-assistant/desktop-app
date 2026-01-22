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
    <a href="https://github.com/music-assistant/desktop-app/releases/latest"><img src="https://img.shields.io/github/v/release/music-assistant/desktop-app?label=Download&style=for-the-badge" alt="Download"></a>
  </p>
</p>

## Downloads

| Platform    | Architecture             | Download                                                                                                                                                                                                                        |
| ----------- | ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Windows** | x64                      | [.msi installer](https://github.com/music-assistant/desktop-app/releases/latest) \| [.exe installer](https://github.com/music-assistant/desktop-app/releases/latest)                                                            |
| **macOS**   | Apple Silicon (M1/M2/M3) | [.dmg](https://github.com/music-assistant/desktop-app/releases/latest)                                                                                                                                                          |
| **macOS**   | Intel                    | [.dmg](https://github.com/music-assistant/desktop-app/releases/latest)                                                                                                                                                          |
| **Linux**   | x64                      | [.deb](https://github.com/music-assistant/desktop-app/releases/latest) \| [.AppImage](https://github.com/music-assistant/desktop-app/releases/latest) \| [.rpm](https://github.com/music-assistant/desktop-app/releases/latest) |
| **Linux**   | ARM64                    | [.deb](https://github.com/music-assistant/desktop-app/releases/latest) \| [.AppImage](https://github.com/music-assistant/desktop-app/releases/latest) \| [.rpm](https://github.com/music-assistant/desktop-app/releases/latest) |

> All downloads available on the [Releases page](https://github.com/music-assistant/desktop-app/releases/latest)

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

Download and run the `.msi` or `.exe` installer from the downloads table above.

### macOS

Download the `.dmg` file for your architecture (Apple Silicon for M1/M2/M3 Macs, Intel for older Macs).

<!--
Or install via Homebrew:
```bash
brew tap music-assistant/tap
brew install music-assistant
```
-->

### Linux

**Debian/Ubuntu:** Download and install the `.deb` package:

```bash
sudo dpkg -i Music.Assistant_*_amd64.deb
```

**Fedora/RHEL:** Download and install the `.rpm` package:

```bash
sudo rpm -i Music.Assistant-*-1.x86_64.rpm
```

**Other distributions:** Download the `.AppImage`, make it executable, and run:

```bash
chmod +x Music.Assistant_*.AppImage
./Music.Assistant_*.AppImage
```

## Development & Contributing

Check the [CONTRIBUTING.md](CONTRIBUTING.md) file.

## License

[Apache-2.0](LICENSE)

---

[![A project from the Open Home Foundation](https://www.openhomefoundation.org/badges/ohf-project.png)](https://www.openhomefoundation.org/)
