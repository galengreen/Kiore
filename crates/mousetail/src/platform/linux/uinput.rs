//! Being controlled on any Linux desktop (GNOME, KDE, X11…) through a kernel virtual input
//! device, for compositors without the wlroots virtual-pointer protocols.
//!
//! The pointer is an absolute device (like a VM's tablet) spanning the whole desktop, so the
//! cursor lands exactly where the controlling computer puts it. Needs access to
//! `/dev/uinput`, which `enable-input.sh` grants to the logged-in user once.

use std::collections::HashSet;

use anyhow::Context;
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, InputEvent, KeyCode, RelativeAxisCode, UinputAbsSetup,
};
use mousetail_core::keys::ev;
use mousetail_core::layout::Rect;

use super::Cmd;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const REL_WHEEL_HI_RES: u16 = 11;
const REL_HWHEEL_HI_RES: u16 = 12;
/// Absolute axis range; positions are scaled into it across the whole desktop.
const RANGE: i32 = 65535;
/// Points of smooth scrolling per wheel notch.
const POINTS_PER_NOTCH: f64 = 15.0;

pub struct Uinput {
    dev: VirtualDevice,
    bounds: Rect,
    keys: HashSet<u16>,
    buttons: HashSet<u16>,
    /// Hi-res wheel units (120 per notch) not yet emitted as whole notches.
    wheel: (i32, i32),
}

impl Uinput {
    pub fn new() -> anyhow::Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=248u16 {
            keys.insert(KeyCode(code));
        }
        for code in ev::BTN_LEFT..=0x117 {
            keys.insert(KeyCode(code));
        }
        let mut rel = AttributeSet::<RelativeAxisCode>::new();
        for axis in [REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
            rel.insert(RelativeAxisCode(axis));
        }
        let abs =
            |code| UinputAbsSetup::new(AbsoluteAxisCode(code), AbsInfo::new(0, 0, RANGE, 0, 0, 1));
        let dev = VirtualDevice::builder()
            .context("can't open /dev/uinput (run enable-input.sh once to allow it)")?
            .name("MouseTail virtual input")
            .with_keys(&keys)?
            .with_relative_axes(&rel)?
            .with_absolute_axis(&abs(ABS_X))?
            .with_absolute_axis(&abs(ABS_Y))?
            .build()
            .context("creating the virtual input device")?;
        Ok(Self {
            dev,
            bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
            keys: HashSet::new(),
            buttons: HashSet::new(),
            wheel: (0, 0),
        })
    }

    pub fn run(mut self, rx: std::sync::mpsc::Receiver<Cmd>) {
        while let Ok(cmd) = rx.recv() {
            if let Err(e) = self.apply(cmd) {
                tracing::warn!("virtual input device: {e}");
            }
        }
    }

    fn emit(&mut self, events: &[(u16, u16, i32)]) -> std::io::Result<()> {
        let mut out: Vec<InputEvent> = events
            .iter()
            .map(|(t, c, v)| InputEvent::new(*t, *c, *v))
            .collect();
        out.push(InputEvent::new(EV_SYN, 0, 0));
        self.dev.emit(&out)
    }

    fn apply(&mut self, cmd: Cmd) -> std::io::Result<()> {
        match cmd {
            Cmd::Bounds(b) => {
                self.bounds = b;
                Ok(())
            }
            Cmd::Motion(x, y) => {
                let b = self.bounds;
                self.emit(&[
                    (EV_ABS, ABS_X, axis(x - b.x, b.w)),
                    (EV_ABS, ABS_Y, axis(y - b.y, b.h)),
                ])
            }
            Cmd::Button(code, down) | Cmd::Key(code, down) => {
                let held = if ev::is_button(code) {
                    &mut self.buttons
                } else {
                    &mut self.keys
                };
                let changed = if down {
                    held.insert(code)
                } else {
                    held.remove(&code)
                };
                if !changed {
                    return Ok(());
                }
                self.emit(&[(EV_KEY, code, down as i32)])
            }
            Cmd::Scroll(s) => {
                // Kernel wheels count "up/right" positive; ours is Wayland's "down/right".
                let (hx, hy) = match s.notches {
                    Some((nx, ny)) => (nx * 120, ny * 120),
                    None => (
                        (s.dx / POINTS_PER_NOTCH * 120.0).round() as i32,
                        (s.dy / POINTS_PER_NOTCH * 120.0).round() as i32,
                    ),
                };
                self.wheel.0 += hx;
                self.wheel.1 += hy;
                let (nx, ny) = (self.wheel.0 / 120, self.wheel.1 / 120);
                self.wheel.0 -= nx * 120;
                self.wheel.1 -= ny * 120;
                let mut events = vec![];
                if hy != 0 {
                    events.push((EV_REL, REL_WHEEL_HI_RES, -hy));
                }
                if hx != 0 {
                    events.push((EV_REL, REL_HWHEEL_HI_RES, hx));
                }
                if ny != 0 {
                    events.push((EV_REL, REL_WHEEL, -ny));
                }
                if nx != 0 {
                    events.push((EV_REL, REL_HWHEEL, nx));
                }
                if events.is_empty() {
                    Ok(())
                } else {
                    self.emit(&events)
                }
            }
            // A kernel wheel has no fingers to lift: scroll smoothly as before.
            Cmd::TrackpadScroll(dx, dy) => self.apply(Cmd::Scroll(mousetail_core::proto::Scroll {
                dx,
                dy,
                notches: None,
            })),
            Cmd::TrackpadScrollEnd => Ok(()),
            Cmd::ReleaseAll => {
                let held: Vec<u16> = self.keys.drain().chain(self.buttons.drain()).collect();
                for code in held {
                    self.emit(&[(EV_KEY, code, 0)])?;
                }
                Ok(())
            }
        }
    }
}

/// Where `v` points along a desktop `len` points wide lands on an absolute axis: the middle of
/// its pixel. Exactly on a screen's edge is where GNOME puts the barriers that tell this
/// computer's own mouse is pushing off it (see `portal`), and one landing there sets them off.
/// The desktop reads the axis back as `value * len / (RANGE + 1)`.
fn axis(v: f64, len: f64) -> i32 {
    let len = len.max(1.0);
    let pixel = v.clamp(0.0, len - 1.0).floor();
    ((pixel + 0.5) / len * (RANGE + 1) as f64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_land_inside_pixels_never_on_an_edge() {
        let read_back = |value: i32, len: f64| value as f64 * len / (RANGE + 1) as f64;
        for len in [1080.0, 1920.0, 3760.0, 5120.0] {
            for v in [
                -2.0,
                0.0,
                0.2,
                317.99,
                318.0,
                1051.875,
                len - 1.0,
                len + 3.0,
            ] {
                let p = read_back(axis(v, len), len);
                let pixel = v.clamp(0.0, len - 1.0).floor();
                assert!(
                    p > pixel + 0.25 && p < pixel + 0.75,
                    "{v} on {len}: read back as {p}"
                );
            }
        }
    }
}
