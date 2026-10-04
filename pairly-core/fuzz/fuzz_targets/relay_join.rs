//! The relay's first read from any client on the internet: the JOIN request.
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use pairly_transport_relay::proto::{Join, MAX_TOKEN_LEN};

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Builder::new_current_thread().build().unwrap())
}

fuzz_target!(|data: &[u8]| {
    let mut input = data;
    if let Ok(join) = runtime().block_on(Join::read(&mut input)) {
        assert!(join.token.as_ref().is_none_or(|t| t.len() <= MAX_TOKEN_LEN));
        // What was read encodes back to the bytes it came from.
        let encoded = join.encode();
        assert_eq!(&data[..encoded.len()], &encoded[..]);
    }
});
