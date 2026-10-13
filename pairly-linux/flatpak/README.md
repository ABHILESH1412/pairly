# Pairly as a Flatpak

One build of the Linux app for every distribution (Ubuntu, Fedora, Debian, Mint, openSUSE,
Arch…), on the GNOME 50 runtime. Every release attaches it as `pairly-X.Y.Z-x86_64.flatpak`,
built by `scripts/release.sh`, which also regenerates `cargo-sources.json` (the Rust crates the
offline build uses) from `Cargo.lock`.

## Install a release

```sh
flatpak install --user pairly-X.Y.Z-x86_64.flatpak
```

The GNOME runtime comes from Flathub if it isn't installed yet. Then open **Pairly** from the
app menu. It starts by itself at login (the desktop asks once) and installs new releases by
itself, unless that's switched off in Settings → Updates.

## Build it yourself

```sh
flatpak install --user flathub org.flatpak.Builder org.gnome.Sdk//50 \
  org.freedesktop.Sdk.Extension.rust-stable//25.08
# from the repository's top folder:
flatpak run org.flatpak.Builder --user --install --force-clean --jobs=2 \
  --state-dir ~/.cache/pairly-flatpak/state ~/.cache/pairly-flatpak/build \
  pairly-linux/flatpak/io.github.abhilesh1412.Pairly.yml
flatpak run io.github.abhilesh1412.Pairly
```

After changing dependencies, regenerate the crate list with
[flatpak-cargo-generator](https://github.com/flatpak/flatpak-builder-tools/tree/master/cargo)
(`scripts/release.sh` does this on every release):

```sh
python3 flatpak-cargo-generator.py pairly-linux/Cargo.lock -o pairly-linux/flatpak/cargo-sources.json
```

## What's different from a normal install

- Settings and pairings live in `~/.var/app/io.github.abhilesh1412.Pairly/`, apart from a normal
  install's.
- No systemd service: D-Bus starts the daemon, and the desktop's Background portal starts it at
  login.
- Locking, restarting and powering off the PC, and the phone's commands, run on the PC itself
  through `flatpak-spawn --host`.
- Screen sharing to the phone encodes video in software (`x264`); the runtime has no hardware
  encoder.
- Hyprland and Sway hide their privileged Wayland interfaces from sandboxed apps (clipboard
  watching, layer-shell, screencopy, virtual input). So there: the clipboard isn't sent
  automatically (the Send Clipboard button still works), the laser pointer can't be drawn,
  screen sharing goes through the portal (which asks which screen), and remote input uses
  `/dev/uinput`. GNOME and KDE behave as in a normal install.

The permissions in the manifest keep every feature working: the app's job is to let your phone
reach this PC (its notifications, files, input, power and commands).
