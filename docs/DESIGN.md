# MouseTail — Design

Share one keyboard and mouse across machines on the same LAN. Move the cursor off the
edge of a screen and it appears on the neighbouring machine; move it back and you're home.
Clipboard follows you across.

Any computer can be the one you're sitting at (the **controller**) and any can be controlled
(a **target**): Mac ↔ Linux in either direction, Mac → Mac and Linux → Linux. Windows is
planned; the platform layer has room for it.

## Goals

- **No fuss.** One install per machine, auto-discovery on the LAN, a one-time 4-digit pairing
  code, then it just reconnects forever.
- **Never trapped.** The cursor can always come home, whatever state the remote is in
  (lock screen, frozen compositor, network drop, sleep).
- **Multi-monitor aware** on both sides, with a drag-to-arrange layout editor like macOS
  *Displays → Arrange*.
- **Clipboard sync** (text first, then images/files).
- **Secure by default.** Everything is encrypted; only paired devices are accepted.
- **Small.** Native binaries, no runtimes, low idle CPU.

## Non-goals (v1)

- More than one remote machine at a time (the model supports it; the UI won't at first).
- File drag-and-drop between machines.
- Internet / cross-subnet use.

## Architecture

```
┌──────────────────────── macOS ───────────────────────┐        ┌─────────────── Omarchy ──────────────┐
│ MouseTail.app (SwiftUI menu bar + arrangement window) │        │ mousetail (systemd --user service)   │
│        │ local socket (JSON lines)                     │        │   ↑ local socket ← Omarchy bar plugin│
│ mousetail-core (Rust)                                 │  QUIC  │ mousetail-core (Rust)                │
│  • capture: CGEventTap                                │◄──────►│  • emulate: Wayland virtual pointer  │
│  • layout + virtual cursor (authoritative)            │ TLS1.3 │    + virtual keyboard (no root)      │
│  • clipboard: NSPasteboard                            │ pinned │  • clipboard: data-control           │
│  • discovery: mDNS                                    │        │  • discovery: mDNS                   │
└───────────────────────────────────────────────────────┘        │  • Omarchy bar plugin (status)       │
                                                                  └──────────────────────────────────────┘
```

- **`mousetail-core`** (Rust library): protocol, pairing, crypto, discovery, layout maths,
  virtual cursor, key mapping, clipboard sync logic. Platform backends behind traits:
  `InputCapture`, `InputEmulation`, `Clipboard`, `DisplayInfo`.
- **macOS app** (`apps/macos`): SwiftUI menu bar app and arrangement window. It bundles the
  Rust daemon (`Contents/MacOS/mousetaild`), runs it while open, and talks to it over the
  daemon's local control socket (one JSON request/response per line). No FFI: the same socket
  serves the CLI and the Omarchy bar plugin, so every front end sees the same thing.
- **Linux daemon** `mousetail run`: headless, runs as a systemd user service started with the
  Hyprland session. No window. Status via an Omarchy shell bar plugin (Omarchy 4 replaced
  Waybar with its Quickshell shell); pairing and events via desktop notifications.
  Feasibility of every piece below is recorded in [`research/RESULTS.md`](research/RESULTS.md).

### Roles

Every node runs both roles when its platform allows: a **controller** (captures its own
keyboard and mouse and may send them elsewhere) and a **target** (injects input it receives).
Each starts when its permissions allow (macOS Accessibility, Linux `/dev/uinput` where needed)
and is re-announced to peers in a fresh `Hello`, so granting a permission takes effect without
a restart. While a node is being controlled, its own controller is suspended, so an injected
cursor reaching an edge can't bounce on to a third machine; on macOS, injected events are also
tagged (`kCGEventSourceUserData`) and ignored by our own tap. The one exception: once the
controlling computer has been still for 250 ms, pushing this computer's own mouse against the
edge that leads back to it takes the cursor over there, and the controlling computer, told by
that `Enter`, brings its own cursor home.

### Platform backends

| | Capture (controller) | Injection (target) | Displays |
|---|---|---|---|
| macOS | Quartz event tap over the whole screen | HID-level `CGEventPost` (drags, click counts, modifier flags, pixel scrolling); wakes the display | CoreGraphics |
| Linux, layer-shell compositors (Hyprland, Sway, KDE…) | 1-px overlay strips on edges that lead somewhere; relative-pointer motion while resting on one; pointer lock + exclusive keyboard + shortcuts inhibitor while remote | `zwlr_virtual_pointer` + `zwp_virtual_keyboard` (no root), else uinput | `xdg-output` |
| Linux, GNOME | not yet (needs the InputCapture portal + libei) | uinput absolute pointer + keyboard (after `enable-input.sh` grants `/dev/uinput` via udev `uaccess`) | `xdg-output` |

Key codes travel as evdev codes. The receiving side translates for its platform: a Mac
controlling Linux gets the Command remap (below); Linux controlling a Mac is positional (Super
is ⌘, Alt is Option, Ctrl is Ctrl), the convention of Synergy, Barrier and Deskflow.

### One arrangement, two computers

Each controller keeps the other's placement in its own layout. Placements are exchanged
(`Placement { x, y, updated }`) and mirrored: if A puts B's origin at (x, y), B puts A's at
(−x, −y). The newest placement a person chose wins (`updated` is a Unix-ms timestamp; automatic
placements are 0); ties go to the smaller device id. A node that keeps its own placement
replies with it, so a message lost to a race (e.g. during pairing) can't leave the two
disagreeing. Re-pairing keeps the placement.

## The key idea: the controller is authoritative

The lock-screen trap in Lan Mouse happens because the *remote* decides when the cursor has hit
the edge to come back; when the lock screen owns input, it never finds out.

In MouseTail the **controller decides everything**:

1. While the cursor is on the remote, the Mac keeps its event tap active, hides and pins its own
   cursor, and tracks a **virtual cursor** in the unified layout coordinate space.
2. It applies its own pointer acceleration to raw deltas, clamps to the remote's displays, and
   sends **absolute** positions to the target.
3. When the virtual cursor crosses a shared edge back to a Mac display, the Mac releases capture
   and warps its real cursor to the matching point. **No message from the remote is required.**
4. Safety valves:
   - Hotkey (default `Ctrl+Opt+Esc`) always returns control.
   - Heartbeat: if the target stops acking for ~1s, return control automatically.
   - Disconnect → immediate return; all held keys are released on the target.

Absolute positioning means the two sides can never drift out of agreement about where the edge
is — even over the lock screen.

## Layout model

All displays from all machines live in one **unified layout** in logical points (not pixels),
exactly like macOS arranges its own displays.

- Each machine reports its displays: id, name, logical size, scale, position within its own
  local space (macOS `CGDisplayBounds`; Hyprland `hyprctl monitors -j`).
- A machine's displays keep their local arrangement as a rigid group. The user places the
  **group** relative to the other machine's group.
- A crossing exists wherever an edge of one machine's display touches an edge of another
  machine's display. Partial overlaps are fine — only the touching segment is a crossing.
  So attaching the iMac to the left of the *Dell* (not the MacBook screen) means only the Dell's
  left edge leads to the iMac.
- Mapping across an edge is one-to-one in layout points along the shared segment (as macOS
  does between its own displays); the arrangement decides where screens line up.
- Display hot-plug on either side re-reports displays; the layout is kept by display identity
  where possible and snapped back to a valid touching position if not.

The layout is stored on the controller and synced to the peer, so a future reverse-direction mode
uses the same arrangement.

### Arrangement window (macOS)

Modelled on *System Settings → Displays → Arrange*:

- Every display drawn to scale; each machine's displays tinted as a group and labelled
  ("This Mac", "omarchy-imac").
- Drag the remote group; it snaps to edges of local displays. Shared edges are highlighted to
  show exactly where the cursor can cross.
- Click a display to flash an identifier on the real screen (both machines).
- Minimal settings alongside: clipboard sync on/off, return hotkey, modifier mapping.

## Input

### Capture (macOS)

- `CGEventTap` at the session level for mouse moves, buttons, scroll (including continuous /
  trackpad scroll and momentum phases) and keys (including `flagsChanged` for modifiers).
- Trackpad scrolling goes out as `TrackpadScroll` with the Mac's momentum left out, and
  `TrackpadScrollEnd` when the fingers lift, so the receiver scrolls as if they were on its
  own trackpad: its touchpad scroll speed and its apps' kinetic scrolling apply.
- While remote: events are swallowed; cursor hidden and dissociated
  (`CGAssociateMouseAndMouseCursorPosition(false)`), deltas read from the events.
- Requires Accessibility + Input Monitoring permissions (first-run guide in the app).

### Emulation (Linux)

Unprivileged Wayland protocols — no root, no udev rule (verified on Hyprland 0.56, including
at the Omarchy lock screen):

- **Pointer:** `zwlr_virtual_pointer_v1.motion_absolute` with the extent set to the whole
  Hyprland layout, so multi-monitor maps naturally. Buttons and `axis` scroll (with
  `axis_source`/`axis_discrete` for smooth vs notched, `Finger` plus `axis_stop` for another
  computer's trackpad) on the same object. Absolute motion means the compositor's pointer
  speed doesn't apply: the controlling computer's own tracking speed does.
- **Keyboard:** `zwp_virtual_keyboard_v1`, loaded with the seat's own keymap so the iMac's
  layout applies; keys are evdev codes.
- Writes must wait for socket writability on `EWOULDBLOCK` rather than drop or panic.
- Fallback backend for non-wlroots compositors: uinput (then a udev rule is needed).

### Omarchy integration

- Injected input resets Omarchy's idle timers, so the iMac won't blank or lock while you're
  using it from the Mac.
- On entering the iMac, dismiss the screensaver the way Omarchy does (SIGTERM its script),
  since pointer motion alone doesn't close it.
- Hyprland 0.56 `hyprctl dispatch` takes Lua (`hl.dsp.…`); use the new syntax or the IPC
  socket directly.

### Key mapping

- Send **physical key positions** (mapped macOS virtual keycode → Linux evdev code). The target
  applies its own keyboard layout, so both machines should use the same layout.
- Modifier mapping (default, configurable):
  - `Cmd` → `Ctrl` so Cmd+C / Cmd+V / Cmd+T etc. behave as expected on Linux.
  - Exceptions stay `Super` for Hyprland: Cmd+Tab, Cmd+Space, Cmd+Return, Cmd+number,
    Cmd+arrows, and Cmd on its own. The exception list is editable.
  - `Ctrl` → `Ctrl`; `Opt` → `Alt`.
- On leaving the remote (or disconnecting) all pressed keys and buttons are released, so nothing
  gets stuck.
- Keys held at the moment of crossing are not carried across.

## Clipboard

- On crossing, the machine being **left** sends its clipboard to the machine being entered,
  unless that machine already has it. (Nothing is sent continuously.)
- Each clipboard goes on a QUIC stream of its own, so a big one doesn't hold up the clicks and
  keys that follow the crossing. The receiver answers once it has decided, and the sender only
  counts it as delivered if it was taken; otherwise it goes again next crossing. It can
  arrive before the `Enter` it belongs to, so the receiver waits up to 2 s for that. Peers
  before protocol 5 get it inline on the main stream.
- macOS: `NSPasteboard.changeCount` to detect changes; read/write text, then images (PNG) and
  file URLs later.
- Linux: data-control (`wl-clipboard-rs`) to read/set the clipboard without a window. Omarchy
  also ships a clipboard-history plugin; synced entries will show up there naturally.
- Size cap (default 10 MB) and an on/off switch.

## Crossing ripple

A water ripple spreads from the edge where the cursor crosses: a full one where it arrives, a
smaller one where it leaves. Only for show, on by default, with an on/off switch (`ripple`).

- Each machine draws its own side, from what it already knows: the controller's `Crossed`
  action (its own cursor leaving or coming home, at the exact edge point), and `Enter`/`Leave`
  when it's being controlled (snapped onto the edge facing the controlling computer, since the
  last injected position can be a fast flick short of it). Nothing new crosses the network.
- The shape is shared (`core::ripple`) so both platforms draw the same thing. macOS: a
  click-through window per display, drawn with Metal, on screen only while rippling (the
  daemon's main thread runs the run loop). Linux: a click-through overlay-layer surface on the
  monitor, drawn on the CPU into shared memory, repainting only the square the waves reach.
- `mousetail ripple <x> <y>` draws one without the daemon, for development.

## Sound

Sound goes to the computer you're sitting at: of two connected computers, whichever was last
used to push the cursor onto the other plays the other's sound. Each remembers the decision per
peer (`listen`, with when it was made). Until the first crossing both use the same default: a
Mac over anything else (it usually has the headphones), else the smaller id. After `Hello`
each side says whether it can play and send sound (`SoundCaps`), and if only one direction is
possible that's the one used. A peer that never says predates sound going both ways: a Mac
there only plays, Linux only sends.

The listener asks for sound with `SoundWanted { wanted, updated }` (`AudioWanted(bool)` for
older peers) on connect, on crossing and when the `audio` setting changes. If both sides think
they're the listener (their saved decisions disagree), the newer claim wins and the other
yields and starts sending. Protocol 4 peers hang up on messages they don't know, so they're
never sent `SoundCaps` or `Media`.

Sending:

- **Linux** creates a PipeWire `Audio/Sink` named after the listener (a real output in
  Omarchy's audio menu) and makes it the default, remembering the previous default. On stop it
  is removed and the previous default restored if the user hadn't picked something else.
- **macOS** (14.2+) taps everything the Mac plays except MouseTail itself with a Core Audio
  process tap, muted locally while tapped, so the sound moves rather than plays twice. It needs
  the System Audio Recording permission.

Whatever is captured is encoded with Opus (48 kHz stereo, 10 ms frames, 160 kbit/s, in-band
FEC) and sent as QUIC datagrams; silence isn't sent. The listener decodes into a playout buffer
(40 ms cushion, conceals short gaps, re-buffers after pauses, skips ahead if it falls 80 ms
past its cushion, resamples to the device rate) and plays through its current default output
(cpal on macOS, a PipeWire stream on Linux), opening it only while sound is arriving. Opus is
built in on macOS (self-contained app) and uses the system library on Linux.

The two machines' sound clocks drift apart by up to a few hundred ppm, so the resampler runs up
to 0.2% fast or slow to hold the cushion at its target instead of clicking every few minutes. A
dropout mid-sound grows the cushion by 20 ms (to 160 ms at most), and each calm minute shrinks it
10 ms back towards 40. Dropouts fade out and re-buffered sound fades in over 5 ms. Linux's
virtual speaker asks PipeWire for 512-frame quanta, so packets leave evenly rather than in
20-40 ms lumps.

**Media controls.** While another computer's sound is arriving, the listener presents itself
as a media player named after that computer: on macOS the "Now Playing" app
(`MPNowPlayingInfoCenter`/`MPRemoteCommandCenter`), on Linux an MPRIS player
(`org.mpris.MediaPlayer2.mousetail`, which Omarchy's shell and `playerctl` pick up). So
AirPods presses, media keys and the desktop's media controls send `Media(PlayPause | Next |
Previous)` to the sender, which presses the matching media key itself so its own handling
applies (Linux: on the virtual keyboard, where Omarchy binds them to its media controls; macOS: an
NX system-defined key event). Two seconds of silence counts as paused: the listener stays the
player, paused, so the next press resumes it. On disconnect, or with sound switched off, it
lets go. macOS delivers these commands on the main thread only, so on macOS the daemon's main
thread runs the run loop and the async runtime runs on another thread.

## Discovery and pairing

- Each node advertises `_mousetail._udp` via mDNS with its device id and name.
- Each device has a long-term self-signed certificate (its identity).
- First connection: the target shows a notification with a 4-digit code; the user enters it in
  the Mac's menu. The code authenticates an exchange of certificate fingerprints (SPAKE2), after
  which both sides pin each other's certificate.
- Each shown code allows one attempt, and only the newest code is live (a new request cancels
  the last, whoever asked). Every attempt counts against a machine-wide limit (3 per 10
  minutes, refunded on success, checked both when a code is shown and when it's used) and
  codes are shown at most every 5 s, so guessing a 4-digit code takes weeks, with a
  notification on screen for every try.
- Subsequent connections: mutual TLS with the pinned certificates. Unknown peers are rejected.
- Unpairing on either computer unpairs both (`NotPaired`).
- Pausing keeps the pairing and the connection, but nothing crosses: the paused computer
  leaves the layout, and input, clipboards and sound from it are ignored. The choice is
  stored per peer and shared (`Paused { paused, updated }`, newest wins, as for placement),
  so pausing or resuming on either computer does both. Releases before `Paused` just see
  `can_be_controlled: false` in a fresh `Hello`.

### Who connects to whom

Connection direction is independent of role. Both sides advertise and both dial; the first
authenticated connection wins and the other is dropped. This matters because Omarchy's ufw
blocks inbound connections by default while the Mac's application firewall allows them — so in
practice the iMac dials the Mac and the Omarchy install needs no firewall change and no sudo.

## Transport

- **QUIC** (`quinn`), one connection per peer, TLS 1.3 with pinned certificates.
  - Pointer motion: unreliable **datagrams** of absolute positions with a sequence number;
    latest wins, so loss self-heals and a Wi-Fi stall can't queue up stale motion. When the
    pointer rests, its final position is re-sent twice so a lost last packet can't leave the
    remote cursor short. (Measured on
    this mesh Wi-Fi: 5 ms average, occasional multi-second stalls.)
  - Keys, buttons, scroll, control: one reliable **stream**; each clipboard: a stream of
    its own.
- One QUIC endpoint per local IPv4 address, all on one port, rebound as networks change. A
  single wildcard socket would answer from the primary (Wi-Fi) address even when the peer
  dialled the Ethernet address, and stateful firewalls drop those replies.
- Path selection: race every (local address, peer address) pair on the same subnet, give the
  stragglers 150 ms after the first success, keep the lowest RTT. Here that's the Mac's wired
  link (~7 ms vs ~26 ms over the Wi-Fi mesh).
- Messages: `Hello`, `Displays`, `Layout`, `Enter{pos}`, `Leave`, `PointerAbs`, `Button`,
  `Scroll`, `Key`, `ReleaseAll`, `Clipboard`, `Heartbeat`, `Ack`.
- Automatic reconnect with backoff; survives sleep/wake and network changes (QUIC connection
  migration helps when the Mac swaps between Wi-Fi and Ethernet).

## Install and running

**macOS**
- `MouseTail.app` into /Applications (DMG or Homebrew cask later). Launch-at-login via
  `SMAppService`. Signed with a self-signed "MouseTail Release" certificate, the same one every
  release, so macOS keeps its permissions across updates (it isn't notarised, so the very first
  open still needs **Open Anyway**; a Developer ID would remove that).

**Omarchy**
- `curl -fsSL …/install.sh | sh` (AUR package later). **No sudo:** installs the binary to
  `~/.local/bin`, enables `mousetail.service` (`systemctl --user`, bound to
  `graphical-session.target`), and installs the bar plugin into `~/.config/omarchy/plugins/`.
- Starts with the Hyprland session; restarts on failure. `mousetail status` CLI for debugging.

### Updates

- One Ed25519 release key signs every download. Only the release workflow has it (secret
  `UPDATE_SIGNING_KEY`); its public half is `PUBLIC_KEY` in `crates/core/src/update.rs` and
  `SUPublicEDKey` in the app. `scripts/write-update-feeds.sh` signs each release and checks the
  signatures against that public key, so a mismatched key fails the release.
- Each release carries `appcast.xml` (for the Mac) and `latest.json` (Linux), fetched through
  GitHub's `releases/latest/download/…` links, so there's no server of our own.
- **Mac:** Sparkle checks every six hours, downloads in the background, verifies the signature
  and installs by relaunching the app, but holds the install until nobody is using another
  computer through this Mac.
- **Linux:** the daemon does the same (`crates/mousetail/src/update.rs`): verify, unpack, test-run
  the new binary's `--version`, wait until idle, swap the binary and bar plugin by renaming,
  keep the old binary in `~/.local/state/mousetail/mousetail.previous`, then `exec` the new one
  so the systemd service carries straight on. Only installer-made installs update themselves.
- Discovery advertises each computer's version (TXT `app`); seeing a newer one prompts a check
  right away, so paired computers don't stay on different releases for long.
- Releases must never break talking to the previous release: the protocol only gains things
  older peers can ignore, until both sides have had time to update. New information goes in
  new message kinds at the end of `Message`/`Datagram` (older peers skip frames they can't
  decode), never in new fields on existing messages; peers at or above
  `MIN_PROTOCOL_VERSION` are accepted (see `crates/core/src/proto.rs`).

## Local control socket

`$XDG_RUNTIME_DIR/mousetail.sock` (Linux) or `~/Library/Application Support/MouseTail/
mousetail.sock` (macOS), mode 0600. Requests: `status`, `layout`, `pair`, `pair_code`,
`unpair`, `set_paused`, `place_at` (drop + snap), `place`, `set_setting`, `release`, `update`, `shutdown`.
`mousetail watch` streams status as JSON lines for status bars. Only one daemon runs per user
(a lock on `mousetail.lock` beside the config); the Mac app starts its own with
`--exit-with-parent` so it stops with the app, crash or not.

## Milestones

1. **Spike** ✅ (see `research/RESULTS.md`).
2. **Core** ✅: discovery, code pairing, pinned-certificate QUIC, keyboard with Command
   mapping, stuck-key protection, dead-peer detection (~1.5 s) and automatic reconnect.
3. **Multi-monitor layout** ✅: unified layout, drop-to-snap placement, per-peer placement saved.
4. **Clipboard** ✅ text. Images and files still to do.
5. **Mac app** ✅ first version: menu bar, pairing, arrangement window, permission guidance,
   Secure Input warning. Still to do: app icon, signed/notarised DMG, first-run walkthrough.
6. **Linux** ✅ first version: no-sudo installer/uninstaller, systemd user service, Omarchy bar
   plugin with pairing code, screensaver handling. Still to do: prebuilt binaries (install
   without Rust), AUR package.
7. **Sound** ✅ either way between Macs and Linux, following where you sit, with media
   controls.
8. **Later:** reverse direction (Linux capture via evdev grab or InputCapture portal, macOS
   injection via `CGEventPost`), multiple remotes, file transfer, image clipboard.

## Known issues / next up

- Occasional multi-second Wi-Fi stalls (likely AWDL); reconnect backoff is capped at 4 s.

## Reference setup

Developed and tested on:

- **Controller:** MacBook Pro (Apple Silicon), macOS 27. Built-in display (main) with a 27"
  external display above it; Wi-Fi plus USB Ethernet.
- **Target:** an Intel iMac running Omarchy 4 (Hyprland 0.56, PipeWire 1.6), on Wi-Fi, with
  ufw denying inbound connections, sitting to the left of the Mac.
- `dev/` holds the tools used for this: `sync.sh` copies the tree to the Linux machine
  (`MOUSETAIL_REMOTE`, an SSH host), `dev.sh` rebuilds and restarts both sides, and `e2e.sh`
  drives the Mac with synthetic input and checks the Linux side over SSH.
- mDNS needs both machines on the same LAN.
