//! GameStream PIN pairing (Gen 7, SHA-256), four plain-HTTP phases plus the final
//! `pairchallenge` over HTTPS with the freshly pinned certificate:
//!
//! 1. `getservercert`: client salt + its cert. Host learns the PIN (fixed or submitted
//!    out of band), derives `key = SHA-256(salt || pin)[..16]`, returns its own cert.
//! 2. `clientchallenge`: `AES-ECB(key, challenge)`; host answers
//!    `AES-ECB(key, SHA-256(challenge || host_cert_sig || server_secret) || server_challenge)`.
//! 3. `serverchallengeresp`: `AES-ECB(key, client_hash)`; host stores it and returns
//!    `server_secret || RSA-SHA256-sign(server_secret)`.
//! 4. `clientpairingsecret`: `client_secret || sig`; host checks
//!    `SHA-256(server_challenge || client_cert_sig || client_secret) == client_hash` and the
//!    signature, then pins the client cert.

use crate::crypto;
use crate::identity::{Identity, PairedStore};
use anyhow::{Context, Result, anyhow, bail};
use rsa::RsaPublicKey;
use rsa::pkcs8::DecodePublicKey;
use sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const PIN_WAIT: Duration = Duration::from_secs(90);
const SESSION_TTL: Duration = Duration::from_secs(300);
/// Ceremonies kept at once; each holds a client certificate, so the map must not grow
/// with every request an unpaired device sends.
const MAX_CEREMONIES: usize = 8;

/// Where the PIN comes from: a fixed PIN from configuration, or one submitted while a
/// client waits in phase 1 (`GameStreamHost::submit_pin`).
pub struct PinGate {
    fixed: Option<String>,
    slot: Mutex<PinSlot>,
    cv: Condvar,
}

#[derive(Default)]
struct PinSlot {
    waiting: usize,
    pin: Option<String>,
}

impl PinGate {
    pub fn new(fixed: Option<String>) -> Self {
        PinGate { fixed, slot: Mutex::new(PinSlot::default()), cv: Condvar::new() }
    }

    /// Deliver a PIN to a waiting phase-1 request. Returns false when nobody waits.
    pub fn submit(&self, pin: &str) -> bool {
        let mut s = self.slot.lock().unwrap();
        if s.waiting == 0 {
            return false;
        }
        s.pin = Some(pin.trim().to_string());
        self.cv.notify_all();
        true
    }

    pub fn is_waiting(&self) -> bool {
        self.slot.lock().unwrap().waiting > 0
    }

    fn take(&self) -> Option<String> {
        if let Some(p) = &self.fixed {
            return Some(p.clone());
        }
        let mut s = self.slot.lock().unwrap();
        if s.waiting > 0 {
            // One ceremony at a time: a second one would race for the same PIN.
            return None;
        }
        s.waiting += 1;
        s.pin = None;
        let deadline = Instant::now() + PIN_WAIT;
        let pin = loop {
            if let Some(p) = s.pin.take() {
                break Some(p);
            }
            let now = Instant::now();
            if now >= deadline {
                break None;
            }
            s = self.cv.wait_timeout(s, deadline - now).unwrap().0;
        };
        s.waiting -= 1;
        pin
    }
}

struct Ceremony {
    peer_ip: IpAddr,
    started: Instant,
    aes_key: [u8; 16],
    client_cert_der: Vec<u8>,
    client_cert_sig: Vec<u8>,
    client_pubkey: RsaPublicKey,
    server_secret: [u8; 16],
    server_challenge: [u8; 16],
    client_hash: Option<Vec<u8>>,
}

pub struct Pairing {
    pub pin: PinGate,
    ceremonies: Mutex<HashMap<String, Ceremony>>,
}

impl Pairing {
    pub fn new(fixed_pin: Option<String>) -> Self {
        Pairing { pin: PinGate::new(fixed_pin), ceremonies: Mutex::new(HashMap::new()) }
    }

    pub fn getservercert(
        &self,
        id: &Identity,
        uniqueid: &str,
        salt_hex: &str,
        clientcert_hex: &str,
        peer_ip: IpAddr,
    ) -> Result<String> {
        let salt: [u8; 16] = hex::decode(salt_hex)
            .context("salt hex")?
            .get(..16)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| anyhow!("salt too short"))?;
        let pem = hex::decode(clientcert_hex).context("clientcert hex")?;
        let (der, sig, pubkey) = parse_client_cert(&pem)?;
        tracing::info!(
            uniqueid,
            %peer_ip,
            "pairing requested — waiting for the PIN shown on the client"
        );
        let pin = self.pin.take().ok_or_else(|| anyhow!("no PIN supplied"))?;
        let mut map = self.ceremonies.lock().unwrap();
        map.retain(|_, c| c.started.elapsed() < SESSION_TTL);
        while map.len() >= MAX_CEREMONIES {
            let Some(oldest) = map.iter().min_by_key(|(_, c)| c.started).map(|(k, _)| k.clone()) else { break };
            map.remove(&oldest);
        }
        map.insert(
            uniqueid.to_string(),
            Ceremony {
                peer_ip,
                started: Instant::now(),
                aes_key: crypto::pin_key(&salt, &pin),
                client_cert_der: der,
                client_cert_sig: sig,
                client_pubkey: pubkey,
                server_secret: crypto::random_bytes(),
                server_challenge: crypto::random_bytes(),
                client_hash: None,
            },
        );
        Ok(pair_xml(true, &format!("<plaincert>{}</plaincert>", hex::encode(id.cert_pem.as_bytes()))))
    }

    pub fn clientchallenge(&self, id: &Identity, uniqueid: &str, v: &str, ip: IpAddr) -> Result<String> {
        let mut map = self.ceremonies.lock().unwrap();
        let c = ceremony(&mut map, uniqueid, ip)?;
        let challenge = crypto::ecb_decrypt(&c.aes_key, &hex::decode(v).context("hex")?);
        if challenge.len() < 16 {
            bail!("short client challenge");
        }
        let hash = crypto::sha256(&[&challenge[..16], &id.cert_signature, &c.server_secret]);
        let mut plain = hash.to_vec();
        plain.extend_from_slice(&c.server_challenge);
        let resp = crypto::ecb_encrypt(&c.aes_key, &plain);
        Ok(pair_xml(true, &format!("<challengeresponse>{}</challengeresponse>", hex::encode(resp))))
    }

    pub fn serverchallengeresp(&self, id: &Identity, uniqueid: &str, v: &str, ip: IpAddr) -> Result<String> {
        let mut map = self.ceremonies.lock().unwrap();
        let c = ceremony(&mut map, uniqueid, ip)?;
        if c.client_hash.is_some() {
            bail!("serverchallengeresp repeated");
        }
        let hash = crypto::ecb_decrypt(&c.aes_key, &hex::decode(v).context("hex")?);
        if hash.len() < 32 {
            bail!("short challenge response");
        }
        c.client_hash = Some(hash[..32].to_vec());
        let sig = id.signing_key.sign(&c.server_secret).to_vec();
        let mut secret = c.server_secret.to_vec();
        secret.extend_from_slice(&sig);
        Ok(pair_xml(true, &format!("<pairingsecret>{}</pairingsecret>", hex::encode(secret))))
    }

    pub fn clientpairingsecret(&self, uniqueid: &str, v: &str, ip: IpAddr, store: &PairedStore) -> Result<String> {
        let mut map = self.ceremonies.lock().unwrap();
        let c = ceremony(&mut map, uniqueid, ip)?;
        let data = hex::decode(v).context("hex")?;
        if data.len() < 16 {
            bail!("short pairing secret");
        }
        let (secret, sig) = data.split_at(16);
        let expected = crypto::sha256(&[&c.server_challenge, &c.client_cert_sig, secret]);
        let hash_ok = c.client_hash.as_deref().is_some_and(|h| crypto::ct_eq(h, &expected));
        let sig_ok = rsa::pkcs1v15::Signature::try_from(sig)
            .ok()
            .is_some_and(|s| {
                rsa::pkcs1v15::VerifyingKey::<Sha256>::new(c.client_pubkey.clone()).verify(secret, &s).is_ok()
            });
        let der = c.client_cert_der.clone();
        map.remove(uniqueid);
        if hash_ok && sig_ok {
            store.add(&der);
            tracing::info!(uniqueid, "pairing complete — client certificate pinned");
            Ok(pair_xml(true, ""))
        } else {
            tracing::warn!(uniqueid, hash_ok, sig_ok, "pairing failed (wrong PIN?)");
            Ok(pair_xml(false, ""))
        }
    }

    /// `/unpair` from a client whose ceremony failed: drop it.
    pub fn abandon(&self, uniqueid: &str, ip: IpAddr) {
        let mut map = self.ceremonies.lock().unwrap();
        if map.get(uniqueid).is_some_and(|c| c.peer_ip == ip) {
            map.remove(uniqueid);
        }
    }
}

fn ceremony<'a>(map: &'a mut HashMap<String, Ceremony>, uniqueid: &str, ip: IpAddr) -> Result<&'a mut Ceremony> {
    let c = map.get_mut(uniqueid).ok_or_else(|| anyhow!("no pairing in progress"))?;
    if c.peer_ip != ip {
        bail!("pairing step from a different address");
    }
    Ok(c)
}

fn parse_client_cert(pem: &[u8]) -> Result<(Vec<u8>, Vec<u8>, RsaPublicKey)> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(pem).map_err(|e| anyhow!("client cert pem: {e}"))?;
    let der = pem.contents.clone();
    let x509 = pem.parse_x509().context("client cert x509")?;
    let sig = x509.signature_value.data.to_vec();
    let key = RsaPublicKey::from_public_key_der(x509.public_key().raw).context("client RSA key")?;
    Ok((der, sig, key))
}

pub fn pair_xml(paired: bool, inner: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><paired>{}</paired>{inner}</root>\n",
        u8::from(paired)
    )
}
