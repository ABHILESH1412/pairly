#!/usr/bin/env bash
# One-command release: bump the version, test, build, tag, push and publish on GitHub.
#
#   scripts/release.sh            next patch version (0.1.0 -> 0.1.1)
#   scripts/release.sh minor      0.1.3 -> 0.2.0
#   scripts/release.sh major      0.4.2 -> 1.0.0
#   scripts/release.sh 0.5.0      an exact version
#   scripts/release.sh --no-test     skip the test run (any of the above)
#   scripts/release.sh --no-flatpak  skip the Flatpak (any of the above)
#   scripts/release.sh --dry-run     show what the version bump would change, then undo it
#
# Commit your own work first; this script makes one commit of its own ("Release vX.Y.Z").
# Needs: the release key (pairly-android/keystore.properties), the update signing key
# (~/pairly-update.key, made with minisign; its public half in keys/update.pub), the GitHub
# CLI, logged in (`gh auth login`), and for the Flatpak: flatpak with org.flatpak.Builder and
# the GNOME 50 SDK. See docs/releasing.md.
#
# If anything fails before the push, the version bump is undone and nothing leaves this PC.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

bump="patch"
run_tests=1
dry_run=0
build_flatpak=1
for arg in "$@"; do
  case "$arg" in
    --no-test) run_tests=0 ;;
    --no-flatpak) build_flatpak=0 ;;
    --dry-run) dry_run=1 ;;
    patch | minor | major) bump="$arg" ;;
    [0-9]*.[0-9]*.[0-9]*) bump="$arg" ;;
    -h | --help) sed -n '2,17p' "$0"; exit 0 ;;
    *) die "unknown argument: $arg (see --help)" ;;
  esac
done

export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
# The Android Gradle plugin needs JDK 17-25; Android Studio's own runtime fits.
if [ -z "${JAVA_HOME:-}" ] && [ -d /opt/android-studio/jbr ]; then
  export JAVA_HOME=/opt/android-studio/jbr
fi
# Heavy builds run in a memory-capped scope so a big compile can't freeze the laptop.
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}"
capped() { systemd-run --user --scope --quiet -p MemoryMax=5G -p MemorySwapMax=1G "$@"; }

metainfo="pairly-linux/data/io.github.abhilesh1412.Pairly.metainfo.xml"
update_key="${PAIRLY_UPDATE_KEY:-$HOME/pairly-update.key}"
pkgbuild="pairly-linux/packaging/arch/PKGBUILD"
gradle="pairly-android/app/build.gradle.kts"
repo_url="$(sed -n "s/^url='\(.*\)'/\1/p" "$pkgbuild")"
app_id="io.github.abhilesh1412.Pairly"
fp_manifest="pairly-linux/flatpak/$app_id.yml"
# Flatpak build state (big, rebuilt quickly on the next release) stays out of the repository.
fp_work="${PAIRLY_FLATPAK_WORK:-$HOME/.cache/pairly-flatpak}"

# The list of Rust crates the Flatpak builds from (it builds offline), from Cargo.lock.
cargo_sources() {
  local gen="$fp_work/flatpak-cargo-generator.py"
  mkdir -p "$fp_work"
  if [ ! -x "$fp_work/venv/bin/python" ]; then
    python3 -m venv "$fp_work/venv" && "$fp_work/venv/bin/pip" install --quiet aiohttp tomlkit
  fi
  [ -f "$gen" ] || curl -fsSL -o "$gen" \
    https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py
  "$fp_work/venv/bin/python" "$gen" pairly-linux/Cargo.lock -o pairly-linux/flatpak/cargo-sources.json >/dev/null
}

# ----- 1. Checks: nothing is changed until all of these pass ----------------------------
step "Checking"
git diff --quiet && git diff --cached --quiet || die "commit (or stash) your changes first"
if [ "$dry_run" = 0 ]; then
  command -v gh >/dev/null || die "the GitHub CLI isn't installed: sudo pacman -S github-cli"
  gh auth status >/dev/null 2>&1 || die "the GitHub CLI isn't logged in: gh auth login"
  [ "$(git branch --show-current)" = main ] || die "switch to the main branch first"
  [ -f pairly-android/keystore.properties ] ||
    die "pairly-android/keystore.properties is missing (see docs/releasing.md, step 2)"
  # Updates install only with a valid signature from this key: no key, no release.
  command -v minisign >/dev/null || die "minisign isn't installed: sudo pacman -S minisign"
  [ -f "$update_key" ] || die "the update signing key $update_key is missing (see docs/releasing.md)"
  grep -q PLACEHOLDER keys/update.pub &&
    die "keys/update.pub is still the placeholder: copy your key's public half there (docs/releasing.md)"
  git fetch --quiet --tags origin main
  [ "$(git rev-list --count HEAD..origin/main)" = 0 ] ||
    die "GitHub has commits you don't: git pull first"
fi
if [ "$build_flatpak" = 1 ]; then
  command -v flatpak >/dev/null || die "flatpak isn't installed (or use --no-flatpak)"
  flatpak info org.flatpak.Builder >/dev/null 2>&1 ||
    die "the Flatpak builder isn't installed: flatpak install --user flathub org.flatpak.Builder (or use --no-flatpak)"
  for ref in org.gnome.Sdk//50 org.freedesktop.Sdk.Extension.rust-stable//25.08; do
    flatpak info "$ref" >/dev/null 2>&1 ||
      die "$ref isn't installed: flatpak install --user flathub $ref (or use --no-flatpak)"
  done
fi

current="$(sed -n 's/^version = "\(.*\)"/\1/p' pairly-core/Cargo.toml | head -1)"
IFS=. read -r major minor patch <<<"$current"
case "$bump" in
  patch) version="$major.$minor.$((patch + 1))" ;;
  minor) version="$major.$((minor + 1)).0" ;;
  major) version="$((major + 1)).0.0" ;;
  *) version="$bump" ;;
esac
tag="v$version"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "$tag already exists"
previous="$(git describe --tags --abbrev=0 2>/dev/null || true)"
echo "Releasing $current -> $version (previous release: ${previous:-none})"

# What changed since the last release, from your commit messages.
changes="$(git log --no-merges --format='- %s' ${previous:+"$previous"..HEAD} |
  grep -v '^- Release v' || true)"
[ -n "$changes" ] || changes="- Maintenance release."

# The release key's password, asked once (not echoed, not stored).
if [ "$dry_run" = 0 ] && ! grep -q '^storePassword=' pairly-android/keystore.properties &&
  [ -z "${PAIRLY_STORE_PASSWORD:-}" ]; then
  read -rsp "Release key password: " PAIRLY_STORE_PASSWORD
  echo
  export PAIRLY_STORE_PASSWORD
fi

# From here until the push, any failure (or Ctrl+C) puts every file back as it was.
pushed=0
start="$(git rev-parse HEAD)"
# Build helpers (Gradle and Kotlin daemons, also ones left by earlier builds) hold gigabytes:
# stop them whenever the script ends, so the memory is free straight away.
free_memory() {
  (cd "$root/pairly-android" && ./gradlew --stop --quiet >/dev/null 2>&1) || true
  # Only Java processes whose command line names the Kotlin daemon class.
  local pid
  for pid in $(pgrep -u "$USER" -x java); do
    if grep -qa 'org.jetbrains.kotlin.daemon.KotlinCompileDaemon' "/proc/$pid/cmdline" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
    fi
  done
}

undo() {
  local status=$?
  free_memory
  if [ "$pushed" = 0 ]; then
    if [ "$dry_run" = 1 ]; then
      echo "Dry run: every file is back as it was." >&2
    else
      echo "Release stopped: undoing the version bump (nothing was pushed)." >&2
    fi
    git tag -d "$tag" >/dev/null 2>&1 || true
    git reset --quiet --hard "$start"
  fi
  exit "$status"
}
trap undo EXIT
trap 'exit 130' INT TERM

# ----- 2. Bump the version everywhere ---------------------------------------------------
step "Setting the version to $version"
for ws in pairly-core pairly-linux; do
  sed -i "0,/^version = \".*\"/s//version = \"$version\"/" "$ws/Cargo.toml"
done
# Refresh the lock files for the new version (ours and the core crates pairly-linux uses),
# without touching any other dependency.
for ws in pairly-core pairly-linux; do
  (cd "$ws" && cargo metadata --offline --format-version 1 >/dev/null)
done
# The Windows app (built and published by GitHub Actions when the release goes out).
sed -i "0,/^version = \".*\"/s//version = \"$version\"/" pairly-windows/src-tauri/Cargo.toml
python3 - "$version" <<'PY'
import json, sys
path = "pairly-windows/src-tauri/tauri.conf.json"
conf = json.load(open(path))
conf["version"] = sys.argv[1]
with open(path, "w") as f:
    json.dump(conf, f, indent=2)
    f.write("\n")
PY
# Online: this PC never builds the Windows app, so its Windows-only crates may not be
# downloaded yet. The lock file still keeps every other dependency as it is.
(cd pairly-windows/src-tauri && cargo metadata --format-version 1 >/dev/null)
# The Flatpak builds offline: keep its list of crates in step with Cargo.lock.
[ "$build_flatpak" = 1 ] && cargo_sources
code="$(sed -n 's/^ *versionCode = \([0-9]*\)/\1/p' "$gradle")"
sed -i "s/^\( *versionCode = \)[0-9]*/\1$((code + 1))/; s/^\( *versionName = \)\".*\"/\1\"$version\"/" "$gradle"
sed -i "s/^pkgver=.*/pkgver=$version/; s/^pkgrel=.*/pkgrel=1/; s/^sha256sums=.*/sha256sums=('SKIP')/" "$pkgbuild"
today="$(date +%F)"
notes_xml="$(printf '%s\n' "$changes" | sed 's/^- //; s/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g' |
  sed 's|.*|          <li>&</li>|')"
python3 - "$metainfo" "$version" "$today" "$notes_xml" <<'EOF'
import sys
path, version, date, items = sys.argv[1:]
s = open(path).read()
entry = (f'    <release version="{version}" date="{date}">\n      <description>\n'
         f'        <ul>\n{items}\n        </ul>\n      </description>\n    </release>\n')
s = s.replace("  <releases>\n", "  <releases>\n" + entry, 1)
open(path, "w").write(s)
EOF
appstreamcli validate --no-net --pedantic "$metainfo" >/dev/null 2>&1 ||
  appstreamcli validate --no-net "$metainfo" >/dev/null

if [ "$dry_run" = 1 ]; then
  step "Dry run: this is what the release would change (now undone)"
  git --no-pager diff --stat
  git --no-pager diff -- "$gradle" "$pkgbuild" "$metainfo" pairly-core/Cargo.toml
  echo
  echo "Release notes would list:"
  echo "$changes"
  exit 0
fi

# ----- 3. Test and build ----------------------------------------------------------------
if [ "$run_tests" = 1 ]; then
  step "Running the tests"
  for ws in pairly-core pairly-linux; do
    (cd "$ws" && capped cargo test --workspace --locked --quiet)
  done
fi

out="dist/$tag"
rm -rf "$out" && mkdir -p "$out"

step "Building the Android app"
# No daemons: the Gradle and Kotlin compiler processes exit with the build instead of keeping
# gigabytes of RAM for hours.
(cd pairly-android && capped ./gradlew --no-daemon -Pkotlin.compiler.execution.strategy=in-process \
  --quiet clean assembleRelease)
apk="pairly-$version.apk"
cp pairly-android/app/build/outputs/apk/release/app-release.apk "$out/$apk"
apksigner="$(ls -d "$ANDROID_HOME"/build-tools/*/apksigner | tail -1)"
if "$apksigner" verify --print-certs "$out/$apk" 2>/dev/null | grep -q "CN=Android Debug"; then
  die "the APK is signed with the debug key"
fi

step "Building the Linux apps"
(cd pairly-linux && capped cargo build --release --locked --quiet)
bundle="pairly-$version-linux-x86_64"
stage="$(mktemp -d "$root/dist/.stage.XXXXXX")"
mkdir -p "$stage/$bundle/target/release"
cp pairly-linux/target/release/{pairlyd,pairly,pairly-gtk} "$stage/$bundle/target/release/"
cp -r pairly-linux/data pairly-linux/Makefile pairly-linux/README.md LICENSE "$stage/$bundle/"
# The newest C library version the programs actually use (not the build machine's).
glibc="$(objdump -T pairly-linux/target/release/{pairlyd,pairly-gtk} | grep -o 'GLIBC_[0-9.]*' |
  sort -uV | tail -1 | cut -d_ -f2)"
cat >"$stage/$bundle/INSTALL.txt" <<EOF
Pairly $version for Linux (x86_64), prebuilt on Arch Linux. Needs glibc $glibc or newer and the
GTK 4 and libadwaita of GNOME 50 or newer (Arch and other rolling distributions, Fedora 44,
Ubuntu 26.04 and later). On any other distribution, install the Flatpak instead.

Install for your user (no root), then it starts automatically:
  make install activate

Remove again:
  make uninstall-user
EOF
tar -C "$stage" -czf "$out/$bundle.tar.gz" "$bundle"
rm -rf "$stage"

flatpak_file="pairly-$version-x86_64.flatpak"
signed=("$out/$bundle.tar.gz")
if [ "$build_flatpak" = 1 ]; then
  step "Building the Flatpak (every Linux distribution)"
  # The builder runs in its own sandbox: its jobs, not a memory cap, keep RAM in check.
  flatpak run org.flatpak.Builder --user --force-clean --jobs="$CARGO_BUILD_JOBS" \
    --state-dir="$fp_work/state" --repo="$fp_work/repo" "$fp_work/build" "$fp_manifest" \
    >"$fp_work/build.log" 2>&1 || die "the Flatpak build failed: see $fp_work/build.log"
  # Installing the file fetches the GNOME runtime from Flathub when it's missing.
  flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
    "$fp_work/repo" "$out/$flatpak_file" "$app_id"
  signed+=("$out/$flatpak_file")
fi

step "Signing the Linux downloads for auto-updates"
# Asks for the update key's password. Installed apps accept an update only with this
# signature, so check it against the public key they carry before publishing.
minisign -S -s "$update_key" -m "${signed[@]}" -t "Pairly $version for Linux"
for f in "${signed[@]}"; do
  minisign -Vq -p keys/update.pub -m "$f" ||
    die "the signature doesn't match keys/update.pub: is that the public half of $update_key?"
done
(cd "$out" && sha256sum "$apk" "${signed[@]##*/}" >SHA256SUMS)

# ----- 4. Commit, tag and push ----------------------------------------------------------
step "Committing and pushing $tag"
git commit --quiet -am "Release $tag"
git tag -a "$tag" -m "Pairly $version"
git push --quiet origin main
git push --quiet origin "$tag"
pushed=1

# ----- 5. Arch package files (they need the tag's tarball on GitHub) -------------------
step "Preparing the Arch package files"
mkdir -p "$out/arch"
cp "$pkgbuild" pairly-linux/packaging/arch/pairly.install "$out/arch/"
curl -fsSL --retry 5 --retry-delay 3 "$repo_url/archive/refs/tags/$tag.tar.gz" -o "$out/arch/src.tar.gz"
sum="$(sha256sum "$out/arch/src.tar.gz" | cut -d' ' -f1)"
rm "$out/arch/src.tar.gz"
sed -i "s/^sha256sums=.*/sha256sums=('$sum')/" "$out/arch/PKGBUILD"
(cd "$out/arch" && makepkg --printsrcinfo >.SRCINFO)

# ----- 6. Publish the GitHub release ---------------------------------------------------
step "Publishing the release on GitHub"
cat >"$out/notes.md" <<EOF
## What's new

$changes

## Install

**Android 8 or newer:** download \`$apk\` below and open it on your phone.

**Windows 10 or 11:** download \`pairly-$version-windows-x64-setup.exe\` below (it appears a few minutes after the release, once GitHub has built it) and run it.

**Arch, EndeavourOS, Manjaro:** download \`PKGBUILD\` and \`pairly.install\` into an empty folder, then run \`makepkg -si\` there, then \`systemctl --user enable --now pairlyd\`.

**Ubuntu, Fedora, Debian, Mint, openSUSE and every other Linux:** download \`$flatpak_file\` below, then run \`flatpak install --user $flatpak_file\` (or open it with your software centre). It updates itself.

**Other Linux, without Flatpak (x86_64):** download \`$bundle.tar.gz\`, unpack it, and run \`make install activate\` inside (installs under ~/.local; needs GNOME 50's GTK and libadwaita).

Then open Pairly on the PC, choose **Pair a New Device**, and scan the code with the phone.

## Checksums (SHA-256)

\`\`\`
$(cat "$out/SHA256SUMS")
\`\`\`
EOF
assets=("$out/$apk" "$out/SHA256SUMS" "$out/arch/PKGBUILD" "$out/arch/pairly.install")
for f in "${signed[@]}"; do
  assets+=("$f" "$f.minisig")
done
gh release create "$tag" --title "Pairly $version" --notes-file "$out/notes.md" --verify-tag \
  "${assets[@]}" ||
  die "the release wasn't published. Everything is pushed; retry with:
  gh release create $tag --title \"Pairly $version\" --notes-file $out/notes.md --verify-tag ${assets[*]}"

step "Released Pairly $version"
echo "$repo_url/releases/tag/$tag"
