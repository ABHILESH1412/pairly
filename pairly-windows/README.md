# Pairly for Windows

The Windows app: a tray app with the same pastel look as the Linux app, built with
[Tauri](https://tauri.app) (a Rust backend running the shared Pairly core, and a small
HTML/CSS/JS window in `ui/`, no build step).

Releases are built on GitHub's Windows machines (`.github/workflows/windows.yml`) and attached to
the GitHub release as `pairly-X.Y.Z-windows-x64-setup.exe`. The installer needs no admin rights.

## What it does (first version)

- Pair with a QR code or a code, pause, unpair, rename, switch Pairly on and off
- Phone notifications as Windows notifications; ping; find my phone (and the phone can ring the PC)
- Clipboard both ways (automatic, or on request); links and text
- Files both ways (pick or drop them on the window; received ones go to `Downloads\Pairly`)
- The phone as a remote mouse and keyboard; the phone can lock, restart or power off the PC
- Lock or power off the phone from the PC
- Light and dark; starts with Windows in the tray (switchable); updates itself (switchable)

Still to come: messages, contacts and calls; media control; sending this PC's notifications to
the phone (Windows only lets packaged apps read them); screen sharing; laser pointer; Bluetooth.

## Develop

On Linux (with WebKitGTK) the app builds and runs too, for working on the window; the
Windows-only parts (power, battery, beeps) do nothing there.

```sh
cd src-tauri
cargo run
```

## Updates

The app reads `latest.json` from the latest GitHub release and installs the new installer only
if it's signed with the project's update key (the public half is in `../keys/update.pub`, put
into `tauri.conf.json` by `scripts/set_update_key.py` at build time). See `../docs/releasing.md`.
