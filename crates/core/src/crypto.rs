//! Encryption at rest (UNDERSTANDING.md §"Encryption at rest").
//!
//! One master password per journal → Argon2id → master key; blake3-derived
//! subkeys key the SQLCipher database and wrap per-blob content keys. Blobs
//! are sealed *before* ingest with chunked XChaCha20-Poly1305 (STREAM nonces),
//! so the blob store, the hashes, and the sync layer only ever see ciphertext.

use anyhow::{bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::TryRngCore;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::event::BlobCrypto;
use crate::journal::SECRET_LEN;

pub const KEY_LEN: usize = 32;
pub const SALT_LEN: usize = 16;
/// XChaCha nonce (24) = base (19) ‖ chunk counter u32 BE (4) ‖ final flag (1).
pub const NONCE_BASE_LEN: usize = 19;
pub const CHUNK_LEN: usize = 64 * 1024;
pub const TAG_LEN: usize = 16;
const WRAP_NONCE_LEN: usize = 24;

const SALT_CONTEXT: &str = "memorious kdf salt v1";
const DB_KEY_CONTEXT: &str = "memorious db key v1";
const WRAP_KEY_CONTEXT: &str = "memorious wrap key v1";

/// Argon2id parameters, recorded per journal in keys.json for future agility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KdfParams {
    /// KiB of memory.
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        // 64 MiB / 3 passes: strong on a laptop, sub-second on a phone.
        Self { m_cost: 64 * 1024, t_cost: 3, p_cost: 1 }
    }
}

/// The Argon2id salt is a pure function of the journal secret, so every paired
/// device derives identical keys with no coordination. It is mirrored into the
/// plaintext keys.json because the secret itself lives inside the encrypted
/// database and cannot be read before unlock.
pub fn salt_from_secret(secret: &[u8; SECRET_LEN]) -> [u8; SALT_LEN] {
    let full = blake3::derive_key(SALT_CONTEXT, secret);
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&full[..SALT_LEN]);
    salt
}

/// The unlocked keys: SQLCipher database key + content-key-wrapping key.
/// The intermediate master key is zeroized inside `derive`.
#[derive(ZeroizeOnDrop)]
pub struct KeySet {
    db_key: [u8; KEY_LEN],
    kwk: [u8; KEY_LEN],
}

impl KeySet {
    pub fn derive(password: &str, salt: &[u8; SALT_LEN], params: &KdfParams) -> Result<Self> {
        let argon = argon2::Argon2::new(
            argon2::Algorithm::Argon2id,
            argon2::Version::V0x13,
            argon2::Params::new(params.m_cost, params.t_cost, params.p_cost, Some(KEY_LEN))
                .map_err(|e| anyhow::anyhow!("bad kdf params: {e}"))?,
        );
        let mut mk = [0u8; KEY_LEN];
        argon
            .hash_password_into(password.as_bytes(), salt, &mut mk)
            .map_err(|e| anyhow::anyhow!("argon2: {e}"))?;
        let set = Self {
            db_key: blake3::derive_key(DB_KEY_CONTEXT, &mk),
            kwk: blake3::derive_key(WRAP_KEY_CONTEXT, &mk),
        };
        mk.zeroize();
        Ok(set)
    }

    /// Raw-key form for `PRAGMA key = "x'…'"` — the password stretching already
    /// happened here; SQLCipher's own KDF must not run again.
    pub fn db_key_hex(&self) -> String {
        data_encoding::HEXLOWER.encode(&self.db_key)
    }

    /// Wrap a fresh content key into the base64 form carried by capture events.
    pub fn wrap(&self, ck: &[u8; KEY_LEN], nonce_base: &[u8; NONCE_BASE_LEN]) -> Result<BlobCrypto> {
        let cipher = XChaCha20Poly1305::new((&self.kwk).into());
        let mut nonce = [0u8; WRAP_NONCE_LEN];
        rand::rngs::OsRng.try_fill_bytes(&mut nonce)?;
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce), ck.as_slice())
            .map_err(|_| anyhow::anyhow!("wrap content key"))?;
        let mut wrapped = nonce.to_vec();
        wrapped.extend_from_slice(&ct);
        Ok(BlobCrypto {
            ck: data_encoding::BASE64.encode(&wrapped),
            nonce: data_encoding::BASE64.encode(nonce_base),
        })
    }

    /// Recover (content key, nonce base) from a capture event's envelope.
    /// Fails authentication if this device's master password differs from the
    /// one the capturing device used — the only cross-device password check.
    pub fn unwrap(&self, crypto: &BlobCrypto) -> Result<([u8; KEY_LEN], [u8; NONCE_BASE_LEN])> {
        let wrapped = data_encoding::BASE64
            .decode(crypto.ck.as_bytes())
            .context("blob crypto: ck base64")?;
        if wrapped.len() != WRAP_NONCE_LEN + KEY_LEN + TAG_LEN {
            bail!("blob crypto: wrapped ck has wrong length");
        }
        let cipher = XChaCha20Poly1305::new((&self.kwk).into());
        let ck_vec = cipher
            .decrypt(XNonce::from_slice(&wrapped[..WRAP_NONCE_LEN]), &wrapped[WRAP_NONCE_LEN..])
            .map_err(|_| anyhow::anyhow!("content key won't unwrap — wrong master password?"))?;
        let ck: [u8; KEY_LEN] = ck_vec.as_slice().try_into().expect("32-byte ck");
        let nb_vec = data_encoding::BASE64
            .decode(crypto.nonce.as_bytes())
            .context("blob crypto: nonce base64")?;
        let nonce_base: [u8; NONCE_BASE_LEN] = nb_vec
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("blob crypto: nonce base has wrong length"))?;
        Ok((ck, nonce_base))
    }
}

/// A sealed blob ready for ingest: ciphertext plus the material to wrap.
pub struct Sealed {
    pub ciphertext: Vec<u8>,
    pub ck: [u8; KEY_LEN],
    pub nonce_base: [u8; NONCE_BASE_LEN],
}

impl Drop for Sealed {
    fn drop(&mut self) {
        self.ck.zeroize();
    }
}

fn chunk_nonce(nonce_base: &[u8; NONCE_BASE_LEN], index: u32, is_final: bool) -> [u8; 24] {
    let mut nonce = [0u8; 24];
    nonce[..NONCE_BASE_LEN].copy_from_slice(nonce_base);
    nonce[NONCE_BASE_LEN..NONCE_BASE_LEN + 4].copy_from_slice(&index.to_be_bytes());
    nonce[23] = is_final as u8;
    nonce
}

/// Seal plaintext under a fresh random content key. 64 KiB chunks, each with
/// its own tag; the counter + final-flag nonce (STREAM) makes reordering,
/// truncation, and cross-blob splicing all fail authentication.
pub fn seal(plaintext: &[u8]) -> Result<Sealed> {
    let mut ck = [0u8; KEY_LEN];
    rand::rngs::OsRng.try_fill_bytes(&mut ck)?;
    let mut nonce_base = [0u8; NONCE_BASE_LEN];
    rand::rngs::OsRng.try_fill_bytes(&mut nonce_base)?;

    let cipher = XChaCha20Poly1305::new((&ck).into());
    let n_chunks = plaintext.len().div_ceil(CHUNK_LEN).max(1);
    if n_chunks > u32::MAX as usize {
        bail!("blob too large");
    }
    let mut ciphertext = Vec::with_capacity(plaintext.len() + n_chunks * TAG_LEN);
    for i in 0..n_chunks {
        let chunk = &plaintext[i * CHUNK_LEN..plaintext.len().min((i + 1) * CHUNK_LEN)];
        let nonce = chunk_nonce(&nonce_base, i as u32, i == n_chunks - 1);
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce), chunk)
            .map_err(|_| anyhow::anyhow!("seal chunk {i}"))?;
        ciphertext.extend_from_slice(&ct);
    }
    Ok(Sealed { ciphertext, ck, nonce_base })
}

/// Open a sealed blob. Any bit flip, chunk swap, or truncation errors out.
pub fn open(
    ciphertext: &[u8],
    ck: &[u8; KEY_LEN],
    nonce_base: &[u8; NONCE_BASE_LEN],
) -> Result<Vec<u8>> {
    const CT_CHUNK: usize = CHUNK_LEN + TAG_LEN;
    if ciphertext.len() < TAG_LEN {
        bail!("ciphertext shorter than one tag");
    }
    let full = ciphertext.len() / CT_CHUNK;
    let rem = ciphertext.len() % CT_CHUNK;
    if rem > 0 && rem < TAG_LEN {
        bail!("ciphertext has a malformed final chunk");
    }
    let n_chunks = full + (rem > 0) as usize;
    let cipher = XChaCha20Poly1305::new(ck.into());
    let mut out = Vec::with_capacity(ciphertext.len());
    for i in 0..n_chunks {
        let ct = &ciphertext[i * CT_CHUNK..ciphertext.len().min((i + 1) * CT_CHUNK)];
        let nonce = chunk_nonce(nonce_base, i as u32, i == n_chunks - 1);
        let chunk = cipher
            .decrypt(XNonce::from_slice(&nonce), ct)
            .map_err(|_| anyhow::anyhow!("blob chunk {i} fails authentication"))?;
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_params() -> KdfParams {
        // Keep KDF tests fast; production uses Default.
        KdfParams { m_cost: 8, t_cost: 1, p_cost: 1 }
    }

    #[test]
    fn kdf_is_deterministic_and_input_sensitive() {
        let salt = salt_from_secret(&[9u8; SECRET_LEN]);
        let a = KeySet::derive("hunter2", &salt, &tiny_params()).unwrap();
        let b = KeySet::derive("hunter2", &salt, &tiny_params()).unwrap();
        let c = KeySet::derive("hunter3", &salt, &tiny_params()).unwrap();
        let other_salt = salt_from_secret(&[10u8; SECRET_LEN]);
        let d = KeySet::derive("hunter2", &other_salt, &tiny_params()).unwrap();
        assert_eq!(a.db_key_hex(), b.db_key_hex());
        assert_ne!(a.db_key_hex(), c.db_key_hex());
        assert_ne!(a.db_key_hex(), d.db_key_hex());
        // db key and wrapping key are independent subkeys
        assert_ne!(a.db_key, a.kwk);
    }

    #[test]
    fn seal_open_round_trips_across_chunk_boundaries() {
        for len in [0, 1, 100, CHUNK_LEN - 1, CHUNK_LEN, CHUNK_LEN + 1, 3 * CHUNK_LEN + 500] {
            let plaintext: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let sealed = seal(&plaintext).unwrap();
            assert_ne!(sealed.ciphertext, plaintext);
            let expected_ct = plaintext.len() + plaintext.len().div_ceil(CHUNK_LEN).max(1) * TAG_LEN;
            assert_eq!(sealed.ciphertext.len(), expected_ct, "len {len}");
            let back = open(&sealed.ciphertext, &sealed.ck, &sealed.nonce_base).unwrap();
            assert_eq!(back, plaintext, "len {len}");
        }
    }

    #[test]
    fn tampering_fails_authentication() {
        let plaintext: Vec<u8> = (0..2 * CHUNK_LEN + 77).map(|i| (i % 256) as u8).collect();
        let sealed = seal(&plaintext).unwrap();
        let (ck, nb) = (&sealed.ck, &sealed.nonce_base);

        // bit flip
        let mut flipped = sealed.ciphertext.clone();
        flipped[10] ^= 1;
        assert!(open(&flipped, ck, nb).is_err());

        // chunk swap (reordering)
        const CT: usize = CHUNK_LEN + TAG_LEN;
        let mut swapped = sealed.ciphertext.clone();
        let (a, rest) = swapped.split_at_mut(CT);
        a.swap_with_slice(&mut rest[..CT]);
        assert!(open(&swapped, ck, nb).is_err());

        // truncation to a clean chunk boundary (final flag catches it)
        assert!(open(&sealed.ciphertext[..CT], ck, nb).is_err());

        // wrong key
        let other = seal(b"x").unwrap();
        assert!(open(&sealed.ciphertext, &other.ck, nb).is_err());
    }

    #[test]
    fn wrap_unwrap_round_trips_and_wrong_password_fails() {
        let salt = salt_from_secret(&[1u8; SECRET_LEN]);
        let keys = KeySet::derive("right", &salt, &tiny_params()).unwrap();
        let sealed = seal(b"media bytes").unwrap();
        let crypto = keys.wrap(&sealed.ck, &sealed.nonce_base).unwrap();
        let (ck, nb) = keys.unwrap(&crypto).unwrap();
        assert_eq!(ck, sealed.ck);
        assert_eq!(nb, sealed.nonce_base);

        let wrong = KeySet::derive("wrong", &salt, &tiny_params()).unwrap();
        let err = wrong.unwrap(&crypto).unwrap_err();
        assert!(err.to_string().contains("wrong master password"));
    }

    #[test]
    fn wrap_is_randomized() {
        let salt = salt_from_secret(&[2u8; SECRET_LEN]);
        let keys = KeySet::derive("pw", &salt, &tiny_params()).unwrap();
        let sealed = seal(b"same").unwrap();
        let a = keys.wrap(&sealed.ck, &sealed.nonce_base).unwrap();
        let b = keys.wrap(&sealed.ck, &sealed.nonce_base).unwrap();
        assert_ne!(a.ck, b.ck, "fresh wrap nonce every time");
    }
}
