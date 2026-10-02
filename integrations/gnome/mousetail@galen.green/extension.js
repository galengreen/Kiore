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
import {ArrangeDialog} from './arrange.js';

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
 * it, or a button in it, leaves the menu open. Reactive, though, as GNOME greys out rows that
 * aren't; and not highlighted on hover. */
function row(...children) {
    const item = new PopupMenu.PopupBaseMenuItem({activate: false, hover: false, can_focus: false});
    item.track_hover = false;
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
        // Escape in the code field stops pairing, as on Omarchy. The panel's menu manager
        // takes Escape before the field sees it, to close the menu, so this goes first:
        // connected before the menu joins the panel.
        this.menu.actor.connect('captured-event', (_actor, event) => {
            if (event.type() !== Clutter.EventType.KEY_PRESS ||
                event.get_key_symbol() !== Clutter.KEY_Escape ||
                !this._codeEntry?.clutter_text.has_key_focus())
                return Clutter.EVENT_PROPAGATE;
            this._cancelPairing();
            return Clutter.EVENT_STOP;
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

        this._computersHeading.visible = running;
        this._arrange.visible = running && peers.some(p => p.paired);
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
        this._codeEntry?.grab_key_focus();
    }

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
        const hadCode = !!this._status.pairing_code;
        this._status = status;
        this._update();
        // A new pairing code is the one thing worth interrupting for.
        if (!hadCode && status.pairing_code && !this.menu.isOpen)
            this.menu.open();
    }

    _stop() {
        this._cancellable.cancel();
        this._pairing?.proc?.force_exit();
        this._watcher?.force_exit();
        this._arrangeDialog?.close();
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
