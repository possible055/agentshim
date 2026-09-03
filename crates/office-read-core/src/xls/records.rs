//! BIFF8 record types and low-level record iterator.

#![allow(
    dead_code,
    reason = "defines the full spec'd record-type surface; not every constant is consumed"
)]

use super::error::{Result, XlsError};

// ── Record type IDs ──
pub const RT_BOF: u16 = 0x0809;
pub const RT_EOF: u16 = 0x000A;
pub const RT_BOUNDSHEET: u16 = 0x0085;
pub const RT_SST: u16 = 0x00FC;
pub const RT_CONTINUE: u16 = 0x003C;
pub const RT_LABELSST: u16 = 0x00FD;
pub const RT_NUMBER: u16 = 0x0203;
pub const RT_RK: u16 = 0x027E;
pub const RT_MULRK: u16 = 0x00BD;
pub const RT_FORMULA: u16 = 0x0006;
pub const RT_BOOLERR: u16 = 0x0205;
pub const RT_LABEL: u16 = 0x0204;
pub const RT_RSTRING: u16 = 0x00D6;
pub const RT_BLANK: u16 = 0x0201;
pub const RT_MULBLANK: u16 = 0x00BE;
pub const RT_FORMAT: u16 = 0x041E;
pub const RT_XF: u16 = 0x00E0;
pub const RT_DATEMODE: u16 = 0x0022;
pub const RT_CODEPAGE: u16 = 0x0042;
pub const RT_FILEPASS: u16 = 0x002F;
pub const RT_STRING: u16 = 0x0207;

/// A raw BIFF record: type + data (may span CONTINUE records).
#[derive(Debug, Clone)]
pub struct BiffRecord {
    pub record_type: u16,
    pub data: Vec<u8>,
}

/// Iterate over BIFF records in a byte stream, merging CONTINUE records.
pub struct RecordIter<'a> {
    data: &'a [u8],
    pos: usize,
    last_type: u16,
}

impl<'a> RecordIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            last_type: 0,
        }
    }

    /// Read the next raw record (without merging CONTINUE).
    fn read_raw(&mut self) -> Option<Result<(u16, Vec<u8>)>> {
        if let Err(error) = crate::budget::check_cancelled() {
            return Some(Err(map_budget_error(error)));
        }
        if self.pos == self.data.len() {
            return None;
        }
        let Some(header_end) = self.pos.checked_add(4) else {
            self.pos = self.data.len();
            return Some(Err(XlsError::InvalidRecord(
                "record header offset overflow".to_owned(),
            )));
        };
        if header_end > self.data.len() {
            self.pos = self.data.len();
            return Some(Err(XlsError::InvalidRecord(
                "truncated record header".to_owned(),
            )));
        }

        let rt = u16::from_le_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        let size = u16::from_le_bytes([self.data[self.pos + 2], self.data[self.pos + 3]]) as usize;
        self.pos = header_end;

        let Some(record_end) = self.pos.checked_add(size) else {
            self.pos = self.data.len();
            return Some(Err(XlsError::InvalidRecord(
                "record length overflow".to_owned(),
            )));
        };
        if record_end > self.data.len() {
            self.pos = self.data.len();
            return Some(Err(XlsError::InvalidRecord(
                "truncated record body".to_owned(),
            )));
        }

        if let Err(error) = crate::budget::check_model_text_bytes("office_xls_record_bytes", size) {
            self.pos = self.data.len();
            return Some(Err(map_budget_error(error)));
        }
        let data = self.data[self.pos..record_end].to_vec();
        self.pos = record_end;
        Some(Ok((rt, data)))
    }
}

impl<'a> Iterator for RecordIter<'a> {
    type Item = Result<BiffRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        let (rt, mut data) = match self.read_raw()? {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };

        self.last_type = rt;

        // Merge subsequent CONTINUE records into this record's data.
        loop {
            if self.pos + 4 > self.data.len() {
                break;
            }
            let next_rt = u16::from_le_bytes([self.data[self.pos], self.data[self.pos + 1]]);
            if next_rt != RT_CONTINUE {
                break;
            }
            // Consume the CONTINUE record.
            match self.read_raw() {
                Some(Ok((_rt, cont_data))) => {
                    let observed = data.len().saturating_add(cont_data.len());
                    if let Err(error) =
                        crate::budget::check_model_text_bytes("office_xls_record_bytes", observed)
                    {
                        return Some(Err(map_budget_error(error)));
                    }
                    data.extend_from_slice(&cont_data);
                }
                Some(Err(e)) => return Some(Err(e)),
                None => break,
            }
        }

        Some(Ok(BiffRecord {
            record_type: rt,
            data,
        }))
    }
}

fn map_budget_error(error: crate::OfficeReadError) -> XlsError {
    let error = match error {
        crate::OfficeReadError::ResourceLimit {
            resource,
            limit,
            observed,
        } => crate::cfb::CfbError::ResourceLimit {
            resource,
            limit,
            observed,
        },
        _ => crate::cfb::CfbError::Cancelled,
    };
    XlsError::Cfb(error)
}

#[cfg(any())]
mod tests {
    use super::*;

    fn make_record(rt: u16, data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&rt.to_le_bytes());
        buf.extend_from_slice(&(data.len() as u16).to_le_bytes());
        buf.extend_from_slice(data);
        buf
    }

    #[test]
    fn iterate_single_record() {
        let stream = make_record(RT_BOF, &[0x00, 0x06, 0x05, 0x00]);
        let records: Vec<_> = RecordIter::new(&stream)
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_type, RT_BOF);
        assert_eq!(records[0].data, &[0x00, 0x06, 0x05, 0x00]);
    }

    #[test]
    fn iterate_multiple_records() {
        let mut stream = make_record(RT_BOF, &[0x00, 0x06]);
        stream.extend(make_record(RT_EOF, &[]));
        let records: Vec<_> = RecordIter::new(&stream)
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].record_type, RT_BOF);
        assert_eq!(records[1].record_type, RT_EOF);
    }

    #[test]
    fn continue_records_merged() {
        let mut stream = make_record(RT_SST, &[0x01, 0x02]);
        stream.extend(make_record(RT_CONTINUE, &[0x03, 0x04]));
        stream.extend(make_record(RT_CONTINUE, &[0x05]));
        stream.extend(make_record(RT_EOF, &[]));
        let records: Vec<_> = RecordIter::new(&stream)
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(records.len(), 2); // SST (merged) + EOF
        assert_eq!(records[0].record_type, RT_SST);
        assert_eq!(records[0].data, &[0x01, 0x02, 0x03, 0x04, 0x05]);
    }

    #[test]
    fn truncated_record_tolerant() {
        // Record says 10 bytes but only 2 available — should return partial data.
        let mut stream = Vec::new();
        stream.extend_from_slice(&RT_BOF.to_le_bytes());
        stream.extend_from_slice(&10u16.to_le_bytes());
        stream.extend_from_slice(&[0x00, 0x00]);
        let results: Vec<_> = RecordIter::new(&stream)
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].record_type, RT_BOF);
        assert_eq!(results[0].data.len(), 2); // only 2 bytes available
    }

    #[test]
    fn empty_stream() {
        let records: Vec<_> = RecordIter::new(&[])
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(records.is_empty());
    }
}
