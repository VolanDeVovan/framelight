//! TLS for nvhttp port 47984: the host cert, and a client-cert verifier that accepts any
//! certificate (pairing is the trust decision, checked against the paired store) while
//! still checking the handshake signature against the presented key.
//!
//! RSA signatures are verified with the pure-Rust `rsa` crate from the raw SPKI, so
//! odd client certificates (X.509 v1, expired) that webpki would reject still work.

use crate::identity::Identity;
use anyhow::{Context, Result};
use rsa::RsaPublicKey;
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::Verifier;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error, ServerConfig, SignatureScheme};
use std::sync::Arc;

pub fn server_config(id: &Identity) -> Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(AnyClientCert { provider: provider.clone() });
    let cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS versions")?
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from(id.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(id.key_pkcs8_der.clone())),
        )
        .context("TLS server cert")?;
    Ok(Arc::new(cfg))
}

#[derive(Debug)]
struct AnyClientCert {
    provider: Arc<CryptoProvider>,
}

impl AnyClientCert {
    fn verify(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
        tls13: bool,
    ) -> Result<HandshakeSignatureValid, Error> {
        use sha2::{Sha256, Sha384, Sha512};
        let bad = || Error::InvalidCertificate(rustls::CertificateError::BadSignature);
        let rsa_key = || -> Result<RsaPublicKey, Error> {
            let (_, x509) = x509_parser::parse_x509_certificate(cert.as_ref())
                .map_err(|_| Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
            RsaPublicKey::from_public_key_der(x509.public_key().raw)
                .map_err(|_| Error::InvalidCertificate(rustls::CertificateError::BadEncoding))
        };
        let sig = dss.signature();
        macro_rules! pkcs1 {
            ($h:ty) => {{
                let s = rsa::pkcs1v15::Signature::try_from(sig).map_err(|_| bad())?;
                rsa::pkcs1v15::VerifyingKey::<$h>::new(rsa_key()?)
                    .verify(message, &s)
                    .map_err(|_| bad())
            }};
        }
        macro_rules! pss {
            ($h:ty) => {{
                let s = rsa::pss::Signature::try_from(sig).map_err(|_| bad())?;
                rsa::pss::VerifyingKey::<$h>::new(rsa_key()?)
                    .verify(message, &s)
                    .map_err(|_| bad())
            }};
        }
        let res = match dss.scheme {
            SignatureScheme::RSA_PKCS1_SHA256 => pkcs1!(Sha256),
            SignatureScheme::RSA_PKCS1_SHA384 => pkcs1!(Sha384),
            SignatureScheme::RSA_PKCS1_SHA512 => pkcs1!(Sha512),
            SignatureScheme::RSA_PSS_SHA256 => pss!(Sha256),
            SignatureScheme::RSA_PSS_SHA384 => pss!(Sha384),
            SignatureScheme::RSA_PSS_SHA512 => pss!(Sha512),
            _ => {
                let algs = &self.provider.signature_verification_algorithms;
                return if tls13 {
                    rustls::crypto::verify_tls13_signature(message, cert, dss, algs)
                } else {
                    rustls::crypto::verify_tls12_signature(message, cert, dss, algs)
                };
            }
        };
        res.map(|_| HandshakeSignatureValid::assertion())
    }
}

impl ClientCertVerifier for AnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.verify(message, cert, dss, false)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.verify(message, cert, dss, true)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
