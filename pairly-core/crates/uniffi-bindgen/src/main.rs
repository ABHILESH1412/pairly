//! `cargo run -p uniffi-bindgen -- generate --library <lib> --language kotlin --out-dir <dir>`
#![forbid(unsafe_code)]

fn main() {
    uniffi::uniffi_bindgen_main();
}
