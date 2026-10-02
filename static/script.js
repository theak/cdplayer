// CD Player web remote: polls /api/status once a second and posts transport actions.

const $ = (id) => document.getElementById(id);
const fmt = (s) => {
    s = Math.max(0, Math.floor(s || 0));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
};

const DRIVE_LABELS = {
    missing: 'No drive connected',
    empty: 'No disc',
    data: 'No MP3 or FLAC files on this disc',
};

// A failed action's message, shown briefly over the status's own error.
let flash = { text: '', until: 0 };

// Rebuild the track list only when the disc/album info changes, not every poll.
let tracklistKey = '';

function renderTracklist(s) {
    const key = JSON.stringify([s.tracklist, s.album]);
    if (key !== tracklistKey) {
        tracklistKey = key;
        const list = $('tracklist');
        list.replaceChildren(...s.tracklist.map((t, i) => {
            const li = document.createElement('li');
            const num = Object.assign(document.createElement('span'), { className: 'num', textContent: i + 1 });
            const name = Object.assign(document.createElement('span'), { className: 'name', textContent: t.title || `Track ${i + 1}` });
            // Show per-track artists only where they differ from the album's (compilations, features).
            if (t.artist && s.album && t.artist !== s.album.artist) {
                name.append(' ', Object.assign(document.createElement('span'), { className: 'who', textContent: t.artist }));
            }
            const dur = Object.assign(document.createElement('span'), { className: 'dur', textContent: t.length == null ? '' : fmt(t.length) });
            li.append(num, name, dur);
            li.addEventListener('click', () => post(`/api/track/${i + 1}`));
            return li;
        }));
    }
    $('tracklist-card').hidden = !s.tracklist.length;
    [...$('tracklist').children].forEach((li, i) => li.classList.toggle('current', s.track === i + 1));
}

function renderArt(s) {
    const cover = s.album && s.album.cover;
    const img = $('art');
    if (cover && img.dataset.src !== cover) {
        img.dataset.src = cover;
        img.src = cover;
    }
    img.hidden = !cover || img.dataset.failed === cover;
    $('art-placeholder').hidden = !img.hidden;
}

function render(s) {
    const active = s.state === 'playing' || s.state === 'paused';
    const info = s.track ? s.tracklist[s.track - 1] : null;
    const position = s.track ? `Track ${s.track} of ${s.tracks}` : '';

    $('status').textContent =
        s.state === 'playing' ? (info && info.title ? `Playing · ${position}` : 'Playing')
        : s.state === 'paused' ? (info && info.title ? `Paused · ${position}` : 'Paused')
        : s.state === 'stopped' ? `Stopped · ${s.tracks} tracks`
        : DRIVE_LABELS[s.drive];
    $('track').textContent =
        info && info.title ? info.title
        : s.track ? position
        : active ? 'Starting…'
        : s.album ? s.album.title
        : s.drive === 'audio' ? 'Audio CD'
        : s.state === 'stopped' ? 'Data disc'
        : '—';
    $('subtitle').textContent = s.album
        ? (s.track ? `${(info && info.artist) || s.album.artist} — ${s.album.title}` : s.album.artist)
        : '';
    document.title = info && info.title ? `${info.title} · CD Player` : 'CD Player';

    $('elapsed').textContent = fmt(s.elapsed);
    // Some MP3s don't record their length; show nothing rather than a wrong 0:00.
    $('length').textContent = s.length == null && s.track ? '' : fmt(s.length);
    $('bar').style.width = s.length ? `${Math.min(100, (s.elapsed / s.length) * 100)}%` : '0';
    // Seeking needs the track's length to turn a click into a time.
    seekLength = active && s.length ? s.length : 0;
    $('seek').classList.toggle('enabled', seekLength > 0);

    const playing = s.state === 'playing';
    $('playpause').classList.toggle('playing', playing);
    $('playpause').setAttribute('aria-label', playing ? 'Pause' : 'Play');

    const enabled = {
        playpause: active || s.state === 'stopped',
        prev: active,
        next: active,
        stop: active,
        eject: s.drive === 'audio' || s.drive === 'data',
    };
    for (const btn of document.querySelectorAll('[data-action]')) {
        btn.disabled = !enabled[btn.dataset.action];
    }

    // Don't move the slider out from under someone dragging it.
    if (Date.now() > volumeHeldUntil) $('volume').value = s.volume;

    renderArt(s);
    renderTracklist(s);

    const err = Date.now() < flash.until ? flash.text : s.error;
    $('error').hidden = !err;
    $('error').textContent = err || '';
}

async function refresh() {
    try {
        const res = await fetch('/api/status', { cache: 'no-store' });
        render(await res.json());
    } catch (_) {
        $('status').textContent = "Can't reach the player";
    }
}

async function post(path) {
    const res = await fetch(path, { method: 'POST' });
    if (!res.ok) {
        const data = await res.json().catch(() => ({}));
        flash = { text: data.error || 'Something went wrong', until: Date.now() + 4000 };
    }
    refresh();
}

const control = (action) => post(`/api/control/${action}`);

// ---- seeking ----

let seekLength = 0;

$('seek').addEventListener('click', async (e) => {
    if (!seekLength) return;
    const rect = e.currentTarget.getBoundingClientRect();
    const fraction = Math.min(Math.max((e.clientX - rect.left) / rect.width, 0), 0.999);
    const res = await fetch('/api/seek', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ seconds: fraction * seekLength }),
    });
    if (!res.ok) {
        const data = await res.json().catch(() => ({}));
        flash = { text: data.error || 'Something went wrong', until: Date.now() + 4000 };
    }
    refresh();
});

// ---- volume ----

// Send the slider's value as it moves: one request at a time, always the latest value.
let volumeHeldUntil = 0;
let volumeSending = false;
let volumePending = null;

async function sendVolume(volume) {
    volumePending = volume;
    if (volumeSending) return;
    volumeSending = true;
    while (volumePending !== null) {
        const body = JSON.stringify({ volume: volumePending });
        volumePending = null;
        await fetch('/api/volume', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body,
        }).catch(() => {});
    }
    volumeSending = false;
}

$('volume').addEventListener('input', (e) => {
    volumeHeldUntil = Date.now() + 2000;
    sendVolume(Number(e.target.value));
});

// ---- settings ----

const form = $('settings');

async function loadSettings() {
    const cfg = await (await fetch('/api/config')).json();
    form.start_webhook.value = cfg.start_webhook;
    form.stop_webhook.value = cfg.stop_webhook;
    form.eject_when_finished.checked = cfg.eject_when_finished;
}

function setHint(el, text, cls) {
    el.textContent = text;
    el.className = `hint ${cls || ''}`;
}

async function saveSettings(e) {
    e.preventDefault();
    const res = await fetch('/api/config', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            start_webhook: form.start_webhook.value,
            stop_webhook: form.stop_webhook.value,
            eject_when_finished: form.eject_when_finished.checked,
        }),
    });
    const data = await res.json().catch(() => ({}));
    setHint($('saved'), res.ok ? 'Saved' : data.error || 'Save failed', res.ok ? 'ok' : 'bad');
}

async function testWebhook(event) {
    const hint = $(`${event}_webhook_result`);
    setHint(hint, 'Sending…');
    const res = await fetch('/api/webhook-test', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ event, url: form[`${event}_webhook`].value }),
    });
    const data = await res.json().catch(() => ({}));
    setHint(hint, res.ok ? 'Sent' : data.error || 'Failed', res.ok ? 'ok' : 'bad');
}

// ---- wiring ----

// A broken cover URL falls back to the disc drawing.
$('art').addEventListener('error', () => {
    $('art').dataset.failed = $('art').dataset.src;
    $('art').hidden = true;
    $('art-placeholder').hidden = false;
});

for (const btn of document.querySelectorAll('[data-action]')) {
    btn.addEventListener('click', () => control(btn.dataset.action));
}
for (const btn of document.querySelectorAll('[data-test]')) {
    btn.addEventListener('click', () => testWebhook(btn.dataset.test));
}
form.addEventListener('submit', saveSettings);

refresh();
loadSettings();
setInterval(() => { if (document.visibilityState === 'visible') refresh(); }, 1000);
document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') refresh();
});
