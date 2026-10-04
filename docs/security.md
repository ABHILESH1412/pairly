# Pairly security model

What Pairly protects, from whom, how, and where the limits are. The design is in
[`plan.md` §6](../plan.md#6-security-design). This document records the Phase 11 review of it
against the code.

## 1. What is protected

- **Content:**
  - notifications;
  - texts;
  - call details;
  - contacts;
  - clipboard;
  - files;
  - media state;
  - remote input;
  - commands.
- **Control of each device:**
  - a paired PC can read the phone's files, texts and contacts, and lock or power it off;
  - a paired phone can type and click on the PC, run its listed commands, and lock or power
    it off.
- **Long-term secrets:**
  - each device's X25519 identity key;
  - the per-pairing `pair_secret`, which derives the relay room;
  - the relay's access token.

## 2. Who might attack, and what they get

| Attacker | Reads content | Controls a device | Why not |
|---|---|---|---|
| Someone on the same Wi-Fi (passive) | No | No | Every link is a Noise session with fresh ephemeral keys. |
| Someone on the same Wi-Fi (active) | No | No | Unpaired keys are refused right after the first handshake message. Pairing needs the user's code check or the QR code. |
| The relay operator | No | No | The relay only joins two Noise streams by room ID. It sees metadata (§6). |
| A man-in-the-middle during pairing | No | No | QR: the PSK and the pinned key from the code. Code comparison: commit/reveal, so one 1-in-10⁶ guess per attempt (§3). |
| Someone who steals a key later | Not past traffic | Yes, until unpaired | Forward secrecy from ephemeral keys, and rekeying every hour or GiB. A stolen identity key can impersonate the device; unpair it from the other device. |
| Someone near the phone over Bluetooth | No | No | Bluetooth carries the same Noise sessions. Pairing still needs the code or QR. |
| A malicious **paired** device | Yes | Yes | By design: pairing grants every feature (§5). |
| Malware on the phone or PC, as the user | Yes | Yes | Out of scope: the OS sandbox and user account are the boundary. |

## 3. Pairing review (MITM, replay, downgrade, DoS)

**QR flow (`Noise_XXpsk3`)**
- The code carries the PC's public key, a one-time 128-bit secret, and addresses.
- The phone proves it saw the code (the PSK).
- The PC proves it is the device in the code: the phone checks the handshake's static key
  against the key in the code (`a different device answered`).
- A code pairs once (the first completed handshake consumes it) and expires after 5 minutes.
- *Limit:* while the code is on screen, anyone who photographs it can pair without a further
  prompt. The PC shows "Paired with …" afterwards; unpair if it wasn't you.

**Code comparison (`Noise_XX` + SAS)**
- A code derived only from the handshake hash could be brute-forced by a MITM that controls
  both legs.
- The commit/reveal exchange prevents that:
  - the responder commits to `n_r` before it sees `n_i`;
  - the initiator reveals `n_i` before it sees `n_r`.
- So a MITM's two codes match with probability 10⁻⁶ per attempt, and the users see a
  mismatch or a failed pairing.
- Pairing is stored only after both users accept.

**Downgrade**
- The 2-byte hello (`[version, kind]`) is bound into the Noise prologue. Rewriting it (for
  example from QR to code pairing) makes the handshake fail.
- A version mismatch is refused.
- The phone chooses the QR flow itself when it scans, so a network attacker can't steer it to
  the code flow.

**Replay**
- Noise uses fresh ephemerals per connection, and transport nonces are per channel.
- Session packet IDs plus the dedup window drop replays across reconnects.
- Replaying a recorded `IK` first message completes only the responder's side. The attacker
  can't derive the transport keys, so the identity exchange fails within 10 s and nothing is
  attached.
  - *Fixed in this review:* a Bluetooth address was recorded at that point, so a replay over
    Bluetooth could point the PC at the wrong address. It is now recorded only after the
    identity exchange proves the key live.

**Denial of service (fixed in this review)**
- At most **16 inbound connections** may be in their unauthenticated handshake at once; more
  are dropped unanswered. Each handshake times out after 10 s.
- At most **3 pairing prompts** wait at once.
- After an incoming request is declined or ignored, new ones are refused for **10 s**. This is
  per node, because an attacker can mint new device IDs at will.
- **Outgoing queues are bounded:**
  - at most 512 unreliable packets per priority, after which new ones are dropped;
  - at most 4,096 reliable packets awaiting acks, after which the send fails with "backlog".
- Inbound packets are read only as fast as the plugins consume them (a bounded 256-event
  channel per peer), so a fast sender can't grow memory.

## 4. Channel protection

- **Cipher:** `Noise_*_25519_ChaChaPoly_BLAKE2s`. QUIC/TLS around it is only an outer layer and
  is not relied on.
- **Rekeying (added in this review):**
  - each direction replaces its key every **1 GiB** or **1 hour** (Noise `REKEY`);
  - the switch is marked in-band on the last frame under the old key, so it needs no round
    trip and can't desynchronise;
  - a key leaked later can't decrypt earlier traffic in the same session.
- **Size limits:**
  - frames up to 64 KiB;
  - reassembled packets up to 1 MiB;
  - CBOR nesting up to 32 levels;
  - packet type names up to 64 characters (`[a-z0-9._]`);
  - per-field limits in each plugin (names, texts, attachments, file chunks).

## 5. What pairing grants

Pairing is all-or-nothing: a paired device is trusted with every feature the other side has
turned on.

| A paired PC can… | A paired phone can… |
|---|---|
| Read notifications, texts, contacts, call state | Read the PC's notifications and clipboard |
| Browse, download, upload, rename and delete files in the phone's shared storage (after "All files access") | Type and click on the PC (remote input) |
| Send texts and place, answer and end calls | Run the PC's **listed** commands (only those in `commands.json`, confirmed on the phone) |
| Lock the phone, open its power menu, press Power off or Restart (accessibility service) | Lock, power off or restart the PC (`[power] from_phone = false` turns this off) |
| Read what's copied on the phone (accessibility service, if on) | Send files and links (links open in the browser) |

Each Android permission is asked for separately and can be withdrawn in system settings:
- notification access;
- SMS and contacts;
- phone;
- all-files access;
- accessibility.

**Not yet:** per-device permissions (for example, a second PC that may receive files but not
browse the phone).

## 6. What the relay can see

The relay can see:
- **the IP addresses** of both devices, and when they connect and disconnect;
- **the room ID**, a 16-byte BLAKE2s hash of the pairing's secret. It is stable for a pairing,
  so the relay can tell that the same two devices meet again, but not who they are;
- **the access token**, if one is configured;
- **traffic volume and timing**.

With padding on (the default, `[relay] padding = true`), frame sizes are visible only to
256-byte steps. Padding is inside the Noise encryption, so it can't be stripped. It costs
about 128 bytes per frame.

The relay can't see device names, IDs or keys, or any content. Self-hosting removes the third
party; the relay's README covers deploying one.

**Not yet:** rotating the room ID over time, to stop session linking.

## 7. Data at rest

| Data | Linux | Android |
|---|---|---|
| Identity key | The desktop keyring (Secret Service: gnome-keyring or KWallet), encrypted with the login password. A `0600` `identity.key` file only when there is no keyring. | Wrapped with a non-exportable Android Keystore AES key, in app-private storage |
| `pair_secret` and the relay address (with its token) | **Sealed** in `registry.db` (ChaCha20-Poly1305), with a key derived from the identity key | **Sealed** the same way, in app-private storage |
| Device names, public keys, Bluetooth addresses | `registry.db`, readable (not secret) | App-private storage |
| Contacts (vCard), message pictures, artwork | `~/.local/share/pairly/contacts`, `~/.cache/pairly`, both made 0700 | App cache |
| Received files | `~/Downloads`, normal permissions | Downloads |

Sealing (`pairly_crypto::FieldKey`):
- **Key:** `BLAKE2s("pairly-field-key" ‖ "registry" ‖ identity secret)`, so the registry's
  secrets are exactly as protected as the identity, and there is no second key to store.
- **Binding:** each value's associated data names its field and device, so a sealed value
  can't be moved to another field or row.
- **Migration:** a plaintext registry (schema 3) is sealed in place in one transaction on first
  start (schema 4).

Keyring rules (Linux):
- The old `identity.key` moves into the keyring and is deleted only after the keyring gives the
  same key back.
- At login, pairlyd can start before the keyring is unlocked. It then waits up to a minute for
  the login to unlock it before asking with an unlock prompt.
- If the keyring is unavailable while devices are paired, pairlyd **refuses to start** with an
  explanation, instead of making a new identity (which would orphan every pairing).

If an identity is ever lost anyway (for example, Android clears the Keystore on reinstall),
the pairings it sealed can't be opened. They are removed at start with a warning, because the
other devices pinned the old key, and the devices are paired again.

The full-database encryption the plan first named (SQLCipher) was dropped: names and public
keys aren't secret, and bundling OpenSSL into the Android app wasn't worth it.

## 8. Verification

- **Fuzzing** (`pairly-core/fuzz`, `cargo +nightly fuzz run <target>`):

  | Target | What it covers |
  |---|---|
  | `envelope` | every packet body type, plus identity validation |
  | `handshake` | all three responder kinds, and the initiator's reply handling |
  | `relay_join` | the relay's first read |
  | `text` | QR codes, relay addresses, Bluetooth addresses, received file names |
  | `files_path` | file browsing can't leave the shared folder, even through a symlink |

  Run for an hour in total with no crashes.
- **`cargo deny check`** in both workspaces:
  - no known vulnerabilities (RustSec);
  - permissive licences only (plus MPL-2.0 for UniFFI);
  - crates.io as the only source.
- **Tests:**
  - pairing, rejection and cooldown, strangers refused, QR one-time use and expiry;
  - impostor keys, rekeying, padding, backlog limits;
  - each plugin end to end.
- **Android:**
  - lint is clean;
  - StrictMode logs disk and network work on the main thread in debug builds. What it found
    was fixed: the message-store watcher now runs on its own thread, and settings files are
    read in the background at startup.
