//! `pairlyd`: headless daemon started as a systemd user service.
#![forbid(unsafe_code)]

fn main() {
    println!(
        "pairlyd {} (protocol v{}), bus name {}",
        env!("CARGO_PKG_VERSION"),
        pairly_core::PROTOCOL_VERSION,
        pairly_dbus::BUS_NAME
    );
}
