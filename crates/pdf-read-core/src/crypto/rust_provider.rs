//! Default [`CryptoProvider`] backed by the existing RustCrypto
//! crate stack — `sha2`, `sha1`, `md-5`, `aes`, `cbc`, `rsa`,
//! `p256`, `p384`, `getrandom`.
//!
//! This provider exists primarily so the rest of the crate can route
//! all crypto operations through the trait without changing
//! observable behaviour. Phases 3 and 4 use this to keep the existing
//! 86-PDF cross-build sweep at 888 / 888 byte-equal during the
//! migration.
//!
//! [`is_legacy_allowed`] returns `true`: every algorithm PDF specs
//! reference is permitted, including MD5, SHA-1 (sign and verify),
//! and RC4. Use [`super::AwsLcProvider`] (Phase 6) for FIPS-validated
//! deployments.
//!
//! [`is_legacy_allowed`]: super::CryptoProvider::is_legacy_allowed
//! [`super::AwsLcProvider`]: super::AwsLcProvider

use super::error::{Error, Result};
use super::provider::{Hasher, SymmetricCipher};
use super::types::{AesKeySize, HashAlgorithm, Padding};

/// The default Rust-only crypto provider.
///
/// Constructed via [`Self::new`]. Has no fields — providers are
/// stateless apart from any backend-specific initialization (which
/// the FIPS provider needs but this one doesn't).
#[derive(Debug, Default, Clone, Copy)]
pub struct RustCryptoProvider;

impl RustCryptoProvider {
    /// Create a new default-policy provider. Accepts every PDF-spec
    /// algorithm including the legacy MD5 / SHA-1 / RC4 paths.
    pub const fn new() -> Self {
        Self
    }
}

impl RustCryptoProvider {
    pub(crate) fn name(&self) -> &'static str {
        "rust-crypto"
    }

    pub(crate) fn is_legacy_allowed(&self) -> bool {
        cfg!(feature = "legacy-crypto")
    }

    pub(crate) fn hasher(&self, algo: HashAlgorithm) -> Result<Box<dyn Hasher>> {
        Ok(match algo {
            HashAlgorithm::Md5 => {
                #[cfg(feature = "legacy-crypto")]
                {
                    Box::new(Md5Hasher::new())
                }
                #[cfg(not(feature = "legacy-crypto"))]
                {
                    return Err(Error::AlgorithmNotPermitted {
                        kind: crate::crypto::error::AlgorithmKind::Hash,
                        name: "MD5",
                        reason: "legacy-crypto feature disabled at compile time",
                    });
                }
            }
            HashAlgorithm::Sha256 => Box::new(Sha256Hasher::new()),
            HashAlgorithm::Sha384 => Box::new(Sha384Hasher::new()),
            HashAlgorithm::Sha512 => Box::new(Sha512Hasher::new()),
        })
    }

    pub(crate) fn symmetric(&self) -> &dyn SymmetricCipher {
        &RustSymmetric
    }
}

// ---------------------------------------------------------------------------
// Hashers — one impl per algorithm. Boxed dispatch keeps the Hasher trait
// object-safe and avoids leaking generic digest::Update bounds out of the
// crypto module.
// ---------------------------------------------------------------------------

#[cfg(feature = "legacy-crypto")]
struct Md5Hasher(md5::Md5);
#[cfg(feature = "legacy-crypto")]
impl Md5Hasher {
    fn new() -> Self {
        use md5::Digest;
        Self(md5::Md5::new())
    }
}
#[cfg(feature = "legacy-crypto")]
impl Hasher for Md5Hasher {
    fn update(&mut self, data: &[u8]) {
        use md5::Digest;
        self.0.update(data);
    }
    fn finalize(self: Box<Self>) -> Vec<u8> {
        use md5::Digest;
        self.0.finalize().to_vec()
    }
    fn algorithm(&self) -> HashAlgorithm {
        HashAlgorithm::Md5
    }
}

macro_rules! sha2_hasher {
    ($name:ident, $inner:ty, $algo:expr) => {
        struct $name($inner);
        impl $name {
            fn new() -> Self {
                use sha2::Digest;
                Self(<$inner>::new())
            }
        }
        impl Hasher for $name {
            fn update(&mut self, data: &[u8]) {
                use sha2::Digest;
                self.0.update(data);
            }
            fn finalize(self: Box<Self>) -> Vec<u8> {
                use sha2::Digest;
                self.0.finalize().to_vec()
            }
            fn algorithm(&self) -> HashAlgorithm {
                $algo
            }
        }
    };
}

sha2_hasher!(Sha256Hasher, sha2::Sha256, HashAlgorithm::Sha256);
sha2_hasher!(Sha384Hasher, sha2::Sha384, HashAlgorithm::Sha384);
sha2_hasher!(Sha512Hasher, sha2::Sha512, HashAlgorithm::Sha512);

// ---------------------------------------------------------------------------
// Symmetric — AES-128/256-CBC (PKCS#7 + no-padding) and RC4.
// ---------------------------------------------------------------------------

struct RustSymmetric;

impl SymmetricCipher for RustSymmetric {
    fn aes_cbc_encrypt(
        &self,
        key_size: AesKeySize,
        key: &[u8],
        iv: &[u8],
        data: &[u8],
        padding: Padding,
    ) -> Result<Vec<u8>> {
        check_key_iv(key_size, key, iv)?;
        if matches!(padding, Padding::None) && !data.len().is_multiple_of(16) {
            return Err(Error::InvalidInput(
                "no-padding AES-CBC requires data length to be a 16-byte multiple",
            ));
        }
        match (key_size, padding) {
            (AesKeySize::Aes128, Padding::Pkcs7) => {
                aes_cbc_encrypt_pkcs7::<aes::Aes128>(key, iv, data)
            }
            (AesKeySize::Aes128, Padding::None) => {
                aes_cbc_encrypt_no_pad::<aes::Aes128>(key, iv, data)
            }
            (AesKeySize::Aes256, Padding::Pkcs7) => {
                aes_cbc_encrypt_pkcs7::<aes::Aes256>(key, iv, data)
            }
            (AesKeySize::Aes256, Padding::None) => {
                aes_cbc_encrypt_no_pad::<aes::Aes256>(key, iv, data)
            }
        }
    }

    fn aes_cbc_decrypt(
        &self,
        key_size: AesKeySize,
        key: &[u8],
        iv: &[u8],
        data: &[u8],
        padding: Padding,
    ) -> Result<Vec<u8>> {
        check_key_iv(key_size, key, iv)?;
        if !data.len().is_multiple_of(16) {
            return Err(Error::InvalidInput(
                "AES-CBC ciphertext must be a 16-byte multiple",
            ));
        }
        match (key_size, padding) {
            (AesKeySize::Aes128, Padding::Pkcs7) => {
                aes_cbc_decrypt_pkcs7::<aes::Aes128>(key, iv, data)
            }
            (AesKeySize::Aes128, Padding::None) => {
                aes_cbc_decrypt_no_pad::<aes::Aes128>(key, iv, data)
            }
            (AesKeySize::Aes256, Padding::Pkcs7) => {
                aes_cbc_decrypt_pkcs7::<aes::Aes256>(key, iv, data)
            }
            (AesKeySize::Aes256, Padding::None) => {
                aes_cbc_decrypt_no_pad::<aes::Aes256>(key, iv, data)
            }
        }
    }

    #[cfg_attr(not(feature = "legacy-crypto"), allow(unused_variables))]
    fn rc4(&self, key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        #[cfg(not(feature = "legacy-crypto"))]
        {
            Err(Error::AlgorithmNotPermitted {
                kind: crate::crypto::error::AlgorithmKind::SymmetricCipher,
                name: "RC4",
                reason: "legacy-crypto feature disabled at compile time",
            })
        }
        #[cfg(feature = "legacy-crypto")]
        {
            if key.is_empty() || key.len() > 256 {
                return Err(Error::InvalidInput("RC4 key must be 1..=256 bytes"));
            }
            // Calls the in-tree pure cipher impl directly (not the
            // `pub fn rc4_crypt` wrapper, which itself routes through us
            // — that would loop). Byte-equal to pre-Phase-3 output.
            Ok(crate::encryption::rc4::rc4_crypt_impl(key, data))
        }
    }
}

fn check_key_iv(key_size: AesKeySize, key: &[u8], iv: &[u8]) -> Result<()> {
    if key.len() != key_size.key_bytes() {
        return Err(Error::InvalidInput(match key_size {
            AesKeySize::Aes128 => "AES-128 requires a 16-byte key",
            AesKeySize::Aes256 => "AES-256 requires a 32-byte key",
        }));
    }
    if iv.len() != 16 {
        return Err(Error::InvalidInput("AES-CBC requires a 16-byte IV"));
    }
    Ok(())
}

// Generic over the block cipher so AES-128 and AES-256 share the body.

fn aes_cbc_encrypt_pkcs7<C>(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>>
where
    C: aes::cipher::BlockCipherEncrypt + aes::cipher::KeyInit,
{
    use aes::cipher::{block_padding::Pkcs7, BlockModeEncrypt, KeyIvInit};
    type Enc<C> = cbc::Encryptor<C>;
    let cipher = <Enc<C> as KeyIvInit>::new_from_slices(key, iv)
        .map_err(|_| Error::InvalidInput("AES-CBC key/iv length mismatch"))?;
    // `encrypt_padded` writes from `buf[..msg_len]` and adds padding
    // up to one extra block; size the buffer accordingly and copy the
    // plaintext into the prefix region first.
    let mut buf = vec![0u8; data.len() + 16];
    buf[..data.len()].copy_from_slice(data);
    let n = cipher
        .encrypt_padded::<Pkcs7>(&mut buf, data.len())
        .map_err(|_| Error::Backend("AES-CBC PKCS#7 encryption failed"))?
        .len();
    buf.truncate(n);
    Ok(buf)
}

fn aes_cbc_decrypt_pkcs7<C>(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>>
where
    C: aes::cipher::BlockCipherDecrypt + aes::cipher::KeyInit,
{
    use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};
    type Dec<C> = cbc::Decryptor<C>;
    let cipher = <Dec<C> as KeyIvInit>::new_from_slices(key, iv)
        .map_err(|_| Error::InvalidInput("AES-CBC key/iv length mismatch"))?;
    let mut buf = data.to_vec();
    let n = cipher
        .decrypt_padded::<Pkcs7>(&mut buf)
        .map_err(|_| Error::Verification("AES-CBC PKCS#7 padding invalid"))?
        .len();
    buf.truncate(n);
    Ok(buf)
}

fn aes_cbc_encrypt_no_pad<C>(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>>
where
    C: aes::cipher::BlockCipherEncrypt + aes::cipher::KeyInit,
{
    use aes::cipher::{block_padding::NoPadding, BlockModeEncrypt, KeyIvInit};
    type Enc<C> = cbc::Encryptor<C>;
    let cipher = <Enc<C> as KeyIvInit>::new_from_slices(key, iv)
        .map_err(|_| Error::InvalidInput("AES-CBC key/iv length mismatch"))?;
    let mut buf = data.to_vec();
    let len = data.len();
    cipher
        .encrypt_padded::<NoPadding>(&mut buf, len)
        .map_err(|_| Error::Backend("AES-CBC no-padding encryption failed"))?;
    Ok(buf)
}

fn aes_cbc_decrypt_no_pad<C>(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>>
where
    C: aes::cipher::BlockCipherDecrypt + aes::cipher::KeyInit,
{
    use aes::cipher::{block_padding::NoPadding, BlockModeDecrypt, KeyIvInit};
    type Dec<C> = cbc::Decryptor<C>;
    let cipher = <Dec<C> as KeyIvInit>::new_from_slices(key, iv)
        .map_err(|_| Error::InvalidInput("AES-CBC key/iv length mismatch"))?;
    let mut buf = data.to_vec();
    cipher
        .decrypt_padded::<NoPadding>(&mut buf)
        .map_err(|_| Error::Backend("AES-CBC no-padding decryption failed"))?;
    Ok(buf)
}
