//! Linux volume control implementation using `PulseAudio` targeting sink-inputs

use super::{VolumeChangeCallback, VolumeControlImpl};
use libpulse_binding::{
    callbacks::ListResult,
    context::{
        subscribe::{Facility, InterestMaskSet, Operation},
        Context, FlagSet as ContextFlagSet,
    },
    mainloop::threaded::Mainloop,
    proplist::Proplist,
    volume::Volume,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Commands sent from the main thread interface to the background `PulseAudio` worker thread.
enum VolumeCommand {
    SetVolume(u8, Sender<Result<(), String>>),
    SetMute(bool, Sender<Result<(), String>>),
    GetVolume(Sender<Result<u8, String>>),
    GetMute(Sender<Result<bool, String>>),
    IsAvailable(Sender<bool>),
    SetChangeCallback(VolumeChangeCallback, Sender<Result<(), String>>),
    Shutdown,
}

pub struct LinuxVolumeControl {
    command_tx: Sender<VolumeCommand>,
}

impl LinuxVolumeControl {
    #[allow(clippy::new_ret_no_self)]
    #[allow(clippy::unnecessary_wraps)]
    pub fn new() -> Option<Box<dyn VolumeControlImpl + Send>> {
        let control = Self::initialize();
        log::info!("[VolumeControl] Linux PulseAudio volume control initialized successfully");
        Some(Box::new(control))
    }

    fn initialize() -> Self {
        let (command_tx, command_rx) = channel::<VolumeCommand>();

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
            {
                log::error!("[VolumeControl] Failed to connect to PulseAudio server");
                return;
            }

            if mainloop.start().is_err() {
                log::error!("[VolumeControl] Failed to start PulseAudio mainloop");
                return;
            }

            loop {
                match context.get_state() {
                    libpulse_binding::context::State::Ready => break,
                    libpulse_binding::context::State::Failed
                    | libpulse_binding::context::State::Terminated => {
                        log::error!("[VolumeControl] PulseAudio context failed");
                        return;
                    }
                    _ => thread::sleep(Duration::from_millis(10)),
                }
            }

            log::info!("[VolumeControl] PulseAudio context ready");

            let sink_input_idx = Arc::new(Mutex::new(None::<u32>));
            let last_self_change = Arc::new(AtomicU64::new(0));

            // Helper closure to look up our specific app stream's sink-input index via its Process ID (PID)
            let sink_input_idx_clone = sink_input_idx.clone();
            let find_sink_input = move |ctx: &Context| {
                let introspect = ctx.introspect();
                let my_pid = std::process::id();
                let (tx, rx) = channel();

                // Introspect active audio streams (sink-inputs) to locate our application
                introspect.get_sink_input_info_list(move |result| {
                    match result {
                        ListResult::Item(info) => {
                            // Check if this stream's process ID matches our own app PID
                            if let Some(pid_str) = info.proplist.get_str("application.process.id") {
                                if let Ok(pid) = pid_str.parse::<u32>() {
                                    if pid == my_pid {
                                        let _ = tx.send(Some(info.index));
                                    }
                                }
                            }
                        }
                        ListResult::End | ListResult::Error => {
                            let _ = tx.send(None);
                        }
                    }
                });

                if let Ok(Some(idx)) = rx.recv_timeout(Duration::from_secs(1)) {
                    *sink_input_idx_clone.lock().unwrap() = Some(idx);
                }
            };

            find_sink_input(&context);

            let change_callback: Arc<Mutex<Option<VolumeChangeCallback>>> =
                Arc::new(Mutex::new(None));

            // Main event loop handling command requests from the API interface
            while let Ok(command) = command_rx.recv() {
                if sink_input_idx.lock().unwrap().is_none() {
                    find_sink_input(&context);
                }

                match command {
                    VolumeCommand::SetVolume(volume, response_tx) => {
                        // Mark timestamp of self-induced change to prevent feedback loops in change listeners
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        last_self_change.store(now, Ordering::Relaxed);

                        let result = Self::handle_set_volume(&context, &sink_input_idx, volume);
                        let _ = response_tx.send(result);
                    }
                    VolumeCommand::SetMute(muted, response_tx) => {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        last_self_change.store(now, Ordering::Relaxed);

                        let result = Self::handle_set_mute(&context, &sink_input_idx, muted);
                        let _ = response_tx.send(result);
                    }
                    VolumeCommand::GetVolume(response_tx) => {
                        let result = Self::handle_get_volume(&context, &sink_input_idx);
                        let _ = response_tx.send(result);
                    }
                    VolumeCommand::GetMute(response_tx) => {
                        let result = Self::handle_get_mute(&context, &sink_input_idx);
                        let _ = response_tx.send(result);
                    }
                    VolumeCommand::IsAvailable(response_tx) => {
                        let available =
                            context.get_state() == libpulse_binding::context::State::Ready;
                        let _ = response_tx.send(available);
                    }
                    VolumeCommand::SetChangeCallback(callback, response_tx) => {
                        let result = Self::handle_set_change_callback(
                            &mut context,
                            &sink_input_idx,
                            &change_callback,
                            callback,
                            &last_self_change,
                        );
                        let _ = response_tx.send(result);
                    }
                    VolumeCommand::Shutdown => {
                        break;
                    }
                }
            }

            mainloop.stop();
            context.disconnect();
        });

        Self { command_tx }
    }

    /// Fetches current stream volume properties, computes the target level, and applies it to the sink-input.
    fn handle_set_volume(
        context: &Context,
        sink_input_idx: &Arc<Mutex<Option<u32>>>,
        volume: u8,
    ) -> Result<(), String> {
        use libpulse_binding::volume::ChannelVolumes;

        let idx = *sink_input_idx.lock().unwrap();
        let idx =
            idx.ok_or_else(|| "Sink-input stream not found (is audio playing?)".to_string())?;

        let (result_tx, result_rx) = channel::<Result<ChannelVolumes, String>>();
        let result_tx = Arc::new(Mutex::new(Some(result_tx)));

        let result_tx_clone = result_tx.clone();
        let introspect = context.introspect();

        // Query current stream details to preserve correct channel layout mapping
        introspect.get_sink_input_info(idx, move |result| {
            if let libpulse_binding::callbacks::ListResult::Item(info) = result {
                let mut new_volume = info.volume;
                let volume_norm = Volume(Volume::NORMAL.0 * u32::from(volume) / 100);
                new_volume.set(new_volume.len(), volume_norm);

                if let Some(tx) = result_tx_clone.lock().unwrap().take() {
                    let _ = tx.send(Ok(new_volume));
                }
            }
        });

        let new_volume = result_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "Timeout getting sink-input info".to_string())??;

        let (set_result_tx, set_result_rx) = channel();
        let set_result_tx = Arc::new(Mutex::new(Some(set_result_tx)));

        let mut introspect = context.introspect();
        introspect.set_sink_input_volume(
            idx,
            &new_volume,
            Some(Box::new(move |success| {
                if let Some(tx) = set_result_tx.lock().unwrap().take() {
                    let _ = tx.send(success);
                }
            })),
        );

        let success = set_result_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "Timeout setting sink-input volume".to_string())?;

        if success {
            Ok(())
        } else {
            Err("Failed to set sink-input volume".to_string())
        }
    }

    fn handle_set_mute(
        context: &Context,
        sink_input_idx: &Arc<Mutex<Option<u32>>>,
        muted: bool,
    ) -> Result<(), String> {
        let idx = *sink_input_idx.lock().unwrap();
        let idx = idx.ok_or_else(|| "Sink-input stream not found".to_string())?;

        let (result_tx, result_rx) = channel();
        let result_tx = Arc::new(Mutex::new(Some(result_tx)));

        let mut introspect = context.introspect();
        introspect.set_sink_input_mute(
            idx,
            muted,
            Some(Box::new(move |success| {
                if let Some(tx) = result_tx.lock().unwrap().take() {
                    let _ = tx.send(success);
                }
            })),
        );

        let success = result_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "Timeout setting sink-input mute".to_string())?;

        if success {
            Ok(())
        } else {
            Err("Failed to set sink-input mute".to_string())
        }
    }

    fn handle_get_volume(
        context: &Context,
        sink_input_idx: &Arc<Mutex<Option<u32>>>,
    ) -> Result<u8, String> {
        let idx = *sink_input_idx.lock().unwrap();
        let idx = idx.ok_or_else(|| "Sink-input stream not found".to_string())?;

        let (result_tx, result_rx) = channel();
        let result_tx = Arc::new(Mutex::new(Some(result_tx)));

        let introspect = context.introspect();
        introspect.get_sink_input_info(idx, move |result| {
            if let libpulse_binding::callbacks::ListResult::Item(info) = result {
                let avg_volume = info.volume.avg();
                let volume_percent = (avg_volume.0 * 100 / Volume::NORMAL.0) as u8;
                if let Some(tx) = result_tx.lock().unwrap().take() {
                    let _ = tx.send(volume_percent);
                }
            }
        });

        result_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "Timeout getting sink-input volume".to_string())
    }

    fn handle_get_mute(
        context: &Context,
        sink_input_idx: &Arc<Mutex<Option<u32>>>,
    ) -> Result<bool, String> {
        let idx = *sink_input_idx.lock().unwrap();
        let idx = idx.ok_or_else(|| "Sink-input stream not found".to_string())?;

        let (result_tx, result_rx) = channel();
        let result_tx = Arc::new(Mutex::new(Some(result_tx)));

        let introspect = context.introspect();
        introspect.get_sink_input_info(idx, move |result| {
            if let libpulse_binding::callbacks::ListResult::Item(info) = result {
                if let Some(tx) = result_tx.lock().unwrap().take() {
                    let _ = tx.send(info.mute);
                }
            }
        });

        result_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "Timeout getting sink-input mute state".to_string())
    }

    /// Registers a subscription callback to notify upstream components when volume/mute changes externally.
    fn handle_set_change_callback(
        context: &mut Context,
        sink_input_idx: &Arc<Mutex<Option<u32>>>,
        change_callback: &Arc<Mutex<Option<VolumeChangeCallback>>>,
        callback: VolumeChangeCallback,
        last_self_change: &Arc<AtomicU64>,
    ) -> Result<(), String> {
        *change_callback.lock().unwrap() = Some(callback);

        let interest = InterestMaskSet::SINK_INPUT;
        let (result_tx, result_rx) = channel();
        let result_tx = Arc::new(Mutex::new(Some(result_tx)));

        context.subscribe(interest, move |success| {
            if let Some(tx) = result_tx.lock().unwrap().take() {
                let _ = tx.send(success);
            }
        });

        let success = result_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "Timeout subscribing to sink-input events".to_string())?;

        if !success {
            return Err("Failed to subscribe to sink-input events".to_string());
        }

        let sink_idx_clone = sink_input_idx.clone();
        let change_callback_clone = change_callback.clone();
        let last_self_change_clone = last_self_change.clone();
        let introspect = context.introspect();

        context.set_subscribe_callback(Some(Box::new(move |facility, operation, idx| {
            const SELF_CHANGE_GRACE_PERIOD: u64 = 200; // ms

            if facility != Some(Facility::SinkInput) {
                return;
            }

            let our_idx = *sink_idx_clone.lock().unwrap();
            if our_idx != Some(idx) {
                return;
            }

            if operation != Some(Operation::Changed) {
                return;
            }

            // Ignore events triggered shortly after our own volume writes to prevent notification loops
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let last_self_ms = last_self_change_clone.load(Ordering::Relaxed);
            if now_ms.saturating_sub(last_self_ms) < SELF_CHANGE_GRACE_PERIOD {
                return;
            }

            let callback_clone = change_callback_clone.clone();
            introspect.get_sink_input_info(idx, move |result| {
                if let ListResult::Item(info) = result {
                    let avg_volume = info.volume.avg();
                    let volume_percent = (avg_volume.0 * 100 / Volume::NORMAL.0) as u8;
                    let muted = info.mute;

                    if let Some(ref cb) = *callback_clone.lock().unwrap() {
                        let _ = cb.send((volume_percent, muted));
                    }
                }
            });
        })));

        log::info!("[VolumeControl] Linux PulseAudio sink-input volume change listener registered");
        Ok(())
    }
}

// Public trait implementations routing commands safely across threads
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
        response_rx
            .recv_timeout(Duration::from_millis(500))
            .unwrap_or(false)
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
