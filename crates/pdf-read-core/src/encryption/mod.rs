//! PDF encryption support.
//!
//! This module implements PDF encryption/decryption according to the PDF specification
//! (ISO 32000-1:2008, Section 7.6). It supports:
//!
//! - RC4 encryption (40-bit and 128-bit) for PDF 1.4-1.5
//! - AES encryption (128-bit and 256-bit) for PDF 1.6+
//! - Standard Security Handler (password validation, permissions)
//!
//! # Encryption Algorithms
//!
//! ## RC4 (PDF 1.4-1.5)
//! - RC4-40: 40-bit key length (weak, legacy)
//! - RC4-128: 128-bit key length
//!
//! ## AES (PDF 1.6+)
//! - AES-128: 128-bit key length with CBC mode
//! - AES-256: 256-bit key length with CBC mode (PDF 2.0)
//!
//! # Security Considerations
//!
//! - RC4-40 is cryptographically weak and should only be used for legacy documents
//! - Password validation uses constant-time comparison to prevent timing attacks
//! - Key derivation follows PDF specification algorithms (using MD5 or SHA-256)
//!
//! # References
//!
//! - PDF Spec Section 7.6: Encryption
//! - PDF Spec Section 7.6.3: Standard Security Handler
//! - PDF Spec Section 7.6.5: Algorithm 2 (Key Derivation)

use crate::error::{Error, Result};
use crate::object::Object;

mod aes;
mod algorithms;
mod handler;
pub mod permissions;
// `pub(crate)` so the `crypto::RustCryptoProvider::SymmetricCipher`
// impl in `src/crypto/rust_provider.rs` can call
// `rc4::rc4_crypt_impl` (the cipher-only entry point that does NOT
// re-route through the active provider, breaking the
// `provider.rc4() → rc4_crypt → provider.rc4()` cycle). RC4 is
// required by PDF Standard Security Handler R≤4 (ISO 32000-1 §7.6.3).
pub(crate) mod rc4;

pub use handler::EncryptionHandler;
pub use permissions::PdfPermissions;

/// A fresh MD5 hasher for the ISO 32000-1 §7.6.3 Standard-Security-
/// Handler password key-derivation, obtained through the active
/// [`crate::crypto`] provider (#230 Phase C).
///
/// Routing the *primitive* through the provider — not just the
/// operation gate in [`handler::EncryptionHandler::new`] /
/// [`write_handler`] — lets a `strict`/`fips-strict`
/// `SecurityPolicy` (or a FIPS provider) deny weak-crypto key
/// derivation at the boundary it actually happens, closing the
/// "operation-gated but primitive-ungated" gap (feature-230 §3.5 risk
/// table). Under the default `compat` policy the returned hasher is
/// the same `md5::Md5`, so every existing encrypted PDF still decrypts
/// and newly written ones are **byte-for-byte unchanged**.
///
/// The *non-security* opaque MD5 uses — the file identifier
/// ([`generate_file_id`]) and the embedded-file `/CheckSum`
/// (`writer::embedded_files`) — are deliberately **not** routed here:
/// they are not security decisions, and `AlgorithmUse` has no
/// non-security variant, so gating them would make a strict policy
/// refuse to write otherwise-compliant AES-256 documents. They remain
/// direct `md5` calls (feature-230 §2.2).
///
/// # Errors
/// Maps the provider/policy denial
/// ([`crate::crypto::Error::AlgorithmNotPermitted`]) to a clear
/// [`Error::InvalidPdf`], consistent with the existing R≤4 / legacy
/// rejection messages in this module.
#[cfg(feature = "legacy-crypto")]
pub(crate) fn md5_kdf_hasher() -> Result<Box<dyn crate::crypto::Hasher>> {
    crate::crypto::active()
        .hasher(crate::crypto::HashAlgorithm::Md5)
        .map_err(|e| {
            Error::InvalidPdf(format!(
                "MD5 key derivation is not permitted by the active crypto \
                 policy/provider (ISO 32000-1 §7.6.3 Standard Security \
                 Handler, R≤4): {e}"
            ))
        })
}

/// Encryption algorithm used in the PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// No encryption
    None,
    /// RC4 with 40-bit key (PDF 1.4, V=1, R=2)
    RC4_40,
    /// RC4 with 128-bit key (PDF 1.5, V=2, R=3)
    Rc4_128,
    /// AES with 128-bit key in CBC mode (PDF 1.6, V=4, R=4)
    Aes128,
    /// AES with 256-bit key in CBC mode (PDF 2.0, V=5, R=5/6)
    Aes256,
}

impl Algorithm {
    /// Get the key length in bytes for this algorithm.
    pub fn key_length(&self) -> usize {
        match self {
            Algorithm::None => 0,
            Algorithm::RC4_40 => 5,   // 40 bits = 5 bytes
            Algorithm::Rc4_128 => 16, // 128 bits = 16 bytes
            Algorithm::Aes128 => 16,  // 128 bits = 16 bytes
            Algorithm::Aes256 => 32,  // 256 bits = 32 bytes
        }
    }

    /// Check if this is an AES algorithm.
    pub fn is_aes(&self) -> bool {
        matches!(self, Algorithm::Aes128 | Algorithm::Aes256)
    }

    /// Check if this is an RC4 algorithm.
    pub fn is_rc4(&self) -> bool {
        matches!(self, Algorithm::RC4_40 | Algorithm::Rc4_128)
    }
}

/// PDF encryption dictionary (/Encrypt entry in trailer).
///
/// PDF Spec: Section 7.6.1 - General
#[derive(Debug, Clone)]
pub struct EncryptDict {
    /// Filter name (should be "Standard")
    pub filter: String,
    /// SubFilter name (optional, for public-key security)
    pub sub_filter: Option<String>,
    /// Algorithm version (V): 1=RC4-40, 2=RC4-128, 4=AES-128, 5=AES-256
    pub version: u32,
    /// Key length in bits (Length): 40-128 for RC4, 128/256 for AES
    pub length: Option<u32>,
    /// Revision number (R): 2, 3, 4, 5, or 6
    pub revision: u32,
    /// Owner password hash (O): 32 or 48 bytes
    pub owner_password: Vec<u8>,
    /// User password hash (U): 32 or 48 bytes
    pub user_password: Vec<u8>,
    /// User permissions (P): 32-bit integer
    pub permissions: i32,
    /// Encrypt metadata flag (EncryptMetadata): true by default
    pub encrypt_metadata: bool,
    /// Additional encryption parameters (for V=5/R=6)
    pub owner_encryption: Option<Vec<u8>>, // OE
    /// User encryption key (UE entry, for V=5/R=6)
    pub user_encryption: Option<Vec<u8>>, // UE
    /// Encrypted permissions (Perms entry, for V=5/R=6)
    pub perms: Option<Vec<u8>>, // Perms (encrypted permissions)
    /// Stream crypt filter method (CFM from /CF dictionary, for V=4).
    /// "V2" = RC4-128, "AESV2" = AES-128. None means not specified (defaults to AES-128).
    pub stream_crypt_method: Option<String>,
}

impl EncryptDict {
    /// Parse an encryption dictionary from a PDF object.
    ///
    /// PDF Spec: Section 7.6.1 - General
    pub fn from_object(obj: &Object) -> Result<Self> {
        let dict = obj
            .as_dict()
            .ok_or_else(|| Error::InvalidPdf("Encrypt entry is not a dictionary".to_string()))?;

        // Extract required fields
        let filter = dict
            .get("Filter")
            .and_then(|o| o.as_name())
            .ok_or_else(|| Error::InvalidPdf("Encrypt dictionary missing /Filter".to_string()))?
            .to_string();

        let version = dict
            .get("V")
            .and_then(|o| match o {
                Object::Integer(i) => Some(*i as u32),
                _ => None,
            })
            .ok_or_else(|| Error::InvalidPdf("Encrypt dictionary missing /V".to_string()))?;

        let revision = dict
            .get("R")
            .and_then(|o| match o {
                Object::Integer(i) => Some(*i as u32),
                _ => None,
            })
            .ok_or_else(|| Error::InvalidPdf("Encrypt dictionary missing /R".to_string()))?;

        let owner_password = dict
            .get("O")
            .and_then(|o| o.as_string())
            .ok_or_else(|| Error::InvalidPdf("Encrypt dictionary missing /O".to_string()))?
            .to_vec();

        let user_password = dict
            .get("U")
            .and_then(|o| o.as_string())
            .ok_or_else(|| Error::InvalidPdf("Encrypt dictionary missing /U".to_string()))?
            .to_vec();

        let permissions = dict
            .get("P")
            .and_then(|o| match o {
                Object::Integer(i) => Some(*i as i32),
                _ => None,
            })
            .ok_or_else(|| Error::InvalidPdf("Encrypt dictionary missing /P".to_string()))?;

        // Optional fields
        let sub_filter = dict
            .get("SubFilter")
            .and_then(|o| o.as_name())
            .map(|s| s.to_string());

        let length = dict.get("Length").and_then(|o| match o {
            Object::Integer(i) => Some(*i as u32),
            _ => None,
        });

        let encrypt_metadata = dict
            .get("EncryptMetadata")
            .and_then(|o| match o {
                Object::Boolean(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true);

        // V=5/R=6 additional fields
        let owner_encryption = dict
            .get("OE")
            .and_then(|o| o.as_string())
            .map(|s| s.to_vec());

        let user_encryption = dict
            .get("UE")
            .and_then(|o| o.as_string())
            .map(|s| s.to_vec());

        let perms = dict
            .get("Perms")
            .and_then(|o| o.as_string())
            .map(|s| s.to_vec());

        // V=4: Parse /CF crypt filter dictionary to determine the actual algorithm.
        // /StmF names the crypt filter for streams; look up its /CFM entry.
        // CFM "V2" = RC4-128, "AESV2" = AES-128 (PDF Spec Table 25).
        let stream_crypt_method = if version == 4 {
            let stm_filter_name = dict
                .get("StmF")
                .and_then(|o| o.as_name())
                .unwrap_or("Identity");
            dict.get("CF")
                .and_then(|cf| cf.as_dict())
                .and_then(|cf_dict| cf_dict.get(stm_filter_name))
                .and_then(|filter_obj| filter_obj.as_dict())
                .and_then(|filter_dict| filter_dict.get("CFM"))
                .and_then(|cfm| cfm.as_name())
                .map(|s| s.to_string())
        } else {
            None
        };

        Ok(EncryptDict {
            filter,
            sub_filter,
            version,
            length,
            revision,
            owner_password,
            user_password,
            permissions,
            encrypt_metadata,
            owner_encryption,
            user_encryption,
            perms,
            stream_crypt_method,
        })
    }

    /// Determine the encryption algorithm from V and R values.
    ///
    /// PDF Spec: Table 20 - Encryption dictionary entries
    pub fn algorithm(&self) -> Result<Algorithm> {
        match (self.version, self.revision) {
            (1, 2) => Ok(Algorithm::RC4_40),
            (2, 3) => Ok(Algorithm::Rc4_128),
            (4, _) => {
                // V=4 means crypt-filter-based encryption. The actual algorithm
                // is determined by /CFM in the /CF dictionary (PDF Spec Table 25):
                //   "V2" = RC4-128
                //   "AESV2" = AES-128
                match self.stream_crypt_method.as_deref() {
                    Some("V2") => {
                        log::info!("V=4 R={}: CFM=V2 → RC4-128", self.revision);
                        Ok(Algorithm::Rc4_128)
                    }
                    Some("AESV2") | None => {
                        // None: /CF missing or unparseable — default to AES-128
                        // for backward compatibility.
                        Ok(Algorithm::Aes128)
                    }
                    Some(other) => {
                        log::warn!(
                            "V=4 R={}: unknown CFM '{}', falling back to AES-128",
                            self.revision,
                            other
                        );
                        Ok(Algorithm::Aes128)
                    }
                }
            }
            (5, 5) | (5, 6) => Ok(Algorithm::Aes256),
            // Lenient: V determines algorithm, R may be non-standard
            (1, r) => {
                log::warn!("Non-standard encryption V=1, R={}, using RC4-40", r);
                Ok(Algorithm::RC4_40)
            }
            (2, r) => {
                log::warn!("Non-standard encryption V=2, R={}, using RC4-128", r);
                Ok(Algorithm::Rc4_128)
            }
            _ => Err(Error::Unsupported(format!(
                "Unsupported encryption version V={}, R={}",
                self.version, self.revision
            ))),
        }
    }

    /// Get the effective key length in bytes.
    pub fn key_length_bytes(&self) -> usize {
        if let Some(length) = self.length {
            (length / 8) as usize
        } else {
            // Default key lengths based on version
            match self.version {
                1 => 5,  // 40 bits
                2 => 16, // 128 bits
                4 => 16, // 128 bits (AES-128)
                5 => 32, // 256 bits (AES-256)
                _ => 16, // Default to 128 bits
            }
        }
    }
}

/// PDF encryption permissions (P field).
///
/// PDF Spec: Table 22 - User access permissions
#[derive(Debug, Clone, Copy)]
pub struct Permissions {
    bits: i32,
}

impl Permissions {
    /// Create permissions from the P field value.
    pub fn from_bits(bits: i32) -> Self {
        Self { bits }
    }

    /// Decoded view per [`PdfPermissions`]. The bit decoding lives
    /// in one place; this method-style API delegates so the two
    /// decoders cannot drift apart.
    #[inline]
    fn decoded(&self) -> PdfPermissions {
        PdfPermissions::from_p_flag(self.bits)
    }

    /// Check if printing is allowed.
    pub fn can_print(&self) -> bool {
        self.decoded().print_low_res
    }

    /// Check if modifying the document is allowed.
    pub fn can_modify(&self) -> bool {
        self.decoded().modify
    }

    /// Check if copying text/graphics is allowed.
    pub fn can_copy(&self) -> bool {
        self.decoded().copy
    }

    /// Check if adding/modifying annotations is allowed.
    pub fn can_annotate(&self) -> bool {
        self.decoded().annotate
    }

    /// Check if filling form fields is allowed (R>=3).
    pub fn can_fill_forms(&self) -> bool {
        self.decoded().fill_forms
    }

    /// Check if content extraction for accessibility is allowed (R>=3).
    pub fn can_extract_accessibility(&self) -> bool {
        self.decoded().accessibility
    }

    /// Check if assembling the document is allowed (R>=3).
    pub fn can_assemble(&self) -> bool {
        self.decoded().assemble
    }

    /// Check if high-quality printing is allowed (R>=3).
    pub fn can_print_high_quality(&self) -> bool {
        self.decoded().print_high_res
    }
}
