//! Cryptographic provider abstraction.
//!
//! This module decouples PDF encryption and signature paths from any
//! one cryptography crate so deployments can choose between:
//!
//! - **`RustCryptoProvider`** (default) — built on `sha2`, `sha1`,
//!   `md-5`, `aes`, `rsa`, `p256`, `p384`, `getrandom`. Permits all
//!   PDF-spec-required algorithms including the legacy MD5+RC4 path
//!   needed for ISO 32000-1 R≤4 documents.
//! - **`AwsLcProvider`** (Phase 6, behind `--features fips`)
//!   — built on `aws-lc-rs` with the `fips` feature. FIPS 140-3
//!   validated since 2024. Refuses MD5, RC4, and SHA-1-for-signing.
//!
//! Downstream consumers can also implement [`CryptoProvider`] for
//! HSM/PKCS#11 backends, sovereign-jurisdiction algorithms (GOST,
//! SM2/3/4), or hardware-rooted Cloud KMS providers.
//!
//! Tracking issue: <https://github.com/yfedoseev/pdf_oxide/issues/236>.

mod active;
mod error;
mod provider;
mod rust_provider;
mod types;

pub(crate) use active::active;
pub(crate) use error::Error;
pub(crate) use provider::{Hasher, SymmetricCipher};
pub(crate) use types::{AesKeySize, HashAlgorithm, Padding};
