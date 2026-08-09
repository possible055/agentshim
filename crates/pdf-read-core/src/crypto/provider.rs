//! The [`CryptoProvider`] trait family.
//!
//! These traits decouple PDF encryption and signature paths from any
//! one cryptography crate, so deployments that need a FIPS 140-3
//! validated module (`aws-lc-rs` with the `fips` feature) or a
//! sovereign-jurisdiction provider (GOST R 34.11/34.10, Chinese
//! SM2/SM3/SM4) can swap in a different backend without touching the
//! parsing or signature-construction code.
//!
//! See `docs/CRYPTO_PROVIDERS.md` (added in Phase 8) for the
//! end-to-end story; tracking issue #236.
//!
//! # Trait shape
//!
//! Three sub-traits handle independent concerns:
//!
//! - [`Hasher`] — incremental hashing (`update` / `finalize`).
//! - [`SymmetricCipher`] — AES-CBC (PKCS#7 and no-padding) and RC4.
//! - [`SignatureVerifier`] — RSA-PKCS#1-v1.5 / RSA-PSS / ECDSA verify.
//! - [`Signer`] — opaque signing handle (decouples PEM/DER loading
//!   from the call site so HSM / PKCS#11 providers can plug in).
//!
//! [`CryptoProvider`] composes them and adds policy
//! ([`is_legacy_allowed`]) plus secure RNG.
//!
//! # FIPS posture
//!
//! Every provider documents what it permits via
//! [`CryptoProvider::is_legacy_allowed`]. When `false`, MD5, SHA-1
//! signing, RC4, and RSA-PKCS#1-v1.5 with SHA-1 return
//! [`Error::AlgorithmNotPermitted`]. SHA-1 *verification* of
//! historical signatures is permitted (NIST SP 800-131A) — the policy
//! split happens in [`SignatureVerifier`] vs [`Signer`].

use super::error::Result;
use super::types::{AesKeySize, HashAlgorithm, Padding};

/// Incremental hashing.
///
/// Modeled after the `digest` crate's `DynDigest` so providers can
/// trivially adapt — but stripped to just the operations PDF needs
/// (no XOF, no variable-output, no reset).
pub trait Hasher: Send {
    /// Feed input into the hash state. May be called any number of
    /// times before [`Self::finalize`].
    fn update(&mut self, data: &[u8]);

    /// Finalize the hash, consuming `self`. The returned `Vec` is
    /// exactly [`HashAlgorithm::output_size`] bytes long.
    ///
    /// Boxed receiver lets implementors live behind `Box<dyn Hasher>`
    /// without paying for `Sized` constraints up the call stack.
    fn finalize(self: Box<Self>) -> Vec<u8>;

    /// Reports the algorithm so callers can sanity-check the output
    /// size or feed the right OID into a CMS construction.
    fn algorithm(&self) -> HashAlgorithm;
}

/// Symmetric encryption operations PDF needs.
///
/// All methods return owned `Vec<u8>` to match the existing
/// `src/encryption/aes.rs` / `src/encryption/rc4.rs` shape so Phase 3
/// migration is mechanical. Performance-critical callers can be
/// converted to streaming later (in-place CBC, etc.) without breaking
/// the trait — adding methods is non-breaking.
pub trait SymmetricCipher: Send + Sync {
    /// AES-CBC encrypt.
    ///
    /// `key.len()` must equal `key_size.key_bytes()`; `iv.len()` must
    /// be 16. With [`Padding::None`], `data.len()` must be a multiple
    /// of 16.
    fn aes_cbc_encrypt(
        &self,
        key_size: AesKeySize,
        key: &[u8],
        iv: &[u8],
        data: &[u8],
        padding: Padding,
    ) -> Result<Vec<u8>>;

    /// AES-CBC decrypt. Same argument constraints as
    /// [`Self::aes_cbc_encrypt`].
    fn aes_cbc_decrypt(
        &self,
        key_size: AesKeySize,
        key: &[u8],
        iv: &[u8],
        data: &[u8],
        padding: Padding,
    ) -> Result<Vec<u8>>;

    /// RC4 encrypt/decrypt (the operation is symmetric so one method
    /// covers both directions).
    ///
    /// Required for PDF Standard Security R≤4 (ISO 32000-1 §7.6.3).
    /// Returns [`super::error::Error::AlgorithmNotPermitted`] under FIPS providers.
    fn rc4(&self, key: &[u8], data: &[u8]) -> Result<Vec<u8>>;
}
