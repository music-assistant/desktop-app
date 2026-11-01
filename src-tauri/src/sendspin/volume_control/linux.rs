//! Linux volume control implementation using `PulseAudio`

use super::{VolumeChangeCallback, VolumeControlImpl, VolumeSample, VolumeTarget};
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
use parking_lot::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_SINK_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const CONNECTION_RETRY_MAX: Duration = Duration::from_secs(5);
// Connection, subscription and two sink queries each have a one-second budget.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(6);

fn selected_pcm(target: &VolumeTarget) -> Result<&str, String> {
    match target {
        VolumeTarget::Device { native_id } if !native_id.is_empty() => Ok(native_id),
        _ => Err("Opened ALSA output has no volume target".into()),
    }
}

fn default_sink_name(name: Option<&str>) -> Result<&str, String> {
    name.filter(|name| !name.is_empty())
        .ok_or_else(|| "Default sink not found".to_string())
}

fn unique_named_sink(name: &str, sinks: &[(u32, String)]) -> Result<u32, String> {
    let indices: Vec<_> = sinks
        .iter()
        .filter_map(|(index, sink_name)| (*sink_name == name).then_some(*index))
        .collect();
    unique_sink(&indices)
}

fn unique_sink(indices: &[u32]) -> Result<u32, String> {
    match indices {
        [index] => Ok(*index),
        [] => Err("Selected ALSA output has no matching PulseAudio sink".to_string()),
        _ => Err("Selected ALSA output matches multiple PulseAudio sinks".to_string()),
    }
}

fn system_default_sink_index(mainloop: &mut Mainloop, context: &Context) -> Result<u32, String> {
    let (tx, rx) = channel();
    with_mainloop(mainloop, || {
        context.introspect().get_server_info(move |info| {
            let _ = tx.send(info.default_sink_name.as_ref().map(ToString::to_string));
        });
    });
    let name = rx
        .recv_timeout(REQUEST_TIMEOUT)
        .map_err(|_| "Timeout getting default sink".to_string())?;
    let name = default_sink_name(name.as_deref())?.to_string();
    let (tx, rx) = channel();
    let mut sinks = Vec::new();
    with_mainloop(mainloop, || {
        context
            .introspect()
            .get_sink_info_list(move |result| match result {
                ListResult::Item(info) => {
                    if let Some(sink_name) = info.name.as_ref() {
                        sinks.push((info.index, sink_name.to_string()));
                    }
                }
                ListResult::End => {
                    let _ = tx.send(unique_named_sink(&name, &sinks));
                }
                ListResult::Error => {
                    let _ = tx.send(Err("Failed to enumerate PulseAudio sinks".to_string()));
                }
            });
    });
    rx.recv_timeout(REQUEST_TIMEOUT)
        .map_err(|_| "Timeout resolving default sink".to_string())?
}

fn selected_sink_index(
    mainloop: &mut Mainloop,
    context: &Context,
    target: &VolumeTarget,
) -> Result<u32, String> {
    if matches!(target, VolumeTarget::SystemDefault) {
        return system_default_sink_index(mainloop, context);
    }
    let pcm = selected_pcm(target)?.to_string();
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
}

fn unavailable_command(
    command: VolumeCommand,
    callback: &mut Option<VolumeChangeCallback>,
) -> bool {
    match command {
        VolumeCommand::Shutdown => false,
        VolumeCommand::SetChangeCallback(new_callback, response) => {
            *callback = Some(new_callback);
            let _ = response.send(Ok(()));
            true
        }
        VolumeCommand::IsAvailable(response) => {
            let _ = response.send(false);
            true
        }
        VolumeCommand::GetVolume(response) => {
            let _ = response.send(Err("PulseAudio unavailable".into()));
            true
        }
        VolumeCommand::GetMute(response) => {
            let _ = response.send(Err("PulseAudio unavailable".into()));
            true
        }
        VolumeCommand::SetVolume(_, response) | VolumeCommand::SetMute(_, response) => {
            let _ = response.send(Err("PulseAudio unavailable".into()));
            true
        }
        VolumeCommand::SetTarget
        | VolumeCommand::RefreshDefault
        | VolumeCommand::HealthCheck
        | VolumeCommand::SinkChanged(..) => true,
    }
}

fn connection_lost_command(
    command: VolumeCommand,
    callback: &mut Option<VolumeChangeCallback>,
) -> Result<(), String> {
    if unavailable_command(command, callback) {
        Err("PulseAudio connection lost".into())
    } else {
        Ok(())
    }
}

fn retry_connection(
    receiver: &Receiver<VolumeCommand>,
    callback: &mut Option<VolumeChangeCallback>,
    delay: Duration,
) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(command) => {
                if !unavailable_command(command, callback) {
                    return false;
                }
            }
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn receive_command(
    receiver: &Receiver<VolumeCommand>,
    sink_valid: bool,
    retry_interval: Duration,
) -> Option<VolumeCommand> {
    match receiver.recv_timeout(if sink_valid {
        CONNECTION_RETRY_MAX
    } else {
        retry_interval
    }) {
        Ok(command) => Some(command),
        Err(RecvTimeoutError::Timeout) => Some(if sink_valid {
            VolumeCommand::HealthCheck
        } else {
            VolumeCommand::RefreshDefault
        }),
        Err(RecvTimeoutError::Disconnected) => None,
    }
}

pub(super) enum VolumeCommand {
    SetTarget,
    HealthCheck,
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

fn target_snapshot(target: &RwLock<VolumeTarget>, generation: &AtomicU64) -> (VolumeTarget, u64) {
    let selected = target.read();
    (selected.clone(), generation.load(Ordering::SeqCst))
}

fn publish_current(
    sink: SinkSnapshot,
    target: &RwLock<VolumeTarget>,
    generation: &AtomicU64,
    callback: Option<&VolumeChangeCallback>,
) -> bool {
    let _selected = target.read();
    if !sink.is_current(generation.load(Ordering::SeqCst)) {
        return false;
    }
    publish(sink, callback);
    true
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
    target: &VolumeTarget,
    index: Option<u32>,
) -> Result<SinkSnapshot, String> {
    if !with_mainloop(mainloop, || {
        context.get_state() == libpulse_binding::context::State::Ready
    }) {
        return Err("PulseAudio context is not ready".to_string());
    }
    let index = match index {
        Some(index) => index,
        None => selected_sink_index(mainloop, context, target)?,
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
        introspect.get_sink_info_by_index(index, callback);
    });
    rx.recv_timeout(REQUEST_TIMEOUT)
        .map_err(|_| "Timeout getting sink info".to_string())?
}

fn refresh_default(
    mainloop: &mut Mainloop,
    context: &Context,
    generation: &AtomicU64,
    shared_target: &RwLock<VolumeTarget>,
    target: &VolumeTarget,
    sink: &mut Option<SinkSnapshot>,
    callback: Option<&VolumeChangeCallback>,
) -> Result<(), String> {
    // Clear the old target even if the new default is missing or lookup fails.
    *sink = None;
    let (current_target, expected) = target_snapshot(shared_target, generation);
    if &current_target != target {
        return Err("Output target changed before sink lookup".into());
    }
    let snapshot = query_sink(mainloop, context, expected, target, None)?;
    if !publish_current(snapshot, shared_target, generation, callback) {
        return Err("Output target changed during sink lookup".into());
    }
    *sink = Some(snapshot);
    Ok(())
}

fn publish(sink: SinkSnapshot, callback: Option<&VolumeChangeCallback>) {
    if let Some(callback) = callback {
        let _ = callback.send(VolumeSample {
            revision: sink.generation,
            volume: sink.volume_percent(),
            muted: sink.muted,
        });
    }
}

pub struct LinuxVolumeControl {
    command_tx: Sender<VolumeCommand>,
}

impl LinuxVolumeControl {
    #[allow(clippy::new_ret_no_self)]
    #[allow(clippy::unnecessary_wraps)]
    pub fn new(
        target: Arc<RwLock<VolumeTarget>>,
        generation: Arc<AtomicU64>,
    ) -> (Box<dyn VolumeControlImpl + Send>, Sender<VolumeCommand>) {
        let control = Self::initialize(target, generation);
        let sender = control.command_tx.clone();
        (Box::new(control), sender)
    }

    fn initialize(target: Arc<RwLock<VolumeTarget>>, generation: Arc<AtomicU64>) -> Self {
        let (command_tx, command_rx) = channel::<VolumeCommand>();
        let event_tx = command_tx.clone();
        thread::spawn(move || {
            let mut callback = None;
            let mut delay = DEFAULT_SINK_RETRY_INTERVAL;
            loop {
                match Self::run_connected(
                    &command_rx,
                    &event_tx,
                    &target,
                    &generation,
                    &mut callback,
                ) {
                    Ok(()) => break,
                    Err(error) => {
                        log::debug!(
                            "[VolumeControl] PulseAudio connection unavailable; retrying: {error}"
                        );
                        if !retry_connection(&command_rx, &mut callback, delay) {
                            break;
                        }
                        delay = delay.saturating_mul(2).min(CONNECTION_RETRY_MAX);
                    }
                }
            }
        });
        Self { command_tx }
    }

    fn run_connected(
        command_rx: &Receiver<VolumeCommand>,
        event_tx: &Sender<VolumeCommand>,
        target: &Arc<RwLock<VolumeTarget>>,
        generation: &Arc<AtomicU64>,
        callback: &mut Option<VolumeChangeCallback>,
    ) -> Result<(), String> {
        let Some(mut mainloop) = Mainloop::new() else {
            return Err("Failed to create PulseAudio mainloop".into());
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
            return Err("Failed to create PulseAudio context".into());
        };
        if context
            .connect(None, ContextFlagSet::NOFLAGS, None)
            .is_err()
            || mainloop.start().is_err()
        {
            mainloop.stop();
            context.disconnect();
            return Err("Failed to start PulseAudio connection".into());
        }
        let connection_started = Instant::now();
        loop {
            if connection_started.elapsed() >= REQUEST_TIMEOUT {
                mainloop.stop();
                context.disconnect();
                return Err("Timeout connecting to PulseAudio".into());
            }
            match with_mainloop(&mut mainloop, || context.get_state()) {
                libpulse_binding::context::State::Ready => break,
                libpulse_binding::context::State::Failed
                | libpulse_binding::context::State::Terminated => {
                    mainloop.stop();
                    context.disconnect();
                    return Err("PulseAudio connection failed".into());
                }
                _ => thread::sleep(Duration::from_millis(10)),
            }
        }

        let event_generation = generation.clone();
        let event_tx = event_tx.clone();
        let (tx, rx) = channel();
        with_mainloop(&mut mainloop, || {
            // Install the callback before subscribing so no change is lost.
            context.set_subscribe_callback(Some(Box::new(move |facility, operation, index| {
                if facility == Some(Facility::Server)
                    || (facility == Some(Facility::Sink)
                        && matches!(operation, Some(Operation::New | Operation::Removed)))
                {
                    // Invalidate in the callback, not later on the worker: a
                    // lookup already in flight must not publish the old sink.
                    event_generation.fetch_add(1, Ordering::SeqCst);
                    let _ = event_tx.send(VolumeCommand::RefreshDefault);
                } else if facility == Some(Facility::Sink) && operation == Some(Operation::Changed)
                {
                    let _ = event_tx.send(VolumeCommand::SinkChanged(
                        index,
                        event_generation.load(Ordering::SeqCst),
                    ));
                }
            })));
            context.subscribe(
                InterestMaskSet::SERVER | InterestMaskSet::SINK,
                move |success| {
                    let _ = tx.send(success);
                },
            );
        });
        if rx.recv_timeout(REQUEST_TIMEOUT) != Ok(true) {
            mainloop.stop();
            context.disconnect();
            return Err("Failed to subscribe to PulseAudio events".into());
        }

        let mut sink = None;
        let mut selected_target = target.read().clone();
        let mut last_self_change: Option<(u64, Instant)> = None;
        let _ = refresh_default(
            &mut mainloop,
            &context,
            generation,
            target,
            &selected_target,
            &mut sink,
            callback.as_ref(),
        );
        log::info!("[VolumeControl] Linux PulseAudio volume control initialized");

        // A failed lookup leaves no target. Retry without requiring another
        // server event, but keep an indefinite wait while the target is valid.
        while let Some(command) = receive_command(
            command_rx,
            sink.is_some_and(|s| s.is_current(generation.load(Ordering::SeqCst))),
            DEFAULT_SINK_RETRY_INTERVAL,
        ) {
            if with_mainloop(&mut mainloop, || context.get_state())
                != libpulse_binding::context::State::Ready
            {
                mainloop.stop();
                context.set_subscribe_callback(None);
                context.disconnect();
                return connection_lost_command(command, callback);
            }
            let (latest, latest_generation) = target_snapshot(target, generation);
            if latest != selected_target || !sink.is_some_and(|s| s.is_current(latest_generation)) {
                selected_target = latest;
                sink = None;
                last_self_change = None;
            }
            match command {
                VolumeCommand::Shutdown => break,
                VolumeCommand::HealthCheck => {}
                VolumeCommand::SetTarget | VolumeCommand::RefreshDefault => {
                    let _ = refresh_default(
                        &mut mainloop,
                        &context,
                        generation,
                        target,
                        &selected_target,
                        &mut sink,
                        callback.as_ref(),
                    );
                }
                VolumeCommand::SetChangeCallback(new_callback, response) => {
                    *callback = Some(new_callback);
                    let _ = refresh_default(
                        &mut mainloop,
                        &context,
                        generation,
                        target,
                        &selected_target,
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
                    if let Ok(updated) = query_sink(
                        &mut mainloop,
                        &context,
                        current,
                        &selected_target,
                        Some(index),
                    ) {
                        if publish_current(updated, target, generation, callback.as_ref()) {
                            sink = Some(updated);
                        }
                    }
                }
                command => {
                    if !sink.is_some_and(|s| s.is_current(generation.load(Ordering::SeqCst))) {
                        let _ = refresh_default(
                            &mut mainloop,
                            &context,
                            generation,
                            target,
                            &selected_target,
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
                            let result = Self::read_sink(
                                &mut mainloop,
                                &context,
                                generation,
                                &selected_target,
                                sink,
                            )
                            .map(|s| s.volume_percent());
                            let _ = response.send(result);
                        }
                        VolumeCommand::GetMute(response) => {
                            let result = Self::read_sink(
                                &mut mainloop,
                                &context,
                                generation,
                                &selected_target,
                                sink,
                            )
                            .map(|s| s.muted);
                            let _ = response.send(result);
                        }
                        VolumeCommand::SetVolume(volume, response) => {
                            let result = Self::read_sink(
                                &mut mainloop,
                                &context,
                                generation,
                                &selected_target,
                                sink,
                            )
                            .and_then(|mut selected_sink| {
                                selected_sink.volume.set(
                                    selected_sink.volume.len(),
                                    Volume(Volume::NORMAL.0 * u32::from(volume.min(100)) / 100),
                                );
                                last_self_change = Some((selected_sink.generation, Instant::now()));
                                Self::write_sink(
                                    &mut mainloop,
                                    &context,
                                    generation,
                                    target,
                                    selected_sink,
                                    false,
                                )
                            });
                            let _ = response.send(result);
                        }
                        VolumeCommand::SetMute(muted, response) => {
                            let result = sink.ok_or_else(|| "Sink not found".to_string()).and_then(
                                |mut selected_sink| {
                                    selected_sink.muted = muted;
                                    last_self_change =
                                        Some((selected_sink.generation, Instant::now()));
                                    Self::write_sink(
                                        &mut mainloop,
                                        &context,
                                        generation,
                                        target,
                                        selected_sink,
                                        true,
                                    )
                                },
                            );
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
        Ok(())
    }

    fn read_sink(
        mainloop: &mut Mainloop,
        context: &Context,
        generation: &AtomicU64,
        target: &VolumeTarget,
        sink: Option<SinkSnapshot>,
    ) -> Result<SinkSnapshot, String> {
        let sink = sink.ok_or_else(|| "Sink not found".to_string())?;
        let updated = query_sink(mainloop, context, sink.generation, target, Some(sink.index))?;
        if !updated.is_current(generation.load(Ordering::SeqCst)) {
            return Err("Default sink changed during lookup".to_string());
        }
        Ok(updated)
    }

    fn write_sink(
        mainloop: &mut Mainloop,
        context: &Context,
        generation: &AtomicU64,
        shared_target: &RwLock<VolumeTarget>,
        sink: SinkSnapshot,
        mute: bool,
    ) -> Result<(), String> {
        let (tx, rx) = channel();
        with_mainloop(mainloop, || {
            let _selected = shared_target.read();
            if !sink.is_current(generation.load(Ordering::SeqCst)) {
                return Err("Output target changed before write".to_string());
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
    fn startup_retry_retains_callback_and_answers_unavailable() {
        let (command_tx, command_rx) = channel();
        let (callback_tx, _callback_rx) = channel();
        let (registered_tx, registered_rx) = channel();
        let (available_tx, available_rx) = channel();
        command_tx
            .send(VolumeCommand::SetChangeCallback(callback_tx, registered_tx))
            .unwrap();
        command_tx
            .send(VolumeCommand::IsAvailable(available_tx))
            .unwrap();
        let mut callback = None;
        assert!(retry_connection(
            &command_rx,
            &mut callback,
            Duration::from_millis(5)
        ));
        assert_eq!(registered_rx.try_recv(), Ok(Ok(())));
        assert_eq!(available_rx.try_recv(), Ok(false));
        assert!(callback.is_some());
        assert!(retry_connection(&command_rx, &mut callback, Duration::ZERO));
    }

    #[test]
    fn shutdown_interrupts_connection_backoff() {
        let (tx, rx) = channel();
        tx.send(VolumeCommand::Shutdown).unwrap();
        let mut callback = None;
        let start = Instant::now();
        assert!(!retry_connection(&rx, &mut callback, CONNECTION_RETRY_MAX));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn connection_loss_dispatches_dequeued_commands() {
        let (command_tx, command_rx) = channel();
        let _event_tx = command_tx.clone();
        let mut callback = None;
        let (callback_tx, callback_rx) = channel();
        let retained_callback = callback_tx.clone();
        let (registered_tx, registered_rx) = channel();
        command_tx
            .send(VolumeCommand::SetChangeCallback(callback_tx, registered_tx))
            .unwrap();
        let command = receive_command(&command_rx, true, Duration::ZERO).unwrap();
        assert_eq!(
            connection_lost_command(command, &mut callback),
            Err("PulseAudio connection lost".into())
        );
        assert_eq!(registered_rx.try_recv(), Ok(Ok(())));
        drop(retained_callback);
        assert!(callback.is_some());
        assert!(matches!(
            callback_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        let (available_tx, available_rx) = channel();
        let (volume_tx, volume_rx) = channel();
        let (mute_tx, mute_rx) = channel();
        let (set_volume_tx, set_volume_rx) = channel();
        let (set_mute_tx, set_mute_rx) = channel();
        command_tx
            .send(VolumeCommand::IsAvailable(available_tx))
            .unwrap();
        command_tx
            .send(VolumeCommand::GetVolume(volume_tx))
            .unwrap();
        command_tx.send(VolumeCommand::GetMute(mute_tx)).unwrap();
        command_tx
            .send(VolumeCommand::SetVolume(50, set_volume_tx))
            .unwrap();
        command_tx
            .send(VolumeCommand::SetMute(true, set_mute_tx))
            .unwrap();
        for _ in 0..5 {
            let command = receive_command(&command_rx, true, Duration::ZERO).unwrap();
            assert_eq!(
                connection_lost_command(command, &mut callback),
                Err("PulseAudio connection lost".into())
            );
        }
        assert_eq!(available_rx.try_recv(), Ok(false));
        assert_eq!(
            volume_rx.try_recv(),
            Ok(Err("PulseAudio unavailable".into()))
        );
        assert_eq!(mute_rx.try_recv(), Ok(Err("PulseAudio unavailable".into())));
        assert_eq!(
            set_volume_rx.try_recv(),
            Ok(Err("PulseAudio unavailable".into()))
        );
        assert_eq!(
            set_mute_rx.try_recv(),
            Ok(Err("PulseAudio unavailable".into()))
        );

        command_tx.send(VolumeCommand::Shutdown).unwrap();
        let command = receive_command(&command_rx, true, Duration::ZERO).unwrap();
        assert_eq!(connection_lost_command(command, &mut callback), Ok(()));
        assert!(callback.is_some());
    }

    #[test]
    fn explicit_pcm_never_falls_back_to_default() {
        assert_eq!(
            selected_pcm(&VolumeTarget::Unavailable),
            Err("Opened ALSA output has no volume target".into())
        );
        assert_eq!(
            selected_pcm(&VolumeTarget::Device {
                native_id: "hw:1,0".into()
            }),
            Ok("hw:1,0")
        );
        assert!(selected_pcm(&VolumeTarget::Device {
            native_id: String::new()
        })
        .is_err());
    }

    #[test]
    fn target_command_is_nonblocking_and_changes_invalidate_snapshot() {
        let (tx, rx) = channel();
        tx.send(VolumeCommand::SetTarget).unwrap();
        assert!(matches!(rx.try_recv(), Ok(VolumeCommand::SetTarget)));
        let old = SinkSnapshot {
            index: 7,
            generation: 1,
            volume: ChannelVolumes::default(),
            muted: false,
        };
        assert!(!old.is_current(2));
    }

    #[test]
    fn in_flight_query_cannot_publish_or_write_after_target_publication() {
        use crate::sendspin::volume_control::VolumeController;
        let (command_tx, command_rx) = channel();
        let target = Arc::new(RwLock::new(VolumeTarget::Device {
            native_id: "hw:1,0".into(),
        }));
        let generation = Arc::new(AtomicU64::new(0));
        let controller = VolumeController {
            inner: Arc::new(parking_lot::Mutex::new(Box::new(LinuxVolumeControl {
                command_tx: command_tx.clone(),
            }))),
            target: Arc::clone(&target),
            target_tx: command_tx,
            generation: Arc::clone(&generation),
        };
        let (started_tx, started_rx) = channel();
        let (resume_tx, resume_rx) = channel();
        let (sample_tx, sample_rx) = channel();
        let query = thread::spawn(move || {
            let (_, old_revision) = target_snapshot(&target, &generation);
            started_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            let stale = SinkSnapshot {
                index: 7,
                generation: old_revision,
                volume: ChannelVolumes::default(),
                muted: false,
            };
            let published = publish_current(stale, &target, &generation, Some(&sample_tx));
            let write_allowed = {
                let _selected = target.read();
                stale.is_current(generation.load(Ordering::SeqCst))
            };
            (published, write_allowed)
        });
        started_rx.recv().unwrap();
        controller.set_target(VolumeTarget::Device {
            native_id: "hw:2,0".into(),
        });
        resume_tx.send(()).unwrap();
        assert_eq!(query.join().unwrap(), (false, false));
        assert!(sample_rx.try_recv().is_err());
        assert!(matches!(
            command_rx.try_recv(),
            Ok(VolumeCommand::SetTarget)
        ));
    }

    #[test]
    fn queued_sample_is_rejected_after_target_reuse() {
        use crate::sendspin::volume_control::VolumeController;
        let (command_tx, _command_rx) = channel();
        let controller = VolumeController {
            inner: Arc::new(parking_lot::Mutex::new(Box::new(LinuxVolumeControl {
                command_tx: command_tx.clone(),
            }))),
            target: Arc::new(RwLock::new(VolumeTarget::Device {
                native_id: "hw:1,0".into(),
            })),
            target_tx: command_tx,
            generation: Arc::new(AtomicU64::new(0)),
        };
        let old = VolumeSample {
            revision: 0,
            volume: 42,
            muted: false,
        };
        assert!(controller.is_current_sample(&old));
        controller.set_target(VolumeTarget::Device {
            native_id: "hw:2,0".into(),
        });
        controller.set_target(VolumeTarget::Device {
            native_id: "hw:1,0".into(),
        });
        assert!(!controller.is_current_sample(&old));
        assert!(controller.is_current_sample(&VolumeSample { revision: 2, ..old }));
    }

    #[test]
    fn server_default_requires_one_named_sink() {
        let sinks = [(3, "other".to_string()), (7, "speakers".to_string())];
        assert_eq!(
            unique_named_sink(default_sink_name(Some("speakers")).unwrap(), &sinks),
            Ok(7)
        );
        assert!(default_sink_name(None).is_err());
        assert!(default_sink_name(Some("")).is_err());
        assert!(unique_named_sink("missing", &sinks).is_err());
        assert!(unique_named_sink(
            "speakers",
            &[(7, "speakers".into()), (8, "speakers".into())]
        )
        .is_err());
        assert!(selected_pcm(&VolumeTarget::SystemDefault).is_err());
    }

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
        assert_eq!(
            rx.try_recv(),
            Ok(VolumeSample {
                revision: 2,
                volume: 50,
                muted: true
            })
        );
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
