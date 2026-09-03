//! File Information Block (FIB) parsing.
//!
//! The FIB is at the start of the WordDocument stream. It contains metadata
//! and pointers to other structures in the Table stream.

use super::error::{DocError, Result};

#[derive(Debug, Clone)]
pub struct Fib {
    pub use_table1: bool,
    pub clx_offset: u32,
    pub clx_size: u32,
    pub text_len: u32,
}

impl Fib {
    pub fn parse(data: &[u8]) -> Result<Self> {
        const FIB_BASE_BYTES: usize = 32;
        const CLX_PAIR_INDEX: usize = 33;

        if data.len() < FIB_BASE_BYTES + 2 {
            return Err(DocError::InvalidFib("FIB base is truncated".into()));
        }
        let identifier = read_u16(data, 0)?;
        if identifier == 0xA5DC {
            return Err(DocError::Unsupported("Word 6/95 binary format".into()));
        }
        if identifier != 0xA5EC {
            return Err(DocError::InvalidFib("invalid FIB identifier".into()));
        }

        let version = read_u16(data, 2)?;
        if !(0x00C1..=0x0112).contains(&version) {
            return Err(DocError::Unsupported(format!(
                "FIB version 0x{version:04X}"
            )));
        }

        let flags = read_u16(data, 0x0A)?;
        if flags & ((1 << 8) | (1 << 15)) != 0 {
            return Err(DocError::Unsupported("encrypted or obfuscated DOC".into()));
        }
        let use_table1 = flags & (1 << 9) != 0;

        let csw = read_u16(data, FIB_BASE_BYTES)? as usize;
        let cslw_offset = FIB_BASE_BYTES
            .checked_add(2)
            .and_then(|offset| offset.checked_add(csw.checked_mul(2)?))
            .ok_or_else(|| DocError::InvalidFib("FibRgW size overflow".into()))?;
        let cslw = read_u16(data, cslw_offset)? as usize;
        if cslw < 4 {
            return Err(DocError::InvalidFib("FibRgLw is too short".into()));
        }
        let fib_rg_lw_offset = cslw_offset
            .checked_add(2)
            .ok_or_else(|| DocError::InvalidFib("FibRgLw offset overflow".into()))?;
        let text_len = read_u32(
            data,
            fib_rg_lw_offset
                .checked_add(3 * 4)
                .ok_or_else(|| DocError::InvalidFib("ccpText offset overflow".into()))?,
        )?;

        let pair_count_offset = fib_rg_lw_offset
            .checked_add(
                cslw.checked_mul(4)
                    .ok_or_else(|| DocError::InvalidFib("FibRgLw size overflow".into()))?,
            )
            .ok_or_else(|| DocError::InvalidFib("FibRgFcLcb offset overflow".into()))?;
        let pair_count = read_u16(data, pair_count_offset)? as usize;
        if pair_count <= CLX_PAIR_INDEX {
            return Err(DocError::Unsupported(
                "FIB does not contain a Word 97 CLX pair".into(),
            ));
        }
        let pairs_offset = pair_count_offset
            .checked_add(2)
            .ok_or_else(|| DocError::InvalidFib("FibRgFcLcb offset overflow".into()))?;
        let clx_pair_offset = pairs_offset
            .checked_add(
                CLX_PAIR_INDEX
                    .checked_mul(8)
                    .ok_or_else(|| DocError::InvalidFib("CLX pair offset overflow".into()))?,
            )
            .ok_or_else(|| DocError::InvalidFib("CLX pair offset overflow".into()))?;
        let clx_offset = read_u32(data, clx_pair_offset)?;
        let clx_size = read_u32(data, clx_pair_offset + 4)?;

        if text_len != 0 && clx_size == 0 {
            return Err(DocError::InvalidFib(
                "non-empty document has no piece table".into(),
            ));
        }

        Ok(Self {
            use_table1,
            clx_offset,
            clx_size,
            text_len,
        })
    }
}

fn read_u16(data: &[u8], offset: usize) -> Result<u16> {
    let bytes = data
        .get(offset..offset.saturating_add(2))
        .ok_or_else(|| DocError::InvalidFib("FIB field is truncated".into()))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| DocError::InvalidFib("FIB field is truncated".into()))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(any())]
mod tests {
    use super::*;

    fn build_minimal_fib() -> Vec<u8> {
        let mut data = vec![0u8; 1024];
        // wIdent = Word 97
        data[0..2].copy_from_slice(&0xA5ECu16.to_le_bytes());
        // nFib (version)
        data[2..4].copy_from_slice(&0x00C1u16.to_le_bytes());
        // flags: use 1Table (bit 9)
        data[0x0A..0x0C].copy_from_slice(&(1u16 << 9).to_le_bytes());
        // ccpText = 100
        data[0x4C..0x50].copy_from_slice(&100u32.to_le_bytes());
        // fcClx
        data[0x01A2..0x01A6].copy_from_slice(&512u32.to_le_bytes());
        // lcbClx
        data[0x01A6..0x01AA].copy_from_slice(&64u32.to_le_bytes());
        data
    }

    #[test]
    fn parse_valid_fib() {
        let data = build_minimal_fib();
        let fib = Fib::parse(&data).unwrap();
        assert_eq!(fib.version, 0x00C1);
        assert!(fib.use_table1);
        assert_eq!(fib.text_len, 100);
        assert_eq!(fib.clx_offset, 512);
        assert_eq!(fib.clx_size, 64);
    }

    #[test]
    fn bad_wident_rejected() {
        let mut data = build_minimal_fib();
        data[0..2].copy_from_slice(&0x1234u16.to_le_bytes());
        assert!(Fib::parse(&data).is_err());
    }

    #[test]
    fn too_short_rejected() {
        let data = vec![0u8; 100];
        assert!(Fib::parse(&data).is_err());
    }

    #[test]
    fn use_table0() {
        let mut data = build_minimal_fib();
        data[0x0A..0x0C].copy_from_slice(&0u16.to_le_bytes()); // clear bit 9
        let fib = Fib::parse(&data).unwrap();
        assert!(!fib.use_table1);
    }
}
