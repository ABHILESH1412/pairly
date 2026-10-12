# Pairly for Windows

The Windows app: a tray app with the same pastel look as the Linux app, built with
[Tauri](https://tauri.app) (a Rust backend running the shared Pairly core, and a small
HTML/CSS/JS window in `ui/`, no build step).

Releases are built on GitHub's Windows machines (`.github/workflows/windows.yml`) and attached to
the GitHub release as `pairly-X.Y.Z-windows-x64-setup.exe`. The installer needs no admin rights.

## What it does

The same as the Linux app:

- Pair with a QR code or a code, pause, unpair, rename, switch Pairly on and off
- Phone notifications as Windows notifications, with their buttons and a reply box; closing one
  here clears it on the phone
- This PC's notifications on the phone (Settings lists the apps, to leave some out)
- Texts: read conversations (pictures included) and send texts and picture messages
- Contacts and calls: call or text a contact; answer (also on speaker), reject or hang up a call
  from its notification or the window; this PC's music pauses while the phone rings
- Browse the phone's files: download, upload (or drop files on the window), rename, delete
- The phone's screen in a window, to watch and control it (same Wi-Fi)
- Another PC's screen (Linux or Windows) in a window, controlled with your mouse and keyboard,
  shortcuts included (same Wi-Fi)
- This PC's screen on the phone or another PC, to watch and control it (same Wi-Fi; a
  notification shows while it lasts, with a button to stop it)
- Media both ways: the phone's music in the window, and this PC's players (Spotify, browsers,
  Media Player… anything using Windows' media controls) on the phone
- Clipboard both ways (automatic, or on request); links and text
- Files both ways (pick or drop them on the window; received ones go to `Downloads\Pairly`)
- Find my phone, and the phone can ring the PC; ping
- The phone as a remote mouse, keyboard and presenter, with the laser pointer drawn over
  everything on screen (full-screen slides too)
- Commands the phone may run on this PC (`cmd /C`, each run shown in a notification)
- The phone can lock, restart or power off the PC (switchable); lock or power off the phone
- Over the local network, Bluetooth (bonded devices, no network needed) or your own relay
- Light and dark; starts with Windows in the tray (switchable); updates itself (switchable)

Reading this PC's notifications needs Windows' permission: if Windows refuses it, Settings says so.

## Develop

Needs Rust (`rustup`) and the Visual Studio Build Tools (C++); Windows 10 and 11 have the
WebView2 runtime already.

```sh
cd src-tauri
cargo run
```

To run it beside an installed Pairly, give it a data folder of its own (it then skips the
single-instance lock and starting with Windows) and another port:

```powershell
$env:PAIRLY_DATA_DIR = "$env:TEMP\pairly-dev"; $env:PAIRLY_LAN_PORT = "47110"; cargo run
```

`cargo run --example fake_phone -- <folder>` is a pretend Android phone for trying the app
without one: it pairs (type `pair <id>`; `devices` lists ids), has texts, contacts, files and a
music player, shares a moving test picture as its screen, and on command posts notifications,
rings, takes calls, points the laser, runs this PC's commands… (`help` lists them).
`cargo run --example capture_probe` shows which ways of recording the screen work on this PC.

On Linux (with WebKitGTK) the app builds and runs too, for working on the window; the
Windows-only parts (power, battery, notifications' buttons, screen recording, Bluetooth, media
sessions) do nothing there.

## Updates

The app reads `latest.json` from the latest GitHub release and installs the new installer only
if it's signed with the project's update key (the public half is in `../keys/update.pub`, put
into `tauri.conf.json` by `scripts/set_update_key.py` at build time). See `../docs/releasing.md`.
