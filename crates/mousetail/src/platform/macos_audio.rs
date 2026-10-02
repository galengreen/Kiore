//! Plays sound from another computer through this Mac's current output (AirPods, speakers,
//! whatever is selected in the menu bar).
//!
//! The output device is only opened while sound is arriving, so it isn't held busy (and
//! AirPods can still hand off to a phone) when nothing is playing. It follows the system's
//! default output, rebuilding the stream when that changes.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use mousetail_core::audio::{AudioPacket, Playout, Receiver};
use tracing::{debug, info, warn};

/// How long without packets before the output device is released.
const IDLE: Duration = Duration::from_secs(3);

pub struct Player {
    tx: mpsc::Sender<AudioPacket>,
}

impl Player {
    pub fn start() -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("audio-in".into())
            .spawn(move || run(rx))?;
        Ok(Self { tx })
    }

    pub fn play(&self, packet: AudioPacket) {
        let _ = self.tx.send(packet);
    }
}

struct Output {
    _stream: cpal::Stream,
    device: String,
}

fn run(rx: mpsc::Receiver<AudioPacket>) {
    let Ok(mut receiver) = Receiver::new() else {
        warn!("couldn't start the audio decoder");
        return;
    };
    let playout = Arc::new(Mutex::new(Playout::default()));
    let mut output: Option<Output> = None;
    let mut last_packet = Instant::now();
    let mut last_stats = Instant::now();
    let mut last_device_check = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(packet) => {
                last_packet = Instant::now();
                let (pcm, fresh) = receiver.receive(&packet);
                let mut p = playout.lock().unwrap();
                if fresh {
                    p.reset();
                }
                p.push(&pcm);
                drop(p);
                if output.is_none() {
                    output = open(&playout);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if output.is_some() && last_stats.elapsed() > Duration::from_secs(10) {
            last_stats = Instant::now();
            let stats = playout.lock().unwrap().stats();
            debug!(?stats, "playing");
        }
        if output.is_some() && last_packet.elapsed() > IDLE {
            debug!("no sound for a while; releasing the output device");
            output = None;
            playout.lock().unwrap().reset();
        }
        // Follow the output the user picks (e.g. AirPods connecting).
        if last_device_check.elapsed() > Duration::from_secs(1) {
            last_device_check = Instant::now();
            if let Some(o) = &output
                && default_device_name().is_some_and(|d| d != o.device)
            {
                output = open(&playout);
            }
        }
    }
}

fn default_device_name() -> Option<String> {
    let device = cpal::default_host().default_output_device()?;
    device_name(&device)
}

fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_string())
}

fn open(playout: &Arc<Mutex<Playout>>) -> Option<Output> {
    let device = cpal::default_host().default_output_device()?;
    let name = device_name(&device).unwrap_or_default();
    let config = match device.default_output_config() {
        Ok(c) => c.config(),
        Err(e) => {
            warn!("no usable output format on {name}: {e}");
            return None;
        }
    };
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    let p = playout.clone();
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            match p.try_lock() {
                Ok(mut p) => p.pull(data, channels, rate),
                // Never block the audio thread; a moment of silence beats a glitch.
                Err(_) => data.fill(0.0),
            }
        },
        |e| warn!("audio output: {e}"),
        None,
    );
    match stream.and_then(|s| s.play().map(|()| s)) {
        Ok(s) => {
            info!("playing sound on {name}");
            Some(Output {
                _stream: s,
                device: name,
            })
        }
        Err(e) => {
            warn!("couldn't open {name}: {e}");
            None
        }
    }
}
