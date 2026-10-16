//! Hardware regression for stereo playback on Windows surround endpoints.

#![cfg(target_os = "windows")]

use cpal::traits::{DeviceTrait, HostTrait};
use parking_lot::Mutex;
use sendspin::audio::{AudioFormat, Codec, SyncedPlayer, SyncedPlayerConfig};
use sendspin::sync::ClockSync;
use sendspin::DefaultClock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Run with `cargo test --test windows_shared_output -- --ignored --nocapture`.
/// Select a surround endpoint with `SENDSPIN_TEST_OUTPUT_DEVICE`, or leave it
/// unset to exercise the system-default path. No audio is enqueued, and the
/// player starts muted; the test must remain silent.
#[test]
#[ignore = "requires a Windows playback endpoint; use an eight-channel endpoint to reproduce #146"]
fn stereo_stream_opens_and_receives_silent_callbacks() {
    let host = cpal::default_host();
    let device_name = std::env::var("SENDSPIN_TEST_OUTPUT_DEVICE").ok();

    for sample_rate in [44_100, 48_000, 96_000] {
        let device = device_name.as_ref().map(|name| {
            host.output_devices()
                .expect("enumerate output devices")
                .find(|device| {
                    device
                        .description()
                        .is_ok_and(|description| description.name() == name)
                })
                .unwrap_or_else(|| panic!("output device not found: {name}"))
        });
        let endpoint = device.as_ref().map_or_else(
            || {
                host.default_output_device()
                    .expect("default output device")
                    .default_output_config()
                    .expect("native format")
            },
            |device| device.default_output_config().expect("native format"),
        );
        println!(
            "stereo {sample_rate} Hz -> {} channels, {} Hz ({})",
            endpoint.channels(),
            endpoint.sample_rate(),
            device_name.as_deref().unwrap_or("system default")
        );

        let callbacks = Arc::new(AtomicUsize::new(0));
        let observed_callbacks = Arc::clone(&callbacks);
        let non_silent_samples = Arc::new(AtomicUsize::new(0));
        let observed_non_silent = Arc::clone(&non_silent_samples);
        let player = SyncedPlayer::with_process_callback(
            AudioFormat {
                codec: Codec::Pcm,
                channels: 2,
                sample_rate,
                bit_depth: 24,
                codec_header: None,
            },
            Arc::new(Mutex::new(ClockSync::new(Arc::new(DefaultClock::new())))),
            SyncedPlayerConfig {
                device,
                muted: true,
                ..SyncedPlayerConfig::new()
            },
            Box::new(move |samples| {
                observed_callbacks.fetch_add(1, Ordering::Relaxed);
                observed_non_silent.fetch_add(
                    samples.iter().filter(|&&sample| sample != 0.0).count(),
                    Ordering::Relaxed,
                );
            }),
        )
        .unwrap_or_else(|error| panic!("stereo {sample_rate} Hz output failed: {error}"));

        let deadline = Instant::now() + Duration::from_secs(2);
        while callbacks.load(Ordering::Relaxed) < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !player.has_error(),
            "output error: {:?}",
            player.take_error()
        );
        assert!(
            callbacks.load(Ordering::Relaxed) >= 3,
            "no output callbacks"
        );
        assert_eq!(non_silent_samples.load(Ordering::Relaxed), 0);
    }
}
