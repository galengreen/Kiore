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
