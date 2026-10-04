//! Decrypted packets from a peer: the envelope, then the body as every packet type (whatever
//! its `ty` says), plus the validation applied to identities.
#![no_main]

use ciborium::Value;
use libfuzzer_sys::fuzz_target;
use pairly_proto::Envelope;
use pairly_proto::packets::*;

fn bodies(value: &Value) {
    macro_rules! try_all {
        ($($t:ty),* $(,)?) => {$( let _ = value.deserialized::<$t>(); )*};
    }
    use pairly_plugins::*;
    try_all!(
        Ack, Keepalive, KeepaliveAck, PairCommit, PairConfirm, PairNonce, PairReveal,
        battery::BatteryState, clipboard::ClipboardSet,
        command::CommandDone, command::CommandList, command::CommandRun,
        contacts::ContactsRequest, contacts::ContactsResponse,
        files::FilesRequest, files::FilesResponse, findmy::Ring,
        input::KeyInput, input::PointerButton, input::PointerMotion,
        media::MediaArt, media::MediaCommand, media::MediaPlayers,
        notification::Action, notification::Active, notification::Dismiss,
        notification::Notification, notification::Removed, notification::Reply,
        ping::Ping, power::PowerRequest, power::PowerResponse,
        share::ShareAccept, share::ShareCancel, share::ShareChunk, share::ShareDone,
        share::ShareFinished, share::ShareOffer, share::ShareProgress, share::ShareText,
        sms::SmsNew, sms::SmsRequest, sms::SmsResponse, sms::SmsSend, sms::SmsStatus,
        telephony::CallControl, telephony::CallEvent, telephony::Dial,
    );
    if let Ok(identity) = value.deserialized::<Identity>() {
        let _ = identity.validate();
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(env) = Envelope::decode(data) {
        bodies(&env.body);
        // Whatever decodes must encode again.
        env.encode().expect("a decoded envelope re-encodes");
    }
    // Bodies directly, so the fuzzer needn't build a whole envelope to reach them.
    if let Ok(value) = ciborium::de::from_reader_with_recursion_limit::<Value, _>(data, 32) {
        bodies(&value);
    }
});
