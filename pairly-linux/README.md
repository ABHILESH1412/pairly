# pairly-linux

Linux app: `pairlyd` daemon, `pairly-gtk` (GTK4 + libadwaita UI, Phase 4), the `pairly` CLI,
the D-Bus interface crate (`io.github.abhilesh1412.Pairly.Daemon1`) and the Bluetooth transport (Phase 9).
Depends on `../pairly-core` by path. See `../plan.md` section 9.

## Install (current user, no root)

```sh
make install-user      # release build, installs to ~/.local, enables the pairlyd user service
make uninstall-user    # removes it again; pairings in ~/.local/share/pairly are kept
```

This installs `pairlyd`, `pairly` and `pairly-gtk`, the app launcher and icons, a systemd
user unit (`pairlyd.service`, started at login) and a D-Bus activation file, so any client
starts the daemon on demand. The build uses 4 parallel jobs (`JOBS=8` to change); each
rustc can need 1–2 GB of RAM.

Open **Pairly** from your launcher, or the tray icon. Click **+** to show a QR code and scan it
with the Android app's **Scan code** button.

## Try it from the source tree

```sh
cargo build
./target/debug/pairlyd                 # terminal 1 (logs: PAIRLY_LOG=debug)
./target/debug/pairly devices          # terminal 2
./target/debug/pairly pair <id|name>   # compare the code on both devices
./target/debug/pairly ping <id|name> "hello"
./target/debug/pairly listen           # answer incoming pairing requests, show pings
./target/debug/pairly send <dev> a.pdf b.jpg   # with progress; --no-wait to return at once
./target/debug/pairly url <dev> https://example.com   # opens on the phone (tap the notification)
./target/debug/pairly text <dev> "some text"          # lands on the phone's clipboard
./target/debug/pairly transfers        # what's in flight
```

## Sending files

- **GTK app:** drop files on the window or on a device in the sidebar, or use **Send…** on the
  device page. **Link or Text** sends a link (opened there) or text (copied there).
- **Nautilus:** right-click → **Scripts → Send to Device**. Dolphin and Nemo get a
  "Send to Device (Pairly)" entry too. Thunar: add a custom action running
  `pairly-gtk --send %F`.
- **Tray:** a device's **Send Files…**. **Launcher:** Pairly's **Send Files** action.
- **yazi** (or any terminal file manager): `pairly-gtk --send <files>` picks the device for
  you; for example in `~/.config/yazi/keymap.toml`:
  `{ on = "S", run = 'shell -- pairly-gtk --send "$@"', desc = "Send to phone" }`.

Received files go to your download folder (`XDG_DOWNLOAD_DIR`), written as `name.part` and
renamed once the SHA-256 checks out. The notification has **Open** and **Show in Folder**.
Links from the phone open in your browser; text goes to the clipboard.

Data lives in `$XDG_DATA_HOME/pairly` (`identity.key`, `registry.db`). Settings are in
`$XDG_CONFIG_HOME/pairly/config.toml`; every key is optional:

```toml
name = "My Laptop"
device_type = "laptop"
bus_name = "io.github.abhilesh1412.Pairly.Daemon"
tray = true
[lan]
port = 47100
mdns = true
enabled = true              # false: no LAN at all, only the relay

[relay]
address = "pairly-relay://TOKEN@host:47200/PIN"   # see pairly-core/crates/pairly-relay/README.md
padding = true              # hide exact message sizes from the relay (256-byte steps)

[clipboard]
auto = true                 # send every copy to connected devices (false: only on request)

[share]
download_dir = "~/Downloads/Pairly"   # default: your XDG download folder
auto_accept = true          # false: Accept/Decline buttons on a notification
open_urls = true            # false: a notification with an Open button instead

[notifications]
send = true                 # forward this PC's notifications to your phone
show = true                 # show your phone's notifications here
ignore_apps = ["Spotify"]   # app names never forwarded
reply = "auto"              # "inline", "dialog" or "auto"
dismiss_on_phone = true     # closing a phone's notification here clears it on the phone

[power]
from_phone = true           # paired phones may lock, power off or restart this PC
```

Replies to phone notifications use the notification server's inline reply field when it has
one. swaync advertises inline replies even with `"notification-inline-replies": false` in its
config, so pairlyd reads that setting and uses a small reply window instead. Change it to
`true` (then `swaync-client -R` and `systemctl --user restart pairlyd`) for the inline field.

### Phone features

- **Media:** your phone's player shows up as an MPRIS player (`playerctl`, Waybar, media keys),
  and this PC's players appear on the phone's lock screen.
- **Calls:** an incoming call shows here and pauses your music (`[telephony] pause_media`).
- **Messages:** on the phone's page, **Messages** reads and sends SMS through the phone.
- **Commands:** the terminal icon (or the **Commands** tile) sets which commands the phone may
  run here. The phone can only run commands from that list, after confirming, and each run shows
  a notification.
- **Touchpad, keyboard, presenter:** driven from the phone. They need a compositor with the
  wlroots virtual pointer and keyboard protocols (Hyprland, Sway, river).

### Bluetooth

With no network (or Wi-Fi off), Pairly reaches a paired phone over Bluetooth, as long as the
phone and PC are also *paired in Bluetooth settings* once:

```sh
bluetoothctl     # then: agent on, default-agent, pairable on, discoverable on
```

On the phone, open **Settings → Connected devices → Pair new device**, pick this PC and confirm
the code on both (type `yes` in `bluetoothctl`). Then run `discoverable off`, and in the Pairly
app tap **Allow Bluetooth**. Pairly's own encrypted pairing still runs on top. Bluetooth is
slow (about 200 KB/s), so Pairly moves back to Wi-Fi as soon as it's available.
`[bluetooth] enabled = false` turns it off.

### Firewall

Phones find the PC with mDNS and connect over UDP 47100. With firewalld (default on
EndeavourOS/Fedora), open both in the zone of your network interface:

```sh
sudo firewall-cmd --permanent --zone=public --add-service=mdns
sudo firewall-cmd --permanent --zone=public --add-port=47100/udp
sudo firewall-cmd --reload
```

To run a second daemon on the same machine, give it its own `data_dir`, `bus_name` and
`lan.port`, start it with `pairlyd --config <file>`, and talk to it with
`pairly --bus-name <name> ...`.
