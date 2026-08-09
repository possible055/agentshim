//! PDF encryption algorithms.
//!
//! This module implements the cryptographic algorithms specified in the PDF specification
//! for key derivation and password validation.
//!
//! PDF Spec: Section 7.6.3 - Standard Security Handler
//! PDF 2.0 Spec (ISO 32000-2:2020): Section 7.6.4.3.3 - Algorithm 8-11 for R>=5

use sha2::{Digest, Sha256, Sha384, Sha512};

/// Padding string used in PDF encryption (32 bytes).
///
/// PDF Spec: Algorithm 2, step 1
const PADDING: &[u8; 32] = b"\x28\xBF\x4E\x5E\x4E\x75\x8A\x41\
                              \x64\x00\x4E\x56\xFF\xFA\x01\x08\
                              \x2E\x2E\x00\xB6\xD0\x68\x3E\x80\
                              \x2F\x0C\xA9\xFE\x64\x53\x69\x7A";

/// Compute the encryption key from a password (Algorithm 2).
///
/// PDF Spec: Section 7.6.3.3 - Algorithm 2: Computing an encryption key
///
/// # Arguments
///
/// * `password` - User or owner password (up to 32 bytes)
/// * `owner_key` - 32-byte owner password hash from encryption dictionary
/// * `permissions` - User access permissions (P field)
/// * `file_id` - First element of file identifier array
/// * `revision` - Encryption revision number (R field)
/// * `key_length` - Key length in bytes
/// * `encrypt_metadata` - Whether to encrypt metadata
///
/// # Returns
///
/// The derived encryption key
#[cfg_attr(not(feature = "legacy-crypto"), allow(unused_variables))]
pub fn compute_encryption_key(
    password: &[u8],
    owner_key: &[u8],
    permissions: i32,
    file_id: &[u8],
    revision: u32,
    key_length: usize,
    encrypt_metadata: bool,
) -> crate::Result<Vec<u8>> {
    // R<=4 requires MD5 key derivation; needs the legacy-crypto feature.
    #[cfg(not(feature = "legacy-crypto"))]
    return Err(crate::Error::InvalidPdf(
        "pdf_oxide built without 'legacy-crypto': PDF Standard Security R≤4 (MD5 key derivation) is not supported".to_string()
    ));

    #[cfg(feature = "legacy-crypto")]
    {
        // #230 Phase C: route the primitive through the governed
        // provider (byte-identical under the default `compat` policy).
        let mut hasher = super::md5_kdf_hasher()?;

        // Step a: Pad or truncate password to 32 bytes
        let mut padded_password = [0u8; 32];
        let pass_len = password.len().min(32);
        padded_password[..pass_len].copy_from_slice(&password[..pass_len]);
        if pass_len < 32 {
            padded_password[pass_len..].copy_from_slice(&PADDING[..(32 - pass_len)]);
        }

        // Step b: Pass the password to MD5
        hasher.update(&padded_password);

        // Step c: Pass the owner password hash
        hasher.update(owner_key);

        // Step d: Pass permissions as 32-bit little-endian
        hasher.update(&permissions.to_le_bytes());

        // Step e: Pass the file identifier
        hasher.update(file_id);

        // Step f: For R >= 4, if EncryptMetadata is false, pass 0xFFFFFFFF
        if revision >= 4 && !encrypt_metadata {
            hasher.update(&[0xFF, 0xFF, 0xFF, 0xFF]);
        }

        // Step g: Finish MD5 hash
        let mut hash = hasher.finalize();

        // Step h: For R >= 3, do 50 additional MD5 iterations on first key_length bytes
        if revision >= 3 {
            for _ in 0..50 {
                let mut h = super::md5_kdf_hasher()?;
                h.update(&hash[..key_length.min(16)]);
                hash = h.finalize();
            }
        }

        // Step i: Return first key_length bytes (max 16 for MD5)
        Ok(hash[..key_length.min(16)].to_vec())
    }
}

/// Pad or truncate a password to 32 bytes using the standard padding.
///
/// PDF Spec: Algorithm 2, step 1
#[allow(dead_code)]
pub fn pad_password(password: &[u8]) -> Vec<u8> {
    let mut padded = Vec::with_capacity(32);
    let pass_len = password.len().min(32);
    padded.extend_from_slice(&password[..pass_len]);
    if pass_len < 32 {
        padded.extend_from_slice(&PADDING[..(32 - pass_len)]);
    }
    padded
}

/// Authenticate the user password (Algorithm 4/5 for R<=4, Algorithm 11 for R>=5).
///
/// PDF Spec: Section 7.6.3.4 - Algorithm 4/5: User password authentication
/// PDF 2.0 Spec: Algorithm 11 - Authenticating user password for R>=5
///
/// Returns the encryption key if authentication succeeds.
#[cfg_attr(not(feature = "legacy-crypto"), allow(unused_variables))]
pub fn authenticate_user_password(
    password: &[u8],
    user_key: &[u8],
    owner_key: &[u8],
    permissions: i32,
    file_id: &[u8],
    revision: u32,
    key_length: usize,
    encrypt_metadata: bool,
    user_encryption: Option<&[u8]>,
) -> Option<Vec<u8>> {
    // R>=5 uses SHA-256 based verification (Algorithm 11 for R5, Algorithm 2.A for R6)
    if revision >= 5 {
        return authenticate_user_password_r5_r6(password, user_key, revision, user_encryption);
    }

    #[cfg(not(feature = "legacy-crypto"))]
    {
        None
    }

    #[cfg(feature = "legacy-crypto")]
    {
        // Compute encryption key from password
        let key = compute_encryption_key(
            password,
            owner_key,
            permissions,
            file_id,
            revision,
            key_length,
            encrypt_metadata,
        )
        .ok()?;

        // Compute expected user key
        let expected_user_key = if revision >= 3 {
            compute_user_key_r3(&key, file_id).ok()?
        } else {
            compute_user_key_r2(&key).ok()?
        };

        // Compare first 16 bytes (constant-time comparison)
        if user_key.len() < 16 || expected_user_key.len() < 16 {
            return None;
        }
        let matches = constant_time_compare(&user_key[..16], &expected_user_key[..16]);

        if matches {
            Some(key)
        } else {
            None
        }
    }
}

/// Verify user password for R>=5 (PDF 2.0 Algorithm 11 for R5, Algorithm 2.A for R6).
///
/// R5: Simple SHA-256 hash comparison.
/// R6: Uses Algorithm 2.B (iterative hash with SHA-256/384/512 and AES-CBC).
fn authenticate_user_password_r5_r6(
    password: &[u8],
    user_key: &[u8],
    revision: u32,
    user_encryption: Option<&[u8]>,
) -> Option<Vec<u8>> {
    if user_key.len() < 48 {
        return None;
    }

    let password = saslprep_password(password);
    let password = truncate_password_utf8(&password);

    let validation_salt = &user_key[32..40];
    let key_salt = &user_key[40..48];

    // Compute verification hash
    let hash = if revision >= 6 {
        // R6: Algorithm 2.B (ISO 32000-2:2020 S7.6.4.3.4)
        algorithm_2b(&password, validation_salt, &[])
    } else {
        // R5: Simple SHA-256(password || validation_salt)
        let mut hasher = Sha256::new();
        hasher.update(&password);
        hasher.update(validation_salt);
        hasher.finalize().to_vec()
    };

    if !constant_time_compare(&hash[..32], &user_key[..32]) {
        return None;
    }

    if revision >= 6 {
        // R6: Derive intermediate key via Algorithm 2.B, then unwrap UE
        let ue = user_encryption?;
        if ue.len() < 32 {
            return None;
        }
        let intermediate_key = algorithm_2b(&password, key_salt, &[]);
        let iv = [0u8; 16];
        super::aes::aes256_decrypt_no_padding(&intermediate_key[..32], &iv, &ue[..32]).ok()
    } else {
        // R5: Simple SHA-256(password || key_salt)
        let mut hasher = Sha256::new();
        hasher.update(&password);
        hasher.update(key_salt);
        Some(hasher.finalize().to_vec())
    }
}

/// Apply SASLprep (RFC 4013) normalization to a password.
///
/// PDF 2.0 Spec requires SASLprep for Unicode passwords in R>=5.
/// Falls back to raw bytes if the input is not valid UTF-8 or normalization fails.
fn saslprep_password(password: &[u8]) -> Vec<u8> {
    let Ok(password_str) = std::str::from_utf8(password) else {
        return password.to_vec();
    };
    match stringprep::saslprep(password_str) {
        Ok(normalized) => normalized.as_bytes().to_vec(),
        Err(_) => password.to_vec(),
    }
}

/// ISO 32000-2:2020 Algorithm 2.B — Computing a hash (revision 6).
///
/// This iterative hash algorithm uses SHA-256, SHA-384, and SHA-512 combined
/// with AES-128-CBC encryption. It replaces simple SHA-256 hashing used in R5.
///
/// # Arguments
/// * `password` - The preprocessed password (SASLprep'd and truncated)
/// * `salt` - 8-byte salt (validation_salt or key_salt)
/// * `user_key` - Additional data: empty for user auth, U[0..48] for owner auth
fn algorithm_2b(password: &[u8], salt: &[u8], user_key: &[u8]) -> Vec<u8> {
    // Step 1: Initial hash = SHA-256(password || salt || user_key)
    let mut hasher = Sha256::new();
    hasher.update(password);
    hasher.update(salt);
    hasher.update(user_key);
    let mut k = hasher.finalize().to_vec(); // 32 bytes

    let mut round: usize = 0;
    loop {
        // Step a: Build K1 = (password || K || user_key) repeated 64 times
        let k1_unit_len = password.len() + k.len() + user_key.len();
        let mut k1 = Vec::with_capacity(k1_unit_len * 64);
        for _ in 0..64 {
            k1.extend_from_slice(password);
            k1.extend_from_slice(&k);
            k1.extend_from_slice(user_key);
        }

        // Pad K1 to multiple of 16 for AES-CBC
        let remainder = k1.len() % 16;
        if remainder != 0 {
            k1.extend(std::iter::repeat_n(0u8, 16 - remainder));
        }

        // Step b: E = AES-128-CBC-encrypt(key=K[0..16], iv=K[16..32], data=K1)
        let aes_key = &k[..16];
        let aes_iv = &k[16..32];
        let e = match super::aes::aes128_encrypt_no_padding(aes_key, aes_iv, &k1) {
            Ok(encrypted) => encrypted,
            Err(_) => return k, // Fallback on error
        };

        // Step c: Determine next hash algorithm.
        // Sum of first 16 bytes of E, mod 3
        let sum: u32 = e.iter().take(16).map(|&b| b as u32).sum();
        let remainder = sum % 3;

        // Step d: Hash E with selected algorithm
        k = match remainder {
            0 => {
                let mut h = Sha256::new();
                h.update(&e);
                h.finalize().to_vec()
            }
            1 => {
                let mut h = Sha384::new();
                h.update(&e);
                h.finalize().to_vec()
            }
            _ => {
                let mut h = Sha512::new();
                h.update(&e);
                h.finalize().to_vec()
            }
        };

        // Step e: per ISO 32000-2:2020 Algorithm 2.B step f, the round
        // counter increments before the termination check. Stop once at
        // least 64 rounds have run AND the last byte of E is ≤ round - 32.
        round += 1;
        let last_byte = *e.last().unwrap_or(&0) as usize;
        if round >= 64 && last_byte <= round.saturating_sub(32) {
            break;
        }
    }

    // Return first 32 bytes
    k.truncate(32);
    k
}

/// Compute the user password hash for R=2 (Algorithm 4).
///
/// PDF Spec: Section 7.6.3.4 - Algorithm 4
#[cfg(feature = "legacy-crypto")]
fn compute_user_key_r2(key: &[u8]) -> crate::Result<Vec<u8>> {
    // Encrypt padding string with key
    super::rc4::rc4_crypt(key, PADDING)
}

/// Compute the user password hash for R>=3 (Algorithm 5).
///
/// PDF Spec: Section 7.6.3.4 - Algorithm 5
#[cfg(feature = "legacy-crypto")]
fn compute_user_key_r3(key: &[u8], file_id: &[u8]) -> crate::Result<Vec<u8>> {
    // Step a: Create MD5 hash of padding + file ID (#230 Phase C:
    // governed provider; byte-identical under the default policy).
    let mut hasher = super::md5_kdf_hasher()?;
    hasher.update(PADDING);
    hasher.update(file_id);
    let mut hash = hasher.finalize();

    // Step b: Encrypt the hash 20 times with modified keys
    for i in 0..20 {
        let mut modified_key = key.to_vec();
        for byte in &mut modified_key {
            *byte ^= i as u8;
        }
        hash = super::rc4::rc4_crypt(&modified_key, &hash)?;
    }

    // Step c: Append 16 arbitrary bytes (we use zeros)
    hash.extend_from_slice(&[0u8; 16]);
    Ok(hash)
}

/// Truncate password to 127 bytes for UTF-8 (R>=5 requirement).
///
/// PDF 2.0 Spec: For R>=5, passwords are UTF-8 encoded and
/// limited to 127 bytes.
fn truncate_password_utf8(password: &[u8]) -> Vec<u8> {
    let mut result = password.to_vec();
    if result.len() > 127 {
        // Find UTF-8 boundary for truncation
        let mut end = 127;
        while end > 0 && (result[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        result.truncate(end);
    }
    result
}

/// Authenticate the owner password (Algorithm 7 for R≤4, Algorithm 12 for R≥5).
///
/// PDF Spec: Section 7.6.3.4 - Algorithm 7: Owner password authentication
/// PDF 2.0 Spec: Algorithm 12 - Authenticating owner password for R>=5
///
/// Returns the encryption key if authentication succeeds.
#[cfg_attr(not(feature = "legacy-crypto"), allow(unused_variables))]
pub fn authenticate_owner_password(
    owner_password: &[u8],
    user_key: &[u8],
    owner_key: &[u8],
    permissions: i32,
    file_id: &[u8],
    revision: u32,
    key_length: usize,
    encrypt_metadata: bool,
    owner_encryption: Option<&[u8]>,
) -> crate::Result<Option<Vec<u8>>> {
    if revision >= 5 {
        return Ok(authenticate_owner_password_r5_r6(
            owner_password,
            owner_key,
            user_key,
            revision,
            owner_encryption,
        ));
    }

    #[cfg(not(feature = "legacy-crypto"))]
    return Ok(None);

    // Algorithm 7: Authenticate owner password for R≤4
    #[cfg(feature = "legacy-crypto")]
    {
        // Steps a-d: Compute RC4 key from owner password (same as Algorithm 3 steps a-d)
        if owner_password.is_empty() {
            return Ok(None);
        }
        let padded_password = pad_password(owner_password);

        // #230 Phase C: governed provider; byte-identical default policy.
        let mut hasher = super::md5_kdf_hasher()?;
        hasher.update(&padded_password);
        let mut hash = hasher.finalize();

        if revision >= 3 {
            for _ in 0..50 {
                let mut h = super::md5_kdf_hasher()?;
                h.update(&hash[..key_length.min(16)]);
                hash = h.finalize();
            }
        }

        let rc4_key_len = key_length.min(16);
        let rc4_key = &hash[..rc4_key_len];

        // Step e: Decrypt the /O value to recover the padded user password
        let user_password_padded = if revision == 2 {
            // R=2: Single RC4 decryption
            super::rc4::rc4_crypt(rc4_key, owner_key)?
        } else {
            // R≥3: 20 RC4 decryptions with XOR'd keys (19 down to 0)
            let mut result = owner_key.to_vec();
            for i in (0..=19).rev() {
                let mut modified_key = rc4_key.to_vec();
                for byte in &mut modified_key {
                    *byte ^= i as u8;
                }
                result = super::rc4::rc4_crypt(&modified_key, &result)?;
            }
            result
        };

        // Step f: Use recovered user password to authenticate via Algorithm 6
        return Ok(authenticate_user_password(
            &user_password_padded,
            user_key,
            owner_key,
            permissions,
            file_id,
            revision,
            key_length,
            encrypt_metadata,
            None, // R<=4 path, no UE needed
        ));
    }
}

/// Verify owner password for R>=5 (PDF 2.0 Algorithm 12 for R5, Algorithm 2.A for R6).
///
/// R5: Simple SHA-256 hash comparison.
/// R6: Uses Algorithm 2.B (iterative hash with SHA-256/384/512 and AES-CBC).
fn authenticate_owner_password_r5_r6(
    password: &[u8],
    owner_key: &[u8],
    user_key: &[u8],
    revision: u32,
    owner_encryption: Option<&[u8]>,
) -> Option<Vec<u8>> {
    if owner_key.len() < 48 || user_key.len() < 48 {
        return None;
    }

    let password = saslprep_password(password);
    let password = truncate_password_utf8(&password);

    let owner_validation_salt = &owner_key[32..40];
    let owner_key_salt = &owner_key[40..48];
    let u_value = &user_key[..48];

    // Compute verification hash
    let hash = if revision >= 6 {
        // R6: Algorithm 2.B with U[0..48] as additional data
        algorithm_2b(&password, owner_validation_salt, u_value)
    } else {
        // R5: SHA-256(password || owner_validation_salt || U[0..48])
        let mut hasher = Sha256::new();
        hasher.update(&password);
        hasher.update(owner_validation_salt);
        hasher.update(u_value);
        hasher.finalize().to_vec()
    };

    if !constant_time_compare(&hash[..32], &owner_key[..32]) {
        return None;
    }

    if revision >= 6 {
        // R6: Derive intermediate key via Algorithm 2.B, then unwrap OE
        let oe = owner_encryption?;
        if oe.len() < 32 {
            return None;
        }
        let intermediate_key = algorithm_2b(&password, owner_key_salt, u_value);
        let iv = [0u8; 16];
        super::aes::aes256_decrypt_no_padding(&intermediate_key[..32], &iv, &oe[..32]).ok()
    } else {
        // R5: SHA-256(password || owner_key_salt || U[0..48])
        let mut hasher = Sha256::new();
        hasher.update(&password);
        hasher.update(owner_key_salt);
        hasher.update(u_value);
        Some(hasher.finalize().to_vec())
    }
}

/// Constant-time comparison to prevent timing attacks.
///
/// Returns true if the slices are equal.
fn constant_time_compare(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }

    result == 0
}
