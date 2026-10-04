//! TLS for relay connections. The relay has a self-signed certificate and clients pin its
//! SHA-256, so a relay needs no domain name or CA, and a stolen DNS name can't impersonate it.
//! (End-to-end security never depends on this: the Noise session inside is authenticated
//! against each device's pinned key. The pin protects the access token and metadata.)

use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::proto::{ALPN, SERVER_NAME};

/// Clients ping this often, which keeps NAT mappings (and mobile carriers' UDP state) open.
pub const KEEPALIVE: Duration = Duration::from_secs(15);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub fn pin_of(cert_der: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, cert_der);
    let mut pin = [0u8; 32];
    pin.copy_from_slice(digest.as_ref());
    pin
}

/// A new self-signed certificate: `(cert_der, pkcs8_key_der)`.
pub fn generate_cert() -> Result<(Vec<u8>, Vec<u8>), Error> {
    let cert = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_owned()])?;
    Ok((cert.cert.der().to_vec(), cert.signing_key.serialize_der()))
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport(server: bool, max_streams: u32) -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(IDLE_TIMEOUT.try_into().ok());
    if !server {
        t.keep_alive_interval(Some(KEEPALIVE));
    }
    t.max_concurrent_bidi_streams(max_streams.into());
    t.max_concurrent_uni_streams(0u32.into());
    Arc::new(t)
}

pub fn server_config(
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    max_streams_per_conn: u32,
) -> Result<quinn::ServerConfig, Error> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert_der)],
            PrivatePkcs8KeyDer::from(key_der).into(),
        )?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(transport(true, max_streams_per_conn));
    Ok(config)
}

pub fn client_config(pin: [u8; 32]) -> Result<quinn::ClientConfig, Error> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned { pin, provider }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(transport(false, 0));
    Ok(config)
}

/// Accepts exactly the certificate whose SHA-256 is `pin`.
#[derive(Debug)]
struct Pinned {
    pin: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // The pin is a public certificate hash, so a plain comparison is fine.
        if pin_of(end_entity) == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the relay's certificate doesn't match the pin in its address".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
