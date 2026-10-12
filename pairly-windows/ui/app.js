// Pairly for Windows: the window. The backend sends the whole picture as one `state` event
// whenever anything changes; this file draws it and turns clicks into backend commands. The
// bigger pages (messages, contacts, a phone's files, commands) live in views.js and fetch what
// they show themselves.

const tauri = window.__TAURI__;
const invoke = (cmd, args) => tauri.core.invoke(cmd, args);
const listen = (name, f) => tauri.event.listen(name, (e) => f(e.payload));

let state = null;
let selected = null;
// "device" | "settings" | one of the views in views.js ("messages", "contacts", "files", "commands")
let view = "device";
let narrowOpen = false; // the device list, slid over the page on a narrow window
const narrow = () => window.innerWidth <= 640;

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

function toast(text) {
  const t = document.createElement("div");
  t.className = "toast";
  t.textContent = text;
  $("toasts").append(t);
  setTimeout(() => t.remove(), 3500);
}

/** Run a command; errors become a toast. */
async function run(cmd, args, done) {
  try {
    const r = await invoke(cmd, args);
    if (done) toast(typeof done === "function" ? done(r) : done);
    return r;
  } catch (e) {
    toast(String(e));
  }
}

function humanSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = bytes / 1024, i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

function clock(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  const mm = h ? String(m).padStart(2, "0") : m;
  return `${h ? h + ":" : ""}${mm}:${String(sec).padStart(2, "0")}`;
}

/** "14:05" today, "Mon" this week, else "3 Oct" (or "3 Oct 2024"). */
function when(ms) {
  if (!ms) return "";
  const d = new Date(ms), now = new Date();
  if (d.toDateString() === now.toDateString())
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (now - d < 6 * 864e5) return d.toLocaleDateString([], { weekday: "short" });
  const opts = { day: "numeric", month: "short" };
  if (d.getFullYear() !== now.getFullYear()) opts.year = "numeric";
  return d.toLocaleDateString([], opts);
}

// ----- light and dark ------------------------------------------------------------------------

const systemDark = window.matchMedia("(prefers-color-scheme: dark)");
function applyTheme() {
  const choice = state?.settings.theme ?? "system";
  const dark = choice === "dark" || (choice === "system" && systemDark.matches);
  document.documentElement.dataset.theme = dark ? "dark" : "light";
}
systemDark.addEventListener("change", applyTheme);

// ----- the drawn device ------------------------------------------------------------------------

/** A phone or laptop drawing with its battery: a bar on the screen and the percentage. */
function deviceArt(kind, battery, charging, scale = 1) {
  const laptop = !["phone", "tablet"].includes(kind);
  const w = (laptop ? 96 : 52) * scale, h = (laptop ? 66 : 88) * scale;
  const level = battery >= 0 ? Math.min(100, battery) : null;
  const fill = charging ? "var(--green)" : level !== null && level <= 15 ? "var(--red)" : "var(--art)";
  const screen = laptop ? { x: 8, y: 2, w: 80, h: 54, r: 6 } : { x: 2, y: 2, w: 48, h: 84, r: 11 };
  const bar = level === null ? "" : `
    <rect x="${screen.x + 8}" y="${screen.y + screen.h - 11}" width="${screen.w - 16}" height="5" rx="2.5"
      fill="var(--art)" opacity=".2"/>
    <rect x="${screen.x + 8}" y="${screen.y + screen.h - 11}" width="${Math.max(5, (screen.w - 16) * level / 100)}"
      height="5" rx="2.5" fill="${fill}"/>
    <text x="${screen.x + screen.w / 2}" y="${screen.y + screen.h / 2 + 2}" text-anchor="middle"
      font-size="13" font-weight="700" fill="var(--art)">${level}%</text>`;
  const extra = laptop
    ? `<rect x="1" y="57" width="94" height="7" rx="3.5" fill="var(--art)"/>`
    : `<rect x="19" y="7" width="14" height="3" rx="1.5" fill="var(--art)" opacity=".6"/>`;
  return `<svg width="${w}" height="${h}" viewBox="0 0 ${laptop ? 96 : 52} ${laptop ? 66 : 88}" aria-hidden="true">
    <rect x="${screen.x}" y="${screen.y}" width="${screen.w}" height="${screen.h}" rx="${screen.r}"
      fill="var(--art)" fill-opacity=".1" stroke="var(--art)" stroke-width="2.5"/>
    ${extra}${bar}</svg>`;
}

const isPhone = (d) => d && (d.kind === "phone" || d.kind === "tablet");

// ----- the device list -----------------------------------------------------------------------

function renderSidebar() {
  const list = $("device-list");
  const paired = state.devices.filter((d) => d.paired);
  const nearby = state.devices.filter((d) => !d.paired);
  const row = (d) => {
    const status = d.paused
      ? `<span class="paused-label">❚❚ Paused</span>`
      : !d.paired ? `<span>Available to pair</span>`
      : d.call ? `<span class="on"><span class="dot"></span>${d.call.state === "ringing" ? "Ringing" : "On a call"}</span>`
      : d.connected ? `<span class="on"><span class="dot"></span>Connected</span>`
      : `<span class="off"><span class="dot"></span>Offline</span>`;
    const icon = isPhone(d) || !d.kind ? "📱" : "💻";
    const here = d.id === selected && view !== "settings" && view !== "commands";
    return `<button class="device-row ${here ? "selected" : ""}" data-id="${d.id}">
      <span class="avatar">${icon}</span>
      <span><div class="name">${esc(d.name)}</div><div class="sub">${status}</div></span></button>`;
  };
  list.innerHTML =
    (paired.length ? `<div class="section">Your devices</div>${paired.map(row).join("")}` : "") +
    (nearby.length ? `<div class="section">Nearby</div>${nearby.map(row).join("")}` : "");
  list.querySelectorAll(".device-row").forEach((b) => b.addEventListener("click", () => {
    const changed = selected !== b.dataset.id;
    selected = b.dataset.id;
    // Stay on the same kind of page for the other phone, when it has one.
    if (changed || !Views[view]) view = "device";
    narrowOpen = false;
    render(true);
  }));
  $("me-name").textContent = state.me ? state.me[1] : "";
  const on = state.settings.enabled;
  $("power-switch").checked = on;
  $("power-label").textContent = on ? "Pairly is on" : "Pairly is off";
  $("app").classList.toggle("no-sidebar", narrow() ? !narrowOpen : !state.settings.sidebar);
}

// ----- the device page -----------------------------------------------------------------------

function tile(icon, name, hint, hue, enabled, action) {
  return `<button class="tile hue-${hue}" data-action="${action}" ${enabled ? "" : "disabled"} title="${esc(hint)}">
    <span class="glyph">${icon}</span><span><div class="name">${esc(name)}</div><div class="hint">${esc(hint)}</div></span></button>`;
}

function linkText(d) {
  const link = { Lan: "Local network", Bluetooth: "Bluetooth", Relay: "Internet" }[d.link] ?? d.link;
  return d.rtt_ms ? `${link} · ${d.rtt_ms} ms` : link;
}

/** A call on the phone: answer or reject it from here. */
function callCard(d) {
  const c = d.call;
  if (!c) return "";
  const who = esc(c.caller) + (c.number && c.number !== c.caller ? `<div class="sub">${esc(c.number)}</div>` : "");
  const buttons = c.state === "ringing"
    ? `<button class="btn primary" data-call="answer">Answer</button>
       <button class="btn" data-call="speaker">Speaker</button>
       <button class="btn bad" data-call="reject">Reject</button>`
    : `<button class="btn bad" data-call="hangup">Hang Up</button>`;
  return `<div class="card call-card ${c.state}"><div class="row">
    <span class="call-glyph">📞</span>
    <div class="grow"><div class="sub">${c.state === "ringing" ? "Incoming call" : "On a call"}</div><b>${who}</b></div>
    ${buttons}</div></div>`;
}

/** This PC's screen shown on the phone. */
function castCard(d) {
  if (!d.sharing) return "";
  return `<div class="card"><div class="row"><span>🖥️</span>
    <div class="grow"><b>Your screen is shown on ${esc(d.name)}</b>
    <div class="sub">It can see this screen and use the mouse and keyboard</div></div>
    <button class="btn bad" data-action="stop-cast">Stop Sharing</button></div></div>`;
}

// Artwork fetched from the backend: "device|key" → URL (or "" while asking).
const artCache = new Map();
function artFor(device, key) {
  if (!key) return null;
  const k = `${device}|${key}`;
  if (!artCache.has(k)) {
    artCache.set(k, "");
    invoke("artwork", { id: device, key }).then((url) => {
      if (url) { artCache.set(k, url); render(); } else artCache.delete(k);
    }).catch(() => artCache.delete(k));
  }
  return artCache.get(k) || null;
}

function position(d, p) {
  const at = p.position_ms ?? 0;
  const moved = p.playing && d.players_at ? Date.now() - d.players_at : 0;
  return p.length_ms ? Math.min(p.length_ms, at + moved) : at + moved;
}

/** The phone's music: what plays, with its controls. */
function nowPlaying(d) {
  if (!d.players?.length) return "";
  return `<div class="group"><h2>Now playing on ${esc(d.name)}</h2>${d.players.slice(0, 3).map((p) => {
    const art = artFor(d.id, p.art);
    const pos = position(d, p);
    const seek = p.can_seek && p.length_ms
      ? `<div class="seek"><span data-pos="${esc(p.id)}">${clock(pos)}</span>
          <input type="range" min="0" max="${p.length_ms}" value="${pos}" data-seek="${esc(p.id)}">
          <span>${clock(p.length_ms)}</span></div>` : "";
    const volume = p.volume != null && p.volume >= 0
      ? `<label class="volume" title="Volume">🔈<input type="range" min="0" max="100" value="${p.volume}" data-volume="${esc(p.id)}"></label>` : "";
    return `<div class="card player"><div class="row">
      <div class="art">${art ? `<img src="${esc(art)}" alt="">` : "🎵"}</div>
      <div class="grow"><b>${esc(p.title || "Nothing playing")}</b>
        <div class="sub">${esc([p.artist, p.album].filter(Boolean).join(" · ") || p.name)}</div>
        ${seek}</div>
      <div class="controls">
        <button class="icon" data-media="previous" data-player="${esc(p.id)}" ${p.can_previous ? "" : "disabled"} title="Previous">⏮</button>
        <button class="icon big" data-media="play_pause" data-player="${esc(p.id)}" title="${p.playing ? "Pause" : "Play"}">${p.playing ? "⏸" : "▶"}</button>
        <button class="icon" data-media="next" data-player="${esc(p.id)}" ${p.can_next ? "" : "disabled"} title="Next">⏭</button>
        ${volume}
      </div></div></div>`;
  }).join("")}</div>`;
}

function devicePage(d) {
  const tags = [];
  if (!d.paired) tags.push(`<span class="tag">Not paired</span>`);
  else if (d.paused) tags.push(`<span class="tag bad">❚❚ Paused</span>`);
  else if (d.connected) tags.push(`<span class="tag good">● Connected</span>`, `<span class="tag">${esc(linkText(d))}</span>`);
  else tags.push(`<span class="tag bad">● Offline</span>`);
  if (d.charging) tags.push(`<span class="tag good">Charging</span>`);
  else if (d.battery >= 0 && d.battery <= 15) tags.push(`<span class="tag bad">Battery low</span>`);
  const pause = d.paired
    ? `<button class="round" data-action="${d.paused ? "resume" : "pause"}"
         title="${d.paused ? "Resume: allow this device again" : "Pause: no connection either way until resumed"}">${d.paused ? "▶" : "❚❚"}</button>`
    : "";
  let body = "";
  if (!d.paired) {
    body = `<div class="card"><div class="row"><div class="grow"><b>Pair with a code</b>
      <div class="sub">Both screens will show the same 6-digit code to compare</div></div>
      <button class="btn primary" data-action="pair">Pair</button></div></div>`;
  } else if (d.paused) {
    body = `<div class="card"><div class="row"><div class="grow"><b>Paused</b>
      <div class="sub">${esc(d.name)} can't reach this PC and this PC can't reach it: no notifications, files, messages or control either way. It stays paired.</div></div>
      <button class="btn primary" data-action="resume">Resume</button></div></div>`;
  } else {
    const on = d.connected;
    const phone = isPhone(d);
    const pc = d.kind === "desktop" || d.kind === "laptop";
    const ringing = state.ringing.includes(d.id);
    const lan = d.link === "Lan";
    body = `${callCard(d)}${castCard(d)}<div class="tiles">
      ${phone ? tile("📲", "Phone Screen", lan || !on ? "See and control it" : "Needs the same Wi-Fi", "purple", on && lan, "screen") : ""}
      ${phone ? tile("💬", "Messages", "Read and send texts", "teal", on, "messages") : ""}
      ${phone ? tile("📇", "Contacts & Calls", "Call or text from here", "coral", on, "contacts") : ""}
      ${phone ? tile("🗂️", "Browse Files", "The phone's storage", "amber", on, "files") : ""}
      ${pc ? tile("🗂️", "Browse Files", "The folder it shares", "amber", on, "files") : ""}
      ${pc ? tile("🖥️", "PC Screen", lan || !on ? "See and control it" : "Needs the same Wi-Fi", "purple", on && lan, "screen") : ""}
      ${tile("📤", "Send Files", "Or drop them on this window", "blue", on, "send-files")}
      ${tile("🔗", "Link or Text", "Opens or copies there", "blue", on, "text")}
      ${tile("📋", "Clipboard", "Send what you copied", "pink", on, "clipboard")}
      ${ringing ? tile("🔕", "Stop Ringing", "Ringing now…", "red", on, "ring-off")
                : tile("🔔", phone ? "Find My Phone" : "Ring It", "Rings, even on silent", "green", on, "ring-on")}
      ${tile("👋", "Ping", "Show a notification", "green", on, "ping")}
      ${phone ? tile("🔒", "Lock Phone", "Lock its screen now", "gray", on, "lock") : ""}
      ${phone ? tile("⏻", "Power Off", "Power off or restart", "red", on, "power") : ""}
      ${tile("⌨️", "Commands", "What it may run here", "gray", true, "commands")}
      ${tile("📁", "Received Files", "Open the Pairly folder", "amber", true, "downloads")}
    </div>${nowPlaying(d)}`;
  }
  const transfers = state.transfers.filter((t) => t.device === d.id && !/^(done|cancelled|failed)/.test(t.state));
  const transferRows = transfers.map((t) => `<div class="row">
      <span>${t.incoming ? "📥" : "📤"}</span>
      <div class="grow"><b>${esc(t.name)}</b><div class="sub">${t.state === "waiting" ? "Waiting…" : `${Math.round(100 * t.bytes / Math.max(1, t.size))}% of ${humanSize(t.size)}`}</div></div>
      <progress max="${Math.max(1, t.size)}" value="${t.bytes}"></progress>
      <button class="btn" data-transfer="${t.id}">Cancel</button></div>`).join("");
  return `<div class="page-inner">
    <section class="banner">${deviceArt(d.kind, d.battery, d.charging)}
      <div class="text"><h1>${esc(d.name)}</h1><div class="tags">${tags.join("")}</div></div>${pause}</section>
    ${body}
    ${transfers.length ? `<div class="group"><h2>Transfers</h2><div class="card">${transferRows}</div></div>` : ""}
    <div class="group"><h2>Details</h2><div class="card"><div class="row"><div class="grow">Device ID<div class="sub">${esc(d.id)}</div></div></div></div></div>
    ${d.paired ? `<div><button class="btn danger" data-action="unpair">Unpair…</button></div>` : ""}
  </div>`;
}

function emptyPage() {
  if (!state.settings.enabled) {
    return `<div class="empty"><div class="big">⏻</div><h1>Pairly is off</h1>
      <p>Your devices can't reach this PC and nothing is shared. Turn it on when you need it.</p>
      <button class="btn primary" id="turn-on">Turn on</button></div>`;
  }
  const any = state.devices.some((d) => d.paired);
  return `<div class="empty"><div class="big">📱</div><h1>${any ? "Select a device" : "No devices yet"}</h1>
    <p>${any ? "" : "Pair your phone by scanning a code with the Pairly app."}</p>
    ${any ? "" : `<button class="btn primary" id="pair-empty">Pair a device</button>`}</div>`;
}

// ----- settings ------------------------------------------------------------------------------

// This PC's notification apps, fetched when Settings opens.
let pcNotifications = null;

function settingsPage() {
  const s = state.settings;
  const u = state.update;
  const status = {
    checking: "Checking for updates…",
    "up-to-date": `Pairly ${state.version} is up to date`,
    available: `Pairly ${u.detail} is available`,
    downloading: `Downloading Pairly ${u.detail}…`,
    installed: `Updated to ${u.detail}: restarting…`,
    failed: `Couldn't update: ${u.detail}`,
    unsupported: u.detail,
  }[u.state] ?? `Version ${state.version}`;
  const seg = (v, label) => `<button class="${s.theme === v ? "on" : ""}" data-theme-choice="${v}">${label}</button>`;
  const sw = (key, on, enabled = true) => `<label class="switch"><input type="checkbox" data-option="${key}" ${on ? "checked" : ""} ${enabled ? "" : "disabled"}><span></span></label>`;
  const row = (title, sub, control) => `<div class="row"><div class="grow"><b>${title}</b>${sub ? `<div class="sub">${sub}</div>` : ""}</div>${control}</div>`;
  const bt = state.bluetooth;
  const btText = bt === "on" ? "On: bonded phones can connect without a network"
    : bt === "off" || !bt ? "Phones reach this PC only over the network"
    : `Not available: ${esc(bt)}`;
  const pcn = pcNotifications;
  const pcStatus = !pcn ? "…" : pcn.status === "on" ? "Your phone gets this PC's notifications" : esc(pcn.status);
  const apps = pcn?.apps?.length
    ? pcn.apps.map(([name, send]) => `<div class="row"><div class="grow">${esc(name)}</div>
        <label class="switch"><input type="checkbox" data-app="${esc(name)}" ${send ? "checked" : ""}><span></span></label></div>`).join("")
    : "";
  return `<div class="page-inner settings">
    <div class="group"><h2>Appearance</h2><div class="card">
      ${row("Style", "Light, dark, or the same as Windows", `<div class="segments">${seg("system", "System")}${seg("light", "Light")}${seg("dark", "Dark")}</div>`)}
    </div></div>
    <div class="group"><h2>This PC</h2><div class="card">
      <div class="row"><div class="grow"><b>Device name</b><div class="sub">Your phone shows this name</div>
        <input class="field" id="name-field" value="${esc(state.me ? state.me[1] : s.name ?? "")}" maxlength="64"></div>
        <button class="btn" id="rename">Save</button></div>
      <div class="row"><div class="grow"><b>Start with Windows</b><div class="sub">Pairly runs in the tray after you sign in</div></div>
        <label class="switch"><input type="checkbox" id="autostart"><span></span></label></div>
      ${row("Send copied text automatically", "What you copy here goes to your connected devices", sw("clipboard_auto", s.clipboard_auto))}
      ${row("Open links right away", "Links sent from the phone open in your browser (else: a notification to open them)", sw("open_links", s.open_links))}
      ${row("Received files", "Saved in Downloads\\Pairly", `<button class="btn" data-action="downloads">Open</button>`)}
      ${row("Commands", "What your phone may run on this PC", `<button class="btn" data-action="commands">Edit</button>`)}
      ${state.me ? `<div class="row"><div class="grow">Device ID<div class="sub">${esc(state.me[0])}</div></div></div>` : ""}
    </div></div>
    <div class="group"><h2>Share files</h2><div class="card">
      ${row("Share a folder with my devices", "They can browse it and download from it, wherever they are. Hidden and system files are never shared.", sw("share_files", s.share_files))}
      ${row("Folder", esc(s.share_folder ?? "Your user folder"), `<button class="btn" data-action="share-folder" ${s.share_files ? "" : "disabled"}>Choose</button>`)}
      ${row("Allow changes", "They may also upload, rename and delete there", sw("share_changes", s.share_changes, s.share_files))}
    </div></div>
    <div class="group"><h2>Phone notifications</h2><div class="card">
      ${row("Show the phone's notifications", "With their buttons and replies", sw("show_notifications", s.show_notifications))}
      ${row("Closing one here clears it on the phone", "", sw("dismiss_on_phone", s.dismiss_on_phone, s.show_notifications))}
    </div></div>
    <div class="group"><h2>This PC's notifications</h2><div class="card">
      ${row("Send them to your phone", pcStatus, sw("send_notifications", s.send_notifications))}
      ${apps ? `<div class="row"><div class="grow sub">Apps that posted notifications here, and whether they go to your phone</div></div>${apps}` : ""}
    </div></div>
    <div class="group"><h2>Calls and control</h2><div class="card">
      ${row("Pause music during calls", "This PC's players pause while the phone rings or is in a call", sw("pause_media_for_calls", s.pause_media_for_calls))}
      ${row("Phones may lock, restart or power off this PC", "", sw("power_from_phone", s.power_from_phone))}
      ${row("Phones and PCs may view and control this screen", "On the same Wi-Fi; a notification shows while it does", sw("share_screen", s.share_screen))}
    </div></div>
    <div class="group"><h2>Connections</h2><div class="card">
      <div class="row"><div class="grow"><b>Relay</b>
        <div class="sub">Reach your devices over the internet through your own relay: the <code>wss://…</code> address of your Cloudflare relay, or the <code>pairly-relay://…</code> one your server prints. Paired phones learn it from this PC.</div>
        <input class="field" id="relay-field" value="${esc(s.relay ?? "")}" placeholder="wss://…" spellcheck="false">
        ${state.relay_error ? `<div class="sub bad-text">${esc(state.relay_error)}</div>` : ""}</div>
        <button class="btn" id="save-relay">Save</button></div>
      ${row("Hide message sizes from the relay", "Pads what goes through it to 256-byte steps", sw("relay_padding", s.relay_padding, !!s.relay))}
      ${row("Bluetooth", btText, sw("bluetooth", s.bluetooth))}
    </div></div>
    <div class="group"><h2>Updates</h2><div class="card">
      ${row("Install updates automatically", "New versions from Pairly's GitHub releases, installed only when signed by the project", sw("auto_update", s.auto_update))}
      <div class="row"><div class="grow"><b>Pairly</b><div class="sub">${esc(status)}</div></div>
        <button class="btn" id="check-update" ${["checking", "downloading"].includes(u.state) ? "disabled" : ""}>${u.state === "available" ? "Install" : "Check now"}</button></div>
    </div></div>
    <div class="group"><h2>About</h2><div class="card">
      <div class="row"><div class="grow">Version<div class="sub">${esc(state.version)}</div></div></div>
      <div class="row"><div class="grow">Licence<div class="sub">GPL-3.0</div></div></div>
    </div></div>
  </div>`;
}

async function loadNotificationApps() {
  pcNotifications = await invoke("notification_apps").catch(() => null);
  if (view === "settings") render(true);
}

// ----- dialogs -------------------------------------------------------------------------------

function openDialog(html) {
  $("dialog").innerHTML = html;
  $("modal").hidden = false;
}
function closeDialog() {
  $("modal").hidden = true;
  $("dialog").innerHTML = "";
}

let qrOpen = false;
async function showPairing() {
  qrOpen = true;
  await run("start_qr");
  renderPairing();
}
function renderPairing() {
  if (!qrOpen) return;
  const nearby = state.devices.filter((d) => !d.paired);
  openDialog(`<h2>Pair a device</h2>
    <div class="qr">${state.qr ?? "…"}</div>
    <p>Open Pairly on your phone, tap <b>Scan</b>, and point the camera at this code. It works once and refreshes every few minutes.</p>
    ${nearby.length ? `<div class="card">${nearby.map((d) => `<div class="row"><div class="grow">${esc(d.name)}<div class="sub">Nearby</div></div>
      <button class="btn" data-pair="${d.id}">Pair with a code</button></div>`).join("")}</div>` : ""}
    <div class="actions"><button class="btn" id="close-pairing">Close</button></div>`);
}

function renderCodePrompt() {
  const p = state.pairing;
  if (!p) return;
  qrOpen = false;
  if ($("dialog").dataset.code === p.code) return;
  openDialog(`<h2>Pair with ${esc(p.name)}?</h2>
    <p>Check that ${esc(p.name)} shows the same code:</p>
    <div class="code">${esc(p.code)}</div>
    <div class="actions"><button class="btn" data-confirm="no">Cancel</button>
      <button class="btn primary" data-confirm="yes">Codes match</button></div>`);
  $("dialog").dataset.code = p.code;
}

// ----- drawing -------------------------------------------------------------------------------

let mounted = null; // which view is in the page now: "messages:<id>" and the like

/** Draw everything. Views from views.js keep their page (with what's typed and scrolled) across
 *  the regular redraws; `force` draws it afresh. */
function render(force = false) {
  if (!state) return;
  applyTheme();
  renderSidebar();
  const d = state.devices.find((x) => x.id === selected) ??
    state.devices.find((x) => x.paired) ?? null;
  if (d && !selected) selected = d.id;
  const page = $("page");
  const sub = Views[view];
  page.classList.toggle("full", !!sub?.full);
  if (sub && (view === "commands" || (d && d.paired && state.settings.enabled))) {
    const key = `${view}:${view === "commands" ? "" : d.id}`;
    $("page-title").textContent = sub.title(d);
    if (force || mounted !== key) {
      mounted = key;
      page.innerHTML = "";
      sub.mount(page, d);
    } else {
      sub.update?.(d);
    }
  } else {
    if (sub) view = "device";
    mounted = null;
    // Keep what's being typed (and where) across the regular redraws.
    const active = document.activeElement;
    const keep = active && active.id && page.contains(active) && /^(INPUT|TEXTAREA)$/.test(active.tagName)
      ? { id: active.id, value: active.value, start: active.selectionStart, end: active.selectionEnd } : null;
    // Don't redraw under a slider being dragged.
    if (!force && dragging) return;
    const scroll = page.scrollTop;
    if (view === "settings") {
      $("page-title").textContent = "Settings";
      page.innerHTML = settingsPage();
      run("autostart", {}).then((on) => { const a = $("autostart"); if (a) a.checked = !!on; });
    } else if (d && state.settings.enabled) {
      $("page-title").textContent = d.name;
      page.innerHTML = devicePage(d);
    } else {
      $("page-title").textContent = "Pairly";
      page.innerHTML = emptyPage();
    }
    page.scrollTop = scroll;
    if (keep) {
      const field = $(keep.id);
      if (field) {
        field.value = keep.value;
        field.focus();
        try { field.setSelectionRange(keep.start, keep.end); } catch { /* not a text field */ }
      }
    }
  }
  $("back-button").hidden = !sub;
  if (state.pairing) renderCodePrompt();
  else if (qrOpen) renderPairing();
}

let dragging = false;
document.addEventListener("pointerdown", (e) => { if (e.target.matches?.("input[type=range]")) dragging = true; });
document.addEventListener("pointerup", () => { dragging = false; });

// Positions of playing music move on by themselves.
setInterval(() => {
  if (!state || view !== "device") return;
  const d = current();
  for (const p of d?.players ?? []) {
    const label = document.querySelector(`[data-pos="${CSS.escape(p.id)}"]`);
    if (label) label.textContent = clock(position(d, p));
    const bar = document.querySelector(`[data-seek="${CSS.escape(p.id)}"]`);
    if (bar && !dragging) bar.value = position(d, p);
  }
}, 1000);

// ----- clicks --------------------------------------------------------------------------------

function current() {
  return state?.devices.find((x) => x.id === selected);
}

function show(v) {
  view = v;
  narrowOpen = false;
  if (v === "settings") loadNotificationApps();
  render(true);
}

async function pickFiles(d) {
  const picked = await tauri.dialog.open({ multiple: true, title: `Send files to ${d.name}` });
  const paths = picked ? (Array.isArray(picked) ? picked : [picked]) : [];
  if (paths.length) await run("send_files", { id: d.id, paths }, (n) => `Sending ${n} file${n === 1 ? "" : "s"} to ${d.name}`);
}

function askText(d) {
  openDialog(`<h2>Send to ${esc(d.name)}</h2>
    <p>A link opens in the browser there; other text is copied to the clipboard.</p>
    <textarea class="field" id="text-field" rows="4" placeholder="https://… or any text"></textarea>
    <div class="actions"><button class="btn" id="cancel-dialog">Cancel</button><button class="btn primary" id="send-text">Send</button></div>`);
  $("text-field").focus();
}

function askPower(d) {
  openDialog(`<h2>Power off ${esc(d.name)}?</h2><p>You'll need to turn it back on at the phone.</p>
    <div class="actions"><button class="btn" id="cancel-dialog">Cancel</button>
      <button class="btn" data-power="restart">Restart</button><button class="btn primary" data-power="poweroff">Power off</button></div>`);
}

function askUnpair(d) {
  openDialog(`<h2>Unpair ${esc(d.name)}?</h2><p>You'll need to pair again to reconnect.</p>
    <div class="actions"><button class="btn" id="cancel-dialog">Cancel</button><button class="btn primary" id="do-unpair">Unpair</button></div>`);
}

document.addEventListener("click", async (e) => {
  const t = e.target.closest("button, [data-theme-choice]");
  if (!t || t.disabled) return;
  const d = current();
  const action = t.dataset.action;
  if (action && d) {
    switch (action) {
      case "send-files": return pickFiles(d);
      case "text": return askText(d);
      case "clipboard": return run("send_clipboard", { id: d.id }, `Sent the clipboard to ${d.name}`);
      case "ring-on": return run("ring", { id: d.id, on: true });
      case "ring-off": return run("ring", { id: d.id, on: false });
      case "ping": return run("ping", { id: d.id }, `Pinged ${d.name}`);
      case "lock": return run("phone_power", { id: d.id, action: "lock" }, `Locking ${d.name}…`);
      case "power": return askPower(d);
      case "pause": return run("set_paused", { id: d.id, paused: true }, `Paused ${d.name}: nothing passes either way until you resume it`);
      case "resume": return run("set_paused", { id: d.id, paused: false }, `Resumed ${d.name}`);
      case "pair": return run("pair", { id: d.id });
      case "unpair": return askUnpair(d);
      case "screen": return run("screen_open", { id: d.id });
      case "stop-cast": return run("stop_cast", { id: d.id });
      case "messages": case "contacts": case "files": return show(action);
    }
  }
  if (action === "commands") return show("commands");
  if (action === "downloads") return run("open_downloads");
  if (action === "share-folder") {
    const folder = await tauri.dialog.open({ directory: true, title: "Folder to share" });
    if (folder) return run("set_share_folder", { folder }, "Sharing that folder with your devices");
    return;
  }
  if (t.dataset.call && d) return run("call_action", { id: d.id, action: t.dataset.call });
  if (t.dataset.media && d) {
    return run("media_control", { id: d.id, player: t.dataset.player, action: t.dataset.media, value: 0 });
  }
  if (t.dataset.transfer) return run("cancel_transfer", { id: Number(t.dataset.transfer) });
  if (t.dataset.pair) return run("pair", { id: t.dataset.pair });
  if (t.dataset.confirm) {
    const p = state.pairing;
    closeDialog();
    $("dialog").dataset.code = "";
    if (p) await run("confirm_pair", { id: p.id, accept: t.dataset.confirm === "yes" });
    return;
  }
  if (t.dataset.power && d) {
    closeDialog();
    return run("phone_power", { id: d.id, action: t.dataset.power }, `${t.dataset.power === "restart" ? "Restarting" : "Powering off"} ${d.name}…`);
  }
  if (t.dataset.themeChoice) return run("set_theme", { theme: t.dataset.themeChoice });
  switch (t.id) {
    case "pair-button": case "pair-empty": return showPairing();
    case "close-pairing": qrOpen = false; closeDialog(); return run("cancel_qr");
    case "settings-button": return show(view === "settings" ? "device" : "settings");
    case "back-button": return show(view === "commands" && !d ? "settings" : "device");
    case "sidebar-button": {
      if (narrow()) {
        narrowOpen = !narrowOpen;
        return render();
      }
      return run("set_option", { key: "sidebar", on: !state.settings.sidebar });
    }
    case "turn-on": return run("set_enabled", { on: true });
    case "cancel-dialog": return closeDialog();
    case "send-text": {
      const text = $("text-field").value.trim();
      closeDialog();
      if (text && d) await run("send_text", { id: d.id, text }, `Sent to ${d.name}`);
      return;
    }
    case "do-unpair": closeDialog(); return d && run("unpair", { id: d.id });
    case "rename": return run("rename", { name: $("name-field").value }, "Renamed. Reconnecting your devices…");
    case "save-relay": {
      const address = $("relay-field").value.trim();
      return run("set_relay", { address }, address ? "Relay saved. Reconnecting…" : "Relay removed");
    }
    case "check-update": return run("check_update", { install: state.update.state === "available" });
  }
});

document.addEventListener("change", (e) => {
  const t = e.target;
  const d = current();
  if (t.id === "power-switch") return run("set_enabled", { on: t.checked });
  if (t.id === "autostart") return run("autostart", { on: t.checked });
  if (t.dataset.option) return run("set_option", { key: t.dataset.option, on: t.checked });
  if (t.dataset.app) {
    return run("set_app_muted", { app: t.dataset.app, muted: !t.checked }).then(loadNotificationApps);
  }
  if (t.dataset.seek && d) {
    dragging = false;
    return run("media_control", { id: d.id, player: t.dataset.seek, action: "set_position", value: Number(t.value) });
  }
  if (t.dataset.volume && d) {
    dragging = false;
    return run("media_control", { id: d.id, player: t.dataset.volume, action: "set_volume", value: Number(t.value) });
  }
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !$("modal").hidden && !state?.pairing) {
    if (qrOpen) { qrOpen = false; run("cancel_qr"); }
    closeDialog();
  }
  if (e.key === "Enter" && e.target.id === "relay-field") $("save-relay").click();
  if (e.key === "Enter" && e.target.id === "name-field") $("rename").click();
});

// ----- events from the backend ---------------------------------------------------------------

(async () => {
  // Listen first, then ask for the picture (so the first one isn't missed).
  await listen("state", (s) => {
    state = s;
    render();
  });
  await listen("show-pairing", () => showPairing());
  await listen("files-dropped", (paths) => {
    const d = current();
    if (Views[view]?.onDrop && d) return Views[view].onDrop(paths);
    if (!d || !d.paired || !d.connected) return toast("Open a connected device to send files to it");
    run("send_files", { id: d.id, paths }, (n) => `Sending ${n} file${n === 1 ? "" : "s"} to ${d.name}`);
  });
  await listen("artwork", () => { artCache.clear(); render(); });
  await listen("sms", ([device, thread]) => Views.messages.onSms?.(device, thread));
  await listen("sms-status", ([device, ok, detail]) => Views.messages.onStatus?.(device, ok, detail));
  await listen("files-progress", (p) => Views.files.onProgress?.(p));
  tauri.event.listen("tauri://drag-enter", () => document.body.classList.add("dropping"));
  tauri.event.listen("tauri://drag-leave", () => document.body.classList.remove("dropping"));
  tauri.event.listen("tauri://drag-drop", () => document.body.classList.remove("dropping"));
  window.addEventListener("resize", () => render());
  invoke("refresh");
})();
