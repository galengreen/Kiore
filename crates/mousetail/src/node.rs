//! The MouseTail node: one per machine. Discovers peers, keeps a connection to each, pairs,
//! and routes input between the local capture/emulation backends and the network.
//!
//! Connection direction doesn't matter: both sides dial each other and the first
//! authenticated connection wins (ties broken deterministically), so a firewall that blocks
//! one direction is fine.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use mousetail_core::audio::{self, AudioPacket};
use mousetail_core::config::{Config, PeerConfig};
use mousetail_core::controller::{Action, Controller};
use mousetail_core::discovery::{self, Found};
use mousetail_core::identity::{Identity, id_from_fingerprint};
use mousetail_core::keys::{CommandRemap, ev};
use mousetail_core::layout::{Point, Rect, Side};
use mousetail_core::net;
use mousetail_core::net::Endpoints;
use mousetail_core::pairing::{self, PakeState, Role};
use mousetail_core::proto::{
    self, Datagram, DisplayInfo, Hello, MAX_FRAME, MAX_UNPAIRED_FRAME, MIN_PROTOCOL_VERSION,
    MediaKey, Message, Motion, PROTOCOL_VERSION, Platform,
};
use mousetail_core::quinn::{Connection, Endpoint, RecvStream};
use mousetail_core::ripple;
use mousetail_core::update;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use crate::paths::Paths;
use crate::platform::{self, MediaCommand};

/// A clipboard sent on its own stream can arrive before the crossing it's part of; wait this
/// long for the crossing before turning it away.
const CLIPBOARD_WAIT: Duration = Duration::from_secs(2);
/// Longest a clipboard transfer may take (10 MB over poor Wi-Fi).
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a new connection has to say hello.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a shown pairing code stays valid.
const CODE_LIFETIME: Duration = Duration::from_secs(120);
/// Silence (nothing is sent) for this long means the other computer's sound was paused.
const SOUND_STOPPED: Duration = Duration::from_secs(2);
/// Input this soon after another computer stops controlling this one may still be theirs.
const SETTLE_AFTER_VISIT: Duration = Duration::from_secs(3);
/// Sound still arriving this soon after pausing is the tail of what was playing.
const PAUSE_TAIL: Duration = Duration::from_secs(1);
/// Longest wait between tries of a paired computer that isn't advertising (seconds).
const REMEMBERED_RETRY_MAX: u64 = 15;
/// How often to search the network again while a paired computer is nowhere to be seen.
const SEARCH_WHILE_MISSING: Duration = Duration::from_secs(30);
/// A gap this much longer than the network check's tick means the computer was asleep.
const SLEPT: Duration = Duration::from_secs(20);
/// Minimum gap between pairing codes shown on this machine.
const PAIR_REQUEST_INTERVAL: Duration = Duration::from_secs(5);
/// Wrong codes allowed per window before pairing locks for the rest of it. With 4 digits,
/// guessing then takes weeks, with a notification on screen for every try.
const PAIR_MAX_FAILURES: usize = 3;
const PAIR_FAILURE_WINDOW: Duration = Duration::from_secs(600);

/// Machine-wide pairing limits.
#[derive(Default)]
struct PairGuard {
    last_shown: Option<Instant>,
    failures: std::collections::VecDeque<Instant>,
}

const PAIR_LOCKED: &str = "too many wrong codes; try again in a few minutes";

impl PairGuard {
    /// Too many wrong codes lately: no new codes, and no guesses at codes already shown.
    fn locked(&mut self) -> bool {
        while self
            .failures
            .front()
            .is_some_and(|t| t.elapsed() > PAIR_FAILURE_WINDOW)
        {
            self.failures.pop_front();
        }
        self.failures.len() >= PAIR_MAX_FAILURES
    }

    fn allow(&mut self) -> Result<(), String> {
        if self.locked() {
            return Err(PAIR_LOCKED.into());
        }
        if self
            .last_shown
            .is_some_and(|t| t.elapsed() < PAIR_REQUEST_INTERVAL)
        {
            return Err("a code was just shown; wait a moment".into());
        }
        self.last_shown = Some(Instant::now());
        Ok(())
    }

    fn failed(&mut self) {
        self.failures.push_back(Instant::now());
    }

    fn succeeded(&mut self) {
        self.failures.pop_back();
    }
}

/// What to dial: everyone advertising, plus paired computers at their remembered addresses
/// (after any advertised ones), leaving out those connected already. Each comes with whether
/// it's advertising.
fn dial_targets(
    advertised: HashMap<String, Vec<SocketAddr>>,
    remembered: Vec<(String, Vec<SocketAddr>)>,
    connected: &HashSet<String>,
    own_id: &str,
) -> HashMap<String, (Vec<SocketAddr>, bool)> {
    let mut wanted: HashMap<String, (Vec<SocketAddr>, bool)> = advertised
        .into_iter()
        .map(|(id, addrs)| (id, (addrs, true)))
        .collect();
    for (id, addrs) in remembered {
        let (all, _) = wanted.entry(id).or_insert((vec![], false));
        for a in addrs {
            if !all.contains(&a) {
                all.push(a);
            }
        }
    }
    wanted.retain(|id, (addrs, _)| !connected.contains(id) && id != own_id && !addrs.is_empty());
    wanted
}

/// Locks are only ever taken in this order: config, peers, discovered, dials, controller, target,
/// pairing. Nothing is sent to a peer (which takes `peers`) while a later lock is held.
/// `pair_guard`, `left_peer`, `keyboard_warned`, `told_not_paired`, `clipboard` and `displays`
/// are leaves: nothing else is locked while holding them.
pub struct Node {
    id: String,
    name: String,
    identity: Identity,
    config: Mutex<Config>,
    config_path: PathBuf,
    endpoints: Endpoints,
    port: u16,
    peers: Mutex<HashMap<String, Peer>>,
    /// Computers advertising on the network right now.
    discovered: Mutex<HashMap<String, Found>>,
    /// Computers we're trying to reach (advertising, or paired and remembered).
    dials: Mutex<HashMap<String, Dial>>,
    /// Asks the network afresh who's there.
    search: discovery::Search,
    /// The firewall keeping other computers from dialling us, if any (see `firewall`).
    firewall: Mutex<Option<&'static str>>,
    pairing: Mutex<HashMap<String, Pairing>>,
    displays: Mutex<Vec<DisplayInfo>>,
    controller: Arc<Mutex<Controller>>,
    /// Set once input capture is running (it may wait for the user to grant permission).
    capture: OnceLock<platform::Capture>,
    capture_error: Mutex<Option<String>>,
    /// Set once input injection is available (it may wait for permission, as on macOS).
    target: OnceLock<Mutex<Target>>,
    clipboard: Arc<Mutex<ClipboardState>>,
    /// Fires whenever another computer takes control of this one, for clipboards that arrive
    /// ahead of their crossing.
    entered: tokio::sync::Notify,
    keyboard_warned: Mutex<Option<Instant>>,
    pair_guard: Mutex<PairGuard>,
    /// The peer the cursor most recently left and when, so its clipboard is accepted.
    left_peer: Mutex<Option<(String, Instant)>>,
    /// Latest pointer position sent, re-sent once the pointer rests (see `settle_loop`).
    settle: watch::Sender<Option<(String, Motion)>>,
    last_wake: Mutex<HashMap<String, Instant>>,
    /// When we last told each computer we're not paired with it. Releases before `NotPaired`
    /// existed hang up on it and redial, so don't say it on every connection.
    told_not_paired: Mutex<HashMap<String, Instant>>,
    /// Plays other machines' sound here (macOS today).
    player: Option<platform::AudioPlayer>,
    /// Our sound going to another machine: (peer, connection, speaker).
    audio_out: Mutex<Option<(String, usize, platform::AudioSource)>>,
    /// Held while a speaker is set up or taken down (both slow): one at a time, so two can't
    /// be made at once or one taken down after its replacement is up.
    audio_busy: Arc<Mutex<()>>,
    /// Sends this machine's media controls to the computer whose sound is playing here.
    now_playing: Option<platform::NowPlaying>,
    /// Draws the ripple where the cursor crosses (not everywhere can).
    ripples: Option<platform::Ripples>,
    listening: Mutex<Listening>,
    /// When another computer last controlled this one (its input isn't someone sitting here).
    last_controlled: Mutex<Option<Instant>>,
    /// Ourselves, for handing work to background threads from `&self` methods.
    me: OnceLock<std::sync::Weak<Node>>,
    /// Keeps this machine up to date (Linux; the Mac app updates itself).
    pub updater: crate::update::Updater,
    /// Asked to stop over the control socket.
    stop: tokio::sync::Notify,
}

#[derive(Clone)]
struct Peer {
    conn: Connection,
    tx: mpsc::UnboundedSender<Message>,
    hello: Hello,
    fingerprint: String,
    initiator: String,
    paired: bool,
    /// What it can do with sound (`SoundCaps`): (play, share). `None` until it says.
    sound: Option<(bool, bool)>,
}

impl Peer {
    /// What it can do with sound: (play, share). Peers that never say predate sound going
    /// both ways: there a Mac only plays sound and Linux only sends it.
    fn sound_caps(&self) -> (bool, bool) {
        self.sound.unwrap_or(match self.hello.platform {
            Platform::MacOs => (true, false),
            Platform::Linux => (false, true),
            _ => (false, false),
        })
    }
}

/// What we know about other computers' clipboards.
#[derive(Default)]
struct ClipboardState {
    /// What each one's clipboard holds, as far as we know (it took ours, or sent us its own),
    /// so the same thing isn't sent again or echoed back.
    known: HashMap<String, [u8; 32]>,
    /// The newest clipboard stream taken from each (streams are numbered in order on a
    /// connection), so a slow older one can't land on top of a newer one.
    newest: HashMap<String, u64>,
}

struct Dial {
    /// Where it's tried, so new addresses are tried straight away.
    addrs: Vec<SocketAddr>,
    /// Advertising itself now, rather than only remembered from before.
    advertised: bool,
    dialing: bool,
    failures: u32,
    next_attempt: Instant,
}

impl Dial {
    /// Wait before trying again after a failure. Something advertising is most likely there
    /// and a failure a brief blip (Wi-Fi hopping channels, a laptop waking up), so stay eager;
    /// a remembered address may be asleep or gone, so ease off, but not so far that a waking
    /// computer waits long.
    fn backoff(&self) -> Duration {
        let secs = if self.advertised {
            1u64 << self.failures.min(2)
        } else {
            (1u64 << self.failures.min(4)).min(REMEMBERED_RETRY_MAX)
        };
        Duration::from_secs(secs)
    }
}

enum Pairing {
    /// We showed a code and wait for the other side to use it. Each code allows exactly one
    /// attempt: after that (right or wrong) a new code is needed, so it can't be guessed.
    Shown {
        code: String,
        expires: Instant,
        key: Option<Vec<u8>>,
    },
    /// We typed a code and wait for the exchange to finish.
    Typed {
        state: Option<PakeState>,
        key: Option<Vec<u8>>,
        done: Option<oneshot::Sender<Result<(), String>>>,
    },
}

/// Whose sound is playing here, so media controls can go back to it.
#[derive(Default)]
struct Listening {
    source: Option<String>,
    playing: bool,
    last_sound: Option<Instant>,
    /// Paused from here: ignore sound until then (see `PAUSE_TAIL`).
    paused_until: Option<Instant>,
}

/// Receiving side: turns messages from the active controller into injected input.
struct Target {
    emulator: platform::Emulator,
    remap: CommandRemap,
    active: Option<String>,
    remap_active: bool,
    /// Newest motion applied, and whose it was: kept across a Leave and Enter so a motion
    /// delayed from before can't jump the cursor back; reset for a new controller or
    /// connection, whose count starts again.
    last_seq: u64,
    seq_from: Option<String>,
    /// Where we last put the active controller's cursor (local coordinates).
    at: Point,
}

impl Node {
    /// Run until told to stop. `exit_with_parent`: also stop when whatever started us exits
    /// (the Mac app), so a crashed app can't leave its daemon sharing input behind it.
    pub async fn run(paths: Paths, exit_with_parent: bool) -> anyhow::Result<()> {
        let _instance = single_instance(&paths)?;
        let (config, problem) = Config::load_or_recover(&paths.config).context("reading config")?;
        if let Some(problem) = problem {
            warn!("{problem}");
            platform::notify(
                "MouseTail",
                "Its settings file was damaged. Anything unreadable was reset; you may need to \
                 pair again.",
            );
        }
        let identity = Identity::load_or_create(&paths.dir).context("loading identity")?;
        let id = identity.id();
        let name = config.name.clone().unwrap_or_else(platform::machine_name);
        let endpoints = Endpoints::new(&identity, config.port)?;
        let port = endpoints.port();
        let displays = platform::displays();

        let (action_tx, action_rx) = mpsc::unbounded_channel();
        let controller = Arc::new(Mutex::new(Controller::new(&id, displays.clone())));
        let player = platform::AudioPlayer::start().ok();
        let (media_tx, media_rx) = mpsc::unbounded_channel();
        let now_playing = player
            .as_ref()
            .and_then(|_| platform::NowPlaying::start(media_tx).ok());

        let node = Arc::new(Node {
            id: id.clone(),
            name: name.clone(),
            identity,
            config: Mutex::new(config),
            config_path: paths.config.clone(),
            endpoints,
            port,
            peers: Mutex::default(),
            discovered: Mutex::default(),
            dials: Mutex::default(),
            search: discovery::Search::default(),
            firewall: Mutex::new(None),
            pairing: Mutex::default(),
            displays: Mutex::new(displays),
            controller,
            capture: OnceLock::new(),
            capture_error: Mutex::new(None),
            target: OnceLock::new(),
            clipboard: Arc::default(),
            entered: tokio::sync::Notify::new(),
            keyboard_warned: Mutex::new(None),
            pair_guard: Mutex::default(),
            left_peer: Mutex::new(None),
            settle: watch::Sender::new(None),
            last_wake: Mutex::default(),
            told_not_paired: Mutex::default(),
            player,
            audio_out: Mutex::new(None),
            audio_busy: Arc::default(),
            now_playing,
            ripples: platform::Ripples::start()
                .inspect_err(|e| debug!("no crossing ripple: {e:#}"))
                .ok(),
            listening: Mutex::default(),
            last_controlled: Mutex::new(None),
            me: OnceLock::new(),
            updater: Default::default(),
            stop: tokio::sync::Notify::new(),
        });
        info!(
            "MouseTail {} as {name:?} ({id}) on UDP {port}",
            env!("CARGO_PKG_VERSION"),
        );
        let _ = node.me.set(Arc::downgrade(&node));
        node.add_offline_peers(None);
        if platform::Capture::supported() {
            tokio::spawn(node.clone().start_capture(action_tx));
        }
        tokio::spawn(node.clone().start_target());

        let (_discovery, found_rx) = discovery::start(&id, &name, port, node.search.clone())?;

        for endpoint in node.endpoints.all() {
            tokio::spawn(node.clone().accept_loop(endpoint));
        }
        tokio::spawn(node.clone().network_loop());
        tokio::spawn(node.clone().firewall_loop());
        tokio::spawn(node.clone().discovery_loop(found_rx));
        tokio::spawn(node.clone().dial_loop());
        tokio::spawn(node.clone().action_loop(action_rx));
        tokio::spawn(node.clone().settle_loop());
        tokio::spawn(node.clone().media_loop(media_rx));
        tokio::spawn(node.clone().display_loop());
        tokio::spawn(crate::ipc::serve(node.clone(), paths.socket.clone()));
        tokio::spawn(crate::update::run(node.clone()));

        tokio::select! {
            () = shutdown_signal() => {}
            () = node.stop.notified() => {}
            () = parent_exited(), if exit_with_parent => info!("the app that started us has gone"),
        }
        info!("shutting down");
        let actions = node.controller.lock().unwrap().release();
        node.apply_actions(actions);
        if let Some(t) = node.target.get() {
            t.lock().unwrap().emulator.release_all();
        }
        node.endpoints.close();
        let _ = std::fs::remove_file(&paths.socket);
        Ok(())
    }

    /// Put paired computers in the layout at their remembered spots before they connect, so
    /// pushing towards one that's asleep can wake it.
    fn add_offline_peers(&self, only: Option<&str>) {
        let peers = self.config.lock().unwrap().peers.clone();
        let mut controller = self.controller.lock().unwrap();
        for p in peers {
            if let Some(offset) = p.placement
                && !p.displays.is_empty()
                && !p.paused
                && only.is_none_or(|id| id == p.id)
            {
                controller.set_peer(&p.id, p.displays, offset);
            }
        }
    }

    /// Start injecting input (being controlled), waiting quietly for permission if needed.
    async fn start_target(self: Arc<Self>) {
        let mut logged = false;
        loop {
            match platform::Emulator::start() {
                Ok(emulator) => {
                    let _ = self.target.set(Mutex::new(Target {
                        emulator,
                        remap: CommandRemap::new(platform::command_super_keys()),
                        active: None,
                        remap_active: false,
                        last_seq: 0,
                        seq_from: None,
                        at: Point::default(),
                    }));
                    info!("accepting input from other computers");
                    self.announce();
                    return;
                }
                Err(e) if !platform::Emulator::supported() => {
                    info!("not accepting input here: {e:#}");
                    return;
                }
                Err(e) => {
                    if !logged {
                        warn!("can't accept input yet: {e:#}");
                        logged = true;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Tell connected computers what we can do now (a role just became available).
    fn announce(&self) {
        let peers: Vec<(String, bool)> = self
            .peers
            .lock()
            .unwrap()
            .iter()
            .map(|(id, p)| (id.clone(), p.paired))
            .collect();
        for (id, paired) in peers {
            self.send(&id, Message::Hello(self.hello(&id, paired)));
        }
    }

    /// Start capturing input, waiting quietly for permission if it hasn't been granted yet.
    async fn start_capture(self: Arc<Self>, actions: mpsc::UnboundedSender<Action>) {
        let mut prompt = true;
        loop {
            // On its own thread: it can wait as long as someone takes to answer GNOME's
            // permission dialog, and quitting waits for blocking tasks but not for threads.
            let (controller, actions) = (self.controller.clone(), actions.clone());
            let (done, started) = oneshot::channel();
            std::thread::spawn(move || {
                let _ = done.send(platform::Capture::start(controller, actions, prompt));
            });
            let started = started
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("starting capture stopped")));
            match started {
                Ok(c) => {
                    let _ = self.capture.set(c);
                    *self.capture_error.lock().unwrap() = None;
                    info!("capturing keyboard and mouse");
                    let ids: Vec<String> = self.peers.lock().unwrap().keys().cloned().collect();
                    for id in ids {
                        self.peer_up(&id);
                    }
                    self.refresh_edges();
                    self.announce();
                    return;
                }
                Err(e) => {
                    if prompt {
                        warn!("can't capture input yet: {e:#}");
                    }
                    *self.capture_error.lock().unwrap() = Some(format!("{e:#}"));
                }
            }
            prompt = false;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// What we tell a computer about ourselves. Wake-on-LAN addresses only go to paired ones;
    /// a paused one hears we can't take input, which is all a release before `Paused` knows.
    fn hello(&self, id: &str, paired: bool) -> Hello {
        Hello {
            protocol: PROTOCOL_VERSION,
            name: self.name.clone(),
            platform: Platform::current(),
            displays: self.displays.lock().unwrap().clone(),
            can_control: platform::Capture::supported(),
            can_be_controlled: self.target.get().is_some() && !self.is_paused(id),
            wake_macs: if paired {
                platform::wake_macs()
            } else {
                vec![]
            },
        }
    }

    // -----------------------------------------------------------------------------------------
    // Connections

    /// Follow network changes (Wi-Fi joins, cables plugged in) and accept on new addresses.
    /// After a network change or a sleep, look for everyone again at once.
    async fn network_loop(self: Arc<Self>) {
        const TICK: Duration = Duration::from_secs(5);
        let mut tick = tokio::time::interval(TICK);
        let mut addrs = self.endpoints.addrs();
        // Wall-clock time: the monotonic clock stops while asleep, on macOS and Linux alike.
        let mut last = std::time::SystemTime::now();
        loop {
            tick.tick().await;
            for endpoint in self.endpoints.refresh() {
                debug!("listening on {:?}", endpoint.local_addr());
                tokio::spawn(self.clone().accept_loop(endpoint));
            }
            let now = std::time::SystemTime::now();
            let slept = now.duration_since(last).is_ok_and(|gap| gap > TICK + SLEPT);
            last = now;
            let now_addrs = self.endpoints.addrs();
            if slept || now_addrs != addrs {
                debug!(
                    "{}: looking for everyone again",
                    if slept { "awake" } else { "network changed" }
                );
                addrs = now_addrs;
                self.search.again();
                self.dial_everyone_now();
            }
        }
    }

    /// Keep an eye on the firewall, which can change (or be fixed) at any time.
    async fn firewall_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            let port = self.port;
            let Ok(now) =
                tokio::task::spawn_blocking(move || crate::firewall::blocking(port)).await
            else {
                continue;
            };
            let before = std::mem::replace(&mut *self.firewall.lock().unwrap(), now);
            if now != before {
                match now {
                    Some(f) => warn!(
                        "the firewall ({f}) stops other computers reaching this one on UDP {port}; \
                         run ~/.local/share/mousetail/enable-firewall.sh to fix it"
                    ),
                    None if before.is_some() => info!("the firewall lets other computers in now"),
                    None => {}
                }
            }
        }
    }

    /// Forget any backoff: something changed, so every computer is worth trying again now.
    fn dial_everyone_now(&self) {
        let now = Instant::now();
        for d in self.dials.lock().unwrap().values_mut() {
            d.failures = 0;
            d.next_attempt = now;
        }
    }

    async fn accept_loop(self: Arc<Self>, endpoint: Endpoint) {
        while let Some(incoming) = endpoint.accept().await {
            let node = self.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => node.run_connection(conn, false).await,
                    Err(e) => debug!("incoming connection failed: {e}"),
                }
            });
        }
    }

    async fn discovery_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<discovery::Event>) {
        while let Some(event) = rx.recv().await {
            match event {
                discovery::Event::Found(found) => {
                    debug!(
                        "discovered {} ({}) at {:?}",
                        found.name, found.id, found.addrs
                    );
                    let newer = found
                        .version
                        .as_deref()
                        .is_some_and(|v| update::is_newer(v, update::VERSION));
                    let id = found.id.clone();
                    let mut discovered = self.discovered.lock().unwrap();
                    discovered.insert(id.clone(), found);
                    // Just heard from: it's there, so don't sit out a backoff.
                    if let Some(d) = self.dials.lock().unwrap().get_mut(&id) {
                        d.failures = 0;
                        d.next_attempt = Instant::now();
                    }
                    drop(discovered);
                    if newer {
                        self.updater.nudge();
                    }
                }
                discovery::Event::Lost(id) => {
                    debug!("{id} stopped advertising");
                    self.discovered.lock().unwrap().remove(&id);
                }
            }
        }
    }

    /// Dial every computer we aren't connected to: those advertising, and paired ones at
    /// their remembered addresses (mDNS can be slow, filtered, or confused, and a firewall
    /// that blocks them dialling us lets us dial them). Back off on failure.
    async fn dial_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut last_search = Instant::now();
        loop {
            tick.tick().await;
            let remembered: Vec<(String, Vec<SocketAddr>)> = self
                .config
                .lock()
                .unwrap()
                .peers
                .iter()
                .map(|p| (p.id.clone(), p.addrs.clone()))
                .collect();
            let connected: HashSet<String> = self.peers.lock().unwrap().keys().cloned().collect();
            let advertised: HashMap<String, Vec<SocketAddr>> = self
                .discovered
                .lock()
                .unwrap()
                .iter()
                .map(|(id, f)| (id.clone(), f.addrs.clone()))
                .collect();

            let missing = remembered
                .iter()
                .any(|(id, _)| !connected.contains(id) && !advertised.contains_key(id));
            if missing && last_search.elapsed() >= SEARCH_WHILE_MISSING {
                last_search = Instant::now();
                self.search.again();
            }

            let wanted = dial_targets(advertised, remembered, &connected, &self.id);
            let now = Instant::now();
            let due: Vec<(String, Vec<SocketAddr>)> = {
                let mut dials = self.dials.lock().unwrap();
                dials.retain(|id, d| d.dialing || wanted.contains_key(id));
                wanted
                    .into_iter()
                    .filter_map(|(id, (addrs, advertised))| {
                        let d = dials.entry(id.clone()).or_insert(Dial {
                            addrs: vec![],
                            advertised,
                            dialing: false,
                            failures: 0,
                            next_attempt: now,
                        });
                        if d.addrs != addrs {
                            d.addrs = addrs.clone();
                            d.failures = 0;
                            d.next_attempt = now;
                        }
                        d.advertised = advertised;
                        (!d.dialing && d.next_attempt <= now).then(|| {
                            d.dialing = true;
                            (id, addrs)
                        })
                    })
                    .collect()
            };
            for (id, addrs) in due {
                let node = self.clone();
                tokio::spawn(async move {
                    let result = net::connect_fastest(&node.endpoints.attempts(&addrs)).await;
                    if let Some(d) = node.dials.lock().unwrap().get_mut(&id) {
                        // Connected: still "dialing" until the connection ends, so it isn't
                        // dialled again meanwhile.
                        d.dialing = result.is_ok();
                        if result.is_err() {
                            d.failures += 1;
                            d.next_attempt = Instant::now() + d.backoff();
                        } else {
                            d.failures = 0;
                        }
                    }
                    match result {
                        Ok(conn) => {
                            node.clone().run_connection(conn, true).await;
                            if let Some(d) = node.dials.lock().unwrap().get_mut(&id) {
                                d.dialing = false;
                            }
                        }
                        Err(e) => debug!("couldn't reach {id} at {addrs:?}: {e:#}"),
                    }
                });
            }
        }
    }

    async fn run_connection(self: Arc<Self>, conn: Connection, we_dialed: bool) {
        let remote = conn.remote_address();
        if let Err(e) = self.clone().connection(conn, we_dialed).await {
            debug!("connection with {remote} ended: {e:#}");
        }
    }

    async fn connection(self: Arc<Self>, conn: Connection, we_dialed: bool) -> anyhow::Result<()> {
        let fingerprint = net::peer_fingerprint(&conn).context("peer sent no certificate")?;
        let id = id_from_fingerprint(&fingerprint);
        if id == self.id {
            conn.close(0u32.into(), b"self");
            return Ok(());
        }
        let paired = self.config.lock().unwrap().is_paired(&fingerprint);
        let handshake = async {
            let (mut send, mut recv) = if we_dialed {
                conn.open_bi().await?
            } else {
                conn.accept_bi().await?
            };
            net::write_message(&mut send, &Message::Hello(self.hello(&id, paired))).await?;
            let Message::Hello(hello) = net::read_message(&mut recv, MAX_UNPAIRED_FRAME).await?
            else {
                anyhow::bail!("expected hello");
            };
            Ok((send, recv, hello))
        };
        let (mut send, mut recv, hello) = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
            .await
            .context("handshake timed out")??;
        anyhow::ensure!(
            hello.protocol >= MIN_PROTOCOL_VERSION,
            "{} speaks protocol {}, too old for this version (needs {MIN_PROTOCOL_VERSION})",
            hello.name,
            hello.protocol
        );
        let initiator = if we_dialed {
            self.id.clone()
        } else {
            id.clone()
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let peer = Peer {
            conn: conn.clone(),
            tx,
            hello: hello.clone(),
            fingerprint,
            initiator: initiator.clone(),
            paired,
            sound: None,
        };
        let replaced = {
            let mut peers = self.peers.lock().unwrap();
            let mut replaced = false;
            if let Some(existing) = peers.get(&id) {
                // Both sides apply the same rule: keep the connection started by the smaller
                // id; a newer connection from the same initiator replaces a stale one.
                let keep_new = existing.initiator == initiator || initiator < existing.initiator;
                if !keep_new {
                    conn.close(0u32.into(), b"duplicate");
                    return Ok(());
                }
                existing.conn.close(0u32.into(), b"replaced");
                replaced = true;
            }
            peers.insert(id.clone(), peer);
            replaced
        };
        if replaced {
            // Whatever was in flight on the old link is lost (key releases included), so
            // start clean: cursor home, remote keys released.
            self.peer_down(&id);
        }
        info!(
            "connected to {} ({id}) via {} [{}]",
            hello.name,
            conn.remote_address(),
            if paired { "paired" } else { "not paired" }
        );
        if paired {
            self.reached(&id, conn.remote_address());
            self.share_paused(&id);
        }
        self.peer_up(&id);

        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if net::write_message(&mut send, &msg).await.is_err() {
                    break;
                }
            }
        });

        let stable_id = conn.stable_id();
        // Clipboards on streams of their own (protocol 5).
        let clipboards = tokio::spawn({
            let node = self.clone();
            let conn = conn.clone();
            let id = id.clone();
            async move {
                while let Ok((send, recv)) = conn.accept_bi().await {
                    let node = node.clone();
                    let id = id.clone();
                    tokio::spawn(async move {
                        let taken = node.clipboard_stream(&id, stable_id, recv).await;
                        net::answer_clipboard(send, taken).await;
                    });
                }
            }
        });
        let datagrams = {
            let node = self.clone();
            let conn = conn.clone();
            let id = id.clone();
            async move {
                while let Ok(bytes) = conn.read_datagram().await {
                    match proto::decode::<Datagram>(&bytes) {
                        Ok(Datagram::Motion(motion)) => node.on_motion(&id, stable_id, motion),
                        Ok(Datagram::Audio(packet)) => node.on_audio(&id, stable_id, packet),
                        Err(_) => {}
                    }
                }
            }
        };
        let reader = {
            let node = self.clone();
            let id = id.clone();
            async move {
                let mut skipped = false;
                loop {
                    let paired = node
                        .peers
                        .lock()
                        .unwrap()
                        .get(&id)
                        .is_some_and(|p| p.paired);
                    let max = if paired {
                        MAX_FRAME
                    } else {
                        MAX_UNPAIRED_FRAME
                    };
                    let frame = net::read_frame(&mut recv, max).await?;
                    match proto::decode::<Message>(&frame) {
                        Ok(msg) => node.on_message(&id, stable_id, msg),
                        // Most likely something a newer release added: skip it, don't hang up.
                        Err(e) if !skipped => {
                            skipped = true;
                            warn!("skipping a message from {id} this version doesn't know: {e}");
                        }
                        Err(_) => {}
                    }
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            }
        };
        let result = tokio::select! {
            r = reader => r,
            _ = datagrams => Ok(()),
        };
        writer.abort();
        clipboards.abort();

        let removed = {
            let mut peers = self.peers.lock().unwrap();
            match peers.get(&id) {
                Some(p) if p.conn.stable_id() == conn.stable_id() => peers.remove(&id).is_some(),
                _ => false,
            }
        };
        if removed {
            info!("disconnected from {}", hello.name);
            self.peer_down(&id);
        }
        result
    }

    /// Remember where a paired computer was reached, to dial it there again if it can't be
    /// found on the network.
    fn reached(&self, id: &str, addr: SocketAddr) {
        let addr = SocketAddr::new(addr.ip().to_canonical(), addr.port());
        let mut config = self.config.lock().unwrap();
        if config.peer_mut(id).is_some_and(|p| p.reached_at(addr))
            && let Err(e) = config.save(&self.config_path)
        {
            warn!("couldn't save config: {e:#}");
        }
    }

    fn peer(&self, id: &str) -> Option<Peer> {
        self.peers.lock().unwrap().get(id).cloned()
    }

    fn send(&self, id: &str, msg: Message) {
        if let Some(p) = self.peers.lock().unwrap().get(id)
            && let Some(msg) = msg.for_protocol(p.hello.protocol)
        {
            let _ = p.tx.send(msg);
        }
    }

    /// A paired peer is connected (or its displays changed): put it in the layout.
    fn peer_up(&self, id: &str) {
        let Some(peer) = self.peer(id) else { return };
        if !peer.paired {
            return;
        }
        // Remember what it looks like, so it can be shown and woken while offline.
        let known = self
            .config
            .lock()
            .unwrap()
            .peer(id)
            .map(|p| (p.displays.clone(), p.wake_macs.clone(), p.name.clone()));
        let now = (
            peer.hello.displays.clone(),
            peer.hello.wake_macs.clone(),
            peer.hello.name.clone(),
        );
        if known.is_some_and(|k| k != now) {
            self.update_config(|c| {
                if let Some(p) = c.peer_mut(id) {
                    (p.displays, p.wake_macs, p.name) = now;
                }
            });
        }
        if self.is_paused(id) {
            return;
        }
        if peer.hello.protocol >= proto::SKIPS_UNKNOWN {
            self.send(
                id,
                Message::SoundCaps {
                    can_play: self.player.is_some(),
                    can_share: platform::AudioSource::supported(),
                },
            );
        }
        self.request_audio(id, &peer);
        self.share_placement(id);
        if self.capture.get().is_none() {
            return;
        }
        if !peer.hello.can_be_controlled {
            // Connected, but it can't take input (yet): not somewhere to push the cursor, nor
            // a sleeping computer to keep sending wake-ups to.
            let actions = self.controller.lock().unwrap().remove_peer(id);
            self.apply_actions(actions);
            self.refresh_edges();
            return;
        }
        let placement = self
            .config
            .lock()
            .unwrap()
            .peer(id)
            .and_then(|p| p.placement);
        let mut controller = self.controller.lock().unwrap();
        let displays = peer.hello.displays.clone();
        let mut actions = controller.set_peer(id, displays.clone(), placement.unwrap_or_default());
        // First time: put it to the left of the primary display. Easy to change later.
        let first_placement = placement
            .is_none()
            .then(|| controller.offset_beside(id, Side::Left, None))
            .flatten();
        if let Some(offset) = first_placement {
            actions.extend(controller.set_peer(id, displays, offset));
        }
        actions.extend(controller.set_reachable(id, true));
        drop(controller);
        self.refresh_edges();
        if let Some(offset) = first_placement {
            self.update_config(|c| {
                if let Some(p) = c.peer_mut(id) {
                    p.placement = Some(offset);
                }
            });
        }
        self.apply_actions(actions);
    }

    /// Tell the capture backend which of our edges lead to other computers.
    fn refresh_edges(&self) {
        let Some(capture) = self.capture.get() else {
            return;
        };
        let displays = self.displays.lock().unwrap().clone();
        let sides = self.controller.lock().unwrap().layout().exit_sides(0);
        let edges = sides
            .into_iter()
            .filter_map(|(d, side)| {
                displays.get(d).map(|display| platform::Edge {
                    display: display.id.clone(),
                    side,
                    rect: display.rect,
                })
            })
            .collect();
        capture.set_edges(edges);
    }

    /// Tell a peer where it sits in our arrangement, so it can mirror it.
    fn share_placement(&self, id: &str) {
        let placed = self
            .config
            .lock()
            .unwrap()
            .peer(id)
            .and_then(|p| p.placement.map(|o| (o, p.placement_updated)));
        if let Some((o, updated)) = placed {
            self.send(
                id,
                Message::Placement {
                    x: o.x,
                    y: o.y,
                    updated,
                },
            );
        }
    }

    /// A peer told us where we sit in its arrangement. Mirror it if it's newer than ours (or
    /// we have none), so arranging on either computer arranges both. Ties between two
    /// automatic placements go to the computer with the smaller id, so both agree.
    fn adopt_placement(&self, id: &str, us_in_theirs: Point, updated: u64) {
        let mirror = Point::new(-us_in_theirs.x, -us_in_theirs.y);
        let (own, own_updated) = match self.config.lock().unwrap().peer(id) {
            Some(p) => (p.placement, p.placement_updated),
            None => return,
        };
        let newer = updated > own_updated || (updated == own_updated && id < self.id.as_str());
        if own == Some(mirror) {
            return;
        }
        if own.is_some() && !newer {
            // Ours wins. Say so, in case they never heard it (e.g. it arrived before they
            // finished pairing), so both computers end up with the same arrangement.
            self.share_placement(id);
            return;
        }
        debug!("adopting {id}'s arrangement");
        self.update_config(|c| {
            if let Some(p) = c.peer_mut(id) {
                p.placement = Some(mirror);
                p.placement_updated = updated;
            }
        });
        self.peer_up(id);
    }

    fn peer_down(&self, id: &str) {
        self.stop_audio(Some(id));
        self.stop_listening(Some(id));
        {
            // Its clipboard may change while it's away; and stream numbers start again.
            let mut clipboard = self.clipboard.lock().unwrap();
            clipboard.known.remove(id);
            clipboard.newest.remove(id);
        }
        let actions = self.controller.lock().unwrap().set_reachable(id, false);
        self.apply_actions(actions);
        let was_controlling_us = self.target.get().is_some_and(|t| {
            let mut t = t.lock().unwrap();
            if t.seq_from.as_deref() == Some(id) {
                t.seq_from = None;
            }
            let active = t.active.as_deref() == Some(id);
            if active {
                t.leave();
            }
            active
        });
        if was_controlling_us {
            self.controller.lock().unwrap().set_controlled_by(None);
        }
        self.pairing.lock().unwrap().remove(id);
    }

    // -----------------------------------------------------------------------------------------
    // Controlling side

    async fn action_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Action>) {
        while let Some(action) = rx.recv().await {
            self.apply_actions(vec![action]);
        }
    }

    fn apply_actions(&self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Grab | Action::Release { .. } => {
                    if let Some(c) = self.capture.get() {
                        c.apply(&action);
                    }
                }
                Action::Send { peer, msg } => {
                    if matches!(msg, Message::Leave) {
                        *self.left_peer.lock().unwrap() = Some((peer.clone(), Instant::now()));
                    }
                    let entering = matches!(msg, Message::Enter { .. });
                    self.send(&peer, msg);
                    if entering {
                        debug!("cursor → {peer}");
                        self.claim_sound(&peer);
                        // Taking the cursor back to the computer that was controlling us.
                        if let Some(t) = self.target.get() {
                            let mut t = t.lock().unwrap();
                            if t.active.as_deref() == Some(peer.as_str()) {
                                t.leave();
                            }
                        }
                        // Our clipboard travels with the cursor. After Enter on the same
                        // ordered stream, so the other side knows it's part of the crossing.
                        self.push_clipboard(&peer);
                        self.warn_if_keyboard_blocked();
                        // The other side starts each visit with Caps Lock off; match ours.
                        if platform::caps_lock_on() {
                            for down in [true, false] {
                                self.send(
                                    &peer,
                                    Message::Key {
                                        code: ev::CAPSLOCK,
                                        down,
                                    },
                                );
                            }
                        }
                    }
                }
                Action::Motion { peer, motion } => {
                    self.send_motion(&peer, motion);
                    self.settle.send_replace(Some((peer, motion)));
                }
                Action::Wake { peer } => self.wake(&peer),
                Action::Crossed { at, arrived } => self.ripple(at, arrived),
            }
        }
    }

    /// The ripple where the cursor crossed an edge here (local coordinates), unless it's
    /// been turned off.
    fn ripple(&self, at: Point, arrived: bool) {
        if let Some(r) = &self.ripples
            && self.config.lock().unwrap().settings.ripple
        {
            let strength = if arrived {
                ripple::ARRIVING
            } else {
                ripple::LEAVING
            };
            r.show(at, strength);
        }
    }

    /// Someone pushed the cursor towards a paired computer that's offline: send it a
    /// Wake-on-LAN packet (at most every 10 s; the edge gets pushed many times a second).
    fn wake(&self, id: &str) {
        let Some(peer) = self.config.lock().unwrap().peer(id).cloned() else {
            return;
        };
        {
            let mut last = self.last_wake.lock().unwrap();
            if last
                .get(id)
                .is_some_and(|t: &Instant| t.elapsed() < Duration::from_secs(10))
            {
                return;
            }
            last.insert(id.to_string(), Instant::now());
        }
        // Someone wants it: look for it and try it now, whether or not it can be woken.
        self.search.again();
        if let Some(d) = self.dials.lock().unwrap().get_mut(id) {
            d.failures = 0;
            d.next_attempt = Instant::now();
        }
        if peer.wake_macs.is_empty() {
            debug!("{} is offline and can't be woken remotely", peer.name);
            return;
        }
        info!("waking {}", peer.name);
        tokio::task::spawn_blocking(move || net::wake_on_lan(&peer.wake_macs));
    }

    fn send_motion(&self, peer: &str, motion: Motion) {
        if let Some(p) = self.peers.lock().unwrap().get(peer) {
            let _ = p
                .conn
                .send_datagram(proto::encode(&Datagram::Motion(motion)).into());
        }
    }

    /// Motion travels as unreliable datagrams where the newest wins, so a lost final packet
    /// would leave the remote cursor short of where it should be until the next move. Once
    /// the pointer rests, send the last position again (the receiver ignores repeats).
    async fn settle_loop(self: Arc<Self>) {
        let mut rx = self.settle.subscribe();
        while rx.changed().await.is_ok() {
            loop {
                tokio::select! {
                    changed = rx.changed() => if changed.is_err() { return },
                    _ = tokio::time::sleep(Duration::from_millis(40)) => break,
                }
            }
            let last = rx.borrow_and_update().clone();
            if let Some((peer, motion)) = last {
                for _ in 0..2 {
                    self.send_motion(&peer, motion);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
            }
        }
    }

    /// Watch for display changes (plugging in a monitor, changing resolution).
    async fn display_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tick.tick().await;
            let now = tokio::task::spawn_blocking(platform::displays)
                .await
                .unwrap_or_default();
            if now.is_empty() || *self.displays.lock().unwrap() == now {
                continue;
            }
            info!("displays changed");
            *self.displays.lock().unwrap() = now.clone();
            let actions = self
                .controller
                .lock()
                .unwrap()
                .set_local_displays(now.clone());
            self.apply_actions(actions);
            self.refresh_edges();
            if let Some(t) = self.target.get()
                && let Some(bounds) = Rect::bounding(now.iter().map(|d| d.rect))
            {
                t.lock().unwrap().emulator.set_bounds(bounds);
            }
            let ids: Vec<String> = self.peers.lock().unwrap().keys().cloned().collect();
            for id in ids {
                self.send(&id, Message::Displays(now.clone()));
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Messages

    fn on_motion(&self, id: &str, conn: usize, motion: Motion) {
        if !self.is_current(id, conn) || self.is_paused(id) {
            return;
        }
        let Some(t) = self.target.get() else { return };
        let moved = {
            let mut t = t.lock().unwrap();
            let fresh = t.active.as_deref() == Some(id) && motion.seq > t.last_seq;
            if fresh {
                t.last_seq = motion.seq;
                t.emulator.motion(motion.x, motion.y);
                t.at = Point::new(motion.x, motion.y);
            }
            fresh
        };
        if moved {
            self.controller.lock().unwrap().note_injected();
        }
    }

    /// Is `conn` the connection we're using for `id`? Messages from a replaced connection are
    /// dropped (its certificate may not even be the one we have on record).
    fn is_current(&self, id: &str, conn: usize) -> bool {
        self.peers
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|p| p.conn.stable_id() == conn)
    }

    fn on_message(&self, id: &str, conn: usize, msg: Message) {
        let Some(peer) = self.peer(id) else { return };
        if peer.conn.stable_id() != conn {
            return;
        }
        match msg {
            Message::PairRequest => self.pair_show_code(id, &peer),
            Message::PairSpake(m) => self.pair_spake(id, &peer, m),
            Message::PairConfirm(tag) => self.pair_confirm(id, &peer, tag),
            Message::PairFailed(reason) => {
                debug!("pairing with {} failed: {reason}", peer.hello.name);
                self.pair_finish(id, Err(reason));
            }
            Message::Hello(_) | Message::Displays(_) | Message::Leave | Message::NotPaired
                if !peer.paired => {}
            _ if !peer.paired => {
                // It thinks we're paired (we were, until this side forgot it): tell it.
                debug!("ignoring message from unpaired {}", peer.hello.name);
                let tell = {
                    let mut told = self.told_not_paired.lock().unwrap();
                    let recent = told
                        .get(id)
                        .is_some_and(|t| t.elapsed() < Duration::from_secs(600));
                    if !recent {
                        told.insert(id.to_string(), Instant::now());
                    }
                    !recent
                };
                if tell {
                    self.send(id, Message::NotPaired);
                }
            }
            Message::NotPaired => {
                // Re-pairing already under way will settle it either way.
                if self.pairing.lock().unwrap().contains_key(id) {
                    return;
                }
                info!("{} unpaired from this computer", peer.hello.name);
                self.forget(id);
                platform::notify(
                    "MouseTail",
                    &format!(
                        "{} unpaired from this computer. Pair again to use it.",
                        peer.hello.name
                    ),
                );
            }
            Message::Paused { paused, updated } => self.adopt_paused(id, paused, updated),
            // Paused: nothing crosses. Leave still counts (our cursor comes home).
            Message::Enter { .. }
            | Message::Button { .. }
            | Message::Scroll(_)
            | Message::TrackpadScroll { .. }
            | Message::TrackpadScrollEnd
            | Message::Key { .. }
            | Message::Clipboard { .. }
            | Message::Media(_)
            | Message::AudioWanted(true)
            | Message::SoundWanted { wanted: true, .. }
                if self.is_paused(id) =>
            {
                debug!("ignoring input from {}: paused", peer.hello.name);
            }
            Message::Hello(hello) => {
                if let Some(p) = self.peers.lock().unwrap().get_mut(id) {
                    p.hello = hello;
                }
                self.peer_up(id);
            }
            Message::Displays(displays) => {
                if let Some(p) = self.peers.lock().unwrap().get_mut(id) {
                    p.hello.displays = displays;
                }
                self.peer_up(id);
            }
            Message::Clipboard { mime, data } => self.receive_clipboard(id, &peer, mime, data),
            Message::Placement { x, y, updated } => {
                self.adopt_placement(id, Point::new(x, y), updated)
            }
            Message::Media(key) => {
                debug!("media key {key:?} from {}", peer.hello.name);
                if let Some(t) = self.target.get() {
                    t.lock().unwrap().emulator.media(key);
                }
            }
            Message::SoundCaps {
                can_play,
                can_share,
            } => {
                let peer = {
                    let mut peers = self.peers.lock().unwrap();
                    let Some(p) = peers.get_mut(id) else { return };
                    p.sound = Some((can_play, can_share));
                    p.clone()
                };
                self.request_audio(id, &peer);
            }
            Message::AudioWanted(wanted) => self.on_sound_wanted(id, peer, wanted, 0),
            Message::SoundWanted { wanted, updated } => {
                self.on_sound_wanted(id, peer, wanted, updated)
            }
            Message::Leave => {
                // The cursor is leaving us: our clipboard goes with it.
                let was_at = self.target.get().and_then(|t| {
                    let t = t.lock().unwrap();
                    (t.active.as_deref() == Some(id)).then_some(t.at)
                });
                let was_active = was_at.is_some();
                self.on_input(id, &peer, Message::Leave);
                // Only if it left over the edge between us, not sent home some other way.
                let edge =
                    was_at.and_then(|at| self.controller.lock().unwrap().edge_towards(id, at));
                if let Some(edge) = edge {
                    self.ripple(edge, false);
                    // Only one cursor on show: theirs is wherever it went. Ours goes to where
                    // it crossed (the last move before crossing can stop well short of the
                    // edge), which is on the edge strip, where capture can hide it.
                    let inside = self
                        .displays
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|d| d.rect.clamp(edge))
                        .min_by(|a, b| {
                            let far = |p: &Point| (p.x - edge.x).hypot(p.y - edge.y);
                            far(a).total_cmp(&far(b))
                        });
                    if let (Some(inside), Some(t)) = (inside, self.target.get()) {
                        t.lock().unwrap().emulator.motion(inside.x, inside.y);
                    }
                    if let Some(c) = self.capture.get() {
                        c.hide_cursor();
                    }
                }
                if was_active {
                    self.controller.lock().unwrap().set_controlled_by(None);
                    self.push_clipboard(id);
                }
                // From the computer our cursor is on: someone else has taken it over (or it
                // crossed into us at the same moment we crossed into it). Come home.
                let actions = self.controller.lock().unwrap().sent_home_by(id);
                self.apply_actions(actions);
            }
            Message::Enter { x, y } if self.target.get().is_some() => {
                debug!("cursor ← {id}");
                // Being controlled: our own cursor comes home if it's off on another computer
                // (perhaps this one: its own mouse took the cursor back, or we both crossed at
                // once), and our capture stands down until they leave.
                let actions = self.controller.lock().unwrap().set_controlled_by(Some(id));
                self.apply_actions(actions);
                // Whoever was controlling us is replaced: tell them, so they come home.
                let replaced = self.target.get().and_then(|t| {
                    let active = t.lock().unwrap().active.clone();
                    active.filter(|a| a != id)
                });
                self.on_input(id, &peer, msg);
                let at = Point::new(x, y);
                let edge = {
                    let mut c = self.controller.lock().unwrap();
                    c.note_injected();
                    c.edge_towards(id, at)
                };
                self.ripple(edge.unwrap_or(at), true);
                if let Some(replaced) = replaced {
                    self.send(&replaced, Message::Leave);
                }
            }
            input => {
                let moves = matches!(input, Message::Button { .. });
                self.on_input(id, &peer, input);
                if moves {
                    self.controller.lock().unwrap().note_injected();
                }
            }
        }
    }

    fn on_input(&self, id: &str, peer: &Peer, msg: Message) {
        let Some(t) = self.target.get() else { return };
        let mut t = t.lock().unwrap();
        match msg {
            Message::Enter { x, y } => {
                if t.active.as_deref() != Some(id) {
                    t.leave();
                }
                t.active = Some(id.to_string());
                self.entered.notify_waiters();
                t.remap_active = peer.hello.platform == Platform::MacOs
                    && Platform::current() != Platform::MacOs;
                if t.seq_from.as_deref() != Some(id) {
                    t.seq_from = Some(id.to_string());
                    t.last_seq = 0;
                }
                platform::on_enter();
                t.emulator.motion(x, y);
                t.at = Point::new(x, y);
            }
            _ if t.active.as_deref() != Some(id) => {}
            Message::Leave => t.leave(),
            Message::Button { code, down, x, y } => {
                t.emulator.motion(x, y);
                t.at = Point::new(x, y);
                for (code, down) in t.map(code, down) {
                    t.emulator.button_or_key(code, down);
                }
            }
            Message::Key { code, down } => {
                tracing::trace!("key {code} {}", if down { "down" } else { "up" });
                for (code, down) in t.map(code, down) {
                    t.emulator.button_or_key(code, down);
                }
            }
            Message::Scroll(s) => t.emulator.scroll(s),
            Message::TrackpadScroll { dx, dy } => t.emulator.trackpad_scroll(dx, dy),
            Message::TrackpadScrollEnd => t.emulator.trackpad_scroll_end(),
            _ => {}
        }
    }

    // -----------------------------------------------------------------------------------------
    // Sound goes to the computer you're sitting at: whichever was last used to push the
    // cursor onto the other. That one asks; the other streams into it.

    /// Do we play `id`'s sound (rather than it playing ours)? Returns when that was decided.
    fn listens_to(&self, id: &str, peer: &Peer) -> (bool, u64) {
        self.listens_in(&self.config.lock().unwrap(), id, peer)
    }

    fn listens_in(&self, config: &Config, id: &str, peer: &Peer) -> (bool, u64) {
        let (they_play, they_share) = peer.sound_caps();
        let can_hear = self.player.is_some() && they_share;
        let can_send = platform::AudioSource::supported() && they_play;
        let (stored, updated) = config
            .peer(id)
            .map(|p| (p.listen, p.listen_updated))
            .unwrap_or((None, 0));
        let listen = match (can_hear, can_send) {
            (false, _) => false,
            (true, false) => true,
            (true, true) => stored.unwrap_or_else(|| {
                audio::listens_by_default(
                    (&self.id, Platform::current()),
                    (id, peer.hello.platform),
                )
            }),
        };
        (listen, updated)
    }

    /// Which way sound goes between us and a connected peer: "here" or "there" (or neither).
    fn sound_way(&self, config: &Config, id: &str, peer: &Peer) -> Option<&'static str> {
        if !peer.paired || !config.settings.audio || config.peer(id).is_some_and(|p| p.paused) {
            return None;
        }
        if self.listens_in(config, id, peer).0 {
            Some("here")
        } else if platform::AudioSource::supported() && peer.sound_caps().0 {
            Some("there")
        } else {
            None
        }
    }

    fn set_listen(&self, id: &str, listen: bool, updated: u64) {
        let unchanged = self
            .config
            .lock()
            .unwrap()
            .peer(id)
            .is_some_and(|p| p.listen == Some(listen) && p.listen_updated == updated);
        if !unchanged {
            self.update_config(|c| {
                if let Some(p) = c.peer_mut(id) {
                    p.listen = Some(listen);
                    p.listen_updated = updated;
                }
            });
        }
    }

    /// Ask a paired machine for its sound if we're the one that listens (else tell it not to
    /// send any).
    fn request_audio(&self, id: &str, peer: &Peer) {
        if !peer.paired {
            return;
        }
        let (listen, updated) = self.listens_to(id, peer);
        let wanted = listen && self.config.lock().unwrap().settings.audio && !self.is_paused(id);
        let msg = match peer.sound {
            Some(_) => Message::SoundWanted { wanted, updated },
            None => Message::AudioWanted(wanted),
        };
        self.send(id, msg);
    }

    /// The cursor just went from here onto `id`: the user is here, so its sound should be too.
    fn claim_sound(&self, id: &str) {
        let Some(peer) = self.peer(id) else { return };
        if !peer.paired || self.player.is_none() || !peer.sound_caps().1 || self.is_paused(id) {
            return;
        }
        if self.listens_to(id, &peer).0 {
            return;
        }
        info!("sound from {} now plays here", peer.hello.name);
        self.set_listen(id, true, now_ms());
        self.stop_audio(Some(id));
        self.request_audio(id, &peer);
    }

    /// Someone is using this computer's own keyboard or mouse: they're sitting here, so the
    /// other computers' sound should come here too (even if they last crossed from elsewhere).
    fn check_sitting_here(&self) {
        let Some(idle) = platform::idle_time() else {
            return;
        };
        let controlled = self
            .target
            .get()
            .is_some_and(|t| t.lock().unwrap().active.is_some());
        let mut last = self.last_controlled.lock().unwrap();
        if controlled {
            *last = Some(Instant::now());
            return;
        }
        // Injected input from a visit that just ended still counts as recent input.
        if idle > Duration::from_secs(1) || last.is_some_and(|t| t.elapsed() < SETTLE_AFTER_VISIT) {
            return;
        }
        drop(last);
        let ids: Vec<String> = self.peers.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.claim_sound(&id);
        }
    }

    fn on_sound_wanted(&self, id: &str, peer: Peer, wanted: bool, updated: u64) {
        // Setting up the sound source can take a moment; keep this connection's input flowing
        // meanwhile.
        if let Some(node) = self.me.get().and_then(std::sync::Weak::upgrade) {
            let id = id.to_string();
            tokio::task::spawn_blocking(move || node.audio_requested(&id, &peer, wanted, updated));
        }
    }

    /// `id` asked for our sound (or to stop).
    fn audio_requested(&self, id: &str, peer: &Peer, wanted: bool, updated: u64) {
        if wanted {
            let (listen, ours) = self.listens_to(id, peer);
            if listen && self.config.lock().unwrap().settings.audio {
                // Both think the user is with them: the newer claim wins.
                let theirs_newer = updated > ours || (updated == ours && id < self.id.as_str());
                if !theirs_newer {
                    self.request_audio(id, peer);
                    return;
                }
            }
            self.set_listen(id, false, updated);
            self.stop_listening(Some(id));
        }
        self.audio_wanted(id, peer, wanted);
    }

    fn on_audio(&self, id: &str, conn: usize, packet: AudioPacket) {
        let Some(player) = &self.player else { return };
        let paired = self
            .peers
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|p| p.paired && p.conn.stable_id() == conn);
        if paired && self.config.lock().unwrap().settings.audio && !self.is_paused(id) {
            player.play(packet);
            self.heard_sound(id);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Media controls: while another computer's sound plays here, AirPods presses and media
    // keys go to it.

    fn heard_sound(&self, id: &str) {
        let Some(now_playing) = &self.now_playing else {
            return;
        };
        let now = Instant::now();
        let mut l = self.listening.lock().unwrap();
        if l.paused_until.is_some_and(|t| now < t) {
            return;
        }
        l.last_sound = Some(now);
        if l.playing && l.source.as_deref() == Some(id) {
            return;
        }
        l.source = Some(id.to_string());
        l.playing = true;
        drop(l);
        now_playing.show(&self.peer_name(id), true);
    }

    /// Notice when the sound stops, and pass on media controls.
    async fn media_loop(self: Arc<Self>, mut commands: mpsc::UnboundedReceiver<MediaCommand>) {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(c) => self.on_media_command(c),
                    None => return,
                },
                _ = tick.tick() => {
                    self.check_sound_stopped();
                    self.check_sitting_here();
                }
            }
        }
    }

    fn check_sound_stopped(&self) {
        let Some(now_playing) = &self.now_playing else {
            return;
        };
        let mut l = self.listening.lock().unwrap();
        if !l.playing || l.last_sound.is_some_and(|t| t.elapsed() < SOUND_STOPPED) {
            return;
        }
        l.playing = false;
        let Some(source) = l.source.clone() else {
            return;
        };
        drop(l);
        // Stay Now Playing, paused, so the next press resumes it.
        now_playing.show(&self.peer_name(&source), false);
    }

    fn on_media_command(&self, command: MediaCommand) {
        let Some(now_playing) = &self.now_playing else {
            return;
        };
        let mut l = self.listening.lock().unwrap();
        let Some(source) = l.source.clone() else {
            return;
        };
        debug!("media control {command:?} for {source}");
        let key = match command {
            MediaCommand::Play if l.playing => return,
            MediaCommand::Pause if !l.playing => return,
            MediaCommand::PlayPause | MediaCommand::Play | MediaCommand::Pause => {
                // Show the change straight away rather than when the sound stops or starts.
                l.playing = !l.playing;
                let now = Instant::now();
                if l.playing {
                    l.last_sound = Some(now);
                    l.paused_until = None;
                } else {
                    l.paused_until = Some(now + PAUSE_TAIL);
                }
                let playing = l.playing;
                drop(l);
                now_playing.show(&self.peer_name(&source), playing);
                MediaKey::PlayPause
            }
            MediaCommand::Next => MediaKey::Next,
            MediaCommand::Previous => MediaKey::Previous,
        };
        if self
            .peer(&source)
            .is_some_and(|p| p.hello.protocol >= proto::SKIPS_UNKNOWN)
        {
            self.send(&source, Message::Media(key));
        }
    }

    /// `id`'s sound (or anyone's) no longer plays here: hand the controls back to the Mac.
    fn stop_listening(&self, id: Option<&str>) {
        let Some(now_playing) = &self.now_playing else {
            return;
        };
        let mut l = self.listening.lock().unwrap();
        if l.source.is_none() || id.is_some_and(|id| l.source.as_deref() != Some(id)) {
            return;
        }
        *l = Listening::default();
        drop(l);
        now_playing.clear();
    }

    fn peer_name(&self, id: &str) -> String {
        self.peer(id)
            .map(|p| p.hello.name)
            .unwrap_or_else(|| id.to_string())
    }

    /// Runs on a blocking thread: setting up the speaker takes a moment.
    fn audio_wanted(&self, id: &str, peer: &Peer, wanted: bool) {
        let _busy = self.audio_busy.lock().unwrap();
        if !wanted || !self.config.lock().unwrap().settings.audio {
            drop(self.take_audio(Some(id)));
            return;
        }
        let conn = peer.conn.clone();
        {
            let current = self.audio_out.lock().unwrap();
            if current
                .as_ref()
                .is_some_and(|(p, c, _)| p == id && *c == conn.stable_id())
            {
                return; // already streaming to them
            }
        }
        drop(self.take_audio(None));
        let (speaker, pcm) = match platform::AudioSource::start(id, &peer.hello.name) {
            Ok(s) => s,
            Err(e) => {
                debug!("not sharing sound: {e:#}");
                return;
            }
        };
        // They may have gone (or sound been turned off) while it was being set up: then put
        // the old output straight back (by dropping `speaker`) rather than leave sound going
        // nowhere.
        if !self.is_current(id, conn.stable_id()) || !self.config.lock().unwrap().settings.audio {
            return;
        }
        info!("sound now plays on {}", peer.hello.name);
        *self.audio_out.lock().unwrap() = Some((id.to_string(), conn.stable_id(), speaker));
        std::thread::Builder::new()
            .name("audio-encode".into())
            .spawn(move || stream_audio(pcm, conn))
            .ok();
    }

    /// Stop sending sound to `id` (or to anyone). Removes the virtual speaker.
    fn stop_audio(&self, id: Option<&str>) {
        if let Some(speaker) = self.take_audio(id) {
            // Dropping the speaker restores the previous output; that shells out, so do it
            // off the async runtime, in turn with any speaker being set up.
            let busy = self.audio_busy.clone();
            std::thread::spawn(move || {
                let _busy = busy.lock().unwrap();
                drop(speaker);
            });
        }
    }

    /// Take the speaker we're streaming from, if it's for `id` (or any, with `None`).
    fn take_audio(&self, id: Option<&str>) -> Option<(String, usize, platform::AudioSource)> {
        let mut out = self.audio_out.lock().unwrap();
        if out
            .as_ref()
            .is_some_and(|(p, _, _)| id.is_none_or(|id| id == p))
        {
            out.take()
        } else {
            None
        }
    }

    /// Typing silently doing nothing is baffling, so say why (at most once a minute).
    fn warn_if_keyboard_blocked(&self) {
        if !platform::keyboard_blocked() {
            return;
        }
        let mut last = self.keyboard_warned.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return;
        }
        *last = Some(Instant::now());
        warn!("keyboard blocked by Secure Input on this machine");
        platform::notify(
            "Keyboard paused",
            "A password field or locked screen on this Mac is blocking typing. The mouse still works.",
        );
    }

    // -----------------------------------------------------------------------------------------
    // Clipboard: carried across on each crossing, text for now.

    fn push_clipboard(&self, to: &str) {
        let (enabled, max) = {
            let c = self.config.lock().unwrap();
            (c.settings.clipboard, c.settings.clipboard_limit())
        };
        if !enabled || self.is_paused(to) {
            return;
        }
        let Some(peer) = self.peer(to) else { return };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let state = self.clipboard.clone();
        let to = to.to_string();
        rt.spawn(async move {
            // Reading the clipboard can take a few milliseconds; keep it off the input path.
            let Ok(Some((mime, data))) = tokio::task::spawn_blocking(platform::clipboard_get).await
            else {
                return;
            };
            if data.len() > max {
                debug!("clipboard too large to send ({} bytes)", data.len());
                return;
            }
            let hash = clipboard_hash(&data);
            if state.lock().unwrap().known.get(&to) == Some(&hash) {
                return;
            }
            debug!("sending clipboard ({} bytes)", data.len());
            let msg = Message::Clipboard { mime, data };
            if peer.hello.protocol < proto::CLIPBOARD_STREAMS {
                // Older release: inline, after the Enter already queued. No word back, so
                // assume it arrived.
                state.lock().unwrap().known.insert(to, hash);
                let _ = peer.tx.send(msg);
                return;
            }
            match tokio::time::timeout(CLIPBOARD_TIMEOUT, net::send_clipboard(&peer.conn, &msg))
                .await
            {
                Ok(Ok(true)) => {
                    state.lock().unwrap().known.insert(to, hash);
                }
                // Turned away or lost: it goes again on the next crossing.
                Ok(Ok(false)) => debug!("{} didn't take the clipboard", peer.hello.name),
                Ok(Err(e)) => debug!("clipboard to {} failed: {e:#}", peer.hello.name),
                Err(_) => debug!("clipboard to {} timed out", peer.hello.name),
            }
        });
    }

    /// A clipboard on a stream of its own. True if we took it.
    async fn clipboard_stream(&self, id: &str, conn: usize, mut recv: RecvStream) -> bool {
        if !self.is_current(id, conn) || !self.peer(id).is_some_and(|p| p.paired) {
            let _ = recv.stop(0u32.into());
            return false;
        }
        let stream = recv.id().index();
        let frame =
            match tokio::time::timeout(CLIPBOARD_TIMEOUT, net::read_frame(&mut recv, MAX_FRAME))
                .await
            {
                Ok(Ok(frame)) => frame,
                _ => return false,
            };
        let Ok(Message::Clipboard { mime, data }) = proto::decode(&frame) else {
            return false;
        };
        // It may have overtaken the Enter it goes with.
        if !self.wait_until_welcome(id).await || !self.is_current(id, conn) {
            debug!("ignoring clipboard from {id}: not part of a crossing");
            return false;
        }
        {
            let mut state = self.clipboard.lock().unwrap();
            if state.newest.get(id).is_some_and(|&n| n > stream) {
                return false; // a newer one already landed
            }
            state.newest.insert(id.to_string(), stream);
        }
        self.take_clipboard(id, mime, data)
    }

    /// A clipboard on the main stream (from a release before 5).
    fn receive_clipboard(&self, id: &str, peer: &Peer, mime: String, data: Vec<u8>) {
        if !self.clipboard_welcome(id) {
            debug!("ignoring clipboard from {}", peer.hello.name);
            return;
        }
        self.take_clipboard(id, mime, data);
    }

    /// Only as part of a crossing: from the machine controlling us, or the one the cursor just
    /// left. A paired machine can't rewrite the clipboard whenever it likes.
    fn clipboard_welcome(&self, id: &str) -> bool {
        let controlling_us = self
            .target
            .get()
            .is_some_and(|t| t.lock().unwrap().active.as_deref() == Some(id));
        let just_left = self
            .left_peer
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(p, t)| p == id && t.elapsed() < Duration::from_secs(5));
        controlling_us || just_left
    }

    async fn wait_until_welcome(&self, id: &str) -> bool {
        let deadline = tokio::time::Instant::now() + CLIPBOARD_WAIT;
        loop {
            let entered = self.entered.notified();
            tokio::pin!(entered);
            entered.as_mut().enable();
            if self.clipboard_welcome(id) {
                return true;
            }
            if tokio::time::timeout_at(deadline, entered).await.is_err() {
                return false;
            }
        }
    }

    /// Put another computer's clipboard on ours, if clipboard sharing is on and it's not too big.
    fn take_clipboard(&self, id: &str, mime: String, data: Vec<u8>) -> bool {
        let (enabled, max) = {
            let c = self.config.lock().unwrap();
            (c.settings.clipboard, c.settings.clipboard_limit())
        };
        if !enabled || data.len() > max || self.is_paused(id) {
            return false;
        }
        debug!("clipboard from {id} ({} bytes)", data.len());
        // It has this now, so it needn't come back.
        self.clipboard
            .lock()
            .unwrap()
            .known
            .insert(id.to_string(), clipboard_hash(&data));
        tokio::task::spawn_blocking(move || platform::clipboard_set(&mime, &data));
        true
    }

    // -----------------------------------------------------------------------------------------
    // Pairing (see mousetail_core::pairing)

    /// The other side asked to pair: show a code here.
    fn pair_show_code(&self, id: &str, peer: &Peer) {
        // Limits apply to this machine as a whole, not per requester: a new certificate is
        // free, so per-peer limits would do nothing against someone guessing codes.
        let allowed = self.pair_guard.lock().unwrap().allow();
        if let Err(reason) = allowed {
            warn!(
                "ignoring pairing request from {}: {reason}",
                peer.hello.name
            );
            self.send(id, Message::PairFailed(reason));
            return;
        }
        let code = pairing::new_code();
        info!("pairing requested by {}: code {code}", peer.hello.name);
        platform::notify(
            "MouseTail pairing",
            &format!("Enter {code} on {} to connect it", peer.hello.name),
        );
        let mut pairing = self.pairing.lock().unwrap();
        // Only one code is live at a time. Otherwise each new certificate would hold its own
        // code open, and someone could collect a few dozen and guess them all at once.
        pairing.retain(|_, p| !matches!(p, Pairing::Shown { .. }));
        pairing.insert(
            id.to_string(),
            Pairing::Shown {
                code,
                expires: Instant::now() + CODE_LIFETIME,
                key: None,
            },
        );
    }

    /// We typed the code shown on the other machine.
    pub async fn pair_with_code(&self, id: &str, code: &str) -> Result<(), String> {
        let (state, msg) = pairing::start(code);
        let (done_tx, done_rx) = oneshot::channel();
        self.pairing.lock().unwrap().insert(
            id.to_string(),
            Pairing::Typed {
                state: Some(state),
                key: None,
                done: Some(done_tx),
            },
        );
        self.send(id, Message::PairSpake(msg));
        match tokio::time::timeout(Duration::from_secs(15), done_rx).await {
            Ok(Ok(result)) => result,
            _ => {
                self.pairing.lock().unwrap().remove(id);
                Err("no answer from the other machine".into())
            }
        }
    }

    fn pair_spake(&self, id: &str, peer: &Peer, msg: Vec<u8>) {
        let mut pairing = self.pairing.lock().unwrap();
        match pairing.get_mut(id) {
            Some(Pairing::Shown {
                code, expires, key, ..
            }) => {
                if Instant::now() > *expires || key.is_some() {
                    // Expired, or already used for an attempt.
                    pairing.remove(id);
                    drop(pairing);
                    self.send(
                        id,
                        Message::PairFailed("that code has expired; ask for a new one".into()),
                    );
                    return;
                }
                // Count the attempt now: the tag we send back lets the other side check its
                // guess offline, so it may never report a failure. Success refunds it.
                {
                    let mut guard = self.pair_guard.lock().unwrap();
                    if guard.locked() {
                        drop(guard);
                        pairing.remove(id);
                        drop(pairing);
                        self.send(id, Message::PairFailed(PAIR_LOCKED.into()));
                        return;
                    }
                    guard.failed();
                }
                let (state, mine) = pairing::start(code);
                match pairing::finish(state, &msg) {
                    Ok(k) => {
                        let tag = pairing::tag(
                            &k,
                            Role::Responder,
                            &peer.fingerprint,
                            &self.identity.fingerprint,
                        );
                        *key = Some(k);
                        drop(pairing);
                        self.send(id, Message::PairSpake(mine));
                        self.send(id, Message::PairConfirm(tag));
                    }
                    Err(_) => {
                        pairing.remove(id);
                        drop(pairing);
                        self.send(id, Message::PairFailed("pairing exchange failed".into()));
                    }
                }
            }
            Some(Pairing::Typed { state, key, .. }) => {
                if let Some(s) = state.take() {
                    *key = pairing::finish(s, &msg).ok();
                }
            }
            None => {
                drop(pairing);
                self.send(
                    id,
                    Message::PairFailed("that code has expired; ask for a new one".into()),
                );
            }
        }
    }

    fn pair_confirm(&self, id: &str, peer: &Peer, tag: Vec<u8>) {
        // Decide under the lock, act after dropping it (sending takes the peers lock).
        let (ok, shown, reply) = {
            let pairing = self.pairing.lock().unwrap();
            match pairing.get(id) {
                // We showed the code: the typer confirms it knew it.
                Some(Pairing::Shown { key: Some(k), .. }) => {
                    let ok = pairing::verify(
                        k,
                        Role::Initiator,
                        &peer.fingerprint,
                        &self.identity.fingerprint,
                        &tag,
                    );
                    (ok, true, None)
                }
                // We typed the code: check the shower's tag, then send ours.
                Some(Pairing::Typed { key: Some(k), .. }) => {
                    let ok = pairing::verify(
                        k,
                        Role::Responder,
                        &self.identity.fingerprint,
                        &peer.fingerprint,
                        &tag,
                    );
                    let reply = ok.then(|| {
                        pairing::tag(
                            k,
                            Role::Initiator,
                            &self.identity.fingerprint,
                            &peer.fingerprint,
                        )
                    });
                    (ok, false, reply)
                }
                _ => return,
            }
        };
        if let Some(mine) = reply {
            self.send(id, Message::PairConfirm(mine));
        }
        if ok {
            self.pin(id, peer);
            if shown {
                self.pair_guard.lock().unwrap().succeeded();
                platform::notify("MouseTail", &format!("Paired with {}", peer.hello.name));
            }
            self.pair_finish(id, Ok(()));
        } else {
            if shown {
                warn!("{} entered the wrong pairing code", peer.hello.name);
            }
            // Either way, tell the other side so it retires its code now.
            self.send(id, Message::PairFailed("wrong code".into()));
            self.pair_finish(id, Err("wrong code".into()));
        }
    }

    fn pair_finish(&self, id: &str, result: Result<(), String>) {
        if let Some(Pairing::Typed {
            done: Some(done), ..
        }) = self.pairing.lock().unwrap().remove(id)
        {
            let _ = done.send(result);
        }
    }

    fn pin(&self, id: &str, peer: &Peer) {
        info!("paired with {} ({id})", peer.hello.name);
        self.update_config(|c| {
            // Pairing again (same computer) keeps where it sits on the desk.
            let (placement, placement_updated, listen, listen_updated) = c
                .peer(id)
                .map(|p| (p.placement, p.placement_updated, p.listen, p.listen_updated))
                .unwrap_or((None, 0, None, 0));
            c.add_peer(PeerConfig {
                id: id.to_string(),
                name: peer.hello.name.clone(),
                fingerprint: peer.fingerprint.clone(),
                placement,
                placement_updated,
                displays: peer.hello.displays.clone(),
                wake_macs: peer.hello.wake_macs.clone(),
                listen,
                listen_updated,
                paused: false,
                paused_updated: 0,
                addrs: vec![peer.conn.remote_address()],
            })
        });
        if let Some(p) = self.peers.lock().unwrap().get_mut(id) {
            p.paired = true;
        }
        // Now it can have what we keep from unpaired computers (how to wake us).
        self.send(id, Message::Hello(self.hello(id, true)));
        self.peer_up(id);
    }

    fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut config = self.config.lock().unwrap();
        f(&mut config);
        if let Err(e) = config.save(&self.config_path) {
            warn!("couldn't save config: {e:#}");
        }
    }

    // -----------------------------------------------------------------------------------------
    // Local control (IPC)

    pub fn status(&self) -> Value {
        let config = self.config.lock().unwrap();
        let peers = self.peers.lock().unwrap();
        let discovered = self.discovered.lock().unwrap();
        let controller = self.controller.lock().unwrap();
        let controlled_by = self
            .target
            .get()
            .and_then(|t| t.lock().unwrap().active.clone());

        let mut ids: Vec<String> = config.peers.iter().map(|p| p.id.clone()).collect();
        ids.extend(peers.keys().cloned());
        ids.extend(discovered.keys().cloned());
        ids.sort();
        ids.dedup();
        let list: Vec<Value> = ids
            .iter()
            .map(|id| {
                let conn = peers.get(id);
                let name = conn
                    .map(|p| p.hello.name.clone())
                    .or_else(|| config.peer(id).map(|p| p.name.clone()))
                    .or_else(|| discovered.get(id).map(|f| f.name.clone()))
                    .unwrap_or_else(|| id.clone());
                json!({
                    "id": id,
                    "name": name,
                    "paired": config.peer(id).is_some(),
                    "connected": conn.is_some(),
                    "address": conn.map(|p| p.conn.remote_address().to_string()),
                    "rtt_ms": conn.map(|p| p.conn.rtt().as_secs_f64() * 1000.0),
                    "platform": conn.map(|p| p.hello.platform),
                    "placement": config.peer(id).and_then(|p| p.placement),
                    "version": discovered.get(id).and_then(|f| f.version.clone()),
                    "sound": conn.and_then(|p| self.sound_way(&config, id, p)),
                    "paused": config.peer(id).is_some_and(|p| p.paused),
                })
            })
            .collect();
        // A code we're showing right now, so every UI can display it, not just the
        // notification.
        let pairing_code = self
            .pairing
            .lock()
            .unwrap()
            .iter()
            .find_map(|(id, p)| match p {
                Pairing::Shown { code, expires, .. } if *expires > Instant::now() => Some(json!({
                    "peer": id,
                    "name": peers.get(id).map(|p| p.hello.name.clone()),
                    "code": code,
                })),
                _ => None,
            });
        json!({
            "id": self.id,
            "name": self.name,
            "port": self.port,
            "pairing_code": pairing_code,
            "can_control": self.capture.get().is_some(),
            "keyboard_blocked": platform::keyboard_blocked(),
            "capture_error": *self.capture_error.lock().unwrap(),
            "firewall": *self.firewall.lock().unwrap(),
            "can_be_controlled": self.target.get().is_some(),
            "displays": *self.displays.lock().unwrap(),
            "controlling": controller.active_peer(),
            "controlled_by": controlled_by,
            "settings": config.settings,
            "peers": list,
            "version": update::VERSION,
            "update": self.updater.status(),
        })
    }

    /// Resolve a user's peer reference (name, id prefix) among connected peers.
    pub fn resolve_peer(&self, query: Option<&str>, want_unpaired: bool) -> Result<String, String> {
        let peers = self.peers.lock().unwrap();
        let matches: Vec<&String> = peers
            .iter()
            .filter(|(id, p)| match query {
                Some(q) => id.starts_with(q) || p.hello.name.eq_ignore_ascii_case(q),
                None => !want_unpaired || !p.paired,
            })
            .map(|(id, _)| id)
            .collect();
        match matches.as_slice() {
            [id] => Ok((*id).clone()),
            [] => Err(match query {
                Some(q) => format!("no connected machine matches {q:?}"),
                None => "no other MouseTail machine found on the network yet".into(),
            }),
            _ => Err("more than one machine matches; name one".into()),
        }
    }

    pub fn request_pairing(&self, id: &str) {
        self.send(id, Message::PairRequest);
    }

    pub fn unpair(&self, query: &str) -> Result<String, String> {
        let peer = self
            .config
            .lock()
            .unwrap()
            .find_peer(query)
            .cloned()
            .ok_or_else(|| format!("not paired with {query:?}"))?;
        // So it forgets us too, rather than carrying on as if we were still paired.
        self.send(&peer.id, Message::NotPaired);
        self.forget(&peer.id);
        Ok(peer.name)
    }

    /// Stop being paired with `id`: out of the config and the layout, and anything under way
    /// with it (the cursor on it, it controlling us, sound) stopped.
    fn forget(&self, id: &str) {
        self.update_config(|c| c.peers.retain(|p| p.id != id));
        if let Some(p) = self.peers.lock().unwrap().get_mut(id) {
            p.paired = false;
        }
        self.peer_down(id);
        let actions = self.controller.lock().unwrap().remove_peer(id);
        self.apply_actions(actions);
        self.refresh_edges();
    }

    /// Pause (or resume) a paired machine, here and on it.
    pub fn set_paused(&self, query: &str, paused: bool) -> Result<String, String> {
        let peer = self
            .config
            .lock()
            .unwrap()
            .find_peer(query)
            .cloned()
            .ok_or_else(|| format!("not paired with {query:?}"))?;
        if peer.paused != paused {
            self.apply_paused(&peer.id, paused, now_ms());
            self.share_paused(&peer.id);
        }
        Ok(peer.name)
    }

    fn is_paused(&self, id: &str) -> bool {
        self.config
            .lock()
            .unwrap()
            .peer(id)
            .is_some_and(|p| p.paused)
    }

    /// Tell a peer whether the link is paused, if it's ever been paused and it understands.
    fn share_paused(&self, id: &str) {
        let Some((paused, updated)) = self
            .config
            .lock()
            .unwrap()
            .peer(id)
            .map(|p| (p.paused, p.paused_updated))
        else {
            return;
        };
        let understands = self
            .peer(id)
            .is_some_and(|p| p.hello.protocol >= proto::SKIPS_UNKNOWN);
        if updated > 0 && understands {
            self.send(id, Message::Paused { paused, updated });
        }
    }

    /// A peer told us the link is paused or resumed. The newer choice wins (ties to the
    /// smaller id, so both agree); if ours is newer, tell it.
    fn adopt_paused(&self, id: &str, paused: bool, updated: u64) {
        let Some((own, own_updated)) = self
            .config
            .lock()
            .unwrap()
            .peer(id)
            .map(|p| (p.paused, p.paused_updated))
        else {
            return;
        };
        if own == paused {
            return;
        }
        let newer = updated > own_updated || (updated == own_updated && id < self.id.as_str());
        if !newer {
            self.share_paused(id);
            return;
        }
        info!(
            "{} {} the link from there",
            self.peer_name(id),
            if paused { "paused" } else { "resumed" }
        );
        self.apply_paused(id, paused, updated);
    }

    fn apply_paused(&self, id: &str, paused: bool, updated: u64) {
        self.update_config(|c| {
            if let Some(p) = c.peer_mut(id) {
                p.paused = paused;
                p.paused_updated = updated;
            }
        });
        // Whether we take input from it has changed.
        self.send(id, Message::Hello(self.hello(id, true)));
        if paused {
            // Out of the layout, so the cursor can't cross to it (and comes home, telling it,
            // if it's there); then stop anything else under way with it (it controlling us,
            // sound).
            let actions = self.controller.lock().unwrap().remove_peer(id);
            self.apply_actions(actions);
            self.refresh_edges();
            let controlling_us = self
                .target
                .get()
                .is_some_and(|t| t.lock().unwrap().active.as_deref() == Some(id));
            self.peer_down(id);
            if controlling_us {
                // Its cursor comes home.
                self.send(id, Message::Leave);
            }
            if let Some(peer) = self.peer(id) {
                self.request_audio(id, &peer);
            }
        } else if self.peer(id).is_some() {
            self.peer_up(id);
        } else {
            // Offline: back at its remembered spot, so pushing towards it can wake it.
            self.add_offline_peers(Some(id));
            self.refresh_edges();
        }
    }

    /// Put a peer beside one of this machine's displays.
    pub fn place(&self, query: &str, side: Side, display: Option<usize>) -> Result<Point, String> {
        let id = self.resolve_peer(Some(query), false)?;
        let offset = {
            let controller = self.controller.lock().unwrap();
            controller
                .offset_beside(&id, side, display)
                .ok_or("that machine isn't in the layout (is it paired and connected?)")?
        };
        self.update_config(|c| {
            if let Some(p) = c.peer_mut(&id) {
                p.placement = Some(offset);
                p.placement_updated = now_ms();
            }
        });
        self.peer_up(&id);
        Ok(offset)
    }

    /// Every machine's displays and where they sit, for the arrangement view.
    pub fn layout(&self) -> Value {
        let config = self.config.lock().unwrap();
        let peers = self.peers.lock().unwrap();
        let controller = self.controller.lock().unwrap();
        let mut machines = vec![json!({
            "id": self.id,
            "name": self.name,
            "this": true,
            "connected": true,
            "displays": *self.displays.lock().unwrap(),
            "offset": Point::default(),
        })];
        for p in &config.peers {
            let live = peers.get(&p.id);
            let placed = controller
                .layout()
                .machines
                .iter()
                .find(|m| m.id == p.id)
                .map(|m| m.offset);
            machines.push(json!({
                "id": p.id,
                "name": live.map(|l| l.hello.name.clone()).unwrap_or_else(|| p.name.clone()),
                "this": false,
                "connected": live.is_some(),
                "paused": p.paused,
                "displays": live.map(|l| l.hello.displays.clone()).unwrap_or_else(|| p.displays.clone()),
                "offset": placed.or(p.placement),
            }));
        }
        // Include offline machines at their remembered spots, so the view still shows where
        // they'll connect.
        let mut all = controller.layout().clone();
        for p in &config.peers {
            if all.machine_index(&p.id).is_none()
                && !p.paused
                && let Some(offset) = p.placement
            {
                all.machines.push(mousetail_core::layout::Machine {
                    id: p.id.clone(),
                    displays: p.displays.clone(),
                    offset,
                });
            }
        }
        let crossings = all.crossing_edges();
        json!({
            "machines": machines,
            // Exactly where the cursor can pass between computers, straight from the logic
            // that moves it, so the arrangement view can't disagree with reality.
            "crossings": crossings,
        })
    }

    /// Drop a machine at `desired` (layout coordinates); it snaps to the nearest valid spot.
    pub fn place_at(&self, query: &str, desired: Point) -> Result<Point, String> {
        let id = self
            .config
            .lock()
            .unwrap()
            .find_peer(query)
            .map(|p| p.id.clone())
            .ok_or_else(|| format!("not paired with {query:?}"))?;
        // Lock order everywhere: config before controller.
        let remembered = self
            .config
            .lock()
            .unwrap()
            .peer(&id)
            .map(|p| p.displays.clone());
        let offset = {
            let mut controller = self.controller.lock().unwrap();
            if controller.layout().machine_index(&id).is_none() {
                // Offline: lay it out from its remembered displays.
                controller.set_peer(&id, remembered.clone().unwrap_or_default(), desired);
            }
            controller
                .snap(&id, desired)
                .ok_or("there's nowhere valid to put it")?
        };
        self.update_config(|c| {
            if let Some(p) = c.peer_mut(&id) {
                p.placement = Some(offset);
                p.placement_updated = now_ms();
            }
        });
        if self.is_paused(&id) {
            // Moved, but nothing crosses to it until it's resumed.
            let actions = self.controller.lock().unwrap().remove_peer(&id);
            self.apply_actions(actions);
            return Ok(offset);
        }
        let live = self.peer(&id).map(|p| p.hello.displays);
        let actions = self.controller.lock().unwrap().set_peer(
            &id,
            live.or(remembered).unwrap_or_default(),
            offset,
        );
        self.apply_actions(actions);
        self.peer_up(&id);
        Ok(offset)
    }

    pub fn settings(&self) -> Value {
        json!(self.config.lock().unwrap().settings)
    }

    pub fn set_setting(&self, key: &str, value: &Value) -> Result<(), String> {
        let result = self.apply_setting(key, value);
        if result.is_ok() && key == "audio" {
            let peers: Vec<(String, Peer)> = self
                .peers
                .lock()
                .unwrap()
                .iter()
                .map(|(id, p)| (id.clone(), p.clone()))
                .collect();
            for (id, peer) in peers {
                self.request_audio(&id, &peer);
            }
            if value == &Value::Bool(false) {
                self.stop_audio(None);
                self.stop_listening(None);
            }
        }
        result
    }

    fn apply_setting(&self, key: &str, value: &Value) -> Result<(), String> {
        let mut result = Ok(());
        self.update_config(|c| match (key, value) {
            ("clipboard", Value::Bool(b)) => c.settings.clipboard = *b,
            ("audio", Value::Bool(b)) => c.settings.audio = *b,
            ("updates", Value::Bool(b)) => c.settings.updates = *b,
            ("ripple", Value::Bool(b)) => c.settings.ripple = *b,
            _ => result = Err(format!("unknown setting {key:?}")),
        });
        result
    }

    /// Someone is using another computer through this one, or this one from another.
    pub fn in_use(&self) -> bool {
        self.controller.lock().unwrap().active_peer().is_some()
            || self
                .target
                .get()
                .is_some_and(|t| t.lock().unwrap().active.is_some())
    }

    pub fn release(&self) {
        let actions = self.controller.lock().unwrap().release();
        self.apply_actions(actions);
    }

    /// Stop the daemon (cleanly, as for a signal).
    pub fn shut_down(&self) {
        self.stop.notify_one();
    }
}

impl Target {
    fn map(&mut self, code: u16, down: bool) -> Vec<(u16, bool)> {
        if self.remap_active {
            self.remap.map(code, down)
        } else {
            vec![(code, down)]
        }
    }

    fn leave(&mut self) {
        for (code, down) in self.remap.reset() {
            self.emulator.button_or_key(code, down);
        }
        self.emulator.release_all();
        self.active = None;
    }
}

trait ButtonOrKey {
    fn button_or_key(&self, code: u16, down: bool);
}

impl ButtonOrKey for platform::Emulator {
    fn button_or_key(&self, code: u16, down: bool) {
        if mousetail_core::keys::ev::is_button(code) {
            self.button(code, down);
        } else {
            self.key(code, down);
        }
    }
}

/// Encode whatever plays into the virtual speaker and send it, 10 ms per packet. Silence
/// isn't sent (the receiver notices the gap and re-buffers when sound resumes). Ends when the
/// speaker is removed or the connection closes.
fn stream_audio(pcm: std::sync::mpsc::Receiver<Vec<i16>>, conn: Connection) {
    const FRAME_SAMPLES: usize = audio::FRAME * audio::CHANNELS;
    const SILENCE_FRAMES: u32 = 50;
    let Ok(mut encoder) = audio::Encoder::new() else {
        warn!("couldn't start the audio encoder");
        return;
    };
    let stream = rand_u32();
    let mut seq = 0u32;
    let mut pending: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES * 4);
    let mut quiet = 0u32;
    while let Ok(chunk) = pcm.recv() {
        pending.extend(chunk);
        while pending.len() >= FRAME_SAMPLES {
            let frame: Vec<i16> = pending.drain(..FRAME_SAMPLES).collect();
            quiet = if frame.iter().all(|s| s.unsigned_abs() < 8) {
                quiet + 1
            } else {
                0
            };
            if quiet <= SILENCE_FRAMES
                && let Ok(data) = encoder.encode(&frame)
            {
                let packet = Datagram::Audio(AudioPacket { stream, seq, data });
                if conn.send_datagram(proto::encode(&packet).into()).is_err()
                    && conn.close_reason().is_some()
                {
                    return;
                }
            }
            seq = seq.wrapping_add(1);
        }
    }
}

fn rand_u32() -> u32 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(Instant::now()) as u32
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn clipboard_hash(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).into()
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("signal handler");
        let mut hangup = signal(SignalKind::hangup()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = hangup.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// Resolves once the process that started us has exited (we get handed to another parent).
async fn parent_exited() {
    #[cfg(unix)]
    {
        let parent = std::os::unix::process::parent_id();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if std::os::unix::process::parent_id() != parent {
                return;
            }
        }
    }
    #[cfg(not(unix))]
    std::future::pending::<()>().await;
}

/// Only one daemon per user: two would fight over the socket and both capture input. The
/// lock goes with the returned file, including if the process dies.
fn single_instance(paths: &Paths) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(&paths.dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.dir.join("mousetail.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!("MouseTail is already running"),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dials_advertised_and_remembered_computers() {
        let addr = |s: &str| -> SocketAddr { s.parse().unwrap() };
        let advertised = HashMap::from([
            ("mac".to_string(), vec![addr("10.0.0.2:24802")]),
            ("linked".to_string(), vec![addr("10.0.0.3:24802")]),
        ]);
        let remembered = vec![
            (
                "mac".to_string(),
                vec![addr("10.0.0.9:24802"), addr("10.0.0.2:24802")],
            ),
            ("asleep".to_string(), vec![addr("10.0.0.4:24802")]),
            ("never-reached".to_string(), vec![]),
        ];
        let connected = HashSet::from(["linked".to_string()]);
        let wanted = dial_targets(advertised, remembered, &connected, "me");
        assert_eq!(
            wanted["mac"],
            (vec![addr("10.0.0.2:24802"), addr("10.0.0.9:24802")], true)
        );
        assert_eq!(wanted["asleep"], (vec![addr("10.0.0.4:24802")], false));
        assert_eq!(wanted.len(), 2);
    }

    #[test]
    fn only_one_daemon_at_a_time() {
        let dir = std::env::temp_dir().join(format!("mousetail-lock-{}", std::process::id()));
        let paths = Paths {
            config: dir.join("config.toml"),
            socket: dir.join("mousetail.sock"),
            dir,
        };
        let first = single_instance(&paths).unwrap();
        assert!(single_instance(&paths).is_err());
        drop(first);
        assert!(single_instance(&paths).is_ok());
    }

    #[test]
    fn pair_guard_locks_after_max_failures() {
        let mut guard = PairGuard::default();
        for _ in 0..PAIR_MAX_FAILURES {
            assert!(!guard.locked());
            guard.failed();
        }
        assert!(guard.locked());
        assert_eq!(guard.allow(), Err(PAIR_LOCKED.to_string()));
    }

    #[test]
    fn pair_guard_success_refunds_attempt() {
        let mut guard = PairGuard::default();
        for _ in 0..PAIR_MAX_FAILURES {
            guard.failed();
        }
        guard.succeeded();
        assert!(!guard.locked());
    }

    #[test]
    fn pair_guard_spaces_out_codes() {
        let mut guard = PairGuard::default();
        assert!(guard.allow().is_ok());
        assert!(guard.allow().is_err());
    }

    #[test]
    fn pair_guard_forgets_old_failures() {
        let mut guard = PairGuard::default();
        let old = Instant::now() - PAIR_FAILURE_WINDOW - Duration::from_secs(1);
        guard
            .failures
            .extend(std::iter::repeat_n(old, PAIR_MAX_FAILURES));
        assert!(!guard.locked());
        assert!(guard.failures.is_empty());
    }
}
