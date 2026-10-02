// CD Player web remote: polls /api/status once a second and posts transport actions.

const $ = (id) => document.getElementById(id);
const fmt = (s) => {
    s = Math.max(0, Math.floor(s || 0));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
};

const DRIVE_LABELS = {
    missing: 'No drive connected',
    empty: 'No disc',
    data: 'Not an audio CD',
};

// A failed action's message, shown briefly over the status's own error.
let flash = { text: '', until: 0 };

function render(s) {
    const active = s.state === 'playing' || s.state === 'paused';

    $('status').textContent =
        s.state === 'playing' ? 'Playing'
        : s.state === 'paused' ? 'Paused'
        : s.drive === 'audio' ? `Stopped · ${s.tracks} tracks`
        : DRIVE_LABELS[s.drive];
    $('track').textContent =
        s.track ? `Track ${s.track} of ${s.tracks}`
        : active ? 'Starting…'
        : s.drive === 'audio' ? 'Audio CD'
        : '—';

    $('elapsed').textContent = fmt(s.elapsed);
    $('length').textContent = fmt(s.length);
    $('bar').style.width = s.length ? `${Math.min(100, (s.elapsed / s.length) * 100)}%` : '0';

    $('icon-play').hidden = s.state === 'playing';
    $('icon-pause').hidden = s.state !== 'playing';

    const enabled = {
        playpause: active || s.drive === 'audio',
        prev: active,
        next: active,
        stop: active,
        eject: s.drive === 'audio' || s.drive === 'data',
    };
    for (const btn of document.querySelectorAll('[data-action]')) {
        btn.disabled = !enabled[btn.dataset.action];
    }

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

async function control(action) {
    const res = await fetch(`/api/control/${action}`, { method: 'POST' });
    if (!res.ok) {
        const data = await res.json().catch(() => ({}));
        flash = { text: data.error || 'Something went wrong', until: Date.now() + 4000 };
    }
    refresh();
}

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
