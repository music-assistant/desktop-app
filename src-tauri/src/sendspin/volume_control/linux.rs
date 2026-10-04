//! Linux volume control implementation using `PulseAudio`

use super::{VolumeChangeCallback, VolumeControlImpl};
use cpal::traits::{DeviceTrait, HostTrait};
use libpulse_binding::{
    callbacks::ListResult,
    context::{
        subscribe::{Facility, InterestMaskSet, Operation},
        Context, FlagSet as ContextFlagSet,
    },
    mainloop::threaded::Mainloop,
    proplist::Proplist,
    volume::{ChannelVolumes, Volume},
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_SINK_RETRY_INTERVAL: Duration = Duration::from_secs(1);
// Connection, subscription and two sink queries each have a one-second budget.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(6);

fn selected_pcm() -> Result<Option<String>, String> {
    let Some(name) = crate::settings::get_settings().audio_device_id else {
        return Ok(None);
    };
    let host = cpal::default_host();
    let mut outputs = host.output_devices().map_err(|e| e.to_string())?;
    let selected = outputs.find(|device| {
        device
            .description()
            .is_ok_and(|description| description.name() == name)
    });
    // Playback also falls back when its configured display name disappears.
    let Some(selected) = selected else {
        return Ok(None);
    };
    if host.default_output_device().is_some_and(|default| {
        default
            .id()
            .ok()
            .zip(selected.id().ok())
            .is_some_and(|(a, b)| a == b)
    }) {
        return Ok(None);
    }
    selected
        .description()
        .map_err(|e| e.to_string())?
        .driver()
        .map(str::to_owned)
        .map(Some)
        .ok_or_else(|| "Selected ALSA output has no PCM identifier".to_string())
}

fn unique_sink(indices: &[u32]) -> Result<u32, String> {
    match indices {
        [index] => Ok(*index),
        [] => Err("Selected ALSA output has no matching PulseAudio sink".to_string()),
        _ => Err("Selected ALSA output matches multiple PulseAudio sinks".to_string()),
    }
}

fn selected_sink_index(mainloop: &mut Mainloop, context: &Context) -> Result<Option<u32>, String> {
    let Some(pcm) = selected_pcm()? else {
        return Ok(None);
    };
    let (tx, rx) = channel();
    let mut indices = Vec::new();
    with_mainloop(mainloop, || {
        context
            .introspect()
            .get_sink_info_list(move |result| match result {
                ListResult::Item(info) => {
                    // CPAL's ALSA driver is the PCM identifier, not a display name; match it
                    // exactly against PulseAudio's device.string, never descriptions, card
                    // names or substrings: custom ALSA plugins can route anywhere (or nowhere).
                    if info.proplist.get_str("device.string").as_deref() == Some(pcm.as_str()) {
                        indices.push(info.index);
                    }
                }
                ListResult::End => {
                    let _ = tx.send(unique_sink(&indices));
                }
                ListResult::Error => {
                    let _ = tx.send(Err("Failed to enumerate PulseAudio sinks".to_string()));
                }
            });
    });
    rx.recv_timeout(REQUEST_TIMEOUT)
        .map_err(|_| "Timeout resolving selected sink".to_string())?
        .map(Some)
}

fn receive_command(
    receiver: &Receiver<VolumeCommand>,
    sink_valid: bool,
    retry_interval: Duration,
) -> Option<VolumeCommand> {
    if sink_valid {
        return receiver.recv().ok();
    }
    match receiver.recv_timeout(retry_interval) {
        Ok(command) => Some(command),
        Err(RecvTimeoutError::Timeout) => Some(VolumeCommand::RefreshDefault),
        Err(RecvTimeoutError::Disconnected) => None,
    }
}

enum VolumeCommand {
    SetVolume(u8, Sender<Result<(), String>>),
    SetMute(bool, Sender<Result<(), String>>),
    GetVolume(Sender<Result<u8, String>>),
    GetMute(Sender<Result<bool, String>>),
    IsAvailable(Sender<bool>),
    SetChangeCallback(VolumeChangeCallback, Sender<Result<(), String>>),
    RefreshDefault,
    SinkChanged(u32, u64),
    Shutdown,
}

#[derive(Clone, Copy)]
struct SinkSnapshot {
    index: u32,
    generation: u64,
    volume: ChannelVolumes,
    muted: bool,
}

impl SinkSnapshot {
    fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    fn volume_percent(&self) -> u8 {
        (u64::from(self.volume.avg().0) * 100 / u64::from(Volume::NORMAL.0)).min(100) as u8
    }
}

// All PulseAudio objects stay on the command thread. Its mainloop lock is held
// only while calling PulseAudio, never while waiting for a callback response.
fn with_mainloop<T>(mainloop: &mut Mainloop, action: impl FnOnce() -> T) -> T {
    mainloop.lock();
    let result = action();
    mainloop.unlock();
    result
}

fn query_sink(
    mainloop: &mut Mainloop,
    context: &Context,
    generation: u64,
    index: Option<u32>,
) -> Result<SinkSnapshot, String> {
    if !with_mainloop(mainloop, || {
        context.get_state() == libpulse_binding::context::State::Ready
    }) {
        return Err("PulseAudio context is not ready".to_string());
    }
    let index = match index {
        Some(index) => Some(index),
        None => selected_sink_index(mainloop, context)?,
    };
    let name = if index.is_none() {
        let (tx, rx) = channel();
        with_mainloop(mainloop, || {
            context.introspect().get_server_info(move |info| {
                let _ = tx.send(info.default_sink_name.as_ref().map(ToString::to_string));
            });
        });
        Some(
            rx.recv_timeout(REQUEST_TIMEOUT)
                .map_err(|_| "Timeout getting default sink".to_string())?
                .ok_or_else(|| "Default sink not found".to_string())?,
        )
    } else {
        None
    };

    let (tx, rx) = channel();
    let callback =
        move |result: ListResult<&libpulse_binding::context::introspect::SinkInfo<'_>>| {
            let response = match result {
                ListResult::Item(info) => Ok(SinkSnapshot {
                    index: info.index,
                    generation,
                    volume: info.volume,
                    muted: info.mute,
                }),
                ListResult::End | ListResult::Error => Err("Sink not found".to_string()),
            };
            let _ = tx.send(response);
        };
    with_mainloop(mainloop, || {
        let introspect = context.introspect();
        if let Some(index) = index {
            introspect.get_sink_info_by_index(index, callback);
        } else if let Some(name) = name {
            introspect.get_sink_info_by_name(&name, callback);
        }
    });
    rx.recv_timeout(REQUEST_TIMEOUT)
        .map_err(|_| "Timeout getting sink info".to_string())?
}

fn refresh_default(
    mainloop: &mut Mainloop,
    context: &Context,
    generation: &AtomicU64,
    sink: &mut Option<SinkSnapshot>,
    callback: Option<&VolumeChangeCallback>,
) -> Result<(), String> {
    // Clear the old target even if the new default is missing or lookup fails.
    *sink = None;
    let expected = generation.load(Ordering::SeqCst);
    let snapshot = query_sink(mainloop, context, expected, None)?;
    with_mainloop(mainloop, || {
        if !snapshot.is_current(generation.load(Ordering::SeqCst)) {
            return Err("Default sink changed during lookup".to_string());
        }
        *sink = Some(snapshot);
        publish(snapshot, callback);
        Ok(())
    })
}

fn publish(sink: SinkSnapshot, callback: Option<&VolumeChangeCallback>) {
    if let Some(callback) = callback {
        let _ = callback.send((sink.volume_percent(), sink.muted));
    }
}

pub struct LinuxVolumeControl {
    command_tx: Sender<VolumeCommand>,
}

impl LinuxVolumeControl {
    #[allow(clippy::new_ret_no_self)]
    #[allow(clippy::unnecessary_wraps)]
    pub fn new() -> Option<Box<dyn VolumeControlImpl + Send>> {
        let control = Self::initialize();
        Some(Box::new(control))
    }

    fn initialize() -> Self {
        let (command_tx, command_rx) = channel::<VolumeCommand>();
        let event_tx = command_tx.clone();
        thread::spawn(move || {
            let Some(mut mainloop) = Mainloop::new() else {
                log::error!("[VolumeControl] Failed to create PulseAudio mainloop");
                return;
            };
            let mut proplist = Proplist::new().unwrap();
            proplist
                .set_str(
                    libpulse_binding::proplist::properties::APPLICATION_NAME,
                    "Music Assistant",
                )
                .unwrap();
            let Some(mut context) =
                Context::new_with_proplist(&mainloop, "MusicAssistantContext", &proplist)
            else {
                log::error!("[VolumeControl] Failed to create PulseAudio context");
                return;
            };
            if context
                .connect(None, ContextFlagSet::NOFLAGS, None)
                .is_err()
                || mainloop.start().is_err()
            {
                log::error!("[VolumeControl] Failed to start PulseAudio connection");
                return;
            }
            let connection_started = Instant::now();
            loop {
                if connection_started.elapsed() >= REQUEST_TIMEOUT {
                    mainloop.stop();
                    return;
                }
                match with_mainloop(&mut mainloop, || context.get_state()) {
                    libpulse_binding::context::State::Ready => break,
                    libpulse_binding::context::State::Failed
                    | libpulse_binding::context::State::Terminated => {
                        mainloop.stop();
                        return;
                    }
                    _ => thread::sleep(Duration::from_millis(10)),
                }
            }

            let generation = Arc::new(AtomicU64::new(0));
            let event_generation = generation.clone();
            let (tx, rx) = channel();
            with_mainloop(&mut mainloop, || {
                // Install the callback before subscribing so no change is lost.
                context.set_subscribe_callback(Some(Box::new(
                    move |facility, operation, index| {
                        if facility == Some(Facility::Server)
                            || (facility == Some(Facility::Sink)
                                && matches!(operation, Some(Operation::New | Operation::Removed)))
                        {
                            // Invalidate in the callback, not later on the worker: a
                            // lookup already in flight must not publish the old sink.
                            event_generation.fetch_add(1, Ordering::SeqCst);
                            let _ = event_tx.send(VolumeCommand::RefreshDefault);
                        } else if facility == Some(Facility::Sink)
                            && operation == Some(Operation::Changed)
                        {
                            let _ = event_tx.send(VolumeCommand::SinkChanged(
                                index,
                                event_generation.load(Ordering::SeqCst),
                            ));
                        }
                    },
                )));
                context.subscribe(
                    InterestMaskSet::SERVER | InterestMaskSet::SINK,
                    move |success| {
                        let _ = tx.send(success);
                    },
                );
            });
            if rx.recv_timeout(REQUEST_TIMEOUT) != Ok(true) {
                log::error!("[VolumeControl] Failed to subscribe to PulseAudio events");
                mainloop.stop();
                context.disconnect();
                return;
            }

            let mut sink = None;
            let mut selected_name = crate::settings::get_settings().audio_device_id;
            let mut callback = None;
            let mut last_self_change: Option<(u64, Instant)> = None;
            let _ = refresh_default(
                &mut mainloop,
                &context,
                &generation,
                &mut sink,
                callback.as_ref(),
            );
            log::info!("[VolumeControl] Linux PulseAudio volume control initialized");

            // A failed lookup leaves no target. Retry without requiring another
            // server event, but keep an indefinite wait while the target is valid.
            while let Some(command) = receive_command(
                &command_rx,
                sink.is_some_and(|s| s.is_current(generation.load(Ordering::SeqCst))),
                DEFAULT_SINK_RETRY_INTERVAL,
            ) {
                let configured = crate::settings::get_settings().audio_device_id;
                if configured != selected_name {
                    selected_name = configured;
                    generation.fetch_add(1, Ordering::SeqCst);
                    sink = None;
                }
                match command {
                    VolumeCommand::Shutdown => break,
                    VolumeCommand::RefreshDefault => {
                        // Default changes must publish even during self-change suppression.
                        let _ = refresh_default(
                            &mut mainloop,
                            &context,
                            &generation,
                            &mut sink,
                            callback.as_ref(),
                        );
                    }
                    VolumeCommand::SetChangeCallback(new_callback, response) => {
                        callback = Some(new_callback);
                        // Registration works without a default; later hotplug events recover it.
                        let _ = refresh_default(
                            &mut mainloop,
                            &context,
                            &generation,
                            &mut sink,
                            callback.as_ref(),
                        );
                        let _ = response.send(Ok(()));
                    }
                    VolumeCommand::SinkChanged(index, event_generation) => {
                        let current = generation.load(Ordering::SeqCst);
                        if event_generation != current
                            || !sink.is_some_and(|s| s.index == index && s.is_current(current))
                            || last_self_change.is_some_and(|(g, time)| {
                                g == current && time.elapsed() < Duration::from_millis(200)
                            })
                        {
                            continue;
                        }
                        if let Ok(updated) =
                            query_sink(&mut mainloop, &context, current, Some(index))
                        {
                            with_mainloop(&mut mainloop, || {
                                if updated.is_current(generation.load(Ordering::SeqCst)) {
                                    sink = Some(updated);
                                    publish(updated, callback.as_ref());
                                }
                            });
                        }
                    }
                    command => {
                        if !sink.is_some_and(|s| s.is_current(generation.load(Ordering::SeqCst))) {
                            let _ = refresh_default(
                                &mut mainloop,
                                &context,
                                &generation,
                                &mut sink,
                                callback.as_ref(),
                            );
                        }
                        match command {
                            VolumeCommand::IsAvailable(response) => {
                                let available = with_mainloop(&mut mainloop, || {
                                    context.get_state() == libpulse_binding::context::State::Ready
                                        && sink.is_some_and(|s| {
                                            s.is_current(generation.load(Ordering::SeqCst))
                                        })
                                });
                                let _ = response.send(available);
                            }
                            VolumeCommand::GetVolume(response) => {
                                let result =
                                    Self::read_sink(&mut mainloop, &context, &generation, sink)
                                        .map(|s| s.volume_percent());
                                let _ = response.send(result);
                            }
                            VolumeCommand::GetMute(response) => {
                                let result =
                                    Self::read_sink(&mut mainloop, &context, &generation, sink)
                                        .map(|s| s.muted);
                                let _ = response.send(result);
                            }
                            VolumeCommand::SetVolume(volume, response) => {
                                let result =
                                    Self::read_sink(&mut mainloop, &context, &generation, sink)
                                        .and_then(|mut target| {
                                            target.volume.set(
                                                target.volume.len(),
                                                Volume(
                                                    Volume::NORMAL.0 * u32::from(volume.min(100))
                                                        / 100,
                                                ),
                                            );
                                            last_self_change =
                                                Some((target.generation, Instant::now()));
                                            Self::write_sink(
                                                &mut mainloop,
                                                &context,
                                                &generation,
                                                target,
                                                false,
                                            )
                                        });
                                let _ = response.send(result);
                            }
                            VolumeCommand::SetMute(muted, response) => {
                                let result = sink
                                    .ok_or_else(|| "Sink not found".to_string())
                                    .and_then(|mut target| {
                                        target.muted = muted;
                                        last_self_change =
                                            Some((target.generation, Instant::now()));
                                        Self::write_sink(
                                            &mut mainloop,
                                            &context,
                                            &generation,
                                            target,
                                            true,
                                        )
                                    });
                                let _ = response.send(result);
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
            mainloop.stop();
            context.set_subscribe_callback(None);
            context.disconnect();
        });
        Self { command_tx }
    }

    fn read_sink(
        mainloop: &mut Mainloop,
        context: &Context,
        generation: &AtomicU64,
        sink: Option<SinkSnapshot>,
    ) -> Result<SinkSnapshot, String> {
        let sink = sink.ok_or_else(|| "Sink not found".to_string())?;
        let updated = query_sink(mainloop, context, sink.generation, Some(sink.index))?;
        if !updated.is_current(generation.load(Ordering::SeqCst)) {
            return Err("Default sink changed during lookup".to_string());
        }
        Ok(updated)
    }

    fn write_sink(
        mainloop: &mut Mainloop,
        context: &Context,
        generation: &AtomicU64,
        sink: SinkSnapshot,
        mute: bool,
    ) -> Result<(), String> {
        let (tx, rx) = channel();
        with_mainloop(mainloop, || {
            if !sink.is_current(generation.load(Ordering::SeqCst)) {
                return Err("Default sink changed before write".to_string());
            }
            let callback = Some(Box::new(move |success| {
                let _ = tx.send(success);
            }) as Box<dyn FnMut(bool)>);
            let mut introspect = context.introspect();
            if mute {
                introspect.set_sink_mute_by_index(sink.index, sink.muted, callback);
            } else {
                introspect.set_sink_volume_by_index(sink.index, &sink.volume, callback);
            }
            Ok(())
        })?;
        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(true) => Ok(()),
            Ok(false) => Err("Failed to update sink".to_string()),
            Err(_) => Err("Timeout updating sink".to_string()),
        }
    }
}

impl VolumeControlImpl for LinuxVolumeControl {
    fn set_volume(&mut self, volume: u8) -> Result<(), String> {
        let (response_tx, response_rx) = channel();
        self.command_tx
            .send(VolumeCommand::SetVolume(volume, response_tx))
            .map_err(|_| "Failed to send command".to_string())?;
        response_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "Timeout waiting for response".to_string())?
    }

    fn set_mute(&mut self, muted: bool) -> Result<(), String> {
        let (response_tx, response_rx) = channel();
        self.command_tx
            .send(VolumeCommand::SetMute(muted, response_tx))
            .map_err(|_| "Failed to send command".to_string())?;
        response_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "Timeout waiting for response".to_string())?
    }

    fn get_volume(&self) -> Result<u8, String> {
        let (response_tx, response_rx) = channel();
        self.command_tx
            .send(VolumeCommand::GetVolume(response_tx))
            .map_err(|_| "Failed to send command".to_string())?;
        response_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "Timeout waiting for response".to_string())?
    }

    fn get_mute(&self) -> Result<bool, String> {
        let (response_tx, response_rx) = channel();
        self.command_tx
            .send(VolumeCommand::GetMute(response_tx))
            .map_err(|_| "Failed to send command".to_string())?;
        response_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "Timeout waiting for response".to_string())?
    }

    fn is_available(&self) -> bool {
        let (response_tx, response_rx) = channel();
        if self
            .command_tx
            .send(VolumeCommand::IsAvailable(response_tx))
            .is_err()
        {
            return false;
        }
        response_rx.recv_timeout(COMMAND_TIMEOUT).unwrap_or(false)
    }

    fn set_change_callback(&mut self, callback: VolumeChangeCallback) -> Result<(), String> {
        let (response_tx, response_rx) = channel();
        self.command_tx
            .send(VolumeCommand::SetChangeCallback(callback, response_tx))
            .map_err(|_| "Failed to send command".to_string())?;
        response_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "Timeout waiting for response".to_string())?
    }
}

impl Drop for LinuxVolumeControl {
    fn drop(&mut self) {
        let _ = self.command_tx.send(VolumeCommand::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_sink_requires_exactly_one_match() {
        assert_eq!(unique_sink(&[42]), Ok(42));
        assert!(unique_sink(&[]).is_err());
        assert!(unique_sink(&[1, 2]).is_err());
    }

    #[test]
    fn availability_waits_for_worker_refresh() {
        let (tx, rx) = channel();
        let control = LinuxVolumeControl { command_tx: tx };
        let worker = thread::spawn(move || {
            if let VolumeCommand::IsAvailable(response) = rx.recv().unwrap() {
                thread::sleep(Duration::from_millis(600));
                response.send(true).unwrap();
            }
        });
        assert!(control.is_available());
        worker.join().unwrap();
    }

    #[test]
    fn missing_sink_retries_after_idle_timeout() {
        let (_tx, rx) = channel();
        assert!(matches!(
            receive_command(&rx, false, Duration::ZERO),
            Some(VolumeCommand::RefreshDefault)
        ));
    }

    #[test]
    fn queued_sink_event_does_not_prevent_next_idle_retry() {
        let (tx, rx) = channel();
        tx.send(VolumeCommand::SinkChanged(7, 1)).unwrap();
        assert!(matches!(
            receive_command(&rx, false, Duration::ZERO),
            Some(VolumeCommand::SinkChanged(7, 1))
        ));
        assert!(matches!(
            receive_command(&rx, false, Duration::ZERO),
            Some(VolumeCommand::RefreshDefault)
        ));
    }

    #[test]
    fn shutdown_is_received_with_or_without_a_valid_sink() {
        for sink_valid in [false, true] {
            let (tx, rx) = channel();
            tx.send(VolumeCommand::Shutdown).unwrap();
            assert!(matches!(
                receive_command(&rx, sink_valid, DEFAULT_SINK_RETRY_INTERVAL),
                Some(VolumeCommand::Shutdown)
            ));
        }
    }

    #[test]
    fn disconnected_channel_exits_instead_of_retrying() {
        for sink_valid in [false, true] {
            let (tx, rx) = channel();
            drop(tx);
            assert!(receive_command(&rx, sink_valid, Duration::ZERO).is_none());
        }
    }

    #[test]
    fn stale_sink_is_rejected_even_when_index_is_reused() {
        let sink = SinkSnapshot {
            index: 7,
            generation: 1,
            volume: ChannelVolumes::default(),
            muted: false,
        };
        assert!(sink.is_current(1));
        assert!(!sink.is_current(2));
    }

    #[test]
    fn publishes_new_sink_volume_and_mute_together() {
        let (tx, rx) = channel();
        let mut volume = ChannelVolumes::default();
        volume.set(2, Volume(Volume::NORMAL.0 / 2));
        publish(
            SinkSnapshot {
                index: 9,
                generation: 2,
                volume,
                muted: true,
            },
            Some(&tx),
        );
        assert_eq!(rx.try_recv(), Ok((50, true)));
    }

    #[test]
    fn volume_percent_is_bounded_for_amplified_sinks() {
        let mut volume = ChannelVolumes::default();
        volume.set(2, Volume(Volume::NORMAL.0 * 2));
        let sink = SinkSnapshot {
            index: 7,
            generation: 1,
            volume,
            muted: true,
        };
        assert_eq!(sink.volume_percent(), 100);
    }
}
