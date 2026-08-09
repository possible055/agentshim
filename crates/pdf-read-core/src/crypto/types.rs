//! Algorithm enums and value types shared by every [`CryptoProvider`]
//! implementation.
//!
//! These types are intentionally small and `Copy` where possible so the
//! trait surface stays cheap to call. Anything that needs heap data
//! (RSA modulus, X.509 cert bytes) is passed by reference.

/// Hash algorithms PDF and CMS care about.
///
/// PDF Standard Security R≤4 hard-requires MD5 (ISO 32000-1 §7.6.3
/// Algorithms 2/3/4/5). PKCS#7 / CMS signatures use SHA-1, SHA-256,
/// SHA-384, SHA-512 (ISO 32000-1 §12.8.3 Table 252). RIPEMD-160 is
/// listed in the spec but pdf_oxide does not currently support it; if
/// a downstream provider implements it, add a variant here behind a
/// minor-version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashAlgorithm {
    /// MD5 — legacy. Required for PDF R≤4 password derivation.
    /// FIPS 140-3 forbids MD5 for any use; FIPS providers reject this.
    Md5,
    /// SHA-256 — FIPS 140-3 approved.
    Sha256,
    /// SHA-384 — FIPS 140-3 approved.
    Sha384,
    /// SHA-512 — FIPS 140-3 approved.
    Sha512,
}

impl HashAlgorithm {
    /// Output size in bytes (matches `hash`-style crates' `OutputSize`).
    pub const fn output_size(self) -> usize {
        match self {
            HashAlgorithm::Md5 => 16,
            HashAlgorithm::Sha256 => 32,
            HashAlgorithm::Sha384 => 48,
            HashAlgorithm::Sha512 => 64,
        }
    }

    /// Human-readable name used in error messages and audit logs.
    pub const fn name(self) -> &'static str {
        match self {
            HashAlgorithm::Md5 => "MD5",
            HashAlgorithm::Sha256 => "SHA-256",
            HashAlgorithm::Sha384 => "SHA-384",
            HashAlgorithm::Sha512 => "SHA-512",
        }
    }

    /// Whether this hash is FIPS 140-3 approved for new use.
    /// SHA-1 is allowed for verify-only by some FIPS deployments
    /// (NIST SP 800-131A) but not for signing — that policy decision
    /// is made by the provider, not the algorithm enum.
    pub const fn is_fips_approved(self) -> bool {
        matches!(
            self,
            HashAlgorithm::Sha256 | HashAlgorithm::Sha384 | HashAlgorithm::Sha512
        )
    }
}

/// Padding mode for AES-CBC.
///
/// PDF stream/string encryption (V≥4) uses PKCS#7 padding. Algorithm
/// 2.B inner encryption and the V=5 R=5/6 key-wrap (UE/OE entries)
/// require **no padding** — the caller pre-pads to a 16-byte multiple
/// or supplies exactly two blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Padding {
    /// PKCS#7 padding (PDF V≥4 stream/string encryption).
    Pkcs7,
    /// No padding — input must be a 16-byte multiple. PDF Algorithm
    /// 2.B and V=5 R=5/6 UE/OE key wrap.
    None,
}

/// AES key size in bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AesKeySize {
    /// 128-bit key (16 bytes). PDF V=4, R=4 (`/CFM AESV2`).
    Aes128,
    /// 256-bit key (32 bytes). PDF V=5, R=5/6 (`/CFM AESV3`).
    Aes256,
}

impl AesKeySize {
    /// Key length in bytes (16 for AES-128, 32 for AES-256).
    pub const fn key_bytes(self) -> usize {
        match self {
            AesKeySize::Aes128 => 16,
            AesKeySize::Aes256 => 32,
        }
    }
    /// Human-readable algorithm name (`"AES-128"` / `"AES-256"`).
    pub const fn name(self) -> &'static str {
        match self {
            AesKeySize::Aes128 => "AES-128",
            AesKeySize::Aes256 => "AES-256",
        }
    }
}
