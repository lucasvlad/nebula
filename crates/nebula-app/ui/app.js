// Nebula desktop UI. Talks to the Rust side only through the commands and events in
// docs/ui-contract.md. Opened in a plain browser, it falls back to mock.js.

const api = window.__TAURI__ ?? (await import('./mock.js'));
const { invoke } = api.core;
const { listen } = api.event;
const pickFolder = api.dialog.open;

const FEED_LIMIT = 200;
const $ = (id) => document.getElementById(id);

// ---------- Small helpers ----------

/** Build an element. Text is always set with textContent, never parsed as HTML,
 *  because file and device names come from another machine. */
function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat()) if (c != null && c !== false) el.append(c instanceof Node ? c : String(c));
  return el;
}

function formatBytes(n) {
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${i === 0 ? n : n.toFixed(n < 100 ? 1 : 0)} ${units[i]}`;
}

function timeAgo(at) {
  const s = Math.max(0, Math.round((Date.now() - at) / 1000));
  if (s < 10) return 'just now';
  if (s < 60) return `${s} s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  const hrs = Math.round(m / 60);
  return hrs < 24 ? `${hrs} h ago` : new Date(at).toLocaleDateString();
}

const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;
const shortPath = (p) => p.replace(/^\/(Users|home)\/[^/]+/, '~');
const baseName = (p) => p.split(/[\\/]/).filter(Boolean).pop() ?? p;
const hostOf = (addr) => (addr ?? '').replace(/:\d+$/, '').replace(/^\[|\]$/g, '');
const listNames = (names) =>
  names.length <= 1 ? names.join('') : `${names.slice(0, -1).join(', ')} and ${names.at(-1)}`;

// Inline SVG icons (constants only; safe to use as innerHTML).
const ICONS = {
  arrive: '<path d="M12 4v11m0 0l-4.5-4.5M12 15l4.5-4.5M5 19.5h14"/>',
  send: '<path d="M12 19V8m0 0L7.5 12.5M12 8l4.5 4.5M5 4.5h14"/>',
  conflict: '<path d="M9.5 3.5h5.5l3.5 3.5v9.5a1.5 1.5 0 0 1-1.5 1.5h-7.5A1.5 1.5 0 0 1 8 16.5V5a1.5 1.5 0 0 1 1.5-1.5z"/><path d="M5 7.5v11A1.5 1.5 0 0 0 6.5 20H14"/>',
  trash: '<path d="M5 7h14M10 7V4.5h4V7M7 7l1 12.5h8L17 7"/>',
  link: '<path d="M10 14l4-4M8.5 11.5L6.8 13.2a3 3 0 0 0 4.2 4.2l1.7-1.7M15.5 12.5l1.7-1.7a3 3 0 0 0-4.2-4.2l-1.7 1.7"/>',
  unlink: '<path d="M8.5 11.5L6.8 13.2a3 3 0 0 0 4.2 4.2l1.7-1.7M15.5 12.5l1.7-1.7a3 3 0 0 0-4.2-4.2l-1.7 1.7M4 4l16 16"/>',
  shield: '<path d="M12 3.5l7 2.8v5.2c0 4.3-3 7.6-7 9-4-1.4-7-4.7-7-9V6.3z"/><path d="M9.5 9.5l5 5M14.5 9.5l-5 5"/>',
  check: '<path d="M12 3.5l7 2.8v5.2c0 4.3-3 7.6-7 9-4-1.4-7-4.7-7-9V6.3z"/><path d="M8.8 12.2l2.3 2.3 4.3-4.6"/>',
  folder: '<path d="M3.5 6.5a1.5 1.5 0 0 1 1.5-1.5h4.2l2 2.2H19a1.5 1.5 0 0 1 1.5 1.5v9.3a1.5 1.5 0 0 1-1.5 1.5H5a1.5 1.5 0 0 1-1.5-1.5z"/>',
  pairFail: '<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5v5.5M12 16.2v.1"/>',
};
const icon = (name, cls = 'icon') => {
  const s = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  s.setAttribute('viewBox', '0 0 24 24');
  s.setAttribute('class', cls);
  s.setAttribute('aria-hidden', 'true');
  s.innerHTML = ICONS[name];
  return s;
};

const words = (code) =>
  h('ul', { class: 'words', 'aria-label': 'Security words' },
    (code ?? '').split(/\s+/).filter(Boolean).map((w) => h('li', {}, w.toLowerCase())));

// ---------- Errors ----------

function toast(message) {
  const t = h('div', { class: 'toast' },
    h('span', {}, String(message)),
    h('button', { class: 'icon-btn', type: 'button', 'aria-label': 'Dismiss', onclick: () => t.remove() }, '×'));
  $('toasts').append(t);
  setTimeout(() => t.remove(), 7000);
}

/** Run a command; show its plain-English error in a toast and return undefined on failure. */
async function run(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (err) {
    toast(err);
    return undefined;
  }
}

// ---------- Status: header, devices, folders ----------

let status = { me: { name: '', id: '', port: 0, addresses: [] }, peers: [], folders: [] };

async function refresh() {
  try {
    status = await invoke('status');
  } catch (err) {
    toast(err);
    return;
  }
  renderHeader();
  renderPeers();
  renderFolders();
}

function renderHeader() {
  const { me, peers } = status;
  if (!renaming) $('me-name-text').textContent = me.name;
  $('me-sub').textContent = `This device, ID ${me.id}`;

  const online = peers.filter((p) => p.online);
  const link = $('link');
  link.dataset.state = online.length ? 'connected' : 'idle';
  if (online.length) {
    const first = online[0];
    $('peer-name').textContent = first.name;
    $('peer-sub').textContent = online.length > 1
      ? `Online at ${hostOf(first.address)}, plus ${online.length - 1} more`
      : `Online at ${hostOf(first.address)}`;
    $('link-caption').replaceChildren(
      h('strong', {}, `Direct over your local network to ${hostOf(first.address)}.`), ' Nothing goes to the internet.');
  } else {
    $('peer-name').textContent = peers.length ? 'Your devices are offline' : 'No devices paired yet';
    $('peer-sub').textContent = peers.length ? 'Waiting for them on this network' : 'Pair one to start syncing';
    $('link-caption').replaceChildren(
      h('strong', {}, 'Only looking on your local network.'), ' Nothing goes to the internet.');
  }
}

function renderPeers() {
  const list = $('peer-list');
  if (!status.peers.length) {
    list.replaceChildren(h('li', { class: 'empty' }, 'No devices yet. Choose Pair a device, then follow the steps on both screens.'));
    return;
  }
  list.replaceChildren(...status.peers.map((p) =>
    h('li', { class: `peer ${p.online ? 'is-online' : ''}` },
      h('div', { class: 'peer-top' },
        h('span', { class: 'dot', 'aria-hidden': 'true' }),
        h('div', { class: 'peer-text' },
          h('span', { class: 'peer-title' }, p.name),
          h('span', { class: 'peer-state' }, p.online ? `Online at ${hostOf(p.address)}` : 'Offline')),
        h('button', { class: 'btn btn-quiet', type: 'button', onclick: () => confirmUnpair(p) }, 'Unpair')),
      p.online && p.security_code
        ? h('div', { class: 'peer-words' }, words(p.security_code),
            h('p', { class: 'hint' }, 'Should match the words on the other device.'))
        : h('p', { class: 'hint' }, 'Security words appear when it connects.'))));
}

function renderFolders() {
  const list = $('folder-list');
  if (!status.folders.length) {
    list.replaceChildren(h('li', { class: 'empty' }, 'Nothing is synced yet. Add a folder here and one with the same name on your other device.'));
    return;
  }
  list.replaceChildren(...status.folders.map((f) =>
    h('li', { class: 'folder' },
      icon('folder', 'folder-icon'),
      h('div', { class: 'folder-text' },
        h('span', { class: 'folder-title' }, f.label),
        h('span', { class: 'folder-path', title: f.path }, shortPath(f.path)),
        h('span', { class: 'folder-meta' }, `${plural(f.files, 'file')}, ${formatBytes(f.bytes)}`)),
      h('button', { class: 'btn btn-quiet', type: 'button', onclick: () => confirmRemove(f) }, 'Stop syncing'))));
}

// ---------- Rename this device ----------

let renaming = false;
$('me-name').addEventListener('click', () => {
  if (renaming) return;
  renaming = true;
  const btn = $('me-name');
  const input = h('input', { class: 'me-name-input', value: status.me.name, 'aria-label': 'Device name', maxlength: '64' });
  btn.hidden = true;
  btn.after(input);
  input.select();
  let done = false;
  const finish = async (save) => {
    if (done) return;
    done = true;
    const name = input.value.trim();
    input.remove();
    btn.hidden = false;
    renaming = false;
    btn.focus();
    if (save && name && name !== status.me.name) {
      $('me-name-text').textContent = name;
      if ((await run('rename_device', { name })) === undefined) renderHeader();
      refresh();
    }
  };
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') finish(true);
    if (e.key === 'Escape') { e.preventDefault(); finish(false); }
  });
  input.addEventListener('blur', () => finish(true));
});

// ---------- Confirm dialog ----------

function confirmBox(title, body, okLabel) {
  const d = $('confirm-dialog');
  $('confirm-title').textContent = title;
  $('confirm-body').textContent = body;
  $('confirm-ok').textContent = okLabel;
  d.returnValue = '';
  d.showModal();
  return new Promise((resolve) => d.addEventListener('close', () => resolve(d.returnValue === 'ok'), { once: true }));
}

async function confirmUnpair(p) {
  const ok = await confirmBox(`Unpair ${p.name}?`,
    'It will be disconnected right away and can no longer sync with this device. You can pair it again later with a new code.',
    'Unpair');
  if (ok && (await run('unpair', { id: p.id })) !== undefined) refresh();
}

async function confirmRemove(f) {
  const ok = await confirmBox(`Stop syncing ${f.label}?`,
    `Changes in this folder will no longer be sent or received. The files are not deleted; they stay in ${f.path}.`,
    'Stop syncing');
  if (ok && (await run('remove_folder', { label: f.label })) !== undefined) refresh();
}

// ---------- Add folder ----------

$('add-folder-btn').addEventListener('click', async () => {
  let path;
  try {
    path = await pickFolder({ directory: true, multiple: false, title: 'Choose a folder to sync' });
  } catch (err) {
    toast(err);
    return;
  }
  if (!path) return; // cancelled
  const d = $('folder-dialog');
  $('folder-path').textContent = path;
  $('folder-label').value = baseName(path);
  d.dataset.path = path;
  d.showModal();
  $('folder-label').select();
});

$('folder-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const d = $('folder-dialog');
  const label = $('folder-label').value.trim();
  if (!label) return $('folder-label').focus();
  d.close();
  if ((await run('add_folder', { path: d.dataset.path, label })) !== undefined) refresh();
});

for (const btn of document.querySelectorAll('[data-close]')) {
  btn.addEventListener('click', () => btn.closest('dialog').close());
}

// ---------- Pair dialog ----------

const pair = { waiting: false, timer: null };

function showStep(pane, step) {
  for (const el of $(pane).querySelectorAll(':scope > [data-step]')) el.hidden = el.dataset.step !== step;
}

function pairDone(pane, p) {
  const box = $(pane).querySelector('[data-step="done"]');
  box.replaceChildren(
    h('div', { class: 'done-mark' }, icon('check')),
    h('p', { class: 'done-title' }, `Paired with ${p.name}`),
    words(p.security_code),
    h('p', { class: 'done-hint' }, 'Check the other screen shows the same four words. If they differ, unpair: something is in the middle.'),
    h('button', { class: 'btn btn-primary', type: 'button', onclick: () => $('pair-dialog').close() }, 'Done'));
  showStep(pane, 'done');
  box.querySelector('button').focus();
}

async function startShowCode() {
  stopShowCode();
  showStep('pane-show', 'waiting');
  $('pair-code').textContent = '––– –––';
  $('countdown-text').textContent = 'Getting a code…';
  let res;
  try {
    res = await invoke('pair_start');
  } catch (err) {
    return pairFailed(String(err));
  }
  pair.waiting = true;
  $('pair-code').textContent = res.code.replace('-', ' ');
  $('pair-code').setAttribute('aria-label', `Code ${res.code.split('').join(' ')}`);
  const end = Date.now() + res.expires_in_secs * 1000;
  const tick = () => {
    const left = Math.max(0, Math.ceil((end - Date.now()) / 1000));
    $('countdown-bar').style.transform = `scaleX(${left / res.expires_in_secs})`;
    $('countdown-text').textContent =
      `Waiting for the other device. Expires in ${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')}`;
    if (left === 0) pairFailed('The code expired before the other device used it.');
  };
  tick();
  pair.timer = setInterval(tick, 1000);
}

function stopShowCode() {
  clearInterval(pair.timer);
  if (pair.waiting) invoke('pair_cancel').catch(() => {});
  pair.waiting = false;
}

function pairFailed(reason) {
  clearInterval(pair.timer);
  pair.waiting = false;
  $('show-error').textContent = reason;
  showStep('pane-show', 'failed');
}

function selectTab(which) {
  const show = which === 'show';
  $('tab-show').setAttribute('aria-selected', show);
  $('tab-enter').setAttribute('aria-selected', !show);
  $('tab-show').tabIndex = show ? 0 : -1;
  $('tab-enter').tabIndex = show ? -1 : 0;
  $('pane-show').hidden = !show;
  $('pane-enter').hidden = show;
  if (show) startShowCode();
  else {
    stopShowCode();
    showStep('pane-enter', 'form');
    $('join-error').hidden = true;
    $('join-code').focus();
  }
}

$('pair-btn').addEventListener('click', () => {
  $('pair-dialog').showModal();
  selectTab('show');
  $('tab-show').focus();
});
$('pair-dialog').addEventListener('close', stopShowCode);
$('tab-show').addEventListener('click', () => selectTab('show'));
$('tab-enter').addEventListener('click', () => selectTab('enter'));
document.querySelector('.tabs').addEventListener('keydown', (e) => {
  if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
  const next = document.activeElement === $('tab-show') ? 'enter' : 'show';
  selectTab(next);
  $(`tab-${next}`).focus();
});
$('show-retry').addEventListener('click', startShowCode);

// Format the code as "123-456" while typing.
$('join-code').addEventListener('input', (e) => {
  const d = e.target.value.replace(/\D/g, '').slice(0, 6);
  e.target.value = d.length > 3 ? `${d.slice(0, 3)}-${d.slice(3)}` : d;
});

$('join-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const digits = $('join-code').value.replace(/\D/g, '');
  const err = $('join-error');
  if (digits.length !== 6) {
    err.textContent = 'The code has 6 digits, like 123-456.';
    err.hidden = false;
    return $('join-code').focus();
  }
  const btn = $('join-submit');
  btn.disabled = true;
  btn.textContent = 'Looking for the device…';
  err.hidden = true;
  try {
    const addr = $('join-addr').value.trim() || null;
    const p = await invoke('pair_join', { code: `${digits.slice(0, 3)}-${digits.slice(3)}`, addr });
    addEntry({ kind: 'paired', at: Date.now(), ...p });
    pairDone('pane-enter', p);
    $('join-form').reset();
    refresh();
  } catch (e2) {
    err.textContent = String(e2);
    err.hidden = false;
  } finally {
    btn.disabled = false;
    btn.textContent = 'Pair';
  }
});

// ---------- Live activity feed ----------

const feed = [];       // newest first: { key, at, node }
let savedBytes = 0;    // bytes that did not have to travel, this session

/** Describe one event as a feed entry, or return null for events that don't belong in the feed. */
function describe(ev) {
  const file = (path, folder) => h('span', { class: 'file', title: `${folder} / ${path}` }, baseName(path));
  const where = (folder, path) => h('span', { class: 'where' }, `${folder} / ${path}`);
  switch (ev.kind) {
    case 'peer_online':
      // Keyed per device: if both devices dialled at once, the session that
      // survives replaces the first entry, so the words shown are always the
      // ones the other screen shows too.
      return { key: `online:${ev.id}`, icon: 'link', tone: 'good', text: [h('strong', {}, ev.name), ` connected directly at ${hostOf(ev.address)}. The connection is encrypted.`],
        extra: h('div', { class: 'entry-words' }, words(ev.security_code), h('span', { class: 'hint' }, 'Same words on both screens means no one is in the middle.')) };
    case 'peer_offline':
      return { icon: 'unlink', tone: 'quiet', text: [h('strong', {}, ev.name), ' disconnected.'] };
    case 'paired':
      return { icon: 'check', tone: 'good', text: ['Paired with ', h('strong', {}, ev.name), '.'],
        extra: h('div', { class: 'entry-words' }, words(ev.security_code)) };
    case 'pair_failed':
      return { icon: 'pairFail', tone: 'alert', text: [`Pairing didn't finish: ${ev.reason}`] };
    case 'local_change': {
      const n = ev.added + ev.modified + ev.deleted;
      const parts = [ev.added && `${ev.added} new`, ev.modified && `${ev.modified} edited`, ev.deleted && `${ev.deleted} deleted`].filter(Boolean);
      const to = status.peers.filter((p) => p.online).map((p) => p.name);
      return { icon: 'send', tone: 'accent', flow: 'out',
        text: [`You changed ${plural(n, 'file')} in `, h('strong', {}, ev.folder), to.length ? ` — sending to ${listNames(to)}.` : '. They will be sent when your devices come online.'],
        extra: h('span', { class: 'where' }, parts.join(', ')) };
    }
    case 'download_progress':
      return { key: `dl:${ev.folder}/${ev.path}`, icon: 'arrive', tone: 'good', flow: 'in', progress: true,
        text: ['Receiving ', file(ev.path, ev.folder), ' from ', h('strong', {}, ev.from), '…'],
        extra: h('div', { class: 'progress' },
          h('div', { class: 'bar', role: 'progressbar', 'aria-valuemin': '0', 'aria-valuemax': '100', 'aria-valuenow': '0' }, h('span')),
          h('span', { class: 'progress-text' })) };
    case 'file_received': {
      const reused = ev.size - ev.transferred;
      const delta = ev.size > 0 && reused > ev.size * 0.05;
      return { key: `dl:${ev.folder}/${ev.path}`, icon: 'arrive', tone: 'good', flow: 'in',
        text: [file(ev.path, ev.folder), ' arrived from ', h('strong', {}, ev.from), delta ? '.' : ` (${formatBytes(ev.size)}).`],
        extra: delta
          ? h('div', { class: 'saving' },
              h('p', {}, 'Only ', h('strong', { class: 'teal' }, formatBytes(ev.transferred)), ` of ${formatBytes(ev.size)} had to travel. The rest was already here.`),
              h('div', { class: 'saving-meter', 'aria-hidden': 'true' },
                h('span', { style: `width:${Math.max(0.6, (100 * ev.transferred) / ev.size)}%` })),
              where(ev.folder, ev.path))
          : where(ev.folder, ev.path) };
    }
    case 'file_deleted':
      return { icon: 'trash', tone: 'quiet', text: [file(ev.path, ev.folder), ' was deleted on ', h('strong', {}, ev.from), ', so it was removed here too.'], extra: where(ev.folder, ev.path) };
    case 'conflict':
      return { icon: 'conflict', tone: 'accent',
        text: ['Both devices edited ', file(ev.path, ev.folder), '. Both versions were kept.'],
        extra: h('span', { class: 'where' }, 'Other version saved as ', h('span', { class: 'file' }, ev.copy)) };
    case 'refused':
      return { icon: 'shield', tone: 'alert',
        text: [`A device that isn't paired tried to connect from ${hostOf(ev.address)} and was blocked.`] };
    default:
      return null;
  }
}

function renderEntry(ev, d) {
  const li = h('li', { class: `entry tone-${d.tone}` },
    h('span', { class: 'entry-icon' }, icon(d.icon)),
    h('div', { class: 'entry-body' }, h('p', { class: 'entry-text' }, d.text), d.extra ?? null),
    h('time', { 'data-at': ev.at, datetime: new Date(ev.at).toISOString() }, timeAgo(ev.at)));
  if (d.progress) updateProgress(li, ev);
  return li;
}

function updateProgress(li, ev) {
  const pct = ev.total ? Math.min(100, (100 * ev.done) / ev.total) : 0;
  li.querySelector('.bar span').style.width = `${pct}%`;
  li.querySelector('.bar').setAttribute('aria-valuenow', Math.round(pct));
  li.querySelector('.progress-text').textContent = `${formatBytes(ev.done)} of ${formatBytes(ev.total)}`;
}

function addEntry(ev) {
  const d = describe(ev);
  if (!d) return;
  if (d.flow) flow(d.flow);
  const list = $('feed');
  const i = d.key ? feed.findIndex((e) => e.key === d.key) : -1;

  // A progress update for a download already on screen: just move its bar.
  if (i >= 0 && d.progress) {
    updateProgress(feed[i].node, ev);
    return;
  }
  // Finished (or a new) download: drop the in-flight entry, the finished one goes on top.
  if (i >= 0) feed.splice(i, 1)[0].node.remove();

  if (ev.kind === 'file_received') savedBytes += Math.max(0, ev.size - ev.transferred);
  const node = renderEntry(ev, d);
  node.classList.add('is-new');
  feed.unshift({ key: d.key, at: ev.at, node });
  list.prepend(node);
  while (feed.length > FEED_LIMIT) feed.pop().node.remove();

  $('feed-empty').hidden = true;
  const total = $('saved-total');
  total.hidden = savedBytes === 0;
  total.replaceChildren(h('strong', { class: 'teal' }, formatBytes(savedBytes)), ' did not need to cross the network this session');
}

// Animate the wire in the header while data moves (direction: 'in' or 'out').
let flowTimer = null;
function flow(direction) {
  const link = $('link');
  link.dataset.flow = direction;
  clearTimeout(flowTimer);
  flowTimer = setTimeout(() => delete link.dataset.flow, 2500);
}

setInterval(() => {
  for (const t of document.querySelectorAll('time[data-at]')) t.textContent = timeAgo(Number(t.dataset.at));
}, 15000);

// ---------- Events from the backend ----------

await listen('nebula://event', ({ payload: ev }) => {
  if (ev.kind === 'status_changed') return refresh();
  if (ev.kind === 'paired' && pair.waiting) {
    clearInterval(pair.timer);
    pair.waiting = false;
    pairDone('pane-show', ev);
  }
  if (ev.kind === 'pair_failed' && pair.waiting) pairFailed(ev.reason);
  if (ev.kind === 'peer_online' || ev.kind === 'peer_offline' || ev.kind === 'paired') refresh();
  addEntry(ev);
});

if (api.isMock) $('mock-badge').hidden = false;
await refresh();
setInterval(refresh, 5000);
