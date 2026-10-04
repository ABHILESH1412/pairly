#!/usr/bin/env bash
# Build libpairly_ffi.so for Android with cargo-ndk and generate its Kotlin bindings.
#
#   build-rust.sh <jnilibs-dir> <kotlin-dir> [abis]
#
# Produces <jnilibs-dir>/<abi>/libpairly_ffi.so and <kotlin-dir>/dev/pairly/core/ffi/*.kt. abis is comma-separated (default: arm64-v8a,x86_64). Gradle runs this from the
# :core-bindings buildRust task; ANDROID_HOME/ANDROID_NDK_HOME come from the Android plugin.
set -euo pipefail

jnilibs="$(realpath -m "$1")"
kotlin="$(realpath -m "$2")"
abis="${3:-arm64-v8a,x86_64}"
profile="${PAIRLY_RUST_PROFILE:-release}"
core="$(cd "$(dirname "$0")/../../pairly-core" && pwd)"
min_sdk=26
# Bounded parallelism: these builds use LTO and can exhaust RAM on 8 GB machines.
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

triple_for() {
    case "$1" in
        arm64-v8a) echo aarch64-linux-android ;;
        armeabi-v7a) echo armv7-linux-androideabi ;;
        x86_64) echo x86_64-linux-android ;;
        x86) echo i686-linux-android ;;
        *) echo "unknown ABI: $1" >&2; exit 1 ;;
    esac
}

IFS=, read -ra abi_list <<< "$abis"
targets=()
for abi in "${abi_list[@]}"; do targets+=(-t "$abi"); done

cd "$core"
rm -rf "$jnilibs" "$kotlin"
cargo ndk "${targets[@]}" -P "$min_sdk" -o "$jnilibs" build -p pairly-ffi --lib --profile "$profile"

# The UniFFI metadata is identical for every ABI; read it from the first library.
lib="target/$(triple_for "${abi_list[0]}")/$profile/libpairly_ffi.so"
cargo run -q -p uniffi-bindgen -- generate --library "$lib" --language kotlin --no-format --out-dir "$kotlin"
