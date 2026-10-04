# pairly-android

Android app: Kotlin + Jetpack Compose (Material 3), a foreground service that keeps the
connection alive, and `core-bindings`: the Rust core (`../pairly-core/crates/pairly-ffi`)
built with cargo-ndk plus its UniFFI-generated Kotlin (`dev.pairly.core.ffi`).
See `../plan.md` section 10.

## Build and run

Requirements: rustup with the Android targets, `cargo install cargo-ndk`, the Android SDK and
NDK (`ndkVersion` in `gradle/libs.versions.toml`), JDK 21+ (Android Studio's JBR works).

```sh
./gradlew installDebug                                # builds Rust for arm64-v8a + x86_64 too
./gradlew installDebug -Ppairly.abis=arm64-v8a        # faster: phone ABI only
```

Opening the project in Android Studio and pressing Run does the same. The Rust build runs
as `:core-bindings:buildRustDebug` and writes to `core-bindings/build/generated/`.

## Connecting to the PC

The phone and PC must be able to reach each other directly:

- On the PC, run `pairlyd` (see `../pairly-linux/README.md`). With firewalld, allow
  mDNS and UDP 47100 in your Wi-Fi's zone.
- Some routers isolate Wi-Fi clients from each other. If `adb shell ping <pc-ip>` fails,
  use USB tethering or the phone's hotspot until the relay lands (Phase 8).

## Sharing

- **To the PC:** share from any app and pick **Pairly** (files, links, text), or use
  **Send files** on a device card.
- **From the PC:** files are saved to `Download/Pairly` (hidden until the checksum verifies)
  with a progress notification; tap the finished one to open it. Links arrive as a
  notification to tap, since Android doesn't let background apps open the browser. Text goes to
  the clipboard. Turn on **Ask before receiving files** to get Accept/Decline instead.

Logs: `adb logcat -s pairly:V Pairly:V`.
