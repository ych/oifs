use argon2::password_hash::SaltString;
use argon2::{Algorithm, Argon2, Params, PasswordHasher, Version};
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
/// Domain separation label for deriving the 256-bit IPC challenge-response authentication subkey.
pub const KDF_CONTEXT_IPC_AUTH: &[u8] = b"oifs.kdf.v2.ipc.auth.blake2b";

/// Derive a cryptographically isolated 256-bit subkey from a 256-bit master key and
/// a domain-separation `context` label using `BLAKE2b-512`.
///
/// Ensures that the master key is never reused directly across distinct primitives
/// (`XChaCha20-Poly1305` file payloads, `BLAKE2b` filename SIV MAC, `XChaCha20-Poly1305`
/// filename AEAD, and `BLAKE2b` IPC authentication).
#[inline]
pub fn derive_subkey(master_key: &[u8; 32], context: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b512::new();
    hasher.update(b"OIFS-BLAKE2B-SUBKEY-KDF-V2");
    hasher.update([context.len() as u8]);
    hasher.update(context);
    hasher.update(master_key);
    let digest = hasher.finalize();
    let mut subkey = [0u8; 32];
    subkey.copy_from_slice(&digest[..32]);
    subkey
}

/// Encryption key hierarchy with BLAKE2b domain-separated subkeys and automatic zeroization on drop.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct EncryptionKey {
    /// Master 256-bit key derived from Argon2id (or supplied directly)
    key: [u8; 32],
    /// Domain-separated 256-bit subkey for file and seekable-chunk `XChaCha20-Poly1305` payloads
    payload_key: [u8; 32],
    /// Domain-separated 256-bit subkey for filename SIV `BLAKE2b-512` synthetic nonce derivation
    filename_siv_key: [u8; 32],
    /// Domain-separated 256-bit subkey for filename `XChaCha20-Poly1305` AEAD encryption
    filename_cipher_key: [u8; 32],
    /// Domain-separated 256-bit subkey for IPC challenge-response authentication
    ipc_auth_key: [u8; 32],
}

impl EncryptionKey {
    /// Create a new encryption key hierarchy from a 256-bit master key,
    /// immediately deriving domain-separated subkeys via `BLAKE2b-512`.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        let payload_key = derive_subkey(&bytes, KDF_CONTEXT_PAYLOAD);
        let filename_siv_key = derive_subkey(&bytes, KDF_CONTEXT_FILENAME_SIV);
        let filename_cipher_key = derive_subkey(&bytes, KDF_CONTEXT_FILENAME_CIPHER);
        let ipc_auth_key = derive_subkey(&bytes, KDF_CONTEXT_IPC_AUTH);
        Self {
            key: bytes,
            payload_key,
            filename_siv_key,
            filename_cipher_key,
            ipc_auth_key,
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

    /// Domain-separated subkey for IPC challenge-response authentication.
    #[inline]
    pub fn ipc_auth_key(&self) -> &[u8; 32] {
        &self.ipc_auth_key
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

    // Convert salt to SaltString format
    let salt_string =
        SaltString::encode_b64(salt).map_err(|_| EncryptionError::KeyDerivationFailed)?;

    // Derive key using Argon2id
    let password_hash = argon2
        .hash_password(password.as_bytes(), &salt_string)
        .map_err(|_| EncryptionError::KeyDerivationFailed)?;

    // Extract the hash output as our encryption key
    let hash = password_hash
        .hash
        .ok_or(EncryptionError::KeyDerivationFailed)?;

    let hash_bytes = hash.as_bytes();
    if hash_bytes.len() < 32 {
        return Err(EncryptionError::KeyDerivationFailed);
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&hash_bytes[..32]);
    let enc_key = EncryptionKey::from_bytes(key);
    key.zeroize();

    Ok(enc_key)
}

/// Encrypt data using XChaCha20-Poly1305 AEAD cipher
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
    let cipher = XChaCha20Poly1305::new_from_slice(key.payload_key())
        .map_err(|_| EncryptionError::InvalidKey)?;

    let xnonce = XNonce::from_slice(nonce);

    cipher
        .encrypt(
            xnonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| EncryptionError::EncryptionFailed)
}

/// Decrypt data using XChaCha20-Poly1305 AEAD cipher
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
    let cipher = XChaCha20Poly1305::new_from_slice(key.payload_key())
        .map_err(|_| EncryptionError::InvalidKey)?;

    let xnonce = XNonce::from_slice(nonce);

    cipher
        .decrypt(
            xnonce,
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| EncryptionError::DecryptionFailed)
}

/// Encrypt data in-place using XChaCha20-Poly1305 AEAD cipher (P4.21),
/// appending the 16-byte authentication tag directly to `buffer` without
/// allocating a second full-payload buffer.
pub fn encrypt_data_in_place(
    buffer: &mut Vec<u8>,
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
) -> Result<(), EncryptionError> {
    let cipher = XChaCha20Poly1305::new_from_slice(key.payload_key())
        .map_err(|_| EncryptionError::InvalidKey)?;
    let xnonce = XNonce::from_slice(nonce);
    cipher
        .encrypt_in_place(xnonce, aad, buffer)
        .map_err(|_| EncryptionError::EncryptionFailed)
}

/// Encrypt `plaintext` into a single `[24-byte nonce | ciphertext | 16-byte tag]`
/// allocation (P4.17), avoiding an intermediate ciphertext `Vec` copy.
pub fn encrypt_with_nonce_prefix(
    plaintext: &[u8],
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
) -> Result<Vec<u8>, EncryptionError> {
    let cipher = XChaCha20Poly1305::new_from_slice(key.payload_key())
        .map_err(|_| EncryptionError::InvalidKey)?;
    let xnonce = XNonce::from_slice(nonce);

    let mut out = Vec::with_capacity(24 + plaintext.len() + 16);
    out.extend_from_slice(nonce);
    out.extend_from_slice(plaintext);
    let tag = cipher
        .encrypt_in_place_detached(xnonce, aad, &mut out[24..])
        .map_err(|_| EncryptionError::EncryptionFailed)?;
    out.extend_from_slice(tag.as_slice());
    Ok(out)
}

/// Decrypt `ciphertext_with_tag` into a caller-provided slice `dst` (which must
/// have length `ciphertext_with_tag.len() - 16`) without heap allocations (P4.17).
pub fn decrypt_into_slice(
    ciphertext_with_tag: &[u8],
    dst: &mut [u8],
    key: &EncryptionKey,
    nonce: &[u8; 24],
    aad: &[u8],
) -> Result<(), EncryptionError> {
    if ciphertext_with_tag.len() < 16 || dst.len() != ciphertext_with_tag.len() - 16 {
        return Err(EncryptionError::DecryptionFailed);
    }
    let ct_len = dst.len();
    dst.copy_from_slice(&ciphertext_with_tag[..ct_len]);
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

/// The context-bound encryption scheme (SEC-19).
///
/// The associated data binds the ciphertext to the file (and, for a seekable
/// chunk, to the chunk index and its flags), so a chunk cannot be moved to
/// another file, reordered or rolled back without the authentication failing.
pub const ENCRYPTION_SCHEME_CONTEXT_BOUND: u8 = 2;

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
/// noticing (SEC-19).
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
/// The legacy scheme uses empty associated data, so images written before
/// SEC-19 stay readable.
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

#[inline]
fn ct_eq_slice(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Encrypts a filename deterministically using a 192-bit (24-byte) Synthetic IV (SIV)
/// with `XChaCha20-Poly1305`, domain-separated subkeys (`filename_siv_key` and
/// `filename_cipher_key`), and `parent_inode` bound directly into the Poly1305 AAD.
///
/// Returns a safe ASCII string starting with `_e_` followed by unpadded Base64URL-encoded
/// `[24-byte XNonce | ciphertext | 16-byte Poly1305 tag]`.
/// If the input is already encrypted with `_e_`, it returns it as-is.
pub fn encrypt_filename(
    key: &EncryptionKey,
    parent_inode: u64,
    name: &str,
) -> Result<String, EncryptionError> {
    if name.is_empty() || name == "." || name == ".." {
        return Ok(name.to_string());
    }
    if name.starts_with(FILENAME_ENC_PREFIX) {
        return Ok(name.to_string());
    }

    let name_bytes = name.as_bytes();
    let nonce = derive_filename_siv_nonce(key, parent_inode, name_bytes);
    let aad = filename_aad_stack(parent_inode);

    let cipher = XChaCha20Poly1305::new_from_slice(key.filename_cipher_key())
        .map_err(|_| EncryptionError::InvalidKey)?;

    if name_bytes.len() <= 255 {
        // Fast stack-allocated path (P4.29): 1 heap String allocation instead of 4.
        let total_raw = 24 + name_bytes.len() + 16;
        let mut raw = [0u8; 24 + 255 + 16];
        raw[..24].copy_from_slice(&nonce);
        raw[24..24 + name_bytes.len()].copy_from_slice(name_bytes);
        let tag = cipher
            .encrypt_in_place_detached(
                XNonce::from_slice(&nonce),
                &aad,
                &mut raw[24..24 + name_bytes.len()],
            )
            .map_err(|_| EncryptionError::EncryptionFailed)?;
        raw[24 + name_bytes.len()..total_raw].copy_from_slice(tag.as_slice());

        let b64_len = Base64UrlUnpadded::encoded_len(&raw[..total_raw]);
        let mut out = vec![0u8; FILENAME_ENC_PREFIX.len() + b64_len];
        out[..FILENAME_ENC_PREFIX.len()].copy_from_slice(FILENAME_ENC_PREFIX.as_bytes());
        Base64UrlUnpadded::encode(&raw[..total_raw], &mut out[FILENAME_ENC_PREFIX.len()..])
            .map_err(|_| EncryptionError::EncryptionFailed)?;
        // Base64UrlUnpadded output and "_e_" prefix are guaranteed ASCII.
        return Ok(String::from_utf8(out).unwrap());
    }

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

/// Decrypts a filename encrypted by `encrypt_filename`.
///
/// Authenticates the 24-byte `XChaCha20-Poly1305` ciphertext with `parent_inode` bound
/// in the Poly1305 AAD and verifies the 24-byte synthetic nonce in constant time.
/// Falls back to the legacy 12-byte `ChaCha20-Poly1305` format if needed, or returns
/// the original name if `name` does not start with `_e_` or fails authentication.
pub fn decrypt_filename(
    key: &EncryptionKey,
    parent_inode: u64,
    name: &str,
) -> Result<String, EncryptionError> {
    if !name.starts_with(FILENAME_ENC_PREFIX) {
        return Ok(name.to_string());
    }

    let b64 = &name[FILENAME_ENC_PREFIX.len()..];
    let aad = filename_aad_stack(parent_inode);

    if b64.len() <= 400 {
        let mut decode_buf = [0u8; 320];
        let payload_len = match Base64UrlUnpadded::decode(b64, &mut decode_buf) {
            Ok(p) if p.len() >= 28 => p.len(),
            _ => return Ok(name.to_string()),
        };

        if payload_len >= 40 {
            let mut nonce = [0u8; 24];
            nonce.copy_from_slice(&decode_buf[..24]);
            let pt_len = payload_len - 40;
            let mut tag_bytes = [0u8; 16];
            tag_bytes.copy_from_slice(&decode_buf[24 + pt_len..payload_len]);
            let tag = Tag::from_slice(&tag_bytes);

            let cipher = XChaCha20Poly1305::new_from_slice(key.filename_cipher_key())
                .map_err(|_| EncryptionError::InvalidKey)?;
            let mut pt_buf = [0u8; 280];
            pt_buf[..pt_len].copy_from_slice(&decode_buf[24..24 + pt_len]);
            if cipher
                .decrypt_in_place_detached(
                    XNonce::from_slice(&nonce),
                    &aad,
                    &mut pt_buf[..pt_len],
                    tag,
                )
                .is_ok()
                && let Ok(plaintext) = std::str::from_utf8(&pt_buf[..pt_len])
            {
                let expected_nonce =
                    derive_filename_siv_nonce(key, parent_inode, plaintext.as_bytes());
                if ct_eq_slice(&expected_nonce, &nonce) {
                    return Ok(plaintext.to_string());
                }
            }
        }

        if let Some(legacy_pt) =
            decrypt_filename_legacy_v1(key, parent_inode, &decode_buf[..payload_len])
        {
            return Ok(legacy_pt);
        }
        return Ok(name.to_string());
    }

    let payload = match Base64UrlUnpadded::decode_vec(b64) {
        Ok(p) => p,
        Err(_) => return Ok(name.to_string()),
    };

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

    if let Some(legacy_pt) = decrypt_filename_legacy_v1(key, parent_inode, &payload) {
        return Ok(legacy_pt);
    }

    Ok(name.to_string())
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

        // 5. Decrypt with wrong parent_inode falls back gracefully
        let wrong_dir = decrypt_filename(&key, 999u64, &encrypted).unwrap();
        assert_ne!(wrong_dir, original_name); // Decryption fails synthetic check

        // 6. Non-encrypted name passes through unchanged
        let plain = "normal_unencrypted.txt";
        assert_eq!(decrypt_filename(&key, parent_inode, plain).unwrap(), plain);
    }

    #[test]
    fn test_blake2b_subkey_domain_separation_and_isolation() {
        let master = [0x5Au8; 32];
        let key = EncryptionKey::from_bytes(master);

        // Every subkey must be deterministically derived and pairwise distinct from the master key
        assert_ne!(key.payload_key(), &master);
        assert_ne!(key.filename_siv_key(), &master);
        assert_ne!(key.filename_cipher_key(), &master);
        assert_ne!(key.ipc_auth_key(), &master);

        assert_ne!(key.payload_key(), key.filename_siv_key());
        assert_ne!(key.payload_key(), key.filename_cipher_key());
        assert_ne!(key.payload_key(), key.ipc_auth_key());
        assert_ne!(key.filename_siv_key(), key.filename_cipher_key());
        assert_ne!(key.filename_siv_key(), key.ipc_auth_key());
        assert_ne!(key.filename_cipher_key(), key.ipc_auth_key());
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
            assert_eq!(
                decrypt_filename(&key, parent_inode, &tampered_tok).unwrap(),
                tampered_tok,
                "Tampered byte at index {idx} must be rejected by Poly1305/SIV"
            );
        }

        // Legacy v1 12-byte ChaCha20Poly1305 encrypted filenames remain readable
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
            decrypt_filename(&key, parent_inode, &legacy_tok).unwrap(),
            name
        );
    }

    #[test]
    fn test_argon2id_owasp_rfc9106_parameters() {
        assert_eq!(ARGON2_M_COST_KIB, 65_536); // 64 MiB
        assert_eq!(ARGON2_T_COST, 3);
        assert_eq!(ARGON2_P_COST, 4);
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
}
