//! QUIC endpoint and the stream wrapper that the core sees as a `Duplex`.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use pairly_core::{CoreError, Result};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const ALPN: &[u8] = b"pairly/1";
const SERVER_NAME: &str = "pairly";
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a dropped stream waits for its last bytes to be acknowledged before closing.
const LINGER: Duration = Duration::from_secs(2);

fn err(e: impl std::fmt::Display) -> CoreError {
    CoreError::Transport(e.to_string())
}

#[derive(Clone)]
pub struct QuicEndpoint {
    endpoint: quinn::Endpoint,
}

impl QuicEndpoint {
    /// Bind `port` on all interfaces (dual-stack where available), or a random port if taken.
    pub fn bind(port: u16) -> Result<Self> {
        let socket = bind_socket(port).or_else(|_| bind_socket(0))?;
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(server_config()?),
            socket,
            Arc::new(quinn::TokioRuntime),
        )?;
        let mut endpoint = endpoint;
        endpoint.set_default_client_config(client_config()?);
        Ok(Self { endpoint })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    pub async fn accept(&self) -> Option<quinn::Incoming> {
        self.endpoint.accept().await
    }

    pub async fn connect(&self, addr: SocketAddr) -> Result<QuicStream> {
        let connect = async {
            let conn = self
                .endpoint
                .connect(addr, SERVER_NAME)
                .map_err(err)?
                .await
                .map_err(err)?;
            let (send, recv) = conn.open_bi().await.map_err(err)?;
            Ok(QuicStream {
                conn,
                send: Some(send),
                recv,
            })
        };
        tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| CoreError::Timeout)?
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"bye");
    }
}

/// Complete an inbound connection and take its (single) stream.
pub async fn accept_stream(incoming: quinn::Incoming) -> Result<QuicStream> {
    let accept = async {
        let conn = incoming.await.map_err(err)?;
        let (send, recv) = conn.accept_bi().await.map_err(err)?;
        Ok(QuicStream {
            conn,
            send: Some(send),
            recv,
        })
    };
    tokio::time::timeout(CONNECT_TIMEOUT, accept)
        .await
        .map_err(|_| CoreError::Timeout)?
}

fn bind_socket(port: u16) -> io::Result<UdpSocket> {
    UdpSocket::bind((Ipv6Addr::UNSPECIFIED, port))
        .or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)))
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(
        IDLE_TIMEOUT
            .try_into()
            .expect("60s is a valid idle timeout"),
    ));
    t.max_concurrent_bidi_streams(1u32.into());
    t.max_concurrent_uni_streams(0u32.into());
    Arc::new(t)
}

fn server_config() -> Result<quinn::ServerConfig> {
    let cert = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_owned()]).map_err(err)?;
    let cert_der = CertificateDer::from(cert.cert);
    let key = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(err)?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key.into())
        .map_err(err)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls).map_err(err)?));
    config.transport_config(transport_config());
    Ok(config)
}

fn client_config() -> Result<quinn::ClientConfig> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(err)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoiseAuthenticates(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config =
        quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).map_err(err)?));
    config.transport_config(transport_config());
    Ok(config)
}

/// Accepts any server certificate. Safe here because QUIC is only the outer layer: the peer is
/// authenticated by the Noise handshake inside the stream, against its pinned static key.
#[derive(Debug)]
struct NoiseAuthenticates(Arc<CryptoProvider>);

impl ServerCertVerifier for NoiseAuthenticates {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
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
        verify_tls13_signature(
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

/// One QUIC connection with its single bidirectional stream.
pub struct QuicStream {
    conn: quinn::Connection,
    send: Option<quinn::SendStream>,
    recv: quinn::RecvStream,
}

impl QuicStream {
    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }
}

impl AsyncRead for QuicStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for QuicStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut().send.as_mut() {
            Some(send) => AsyncWrite::poll_write(Pin::new(send), cx, buf),
            None => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut().send.as_mut() {
            Some(send) => AsyncWrite::poll_flush(Pin::new(send), cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut().send.as_mut() {
            Some(send) => AsyncWrite::poll_shutdown(Pin::new(send), cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

impl Drop for QuicStream {
    fn drop(&mut self) {
        // Closing the connection right away would discard bytes still in flight (e.g. the acks
        // a graceful session close just wrote), so linger until the peer has them.
        let conn = self.conn.clone();
        let send = self.send.take();
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            conn.close(0u32.into(), b"");
            return;
        };
        rt.spawn(async move {
            if let Some(mut send) = send {
                let _ = send.finish();
                let _ = tokio::time::timeout(LINGER, send.stopped()).await;
            }
            conn.close(0u32.into(), b"");
        });
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn stream_roundtrip_over_loopback() {
        let server = QuicEndpoint::bind(0).unwrap();
        let client = QuicEndpoint::bind(0).unwrap();
        let port = server.local_addr().unwrap().port();
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();

        let accept = async {
            let mut s = accept_stream(server.accept().await.unwrap()).await.unwrap();
            let mut buf = [0u8; 5];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(b"world").await.unwrap();
            s.shutdown().await.unwrap();
            buf
        };
        let connect = async {
            let mut c = client.connect(addr).await.unwrap();
            c.write_all(b"hello").await.unwrap();
            let mut buf = Vec::new();
            c.read_to_end(&mut buf).await.unwrap();
            buf
        };
        let (got_server, got_client) = tokio::join!(accept, connect);
        assert_eq!(&got_server, b"hello");
        assert_eq!(got_client, b"world");
    }
}
