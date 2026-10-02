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
