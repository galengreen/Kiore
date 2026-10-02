# GNOME Shell extension — design

2026-10-02 · Agreed in conversation; this records it for review before planning.

## Goal

People on GNOME get what Omarchy gets from its bar plugin: an icon in the top bar whose menu
shows MouseTail's state and has every control the Omarchy panel has, plus Arrange Displays.
The installer works out which desktop it's on and puts the matching integration in place; the
uninstaller takes away whichever is there.

Success: on this Fedora 44 / GNOME 50 machine, a fresh `install.sh` from a release folder
leaves the extension installed and enabled for the next login; after logging in, every control
below works against the running daemon; `uninstall.sh` removes the extension again.

## Not in scope

- GNOME versions other than 50: only 50 can be tested here (add others once tried).
- Daemon changes or new IPC: the extension only uses existing `mousetail` commands.
- A keyboard shortcut for Arrange Displays (Omarchy's IPC hook has no GNOME equivalent yet).
- Publishing on extensions.gnome.org.

## Files

`integrations/gnome/mousetail@galen.green/` (UUID on the project's domain, galen.green):

| File | Role |
|---|---|
| `metadata.json` | uuid, name, description, `shell-version: ["50"]`, url; `version-name` set by packaging |
| `extension.js` | the `Extension`: the top-bar indicator and its menu, `mousetail watch`, the command queue |
| `arrange.js` | the Arrange Displays dialog |
| `status.js` | pure logic, no GNOME imports: the summary line, each computer's status line, the arrange view's fit and transform |
| `stylesheet.css` | menu and dialog styling |
| `icons/mousetail-symbolic.svg` | the logo (from the Omarchy plugin's `icon.svg`) as a symbolic icon, so the top bar tints it |
| `test/status.test.js` | a runnable check of `status.js` |

MIT, like the rest of the repo.

## Talking to MouseTail

Exactly as the Omarchy plugin does, through `~/.local/bin/mousetail`:

- **Status:** `mousetail watch` under `Gio.Subprocess`, one JSON line per change (the status, or
  `{"running": false}`). When it exits: not running, and start it again 3 s later. Killed in
  `disable()`.
- **Commands,** one at a time in a queue: `set <clipboard|audio|ripple|updates> on|off`,
  `pause|resume|unpair <id>`, `update`, and `systemctl --user
  enable|disable|start|stop|is-enabled mousetail`. A command's stderr (less a leading
  `mousetail: `) or `update`'s stdout becomes the menu's message line.
- **Pairing:** `mousetail pair <id>` with stdin open; the typed code goes to stdin; a
  `Paired with` line ends it. A wrong code ends the command, so stderr or a failed exit shows
  as the error and the row offers **Pair…** again for a fresh code (the Omarchy panel leaves a
  dead field there instead).
- **Arrange:** `mousetail layout` (machines with displays and offsets, and `crossings`), and
  `mousetail place-at <id> <x> <y>` (answers with the snapped `offset`).
- **Fix Firewall…:** runs `~/.local/share/mousetail/enable-firewall.sh` in a terminal (it asks
  for a password): `xdg-terminal-exec`, else Ptyxis, Console (`kgx`), GNOME Terminal.

## The menu

Same order and wording as the Omarchy panel:

1. **Header:** the logo, "MouseTail", and the summary: Not running / Pairing with X / In use
   from X / Using X / Connected to X, Y / Not connected.
2. **Problems** (only when there are any): the capture error; "Other computers can't control
   this one yet. Run this once: ~/.local/share/mousetail/enable-input.sh"; the firewall note with
   a **Fix Firewall…** button.
3. **Pairing code** (while this computer shows one): the digits, large and spaced, and "Type
   this on X to connect it." A newly shown code opens the menu, as on Omarchy.
4. **Computers:** each paired or connected one, with its name (dimmed when offline or paused)
   and status line (Found on your network / Paused / Offline / Using this computer now /
   Connected · its sound plays here …). **Pair…** for unpaired ones found on the network;
   **Pause/Resume** and **Forget** for paired ones. Pairing shows a code field under the row
   (Enter sends, Escape or Cancel stops). "Looking for other computers on your network…" when
   there are none. **Arrange Displays…** once one is paired.
5. **Settings** (switches): Sound follows you, Share clipboard, Ripple when crossing, Start at
   login (read with `systemctl --user is-enabled` each time the menu opens), Update
   automatically.
6. **Footer:** the last message, **Check for Updates**, **Stop MouseTail** / **Start
   MouseTail**, and the version. When not running: "MouseTail isn't running." and Start.

## Arrange Displays

A modal dialog (GNOME's `ModalDialog`: the screen dims, a card in the middle, Escape closes),
looking like the Omarchy overlay: MouseTail's own colours (#080808 card, #111212 canvas,
#f2f1ec text, #ffeba7 glow) rather than the theme's; "Arrange Displays" and the same help text;
every machine's displays drawn to scale and fitted with a 40 px margin; this computer graphite,
others warm, offline ones dim; a strip on each computer's main display; "This computer" or the
name, and the display name or "Offline". Other computers' tiles drag; dropping one calls
`place-at` and the tile glides (220 ms) to where it snapped. Glowing lines mark the crossings,
hidden while dragging. The layout reloads every 2 s while nothing is being dragged. Footer:
the legend (This computer · Other computers · Cursor crosses here) and **Done**.

## Install, update, uninstall

- **Which desktop:** Omarchy when `~/.config/omarchy` exists (as now). GNOME when
  `$XDG_CURRENT_DESKTOP` names GNOME or `gnome-shell` is running for this user (so it also works
  from SSH or a TTY). Neither: no integration, as now.
- **Installing on GNOME:** copy the extension to
  `${XDG_DATA_HOME:-~/.local/share}/gnome-shell/extensions/mousetail@galen.green` (replacing an
  old copy), then `gnome-extensions enable`; for a new install the running shell hasn't seen it
  and refuses, so add the UUID to `org.gnome.shell enabled-extensions` (and take it out of
  `disabled-extensions`) through `gjs` and `Gio.Settings`. Say the icon appears after the next
  login (GNOME on Wayland only loads new extensions then). If all extensions are switched off
  (`disable-user-extensions`), say that instead.
- **Uninstalling:** if the extension is there: `gnome-extensions disable` (or take the UUID out
  of `enabled-extensions`), then delete it. The Omarchy plugin is removed as now.
- **Updating:** the updater replaces an installed extension from the release's
  `gnome-extension/` folder, like the Omarchy plugin; GNOME runs the new copy from the next login.
- **Packaging:** the Linux tarball gains `gnome-extension/mousetail@galen.green`, its
  `version-name` set to MouseTail's version.

## Testing

- `test/status.test.js`: the summary for each state, the status lines, and the fit maths, run
  with Node in podman (no Node on the host). `node --check` on every module.
- The installer, end to end on this machine: package a release folder in the build container,
  run its `install.sh`, check the files and `enabled-extensions`; run `uninstall.sh`, check
  they've gone.
- The UI in a real GNOME Shell: a nested one (`gnome-shell --devkit`) if `mutter-devkit` gets
  installed, otherwise after logging out and in. Check each menu item and the arrange drag
  against the running daemon and the Omarchy laptop.

## Docs

README (the installer's note, arranging from GNOME's top bar, the project layout table), the
website table's GNOME cells (status, pause, arrange), and DESIGN.md's front-end note.
