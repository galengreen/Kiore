# GNOME Shell extension — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A GNOME Shell 50 extension that gives GNOME what the Omarchy bar plugin gives Omarchy, installed and removed by MouseTail's own installer and uninstaller.

**Architecture:** An ESM GNOME Shell extension in `integrations/gnome/mousetail@galen.green/`. A
`PanelMenu.Button` streams status from `mousetail watch` and runs every action as a `mousetail` or
`systemctl --user` command, exactly like the Omarchy plugin. A `ModalDialog` subclass draws
Arrange Displays. The logic that needs no GNOME (summary lines, the arrange view's maths) lives in
`status.js` so Node can test it. The installer picks the integration by desktop.

**Tech Stack:** GJS (ESM), St, Clutter, Gio.Subprocess; GNOME Shell 50.5 APIs (`PanelMenu`,
`PopupMenu`, `ModalDialog`); bash; Rust (one updater hunk); Node 22 (tests, in podman).

**Spec:** `docs/superpowers/specs/2026-10-02-gnome-extension-design.md`

## Global Constraints

- `metadata.json`: `"uuid": "mousetail@galen.green"`, `"shell-version": ["50"]`.
- Extension files live in `integrations/gnome/mousetail@galen.green/`; tests in `integrations/gnome/test/` (never installed).
- Only existing commands through `~/.local/bin/mousetail` (`watch`, `set`, `pause`, `resume`, `unpair`, `pair`, `update`, `layout`, `place-at`) and `systemctl --user … mousetail`. No daemon changes.
- MIT, like the repo; no GPL template header.
- Wording follows the Omarchy panel (`integrations/omarchy/nz.galengreen.mousetail/Widget.qml`) exactly, except where GNOME needs otherwise (no tooltips; pairing offers Pair… again after a wrong code).
- Arrange Displays uses MouseTail's colours: card `#080808`, canvas `#111212`, text `#f2f1ec`, glow `#ffeba7`; tiles graphite (`#383939`→`#292a2a`) for this computer, warm (`#4a4535`→`#353226`) for others, dim (`#232424`→`#1b1c1c`) offline; glide 220 ms.
- GNOME 50 APIs: `St.BoxLayout` takes `orientation: Clutter.Orientation.VERTICAL` (not `vertical`).
- Every `GObject.registerClass` gets an explicit `GTypeName` starting `MouseTail`, so it can't clash with another extension's class.
- Installer: Omarchy when `$config_home/omarchy` exists; GNOME when `$XDG_CURRENT_DESKTOP` contains `GNOME` or `gnome-shell` runs for this user.
- Commit as the global git user; no attribution lines in commit messages.

## Review Focus

- MouseTail not installed, or its daemon stopped and started: the icon says "Not running" and recovers within ~3 s of it starting, without errors repeating in the log (Task 2, step 7).
- The extension disabled while commands are still running (the screen locks, say): their replies must not touch destroyed widgets (Task 2, step 8).
- A status update arriving while a pairing code is half typed must leave the field and its text alone (Task 3, step 6).
- Arrange Displays closed while `layout` or `place-at` is still running: no errors when the reply comes (Task 4, step 6).
- A newly shown pairing code opens the menu once, not again on every later update (Task 3, step 7).

---

### Task 1: Status logic (`status.js`) and its test

**Files:**
- Create: `integrations/gnome/mousetail@galen.green/status.js`
- Test: `integrations/gnome/test/status.test.js`

**Interfaces:**
- Produces (all pure, no imports): `parse(line) → status | null`; `nameOf(status, id) → string`; `shownPeers(status) → peer[]`; `summary(status) → string`; `peerDetail(status, peer) → string`; `setting(status, key) → bool`; `placed(layout) → machine[]`; `offsetOf(machine) → {x, y}`; `fit(machines, width, height, margin = 40) → {scale, x, y, bx, by}`; `viewX(view, x)`, `viewY(view, y) → number`; `dropAt(machine, view, dx, dy) → {x, y}` (rounded); `crossingBar(view, [a, b], thickness = 3) → {x, y, width, height}`.

- [ ] **Step 1: Write the failing test**

`integrations/gnome/test/status.test.js`:

```js
// The top-bar menu's logic, outside GNOME: node --test integrations/gnome/test/
import assert from 'node:assert/strict';
import {test} from 'node:test';

import * as Status from '../mousetail@galen.green/status.js';

const omarchy = {id: 'o', name: 'omarchy', paired: true, connected: true, paused: false, sound: 'here'};
const running = {running: true, peers: [omarchy]};

test('reads watch lines, ignoring junk', () => {
    assert.equal(Status.parse('not json'), null);
    assert.deepEqual(Status.parse('{"running":false}'), {running: false});
    assert.deepEqual(Status.parse(JSON.stringify(running)), running);
});

test('the summary says what MouseTail is doing', () => {
    assert.equal(Status.summary({running: false}), 'Not running');
    assert.equal(Status.summary(running), 'Connected to omarchy');
    assert.equal(Status.summary({...running, controlling: 'o'}), 'Using omarchy');
    assert.equal(Status.summary({...running, controlled_by: 'o'}), 'In use from omarchy');
    assert.equal(Status.summary({...running, pairing_code: {name: 'mac', code: '1234'}}), 'Pairing with mac');
    assert.equal(Status.summary({...running, pairing_code: {code: '1234'}}), 'Pairing with another computer');
    assert.equal(Status.summary({...running, peers: [{...omarchy, paused: true}]}), 'Not connected');
});

test('each computer says how it is', () => {
    const detail = p => Status.peerDetail(running, p);
    assert.equal(detail(omarchy), 'Connected · its sound plays here');
    assert.equal(detail({...omarchy, sound: 'there'}), 'Connected · plays your sound');
    assert.equal(detail({...omarchy, connected: false}), 'Offline');
    assert.equal(detail({...omarchy, paused: true, connected: false}), 'Paused · Offline');
    assert.equal(detail({...omarchy, paired: false}), 'Found on your network');
    assert.equal(Status.peerDetail({...running, controlled_by: 'o'}, omarchy), 'Using this computer now');
    assert.equal(Status.nameOf(running, 'nobody'), 'nobody');
});

test('only paired or found computers are listed', () => {
    const gone = {id: 'g', name: 'gone', paired: false, connected: false};
    assert.deepEqual(Status.shownPeers({running: true, peers: [omarchy, gone]}), [omarchy]);
});

test('settings read as the daemon defaults them', () => {
    assert.equal(Status.setting({running: true}, 'clipboard'), true);
    assert.equal(Status.setting({settings: {}}, 'clipboard'), false);
    assert.equal(Status.setting({settings: {clipboard: true, audio: false}}, 'audio'), false);
    assert.equal(Status.setting({settings: {}}, 'ripple'), true);
});

test('the arrange view fits everything in, centred, with a margin', () => {
    const screen = {rect: {x: 0, y: 0, w: 1000, h: 500}};
    const machines = [
        {id: 'f', this: true, displays: [screen]},
        {id: 'o', offset: {x: 1000, y: 0}, displays: [screen]},
    ];
    const view = Status.fit(machines, 480, 400);
    // 2000 × 500 points into 400 × 320 pixels: the width decides.
    assert.equal(view.scale, 0.2);
    assert.equal(view.x, 40);
    assert.equal(view.y, 150);
    assert.equal(Status.viewX(view, 1000), 240);
    assert.equal(Status.viewY(view, 500), 250);
    assert.deepEqual(Status.dropAt(machines[1], view, 20, -10), {x: 1100, y: -50});
    assert.deepEqual(Status.crossingBar(view, [{x: 1000, y: 0}, {x: 1000, y: 500}]),
        {x: 238.5, y: 148.5, width: 3, height: 103});
    assert.deepEqual(Status.placed({machines: [...machines, {id: 'x', displays: [screen]}]}), machines);
    assert.deepEqual(Status.fit([], 480, 400), {scale: 0.1, x: 0, y: 0, bx: 0, by: 0});
});
```

- [ ] **Step 2: Run it to see it fail**

Run: `podman run --rm --security-opt label=disable -v "$PWD":/src -w /src docker.io/library/node:22-alpine node --test integrations/gnome/test/`
Expected: FAIL, `Cannot find module '…/mousetail@galen.green/status.js'`.

- [ ] **Step 3: Write `status.js`**

```js
// What the top-bar menu says about MouseTail, worked out from `mousetail watch`, and the
// arrange view's maths. No GNOME imports, so Node can check it (../test/status.test.js).

/** A `mousetail watch` line: the status (`{running: false}` while the daemon is down), or null
 * for anything unreadable, which is ignored as the Omarchy panel does. */
export function parse(line) {
    let data;
    try {
        data = JSON.parse(line);
    } catch {
        return null;
    }
    return data?.running === true ? data : {running: false};
}

export function nameOf(status, id) {
    if (!id)
        return '';
    return (status.peers ?? []).find(p => p.id === id)?.name ?? id;
}

/** The computers worth listing: paired ones, and unpaired ones found on the network. */
export function shownPeers(status) {
    return (status.peers ?? []).filter(p => p.paired || p.connected);
}

/** One line on what MouseTail is doing, under the menu's title. */
export function summary(status) {
    if (!status.running)
        return 'Not running';
    if (status.pairing_code)
        return `Pairing with ${status.pairing_code.name || 'another computer'}`;
    if (status.controlled_by)
        return `In use from ${nameOf(status, status.controlled_by)}`;
    if (status.controlling)
        return `Using ${nameOf(status, status.controlling)}`;
    const connected = shownPeers(status).filter(p => p.paired && p.connected && !p.paused);
    if (connected.length > 0)
        return `Connected to ${connected.map(p => p.name).join(', ')}`;
    return 'Not connected';
}

/** How a computer is doing, under its name. */
export function peerDetail(status, p) {
    if (!p.paired)
        return p.connected ? 'Found on your network' : 'Not paired';
    if (p.paused)
        return p.connected ? 'Paused' : 'Paused · Offline';
    if (!p.connected)
        return 'Offline';
    if (status.controlled_by === p.id)
        return 'Using this computer now';
    const parts = ['Connected'];
    if (p.sound === 'here')
        parts.push('its sound plays here');
    if (p.sound === 'there')
        parts.push('plays your sound');
    return parts.join(' · ');
}

/** A setting's switch. Missing settings read as on, but a clipboard setting must say so. */
export function setting(status, key) {
    const settings = status.settings;
    if (!settings)
        return true;
    return key === 'clipboard' ? settings.clipboard === true : settings[key] !== false;
}

/** What the arrange view draws: this computer, and the others that have a place. */
export function placed(layout) {
    return layout?.machines?.filter(m => m.this || m.offset) ?? [];
}

export function offsetOf(m) {
    return m.offset ?? {x: 0, y: 0};
}

/** Fits every machine's displays into `width` × `height` view pixels with `margin` round them:
 * the scale, and where the layout's top-left (`bx`, `by`) lands (`x`, `y`). */
export function fit(machines, width, height, margin = 40) {
    const rects = machines.flatMap(m => {
        const o = offsetOf(m);
        return m.displays.map(d => ({x: d.rect.x + o.x, y: d.rect.y + o.y, w: d.rect.w, h: d.rect.h}));
    });
    if (rects.length === 0 || width <= 0 || height <= 0)
        return {scale: 0.1, x: 0, y: 0, bx: 0, by: 0};
    const x0 = Math.min(...rects.map(r => r.x));
    const y0 = Math.min(...rects.map(r => r.y));
    const x1 = Math.max(...rects.map(r => r.x + r.w));
    const y1 = Math.max(...rects.map(r => r.y + r.h));
    const scale = Math.max(0.01,
        Math.min((width - 2 * margin) / (x1 - x0), (height - 2 * margin) / (y1 - y0)));
    return {
        scale,
        x: (width - (x1 - x0) * scale) / 2,
        y: (height - (y1 - y0) * scale) / 2,
        bx: x0,
        by: y0,
    };
}

export function viewX(view, x) {
    return view.x + (x - view.bx) * view.scale;
}

export function viewY(view, y) {
    return view.y + (y - view.by) * view.scale;
}

/** Where machine `m`, dragged `dx`, `dy` view pixels, asks to go (for `mousetail place-at`). */
export function dropAt(m, view, dx, dy) {
    const o = offsetOf(m);
    return {x: Math.round(o.x + dx / view.scale), y: Math.round(o.y + dy / view.scale)};
}

/** The bar that draws a crossing edge, in view pixels, `thickness` across. */
export function crossingBar(view, [a, b], thickness = 3) {
    const x0 = viewX(view, Math.min(a.x, b.x));
    const y0 = viewY(view, Math.min(a.y, b.y));
    const x1 = viewX(view, Math.max(a.x, b.x));
    const y1 = viewY(view, Math.max(a.y, b.y));
    return {
        x: x0 - thickness / 2,
        y: y0 - thickness / 2,
        width: x1 - x0 + thickness,
        height: y1 - y0 + thickness,
    };
}
```

- [ ] **Step 4: Run the test to see it pass**

Run: `podman run --rm --security-opt label=disable -v "$PWD":/src -w /src docker.io/library/node:22-alpine node --test integrations/gnome/test/`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add integrations/gnome/mousetail@galen.green/status.js integrations/gnome/test/status.test.js
git commit -m "GNOME extension: what the menu says, and the arrange view's maths"
```

---

### Task 2: The extension, its icon and the menu's frame

The indicator, `mousetail watch`, the command queue, the header, settings and footer. Computers,
problems and the pairing code come in Task 3; Arrange Displays in Task 4 (its menu item
appears here but opens nothing until then).

**Files:**
- Create: `integrations/gnome/mousetail@galen.green/metadata.json`
- Create: `integrations/gnome/mousetail@galen.green/extension.js`
- Create: `integrations/gnome/mousetail@galen.green/stylesheet.css`
- Create: `integrations/gnome/mousetail@galen.green/icons/mousetail-symbolic.svg` (copy of `integrations/omarchy/nz.galengreen.mousetail/icon.svg`)
- Scratch (not committed): `$SCRATCH/devshell.sh`, `$SCRATCH/shell.sh`

**Interfaces:**
- Consumes: `Status.parse`, `Status.summary`, `Status.setting` (Task 1).
- Produces (inside `extension.js`, used by Tasks 3–4): `run(argv, cancellable, done(ok, out, err))`; `readLines(stream, cancellable, onLine, onEnd)`; `said(text)`; `note(text, styleClass)`; `row(...children)`; `button(label, onClick)`; `iconButton(iconName, accessibleName, onClick, styleClass)`; `Indicator` with `_status`, `_message`, `_cancellable`, `_update()`, `_refill(section, key, items)`, `_command(argv)`, sections `_problems`, `_code`, `_computers`, item `_arrange`; constants `BINARY`, `HELPERS`.

- [ ] **Step 1: Write `metadata.json`**

```json
{
  "uuid": "mousetail@galen.green",
  "name": "MouseTail",
  "description": "Share one keyboard, mouse, clipboard and sound between this computer and the ones beside it. Shows what's connected and any pairing code.",
  "url": "https://github.com/galengreen/mousetail",
  "shell-version": ["50"],
  "version-name": "0.2.7"
}
```

- [ ] **Step 2: The icon**

```bash
mkdir -p integrations/gnome/mousetail@galen.green/icons
cp integrations/omarchy/nz.galengreen.mousetail/icon.svg integrations/gnome/mousetail@galen.green/icons/mousetail-symbolic.svg
```

A `-symbolic.svg` file is tinted by the shell like its own icons, whatever its fill.

- [ ] **Step 3: Write `extension.js`**

```js
// MouseTail in GNOME's top bar: the MouseTail icon, and a menu with what the Omarchy bar
// plugin's panel has: who's connected (pair, pause, forget), Arrange Displays (arrange.js), any
// pairing code, the settings, and updates. Status streams from `mousetail watch`, one JSON line
// per change; everything else is the `mousetail` command.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Pango from 'gi://Pango';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

import * as Status from './status.js';

const BINARY = GLib.build_filenamev([GLib.get_home_dir(), '.local', 'bin', 'mousetail']);
const HELPERS = GLib.build_filenamev([GLib.get_user_data_dir(), 'mousetail']);

/** Run `argv`, then `done(ok, stdout, stderr)`, unless `cancellable` was cancelled first. */
function run(argv, cancellable, done) {
    let proc;
    try {
        proc = Gio.Subprocess.new(argv,
            Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE);
    } catch (e) {
        done(false, '', e.message);
        return;
    }
    proc.communicate_utf8_async(null, cancellable, (p, result) => {
        if (cancellable.is_cancelled())
            return;
        let out = '', err = '';
        try {
            [, out, err] = p.communicate_utf8_finish(result);
        } catch (e) {
            err = e.message;
        }
        done(p.get_successful(), out ?? '', err ?? '');
    });
}

/** Call `onLine` with each line `stream` gives, then `onEnd`, unless cancelled. */
function readLines(stream, cancellable, onLine, onEnd = () => {}) {
    const lines = new Gio.DataInputStream({base_stream: stream, close_base_stream: true});
    const next = () => lines.read_line_async(GLib.PRIORITY_DEFAULT, cancellable, (s, result) => {
        if (cancellable.is_cancelled())
            return;
        let line = null;
        try {
            [line] = s.read_line_finish_utf8(result);
        } catch {
            // The process has gone.
        }
        if (line === null) {
            onEnd();
            return;
        }
        onLine(line);
        next();
    });
    next();
}

/** What a command said on stderr, as the panel shows it. */
function said(text) {
    return text.trim().replace(/^mousetail: /, '');
}

/** Wrapped text, as wide as the menu. Dimmed through opacity, so it suits light and dark. */
function note(text, styleClass = 'mousetail-note') {
    const label = new St.Label({text, style_class: styleClass, x_expand: true});
    label.clutter_text.line_wrap = true;
    label.clutter_text.line_wrap_mode = Pango.WrapMode.WORD_CHAR;
    label.clutter_text.ellipsize = Pango.EllipsizeMode.NONE;
    if (styleClass === 'mousetail-note')
        label.opacity = 165;
    return label;
}

/** A menu row holding other things (buttons, an entry), which isn't clickable itself: clicking
 * it, or a button in it, leaves the menu open. */
function row(...children) {
    const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
    children.forEach(child => item.add_child(child));
    return item;
}

function button(label, onClick) {
    const b = new St.Button({
        label,
        style_class: 'button mousetail-button',
        can_focus: true,
        y_align: Clutter.ActorAlign.CENTER,
    });
    b.connect('clicked', onClick);
    return b;
}

function iconButton(iconName, accessibleName, onClick, styleClass = '') {
    const b = new St.Button({
        style_class: `button mousetail-icon-button ${styleClass}`,
        can_focus: true,
        accessible_name: accessibleName,
        y_align: Clutter.ActorAlign.CENTER,
        child: new St.Icon({icon_name: iconName, icon_size: 16}),
    });
    b.connect('clicked', onClick);
    return b;
}

// A switch that leaves the menu open, like the Omarchy panel's.
const SettingItem = GObject.registerClass({GTypeName: 'MouseTailSettingItem'},
class SettingItem extends PopupMenu.PopupSwitchMenuItem {
    activate() {
        this.toggle();
    }
});

const Indicator = GObject.registerClass({GTypeName: 'MouseTailIndicator'},
class Indicator extends PanelMenu.Button {
    _init(path) {
        super._init(0.5, 'MouseTail');
        this._logo = Gio.icon_new_for_string(`${path}/icons/mousetail-symbolic.svg`);
        this.add_child(new St.Icon({gicon: this._logo, style_class: 'system-status-icon'}));
        this.menu.box.add_style_class_name('mousetail-menu');

        this._status = {running: false};
        this._message = '';
        this._atLogin = true;
        this._queue = [];
        this._busy = false;
        this._shown = new Map();
        this._cancellable = new Gio.Cancellable();

        this._build();
        this._update();
        this.menu.connect('open-state-changed', (_menu, open) => {
            if (open)
                this._opened();
        });
        this.connect('destroy', () => this._stop());
        this._watch();
    }

    _build() {
        const menu = this.menu;

        const title = new St.BoxLayout({
            orientation: Clutter.Orientation.VERTICAL,
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        });
        title.add_child(new St.Label({text: 'MouseTail', style_class: 'mousetail-title'}));
        this._summary = new St.Label({style_class: 'mousetail-summary'});
        this._summary.opacity = 165;
        title.add_child(this._summary);
        menu.addMenuItem(row(
            new St.Icon({gicon: this._logo, icon_size: 32, style_class: 'mousetail-logo'}),
            title));

        // Anything stopping MouseTail doing its job here; the pairing code (Task 3).
        this._problems = new PopupMenu.PopupMenuSection();
        menu.addMenuItem(this._problems);
        this._code = new PopupMenu.PopupMenuSection();
        menu.addMenuItem(this._code);

        this._computersHeading = new PopupMenu.PopupSeparatorMenuItem('Computers');
        menu.addMenuItem(this._computersHeading);
        this._computers = new PopupMenu.PopupMenuSection();
        menu.addMenuItem(this._computers);
        this._arrange = new PopupMenu.PopupMenuItem('Arrange Displays…');
        this._arrange.connect('activate', () => this._openArrange());
        menu.addMenuItem(this._arrange);

        this._settingsHeading = new PopupMenu.PopupSeparatorMenuItem('Settings');
        menu.addMenuItem(this._settingsHeading);
        this._switches = [
            ['Sound follows you', 'audio'],
            ['Share clipboard', 'clipboard'],
            ['Ripple when crossing', 'ripple'],
            ['Start at login', null],
            ['Update automatically', 'updates'],
        ].map(([label, key]) => {
            const item = new SettingItem(label, true);
            item.connect('toggled', (_item, on) => {
                if (this._syncing)
                    return;
                if (key)
                    this._command([BINARY, 'set', key, on ? 'on' : 'off']);
                else
                    this._setAtLogin(on);
            });
            menu.addMenuItem(item);
            return {item, key};
        });

        this._notRunning = row(note("MouseTail isn't running."));
        menu.addMenuItem(this._notRunning);

        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._messageLabel = note('');
        this._messageRow = row(this._messageLabel);
        menu.addMenuItem(this._messageRow);
        this._updatesButton = button('Check for Updates', () => this._checkForUpdates());
        this._startStop = button('Stop MouseTail', () => this._command(
            ['systemctl', '--user', this._status.running ? 'stop' : 'start', 'mousetail']));
        const actions = new St.BoxLayout({style_class: 'mousetail-actions', x_expand: true});
        actions.add_child(this._updatesButton);
        actions.add_child(this._startStop);
        menu.addMenuItem(row(actions));
        this._versionLabel = note('');
        this._versionRow = row(this._versionLabel);
        menu.addMenuItem(this._versionRow);
    }

    /** Make the menu say what the status says. */
    _update() {
        const st = this._status;
        const running = st.running === true;
        this._summary.text = Status.summary(st);

        this._computersHeading.visible = running;
        this._arrange.visible = running && Status.shownPeers(st).some(p => p.paired);
        this._settingsHeading.visible = running;
        this._syncing = true;
        for (const {item, key} of this._switches) {
            item.visible = running;
            item.setToggleState(key ? Status.setting(st, key) : this._atLogin);
        }
        this._syncing = false;

        this._notRunning.visible = !running;
        this._messageLabel.text = this._message;
        this._messageRow.visible = this._message !== '';
        this._updatesButton.visible = running;
        this._startStop.label = running ? 'Stop MouseTail' : 'Start MouseTail';
        this._versionLabel.text = st.version ? `MouseTail ${st.version}` : '';
        this._versionRow.visible = !!st.version;
    }

    /** Fill `section` with `items()` when what it shows (`key`) has changed, so an unrelated
     * update doesn't sweep away a field being typed in. A falsy key empties it. */
    _refill(section, key, items) {
        const shows = JSON.stringify(key || null);
        if (this._shown.get(section) === shows)
            return;
        this._shown.set(section, shows);
        section.removeAll();
        if (key)
            items().forEach(item => section.addMenuItem(item));
    }

    _opened() {
        this._message = '';
        run(['systemctl', '--user', 'is-enabled', 'mousetail'], this._cancellable, (_ok, out) => {
            this._atLogin = out.trim() === 'enabled';
            this._update();
        });
        this._update();
    }

    _openArrange() {
        // Task 4.
    }

    // -------------------------------------------------------------- commands

    /** Run a command once those before it have finished; what it says goes in the menu. */
    _command(argv) {
        this._queue.push(argv);
        if (!this._busy)
            this._next();
    }

    _next() {
        const argv = this._queue.shift();
        this._busy = argv !== undefined;
        if (!this._busy)
            return;
        run(argv, this._cancellable, (_ok, _out, err) => {
            if (said(err) !== '') {
                this._message = said(err);
                this._update();
            }
            this._next();
        });
    }

    _setAtLogin(on) {
        this._atLogin = on;
        this._command(['systemctl', '--user', on ? 'enable' : 'disable', 'mousetail']);
    }

    _checkForUpdates() {
        this._message = 'Checking for updates…';
        this._updatesButton.reactive = false;
        this._update();
        run([BINARY, 'update'], this._cancellable, (_ok, out, err) => {
            this._updatesButton.reactive = true;
            this._message = said(err) || out.trim();
            this._update();
        });
    }

    // -------------------------------------------------------------- status

    _watch() {
        let proc;
        try {
            proc = Gio.Subprocess.new([BINARY, 'watch'],
                Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_SILENCE);
        } catch {
            // Not installed (yet).
            this._watchAgain();
            return;
        }
        this._watcher = proc;
        readLines(proc.get_stdout_pipe(), this._cancellable, line => {
            const status = Status.parse(line);
            if (status)
                this._setStatus(status);
        });
        proc.wait_async(this._cancellable, (p, result) => {
            try {
                p.wait_finish(result);
            } catch {
                return; // Cancelled: the extension is going away.
            }
            this._watcher = null;
            this._setStatus({running: false});
            this._watchAgain();
        });
    }

    _watchAgain() {
        this._watchTimer = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 3, () => {
            this._watchTimer = 0;
            this._watch();
            return GLib.SOURCE_REMOVE;
        });
    }

    _setStatus(status) {
        this._status = status;
        this._update();
    }

    _stop() {
        this._cancellable.cancel();
        this._watcher?.force_exit();
        if (this._watchTimer)
            GLib.source_remove(this._watchTimer);
    }
});

export default class MouseTailExtension extends Extension {
    enable() {
        this._indicator = new Indicator(this.path);
        Main.panel.addToStatusArea(this.uuid, this._indicator);
    }

    disable() {
        this._indicator?.destroy();
        this._indicator = null;
    }
}
```

- [ ] **Step 4: Write `stylesheet.css`**

```css
/* The top-bar menu, as wide as the Omarchy panel. */
.mousetail-menu { width: 22em; }
.mousetail-title { font-weight: bold; font-size: 1.15em; }
.mousetail-summary,
.mousetail-note { font-size: 0.9em; }
.mousetail-urgent { font-size: 0.9em; color: #f66151; }
.mousetail-logo { margin-right: 6px; }
.mousetail-actions { spacing: 6px; }
.mousetail-button { padding: 4px 12px; font-size: 0.9em; }
.mousetail-icon-button { padding: 5px; border-radius: 99px; }
```

- [ ] **Step 5: Syntax check, and a nested shell to try it in**

Run: `podman run --rm --security-opt label=disable -v "$PWD":/src -w /src docker.io/library/node:22-alpine sh -c 'for f in integrations/gnome/mousetail@galen.green/*.js; do node --check "$f" || exit 1; done && echo SYNTAX-OK'`
Expected: `SYNTAX-OK`.

Write `$SCRATCH/devshell.sh` (scratchpad; not committed). It starts GNOME Shell nested in a
window, with a throwaway home (own dconf, own extensions, so nothing reaches the real session),
this extension, and a helper extension that turns on unsafe mode so the shell can be driven over
D-Bus:

```bash
#!/bin/bash
# Nested GNOME Shell for trying the extension: dev/devshell.sh, then shell.sh '<js>'.
set -euo pipefail
repo=/home/ming/Documents/projects/MouseTail
root=${SCRATCH:?}/devhome
rm -rf "$root"
mkdir -p "$root/.local/share/gnome-shell/extensions/unsafe@dev" "$root/.local/bin" "$root/.config" "$root/.cache"
ln -s "$repo/integrations/gnome/mousetail@galen.green" "$root/.local/share/gnome-shell/extensions/"
ln -s "$HOME/.local/bin/mousetail" "$root/.local/bin/mousetail"
cat > "$root/.local/share/gnome-shell/extensions/unsafe@dev/metadata.json" <<'JSON'
{"uuid": "unsafe@dev", "name": "unsafe", "description": "dev only", "shell-version": ["50"]}
JSON
cat > "$root/.local/share/gnome-shell/extensions/unsafe@dev/extension.js" <<'JS'
export default class { enable() { global.context.unsafe_mode = true; } disable() {} }
JS
export HOME=$root XDG_CONFIG_HOME=$root/.config XDG_DATA_HOME=$root/.local/share XDG_CACHE_HOME=$root/.cache
exec dbus-run-session -- bash -c "
  echo \"\$DBUS_SESSION_BUS_ADDRESS\" > '$root/bus'
  gsettings set org.gnome.shell enabled-extensions \"['mousetail@galen.green', 'unsafe@dev']\"
  exec gnome-shell --devkit --wayland"
```

and `$SCRATCH/shell.sh`, which runs JavaScript in it (with `shot` to screenshot it to a PNG):

```bash
#!/bin/bash
# shell.sh '<js>' runs it in the nested shell; shell.sh shot FILE.png screenshots it.
root=${SCRATCH:?}/devhome
export DBUS_SESSION_BUS_ADDRESS=$(cat "$root/bus")
code=$1
if [[ $1 == shot ]]; then
  code="(async () => { const f = Gio.File.new_for_path('$2'); const s = f.replace(null, false, 0, null);
    await new Shell.Screenshot().screenshot(false, s); s.close(null); return 'saved'; })()"
fi
gdbus call --session --dest org.gnome.Shell --object-path /org/gnome/Shell --method org.gnome.Shell.Eval "$code"
```

Run (background): `SCRATCH=$SCRATCH bash $SCRATCH/devshell.sh > $SCRATCH/devshell.log 2>&1`
Expected: a window with a nested GNOME Shell; `$SCRATCH/devshell.log` has no `JS ERROR` naming `mousetail@galen.green`.

- [ ] **Step 6: Look at it**

Run: `bash $SCRATCH/shell.sh "Main.panel.statusArea['mousetail@galen.green'].menu.open(); 'ok'"` then `bash $SCRATCH/shell.sh shot $SCRATCH/menu.png`, and view the PNG.
Expected: the MouseTail icon in the nested top bar; the menu shows the logo, "MouseTail", the summary ("Connected to omarchy" with the daemon running), Computers and Settings headings, the five switches reflecting `mousetail status`, Check for Updates / Stop MouseTail, and "MouseTail 0.2.7".

Toggle a switch from the shell (`…menu._getMenuItems()` or the `_switches` list via `statusArea[…]._switches[2].item.toggle()`), wait a second, check `mousetail status` shows the setting changed, and toggle it back.

- [ ] **Step 7: Review focus: not installed, stopped and started**

Run: stop the daemon (`pkill -x mousetail` for a dev run, or `systemctl --user stop mousetail`), wait 4 s, screenshot the open menu; start it again, wait 4 s, screenshot again.
Expected: first "Not running" with "MouseTail isn't running." and Start MouseTail; then back to the full menu. `devshell.log` shows no repeating errors.

- [ ] **Step 8: Review focus: disabled with commands still running**

Run: `bash $SCRATCH/shell.sh "const i = Main.panel.statusArea['mousetail@galen.green']; i._command(['sleep', '2']); i._command(['true']); Main.extensionManager.disableExtension('mousetail@galen.green'); 'ok'"`, wait 3 s, then `…enableExtension('mousetail@galen.green')…`.
Expected: no `JS ERROR` in `devshell.log` after the sleep finishes; the icon comes back working.

- [ ] **Step 9: Commit**

```bash
git add integrations/gnome/mousetail@galen.green
git commit -m "GNOME extension: MouseTail in the top bar, with its settings"
```

---

### Task 3: Problems, the pairing code, computers and pairing

**Files:**
- Modify: `integrations/gnome/mousetail@galen.green/extension.js` (`_update`, `_opened`, `_setStatus`, `_stop`; new `_problemItems`, `_codeItems`, `_computerItems`, `_peerItems`, `_codeEntryItems`, `_startPairing`, `_sendCode`, `_cancelPairing`, `_fixFirewall`)
- Modify: `integrations/gnome/mousetail@galen.green/stylesheet.css`

**Interfaces:**
- Consumes: Task 2's helpers and `Indicator` members; `Status.shownPeers`, `Status.peerDetail`.
- Produces: `Indicator._pairing = {id, proc, error, said, typed, paired} | null`; `Indicator._codeEntry`.

- [ ] **Step 1: Fill the sections in `_update`**

In `_update()`, after `this._summary.text = …;`, add:

```js
        this._refill(this._problems,
            running && [st.capture_error, st.can_be_controlled, st.firewall, this._fixingFirewall],
            () => this._problemItems());
        this._refill(this._code, running && st.pairing_code, () => this._codeItems());
        const peers = running ? Status.shownPeers(st) : [];
        const pairing = this._pairing && [this._pairing.id, !!this._pairing.proc, this._pairing.error];
        this._refill(this._computers,
            running && [peers.map(p => [p.id, p.name, p.paired, p.connected, p.paused, Status.peerDetail(st, p)]), pairing],
            () => this._computerItems(peers));
        if (this._focusEntry) {
            // Just shown: focus it, so the code can just be typed.
            if (this.menu.isOpen)
                this._focusEntry.grab_key_focus();
            this._focusEntry = null;
        }
```

and change the `_arrange` line to use `peers`: `this._arrange.visible = running && peers.some(p => p.paired);`

- [ ] **Step 2: Problems and the code**

Add to `Indicator`:

```js
    // -------------------------------------------------------------- sections

    _problemItems() {
        const st = this._status;
        const items = [];
        if (st.capture_error)
            items.push(row(note(st.capture_error, 'mousetail-urgent')));
        if (st.can_be_controlled === false) {
            const script = `${HELPERS.replace(GLib.get_home_dir(), '~')}/enable-input.sh`;
            items.push(row(note(`Other computers can't control this one yet. Run this once: ${script}`)));
        }
        if (st.firewall) {
            items.push(row(note("This computer's firewall stops other computers reaching it, so connecting can be slow or fail.")));
            const fix = button('Fix Firewall…', () => this._fixFirewall());
            fix.reactive = !this._fixingFirewall;
            items.push(row(fix));
        }
        return items;
    }

    _codeItems() {
        const code = this._status.pairing_code;
        return [
            new PopupMenu.PopupSeparatorMenuItem('Pairing code'),
            row(new St.Label({text: code.code.split('').join(' '), style_class: 'mousetail-code'})),
            row(note(`Type this on ${code.name || 'your other computer'} to connect it.`)),
        ];
    }

    _computerItems(peers) {
        if (peers.length === 0)
            return [row(note('Looking for other computers on your network…'))];
        return peers.flatMap(p => this._peerItems(p));
    }

    _peerItems(p) {
        const pairing = this._pairing?.id === p.id ? this._pairing : null;
        const text = new St.BoxLayout({
            orientation: Clutter.Orientation.VERTICAL,
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        });
        const name = new St.Label({text: p.name, style_class: 'mousetail-peer'});
        if (!p.connected || p.paused)
            name.opacity = 140;
        text.add_child(name);
        const detail = new St.Label({text: Status.peerDetail(this._status, p), style_class: 'mousetail-peer-detail'});
        detail.opacity = 165;
        text.add_child(detail);

        const controls = [text];
        if (!p.paired && p.connected && !pairing?.proc)
            controls.push(button('Pair…', () => this._startPairing(p)));
        if (p.paired) {
            controls.push(iconButton(
                p.paused ? 'media-playback-start-symbolic' : 'media-playback-pause-symbolic',
                p.paused ? `Resume ${p.name}` : `Pause ${p.name} without forgetting it`,
                () => this._command([BINARY, p.paused ? 'resume' : 'pause', p.id])));
            controls.push(iconButton('window-close-symbolic', `Forget ${p.name}`,
                () => this._command([BINARY, 'unpair', p.id]), 'mousetail-forget'));
        }
        const items = [row(...controls)];
        if (pairing?.proc)
            items.push(...this._codeEntryItems(p, pairing));
        if (pairing?.error)
            items.push(row(note(pairing.error, 'mousetail-urgent')));
        return items;
    }

    /** Pairing: the other computer shows a code to type here. */
    _codeEntryItems(p, pairing) {
        const entry = new St.Entry({
            hint_text: 'Code',
            text: pairing.typed,
            can_focus: true,
            x_expand: true,
            style_class: 'mousetail-code-entry',
        });
        entry.clutter_text.max_length = 8;
        entry.clutter_text.connect('text-changed', () => (pairing.typed = entry.get_text()));
        entry.clutter_text.connect('activate', () => {
            this._sendCode(entry.get_text());
            entry.set_text('');
        });
        entry.clutter_text.connect('key-press-event', (_actor, event) => {
            if (event.get_key_symbol() !== Clutter.KEY_Escape)
                return Clutter.EVENT_PROPAGATE;
            this._cancelPairing();
            return Clutter.EVENT_STOP;
        });
        entry.connect('destroy', () => {
            if (this._codeEntry === entry)
                this._codeEntry = null;
        });
        this._codeEntry = entry;
        this._focusEntry = entry;
        return [
            row(note(`Type the code showing on ${p.name}:`)),
            row(entry, button('Cancel', () => this._cancelPairing())),
        ];
    }
```

- [ ] **Step 3: Pairing, and the firewall fix**

Add to `Indicator`:

```js
    // -------------------------------------------------------------- pairing

    /** `mousetail pair` asks the other computer to show a code, then reads it from stdin. A
     * wrong code ends it, so the row offers Pair… again, for a fresh code. */
    _startPairing(p) {
        this._cancelPairing(false);
        const pairing = {id: p.id, proc: null, error: '', said: '', typed: '', paired: false};
        this._pairing = pairing;
        try {
            pairing.proc = Gio.Subprocess.new([BINARY, 'pair', p.id],
                Gio.SubprocessFlags.STDIN_PIPE | Gio.SubprocessFlags.STDOUT_PIPE |
                Gio.SubprocessFlags.STDERR_PIPE);
        } catch (e) {
            pairing.error = e.message;
            this._update();
            return;
        }
        let waiting = 2; // its exit, and the end of what it says on stderr
        const finished = () => {
            if (--waiting > 0 || this._pairing !== pairing)
                return;
            if (pairing.paired) {
                this._pairing = null;
            } else {
                pairing.proc = null;
                pairing.error = said(pairing.said) || "That didn't work. Try again.";
            }
            this._update();
        };
        readLines(pairing.proc.get_stdout_pipe(), this._cancellable, line => {
            if (line.includes('Paired with')) {
                pairing.paired = true;
                this._message = line.slice(line.indexOf('Paired with')).trim();
            }
        });
        readLines(pairing.proc.get_stderr_pipe(), this._cancellable,
            line => (pairing.said += `${line}\n`), finished);
        pairing.proc.wait_async(this._cancellable, (proc, result) => {
            try {
                proc.wait_finish(result);
            } catch {
                return; // Cancelled: the extension is going away.
            }
            finished();
        });
        this._update();
    }

    _sendCode(code) {
        code = code.trim();
        const pairing = this._pairing;
        if (code === '' || !pairing?.proc)
            return;
        pairing.error = '';
        try {
            const stdin = pairing.proc.get_stdin_pipe();
            stdin.write_all(new TextEncoder().encode(`${code}\n`), null);
            stdin.flush(null);
        } catch (e) {
            pairing.error = e.message;
            this._update();
        }
    }

    _cancelPairing(update = true) {
        const pairing = this._pairing;
        this._pairing = null;
        pairing?.proc?.force_exit();
        if (update)
            this._update();
    }

    /** enable-firewall.sh asks for a password, so it runs in a terminal: the first there is. */
    _fixFirewall() {
        const script = `${GLib.shell_quote(`${HELPERS}/enable-firewall.sh`)}; read -rp 'Press Enter to close. '`;
        const terminal = 'command -v xdg-terminal-exec >/dev/null && exec xdg-terminal-exec bash -c "$0"; ' +
            'for t in ptyxis kgx gnome-terminal; do command -v "$t" >/dev/null && exec "$t" -- bash -c "$0"; done';
        this._fixingFirewall = true;
        this._update();
        run(['bash', '-c', terminal, script], this._cancellable, () => {
            this._fixingFirewall = false;
            this._update();
        });
    }
```

- [ ] **Step 4: Open on a new code; focus the field on opening; stop pairing on the way out**

Replace `_setStatus`:

```js
    _setStatus(status) {
        const hadCode = !!this._status.pairing_code;
        this._status = status;
        this._update();
        // A new pairing code is the one thing worth interrupting for.
        if (!hadCode && status.pairing_code && !this.menu.isOpen)
            this.menu.open();
    }
```

At the end of `_opened()` add `this._codeEntry?.grab_key_focus();`. In `_stop()` add
`this._pairing?.proc?.force_exit();` after cancelling.

In `stylesheet.css` add:

```css
.mousetail-code { font-size: 2.2em; font-weight: 600; }
.mousetail-peer { font-weight: 500; }
.mousetail-peer-detail { font-size: 0.85em; }
.mousetail-code-entry { min-width: 6em; }
.mousetail-forget:hover { color: #f66151; }
```

- [ ] **Step 5: Syntax check and look**

Run the Task 2 step 5 syntax check; restart the nested shell; open the menu and screenshot.
Expected: Computers lists omarchy, "Connected · its sound plays here", pause and forget buttons;
Arrange Displays… shows. Press pause from the shell (`…_computers` row's pause button, or
`i._command([BINARY, 'pause', '<id>'])`), screenshot: "Paused", name dimmed, play icon; resume it.

- [ ] **Step 6: Review focus: an update mid-typing**

Run in the nested shell: start pairing against a stand-in (`i._pairing = {id: '<omarchy id>', proc: Gio.Subprocess.new(['sleep', '60'], Gio.SubprocessFlags.STDIN_PIPE), error: '', said: '', typed: '', paired: false}; i._update();`), set the entry's text to `12`, then send a status that changes nothing about the computers (`i._setStatus({...i._status, version: '9.9.9'})`).
Expected: the entry still exists and still says `12` (`i._codeEntry.get_text() === '12'`). Then `i._cancelPairing()`.

- [ ] **Step 7: Review focus: a new code opens the menu once**

Run: close the menu; `i._setStatus({...i._status, pairing_code: {peer: 'x', name: 'mac', code: '4821'}})`; check `i.menu.isOpen`; close it; send the same status again; check it stays closed; clear the code.
Expected: `true`, then `false`; the screenshot shows "4 8 2 1" and "Type this on mac to connect it."

- [ ] **Step 8: Commit**

```bash
git add integrations/gnome/mousetail@galen.green
git commit -m "GNOME extension: computers, pairing, the pairing code and problems"
```

---

### Task 4: Arrange Displays

**Files:**
- Create: `integrations/gnome/mousetail@galen.green/arrange.js`
- Modify: `integrations/gnome/mousetail@galen.green/extension.js` (import; `_openArrange`; `_stop`)
- Modify: `integrations/gnome/mousetail@galen.green/stylesheet.css`

**Interfaces:**
- Consumes: `Status.placed`, `Status.offsetOf`, `Status.fit`, `Status.viewX`, `Status.viewY`, `Status.dropAt`, `Status.crossingBar`; `run` (passed in).
- Produces: `export const ArrangeDialog`, constructed as `new ArrangeDialog(binary, run)` where `run(argv, done(ok, out, err))`; a `ModalDialog` (`open()`, `close()`, destroyed on close).

- [ ] **Step 1: Write `arrange.js`**

```js
// Arrange Displays: drag the other computers to where they sit on your desk, like the Mac's
// Arrange Displays and the Omarchy panel's, in MouseTail's own colours (stylesheet.css) rather
// than the theme's. Drops snap to the nearest edge; glowing edges are where the cursor crosses.

import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Pango from 'gi://Pango';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as ModalDialog from 'resource:///org/gnome/shell/ui/modalDialog.js';

import * as Status from './status.js';

/** How long a dropped computer takes to glide to where it snapped. */
const GLIDE_MS = 220;
/** Room the title, help, legend and button take beside the canvas. */
const CHROME = {width: 48, height: 210};

export const ArrangeDialog = GObject.registerClass({GTypeName: 'MouseTailArrangeDialog'},
class ArrangeDialog extends ModalDialog.ModalDialog {
    /** `run(argv, done)` runs a command, then `done(ok, stdout, stderr)`. */
    _init(binary, run) {
        super._init({styleClass: 'mousetail-arrange'});
        this._binary = binary;
        this._run = run;
        this._layout = null;
        this._view = null;
        this._groups = new Map(); // machine id → its tiles
        this._drag = null;
        this._loading = false;
        this._closed = false;

        // About 70% of the screen, as on Omarchy, but no bigger than 900 × 620.
        const monitor = Main.layoutManager.currentMonitor;
        const width = Math.round(Math.min(monitor.width * 0.7, 900) - CHROME.width);
        const height = Math.round(Math.min(monitor.height * 0.7, 620) - CHROME.height);

        this.contentLayout.add_child(new St.Label({text: 'Arrange Displays', style_class: 'mousetail-arrange-title'}));
        const help = new St.Label({
            text: 'Drag each computer to where it sits on your desk. Push the cursor off a highlighted edge to move to the other computer.',
            style_class: 'mousetail-arrange-help',
        });
        help.clutter_text.line_wrap = true;
        this.contentLayout.add_child(help);

        this._canvas = new St.Widget({style_class: 'mousetail-arrange-canvas', width, height, clip_to_allocation: true});
        this._wait = new St.Label({text: 'Loading…', style_class: 'mousetail-arrange-help'});
        this._wait.add_constraint(new Clutter.AlignConstraint({source: this._canvas, align_axis: Clutter.AlignAxis.BOTH, factor: 0.5}));
        this._canvas.add_child(this._wait);
        this.contentLayout.add_child(this._canvas);

        const legend = new St.BoxLayout({style_class: 'mousetail-arrange-legend'});
        for (const [label, swatch] of [['This computer', 'this'], ['Other computers', 'other'], ['Cursor crosses here', 'crossing']]) {
            legend.add_child(new St.Widget({style_class: `mousetail-swatch mousetail-swatch-${swatch}`, y_align: Clutter.ActorAlign.CENTER}));
            legend.add_child(new St.Label({text: label, style_class: 'mousetail-arrange-legend-label', y_align: Clutter.ActorAlign.CENTER}));
        }
        this.contentLayout.add_child(legend);

        this.setButtons([{label: 'Done', action: () => this.close(), key: Clutter.KEY_Escape}]);

        // Keep up with the other computer being moved from there, or connecting.
        this._timer = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 2, () => {
            this._refresh();
            return GLib.SOURCE_CONTINUE;
        });
        this.connect('destroy', () => {
            this._closed = true;
            GLib.source_remove(this._timer);
            this._drag?.grab.dismiss();
        });
        this._refresh();
    }

    _refresh() {
        if (this._drag || this._loading)
            return;
        this._loading = true;
        this._run([this._binary, 'layout'], (_ok, out) => {
            this._loading = false;
            if (this._closed || this._drag)
                return;
            let layout = null;
            try {
                layout = JSON.parse(out);
            } catch {
                return;
            }
            if (layout?.machines)
                this._show(layout);
        });
    }

    /** Draw `layout`: machines kept from one load to the next, so a moved one glides. */
    _show(layout) {
        this._layout = layout;
        this._wait.visible = false;
        const shown = Status.placed(layout);
        const view = Status.fit(shown, this._canvas.width, this._canvas.height);
        this._view = view;

        const ids = new Set(shown.map(m => m.id));
        for (const [id, group] of this._groups) {
            if (!ids.has(id)) {
                group.destroy();
                this._groups.delete(id);
            }
        }
        for (const m of shown) {
            let group = this._groups.get(m.id);
            const fresh = !group;
            if (fresh) {
                group = new St.Widget();
                this._canvas.add_child(group);
                this._groups.set(m.id, group);
            }
            this._drawTiles(group, m, view);
            const o = Status.offsetOf(m);
            const x = Status.viewX(view, o.x);
            const y = Status.viewY(view, o.y);
            if (fresh)
                group.set_position(x, y);
            else
                group.ease({x, y, duration: GLIDE_MS, mode: Clutter.AnimationMode.EASE_OUT_CUBIC});
        }

        // Where the cursor crosses, above the tiles.
        this._crossings?.destroy();
        this._crossings = new St.Widget();
        for (const edge of layout.crossings ?? []) {
            const bar = Status.crossingBar(view, edge);
            this._crossings.add_child(new St.Widget({style_class: 'mousetail-crossing', ...bar}));
        }
        this._canvas.add_child(this._crossings);
    }

    _drawTiles(group, m, view) {
        group.destroy_all_children();
        const kind = m.this ? 'this' : m.connected && !m.paused ? 'other' : 'offline';
        for (const d of m.displays) {
            const width = d.rect.w * view.scale;
            const height = d.rect.h * view.scale;
            const tile = new St.Widget({
                style_class: `mousetail-tile mousetail-tile-${kind}`,
                layout_manager: new Clutter.BinLayout(),
                x: d.rect.x * view.scale,
                y: d.rect.y * view.scale,
                width,
                height,
                reactive: !m.this,
            });
            // A strip marks each computer's main display, as macOS does.
            if (d.primary)
                tile.add_child(new St.Widget({style_class: 'mousetail-tile-main', y_align: Clutter.ActorAlign.START}));
            const labels = new St.BoxLayout({
                orientation: Clutter.Orientation.VERTICAL,
                x_align: Clutter.ActorAlign.CENTER,
                y_align: Clutter.ActorAlign.CENTER,
            });
            for (const [text, styleClass] of [
                [m.this ? 'This computer' : m.name, 'mousetail-tile-name'],
                [m.connected || m.this ? d.name : 'Offline', 'mousetail-tile-display'],
            ]) {
                const label = new St.Label({text, style_class: styleClass, width: Math.max(0, width - 12)});
                label.clutter_text.ellipsize = Pango.EllipsizeMode.END;
                labels.add_child(label);
            }
            tile.add_child(labels);
            if (!m.this)
                this._draggable(tile, group, m);
            group.add_child(tile);
        }
    }

    _draggable(tile, group, m) {
        tile.connect('button-press-event', (_actor, event) => {
            if (event.get_button() !== Clutter.BUTTON_PRIMARY || this._drag)
                return Clutter.EVENT_PROPAGATE;
            const [x, y] = event.get_coords();
            group.remove_all_transitions();
            this._drag = {m, group, x, y, dx: 0, dy: 0, moved: false, view: this._view,
                from: {x: group.x, y: group.y}, grab: global.stage.grab(tile)};
            return Clutter.EVENT_STOP;
        });
        tile.connect('motion-event', (_actor, event) => {
            const drag = this._drag;
            if (drag?.group !== group)
                return Clutter.EVENT_PROPAGATE;
            const [x, y] = event.get_coords();
            drag.dx = x - drag.x;
            drag.dy = y - drag.y;
            if (!drag.moved) {
                if (Math.abs(drag.dx) + Math.abs(drag.dy) < 3)
                    return Clutter.EVENT_STOP;
                // Picked up: it glows, like the logo's tail, above the rest.
                drag.moved = true;
                this._canvas.set_child_above_sibling(group, null);
                group.get_children().forEach(t => t.add_style_class_name('mousetail-tile-lifted'));
                this._crossings?.hide();
            }
            group.set_position(drag.from.x + drag.dx, drag.from.y + drag.dy);
            return Clutter.EVENT_STOP;
        });
        tile.connect('button-release-event', (_actor, event) => {
            const drag = this._drag;
            if (drag?.group !== group || event.get_button() !== Clutter.BUTTON_PRIMARY)
                return Clutter.EVENT_PROPAGATE;
            drag.grab.dismiss();
            if (drag.moved)
                this._drop(drag);
            else
                this._drag = null;
            return Clutter.EVENT_STOP;
        });
    }

    /** Ask MouseTail to put it there; it answers with where it snapped, and the tile glides
     * there. */
    _drop(drag) {
        const at = Status.dropAt(drag.m, drag.view, drag.dx, drag.dy);
        this._run([this._binary, 'place-at', drag.m.id, String(at.x), String(at.y)], (_ok, out) => {
            if (this._closed)
                return;
            let offset = null;
            try {
                offset = JSON.parse(out).offset ?? null;
            } catch {
                // Back where it was.
            }
            this._drag = null;
            const machines = this._layout.machines.map(m => (offset && m.id === drag.m.id ? {...m, offset} : m));
            this._show({machines, crossings: []});
            this._refresh();
        });
    }
});
```

- [ ] **Step 2: Open it from the menu**

In `extension.js`, add `import {ArrangeDialog} from './arrange.js';` after the `Status` import,
and replace `_openArrange`:

```js
    _openArrange() {
        this._arrangeDialog?.close();
        const dialog = new ArrangeDialog(BINARY, (argv, done) => run(argv, this._cancellable, done));
        dialog.connect('destroy', () => {
            if (this._arrangeDialog === dialog)
                this._arrangeDialog = null;
        });
        this._arrangeDialog = dialog;
        dialog.open();
    }
```

In `_stop()` add `this._arrangeDialog?.close();`.

- [ ] **Step 3: Its styles**

Append to `stylesheet.css`:

```css
/* Arrange Displays, in MouseTail's own colours. */
.modal-dialog.mousetail-arrange {
  background-color: #080808;
  border: 1px solid rgba(242, 241, 236, 0.09);
  color: #f2f1ec;
}
.modal-dialog.mousetail-arrange .modal-dialog-content-box { max-width: 900px; spacing: 8px; }
.mousetail-arrange-title { font-size: 1.4em; font-weight: 600; color: #f2f1ec; }
.mousetail-arrange-help { font-size: 0.9em; color: rgba(242, 241, 236, 0.64); }
.mousetail-arrange-canvas {
  background-color: #111212;
  border: 1px solid rgba(242, 241, 236, 0.09);
  border-radius: 12px;
}
.mousetail-tile {
  border-radius: 5px;
  border: 1px solid rgba(255, 255, 255, 0.12);
  background-gradient-direction: vertical;
}
.mousetail-tile-this { background-gradient-start: #383939; background-gradient-end: #292a2a; color: rgba(242, 241, 236, 0.64); }
.mousetail-tile-other { background-gradient-start: #4a4535; background-gradient-end: #353226; border-color: rgba(255, 255, 255, 0.18); color: #f2f1ec; }
.mousetail-tile-offline { background-gradient-start: #232424; background-gradient-end: #1b1c1c; border-color: rgba(255, 255, 255, 0.08); color: rgba(242, 241, 236, 0.42); }
.mousetail-tile-lifted { border: 2px solid #ffeba7; box-shadow: 0 0 6px 3px rgba(255, 224, 110, 0.4); }
.mousetail-tile-main { height: 4px; margin: 1px; border-radius: 4px; background-color: rgba(242, 241, 236, 0.45); }
.mousetail-tile-name { font-size: 0.85em; font-weight: 600; text-align: center; }
.mousetail-tile-display { font-size: 0.75em; text-align: center; }
.mousetail-crossing { background-color: #ffeba7; border-radius: 2px; box-shadow: 0 0 6px 2px rgba(255, 224, 110, 0.4); }
.mousetail-arrange-legend { spacing: 6px; }
.mousetail-arrange-legend-label { font-size: 0.8em; color: rgba(242, 241, 236, 0.64); margin-right: 12px; }
.mousetail-swatch { width: 14px; height: 10px; border-radius: 2px; border: 1px solid rgba(255, 255, 255, 0.15); }
.mousetail-swatch-this { background-color: #383939; }
.mousetail-swatch-other { background-color: #4a4535; }
.mousetail-swatch-crossing { height: 3px; border-width: 0; background-color: #ffeba7; }
.modal-dialog.mousetail-arrange .modal-dialog-button { background-color: #ffeba7; color: #080808; }
.modal-dialog.mousetail-arrange .modal-dialog-button:hover { background-color: #fff2c4; }
```

- [ ] **Step 4: Look at it**

Run the syntax check; restart the nested shell; `bash $SCRATCH/shell.sh "Main.panel.statusArea['mousetail@galen.green']._openArrange(); 'ok'"`; wait 2 s; screenshot.
Expected: the dimmed screen, the dark card, "Arrange Displays" and its help, this computer's two
displays in graphite with "This computer" and the display names, omarchy's in warm, a strip on
each main display, a glowing crossing line where they meet, the legend, and Done.

- [ ] **Step 5: Drag it**

Drive a drag from the shell, moving omarchy's tile 80 px down and back (emit the press/motion/release through `ArrangeDialog._drop` directly: `d._drop({m: d._layout.machines.find(m => !m.this), view: d._view, dx: 0, dy: 80})`), screenshot after 1 s, then `mousetail layout`.
Expected: the tile glides to where it snapped; `mousetail layout` shows omarchy's new offset;
then put it back with `mousetail place-at omarchy -80 461` and check the tile glides back
within 2 s (the refresh). Finally, by hand in the nested window: drag the tile with the mouse;
it lifts with a glow, follows, and snaps on release.

- [ ] **Step 6: Review focus: closed with a command running**

Run: `d._run(['sleep', '2'], () => d._show(d._layout)); d.close();`, wait 3 s.
Expected: no `JS ERROR` in `devshell.log` (the callback returns on `_closed` … the `layout`
refresh too). Also Escape closes the dialog.

- [ ] **Step 7: Commit**

```bash
git add integrations/gnome/mousetail@galen.green
git commit -m "GNOME extension: Arrange Displays"
```

---

### Task 5: Package, install, update and uninstall it

**Files:**
- Modify: `scripts/package-linux.sh`
- Modify: `scripts/install-linux.sh`
- Modify: `scripts/uninstall-linux.sh`
- Modify: `crates/mousetail/src/update.rs`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: the extension directory (Tasks 2–4).
- Produces: release tarball folder `gnome-extension/mousetail@galen.green`; installed copy at `${XDG_DATA_HOME:-~/.local/share}/gnome-shell/extensions/mousetail@galen.green`.

- [ ] **Step 1: Package it**

In `scripts/package-linux.sh`, change the `mkdir` line and add after the Omarchy plugin's lines:

```bash
rm -rf "$out" && mkdir -p "$out/omarchy-plugin" "$out/gnome-extension"
```

```bash
cp -r integrations/gnome/mousetail@galen.green "$out/gnome-extension/"
sed -i.bak "s/\"version-name\": \"[^\"]*\"/\"version-name\": \"$version\"/" "$out/gnome-extension/mousetail@galen.green/metadata.json"
rm "$out/gnome-extension/mousetail@galen.green/metadata.json.bak"
```

(the `cp` goes before the version line, the `sed` after `version=…`). Update its header comment:
"…install/uninstall scripts, the Omarchy bar plugin and the GNOME extension…".

- [ ] **Step 2: Install it on GNOME**

In `scripts/install-linux.sh`: after `omarchy=$config_home/omarchy` add

```bash
gnome_uuid=mousetail@galen.green
gnome_extensions=${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions
```

in the release branch add `gnome_src=$here/gnome-extension/$gnome_uuid`, in the source branch
`gnome_src=$repo/integrations/gnome/$gnome_uuid`; after `say() {…}` add

```bash
# GNOME: the desktop says so, or (run from SSH or a TTY) its shell is running for this user.
on_gnome() {
  [[ ${XDG_CURRENT_DESKTOP:-} == *GNOME* ]] || pgrep -u "$(id -u)" -x gnome-shell >/dev/null 2>&1
}
```

and after the Omarchy bar block:

```bash
if on_gnome && [[ -d $gnome_src ]]; then
  say "Adding MouseTail to GNOME's top bar"
  rm -rf "${gnome_extensions:?}/$gnome_uuid"
  mkdir -p "$gnome_extensions"
  cp -r "$gnome_src" "$gnome_extensions/$gnome_uuid"
  # GNOME only finds a new extension when you log in (on Wayland it can't reload), so if it
  # won't switch this one on now, put it on the list for the next login.
  if ! gnome-extensions enable "$gnome_uuid" 2>/dev/null; then
    gjs -c "
      const {Gio} = imports.gi;
      const shell = new Gio.Settings({schema_id: 'org.gnome.shell'});
      const on = shell.get_strv('enabled-extensions');
      if (!on.includes('$gnome_uuid'))
        shell.set_strv('enabled-extensions', [...on, '$gnome_uuid']);
      shell.set_strv('disabled-extensions', shell.get_strv('disabled-extensions').filter(u => u !== '$gnome_uuid'));
      Gio.Settings.sync();" 2>/dev/null || true
    echo "    It shows in the top bar after you next log in."
  fi
  if [[ $(gsettings get org.gnome.shell disable-user-extensions 2>/dev/null) == true ]]; then
    echo "    GNOME's extensions are switched off: turn them on in the Extensions app to see it."
  fi
fi
```

Update the header comment ("…and on Omarchy or GNOME adds a status icon to the bar.").

- [ ] **Step 3: Uninstall it**

In `scripts/uninstall-linux.sh`, after the Omarchy block:

```bash
gnome_uuid=mousetail@galen.green
gnome_extension=${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/$gnome_uuid
if [[ -d $gnome_extension ]]; then
  gnome-extensions disable "$gnome_uuid" 2>/dev/null || gjs -c "
    const {Gio} = imports.gi;
    const shell = new Gio.Settings({schema_id: 'org.gnome.shell'});
    shell.set_strv('enabled-extensions', shell.get_strv('enabled-extensions').filter(u => u !== '$gnome_uuid'));
    Gio.Settings.sync();" 2>/dev/null
  rm -rf "$gnome_extension"
fi
```

- [ ] **Step 4: Update it**

In `crates/mousetail/src/update.rs`: add `const GNOME_EXTENSION: &str = "mousetail@galen.green";`
beside `PLUGIN_ID`, and in `install()` after the Omarchy plugin's `if`:

```rust
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        });
    let extension = data.join("gnome-shell/extensions").join(GNOME_EXTENSION);
    let new_extension = staged.dir.join("gnome-extension").join(GNOME_EXTENSION);
    if extension.is_dir() && new_extension.is_dir() {
        replace_dir(&new_extension, &extension).context("updating the GNOME extension")?;
    }
```

Update `install()`'s doc comment and the module doc to name the GNOME extension too.

Run: `podman run --rm --security-opt label=disable -v "$PWD":/src -v mousetail-cargo:/cargo localhost/mousetail-build:44 sh -c 'cargo fmt --all --check && cargo clippy --locked -p mousetail --tests -- -D warnings && cargo test --locked -p mousetail'`
Expected: clean, all tests pass.

- [ ] **Step 5: CI runs the extension's test**

In `.github/workflows/ci.yml`, linux job, after `cargo test --locked -p mousetail-core`:

```yaml
      - run: node --test integrations/gnome/test/
```

- [ ] **Step 6: End to end on this machine**

Stop any dev daemon; build a release folder in the container
(`podman run --rm --security-opt label=disable -v "$PWD":/src -v mousetail-cargo:/cargo localhost/mousetail-build:44 scripts/package-linux.sh`); run
`dist/mousetail-linux-x86_64/install.sh` on the host.
Expected: "Adding MouseTail to GNOME's top bar" and "It shows in the top bar after you next log
in."; `~/.local/share/gnome-shell/extensions/mousetail@galen.green/metadata.json` has
`"version-name": "0.2.7"`; `gsettings get org.gnome.shell enabled-extensions` includes it;
`systemctl --user is-active mousetail` is `active`; no Omarchy plugin installed.
Then run `~/.local/share/mousetail/uninstall.sh`: the extension directory and its
`enabled-extensions` entry are gone. Then install again (so the user keeps MouseTail).

- [ ] **Step 7: Commit**

```bash
git add scripts/package-linux.sh scripts/install-linux.sh scripts/uninstall-linux.sh crates/mousetail/src/update.rs .github/workflows/ci.yml
git commit -m "Linux: install, update and remove the GNOME extension, like the Omarchy plugin"
```

---

### Task 6: Docs

**Files:**
- Modify: `README.md`, `website/index.html`, `docs/DESIGN.md`

- [ ] **Step 1: README**

- Install paragraph: "…and, on Omarchy, adds an icon to the bar." → "…and adds an icon to the bar on Omarchy, or to the top bar on GNOME (from your next login)."
- Pair paragraph: "**Arrange Displays…** on a Mac or in the Omarchy bar" → "**Arrange Displays…** on a Mac, in the Omarchy bar or in GNOME's top bar".
- Project layout table: add `| \`integrations/gnome\` | GNOME Shell extension (and its test) |` under the Omarchy row.

- [ ] **Step 2: Website table (GNOME column)**

"Pause a computer without forgetting it": `Command line` → `Top bar`; "Status at a glance":
`Command line` → `Top bar`; "Arrange your computers": `Command line` → `Drag to arrange`.

- [ ] **Step 3: DESIGN.md**

In the Linux daemon paragraph, after the Omarchy bar plugin, add: "On GNOME, a Shell extension
(`integrations/gnome`) does the same from the top bar."

- [ ] **Step 4: Commit**

```bash
git add README.md website/index.html docs/DESIGN.md
git commit -m "Docs: the GNOME extension"
```
