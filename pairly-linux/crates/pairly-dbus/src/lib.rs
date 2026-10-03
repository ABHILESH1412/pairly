//! The `dev.pairly.Daemon1` D-Bus interface: method/signal definitions, transfer types and
//! zbus proxies used by every Pairly client on Linux.
#![forbid(unsafe_code)]

/// Well-known bus name owned by `pairlyd`.
pub const BUS_NAME: &str = "dev.pairly.Daemon";
/// Object path of the daemon interface.
pub const OBJECT_PATH: &str = "/dev/pairly/Daemon";
