// A phone's (or another PC's) screen: its H.264 video, decoded on the GPU (WebCodecs), and what
// you do here sent back: taps, swipes, keys and text to a phone; the mouse and keyboard as they
// are to a PC. Messages from the backend: [kind][payload],
// kind 1 started [width u32][height u32], 2 frame [flags][access unit] (flag 1 key frame,
// 2 codec setup), 3 stopped [reason].

const tauri = window.__TAURI__;
const invoke = (cmd, args) => tauri.core.invoke(cmd, args);
const params = new URLSearchParams(location.search);
const id = params.get("id");
const name = params.get("name") || "Phone";
const pc = params.get("pc") === "1";

const canvas = document.getElementById("video");
const ctx = canvas.getContext("2d");
const statusBox = document.getElementById("status");
const statusText = document.getElementById("status-text");

if (pc) {
  document.title = "PC screen";
  document.querySelector(".status .big").textContent = "🖥️";
  statusText.textContent = `Asking ${name} to share its screen…`;
  canvas.classList.add("pc");
}

function status(text) {
  statusBox.hidden = !text;
  if (text) statusText.textContent = text;
}

// ----- video ---------------------------------------------------------------------------------

let decoder = null;
let config = null; // the latest SPS/PPS, put in front of key frames that lack them
let waitingForKey = true;
let timestamp = 0;

/** NAL units of an Annex B access unit: [type, start, end]. */
function nals(data) {
  const out = [];
  let i = 0, start = -1;
  while (i + 2 < data.length) {
    if (data[i] === 0 && data[i + 1] === 0 && (data[i + 2] === 1 || (data[i + 2] === 0 && data[i + 3] === 1))) {
      const len = data[i + 2] === 1 ? 3 : 4;
      if (start >= 0) out.push([data[start] & 0x1f, start, i]);
      start = i + len;
      i += len;
    } else i++;
  }
  if (start >= 0 && start < data.length) out.push([data[start] & 0x1f, start, data.length]);
  return out;
}

/** "avc1.PPCCLL" from the stream's SPS. */
function codecOf(data) {
  const sps = nals(data).find(([t]) => t === 7);
  if (!sps) return null;
  const s = sps[1];
  const hex = (v) => v.toString(16).padStart(2, "0");
  return `avc1.${hex(data[s + 1])}${hex(data[s + 2])}${hex(data[s + 3])}`;
}

function newDecoder(codec) {
  decoder?.close?.();
  decoder = new VideoDecoder({
    output: (frame) => {
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      ctx.drawImage(frame, 0, 0);
      frame.close();
      if (canvas.classList.contains("idle")) {
        canvas.classList.remove("idle");
        status(null);
      }
    },
    error: (e) => {
      console.warn("decoder", e);
      decoder = null;
      waitingForKey = true;
    },
  });
  decoder.configure({ codec, optimizeForLatency: true, hardwareAcceleration: "no-preference" });
}

function onFrame(flags, data) {
  const key = (flags & 1) !== 0, setup = (flags & 2) !== 0;
  if (setup) {
    config = data;
    return;
  }
  const hasSps = key && nals(data).some(([t]) => t === 7);
  if (key && !hasSps && config) {
    const joined = new Uint8Array(config.length + data.length);
    joined.set(config);
    joined.set(data, config.length);
    data = joined;
  }
  if (key) {
    const codec = codecOf(data);
    if (codec && (!decoder || decoder.state === "closed" || decoder.codec !== codec)) {
      try {
        newDecoder(codec);
        decoder.codec = codec;
      } catch (e) {
        status(`This PC can't show the video: ${e}`);
        return;
      }
    }
    waitingForKey = false;
  }
  if (!decoder || waitingForKey) return;
  // Falling behind: skip to the next key frame rather than lag.
  if (!key && decoder.decodeQueueSize > 10) {
    waitingForKey = true;
    return;
  }
  timestamp += 33333;
  try {
    decoder.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp, data }));
  } catch (e) {
    console.warn("decode", e);
    waitingForKey = true;
  }
}

/** The other device's sound, played as it comes: each piece scheduled right after the last,
 *  a little ahead of now; when it falls more than a moment behind, it starts again from now
 *  (so the sound stays with the picture). */
const sound = {
  context: null,
  next: 0,
  play(rate, channels, pcm) {
    if (!this.context) {
      try { this.context = new AudioContext({ latencyHint: "interactive" }); } catch { return; }
    }
    const ctx = this.context;
    if (ctx.state === "suspended") ctx.resume().catch(() => {});
    const frames = Math.floor(pcm.length / 2 / channels);
    if (!frames) return;
    const buffer = ctx.createBuffer(channels, frames, rate);
    const view = new DataView(pcm.buffer, pcm.byteOffset, pcm.byteLength);
    for (let c = 0; c < channels; c++) {
      const out = buffer.getChannelData(c);
      for (let i = 0; i < frames; i++) out[i] = view.getInt16((i * channels + c) * 2, true) / 32768;
    }
    const source = ctx.createBufferSource();
    source.buffer = buffer;
    source.connect(ctx.destination);
    const now = ctx.currentTime;
    if (this.next < now + 0.02 || this.next > now + 0.3) this.next = now + 0.06;
    source.start(this.next);
    this.next += buffer.duration;
  },
  stop() {
    this.context?.close().catch(() => {});
    this.context = null;
  },
};

// Browsers keep sound paused until the page is used: the first click or key starts it.
for (const type of ["pointerdown", "keydown"]) {
  window.addEventListener(type, () => sound.context?.resume().catch(() => {}), true);
}

const channel = new tauri.core.Channel();
channel.onmessage = (msg) => {
  const bytes = msg instanceof ArrayBuffer ? new Uint8Array(msg) : new Uint8Array(msg);
  if (!bytes.length) return;
  const kind = bytes[0];
  if (kind === 1 && bytes.length >= 9) {
    const view = new DataView(bytes.buffer, bytes.byteOffset);
    canvas.width = view.getUint32(1);
    canvas.height = view.getUint32(5);
    status(`Starting ${name}'s screen…`);
  } else if (kind === 2 && bytes.length > 2) {
    onFrame(bytes[1], bytes.subarray(2));
  } else if (kind === 4 && bytes.length > 6) {
    const view = new DataView(bytes.buffer, bytes.byteOffset);
    sound.play(view.getUint32(1), bytes[5], bytes.subarray(6));
  } else if (kind === 3) {
    sound.stop();
    const reason = new TextDecoder().decode(bytes.subarray(1));
    canvas.classList.add("idle");
    status(`${name} stopped sharing${reason ? `: ${reason}` : ""}. Close this window, or open it again from Pairly.`);
    decoder?.close?.();
    decoder = null;
  }
};

if (!("VideoDecoder" in window)) {
  status("This version of WebView2 can't decode video. Update Microsoft Edge WebView2 and try again.");
} else {
  invoke("screen_watch", { id, channel })
    .then(() => status(pc ? `Waiting for ${name}…` : `Waiting for ${name}… Accept the request on the phone.`))
    .catch((e) => status(String(e)));
}

// ----- control -------------------------------------------------------------------------------

const send = (input) => invoke("screen_input", { id, input }).catch(() => {});

/** Where on the video, as fractions. */
function at(e) {
  const r = canvas.getBoundingClientRect();
  const clamp = (v) => Math.min(1, Math.max(0, v));
  return [clamp((e.clientX - r.left) / r.width), clamp((e.clientY - r.top) / r.height)];
}

function inside(e) {
  const r = canvas.getBoundingClientRect();
  return e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
}

/** A phone: clicks and drags become taps, long presses and swipes; keys and text. */
function phoneControls() {
  const LONG_PRESS_MS = 500, TAP_SLOP = 8, MAX_POINTS = 120;
  let press = null;

  canvas.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    canvas.setPointerCapture(e.pointerId);
    press = { t: performance.now(), x: e.clientX, y: e.clientY, points: [at(e)], moved: false };
  });

  canvas.addEventListener("pointermove", (e) => {
    if (!press) return;
    if (Math.hypot(e.clientX - press.x, e.clientY - press.y) > TAP_SLOP) press.moved = true;
    if (press.moved && press.points.length < MAX_POINTS) press.points.push(at(e));
  });

  canvas.addEventListener("pointerup", (e) => {
    if (!press || e.button !== 0) return;
    const p = press;
    press = null;
    const ms = Math.round(performance.now() - p.t);
    if (!p.moved) {
      const [x, y] = p.points[0];
      send({ kind: ms >= LONG_PRESS_MS ? "long_press" : "tap", x, y });
    } else {
      p.points.push(at(e));
      send({ kind: "swipe", points: p.points, duration_ms: Math.max(50, ms) });
    }
  });

  // Right click: Back, as on many phones' mice.
  canvas.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    send({ kind: "key", key: "back" });
  });

  // The wheel scrolls: a short swipe from the pointer.
  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    const [x, y] = at(e);
    const steps = Math.max(-3, Math.min(3, e.deltaY / 100));
    const to = Math.min(0.98, Math.max(0.02, y - steps * 0.12));
    send({ kind: "swipe", points: [[x, y], [x, to]], duration_ms: 120 });
  }, { passive: false });

  // Typing goes to the phone's focused text field (letters gathered briefly into one message).
  let typed = "", typedTimer = null;
  function flushText() {
    if (typed) send({ kind: "text", text: typed });
    typed = "";
    typedTimer = null;
  }
  window.addEventListener("keydown", (e) => {
    const keys = { Enter: "enter", Backspace: "backspace", Delete: "delete", ArrowLeft: "left",
      ArrowRight: "right", ArrowUp: "up", ArrowDown: "down", Escape: "back" };
    if (e.ctrlKey && e.key.toLowerCase() === "v") {
      e.preventDefault();
      navigator.clipboard?.readText?.().then((t) => t && send({ kind: "text", text: t.slice(0, 1000) })).catch(() => {});
      return;
    }
    if (e.ctrlKey || e.altKey || e.metaKey) return;
    if (keys[e.key]) {
      e.preventDefault();
      flushText();
      send({ kind: "key", key: keys[e.key] });
    } else if (e.key.length === 1) {
      e.preventDefault();
      typed += e.key;
      if (!typedTimer) typedTimer = setTimeout(flushText, 60);
    }
  });
}

const NO_MODIFIERS = { ctrl: false, alt: false, shift: false, super: false };
const SPECIAL_KEYS = {
  Enter: "enter", Backspace: "backspace", Delete: "delete", Tab: "tab", Escape: "escape",
  ArrowLeft: "left", ArrowRight: "right", ArrowUp: "up", ArrowDown: "down", Home: "home",
  End: "end", PageUp: "page_up", PageDown: "page_down", AudioVolumeUp: "volume_up",
  AudioVolumeDown: "volume_down", AudioVolumeMute: "mute",
};

/** What a key press sends to another PC, or null: special keys and shortcuts with their
 *  modifiers, plain characters as text (Shift is already in them). */
function pcKey(e) {
  const modifiers = { ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, super: e.metaKey };
  // AltGr comes as Ctrl+Alt: what it types is a character, not a shortcut.
  const altGr = e.getModifierState?.("AltGraph");
  const shortcut = !altGr && (e.ctrlKey || e.altKey || e.metaKey);
  if (SPECIAL_KEYS[e.key]) return { kind: "combo", key: SPECIAL_KEYS[e.key], modifiers };
  const f = /^F(\d{1,2})$/.exec(e.key);
  if (f && +f[1] >= 1 && +f[1] <= 24) return { kind: "combo", key: { f: +f[1] }, modifiers };
  if (e.key === " " && shortcut) return { kind: "combo", key: "space", modifiers };
  if ([...e.key].length !== 1) return null; // a modifier, a dead key, an IME…
  if (!shortcut) return { kind: "combo", text: e.key, modifiers: NO_MODIFIERS };
  // Ctrl+Shift+T comes as "T", and Ctrl+Shift+1 as "!": send the key itself, Shift held.
  const digit = /^Digit(\d)$/.exec(e.code);
  const text = /^[a-z]$/i.test(e.key) ? e.key.toLowerCase() : digit ? digit[1] : e.key.toLowerCase();
  return { kind: "combo", text, modifiers };
}

/** Another PC: the mouse and keyboard go there as they are. */
function pcControls() {
  const BUTTONS = ["left", "middle", "right"];
  const held = new Map(); // button → where it went down
  let moveTo = null, moveQueued = false;

  // Moves: the latest one each frame.
  function move(e) {
    if (!held.size && !inside(e)) return;
    moveTo = at(e);
    if (moveQueued) return;
    moveQueued = true;
    requestAnimationFrame(() => {
      moveQueued = false;
      if (moveTo) send({ kind: "pointer", x: moveTo[0], y: moveTo[1] });
      moveTo = null;
    });
  }
  window.addEventListener("mousemove", move);

  canvas.addEventListener("mousedown", (e) => {
    const button = BUTTONS[e.button];
    if (!button) return;
    e.preventDefault(); // no middle-click autoscroll, no text selection
    canvas.focus();
    const [x, y] = at(e);
    held.set(button, [x, y]);
    send({ kind: "button", x, y, button, action: "press" });
  });
  // Released anywhere (a drag may end outside the window).
  window.addEventListener("mouseup", (e) => {
    const button = BUTTONS[e.button];
    if (!button || !held.has(button)) return;
    held.delete(button);
    const [x, y] = at(e);
    send({ kind: "button", x, y, button, action: "release" });
  });
  // Switching away mid-drag mustn't leave a button down there.
  window.addEventListener("blur", () => {
    for (const [button, [x, y]] of held) send({ kind: "button", x, y, button, action: "release" });
    held.clear();
  });
  canvas.addEventListener("contextmenu", (e) => e.preventDefault());

  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    // In wheel steps: about 100 pixels or 3 lines each.
    const per = e.deltaMode === 1 ? 3 : e.deltaMode === 2 ? 1 / 10 : 100;
    send({ kind: "wheel", dx: e.deltaX / per, dy: e.deltaY / per });
  }, { passive: false });

  window.addEventListener("keydown", (e) => {
    // Nothing here acts on keys (no reloading, no browser shortcuts): they're all for there.
    e.preventDefault();
    const input = pcKey(e);
    if (input) send(input);
  });

  // Keys this PC keeps for itself (the Windows key, Alt+Tab), as buttons; Control mode and
  // full screen hand them over entirely.
  const keys = document.querySelector(".keys");
  keys.innerHTML = `
    <button data-combo="start" title="Start menu (Windows key)">⊞</button>
    <button data-combo="switch" title="Switch windows (Alt+Tab)">⧉</button>
    <button id="control" title="Control mode: the Windows key, Alt+Tab and every shortcut go to this PC (Ctrl+Alt to release)">⌨</button>
    <button id="fullscreen" title="Full screen (Control mode on)">⛶</button>`;
  controlMode(keys);
  const combos = {
    start: { kind: "combo", modifiers: { ...NO_MODIFIERS, super: true } },
    switch: { kind: "combo", key: "tab", modifiers: { ...NO_MODIFIERS, alt: true } },
  };
  keys.querySelectorAll("button[data-combo]").forEach((b) => {
    b.tabIndex = -1;
    b.addEventListener("mousedown", (e) => e.preventDefault()); // keep the keyboard on the screen
    b.addEventListener("click", () => send(combos[b.dataset.combo]));
  });
}

/** Control mode: Windows' own shortcuts go to the other PC too, while this window is in front.
 *  On with its button or in full screen; Ctrl+Alt pressed and let go on their own turns it off
 *  (Ctrl+Alt+anything still goes there). */
function controlMode(keys) {
  const button = keys.querySelector("#control");
  const banner = document.getElementById("banner");
  document.getElementById("banner-text").textContent =
    `Controlling ${name}: the Windows key, Alt+Tab and every shortcut go there. Ctrl+Alt gives them back.`;
  const win = tauri.window.getCurrentWindow();
  let on = false, fullscreen = false;

  async function set(value) {
    if (on === value) return;
    on = value;
    button.classList.toggle("on", on);
    banner.hidden = !on;
    await invoke("screen_control", { id, on }).catch(() => {});
    if (!on && fullscreen) {
      fullscreen = false;
      await win.setFullscreen(false).catch(() => {});
    }
  }
  button.addEventListener("click", () => set(!on));
  document.getElementById("release").addEventListener("click", () => set(false));
  keys.querySelector("#fullscreen").addEventListener("click", async () => {
    fullscreen = true;
    await win.setFullscreen(true).catch(() => {});
    set(true);
  });
  // Leaving full screen (another way) ends Control mode too.
  win.onResized(async () => {
    if (fullscreen && !(await win.isFullscreen().catch(() => true))) {
      fullscreen = false;
      set(false);
    }
  });
  for (const b of [button, keys.querySelector("#fullscreen"), document.getElementById("release")]) {
    b.tabIndex = -1;
    b.addEventListener("mousedown", (e) => e.preventDefault()); // keep the keyboard on the screen
  }

  // Ctrl+Alt alone: out of Control mode.
  let ctrl = false, alt = false, both = false, other = false;
  window.addEventListener("keydown", (e) => {
    if (e.key === "Control") ctrl = true;
    else if (e.key === "Alt") alt = true;
    else other = true;
    if (ctrl && alt) both = true;
  }, true);
  window.addEventListener("keyup", (e) => {
    if (e.key === "Control") ctrl = false;
    else if (e.key === "Alt") alt = false;
    else return;
    if (ctrl || alt) return;
    const lone = both && !other;
    both = other = false;
    if (lone && on) set(false);
  }, true);
  window.addEventListener("beforeunload", () => { if (on) invoke("screen_control", { id, on: false }); });
}

if (pc) pcControls();
else {
  phoneControls();
  document.querySelectorAll("[data-key]").forEach((b) =>
    b.addEventListener("click", () => send({ kind: "key", key: b.dataset.key })));
}
