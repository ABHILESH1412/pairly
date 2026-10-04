//! The relay wire protocol, shared by the client transport and `pairly-relay`.
//!
//! A device holds one QUIC connection per relay. Each request is a bidirectional stream that
//! starts with a [`Join`]:
//!
//! - **Listen** for a room: the relay answers with [`PRESENT`] / [`ABSENT`] whenever the other
//!   device's presence in the room changes, and [`MATCHED`] when that device dials in. After
//!   `MATCHED` the stream is a raw pipe to the dialer; the client opens a fresh listener.
//! - **Dial** a room: the relay pairs the stream with the other device's waiting listener and
//!   answers [`MATCHED`] (then pipes), or [`NO_PEER`].
//!
//! Rooms are opaque 16-byte ids derived from each pairing's secret, and everything after
//! `MATCHED` is a Noise session, so the relay learns neither who talks nor what is said.

use std::fmt;
use std::str::FromStr;

use data_encoding::BASE32_NOPAD;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const ALPN: &[u8] = b"pairly-relay/1";
/// TLS server name; the certificate is checked by its pinned hash, not by name.
pub const SERVER_NAME: &str = "pairly-relay";
pub const DEFAULT_PORT: u16 = 47200;
pub const MAGIC: [u8; 4] = *b"PRY1";
pub const ROOM_LEN: usize = 16;
pub const MAX_TOKEN_LEN: usize = 64;

/// Relay → client, one byte each.
pub const MATCHED: u8 = 1;
pub const PRESENT: u8 = 2;
pub const ABSENT: u8 = 3;
/// Bad access token, or a limit was hit.
pub const DENIED: u8 = 4;
/// Dial: nobody is listening in that room.
pub const NO_PEER: u8 = 5;

pub type Room = [u8; ROOM_LEN];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Listen = 1,
    Dial = 2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Join {
    pub role: Role,
    pub room: Room,
    pub token: Option<String>,
}

impl Join {
    pub fn encode(&self) -> Vec<u8> {
        let token = self.token.as_deref().unwrap_or("").as_bytes();
        let mut out = Vec::with_capacity(MAGIC.len() + 2 + ROOM_LEN + token.len());
        out.extend_from_slice(&MAGIC);
        out.push(self.role as u8);
        out.extend_from_slice(&self.room);
        // Tokens are validated to at most MAX_TOKEN_LEN bytes, so this fits.
        out.push(u8::try_from(token.len()).unwrap_or(0));
        out.extend_from_slice(token);
        out
    }

    pub async fn read(r: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Self> {
        let bad =
            |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned());
        let mut head = [0u8; MAGIC.len() + 1 + ROOM_LEN + 1];
        r.read_exact(&mut head).await?;
        if head[..4] != MAGIC {
            return Err(bad("not a pairly relay request"));
        }
        let role = match head[4] {
            1 => Role::Listen,
            2 => Role::Dial,
            _ => return Err(bad("unknown role")),
        };
        let mut room = [0u8; ROOM_LEN];
        room.copy_from_slice(&head[5..5 + ROOM_LEN]);
        let len = usize::from(head[5 + ROOM_LEN]);
        if len > MAX_TOKEN_LEN {
            return Err(bad("token too long"));
        }
        let mut token = vec![0u8; len];
        r.read_exact(&mut token).await?;
        let token = String::from_utf8(token).map_err(|_| bad("token is not UTF-8"))?;
        Ok(Self {
            role,
            room,
            token: (!token.is_empty()).then_some(token),
        })
    }
}

/// Where a relay is and how to trust it: `pairly-relay://[token@]host:port/pin`, where `pin` is
/// the base32 SHA-256 of the relay's certificate (printed by `pairly-relay` at startup).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RelayAddr {
    pub host: String,
    pub port: u16,
    pub pin: [u8; 32],
    pub token: Option<String>,
}

const SCHEME: &str = "pairly-relay://";

pub fn encode_pin(pin: &[u8; 32]) -> String {
    BASE32_NOPAD.encode(pin).to_ascii_lowercase()
}

fn valid_token(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= MAX_TOKEN_LEN
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
}

impl RelayAddr {
    /// `host:port` for resolving (IPv6 hosts in brackets).
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl fmt::Display for RelayAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(SCHEME)?;
        if let Some(token) = &self.token {
            write!(f, "{token}@")?;
        }
        write!(f, "{}/{}", self.authority(), encode_pin(&self.pin))
    }
}

/// Never print the token.
impl fmt::Debug for RelayAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RelayAddr({})", self.authority())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadRelayAddr(pub &'static str);

impl fmt::Display for BadRelayAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bad relay address: {}", self.0)
    }
}

impl std::error::Error for BadRelayAddr {}

impl FromStr for RelayAddr {
    type Err = BadRelayAddr;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s
            .trim()
            .strip_prefix(SCHEME)
            .ok_or(BadRelayAddr("must start with pairly-relay://"))?;
        let (authority, pin) = rest
            .rsplit_once('/')
            .ok_or(BadRelayAddr("missing /<pin> at the end"))?;
        let pin = BASE32_NOPAD
            .decode(pin.to_ascii_uppercase().as_bytes())
            .ok()
            .and_then(|p| <[u8; 32]>::try_from(p).ok())
            .ok_or(BadRelayAddr("the pin is not a certificate hash"))?;
        let (token, hostport) = match authority.rsplit_once('@') {
            Some((t, h)) if valid_token(t) => (Some(t.to_owned()), h),
            Some(_) => return Err(BadRelayAddr("the token may only use A-Z a-z 0-9 - _ . ~")),
            None => (None, authority),
        };
        let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
            let (host, port) = v6
                .split_once("]:")
                .ok_or(BadRelayAddr("expected [ipv6]:port"))?;
            (host, port)
        } else {
            hostport
                .rsplit_once(':')
                .ok_or(BadRelayAddr("expected host:port"))?
        };
        if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c == '/') {
            return Err(BadRelayAddr("bad host"));
        }
        let port = port.parse().map_err(|_| BadRelayAddr("bad port"))?;
        Ok(Self {
            host: host.to_owned(),
            port,
            pin,
            token,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn join_round_trips() {
        let join = Join {
            role: Role::Dial,
            room: [7; ROOM_LEN],
            token: Some("s3cret".into()),
        };
        let bytes = join.encode();
        assert_eq!(Join::read(&mut bytes.as_slice()).await.ok(), Some(join));
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert!(Join::read(&mut bad.as_slice()).await.is_err());
    }

    #[test]
    fn addresses_parse_and_print() {
        let pin = [42u8; 32];
        let a = RelayAddr {
            host: "relay.example.com".into(),
            port: 47200,
            pin,
            token: Some("abc-123".into()),
        };
        let s = a.to_string();
        assert!(s.starts_with("pairly-relay://abc-123@relay.example.com:47200/"));
        assert_eq!(s.parse::<RelayAddr>(), Ok(a));

        let v6: RelayAddr = format!("pairly-relay://[2001:db8::1]:9/{}", encode_pin(&pin))
            .parse()
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (v6.host.as_str(), v6.port, v6.token.as_deref()),
            ("2001:db8::1", 9, None)
        );
        assert!(!format!("{v6:?}").contains("abc"));

        for bad in [
            "relay.example.com:1/x",
            "pairly-relay://relay.example.com/abc",
            "pairly-relay://relay.example.com:1/notapin",
            "pairly-relay://bad token@h:1/aaaa",
        ] {
            assert!(bad.parse::<RelayAddr>().is_err(), "{bad}");
        }
    }
}
