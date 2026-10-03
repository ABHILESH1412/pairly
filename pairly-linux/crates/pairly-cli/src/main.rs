//! `pairly`: command-line client talking to `pairlyd` over D-Bus.
#![forbid(unsafe_code)]

fn main() {
    println!(
        "pairly {} (daemon: {})",
        env!("CARGO_PKG_VERSION"),
        pairly_dbus::BUS_NAME
    );
}
