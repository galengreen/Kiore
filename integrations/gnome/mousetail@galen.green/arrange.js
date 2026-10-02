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
                tile.add_child(new St.Widget({
                    style_class: 'mousetail-tile-main',
                    x_expand: true,
                    y_expand: true,
                    y_align: Clutter.ActorAlign.START,
                }));
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

    // Through the `event` signal: GNOME 50 no longer sends a tile `button-press-event` or
    // `button-release-event`.
    _draggable(tile, group, m) {
        tile.connect('event', (_actor, event) => {
            switch (event.type()) {
            case Clutter.EventType.BUTTON_PRESS:
                return this._press(event, tile, group, m);
            case Clutter.EventType.MOTION:
                return this._move(event, group);
            case Clutter.EventType.BUTTON_RELEASE:
                return this._release(event, group);
            default:
                return Clutter.EVENT_PROPAGATE;
            }
        });
    }

    _press(event, tile, group, m) {
        if (event.get_button() !== Clutter.BUTTON_PRIMARY || this._drag)
            return Clutter.EVENT_PROPAGATE;
        const [x, y] = event.get_coords();
        group.remove_all_transitions();
        this._drag = {m, group, x, y, dx: 0, dy: 0, moved: false, view: this._view,
            from: {x: group.x, y: group.y}, grab: global.stage.grab(tile)};
        return Clutter.EVENT_STOP;
    }

    _move(event, group) {
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
    }

    _release(event, group) {
        const drag = this._drag;
        if (drag?.group !== group || event.get_button() !== Clutter.BUTTON_PRIMARY)
            return Clutter.EVENT_PROPAGATE;
        drag.grab.dismiss();
        if (drag.moved)
            this._drop(drag);
        else
            this._drag = null;
        return Clutter.EVENT_STOP;
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
