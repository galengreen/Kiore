//! Linux as the main computer where there's no `wlr-layer-shell` (GNOME), through the
//! InputCapture desktop portal.
//!
//! We set pointer barriers along the stretches of edge that lead to other computers. When the
//! pointer pushes through one, the desktop holds it there and sends us everything (relative
//! motion, buttons, scrolling, keys) over libei until we let it go, saying where the pointer
//! comes back. That feeds the same `Controller` as everywhere else. Barriers can only change
//! while capture is off, so a new arrangement waits until the cursor is home.
//!
//! GNOME asks the person each time MouseTail starts (this version of the portal can't remember
//! the answer). If they say no, we don't ask again until the next start.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, mpsc};

use anyhow::{Context, anyhow, bail};
use futures_util::StreamExt;
use mousetail_core::controller::{Action, Controller, Input};
use mousetail_core::keys::ev;
use mousetail_core::layout::{Point, Side};
use mousetail_core::proto::Scroll;
use reis::ei::{self, button::ButtonState, handshake::ContextType, keyboard::KeyState};
use reis::event::{DeviceCapability, EiEvent};
use reis::tokio::EiConvertEventStream;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};
use zbus::zvariant::{self, OwnedObjectPath, OwnedValue, Structure, Value};
use zbus::{Connection, Message, Proxy};

use super::capture::Cmd;

const DESKTOP: &str = "org.freedesktop.portal.Desktop";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const INPUT_CAPTURE: &str = "org.freedesktop.portal.InputCapture";
/// Keyboard and pointer.
const CAPABILITIES: u32 = 1 | 2;

/// Once the person has been asked, why capture can't start this time MouseTail runs: the
/// retries mustn't put the question up again every couple of seconds.
static GAVE_UP: OnceLock<String> = OnceLock::new();

type Options<'a> = HashMap<&'a str, Value<'a>>;
type Results = HashMap<String, OwnedValue>;

/// Capture through the portal on this thread until it stops. `ready` hears once it's running
/// (the person allowed it), or why not.
pub(super) fn run(
    controller: Arc<Mutex<Controller>>,
    actions: UnboundedSender<Action>,
    cmds: mpsc::Receiver<Cmd>,
    wake: OwnedFd,
    ready: mpsc::Sender<anyhow::Result<()>>,
) {
    if let Some(why) = GAVE_UP.get() {
        let _ = ready.send(Err(anyhow!("{why}")));
        return;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            let _ = ready.send(Err(e.into()));
            return;
        }
    };
    runtime.block_on(async move {
        let (mut portal, events) = match Portal::open(controller, actions).await {
            Ok(opened) => opened,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _ = ready.send(Ok(()));
        if let Err(e) = portal.run(events, cmds, wake).await {
            tracing::error!("input capture stopped: {e:#}");
        }
        // The desktop ended capture (or the portal went away). Nothing can bring the cursor
        // back or see the edges from here on, so it comes home and MouseTail starts over, as
        // when it loses layer-shell; GNOME asks again.
        portal.go_home().await;
        std::process::exit(1);
    });
}

/// A stretch of one of our edges that leads to another computer, in local coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Barrier {
    side: Side,
    /// Start, and end (exclusive), along the edge.
    from: Point,
    to: Point,
}

impl Barrier {
    /// x1, y1, x2, y2 as the portal wants them: on the edge, both ends inclusive.
    fn position(&self) -> (i32, i32, i32, i32) {
        let (x, y) = (self.from.x.round() as i32, self.from.y.round() as i32);
        match self.side {
            Side::Left | Side::Right => (x, y, x, self.to.y.round() as i32 - 1),
            Side::Above | Side::Below => (x, y, self.to.x.round() as i32 - 1, y),
        }
    }

    /// Where the pointer is held after pushing through at `p` (just inside the screen), and
    /// which way it was going.
    fn crossing(&self, p: Point) -> (Point, f64, f64) {
        let along = |v: f64, lo: f64, hi: f64| v.min(hi - 1.0).max(lo);
        let (from, to) = (self.from, self.to);
        match self.side {
            Side::Left => (Point::new(from.x, along(p.y, from.y, to.y)), -1.0, 0.0),
            Side::Right => (Point::new(from.x - 1.0, along(p.y, from.y, to.y)), 1.0, 0.0),
            Side::Above => (Point::new(along(p.x, from.x, to.x), from.y), 0.0, -1.0),
            Side::Below => (Point::new(along(p.x, from.x, to.x), from.y - 1.0), 0.0, 1.0),
        }
    }

    fn distance(&self, p: Point) -> f64 {
        let (q, ..) = self.crossing(p);
        (q.x - p.x).hypot(q.y - p.y)
    }
}

struct Portal {
    conn: Connection,
    portal: Proxy<'static>,
    session: OwnedObjectPath,
    ei: ei::Context,
    controller: Arc<Mutex<Controller>>,
    actions: UnboundedSender<Action>,
    /// Barrier `n` (they count from 1) is `barriers[n - 1]`.
    barriers: Vec<Barrier>,
    /// The barriers need setting again once the cursor is home.
    stale: bool,
    /// The capture under way, by its activation id.
    active: Option<u32>,
    /// The cursor is on another computer.
    grabbed: bool,
    /// Where the pointer is held meanwhile.
    held_at: Point,
    /// Where the desktop last ended a capture itself, leaving the pointer on the barrier.
    left_on: Option<Point>,
    buttons: HashSet<u16>,
}

impl Portal {
    async fn open(
        controller: Arc<Mutex<Controller>>,
        actions: UnboundedSender<Action>,
    ) -> anyhow::Result<(Self, EiConvertEventStream)> {
        let conn = Connection::session()
            .await
            .context("connecting to the session bus")?;
        let portal = Proxy::new(&conn, DESKTOP, DESKTOP_PATH, INPUT_CAPTURE).await?;
        // Missing where the desktop can't do it, or isn't up yet (so trying again is fine).
        portal.get_property::<u32>("version").await.context(
            "this desktop supports neither wlr-layer-shell nor the input capture portal, so it \
             can't be the main computer yet",
        )?;

        let token = next_token();
        let options = Options::from([
            ("handle_token", Value::from(token.as_str())),
            ("session_handle_token", Value::from("mousetail")),
            ("capabilities", Value::from(CAPABILITIES)),
        ]);
        // GNOME asks the person first, so this can take a while.
        let answer = request(&conn, &portal, "CreateSession", &token, &("", options)).await?;
        let Some(session) = answer
            .ok()
            .and_then(|mut r| take_path(&mut r, "session_handle"))
        else {
            return Err(give_up(anyhow!(
                "input capture wasn't allowed (GNOME asks each time MouseTail starts)"
            )));
        };
        let connect = async {
            let fd: zvariant::OwnedFd = portal
                .call("ConnectToEIS", &(&session, Options::new()))
                .await?;
            let ei = ei::Context::new(UnixStream::from(OwnedFd::from(fd)))?;
            let (_, events) = ei
                .handshake_tokio("MouseTail", ContextType::Receiver)
                .await?;
            anyhow::Ok((ei, events))
        };
        let (ei, events) = connect
            .await
            .context("connecting to the desktop's input")
            .map_err(give_up)?;
        debug!("input capture session {session}");
        let portal = Self {
            conn,
            portal,
            session,
            ei,
            controller,
            actions,
            barriers: vec![],
            stale: false,
            active: None,
            grabbed: false,
            held_at: Point::default(),
            left_on: None,
            buttons: HashSet::new(),
        };
        Ok((portal, events))
    }

    async fn run(
        &mut self,
        mut events: EiConvertEventStream,
        cmds: mpsc::Receiver<Cmd>,
        wake: OwnedFd,
    ) -> anyhow::Result<()> {
        let wake = AsyncFd::new(wake)?;
        let mut activated = self.portal.receive_signal("Activated").await?;
        let mut deactivated = self.portal.receive_signal("Deactivated").await?;
        let mut zones_changed = self.portal.receive_signal("ZonesChanged").await?;
        let mut disabled = self.portal.receive_signal("Disabled").await?;
        let session = Proxy::new(
            &self.conn,
            DESKTOP,
            self.session.clone(),
            "org.freedesktop.portal.Session",
        )
        .await?;
        let mut closed = session.receive_signal("Closed").await?;
        loop {
            tokio::select! {
                event = events.next() => match event {
                    Some(event) => self.on_ei(event.context("input from the desktop")?).await?,
                    None => bail!("the desktop closed the input connection"),
                },
                Some(m) = activated.next() => if let Some(o) = self.ours(&m) {
                    self.activated(o).await
                },
                Some(m) = deactivated.next() => if let Some(o) = self.ours(&m) {
                    self.deactivated(o).await
                },
                // The monitors changed and the desktop switched capture off: set the barriers
                // up again (and again once the arrangement follows).
                Some(m) = zones_changed.next() => if self.ours(&m).is_some() {
                    self.stale = true;
                    self.rearm().await
                },
                Some(m) = disabled.next() => if self.ours(&m).is_some() {
                    self.desktop_ended().await
                },
                Some(_) = closed.next() => bail!("the desktop ended input capture"),
                ready = wake.readable() => {
                    let mut ready = ready?;
                    let mut buf = [0u8; 64];
                    while unsafe { libc::read(wake.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) }
                        > 0
                    {}
                    ready.clear_ready();
                    while let Ok(cmd) = cmds.try_recv() {
                        self.command(cmd).await;
                    }
                }
            }
        }
    }

    /// The options of a signal about our session (other apps' come too).
    fn ours(&self, m: &Message) -> Option<Results> {
        let (session, options): (OwnedObjectPath, Results) = m.body().deserialize().ok()?;
        (session == self.session).then_some(options)
    }

    async fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Apply(Action::Release { warp }) => self.release(Some(warp)).await,
            // Grabs follow crossings, which happen here; and the portal can't choose the
            // pointer's image.
            Cmd::Apply(_) | Cmd::HideCursor => {}
            // The barriers follow the arrangement those edges came from.
            Cmd::Edges(_) => self.rearm().await,
        }
    }

    // ------------------------------------------------------------------------- barriers

    /// Where barriers go: wherever the arrangement leads off our screens.
    fn wanted(&self) -> Vec<Barrier> {
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        controller
            .layout()
            .exit_edges(0)
            .into_iter()
            .map(|(_, side, from, to)| Barrier { side, from, to })
            .collect()
    }

    /// Put the barriers where they're wanted, unless they're there already.
    async fn rearm(&mut self) {
        if self.grabbed {
            // Changing them now would end the capture under way.
            self.stale = true;
            return;
        }
        let stale = std::mem::take(&mut self.stale);
        let wanted = self.wanted();
        if wanted == self.barriers && !stale {
            return;
        }
        if let Err(e) = self.set_barriers(wanted).await {
            warn!("couldn't watch the screen edges: {e:#}");
        }
    }

    async fn set_barriers(&mut self, wanted: Vec<Barrier>) -> anyhow::Result<()> {
        // They only change while capture is off. (This fails if it's off already.)
        let _ = self.call("Disable", Options::new()).await;
        self.barriers.clear();
        if wanted.is_empty() {
            return Ok(());
        }
        let token = next_token();
        let mut zones = self
            .request("GetZones", &token, &(&self.session, handle(&token)))
            .await?;
        let zone_set: u32 = take(&mut zones, "zone_set").context("no zone set")?;
        debug!(
            "screens {:?}, barriers {wanted:?}",
            take::<Vec<(u32, u32, i32, i32)>>(&mut zones, "zones")
        );
        let list: Vec<Options> = wanted
            .iter()
            .zip(1u32..)
            .map(|(b, id)| {
                Options::from([
                    ("barrier_id", Value::from(id)),
                    ("position", Value::from(Structure::from(b.position()))),
                ])
            })
            .collect();
        let token = next_token();
        let body = (&self.session, handle(&token), list, zone_set);
        let mut set = self.request("SetPointerBarriers", &token, &body).await?;
        let failed: Vec<u32> = take(&mut set, "failed_barriers").unwrap_or_default();
        if !failed.is_empty() {
            warn!("the desktop wouldn't watch edges {failed:?} of {wanted:?}");
        }
        self.call("Enable", Options::new())
            .await
            .context("turning capture on")?;
        self.barriers = wanted;
        Ok(())
    }

    /// Arm the barriers again (it fails harmlessly if they're armed already).
    async fn enable(&self) {
        if self.barriers.is_empty() {
            return;
        }
        if let Err(e) = self.call("Enable", Options::new()).await {
            debug!("turning capture back on: {e}");
        }
    }

    // ------------------------------------------------------------------------- capture

    /// The pointer pushed through a barrier and the desktop is holding it there.
    async fn activated(&mut self, mut o: Results) {
        let id = take::<u32>(&mut o, "activation_id");
        let at = take::<(f64, f64)>(&mut o, "cursor_position").map(|(x, y)| Point::new(x, y));
        let barrier = take::<u32>(&mut o, "barrier_id");
        debug!("capture {id:?} at {at:?} through barrier {barrier:?}");
        self.active = Some(id.unwrap_or_default());
        self.buttons.clear();
        let barrier = barrier
            .and_then(|b| self.barriers.get((b as usize).checked_sub(1)?))
            .or_else(|| {
                let p = at?;
                (self.barriers.iter()).min_by(|a, b| a.distance(p).total_cmp(&b.distance(p)))
            })
            .copied();
        let (Some(barrier), Some(at)) = (barrier, at) else {
            return self.release(None).await;
        };
        let (at, dx, dy) = barrier.crossing(at);
        if let Some(p) = self.left_on.take()
            && (p.x - at.x).abs() < 2.0
            && (p.y - at.y).abs() < 2.0
        {
            // The pointer moving off where the desktop left it, not pushing on through.
            return self.release(Some(at)).await;
        }
        self.held_at = at;
        self.feed(Input::Motion {
            at,
            dx,
            dy,
            dragging: false,
        })
        .await;
        if !self.grabbed {
            // Not crossing after all (being controlled, or the computer there is asleep):
            // give the pointer straight back.
            self.release(Some(at)).await;
        }
    }

    async fn deactivated(&mut self, mut o: Results) {
        let id = take::<u32>(&mut o, "activation_id");
        if self.active.is_none() || id.is_some_and(|id| Some(id) != self.active) {
            // One we released, or an old one.
            return;
        }
        // The desktop ended it (its own escape shortcut, say): the cursor comes home, and the
        // barriers go back on if that turned them off.
        debug!("the desktop ended capture {id:?}");
        self.desktop_ended().await;
    }

    async fn desktop_ended(&mut self) {
        if self.active.take().is_some() {
            // It leaves the pointer where it held it, on the barrier.
            self.left_on = Some(self.held_at);
        }
        self.go_home().await;
        self.enable().await;
    }

    /// Let the pointer go, at `warp` (where the controller says it came home).
    async fn release(&mut self, warp: Option<Point>) {
        self.grabbed = false;
        if let Some(id) = self.active.take() {
            let mut options = Options::from([("activation_id", Value::from(id))]);
            if let Some(p) = warp.map(|p| clear_of(&self.barriers, p)) {
                debug!("capture {id} over, the pointer back at {p:?}");
                options.insert("cursor_position", Value::from(Structure::from((p.x, p.y))));
            }
            if let Err(e) = self.call("Release", options).await {
                debug!("letting the pointer go: {e}");
            }
        }
        if self.stale {
            self.rearm().await;
        }
    }

    async fn go_home(&mut self) {
        let actions = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .release();
        self.apply(actions).await;
    }

    async fn feed(&mut self, input: Input) {
        let actions = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .handle(input)
            .actions;
        self.apply(actions).await;
    }

    async fn apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Grab => self.grabbed = true,
                Action::Release { warp } => self.release(Some(warp)).await,
                other => {
                    let _ = self.actions.send(other);
                }
            }
        }
    }

    async fn on_ei(&mut self, event: EiEvent) -> anyhow::Result<()> {
        match event {
            EiEvent::SeatAdded(e) => {
                e.seat.bind_capabilities(
                    DeviceCapability::Pointer
                        | DeviceCapability::Button
                        | DeviceCapability::Scroll
                        | DeviceCapability::Keyboard,
                );
                self.ei.flush().context("answering the desktop")?;
            }
            EiEvent::Disconnected(e) => {
                bail!("the desktop disconnected ({:?})", e.reason)
            }
            EiEvent::DeviceAdded(e) => debug!("captured device {:?}", e.device.name()),
            EiEvent::DeviceRemoved(e) => debug!("captured device {:?} gone", e.device.name()),
            _ if !self.grabbed => {}
            EiEvent::PointerMotion(e) => {
                let dragging = self.buttons.iter().any(|b| ev::is_button(*b));
                self.feed(Input::Motion {
                    at: self.held_at,
                    dx: e.dx.into(),
                    dy: e.dy.into(),
                    dragging,
                })
                .await;
            }
            EiEvent::Button(e) => {
                let (code, down) = (e.button as u16, e.state == ButtonState::Press);
                if down {
                    self.buttons.insert(code);
                } else {
                    self.buttons.remove(&code);
                }
                self.feed(Input::Button { code, down }).await;
            }
            // Wheels come in whole notches of 120; trackpads smoothly.
            EiEvent::ScrollDiscrete(e) => {
                let (x, y) = (e.discrete_dx, e.discrete_dy);
                let notches = (x % 120 == 0 && y % 120 == 0).then_some((x / 120, y / 120));
                self.feed(Input::Scroll(Scroll {
                    dx: x as f64 / 8.0,
                    dy: y as f64 / 8.0,
                    notches,
                }))
                .await;
            }
            EiEvent::ScrollDelta(e) => {
                self.feed(Input::Scroll(Scroll {
                    dx: e.dx.into(),
                    dy: e.dy.into(),
                    notches: None,
                }))
                .await;
            }
            EiEvent::KeyboardKey(e) => {
                let down = e.state == KeyState::Press;
                self.feed(Input::Key {
                    code: e.key as u16,
                    down,
                })
                .await;
            }
            _ => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------------------- D-Bus

    /// Call a portal method on our session that answers straight away.
    async fn call(&self, method: &str, options: Options<'_>) -> zbus::Result<()> {
        self.portal.call(method, &(&self.session, options)).await
    }

    /// Call one that answers later, through a request (and say no if the desktop did).
    async fn request<B>(&self, method: &str, token: &str, body: &B) -> anyhow::Result<Results>
    where
        B: serde::Serialize + zvariant::DynamicType,
    {
        request(&self.conn, &self.portal, method, token, body)
            .await?
            .map_err(|code| anyhow!("{method} failed ({code})"))
    }
}

/// Call a portal method that answers through a Request object, and wait for the answer: the
/// results, or the portal's code for why not (1: the person said no).
async fn request<B>(
    conn: &Connection,
    portal: &Proxy<'_>,
    method: &str,
    token: &str,
    body: &B,
) -> anyhow::Result<Result<Results, u32>>
where
    B: serde::Serialize + zvariant::DynamicType,
{
    let me = conn.unique_name().context("no name on the session bus")?;
    let me = me.trim_start_matches(':').replace('.', "_");
    // Listening before asking, so even an instant answer is heard.
    let path = format!("{DESKTOP_PATH}/request/{me}/{token}");
    let request = Proxy::new(conn, DESKTOP, path, "org.freedesktop.portal.Request").await?;
    let mut answers = request.receive_signal("Response").await?;
    let _: OwnedObjectPath = portal
        .call(method, body)
        .await
        .with_context(|| format!("asking the desktop portal to {method}"))?;
    let answer = answers
        .next()
        .await
        .context("the desktop portal went away")?;
    let (code, results): (u32, Results) = answer.body().deserialize()?;
    Ok(if code == 0 { Ok(results) } else { Err(code) })
}

/// `p`, moved off any left or top barrier it's on. The desktop puts the pointer back on a
/// whole pixel, and one exactly on a barrier sets it off again as soon as it moves sideways,
/// even away from the edge. (Right and bottom ones sit just past the last pixel.)
fn clear_of(barriers: &[Barrier], mut p: Point) -> Point {
    for b in barriers {
        match b.side {
            Side::Left if p.x < b.from.x + 1.0 && (b.from.y..b.to.y).contains(&p.y) => {
                p.x = b.from.x + 1.0
            }
            Side::Above if p.y < b.from.y + 1.0 && (b.from.x..b.to.x).contains(&p.x) => {
                p.y = b.from.y + 1.0
            }
            _ => {}
        }
    }
    p
}

fn next_token() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!("mousetail{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn handle(token: &str) -> Options<'_> {
    Options::from([("handle_token", Value::from(token))])
}

fn take<T: TryFrom<OwnedValue>>(o: &mut Results, key: &str) -> Option<T> {
    T::try_from(o.remove(key)?).ok()
}

/// An object path, which some portals send as a string.
fn take_path(o: &mut Results, key: &str) -> Option<OwnedObjectPath> {
    let v = o.remove(key)?;
    match v.try_clone().ok().map(OwnedObjectPath::try_from) {
        Some(Ok(path)) => Some(path),
        _ => OwnedObjectPath::try_from(String::try_from(v).ok()?).ok(),
    }
}

/// Don't ask the person again this run (see `GAVE_UP`).
fn give_up(e: anyhow::Error) -> anyhow::Error {
    let _ = GAVE_UP.set(format!("{e:#}"));
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barriers_sit_on_the_edge_with_inclusive_ends() {
        // The portal's own example: two 1920×1080 screens side by side.
        let left = Barrier {
            side: Side::Left,
            from: Point::new(0.0, 0.0),
            to: Point::new(0.0, 1080.0),
        };
        assert_eq!(left.position(), (0, 0, 0, 1079));
        let right = Barrier {
            side: Side::Right,
            from: Point::new(3840.0, 0.0),
            to: Point::new(3840.0, 1080.0),
        };
        assert_eq!(right.position(), (3840, 0, 3840, 1079));
        let below = Barrier {
            side: Side::Below,
            from: Point::new(1920.0, 1080.0),
            to: Point::new(3840.0, 1080.0),
        };
        assert_eq!(below.position(), (1920, 1080, 3839, 1080));
    }

    #[test]
    fn a_push_through_is_held_just_inside_heading_out() {
        let right = Barrier {
            side: Side::Right,
            from: Point::new(3840.0, 100.0),
            to: Point::new(3840.0, 500.0),
        };
        // Pushed well past the edge, a little above the stretch: held on it, inside.
        let (at, dx, dy) = right.crossing(Point::new(3852.5, 90.0));
        assert_eq!((at, dx, dy), (Point::new(3839.0, 100.0), 1.0, 0.0));
        let above = Barrier {
            side: Side::Above,
            from: Point::new(0.0, 0.0),
            to: Point::new(1920.0, 0.0),
        };
        let (at, dx, dy) = above.crossing(Point::new(700.0, -6.0));
        assert_eq!((at, dx, dy), (Point::new(700.0, 0.0), 0.0, -1.0));
    }

    #[test]
    fn the_pointer_goes_back_clear_of_left_and_top_barriers() {
        let barriers = [
            Barrier {
                side: Side::Left,
                from: Point::new(0.0, 420.0),
                to: Point::new(0.0, 1500.0),
            },
            Barrier {
                side: Side::Above,
                from: Point::new(1520.0, 318.0),
                to: Point::new(3440.0, 318.0),
            },
            Barrier {
                side: Side::Right,
                from: Point::new(3760.0, 462.0),
                to: Point::new(3760.0, 1542.0),
            },
        ];
        let back = |x, y| clear_of(&barriers, Point::new(x, y));
        assert_eq!(back(0.0, 794.0), Point::new(1.0, 794.0));
        assert_eq!(back(2000.0, 318.0), Point::new(2000.0, 319.0));
        // Off the end of the barrier, or already clear: left alone.
        assert_eq!(back(0.0, 1600.0), Point::new(0.0, 1600.0));
        assert_eq!(back(3759.0, 800.0), Point::new(3759.0, 800.0));
    }
}
