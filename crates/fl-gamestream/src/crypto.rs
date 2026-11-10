//! Small crypto helpers: pairing AES-128-ECB, SHA-256, the AES-128-GCM used by the
//! control stream, and the AES-128-CBC used by encrypted audio.

use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use aes_gcm::aead::AeadInPlace;
use aes_gcm::aead::consts::{U12, U16};
use aes_gcm::{AesGcm, Nonce, Tag};
use rand::RngCore;
use sha2::{Digest, Sha256};

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Constant-time equality (length is not secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Pairing AES key: `SHA-256(salt || pin)[..16]` (Gen 7+ hosts use SHA-256).
pub fn pin_key(salt: &[u8; 16], pin: &str) -> [u8; 16] {
    let d = sha256(&[salt, pin.as_bytes()]);
    let mut k = [0u8; 16];
    k.copy_from_slice(&d[..16]);
    k
}

/// AES-128-ECB without padding; input is zero-extended to a block multiple.
pub fn ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = data.to_vec();
    out.resize(data.len().div_ceil(16) * 16, 0);
    for chunk in out.chunks_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }
    out
}

/// AES-128-ECB without padding; trailing partial blocks are dropped.
pub fn ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    out
}

/// AES-128-GCM seal with a 12- or 16-byte nonce. Returns `(ciphertext, tag)`.
pub fn gcm_seal(key: &[u8; 16], nonce: &[u8], pt: &[u8]) -> (Vec<u8>, [u8; 16]) {
    let mut buf = pt.to_vec();
    let tag: Tag = match nonce.len() {
        12 => AesGcm::<Aes128, U12>::new(GenericArray::from_slice(key))
            .encrypt_in_place_detached(Nonce::<U12>::from_slice(nonce), b"", &mut buf)
            .expect("GCM seal"),
        16 => AesGcm::<Aes128, U16>::new(GenericArray::from_slice(key))
            .encrypt_in_place_detached(Nonce::<U16>::from_slice(nonce), b"", &mut buf)
            .expect("GCM seal"),
        n => panic!("unsupported GCM nonce length {n}"),
    };
    (buf, tag.into())
}

/// AES-128-GCM open with a 12- or 16-byte nonce. `None` unless the tag authenticates.
pub fn gcm_open(key: &[u8; 16], nonce: &[u8], ct: &[u8], tag: &[u8]) -> Option<Vec<u8>> {
    if tag.len() != 16 {
        return None;
    }
    let mut buf = ct.to_vec();
    let tag = Tag::from_slice(tag);
    let ok = match nonce.len() {
        12 => AesGcm::<Aes128, U12>::new(GenericArray::from_slice(key))
            .decrypt_in_place_detached(Nonce::<U12>::from_slice(nonce), b"", &mut buf, tag)
            .is_ok(),
        16 => AesGcm::<Aes128, U16>::new(GenericArray::from_slice(key))
            .decrypt_in_place_detached(Nonce::<U16>::from_slice(nonce), b"", &mut buf, tag)
            .is_ok(),
        _ => false,
    };
    ok.then_some(buf)
}

/// AES-128-CBC with PKCS#7 padding (GameStream audio encryption).
pub fn cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], pt: &[u8]) -> Vec<u8> {
    use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
    cbc::Encryptor::<Aes128>::new(key.into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(pt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcm_roundtrip_both_nonce_sizes() {
        let key = [7u8; 16];
        for n in [12usize, 16] {
            let nonce = vec![3u8; n];
            let (ct, tag) = gcm_seal(&key, &nonce, b"hello control");
            assert_eq!(gcm_open(&key, &nonce, &ct, &tag).unwrap(), b"hello control");
            let mut bad = tag;
            bad[0] ^= 1;
            assert!(gcm_open(&key, &nonce, &ct, &bad).is_none());
        }
    }

    #[test]
    fn ecb_roundtrip() {
        let key = [9u8; 16];
        let ct = ecb_encrypt(&key, &[1u8; 32]);
        assert_eq!(ecb_decrypt(&key, &ct), vec![1u8; 32]);
    }

    #[test]
    fn cbc_pads_to_block() {
        assert_eq!(cbc_encrypt(&[0; 16], &[0; 16], &[1]).len(), 16);
        assert_eq!(cbc_encrypt(&[0; 16], &[0; 16], &[1; 16]).len(), 32);
    }
}
