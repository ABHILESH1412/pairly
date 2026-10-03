package dev.pairly.core

/**
 * Entry point to the Rust core. In Phase 3 `scripts/build-rust.sh` generates the UniFFI
 * bindings into `src/main/kotlin/uniffi/` and the shared libraries into `src/main/jniLibs/`.
 */
object PairlyCore {
    const val LIBRARY_NAME = "pairly_ffi"
}
