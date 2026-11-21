# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.8](https://github.com/Sendspin/sendspin-rs/compare/v0.3.7...v0.3.8) - 2026-10-15

### Fixed

- *(audio)* support unsigned 24-bit output ([#107](https://github.com/Sendspin/sendspin-rs/pull/107))
- hold sync corrections while a cold output stream's delay settles ([#103](https://github.com/Sendspin/sendspin-rs/pull/103))

### Other

- cover startup synchronization correction transitions ([#106](https://github.com/Sendspin/sendspin-rs/pull/106))

### Fixed

- Stop a freshly opened output stream from being corrected at up to 4% speed
  for several seconds after playback starts. On some backends (observed with
  ALSA over PulseAudio/PipeWire) the reported output delay of a cold stream
  overshoots by a few hundred milliseconds and then relaxes while the audio
  plays gaplessly, and the sync error moves with it. Corrections are now held
  back until 500ms observation-window floors stay within a 10ms range over
  a baseline window and two seconds of comparisons, independent of callback
  size (capped at 10s after playback starts). Speed corrections are limited to
  0.5% for the first 20s after a start. Callback gaps of 500ms or more discard
  settling evidence. The all-time delta floor used for reanchors is re-seeded
  from the observed floor each time the delay settles.

## [0.3.7](https://github.com/Sendspin/sendspin-rs/compare/v0.3.6...v0.3.7) - 2026-08-21

### Fixed

- *(clippy)* use as_chunks for PCM decoding ([#100](https://github.com/Sendspin/sendspin-rs/pull/100))
- fix build error
- calculation in musl targets

### Other

- Merge branch 'pr-99'
- Fix clippy error on musl-linux
- Apply suggestion from @DanielHabenicht
- Update raw_clock.rs
- Update time conversion in now_micros function
- Document conversion requirement for musl targets

### Added

- Add spec-aligned `visualizer@v1` negotiation and forwarding for loudness, beat,
  dominant-frequency, spectrum, and peak data. Visualizer payloads remain raw
  bytes for applications to decode and render.

### Changed

- Expand visualizer stream configuration and format-request APIs.

## [0.3.6](https://github.com/Sendspin/sendspin-rs/compare/v0.3.5...v0.3.6) - 2026-07-15

### Other

- Add FLAC decoding support ([#92](https://github.com/Sendspin/sendspin-rs/pull/92))
- Preserve ConnectionGuard unwind safety ([#90](https://github.com/Sendspin/sendspin-rs/pull/90))
- Add ConnectionManager: multi-server arbitration with automatic goodbye ([#88](https://github.com/Sendspin/sendspin-rs/pull/88))

## [0.3.5](https://github.com/Sendspin/sendspin-rs/compare/v0.3.4...v0.3.5) - 2026-07-10

### Other

- Fix timing robustness ([#84](https://github.com/Sendspin/sendspin-rs/pull/84))

### Added

- Count correction engagements per playback generation and report the total in
  the generation-change summary line, so "did the corrector ever fire?" is
  answerable from logs retroactively even when debug logging was off

### Fixed

- Stop drift corrections from chasing period-quantized presentation-timestamp
  noise: sync errors are floor-filtered (windowed minimum over ~1s, robust to
  one-sided timestamp flapping whenever the quiet mode is sampled at least
  once per window), corrections only engage after a sustained over-threshold
  streak, and stream reanchors derive their timeline anchor from the same
  delta floor instead of a single wake's reading, so measurement flapping no
  longer triggers immediate drop/insert churn or biased stream-start anchors
- Promote the Windows audio callback thread to real-time priority (MMCSS via
  cpal's `realtime` feature), greatly reducing the chance of UI load starving
  the WASAPI event loop; a failed promotion is logged as a warning instead of
  surfacing as a stream error
- Request a 40ms WASAPI endpoint buffer by default on Windows (engine default
  is 20ms), giving late callback wakes margin before the mixer starves;
  explicit `SyncedPlayerConfig::buffer_size` still overrides

## [0.3.4](https://github.com/Sendspin/sendspin-rs/compare/v0.3.3...v0.3.4) - 2026-07-05

### Fixed

- Stop evicting queued audio on sub-frame timestamp overlaps ([#82](https://github.com/Sendspin/sendspin-rs/pull/82))

## [0.3.3](https://github.com/Sendspin/sendspin-rs/compare/v0.3.2...v0.3.3) - 2026-07-04

### Added

- Add comprehensive trace-level logging ([#80](https://github.com/Sendspin/sendspin-rs/pull/80))

### Fixed

- Eliminate early-stream audio glitches from clock sync warm-up ([#81](https://github.com/Sendspin/sendspin-rs/pull/81))

### Other

- Fix static delay reanchor handling ([#78](https://github.com/Sendspin/sendspin-rs/pull/78))

## [0.3.2](https://github.com/Sendspin/sendspin-rs/compare/v0.3.1...v0.3.2) - 2026-07-03

### Other

- Fix/update logging ([#76](https://github.com/Sendspin/sendspin-rs/pull/76))

## [0.3.1](https://github.com/Sendspin/sendspin-rs/compare/v0.3.0...v0.3.1) - 2026-07-02

### Other

- bump dependencies to latest stable ([#74](https://github.com/Sendspin/sendspin-rs/pull/74))

## [0.3.0](https://github.com/Sendspin/sendspin-rs/compare/v0.2.1...v0.3.0) - 2026-06-30

### Added

- [**breaking**] Align to sendspin spec for sync state ([#72](https://github.com/Sendspin/sendspin-rs/pull/72))
- add mac_address field to DeviceInfo per spec PR #87 ([#70](https://github.com/Sendspin/sendspin-rs/pull/70))
- support more hardware formats ([#62](https://github.com/Sendspin/sendspin-rs/pull/62))
- [**breaking**] Add playback underrun recovery buffering ([#69](https://github.com/Sendspin/sendspin-rs/pull/69))
- Add external source API ([#68](https://github.com/Sendspin/sendspin-rs/pull/68))
- emit stream/request-format messages ([#56](https://github.com/Sendspin/sendspin-rs/pull/56)) ([#63](https://github.com/Sendspin/sendspin-rs/pull/63))
- apply static_delay_ms in the SyncedPlayer playback path ([#65](https://github.com/Sendspin/sendspin-rs/pull/65))
- gate Kalman drift behind SNR check before conversion ([#64](https://github.com/Sendspin/sendspin-rs/pull/64))
- [**breaking**] add option to overwrite the default buffer size ([#48](https://github.com/Sendspin/sendspin-rs/pull/48))
- [**breaking**] add inbound WebSocket listener for server-initiated connections ([#57](https://github.com/Sendspin/sendspin-rs/pull/57))
- add metadata role support ([#58](https://github.com/Sendspin/sendspin-rs/pull/58))

### Fixed

- [**breaking**] use cpal sample instead of custom implementation ([#47](https://github.com/Sendspin/sendspin-rs/pull/47))
- pr#48 left synced_player uncompilable ([#61](https://github.com/Sendspin/sendspin-rs/pull/61))
- [**breaking**] remove unused output code ([#46](https://github.com/Sendspin/sendspin-rs/pull/46))

### Other

- Add repeat/shuffle to ControllerState; deprecate on MetadataState ([#66](https://github.com/Sendspin/sendspin-rs/pull/66)) ([#73](https://github.com/Sendspin/sendspin-rs/pull/73))
- Fix synced playback startup handoff ([#71](https://github.com/Sendspin/sendspin-rs/pull/71))
- [**breaking**] remove vestigial AudioScheduler and AudioBuffer::play_at ([#67](https://github.com/Sendspin/sendspin-rs/pull/67))
- add server-initiated metadata example ([#60](https://github.com/Sendspin/sendspin-rs/pull/60))

### Removed

- [**breaking**] Remove unused `AudioScheduler` and the `AudioBuffer::play_at` field — superseded by `SyncedPlayer`, which converts server timestamps to local play time live in the output callback rather than baking in a schedule that goes stale when the clock estimate moves

## [0.2.1](https://github.com/Sendspin/sendspin-rs/compare/v0.2.0...v0.2.1) - 2026-05-25

### Other

- close coverage gaps flagged by cargo mutants ([#43](https://github.com/Sendspin/sendspin-rs/pull/43))

## [0.2.0](https://github.com/Sendspin/sendspin-rs/compare/v0.1.2...v0.2.0) - 2026-04-16

### Added

- [**breaking**] Use raw monotonic clock for NTP-immune time sync ([#42](https://github.com/Sendspin/sendspin-rs/pull/42))
- Expose GainControl for standalone use ([#39](https://github.com/Sendspin/sendspin-rs/pull/39))
- [**breaking**] narrow public API surface — builder is the sole entry point ([#36](https://github.com/Sendspin/sendspin-rs/pull/36))
- Harden audio pipeline against silent data loss and malformed input ([#40](https://github.com/Sendspin/sendspin-rs/pull/40))
- [**breaking**] Add controller role support ([#35](https://github.com/Sendspin/sendspin-rs/pull/35))
- [**breaking**] Implement graceful disconnect and initial client state in builder ([#34](https://github.com/Sendspin/sendspin-rs/pull/34))
- [**breaking**] align protocol message types with Sendspin spec ([#31](https://github.com/Sendspin/sendspin-rs/pull/31))

### Other

- *(sync/clock)* add regression tests for time filter monotonicity guard ([#38](https://github.com/Sendspin/sendspin-rs/pull/38))
- Fix time filter monotonicity guard (== -> <=) ([#37](https://github.com/Sendspin/sendspin-rs/pull/37))

## [0.1.2](https://github.com/Sendspin/sendspin-rs/compare/v0.1.1...v0.1.2) - 2026-03-16

### Other

- Increase test coverage ([#29](https://github.com/Sendspin/sendspin-rs/pull/29))
- Update to latest CPAL. Also add some convience features to the player example ([#27](https://github.com/Sendspin/sendspin-rs/pull/27))

## [0.1.1](https://github.com/Sendspin/sendspin-rs/compare/v0.1.0...v0.1.1) - 2026-03-03

### Added

- implement clock synchronization with server ([#18](https://github.com/Sendspin/sendspin-rs/pull/18))
- add software volume/mute control and fix playback start blip ([#10](https://github.com/Sendspin/sendspin-rs/pull/10))

### Fixed

- add 16 bit PCM fallback for Music Assistant 2.7 ([#24](https://github.com/Sendspin/sendspin-rs/pull/24))
- validate channels > 0 at construction to prevent callback panic ([#14](https://github.com/Sendspin/sendspin-rs/pull/14))
- reject time conversions when filter drift is implausible ([#13](https://github.com/Sendspin/sendspin-rs/pull/13))
- five clock sync filter correctness issues ([#12](https://github.com/Sendspin/sendspin-rs/pull/12))
- correct f32 sample normalization and extract Sample::to_f32() ([#11](https://github.com/Sendspin/sendspin-rs/pull/11))

### Other

- create release workflow ([#25](https://github.com/Sendspin/sendspin-rs/pull/25))
- Add instructions for deps. ([#23](https://github.com/Sendspin/sendspin-rs/pull/23))
- consolidate mutex acquisitions in audio callback ([#17](https://github.com/Sendspin/sendspin-rs/pull/17))
- use a typed builder  ([#7](https://github.com/Sendspin/sendspin-rs/pull/7))
- add devcontainer ([#21](https://github.com/Sendspin/sendspin-rs/pull/21))
- Allow custom SSL and auth via `IntoClientRequest` ([#8](https://github.com/Sendspin/sendspin-rs/pull/8))
