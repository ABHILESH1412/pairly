//! Strings that come from outside: scanned QR codes, relay addresses (from a peer's identity
//! or the config), Bluetooth addresses, and the names of files a peer sends.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pairly_core::qr::QrInvite;
use pairly_plugins::share::safe_file_name;
use pairly_proto::packets::is_bluetooth_address;
use pairly_transport_relay::RelayAddr;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(qr) = QrInvite::parse(s) {
        // A parsed invite must survive a round trip.
        let again = QrInvite::parse(&qr.to_uri()).expect("re-parse");
        assert_eq!(again.to_uri(), qr.to_uri());
    }
    if let Ok(addr) = s.parse::<RelayAddr>() {
        let _ = addr.authority();
    }
    let _ = is_bluetooth_address(s);
    // A received file's name can't climb out of the downloads folder or hide itself.
    let name = safe_file_name(s);
    assert!(!name.is_empty() && !name.contains(['/', '\\']), "{s:?} -> {name:?}");
    assert!(!name.starts_with('.') && name.chars().all(|c| !c.is_control()), "{s:?} -> {name:?}");
});
