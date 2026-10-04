#!/usr/bin/env bash
# Build the release files for the version in pairly-core/Cargo.toml (see docs/releasing.md).
#
#   scripts/release.sh          after `git tag vX.Y.Z && git push --tags`
#
# Produces dist/vX.Y.Z/:
#   pairly-X.Y.Z.apk (signed with your release key) and its SHA-256
#   aur/PKGBUILD and aur/.SRCINFO, with the checksum of GitHub's tarball for the tag
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' pairly-core/Cargo.toml | head -1)"
tag="v$version"
out="dist/$tag"
repo_url="$(sed -n "s/^url='\(.*\)'/\1/p" pairly-linux/packaging/arch/PKGBUILD)"

die() { echo "error: $*" >&2; exit 1; }

export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
# The Android Gradle plugin needs JDK 17-25; Android Studio's own runtime fits.
if [ -z "${JAVA_HOME:-}" ] && [ -d /opt/android-studio/jbr ]; then
  export JAVA_HOME=/opt/android-studio/jbr
fi

# Every place that carries the version must agree.
grep -q "^version = \"$version\"" pairly-linux/Cargo.toml || die "pairly-linux/Cargo.toml isn't $version"
grep -q "versionName = \"$version\"" pairly-android/app/build.gradle.kts || die "Android versionName isn't $version"
grep -q "^pkgver=$version$" pairly-linux/packaging/arch/PKGBUILD || die "PKGBUILD pkgver isn't $version"
grep -q "release version=\"$version\"" pairly-linux/data/*.metainfo.xml || die "metainfo has no $version release"

# Releases come from a committed, tagged tree.
git diff --quiet && git diff --cached --quiet || die "commit your changes first"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null || die "tag $tag doesn't exist (git tag $tag)"
[ "$(git rev-parse "$tag^{commit}")" = "$(git rev-parse HEAD)" ] || die "HEAD isn't $tag"

# Android: only with the real release key.
[ -f pairly-android/keystore.properties ] ||
  die "pairly-android/keystore.properties is missing (see docs/releasing.md, step 2)"

# Ask for the key's password unless the file or the environment has it (it isn't echoed or
# stored; Gradle reads it from the environment of this run only).
if ! grep -q '^storePassword=' pairly-android/keystore.properties && [ -z "${PAIRLY_STORE_PASSWORD:-}" ]; then
  read -rsp "Release key password: " PAIRLY_STORE_PASSWORD
  echo
  export PAIRLY_STORE_PASSWORD
fi

mkdir -p "$out/aur"
(cd pairly-android && ./gradlew --quiet clean assembleRelease)
apk="$out/pairly-$version.apk"
cp pairly-android/app/build/outputs/apk/release/app-release.apk "$apk"
apksigner="$(ls -d "$ANDROID_HOME"/build-tools/*/apksigner | tail -1)"
"$apksigner" verify --print-certs "$apk" | grep -q "CN=Android Debug" &&
  die "the APK is signed with the debug key"
(cd "$out" && sha256sum "pairly-$version.apk" > "pairly-$version.apk.sha256")

# AUR: the PKGBUILD with the checksum of GitHub's tarball (it must already be pushed).
cp pairly-linux/packaging/arch/PKGBUILD pairly-linux/packaging/arch/pairly.install "$out/aur/"
curl -fsSL "$repo_url/archive/refs/tags/$tag.tar.gz" -o "$out/aur/pairly-$version.tar.gz" ||
  die "couldn't download $repo_url/archive/refs/tags/$tag.tar.gz: push the tag first"
sum="$(sha256sum "$out/aur/pairly-$version.tar.gz" | cut -d' ' -f1)"
sed -i "s/^sha256sums=.*/sha256sums=('$sum')/" "$out/aur/PKGBUILD"
rm "$out/aur/pairly-$version.tar.gz"
(cd "$out/aur" && makepkg --printsrcinfo > .SRCINFO)

echo "Done: $out"
ls -la "$out" "$out/aur"
