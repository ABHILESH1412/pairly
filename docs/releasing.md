# Releasing Pairly

Each release ships:
- **a signed Android APK**, on GitHub Releases;
- **an Arch package recipe** (PKGBUILD), attached to the release, which builds from the
  release's source tarball;
- optionally, **a relay Docker image** on GitHub's container registry.

Steps 1 and 2 are done once. Steps 3 to 6 are done for every release. In this guide,
`X.Y.Z` stands for the version number, such as `0.1.0`: always type the real number.

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

## 3. Set the version

The version must match everywhere; `scripts/release.sh` checks it.

| Where | What |
|---|---|
| `pairly-core/Cargo.toml` and `pairly-linux/Cargo.toml` | `version = "X.Y.Z"` (`[workspace.package]`) |
| `pairly-android/app/build.gradle.kts` | `versionName = "X.Y.Z"`, and **increase `versionCode` by 1** |
| `pairly-linux/packaging/arch/PKGBUILD` | `pkgver=X.Y.Z`, `pkgrel=1` |
| `pairly-linux/data/io.github.abhilesh1412.Pairly.metainfo.xml` | a new `<release version="X.Y.Z" date="…">` at the top |

For 0.1.0 all of these are already set.

## 4. Commit, tag and push

Use the real version number. For the first release:

```sh
git commit -am "Release v0.1.0"
git tag v0.1.0
git push && git push origin v0.1.0
```

(For a later release, replace `0.1.0` with its number, e.g. `0.1.1`.)

## 5. Build the release files

```sh
scripts/release.sh
```

The script:
1. checks the versions, a clean tree, the tag and the release key;
2. builds the APK and refuses one signed with the debug key;
3. downloads GitHub's tarball for the tag to put its checksum in the PKGBUILD;
4. writes everything to `dist/vX.Y.Z/`.

## 6. Publish

**GitHub Release**
1. On the repository, open **Releases → Draft a new release** and choose the tag `vX.Y.Z`.
2. Upload `dist/vX.Y.Z/pairly-X.Y.Z.apk` and its `.sha256`.
3. Write what changed, then publish.

**Arch package (attached to the release; the AUR isn't open to new accounts yet)**
- On the release, upload `dist/vX.Y.Z/aur/PKGBUILD` and `dist/vX.Y.Z/aur/pairly.install` too.
  Arch users download both and run `makepkg -si` (the README explains it).
- If AUR registration opens later:
  1. Create an account and add your SSH key there.
  2. `git clone ssh://aur@aur.archlinux.org/pairly.git`.
  3. Copy in `PKGBUILD`, `.SRCINFO` and `pairly.install`.
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

## Checks before tagging

Run each command from its own folder:

| Folder | Command |
|---|---|
| `pairly-core` and `pairly-linux` | `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` and `cargo deny check` |
| `pairly-android` | `./gradlew lintDebug` |

Then install the release APK on a phone, check that pairing, notifications and file sending
work, and `makepkg` the PKGBUILD once.
