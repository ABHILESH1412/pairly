// Pairly for Windows: the window. The backend sends the whole picture as one `state` event
// whenever anything changes; this file draws it and turns clicks into backend commands.

const tauri = window.__TAURI__;
const invoke = (cmd, args) => tauri.core.invoke(cmd, args);
const listen = (name, f) => tauri.event.listen(name, (e) => f(e.payload));

let state = null;
let selected = null;
let view = "device"; // "device" | "settings"
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

// ----- the device list -----------------------------------------------------------------------

function renderSidebar() {
  const list = $("device-list");
  const paired = state.devices.filter((d) => d.paired);
  const nearby = state.devices.filter((d) => !d.paired);
  const row = (d) => {
    const status = d.paused
      ? `<span class="paused-label">❚❚ Paused</span>`
      : !d.paired ? `<span>Available to pair</span>`
      : d.connected ? `<span class="on"><span class="dot"></span>Connected</span>`
      : `<span class="off"><span class="dot"></span>Offline</span>`;
    const icon = ["phone", "tablet"].includes(d.kind) || !d.kind ? "📱" : "💻";
    return `<button class="device-row ${d.id === selected && view === "device" ? "selected" : ""}" data-id="${d.id}">
      <span class="avatar">${icon}</span>
      <span><div class="name">${esc(d.name)}</div><div class="sub">${status}</div></span></button>`;
  };
  list.innerHTML =
    (paired.length ? `<div class="section">Your devices</div>${paired.map(row).join("")}` : "") +
    (nearby.length ? `<div class="section">Nearby</div>${nearby.map(row).join("")}` : "");
  list.querySelectorAll(".device-row").forEach((b) => b.addEventListener("click", () => {
    selected = b.dataset.id;
    view = "device";
    narrowOpen = false;
    render();
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
      <div class="sub">${esc(d.name)} can't reach this PC and this PC can't reach it. It stays paired.</div></div>
      <button class="btn primary" data-action="resume">Resume</button></div></div>`;
  } else {
    const on = d.connected;
    const phone = d.kind === "phone" || d.kind === "tablet";
    const ringing = state.ringing.includes(d.id);
    body = `<div class="tiles">
      ${tile("📤", "Send Files", "Or drop them on this window", "blue", on, "files")}
      ${tile("🔗", "Link or Text", "Opens or copies there", "blue", on, "text")}
      ${tile("📋", "Clipboard", "Send what you copied", "pink", on, "clipboard")}
      ${ringing ? tile("🔕", "Stop Ringing", "Ringing now…", "red", on, "ring-off")
                : tile("🔔", phone ? "Find My Phone" : "Ring It", "Rings, even on silent", "green", on, "ring-on")}
      ${tile("👋", "Ping", "Show a notification", "green", on, "ping")}
      ${phone ? tile("🔒", "Lock Phone", "Lock its screen now", "gray", on, "lock") : ""}
      ${phone ? tile("⏻", "Power Off", "Power off or restart", "red", on, "power") : ""}
      ${tile("📁", "Received Files", "Open the Pairly folder", "amber", true, "downloads")}
    </div>`;
  }
  const transfers = state.transfers.filter((t) => t.device === d.id && !/^(done|cancelled|failed)/.test(t.state));
  const transferRows = transfers.map((t) => `<div class="row">
      <span>${t.incoming ? "📥" : "📤"}</span>
      <div class="grow"><b>${esc(t.name)}</b><div class="sub">${t.state === "waiting" ? "Waiting…" : `${Math.round(100 * t.bytes / Math.max(1, t.size))}%`}</div></div>
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
  const sw = (key, on) => `<label class="switch"><input type="checkbox" data-option="${key}" ${on ? "checked" : ""}><span></span></label>`;
  return `<div class="page-inner">
    <div class="group"><h2>Appearance</h2><div class="card"><div class="row"><div class="grow"><b>Style</b>
      <div class="sub">Light, dark, or the same as Windows</div></div>
      <div class="segments">${seg("system", "System")}${seg("light", "Light")}${seg("dark", "Dark")}</div></div></div></div>
    <div class="group"><h2>This PC</h2><div class="card">
      <div class="row"><div class="grow"><b>Device name</b><div class="sub">Your phone shows this name</div>
        <input class="field" id="name-field" value="${esc(state.me ? state.me[1] : s.name ?? "")}" maxlength="64"></div>
        <button class="btn" id="rename">Save</button></div>
      <div class="row"><div class="grow"><b>Start with Windows</b><div class="sub">Pairly runs in the tray after you sign in</div></div>
        <label class="switch"><input type="checkbox" id="autostart"><span></span></label></div>
      <div class="row"><div class="grow"><b>Send copied text automatically</b><div class="sub">What you copy here goes to your connected devices</div></div>
        ${sw("clipboard_auto", s.clipboard_auto)}</div>
      <div class="row"><div class="grow"><b>Received files</b><div class="sub">Saved in Downloads\\Pairly</div></div>
        <button class="btn" data-action="downloads">Open</button></div>
      ${state.me ? `<div class="row"><div class="grow">Device ID<div class="sub">${esc(state.me[0])}</div></div></div>` : ""}
    </div></div>
    <div class="group"><h2>Updates</h2><div class="card">
      <div class="row"><div class="grow"><b>Install updates automatically</b><div class="sub">New versions from Pairly's GitHub releases, installed only when signed by the project</div></div>
        ${sw("auto_update", s.auto_update)}</div>
      <div class="row"><div class="grow"><b>Pairly</b><div class="sub">${esc(status)}</div></div>
        <button class="btn" id="check-update" ${["checking", "downloading"].includes(u.state) ? "disabled" : ""}>${u.state === "available" ? "Install" : "Check now"}</button></div>
    </div></div>
    <div class="group"><h2>About</h2><div class="card">
      <div class="row"><div class="grow">Version<div class="sub">${esc(state.version)}</div></div></div>
      <div class="row"><div class="grow">Licence<div class="sub">GPL-3.0</div></div></div>
    </div></div>
  </div>`;
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
  openDialog(`<h2>Pair with ${esc(p.name)}?</h2>
    <p>Check that ${esc(p.name)} shows the same code:</p>
    <div class="code">${esc(p.code)}</div>
    <div class="actions"><button class="btn" data-confirm="no">Cancel</button>
      <button class="btn primary" data-confirm="yes">Codes match</button></div>`);
}

// ----- drawing -------------------------------------------------------------------------------

function render() {
  if (!state) return;
  applyTheme();
  renderSidebar();
  const d = state.devices.find((x) => x.id === selected) ??
    state.devices.find((x) => x.paired) ?? null;
  if (d && !selected) selected = d.id;
  const page = $("page");
  // Keep what's being typed in the name box across the regular redraws.
  const typing = document.activeElement?.id === "name-field" ? document.activeElement.value : null;
  if (view === "settings") {
    $("page-title").textContent = "Settings";
    page.innerHTML = settingsPage();
    run("autostart", {}).then((on) => { const a = $("autostart"); if (a) a.checked = !!on; });
    if (typing !== null) {
      const field = $("name-field");
      field.value = typing;
      field.focus();
    }
  } else if (d && state.settings.enabled) {
    $("page-title").textContent = d.name;
    page.innerHTML = devicePage(d);
  } else {
    $("page-title").textContent = "Pairly";
    page.innerHTML = emptyPage();
  }
  if (state.pairing) renderCodePrompt();
  else if (qrOpen) renderPairing();
}

// ----- clicks --------------------------------------------------------------------------------

function current() {
  return state?.devices.find((x) => x.id === selected);
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
  if (!t) return;
  const d = current();
  const action = t.dataset.action;
  if (action && d) {
    switch (action) {
      case "files": return pickFiles(d);
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
    }
  }
  if (action === "downloads") return run("open_downloads");
  if (t.dataset.transfer) return run("cancel_transfer", { id: Number(t.dataset.transfer) });
  if (t.dataset.pair) return run("pair", { id: t.dataset.pair });
  if (t.dataset.confirm) {
    const p = state.pairing;
    closeDialog();
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
    case "settings-button": view = view === "settings" ? "device" : "settings"; narrowOpen = false; return render();
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
    case "check-update": return run("check_update", { install: state.update.state === "available" });
  }
});

document.addEventListener("change", (e) => {
  const t = e.target;
  if (t.id === "power-switch") return run("set_enabled", { on: t.checked });
  if (t.id === "autostart") return run("autostart", { on: t.checked });
  if (t.dataset.option) return run("set_option", { key: t.dataset.option, on: t.checked });
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !$("modal").hidden && !state?.pairing) {
    if (qrOpen) { qrOpen = false; run("cancel_qr"); }
    closeDialog();
  }
});

// ----- events from the backend ---------------------------------------------------------------

listen("state", (s) => {
  state = s;
  render();
});
listen("show-pairing", () => showPairing());
listen("files-dropped", (paths) => {
  const d = current();
  if (!d || !d.paired || !d.connected) return toast("Open a connected device to send files to it");
  run("send_files", { id: d.id, paths }, (n) => `Sending ${n} file${n === 1 ? "" : "s"} to ${d.name}`);
});
tauri.event.listen("tauri://drag-enter", () => document.body.classList.add("dropping"));
tauri.event.listen("tauri://drag-leave", () => document.body.classList.remove("dropping"));
tauri.event.listen("tauri://drag-drop", () => document.body.classList.remove("dropping"));

window.addEventListener("resize", () => render());
invoke("refresh");
