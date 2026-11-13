//! Wireless debugging on this phone, from Pairly itself: pairing once (with the 6-digit code
//! Android shows in Settings → Developer options → Wireless debugging) and running a shell
//! command over adb's TLS connection. That's how Pairly starts its screen helper as Android's
//! "shell" user without a PC, the way Shizuku does.
//!
//! Ported from AOSP's adb (`pairing_auth`, `pairing_connection`, `tls`, the adb protocol) and
//! BoringSSL's SPAKE2 (`crypto/curve25519/spake25519.cc`); the pairing must match theirs byte
//! for byte, so the comments point at what each step mirrors.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use ring::rand::SecureRandom;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

const TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) type Result<T> = std::result::Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ----- SPAKE2 (BoringSSL's) --------------------------------------------------------------------

pub(crate) mod spake2 {
    use curve25519_dalek::constants::ED25519_BASEPOINT_POINT;
    use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
    use curve25519_dalek::scalar::Scalar;
    use ring::digest::{Context, SHA512, digest};

    // The two fixed points, from the comment above BoringSSL's precomputed tables.
    const M: [u8; 32] = hex32("5ada7e4bf6ddd9adb6626d32131c6b5c51a1e347a3478f53cfcf441b88eed12e");
    const N: [u8; 32] = hex32("10e3df0ae37d8e7a99b5fe74b44672103dbddcbd06af680d71329a11693bc778");

    const fn hex32(s: &str) -> [u8; 32] {
        let b = s.as_bytes();
        let mut out = [0; 32];
        let mut i = 0;
        while i < 32 {
            out[i] = (nibble(b[2 * i]) << 4) | nibble(b[2 * i + 1]);
            i += 1;
        }
        out
    }

    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            _ => c - b'a' + 10,
        }
    }

    fn point(bytes: [u8; 32]) -> EdwardsPoint {
        CompressedEdwardsY(bytes)
            .decompress()
            .expect("SPAKE2's fixed points are on the curve")
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Role {
        /// The pairing client.
        Alice,
        /// The pairing server (the phone's adbd): only the tests play it.
        #[cfg_attr(not(test), allow(dead_code))]
        Bob,
    }

    pub struct Spake2 {
        role: Role,
        my_name: Vec<u8>,
        their_name: Vec<u8>,
        /// BoringSSL's private key is 8·r (`left_shift_3` of a reduced random scalar).
        r: Scalar,
        /// BoringSSL's password scalar is h(password) plus multiples of the group order, made a
        /// multiple of eight; it's 8·k.
        k: Scalar,
        password_hash: [u8; 64],
        my_msg: [u8; 32],
    }

    impl Spake2 {
        /// `random`: 64 random bytes (the private key).
        pub fn new(
            role: Role,
            my_name: &[u8],
            their_name: &[u8],
            password: &[u8],
            random: &[u8; 64],
        ) -> Self {
            let r = Scalar::from_bytes_mod_order_wide(random);
            let p = (ED25519_BASEPOINT_POINT * r).mul_by_cofactor();
            let mut password_hash = [0; 64];
            password_hash.copy_from_slice(digest(&SHA512, password).as_ref());
            // The password scalar ps ≡ h (mod l), 8 | ps and ps < 8·l, so ps/8 = h/8 (mod l).
            let h = Scalar::from_bytes_mod_order_wide(&password_hash);
            let k = h * Scalar::from(8_u8).invert();
            let mask = (point(if role == Role::Alice { M } else { N }) * k).mul_by_cofactor();
            Self {
                role,
                my_name: my_name.to_vec(),
                their_name: their_name.to_vec(),
                r,
                k,
                password_hash,
                my_msg: (p + mask).compress().to_bytes(),
            }
        }

        /// The message to send.
        pub fn msg(&self) -> [u8; 32] {
            self.my_msg
        }

        /// The shared key (64 bytes), from the other side's message.
        pub fn finish(&self, their_msg: &[u8]) -> Option<[u8; 64]> {
            let theirs: [u8; 32] = their_msg.try_into().ok()?;
            let q_star = CompressedEdwardsY(theirs).decompress()?;
            let their_point = if self.role == Role::Alice { N } else { M };
            let q = q_star - (point(their_point) * self.k).mul_by_cofactor();
            let dh = (q * self.r).mul_by_cofactor().compress().to_bytes();
            let mut sha = Context::new(&SHA512);
            let mut add = |data: &[u8]| {
                sha.update(&(data.len() as u64).to_le_bytes());
                sha.update(data);
            };
            if self.role == Role::Alice {
                add(&self.my_name);
                add(&self.their_name);
                add(&self.my_msg);
                add(&theirs);
            } else {
                add(&self.their_name);
                add(&self.my_name);
                add(&theirs);
                add(&self.my_msg);
            }
            add(&dh);
            add(&self.password_hash);
            let mut key = [0; 64];
            key.copy_from_slice(sha.finish().as_ref());
            Some(key)
        }
    }
}

// ----- the pairing cipher (adb's Aes128Gcm) ----------------------------------------------------

struct Cipher {
    key: ring::aead::LessSafeKey,
    sent: u64,
    received: u64,
}

struct Len16;

impl ring::hkdf::KeyType for Len16 {
    fn len(&self) -> usize {
        16
    }
}

impl Cipher {
    fn new(key_material: &[u8]) -> Result<Self> {
        let mut key = [0; 16];
        ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, &[])
            .extract(key_material)
            .expand(&[b"adb pairing_auth aes-128-gcm key"], Len16)
            .and_then(|okm| okm.fill(&mut key))
            .map_err(|_| "key derivation failed")?;
        let key = ring::aead::UnboundKey::new(&ring::aead::AES_128_GCM, &key)
            .map_err(|_| "bad pairing key")?;
        Ok(Self {
            key: ring::aead::LessSafeKey::new(key),
            sent: 0,
            received: 0,
        })
    }

    /// The nonce is the message counter's bytes (as memcpy'd on a little-endian phone).
    fn nonce(n: u64) -> ring::aead::Nonce {
        let mut nonce = [0; 12];
        nonce[..8].copy_from_slice(&n.to_le_bytes());
        ring::aead::Nonce::assume_unique_for_key(nonce)
    }

    fn seal(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut out = data.to_vec();
        self.key
            .seal_in_place_append_tag(Self::nonce(self.sent), ring::aead::Aad::empty(), &mut out)
            .map_err(|_| "encryption failed")?;
        self.sent += 1;
        Ok(out)
    }

    fn open(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        let mut buf = data.to_vec();
        let len = self
            .key
            .open_in_place(
                Self::nonce(self.received),
                ring::aead::Aad::empty(),
                &mut buf,
            )
            .ok()?
            .len();
        self.received += 1;
        buf.truncate(len);
        Some(buf)
    }
}

// ----- pairing -----------------------------------------------------------------------------------

const PAIRING_VERSION: u8 = 1;
const SPAKE2_MSG: u8 = 0;
const PEER_INFO: u8 = 1;
/// `PeerInfo`: `[type u8][data, zero-padded]`, 8192 bytes.
const PEER_INFO_SIZE: usize = 8192;
const ADB_RSA_PUB_KEY: u8 = 0;
const CLIENT_NAME: &[u8] = b"adb pair client\0";
const SERVER_NAME: &[u8] = b"adb pair server\0";
const EXPORT_LABEL: &[u8] = b"adb-label\0";

fn write_pairing(w: &mut impl Write, kind: u8, payload: &[u8]) -> Result<()> {
    let mut packet = vec![PAIRING_VERSION, kind];
    packet.extend_from_slice(&u32::try_from(payload.len()).map_err(err)?.to_be_bytes());
    packet.extend_from_slice(payload);
    w.write_all(&packet).map_err(err)?;
    w.flush().map_err(err)
}

fn read_pairing(r: &mut impl Read, kind: u8) -> Result<Vec<u8>> {
    let mut head = [0; 6];
    r.read_exact(&mut head)
        .map_err(|_| "the phone ended the pairing (wrong code?)")?;
    let len = u32::from_be_bytes([head[2], head[3], head[4], head[5]]) as usize;
    if head[1] != kind || len == 0 || len > 2 * PEER_INFO_SIZE {
        return Err("unexpected pairing message".into());
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).map_err(err)?;
    Ok(body)
}

fn random64() -> Result<[u8; 64]> {
    let mut random = [0; 64];
    ring::rand::SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| "no randomness")?;
    Ok(random)
}

/// The SPAKE2 exchange and the encrypted `PeerInfo` swap, after the TLS handshake. `exported`:
/// the TLS connection's exported key (so the pairing can't be relayed). Returns the other
/// side's `PeerInfo`.
fn pairing_exchange(
    stream: &mut (impl Read + Write),
    role: spake2::Role,
    code: &str,
    exported: &[u8; 64],
    my_info: &[u8; PEER_INFO_SIZE],
) -> Result<Vec<u8>> {
    let mut password = code.as_bytes().to_vec();
    password.extend_from_slice(exported);
    let (me, them) = match role {
        spake2::Role::Alice => (CLIENT_NAME, SERVER_NAME),
        spake2::Role::Bob => (SERVER_NAME, CLIENT_NAME),
    };
    let spake = spake2::Spake2::new(role, me, them, &password, &random64()?);
    let theirs = if role == spake2::Role::Alice {
        write_pairing(stream, SPAKE2_MSG, &spake.msg())?;
        read_pairing(stream, SPAKE2_MSG)?
    } else {
        let theirs = read_pairing(stream, SPAKE2_MSG)?;
        write_pairing(stream, SPAKE2_MSG, &spake.msg())?;
        theirs
    };
    let key = spake.finish(&theirs).ok_or("bad pairing message")?;
    let mut cipher = Cipher::new(&key)?;
    let wrong = "wrong pairing code";
    if role == spake2::Role::Alice {
        write_pairing(stream, PEER_INFO, &cipher.seal(my_info)?)?;
        let theirs = read_pairing(stream, PEER_INFO).map_err(|_| wrong)?;
        cipher.open(&theirs).ok_or_else(|| wrong.into())
    } else {
        let theirs = read_pairing(stream, PEER_INFO)?;
        let info = cipher.open(&theirs).ok_or(wrong)?;
        write_pairing(stream, PEER_INFO, &cipher.seal(my_info)?)?;
        Ok(info)
    }
}

/// Our `PeerInfo`: the public key in adb's format (`<base64> <name>`).
fn key_info(public_key: &str) -> Result<[u8; PEER_INFO_SIZE]> {
    let mut info = [0; PEER_INFO_SIZE];
    info[0] = ADB_RSA_PUB_KEY;
    let key = public_key.as_bytes();
    if key.len() >= PEER_INFO_SIZE - 1 {
        return Err("public key too long".into());
    }
    info[1..=key.len()].copy_from_slice(key);
    Ok(info)
}

/// Pair with this phone's wireless debugging (`port`: the pairing port it shows, `code`: the
/// 6-digit code), so it trusts `key_pkcs8` (an RSA key; `public_key` is it in adb's format).
pub fn pair(port: u16, code: &str, key_pkcs8: &[u8], public_key: &str) -> Result<()> {
    let mut tls = tls_client(connect(port)?, key_pkcs8)?;
    let mut exported = [0; 64];
    tls.conn
        .export_keying_material(&mut exported, EXPORT_LABEL, None)
        .map_err(err)?;
    pairing_exchange(
        &mut tls,
        spake2::Role::Alice,
        code,
        &exported,
        &key_info(public_key)?,
    )?;
    Ok(())
}

// ----- TLS -------------------------------------------------------------------------------------

fn connect(port: u16) -> Result<TcpStream> {
    let tcp = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), TIMEOUT)
        .map_err(|e| format!("can't reach wireless debugging (is it on?): {e}"))?;
    tcp.set_read_timeout(Some(TIMEOUT)).map_err(err)?;
    tcp.set_write_timeout(Some(TIMEOUT)).map_err(err)?;
    Ok(tcp)
}

/// A self-signed certificate for `key_pkcs8` (adb identifies us by the key, not the cert).
fn certificate(key_pkcs8: &[u8]) -> Result<CertificateDer<'static>> {
    let key = PrivatePkcs8KeyDer::from(key_pkcs8.to_vec());
    let pair = rcgen::KeyPair::try_from(&key).map_err(err)?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(err)?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Pairly");
    Ok(params.self_signed(&pair).map_err(err)?.der().clone())
}

/// adbd's certificate is self-signed and changes; who it is was settled by pairing (or it's
/// this phone, on 127.0.0.1). Signatures are still checked.
#[derive(Debug)]
struct AnyCertificate(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// TLS 1.3 (as adb requires) with our key as the client certificate, handshake done.
fn tls_client(
    tcp: TcpStream,
    key_pkcs8: &[u8],
) -> Result<StreamOwned<ClientConnection, TcpStream>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(err)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_client_auth_cert(
            vec![certificate(key_pkcs8)?],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pkcs8.to_vec())),
        )
        .map_err(err)?;
    let name = ServerName::IpAddress(Ipv4Addr::LOCALHOST.into());
    let conn = ClientConnection::new(Arc::new(config), name).map_err(err)?;
    let mut tls = StreamOwned::new(conn, tcp);
    while tls.conn.is_handshaking() {
        tls.conn
            .complete_io(&mut tls.sock)
            .map_err(|e| format!("secure connection failed (not paired?): {e}"))?;
    }
    Ok(tls)
}

// ----- the adb protocol ------------------------------------------------------------------------

const A_CNXN: u32 = 0x4e58_4e43;
const A_STLS: u32 = 0x534c_5453;
const A_AUTH: u32 = 0x4854_5541;
const A_OPEN: u32 = 0x4e45_504f;
const A_OKAY: u32 = 0x5941_4b4f;
const A_WRTE: u32 = 0x4554_5257;
const A_CLSE: u32 = 0x4553_4c43;
const A_VERSION: u32 = 0x0100_0001;
const A_STLS_VERSION: u32 = 0x0100_0000;
const MAX_PAYLOAD: u32 = 1024 * 1024;

struct Message {
    command: u32,
    arg0: u32,
    arg1: u32,
    data: Vec<u8>,
}

fn write_message(
    w: &mut impl Write,
    command: u32,
    arg0: u32,
    arg1: u32,
    data: &[u8],
) -> Result<()> {
    let len = u32::try_from(data.len()).map_err(err)?;
    let mut packet = Vec::with_capacity(24 + data.len());
    // The checksum is optional from A_VERSION 0x01000001 on.
    for word in [command, arg0, arg1, len, 0, command ^ 0xffff_ffff] {
        packet.extend_from_slice(&word.to_le_bytes());
    }
    packet.extend_from_slice(data);
    w.write_all(&packet).map_err(err)?;
    w.flush().map_err(err)
}

fn read_message(r: &mut impl Read) -> Result<Message> {
    let mut head = [0; 24];
    r.read_exact(&mut head).map_err(err)?;
    let word = |i: usize| u32::from_le_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
    let len = word(12);
    if len > MAX_PAYLOAD {
        return Err("adb message too large".into());
    }
    let mut data = vec![0; len as usize];
    r.read_exact(&mut data).map_err(err)?;
    Ok(Message {
        command: word(0),
        arg0: word(4),
        arg1: word(8),
        data,
    })
}

/// Run `command` as the shell user through this phone's wireless debugging (`port`: its
/// connect port), with a key paired before. Returns what it printed.
pub fn shell(port: u16, key_pkcs8: &[u8], command: &str) -> Result<String> {
    let mut tcp = connect(port)?;
    write_message(&mut tcp, A_CNXN, A_VERSION, MAX_PAYLOAD, b"host::\0")?;
    let reply = read_message(&mut tcp)?;
    match reply.command {
        A_STLS => {}
        A_AUTH => return Err("wireless debugging wants the older login: pair Pairly again".into()),
        _ => return Err("unexpected answer from wireless debugging".into()),
    }
    write_message(&mut tcp, A_STLS, A_STLS_VERSION, 0, &[])?;
    let mut tls = tls_client(tcp, key_pkcs8)?;
    if read_message(&mut tls)?.command != A_CNXN {
        return Err("wireless debugging didn't let Pairly in (pair again?)".into());
    }
    const LOCAL: u32 = 1;
    write_message(
        &mut tls,
        A_OPEN,
        LOCAL,
        0,
        format!("shell:{command}\0").as_bytes(),
    )?;
    let mut output = Vec::new();
    loop {
        let m = read_message(&mut tls)?;
        match m.command {
            A_OKAY => {}
            A_WRTE => {
                output.extend_from_slice(&m.data);
                write_message(&mut tls, A_OKAY, LOCAL, m.arg0, &[])?;
            }
            A_CLSE => {
                let _ = write_message(&mut tls, A_CLSE, LOCAL, m.arg0, &[]);
                break;
            }
            _ => {}
        }
        let _ = m.arg1;
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

// ----- for the phone app ---------------------------------------------------------------------

/// Pair Pairly with this phone's wireless debugging: `port` and `code` as Settings shows them
/// ("Pair device with pairing code"), `key_pkcs8` Pairly's RSA key and `public_key` the same in
/// adb's format (`<base64> <name>`). Blocks for up to a few seconds.
#[uniffi::export]
pub fn adb_pair(
    port: u16,
    code: String,
    key_pkcs8: Vec<u8>,
    public_key: String,
) -> std::result::Result<(), crate::PairlyError> {
    pair(port, code.trim(), &key_pkcs8, &public_key)
        .map_err(|reason| crate::PairlyError::Failed { reason })
}

/// Run `command` as Android's shell user through wireless debugging (`port`: its connect port)
/// with the key paired before; returns its output. Blocks for up to a few seconds.
#[uniffi::export]
pub fn adb_shell(
    port: u16,
    key_pkcs8: Vec<u8>,
    command: String,
) -> std::result::Result<String, crate::PairlyError> {
    shell(port, &key_pkcs8, &command).map_err(|reason| crate::PairlyError::Failed { reason })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::{ServerConfig, ServerConnection};

    #[test]
    fn both_sides_agree_on_the_key_only_with_the_same_password() {
        use spake2::{Role, Spake2};
        let alice = Spake2::new(Role::Alice, CLIENT_NAME, SERVER_NAME, b"123456", &[7; 64]);
        let bob = Spake2::new(Role::Bob, SERVER_NAME, CLIENT_NAME, b"123456", &[9; 64]);
        let a = alice.finish(&bob.msg()).unwrap();
        assert_eq!(Some(a), bob.finish(&alice.msg()));
        let mallory = Spake2::new(Role::Bob, SERVER_NAME, CLIENT_NAME, b"654321", &[9; 64]);
        assert_ne!(Some(a), mallory.finish(&alice.msg()));
    }

    /// The phone's side of pairing, for tests: a TLS server doing adbd's half.
    fn serve_pairing(
        listener: std::net::TcpListener,
        code: &'static str,
    ) -> std::thread::JoinHandle<Result<Vec<u8>>> {
        std::thread::spawn(move || {
            let (tcp, _) = listener.accept().map_err(err)?;
            let key = rcgen::KeyPair::generate().map_err(err)?;
            let cert = rcgen::CertificateParams::new(vec!["adbd".into()])
                .map_err(err)?
                .self_signed(&key)
                .map_err(err)?;
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = ServerConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(err)?
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.der().clone()],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
                )
                .map_err(err)?;
            let mut tls =
                StreamOwned::new(ServerConnection::new(Arc::new(config)).map_err(err)?, tcp);
            while tls.conn.is_handshaking() {
                tls.conn.complete_io(&mut tls.sock).map_err(err)?;
            }
            let mut exported = [0; 64];
            tls.conn
                .export_keying_material(&mut exported, EXPORT_LABEL, None)
                .map_err(err)?;
            let mut info = [0; PEER_INFO_SIZE];
            info[0] = 1; // ADB_DEVICE_GUID
            info[1..12].copy_from_slice(b"pairly-test");
            pairing_exchange(&mut tls, spake2::Role::Bob, code, &exported, &info)
        })
    }

    fn test_key() -> Vec<u8> {
        rcgen::KeyPair::generate().unwrap().serialize_der()
    }

    #[test]
    fn pairs_with_the_right_code_only() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = serve_pairing(listener, "482913");
        pair(port, "482913", &test_key(), "QUFB pairly@test").unwrap();
        let info = server.join().unwrap().unwrap();
        assert_eq!(&info[..17], b"\0QUFB pairly@test");

        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = serve_pairing(listener, "482913");
        assert!(pair(port, "000000", &test_key(), "QUFB pairly@test").is_err());
        assert!(server.join().unwrap().is_err());
    }

    /// Google's own `adb pair` against our implementation of the phone's side: proves the
    /// handshake matches BoringSSL and adb byte for byte. Needs `adb`; uses a separate adb
    /// server and home folder, so the PC's own adb setup is untouched.
    #[test]
    #[ignore = "needs adb installed"]
    fn real_adb_pairs_with_us() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = serve_pairing(listener, "135790");
        let home = std::env::temp_dir().join(format!("pairly-adb-test-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let out = std::process::Command::new("adb")
            .args(["-P", "5097", "pair", &format!("127.0.0.1:{port}"), "135790"])
            .env("ANDROID_USER_HOME", &home)
            .output()
            .unwrap();
        let _ = std::process::Command::new("adb")
            .args(["-P", "5097", "kill-server"])
            .env("ANDROID_USER_HOME", &home)
            .output();
        let _ = std::fs::remove_dir_all(&home);
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        let info = server.join().unwrap();
        assert!(
            text.contains("Successfully paired"),
            "adb said: {text}; our side: {info:?}"
        );
        assert_eq!(info.unwrap()[0], ADB_RSA_PUB_KEY);
    }
}
