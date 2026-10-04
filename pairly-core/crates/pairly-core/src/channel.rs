//! An encrypted channel: hello + Noise handshake over a [`BoxDuplex`], then encrypted,
//! fragmented [`Envelope`]s.
//!
//! Wire format after the handshake: each frame is one Noise message whose plaintext is
//! `[flag][fragment]`, where `flag` is [`FRAGMENT_LAST`] or [`FRAGMENT_MORE`].

use std::sync::Arc;
use std::time::Duration;

use pairly_crypto::{
    DeviceId, Handshake, HandshakeInfo, HandshakeKind, IdentityKeypair, Initiate, PSK_LEN,
    PublicKey, SessionCipher,
};
use pairly_proto::frame::{read_frame, write_frame};
use pairly_proto::{Envelope, MAX_PACKET_SIZE, PacketBody, ProtoError};
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};

use crate::transport::{BoxDuplex, TransportKind};
use crate::{CoreError, Result};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const FRAGMENT_LAST: u8 = 0;
const FRAGMENT_MORE: u8 = 1;
const MAX_FRAGMENT: usize = SessionCipher::MAX_PLAINTEXT - 1;

/// Decides what an inbound handshake may do.
pub trait AcceptPolicy: Send + Sync {
    /// Accept a reconnect (`IK`) from this key? True only for paired devices.
    fn is_paired(&self, key: &PublicKey) -> bool;
    /// Accept new pairing requests?
    fn allow_pairing(&self) -> bool;
    /// PSK of the QR code currently on screen, if any.
    fn pairing_psk(&self) -> Option<[u8; PSK_LEN]>;
}

/// A freshly established channel.
pub struct Channel {
    pub reader: ChannelReader,
    pub writer: ChannelWriter,
    pub info: HandshakeInfo,
    /// Whether we opened this connection.
    pub initiator: bool,
    pub transport: TransportKind,
}

impl Channel {
    pub fn peer_id(&self) -> DeviceId {
        self.info.remote.device_id()
    }
}

/// Open a channel as the initiator.
pub async fn connect(
    stream: BoxDuplex,
    transport: TransportKind,
    how: Initiate<'_>,
    local: &IdentityKeypair,
) -> Result<Channel> {
    let mut stream = stream;
    let handshake = async {
        let mut hs = Handshake::initiator(how, local)?;
        write_frame(&mut stream, &how.kind().hello()).await?;
        drive(&mut stream, &mut hs, |_| Ok(())).await?;
        Ok::<_, CoreError>(hs)
    };
    let hs = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .map_err(|_| CoreError::Timeout)??;
    finish(stream, hs, true, transport)
}

/// Accept a channel as the responder.
pub async fn accept(
    stream: BoxDuplex,
    transport: TransportKind,
    local: &IdentityKeypair,
    policy: &dyn AcceptPolicy,
) -> Result<Channel> {
    let mut stream = stream;
    let handshake = async {
        let hello = read_frame(&mut stream).await?.ok_or(CoreError::Closed)?;
        let kind = HandshakeKind::from_hello(&hello)?;
        let mut hs = match kind {
            HandshakeKind::Pair if !policy.allow_pairing() => {
                return Err(CoreError::PairingDisabled);
            }
            HandshakeKind::Pair | HandshakeKind::Reconnect => {
                Handshake::responder(kind, local, None)?
            }
            HandshakeKind::PairPsk => {
                let psk = policy.pairing_psk().ok_or(CoreError::PairingDisabled)?;
                Handshake::responder(kind, local, Some(&psk))?
            }
        };
        // For IK the initiator's key is known after its first message: reject strangers before
        // we answer.
        drive(&mut stream, &mut hs, |hs| {
            match (kind, hs.remote_static()) {
                (HandshakeKind::Reconnect, Some(key)) if !policy.is_paired(&key) => {
                    Err(CoreError::NotPaired(key.device_id()))
                }
                _ => Ok(()),
            }
        })
        .await?;
        Ok::<_, CoreError>(hs)
    };
    let hs = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .map_err(|_| CoreError::Timeout)??;
    finish(stream, hs, false, transport)
}

async fn drive(
    stream: &mut BoxDuplex,
    hs: &mut Handshake,
    after_read: impl Fn(&Handshake) -> Result<()>,
) -> Result<()> {
    while !hs.is_finished() {
        if hs.is_my_turn() {
            write_frame(stream, &hs.write_message()?).await?;
        } else {
            let msg = read_frame(stream).await?.ok_or(CoreError::Closed)?;
            hs.read_message(&msg)?;
            after_read(hs)?;
        }
    }
    Ok(())
}

fn finish(
    stream: BoxDuplex,
    hs: Handshake,
    initiator: bool,
    transport: TransportKind,
) -> Result<Channel> {
    let (cipher, info) = hs.finish()?;
    let cipher = Arc::new(cipher);
    let (r, w) = tokio::io::split(stream);
    Ok(Channel {
        reader: ChannelReader {
            r,
            cipher: cipher.clone(),
            nonce: 0,
        },
        writer: ChannelWriter {
            w,
            cipher,
            nonce: 0,
        },
        info,
        initiator,
        transport,
    })
}

pub struct ChannelWriter {
    w: WriteHalf<BoxDuplex>,
    cipher: Arc<SessionCipher>,
    nonce: u64,
}

impl ChannelWriter {
    pub async fn send(&mut self, env: &Envelope) -> Result<()> {
        let data = env.encode()?;
        let mut chunks = data.chunks(MAX_FRAGMENT).peekable();
        while let Some(chunk) = chunks.next() {
            let mut plain = Vec::with_capacity(chunk.len() + 1);
            plain.push(if chunks.peek().is_some() {
                FRAGMENT_MORE
            } else {
                FRAGMENT_LAST
            });
            plain.extend_from_slice(chunk);
            let sealed = self.cipher.encrypt(self.nonce, &plain)?;
            self.nonce += 1;
            write_frame(&mut self.w, &sealed).await?;
        }
        Ok(())
    }

    /// Send a pre-session packet (identity, pairing). These use id 0 and are never acked.
    pub async fn send_body<T: PacketBody>(&mut self, body: &T) -> Result<()> {
        self.send(&Envelope::new(0, false, body)?).await
    }

    pub async fn shutdown(&mut self) {
        let _ = self.w.shutdown().await;
    }
}

pub struct ChannelReader {
    r: ReadHalf<BoxDuplex>,
    cipher: Arc<SessionCipher>,
    nonce: u64,
}

impl ChannelReader {
    /// Next envelope, or `None` on a clean close. Not cancel-safe.
    pub async fn recv(&mut self) -> Result<Option<Envelope>> {
        let mut packet = Vec::new();
        loop {
            let Some(frame) = read_frame(&mut self.r).await? else {
                return if packet.is_empty() {
                    Ok(None)
                } else {
                    Err(CoreError::Closed)
                };
            };
            let plain = self.cipher.decrypt(self.nonce, &frame)?;
            self.nonce += 1;
            let (&flag, fragment) = plain
                .split_first()
                .ok_or(CoreError::Violation("empty fragment"))?;
            if packet.len() + fragment.len() > MAX_PACKET_SIZE {
                return Err(ProtoError::TooLarge(packet.len() + fragment.len()).into());
            }
            packet.extend_from_slice(fragment);
            match flag {
                FRAGMENT_LAST => return Ok(Some(Envelope::decode(&packet)?)),
                FRAGMENT_MORE => {}
                _ => return Err(CoreError::Violation("bad fragment flag")),
            }
        }
    }

    /// Receive a pre-session packet of type `T`.
    pub async fn recv_body<T: PacketBody>(&mut self) -> Result<T> {
        let env = self.recv().await?.ok_or(CoreError::Closed)?;
        Ok(env.body()?)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    pub(crate) struct OpenPolicy {
        pub paired: Vec<PublicKey>,
        pub pairing: bool,
    }

    impl AcceptPolicy for OpenPolicy {
        fn is_paired(&self, key: &PublicKey) -> bool {
            self.paired.contains(key)
        }
        fn allow_pairing(&self) -> bool {
            self.pairing
        }
        fn pairing_psk(&self) -> Option<[u8; PSK_LEN]> {
            None
        }
    }

    /// A connected `XX` channel pair over an in-memory pipe.
    pub(crate) async fn channel_pair(
        a: &IdentityKeypair,
        b: &IdentityKeypair,
    ) -> (Channel, Channel) {
        let (sa, sb) = tokio::io::duplex(1 << 20);
        let policy = OpenPolicy {
            paired: vec![],
            pairing: true,
        };
        let (ca, cb) = tokio::join!(
            connect(Box::new(sa), TransportKind::Memory, Initiate::Pair, a),
            accept(Box::new(sb), TransportKind::Memory, b, &policy),
        );
        (ca.unwrap(), cb.unwrap())
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Blob {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    }
    impl PacketBody for Blob {
        const TYPE: &'static str = "test.blob";
    }

    #[tokio::test]
    async fn large_packets_are_fragmented_and_reassembled() {
        let (a, b) = (IdentityKeypair::generate(), IdentityKeypair::generate());
        let (mut ca, mut cb) = channel_pair(&a, &b).await;
        assert_eq!(ca.peer_id(), b.device_id());
        assert_eq!(cb.peer_id(), a.device_id());

        let big = Blob {
            data: (0..1_000_000u32).map(|i| i as u8).collect(),
        };
        let small = Blob {
            data: vec![1, 2, 3],
        };
        let send = async {
            ca.writer.send_body(&big).await.unwrap();
            ca.writer.send_body(&small).await.unwrap();
        };
        let recv = async {
            assert_eq!(cb.reader.recv_body::<Blob>().await.unwrap(), big);
            assert_eq!(cb.reader.recv_body::<Blob>().await.unwrap(), small);
        };
        tokio::join!(send, recv);

        ca.writer.shutdown().await;
        assert!(cb.reader.recv().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reconnect_from_unpaired_key_is_rejected() {
        let (a, b) = (IdentityKeypair::generate(), IdentityKeypair::generate());
        let (sa, sb) = tokio::io::duplex(4096);
        let bk = b.public();
        let policy = OpenPolicy {
            paired: vec![],
            pairing: true,
        };
        let (ca, cb) = tokio::join!(
            connect(
                Box::new(sa),
                TransportKind::Memory,
                Initiate::Reconnect(&bk),
                &a
            ),
            accept(Box::new(sb), TransportKind::Memory, &b, &policy),
        );
        assert!(ca.is_err());
        assert!(matches!(cb, Err(CoreError::NotPaired(id)) if id == a.device_id()));
    }

    #[tokio::test]
    async fn reconnect_from_paired_key_is_accepted() {
        let (a, b) = (IdentityKeypair::generate(), IdentityKeypair::generate());
        let (sa, sb) = tokio::io::duplex(4096);
        let bk = b.public();
        let policy = OpenPolicy {
            paired: vec![a.public()],
            pairing: false,
        };
        let (ca, cb) = tokio::join!(
            connect(
                Box::new(sa),
                TransportKind::Memory,
                Initiate::Reconnect(&bk),
                &a
            ),
            accept(Box::new(sb), TransportKind::Memory, &b, &policy),
        );
        assert_eq!(cb.unwrap().info.kind, HandshakeKind::Reconnect);
        assert_eq!(ca.unwrap().peer_id(), b.device_id());
    }

    #[tokio::test]
    async fn reconnect_to_impostor_fails() {
        let (a, b, mallory) = (
            IdentityKeypair::generate(),
            IdentityKeypair::generate(),
            IdentityKeypair::generate(),
        );
        let (sa, sb) = tokio::io::duplex(4096);
        let bk = b.public();
        let policy = OpenPolicy {
            paired: vec![a.public()],
            pairing: false,
        };
        let (ca, cb) = tokio::join!(
            connect(
                Box::new(sa),
                TransportKind::Memory,
                Initiate::Reconnect(&bk),
                &a
            ),
            accept(Box::new(sb), TransportKind::Memory, &mallory, &policy),
        );
        assert!(ca.is_err() && cb.is_err());
    }

    #[tokio::test]
    async fn pairing_can_be_disabled() {
        let (a, b) = (IdentityKeypair::generate(), IdentityKeypair::generate());
        let (sa, sb) = tokio::io::duplex(4096);
        let policy = OpenPolicy {
            paired: vec![],
            pairing: false,
        };
        let (ca, cb) = tokio::join!(
            connect(Box::new(sa), TransportKind::Memory, Initiate::Pair, &a),
            accept(Box::new(sb), TransportKind::Memory, &b, &policy),
        );
        assert!(ca.is_err());
        assert!(matches!(cb, Err(CoreError::PairingDisabled)));
    }
}
