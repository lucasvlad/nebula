// Fake backend for opening the UI in a plain browser (no Tauri).
// It has the same shape as window.__TAURI__ (core.invoke, event.listen, dialog.open),
// implements every command in docs/ui-contract.md, and plays a looping script of events.
// Add ?script=off to the URL to stop the script (handy for a quiet "idle" screen).

const MiB = 1024 * 1024;
const UBUNTU = "Lucas's Ubuntu laptop";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const clone = (o) => JSON.parse(JSON.stringify(o));

const state = {
  me: { name: "Lucas's MacBook Air", id: 'a3f9c21e', port: 7878, addresses: ['192.168.1.17'] },
  peers: [
    { name: UBUNTU, id: '7c04be91', online: false, address: null, security_code: null },
    { name: 'Studio iMac', id: 'e2d81f07', online: false, address: null, security_code: null },
  ],
  folders: [
    { label: 'Homework', path: '/Users/lucas/Documents/Homework', files: 214, bytes: 1_738_014_720 },
    { label: 'Demo', path: '/Users/lucas/Demo', files: 37, bytes: 412 * MiB },
  ],
};

const listeners = [];
function emit(kind, fields = {}) {
  const payload = { kind, at: Date.now(), ...fields };
  for (const cb of listeners) cb({ event: 'nebula://event', payload });
}
const changed = () => emit('status_changed');
const peer = (name) => state.peers.find((p) => p.name === name);
const folder = (label) => state.folders.find((f) => f.label === label);

let pairTimer = null;
const randomCode = () => String(Math.floor(100000 + Math.random() * 900000)).replace(/(\d{3})(\d{3})/, '$1-$2');
const randomId = () => Math.floor(Math.random() * 0xffffffff).toString(16).padStart(8, '0');

function addPeer(name, words) {
  const p = { name, id: randomId(), security_code: words };
  state.peers.push({ ...p, online: true, address: '192.168.1.57:7878' });
  changed();
  return p;
}

const commands = {
  async status() {
    return clone(state);
  },
  async pair_start() {
    clearTimeout(pairTimer);
    // Pretend someone types the code on the other device after a few seconds.
    pairTimer = setTimeout(() => emit('paired', addPeer("Lucas's iPad", 'ORBIT LANTERN PEBBLE SAFFRON')), 8000);
    return { code: randomCode(), expires_in_secs: 120 };
  },
  async pair_cancel() {
    clearTimeout(pairTimer);
    return null;
  },
  async pair_join({ code, addr }) {
    const digits = code.replace(/\D/g, '');
    await sleep(1200);
    if (digits === '000000') throw 'Wrong code. Check the code on the other device and try again.';
    if (digits === '999999') throw 'No device is waiting to pair. Choose "Show a code" on the other device first.';
    if (addr && !addr.includes('.')) throw `Couldn't reach ${addr}. Check the address and that both devices are on the same network.`;
    return addPeer('Lab desktop', 'COMET HARBOR TULIP WALNUT');
  },
  async unpair({ id }) {
    const i = state.peers.findIndex((p) => p.id === id);
    if (i < 0) throw "That device isn't paired.";
    state.peers.splice(i, 1);
    changed();
    return null;
  },
  async add_folder({ path, label }) {
    if (!label.trim()) throw 'The folder needs a name.';
    if (folder(label)) throw `A folder named "${label}" is already synced.`;
    if (state.folders.some((f) => f.path === path)) throw 'That folder is already synced.';
    state.folders.push({ label, path, files: 0, bytes: 0 });
    changed();
    setTimeout(() => { Object.assign(folder(label) ?? {}, { files: 58, bytes: 286 * MiB }); changed(); }, 1500);
    return null;
  },
  async remove_folder({ label }) {
    if (!folder(label)) throw `No synced folder is named "${label}".`;
    state.folders = state.folders.filter((f) => f.label !== label);
    changed();
    return null;
  },
  async rename_device({ name }) {
    if (!name.trim()) throw "The device name can't be empty.";
    state.me.name = name.trim();
    changed();
    return null;
  },
  async connect_addr({ addr }) {
    if (!addr.includes(':')) throw 'Use the form 192.168.1.42:7878.';
    return null;
  },
};

// ---- Scripted demo: one pass shows every kind of event, then it repeats. ----

async function download(folderLabel, path, total, from, seconds) {
  const steps = seconds * 4; // the real backend sends at most 4 updates per second
  for (let i = 1; i < steps; i++) {
    emit('download_progress', { folder: folderLabel, path, done: Math.round((total * i) / steps), total, from });
    await sleep(250);
  }
}

function received(folderLabel, path, size, transferred, from = UBUNTU) {
  emit('file_received', { folder: folderLabel, path, from, size, transferred });
  const f = folder(folderLabel);
  if (f) { f.files += 1; f.bytes += size; }
  changed();
}

function setOnline(online, words) {
  const p = peer(UBUNTU);
  if (!p) return false;
  Object.assign(p, { online, address: online ? '192.168.1.42:7878' : null, security_code: online ? words : null });
  if (online) emit('peer_online', { name: p.name, id: p.id, address: p.address, security_code: words });
  else emit('peer_offline', { name: p.name, id: p.id });
  changed();
  return true;
}

async function script() {
  const sessionWords = ['MUFFIN THIMBLE GLOBE VELVET', 'RIVER CANDLE ORBIT MAPLE'];
  for (let round = 0; ; round++) {
    await sleep(1200);
    setOnline(true, sessionWords[round % 2]);
    await sleep(3500);
    emit('local_change', { folder: 'Homework', added: 0, modified: 2, deleted: 0 });
    await sleep(3500);
    await download('Demo', 'Videos/demo-day.mov', 95.4 * MiB, UBUNTU, 7);
    received('Demo', 'Videos/demo-day.mov', 95.4 * MiB, 95.4 * MiB);
    await sleep(4000);
    // One line changed in a big file: almost every chunk was already here.
    received('Demo', 'Datasets/survey-results.csv', 95.4 * MiB, 2355);
    await sleep(5000);
    emit('conflict', { folder: 'Homework', path: 'notes.txt', copy: `notes (conflict copy ${UBUNTU} 2026-11-23 14-02).txt` });
    await sleep(4500);
    emit('file_deleted', { folder: 'Homework', path: 'drafts/old-outline.docx', from: UBUNTU });
    await sleep(4000);
    emit('refused', { address: '192.168.1.88:51234', id: '9d1c44f0' });
    await sleep(4000);
    emit('local_change', { folder: 'Demo', added: 3, modified: 0, deleted: 1 });
    await sleep(3500);
    await download('Demo', 'Photos/IMG_2041.heic', 4.2 * MiB, UBUNTU, 2);
    received('Demo', 'Photos/IMG_2041.heic', 4.2 * MiB, 4.2 * MiB);
    await sleep(5000);
    setOnline(false);
    await sleep(4000);
  }
}

if (new URLSearchParams(location.search).get('script') === 'off') {
  peer(UBUNTU).online = true;
  setOnline(true, 'MUFFIN THIMBLE GLOBE VELVET');
} else {
  script();
}

const folderChoices = ['/Users/lucas/Pictures/Trip photos', '/Users/lucas/Documents/Capstone paper', '/Users/lucas/Music/Practice'];
let folderPick = 0;

export const core = {
  async invoke(cmd, args = {}) {
    if (!commands[cmd]) throw `Unknown command: ${cmd}`;
    return commands[cmd](args);
  },
};
export const event = {
  async listen(name, cb) {
    if (name === 'nebula://event') listeners.push(cb);
    return () => listeners.splice(listeners.indexOf(cb), 1);
  },
};
export const dialog = {
  async open() {
    return folderChoices[folderPick++ % folderChoices.length];
  },
};
export const isMock = true;
