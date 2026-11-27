use aes::cipher::BlockEncrypt;
use aes::{Aes256, Block as AesBlock};
use aes_gcm::{Aes256Gcm, Nonce as AesGcmNonce, Tag as AesGcmTag};
use aes_gcm_siv::{Aes256GcmSiv, Nonce as AesGcmSivNonce, Tag as AesGcmSivTag};
use argon2::{Algorithm, Argon2, Params, Version};
use base64ct::{Base64UrlUnpadded, Encoding};
use blake2::{Blake2b512, Digest};
use chacha20poly1305::{
    ChaCha20Poly1305, Nonce, Tag, XChaCha20Poly1305, XNonce,
    aead::{Aead, AeadInPlace, KeyInit, OsRng, Payload},
};
use std::fmt;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// OWASP / RFC 9106 recommended Argon2id memory cost for disk encryption (64 MiB = 65,536 KiB).
pub const ARGON2_M_COST_KIB: u32 = 65_536;
/// OWASP / RFC 9106 recommended Argon2id time cost (3 iterations).
pub const ARGON2_T_COST: u32 = 3;
/// OWASP / RFC 9106 recommended Argon2id parallelism degree (4 lanes).
pub const ARGON2_P_COST: u32 = 4;

/// Domain separation label for deriving the 256-bit file/chunk payload `XChaCha20-Poly1305` subkey.
pub const KDF_CONTEXT_PAYLOAD: &[u8] = b"oifs.kdf.v2.payload.xchacha20poly1305";
/// Domain separation label for deriving the 256-bit filename SIV `BLAKE2b-512` MAC subkey.
pub const KDF_CONTEXT_FILENAME_SIV: &[u8] = b"oifs.kdf.v2.filename.siv.blake2b";
/// Domain separation label for deriving the 256-bit filename `XChaCha20-Poly1305` AEAD subkey.
pub const KDF_CONTEXT_FILENAME_CIPHER: &[u8] = b"oifs.kdf.v2.filename.aead.xchacha20poly1305";
/// Domain separation label for deriving the 256-bit file/chunk payload C2SP `XAES-256-GCM` subkey.
pub const KDF_CONTEXT_PAYLOAD_XAES256GCM: &[u8] = b"oifs.kdf.v3.payload.xaes256gcm";
/// Domain separation label for deriving the 256-bit filename RFC 8452 `AES-256-GCM-SIV` subkey.
pub const KDF_CONTEXT_FILENAME_AES256GCMSIV: &[u8] = b"oifs.kdf.v3.filename.aes256gcmsiv";
/// Domain separation label for deriving the 256-bit IPC challenge-response authentication subkey.
pub const KDF_CONTEXT_IPC_AUTH: &[u8] = b"oifs.kdf.v2.ipc.auth.blake2b";
/// Domain separation label for deriving the 256-bit `SuperBlock::header_mac` BLAKE2b subkey.
pub const KDF_CONTEXT_SUPERBLOCK_MAC: &[u8] = b"oifs.kdf.v3.superblock.mac.blake2b";

/// Derive a cryptographically isolated 256-bit subkey from a 256-bit master key and
/// a domain-separation `context` label using `BLAKE2b-512`.
///
/// Ensures that the master key is never reused directly across distinct primitives
/// (`XChaCha20-Poly1305` file payloads, `BLAKE2b` filename SIV MAC, `XChaCha20-Poly1305`
/// filename AEAD, and `BLAKE2b` IPC authentication).
#[inline]
pub fn derive_subkey(master_key: &[u8; 32], context: &[u8]) -> [u8; 32] {
    debug_assert!(context.len() <= u8::MAX as usize);
    let mut hasher = Blake2b512::new();
    hasher.update(b"OIFS-BLAKE2B-SUBKEY-KDF-V2");
    hasher.update([context.len() as u8]);
    hasher.update(context);
    hasher.update(master_key);
    let mut digest = hasher.finalize_reset();
    let mut subkey = [0u8; 32];
    subkey.copy_from_slice(&digest[..32]);
    digest.zeroize();
    subkey
}

/// Encryption key hierarchy with BLAKE2b domain-separated subkeys and automatic zeroization on drop.
#[derive(Clone)]
pub struct EncryptionKey {
    /// Master 256-bit key derived from Argon2id (or supplied directly)
    key: [u8; 32],
    /// Domain-separated 256-bit subkey for file and seekable-chunk `XChaCha20-Poly1305` payloads (v1/v2)
    payload_key: [u8; 32],
    /// Domain-separated 256-bit subkey for filename SIV `BLAKE2b-512` synthetic nonce derivation (v2)
    filename_siv_key: [u8; 32],
    /// Domain-separated 256-bit subkey for filename `XChaCha20-Poly1305` AEAD encryption (v2)
    filename_cipher_key: [u8; 32],
    /// Domain-separated 256-bit subkey for file and chunk C2SP `XAES-256-GCM` payloads (v3)
    aes_payload_key: [u8; 32],
    /// Pre-expanded AES-256 cipher schedule for C2SP `XAES-256-GCM` subkey derivation
    aes_payload_cipher: Aes256,
    /// Precomputed C2SP `XAES-256-GCM` CMAC doubling constant `K1` for `aes_payload_key`
    aes_payload_k1: [u8; 16],
    /// Domain-separated 256-bit subkey for filename RFC 8452 `AES-256-GCM-SIV` encryption (v3)
    aes_filename_key: [u8; 32],
    /// Pre-expanded AES-256-GCM-SIV cipher instance for deterministic `v3` filenames
    aes_filename_cipher: Aes256GcmSiv,
    /// Domain separation label: see [`KDF_CONTEXT_IPC_AUTH`].
    /// Domain-separated 256-bit subkey for IPC challenge-response authentication
    ipc_auth_key: [u8; 32],
    /// Domain-separated 256-bit subkey for `SuperBlock::header_mac` anti-downgrade MAC
    superblock_mac_key: [u8; 32],
}

impl Zeroize for EncryptionKey {
    fn zeroize(&mut self) {
        self.key.zeroize();
        self.payload_key.zeroize();
        self.filename_siv_key.zeroize();
        self.filename_cipher_key.zeroize();
        self.aes_payload_key.zeroize();
        self.aes_payload_cipher = Aes256::new(&[0u8; 32].into());
        self.aes_payload_k1.zeroize();
        self.aes_filename_key.zeroize();
        self.aes_filename_cipher = Aes256GcmSiv::new(&[0u8; 32].into());
        self.ipc_auth_key.zeroize();
        self.superblock_mac_key.zeroize();
    }
}

impl Drop for EncryptionKey {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl ZeroizeOnDrop for EncryptionKey {}

impl EncryptionKey {
    /// Create a new encryption key hierarchy from a 256-bit master key,
    /// immediately deriving domain-separated subkeys via `BLAKE2b-512`.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        let payload_key = derive_subkey(&bytes, KDF_CONTEXT_PAYLOAD);
        let filename_siv_key = derive_subkey(&bytes, KDF_CONTEXT_FILENAME_SIV);
        let filename_cipher_key = derive_subkey(&bytes, KDF_CONTEXT_FILENAME_CIPHER);
        let aes_payload_key = derive_subkey(&bytes, KDF_CONTEXT_PAYLOAD_XAES256GCM);
        let aes_payload_cipher = Aes256::new((&aes_payload_key).into());
        let aes_payload_k1 = xaes256gcm_derive_k1(&aes_payload_key);
        let aes_filename_key = derive_subkey(&bytes, KDF_CONTEXT_FILENAME_AES256GCMSIV);
        let aes_filename_cipher = Aes256GcmSiv::new((&aes_filename_key).into());
        let ipc_auth_key = derive_subkey(&bytes, KDF_CONTEXT_IPC_AUTH);
        let superblock_mac_key = derive_subkey(&bytes, KDF_CONTEXT_SUPERBLOCK_MAC);
        Self {
            key: bytes,
            payload_key,
            filename_siv_key,
            filename_cipher_key,
            aes_payload_key,
            aes_payload_cipher,
            aes_payload_k1,
            aes_filename_key,
            aes_filename_cipher,
            ipc_auth_key,
            superblock_mac_key,
        }
    }

    /// Get master key bytes (internal use only)
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.key
    }

    /// Domain-separated subkey for file and chunk payload `XChaCha20-Poly1305` AEAD.
    #[inline]
    pub fn payload_key(&self) -> &[u8; 32] {
        &self.payload_key
    }

    /// Domain-separated subkey for deterministic filename SIV `BLAKE2b-512` nonce derivation.
    #[inline]
    pub fn filename_siv_key(&self) -> &[u8; 32] {
        &self.filename_siv_key
    }

    /// Domain-separated subkey for deterministic filename `XChaCha20-Poly1305` AEAD.
    #[inline]
    pub fn filename_cipher_key(&self) -> &[u8; 32] {
        &self.filename_cipher_key
    }

    /// Domain-separated subkey for file and chunk C2SP `XAES-256-GCM` AEAD.
    #[inline]
    pub fn aes_payload_key(&self) -> &[u8; 32] {
        &self.aes_payload_key
    }

    /// Domain-separated subkey for deterministic filename RFC 8452 `AES-256-GCM-SIV` AEAD.
    #[inline]
    pub fn aes_filename_key(&self) -> &[u8; 32] {
        &self.aes_filename_key
    }

    /// Domain-separated subkey for IPC challenge-response authentication.
    #[inline]
    pub fn ipc_auth_key(&self) -> &[u8; 32] {
        &self.ipc_auth_key
    }

    /// Domain-separated subkey for `SuperBlock::header_mac` verification.
    #[inline]
    pub fn superblock_mac_key(&self) -> &[u8; 32] {
        &self.superblock_mac_key
    }
}

impl fmt::Debug for EncryptionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EncryptionKey([REDACTED])")
    }
}

/// Derive an encryption key from a password using OWASP / RFC 9106 hardened `Argon2id`
/// (`m = 64 MiB`, `t = 3`, `p = 4`) and populate its `BLAKE2b` domain-separated subkey hierarchy.
///
/// # Arguments
/// * `password` - User password/passphrase
/// * `salt` - 16-byte salt (stored in SuperBlock)
///
/// # Returns
/// 256-bit master key and domain-separated subkeys suitable for `XChaCha20-Poly1305`
pub fn derive_key(password: &str, salt: &[u8; 16]) -> Result<EncryptionKey, EncryptionError> {
    derive_key_with_params(
        password,
        salt,
        ARGON2_M_COST_KIB,
        ARGON2_T_COST,
        ARGON2_P_COST,
    )
}

/// Derive an encryption key from a password using explicit `Argon2id` cost parameters.
pub fn derive_key_with_params(
    password: &str,
    salt: &[u8; 16],
    m_cost_kib: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<EncryptionKey, EncryptionError> {
    let params = Params::new(m_cost_kib, t_cost, p_cost, Some(32))
        .map_err(|_| EncryptionError::KeyDerivationFailed)?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    // Derive directly into a zeroized stack buffer (avoids leaving master key bytes
    // in an unzeroized `password_hash::Output` buffer).
    let mut key = [0u8; 32];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|_| EncryptionError::KeyDerivationFailed)?;
    let enc_key = EncryptionKey::from_bytes(key);
    key.zeroize();

    Ok(enc_key)
}

/// Constant-time CMAC GF(2^128) subkey `K1` derivation for C2SP `XAES-256-GCM`.
#[inline]
pub(crate) fn xaes256gcm_derive_k1(key: &[u8; 32]) -> [u8; 16] {
    let cipher = Aes256::new(key.into());
    let mut l_block = AesBlock::default();
    cipher.encrypt_block(&mut l_block);
    let mut l: [u8; 16] = l_block.into();
    let msb = l[0] >> 7;
    let mut k1 = [0u8; 16];
    let mut carry = 0u8;
    let mut i = 16usize;
    while i > 0 {
        i -= 1;
        let b = l[i];
        k1[i] = (b << 1) | carry;
        carry = b >> 7;
    }
    k1[15] ^= 0u8.wrapping_sub(msb) & 0x87;
    l.zeroize();
    k1
}

/// Derive `(Kx, Nx)` using an already-expanded `Aes256` key schedule and precomputed `k1`
/// per `c2sp.org/XAES-256-GCM`.
#[inline]
pub(crate) fn xaes256gcm_derive_subkey_with_cipher(
    cipher: &Aes256,
    k1: &[u8; 16],
    nonce: &[u8; 24],
) -> ([u8; 32], [u8; 12]) {
    let mut m1 = [0u8; 16];
    m1[0] = 0x00;
    m1[1] = 0x01;
    m1[2] = b'X';
    m1[3] = 0x00;
    m1[4..16].copy_from_slice(&nonce[0..12]);

    let mut m2 = m1;
    m2[1] = 0x02;

    for i in 0..16 {
        m1[i] ^= k1[i];
        m2[i] ^= k1[i];
    }

    let mut blocks = [AesBlock::from(m1), AesBlock::from(m2)];
    m1.zeroize();
    m2.zeroize();
    cipher.encrypt_blocks(&mut blocks);

    let mut kx = [0u8; 32];
    kx[..16].copy_from_slice(blocks[0].as_slice());
    kx[16..].copy_from_slice(blocks[1].as_slice());
    blocks[0].zeroize();
    blocks[1].zeroize();

    let mut nx = [0u8; 12];
    nx.copy_from_slice(&nonce[12..24]);
    (kx, nx)
}

/// Derive `(Kx, Nx)` from raw 256-bit key `key`, precomputed `k1`, and 192-bit `nonce`
/// per `c2sp.org/XAES-256-GCM`.
#[allow(dead_code)]
#[inline]
pub(crate) fn xaes256gcm_derive_subkey(
    key: &[u8; 32],
    k1: &[u8; 16],
    nonce: &[u8; 24],
) -> ([u8; 32], [u8; 12]) {
    let cipher = Aes256::new(key.into());
    xaes256gcm_derive_subkey_with_cipher(&cipher, k1, nonce)
}

#[inline]
fn xaes256gcm_encrypt_in_place_detached_with_cipher(
    aes_cipher: &Aes256,
    k1: &[u8; 16],
    nonce: &[u8; 24],
    aad: &[u8],
    buffer: &mut [u8],
) -> Result<[u8; 16], EncryptionError> {
    let (mut kx, nx) = xaes256gcm_derive_subkey_with_cipher(aes_cipher, k1, nonce);
    let cipher = Aes256Gcm::new((&kx).into());
    kx.zeroize();
    let tag = cipher
        .encrypt_in_place_detached(AesGcmNonce::from_slice(&nx), aad, buffer)
        .map_err(|_| EncryptionError::EncryptionFailed)?;
    let mut out = [0u8; 16];
    out.copy_from_slice(tag.as_slice());
    Ok(out)
}

#[inline]
fn xaes256gcm_decrypt_in_place_detached_with_cipher(
    aes_cipher: &Aes256,
    k1: &[u8; 16],
    nonce: &[u8; 24],
    aad: &[u8],
    buffer: &mut [u8],
    tag_bytes: &[u8; 16],
) -> Result<(), EncryptionError> {
    let (mut kx, nx) = xaes256gcm_derive_subkey_with_cipher(aes_cipher, k1, nonce);
    let cipher = Aes256Gcm::new((&kx).into());
    kx.zeroize();
    cipher
        .decrypt_in_place_detached(
            AesGcmNonce::from_slice(&nx),
            aad,
            buffer,
            AesGcmTag::from_slice(tag_bytes),
        )
        .map_err(|_| EncryptionError::DecryptionFailed)
}

#[inline]
fn xaes256gcm_encrypt_in_place_with_cipher(
    aes_cipher: &Aes256,
    k1: &[u8; 16],
    nonce: &[u8; 24],
    aad: &[u8],
    buffer: &mut Vec<u8>,
) -> Result<(), EncryptionError> {
    let (mut kx, nx) = xaes256gcm_derive_subkey_with_cipher(aes_cipher, k1, nonce);
    let cipher = Aes256Gcm::new((&kx).into());
    kx.zeroize();
    cipher
        .encrypt_in_place(AesGcmNonce::from_slice(&nx), aad, buffer)
        .map_err(|_| EncryptionError::EncryptionFailed)
}

#[inline]
fn xaes256gcm_decrypt_in_place_with_cipher(
    aes_cipher: &Aes256,
    k1: &[u8; 16],
    nonce: &[u8; 24],
    aad: &[u8],
    buffer: &mut Vec<u8>,
) -> Result<(), EncryptionError> {
    let (mut kx, nx) = xaes256gcm_derive_subkey_with_cipher(aes_cipher, k1, nonce);
    let cipher = Aes256Gcm::new((&kx).into());
    kx.zeroize();
    cipher
        .decrypt_in_place(AesGcmNonce::from_slice(&nx), aad, buffer)
        .map_err(|_| EncryptionError::DecryptionFailed)
}

#[cfg(test)]
#[inline]
fn xaes256gcm_encrypt_in_place(
    key: &[u8; 32],
    k1: &[u8; 16],
    nonce: &[u8; 24],
    aad: &[u8],
    buffer: &mut Vec<u8>,
) -> Result<(), EncryptionError> {
    let aes_cipher = Aes256::new(key.into());
    xaes256gcm_encrypt_in_place_with_cipher(&aes_cipher, k1, nonce, aad, buffer)
}

#[cfg(test)]
#[inline]
fn xaes256gcm_decrypt_in_place(
    key: &[u8; 32],
    k1: &[u8; 16],
    nonce: &[u8; 24],
    aad: &[u8],
    buffer: &mut Vec<u8>,
) -> Result<(), EncryptionError> {
    let aes_cipher = Aes256::new(key.into());
    xaes256gcm_decrypt_in_place_with_cipher(&aes_cipher, k1, nonce, aad, buffer)
}

/// Derives a unique 24-byte chunk nonce within a write batch while keeping `nonce[0..12]`
/// identical so C2SP `XAES-256-GCM` reuses one derived `AES-256-GCM` key schedule `Kx` across
/// all chunks in the batch.
#[inline]
pub fn derive_batch_chunk_nonce(batch_seed: &[u8; 24], batch_idx: u64) -> [u8; 24] {
    let mut out = *batch_seed;
    let tail = u64::from_le_bytes(out[16..24].try_into().unwrap());
    out[16..24].copy_from_slice(&(tail ^ batch_idx.wrapping_add(1)).to_le_bytes());
    out
}

/// Pre-expanded batch AEAD cipher shared across all chunks in a single Seekable 64K write,
/// eliminating per-chunk `OsRng` syscalls and repeated `AES-256` / `AES-256-GCM` key setup.
#[allow(clippy::large_enum_variant)]
pub enum PrecomputedBatchAead {
    Xaes(Aes256Gcm),
    XChaCha(XChaCha20Poly1305),
}

impl PrecomputedBatchAead {
    /// Initializes a batch AEAD cipher bound to `batch_seed[0..12]` and `scheme`.
    pub fn new(
        key: &EncryptionKey,
        batch_seed: &[u8; 24],
        scheme: u8,
    ) -> Result<Self, EncryptionError> {
        if scheme == ENCRYPTION_SCHEME_AES256_GCM_V3 {
            let (mut kx, _) = xaes256gcm_derive_subkey_with_cipher(
                &key.aes_payload_cipher,
                &key.aes_payload_k1,
                batch_seed,
            );
            let cipher = Aes256Gcm::new((&kx).into());
            kx.zeroize();
            return Ok(Self::Xaes(cipher));
        }
        let cipher = XChaCha20Poly1305::new_from_slice(key.payload_key())
            .map_err(|_| EncryptionError::InvalidKey)?;
        Ok(Self::XChaCha(cipher))
    }

    /// Encrypts `plaintext` directly into a single `[24-byte nonce | ciphertext | 16-byte tag]`
    /// buffer (pre-reserving `extra_tail_cap` bytes so subsequent block-alignment zero padding
    /// does not reallocate).
    #[inline]
    pub fn encrypt_with_nonce_prefix(
        &self,
        plaintext: &[u8],
        nonce: &[u8; 24],
        aad: &[u8],
        extra_tail_cap: usize,
    ) -> Result<Vec<u8>, EncryptionError> {
        let mut out = Vec::with_capacity(24 + plaintext.len() + 16 + extra_tail_cap);
        out.extend_from_slice(nonce);
        out.extend_from_slice(plaintext);
        match self {
            Self::Xaes(cipher) => {
                let tag = cipher
                    .encrypt_in_place_detached(
                        AesGcmNonce::from_slice(&nonce[12..24]),
                        aad,
                        &mut out[24..],
                    )
                    .map_err(|_| EncryptionError::EncryptionFailed)?;
                out.extend_from_slice(tag.as_slice());
            }
            Self::XChaCha(cipher) => {
                let tag = cipher
                    .encrypt_in_place_detached(XNonce::from_slice(nonce), aad, &mut out[24..])
                    .map_err(|_| EncryptionError::EncryptionFailed)?;
                out.extend_from_slice(tag.as_slice());
            }
        }
        Ok(out)
    }
}

/// Encrypt data using XChaCha20-Poly1305 AEAD cipher (legacy/default v2 helper)
///
/// # Arguments
/// * `plaintext` - Data to encrypt
/// * `key` - Encryption key (256-bit)
/// * `nonce` - Unique 192-bit nonce (MUST be unique per encryption)
///
/// # Returns
/// Encrypted data with 16-byte authentication tag appended
pub fn encrypt_data(
    plaintext: &[u8],
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
) -> Result<Vec<u8>, EncryptionError> {
    encrypt_data_for_scheme(plaintext, key, nonce, aad, ENCRYPTION_SCHEME_CONTEXT_BOUND)
}

/// Encrypt data using the AEAD cipher selected by `scheme` (`v3` = C2SP XAES-256-GCM,
/// `v1/v2` = XChaCha20-Poly1305).
pub fn encrypt_data_for_scheme(
    plaintext: &[u8],
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
    scheme: u8,
) -> Result<Vec<u8>, EncryptionError> {
    let mut out = Vec::with_capacity(plaintext.len() + 16);
    out.extend_from_slice(plaintext);
    encrypt_data_in_place_for_scheme(&mut out, key, nonce, aad, scheme)?;
    Ok(out)
}

/// Decrypt data using XChaCha20-Poly1305 AEAD cipher (legacy/v2 helper)
///
/// # Arguments
/// * `ciphertext` - Encrypted data with auth tag
/// * `key` - Encryption key (256-bit)
/// * `nonce` - Same 192-bit nonce used for encryption
///
/// # Returns
/// Decrypted plaintext (authentication verified)
pub fn decrypt_data(
    ciphertext: &[u8],
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
) -> Result<Vec<u8>, EncryptionError> {
    decrypt_data_for_scheme(ciphertext, key, nonce, aad, ENCRYPTION_SCHEME_CONTEXT_BOUND)
}

/// Decrypt data using the AEAD cipher selected by `scheme` (`v3` = C2SP XAES-256-GCM,
/// `v1/v2` = XChaCha20-Poly1305).
pub fn decrypt_data_for_scheme(
    ciphertext: &[u8],
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
    scheme: u8,
) -> Result<Vec<u8>, EncryptionError> {
    let mut out = ciphertext.to_vec();
    decrypt_data_in_place_for_scheme(&mut out, key, nonce, aad, scheme)?;
    Ok(out)
}

/// Encrypt data in-place using XChaCha20-Poly1305 AEAD cipher,
/// appending the 16-byte authentication tag directly to `buffer` without
/// allocating a second full-payload buffer.
pub fn encrypt_data_in_place(
    buffer: &mut Vec<u8>,
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
) -> Result<(), EncryptionError> {
    encrypt_data_in_place_for_scheme(buffer, key, nonce, aad, ENCRYPTION_SCHEME_CONTEXT_BOUND)
}

/// Encrypt data in-place using the AEAD cipher selected by `scheme`.
pub fn encrypt_data_in_place_for_scheme(
    buffer: &mut Vec<u8>,
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
    scheme: u8,
) -> Result<(), EncryptionError> {
    if scheme == ENCRYPTION_SCHEME_AES256_GCM_V3 {
        return xaes256gcm_encrypt_in_place_with_cipher(
            &key.aes_payload_cipher,
            &key.aes_payload_k1,
            nonce,
            aad,
            buffer,
        );
    }
    let cipher = XChaCha20Poly1305::new_from_slice(key.payload_key())
        .map_err(|_| EncryptionError::InvalidKey)?;
    let xnonce = XNonce::from_slice(nonce);
    cipher
        .encrypt_in_place(xnonce, aad, buffer)
        .map_err(|_| EncryptionError::EncryptionFailed)
}

/// Encrypt `plaintext` into a single `[24-byte nonce | ciphertext | 16-byte tag]`
/// allocation, avoiding an intermediate ciphertext `Vec` copy.
pub fn encrypt_with_nonce_prefix(
    plaintext: &[u8],
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
) -> Result<Vec<u8>, EncryptionError> {
    encrypt_with_nonce_prefix_for_scheme(
        plaintext,
        key,
        nonce,
        aad,
        ENCRYPTION_SCHEME_CONTEXT_BOUND,
    )
}

/// Encrypt `plaintext` into `[24-byte nonce | ciphertext | 16-byte tag]` using `scheme`.
pub fn encrypt_with_nonce_prefix_for_scheme(
    plaintext: &[u8],
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
    scheme: u8,
) -> Result<Vec<u8>, EncryptionError> {
    let mut out = Vec::with_capacity(24 + plaintext.len() + 16);
    out.extend_from_slice(nonce);
    out.extend_from_slice(plaintext);

    if scheme == ENCRYPTION_SCHEME_AES256_GCM_V3 {
        let tag = xaes256gcm_encrypt_in_place_detached_with_cipher(
            &key.aes_payload_cipher,
            &key.aes_payload_k1,
            nonce,
            aad,
            &mut out[24..],
        )?;
        out.extend_from_slice(&tag);
        return Ok(out);
    }

    let cipher = XChaCha20Poly1305::new_from_slice(key.payload_key())
        .map_err(|_| EncryptionError::InvalidKey)?;
    let xnonce = XNonce::from_slice(nonce);
    let tag = cipher
        .encrypt_in_place_detached(xnonce, aad, &mut out[24..])
        .map_err(|_| EncryptionError::EncryptionFailed)?;
    out.extend_from_slice(tag.as_slice());
    Ok(out)
}

/// Decrypt `ciphertext_with_tag` into a caller-provided slice `dst` (which must
/// have length `ciphertext_with_tag.len() - 16`) without heap allocations.
pub fn decrypt_into_slice(
    ciphertext_with_tag: &[u8],
    dst: &mut [u8],
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
) -> Result<(), EncryptionError> {
    decrypt_into_slice_for_scheme(
        ciphertext_with_tag,
        dst,
        key,
        nonce,
        aad,
        ENCRYPTION_SCHEME_CONTEXT_BOUND,
    )
}

/// Decrypt `ciphertext_with_tag` directly into `dst` using the AEAD cipher selected by `scheme`.
pub fn decrypt_into_slice_for_scheme(
    ciphertext_with_tag: &[u8],
    dst: &mut [u8],
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
    scheme: u8,
) -> Result<(), EncryptionError> {
    if ciphertext_with_tag.len() < 16 || dst.len() != ciphertext_with_tag.len() - 16 {
        return Err(EncryptionError::DecryptionFailed);
    }
    let ct_len = dst.len();
    dst.copy_from_slice(&ciphertext_with_tag[..ct_len]);
    if scheme == ENCRYPTION_SCHEME_AES256_GCM_V3 {
        let tag_bytes: &[u8; 16] = ciphertext_with_tag[ct_len..]
            .try_into()
            .map_err(|_| EncryptionError::DecryptionFailed)?;
        return xaes256gcm_decrypt_in_place_detached_with_cipher(
            &key.aes_payload_cipher,
            &key.aes_payload_k1,
            nonce,
            aad,
            dst,
            tag_bytes,
        );
    }
    let tag = Tag::from_slice(&ciphertext_with_tag[ct_len..]);
    let cipher = XChaCha20Poly1305::new_from_slice(key.payload_key())
        .map_err(|_| EncryptionError::InvalidKey)?;
    let xnonce = XNonce::from_slice(nonce);
    cipher
        .decrypt_in_place_detached(xnonce, aad, dst, tag)
        .map_err(|_| EncryptionError::DecryptionFailed)
}

/// Decrypt data in-place using XChaCha20-Poly1305 AEAD cipher, avoiding
/// a second full-payload heap allocation.
pub fn decrypt_data_in_place(
    buffer: &mut Vec<u8>,
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
) -> Result<(), EncryptionError> {
    decrypt_data_in_place_for_scheme(buffer, key, nonce, aad, ENCRYPTION_SCHEME_CONTEXT_BOUND)
}

/// Decrypt data in-place using the AEAD cipher selected by `scheme`.
pub fn decrypt_data_in_place_for_scheme(
    buffer: &mut Vec<u8>,
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
    scheme: u8,
) -> Result<(), EncryptionError> {
    if scheme == ENCRYPTION_SCHEME_AES256_GCM_V3 {
        return xaes256gcm_decrypt_in_place_with_cipher(
            &key.aes_payload_cipher,
            &key.aes_payload_k1,
            nonce,
            aad,
            buffer,
        );
    }
    let cipher = XChaCha20Poly1305::new_from_slice(key.payload_key())
        .map_err(|_| EncryptionError::InvalidKey)?;

    let xnonce = XNonce::from_slice(nonce);

    cipher
        .decrypt_in_place(xnonce, aad, buffer)
        .map_err(|_| EncryptionError::DecryptionFailed)
}

/// Generate a cryptographically secure random nonce
pub fn generate_nonce() -> [u8; 24] {
    let mut nonce = [0u8; 24];
    use chacha20poly1305::aead::rand_core::RngCore;
    OsRng.fill_bytes(&mut nonce);
    nonce
}

/// Generate a cryptographically secure random salt
pub fn generate_salt() -> [u8; 16] {
    let mut salt = [0u8; 16];
    use chacha20poly1305::aead::rand_core::RngCore;
    OsRng.fill_bytes(&mut salt);
    salt
}

/// Constant prefix for encrypted filenames
pub const FILENAME_ENC_PREFIX: &str = "_e_";

/// The legacy encryption scheme: XChaCha20-Poly1305 without associated data.
pub const ENCRYPTION_SCHEME_LEGACY: u8 = 1;

/// The context-bound XChaCha20-Poly1305 encryption scheme (`v2`).
///
/// The associated data binds the ciphertext to the file (and, for a seekable
/// chunk, to the chunk index and its flags), so a chunk cannot be moved to
/// another file, reordered or rolled back without the authentication failing.
pub const ENCRYPTION_SCHEME_CONTEXT_BOUND: u8 = 2;

/// Hardware-accelerated AEAD scheme (`v3`, default for new encrypted images):
/// - File & chunk payloads: C2SP `XAES-256-GCM` (24-byte nonce, single-pass AES-256-GCM with AAD)
/// - Deterministic filenames: RFC 8452 `AES-256-GCM-SIV` (`parent_inode` bound in nonce & AAD)
pub const ENCRYPTION_SCHEME_AES256_GCM_V3: u8 = 3;

/// Default encryption scheme version for newly created encrypted images.
pub const ENCRYPTION_SCHEME_DEFAULT: u8 = ENCRYPTION_SCHEME_AES256_GCM_V3;

/// Selectable authenticated encryption cipher suite for new encrypted OIFS images.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum EncryptionCipher {
    /// Context-bound `XChaCha20-Poly1305` for payloads & filenames (`encryption_version = 2`).
    XChaCha20Poly1305 = ENCRYPTION_SCHEME_CONTEXT_BOUND,
    /// C2SP `XAES-256-GCM` for payloads + RFC 8452 `AES-256-GCM-SIV` for filenames (`encryption_version = 3`, default).
    #[default]
    Aes256Gcm = ENCRYPTION_SCHEME_AES256_GCM_V3,
}

impl EncryptionCipher {
    /// Returns the on-disk `SuperBlock::encryption_version` corresponding to this cipher suite.
    #[inline]
    pub const fn scheme_version(self) -> u8 {
        self as u8
    }

    /// Maps an on-disk `SuperBlock::encryption_version` (`1`, `2`, or `3`) to its cipher suite.
    #[inline]
    pub const fn from_scheme_version(version: u8) -> Option<Self> {
        match version {
            ENCRYPTION_SCHEME_LEGACY | ENCRYPTION_SCHEME_CONTEXT_BOUND => {
                Some(Self::XChaCha20Poly1305)
            }
            ENCRYPTION_SCHEME_AES256_GCM_V3 => Some(Self::Aes256Gcm),
            _ => None,
        }
    }

    /// Canonical identifier string (`"aes256-gcm"` or `"xchacha20-poly1305"`).
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aes256Gcm => "aes256-gcm",
            Self::XChaCha20Poly1305 => "xchacha20-poly1305",
        }
    }

    /// Parses a user-facing cipher string (`aes256-gcm`, `xaes-256-gcm`, `xchacha20-poly1305`, ...).
    pub fn parse(s: &str) -> Result<Self, &'static str> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" | "aes" | "aes256" | "aes-256-gcm" | "aes256-gcm" | "aes_256_gcm"
            | "aes256_gcm" | "xaes" | "xaes-256-gcm" | "xaes256-gcm" | "v3" => Ok(Self::Aes256Gcm),
            "chacha" | "chacha20" | "xchacha20" | "xchacha20-poly1305" | "xchacha20poly1305"
            | "xchacha20_poly1305" | "v2" => Ok(Self::XChaCha20Poly1305),
            _ => Err("Unknown encryption cipher; expected 'aes256-gcm' or 'xchacha20-poly1305'"),
        }
    }
}

impl fmt::Display for EncryptionCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Domain separator of the associated data.
const AAD_DOMAIN: &[u8] = b"OIFS-AAD-v1";

/// The context kind of a whole-file ciphertext.
const AAD_KIND_FILE: u8 = 0x01;

/// The context kind of a seekable chunk ciphertext.
const AAD_KIND_CHUNK: u8 = 0x02;

/// The context kind of an encrypted directory filename entry.
const AAD_KIND_FILENAME: u8 = 0x03;

/// Stack-allocated associated data for a whole-file ciphertext (zero heap allocations).
#[inline]
pub fn file_aad_stack(inode_id: u64) -> [u8; 20] {
    let mut aad = [0u8; 20];
    aad[0] = AAD_KIND_FILE;
    aad[1..12].copy_from_slice(AAD_DOMAIN);
    aad[12..20].copy_from_slice(&inode_id.to_le_bytes());
    aad
}

/// Stack-allocated associated data for a seekable chunk ciphertext (zero heap allocations).
#[inline]
pub fn chunk_aad_stack(inode_id: u64, chunk_idx: u64, flags: u8) -> [u8; 29] {
    let mut aad = [0u8; 29];
    aad[0] = AAD_KIND_CHUNK;
    aad[1..12].copy_from_slice(AAD_DOMAIN);
    aad[12..20].copy_from_slice(&inode_id.to_le_bytes());
    aad[20..28].copy_from_slice(&chunk_idx.to_le_bytes());
    aad[28] = flags;
    aad
}

/// Stack-allocated associated data binding an encrypted filename to its `parent_inode`
/// inside the `XChaCha20-Poly1305` AEAD authentication tag.
#[inline]
pub fn filename_aad_stack(parent_inode: u64) -> [u8; 20] {
    let mut aad = [0u8; 20];
    aad[0] = AAD_KIND_FILENAME;
    aad[1..12].copy_from_slice(AAD_DOMAIN);
    aad[12..20].copy_from_slice(&parent_inode.to_le_bytes());
    aad
}

/// Builds the associated data that binds a whole-file ciphertext to its inode.
///
/// Without it, every file of an image shares the same master key and nothing
/// but the nonce distinguishes them, so an attacker with write access to the
/// image could swap two ciphertexts (or roll one back) without the AEAD
/// noticing.
pub fn file_aad(inode_id: u64) -> Vec<u8> {
    file_aad_stack(inode_id).to_vec()
}

/// Builds the associated data that binds a chunk ciphertext to its chunk.
pub fn chunk_aad(inode_id: u64, chunk_idx: u64, flags: u8) -> Vec<u8> {
    chunk_aad_stack(inode_id, chunk_idx, flags).to_vec()
}

/// Returns the associated data of a whole-file ciphertext for the given scheme on the stack
/// with zero heap allocations (`(buf, len)` -> `&buf[..len]`).
#[inline]
pub fn file_aad_for_scheme_stack(inode_id: u64, scheme: u8) -> ([u8; 20], usize) {
    if scheme >= ENCRYPTION_SCHEME_CONTEXT_BOUND {
        (file_aad_stack(inode_id), 20)
    } else {
        ([0u8; 20], 0)
    }
}

/// Returns the associated data of a chunk ciphertext for the given scheme on the stack
/// with zero heap allocations (`(buf, len)` -> `&buf[..len]`).
#[inline]
pub fn chunk_aad_for_scheme_stack(
    inode_id: u64,
    chunk_idx: u64,
    flags: u8,
    scheme: u8,
) -> ([u8; 29], usize) {
    if scheme >= ENCRYPTION_SCHEME_CONTEXT_BOUND {
        (chunk_aad_stack(inode_id, chunk_idx, flags), 29)
    } else {
        ([0u8; 29], 0)
    }
}

/// Returns the associated data of a whole-file ciphertext for the given scheme.
///
/// The legacy scheme uses empty associated data, so older images stay readable.
pub fn file_aad_for_scheme(inode_id: u64, scheme: u8) -> Vec<u8> {
    if scheme >= ENCRYPTION_SCHEME_CONTEXT_BOUND {
        file_aad(inode_id)
    } else {
        Vec::new()
    }
}

/// Returns the associated data of a chunk ciphertext for the given scheme.
pub fn chunk_aad_for_scheme(inode_id: u64, chunk_idx: u64, flags: u8, scheme: u8) -> Vec<u8> {
    if scheme >= ENCRYPTION_SCHEME_CONTEXT_BOUND {
        chunk_aad(inode_id, chunk_idx, flags)
    } else {
        Vec::new()
    }
}

/// Short-message scalar threshold (bytes).
///
/// On `aarch64` with `chacha20_force_neon`, `chacha20 0.9`'s NEON `gen_ks_block` computes
/// 4 ChaCha20 blocks (256 B) and discards 3 of them for every single-block call, which
/// wastes 6 of 8 blocks on short filenames (`<= 64 B`). This exact RFC 8439 + XChaCha20
/// scalar fast path computes only the exact number of 64-byte blocks needed while remaining
/// 100% byte-for-byte identical to `XChaCha20Poly1305`, preserving full NEON SIMD speed (+36%)
/// for file and chunk payloads.
const SHORT_XCHACHA_THRESHOLD: usize = 192;

#[inline(always)]
fn chacha_quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] ^= s[a];
    s[d] = s[d].rotate_left(16);

    s[c] = s[c].wrapping_add(s[d]);
    s[b] ^= s[c];
    s[b] = s[b].rotate_left(12);

    s[a] = s[a].wrapping_add(s[b]);
    s[d] ^= s[a];
    s[d] = s[d].rotate_left(8);

    s[c] = s[c].wrapping_add(s[d]);
    s[b] ^= s[c];
    s[b] = s[b].rotate_left(7);
}

#[inline(always)]
fn chacha20_rounds(s: &mut [u32; 16]) {
    for _ in 0..10 {
        chacha_quarter_round(s, 0, 4, 8, 12);
        chacha_quarter_round(s, 1, 5, 9, 13);
        chacha_quarter_round(s, 2, 6, 10, 14);
        chacha_quarter_round(s, 3, 7, 11, 15);
        chacha_quarter_round(s, 0, 5, 10, 15);
        chacha_quarter_round(s, 1, 6, 11, 12);
        chacha_quarter_round(s, 2, 7, 8, 13);
        chacha_quarter_round(s, 3, 4, 9, 14);
    }
}

#[inline]
fn hchacha20_subkey(key: &[u8; 32], nonce16: &[u8]) -> [u32; 8] {
    let mut s = [
        0x6170_7865,
        0x3320_646e,
        0x7962_2d32,
        0x6b20_6574,
        u32::from_le_bytes(key[0..4].try_into().unwrap()),
        u32::from_le_bytes(key[4..8].try_into().unwrap()),
        u32::from_le_bytes(key[8..12].try_into().unwrap()),
        u32::from_le_bytes(key[12..16].try_into().unwrap()),
        u32::from_le_bytes(key[16..20].try_into().unwrap()),
        u32::from_le_bytes(key[20..24].try_into().unwrap()),
        u32::from_le_bytes(key[24..28].try_into().unwrap()),
        u32::from_le_bytes(key[28..32].try_into().unwrap()),
        u32::from_le_bytes(nonce16[0..4].try_into().unwrap()),
        u32::from_le_bytes(nonce16[4..8].try_into().unwrap()),
        u32::from_le_bytes(nonce16[8..12].try_into().unwrap()),
        u32::from_le_bytes(nonce16[12..16].try_into().unwrap()),
    ];
    chacha20_rounds(&mut s);
    let out = [s[0], s[1], s[2], s[3], s[12], s[13], s[14], s[15]];
    s.zeroize();
    out
}

#[inline]
fn chacha20_block_words(subkey: &[u32; 8], counter: u32, nonce8: &[u8]) -> [u8; 64] {
    let mut init = [
        0x6170_7865,
        0x3320_646e,
        0x7962_2d32,
        0x6b20_6574,
        subkey[0],
        subkey[1],
        subkey[2],
        subkey[3],
        subkey[4],
        subkey[5],
        subkey[6],
        subkey[7],
        counter,
        0,
        u32::from_le_bytes(nonce8[0..4].try_into().unwrap()),
        u32::from_le_bytes(nonce8[4..8].try_into().unwrap()),
    ];
    let mut s = init;
    chacha20_rounds(&mut s);
    let mut out = [0u8; 64];
    for i in 0..16 {
        let w = s[i].wrapping_add(init[i]);
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    s.zeroize();
    init.zeroize();
    out
}

#[inline]
fn poly1305_tag_rfc8439(poly_key: &[u8; 32], aad: &[u8], ciphertext: &[u8]) -> [u8; 16] {
    use poly1305::universal_hash::{KeyInit, UniversalHash};
    let mut mac = poly1305::Poly1305::new(poly1305::Key::from_slice(poly_key));
    mac.update_padded(aad);
    mac.update_padded(ciphertext);
    let mut lens = [0u8; 16];
    lens[..8].copy_from_slice(&(aad.len() as u64).to_le_bytes());
    lens[8..16].copy_from_slice(&(ciphertext.len() as u64).to_le_bytes());
    mac.update_padded(&lens);
    let tag = mac.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(tag.as_slice());
    out
}

#[inline]
fn xchacha20_poly1305_encrypt_short(
    key: &[u8; 32],
    nonce: &[u8; 24],
    aad: &[u8],
    buf: &mut [u8],
) -> [u8; 16] {
    let mut subkey = hchacha20_subkey(key, &nonce[..16]);
    let mut poly_block = chacha20_block_words(&subkey, 0, &nonce[16..24]);
    let mut poly_key = [0u8; 32];
    poly_key.copy_from_slice(&poly_block[..32]);
    poly_block.zeroize();

    for (blk_idx, chunk) in buf.chunks_mut(64).enumerate() {
        let mut ks = chacha20_block_words(&subkey, 1 + blk_idx as u32, &nonce[16..24]);
        for (b, k) in chunk.iter_mut().zip(ks.iter()) {
            *b ^= *k;
        }
        ks.zeroize();
    }

    let tag = poly1305_tag_rfc8439(&poly_key, aad, buf);
    poly_key.zeroize();
    subkey.zeroize();
    tag
}

#[inline]
fn xchacha20_poly1305_decrypt_short(
    key: &[u8; 32],
    nonce: &[u8; 24],
    aad: &[u8],
    buf: &mut [u8],
    tag: &[u8; 16],
) -> bool {
    let mut subkey = hchacha20_subkey(key, &nonce[..16]);
    let mut poly_block = chacha20_block_words(&subkey, 0, &nonce[16..24]);
    let mut poly_key = [0u8; 32];
    poly_key.copy_from_slice(&poly_block[..32]);
    poly_block.zeroize();

    let expected_tag = poly1305_tag_rfc8439(&poly_key, aad, buf);
    poly_key.zeroize();
    if !ct_eq_slice(&expected_tag, tag) {
        subkey.zeroize();
        return false;
    }

    for (blk_idx, chunk) in buf.chunks_mut(64).enumerate() {
        let mut ks = chacha20_block_words(&subkey, 1 + blk_idx as u32, &nonce[16..24]);
        for (b, k) in chunk.iter_mut().zip(ks.iter()) {
            *b ^= *k;
        }
        ks.zeroize();
    }
    subkey.zeroize();
    true
}

/// Computes the 24-byte (192-bit) synthetic nonce for deterministic filename encryption
/// using the domain-separated `filename_siv_key`.
#[inline]
fn derive_filename_siv_nonce(key: &EncryptionKey, parent_inode: u64, name: &[u8]) -> [u8; 24] {
    let mut hasher = Blake2b512::new();
    hasher.update(b"OIFS_SIV_FILENAME_V2_XCHACHA20");
    hasher.update(key.filename_siv_key());
    hasher.update(parent_inode.to_le_bytes());
    hasher.update(name);
    let hash = hasher.finalize();
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&hash[..24]);
    nonce
}

#[inline(never)]
pub(crate) fn ct_eq_slice(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= std::hint::black_box(x ^ y);
    }
    std::hint::black_box(diff) == 0
}

/// Computes the keyed BLAKE2b-256 authentication tag over `sb.header_mac_canonical_bytes()`.
///
/// The returned tag is guaranteed non-zero so `header_mac == [0u8; 32]` unambiguously
/// distinguishes pre-MAC legacy headers from authenticated headers.
pub fn compute_superblock_mac(key: &EncryptionKey, sb: &crate::superblock::SuperBlock) -> [u8; 32] {
    let mut hasher = Blake2b512::new();
    hasher.update(b"OIFS-SUPERBLOCK-MAC-V1");
    hasher.update(key.superblock_mac_key());
    hasher.update(sb.header_mac_canonical_bytes());
    let mut digest = hasher.finalize_reset();
    let mut mac = [0u8; 32];
    mac.copy_from_slice(&digest[..32]);
    digest.zeroize();
    if mac == [0u8; 32] {
        mac[0] = 1;
    }
    mac
}

/// Verifies `sb.header_mac` in constant time against `compute_superblock_mac(key, sb)`.
///
/// Returns `false` if `header_mac` is all-zero or does not match.
pub fn verify_superblock_mac(key: &EncryptionKey, sb: &crate::superblock::SuperBlock) -> bool {
    if !sb.has_header_mac() {
        return false;
    }
    let expected = compute_superblock_mac(key, sb);
    ct_eq_slice(&expected, &sb.header_mac)
}

/// Encrypts a filename deterministically using a 192-bit (24-byte) Synthetic IV (SIV)
/// with `XChaCha20-Poly1305`, domain-separated subkeys (`filename_siv_key` and
/// `filename_cipher_key`), and `parent_inode` bound directly into the Poly1305 AAD.
///
/// Returns a safe ASCII string starting with `_e_` followed by unpadded Base64URL-encoded
/// `[24-byte XNonce | ciphertext | 16-byte Poly1305 tag]`.
/// Even inputs literally beginning with `_e_` are encrypted and authenticated.
pub fn encrypt_filename(
    key: &EncryptionKey,
    parent_inode: u64,
    name: &str,
) -> Result<String, EncryptionError> {
    if name.is_empty() || name == "." || name == ".." {
        return Ok(name.to_string());
    }

    let name_bytes = name.as_bytes();
    let nonce = derive_filename_siv_nonce(key, parent_inode, name_bytes);
    let aad = filename_aad_stack(parent_inode);

    if name_bytes.len() <= 255 {
        // Fast stack-allocated path: 1 heap String allocation instead of 4.
        let total_raw = 24 + name_bytes.len() + 16;
        let mut raw = [0u8; 24 + 255 + 16];
        raw[..24].copy_from_slice(&nonce);
        raw[24..24 + name_bytes.len()].copy_from_slice(name_bytes);
        if name_bytes.len() <= SHORT_XCHACHA_THRESHOLD {
            let tag = xchacha20_poly1305_encrypt_short(
                key.filename_cipher_key(),
                &nonce,
                &aad,
                &mut raw[24..24 + name_bytes.len()],
            );
            raw[24 + name_bytes.len()..total_raw].copy_from_slice(&tag);
        } else {
            let cipher = XChaCha20Poly1305::new_from_slice(key.filename_cipher_key())
                .map_err(|_| EncryptionError::InvalidKey)?;
            let tag = cipher
                .encrypt_in_place_detached(
                    XNonce::from_slice(&nonce),
                    &aad,
                    &mut raw[24..24 + name_bytes.len()],
                )
                .map_err(|_| EncryptionError::EncryptionFailed)?;
            raw[24 + name_bytes.len()..total_raw].copy_from_slice(tag.as_slice());
        }

        let b64_len = Base64UrlUnpadded::encoded_len(&raw[..total_raw]);
        let mut out = vec![0u8; FILENAME_ENC_PREFIX.len() + b64_len];
        out[..FILENAME_ENC_PREFIX.len()].copy_from_slice(FILENAME_ENC_PREFIX.as_bytes());
        Base64UrlUnpadded::encode(&raw[..total_raw], &mut out[FILENAME_ENC_PREFIX.len()..])
            .map_err(|_| EncryptionError::EncryptionFailed)?;
        // Base64UrlUnpadded output and "_e_" prefix are guaranteed ASCII.
        return Ok(String::from_utf8(out).unwrap());
    }

    let cipher = XChaCha20Poly1305::new_from_slice(key.filename_cipher_key())
        .map_err(|_| EncryptionError::InvalidKey)?;

    let ciphertext_with_tag = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: name_bytes,
                aad: &aad,
            },
        )
        .map_err(|_| EncryptionError::EncryptionFailed)?;

    let mut payload = Vec::with_capacity(24 + ciphertext_with_tag.len());
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&ciphertext_with_tag);

    let b64 = Base64UrlUnpadded::encode_string(&payload);
    Ok(format!("{}{}", FILENAME_ENC_PREFIX, b64))
}

/// Deterministic 12-byte per-directory nonce for `v3` RFC 8452 `AES-256-GCM-SIV` filenames.
#[inline]
pub(crate) fn filename_aes_gcmsiv_nonce(parent_inode: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(b"OFN3");
    nonce[4..12].copy_from_slice(&parent_inode.to_le_bytes());
    nonce
}

/// Encrypts a filename deterministically according to `scheme`:
/// - `v3` (`ENCRYPTION_SCHEME_AES256_GCM_V3`): RFC 8452 `AES-256-GCM-SIV` with
///   `parent_inode` bound in both nonce (`"OFN3" || parent_inode`) and AAD (`_e_<ct||tag>`).
/// - `v1/v2`: `XChaCha20-Poly1305` 192-bit BLAKE2b SIV.
pub fn encrypt_filename_for_scheme(
    key: &EncryptionKey,
    parent_inode: u64,
    name: &str,
    scheme: u8,
) -> Result<String, EncryptionError> {
    if scheme != ENCRYPTION_SCHEME_AES256_GCM_V3 {
        return encrypt_filename(key, parent_inode, name);
    }
    if name.is_empty() || name == "." || name == ".." {
        return Ok(name.to_string());
    }

    let name_bytes = name.as_bytes();
    let nonce = filename_aes_gcmsiv_nonce(parent_inode);
    let aad = filename_aad_stack(parent_inode);
    let cipher = &key.aes_filename_cipher;

    if name_bytes.len() <= 255 {
        let total_raw = name_bytes.len() + 16;
        let mut raw = [0u8; 255 + 16];
        raw[..name_bytes.len()].copy_from_slice(name_bytes);
        let tag = cipher
            .encrypt_in_place_detached(
                AesGcmSivNonce::from_slice(&nonce),
                &aad,
                &mut raw[..name_bytes.len()],
            )
            .map_err(|_| EncryptionError::EncryptionFailed)?;
        raw[name_bytes.len()..total_raw].copy_from_slice(tag.as_slice());

        let b64_len = Base64UrlUnpadded::encoded_len(&raw[..total_raw]);
        let mut str_buf = [0u8; 384];
        let prefix_len = FILENAME_ENC_PREFIX.len();
        str_buf[..prefix_len].copy_from_slice(FILENAME_ENC_PREFIX.as_bytes());
        Base64UrlUnpadded::encode(
            &raw[..total_raw],
            &mut str_buf[prefix_len..prefix_len + b64_len],
        )
        .map_err(|_| EncryptionError::EncryptionFailed)?;
        return Ok(std::str::from_utf8(&str_buf[..prefix_len + b64_len])
            .unwrap()
            .to_owned());
    }

    let ct_with_tag = cipher
        .encrypt(
            AesGcmSivNonce::from_slice(&nonce),
            Payload {
                msg: name_bytes,
                aad: &aad,
            },
        )
        .map_err(|_| EncryptionError::EncryptionFailed)?;
    let b64 = Base64UrlUnpadded::encode_string(&ct_with_tag);
    Ok(format!("{FILENAME_ENC_PREFIX}{b64}"))
}

/// Legacy v1 fallback decoder for 12-byte `ChaCha20Poly1305` filenames (`OIFS_SIV_FILENAME_V1`).
fn decrypt_filename_legacy_v1(
    key: &EncryptionKey,
    parent_inode: u64,
    raw_payload: &[u8],
) -> Option<String> {
    if raw_payload.len() < 12 + 16 {
        return None;
    }
    let nonce = &raw_payload[..12];
    let ciphertext_with_tag = &raw_payload[12..];
    let cipher = ChaCha20Poly1305::new_from_slice(key.as_bytes()).ok()?;
    let decrypted_bytes = cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext_with_tag)
        .ok()?;
    let plaintext = String::from_utf8(decrypted_bytes).ok()?;

    let mut hasher = Blake2b512::new();
    hasher.update(b"OIFS_SIV_FILENAME_V1");
    hasher.update(key.as_bytes());
    hasher.update(parent_inode.to_le_bytes());
    hasher.update(plaintext.as_bytes());
    let hash = hasher.finalize();
    if ct_eq_slice(&hash[..12], nonce) {
        Some(plaintext)
    } else {
        None
    }
}

/// Authenticates and decrypts an encrypted filename (`_e_...`) bound to `parent_inode`.
///
/// - When `encryption_version == ENCRYPTION_SCHEME_AES256_GCM_V3`, uses RFC 8452
///   `AES-256-GCM-SIV` with zero fallback to legacy schemes.
/// - Otherwise uses `XChaCha20-Poly1305` SIV (with `v1` fallback only when
///   `encryption_version < ENCRYPTION_SCHEME_CONTEXT_BOUND`).
pub fn decrypt_filename_authenticated(
    key: &EncryptionKey,
    parent_inode: u64,
    name: &str,
    encryption_version: u8,
) -> Result<String, EncryptionError> {
    if !name.starts_with(FILENAME_ENC_PREFIX) {
        return Err(EncryptionError::DecryptionFailed);
    }

    let b64 = &name[FILENAME_ENC_PREFIX.len()..];
    let aad = filename_aad_stack(parent_inode);

    if encryption_version == ENCRYPTION_SCHEME_AES256_GCM_V3 {
        let nonce = filename_aes_gcmsiv_nonce(parent_inode);
        let cipher = &key.aes_filename_cipher;
        if b64.len() <= 400 {
            let mut decode_buf = [0u8; 320];
            let payload_len = match Base64UrlUnpadded::decode(b64, &mut decode_buf) {
                Ok(p) if p.len() >= 16 => p.len(),
                _ => return Err(EncryptionError::DecryptionFailed),
            };
            let pt_len = payload_len - 16;
            let mut tag_bytes = [0u8; 16];
            tag_bytes.copy_from_slice(&decode_buf[pt_len..payload_len]);
            cipher
                .decrypt_in_place_detached(
                    AesGcmSivNonce::from_slice(&nonce),
                    &aad,
                    &mut decode_buf[..pt_len],
                    AesGcmSivTag::from_slice(&tag_bytes),
                )
                .map_err(|_| EncryptionError::DecryptionFailed)?;
            let plaintext = std::str::from_utf8(&decode_buf[..pt_len])
                .map_err(|_| EncryptionError::DecryptionFailed)?;
            return Ok(plaintext.to_string());
        }
        let payload =
            Base64UrlUnpadded::decode_vec(b64).map_err(|_| EncryptionError::DecryptionFailed)?;
        if payload.len() < 16 {
            return Err(EncryptionError::DecryptionFailed);
        }
        let dec = cipher
            .decrypt(
                AesGcmSivNonce::from_slice(&nonce),
                Payload {
                    msg: &payload,
                    aad: &aad,
                },
            )
            .map_err(|_| EncryptionError::DecryptionFailed)?;
        return String::from_utf8(dec).map_err(|_| EncryptionError::DecryptionFailed);
    }

    let allow_legacy_v1 = encryption_version < ENCRYPTION_SCHEME_CONTEXT_BOUND;

    if b64.len() <= 400 {
        let mut decode_buf = [0u8; 320];
        let payload_len = match Base64UrlUnpadded::decode(b64, &mut decode_buf) {
            Ok(p) if p.len() >= 28 => p.len(),
            _ => return Err(EncryptionError::DecryptionFailed),
        };

        if payload_len >= 40 {
            let mut nonce = [0u8; 24];
            nonce.copy_from_slice(&decode_buf[..24]);
            let pt_len = payload_len - 40;
            let mut tag_bytes = [0u8; 16];
            tag_bytes.copy_from_slice(&decode_buf[24 + pt_len..payload_len]);

            let mut pt_buf = [0u8; 280];
            pt_buf[..pt_len].copy_from_slice(&decode_buf[24..24 + pt_len]);
            let decrypted_ok = if pt_len <= SHORT_XCHACHA_THRESHOLD {
                xchacha20_poly1305_decrypt_short(
                    key.filename_cipher_key(),
                    &nonce,
                    &aad,
                    &mut pt_buf[..pt_len],
                    &tag_bytes,
                )
            } else {
                let cipher = XChaCha20Poly1305::new_from_slice(key.filename_cipher_key())
                    .map_err(|_| EncryptionError::InvalidKey)?;
                let tag = Tag::from_slice(&tag_bytes);
                cipher
                    .decrypt_in_place_detached(
                        XNonce::from_slice(&nonce),
                        &aad,
                        &mut pt_buf[..pt_len],
                        tag,
                    )
                    .is_ok()
            };
            if decrypted_ok && let Ok(plaintext) = std::str::from_utf8(&pt_buf[..pt_len]) {
                let expected_nonce =
                    derive_filename_siv_nonce(key, parent_inode, plaintext.as_bytes());
                if ct_eq_slice(&expected_nonce, &nonce) {
                    return Ok(plaintext.to_string());
                }
            }
        }

        if allow_legacy_v1
            && let Some(legacy_pt) =
                decrypt_filename_legacy_v1(key, parent_inode, &decode_buf[..payload_len])
        {
            return Ok(legacy_pt);
        }
        return Err(EncryptionError::DecryptionFailed);
    }

    let payload =
        Base64UrlUnpadded::decode_vec(b64).map_err(|_| EncryptionError::DecryptionFailed)?;

    if payload.len() >= 40 {
        let nonce = &payload[..24];
        let ciphertext_with_tag = &payload[24..];

        let cipher = XChaCha20Poly1305::new_from_slice(key.filename_cipher_key())
            .map_err(|_| EncryptionError::InvalidKey)?;

        if let Ok(decrypted_bytes) = cipher.decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext_with_tag,
                aad: &aad,
            },
        ) && let Ok(plaintext) = String::from_utf8(decrypted_bytes)
        {
            let expected_nonce = derive_filename_siv_nonce(key, parent_inode, plaintext.as_bytes());
            if ct_eq_slice(&expected_nonce, nonce) {
                return Ok(plaintext);
            }
        }
    }

    if allow_legacy_v1
        && let Some(legacy_pt) = decrypt_filename_legacy_v1(key, parent_inode, &payload)
    {
        return Ok(legacy_pt);
    }

    Err(EncryptionError::DecryptionFailed)
}

/// Decrypts a filename encrypted by `encrypt_filename`.
///
/// Non-prefixed names pass through unchanged; any `_e_`-prefixed entry must
/// authenticate cleanly via `decrypt_filename_authenticated` or return `Err(DecryptionFailed)`.
pub fn decrypt_filename(
    key: &EncryptionKey,
    parent_inode: u64,
    name: &str,
) -> Result<String, EncryptionError> {
    if !name.starts_with(FILENAME_ENC_PREFIX) {
        return Ok(name.to_string());
    }
    decrypt_filename_authenticated(key, parent_inode, name, ENCRYPTION_SCHEME_LEGACY)
}

/// Encryption-related errors
#[derive(Debug)]
pub enum EncryptionError {
    KeyDerivationFailed,
    InvalidKey,
    EncryptionFailed,
    DecryptionFailed,
}

impl std::fmt::Display for EncryptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KeyDerivationFailed => write!(f, "Key derivation failed"),
            Self::InvalidKey => write!(f, "Invalid encryption key"),
            Self::EncryptionFailed => write!(f, "Encryption operation failed"),
            Self::DecryptionFailed => {
                write!(f, "Decryption failed - wrong password or corrupted data")
            }
        }
    }
}

impl std::error::Error for EncryptionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_derivation_deterministic() {
        let password = "test_password_123";
        let salt = [42u8; 16];

        let key1 = derive_key(password, &salt).unwrap();
        let key2 = derive_key(password, &salt).unwrap();

        // Same password + salt should derive same key
        assert_eq!(key1.key, key2.key);
    }

    #[test]
    fn test_key_derivation_different_salts() {
        let password = "test_password_123";
        let salt1 = [42u8; 16];
        let salt2 = [43u8; 16];

        let key1 = derive_key(password, &salt1).unwrap();
        let key2 = derive_key(password, &salt2).unwrap();

        // Different salts should derive different keys
        assert_ne!(key1.key, key2.key);
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = EncryptionKey::from_bytes([1u8; 32]);
        let nonce = [2u8; 24];
        let plaintext = b"Hello, encryption!";

        let ciphertext = encrypt_data(plaintext, &key, &nonce, &[]).unwrap();
        assert_ne!(ciphertext.as_slice(), plaintext); // Should be encrypted

        let decrypted = decrypt_data(&ciphertext, &key, &nonce, &[]).unwrap();
        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_wrong_key_fails() {
        let key1 = EncryptionKey::from_bytes([1u8; 32]);
        let key2 = EncryptionKey::from_bytes([2u8; 32]);
        let nonce = [3u8; 24];
        let plaintext = b"Secret data";

        let ciphertext = encrypt_data(plaintext, &key1, &nonce, &[]).unwrap();

        // Decryption with wrong key should fail
        let result = decrypt_data(&ciphertext, &key2, &nonce, &[]);
        assert!(result.is_err());

        let mut in_place_buf = ciphertext.clone();
        assert!(decrypt_data_in_place(&mut in_place_buf, &key2, &nonce, &[]).is_err());
        let mut in_place_ok = ciphertext;
        decrypt_data_in_place(&mut in_place_ok, &key1, &nonce, &[]).unwrap();
        assert_eq!(in_place_ok.as_slice(), plaintext);
    }

    #[test]
    fn test_nonce_generation_unique() {
        let nonce1 = generate_nonce();
        let nonce2 = generate_nonce();

        // Very unlikely to generate same nonce twice
        assert_ne!(nonce1, nonce2);
    }

    #[test]
    fn test_filename_encryption_roundtrip_and_properties() {
        let key = EncryptionKey::from_bytes([42u8; 32]);
        let parent_inode = 100u64;
        let original_name = "my_confidential_report_2026.pdf";

        // 1. Encrypt filename
        let encrypted = encrypt_filename(&key, parent_inode, original_name).unwrap();
        assert!(encrypted.starts_with(FILENAME_ENC_PREFIX));
        assert!(!encrypted.contains(original_name));
        assert!(!encrypted.contains('/'));

        // 2. Deterministic: same key + parent_inode + name produces exact same ciphertext
        let encrypted_again = encrypt_filename(&key, parent_inode, original_name).unwrap();
        assert_eq!(encrypted, encrypted_again);

        // 3. Tweakable: different parent_inode produces different ciphertext
        let encrypted_other_dir = encrypt_filename(&key, 101u64, original_name).unwrap();
        assert_ne!(encrypted, encrypted_other_dir);

        // 4. Decrypt with correct key and parent_inode
        let decrypted = decrypt_filename(&key, parent_inode, &encrypted).unwrap();
        assert_eq!(decrypted, original_name);

        // 5. Decrypt with wrong parent_inode fails authentication cleanly
        let wrong_dir = decrypt_filename(&key, 999u64, &encrypted);
        assert!(wrong_dir.is_err(), "wrong parent_inode must fail auth");

        // 6. Plaintext `_e_`-prefixed filenames are encrypted (never stored in plaintext)
        let prefixed = "_e_literal_secret.txt";
        let enc_prefixed = encrypt_filename(&key, parent_inode, prefixed).unwrap();
        assert_ne!(enc_prefixed, prefixed);
        assert!(!enc_prefixed.contains("literal_secret"));
        assert_eq!(
            decrypt_filename_authenticated(
                &key,
                parent_inode,
                &enc_prefixed,
                ENCRYPTION_SCHEME_CONTEXT_BOUND
            )
            .unwrap(),
            prefixed
        );

        // 7. Unprefixed names fail strict authenticated decryption, pass legacy decrypt_filename
        let plain = "normal_unencrypted.txt";
        assert_eq!(decrypt_filename(&key, parent_inode, plain).unwrap(), plain);
        assert!(
            decrypt_filename_authenticated(
                &key,
                parent_inode,
                plain,
                ENCRYPTION_SCHEME_CONTEXT_BOUND
            )
            .is_err()
        );
    }

    #[test]
    fn test_blake2b_subkey_domain_separation_and_isolation() {
        let master = [0x5Au8; 32];
        let key = EncryptionKey::from_bytes(master);

        // Every subkey must be deterministically derived and pairwise distinct from the master key
        assert_ne!(key.payload_key(), &master);
        assert_ne!(key.filename_siv_key(), &master);
        assert_ne!(key.filename_cipher_key(), &master);
        assert_ne!(key.aes_payload_key(), &master);
        assert_ne!(key.aes_filename_key(), &master);
        assert_ne!(key.ipc_auth_key(), &master);
        assert_ne!(key.superblock_mac_key(), &master);

        let all_subkeys = [
            key.payload_key(),
            key.filename_siv_key(),
            key.filename_cipher_key(),
            key.aes_payload_key(),
            key.aes_filename_key(),
            key.ipc_auth_key(),
            key.superblock_mac_key(),
        ];
        for i in 0..all_subkeys.len() {
            for j in (i + 1)..all_subkeys.len() {
                assert_ne!(all_subkeys[i], all_subkeys[j]);
            }
        }
    }

    #[test]
    fn test_superblock_mac_detects_single_bit_tampering_and_wrong_key() {
        let key = EncryptionKey::from_bytes([0x42u8; 32]);
        let wrong_key = EncryptionKey::from_bytes([0x43u8; 32]);

        let mut sb = crate::superblock::SuperBlock::new(2560);
        sb.encrypted = true;
        sb.encryption_salt = [0x99u8; 16];
        sb.encryption_version = ENCRYPTION_SCHEME_AES256_GCM_V3;

        assert!(!verify_superblock_mac(&key, &sb));
        sb.header_mac = compute_superblock_mac(&key, &sb);
        assert!(sb.has_header_mac());
        assert!(verify_superblock_mac(&key, &sb));
        assert!(!verify_superblock_mac(&wrong_key, &sb));

        let mut bad_ver = sb;
        bad_ver.encryption_version = ENCRYPTION_SCHEME_CONTEXT_BOUND;
        assert!(!verify_superblock_mac(&key, &bad_ver));

        let mut bad_fmt = sb;
        bad_fmt.format_version = 1;
        assert!(!verify_superblock_mac(&key, &bad_fmt));

        let mut bad_salt = sb;
        bad_salt.encryption_salt[0] ^= 0x01;
        assert!(!verify_superblock_mac(&key, &bad_salt));

        let mut bad_mac = sb;
        bad_mac.header_mac[15] ^= 0x80;
        assert!(!verify_superblock_mac(&key, &bad_mac));
    }

    fn decode_hex_fixed<const N: usize>(hex: &str) -> [u8; N] {
        assert_eq!(hex.len(), N * 2);
        let mut out = [0u8; N];
        for i in 0..N {
            out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }

    fn decode_hex_vec(hex: &str) -> Vec<u8> {
        assert!(hex.len().is_multiple_of(2));
        (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn test_xaes256gcm_c2sp_official_test_vectors() {
        let nonce: [u8; 24] = *b"ABCDEFGHIJKLMNOPQRSTUVWX";
        let pt = b"XAES-256-GCM";

        // Vector 1: empty AAD (plus intermediate K1/Kx check)
        let key1 = [0x01u8; 32];
        let k1 = xaes256gcm_derive_k1(&key1);
        assert_eq!(
            k1,
            decode_hex_fixed::<16>("e531954aca063d5b8d9c47a47d4cc6f0")
        );
        let (kx1, nx1) = xaes256gcm_derive_subkey(&key1, &k1, &nonce);
        assert_eq!(
            kx1,
            decode_hex_fixed::<32>(
                "c8612c9ed53fe43e8e005b828a1631a0bbcb6ab2f46514ec4f439fcfd0fa969b"
            )
        );
        assert_eq!(&nx1, b"MNOPQRSTUVWX");

        let mut buf1 = pt.to_vec();
        xaes256gcm_encrypt_in_place(&key1, &k1, &nonce, b"", &mut buf1).unwrap();
        assert_eq!(
            buf1,
            decode_hex_vec("ce546ef63c9cc60765923609b33a9a1974e96e52daf2fcf7075e2271")
        );
        let mut dec1 = buf1.clone();
        xaes256gcm_decrypt_in_place(&key1, &k1, &nonce, b"", &mut dec1).unwrap();
        assert_eq!(dec1, pt);

        // Vector 2: AAD = "c2sp.org/XAES-256-GCM"
        let key2 = [0x03u8; 32];
        let k1_2 = xaes256gcm_derive_k1(&key2);
        let aad2 = b"c2sp.org/XAES-256-GCM";
        let mut buf2 = pt.to_vec();
        xaes256gcm_encrypt_in_place(&key2, &k1_2, &nonce, aad2, &mut buf2).unwrap();
        assert_eq!(
            buf2,
            decode_hex_vec("986ec1832593df5443a179437fd083bf3fdb41abd740a21f71eb769d")
        );

        // Single-bit tamper rejection on ciphertext, tag, nonce, and AAD
        let mut bad_ct = buf2.clone();
        bad_ct[0] ^= 0x01;
        assert!(xaes256gcm_decrypt_in_place(&key2, &k1_2, &nonce, aad2, &mut bad_ct).is_err());

        let mut bad_tag = buf2.clone();
        *bad_tag.last_mut().unwrap() ^= 0x80;
        assert!(xaes256gcm_decrypt_in_place(&key2, &k1_2, &nonce, aad2, &mut bad_tag).is_err());

        let mut bad_nonce = nonce;
        bad_nonce[3] ^= 0x01;
        let mut dec_bad_n = buf2.clone();
        assert!(
            xaes256gcm_decrypt_in_place(&key2, &k1_2, &bad_nonce, aad2, &mut dec_bad_n).is_err()
        );

        let mut dec_bad_aad = buf2;
        assert!(
            xaes256gcm_decrypt_in_place(
                &key2,
                &k1_2,
                &nonce,
                b"c2sp.org/XAES-256-GCm",
                &mut dec_bad_aad
            )
            .is_err()
        );
    }

    #[test]
    fn test_filename_aes256_gcm_siv_v3_roundtrip_tamper_and_long_names() {
        let key = EncryptionKey::from_bytes([0x77u8; 32]);
        let parent_inode = 42u64;
        let name = "budget_2026_confidential_report.parquet";

        let enc =
            encrypt_filename_for_scheme(&key, parent_inode, name, ENCRYPTION_SCHEME_AES256_GCM_V3)
                .unwrap();
        assert!(enc.starts_with(FILENAME_ENC_PREFIX));
        // Deterministic lookup key for same (key, parent_inode, name)
        assert_eq!(
            enc,
            encrypt_filename_for_scheme(&key, parent_inode, name, ENCRYPTION_SCHEME_AES256_GCM_V3)
                .unwrap()
        );
        // Domain-separated per directory
        assert_ne!(
            enc,
            encrypt_filename_for_scheme(&key, 43u64, name, ENCRYPTION_SCHEME_AES256_GCM_V3)
                .unwrap()
        );
        // Compact wire format: zero stored nonce -> ciphertext + 16-byte tag only
        let raw = Base64UrlUnpadded::decode_vec(&enc[FILENAME_ENC_PREFIX.len()..]).unwrap();
        assert_eq!(raw.len(), name.len() + 16);

        // Decryption & AAD binding check
        assert_eq!(
            decrypt_filename_authenticated(
                &key,
                parent_inode,
                &enc,
                ENCRYPTION_SCHEME_AES256_GCM_V3
            )
            .unwrap(),
            name
        );
        assert!(
            decrypt_filename_authenticated(&key, 999, &enc, ENCRYPTION_SCHEME_AES256_GCM_V3)
                .is_err()
        );

        // Tampered tag/ciphertext must fail auth
        for idx in [0usize, raw.len() / 2, raw.len() - 1] {
            let mut t = raw.clone();
            t[idx] ^= 0x01;
            let tok = format!(
                "{FILENAME_ENC_PREFIX}{}",
                Base64UrlUnpadded::encode_string(&t)
            );
            assert!(
                decrypt_filename_authenticated(
                    &key,
                    parent_inode,
                    &tok,
                    ENCRYPTION_SCHEME_AES256_GCM_V3
                )
                .is_err()
            );
        }

        // Compact v3 format fits up to 170+ byte filenames comfortably under 255-char dir limits
        let long_name = "a".repeat(170);
        let enc_long = encrypt_filename_for_scheme(
            &key,
            parent_inode,
            &long_name,
            ENCRYPTION_SCHEME_AES256_GCM_V3,
        )
        .unwrap();
        assert!(enc_long.len() <= 255);
        assert_eq!(
            decrypt_filename_authenticated(
                &key,
                parent_inode,
                &enc_long,
                ENCRYPTION_SCHEME_AES256_GCM_V3
            )
            .unwrap(),
            long_name
        );

        // Anti-downgrade: v2 token rejected under v3, v3 token rejected under v2
        let v2_tok = encrypt_filename(&key, parent_inode, name).unwrap();
        assert!(
            decrypt_filename_authenticated(
                &key,
                parent_inode,
                &v2_tok,
                ENCRYPTION_SCHEME_AES256_GCM_V3
            )
            .is_err()
        );
        assert!(
            decrypt_filename_authenticated(
                &key,
                parent_inode,
                &enc,
                ENCRYPTION_SCHEME_CONTEXT_BOUND
            )
            .is_err()
        );
    }

    #[test]
    #[ignore = "Manual M5 Pro / hardware AES throughput benchmark (`cargo test --release --lib bench_cipher_throughput_comparison -- --ignored --nocapture`)"]
    fn bench_cipher_throughput_comparison() {
        let key = EncryptionKey::from_bytes([0x55u8; 32]);
        let nonce = [0x24u8; 24];
        let aad = file_aad_stack(7);

        for (label, size, iters) in [
            ("64KiB", 64 * 1024usize, 4096usize),
            ("1MiB", 1024 * 1024, 512),
        ] {
            let payload = vec![0xA5u8; size];
            for (cipher_name, scheme) in [
                ("XChaCha20-Poly1305 (v2)", ENCRYPTION_SCHEME_CONTEXT_BOUND),
                ("C2SP XAES-256-GCM   (v3)", ENCRYPTION_SCHEME_AES256_GCM_V3),
            ] {
                let mut enc_buf = Vec::with_capacity(size + 16);
                let t0 = std::time::Instant::now();
                for _ in 0..iters {
                    enc_buf.clear();
                    enc_buf.extend_from_slice(&payload);
                    encrypt_data_in_place_for_scheme(
                        std::hint::black_box(&mut enc_buf),
                        &key,
                        &nonce,
                        &aad,
                        scheme,
                    )
                    .unwrap();
                    std::hint::black_box(&enc_buf);
                }
                let enc_s = t0.elapsed().as_secs_f64();

                let ct = encrypt_data_for_scheme(&payload, &key, &nonce, &aad, scheme).unwrap();
                let mut dst = vec![0u8; size];
                let t1 = std::time::Instant::now();
                for _ in 0..iters {
                    decrypt_into_slice_for_scheme(
                        std::hint::black_box(&ct),
                        &mut dst,
                        &key,
                        &nonce,
                        &aad,
                        scheme,
                    )
                    .unwrap();
                    std::hint::black_box(&dst);
                }
                let dec_s = t1.elapsed().as_secs_f64();

                let total_mib = (size * iters) as f64 / (1024.0 * 1024.0);
                println!(
                    "[bench {label}] {cipher_name}: encrypt = {:7.1} MiB/s ({:.2} GiB/s) | decrypt = {:7.1} MiB/s ({:.2} GiB/s)",
                    total_mib / enc_s,
                    (total_mib / 1024.0) / enc_s,
                    total_mib / dec_s,
                    (total_mib / 1024.0) / dec_s,
                );
            }
        }

        let name = "very_typical_agent_log_2026.json";
        let fn_iters = 50_000usize;
        for (label, scheme) in [
            ("XChaCha20-SIV (v2)", ENCRYPTION_SCHEME_CONTEXT_BOUND),
            ("AES-256-GCM-SIV (v3)", ENCRYPTION_SCHEME_AES256_GCM_V3),
        ] {
            let t0 = std::time::Instant::now();
            for i in 0..fn_iters {
                let enc = encrypt_filename_for_scheme(&key, (i & 31) as u64, name, scheme).unwrap();
                std::hint::black_box(
                    decrypt_filename_authenticated(&key, (i & 31) as u64, &enc, scheme).unwrap(),
                );
            }
            let elapsed = t0.elapsed().as_secs_f64();
            println!(
                "[bench filename 32B] {label}: {:.0} roundtrips/s ({:.2} us/op)",
                fn_iters as f64 / elapsed,
                (elapsed * 1e6) / fn_iters as f64
            );
        }
    }

    #[test]
    fn test_filename_xchacha20_24byte_nonce_aad_and_legacy_fallback() {
        let key = EncryptionKey::from_bytes([0x33u8; 32]);
        let parent_inode = 42u64;
        let name = "quantum_resilient_file.dat";

        let enc = encrypt_filename(&key, parent_inode, name).unwrap();
        let b64 = &enc[FILENAME_ENC_PREFIX.len()..];
        let raw = Base64UrlUnpadded::decode_vec(b64).unwrap();

        // Must carry 24-byte (192-bit) XNonce + ciphertext + 16-byte Poly1305 tag
        assert_eq!(raw.len(), 24 + name.len() + 16);

        // Tampering with any byte in the 24-byte nonce, ciphertext, or 16-byte Poly1305 tag must fail auth
        for idx in [0usize, 12, 23, 24, raw.len() - 1] {
            let mut tampered = raw.clone();
            tampered[idx] ^= 0x01;
            let tampered_tok = format!(
                "{}{}",
                FILENAME_ENC_PREFIX,
                Base64UrlUnpadded::encode_string(&tampered)
            );
            assert!(
                decrypt_filename(&key, parent_inode, &tampered_tok).is_err(),
                "Tampered byte at index {idx} must be rejected by Poly1305/SIV"
            );
        }

        // Legacy v1 12-byte ChaCha20Poly1305 encrypted filenames remain readable on v1 images
        // but are rejected on v2+ context-bound images (anti-downgrade).
        let mut hasher = Blake2b512::new();
        hasher.update(b"OIFS_SIV_FILENAME_V1");
        hasher.update(key.as_bytes());
        hasher.update(parent_inode.to_le_bytes());
        hasher.update(name.as_bytes());
        let hash = hasher.finalize();
        let mut legacy_nonce = [0u8; 12];
        legacy_nonce.copy_from_slice(&hash[..12]);
        let legacy_cipher = ChaCha20Poly1305::new_from_slice(key.as_bytes()).unwrap();
        let legacy_ct = legacy_cipher
            .encrypt(Nonce::from_slice(&legacy_nonce), name.as_bytes())
            .unwrap();
        let mut legacy_raw = Vec::new();
        legacy_raw.extend_from_slice(&legacy_nonce);
        legacy_raw.extend_from_slice(&legacy_ct);
        let legacy_tok = format!(
            "{}{}",
            FILENAME_ENC_PREFIX,
            Base64UrlUnpadded::encode_string(&legacy_raw)
        );
        assert_eq!(
            decrypt_filename_authenticated(
                &key,
                parent_inode,
                &legacy_tok,
                ENCRYPTION_SCHEME_LEGACY
            )
            .unwrap(),
            name
        );
        assert!(
            decrypt_filename_authenticated(
                &key,
                parent_inode,
                &legacy_tok,
                ENCRYPTION_SCHEME_CONTEXT_BOUND
            )
            .is_err()
        );
    }

    #[test]
    fn test_derive_key_hash_password_into_matches_password_hasher() {
        use argon2::PasswordHasher;
        use argon2::password_hash::SaltString;

        let salt = [0x42u8; 16];
        let pwd = "correct-horse-battery-staple";
        let params = Params::new(1024, 1, 1, Some(32)).unwrap();
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let salt_string = SaltString::encode_b64(&salt).unwrap();
        let ph = argon2.hash_password(pwd.as_bytes(), &salt_string).unwrap();
        let ref_bytes = ph.hash.unwrap();
        let derived = derive_key_with_params(pwd, &salt, 1024, 1, 1).unwrap();
        assert_eq!(derived.as_bytes(), ref_bytes.as_bytes());
    }

    #[test]
    fn test_argon2id_owasp_rfc9106_parameters() {
        assert_eq!(ARGON2_M_COST_KIB, 65_536); // 64 MiB
        assert_eq!(ARGON2_T_COST, 3);
        assert_eq!(ARGON2_P_COST, 4);
    }

    #[test]
    fn test_short_scalar_xchacha20_matches_reference_byte_for_byte() {
        let key: [u8; 32] = core::array::from_fn(|i| (i as u8).wrapping_mul(13).wrapping_add(5));
        let nonce: [u8; 24] = core::array::from_fn(|i| (i as u8).wrapping_mul(29).wrapping_add(1));
        let ref_cipher = XChaCha20Poly1305::new_from_slice(&key).unwrap();

        for aad_len in [0usize, 1, 15, 16, 17, 20, 32] {
            let aad: Vec<u8> = (0..aad_len).map(|i| (i as u8) ^ 0xA5).collect();
            for msg_len in 0..=SHORT_XCHACHA_THRESHOLD {
                let pt: Vec<u8> = (0..msg_len)
                    .map(|i| (i as u8).wrapping_mul(7).wrapping_add(3))
                    .collect();

                let mut scalar_buf = pt.clone();
                let scalar_tag =
                    xchacha20_poly1305_encrypt_short(&key, &nonce, &aad, &mut scalar_buf);

                let mut ref_buf = pt.clone();
                let ref_tag = ref_cipher
                    .encrypt_in_place_detached(XNonce::from_slice(&nonce), &aad, &mut ref_buf)
                    .unwrap();

                assert_eq!(
                    scalar_buf, ref_buf,
                    "ciphertext mismatch at aad_len={aad_len}, msg_len={msg_len}"
                );
                assert_eq!(
                    scalar_tag.as_slice(),
                    ref_tag.as_slice(),
                    "tag mismatch at aad_len={aad_len}, msg_len={msg_len}"
                );

                // Tampered tag must fail authentication without modifying plaintext check.
                let mut bad_tag = scalar_tag;
                bad_tag[0] ^= 0x01;
                let mut tamper_buf = scalar_buf.clone();
                assert!(!xchacha20_poly1305_decrypt_short(
                    &key,
                    &nonce,
                    &aad,
                    &mut tamper_buf,
                    &bad_tag
                ));

                // Valid tag decrypts back to exact plaintext.
                assert!(xchacha20_poly1305_decrypt_short(
                    &key,
                    &nonce,
                    &aad,
                    &mut scalar_buf,
                    &scalar_tag
                ));
                assert_eq!(scalar_buf, pt);
            }
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Prove that whole-file, chunk, and filename AAD tags (`file_aad_stack`, `chunk_aad_stack`,
    /// `filename_aad_stack`) are strictly domain-separated by their leading kind byte (`0x01`,
    /// `0x02`, `0x03`) and encode `inode_id`/`parent_inode` bijectively without collision.
    #[kani::proof]
    fn proof_aad_domain_separation_and_filename_aad_layout() {
        let id_a: u64 = kani::any();
        let id_b: u64 = kani::any();
        let chunk_idx: u64 = kani::any();
        let flags: u8 = kani::any();

        let f_aad = file_aad_stack(id_a);
        let c_aad = chunk_aad_stack(id_a, chunk_idx, flags);
        let fn_aad_a = filename_aad_stack(id_a);
        let fn_aad_b = filename_aad_stack(id_b);

        // Kind tags must never collide across file, chunk, and filename domains
        assert_ne!(f_aad[0], c_aad[0]);
        assert_ne!(f_aad[0], fn_aad_a[0]);
        assert_ne!(c_aad[0], fn_aad_a[0]);
        assert_ne!(f_aad, fn_aad_a);

        // Filename AAD is injective in parent_inode
        assert_eq!(fn_aad_a == fn_aad_b, id_a == id_b);
    }

    #[kani::proof]
    #[kani::unwind(10)]
    fn proof_ct_eq_slice_equivalence_and_bounds() {
        let a: [u8; 8] = kani::any();
        let b: [u8; 8] = kani::any();
        let la: usize = kani::any();
        let lb: usize = kani::any();
        kani::assume(la <= 8 && lb <= 8);

        assert_eq!(ct_eq_slice(&a[..la], &b[..lb]), a[..la] == b[..lb]);
    }

    #[kani::proof]
    fn proof_encryption_cipher_roundtrip_and_v3_filename_nonce_injective() {
        let ver: u8 = kani::any();
        if let Some(c) = EncryptionCipher::from_scheme_version(ver) {
            if ver == ENCRYPTION_SCHEME_LEGACY || ver == ENCRYPTION_SCHEME_CONTEXT_BOUND {
                assert_eq!(c, EncryptionCipher::XChaCha20Poly1305);
                assert_eq!(c.scheme_version(), ENCRYPTION_SCHEME_CONTEXT_BOUND);
            } else {
                assert_eq!(ver, ENCRYPTION_SCHEME_AES256_GCM_V3);
                assert_eq!(c, EncryptionCipher::Aes256Gcm);
                assert_eq!(c.scheme_version(), ENCRYPTION_SCHEME_AES256_GCM_V3);
            }
        } else {
            assert!(ver != 1 && ver != 2 && ver != 3);
        }

        let dir_a: u64 = kani::any();
        let dir_b: u64 = kani::any();
        let na = filename_aes_gcmsiv_nonce(dir_a);
        let nb = filename_aes_gcmsiv_nonce(dir_b);
        assert_eq!(na == nb, dir_a == dir_b);
    }
}
