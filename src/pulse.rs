// SPDX-License-Identifier: GPL-3.0

//! PulseAudio sink-input monitor for per-application volume control.
//!
//! Runs PulseAudio mainloop in a dedicated thread. Sends snapshots of
//! active app streams to the applet. Receives volume/mute commands.

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::subscribe::{Facility, InterestMaskSet};
use pulse::context::{Context, FlagSet};
use pulse::mainloop::standard::Mainloop;
use pulse::operation::State as OpState;
use pulse::volume::{ChannelVolumes, Volume};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::mpsc as tokio_mpsc;

/// Info about a single app's audio stream.
#[derive(Debug, Clone)]
pub struct AppStream {
    pub index: u32,
    pub app_name: String,
    pub icon_name: Option<String>,
    pub volume_percent: u32,
    pub muted: bool,
    pub sink_index: u32,
}

/// Info about a single app's recording (input) stream.
#[derive(Debug, Clone)]
pub struct AppRecordStream {
    pub index: u32,
    pub app_name: String,
    pub icon_name: Option<String>,
    pub volume_percent: u32,
    pub muted: bool,
    pub source_index: u32,
}

/// Info about an output device (sink).
#[derive(Debug, Clone)]
pub struct SinkInfo {
    pub index: u32,
    pub name: String,
    pub description: String,
}

/// Info about an input device (source).
#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub index: u32,
    pub name: String,
    pub description: String,
}

/// Snapshot of all active app streams + master volume + available sinks.
#[derive(Debug, Clone)]
pub struct StreamSnapshot {
    pub apps: Vec<AppStream>,
    pub recorders: Vec<AppRecordStream>,
    pub sinks: Vec<SinkInfo>,
    pub sources: Vec<SourceInfo>,
    pub master_volume: u32,
    pub master_muted: bool,
}

/// Commands from the UI to PulseAudio.
#[derive(Debug)]
pub enum PulseCommand {
    SetAppVolume(u32, u32),
    SetAppMute(u32, bool),
    SetAppSink(u32, u32),
    SetRecVolume(u32, u32),
    SetRecMute(u32, bool),
    SetRecSource(u32, u32),
}

fn vol_to_percent(cv: &ChannelVolumes) -> u32 {
    let avg = cv.avg();
    let norm = Volume::NORMAL.0 as f64;
    let pct = (avg.0 as f64 / norm * 100.0).round() as u32;
    pct.min(150)
}

fn percent_to_vol(percent: u32, channels: u8) -> ChannelVolumes {
    let norm = Volume::NORMAL.0 as f64;
    let val = (percent as f64 / 100.0 * norm).round() as u32;
    let mut cv = ChannelVolumes::default();
    cv.set_len(channels);
    cv.set(channels, Volume(val));
    cv
}

use once_cell::sync::Lazy;
use std::sync::Mutex;

static PULSE_TX: Lazy<Mutex<Option<std::sync::mpsc::Sender<PulseCommand>>>> =
    Lazy::new(|| Mutex::new(None));
static SNAPSHOT_RX: Lazy<Mutex<Option<tokio_mpsc::UnboundedReceiver<StreamSnapshot>>>> =
    Lazy::new(|| Mutex::new(None));

/// Start the PulseAudio monitor (idempotent). Stores the channels in globals
/// so the subscription can pick them up.
pub fn start() {
    let mut tx_guard = PULSE_TX.lock().unwrap();
    if tx_guard.is_some() {
        return;
    }

    let (snapshot_tx, snapshot_rx) = tokio_mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        run_pulse_thread(snapshot_tx, cmd_rx);
    });

    *tx_guard = Some(cmd_tx);
    *SNAPSHOT_RX.lock().unwrap() = Some(snapshot_rx);
}

/// Send a command to PulseAudio.
pub fn send_command(cmd: PulseCommand) {
    if let Some(tx) = PULSE_TX.lock().unwrap().as_ref() {
        let _ = tx.send(cmd);
    }
}

/// Take ownership of the snapshot receiver (call once from the subscription).
pub fn take_receiver() -> Option<tokio_mpsc::UnboundedReceiver<StreamSnapshot>> {
    SNAPSHOT_RX.lock().unwrap().take()
}

/// An iced subscription that streams snapshots from the PulseAudio thread.
pub fn subscription() -> cosmic::iced::Subscription<StreamSnapshot> {
    use cosmic::iced::futures::stream;
    cosmic::iced::Subscription::run(|| {
        stream::unfold(take_receiver(), |rx| async move {
            match rx {
                Some(mut r) => match r.recv().await {
                    Some(snap) => Some((snap, Some(r))),
                    None => None,
                },
                None => {
                    // No receiver — just sleep forever
                    tokio::time::sleep(std::time::Duration::from_secs(60 * 60 * 24)).await;
                    None
                }
            }
        })
    })
}

fn run_pulse_thread(
    snapshot_tx: tokio_mpsc::UnboundedSender<StreamSnapshot>,
    cmd_rx: std::sync::mpsc::Receiver<PulseCommand>,
) {
    let mut ml = match Mainloop::new() {
        Some(ml) => ml,
        None => {
            tracing::error!("Failed to create PulseAudio mainloop");
            return;
        }
    };

    let mut ctx = match Context::new(&ml, "cosmic-app-volume") {
        Some(ctx) => ctx,
        None => {
            tracing::error!("Failed to create PulseAudio context");
            return;
        }
    };

    if ctx.connect(None, FlagSet::NOFLAGS, None).is_err() {
        tracing::error!("Failed to connect to PulseAudio");
        return;
    }

    // Wait for context to be ready
    loop {
        match ml.iterate(true) {
            pulse::mainloop::standard::IterateResult::Success(_) => {}
            _ => {
                tracing::error!("PulseAudio mainloop iterate failed");
                return;
            }
        }
        match ctx.get_state() {
            pulse::context::State::Ready => break,
            pulse::context::State::Failed | pulse::context::State::Terminated => {
                tracing::error!("PulseAudio context failed");
                return;
            }
            _ => {}
        }
    }

    tracing::info!("Connected to PulseAudio");

    // Shared state for collecting sink input info
    let apps: Rc<RefCell<HashMap<u32, AppStream>>> = Rc::new(RefCell::new(HashMap::new()));
    let recorders: Rc<RefCell<HashMap<u32, AppRecordStream>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let sinks: Rc<RefCell<Vec<SinkInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let sources: Rc<RefCell<Vec<SourceInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let master_vol: Rc<RefCell<(u32, bool)>> = Rc::new(RefCell::new((100, false)));
    let needs_refresh: Rc<RefCell<bool>> = Rc::new(RefCell::new(true));
    let default_sink: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    // Subscribe to sink input and sink changes
    {
        let needs_refresh = Rc::clone(&needs_refresh);
        ctx.set_subscribe_callback(Some(Box::new(move |facility, _op, _idx| {
            if let Some(f) = facility {
                match f {
                    Facility::SinkInput
                    | Facility::Sink
                    | Facility::SourceOutput
                    | Facility::Source => {
                        *needs_refresh.borrow_mut() = true;
                    }
                    _ => {}
                }
            }
        })));
    }
    ctx.subscribe(
        InterestMaskSet::SINK_INPUT
            | InterestMaskSet::SINK
            | InterestMaskSet::SOURCE_OUTPUT
            | InterestMaskSet::SOURCE,
        |_success| {},
    );

    // Do initial query for default sink name
    {
        let default_sink = Rc::clone(&default_sink);
        ctx.introspect().get_server_info(move |info| {
            if let Some(name) = &info.default_sink_name {
                *default_sink.borrow_mut() = name.to_string();
            }
        });
    }

    // Main loop
    let mut last_periodic_refresh = std::time::Instant::now();
    loop {
        // Non-blocking iterate
        match ml.iterate(false) {
            pulse::mainloop::standard::IterateResult::Success(_) => {}
            _ => break,
        }

        // Periodic refresh every 1 second to catch missed events
        if last_periodic_refresh.elapsed() > std::time::Duration::from_secs(1) {
            *needs_refresh.borrow_mut() = true;
            last_periodic_refresh = std::time::Instant::now();
        }

        // Process commands from UI
        while let Ok(cmd) = cmd_rx.try_recv() {
            let mut introspect = ctx.introspect();
            match cmd {
                PulseCommand::SetAppVolume(idx, percent) => {
                    let cv = percent_to_vol(percent, 2);
                    introspect.set_sink_input_volume(idx, &cv, None);
                }
                PulseCommand::SetAppMute(idx, mute) => {
                    introspect.set_sink_input_mute(idx, mute, None);
                }
                PulseCommand::SetAppSink(input_idx, sink_idx) => {
                    introspect.move_sink_input_by_index(input_idx, sink_idx, None);
                }
                PulseCommand::SetRecVolume(idx, percent) => {
                    let cv = percent_to_vol(percent, 2);
                    introspect.set_source_output_volume(idx, &cv, None);
                }
                PulseCommand::SetRecMute(idx, mute) => {
                    introspect.set_source_output_mute(idx, mute, None);
                }
                PulseCommand::SetRecSource(out_idx, src_idx) => {
                    introspect.move_source_output_by_index(out_idx, src_idx, None);
                }
            }
            *needs_refresh.borrow_mut() = true;
        }

        // Refresh if needed
        if *needs_refresh.borrow() {
            *needs_refresh.borrow_mut() = false;

            // Query default sink name
            {
                let default_sink = Rc::clone(&default_sink);
                let op = ctx.introspect().get_server_info(move |info| {
                    if let Some(name) = &info.default_sink_name {
                        *default_sink.borrow_mut() = name.to_string();
                    }
                });
                while op.get_state() == OpState::Running {
                    if matches!(
                        ml.iterate(true),
                        pulse::mainloop::standard::IterateResult::Quit(_)
                            | pulse::mainloop::standard::IterateResult::Err(_)
                    ) {
                        break;
                    }
                }
            }

            // Query default sink volume
            {
                let sink_name = default_sink.borrow().clone();
                if !sink_name.is_empty() {
                    let master_vol = Rc::clone(&master_vol);
                    let op = ctx.introspect().get_sink_info_by_name(
                        &sink_name,
                        move |result| {
                            if let ListResult::Item(info) = result {
                                let pct = vol_to_percent(&info.volume);
                                *master_vol.borrow_mut() = (pct, info.mute);
                            }
                        },
                    );
                    while op.get_state() == OpState::Running {
                        if matches!(
                            ml.iterate(true),
                            pulse::mainloop::standard::IterateResult::Quit(_)
                                | pulse::mainloop::standard::IterateResult::Err(_)
                        ) {
                            break;
                        }
                    }
                }
            }

            // Query all sink inputs
            {
                let apps_inner = Rc::clone(&apps);
                apps_inner.borrow_mut().clear();
                let op = ctx.introspect().get_sink_input_info_list(move |result| {
                    if let ListResult::Item(info) = result {
                        // Filter out system event sounds (volume change beeps, etc.)
                        let media_role = info.proplist.get_str("media.role");
                        if matches!(media_role.as_deref(), Some("event")) {
                            return;
                        }
                        // Filter out known noise binaries
                        let binary = info.proplist.get_str("application.process.binary");
                        if let Some(b) = binary.as_deref() {
                            if matches!(
                                b,
                                "pw-play" | "paplay" | "canberra-gtk-play" | "aplay"
                            ) {
                                return;
                            }
                        }
                        // Filter out streams flagged as not visible to volume controls
                        if matches!(
                            info.proplist.get_str("application.id").as_deref(),
                            Some("org.PulseAudio.pavucontrol")
                        ) {
                            return;
                        }

                        let app_name = info
                            .proplist
                            .get_str("application.name")
                            .or_else(|| info.proplist.get_str("application.process.binary"))
                            .or_else(|| info.proplist.get_str("media.name"))
                            .unwrap_or_else(|| {
                                info.name
                                    .as_ref()
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| format!("Stream {}", info.index))
                            });
                        let icon_name = info
                            .proplist
                            .get_str("application.icon_name")
                            .or_else(|| info.proplist.get_str("application.process.binary"))
                            .map(|s| s.to_lowercase());
                        let pct = vol_to_percent(&info.volume);
                        apps_inner.borrow_mut().insert(
                            info.index,
                            AppStream {
                                index: info.index,
                                app_name,
                                icon_name,
                                volume_percent: pct,
                                muted: info.mute,
                                sink_index: info.sink,
                            },
                        );
                    }
                });
                while op.get_state() == OpState::Running {
                    if matches!(
                        ml.iterate(true),
                        pulse::mainloop::standard::IterateResult::Quit(_)
                            | pulse::mainloop::standard::IterateResult::Err(_)
                    ) {
                        break;
                    }
                }
            }

            // Query all sinks (output devices)
            {
                let sinks_inner = Rc::clone(&sinks);
                sinks_inner.borrow_mut().clear();
                let op = ctx.introspect().get_sink_info_list(move |result| {
                    if let ListResult::Item(info) = result {
                        let name = info
                            .name
                            .as_ref()
                            .map(|s| s.to_string())
                            .unwrap_or_default();
                        let description = info
                            .description
                            .as_ref()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| name.clone());
                        sinks_inner.borrow_mut().push(SinkInfo {
                            index: info.index,
                            name,
                            description,
                        });
                    }
                });
                while op.get_state() == OpState::Running {
                    if matches!(
                        ml.iterate(true),
                        pulse::mainloop::standard::IterateResult::Quit(_)
                            | pulse::mainloop::standard::IterateResult::Err(_)
                    ) {
                        break;
                    }
                }
            }

            // Query all sources (input devices) — filter out monitor sources
            {
                let sources_inner = Rc::clone(&sources);
                sources_inner.borrow_mut().clear();
                let op = ctx.introspect().get_source_info_list(move |result| {
                    if let ListResult::Item(info) = result {
                        // Skip monitor sources (these are sink monitors, not real inputs)
                        if info.monitor_of_sink.is_some() {
                            return;
                        }
                        let name = info
                            .name
                            .as_ref()
                            .map(|s| s.to_string())
                            .unwrap_or_default();
                        let description = info
                            .description
                            .as_ref()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| name.clone());
                        sources_inner.borrow_mut().push(SourceInfo {
                            index: info.index,
                            name,
                            description,
                        });
                    }
                });
                while op.get_state() == OpState::Running {
                    if matches!(
                        ml.iterate(true),
                        pulse::mainloop::standard::IterateResult::Quit(_)
                            | pulse::mainloop::standard::IterateResult::Err(_)
                    ) {
                        break;
                    }
                }
            }

            // Query all source outputs (apps recording)
            {
                let recorders_inner = Rc::clone(&recorders);
                recorders_inner.borrow_mut().clear();
                let op = ctx.introspect().get_source_output_info_list(move |result| {
                    if let ListResult::Item(info) = result {
                        // Filter out system event sounds
                        let media_role = info.proplist.get_str("media.role");
                        if matches!(media_role.as_deref(), Some("event")) {
                            return;
                        }
                        // Filter known noise binaries
                        let binary = info.proplist.get_str("application.process.binary");
                        if let Some(b) = binary.as_deref() {
                            if matches!(
                                b,
                                "pw-record" | "parec" | "pavucontrol" | "cosmic-applet-audio"
                            ) {
                                return;
                            }
                        }

                        let app_name = info
                            .proplist
                            .get_str("application.name")
                            .or_else(|| info.proplist.get_str("application.process.binary"))
                            .or_else(|| info.proplist.get_str("media.name"))
                            .unwrap_or_else(|| {
                                info.name
                                    .as_ref()
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| format!("Stream {}", info.index))
                            });
                        let icon_name = info
                            .proplist
                            .get_str("application.icon_name")
                            .or_else(|| info.proplist.get_str("application.process.binary"))
                            .map(|s| s.to_lowercase());
                        let pct = vol_to_percent(&info.volume);
                        recorders_inner.borrow_mut().insert(
                            info.index,
                            AppRecordStream {
                                index: info.index,
                                app_name,
                                icon_name,
                                volume_percent: pct,
                                muted: info.mute,
                                source_index: info.source,
                            },
                        );
                    }
                });
                while op.get_state() == OpState::Running {
                    if matches!(
                        ml.iterate(true),
                        pulse::mainloop::standard::IterateResult::Quit(_)
                            | pulse::mainloop::standard::IterateResult::Err(_)
                    ) {
                        break;
                    }
                }
            }

            // Send snapshot
            let mv = *master_vol.borrow();
            let snapshot = StreamSnapshot {
                apps: {
                    let mut v: Vec<_> = apps.borrow().values().cloned().collect();
                    v.sort_by(|a, b| a.app_name.to_lowercase().cmp(&b.app_name.to_lowercase()));
                    v
                },
                recorders: {
                    let mut v: Vec<_> = recorders.borrow().values().cloned().collect();
                    v.sort_by(|a, b| a.app_name.to_lowercase().cmp(&b.app_name.to_lowercase()));
                    v
                },
                sinks: sinks.borrow().clone(),
                sources: sources.borrow().clone(),
                master_volume: mv.0,
                master_muted: mv.1,
            };
            let _ = snapshot_tx.send(snapshot);
        }

        // Small sleep to avoid busy-looping
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
