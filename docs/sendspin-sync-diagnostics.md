# Sendspin clock synchronization diagnostics

This temporary branch investigates [issue #247](https://github.com/music-assistant/desktop-app/issues/247). It adds application-level timing logs without changing TCP socket options, clock synchronization thresholds, or playback settings. It is not a proposed fix. The exact released Sendspin library source is included under `src-tauri/vendor/sendspin` with a local Cargo patch so its internal timing can be logged from a single checkout.

## Build and run

Follow the platform prerequisites in [CONTRIBUTING.md](../CONTRIBUTING.md), including Tauri's Linux development libraries, Rust, Node.js, and Yarn. In a separate clone:

```sh
git clone --branch diagnostic/sendspin-sync-rtt https://github.com/music-assistant/desktop-app.git desktop-app-sync-diagnostics
cd desktop-app-sync-diagnostics
yarn install --frozen-lockfile
yarn build
```

The Linux AppImage, when produced by the build, is under `src-tauri/target/release/bundle/appimage/`. Alternatively, `yarn dev` runs a development build. Keep the build mode the same for both connection paths.

Quit your installed copy before launching the diagnostic build. This build uses the same application identity and saved settings as the installed app; a separate source clone does not isolate those settings. Do not run both copies together. Avoid installing the diagnostic build over your regular package, and do not accept an updater installation during the test.

## Collect comparable logs

1. Use the normal **Info** log level; the new diagnostics do not require Debug or Trace.
2. Connect using the direct LAN URL, start playback, and leave it running for at least 60 seconds, including a period of silence if playback fails.
3. Save the log using the tray's **Open log file** action. Note the time the test started and whether sound played.
4. Repeat through the proxied URL, with the same player, network connection, and build. Save that log separately. Restore your usual server URL afterward.
5. Include whether the desktop uses Ethernet or Wi-Fi, the proxy software, and whether the VPS forwards to a server at home. Say which host/address the comparison ping targeted. Please redact private addresses and unrelated personal information before sharing logs.

The added `[Sendspin diagnostics]` entries contain timing and socket settings, not authentication tokens or media metadata. Other existing app logs may contain server addresses, player identifiers, or metadata; review the entire log before posting it publicly.

## Interpretation

Timing messages are emitted for the first ten samples and then every thirtieth sample per connection. Repeated rejected samples also produce a rate-limited warning. Match send and receive entries by `t1` (the echoed client timestamp), not by log ordering: the writer and receiver run as separate tasks.

The socket entry reports the observed `TCP_NODELAY` setting without changing it. A completed WebSocket flush means the application handed data to the transport; it does **not** mean the remote server received it. Likewise, receipt of a whole WebSocket message is not a kernel packet-arrival timestamp. These logs help distinguish application queueing from the remaining transport/server path but cannot by themselves prove Nagle's algorithm or delayed ACK caused a delay. A packet capture may still be needed.
