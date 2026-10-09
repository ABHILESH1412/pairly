# Releasing Pairly

Each release ships, on GitHub Releases:
- **a signed Android APK**;
- **a prebuilt Linux bundle** (x86_64): unpack it and run `make install activate`;
- **the Arch package recipe** (`PKGBUILD` and `pairly.install`), which builds from the
  release's source tarball;
- `SHA256SUMS` for the downloads.

Steps 1 to 3 are done once. After that, every release is step 4: one command.

## 1. Put the code on GitHub (once)

1. Create an empty **public** repository named `pairly` at <https://github.com/new>. Don't add a
   README or licence; the code has both.
2. Push the code:

   ```sh
   git remote add origin git@github.com:ABHILESH1412/pairly.git
   git push -u origin main
   ```

## 2. Create the Android release key (once, and keep it forever)

Android only installs an update if it's signed with **the same key** as the installed app.
**If you lose this key or its password, you can never update the app.** People would have to
uninstall it and pair again.

1. Create the key. It asks for a password, so pick a strong one and save it in your password
   manager:

   ```sh
   /opt/android-studio/jbr/bin/keytool -genkeypair -v -keystore ~/pairly-release.jks \
     -alias pairly -keyalg RSA -keysize 4096 -validity 10000 -dname "CN=Abhilesh Singh"
   ```

2. Tell the build where it is. Create `pairly-android/keystore.properties`; it's in
   `.gitignore`, so it is never committed. Leave the password out: `scripts/release.sh` asks
   for it each time, and it is never saved:

   ```properties
   storeFile=/home/three/pairly-release.jks
   keyAlias=pairly
   ```

   (You *can* add `storePassword=…` and `keyPassword=…` lines to skip the question. Then run
   `chmod 600 pairly-android/keystore.properties`.)

3. **Back up** `~/pairly-release.jks` somewhere off this laptop, such as an encrypted USB stick
   or your password manager's file storage.

## 3. Install and log in to the GitHub CLI (once)

The release script publishes through GitHub's official command-line tool:

```sh
sudo pacman -S github-cli
gh auth login
```

In `gh auth login`, choose **GitHub.com**, then **SSH** (it finds the key you already added),
then **Login with a web browser**, and enter the code it shows.

## 3b. Create the update signing key (once, and keep it forever)

All three apps update themselves from GitHub releases. The Linux and Windows apps install an
update only if it carries a signature from **your** update key, so a tampered download (or
someone who got into your GitHub account) can't install anything. (Android checks the APK's
own signature, made with the release key from step 2.)

```sh
sudo pacman -S minisign
minisign -G -p ~/pairly-update.pub -s ~/pairly-update.key
```

It asks for a password: pick a strong one and keep it with the release key's. Then put the
**public** half in the repository (the apps carry it) and commit it:

```sh
cp ~/pairly-update.pub ~/work/pairly/keys/update.pub
```

Never commit `~/pairly-update.key`, and back it up with the release key. If you lose it, the
installed apps can't take updates any more until they're reinstalled by hand.

### Let GitHub sign the Windows installer

The Windows installer is built on GitHub's machines when a release is published. Give them the
key once: in the repository on GitHub, open **Settings → Secrets and variables → Actions →
New repository secret**, and add:

- `TAURI_SIGNING_PRIVATE_KEY`: the output of `base64 -w0 ~/pairly-update.key`
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: the key's password

## 4. Release (every time)

Commit your work as usual (`git add`, `git commit`), then run **one command** from the
`pairly` folder:

```sh
scripts/release.sh            # next patch version, e.g. 0.1.0 -> 0.1.1
scripts/release.sh minor      # new features: 0.1.4 -> 0.2.0
scripts/release.sh major      # big changes: 0.9.2 -> 1.0.0
```

It asks for the release key's password, then works through these steps by itself (about
10 to 20 minutes):

1. **Checks** that your work is committed, you're on `main` and up to date with GitHub, the
   release key is set up and `gh` is logged in.
2. **Bumps the version** everywhere:
   - both Rust workspaces and their lock files;
   - the Android `versionName`, and `versionCode` + 1;
   - the PKGBUILD;
   - the software-centre metadata, with release notes made from your commit messages.
3. **Runs the tests**, then **builds** the signed APK and the Linux bundle (memory-capped).
4. **Commits** "Release vX.Y.Z", **tags** it, and **pushes** both.
5. **Publishes the GitHub release** "Pairly X.Y.Z": what's new (your commit messages since
   the last release), install steps and checksums, with every file attached.

If anything fails before the push, the version bump is undone and nothing leaves your PC.
Fix the problem and run it again.

Useful options:
- `scripts/release.sh --dry-run`: shows what the version bump would change, then puts
  everything back. Nothing is built or pushed.
- `scripts/release.sh --no-test`: skips the test run (faster; use it when you've just run
  the tests yourself).
- `scripts/release.sh 0.3.0`: releases an exact version.

**Tip:** the release notes are your commit messages, so write them for people reading the
release page, e.g. "Choose which PC apps send notifications to the phone".

## Optional extras

**AUR** (not open to new accounts at the moment). If you get an account later:
1. Add your SSH key there.
2. `git clone ssh://aur@aur.archlinux.org/pairly.git`.
3. Copy in `PKGBUILD`, `.SRCINFO` and `pairly.install` from `dist/vX.Y.Z/arch/`.
4. Commit and push.

**Relay image (optional)**

Log in with a GitHub token that has the `write:packages` scope
(GitHub → Settings → Developer settings → Personal access tokens). Then:

```sh
docker login ghcr.io -u ABHILESH1412
cd pairly-core
docker build -f crates/pairly-relay/deploy/Dockerfile -t ghcr.io/abhilesh1412/pairly-relay:X.Y.Z .
docker push ghcr.io/abhilesh1412/pairly-relay:X.Y.Z
```

## Checks you can run yourself

Run each command from its own folder:

| Folder | Command |
|---|---|
| `pairly-core` and `pairly-linux` | `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` and `cargo deny check` |
| `pairly-android` | `./gradlew lintDebug` |

Then install the release APK on a phone, check that pairing, notifications and file sending
work, and `makepkg` the PKGBUILD once.
