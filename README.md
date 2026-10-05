# Pairly

Connect your Android phone and your Linux PC, like KDE Connect: notifications, texts, calls,
files, clipboard, media and remote input. Everything is end-to-end encrypted, over Wi-Fi,
Bluetooth or your own relay.

## What it does

| On the PC | On the phone |
|---|---|
| Phone notifications, with replies, buttons and dismissal | PC notifications |
| Read and send texts | Send files, links and text to the PC |
| See calls; answer (also on speaker), reject or hang up | Use the phone as a touchpad, keyboard or presenter remote |
| Browse, download and upload the phone's files | Control the PC's music and video players |
| Contacts: call or text from the PC | Run the PC's commands; lock, restart or power it off |
| Clipboard both ways, automatically | The PC's media controls on the lock screen |
| Find your phone (rings it, even on silent), see its battery | |
| Send files and links to the phone; lock or power off the phone | |

It works:
- **on the same network**, over Wi-Fi with automatic discovery;
- **over Bluetooth**, when there is no shared network;
- **anywhere**, through a small relay server you host yourself (see the
  [relay guide](pairly-core/crates/pairly-relay/README.md)).

It always uses the best link available and switches without dropping anything.

## Install

**Linux (Arch, EndeavourOS, Manjaro)**

Download `PKGBUILD` and `pairly.install` from the latest
[release](https://github.com/ABHILESH1412/pairly/releases) into an empty folder. Then, in that
folder:

```sh
makepkg -si
systemctl --user enable --now pairlyd
```

Then open **Pairly** from the app menu. The build takes a few minutes and about 4 GB of RAM.

**Other distributions:** build from source (below). Pairly needs GTK 4 and libadwaita.

**Remote input:** on Wayland it works directly on wlroots compositors (Hyprland, Sway). GNOME
and KDE go through the RemoteDesktop portal, which isn't tested yet. The `uinput` fallback works
anywhere the user can write to `/dev/uinput` (recent systemd grants that to the logged-in user).

**Android 8 or newer:** download `pairly-X.Y.Z.apk` from
[Releases](https://github.com/ABHILESH1412/pairly/releases) and install it.

**Pairing:** on the PC choose **Pair a New Device**, and on the phone tap **Scan code**.
Then turn on, in the app, whichever phone features you want:
- notification access;
- texts and calls;
- file access;
- the accessibility service, for locking and automatic clipboard.

If the PC runs a firewall, allow UDP port 47100 and mDNS, for example:

```sh
sudo firewall-cmd --permanent --add-port=47100/udp --add-service=mdns && sudo firewall-cmd --reload
```

## Security

- **Encryption:** every connection is a [Noise](https://noiseprotocol.org/) session with keys
  pinned at pairing time, and the keys are renewed every hour.
- **Pairing:** it needs the QR code, or both people confirming the same six-digit code. A
  stranger on your Wi-Fi can't connect or read anything.
- **The relay:** it only sees encrypted, padded traffic.
- **Secrets at rest:** they're kept in your keyring (Linux) or the Android Keystore.

The details, including what a paired device is trusted to do, are in
[docs/security.md](docs/security.md).

## Build from source

| Part | Folder | Command |
|---|---|---|
| Linux app and daemon | `pairly-linux` | `make install-user` (installs under `~/.local` and starts the service) |
| Android app | `pairly-android` | `./gradlew installDebug` (needs the Android SDK and NDK, and `cargo-ndk`) |
| Relay | `pairly-core` | `cargo build --release -p pairly-relay` |

The details are in each folder's README. [plan.md](plan.md) is the design and build log.
[docs/releasing.md](docs/releasing.md) describes how releases are made.

## Licence

[GPL-3.0-or-later](LICENSE).
