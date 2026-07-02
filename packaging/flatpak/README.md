# Flatpak packaging

This directory contains a local, source-based Flatpak build for the Music Assistant
Desktop Companion app.

## Build locally

Install Flatpak tooling, the GNOME runtime/SDK, and the Rust SDK extension for the
manifest runtime branch:

```bash
flatpak install flathub org.gnome.Platform//49 org.gnome.Sdk//49 \
  org.freedesktop.Sdk.Extension.rust-stable//25.08
```

Generate/update the Cargo dependency source list whenever `src-tauri/Cargo.lock`
changes:

```bash
packaging/flatpak/generate-cargo-sources.sh
```

Then build and run:

```bash
flatpak-builder --force-clean build-dir packaging/flatpak/io.music_assistant.Companion.yml
flatpak-builder --run build-dir packaging/flatpak/io.music_assistant.Companion.yml music-assistant-companion
```

To create a single-file bundle for local installation/testing:

```bash
flatpak-builder --force-clean --repo=repo build-dir packaging/flatpak/io.music_assistant.Companion.yml
flatpak build-bundle repo music-assistant-companion.flatpak io.music_assistant.Companion
```

## Notes

- The build uses the checked-in static Tauri frontend resources under
  `src-tauri/resources`; it does not run Yarn/Node inside Flatpak.
- Cargo dependencies are vendored through `cargo-sources.json`, generated from
  `src-tauri/Cargo.lock` using Flatpak's cargo generator.
- The Cargo crate name is `music-assistant`, but Tauri's configured main binary
  name is `music-assistant-companion`. The manifest installs the compiled Cargo
  binary under the Tauri binary name.
- Tauri resolves Linux resources relative to the executable at
  `../lib/<package-info-name>`. Because `productName` is `Music Assistant`, the
  manifest installs resources under `/app/lib/Music Assistant/resources`.
- The current Linux app code forces `GDK_BACKEND=x11` for tray stability, so the
  manifest grants X11 rather than Wayland-only access.
- MPRIS currently owns `org.mpris.MediaPlayer2.music_assistant.*`; the manifest
  grants that name explicitly. If the app later changes its MPRIS name to
  `org.mpris.MediaPlayer2.io.music_assistant.Companion`, the explicit
  `--own-name` can be removed because Flatpak permits that pattern by default.
- The Tauri single-instance plugin uses the configured identifier
  `io.music-assistant.companion`, transformed to the D-Bus name
  `org.io_music_assistant_companion.SingleInstance`, so the manifest grants
  ownership of that name explicitly. This is the only single-instance mechanism
  used by the Flatpak build; `own` is sufficient and should not be paired with a
  duplicate lower `talk` policy for the same name.
- Flatpak autostart writes a host XDG autostart launcher at
  `~/.config/autostart/io.music_assistant.Companion.desktop` with command line
  `flatpak run io.music_assistant.Companion`. The manifest grants only
  `xdg-config/autostart:create` for this purpose, and the generated launcher
  includes `X-GNOME-Autostart-enabled=true` for GNOME sessions.
