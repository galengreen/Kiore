//! Plays sound from another computer through this machine's default PipeWire output.
//!
//! The playback stream only exists while sound is arriving, so nothing sits in the mixer (or
//! the desktop's volume panel) when nothing is playing. PipeWire moves it along when the user
//! picks another output, except onto one of our own virtual speakers: that would send the
//! sound straight on to a third computer, so while one of those is the default the stream is
//! pinned to a real output instead.

use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use mousetail_core::audio::{AudioPacket, CHANNELS, Playout, Receiver, SAMPLE_RATE};
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;
use tracing::{debug, info, warn};

/// How long without packets before the output stream is released.
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

fn run(rx: mpsc::Receiver<AudioPacket>) {
    let Ok(mut receiver) = Receiver::new() else {
        warn!("couldn't start the audio decoder");
        return;
    };
    let playout = Arc::new(Mutex::new(Playout::default()));
    let mut output: Option<Output> = None;
    let mut last_packet = Instant::now();
    let mut last_stats = Instant::now();
    let mut last_default_check = Instant::now();
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
                    output = Output::open(&playout);
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
            debug!("no sound for a while; releasing the output stream");
            output = None;
            playout.lock().unwrap().reset();
        }
        // Step off (or back onto) the default output when one of our speakers becomes it.
        if last_default_check.elapsed() > Duration::from_secs(1) {
            last_default_check = Instant::now();
            if let Some(o) = &output
                && o.avoiding_default != default_is_ours()
            {
                drop(output.take());
                output = Output::open(&playout);
            }
        }
    }
}

/// A playback stream, running on its own PipeWire thread until dropped.
struct Output {
    quit: pw::channel::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
    /// Pinned to a real output because the default is one of our virtual speakers.
    avoiding_default: bool,
}

impl Output {
    fn open(playout: &Arc<Mutex<Playout>>) -> Option<Self> {
        let avoiding_default = default_is_ours();
        let target = if avoiding_default {
            match real_sink() {
                Some(s) => Some(s),
                None => {
                    debug!("the only output is one of our virtual speakers; not playing");
                    return None;
                }
            }
        } else {
            None
        };
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let p = playout.clone();
        let thread = thread::Builder::new()
            .name("audio-play".into())
            .spawn(move || {
                if let Err(e) = play(p, target.as_deref(), quit_rx, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            });
        let result = thread
            .context("starting the playback thread")
            .and_then(|t| {
                ready_rx.recv().context("playback thread died")??;
                Ok(t)
            });
        match result {
            Ok(thread) => {
                info!("playing sound from another computer");
                Some(Self {
                    quit: quit_tx,
                    thread: Some(thread),
                    avoiding_default,
                })
            }
            Err(e) => {
                warn!("couldn't open a playback stream: {e:#}");
                None
            }
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Frames filled per step of the realtime callback (kept on the stack).
const CHUNK: usize = 256;

fn play(
    playout: Arc<Mutex<Playout>>,
    target: Option<&str>,
    quit: pw::channel::Receiver<()>,
    ready: &mpsc::Sender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::MEDIA_NAME => "Sound from another computer",
        *pw::keys::NODE_NAME => "mousetail.player",
        *pw::keys::NODE_DESCRIPTION => "MouseTail",
        *pw::keys::APP_NAME => "MouseTail",
        "application.icon-name" => "computer",
        // About 10 ms per callback, well inside the playout buffer's 40 ms cushion.
        *pw::keys::NODE_LATENCY => "512/48000",
    };
    if let Some(target) = target {
        props.insert(*pw::keys::TARGET_OBJECT, target);
    }
    let stream = pw::stream::StreamBox::new(&core, "MouseTail", props)?;
    let _listener = stream
        .add_local_listener_with_user_data(playout)
        .process(|stream, playout| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let stride = CHANNELS * size_of::<f32>();
            let mut frames = 0;
            if let Some(bytes) = data.data() {
                frames = bytes.len() / stride;
                if requested > 0 {
                    frames = frames.min(requested);
                }
                let bytes = &mut bytes[..frames * stride];
                // Never block the audio thread; a moment of silence beats a glitch.
                let mut p = playout.try_lock().ok();
                let mut pcm = [0.0f32; CHUNK * CHANNELS];
                for out in bytes.chunks_mut(CHUNK * stride) {
                    let pcm = &mut pcm[..out.len() / size_of::<f32>()];
                    match p.as_mut() {
                        Some(p) => p.pull(pcm, CHANNELS, SAMPLE_RATE),
                        None => pcm.fill(0.0),
                    }
                    for (b, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(pcm.iter()) {
                        *b = s.to_le_bytes();
                    }
                }
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (frames * stride) as u32;
        })
        .register()?;

    // Exactly what the playout buffer produces; PipeWire converts for the output.
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS as u32);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow::anyhow!("format: {e:?}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&bytes).context("format pod")?];

    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    let ml = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |()| ml.quit());
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}

/// One of our virtual speakers (see `audio.rs`).
fn is_ours(sink: &str) -> bool {
    sink.starts_with("mousetail.")
}

fn default_is_ours() -> bool {
    Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| is_ours(String::from_utf8_lossy(&o.stdout).trim()))
}

/// The first output that isn't one of our virtual speakers.
fn real_sink() -> Option<String> {
    let out = Command::new("pactl")
        .args(["list", "short", "sinks"])
        .output()
        .ok()?;
    first_real_sink(&String::from_utf8_lossy(&out.stdout))
}

/// Picks from `pactl list short sinks` (id, name, driver, format, state; tab separated).
fn first_real_sink(list: &str) -> Option<String> {
    list.lines()
        .filter_map(|l| l.split('\t').nth(1))
        .find(|name| !is_ours(name))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_our_own_speakers() {
        let list = "71\tmousetail.abc\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n\
                    52\talsa_output.pci-0000_00_1f.3.analog-stereo\tPipeWire\ts32le 2ch 48000Hz\tIDLE\n";
        assert_eq!(
            first_real_sink(list).as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo")
        );
        assert_eq!(first_real_sink("71\tmousetail.abc\tPipeWire\t\t\n"), None);
    }
}
