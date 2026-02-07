//! Linux volume control implementation using `PulseAudio`

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
        eprintln!("[VolumeControl] Linux PulseAudio volume control initialized successfully");
        Some(Box::new(control))
    }

    fn initialize() -> Self {
        let (command_tx, command_rx) = channel::<VolumeCommand>();

        // Spawn a background thread to handle PulseAudio operations
        // This is necessary because PulseAudio types (Mainloop, Context) are not Send
        thread::spawn(move || {
            // Create mainloop
            let Some(mut mainloop) = Mainloop::new() else {
                eprintln!("[VolumeControl] Failed to create PulseAudio mainloop");
                return;
            };

            // Create context
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
                eprintln!("[VolumeControl] Failed to create PulseAudio context");
                return;
            };

            // Connect to PulseAudio server
            if context
                .connect(None, ContextFlagSet::NOFLAGS, None)
                .is_err()
            {
                eprintln!("[VolumeControl] Failed to connect to PulseAudio server");
                return;
            }

            // Start mainloop
            if mainloop.start().is_err() {
                eprintln!("[VolumeControl] Failed to start PulseAudio mainloop");
                return;
            }

            // Wait for context to be ready
            loop {
                match context.get_state() {
                    libpulse_binding::context::State::Ready => break,
                    libpulse_binding::context::State::Failed
                    | libpulse_binding::context::State::Terminated => {
                        eprintln!("[VolumeControl] PulseAudio context failed");
                        return;
                    }
                    _ => thread::sleep(Duration::from_millis(10)),
                }
            }

            eprintln!("[VolumeControl] PulseAudio context ready");

            // Store the default sink index (output device)
            let sink_idx = Arc::new(Mutex::new(None::<u32>));

            // Timestamp of last self-initiated volume change (to prevent feedback loops)
            let last_self_change = Arc::new(AtomicU64::new(0));

            // Get default sink immediately
            let sink_idx_clone = sink_idx.clone();
            let (init_tx, init_rx) = channel();
            let init_tx = Arc::new(Mutex::new(Some(init_tx)));

            let introspect = context.introspect();
            let introspect_clone = context.introspect();
            introspect.get_server_info(move |server_info| {
                if let Some(default_sink_name) = &server_info.default_sink_name {
                    eprintln!("[VolumeControl] Default sink: {:?}", default_sink_name);
                    // Look up the sink by name to get its index
                    let sink_name = default_sink_name.clone();
                    let sink_idx_clone2 = sink_idx_clone.clone();
                    let init_tx_clone = init_tx.clone();
                    introspect_clone.get_sink_info_by_name(&sink_name, move |list_result| {
                        if let libpulse_binding::callbacks::ListResult::Item(sink_info) =
                            list_result
                        {
                            *sink_idx_clone2.lock().unwrap() = Some(sink_info.index);
                            if let Some(tx) = init_tx_clone.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                        }
                    });
                }
            });

            // Wait for initial sink to be found
            let _ = init_rx.recv_timeout(Duration::from_secs(1));

            // Store change callback (if set)
            let change_callback: Arc<Mutex<Option<VolumeChangeCallback>>> =
                Arc::new(Mutex::new(None));

            // Process commands
            while let Ok(command) = command_rx.recv() {
                match command {
                    VolumeCommand::SetVolume(volume, response_tx) => {
                        // Record timestamp to prevent feedback loop
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        last_self_change.store(now, Ordering::Relaxed);

                        let result = Self::handle_set_volume(&context, &sink_idx, volume);
                        let _ = response_tx.send(result);
                    }
                    VolumeCommand::SetMute(muted, response_tx) => {
                        // Record timestamp to prevent feedback loop
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        last_self_change.store(now, Ordering::Relaxed);

                        let result = Self::handle_set_mute(&context, &sink_idx, muted);
                        let _ = response_tx.send(result);
                    }
                    VolumeCommand::GetVolume(response_tx) => {
                        let result = Self::handle_get_volume(&context, &sink_idx);
                        let _ = response_tx.send(result);
                    }
                    VolumeCommand::GetMute(response_tx) => {
                        let result = Self::handle_get_mute(&context, &sink_idx);
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
                            &sink_idx,
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

            // Cleanup
            mainloop.stop();
            context.disconnect();
        });

        Self { command_tx }
    }

    fn handle_set_volume(
        context: &Context,
        sink_idx: &Arc<Mutex<Option<u32>>>,
        volume: u8,
    ) -> Result<(), String> {
        use libpulse_binding::volume::ChannelVolumes;

        let idx = *sink_idx.lock().unwrap();
        if idx.is_none() {
            return Err("Sink not found".to_string());
        }

        let idx = idx.unwrap();

        let (result_tx, result_rx) = channel::<Result<ChannelVolumes, String>>();
        let result_tx = Arc::new(Mutex::new(Some(result_tx)));

        // Get current sink info to determine channel count
        let result_tx_clone = result_tx.clone();
        let introspect = context.introspect();
        introspect.get_sink_info_by_index(idx, move |result| {
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
            .map_err(|_| "Timeout getting sink info".to_string())??;

        // Set the sink volume
        let (set_result_tx, set_result_rx) = channel();
        let set_result_tx = Arc::new(Mutex::new(Some(set_result_tx)));

        let mut introspect = context.introspect();
        introspect.set_sink_volume_by_index(
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
            .map_err(|_| "Timeout setting volume".to_string())?;

        if success {
            Ok(())
        } else {
            Err("Failed to set volume".to_string())
        }
    }

    fn handle_set_mute(
        context: &Context,
        sink_idx: &Arc<Mutex<Option<u32>>>,
        muted: bool,
    ) -> Result<(), String> {
        let idx = *sink_idx.lock().unwrap();
        if idx.is_none() {
            return Err("Sink not found".to_string());
        }

        let idx = idx.unwrap();

        // Set the sink mute state
        let (result_tx, result_rx) = channel();
        let result_tx = Arc::new(Mutex::new(Some(result_tx)));

        let mut introspect = context.introspect();
        introspect.set_sink_mute_by_index(
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
            .map_err(|_| "Timeout setting mute".to_string())?;

        if success {
            Ok(())
        } else {
            Err("Failed to set mute".to_string())
        }
    }

    fn handle_get_volume(
        context: &Context,
        sink_idx: &Arc<Mutex<Option<u32>>>,
    ) -> Result<u8, String> {
        let idx = *sink_idx.lock().unwrap();
        if idx.is_none() {
            return Err("Sink not found".to_string());
        }

        let idx = idx.unwrap();

        // Get the sink volume
        let (result_tx, result_rx) = channel();
        let result_tx = Arc::new(Mutex::new(Some(result_tx)));

        let introspect = context.introspect();
        introspect.get_sink_info_by_index(idx, move |result| {
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
            .map_err(|_| "Timeout getting volume".to_string())
    }

    fn handle_get_mute(
        context: &Context,
        sink_idx: &Arc<Mutex<Option<u32>>>,
    ) -> Result<bool, String> {
        let idx = *sink_idx.lock().unwrap();
        if idx.is_none() {
            return Err("Sink not found".to_string());
        }

        let idx = idx.unwrap();

        // Get the sink mute state
        let (result_tx, result_rx) = channel();
        let result_tx = Arc::new(Mutex::new(Some(result_tx)));

        let introspect = context.introspect();
        introspect.get_sink_info_by_index(idx, move |result| {
            if let libpulse_binding::callbacks::ListResult::Item(info) = result {
                if let Some(tx) = result_tx.lock().unwrap().take() {
                    let _ = tx.send(info.mute);
                }
            }
        });

        result_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "Timeout getting mute state".to_string())
    }

    fn handle_set_change_callback(
        context: &mut Context,
        sink_idx: &Arc<Mutex<Option<u32>>>,
        change_callback: &Arc<Mutex<Option<VolumeChangeCallback>>>,
        callback: VolumeChangeCallback,
        last_self_change: &Arc<AtomicU64>,
    ) -> Result<(), String> {
        // Store the callback
        *change_callback.lock().unwrap() = Some(callback);

        let idx = *sink_idx.lock().unwrap();
        if idx.is_none() {
            return Err("Sink not found".to_string());
        }

        // Subscribe to sink events
        let interest = InterestMaskSet::SINK;
        let (result_tx, result_rx) = channel();
        let result_tx = Arc::new(Mutex::new(Some(result_tx)));

        context.subscribe(interest, move |success| {
            if let Some(tx) = result_tx.lock().unwrap().take() {
                let _ = tx.send(success);
            }
        });

        let success = result_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "Timeout subscribing to events".to_string())?;

        if !success {
            return Err("Failed to subscribe to sink events".to_string());
        }

        // Set up subscription callback
        let sink_idx_clone = sink_idx.clone();
        let change_callback_clone = change_callback.clone();
        let last_self_change_clone = last_self_change.clone();
        let introspect = context.introspect();

        context.set_subscribe_callback(Some(Box::new(move |facility, operation, idx| {
            const SELF_CHANGE_GRACE_PERIOD: u64 = 200; // milliseconds

            // Only handle sink changes
            if facility != Some(Facility::Sink) {
                return;
            }

            // Check if this is our sink
            let our_idx = *sink_idx_clone.lock().unwrap();
            if our_idx != Some(idx) {
                return;
            }

            // Only handle change operations
            if operation != Some(Operation::Changed) {
                return;
            }

            // Check if this change was self-initiated (within grace period)
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let last_self_ms = last_self_change_clone.load(Ordering::Relaxed);
            if now_ms.saturating_sub(last_self_ms) < SELF_CHANGE_GRACE_PERIOD {
                // Skip notification - this was triggered by our own volume change
                return;
            }

            // Query the sink to get updated volume/mute
            let callback_clone = change_callback_clone.clone();
            introspect.get_sink_info_by_index(idx, move |result| {
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

        eprintln!("[VolumeControl] Linux PulseAudio sink volume change listener registered");
        Ok(())
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
