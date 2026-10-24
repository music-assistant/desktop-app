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

Check that the committed source list is current:

```bash
packaging/flatpak/generate-cargo-sources.sh --check
```

Then build and run:

```bash
flatpak-builder --force-clean build-dir packaging/flatpak/io.music_assistant.Companion.yml
flatpak-builder --run build-dir packaging/flatpak/io.music_assistant.Companion.yml music-assistant-companion
```

To create a single-file bundle for local installation/testing:

```bash
flatpak-builder --force-clean --repo=repo build-dir packaging/flatpak/io.music_assistant.Companion.yml
flatpak build-bundle repo Music.Assistant_x86_64.flatpak io.music_assistant.Companion
```

Release CI names the bundle to match the other release assets:
`Music.Assistant_<version>_<arch>.flatpak` (see the `Create Flatpak bundle` step
in `.github/workflows/build-packages.yml`). The bundle filename is cosmetic —
the installed application ID is always `io.music_assistant.Companion`.

## Private certificate authorities

Native packages use the operating system's certificate trust. In Flatpak, the
native HTTP client reads the sandbox's CA bundle, which may not include private
CAs installed on the host. A certificate working in a host browser does not prove
that it is trusted inside the sandbox.

For a server using a private CA, first install that CA through your distribution's
normal trust-management tools. You can then opt in to using the host's generated
CA bundle for one app launch. On Debian/Ubuntu:

```bash
flatpak run --filesystem=host-etc:ro \
  --env=SSL_CERT_FILE=/run/host/etc/ssl/certs/ca-certificates.crt \
  io.music_assistant.Companion
```

This grants the app read-only access to **all of the host's `/etc`**, not just the
certificate bundle. It is not enabled by default. These launch arguments are
temporary: quit the app and launch normally to stop using them. Quit any running
instance before testing a different trust configuration.

The bundle path depends on the distribution. Fedora/RHEL commonly use
`/run/host/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem`. Select an existing
regular bundle file; an absolute symlink may resolve to the runtime's certificate
store instead. `SSL_CERT_FILE` replaces the native HTTP client's default root
source rather than adding to it, so use the complete system bundle, not just a
single private CA. A missing bundle causes certificate verification to fail.
Certificate expiry and hostname checks remain enabled.

The Debian/Ubuntu launch configuration has been tested with the native HTTP
client inside the installed app sandbox. The webview uses a separate TLS stack;
this setting does not guarantee identical trust handling in every component.

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
- The manifest grants both Wayland and X11 access. The app keeps its X11
  workaround on GNOME unless `GDK_BACKEND` is explicitly set; other desktops
  use GTK's backend selection. X11 remains available for that workaround and
  explicit overrides. Do not replace it with `fallback-x11`, which withholds
  X11 when Wayland is available.
- To explicitly test native Wayland, run
  `flatpak run --env=GDK_BACKEND=wayland io.music_assistant.Companion`.
  To select X11 instead, use `--env=GDK_BACKEND=x11`. Quit the existing app
  first so single-instance handling does not reuse a process on another backend.
- MPRIS owns `org.mpris.MediaPlayer2.io_music_assistant_companion.instance<PID>`.
  The manifest grants the transformed MPRIS namespace explicitly because the
  MPRIS name uses the D-Bus-safe underscore form of the Tauri identifier.
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
