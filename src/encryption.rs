use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHasher};
use base64ct::{Base64UrlUnpadded, Encoding};
use blake2::{Blake2b512, Digest};
use chacha20poly1305::{
    ChaCha20Poly1305, Nonce, XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, OsRng},
};
use std::fmt;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Encryption key with automatic zeroization on drop
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct EncryptionKey {
    key: [u8; 32], // 256-bit key for XChaCha20-Poly1305
}

impl EncryptionKey {
    /// Create a new encryption key from raw bytes
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self { key: bytes }
    }

    /// Get key bytes (internal use only)
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.key
    }
}

impl fmt::Debug for EncryptionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EncryptionKey([REDACTED])")
    }
}

/// Derive an encryption key from a password using Argon2id
///
/// # Arguments
/// * `password` - User password/passphrase
/// * `salt` - 16-byte salt (should be stored in SuperBlock)
///
/// # Returns
/// 256-bit encryption key suitable for XChaCha20-Poly1305
pub fn derive_key(password: &str, salt: &[u8; 16]) -> Result<EncryptionKey, EncryptionError> {
    let argon2 = Argon2::default();

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

    Ok(EncryptionKey::from_bytes(key))
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
) -> Result<Vec<u8>, EncryptionError> {
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_bytes())
        .map_err(|_| EncryptionError::InvalidKey)?;

    let xnonce = XNonce::from_slice(nonce);

    cipher
        .encrypt(xnonce, plaintext)
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
) -> Result<Vec<u8>, EncryptionError> {
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_bytes())
        .map_err(|_| EncryptionError::InvalidKey)?;

    let xnonce = XNonce::from_slice(nonce);

    cipher
        .decrypt(xnonce, ciphertext)
        .map_err(|_| EncryptionError::DecryptionFailed)
}

/// Generate a cryptographically secure random nonce
pub fn generate_nonce() -> [u8; 24] {
    let mut nonce = [0u8; 24];
    use rand::RngCore;
    OsRng.fill_bytes(&mut nonce);
    nonce
}

/// Generate a cryptographically secure random salt
pub fn generate_salt() -> [u8; 16] {
    let mut salt = [0u8; 16];
    use rand::RngCore;
    OsRng.fill_bytes(&mut salt);
    salt
}

/// Constant prefix for encrypted filenames
pub const FILENAME_ENC_PREFIX: &str = "_e_";

/// Encrypts a filename deterministically using Synthetic IV (SIV) with parent_inode as tweak.
///
/// Returns a safe ASCII string starting with `_e_` followed by unpadded Base64URL-encoded ciphertext.
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

    // Deterministic Synthetic Nonce derived from (key, parent_inode, name)
    let mut hasher = Blake2b512::new();
    hasher.update(b"OIFS_SIV_FILENAME_V1");
    hasher.update(key.as_bytes());
    hasher.update(parent_inode.to_le_bytes());
    hasher.update(name.as_bytes());
    let hash = hasher.finalize();

    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&hash[..12]);

    let cipher = ChaCha20Poly1305::new_from_slice(key.as_bytes())
        .map_err(|_| EncryptionError::InvalidKey)?;

    let ciphertext_with_tag = cipher
        .encrypt(Nonce::from_slice(&nonce), name.as_bytes())
        .map_err(|_| EncryptionError::EncryptionFailed)?;

    let mut payload = Vec::with_capacity(12 + ciphertext_with_tag.len());
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&ciphertext_with_tag);

    let b64 = Base64UrlUnpadded::encode_string(&payload);
    Ok(format!("{}{}", FILENAME_ENC_PREFIX, b64))
}

/// Decrypts a filename encrypted by `encrypt_filename`.
///
/// If `name` does not start with `_e_` or fails to decode/authenticate,
/// it gracefully returns the original name (for backward compatibility).
pub fn decrypt_filename(
    key: &EncryptionKey,
    parent_inode: u64,
    name: &str,
) -> Result<String, EncryptionError> {
    if !name.starts_with(FILENAME_ENC_PREFIX) {
        return Ok(name.to_string());
    }

    let b64 = &name[FILENAME_ENC_PREFIX.len()..];
    let payload = match Base64UrlUnpadded::decode_vec(b64) {
        Ok(p) => p,
        Err(_) => return Ok(name.to_string()),
    };

    if payload.len() < 12 + 16 {
        return Ok(name.to_string());
    }

    let nonce = &payload[..12];
    let ciphertext_with_tag = &payload[12..];

    let cipher = ChaCha20Poly1305::new_from_slice(key.as_bytes())
        .map_err(|_| EncryptionError::InvalidKey)?;

    let decrypted_bytes = match cipher.decrypt(Nonce::from_slice(nonce), ciphertext_with_tag) {
        Ok(b) => b,
        Err(_) => return Ok(name.to_string()),
    };

    let plaintext = match String::from_utf8(decrypted_bytes) {
        Ok(s) => s,
        Err(_) => return Ok(name.to_string()),
    };

    // Verify synthetic nonce matches (key, parent_inode, plaintext)
    let mut hasher = Blake2b512::new();
    hasher.update(b"OIFS_SIV_FILENAME_V1");
    hasher.update(key.as_bytes());
    hasher.update(parent_inode.to_le_bytes());
    hasher.update(plaintext.as_bytes());
    let hash = hasher.finalize();

    if &hash[..12] != nonce {
        return Ok(name.to_string());
    }

    Ok(plaintext)
}

/// Encryption-related errors
#[derive(Debug, thiserror::Error)]
pub enum EncryptionError {
    #[error("Key derivation failed")]
    KeyDerivationFailed,

    #[error("Invalid encryption key")]
    InvalidKey,

    #[error("Encryption operation failed")]
    EncryptionFailed,

    #[error("Decryption failed - wrong password or corrupted data")]
    DecryptionFailed,
}

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

        let ciphertext = encrypt_data(plaintext, &key, &nonce).unwrap();
        assert_ne!(ciphertext.as_slice(), plaintext); // Should be encrypted

        let decrypted = decrypt_data(&ciphertext, &key, &nonce).unwrap();
        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_wrong_key_fails() {
        let key1 = EncryptionKey::from_bytes([1u8; 32]);
        let key2 = EncryptionKey::from_bytes([2u8; 32]);
        let nonce = [3u8; 24];
        let plaintext = b"Secret data";

        let ciphertext = encrypt_data(plaintext, &key1, &nonce).unwrap();

        // Decryption with wrong key should fail
        let result = decrypt_data(&ciphertext, &key2, &nonce);
        assert!(result.is_err());
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
}
