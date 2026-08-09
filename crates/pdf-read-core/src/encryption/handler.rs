//! Encryption handler for PDF documents.
//!
//! This module provides the main interface for handling encrypted PDFs,
//! including password authentication and stream/string decryption.

use super::algorithms;
use super::{Algorithm, EncryptDict, Permissions};
use crate::error::{Error, Result};
use crate::object::Object;

/// Main encryption handler for PDF documents.
///
/// This struct manages the encryption state and provides methods for
/// authenticating passwords and decrypting content.
#[derive(Debug, Clone)]
pub struct EncryptionHandler {
    /// Encryption dictionary
    dict: EncryptDict,
    /// Derived encryption key (set after successful authentication)
    encryption_key: Option<Vec<u8>>,
    /// File identifier (first element of /ID array)
    file_id: Vec<u8>,
    /// Encryption algorithm
    algorithm: Algorithm,
}

impl EncryptionHandler {
    /// Create a new encryption handler from an encryption dictionary.
    ///
    /// # Arguments
    ///
    /// * `encrypt_obj` - The /Encrypt dictionary object from the PDF trailer
    /// * `file_id` - The first element of the /ID array from the PDF trailer
    ///
    /// # Returns
    ///
    /// An encryption handler ready for password authentication
    pub fn new(encrypt_obj: &Object, file_id: Vec<u8>) -> Result<Self> {
        let dict = EncryptDict::from_object(encrypt_obj)?;
        let algorithm = dict.algorithm()?;

        log::info!(
            "PDF is encrypted with {:?} (V={}, R={})",
            algorithm,
            dict.version,
            dict.revision
        );

        // Compile-time gate: when legacy-crypto is disabled, R≤4 is unconditionally
        // unsupported since MD5 key-derivation is not compiled in.
        #[cfg(not(feature = "legacy-crypto"))]
        if dict.revision <= 4 {
            return Err(Error::InvalidPdf(format!(
                "pdf_oxide compiled without 'legacy-crypto': PDF Standard Security R={} \
                 (R≤4 requires MD5 key derivation) is not supported. \
                 Add `features = [\"legacy-crypto\"]` (the default) to re-enable.",
                dict.revision
            )));
        }

        Ok(Self {
            dict,
            encryption_key: None,
            file_id,
            algorithm,
        })
    }

    /// Authenticate with a password.
    ///
    /// This attempts to authenticate with the given password as either
    /// a user password or owner password. If successful, the encryption
    /// key is derived and stored for future decryption operations.
    ///
    /// # Arguments
    ///
    /// * `password` - The password to authenticate (empty string for no password)
    ///
    /// # Returns
    ///
    /// `Ok(true)` if authentication succeeded, `Ok(false)` if it failed,
    /// or an error if the encryption is unsupported.
    pub fn authenticate(&mut self, password: &[u8]) -> Result<bool> {
        // Try authenticating as user password
        if let Some(key) = algorithms::authenticate_user_password(
            password,
            &self.dict.user_password,
            &self.dict.owner_password,
            self.dict.permissions,
            &self.file_id,
            self.dict.revision,
            self.dict.key_length_bytes(),
            self.dict.encrypt_metadata,
            self.dict.user_encryption.as_deref(),
        ) {
            self.encryption_key = Some(key);
            log::info!("Successfully authenticated with user password");
            return Ok(true);
        }

        // Try authenticating as owner password (Algorithm 7 for R≤4, Algorithm 12 for R≥5)
        if let Some(key) = algorithms::authenticate_owner_password(
            password,
            &self.dict.user_password,
            &self.dict.owner_password,
            self.dict.permissions,
            &self.file_id,
            self.dict.revision,
            self.dict.key_length_bytes(),
            self.dict.encrypt_metadata,
            self.dict.owner_encryption.as_deref(),
        )? {
            self.encryption_key = Some(key);
            log::info!("Successfully authenticated with owner password");
            return Ok(true);
        }

        log::warn!("Password authentication failed");
        Ok(false)
    }

    /// Check if the handler has been authenticated.
    pub fn is_authenticated(&self) -> bool {
        self.encryption_key.is_some()
    }

    /// Get the encryption key (if authenticated).
    pub fn encryption_key(&self) -> Option<&[u8]> {
        self.encryption_key.as_deref()
    }

    /// Get the permissions.
    pub fn permissions(&self) -> Permissions {
        Permissions::from_bits(self.dict.permissions)
    }

    /// Get the raw `/P` permission flag integer per PDF spec §7.6.3.2
    /// Table 22. Used by [`crate::document::PdfDocument::permissions`]
    /// to construct the [`crate::encryption::permissions::PdfPermissions`]
    /// struct.
    ///
    /// PDF spec uses two's-complement int32 with bits 13-32 reserved.
    pub fn raw_permissions(&self) -> i32 {
        self.dict.permissions
    }

    /// Get the encryption algorithm.
    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Decrypt a stream using the encryption key.
    ///
    /// PDF Spec: Section 7.6.2 - General Encryption Algorithm
    ///
    /// # Arguments
    ///
    /// * `data` - The encrypted stream data
    /// * `obj_num` - Object number
    /// * `gen_num` - Generation number
    ///
    /// # Returns
    ///
    /// The decrypted stream data, or an error if decryption fails
    pub fn decrypt_stream(&self, data: &[u8], obj_num: u32, gen_num: u32) -> Result<Vec<u8>> {
        let key = self
            .encryption_key
            .as_ref()
            .ok_or_else(|| Error::InvalidPdf("Not authenticated".to_string()))?;

        // Decrypt based on algorithm
        match self.algorithm {
            Algorithm::None => return Ok(data.to_vec()),
            Algorithm::Aes256 => {
                // AES-256 uses the file encryption key directly (no per-object key derivation)
                // per ISO 32000-2:2020 Section 7.6.3.3
                if key.len() < 32 {
                    return Err(Error::InvalidPdf(format!(
                        "AES-256 file key too short: {} bytes (need 32)",
                        key.len()
                    )));
                }
                if data.len() < 16 {
                    return Err(Error::InvalidPdf(
                        "AES encrypted data too short".to_string(),
                    ));
                }
                let (iv, ciphertext) = data.split_at(16);
                return super::aes::aes256_decrypt(&key[..32], iv, ciphertext)
                    .map_err(|e| Error::InvalidPdf(format!("AES-256 decryption failed: {}", e)));
            }
            _ => {}
        }

        // Per-object key derivation via MD5 — required for RC4 and AES-128 (R≤4).
        let obj_key = self.compute_object_key(key, obj_num, gen_num)?;

        // Decrypt based on algorithm (AES-256 and None already handled above)
        match self.algorithm {
            Algorithm::RC4_40 | Algorithm::Rc4_128 => super::rc4::rc4_crypt(&obj_key, data),
            Algorithm::Aes128 => {
                if obj_key.len() < 16 {
                    return Err(Error::InvalidPdf(format!(
                        "AES-128 object key too short: {} bytes (need 16)",
                        obj_key.len()
                    )));
                }
                if data.len() < 16 {
                    return Err(Error::InvalidPdf(
                        "AES encrypted data too short".to_string(),
                    ));
                }
                let (iv, ciphertext) = data.split_at(16);
                super::aes::aes128_decrypt(&obj_key[..16], iv, ciphertext)
                    .map_err(|e| Error::InvalidPdf(format!("AES-128 decryption failed: {}", e)))
            }
            Algorithm::None | Algorithm::Aes256 => unreachable!("handled above"),
        }
    }

    /// Decrypt a string using the encryption key.
    ///
    /// # Arguments
    ///
    /// * `data` - The encrypted string data
    /// * `obj_num` - Object number
    /// * `gen_num` - Generation number
    ///
    /// # Returns
    ///
    /// The decrypted string data
    pub fn decrypt_string(&self, data: &[u8], obj_num: u32, gen_num: u32) -> Result<Vec<u8>> {
        // Strings are decrypted the same way as streams
        self.decrypt_stream(data, obj_num, gen_num)
    }

    /// Compute the object-specific encryption key.
    ///
    /// PDF Spec: Algorithm 1 - Encryption key algorithm
    ///
    /// # Arguments
    ///
    /// * `base_key` - The base encryption key
    /// * `obj_num` - Object number
    /// * `gen_num` - Generation number
    ///
    /// # Returns
    ///
    /// The object-specific key
    #[cfg_attr(not(feature = "legacy-crypto"), allow(unused_variables))]
    fn compute_object_key(&self, base_key: &[u8], obj_num: u32, gen_num: u32) -> Result<Vec<u8>> {
        // Unreachable when legacy-crypto is off: handler::new() rejects R≤4.
        #[cfg(not(feature = "legacy-crypto"))]
        return Err(Error::InvalidPdf(
            "compute_object_key requires legacy-crypto feature (MD5)".to_string(),
        ));

        #[cfg(feature = "legacy-crypto")]
        {
            // #230 Phase C: route the Algorithm-1 per-object-key MD5
            // through the governed provider (byte-identical under the
            // default `compat` policy; a strict/FIPS policy denies it
            // at the primitive, not just the operation gate).
            let mut hasher = super::md5_kdf_hasher()?;

            // Step a: Extend key with object/generation number
            hasher.update(base_key);
            hasher.update(&obj_num.to_le_bytes()[..3]); // Low 3 bytes
            hasher.update(&gen_num.to_le_bytes()[..2]); // Low 2 bytes

            // Step b: For AES, add "sAlT" string
            if self.algorithm.is_aes() {
                hasher.update(b"sAlT");
            }

            // Step c: MD5 hash
            let hash = hasher.finalize();

            // Step d: Key is first (n + 5) bytes, max 16
            let key_len = (base_key.len() + 5).min(16);
            Ok(hash[..key_len].to_vec())
        }
    }
}
