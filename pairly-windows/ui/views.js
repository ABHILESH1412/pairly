// Pairly for Windows: the pages that fetch what they show (a phone's texts, contacts and files,
// and the commands phones may run here). Each view draws its page once (`mount`) and then
// keeps it, updating only what changed, so typing and scrolling survive the regular redraws.
// Uses the helpers in app.js (loaded after this file; everything here runs later).

const Views = {};

function pane(html) {
  const div = document.createElement("div");
  div.innerHTML = html;
  return div.firstElementChild;
}

function initials(name) {
  const words = String(name || "?").replace(/[^\p{L}\p{N} ]/gu, "").trim().split(/\s+/);
  const letters = words.length > 1 ? words[0][0] + words[words.length - 1][0] : (words[0] || "?").slice(0, 2);
  return letters.toUpperCase() || "#";
}

// ----- messages ------------------------------------------------------------------------------

Views.messages = {
  full: true,
  title: (d) => `Messages · ${d.name}`,
  device: null,
  conversations: [],
  thread: null, // { thread_id, addresses, names } or { new: true }
  messages: [],
  files: [], // attachments to send
  pictures: new Map(), // part id → data URL

  mount(page, d) {
    this.device = d.id;
    this.thread = null;
    this.messages = [];
    this.files = [];
    page.append(pane(`<div class="split">
      <div class="list-pane" id="m-list-pane">
        <div class="pane-bar">
          <input class="field" id="m-search" placeholder="Search" spellcheck="false">
          <button class="btn primary nowrap" id="m-new" title="New message">+ New</button>
        </div>
        <div class="scroll" id="m-convs"><div class="hint-line">Loading…</div></div>
      </div>
      <div class="thread-pane" id="m-thread-pane">
        <div class="empty small"><div class="big">💬</div><p>Pick a conversation, or start a new one.</p></div>
      </div></div>`));
    $("m-search").addEventListener("input", () => this.drawList());
    $("m-new").addEventListener("click", () => this.compose());
    this.load();
  },

  update(d) {
    // Went offline: say so in the thread's composer.
    const send = $("m-send");
    if (send) send.disabled = !d.connected;
  },

  async load() {
    const id = this.device;
    try {
      const list = await invoke("conversations", { id });
      if (id !== this.device) return;
      this.conversations = list;
      this.drawList();
    } catch (e) {
      const box = $("m-convs");
      if (box) box.innerHTML = `<div class="hint-line">${esc(e)}</div>`;
    }
  },

  label(c) {
    const names = c.addresses.map((a, i) => c.names?.[i] || a);
    return names.join(", ") || "Unknown";
  },

  drawList() {
    const box = $("m-convs");
    if (!box) return;
    const q = ($("m-search")?.value ?? "").trim().toLowerCase();
    const list = this.conversations.filter((c) => !q ||
      this.label(c).toLowerCase().includes(q) || c.addresses.some((a) => a.includes(q)) ||
      c.snippet.toLowerCase().includes(q));
    if (!list.length) {
      box.innerHTML = `<div class="hint-line">${this.conversations.length ? "Nothing matches" : "No conversations"}</div>`;
      return;
    }
    box.innerHTML = list.map((c) => `<button class="conv ${this.thread?.thread_id === c.thread_id ? "selected" : ""} ${c.read ? "" : "unread"}" data-thread="${c.thread_id}">
      <span class="avatar small">${esc(initials(this.label(c)))}</span>
      <span class="grow"><span class="top"><b>${esc(this.label(c))}</b><small>${esc(when(c.date_ms))}</small></span>
      <span class="snippet">${esc(c.snippet)}</span></span></button>`).join("");
    box.querySelectorAll(".conv").forEach((b) => b.addEventListener("click", () => {
      const c = this.conversations.find((x) => String(x.thread_id) === b.dataset.thread);
      if (c) this.open(c);
    }));
  },

  open(c) {
    this.thread = c;
    this.messages = [];
    this.files = [];
    $("m-list-pane").classList.add("narrow-hide");
    this.drawThread();
    this.drawList();
    this.loadMessages(false);
  },

  compose(address = "") {
    this.thread = { new: true, addresses: address ? [address] : [] };
    this.messages = [];
    this.files = [];
    $("m-list-pane")?.classList.add("narrow-hide");
    this.drawThread();
    this.drawList();
    $(address ? "m-text" : "m-to")?.focus();
    // Names to pick from while typing a number.
    invoke("contacts", { id: this.device }).then((list) => {
      const dl = $("m-contacts");
      if (dl) dl.innerHTML = list.flatMap((c) => c.numbers.map((n) =>
        `<option value="${esc(n)}">${esc(c.name)}</option>`)).join("");
    }).catch(() => {});
  },

  drawThread() {
    const t = this.thread;
    const box = $("m-thread-pane");
    if (!box || !t) return;
    const d = current();
    const head = t.new
      ? `<input class="field" id="m-to" list="m-contacts" placeholder="To: phone numbers, separated by commas" value="${esc(t.addresses.join(", "))}" spellcheck="false"><datalist id="m-contacts"></datalist>`
      : `<div class="grow"><b>${esc(this.label(t))}</b><div class="sub">${esc(t.addresses.join(", "))}</div></div>
         ${t.addresses.length === 1 ? `<button class="btn" id="m-call" title="Call from the phone">📞 Call</button>` : ""}`;
    box.innerHTML = `<div class="pane-bar">
        <button class="icon narrow-only" id="m-back" title="Conversations">←</button>${head}</div>
      <div class="scroll msgs" id="m-msgs">${t.new ? "" : `<div class="hint-line">Loading…</div>`}</div>
      <div class="attached" id="m-attached"></div>
      <div class="composer">
        <button class="icon" id="m-attach" title="Add a picture or file">📎</button>
        <textarea class="field" id="m-text" rows="1" placeholder="Text message"></textarea>
        <button class="btn primary" id="m-send" ${d?.connected ? "" : "disabled"}>Send</button>
      </div>`;
    $("m-back").addEventListener("click", () => {
      this.thread = null;
      $("m-list-pane").classList.remove("narrow-hide");
      box.innerHTML = `<div class="empty small"><div class="big">💬</div><p>Pick a conversation, or start a new one.</p></div>`;
      this.drawList();
    });
    $("m-call")?.addEventListener("click", () => run("dial", { id: this.device, number: t.addresses[0] }, `Calling ${this.label(t)} on your phone…`));
    $("m-attach").addEventListener("click", async () => {
      const picked = await tauri.dialog.open({ multiple: true, title: "Add to the message" });
      const paths = picked ? (Array.isArray(picked) ? picked : [picked]) : [];
      this.files.push(...paths);
      this.drawAttached();
    });
    const text = $("m-text");
    text.addEventListener("input", () => {
      text.style.height = "auto";
      text.style.height = Math.min(160, text.scrollHeight) + "px";
    });
    text.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        this.send();
      }
    });
    $("m-send").addEventListener("click", () => this.send());
    this.drawAttached();
  },

  drawAttached() {
    const box = $("m-attached");
    if (!box) return;
    box.innerHTML = this.files.map((f, i) => `<span class="chip">📎 ${esc(f.split(/[\\/]/).pop())}
      <button class="icon tiny" data-unattach="${i}" title="Remove">✕</button></span>`).join("");
    box.querySelectorAll("[data-unattach]").forEach((b) => b.addEventListener("click", () => {
      this.files.splice(Number(b.dataset.unattach), 1);
      this.drawAttached();
    }));
  },

  async loadMessages(older) {
    const t = this.thread;
    if (!t || t.new) return;
    const before = older && this.messages.length ? this.messages[0].date_ms : 0;
    try {
      const page = await invoke("messages", { id: this.device, thread: t.thread_id, before });
      if (this.thread !== t) return;
      if (older) {
        const box = $("m-msgs");
        const fromBottom = box ? box.scrollHeight - box.scrollTop : 0;
        this.messages = [...page, ...this.messages];
        this.drawMessages(page.length >= 50);
        if (box) box.scrollTop = box.scrollHeight - fromBottom;
      } else {
        this.messages = page;
        this.drawMessages(page.length >= 50);
        const box = $("m-msgs");
        if (box) box.scrollTop = box.scrollHeight;
      }
    } catch (e) {
      const box = $("m-msgs");
      if (box) box.innerHTML = `<div class="hint-line">${esc(e)}</div>`;
    }
  },

  drawMessages(more) {
    const box = $("m-msgs");
    if (!box) return;
    const group = this.thread.addresses.length > 1;
    let lastDay = "";
    const items = this.messages.map((m) => {
      const day = new Date(m.date_ms).toDateString();
      const sep = day !== lastDay ? `<div class="day">${esc(new Date(m.date_ms).toLocaleDateString([], { weekday: "long", day: "numeric", month: "long" }))}</div>` : "";
      lastDay = day;
      const who = group && !m.outgoing ? `<div class="who">${esc(this.nameOf(m.address))}</div>` : "";
      const parts = m.attachments.map((a) => a.mime.startsWith("image/")
        ? `<button class="pic" data-part="${a.part_id}" data-name="${esc(a.name)}" data-mime="${esc(a.mime)}">${this.pictures.has(a.part_id) ? `<img src="${esc(this.pictures.get(a.part_id))}" alt="">` : "🖼️ Picture"}</button>`
        : `<button class="chip" data-part="${a.part_id}" data-name="${esc(a.name)}" data-mime="${esc(a.mime)}">📎 ${esc(a.name || a.mime)} · ${humanSize(a.size)}</button>`).join("");
      const pending = m.pending ? `<small>Sending…</small>` : `<small>${esc(new Date(m.date_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }))}</small>`;
      return `${sep}<div class="bubble ${m.outgoing ? "out" : "in"}">${who}${parts}${m.body ? `<div class="body">${esc(m.body)}</div>` : ""}${pending}</div>`;
    });
    box.innerHTML = (more ? `<button class="btn older" id="m-older">Load older messages</button>` : "") +
      (items.join("") || `<div class="hint-line">No messages</div>`);
    $("m-older")?.addEventListener("click", () => this.loadMessages(true));
    box.querySelectorAll("[data-part]").forEach((b) => b.addEventListener("click", () => this.attachment(b)));
    // Show the newest pictures straight away.
    [...box.querySelectorAll(".pic[data-part]")].slice(-6).forEach((b) => {
      if (!this.pictures.has(Number(b.dataset.part))) this.attachment(b);
    });
  },

  nameOf(address) {
    const t = this.thread;
    const i = t.addresses.indexOf(address);
    return (i >= 0 && t.names?.[i]) || address;
  },

  async attachment(b) {
    const part = Number(b.dataset.part);
    try {
      const got = await invoke("sms_attachment", { id: this.device, part, name: b.dataset.name, mime: b.dataset.mime });
      if (got.url) {
        this.pictures.set(part, got.url);
        b.innerHTML = `<img src="${esc(got.url)}" alt="">`;
      } else if (got.path) {
        toast(`Saved to ${got.path}`);
      }
    } catch (e) {
      toast(String(e));
    }
  },

  async send() {
    const t = this.thread;
    const text = $("m-text");
    if (!t || !text) return;
    const body = text.value.trim();
    const addresses = t.new
      ? ($("m-to")?.value ?? "").split(/[,;]/).map((a) => a.trim()).filter(Boolean)
      : t.addresses;
    if (!addresses.length) return toast("Who to? Type a phone number.");
    if (!body && !this.files.length) return;
    const files = this.files;
    try {
      await invoke("send_sms", { id: this.device, addresses, body, files });
    } catch (e) {
      return toast(String(e));
    }
    text.value = "";
    text.style.height = "auto";
    this.files = [];
    this.drawAttached();
    if (t.new) {
      toast("Sending…");
      // The phone files it in a conversation: find it and open it.
      setTimeout(async () => {
        await this.load();
        const c = this.conversations.find((x) => x.addresses.length === addresses.length &&
          addresses.every((a) => x.addresses.some((b) => b.replace(/\D/g, "").endsWith(a.replace(/\D/g, "").slice(-9)))));
        if (c && this.thread === t) this.open(c);
      }, 2500);
    } else {
      this.messages.push({ id: -1, thread_id: t.thread_id, address: "", body, date_ms: Date.now(), outgoing: true, read: true, attachments: [], pending: true });
      this.drawMessages(false);
      const box = $("m-msgs");
      if (box) box.scrollTop = box.scrollHeight;
    }
  },

  // Files dropped on the window go into the message being written.
  onDrop(paths) {
    if (!this.thread) return toast("Open a conversation to send pictures in it");
    this.files.push(...paths);
    this.drawAttached();
  },

  onSms(device, thread) {
    if (device !== this.device || view !== "messages") return;
    this.load();
    if (this.thread && !this.thread.new && this.thread.thread_id === thread) this.loadMessages(false);
  },

  onStatus(device, ok) {
    if (device !== this.device || view !== "messages") return;
    if (ok && this.thread && !this.thread.new) setTimeout(() => this.loadMessages(false), 800);
  },
};

// ----- contacts and calls --------------------------------------------------------------------

Views.contacts = {
  title: (d) => `Contacts · ${d.name}`,
  device: null,
  list: [],

  mount(page, d) {
    this.device = d.id;
    this.list = [];
    page.append(pane(`<div class="page-inner">
      <div id="c-call"></div>
      <div class="card"><div class="row">
        <span>📞</span>
        <input class="field" id="c-number" placeholder="Call a number" spellcheck="false">
        <button class="btn primary" id="c-dial">Call</button>
        <button class="btn" id="c-text">Text</button></div></div>
      <div class="group"><h2 id="c-count">Contacts</h2>
        <input class="field" id="c-search" placeholder="Search contacts" spellcheck="false">
        <div class="card contact-list" id="c-list"><div class="row"><div class="grow sub">Loading…</div></div></div>
      </div></div>`));
    $("c-dial").addEventListener("click", () => this.dial($("c-number").value));
    $("c-text").addEventListener("click", () => this.text($("c-number").value));
    $("c-number").addEventListener("keydown", (e) => { if (e.key === "Enter") this.dial(e.target.value); });
    $("c-search").addEventListener("input", () => this.draw());
    this.update(d);
    this.load();
  },

  update(d) {
    const box = $("c-call");
    if (box) box.innerHTML = callCard(d);
  },

  async load() {
    const id = this.device;
    try {
      const list = await invoke("contacts", { id });
      if (id !== this.device) return;
      this.list = list;
      this.draw();
    } catch (e) {
      const box = $("c-list");
      if (box) box.innerHTML = `<div class="row"><div class="grow sub">${esc(e)}</div></div>`;
    }
  },

  draw() {
    const box = $("c-list");
    if (!box) return;
    const q = ($("c-search")?.value ?? "").trim().toLowerCase();
    const digits = q.replace(/\D/g, "");
    const list = this.list.filter((c) => !q || c.name.toLowerCase().includes(q) ||
      (digits && c.numbers.some((n) => n.replace(/\D/g, "").includes(digits))));
    $("c-count").textContent = `Contacts (${this.list.length})`;
    // Long lists: the first few hundred matches (search narrows them).
    box.innerHTML = list.slice(0, 300).map((c) => `<div class="row contact">
      <span class="avatar small">${esc(initials(c.name))}</span>
      <div class="grow"><b>${esc(c.name)}</b>${c.numbers.map((n) => `<div class="number">
        <span class="sub">${esc(n)}</span>
        <button class="icon tiny" data-dial="${esc(n)}" title="Call">📞</button>
        <button class="icon tiny" data-sms="${esc(n)}" title="Text">💬</button></div>`).join("")}</div>
    </div>`).join("") || `<div class="row"><div class="grow sub">${this.list.length ? "Nothing matches" : "No contacts"}</div></div>`;
    box.querySelectorAll("[data-dial]").forEach((b) => b.addEventListener("click", () => this.dial(b.dataset.dial)));
    box.querySelectorAll("[data-sms]").forEach((b) => b.addEventListener("click", () => this.text(b.dataset.sms)));
  },

  dial(number) {
    number = String(number || "").trim();
    if (!number) return toast("Type a number to call");
    run("dial", { id: this.device, number }, `Calling ${number} on your phone…`);
  },

  text(number) {
    number = String(number || "").trim();
    view = "messages";
    render(true);
    Views.messages.compose(number);
  },
};

// ----- the phone's files ---------------------------------------------------------------------

Views.files = {
  title: (d) => `Files · ${d.name}`,
  device: null,
  path: "",
  entries: [],

  mount(page, d) {
    this.device = d.id;
    this.path = "";
    page.append(pane(`<div class="page-inner">
      <div class="card"><div class="row toolbar">
        <button class="icon" id="f-up" title="Up a folder">⬆</button>
        <div class="grow crumbs" id="f-crumbs"></div>
        <button class="icon" id="f-refresh" title="Refresh">⟳</button>
        <button class="btn" id="f-mkdir">New folder</button>
        <button class="btn primary" id="f-upload">Upload</button></div>
        <div class="row" id="f-progress" hidden><div class="grow"><div class="sub" id="f-progress-text"></div>
          <progress id="f-progress-bar" max="1" value="0"></progress></div></div></div>
      <div class="card file-list" id="f-list"></div>
      <p class="sub center">Drop files on this window to put them in this folder. Downloads go to Downloads\\Pairly.</p>
    </div>`));
    $("f-up").addEventListener("click", () => this.go(this.path.split("/").slice(0, -1).join("/")));
    $("f-refresh").addEventListener("click", () => this.load());
    $("f-mkdir").addEventListener("click", () => this.mkdir());
    $("f-upload").addEventListener("click", async () => {
      const picked = await tauri.dialog.open({ multiple: true, title: "Upload to the phone" });
      const paths = picked ? (Array.isArray(picked) ? picked : [picked]) : [];
      if (paths.length) this.upload(paths);
    });
    this.load();
  },

  go(path) {
    this.path = path.replace(/^\/+|\/+$/g, "");
    this.load();
  },

  join(name) {
    return this.path ? `${this.path}/${name}` : name;
  },

  async load() {
    const id = this.device, path = this.path;
    const crumbs = $("f-crumbs");
    if (crumbs) {
      const parts = path ? path.split("/") : [];
      crumbs.innerHTML = [`<button class="crumb" data-go="">Phone</button>`,
        ...parts.map((p, i) => `<span>›</span><button class="crumb" data-go="${esc(parts.slice(0, i + 1).join("/"))}">${esc(p)}</button>`)].join("");
      crumbs.querySelectorAll("[data-go]").forEach((b) => b.addEventListener("click", () => this.go(b.dataset.go)));
    }
    $("f-up").disabled = !path;
    const box = $("f-list");
    if (box) box.innerHTML = `<div class="row"><div class="grow sub">Loading…</div></div>`;
    try {
      const entries = await invoke("files_list", { id, path });
      if (id !== this.device || path !== this.path) return;
      this.entries = entries;
      this.draw();
    } catch (e) {
      if (box) box.innerHTML = `<div class="row"><div class="grow sub">${esc(e)}</div></div>`;
    }
  },

  draw() {
    const box = $("f-list");
    if (!box) return;
    const icon = (e) => e.dir ? "📁" : /\.(jpe?g|png|gif|webp|heic|bmp)$/i.test(e.name) ? "🖼️"
      : /\.(mp4|mkv|mov|webm|3gp)$/i.test(e.name) ? "🎞️" : /\.(mp3|m4a|aac|ogg|opus|flac|wav)$/i.test(e.name) ? "🎵"
      : /\.(pdf|docx?|xlsx?|pptx?|txt|odt)$/i.test(e.name) ? "📄" : /\.apk$/i.test(e.name) ? "📦" : "📄";
    box.innerHTML = this.entries.map((e, i) => `<div class="row file ${e.dir ? "dir" : ""}" data-i="${i}">
      <span class="file-icon">${icon(e)}</span>
      <div class="grow"><b>${esc(e.name)}</b><div class="sub">${e.dir ? "Folder" : humanSize(e.size)}${e.modified_ms ? " · " + esc(when(e.modified_ms)) : ""}</div></div>
      ${e.dir ? "" : `<button class="icon tiny" data-f="download" title="Download">⬇</button>`}
      <button class="icon tiny" data-f="rename" title="Rename">✎</button>
      <button class="icon tiny" data-f="delete" title="Delete">🗑</button></div>`).join("")
      || `<div class="row"><div class="grow sub">This folder is empty</div></div>`;
    box.querySelectorAll(".row.file").forEach((row) => row.addEventListener("click", (ev) => {
      const e = this.entries[Number(row.dataset.i)];
      const act = ev.target.closest("[data-f]")?.dataset.f;
      if (act === "download") return this.download(e);
      if (act === "rename") return this.rename(e);
      if (act === "delete") return this.remove(e);
      if (e.dir) this.go(this.join(e.name));
      else this.download(e);
    }));
  },

  async download(e) {
    const path = this.join(e.name);
    this.progress(`Downloading ${e.name}…`, 0, e.size);
    try {
      const saved = await invoke("files_download", { id: this.device, path, size: e.size });
      this.progress(null);
      openDialog(`<h2>Downloaded</h2><p>${esc(e.name)} is in Downloads\\Pairly.</p>
        <div class="actions"><button class="btn" id="cancel-dialog">Close</button>
        <button class="btn" data-open-folder="${esc(saved)}">Show in Folder</button>
        <button class="btn primary" data-open-file="${esc(saved)}">Open</button></div>`);
      document.querySelector("[data-open-folder]")?.addEventListener("click", (ev) => { closeDialog(); invoke("open_path", { path: ev.target.dataset.openFolder, folder: true }); });
      document.querySelector("[data-open-file]")?.addEventListener("click", (ev) => { closeDialog(); invoke("open_path", { path: ev.target.dataset.openFile, folder: false }); });
    } catch (err) {
      this.progress(null);
      toast(String(err));
    }
  },

  async upload(paths) {
    const n = paths.length;
    this.progress(`Uploading ${n} file${n === 1 ? "" : "s"}…`, 0, 1);
    try {
      const sent = await invoke("files_upload", { id: this.device, paths, dir: this.path });
      toast(`Uploaded ${sent} file${sent === 1 ? "" : "s"}`);
    } catch (e) {
      toast(String(e));
    }
    this.progress(null);
    this.load();
  },

  onDrop(paths) {
    this.upload(paths);
  },

  mkdir() {
    openDialog(`<h2>New folder</h2><input class="field" id="f-name" placeholder="Folder name" spellcheck="false">
      <div class="actions"><button class="btn" id="cancel-dialog">Cancel</button><button class="btn primary" id="f-do">Create</button></div>`);
    const go = async () => {
      const name = $("f-name").value.trim();
      closeDialog();
      if (!name) return;
      await run("files_op", { id: this.device, op: "mkdir", path: this.join(name), to: "" });
      this.load();
    };
    $("f-do").addEventListener("click", go);
    $("f-name").addEventListener("keydown", (e) => { if (e.key === "Enter") go(); });
    $("f-name").focus();
  },

  rename(e) {
    openDialog(`<h2>Rename</h2><input class="field" id="f-name" value="${esc(e.name)}" spellcheck="false">
      <div class="actions"><button class="btn" id="cancel-dialog">Cancel</button><button class="btn primary" id="f-do">Rename</button></div>`);
    const go = async () => {
      const name = $("f-name").value.trim();
      closeDialog();
      if (!name || name === e.name) return;
      await run("files_op", { id: this.device, op: "rename", path: this.join(e.name), to: this.join(name) });
      this.load();
    };
    $("f-do").addEventListener("click", go);
    $("f-name").addEventListener("keydown", (ev) => { if (ev.key === "Enter") go(); });
    const field = $("f-name");
    field.focus();
    field.setSelectionRange(0, e.dir ? e.name.length : Math.max(0, e.name.lastIndexOf(".")) || e.name.length);
  },

  remove(e) {
    openDialog(`<h2>Delete ${esc(e.name)}?</h2><p>${e.dir ? "The folder and everything in it are" : "It's"} deleted from the phone.</p>
      <div class="actions"><button class="btn" id="cancel-dialog">Cancel</button><button class="btn primary" id="f-do">Delete</button></div>`);
    $("f-do").addEventListener("click", async () => {
      closeDialog();
      await run("files_op", { id: this.device, op: "delete", path: this.join(e.name), to: "" });
      this.load();
    });
  },

  progress(text, done, total) {
    const row = $("f-progress");
    if (!row) return;
    row.hidden = text === null;
    if (text === null) return;
    $("f-progress-text").textContent = text;
    $("f-progress-bar").max = Math.max(1, total);
    $("f-progress-bar").value = done;
  },

  onProgress(p) {
    if (p.device !== this.device || view !== "files") return;
    const name = p.path.split("/").pop();
    this.progress(`${p.upload ? "Uploading" : "Downloading"} ${name}: ${humanSize(p.done)} of ${humanSize(p.total)}`, p.done, p.total);
  },
};

// ----- commands phones may run here ----------------------------------------------------------

Views.commands = {
  title: () => "Commands",

  mount(page) {
    page.append(pane(`<div class="page-inner">
      <p class="sub">Your phone can run these on this PC (from its Commands screen). Each run shows a notification here.
        They run with <code>cmd /C</code>, in your user folder, and are stopped after a minute.</p>
      <div class="card" id="k-list"></div>
      <div class="group"><h2>Add a command</h2><div class="card">
        <div class="row"><div class="grow"><b>Name</b><input class="field" id="k-name" placeholder="Lock and mute" maxlength="80"></div></div>
        <div class="row"><div class="grow"><b>Command line</b><input class="field mono" id="k-line" placeholder="rundll32.exe user32.dll,LockWorkStation" spellcheck="false"></div>
          <button class="btn primary" id="k-add">Add</button></div>
      </div></div>
      <div class="group"><h2>Ideas</h2><div class="card">
        ${[["Mute", "powershell -c \"(New-Object -ComObject WScript.Shell).SendKeys([char]173)\""],
           ["Sleep", "rundll32.exe powrprof.dll,SetSuspendState 0,1,0"],
           ["Open Downloads", "explorer shell:Downloads"]].map(([n, c]) => `<div class="row"><div class="grow"><b>${esc(n)}</b><div class="sub mono">${esc(c)}</div></div>
           <button class="btn" data-idea-name="${esc(n)}" data-idea-line="${esc(c)}">Use</button></div>`).join("")}
      </div></div></div>`));
    $("k-add").addEventListener("click", () => this.add());
    $("k-line").addEventListener("keydown", (e) => { if (e.key === "Enter") this.add(); });
    page.querySelectorAll("[data-idea-name]").forEach((b) => b.addEventListener("click", () => {
      $("k-name").value = b.dataset.ideaName;
      $("k-line").value = b.dataset.ideaLine;
    }));
    this.load();
  },

  async load() {
    const list = await invoke("list_commands").catch(() => []);
    const box = $("k-list");
    if (!box) return;
    box.innerHTML = list.map((c) => `<div class="row"><span>⌨️</span><div class="grow"><b>${esc(c.name)}</b><div class="sub mono">${esc(c.command)}</div></div>
      <button class="btn danger" data-remove="${esc(c.id)}">Remove</button></div>`).join("")
      || `<div class="row"><div class="grow sub">No commands yet</div></div>`;
    box.querySelectorAll("[data-remove]").forEach((b) => b.addEventListener("click", async () => {
      await run("remove_command", { id: b.dataset.remove });
      this.load();
    }));
  },

  async add() {
    const name = $("k-name").value, command = $("k-line").value;
    const ok = await run("add_command", { name, command });
    if (ok) {
      $("k-name").value = "";
      $("k-line").value = "";
      this.load();
    }
  },
};
