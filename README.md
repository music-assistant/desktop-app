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

## About Music Assistant

Music Assistant is a free, opensource Media library manager that connects to your streaming services and a wide range of connected speakers. The server is the beating heart, the core of Music Assistant and must run on an always-on device like a Raspberry Pi, a NAS or an Intel NUC or alike. The desktop app discovers running Music Assistant servers running on your network and allows you to connect to one, basically wrapping the frontend into this app and provides a couple of native features, such as a sendspin player and discord rich presence. It will sit in your system tray, ready to control playback or show the interface.

### Documentation and support

[Documentation](https://music-assistant.io)

[Beta Documentation](https://beta.music-assistant.io)

For issues, please go to [the issue tracker](https://github.com/music-assistant/support/issues).

For feature requests, please see [feature requests](https://github.com/music-assistant/support/discussions/categories/feature-requests-and-ideas).

---

### Downloads

| Platform    | Architecture             | Download                                                                                                                                                                                                                                                                                                      |
| ----------- | ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Windows** | x64                      | [.msi installer](https://github.com/music-assistant/desktop-app/releases/latest) \| [.exe installer](https://github.com/music-assistant/desktop-app/releases/latest)                                                                                                                                          |
| **macOS**   | Apple Silicon (M1/M2/M3) | [.dmg](https://github.com/music-assistant/desktop-app/releases/latest)                                                                                                                                                                                                                                        |
| **macOS**   | Intel                    | [.dmg](https://github.com/music-assistant/desktop-app/releases/latest)                                                                                                                                                                                                                                        |
| **Linux**   | x64                      | [.deb](https://github.com/music-assistant/desktop-app/releases/latest) \| [.AppImage](https://github.com/music-assistant/desktop-app/releases/latest) \| [.rpm](https://github.com/music-assistant/desktop-app/releases/latest) \| [.flatpak](https://github.com/music-assistant/desktop-app/releases/latest) |
| **Linux**   | ARM64                    | [.deb](https://github.com/music-assistant/desktop-app/releases/latest) \| [.AppImage](https://github.com/music-assistant/desktop-app/releases/latest) \| [.rpm](https://github.com/music-assistant/desktop-app/releases/latest) \| [.flatpak](https://github.com/music-assistant/desktop-app/releases/latest) |

> All downloads available on the [Releases page](https://github.com/music-assistant/desktop-app/releases/latest)

### Features

- **Native Audio Playback** - High-quality audio output via Sendspin protocol with device selection
- **System Tray Integration** - Control playback and see what's playing from the system tray
- **OS Media Controls** - Integrates with macOS Control Center, Windows Media Controls, and Linux MPRIS
- **Discord Rich Presence** - Show what you're listening to on Discord
- **Server Discovery** - Automatic discovery of Music Assistant servers via mDNS

### Architecture

The companion app wraps the Music Assistant frontend (hosted on your MA server) in a native webview, while providing some additional native features:

- Native Sendspin client for bit-perfect audio playback
- System-level media controls and Now Playing integration
- Background operation with tray icon
- Auto-start on system boot

### Installation

#### Windows

Download and run the `.msi` or `.exe` installer from the downloads table above.

#### macOS

Download the `.dmg` file for your architecture (Apple Silicon for M1/M2/M3 Macs, Intel for older Macs).

Or install via Homebrew:

```bash
brew tap music-assistant/tap
brew install music-assistant
```

#### Linux

**Debian/Ubuntu:** Download and install the `.deb` package:

```bash
sudo dpkg -i Music.Assistant_*_amd64.deb
```

**Fedora/RHEL:** Download and install the `.rpm` package:

```bash
sudo rpm -i Music.Assistant-*-1.x86_64.rpm
```

**Flatpak:** Download and install the `.flatpak` bundle for your architecture:

```bash
flatpak install Music.Assistant_*_x86_64.flatpak
flatpak run io.music_assistant.Companion
```

> Installing the bundle pulls in the GNOME runtime, so make sure the [Flathub remote](https://flathub.org/setup) is configured first.

**Arch Linux:** Available on the [AUR](https://aur.archlinux.org/), e.g. with an AUR helper:

```bash
yay -S music-assistant-desktop
```

- [music-assistant-desktop](https://aur.archlinux.org/packages/music-assistant-desktop) - builds from the latest release
- [music-assistant-desktop-bin](https://aur.archlinux.org/packages/music-assistant-desktop-bin) - prebuilt binary from the latest release
- [music-assistant-desktop-git](https://aur.archlinux.org/packages/music-assistant-desktop-git) - builds from the latest git commit

These are maintained by MA community member [Raggi](https://github.com/raggi), which is greatly appreciated.

**Other distributions:** Download the `.AppImage`, make it executable, and run:

```bash
chmod +x Music.Assistant_*.AppImage
./Music.Assistant_*.AppImage
```

### Troubleshooting

If the app does not connect, playback controls stop responding, or the app crashes, please include logs when opening an [issue](https://github.com/music-assistant/desktop-app/issues/new/choose).

#### Enable debug logging

1. Open **Settings** from the Music Assistant tray/menubar icon.
2. Enable **Debug logging**.
3. Reproduce the problem.
4. Use the tray/menubar menu item **Open log file** to open the current log.
5. Attach the relevant log output to your bug report.

##### If the issue relates to audio quality

- Enable **Trace logging** by enabling debug logging and then trace in the same settings window.

#### Linux crash diagnostics

For Linux crashes, especially AppImage crashes that only print `Segmentation fault` in the terminal, run the app from a terminal with extra diagnostics enabled:

```bash
RUST_BACKTRACE=1 G_MESSAGES_DEBUG=all WEBKIT_DEBUG=all ./Music.Assistant_*.AppImage
```

Then copy the full terminal output into the issue. If your distribution uses `systemd-coredump`, also include the core dump summary:

```bash
coredumpctl info music-assistant-companion
```

For deeper native crashes, you can collect a backtrace with `gdb`:

```bash
coredumpctl gdb music-assistant-companion
```

Inside `gdb`, run:

```gdb
bt full
info sharedlibrary
```

If the AppImage crashes but the `.deb` or `.rpm` package works on the same machine, please mention that. It helps identify AppImage-specific GTK/WebKit/GLib library issues.

### Development & Contributing

Check the [CONTRIBUTING.md](CONTRIBUTING.md) file.

### License

[Apache-2.0](LICENSE)

---

[![A project from the Open Home Foundation](https://www.openhomefoundation.org/badges/ohf-project.png)](https://www.openhomefoundation.org/)
