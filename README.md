<p align="center">
  <img src="assets/logo-512.png" width="128" alt="MouseTail logo: a mouse leaping, its tail a cursor trail ending in an arrow">
</p>

<h1 align="center">MouseTail</h1>

<p align="center">
  <strong>One mouse. Every computer.</strong><br>
  Push your cursor off the edge of one computer's screen and keep going, onto the one beside it.<br>
  Your keyboard, clipboard and sound come too.
</p>

<p align="center">
  <a href="https://mousetail.galen.green/">Website</a> ·
  <a href="https://github.com/galengreen/mousetail/releases/latest">Download</a> ·
  <a href="docs/DESIGN.md">How it works</a>
</p>

<p align="center">
  <img src="assets/demo.gif" width="720" alt="Demo: the cursor moves from a MacBook onto an iMac, pastes text copied on the Mac, and the iMac's sound plays through the Mac's headphones">
</p>

## What it does

- **Push off the edge.** Arrange your computers like the Displays settings on your Mac; the
  cursor crosses wherever the screens meet, even across a gap if they don't line up exactly.
- **Never gets stuck.** Your Mac is always in charge. A lock screen, a frozen computer or a
  Wi-Fi drop-out can't trap the cursor: it comes straight home. **Ctrl + Option + Esc** always
  brings it back too.
- **Clipboard follows you.** Copy on one computer, paste on the other.
- **Sound follows you.** The computer you're sitting at plays the others' sound, through
  whatever it's using, AirPods included: push the cursor from your Mac onto Linux and Linux's
  sound comes to the Mac; push from Linux onto the Mac and the Mac's sound goes to Linux.
  Pressing your AirPods or the play/pause and skip keys controls whatever is playing.
- **Shortcuts feel right.** From a Mac to Linux, ⌘Tab, ⌘Space, ⌘Return, ⌘arrows and ⌘numbers
  go to the desktop as Super, ⌘C/⌘V/⌘X use Omarchy's universal copy and paste where it has it,
  and everything else (⌘T, ⌘F, ⌘W…) becomes Ctrl. From Linux to a Mac, Super is ⌘ and Alt is
  Option, where those keys sit.
- **Wakes things up.** Moving onto a blanked or locked screen turns it on; pushing towards a
  sleeping computer sends it a Wake-on-LAN packet.
- **Nothing to configure.** Computers find each other on your network, pair once with a
  four-digit code and pick the fastest connection (wired beats Wi-Fi).

Any computer can be the one you're sitting at: Mac → Linux, Linux → Mac, Mac → Mac and
Linux → Linux, and each can be both at different times.

| | Main computer (yours moves over) | Controlled (you move onto it) |
|---|---|---|
| **macOS 14.2+** | Yes | Yes |
| **Linux: Hyprland** (incl. [Omarchy](https://omarchy.org)) | Yes | Yes |
| **Linux: Sway, river, niri and other wlroots desktops** | Yes, untested | Yes, untested |
| **Linux: KDE Plasma** | Yes, untested | Yes, after `enable-input.sh`, untested |
| **Linux: GNOME** | Yes; GNOME asks each time MouseTail starts | Yes, after `enable-input.sh`, untested |
| **Windows** | Planned | Planned |

Clipboard works everywhere except GNOME (which doesn't let background apps use the
clipboard yet). Sound goes either way between Macs and Linux (PipeWire); sending a Mac's sound
needs the System Audio Recording permission.

## Install

**Mac** (macOS 14.2 or later, Apple Silicon or Intel)

1. Download [MouseTail-macos.dmg](https://github.com/galengreen/mousetail/releases/latest/download/MouseTail-macos.dmg)
   and drag MouseTail into Applications.
2. Open it. This build isn't notarised yet, so the first time macOS will refuse: open
   **System Settings → Privacy & Security** and click **Open Anyway**.
3. Allow **Local Network**, **Accessibility** and **Input Monitoring** when asked. MouseTail starts
   working as soon as they're on.

**Linux** (Wayland), no sudo needed:

```sh
curl -fsSL https://mousetail.galen.green/install.sh | sh
```

This installs `~/.local/bin/mousetail`, runs it as a systemd user service that starts with your
desktop and adds an icon to the bar on Omarchy, or to the top bar on GNOME 50 (from your next
login). On GNOME or KDE the installer will ask you to run
`~/.local/share/mousetail/enable-input.sh` once (it needs your password) so other
computers can control this one. `~/.local/share/mousetail/uninstall.sh` removes everything. To
let your Mac wake this computer from sleep, run `~/.local/share/mousetail/enable-wake.sh`
(asks for your password once).

**Pair:** on a Mac, click the mouse in the menu bar, then **Pair…** next to the other computer;
on Linux, run `mousetail pair`. Type the code the other computer shows. Then put it where it sits on
your desk: **Arrange Displays…** on a Mac, in the Omarchy bar or in GNOME's top bar, or `mousetail place <computer> left` on Linux. Both
computers share one arrangement, so you only do this once.

**Updates** install themselves. MouseTail checks for a new release every few hours (and
straight away if another computer already has it, so the two stay compatible), and installs
it once you're not using another computer through it. Turn it off with **Update
automatically** in the Mac menu, or `mousetail set updates off` on Linux; `mousetail update`
installs the latest now.

## Privacy and security

- MouseTail only talks to computers on your local network, directly. There's no account, server
  or cloud.
- Every connection is encrypted (QUIC with TLS 1.3). Each computer has its own key; pairing
  pins the other computer's key, and unpaired computers can do nothing but ask to pair.
- Pairing uses SPAKE2 with the four-digit code, so the code never crosses the network. Each
  code allows one attempt and repeated wrong codes lock pairing for a while.
- Keystrokes are only sent while the cursor is on the other computer. macOS blocks this
  entirely while a password field has Secure Input on; MouseTail tells you when that happens.
- Updates come from this repository's GitHub releases and are signed with MouseTail's release
  key; anything without a valid signature is refused. The only other thing MouseTail fetches
  is that update check.

Found a security problem? Please open a private
[security advisory](https://github.com/galengreen/mousetail/security/advisories/new) rather than
an issue.

## Command line

Both platforms have the same `mousetail` command (on the Mac it's inside the app:
`/Applications/MouseTail.app/Contents/MacOS/mousetaild`).

```
mousetail status                      what's connected, and where the cursor is
mousetail pair [computer]             pair (the other computer shows a code to type here)
mousetail unpair <computer>           forget a computer
mousetail pause <computer>            stop sharing with it for now (it stays paired)
mousetail resume <computer>           start sharing with it again
mousetail place <computer> <side> [n] put it left/right/above/below display n
mousetail set <clipboard|audio|updates|ripple> <on|off>
mousetail update                      install the latest release now (Linux)
mousetail release                     bring the cursor home
mousetail watch                       status as JSON lines, for status bars
```

## Building from source

You'll need Rust (stable), and on Linux the PipeWire and Opus development packages
(`libpipewire-0.3-dev libopus-dev clang` on Debian/Ubuntu; `pipewire opus clang` on Arch).

```sh
scripts/install-linux.sh          # Linux: build and install for your user
scripts/build-mac-app.sh          # Mac: build dist/MouseTail.app
scripts/package-mac.sh            # Mac: universal DMG
scripts/package-linux.sh          # Linux: release tarball
cargo test -p mousetail-core          # the core's tests
```

Releases are built by GitHub Actions when a `v*` tag is pushed.

## Project layout

| Path | What's there |
|---|---|
| `crates/core` | Platform-independent core: layout and cursor controller, keys, protocol, pairing, QUIC transport, discovery, audio (with tests) |
| `crates/mousetail` | The daemon and CLI, with macOS and Linux backends |
| `apps/macos` | SwiftUI menu bar app |
| `integrations/omarchy` | Omarchy bar plugin |
| `integrations/gnome` | GNOME Shell extension (and its test) |
| `website` | [mousetail.galen.green](https://mousetail.galen.green/) |
| `docs/DESIGN.md` | Architecture and the reasoning behind it |
| `research` | The feasibility experiments done before building, and their results |
| `dev` | Scripts for developing against a real Mac + Linux pair |

## Contributing

Issues and pull requests are welcome. For anything big (a new platform, say), open an issue
first so we can talk it through. Please run `cargo fmt`, `cargo clippy` and
`cargo test -p mousetail-core` before sending changes.

## Licence

[MIT](LICENSE) © Galen Green. Made in Aotearoa New Zealand.
