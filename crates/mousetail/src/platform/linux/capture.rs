//! Linux as the main computer, on compositors with `wlr-layer-shell` (Hyprland, Sway, KDE,
//! river, niri…).
//!
//! Where an edge leads to another computer we lay a one-pixel invisible strip along it, on
//! the overlay layer. While the pointer rests on a strip, the compositor sends us its raw
//! relative motion even when the cursor can't move any further, which is exactly "pushing
//! against the edge". That feeds the same `Controller` the Mac uses; when it decides to
//! cross we lock the pointer to the strip, take keyboard focus and inhibit compositor
//! shortcuts, and forward everything until it brings the cursor home.
//!
//! Without layer-shell (GNOME), the desktop portal does the watching instead (`portal`).

use std::collections::HashSet;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use mousetail_core::controller::{Action, Controller, Input};
use mousetail_core::keys::ev;
use mousetail_core::layout::{Point, Side};
use mousetail_core::proto::Scroll;
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;
use wayland_client::protocol::wl_pointer::{self, ButtonState};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_region, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
    zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
    zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::platform::Edge;

pub(super) enum Cmd {
    Apply(Action),
    Edges(Vec<Edge>),
    HideCursor,
}

pub struct Capture {
    tx: mpsc::Sender<Cmd>,
    wake: OwnedFd,
}

impl Capture {
    pub fn supported() -> bool {
        std::env::var_os("WAYLAND_DISPLAY").is_some()
    }

    pub fn start(
        controller: Arc<Mutex<Controller>>,
        actions: UnboundedSender<Action>,
        _prompt: bool,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut fds = [0; 2];
        anyhow::ensure!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } == 0,
            "pipe: {}",
            std::io::Error::last_os_error()
        );
        let (wake_rx, wake_tx) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let (ready_tx, ready_rx) = mpsc::channel();
        thread::Builder::new()
            .name("capture".into())
            .spawn(move || match Grabber::connect(&controller, &actions) {
                Ok((grabber, queue)) => {
                    let _ = ready_tx.send(Ok(()));
                    if let Err(e) = grabber.run(queue, rx, wake_rx) {
                        // Lost the compositor (it restarted, say). Carrying on would leave the
                        // controller thinking it can still see the edges, or even that the
                        // cursor is away; start over with a fresh connection instead, as the
                        // injection side does.
                        tracing::error!("input capture stopped: {e:#}");
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    debug!("no layer-shell capture ({e:#}), so the desktop portal");
                    super::portal::run(controller, actions, rx, wake_rx, ready_tx);
                }
            })?;
        ready_rx.recv().context("capture thread died")??;
        Ok(Self { tx, wake: wake_tx })
    }

    fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_ok() {
            unsafe { libc::write(self.wake.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
        }
    }

    pub fn apply(&self, action: &Action) {
        self.send(Cmd::Apply(action.clone()));
    }

    /// Another computer's cursor just left us over an edge: hide ours where it stopped, until
    /// this computer's own mouse moves it.
    pub fn hide_cursor(&self) {
        self.send(Cmd::HideCursor);
    }

    /// Which edges lead somewhere (changes with the arrangement and the displays).
    pub fn set_edges(&self, edges: Vec<Edge>) {
        self.send(Cmd::Edges(edges));
    }
}

/// A monitor, followed as it comes and goes. Its position comes with each `Edge` (from the
/// same display list the controller uses); here we only need to know which one is which.
struct Output {
    /// The registry's name for it, to notice when it's unplugged.
    global: u32,
    wl: wl_output::WlOutput,
    name: String,
}

struct Zone {
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    edge: Edge,
    /// Global position of the surface's top-left corner.
    origin: Point,
    buffer: Option<wl_buffer::WlBuffer>,
}

struct Grab {
    zone: usize,
    locked: ZwpLockedPointerV1,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
}

#[derive(Default)]
struct PendingScroll {
    dx: f64,
    dy: f64,
    v120: (i32, i32),
    any: bool,
}

struct Grabber {
    qh: QueueHandle<Grabber>,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    seat: wl_seat::WlSeat,
    layer_shell: ZwlrLayerShellV1,
    constraints: ZwpPointerConstraintsV1,
    relative: ZwpRelativePointerManagerV1,
    inhibit: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    xdg_outputs: Option<ZxdgOutputManagerV1>,
    outputs: Vec<Output>,
    /// The edges the node wants strips on; rebuilt when they or the monitors change.
    wanted: Vec<Edge>,
    /// A rebuild waiting for the current grab to end.
    rebuild_pending: bool,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    relative_pointer: Option<ZwpRelativePointerV1>,
    zones: Vec<Zone>,
    hovered: Option<usize>,
    enter_serial: u32,
    /// Hide the cursor if it lands on a strip before then: another computer's cursor just
    /// left over that edge (and the pointer may reach the strip after we hear so).
    hide_until: Option<Instant>,
    pos: Point,
    buttons: HashSet<u16>,
    grab: Option<Grab>,
    scroll: PendingScroll,
    controller: Arc<Mutex<Controller>>,
    actions: UnboundedSender<Action>,
}

/// Globals gathered during setup.
#[derive(Default)]
struct Globals {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    layer_shell: Option<ZwlrLayerShellV1>,
    constraints: Option<ZwpPointerConstraintsV1>,
    relative: Option<ZwpRelativePointerManagerV1>,
    inhibit: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    xdg_outputs: Option<ZxdgOutputManagerV1>,
}

impl Grabber {
    fn connect(
        controller: &Arc<Mutex<Controller>>,
        actions: &UnboundedSender<Action>,
    ) -> anyhow::Result<(Self, EventQueue<Grabber>)> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
        // Gather globals on a throwaway queue, then build the real state.
        let mut gq = conn.new_event_queue::<Globals>();
        let gqh = gq.handle();
        conn.display().get_registry(&gqh, ());
        let mut g = Globals::default();
        gq.roundtrip(&mut g)?;
        let need = |name: &str| {
            format!("this desktop doesn't support {name}, so it can't be the main computer yet")
        };
        let compositor = g.compositor.clone().context("no wl_compositor")?;
        let shm = g.shm.clone().context("no wl_shm")?;
        let seat = g.seat.clone().context("no wl_seat")?;
        let layer_shell = g
            .layer_shell
            .clone()
            .with_context(|| need("wlr-layer-shell"))?;
        let constraints = g
            .constraints
            .clone()
            .with_context(|| need("pointer constraints"))?;
        let relative = g
            .relative
            .clone()
            .with_context(|| need("relative pointer motion"))?;

        let queue = conn.new_event_queue::<Grabber>();
        let qh = queue.handle();
        // Re-bind the seat on our queue so its events (pointer, keyboard) come to us.
        let registry = conn.display().get_registry(&qh, ());
        let _ = registry;
        let grabber = Grabber {
            qh: qh.clone(),
            compositor,
            shm,
            seat: seat.clone(),
            layer_shell,
            constraints,
            relative,
            inhibit: g.inhibit.clone(),
            xdg_outputs: g.xdg_outputs.clone(),
            outputs: vec![],
            wanted: vec![],
            rebuild_pending: false,
            pointer: None,
            keyboard: None,
            relative_pointer: None,
            zones: vec![],
            hovered: None,
            enter_serial: 0,
            hide_until: None,
            pos: Point::default(),
            buttons: HashSet::new(),
            grab: None,
            scroll: PendingScroll::default(),
            controller: controller.clone(),
            actions: actions.clone(),
        };
        Ok((grabber, queue))
    }

    fn run(
        mut self,
        mut queue: EventQueue<Grabber>,
        rx: mpsc::Receiver<Cmd>,
        wake: OwnedFd,
    ) -> anyhow::Result<()> {
        queue.roundtrip(&mut self)?;
        loop {
            queue.flush()?;
            let guard = loop {
                queue.dispatch_pending(&mut self)?;
                if let Some(g) = queue.prepare_read() {
                    break g;
                }
            };
            let mut fds = [
                libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err.into());
            }
            if fds[0].revents & libc::POLLIN != 0 {
                guard.read()?;
            } else {
                drop(guard);
            }
            if fds[1].revents & libc::POLLIN != 0 {
                let mut buf = [0u8; 64];
                while unsafe { libc::read(wake.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) }
                    > 0
                {}
            }
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    Cmd::Apply(a) => self.do_action(&a),
                    Cmd::Edges(e) => self.set_edges(e),
                    Cmd::HideCursor => self.hide_cursor(),
                }
            }
            queue.dispatch_pending(&mut self)?;
        }
    }

    // ---------------------------------------------------------------------- edge strips

    fn set_edges(&mut self, edges: Vec<Edge>) {
        let current: Vec<Edge> = self.zones.iter().map(|z| z.edge.clone()).collect();
        self.wanted = edges;
        if current != self.wanted {
            self.rebuild();
        }
    }

    /// Lay the strips out afresh for `wanted` on the monitors there are now.
    fn rebuild(&mut self) {
        if self.grab.is_some() {
            // Don't pull the rug out mid-grab; do it once the cursor is home.
            self.rebuild_pending = true;
            return;
        }
        self.rebuild_pending = false;
        for z in self.zones.drain(..) {
            z.layer.destroy();
            z.surface.destroy();
            if let Some(b) = z.buffer {
                b.destroy();
            }
        }
        self.hovered = None;
        for edge in self.wanted.clone() {
            let Some(wl) = self
                .outputs
                .iter()
                .find(|o| o.name == edge.display)
                .map(|o| o.wl.clone())
            else {
                // Not announced yet (just plugged in): rebuilt when it is.
                debug!("no output called {} for an edge strip yet", edge.display);
                continue;
            };
            let r = edge.rect;
            let (x, y, w, h) = (r.x as i32, r.y as i32, r.w as i32, r.h as i32);
            let surface = self.compositor.create_surface(&self.qh, ());
            let index = self.zones.len();
            let layer = self.layer_shell.get_layer_surface(
                &surface,
                Some(&wl),
                Layer::Overlay,
                "mousetail-edge".into(),
                &self.qh,
                index,
            );
            let (anchor, size, origin) = match edge.side {
                Side::Left => (Anchor::Left | Anchor::Top | Anchor::Bottom, (1, 0), (x, y)),
                Side::Right => (
                    Anchor::Right | Anchor::Top | Anchor::Bottom,
                    (1, 0),
                    (x + w - 1, y),
                ),
                Side::Above => (Anchor::Top | Anchor::Left | Anchor::Right, (0, 1), (x, y)),
                Side::Below => (
                    Anchor::Bottom | Anchor::Left | Anchor::Right,
                    (0, 1),
                    (x, y + h - 1),
                ),
            };
            layer.set_anchor(anchor);
            layer.set_size(size.0, size.1);
            // -1: span the whole edge, even alongside bars that reserve space.
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            surface.commit();
            self.zones.push(Zone {
                surface,
                layer,
                edge,
                origin: Point::new(origin.0 as f64, origin.1 as f64),
                buffer: None,
            });
        }
    }

    /// A transparent buffer for a strip (layer surfaces need content to be shown).
    fn transparent_buffer(&self, w: i32, h: i32) -> Option<wl_buffer::WlBuffer> {
        let size = (w.max(1) * h.max(1) * 4) as usize;
        let fd = unsafe { libc::memfd_create(c"mousetail-edge".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) } != 0 {
            return None;
        }
        // Not quite fully transparent: some compositors skip fully clear surfaces when
        // deciding what's under the pointer. Alpha 1/255 is invisible to the eye.
        let pixels: Vec<u8> = std::iter::repeat_n([0u8, 0, 0, 1], size / 4)
            .flatten()
            .collect();
        unsafe { libc::write(fd.as_raw_fd(), pixels.as_ptr().cast(), pixels.len()) };
        let pool = self.shm.create_pool(fd.as_fd(), size as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            w.max(1),
            h.max(1),
            w.max(1) * 4,
            wl_shm::Format::Argb8888,
            &self.qh,
            (),
        );
        pool.destroy();
        Some(buffer)
    }

    // ------------------------------------------------------------------ controller glue

    fn feed(&mut self, input: Input) {
        let outcome = self
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .handle(input);
        for action in outcome.actions {
            match action {
                Action::Grab | Action::Release { .. } => self.do_action(&action),
                other => {
                    let _ = self.actions.send(other);
                }
            }
        }
    }

    /// The pointer rests on our strip where another computer's cursor left us, so we choose
    /// its image: none. The compositor shows it again once it's moved off the strip, and
    /// pushing on across still works.
    fn hide_cursor(&mut self) {
        match (&self.pointer, self.hovered) {
            (Some(pointer), Some(_)) if self.grab.is_none() => {
                pointer.set_cursor(self.enter_serial, None, 0, 0);
                self.hide_until = None;
            }
            _ => self.hide_until = Some(Instant::now() + Duration::from_secs(1)),
        }
    }

    fn do_action(&mut self, action: &Action) {
        match action {
            Action::Grab => self.grab(),
            Action::Release { warp } => self.release(*warp),
            _ => {}
        }
    }

    fn grab(&mut self) {
        if self.grab.is_some() {
            return;
        }
        let (Some(zone), Some(pointer)) = (self.hovered, self.pointer.clone()) else {
            return;
        };
        let z = &self.zones[zone];
        let locked = self.constraints.lock_pointer(
            &z.surface,
            &pointer,
            None,
            Lifetime::Persistent,
            &self.qh,
            (),
        );
        pointer.set_cursor(self.enter_serial, None, 0, 0);
        z.layer
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        let inhibitor = self
            .inhibit
            .as_ref()
            .map(|m| m.inhibit_shortcuts(&z.surface, &self.seat, &self.qh, ()));
        z.surface.commit();
        self.grab = Some(Grab {
            zone,
            locked,
            inhibitor,
        });
    }

    fn release(&mut self, warp: Point) {
        let Some(g) = self.grab.take() else { return };
        let z = &self.zones[g.zone];
        // Ask the compositor to leave the cursor where the controller says it came home.
        g.locked
            .set_cursor_position_hint(warp.x - z.origin.x, warp.y - z.origin.y);
        z.surface.commit();
        g.locked.destroy();
        if let Some(i) = g.inhibitor {
            i.destroy();
        }
        z.layer
            .set_keyboard_interactivity(KeyboardInteractivity::None);
        z.surface.commit();
        self.pos = warp;
        self.buttons.clear();
        if self.rebuild_pending {
            self.rebuild();
        }
    }

    /// Bring the cursor home: the controller decides where, as for the hotkey.
    fn go_home(&mut self) {
        let actions = self
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .release();
        for action in actions {
            match action {
                Action::Grab | Action::Release { .. } => self.do_action(&action),
                other => {
                    let _ = self.actions.send(other);
                }
            }
        }
    }
}

// ------------------------------------------------------------------------------ globals

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
    fn event(
        g: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => g.compositor = Some(registry.bind(name, version.min(4), qh, ())),
                "wl_shm" => g.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_seat" if g.seat.is_none() => {
                    g.seat = Some(registry.bind(name, version.min(8), qh, ()))
                }
                "zwlr_layer_shell_v1" => {
                    g.layer_shell = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "zwp_pointer_constraints_v1" => {
                    g.constraints = Some(registry.bind(name, 1, qh, ()))
                }
                "zwp_relative_pointer_manager_v1" => {
                    g.relative = Some(registry.bind(name, 1, qh, ()))
                }
                "zwp_keyboard_shortcuts_inhibit_manager_v1" => {
                    g.inhibit = Some(registry.bind(name, 1, qh, ()))
                }
                "zxdg_output_manager_v1" => {
                    g.xdg_outputs = Some(registry.bind(name, version.min(3), qh, ()))
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(Globals: ignore wl_compositor::WlCompositor);
delegate_noop!(Globals: ignore wl_shm::WlShm);
delegate_noop!(Globals: ignore wl_seat::WlSeat);
delegate_noop!(Globals: ignore ZwlrLayerShellV1);
delegate_noop!(Globals: ignore ZwpPointerConstraintsV1);
delegate_noop!(Globals: ignore ZwpRelativePointerManagerV1);
delegate_noop!(Globals: ignore ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(Globals: ignore ZxdgOutputManagerV1);

// ------------------------------------------------------------------------ live events

impl Dispatch<wl_registry::WlRegistry, ()> for Grabber {
    fn event(
        s: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            // Our own seat binding, so pointer and keyboard events arrive on this queue.
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_seat" && s.pointer.is_none() => {
                let seat: wl_seat::WlSeat = registry.bind(name, version.min(8), qh, ());
                tracing::trace!("bound seat for capture");
                s.seat = seat;
            }
            // Monitors, now and as they're plugged in. Their names arrive shortly.
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => {
                let wl: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                if let Some(m) = &s.xdg_outputs {
                    m.get_xdg_output(&wl, qh, name);
                }
                s.outputs.push(Output {
                    global: name,
                    wl,
                    name: String::new(),
                });
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = s.outputs.iter().position(|o| o.global == name) {
                    let gone = s.outputs.remove(i);
                    if gone.wl.version() >= 3 {
                        gone.wl.release();
                    }
                    s.rebuild();
                }
            }
            _ => {}
        }
    }
}

impl Grabber {
    /// A monitor told us its name: strips waiting for it can go down now.
    fn output_named(&mut self, global: u32, name: String) {
        let Some(o) = self.outputs.iter_mut().find(|o| o.global == global) else {
            return;
        };
        if o.name.is_empty() {
            o.name = name;
            if self.wanted.iter().any(|e| e.display == o.name) {
                self.rebuild();
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Grabber {
    fn event(
        s: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(global) = s.outputs.iter().find(|o| o.wl == *output).map(|o| o.global)
        {
            s.output_named(global, name);
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for Grabber {
    fn event(
        s: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_output_v1::Event::Name { name } = event {
            s.output_named(*global, name);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Grabber {
    fn event(
        s: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            tracing::trace!("seat capabilities {caps:?}");
            if caps.contains(wl_seat::Capability::Pointer) && s.pointer.is_none() {
                let pointer = seat.get_pointer(qh, ());
                s.relative_pointer = Some(s.relative.get_relative_pointer(&pointer, qh, ()));
                s.pointer = Some(pointer);
            }
            if caps.contains(wl_seat::Capability::Keyboard) && s.keyboard.is_none() {
                s.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for Grabber {
    fn event(
        s: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                tracing::trace!("edge strip {index} configured {width}x{height}");
                layer.ack_configure(serial);
                if let Some(z) = s.zones.get(*index)
                    && z.buffer.is_none()
                {
                    let buffer = s.transparent_buffer(width as i32, height as i32);
                    let z = &mut s.zones[*index];
                    if let Some(b) = &buffer {
                        z.surface.attach(Some(b), 0, 0);
                        z.surface.damage_buffer(0, 0, width as i32, height as i32);
                    }
                    // Say explicitly that the whole strip takes pointer input.
                    let region = s.compositor.create_region(&s.qh, ());
                    region.add(0, 0, width.max(1) as i32, height.max(1) as i32);
                    z.surface.set_input_region(Some(&region));
                    region.destroy();
                    z.surface.commit();
                    z.buffer = buffer;
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {}
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Grabber {
    fn event(
        s: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                tracing::trace!("pointer entered a surface at {surface_x},{surface_y}");
                s.enter_serial = serial;
                s.hovered = s.zones.iter().position(|z| z.surface == surface);
                if let Some(i) = s.hovered {
                    let o = s.zones[i].origin;
                    s.pos = Point::new(o.x + surface_x, o.y + surface_y);
                    if s.hide_until.take().is_some_and(|t| Instant::now() < t) {
                        s.hide_cursor();
                    }
                }
            }
            wl_pointer::Event::Leave { .. } => {
                if s.grab.is_none() {
                    s.hovered = None;
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(i) = s.hovered
                    && s.grab.is_none()
                {
                    let o = s.zones[i].origin;
                    s.pos = Point::new(o.x + surface_x, o.y + surface_y);
                }
            }
            wl_pointer::Event::Button { button, state, .. } => {
                let code = button as u16;
                let down = matches!(state, WEnum::Value(ButtonState::Pressed));
                if down {
                    s.buttons.insert(code);
                } else {
                    s.buttons.remove(&code);
                }
                if s.grab.is_some() {
                    s.feed(Input::Button { code, down });
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                s.scroll.any = true;
                match axis {
                    WEnum::Value(wl_pointer::Axis::VerticalScroll) => s.scroll.dy += value,
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => s.scroll.dx += value,
                    _ => {}
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => match axis {
                WEnum::Value(wl_pointer::Axis::VerticalScroll) => s.scroll.v120.1 += value120,
                WEnum::Value(wl_pointer::Axis::HorizontalScroll) => s.scroll.v120.0 += value120,
                _ => {}
            },
            wl_pointer::Event::Frame => {
                let sc = std::mem::take(&mut s.scroll);
                if sc.any && s.grab.is_some() {
                    // Whole notches from a notched wheel. High-resolution wheels send fractions
                    // of a notch, which go as smooth scrolling (dividing would make them 0).
                    let whole = sc.v120.0 % 120 == 0 && sc.v120.1 % 120 == 0;
                    let notches =
                        (sc.v120 != (0, 0) && whole).then_some((sc.v120.0 / 120, sc.v120.1 / 120));
                    s.feed(Input::Scroll(Scroll {
                        dx: sc.dx,
                        dy: sc.dy,
                        notches,
                    }));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for Grabber {
    fn event(
        s: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = &event {
            tracing::trace!("relative motion {dx},{dy} hovered={:?}", s.hovered);
        }
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event
            && (s.hovered.is_some() || s.grab.is_some())
        {
            let dragging = s.buttons.iter().any(|b| ev::is_button(*b));
            s.feed(Input::Motion {
                at: s.pos,
                dx,
                dy,
                dragging,
            });
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Grabber {
    fn event(
        s: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key { key, state, .. } = event
            && s.grab.is_some()
        {
            let down = matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed));
            s.feed(Input::Key {
                code: key as u16,
                down,
            });
        }
    }
}

delegate_noop!(Grabber: ignore wl_surface::WlSurface);
delegate_noop!(Grabber: ignore wl_region::WlRegion);
delegate_noop!(Grabber: ignore wl_buffer::WlBuffer);
delegate_noop!(Grabber: ignore wl_shm_pool::WlShmPool);

impl Dispatch<ZwpLockedPointerV1, ()> for Grabber {
    fn event(
        s: &mut Self,
        _: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The compositor took the pointer back (the screen locked, another app grabbed it):
        // nothing reaches us now, so come home rather than stay "away" with no way back.
        if let zwp_locked_pointer_v1::Event::Unlocked = event
            && s.grab.is_some()
        {
            s.go_home();
        }
    }
}
delegate_noop!(Grabber: ignore ZwpKeyboardShortcutsInhibitorV1);
