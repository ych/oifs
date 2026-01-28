use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng},
    XChaCha20Poly1305, XNonce,
};
use argon2::{Argon2, PasswordHasher};
use argon2::password_hash::{PasswordHash, SaltString};
use zeroize::{Zeroize, ZeroizeOnDrop};
use std::fmt;

/// Encryption key with automatic zeroization on drop
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct EncryptionKey {
    key: [u8; 32],  // 256-bit key for XChaCha20-Poly1305
}

impl EncryptionKey {
    /// Create a new encryption key from raw bytes
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self { key: bytes }
    }

    /// Get key bytes (internal use only)
    fn as_bytes(&self) -> &[u8; 32] {
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
    let salt_string = SaltString::encode_b64(salt)
        .map_err(|_| EncryptionError::KeyDerivationFailed)?;
    
    // Derive key using Argon2id
    let password_hash = argon2
        .hash_password(password.as_bytes(), &salt_string)
        .map_err(|_| EncryptionError::KeyDerivationFailed)?;
    
    // Extract the hash output as our encryption key
    let hash = password_hash.hash
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
        assert_ne!(ciphertext.as_slice(), plaintext);  // Should be encrypted
        
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
}
