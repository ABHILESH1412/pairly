# Pairly: Architecture and Build Plan

Pairly connects a Linux PC and an Android phone so each one can see the other's notifications and share
clipboard, files, media controls and more. It is similar to KDE Connect, but it keeps working when the devices
are not on the same network.

- **Transport:** the app picks the best available link (LAN, then Bluetooth, then an internet relay) and
  switches between them without losing messages.
- **Security:** every packet is end-to-end encrypted with the Noise protocol, whichever link carries it.
  Paired device keys are pinned, and the relay sees only ciphertext.
- **Stack:** a shared Rust core, a Rust daemon with a GTK4 + libadwaita UI on Linux, and a Kotlin +
  Jetpack Compose app on Android that calls the Rust core through UniFFI.

---

## Table of contents

1. [Goals and non-goals](#1-goals-and-non-goals)
2. [Repository layout](#2-repository-layout)
3. [System architecture](#3-system-architecture)
4. [Shared Rust core (`pairly-core`)](#4-shared-rust-core-pairly-core)
5. [Protocol specification](#5-protocol-specification)
6. [Security design](#6-security-design)
7. [Transport manager](#7-transport-manager)
8. [Plugin system and feature catalog](#8-plugin-system-and-feature-catalog)
9. [Linux app (`pairly-linux`)](#9-linux-app-pairly-linux)
10. [Android app (`pairly-android`)](#10-android-app-pairly-android)
11. [Relay server](#11-relay-server)
12. [Phase-by-phase build guide](#12-phase-by-phase-build-guide)
13. [Testing strategy](#13-testing-strategy)
14. [Packaging and release](#14-packaging-and-release)
15. [Risks and mitigations](#15-risks-and-mitigations)
16. [Open decisions](#16-open-decisions)

---

## 1. Goals and non-goals

### Goals

- Notification sync in both directions, with dismiss, actions and replies.
- Hybrid connectivity: LAN, then Bluetooth, then internet relay, with switching that loses no messages.
- End-to-end encryption that does not depend on the transport.
- Most of KDE Connect's features: clipboard, file and URL sharing, battery, find my phone, media control,
  remote input, SMS, run commands.
- A light footprint: a small Linux daemon, a UI process that only runs while its window is open, and an
  Android app that is careful with battery.
- A modern, polished Linux UI (libadwaita) and a Material 3 Android UI.

### Non-goals for v1

- Wire compatibility with KDE Connect. We use our own protocol so that Noise encryption and transport
  switching are built in from the start. Compatibility could be added later as an optional bridge.
- iOS, Windows or macOS clients. The core is portable, but no work is planned for them.
- Groups with more than two devices. The design allows N devices, but testing focuses on one PC and one phone.
- Google Play distribution in v1. SMS and accessibility permissions complicate Play review, so v1 ships
  through GitHub Releases and F-Droid.

---

## 2. Repository layout

The code is split into three top-level folders. The Linux and Android apps live in separate folders as you
asked. A third folder, `pairly-core`, holds the Rust code that both apps share, so that it is not duplicated
or owned by one app.

```
pairly/
├── plan.md                         ← this file
│
├── pairly-core/                    ← shared Rust Cargo workspace (used by BOTH apps)
│   ├── Cargo.toml                  (workspace)
│   └── crates/
│       ├── pairly-proto/           packet types, CBOR codec, framing
│       ├── pairly-crypto/          identity keys, Noise handshakes, pairing SAS, key storage traits
│       ├── pairly-core/            node, sessions, device registry, transport manager, plugin host
│       ├── pairly-plugins/         platform-agnostic plugin logic (notifications, clipboard, share, …)
│       ├── pairly-transport-lan/   QUIC (quinn) + mDNS discovery
│       ├── pairly-transport-relay/ QUIC client to relay + hole punching
│       ├── pairly-ffi/             UniFFI bindings → Kotlin (builds libpairly_ffi.so for Android)
│       └── pairly-relay/           relay server binary (deployed to a VPS)
│
├── pairly-linux/                   ← Linux app: Cargo workspace, depends on ../pairly-core by path
│   ├── Cargo.toml
│   ├── crates/
│   │   ├── pairlyd/                headless daemon (systemd user service)
│   │   ├── pairly-gtk/             GTK4 + libadwaita + relm4 UI (D-Bus client)
│   │   ├── pairly-cli/             `pairly` CLI (D-Bus client)
│   │   ├── pairly-dbus/            shared D-Bus interface + types (zbus proxies)
│   │   └── pairly-transport-bt/    Bluetooth RFCOMM via bluer (Linux only)
│   ├── data/                       .desktop, icons, systemd unit, D-Bus service, metainfo
│   └── packaging/                  PKGBUILD (Arch/AUR), later .deb / Flatpak
│
└── pairly-android/                 ← Android app: Gradle project
    ├── settings.gradle.kts
    ├── build.gradle.kts
    ├── gradle/libs.versions.toml
    ├── core-bindings/              Android library: generated Kotlin bindings + jniLibs (.so)
    └── app/                        Compose UI, services, platform adapters
```

**Why `pairly-core` is its own folder.** Protocol, crypto, sessions, transport switching and most plugin logic
are written once, in Rust. Linux links them as normal crates. Android links them as a `.so` through
`pairly-ffi`. If this code lived inside `pairly-linux`, the Android build would depend on the Linux app's folder.

---

## 3. System architecture

```
 ┌──────────────────────── Linux PC ─────────────────────────┐        ┌───────────────────── Android phone ─────────────────────┐
 │                                                           │        │                                                          │
 │  pairly-gtk (GTK4/libadwaita)      pairly-cli             │        │  Jetpack Compose UI (Material 3)                         │
 │        │   D-Bus (zbus)               │                   │        │        │  StateFlow / ViewModels                         │
 │        └──────────────┬───────────────┘                   │        │        ▼                                                 │
 │                       ▼                                   │        │  PairlyService (foreground service, connectedDevice)     │
 │  pairlyd (daemon, tokio)                                  │        │   ├ NotificationListenerService adapter                  │
 │   ├ Linux platform adapters:                              │        │   ├ Clipboard / Battery / MediaSession / SMS adapters    │
 │   │   D-Bus notif monitor, notify, Wayland clipboard,     │        │   ├ Bluetooth RFCOMM adapter (Kotlin owns sockets)       │
 │   │   MPRIS, UPower, uinput/portal, tray (ksni)           │        │   └ Keystore-wrapped secrets                             │
 │   ├ pairly-transport-bt (bluer)                           │        │        │  UniFFI (JNI)                                   │
 │   └──────────────┐                                        │        │        ▼                                                 │
 │                  ▼                                        │        │  libpairly_ffi.so                                        │
 │   ┌──────────── pairly-core (shared Rust) ────────────┐   │        │   ┌──────────── pairly-core (same code) ──────────────┐  │
 │   │ Plugin host ── plugins (notif, clip, share, …)    │   │        │   │ Plugin host ── plugins                             │  │
 │   │ Session layer: Noise channel, ack queue, mux      │   │        │   │ Session layer                                      │  │
 │   │ Transport manager: score / migrate                │   │        │   │ Transport manager                                  │  │
 │   │ Transports: LAN (QUIC+mDNS) · Relay (QUIC) · BT*  │   │        │   │ LAN · Relay · BT (via foreign transport)           │  │
 │   │ Device registry (SQLCipher)                       │   │        │   │ Device registry                                    │  │
 │   └───────────────────────────────────────────────────┘   │        │   └────────────────────────────────────────────────────┘  │
 └───────────────────────────┬───────────────────────────────┘        └──────────────────────────────┬───────────────────────────┘
                             │                                                                         │
                             │   LAN: QUIC/UDP direct    ◄───────────────────────────────────────►     │
                             │   Bluetooth: RFCOMM       ◄───────────────────────────────────────►     │
                             │   Internet: QUIC ──► pairly-relay (blind, VPS) ──► QUIC                │
                             └──────────────── always inside a Noise session (E2E) ────────────────────┘
```

### Layers, bottom to top

| Layer | What it does | Where it lives |
|---|---|---|
| Transport | Moves opaque bytes as a reliable, ordered stream. Knows nothing about crypto or packets. | `pairly-transport-*` and the Android BT adapter |
| Channel | Runs the Noise handshake on a transport stream and encrypts and decrypts frames. One channel per transport connection. | `pairly-crypto` + `pairly-core::channel` |
| Session | One logical link per paired device. It outlives channels. It handles packet IDs, acks, retransmit after migration, priority multiplexing and dedup. | `pairly-core::session` |
| Transport manager | Discovers peers on every transport, scores them, opens the new channel before closing the old one, and migrates the session. | `pairly-core::transport_manager` |
| Plugin host | Routes packets to plugins by type and exchanges capabilities with the peer. | `pairly-core::plugin` |
| Plugins | Feature logic. They reach the OS through a `Platform` trait implemented natively on each OS. | `pairly-plugins` |
| Platform adapters | OS integration (D-Bus, Wayland, Android APIs). | `pairlyd` and the Android `app` |
| UI | Shows state and sends commands. Never on the data path. | `pairly-gtk`, `pairly-cli`, Compose UI |

### Key principle

Encryption sits **above** the transport layer, and feature logic sits **above** the session layer. Adding a
transport touches no plugin code, and adding a plugin touches no transport code.

---

## 4. Shared Rust core (`pairly-core`)

### 4.1 Crate dependencies (check the latest versions in Phase 0, then pin them in `Cargo.lock`)

| Purpose | Crate |
|---|---|
| Async runtime | `tokio` |
| QUIC | `quinn` with `rustls` and `rcgen` (self-signed certs; QUIC TLS is only the outer layer) |
| mDNS discovery | `mdns-sd` |
| Noise protocol | `snow` |
| Hashing / KDF | `blake2`, `hkdf`, `sha2` |
| Serialization | `serde` + `ciborium` (CBOR) |
| Storage | `rusqlite` with the `bundled-sqlcipher` feature |
| Errors / logging | `thiserror`, `anyhow` (binaries only), `tracing`, `tracing-subscriber` |
| FFI | `uniffi` (proc-macro mode) |
| Misc | `bytes`, `futures`, `async-trait` (only where native async-fn-in-trait is not enough), `uuid`, `rand` |

### 4.2 Core traits (sketch)

```rust
// pairly-core/src/transport.rs
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    fn kind(&self) -> TransportKind;               // Lan | Bluetooth | Relay
    /// Start advertising + discovering. Emits candidates for known/unknown peers.
    async fn start(&self, events: mpsc::Sender<TransportEvent>) -> Result<()>;
    /// Open a reliable, ordered byte stream to a candidate.
    async fn connect(&self, candidate: &PeerCandidate) -> Result<Box<dyn Duplex>>;
    async fn stop(&self) -> Result<()>;
}

pub trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}

pub enum TransportEvent {
    Discovered(PeerCandidate),                     // { device_id_hint, transport, address, rtt_hint }
    Lost(PeerCandidate),
    Incoming(Box<dyn Duplex>, TransportKind),      // inbound connection to run a handshake on
}
```

```rust
// pairly-core/src/plugin.rs
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    fn id(&self) -> &'static str;                  // "notification", "clipboard", …
    fn incoming_types(&self) -> &'static [&'static str];
    fn outgoing_types(&self) -> &'static [&'static str];
    async fn on_connected(&self, ctx: &PluginCtx) -> Result<()> { Ok(()) }
    async fn on_packet(&self, ctx: &PluginCtx, pkt: Packet) -> Result<()>;
    async fn on_local_event(&self, ctx: &PluginCtx, ev: LocalEvent) -> Result<()> { Ok(()) }
    async fn on_disconnected(&self, ctx: &PluginCtx) {}
}

// PluginCtx gives: ctx.send(packet, Priority), ctx.peer(), ctx.platform(), ctx.storage()
```

```rust
// pairly-core/src/platform.rs — implemented natively on Linux, via UniFFI callback interface on Android
pub trait Platform: Send + Sync {
    fn show_notification(&self, n: RemoteNotification) -> Result<()>;
    fn dismiss_notification(&self, id: &str) -> Result<()>;
    fn set_clipboard(&self, content: ClipContent) -> Result<()>;
    fn battery_state(&self) -> Option<BatteryState>;
    fn ring(&self, on: bool) -> Result<()>;
    fn downloads_dir(&self) -> PathBuf;
    // … grows one method group per plugin
}
```

### 4.3 Public node API (the same surface for Linux and for the FFI layer)

```rust
pub struct PairlyNode { … }
impl PairlyNode {
    pub async fn start(config: NodeConfig, platform: Arc<dyn Platform>, extra_transports: Vec<Arc<dyn Transport>>) -> Result<Self>;
    pub fn identity(&self) -> DeviceIdentity;                     // id, name, type, fingerprint
    pub fn devices(&self) -> Vec<DeviceInfo>;                     // paired + discovered, with link state
    pub async fn request_pair(&self, device: DeviceId) -> Result<PairingHandle>;
    pub async fn pairing_qr(&self) -> Result<PairingQr>;          // payload for a QR code
    pub async fn pair_from_qr(&self, payload: &str) -> Result<PairingHandle>;
    pub async fn confirm_pair(&self, handle: PairingHandle, accept: bool) -> Result<()>;
    pub async fn unpair(&self, device: DeviceId) -> Result<()>;
    pub async fn local_event(&self, ev: LocalEvent) -> Result<()>; // e.g. a local notification was posted
    pub async fn command(&self, device: DeviceId, cmd: Command) -> Result<()>; // send file, ring, …
    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent>;    // UI updates
    pub async fn shutdown(self);
}
```

On Android, `pairly-ffi` wraps this as UniFFI `Object`s. `NodeEvent` is delivered through a callback interface,
and every `async fn` becomes a Kotlin `suspend fun`.

---

## 5. Protocol specification

### 5.1 Framing (on every transport)

```
Transport stream:  [ u16 BE length ][ Noise message (≤ 65535 bytes) ] [ u16 len ][ … ] …
```

- The first messages are the Noise handshake. After it completes, each frame is one Noise transport message
  holding an encrypted **inner packet**.
- A packet larger than one frame (for example a 64 KiB file chunk plus header) is split by the session layer
  into multiple frames, each with a `more` flag.

### 5.2 Inner packet (CBOR)

```rust
struct Envelope {
    v: u8,                 // protocol version (1)
    id: u64,               // per-session monotonic, used for ack + dedup
    ack: bool,             // sender wants an ack
    ty: String,            // "notification.posted", "clipboard.set", …
    body: ciborium::Value, // type-specific payload
}
// Session control: "ack" { ids: [u64] }, "keepalive" { t }, "keepalive.ack" { t }
// Pre-session (id 0, never acked): "identity" {…}, "pair.commit", "pair.nonce", "pair.reveal", "pair.confirm"
```

`ty` must be lowercase ASCII (`a-z`, `0-9`, `.`, `_`), at most 64 bytes. An envelope is at most ~1 MiB, and
decoding untrusted input uses a CBOR nesting limit of 32.

### 5.3 Session rules

- **Acks:** packets with `ack: true` stay in the outbound queue until the peer acks them. Acks are batched
  every 50 ms or 32 packets.
- **Migration:** when the transport manager swaps channels, every unacked packet is resent on the new channel.
  The receiver drops duplicates with a sliding window of the last 4096 IDs.
- **Restarts:** each process run picks a random `session_nonce` (sent in `identity`). A changed nonce tells the
  peer to reset its dedup window, since the restarted side's IDs start over at 1. Delivery is
  *at least once*: if a device crashes before acking, the sender resends after reconnect, so plugins must
  tolerate a repeated packet (for example, notification IDs are idempotent). A clean shutdown flushes
  pending acks first, so this only happens after a crash.
- **Priorities:** `Control > Interactive (notifications, clipboard, input) > Bulk (file chunks)`. The
  scheduler sends interactive packets between file chunks, so a 2 GB transfer never delays a notification.
- **Keepalive:** a `keepalive` every 15 s on LAN and BT, and every 60–120 s on the relay. The echoed
  `keepalive.ack` gives the RTT. 45 s without any inbound traffic marks the channel dead.

### 5.4 Identity / capability exchange (first packet after the handshake)

```cbor
identity {
  name, device_type: "desktop"|"phone"|"tablet"|"laptop",
  app_version, session_nonce,
  incoming: ["notification.posted", …], outgoing: [ … ]
  // later phases add: transports: { bt: { addr }, relay: { url } }
}
```

There is no `device_id` field. The receiver derives the ID from the peer's Noise static key, which the
handshake has already authenticated, so a peer cannot claim someone else's ID.

Each side enables a plugin only if the peer supports the matching types, as KDE Connect does.

### 5.5 Packet catalog (v1)

| Type | Direction | Body (summary) | Ack |
|---|---|---|---|
| `notification.posted` | both | id, app, app_icon_png?, title, text, time, actions[], can_reply, silent, origin_key | yes |
| `notification.removed` | both | id | yes |
| `notification.action` | both | id, action_key | yes |
| `notification.reply` | both | id, text | yes |
| `clipboard.set` | both | mime, data (≤ 1 MiB; larger goes through share) | yes |
| `battery.state` | both | percent, charging, threshold_event? | no |
| `findmy.ring` | PC→phone (also reverse) | on/off | yes |
| `share.offer` | both | transfer_id, kind(file/url/text), name, size, mime, sha256 | yes |
| `share.accept` / `share.reject` | both | transfer_id, resume_offset | yes |
| `share.chunk` | both | transfer_id, offset, data | no (the transfer layer checks offsets and the final hash) |
| `share.done` / `share.cancel` | both | transfer_id, sha256 | yes |
| `media.state` / `media.command` | both | player, title, artist, position, playing / play/pause/next/seek/volume | mixed |
| `input.pointer` / `input.key` | phone→PC | dx, dy, buttons, scroll / keysym, text | no |
| `sms.list` / `sms.send` / `sms.received` | phone↔PC | threads, messages / to, text | yes |
| `command.list` / `command.run` | both | name, id | yes |
| `telephony.event` | phone→PC | ringing/talking/missed, contact | yes |

---

## 6. Security design

### 6.1 Identity

- Each device generates a long-term **X25519 static key pair** on first launch.
- `device_id` is the first 16 bytes of `BLAKE2s(static_pubkey)`, base32-encoded. It is used in
  mDNS TXT records and in the UI.
- Key storage:
  - **Linux:** the Secret Service API through the `oo7` crate. Hyprland may have no keyring daemon
    (gnome-keyring or KWallet), so the fallback is a `0600` file in `$XDG_DATA_HOME/pairly/`.
  - **Android:** the Rust core generates the key. An AES-GCM key held in the **Android Keystore**
    (non-exportable) encrypts it, and the encrypted blob is stored in app-private storage. Kotlin unwraps it at
    start and passes it to Rust. The Keystore cannot hold X25519 keys usable by `snow`, which is why the key is
    wrapped instead.

### 6.2 Pairing

There are two flows, and both end with each device pinning the other's static public key.

1. **QR flow (preferred, works over LAN or relay)**
   - The PC shows a QR with `pairly://pair?id=<device_id>&pk=<static_pubkey>&secret=<128-bit one-time>&lan=<ip:port>&relay=<url>`.
   - The phone scans it, connects over any transport, and runs **`Noise_XXpsk3`**, with the PSK derived from the
     one-time secret.
   - The phone already knows the PC's key from the QR, and the PSK proves the PC showed that QR, so a
     man-in-the-middle is not possible. No code needs to be compared.
2. **Code-comparison flow (LAN discovery, no camera)**
   - The devices run **`Noise_XX`**, then a commit/reveal exchange inside the encrypted channel (the same idea as
     Bluetooth's numeric comparison). `h` is the handshake hash.
     1. responder → initiator: `pair.commit { BLAKE2s("pairly-sas-commit" ‖ h ‖ n_r) }`
     2. initiator → responder: `pair.nonce { n_i }`
     3. responder → initiator: `pair.reveal { n_r }`, and the initiator checks the commitment.
   - Both screens show `SAS = BLAKE2s("pairly-sas-code" ‖ h ‖ n_i ‖ n_r) mod 10^6` as `123 456`.
   - The user checks that the codes match and taps *Accept* on both devices (`pair.confirm`). The pairing is
     stored only after both devices accept.
   - **Why the commit step matters:** a code derived from `h` alone can be brute-forced by an active
     man-in-the-middle. It controls the ephemeral keys on both legs and can try about 10^6 values offline
     until the two codes match. With the commit step, the responder is bound to `n_r` before it sees `n_i`, and
     the initiator reveals `n_i` before it sees `n_r`, so the attacker gets one 1-in-a-million guess per
     attempt.

After pairing, both devices store `{device_id, static_pubkey, name, device_type, pair_secret}`, where
`pair_secret = BLAKE2s("pairly-pair-secret" ‖ h ‖ n_i ‖ n_r)`. It is used later to derive the relay room (see
section 11). The nonces travel encrypted, so `pair_secret` is unknown to observers. The handshake hash on its
own is **not** secret, so it must never be used as a key.

Before the first Noise message, the initiator sends a 2-byte **hello** `[version, kind]` in the clear
(kind 1 = pair, 2 = QR pair, 3 = reconnect). The hello is also bound into the Noise prologue, so an attacker
who rewrites it makes the handshake fail. A responder in IK mode checks the initiator's key after the first
message and refuses unpaired keys before replying.

### 6.3 Reconnect

- Use **`Noise_IK`** with the pinned key (1-RTT). Reject any peer whose static key does not match its pinned key.
- Cipher suite: `Noise_*_25519_ChaChaPoly_BLAKE2s`.
- Every connection uses fresh ephemeral keys, which gives forward secrecy.
- Rekey after 1 GiB of data or 1 hour, whichever comes first (`snow` `rekey_outgoing`, signalled with a control
  packet).

### 6.4 Defense in depth

- QUIC's TLS 1.3 is a second, outer layer. It uses self-signed certificates and is not relied on for security.
- Bluetooth link encryption is ignored because Noise already protects the data.
- Replay protection: Noise nonces cover a single channel. Session packet IDs plus the persisted dedup window
  cover reconnects.
- Data at rest:
  - The SQLCipher database (devices, notification history, transfer state) is keyed from the keystore /
    Secret Service.
  - Received files go to the downloads folder with normal user permissions.
- Untrusted input: CBOR decoding has size limits, every field is validated, and the decoder is fuzzed
  (Phase 11).
- Relay metadata: the relay sees IP addresses, timing and sizes. Self-hosting reduces this, and optional
  padding to 256-byte buckets is a setting.

### 6.5 Threat model (summary)

| Attacker | Can they read data? | Mitigation |
|---|---|---|
| Same Wi-Fi / LAN sniffer | No | Noise E2E |
| Malicious relay operator | No (sees only metadata) | E2E, blind relay, self-hosting |
| Active MITM during pairing | No | QR + PSK, or SAS comparison |
| Stolen pinned key later | No access to past traffic | Forward secrecy (ephemeral keys) |
| Rogue device on LAN | Cannot connect | Pinned keys; unpaired devices can only request pairing, which the user must accept |
| Malware on the phone or PC | Yes | Out of scope (OS sandboxing) |

---

## 7. Transport manager

### 7.1 Responsibilities

1. Run every enabled transport's discovery: mDNS on LAN, BT probes of paired addresses, and a relay presence
   connection.
2. Keep a **candidate table** for each paired device, with transport, address, last seen, measured RTT and loss.
3. Hold **one active channel** per paired device and score the alternatives.
4. Migrate by opening the new channel before closing the old one:
   1. Connect on the better transport.
   2. Complete the Noise IK handshake.
   3. Point the session at the new channel.
   4. Resend unacked packets.
   5. Close the old channel after a 2 s grace period.

### 7.2 Scoring

```
score = base(transport) − rtt_penalty − loss_penalty − battery_penalty
base:  LAN 100, Bluetooth 60, Relay-direct (hole-punched) 50, Relay 30
battery_penalty (phone only, not charging): Relay +10, BT +5
```

- **Hysteresis:** switch only if the new score is at least 15 points higher **and** stays higher for 5 s.
  This avoids flapping between links.
- **Probing:**
  - mDNS runs continuously.
  - BT probes the paired address every 60 s, but only while LAN is down.
  - The relay connection is kept alive only when no direct link exists (on the phone, to save battery). On
    the PC it can stay up permanently.

### 7.3 Connection direction

- Both sides may connect. If both connect at the same moment, the side with the lexicographically lower
  `device_id` keeps its outbound connection and the other drops its own.

---

## 8. Plugin system and feature catalog

| # | Plugin | Linux side | Android side | Phase |
|---|---|---|---|---|
| 1 | **ping** | send/receive, toast | send/receive, toast | 2–3 |
| 2 | **notification** | D-Bus monitor of `Notify` calls; shows remote notifications with actions and reply | `NotificationListenerService`; posts PC notifications to a dedicated channel | 5 |
| 3 | **clipboard** | Wayland `wlr/ext-data-control` (`wl-clipboard-rs`), X11 fallback | write from the background; reading needs a user action on Android 10+ (see 10.4) | 6 |
| 4 | **battery** | UPower over D-Bus | `BatteryManager` sticky intent | 6 |
| 5 | **findmy** | rings the phone; also "find my PC" sound | plays the alarm at full volume, overriding DND | 6 |
| 6 | **share** | drag-and-drop, CLI `pairly send`, file manager action | system share target (`ACTION_SEND`), progress notification | 7 |
| 7 | **media** | MPRIS over D-Bus | `MediaSessionManager.getActiveSessions` (allowed by the notification listener permission) | 10 |
| 8 | **input** (phone as touchpad and keyboard) | XDG RemoteDesktop portal + libei (`ashpd` + `reis`); `uinput` fallback | touchpad and keyboard UI | 10 |
| 9 | **sms** | conversation view in the GTK UI | `Telephony` provider, `SmsManager` | 10 |
| 10 | **command** | runs configured shell commands | lists the PC's commands and runs them | 10 |
| 11 | **telephony** | shows the call, pauses media during calls | `TelephonyCallback` | 10 |
| 12 | **presenter** | arrow keys plus a laser pointer overlay | volume keys / buttons | stretch |

### Notification loop prevention (important)

Without these rules, a mirrored notification would be picked up by the other side's listener and sent back,
forever.

- **Linux:** mirrored notifications carry a hint `x-pairly-origin=<device_id>`, and the monitor ignores any
  `Notify` call with that hint. It also ignores calls whose sender is the daemon's own D-Bus name.
- **Android:** the listener ignores notifications from Pairly's own package.
- Per-app filters on both sides (blocklist or allowlist), set in the UI and stored in the registry.
- Only the **source** device sends `notification.removed`, which keeps dismissal in sync.

---

## 9. Linux app (`pairly-linux`)

### 9.1 Processes

| Process | Role | Lifetime |
|---|---|---|
| `pairlyd` | Runs `PairlyNode` plus the Linux platform adapters and the tray. Owns the D-Bus name `dev.pairly.Daemon`. | systemd **user** service, starts at login |
| `pairly-gtk` | UI, a pure D-Bus client | Only while the window is open |
| `pairly` (CLI) | Scripting: `pairly devices`, `pairly send <dev> file`, `pairly ring`, `pairly pair` | One shot |

### 9.2 D-Bus API (`pairly-dbus` crate, zbus)

```
Bus name:  dev.pairly.Daemon      Object: /dev/pairly/Daemon
Interface: dev.pairly.Daemon1
  Methods:  GetIdentity() → (s id, s name, s fingerprint)
            ListDevices() → a(ssssbu…)           // id, name, type, link(lan|bt|relay|none), paired, battery
            RequestPair(s id) / PairingQr() → s / ConfirmPair(s id, b accept) / Unpair(s id)
            SendFiles(s id, as paths) / SendText(s id, s text) / SendClipboard(s id)
            Ring(s id) / RunCommand(s id, s cmd)
            GetSettings() → a{sv} / SetSetting(s key, v value)
  Signals:  DeviceChanged(s id)  PairingRequested(s id, s name, s sas)
            TransferProgress(s transfer_id, t done, t total)  NotificationMirrored(s id, s title)
```

Use `dev.pairly` as a placeholder. Before the first release, change it to a reverse-DNS name you control,
such as `io.github.<username>.Pairly`. Flathub and app-store metadata require that.

### 9.3 Linux platform adapters (inside `pairlyd`)

| Feature | Implementation notes |
|---|---|
| Read local notifications | zbus `org.freedesktop.DBus.Monitoring.BecomeMonitor` on the session bus with match `interface='org.freedesktop.Notifications',member='Notify'`. This works with any notification daemon (mako, dunst, swaync, GNOME, Plasma). Ignore calls carrying our own hint. |
| Show remote notifications | Call `org.freedesktop.Notifications.Notify` through zbus with actions, the `x-pairly-origin` hint and the app icon as `image-data`. Listen for `ActionInvoked` and `NotificationClosed`. |
| Inline reply | If `GetCapabilities` includes `inline-reply` (Plasma, swaync), use it. Otherwise add a "Reply" action that opens a small libadwaita reply dialog. |
| Clipboard | `wl-clipboard-rs` (Hyprland supports data-control). GNOME/Mutter lacks data-control, so on GNOME the clipboard can only be sent from the UI window or with a hotkey. Watch for changes with a data-control listener. |
| Tray | `ksni` (StatusNotifierItem). On Hyprland, Waybar's `tray` module displays it. |
| Battery | UPower `DisplayDevice` over zbus |
| Media | MPRIS over zbus: enumerate `org.mpris.MediaPlayer2.*` |
| Remote input | XDG Desktop Portal RemoteDesktop through `ashpd`, plus libei. Hyprland's portal (xdg-desktop-portal-hyprland) supports this partially, so `uinput` is the fallback. It needs a udev rule and the `input` group, documented in the README. |
| Bluetooth | `bluer`: RFCOMM profile registration with a fixed Pairly service UUID |
| Autostart | `~/.config/systemd/user/pairlyd.service`, plus a D-Bus activation file so the UI can start the daemon |

### 9.4 GTK UI design (libadwaita + relm4)

- **Window:** `AdwApplicationWindow` with an `AdwNavigationSplitView`.
  - **Sidebar:** device list. Each row shows the device icon, name, a status dot and a **link badge** (Wi-Fi,
    Bluetooth or globe icon) so you can see which transport is active.
  - **Content:** a device page made of `AdwPreferencesPage` groups:
    - *Status:* battery, link, latency.
    - *Quick actions:* buttons for Ring, Send files, Send clipboard, Send text.
    - *Notifications:* recent mirrored notifications.
    - *Plugins:* switches to enable or disable each plugin.
- **Pairing:** an `AdwDialog` that shows the QR code (rendered with the `qrcode` crate into a `GdkTexture`).
  A "Pair with code" button switches to the SAS view with two large 3-digit groups.
- **Transfers:** a popover with progress rows. A toast appears when a transfer finishes, with an "Open folder"
  button.
- **Settings:** `AdwPreferencesDialog` with device name, download folder, relay URL (default plus a custom
  option), notification app filters, and launch at login.
- Follows the system light/dark preference (`AdwStyleManager`) and the accent color.
- Drag files onto a device row to send them.

---

## 10. Android app (`pairly-android`)

### 10.1 Stack

- Kotlin, Jetpack Compose, Material 3 (dynamic color), Navigation Compose and ViewModels. Use Hilt or simple
  manual DI. Manual DI is enough for an app this size.
- `minSdk 26`, and `targetSdk` / `compileSdk` set to the latest stable SDK.
- Rust is integrated through **UniFFI**:
  - `cargo-ndk` builds `pairly-ffi` for `arm64-v8a`, `armeabi-v7a` and `x86_64` (the emulator), outputting
    into `core-bindings/src/main/jniLibs`.
  - `uniffi-bindgen` generates Kotlin into `core-bindings/src/main/kotlin`.
  - A Gradle `Exec` task runs both steps before `preBuild`.
- QR scanning uses CameraX with **ZXing** (`zxing-android-embedded` or `zxing-core`) instead of ML Kit, so the
  app has no Google Play Services dependency and stays F-Droid friendly.

### 10.2 Components

```
app/src/main/java/dev/pairly/android/
├── PairlyApp.kt                 Application: loads native lib, holds the node handle
├── service/
│   ├── PairlyService.kt         foreground service (type: connectedDevice), owns PairlyNode lifetime
│   ├── NotificationListener.kt  NotificationListenerService → node.localEvent(...)
│   ├── BootReceiver.kt          restart service after reboot
│   └── ShareReceiverActivity.kt ACTION_SEND / SEND_MULTIPLE target (transparent)
├── platform/
│   ├── AndroidPlatform.kt       implements UniFFI `Platform` callback interface
│   ├── Notifications.kt         post mirrored PC notifications (own channel), reply via RemoteInput
│   ├── Clipboard.kt             set clip; "send clipboard" tile / notification action
│   ├── Battery.kt, Media.kt, Sms.kt, Telephony.kt, FindMyPhone.kt
│   ├── BluetoothTransport.kt    RFCOMM server/client → UniFFI `ForeignTransport`
│   └── KeyStore.kt              wrap/unwrap identity key with Android Keystore AES-GCM
├── ui/
│   ├── onboarding/              permission wizard
│   ├── devices/                 list + detail screens
│   ├── pairing/                 QR scanner + SAS confirm
│   ├── transfers/, settings/, notifications filter/
│   └── theme/
└── tile/ClipboardTileService.kt Quick Settings tile "Send clipboard"
```

### 10.3 Permissions and the onboarding wizard

Walk the user through these in order, explaining each one:

1. `POST_NOTIFICATIONS` (Android 13+)
2. **Notification access**: opens the system settings screen for the `NotificationListenerService`.
3. **Ignore battery optimizations**: `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`. This is justified because the app
   holds a persistent connection to a companion device.
4. `NEARBY_WIFI_DEVICES` (13+); `ACCESS_FINE_LOCATION` on older versions, only if mDNS needs it.
5. `BLUETOOTH_CONNECT` and `BLUETOOTH_SCAN` (12+), requested when Bluetooth is enabled.
6. Optional, requested only when the plugin is enabled: `READ_SMS`/`SEND_SMS`/`RECEIVE_SMS`, `READ_PHONE_STATE`,
   `READ_CONTACTS`, `ACCESS_NOTIFICATION_POLICY` (for find-my-phone to override DND).

Also declare `FOREGROUND_SERVICE` and `FOREGROUND_SERVICE_CONNECTED_DEVICE`, plus the
`CHANGE_WIFI_MULTICAST_STATE` and `INTERNET` permissions.

### 10.4 Android-specific gotchas

- **mDNS:** acquire a `WifiManager.MulticastLock` while LAN discovery runs, otherwise many phones drop
  multicast packets. If `mdns-sd` misbehaves on some devices, implement discovery in Kotlin with `NsdManager`
  and feed the results to Rust through a callback.
- **Clipboard:** since Android 10, background apps cannot read the clipboard. Provide three ways to send it:
  - a Quick Settings tile,
  - an action on the persistent notification (both open a transparent activity for a moment),
  - the share sheet.
  Receiving clipboard content (writing it) works in the background.
- **Doze and OEM killers:** use the foreground service and the battery-optimization exemption. Link users to
  dontkillmyapp.com from Settings when the phone is from an OEM with aggressive app killing.
- **Relay mode on battery:** keep the relay QUIC connection with long keepalives (60–120 s), and shorten them
  while charging. Stretch goal: **UnifiedPush** wake-ups (an ntfy distributor) so the phone can drop the relay
  socket entirely and wake only when the PC has something to send.
- **Threads:** the Rust tokio runtime runs on its own threads inside the `.so`. Calls from Kotlin are UniFFI
  `suspend` functions, and callbacks into Kotlin must hop to `Dispatchers.Main` before touching UI state.

### 10.5 Android UI design (Material 3)

- **Home:** a large card for the paired PC showing a link chip (LAN / BT / Internet), PC battery, and buttons
  for Send files, Send clipboard, Ring PC and Touchpad. Below it, a list of other discovered devices with a
  "Pair" button.
- **Pairing:** a full-screen QR scanner. A "Use code instead" link leads to the SAS compare screen.
- **Device detail:** plugin switches, notification app filter (app list with switches), and a "Forget device"
  action.
- **Persistent notification:** shows "Connected to <PC> via Wi-Fi", with actions *Send clipboard* and
  *Disconnect*.

---

## 11. Relay server

- `pairly-relay` is one Rust binary built on tokio and quinn. It holds roughly a few MB of state per thousand
  connections. Deploy it on a small VPS with Docker or a systemd service.
- **Blind rooms:**
  - Both paired devices compute `room = BLAKE2s(pair_secret, "relay-room")[..16]`.
  - Each device connects and sends `JOIN room`. The relay pairs the two connections that share a room and
    forwards bytes between them.
  - The relay never sees device IDs, names or keys, and the Noise IK handshake then runs through it.
- **Hole punching (stretch, Phase 8b):**
  - The relay tells each side the other's observed `ip:port`.
  - Both sides send QUIC packets from the **same UDP socket** they used for the relay.
  - If a direct QUIC connection completes, the transport manager scores it as `Relay-direct` and migrates
    to it. Otherwise traffic stays on the relay.
- **Abuse limits:** connections per IP, a bandwidth cap per room, idle timeout, and a maximum room lifetime.
  An optional shared access token keeps a self-hosted relay private.
- Configuration: listen address, TLS certificate (Let's Encrypt or self-signed with the pin shipped in the
  apps), and limits.

---

## 12. Phase-by-phase build guide

Each phase ends with something you can run. Do not start the next phase until the current phase's
**Done when** checklist passes.

### Phase 0: Environment and skeletons

> **Status: done (2026-10-03).** rustup stable 1.99 + Android targets, cargo-ndk 4.1.2, Android
> Studio (JBR 25), SDK API 37, NDK 30.0.16248370. Both Rust workspaces build clean; the Android
> skeleton runs on the test phone (moto g85 5G, Android 16, arm64-v8a).

**Toolchain (findings from your machine: EndeavourOS + Hyprland, Rust 1.99 from pacman, JDK 27, adb present,
no Android SDK/NDK):**

1. Switch to `rustup`, because the Android Rust targets are only available through it. On Arch, run
   `sudo pacman -S rustup` (pacman will offer to replace the `rust` package), then
   `rustup default stable`.
2. Add the Android targets:
   `rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android`.
3. Install the Rust tools: `cargo install cargo-ndk`. Add `uniffi-bindgen` later, as a binary inside the
   workspace.
4. Install Android Studio (AUR `android-studio`), or cmdline-tools plus the SDK, platform-tools, the latest
   platform and the **NDK**.
   - Set `ANDROID_HOME` and `ANDROID_NDK_HOME`.
   - Use **JDK 21**, either Android Studio's bundled JBR or `jdk21-openjdk`, for Gradle. JDK 27 may be newer
     than the Android Gradle Plugin supports.
5. Install the Linux development packages: `gtk4`, `libadwaita`, `bluez`, `bluez-libs`, `sqlcipher` (or use the
   bundled feature), `dbus`, `xdg-desktop-portal-hyprland`, plus `blueprint-compiler` if you use it.
6. Optional: `cargo-deny`, `cargo-nextest` and `cargo-fuzz` (the last needs nightly).

**Skeleton steps:**

1. `pairly-core/`: create the workspace with empty crates (`proto`, `crypto`, `core`, `plugins`,
   `transport-lan`, `transport-relay`, `ffi`, `relay`). Each one should compile.
2. `pairly-linux/`: create a workspace whose path dependencies point to `../pairly-core/crates/*`. Create
   empty `pairlyd`, `pairly-gtk`, `pairly-cli`, `pairly-dbus` and `pairly-transport-bt` crates.
3. `pairly-android/`: create a new Compose project in Android Studio (package `dev.pairly.android`). Add a
   `core-bindings` library module.
4. Make the shared settings: a `rustfmt.toml`, `clippy` lints in `[workspace.lints]`, a single
   `rust-toolchain.toml` (stable), `.editorconfig` and `.gitignore`.
5. Run `git init` at the root (you commit and push yourself). Add CI later (GitHub Actions: `cargo test`,
   `clippy`, Android `assembleDebug`).

**Done when:**
- `cargo build` passes in `pairly-core` and in `pairly-linux`.
- Android Studio runs an empty app on your phone over `adb`.

---

### Phase 1: Protocol and crypto core (no network yet)

> **Status: done (2026-10-03).** 44 tests across `pairly-proto` (11), `pairly-crypto` (14) and
> `pairly-core` (14 unit + 5 end-to-end node tests); 15 consecutive full runs passed with no flakes. Clippy and
> rustfmt are clean. The whole core cross-compiles for `aarch64-linux-android`.
> Changes from the original plan:
> - The SAS pairing now has a commit/reveal step, and `pair_secret` is derived from the exchanged nonces
>   (see 6.2).
> - `identity` has no `device_id` field and now carries a `session_nonce` (see 5.3–5.4).
> - Keepalive packets are named `keepalive` / `keepalive.ack`.
> - `Session::close` flushes pending acks on a clean shutdown.
> - Reconnecting with backoff and the tie-break for simultaneous connects are already implemented in the node.
>   Phase 2 only has to add real transports.

1. `pairly-proto`: write the `Envelope` and the packet structs from section 5.5 (ping, identity, ack first).
   Add CBOR encode/decode with size limits, and the `u16`-length frame codec as a `tokio_util::codec`.
2. `pairly-crypto`:
   - identity key generation and loading through a `KeyStore` trait, with a file implementation for now;
   - `device_id` derivation;
   - Noise `XX`, `XXpsk3` and `IK` handshake builders over any `AsyncRead + AsyncWrite`;
   - SAS derivation from the handshake hash;
   - `pair_secret` derivation.
3. `pairly-core`:
   - `Channel` (an encrypted frame stream over a `Duplex`);
   - `Session` (packet IDs, ack queue, dedup window, priority scheduler);
   - the device registry (SQLite, plain for now, SQLCipher in Phase 11);
   - a pairing state machine;
   - the `Transport`, `Plugin` and `Platform` traits.
4. Write a **memory transport** (`tokio::io::duplex`) for tests.
5. Tests:
   - two in-process nodes pair through the SAS flow and exchange ping and pong;
   - a wrong key is rejected after pairing;
   - a duplicate packet ID is dropped;
   - a session resends unacked packets after the channel is swapped.

**Done when:**
- `cargo test` in `pairly-core` covers pairing, encrypted ping, rejection of a wrong key, and migration
  resend over the memory transport.

---

### Phase 2: LAN transport and Linux daemon MVP

> **Status: done (2026-10-03).** Verified live with two `pairlyd` instances on one machine:
> - They discovered each other over mDNS and paired via `pairly pair` / `pairly listen`, with matching codes.
> - Pings went both ways, about 15 ms end to end including CLI start.
> - After a restart, B reconnected automatically in under 1 s, with no re-pairing, and the keepalive RTT
>   shows as 1 ms.
>
> 46 core tests (including an mDNS + QUIC end-to-end test) passed 10 runs in a row, and the core
> cross-compiles for arm64 Android with QUIC, TLS and mDNS included.
> Changes from the original plan:
> - The second-instance override is a `bus_name` key in `config.toml`, plus `pairly --bus-name`.
> - The CLI gained `listen` (answers incoming pairing requests and shows pings) and `unpair`.
>   `--yes` exists for scripted tests only.
> - The first plugin (`ping`) and the first `Platform` hook (`ping_received`) are in. Platform methods
>   have no-op defaults.
> - QUIC uses the `ring` crypto backend rather than `aws-lc-rs`, because it cross-compiles to Android
>   without CMake.
> - A dropped QUIC stream waits up to 2 s for in-flight bytes, such as acks flushed on close, to be
>   acknowledged before the connection closes.
> - `pairlyd` emits D-Bus signals before showing desktop notifications (in the background), so a slow
>   notification daemon can't delay clients.

1. `pairly-transport-lan`:
   - a quinn endpoint on a UDP port (default 47100, with fallback to a random port);
   - a self-signed cert, with a client config that skips verification (Noise handles authentication);
   - one bidirectional stream per channel, exposed as a `Duplex`;
   - mDNS service `_pairly._udp.local.` with TXT `id`, `name`, `type` and `v`.
2. `PairlyNode::start` wires up the transport manager, still with only LAN and no scoring.
3. `pairly-dbus`: the interface definitions from section 9.2 (start with GetIdentity, ListDevices,
   RequestPair, ConfirmPair, Ping and the signals).
4. `pairlyd`:
   - loads its config from `$XDG_CONFIG_HOME/pairly/config.toml`;
   - starts the node and serves D-Bus;
   - logs through `tracing` to journald.
5. `pairly-cli`: `pairly id`, `pairly devices`, `pairly pair <id>` (shows the SAS and asks y/n), and
   `pairly ping <id>`.
6. Test with two daemons on one machine. Use different config dirs and ports, and a `--session-bus-name`
   override for the second instance.

**Done when:**
- Two `pairlyd` instances discover each other.
- They pair with the CLI after the SAS codes are compared.
- Pings work in both directions and the pairing persists across restarts.

---

### Phase 3: Android shell and FFI and LAN pairing with the PC

> **Status: done (2026-10-03).** Verified on the moto g85 5G (Android 16) against `pairlyd` on the PC:
> - The phone discovered the PC over mDNS and paired from the phone UI, with the same code on both
>   screens (152 710).
> - Pings work both ways, with a heads-up notification on the phone and a 3–5 ms RTT.
> - After a force-stop and reopen, the phone reconnects in about 2 s.
> - After a `pairlyd` restart, the two reconnect in about 1 s.
> - When USB tethering was turned off and on (the PC's address changed), both sides rediscovered each other
>   in about 2 s. The live session survived through QUIC connection migration, with no reconnect needed.
>
> What was built:
> - `pairly-ffi`: UniFFI 0.32, a private Tokio runtime, `EventListener`/`SecretStore` callback traits, and
>   logs sent to logcat.
> - A `crates/uniffi-bindgen` host tool.
> - `scripts/build-rust.sh`, plus a typed Gradle `BuildRust` task wired in through the AGP 9 Variant API
>   (`addGeneratedSourceDirectory`). The output goes to `build/`, so nothing generated is committed.
> - A Keystore AES-GCM–wrapped identity, a foreground service (`connectedDevice`) holding a multicast lock,
>   and a Compose UI: device cards, an available list, the code dialog, and unpair confirmation.
>
> Changes from the original plan:
> - The `ForeignTransport` stub is postponed to Phase 9, when Bluetooth needs it.
> - Pings reach Kotlin as an `Event` rather than through a `Platform` callback interface, which arrives with
>   Phase 5.
> - Added `Transport::network_changed` / `Node.networkChanged()`: Android calls it from a `NetworkCallback`
>   and when the app returns to the foreground, to re-announce and re-browse over mDNS. Without it, a network
>   that appeared after startup went unnoticed until the app restarted.
>
> **Setup lessons (also in the Linux README):**
> - firewalld's `public` zone blocks this by default. It needs `mdns` and `47100/udp` opened.
> - Routers with client/AP isolation block phone↔PC traffic entirely, even pings. USB tethering or the phone's
>   hotspot works around it. Phase 8 (the relay) is the real fix for networks like that.

1. `pairly-ffi`: expose `PairlyNode` through UniFFI:
   - `#[uniffi::export]` on async methods;
   - a callback interface for `Platform`;
   - a callback interface for `NodeEvent` listeners;
   - a callback interface for the `ForeignTransport` stub (used by BT later).
2. Build script `pairly-android/scripts/build-rust.sh`:
   - run `cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o ../core-bindings/src/main/jniLibs build -p pairly-ffi --release`;
   - run `uniffi-bindgen generate --language kotlin`;
   - hook both into Gradle `preBuild`.
3. `PairlyService` (foreground service) starts the node, and `AndroidPlatform` provides stubs.
4. Acquire the `MulticastLock` and check that discovery works on real Wi-Fi.
5. Compose UI: a device list, a pair button and a SAS confirmation dialog. Show a ping toast.
6. Store the identity key wrapped with Keystore (`KeyStore.kt`).

**Done when:**
- The phone and `pairlyd` on your PC discover each other, pair with SAS, and ping in both directions.
- Killing and reopening the app reconnects automatically.

---

### Phase 4: Linux GTK UI

> **Status: done (2026-10-03).** Verified on the PC and phone:
> - Scanning the QR code in the GTK window with the phone's new **Scan code** screen paired both devices from
>   scratch, with no code comparison. The dialog closed and both sides showed "Connected".
> - Pings from the phone appear as toasts plus a desktop notification.
> - The tray icon shows in Waybar.
> - `pairlyd` runs as a systemd user service, also started on demand by D-Bus activation.
> - Quitting the window (Ctrl+Q) leaves the daemon and the connection running.
>
> What was built:
> - **QR pairing (core):** `QrInvite` (`pairly://pair?v=1&pk=…&s=…&a=lan:ip:port`). It is one-time and
>   expires after 5 minutes. The PSK is BLAKE2s of the 128-bit secret, and the phone pins `pk`. Tested:
>   reuse, cancel, expiry, and a tampered secret or key.
> - **Parallel dialing:** candidates are dialed in parallel, each 200 ms after the previous, so VPN addresses
>   like Cloudflare WARP don't add a 10 s timeout.
> - **`pairly-gtk`** (relm4 + libadwaita): split view, device page, QR dialog that refreshes itself, code
>   dialog, toasts, an `app.pair` action and `--pair` flag, and single-instance raise.
> - **Tray:** ksni in `pairlyd` (Open, Pair a New Device…, per-device Send Ping, Quit).
> - **Install:** `data/` (icons, `.desktop`, systemd unit, D-Bus activation file) and
>   `make install-user` / `make uninstall-user`.
>
> Changes from the original plan:
> - The GTK UI uses relm4 for the component loop and async commands, but builds the dynamic widgets by hand
>   rather than with `view!`.
> - There is no `pairly://` deep link on Android yet. A web page could fire it and pair the phone without the
>   user's intent, so it would need a confirmation dialog first.
> - **Build memory:** an unbounded release build (LTO with `codegen-units = 1` and 8 parallel jobs) froze the
>   7.5 GB dev machine. The Linux release profile no longer uses LTO, and both the Makefile and the Android
>   Rust script default to 4 jobs.

1. Set up `pairly-gtk` with relm4 and libadwaita, using the async D-Bus proxy from `pairly-dbus`.
2. Build the split view, device list with status badges, and device page with Ping.
3. Build the pairing dialog: QR display (implement the QR flow from section 6.2 in the core here, together with
   the phone-side scanner using CameraX + ZXing) and the SAS view.
4. Add the tray via `ksni` in `pairlyd`, with *Open Pairly*, the device list and *Quit*.
5. Add the desktop entry, icon, systemd user unit and D-Bus activation file in `data/`, plus a
   `just install-user` (or Makefile) target that installs them under `~/.local`.

**Done when:**
- You can pair the phone by scanning the QR from the GTK window.
- The UI updates live through D-Bus signals when the phone connects or disconnects.
- Closing the window leaves the daemon running, and the tray shows it in Waybar.

---

### Phase 5: Notification sync both ways (the first real feature)

> **Status: done (2026-10-03).** Verified with the moto g85 and swaync 0.12.6 on Hyprland:
> - **Phone → PC:** a notification reaches a swaync popup in about 0.3 s, with the app icon and the app's
>   action buttons.
> - **WhatsApp replies:** a reply typed on the PC was delivered, including 6 s after WhatsApp had already
>   removed the notification.
> - **PC → phone:** `notify-send` shows on the phone.
> - **Dismissal:** all four paths sync (phone swipe ↔ PC close, swaync dismiss ↔ phone cancel).
> - **No loops:** exactly one notification per event on each side.
>
> What was built:
> - **Plugin:** the `notification` plugin (posted/removed/active from the source; dismiss/action/reply from
>   mirrors, with a resync on connect) behind a per-plugin `NotificationHost` trait. Covered by an
>   end-to-end test.
> - **Linux mirrors:** shown in the notification server with buttons, an RGBA icon and the
>   `x-pairly-origin` hint. Replies use inline reply or a floating `pairly-gtk --reply` window. Only a
>   *user* dismissal is sent back; an expired popup is not.
> - **Linux monitor:** a dedicated D-Bus monitor connection forwards this PC's notifications. It skips its
>   own, transient, OSD and ignored ones.
> - **Android:** a `NotificationListenerService` (skips ongoing, summary, media, own and muted
>   notifications), mirrors with dismiss/action/reply receivers, an access card and a per-app picker.
>
> Lessons from real apps:
> - **swaync** advertises `inline-reply` even when its config turns the field off, and then hides the
>   action. `pairlyd` reads swaync's config and falls back to the dialog. There's also
>   `[notifications] reply = auto|inline|dialog`.
> - **Chat apps** replace or clear message notifications within seconds (WhatsApp cleared them about 2 s
>   after posting). Actions and replies resolve to the newest notification of the same conversation
>   (`shortcutId` or title), or to a removed notification's saved `PendingIntent`s for 15 minutes.
>
> Not done / limits:
> - Actions on *PC* notifications can't be triggered from the phone, because the spec has no API for
>   invoking another app's actions.
> - PC notifications go to the phone without icons.
> - Linux per-app filtering is `ignore_apps` in `config.toml` only, with no GTK UI yet.

1. Add the `notification` plugin logic in `pairly-plugins`: packet handling, loop-prevention rules and the
   history store.
2. **Android → PC:**
   - `NotificationListener` maps each `StatusBarNotification` (key, package, app label, title, text, actions,
     `RemoteInput`, icon to PNG) to `notification.posted` and `notification.removed`.
   - On the PC, `pairlyd` shows the notification with actions, and its `ActionInvoked` handler sends
     `notification.action` back.
   - Reply: inline if the notification daemon supports it, otherwise a GTK reply dialog. The phone fills the
     `RemoteInput` and fires the `PendingIntent`.
   - Dismissing on the PC sends `notification.removed`, and the phone calls `cancelNotification(key)`.
3. **PC → Android:**
   - the D-Bus monitor picks up `Notify` calls and sends `notification.posted`;
   - the phone posts them to the "PC notifications" channel with the actions;
   - tapping an action sends `notification.action`, and the PC daemon emits `ActionInvoked` on the original
     sender's notification ID (an `org.freedesktop.Notifications` call through the daemon that owns it).
     This works only for apps that are still running. Otherwise just dismiss.
4. Per-app filters in both UIs. Default to excluding persistent/ongoing notifications and Pairly's own.
5. Store notification history for the GTK "recent notifications" view.

**Done when:**
- A WhatsApp or Telegram message on the phone appears on Hyprland within 1 s on LAN, and replying from the
  PC sends a real reply.
- A `notify-send` on the PC appears on the phone.
- Dismissing on either side dismisses on the other.
- No loops: running for a day produces no duplicate storms.

---

### Phase 6: Clipboard, battery, find my phone

> **Status: done (2026-10-04).** Verified with the moto g85 on Hyprland:
> - **Clipboard:** copying on the PC pastes on the phone ("Copied from three"). The phone's
>   clipboard reaches the PC from the device card, the notification action and the Quick
>   Settings tile, with no echo back.
> - **Battery:** both sides show the other's level and charging state, live.
> - **Find my phone:** the phone rings and vibrates on silent until stopped from either side.
> - **Find my PC:** plays an alarm for 30 s, or until stopped from the phone.
>
> What was built:
> - **Plugins** (`clipboard`, `battery`, `findmy`) behind small host traits, covered by an
>   end-to-end test.
> - **Clipboard** is text only, up to 512 KiB, and remembers the last text sent or received so a
>   received clipboard isn't sent back.
> - **Linux:** `wl-paste --watch` / `wl-copy` (skipping `x-kde-passwordManagerHint`), UPower's
>   DisplayDevice, low-battery alerts at 15 %, and Clipboard / Find My Phone rows in GTK, the tray
>   and the CLI (`pairly clip`, `pairly ring [--stop]`). `[clipboard] auto = false` turns the
>   watcher off.
> - **Android:**
>   - a translucent activity that reads the clipboard (it needs focus on Android 10+), used by
>     the QS tile and the notification action;
>   - the sticky battery broadcast;
>   - an alarm-stream ringer with vibration and a stop notification.
>
> Changes from plan:
> - Clipboard uses the `wl-clipboard` tools instead of `wl-clipboard-rs`: they work on every
>   wlroots compositor without another protocol binding.
> - The ringer is a high-priority notification with Stop, not a full-screen activity.
>
> Lessons:
> - **"Find my PC" must ignore the default output.** It was a pair of Bluetooth earbuds at 21 %,
>   so the PC rang silently. `pairlyd` now picks the first non-Bluetooth sink from `pw-dump` and
>   plays at full stream volume.
>
> Not done / limits:
> - Images and files on the clipboard (files go through Phase 7 sharing).
> - GNOME has no data-control protocol, so the automatic watcher only works on wlroots/KDE.
> - The phone can't send its clipboard automatically in the background (Android 10+ blocks it).

1. **clipboard:**
   - **Linux:** a `wl-clipboard-rs` watcher sends changes automatically (a setting: auto or manual) and sets
     the clipboard on receive.
   - **Android:** setting the clipboard works in the background. Sending is manual, through the tile,
     notification action or share sheet.
   - Skip content marked sensitive (Android `ClipDescription.EXTRA_IS_SENSITIVE`, and the Linux
     `x-kde-passwordManagerHint` MIME type).
2. **battery:** send the state on change and on connect. Raise low-battery alerts on the PC.
3. **findmy:**
   - "Ring" makes the phone play an alarm at full volume, overriding DND, with a full-screen stop activity.
   - "Find my PC" plays a sound through `pw-play` / libcanberra.

**Done when:**
- Copying on the PC lets you paste on the phone.
- The tile sends the phone's clipboard to the PC.
- Battery shows on both sides.
- Ring works with the phone on silent.

---

### Phase 7: File, URL and text sharing

> **Status: done (2026-10-04).** Verified with the moto g85 over USB tethering:
> - **1 GB, PC → phone:** 39 s (about 25.6 MB/s, 205 Mbit/s, roughly the tethering link's limit).
>   The SHA-256 matches on the phone.
> - **1 GB, phone → PC, with USB tethering switched off partway and back on:** the transfer resumed
>   and finished, and the SHA-256 matches.
> - **Photos:** 12 at once from Gallery's share sheet arrived in `~/Downloads`.
> - **Links:** both ways. They open in the PC's browser, and as a tap-to-open notification on the phone.
> - **Text:** PC → phone lands on the clipboard with a notification.
>
> Protocol (as built):
> - `share.offer{transfer, name, size, mime}` → `share.accept{offset}`.
> - `share.chunk{offset, data}`: 63 KiB, unreliable, bulk priority. Paced by
>   `share.progress{offset}` (receiver → sender every 1 MiB, with an 8 MiB window).
> - `share.done{sha256}` once everything is confirmed, then `share.finished` or
>   `share.cancel{reason?}` (either side).
> - The receiver only takes the chunk at the offset it expects. After any reconnect or channel
>   swap it re-sends `share.accept` with what it has, and the sender rewinds there.
> - The sender rehashes the prefix if it has to go back. If no resume comes within 30 s of a
>   reconnect, the sender fails the transfer, because the receiver forgot it (restarted).
> - `share.text{text, url}` carries links and text. Only `http(s)` links are ever opened.
> - The receiver strips paths, control characters and leading dots from names, and gives incoming
>   transfers its own local ids.
> - File I/O runs on one OS thread per transfer, using positioned reads and writes.
>
> What was built:
> - **Core:**
>   - the `share` plugin and an end-to-end test that cuts the connection mid-file
>     (`MemoryNetwork::sever`);
>   - the session now drops unreliable packets that were queued for a dead channel.
> - **Linux:**
>   - files are written as `name.part` in `XDG_DOWNLOAD_DIR`, created with `create_new`, then
>     renamed to a unique name;
>   - **Open** / **Show in Folder** notification buttons;
>   - `[share] auto_accept`, `open_urls`, `download_dir`;
>   - D-Bus `SendFiles` / `SendText` / `AcceptTransfer` / `CancelTransfer` / `ListTransfers` and
>     a `TransferChanged` signal;
>   - CLI `pairly send|url|text|transfers`;
>   - GTK: drag-and-drop onto the window or a sidebar device, **Send…**, a **Link or Text** dialog,
>     transfer rows with live progress and Accept/Decline, and toasts;
>   - `pairly-gtk --send [--to <dev>] [files]`, used by the Nautilus script, the Dolphin and Nemo
>     entries, the launcher action and the tray's **Send Files…**.
> - **Android:**
>   - share-sheet target (`ShareActivity`) and a file picker on the device card;
>   - received files go through MediaStore into `Download/Pairly`, kept pending until verified;
>   - descriptors are handed to Rust with `detachFd()`; non-seekable streams are copied to the
>     cache first;
>   - progress notifications with Cancel, an offer notification with Accept/Decline, and an
>     **Ask before receiving files** setting (off by default).
>
> Changes from plan:
> - The SHA-256 is sent only in `share.done`, so sending starts without hashing the whole file
>   first.
> - Links and text use their own `share.text` packet instead of `share.offer`.
> - Linux progress is in the GTK app and the CLI, not in popovers or notifications.
>
> Not done / limits:
> - Transfers resume across disconnects and transport switches, but not across a daemon or app
>   restart: partial files are deleted, and no transfer state is kept in the registry.
> - Folders aren't supported. Send their files, or zip them first.
> - Android 8–9 saves to the app's own folder, because MediaStore Downloads needs Android 10.

1. The `share` plugin with resumable transfers:
   - `share.offer` → `share.accept{resume_offset}` → `share.chunk` stream (64 KiB, bulk priority) →
     `share.done{sha256}`;
   - persist partial `.part` files and transfer state in the registry so a transfer resumes after a
     disconnect or transport switch.
2. Auto-accept from paired devices (a setting), or ask with a notification.
3. **Linux senders:**
   - drag-and-drop onto a GTK device row;
   - `pairly send <dev> <paths…>`;
   - a "Send to phone" action through a `.desktop` file action and a Nautilus/Dolphin/Thunar service menu.
4. **Android senders:** the share-target activity, plus a file picker in the app.
5. URLs open in the default browser on receive (with a setting to ask first). Text goes to the clipboard plus
   a notification.
6. Progress: GTK popover and toasts. On Android, a progress notification with Cancel.

**Done when:**
- A 1 GB file transfers PC → phone and phone → PC on LAN at roughly Wi-Fi speed.
- Killing Wi-Fi mid-transfer and restoring it resumes the transfer.
- The SHA-256 verifies.

---

### Phase 8: Relay server, internet transport, migration

> **Status: built and tested locally (2026-10-04); waiting on the VPS for the mobile-data test.**
> Verified with the relay running on the PC and the moto g85 on USB tethering, with LAN switched
> off on the PC (`[lan] enabled = false`):
> - **Learning the relay:** the phone learned the relay from the PC over LAN, with nothing
>   configured on the phone.
> - **Connecting:** it met the PC on the relay 10 ms after the daemon started.
> - **Traffic:**
>   - ping and text arrived;
>   - a 100 MB file took 4.2 s, its SHA-256 matched, and the relay counted exactly 100 MB;
>   - notifications went both ways, and a WhatsApp reply from the PC was delivered.
> - **LAN back on:** the session moved from relay to LAN 80 ms after connecting. The phone then
>   left the relay (relay stats: 1 connection, 0 pipes).
> - **Automated test:** a real in-process relay fails over when the LAN disappears and moves
>   back while 20 messages are in flight, with none lost or duplicated. Wrong tokens and wrong
>   certificate pins are refused. The test passed 12 times in a row.
>
> Design as built (`pairly-transport-relay::proto`):
> - **Connections and streams:** one QUIC connection per relay. Each request is a stream that
>   starts with `JOIN{role, room[16], token}`.
>   - A *listener* gets `PRESENT`/`ABSENT` as the other device comes and goes, and `MATCHED`
>     when it's called. After that the stream is a pipe, and a new listener is opened.
>   - A *dialer* gets `MATCHED` or `NO_PEER`.
> - **Presence is membership:** a device is "present" from its first listener until it hangs
>   up or disconnects, so re-arming after each call doesn't flap.
> - **Rooms:** `BLAKE2s("pairly-relay-room" ‖ pair_secret)[..16]`. The relay sees only room IDs
>   and Noise ciphertext.
> - **Trust:** the relay certificate is self-signed and clients pin its SHA-256. The address
>   format is `pairly-relay://[token@]host:port/<base32 pin>`, so no domain or CA is needed.
> - **Relay discovery:** devices announce their relay in the encrypted `identity` packet
>   (`Identity.relay`), and peers store it (registry schema v2: a `relay` column). Only the PC
>   needs configuring.
> - **Link ranking:** LAN 100 > BT 60 > Relay 30.
>   - `replaces()` prefers the higher rank before the direction tie-break.
>   - Seeing a better candidate while connected starts an upgrade loop (retries 10 s → 5 min,
>     woken early by a new sighting). A reconnect dials the best kinds first.
>   - LAN mDNS now reports every resolve, so "seen again" triggers the move back home.
> - **Phone battery policy:** relay connections are only kept for devices without a better link
>   (`RelayConfig::only_when_needed`). The PC stays in every room.
> - **Relay limits:** an access token, connections per IP (32), rooms per connection (64), an
>   optional per-pipe rate cap, a 60 s idle timeout, and a statistics log line.
> - **Shutdown:** the relay waits at most 2 s for clients when it stops.
> - **Deployment:** Docker and compose (no-LTO build so it fits a small VPS), a hardened systemd
>   unit, and a step-by-step VPS guide in `pairly-core/crates/pairly-relay/README.md`.
> - **Linux:**
>   - `[relay] address`, and `[lan] enabled = false` for relay-only setups;
>   - a `session on link` log line for every link change.
>
> Changes from plan:
> - There is no full scoring formula with hysteresis. Links have a strict rank, and the session
>   only moves up, and only after the better link completes its handshake. That rules out
>   flapping without needing timers.
> - The old channel is replaced at once, with no 2 s grace period. Unacked packets are resent
>   and duplicates dropped.
> - The relay address is learned from the paired PC instead of being typed into both UIs.
>
> Not done / limits:
> - **The mobile-data test needs the VPS** (follow the relay README, then put the address in
>   `config.toml`).
> - Pairing still needs both devices on the same network (or a QR code reachable on it). The
>   relay is only used for paired devices.
> - Hole punching and UnifiedPush (8b) are not started.
> - Moving *down* from LAN waits for the 45 s keepalive timeout, so leaving home takes up to
>   about 45 s to switch to the relay.
> - Networks that block UDP entirely can't reach the relay. There is no TCP fallback yet.
> - Android has no UI to set its own relay; it always uses the PC's.

1. `pairly-relay`:
   - QUIC listener and the room protocol (`JOIN`, `PAIRED`, then opaque piping);
   - limits and metrics (connection count, bytes);
   - a Dockerfile and a systemd unit.
2. Deploy it to a cheap VPS, or run it locally with Tailscale for testing. Put the default relay URL in the
   config, editable in both UIs.
3. `pairly-transport-relay`:
   - connect and join every paired device's room;
   - expose the result as a `Duplex`;
   - reconnect with backoff.
4. The full **transport manager**: candidate table, scoring, hysteresis, migration that opens the new channel
   first, and unacked resend (section 7).
5. Android relay policy: connect to the relay only when LAN and BT are down, and use long keepalives on
   battery.
6. **8b (stretch):** hole punching through the relay rendezvous, then **UnifiedPush** wake-ups.

**Done when:**
- With the phone on mobile data, notifications still sync through the relay.
- Walking back onto home Wi-Fi switches the link badge to LAN within about 10 s, with no lost or duplicated
  notifications.
- A packet capture on the relay shows only ciphertext.

---

### Phase 9: Bluetooth transport

1. **Linux:** `pairly-transport-bt` with `bluer`:
   - register an RFCOMM profile with the Pairly UUID;
   - accept and dial connections;
   - expose them as a `Duplex`;
   - the target is the BT address stored at pairing, learned from the identity packet.
2. **Android:** `BluetoothTransport.kt`:
   - `listenUsingRfcommWithServiceRecord(UUID)` server, plus a client socket to the PC;
   - pump bytes to and from Rust through the UniFFI `ForeignTransport` / `ForeignDuplex` callback interfaces
     (a read loop in Kotlin, writes through a Rust-called callback).
3. Plug both into the transport manager (BT score 60, and probe only while LAN is down).
4. The OS-level Bluetooth pairing must already exist (bonded devices). Pairly's own Noise pairing still runs
   on top. The UI should explain this.

**Done when:**
- With Wi-Fi off on both devices and the relay disabled, notifications and clipboard work over BT.
- Turning Wi-Fi back on migrates to LAN.

---

### Phase 10: More KDE Connect features

Build each plugin separately. Each one is about a week of work.

1. **media:** MPRIS ↔ MediaSession. Show the PC's player (play/pause, next, seek, volume, artwork) on the
   phone, and the phone's player in GTK.
2. **input:**
   - an Android touchpad screen (gestures for move, two-finger scroll, tap to click) and a keyboard;
   - on Linux, apply them through the portal (libei) with a `uinput` fallback.
3. **sms:**
   - Android reads threads and new SMS and sends SMS;
   - GTK gets a conversation view (an `AdwNavigationView` with a message list and a compose bar).
4. **command:**
   - define commands on the PC (name plus shell command) in GTK settings;
   - the phone lists and runs them;
   - confirm before running, and run only commands from the allowlist.
5. **telephony:** show incoming calls on the PC and pause MPRIS players while a call is ringing or active.
6. Stretch: presenter mode, contacts sync, and file browsing (SFTP-like).

**Done when:** each plugin has its own checklist, and each works over LAN, BT and the relay.

---

### Phase 11: Hardening and security review

1. Switch the registry to **SQLCipher**, keyed from Secret Service (Linux) or the Keystore-wrapped key
   (Android).
2. Run `cargo-fuzz` targets on the frame decoder, CBOR envelope decoding and every packet body parser.
3. Implement Noise rekeying, enforce size limits everywhere, and add backpressure to every queue.
4. Add optional relay padding and document the metadata the relay can see.
5. Write a threat-model document, and review the pairing flows against MITM, replay, downgrade and DoS.
6. Run `cargo deny` (licenses and advisories), and Android lint plus StrictMode in debug builds.
7. Measure battery use on Android over 24 h idle connected (LAN, then relay), and tune the keepalives.

**Done when:**
- The fuzzers run for an hour with no crashes.
- The battery drain target is met (≤ 2 %/day idle on LAN as a goal).
- The threat model is written.

---

### Phase 12: Packaging and release

See [section 14](#14-packaging-and-release).

**Done when:** v0.1.0 is tagged, an AUR package is installable, and a signed APK is on GitHub Releases.

---

## 13. Testing strategy

| Level | What | How |
|---|---|---|
| Unit | Codec, crypto, session ack/dedup, scoring | `cargo nextest` in `pairly-core` |
| Integration | N in-process nodes over memory, LAN loopback and a local relay | tokio tests that spawn the relay in-process |
| Network chaos | Packet loss and latency during migration | Linux `tc netem`, plus a "flaky" memory transport wrapper |
| FFI | Kotlin ↔ Rust round trips | Android instrumented tests on an emulator (x86_64 `.so`) |
| Android UI | Onboarding and pairing screens | Compose UI tests |
| End to end | Real phone + real PC checklist per phase | Manual checklist in each phase's **Done when** |
| Fuzz | Decoders | `cargo-fuzz` (Phase 11) |

---

## 14. Packaging and release

### Linux

- **AUR / PKGBUILD** first, since you are on EndeavourOS. It installs the binaries, the systemd user unit,
  the D-Bus service file, the `.desktop` file, icons and the metainfo.
- `.deb` through `cargo-deb`, for Ubuntu/Debian users.
- **Flatpak caveat:** inside a Flatpak, the daemon cannot `BecomeMonitor` on the session bus or reach `uinput`.
  Options: ship only the UI as a Flatpak and the daemon natively, or keep Flatpak out of v1.

### Android

- Release keystore, signed APK and AAB, and GitHub Releases. Submit to **F-Droid**, which builds from source,
  so the Rust build must be reproducible: pinned toolchain and `--locked`.
- Google Play later, if ever. The SMS permission declaration and the background-service policy need justification.

### Relay

- A Docker image (`ghcr.io/<you>/pairly-relay`) and a sample `docker-compose.yml`, with the TLS setup
  documented.

---

## 15. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Android OEMs kill the background service | Missed sync | Foreground service, battery-optimization exemption, dontkillmyapp guidance, UnifiedPush wake (stretch) |
| Background clipboard read blocked on Android 10+ | Clipboard is one-way automatic | Tile, notification action and share sheet; documented as a platform limit |
| GNOME lacks Wayland data-control | No automatic clipboard on GNOME | Manual send from the UI or a hotkey; Hyprland and KDE work |
| Remote input on Wayland is fragmented | Input plugin fails on some compositors | Portal/libei first, `uinput` fallback with udev setup |
| `mdns-sd` multicast issues on some phones | No LAN discovery | MulticastLock, NsdManager fallback, manual "add by IP", QR containing the LAN address |
| UniFFI async and callback complexity | Slower Android progress | Keep the FFI surface small and coarse (node-level API only) |
| Rust learning curve (snow, quinn, zbus all at once) | Schedule slip | Phases add one new library family at a time |
| Relay cost or abuse | Server bill, bans | Limits, an optional access token, self-hosting as the default recommendation |

---

## 16. Open decisions

Decide these when you reach the relevant phase. Each one has a default.

1. **App ID / reverse-DNS name:** default placeholder `dev.pairly`. Change it before the first release.
2. **UI builder for GTK:** relm4 widget macros (default) or Blueprint files with plain gtk4-rs.
3. **DI on Android:** manual (default) or Hilt.
4. **Default relay:** your VPS (default), or no default with users entering their own.
5. **KDE Connect compatibility bridge:** not in v1. Revisit after Phase 10.
6. **License:** GPL-3.0 (matching the ecosystem, and needed if code is ever shared with KDE Connect) or
   MIT/Apache-2.0.

---

### Next step

Start **Phase 0**: install the toolchains, then generate the three skeletons (the `pairly-core` workspace, the
`pairly-linux` workspace and the Android Gradle project) so that everything builds.
