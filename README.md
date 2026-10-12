# 📱🔗💻 Pairly

**Your phone and your computers, working as one.** Pairly links your Android phone with your
Linux and Windows PCs, like KDE Connect or Phone Link: notifications, texts, calls, files,
clipboard, media, screens and remote control, all end-to-end encrypted.

🏠 At home over Wi-Fi · 📶 over Bluetooth with no network · 🌍 from anywhere through your own free relay

[![Latest release](https://img.shields.io/github/v/release/ABHILESH1412/pairly?label=download)](https://github.com/ABHILESH1412/pairly/releases/latest)
[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)

---

## 📚 Contents

- [✨ What it does](#-what-it-does)
- [🔌 How devices connect](#-how-devices-connect)
- [⬇️ Install](#️-install)
- [🤝 Pair your devices](#-pair-your-devices)
- [🌍 Use it away from home (free relay)](#-use-it-away-from-home-free-relay)
- [🗂️ Browse files between devices](#️-browse-files-between-devices)
- [🔄 Updates](#-updates)
- [🔒 Security and privacy](#-security-and-privacy)
- [🛠️ Troubleshooting](#️-troubleshooting)
- [👩‍💻 Build from source](#-build-from-source)
- [📜 Licence](#-licence)

---

## ✨ What it does

### 🔔 Notifications
- 📲 → 💻 **Phone notifications on your PC**, with their buttons and a reply box. Closing one on
  the PC clears it on the phone.
- 💻 → 📲 **PC notifications on your phone.** You choose which apps may send them.

### 💬 Texts, 📞 calls and 👥 contacts
- **Read and send texts** from the PC, pictures included.
- **See who's calling**; answer (also on speaker), reject or hang up from the PC. Your music
  pauses while the phone rings.
- **Contacts:** call or text anyone from the PC.

### 📁 Files
- **Send files** both ways: pick them, drag and drop them on the window, or use your phone's
  share menu. Received files land in `Downloads/Pairly`.
- **Browse another device's files** and download what you need, from anywhere: the PC browses
  the phone, the phone browses a PC, a PC browses another PC. [More below](#️-browse-files-between-devices).
- **Send to Device** right from your file manager (Nautilus, Dolphin, Nemo) on Linux.

### 📋 Clipboard and links
- **Copy on one device, paste on the other**, automatically or when you ask.
- **Links** you send open in the browser on the other side; text is copied.

### 🖥️ Screens
- **See and control your phone's screen** on the PC: click, type, scroll.
- **See and control your PC's screen** on the phone, with zoom and rotate.
- **PC to PC:** see and control your other computer (Linux ↔ Windows).

### 🖱️ Remote control
- **Touchpad and keyboard:** your phone moves the PC's mouse and types on it.
- **Presenter:** next and previous slide, with a 🔴 **laser pointer** drawn on the PC's screen.
- **⌨️ Type on your phone with the PC's keyboard** while you view its screen, arrow keys included.

### 🎵 Media
- **Control the PC's music and videos** (Spotify, browsers…) from the phone, lock screen included.
- **Control the phone's music** from the PC.

### 🔋 And more
- **🔍 Find my phone:** rings it at full volume, even on silent. The phone can ring the PC too.
- **🔋 Battery:** see each device's battery.
- **⚡ Power:** lock, restart or power off the PC from the phone; lock or power off the phone from the PC.
- **⌨️ Commands:** run your own commands on the PC from the phone.
- **⏸️ Pause a device:** cut one device off completely (it can't connect at all) until you
  resume it, without unpairing.
- **🔌 On/off switch:** turn Pairly off everywhere with one switch.
- **🌗 Light and dark themes**, and rename your devices whenever you like.

### 🧩 What each app can do

| Feature | 🐧 Linux | 🪟 Windows | 🤖 Android |
|---|:---:|:---:|:---:|
| Notifications both ways | ✅ | ✅ | ✅ |
| Texts, calls, contacts | ✅ (from the phone) | ✅ (from the phone) | ✅ (shares them) |
| Send files, links, text | ✅ | ✅ | ✅ |
| Browse other devices' files | ✅ | ✅ | ✅ (a PC's shared folder) |
| Share its own files | ✅ (a folder you pick) | ✅ (a folder you pick) | ✅ (its storage) |
| Clipboard both ways | ✅ | ✅ | ✅ |
| See and control the other screen | ✅ | ✅ | ✅ |
| Phone as touchpad, keyboard, presenter | ✅ | ✅ | ✅ |
| Media control | ✅ | ✅ | ✅ |
| Find my phone, battery, power | ✅ | ✅ | ✅ |
| Updates itself | ✅ | ✅ | ✅ |

---

## 🔌 How devices connect

Pairly always uses the best link it has, and moves between them without losing anything:

| Link | When | Speed |
|---|---|---|
| 🏠 **Wi-Fi** (same network) | At home or the office. Devices find each other by themselves. | ⚡⚡⚡ fastest |
| 📶 **Bluetooth** | No shared network, devices close by (paired in Bluetooth settings first). | ⚡ slow, fine for messages |
| 🌍 **Internet relay** | Anywhere else: mobile data, another Wi-Fi. | ⚡⚡ good |

📺 **Screen sharing needs the same Wi-Fi**; everything else works over all three.

---

## ⬇️ Install

All downloads are on the [**Releases page**](https://github.com/ABHILESH1412/pairly/releases/latest).

### 🤖 Android (8 or newer)

1. Download **`pairly-X.Y.Z.apk`** on your phone and open it.
2. If Android asks, allow your browser or Files app to **install unknown apps**.
3. If **Play Protect** warns you, tap **More details → Install anyway**. It warns about every
   app that doesn't come from the Play Store.

📌 Only the first install needs this: after that, Pairly updates itself.

### 🪟 Windows 10 or 11

1. Download **`pairly-X.Y.Z-windows-x64-setup.exe`** and run it. No admin rights needed.
2. If Windows shows **"Windows protected your PC"**, click **More info → Run anyway**.
3. When the Windows firewall asks, **allow Pairly on private networks**, so your phone can find
   your PC.

Pairly starts with Windows and lives in the tray 🔽.

### 🐧 Linux

**Arch, EndeavourOS, Manjaro:** download **`PKGBUILD`** and **`pairly.install`** into an empty
folder, then run:

```sh
makepkg -si
systemctl --user enable --now pairlyd
```

The build takes a few minutes and about 4 GB of RAM.

**Any other recent distribution:** download **`pairly-X.Y.Z-linux-x86_64.tar.gz`**, which is
already built (it needs GTK 4, libadwaita and glibc 2.44 or newer):

```sh
tar xzf pairly-*-linux-x86_64.tar.gz
cd pairly-*-linux-x86_64
make install activate      # installs for your user under ~/.local (no root)
```

Then open **Pairly** from your app menu.

**If your PC has a firewall,** allow Pairly's port (UDP 47100) and device discovery (mDNS):

```sh
sudo firewall-cmd --permanent --add-port=47100/udp --add-service=mdns && sudo firewall-cmd --reload
```

<details>
<summary>🖱️ Remote control on Linux: which desktops work</summary>

- **Hyprland, Sway and other wlroots compositors:** works directly.
- **GNOME and KDE:** goes through the desktop's RemoteDesktop portal (not tested yet).
- **Anywhere else:** the `uinput` fallback works if your user may write to `/dev/uinput`
  (recent systemd allows that for the logged-in user).
</details>

---

## 🤝 Pair your devices

1. 💻 On the PC, click **➕ Pair a New Device**: a QR code appears.
2. 📱 On the phone, tap **Scan code** and point it at the QR code.
   (No camera? Pick the PC in the phone's list of nearby devices instead, and **check that both
   show the same code**.)
3. ✅ Done. Each device now shows the other's page, with everything you can do.
4. 📲 On the phone, turn on the features you want. Pairly asks Android for each permission when
   you first use it:

| Permission | For |
|---|---|
| 🔔 Notification access | Sending the phone's notifications to the PC |
| 💬 SMS, 📞 phone, 👥 contacts | Texts, calls and contacts on the PC |
| 📁 All files access | Letting the PC browse the phone's storage |
| ♿ Accessibility service | Locking the phone, typing from the PC, automatic clipboard |
| 📺 Screen capture | Showing the phone's screen on the PC (asked each time) |

---

## 🌍 Use it away from home (free relay)

To reach your PC from mobile data or another network, Pairly needs a meeting point on the
internet. You can run your own **for free on Cloudflare**, no server needed, in about 5 minutes:

1. Follow the [**relay guide**](pairly-relay-worker/README.md). It gives you an address like
   `wss://TOKEN@pairly-relay.yourname.workers.dev`.
2. Put that address into Pairly **on your PC**:
   - 🐧 **Linux:** Settings → **Away from home** → Relay address
   - 🪟 **Windows:** Settings → **Connections** → Relay
3. 📱 **Your phone learns it automatically** the next time it's connected to that PC at home.

That's it: leave home, and your devices stay connected. 🎉

- 🔐 The relay only passes on encrypted data. Cloudflare can't read anything, and only your
  token gets in.
- 🔋 It's light on the battery: an idle connection only checks in every 2 minutes.
- 🖥️ Prefer your own server? There's a [self-hosted relay](pairly-core/crates/pairly-relay/README.md) too.

---

## 🗂️ Browse files between devices

Forgot a file at home? Open the other device in Pairly and tap **🗂️ Browse Files**. It works
over Wi-Fi, Bluetooth and the internet relay.

**📲 A phone** shares its storage (once you allow file access).

**💻 A PC** shares only what you choose. In Settings → **Share Files**:

| Setting | What it does |
|---|---|
| **Share a folder with my devices** | Off until you turn it on |
| **Folder** | Which folder (your home folder unless you pick another) |
| **Allow changes** | Off: others can only look and download. On: they may also upload, rename and delete there. |

🙈 **Hidden files are never shared** (like `.ssh` with your keys, or `.config`), nor Windows'
hidden and system files.

---

## 🔄 Updates

All three apps **update themselves** when a new version is released here, and tell you when
they did. You can switch this off in each app's Settings → **Updates**.

- 🔏 **Signed:** updates are only installed if they carry the project's signature. A tampered
  download, or a hijacked GitHub account, can't push anything to your devices.
- 🐧 **Linux package installs** (`makepkg`, under `/usr`) are left to your package manager:
  Pairly tells you when there's a new version.

---

## 🔒 Security and privacy

- 🔐 **End-to-end encryption:** every connection is a [Noise](https://noiseprotocol.org/)
  session with keys pinned at pairing, renewed every hour.
- 🤝 **Only your devices:** pairing needs the QR code, or both people confirming the same
  six-digit code. A stranger on your Wi-Fi can't connect, watch your screen or read anything.
- ⏸️ **Pause** cuts a device off completely at the connection itself, not just in the app.
- 🌍 **The relay is blind:** it only sees encrypted, padded traffic.
- 🗝️ **Secrets at rest** are kept in your keyring (Linux), the Windows data folder, or the
  Android Keystore.
- 📁 **File sharing is off by default** on PCs, read-only unless you allow changes, and never
  includes hidden files.
- 🚫 **No accounts, no tracking, no ads, no cloud of ours.**

The details, including what a paired device is trusted to do, are in
[docs/security.md](docs/security.md).

---

## 🛠️ Troubleshooting

<details>
<summary>📱 Android won't install the APK</summary>

- **"Not allowed to install unknown apps":** tap **Settings** in the message and allow your
  browser or Files app.
- **"Blocked by Play Protect":** tap **More details → Install anyway**.
- **"App not installed" / "conflicts with an existing package":** an older Pairly may be
  installed in another profile on the phone (work or private profile). Uninstall it there.
</details>

<details>
<summary>🔍 My devices don't see each other</summary>

- Both must be on the **same Wi-Fi** (or set up the [relay](#-use-it-away-from-home-free-relay)).
- **Firewall:** allow UDP 47100 and mDNS on Linux, and allow Pairly in the Windows firewall
  prompt (private networks).
- Some routers (guest networks, "client isolation") stop devices talking to each other: use the
  relay, Bluetooth, or USB tethering.
</details>

<details>
<summary>🐢 Switching to the relay takes a while after I leave Wi-Fi</summary>

When Wi-Fi fades away, it looks to Pairly as if the link simply went quiet, so it waits up to
about 45 seconds before switching to the relay. Turning Wi-Fi off by hand also works.
</details>

<details>
<summary>🖱️ Remote control doesn't move the mouse on Linux</summary>

See [Remote control on Linux](#️-install): wlroots desktops work directly, others need the
portal or `/dev/uinput` access.
</details>

---

## 👩‍💻 Build from source

| Part | Folder | Command |
|---|---|---|
| 🐧 Linux app and service | [`pairly-linux`](pairly-linux) | `make install-user` (installs under `~/.local` and starts the service) |
| 🪟 Windows app | [`pairly-windows`](pairly-windows) | `cargo run` in `src-tauri` (needs the Visual Studio C++ build tools) |
| 🤖 Android app | [`pairly-android`](pairly-android) | `./gradlew installDebug` (needs the Android SDK and NDK, and `cargo-ndk`) |
| 🌍 Relay (Cloudflare) | [`pairly-relay-worker`](pairly-relay-worker) | `npx wrangler deploy` |
| 🖥️ Relay (own server) | [`pairly-core`](pairly-core) | `cargo build --release -p pairly-relay` |

🦀 All apps share one core written in Rust ([`pairly-core`](pairly-core)): the encryption,
pairing, connections and every feature's protocol.

📖 More details are in each folder's README. [docs/releasing.md](docs/releasing.md) explains how
releases are made, and [plan.md](plan.md) is the design and build log.

---

## 📜 Licence

Pairly is free software under the [GPL-3.0-or-later](LICENSE) licence. 💚
