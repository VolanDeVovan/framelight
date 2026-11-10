//! Host identity (RSA-2048 self-signed cert, persisted) and the paired-client store.
//!
//! State dir layout:
//! - `host-key.pem`  PKCS#8 RSA-2048 private key (0600)
//! - `host-cert.pem` self-signed X.509; Moonlight pins it and pairing binds its signature
//! - `uniqueid`      16 hex chars, stable across runs
//! - `paired-clients` one hex-encoded client certificate DER per line

use anyhow::{Context, Result, anyhow};
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use sha2::Sha256;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Identity {
    pub cert_pem: String,
    pub cert_der: Vec<u8>,
    pub key_pkcs8_der: Vec<u8>,
    /// X.509 `signatureValue`, bound into the pairing hashes.
    pub cert_signature: Vec<u8>,
    pub signing_key: SigningKey<Sha256>,
    pub uniqueid: String,
}

impl Identity {
    pub fn load_or_create(dir: &Path) -> Result<Identity> {
        fs::create_dir_all(dir).with_context(|| format!("create state dir {}", dir.display()))?;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        let key_path = dir.join("host-key.pem");
        let cert_path = dir.join("host-cert.pem");
        let (cert_pem, key_pem) = match (fs::read_to_string(&cert_path), fs::read_to_string(&key_path)) {
            (Ok(c), Ok(k)) if !c.trim().is_empty() && !k.trim().is_empty() => (c, k),
            _ => {
                let (c, k) = generate_cert()?;
                write_private(&key_path, k.as_bytes())?;
                write_private(&cert_path, c.as_bytes())?;
                tracing::info!(path = %cert_path.display(), "generated host certificate (RSA-2048)");
                (c, k)
            }
        };
        let priv_key = RsaPrivateKey::from_pkcs8_pem(&key_pem).context("parse host key")?;
        let key_pkcs8_der = priv_key.to_pkcs8_der().context("encode host key")?.as_bytes().to_vec();
        let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
            .map_err(|e| anyhow!("parse host cert pem: {e}"))?;
        let cert_der = pem.contents.clone();
        let x509 = pem.parse_x509().context("parse host cert")?;
        let cert_signature = x509.signature_value.data.to_vec();

        let id_path = dir.join("uniqueid");
        let uniqueid = match fs::read_to_string(&id_path) {
            Ok(s) if s.trim().len() == 16 => s.trim().to_string(),
            _ => {
                let id = hex::encode_upper(crate::crypto::random_bytes::<8>());
                write_private(&id_path, id.as_bytes())?;
                id
            }
        };
        Ok(Identity {
            cert_pem,
            cert_der,
            key_pkcs8_der,
            cert_signature,
            signing_key: SigningKey::<Sha256>::new(priv_key),
            uniqueid,
        })
    }
}

fn generate_cert() -> Result<(String, String)> {
    // rcgen cannot generate RSA keys; mint one with `rsa` and let rcgen self-sign with it.
    let key = RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).context("generate RSA key")?;
    let key_pem = key.to_pkcs8_pem(LineEnding::LF).context("encode key")?.to_string();
    let kp = rcgen::KeyPair::from_pkcs8_pem_and_sign_algo(&key_pem, &rcgen::PKCS_RSA_SHA256)
        .context("load key into rcgen")?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).context("cert params")?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "framelight");
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2054, 1, 1);
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&crate::crypto::random_bytes::<8>()));
    let cert = params.self_signed(&kp).context("self-sign host cert")?;
    Ok((cert.pem(), key_pem))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("write {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Pinned client certificates, persisted after every change.
pub struct PairedStore {
    path: PathBuf,
    certs: Mutex<Vec<Vec<u8>>>,
}

impl PairedStore {
    pub fn load(dir: &Path) -> PairedStore {
        let path = dir.join("paired-clients");
        let certs: Vec<Vec<u8>> = fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| hex::decode(l.trim()).ok())
            .filter(|d| !d.is_empty())
            .collect();
        if !certs.is_empty() {
            tracing::info!(clients = certs.len(), "loaded paired clients");
        }
        PairedStore { path, certs: Mutex::new(certs) }
    }

    pub fn is_paired(&self, der: &[u8]) -> bool {
        self.certs.lock().unwrap().iter().any(|c| c == der)
    }

    pub fn add(&self, der: &[u8]) {
        let mut certs = self.certs.lock().unwrap();
        if certs.iter().any(|c| c == der) {
            return;
        }
        certs.push(der.to_vec());
        let body: String = certs.iter().map(|c| hex::encode(c) + "\n").collect();
        if let Err(e) = write_private(&self.path, body.as_bytes()) {
            tracing::warn!(error = %format!("{e:#}"), "could not persist paired clients");
        }
    }

    pub fn count(&self) -> usize {
        self.certs.lock().unwrap().len()
    }
}
