//! A virtual speaker in PipeWire. Whatever plays into it comes out as 48 kHz stereo PCM on a
//! channel, ready to encode and send to the other computer.
//!
//! The speaker lives as long as the `VirtualSpeaker`; dropping it removes the device, and
//! PipeWire moves any playing apps to another output. While it exists it's made the default
//! output (remembering the previous one, which is restored on drop) unless the user picks a
//! different output themselves.

use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use mousetail_core::audio::{CHANNELS, SAMPLE_RATE};
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;

pub struct VirtualSpeaker {
    quit: pw::channel::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
    node_name: String,
    previous_default: Option<String>,
}

impl VirtualSpeaker {
    pub fn supported() -> bool {
        true
    }

    /// Create the speaker, named `description` (e.g. the Mac's name), and make it the default
    /// output. PCM arrives on the returned channel in whatever chunk sizes PipeWire uses.
    pub fn start(id: &str, description: &str) -> anyhow::Result<(Self, mpsc::Receiver<Vec<i16>>)> {
        let node_name = format!("mousetail.{id}");
        // Before the speaker exists: PipeWire may make it the default as soon as it appears
        // (it remembers it from last time), and then there'd be nothing to go back to.
        let previous_default = default_sink().filter(|s| *s != node_name);
        let (pcm_tx, pcm_rx) = mpsc::channel();
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let name = node_name.clone();
        let desc = description.to_string();
        let thread = thread::Builder::new()
            .name("audio-out".into())
            .spawn(move || {
                if let Err(e) = run(&name, &desc, pcm_tx, quit_rx, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })?;
        ready_rx.recv().context("audio thread died")??;

        // The node takes a moment to appear in the graph.
        for _ in 0..20 {
            if set_default_sink(&node_name) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        Ok((
            Self {
                quit: quit_tx,
                thread: Some(thread),
                node_name,
                previous_default,
            },
            pcm_rx,
        ))
    }
}

impl Drop for VirtualSpeaker {
    fn drop(&mut self) {
        let was_default = default_sink().as_deref() == Some(self.node_name.as_str());
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // Only hand the default back if the user hadn't already chosen something else.
        if was_default && let Some(prev) = &self.previous_default {
            set_default_sink(prev);
        }
    }
}

fn run(
    node_name: &str,
    description: &str,
    pcm: mpsc::Sender<Vec<i16>>,
    quit: pw::channel::Receiver<()>,
    ready: &mpsc::Sender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::NODE_NAME => node_name,
        *pw::keys::NODE_DESCRIPTION => description,
        "node.nick" => description,
        "device.icon-name" => "computer",
        "audio.channels" => "2",
        "audio.position" => "FL,FR",
        // Hand sound over about every 10 ms. Left to itself PipeWire may batch 20-40 ms at a
        // time, which arrives on the other computer in lumps as big as its whole cushion.
        *pw::keys::NODE_LATENCY => "512/48000",
    };
    let stream = pw::stream::StreamBox::new(&core, "MouseTail", props)?;
    let _listener = stream
        .add_local_listener_with_user_data(pcm)
        .process(|stream, pcm| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            if let Some(bytes) = data.data() {
                let end = (offset + size).min(bytes.len());
                let samples: Vec<i16> = bytes[offset.min(end)..end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b))
                    .collect();
                if !samples.is_empty() {
                    let _ = pcm.send(samples);
                }
            }
        })
        .register()?;

    // Fixed format: PipeWire converts whatever apps play into it.
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
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
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    let ml = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |()| ml.quit());
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}

fn default_sink() -> Option<String> {
    let out = Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

fn set_default_sink(name: &str) -> bool {
    Command::new("pactl")
        .args(["set-default-sink", name])
        .status()
        .is_ok_and(|s| s.success())
        && default_sink().as_deref() == Some(name)
}
